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
    pub build: Option<BuildSpec>,
    pub update: Option<UpdateSpec>,
    pub auth: Option<AuthGate>,
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

/// Where a managed deployment's image is built from.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct BuildSpec {
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    pub image_name: Option<String>,
    pub image_size_mb: Option<u64>,
    pub auth: Option<SecretRef>,
}

impl BuildSpec {
    /// One column: `github.com/acme/web@main`.
    pub fn summary(&self) -> String {
        let repo = self
            .repo
            .trim_end_matches(".git")
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("ssh://");
        match &self.git_ref {
            Some(r) => format!("{repo}@{r}"),
            None => format!("{repo}@(default branch)"),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SecretRef {
    pub secret: String,
    pub key: String,
    pub username: Option<String>,
}

impl SecretRef {
    pub fn render(&self) -> String {
        format!("{}/{}", self.secret, self.key)
    }
}

/// Where a static deployment's backend is updated: a directory on the app-lb
/// host, and commands to run in it.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct UpdateSpec {
    pub working_dir: String,
    pub commands: Vec<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub env_from: Vec<SecretEnv>,
    pub auth: Option<SecretRef>,
    pub timeout_secs: Option<u64>,
    pub verify_timeout_secs: Option<u64>,
}

impl UpdateSpec {
    /// One column: `/srv/app-obs (3 commands)`.
    pub fn summary(&self) -> String {
        format!(
            "{} ({} command{})",
            self.working_dir,
            self.commands.len(),
            if self.commands.len() == 1 { "" } else { "s" }
        )
    }
}

/// An optional sign-in gate in front of a deployment.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct AuthGate {
    pub provider: String,
    pub client_id: String,
    pub client_secret: SecretRef,
    pub allowed_domains: Vec<String>,
    pub allowed_emails: Vec<String>,
    pub public_paths: Vec<String>,
    pub base_path: String,
    pub session_ttl_secs: u64,
    pub cookie_name: String,
    pub redirect_url: Option<String>,
    pub forward_identity: bool,
}

impl AuthGate {
    /// Who may enter, as one line.
    pub fn allow_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.allowed_domains.iter().any(|d| d == "*") {
            parts.push("any Google account".into());
        } else {
            parts.extend(self.allowed_domains.iter().map(|d| format!("@{d}")));
        }
        parts.extend(self.allowed_emails.iter().cloned());
        if parts.is_empty() {
            "<nobody>".into()
        } else {
            parts.join(", ")
        }
    }

    /// The URL that has to be registered with the provider, given a hostname.
    pub fn callback_url(&self, host: &str) -> String {
        format!(
            "https://{host}{}/callback",
            self.base_path.trim_end_matches('/')
        )
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SecretEnv {
    pub secret: String,
    pub key: String,
    #[serde(rename = "as")]
    pub env: Option<String>,
}

impl SecretEnv {
    /// The variable the command sees. Mirrors the server's default of the
    /// upper-cased key.
    pub fn env_name(&self) -> String {
        self.env
            .clone()
            .unwrap_or_else(|| self.key.to_ascii_uppercase())
    }

    pub fn render(&self) -> String {
        format!("{}={}/{}", self.env_name(), self.secret, self.key)
    }
}

// -- GET /secrets ----------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SecretSummary {
    pub id: String,
    pub description: Option<String>,
    /// Key names only. app-lb has no endpoint that returns a value.
    pub keys: Vec<String>,
    pub updated_at: u64,
    pub encrypted_at_rest: bool,
}

// -- GET /jobs, POST /deployments/:id/{build,update} -----------------------

/// One deploy job. Two kinds share the type, and the fields the other kind uses
/// are simply absent — `image-build` carries `image`/`commit`, `host-update`
/// carries `working_dir`/`commands_*`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct JobRecord {
    pub id: String,
    pub deployment: String,
    /// `image-build` or `host-update`.
    pub kind: String,
    /// `running`, `succeeded` or `failed`.
    pub status: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,

    // image-build
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub commit: Option<String>,
    pub dockerfile: Option<String>,
    pub image: Option<String>,
    pub rolled_out: bool,

    // host-update
    pub working_dir: Option<String>,
    pub commands_total: Option<usize>,
    pub commands_run: Option<usize>,
    pub verified: Option<bool>,

    pub error: Option<String>,
    pub log: Vec<String>,
}

impl JobRecord {
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }

    pub fn succeeded(&self) -> bool {
        self.status == "succeeded"
    }

    pub fn is_update(&self) -> bool {
        self.kind == "host-update"
    }

    /// The commit, short enough for a column.
    pub fn short_commit(&self) -> String {
        match &self.commit {
            Some(c) => c.chars().take(12).collect(),
            None => "—".into(),
        }
    }

    /// What this job produced, as one column: the image for a build, how far
    /// the commands got for an update.
    pub fn result_summary(&self) -> String {
        if self.is_update() {
            return match (self.commands_run, self.commands_total) {
                (Some(run), Some(total)) => format!("{run}/{total} commands"),
                _ => "—".into(),
            };
        }
        self.image.clone().unwrap_or_else(|| "—".into())
    }

    /// What it was asked to act on: a git ref for a build, the directory for an
    /// update.
    pub fn target_summary(&self) -> String {
        if self.is_update() {
            return self.working_dir.clone().unwrap_or_else(|| "—".into());
        }
        self.git_ref.clone().unwrap_or_else(|| "(default)".into())
    }

    /// How long it ran (or has been running), given the server's clock.
    pub fn elapsed_secs(&self, now: u64) -> u64 {
        self.finished_at
            .unwrap_or(now)
            .saturating_sub(self.started_at)
    }
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
    pub boot_timeout_secs: u64,
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
    /// Absent when the LB is not shipping logs to app-obs.
    pub obs: Option<ObsStats>,
    pub deployments: Vec<DeploymentView>,
}

/// Log-shipping counters. Worth surfacing because the pipeline drops rather than
/// blocks by design, and `dropped` is the only trace a lost record leaves.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ObsStats {
    pub queued: u64,
    pub dropped: u64,
    pub shipped: u64,
    pub failed: u64,
    pub healthy: bool,
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
    fn a_build_source_renders_as_one_column() {
        let b = BuildSpec {
            repo: "https://github.com/acme/web.git".into(),
            git_ref: Some("main".into()),
            ..Default::default()
        };
        assert_eq!(b.summary(), "github.com/acme/web@main");

        let default_branch = BuildSpec {
            repo: "git@github.com:acme/web.git".into(),
            ..Default::default()
        };
        assert_eq!(
            default_branch.summary(),
            "git@github.com:acme/web@(default branch)"
        );
    }

    #[test]
    fn a_job_record_parses_from_a_partial_body() {
        let r: JobRecord =
            serde_json::from_str(r#"{"id":"job-1","status":"running","started_at":100}"#).unwrap();
        assert!(r.is_running());
        assert!(!r.succeeded());
        assert_eq!(r.short_commit(), "—");
        assert_eq!(r.elapsed_secs(130), 30, "a running job measures against now");

        let done: JobRecord = serde_json::from_str(
            r#"{"id":"job-1","kind":"image-build","status":"succeeded","started_at":100,
                "finished_at":160,"commit":"0123456789abcdef","ref":"main","image":"web-0123"}"#,
        )
        .unwrap();
        assert!(done.succeeded());
        assert_eq!(done.short_commit(), "0123456789ab");
        assert_eq!(done.elapsed_secs(999), 60, "a finished one measures its own span");
        assert_eq!(done.target_summary(), "main");
        assert_eq!(done.result_summary(), "web-0123");
    }

    /// The two kinds share the type, so the columns have to mean the right thing
    /// for each: a host update has no image and no commit.
    #[test]
    fn an_update_record_summarizes_its_own_fields() {
        let update: JobRecord = serde_json::from_str(
            r#"{"id":"job-2","kind":"host-update","status":"failed","started_at":100,
                "working_dir":"/srv/app-obs","commands_total":3,"commands_run":1,
                "verified":false}"#,
        )
        .unwrap();
        assert!(update.is_update());
        assert_eq!(update.target_summary(), "/srv/app-obs");
        assert_eq!(update.result_summary(), "1/3 commands");
        assert_eq!(update.verified, Some(false));
    }

    #[test]
    fn an_update_source_renders_its_directory_and_command_count() {
        let u = UpdateSpec {
            working_dir: "/srv/app-obs".into(),
            commands: vec!["git pull".into(), "cargo build --release".into()],
            ..Default::default()
        };
        assert_eq!(u.summary(), "/srv/app-obs (2 commands)");

        let one = UpdateSpec {
            working_dir: "/srv/x".into(),
            commands: vec!["make deploy".into()],
            ..Default::default()
        };
        assert_eq!(one.summary(), "/srv/x (1 command)");
    }

    #[test]
    fn a_gate_says_who_may_enter_and_where_the_provider_redirects() {
        let g = AuthGate {
            allowed_domains: vec!["example.com".into()],
            allowed_emails: vec!["contractor@gmail.com".into()],
            base_path: "/__applb/auth".into(),
            ..Default::default()
        };
        assert_eq!(g.allow_summary(), "@example.com, contractor@gmail.com");
        assert_eq!(
            g.callback_url("app.example.com"),
            "https://app.example.com/__applb/auth/callback"
        );

        // The escape hatch reads as what it is, not as a literal `@*`.
        let any = AuthGate {
            allowed_domains: vec!["*".into()],
            ..Default::default()
        };
        assert_eq!(any.allow_summary(), "any Google account");

        // A server a version ahead could send a gate this build understands
        // nothing about; it must still render rather than fail to parse.
        let unknown: AuthGate = serde_json::from_str(r#"{"provider":"okta","client_id":"x"}"#).unwrap();
        assert_eq!(unknown.provider, "okta");
        assert_eq!(unknown.allow_summary(), "<nobody>");
    }

    #[test]
    fn a_secret_env_defaults_to_the_upper_cased_key() {
        let e = SecretEnv {
            secret: "obs".into(),
            key: "ingest_token".into(),
            env: None,
        };
        assert_eq!(e.env_name(), "INGEST_TOKEN");
        assert_eq!(e.render(), "INGEST_TOKEN=obs/ingest_token");

        let renamed = SecretEnv {
            env: Some("APP_OBS_INGEST_TOKEN".into()),
            ..e
        };
        assert_eq!(renamed.render(), "APP_OBS_INGEST_TOKEN=obs/ingest_token");
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
