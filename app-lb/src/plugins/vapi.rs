//! The vapi plugin: watch and drive [vapi](https://github.com/Heyo-Computer/vapi)
//! inference gateways from app-lb.
//!
//! A vapi gateway serves an OpenAI-compatible API (`/v1/chat/completions`,
//! `/v1/completions`, `/v1/decisions`, `/v1/audio/transcriptions`) plus its own
//! dashboard JSON at `/dashboard/stats`, and — once any API key is configured —
//! wants a bearer token on all of it. This plugin holds that token, as a
//! reference into app-lb's secret store rather than in its own record, and
//! exposes the gateway through app-lb's admin gate. Two things follow that are
//! worth having: the browser on the Plugins page never sees a vapi key, and who
//! may *watch* a gateway versus *spend GPU time on it* is decided by app-lb's
//! view and CRUD tiers instead of by the one key that can do everything.
//!
//! The tier split is the reason the routes are enumerated rather than
//! wildcarded. Reading `/dashboard/stats` or the model list is view-tier;
//! running a completion is not, because it occupies a worker, evicts other
//! callers' KV blocks and costs real time on a GPU that has one of everything.
//! Changing the gateway's admission settings is likewise CRUD-tier.
//!
//! This is a control plane, not a data plane. Serving application traffic to a
//! vapi gateway is what a static (`upstreams`) deployment is for — it streams,
//! it load-balances, it health-checks. The proxy here buffers whole responses
//! and refuses `"stream": true` rather than pretending otherwise.
//!
//! A background poller reads every gateway's `/health` and `/dashboard/stats`,
//! so the page can show which gateways are up, what model each is serving and
//! how loaded its workers are without a request per gateway per render.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Plugin, PluginMeta};
use crate::secrets::{SecretRef, SecretStore};

/// Generation is not a fast request: a few thousand tokens behind a queue of
/// other callers' work is minutes, and the gateway's own first-token and idle
/// timeouts are the ones that should decide when to give up. This only has to
/// be longer than they are.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// A poll asks for numbers the gateway already has in memory. If that takes
/// ten seconds the gateway is not healthy, which is what the page should say.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_POLL_SECS: u64 = 15;

// ---- configuration --------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VapiConfig {
    pub gateways: Vec<GatewayConfig>,
    /// How often to poll each gateway.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
}

fn default_poll_secs() -> u64 {
    DEFAULT_POLL_SECS
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Short name, used in URLs: lowercase letters, digits and `-`.
    pub name: String,
    /// The gateway's HTTP listener (vapi's `gateway.bind`), e.g.
    /// `http://127.0.0.1:8080`.
    pub url: String,
    /// One of the gateway's `[[auth.keys]]` values, as a reference into
    /// app-lb's secret store: `{"secret": "vapi", "key": "api_key"}`. Omit it
    /// when the gateway has no keys configured — vapi then leaves `/v1` open.
    ///
    /// Which key this is matters beyond access: vapi gives each key its own
    /// prefix-cache namespace, so calls made through this plugin share a cache
    /// with each other and with nobody else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
}

fn parse_config(config: &Value) -> Result<VapiConfig, String> {
    let cfg: VapiConfig = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    if cfg.gateways.is_empty() {
        return Err("add at least one gateway: {\"gateways\": [{\"name\": \"local\", \"url\": \"http://127.0.0.1:8080\", …}]}".into());
    }
    if !(5..=3600).contains(&cfg.poll_secs) {
        return Err("poll_secs must be between 5 and 3600".into());
    }
    let mut seen = std::collections::HashSet::new();
    for g in &cfg.gateways {
        let name_ok = !g.name.is_empty()
            && g.name.len() <= 32
            && g.name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !name_ok {
            return Err(format!(
                "gateway name {:?}: use 1–32 lowercase letters, digits or '-'",
                g.name
            ));
        }
        if !seen.insert(g.name.as_str()) {
            return Err(format!("gateway name {:?} is used twice", g.name));
        }
        match url::Url::parse(&g.url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host().is_some() => {}
            _ => {
                return Err(format!(
                    "gateway {}: url {:?} must be an http(s) URL",
                    g.name, g.url
                ));
            }
        }
        if let Some(k) = &g.api_key {
            k.validate()
                .map_err(|e| format!("gateway {}: api_key: {e}", g.name))?;
        }
    }
    Ok(cfg)
}

// ---- what a gateway reports ----------------------------------------------

/// The part of vapi's `/dashboard/stats` this plugin understands.
///
/// Deliberately *not* `deny_unknown_fields`, and every field defaulted: vapi
/// and app-lb are separate repositories on separate release cadences, and a
/// gateway that starts reporting a new counter must not make this plugin
/// report it as down. Whatever is not named here still reaches the page,
/// which reads `…/stats` through the proxy and gets vapi's body verbatim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Stats {
    #[serde(default)]
    model: String,
    #[serde(default)]
    uptime: String,
    #[serde(default)]
    queued: u64,
    #[serde(default)]
    started: u64,
    #[serde(default)]
    completed: u64,
    #[serde(default)]
    failed: u64,
    #[serde(default)]
    refused: u64,
    #[serde(default)]
    cache_hit_rate: String,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    workers: Vec<WorkerRow>,
    #[serde(default)]
    settings: GatewaySettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorkerRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    running: u64,
    #[serde(default)]
    waiting: u64,
    #[serde(default)]
    max_concurrent: u64,
    #[serde(default)]
    load: String,
    #[serde(default)]
    kv: String,
    #[serde(default)]
    prefix_hit: String,
    #[serde(default)]
    uptime: String,
}

/// The gateway settings vapi will accept a change to. Everything a *worker*
/// acts on (cache size, batch limits, the model) is read at its start, so vapi
/// shows it read-only and this plugin cannot move it either.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GatewaySettings {
    #[serde(default)]
    max_queued_requests: u64,
    #[serde(default)]
    response_cache: bool,
    #[serde(default)]
    response_cache_available: bool,
    #[serde(default)]
    first_token_timeout_secs: u64,
    #[serde(default)]
    stream_idle_timeout_secs: u64,
}

/// What the poller last learned about a gateway.
#[derive(Debug, Clone, Default, Serialize)]
struct GatewayStatus {
    up: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Unix seconds of the last poll.
    polled_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<Stats>,
}

/// One gateway as `GET …/gateways` reports it. No credential, by construction.
#[derive(Debug, Serialize)]
struct GatewayView {
    name: String,
    url: String,
    /// Whether a key is configured for it, but never which one or its value.
    authenticated: bool,
    #[serde(flatten)]
    status: GatewayStatus,
}

// ---- the plugin -----------------------------------------------------------

pub struct VapiPlugin {
    /// For the poller task, which outlives the `&self` that `apply` gets.
    me: std::sync::Weak<VapiPlugin>,
    secrets: Arc<SecretStore>,
    http: reqwest::Client,
    config: RwLock<Option<Arc<VapiConfig>>>,
    status: RwLock<HashMap<String, GatewayStatus>>,
    poller: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl VapiPlugin {
    pub fn new(secrets: Arc<SecretStore>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            secrets,
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                // vapi answers `/dashboard?key=…` with a 303 that sets a
                // session cookie. Nothing this plugin calls should redirect,
                // and following one would turn a settings POST's "applied"
                // into the dashboard's HTML — so a redirect stays visible as
                // itself.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client builds"),
            config: RwLock::new(None),
            status: RwLock::new(HashMap::new()),
            poller: Mutex::new(None),
        })
    }

    fn gateway(&self, name: &str) -> Option<GatewayConfig> {
        let cfg = self.config.read().unwrap().clone()?;
        cfg.gateways.iter().find(|g| g.name == name).cloned()
    }

    /// A request to `gateway`'s `<path>` with its key attached.
    ///
    /// The key is resolved per request, so rotating the secret takes effect
    /// without re-applying the plugin.
    fn request(
        &self,
        gateway: &GatewayConfig,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, String> {
        let url = format!("{}{path}", gateway.url.trim_end_matches('/'));
        let method =
            reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(|e| e.to_string())?;
        let mut req = self.http.request(method, url);
        if let Some(r) = &gateway.api_key {
            let key = self.secrets.resolve(r).map_err(|e| {
                format!("gateway {}: cannot resolve its api_key: {e}", gateway.name)
            })?;
            req = req.bearer_auth(key);
        }
        Ok(req)
    }

    async fn poll_gateway(&self, gateway: &GatewayConfig) -> GatewayStatus {
        let now = crate::deployment::now_secs();
        // `/health` is open even when keys are configured, so it answers "is
        // the process up" without involving the credential; `/dashboard/stats`
        // answers everything else and proves the credential at the same time.
        let health = async {
            let resp = self
                .request(gateway, Method::GET, "/health")?
                .timeout(POLL_TIMEOUT)
                .send()
                .await
                .map_err(|e| format!("/health: {e}"))?;
            if resp.status().is_success() {
                Ok(())
            } else {
                Err(format!("/health: HTTP {}", resp.status()))
            }
        };
        let stats = async {
            let resp = self
                .request(gateway, Method::GET, "/dashboard/stats")?
                .timeout(POLL_TIMEOUT)
                .send()
                .await
                .map_err(|e| format!("/dashboard/stats: {e}"))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err("vapi rejected the configured api_key (401)".to_string());
            }
            if !resp.status().is_success() {
                return Err(format!("/dashboard/stats: HTTP {}", resp.status()));
            }
            resp.json::<Stats>()
                .await
                .map_err(|e| format!("/dashboard/stats: {e}"))
        };
        let (health, stats) = tokio::join!(health, stats);
        match (health, stats) {
            // Up and readable: the ordinary case.
            (Ok(()), Ok(s)) => GatewayStatus {
                up: true,
                error: None,
                polled_at: now,
                stats: Some(s),
            },
            // Alive but not readable — almost always a wrong or missing key.
            // Reporting that as "down" would send an operator to look at the
            // gateway when the thing to fix is here.
            (Ok(()), Err(e)) => GatewayStatus {
                up: true,
                error: Some(e),
                polled_at: now,
                stats: None,
            },
            (Err(e), _) => GatewayStatus {
                up: false,
                error: Some(e),
                polled_at: now,
                stats: None,
            },
        }
    }

    async fn poll_all(&self, cfg: &VapiConfig) {
        // Concurrently, so one slow gateway does not delay the others' status.
        let results = futures::future::join_all(
            cfg.gateways
                .iter()
                .map(|g| async move { (g.name.clone(), self.poll_gateway(g).await) }),
        )
        .await;
        let mut status = self.status.write().unwrap();
        status.clear();
        status.extend(results);
    }

    fn stop_poller(&self) {
        if let Some(h) = self.poller.lock().unwrap().take() {
            h.abort();
        }
    }
}

#[async_trait]
impl Plugin for VapiPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "vapi",
            name: "vapi inference",
            description: "Watch and drive vapi LLM inference gateways: models, workers, KV cache and \
                 queue depth, admission settings, and completions run through app-lb's gate so \
                 the browser never holds a vapi API key.",
            config_schema: json!({
                "type": "object",
                "required": ["gateways"],
                "properties": {
                    "poll_secs": {"type": "integer", "minimum": 5, "maximum": 3600, "default": DEFAULT_POLL_SECS},
                    "gateways": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["name", "url"],
                            "properties": {
                                "name": {"type": "string", "pattern": "^[a-z0-9-]{1,32}$"},
                                "url": {"type": "string", "description": "the gateway's listener, e.g. http://127.0.0.1:8080"},
                                "api_key": {
                                    "type": "object",
                                    "description": "A secret reference: {\"secret\": \"vapi\", \"key\": \"api_key\"}. Omit when the gateway has no keys configured.",
                                    "required": ["secret"],
                                    "properties": {"secret": {"type": "string"}, "key": {"type": "string"}}
                                }
                            }
                        }
                    }
                }
            }),
        }
    }

    fn validate(&self, config: &Value) -> Result<(), String> {
        parse_config(config).map(|_| ())
    }

    async fn apply(&self, config: Option<Value>) -> Result<(), String> {
        self.stop_poller();
        let Some(config) = config else {
            *self.config.write().unwrap() = None;
            self.status.write().unwrap().clear();
            return Ok(());
        };
        let cfg = Arc::new(parse_config(&config)?);
        *self.config.write().unwrap() = Some(cfg.clone());
        // First poll inline, so enabling reports an unreachable gateway or a
        // wrong key on the spot instead of as a quiet "down" on the next page
        // refresh.
        self.poll_all(&cfg).await;
        let me = self.me.clone();
        let every = Duration::from_secs(cfg.poll_secs);
        let poll_cfg = cfg.clone();
        *self.poller.lock().unwrap() = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.tick().await; // the inline poll above was this one
            loop {
                tick.tick().await;
                let Some(me) = me.upgrade() else { return };
                me.poll_all(&poll_cfg).await;
            }
        }));
        // The poller keeps running either way — a gateway that is down now may
        // come up, and a model takes minutes to load — but an operator who just
        // switched this on should hear about the ones it cannot use.
        let bad: Vec<String> = self
            .status
            .read()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.error.is_some())
            .map(|(n, s)| format!("{n}: {}", s.error.as_deref().unwrap_or("down")))
            .collect();
        if bad.is_empty() {
            Ok(())
        } else {
            Err(bad.join("; "))
        }
    }

    async fn status(&self) -> Value {
        let Some(cfg) = self.config.read().unwrap().clone() else {
            return json!({});
        };
        let status = self.status.read().unwrap();
        let gateways: Vec<Value> = cfg
            .gateways
            .iter()
            .map(|g| {
                let s = status.get(&g.name).cloned().unwrap_or_default();
                let stats = s.stats.as_ref();
                json!({
                    "name": g.name,
                    "up": s.up,
                    "error": s.error,
                    "model": stats.map(|s| s.model.clone()),
                    "workers": stats.map(|s| s.workers.len()),
                    "queued": stats.map(|s| s.queued),
                    "completed": stats.map(|s| s.completed),
                })
            })
            .collect();
        json!({ "gateways": gateways })
    }

    fn view_routes(self: Arc<Self>) -> Router {
        Router::new()
            .route("/gateways", get(list_gateways))
            .route("/gateways/:gw/stats", get(stats))
            .route("/gateways/:gw/models", get(models))
            .with_state(self)
    }

    fn crud_routes(self: Arc<Self>) -> Router {
        Router::new()
            // Inference is CRUD-tier: it occupies a worker, evicts other
            // callers' KV blocks and costs GPU time. Watching a gateway should
            // not imply being able to spend it.
            .route("/gateways/:gw/chat/completions", post(chat_completions))
            .route("/gateways/:gw/completions", post(completions))
            .route("/gateways/:gw/decisions", post(decisions))
            .route("/gateways/:gw/settings", put(settings))
            .with_state(self)
    }
}

// ---- handlers -------------------------------------------------------------

fn fail(code: StatusCode, error: impl Into<String>) -> Response {
    (code, axum::Json(json!({ "error": error.into() }))).into_response()
}

/// `GET …/gateways` — the configured gateways and what the poller last saw.
async fn list_gateways(State(p): State<Arc<VapiPlugin>>) -> Response {
    let Some(cfg) = p.config.read().unwrap().clone() else {
        return axum::Json(Vec::<Value>::new()).into_response();
    };
    let status = p.status.read().unwrap();
    let gateways: Vec<GatewayView> = cfg
        .gateways
        .iter()
        .map(|g| GatewayView {
            name: g.name.clone(),
            url: g.url.clone(),
            authenticated: g.api_key.is_some(),
            status: status.get(&g.name).cloned().unwrap_or_default(),
        })
        .collect();
    axum::Json(gateways).into_response()
}

/// Forward to `path` on the named gateway, returning vapi's status and body.
async fn forward(
    p: &VapiPlugin,
    name: &str,
    method: Method,
    path: &str,
    body: Option<Bytes>,
) -> Response {
    let Some(gateway) = p.gateway(name) else {
        return fail(
            StatusCode::NOT_FOUND,
            format!("no vapi gateway named {name:?}"),
        );
    };
    let mut req = match p.request(&gateway, method, path) {
        Ok(r) => r,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, e),
    };
    if let Some(body) = body {
        req = req
            .header(header::CONTENT_TYPE.as_str(), "application/json")
            .body(body);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return fail(
                StatusCode::BAD_GATEWAY,
                format!("vapi gateway {name} is unreachable: {e}"),
            );
        }
    };
    let status = resp.status().as_u16();
    // A 401 from vapi means *app-lb's* stored key is wrong. Passing it through
    // would read as the caller's own session failing.
    if status == 401 {
        return fail(
            StatusCode::BAD_GATEWAY,
            format!("vapi gateway {name} rejected the api_key configured for it"),
        );
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return fail(
                StatusCode::BAD_GATEWAY,
                format!("reading vapi's response: {e}"),
            );
        }
    };
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        [(header::CONTENT_TYPE, content_type)],
        bytes,
    )
        .into_response()
}

async fn stats(State(p): State<Arc<VapiPlugin>>, Path(gw): Path<String>) -> Response {
    forward(&p, &gw, Method::GET, "/dashboard/stats", None).await
}

async fn models(State(p): State<Arc<VapiPlugin>>, Path(gw): Path<String>) -> Response {
    forward(&p, &gw, Method::GET, "/v1/models", None).await
}

/// Check an inference body before it is forwarded.
///
/// `stream: true` is refused rather than quietly buffered: the caller would
/// get one response containing every SSE frame at the end, which is the exact
/// opposite of what asking for a stream is for, and a silent lie about
/// latency. Anything that needs real streaming wants the data plane.
fn refuse_inference(body: &Bytes) -> Option<Response> {
    let value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return Some(fail(
                StatusCode::BAD_REQUEST,
                format!("expected a JSON body: {e}"),
            ));
        }
    };
    if value.get("stream").and_then(Value::as_bool) == Some(true) {
        return Some(fail(
            StatusCode::BAD_REQUEST,
            "this proxy buffers whole responses and cannot stream. Drop \"stream\": true, or \
             register the gateway as a static deployment and send streaming traffic through the \
             load balancer instead.",
        ));
    }
    None
}

async fn inference(p: &VapiPlugin, gw: &str, path: &'static str, body: Bytes) -> Response {
    if let Some(refusal) = refuse_inference(&body) {
        return refusal;
    }
    forward(p, gw, Method::POST, path, Some(body)).await
}

async fn chat_completions(
    State(p): State<Arc<VapiPlugin>>,
    Path(gw): Path<String>,
    body: Bytes,
) -> Response {
    inference(&p, &gw, "/v1/chat/completions", body).await
}

async fn completions(
    State(p): State<Arc<VapiPlugin>>,
    Path(gw): Path<String>,
    body: Bytes,
) -> Response {
    inference(&p, &gw, "/v1/completions", body).await
}

async fn decisions(
    State(p): State<Arc<VapiPlugin>>,
    Path(gw): Path<String>,
    body: Bytes,
) -> Response {
    inference(&p, &gw, "/v1/decisions", body).await
}

/// The fields `PUT …/settings` will change. All optional: an operator who
/// wants the response cache off should not have to restate three timeouts to
/// say so.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsBody {
    max_queued_requests: Option<u64>,
    first_token_timeout_secs: Option<u64>,
    stream_idle_timeout_secs: Option<u64>,
    response_cache: Option<bool>,
}

/// `PUT …/settings` — change a gateway's admission settings.
///
/// vapi's own form posts every field at once and answers with a redirect back
/// to the dashboard, so a partial change has to be assembled here: read the
/// current settings, lay the requested ones over them, and send the whole
/// form. The applied settings come back as JSON, which is more useful to the
/// page than a 303 to a page it is not on.
async fn settings(
    State(p): State<Arc<VapiPlugin>>,
    Path(gw): Path<String>,
    body: Bytes,
) -> Response {
    let want: SettingsBody = if body.is_empty() {
        SettingsBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return fail(
                    StatusCode::BAD_REQUEST,
                    format!("expected a JSON body: {e}"),
                );
            }
        }
    };
    let Some(gateway) = p.gateway(&gw) else {
        return fail(
            StatusCode::NOT_FOUND,
            format!("no vapi gateway named {gw:?}"),
        );
    };

    let current = match p
        .request(&gateway, Method::GET, "/dashboard/stats")
        .map(|r| r.timeout(POLL_TIMEOUT))
    {
        Ok(req) => match req.send().await {
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
                return fail(
                    StatusCode::BAD_GATEWAY,
                    format!("vapi gateway {gw} rejected the api_key configured for it"),
                );
            }
            Ok(r) if r.status().is_success() => match r.json::<Stats>().await {
                Ok(s) => s.settings,
                Err(e) => {
                    return fail(
                        StatusCode::BAD_GATEWAY,
                        format!("reading gateway {gw}'s current settings: {e}"),
                    );
                }
            },
            Ok(r) => {
                return fail(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "reading gateway {gw}'s current settings: HTTP {}",
                        r.status()
                    ),
                );
            }
            Err(e) => {
                return fail(
                    StatusCode::BAD_GATEWAY,
                    format!("vapi gateway {gw} is unreachable: {e}"),
                );
            }
        },
        Err(e) => return fail(StatusCode::BAD_GATEWAY, e),
    };

    let applied = GatewaySettings {
        max_queued_requests: want
            .max_queued_requests
            .unwrap_or(current.max_queued_requests),
        first_token_timeout_secs: want
            .first_token_timeout_secs
            .unwrap_or(current.first_token_timeout_secs),
        stream_idle_timeout_secs: want
            .stream_idle_timeout_secs
            .unwrap_or(current.stream_idle_timeout_secs),
        response_cache: want.response_cache.unwrap_or(current.response_cache),
        response_cache_available: current.response_cache_available,
    };
    // The form is a browser form: a checkbox is absent when unticked, and vapi
    // reads the absence as off.
    let mut form = format!(
        "max_queued_requests={}&first_token_timeout_secs={}&stream_idle_timeout_secs={}",
        applied.max_queued_requests,
        applied.first_token_timeout_secs,
        applied.stream_idle_timeout_secs
    );
    if applied.response_cache {
        form.push_str("&response_cache=on");
    }
    let req = match p.request(&gateway, Method::POST, "/dashboard/settings") {
        Ok(r) => r
            .header(
                header::CONTENT_TYPE.as_str(),
                "application/x-www-form-urlencoded",
            )
            .body(form),
        Err(e) => return fail(StatusCode::BAD_GATEWAY, e),
    };
    match req.send().await {
        // vapi redirects back to its dashboard on success; the client does not
        // follow it, so a 3xx here *is* the acknowledgement.
        Ok(r) if r.status().is_success() || r.status().is_redirection() => {
            axum::Json(applied).into_response()
        }
        Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => fail(
            StatusCode::BAD_GATEWAY,
            format!("vapi gateway {gw} rejected the api_key configured for it"),
        ),
        Ok(r) => fail(
            StatusCode::BAD_GATEWAY,
            format!("gateway {gw} refused the settings: HTTP {}", r.status()),
        ),
        Err(e) => fail(
            StatusCode::BAD_GATEWAY,
            format!("vapi gateway {gw} is unreachable: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_service::Service;

    fn secrets_with(key: &str) -> Arc<SecretStore> {
        let dir =
            std::env::temp_dir().join(format!("app-lb-vapi-{}-{}", std::process::id(), key.len()));
        let store = Arc::new(SecretStore::new(dir.join("secrets.json"), None));
        store.put(crate::secrets::SecretSpec {
            id: "vapi".into(),
            namespace: crate::config::DEFAULT_NAMESPACE.into(),
            description: None,
            data: [("api_key".to_string(), key.to_string())].into(),
            updated_at: 0,
        });
        store
    }

    #[test]
    fn a_config_is_checked_before_it_is_stored() {
        let ok = json!({"gateways": [{"name": "local", "url": "http://127.0.0.1:8080",
            "api_key": {"secret": "vapi", "key": "api_key"}}]});
        assert!(parse_config(&ok).is_ok());
        assert!(
            parse_config(&json!({"gateways": [{"name": "open", "url": "http://127.0.0.1:8080"}]}))
                .is_ok(),
            "a gateway with no keys configured needs no api_key"
        );
        for bad in [
            json!({"gateways": []}),
            json!({"gateways": [{"name": "Local!", "url": "http://x"}]}),
            json!({"gateways": [{"name": "a", "url": "ftp://x"}]}),
            json!({"gateways": [{"name": "a", "url": "http://x"}, {"name": "a", "url": "http://y"}]}),
            json!({"gateways": [{"name": "a", "url": "http://x", "api_key": {"secret": "!"}}]}),
            json!({"gateways": [{"name": "a", "url": "http://x", "apikey": {}}]}),
            json!({"gateways": [{"name": "a", "url": "http://x"}], "poll_secs": 1}),
        ] {
            assert!(parse_config(&bad).is_err(), "accepted {bad}");
        }
    }

    /// A vapi-shaped gateway: `/health` open, everything else behind the
    /// bearer token, and a body that echoes what it was asked so a test sees
    /// exactly what the plugin forwarded.
    async fn fake_vapi(key: &str) -> String {
        use axum::extract::Request;
        let expected = format!("Bearer {key}");
        let app = Router::new().fallback(move |req: Request| {
            let expected = expected.clone();
            async move {
                let path = req.uri().path().to_string();
                if path == "/health" {
                    return "ok".into_response();
                }
                let auth = req
                    .headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if auth != expected {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                if path == "/dashboard/stats" {
                    // Shaped like vapi's own Snapshot, including fields this
                    // plugin does not model.
                    return axum::Json(json!({
                        "model": "Qwen/Qwen3-0.6B", "uptime": "0:04:12", "queued": 0,
                        "started": 9, "completed": 9, "failed": 0, "refused": 0,
                        "cache_hits": 3, "cache_hit_rate": "33%",
                        "prompt_tokens": 120, "completion_tokens": 400,
                        "workers": [{"id": "w-3af9", "model": "Qwen/Qwen3-0.6B", "running": 1,
                            "waiting": 0, "max_concurrent": 32, "load": "3%", "kv": "12%",
                            "kv_pct": 12, "prefix_hit": "44%", "uptime": "0:04:00",
                            "partitions": "all"}],
                        "recent": [], "fixed": [], "features": [],
                        "settings": {"max_queued_requests": 64, "response_cache": true,
                            "response_cache_available": true, "first_token_timeout_secs": 30,
                            "stream_idle_timeout_secs": 60}
                    }))
                    .into_response();
                }
                let method = req.method().to_string();
                let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                if path == "/dashboard/settings" {
                    // vapi answers the form with a redirect back to the page.
                    return (
                        StatusCode::SEE_OTHER,
                        [
                            (header::LOCATION, "/dashboard?saved=1"),
                            (header::CONTENT_TYPE, "text/plain"),
                        ],
                        String::from_utf8_lossy(&body).to_string(),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    axum::Json(json!({"method": method, "path": path,
                        "body": String::from_utf8_lossy(&body)})),
                )
                    .into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn call(
        router: &mut Router,
        method: Method,
        uri: &str,
        body: &str,
    ) -> (StatusCode, Value) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let resp = router.call(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn configured(url: &str) -> Value {
        json!({"gateways": [{"name": "local", "url": url,
            "api_key": {"secret": "vapi", "key": "api_key"}}]})
    }

    /// A real gateway's `/dashboard/stats`, recorded verbatim from
    /// vapi 0.1 serving Qwen3-0.6B.
    ///
    /// The point is the fields this plugin does *not* model — `recent`,
    /// `fixed`, `features`, `cache_hits` — which must not make a gateway
    /// unreadable. vapi and app-lb ship separately, and the day vapi adds a
    /// counter should not be the day this plugin reports every gateway down.
    #[test]
    fn a_real_gateways_snapshot_parses_including_the_fields_we_ignore() {
        let recorded = r#"{"model":"Qwen/Qwen3-0.6B","uptime":"0:00:00","queued":0,"started":0,
            "completed":0,"failed":0,"refused":0,"cache_hits":0,"cache_hit_rate":"—",
            "prompt_tokens":0,"completion_tokens":0,"workers":[],"recent":[],
            "settings":{"max_queued_requests":256,"response_cache":false,
            "response_cache_available":false,"first_token_timeout_secs":120,
            "stream_idle_timeout_secs":60},
            "fixed":[["model.path","/models/qwen3-0.6b"],["model.max_context","32768"]],
            "features":[["tool calls","JSON objects, in this model's markers"]]}"#;
        let stats: Stats = serde_json::from_str(recorded).expect("a real snapshot parses");
        assert_eq!(stats.model, "Qwen/Qwen3-0.6B");
        assert_eq!(stats.settings.max_queued_requests, 256);
        assert!(!stats.settings.response_cache_available);
        assert!(stats.workers.is_empty(), "no worker was running");
    }

    #[tokio::test]
    async fn polling_reports_the_model_and_workers_and_never_the_key() {
        let url = fake_vapi("sk-vapi-1").await;
        let plugin = VapiPlugin::new(secrets_with("sk-vapi-1"));
        plugin
            .apply(Some(configured(&url)))
            .await
            .expect("the gateway is reachable and the key is right");

        let status = plugin.status().await;
        assert_eq!(status["gateways"][0]["up"], true);
        assert_eq!(status["gateways"][0]["model"], "Qwen/Qwen3-0.6B");
        assert_eq!(status["gateways"][0]["workers"], 1);

        let mut view = plugin.clone().view_routes();
        let (code, gateways) = call(&mut view, Method::GET, "/gateways", "").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(gateways[0]["name"], "local");
        assert_eq!(gateways[0]["authenticated"], true);
        assert_eq!(gateways[0]["stats"]["workers"][0]["id"], "w-3af9");
        assert!(
            !gateways.to_string().contains("sk-vapi-1"),
            "the gateway list never carries the key"
        );

        // The proxied body is vapi's own, including fields the plugin does not
        // model — the page gets everything the gateway said.
        let (code, stats) = call(&mut view, Method::GET, "/gateways/local/stats", "").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(stats["cache_hits"], 3);
        assert_eq!(stats["workers"][0]["kv_pct"], 12);

        let (code, _) = call(&mut view, Method::GET, "/gateways/nope/stats", "").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        plugin.apply(None).await.unwrap();
    }

    #[tokio::test]
    async fn completions_are_forwarded_with_the_stored_key_and_streaming_is_refused() {
        let url = fake_vapi("sk-vapi-2").await;
        let plugin = VapiPlugin::new(secrets_with("sk-vapi-2"));
        plugin.apply(Some(configured(&url))).await.unwrap();
        let mut crud = plugin.clone().crud_routes();

        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let (code, echo) = call(
            &mut crud,
            Method::POST,
            "/gateways/local/chat/completions",
            body,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(echo["path"], "/v1/chat/completions");
        assert_eq!(echo["method"], "POST");
        assert_eq!(echo["body"], body, "the body is forwarded byte for byte");

        let (_, echo) = call(
            &mut crud,
            Method::POST,
            "/gateways/local/decisions",
            r#"{"questions":[]}"#,
        )
        .await;
        assert_eq!(echo["path"], "/v1/decisions");

        let (code, err) = call(
            &mut crud,
            Method::POST,
            "/gateways/local/chat/completions",
            r#"{"model":"m","stream":true}"#,
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(
            err["error"].as_str().unwrap().contains("cannot stream"),
            "{err}"
        );

        let (code, _) = call(
            &mut crud,
            Method::POST,
            "/gateways/local/completions",
            "not json",
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        plugin.apply(None).await.unwrap();
    }

    #[tokio::test]
    async fn a_partial_settings_change_keeps_the_fields_it_did_not_name() {
        let url = fake_vapi("sk-vapi-3").await;
        let plugin = VapiPlugin::new(secrets_with("sk-vapi-3"));
        plugin.apply(Some(configured(&url))).await.unwrap();
        let mut crud = plugin.clone().crud_routes();

        let (code, applied) = call(
            &mut crud,
            Method::PUT,
            "/gateways/local/settings",
            r#"{"response_cache": false}"#,
        )
        .await;
        assert_eq!(
            code,
            StatusCode::OK,
            "a 303 from vapi is the acknowledgement"
        );
        assert_eq!(applied["response_cache"], false);
        assert_eq!(
            applied["max_queued_requests"], 64,
            "the settings it did not name come from the gateway"
        );
        assert_eq!(applied["first_token_timeout_secs"], 30);

        let (code, err) = call(
            &mut crud,
            Method::PUT,
            "/gateways/local/settings",
            r#"{"max_batch_tokens": 8192}"#,
        )
        .await;
        assert_eq!(
            code,
            StatusCode::BAD_REQUEST,
            "a setting the gateway reads at a worker's start is not ours to move: {err}"
        );
        plugin.apply(None).await.unwrap();
    }

    #[tokio::test]
    async fn a_wrong_stored_key_is_a_bad_gateway_not_the_callers_401() {
        let url = fake_vapi("sk-right").await;
        let plugin = VapiPlugin::new(secrets_with("sk-wrong"));
        let err = plugin.apply(Some(configured(&url))).await.unwrap_err();
        assert!(err.contains("rejected"), "{err}");

        // Alive but unreadable is reported as up-with-an-error, so an operator
        // looks at the key rather than at the gateway.
        let status = plugin.status().await;
        assert_eq!(status["gateways"][0]["up"], true);
        assert!(
            status["gateways"][0]["error"]
                .as_str()
                .unwrap()
                .contains("401")
        );

        let mut view = plugin.clone().view_routes();
        let (code, body) = call(&mut view, Method::GET, "/gateways/local/stats", "").await;
        assert_eq!(code, StatusCode::BAD_GATEWAY);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("rejected the api_key")
        );
        plugin.apply(None).await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_gateway_is_reported_without_stopping_the_poller() {
        let plugin = VapiPlugin::new(secrets_with("sk-vapi-4"));
        // Port 1 on loopback: nothing listens, and connecting fails at once.
        let err = plugin
            .apply(Some(
                json!({"gateways": [{"name": "gone", "url": "http://127.0.0.1:1"}]}),
            ))
            .await
            .unwrap_err();
        assert!(err.starts_with("gone: "), "{err}");
        let status = plugin.status().await;
        assert_eq!(status["gateways"][0]["up"], false);
        assert!(plugin.poller.lock().unwrap().is_some(), "it keeps watching");
        plugin.apply(None).await.unwrap();
        assert!(plugin.poller.lock().unwrap().is_none());
        assert_eq!(plugin.status().await, json!({}));
    }
}
