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
use crate::store::schema::{MetricRecord, Record};
use serde::Deserialize;
use std::time::Duration;

/// Reserved deployment id for whole-host samples, which belong to no
/// deployment. Chosen to be a legal partition name while being unlikely to
/// collide: app-lb ids in practice are slugs like `demo` or `vault-86a37f`.
pub const HOST_DEPLOYMENT: &str = "_host";

#[derive(Debug, Deserialize)]
struct MetricsResponse {
    /// Unix seconds. Used as the timestamp for every row in the poll so a chart
    /// lines them up instead of smearing them across the collection latency.
    generated_at: u64,
    host: HostUsage,
    deployments: Vec<DeploymentView>,
}

#[derive(Debug, Deserialize)]
struct HostUsage {
    available: bool,
    cpu_percent: f64,
    memory_used_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct DeploymentView {
    id: String,
    pool: PoolStatus,
    #[serde(default)]
    vms: Vec<VmView>,
    metrics: DeploymentMetrics,
}

#[derive(Debug, Deserialize)]
struct PoolStatus {
    ready: u32,
    draining: u32,
    pending: u32,
    total_in_flight: u32,
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct VmView {
    /// app-lb calls this `sandbox_id` for both kinds of backend, but for a
    /// static deployment it holds the upstream `host:port` rather than a
    /// sandbox. Stored as `backend` for that reason; the wire name is app-lb's.
    #[serde(rename = "sandbox_id")]
    backend: String,
    in_flight: u32,
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeploymentMetrics {
    requests: StatusCounts,
    latency_ms: Histogram,
}

#[derive(Debug, Deserialize)]
struct StatusCounts {
    total: u64,
    errors: u64,
}

#[derive(Debug, Deserialize)]
struct Histogram {
    count: u64,
    p50: f64,
    p90: f64,
    p99: f64,
}

pub struct Poller {
    client: reqwest::Client,
    url: String,
    auth: Option<(String, String)>,
    interval: Duration,
    sink: Sink,
}

impl Poller {
    pub fn new(
        base_url: &str,
        user: Option<String>,
        password: Option<String>,
        interval: Duration,
        sink: Sink,
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

        let records = flatten(&snapshot);
        let count = records.len();
        for record in records {
            self.sink.send(record);
        }
        Ok(count)
    }
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
        assert_eq!(snapshot.deployments[0].vms.len(), 2);
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
}
