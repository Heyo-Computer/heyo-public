//! Observing heyo cloud's queue, without touching it.
//!
//! queue-fn and heyo's cloud service can share a NATS server — that is the whole
//! point of `nats_auth` accepting the credential forms an operator already has.
//! When they do share one, the cloud's work is the other half of what the person
//! looking at this dashboard is responsible for, so the dashboard renders it too.
//!
//! **Nothing here consumes.** The cloud's `HEYO_SANDBOX` stream is
//! `WorkQueue`-retained, and that forbids the obvious approach twice over: a
//! message is deleted when its worker acks it, so there is no history to read
//! back, and JetStream refuses a second consumer whose filter overlaps an
//! existing one, so queue-fn cannot bind to `sandbox.>` at all. If it somehow
//! could, it would be stealing the cloud's work rather than watching it. There
//! are two read-only vantage points instead:
//!
//! 1. **A core-NATS subscription** to `sandbox.>`. A JetStream publish is an
//!    ordinary publish that a stream happens to capture, so a plain subscriber
//!    receives its own copy and consumes nothing. Commands and lifecycle events
//!    both cross this subscription, which is what makes succeeded/failed
//!    knowable at all.
//! 2. **Polling stream and consumer info** over the JetStream management API —
//!    the same call cloud's own admin overview makes. `num_pending` is work
//!    enqueued and undelivered; `num_ack_pending` is work a cloud worker is
//!    holding right now.
//!
//! The split is worth keeping straight when reading the dashboard. The gauges
//! from (2) are absolute: they describe the queue as it is, including work
//! enqueued long before queue-fn started. The counters from (1) are not — a
//! subscriber only ever receives copies of messages published while it was
//! subscribed — so "succeeded" means "since observation started", and the
//! snapshot carries `observed_since_ms` so the UI can say which window it means.
//!
//! **Some cloud commands report no outcome.** `sandbox.cmd.restart` and
//! `sandbox.cmd.wake` are acked by their workers without publishing anything;
//! only the create paths emit `evt.ready`/`evt.failed`. Those jobs are tracked
//! as `unreported` rather than being counted as successes, because the bus
//! genuinely does not say how they ended and a dashboard that guesses is worse
//! than one that admits the gap.

use crate::bus::{Bus, now_ms};
use crate::metrics::{Histogram, HistogramSnapshot};
use futures::StreamExt;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// The cloud's JetStream stream (`cloud/src/sandbox_messages.rs`).
pub const DEFAULT_STREAM: &str = "HEYO_SANDBOX";
/// Its configured subject filter. Every command, event, and telemetry subject
/// the cloud uses lives under it.
pub const DEFAULT_SUBJECT: &str = "sandbox.>";

/// Job wall-clock bucket bounds, in milliseconds.
///
/// Milliseconds rather than seconds because this queue carries both kinds of
/// work: a sqlite mutation finishes in a few hundred milliseconds, and a cold
/// multi-GB image download runs for half an hour. Recording seconds would floor
/// every sqlite operation to zero. The tail runs past the create consumer's
/// 35-minute ack wait, so the slowest jobs land in a bucket rather than off the
/// end of the histogram.
pub const JOB_MS_BOUNDS: &[u64] = &[
    100, 250, 500, 1_000, 2_500, 5_000, 15_000, 60_000, 300_000, 900_000, 1_800_000,
];

/// How long to wait before re-subscribing after the subscription ends.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(5);

/// Cap on the per-subject breakdown. The cloud's subject set is fixed and small;
/// this only exists so a stream that has grown unexpected subjects cannot make
/// the metrics response unbounded.
const MAX_SUBJECT_ROWS: usize = 64;

// --- What a message on the wire is ---------------------------------------

/// Where a job is, as far as the bus can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// The command was published; no lifecycle event has followed.
    Queued,
    /// A worker published `evt.provisioning` for it.
    Running,
    Succeeded,
    Failed,
    /// The command kind publishes no completion event, so the bus cannot say how
    /// it ended. Not a success and not a failure — an unknown.
    Unreported,
}

impl JobState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Unreported)
    }
}

/// Fire-and-forget traffic that is not a job: telemetry and notifications that
/// no command started and no outcome ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    UsageStarted,
    UsageStopped,
    HealthStuck,
    SqliteMutated,
    SqliteSizeSample,
}

/// The classification of one subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A command that starts a job.
    Command {
        /// Stable label for grouping, e.g. `sandbox.create`.
        kind: &'static str,
        /// Whether this command's worker publishes a terminal lifecycle event.
        /// `false` is why `JobState::Unreported` exists.
        reports_outcome: bool,
    },
    /// A lifecycle event moving a job along.
    Lifecycle {
        state: JobState,
        /// The command kind this event can only have come from, used to label a
        /// job whose command was published before queue-fn was watching.
        implies_kind: &'static str,
    },
    Signal(Signal),
    /// A `sandbox.>` subject this build does not know. Counted rather than
    /// dropped: a subject the cloud adds later should show up as "there is
    /// traffic here queue-fn cannot interpret", not as silence.
    Unknown,
}

/// Map a subject to what it means. Pure, so the whole classification table is
/// testable without a bus.
fn classify(subject: &str) -> Kind {
    match subject {
        "sandbox.cmd.create" => Kind::Command {
            kind: "sandbox.create",
            reports_outcome: true,
        },
        // The restart worker calls the backend and acks; it publishes nothing.
        "sandbox.cmd.restart" => Kind::Command {
            kind: "sandbox.restart",
            reports_outcome: false,
        },
        // Likewise the wake worker — the daemon's restart tracker owns the
        // outcome, and it is not on this bus.
        "sandbox.cmd.wake" => Kind::Command {
            kind: "sandbox.wake",
            reports_outcome: false,
        },
        "sandbox.evt.provisioning" => Kind::Lifecycle {
            state: JobState::Running,
            implies_kind: "sandbox.create",
        },
        "sandbox.evt.ready" => Kind::Lifecycle {
            state: JobState::Succeeded,
            implies_kind: "sandbox.create",
        },
        "sandbox.evt.failed" => Kind::Lifecycle {
            state: JobState::Failed,
            implies_kind: "sandbox.create",
        },

        "sandbox.sqlite.cmd.create" => Kind::Command {
            kind: "sqlite.create",
            reports_outcome: true,
        },
        "sandbox.sqlite.cmd.delete" => Kind::Command {
            kind: "sqlite.delete",
            reports_outcome: true,
        },
        "sandbox.sqlite.cmd.snapshot" => Kind::Command {
            kind: "sqlite.snapshot",
            reports_outcome: true,
        },
        "sandbox.sqlite.cmd.restore" => Kind::Command {
            kind: "sqlite.restore",
            reports_outcome: true,
        },
        "sandbox.sqlite.evt.provisioning" => Kind::Lifecycle {
            state: JobState::Running,
            implies_kind: "sqlite.create",
        },
        "sandbox.sqlite.evt.ready" => Kind::Lifecycle {
            state: JobState::Succeeded,
            implies_kind: "sqlite.create",
        },
        "sandbox.sqlite.evt.deleted" => Kind::Lifecycle {
            state: JobState::Succeeded,
            implies_kind: "sqlite.delete",
        },
        "sandbox.sqlite.evt.snapshotted" => Kind::Lifecycle {
            state: JobState::Succeeded,
            implies_kind: "sqlite.snapshot",
        },
        "sandbox.sqlite.evt.restored" => Kind::Lifecycle {
            state: JobState::Succeeded,
            implies_kind: "sqlite.restore",
        },
        // `evt.failed` is published by all four sqlite operations, so an orphan
        // one cannot be attributed to a specific op.
        "sandbox.sqlite.evt.failed" => Kind::Lifecycle {
            state: JobState::Failed,
            implies_kind: "sqlite",
        },

        "sandbox.usage.started" => Kind::Signal(Signal::UsageStarted),
        "sandbox.usage.stopped" => Kind::Signal(Signal::UsageStopped),
        "sandbox.health.stuck" => Kind::Signal(Signal::HealthStuck),
        "sandbox.sqlite.evt.mutated" => Kind::Signal(Signal::SqliteMutated),
        "sandbox.sqlite.size.sample_request" => Kind::Signal(Signal::SqliteSizeSample),

        _ => Kind::Unknown,
    }
}

// --- The envelope, read loosely -----------------------------------------

/// The subset of `SandboxEnvelope` this needs, deserialized permissively.
///
/// Deliberately not a copy of the cloud's type: queue-fn is an observer of
/// somebody else's schema, and a strict mirror would start failing to parse the
/// moment cloud adds a field. Every field defaults, so a message shaped
/// differently than expected still yields whatever it does carry.
#[derive(Debug, Default, serde::Deserialize)]
struct Envelope {
    #[serde(default)]
    correlation_id: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    sandbox_id: Option<String>,
    #[serde(default)]
    producer: String,
    #[serde(default)]
    payload: serde_json::Value,
}

fn field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Envelope {
    /// The key a job is tracked under. `correlation_id` is the right one: the
    /// cloud mints it on the command and copies it onto every lifecycle event
    /// the command causes, which is exactly the join this needs. The fallbacks
    /// only matter for a message that arrives without one.
    fn correlation_key(&self) -> Option<String> {
        for candidate in [
            &self.correlation_id,
            &self.idempotency_key,
            &self.message_id,
        ] {
            let trimmed = candidate.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        None
    }

    /// What the job is about: a deployment, a database, or a sandbox.
    fn target(&self) -> Option<String> {
        field(&self.payload, "deployment_id")
            .or_else(|| field(&self.payload, "database_id"))
            .or_else(|| field(&self.payload, "sandbox_id"))
            .or_else(|| self.sandbox_id.clone())
    }

    /// A human-facing label when the payload carries one. Lifecycle events nest
    /// the original request, so the name survives onto the event too.
    fn label(&self) -> Option<String> {
        field(&self.payload, "name")
            .or_else(|| self.payload.get("request").and_then(|r| field(r, "name")))
            .or_else(|| field(&self.payload, "subdomain"))
    }

    fn error(&self) -> Option<String> {
        field(&self.payload, "error")
    }
}

// --- The observed record -------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct CloudJob {
    /// The cloud's correlation id — the same value that ties its own logs and
    /// traces together, so it is worth surfacing verbatim.
    pub id: String,
    pub kind: String,
    /// The subject the job was last seen on.
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    pub state: JobState,
    /// False when the first thing seen for this job was a lifecycle event —
    /// its command was published before queue-fn subscribed, so `enqueued_at_ms`
    /// is when it was *noticed*, not when it was enqueued, and no duration is
    /// recorded for it.
    pub command_observed: bool,
    pub enqueued_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CloudJob {
    fn duration_ms(&self) -> Option<u64> {
        self.finished_at_ms
            .map(|end| end.saturating_sub(self.enqueued_at_ms))
    }
}

/// Monotonic totals per command kind, so the dashboard can say which kind of
/// work is failing rather than only that something is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct KindTotals {
    pub enqueued: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub unreported: u64,
    /// Jobs currently in `Queued` or `Running`, derived from the retained ring.
    pub in_progress: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SignalTotals {
    pub usage_started: u64,
    pub usage_stopped: u64,
    pub health_stuck: u64,
    pub sqlite_mutated: u64,
    pub sqlite_size_samples: u64,
}

/// Counts of what crossed the subscription. Everything here is "since
/// observation started", never absolute.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ObservedTotals {
    /// Every message seen, whatever it was.
    pub messages: u64,
    pub commands: u64,
    pub events: u64,
    pub signals: u64,
    /// Traffic on a `sandbox.>` subject this build does not classify.
    pub unknown_subjects: u64,
    /// A message whose body would not parse as an envelope.
    pub undecodable: u64,
    /// A command re-published inside JetStream's dedupe window — the server
    /// collapsed it into the first, so counting it as a second job would
    /// overstate the queue. The wake path publishes these by design.
    pub duplicates: u64,
    /// Lifecycle events for a job whose command was never seen.
    pub orphan_events: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub unreported: u64,
    /// Times the subscription had to be re-established. Non-zero means there is
    /// a window whose messages nothing saw, so the outcome counters are a floor.
    pub resubscribes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudConsumer {
    pub name: String,
    pub filter_subject: String,
    pub ack_wait_secs: u64,
    /// Enqueued and undelivered — work no cloud worker has yet.
    pub num_pending: u64,
    /// Delivered and unacked — a worker is holding it right now.
    pub num_ack_pending: u64,
    pub num_redelivered: u64,
    pub num_waiting: u64,
    pub delivered_stream_seq: u64,
    pub ack_floor_stream_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudSubjectStat {
    pub subject: String,
    /// Messages still retained on the subject. Under `WorkQueue` retention this
    /// is backlog, not history: an acked message is gone.
    pub messages: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudStreamInfo {
    pub retention: String,
    pub storage: String,
    pub subjects: Vec<String>,
    pub messages: u64,
    pub bytes: u64,
    pub consumer_count: usize,
    pub first_seq: u64,
    pub last_seq: u64,
    pub last_message_ms: i64,
}

/// The backlog as the queue itself reports it. A gauge, absolute, independent of
/// when observation started.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueueGauges {
    pub pending: u64,
    pub in_flight: u64,
    pub redelivered: u64,
    pub waiting: u64,
    pub stream_messages: u64,
    pub stream_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudSnapshot {
    pub stream: String,
    pub subject: String,
    /// The last stream poll succeeded — the stream exists and is readable.
    pub connected: bool,
    /// Why it didn't, when it didn't. `HEYO_SANDBOX` being absent is the normal
    /// reading of "the cloud service isn't deployed against this NATS".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The core-NATS subscription is live.
    pub subscribed: bool,
    /// When outcome counting started. Everything in `observed` is since then.
    pub observed_since_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polled_at_ms: Option<u64>,
    pub queue: QueueGauges,
    /// Live count of retained jobs not yet in a terminal state.
    pub in_progress: u64,
    pub observed: ObservedTotals,
    pub signals: SignalTotals,
    pub by_kind: BTreeMap<String, KindTotals>,
    /// Wall clock from command to terminal event, in milliseconds. Only jobs
    /// whose command *and* outcome were both observed contribute.
    pub job_ms: HistogramSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_info: Option<CloudStreamInfo>,
    pub consumers: Vec<CloudConsumer>,
    pub subjects: Vec<CloudSubjectStat>,
    /// Newest first, up to the caller's limit.
    pub jobs: Vec<CloudJob>,
    /// How many jobs are retained in total, so a truncated list says so.
    pub jobs_retained: usize,
}

// --- State ---------------------------------------------------------------

/// Everything the subscription accumulates, behind one mutex.
///
/// Unlike `results::Results`, this is not on a hot path — it is one lock per
/// cloud message, at whatever rate somebody else's control plane publishes,
/// which is orders of magnitude below the dispatcher's. A plain mutex buys a
/// coherent snapshot (a job's state and the counter that recorded it can never
/// disagree) for a cost that does not matter here.
#[derive(Debug)]
struct Observed {
    /// Jobs keyed by insertion order, so eviction is "drop the oldest".
    jobs: BTreeMap<u64, CloudJob>,
    /// correlation key -> jobs key.
    by_correlation: HashMap<String, u64>,
    /// JetStream's `Nats-Msg-Id` values already seen, so a re-publish the server
    /// deduped is not counted as a second job. Bounded, FIFO.
    dedupe_seen: HashSet<String>,
    dedupe_order: VecDeque<String>,
    next_seq: u64,
    capacity: usize,
    totals: ObservedTotals,
    signals: SignalTotals,
    by_kind: BTreeMap<String, KindTotals>,
}

impl Observed {
    fn new(capacity: usize) -> Self {
        Self {
            jobs: BTreeMap::new(),
            by_correlation: HashMap::new(),
            dedupe_seen: HashSet::new(),
            dedupe_order: VecDeque::new(),
            next_seq: 0,
            capacity: capacity.max(1),
            totals: ObservedTotals::default(),
            signals: SignalTotals::default(),
            by_kind: BTreeMap::new(),
        }
    }

    /// Record a dedupe id. Returns false if it had already been seen, meaning
    /// JetStream collapsed this publish into an earlier one.
    fn note_dedupe(&mut self, id: String) -> bool {
        if !self.dedupe_seen.insert(id.clone()) {
            return false;
        }
        self.dedupe_order.push_back(id);
        // Kept at the same order of magnitude as the job ring: once a job has
        // aged out there is nothing left for a late duplicate to collide with.
        while self.dedupe_order.len() > self.capacity {
            if let Some(old) = self.dedupe_order.pop_front() {
                self.dedupe_seen.remove(&old);
            }
        }
        true
    }

    fn insert(&mut self, key: String, job: CloudJob) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.by_correlation.insert(key, seq);
        self.jobs.insert(seq, job);

        while self.jobs.len() > self.capacity {
            let Some((&oldest, _)) = self.jobs.iter().next() else {
                break;
            };
            if let Some(dropped) = self.jobs.remove(&oldest) {
                // Only clear the index if it still points at the evicted job: a
                // correlation id reused by a later job must keep the new entry.
                if self.by_correlation.get(&dropped.id) == Some(&oldest) {
                    self.by_correlation.remove(&dropped.id);
                }
            }
        }
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut CloudJob> {
        let seq = *self.by_correlation.get(key)?;
        self.jobs.get_mut(&seq)
    }

    fn kind_mut(&mut self, kind: &str) -> &mut KindTotals {
        self.by_kind.entry(kind.to_string()).or_default()
    }
}

/// Shared, read by the admin API and written by the observer task.
#[derive(Debug)]
pub struct CloudState {
    stream: String,
    subject: String,
    observed: Mutex<Observed>,
    /// The last stream/consumer poll. Separate mutex: a slow poll must not block
    /// the subscription, and the two never need to be consistent with each other
    /// (one is a gauge, the other a counter).
    poll: Mutex<PollState>,
    job_ms: Histogram,
    subscribed: AtomicU64,
    observed_since_ms: u64,
}

#[derive(Debug, Default)]
struct PollState {
    polled_at_ms: Option<u64>,
    error: Option<String>,
    stream_info: Option<CloudStreamInfo>,
    consumers: Vec<CloudConsumer>,
    subjects: Vec<CloudSubjectStat>,
}

impl CloudState {
    pub fn new(stream: String, subject: String, capacity: usize) -> Self {
        Self {
            stream,
            subject,
            observed: Mutex::new(Observed::new(capacity)),
            poll: Mutex::new(PollState::default()),
            job_ms: Histogram::new(JOB_MS_BOUNDS),
            subscribed: AtomicU64::new(0),
            observed_since_ms: now_ms(),
        }
    }

    fn set_subscribed(&self, up: bool) {
        self.subscribed.store(up as u64, Ordering::Relaxed);
    }

    /// A poisoned lock is not worth taking the process down over — this is a
    /// monitoring view, and the data behind it is counters.
    fn observed(&self) -> std::sync::MutexGuard<'_, Observed> {
        self.observed.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn poll_state(&self) -> std::sync::MutexGuard<'_, PollState> {
        self.poll.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record one message seen on the wire.
    ///
    /// `dedupe_id` is JetStream's `Nats-Msg-Id` header, which the cloud sets on
    /// every publish. `now` is passed rather than read so the state machine is
    /// testable at whatever clock the test wants.
    pub fn observe(&self, subject: &str, dedupe_id: Option<&str>, payload: &[u8], now: u64) {
        let kind = classify(subject);
        let mut s = self.observed();
        s.totals.messages += 1;

        match kind {
            Kind::Signal(signal) => {
                s.totals.signals += 1;
                let t = &mut s.signals;
                match signal {
                    Signal::UsageStarted => t.usage_started += 1,
                    Signal::UsageStopped => t.usage_stopped += 1,
                    Signal::HealthStuck => t.health_stuck += 1,
                    Signal::SqliteMutated => t.sqlite_mutated += 1,
                    Signal::SqliteSizeSample => t.sqlite_size_samples += 1,
                }
                return;
            }
            Kind::Unknown => {
                s.totals.unknown_subjects += 1;
                return;
            }
            _ => {}
        }

        let Ok(env) = serde_json::from_slice::<Envelope>(payload) else {
            s.totals.undecodable += 1;
            return;
        };
        let Some(key) = env.correlation_key() else {
            s.totals.undecodable += 1;
            return;
        };

        match kind {
            Kind::Command {
                kind,
                reports_outcome,
            } => {
                s.totals.commands += 1;

                // The server's own dedupe key, so this mirrors exactly what
                // JetStream did with the publish rather than guessing at it.
                let dedupe = dedupe_id
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| key.clone());
                if !s.note_dedupe(dedupe) {
                    s.totals.duplicates += 1;
                    return;
                }

                let state = if reports_outcome {
                    JobState::Queued
                } else {
                    JobState::Unreported
                };
                if !reports_outcome {
                    s.totals.unreported += 1;
                }
                let job = CloudJob {
                    id: key.clone(),
                    kind: kind.to_string(),
                    subject: subject.to_string(),
                    target: env.target(),
                    label: env.label(),
                    account_id: env.account_id.clone(),
                    producer: (!env.producer.is_empty()).then(|| env.producer.clone()),
                    state,
                    command_observed: true,
                    enqueued_at_ms: now,
                    started_at_ms: None,
                    finished_at_ms: None,
                    error: None,
                };
                let totals = s.kind_mut(kind);
                totals.enqueued += 1;
                if !reports_outcome {
                    totals.unreported += 1;
                }
                s.insert(key, job);
            }

            Kind::Lifecycle {
                state,
                implies_kind,
            } => {
                s.totals.events += 1;

                if s.get_mut(&key).is_none() {
                    // The command predates the subscription. The job is still
                    // real and its outcome still counts; only its start time and
                    // therefore its duration are unknowable.
                    s.totals.orphan_events += 1;
                    let job = CloudJob {
                        id: key.clone(),
                        kind: implies_kind.to_string(),
                        subject: subject.to_string(),
                        target: env.target(),
                        label: env.label(),
                        account_id: env.account_id.clone(),
                        producer: (!env.producer.is_empty()).then(|| env.producer.clone()),
                        state: JobState::Queued,
                        command_observed: false,
                        enqueued_at_ms: now,
                        started_at_ms: None,
                        finished_at_ms: None,
                        error: None,
                    };
                    s.kind_mut(implies_kind).enqueued += 1;
                    s.insert(key.clone(), job);
                }

                let Some(job) = s.get_mut(&key) else {
                    return;
                };

                // Terminal is terminal. A redelivered or replayed event must not
                // count a second outcome, and a late `provisioning` must not
                // walk a finished job backwards.
                if job.state.is_terminal() {
                    return;
                }
                if state == JobState::Running && job.state != JobState::Queued {
                    return;
                }

                job.subject = subject.to_string();
                job.state = state;
                if job.target.is_none() {
                    job.target = env.target();
                }
                if job.label.is_none() {
                    job.label = env.label();
                }

                let job_kind = job.kind.clone();
                let command_observed = job.command_observed;
                let mut duration_ms = None;

                match state {
                    JobState::Running => job.started_at_ms = Some(now),
                    JobState::Succeeded | JobState::Failed => {
                        job.finished_at_ms = Some(now);
                        job.error = env.error();
                        // A duration measured from "when it was noticed" would
                        // be a made-up number, so only fully-observed jobs land
                        // in the histogram.
                        if command_observed {
                            duration_ms = job.duration_ms();
                        }
                    }
                    _ => {}
                }

                match state {
                    JobState::Succeeded => {
                        s.totals.succeeded += 1;
                        s.kind_mut(&job_kind).succeeded += 1;
                    }
                    JobState::Failed => {
                        s.totals.failed += 1;
                        s.kind_mut(&job_kind).failed += 1;
                    }
                    _ => {}
                }
                if let Some(ms) = duration_ms {
                    self.job_ms.record(ms);
                }
            }

            Kind::Signal(_) | Kind::Unknown => unreachable!("handled above"),
        }
    }

    fn record_poll_error(&self, error: String) {
        let mut p = self.poll_state();
        p.polled_at_ms = Some(now_ms());
        p.error = Some(error);
        // Deliberately not cleared: the last good reading, clearly stamped as
        // stale by `polled_at_ms`, beats blanking the panel on one failed poll.
    }

    pub fn snapshot(&self, job_limit: usize) -> CloudSnapshot {
        let s = self.observed();
        let p = self.poll_state();

        let mut by_kind = s.by_kind.clone();
        let mut in_progress = 0u64;
        for job in s.jobs.values() {
            if !job.state.is_terminal() {
                in_progress += 1;
                by_kind.entry(job.kind.clone()).or_default().in_progress += 1;
            }
        }

        // Newest first: the map is keyed by a monotonic sequence.
        let jobs: Vec<CloudJob> = s.jobs.values().rev().take(job_limit).cloned().collect();

        let queue = QueueGauges {
            pending: p.consumers.iter().map(|c| c.num_pending).sum(),
            in_flight: p.consumers.iter().map(|c| c.num_ack_pending).sum(),
            redelivered: p.consumers.iter().map(|c| c.num_redelivered).sum(),
            waiting: p.consumers.iter().map(|c| c.num_waiting).sum(),
            stream_messages: p.stream_info.as_ref().map_or(0, |i| i.messages),
            stream_bytes: p.stream_info.as_ref().map_or(0, |i| i.bytes),
        };

        CloudSnapshot {
            stream: self.stream.clone(),
            subject: self.subject.clone(),
            connected: p.error.is_none() && p.stream_info.is_some(),
            error: p.error.clone(),
            subscribed: self.subscribed.load(Ordering::Relaxed) == 1,
            observed_since_ms: self.observed_since_ms,
            polled_at_ms: p.polled_at_ms,
            queue,
            in_progress,
            observed: s.totals.clone(),
            signals: s.signals.clone(),
            by_kind,
            job_ms: self.job_ms.snapshot(),
            stream_info: p.stream_info.clone(),
            consumers: p.consumers.clone(),
            subjects: p.subjects.clone(),
            jobs_retained: s.jobs.len(),
            jobs,
        }
    }
}

// --- The worker ----------------------------------------------------------

/// Watch the cloud's queue until shutdown.
///
/// Two independent loops: one tails the subscription, one polls the management
/// API. They are separate because they fail separately — a credential without
/// subscribe permission on `sandbox.>` still leaves the backlog gauges working,
/// and a missing stream still leaves the event feed working.
pub async fn run(
    state: std::sync::Arc<CloudState>,
    bus: std::sync::Arc<Bus>,
    poll_interval: Duration,
    shutdown: watch::Receiver<bool>,
) {
    tokio::join!(
        subscribe_loop(state.clone(), bus.clone(), shutdown.clone()),
        poll_loop(state, bus, poll_interval, shutdown),
    );
}

/// True once shutdown has been signalled.
async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
    // The sender is gone, which only happens as the process exits.
}

async fn subscribe_loop(
    state: std::sync::Arc<CloudState>,
    bus: std::sync::Arc<Bus>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut first = true;
    loop {
        if *shutdown.borrow() {
            return;
        }

        let subscription = bus.client().subscribe(state.subject.clone()).await;
        let mut sub = match subscription {
            Ok(sub) => {
                state.set_subscribed(true);
                if first {
                    tracing::info!(subject = %state.subject, "observing the cloud queue");
                } else {
                    state.observed().totals.resubscribes += 1;
                    tracing::info!(subject = %state.subject, "re-subscribed to the cloud queue");
                }
                first = false;
                sub
            }
            Err(e) => {
                state.set_subscribed(false);
                // Most likely a credential without permission on the subject,
                // which is a configuration answer rather than a transient one —
                // but retrying costs nothing and the poll loop keeps working.
                tracing::warn!(
                    subject = %state.subject,
                    error = %e,
                    "could not subscribe to the cloud queue; retrying",
                );
                tokio::select! {
                    _ = wait_for_shutdown(&mut shutdown) => return,
                    _ = tokio::time::sleep(RESUBSCRIBE_DELAY) => continue,
                }
            }
        };

        loop {
            tokio::select! {
                _ = wait_for_shutdown(&mut shutdown) => {
                    state.set_subscribed(false);
                    return;
                }
                message = sub.next() => {
                    let Some(message) = message else { break };
                    let dedupe = message
                        .headers
                        .as_ref()
                        .and_then(|h| h.get("Nats-Msg-Id"))
                        .map(|v| v.as_str());
                    state.observe(
                        message.subject.as_str(),
                        dedupe,
                        &message.payload,
                        now_ms(),
                    );
                }
            }
        }

        state.set_subscribed(false);
        tracing::warn!("the cloud queue subscription ended; reconnecting");
        tokio::select! {
            _ = wait_for_shutdown(&mut shutdown) => return,
            _ = tokio::time::sleep(RESUBSCRIBE_DELAY) => {}
        }
    }
}

async fn poll_loop(
    state: std::sync::Arc<CloudState>,
    bus: std::sync::Arc<Bus>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = wait_for_shutdown(&mut shutdown) => return,
            _ = ticker.tick() => poll_once(&state, &bus).await,
        }
    }
}

async fn poll_once(state: &CloudState, bus: &Bus) {
    let stream = match bus.jetstream().get_stream(state.stream.clone()).await {
        Ok(s) => s,
        Err(e) => {
            state.record_poll_error(format!("stream {}: {e}", state.stream));
            return;
        }
    };

    // One request that returns both the stream state and the per-subject
    // counts, rather than two.
    let mut with_subjects = match stream.info_with_subjects(">").await {
        Ok(i) => i,
        Err(e) => {
            state.record_poll_error(format!("stream info: {e}"));
            return;
        }
    };
    let info = with_subjects.info.clone();

    let mut subjects = Vec::new();
    while let Some(entry) = with_subjects.next().await {
        match entry {
            Ok((subject, messages)) => subjects.push(CloudSubjectStat {
                subject,
                messages: messages as u64,
            }),
            Err(e) => {
                state.record_poll_error(format!("subject stats: {e}"));
                return;
            }
        }
    }
    subjects.sort_by(|a, b| b.messages.cmp(&a.messages).then(a.subject.cmp(&b.subject)));
    subjects.truncate(MAX_SUBJECT_ROWS);

    let mut consumers = Vec::new();
    let mut listing = stream.consumers();
    while let Some(entry) = listing.next().await {
        match entry {
            Ok(c) => consumers.push(CloudConsumer {
                name: c.name,
                filter_subject: c.config.filter_subject,
                ack_wait_secs: c.config.ack_wait.as_secs(),
                num_pending: c.num_pending,
                num_ack_pending: c.num_ack_pending as u64,
                num_redelivered: c.num_redelivered as u64,
                num_waiting: c.num_waiting as u64,
                delivered_stream_seq: c.delivered.stream_sequence,
                ack_floor_stream_seq: c.ack_floor.stream_sequence,
                last_active_ms: c
                    .delivered
                    .last_active
                    .or(c.ack_floor.last_active)
                    .map(|ts| ts.unix_timestamp() * 1000),
            }),
            Err(e) => {
                state.record_poll_error(format!("consumer listing: {e}"));
                return;
            }
        }
    }
    consumers.sort_by(|a, b| a.name.cmp(&b.name));

    let stream_info = CloudStreamInfo {
        retention: format!("{:?}", info.config.retention).to_lowercase(),
        storage: format!("{:?}", info.config.storage).to_lowercase(),
        subjects: info.config.subjects.clone(),
        messages: info.state.messages,
        bytes: info.state.bytes,
        consumer_count: info.state.consumer_count,
        first_seq: info.state.first_sequence,
        last_seq: info.state.last_sequence,
        last_message_ms: info.state.last_timestamp.unix_timestamp() * 1000,
    };

    let mut p = state.poll_state();
    p.polled_at_ms = Some(now_ms());
    p.error = None;
    p.stream_info = Some(stream_info);
    p.consumers = consumers;
    p.subjects = subjects;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> CloudState {
        CloudState::new(DEFAULT_STREAM.into(), DEFAULT_SUBJECT.into(), 100)
    }

    fn envelope(correlation: &str, payload: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "message_id": format!("msg-{correlation}"),
            "event_type": "test",
            "payload_version": 1,
            "occurred_at": "2026-01-01T00:00:00Z",
            "correlation_id": correlation,
            "idempotency_key": format!("key-{correlation}"),
            "producer": "cloud",
            "payload": payload,
        }))
        .unwrap()
    }

    fn create(correlation: &str, deployment: &str) -> Vec<u8> {
        envelope(
            correlation,
            serde_json::json!({ "deployment_id": deployment, "name": "web" }),
        )
    }

    fn lifecycle(correlation: &str, deployment: &str, status: &str) -> Vec<u8> {
        envelope(
            correlation,
            serde_json::json!({
                "deployment_id": deployment,
                "status": status,
                "request": { "name": "web" },
            }),
        )
    }

    #[test]
    fn a_command_and_its_events_are_one_job() {
        let s = state();
        s.observe(
            "sandbox.cmd.create",
            Some("id-1"),
            &create("c1", "d1"),
            1_000,
        );
        assert_eq!(s.snapshot(10).jobs.len(), 1);

        s.observe(
            "sandbox.evt.provisioning",
            Some("id-2"),
            &lifecycle("c1", "d1", "provisioning"),
            2_000,
        );
        s.observe(
            "sandbox.evt.ready",
            Some("id-3"),
            &lifecycle("c1", "d1", "running"),
            9_000,
        );

        let snap = s.snapshot(10);
        assert_eq!(
            snap.jobs.len(),
            1,
            "the events joined the command, not new rows"
        );
        let job = &snap.jobs[0];
        assert_eq!(job.state, JobState::Succeeded);
        assert_eq!(job.kind, "sandbox.create");
        assert_eq!(job.target.as_deref(), Some("d1"));
        assert_eq!(job.label.as_deref(), Some("web"));
        assert_eq!(job.started_at_ms, Some(2_000));
        assert_eq!(job.finished_at_ms, Some(9_000));
        assert_eq!(snap.observed.succeeded, 1);
        assert_eq!(snap.observed.failed, 0);
        assert_eq!(snap.in_progress, 0);
        assert_eq!(snap.job_ms.count, 1, "8s of wall clock was recorded");
        assert_eq!(snap.by_kind["sandbox.create"].succeeded, 1);
    }

    #[test]
    fn a_failed_job_carries_its_error() {
        let s = state();
        s.observe("sandbox.cmd.create", Some("id-1"), &create("c1", "d1"), 0);
        let failed = envelope(
            "c1",
            serde_json::json!({
                "deployment_id": "d1",
                "status": "failed",
                "error": "no backend with capacity",
            }),
        );
        s.observe("sandbox.evt.failed", Some("id-2"), &failed, 5_000);

        let snap = s.snapshot(10);
        assert_eq!(snap.jobs[0].state, JobState::Failed);
        assert_eq!(
            snap.jobs[0].error.as_deref(),
            Some("no backend with capacity")
        );
        assert_eq!(snap.observed.failed, 1);
        assert_eq!(snap.by_kind["sandbox.create"].failed, 1);
    }

    /// Restart and wake are acked by their workers without publishing anything,
    /// so calling them successes would be inventing an outcome the bus never
    /// reported.
    #[test]
    fn a_command_with_no_lifecycle_event_is_unreported_not_successful() {
        let s = state();
        let wake = envelope(
            "c1",
            serde_json::json!({ "sandbox_id": "sb-1", "subdomain": "abc" }),
        );
        s.observe("sandbox.cmd.wake", Some("id-1"), &wake, 0);

        let snap = s.snapshot(10);
        assert_eq!(snap.jobs[0].state, JobState::Unreported);
        assert_eq!(snap.jobs[0].kind, "sandbox.wake");
        assert_eq!(snap.observed.succeeded, 0);
        assert_eq!(snap.observed.unreported, 1);
        assert_eq!(
            snap.in_progress, 0,
            "an unreported job is finished with the queue, not stuck in it"
        );
    }

    /// The wake path publishes a storm of commands per 30s bucket on purpose and
    /// lets JetStream's `Nats-Msg-Id` dedupe collapse them. Counting each publish
    /// would show a queue depth the server never had.
    #[test]
    fn a_publish_deduped_by_the_server_is_not_a_second_job() {
        let s = state();
        let wake = envelope("c1", serde_json::json!({ "sandbox_id": "sb-1" }));
        let wake_again = envelope("c2", serde_json::json!({ "sandbox_id": "sb-1" }));
        s.observe("sandbox.cmd.wake", Some("sandbox:wake:sb-1:1"), &wake, 0);
        s.observe(
            "sandbox.cmd.wake",
            Some("sandbox:wake:sb-1:1"),
            &wake_again,
            10,
        );

        let snap = s.snapshot(10);
        assert_eq!(snap.jobs.len(), 1);
        assert_eq!(snap.observed.duplicates, 1);
        assert_eq!(snap.by_kind["sandbox.wake"].enqueued, 1);
    }

    /// queue-fn starting mid-flight sees the outcome but not the command. The
    /// outcome still counts; the duration does not, because its start is a guess.
    #[test]
    fn an_event_without_its_command_still_counts_but_reports_no_duration() {
        let s = state();
        s.observe(
            "sandbox.evt.ready",
            Some("id-1"),
            &lifecycle("c9", "d9", "running"),
            5_000,
        );

        let snap = s.snapshot(10);
        assert_eq!(snap.jobs.len(), 1);
        assert!(!snap.jobs[0].command_observed);
        assert_eq!(snap.jobs[0].state, JobState::Succeeded);
        assert_eq!(snap.observed.succeeded, 1);
        assert_eq!(snap.observed.orphan_events, 1);
        assert_eq!(
            snap.job_ms.count, 0,
            "a duration measured from when it was noticed would be fabricated"
        );
    }

    /// Redelivery is normal on a WorkQueue stream, and the events consumer naks
    /// on failure. A second copy of a terminal event must not count twice.
    #[test]
    fn a_repeated_terminal_event_counts_once() {
        let s = state();
        s.observe("sandbox.cmd.create", Some("id-1"), &create("c1", "d1"), 0);
        let ready = lifecycle("c1", "d1", "running");
        s.observe("sandbox.evt.ready", Some("id-2"), &ready, 1_000);
        s.observe("sandbox.evt.ready", Some("id-2"), &ready, 2_000);

        let snap = s.snapshot(10);
        assert_eq!(snap.observed.succeeded, 1);
        assert_eq!(snap.jobs[0].finished_at_ms, Some(1_000));
    }

    #[test]
    fn a_late_provisioning_event_does_not_reopen_a_finished_job() {
        let s = state();
        s.observe("sandbox.cmd.create", Some("id-1"), &create("c1", "d1"), 0);
        s.observe(
            "sandbox.evt.ready",
            Some("id-2"),
            &lifecycle("c1", "d1", "running"),
            1_000,
        );
        s.observe(
            "sandbox.evt.provisioning",
            Some("id-3"),
            &lifecycle("c1", "d1", "provisioning"),
            2_000,
        );

        let snap = s.snapshot(10);
        assert_eq!(snap.jobs[0].state, JobState::Succeeded);
        assert_eq!(snap.in_progress, 0);
    }

    #[test]
    fn sqlite_operations_are_tracked_as_their_own_kinds() {
        let s = state();
        let payload = serde_json::json!({ "database_id": "db-1", "name": "app" });
        s.observe(
            "sandbox.sqlite.cmd.snapshot",
            Some("id-1"),
            &envelope("c1", payload.clone()),
            0,
        );
        s.observe(
            "sandbox.sqlite.evt.snapshotted",
            Some("id-2"),
            &envelope("c1", payload),
            3_000,
        );

        let snap = s.snapshot(10);
        assert_eq!(snap.jobs[0].kind, "sqlite.snapshot");
        assert_eq!(snap.jobs[0].state, JobState::Succeeded);
        assert_eq!(snap.jobs[0].target.as_deref(), Some("db-1"));
        assert_eq!(snap.by_kind["sqlite.snapshot"].succeeded, 1);
    }

    #[test]
    fn telemetry_is_counted_but_is_not_a_job() {
        let s = state();
        let usage = envelope("c1", serde_json::json!({ "sandbox_id": "sb-1" }));
        s.observe("sandbox.usage.started", Some("id-1"), &usage, 0);
        s.observe("sandbox.usage.stopped", Some("id-2"), &usage, 1);
        s.observe("sandbox.health.stuck", Some("id-3"), &usage, 2);
        s.observe("sandbox.sqlite.evt.mutated", Some("id-4"), &usage, 3);
        s.observe(
            "sandbox.sqlite.size.sample_request",
            Some("id-5"),
            &usage,
            4,
        );

        let snap = s.snapshot(10);
        assert!(snap.jobs.is_empty(), "telemetry starts no job");
        assert_eq!(snap.signals.usage_started, 1);
        assert_eq!(snap.signals.usage_stopped, 1);
        assert_eq!(snap.signals.health_stuck, 1);
        assert_eq!(snap.signals.sqlite_mutated, 1);
        assert_eq!(snap.signals.sqlite_size_samples, 1);
        assert_eq!(snap.observed.signals, 5);
    }

    /// A subject the cloud adds later must read as "traffic queue-fn cannot
    /// interpret", not as no traffic at all.
    #[test]
    fn an_unrecognised_subject_is_counted_rather_than_dropped() {
        let s = state();
        s.observe("sandbox.something.new", None, b"{}", 0);
        let snap = s.snapshot(10);
        assert_eq!(snap.observed.unknown_subjects, 1);
        assert_eq!(snap.observed.messages, 1);
        assert!(snap.jobs.is_empty());
    }

    #[test]
    fn an_undecodable_body_does_not_derail_the_observer() {
        let s = state();
        s.observe("sandbox.cmd.create", Some("id-1"), b"not json", 0);
        s.observe("sandbox.cmd.create", Some("id-2"), b"{}", 0);
        let snap = s.snapshot(10);
        assert_eq!(
            snap.observed.undecodable, 2,
            "an envelope with no correlation id is as unusable as unparseable bytes"
        );
        assert!(snap.jobs.is_empty());
        assert_eq!(snap.observed.messages, 2);
    }

    #[test]
    fn the_job_ring_drops_the_oldest_and_keeps_the_newest() {
        let s = CloudState::new(DEFAULT_STREAM.into(), DEFAULT_SUBJECT.into(), 3);
        for i in 0..5 {
            let c = format!("c{i}");
            s.observe(
                "sandbox.cmd.create",
                Some(&format!("id-{i}")),
                &create(&c, &format!("d{i}")),
                i as u64,
            );
        }
        let snap = s.snapshot(10);
        let ids: Vec<_> = snap.jobs.iter().map(|j| j.id.as_str()).collect();
        assert_eq!(ids, ["c4", "c3", "c2"], "newest first, oldest evicted");
        assert_eq!(snap.jobs_retained, 3);
        assert_eq!(
            snap.by_kind["sandbox.create"].enqueued, 5,
            "the counters are monotonic even though the ring is not"
        );
    }

    /// Eviction must clean the correlation index too, or a job's slot is held by
    /// a key pointing at a row that is gone.
    #[test]
    fn evicting_a_job_forgets_its_correlation_id() {
        let s = CloudState::new(DEFAULT_STREAM.into(), DEFAULT_SUBJECT.into(), 2);
        for i in 0..3 {
            let c = format!("c{i}");
            s.observe(
                "sandbox.cmd.create",
                Some(&format!("id-{i}")),
                &create(&c, "d"),
                0,
            );
        }
        // c0 has been evicted; its terminal event now looks like an orphan
        // rather than silently updating nothing.
        s.observe(
            "sandbox.evt.ready",
            Some("id-x"),
            &lifecycle("c0", "d", "running"),
            10,
        );
        let snap = s.snapshot(10);
        assert_eq!(snap.observed.orphan_events, 1);
        assert_eq!(snap.observed.succeeded, 1);
    }

    #[test]
    fn in_progress_counts_only_unfinished_jobs() {
        let s = state();
        s.observe("sandbox.cmd.create", Some("id-1"), &create("c1", "d1"), 0);
        s.observe("sandbox.cmd.create", Some("id-2"), &create("c2", "d2"), 0);
        s.observe(
            "sandbox.evt.provisioning",
            Some("id-3"),
            &lifecycle("c2", "d2", "provisioning"),
            1,
        );
        s.observe("sandbox.cmd.create", Some("id-4"), &create("c3", "d3"), 0);
        s.observe(
            "sandbox.evt.ready",
            Some("id-5"),
            &lifecycle("c3", "d3", "running"),
            2,
        );

        let snap = s.snapshot(10);
        assert_eq!(snap.in_progress, 2, "one queued, one running, one finished");
        assert_eq!(snap.by_kind["sandbox.create"].in_progress, 2);
    }

    #[test]
    fn the_classification_table_covers_every_cloud_subject() {
        // Every subject `cloud/src/sandbox_messages.rs` publishes on.
        for subject in [
            "sandbox.cmd.create",
            "sandbox.cmd.restart",
            "sandbox.cmd.wake",
            "sandbox.evt.provisioning",
            "sandbox.evt.ready",
            "sandbox.evt.failed",
            "sandbox.sqlite.cmd.create",
            "sandbox.sqlite.cmd.delete",
            "sandbox.sqlite.cmd.snapshot",
            "sandbox.sqlite.cmd.restore",
            "sandbox.sqlite.evt.provisioning",
            "sandbox.sqlite.evt.ready",
            "sandbox.sqlite.evt.failed",
            "sandbox.sqlite.evt.deleted",
            "sandbox.sqlite.evt.snapshotted",
            "sandbox.sqlite.evt.restored",
            "sandbox.sqlite.evt.mutated",
            "sandbox.sqlite.size.sample_request",
            "sandbox.usage.started",
            "sandbox.usage.stopped",
            "sandbox.health.stuck",
        ] {
            assert!(
                !matches!(classify(subject), Kind::Unknown),
                "{subject} is published by cloud but not classified here",
            );
        }
    }

    /// A snapshot taken before the first poll must render as "not read yet", not
    /// as an empty queue — the two look identical in a tile otherwise.
    #[test]
    fn an_unpolled_snapshot_is_disconnected_rather_than_empty() {
        let snap = state().snapshot(10);
        assert!(!snap.connected);
        assert!(snap.polled_at_ms.is_none());
        assert!(snap.stream_info.is_none());
        assert_eq!(snap.queue.pending, 0);
    }

    #[test]
    fn a_failed_poll_keeps_the_last_good_reading_and_says_it_failed() {
        let s = state();
        {
            let mut p = s.poll_state();
            p.polled_at_ms = Some(1);
            p.consumers = vec![CloudConsumer {
                name: "sandbox-create-worker".into(),
                filter_subject: "sandbox.cmd.create".into(),
                ack_wait_secs: 2100,
                num_pending: 4,
                num_ack_pending: 1,
                num_redelivered: 0,
                num_waiting: 0,
                delivered_stream_seq: 10,
                ack_floor_stream_seq: 9,
                last_active_ms: None,
            }];
        }
        s.record_poll_error("stream HEYO_SANDBOX: not found".into());

        let snap = s.snapshot(10);
        assert!(!snap.connected);
        assert!(snap.error.is_some());
        assert_eq!(
            snap.queue.pending, 4,
            "the last reading is kept, stamped stale"
        );
    }

    #[test]
    fn queue_gauges_sum_across_consumers() {
        let s = state();
        {
            let mut p = s.poll_state();
            p.polled_at_ms = Some(1);
            p.stream_info = Some(CloudStreamInfo {
                retention: "workqueue".into(),
                storage: "file".into(),
                subjects: vec!["sandbox.>".into()],
                messages: 7,
                bytes: 2048,
                consumer_count: 2,
                first_seq: 1,
                last_seq: 7,
                last_message_ms: 0,
            });
            p.consumers = vec![
                CloudConsumer {
                    name: "a".into(),
                    filter_subject: "sandbox.cmd.create".into(),
                    ack_wait_secs: 60,
                    num_pending: 3,
                    num_ack_pending: 1,
                    num_redelivered: 2,
                    num_waiting: 1,
                    delivered_stream_seq: 0,
                    ack_floor_stream_seq: 0,
                    last_active_ms: None,
                },
                CloudConsumer {
                    name: "b".into(),
                    filter_subject: "sandbox.evt.>".into(),
                    ack_wait_secs: 60,
                    num_pending: 4,
                    num_ack_pending: 2,
                    num_redelivered: 0,
                    num_waiting: 3,
                    delivered_stream_seq: 0,
                    ack_floor_stream_seq: 0,
                    last_active_ms: None,
                },
            ];
        }
        let snap = s.snapshot(10);
        assert!(snap.connected);
        assert_eq!(snap.queue.pending, 7);
        assert_eq!(snap.queue.in_flight, 3);
        assert_eq!(snap.queue.redelivered, 2);
        assert_eq!(snap.queue.waiting, 4);
        assert_eq!(snap.queue.stream_messages, 7);
    }

    #[test]
    fn the_job_list_honours_its_limit_and_says_how_many_are_held() {
        let s = state();
        for i in 0..10 {
            s.observe(
                "sandbox.cmd.create",
                Some(&format!("id-{i}")),
                &create(&format!("c{i}"), "d"),
                0,
            );
        }
        let snap = s.snapshot(3);
        assert_eq!(snap.jobs.len(), 3);
        assert_eq!(snap.jobs_retained, 10);
    }
}
