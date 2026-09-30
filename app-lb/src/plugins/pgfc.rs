//! The pg-fc plugin: monitor and configure pg-vm-pool databases from app-lb.
//!
//! pg-vm-pool serves a JSON admin API on its dashboard listener (see
//! `pg-fc/src/dashboard/api.rs`), behind HTTP Basic auth. This plugin holds
//! that credential — as a reference into app-lb's secret store, never in its
//! own record — and exposes the API to the Plugins page through app-lb's own
//! admin gate. So the browser never sees a pg-fc password, and who may *read*
//! a node versus *act* on it is decided by app-lb's view and CRUD tiers rather
//! than by the one all-powerful credential pg-fc has.
//!
//! The proxying is deliberately thin: a request to
//! `/api/plugins/pgfc/nodes/<node>/<rest>` goes to `<url>/api/<rest>` with the
//! query string, and pg-fc's status and JSON body come back verbatim. The
//! routes are enumerated rather than a wildcard so the tier split is explicit
//! and a new pg-fc route is not exposed until someone decides which tier it
//! belongs on.
//!
//! A background poller reads every node's `/api/health` and `/api/host` so the
//! page (and the plugin's status) can show up/down and tier counts without a
//! request per node per render.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use pg_fc_api::{Health, HostInfo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Plugin, PluginMeta};
use crate::secrets::{SecretRef, SecretStore};

/// Reads answer fast; actions can wait on a VM stop/start, which pg-fc bounds
/// at 60s. Leave headroom so pg-fc's own timeout is the one that fires.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(75);
const POLL_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_POLL_SECS: u64 = 15;
const DEFAULT_PG_PORT: u16 = 6432;

// ---- configuration --------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PgFcConfig {
    pub nodes: Vec<NodeConfig>,
    /// How often to poll each node's health.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
}

fn default_poll_secs() -> u64 {
    DEFAULT_POLL_SECS
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Short name, used in URLs: lowercase letters, digits and `-`.
    pub name: String,
    /// pg-vm-pool's dashboard listener (`PG_VM_POOL_DASHBOARD_LISTEN`).
    pub url: String,
    /// `PG_VM_POOL_DASHBOARD_USER`. Omit when the dashboard has no auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// `PG_VM_POOL_DASHBOARD_PASSWORD`, as a reference into app-lb's secret
    /// store: `{"secret": "pg-fc", "key": "password"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretRef>,
    /// Where clients reach the pooler, for connection strings. Defaults to
    /// the host in `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_host: Option<String>,
    /// Defaults to 6432.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_port: Option<u16>,
}

impl NodeConfig {
    fn pg_host(&self) -> String {
        self.pg_host.clone().unwrap_or_else(|| {
            url::Url::parse(&self.url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| "127.0.0.1".into())
        })
    }
}

fn parse_config(config: &Value) -> Result<PgFcConfig, String> {
    let cfg: PgFcConfig = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    if cfg.nodes.is_empty() {
        return Err("add at least one node: {\"nodes\": [{\"name\": \"local\", \"url\": \"http://127.0.0.1:34199\", …}]}".into());
    }
    if !(5..=3600).contains(&cfg.poll_secs) {
        return Err("poll_secs must be between 5 and 3600".into());
    }
    let mut seen = std::collections::HashSet::new();
    for n in &cfg.nodes {
        let name_ok = !n.name.is_empty()
            && n.name.len() <= 32
            && n.name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !name_ok {
            return Err(format!(
                "node name {:?}: use 1–32 lowercase letters, digits or '-'",
                n.name
            ));
        }
        if !seen.insert(n.name.as_str()) {
            return Err(format!("node name {:?} is used twice", n.name));
        }
        match url::Url::parse(&n.url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host().is_some() => {}
            _ => {
                return Err(format!(
                    "node {}: url {:?} must be an http(s) URL",
                    n.name, n.url
                ));
            }
        }
        if let Some(p) = &n.password {
            p.validate()
                .map_err(|e| format!("node {}: password: {e}", n.name))?;
        }
        if n.password.is_some() && n.user.is_none() {
            return Err(format!("node {}: a password needs a user", n.name));
        }
    }
    Ok(cfg)
}

// ---- state ----------------------------------------------------------------

/// What the poller last learned about a node.
#[derive(Debug, Clone, Default, Serialize)]
struct NodeStatus {
    up: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Unix seconds of the last poll.
    polled_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<Health>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<HostInfo>,
}

/// One node as `GET …/nodes` reports it. No credential, by construction.
#[derive(Debug, Serialize)]
struct NodeView {
    name: String,
    url: String,
    pg_host: String,
    pg_port: u16,
    #[serde(flatten)]
    status: NodeStatus,
}

pub struct PgFcPlugin {
    /// For the poller task, which outlives the `&self` that `apply` gets.
    me: std::sync::Weak<PgFcPlugin>,
    secrets: Arc<SecretStore>,
    http: reqwest::Client,
    config: RwLock<Option<Arc<PgFcConfig>>>,
    status: RwLock<HashMap<String, NodeStatus>>,
    poller: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl PgFcPlugin {
    pub fn new(secrets: Arc<SecretStore>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            secrets,
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client builds"),
            config: RwLock::new(None),
            status: RwLock::new(HashMap::new()),
            poller: Mutex::new(None),
        })
    }

    fn node(&self, name: &str) -> Option<NodeConfig> {
        let cfg = self.config.read().unwrap().clone()?;
        cfg.nodes.iter().find(|n| n.name == name).cloned()
    }

    /// A request to `node`'s `/api<path>` with its credential attached.
    ///
    /// The password is resolved per request, so rotating the secret takes
    /// effect without re-applying the plugin.
    fn request(
        &self,
        node: &NodeConfig,
        method: Method,
        path_and_query: &str,
    ) -> Result<reqwest::RequestBuilder, String> {
        let url = format!("{}/api{path_and_query}", node.url.trim_end_matches('/'));
        let method =
            reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(|e| e.to_string())?;
        let mut req = self.http.request(method, url);
        if let Some(user) = &node.user {
            let password = match &node.password {
                Some(r) => Some(self.secrets.resolve(r).map_err(|e| {
                    format!("node {}: cannot resolve its password: {e}", node.name)
                })?),
                None => None,
            };
            req = req.basic_auth(user, password);
        }
        Ok(req)
    }

    async fn poll_node(&self, node: &NodeConfig) -> NodeStatus {
        let now = crate::deployment::now_secs();
        let get = |path: &'static str| async move {
            let req = self.request(node, Method::GET, path)?.timeout(POLL_TIMEOUT);
            let resp = req.send().await.map_err(|e| format!("{path}: {e}"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err("pg-fc rejected the configured credentials (401)".to_string());
            }
            if !status.is_success() {
                return Err(format!("{path}: HTTP {status}"));
            }
            resp.json::<Value>()
                .await
                .map_err(|e| format!("{path}: {e}"))
        };
        let (health, host) = tokio::join!(get("/health"), get("/host"));
        match health {
            Ok(h) => NodeStatus {
                up: true,
                error: host.as_ref().err().cloned(),
                polled_at: now,
                health: serde_json::from_value(h).ok(),
                host: host.ok().and_then(|v| serde_json::from_value(v).ok()),
            },
            Err(e) => NodeStatus {
                up: false,
                error: Some(e),
                polled_at: now,
                health: None,
                host: None,
            },
        }
    }

    async fn poll_all(&self, cfg: &PgFcConfig) {
        // Concurrently, so one slow node does not delay the others' status.
        let results = futures::future::join_all(
            cfg.nodes
                .iter()
                .map(|n| async move { (n.name.clone(), self.poll_node(n).await) }),
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

// ---- the plugin -----------------------------------------------------------

#[async_trait]
impl Plugin for PgFcPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: "pgfc",
            name: "pg-fc databases",
            description: "Monitor and configure pg-vm-pool (pg-fc) Postgres pools: schemas and their \
                          tiers, host health, dedicated databases, runtime settings and logs.",
            config_schema: json!({
                "type": "object",
                "required": ["nodes"],
                "properties": {
                    "poll_secs": {"type": "integer", "minimum": 5, "maximum": 3600, "default": DEFAULT_POLL_SECS},
                    "nodes": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["name", "url"],
                            "properties": {
                                "name": {"type": "string", "pattern": "^[a-z0-9-]{1,32}$"},
                                "url": {"type": "string", "description": "PG_VM_POOL_DASHBOARD_LISTEN, e.g. http://127.0.0.1:34199"},
                                "user": {"type": "string"},
                                "password": {
                                    "type": "object",
                                    "description": "A secret reference: {\"secret\": \"pg-fc\", \"key\": \"password\"}",
                                    "required": ["secret"],
                                    "properties": {"secret": {"type": "string"}, "key": {"type": "string"}}
                                },
                                "pg_host": {"type": "string"},
                                "pg_port": {"type": "integer", "default": DEFAULT_PG_PORT}
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
        // First poll inline, so enabling reports an unreachable node on the
        // spot instead of as a quiet "down" on the next page refresh.
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
        // The poller keeps running either way — a node that is down now may
        // come up — but an operator who just switched this on should hear
        // about the ones it cannot reach.
        let down: Vec<String> = self
            .status
            .read()
            .unwrap()
            .iter()
            .filter(|(_, s)| !s.up)
            .map(|(n, s)| format!("{n}: {}", s.error.as_deref().unwrap_or("down")))
            .collect();
        if down.is_empty() {
            Ok(())
        } else {
            Err(down.join("; "))
        }
    }

    async fn status(&self) -> Value {
        let Some(cfg) = self.config.read().unwrap().clone() else {
            return json!({});
        };
        let status = self.status.read().unwrap();
        let nodes: Vec<Value> = cfg
            .nodes
            .iter()
            .map(|n| {
                let s = status.get(&n.name).cloned().unwrap_or_default();
                json!({
                    "name": n.name,
                    "up": s.up,
                    "error": s.error,
                    "version": s.health.as_ref().map(|h| h.version.clone()),
                    "known_schemas": s.health.as_ref().map(|h| h.known_schemas),
                    "warm_schemas": s.health.as_ref().map(|h| h.warm_schemas),
                })
            })
            .collect();
        json!({ "nodes": nodes })
    }

    fn view_routes(self: Arc<Self>) -> Router {
        Router::new()
            .route("/nodes", get(list_nodes))
            .route("/nodes/:node/health", get(proxy))
            .route("/nodes/:node/host", get(proxy))
            .route("/nodes/:node/config", get(proxy))
            .route("/nodes/:node/databases", get(proxy))
            .route("/nodes/:node/schemas", get(proxy))
            .route("/nodes/:node/schemas/:schema", get(proxy))
            .route("/nodes/:node/events", get(proxy))
            .route("/nodes/:node/logs/:which", get(proxy))
            .with_state(self)
    }

    fn crud_routes(self: Arc<Self>) -> Router {
        Router::new()
            .route("/nodes/:node/schemas/:schema/:action", post(proxy))
            .route("/nodes/:node/maintenance/:op", post(proxy))
            .route("/nodes/:node/config", axum::routing::put(proxy))
            .route("/nodes/:node/databases", post(proxy))
            .route("/nodes/:node/databases/:database", delete(proxy))
            // A schema's Postgres log is read by running a command inside its
            // VM, so it sits with the actions rather than the reads.
            .route("/nodes/:node/logs/schema/:schema", get(proxy))
            .with_state(self)
    }
}

// ---- handlers -------------------------------------------------------------

fn fail(code: StatusCode, error: impl Into<String>) -> Response {
    (code, axum::Json(json!({ "error": error.into() }))).into_response()
}

/// `GET …/nodes` — the configured nodes and what the poller last saw.
async fn list_nodes(State(p): State<Arc<PgFcPlugin>>) -> Response {
    let Some(cfg) = p.config.read().unwrap().clone() else {
        return axum::Json(Vec::<Value>::new()).into_response();
    };
    let status = p.status.read().unwrap();
    let nodes: Vec<NodeView> = cfg
        .nodes
        .iter()
        .map(|n| NodeView {
            name: n.name.clone(),
            url: n.url.clone(),
            pg_host: n.pg_host(),
            pg_port: n.pg_port.unwrap_or(DEFAULT_PG_PORT),
            status: status.get(&n.name).cloned().unwrap_or_default(),
        })
        .collect();
    axum::Json(nodes).into_response()
}

/// Split a nested-router path `/nodes/<node>/<rest>` into the node and the
/// pg-fc path `/<rest>`.
fn split_node_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/nodes/")?;
    let slash = rest.find('/')?;
    Some((&rest[..slash], &rest[slash..]))
}

/// Forward to the node's `/api/<rest>`, returning pg-fc's status and body.
async fn proxy(
    State(p): State<Arc<PgFcPlugin>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let Some((node_name, rest)) = split_node_path(uri.path()) else {
        return fail(StatusCode::NOT_FOUND, "expected /nodes/<node>/…");
    };
    let Some(node) = p.node(node_name) else {
        return fail(
            StatusCode::NOT_FOUND,
            format!("no pg-fc node named {node_name:?}"),
        );
    };
    let path_and_query = match uri.query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest.to_string(),
    };
    let mut req = match p.request(&node, method.clone(), &path_and_query) {
        Ok(r) => r,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, e),
    };
    if matches!(method, Method::POST | Method::PUT) {
        req = req
            .header(header::CONTENT_TYPE.as_str(), "application/json")
            .body(body);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return fail(
                StatusCode::BAD_GATEWAY,
                format!("pg-fc node {node_name} is unreachable: {e}"),
            );
        }
    };
    let status = resp.status().as_u16();
    // A 401 from pg-fc means *app-lb's* stored credential is wrong. Passing it
    // through would read as the caller's own session failing (and could make
    // a browser prompt for a password that is not theirs to know).
    if status == 401 {
        return fail(
            StatusCode::BAD_GATEWAY,
            format!("pg-fc node {node_name} rejected the credentials configured for it"),
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
                format!("reading pg-fc's response: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tower_service::Service;

    fn secrets_with(password: &str) -> Arc<SecretStore> {
        let dir = std::env::temp_dir().join(format!(
            "app-lb-pgfc-{}-{}",
            std::process::id(),
            password.len()
        ));
        let store = Arc::new(SecretStore::new(dir.join("secrets.json"), None));
        store.put(crate::secrets::SecretSpec {
            id: "pg-fc".into(),
            namespace: crate::config::DEFAULT_NAMESPACE.into(),
            description: None,
            data: [("password".to_string(), password.to_string())].into(),
            updated_at: 0,
        });
        store
    }

    #[test]
    fn a_config_is_checked_before_it_is_stored() {
        let ok = json!({"nodes": [{"name": "local", "url": "http://127.0.0.1:34199", "user": "admin",
            "password": {"secret": "pg-fc", "key": "password"}}]});
        assert!(parse_config(&ok).is_ok());
        for bad in [
            json!({"nodes": []}),
            json!({"nodes": [{"name": "Local!", "url": "http://x"}]}),
            json!({"nodes": [{"name": "a", "url": "ftp://x"}]}),
            json!({"nodes": [{"name": "a", "url": "http://x"}, {"name": "a", "url": "http://y"}]}),
            json!({"nodes": [{"name": "a", "url": "http://x", "password": {"secret": "s"}}]}),
            json!({"nodes": [{"name": "a", "url": "http://x", "pasword": {}}]}),
            json!({"nodes": [{"name": "a", "url": "http://x"}], "poll_secs": 1}),
        ] {
            assert!(parse_config(&bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn node_paths_split_into_the_node_and_the_pg_fc_path() {
        assert_eq!(
            split_node_path("/nodes/local/schemas/acme/stop"),
            Some(("local", "/schemas/acme/stop"))
        );
        assert_eq!(
            split_node_path("/nodes/local/health"),
            Some(("local", "/health"))
        );
        assert_eq!(split_node_path("/nodes/local"), None);
        assert_eq!(split_node_path("/other"), None);
        let node = NodeConfig {
            name: "a".into(),
            url: "http://db.internal:34199".into(),
            user: None,
            password: None,
            pg_host: None,
            pg_port: None,
        };
        assert_eq!(node.pg_host(), "db.internal");
    }

    /// A fake pg-fc that insists on the right Basic credential and echoes what
    /// it was asked, so the test sees exactly what the plugin forwarded.
    async fn fake_pg_fc() -> String {
        use axum::extract::Request;
        let expected = format!(
            "Basic {}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "admin:s3cret")
        );
        let app = Router::new().fallback(move |req: Request| {
            let expected = expected.clone();
            async move {
                let auth = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
                if auth != expected {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                if req.uri().path() == "/api/health" {
                    return axum::Json(json!({"version": "9.9.9", "uptime_secs": 1, "listen": "x",
                        "warm_schemas": 1, "known_schemas": 2, "tls": false, "replication": false}))
                    .into_response();
                }
                let method = req.method().to_string();
                let uri = req.uri().to_string();
                let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap();
                (
                    StatusCode::ACCEPTED,
                    axum::Json(json!({"method": method, "uri": uri, "body": String::from_utf8_lossy(&body)})),
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

    #[tokio::test]
    async fn requests_are_forwarded_with_the_stored_credential_and_come_back_verbatim() {
        let url = fake_pg_fc().await;
        let plugin = PgFcPlugin::new(secrets_with("s3cret"));
        let cfg = json!({"nodes": [{"name": "local", "url": url, "user": "admin",
            "password": {"secret": "pg-fc", "key": "password"}}]});
        plugin
            .apply(Some(cfg))
            .await
            .expect("the node is reachable");

        let status = plugin.status().await;
        assert_eq!(status["nodes"][0]["up"], true);
        assert_eq!(status["nodes"][0]["version"], "9.9.9");

        let mut crud = plugin.clone().crud_routes();
        let (code, echo) = call(
            &mut crud,
            Method::POST,
            "/nodes/local/schemas/acme/resize",
            r#"{"size_class":"small"}"#,
        )
        .await;
        assert_eq!(code, StatusCode::ACCEPTED, "pg-fc's status passes through");
        assert_eq!(echo["method"], "POST");
        assert_eq!(echo["uri"], "/api/schemas/acme/resize");
        assert_eq!(echo["body"], r#"{"size_class":"small"}"#);

        let mut view = plugin.clone().view_routes();
        let (_, echo) = call(
            &mut view,
            Method::GET,
            "/nodes/local/schemas?tier=frozen",
            "",
        )
        .await;
        assert_eq!(
            echo["uri"], "/api/schemas?tier=frozen",
            "the query string is forwarded"
        );

        let (code, _) = call(&mut view, Method::GET, "/nodes/nope/schemas", "").await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        let (_, nodes) = call(&mut view, Method::GET, "/nodes", "").await;
        assert_eq!(nodes[0]["name"], "local");
        assert!(
            !nodes.to_string().contains("s3cret"),
            "the node list never carries the password"
        );
        plugin.apply(None).await.unwrap();
    }

    #[tokio::test]
    async fn a_wrong_stored_credential_is_a_bad_gateway_not_the_callers_401() {
        let url = fake_pg_fc().await;
        let plugin = PgFcPlugin::new(secrets_with("wrong"));
        let cfg = json!({"nodes": [{"name": "local", "url": url, "user": "admin",
            "password": {"secret": "pg-fc", "key": "password"}}]});
        let err = plugin.apply(Some(cfg)).await.unwrap_err();
        assert!(err.contains("rejected"), "{err}");

        let mut view = plugin.clone().view_routes();
        let (code, body) = call(&mut view, Method::GET, "/nodes/local/host", "").await;
        assert_eq!(code, StatusCode::BAD_GATEWAY);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("rejected the credentials")
        );
        plugin.apply(None).await.unwrap();
    }
}
