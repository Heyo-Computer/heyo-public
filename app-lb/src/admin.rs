//! The control plane.
//!
//! Runs as a pingora `BackgroundService` so it shares the server's lifecycle and
//! graceful shutdown. It binds its own listener rather than using a pingora
//! listening service, which trades away zero-downtime socket handoff for that
//! port in exchange for real routing — an acceptable deal for an admin API.

use crate::autoscale::Autoscaler;
use crate::config::DeploymentSpec;
use crate::deployment::now_secs;
use crate::metrics::{DeploymentMetricsSnapshot, HostUsageSnapshot, Metrics};
use crate::registry::Registry;
use async_trait::async_trait;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use serde::Serialize;
use std::sync::Arc;

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

#[derive(Clone)]
struct AdminState {
    registry: Arc<Registry>,
    autoscaler: Arc<Autoscaler>,
    metrics: Arc<Metrics>,
    /// `None` disables the gate — the dashboard and `/metrics` are then open.
    auth: Option<Arc<DashboardAuth>>,
    /// Process start (LB clock), so the dashboard can show how long the numbers
    /// have been accumulating.
    started_at: u64,
}

pub struct AdminApi {
    addr: String,
    state: AdminState,
}

impl AdminApi {
    pub fn new(
        addr: String,
        registry: Arc<Registry>,
        autoscaler: Arc<Autoscaler>,
        metrics: Arc<Metrics>,
        dashboard_user: Option<String>,
        dashboard_password: Option<String>,
    ) -> Self {
        // The gate turns on as soon as a password is set; the username is
        // optional and defaults to "admin", so one env var is enough to secure
        // it and there is no "half-configured, silently open" state.
        let auth = dashboard_password.map(|password| {
            let user = dashboard_user.unwrap_or_else(|| "admin".to_string());
            tracing::info!(user = %user, "dashboard auth enabled");
            Arc::new(DashboardAuth::new(&user, &password))
        });
        if auth.is_none() {
            tracing::info!("dashboard auth disabled (set APP_LB_DASHBOARD_PASSWORD to enable)");
        }

        Self {
            addr,
            state: AdminState {
                registry,
                autoscaler,
                metrics,
                auth,
                started_at: now_secs(),
            },
        }
    }
}

/// Gate the dashboard routes on Basic auth when configured. A `401` carries the
/// `WWW-Authenticate` challenge so a browser shows its native login prompt and
/// caches the credentials for the same-origin `/metrics` polls that follow.
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
    desired_replicas: u32,
    ready: usize,
    pending: usize,
    total_in_flight: usize,
    vms: Vec<VmStatus>,
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
        desired_replicas: d.desired_replicas(),
        ready: backends.len(),
        pending: d.pending().len(),
        total_in_flight: d.total_in_flight(),
        vms: backends
            .iter()
            .map(|b| VmStatus {
                sandbox_id: b.sandbox_id.clone(),
                addr: b.addr.to_string(),
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
                pool: pool_status_of(d),
                vms: backends
                    .iter()
                    .map(|b| {
                        let usage = b.usage();
                        VmView {
                            sandbox_id: b.sandbox_id.clone(),
                            addr: b.addr.to_string(),
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
        deployments: views,
    })
}

async fn dashboard() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
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
    if let Some(old) = state.registry.get(&id) {
        state.autoscaler.teardown(&old).await;
    }

    let deployment = state.registry.upsert(spec);
    if let Err(e) = state.registry.persist() {
        tracing::error!(error = %e, "failed to persist state");
    }
    tracing::info!(deployment = %id, "registered");

    // Let the autoscaler build the warm pool without waiting for the next tick.
    deployment.scale_signal.notify_one();

    (StatusCode::CREATED, Json(status_of(&deployment))).into_response()
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

async fn healthz() -> &'static str {
    "ok\n"
}

fn router(state: AdminState) -> Router {
    // The dashboard and its data source sit behind the optional auth gate;
    // `route_layer` runs the middleware only for these matched routes, so a 404
    // elsewhere never triggers a challenge. The CRUD/health routes are left
    // open — they are a separate concern, still bound to `admin_addr`.
    let gated = Router::new()
        .route("/metrics", get(metrics_snapshot))
        .route("/dashboard", get(dashboard))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_dashboard_auth,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/deployments", post(register).get(list))
        .route("/deployments/:id", get(get_one))
        .route("/deployments/:id", delete(deregister))
        .merge(gated)
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
