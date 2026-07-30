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
    if d.spec.is_static() { "static" } else { "vm" }
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
    pool: PoolStatus,
    vms: Vec<VmView>,
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
    deployments: Vec<DeploymentView>,
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
    }
}

/// The dashboard's data source: live pool gauges joined with accumulated
/// metrics, per deployment plus a global rollup.
async fn metrics_snapshot(State(state): State<AdminState>) -> impl IntoResponse {
    let deployments = state.registry.deployments();
    let mut views: Vec<DeploymentView> = deployments
        .values()
        .map(|d| {
            let backends = d.backends();
            DeploymentView {
                id: d.spec.id.clone(),
                kind: deployment_kind(d),
                upstreams: d.spec.upstreams.clone(),
                pool: pool_status_of(d),
                vms: backends
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
                    .collect(),
                metrics: state.metrics.deployment_snapshot(&d.spec.id),
            }
        })
        .collect();
    views.sort_by(|a, b| a.id.cmp(&b.id));

    let fleet = FleetPool {
        deployments: views.len(),
        ready: views.iter().map(|v| v.pool.ready).sum(),
        draining: views.iter().map(|v| v.pool.draining).sum(),
        pending: views.iter().map(|v| v.pool.pending).sum(),
        total_in_flight: views.iter().map(|v| v.pool.total_in_flight).sum(),
    };

    let now = now_secs();
    Json(MetricsResponse {
        generated_at: now,
        uptime_secs: now.saturating_sub(state.started_at),
        host: state.metrics.host_snapshot(),
        fleet,
        global: state.metrics.global_snapshot(),
        obs: state.obs.as_ref().map(|o| o.snapshot()),
        deployments: views,
    })
}

async fn dashboard(State(state): State<AdminState>) -> impl IntoResponse {
    Html(state.dashboard_html.to_string())
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
    if let Err(e) = state.registry.persist() {
        tracing::error!(error = %e, "failed to persist state");
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

    if let Err(e) = state.registry.persist() {
        tracing::error!(error = %e, "failed to persist state");
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

    // A static (proxy_pass) deployment isn't autoscaled — its scaling policy is
    // inert — so a scale request is a mistake, not a no-op. Reject it explicitly.
    if old.spec.is_static() {
        return err(
            StatusCode::BAD_REQUEST,
            format!("deployment {id:?} is static (proxy_pass) and cannot be scaled; edit its `upstreams` via PUT instead"),
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
    if let Err(e) = state.registry.persist() {
        tracing::error!(error = %e, "failed to persist state");
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
    if let Err(e) = state.registry.persist() {
        tracing::error!(error = %e, "failed to persist state");
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

    // A static (proxy_pass) deployment has no autoscaler to boot a replacement,
    // so evicting one of its fixed upstreams is meaningless — reject it. Edit the
    // `upstreams` list via PUT to change targets instead.
    if d.spec.is_static() {
        return err(
            StatusCode::BAD_REQUEST,
            format!("deployment {id:?} is static (proxy_pass); its upstreams cannot be evicted — edit the spec instead"),
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
