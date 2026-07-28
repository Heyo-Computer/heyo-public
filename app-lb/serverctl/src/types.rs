//! Read-side views of the admin API's JSON.
//!
//! Deliberately lenient: every field defaults, so a serverctl that is a version
//! behind the app-lb it is talking to still renders what it understands instead
//! of failing to parse. These types are only ever used for *display* — writes
//! go through `serde_json::Value` so nothing is dropped on a round trip.

use serde::Deserialize;
use std::collections::BTreeMap;

// -- GET /deployments, GET /deployments/:id --------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeploymentStatus {
    pub spec: DeploymentSpec,
    /// `"vm"` (managed pool) or `"static"` (fixed proxy_pass upstreams).
    pub kind: String,
    pub desired_replicas: u32,
    pub ready: usize,
    pub pending: usize,
    pub total_in_flight: usize,
    pub vms: Vec<VmStatus>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct VmStatus {
    pub sandbox_id: String,
    pub addr: String,
    pub in_flight: usize,
    pub healthy: bool,
    pub draining: bool,
}

impl VmStatus {
    /// The single-word status column, in precedence order: a draining VM is
    /// still serving, so that fact outranks its health.
    pub fn status(&self) -> &'static str {
        if self.draining {
            "Draining"
        } else if self.healthy {
            "Ready"
        } else {
            "NotReady"
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeploymentSpec {
    pub id: String,
    pub routes: Vec<RouteRule>,
    pub vm: Option<VmSpec>,
    pub scaling: ScalingPolicy,
    pub health: HealthCheck,
    pub upstreams: Vec<String>,
}

impl DeploymentSpec {
    pub fn is_static(&self) -> bool {
        !self.upstreams.is_empty()
    }

    /// The routes as one column: `secrets.local`, `*.apps.example.com/api`, …
    pub fn routes_summary(&self) -> String {
        if self.routes.is_empty() {
            return "<none>".into();
        }
        let shown: Vec<String> = self.routes.iter().take(3).map(RouteRule::render).collect();
        let extra = self.routes.len().saturating_sub(shown.len());
        if extra > 0 {
            format!("{} +{extra} more", shown.join(","))
        } else {
            shown.join(",")
        }
    }

    /// What traffic actually lands on: the image for a managed pool, the
    /// upstream list for a static one.
    pub fn backend_summary(&self) -> String {
        if self.is_static() {
            return self.upstreams.join(",");
        }
        match &self.vm {
            Some(vm) => {
                let image = vm.image.as_deref().unwrap_or("ubuntu:24.04 (default)");
                format!("{image}:{}", vm.port)
            }
            None => "<none>".into(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RouteRule {
    pub host: Option<String>,
    pub host_suffix: Option<String>,
    pub path_prefix: Option<String>,
}

impl RouteRule {
    /// One rule as a single token. A `host_suffix` is shown with the `*.` that
    /// its semantics imply (apex *and* any subdomain).
    pub fn render(&self) -> String {
        let mut s = String::new();
        if let Some(h) = &self.host {
            s.push_str(h);
        }
        if let Some(suffix) = &self.host_suffix {
            if !s.is_empty() {
                s.push('+');
            }
            s.push_str("*.");
            s.push_str(suffix.trim_start_matches('.'));
        }
        if let Some(p) = &self.path_prefix {
            if s.is_empty() {
                s.push('*');
            }
            s.push_str(p);
        }
        if s.is_empty() { "<empty>".into() } else { s }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct VmSpec {
    pub driver: String,
    pub image: Option<String>,
    pub port: u16,
    pub start_command: Option<String>,
    pub size_class: Option<String>,
    pub disk_size_gb: Option<u32>,
    pub working_directory: Option<String>,
    pub env_vars: Option<BTreeMap<String, String>>,
    pub setup_hooks: Option<Vec<String>>,
    pub open_ports: Vec<u16>,
    pub ttl_seconds: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ScalingPolicy {
    pub min_replicas: u32,
    pub max_replicas: u32,
    pub warm_pool: u32,
    pub target_concurrency: u32,
    pub scale_to_zero_after_secs: u64,
    pub cold_start_timeout_secs: u64,
    pub drain_timeout_secs: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HealthCheck {
    pub path: Option<String>,
    pub port: Option<u16>,
    pub timeout_secs: u64,
}

// -- GET /metrics ----------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MetricsResponse {
    pub generated_at: u64,
    pub uptime_secs: u64,
    pub host: HostUsage,
    pub fleet: FleetPool,
    pub global: DeploymentMetrics,
    pub deployments: Vec<DeploymentView>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HostUsage {
    /// False until the daemon has produced a sample.
    pub available: bool,
    pub cpu_count: u64,
    pub cpu_percent: f64,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub sampled_at_ms: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FleetPool {
    pub deployments: usize,
    pub ready: usize,
    pub draining: usize,
    pub pending: usize,
    pub total_in_flight: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeploymentView {
    pub id: String,
    pub kind: String,
    pub upstreams: Vec<String>,
    pub pool: PoolStatus,
    pub vms: Vec<VmView>,
    pub metrics: DeploymentMetrics,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PoolStatus {
    pub desired_replicas: u32,
    pub ready: usize,
    pub draining: usize,
    pub pending: usize,
    pub total_in_flight: usize,
    pub target_concurrency: u32,
    pub min_replicas: u32,
    pub max_replicas: u32,
    pub warm_pool: u32,
    /// `None` when there is no available capacity to divide by — rendered as
    /// "—", never as a fake 0%.
    pub utilization: Option<f64>,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct VmView {
    pub sandbox_id: String,
    pub addr: String,
    pub in_flight: usize,
    pub healthy: bool,
    pub draining: bool,
    pub uptime_secs: u64,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

impl VmView {
    pub fn status(&self) -> &'static str {
        if self.draining {
            "Draining"
        } else if self.healthy {
            "Ready"
        } else {
            "NotReady"
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeploymentMetrics {
    pub requests: StatusCounts,
    pub latency_ms: Histogram,
    pub cold_start_s: Histogram,
    pub autoscale: AutoscaleCounts,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StatusCounts {
    pub total: u64,
    pub c2xx: u64,
    pub c3xx: u64,
    pub c4xx: u64,
    pub c5xx: u64,
    pub errors: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Histogram {
    pub count: u64,
    pub sum: u64,
    pub mean: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AutoscaleCounts {
    pub vms_created: u64,
    pub vms_drained: u64,
    pub vms_reaped: u64,
    pub scale_up_events: u64,
    pub scale_down_events: u64,
    pub cold_start_waits: u64,
    pub cold_start_hits: u64,
    pub cold_start_timeouts: u64,
}

// -- GET /certs ------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CertStatus {
    pub host: String,
    pub not_after: String,
    pub issuer: String,
    pub needs_renewal: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deployment_parses_from_a_partial_body() {
        // Everything defaults, so a server that grows a field — or drops one
        // this build knows about — still renders.
        let d: DeploymentStatus = serde_json::from_str(r#"{"spec":{"id":"web"},"ready":2}"#).unwrap();
        assert_eq!(d.spec.id, "web");
        assert_eq!(d.ready, 2);
        assert_eq!(d.desired_replicas, 0);
    }

    #[test]
    fn routes_render_with_their_matching_semantics() {
        let exact = RouteRule {
            host: Some("secrets.local".into()),
            ..Default::default()
        };
        assert_eq!(exact.render(), "secrets.local");

        let wild = RouteRule {
            host_suffix: Some(".apps.example.com".into()),
            path_prefix: Some("/api".into()),
            ..Default::default()
        };
        assert_eq!(wild.render(), "*.apps.example.com/api");

        let path_only = RouteRule {
            path_prefix: Some("/legacy".into()),
            ..Default::default()
        };
        assert_eq!(path_only.render(), "*/legacy");

        assert_eq!(RouteRule::default().render(), "<empty>");
    }

    #[test]
    fn many_routes_collapse_to_a_summary() {
        let spec = DeploymentSpec {
            routes: (0..5)
                .map(|i| RouteRule {
                    host: Some(format!("h{i}.local")),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(spec.routes_summary(), "h0.local,h1.local,h2.local +2 more");
    }

    #[test]
    fn vm_status_puts_draining_ahead_of_health() {
        let vm = VmStatus {
            healthy: true,
            draining: true,
            ..Default::default()
        };
        assert_eq!(vm.status(), "Draining");
    }
}
