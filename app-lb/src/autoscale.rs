//! The control loop: owns every VM lifecycle decision.
//!
//! The daemon offers no event stream, so this polls. It hits `Sandbox::list()`
//! exactly once per tick and indexes the result, because `Sandbox::info()`
//! fetches that same full list and filters client-side — polling per-VM would be
//! quadratic in fleet size.
//!
//! All VM creation happens here and never in a proxy filter, so a slow boot can
//! never stall request handling.

use crate::deployment::{Deployment, PendingVm, VmBackend, now_secs};
use crate::health;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::vm::{self, VmManager};
use async_trait::async_trait;
use heyo_sdk::SandboxInfo;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const TICK: Duration = Duration::from_secs(2);

pub struct Autoscaler {
    registry: Arc<Registry>,
    vms: VmManager,
    metrics: Arc<Metrics>,
    /// Monotonic source for replica-name nonces. Not for addressing — the
    /// daemon assigns sandbox ids — just to keep our names unique.
    nonce: AtomicU64,
}

impl Autoscaler {
    pub fn new(registry: Arc<Registry>, vms: VmManager, metrics: Arc<Metrics>) -> Self {
        Self {
            registry,
            vms,
            metrics,
            nonce: AtomicU64::new(now_secs()),
        }
    }

    fn next_nonce(&self) -> u64 {
        self.nonce.fetch_add(1, Ordering::Relaxed)
    }

    /// Whether `d` is still the registry's object for its id.
    ///
    /// It may not be. `reconcile` works from a snapshot taken at the top of the
    /// tick, and then awaits — on a VM create, on a health probe — for long
    /// enough that an admin request can deregister the deployment or rebuild it
    /// underneath us. `Registry::remove`, `upsert` and `update` all install a
    /// *different* `Arc<Deployment>` (or none), so pointer identity is the test.
    fn is_live(&self, d: &Arc<Deployment>) -> bool {
        self.registry
            .get(&d.spec.id)
            .is_some_and(|live| Arc::ptr_eq(&live, d))
    }

    /// Which of `ids` no live deployment is tracking any more.
    ///
    /// This is the fix for a leak with no other backstop than the VM's TTL: a
    /// sandbox created (or promoted) against a `Deployment` the registry has
    /// since dropped is published to an object nothing reads, so no `prune`,
    /// `reap_drained` or `teardown` will ever see it. It keeps running, unrouted,
    /// until `ttl_seconds` expires — potentially an hour later.
    ///
    /// Whether that has happened cannot be decided from the stale object alone,
    /// because one of the replacement paths is benign: a pool-preserving edit
    /// (`Registry::update`) copies the pending and backend lists onto the new
    /// object, which then owns those VMs. So ask the registry what the live
    /// deployment claims; anything left over is unreachable and must be killed.
    ///
    /// Pure, so the decision is testable without a daemon; [`kill_unclaimed`]
    /// acts on it.
    ///
    /// [`kill_unclaimed`]: Self::kill_unclaimed
    fn unclaimed(&self, d: &Arc<Deployment>, ids: &[String]) -> Vec<String> {
        if ids.is_empty() {
            return Vec::new();
        }
        let live = self.registry.get(&d.spec.id);
        if live.as_ref().is_some_and(|l| Arc::ptr_eq(l, d)) {
            return Vec::new(); // still ours: the ids are tracked where we put them
        }

        let claimed: HashSet<String> = live
            .iter()
            .flat_map(|l| {
                let (pending, backends) = (l.pending(), l.backends());
                pending
                    .iter()
                    .map(|p| p.sandbox_id.clone())
                    .chain(backends.iter().map(|b| b.sandbox_id.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();

        ids.iter()
            .filter(|id| !claimed.contains(*id))
            .cloned()
            .collect()
    }

    /// Kill the VMs [`unclaimed`](Self::unclaimed) identifies as abandoned.
    async fn kill_unclaimed(&self, d: &Arc<Deployment>, ids: &[String]) {
        for id in self.unclaimed(d, ids) {
            tracing::info!(
                deployment = %d.spec.id,
                sandbox = %id,
                "deployment was deregistered or rebuilt while this VM was being created; killing it",
            );
            if let Err(e) = self.vms.kill(&id).await {
                tracing::warn!(sandbox = %id, error = %e, "failed to kill abandoned VM");
            }
        }
    }

    /// One full pass over every deployment.
    async fn reconcile(&self) {
        let deployments = self.registry.deployments();

        // Static (proxy_pass) deployments need no daemon interaction — health-
        // re-probe them first, and unconditionally, so they keep working even
        // when the daemon is unreachable (or app-lb runs with no VM deployments
        // at all and heyvmd isn't running).
        for deployment in deployments.values() {
            if deployment.spec.is_static() {
                self.reconcile_static(deployment).await;
            }
        }

        // Managed deployments need the fleet list; if it fails, skip the VM work
        // this tick (the static probes above have already run).
        let fleet = match self.vms.list().await {
            Ok(list) => vm::index_by_id(list),
            Err(e) => {
                // Log explicitly: without this the loop fails silently and the
                // pool just quietly stops updating.
                tracing::error!(error = %e, "failed to list sandboxes; skipping VM reconcile this tick");
                return;
            }
        };

        // Fetch host + per-VM resource usage alongside the fleet list. It is a
        // best-effort gauge: a failure here must not derail scaling, so we log
        // and carry on with an empty index (VMs keep their last sample).
        let usage = self.sample_usage().await;

        for deployment in deployments.values() {
            if !deployment.spec.is_static() {
                self.reconcile_one(deployment, &fleet, &usage).await;
            }
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
                // Poller not ready yet: mark the host gauge unavailable so the
                // dashboard shows "—" rather than stale numbers.
                self.metrics.record_host_usage(false, 0, 0.0, 0, 0, 0);
                HashMap::new()
            }
        }
    }

    async fn reconcile_one(
        &self,
        d: &Arc<Deployment>,
        fleet: &HashMap<String, SandboxInfo>,
        usage: &HashMap<String, vm::SandboxUsage>,
    ) {
        // Managed-only: static deployments are reconciled separately in
        // `reconcile` (they own no VMs and need no daemon interaction).
        debug_assert!(!d.spec.is_static(), "reconcile_one called on a static deployment");

        // The tick's snapshot can already be stale: an admin request may have
        // deregistered or rebuilt this deployment while we were reconciling an
        // earlier one. Everything below would then act on an object nobody
        // reads — including booting VMs that nothing would ever reap.
        if !self.is_live(d) {
            return;
        }

        self.prune(d, fleet);
        self.promote_pending(d, fleet).await;

        let desired = d.desired_replicas();
        let ready = d.backends().len();
        let pending = d.pending().len();
        let live = ready + pending;

        if live < desired as usize {
            self.scale_up(d, desired as usize - live).await;
        } else if ready > desired as usize {
            self.scale_down(d, ready - desired as usize).await;
        }

        self.reap_drained(d).await;
        self.renew_ttls(d, fleet).await;
        self.apply_usage(d, usage);
    }

    /// Health-re-probe the fixed upstreams of a static (proxy_pass) deployment.
    ///
    /// A static deployment has no VM lifecycle, but its upstreams can still come
    /// and go. `select` skips backends that `fail_to_connect` marked unhealthy;
    /// this is what brings a recovered upstream back — and what proactively skips
    /// one that is down but hasn't been dialed since. A hostname is re-resolved
    /// each tick, so a name that fails to resolve reads as unhealthy.
    async fn reconcile_static(&self, d: &Arc<Deployment>) {
        for b in d.backends().iter() {
            let healthy = match tokio::net::lookup_host(&b.peer).await {
                Ok(mut addrs) => match addrs.next() {
                    Some(addr) => health::probe(addr, &d.spec.health).await,
                    None => false, // resolved to nothing
                },
                Err(e) => {
                    tracing::debug!(
                        deployment = %d.spec.id,
                        upstream = %b.peer,
                        error = %e,
                        "static upstream did not resolve; marking unhealthy",
                    );
                    false
                }
            };
            let was = b.is_healthy();
            b.set_healthy(healthy);
            if was != healthy {
                tracing::info!(
                    deployment = %d.spec.id,
                    upstream = %b.peer,
                    healthy,
                    "static upstream health changed",
                );
            }
        }
    }

    /// Copy the latest per-VM CPU/memory sample onto each live backend.
    fn apply_usage(&self, d: &Arc<Deployment>, usage: &HashMap<String, vm::SandboxUsage>) {
        if usage.is_empty() {
            return;
        }
        for b in d.backends().iter() {
            if let Some(u) = usage.get(&b.sandbox_id) {
                b.set_usage(u.cpu_percent, u.memory_bytes);
            }
        }
    }

    /// Keep long-lived VMs from hitting their TTL backstop.
    ///
    /// The TTL exists so VMs self-destruct if this LB dies without reaping them.
    /// While we *are* alive, renew it past the halfway mark so a healthy VM
    /// under steady traffic doesn't get culled out from under us.
    async fn renew_ttls(&self, d: &Arc<Deployment>, fleet: &HashMap<String, SandboxInfo>) {
        let ttl = d.spec.vm_spec().ttl_seconds;
        for b in d.backends().iter() {
            let Some(info) = fleet.get(&b.sandbox_id) else {
                continue;
            };
            let remaining = info.ttl_seconds.unwrap_or(ttl);
            if info.uptime_secs < remaining / 2 {
                continue;
            }
            if let Err(e) = self.vms.renew_ttl(&b.sandbox_id, ttl).await {
                tracing::warn!(sandbox = %b.sandbox_id, error = %e, "failed to renew TTL");
            }
        }
    }

    /// Drop backends the daemon no longer reports as running.
    ///
    /// This is what catches a VM killed out-of-band: it disappears from the
    /// fleet list, and we stop routing to it.
    fn prune(&self, d: &Arc<Deployment>, fleet: &HashMap<String, SandboxInfo>) {
        let backends = d.backends();
        let kept: Vec<_> = backends
            .iter()
            .filter(|b| match fleet.get(&b.sandbox_id) {
                Some(info) => {
                    let alive = !vm::is_terminal(&info.status);
                    if !alive {
                        tracing::info!(
                            deployment = %d.spec.id,
                            sandbox = %b.sandbox_id,
                            status = ?info.status,
                            "dropping backend: VM is no longer running",
                        );
                    }
                    alive
                }
                None => {
                    tracing::info!(
                        deployment = %d.spec.id,
                        sandbox = %b.sandbox_id,
                        "dropping backend: VM is gone from the daemon",
                    );
                    false
                }
            })
            .cloned()
            .collect();

        if kept.len() != backends.len() {
            d.set_backends(kept);
        }
    }

    /// Move booted VMs into the pool.
    ///
    /// A VM is only promoted when the daemon says `Running`, it has a
    /// `guest_ip`, *and* it answers a probe. The first two are not sufficient:
    /// `wait_for_ready` reports `Ok` for stopped VMs, and `Running` says nothing
    /// about whether the guest's server is listening yet.
    async fn promote_pending(&self, d: &Arc<Deployment>, fleet: &HashMap<String, SandboxInfo>) {
        let pending = d.pending();
        if pending.is_empty() {
            return;
        }

        let mut still_pending = Vec::new();
        let mut promoted = Vec::new();
        let mut doomed = Vec::new();
        // Sandbox ids of the promoted VMs, for the post-publish ownership check:
        // the probes below are awaits, so this deployment can be replaced while
        // they run, and a VM promoted onto a dropped object is unreachable.
        let mut promoted_ids = Vec::new();

        for p in pending.iter() {
            let Some(info) = fleet.get(&p.sandbox_id) else {
                tracing::warn!(
                    deployment = %d.spec.id,
                    sandbox = %p.sandbox_id,
                    "pending VM vanished from the daemon",
                );
                continue;
            };

            match vm::routable_addr(info, d.spec.vm_spec().port) {
                Ok(addr) => {
                    if health::probe(addr, &d.spec.health).await {
                        let boot_secs = now_secs().saturating_sub(p.created_at);
                        tracing::info!(
                            deployment = %d.spec.id,
                            sandbox = %p.sandbox_id,
                            %addr,
                            boot_secs,
                            "VM ready",
                        );
                        self.metrics.record_cold_start(&d.spec.id, boot_secs);
                        promoted_ids.push(p.sandbox_id.clone());
                        promoted.push(Arc::new(VmBackend::new(p.sandbox_id.clone(), addr)));
                    } else {
                        still_pending.push(p.clone()); // booting; try next tick
                    }
                }
                Err(vm::VmError::NotRunning { status, .. }) if !vm::is_terminal(&status) => {
                    still_pending.push(p.clone()); // provisioning
                }
                Err(e) => {
                    // Terminal, or unroutable (no guest_ip). Either way it will
                    // never serve, so stop waiting on it and reclaim the slot.
                    tracing::error!(
                        deployment = %d.spec.id,
                        sandbox = %p.sandbox_id,
                        error = %e,
                        "giving up on VM",
                    );
                    doomed.push(p.sandbox_id.clone());
                }
            }
        }

        d.set_pending(still_pending);

        if !promoted.is_empty() {
            let mut backends = (*d.backends()).clone();
            backends.extend(promoted);
            d.set_backends(backends);
            // Release anything blocked on a cold start.
            d.ready_signal.notify_waiters();
        }

        for id in doomed {
            if let Err(e) = self.vms.kill(&id).await {
                tracing::warn!(sandbox = %id, error = %e, "failed to kill doomed VM");
            }
        }

        // A promotion moves a VM out of `pending` and into `backends`; if the
        // deployment was replaced between those two stores, the replacement
        // inherited neither and the VM is now tracked nowhere.
        self.kill_unclaimed(d, &promoted_ids).await;
    }

    async fn scale_up(&self, d: &Arc<Deployment>, count: usize) {
        tracing::info!(deployment = %d.spec.id, count, "scaling up");
        let mut pending = (*d.pending()).clone();
        let mut created = Vec::new();

        for _ in 0..count {
            // Each create is a slow await, so re-check between boots: an admin
            // request can have deregistered this deployment since the last one,
            // and every further VM would be born abandoned.
            if !self.is_live(d) {
                tracing::info!(
                    deployment = %d.spec.id,
                    "deployment is no longer registered; stopping scale-up",
                );
                break;
            }
            let name = vm::replica_name(&d.spec.id, self.next_nonce());
            match self.vms.create(d.spec.vm_spec(), name).await {
                Ok(sandbox) => {
                    let sandbox_id = sandbox.sandbox_id().to_string();
                    created.push(sandbox_id.clone());
                    pending.push(PendingVm {
                        sandbox_id,
                        created_at: now_secs(),
                    });
                }
                Err(e) => {
                    tracing::error!(deployment = %d.spec.id, error = %e, "failed to create VM");
                    break; // daemon is unhappy; don't hammer it this tick
                }
            }
        }

        // Record only VMs the daemon actually accepted, so the dashboard's
        // create count matches what booted rather than what was attempted.
        self.metrics.record_scale_up(&d.spec.id, created.len() as u64);

        // Publish *before* the ownership check, never after: a pool-preserving
        // edit copies whatever is visible here, so anything already published is
        // safely inherited and must not be killed. Whatever the replacement did
        // not take, `kill_unclaimed` reaps.
        d.set_pending(pending);
        self.kill_unclaimed(d, &created).await;
    }

    /// Retire surplus VMs, most-idle first, by marking them draining.
    ///
    /// Draining stops new requests without cutting off in-flight ones; the
    /// actual kill happens in `reap_drained` once they finish.
    async fn scale_down(&self, d: &Arc<Deployment>, count: usize) {
        let backends = d.backends();
        let mut candidates: Vec<_> = backends
            .iter()
            .filter(|b| !b.is_draining())
            .cloned()
            .collect();
        // Prefer idle VMs so draining finishes quickly.
        candidates.sort_by_key(|b| (b.in_flight(), b.last_active()));

        let mut drained = 0u64;
        for b in candidates.iter().take(count) {
            tracing::info!(
                deployment = %d.spec.id,
                sandbox = %b.sandbox_id,
                in_flight = b.in_flight(),
                "draining VM",
            );
            b.set_draining();
            drained += 1;
        }
        self.metrics.record_scale_down(&d.spec.id, drained);
    }

    /// Kill drained VMs, and force-kill any that overstay the drain deadline.
    async fn reap_drained(&self, d: &Arc<Deployment>) {
        let backends = d.backends();
        let deadline = d.spec.scaling.drain_timeout_secs;

        let (done, keep): (Vec<_>, Vec<_>) = backends.iter().cloned().partition(|b| {
            if !b.is_draining() {
                return false;
            }
            let idle = b.in_flight() == 0;
            let expired = now_secs().saturating_sub(b.last_active()) >= deadline;
            if !idle && expired {
                tracing::warn!(
                    deployment = %d.spec.id,
                    sandbox = %b.sandbox_id,
                    in_flight = b.in_flight(),
                    "drain deadline exceeded; killing VM with requests in flight",
                );
            }
            idle || expired
        });

        if done.is_empty() {
            return;
        }
        d.set_backends(keep);

        self.metrics.record_reaped(&d.spec.id, done.len() as u64);
        for b in done {
            tracing::info!(deployment = %d.spec.id, sandbox = %b.sandbox_id, "killing VM");
            if let Err(e) = self.vms.kill(&b.sandbox_id).await {
                tracing::warn!(sandbox = %b.sandbox_id, error = %e, "failed to kill VM");
            }
        }
    }

    /// Adopt VMs from a previous run of this LB.
    ///
    /// Without this, a restart would leave old VMs running while booting a fresh
    /// set — the orphans would only die when their TTL expired.
    pub async fn adopt_existing(&self) {
        let fleet = match self.vms.list().await {
            Ok(list) => list,
            Err(e) => {
                tracing::error!(error = %e, "could not list sandboxes for adoption");
                return;
            }
        };

        let deployments = self.registry.deployments();
        let mut adopted: HashMap<String, Vec<Arc<VmBackend>>> = HashMap::new();
        let mut orphans = Vec::new();

        for info in &fleet {
            let Some(owner) = vm::owner_of(&info.name) else {
                continue; // not ours; leave it alone
            };
            let Some(d) = deployments.get(owner) else {
                // Ours, but its deployment is gone from the state file.
                orphans.push(info.id.clone());
                continue;
            };
            if d.spec.is_static() {
                // The deployment id was reused for a static one since this VM was
                // created; it owns no VMs, so this sandbox is an orphan.
                orphans.push(info.id.clone());
                continue;
            }
            match vm::routable_addr(info, d.spec.vm_spec().port) {
                Ok(addr) if health::probe(addr, &d.spec.health).await => {
                    tracing::info!(
                        deployment = %owner,
                        sandbox = %info.id,
                        %addr,
                        "adopting existing VM",
                    );
                    adopted
                        .entry(owner.to_string())
                        .or_default()
                        .push(Arc::new(VmBackend::new(info.id.clone(), addr)));
                }
                _ => orphans.push(info.id.clone()),
            }
        }

        for (id, backends) in adopted {
            if let Some(d) = deployments.get(&id) {
                d.set_backends(backends);
            }
        }

        for id in orphans {
            tracing::info!(sandbox = %id, "killing orphaned VM from a previous run");
            if let Err(e) = self.vms.kill(&id).await {
                tracing::warn!(sandbox = %id, error = %e, "failed to kill orphan");
            }
        }
    }

    /// Drain and kill every VM of a deployment, e.g. on DELETE.
    pub async fn teardown(&self, d: &Arc<Deployment>) {
        // A static deployment's backends are not sandboxes — there is nothing to
        // kill on the daemon. Just drop them from routing.
        if d.spec.is_static() {
            d.set_backends(Vec::new());
            return;
        }
        for b in d.backends().iter() {
            b.set_draining();
            if let Err(e) = self.vms.kill(&b.sandbox_id).await {
                tracing::warn!(sandbox = %b.sandbox_id, error = %e, "failed to kill VM");
            }
        }
        for p in d.pending().iter() {
            if let Err(e) = self.vms.kill(&p.sandbox_id).await {
                tracing::warn!(sandbox = %p.sandbox_id, error = %e, "failed to kill pending VM");
            }
        }
        d.set_backends(Vec::new());
        d.set_pending(Vec::new());
    }

    /// Evict a single VM from a deployment's pool.
    ///
    /// Two modes, both consistent with the rule that the autoscaler is the only
    /// writer of the `backends`/`pending` vecs — this never mutates them, it
    /// flips the backend's atomic drain flag and/or kills the sandbox, and lets
    /// the next reconcile tick reconcile the vecs:
    ///
    /// - **graceful** (`force = false`): mark the VM draining so it stops taking
    ///   new requests but finishes in-flight ones; `reap_drained` kills it once
    ///   idle or at `drain_timeout_secs`. Returns [`EvictOutcome::Draining`].
    /// - **force** (`force = true`): kill the sandbox now, dropping in-flight
    ///   requests (they fail over to another VM via the proxy's retry). Returns
    ///   [`EvictOutcome::Killed`].
    ///
    /// A pending (still-booting) VM holds no traffic, so it is simply killed in
    /// either mode. After eviction the autoscaler is nudged, so a replacement
    /// boots immediately if the scaling policy still wants the capacity.
    pub async fn evict(&self, d: &Arc<Deployment>, sandbox_id: &str, force: bool) -> EvictOutcome {
        // A ready backend.
        if let Some(b) = d
            .backends()
            .iter()
            .find(|b| b.sandbox_id == sandbox_id)
            .cloned()
        {
            // Stop new traffic regardless of mode; a draining VM is skipped by
            // `select`, so no request is routed to it after this point.
            b.set_draining();

            if !force {
                tracing::info!(
                    deployment = %d.spec.id,
                    sandbox = %sandbox_id,
                    in_flight = b.in_flight(),
                    "evicting VM (draining)",
                );
                d.scale_signal.notify_one();
                return EvictOutcome::Draining;
            }

            tracing::info!(
                deployment = %d.spec.id,
                sandbox = %sandbox_id,
                in_flight = b.in_flight(),
                "evicting VM (force kill)",
            );
            if let Err(e) = self.vms.kill(sandbox_id).await {
                tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to kill evicted VM");
                return EvictOutcome::KillFailed(e.to_string());
            }
            // `prune` removes the now-dead backend next tick and won't record a
            // reap, so count it here to keep the dashboard's reaped total honest.
            self.metrics.record_reaped(&d.spec.id, 1);
            d.scale_signal.notify_one();
            return EvictOutcome::Killed;
        }

        // A pending, still-booting VM: nothing in-flight, so just kill it. The
        // next `promote_pending` drops it from the pending vec.
        if d.pending().iter().any(|p| p.sandbox_id == sandbox_id) {
            tracing::info!(
                deployment = %d.spec.id,
                sandbox = %sandbox_id,
                "evicting pending VM",
            );
            if let Err(e) = self.vms.kill(sandbox_id).await {
                tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to kill evicted pending VM");
                return EvictOutcome::KillFailed(e.to_string());
            }
            d.scale_signal.notify_one();
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
    /// The VM was marked draining; the autoscaler will reap it once idle.
    Draining,
    /// No VM with that id is in the deployment's pool (ready or pending).
    NotFound,
    /// The VM was found but the daemon refused to kill it.
    KillFailed(String),
}

#[async_trait]
impl BackgroundService for Autoscaler {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        tracing::info!("autoscaler starting");
        self.adopt_existing().await;

        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // A cold-start request nudges `scale_signal`, so a scaled-to-zero
            // deployment reacts immediately instead of waiting out the tick.
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
}

/// Resolve as soon as *any* deployment asks to be scaled.
async fn wait_for_any_scale_signal(registry: &Arc<Registry>) {
    let deployments = registry.deployments();
    if deployments.is_empty() {
        // Nothing to wait on; let the ticker drive the loop.
        std::future::pending::<()>().await;
    }
    let waits: Vec<_> = deployments
        .values()
        .cloned()
        .map(|d| {
            Box::pin(async move { d.scale_signal.notified().await })
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        })
        .collect();
    let _ = futures::future::select_all(waits).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeploymentSpec, HealthCheck, RouteRule, ScalingPolicy, VmSpec};
    use crate::deployment::VmBackend;
    use crate::metrics::Metrics;
    use heyo_sdk::SandboxDriver;

    fn spec() -> DeploymentSpec {
        DeploymentSpec {
            id: "demo".into(),
            routes: vec![RouteRule {
                host: Some("demo.local".into()),
                host_suffix: None,
                path_prefix: None,
            }],
            vm: Some(VmSpec {
                driver: SandboxDriver::Firecracker,
                image: None,
                port: 8080,
                start_command: None,
                size_class: None,
                disk_size_gb: None,
                working_directory: None,
                env_vars: None,
                setup_hooks: None,
                open_ports: vec![],
                ttl_seconds: 3600,
            }),
            scaling: ScalingPolicy::default(),
            health: HealthCheck::default(),
            upstreams: vec![],
        }
    }

    /// A static (proxy_pass) deployment with fixed upstreams.
    fn static_spec() -> DeploymentSpec {
        DeploymentSpec {
            id: "proxy".into(),
            routes: vec![RouteRule {
                host: None,
                host_suffix: None,
                path_prefix: Some("/legacy".into()),
            }],
            vm: None,
            scaling: ScalingPolicy::default(),
            health: HealthCheck::default(),
            upstreams: vec!["127.0.0.1:9".into()],
        }
    }

    fn autoscaler() -> (Autoscaler, Arc<Registry>) {
        let registry = Arc::new(Registry::new("unused.json"));
        registry.upsert(spec());
        // A daemon URL nothing listens on: fine, because the graceful and
        // not-found paths never call it.
        let vms = VmManager::new(Some("http://127.0.0.1:1".into()));
        (
            Autoscaler::new(registry.clone(), vms, Arc::new(Metrics::new())),
            registry,
        )
    }

    #[tokio::test]
    async fn graceful_eviction_marks_the_backend_draining() {
        let (a, reg) = autoscaler();
        let d = reg.get("demo").unwrap();
        let b = Arc::new(VmBackend::new("sb-1".into(), "10.0.0.1:80".parse().unwrap()));
        d.set_backends(vec![b.clone()]);

        let out = a.evict(&d, "sb-1", false).await;
        assert!(matches!(out, EvictOutcome::Draining), "got {out:?}");
        assert!(b.is_draining(), "eviction must stop new traffic to the VM");
        // The backend is still in the pool (the autoscaler reaps it later), but
        // is no longer selectable.
        assert!(d.select(&[]).is_none());
    }

    #[tokio::test]
    async fn evicting_an_unknown_vm_is_not_found() {
        let (a, reg) = autoscaler();
        let d = reg.get("demo").unwrap();
        d.set_backends(vec![Arc::new(VmBackend::new(
            "sb-1".into(),
            "10.0.0.1:80".parse().unwrap(),
        ))]);

        let out = a.evict(&d, "sb-does-not-exist", false).await;
        assert!(matches!(out, EvictOutcome::NotFound), "got {out:?}");
    }

    fn pending(id: &str) -> PendingVm {
        PendingVm {
            sandbox_id: id.into(),
            created_at: now_secs(),
        }
    }

    #[test]
    fn a_deployment_is_live_only_while_the_registry_holds_that_object() {
        let (a, reg) = autoscaler();
        let d = reg.get("demo").unwrap();
        assert!(a.is_live(&d));

        // A rebuild installs a different object; the old handle is stale even
        // though the id is still registered.
        let fresh = reg.upsert(spec());
        assert!(!a.is_live(&d));
        assert!(a.is_live(&fresh));

        reg.remove("demo");
        assert!(!a.is_live(&fresh));
    }

    /// The leak this guards: a VM created while an admin request deregisters the
    /// deployment lands in a pool nobody reconciles, and runs until its TTL.
    #[test]
    fn vms_created_for_a_deregistered_deployment_are_unclaimed() {
        let (a, reg) = autoscaler();
        let d = reg.get("demo").unwrap();
        let created = vec!["sb-1".to_string()];

        assert!(
            a.unclaimed(&d, &created).is_empty(),
            "while it is live, the autoscaler owns what it created",
        );

        reg.remove("demo");
        assert_eq!(a.unclaimed(&d, &created), created);
    }

    /// A rebuild (`POST`, or a `PUT` that changes the VM template) starts from an
    /// empty pool, so nothing it left behind is inherited.
    #[test]
    fn a_rebuild_inherits_nothing() {
        let (a, reg) = autoscaler();
        let d = reg.get("demo").unwrap();
        d.set_pending(vec![pending("sb-1")]);

        reg.upsert(spec());
        assert_eq!(a.unclaimed(&d, &["sb-1".to_string()]), vec!["sb-1".to_string()]);
    }

    /// …but a pool-preserving edit carries the pool over, so those VMs are still
    /// tracked and must *not* be killed.
    #[test]
    fn a_pool_preserving_edit_keeps_the_vms_it_inherited() {
        let (a, reg) = autoscaler();
        let d = reg.get("demo").unwrap();
        d.set_pending(vec![pending("sb-inherited")]);
        d.set_backends(vec![Arc::new(VmBackend::new(
            "sb-running".into(),
            "10.0.0.1:80".parse().unwrap(),
        ))]);

        // A scaling-only edit: `Registry::update` copies both lists onto the new
        // object, which now owns those VMs.
        let mut edited = spec();
        edited.scaling.max_replicas = 9;
        let new = reg.update(edited).unwrap();
        assert!(!Arc::ptr_eq(&new, &d), "the edit installs a new object");

        assert!(
            a.unclaimed(&d, &["sb-inherited".into(), "sb-running".into()]).is_empty(),
            "the replacement inherited these; killing them would drop live capacity",
        );
        // One created *after* the copy is in neither list, so it is abandoned.
        assert_eq!(
            a.unclaimed(&d, &["sb-too-late".to_string()]),
            vec!["sb-too-late".to_string()],
        );
    }

    #[test]
    fn nothing_created_means_nothing_to_reap() {
        let (a, reg) = autoscaler();
        let d = reg.get("demo").unwrap();
        reg.remove("demo");
        // Stale, but there is nothing to check — and no registry lookup to make.
        assert!(a.unclaimed(&d, &[]).is_empty());
    }

    #[tokio::test]
    async fn tearing_down_a_static_deployment_just_clears_routing() {
        let (a, reg) = autoscaler();
        reg.upsert(static_spec());
        let d = reg.get("proxy").unwrap();
        // Prepopulated from the spec's upstreams.
        assert_eq!(d.backends().len(), 1);

        // Static backends are not sandboxes, so teardown must not dial the
        // daemon (the test VmManager points at a dead port); it just drops them.
        a.teardown(&d).await;
        assert!(d.backends().is_empty());
    }
}
