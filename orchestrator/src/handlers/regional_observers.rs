//! Observe every configured ingress before advancing a regional rollout.
use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use heyosecret_client::{HeyoSecretClient, HeyoSecretClientOptions};
use serde::Deserialize;

use super::service_discovery::ServiceDiscoverySnapshot;
use crate::{config::DiscoveryObserver, AppState};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Observation {
    service_id: String,
    #[serde(default)]
    source_url: Option<String>,
    version: Option<u64>,
    upstreams: Vec<Upstream>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Upstream {
    peer: String,
    draining: bool,
    in_flight: u64,
}

fn observer_url(observer: &DiscoveryObserver) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&observer.base_url)?;
    if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty()
        || url.password().is_some() || url.query().is_some() || url.fragment().is_some()
        || observer.deployment_id.is_empty() || observer.token_secret_path.trim().is_empty()
    {
        bail!("invalid discovery observer configuration");
    }
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("observer URL cannot be a base"))?
        .pop_if_empty().extend(["deployments", &observer.deployment_id, "discovery-status"]);
    Ok(url)
}

pub(super) fn topology(state: &AppState, service_id: &str) -> Result<String> {
    if super::host_ingress::enabled(state, service_id) {
        let mut members = state.config.discovery_observers.iter().filter(|o| o.service_id == service_id)
            .map(|o| Ok((o.region.clone(), observer_url(o)?.to_string(), o.ingress_url.clone(), o.discovery_url.clone())))
            .collect::<Result<Vec<_>>>()?;
        members.sort();
        return Ok(serde_json::to_string(&members)?);
    }
    let mut members = state.config.discovery_observers.iter().filter(|o| o.service_id == service_id)
        .map(|o| Ok((o.region.clone(), observer_url(o)?.to_string())))
        .collect::<Result<Vec<_>>>()?;
    members.sort();
    Ok(serde_json::to_string(&members)?)
}

pub(super) async fn validate_regional_observers(
    state: &AppState, service_id: &str, regions: &[String],
) -> Result<()> {
    if !state.config.service_uses_discovery_routing(service_id) {
        bail!("regional rollout requires existing discovery-routed ingress");
    }
    let mut covered = HashSet::new();
    let mut urls = HashSet::new();
    for observer in state.config.discovery_observers.iter().filter(|o| o.service_id == service_id) {
        let url = observer_url(observer)?;
        if !urls.insert(url.to_string()) {
            bail!("one observer cannot stand in for multiple regional ingress instances");
        }
        covered.insert(observer.region.as_str());
    }
    if regions.is_empty() || regions.iter().any(|r| !covered.contains(r.as_str())) {
        bail!("configure every ingress observer, including at least one in each rollout region");
    }
    if state.config.heyosecret_url.is_empty() {
        bail!("regional observer credentials require HeyoSecret");
    }
    Ok(())
}

/// IDs are resolved against the exact versioned snapshot being observed. A
/// missing ID is an error, not evidence that all requests have drained.
pub(super) async fn regional_observe(
    state: &AppState, service_id: &str, snapshot: &ServiceDiscoverySnapshot, withdrawn: &[String],
) -> Result<bool> {
    let peers = withdrawn.iter().map(|id| {
        let endpoint = snapshot.endpoints.iter().find(|e| &e.deployment_id == id)
            .context("withdrawn endpoint disappeared before drain observation")?;
        if !endpoint.draining { bail!("withdrawn endpoint is still eligible for routing"); }
        endpoint_peer(&endpoint.url)
    }).collect::<Result<Vec<_>>>()?;
    let serving = snapshot.endpoints.iter().filter(|e| !e.draining && e.health_status == "healthy")
        .map(|e| endpoint_peer(&e.url)).collect::<Result<HashSet<_>>>()?;
    let observers: Vec<_> = state.config.discovery_observers.iter()
        .filter(|o| o.service_id == service_id).collect();
    if observers.is_empty() { bail!("no ingress observers configured"); }
    let secrets = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url: state.config.heyosecret_url.clone(),
        token: if state.config.heyosecret_internal_api_key.is_empty() {
            state.config.internal_api_key.clone()
        } else { state.config.heyosecret_internal_api_key.clone() },
        timeout: Some(Duration::from_secs(10)),
    })?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none()).build()?;
    let mut ready = true;
    for observer in observers {
        let credential = secrets.read_active(&observer.token_secret_path).await?;
        let token = String::from_utf8(credential.value).context("observer credential must be UTF-8")?;
        if token.trim().is_empty() { bail!("observer credential is empty"); }
        let observation: Observation = client.get(observer_url(observer)?)
            .bearer_auth(token.trim()).send().await?.error_for_status()?.json().await?;
        if let Some(expected) = &observer.discovery_url {
            match observation.source_url.as_ref() {
                None => ready = false, // first poll has not been durably applied
                Some(source) if source != expected => bail!("ingress is not consuming the configured authoritative discovery URL"),
                Some(_) => {}
            }
        }
        ready &= observation_ready(&observation, service_id, snapshot.version, &peers)?;
        // Adoption means the serving set matches too: a manually edited or
        // operator-cordoned pool must not claim successful traffic restoration.
        let accepting: HashSet<_> = observation.upstreams.iter().filter(|u| !u.draining)
            .map(|u| u.peer.clone()).collect();
        ready &= accepting == serving;
    }
    Ok(ready)
}

fn endpoint_peer(value: &str) -> Result<String> {
    super::service_discovery::validate_endpoint_url(value)?;
    let url = reqwest::Url::parse(value)?;
    Ok(format!("{}:{}", url.host_str().context("endpoint host missing")?,
        url.port_or_known_default().context("endpoint port missing")?))
}

fn observation_ready(o: &Observation, service: &str, version: u64, peers: &[String]) -> Result<bool> {
    if o.service_id != service { bail!("observer returned a different service"); }
    Ok(o.version.is_some_and(|v| v >= version) && peers.iter().all(|peer| {
        o.upstreams.iter().filter(|u| &u.peer == peer)
            .all(|u| u.draining && u.in_flight == 0)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_empty_and_wrong_service_observations_are_not_drain_proof() {
        let mut o = Observation { service_id: "ci".into(), source_url: None, version: None, upstreams: vec![] };
        let peers = vec!["eu:8080".into()];
        assert!(!observation_ready(&o, "ci", 9, &peers).unwrap());
        o.version = Some(8);
        assert!(!observation_ready(&o, "ci", 9, &peers).unwrap());
        o.version = Some(9);
        assert!(observation_ready(&o, "ci", 9, &peers).unwrap());
        assert!(observation_ready(&o, "other", 9, &peers).is_err());
        o.upstreams = vec![Upstream { peer: peers[0].clone(), draining: true, in_flight: 1 }];
        assert!(!observation_ready(&o, "ci", 9, &peers).unwrap());
        o.upstreams[0].in_flight = 0;
        o.upstreams[0].draining = false;
        assert!(!observation_ready(&o, "ci", 9, &peers).unwrap());
        o.upstreams[0].draining = true;
        assert!(observation_ready(&o, "ci", 9, &peers).unwrap());
    }

    #[test]
    fn endpoint_identity_matches_app_lb_default_ports_and_ipv6() {
        assert_eq!(endpoint_peer("http://eu:80/").unwrap(), "eu:80");
        assert_eq!(endpoint_peer("http://[::1]:8080").unwrap(), "[::1]:8080");
    }
}
