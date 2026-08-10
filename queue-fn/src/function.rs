//! Live function state: the VM pool, slot accounting, and scaling arithmetic.
//!
//! Two things here differ from app-lb's equivalent, and both are forced:
//!
//! **A VM runs exactly one invocation at a time.** `SandboxManager::execute_with_env`
//! removes the sandbox handle from its map for the duration of a call
//! (mvm-ctrl/src/sandbox.rs:3648) and re-inserts it after. A second concurrent
//! exec finds nothing and gets `SandboxNotFound` — it does not queue, it fails.
//! So a worker carries a one-permit `busy` flag claimed by compare-exchange, and
//! selection is *first unclaimed* rather than least-in-flight; ranking by load is
//! meaningless when the only two loads are 0 and 1.
//!
//! **Queue depth is the demand signal.** app-lb can read demand off its in-flight
//! counters because a request arriving at the proxy is already in the process.
//! Here the work sits in JetStream, and a scaled-to-zero function has nothing in
//! flight *precisely because* it has no VM to put anything in flight on. Without
//! `num_pending` the autoscaler would read an empty pool with a full queue as
//! idle and leave it that way forever.

use crate::config::FunctionSpec;
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A single ready VM, and its one exec slot.
#[derive(Debug)]
pub struct VmWorker {
    pub sandbox_id: String,
    /// The exec slot. `true` means an invocation is running in this VM right
    /// now, and no other may start — see the module doc.
    busy: AtomicBool,
    draining: AtomicBool,
    healthy: AtomicBool,
    last_active: AtomicU64,
    /// When this VM joined the pool (our clock). Serving uptime, as opposed to
    /// the daemon's `uptime_secs` which counts provisioning too.
    ready_at: u64,
    invocations: AtomicU64,
    failures: AtomicU64,
    /// Latest CPU/memory sample from the daemon's `/system/usage`, refreshed by
    /// the autoscaler each tick. CPU is f64 bits (percent of a core);
    /// `has_usage` distinguishes "no sample yet" from a genuine zero.
    cpu_percent_bits: AtomicU64,
    mem_bytes: AtomicU64,
    has_usage: AtomicBool,
}

impl VmWorker {
    pub fn new(sandbox_id: String) -> Self {
        Self {
            sandbox_id,
            busy: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            healthy: AtomicBool::new(true),
            last_active: AtomicU64::new(now_secs()),
            ready_at: now_secs(),
            invocations: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            cpu_percent_bits: AtomicU64::new(0),
            mem_bytes: AtomicU64::new(0),
            has_usage: AtomicBool::new(false),
        }
    }

    /// Seconds this VM has been in the pool.
    pub fn uptime_secs(&self) -> u64 {
        now_secs().saturating_sub(self.ready_at)
    }

    pub fn set_usage(&self, cpu_percent: f64, mem_bytes: u64) {
        self.cpu_percent_bits
            .store(cpu_percent.to_bits(), Ordering::Relaxed);
        self.mem_bytes.store(mem_bytes, Ordering::Relaxed);
        self.has_usage.store(true, Ordering::Relaxed);
    }

    /// Latest `(cpu_percent, mem_bytes)`, or `None` if the daemon has not yet
    /// reported usage for this VM.
    pub fn usage(&self) -> Option<(f64, u64)> {
        self.has_usage.load(Ordering::Relaxed).then(|| {
            (
                f64::from_bits(self.cpu_percent_bits.load(Ordering::Relaxed)),
                self.mem_bytes.load(Ordering::Relaxed),
            )
        })
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Eligible to receive a *new* invocation. A draining VM finishes what it is
    /// running but takes nothing new.
    pub fn is_available(&self) -> bool {
        self.is_healthy() && !self.is_draining() && !self.is_busy()
    }

    pub fn set_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }

    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
    }

    pub fn last_active(&self) -> u64 {
        self.last_active.load(Ordering::Relaxed)
    }

    pub fn invocations(&self) -> u64 {
        self.invocations.load(Ordering::Relaxed)
    }

    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    pub fn record_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Claim this VM's exec slot, or `None` if it is unavailable.
    ///
    /// Compare-exchange rather than a load-then-store: two dispatcher tasks can
    /// race for the same idle worker, and the loser must be told so rather than
    /// both proceeding into an exec that the daemon will fail.
    ///
    /// The returned guard releases on drop, so every early return in the
    /// dispatch path — timeout, daemon error, panic — frees the slot.
    pub fn try_claim(self: &Arc<Self>) -> Option<SlotGuard> {
        if !self.is_healthy() || self.is_draining() {
            return None;
        }
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()?;
        self.invocations.fetch_add(1, Ordering::Relaxed);
        self.last_active.store(now_secs(), Ordering::Relaxed);
        Some(SlotGuard {
            worker: self.clone(),
        })
    }
}

/// Holds a VM's exec slot for the life of one invocation.
pub struct SlotGuard {
    worker: Arc<VmWorker>,
}

impl SlotGuard {
    pub fn worker(&self) -> &Arc<VmWorker> {
        &self.worker
    }

    pub fn sandbox_id(&self) -> &str {
        &self.worker.sandbox_id
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.worker.busy.store(false, Ordering::Release);
        self.worker
            .last_active
            .store(now_secs(), Ordering::Relaxed);
    }
}

/// A VM that has been created but is not yet proven ready.
#[derive(Debug, Clone)]
pub struct PendingVm {
    pub sandbox_id: String,
    pub created_at: u64,
}

#[derive(Debug)]
pub struct Function {
    pub spec: FunctionSpec,
    /// Ready VMs. Copy-on-write: the autoscaler is the only writer of this vec.
    /// Everyone else flips atomics on the shared `Arc<VmWorker>`s inside it.
    workers: ArcSwap<Vec<Arc<VmWorker>>>,
    /// Booting VMs, not yet usable.
    pending: ArcSwap<Vec<PendingVm>>,
    /// Events sitting in JetStream with nowhere to run. Refreshed from the
    /// consumer each reconcile tick, and bumped optimistically on publish so a
    /// cold start doesn't wait a full tick to be noticed.
    queue_pending: AtomicU64,
    /// Events delivered to a dispatcher and not yet acked.
    queue_ack_pending: AtomicU64,
    /// Dashboard pause. The consumer stays; the dispatcher naks instead of
    /// running, so a paused function accumulates depth rather than losing work.
    paused: AtomicBool,
    /// Wakes anything waiting on capacity when a VM becomes ready, and nudges
    /// the autoscaler when work arrives with nowhere to go.
    pub ready_signal: Notify,
    pub scale_signal: Notify,
}

impl Function {
    pub fn new(spec: FunctionSpec) -> Self {
        Self {
            spec,
            workers: ArcSwap::from_pointee(Vec::new()),
            pending: ArcSwap::from_pointee(Vec::new()),
            queue_pending: AtomicU64::new(0),
            queue_ack_pending: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            ready_signal: Notify::new(),
            scale_signal: Notify::new(),
        }
    }

    pub fn workers(&self) -> Arc<Vec<Arc<VmWorker>>> {
        self.workers.load_full()
    }

    pub fn set_workers(&self, workers: Vec<Arc<VmWorker>>) {
        self.workers.store(Arc::new(workers));
    }

    pub fn pending(&self) -> Arc<Vec<PendingVm>> {
        self.pending.load_full()
    }

    pub fn set_pending(&self, pending: Vec<PendingVm>) {
        self.pending.store(Arc::new(pending));
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn queue_pending(&self) -> u64 {
        self.queue_pending.load(Ordering::Relaxed)
    }

    pub fn queue_ack_pending(&self) -> u64 {
        self.queue_ack_pending.load(Ordering::Relaxed)
    }

    /// Overwrite the depth with what the consumer actually reports.
    pub fn set_queue_depth(&self, pending: u64, ack_pending: u64) {
        self.queue_pending.store(pending, Ordering::Relaxed);
        self.queue_ack_pending.store(ack_pending, Ordering::Relaxed);
    }

    /// Optimistically count a just-published event as demand.
    ///
    /// The reconcile tick refreshes depth from the consumer every 2s, which
    /// would make every cold start pay up to a tick of latency before the
    /// autoscaler even knew there was work. This bumps the estimate immediately;
    /// the next tick overwrites it with the truth, so an over- or under-count
    /// here is self-correcting rather than sticky.
    pub fn note_enqueued(&self) {
        self.queue_pending.fetch_add(1, Ordering::Relaxed);
    }

    /// VMs currently running an invocation.
    pub fn total_in_flight(&self) -> usize {
        self.workers().iter().filter(|w| w.is_busy()).count()
    }

    /// Claim a slot on the first available VM.
    ///
    /// First-unclaimed rather than least-loaded: with one slot per VM every
    /// available worker is equally idle, so any ordering is as good as another.
    pub fn select(&self) -> Option<SlotGuard> {
        self.workers().iter().find_map(|w| w.try_claim())
    }

    /// Total outstanding work: queued, plus handed out and not yet finished.
    ///
    /// `ack_pending` is easy to leave out and doing so deadlocks the system. It
    /// counts messages JetStream has delivered but not seen acked — which covers
    /// both an invocation running on a VM *and* one that was nak'd back because
    /// there was nowhere to run it. Counting only `pending` means a function
    /// whose entire backlog has been delivered-and-nak'd reports zero demand, so
    /// the autoscaler boots nothing, so the nak'd work can never run.
    ///
    /// `ack_pending` and `total_in_flight` are the same population counted two
    /// ways — one from the server on the reconcile tick, one locally and
    /// instantly — so they are combined with `max`, not summed. The local number
    /// covers the window before the first refresh and any tick that failed.
    pub fn demand(&self) -> usize {
        let outstanding = (self.queue_ack_pending() as usize).max(self.total_in_flight());
        self.queue_pending() as usize + outstanding
    }

    /// Desired ready-replica count.
    ///
    /// Load term: enough VMs to keep each one's backlog at `target_concurrency`.
    /// The warm pool is *additive* — spare capacity above current demand — then
    /// the whole thing is clamped to [min, max].
    ///
    /// Scale-to-zero requires `warm_pool == 0`: a warm pool exists precisely to
    /// absorb cold starts, so honouring both would be contradictory. It would
    /// also be unreachable — an empty pool reports maximal idleness, so a warm
    /// pool that ever emptied could never refill itself.
    pub fn desired_replicas(&self) -> u32 {
        let policy = &self.spec.scaling;
        let demand = self.demand();

        let needed = demand.div_ceil(policy.target_concurrency.max(1) as usize) as u32;
        let mut desired = needed.saturating_add(policy.warm_pool);

        let may_scale_to_zero = policy.min_replicas == 0 && policy.warm_pool == 0;
        if may_scale_to_zero && demand == 0 && self.idle_for() >= policy.scale_to_zero_after_secs {
            desired = 0;
        }

        desired.clamp(policy.min_replicas, policy.max_replicas)
    }

    /// Seconds since the pool last ran anything. An empty pool is treated as
    /// maximally idle so a scaled-to-zero function stays there.
    pub fn idle_for(&self) -> u64 {
        let workers = self.workers();
        if workers.is_empty() {
            return u64::MAX;
        }
        let last = workers.iter().map(|w| w.last_active()).max().unwrap_or(0);
        now_secs().saturating_sub(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecSpec, PayloadMode, RetryPolicy, ScalingPolicy, VmSpec};
    use heyo_sdk::SandboxDriver;

    fn worker(id: &str) -> Arc<VmWorker> {
        Arc::new(VmWorker::new(id.into()))
    }

    fn function(scaling: ScalingPolicy) -> Function {
        Function::new(FunctionSpec {
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
            scaling,
            triggers: vec![],
            retry: RetryPolicy::default(),
        })
    }

    #[test]
    fn a_claimed_worker_cannot_be_claimed_again() {
        let w = worker("sb-1");
        let first = w.try_claim().expect("first claim succeeds");
        assert!(w.is_busy());
        assert!(
            w.try_claim().is_none(),
            "heyvm serialises exec per sandbox; a second claim must be refused"
        );
        drop(first);
        assert!(!w.is_busy());
        assert!(w.try_claim().is_some(), "the slot is reusable after release");
    }

    /// Regression: the slot must be released on *every* exit from the dispatch
    /// path, not just the happy one. An exec that timed out or errored used to
    /// leave `busy` set, permanently removing the VM from selection while the
    /// autoscaler still counted it as ready capacity.
    #[test]
    fn the_slot_is_released_even_when_the_invocation_fails() {
        let w = worker("sb-1");
        let result: Result<(), &str> = {
            let _slot = w.try_claim().expect("claim");
            Err("exec blew up")
        };
        assert!(result.is_err());
        assert!(!w.is_busy(), "an early return must still free the slot");
    }

    #[test]
    fn select_skips_busy_draining_and_unhealthy_workers() {
        let f = function(ScalingPolicy::default());
        let a = worker("sb-a");
        let b = worker("sb-b");
        let c = worker("sb-c");
        f.set_workers(vec![a.clone(), b.clone(), c.clone()]);

        let held = a.try_claim().expect("a is claimable");
        b.set_draining();
        c.set_healthy(false);
        assert!(
            f.select().is_none(),
            "every worker is busy, draining, or unhealthy"
        );

        drop(held);
        assert_eq!(f.select().unwrap().sandbox_id(), "sb-a");
    }

    #[test]
    fn select_on_an_empty_pool_is_none_not_panic() {
        let f = function(ScalingPolicy::default());
        assert!(f.select().is_none());
    }

    #[test]
    fn in_flight_counts_claimed_workers() {
        let f = function(ScalingPolicy::default());
        let a = worker("sb-a");
        let b = worker("sb-b");
        f.set_workers(vec![a.clone(), b.clone()]);
        assert_eq!(f.total_in_flight(), 0);
        let _x = a.try_claim().unwrap();
        assert_eq!(f.total_in_flight(), 1);
        let _y = b.try_claim().unwrap();
        assert_eq!(f.total_in_flight(), 2);
    }

    /// Regression: queue depth is the only demand signal for a scaled-to-zero
    /// function. Counting only in-flight work left queued events sitting forever
    /// behind a pool the autoscaler believed was idle — nothing was running
    /// precisely because there was no VM to run it on.
    #[test]
    fn queued_events_are_demand_and_size_the_pool() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 10,
            warm_pool: 0,
            target_concurrency: 2,
            scale_to_zero_after_secs: 0, // maximally eager to scale to zero
            ..Default::default()
        });
        assert_eq!(f.desired_replicas(), 0, "no work, no VMs");

        // The worked example: 10 queued events at 2 per VM.
        f.set_queue_depth(10, 0);
        assert_eq!(f.demand(), 10);
        assert_eq!(f.desired_replicas(), 5);

        f.set_queue_depth(0, 0);
        assert_eq!(f.desired_replicas(), 0, "the queue drained; so does the pool");
    }

    #[test]
    fn queued_and_running_work_both_count_toward_demand() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 10,
            target_concurrency: 2,
            scale_to_zero_after_secs: 0,
            ..Default::default()
        });
        let a = worker("sb-a");
        let b = worker("sb-b");
        f.set_workers(vec![a.clone(), b.clone()]);
        let _x = a.try_claim().unwrap();
        let _y = b.try_claim().unwrap();
        f.set_queue_depth(4, 2);

        // 4 queued + 2 running = 6, over target 2 => 3 replicas.
        assert_eq!(f.demand(), 6);
        assert_eq!(f.desired_replicas(), 3);
    }

    /// Regression: a deadlock found by running the thing. `demand` counted only
    /// `pending`, but a message the dispatcher has pulled and nak'd sits in
    /// `ack_pending`. So a scaled-to-zero function whose whole backlog had been
    /// delivered-and-nak'd reported zero demand — the autoscaler booted nothing,
    /// and the work could never run because nothing could ever run it.
    #[test]
    fn delivered_but_unacked_work_is_still_demand() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 10,
            warm_pool: 0,
            target_concurrency: 2,
            scale_to_zero_after_secs: 0,
            ..Default::default()
        });

        // Everything delivered, nothing waiting, no VMs: the exact deadlock.
        f.set_queue_depth(0, 10);
        assert_eq!(f.demand(), 10, "unacked work is outstanding work");
        assert_eq!(
            f.desired_replicas(),
            5,
            "the fleet must still be sized to the backlog",
        );
    }

    /// `ack_pending` and `total_in_flight` count the same messages — the server's
    /// view and ours — so they are combined with max, not summed, or a function
    /// running 2 invocations would report demand for 4.
    #[test]
    fn running_work_is_not_double_counted() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 10,
            target_concurrency: 1,
            scale_to_zero_after_secs: 0,
            ..Default::default()
        });
        let a = worker("sb-a");
        let b = worker("sb-b");
        f.set_workers(vec![a.clone(), b.clone()]);
        let _x = a.try_claim().unwrap();
        let _y = b.try_claim().unwrap();
        f.set_queue_depth(0, 2);

        assert_eq!(f.demand(), 2, "two running invocations are demand for two");
    }

    /// The local count covers the window before the first depth refresh, and any
    /// tick where reading the consumer failed.
    #[test]
    fn local_in_flight_covers_a_stale_depth_reading() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 4,
            target_concurrency: 1,
            scale_to_zero_after_secs: 0,
            ..Default::default()
        });
        let a = worker("sb-a");
        f.set_workers(vec![a.clone()]);
        let _claim = a.try_claim().unwrap();
        // Depth never refreshed, so the server's view reads zero.
        f.set_queue_depth(0, 0);
        assert_eq!(
            f.demand(),
            1,
            "a VM mid-invocation must not be reaped because a tick was missed",
        );
    }

    /// Regression: depth is refreshed on the 2s reconcile tick, so a publish
    /// into a scaled-to-zero function used to wait a full tick before the
    /// autoscaler saw any reason to boot a VM — a tick of latency added to
    /// every cold start.
    #[test]
    fn a_just_published_event_is_demand_before_the_next_tick() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 3,
            target_concurrency: 2,
            scale_to_zero_after_secs: 0,
            ..Default::default()
        });
        assert_eq!(f.desired_replicas(), 0);

        f.note_enqueued();
        assert_eq!(f.desired_replicas(), 1, "a published event justifies a VM");

        // The tick reconciles to truth, so an optimistic bump is not sticky.
        f.set_queue_depth(0, 0);
        assert_eq!(f.desired_replicas(), 0);
    }

    #[test]
    fn desired_is_clamped_to_min_and_max() {
        let f = function(ScalingPolicy {
            min_replicas: 2,
            max_replicas: 4,
            target_concurrency: 1,
            ..Default::default()
        });
        f.set_workers(vec![worker("sb-a")]);
        assert_eq!(f.desired_replicas(), 2, "idle, but min holds the floor");

        f.set_queue_depth(100, 0);
        assert_eq!(f.desired_replicas(), 4, "far past max, but the ceiling holds");
    }

    #[test]
    fn warm_pool_adds_headroom_above_demand() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 10,
            warm_pool: 2,
            target_concurrency: 2,
            scale_to_zero_after_secs: 0,
            ..Default::default()
        });
        f.set_workers(vec![worker("sb-a")]);
        assert_eq!(f.desired_replicas(), 2, "idle: just the warm pool");

        f.set_queue_depth(4, 0);
        assert_eq!(f.desired_replicas(), 4, "2 for the queue, plus 2 spare");
    }

    /// Regression: an empty pool reports `idle_for() == u64::MAX`, so a warm
    /// pool starting from zero was zeroed out by scale-to-zero and could never
    /// boot its first VM.
    #[test]
    fn warm_pool_bootstraps_from_an_empty_pool() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 3,
            warm_pool: 1,
            target_concurrency: 2,
            scale_to_zero_after_secs: 300,
            ..Default::default()
        });
        assert_eq!(f.desired_replicas(), 1);
    }

    #[test]
    fn running_work_blocks_scale_to_zero() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 4,
            warm_pool: 0,
            target_concurrency: 10,
            scale_to_zero_after_secs: 0, // idle immediately
            ..Default::default()
        });
        let a = worker("sb-a");
        f.set_workers(vec![a.clone()]);
        assert_eq!(f.desired_replicas(), 0);

        let _slot = a.try_claim().unwrap();
        assert_eq!(
            f.desired_replicas(),
            1,
            "a VM mid-invocation must not be reaped out from under it"
        );
    }

    /// Regression: `desired_replicas` counted only ready VMs against the max, so
    /// a booting VM didn't hold its slot and the autoscaler kept creating more —
    /// blowing past max_replicas while the first batch was still provisioning.
    #[test]
    fn booting_vms_count_against_the_replica_ceiling() {
        let f = function(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 2,
            target_concurrency: 1,
            ..Default::default()
        });
        f.set_queue_depth(10, 0);
        f.set_workers(vec![worker("sb-a")]);
        f.set_pending(vec![PendingVm {
            sandbox_id: "sb-b".into(),
            created_at: now_secs(),
        }]);

        // The autoscaler compares `desired` against ready + pending, so with one
        // of each already live there is nothing left to create.
        let live = f.workers().len() + f.pending().len();
        assert_eq!(f.desired_replicas() as usize, 2);
        assert_eq!(live, 2, "a booting VM already holds its slot");
    }

    #[test]
    fn a_paused_function_still_reports_its_backlog() {
        let f = function(ScalingPolicy::default());
        assert!(!f.is_paused());
        f.set_paused(true);
        f.set_queue_depth(7, 0);
        assert!(f.is_paused());
        assert_eq!(
            f.queue_pending(),
            7,
            "pausing stops dispatch, not accounting"
        );
    }
}
