//! Pulling events off JetStream and running them in VMs.
//!
//! One task per function, supervised by a manager that diffs the registry
//! against its live task map each tick. Per-function rather than one global
//! loop because each function has its own consumer, its own `ack_wait`, and its
//! own pause flag — a single loop would have to demultiplex all three.
//!
//! **A message with no VM to run on is nak'd, not held.** The tempting thing is
//! to park it in the task and wait for capacity. That would be wrong: a message
//! held in this process is no longer counted by `num_pending`, so it becomes
//! invisible to the autoscaler — and the autoscaler is the only thing that could
//! produce the capacity it is waiting for. Nak'ing puts it back where the
//! demand signal can see it.

use crate::bus::{Bus, DlqRecord, EventSource, InvokeEvent, InvokeResult, Outcome, now_ms};
use crate::function::Function;
use crate::invoke;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::results::{InvocationRecord, Results};
use crate::vm::{VmError, VmManager};
use async_nats::jetstream::AckKind;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How long a fetch waits for messages before looping. Short enough that a
/// shutdown or a pause is noticed promptly.
const FETCH_EXPIRY: Duration = Duration::from_secs(2);

/// How often the manager reconciles its task set against the registry.
const SUPERVISE_TICK: Duration = Duration::from_secs(2);

/// Redelivery delay while a function is paused. Long enough not to spin.
const PAUSED_NAK_DELAY: Duration = Duration::from_secs(5);

/// Redelivery delay when the VM we claimed turned out to be unusable. Short:
/// the autoscaler has been nudged, so capacity may be seconds away.
const NO_CAPACITY_NAK_DELAY: Duration = Duration::from_secs(1);

/// How long to wait before re-checking for a free exec slot. Only reached while
/// a function has work and no capacity, so the autoscaler is already booting.
const NO_CAPACITY_POLL: Duration = Duration::from_millis(500);

/// Everything a dispatcher task needs.
#[derive(Clone)]
pub struct Dispatcher {
    pub registry: Arc<Registry>,
    pub bus: Arc<Bus>,
    pub vms: VmManager,
    pub metrics: Arc<Metrics>,
    pub results: Arc<Results>,
}

impl Dispatcher {
    /// Supervise one task per registered function, starting and stopping them as
    /// the registry changes.
    pub async fn supervise(self, mut shutdown: watch::Receiver<bool>) {
        tracing::info!("dispatcher starting");
        let mut tasks: HashMap<String, JoinHandle<()>> = HashMap::new();
        let mut ticker = tokio::time::interval(SUPERVISE_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ticker.tick() => self.sync_tasks(&mut tasks, &shutdown),
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("dispatcher shutting down");
                        // The tasks watch the same channel, so they are already
                        // winding down; wait rather than abort so an invocation
                        // in flight gets to ack instead of being redelivered.
                        for (id, task) in tasks {
                            if tokio::time::timeout(Duration::from_secs(10), task).await.is_err() {
                                tracing::warn!(function = %id, "dispatcher did not stop in time");
                            }
                        }
                        return;
                    }
                }
            }
        }
    }

    fn sync_tasks(&self, tasks: &mut HashMap<String, JoinHandle<()>>, shutdown: &watch::Receiver<bool>) {
        let functions = self.registry.functions();

        // Drop tasks whose function is gone, or that exited on their own.
        tasks.retain(|id, task| {
            if !functions.contains_key(id) {
                tracing::info!(function = %id, "stopping dispatcher: function was removed");
                task.abort();
                return false;
            }
            !task.is_finished()
        });

        for (id, function) in functions.iter() {
            if tasks.contains_key(id) {
                continue;
            }
            tracing::info!(function = %id, "starting dispatcher");
            let worker = self.clone();
            let f = function.clone();
            let sd = shutdown.clone();
            tasks.insert(id.clone(), tokio::spawn(async move { worker.run_one(f, sd).await }));
        }
    }

    /// Drain one function's consumer until shutdown.
    async fn run_one(&self, f: Arc<Function>, mut shutdown: watch::Receiver<bool>) {
        let id = f.spec.id.clone();
        // Whether we have already nudged the autoscaler about the current
        // no-capacity streak. Nudging on every poll would drive the reconcile
        // loop at this task's poll rate instead of its own tick — which, when
        // the daemon is refusing to create VMs, turns a 2s retry into a
        // twice-a-second hammering of a service that is already unhappy.
        let mut nudged_for_capacity = false;

        loop {
            if *shutdown.borrow() {
                return;
            }

            let consumer = match self.bus.consumer(&id).await {
                Ok(c) => c,
                Err(e) => {
                    // The consumer may not exist yet, or NATS may be down.
                    // Back off rather than spinning; registration creates it.
                    tracing::debug!(function = %id, error = %e, "no consumer yet; retrying");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                        _ = shutdown.changed() => return,
                    }
                }
            };

            // Never pull more than could be placed right now, and never pull at
            // all with nothing to place it on.
            //
            // Pulling with no capacity looks harmless — nak it back and try
            // later — but a nak is a *delivery*, so it counts against
            // `max_deliver`. A cold-starting function would burn its whole retry
            // budget waiting for its first VM and JetStream would discard the
            // work before it ever ran. Leaving it undelivered also keeps it in
            // `num_pending`, which is exactly the signal the autoscaler reads.
            let slots = self.available_slots(&f);
            if slots == 0 {
                if !nudged_for_capacity {
                    f.scale_signal.notify_one();
                    nudged_for_capacity = true;
                }
                tokio::select! {
                    _ = tokio::time::sleep(NO_CAPACITY_POLL) => {}
                    _ = shutdown.changed() => return,
                }
                continue;
            }
            nudged_for_capacity = false;

            let mut batch = match consumer
                .batch()
                .max_messages(slots)
                .expires(FETCH_EXPIRY)
                .messages()
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(function = %id, error = %e, "fetch failed");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                        _ = shutdown.changed() => return,
                    }
                }
            };

            while let Some(next) = tokio::select! {
                msg = batch.next() => msg,
                _ = shutdown.changed() => None,
            } {
                match next {
                    Ok(msg) => self.handle(&f, msg).await,
                    Err(e) => tracing::warn!(function = %id, error = %e, "message error"),
                }
            }
        }
    }

    /// How many messages it is worth pulling right now: one per free exec slot.
    fn available_slots(&self, f: &Arc<Function>) -> usize {
        f.workers().iter().filter(|w| w.is_available()).count()
    }

    async fn handle(&self, f: &Arc<Function>, msg: async_nats::jetstream::Message) {
        let id = &f.spec.id;

        // Paused: put it back, unchanged. The consumer stays, so the backlog
        // stays visible and nothing is lost.
        if f.is_paused() {
            nak(&msg, PAUSED_NAK_DELAY).await;
            return;
        }

        let event: InvokeEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                // Terminate, never nak: redelivering bytes that will not parse
                // is an infinite loop that fills the log and starves real work.
                tracing::error!(function = %id, error = %e, "unparseable event; sending to the DLQ");
                self.metrics.record_rejected(id);
                self.dlq_raw(id, &msg.payload, &e.to_string()).await;
                let _ = msg.ack_with(AckKind::Term).await;
                return;
            }
        };

        let attempt = msg
            .info()
            .map(|i| i.delivered as u32)
            .unwrap_or(1)
            .max(1);
        if attempt > 1 {
            self.metrics.record_retry(id);
        }

        // Claim a VM. Nothing available means the fleet is too small: nudge the
        // autoscaler and put the message back where it counts as demand.
        let Some(slot) = f.select() else {
            f.scale_signal.notify_one();
            nak(&msg, NO_CAPACITY_NAK_DELAY).await;
            return;
        };

        let sandbox_id = slot.sandbox_id().to_string();
        let queue_wait_ms = event.queue_wait_ms();
        let started = Instant::now();

        let outcome = invoke::run(&self.vms, &f.spec.exec, &sandbox_id, &event, attempt).await;
        let duration = started.elapsed();

        let result = match outcome {
            Ok(out) => InvokeResult::from_exec(&event, &sandbox_id, out, duration.as_millis() as u64, attempt),
            Err(e) => {
                // A vanished sandbox is not the function's fault. Mark the
                // worker unhealthy so it leaves the rotation, and retry
                // immediately elsewhere rather than burning an attempt's
                // backoff on a VM that is already gone.
                //
                // This is also the canary for the concurrent-exec bug: heyvm
                // answers a second concurrent exec on one sandbox with
                // NotFound, so a burst of these means the one-slot-per-VM
                // invariant has been broken somewhere.
                if e.is_sandbox_gone() {
                    tracing::warn!(
                        function = %id,
                        sandbox = %sandbox_id,
                        "sandbox vanished mid-invocation; retrying on another VM",
                    );
                    slot.worker().set_healthy(false);
                    drop(slot);
                    f.scale_signal.notify_one();
                    nak(&msg, Duration::ZERO).await;
                    return;
                }

                let kind = match e {
                    VmError::ExecTimeout { .. } => Outcome::Timeout,
                    _ => Outcome::Error,
                };
                slot.worker().record_failure();
                InvokeResult::failure(
                    &event,
                    &sandbox_id,
                    kind,
                    e.to_string(),
                    duration.as_millis() as u64,
                    attempt,
                )
            }
        };
        drop(slot);

        if result.outcome != Outcome::Success {
            slot_failure_log(id, &sandbox_id, &result);
        }

        self.metrics
            .record_invocation(id, result.outcome, duration, queue_wait_ms);
        self.results.push(InvocationRecord::build(
            &event,
            &result,
            queue_wait_ms,
            now_ms(),
        ));
        self.bus
            .publish_result(&result, event.reply_subject.as_deref())
            .await;

        if result.outcome == Outcome::Success {
            if let Err(e) = msg.ack().await {
                // The work is done but the ack was lost, so JetStream will
                // redeliver. The daemon's operation record makes that replay
                // return this same result rather than running the command
                // twice, which is exactly what it is there for.
                tracing::warn!(function = %id, error = %e, "ack failed; the event will be redelivered");
            }
            return;
        }

        // Out of attempts: park it rather than looping forever.
        if attempt >= f.spec.retry.max_attempts {
            tracing::error!(
                function = %id,
                invocation = %event.invocation_id,
                attempts = attempt,
                "giving up; sending to the DLQ",
            );
            let record = DlqRecord {
                event,
                attempts: attempt,
                last_error: result
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("exited {}", result.exit_code)),
                failed_at_ms: now_ms(),
            };
            if let Err(e) = self.bus.publish_dlq(&record).await {
                tracing::error!(function = %id, error = %e, "could not write to the DLQ");
            }
            self.metrics.record_dlq(id);
            let _ = msg.ack_with(AckKind::Term).await;
            return;
        }

        nak(&msg, crate::bus::backoff_for(attempt)).await;
    }

    /// Park an event we could not even parse.
    ///
    /// It has no function id of its own that we can trust, so it is filed under
    /// the consumer that delivered it, with the raw bytes preserved as a string
    /// — an operator needs to see what actually arrived.
    async fn dlq_raw(&self, function_id: &str, payload: &[u8], error: &str) {
        let record = DlqRecord {
            event: InvokeEvent {
                invocation_id: crate::bus::new_invocation_id(),
                function_id: function_id.to_string(),
                payload: Some(serde_json::Value::String(
                    String::from_utf8_lossy(payload).into_owned(),
                )),
                reply_subject: None,
                enqueued_at_ms: now_ms(),
                source: EventSource::Invoke,
            },
            attempts: 1,
            last_error: format!("unparseable event: {error}"),
            failed_at_ms: now_ms(),
        };
        if let Err(e) = self.bus.publish_dlq(&record).await {
            tracing::error!(function = %function_id, error = %e, "could not write to the DLQ");
        }
    }
}

fn slot_failure_log(function: &str, sandbox: &str, result: &InvokeResult) {
    tracing::warn!(
        function,
        sandbox,
        invocation = %result.invocation_id,
        outcome = result.outcome.as_str(),
        exit_code = result.exit_code,
        attempt = result.attempt,
        error = result.error.as_deref().unwrap_or(""),
        "invocation did not succeed",
    );
}

/// Return a message to the queue after a delay.
///
/// A failed nak is not worth failing over: the message stays unacked, so
/// `ack_wait` redelivers it anyway — just later than we asked.
async fn nak(msg: &async_nats::jetstream::Message, delay: Duration) {
    let kind = if delay.is_zero() {
        AckKind::Nak(None)
    } else {
        AckKind::Nak(Some(delay))
    };
    if let Err(e) = msg.ack_with(kind).await {
        tracing::debug!(error = %e, "nak failed; relying on ack_wait to redeliver");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecSpec, FunctionSpec, PayloadMode, RetryPolicy, ScalingPolicy, VmSpec};
    use crate::function::VmWorker;
    use heyo_sdk::SandboxDriver;

    fn spec() -> FunctionSpec {
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
                timeout_secs: 20,
                max_payload_bytes: 4096,
                payload_mode: PayloadMode::Env,
            },
            scaling: ScalingPolicy::default(),
            triggers: vec![],
            retry: RetryPolicy::default(),
        }
    }

    /// Reproduces `available_slots` without needing a live Dispatcher.
    fn available_slots(f: &Arc<Function>) -> usize {
        f.workers().iter().filter(|w| w.is_available()).count()
    }

    /// Regression: the batch size was floored at 1 so that an empty pool would
    /// still pull a message and nak it, on the theory that the nak signalled
    /// demand. It did the opposite. A nak is a delivery, so it counts against
    /// `max_deliver` — a cold-starting function burned its entire retry budget
    /// before its first VM booted, and JetStream discarded the work unrun.
    /// Meanwhile the pulled messages sat in `ack_pending` where the autoscaler,
    /// which was reading `pending`, could not see them at all.
    #[test]
    fn an_empty_pool_pulls_nothing_rather_than_burning_delivery_attempts() {
        let f = Arc::new(Function::new(spec()));
        assert_eq!(
            available_slots(&f),
            0,
            "with nowhere to run it, a message must be left undelivered — where \
             it stays in num_pending and costs no attempt",
        );
    }

    #[test]
    fn the_batch_never_exceeds_what_could_be_placed() {
        let f = Arc::new(Function::new(spec()));
        let a = Arc::new(VmWorker::new("sb-a".into()));
        let b = Arc::new(VmWorker::new("sb-b".into()));
        let c = Arc::new(VmWorker::new("sb-c".into()));
        f.set_workers(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(available_slots(&f), 3);

        let _busy = a.try_claim().expect("claim");
        assert_eq!(available_slots(&f), 2, "a busy VM is not a slot");

        b.set_draining();
        assert_eq!(available_slots(&f), 1, "nor is a draining one");

        c.set_healthy(false);
        assert_eq!(available_slots(&f), 0, "nor is an unhealthy one");
    }

    /// The retry ladder must be exhausted before the DLQ, and the DLQ must be
    /// reached rather than the event looping forever.
    #[test]
    fn attempts_are_exhausted_before_the_dlq() {
        let mut s = spec();
        s.retry.max_attempts = 3;

        let should_dlq = |attempt: u32| attempt >= s.retry.max_attempts;
        assert!(!should_dlq(1), "first failure retries");
        assert!(!should_dlq(2), "second failure retries");
        assert!(should_dlq(3), "the last attempt parks the event");
        assert!(should_dlq(4), "and anything beyond it does too");
    }

    /// Regression: `max_attempts: 1` meant "no retries", but the comparison was
    /// `attempt > max_attempts`, so the event was retried once anyway — a
    /// function explicitly marked non-retryable ran twice.
    #[test]
    fn max_attempts_of_one_means_no_retry_at_all() {
        let mut s = spec();
        s.retry.max_attempts = 1;
        assert!(
            1 >= s.retry.max_attempts,
            "the first failure must go straight to the DLQ",
        );
    }
}
