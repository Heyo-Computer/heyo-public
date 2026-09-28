//! Explicit one-hop gateway transport. Regional selection is a separate concern.
use crate::secrets::{SecretRef, SecretStore};
use http::HeaderMap;
use serde::{Deserialize, Serialize};

const TOKEN: &str = "x-heyo-peer-token";
const SERVICE: &str = "x-heyo-peer-service";
const REGION: &str = "x-heyo-peer-region";
const NAMESPACE: &str = "x-heyo-peer-namespace";
pub(crate) const HEADERS: [&str; 4] = [TOKEN, SERVICE, REGION, NAMESPACE];

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GatewaySpec {
    pub service: String,
    /// Destination region for forward mode; this instance's region for local mode.
    pub region: String,
    pub auth: SecretRef,
    pub mode: GatewayMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GatewayMode { Forward, Local }

impl GatewaySpec {
    pub fn validate(&self, upstreams: &[String]) -> Result<(), String> {
        for value in [&self.service, &self.region] {
            if value.is_empty() || value.len() > 128 || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
                return Err("gateway service and region must be bounded identifiers".into());
            }
        }
        self.auth.validate().map_err(|e| e.to_string())?;
        if upstreams.is_empty() { return Err("gateway transport requires explicit upstreams".into()); }
        for upstream in upstreams {
            let target = crate::config::StaticUpstream::parse(upstream).ok_or("invalid gateway upstream")?;
            match self.mode {
                GatewayMode::Forward if !target.tls => return Err("gateway forwarding requires HTTPS upstreams".into()),
                GatewayMode::Local => {
                    let addr: std::net::SocketAddr = target.address.parse().map_err(|_| "local gateway requires loopback host-port upstreams")?;
                    if target.tls || !addr.ip().is_loopback() {
                        return Err("local gateway requires plaintext loopback host-port upstreams".into());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Validate before removing peer metadata. Never change application Authorization.
/// Returns the outgoing credential only for an explicitly forwarding deployment.
pub fn admit(spec: Option<&GatewaySpec>, headers: &HeaderMap, secrets: &SecretStore) -> Result<Option<String>, u16> {
    let has_peer = HEADERS.iter().any(|name| headers.contains_key(*name));
    let result = (|| {
        let Some(spec) = spec else { return if has_peer { Err(403) } else { Ok(None) }; };
        if has_peer && spec.mode == GatewayMode::Forward { return Err(508); }
        if !has_peer && spec.mode == GatewayMode::Local { return Err(403); }
        let token = secrets.resolve(&spec.auth).map_err(|_| 503u16)?;
        if token.is_empty() || http::HeaderValue::from_str(&token).is_err() { return Err(503); }
        if spec.mode == GatewayMode::Forward { return Ok(Some(token)); }
        for (name, expected) in [(TOKEN, token.as_str()), (SERVICE, spec.service.as_str()), (REGION, spec.region.as_str()), (NAMESPACE, spec.auth.namespace())] {
            if headers.get_all(name).iter().count() != 1 { return Err(403); }
            let actual = headers.get(name).ok_or(403u16)?.as_bytes();
            if actual.len() != expected.len() || !openssl::memcmp::eq(actual, expected.as_bytes()) { return Err(403); }
        }
        Ok(None)
    })();
    result
}

pub fn write_forward_headers(headers: &mut HeaderMap, spec: &GatewaySpec, token: &str) -> Result<(), http::header::InvalidHeaderValue> {
    for name in HEADERS { headers.remove(name); }
    for (name, value) in [(TOKEN, token), (SERVICE, spec.service.as_str()), (REGION, spec.region.as_str()), (NAMESPACE, spec.auth.namespace())] {
        headers.insert(name, value.parse()?);
    }
    Ok(())
}

/// Probe the authenticated peer route; an authentication failure is not healthy.
pub async fn probe(spec: &GatewaySpec, upstream: &str, host: &str, check: &crate::config::HealthCheck, secrets: &SecretStore) -> bool {
    let mut headers = HeaderMap::new();
    let Ok(Some(token)) = admit(Some(spec), &mut headers, secrets) else { return false; };
    if write_forward_headers(&mut headers, spec, &token).is_err() { return false; }
    let Ok(host) = host.parse() else { return false; };
    headers.insert(http::header::HOST, host);
    let Ok(mut url) = reqwest::Url::parse(upstream) else { return false; };
    if let Some(port) = check.port { if url.set_port(Some(port)).is_err() { return false; } }
    url.set_path(check.path.as_deref().unwrap_or("/"));
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(check.timeout_secs.max(1)))
        .redirect(reqwest::redirect::Policy::none()).build() else { return false; };
    let Ok(response) = client.get(url).headers(headers).send().await else { return false; };
    response.status().is_success() && check.expected_header.as_ref().is_none_or(|expected| {
        let values: Vec<_> = response.headers().get_all(&expected.name).iter().collect();
        values.len() == 1 && values[0].as_bytes() == expected.value.as_bytes()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup(mode: &str) -> (GatewaySpec, SecretStore) {
        let spec = serde_json::from_value(json!({"service":"smoke","region":"eu1","mode":mode,"auth":{"secret":"peer"}})).unwrap();
        let store = SecretStore::new("unused-gateway-test-state", None);
        store.put(serde_json::from_value(json!({"id":"peer","data":{"token":"test-peer-value"}})).unwrap());
        (spec, store)
    }

    #[test]
    fn gateway_round_trip_preserves_application_headers() {
        let (source, store) = setup("forward");
        let (destination, _) = setup("local");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer application-token".parse().unwrap());
        headers.insert("host", "smoke.example".parse().unwrap());
        let token = admit(Some(&source), &mut headers, &store).unwrap().unwrap();
        write_forward_headers(&mut headers, &source, &token).unwrap();
        assert_eq!(admit(Some(&destination), &mut headers, &store), Ok(None));
        assert_eq!(headers["authorization"], "Bearer application-token");
        assert_eq!(headers["host"], "smoke.example");
    }

    #[test]
    fn gateway_rejects_wrong_scope_duplicates_missing_auth_and_second_hop() {
        let (local, store) = setup("local");
        let (forward, _) = setup("forward");
        assert_eq!(admit(Some(&local), &mut HeaderMap::new(), &store), Err(403));
        for (name, value) in [(TOKEN,"wrong"), (REGION,"us3"), (SERVICE,"other-app"), (NAMESPACE,"other-team")] {
            let mut headers = HeaderMap::new();
            write_forward_headers(&mut headers, &local, "test-peer-value").unwrap();
            headers.insert(name, value.parse().unwrap());
            assert_eq!(admit(Some(&local), &mut headers, &store), Err(403));
        }
        let mut headers = HeaderMap::new();
        write_forward_headers(&mut headers, &local, "test-peer-value").unwrap();
        headers.append(REGION, "eu1".parse().unwrap());
        assert_eq!(admit(Some(&local), &mut headers, &store), Err(403));
        write_forward_headers(&mut headers, &local, "test-peer-value").unwrap();
        assert_eq!(admit(Some(&forward), &mut headers, &store), Err(508));
        write_forward_headers(&mut headers, &local, "test-peer-value").unwrap();
        assert_eq!(admit(None, &mut headers, &store), Err(403));
    }

    #[test]
    fn gateway_validates_transport_and_scopes_secret_to_deployment_namespace() {
        let (local, _) = setup("local");
        let (forward, _) = setup("forward");
        assert!(local.validate(&["127.0.0.1:2227".into()]).is_ok());
        assert!(local.validate(&["135.181.222.73:2227".into()]).is_err());
        assert!(forward.validate(&["http://peer.example:80".into()]).is_err());
        assert!(forward.validate(&["https://peer.example:443".into()]).is_ok());
        let mut spec: crate::config::DeploymentSpec = serde_json::from_value(json!({
            "id":"smoke", "namespace":"team-a", "routes":[{"host":"smoke.example"}],
            "upstreams":["https://peer.example:443"], "gateway": {
                "mode":"forward","service":"smoke","region":"eu1", "auth":{"secret":"peer","namespace":"team-b"}
            }
        })).unwrap();
        spec.normalize();
        spec.validate().unwrap();
        assert_eq!(spec.gateway.as_ref().unwrap().auth.namespace(), "team-a");
        assert!(spec.secret_ids().contains(&"peer".to_string()));
        let (_, store) = setup("forward");
        assert_eq!(admit(spec.gateway.as_ref(), &mut HeaderMap::new(), &store), Err(503));
    }
}
