//! The control loop: owns every VM lifecycle decision.
//!
//! The daemon offers no event stream, so this polls. It hits `Sandbox::list()`
//! exactly once per tick and indexes the result, because `Sandbox::info()`
//! fetches that same full list and filters client-side — polling per-VM would be
//! quadratic in fleet size.
//!
//! All VM creation happens here and never in a proxy filter, so a slow boot can
//! never stall request handling.

use crate::config::IdleAction;
use crate::deployment::{Deployment, PendingVm, VmBackend, now_secs};
use crate::health;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::vm::{self, VmManager};
use async_trait::async_trait;
use futures::StreamExt;
use heyo_sdk::SandboxInfo;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const TICK: Duration = Duration::from_secs(2);

/// How long a still-booting VM may go without a log line. Chosen far above
/// [`TICK`]: the point is to prove a stuck boot is still stuck, not to narrate
/// every poll of a VM that will be up in ten seconds.
const BOOT_HEARTBEAT: u64 = 30;

/// How many deployments reconcile at once.
///
/// The bound exists because the daemon is one process on one host: a fleet of
/// thousands of sandboxes would otherwise open thousands of simultaneous
/// connections to it and to guest health endpoints. High enough that one slow
/// VM cannot stall the tick, low enough to stay a polite client.
const RECONCILE_CONCURRENCY: usize = 32;

/// How many VM creates may be in flight across the *whole fleet* at once.
///
/// Separate from — and much smaller than — [`RECONCILE_CONCURRENCY`], because a
/// create is not just a request: it allocates a tap device, copies a rootfs and
/// starts a hypervisor. Thirty-two concurrent reconciles each deciding to scale
/// up is a host problem, not a loop problem, so the limit is on the expensive
/// operation rather than on the loop that reaches it.
const CREATE_CONCURRENCY: usize = 8;

/// How often to look for suspended VMs no deployment claims.
///
/// Slow on purpose. The daemon answers `GET /sandboxes/inactive` by walking its
/// persistence directory and loading metadata for every sandbox it has ever
/// created, so this is the most expensive question app-lb asks it. It is also a
/// backstop, not a control loop: what it catches is the residue of a crash
/// between stopping a VM and recording that we did.
const SUSPENDED_SWEEP: Duration = Duration::from_secs(300);

pub struct Autoscaler {
    registry: Arc<Registry>,
    vms: VmManager,
    metrics: Arc<Metrics>,
    /// Monotonic source for replica-name nonces. Not for addressing — the
    /// daemon assigns sandbox ids — just to keep our names unique.
    nonce: AtomicU64,
    /// Fleet-wide budget for simultaneous VM creates. See
    /// [`CREATE_CONCURRENCY`].
    creates: tokio::sync::Semaphore,
}

impl Autoscaler {
    pub fn new(registry: Arc<Registry>, vms: VmManager, metrics: Arc<Metrics>) -> Self {
        Self {
            registry,
            vms,
            metrics,
            nonce: AtomicU64::new(now_secs()),
            creates: tokio::sync::Semaphore::new(CREATE_CONCURRENCY),
        }
    }

    /// The daemon client, for callers that need to talk to a VM the autoscaler
    /// owns — `exec` and `shell` in the admin API.
    pub fn vms(&self) -> &VmManager {
        &self.vms
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
    ///
    /// Concurrent, not sequential. A fleet of agent sandboxes is thousands of
    /// deployments; awaiting each one's health probes and daemon calls in turn
    /// meant a single slow VM held up every other deployment's tick, and the
    /// pass could outrun [`TICK`] entirely. Bounded by
    /// [`RECONCILE_CONCURRENCY`] so a large fleet cannot open thousands of
    /// simultaneous connections to the daemon.
    async fn reconcile(&self) {
        let deployments = self.registry.deployments();
        let (statics, managed): (Vec<_>, Vec<_>) = deployments
            .values()
            .cloned()
            .partition(|d| d.spec.is_static());

        // Static (proxy_pass) deployments need no daemon interaction — health-
        // re-probe them first, and unconditionally, so they keep working even
        // when the daemon is unreachable (or app-lb runs with no VM deployments
        // at all and heyvmd isn't running).
        futures::stream::iter(&statics)
            .for_each_concurrent(RECONCILE_CONCURRENCY, |d| self.reconcile_static(d))
            .await;

        if managed.is_empty() {
            return; // nothing needs the daemon; don't call it at all
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

        // Split before dispatching: a deployment sitting at its desired size
        // with nothing booting and nothing draining needs no `await` at all, and
        // at fleet scale that is nearly all of them. Doing their bookkeeping
        // inline keeps the concurrent stream carrying only real work.
        let mut busy = Vec::new();
        for d in &managed {
            if !self.is_live(d) {
                continue;
            }
            self.prune(d, &fleet);
            if at_rest(d, &fleet) {
                self.apply_usage(d, &usage);
            } else {
                busy.push(d);
            }
        }

        futures::stream::iter(busy)
            .for_each_concurrent(RECONCILE_CONCURRENCY, |d| {
                self.reconcile_one(d, &fleet, &usage)
            })
            .await;
    }

    // (`at_rest` is a free function below — it needs no autoscaler state, and
    // being pure is what makes the fast path testable without a daemon.)

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
        // deregistered or rebuilt this deployment while a *sibling* deployment
        // was reconciling. Everything below would then act on an object nobody
        // reads — including booting VMs that nothing would ever reap. Checked
        // again here even though `reconcile` checked before dispatching,
        // because the dispatch itself yields.
        if !self.is_live(d) {
            return;
        }

        // `prune` already ran in `reconcile`, which needed a current backend
        // list to decide this deployment had work at all.
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

    /// Move booted VMs into the pool, and say what the rest are waiting on.
    ///
    /// A VM is only promoted when the daemon says `Running`, it has a
    /// `guest_ip`, *and* it answers a probe. The first two are not sufficient:
    /// `wait_for_ready` reports `Ok` for stopped VMs, and `Running` says nothing
    /// about whether the guest's server is listening yet.
    ///
    /// The other half of this function is the case where that never happens. A VM
    /// whose guest boots but whose *server* doesn't — a bad `start_command`, an
    /// env var pointing at a directory that isn't there, a binary that exits — is
    /// `Running` with a `guest_ip` and fails the probe forever. Without the
    /// progress logging and the deadline below, that is completely invisible:
    /// nothing is logged, `min_replicas` is silently never met, and the only
    /// symptom is requests timing out on a cold start that will never end.
    async fn promote_pending(&self, d: &Arc<Deployment>, fleet: &HashMap<String, SandboxInfo>) {
        let pending = d.pending();
        if pending.is_empty() {
            return;
        }

        let boot_timeout = d.spec.scaling.boot_timeout_secs;
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
                    age_secs = p.age_secs(),
                    "pending VM vanished from the daemon",
                );
                continue;
            };
            let age = p.age_secs();

            let addr = match vm::routable_addr(info, d.spec.vm_spec().port) {
                Ok(addr) => health::probe(addr, &d.spec.health).await.then_some(addr),
                // Provisioning, or a status the daemon hasn't classified yet.
                Err(vm::VmError::NotRunning { status, .. }) if !vm::is_terminal(&status) => None,
                Err(e) => {
                    // Terminal, or unroutable (no guest_ip). Either way it will
                    // never serve, so stop waiting on it and reclaim the slot.
                    tracing::error!(
                        deployment = %d.spec.id,
                        sandbox = %p.sandbox_id,
                        age_secs = age,
                        error = %e,
                        "giving up on VM",
                    );
                    doomed.push(p.sandbox_id.clone());
                    continue;
                }
            };

            if let Some(addr) = addr {
                tracing::info!(
                    deployment = %d.spec.id,
                    sandbox = %p.sandbox_id,
                    %addr,
                    boot_secs = age,
                    "VM ready",
                );
                self.metrics.record_cold_start(&d.spec.id, age);
                promoted_ids.push(p.sandbox_id.clone());
                promoted.push(Arc::new(VmBackend::new(p.sandbox_id.clone(), addr)));
                continue;
            }

            // Still booting. Either it gets a deadline or it gets a heartbeat,
            // but it does not get silence.
            if boot_timeout > 0 && age >= boot_timeout {
                tracing::error!(
                    deployment = %d.spec.id,
                    sandbox = %p.sandbox_id,
                    age_secs = age,
                    boot_timeout_secs = boot_timeout,
                    status = ?info.status,
                    waiting_on = %boot_stall(info, &d.spec.health, d.spec.vm_spec().port),
                    "VM never became ready inside its boot timeout; killing it so the pool \
                     can try again",
                );
                self.metrics.record_boot_timeout(&d.spec.id);
                doomed.push(p.sandbox_id.clone());
                continue;
            }
            still_pending.push(self.note_boot_progress(d, p, info, age));
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

    /// Log a still-booting VM's progress when there is something new to say, and
    /// return it with the bookkeeping for the next tick.
    ///
    /// "Something new" is a status transition or [`BOOT_HEARTBEAT`] elapsed —
    /// a line per pending VM per 2s tick would drown the log it is meant to make
    /// readable, and a warm pool booting normally has nothing to report. The first
    /// sighting always logs, because until then nothing anywhere has named this
    /// sandbox id: `scale_up` only counts what it created.
    fn note_boot_progress(
        &self,
        d: &Arc<Deployment>,
        p: &PendingVm,
        info: &SandboxInfo,
        age: u64,
    ) -> PendingVm {
        let next = PendingVm {
            status: Some(info.status.clone()),
            ..p.clone()
        };
        let changed = p.status.as_ref() != Some(&info.status);
        if !changed && age.saturating_sub(p.reported_at_secs) < BOOT_HEARTBEAT {
            return next;
        }

        // Past the request budget this boot has already cost somebody a 503, so
        // it stops being routine progress.
        if age >= d.spec.scaling.cold_start_timeout_secs {
            tracing::warn!(
                deployment = %d.spec.id,
                sandbox = %p.sandbox_id,
                age_secs = age,
                status = ?info.status,
                waiting_on = %boot_stall(info, &d.spec.health, d.spec.vm_spec().port),
                "VM is taking longer to boot than a request will wait for",
            );
        } else {
            tracing::info!(
                deployment = %d.spec.id,
                sandbox = %p.sandbox_id,
                age_secs = age,
                status = ?info.status,
                waiting_on = %boot_stall(info, &d.spec.health, d.spec.vm_spec().port),
                "VM is still booting",
            );
        }
        PendingVm {
            reported_at_secs: age,
            ..next
        }
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
            // Held across the create or resume. Reconciles run concurrently, so
            // without this a fleet-wide scale-up event would ask the daemon to
            // start `RECONCILE_CONCURRENCY` hypervisors at once.
            let _permit = self.creates.acquire().await;

            // A suspended sandbox is preferred over a fresh one, and not just to
            // save a boot: it *is* the deployment's state. Creating a new VM
            // while one sits stopped would strand that VM's `/workspace` disk
            // and hand the caller an empty sandbox in its place.
            if let Some(sandbox_id) = self.take_suspended(d) {
                match self.vms.resume(&sandbox_id).await {
                    Ok(_) => {
                        tracing::info!(
                            deployment = %d.spec.id,
                            sandbox = %sandbox_id,
                            "resumed suspended VM",
                        );
                        created.push(sandbox_id.clone());
                        pending.push(PendingVm::new(sandbox_id));
                        continue;
                    }
                    Err(e) => {
                        // It is already forgotten, so it will not be resumed
                        // again. Kill it rather than leave it stopped forever
                        // holding a disk nothing tracks.
                        tracing::warn!(
                            deployment = %d.spec.id,
                            sandbox = %sandbox_id,
                            error = %e,
                            "failed to resume suspended VM; destroying it and booting a fresh one",
                        );
                        if let Err(e) = self.vms.kill(&sandbox_id).await {
                            tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to kill VM");
                        }
                    }
                }
            }

            let name = vm::replica_name(&d.spec.id, self.next_nonce());
            match self.vms.create(d.spec.vm_spec(), name).await {
                Ok(sandbox) => {
                    let sandbox_id = sandbox.sandbox_id().to_string();
                    created.push(sandbox_id.clone());
                    pending.push(PendingVm::new(sandbox_id));
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
        let retain = d.spec.scaling.idle_action == IdleAction::Retain;
        let mut suspended = Vec::new();
        for b in done {
            if retain {
                tracing::info!(deployment = %d.spec.id, sandbox = %b.sandbox_id, "suspending VM");
                match self.vms.suspend(&b.sandbox_id).await {
                    // Recorded only on success. A sandbox we failed to stop is
                    // still running and still in the fleet list, so recording it
                    // as suspended would make the next tick skip a live VM.
                    Ok(()) => suspended.push(b.sandbox_id.clone()),
                    Err(e) => {
                        tracing::warn!(
                            deployment = %d.spec.id,
                            sandbox = %b.sandbox_id,
                            error = %e,
                            "failed to suspend VM; killing it instead so it cannot leak",
                        );
                        if let Err(e) = self.vms.kill(&b.sandbox_id).await {
                            tracing::warn!(sandbox = %b.sandbox_id, error = %e, "failed to kill VM");
                        }
                    }
                }
            } else {
                tracing::info!(deployment = %d.spec.id, sandbox = %b.sandbox_id, "killing VM");
                if let Err(e) = self.vms.kill(&b.sandbox_id).await {
                    tracing::warn!(sandbox = %b.sandbox_id, error = %e, "failed to kill VM");
                }
            }
        }
        self.remember_suspended(d, suspended);
    }

    /// Record sandboxes we stopped, and persist that immediately.
    ///
    /// Immediately, and not on some later flush, because between the stop and
    /// the write this id exists in exactly one place: memory. The daemon drops a
    /// stopped sandbox from `GET /sandboxes`, so a crash in that window leaves a
    /// VM holding a disk that nothing knows to resume or reap.
    fn remember_suspended(&self, d: &Arc<Deployment>, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let changed = d.mutate_state(|s| {
            for id in ids {
                if !s.suspended.contains(&id) {
                    s.suspended.push(id);
                }
            }
        });
        if changed && let Err(e) = self.registry.persist_one(&d.spec.id) {
            tracing::error!(
                deployment = %d.spec.id,
                error = %e,
                "failed to persist suspended sandboxes; they may be leaked on restart",
            );
        }
    }

    /// Forget a sandbox we suspended — it has been resumed, or destroyed.
    fn forget_suspended(&self, d: &Arc<Deployment>, sandbox_id: &str) {
        let changed = d.mutate_state(|s| s.suspended.retain(|id| id != sandbox_id));
        if changed && let Err(e) = self.registry.persist_one(&d.spec.id) {
            tracing::error!(deployment = %d.spec.id, error = %e, "failed to persist state");
        }
    }

    /// Claim the oldest suspended sandbox, removing it from the record.
    ///
    /// Claimed *before* the resume rather than after, so two concurrent
    /// scale-ups cannot both try to start the same VM. The cost of that choice
    /// is that a failed resume has already been forgotten, which is why
    /// `scale_up` kills it rather than leaving it stopped and untracked.
    fn take_suspended(&self, d: &Arc<Deployment>) -> Option<String> {
        let mut taken = None;
        let changed = d.mutate_state(|s| {
            if !s.suspended.is_empty() {
                taken = Some(s.suspended.remove(0));
            }
        });
        if changed && let Err(e) = self.registry.persist_one(&d.spec.id) {
            tracing::error!(deployment = %d.spec.id, error = %e, "failed to persist state");
        }
        taken
    }

    /// Destroy stopped sandboxes of ours that no deployment claims.
    ///
    /// The backstop for `idle_action: retain`. A stopped sandbox is invisible to
    /// every other mechanism here — it is absent from the fleet list, so `prune`
    /// and `adopt_existing` cannot see it, and its TTL does not run while it is
    /// stopped. If the record of it is lost (a crash between the stop and the
    /// state write, a state file deleted by hand), it keeps its disk forever with
    /// nothing to reclaim it.
    ///
    /// Only sandboxes named `applb-<deployment>-<nonce>` are touched, and only
    /// when their deployment either does not exist or does not list them. A
    /// sandbox somebody else made is never destroyed.
    async fn sweep_suspended(&self) {
        // Both listings, because *where* a stopped sandbox turns up depends on
        // the backend. mvm-ctrl re-adds every persisted **KVM** sandbox to
        // `GET /sandboxes` on each call, so a stopped one appears there with
        // status `Stopped` — and is therefore excluded from the inactive
        // listing, which subtracts whatever the active list holds. A stopped
        // **Firecracker** sandbox is the other way round: absent from the fleet
        // list, present in the inactive one. Reading only one would sweep half
        // the fleet and silently ignore the other.
        let fleet = match self.vms.list().await {
            Ok(list) => list,
            Err(e) => {
                tracing::debug!(error = %e, "no fleet list; skipping the suspended sweep");
                return;
            }
        };
        let inactive = match self.vms.list_inactive().await {
            Ok(list) => list,
            Err(e) => {
                tracing::debug!(error = %e, "could not list inactive sandboxes; skipping sweep");
                return;
            }
        };

        let deployments = self.registry.deployments();

        // Everything of ours the daemon does not report as running, from either
        // listing. A *running* VM is out of scope here: `adopt_existing` and
        // `kill_unclaimed` own that case, and killing one on this path would
        // race them.
        let stopped = inactive
            .iter()
            .chain(fleet.iter().filter(|i| vm::is_terminal(&i.status)));

        let mut orphans: Vec<(String, String)> = Vec::new();
        for info in stopped {
            let Some(owner) = vm::owner_of(&info.name) else {
                continue; // not ours
            };
            let claimed = deployments
                .get(owner)
                .is_some_and(|d| d.state().suspended.contains(&info.id));
            if !claimed && !orphans.iter().any(|(id, _)| id == &info.id) {
                orphans.push((info.id.clone(), owner.to_string()));
            }
        }

        for (sandbox_id, owner) in &orphans {
            tracing::warn!(deployment = %owner, sandbox = %sandbox_id, "destroying unclaimed suspended VM");
            if let Err(e) = self.vms.kill(sandbox_id).await {
                tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to kill suspended VM");
            }
        }

        // And the other direction: ids we still think are suspended that the
        // daemon has no record of at all — deleted out of band, or lost with the
        // host's state. Left in place they would cost a doomed resume attempt on
        // every scale-up.
        let known: HashSet<&str> = inactive
            .iter()
            .chain(fleet.iter())
            .map(|i| i.id.as_str())
            .collect();
        for d in deployments.values() {
            let stale: Vec<String> = d
                .state()
                .suspended
                .iter()
                .filter(|id| !known.contains(id.as_str()))
                .cloned()
                .collect();
            for id in stale {
                tracing::warn!(
                    deployment = %d.spec.id,
                    sandbox = %id,
                    "forgetting a suspended VM the daemon no longer has",
                );
                self.forget_suspended(d, &id);
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
            // A VM this deployment deliberately suspended is neither adoptable
            // nor an orphan: it is stopped on purpose and its data disk *is* the
            // deployment's state. Without this it fails `routable_addr` and gets
            // killed here — losing the sandbox on every restart, which is the
            // exact opposite of what `idle_action: retain` was asked for.
            //
            // Checked rather than assumed absent, because whether a stopped
            // sandbox appears in the fleet list at all is backend-dependent:
            // mvm-ctrl re-adds every persisted *KVM* sandbox to `GET /sandboxes`
            // on each call, while a stopped *Firecracker* one is simply missing.
            if d.state().suspended.contains(&info.id) {
                tracing::info!(
                    deployment = %owner,
                    sandbox = %info.id,
                    "leaving a suspended VM stopped; it will be resumed on demand",
                );
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
            if let Some(d) = deployments.get(id.as_str()) {
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
        // Suspended sandboxes are not in either list and are absent from the
        // daemon's fleet list, so nothing else would ever find them. Teardown is
        // the last moment this deployment's record of them exists.
        for sandbox_id in &d.state().suspended {
            tracing::info!(
                deployment = %d.spec.id,
                sandbox = %sandbox_id,
                "destroying suspended VM as its deployment goes away",
            );
            if let Err(e) = self.vms.kill(sandbox_id).await {
                tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to kill suspended VM");
            }
        }
        d.set_backends(Vec::new());
        d.set_pending(Vec::new());
        d.mutate_state(|s| s.suspended.clear());
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
        // Deliberately a separate, much slower ticker: the sweep asks the daemon
        // to walk its persistence directory, which is not something to do every
        // two seconds. See `sweep_suspended`.
        let mut sweeper = tokio::time::interval(SUSPENDED_SWEEP);
        sweeper.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        sweeper.tick().await; // the first tick is immediate; skip it

        loop {
            // A cold-start request nudges `scale_signal`, so a scaled-to-zero
            // deployment reacts immediately instead of waiting out the tick.
            let nudged = wait_for_any_scale_signal(&self.registry);

            tokio::select! {
                _ = ticker.tick() => self.reconcile().await,
                _ = nudged => self.reconcile().await,
                _ = sweeper.tick() => self.sweep_suspended().await,
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

/// Why a VM that has not joined the pool yet has not joined the pool yet.
///
/// This one string is the whole diagnosis, and it is not derivable from a status:
/// `Provisioning` means the daemon hasn't finished starting the VM and there is
/// nothing to do but wait, while `Running` plus a failing probe means the *guest*
/// is the problem — the start command, an env var, a binary that exited — and
/// waiting will not fix it. Naming the probe target is what turns "it hangs" into
/// something to go and check.
/// `vm_port` is the deployment's proxied port, which the health check's own `port`
/// overrides when set — the same resolution `health::probe` does, so the message
/// names the port actually dialled rather than the one in the spec.
/// Whether `d` needs nothing this tick beyond a usage sample.
///
/// This is the fast path that makes a fleet of thousands viable: at rest, a
/// sandbox deployment is one healthy VM sitting at its desired size, and
/// deciding that must not cost a daemon round trip. Every condition here is a
/// count or a flag already in memory.
///
/// Call after `prune`, so the backend list reflects the fleet snapshot.
fn at_rest(d: &Arc<Deployment>, fleet: &HashMap<String, SandboxInfo>) -> bool {
    let backends = d.backends();
    if !d.pending().is_empty() || backends.iter().any(|b| b.is_draining()) {
        return false;
    }
    if backends.len() != d.desired_replicas() as usize {
        return false;
    }
    // A TTL past its halfway mark needs a renewal call, which is an await. A
    // backend the fleet snapshot doesn't mention is *not* at rest either: it
    // vanished between the snapshot and now, which `reconcile_one` must see.
    backends.iter().all(|b| {
        fleet.get(&b.sandbox_id).is_some_and(|info| {
            info.uptime_secs < info.ttl_seconds.unwrap_or(d.spec.vm_spec().ttl_seconds) / 2
        })
    })
}

fn boot_stall(info: &SandboxInfo, check: &crate::config::HealthCheck, vm_port: u16) -> String {
    if info.status != heyo_sdk::SandboxStatus::Running {
        return match info.error_message.as_deref() {
            Some(e) => format!("the daemon reports {:?}: {e}", info.status),
            None => format!("the daemon has not reported it Running yet ({:?})", info.status),
        };
    }
    let port = check.port.unwrap_or(vm_port);
    let where_ = match info.guest_ip.as_deref() {
        Some(ip) => format!("{ip}:{port}"),
        None => format!("port {port}"),
    };
    match check.path.as_deref() {
        Some(path) => format!("the guest is up but has not answered GET {path} on {where_}"),
        None => format!("the guest is up but is not accepting TCP connections on {where_}"),
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
            build: None,
            artifact: None,
            update: None,
            auth: None,
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
            build: None,
            artifact: None,
            update: None,
            auth: None,
        }
    }

    /// A registry whose state directory is scratch space. The suspended-VM
    /// bookkeeping persists on every change, so this must not be the CWD — and
    /// must not be shared, since tests run in parallel against the same id.
    fn autoscaler() -> (Autoscaler, Arc<Registry>) {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "app-lb-as-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let registry = Arc::new(Registry::new(dir.join("state.json")));
        registry.upsert(spec());
        // A daemon URL nothing listens on: fine, because the graceful and
        // not-found paths never call it.
        let vms = VmManager::new(Some("http://127.0.0.1:1".into()));
        (
            Autoscaler::new(registry.clone(), vms, Arc::new(Metrics::new())),
            registry,
        )
    }

    /// The record of a suspended sandbox is the *only* record: mvm-ctrl drops a
    /// stopped sandbox from `GET /sandboxes`, so anything lost here is a VM
    /// still holding a disk that nothing will ever resume or reap.
    mod suspended_bookkeeping {
        use super::*;

        #[test]
        fn remembering_is_idempotent() {
            let (a, reg) = autoscaler();
            let d = reg.get("demo").unwrap();

            a.remember_suspended(&d, vec!["sb-1".into(), "sb-2".into()]);
            a.remember_suspended(&d, vec!["sb-2".into(), "sb-3".into()]);

            assert_eq!(d.state().suspended, vec!["sb-1", "sb-2", "sb-3"]);
        }

        /// Claimed before the resume is attempted, so two concurrent scale-ups
        /// cannot both try to start the same VM.
        #[test]
        fn taking_removes_it_oldest_first() {
            let (a, reg) = autoscaler();
            let d = reg.get("demo").unwrap();
            a.remember_suspended(&d, vec!["sb-1".into(), "sb-2".into()]);

            assert_eq!(a.take_suspended(&d).as_deref(), Some("sb-1"));
            assert_eq!(d.state().suspended, vec!["sb-2"]);
            assert_eq!(a.take_suspended(&d).as_deref(), Some("sb-2"));
            assert_eq!(a.take_suspended(&d), None, "an empty record yields nothing");
        }

        #[test]
        fn forgetting_removes_only_the_named_one() {
            let (a, reg) = autoscaler();
            let d = reg.get("demo").unwrap();
            a.remember_suspended(&d, vec!["sb-1".into(), "sb-2".into()]);

            a.forget_suspended(&d, "sb-1");
            assert_eq!(d.state().suspended, vec!["sb-2"]);
            a.forget_suspended(&d, "never-there"); // not an error
            assert_eq!(d.state().suspended, vec!["sb-2"]);
        }

        /// The record survives a restart, or the VM is stranded.
        #[test]
        fn the_record_is_persisted_as_it_changes() {
            let (a, reg) = autoscaler();
            let d = reg.get("demo").unwrap();
            a.remember_suspended(&d, vec!["sb-1".into()]);

            let reloaded = Registry::new(reg.state_dir().parent().unwrap().join("state.json"));
            assert_eq!(reloaded.load().unwrap(), 1);
            assert_eq!(reloaded.get("demo").unwrap().state().suspended, vec!["sb-1"]);
        }

        /// Startup adoption must leave suspended VMs alone.
        ///
        /// This is the bug that would have destroyed every `retain` sandbox on
        /// every restart: a stopped **KVM** sandbox *is* in the daemon's fleet
        /// list (mvm-ctrl reloads persisted KVM sandboxes on every `list`), it
        /// is not routable, and `adopt_existing` kills whatever it cannot adopt.
        #[test]
        fn a_suspended_vm_is_neither_adopted_nor_orphaned() {
            let (_a, reg) = autoscaler();
            let d = reg.get("demo").unwrap();
            d.set_state(crate::deployment::DeploymentState {
                suspended: vec!["sb-1".into()],
            });

            // What the fleet list looks like for a stopped KVM sandbox of ours.
            let mut stopped = info(heyo_sdk::SandboxStatus::Stopped, None);
            stopped.id = "sb-1".into();
            stopped.name = "applb-demo-000000000001".into();

            assert_eq!(vm::owner_of(&stopped.name), Some("demo"), "it is ours");
            assert!(
                vm::routable_addr(&stopped, 8080).is_err(),
                "and not routable, so adoption would otherwise treat it as an orphan",
            );
            assert!(
                d.state().suspended.contains(&stopped.id),
                "the suspended record is what has to save it",
            );
        }

        /// Deregistering must not leave a stopped VM behind. Teardown is the
        /// last moment anything knows the sandbox exists.
        #[tokio::test]
        async fn teardown_clears_the_record() {
            let (a, reg) = autoscaler();
            let d = reg.get("demo").unwrap();
            a.remember_suspended(&d, vec!["sb-1".into()]);

            // The kill calls fail against a dead daemon; teardown logs and
            // carries on, which is what must not leave the record behind.
            a.teardown(&d).await;
            assert!(d.state().suspended.is_empty());
        }
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
        PendingVm::new(id.into())
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

    fn info(status: heyo_sdk::SandboxStatus, guest_ip: Option<&str>) -> SandboxInfo {
        SandboxInfo {
            id: "sb-1".into(),
            name: "applb-demo-000000000001".into(),
            status,
            image: "artifacts".into(),
            region: None,
            start_command: None,
            working_directory: None,
            size_class: None,
            env_vars: None,
            setup_hooks: None,
            uptime_secs: 0,
            ttl_seconds: None,
            is_deployed: true,
            error_message: None,
            status_changed_at: String::new(),
            urls: vec![],
            guest_ip: guest_ip.map(Into::into),
            metadata: None,
        }
    }

    /// `at_rest` decides whether a deployment is skipped for the tick, so every
    /// false positive is a deployment that silently stops being reconciled.
    /// These cases are the ones that would produce one.
    mod at_rest {
        use super::*;

        /// One healthy VM at the desired size, TTL fresh: the shape of an idle
        /// agent sandbox, and the case the fast path exists for.
        fn settled() -> (Arc<Deployment>, HashMap<String, SandboxInfo>) {
            let mut s = spec();
            s.scaling.min_replicas = 1;
            s.scaling.max_replicas = 1;
            let d = Arc::new(Deployment::new(s));
            d.set_backends(vec![Arc::new(VmBackend::new(
                "sb-1".into(),
                "10.0.0.1:8080".parse().unwrap(),
            ))]);
            let mut fleet = HashMap::new();
            fleet.insert("sb-1".to_string(), info(heyo_sdk::SandboxStatus::Running, Some("10.0.0.1")));
            (d, fleet)
        }

        #[test]
        fn an_idle_pool_at_its_desired_size_is_at_rest() {
            let (d, fleet) = settled();
            assert!(at_rest(&d, &fleet));
        }

        #[test]
        fn a_booting_vm_is_work() {
            let (d, fleet) = settled();
            d.set_pending(vec![PendingVm::new("sb-2".into())]);
            assert!(!at_rest(&d, &fleet));
        }

        #[test]
        fn a_draining_vm_is_work() {
            let (d, fleet) = settled();
            d.backends()[0].set_draining();
            assert!(!at_rest(&d, &fleet));
        }

        #[test]
        fn being_below_the_desired_size_is_work() {
            let (d, fleet) = settled();
            d.set_backends(vec![]);
            assert!(!at_rest(&d, &fleet));
        }

        /// Half the TTL gone means a renewal call is due, and that is an await.
        #[test]
        fn a_ttl_past_its_halfway_mark_is_work() {
            let (d, mut fleet) = settled();
            let entry = fleet.get_mut("sb-1").unwrap();
            entry.ttl_seconds = Some(3600);
            entry.uptime_secs = 1800;
            assert!(!at_rest(&d, &fleet));
        }

        /// A backend the daemon no longer reports has just disappeared. Reading
        /// that as "nothing to do" would leave a dead VM in the routing pool.
        #[test]
        fn a_backend_missing_from_the_fleet_is_work() {
            let (d, _) = settled();
            assert!(!at_rest(&d, &HashMap::new()));
        }
    }

    /// The message is the deliverable here: "it hangs on VM creation" has two
    /// completely different causes, and only one of them is worth waiting out.
    #[test]
    fn a_stalled_boot_says_whether_the_daemon_or_the_guest_is_the_problem() {
        let check = crate::config::HealthCheck {
            path: Some("/healthz".into()),
            port: None,
            timeout_secs: 2,
        };

        // Daemon side: nothing to do but wait.
        let waiting = boot_stall(&info(heyo_sdk::SandboxStatus::Provisioning, None), &check, 8080);
        assert!(waiting.contains("daemon"), "{waiting}");
        assert!(!waiting.contains("guest"), "{waiting}");

        // Guest side — the artifacts case: the VM is up, the server inside is
        // not, and no amount of waiting will change that. The probe target has to
        // be named, because that is what somebody goes and checks.
        let stalled = boot_stall(&info(heyo_sdk::SandboxStatus::Running, Some("172.16.0.2")), &check, 8080);
        assert!(stalled.contains("guest is up"), "{stalled}");
        assert!(stalled.contains("/healthz"), "{stalled}");
        // The address actually dialled, so it can be tried by hand from the host.
        assert!(stalled.contains("172.16.0.2:8080"), "{stalled}");

        // A daemon that has an explanation gets to give it.
        let mut failing = info(heyo_sdk::SandboxStatus::Provisioning, None);
        failing.error_message = Some("no space left on device".into());
        assert!(
            boot_stall(&failing, &check, 8080).contains("no space left on device"),
            "the daemon's own reason must survive",
        );

        // A TCP-only check names no path, so it must not claim to have requested one.
        let tcp = crate::config::HealthCheck { path: None, port: Some(9000), timeout_secs: 2 };
        let msg = boot_stall(&info(heyo_sdk::SandboxStatus::Running, Some("172.16.0.2")), &tcp, 8080);
        assert!(msg.contains("TCP") && msg.contains("9000"), "{msg}");
    }

    /// A pending VM per 2s tick per line would bury the log this is meant to make
    /// readable; a pending VM with *no* line is the bug being fixed. So: first
    /// sighting, every transition, and a heartbeat.
    #[test]
    fn boot_progress_is_logged_on_change_and_on_a_heartbeat_but_not_every_tick() {
        use heyo_sdk::SandboxStatus;
        let (a, reg) = autoscaler();
        let d = reg.get("demo").unwrap();
        let p = pending("sb-1");

        // First sighting: nothing has named this sandbox id yet, so it reports.
        let first = a.note_boot_progress(&d, &p, &info(SandboxStatus::Provisioning, None), 2);
        assert_eq!(first.status, Some(SandboxStatus::Provisioning));
        assert_eq!(first.reported_at_secs, 2);

        // Same status a tick later: quiet.
        let quiet = a.note_boot_progress(&d, &first, &info(SandboxStatus::Provisioning, None), 4);
        assert_eq!(quiet.reported_at_secs, 2, "should not have reported again");

        // Transition to Running: reports immediately, without waiting out the
        // heartbeat — this is the moment the diagnosis changes from "the daemon is
        // slow" to "the guest is not answering".
        let moved = a.note_boot_progress(&d, &quiet, &info(SandboxStatus::Running, Some("172.16.0.2")), 6);
        assert_eq!(moved.reported_at_secs, 6);
        assert_eq!(moved.status, Some(SandboxStatus::Running));

        // Unchanged, but the heartbeat is due.
        let beat = a.note_boot_progress(
            &d,
            &moved,
            &info(SandboxStatus::Running, Some("172.16.0.2")),
            6 + BOOT_HEARTBEAT,
        );
        assert_eq!(beat.reported_at_secs, 6 + BOOT_HEARTBEAT);

        // The identity that makes any of this safe: nothing but the bookkeeping
        // changes, so the VM keeps its id and its birthday across every tick.
        assert_eq!(beat.sandbox_id, p.sandbox_id);
        assert_eq!(beat.created_at, p.created_at);
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
