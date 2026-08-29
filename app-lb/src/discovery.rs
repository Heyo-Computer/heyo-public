//! Polls Orchestrator-owned service endpoint sets into static deployments.

use crate::registry::Registry;
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
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err("APP_LB_DISCOVERY_URL must use http or https".into());
        }
        Ok(Some(Self { base_url, token, interval: Duration::from_secs(interval) }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    service_id: String,
    version: u64,
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    url: String,
    health_status: HealthStatus,
    draining: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum HealthStatus { Healthy, Unhealthy, Unknown }

pub struct DiscoveryWatcher {
    cfg: DiscoveryConfig,
    registry: Arc<Registry>,
    client: reqwest::Client,
    failed: tokio::sync::Mutex<HashSet<(String, String)>>,
}

impl DiscoveryWatcher {
    pub fn new(cfg: DiscoveryConfig, registry: Arc<Registry>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("discovery HTTP client configuration is valid");
        Self { cfg, registry, client, failed: Default::default() }
    }

    fn service_url(&self, service_id: &str) -> Result<reqwest::Url, String> {
        let mut url = self.cfg.base_url.clone();
        let mut segments = url.path_segments_mut().map_err(|_| "discovery base URL cannot be a base".to_string())?;
        segments.pop_if_empty();
        segments.extend(["orchestration", "services", service_id, "discovery"]);
        drop(segments);
        Ok(url)
    }

    async fn tick(&self) {
        let targets: Vec<(String, String)> = self.registry.deployments().values()
            .filter_map(|d| d.spec.discovery.as_ref().map(|x| (d.spec.id.clone(), x.service_id.clone())))
            .collect();
        let refreshes = targets
            .iter()
            .map(|(deployment_id, service_id)| self.refresh(deployment_id, service_id));
        let results = join_all(refreshes).await;
        let mut failed = self.failed.lock().await;
        let target_keys: HashSet<_> = targets.iter().cloned().collect();
        failed.retain(|key| target_keys.contains(key));
        for ((deployment_id, service_id), result) in targets.iter().zip(results) {
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

    async fn refresh(&self, deployment_id: &str, service_id: &str) -> Result<bool, String> {
        let snapshot: Snapshot = self.client.get(self.service_url(service_id)?)
            .bearer_auth(&self.cfg.token).send().await.map_err(|e| e.to_string())?
            .error_for_status().map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        if snapshot.service_id != service_id {
            return Err(format!("snapshot serviceId {:?} does not match {:?}", snapshot.service_id, service_id));
        }
        let upstreams = snapshot_upstreams(&snapshot)?;
        let _guard = self.registry.change_guard().await;
        let Some(current) = self.registry.get(deployment_id) else { return Ok(false) };
        if current.spec.discovery.as_ref().map(|d| d.service_id.as_str()) != Some(service_id) {
            return Ok(false);
        }
        let previous_version = current.state().discovery_version;
        if !should_apply(
            previous_version,
            snapshot.version,
            &current.spec.upstreams,
            &upstreams,
        ) {
            return Ok(false);
        }
        let mut spec = current.spec.clone();
        spec.upstreams = upstreams;
        let deployment = self.registry.upsert(spec);
        deployment.mutate_state(|state| state.discovery_version = Some(snapshot.version));
        if let Err(error) = self.registry.persist_one(deployment_id) {
            // Keep the previous version eligible for retry. The in-memory
            // upstream set is already safe to route, but it is not durable yet.
            deployment.mutate_state(|state| state.discovery_version = previous_version);
            return Err(error.to_string());
        }
        Ok(true)
    }
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

fn snapshot_upstreams(snapshot: &Snapshot) -> Result<Vec<String>, String> {
    snapshot.endpoints.iter()
        .filter(|e| e.health_status == HealthStatus::Healthy && !e.draining)
        .map(|e| upstream_from_url(&e.url))
        .collect::<Result<BTreeSet<_>, _>>()
        .map(|upstreams| upstreams.into_iter().collect())
}

fn upstream_from_url(value: &str) -> Result<String, String> {
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
        let mut ticker = tokio::time::interval(self.cfg.interval);
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
        let snapshot = Snapshot { service_id: "cloud".into(), version: 2, endpoints: vec![
            Endpoint { url: "http://b:80".into(), health_status: HealthStatus::Healthy, draining: false },
            Endpoint { url: "http://a:80".into(), health_status: HealthStatus::Healthy, draining: false },
            Endpoint { url: "http://a:80".into(), health_status: HealthStatus::Healthy, draining: false },
            Endpoint { url: "http://c:80".into(), health_status: HealthStatus::Unhealthy, draining: false },
        ]};
        assert_eq!(snapshot_upstreams(&snapshot).unwrap(), ["a:80", "b:80"]);
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
