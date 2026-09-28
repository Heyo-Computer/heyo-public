//! Bootstrap discovery routes on existing host-managed app-lb instances.
//! Never replace a deployment; partial registration is retained for safe retry.
use anyhow::{bail, Context, Result};
use heyosecret_client::{HeyoSecretClient, HeyoSecretClientOptions};
use serde_json::{json, Value};
use std::time::Duration;

use super::{regional_observers, service_deploy::ServiceRouteRequest, service_discovery};
use crate::{config::DiscoveryObserver, AppState};

pub(super) fn enabled(state: &AppState, service: &str) -> bool {
    state.config.discovery_observers.iter().any(|o| o.service_id == service
        && (o.ingress_url.is_some() || o.discovery_url.is_some()))
}

pub(super) fn http_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none()
        || !url.username().is_empty() || url.password().is_some()
        || url.query().is_some() || url.fragment().is_some()
    { bail!("ingress URLs must be credential-free HTTP(S) URLs"); }
    Ok(url)
}

pub(super) fn validate(state: &AppState, service: &str, route: &ServiceRouteRequest) -> Result<()> {
    let mut source = None;
    for observer in state.config.discovery_observers.iter().filter(|o| o.service_id == service) {
        let ingress = http_url(observer.ingress_url.as_deref().context("every host ingress needs ingress_url")?)?;
        if ingress.path() != "/" { bail!("ingress_url must be an origin without a path"); }
        let discovery = http_url(observer.discovery_url.as_deref().context("every host ingress needs discovery_url")?)?;
        if !discovery.path().ends_with(&format!("/orchestration/services/{service}/discovery")) {
            bail!("discovery_url must name this service's authoritative discovery endpoint");
        }
        if source.as_ref().is_some_and(|s| s != &discovery) {
            bail!("all ingress instances must consume one authoritative discovery URL");
        }
        if observer.discovery_token_secret.as_ref().is_some_and(|s| s.is_empty() || s.len() > 64 || s.contains("..")
            || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')) {
            bail!("discovery_token_secret must be an app-lb secret ID");
        }
        source = Some(discovery);
    }
    if source.is_none() { bail!("host ingress requires observers"); }
    if route.host.is_empty() || route.strip_prefix || route.backend_url.is_some()
        || route.entry_points.is_some() || route.cert_resolver.is_some() || route.priority.is_some()
    { bail!("host ingress supports host and preserved pathPrefix, not legacy Traefik routing options"); }
    Ok(())
}

fn spec(observer: &DiscoveryObserver, route: &ServiceRouteRequest) -> Value {
    let mut spec = json!({"id":observer.deployment_id,"discovery":{"service_id":observer.service_id},
        "routes":[{"host":route.host,"path_prefix":route.path_prefix}]});
    if let Some(secret) = &observer.discovery_token_secret {
        spec["discovery"]["source"] = json!({"url":observer.discovery_url,
            "auth":{"secret":secret,"key":"token","namespace":"default"}});
    }
    spec
}

fn matches_spec(actual: &Value, expected: &Value) -> bool {
    actual["id"] == expected["id"] && actual["discovery"] == expected["discovery"]
        && actual["routes"] == expected["routes"]
        && actual["vm"].is_null() && actual["site"].is_null()
        && (expected["discovery"]["regional"].is_null()
            || (actual["namespace"].as_str().unwrap_or("default") == expected["namespace"].as_str().unwrap_or("default")
                && actual["health"] == expected["health"]))
}

async fn ensure_route(client: &reqwest::Client, observer: &DiscoveryObserver, route: &ServiceRouteRequest, token: &str) -> Result<()> {
    ensure_spec(client, observer, &spec(observer, route), token).await
}

pub(super) async fn ensure_spec(client: &reqwest::Client, observer: &DiscoveryObserver, expected: &Value, token: &str) -> Result<()> {
    let mut collection = http_url(&observer.base_url)?;
    collection.path_segments_mut().unwrap().pop_if_empty().push("deployments");
    let mut resource = collection.clone();
    resource.path_segments_mut().unwrap().push(&observer.deployment_id);
    let response = client.get(resource.clone()).bearer_auth(token).send().await?;
    // Namespace-scoped app-lb tokens receive 403 for absent IDs because no
    // namespace can yet be established. Create-only registration is safe in
    // either case: it cannot replace an existing deployment, and both create
    // permission and the authenticated matching read-back remain mandatory.
    if matches!(response.status(), reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN) {
        let capability = client.get(collection.clone()).bearer_auth(token).send().await?.error_for_status()?;
        if capability.headers().get("x-app-lb-create-only").is_none_or(|v| v != "1") {
            bail!("app-lb does not support safe create-only registration; upgrade it first");
        }
        if observer.discovery_token_secret.is_some()
            && capability.headers().get("x-app-lb-discovery-source").is_none_or(|v| v != "1") {
            bail!("app-lb does not support managed discovery sources; upgrade it first");
        }
        if !expected["discovery"]["regional"].is_null()
            && capability.headers().get("x-app-lb-regional-admission").is_none_or(|v| v != "1") {
            bail!("app-lb does not support cold regional admission; upgrade it first");
        }
        let created = client.post(collection).bearer_auth(token)
            .header("If-None-Match", "*").json(expected).send().await?;
        // A concurrent creator is acceptable only if read-back matches exactly.
        if created.status() != reqwest::StatusCode::PRECONDITION_FAILED {
            created.error_for_status()?;
        }
    } else {
        let actual: Value = response.error_for_status()?.json().await?;
        if !matches_spec(&actual["spec"], expected) {
            bail!("existing ingress deployment {} differs; refusing replacement", observer.deployment_id);
        }
    }
    let actual: Value = client.get(resource).bearer_auth(token).send().await?
        .error_for_status()?.json().await?;
    if !matches_spec(&actual["spec"], expected) {
        bail!("ingress registration read-back differs from the requested discovery route");
    }
    Ok(())
}

pub(super) async fn establish(state: &AppState, service: &str, route: &ServiceRouteRequest,
    health_path: &str, timeout_seconds: u64, snapshot: &service_discovery::ServiceDiscoverySnapshot) -> Result<String> {
    validate(state, service, route)?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none()).build()?;
    let secrets = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url: state.config.heyosecret_url.clone(),
        token: if state.config.heyosecret_internal_api_key.is_empty() {
            state.config.internal_api_key.clone()
        } else { state.config.heyosecret_internal_api_key.clone() },
        timeout: Some(Duration::from_secs(10)),
    })?;
    let observers: Vec<_> = state.config.discovery_observers.iter().filter(|o| o.service_id == service).collect();
    let source = observers[0].discovery_url.as_ref().unwrap();
    let authoritative: service_discovery::ServiceDiscoverySnapshot = client.get(source)
        .bearer_auth(&state.config.internal_api_key).send().await?.error_for_status()?.json().await?;
    if serde_json::to_value(&authoritative)? != serde_json::to_value(snapshot)? {
        bail!("configured discovery authority does not match this controller's service state");
    }
    for observer in &observers {
        let credential = secrets.read_active(&observer.token_secret_path).await?;
        let token = String::from_utf8(credential.value).context("ingress credential must be UTF-8")?;
        if token.trim().is_empty() { bail!("ingress credential is empty"); }
        ensure_route(&client, observer, route, token.trim()).await?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    let mut healthy_since = None;
    loop {
        let mut ready = regional_observers::regional_observe(state, service, snapshot, &[]).await?;
        for observer in &observers {
            let prefix = route.path_prefix.as_deref().unwrap_or("/").trim_end_matches('/');
            let url = format!("{}{prefix}/{}", observer.ingress_url.as_ref().unwrap().trim_end_matches('/'), health_path.trim_start_matches('/'));
            ready &= client.get(url).header(reqwest::header::HOST, &route.host)
                .send().await?.status().is_success();
        }
        if ready {
            let since = healthy_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= Duration::from_secs(super::service_deploy::SERVICE_HEALTH_STABILIZATION_SECONDS) {
                return Ok(observers[0].ingress_url.clone().unwrap());
            }
        } else { healthy_since = None; }
        if tokio::time::Instant::now() >= deadline { bail!("host ingress adoption or routed health timed out"); }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::{Path, State}, http::{HeaderMap, StatusCode}, routing::{get, post}, Json, Router};
    use std::sync::{Arc, Mutex, atomic::{AtomicU8, AtomicUsize, Ordering}};

    #[derive(Clone)]
    struct Mock {
        stored: Arc<Mutex<Option<Value>>>,
        source: Arc<Mutex<String>>,
        snapshot: service_discovery::ServiceDiscoverySnapshot,
        mode: Arc<AtomicU8>,
        creates: Arc<AtomicUsize>,
    }

    async fn mock() -> (String, Mock, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Mock {
            stored: Default::default(), source: Default::default(), mode: Default::default(), creates: Default::default(),
            snapshot: serde_json::from_value(json!({"serviceId":"smoke","version":17,"updatedAt":"2026-09-21T00:00:00Z",
                "endpoints":[
                    {"deploymentId":"old-us","region":"us","url":"http://us:8080","healthStatus":"healthy","draining":false},
                    {"deploymentId":"old-eu","region":"eu","url":"http://eu:9090","healthStatus":"healthy","draining":false}
                ]})).unwrap(),
        };
        let router = Router::new()
            .route("/v1/secrets/read", post(|| async { Json(json!({"path":"test/observer","version":1,
                "status":"active","valueBase64":"dGVzdA==","createdAt":"2026-09-21T00:00:00Z","metadata":{}})) }))
            .route("/orchestration/services/smoke/discovery", get(|State(s): State<Mock>| async move { Json(s.snapshot) }))
            .route("/deployments", get(|State(s): State<Mock>| async move {
                ([("x-app-lb-create-only", if s.mode.load(Ordering::SeqCst) == 4 { "0" } else { "1" }),
                    ("x-app-lb-discovery-source", if s.mode.load(Ordering::SeqCst) == 6 { "0" } else { "1" }),
                    ("x-app-lb-regional-admission", if s.mode.load(Ordering::SeqCst) == 10 { "0" } else { "1" })], Json(json!([])))
            }).post(|State(s): State<Mock>, headers: HeaderMap, Json(spec): Json<Value>| async move {
                assert_eq!(headers["if-none-match"], "*");
                assert_eq!(headers["authorization"], "Bearer test");
                s.creates.fetch_add(1, Ordering::SeqCst);
                if s.mode.load(Ordering::SeqCst) == 9 { return StatusCode::FORBIDDEN; }
                let mut stored = s.stored.lock().unwrap();
                if stored.is_some() { return StatusCode::PRECONDITION_FAILED; }
                *stored = Some(spec);
                StatusCode::CREATED
            }))
            .route("/deployments/{id}", get(|State(s): State<Mock>, Path(_id): Path<String>| async move {
                if s.mode.load(Ordering::SeqCst) == 8 {
                    return (StatusCode::FORBIDDEN, Json(json!({})));
                }
                match s.stored.lock().unwrap().clone() {
                    Some(spec) => (StatusCode::OK, Json(json!({"spec":spec}))),
                    None if matches!(s.mode.load(Ordering::SeqCst), 7 | 9) => (StatusCode::FORBIDDEN, Json(json!({}))),
                    None => (StatusCode::NOT_FOUND, Json(json!({}))),
                }
            }))
            .route("/deployments/{id}/discovery-status", get(|State(s): State<Mock>| async move {
                Json(json!({"serviceId":"smoke","version":if s.mode.load(Ordering::SeqCst) == 1 { 16 } else { 17 },
                    "sourceUrl":match s.mode.load(Ordering::SeqCst) {
                        3 => json!("http://wrong-authority"), 5 => Value::Null,
                        _ => json!(s.source.lock().unwrap().clone()),
                    },
                    "upstreams":[{"peer":"us:8080","draining":false,"inFlight":0},{"peer":"eu:9090","draining":false,"inFlight":0}]}))
            }))
            .route("/smoke/health", get(|State(s): State<Mock>, headers: HeaderMap| async move {
                assert_eq!(headers["host"], "smoke.example");
                if s.mode.load(Ordering::SeqCst) == 2 { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK }
            })).with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, state, task)
    }

    #[tokio::test]
    async fn managed_source_registration_is_capability_checked_and_never_replaced() {
        let (base, mock, task) = mock().await;
        let observer: DiscoveryObserver = serde_json::from_value(json!({
            "service_id":"smoke","region":"us","deployment_id":"smoke","base_url":base,
            "ingress_url":base,"discovery_url":format!("{base}/orchestration/services/smoke/discovery"),
            "token_secret_path":"test/observer","discovery_token_secret":"reader"
        })).unwrap();
        let route: ServiceRouteRequest = serde_json::from_value(json!({"host":"smoke.example","pathPrefix":"/smoke","stripPrefix":false})).unwrap();
        let client = reqwest::Client::new();
        mock.mode.store(6, Ordering::SeqCst);
        assert!(ensure_route(&client, &observer, &route, "test").await.unwrap_err().to_string().contains("managed discovery sources"));
        assert_eq!(mock.creates.load(Ordering::SeqCst), 0);
        mock.mode.store(7, Ordering::SeqCst);
        ensure_route(&client, &observer, &route, "test").await.unwrap();
        ensure_route(&client, &observer, &route, "test").await.unwrap();
        assert_eq!(mock.creates.load(Ordering::SeqCst), 1);
        assert_eq!(mock.stored.lock().unwrap().as_ref().unwrap()["discovery"]["source"],
            json!({"url":observer.discovery_url,"auth":{"secret":"reader","key":"token","namespace":"default"}}));
        mock.stored.lock().unwrap().as_mut().unwrap()["discovery"]["source"]["auth"]["secret"] = json!("other");
        assert!(ensure_route(&client, &observer, &route, "test").await.unwrap_err().to_string().contains("refusing replacement"));
        assert_eq!(mock.creates.load(Ordering::SeqCst), 1);

        // A real forbidden read is not permission to replace the resource.
        let protected = mock.stored.lock().unwrap().clone();
        mock.mode.store(8, Ordering::SeqCst);
        assert!(ensure_route(&client, &observer, &route, "test").await.is_err());
        assert_eq!(*mock.stored.lock().unwrap(), protected);
        assert_eq!(mock.creates.load(Ordering::SeqCst), 2);

        // A token unable to create also remains a hard failure.
        *mock.stored.lock().unwrap() = None;
        mock.mode.store(9, Ordering::SeqCst);
        assert!(ensure_route(&client, &observer, &route, "test").await.is_err());
        assert!(mock.stored.lock().unwrap().is_none());
        task.abort();
    }

    #[tokio::test]
    async fn cold_registration_requires_capability_and_preserves_namespace_and_identity() {
        let (base, mock, task) = mock().await;
        let observer: DiscoveryObserver = serde_json::from_value(json!({
            "service_id":"smoke","region":"eu1","deployment_id":"smoke","base_url":base,
            "token_secret_path":"test/observer","discovery_token_secret":"reader"
        })).unwrap();
        let expected = json!({"id":"smoke","namespace":"team-a","routes":[{"host":"smoke.example"}],
            "discovery":{"service_id":"smoke","regional":{"gateway_id":"eu","backend_server_id":"host-eu"}}});
        let client = reqwest::Client::new();
        mock.mode.store(10,Ordering::SeqCst);
        assert!(ensure_spec(&client,&observer,&expected,"test").await.unwrap_err().to_string().contains("cold regional admission"));
        assert_eq!(mock.creates.load(Ordering::SeqCst),0);
        mock.mode.store(0,Ordering::SeqCst);
        ensure_spec(&client,&observer,&expected,"test").await.unwrap();
        ensure_spec(&client,&observer,&expected,"test").await.unwrap();
        for field in ["namespace","discovery"] {
            let mut changed = expected.clone();
            changed[field] = json!("different");
            assert!(ensure_spec(&client,&observer,&changed,"test").await.is_err());
        }
        assert_eq!(*mock.stored.lock().unwrap(),Some(expected));
        assert_eq!(mock.creates.load(Ordering::SeqCst),1);
        task.abort();
    }

    #[tokio::test]
    async fn bootstrap_two_ingresses_retries_without_replacement_and_requires_both_gates() {
        let (us, us_mock, us_task) = mock().await;
        let (eu, eu_mock, eu_task) = mock().await;
        let source = format!("{us}/orchestration/services/smoke/discovery");
        *us_mock.source.lock().unwrap() = source.clone();
        *eu_mock.source.lock().unwrap() = source.clone();
        let config: crate::config::Config = serde_json::from_value(json!({
            "server_port":0,"database_url":"unused","agent_provider":"test","agent_model":"test","agent_api_key":"",
            "agent_timeout_seconds":1,"agent_max_iterations":1,"jwt_secret":"test","cloud_internal_url":us,
            "internal_api_key":"test","heyosecret_url":us,"discovery_routed_services":"smoke",
            "discovery_observers":[
                {"service_id":"smoke","region":"us","deployment_id":"smoke","base_url":us,"ingress_url":us,"discovery_url":source,"token_secret_path":"test/observer"},
                {"service_id":"smoke","region":"eu","deployment_id":"smoke","base_url":eu,"ingress_url":eu,"discovery_url":source,"token_secret_path":"test/observer"}
            ]})).unwrap();
        let mut state = AppState { config: Arc::new(config), http_client: reqwest::Client::new(),
            worker_id: Arc::new("test".into()), ci_workspace_cache: Default::default() };
        let route: ServiceRouteRequest = serde_json::from_value(json!({"host":"smoke.example","pathPrefix":"/smoke","stripPrefix":false})).unwrap();
        let snapshot = &us_mock.snapshot;
        assert_eq!(establish(&state, "smoke", &route, "/health", 20, snapshot).await.unwrap(), us);
        assert!(establish(&state, "smoke", &route, "/health", 20, snapshot).await.is_ok());
        assert_eq!(us_mock.creates.load(Ordering::SeqCst), 1);
        assert_eq!(eu_mock.creates.load(Ordering::SeqCst), 1);
        // A single stale, unhealthy, or wrongly sourced EU observer must block
        // even though the US observer remains healthy throughout.
        for mode in [1, 2, 3, 5] {
            eu_mock.mode.store(mode, Ordering::SeqCst);
            assert!(establish(&state, "smoke", &route, "/health", 1, snapshot).await.is_err());
        }
        eu_mock.mode.store(0, Ordering::SeqCst);
        let mut other = snapshot.clone();
        other.version = 18;
        assert!(establish(&state, "smoke", &route, "/health", 1, &other).await.unwrap_err().to_string().contains("controller's service state"));
        eu_mock.stored.lock().unwrap().as_mut().unwrap()["discovery"]["service_id"] = json!("production");
        assert!(establish(&state, "smoke", &route, "/health", 1, snapshot).await.unwrap_err().to_string().contains("refusing replacement"));
        assert_eq!(eu_mock.creates.load(Ordering::SeqCst), 1);
        *eu_mock.stored.lock().unwrap() = None;
        eu_mock.mode.store(4, Ordering::SeqCst);
        assert!(establish(&state, "smoke", &route, "/health", 1, snapshot).await.unwrap_err().to_string().contains("upgrade it first"));
        assert_eq!(eu_mock.creates.load(Ordering::SeqCst), 1);
        let pinned = regional_observers::topology(&state, "smoke").unwrap();
        Arc::make_mut(&mut state.config).discovery_observers[1].discovery_url = Some(format!("{eu}/orchestration/services/smoke/discovery"));
        assert!(validate(&state, "smoke", &route).is_err());
        assert_ne!(pinned, regional_observers::topology(&state, "smoke").unwrap());
        us_task.abort();
        eu_task.abort();
    }
}
