//! In-process metrics.
//!
//! queue-fn's vantage point is the queue and the exec, so that is what it can
//! honestly measure: how long work waits before a VM picks it up, how long the
//! command then takes, how long a cold VM takes to become usable, and how the
//! fleet grows and shrinks. Guest-internal telemetry is not available — the
//! daemon reports host-process CPU and RSS per sandbox and nothing from inside —
//! so "resource usage" here is the host's view, never fabricated guest counters.
//!
//! Everything on the hot path is a relaxed atomic; counters and histogram
//! buckets are additive, so recording never takes a lock. Reads are for the
//! dashboard poll and tolerate a snapshot torn across fields — it is a
//! monitoring view, not an invariant.
//!
//! **Counters are monotonic; rates are the dashboard's job.** Storing a rate
//! here would bake in a window, and the dashboard already keeps a history buffer
//! to diff against. The one exception is `QueueGauge`, which is a genuine gauge
//! (a backlog has a current value, not a total) and is *set*, not added, from
//! each reconcile tick.

use arc_swap::ArcSwap;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Exec duration bucket bounds in milliseconds. Dense across the 100ms–5s band
/// where a function body actually lives, with a tail to 30s — the guest-side
/// ceiling (mvm-ctrl/src/driver/firecracker.rs:1716), past which the daemon
/// errors rather than returning a result at all.
pub const EXEC_BOUNDS_MS: &[u64] = &[
    10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 20000, 30000,
];

/// Queue wait bucket bounds in milliseconds: enqueue to dispatch. This is the
/// number that separates "the fleet is undersized" from "the function is slow" —
/// a fast function with a five-minute queue wait is a scaling problem.
pub const QUEUE_WAIT_BOUNDS_MS: &[u64] =
    &[10, 50, 100, 500, 1000, 5000, 15000, 60000, 300000];

/// Cold-start bucket bounds in seconds. Boot is measured in whole seconds and a
/// Firecracker VM is typically usable in ~1–2s, so the low end is dense; the
/// tail runs out to the default cold-start budget.
pub const COLD_START_BOUNDS_S: &[u64] = &[1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 120];

/// A fixed-bucket cumulative histogram over `u64` samples in some base unit.
///
/// Buckets are `bounds.len() + 1` wide — one per upper bound plus an overflow
/// bucket for samples past the last bound. `record` is a couple of relaxed adds;
/// percentiles are derived at snapshot time from the bucket counts.
#[derive(Debug)]
pub struct Histogram {
    bounds: &'static [u64],
    buckets: Box<[AtomicU64]>,
    count: AtomicU64,
    sum: AtomicU64,
}

impl Histogram {
    pub fn new(bounds: &'static [u64]) -> Self {
        Self {
            bounds,
            buckets: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
        }
    }

    pub fn record(&self, value: u64) {
        // First bucket whose bound the sample fits under; the overflow bucket
        // (index == bounds.len()) catches everything larger.
        let idx = self
            .bounds
            .iter()
            .position(|&b| value <= b)
            .unwrap_or(self.bounds.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
        HistogramSnapshot::from_parts(
            self.bounds,
            counts,
            self.count.load(Ordering::Relaxed),
            self.sum.load(Ordering::Relaxed),
        )
    }
}

/// One `le` (less-than-or-equal) bucket. `le: None` is the overflow bucket —
/// samples above the last finite bound.
#[derive(Debug, Clone, Serialize)]
pub struct Bucket {
    pub le: Option<u64>,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub sum: u64,
    pub mean: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub buckets: Vec<Bucket>,
    /// Carried so snapshots can be merged (global = sum of per-function).
    #[serde(skip)]
    bounds: &'static [u64],
}

impl HistogramSnapshot {
    fn from_parts(bounds: &'static [u64], counts: Vec<u64>, count: u64, sum: u64) -> Self {
        let mean = if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        };
        let buckets = bounds
            .iter()
            .map(|&b| Some(b))
            .chain(std::iter::once(None))
            .zip(counts.iter().copied())
            .map(|(le, count)| Bucket { le, count })
            .collect::<Vec<_>>();
        Self {
            count,
            sum,
            mean,
            p50: percentile(bounds, &counts, count, 0.50),
            p90: percentile(bounds, &counts, count, 0.90),
            p99: percentile(bounds, &counts, count, 0.99),
            buckets,
            bounds,
        }
    }

    pub fn empty(bounds: &'static [u64]) -> Self {
        Self::from_parts(bounds, vec![0; bounds.len() + 1], 0, 0)
    }

    /// Fold another histogram over the same bounds into this one.
    ///
    /// Buckets are additive across functions, so the global view is an exact
    /// merge — not an average of percentiles, which would be meaningless.
    pub fn merge(&mut self, other: &HistogramSnapshot) {
        debug_assert_eq!(self.bounds, other.bounds, "cannot merge unlike histograms");
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .zip(&other.buckets)
            .map(|(a, b)| a.count + b.count)
            .collect();
        *self = Self::from_parts(
            self.bounds,
            counts,
            self.count + other.count,
            self.sum + other.sum,
        );
    }
}

/// Rank-based percentile over cumulative buckets, linearly interpolated within
/// the containing bucket. Approximate by construction — the sample's exact value
/// is gone, only its bucket survives — which is the accepted trade for O(1)
/// recording. A sample in the overflow bucket reports the last finite bound as a
/// floor rather than an unbounded number.
fn percentile(bounds: &[u64], counts: &[u64], total: u64, q: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let target = q * total as f64;
    let mut cumulative = 0u64;
    let mut lower = 0f64;
    for (i, &c) in counts.iter().enumerate() {
        let prev_cumulative = cumulative;
        cumulative += c;
        if (cumulative as f64) >= target && c > 0 {
            let upper = bounds.get(i).map(|&b| b as f64).unwrap_or(lower);
            if upper <= lower {
                return upper;
            }
            // How far into this bucket the target rank falls.
            let into = (target - prev_cumulative as f64) / c as f64;
            return lower + into * (upper - lower);
        }
        lower = bounds.get(i).map(|&b| b as f64).unwrap_or(lower);
    }
    lower
}

/// Invocation outcomes. Monotonic since process start.
#[derive(Debug, Default)]
struct InvocationCounts {
    total: AtomicU64,
    /// Ran and exited 0.
    success: AtomicU64,
    /// Ran and exited non-zero. A real result, just not a happy one.
    failed: AtomicU64,
    /// Did not finish inside the function's budget.
    timeout: AtomicU64,
    /// Never produced an exit code at all — the daemon or the VM failed.
    /// Distinct from `failed`, because only this one implicates the platform.
    errors: AtomicU64,
    /// Redeliveries. Not a separate invocation; a repeat of one.
    retries: AtomicU64,
    /// Gave up after `max_attempts` and parked the event.
    dlq: AtomicU64,
    /// Refused before running: oversized payload, or an unparseable envelope.
    rejected: AtomicU64,
}

impl InvocationCounts {
    fn snapshot(&self) -> InvocationSnapshot {
        InvocationSnapshot {
            total: self.total.load(Ordering::Relaxed),
            success: self.success.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            timeout: self.timeout.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            dlq: self.dlq.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct InvocationSnapshot {
    pub total: u64,
    pub success: u64,
    pub failed: u64,
    pub timeout: u64,
    pub errors: u64,
    pub retries: u64,
    pub dlq: u64,
    pub rejected: u64,
}

impl InvocationSnapshot {
    fn merge(&mut self, o: &InvocationSnapshot) {
        self.total += o.total;
        self.success += o.success;
        self.failed += o.failed;
        self.timeout += o.timeout;
        self.errors += o.errors;
        self.retries += o.retries;
        self.dlq += o.dlq;
        self.rejected += o.rejected;
    }
}

/// Autoscaler activity, all monotonic since process start.
#[derive(Debug, Default)]
struct AutoscaleCounts {
    vms_created: AtomicU64,
    vms_drained: AtomicU64,
    vms_reaped: AtomicU64,
    /// Reconcile ticks that grew / shrank the pool. Event counts, not VM
    /// counts: one event can move several VMs.
    scale_up_events: AtomicU64,
    scale_down_events: AtomicU64,
    /// Synchronous invokes that arrived with no capacity and waited for a boot.
    cold_start_waits: AtomicU64,
    /// …of those, the ones a VM eventually ran vs. the ones that gave up.
    cold_start_hits: AtomicU64,
    cold_start_timeouts: AtomicU64,
}

impl AutoscaleCounts {
    fn snapshot(&self) -> AutoscaleSnapshot {
        AutoscaleSnapshot {
            vms_created: self.vms_created.load(Ordering::Relaxed),
            vms_drained: self.vms_drained.load(Ordering::Relaxed),
            vms_reaped: self.vms_reaped.load(Ordering::Relaxed),
            scale_up_events: self.scale_up_events.load(Ordering::Relaxed),
            scale_down_events: self.scale_down_events.load(Ordering::Relaxed),
            cold_start_waits: self.cold_start_waits.load(Ordering::Relaxed),
            cold_start_hits: self.cold_start_hits.load(Ordering::Relaxed),
            cold_start_timeouts: self.cold_start_timeouts.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AutoscaleSnapshot {
    pub vms_created: u64,
    pub vms_drained: u64,
    pub vms_reaped: u64,
    pub scale_up_events: u64,
    pub scale_down_events: u64,
    pub cold_start_waits: u64,
    pub cold_start_hits: u64,
    pub cold_start_timeouts: u64,
}

impl AutoscaleSnapshot {
    fn merge(&mut self, o: &AutoscaleSnapshot) {
        self.vms_created += o.vms_created;
        self.vms_drained += o.vms_drained;
        self.vms_reaped += o.vms_reaped;
        self.scale_up_events += o.scale_up_events;
        self.scale_down_events += o.scale_down_events;
        self.cold_start_waits += o.cold_start_waits;
        self.cold_start_hits += o.cold_start_hits;
        self.cold_start_timeouts += o.cold_start_timeouts;
    }
}

/// The backlog. A **gauge**: set from each reconcile tick, never accumulated.
#[derive(Debug, Default)]
struct QueueGaugeCell {
    pending: AtomicU64,
    ack_pending: AtomicU64,
    dlq: AtomicU64,
    paused: AtomicBool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct QueueGauge {
    pub pending: u64,
    pub ack_pending: u64,
    pub dlq: u64,
    pub paused: bool,
}

impl QueueGauge {
    /// Summing a gauge is only meaningful for the fleet-wide backlog total,
    /// which is what the dashboard's "queued" tile shows.
    fn merge(&mut self, o: &QueueGauge) {
        self.pending += o.pending;
        self.ack_pending += o.ack_pending;
        self.dlq += o.dlq;
        // Deliberately not merged: "some function is paused" is not a property
        // of the fleet, and OR-ing it would make one paused function look like a
        // global halt.
    }
}

/// Everything measured for a single function. All storage lives here; the global
/// view is these merged, so a counter is never double-recorded.
#[derive(Debug)]
pub struct FunctionMetrics {
    invocations: InvocationCounts,
    exec_ms: Histogram,
    queue_wait_ms: Histogram,
    cold_start_s: Histogram,
    autoscale: AutoscaleCounts,
    queue: QueueGaugeCell,
}

impl Default for FunctionMetrics {
    fn default() -> Self {
        Self {
            invocations: InvocationCounts::default(),
            exec_ms: Histogram::new(EXEC_BOUNDS_MS),
            queue_wait_ms: Histogram::new(QUEUE_WAIT_BOUNDS_MS),
            cold_start_s: Histogram::new(COLD_START_BOUNDS_S),
            autoscale: AutoscaleCounts::default(),
            queue: QueueGaugeCell::default(),
        }
    }
}

impl FunctionMetrics {
    fn snapshot(&self) -> FunctionMetricsSnapshot {
        FunctionMetricsSnapshot {
            invocations: self.invocations.snapshot(),
            exec_ms: self.exec_ms.snapshot(),
            queue_wait_ms: self.queue_wait_ms.snapshot(),
            cold_start_s: self.cold_start_s.snapshot(),
            autoscale: self.autoscale.snapshot(),
            queue: QueueGauge {
                pending: self.queue.pending.load(Ordering::Relaxed),
                ack_pending: self.queue.ack_pending.load(Ordering::Relaxed),
                dlq: self.queue.dlq.load(Ordering::Relaxed),
                paused: self.queue.paused.load(Ordering::Relaxed),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionMetricsSnapshot {
    pub invocations: InvocationSnapshot,
    pub exec_ms: HistogramSnapshot,
    pub queue_wait_ms: HistogramSnapshot,
    pub cold_start_s: HistogramSnapshot,
    pub autoscale: AutoscaleSnapshot,
    pub queue: QueueGauge,
}

impl FunctionMetricsSnapshot {
    pub fn empty() -> Self {
        Self {
            invocations: InvocationSnapshot::default(),
            exec_ms: HistogramSnapshot::empty(EXEC_BOUNDS_MS),
            queue_wait_ms: HistogramSnapshot::empty(QUEUE_WAIT_BOUNDS_MS),
            cold_start_s: HistogramSnapshot::empty(COLD_START_BOUNDS_S),
            autoscale: AutoscaleSnapshot::default(),
            queue: QueueGauge::default(),
        }
    }

    fn merge(&mut self, o: &FunctionMetricsSnapshot) {
        self.invocations.merge(&o.invocations);
        self.exec_ms.merge(&o.exec_ms);
        self.queue_wait_ms.merge(&o.queue_wait_ms);
        self.cold_start_s.merge(&o.cold_start_s);
        self.autoscale.merge(&o.autoscale);
        self.queue.merge(&o.queue);
    }
}

/// Whole-host resource usage from the daemon, refreshed each reconcile tick. A
/// single gauge (not per-function), so a plain set of atomics; CPU% is stored as
/// f64 bits.
#[derive(Debug, Default)]
struct HostGauge {
    available: AtomicBool,
    cpu_count: AtomicU64,
    cpu_percent_bits: AtomicU64,
    memory_total_bytes: AtomicU64,
    memory_used_bytes: AtomicU64,
    sampled_at_ms: AtomicU64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HostUsageSnapshot {
    /// False until the daemon's poller has produced a sample. The dashboard
    /// shows "—" rather than a misleading zero.
    pub available: bool,
    pub cpu_count: u64,
    pub cpu_percent: f64,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub sampled_at_ms: u64,
}

/// The metrics registry: one `FunctionMetrics` per function, the derived global
/// rollup, and the host gauge. Shared as `Arc<Metrics>` by the dispatcher, the
/// autoscaler, and the admin API.
#[derive(Debug)]
pub struct Metrics {
    per_function: ArcSwap<HashMap<String, Arc<FunctionMetrics>>>,
    host: HostGauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            per_function: ArcSwap::from_pointee(HashMap::new()),
            host: HostGauge::default(),
        }
    }

    /// Overwrite the host usage gauge with the daemon's latest sample.
    pub fn record_host_usage(
        &self,
        available: bool,
        cpu_count: u32,
        cpu_percent: f64,
        memory_total_bytes: u64,
        memory_used_bytes: u64,
        sampled_at_ms: u64,
    ) {
        self.host.available.store(available, Ordering::Relaxed);
        self.host
            .cpu_count
            .store(cpu_count as u64, Ordering::Relaxed);
        self.host
            .cpu_percent_bits
            .store(cpu_percent.to_bits(), Ordering::Relaxed);
        self.host
            .memory_total_bytes
            .store(memory_total_bytes, Ordering::Relaxed);
        self.host
            .memory_used_bytes
            .store(memory_used_bytes, Ordering::Relaxed);
        self.host
            .sampled_at_ms
            .store(sampled_at_ms, Ordering::Relaxed);
    }

    pub fn host_snapshot(&self) -> HostUsageSnapshot {
        HostUsageSnapshot {
            available: self.host.available.load(Ordering::Relaxed),
            cpu_count: self.host.cpu_count.load(Ordering::Relaxed),
            cpu_percent: f64::from_bits(self.host.cpu_percent_bits.load(Ordering::Relaxed)),
            memory_total_bytes: self.host.memory_total_bytes.load(Ordering::Relaxed),
            memory_used_bytes: self.host.memory_used_bytes.load(Ordering::Relaxed),
            sampled_at_ms: self.host.sampled_at_ms.load(Ordering::Relaxed),
        }
    }

    /// Get, or insert-then-get, the metrics for a function.
    ///
    /// The fast path is a lock-free load and a hash lookup. The slow path
    /// (copy-on-write insert) runs at most once per function id ever seen.
    /// Racing inserts are resolved by re-checking under the rebuilt map, so two
    /// callers cannot install divergent counters.
    pub fn function(&self, id: &str) -> Arc<FunctionMetrics> {
        if let Some(m) = self.per_function.load().get(id) {
            return m.clone();
        }
        let created = Arc::new(FunctionMetrics::default());
        let mut installed = created.clone();
        self.per_function.rcu(|current| {
            let mut next = (**current).clone();
            installed = next
                .entry(id.to_string())
                .or_insert(created.clone())
                .clone();
            next
        });
        installed
    }

    // --- Recording -------------------------------------------------------

    /// A finished invocation. `exec` is the command's wall clock; `queue_wait`
    /// is how long the event sat before dispatch.
    pub fn record_invocation(
        &self,
        function: &str,
        outcome: crate::bus::Outcome,
        exec: Duration,
        queue_wait_ms: u64,
    ) {
        use crate::bus::Outcome;
        let m = self.function(function);
        m.invocations.total.fetch_add(1, Ordering::Relaxed);
        let bucket = match outcome {
            Outcome::Success => &m.invocations.success,
            Outcome::Failed => &m.invocations.failed,
            Outcome::Timeout => &m.invocations.timeout,
            Outcome::Error => &m.invocations.errors,
        };
        bucket.fetch_add(1, Ordering::Relaxed);

        // Saturate rather than truncate: a multi-hour hang is still "very slow",
        // and a u128 -> u64 cast could otherwise wrap it small.
        m.exec_ms
            .record(exec.as_millis().min(u64::MAX as u128) as u64);
        m.queue_wait_ms.record(queue_wait_ms);
    }

    /// An event was redelivered.
    pub fn record_retry(&self, function: &str) {
        self.function(function)
            .invocations
            .retries
            .fetch_add(1, Ordering::Relaxed);
    }

    /// An event exhausted its attempts and was parked.
    pub fn record_dlq(&self, function: &str) {
        self.function(function)
            .invocations
            .dlq
            .fetch_add(1, Ordering::Relaxed);
    }

    /// An event was refused before it ever ran.
    pub fn record_rejected(&self, function: &str) {
        self.function(function)
            .invocations
            .rejected
            .fetch_add(1, Ordering::Relaxed);
    }

    /// A VM went from created to usable in `secs`.
    pub fn record_cold_start(&self, function: &str, secs: u64) {
        self.function(function).cold_start_s.record(secs);
    }

    /// Refresh a function's backlog gauge from the reconcile tick.
    pub fn record_queue_depth(&self, function: &str, pending: u64, ack_pending: u64, dlq: u64, paused: bool) {
        let m = self.function(function);
        m.queue.pending.store(pending, Ordering::Relaxed);
        m.queue.ack_pending.store(ack_pending, Ordering::Relaxed);
        m.queue.dlq.store(dlq, Ordering::Relaxed);
        m.queue.paused.store(paused, Ordering::Relaxed);
    }

    pub fn record_scale_up(&self, function: &str, n: u64) {
        if n == 0 {
            return;
        }
        let m = self.function(function);
        m.autoscale.vms_created.fetch_add(n, Ordering::Relaxed);
        m.autoscale.scale_up_events.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_scale_down(&self, function: &str, n: u64) {
        if n == 0 {
            return;
        }
        let m = self.function(function);
        m.autoscale.vms_drained.fetch_add(n, Ordering::Relaxed);
        m.autoscale.scale_down_events.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_reaped(&self, function: &str, n: u64) {
        if n == 0 {
            return;
        }
        self.function(function)
            .autoscale
            .vms_reaped
            .fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_cold_start_wait(&self, function: &str) {
        self.function(function)
            .autoscale
            .cold_start_waits
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cold_start_hit(&self, function: &str) {
        self.function(function)
            .autoscale
            .cold_start_hits
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cold_start_timeout(&self, function: &str) {
        self.function(function)
            .autoscale
            .cold_start_timeouts
            .fetch_add(1, Ordering::Relaxed);
    }

    // --- Reading (dashboard poll) ----------------------------------------

    /// The snapshot for one function, or an all-zero snapshot if it has never
    /// recorded anything — so a freshly-registered function still renders.
    pub fn function_snapshot(&self, id: &str) -> FunctionMetricsSnapshot {
        self.per_function
            .load()
            .get(id)
            .map(|m| m.snapshot())
            .unwrap_or_else(FunctionMetricsSnapshot::empty)
    }

    /// Global rollup: every function's snapshot merged. Exact, because the
    /// underlying buckets and counters are additive.
    pub fn global_snapshot(&self) -> FunctionMetricsSnapshot {
        let mut global = FunctionMetricsSnapshot::empty();
        for m in self.per_function.load().values() {
            global.merge(&m.snapshot());
        }
        global
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Outcome;

    #[test]
    fn histogram_buckets_and_overflow() {
        let h = Histogram::new(EXEC_BOUNDS_MS);
        h.record(0); // <= 10
        h.record(10); // <= 10
        h.record(30); // <= 50
        h.record(999_999); // overflow
        let s = h.snapshot();
        assert_eq!(s.count, 4);
        assert_eq!(s.buckets[0].count, 2, "0 and 10 fall in the <=10 bucket");
        assert_eq!(s.buckets[0].le, Some(10));
        assert_eq!(
            s.buckets.last().unwrap().le,
            None,
            "the final bucket is overflow"
        );
        assert_eq!(s.buckets.last().unwrap().count, 1);
    }

    #[test]
    fn percentiles_are_monotonic_and_bounded() {
        let h = Histogram::new(EXEC_BOUNDS_MS);
        for v in [10, 25, 50, 100, 250, 500, 1000] {
            for _ in 0..10 {
                h.record(v);
            }
        }
        let s = h.snapshot();
        assert!(s.p50 <= s.p90, "p50 {} !<= p90 {}", s.p50, s.p90);
        assert!(s.p90 <= s.p99, "p90 {} !<= p99 {}", s.p90, s.p99);
        assert!(s.p99 <= 1000.0, "p99 {} exceeds the max sample", s.p99);
    }

    #[test]
    fn an_empty_histogram_reports_zeroes_not_nan() {
        let s = Histogram::new(COLD_START_BOUNDS_S).snapshot();
        assert_eq!(s.count, 0);
        assert_eq!(s.mean, 0.0);
        assert_eq!(s.p50, 0.0);
        assert!(s.mean.is_finite(), "a zero-count mean must not be NaN");
    }

    /// Regression: merging produced the global view by averaging percentiles,
    /// which is not a percentile of anything. Merging the buckets and
    /// recomputing makes the global p99 exact.
    #[test]
    fn merged_histograms_recompute_percentiles_from_buckets() {
        let a = Histogram::new(EXEC_BOUNDS_MS);
        let b = Histogram::new(EXEC_BOUNDS_MS);
        for _ in 0..98 {
            a.record(10);
        }
        // Two slow samples, so the 99th percentile of 100 genuinely falls among
        // them rather than in the fast bucket.
        b.record(20000);
        b.record(20000);

        let mut merged = a.snapshot();
        merged.merge(&b.snapshot());
        assert_eq!(merged.count, 100, "count is additive across merges");
        assert_eq!(merged.sum, 98 * 10 + 2 * 20000, "so is the sum");
        assert!(
            merged.p50 <= 10.0,
            "p50 must reflect the 98 fast samples, got {}",
            merged.p50
        );
        assert!(
            merged.p99 > 10000.0,
            "p99 must reflect the slow tail, got {}",
            merged.p99
        );
        // Averaging the two histograms' own p99s would have given roughly
        // (10 + 20000)/2 — a number no sample is anywhere near.
        assert!(merged.p99 <= 20000.0, "and must not exceed the slowest sample");
    }

    #[test]
    fn invocation_outcomes_are_partitioned() {
        let m = Metrics::new();
        m.record_invocation("demo", Outcome::Success, Duration::from_millis(5), 1);
        m.record_invocation("demo", Outcome::Success, Duration::from_millis(5), 1);
        m.record_invocation("demo", Outcome::Failed, Duration::from_millis(5), 1);
        m.record_invocation("demo", Outcome::Timeout, Duration::from_millis(5), 1);
        m.record_invocation("demo", Outcome::Error, Duration::from_millis(5), 1);

        let s = m.function_snapshot("demo").invocations;
        assert_eq!(s.total, 5);
        assert_eq!(s.success, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.timeout, 1);
        assert_eq!(s.errors, 1);
        assert_eq!(
            s.success + s.failed + s.timeout + s.errors,
            s.total,
            "every invocation lands in exactly one outcome bucket"
        );
    }

    #[test]
    fn function_metrics_are_shared_not_duplicated() {
        let m = Metrics::new();
        let a = m.function("demo");
        let b = m.function("demo");
        assert!(Arc::ptr_eq(&a, &b), "same id must return the same counters");
        m.record_invocation("demo", Outcome::Success, Duration::from_millis(5), 0);
        assert_eq!(m.function_snapshot("demo").invocations.total, 1);
    }

    #[test]
    fn global_is_the_merge_of_all_functions() {
        let m = Metrics::new();
        m.record_invocation("a", Outcome::Success, Duration::from_millis(3), 10);
        m.record_invocation("b", Outcome::Failed, Duration::from_millis(7), 20);
        m.record_cold_start("a", 2);
        m.record_scale_up("b", 3);
        m.record_dlq("b");

        let g = m.global_snapshot();
        assert_eq!(g.invocations.total, 2);
        assert_eq!(g.invocations.success, 1);
        assert_eq!(g.invocations.failed, 1);
        assert_eq!(g.invocations.dlq, 1);
        assert_eq!(g.exec_ms.count, 2);
        assert_eq!(g.queue_wait_ms.count, 2);
        assert_eq!(g.cold_start_s.count, 1);
        assert_eq!(g.autoscale.vms_created, 3);
        assert_eq!(g.autoscale.scale_up_events, 1);
    }

    /// Regression: queue depth was accumulated like a counter, so a function
    /// holding a steady backlog of 3 reported an ever-growing number that made
    /// the dashboard look like an unbounded pile-up.
    #[test]
    fn queue_depth_is_a_gauge_not_a_counter() {
        let m = Metrics::new();
        m.record_queue_depth("demo", 3, 1, 0, false);
        m.record_queue_depth("demo", 3, 1, 0, false);
        m.record_queue_depth("demo", 3, 1, 0, false);
        let q = m.function_snapshot("demo").queue;
        assert_eq!(q.pending, 3, "repeated samples of a steady backlog stay 3");
        assert_eq!(q.ack_pending, 1);

        m.record_queue_depth("demo", 0, 0, 2, true);
        let q = m.function_snapshot("demo").queue;
        assert_eq!(q.pending, 0, "a drained queue reports zero");
        assert_eq!(q.dlq, 2);
        assert!(q.paused);
    }

    /// The fleet-wide backlog is the sum of the per-function ones, but "paused"
    /// is not a fleet property — one paused function must not read as a global
    /// halt on the dashboard.
    #[test]
    fn the_global_backlog_sums_depth_but_not_the_pause_flag() {
        let m = Metrics::new();
        m.record_queue_depth("a", 4, 1, 1, true);
        m.record_queue_depth("b", 6, 2, 0, false);
        let g = m.global_snapshot();
        assert_eq!(g.queue.pending, 10);
        assert_eq!(g.queue.ack_pending, 3);
        assert_eq!(g.queue.dlq, 1);
        assert!(!g.queue.paused);
    }

    #[test]
    fn an_unknown_function_snapshot_is_empty_not_missing() {
        let m = Metrics::new();
        let s = m.function_snapshot("never-seen");
        assert_eq!(s.invocations.total, 0);
        assert_eq!(s.exec_ms.buckets.len(), EXEC_BOUNDS_MS.len() + 1);
        assert_eq!(s.queue.pending, 0);
    }

    #[test]
    fn cold_start_outcomes_are_counted() {
        let m = Metrics::new();
        m.record_cold_start_wait("demo");
        m.record_cold_start_wait("demo");
        m.record_cold_start_hit("demo");
        m.record_cold_start_timeout("demo");
        let a = m.function_snapshot("demo").autoscale;
        assert_eq!(a.cold_start_waits, 2);
        assert_eq!(a.cold_start_hits, 1);
        assert_eq!(a.cold_start_timeouts, 1);
    }

    #[test]
    fn zero_count_scaling_events_are_not_recorded() {
        let m = Metrics::new();
        m.record_scale_up("demo", 0);
        m.record_scale_down("demo", 0);
        m.record_reaped("demo", 0);
        let a = m.function_snapshot("demo").autoscale;
        assert_eq!(
            a.scale_up_events, 0,
            "a tick that created nothing is not a scale-up event"
        );
        assert_eq!(a.scale_down_events, 0);
        assert_eq!(a.vms_reaped, 0);
    }

    /// A retry is a redelivery of one invocation, not an extra invocation, so it
    /// must not inflate the total.
    #[test]
    fn a_retry_is_counted_separately_from_the_invocation_total() {
        let m = Metrics::new();
        m.record_retry("demo");
        m.record_retry("demo");
        m.record_invocation("demo", Outcome::Success, Duration::from_millis(1), 0);
        let s = m.function_snapshot("demo").invocations;
        assert_eq!(s.retries, 2);
        assert_eq!(s.total, 1);
    }
}
