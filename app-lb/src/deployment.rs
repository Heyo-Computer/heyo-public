//! Live deployment state: the VM pool, in-flight accounting, and selection.

use crate::config::DeploymentSpec;
use arc_swap::ArcSwap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A single ready VM.
///
/// `in_flight` is the one number that matters: it picks the upstream *and*
/// drives the autoscaler, which is why we track it ourselves instead of using
/// pingora-load-balancing (whose selection algorithms cannot see it).
#[derive(Debug)]
pub struct VmBackend {
    pub sandbox_id: String,
    pub addr: SocketAddr,
    in_flight: AtomicUsize,
    draining: AtomicBool,
    healthy: AtomicBool,
    last_active: AtomicU64,
}

impl VmBackend {
    pub fn new(sandbox_id: String, addr: SocketAddr) -> Self {
        Self {
            sandbox_id,
            addr,
            in_flight: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            healthy: AtomicBool::new(true),
            last_active: AtomicU64::new(now_secs()),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Eligible to receive a *new* request. A draining VM keeps serving what it
    /// already has, but takes nothing new.
    pub fn is_available(&self) -> bool {
        self.is_healthy() && !self.is_draining()
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

    pub fn acquire(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        self.last_active.store(now_secs(), Ordering::Relaxed);
    }

    /// Saturating so a double-release can never wrap to `usize::MAX` and pin the
    /// deployment at max replicas forever.
    pub fn release(&self) {
        let _ = self
            .in_flight
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
        self.last_active.store(now_secs(), Ordering::Relaxed);
    }
}

/// A VM that has been created but is not yet serving.
#[derive(Debug, Clone)]
pub struct PendingVm {
    pub sandbox_id: String,
    pub created_at: u64,
}

#[derive(Debug)]
pub struct Deployment {
    pub spec: DeploymentSpec,
    /// Ready, routable VMs. Copy-on-write: the autoscaler is the only writer.
    backends: ArcSwap<Vec<Arc<VmBackend>>>,
    /// Booting VMs, not yet routable.
    pending: ArcSwap<Vec<PendingVm>>,
    /// Requests blocked waiting for a VM to boot.
    ///
    /// This is real demand that `in_flight` cannot see: a waiting request has no
    /// backend to hold a slot on. Without counting it, a scaled-to-zero
    /// deployment looks idle *while a request is waiting on it*, so the
    /// autoscaler leaves it at zero and the request blocks until it times out.
    waiters: AtomicUsize,
    /// Wakes cold-start waiters when a VM becomes ready, and nudges the
    /// autoscaler when a request arrives with nowhere to go.
    pub ready_signal: Notify,
    pub scale_signal: Notify,
}

impl Deployment {
    pub fn new(spec: DeploymentSpec) -> Self {
        Self {
            spec,
            backends: ArcSwap::from_pointee(Vec::new()),
            pending: ArcSwap::from_pointee(Vec::new()),
            waiters: AtomicUsize::new(0),
            ready_signal: Notify::new(),
            scale_signal: Notify::new(),
        }
    }

    pub fn waiters(&self) -> usize {
        self.waiters.load(Ordering::Relaxed)
    }

    /// Register a cold-start waiter for as long as the guard lives.
    pub fn track_waiter(self: &Arc<Self>) -> WaiterGuard {
        self.waiters.fetch_add(1, Ordering::Relaxed);
        WaiterGuard {
            deployment: self.clone(),
        }
    }

    pub fn backends(&self) -> Arc<Vec<Arc<VmBackend>>> {
        self.backends.load_full()
    }

    pub fn set_backends(&self, backends: Vec<Arc<VmBackend>>) {
        self.backends.store(Arc::new(backends));
    }

    pub fn pending(&self) -> Arc<Vec<PendingVm>> {
        self.pending.load_full()
    }

    pub fn set_pending(&self, pending: Vec<PendingVm>) {
        self.pending.store(Arc::new(pending));
    }

    /// Total in-flight across the pool, including draining VMs — they still
    /// represent real load.
    pub fn total_in_flight(&self) -> usize {
        self.backends().iter().map(|b| b.in_flight()).sum()
    }

    /// Least-in-flight pick, skipping backends already tried on this request.
    ///
    /// `exclude` is what makes retry work: on `fail_to_connect` we re-enter
    /// `upstream_peer` and must not hand back the VM that just failed.
    pub fn select(&self, exclude: &[SocketAddr]) -> Option<Arc<VmBackend>> {
        self.backends()
            .iter()
            .filter(|b| b.is_available() && !exclude.contains(&b.addr))
            .min_by_key(|b| b.in_flight())
            .cloned()
    }

    /// Whether the autoscaler could still add capacity, i.e. whether a request
    /// arriving now is worth holding for a cold start.
    pub fn can_grow(&self) -> bool {
        let live = self.backends().len() + self.pending().len();
        (live as u32) < self.spec.scaling.max_replicas
    }

    /// Total demand: requests being served plus requests waiting for a VM.
    ///
    /// Waiters must be included or a scaled-to-zero deployment can never wake
    /// up: the request that would justify a VM is precisely the one with no VM
    /// to be counted against.
    pub fn demand(&self) -> usize {
        self.total_in_flight() + self.waiters()
    }

    /// Desired ready-replica count.
    ///
    /// Load term: enough VMs to keep each at `target_concurrency`. The warm pool
    /// is *additive* — spare capacity above current load — then the whole thing
    /// is clamped to [min, max].
    ///
    /// Scale-to-zero requires `warm_pool == 0`: a warm pool exists precisely to
    /// absorb cold starts, so honouring both would be contradictory. It would
    /// also be unreachable in practice — an empty pool reports maximal idleness,
    /// so a warm pool that ever emptied could never refill itself.
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

    /// Seconds since the pool last saw activity. An empty pool is treated as
    /// maximally idle so a scaled-to-zero deployment stays there.
    pub fn idle_for(&self) -> u64 {
        let backends = self.backends();
        if backends.is_empty() {
            return u64::MAX;
        }
        let last = backends.iter().map(|b| b.last_active()).max().unwrap_or(0);
        now_secs().saturating_sub(last)
    }
}

/// Keeps a deployment's waiter count accurate for the life of a cold-start wait.
///
/// RAII because the wait has several exits (VM arrives, timeout, request
/// cancelled); a leaked waiter would keep the deployment scaled up forever.
pub struct WaiterGuard {
    deployment: Arc<Deployment>,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let _ = self
            .deployment
            .waiters
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HealthCheck, RouteRule, ScalingPolicy, VmSpec};
    use heyo_sdk::SandboxDriver;

    fn backend(addr: &str) -> Arc<VmBackend> {
        Arc::new(VmBackend::new(format!("sb-{addr}"), addr.parse().unwrap()))
    }

    fn deployment(scaling: ScalingPolicy) -> Deployment {
        Deployment::new(DeploymentSpec {
            id: "demo".into(),
            routes: vec![RouteRule {
                host: Some("demo.local".into()),
                path_prefix: None,
            }],
            vm: VmSpec {
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
            },
            scaling,
            health: HealthCheck::default(),
        })
    }

    #[test]
    fn selects_least_in_flight() {
        let d = deployment(ScalingPolicy::default());
        let a = backend("10.0.0.1:80");
        let b = backend("10.0.0.2:80");
        a.acquire();
        a.acquire();
        b.acquire();
        d.set_backends(vec![a, b.clone()]);
        assert_eq!(d.select(&[]).unwrap().addr, b.addr);
    }

    #[test]
    fn select_skips_excluded_draining_and_unhealthy() {
        let d = deployment(ScalingPolicy::default());
        let a = backend("10.0.0.1:80");
        let b = backend("10.0.0.2:80");
        let c = backend("10.0.0.3:80");
        d.set_backends(vec![a.clone(), b.clone(), c.clone()]);

        // Excluding the first (e.g. it just failed to connect) moves on.
        assert_eq!(d.select(&[a.addr]).unwrap().addr, b.addr);

        b.set_draining();
        c.set_healthy(false);
        assert!(d.select(&[a.addr]).is_none());
        assert_eq!(d.select(&[]).unwrap().addr, a.addr);
    }

    #[test]
    fn select_on_empty_pool_is_none_not_panic() {
        let d = deployment(ScalingPolicy::default());
        assert!(d.select(&[]).is_none());
    }

    #[test]
    fn release_saturates_at_zero() {
        let b = backend("10.0.0.1:80");
        b.release();
        b.release();
        assert_eq!(b.in_flight(), 0);
        b.acquire();
        assert_eq!(b.in_flight(), 1);
    }

    #[test]
    fn acquire_release_round_trips() {
        let b = backend("10.0.0.1:80");
        for _ in 0..5 {
            b.acquire();
        }
        assert_eq!(b.in_flight(), 5);
        for _ in 0..5 {
            b.release();
        }
        assert_eq!(b.in_flight(), 0);
    }

    #[test]
    fn desired_scales_with_load() {
        let d = deployment(ScalingPolicy {
            min_replicas: 1,
            max_replicas: 10,
            target_concurrency: 10,
            ..Default::default()
        });
        let a = backend("10.0.0.1:80");
        d.set_backends(vec![a.clone()]);

        // 25 in-flight / target 10 -> ceil = 3.
        for _ in 0..25 {
            a.acquire();
        }
        assert_eq!(d.desired_replicas(), 3);
    }

    #[test]
    fn desired_is_clamped_to_min_and_max() {
        let d = deployment(ScalingPolicy {
            min_replicas: 2,
            max_replicas: 4,
            target_concurrency: 1,
            ..Default::default()
        });
        let a = backend("10.0.0.1:80");
        d.set_backends(vec![a.clone()]);
        // Idle, but min_replicas holds the floor.
        assert_eq!(d.desired_replicas(), 2);

        // Far past max, but the ceiling holds.
        for _ in 0..100 {
            a.acquire();
        }
        assert_eq!(d.desired_replicas(), 4);
    }

    #[test]
    fn warm_pool_adds_headroom_above_load() {
        let d = deployment(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 10,
            warm_pool: 2,
            target_concurrency: 10,
            scale_to_zero_after_secs: 0,
            ..Default::default()
        });
        let a = backend("10.0.0.1:80");
        d.set_backends(vec![a.clone()]);

        // Idle: just the warm pool. Scale-to-zero must not eat it, even though
        // min_replicas is 0 and the pool is idle.
        assert_eq!(d.desired_replicas(), 2);

        // 10 in-flight needs 1, plus 2 warm spares.
        for _ in 0..10 {
            a.acquire();
        }
        assert_eq!(d.desired_replicas(), 3);
    }

    /// Regression: an empty pool reports `idle_for() == u64::MAX`, so a warm
    /// pool starting from zero used to be zeroed out by scale-to-zero and could
    /// never boot its first VM.
    #[test]
    fn warm_pool_bootstraps_from_an_empty_pool() {
        let d = deployment(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 3,
            warm_pool: 1,
            target_concurrency: 2,
            scale_to_zero_after_secs: 300,
            ..Default::default()
        });
        assert_eq!(d.desired_replicas(), 1);
    }

    #[test]
    fn scales_to_zero_only_when_idle_and_min_is_zero() {
        let d = deployment(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 4,
            warm_pool: 0,
            target_concurrency: 10,
            scale_to_zero_after_secs: 0, // idle immediately
            ..Default::default()
        });
        let a = backend("10.0.0.1:80");
        d.set_backends(vec![a.clone()]);
        assert_eq!(d.desired_replicas(), 0);

        // In-flight work blocks scale-to-zero even when "idle" by time.
        a.acquire();
        assert_eq!(d.desired_replicas(), 1);
    }

    #[test]
    fn empty_pool_stays_at_zero_when_allowed() {
        let d = deployment(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 4,
            scale_to_zero_after_secs: 300,
            ..Default::default()
        });
        // No backends => idle_for is MAX => desired 0, and no panic on max().
        assert_eq!(d.desired_replicas(), 0);
    }

    /// Regression: a request waiting on a scaled-to-zero deployment holds no
    /// in-flight slot, so the pool looked idle and stayed at zero — the waiting
    /// request then blocked until its cold-start timeout and 500'd.
    #[test]
    fn a_cold_start_waiter_is_demand_and_forces_scale_up() {
        let d = Arc::new(deployment(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 3,
            warm_pool: 0,
            target_concurrency: 2,
            scale_to_zero_after_secs: 0, // maximally eager to scale to zero
            ..Default::default()
        }));
        assert_eq!(d.desired_replicas(), 0);

        let guard = d.track_waiter();
        assert_eq!(d.demand(), 1);
        assert_eq!(
            d.desired_replicas(),
            1,
            "a waiting request must justify booting a VM",
        );

        drop(guard);
        assert_eq!(d.demand(), 0);
        assert_eq!(d.desired_replicas(), 0, "waiter must not leak after drop");
    }

    #[test]
    fn waiters_and_in_flight_both_count_toward_demand() {
        let d = Arc::new(deployment(ScalingPolicy {
            min_replicas: 0,
            max_replicas: 10,
            target_concurrency: 2,
            scale_to_zero_after_secs: 0,
            ..Default::default()
        }));
        let a = backend("10.0.0.1:80");
        d.set_backends(vec![a.clone()]);
        for _ in 0..4 {
            a.acquire();
        }
        let _g1 = d.track_waiter();
        let _g2 = d.track_waiter();
        // 4 in-flight + 2 waiting = 6, over target 2 => 3 replicas.
        assert_eq!(d.demand(), 6);
        assert_eq!(d.desired_replicas(), 3);
    }

    #[test]
    fn concurrent_waiters_are_tracked_independently() {
        let d = Arc::new(deployment(ScalingPolicy::default()));
        let guards: Vec<_> = (0..5).map(|_| d.track_waiter()).collect();
        assert_eq!(d.waiters(), 5);
        drop(guards);
        assert_eq!(d.waiters(), 0);
    }

    #[test]
    fn can_grow_counts_pending_against_max() {
        let d = deployment(ScalingPolicy {
            max_replicas: 2,
            ..Default::default()
        });
        assert!(d.can_grow());
        d.set_backends(vec![backend("10.0.0.1:80")]);
        assert!(d.can_grow());
        // A booting VM already counts, so we don't over-provision.
        d.set_pending(vec![PendingVm {
            sandbox_id: "sb-2".into(),
            created_at: now_secs(),
        }]);
        assert!(!d.can_grow());
    }
}
