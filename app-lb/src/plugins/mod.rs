//! Plugins: optional capabilities compiled into app-lb and switched on at runtime.
//!
//! Every other optional integration app-lb has — log shipping, discovery,
//! Incus, Route53 — is decided by an env var at startup and cannot change
//! without a restart. A plugin is the same idea with the switch moved to the
//! admin API: the set of plugins is fixed at compile time (there is no dynamic
//! loading, and there is not going to be), but whether each one runs, and with
//! what configuration, is an object on disk that the dashboard's Plugins page
//! and `heyctl plugins` edit.
//!
//! ## What a plugin owns
//!
//! Its own tasks. pingora's background services are started once and never
//! stopped, which is the wrong shape for something an operator can switch off,
//! so [`Plugin::apply`] is handed the new configuration (or `None`) and starts,
//! restarts or stops whatever it runs. The host serialises calls per plugin, so
//! an implementation never sees two `apply`s at once.
//!
//! Its own routes. Each plugin contributes a view-tier and a CRUD-tier router,
//! nested at `/api/plugins/<id>` and put behind the same gate as the rest of the
//! admin API. They are always mounted — the set is static — and answer 409
//! while the plugin is disabled, which is a more useful answer than a 404 on a
//! route the page knows exists.
//!
//! ## Credentials
//!
//! A plugin's configuration is stored in plain JSON beside the deployment
//! state, so it must never carry a credential. Fields that need one take a
//! [`crate::secrets::SecretRef`] and resolve it through the secret store when
//! they use it — the same indirection a deployment's git credential uses.

pub mod pgfc;
pub mod tunnel;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What the Plugins page shows about a plugin before it is switched on.
#[derive(Debug, Clone, Serialize)]
pub struct PluginMeta {
    /// Stable identifier: the file name on disk and the path segment under
    /// `/api/plugins`. Lowercase ASCII, digits and `-`.
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema for the configuration object, for the page's editor and for
    /// `heyctl plugins set`. Advisory: [`Plugin::validate`] is the authority.
    pub config_schema: Value,
}

/// A built-in plugin.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn meta(&self) -> PluginMeta;

    /// Reject a configuration before it is persisted. The message is returned
    /// to the caller as a 400, so it should say what to change.
    fn validate(&self, _config: &Value) -> Result<(), String> {
        Ok(())
    }

    /// Start, reconfigure or (with `None`) stop the plugin.
    ///
    /// An error leaves the plugin's record as written — the operator asked for
    /// it, and a transient failure (heyvmd down, a peer unreachable) should
    /// retry on the next start rather than silently revert — and is reported
    /// on the plugin's status as `last_error`.
    async fn apply(&self, config: Option<Value>) -> Result<(), String>;

    /// Live status for the page: whatever the plugin wants an operator to see.
    async fn status(&self) -> Value;

    /// View-tier routes, relative to `/api/plugins/<id>`.
    fn view_routes(self: Arc<Self>) -> Router {
        Router::new()
    }

    /// CRUD-tier routes, relative to `/api/plugins/<id>`.
    fn crud_routes(self: Arc<Self>) -> Router {
        Router::new()
    }
}

/// One plugin's persisted state.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct PluginRecord {
    pub enabled: bool,
    /// The plugin's configuration, kept while it is disabled so switching it
    /// back on does not mean typing it in again.
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub updated_at: u64,
}

// ---- the store ------------------------------------------------------------

/// One JSON file per plugin, in the shape `namespaces.rs` uses: an unreadable
/// record loses itself rather than every plugin, and no write rewrites the
/// others.
#[derive(Debug)]
pub struct PluginStore {
    records: ArcSwap<HashMap<String, PluginRecord>>,
    dir: PathBuf,
}

impl PluginStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            records: ArcSwap::from_pointee(HashMap::new()),
            dir: dir.into(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn get(&self, id: &str) -> Option<PluginRecord> {
        self.records.load().get(id).cloned()
    }

    /// Replace, then persist. Write-then-rename, so a crash mid-write leaves
    /// the previous version.
    pub fn put(&self, id: &str, record: PluginRecord) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec_pretty(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let path = self.dir.join(format!("{id}.json"));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        let mut next = (**self.records.load()).clone();
        next.insert(id.to_string(), record);
        self.records.store(Arc::new(next));
        Ok(())
    }

    /// Load every record, skipping any that will not parse. Returns
    /// `(loaded, skipped)`, like the other object stores.
    pub fn load(&self) -> (usize, usize) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return (0, 0);
        };
        let mut loaded = HashMap::new();
        let mut skipped = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| is_valid_id(s))
            else {
                skipped += 1;
                continue;
            };
            match std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<PluginRecord>(&b).ok())
            {
                Some(record) => {
                    loaded.insert(id.to_string(), record);
                }
                None => {
                    tracing::warn!(
                        "skipping unreadable plugin record {}; it is still on disk",
                        path.display()
                    );
                    skipped += 1;
                }
            }
        }
        let n = loaded.len();
        self.records.store(Arc::new(loaded));
        (n, skipped)
    }
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `app-lb-state.json` -> `app-lb-plugins.d`, beside it.
pub fn plugin_dir(state_path: &str) -> PathBuf {
    let path = Path::new(state_path);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("app-lb-state");
    let name = match stem.strip_suffix("-state") {
        Some(prefix) => format!("{prefix}-plugins.d"),
        None => format!("{stem}-plugins.d"),
    };
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

// ---- the host -------------------------------------------------------------

/// What `GET /api/plugins` returns per plugin.
#[derive(Debug, Clone, Serialize)]
pub struct PluginView {
    #[serde(flatten)]
    pub meta: PluginMeta,
    pub enabled: bool,
    pub config: Value,
    pub updated_at: u64,
    /// Why the last `apply` failed, if it did. Cleared by the next success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub status: Value,
}

#[derive(Debug)]
pub enum SetError {
    NotFound,
    Invalid(String),
    Io(std::io::Error),
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetError::NotFound => f.write_str("no such plugin"),
            SetError::Invalid(m) => write!(f, "invalid configuration: {m}"),
            SetError::Io(e) => write!(f, "could not save the plugin record: {e}"),
        }
    }
}

struct Slot {
    plugin: Arc<dyn Plugin>,
    /// Held across `apply`, so two edits to one plugin cannot interleave.
    lock: tokio::sync::Mutex<()>,
    last_error: std::sync::Mutex<Option<String>>,
}

/// The built-in plugins and their records.
pub struct PluginHost {
    slots: Vec<Arc<Slot>>,
    store: PluginStore,
}

impl PluginHost {
    pub fn new(plugins: Vec<Arc<dyn Plugin>>, store: PluginStore) -> Self {
        let slots = plugins
            .into_iter()
            .map(|plugin| {
                debug_assert!(
                    is_valid_id(plugin.meta().id),
                    "plugin id {:?}",
                    plugin.meta().id
                );
                Arc::new(Slot {
                    plugin,
                    lock: tokio::sync::Mutex::new(()),
                    last_error: std::sync::Mutex::new(None),
                })
            })
            .collect();
        Self { slots, store }
    }

    #[cfg(test)]
    pub fn store(&self) -> &PluginStore {
        &self.store
    }

    fn slot(&self, id: &str) -> Option<&Arc<Slot>> {
        self.slots.iter().find(|s| s.plugin.meta().id == id)
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.store.get(id).is_some_and(|r| r.enabled)
    }

    pub async fn list(&self) -> Vec<PluginView> {
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            out.push(self.view(slot).await);
        }
        out
    }

    pub async fn get(&self, id: &str) -> Option<PluginView> {
        let slot = self.slot(id)?;
        Some(self.view(slot).await)
    }

    async fn view(&self, slot: &Slot) -> PluginView {
        let meta = slot.plugin.meta();
        let record = self.store.get(meta.id).unwrap_or_default();
        // Read before the await: the guard is a std mutex and must not be
        // held across it.
        let last_error = slot.last_error.lock().unwrap().clone();
        let status = slot.plugin.status().await;
        PluginView {
            enabled: record.enabled,
            config: record.config,
            updated_at: record.updated_at,
            last_error,
            status,
            meta,
        }
    }

    /// Write a plugin's record and apply it. `config: None` keeps the stored
    /// configuration, which is what enable/disable want.
    pub async fn set(
        &self,
        id: &str,
        enabled: bool,
        config: Option<Value>,
    ) -> Result<PluginView, SetError> {
        let slot = self.slot(id).ok_or(SetError::NotFound)?.clone();
        let _held = slot.lock.lock().await;
        let current = self.store.get(id).unwrap_or_default();
        let config = config.unwrap_or(current.config);
        if enabled {
            slot.plugin.validate(&config).map_err(SetError::Invalid)?;
        }
        let record = PluginRecord {
            enabled,
            config: config.clone(),
            updated_at: crate::deployment::now_secs(),
        };
        self.store.put(id, record).map_err(SetError::Io)?;
        self.apply_slot(&slot, enabled.then_some(config)).await;
        drop(_held);
        Ok(self.view(&slot).await)
    }

    async fn apply_slot(&self, slot: &Slot, config: Option<Value>) {
        let id = slot.plugin.meta().id;
        let on = config.is_some();
        let result = slot.plugin.apply(config).await;
        if let Err(e) = &result {
            tracing::warn!(plugin = id, enabled = on, error = %e, "plugin apply failed");
        } else {
            tracing::info!(plugin = id, enabled = on, "plugin applied");
        }
        *slot.last_error.lock().unwrap() = result.err();
    }

    /// Apply every enabled record — the boot path.
    pub async fn start_enabled(&self) {
        for slot in &self.slots {
            let id = slot.plugin.meta().id;
            let Some(record) = self.store.get(id).filter(|r| r.enabled) else {
                continue;
            };
            let _held = slot.lock.lock().await;
            if let Err(e) = slot.plugin.validate(&record.config) {
                tracing::warn!(plugin = id, error = %e, "stored plugin configuration is invalid; not starting it");
                *slot.last_error.lock().unwrap() = Some(format!("invalid configuration: {e}"));
                continue;
            }
            self.apply_slot(slot, Some(record.config)).await;
        }
    }

    /// Stop every plugin — the shutdown path.
    pub async fn stop_all(&self) {
        for slot in &self.slots {
            if self.is_enabled(slot.plugin.meta().id) {
                let _held = slot.lock.lock().await;
                let _ = slot.plugin.apply(None).await;
            }
        }
    }

    /// Every plugin's routes, nested at `/api/plugins/<id>`, as `(view, crud)`.
    ///
    /// Each is wrapped so it answers 409 while its plugin is disabled. The
    /// caller puts the auth gate on top.
    pub fn routers(self: &Arc<Self>) -> (Router, Router) {
        let mut view = Router::new();
        let mut crud = Router::new();
        for slot in &self.slots {
            let id = slot.plugin.meta().id;
            let prefix = format!("/api/plugins/{id}");
            let guard = axum::middleware::from_fn({
                let host = self.clone();
                move |req: Request, next: Next| {
                    let host = host.clone();
                    async move {
                        if host.is_enabled(id) {
                            next.run(req).await
                        } else {
                            disabled(id)
                        }
                    }
                }
            });
            // `route_layer` panics on a router with no routes, and a plugin
            // with nothing to say on one tier is ordinary.
            let v = slot.plugin.clone().view_routes();
            if v.has_routes() {
                view = view.nest(&prefix, v.route_layer(guard.clone()));
            }
            let c = slot.plugin.clone().crud_routes();
            if c.has_routes() {
                crud = crud.nest(&prefix, c.route_layer(guard));
            }
        }
        (view, crud)
    }
}

fn disabled(id: &str) -> Response {
    (
        StatusCode::CONFLICT,
        axum::Json(serde_json::json!({
            "error": format!("the {id} plugin is disabled; enable it on /plugins or with `heyctl plugins enable {id}`"),
        })),
    )
        .into_response()
}

/// Runs the enabled plugins for the life of the process.
pub struct PluginService {
    host: Arc<PluginHost>,
}

impl PluginService {
    pub fn new(host: Arc<PluginHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl pingora_core::services::background::BackgroundService for PluginService {
    async fn start(&self, mut shutdown: pingora_core::server::ShutdownWatch) {
        self.host.start_enabled().await;
        while shutdown.changed().await.is_ok() {
            if *shutdown.borrow() {
                break;
            }
        }
        self.host.stop_all().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("app-lb-plugins-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Records every `apply` it is given.
    #[derive(Default)]
    struct Probe {
        applied: std::sync::Mutex<Vec<Option<Value>>>,
        fail: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Plugin for Probe {
        fn meta(&self) -> PluginMeta {
            PluginMeta {
                id: "probe",
                name: "Probe",
                description: "test plugin",
                config_schema: serde_json::json!({"type": "object"}),
            }
        }
        fn validate(&self, config: &Value) -> Result<(), String> {
            if config.get("bad").is_some() {
                Err("bad is not allowed".into())
            } else {
                Ok(())
            }
        }
        async fn apply(&self, config: Option<Value>) -> Result<(), String> {
            self.applied.lock().unwrap().push(config);
            if self.fail.load(Ordering::Relaxed) {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
        async fn status(&self) -> Value {
            serde_json::json!({"applies": self.applied.lock().unwrap().len()})
        }
        fn view_routes(self: Arc<Self>) -> Router {
            Router::new().route("/ping", axum::routing::get(|| async { "pong" }))
        }
    }

    fn host(dir: &TempDir) -> (Arc<Probe>, PluginHost) {
        let probe = Arc::new(Probe::default());
        let host = PluginHost::new(vec![probe.clone()], PluginStore::new(&dir.0));
        (probe, host)
    }

    #[tokio::test]
    async fn enabling_applies_and_persists_and_disabling_keeps_the_config() {
        let dir = TempDir::new();
        let (probe, host) = host(&dir);
        let cfg = serde_json::json!({"x": 1});

        let view = host.set("probe", true, Some(cfg.clone())).await.unwrap();
        assert!(view.enabled);
        assert_eq!(view.status["applies"], 1);

        let view = host.set("probe", false, None).await.unwrap();
        assert!(!view.enabled);
        assert_eq!(view.config, cfg, "disabling must keep the configuration");
        assert_eq!(
            *probe.applied.lock().unwrap(),
            vec![Some(cfg.clone()), None]
        );

        let reloaded = PluginStore::new(&dir.0);
        assert_eq!(reloaded.load(), (1, 0));
        assert_eq!(reloaded.get("probe").unwrap().config, cfg);
    }

    #[tokio::test]
    async fn an_invalid_config_is_refused_before_it_is_written() {
        let dir = TempDir::new();
        let (probe, host) = host(&dir);
        let e = host
            .set("probe", true, Some(serde_json::json!({"bad": 1})))
            .await
            .unwrap_err();
        assert!(matches!(e, SetError::Invalid(_)));
        assert!(host.store().get("probe").is_none());
        assert!(probe.applied.lock().unwrap().is_empty());
        assert!(matches!(
            host.set("nope", true, None).await,
            Err(SetError::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_failed_apply_is_kept_and_reported() {
        let dir = TempDir::new();
        let (probe, host) = host(&dir);
        probe.fail.store(true, Ordering::Relaxed);
        let view = host
            .set("probe", true, Some(serde_json::json!({})))
            .await
            .unwrap();
        assert!(view.enabled, "the operator's intent is kept");
        assert_eq!(view.last_error.as_deref(), Some("boom"));

        probe.fail.store(false, Ordering::Relaxed);
        host.start_enabled().await;
        assert_eq!(host.get("probe").await.unwrap().last_error, None);
    }

    #[tokio::test]
    async fn plugin_routes_are_nested_and_refuse_while_disabled() {
        use tower_service::Service;
        let dir = TempDir::new();
        let (_, host) = host(&dir);
        let host = Arc::new(host);
        let (mut view, crud) = host.routers();
        assert!(
            !crud.has_routes(),
            "a plugin with no CRUD routes contributes none"
        );

        let get = || {
            axum::http::Request::get("/api/plugins/probe/ping")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        assert_eq!(
            view.call(get()).await.unwrap().status(),
            StatusCode::CONFLICT
        );

        host.set("probe", true, Some(serde_json::json!({})))
            .await
            .unwrap();
        let resp = view.call(get()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn unreadable_records_are_skipped_and_the_directory_sits_beside_the_state() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("good.json"), br#"{"enabled":true,"config":{}}"#).unwrap();
        std::fs::write(dir.0.join("bad.json"), b"{ nope").unwrap();
        std::fs::write(dir.0.join("Bad Name.json"), br#"{"enabled":true}"#).unwrap();
        let store = PluginStore::new(&dir.0);
        assert_eq!(store.load(), (1, 2));
        assert_eq!(
            plugin_dir("/var/lib/app-lb/app-lb-state.json"),
            PathBuf::from("/var/lib/app-lb/app-lb-plugins.d"),
        );
    }
}
