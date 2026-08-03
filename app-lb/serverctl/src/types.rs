//! Read-side views of the admin API's JSON.
//!
//! Deliberately lenient: every field defaults, so a client a version behind the
//! app-lb it is talking to still renders what it understands instead of failing
//! to parse. Writes go through `serde_json::Value` so nothing is dropped on a
//! round trip — see [`crate::api`].
//!
//! # `extra`, and why leniency needed a counterweight
//!
//! Leniency alone is how these types silently fell behind: to a defaulting
//! deserializer an unknown field and an absent one are the same thing, so
//! `DeploymentView::urls`, `PoolStatus::boot_timeout_secs`,
//! `MetricsResponse::matched` and two more went missing without a single test
//! failing.
//!
//! Every response type therefore carries a `#[serde(flatten)] extra` map. It
//! keeps the leniency — an unknown field parses fine, and is *reachable* rather
//! than discarded — and it makes the gap visible: the tests in
//! `tests/wire_contract.rs` read `testdata/wire/*.json`, written by app-lb's own
//! response types, and assert `extra` is empty. A field this crate stops
//! understanding fails a test instead of blanking a column.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// Fields the server sent that this build has no name for.
///
/// Empty in a matched pair of versions. Non-empty means app-lb is ahead, and
/// what is in here is exactly what this crate is not yet reading.
pub type Extra = serde_json::Map<String, Value>;

// -- GET /deployments, GET /deployments/:id --------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeploymentStatus {
    pub spec: DeploymentSpec,
    /// `"vm"` (managed pool), `"static"` (fixed proxy_pass upstreams) or
    /// `"site"` (files served off disk).
    pub kind: String,
    pub desired_replicas: u32,
    pub ready: usize,
    pub pending: usize,
    pub total_in_flight: usize,
    pub vms: Vec<VmStatus>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct VmStatus {
    pub sandbox_id: String,
    pub addr: String,
    pub in_flight: usize,
    pub healthy: bool,
    pub draining: bool,
    #[serde(flatten)]
    pub extra: Extra,
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
    pub artifact: Option<ArtifactSpec>,
    pub update: Option<UpdateSpec>,
    pub auth: Option<AuthGate>,
    pub site: Option<SiteSpec>,
}

/// A static site: a directory on the app-lb host, served straight off disk.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SiteSpec {
    pub root: String,
    pub index: String,
    pub not_found: Option<String>,
    pub spa: bool,
    pub cache_control: String,
}

impl DeploymentSpec {
    /// Forwards to fixed upstreams. **Not** true for a site, which has no
    /// backends of any kind.
    pub fn is_static(&self) -> bool {
        !self.upstreams.is_empty()
    }

    /// Serves files off disk rather than proxying anywhere.
    pub fn is_site(&self) -> bool {
        self.site.is_some()
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
    /// upstream list for a static one, the directory for a site.
    pub fn backend_summary(&self) -> String {
        if let Some(site) = &self.site {
            return site.root.clone();
        }
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

/// Where a managed deployment's image is pulled from. The other image source,
/// and mutually exclusive with [`BuildSpec`].
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ArtifactSpec {
    pub store: String,
    #[serde(rename = "ref")]
    pub artifact_ref: String,
    pub image_name: Option<String>,
    pub grow_gb: Option<u64>,
    pub auth: Option<SecretRef>,
}

impl ArtifactSpec {
    /// One column: `10.0.0.4:8080/web-v2`. Shaped like [`BuildSpec::summary`] on
    /// purpose — the two share the SOURCE column, and a reader scanning a fleet
    /// should be able to tell a repo from a store at a glance without the
    /// column changing format underneath them.
    pub fn summary(&self) -> String {
        let store = self
            .store
            .trim_end_matches('/')
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        format!("{store}/{}", self.artifact_ref)
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
    /// One provider (`"google"`) or several (`["google", "app-token"]`). A
    /// `Value` rather than a `String` because the server serializes a
    /// single-provider gate as a bare string and a multi-provider one as an
    /// array — modelling only the first would fail to parse the second, and
    /// with `#[serde(default)]` that failure would look like an *absent* gate.
    pub provider: Value,
    /// Required for `google`, absent on a token-only gate.
    pub client_id: Option<String>,
    pub client_secret: Option<SecretRef>,
    pub allowed_domains: Vec<String>,
    pub allowed_emails: Vec<String>,
    pub public_paths: Vec<String>,
    pub base_path: String,
    pub session_ttl_secs: u64,
    pub cookie_name: String,
    /// Set to share one sign-in across every deployment under a parent domain;
    /// `None` is a per-host session.
    pub cookie_domain: Option<String>,
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

    /// The providers this gate accepts, however the server spelled them.
    ///
    /// Empty in the payload means Google, which is what an `auth` block written
    /// before app-tokens existed says by omission.
    pub fn providers(&self) -> Vec<String> {
        match &self.provider {
            Value::String(s) if !s.is_empty() => vec![s.clone()],
            Value::Array(items) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => vec!["google".to_string()],
        }
    }

    /// Whether a program can get past this gate with an app-token.
    pub fn accepts_app_token(&self) -> bool {
        self.providers().iter().any(|p| p == "app-token")
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
    #[serde(flatten)]
    pub extra: Extra,
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

    // artifact-pull
    pub store: Option<String>,
    /// What was asked for: a tag or a digest. Spelled `artifact` on the wire so
    /// it does not collide with a build's `ref`.
    #[serde(rename = "artifact")]
    pub artifact_ref: Option<String>,
    /// What it resolved to — the pull's answer to "which bytes are live?".
    pub digest: Option<String>,
    /// Bytes transferred. `0` with `reused` is a skipped fetch, not a no-op job;
    /// `0` without it is a local store hardlinking the blob rather than copying.
    pub bytes: Option<u64>,
    pub reused: bool,
    /// Set only when the pull was a *site* pull — a bundle unpacked into this
    /// directory rather than a rootfs written to an image.
    pub site_root: Option<String>,
    /// Regular files unpacked, for the same kind of pull. The answer `bytes`
    /// cannot give when the blob was hardlinked and the transfer was free.
    pub files: Option<usize>,

    // host-update
    pub working_dir: Option<String>,
    pub commands_total: Option<usize>,
    pub commands_run: Option<usize>,
    pub verified: Option<bool>,

    pub error: Option<String>,
    pub log: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
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

    /// Whether this job pulled its image from an artifact store rather than
    /// building it. The two produce the same thing — a new `vm.image` and a
    /// recycled pool — but describe it with different fields, so every renderer
    /// has to tell them apart.
    pub fn is_pull(&self) -> bool {
        self.kind == "artifact-pull"
    }

    /// The commit, short enough for a column.
    pub fn short_commit(&self) -> String {
        match &self.commit {
            Some(c) => c.chars().take(12).collect(),
            None => "—".into(),
        }
    }

    /// The resolved digest, short enough for a column. The pull's counterpart of
    /// [`short_commit`](Self::short_commit).
    pub fn short_digest(&self) -> String {
        match &self.digest {
            Some(d) => d.chars().take(12).collect(),
            None => "—".into(),
        }
    }

    /// What this job produced, as one column: the image for either image
    /// source, how far the commands got for an update.
    pub fn result_summary(&self) -> String {
        if self.is_update() {
            return match (self.commands_run, self.commands_total) {
                (Some(run), Some(total)) => format!("{run}/{total} commands"),
                _ => "—".into(),
            };
        }
        self.image.clone().unwrap_or_else(|| "—".into())
    }

    /// What it was asked to act on: a git ref for a build, a store reference for
    /// a pull, the directory for an update.
    pub fn target_summary(&self) -> String {
        if self.is_update() {
            return self.working_dir.clone().unwrap_or_else(|| "—".into());
        }
        if self.is_pull() {
            return self.artifact_ref.clone().unwrap_or_else(|| "—".into());
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
    /// `destroy` or `retain` — what becomes of a VM the autoscaler retires.
    /// A `String` rather than an enum so a value this build has not heard of
    /// displays as-is instead of failing the whole read.
    pub idle_action: String,
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
    /// Absent when the LB has security monitoring off (`APP_LB_SIEM=0`).
    pub security: Option<SecuritySummary>,
    pub deployments: Vec<DeploymentView>,
    /// How many deployments matched before `limit`/`offset`, so a caller can
    /// page without guessing.
    pub matched: usize,
    /// How many deployments hold their own counters. Climbing past the number
    /// registered means retirement is not keeping up.
    pub tracked_deployments: usize,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Alert counts from the detection engine, carried on `/metrics` so a status
/// display can show one without a second request. The alerts themselves are on
/// `GET /security`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SecuritySummary {
    /// Alerts currently held in the LB's in-memory ring.
    pub open: usize,
    /// How many of those are `high` or `critical`.
    pub urgent: u64,
    /// Observations dropped because the analysis queue was full. Non-zero means
    /// detection is sampling rather than complete — the same failure mode, and
    /// the same warning, as [`ObsStats::dropped`].
    pub dropped: u64,
    /// Whether the per-source table is full, which means the same for addresses
    /// as `dropped` does for events.
    pub clients_at_capacity: bool,
    /// Guard rules the data plane is enforcing, and how many requests they have
    /// refused. Reported beside the alert counts because "we are blocking
    /// traffic" belongs next to "we are seeing attacks".
    pub rules: usize,
    pub blocked: u64,
    #[serde(flatten)]
    pub extra: Extra,
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
    #[serde(flatten)]
    pub extra: Extra,
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
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FleetPool {
    pub deployments: usize,
    pub ready: usize,
    pub draining: usize,
    pub pending: usize,
    pub total_in_flight: usize,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeploymentView {
    pub id: String,
    /// `"vm"`, `"static"` or `"site"`.
    pub kind: String,
    pub upstreams: Vec<String>,
    /// Exact hostnames this deployment is routed on — `host` rules only, since
    /// a `host_suffix` names no single certificate subject.
    pub hosts: Vec<String>,
    /// The same routes as URLs, with the *data plane's* scheme and port. Built
    /// server-side because the admin listener knows neither.
    pub urls: Vec<String>,
    /// For a site, the directory it serves. Absent for every other kind.
    pub site_root: Option<String>,
    pub site_spa: bool,
    /// `"build"` or `"update"` — which deploy job this deployment accepts, if
    /// either.
    pub job_kind: Option<String>,
    pub pool: PoolStatus,
    pub vms: Vec<VmView>,
    /// Booting VMs, oldest first — the ones holding a cold start open. A count
    /// alone cannot tell a 3-second boot from a guest that has been failing its
    /// health check for six minutes.
    pub pending_vms: Vec<PendingVmView>,
    pub metrics: DeploymentMetrics,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A VM created but not yet in the pool.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PendingVmView {
    pub sandbox_id: String,
    /// Seconds since the daemon accepted the create call.
    pub age_secs: u64,
    /// The daemon's last reported status, absent before the first observation:
    /// `provisioning`, `running`, `stopped`, `paused`, `failed`, `cold-stored`
    /// or `unknown`.
    pub status: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
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
    /// How long a booting VM gets before the autoscaler kills it; `0` waits
    /// indefinitely.
    pub boot_timeout_secs: u64,
    /// How long a request waits on a cold start. A pending VM older than this
    /// has already cost somebody a 503.
    pub cold_start_timeout_secs: u64,
    #[serde(flatten)]
    pub extra: Extra,
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
    #[serde(flatten)]
    pub extra: Extra,
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
    #[serde(flatten)]
    pub extra: Extra,
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
    #[serde(flatten)]
    pub extra: Extra,
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
    #[serde(flatten)]
    pub extra: Extra,
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
    #[serde(flatten)]
    pub extra: Extra,
}

// -- GET /certs ------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CertStatus {
    pub host: String,
    pub not_after: String,
    pub issuer: String,
    pub needs_renewal: bool,
    #[serde(flatten)]
    pub extra: Extra,
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
    fn an_artifact_source_renders_as_one_column_shaped_like_a_build_one() {
        let a = ArtifactSpec {
            store: "http://10.0.0.4:8080/".into(),
            artifact_ref: "web-v2".into(),
            ..Default::default()
        };
        assert_eq!(a.summary(), "10.0.0.4:8080/web-v2");

        // A store root keeps its leading slash — it is a path, and stripping it
        // would make an absolute one look relative in the SOURCE column.
        let local = ArtifactSpec {
            store: "/srv/artifacts".into(),
            artifact_ref: "debian-hermes".into(),
            ..Default::default()
        };
        assert_eq!(local.summary(), "/srv/artifacts/debian-hermes");
    }

    #[test]
    fn a_pull_record_summarizes_the_store_side_fields_not_the_git_ones() {
        let pull: JobRecord = serde_json::from_str(
            r#"{"id":"job-3","kind":"artifact-pull","status":"succeeded","started_at":100,
                "store":"http://127.0.0.1:8080","artifact":"debian-hermes",
                "digest":"c74abee2ce8409f1aaaa","image":"web-c74abee2ce84",
                "bytes":609222656,"rolled_out":true}"#,
        )
        .unwrap();
        assert!(pull.is_pull());
        assert!(!pull.is_update());
        // The reference asked for, not a git ref it does not have.
        assert_eq!(pull.target_summary(), "debian-hermes");
        assert_eq!(pull.result_summary(), "web-c74abee2ce84");
        assert_eq!(pull.short_digest(), "c74abee2ce84");
        assert_eq!(pull.short_commit(), "—", "a pull has no commit");
        assert!(!pull.reused);
    }

    #[test]
    fn a_reused_image_is_distinguishable_from_a_pull_that_never_ran() {
        let reused: JobRecord = serde_json::from_str(
            r#"{"id":"job-4","kind":"artifact-pull","status":"succeeded","started_at":1,
                "bytes":0,"reused":true}"#,
        )
        .unwrap();
        assert!(reused.reused);
        assert_eq!(reused.bytes, Some(0));

        // A record with neither is one that failed before it got that far.
        let failed: JobRecord = serde_json::from_str(
            r#"{"id":"job-5","kind":"artifact-pull","status":"failed","started_at":1}"#,
        )
        .unwrap();
        assert!(!failed.reused);
        assert_eq!(failed.bytes, None);
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

// -- POST /deployments/:id/exec --------------------------------------------

/// What a command did. A non-zero `exit_code` is a successful *request* — the
/// command ran and failed, which is not the same as being unable to run it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ExecOutput {
    /// Which VM ran it. Worth having even for a single-VM sandbox: after a
    /// resume or a rebuild it is a different sandbox than last time.
    pub sandbox_id: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// stdout and stderr interleaved as the guest wrote them. Not
    /// `stdout + stderr` — the only faithful rendering of interleaved output.
    pub output: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ExecOutput {
    /// Whether the command itself succeeded.
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

// -- DELETE /deployments/:id/vms/:sandbox_id -------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EvictOutcome {
    pub sandbox_id: String,
    /// `"killed"` (immediate) or `"draining"` (still serving what it has).
    pub outcome: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl EvictOutcome {
    pub fn is_draining(&self) -> bool {
        self.outcome == "draining"
    }
}

// -- app-tokens -------------------------------------------------------------

/// What a token may do on the admin API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AdminScope {
    /// Nothing. Still usable against a deployment's own gate, which is the
    /// point of a token handed to an application.
    #[default]
    None,
    /// `/metrics` and `/dashboard`.
    View,
    /// Everything, within the token's `deployments` scope.
    Admin,
}

impl std::fmt::Display for AdminScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::View => "view",
            Self::Admin => "admin",
        })
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct TokenSummary {
    pub id: String,
    pub name: String,
    pub admin: AdminScope,
    /// Deployment ids, or `["*"]` for all of them.
    pub deployments: Vec<String>,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    /// `None` also means "not used since the store was last written" — the
    /// stamp is flushed opportunistically, not per request, so a busy token can
    /// still read as unused.
    pub last_used_at: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl TokenSummary {
    /// Whether this token's scope covers the whole fleet.
    pub fn covers_fleet(&self) -> bool {
        self.deployments.iter().any(|d| d == "*")
    }

    pub fn allows(&self, deployment: &str) -> bool {
        self.deployments.iter().any(|d| d == "*" || d == deployment)
    }
}

/// The reply to a mint. **`token` is the only time the secret is ever
/// returned** — app-lb stores only its hash, and there is no endpoint that
/// reads it back. Store it here or mint another one.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct MintedToken {
    #[serde(flatten)]
    pub summary: TokenSummary,
    pub token: String,
}
