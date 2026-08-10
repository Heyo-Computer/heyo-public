//! The NATS JetStream event bus: the single path every invocation takes.
//!
//! Async publishes, synchronous HTTP invokes, and scheduled triggers all become
//! a message on `<prefix>.invoke.<function>`, pulled by that function's durable
//! consumer. One dispatch path means one demand signal, and the demand signal is
//! the whole point: `num_pending` on the consumer is what tells the autoscaler
//! there is work for a function whose pool is empty.
//!
//! **Own stream, not heyvm's.** heyvm has its own JetStream queue
//! (`HEYO_MVM_CMD`, `mvm.cmd.<backend>.{create,delete}`) but it carries VM
//! lifecycle, not exec, and it has its own retention and dispatcher. Sharing it
//! would mean a queue-fn purge destroying pending VM creates. The client version
//! is pinned to heyvm's so that if the two are ever merged it is a routing
//! change and not a dependency upgrade.
//!
//! **`WorkQueue` retention is load-bearing, and it makes queue-fn
//! single-instance.** It deletes on ack, so the stream's message count *is* the
//! backlog — self-clearing, and exactly what `desired_replicas` wants. The cost
//! is that JetStream permits only one consumer per subject under that policy, so
//! two queue-fn processes against one NATS would collide. app-lb is likewise a
//! single writer, so this is consistent rather than a new constraint.

use crate::config::FunctionSpec;
use crate::nats_auth::{Credential, NatsEndpoint};
use crate::vm::ExecOutput;
use async_nats::HeaderMap;
use async_nats::jetstream::{
    self, consumer,
    stream::{DiscardPolicy, RetentionPolicy, StorageType},
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Slack above a function's own timeout before JetStream assumes the dispatcher
/// died and redelivers. Must exceed the exec plus the poll interval, or a
/// perfectly healthy invocation is redelivered while it is still running.
const ACK_WAIT_SLACK_SECS: u64 = 15;

/// Redelivery ladder. Indexed by attempt, saturating at the last entry.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];

pub fn backoff_for(attempt: u32) -> Duration {
    let idx = (attempt as usize).saturating_sub(1).min(BACKOFF.len() - 1);
    BACKOFF[idx]
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Monotonic counter making concurrently-minted ids in the same millisecond
/// distinct.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Mint an invocation id: `<epoch_ms:012x>-<seq:08x>`.
///
/// Hex and dash only, so it is directly usable as the daemon's `operationId`
/// (`[A-Za-z0-9._-]`, ≤160 bytes — mvm-ctrl/src/api.rs:5283) and as a NATS
/// subject token. Time-ordered so a sorted listing reads chronologically.
pub fn new_invocation_id() -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{:012x}-{:08x}", now_ms(), seq & 0xffff_ffff)
}

/// Where an event came from. Surfaced in the dashboard's recent table, and in
/// `QFN_SOURCE` inside the guest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    #[default]
    Invoke,
    Schedule,
    Replay,
}

impl EventSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Invoke => "invoke",
            Self::Schedule => "schedule",
            Self::Replay => "replay",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeEvent {
    /// Doubles as the daemon's `operationId`, which is what makes a redelivery
    /// idempotent rather than a second execution. See `invoke::run`.
    pub invocation_id: String,
    pub function_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Core-NATS subject to publish the result to. Set by a synchronous caller
    /// that is blocked waiting for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_subject: Option<String>,
    pub enqueued_at_ms: u64,
    #[serde(default)]
    pub source: EventSource,
}

impl InvokeEvent {
    pub fn new(function_id: &str, payload: Option<serde_json::Value>, source: EventSource) -> Self {
        Self {
            invocation_id: new_invocation_id(),
            function_id: function_id.to_string(),
            payload,
            reply_subject: None,
            enqueued_at_ms: now_ms(),
            source,
        }
    }

    /// Milliseconds this event spent queued before being dispatched. The number
    /// that separates "the fleet is undersized" from "the function is slow".
    pub fn queue_wait_ms(&self) -> u64 {
        now_ms().saturating_sub(self.enqueued_at_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Exit code 0.
    Success,
    /// The command ran and exited non-zero.
    Failed,
    /// The command did not finish within the function's timeout.
    Timeout,
    /// The daemon or the bus errored; no exit code was ever produced.
    Error,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvokeResult {
    pub invocation_id: String,
    pub function_id: String,
    #[serde(default)]
    pub sandbox_id: String,
    pub outcome: Outcome,
    pub exit_code: i32,
    pub stdout: String,
    /// Empty on the firecracker serial path: the daemon runs `(cmd) 2>&1` and
    /// returns an empty stderr, so both streams arrive folded into `stdout`.
    pub stderr: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl InvokeResult {
    pub fn from_exec(
        event: &InvokeEvent,
        sandbox_id: &str,
        out: ExecOutput,
        duration_ms: u64,
        attempt: u32,
    ) -> Self {
        Self {
            invocation_id: event.invocation_id.clone(),
            function_id: event.function_id.clone(),
            sandbox_id: sandbox_id.to_string(),
            outcome: if out.succeeded() {
                Outcome::Success
            } else {
                Outcome::Failed
            },
            exit_code: out.exit_code,
            stdout: out.stdout,
            stderr: out.stderr,
            duration_ms,
            attempt,
            error: None,
        }
    }

    pub fn failure(
        event: &InvokeEvent,
        sandbox_id: &str,
        outcome: Outcome,
        error: String,
        duration_ms: u64,
        attempt: u32,
    ) -> Self {
        Self {
            invocation_id: event.invocation_id.clone(),
            function_id: event.function_id.clone(),
            sandbox_id: sandbox_id.to_string(),
            outcome,
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms,
            attempt,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DlqRecord {
    pub event: InvokeEvent,
    pub attempts: u32,
    pub last_error: String,
    pub failed_at_ms: u64,
}

/// What the consumer says about a function's backlog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct QueueDepth {
    /// Waiting, undelivered. This is the autoscaler's input.
    pub pending: u64,
    /// Delivered to a dispatcher and not yet acked.
    pub ack_pending: u64,
}

#[derive(Debug)]
pub enum BusError {
    Connect(String),
    Stream(String),
    Consumer(String),
    Publish(String),
    /// Rejected before publishing: a retry cannot make a payload smaller.
    PayloadTooLarge { got: usize, max: usize },
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "NATS connection failed: {e}"),
            Self::Stream(e) => write!(f, "JetStream stream error: {e}"),
            Self::Consumer(e) => write!(f, "JetStream consumer error: {e}"),
            Self::Publish(e) => write!(f, "publish failed: {e}"),
            Self::PayloadTooLarge { got, max } => write!(
                f,
                "payload is {got} bytes, over the limit of {max}: it crosses a serial \
                 console as one command line, so an oversized one would be truncated \
                 rather than delivered"
            ),
        }
    }
}

impl std::error::Error for BusError {}

/// Subject and consumer naming. Kept together so the one-consumer-per-subject
/// invariant that `WorkQueue` retention demands is checkable by reading them.
pub fn invoke_subject(prefix: &str, function_id: &str) -> String {
    format!("{prefix}.invoke.{function_id}")
}
pub fn result_subject(prefix: &str, function_id: &str, invocation_id: &str) -> String {
    format!("{prefix}.result.{function_id}.{invocation_id}")
}
pub fn dlq_subject(prefix: &str, function_id: &str) -> String {
    format!("{prefix}.dlq.{function_id}")
}
/// Core-NATS reply subject for a blocked synchronous caller. Not a JetStream
/// subject: nobody needs a durable record of a reply whose only reader is a
/// request that will have timed out by the time it could be replayed.
pub fn reply_subject(prefix: &str, invocation_id: &str) -> String {
    format!("{prefix}.reply.{invocation_id}")
}
pub fn consumer_name(function_id: &str) -> String {
    format!("qfn-{function_id}")
}

fn events_stream(prefix: &str) -> String {
    format!("{}_EVENTS", prefix.to_uppercase())
}
fn results_stream(prefix: &str) -> String {
    format!("{}_RESULTS", prefix.to_uppercase())
}
fn dlq_stream(prefix: &str) -> String {
    format!("{}_DLQ", prefix.to_uppercase())
}

#[derive(Clone)]
pub struct Bus {
    client: async_nats::Client,
    js: jetstream::Context,
    prefix: String,
}

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bus").field("prefix", &self.prefix).finish()
    }
}

impl Bus {
    /// Connect to a resolved endpoint.
    ///
    /// Takes a `NatsEndpoint` rather than a URL so that credentials cannot
    /// reach this layer still embedded in a string — `async_nats` would parse
    /// them, drop them, and fail with an authorization error that names nothing
    /// (see `nats_auth`'s module doc).
    pub async fn connect(endpoint: &NatsEndpoint, prefix: String) -> Result<Self, BusError> {
        let mut opts = async_nats::ConnectOptions::new()
            // Named so the credential's owner is identifiable in `nats server
            // report connections` — with shared infrastructure, an unnamed
            // client is indistinguishable from any other consumer of the same
            // account.
            .name("queue-fn");
        opts = match &endpoint.credential {
            Credential::None => opts,
            Credential::Token(t) => opts.token(t.clone()),
            Credential::UserPassword { user, password } => {
                opts.user_and_password(user.clone(), password.clone())
            }
            Credential::NKeySeed(seed) => opts.nkey(seed.clone()),
            Credential::Creds(contents) => opts
                .credentials(contents)
                .map_err(|e| BusError::Connect(format!("invalid NATS creds file: {e}")))?,
        };

        // Split into `ServerAddr`s rather than passing the joined string:
        // `ToServerAddrs for str` parses exactly one address, so a cluster URL
        // would be a parse error instead of a failover list.
        let servers: Vec<async_nats::ServerAddr> = endpoint
            .servers
            .iter()
            .map(|s| s.parse())
            .collect::<Result<_, _>>()
            .map_err(|e| BusError::Connect(format!("invalid NATS server address: {e}")))?;

        let client = opts
            .connect(servers)
            .await
            .map_err(|e| BusError::Connect(e.to_string()))?;
        let js = jetstream::new(client.clone());
        Ok(Self { client, js, prefix })
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn client(&self) -> &async_nats::Client {
        &self.client
    }

    /// The JetStream context, for read-only management calls against streams
    /// queue-fn does not own — see `cloud`, which polls the cloud service's
    /// stream and consumers without binding to either.
    pub fn jetstream(&self) -> &jetstream::Context {
        &self.js
    }

    /// Create the three streams if they are absent, and leave them alone if not.
    ///
    /// Awaited before any dispatcher is spawned, so consumer creation never
    /// races stream creation.
    pub async fn ensure_streams(&self) -> Result<(), BusError> {
        // Events: WorkQueue so message count == backlog, and acked work is
        // deleted rather than accumulating. File storage so a restart does not
        // drop queued work on the floor.
        self.js
            .get_or_create_stream(jetstream::stream::Config {
                name: events_stream(&self.prefix),
                subjects: vec![format!("{}.invoke.*", self.prefix)],
                retention: RetentionPolicy::WorkQueue,
                storage: StorageType::File,
                // Dedupe window for `Nats-Msg-Id`. Wider than any plausible
                // publish retry, so a double publish of the same invocation is
                // collapsed rather than run twice.
                duplicate_window: Duration::from_secs(120),
                ..Default::default()
            })
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;

        // Results: an audit trail that expires on its own. Limits retention, so
        // reading is non-destructive and several readers can tail it.
        self.js
            .get_or_create_stream(jetstream::stream::Config {
                name: results_stream(&self.prefix),
                subjects: vec![format!("{}.result.*.*", self.prefix)],
                retention: RetentionPolicy::Limits,
                storage: StorageType::File,
                max_age: Duration::from_secs(3600),
                discard: DiscardPolicy::Old,
                ..Default::default()
            })
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;

        // DLQ: a week, so a Monday-morning look at a Friday-night failure still
        // finds it. Also Limits, so peeking does not consume.
        self.js
            .get_or_create_stream(jetstream::stream::Config {
                name: dlq_stream(&self.prefix),
                subjects: vec![format!("{}.dlq.*", self.prefix)],
                retention: RetentionPolicy::Limits,
                storage: StorageType::File,
                max_age: Duration::from_secs(7 * 24 * 3600),
                discard: DiscardPolicy::Old,
                ..Default::default()
            })
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;

        Ok(())
    }

    fn pull_config(spec: &FunctionSpec, prefix: &str) -> consumer::pull::Config {
        // The server rejects a consumer whose backoff ladder is not shorter than
        // max_deliver ("max deliver is required to be > length of backoff
        // values"), which is also the honest shape: a ladder has one rung per
        // *retry*, and `max_attempts` counts the first delivery too.
        let retries = spec.retry.max_attempts.saturating_sub(1) as usize;
        let backoff = BACKOFF[..retries.min(BACKOFF.len())].to_vec();

        consumer::pull::Config {
            durable_name: Some(consumer_name(&spec.id)),
            filter_subject: invoke_subject(prefix, &spec.id),
            ack_policy: consumer::AckPolicy::Explicit,
            // Must outlast one exec, or JetStream redelivers work that is still
            // running and the function runs twice.
            ack_wait: Duration::from_secs(spec.exec.timeout_secs + ACK_WAIT_SLACK_SECS),
            max_deliver: spec.retry.max_attempts as i64,
            // Never hand out more than the fleet could ever be running at once.
            max_ack_pending: (spec.scaling.max_replicas.max(1)
                * spec.scaling.target_concurrency.max(1)) as i64,
            backoff,
            ..Default::default()
        }
    }

    /// Create or update a function's durable consumer.
    ///
    /// Registration fails if this fails: a function with no consumer would
    /// accept enqueues that nothing will ever pull, which is worse than an error
    /// at registration time.
    pub async fn ensure_consumer(&self, spec: &FunctionSpec) -> Result<(), BusError> {
        let stream = self
            .js
            .get_stream(events_stream(&self.prefix))
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;
        stream
            .create_consumer(Self::pull_config(spec, &self.prefix))
            .await
            .map_err(|e| BusError::Consumer(e.to_string()))?;
        Ok(())
    }

    pub async fn consumer(
        &self,
        function_id: &str,
    ) -> Result<consumer::Consumer<consumer::pull::Config>, BusError> {
        let stream = self
            .js
            .get_stream(events_stream(&self.prefix))
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;
        stream
            .get_consumer(&consumer_name(function_id))
            .await
            .map_err(|e| BusError::Consumer(e.to_string()))
    }

    pub async fn delete_consumer(&self, function_id: &str) -> Result<(), BusError> {
        let stream = self
            .js
            .get_stream(events_stream(&self.prefix))
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;
        match stream.delete_consumer(&consumer_name(function_id)).await {
            Ok(_) => Ok(()),
            // Already gone is the desired end state, not a failure.
            Err(e) if e.to_string().contains("not found") => Ok(()),
            Err(e) => Err(BusError::Consumer(e.to_string())),
        }
    }

    /// Current backlog for a function.
    pub async fn depth(&self, function_id: &str) -> Result<QueueDepth, BusError> {
        let mut consumer = self.consumer(function_id).await?;
        let info = consumer
            .info()
            .await
            .map_err(|e| BusError::Consumer(e.to_string()))?;
        Ok(QueueDepth {
            pending: info.num_pending,
            ack_pending: info.num_ack_pending as u64,
        })
    }

    /// Publish an event, enforcing the payload limit first.
    ///
    /// `Nats-Msg-Id` is the invocation id, so JetStream collapses a duplicate
    /// publish inside the dedupe window. That is the outer of two idempotency
    /// layers; the inner one is the daemon's `operationId` record.
    pub async fn publish(&self, event: &InvokeEvent, max_payload: usize) -> Result<(), BusError> {
        let body = serde_json::to_vec(event)
            .map_err(|e| BusError::Publish(format!("could not serialize event: {e}")))?;

        if let Some(payload) = &event.payload {
            let size = serde_json::to_vec(payload).map(|v| v.len()).unwrap_or(0);
            if size > max_payload {
                return Err(BusError::PayloadTooLarge {
                    got: size,
                    max: max_payload,
                });
            }
        }

        let mut headers = HeaderMap::new();
        headers.insert("Nats-Msg-Id", event.invocation_id.as_str());

        self.js
            .publish_with_headers(
                invoke_subject(&self.prefix, &event.function_id),
                headers,
                body.into(),
            )
            .await
            .map_err(|e| BusError::Publish(e.to_string()))?
            .await
            .map_err(|e| BusError::Publish(e.to_string()))?;
        Ok(())
    }

    /// Record a result on the audit stream, and to a blocked caller's reply
    /// subject if there is one.
    ///
    /// A reply-subject failure is logged rather than returned: the invocation
    /// itself succeeded, and failing it because nobody was listening for the
    /// answer would turn a delivered result into a retry.
    pub async fn publish_result(&self, result: &InvokeResult, reply_to: Option<&str>) {
        let body = match serde_json::to_vec(result) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "could not serialize result");
                return;
            }
        };

        if let Some(subject) = reply_to
            && let Err(e) = self
                .client
                .publish(subject.to_string(), body.clone().into())
                .await
        {
            tracing::warn!(subject, error = %e, "could not deliver result to the waiting caller");
        }

        if let Err(e) = self
            .js
            .publish(
                result_subject(&self.prefix, &result.function_id, &result.invocation_id),
                body.into(),
            )
            .await
        {
            tracing::warn!(error = %e, "could not record result");
        }
    }

    pub async fn publish_dlq(&self, record: &DlqRecord) -> Result<(), BusError> {
        let body = serde_json::to_vec(record)
            .map_err(|e| BusError::Publish(format!("could not serialize DLQ record: {e}")))?;
        self.js
            .publish(
                dlq_subject(&self.prefix, &record.event.function_id),
                body.into(),
            )
            .await
            .map_err(|e| BusError::Publish(e.to_string()))?
            .await
            .map_err(|e| BusError::Publish(e.to_string()))?;
        Ok(())
    }

    /// Read up to `limit` DLQ records without consuming them.
    ///
    /// An ephemeral, `AckPolicy::None` consumer, so a peek from the dashboard
    /// leaves the DLQ exactly as it found it and several operators can look at
    /// once.
    pub async fn dlq_peek(
        &self,
        function_id: &str,
        limit: usize,
    ) -> Result<Vec<DlqRecord>, BusError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let stream = self
            .js
            .get_stream(dlq_stream(&self.prefix))
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;
        let consumer = stream
            .create_consumer(consumer::pull::Config {
                filter_subject: dlq_subject(&self.prefix, function_id),
                ack_policy: consumer::AckPolicy::None,
                ..Default::default()
            })
            .await
            .map_err(|e| BusError::Consumer(e.to_string()))?;

        let mut batch = consumer
            .batch()
            .max_messages(limit)
            // Bounded so an empty DLQ returns promptly instead of blocking the
            // admin request for the server default.
            .expires(Duration::from_millis(500))
            .messages()
            .await
            .map_err(|e| BusError::Consumer(e.to_string()))?;

        let mut out = Vec::new();
        while let Some(Ok(msg)) = batch.next().await {
            match serde_json::from_slice::<DlqRecord>(&msg.payload) {
                Ok(rec) => out.push(rec),
                // A record we cannot parse is still evidence something failed;
                // skip it rather than failing the whole listing.
                Err(e) => tracing::warn!(error = %e, "skipping unparseable DLQ record"),
            }
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Republish DLQ records onto the invoke subject, tagged `Replay`.
    ///
    /// The invocation id is re-minted so the replay is a genuinely new
    /// invocation: reusing the original would be deduped by `Nats-Msg-Id` and,
    /// worse, would hit the daemon's existing `operationId` record and return
    /// the old failure without running anything.
    pub async fn dlq_replay(
        &self,
        function_id: &str,
        limit: usize,
        max_payload: usize,
    ) -> Result<usize, BusError> {
        let records = self.dlq_peek(function_id, limit).await?;
        let mut replayed = 0;
        for rec in records {
            let mut event = rec.event;
            event.invocation_id = new_invocation_id();
            event.enqueued_at_ms = now_ms();
            event.source = EventSource::Replay;
            // A replay is fire-and-forget: whoever was waiting on the original
            // reply subject is long gone.
            event.reply_subject = None;
            match self.publish(&event, max_payload).await {
                Ok(()) => replayed += 1,
                Err(e) => {
                    tracing::warn!(function = function_id, error = %e, "could not replay DLQ record");
                }
            }
        }
        Ok(replayed)
    }

    /// Drop every DLQ record for a function. Returns how many were removed.
    pub async fn dlq_purge(&self, function_id: &str) -> Result<u64, BusError> {
        let stream = self
            .js
            .get_stream(dlq_stream(&self.prefix))
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;
        let resp = stream
            .purge()
            .filter(dlq_subject(&self.prefix, function_id))
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;
        Ok(resp.purged)
    }

    /// How many DLQ records a function has.
    ///
    /// Uses the per-subject message counts rather than reading the records, so
    /// this stays a single round trip no matter how deep the DLQ is — the
    /// dashboard polls it every 2s.
    pub async fn dlq_count(&self, function_id: &str) -> Result<u64, BusError> {
        let stream = self
            .js
            .get_stream(dlq_stream(&self.prefix))
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;
        let subject = dlq_subject(&self.prefix, function_id);
        let mut info = stream
            .info_with_subjects(&subject)
            .await
            .map_err(|e| BusError::Stream(e.to_string()))?;
        while let Some(entry) = info.next().await {
            let (s, count) = entry.map_err(|e| BusError::Stream(e.to_string()))?;
            if s == subject {
                return Ok(count as u64);
            }
        }
        // No entry for the subject means no messages on it, which is the normal
        // case for a function that has never failed.
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_are_namespaced_by_prefix_and_function() {
        assert_eq!(invoke_subject("qfn", "echo"), "qfn.invoke.echo");
        assert_eq!(
            result_subject("qfn", "echo", "abc-1"),
            "qfn.result.echo.abc-1"
        );
        assert_eq!(dlq_subject("qfn", "echo"), "qfn.dlq.echo");
        assert_eq!(reply_subject("qfn", "abc-1"), "qfn.reply.abc-1");
        assert_eq!(consumer_name("echo"), "qfn-echo");
    }

    /// The events stream's wildcard is a single token (`*`, not `>`), so a
    /// function id containing a dot would land outside its own consumer's
    /// filter and never be delivered. `FunctionSpec::validate` is what keeps
    /// ids to a single token; this records why it has to.
    #[test]
    fn the_invoke_wildcard_is_one_token_deep() {
        let subject = invoke_subject("qfn", "echo");
        assert_eq!(
            subject.split('.').count(),
            3,
            "an id spanning more than one token would escape the stream's `qfn.invoke.*`"
        );
    }

    #[test]
    fn invocation_ids_are_unique_time_ordered_and_daemon_safe() {
        let ids: Vec<_> = (0..1000).map(|_| new_invocation_id()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "ids collided");

        for id in &ids {
            assert!(
                crate::vm::valid_operation_id(id),
                "id {id:?} is not usable as a daemon operationId"
            );
            assert!(
                !id.contains('.') && !id.contains('*') && !id.contains('>'),
                "id {id:?} would corrupt a NATS subject"
            );
        }

        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(sorted, ids, "ids must sort chronologically");
    }

    #[test]
    fn backoff_climbs_and_then_saturates() {
        assert_eq!(backoff_for(1), Duration::from_secs(1));
        assert_eq!(backoff_for(2), Duration::from_secs(5));
        assert_eq!(backoff_for(3), Duration::from_secs(30));
        assert_eq!(
            backoff_for(99),
            Duration::from_secs(30),
            "a high attempt count must saturate, not index out of bounds"
        );
        // Attempt 0 shouldn't happen, but must not panic if it does.
        assert_eq!(backoff_for(0), Duration::from_secs(1));
    }

    /// Regression: `ack_wait` was the function's timeout exactly, so an
    /// invocation that used its full budget was redelivered a moment before it
    /// finished — and ran a second time on another VM.
    #[test]
    fn ack_wait_outlasts_the_functions_own_timeout() {
        let spec = spec_with_timeout(25);
        let cfg = Bus::pull_config(&spec, "qfn");
        assert!(
            cfg.ack_wait > Duration::from_secs(25),
            "ack_wait must exceed the exec budget or JetStream redelivers live work"
        );
        assert_eq!(cfg.ack_wait, Duration::from_secs(25 + ACK_WAIT_SLACK_SECS));
    }

    #[test]
    fn consumer_config_mirrors_the_spec() {
        let mut spec = spec_with_timeout(10);
        spec.retry.max_attempts = 5;
        spec.scaling.max_replicas = 4;
        spec.scaling.target_concurrency = 3;

        let cfg = Bus::pull_config(&spec, "qfn");
        assert_eq!(cfg.durable_name.as_deref(), Some("qfn-demo"));
        assert_eq!(cfg.filter_subject, "qfn.invoke.demo");
        assert_eq!(cfg.max_deliver, 5);
        assert_eq!(
            cfg.max_ack_pending, 12,
            "never hand out more than the fleet could be running at once"
        );
    }

    /// Regression: the backoff ladder was always the full three rungs, but the
    /// server requires `max_deliver > backoff.len()` — so the default
    /// `max_attempts: 3` was rejected outright and *every* registration failed
    /// with a 400. The ladder needs one rung per retry, not per attempt.
    #[test]
    fn the_backoff_ladder_is_shorter_than_the_delivery_limit() {
        for attempts in 1..=6u32 {
            let mut spec = spec_with_timeout(10);
            spec.retry.max_attempts = attempts;
            let cfg = Bus::pull_config(&spec, "qfn");
            assert!(
                (cfg.backoff.len() as i64) < cfg.max_deliver,
                "max_attempts {attempts}: {} backoff rungs against max_deliver {} — \
                 the server rejects this",
                cfg.backoff.len(),
                cfg.max_deliver,
            );
        }
    }

    /// `max_attempts: 1` means no retries, so there is no rung to climb.
    #[test]
    fn a_non_retryable_function_gets_an_empty_backoff_ladder() {
        let mut spec = spec_with_timeout(10);
        spec.retry.max_attempts = 1;
        assert!(Bus::pull_config(&spec, "qfn").backoff.is_empty());
    }

    /// A zeroed replica count or concurrency would make `max_ack_pending` zero,
    /// which JetStream reads as "unlimited" — the exact opposite of intent.
    #[test]
    fn max_ack_pending_is_never_zero() {
        let mut spec = spec_with_timeout(10);
        spec.scaling.max_replicas = 0;
        spec.scaling.target_concurrency = 0;
        assert!(Bus::pull_config(&spec, "qfn").max_ack_pending >= 1);
    }

    #[test]
    fn an_event_round_trips_through_json() {
        let mut ev = InvokeEvent::new(
            "demo",
            Some(serde_json::json!({"hello": "world"})),
            EventSource::Schedule,
        );
        ev.reply_subject = Some("qfn.reply.x".into());
        let json = serde_json::to_vec(&ev).unwrap();
        let back: InvokeEvent = serde_json::from_slice(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn a_result_round_trips_through_json() {
        let ev = InvokeEvent::new("demo", None, EventSource::Invoke);
        let result = InvokeResult::from_exec(
            &ev,
            "sb-1",
            ExecOutput {
                stdout: "hi".into(),
                stderr: String::new(),
                exit_code: 0,
            },
            42,
            1,
        );
        assert_eq!(result.outcome, Outcome::Success);
        let json = serde_json::to_vec(&result).unwrap();
        let back: InvokeResult = serde_json::from_slice(&json).unwrap();
        assert_eq!(result, back);
    }

    #[test]
    fn a_nonzero_exit_is_a_failure_not_an_error() {
        let ev = InvokeEvent::new("demo", None, EventSource::Invoke);
        let result = InvokeResult::from_exec(
            &ev,
            "sb-1",
            ExecOutput {
                stdout: String::new(),
                stderr: "boom".into(),
                exit_code: 3,
            },
            10,
            1,
        );
        assert_eq!(
            result.outcome,
            Outcome::Failed,
            "a command that ran and exited non-zero is a failure; an error is when it never ran"
        );
        assert_eq!(result.exit_code, 3);
        assert!(result.error.is_none());
    }

    #[test]
    fn queue_wait_is_measured_from_enqueue() {
        let mut ev = InvokeEvent::new("demo", None, EventSource::Invoke);
        ev.enqueued_at_ms = now_ms().saturating_sub(1500);
        assert!(ev.queue_wait_ms() >= 1500);
    }

    /// A clock that moved backwards must not produce an enormous wait figure
    /// from an underflow.
    #[test]
    fn queue_wait_saturates_when_the_clock_moves_backwards() {
        let mut ev = InvokeEvent::new("demo", None, EventSource::Invoke);
        ev.enqueued_at_ms = now_ms() + 10_000;
        assert_eq!(ev.queue_wait_ms(), 0);
    }

    fn spec_with_timeout(timeout_secs: u64) -> FunctionSpec {
        use crate::config::{ExecSpec, PayloadMode, RetryPolicy, ScalingPolicy, VmSpec};
        use heyo_sdk::SandboxDriver;
        FunctionSpec {
            id: "demo".into(),
            vm: VmSpec {
                driver: SandboxDriver::Firecracker,
                image: None,
                start_command: None,
                size_class: None,
                disk_size_gb: None,
                working_directory: None,
                env_vars: None,
                ttl_seconds: 3600,
            },
            exec: ExecSpec {
                command: "true".into(),
                working_directory: None,
                env: None,
                timeout_secs,
                max_payload_bytes: 4096,
                payload_mode: PayloadMode::Env,
            },
            scaling: ScalingPolicy::default(),
            triggers: vec![],
            retry: RetryPolicy::default(),
        }
    }
}
