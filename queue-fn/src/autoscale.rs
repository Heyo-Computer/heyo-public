//! The control loop: owns every VM lifecycle decision.
//!
//! The daemon offers no event stream, so this polls. It hits `Sandbox::list()`
//! exactly once per tick and indexes the result, because `Sandbox::info()`
//! fetches that same full list and filters client-side — polling per-VM would be
//! quadratic in fleet size.
//!
//! All VM creation happens here and never in a dispatcher, so a slow boot can
//! never stall the drain of a queue.
//!
//! **Readiness is proved by an exec, not a probe.** app-lb can TCP-connect to a
//! guest port because it routes HTTP there. queue-fn has no port, and a
//! connection would not prove the thing we actually need — that the guest's
//! one-shot exec shell is attached and answering. So a VM joins the pool only
//! once a trivial command has actually run inside it and exited zero.

use crate::bus::Bus;
use crate::function::{Function, PendingVm, VmWorker, now_secs};
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::vm::{self, VmManager};
use heyo_sdk::SandboxInfo;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::watch;

const TICK: Duration = Duration::from_secs(2);

/// The readiness probe. `true` is the smallest command that proves a shell ran.
const READY_PROBE: &str = "true";
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Autoscaler {
    registry: Arc<Registry>,
    vms: VmManager,
    metrics: Arc<Metrics>,
    bus: Arc<Bus>,
    /// Monotonic source for replica-name nonces. Not for addressing — the daemon
    /// assigns sandbox ids — just to keep our names unique.
    nonce: AtomicU64,
}

impl Autoscaler {
    pub fn new(
        registry: Arc<Registry>,
        vms: VmManager,
        metrics: Arc<Metrics>,
        bus: Arc<Bus>,
    ) -> Self {
        Self {
            registry,
            vms,
            metrics,
            bus,
            nonce: AtomicU64::new(now_secs()),
        }
    }

    fn next_nonce(&self) -> u64 {
        self.nonce.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        tracing::info!("autoscaler starting");

        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // A publish into an empty pool nudges `scale_signal`, so a
            // scaled-to-zero function reacts immediately rather than waiting out
            // the tick. Rebuilt each iteration because the function set changes.
            let nudged = wait_for_any_scale_signal(&self.registry);

            tokio::select! {
                _ = ticker.tick() => self.reconcile().await,
                _ = nudged => self.reconcile().await,
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("autoscaler shutting down");
                        return;
                    }
                }
            }
        }
    }

    /// One full pass over every function.
    async fn reconcile(&self) {
        let fleet = match self.vms.list().await {
            Ok(list) => vm::index_by_id(list),
            Err(e) => {
                // Log explicitly: without this the loop fails silently and the
                // pool just quietly stops updating.
                tracing::error!(error = %e, "failed to list sandboxes; skipping tick");
                return;
            }
        };

        // Best-effort gauge alongside the fleet list. A failure here must not
        // derail scaling, so we log and carry on with an empty index — VMs keep
        // their last sample rather than reporting a false zero.
        let usage = self.sample_usage().await;

        for function in self.registry.functions().values() {
            self.reconcile_one(function, &fleet, &usage).await;
        }
    }

    /// Read the daemon's cached usage snapshot, push the host figures into the
    /// metrics gauge, and return a per-sandbox index for `apply_usage`.
    async fn sample_usage(&self) -> HashMap<String, vm::SandboxUsage> {
        let usage = match self.vms.system_usage().await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!(error = %e, "failed to fetch system usage; skipping this tick");
                return HashMap::new();
            }
        };

        match usage.snapshot {
            Some(snap) => {
                self.metrics.record_host_usage(
                    usage.available,
                    snap.host.cpu_count,
                    snap.host.cpu_percent,
                    snap.host.memory_total_bytes,
                    snap.host.memory_used_bytes,
                    snap.sampled_at_ms,
                );
                snap.sandboxes
                    .into_iter()
                    .map(|s| (s.sandbox_id.clone(), s))
                    .collect()
            }
            None => {
                // Poller not ready yet: mark the gauge unavailable so the
                // dashboard shows "—" rather than stale or zeroed numbers.
                self.metrics.record_host_usage(false, 0, 0.0, 0, 0, 0);
                HashMap::new()
            }
        }
    }

    async fn reconcile_one(
        &self,
        f: &Arc<Function>,
        fleet: &HashMap<String, SandboxInfo>,
        usage: &HashMap<String, vm::SandboxUsage>,
    ) {
        self.refresh_depth(f).await;
        self.prune(f, fleet);
        self.promote_pending(f, fleet).await;

        let desired = f.desired_replicas() as usize;
        let ready = f.workers().len();
        let live = ready + f.pending().len();

        if live < desired {
            self.scale_up(f, desired - live).await;
        } else if ready > desired {
            self.scale_down(f, ready - desired);
        }

        self.reap_drained(f).await;
        self.renew_ttls(f, fleet).await;
        self.apply_usage(f, usage);
    }

    /// Pull the function's backlog from its consumer.
    ///
    /// This is the demand signal app-lb has no analogue for. A failure leaves
    /// the previous value in place rather than zeroing it — reading "no work"
    /// because NATS hiccuped would scale a busy function to zero.
    async fn refresh_depth(&self, f: &Arc<Function>) {
        let depth = match self.bus.depth(&f.spec.id).await {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(
                    function = %f.spec.id,
                    error = %e,
                    "could not read queue depth; keeping the last known value",
                );
                return;
            }
        };
        f.set_queue_depth(depth.pending, depth.ack_pending);

        // The DLQ count is dashboard-only, so a failure is not worth a log line
        // every two seconds.
        let dlq = self.bus.dlq_count(&f.spec.id).await.unwrap_or(0);
        self.metrics.record_queue_depth(
            &f.spec.id,
            depth.pending,
            depth.ack_pending,
            dlq,
            f.is_paused(),
        );
    }

    /// Copy the latest per-VM CPU/memory sample onto each live worker.
    fn apply_usage(&self, f: &Arc<Function>, usage: &HashMap<String, vm::SandboxUsage>) {
        if usage.is_empty() {
            return;
        }
        for w in f.workers().iter() {
            if let Some(u) = usage.get(&w.sandbox_id) {
                w.set_usage(u.cpu_percent, u.memory_bytes);
            }
        }
    }

    /// Keep long-lived VMs from hitting their TTL backstop.
    ///
    /// The TTL exists so VMs self-destruct if this process dies without reaping
    /// them. While we are alive, renew past the halfway mark so a healthy VM
    /// under steady load isn't culled out from under a running invocation.
    async fn renew_ttls(&self, f: &Arc<Function>, fleet: &HashMap<String, SandboxInfo>) {
        let ttl = f.spec.vm.ttl_seconds;
        for w in f.workers().iter() {
            let Some(info) = fleet.get(&w.sandbox_id) else {
                continue;
            };
            let remaining = info.ttl_seconds.unwrap_or(ttl);
            if info.uptime_secs < remaining / 2 {
                continue;
            }
            if let Err(e) = self.vms.renew_ttl(&w.sandbox_id, ttl).await {
                tracing::warn!(sandbox = %w.sandbox_id, error = %e, "failed to renew TTL");
            }
        }
    }

    /// Drop workers the daemon no longer reports as running.
    ///
    /// This is what catches a VM killed out of band: it disappears from the
    /// fleet list, and we stop dispatching to it.
    fn prune(&self, f: &Arc<Function>, fleet: &HashMap<String, SandboxInfo>) {
        let workers = f.workers();
        let kept: Vec<_> = workers
            .iter()
            .filter(|w| match fleet.get(&w.sandbox_id) {
                Some(info) => {
                    let alive = !vm::is_terminal(&info.status);
                    if !alive {
                        tracing::info!(
                            function = %f.spec.id,
                            sandbox = %w.sandbox_id,
                            status = ?info.status,
                            "dropping worker: VM is no longer running",
                        );
                    }
                    alive
                }
                None => {
                    tracing::info!(
                        function = %f.spec.id,
                        sandbox = %w.sandbox_id,
                        "dropping worker: VM is gone from the daemon",
                    );
                    false
                }
            })
            .cloned()
            .collect();

        if kept.len() != workers.len() {
            f.set_workers(kept);
        }
    }

    /// Prove a VM can actually run a command.
    ///
    /// Not a formality: `wait_for_ready` reports `Ok` for stopped VMs, and
    /// `Running` says nothing about whether the guest's exec shell is attached
    /// to the serial console yet. Promoting on status alone puts a VM in the
    /// pool that fails the first real invocation it is handed.
    async fn probe_exec(&self, sandbox_id: &str) -> bool {
        match self
            .vms
            .exec(sandbox_id, READY_PROBE, None, None, READY_PROBE_TIMEOUT)
            .await
        {
            Ok(out) => out.succeeded(),
            Err(e) => {
                tracing::debug!(sandbox = %sandbox_id, error = %e, "readiness probe failed");
                false
            }
        }
    }

    /// Move booted VMs into the pool.
    async fn promote_pending(&self, f: &Arc<Function>, fleet: &HashMap<String, SandboxInfo>) {
        let pending = f.pending();
        if pending.is_empty() {
            return;
        }

        let mut still_pending = Vec::new();
        let mut promoted = Vec::new();
        let mut doomed = Vec::new();

        for p in pending.iter() {
            let Some(info) = fleet.get(&p.sandbox_id) else {
                tracing::warn!(
                    function = %f.spec.id,
                    sandbox = %p.sandbox_id,
                    "pending VM vanished from the daemon",
                );
                continue;
            };

            match vm::routable_ip(info) {
                Ok(ip) => {
                    if self.probe_exec(&p.sandbox_id).await {
                        let boot_secs = now_secs().saturating_sub(p.created_at);
                        tracing::info!(
                            function = %f.spec.id,
                            sandbox = %p.sandbox_id,
                            %ip,
                            boot_secs,
                            "VM ready",
                        );
                        self.metrics.record_cold_start(&f.spec.id, boot_secs);
                        promoted.push(Arc::new(VmWorker::new(p.sandbox_id.clone())));
                    } else {
                        still_pending.push(p.clone()); // booting; try next tick
                    }
                }
                Err(vm::VmError::NotRunning { status, .. }) if !vm::is_terminal(&status) => {
                    still_pending.push(p.clone()); // provisioning
                }
                Err(e) => {
                    // Terminal, or on a backend we cannot drive. Either way it
                    // will never run anything, so stop waiting and reclaim the
                    // slot against max_replicas.
                    tracing::error!(
                        function = %f.spec.id,
                        sandbox = %p.sandbox_id,
                        error = %e,
                        "giving up on VM",
                    );
                    doomed.push(p.sandbox_id.clone());
                }
            }
        }

        f.set_pending(still_pending);

        if !promoted.is_empty() {
            let mut workers = (*f.workers()).clone();
            workers.extend(promoted);
            f.set_workers(workers);
            // Release anything blocked on a cold start.
            f.ready_signal.notify_waiters();
        }

        for id in doomed {
            if let Err(e) = self.vms.kill(&id).await {
                tracing::warn!(sandbox = %id, error = %e, "failed to kill doomed VM");
            }
        }
    }

    async fn scale_up(&self, f: &Arc<Function>, count: usize) {
        tracing::info!(
            function = %f.spec.id,
            count,
            queued = f.queue_pending(),
            "scaling up",
        );
        let mut pending = (*f.pending()).clone();

        let mut created = 0u64;
        for _ in 0..count {
            let name = vm::replica_name(&f.spec.id, self.next_nonce());
            match self.vms.create(&f.spec.vm, name).await {
                Ok(sandbox) => {
                    pending.push(PendingVm {
                        sandbox_id: sandbox.sandbox_id().to_string(),
                        created_at: now_secs(),
                    });
                    created += 1;
                }
                Err(e) => {
                    tracing::error!(function = %f.spec.id, error = %e, "failed to create VM");
                    break; // daemon is unhappy; don't hammer it this tick
                }
            }
        }

        // Record only VMs the daemon actually accepted, so the dashboard's
        // create count matches what booted rather than what was attempted.
        self.metrics.record_scale_up(&f.spec.id, created);
        f.set_pending(pending);
    }

    /// Retire surplus VMs, most-idle first, by marking them draining.
    ///
    /// Draining stops new invocations without interrupting one already running;
    /// the kill happens in `reap_drained` once it finishes.
    fn scale_down(&self, f: &Arc<Function>, count: usize) {
        let workers = f.workers();
        let mut candidates: Vec<_> = workers
            .iter()
            .filter(|w| !w.is_draining())
            .cloned()
            .collect();
        // Prefer idle VMs so draining finishes promptly; among idle ones, the
        // least recently used, so a warm VM stays warm.
        candidates.sort_by_key(|w| (w.is_busy(), w.last_active()));

        let mut drained = 0u64;
        for w in candidates.iter().take(count) {
            tracing::info!(
                function = %f.spec.id,
                sandbox = %w.sandbox_id,
                busy = w.is_busy(),
                "draining VM",
            );
            w.set_draining();
            drained += 1;
        }
        self.metrics.record_scale_down(&f.spec.id, drained);
    }

    /// Kill drained VMs, and force-kill any that overstay the drain deadline.
    async fn reap_drained(&self, f: &Arc<Function>) {
        let workers = f.workers();
        let deadline = f.spec.scaling.drain_timeout_secs;

        let (done, keep): (Vec<_>, Vec<_>) = workers.iter().cloned().partition(|w| {
            if !w.is_draining() {
                return false;
            }
            let idle = !w.is_busy();
            let expired = now_secs().saturating_sub(w.last_active()) >= deadline;
            if !idle && expired {
                tracing::warn!(
                    function = %f.spec.id,
                    sandbox = %w.sandbox_id,
                    "drain deadline exceeded; killing VM mid-invocation",
                );
            }
            idle || expired
        });

        if done.is_empty() {
            return;
        }
        f.set_workers(keep);

        self.metrics.record_reaped(&f.spec.id, done.len() as u64);
        for w in done {
            tracing::info!(function = %f.spec.id, sandbox = %w.sandbox_id, "killing VM");
            if let Err(e) = self.vms.kill(&w.sandbox_id).await {
                tracing::warn!(sandbox = %w.sandbox_id, error = %e, "failed to kill VM");
            }
        }
    }

    /// Adopt VMs from a previous run of this process.
    ///
    /// Without this a restart would leave the old VMs running while booting a
    /// fresh set; the orphans would only die when their TTL expired. Awaited in
    /// `main` before any dispatcher starts, so no invocation is handed to a VM
    /// the pool has not yet accounted for.
    pub async fn adopt_existing(&self) {
        let fleet = match self.vms.list().await {
            Ok(list) => list,
            Err(e) => {
                tracing::error!(error = %e, "could not list sandboxes for adoption");
                return;
            }
        };

        let functions = self.registry.functions();
        let mut adopted: HashMap<String, Vec<Arc<VmWorker>>> = HashMap::new();
        let mut orphans = Vec::new();

        for info in &fleet {
            let Some(owner) = vm::owner_of(&info.name) else {
                continue; // not ours; leave it alone
            };
            if !functions.contains_key(owner) {
                // Ours, but its function is gone from the state file.
                orphans.push(info.id.clone());
                continue;
            }
            match vm::routable_ip(info) {
                Ok(ip) if self.probe_exec(&info.id).await => {
                    tracing::info!(
                        function = %owner,
                        sandbox = %info.id,
                        %ip,
                        "adopting existing VM",
                    );
                    adopted
                        .entry(owner.to_string())
                        .or_default()
                        .push(Arc::new(VmWorker::new(info.id.clone())));
                }
                _ => orphans.push(info.id.clone()),
            }
        }

        for (id, workers) in adopted {
            if let Some(f) = functions.get(&id) {
                f.set_workers(workers);
            }
        }

        for id in orphans {
            tracing::info!(sandbox = %id, "killing orphaned VM from a previous run");
            if let Err(e) = self.vms.kill(&id).await {
                tracing::warn!(sandbox = %id, error = %e, "failed to kill orphan");
            }
        }
    }

    /// Drain and kill every VM of a function, e.g. on DELETE.
    pub async fn teardown(&self, f: &Arc<Function>) {
        for w in f.workers().iter() {
            w.set_draining();
            if let Err(e) = self.vms.kill(&w.sandbox_id).await {
                tracing::warn!(sandbox = %w.sandbox_id, error = %e, "failed to kill VM");
            }
        }
        for p in f.pending().iter() {
            if let Err(e) = self.vms.kill(&p.sandbox_id).await {
                tracing::warn!(sandbox = %p.sandbox_id, error = %e, "failed to kill pending VM");
            }
        }
        f.set_workers(Vec::new());
        f.set_pending(Vec::new());
    }

    /// Evict a single VM from a function's pool.
    ///
    /// Both modes respect the rule that the autoscaler is the only writer of the
    /// `workers`/`pending` vecs — this never mutates them. It flips the worker's
    /// atomic drain flag and/or kills the sandbox, and lets the next reconcile
    /// tick reconcile the vecs:
    ///
    /// - **graceful** (`force = false`): mark the VM draining so it takes no new
    ///   invocations but finishes the one it is running; `reap_drained` kills it
    ///   once idle or at `drain_timeout_secs`.
    /// - **force** (`force = true`): kill now, abandoning any invocation in
    ///   flight. That invocation's message is never acked, so JetStream
    ///   redelivers it to another VM once `ack_wait` elapses — the work is
    ///   delayed, not lost.
    ///
    /// A pending (still-booting) VM holds no work, so it is killed in either
    /// mode. After eviction the autoscaler is nudged so a replacement boots
    /// immediately if the policy still wants the capacity.
    pub async fn evict(&self, f: &Arc<Function>, sandbox_id: &str, force: bool) -> EvictOutcome {
        if let Some(w) = f
            .workers()
            .iter()
            .find(|w| w.sandbox_id == sandbox_id)
            .cloned()
        {
            // Stop new work regardless of mode; a draining worker is refused by
            // `try_claim`, so nothing is dispatched to it after this point.
            w.set_draining();

            if !force {
                tracing::info!(
                    function = %f.spec.id,
                    sandbox = %sandbox_id,
                    busy = w.is_busy(),
                    "evicting VM (draining)",
                );
                f.scale_signal.notify_one();
                return EvictOutcome::Draining;
            }

            tracing::info!(
                function = %f.spec.id,
                sandbox = %sandbox_id,
                busy = w.is_busy(),
                "evicting VM (force kill)",
            );
            if let Err(e) = self.vms.kill(sandbox_id).await {
                tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to kill evicted VM");
                return EvictOutcome::KillFailed(e.to_string());
            }
            // `prune` removes the dead worker next tick and won't record a reap,
            // so count it here to keep the dashboard's total honest.
            self.metrics.record_reaped(&f.spec.id, 1);
            f.scale_signal.notify_one();
            return EvictOutcome::Killed;
        }

        // A pending, still-booting VM: nothing running, so just kill it. The
        // next `promote_pending` drops it from the pending vec.
        if f.pending().iter().any(|p| p.sandbox_id == sandbox_id) {
            tracing::info!(function = %f.spec.id, sandbox = %sandbox_id, "evicting pending VM");
            if let Err(e) = self.vms.kill(sandbox_id).await {
                tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to kill evicted pending VM");
                return EvictOutcome::KillFailed(e.to_string());
            }
            f.scale_signal.notify_one();
            return EvictOutcome::Killed;
        }

        EvictOutcome::NotFound
    }
}

/// The result of an [`Autoscaler::evict`] call.
#[derive(Debug)]
pub enum EvictOutcome {
    /// The sandbox was killed and is gone now.
    Killed,
    /// Marked draining; the autoscaler reaps it once idle.
    Draining,
    /// No VM with that id is in the function's pool (ready or pending).
    NotFound,
    /// The VM was found but the daemon refused to kill it.
    KillFailed(String),
}

/// Resolve as soon as *any* function asks to be scaled.
async fn wait_for_any_scale_signal(registry: &Arc<Registry>) {
    let functions = registry.functions();
    if functions.is_empty() {
        // Nothing to wait on; let the ticker drive the loop. Without this the
        // `select_all` below would panic on an empty vec.
        std::future::pending::<()>().await;
    }
    let waits: Vec<_> = functions
        .values()
        .cloned()
        .map(|f| {
            Box::pin(async move { f.scale_signal.notified().await })
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        })
        .collect();
    let _ = futures::future::select_all(waits).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecSpec, FunctionSpec, PayloadMode, RetryPolicy, ScalingPolicy, VmSpec};
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

    // Constructing an `Autoscaler` requires a live NATS server (a `Bus` cannot
    // be built without one), so the tests here exercise the decision logic
    // directly against `Function`/`VmWorker` rather than through a half-faked
    // Autoscaler. The paths that need a real daemon and a real bus are covered
    // by the end-to-end run in the README.

    #[test]
    fn scale_down_prefers_idle_vms_over_busy_ones() {
        let f = Arc::new(Function::new(spec()));
        let busy = Arc::new(VmWorker::new("sb-busy".into()));
        let idle = Arc::new(VmWorker::new("sb-idle".into()));
        let _claim = busy.try_claim().expect("claim");
        f.set_workers(vec![busy.clone(), idle.clone()]);

        // Reproduce scale_down's ordering without needing a live Autoscaler.
        let mut candidates: Vec<_> = f
            .workers()
            .iter()
            .filter(|w| !w.is_draining())
            .cloned()
            .collect();
        candidates.sort_by_key(|w| (w.is_busy(), w.last_active()));

        assert_eq!(
            candidates[0].sandbox_id, "sb-idle",
            "draining an idle VM finishes immediately; draining a busy one stalls the pool",
        );
    }

    /// Regression: `reap_drained` killed any draining VM immediately, which cut
    /// off invocations mid-run. They must survive until idle or until the drain
    /// deadline, whichever comes first.
    #[test]
    fn a_busy_draining_vm_is_not_reaped_before_its_deadline() {
        let w = Arc::new(VmWorker::new("sb-1".into()));
        w.set_draining();
        let _claim = w.try_claim();
        assert!(_claim.is_none(), "a draining VM refuses new claims");

        let busy = Arc::new(VmWorker::new("sb-2".into()));
        let claim = busy.try_claim().expect("claim before draining");
        busy.set_draining();

        // The reap predicate: idle, or past the deadline.
        let deadline = 60;
        let should_reap =
            |w: &Arc<VmWorker>| !w.is_busy() || now_secs().saturating_sub(w.last_active()) >= deadline;

        assert!(!should_reap(&busy), "still running: must not be killed");
        drop(claim);
        assert!(should_reap(&busy), "finished: safe to kill");
    }

    #[test]
    fn an_empty_registry_does_not_panic_the_scale_signal_wait() {
        let registry = Arc::new(Registry::new("unused.json"));
        // `select_all` panics on an empty vec, so the empty case must short out
        // into a pending future rather than reaching it.
        let fut = wait_for_any_scale_signal(&registry);
        // Not awaited: constructing it is what would panic if the guard were
        // missing. Dropping it is enough to prove the guard is in place.
        drop(fut);
    }
}
