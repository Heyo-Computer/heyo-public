//! The control plane.
//!
//! Runs as a pingora `BackgroundService` so it shares the server's lifecycle and
//! graceful shutdown. It binds its own listener rather than using a pingora
//! listening service, which trades away zero-downtime socket handoff for that
//! port in exchange for real routing — an acceptable deal for an admin API.

use crate::autoscale::{Autoscaler, EvictOutcome};
use crate::config::DeploymentSpec;
use crate::jobs::{Jobs, StartError};
use crate::deployment::now_secs;
use crate::metrics::{DeploymentMetricsSnapshot, HostUsageSnapshot, Metrics};
use crate::registry::Registry;
use crate::secrets::{SecretSpec, SecretStore};
use crate::tls::CertStore;
use async_trait::async_trait;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use base64::Engine;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Notify;

/// The live dashboard page. Self-contained (no external fetches beyond the
/// same-origin `/metrics` poll) so it works over an SSH tunnel with no assets.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// How to turn a deployment's hostname into a URL somebody can click.
///
/// The dashboard runs on the *admin* listener, so it cannot infer the data
/// plane's scheme or port from its own location — an app-lb serving HTTPS on
/// 6189 would otherwise be linked as `http://host`, which connects to nothing.
#[derive(Debug, Clone)]
pub struct PublicUrl {
    scheme: &'static str,
    /// Appended as `:port`, unless it is the default for the scheme.
    port: Option<u16>,
}

impl PublicUrl {
    /// Derived from the listener config: the HTTPS listener when TLS is on
    /// (that is where a browser should land), the plaintext one otherwise.
    pub fn from_config(tls_enabled: bool, proxy_addr: &str, tls_addr: &str) -> Self {
        let (scheme, addr, default) = if tls_enabled {
            ("https", tls_addr, 443)
        } else {
            ("http", proxy_addr, 80)
        };
        Self {
            scheme,
            port: port_of(addr).filter(|p| *p != default),
        }
    }

    /// The URL for one route rule, or `None` if it names no host.
    ///
    /// A rule with only a `path_prefix` or a `host_suffix` is deliberately not
    /// linkable: neither names a single hostname a browser could be sent to.
    fn of(&self, rule: &crate::config::RouteRule) -> Option<String> {
        let host = rule.host.as_deref()?.trim();
        if host.is_empty() {
            return None;
        }
        let mut url = format!("{}://{host}", self.scheme);
        if let Some(port) = self.port {
            url.push_str(&format!(":{port}"));
        }
        // A host+path rule only matches under that prefix, so linking the bare
        // host would land on a 404 from this very deployment.
        if let Some(path) = &rule.path_prefix {
            url.push_str(path);
        }
        Some(url)
    }
}

/// The port from a `host:port` listen address, including `[::]:port`.
fn port_of(addr: &str) -> Option<u16> {
    let tail = match addr.rfind(']') {
        Some(end) => addr.get(end + 1..)?.strip_prefix(':')?,
        None => addr.rsplit_once(':')?.1,
    };
    tail.parse().ok()
}

/// The optional Basic-auth gate over the dashboard and `/metrics`.
///
/// Credentials are collapsed to the exact `Authorization` header they must
/// produce, computed once at startup, so verifying a request is a single
/// constant-time byte comparison — no per-request base64 decode, and no branch
/// on where the first mismatch is.
struct DashboardAuth {
    expected_header: String,
}

impl DashboardAuth {
    fn new(user: &str, password: &str) -> Self {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        Self {
            expected_header: format!("Basic {token}"),
        }
    }

    fn accepts(&self, header_value: Option<&str>) -> bool {
        header_value.is_some_and(|got| ct_eq(got.as_bytes(), self.expected_header.as_bytes()))
    }
}

/// Length-then-content comparison that doesn't short-circuit on the first
/// differing byte, so a matching prefix can't be timed out of the credential.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// HTML-escape a value bound for element text / a `<title>` — the display name
/// comes from an env var, so escape it rather than trusting it into markup.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Clone)]
struct AdminState {
    registry: Arc<Registry>,
    autoscaler: Arc<Autoscaler>,
    metrics: Arc<Metrics>,
    /// The dashboard page with the display name substituted in, rendered once at
    /// startup. `Arc<str>` so cloning `AdminState` per request is a refcount bump.
    dashboard_html: Arc<str>,
    /// `None` disables the gate — the dashboard and `/metrics` are then open.
    auth: Option<Arc<DashboardAuth>>,
    /// When true, the gate also covers the deployment CRUD routes (reflected in
    /// `router`), so mutations and spec reads require the same credentials.
    gate_admin: bool,
    /// Process start (LB clock), so the dashboard can show how long the numbers
    /// have been accumulating.
    started_at: u64,
    /// Issued certificates, for `GET /certs`.
    certs: Arc<CertStore>,
    /// Stored secrets. Values enter through this API and never leave it.
    secrets: Arc<SecretStore>,
    /// Runs image builds and host updates, and remembers what they did.
    jobs: Arc<Jobs>,
    /// Nudges the ACME manager to issue for a newly-registered hostname instead
    /// of waiting out its sweep interval. `None` when ACME is disabled.
    acme: Option<Arc<Notify>>,
    /// Counters for the app-obs log shipper. `None` when log shipping is off.
    obs: Option<Arc<crate::obs::Stats>>,
    /// How to turn a deployment's hostname into a link, given where the data
    /// plane actually listens.
    public_url: PublicUrl,
}

impl AdminState {
    /// Ask for an immediate ACME sweep. Issuance is asynchronous — the request
    /// that triggered this returns without waiting for a certificate.
    fn nudge_acme(&self) {
        if let Some(acme) = &self.acme {
            acme.notify_one();
        }
    }
}

pub struct AdminApi {
    addr: String,
    state: AdminState,
}

impl AdminApi {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: String,
        registry: Arc<Registry>,
        autoscaler: Arc<Autoscaler>,
        metrics: Arc<Metrics>,
        name: String,
        dashboard_user: Option<String>,
        dashboard_password: Option<String>,
        gate_admin: bool,
        certs: Arc<CertStore>,
        acme: Option<Arc<Notify>>,
        secrets: Arc<SecretStore>,
        jobs: Arc<Jobs>,
        obs: Option<Arc<crate::obs::Stats>>,
        public_url: PublicUrl,
    ) -> Self {
        // Render the display name into the page once; the placeholder appears in
        // both the <title> and the <h1>.
        let dashboard_html: Arc<str> =
            Arc::from(DASHBOARD_HTML.replace("{{APP_NAME}}", &html_escape(&name)));

        // The gate turns on as soon as a password is set; the username is
        // optional and defaults to "admin", so one env var is enough to secure
        // it and there is no "half-configured, silently open" state.
        let auth = dashboard_password.map(|password| {
            let user = dashboard_user.unwrap_or_else(|| "admin".to_string());
            tracing::info!(user = %user, admin_api = gate_admin, "dashboard auth enabled");
            Arc::new(DashboardAuth::new(&user, &password))
        });
        if auth.is_none() {
            tracing::info!("dashboard auth disabled (set APP_LB_DASHBOARD_PASSWORD to enable)");
        }
        // main() rejects gate_admin without a password, so this can't be a
        // silently-open state; assert the invariant in case that check moves.
        debug_assert!(!gate_admin || auth.is_some(), "admin gate needs credentials");

        Self {
            addr,
            state: AdminState {
                registry,
                autoscaler,
                metrics,
                dashboard_html,
                auth,
                gate_admin,
                started_at: now_secs(),
                certs,
                acme,
                secrets,
                jobs,
                obs,
                public_url,
            },
        }
    }
}

/// Gate the protected routes on Basic auth when configured. A `401` carries the
/// `WWW-Authenticate` challenge so a browser shows its native login prompt and
/// caches the credentials for the same-origin requests (`/metrics` polls, and
/// the dashboard's own write buttons when the admin API is gated too).
async fn require_dashboard_auth(
    State(state): State<AdminState>,
    req: Request,
    next: Next,
) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        return next.run(req).await; // gate disabled
    };

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    if auth.accepts(presented) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"app-lb dashboard\", charset=\"UTF-8\"",
            )],
            "authentication required\n",
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct VmStatus {
    sandbox_id: String,
    addr: String,
    in_flight: usize,
    healthy: bool,
    draining: bool,
}

#[derive(Serialize)]
struct DeploymentStatus {
    spec: DeploymentSpec,
    /// `"vm"` (managed pool) or `"static"` (fixed proxy_pass upstreams).
    kind: &'static str,
    desired_replicas: u32,
    ready: usize,
    pending: usize,
    total_in_flight: usize,
    vms: Vec<VmStatus>,
}

/// The backend kind of a deployment, as a stable string for the API/dashboard.
fn deployment_kind(d: &crate::deployment::Deployment) -> &'static str {
    match d.spec.backend() {
        crate::config::Backend::Site => "site",
        crate::config::Backend::Upstreams => "static",
        crate::config::Backend::Vm => "vm",
    }
}

/// Which `POST` a deployment's code is redeployed with, if either.
///
/// The two are mutually exclusive by validation — there is no image to build for a
/// `proxy_pass` upstream, and no host directory behind a microVM — so this is one
/// answer rather than two flags.
fn job_kind_of(spec: &DeploymentSpec) -> Option<&'static str> {
    if spec.build.is_some() {
        Some("build")
    } else if spec.update.is_some() {
        Some("update")
    } else {
        None
    }
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn err(code: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        code,
        Json(ApiError {
            error: message.into(),
        }),
    )
}

fn status_of(d: &Arc<crate::deployment::Deployment>) -> DeploymentStatus {
    let backends = d.backends();
    DeploymentStatus {
        spec: d.spec.clone(),
        kind: deployment_kind(d),
        desired_replicas: d.desired_replicas(),
        ready: backends.len(),
        pending: d.pending().len(),
        total_in_flight: d.total_in_flight(),
        vms: backends
            .iter()
            .map(|b| VmStatus {
                sandbox_id: b.sandbox_id.clone(),
                addr: b.peer.clone(),
                in_flight: b.in_flight(),
                healthy: b.is_healthy(),
                draining: b.is_draining(),
            })
            .collect(),
    }
}

/// Live pool state for one deployment, as the dashboard shows it. Distinct from
/// the daemon's view: these are the LB's own gauges (in-flight, draining), not
/// anything the daemon reports.
#[derive(Serialize)]
struct PoolStatus {
    desired_replicas: u32,
    /// Total backends in the pool, draining ones included.
    ready: usize,
    /// Backends marked draining (still serving, taking nothing new).
    draining: usize,
    /// Booting VMs not yet routable.
    pending: usize,
    total_in_flight: usize,
    target_concurrency: u32,
    min_replicas: u32,
    max_replicas: u32,
    warm_pool: u32,
    /// Load against capacity: in-flight / (available VMs × target). `None` when
    /// there is no available capacity to divide by (an empty or all-draining
    /// pool), which the dashboard renders as "—" rather than a fake 0%.
    utilization: Option<f64>,
    /// Summed CPU% (percent-of-a-core) and RSS across the pool's VMs, `None`
    /// until the daemon reports usage for at least one of them.
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
    /// How long a booting VM gets before the autoscaler kills it; `0` means it
    /// waits indefinitely. Shown so a pending VM's age can be read against the
    /// deadline it is heading for rather than as a bare number.
    boot_timeout_secs: u64,
    /// How long a request waits on a cold start. The dashboard reads a pending
    /// VM's age against this: past it, the boot has already cost somebody a 503.
    cold_start_timeout_secs: u64,
}

/// A VM that has been created but has not joined the pool.
///
/// Reported because a booting VM is otherwise a *count* only, and a count cannot
/// distinguish "a VM is 3 seconds into a normal boot" from "a VM has been failing
/// its health check for six minutes" — which is the difference between waiting and
/// having a broken guest.
#[derive(Serialize)]
struct PendingVmView {
    sandbox_id: String,
    /// Seconds since the daemon accepted the create call.
    age_secs: u64,
    /// The daemon's last reported status, absent before the first observation.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<heyo_sdk::SandboxStatus>,
}

#[derive(Serialize)]
struct VmView {
    sandbox_id: String,
    addr: String,
    in_flight: usize,
    healthy: bool,
    draining: bool,
    uptime_secs: u64,
    /// Latest per-VM sample from the daemon, `None` if not yet reported.
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
}

#[derive(Serialize)]
struct DeploymentView {
    id: String,
    /// `"vm"` (managed pool) or `"static"` (proxy_pass). The dashboard renders
    /// the two differently — a static deployment hides scaling controls.
    kind: &'static str,
    /// For a static deployment, the configured upstream addresses; empty for a
    /// managed one.
    upstreams: Vec<String>,
    /// Exact hostnames this deployment is routed on — `host` rules only, since a
    /// `host_suffix` names no single certificate subject and a `path_prefix` names
    /// no hostname at all. Reported so the dashboard can say which routed names
    /// have no certificate yet, which is otherwise a join nobody can make from
    /// `/certs` alone.
    hosts: Vec<String>,
    /// The same routes as URLs a browser can be sent to, scheme and non-default
    /// port included. Built here rather than in the page because the dashboard
    /// is served from the *admin* listener and knows nothing about the data
    /// plane's scheme or port — it would guess `http://host` for an app-lb
    /// serving HTTPS on 6189.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    urls: Vec<String>,
    /// For a site, the directory it serves and whether unmatched paths fall back
    /// to the index. Absent for every other kind, so the dashboard can tell the
    /// three apart from the payload alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    site_root: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    site_spa: bool,
    /// The deploy job this deployment accepts — `"build"` for a managed one with a
    /// `build` block, `"update"` for a static one with an `update` block, `None`
    /// when neither is configured and there is nothing to trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    job_kind: Option<&'static str>,
    pool: PoolStatus,
    vms: Vec<VmView>,
    /// Booting VMs, oldest first — the ones holding a cold start open.
    pending_vms: Vec<PendingVmView>,
    metrics: DeploymentMetricsSnapshot,
}

/// A rollup of pool gauges across every deployment, for the top-of-dashboard
/// totals.
#[derive(Serialize)]
struct FleetPool {
    deployments: usize,
    ready: usize,
    draining: usize,
    pending: usize,
    total_in_flight: usize,
}

#[derive(Serialize)]
struct MetricsResponse {
    generated_at: u64,
    uptime_secs: u64,
    /// Whole-host CPU/memory from the daemon.
    host: HostUsageSnapshot,
    fleet: FleetPool,
    /// All deployments' metrics merged. Includes history from deregistered
    /// deployments, so totals don't drop when one is removed.
    global: DeploymentMetricsSnapshot,
    /// Log-shipping counters, absent when it is off. Here because the pipeline
    /// drops rather than blocks by design, and a drop is only visible if
    /// somebody counts it — asking app-obs "are my logs arriving?" cannot
    /// distinguish a quiet deployment from a full queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    obs: Option<crate::obs::ObsSnapshot>,
    /// The slice of deployments this response carries. Scoped by the query
    /// parameters on `MetricsQuery` — at fleet scale the full list is megabytes,
    /// and the dashboard polls it every few seconds.
    deployments: Vec<DeploymentView>,
    /// How many deployments matched before `limit`/`offset`, so a client can
    /// page without guessing.
    matched: usize,
    /// How many deployments currently hold their own counters. Normally equal
    /// to the number registered; a number that climbs past it means retirement
    /// is not keeping up, which is the leak this used to have.
    tracked_deployments: usize,
}

/// Query parameters for `GET /metrics`.
///
/// The unfiltered response used to be the only response. That is fine for a few
/// dozen services and untenable for thousands of sandboxes, where every poll
/// serialises every VM and every histogram. All fields are optional and the
/// defaults reproduce the old behaviour for small fleets.
#[derive(Debug, Default, Deserialize)]
struct MetricsQuery {
    /// Restrict to one deployment by id. The cheap path for "how is this one
    /// sandbox doing", which is the common question about a fleet.
    deployment: Option<String>,
    /// Restrict to deployments whose id starts with this. Sandbox ids are
    /// generated with a common prefix, so this is how a tenant is scoped.
    prefix: Option<String>,
    /// Drop the per-VM detail, keeping pool counts and metrics. The largest
    /// single saving: VM rows dominate the payload for a fleet at rest.
    #[serde(default)]
    summary: bool,
    /// Page size. Absent means no limit, which is what the dashboard sends for
    /// a small fleet.
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
}

fn pool_status_of(d: &Arc<crate::deployment::Deployment>) -> PoolStatus {
    let backends = d.backends();
    let draining = backends.iter().filter(|b| b.is_draining()).count();
    let available = backends.iter().filter(|b| b.is_available()).count();
    let total_in_flight = d.total_in_flight();
    let target = d.spec.scaling.target_concurrency.max(1) as usize;
    let capacity = available * target;
    let utilization = (capacity > 0).then(|| total_in_flight as f64 / capacity as f64);

    // Aggregate resource usage over the VMs the daemon has reported. `None` if
    // none have a sample yet, so the dashboard can distinguish "no data" from 0.
    let samples: Vec<(f64, u64)> = backends.iter().filter_map(|b| b.usage()).collect();
    let (cpu_percent, memory_bytes) = if samples.is_empty() {
        (None, None)
    } else {
        (
            Some(samples.iter().map(|(c, _)| c).sum()),
            Some(samples.iter().map(|(_, m)| m).sum()),
        )
    };

    PoolStatus {
        desired_replicas: d.desired_replicas(),
        ready: backends.len(),
        draining,
        pending: d.pending().len(),
        total_in_flight,
        target_concurrency: d.spec.scaling.target_concurrency,
        min_replicas: d.spec.scaling.min_replicas,
        max_replicas: d.spec.scaling.max_replicas,
        warm_pool: d.spec.scaling.warm_pool,
        utilization,
        cpu_percent,
        memory_bytes,
        boot_timeout_secs: d.spec.scaling.boot_timeout_secs,
        cold_start_timeout_secs: d.spec.scaling.cold_start_timeout_secs,
    }
}

/// The booting VMs of a deployment, oldest first.
fn pending_vms_of(d: &Arc<crate::deployment::Deployment>) -> Vec<PendingVmView> {
    let mut views: Vec<PendingVmView> = d
        .pending()
        .iter()
        .map(|p| PendingVmView {
            sandbox_id: p.sandbox_id.clone(),
            age_secs: p.age_secs(),
            status: p.status.clone(),
        })
        .collect();
    // Oldest first: the one closest to its boot timeout is the one to look at.
    views.sort_by(|a, b| b.age_secs.cmp(&a.age_secs));
    views
}

/// The dashboard's data source: live pool gauges joined with accumulated
/// metrics, per deployment plus a global rollup.
async fn metrics_snapshot(
    State(state): State<AdminState>,
    Query(q): Query<MetricsQuery>,
) -> impl IntoResponse {
    let deployments = state.registry.deployments();

    // The fleet rollup covers the whole registry, never the filter or the page.
    // It sits under "Host & fleet" alongside the global metrics, and a number
    // there that moved when somebody typed in the table's search box would be
    // describing the query rather than the system.
    let mut fleet = FleetPool {
        deployments: deployments.len(),
        ready: 0,
        draining: 0,
        pending: 0,
        total_in_flight: 0,
    };
    for d in deployments.values() {
        let pool = pool_status_of(d);
        fleet.ready += pool.ready;
        fleet.draining += pool.draining;
        fleet.pending += pool.pending;
        fleet.total_in_flight += pool.total_in_flight;
    }

    // Filter and page *before* building views: a view snapshots every VM and
    // every histogram, so the work skipped here is the work that made this
    // endpoint expensive.
    let mut selected: Vec<_> = deployments
        .values()
        .filter(|d| q.deployment.as_ref().is_none_or(|id| &d.spec.id == id))
        .filter(|d| q.prefix.as_ref().is_none_or(|p| d.spec.id.starts_with(p)))
        .collect();
    selected.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));

    let matched = selected.len();
    let page = selected
        .into_iter()
        .skip(q.offset)
        .take(q.limit.unwrap_or(usize::MAX));

    let views: Vec<DeploymentView> = page
        .map(|d| {
            let backends = d.backends();
            DeploymentView {
                id: d.spec.id.clone(),
                kind: deployment_kind(d),
                upstreams: d.spec.upstreams.clone(),
                hosts: d
                    .spec
                    .routes
                    .iter()
                    .filter_map(|r| r.host.clone())
                    .collect(),
                urls: d
                    .spec
                    .routes
                    .iter()
                    .filter_map(|r| state.public_url.of(r))
                    .collect(),
                site_root: d.spec.site.as_ref().map(|s| s.root.clone()),
                site_spa: d.spec.site.as_ref().is_some_and(|s| s.spa),
                job_kind: job_kind_of(&d.spec),
                pool: pool_status_of(d),
                // Skipped under `summary`: one row per VM is what makes this
                // response large, and the pool counts above already say how
                // many there are.
                vms: if q.summary {
                    Vec::new()
                } else {
                    backends
                        .iter()
                        .map(|b| {
                            let usage = b.usage();
                            VmView {
                                sandbox_id: b.sandbox_id.clone(),
                                addr: b.peer.clone(),
                                in_flight: b.in_flight(),
                                healthy: b.is_healthy(),
                                draining: b.is_draining(),
                                uptime_secs: b.uptime_secs(),
                                cpu_percent: usage.map(|(c, _)| c),
                                memory_bytes: usage.map(|(_, m)| m),
                            }
                        })
                        .collect()
                },
                pending_vms: if q.summary { Vec::new() } else { pending_vms_of(d) },
                metrics: state.metrics.deployment_snapshot(&d.spec.id),
            }
        })
        .collect();

    let now = now_secs();
    Json(MetricsResponse {
        generated_at: now,
        uptime_secs: now.saturating_sub(state.started_at),
        host: state.metrics.host_snapshot(),
        fleet,
        global: state.metrics.global_snapshot(),
        obs: state.obs.as_ref().map(|o| o.snapshot()),
        deployments: views,
        matched,
        tracked_deployments: state.metrics.tracked_deployments(),
    })
}

async fn dashboard(State(state): State<AdminState>) -> impl IntoResponse {
    Html(state.dashboard_html.to_string())
}

/// Log the things a spec is allowed to say but probably didn't mean.
///
/// Not `validate`, because none of these make the spec unservable — refusing
/// them would be app-lb deciding it knows better. Logged at the moment the
/// author can still act on it.
fn warn_about(spec: &DeploymentSpec) {
    let Some(vm) = &spec.vm else { return };
    if spec.scaling.idle_action == crate::config::IdleAction::Retain
        && vm.disk_size_gb.unwrap_or(0) == 0
    {
        tracing::warn!(
            deployment = %spec.id,
            "idle_action is `retain` but the VM has no data disk: the daemon recopies the \
             rootfs from the base image on every boot, so a suspended VM keeps nothing. \
             Set `vm.disk_size_gb` and keep state under /workspace, or this only saves \
             boot time",
        );
    }
}

async fn register(
    State(state): State<AdminState>,
    Json(spec): Json<DeploymentSpec>,
) -> impl IntoResponse {
    // Validation is the gate that keeps unroutable VMs (e.g. libvirt, which has
    // no guest_ip) from ever being booted.
    if let Err(e) = spec.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    warn_about(&spec);

    let id = spec.id.clone();
    // Replacing a deployment abandons its old pool; tear it down explicitly so
    // the VMs don't linger until their TTL.
    //
    // The swap happens *first*, for the same reason `deregister` removes before
    // tearing down: while the old deployment is still the registry's, a
    // concurrent autoscaler tick will happily boot VMs into it, and those would
    // be orphaned by the swap that follows. Once it is no longer live the
    // autoscaler stops creating for it and kills anything it created (see
    // `Autoscaler::unclaimed`).
    let old = state.registry.get(&id);
    let deployment = state.registry.upsert(spec);
    if let Some(old) = old {
        state.autoscaler.teardown(&old).await;
    }
    if let Err(e) = state.registry.persist_one(&id) {
        tracing::error!(deployment = %id, error = %e, "failed to persist state");
    }
    tracing::info!(deployment = %id, "registered");

    // Let the autoscaler build the warm pool without waiting for the next tick.
    deployment.scale_signal.notify_one();
    // ...and let ACME start issuing for any new hostname. Asynchronous: this
    // response does not wait for a certificate.
    state.nudge_acme();

    (StatusCode::CREATED, Json(status_of(&deployment))).into_response()
}

/// Edit a deployment in place: `PUT /deployments/:id`.
///
/// The whole spec is replaced (the path id wins, so the body's id can't retarget
/// another deployment). The pool is preserved when the VM *template* is
/// unchanged — a scaling/route/health edit never disturbs running VMs; only a
/// change to the `vm` block reboots them, because the existing VMs were built
/// from the old template.
async fn update(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(mut spec): Json<DeploymentSpec>,
) -> impl IntoResponse {
    spec.id = id.clone();
    if let Err(e) = spec.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    warn_about(&spec);

    let Some(old) = state.registry.get(&id) else {
        return err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response();
    };

    let deployment = if old.spec.vm != spec.vm || old.spec.upstreams != spec.upstreams {
        // The backend set changed — a managed VM *template*, or a static
        // deployment's upstream list (or a switch between the two kinds). The
        // running backends no longer match the spec, so rebuild from scratch
        // (`teardown` is a no-op-that-clears-routing for the static kind).
        //
        // Swap first, tear down second: see the note in `register`.
        tracing::info!(deployment = %id, "updating deployment (backends changed; rebuilding)");
        let deployment = state.registry.upsert(spec);
        state.autoscaler.teardown(&old).await;
        deployment
    } else {
        // Scaling/routes/health only: keep the pool live.
        tracing::info!(deployment = %id, "updating deployment (pool preserved)");
        match state.registry.update(spec) {
            Some(d) => d,
            None => return err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response(),
        }
    };

    if let Err(e) = state.registry.persist_one(&id) {
        tracing::error!(deployment = %id, error = %e, "failed to persist state");
    }
    // Reconcile to the new policy immediately (scale up/down, warm pool).
    deployment.scale_signal.notify_one();
    // An edit can introduce a hostname, so this needs the same nudge as
    // registration.
    state.nudge_acme();

    Json(status_of(&deployment)).into_response()
}

/// Manually scale a deployment: `PATCH /deployments/:id/scaling`.
///
/// The body is a partial `ScalingPolicy` — only the fields present are changed,
/// the rest are kept — so the dashboard can send just `{min_replicas, ...}`
/// without resetting the timeouts it doesn't show. Never touches the VM
/// template, so the pool is always preserved; the autoscaler grows or drains it
/// to match the new policy on the nudge.
async fn scale(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(patch): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(old) = state.registry.get(&id) else {
        return err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response();
    };

    // Only a managed deployment is autoscaled; for the others the scaling policy
    // is inert, so a scale request is a mistake rather than a no-op.
    if !old.spec.is_managed() {
        let fix = if old.spec.is_site() {
            "a site serves files off disk and has nothing to scale"
        } else {
            "edit its `upstreams` via PUT instead"
        };
        return err(
            StatusCode::BAD_REQUEST,
            format!("deployment {id:?} has no VM pool and cannot be scaled; {fix}"),
        )
        .into_response();
    }

    let Some(patch) = patch.as_object() else {
        return err(StatusCode::BAD_REQUEST, "scaling patch must be a JSON object").into_response();
    };

    // Merge the patch onto the current policy, then re-parse so unknown/typed
    // fields are validated by serde.
    let mut merged = match serde_json::to_value(&old.spec.scaling) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return err(StatusCode::INTERNAL_SERVER_ERROR, "could not read scaling policy").into_response(),
    };
    for (k, v) in patch {
        merged.insert(k.clone(), v.clone());
    }
    let scaling: crate::config::ScalingPolicy = match serde_json::from_value(serde_json::Value::Object(merged)) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid scaling policy: {e}")).into_response(),
    };

    let mut spec = old.spec.clone();
    spec.scaling = scaling;
    // Catches min > max, zero target_concurrency, etc.
    if let Err(e) = spec.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    let Some(deployment) = state.registry.update(spec) else {
        return err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response();
    };
    if let Err(e) = state.registry.persist_one(&id) {
        tracing::error!(deployment = %id, error = %e, "failed to persist state");
    }
    tracing::info!(deployment = %id, "scaled");
    deployment.scale_signal.notify_one();

    Json(status_of(&deployment)).into_response()
}

async fn list(State(state): State<AdminState>) -> impl IntoResponse {
    let deployments = state.registry.deployments();
    let mut out: Vec<_> = deployments.values().map(status_of).collect();
    out.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    Json(out)
}

async fn get_one(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.registry.get(&id) {
        Some(d) => Json(status_of(&d)).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response(),
    }
}

async fn deregister(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(d) = state.registry.remove(&id) else {
        return err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response();
    };
    // Removed from routing first, so the teardown can't race new requests in.
    state.autoscaler.teardown(&d).await;
    // Its counters fold into the global rollup and its entry goes; a fleet whose
    // ids churn would otherwise accumulate one per sandbox that ever existed.
    state.metrics.retire(&id);
    if let Err(e) = state.registry.forget(&id) {
        tracing::error!(deployment = %id, error = %e, "failed to drop persisted state");
    }
    tracing::info!(deployment = %id, "deregistered");
    StatusCode::NO_CONTENT.into_response()
}

/// `?force=true` kills the VM now (dropping in-flight); otherwise it is drained.
#[derive(Deserialize)]
struct EvictParams {
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct EvictResponse {
    sandbox_id: String,
    /// `"killed"` (gone now) or `"draining"` (will be reaped once idle).
    outcome: &'static str,
}

/// Evict a single VM from a deployment's pool.
///
/// `DELETE /deployments/:id/vms/:sandbox_id[?force=true]`. The autoscaler boots
/// a replacement on its next tick if the scaling policy still wants the
/// capacity, so this is "recycle this instance", not "shrink the deployment".
async fn evict_vm(
    State(state): State<AdminState>,
    Path((id, sandbox_id)): Path<(String, String)>,
    Query(params): Query<EvictParams>,
) -> impl IntoResponse {
    let Some(d) = state.registry.get(&id) else {
        return err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response();
    };

    // Eviction recycles a VM and lets the autoscaler boot a replacement, which
    // only means something for a managed deployment. A static one's upstreams are
    // addresses and a site has no backends at all.
    if !d.spec.is_managed() {
        return err(
            StatusCode::BAD_REQUEST,
            format!("deployment {id:?} has no VMs to evict — edit the spec instead"),
        )
        .into_response();
    }

    match state.autoscaler.evict(&d, &sandbox_id, params.force).await {
        EvictOutcome::Killed => {
            (StatusCode::OK, Json(EvictResponse { sandbox_id, outcome: "killed" })).into_response()
        }
        // 202: the drain is underway but the VM is not gone yet.
        EvictOutcome::Draining => (
            StatusCode::ACCEPTED,
            Json(EvictResponse { sandbox_id, outcome: "draining" }),
        )
            .into_response(),
        EvictOutcome::NotFound => err(
            StatusCode::NOT_FOUND,
            format!("no VM {sandbox_id:?} in deployment {id:?}"),
        )
        .into_response(),
        EvictOutcome::KillFailed(e) => {
            err(StatusCode::BAD_GATEWAY, format!("failed to evict VM: {e}")).into_response()
        }
    }
}

// --- exec and shell -------------------------------------------------------
//
// The two ways into a VM that are not HTTP. Both matter most for a deployment
// with no routes at all — an agent sandbox — where they are the *only* ways in.
//
// Both go through app-lb rather than handing the caller a daemon address,
// because three things have to happen that only app-lb can do: resolve a
// deployment id to whichever sandbox is currently serving it, apply the admin
// gate, and wake a VM that has been scaled to zero or suspended.

/// How long a command may run before the daemon gives up on it, when the caller
/// names no timeout. Long enough for a build step, short enough that a hung
/// command does not hold an admin connection for the rest of the day.
const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 60;

/// Ceiling on a caller-supplied exec timeout.
const MAX_EXEC_TIMEOUT_SECS: u64 = 3600;

#[derive(Debug, Deserialize)]
struct ExecRequest {
    /// Run through `sh -c` in the guest.
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Boot or resume a VM if none is running. On by default: a sandbox that
    /// scaled to zero should still answer `exec`. `false` asks for a `409`
    /// instead, for callers that want to know rather than wait.
    #[serde(default = "default_true")]
    wake: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct ExecResponse {
    /// Which VM ran it. Worth returning even for a single-VM sandbox: after a
    /// resume or a rebuild it is a different sandbox than last time.
    sandbox_id: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
    /// stdout and stderr interleaved in the order the guest wrote them, as the
    /// daemon captured it. The only faithful rendering of a command whose
    /// output interleaves.
    output: String,
}

#[derive(Debug, Default, Deserialize)]
struct ShellQuery {
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default = "default_true")]
    wake: bool,
}

/// Resolve a deployment to a VM that can run something, waking one if asked.
///
/// The waiting half is the proxy's cold-start path (`proxy::wait_for_capacity`)
/// reused verbatim, so an `exec` against a sleeping sandbox nudges the
/// autoscaler and waits exactly as a request would — including the autoscaler's
/// preference for resuming a suspended VM over booting a fresh one.
///
/// The returned guard holds an in-flight slot for as long as the caller keeps
/// it, which is what stops the VM being scaled out from under a live session.
async fn hold_a_vm(
    state: &AdminState,
    id: &str,
    wake: bool,
) -> Result<crate::deployment::BackendSlot, Response> {
    let Some(d) = state.registry.get(id) else {
        return Err(err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response());
    };
    if !d.spec.is_managed() {
        let why = if d.spec.is_site() {
            "a site is files on disk"
        } else {
            "its upstreams are addresses, not VMs"
        };
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("deployment {id:?} has no VM to run a command in — {why}"),
        )
        .into_response());
    }

    if let Some(b) = d.select(&[]) {
        return Ok(b.hold());
    }
    if !wake {
        return Err(err(
            StatusCode::CONFLICT,
            format!("deployment {id:?} has no running VM (pass wake=true to start one)"),
        )
        .into_response());
    }
    match crate::proxy::wait_for_capacity(&d, &[], &state.metrics).await {
        Some(b) => Ok(b.hold()),
        None => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "deployment {id:?} has no VM and none became available within \
                 cold_start_timeout_secs"
            ),
        )
        .into_response()),
    }
}

/// `POST /deployments/:id/exec` — run one command in the deployment's VM.
async fn exec(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<ExecRequest>,
) -> impl IntoResponse {
    if req.command.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "command must not be empty").into_response();
    }

    let slot = match hold_a_vm(&state, &id, req.wake).await {
        Ok(slot) => slot,
        Err(response) => return response,
    };
    let sandbox_id = slot.sandbox_id().to_string();

    let timeout = std::time::Duration::from_secs(
        req.timeout_secs
            .unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
            .clamp(1, MAX_EXEC_TIMEOUT_SECS),
    );
    let options = heyo_sdk::CommandRunOptions {
        cwd: req.cwd,
        env: req.env,
        timeout: Some(timeout),
    };

    tracing::info!(deployment = %id, sandbox = %sandbox_id, "exec");
    match state
        .autoscaler
        .vms()
        .exec(&sandbox_id, &req.command, options)
        .await
    {
        Ok(result) => Json(ExecResponse {
            sandbox_id,
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
            output: result.output,
        })
        .into_response(),
        // The command's own failure is a 200 with a non-zero `exit_code`; this
        // is app-lb failing to *run* it, which is a different thing entirely and
        // must not be mistaken for one.
        Err(e) => err(
            StatusCode::BAD_GATEWAY,
            format!("could not run the command in {sandbox_id}: {e}"),
        )
        .into_response(),
    }
}

/// `GET /deployments/:id/shell` — an interactive PTY, over a WebSocket.
///
/// Wire protocol with the client, which is the daemon's own minus the parts the
/// SDK session already handles (sequence numbers and acks):
///
/// - client → server, binary `[0x01, ...stdin]`
/// - client → server, text `{"type":"resize","cols":N,"rows":N}`
/// - server → client, text `{"type":"ready","sandbox_id":"…"}` — sent once
/// - server → client, binary `[0x02, ...stdout]` (the PTY merges stderr in)
/// - server → client, text `{"type":"exit","code":N}` or
///   `{"type":"error","message":"…"}`
async fn shell(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Query(q): Query<ShellQuery>,
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    // Everything that can fail with a status code has to fail *before* the
    // upgrade: once the socket is a WebSocket, a client sees a close frame with
    // no explanation instead of a 404.
    let slot = match hold_a_vm(&state, &id, q.wake).await {
        Ok(slot) => slot,
        Err(response) => return response,
    };
    let sandbox_id = slot.sandbox_id().to_string();

    let options = heyo_sdk::ShellOptions {
        cwd: q.cwd,
        env: None,
        cols: q.cols.unwrap_or(80),
        rows: q.rows.unwrap_or(24),
        ..Default::default()
    };
    let session = match state.autoscaler.vms().shell(&sandbox_id, options).await {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("could not open a shell on {sandbox_id}: {e}"),
            )
            .into_response();
        }
    };

    tracing::info!(deployment = %id, sandbox = %sandbox_id, "shell session opened");
    ws.on_upgrade(move |socket| async move {
        // `slot` moves in here, so the VM is held for the life of the session
        // and released however it ends.
        pump_shell(socket, session, sandbox_id.clone(), slot).await;
        tracing::info!(deployment = %id, sandbox = %sandbox_id, "shell session closed");
    })
}

/// Copy between the client's WebSocket and the sandbox's PTY until either ends.
async fn pump_shell(
    socket: axum::extract::ws::WebSocket,
    session: heyo_sdk::ShellSession,
    sandbox_id: String,
    _slot: crate::deployment::BackendSlot,
) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};

    const STDIN: u8 = 0x01;
    const STDOUT: u8 = 0x02;

    let (mut tx, mut rx) = socket.split();
    if tx
        .send(Message::Text(
            serde_json::json!({ "type": "ready", "sandbox_id": sandbox_id }).to_string(),
        ))
        .await
        .is_err()
    {
        return; // client hung up during the handshake
    }

    let mut output = session.output();
    let mut events = session.events();
    loop {
        tokio::select! {
            // Guest → client.
            Some(chunk) = output.next() => {
                let mut frame = Vec::with_capacity(chunk.len() + 1);
                frame.push(STDOUT);
                frame.extend_from_slice(&chunk);
                if tx.send(Message::Binary(frame)).await.is_err() {
                    break;
                }
            }
            // Client → guest.
            msg = rx.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        // Anything not marked stdin is a client bug, not data to
                        // feed a shell — dropping it beats writing a stray frame
                        // header into somebody's terminal.
                        if bytes.first() == Some(&STDIN)
                            && session.write(&bytes[1..]).await.is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                            && v.get("type").and_then(|t| t.as_str()) == Some("resize")
                        {
                            let cols = v.get("cols").and_then(|c| c.as_u64()).unwrap_or(80) as u16;
                            let rows = v.get("rows").and_then(|r| r.as_u64()).unwrap_or(24) as u16;
                            let _ = session.resize(cols, rows).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}       // ping/pong: axum answers these itself
                    Some(Err(_)) => break,  // socket is gone
                }
            }
            // Lifecycle, so a client learns *why* its shell ended.
            Some(event) = events.next() => {
                let msg = match event {
                    heyo_sdk::ShellEvent::Closed { exit_code } => {
                        serde_json::json!({ "type": "exit", "code": exit_code.unwrap_or(0) })
                    }
                    heyo_sdk::ShellEvent::Error(message) => {
                        serde_json::json!({ "type": "error", "message": message })
                    }
                    // Reconnects are the SDK's business; the client's stream is
                    // continuous either way and saying so would only confuse it.
                    _ => continue,
                };
                let closing = msg["type"] == "exit";
                let _ = tx.send(Message::Text(msg.to_string())).await;
                if closing {
                    break;
                }
            }
            else => break,
        }
    }

    let _ = session.close().await;
    let _ = tx.send(Message::Close(None)).await;
}

async fn healthz() -> &'static str {
    "ok\n"
}

/// Issued certificates: `GET /certs`.
///
/// The only way to see *why* a hostname is not yet serving its own certificate —
/// issuance is asynchronous, so a deployment can be live and routing while its
/// certificate is still pending or failing. A hostname with a route but no entry
/// here is either still in flight, backing off after a failure, or a
/// `host_suffix` rule (which ACME cannot cover; see `src/acme.rs`).
async fn certs(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.certs.status())
}

// -- secrets ---------------------------------------------------------------

/// Persist the secret store, mapping a failure onto a 500.
///
/// Unlike the deployment registry — where a failed write is logged and the
/// in-memory change stands — a secret that only exists in memory is a rotation
/// that silently un-rotates on the next restart. Better to fail the request.
fn persist_secrets(state: &AdminState) -> Result<(), (StatusCode, Json<ApiError>)> {
    state.secrets.persist().map_err(|e| {
        tracing::error!(error = %e, "failed to persist secrets");
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the secret was not saved: {e}"),
        )
    })
}

/// Deployments whose build block points at a secret. Used to keep a delete from
/// breaking a deployment that still needs it.
fn secret_users(state: &AdminState, id: &str) -> Vec<String> {
    let mut users: Vec<String> = state
        .registry
        .deployments()
        .values()
        .filter(|d| {
            d.spec
                .build
                .as_ref()
                .and_then(|b| b.auth.as_ref())
                .is_some_and(|a| a.secret == id)
        })
        .map(|d| d.spec.id.clone())
        .collect();
    users.sort();
    users
}

/// `POST /secrets` — create or replace a secret wholesale.
async fn put_secret(
    State(state): State<AdminState>,
    Json(spec): Json<SecretSpec>,
) -> impl IntoResponse {
    if let Err(e) = spec.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    let id = spec.id.clone();
    let existed = state.secrets.get(&id).is_some();
    state.secrets.put(spec);
    if let Err(e) = persist_secrets(&state) {
        return e.into_response();
    }
    // Keys, never values — the same rule the read path follows, so enabling
    // debug logging can't turn into a credential dump.
    tracing::info!(secret = %id, replaced = existed, "secret stored");
    let summary = state.secrets.summary(&id).expect("just stored");
    let code = if existed { StatusCode::OK } else { StatusCode::CREATED };
    (code, Json(summary)).into_response()
}

/// `PUT /secrets/:id` — as `POST`, with the path id winning.
async fn replace_secret(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(mut spec): Json<SecretSpec>,
) -> impl IntoResponse {
    spec.id = id;
    put_secret(State(state), Json(spec)).await.into_response()
}

#[derive(Deserialize)]
struct SecretPatch {
    /// `"KEY": "value"` sets, `"KEY": null` removes. Anything absent is left
    /// alone, so one key can be rotated without resending the others — which
    /// matters here, because there is no way to read the others back.
    data: BTreeMap<String, Option<String>>,
    #[serde(default)]
    description: Option<String>,
}

async fn patch_secret(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(patch): Json<SecretPatch>,
) -> impl IntoResponse {
    if state.secrets.get(&id).is_none() {
        return err(StatusCode::NOT_FOUND, format!("no secret {id:?}")).into_response();
    }
    let updated = match state.secrets.patch(&id, patch.data) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    if let Some(description) = patch.description {
        let mut next = (*updated).clone();
        next.description = Some(description);
        state.secrets.put(next);
    }
    if let Err(e) = persist_secrets(&state) {
        return e.into_response();
    }
    tracing::info!(secret = %id, "secret updated");
    Json(state.secrets.summary(&id).expect("just stored")).into_response()
}

async fn list_secrets(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.secrets.list())
}

async fn get_secret(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.secrets.summary(&id) {
        Some(s) => Json(s).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("no secret {id:?}")).into_response(),
    }
}

#[derive(Deserialize)]
struct ForceParams {
    #[serde(default)]
    force: bool,
}

/// `DELETE /secrets/:id[?force=true]`.
///
/// Refused while a deployment's build still references it: the failure would
/// otherwise surface much later, as a build that cannot authenticate.
async fn delete_secret(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Query(params): Query<ForceParams>,
) -> impl IntoResponse {
    if state.secrets.get(&id).is_none() {
        return err(StatusCode::NOT_FOUND, format!("no secret {id:?}")).into_response();
    }
    let users = secret_users(&state, &id);
    if !users.is_empty() && !params.force {
        return err(
            StatusCode::CONFLICT,
            format!(
                "secret {id:?} is referenced by deployment(s) {}; their builds would stop \
                 authenticating. Repoint them first, or delete with ?force=true",
                users.join(", ")
            ),
        )
        .into_response();
    }
    state.secrets.remove(&id);
    if let Err(e) = persist_secrets(&state) {
        return e.into_response();
    }
    tracing::info!(secret = %id, forced = params.force, "secret deleted");
    StatusCode::NO_CONTENT.into_response()
}

// -- jobs ------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct BuildRequest {
    /// Build this ref instead of the spec's. A one-off: the stored `build.ref`
    /// is left alone, so a hotfix tag doesn't quietly become the new default.
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
}

#[derive(Deserialize, Default)]
struct PullRequest {
    /// Pull this reference instead of the spec's. A one-off, like a build's:
    /// `{"ref": "<digest>"}` is what a rollback to known bytes looks like,
    /// without making that digest the deployment's default.
    #[serde(default, rename = "ref")]
    artifact_ref: Option<String>,
    /// Re-fetch even when the image is already on disk. Rarely wanted — the
    /// filename is the digest, so the image being there is proof the bytes are
    /// right — and it exists for the case where the file was damaged after it
    /// was written.
    #[serde(default)]
    force: bool,
}

/// Map a start failure onto a status. Shared by both job kinds, because the
/// reasons a job can't start are the same for either.
fn job_start_error(e: StartError) -> Response {
    match e {
        e @ StartError::NoDeployment(_) => {
            err(StatusCode::NOT_FOUND, e.to_string()).into_response()
        }
        e @ StartError::AlreadyRunning(_) => {
            err(StatusCode::CONFLICT, e.to_string()).into_response()
        }
        e => err(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// `POST /deployments/:id/build` — clone, build the image, roll the pool.
///
/// Returns `202` with a job record as soon as the work is scheduled. A build
/// takes minutes; poll `GET /jobs/:job_id` for the outcome.
async fn start_build(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    body: Option<Json<BuildRequest>>,
) -> impl IntoResponse {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    match state.jobs.start_build(&id, req.git_ref) {
        Ok(record) => {
            tracing::info!(deployment = %id, job = %record.id, "image build started");
            (StatusCode::ACCEPTED, Json(record)).into_response()
        }
        Err(e) => job_start_error(e),
    }
}

/// `POST /deployments/:id/pull` — materialize a rootfs from an artifact store
/// and roll the pool onto it.
///
/// `202` for the same reason a build is: the bytes may be gigabytes across a
/// network. It is often much faster than a build — an image already on disk is
/// resolved and skipped in one round trip — but "often fast" is not something to
/// hold a request open on.
async fn start_pull(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    body: Option<Json<PullRequest>>,
) -> impl IntoResponse {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    match state.jobs.start_pull(&id, req.artifact_ref, req.force) {
        Ok(record) => {
            tracing::info!(deployment = %id, job = %record.id, "artifact pull started");
            (StatusCode::ACCEPTED, Json(record)).into_response()
        }
        Err(e) => job_start_error(e),
    }
}

/// `POST /deployments/:id/update` — run a static deployment's update commands on
/// this host, then re-probe its upstreams.
///
/// The static counterpart of `build`, and `202` for the same reason: `cargo
/// build && systemctl restart` is not something to hold an HTTP request open
/// for. Nothing in the spec changes — the upstreams are the same addresses, and
/// what moved is the code answering on them.
async fn start_update(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.jobs.start_update(&id) {
        Ok(record) => {
            tracing::info!(deployment = %id, job = %record.id, "host update started");
            (StatusCode::ACCEPTED, Json(record)).into_response()
        }
        Err(e) => job_start_error(e),
    }
}

async fn list_jobs(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.jobs.records(None))
}

async fn deployment_jobs(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.registry.get(&id).is_none() {
        return err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response();
    }
    Json(state.jobs.records(Some(&id))).into_response()
}

async fn get_job(
    State(state): State<AdminState>,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    match state.jobs.record(&job_id) {
        Some(r) => Json(r).into_response(),
        // History is in memory and bounded, so an id can be forgotten rather
        // than never having existed. Say so.
        None => err(
            StatusCode::NOT_FOUND,
            format!("no job {job_id:?} — it may have aged out of the job history"),
        )
        .into_response(),
    }
}

fn router(state: AdminState) -> Router {
    // The dashboard view + its data source are always behind the optional gate.
    let view = Router::new()
        .route("/metrics", get(metrics_snapshot))
        .route("/dashboard", get(dashboard));

    // The deployment CRUD API — register/edit/scale/delete/evict, plus the reads
    // that expose the spec (env vars can hold secrets). Gated too iff
    // `admin_auth` is on; otherwise it stays open, as before.
    let crud = Router::new()
        .route("/deployments", post(register).get(list))
        .route("/deployments/:id", get(get_one).put(update).delete(deregister))
        .route("/deployments/:id/scaling", patch(scale))
        .route("/deployments/:id/vms/:sandbox_id", delete(evict_vm))
        // CRUD-tier on purpose: running a command in a VM is at least as
        // powerful as editing the spec that boots it.
        .route("/deployments/:id/exec", post(exec))
        .route("/deployments/:id/shell", get(shell))
        // Grouped with the CRUD routes so it inherits the `APP_LB_ADMIN_AUTH`
        // gate: it reports which hostnames app-lb holds keys for.
        .route("/certs", get(certs))
        // Secrets: write-only by design. `GET` returns key *names*, never values.
        .route("/secrets", post(put_secret).get(list_secrets))
        .route(
            "/secrets/:id",
            get(get_secret)
                .put(replace_secret)
                .patch(patch_secret)
                .delete(delete_secret),
        )
        // Jobs. `build` runs `git` and `docker` on this host and `update` runs
        // the deployment's own commands, which is why they belong firmly on the
        // gated side of the API. One history covers both kinds: they have the
        // same lifecycle, and "what happened to this deployment lately?" should
        // have one answer.
        .route("/deployments/:id/build", post(start_build))
        .route("/deployments/:id/pull", post(start_pull))
        .route("/deployments/:id/update", post(start_update))
        .route("/deployments/:id/jobs", get(deployment_jobs))
        .route("/jobs", get(list_jobs))
        .route("/jobs/:job_id", get(get_job));

    let (gated, open) = if state.gate_admin {
        (view.merge(crud), Router::new())
    } else {
        (view, crud)
    };

    // `route_layer` runs the auth middleware only for the gated routes, so a 404
    // elsewhere never triggers a challenge. `/healthz` is always open for probes.
    let gated = gated.route_layer(middleware::from_fn_with_state(
        state.clone(),
        require_dashboard_auth,
    ));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(gated)
        .merge(open)
        .with_state(state)
}

#[async_trait]
impl BackgroundService for AdminApi {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let listener = match tokio::net::TcpListener::bind(&self.addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(addr = %self.addr, error = %e, "admin API failed to bind");
                return;
            }
        };
        tracing::info!(addr = %self.addr, "admin API listening");

        let served = axum::serve(listener, router(self.state.clone()))
            .with_graceful_shutdown(async move {
                while shutdown.changed().await.is_ok() {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            })
            .await;

        if let Err(e) = served {
            tracing::error!(error = %e, "admin API stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The link has to reach the *data plane*, which the dashboard cannot infer
    /// from its own location — it is served by the admin listener, on a
    /// different port and (usually) a different scheme.
    mod public_url {
        use super::*;

        fn rule(host: Option<&str>, path: Option<&str>) -> crate::config::RouteRule {
            crate::config::RouteRule {
                host: host.map(str::to_string),
                host_suffix: None,
                path_prefix: path.map(str::to_string),
            }
        }

        #[test]
        fn plaintext_on_the_default_port_needs_no_port() {
            let u = PublicUrl::from_config(false, "0.0.0.0:80", "0.0.0.0:6189");
            assert_eq!(u.of(&rule(Some("web.example.com"), None)).unwrap(), "http://web.example.com");
        }

        /// The out-of-the-box config. Linking `http://host` here would connect
        /// to nothing, which is worse than not linking at all.
        #[test]
        fn a_non_default_port_is_carried_into_the_link() {
            let u = PublicUrl::from_config(false, "0.0.0.0:6188", "0.0.0.0:6189");
            assert_eq!(
                u.of(&rule(Some("web.example.com"), None)).unwrap(),
                "http://web.example.com:6188",
            );
        }

        /// With TLS on, a browser belongs on the HTTPS listener — not the
        /// plaintext one, which would redirect at best.
        #[test]
        fn tls_links_the_https_listener() {
            let u = PublicUrl::from_config(true, "0.0.0.0:80", "0.0.0.0:443");
            assert_eq!(u.of(&rule(Some("web.example.com"), None)).unwrap(), "https://web.example.com");

            let u = PublicUrl::from_config(true, "0.0.0.0:80", "0.0.0.0:6189");
            assert_eq!(
                u.of(&rule(Some("web.example.com"), None)).unwrap(),
                "https://web.example.com:6189",
            );
        }

        /// A host+path rule only matches under its prefix, so linking the bare
        /// host would 404 against this very deployment.
        #[test]
        fn a_path_prefix_is_part_of_the_link() {
            let u = PublicUrl::from_config(false, "0.0.0.0:80", "0.0.0.0:443");
            assert_eq!(
                u.of(&rule(Some("web.example.com"), Some("/api"))).unwrap(),
                "http://web.example.com/api",
            );
        }

        /// Neither a subtree nor a bare path names a hostname a browser could
        /// be sent to, so neither is linkable.
        #[test]
        fn rules_without_a_host_are_not_linkable() {
            let u = PublicUrl::from_config(false, "0.0.0.0:80", "0.0.0.0:443");
            assert!(u.of(&rule(None, Some("/legacy"))).is_none());
            assert!(u.of(&rule(Some("  "), None)).is_none());

            let suffix = crate::config::RouteRule {
                host: None,
                host_suffix: Some("apps.example.com".into()),
                path_prefix: None,
            };
            assert!(u.of(&suffix).is_none());
        }

        #[test]
        fn ipv6_listen_addresses_parse() {
            assert_eq!(port_of("[::]:6188"), Some(6188));
            assert_eq!(port_of("[::1]:443"), Some(443));
            assert_eq!(port_of("0.0.0.0:6188"), Some(6188));
            assert_eq!(port_of("no-port"), None);
        }
    }

    fn header_for(user: &str, password: &str) -> String {
        let token =
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        format!("Basic {token}")
    }

    #[test]
    fn accepts_matching_credentials() {
        let auth = DashboardAuth::new("admin", "s3cret");
        assert!(auth.accepts(Some(&header_for("admin", "s3cret"))));
    }

    #[test]
    fn rejects_wrong_or_missing_credentials() {
        let auth = DashboardAuth::new("admin", "s3cret");
        assert!(!auth.accepts(Some(&header_for("admin", "wrong"))));
        assert!(!auth.accepts(Some(&header_for("root", "s3cret"))));
        assert!(!auth.accepts(Some("Bearer s3cret")), "wrong scheme");
        assert!(!auth.accepts(Some("Basic not-base64")));
        assert!(!auth.accepts(None), "no Authorization header");
    }

    #[test]
    fn password_with_colon_round_trips() {
        // `user:pass:word` must authenticate as password `pass:word`, since
        // Basic auth splits only on the first colon.
        let auth = DashboardAuth::new("admin", "pass:word");
        assert!(auth.accepts(Some(&header_for("admin", "pass:word"))));
    }

    #[test]
    fn ct_eq_matches_std_eq() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"ab", b"abc"));
    }
}
