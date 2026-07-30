//! The dashboard and the JSON it runs on.
//!
//! Bound to loopback and reached through app-lb, which terminates TLS and — with
//! an `auth` block on the deployment spec — puts sign-in in front of it. Nothing
//! here authenticates, because a second gate on the same door only adds a second
//! thing to get wrong. What *is* here is the refusal to be a load problem: every
//! query takes a slot from a bounded pool and a deadline, so a month-wide
//! refresh degrades into a 503 rather than competing with ingest for the machine.
//!
//! The endpoints are typed rather than a SQL passthrough. Each one is a query
//! this module built, so partition pruning and a row cap are always applied —
//! neither is something a caller can forget.

use crate::ingest::Sink;
use crate::query::{
    Engine, HOST_DEPLOYMENT, LogBucket, LogFilter, MAX_LOG_LIMIT, MetricBucket, QueryError, Window,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The dashboard page. Self-contained — no external fetches — so it works on a
/// host with no route to the internet, which is most of them.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Ranges the dashboard offers, longest label first for the picker.
///
/// A closed set rather than a free-form duration: each of these has a rung on
/// the bucket ladder that yields a sensible number of points, and an arbitrary
/// window does not.
const WINDOWS: &[(&str, i64)] = &[
    ("15m", 900),
    ("1h", 3_600),
    ("6h", 21_600),
    ("24h", 86_400),
    ("7d", 604_800),
    ("30d", 2_592_000),
];

/// Points to aim for across a window. The overview draws a row-height sparkline
/// per deployment and the detail page draws full-width charts, so they want
/// different resolutions from the same range.
const FLEET_POINTS: u32 = 40;
const DETAIL_POINTS: u32 = 140;

#[derive(Clone)]
pub struct ApiState {
    pub engine: Arc<Engine>,
    pub sink: Sink,
    /// Rows buffered in the writer, published by the drain task.
    pub buffered: Arc<AtomicUsize>,
    pub flush_secs: u64,
    pub retain_days: u32,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        // The bare hostname should land somewhere useful; app-lb routes a whole
        // host here, so `/` is what someone actually types.
        .route("/", get(|| async { Redirect::temporary("/dashboard") }))
        .route("/dashboard", get(dashboard))
        .route("/api/fleet", get(fleet))
        .route("/api/deployments/{id}", get(detail))
        .route("/api/deployments/{id}/logs", get(logs))
        // Always open, and deliberately not behind a query slot: app-lb health
        // checks this, and a probe that fails because a dashboard is busy would
        // take the deployment out of rotation for no reason.
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/stats", get(stats))
        .with_state(state)
}

async fn dashboard() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

/// Ingest counters plus what is still in memory.
async fn stats(State(state): State<ApiState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "accepted": state.sink.accepted(),
        "dropped": state.sink.dropped(),
        "buffered_rows": state.buffered.load(Ordering::Relaxed),
        "flush_secs": state.flush_secs,
        "retain_days": state.retain_days,
    }))
}

#[derive(Debug, Deserialize)]
struct WindowParams {
    window: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LogParams {
    window: Option<String>,
    level: Option<String>,
    backend: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
    before: Option<i64>,
}

/// What is on disk right now, and how stale it might be.
///
/// `buffered_rows` is the reason this is here at all: a partition flushes on a
/// timer, so the newest rows are legitimately not queryable yet. Without saying
/// so, a dashboard that has just been shown a burst of traffic looks like it lost
/// it.
#[derive(Debug, Serialize)]
struct Freshness {
    buffered_rows: usize,
    flush_secs: u64,
    dropped: u64,
}

#[derive(Debug, Serialize)]
struct FleetResponse {
    generated_at_ms: i64,
    from_ms: i64,
    to_ms: i64,
    step_secs: u32,
    window: String,
    windows: Vec<String>,
    retain_days: u32,
    freshness: Freshness,
    /// Whole-host CPU and memory. `None` when nothing has ever landed under
    /// `_host` — app-lb reports it only once the daemon has sampled the host, and
    /// "never sampled" should not look like "idle".
    host: Option<Vec<MetricBucket>>,
    deployments: Vec<FleetRow>,
}

#[derive(Debug, Serialize)]
struct FleetRow {
    id: String,
    /// Coarse series behind the row's sparkline.
    buckets: Vec<MetricBucket>,
    log_buckets: Vec<LogBucket>,
    /// Most recent non-null of each measure in the window — see `latest_of`.
    latest: MetricBucket,
    log_lines: u64,
    error_logs: u64,
}

#[derive(Debug, Serialize)]
struct DetailResponse {
    id: String,
    generated_at_ms: i64,
    from_ms: i64,
    to_ms: i64,
    step_secs: u32,
    window: String,
    windows: Vec<String>,
    retain_days: u32,
    freshness: Freshness,
    buckets: Vec<MetricBucket>,
    log_buckets: Vec<LogBucket>,
    latest: MetricBucket,
    log_lines: u64,
    error_logs: u64,
    /// Backends that logged in the window, for the log filter.
    backends: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LogsResponse {
    id: String,
    from_ms: i64,
    to_ms: i64,
    rows: Vec<crate::query::LogRow>,
    /// Pass back as `before` for the next page, or `None` at the end.
    ///
    /// Inclusive, so a burst spanning a page edge is re-sent rather than lost;
    /// the caller drops lines it already has.
    next_before_ms: Option<i64>,
    limit: usize,
}

async fn fleet(
    State(state): State<ApiState>,
    Query(params): Query<WindowParams>,
) -> Result<Json<FleetResponse>, ApiError> {
    let (label, window) = resolve_window(params.window.as_deref());
    let step = window.step_secs(FLEET_POINTS);

    // One scan for the whole fleet rather than one per row: `deployment` is a
    // partition column, so the engine still only opens the directories the
    // window touches.
    let metrics = state.engine.metrics(window, step, None).await?;
    let volume = state.engine.log_volume(window, step, None).await?;

    let mut deployments = Vec::new();
    for id in state.engine.deployments() {
        let buckets = metrics.get(&id).cloned().unwrap_or_default();
        let log_buckets = volume.get(&id).cloned().unwrap_or_default();
        deployments.push(FleetRow {
            latest: latest_of(&buckets),
            log_lines: log_buckets.iter().map(|b| b.lines).sum(),
            error_logs: log_buckets.iter().map(|b| b.errors).sum(),
            id,
            buckets,
            log_buckets,
        });
    }

    Ok(Json(FleetResponse {
        generated_at_ms: Utc::now().timestamp_millis(),
        from_ms: window.start_ms(),
        to_ms: window.end_ms(),
        step_secs: step,
        window: label.to_string(),
        windows: window_labels(),
        retain_days: state.retain_days,
        freshness: freshness(&state),
        // An empty vec and `None` mean different things: the first is "host
        // samples exist, none in this window", the second is "the daemon has
        // never reported host usage at all".
        host: metrics
            .get(HOST_DEPLOYMENT)
            .cloned()
            .or_else(|| state.engine.has_host_data().then(Vec::new)),
        deployments,
    }))
}

async fn detail(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<WindowParams>,
) -> Result<Json<DetailResponse>, ApiError> {
    let (label, window) = resolve_window(params.window.as_deref());
    let step = window.step_secs(DETAIL_POINTS);

    let metrics = state.engine.metrics(window, step, Some(&id)).await?;
    let volume = state.engine.log_volume(window, step, Some(&id)).await?;
    let backends = state.engine.backends(window, &id).await?;

    let buckets = metrics.get(&id).cloned().unwrap_or_default();
    let log_buckets = volume.get(&id).cloned().unwrap_or_default();

    Ok(Json(DetailResponse {
        generated_at_ms: Utc::now().timestamp_millis(),
        from_ms: window.start_ms(),
        to_ms: window.end_ms(),
        step_secs: step,
        window: label.to_string(),
        windows: window_labels(),
        retain_days: state.retain_days,
        freshness: freshness(&state),
        latest: latest_of(&buckets),
        log_lines: log_buckets.iter().map(|b| b.lines).sum(),
        error_logs: log_buckets.iter().map(|b| b.errors).sum(),
        id,
        buckets,
        log_buckets,
        backends,
    }))
}

async fn logs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<LogParams>,
) -> Result<Json<LogsResponse>, ApiError> {
    let (_, window) = resolve_window(params.window.as_deref());
    let limit = params.limit.unwrap_or(200).clamp(1, MAX_LOG_LIMIT);

    let filter = LogFilter {
        deployment: id.clone(),
        // An empty query string is what a cleared form field sends. Treating it
        // as a filter would match everything or nothing depending on the
        // operator, and either way it isn't what was meant.
        level: non_empty(params.level),
        backend: non_empty(params.backend),
        search: non_empty(params.q),
        limit,
        before_ms: params.before,
    };

    let rows = state.engine.logs(window, &filter).await?;
    // Only offer another page when this one filled up. A short page is the end
    // of the data, and a `next` that returns nothing invites an endless scroll.
    let next_before_ms = (rows.len() >= limit)
        .then(|| rows.last().map(|r| r.ts))
        .flatten();

    Ok(Json(LogsResponse {
        id,
        from_ms: window.start_ms(),
        to_ms: window.end_ms(),
        rows,
        next_before_ms,
        limit,
    }))
}

fn freshness(state: &ApiState) -> Freshness {
    Freshness {
        buffered_rows: state.buffered.load(Ordering::Relaxed),
        flush_secs: state.flush_secs,
        dropped: state.sink.dropped(),
    }
}

fn window_labels() -> Vec<String> {
    WINDOWS.iter().map(|(l, _)| (*l).to_string()).collect()
}

/// Resolve a window label, falling back to a day.
///
/// An unrecognised label gets the default rather than a 400: this arrives from a
/// URL someone may have bookmarked or hand-edited, and showing them a day of
/// data is more use than an error about a query parameter.
fn resolve_window(label: Option<&str>) -> (&'static str, Window) {
    let now = Utc::now();
    let (label, seconds) = label
        .and_then(|want| WINDOWS.iter().find(|(l, _)| *l == want))
        .copied()
        .unwrap_or(("24h", 86_400));
    (label, Window::trailing(now, seconds))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// The most recent non-null value of each measure in the window.
///
/// Field by field rather than "the last bucket", because the measures do not
/// arrive together: latency is null in any bucket that served no request, and a
/// tile that blanks out because the final bucket happened to be quiet reads as a
/// broken collector. `t` is the last bucket's, so the page can say how old this
/// is.
fn latest_of(buckets: &[MetricBucket]) -> MetricBucket {
    let mut latest = MetricBucket::default();
    for bucket in buckets {
        latest.t = bucket.t;
        for (into, from) in [
            (&mut latest.requests_per_sec, bucket.requests_per_sec),
            (&mut latest.errors_per_sec, bucket.errors_per_sec),
            (&mut latest.mean_latency_ms, bucket.mean_latency_ms),
            (&mut latest.p50_ms, bucket.p50_ms),
            (&mut latest.p90_ms, bucket.p90_ms),
            (&mut latest.p99_ms, bucket.p99_ms),
            (&mut latest.cpu_percent, bucket.cpu_percent),
            (&mut latest.memory_bytes, bucket.memory_bytes),
            (&mut latest.in_flight, bucket.in_flight),
            (&mut latest.ready, bucket.ready),
            (&mut latest.pending, bucket.pending),
            (&mut latest.draining, bucket.draining),
        ] {
            if from.is_some() {
                *into = from;
            }
        }
    }
    latest
}

/// A query failure, as a status the dashboard can act on.
struct ApiError(QueryError);

impl From<QueryError> for ApiError {
    fn from(e: QueryError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self.0 {
            // Not an error in the deployment, so not a 500: the caller should
            // come back, and app-lb should not conclude anything is wrong.
            QueryError::Busy => StatusCode::SERVICE_UNAVAILABLE,
            QueryError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            QueryError::BadDeployment(_) => StatusCode::BAD_REQUEST,
            QueryError::Engine(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if let QueryError::Engine(e) = &self.0 {
            tracing::error!(error = %e, "query failed");
        }
        // The message goes back as well as to the log: this is an operator's
        // tool, and "something went wrong" would just mean two places to look.
        let body = Json(serde_json::json!({ "error": self.0.to_string() }));
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(t: i64, cpu: Option<f64>, latency: Option<f64>) -> MetricBucket {
        MetricBucket {
            t,
            cpu_percent: cpu,
            mean_latency_ms: latency,
            ..Default::default()
        }
    }

    #[test]
    fn a_quiet_final_bucket_does_not_blank_the_tiles() {
        // Latency is null in any bucket that served no request. Reading the tile
        // off the last bucket alone would blank a perfectly healthy deployment
        // the moment traffic paused.
        let latest = latest_of(&[
            bucket(1000, Some(10.0), Some(8.0)),
            bucket(2000, Some(12.0), None),
        ]);
        assert_eq!(latest.cpu_percent, Some(12.0), "newest sample wins");
        assert_eq!(latest.mean_latency_ms, Some(8.0), "carried forward");
        assert_eq!(latest.t, 2000, "timestamp is the newest bucket's");
    }

    #[test]
    fn nothing_measured_stays_nothing() {
        // An empty window must not invent zeros; the dashboard renders these as
        // dashes.
        let latest = latest_of(&[]);
        assert_eq!(latest.cpu_percent, None);
        assert_eq!(latest.requests_per_sec, None);
        assert_eq!(latest.t, 0);
    }

    #[test]
    fn an_unknown_window_falls_back_instead_of_failing() {
        assert_eq!(resolve_window(Some("6h")).0, "6h");
        assert_eq!(resolve_window(None).0, "24h");
        // A hand-edited or stale URL should still show something.
        assert_eq!(resolve_window(Some("99y")).0, "24h");
        assert_eq!(resolve_window(Some("")).0, "24h");
    }

    #[test]
    fn window_lengths_all_have_a_workable_bucket_width() {
        // Every offered range must produce a plottable number of points at both
        // resolutions — no thousand-point sparkline, no two-point chart.
        for (label, seconds) in WINDOWS {
            let window = Window::trailing(Utc::now(), *seconds);
            for target in [FLEET_POINTS, DETAIL_POINTS] {
                let step = window.step_secs(target);
                let points = seconds / i64::from(step);
                assert!(
                    (2..=600).contains(&points),
                    "{label} at {target} points gives {points} buckets of {step}s",
                );
            }
        }
    }

    #[test]
    fn a_cleared_form_field_is_not_a_filter() {
        assert_eq!(non_empty(Some("error".into())), Some("error".into()));
        assert_eq!(non_empty(Some("".into())), None);
        assert_eq!(non_empty(Some("   ".into())), None);
        assert_eq!(non_empty(None), None);
    }
}
