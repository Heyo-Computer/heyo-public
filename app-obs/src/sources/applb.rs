//! Polls app-lb's `GET /metrics` and flattens it into metric rows.
//!
//! app-lb already measures everything worth storing — host CPU/memory from the
//! daemon, per-VM CPU/RSS, pool occupancy, request counts and latency
//! percentiles — but keeps it only in memory, cumulative since process start and
//! gone on restart. This turns that live snapshot into history.
//!
//! Only the fields consumed here are declared; serde ignores the rest, so
//! app-lb can add to its response without breaking this.

use crate::ingest::Sink;
use crate::sources::VmTarget;
use crate::sources::nats::Telemetry;
use crate::store::schema::{MetricRecord, Record};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Reserved deployment id for whole-host samples, which belong to no
/// deployment. Chosen to be a legal partition name while being unlikely to
/// collide: app-lb ids in practice are slugs like `demo` or `vault-86a37f`.
pub const HOST_DEPLOYMENT: &str = "_host";

#[derive(Debug, Clone, Deserialize)]
struct MetricsResponse {
    /// Unix seconds. Used as the timestamp for every row in the poll so a chart
    /// lines them up instead of smearing them across the collection latency.
    generated_at: u64,
    host: HostUsage,
    deployments: Vec<DeploymentView>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct HostUsage {
    pub available: bool,
    pub cpu_percent: f64,
    pub memory_used_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct DeploymentView {
    pub id: String,
    /// `vm` or `static`. Optional because an older app-lb may not send it;
    /// see [`vm_targets`] for how that absence is handled.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub upstreams: Vec<String>,
    /// Older app-lb versions did not report this field. Absence means unknown,
    /// not routed: inventing a route would turn idle agent sandboxes into false
    /// outages during a mixed-version rollout.
    #[serde(default)]
    pub routed: Option<bool>,
    pub pool: PoolStatus,
    #[serde(default)]
    pub vms: Vec<VmView>,
    pub metrics: DeploymentMetrics,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct PoolStatus {
    #[serde(default)]
    pub desired_replicas: Option<u32>,
    pub ready: u32,
    pub draining: u32,
    pub pending: u32,
    #[serde(default)]
    pub min_replicas: Option<u32>,
    pub total_in_flight: u32,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct VmView {
    /// app-lb calls this `sandbox_id` for both kinds of backend, but for a
    /// static deployment it holds the upstream `host:port` rather than a
    /// sandbox. Stored as `backend` for that reason; the wire name is app-lb's.
    #[serde(rename(deserialize = "sandbox_id"))]
    pub backend: String,
    pub in_flight: u32,
    #[serde(default)]
    pub healthy: bool,
    #[serde(default)]
    pub draining: bool,
    #[serde(default)]
    pub uptime_secs: u64,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct DeploymentMetrics {
    pub requests: StatusCounts,
    pub latency_ms: Histogram,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct StatusCounts {
    pub total: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Histogram {
    pub count: u64,
    /// Total of every sample, in the histogram's base unit. Cumulative like
    /// `count`, so the two together give a per-interval mean once differenced —
    /// see `MetricRecord::latency_count`.
    pub sum: u64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
}

/// The current topology and gauges behind the platform status API and NATS.
/// Historical storage remains the flattened parquet rows below.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LiveStatus {
    pub schema_version: u8,
    pub source: String,
    pub observed_at_ms: i64,
    pub app_lb_generated_at_ms: i64,
    pub host: HostUsage,
    pub deployments: Vec<DeploymentView>,
}

pub struct Poller {
    client: reqwest::Client,
    url: String,
    auth: Option<(String, String)>,
    interval: Duration,
    sink: Sink,
    source: String,
    live: tokio::sync::watch::Sender<Option<LiveStatus>>,
    telemetry: Option<Telemetry>,
    /// Where the current VM set goes after each successful poll, for the
    /// daemon log tailer. Deliberately never cleared on a failed poll: app-lb
    /// restarting must not stop log collection from sandboxes that are still
    /// running.
    targets: Option<tokio::sync::watch::Sender<Vec<VmTarget>>>,
}

impl Poller {
    pub fn new(
        base_url: &str,
        user: Option<String>,
        password: Option<String>,
        interval: Duration,
        sink: Sink,
        source: String,
        live: tokio::sync::watch::Sender<Option<LiveStatus>>,
        telemetry: Option<Telemetry>,
        targets: Option<tokio::sync::watch::Sender<Vec<VmTarget>>>,
    ) -> Self {
        // app-lb's admin API is loopback and answers promptly or not at all; a
        // short timeout keeps a wedged connection from stalling the poll loop
        // past its own interval.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        // Only Basic auth, and only when a password is set — matching app-lb,
        // where the gate turns on with `APP_LB_DASHBOARD_PASSWORD` and the user
        // defaults to `admin`.
        let auth = password.map(|p| (user.unwrap_or_else(|| "admin".into()), p));

        Self {
            client,
            url: format!("{}/metrics", base_url.trim_end_matches('/')),
            auth,
            interval,
            sink,
            source,
            live,
            telemetry,
            targets,
        }
    }

    /// Poll until the process ends.
    ///
    /// app-lb being unreachable is expected, not exceptional — it restarts, and
    /// it may well start after we do. Failures are logged and retried on the
    /// next tick rather than ending the loop.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            match self.poll_once().await {
                Ok(count) => tracing::debug!(rows = count, "polled app-lb metrics"),
                Err(e) => tracing::warn!(url = %self.url, error = %e, "app-lb metrics poll failed"),
            }
        }
    }

    async fn poll_once(&self) -> Result<usize, reqwest::Error> {
        let mut request = self.client.get(&self.url);
        if let Some((user, password)) = &self.auth {
            request = request.basic_auth(user, Some(password));
        }
        let response = request.send().await?.error_for_status()?;
        let snapshot: MetricsResponse = response.json().await?;

        let live = LiveStatus {
            schema_version: 1,
            source: self.source.clone(),
            observed_at_ms: now_ms(),
            app_lb_generated_at_ms: (snapshot.generated_at as i64).saturating_mul(1000),
            host: snapshot.host.clone(),
            deployments: snapshot.deployments.clone(),
        };
        self.live.send_replace(Some(live.clone()));
        if let Some(telemetry) = &self.telemetry {
            telemetry.publish(live);
        }

        if let Some(targets) = &self.targets {
            // send_if_modified so an unchanged fleet doesn't wake the tailer
            // manager every poll tick.
            let current = vm_targets(&snapshot);
            targets.send_if_modified(|previous| {
                if *previous == current {
                    false
                } else {
                    *previous = current;
                    true
                }
            });
        }

        let records = flatten(&snapshot);
        let count = records.len();
        for record in records {
            self.sink.send(record);
        }
        Ok(count)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

/// The sandboxes worth tailing daemon logs from: every backend of a VM
/// deployment. Static deployments are excluded — their "vms" entries hold
/// `host:port` upstreams, which the daemon has never heard of. When an older
/// app-lb omits `kind`, the backend's shape decides: a `host:port` always
/// contains a colon, a sandbox id never does.
fn vm_targets(snapshot: &MetricsResponse) -> Vec<VmTarget> {
    let mut targets = Vec::new();
    for deployment in &snapshot.deployments {
        for vm in &deployment.vms {
            let is_vm = match deployment.kind.as_deref() {
                Some("vm") => true,
                Some(_) => false,
                None => !vm.backend.contains(':'),
            };
            if is_vm {
                targets.push(VmTarget {
                    deployment: deployment.id.clone(),
                    backend: vm.backend.clone(),
                });
            }
        }
    }
    targets
}

/// Turn one snapshot into rows: one for the host, one per deployment, one per VM.
fn flatten(snapshot: &MetricsResponse) -> Vec<Record> {
    // Seconds to millis. Every row in a poll shares this, so a chart joins them
    // on a single x value.
    let ts_millis = (snapshot.generated_at as i64).saturating_mul(1000);
    let mut records = Vec::new();

    // `available` is false until the daemon has produced a sample; writing zeros
    // then would draw a real-looking idle host.
    if snapshot.host.available {
        records.push(Record::Metric(MetricRecord {
            ts_millis,
            deployment: HOST_DEPLOYMENT.into(),
            cpu_percent: Some(snapshot.host.cpu_percent),
            memory_bytes: Some(snapshot.host.memory_used_bytes),
            ..Default::default()
        }));
    }

    for deployment in &snapshot.deployments {
        // Deployment-wide row: pool gauges plus traffic. Percentiles are
        // meaningless before any request has been measured, so they stay null
        // rather than reporting a confident 0ms.
        let measured = deployment.metrics.latency_ms.count > 0;
        records.push(Record::Metric(MetricRecord {
            ts_millis,
            deployment: deployment.id.clone(),
            backend: None,
            cpu_percent: deployment.pool.cpu_percent,
            memory_bytes: deployment.pool.memory_bytes,
            in_flight: Some(deployment.pool.total_in_flight),
            ready: Some(deployment.pool.ready),
            pending: Some(deployment.pool.pending),
            draining: Some(deployment.pool.draining),
            requests_total: Some(deployment.metrics.requests.total),
            errors_total: Some(deployment.metrics.requests.errors),
            p50_ms: measured.then_some(deployment.metrics.latency_ms.p50),
            p90_ms: measured.then_some(deployment.metrics.latency_ms.p90),
            p99_ms: measured.then_some(deployment.metrics.latency_ms.p99),
            // Unlike the percentiles, these are kept even at count = 0: a zero
            // count is a real measurement ("nothing has been timed yet"), and
            // the next sample's delta against it is the first honest interval.
            latency_count: Some(deployment.metrics.latency_ms.count),
            latency_sum: Some(deployment.metrics.latency_ms.sum),
        }));

        // Per-VM rows. Pool and traffic figures are deployment-wide and are left
        // null here rather than duplicated, so summing a column across VMs can't
        // silently multiply them.
        for vm in &deployment.vms {
            records.push(Record::Metric(MetricRecord {
                ts_millis,
                deployment: deployment.id.clone(),
                backend: Some(vm.backend.clone()),
                cpu_percent: vm.cpu_percent,
                memory_bytes: vm.memory_bytes,
                in_flight: Some(vm.in_flight),
                ..Default::default()
            }));
        }
    }

    records
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response in the shape app-lb actually serves.
    const SNAPSHOT: &str = r#"{
        "generated_at": 1785260096,
        "uptime_secs": 3600,
        "host": {
            "available": true,
            "cpu_count": 8,
            "cpu_percent": 42.5,
            "memory_total_bytes": 16000000000,
            "memory_used_bytes": 8000000000,
            "sampled_at_ms": 1785260096000
        },
        "fleet": {"deployments": 1, "ready": 2, "draining": 0, "pending": 0, "total_in_flight": 3},
        "global": {
            "requests": {"total": 100, "c2xx": 95, "c3xx": 0, "c4xx": 4, "c5xx": 1, "errors": 5},
            "latency_ms": {"count": 100, "sum": 1000, "mean": 10.0, "p50": 8.0, "p90": 25.0, "p99": 90.0, "buckets": []},
            "cold_start_s": {"count": 0, "sum": 0, "mean": 0.0, "p50": 0.0, "p90": 0.0, "p99": 0.0, "buckets": []},
            "autoscale": {}
        },
        "deployments": [{
            "id": "demo",
            "kind": "vm",
            "upstreams": [],
            "routed": true,
            "pool": {
                "desired_replicas": 2, "ready": 2, "draining": 0, "pending": 1,
                "total_in_flight": 3, "target_concurrency": 10, "min_replicas": 1,
                "max_replicas": 4, "warm_pool": 0, "utilization": 0.15,
                "cpu_percent": 12.5, "memory_bytes": 500000000
            },
            "vms": [
                {"sandbox_id": "sb-aaa", "addr": "172.16.0.2:8080", "in_flight": 2,
                 "healthy": true, "draining": false, "uptime_secs": 120,
                 "cpu_percent": 7.5, "memory_bytes": 300000000},
                {"sandbox_id": "sb-bbb", "addr": "172.16.0.6:8080", "in_flight": 1,
                 "healthy": true, "draining": false, "uptime_secs": 60,
                 "cpu_percent": null, "memory_bytes": null}
            ],
            "metrics": {
                "requests": {"total": 100, "c2xx": 95, "c3xx": 0, "c4xx": 4, "c5xx": 1, "errors": 5},
                "latency_ms": {"count": 100, "sum": 1000, "mean": 10.0, "p50": 8.0, "p90": 25.0, "p99": 90.0, "buckets": []},
                "cold_start_s": {"count": 0, "sum": 0, "mean": 0.0, "p50": 0.0, "p90": 0.0, "p99": 0.0, "buckets": []},
                "autoscale": {}
            }
        }]
    }"#;

    fn parse() -> MetricsResponse {
        serde_json::from_str(SNAPSHOT).expect("app-lb's real response shape must deserialize")
    }

    #[test]
    fn a_real_applb_response_deserializes() {
        let snapshot = parse();
        assert_eq!(snapshot.generated_at, 1_785_260_096);
        assert_eq!(snapshot.deployments.len(), 1);
        assert_eq!(snapshot.deployments[0].routed, Some(true));
        assert_eq!(snapshot.deployments[0].vms.len(), 2);
    }

    #[test]
    fn an_old_response_keeps_routing_unknown() {
        let old = SNAPSHOT.replace("\n            \"routed\": true,", "");
        let snapshot: MetricsResponse = serde_json::from_str(&old).unwrap();
        assert_eq!(snapshot.deployments[0].routed, None);
    }

    #[test]
    fn one_poll_yields_host_deployment_and_per_vm_rows() {
        let records = flatten(&parse());
        assert_eq!(records.len(), 4, "1 host + 1 deployment + 2 VMs");

        // Every row shares the snapshot's timestamp so a chart aligns them.
        for record in &records {
            assert_eq!(record.ts_millis(), 1_785_260_096_000);
        }
    }

    #[test]
    fn host_usage_lands_under_the_reserved_deployment() {
        let records = flatten(&parse());
        let Record::Metric(host) = &records[0] else {
            panic!("expected a metric");
        };
        assert_eq!(host.deployment, HOST_DEPLOYMENT);
        assert_eq!(host.backend, None);
        assert_eq!(host.cpu_percent, Some(42.5));
        assert_eq!(host.memory_bytes, Some(8_000_000_000));
    }

    #[test]
    fn unsampled_host_usage_is_omitted_rather_than_written_as_zero() {
        // app-lb reports available=false until the daemon's first sample;
        // storing zeros would draw a convincing idle host that never existed.
        let mut snapshot = parse();
        snapshot.host.available = false;
        let records = flatten(&snapshot);
        assert_eq!(records.len(), 3, "host row dropped, deployment and VMs stay");
        assert!(records.iter().all(|r| r.deployment() != HOST_DEPLOYMENT));
    }

    #[test]
    fn deployment_rows_carry_pool_and_traffic() {
        let records = flatten(&parse());
        let Record::Metric(deployment) = &records[1] else {
            panic!("expected a metric");
        };
        assert_eq!(deployment.deployment, "demo");
        assert_eq!(deployment.backend, None);
        assert_eq!(deployment.ready, Some(2));
        assert_eq!(deployment.pending, Some(1));
        assert_eq!(deployment.in_flight, Some(3));
        assert_eq!(deployment.requests_total, Some(100));
        assert_eq!(deployment.errors_total, Some(5));
        assert_eq!(deployment.p50_ms, Some(8.0));
        assert_eq!(deployment.p90_ms, Some(25.0));
        assert_eq!(deployment.p99_ms, Some(90.0));
        assert_eq!(deployment.latency_count, Some(100));
        assert_eq!(deployment.latency_sum, Some(1000));
    }

    #[test]
    fn latency_totals_are_kept_even_when_nothing_has_been_timed() {
        // The percentiles go null at count = 0 because 0ms is not a measurement.
        // The totals are the opposite case: a zero count *is* one, and it is
        // what the next sample's delta is taken against. Dropping it would make
        // the first interval after a restart unchartable.
        let mut snapshot = parse();
        snapshot.deployments[0].metrics.latency_ms.count = 0;
        snapshot.deployments[0].metrics.latency_ms.sum = 0;
        let records = flatten(&snapshot);
        let Record::Metric(deployment) = &records[1] else {
            panic!("expected a metric");
        };
        assert_eq!(deployment.p50_ms, None);
        assert_eq!(deployment.latency_count, Some(0));
        assert_eq!(deployment.latency_sum, Some(0));
    }

    #[test]
    fn percentiles_are_null_until_a_request_has_been_measured() {
        // An empty histogram reports 0.0 for every percentile. Storing that
        // would put a flat 0ms line on the chart that reads as "very fast"
        // rather than "no data".
        let mut snapshot = parse();
        snapshot.deployments[0].metrics.latency_ms.count = 0;
        let records = flatten(&snapshot);
        let Record::Metric(deployment) = &records[1] else {
            panic!("expected a metric");
        };
        assert_eq!(deployment.p50_ms, None);
        assert_eq!(deployment.p90_ms, None);
        assert_eq!(deployment.p99_ms, None);
        assert_eq!(
            deployment.requests_total,
            Some(100),
            "counters are still real",
        );
    }

    #[test]
    fn per_vm_rows_do_not_duplicate_deployment_wide_figures() {
        // Summing `requests_total` across VMs must not multiply the
        // deployment's count by the number of replicas.
        let records = flatten(&parse());
        let Record::Metric(vm) = &records[2] else {
            panic!("expected a metric");
        };
        assert_eq!(vm.backend.as_deref(), Some("sb-aaa"));
        assert_eq!(vm.cpu_percent, Some(7.5));
        assert_eq!(vm.in_flight, Some(2), "in-flight is genuinely per-VM");
        assert_eq!(vm.requests_total, None);
        assert_eq!(vm.ready, None);
        assert_eq!(vm.p50_ms, None);
        // Latency is measured at the proxy per deployment, not per VM. Copying
        // the totals down would make a sum across replicas count every request
        // once per VM.
        assert_eq!(vm.latency_count, None);
        assert_eq!(vm.latency_sum, None);
    }

    #[test]
    fn a_vm_the_daemon_has_not_sampled_keeps_null_usage() {
        let records = flatten(&parse());
        let Record::Metric(vm) = &records[3] else {
            panic!("expected a metric");
        };
        assert_eq!(vm.backend.as_deref(), Some("sb-bbb"));
        assert_eq!(vm.cpu_percent, None, "null, not 0.0");
        assert_eq!(vm.memory_bytes, None);
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        // app-lb must be free to add to its response without breaking us.
        let extended = SNAPSHOT.replace(
            r#""generated_at": 1785260096,"#,
            r#""generated_at": 1785260096, "some_new_field": {"nested": true},"#,
        );
        assert!(serde_json::from_str::<MetricsResponse>(&extended).is_ok());
    }

    #[test]
    fn vm_deployments_yield_one_target_per_backend() {
        let targets = vm_targets(&parse());
        assert_eq!(
            targets,
            vec![
                VmTarget {
                    deployment: "demo".into(),
                    backend: "sb-aaa".into(),
                },
                VmTarget {
                    deployment: "demo".into(),
                    backend: "sb-bbb".into(),
                },
            ],
        );
    }

    #[test]
    fn static_deployments_are_not_tailed() {
        // A static deployment's "vms" hold host:port upstreams; asking the
        // daemon to stream logs for "10.0.0.5:3000" would 404 forever.
        let mut snapshot = parse();
        snapshot.deployments[0].kind = Some("static".into());
        assert!(vm_targets(&snapshot).is_empty());
    }

    #[test]
    fn a_missing_kind_falls_back_to_the_backend_shape() {
        // Older app-lb: no `kind`. Sandbox ids never contain a colon;
        // host:port upstreams always do.
        let mut snapshot = parse();
        snapshot.deployments[0].kind = None;
        snapshot.deployments[0].vms[1].backend = "10.0.0.5:3000".into();
        let targets = vm_targets(&snapshot);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].backend, "sb-aaa");
    }
}
