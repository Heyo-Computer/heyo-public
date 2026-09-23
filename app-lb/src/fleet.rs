//! Read-only, explicitly configured gateway observations. Never a placement or
//! rollout authority, and never a proxy for arbitrary browser-supplied URLs.
use crate::secrets::{SecretRef, SecretStore};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::{Arc, Mutex}, time::Duration, path::{Path, PathBuf}};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Gateway {
    id: String,
    region: String,
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    use_caller_auth: bool,
}

pub struct Fleet {
    gateways: Vec<Gateway>,
    client: reqwest::Client,
    secrets: Arc<SecretStore>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewConfig {
    pub gateways: Vec<Gateway>,
    pub control_plane: Vec<Gateway>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureViews {
    pub expected_revision: u64,
    pub config: ViewConfig,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedViews { revision: u64, config: ViewConfig }

#[derive(Serialize)]
pub struct ViewSnapshot {
    pub revision: u64,
    config: ViewConfig,
    pub externally_managed: bool,
    #[serde(skip)]
    pub fleet: Option<Arc<Fleet>>,
    #[serde(skip)]
    pub control_plane: Option<Arc<Fleet>>,
}

/// Gateway-local bindings, never application or rollout authority. Persist
/// before publishing; readers keep one immutable snapshot across a request.
pub struct ViewStore {
    path: PathBuf,
    current: arc_swap::ArcSwap<ViewSnapshot>,
    writer: Mutex<()>,
    secrets: Arc<SecretStore>,
}

impl ViewStore {
    pub fn open(path: PathBuf, secrets: Arc<SecretStore>, overrides: [Option<PathBuf>; 2]) -> Result<Self, String> {
        let saved = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<SavedViews>(&bytes).map_err(|_| "invalid persisted view configuration")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SavedViews { revision: 0, config: ViewConfig::default() },
            Err(_) => return Err("cannot read persisted view configuration".into()),
        };
        let mut config = saved.config;
        for (target, file) in [&mut config.gateways, &mut config.control_plane].into_iter().zip(&overrides) {
            if let Some(file) = file {
                *target = parse(&std::fs::read_to_string(file).map_err(|_| "cannot read configured view file")?)?;
            }
        }
        let snapshot = Self::prepare(saved.revision, config, overrides.iter().any(Option::is_some), secrets.clone())?;
        Ok(Self { path, current: arc_swap::ArcSwap::from_pointee(snapshot), writer: Mutex::new(()), secrets })
    }

    fn prepare(revision: u64, mut config: ViewConfig, externally_managed: bool, secrets: Arc<SecretStore>) -> Result<ViewSnapshot, String> {
        if config.control_plane.iter().any(|g| g.use_caller_auth) {
            return Err("Orchestrator bindings require service credentials".into());
        }
        let mut clients = Vec::new();
        for targets in [&mut config.gateways, &mut config.control_plane] {
            if targets.is_empty() { clients.push(None); continue; }
            *targets = parse(&serde_json::to_string(targets).map_err(|_| "invalid view configuration")?)?;
            clients.push(Some(Arc::new(Fleet::new(targets.clone(), secrets.clone())?)));
        }
        Ok(ViewSnapshot { revision, config, externally_managed, fleet: clients.remove(0), control_plane: clients.remove(0) })
    }

    pub fn snapshot(&self) -> Arc<ViewSnapshot> { self.current.load_full() }

    pub fn configure(&self, request: ConfigureViews) -> Result<Arc<ViewSnapshot>, (http::StatusCode, String)> {
        use http::StatusCode;
        use std::{fs::{File, OpenOptions}, io::Write, os::unix::fs::OpenOptionsExt};
        let _lock = self.writer.lock().map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "view writer unavailable".into()))?;
        let current = self.snapshot();
        if current.externally_managed { return Err((StatusCode::CONFLICT, "view configuration is managed by startup files".into())); }
        if request.expected_revision != current.revision { return Err((StatusCode::CONFLICT, "view configuration revision changed; read it again".into())); }
        let revision = current.revision.checked_add(1).ok_or((StatusCode::CONFLICT, "view revision exhausted".into()))?;
        let next = Self::prepare(revision, request.config, false, self.secrets.clone())
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        // Reject unresolved credentials before acknowledging activation. Values
        // stay in SecretStore; neither persistence nor the response contains them.
        for gateway in next.config.gateways.iter().chain(&next.config.control_plane) {
            if gateway.auth.as_ref().is_some_and(|auth| self.secrets.resolve(auth).map_or(true, |value| value.trim().is_empty())) {
                return Err((StatusCode::BAD_REQUEST, "view credential unavailable".into()));
            }
        }
        let bytes = serde_json::to_vec(&SavedViews { revision, config: next.config.clone() })
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot encode view configuration".into()))?;
        let next = Arc::new(next);
        let persist = || -> std::io::Result<()> {
            let parent = self.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)?;
            let mut nonce = [0u8; 16];
            openssl::rand::rand_bytes(&mut nonce).map_err(std::io::Error::other)?;
            let temp = self.path.with_extension(format!("{:032x}.writing", u128::from_be_bytes(nonce)));
            let mut file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&temp)?;
            let result = (|| {
                file.write_all(&bytes)?;
                file.sync_all()?;
                let directory = File::open(parent)?;
                std::fs::rename(&temp, &self.path)?;
                // Rename is the commit point. If the directory flush fails,
                // return an error but never allow a stale CAS to overwrite it.
                self.current.store(next.clone());
                directory.sync_all()
            })();
            if result.is_err() { let _ = std::fs::remove_file(&temp); }
            result
        };
        persist().map_err(|e| {
            tracing::error!(error = %e, "view configuration persistence failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "cannot persist view configuration".into())
        })?;
        Ok(next)
    }
}

#[derive(Serialize)]
pub struct Observation {
    id: String,
    region: String,
    dashboard_url: String,
    observed_at: u64,
    metrics: Option<GatewayMetrics>,
    error: Option<&'static str>,
}

// Allowlist fields: remote responses must not accidentally expose deployment
// specs, credentials, or new privileged API fields through this view.
#[derive(Deserialize, Serialize)]
struct GatewayMetrics {
    generated_at: u64,
    uptime_secs: u64,
    fleet: Pools,
}

#[derive(Deserialize, Serialize)]
struct Pools {
    deployments: usize,
    ready: usize,
    draining: usize,
    pending: usize,
    total_in_flight: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    services: Vec<Service>,
    next_cursor: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    service_id: String,
    desired_replicas: Option<u32>,
    replica_regions: Option<Vec<String>>,
    discovery_version: Option<u64>,
    endpoints: Option<Vec<Endpoint>>,
    rollout: Option<Rollout>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    region: Option<String>,
    revision: Option<String>,
    health_status: String,
    draining: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Rollout {
    operation_id: String,
    status: String,
    phase: String,
    target_revision: String,
}

fn parse(text: &str) -> Result<Vec<Gateway>, String> {
    let mut gateways: Vec<Gateway> = serde_json::from_str(text)
        .map_err(|_| "fleet file must be an array of gateway definitions".to_string())?;
    if gateways.is_empty() || gateways.len() > 32 {
        return Err("fleet must contain 1–32 gateways".into());
    }
    let mut ids = HashSet::new();
    let mut urls = HashSet::new();
    for gateway in &mut gateways {
        for value in [&gateway.id, &gateway.region] {
            if value.is_empty() || value.len() > 128
                || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
                return Err("gateway id and region must be bounded identifiers".into());
            }
        }
        let url = reqwest::Url::parse(&gateway.url).map_err(|_| "invalid gateway URL")?;
        if url.scheme() != "https" || url.host_str().is_none()
            || !url.username().is_empty() || url.password().is_some()
            || url.query().is_some() || url.fragment().is_some() || url.path() != "/" {
            return Err("gateway URL must be a credential-free HTTPS origin".into());
        }
        gateway.url = url.to_string();
        if !ids.insert(gateway.id.clone()) || !urls.insert(url) {
            return Err("duplicate gateway id or origin".into());
        }
        if gateway.use_caller_auth == gateway.auth.is_some() {
            return Err("choose exactly one gateway credential: auth or use_caller_auth".into());
        }
        if let Some(auth) = &gateway.auth {
            auth.validate().map_err(|_| "invalid fleet secret reference")?;
        }
    }
    Ok(gateways)
}

impl Fleet {
    fn new(gateways: Vec<Gateway>, secrets: Arc<SecretStore>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build().map_err(|_| "cannot build fleet HTTP client")?;
        Ok(Self { gateways, client, secrets })
    }

    async fn fetch<T: serde::de::DeserializeOwned>(&self, gateway: &Gateway, path: &str, caller: Option<&str>) -> Result<T, &'static str> {
        let request = self.client.get(format!("{}{path}", gateway.url.trim_end_matches('/')));
        let request = if gateway.use_caller_auth {
            request.bearer_auth(caller.filter(|s| !s.is_empty()).ok_or("Heyo sign-in required for regional observations")?)
        } else {
            let auth = gateway.auth.as_ref().ok_or("credential unavailable")?;
            let credential = self.secrets.resolve(auth).map_err(|_| "credential unavailable")?;
            if credential.trim().is_empty() { return Err("credential unavailable") }
            match &auth.username {
                Some(user) => request.basic_auth(user, Some(credential)),
                None => request.bearer_auth(credential),
            }
        };
        let mut response = request.send().await.map_err(|_| "gateway unreachable")?;
        if response.status().is_server_error() { return Err("gateway unavailable") }
        if !response.status().is_success() { return Err("gateway rejected observation") }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "gateway response interrupted")? {
            if body.len() + chunk.len() > 1024 * 1024 { return Err("gateway response too large") }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| "invalid gateway metrics")
    }

    /// Only read-only requests fail over. Never replay mutations or merge
    /// inventories from different sources. Operators must bind these endpoints
    /// to the same authoritative database, not independent regional copies.
    pub async fn inventory(&self, after: Option<&str>) -> Result<Inventory, &'static str> {
        let mut path = "/orchestration/services".to_string();
        if let Some(after) = after {
            path.push('?');
            path.push_str(&form_urlencoded::Serializer::new(String::new()).append_pair("after", after).finish());
        }
        let mut last = "control plane unavailable";
        for gateway in &self.gateways {
            match self.fetch(gateway, &path, None).await {
                Ok(inventory) => return Ok(inventory),
                Err(error @ ("gateway unreachable" | "gateway unavailable" | "gateway response interrupted")) => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    pub async fn observe(&self, caller: Option<&str>) -> Vec<Observation> {
        futures::future::join_all(self.gateways.iter().map(|gateway| async move {
            let result = self.fetch(gateway, "/metrics?summary=true&limit=0", caller).await;
            Observation {
                id: gateway.id.clone(), region: gateway.region.clone(),
                dashboard_url: format!("{}/dashboard?view=local", gateway.url.trim_end_matches('/')),
                observed_at: crate::deployment::now_secs(),
                error: result.as_ref().err().copied(), metrics: result.ok(),
            }
        })).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway(url: &str) -> serde_json::Value {
        serde_json::json!({"id":"us3-edge","region":"US","url":url,
            "auth":{"secret":"fleet-observer","key":"token"}})
    }

    #[test]
    fn view_configuration_is_durable_conditional_and_keeps_old_readers_pinned() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("views.json");
        let secrets = Arc::new(SecretStore::new(dir.path().join("secrets.json"), None));
        secrets.put(crate::secrets::SecretSpec { id:"fleet-observer".into(),namespace:"default".into(),
            description:None,updated_at:0,data:std::collections::BTreeMap::from([("token".into(),"never-persist-this-value".into())]) });
        let store = ViewStore::open(path.clone(), secrets.clone(), [None,None]).unwrap();
        let before = store.snapshot();
        let config: ViewConfig = serde_json::from_value(serde_json::json!({"gateways":[gateway("https://edge.example")],
            "control_plane":[gateway("https://authority.example")]})).unwrap();
        let updated = store.configure(ConfigureViews { expected_revision:0,config:config.clone() }).unwrap();
        assert_eq!(updated.revision,1);
        assert_eq!(updated.fleet.as_ref().unwrap().gateways[0].url,"https://edge.example/");
        assert_eq!(updated.control_plane.as_ref().unwrap().gateways[0].url,"https://authority.example/");
        assert!(before.fleet.is_none());
        assert_eq!(before.revision,0);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,0o600);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("never-persist-this-value"));
        assert!(!serde_json::to_string(&*updated).unwrap().contains("never-persist-this-value"));
        assert_eq!(store.configure(ConfigureViews { expected_revision:0,config:ViewConfig::default() }).err().unwrap().0,http::StatusCode::CONFLICT);
        let restarted = ViewStore::open(path.clone(), secrets.clone(), [None,None]).unwrap();
        assert_eq!(restarted.snapshot().revision,1);
        let mut invalid = config.clone();
        invalid.control_plane[0].url = "http://untrusted.example".into();
        assert_eq!(restarted.configure(ConfigureViews { expected_revision:1,config:invalid }).err().unwrap().0,http::StatusCode::BAD_REQUEST);
        let mut missing = config;
        missing.gateways[0].auth.as_mut().unwrap().key = "missing".into();
        assert_eq!(restarted.configure(ConfigureViews { expected_revision:1,config:missing }).err().unwrap().0,http::StatusCode::BAD_REQUEST);
        assert_eq!(restarted.snapshot().revision,1);
        let override_path = dir.path().join("override.json");
        std::fs::write(&override_path,serde_json::json!([gateway("https://operator.example")]).to_string()).unwrap();
        let managed = ViewStore::open(path, secrets, [Some(override_path),None]).unwrap();
        assert_eq!(managed.snapshot().fleet.as_ref().unwrap().gateways[0].url,"https://operator.example/");
        assert_eq!(managed.configure(ConfigureViews { expected_revision:1,config:ViewConfig::default() }).err().unwrap().0,http::StatusCode::CONFLICT);
    }

    #[test]
    fn failed_view_persistence_does_not_publish_and_corruption_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("views.json");
        let secrets = Arc::new(SecretStore::new(dir.path().join("secrets.json"),None));
        let store = ViewStore::open(path.clone(), secrets.clone(),[None,None]).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(store.configure(ConfigureViews { expected_revision:0,config:ViewConfig::default() }).err().unwrap().0,http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(store.snapshot().revision,0);
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path,b"not-json").unwrap();
        assert!(ViewStore::open(path,secrets,[None,None]).is_err());
    }

    #[test]
    fn simultaneous_view_writers_cannot_both_replace_the_same_revision() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(SecretStore::new(dir.path().join("secrets.json"),None));
        let store = Arc::new(ViewStore::open(dir.path().join("views.json"),secrets,[None,None]).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2).map(|_| {
            let store = store.clone(); let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.configure(ConfigureViews { expected_revision:0,config:ViewConfig::default() }).map(|s| s.revision).map_err(|e| e.0)
            })
        }).collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| **r == Ok(1)).count(),1);
        assert_eq!(results.iter().filter(|r| **r == Err(http::StatusCode::CONFLICT)).count(),1);
    }

    #[test]
    fn targets_are_explicit_https_origins_without_credentials_or_ambiguity() {
        assert!(parse(&serde_json::json!([gateway("https://admin.example")]).to_string()).is_ok());
        for url in ["http://admin.example", "https://user:pass@admin.example", "https://admin.example/metrics",
            "https://admin.example?token=x", "https://admin.example#fragment"] {
            assert!(parse(&serde_json::json!([gateway(url)]).to_string()).is_err(), "{url}");
        }
        let a = gateway("https://admin.example");
        let mut b = gateway("https://admin.example:443/");
        b["id"] = "eu1-edge".into();
        assert!(parse(&serde_json::json!([a,b]).to_string()).is_err());
        assert!(parse("[]").is_err());
    }

    #[test]
    fn caller_auth_is_explicit_and_cannot_replace_control_plane_credentials() {
        let mut g = gateway("https://admin.example");
        let legacy = parse(&serde_json::json!([g.clone()]).to_string()).unwrap();
        assert!(serde_json::to_value(&legacy).unwrap()[0].get("use_caller_auth").is_none());
        g["use_caller_auth"] = true.into();
        assert!(parse(&serde_json::json!([g.clone()]).to_string()).is_err());
        g.as_object_mut().unwrap().remove("auth");
        let gateways = parse(&serde_json::json!([g.clone()]).to_string()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(SecretStore::new(dir.path().join("secrets"), None));
        let store = ViewStore::open(dir.path().join("views"), secrets, [None,None]).unwrap();
        let config = ViewConfig { gateways:gateways.clone(), control_plane:vec![] };
        assert_eq!(store.configure(ConfigureViews {expected_revision:0,config}).unwrap().revision,1);
        let config = ViewConfig { gateways:vec![], control_plane:gateways };
        assert_eq!(store.configure(ConfigureViews {expected_revision:1,config}).err().unwrap().0,http::StatusCode::BAD_REQUEST);
        assert_eq!(store.snapshot().revision,1);
        g["use_caller_auth"] = false.into();
        assert!(parse(&serde_json::json!([g]).to_string()).is_err());
    }

    #[test]
    fn remote_fields_are_allowlisted() {
        let metrics: GatewayMetrics = serde_json::from_value(serde_json::json!({
            "generated_at":42,"uptime_secs":7,"secret":"never-forward",
            "fleet":{"deployments":9,"ready":3,"draining":2,"pending":1,"total_in_flight":17}
        })).unwrap();
        let encoded = serde_json::to_value(metrics).unwrap();
        assert_eq!(encoded["fleet"]["total_in_flight"],17);
        assert!(encoded.get("secret").is_none());
        assert!(serde_json::from_value::<GatewayMetrics>(serde_json::json!({"fleet":{}})).is_err());
    }

    #[tokio::test]
    async fn observations_keep_success_when_a_peer_redirects_or_lacks_credentials() {
        use axum::{Router, routing::get, response::IntoResponse};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/metrics", get(|headers: http::HeaderMap| async move {
            match headers.get(http::header::AUTHORIZATION).and_then(|h| h.to_str().ok()) {
                Some("Bearer test-observer") => axum::Json(serde_json::json!({
                    "generated_at":42,"uptime_secs":7,
                    "fleet":{"deployments":9,"ready":3,"draining":2,"pending":1,"total_in_flight":17}
                })).into_response(),
                Some("Bearer redirect-observer") => axum::response::Redirect::temporary("/metrics").into_response(),
                _ => http::StatusCode::UNAUTHORIZED.into_response(),
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let secrets = Arc::new(SecretStore::new("unused-fleet-test.json", None));
        secrets.put(crate::secrets::SecretSpec {
            id: "fleet-observer".into(), namespace: "default".into(), description: None,
            data: std::collections::BTreeMap::from([
                ("token".into(), "test-observer".into()),
                ("redirect".into(), "redirect-observer".into()),
            ]), updated_at: 0,
        });
        // HTTP is only used by this loopback transport test. Config parsing
        // independently requires HTTPS for installed gateways.
        let mut gateways: Vec<Gateway> = (0..3).map(|index| {
            let mut value = gateway(&format!("http://{address}"));
            value["id"] = format!("edge-{index}").into();
            value["auth"]["key"] = ["token", "redirect", "missing"][index].into();
            serde_json::from_value(value).unwrap()
        }).collect();
        gateways[1].region = "eu1".into();
        gateways.push(serde_json::from_value(serde_json::json!({
            "id":"signed-in","region":"eu1","url":format!("http://{address}"),"use_caller_auth":true
        })).unwrap());
        let fleet = Fleet { gateways, secrets, client: reqwest::Client::builder()
            .timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap() };
        let observed = fleet.observe(None).await;
        assert_eq!(observed[0].metrics.as_ref().unwrap().fleet.total_in_flight, 17);
        assert_eq!(observed[1].region, "eu1");
        assert!(observed[1].metrics.is_none());
        assert_eq!(observed[1].error, Some("gateway rejected observation"));
        assert!(observed[2].metrics.is_none());
        assert_eq!(observed[2].error, Some("credential unavailable"));
        assert!(!serde_json::to_string(&observed).unwrap().contains("test-observer"));
        assert_eq!(observed[3].error,Some("Heyo sign-in required for regional observations"));
        let signed_in = fleet.observe(Some("test-observer")).await;
        assert_eq!(signed_in[3].metrics.as_ref().unwrap().fleet.total_in_flight,17);
        assert!(signed_in[3].dashboard_url.ends_with("/dashboard?view=local"));
        assert_eq!(signed_in[1].error,Some("gateway rejected observation"));
        assert_eq!(signed_in[2].error,Some("credential unavailable"));
        assert!(!serde_json::to_string(&signed_in).unwrap().contains("test-observer"));
        server.abort();
    }

    #[tokio::test]
    async fn inventory_fails_over_on_unavailable_region_but_not_denied_credentials() {
        use axum::{Router, routing::get, extract::Query};
        use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
        let status = Arc::new(AtomicU16::new(503));
        let flag = status.clone();
        let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_url = format!("http://{}",first.local_addr().unwrap());
        let a = tokio::spawn(async move {
            axum::serve(first, Router::new().route("/orchestration/services", get(move || {
                let flag = flag.clone();
                async move { http::StatusCode::from_u16(flag.load(Ordering::SeqCst)).unwrap() }
            }))).await.unwrap();
        });
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second_url = format!("http://{}",second.local_addr().unwrap());
        let b = tokio::spawn(async move {
            axum::serve(second, Router::new().route("/orchestration/services", get(move |headers: http::HeaderMap,
                Query(query): Query<std::collections::HashMap<String,String>>| {
                let count = count.clone();
                async move {
                    assert_eq!(headers["authorization"],"Bearer shared-credential");
                    assert_eq!(query.get("after").unwrap(),"a&b");
                    count.fetch_add(1,Ordering::SeqCst);
                    axum::Json(serde_json::json!({"services":[{"serviceId":"global-app","desiredReplicas":2,
                        "replicaRegions":["US","eu1"],"discoveryVersion":8,"endpoints":[],"rollout":null}],"nextCursor":null}))
                }
            }))).await.unwrap();
        });
        let secrets = Arc::new(SecretStore::new("unused-inventory-test.json",None));
        secrets.put(crate::secrets::SecretSpec {
            id:"fleet-observer".into(),namespace:"default".into(),description:None,updated_at:0,
            data:std::collections::BTreeMap::from([("token".into(),"shared-credential".into())]),
        });
        let fleet = Fleet { gateways:vec![serde_json::from_value(gateway(&first_url)).unwrap(),
            serde_json::from_value(gateway(&second_url)).unwrap()], secrets,
            client:reqwest::Client::builder().timeout(Duration::from_secs(1)).build().unwrap() };
        let inventory = fleet.inventory(Some("a&b")).await.unwrap();
        assert_eq!(inventory.services[0].service_id,"global-app");
        assert_eq!(hits.load(Ordering::SeqCst),1);
        status.store(401,Ordering::SeqCst);
        assert_eq!(fleet.inventory(Some("a&b")).await.err(),Some("gateway rejected observation"));
        assert_eq!(hits.load(Ordering::SeqCst),1);
        a.abort();
        let _ = a.await;
        // Existing keep-alive connections can outlive the listener task; use a
        // fresh client to exercise an actual refused regional connection.
        let fleet = Fleet { client: reqwest::Client::new(), ..fleet };
        assert_eq!(fleet.inventory(Some("a&b")).await.unwrap().services[0].desired_replicas,Some(2));
        b.abort();
    }
}
