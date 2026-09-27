//! Polls Orchestrator-owned service endpoint sets into static deployments.

use crate::registry::Registry;
use crate::secrets::SecretStore;
use async_trait::async_trait;
use futures::future::join_all;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use serde::Deserialize;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct DiscoveryConfig {
    base_url: reqwest::Url,
    token: String,
    interval: Duration,
}

impl DiscoveryConfig {
    pub fn from_env() -> Result<Option<Self>, String> {
        let url = std::env::var("APP_LB_DISCOVERY_URL").ok().filter(|v| !v.trim().is_empty());
        let token = std::env::var("APP_LB_DISCOVERY_TOKEN").ok().filter(|v| !v.trim().is_empty());
        let (url, token) = match (url, token) {
            (None, None) => return Ok(None),
            (Some(url), Some(token)) => (url, token),
            _ => return Err("APP_LB_DISCOVERY_URL and APP_LB_DISCOVERY_TOKEN must be configured together".into()),
        };
        let interval = std::env::var("APP_LB_DISCOVERY_INTERVAL_SECS")
            .unwrap_or_else(|_| "5".into())
            .parse::<u64>()
            .map_err(|_| "APP_LB_DISCOVERY_INTERVAL_SECS must be a positive number".to_string())?;
        if interval == 0 {
            return Err("APP_LB_DISCOVERY_INTERVAL_SECS must be positive".into());
        }
        let base_url = reqwest::Url::parse(&url)
            .map_err(|e| format!("APP_LB_DISCOVERY_URL is invalid: {e}"))?;
        if !matches!(base_url.scheme(), "http" | "https") || !base_url.username().is_empty()
            || base_url.password().is_some() || base_url.query().is_some() || base_url.fragment().is_some()
        {
            return Err("APP_LB_DISCOVERY_URL must be credential-free HTTP(S) without query or fragment".into());
        }
        Ok(Some(Self { base_url, token, interval: Duration::from_secs(interval) }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    service_id: String,
    version: u64,
    region: Option<String>,
    regional_policy: Option<serde_json::Value>,
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    url: String,
    region: Option<String>,
    health_status: HealthStatus,
    draining: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum HealthStatus { Healthy, Unhealthy, Unknown }

pub struct DiscoveryWatcher {
    cfg: Option<DiscoveryConfig>,
    registry: Arc<Registry>,
    secrets: Arc<SecretStore>,
    client: reqwest::Client,
    failed: tokio::sync::Mutex<HashSet<(String, String)>>,
}

impl DiscoveryWatcher {
    pub fn new(cfg: Option<DiscoveryConfig>, registry: Arc<Registry>, secrets: Arc<SecretStore>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("discovery HTTP client configuration is valid");
        Self { cfg, registry, secrets, client, failed: Default::default() }
    }

    fn service_url(&self, service_id: &str) -> Result<reqwest::Url, String> {
        let mut url = self.cfg.as_ref().ok_or("discovery source is not configured")?.base_url.clone();
        let mut segments = url.path_segments_mut().map_err(|_| "discovery base URL cannot be a base".to_string())?;
        segments.pop_if_empty();
        segments.extend(["orchestration", "services", service_id, "discovery"]);
        drop(segments);
        Ok(url)
    }

    async fn tick(&self) {
        let targets = self.registry.discovery_targets();
        let refreshes = targets
            .iter()
            .map(|(deployment_id, service_id, staged)| self.refresh_target(deployment_id, service_id, *staged));
        let results = join_all(refreshes).await;
        let mut failed = self.failed.lock().await;
        let target_keys: HashSet<_> = targets.iter().map(|(d,s,_)| (d.clone(),s.clone())).collect();
        failed.retain(|key| target_keys.contains(key));
        for ((deployment_id, service_id, _), result) in targets.iter().zip(results) {
            let key = (deployment_id.clone(), service_id.clone());
            match result {
                Err(error) if failed.insert(key.clone()) => {
                    tracing::warn!(deployment = %deployment_id, service = %service_id, %error, "service discovery refresh failed; retaining last good upstream set");
                }
                Ok(_) if failed.remove(&key) => {
                    tracing::info!(deployment = %deployment_id, service = %service_id, "service discovery refresh recovered");
                }
                _ => {}
            }
        }
    }

    #[cfg(test)]
    async fn refresh(&self, deployment_id: &str, service_id: &str) -> Result<bool, String> {
        self.refresh_target(deployment_id, service_id, false).await
    }

    async fn refresh_target(&self, deployment_id: &str, service_id: &str, staged: bool) -> Result<bool, String> {
        let Some(before) = (if staged { self.registry.staged(deployment_id) } else { self.registry.get(deployment_id) }) else { return Ok(false) };
        let Some(discovery) = &before.spec.discovery else { return Ok(false) };
        if discovery.service_id != service_id { return Ok(false); }
        let (source, token) = if let Some(source) = &discovery.source {
            let url = validate_source_url(&source.url)?;
            let mut auth = source.auth.clone();
            auth.scope_to(&before.spec.namespace);
            let token = self.secrets.resolve(&auth).map_err(|e| e.to_string())?;
            if token.trim().is_empty() { return Err("discovery credential is empty".into()); }
            (url.to_string(), token)
        } else {
            (self.service_url(service_id)?.to_string(), self.cfg.as_ref().unwrap().token.clone())
        };
        let source = if let Some(region) = &discovery.region {
            let mut url = reqwest::Url::parse(&source).map_err(|e| e.to_string())?;
            url.query_pairs_mut().append_pair("region", region);
            url.to_string()
        } else { source };
        if before.state().discovery_source_url.as_ref().is_some_and(|old| old != &source) {
            return Err("discovery authority changed; refusing to reuse the previous source's version".into());
        }
        if let (Some(regional), Some(router)) = (&discovery.regional, &before.regional) {
            let region = discovery.region.as_deref().ok_or("regional scope missing")?;
            let mut url = reqwest::Url::parse(&source).map_err(|e| e.to_string())?;
            url.query_pairs_mut().append_pair("protocol", "regional-v1")
                .append_pair("gatewayId", &regional.gateway_id).append_pair("bootId", &router.boot_id);
            let snapshot: crate::regional::Snapshot = self.client.get(url).bearer_auth(&token).send().await
                .map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?
                .json().await.map_err(|e| e.to_string())?;
            let own_backend = snapshot.policies.iter().find(|p| p.generation == snapshot.proposal_generation)
                .and_then(|p| p.policy.regions.iter().find(|r| r.region == region))
                .and_then(|r| r.gateways.iter().find(|g| g.id == regional.gateway_id))
                .ok_or("snapshot has no local gateway binding")?.backend_server_id.clone();
            let mut upstreams = BTreeSet::new();
            for endpoint in &snapshot.endpoints {
                if endpoint.region != region || endpoint.backend_server_id.is_empty() || endpoint.deployment_id.is_empty()
                    || !matches!(endpoint.health_status.as_str(), "healthy" | "unhealthy" | "unknown") {
                    return Err("invalid regional endpoint scope/identity/health".into());
                }
                if endpoint.backend_server_id != own_backend { continue; }
                let peer = upstream_from_url(&endpoint.url)?;
                let addr: std::net::SocketAddr = peer.parse().map_err(|_| "local gateway endpoint is not a socket address")?;
                if !addr.ip().is_loopback() { return Err("local gateway endpoint must be loopback".into()); }
                if endpoint.health_status == "healthy" && !endpoint.draining { upstreams.insert(peer); }
            }
            let _guard = self.registry.change_guard().await;
            let Some(current) = (if staged { self.registry.staged(deployment_id) } else { self.registry.get(deployment_id) }) else { return Ok(false); };
            if !Arc::ptr_eq(&current, &before) { return Ok(false); }
            if current.state().discovery_version.is_some_and(|v| snapshot.version < v) { return Ok(false); }
            let version = snapshot.version;
            let local: Vec<_> = upstreams.iter().map(|peer| current.backends().iter().find(|b| b.peer == *peer)
                .cloned().unwrap_or_else(|| Arc::new(crate::deployment::VmBackend::for_upstream(peer.clone())))).collect();
            if !router.apply(snapshot, regional, service_id, region, local.clone())? { return Ok(false); }
            let deployment = if staged {
                self.registry.apply_staged_discovery(&current, upstreams.into_iter().collect()).ok_or("staged runtime changed")?
            } else { self.registry.apply_discovery_upstreams(&current, upstreams.into_iter().collect()) };
            deployment.set_backends(local);
            deployment.mutate_state(|s| { s.discovery_version = Some(version); s.discovery_source_url = Some(source); });
            if !staged { self.registry.persist_one(deployment_id).map_err(|e| e.to_string())?; }
            return Ok(true);
        }
        let snapshot: Snapshot = self.client.get(&source)
            .bearer_auth(&token).send().await.map_err(|e| e.to_string())?
            .error_for_status().map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        if snapshot.service_id != service_id {
            return Err(format!("snapshot serviceId {:?} does not match {:?}", snapshot.service_id, service_id));
        }
        let upstreams = snapshot_upstreams(&snapshot, discovery.region.as_deref())?;
        let _guard = self.registry.change_guard().await;
        let Some(current) = (if staged { self.registry.staged(deployment_id) } else { self.registry.get(deployment_id) }) else { return Ok(false) };
        // Do not apply an in-flight response after its deployment was replaced,
        // even when the replacement has the same service ID and source URL.
        if !Arc::ptr_eq(&before, &current) {
            return Ok(false);
        }
        let previous_version = current.state().discovery_version;
        let previous_source = current.state().discovery_source_url.clone();
        if previous_source.as_ref().is_some_and(|old| old != &source) {
            return Err("discovery authority changed; refusing to reuse the previous source's version".into());
        }
        if previous_version.is_some_and(|v| snapshot.version < v) || (previous_source.is_some() && !should_apply(
            previous_version,
            snapshot.version,
            &current.spec.upstreams,
            &upstreams,
        )) {
            return Ok(false);
        }
        let deployment = if staged {
            self.registry.apply_staged_discovery(&current, upstreams).ok_or("staged runtime changed")?
        } else { self.registry.apply_discovery_upstreams(&current, upstreams) };
        deployment.mutate_state(|state| {
            state.discovery_version = Some(snapshot.version);
            state.discovery_source_url = Some(source);
        });
        if !staged && let Err(error) = self.registry.persist_one(deployment_id) {
            // Keep the previous version eligible for retry. The in-memory
            // upstream set is already safe to route, but it is not durable yet.
            deployment.mutate_state(|state| {
                state.discovery_version = previous_version;
                state.discovery_source_url = previous_source;
            });
            return Err(error.to_string());
        }
        Ok(true)
    }
}

pub(crate) fn validate_source_url(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "source URL is invalid".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some()
        || url.query().is_some() || url.fragment().is_some()
    {
        return Err("source URL must be credential-free HTTP(S) without query or fragment".into());
    }
    Ok(url)
}

fn should_apply(
    current_version: Option<u64>,
    candidate_version: u64,
    current_upstreams: &[String],
    candidate_upstreams: &[String],
) -> bool {
    match current_version {
        Some(version) if candidate_version < version => false,
        Some(version) if candidate_version == version => current_upstreams != candidate_upstreams,
        _ => true,
    }
}

fn snapshot_upstreams(snapshot: &Snapshot, region: Option<&str>) -> Result<Vec<String>, String> {
    if snapshot.regional_policy.is_some() {
        return Err("regional routing policy requires a hierarchical consumer; refusing flattened adoption".into());
    }
    if snapshot.region.as_deref() != region {
        return Err("discovery response does not attest the requested region scope".into());
    }
    if let Some(region) = region {
        if snapshot.endpoints.iter().any(|e| e.region.as_deref() != Some(region)) {
            return Err("regional discovery contains foreign or unplaced endpoints".into());
        }
    }
    snapshot.endpoints.iter()
        .filter(|e| e.health_status == HealthStatus::Healthy && !e.draining)
        .map(|e| upstream_from_url(&e.url))
        .collect::<Result<BTreeSet<_>, _>>()
        .map(|upstreams| upstreams.into_iter().collect())
}

pub(crate) fn upstream_from_url(value: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(value).map_err(|e| format!("bad endpoint URL {value:?}: {e}"))?;
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some()
        || url.query().is_some() || url.fragment().is_some() || url.path() != "/"
    {
        return Err(format!("endpoint URL must be plaintext, credential-free and pathless: {value:?}"));
    }
    let host = url.host_str().ok_or_else(|| format!("endpoint URL has no host: {value:?}"))?;
    // `Url::port()` normalizes an explicit default `:80` to `None`, so inspect
    // the original authority to distinguish it from a genuinely omitted port.
    let authority = value.split_once("//").map(|(_, rest)| rest)
        .and_then(|rest| rest.split(['/', '?', '#']).next()).unwrap_or("");
    let has_explicit_port = if authority.starts_with('[') {
        authority.rfind("]:").is_some()
    } else {
        authority.rsplit_once(':').is_some()
    };
    if !has_explicit_port {
        return Err(format!("endpoint URL has no explicit port: {value:?}"));
    }
    let port = url.port_or_known_default().expect("http URL has a default port");
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    Ok(if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") })
}

#[async_trait]
impl BackgroundService for DiscoveryWatcher {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut ticker = tokio::time::interval(self.cfg.as_ref().map_or(Duration::from_secs(5), |c| c.interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => self.tick().await,
                _ = shutdown.changed() => if *shutdown.borrow() { break },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DeploymentSpec, SpecError};

    #[tokio::test]
    async fn managed_source_survives_restart_and_scopes_rotated_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/snapshot", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = seen.clone();
        let app = axum::Router::new().route("/snapshot", axum::routing::get(move |headers: axum::http::HeaderMap,
            axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>| {
            let captured = captured.clone();
            async move {
                assert_eq!(query.get("region").map(String::as_str), Some("eu1"));
                captured.lock().unwrap().push(headers["authorization"].to_str().unwrap().to_owned());
                axum::Json(serde_json::json!({"serviceId":"svc","version":9,"region":"eu1","endpoints":[
                    {"url":"http://east:8081","region":"eu1","healthStatus":"healthy","draining":false},
                    {"url":"http://west:9092","region":"eu1","healthStatus":"healthy","draining":true}]}))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let dir = std::env::temp_dir().join(format!("app-lb-managed-discovery-{}", crate::rollout::revision()));
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Arc::new(Registry::new(dir.join("state.json")));
        let secrets = Arc::new(SecretStore::new(dir.join("secrets.json"), None));
        let put = |namespace: &str, token: &str| {
            secrets.put(serde_json::from_value(serde_json::json!({"id":"reader","namespace":namespace,"data":{"token":token}})).unwrap());
        };
        put("default", "wrong-namespace");
        put("team", "first");
        let mut spec: DeploymentSpec = serde_json::from_value(serde_json::json!({"id":"svc","namespace":"team",
            "routes":[{"host":"svc.example"}],"discovery":{"service_id":"svc","region":"eu1",
            "source":{"url":url,"auth":{"secret":"reader","namespace":"default"}}}})).unwrap();
        spec.normalize();
        spec.validate().unwrap();
        assert_eq!(spec.secret_ids(), vec!["reader"]);
        assert_eq!(spec.discovery.as_ref().unwrap().source.as_ref().unwrap().auth.namespace(), "team");
        registry.upsert(spec);
        let watcher = DiscoveryWatcher::new(None, registry.clone(), secrets.clone());
        assert!(watcher.refresh("svc", "svc").await.unwrap());
        assert_eq!(registry.get("svc").unwrap().spec.upstreams, ["east:8081"]);
        put("team", "rotated");
        secrets.persist().unwrap();
        let loaded = Arc::new(Registry::new(dir.join("state.json")));
        loaded.load().unwrap();
        let loaded_secrets = Arc::new(SecretStore::new(dir.join("secrets.json"), None));
        loaded_secrets.load().unwrap();
        let restarted = DiscoveryWatcher::new(None, loaded.clone(), loaded_secrets.clone());
        assert!(!restarted.refresh("svc", "svc").await.unwrap());
        assert_eq!(*seen.lock().unwrap(), ["Bearer first", "Bearer rotated"]);
        assert_eq!(loaded.get("svc").unwrap().state().discovery_source_url.as_deref(), Some(format!("{url}?region=eu1").as_str()));
        loaded_secrets.remove("team", "reader");
        assert!(restarted.refresh("svc", "svc").await.is_err());
        assert_eq!(loaded.get("svc").unwrap().spec.upstreams, ["east:8081"]);
        assert_eq!(seen.lock().unwrap().len(), 2, "missing own credential must not use another namespace");
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn response_for_replaced_deployment_is_discarded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let arrived = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (a, r) = (arrived.clone(), release.clone());
        let app = axum::Router::new().route("/orchestration/services/svc/discovery", axum::routing::get(move || {
            let (a, r) = (a.clone(), r.clone());
            async move {
                a.notify_one(); r.notified().await;
                axum::Json(serde_json::json!({"serviceId":"svc","version":99,"endpoints":[
                    {"url":"http://stale:8080","healthStatus":"healthy","draining":false}]}))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let registry = Arc::new(Registry::new("unused.json"));
        let spec: DeploymentSpec = serde_json::from_value(serde_json::json!({"id":"svc",
            "routes":[{"host":"svc.example"}],"discovery":{"service_id":"svc"},"upstreams":["original:8080"]})).unwrap();
        registry.upsert(spec.clone());
        let watcher = DiscoveryWatcher::new(Some(DiscoveryConfig { base_url: base.parse().unwrap(), token:"test".into(), interval:Duration::from_secs(1) }),
            registry.clone(), Arc::new(SecretStore::new("unused-secrets.json", None)));
        let refresh = tokio::spawn(async move { watcher.refresh("svc", "svc").await });
        tokio::time::timeout(Duration::from_secs(3), arrived.notified()).await.unwrap();
        registry.upsert(spec);
        release.notify_one();
        assert!(!refresh.await.unwrap().unwrap());
        assert_eq!(registry.get("svc").unwrap().spec.upstreams, ["original:8080"]);
        assert_eq!(registry.get("svc").unwrap().state().discovery_version, None);
        server.abort();
    }

    #[test]
    fn managed_source_rejects_credentials_and_non_http_urls() {
        for url in ["file:///tmp/snapshot", "https://user:password@example.com/snapshot", "https://example.com/?token=x", "https://example.com/#fragment"] {
            assert!(validate_source_url(url).is_err());
        }
        assert!(validate_source_url("https://authority.example/snapshot").is_ok());
    }

    #[tokio::test]
    async fn applied_source_is_persisted_and_cannot_be_relabelled_after_restart() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new().route("/orchestration/services/svc/discovery", axum::routing::get(|| async {
            axum::Json(serde_json::json!({"serviceId":"svc","version":7,"endpoints":[
                {"url":"http://node:8080","healthStatus":"healthy","draining":false}]}))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let dir = std::env::temp_dir().join(format!("app-lb-discovery-{}", crate::rollout::revision()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let registry = Arc::new(Registry::new(&path));
        let spec: DeploymentSpec = serde_json::from_value(serde_json::json!({"id":"svc",
            "routes":[{"host":"svc.example"}],"discovery":{"service_id":"svc"},"upstreams":["node:8080"]})).unwrap();
        registry.upsert(spec).mutate_state(|s| s.discovery_version = Some(7));
        let cfg = DiscoveryConfig { base_url: base.parse().unwrap(), token: "test".into(), interval: Duration::from_secs(1) };
        let secrets = Arc::new(SecretStore::new(dir.join("secrets.json"), None));
        let watcher = DiscoveryWatcher::new(Some(cfg.clone()), registry.clone(), secrets.clone());
        // Even unchanged legacy membership must acquire an actual source stamp.
        assert!(watcher.refresh("svc", "svc").await.unwrap());
        let source = format!("{base}/orchestration/services/svc/discovery");
        assert_eq!(registry.get("svc").unwrap().state().discovery_source_url.as_deref(), Some(source.as_str()));
        assert!(!watcher.refresh("svc", "svc").await.unwrap());
        let loaded = Arc::new(Registry::new(&path));
        loaded.load().unwrap();
        assert_eq!(loaded.get("svc").unwrap().state().discovery_source_url.as_deref(), Some(source.as_str()));
        loaded.get("svc").unwrap().mutate_state(|s| s.discovery_source_url = Some("http://original-authority".into()));
        let restarted = DiscoveryWatcher::new(Some(cfg), loaded.clone(), secrets);
        assert!(restarted.refresh("svc", "svc").await.unwrap_err().contains("authority changed"));
        assert_eq!(loaded.get("svc").unwrap().state().discovery_source_url.as_deref(), Some("http://original-authority"));
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn converts_only_plain_pathless_urls() {
        assert_eq!(upstream_from_url("http://node.local:4444/").unwrap(), "node.local:4444");
        assert_eq!(upstream_from_url("http://[::1]:80").unwrap(), "[::1]:80");
        for bad in ["https://node:443", "http://user@node:80", "http://node:80/x", "http://node:80/?x=1", "http://node"] {
            assert!(upstream_from_url(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn filters_deduplicates_and_sorts_atomically() {
        let snapshot = Snapshot { service_id: "cloud".into(), version: 2, region: None, regional_policy: None, endpoints: vec![
            Endpoint { url: "http://b:80".into(), region: None, health_status: HealthStatus::Healthy, draining: false },
            Endpoint { url: "http://a:80".into(), region: None, health_status: HealthStatus::Healthy, draining: false },
            Endpoint { url: "http://a:80".into(), region: None, health_status: HealthStatus::Healthy, draining: false },
            Endpoint { url: "http://c:80".into(), region: None, health_status: HealthStatus::Unhealthy, draining: false },
        ]};
        assert_eq!(snapshot_upstreams(&snapshot, None).unwrap(), ["a:80", "b:80"]);
    }

    #[test]
    fn regional_membership_requires_explicit_scope_even_for_empty_or_unhealthy_sets() {
        let mut snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "serviceId":"svc", "version":17, "region":"eu1", "endpoints":[
                {"region":"eu1", "url":"http://eu:8081", "healthStatus":"healthy", "draining":false},
                {"region":"eu1", "url":"http://eu:8082", "healthStatus":"healthy", "draining":true}
            ]
        })).unwrap();
        assert_eq!(snapshot_upstreams(&snapshot, Some("eu1")).unwrap(), ["eu:8081"]);
        assert!(snapshot_upstreams(&snapshot, Some("us3")).is_err());
        assert!(snapshot_upstreams(&snapshot, None).is_err());
        snapshot.endpoints[1].region = Some("us3".into());
        snapshot.endpoints[1].health_status = HealthStatus::Unhealthy;
        assert!(snapshot_upstreams(&snapshot, Some("eu1")).is_err());
        snapshot.endpoints[1].region = None;
        assert!(snapshot_upstreams(&snapshot, Some("eu1")).is_err());
        snapshot.endpoints.clear();
        assert!(snapshot_upstreams(&snapshot, Some("eu1")).unwrap().is_empty());
        snapshot.region = None;
        assert!(snapshot_upstreams(&snapshot, Some("eu1")).is_err(), "legacy server must not silently ignore scope");
        snapshot.regional_policy = Some(serde_json::json!({"version":1,"regions":[]}));
        assert!(snapshot_upstreams(&snapshot, None).is_err(), "legacy membership must not attest regional policy adoption");
    }

    #[test]
    fn versions_only_move_forward() {
        let current = ["a:80".to_string()];
        let changed = ["b:80".to_string()];
        assert!(should_apply(None, 1, &current, &current));
        assert!(!should_apply(Some(2), 2, &current, &current));
        assert!(should_apply(Some(2), 2, &current, &changed));
        assert!(!should_apply(Some(2), 1, &current, &changed));
        assert!(should_apply(Some(2), 3, &current, &current));
    }

    #[test]
    fn discovery_validation_allows_empty_upstreams_but_not_vm_or_site() {
        let base = serde_json::json!({
            "id": "cloud", "routes": [{"host": "cloud.example"}],
            "upstreams": [], "discovery": {"service_id": "cloud"}
        });
        let spec: DeploymentSpec = serde_json::from_value(base.clone()).unwrap();
        assert!(spec.validate().is_ok());

        let mut with_vm = base.clone();
        with_vm["vm"] = serde_json::json!({"driver":"firecracker", "port":8080});
        let spec: DeploymentSpec = serde_json::from_value(with_vm).unwrap();
        assert_eq!(spec.validate(), Err(SpecError::DiscoveryWithOtherBackend));

        let mut empty = base;
        empty["discovery"]["service_id"] = serde_json::json!("");
        let spec: DeploymentSpec = serde_json::from_value(empty).unwrap();
        assert_eq!(spec.validate(), Err(SpecError::EmptyDiscoveryServiceId));
    }
}
