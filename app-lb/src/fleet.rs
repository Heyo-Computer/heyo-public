//! Read-only, explicitly configured gateway observations. Never a placement or
//! rollout authority, and never a proxy for arbitrary browser-supplied URLs.
use crate::secrets::{SecretRef, SecretStore};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Gateway {
    id: String,
    region: String,
    url: String,
    auth: SecretRef,
}

pub struct Fleet {
    gateways: Vec<Gateway>,
    client: reqwest::Client,
    secrets: Arc<SecretStore>,
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
        gateway.auth.validate().map_err(|_| "invalid fleet secret reference")?;
    }
    Ok(gateways)
}

impl Fleet {
    pub fn from_env(variable: &str, secrets: Arc<SecretStore>) -> Result<Option<Arc<Self>>, String> {
        let Some(path) = std::env::var_os(variable) else { return Ok(None) };
        let text = std::fs::read_to_string(path).map_err(|_| format!("cannot read {variable}"))?;
        let gateways = parse(&text)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build().map_err(|_| "cannot build fleet HTTP client")?;
        Ok(Some(Arc::new(Self { gateways, client, secrets })))
    }

    async fn fetch<T: serde::de::DeserializeOwned>(&self, gateway: &Gateway, path: &str) -> Result<T, &'static str> {
        let credential = self.secrets.resolve(&gateway.auth).map_err(|_| "credential unavailable")?;
        if credential.trim().is_empty() { return Err("credential unavailable") }
        let request = self.client.get(format!("{}{path}", gateway.url.trim_end_matches('/')));
        let request = match &gateway.auth.username {
            Some(user) => request.basic_auth(user, Some(credential)),
            None => request.bearer_auth(credential),
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
            match self.fetch(gateway, &path).await {
                Ok(inventory) => return Ok(inventory),
                Err(error @ ("gateway unreachable" | "gateway unavailable" | "gateway response interrupted")) => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    pub async fn observe(&self) -> Vec<Observation> {
        futures::future::join_all(self.gateways.iter().map(|gateway| async move {
            let result = self.fetch(gateway, "/metrics?summary=true&limit=0").await;
            Observation {
                id: gateway.id.clone(), region: gateway.region.clone(),
                dashboard_url: format!("{}/dashboard", gateway.url.trim_end_matches('/')),
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
        let fleet = Fleet { gateways, secrets, client: reqwest::Client::builder()
            .timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap() };
        let observed = fleet.observe().await;
        assert_eq!(observed[0].metrics.as_ref().unwrap().fleet.total_in_flight, 17);
        assert_eq!(observed[1].region, "eu1");
        assert!(observed[1].metrics.is_none());
        assert_eq!(observed[1].error, Some("gateway rejected observation"));
        assert!(observed[2].metrics.is_none());
        assert_eq!(observed[2].error, Some("credential unavailable"));
        assert!(!serde_json::to_string(&observed).unwrap().contains("test-observer"));
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
