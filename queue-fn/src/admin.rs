//! The control plane, the dashboard, and the invoke endpoints.
//!
//! **Synchronous invoke goes through JetStream like everything else.** It could
//! claim a VM directly and skip the bus, which would be faster. It doesn't,
//! because two dispatch paths means two demand signals and two places for
//! ordering to differ. Instead the handler subscribes to a core-NATS reply
//! subject, publishes the event, and waits — so a sync invoke is an async one
//! that happens to have someone listening.
//!
//! The subscribe-before-publish ordering is load-bearing: subscribing after
//! publishing loses the result whenever a warm VM finishes before the subscribe
//! lands, which is exactly the fast case.

use crate::autoscale::{Autoscaler, EvictOutcome};
use crate::bus::{Bus, BusError, EventSource, InvokeEvent, InvokeResult, Outcome, reply_subject};
use crate::cloud::{CloudSnapshot, CloudState};
use crate::config::{FunctionSpec, QfnConfig, ScalingPolicy};
use crate::function::{Function, now_secs};
use crate::metrics::{FunctionMetricsSnapshot, HostUsageSnapshot, Metrics};
use crate::registry::Registry;
use crate::results::{InvocationRecord, Results};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use base64::Engine;
use futures::StreamExt;
use heyo_sdk::SandboxDriver;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// The live dashboard page. Self-contained (no external fetches beyond the
/// same-origin `metrics` poll) so it works over an SSH tunnel with no assets.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Default page size for the recent-invocations table.
const DEFAULT_RECENT: usize = 25;
const DEFAULT_DLQ_LIMIT: usize = 50;
/// Cloud jobs embedded in the 2s `metrics` poll. Deliberately smaller than the
/// ring: the dashboard's table shows a page, and `/cloud` serves the rest, so
/// the poll body doesn't grow with a ring an operator sized for depth.
const DEFAULT_CLOUD_JOBS: usize = 40;
/// Ceiling on `?limit=`, so one request cannot ask for an unbounded body.
const MAX_CLOUD_JOBS: usize = 1000;

/// The optional Basic-auth gate.
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

/// HTML-escape a value bound for element text — the display name comes from an
/// env var, so escape it rather than trusting it into markup.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Clone)]
pub struct AdminState {
    registry: Arc<Registry>,
    autoscaler: Arc<Autoscaler>,
    metrics: Arc<Metrics>,
    results: Arc<Results>,
    bus: Arc<Bus>,
    /// `None` when cloud observation is switched off.
    cloud: Option<Arc<CloudState>>,
    dashboard_html: Arc<str>,
    /// `None` disables the gate.
    auth: Option<Arc<DashboardAuth>>,
    gate_admin: bool,
    gate_invoke: bool,
    max_payload_bytes: usize,
    sync_invoke_timeout: Duration,
    started_at: u64,
}

pub struct AdminApi {
    addr: String,
    state: AdminState,
}

impl AdminApi {
    pub fn new(
        cfg: &QfnConfig,
        registry: Arc<Registry>,
        autoscaler: Arc<Autoscaler>,
        metrics: Arc<Metrics>,
        results: Arc<Results>,
        bus: Arc<Bus>,
        cloud: Option<Arc<CloudState>>,
    ) -> Self {
        let dashboard_html: Arc<str> =
            Arc::from(DASHBOARD_HTML.replace("{{APP_NAME}}", &html_escape(&cfg.name)));

        // The gate turns on as soon as a password is set; the username is
        // optional and defaults to "admin", so one env var is enough to secure
        // it and there is no half-configured, silently-open state.
        let auth = cfg.dashboard_password.clone().map(|password| {
            let user = cfg
                .dashboard_user
                .clone()
                .unwrap_or_else(|| "admin".to_string());
            tracing::info!(
                user = %user,
                admin_api = cfg.admin_auth,
                invoke_api = cfg.invoke_auth,
                "dashboard auth enabled",
            );
            Arc::new(DashboardAuth::new(&user, &password))
        });
        if auth.is_none() {
            tracing::info!("dashboard auth disabled (set QFN_DASHBOARD_PASSWORD to enable)");
        }
        // main() rejects a gate without a password, so this cannot be a silently
        // open state; assert the invariant in case that check ever moves.
        debug_assert!(
            !(cfg.admin_auth || cfg.invoke_auth) || auth.is_some(),
            "a gate needs credentials",
        );

        Self {
            addr: cfg.admin_addr.clone(),
            state: AdminState {
                registry,
                autoscaler,
                metrics,
                results,
                bus,
                cloud,
                dashboard_html,
                auth,
                gate_admin: cfg.admin_auth,
                gate_invoke: cfg.invoke_auth,
                max_payload_bytes: cfg.max_payload_bytes,
                sync_invoke_timeout: Duration::from_secs(cfg.sync_invoke_timeout_secs),
                started_at: now_secs(),
            },
        }
    }

    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) {
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

/// Gate the protected routes on Basic auth when configured. A 401 carries the
/// `WWW-Authenticate` challenge so a browser shows its native prompt and caches
/// the credentials for the dashboard's own same-origin requests.
async fn require_auth(State(state): State<AdminState>, req: Request, next: Next) -> Response {
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
                "Basic realm=\"queue-fn dashboard\", charset=\"UTF-8\"",
            )],
            "authentication required\n",
        )
            .into_response()
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

fn not_found(id: &str) -> (StatusCode, Json<ApiError>) {
    err(StatusCode::NOT_FOUND, format!("no function {id:?}"))
}

#[derive(Serialize)]
struct FunctionStatus {
    spec: FunctionSpec,
    desired_replicas: u32,
    ready: usize,
    pending: usize,
    busy: usize,
    queued: u64,
    paused: bool,
}

fn status_of(f: &Arc<Function>) -> FunctionStatus {
    FunctionStatus {
        spec: f.spec.clone(),
        desired_replicas: f.desired_replicas(),
        ready: f.workers().len(),
        pending: f.pending().len(),
        busy: f.total_in_flight(),
        queued: f.queue_pending(),
        paused: f.is_paused(),
    }
}

/// Live pool state for one function, as the dashboard shows it.
#[derive(Serialize)]
struct PoolStatus {
    desired_replicas: u32,
    /// Every VM in the pool, draining ones included.
    ready: usize,
    draining: usize,
    /// Booting VMs, not yet usable.
    pending: usize,
    busy: usize,
    target_concurrency: u32,
    min_replicas: u32,
    max_replicas: u32,
    warm_pool: u32,
    /// Backlog against capacity: (queued + running) / (available VMs × target).
    /// `None` when there is no available capacity to divide by — an empty or
    /// all-draining pool — which the dashboard renders as "—" rather than 0%.
    utilization: Option<f64>,
    /// Summed CPU% (percent-of-a-core) and RSS across the pool, `None` until the
    /// daemon reports usage for at least one VM.
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
}

#[derive(Serialize)]
struct VmView {
    sandbox_id: String,
    busy: bool,
    healthy: bool,
    draining: bool,
    uptime_secs: u64,
    invocations: u64,
    failures: u64,
    cpu_percent: Option<f64>,
    memory_bytes: Option<u64>,
}

#[derive(Serialize)]
struct FunctionView {
    id: String,
    pool: PoolStatus,
    vms: Vec<VmView>,
    metrics: FunctionMetricsSnapshot,
    recent: Vec<InvocationRecord>,
}

/// A rollup of pool gauges across every function, for the top-of-dashboard tiles.
#[derive(Serialize)]
struct FleetPool {
    functions: usize,
    ready: usize,
    draining: usize,
    pending: usize,
    busy: usize,
    queued: u64,
}

#[derive(Serialize)]
struct MetricsResponse {
    generated_at: u64,
    uptime_secs: u64,
    host: HostUsageSnapshot,
    fleet: FleetPool,
    /// Every function's metrics merged. Includes history from deregistered
    /// functions, so totals don't drop when one is removed.
    global: FunctionMetricsSnapshot,
    functions: Vec<FunctionView>,
    /// heyo cloud's queue on the same NATS. Absent — not empty — when
    /// observation is switched off, so the dashboard can tell the two apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    cloud: Option<CloudSnapshot>,
}

fn pool_status_of(f: &Arc<Function>) -> PoolStatus {
    let workers = f.workers();
    let draining = workers.iter().filter(|w| w.is_draining()).count();
    let available = workers.iter().filter(|w| !w.is_draining() && w.is_healthy()).count();
    let target = f.spec.scaling.target_concurrency.max(1) as usize;
    let capacity = available * target;
    let utilization = (capacity > 0).then(|| f.demand() as f64 / capacity as f64);

    // `None` if no VM has a sample yet, so the dashboard can distinguish "no
    // data" from a genuine zero.
    let samples: Vec<(f64, u64)> = workers.iter().filter_map(|w| w.usage()).collect();
    let (cpu_percent, memory_bytes) = if samples.is_empty() {
        (None, None)
    } else {
        (
            Some(samples.iter().map(|(c, _)| c).sum()),
            Some(samples.iter().map(|(_, m)| m).sum()),
        )
    };

    PoolStatus {
        desired_replicas: f.desired_replicas(),
        ready: workers.len(),
        draining,
        pending: f.pending().len(),
        busy: f.total_in_flight(),
        target_concurrency: f.spec.scaling.target_concurrency,
        min_replicas: f.spec.scaling.min_replicas,
        max_replicas: f.spec.scaling.max_replicas,
        warm_pool: f.spec.scaling.warm_pool,
        utilization,
        cpu_percent,
        memory_bytes,
    }
}

/// The dashboard's data source: live pool gauges joined with accumulated
/// metrics, per function plus a global rollup.
async fn metrics_snapshot(State(state): State<AdminState>) -> impl IntoResponse {
    let functions = state.registry.functions();
    let mut views: Vec<FunctionView> = functions
        .values()
        .map(|f| {
            let workers = f.workers();
            FunctionView {
                id: f.spec.id.clone(),
                pool: pool_status_of(f),
                vms: workers
                    .iter()
                    .map(|w| {
                        let usage = w.usage();
                        VmView {
                            sandbox_id: w.sandbox_id.clone(),
                            busy: w.is_busy(),
                            healthy: w.is_healthy(),
                            draining: w.is_draining(),
                            uptime_secs: w.uptime_secs(),
                            invocations: w.invocations(),
                            failures: w.failures(),
                            cpu_percent: usage.map(|(c, _)| c),
                            memory_bytes: usage.map(|(_, m)| m),
                        }
                    })
                    .collect(),
                metrics: state.metrics.function_snapshot(&f.spec.id),
                recent: state.results.recent(&f.spec.id, DEFAULT_RECENT),
            }
        })
        .collect();
    views.sort_by(|a, b| a.id.cmp(&b.id));

    let fleet = FleetPool {
        functions: views.len(),
        ready: views.iter().map(|v| v.pool.ready).sum(),
        draining: views.iter().map(|v| v.pool.draining).sum(),
        pending: views.iter().map(|v| v.pool.pending).sum(),
        busy: views.iter().map(|v| v.pool.busy).sum(),
        queued: views.iter().map(|v| v.metrics.queue.pending).sum(),
    };

    let now = now_secs();
    Json(MetricsResponse {
        generated_at: now,
        uptime_secs: now.saturating_sub(state.started_at),
        host: state.metrics.host_snapshot(),
        fleet,
        global: state.metrics.global_snapshot(),
        functions: views,
        cloud: state.cloud.as_ref().map(|c| c.snapshot(DEFAULT_CLOUD_JOBS)),
    })
}

/// The cloud queue on its own: `GET /cloud[?limit=]`.
///
/// Same data as the `cloud` block in `metrics`, with the whole retained job ring
/// available rather than the page the dashboard renders inline.
async fn cloud_snapshot(
    State(state): State<AdminState>,
    Query(params): Query<LimitParams>,
) -> impl IntoResponse {
    let Some(cloud) = state.cloud.as_ref() else {
        return err(
            StatusCode::NOT_FOUND,
            "cloud observation is disabled; set QFN_CLOUD_OBSERVE=true to enable it",
        )
        .into_response();
    };
    let limit = params
        .limit
        .unwrap_or(DEFAULT_CLOUD_JOBS)
        .min(MAX_CLOUD_JOBS);
    Json(cloud.snapshot(limit)).into_response()
}

async fn dashboard(State(state): State<AdminState>) -> impl IntoResponse {
    Html(state.dashboard_html.to_string())
}

// --- Function CRUD -------------------------------------------------------

async fn register(
    State(state): State<AdminState>,
    Json(spec): Json<FunctionSpec>,
) -> impl IntoResponse {
    if let Err(e) = spec.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    if spec.exec.max_payload_bytes > state.max_payload_bytes {
        return err(
            StatusCode::BAD_REQUEST,
            format!(
                "exec.max_payload_bytes ({}) exceeds this host's limit of {}",
                spec.exec.max_payload_bytes, state.max_payload_bytes
            ),
        )
        .into_response();
    }

    let id = spec.id.clone();

    // The consumer must exist before the function does. A function with no
    // consumer would accept enqueues that nothing ever pulls — silent data loss,
    // which is worse than refusing the registration.
    if let Err(e) = state.bus.ensure_consumer(&spec).await {
        return err(
            StatusCode::BAD_GATEWAY,
            format!("could not create the JetStream consumer: {e}"),
        )
        .into_response();
    }

    // Replacing a function abandons its old pool; tear it down explicitly so the
    // VMs don't linger until their TTL.
    if let Some(old) = state.registry.get(&id) {
        state.autoscaler.teardown(&old).await;
    }

    let function = state.registry.upsert(spec);
    persist(&state);
    tracing::info!(function = %id, "registered");

    // Let the autoscaler build the warm pool without waiting for the next tick.
    function.scale_signal.notify_one();

    (StatusCode::CREATED, Json(status_of(&function))).into_response()
}

/// Edit a function in place: `PUT /functions/:id`.
///
/// The whole spec is replaced (the path id wins, so the body's id cannot
/// retarget another function). The pool is preserved when the VM *template* is
/// unchanged — an exec or scaling edit never disturbs running VMs; only a change
/// to the `vm` block rebuilds them, because the existing VMs were built from the
/// old template.
async fn update(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(mut spec): Json<FunctionSpec>,
) -> impl IntoResponse {
    spec.id = id.clone();
    if let Err(e) = spec.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    let Some(old) = state.registry.get(&id) else {
        return not_found(&id).into_response();
    };

    // The consumer encodes ack_wait and max_deliver from the spec, so an edit to
    // either has to reach JetStream or the change is cosmetic.
    if let Err(e) = state.bus.ensure_consumer(&spec).await {
        return err(
            StatusCode::BAD_GATEWAY,
            format!("could not update the JetStream consumer: {e}"),
        )
        .into_response();
    }

    let function = if old.spec.vm != spec.vm {
        tracing::info!(function = %id, "updating (VM template changed; rebuilding pool)");
        state.autoscaler.teardown(&old).await;
        state.registry.upsert(spec)
    } else {
        tracing::info!(function = %id, "updating (pool preserved)");
        match state.registry.update(spec) {
            Some(f) => f,
            None => return not_found(&id).into_response(),
        }
    };

    persist(&state);
    function.scale_signal.notify_one();
    Json(status_of(&function)).into_response()
}

/// Manually scale: `PATCH /functions/:id/scaling`.
///
/// The body is a *partial* `ScalingPolicy` — only the fields present change — so
/// the dashboard can send just `{"min_replicas": 2}` without resetting the
/// timeouts it doesn't display. Never touches the VM template, so the pool is
/// always preserved; the autoscaler grows or drains it on the nudge.
async fn scale(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(patch): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(old) = state.registry.get(&id) else {
        return not_found(&id).into_response();
    };
    let Some(patch) = patch.as_object() else {
        return err(StatusCode::BAD_REQUEST, "the scaling patch must be a JSON object")
            .into_response();
    };

    // Merge onto the current policy, then re-parse so serde validates types.
    let mut merged = match serde_json::to_value(&old.spec.scaling) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the current scaling policy",
            )
            .into_response();
        }
    };
    for (k, v) in patch {
        merged.insert(k.clone(), v.clone());
    }
    let scaling: ScalingPolicy = match serde_json::from_value(serde_json::Value::Object(merged)) {
        Ok(s) => s,
        Err(e) => {
            return err(StatusCode::BAD_REQUEST, format!("invalid scaling policy: {e}"))
                .into_response();
        }
    };

    let mut spec = old.spec.clone();
    spec.scaling = scaling;
    // Catches min > max, a zero target_concurrency, and so on.
    if let Err(e) = spec.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    // max_ack_pending derives from max_replicas × target_concurrency, so the
    // consumer has to be told about a scaling change too.
    if let Err(e) = state.bus.ensure_consumer(&spec).await {
        return err(
            StatusCode::BAD_GATEWAY,
            format!("could not update the JetStream consumer: {e}"),
        )
        .into_response();
    }

    let Some(function) = state.registry.update(spec) else {
        return not_found(&id).into_response();
    };
    persist(&state);
    tracing::info!(function = %id, "scaled");
    function.scale_signal.notify_one();
    Json(status_of(&function)).into_response()
}

async fn list(State(state): State<AdminState>) -> impl IntoResponse {
    let functions = state.registry.functions();
    let mut out: Vec<_> = functions.values().map(status_of).collect();
    out.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    Json(out)
}

async fn get_one(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.registry.get(&id) {
        Some(f) => Json(status_of(&f)).into_response(),
        None => not_found(&id).into_response(),
    }
}

async fn deregister(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(f) = state.registry.remove(&id) else {
        return not_found(&id).into_response();
    };
    // Removed from the registry first, so the teardown cannot race the
    // dispatcher into starting new work on a VM being killed.
    state.autoscaler.teardown(&f).await;
    if let Err(e) = state.bus.delete_consumer(&id).await {
        tracing::warn!(function = %id, error = %e, "could not delete the consumer");
    }
    state.results.forget(&id);
    persist(&state);
    tracing::info!(function = %id, "deregistered");
    StatusCode::NO_CONTENT.into_response()
}

// --- Invocation ----------------------------------------------------------

#[derive(Deserialize, Default)]
struct InvokeBody {
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct EnqueueResponse {
    invocation_id: String,
}

/// Map a publish failure to a status. An oversized payload is the caller's
/// fault and unretryable; anything else is the bus being unavailable.
fn publish_error(e: BusError) -> (StatusCode, Json<ApiError>) {
    match e {
        BusError::PayloadTooLarge { .. } => err(StatusCode::PAYLOAD_TOO_LARGE, e.to_string()),
        other => err(StatusCode::BAD_GATEWAY, other.to_string()),
    }
}

/// Publish an event and count it as demand immediately.
async fn publish(
    state: &AdminState,
    f: &Arc<Function>,
    event: &InvokeEvent,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    state
        .bus
        .publish(event, f.spec.exec.max_payload_bytes.min(state.max_payload_bytes))
        .await
        .map_err(publish_error)?;
    // The reconcile tick refreshes depth every 2s; bumping the estimate and
    // nudging here keeps a cold start from paying that tick as latency.
    f.note_enqueued();
    f.scale_signal.notify_one();
    Ok(())
}

/// Fire and forget: `POST /functions/:id/enqueue`.
async fn enqueue(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    body: Option<Json<InvokeBody>>,
) -> impl IntoResponse {
    let Some(f) = state.registry.get(&id) else {
        return not_found(&id).into_response();
    };
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let event = InvokeEvent::new(&id, body.payload, EventSource::Invoke);

    if let Err(e) = publish(&state, &f, &event).await {
        if e.0 == StatusCode::PAYLOAD_TOO_LARGE {
            state.metrics.record_rejected(&id);
        }
        return e.into_response();
    }

    (
        StatusCode::ACCEPTED,
        Json(EnqueueResponse {
            invocation_id: event.invocation_id,
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct SyncInvokeResponse {
    #[serde(flatten)]
    result: InvokeResult,
    /// Present on backends whose exec path merges the streams, so an empty
    /// `stderr` is not read as "the function wrote nothing to stderr".
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_note: Option<&'static str>,
}

const STDERR_MERGED_NOTE: &str = "this backend's exec path runs the command as `(cmd) 2>&1`, so \
     stderr is folded into stdout and this field is always empty";

/// Blocking invoke: `POST /functions/:id/invoke`.
///
/// Subscribes to the reply subject *before* publishing. Doing it the other way
/// round loses the result whenever a warm VM finishes between the publish and
/// the subscribe — precisely the fast path, and precisely the failure that is
/// hardest to reproduce.
async fn invoke_sync(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    body: Option<Json<InvokeBody>>,
) -> impl IntoResponse {
    let Some(f) = state.registry.get(&id) else {
        return not_found(&id).into_response();
    };
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let mut event = InvokeEvent::new(&id, body.payload, EventSource::Invoke);
    let subject = reply_subject(state.bus.prefix(), &event.invocation_id);
    event.reply_subject = Some(subject.clone());

    let mut sub = match state.bus.client().subscribe(subject).await {
        Ok(s) => s,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("could not subscribe for the result: {e}"),
            )
            .into_response();
        }
    };

    if let Err(e) = publish(&state, &f, &event).await {
        if e.0 == StatusCode::PAYLOAD_TOO_LARGE {
            state.metrics.record_rejected(&id);
        }
        return e.into_response();
    }

    // Counted as a cold-start wait only when there is nothing ready to run it;
    // otherwise this is just a normal invocation that happens to be watched.
    let cold = f.workers().iter().all(|w| !w.is_available());
    if cold {
        state.metrics.record_cold_start_wait(&id);
    }

    let waited = tokio::time::timeout(state.sync_invoke_timeout, sub.next()).await;

    let message = match waited {
        Ok(Some(m)) => m,
        Ok(None) => {
            return err(
                StatusCode::BAD_GATEWAY,
                "the result subscription closed before a result arrived",
            )
            .into_response();
        }
        Err(_) => {
            if cold {
                state.metrics.record_cold_start_timeout(&id);
            }
            // The event is still queued and will still run — the caller just
            // stopped waiting. Say so, rather than implying it was dropped.
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "timed out after {}s waiting for invocation {}; it remains queued and \
                     will still run — poll /functions/{id}/invocations for its result",
                    state.sync_invoke_timeout.as_secs(),
                    event.invocation_id,
                ),
            )
            .into_response();
        }
    };

    if cold {
        state.metrics.record_cold_start_hit(&id);
    }

    let result: InvokeResult = match serde_json::from_slice(&message.payload) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("could not parse the result: {e}"),
            )
            .into_response();
        }
    };

    let code = match result.outcome {
        Outcome::Success | Outcome::Failed => StatusCode::OK,
        Outcome::Timeout => StatusCode::GATEWAY_TIMEOUT,
        Outcome::Error => StatusCode::BAD_GATEWAY,
    };
    let stderr_note = (f.spec.vm.driver == SandboxDriver::Firecracker && result.stderr.is_empty())
        .then_some(STDERR_MERGED_NOTE);

    (
        code,
        Json(SyncInvokeResponse {
            result,
            stderr_note,
        }),
    )
        .into_response()
}

// --- Pause / inspect / DLQ ----------------------------------------------

async fn pause(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    set_paused(state, id, true).await
}

async fn resume(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    set_paused(state, id, false).await
}

/// Pausing stops dispatch without deleting the consumer, so the backlog keeps
/// accumulating and nothing is lost — the alternative (deleting the consumer)
/// would drop queued work on the floor.
async fn set_paused(state: AdminState, id: String, paused: bool) -> Response {
    let Some(f) = state.registry.get(&id) else {
        return not_found(&id).into_response();
    };
    f.set_paused(paused);
    tracing::info!(function = %id, paused, "pause state changed");
    if !paused {
        f.scale_signal.notify_one();
    }
    Json(status_of(&f)).into_response()
}

#[derive(Deserialize)]
struct LimitParams {
    limit: Option<usize>,
}

async fn invocations(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Query(params): Query<LimitParams>,
) -> impl IntoResponse {
    if state.registry.get(&id).is_none() {
        return not_found(&id).into_response();
    }
    let limit = params.limit.unwrap_or(DEFAULT_RECENT);
    Json(state.results.recent(&id, limit)).into_response()
}

async fn dlq_list(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Query(params): Query<LimitParams>,
) -> impl IntoResponse {
    if state.registry.get(&id).is_none() {
        return not_found(&id).into_response();
    }
    let limit = params.limit.unwrap_or(DEFAULT_DLQ_LIMIT);
    match state.bus.dlq_peek(&id, limit).await {
        Ok(records) => Json(records).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

#[derive(Serialize)]
struct ReplayResponse {
    replayed: usize,
}

async fn dlq_replay(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Query(params): Query<LimitParams>,
) -> impl IntoResponse {
    let Some(f) = state.registry.get(&id) else {
        return not_found(&id).into_response();
    };
    let limit = params.limit.unwrap_or(DEFAULT_DLQ_LIMIT);
    let max_payload = f.spec.exec.max_payload_bytes.min(state.max_payload_bytes);

    match state.bus.dlq_replay(&id, limit, max_payload).await {
        Ok(replayed) => {
            tracing::info!(function = %id, replayed, "replayed DLQ records");
            f.scale_signal.notify_one();
            Json(ReplayResponse { replayed }).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

#[derive(Serialize)]
struct PurgeResponse {
    purged: u64,
}

async fn dlq_purge(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    if state.registry.get(&id).is_none() {
        return not_found(&id).into_response();
    }
    match state.bus.dlq_purge(&id).await {
        Ok(purged) => {
            tracing::info!(function = %id, purged, "purged the DLQ");
            Json(PurgeResponse { purged }).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// `?force=true` kills the VM now; otherwise it is drained.
#[derive(Deserialize)]
struct EvictParams {
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct EvictResponse {
    sandbox_id: String,
    /// `"killed"` (gone now) or `"draining"` (reaped once idle).
    outcome: &'static str,
}

/// Evict a single VM: `DELETE /functions/:id/vms/:sandbox_id[?force=true]`.
///
/// The autoscaler boots a replacement on its next tick if the policy still wants
/// the capacity, so this is "recycle this instance", not "shrink the function".
async fn evict_vm(
    State(state): State<AdminState>,
    Path((id, sandbox_id)): Path<(String, String)>,
    Query(params): Query<EvictParams>,
) -> impl IntoResponse {
    let Some(f) = state.registry.get(&id) else {
        return not_found(&id).into_response();
    };

    match state.autoscaler.evict(&f, &sandbox_id, params.force).await {
        EvictOutcome::Killed => (
            StatusCode::OK,
            Json(EvictResponse {
                sandbox_id,
                outcome: "killed",
            }),
        )
            .into_response(),
        // 202: the drain is underway but the VM is not gone yet.
        EvictOutcome::Draining => (
            StatusCode::ACCEPTED,
            Json(EvictResponse {
                sandbox_id,
                outcome: "draining",
            }),
        )
            .into_response(),
        EvictOutcome::NotFound => err(
            StatusCode::NOT_FOUND,
            format!("no VM {sandbox_id:?} in function {id:?}"),
        )
        .into_response(),
        EvictOutcome::KillFailed(e) => {
            err(StatusCode::BAD_GATEWAY, format!("failed to evict the VM: {e}")).into_response()
        }
    }
}

async fn healthz() -> &'static str {
    "ok\n"
}

/// A persist failure is logged, not returned: the change is already live in
/// memory, and failing the request would suggest it hadn't taken effect.
fn persist(state: &AdminState) {
    if let Err(e) = state.registry.persist() {
        tracing::error!(error = %e, "failed to persist state");
    }
}

fn router(state: AdminState) -> Router {
    // The dashboard view and its data source are always behind the gate.
    let view = Router::new()
        .route("/metrics", get(metrics_snapshot))
        .route("/cloud", get(cloud_snapshot))
        .route("/dashboard", get(dashboard));

    // CRUD, plus the reads that expose the spec (env vars can hold secrets).
    let crud = Router::new()
        .route("/functions", post(register).get(list))
        .route("/functions/:id", get(get_one).put(update).delete(deregister))
        .route("/functions/:id/scaling", patch(scale))
        .route("/functions/:id/pause", post(pause))
        .route("/functions/:id/resume", post(resume))
        .route("/functions/:id/vms/:sandbox_id", delete(evict_vm))
        .route("/functions/:id/dlq", get(dlq_list).delete(dlq_purge))
        .route("/functions/:id/dlq/replay", post(dlq_replay));

    // Invocation is gated separately: "anyone may call my functions, nobody may
    // edit them" is a coherent posture, and the reverse is not.
    let invoke_routes = Router::new()
        .route("/functions/:id/invoke", post(invoke_sync))
        .route("/functions/:id/enqueue", post(enqueue))
        .route("/functions/:id/invocations", get(invocations));

    let mut gated = view;
    let mut open = Router::new();
    if state.gate_admin {
        gated = gated.merge(crud);
    } else {
        open = open.merge(crud);
    }
    if state.gate_invoke {
        gated = gated.merge(invoke_routes);
    } else {
        open = open.merge(invoke_routes);
    }

    // `route_layer` runs the auth middleware only for gated routes, so a 404
    // elsewhere never triggers a challenge. `/healthz` stays open for probes.
    let gated = gated.route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(gated)
        .merge(open)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_accepts_only_the_exact_credential() {
        let auth = DashboardAuth::new("admin", "s3cret");
        let token = base64::engine::general_purpose::STANDARD.encode("admin:s3cret");
        assert!(auth.accepts(Some(&format!("Basic {token}"))));

        assert!(!auth.accepts(None), "a missing header is not a credential");
        assert!(!auth.accepts(Some("")), "nor is an empty one");
        assert!(!auth.accepts(Some(&format!("Bearer {token}"))), "wrong scheme");
        assert!(!auth.accepts(Some(&format!("basic {token}"))), "scheme is case-sensitive");

        let wrong = base64::engine::general_purpose::STANDARD.encode("admin:wrong");
        assert!(!auth.accepts(Some(&format!("Basic {wrong}"))));
        let wrong_user = base64::engine::general_purpose::STANDARD.encode("root:s3cret");
        assert!(!auth.accepts(Some(&format!("Basic {wrong_user}"))));
    }

    /// Regression: comparing with `==` short-circuited on the first differing
    /// byte, so response timing leaked how much of the credential was correct
    /// and the password could be recovered one byte at a time.
    #[test]
    fn credential_comparison_does_not_short_circuit() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"), "differs in the last byte");
        assert!(!ct_eq(b"abc", b"xbc"), "differs in the first byte");
        assert!(!ct_eq(b"abc", b"abcd"), "a prefix is not a match");
        assert!(!ct_eq(b"", b"a"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn the_display_name_cannot_inject_markup() {
        let escaped = html_escape(r#"<script>alert("x")</script>&"#);
        assert!(!escaped.contains('<'), "got {escaped}");
        assert!(!escaped.contains('>'));
        assert_eq!(
            escaped,
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;&amp;"
        );
    }

    /// The placeholder must actually exist in the page, or the display name is
    /// silently dropped and every dashboard reads "queue-fn".
    #[test]
    fn the_dashboard_carries_the_name_placeholder() {
        assert!(
            DASHBOARD_HTML.contains("{{APP_NAME}}"),
            "dashboard.html must contain the {{APP_NAME}} placeholder",
        );
        let rendered = DASHBOARD_HTML.replace("{{APP_NAME}}", "my-fn");
        assert!(!rendered.contains("{{APP_NAME}}"));
        assert!(rendered.contains("my-fn"));
    }

    /// The dashboard polls a *relative* URL so it works behind a tunnel or a
    /// path prefix. An absolute `/metrics` would break the moment it is served
    /// under anything but the root.
    #[test]
    fn the_dashboard_polls_a_relative_url() {
        assert!(
            DASHBOARD_HTML.contains(r#"fetch("metrics""#),
            "the metrics poll must use a relative URL",
        );
        assert!(
            !DASHBOARD_HTML.contains(r#"fetch("/metrics""#),
            "an absolute URL breaks the dashboard behind a path prefix",
        );
    }

    /// Every element `renderCloud` writes into has to exist in the markup. A
    /// typo'd id is silent — `$(...)` returns null, the throw is swallowed by
    /// `poll`'s catch, and the page just stops updating.
    #[test]
    fn the_dashboard_has_a_target_for_every_cloud_element_it_writes() {
        for id in [
            "cloud-section",
            "cloud-head",
            "cloud-tiles",
            "cloud-q-chart",
            "cloud-q-meta",
            "cloud-q-note",
            "cloud-kinds",
            "cloud-kind-meta",
            "cloud-workers",
            "cloud-worker-meta",
            "cloud-jobs",
            "cloud-jobs-meta",
        ] {
            assert!(
                DASHBOARD_HTML.contains(&format!("id=\"{id}\"")),
                "dashboard.html has no element with id {id:?}, but the script writes to it",
            );
        }
    }

    /// The cloud panel is hidden until data arrives, not rendered empty: an
    /// empty section reads as "the cloud queue is idle", which is a different
    /// claim from "queue-fn isn't watching one".
    #[test]
    fn the_cloud_section_starts_hidden() {
        assert!(DASHBOARD_HTML.contains(r#"<div id="cloud-section" hidden>"#));
    }

    #[test]
    fn an_oversized_payload_is_rejected_as_the_callers_fault() {
        let (code, _) = publish_error(BusError::PayloadTooLarge { got: 99, max: 10 });
        assert_eq!(
            code,
            StatusCode::PAYLOAD_TOO_LARGE,
            "a payload that will never fit is a 413, not a retryable 502",
        );

        let (code, _) = publish_error(BusError::Publish("nats is down".into()));
        assert_eq!(code, StatusCode::BAD_GATEWAY);
    }
}
