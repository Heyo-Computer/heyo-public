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
use crate::siem::AuthAction;
use crate::tls::CertStore;
use async_trait::async_trait;
use axum::extract::{ConnectInfo, MatchedPath, Path, Query, Request, State};
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
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Notify;

/// The live dashboard page. Self-contained (no external fetches beyond the
/// same-origin `/metrics` poll) so it works over an SSH tunnel with no assets.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// The server-rendered landing page at `/`. Self-contained for the same reason
/// the dashboard is: it has to work over an SSH tunnel with no route out.
const DIRECTORY_HTML: &str = include_str!("directory.html");

/// The security console at `GET /siem`.
const SIEM_HTML: &str = include_str!("siem.html");

/// The disk console at `GET /storage`.
const DISKS_HTML: &str = include_str!("disks.html");

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
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
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
    /// The directory shell, with the display name already substituted. Only
    /// `{{LEDE}}` and `{{CARDS}}` are left, and those are filled per request
    /// because the registry moves.
    directory_html: Arc<str>,
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
    /// App-tokens. Verified on every gated request, so reads are lock-free.
    tokens: Arc<crate::tokens::TokenStore>,
    /// Runs image builds and host updates, and remembers what they did.
    jobs: Arc<Jobs>,
    /// Nudges the ACME manager to issue for a newly-registered hostname instead
    /// of waiting out its sweep interval. `None` when ACME is disabled.
    acme: Option<Arc<Notify>>,
    /// Counters for the app-obs log shipper. `None` when log shipping is off.
    obs: Option<Arc<crate::obs::Stats>>,
    /// Queues rejected credentials for analysis. `None` when `APP_LB_SIEM=0`.
    security: Option<crate::siem::SecuritySink>,
    /// Findings, for `GET /security`. `None` when `APP_LB_SIEM=0`.
    alerts: Option<Arc<crate::siem::AlertRing>>,
    /// Counters for the detection engine, reported beside `obs` on `/metrics`.
    siem: Option<Arc<crate::siem::SiemStats>>,
    /// The block rules the data plane enforces. Always present, unlike the SIEM:
    /// a rule an operator created must keep working whether or not detection is
    /// switched on.
    guard: Arc<crate::guard::Guard>,
    /// The SIEM console, with the display name already substituted.
    siem_html: Arc<str>,
    /// Per-sandbox disk inventory and reclamation. `None` when the daemon's data
    /// directory could not be resolved, which is the only way it is off.
    disks: Option<Arc<crate::disks::DiskStore>>,
    /// The disk console, with the display name already substituted.
    disks_html: Arc<str>,
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
        tokens: Arc<crate::tokens::TokenStore>,
        jobs: Arc<Jobs>,
        obs: Option<Arc<crate::obs::Stats>>,
        siem: Option<&crate::siem::Siem>,
        guard: Arc<crate::guard::Guard>,
        disks: Option<Arc<crate::disks::DiskStore>>,
        public_url: PublicUrl,
    ) -> Self {
        // Render the display name into the page once; the placeholder appears in
        // both the <title> and the <h1>.
        let dashboard_html: Arc<str> =
            Arc::from(DASHBOARD_HTML.replace("{{APP_NAME}}", &html_escape(&name)));
        let directory_html: Arc<str> =
            Arc::from(DIRECTORY_HTML.replace("{{APP_NAME}}", &html_escape(&name)));
        let siem_html: Arc<str> = Arc::from(SIEM_HTML.replace("{{APP_NAME}}", &html_escape(&name)));
        let disks_html: Arc<str> =
            Arc::from(DISKS_HTML.replace("{{APP_NAME}}", &html_escape(&name)));

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
                directory_html,
                auth,
                gate_admin,
                started_at: now_secs(),
                certs,
                acme,
                secrets,
                tokens,
                jobs,
                obs,
                security: siem.map(|s| s.sink.clone()),
                alerts: siem.map(|s| s.ring.clone()),
                siem: siem.map(|s| s.stats.clone()),
                guard,
                siem_html,
                disks,
                disks_html,
                public_url,
            },
        }
    }
}

/// How a request was identified, and what that permits.
///
/// Placed in the request's extensions by the gate, so a handler that needs to
/// know who is asking can say so in its signature. Most do not: the gate itself
/// enforces both the tier and the deployment scope (see [`authorize`]), which
/// keeps the policy in one auditable place rather than in fifteen handlers where
/// exactly one would eventually be forgotten.
#[derive(Clone, Debug)]
pub(crate) enum Caller {
    /// No gate is configured. This build is not checking credentials at all.
    Ungated,
    /// The configured Basic credential. Unscoped by definition — it is the
    /// credential that *mints* tokens, so it necessarily outranks every token it
    /// could produce.
    Operator,
    /// An app-token, carrying its own scope.
    Token(Arc<crate::tokens::AppToken>),
}

impl Caller {
    fn satisfies(&self, want: crate::tokens::AdminScope) -> bool {
        match self {
            Self::Ungated | Self::Operator => true,
            Self::Token(t) => t.admin.satisfies(want),
        }
    }

    fn may_touch(&self, deployment: &str) -> bool {
        match self {
            Self::Ungated | Self::Operator => true,
            Self::Token(t) => t.allows(deployment),
        }
    }

    /// Whether this caller may use a route that is not about any one deployment
    /// — creating one, listing them all, reading the secret store.
    fn covers_fleet(&self) -> bool {
        match self {
            Self::Ungated | Self::Operator => true,
            Self::Token(t) => t.covers_fleet(),
        }
    }

    /// The deployments this caller may see, or `None` for all of them. Used to
    /// narrow `/metrics` rather than to refuse it: a token scoped to one sandbox
    /// should be able to watch that sandbox.
    fn visible(&self) -> Option<&[String]> {
        match self {
            Self::Ungated | Self::Operator => None,
            Self::Token(t) if t.covers_fleet() => None,
            Self::Token(t) => Some(&t.deployments),
        }
    }
}

/// Routes that are not about a single deployment but that a deployment-scoped
/// token may still reach, because the handler narrows the answer to that token's
/// scope instead of refusing it.
fn narrows_itself(matched: &str) -> bool {
    // `/siem` is here for the same reason `/dashboard` is: it is a static page
    // that narrows itself from `/security`, so a deployment-scoped token should
    // get the console rather than a 403. `/security/rules` is deliberately
    // absent — a scoped token has no business arming a fleet-wide block.
    matches!(
        matched,
        "/" | "/metrics" | "/dashboard" | "/security" | "/siem"
    )
}

/// The deployment a matched route acts on, if it acts on one.
///
/// Read off the *matched* path rather than the raw URI so this cannot be fooled
/// by a path that merely looks like a deployment route, and the id is then taken
/// positionally from the real path — every such route is `/deployments/:id/…`,
/// so the id is always the second segment.
fn deployment_of<'a>(matched: &str, path: &'a str) -> Option<&'a str> {
    if matched != "/deployments/:id" && !matched.starts_with("/deployments/:id/") {
        return None;
    }
    path.split('/').nth(2).filter(|s| !s.is_empty())
}

/// `Bearer <token>`, if that is what was presented.
fn bearer(header: Option<&str>) -> Option<&str> {
    let raw = header?.strip_prefix("Bearer ")?.trim();
    (!raw.is_empty()).then_some(raw)
}

/// `?app_token=…`, accepted on the shell route and nowhere else.
///
/// A credential in a URL is worse than one in a header — it lands in access
/// logs, proxy logs and browser history — so this exists for exactly one reason:
/// a browser's `WebSocket` constructor cannot set headers, and a query parameter
/// is the only thing it *can* carry. Every other route can use a header, so
/// every other route must.
fn ws_query_token(matched: &str, query: Option<&str>) -> Option<String> {
    if matched != "/deployments/:id/shell" {
        return None;
    }
    form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k == "app_token")
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            // Both schemes are advertised, in the order a browser should try
            // them: a browser can only do Basic, and offering Bearer first would
            // suppress its native login prompt on some clients.
            "Basic realm=\"app-lb dashboard\", charset=\"UTF-8\", Bearer",
        )],
        "authentication required\n",
    )
        .into_response()
}

/// A 403 rather than a 401: the credential was good, the scope was not, and
/// re-presenting it will not help. The message names the missing scope, because
/// the alternative is somebody rotating a working token trying to fix a
/// permission problem.
fn forbidden(detail: impl Into<String>) -> Response {
    err(StatusCode::FORBIDDEN, detail).into_response()
}

/// What the gate decided about one request.
#[derive(Debug)]
enum Verdict {
    Allow(Caller),
    /// No usable credential. Answered with a challenge.
    Unauthorized,
    /// The credential was good and the scope was not.
    Forbidden(String),
}

/// Everything the gate needs to know about a request, as plain data.
///
/// A struct rather than a borrowed `Request` so the decision is a pure function
/// — the same shape `auth.rs` uses for the data-plane gate, and for the same
/// reason: authorization logic that can only be exercised through a live socket
/// is authorization logic that does not get exercised.
struct Presented<'a> {
    /// The `Authorization` header, verbatim.
    header: Option<&'a str>,
    /// The route pattern axum matched, e.g. `/deployments/:id/exec`.
    matched: Option<&'a str>,
    /// The concrete request path.
    path: &'a str,
    query: Option<&'a str>,
}

/// Decide whether a request gets through, and as whom.
///
/// Two credentials are accepted:
///
/// - the configured Basic username/password, which is unscoped, and
/// - an app-token as `Authorization: Bearer applb_…` (or `?app_token=` on the
///   shell route only), which carries its own scope.
///
/// `auth: None` means no gate is configured and everything is permitted.
fn decide_access(
    auth: Option<&DashboardAuth>,
    tokens: &crate::tokens::TokenStore,
    req: &Presented<'_>,
    want: crate::tokens::AdminScope,
    now: u64,
) -> Verdict {
    let Some(auth) = auth else {
        return Verdict::Allow(Caller::Ungated);
    };

    let caller = if auth.accepts(req.header) {
        Some(Caller::Operator)
    } else {
        bearer(req.header)
            .map(str::to_owned)
            .or_else(|| ws_query_token(req.matched.unwrap_or_default(), req.query))
            .and_then(|raw| tokens.verify(&raw, now))
            .map(Caller::Token)
    };

    let Some(caller) = caller else {
        return Verdict::Unauthorized;
    };

    if !caller.satisfies(want) {
        return Verdict::Forbidden(
            match want {
                crate::tokens::AdminScope::Admin => {
                    "this token's admin scope is not `admin`, which this route requires"
                }
                _ => "this token has no admin scope, so it cannot read the admin API",
            }
            .into(),
        );
    }

    // Deployment scope. Every route is one of three things: about a named
    // deployment, able to narrow itself, or fleet-wide.
    if let Some(matched) = req.matched {
        match deployment_of(matched, req.path) {
            Some(id) if !caller.may_touch(id) => {
                return Verdict::Forbidden(format!(
                    "this token is not scoped to deployment \"{id}\""
                ));
            }
            None if !narrows_itself(matched) && !caller.covers_fleet() => {
                return Verdict::Forbidden(
                    "this token is scoped to specific deployments, so it cannot use a \
                     fleet-wide route — mint one scoped to \"*\" if that is what you want"
                        .into(),
                );
            }
            _ => {}
        }
    }

    Verdict::Allow(caller)
}

/// Gate the protected routes when a credential is configured.
///
/// A `401` carries the `WWW-Authenticate` challenge so a browser shows its
/// native login prompt and caches the credentials for same-origin requests. A
/// bad *scope* is a `403` and says so.
async fn authorize(
    state: AdminState,
    mut req: Request,
    next: Next,
    want: crate::tokens::AdminScope,
) -> Response {
    let matched = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string());
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_owned);
    // Requires `into_make_service_with_connect_info` on the listener; without it
    // this is always `None` and every alert raised here loses its source.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());

    let verdict = decide_access(
        state.auth.as_deref(),
        &state.tokens,
        &Presented {
            header: header.as_deref(),
            matched: matched.as_deref(),
            path: &path,
            query: query.as_deref(),
        },
        want,
        now_secs(),
    );

    // A rejected credential was invisible before this: the gate answered 401 or
    // 403 and logged nothing, so a password spray against the dashboard left no
    // trace anywhere. The `tracing::warn!` earns its place independently of the
    // SIEM — it works with `APP_LB_SIEM=0`, and `obs::EventLayer` already ships
    // it to app-obs.
    let scheme = crate::siem::AuthScheme::of(header.as_deref());
    match verdict {
        Verdict::Allow(caller) => {
            req.extensions_mut().insert(caller);
            next.run(req).await
        }
        Verdict::Unauthorized => {
            tracing::warn!(
                path = %path,
                scheme = scheme.as_str(),
                client = ?peer,
                "admin API request rejected: no usable credential",
            );
            observe_auth_failure(&state, peer, &path, AuthAction::AdminRejected, scheme);
            unauthorized()
        }
        Verdict::Forbidden(detail) => {
            tracing::warn!(
                path = %path,
                scheme = scheme.as_str(),
                client = ?peer,
                detail = %detail,
                "admin API request rejected: credential is out of scope",
            );
            observe_auth_failure(&state, peer, &path, AuthAction::AdminScope, scheme);
            forbidden(detail)
        }
    }
}

/// Queue one rejected credential for analysis, if the SIEM is running.
///
/// Never carries the credential — not the password, not the token, not a prefix
/// of either. Only the *scheme*, which is what separates token guessing from an
/// unauthenticated probe.
fn observe_auth_failure(
    state: &AdminState,
    peer: Option<std::net::IpAddr>,
    path: &str,
    action: AuthAction,
    scheme: crate::siem::AuthScheme,
) {
    if let Some(siem) = &state.security {
        siem.observe_auth(crate::siem::AuthObs {
            ts: crate::obs::now_millis(),
            client: peer,
            deployment: None,
            path: Box::from(path),
            action,
            scheme,
            subject: None,
        });
    }
}

async fn require_view_auth(State(state): State<AdminState>, req: Request, next: Next) -> Response {
    authorize(state, req, next, crate::tokens::AdminScope::View).await
}

async fn require_crud_auth(State(state): State<AdminState>, req: Request, next: Next) -> Response {
    authorize(state, req, next, crate::tokens::AdminScope::Admin).await
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

/// Which `POST` a deployment's code is redeployed with, if any.
///
/// At most one is ever configured, because validation refuses the combinations:
/// a `proxy_pass` upstream has no image to build, a microVM has no host
/// directory to run commands in, and a site takes `update` or `artifact` but
/// never both. So this is one answer rather than three flags.
fn job_kind_of(spec: &DeploymentSpec) -> Option<&'static str> {
    if spec.build.is_some() {
        Some("build")
    } else if spec.artifact.is_some() {
        Some("pull")
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
    /// Detection-engine counters and a live alert tally, absent when
    /// `APP_LB_SIEM=0`. On the *fast* poll rather than only on `/security` so an
    /// alert reaches the dashboard's stat tiles within two seconds, and so a
    /// dropping queue is visible next to `obs.dropped`, which fails the same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    security: Option<SecuritySummary>,
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

/// The three numbers the dashboard's alert tile needs, without the alert list.
///
/// Separate from [`SecurityResponse`] so the two-second metrics poll does not
/// carry a few hundred alerts it is not going to render.
#[derive(Serialize)]
struct SecuritySummary {
    /// Alerts currently held in the ring.
    open: usize,
    /// How many of those are high or critical — what the tile colours on.
    urgent: u64,
    /// Observations refused because the queue was full. Non-zero means detection
    /// is sampling rather than complete.
    dropped: u64,
    /// Whether the per-client table is full, which means the same thing for
    /// sources rather than for events.
    clients_at_capacity: bool,
    /// Block rules in force, and how many requests they have refused. On the
    /// summary because "we are blocking traffic" belongs next to "we are seeing
    /// attacks" — an operator who forgot a rule exists should trip over it here.
    rules: usize,
    blocked: u64,
}

/// `GET /security`.
///
/// Behind the same gate as `/metrics`, and deliberately: it enumerates attacker
/// addresses and the exact probes that reached the fleet.
#[derive(Serialize)]
struct SecurityResponse {
    generated_at: u64,
    /// `false` with an empty list when `APP_LB_SIEM=0`, rather than a 404. The
    /// dashboard has to be able to render "off"; a 404 is indistinguishable from
    /// an app-lb too old to have this route.
    enabled: bool,
    window_secs: u64,
    /// Newest first, which is the order the dashboard renders. Each carries its
    /// own `response` — the runbook and the ready-to-post rules — so the console
    /// never has to derive "and now what?" from the rule name in JavaScript.
    alerts: Vec<crate::siem::AlertView>,
    totals: crate::siem::SeverityTotals,
    /// The block rules in force. Served here rather than from a route of their
    /// own so the console renders findings and interventions from one fetch, and
    /// cannot show a stale rule list beside fresh alerts.
    rules: Vec<crate::guard::RuleView>,
    guard: crate::guard::GuardStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<crate::siem::SiemSnapshot>,
}

/// Query parameters for `GET /security`.
#[derive(Debug, Default, Deserialize)]
struct SecurityQuery {
    /// Only alerts at or above this severity.
    severity: Option<String>,
    /// Only this rule, e.g. `auth.brute-force`.
    rule: Option<String>,
    /// Only alerts attributed to this deployment.
    deployment: Option<String>,
    limit: Option<usize>,
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
    caller: Option<axum::Extension<Caller>>,
) -> impl IntoResponse {
    let deployments = state.registry.deployments();

    // A deployment-scoped token gets a *narrowed* answer rather than a 403: a
    // token minted to drive one sandbox should be able to watch that sandbox.
    // `None` means unscoped, which is the operator credential and the ungated
    // case both.
    let scope = caller.as_ref().and_then(|c| c.visible());
    let in_scope = |id: &str| scope.is_none_or(|s| s.iter().any(|d| d == id));

    // The fleet rollup covers the whole registry, never the filter or the page.
    // It sits under "Host & fleet" alongside the global metrics, and a number
    // there that moved when somebody typed in the table's search box would be
    // describing the query rather than the system.
    //
    // "The whole registry" still means the whole *visible* registry: a scoped
    // token must not learn the size of the fleet from a total it can't itemise.
    let mut fleet = FleetPool {
        deployments: 0,
        ready: 0,
        draining: 0,
        pending: 0,
        total_in_flight: 0,
    };
    for d in deployments.values().filter(|d| in_scope(&d.spec.id)) {
        let pool = pool_status_of(d);
        fleet.deployments += 1;
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
        .filter(|d| in_scope(&d.spec.id))
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
    // For a scoped token the rollup is over what it can see, not over the fleet.
    // `global_snapshot` also folds in *retired* deployments' counters, which a
    // scoped caller has no business receiving.
    //
    // `host` is left alone deliberately: whole-machine CPU and memory is not an
    // inventory of deployments, and a sandbox operator watching the load on the
    // box their VM sits on is reasonable.
    let (global, tracked) = match scope {
        None => (
            state.metrics.global_snapshot(),
            state.metrics.tracked_deployments(),
        ),
        Some(ids) => {
            let mut merged = crate::metrics::DeploymentMetricsSnapshot::empty();
            for id in ids {
                merged.merge(&state.metrics.deployment_snapshot(id));
            }
            (merged, ids.len())
        }
    };
    Json(MetricsResponse {
        generated_at: now,
        uptime_secs: now.saturating_sub(state.started_at),
        host: state.metrics.host_snapshot(),
        fleet,
        global,
        obs: state.obs.as_ref().map(|o| o.snapshot()),
        security: security_summary(&state),
        deployments: views,
        matched,
        tracked_deployments: tracked,
    })
}

/// The alert tile's numbers. Fleet-wide regardless of any deployment filter, as
/// `host`/`fleet`/`global` are — a filter narrows the table, not the system.
fn security_summary(state: &AdminState) -> Option<SecuritySummary> {
    let (ring, stats) = (state.alerts.as_ref()?, state.siem.as_ref()?);
    let totals = ring.totals();
    let s = stats.snapshot();
    let g = state.guard.stats(now_secs());
    Some(SecuritySummary {
        open: ring.len(),
        urgent: totals.high + totals.critical,
        dropped: s.dropped,
        clients_at_capacity: s.clients_at_capacity,
        rules: g.rules,
        blocked: g.blocked,
    })
}

/// The rules a caller may see.
///
/// Narrowed the same way alerts are: a deployment-scoped token sees the rules
/// that name its own deployment and nothing else. A fleet-wide block is fleet
/// information — it says which addresses are considered hostile, which is the
/// same disclosure the alert list is gated for.
fn visible_rules(
    state: &AdminState,
    scope: Option<&[String]>,
    now: u64,
) -> Vec<crate::guard::RuleView> {
    let enforcing = state.guard.enforcing();
    state
        .guard
        .list()
        .into_iter()
        .filter(|r| match scope {
            None => true,
            Some(ids) => r
                .deployment()
                .is_some_and(|d| ids.iter().any(|id| id == d)),
        })
        // `report`, not `view`: the console charts each rule's recent hits, and
        // that series is what answers "is this rule still doing anything, or am
        // I paying for a branch on every request for nothing?".
        .map(|r| r.report(enforcing, now))
        .collect()
}

/// `GET /security` — the findings, newest first.
///
/// Narrowed for a deployment-scoped token exactly as `/metrics` is, with one
/// extra rule: alerts carrying no deployment are dropped for such a caller.
/// Those are the admin-plane and unrouted-traffic findings, and the set of
/// addresses attacking the LB itself is fleet information a sandbox-scoped token
/// has no business reading.
async fn security_snapshot(
    State(state): State<AdminState>,
    Query(q): Query<SecurityQuery>,
    caller: Option<axum::Extension<Caller>>,
) -> impl IntoResponse {
    let now = now_secs();
    let scope = caller.as_ref().and_then(|c| c.0.visible());

    // Drop rules that have run out on the way past. Expiry is already enforced
    // in `Guard::decide`, so this is tidying rather than correctness — but a
    // console listing rules that stopped doing anything yesterday is a console
    // nobody trusts. Not persisted here: a `GET` that writes to disk is a
    // surprise, and the next mutation or restart rewrites the file anyway.
    state.guard.sweep(now);

    let Some(ring) = state.alerts.as_ref() else {
        // Enabled:false rather than 404 — see `SecurityResponse::enabled`. The
        // rules still come back: enforcement does not depend on detection, and a
        // console that hid the active blocks whenever `APP_LB_SIEM=0` would hide
        // the one thing still affecting traffic.
        return Json(SecurityResponse {
            generated_at: now,
            enabled: false,
            window_secs: 0,
            alerts: Vec::new(),
            totals: crate::siem::SeverityTotals {
                info: 0,
                low: 0,
                medium: 0,
                high: 0,
                critical: 0,
            },
            rules: visible_rules(&state, scope, now),
            guard: state.guard.stats(now),
            stats: None,
        });
    };

    let min = q.severity.as_deref().and_then(crate::siem::Severity::parse);
    let limit = q.limit.unwrap_or(200).min(1000);

    // Read the whole ring and filter, rather than filtering inside it: the ring
    // is a few hundred entries and this keeps the lock hold to one clone.
    let alerts = ring
        .recent(usize::MAX)
        .into_iter()
        .filter(|a| match scope {
            None => true,
            Some(ids) => a
                .deployment
                .as_deref()
                .is_some_and(|d| ids.iter().any(|id| id == d)),
        })
        .filter(|a| min.is_none_or(|m| a.severity >= m))
        .filter(|a| q.rule.as_deref().is_none_or(|r| a.rule == r))
        .filter(|a| {
            q.deployment
                .as_deref()
                .is_none_or(|d| a.deployment.as_deref() == Some(d))
        })
        .take(limit)
        .map(crate::siem::AlertView::from)
        .collect();

    Json(SecurityResponse {
        generated_at: now,
        enabled: true,
        window_secs: ring.window_secs(),
        alerts,
        totals: ring.totals(),
        rules: visible_rules(&state, scope, now),
        guard: state.guard.stats(now),
        // Fleet-wide counters: withheld from a scoped caller, for whom they
        // would describe traffic they cannot see.
        stats: scope
            .is_none()
            .then(|| state.siem.as_ref().map(|s| s.snapshot()))
            .flatten(),
    })
}

// ---- guard rules ----------------------------------------------------------

/// Map a guard rejection onto a status. Everything a caller can get wrong is a
/// 400 except the cap, which is a 409 — that one is about the server's state
/// rather than about the request, and retrying the identical body after
/// deleting a rule is the correct next move.
fn guard_error(e: crate::guard::GuardError) -> Response {
    use crate::guard::GuardError as E;
    let code = match &e {
        E::EmptyMatch | E::BadClient(_) | E::TooLong(_) => StatusCode::BAD_REQUEST,
        E::Full => StatusCode::CONFLICT,
        E::NoRule(_) => StatusCode::NOT_FOUND,
        E::Io(_) | E::Json(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    err(code, e.to_string()).into_response()
}

/// `POST /security/rules` — start refusing something.
///
/// CRUD tier, not view. This is the only route on the admin API that can stop
/// traffic reaching a deployment, so it sits behind the same credentials as
/// editing the spec — a dashboard-only token may read the console and may not
/// arm it.
async fn create_rule(
    State(state): State<AdminState>,
    Json(spec): Json<crate::guard::RuleSpec>,
) -> Response {
    let now = now_secs();
    let rule = match state.guard.insert(spec, now) {
        Ok(r) => r,
        Err(e) => return guard_error(e),
    };
    // Persist before answering. A 200 for a rule that a crash would lose is the
    // wrong way round for a control somebody just used to stop an attack.
    if let Err(e) = state.guard.persist() {
        // The rule is live in memory either way, so undoing it here would be
        // worse than saying what happened: report the failure and leave the
        // block in force.
        tracing::error!(error = %e, path = %state.guard.path().display(), "guard rules not saved");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "the rule is in force but could not be written to {}: {e} — it will not \
                 survive a restart",
                state.guard.path().display()
            ),
        )
        .into_response();
    }
    tracing::warn!(
        rule = %rule.id,
        action = ?rule.action,
        matched = %rule.describe(),
        expires_at = ?rule.expires_at,
        "guard rule created",
    );
    (StatusCode::CREATED, Json(rule.view(state.guard.enforcing()))).into_response()
}

#[derive(Debug, Deserialize)]
struct RuleExpiryBody {
    /// Seconds from now, or `null` for a rule that never expires. Required —
    /// absent is refused rather than read as "forever", because making a block
    /// permanent by omission is exactly the accident this field exists to
    /// prevent.
    #[serde(default, deserialize_with = "double_option")]
    expires_in_secs: Option<Option<u64>>,
}

/// `PATCH /security/rules/:id` — change when a rule expires, including never.
///
/// The counterpart to the bounded lifetime every suggested action carries. That
/// default is right for a rule authored mid-incident, and wrong once an operator
/// has decided an address is simply not welcome; re-posting the rule to make it
/// permanent would work but would reset its hit history, which is the evidence
/// the decision rests on.
async fn patch_rule(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<RuleExpiryBody>,
) -> Response {
    let Some(expires_in_secs) = body.expires_in_secs else {
        return err(
            StatusCode::BAD_REQUEST,
            "expires_in_secs is required: a number of seconds, or null to keep the rule \
             indefinitely",
        )
        .into_response();
    };
    let now = now_secs();
    match state.guard.set_expiry(&id, expires_in_secs, now) {
        Ok(view) => {
            if let Err(e) = state.guard.persist() {
                tracing::error!(error = %e, "guard rules not saved after expiry change");
            }
            match expires_in_secs {
                None => tracing::warn!(
                    rule = %id,
                    summary = %view.summary,
                    "guard rule made permanent; it will now outlive the incident that created it",
                ),
                Some(secs) => tracing::info!(rule = %id, secs, "guard rule expiry extended"),
            }
            Json(view).into_response()
        }
        Err(e) => guard_error(e),
    }
}

/// `DELETE /security/rules/:id` — stop refusing it.
async fn delete_rule(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    let now = now_secs();
    if let Err(e) = state.guard.remove(&id, now) {
        return guard_error(e);
    }
    if let Err(e) = state.guard.persist() {
        tracing::error!(error = %e, "guard rules not saved after delete");
    }
    tracing::warn!(rule = %id, "guard rule removed");
    StatusCode::NO_CONTENT.into_response()
}

// ---- the SIEM console -----------------------------------------------------

/// `GET /siem` — the security console.
///
/// Its own page rather than a bigger card on `/dashboard`, because the two are
/// read at different moments and at different depths. The dashboard answers "is
/// the fleet healthy" in a glance and must stay glanceable; this answers "what
/// is attacking us and what do I do about it", which needs the full alert list,
/// the ECS fields behind each one, and the buttons that change what the data
/// plane does. The dashboard card links here and keeps its summary.
async fn siem_console(State(state): State<AdminState>) -> impl IntoResponse {
    Html(state.siem_html.to_string())
}

// ---- disks ----------------------------------------------------------------

/// The store, or the 503 that explains why there isn't one.
///
/// Disk management is the one subsystem that can be *absent* rather than merely
/// idle: without a resolvable daemon data directory there is nothing to
/// inventory. Saying so with the fix in it beats a 404 on a route the page is
/// hard-coded to call.
fn disks_off() -> Response {
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        "disk management is off: app-lb could not work out where heyvmd keeps its \
         per-sandbox disks. Set APP_LB_VM_DATA_DIR to the daemon's data directory \
         (MVM_DATA_DIR, or ~/.heyo) and restart",
    )
    .into_response()
}

fn disk_error(e: crate::disks::DiskError) -> Response {
    use crate::disks::DiskError as E;
    let code = match &e {
        E::BadId(_) => StatusCode::BAD_REQUEST,
        E::NotFound(_) => StatusCode::NOT_FOUND,
        // 409, not 403: the request was permitted, the disk's state refuses it,
        // and the caller can change that state.
        E::Held { .. } | E::AlreadyArchiving(_) | E::NothingToArchive(_) => StatusCode::CONFLICT,
        E::NoArchiveTarget => StatusCode::NOT_IMPLEMENTED,
        // 500, not 409: nothing about the disk's state refuses this and no
        // `force=1` gets past it. Something on the host — almost always the
        // ownership of the daemon's data directory — stopped app-lb deleting
        // files it was told to delete, and that is the server's problem to fix.
        E::PurgeFailed { .. } | E::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    err(code, e.to_string()).into_response()
}

/// `GET /disks` — every per-sandbox disk on this host.
///
/// View tier, like `/metrics` and `/security`: the console renders it, so the
/// browser's cached view credentials have to work. Fleet-wide, so a
/// deployment-scoped token is refused — a disk inventory spans deployments and
/// includes sandboxes no deployment owns any more.
async fn disks(State(state): State<AdminState>) -> Response {
    let Some(store) = state.disks.as_ref() else {
        return disks_off();
    };
    Json(store.inventory().await).into_response()
}

/// `GET /storage` — the disk console.
async fn storage_console(State(state): State<AdminState>) -> impl IntoResponse {
    Html(state.disks_html.to_string())
}

#[derive(Debug, Deserialize)]
struct DiskPolicyBody {
    /// Absent leaves the flag alone, so a note can be edited without touching
    /// retention and vice versa.
    #[serde(default)]
    retain: Option<bool>,
    /// `null` clears the note; absent leaves it.
    #[serde(default, deserialize_with = "double_option")]
    note: Option<Option<String>>,
}

/// Distinguish "absent" from "present and null" in a JSON body.
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

/// `PATCH /disks/:id` — pin a disk against expiry, or annotate it.
async fn patch_disk(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<DiskPolicyBody>,
) -> Response {
    let Some(store) = state.disks.as_ref() else {
        return disks_off();
    };
    if let Err(e) = store.set_policy(&id, body.retain, body.note) {
        return disk_error(e);
    }
    if let Some(retain) = body.retain {
        tracing::info!(sandbox = %id, retain, "disk retention changed");
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Default, Deserialize)]
struct ForceQuery {
    #[serde(default)]
    force: Option<String>,
}

impl ForceQuery {
    fn on(&self) -> bool {
        matches!(
            self.force.as_deref().map(str::trim),
            Some("1" | "true" | "yes" | "on")
        )
    }
}

/// `DELETE /disks/:id` — reclaim a sandbox's disks.
///
/// CRUD tier, and the most destructive route app-lb has: it deletes gigabytes
/// with no undo. `?force=1` overrides the "a deployment expects to resume it"
/// and "the daemon is unreachable" guards; it does *not* override the running
/// check, which has no legitimate override.
async fn purge_disk(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Query(q): Query<ForceQuery>,
) -> Response {
    let Some(store) = state.disks.as_ref() else {
        return disks_off();
    };
    match store.purge(&id, q.on()).await {
        Ok(outcome) => Json(outcome).into_response(),
        Err(e) => disk_error(e),
    }
}

#[derive(Debug, Default, Deserialize)]
struct ArchiveBody {
    /// Reclaim the disks once the upload succeeds. Never on failure.
    #[serde(default)]
    purge: bool,
}

/// `POST /disks/:id/archive` — stream a sandbox's disks to S3.
///
/// Answers as soon as the upload starts; progress arrives on `GET /disks`
/// alongside the inventory, which is what the console polls anyway.
async fn archive_disk(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    body: Option<Json<ArchiveBody>>,
) -> Response {
    let Some(store) = state.disks.as_ref() else {
        return disks_off();
    };
    let purge = body.map(|Json(b)| b.purge).unwrap_or(false);
    match store.archive(&id, purge).await {
        Ok(view) => (StatusCode::ACCEPTED, Json(view)).into_response(),
        Err(e) => disk_error(e),
    }
}

/// `POST /disks/sweep` — run the expiry sweep now instead of at the next tick.
async fn sweep_disks(State(state): State<AdminState>) -> Response {
    let Some(store) = state.disks.as_ref() else {
        return disks_off();
    };
    Json(store.sweep().await).into_response()
}

// ---- the directory page -------------------------------------------------

/// One clickable destination.
///
/// A card per *URL* rather than per deployment: a deployment with three
/// hostnames genuinely offers three places to go, and this page exists to be
/// clicked.
struct DirectoryEntry {
    url: String,
    deployment: String,
    kind: &'static str,
    state: EntryState,
    /// What is behind it, in a few words — "2 of 3 VMs ready", "serving /srv/www".
    detail: String,
    /// Behind a sign-in gate. Worth saying before the click rather than after:
    /// following one of these can bounce through the provider, and knowing which
    /// cards will do that is the difference between "slow" and "broken".
    gated: bool,
}

/// Whether following the link right now would reach anything.
#[derive(PartialEq)]
enum EntryState {
    Ready,
    /// Nothing available yet, but a VM is booting — a cold start, not an outage.
    Starting,
    /// Registered and routable with nothing healthy behind it.
    Down,
    /// A site: files on this host, so there is no backend to be up or down.
    Files,
}

impl EntryState {
    fn dot(&self) -> &'static str {
        match self {
            Self::Ready | Self::Files => "ready",
            Self::Starting => "starting",
            Self::Down => "down",
        }
    }
}

/// Collect what the directory shows, narrowed to what this caller may see.
///
/// Returns the linkable entries and, separately, the ids of deployments that are
/// registered but have no URL a browser could be sent to. Those are reported
/// rather than silently omitted: a directory that quietly drops things is worse
/// than one that explains the gap, and "why isn't my deployment listed" has
/// exactly one cause here.
fn directory_entries(
    state: &AdminState,
    scope: Option<&[String]>,
) -> (Vec<DirectoryEntry>, Vec<String>) {
    let deployments = state.registry.deployments();
    let mut entries = Vec::new();
    let mut unlinkable = Vec::new();

    for d in deployments.values() {
        // Both operands are pure reads, so collapsing these is safe.
        if let Some(ids) = scope
            && !ids.iter().any(|id| id.as_str() == d.spec.id.as_str())
        {
            continue;
        }

        let urls: Vec<String> = d
            .spec
            .routes
            .iter()
            .filter_map(|r| state.public_url.of(r))
            .collect();
        if urls.is_empty() {
            unlinkable.push(d.spec.id.clone());
            continue;
        }

        let kind = deployment_kind(d);
        let (entry_state, detail) = match d.spec.backend() {
            // No backend by construction — app-lb answers these itself, so there
            // is nothing that can be down.
            crate::config::Backend::Site => (
                EntryState::Files,
                match d.spec.site.as_ref() {
                    Some(s) => format!("serving {}", s.root),
                    None => "static files".to_string(),
                },
            ),
            crate::config::Backend::Upstreams => {
                let backends = d.backends();
                let up = backends.iter().filter(|b| b.is_available()).count();
                let total = backends.len().max(d.spec.upstreams.len());
                (
                    if up > 0 { EntryState::Ready } else { EntryState::Down },
                    format!("{up} of {total} {} up", plural(total, "upstream")),
                )
            }
            crate::config::Backend::Vm => {
                let backends = d.backends();
                let up = backends.iter().filter(|b| b.is_available()).count();
                let pending = d.pending().len();
                if up > 0 {
                    (
                        EntryState::Ready,
                        format!("{up} {} ready", plural(up, "VM")),
                    )
                } else if pending > 0 {
                    (
                        EntryState::Starting,
                        format!("{pending} {} booting", plural(pending, "VM")),
                    )
                } else if d.spec.scaling.min_replicas == 0 {
                    // Scale-to-zero is the configured state, not a fault: the
                    // first request boots a VM. Saying "down" here would send
                    // somebody debugging a system that is working.
                    (EntryState::Starting, "idle — starts on first request".into())
                } else {
                    (EntryState::Down, "no healthy VMs".into())
                }
            }
        };

        let gated = d.spec.auth.is_some();
        for url in urls {
            entries.push(DirectoryEntry {
                url,
                deployment: d.spec.id.clone(),
                kind,
                state: match entry_state {
                    EntryState::Ready => EntryState::Ready,
                    EntryState::Starting => EntryState::Starting,
                    EntryState::Down => EntryState::Down,
                    EntryState::Files => EntryState::Files,
                },
                detail: detail.clone(),
                gated,
            });
        }
    }

    // Stable order, so a reload does not reshuffle the page under a cursor.
    entries.sort_by(|a, b| a.deployment.cmp(&b.deployment).then(a.url.cmp(&b.url)));
    unlinkable.sort();
    (entries, unlinkable)
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// The cards, as HTML.
///
/// Pure, so the escaping and the empty states are testable without a listener.
/// Every interpolation goes through [`html_escape`] — a deployment id and a
/// hostname are operator-supplied rather than attacker-supplied, but they reach
/// this page from a JSON body over the admin API, which is close enough to
/// untrusted that the distinction is not worth relying on.
fn render_directory_cards(entries: &[DirectoryEntry], unlinkable: &[String]) -> String {
    let note = if unlinkable.is_empty() {
        String::new()
    } else {
        let ids = unlinkable
            .iter()
            .map(|id| format!("<code>{}</code>", html_escape(id)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "<div class=\"note\">Not shown: {ids} — a route with only a \
             <code>host_suffix</code> or a <code>path_prefix</code> names no single \
             hostname to link to.</div>"
        )
    };

    if entries.is_empty() {
        let body = if unlinkable.is_empty() {
            "Nothing is registered yet. <code>POST /deployments</code>, or \
             <code>serverctl apply</code>, and it appears here."
        } else {
            "No deployment has a linkable hostname."
        };
        return format!("<div class=\"empty\">{body}</div>{note}");
    }

    let cards = entries
        .iter()
        .map(|e| {
            format!(
                "<a class=\"card\" href=\"{url}\">\
                   <div class=\"card-head\">\
                     <span class=\"id\">{id}</span>\
                     {gate}\
                     <span class=\"tag {kind}\">{kind}</span>\
                   </div>\
                   <div class=\"url\">{url}</div>\
                   <div class=\"meta\"><span class=\"dot {dot}\"></span>{detail}</div>\
                 </a>",
                url = html_escape(&e.url),
                id = html_escape(&e.deployment),
                kind = e.kind,
                dot = e.state.dot(),
                detail = html_escape(&e.detail),
                gate = if e.gated {
                    "<span class=\"tag gated\" title=\"Google sign-in required\">sign-in</span>"
                } else {
                    ""
                },
            )
        })
        .collect::<String>();

    format!("<div class=\"grid\">{cards}</div>{note}")
}

/// One line under the title: what this page is showing.
fn directory_lede(entries: &[DirectoryEntry]) -> String {
    if entries.is_empty() {
        return "No deployments are routable yet.".into();
    }
    // Only the deployments these URLs actually came from. Counting the
    // unlinkable ones here would claim URLs across deployments that contributed
    // none; they get their own line under the cards instead.
    let deployments = {
        let mut ids: Vec<&str> = entries.iter().map(|e| e.deployment.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    };
    let down = entries.iter().filter(|e| e.state == EntryState::Down).count();
    let mut s = format!(
        "{} {} across {} {}.",
        entries.len(),
        plural(entries.len(), "URL"),
        deployments,
        plural(deployments, "deployment"),
    );
    if down > 0 {
        s.push_str(&format!(" {down} with nothing healthy behind {}.",
            if down == 1 { "it" } else { "them" }));
    }
    s
}

/// `GET /` — a directory of everything this app-lb routes.
///
/// Server-rendered, unlike `/dashboard`: it is a landing page that should be
/// complete in its first response, work without JavaScript, and not hold a
/// polling connection open. The cards are built per request because the
/// underlying registry changes; the app name is substituted once at startup.
async fn directory(
    State(state): State<AdminState>,
    caller: Option<axum::Extension<Caller>>,
) -> impl IntoResponse {
    let scope = caller.as_ref().and_then(|c| c.0.visible());
    let (entries, unlinkable) = directory_entries(&state, scope);
    let html = state
        .directory_html
        .replace("{{LEDE}}", &html_escape(&directory_lede(&entries)))
        .replace("{{CARDS}}", &render_directory_cards(&entries, &unlinkable));
    Html(html)
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
        // The landing page. Same tier as the dashboard: it lists every hostname
        // this app-lb routes, which is the same inventory `/metrics` exposes.
        .route("/", get(directory))
        .route("/metrics", get(metrics_snapshot))
        .route("/dashboard", get(dashboard))
        // View tier, not CRUD: the dashboard is its consumer, so the browser's
        // cached view credentials have to work. It must never be ungated — it
        // enumerates attacker addresses and the probes that reached the fleet —
        // and this group is the one that is gated whenever a password is set.
        .route("/security", get(security_snapshot))
        // The console that reads it. View tier for the same reason the dashboard
        // is: it renders `/security`, so it must work with whatever credentials
        // the browser already has. The buttons on it post to the CRUD tier and
        // will be refused for a view-only caller — which is the intended split,
        // and the page says so rather than failing silently.
        .route("/siem", get(siem_console))
        // The disk inventory and the console that renders it, on the same tier
        // and for the same reason. Reading what is on the host is a view-tier
        // question; every route that *changes* it is on the CRUD side below.
        .route("/disks", get(disks))
        .route("/storage", get(storage_console));

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
        // Blocking traffic is a mutation with more blast radius than most of the
        // ones above it, so it belongs on this side of the gate. Reads live on
        // `GET /security` with the alerts, which is why there is no `get` here.
        .route("/security/rules", post(create_rule))
        .route("/security/rules/:id", patch(patch_rule).delete(delete_rule))
        // Disk mutations. `DELETE /disks/:id` deletes gigabytes with no undo,
        // which puts it firmly on this side of the gate — it is the single most
        // destructive route app-lb exposes. `/disks/sweep` is registered as a
        // static segment and so cannot be reached by naming a sandbox `sweep`;
        // matchit prefers a literal over a parameter.
        .route("/disks/sweep", post(sweep_disks))
        .route("/disks/:id", patch(patch_disk).delete(purge_disk))
        .route("/disks/:id/archive", post(archive_disk))
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
        .route("/jobs/:job_id", get(get_job))
        // App-tokens. Firmly CRUD-tier: minting one is minting a credential, so
        // the route that does it must be at least as protected as the things the
        // credential can reach.
        .route("/tokens", post(mint_token).get(list_tokens))
        .route(
            "/tokens/:id",
            get(get_token).patch(patch_token).delete(revoke_token),
        );

    // `route_layer` runs the auth middleware only for the routes it wraps, so a
    // 404 elsewhere never triggers a challenge. `/healthz` is always open.
    //
    // Two layers rather than one, because the tiers want different scopes: a
    // `view` token may read `/metrics`, and only an `admin` one may reach the
    // CRUD routes. When `gate_admin` is off the CRUD routes carry no layer at
    // all — the pre-existing behaviour, and what `main()` warns loudly about.
    let (crud, open) = if state.gate_admin {
        (
            crud.route_layer(middleware::from_fn_with_state(
                state.clone(),
                require_crud_auth,
            )),
            Router::new(),
        )
    } else {
        (Router::new(), crud)
    };
    let view = view.route_layer(middleware::from_fn_with_state(
        state.clone(),
        require_view_auth,
    ));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(view)
        .merge(crud)
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

        // `into_make_service_with_connect_info` rather than the bare router:
        // without it there is no `ConnectInfo` extension anywhere in the admin
        // plane, so every rejected credential would be recorded with no source
        // address and the brute-force rule could never fire. That failure is
        // silent — the SIEM looks healthy and detects nothing — which is why it
        // is worth a comment rather than just a call.
        //
        // The admin listener defaults to 127.0.0.1, so in the usual deployment
        // this is a loopback address and the *count* is the useful part; the
        // address only means something when the port is exposed directly.
        let served = axum::serve(
            listener,
            router(self.state.clone()).into_make_service_with_connect_info::<SocketAddr>(),
        )
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

// -- app-tokens --------------------------------------------------------------

/// What `POST /tokens` answers with: the summary, plus the secret.
///
/// The secret appears here and in no other response, ever. There is no endpoint
/// that reads one back, because only its hash is kept — losing a token means
/// minting a replacement and revoking the old one, which is the behaviour you
/// want from a credential anyway.
#[derive(Serialize)]
struct MintedToken {
    #[serde(flatten)]
    summary: crate::tokens::TokenSummary,
    /// Store this now. It cannot be retrieved again.
    token: String,
}

fn token_error(e: crate::tokens::TokenError) -> Response {
    use crate::tokens::TokenError;
    let code = match &e {
        TokenError::NoToken(_) => StatusCode::NOT_FOUND,
        TokenError::EmptyName | TokenError::NameTooLong | TokenError::BadScope(_) => {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    err(code, e.to_string()).into_response()
}

/// Write the token file, turning a failure into a 500.
///
/// Unlike a deployment write — which logs and carries on, because the running
/// pool is the source of truth — a token that exists in memory but not on disk
/// is a credential that silently stops working at the next restart. The caller
/// is told instead.
/// `Box`ed because axum's `Response` is a large type, and this sits in the
/// `Err` half of a `Result` that several handlers thread through.
fn persist_tokens(state: &AdminState) -> Result<(), Box<Response>> {
    state.tokens.persist().map_err(|e| {
        tracing::error!(path = %state.tokens.path().display(), error = %e, "token file write failed");
        Box::new(
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the token was not saved: {e}"),
            )
            .into_response(),
        )
    })
}

async fn mint_token(
    State(state): State<AdminState>,
    Json(req): Json<crate::tokens::NewToken>,
) -> Response {
    let name = req.name.clone();
    let (summary, token) = match state.tokens.mint(req, now_secs()) {
        Ok(v) => v,
        Err(e) => return token_error(e),
    };
    if let Err(e) = persist_tokens(&state) {
        // Roll back, so a token that could not be saved is not one that works
        // until the next restart and then mysteriously stops.
        state.tokens.revoke(&summary.id);
        return *e;
    }
    tracing::info!(
        token = %summary.id,
        name = %name,
        admin = ?summary.admin,
        deployments = ?summary.deployments,
        "app-token minted",
    );
    (StatusCode::CREATED, Json(MintedToken { summary, token })).into_response()
}

async fn list_tokens(State(state): State<AdminState>) -> impl IntoResponse {
    // Expired tokens already fail verification; drop them here so the listing
    // shows live credentials rather than a graveyard that looks like one.
    if state.tokens.sweep_expired(now_secs()) > 0 {
        let _ = state.tokens.persist();
    }
    Json(state.tokens.list())
}

async fn get_token(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    match state.tokens.get(&id) {
        Some(t) => Json(t.summary()).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("no token {id:?}")).into_response(),
    }
}

async fn patch_token(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(patch): Json<crate::tokens::TokenPatch>,
) -> Response {
    let before = state.tokens.get(&id);
    let summary = match state.tokens.patch(&id, patch) {
        Ok(s) => s,
        Err(e) => return token_error(e),
    };
    if let Err(e) = persist_tokens(&state) {
        if let Some(before) = before {
            state.tokens.restore(before);
        }
        return *e;
    }
    tracing::info!(token = %id, admin = ?summary.admin, deployments = ?summary.deployments, "app-token updated");
    Json(summary).into_response()
}

async fn revoke_token(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    let before = state.tokens.get(&id);
    if !state.tokens.revoke(&id) {
        return err(StatusCode::NOT_FOUND, format!("no token {id:?}")).into_response();
    }
    if let Err(e) = persist_tokens(&state) {
        // A revocation that did not reach disk would come back at the next
        // restart, which is the worst possible direction for this to fail in.
        if let Some(before) = before {
            state.tokens.restore(before);
        }
        return *e;
    }
    tracing::info!(token = %id, "app-token revoked");
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two routing decisions behind the security console, both of which are
    /// silent when wrong: one produces a 403 for a caller who should see the
    /// page, the other hands a scoped token a fleet-wide control.
    #[test]
    fn the_console_narrows_itself_and_the_rule_api_does_not() {
        for view in ["/", "/metrics", "/dashboard", "/security", "/siem"] {
            assert!(narrows_itself(view), "{view} must narrow for a scoped token");
        }
        // A deployment-scoped token has no business arming a fleet-wide block,
        // so these must fall through to the "does not cover the fleet" refusal.
        for mutating in ["/security/rules", "/security/rules/:id"] {
            assert!(!narrows_itself(mutating), "{mutating} must not narrow");
        }
    }

    #[test]
    fn a_guard_rejection_maps_onto_a_status_a_client_can_act_on() {
        use crate::guard::GuardError as E;
        let status = |e: E| guard_error(e).status();
        assert_eq!(status(E::EmptyMatch), StatusCode::BAD_REQUEST);
        assert_eq!(status(E::BadClient("x".into())), StatusCode::BAD_REQUEST);
        assert_eq!(status(E::TooLong("match.host")), StatusCode::BAD_REQUEST);
        // The cap is about the server's state, not the request: deleting a rule
        // and retrying the identical body is the correct next move.
        assert_eq!(status(E::Full), StatusCode::CONFLICT);
        assert_eq!(status(E::NoRule("x".into())), StatusCode::NOT_FOUND);
    }

    /// The landing page at `/`. Rendering is a pure function of the collected
    /// entries, so all of this runs without a listener or a registry.
    mod directory {
        use super::*;

        fn entry(id: &str, url: &str, state: EntryState) -> DirectoryEntry {
            DirectoryEntry {
                url: url.into(),
                deployment: id.into(),
                kind: "vm",
                state,
                detail: "2 VMs ready".into(),
                gated: false,
            }
        }

        /// Following a gated card can bounce through Google, so the card says so
        /// first. With `auth.cookie_domain` set across the fleet that bounce
        /// happens once rather than once per card — see `AuthGate::cookie_domain`.
        #[test]
        fn a_gated_destination_is_labelled_before_it_is_clicked() {
            let open = entry("api", "https://api.example.com", EntryState::Ready);
            let gated = DirectoryEntry {
                gated: true,
                ..entry("private", "https://private.example.com", EntryState::Ready)
            };
            let html = render_directory_cards(&[open, gated], &[]);
            assert_eq!(html.matches("tag gated").count(), 1, "{html}");
            assert!(html.contains(">sign-in<"), "{html}");
        }

        #[test]
        fn a_card_links_to_the_data_plane_url() {
            let html = render_directory_cards(
                &[entry("demo", "https://demo.example.com", EntryState::Ready)],
                &[],
            );
            assert!(html.contains(r#"href="https://demo.example.com""#));
            assert!(html.contains(">demo<"));
            assert!(html.contains("dot ready"));
        }

        /// A deployment with several hostnames offers several destinations, and
        /// this page exists to be clicked — so each gets its own card.
        #[test]
        fn every_url_gets_its_own_card() {
            let html = render_directory_cards(
                &[
                    entry("demo", "https://a.example.com", EntryState::Ready),
                    entry("demo", "https://b.example.com", EntryState::Ready),
                ],
                &[],
            );
            assert_eq!(html.matches("class=\"card\"").count(), 2);
        }

        /// Silently omitting them would make "why isn't my deployment listed?"
        /// unanswerable from the page.
        #[test]
        fn a_deployment_with_no_linkable_host_is_explained_not_dropped() {
            let html = render_directory_cards(
                &[entry("demo", "https://demo.example.com", EntryState::Ready)],
                &["internal-only".into()],
            );
            assert!(html.contains("internal-only"));
            assert!(html.contains("host_suffix"));
        }

        #[test]
        fn an_empty_fleet_says_how_to_fill_it() {
            let html = render_directory_cards(&[], &[]);
            assert!(html.contains("Nothing is registered yet"));
            assert!(html.contains("POST /deployments"));
        }

        /// Everything on this page arrives through the admin API's JSON, which
        /// is close enough to untrusted that escaping is not optional.
        #[test]
        fn operator_supplied_text_is_escaped() {
            let html = render_directory_cards(
                &[entry(
                    "<script>alert(1)</script>",
                    "https://x/\"><img src=x onerror=alert(1)>",
                    EntryState::Ready,
                )],
                &["<b>bad</b>".into()],
            );
            assert!(!html.contains("<script>alert"), "id must be escaped");
            assert!(!html.contains("<img src=x"), "url must be escaped");
            assert!(!html.contains("<b>bad"), "the note must be escaped too");
            assert!(html.contains("&lt;script&gt;"));
        }

        #[test]
        fn the_lede_counts_urls_deployments_and_outages() {
            let entries = vec![
                entry("a", "https://a1", EntryState::Ready),
                entry("a", "https://a2", EntryState::Ready),
                entry("b", "https://b1", EntryState::Down),
            ];
            let lede = directory_lede(&entries);
            assert!(lede.contains("3 URLs"), "{lede}");
            assert!(lede.contains("2 deployments"), "{lede}");
            assert!(lede.contains("1 with nothing healthy behind it"), "{lede}");
        }

        /// A deployment that contributes no URL must not be counted in "across
        /// N deployments" — it has its own line under the cards.
        #[test]
        fn the_lede_counts_only_deployments_that_contributed_a_url() {
            // The unlinkable one is reported under the cards, not counted here.
            let lede = directory_lede(&[entry("a", "https://a1", EntryState::Ready)]);
            assert!(lede.contains("1 URL across 1 deployment."), "{lede}");
        }

        #[test]
        fn the_lede_stays_grammatical_in_the_singular() {
            let lede = directory_lede(&[entry("a", "https://a1", EntryState::Ready)]);
            assert!(lede.contains("1 URL across 1 deployment."), "{lede}");
        }

        /// Scale-to-zero is the configured state, not a fault. Calling it "down"
        /// would send somebody debugging a system that is working as asked.
        #[test]
        fn an_idle_scale_to_zero_deployment_is_not_reported_as_down() {
            let html = render_directory_cards(
                &[DirectoryEntry {
                    detail: "idle — starts on first request".into(),
                    ..entry("demo", "https://demo", EntryState::Starting)
                }],
                &[],
            );
            assert!(html.contains("dot starting"));
            assert!(!html.contains("dot down"));
        }

        /// The shell must have no placeholder left in it after rendering, or the
        /// page ships literal `{{CARDS}}` to a browser.
        #[test]
        fn the_template_has_exactly_the_placeholders_the_handler_fills() {
            assert!(DIRECTORY_HTML.contains("{{APP_NAME}}"));
            assert!(DIRECTORY_HTML.contains("{{LEDE}}"));
            assert!(DIRECTORY_HTML.contains("{{CARDS}}"));
            let rendered = DIRECTORY_HTML
                .replace("{{APP_NAME}}", "app-lb")
                .replace("{{LEDE}}", &directory_lede(&[]))
                .replace("{{CARDS}}", &render_directory_cards(&[], &[]));
            assert!(!rendered.contains("{{"), "an unfilled placeholder would ship to a browser");
        }
    }

    /// The disk console. Unlike the directory it is client-rendered, so the
    /// only server-side contract is the display name and the route it polls.
    mod disk_console {
        use super::*;

        #[test]
        fn the_page_has_only_the_placeholder_the_constructor_fills() {
            assert!(DISKS_HTML.contains("{{APP_NAME}}"));
            let rendered = DISKS_HTML.replace("{{APP_NAME}}", "app-lb");
            assert!(
                !rendered.contains("{{"),
                "an unfilled placeholder would ship to a browser",
            );
        }

        /// The page hard-codes the routes it calls, so a rename here has to
        /// break a test rather than a browser.
        #[test]
        fn the_page_calls_the_routes_the_router_registers() {
            for route in [
                "\"GET\", \"/disks\"",
                "\"PATCH\", \"/disks/\"",
                "/archive`",
                "\"POST\", \"/disks/sweep\"",
                "\"DELETE\", \"/disks/\"",
            ] {
                assert!(DISKS_HTML.contains(route), "page never calls {route}");
            }
        }

        /// Sandbox ids, paths and daemon error strings all reach this markup.
        #[test]
        fn the_page_escapes_what_it_interpolates() {
            assert!(DISKS_HTML.contains("function esc(v)"));
            assert!(DISKS_HTML.contains("encodeURIComponent(id)"));
        }

        /// The status code is the whole contract with the page: it offers the
        /// force override on a 409 and nothing else, so a guard that answered
        /// 403 would be unoverridable and one that answered 500 would look like
        /// a bug.
        #[test]
        fn each_refusal_maps_to_the_code_the_page_acts_on() {
            use crate::disks::DiskError as E;
            let code = |e: E| disk_error(e).status();

            assert_eq!(
                code(E::Held {
                    sandbox_id: "sb-1".into(),
                    reason: "the sandbox is running",
                    forceable: false,
                }),
                StatusCode::CONFLICT,
                "the page offers ?force=1 on a 409 and only on a 409",
            );
            assert_eq!(
                code(E::AlreadyArchiving("sb-1".into())),
                StatusCode::CONFLICT
            );
            assert_eq!(
                code(E::NothingToArchive("sb-1".into())),
                StatusCode::CONFLICT
            );
            assert_eq!(code(E::BadId("../etc".into())), StatusCode::BAD_REQUEST);
            assert_eq!(code(E::NotFound("sb-1".into())), StatusCode::NOT_FOUND);
            // Not 500: nothing is broken, the feature is simply unconfigured.
            assert_eq!(code(E::NoArchiveTarget), StatusCode::NOT_IMPLEMENTED);
            assert_eq!(
                code(E::Io("disk full".into())),
                StatusCode::INTERNAL_SERVER_ERROR
            );
        }

        /// Every refusal has to say what to do next — these messages are shown
        /// verbatim in a `confirm()` dialog, and the page decides whether to
        /// offer the force retry by looking for `force=1` in this very string.
        /// A message that promises an override the server would refuse produces
        /// a dialog whose "yes" fails a second time.
        #[test]
        fn only_a_forceable_refusal_advertises_the_override() {
            use crate::disks::DiskError as E;

            let forceable = E::Held {
                sandbox_id: "sb-1".into(),
                reason: "a deployment expects to resume it",
                forceable: true,
            }
            .to_string();
            assert!(forceable.contains("force=1"), "{forceable}");
            assert!(forceable.contains("sb-1"), "{forceable}");

            let never = E::Held {
                sandbox_id: "sb-1".into(),
                reason: "the sandbox is running; evict or stop it first",
                forceable: false,
            }
            .to_string();
            assert!(!never.contains("force"), "{never}");
            // It still has to say what *would* work.
            assert!(never.contains("stop it first"), "{never}");

            let no_target = E::NoArchiveTarget.to_string();
            assert!(no_target.contains("APP_LB_DISK_ARCHIVE_BUCKET"), "{no_target}");
        }

        /// The page's force-retry condition, pinned against the message it
        /// reads. These two live in different languages and cannot share a
        /// constant, so the coupling is asserted instead.
        #[test]
        fn the_page_gates_its_force_retry_on_that_same_string() {
            assert!(
                DISKS_HTML.contains(r#"reason(r).includes("force=1")"#),
                "the page must not offer force for a refusal that forbids it",
            );
        }
    }

    mod security_console {
        use super::*;

        #[test]
        fn the_page_calls_the_rule_routes_the_router_registers() {
            for route in [
                r#""POST", "/security/rules""#,
                r#""PATCH", "/security/rules/""#,
                r#""DELETE", "/security/rules/""#,
            ] {
                assert!(SIEM_HTML.contains(route), "page never calls {route}");
            }
        }

        /// An all-zero series means "this rule refused nothing", which is the
        /// finding — not "no data". A page that rendered the two the same way
        /// would hide exactly the rule worth removing.
        #[test]
        fn a_rule_that_never_fires_is_called_out_rather_than_drawn_flat() {
            assert!(SIEM_HTML.contains(r#">no hits<"#));
            assert!(SIEM_HTML.contains(r#">no data yet<"#));
        }

        /// Making a block permanent is the one action here with no natural end,
        /// so it must not be reachable without saying so.
        #[test]
        fn keeping_a_rule_forever_is_confirmed() {
            assert!(SIEM_HTML.contains("Keep rule"));
            assert!(
                SIEM_HTML.contains("secs === null && !confirm("),
                "a forever rule must be confirmed and a timed one must not be",
            );
        }

        /// `expires_in_secs` is required on the PATCH: absent must not be read
        /// as "forever", or a client that forgets the field silently makes a
        /// block permanent.
        #[test]
        fn an_absent_expiry_is_refused_rather_than_read_as_forever() {
            let absent: RuleExpiryBody = serde_json::from_str("{}").unwrap();
            assert!(absent.expires_in_secs.is_none(), "absent");

            let forever: RuleExpiryBody =
                serde_json::from_str(r#"{"expires_in_secs":null}"#).unwrap();
            assert_eq!(forever.expires_in_secs, Some(None), "explicitly forever");

            let timed: RuleExpiryBody =
                serde_json::from_str(r#"{"expires_in_secs":3600}"#).unwrap();
            assert_eq!(timed.expires_in_secs, Some(Some(3600)));
        }
    }

    /// The four pages are separate `include_str!`d files with no build step to
    /// share anything through, so what makes them one product is only ever
    /// convention. These pin the parts of that convention a user would notice.
    mod page_consistency {
        use super::*;

        fn pages() -> [(&'static str, &'static str); 4] {
            [
                ("dashboard", DASHBOARD_HTML),
                ("directory", DIRECTORY_HTML),
                ("siem", SIEM_HTML),
                ("disks", DISKS_HTML),
            ]
        }

        /// One key, or the theme resets every time an operator follows a link
        /// between the pages.
        #[test]
        fn every_page_persists_the_theme_under_the_same_key() {
            for (name, html) in pages() {
                assert!(
                    html.contains(r#"localStorage.getItem("app-lb-theme")"#),
                    "{name} does not restore the saved theme",
                );
                assert!(
                    html.contains(r#"localStorage.setItem("app-lb-theme", next)"#),
                    "{name} does not save the theme it just applied",
                );
            }
        }

        /// The restore has to run in `<head>`, before the body exists. Applied
        /// alongside the rest of the page script it would paint one frame in the
        /// wrong theme on every load.
        #[test]
        fn the_theme_is_restored_before_the_body_renders() {
            for (name, html) in pages() {
                let head = html
                    .split_once("</head>")
                    .unwrap_or_else(|| panic!("{name} has no </head>"))
                    .0;
                assert!(
                    head.contains(r#"localStorage.getItem("app-lb-theme")"#),
                    "{name} restores the theme after <head>, which flashes",
                );
            }
        }

        /// Storage throws rather than returning null in a private window and
        /// under some `file://` policies. A theme preference must not be able to
        /// take a page's whole script block down with it.
        #[test]
        fn theme_storage_failures_cannot_break_the_page() {
            for (name, html) in pages() {
                let uses = html.matches("localStorage").count();
                let guards = html.matches("try {").count();
                assert!(
                    guards >= 2 && uses == 2,
                    "{name}: every localStorage access must sit inside a try/catch \
                     ({uses} accesses, {guards} guards)",
                );
            }
        }
    }

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

    // -- the gate ----------------------------------------------------------

    mod gate {
        use super::super::*;
        use super::b64_basic;
        use crate::tokens::{AdminScope, NewToken, TokenStore};

        const NOW: u64 = 1_000;

        fn store() -> TokenStore {
            // Never persisted, so the path is never touched.
            TokenStore::new("/nonexistent/tokens.json")
        }

        fn mint(s: &TokenStore, admin: AdminScope, deployments: &[&str]) -> String {
            s.mint(
                NewToken {
                    name: "test".into(),
                    admin,
                    deployments: deployments.iter().map(|d| d.to_string()).collect(),
                    expires_in_secs: None,
                },
                NOW,
            )
            .unwrap()
            .1
        }

        fn basic() -> DashboardAuth {
            DashboardAuth::new("admin", "hunter2")
        }

        /// `decide_access` with the common shape: a header, a route, no query.
        fn on(
            auth: Option<&DashboardAuth>,
            tokens: &TokenStore,
            header: Option<&str>,
            matched: &str,
            path: &str,
            want: AdminScope,
        ) -> Verdict {
            decide_access(
                auth,
                tokens,
                &Presented {
                    header,
                    matched: Some(matched),
                    path,
                    query: None,
                },
                want,
                NOW,
            )
        }

        fn allowed(v: Verdict) -> Caller {
            match v {
                Verdict::Allow(c) => c,
                other => panic!("expected Allow, got {other:?}"),
            }
        }

        fn forbidden_because(v: Verdict) -> String {
            match v {
                Verdict::Forbidden(d) => d,
                other => panic!("expected Forbidden, got {other:?}"),
            }
        }

        #[test]
        fn no_configured_credential_means_no_gate() {
            let t = store();
            let v = on(None, &t, None, "/deployments", "/deployments", AdminScope::Admin);
            assert!(matches!(allowed(v), Caller::Ungated));
        }

        #[test]
        fn basic_auth_still_works_and_is_unscoped() {
            let t = store();
            let auth = basic();
            let hdr = format!("Basic {}", b64_basic("admin", "hunter2"));

            let caller = allowed(on(
                Some(&auth),
                &t,
                Some(&hdr),
                "/deployments/:id/exec",
                "/deployments/anything/exec",
                AdminScope::Admin,
            ));
            assert!(matches!(caller, Caller::Operator));
            assert!(caller.may_touch("anything"));
            assert!(caller.covers_fleet());
            assert!(
                caller.visible().is_none(),
                "the operator credential must not be narrowed"
            );
        }

        #[test]
        fn a_bearer_token_authenticates_where_basic_would() {
            let t = store();
            let secret = mint(&t, AdminScope::Admin, &["*"]);
            let hdr = format!("Bearer {secret}");
            let caller = allowed(on(
                Some(&basic()),
                &t,
                Some(&hdr),
                "/deployments",
                "/deployments",
                AdminScope::Admin,
            ));
            assert!(matches!(caller, Caller::Token(_)));
        }

        #[test]
        fn a_revoked_token_is_unauthorized_not_forbidden() {
            let t = store();
            let secret = mint(&t, AdminScope::Admin, &["*"]);
            let id = t.list()[0].id.clone();
            t.revoke(&id);

            let hdr = format!("Bearer {secret}");
            assert!(matches!(
                on(Some(&basic()), &t, Some(&hdr), "/deployments", "/deployments", AdminScope::Admin),
                Verdict::Unauthorized
            ));
        }

        #[test]
        fn garbage_credentials_are_unauthorized() {
            let t = store();
            let auth = basic();
            for header in [
                None,
                Some("Bearer applb_000000000000_nope"),
                Some("Bearer "),
                Some("Basic bm9wZTpub3Bl"),
                Some("applb_000000000000_nope"),
                Some("Token applb_000000000000_nope"),
            ] {
                assert!(
                    matches!(
                        on(Some(&auth), &t, header, "/deployments", "/deployments", AdminScope::Admin),
                        Verdict::Unauthorized
                    ),
                    "{header:?} should not authenticate",
                );
            }
        }

        #[test]
        fn a_view_token_reads_metrics_but_cannot_reach_the_crud_tier() {
            let t = store();
            let secret = mint(&t, AdminScope::View, &["*"]);
            let hdr = format!("Bearer {secret}");
            let auth = basic();

            allowed(on(Some(&auth), &t, Some(&hdr), "/metrics", "/metrics", AdminScope::View));

            let why = forbidden_because(on(
                Some(&auth),
                &t,
                Some(&hdr),
                "/deployments",
                "/deployments",
                AdminScope::Admin,
            ));
            assert!(why.contains("admin scope"), "{why}");
        }

        #[test]
        fn a_data_plane_only_token_cannot_read_the_admin_api_at_all() {
            let t = store();
            // `admin: none` — the token an application carries to get past its own
            // deployment's gate, and nothing more.
            let secret = mint(&t, AdminScope::None, &["sb-1"]);
            let hdr = format!("Bearer {secret}");
            let auth = basic();

            assert!(matches!(
                on(Some(&auth), &t, Some(&hdr), "/metrics", "/metrics", AdminScope::View),
                Verdict::Forbidden(_)
            ));
            assert!(matches!(
                on(Some(&auth), &t, Some(&hdr), "/deployments/:id/exec", "/deployments/sb-1/exec", AdminScope::Admin),
                Verdict::Forbidden(_)
            ));
        }

        #[test]
        fn a_scoped_token_reaches_its_own_deployment_and_no_other() {
            let t = store();
            let secret = mint(&t, AdminScope::Admin, &["sb-1"]);
            let hdr = format!("Bearer {secret}");
            let auth = basic();

            for route in [
                ("/deployments/:id", "/deployments/sb-1"),
                ("/deployments/:id/exec", "/deployments/sb-1/exec"),
                ("/deployments/:id/shell", "/deployments/sb-1/shell"),
                ("/deployments/:id/scaling", "/deployments/sb-1/scaling"),
                ("/deployments/:id/jobs", "/deployments/sb-1/jobs"),
                (
                    "/deployments/:id/vms/:sandbox_id",
                    "/deployments/sb-1/vms/applb-x",
                ),
            ] {
                allowed(on(Some(&auth), &t, Some(&hdr), route.0, route.1, AdminScope::Admin));
            }

            let why = forbidden_because(on(
                Some(&auth),
                &t,
                Some(&hdr),
                "/deployments/:id/exec",
                "/deployments/sb-2/exec",
                AdminScope::Admin,
            ));
            assert!(why.contains("sb-2"), "the message should name the deployment: {why}");
        }

        #[test]
        fn a_scoped_token_is_refused_the_fleet_wide_routes() {
            let t = store();
            let secret = mint(&t, AdminScope::Admin, &["sb-1"]);
            let hdr = format!("Bearer {secret}");
            let auth = basic();

            // Creating deployments, reading the secret store and listing every job
            // are not about the one deployment this token was given.
            for route in [
                "/deployments",
                "/secrets",
                "/secrets/:id",
                "/jobs",
                "/jobs/:job_id",
                "/certs",
                "/tokens",
                "/tokens/:id",
            ] {
                assert!(
                    matches!(
                        on(Some(&auth), &t, Some(&hdr), route, route, AdminScope::Admin),
                        Verdict::Forbidden(_)
                    ),
                    "{route} should be refused a deployment-scoped token",
                );
            }
        }

        /// Minting is how you escalate, so it must not be reachable by anything
        /// less than a fleet-wide admin token.
        #[test]
        fn a_scoped_token_cannot_mint_itself_a_wider_one() {
            let t = store();
            let secret = mint(&t, AdminScope::Admin, &["sb-1"]);
            let hdr = format!("Bearer {secret}");
            assert!(matches!(
                on(Some(&basic()), &t, Some(&hdr), "/tokens", "/tokens", AdminScope::Admin),
                Verdict::Forbidden(_)
            ));
        }

        #[test]
        fn metrics_narrows_itself_rather_than_refusing_a_scoped_token() {
            let t = store();
            let secret = mint(&t, AdminScope::View, &["sb-1", "sb-2"]);
            let hdr = format!("Bearer {secret}");

            let caller = allowed(on(
                Some(&basic()),
                &t,
                Some(&hdr),
                "/metrics",
                "/metrics",
                AdminScope::View,
            ));
            assert_eq!(
                caller.visible().expect("a scoped token narrows the answer"),
                ["sb-1".to_string(), "sb-2".to_string()],
            );
        }

        #[test]
        fn a_fleet_scoped_token_narrows_nothing() {
            let t = store();
            let secret = mint(&t, AdminScope::View, &["*"]);
            let hdr = format!("Bearer {secret}");
            let caller = allowed(on(
                Some(&basic()),
                &t,
                Some(&hdr),
                "/metrics",
                "/metrics",
                AdminScope::View,
            ));
            assert!(caller.visible().is_none());
        }

        #[test]
        fn a_query_token_works_on_the_shell_route_and_nowhere_else() {
            let t = store();
            let secret = mint(&t, AdminScope::Admin, &["sb-1"]);
            let auth = basic();
            let query = format!("cols=80&app_token={secret}&rows=24");

            // The one route a browser cannot send a header to.
            let v = decide_access(
                Some(&auth),
                &t,
                &Presented {
                    header: None,
                    matched: Some("/deployments/:id/shell"),
                    path: "/deployments/sb-1/shell",
                    query: Some(&query),
                },
                AdminScope::Admin,
                NOW,
            );
            assert!(matches!(allowed(v), Caller::Token(_)));

            // Everywhere else it is not a credential at all, because everywhere
            // else can use a header — and a token in a URL lands in access logs.
            for (matched, path) in [
                ("/deployments/:id/exec", "/deployments/sb-1/exec"),
                ("/deployments/:id", "/deployments/sb-1"),
                ("/metrics", "/metrics"),
            ] {
                assert!(
                    matches!(
                        decide_access(
                            Some(&auth),
                            &t,
                            &Presented {
                                header: None,
                                matched: Some(matched),
                                path,
                                query: Some(&query),
                            },
                            AdminScope::Admin,
                            NOW,
                        ),
                        Verdict::Unauthorized
                    ),
                    "{matched} must not accept a credential in the query string",
                );
            }
        }

        #[test]
        fn the_query_token_still_has_to_be_in_scope() {
            let t = store();
            let secret = mint(&t, AdminScope::Admin, &["sb-1"]);
            let query = format!("app_token={secret}");
            assert!(matches!(
                decide_access(
                    Some(&basic()),
                    &t,
                    &Presented {
                        header: None,
                        matched: Some("/deployments/:id/shell"),
                        path: "/deployments/sb-2/shell",
                        query: Some(&query),
                    },
                    AdminScope::Admin,
                    NOW,
                ),
                Verdict::Forbidden(_)
            ));
        }

        #[test]
        fn an_expired_token_stops_working_without_anyone_revoking_it() {
            let t = store();
            let secret = t
                .mint(
                    NewToken {
                        name: "short".into(),
                        admin: AdminScope::Admin,
                        deployments: vec!["*".into()],
                        expires_in_secs: Some(60),
                    },
                    NOW,
                )
                .unwrap()
                .1;
            let hdr = format!("Bearer {secret}");
            let auth = basic();
            let at = |now| {
                decide_access(
                    Some(&auth),
                    &t,
                    &Presented {
                        header: Some(&hdr),
                        matched: Some("/deployments"),
                        path: "/deployments",
                        query: None,
                    },
                    AdminScope::Admin,
                    now,
                )
            };
            assert!(matches!(at(NOW + 59), Verdict::Allow(_)));
            assert!(matches!(at(NOW + 60), Verdict::Unauthorized));
        }

        /// The id is read positionally out of the real path, so this pins the
        /// assumption that every deployment route is `/deployments/:id/…`.
        #[test]
        fn the_deployment_is_read_off_the_matched_route() {
            assert_eq!(
                deployment_of("/deployments/:id/exec", "/deployments/sb-1/exec"),
                Some("sb-1")
            );
            assert_eq!(
                deployment_of("/deployments/:id", "/deployments/sb-1"),
                Some("sb-1")
            );
            assert_eq!(
                deployment_of("/deployments/:id/vms/:sandbox_id", "/deployments/sb-1/vms/x"),
                Some("sb-1")
            );
            // Not a deployment route, however much the path looks like one.
            assert_eq!(deployment_of("/deployments", "/deployments"), None);
            assert_eq!(deployment_of("/jobs/:job_id", "/jobs/deployments"), None);
            assert_eq!(deployment_of("/metrics", "/metrics"), None);
        }

        #[test]
        fn bearer_parsing_is_exact() {
            assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
            assert_eq!(bearer(Some("Bearer  abc ")), Some("abc"));
            assert_eq!(bearer(Some("bearer abc")), None, "the scheme is case-sensitive here");
            assert_eq!(bearer(Some("Bearer")), None);
            assert_eq!(bearer(Some("Bearer ")), None);
            assert_eq!(bearer(Some("Basic abc")), None);
            assert_eq!(bearer(None), None);
        }
    }

    /// Basic credentials as the header value they must produce, for the gate
    /// tests. Deliberately re-derived rather than reusing `DashboardAuth`'s own
    /// encoding, so a change to that encoding fails a test instead of silently
    /// agreeing with itself.
    fn b64_basic(user: &str, password: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
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

/// Wire-contract fixtures for the clients that re-declare these types.
/// A child module rather than part of `mod tests` above, because it needs to see
/// the private response structs and nothing else in this file needs to see it.
#[cfg(test)]
#[path = "wire_golden.rs"]
mod wire_golden;
