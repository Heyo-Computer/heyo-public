//! Hierarchical routing with generation-pinned request ownership. Nothing here
//! provisions VMs. Admission, snapshot replacement and reports share one lock.
use crate::{deployment::VmBackend, gateway::{GatewayMode, GatewaySpec}, secrets::{SecretRef, SecretStore}};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, HashSet}, sync::{Arc, Mutex}};

pub const GENERATION: &str = "x-heyo-peer-generation";
pub const ENVIRONMENT: &str = "x-heyo-peer-environment";
pub const PROBE: &str = "x-heyo-peer-probe";
pub const ACTIVE_PROBE: &str = "x-heyo-peer-active-probe";

/// Correlation belongs to the executing workflow, not the latest proposal.
/// Authorization permits only an already-eligible member under the active policy.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveProbeRequest {
    pub operation_id: String, pub step_id: String, pub epoch: i64, pub challenge: String,
    pub generation: i64, pub version: u64, pub region: String, pub gateway_id: String,
    pub gateway_boot_id: String, pub backend_server_id: String, pub deployment_id: String, pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveProbeReceipt {
    pub request: ActiveProbeRequest, pub gateway_id: String, pub gateway_boot_id: String,
    pub backend_server_id: String, pub backend_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbeRequest {
    pub operation_id: String,
    pub generation: i64,
    pub region: String,
    pub gateway_id: String,
    pub gateway_boot_id: String,
    pub deployment_id: String,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RegionalSpec {
    pub gateway_id: String,
    pub backend_server_id: String,
    pub environment: String,
    pub auth: SecretRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Policy { pub version: u32, pub regions: Vec<Region> }
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Region { pub region: String, pub weight: u32, pub gateways: Vec<Gateway> }
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Gateway { pub id: String, pub backend_server_id: String, pub url: String }
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Generation { pub generation: i64, pub policy: Policy }
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub deployment_id: String, pub backend_server_id: String, pub region: String,
    pub revision: Option<String>,
    pub url: String, pub health_status: String, pub draining: bool,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Snapshot {
    pub protocol_version: u32, pub service_id: String, pub environment: String,
    pub region: String, pub gateway_id: String, pub boot_id: String, pub version: u64,
    pub operation_id: String, pub phase: String, pub proposal_generation: i64,
    pub active_generation: Option<i64>, pub drain_target: Option<String>,
    pub closed_through_generation: i64, pub policies: Vec<Generation>, pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Default)]
struct State {
    snapshot: Option<Snapshot>,
    local: Vec<Arc<VmBackend>>,
    counters: BTreeMap<(i64, String), (u64, u64)>,
    sequence: u64,
    fenced: bool,
}

#[derive(Debug)]
pub struct Router { pub boot_id: String, state: Mutex<State> }

impl Router {
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        openssl::rand::rand_bytes(&mut bytes).expect("OS randomness available for gateway boot identity");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let boot_id = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
        Self { boot_id, state: Mutex::new(State::default()) }
    }

    pub fn fence(&self) { self.state.lock().unwrap().fenced = true; }

    pub fn apply(&self, snapshot: Snapshot, spec: &RegionalSpec, service: &str, region: &str,
        local: Vec<Arc<VmBackend>>) -> Result<bool, String> {
        if snapshot.protocol_version != 1 || snapshot.service_id != service || snapshot.region != region
            || snapshot.gateway_id != spec.gateway_id || snapshot.environment != spec.environment || snapshot.boot_id != self.boot_id {
            return Err("regional snapshot identity/protocol mismatch".into());
        }
        let mut generations = HashSet::new();
        for generation in &snapshot.policies {
            if generation.generation <= 0 || !generations.insert(generation.generation) || generation.policy.version != 1 {
                return Err("invalid policy generation/schema".into());
            }
            let mut regions = HashSet::new();
            let mut gateways = HashSet::new();
            let mut addresses = HashSet::new();
            let mut total = 0u64;
            for r in &generation.policy.regions {
                if r.region.is_empty() || !regions.insert(&r.region) || r.gateways.is_empty() { return Err("invalid regional inventory".into()); }
                total += u64::from(r.weight);
                for g in &r.gateways {
                    let url = reqwest::Url::parse(&g.url).map_err(|e| e.to_string())?;
                    if g.id.is_empty() || g.backend_server_id.is_empty() || !gateways.insert(&g.id)
                        || !addresses.insert(url.to_string()) || url.scheme() != "https" || url.domain().is_none()
                        || !url.username().is_empty() || url.password().is_some() || url.path() != "/"
                        || url.query().is_some() || url.fragment().is_some() {
                        return Err("invalid regional gateway transport".into());
                    }
                }
            }
            if total == 0 { return Err("regional policy has no positive weights".into()); }
        }
        if !generations.contains(&snapshot.proposal_generation) || snapshot.active_generation.is_some_and(|g| !generations.contains(&g))
            || snapshot.closed_through_generation < 0 || snapshot.closed_through_generation > snapshot.proposal_generation {
            return Err("regional policy references unavailable generations".into());
        }
        if !snapshot.policies.iter().find(|p| p.generation == snapshot.proposal_generation)
            .and_then(|p| p.policy.regions.iter().find(|r| r.region == region))
            .is_some_and(|r| r.gateways.iter().any(|g| g.id == spec.gateway_id && g.backend_server_id == spec.backend_server_id)) {
            return Err("regional policy does not match the configured host binding".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.fenced { return Err("gateway runtime was fenced by configuration change".into()); }
        if let Some(old) = &state.snapshot {
            if snapshot.version < old.version { return Ok(false); }
            if snapshot.active_generation < old.active_generation || snapshot.closed_through_generation < old.closed_through_generation {
                return Err("regional policy/fence regressed".into());
            }
            for previous in &old.policies {
                if !snapshot.policies.iter().any(|p| p.generation == previous.generation && p.policy == previous.policy) {
                    return Err("immutable regional history changed or disappeared".into());
                }
            }
        }
        state.local = local;
        state.snapshot = Some(snapshot);
        Ok(true)
    }

    /// Select the region and reserve its source/destination accounting before
    /// any DNS/connection await. A peer request can only select a local backend.
    pub fn admit(self: &Arc<Self>, spec: &RegionalSpec, service: &str, region: &str,
        headers: &http::HeaderMap, secrets: &SecretStore) -> Result<Assignment, u16> {
        if headers.contains_key(PROBE) || headers.contains_key(ACTIVE_PROBE) { return Err(403); }
        let has_peer = crate::gateway::HEADERS.iter().chain([GENERATION, ENVIRONMENT].iter()).any(|h| headers.contains_key(*h));
        let mut state = self.state.lock().unwrap();
        let snapshot = state.snapshot.as_ref().filter(|_| !state.fenced).ok_or(503u16)?;
        let generation = if has_peer {
            let local = GatewaySpec { service: service.into(), region: region.into(), auth: spec.auth.clone(), mode: GatewayMode::Local };
            crate::gateway::admit(Some(&local), headers, secrets)?;
            if headers.get_all(ENVIRONMENT).iter().count() != 1 || headers.get(ENVIRONMENT).and_then(|h| h.to_str().ok()) != Some(spec.environment.as_str())
                || headers.get_all(GENERATION).iter().count() != 1 { return Err(403); }
            let generation = headers.get(GENERATION).and_then(|h| h.to_str().ok()).and_then(|h| h.parse::<i64>().ok()).ok_or(403u16)?;
            if generation <= snapshot.closed_through_generation { return Err(503); }
            generation
        } else { snapshot.active_generation.ok_or(503u16)? };
        let policy = &snapshot.policies.iter().find(|p| p.generation == generation).ok_or(503u16)?.policy;
        let destination = if has_peer {
            policy.regions.iter().find(|r| r.region == region && r.weight > 0).ok_or(503u16)?
        } else {
            let total: u64 = policy.regions.iter().map(|r| u64::from(r.weight)).sum();
            let mut random = [0u8; 8];
            openssl::rand::rand_bytes(&mut random).map_err(|_| 503u16)?;
            let mut ticket = u64::from_le_bytes(random) % total;
            policy.regions.iter().find(|r| { if ticket < u64::from(r.weight) { true } else { ticket -= u64::from(r.weight); false } }).ok_or(503u16)?
        };
        let destination = destination.clone();
        let is_local = destination.region == region;
        if is_local && generation <= snapshot.closed_through_generation { return Err(503); }
        let backend = if is_local {
            state.local.iter().filter(|b| b.is_healthy() && !b.is_draining()).min_by_key(|b| b.in_flight()).cloned().ok_or(503u16)?
        } else {
            Arc::new(VmBackend::for_upstream(destination.gateways.first().ok_or(503u16)?.url.clone()))
        };
        let forward = if !is_local {
            let forward = GatewaySpec { service: service.into(), region: destination.region.clone(), auth: spec.auth.clone(), mode: GatewayMode::Forward };
            let token = crate::gateway::admit(Some(&forward), headers, secrets)?.ok_or(503u16)?;
            Some((forward, token, spec.environment.clone()))
        } else { None };
        let counters = state.counters.entry((generation, destination.region.clone())).or_default();
        counters.0 += u64::from(!has_peer);
        counters.1 += u64::from(is_local);
        Ok(Assignment { router: self.clone(), generation, region: destination.region, source: !has_peer, local: is_local, backend, forward })
    }

    fn active_probe_backend(&self, state: &State, spec: &RegionalSpec, request: &ActiveProbeRequest,
        destination: bool) -> Result<(Arc<VmBackend>,Option<String>),u16> {
        if request.epoch <= 0 || request.challenge.len() != 64 || !request.challenge.bytes().all(|b| b.is_ascii_hexdigit())
            || [&request.operation_id,&request.step_id,&request.region,&request.gateway_id,&request.gateway_boot_id,
                &request.backend_server_id,&request.deployment_id,&request.revision].iter().any(|s| s.is_empty() || s.len() > 256) {return Err(400);}
        let snapshot = state.snapshot.as_ref().filter(|_| !state.fenced).ok_or(503u16)?;
        if snapshot.active_generation != Some(request.generation) || snapshot.version != request.version {return Err(409);}
        let policy = &snapshot.policies.iter().find(|p| p.generation == request.generation).ok_or(409u16)?.policy;
        if !policy.regions.iter().any(|r| r.region == snapshot.region && r.gateways.iter().any(|g|
            g.id == spec.gateway_id && g.backend_server_id == spec.backend_server_id)) {return Err(409);}
        let target = policy.regions.iter().find(|r| r.region == request.region && r.weight > 0).ok_or(409u16)?;
        let gateway = target.gateways.iter().find(|g| g.id == request.gateway_id && g.backend_server_id == request.backend_server_id).ok_or(409u16)?;
        if !destination {return Ok((Arc::new(VmBackend::for_upstream(gateway.url.clone())),None));}
        if spec.gateway_id != request.gateway_id || self.boot_id != request.gateway_boot_id
            || spec.backend_server_id != request.backend_server_id || snapshot.region != request.region
            || request.generation <= snapshot.closed_through_generation {return Err(409);}
        let members: Vec<_> = snapshot.endpoints.iter().filter(|e| e.deployment_id == request.deployment_id).collect();
        let member = members.first().filter(|_| members.len() == 1).ok_or(409u16)?;
        if member.region != request.region || member.backend_server_id != request.backend_server_id
            || member.revision.as_deref() != Some(&request.revision) || member.health_status != "healthy" || member.draining {return Err(409);}
        let peer = crate::discovery::upstream_from_url(&member.url).map_err(|_| 409u16)?;
        let address: std::net::SocketAddr = peer.parse().map_err(|_| 409u16)?;
        if !address.ip().is_loopback() {return Err(409);}
        let backend = state.local.iter().find(|b| b.peer == peer && b.is_healthy() && !b.is_draining()).cloned().ok_or(409u16)?;
        Ok((backend,Some(member.url.clone())))
    }

    fn active_probe_assignment(self: &Arc<Self>, spec: &RegionalSpec, request: &ActiveProbeRequest,
        destination: bool) -> Result<Assignment,u16> {
        let mut state = self.state.lock().unwrap();
        let (backend,_) = self.active_probe_backend(&state,spec,request,destination)?;
        let counters = state.counters.entry((request.generation,request.region.clone())).or_default();
        counters.0 += u64::from(!destination); counters.1 += u64::from(destination);
        Ok(Assignment {router:self.clone(),generation:request.generation,region:request.region.clone(),
            source:!destination,local:destination,backend,forward:None})
    }

    pub async fn active_probe_remote(self: &Arc<Self>, spec: &RegionalSpec, service: &str, request: &ActiveProbeRequest,
        host: &str, health: &crate::config::HealthCheck, secrets: &SecretStore) -> Result<ActiveProbeReceipt,u16> {
        let assignment = self.active_probe_assignment(spec,request,false)?;
        let forward = GatewaySpec {service:service.into(),region:request.region.clone(),auth:spec.auth.clone(),mode:GatewayMode::Forward};
        let token = crate::gateway::admit(Some(&forward),&http::HeaderMap::new(),secrets)?.ok_or(503u16)?;
        let mut headers = http::HeaderMap::new();
        crate::gateway::write_forward_headers(&mut headers,&forward,&token).map_err(|_| 503u16)?;
        for (name,value) in [(ENVIRONMENT,spec.environment.clone()),(GENERATION,request.generation.to_string()),
            (ACTIVE_PROBE,serde_json::to_string(request).map_err(|_| 400u16)?)] {
            headers.insert(name,http::HeaderValue::from_str(&value).map_err(|_| 400u16)?);
        }
        headers.insert(http::header::HOST,host.parse().map_err(|_| 400u16)?);
        let mut url = reqwest::Url::parse(&assignment.backend.peer).map_err(|_| 409u16)?;
        url.set_path(health.path.as_deref().ok_or(409u16)?);
        let response = probe_http(health)?.get(url).headers(headers).header(http::header::CACHE_CONTROL,"no-cache, no-store")
            .send().await.map_err(|_| 502u16)?;
        if !response.status().is_success() || response.headers().get("age").is_some_and(|v| v != "0") {return Err(502);}
        let receipt: ActiveProbeReceipt = serde_json::from_slice(&probe_body(response).await?).map_err(|_| 502u16)?;
        if &receipt.request != request || receipt.gateway_id != request.gateway_id
            || receipt.gateway_boot_id != request.gateway_boot_id || receipt.backend_server_id != request.backend_server_id {return Err(502);}
        self.active_probe_backend(&self.state.lock().unwrap(),spec,request,false)?;
        Ok(receipt)
    }

    pub async fn active_probe_local(self: &Arc<Self>, spec: &RegionalSpec, service: &str, region: &str,
        headers: &http::HeaderMap, method: &http::Method, uri: &http::Uri, host: &str,
        health: &crate::config::HealthCheck, secrets: &SecretStore) -> Result<ActiveProbeReceipt,u16> {
        let local = GatewaySpec {service:service.into(),region:region.into(),auth:spec.auth.clone(),mode:GatewayMode::Local};
        crate::gateway::admit(Some(&local),headers,secrets)?;
        if method != http::Method::GET || uri.query().is_some() || Some(uri.path()) != health.path.as_deref()
            || headers.contains_key(PROBE) || headers.get_all(ACTIVE_PROBE).iter().count() != 1
            || headers.get_all(ENVIRONMENT).iter().count() != 1
            || headers.get(ENVIRONMENT).and_then(|v| v.to_str().ok()) != Some(&spec.environment)
            || headers.get_all(GENERATION).iter().count() != 1 {return Err(403);}
        let request: ActiveProbeRequest = serde_json::from_slice(headers.get(ACTIVE_PROBE).ok_or(403u16)?.as_bytes()).map_err(|_| 403u16)?;
        if headers.get(GENERATION).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok()) != Some(request.generation) {return Err(403);}
        let assignment = self.active_probe_assignment(spec,&request,true)?;
        let _backend_slot = assignment.backend.try_hold().ok_or(409u16)?;
        let mut url = reqwest::Url::parse(&format!("http://{}",assignment.backend.address)).map_err(|_| 409u16)?;
        url.set_path(health.path.as_deref().ok_or(409u16)?);
        if health.port.is_some_and(|port| Some(port) != url.port_or_known_default()) {return Err(409);}
        let response = probe_http(health)?.get(url).header(http::header::HOST,host).header(http::header::CACHE_CONTROL,"no-cache, no-store")
            .send().await.map_err(|_| 502u16)?;
        if !response.status().is_success() || response.headers().get("age").is_some_and(|v| v != "0")
            || response.headers().get_all("x-heyo-revision").iter().count() != 1
            || response.headers().get("x-heyo-revision").and_then(|v| v.to_str().ok()) != Some(&request.revision)
            || health.expected_header.as_ref().is_some_and(|expected| response.headers().get_all(&expected.name).iter().count() != 1
                || response.headers().get(&expected.name).is_none_or(|v| v.as_bytes() != expected.value.as_bytes())) {return Err(502);}
        probe_body(response).await?;
        let (_,url) = self.active_probe_backend(&self.state.lock().unwrap(),spec,&request,true)?;
        Ok(ActiveProbeReceipt {request,gateway_id:spec.gateway_id.clone(),gateway_boot_id:self.boot_id.clone(),
            backend_server_id:spec.backend_server_id.clone(),backend_url:url.ok_or(409u16)?})
    }

    /// Readiness work is a separate, exact-target reservation, never a public
    /// selector override. Excluded endpoints require a completed withdrawal;
    /// serving checks require eligible membership in a later open generation.
    fn probe_assignment(self: &Arc<Self>, spec: &RegionalSpec, request: &ProbeRequest, destination: bool) -> Result<Assignment, u16> {
        if [&request.operation_id,&request.region,&request.gateway_id,&request.gateway_boot_id,&request.deployment_id,&request.revision]
            .iter().any(|s| s.is_empty() || s.len() > 256) { return Err(400); }
        let mut state = self.state.lock().unwrap();
        let snapshot = state.snapshot.as_ref().filter(|_| !state.fenced).ok_or(503u16)?;
        let serving = matches!(snapshot.phase.as_str(), "bake" | "verify" | "verify_baseline" | "rolled_back")
            || (snapshot.phase == "passed" && snapshot.drain_target.is_none());
        if snapshot.operation_id != request.operation_id
            || (!serving && !matches!(snapshot.phase.as_str(), "passed" | "probe_candidates" | "probe_retained"))
            || snapshot.active_generation != Some(request.generation) || snapshot.proposal_generation != request.generation
            || (if serving { snapshot.drain_target.is_some() } else { snapshot.drain_target.as_deref() != Some(&request.region) }) { return Err(409); }
        let target = snapshot.policies.iter().find(|p| p.generation == request.generation)
            .and_then(|p| p.policy.regions.iter().find(|r| r.region == request.region
                && (if serving { r.weight > 0 } else { r.weight == 0 }))).ok_or(409u16)?;
        let gateway = target.gateways.iter().find(|g| g.id == request.gateway_id).ok_or(409u16)?;
        let backend = if destination {
            if spec.gateway_id != request.gateway_id || self.boot_id != request.gateway_boot_id
                || snapshot.region != request.region { return Err(409); }
            if serving && request.generation <= snapshot.closed_through_generation
                || !serving && snapshot.closed_through_generation < request.generation { return Err(409); }
            let matching: Vec<_> = snapshot.endpoints.iter().filter(|e| e.deployment_id == request.deployment_id).collect();
            let endpoint = matching.first().filter(|_| matching.len() == 1).ok_or(409u16)?;
            if endpoint.revision.as_deref() != Some(&request.revision) || endpoint.region != request.region
                || endpoint.backend_server_id != spec.backend_server_id
                || (serving && (endpoint.health_status != "healthy" || endpoint.draining)) { return Err(409); }
            let peer = crate::discovery::upstream_from_url(&endpoint.url).map_err(|_| 409u16)?;
            let address: std::net::SocketAddr = peer.parse().map_err(|_| 409u16)?;
            if !address.ip().is_loopback() { return Err(409); }
            // A candidate need not be healthy or publicly admitted yet. This
            // private backend is never inserted into the public backend pool.
            Arc::new(VmBackend::for_upstream(peer))
        } else { Arc::new(VmBackend::for_upstream(gateway.url.clone())) };
        let counters = state.counters.entry((request.generation,request.region.clone())).or_default();
        counters.0 += u64::from(!destination);
        counters.1 += u64::from(destination);
        Ok(Assignment {router:self.clone(),generation:request.generation,region:request.region.clone(),
            source:!destination,local:destination,backend,forward:None})
    }

    pub async fn probe_remote(self: &Arc<Self>, spec: &RegionalSpec, service: &str, request: &ProbeRequest,
        host: &str, health: &crate::config::HealthCheck, secrets: &SecretStore) -> Result<(), u16> {
        let assignment = self.probe_assignment(spec,request,false)?;
        let forward = GatewaySpec {service:service.into(),region:request.region.clone(),auth:spec.auth.clone(),mode:GatewayMode::Forward};
        let token = crate::gateway::admit(Some(&forward),&http::HeaderMap::new(),secrets)?.ok_or(503u16)?;
        let mut headers = http::HeaderMap::new();
        crate::gateway::write_forward_headers(&mut headers,&forward,&token).map_err(|_| 503u16)?;
        for (name,value) in [(ENVIRONMENT,spec.environment.clone()),(GENERATION,request.generation.to_string()),
            (PROBE,serde_json::to_string(request).map_err(|_| 400u16)?)] {
            headers.insert(name,http::HeaderValue::from_str(&value).map_err(|_| 400u16)?);
        }
        headers.insert(http::header::HOST,host.parse().map_err(|_| 400u16)?);
        let path = health.path.as_deref().ok_or(409u16)?;
        let mut url = reqwest::Url::parse(&assignment.backend.peer).map_err(|_| 409u16)?;
        url.set_path(path);
        let response = probe_http(health)?.get(url).headers(headers).send().await.map_err(|_| 502u16)?;
        if !response.status().is_success() { return Err(502); }
        let bytes = probe_body(response).await?;
        let receipt: ProbeRequest = serde_json::from_slice(&bytes).map_err(|_| 502u16)?;
        if &receipt != request { return Err(502); }
        Ok(())
    }

    pub async fn probe_local(self: &Arc<Self>, spec: &RegionalSpec, service: &str, region: &str,
        headers: &http::HeaderMap, method: &http::Method, uri: &http::Uri, host: &str,
        health: &crate::config::HealthCheck, secrets: &SecretStore) -> Result<ProbeRequest,u16> {
        let local = GatewaySpec {service:service.into(),region:region.into(),auth:spec.auth.clone(),mode:GatewayMode::Local};
        crate::gateway::admit(Some(&local),headers,secrets)?;
        if method != http::Method::GET || uri.query().is_some() || Some(uri.path()) != health.path.as_deref()
            || headers.get_all(PROBE).iter().count() != 1 || headers.get_all(ENVIRONMENT).iter().count() != 1
            || headers.get(ENVIRONMENT).and_then(|v| v.to_str().ok()) != Some(&spec.environment)
            || headers.get_all(GENERATION).iter().count() != 1 { return Err(403); }
        let request: ProbeRequest = serde_json::from_slice(headers.get(PROBE).ok_or(403u16)?.as_bytes()).map_err(|_| 403u16)?;
        if headers.get(GENERATION).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok()) != Some(request.generation) { return Err(403); }
        let assignment = self.probe_assignment(spec,&request,true)?;
        let mut url = reqwest::Url::parse(&format!("http://{}",assignment.backend.address)).map_err(|_| 409u16)?;
        url.set_path(health.path.as_deref().ok_or(409u16)?);
        if let Some(port) = health.port { url.set_port(Some(port)).map_err(|_| 409u16)?; }
        // Never forward incoming body, Authorization, query or arbitrary probe
        // headers to the candidate. Only the configured health GET is executed.
        let response = probe_http(health)?.get(url).header(http::header::HOST,host).send().await.map_err(|_| 502u16)?;
        if !response.status().is_success() || response.headers().get_all("x-heyo-revision").iter().count() != 1
            || response.headers().get("x-heyo-revision").and_then(|v| v.to_str().ok()) != Some(&request.revision)
            || health.expected_header.as_ref().is_some_and(|expected| response.headers().get_all(&expected.name).iter().count() != 1
                || response.headers().get(&expected.name).is_none_or(|v| v.as_bytes() != expected.value.as_bytes())) { return Err(502); }
        probe_body(response).await?;
        Ok(request)
    }

    pub fn status(&self, spec: &RegionalSpec, enabled: bool) -> serde_json::Value {
        let mut state = self.state.lock().unwrap();
        state.sequence += 1;
        let Some(s) = &state.snapshot else { return serde_json::json!({"gatewayId":spec.gateway_id,"bootId":self.boot_id,"report":null}); };
        let target = s.drain_target.as_deref();
        let (outgoing, local) = state.counters.iter().filter(|((_, region), _)| Some(region.as_str()) == target)
            .fold((0u64, 0u64), |a, (_, b)| (a.0 + b.0, a.1 + b.1));
        let prepared = enabled && !state.fenced && (s.policies.iter().find(|p| p.generation == s.proposal_generation)
            .and_then(|p| p.policy.regions.iter().find(|r| r.region == s.region)).is_some_and(|r| r.weight == 0)
            || state.local.iter().any(|b| b.is_healthy() && !b.is_draining()));
        serde_json::json!({"gatewayId":spec.gateway_id,"bootId":self.boot_id,"operationId":s.operation_id,
            "report":{"gatewayId":spec.gateway_id,"bootId":self.boot_id,"sequence":state.sequence,
                "generation":s.proposal_generation,"prepared":prepared,"adopted":s.active_generation == Some(s.proposal_generation),
                "outgoingTarget":outgoing,"localTarget":local,"peerAdmissionClosed":s.closed_through_generation >= s.proposal_generation}})
    }
}

fn probe_http(health: &crate::config::HealthCheck) -> Result<reqwest::Client,u16> {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(health.timeout_secs.clamp(1,30)))
        .build().map_err(|_| 503)
}

async fn probe_body(mut response: reqwest::Response) -> Result<Vec<u8>,u16> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| 502u16)? {
        if body.len() + chunk.len() > 65536 { return Err(502); }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub struct Assignment {
    router: Arc<Router>, pub generation: i64, region: String, source: bool, local: bool,
    pub backend: Arc<VmBackend>, pub forward: Option<(GatewaySpec, String, String)>,
}
impl Drop for Assignment {
    fn drop(&mut self) {
        let mut state = self.router.state.lock().unwrap();
        let key = (self.generation, self.region.clone());
        let counters = state.counters.get_mut(&key).expect("admitted assignment has counters");
        counters.0 -= u64::from(self.source);
        counters.1 -= u64::from(self.local);
        if *counters == (0, 0) { state.counters.remove(&key); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (Arc<Router>, RegionalSpec, SecretStore, Snapshot, Vec<Arc<VmBackend>>) {
        let router = Arc::new(Router::new());
        let spec = serde_json::from_value(json!({"gateway_id":"eu","backend_server_id":"host-eu","environment":"test","auth":{"secret":"peer"}})).unwrap();
        let secrets = SecretStore::new("unused-regional-test", None);
        secrets.put(serde_json::from_value(json!({"id":"peer","data":{"token":"test-only"}})).unwrap());
        let snapshot = serde_json::from_value(json!({"protocolVersion":1,"serviceId":"svc","environment":"test",
            "region":"eu1","gatewayId":"eu","bootId":router.boot_id,"version":1,"operationId":"op",
            "phase":"wait_policy_prepared","proposalGeneration":1,"activeGeneration":1,
            "drainTarget":"eu1","closedThroughGeneration":0,"endpoints":[],"policies":[{"generation":1,
                "policy":{"version":1,"regions":[
                    {"region":"eu1","weight":1,"gateways":[{"id":"eu","backendServerId":"host-eu","url":"https://eu.example"}]},
                    {"region":"us3","weight":0,"gateways":[{"id":"us","backendServerId":"host-us","url":"https://us.example"}]}
                ]}}]})).unwrap();
        (router, spec, secrets, snapshot, vec![Arc::new(VmBackend::for_upstream("127.0.0.1:8888".into()))])
    }

    fn probe_fixture() -> (Arc<Router>,RegionalSpec,SecretStore,Snapshot,ProbeRequest) {
        let (router,spec,secrets,mut snapshot,_) = setup();
        snapshot.phase = "passed".into();
        snapshot.closed_through_generation = 1;
        snapshot.policies[0].policy.regions[0].weight = 0;
        snapshot.policies[0].policy.regions[1].weight = 7;
        snapshot.endpoints.push(Endpoint {deployment_id:"candidate".into(),backend_server_id:"host-eu".into(),region:"eu1".into(),
            revision:Some("new-revision".into()),url:"http://127.0.0.1:9999".into(),health_status:"unknown".into(),draining:true});
        let request = ProbeRequest {operation_id:"op".into(),generation:1,region:"eu1".into(),gateway_id:"eu".into(),
            gateway_boot_id:router.boot_id.clone(),deployment_id:"candidate".into(),revision:"new-revision".into()};
        (router,spec,secrets,snapshot,request)
    }

    #[test]
    fn candidate_reservation_requires_exact_identity_and_completed_withdrawal() {
        let (router,spec,secrets,mut snapshot,request) = probe_fixture();
        snapshot.phase = "wait_admission_drained".into();
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_err());
        snapshot.version += 1;
        snapshot.phase = "passed".into();
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        let probe = router.probe_assignment(&spec,&request,true).unwrap();
        assert_eq!(probe.backend.peer,"127.0.0.1:9999");
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],1);
        assert!(router.admit(&spec,"svc","eu1",&http::HeaderMap::new(),&secrets).unwrap().forward.is_some());
        drop(probe);
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],0);
        for field in ["operationId","generation","region","gatewayId","gatewayBootId","deploymentId","revision"] {
            let mut changed = serde_json::to_value(&request).unwrap();
            changed[field] = if field == "generation" { json!(2) } else { json!("wrong") };
            assert!(router.probe_assignment(&spec,&serde_json::from_value(changed).unwrap(),true).is_err(),"accepted {field}");
        }
        snapshot.version += 1;
        snapshot.phase = "probe_candidates".into();
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_ok(), "an owning rollout may probe before its terminal step");
        snapshot.version += 1;
        snapshot.phase = "create_candidate".into();
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_err(), "creation alone is not the probe gate");
        snapshot.version += 1;
        snapshot.phase = "probe_retained".into();
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_ok(), "retained readiness also requires exact-target reservations");
        snapshot.version += 1;
        snapshot.phase = "rollback_entry".into();
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_err(), "rollback entry alone does not prove drain");
        snapshot.version += 1;
        snapshot.phase = "probe_candidates".into();
        snapshot.endpoints[0].backend_server_id = "foreign-host".into();
        router.apply(snapshot,&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_err());
    }

    #[test]
    fn serving_probe_requires_open_generation_and_eligible_exact_member() {
        let (router,spec,_,mut snapshot,mut request) = probe_fixture();
        snapshot.phase = "bake".into();
        snapshot.drain_target = None;
        let mut restored = snapshot.policies[0].clone();
        restored.generation = 2;
        restored.policy.regions.iter_mut().find(|r| r.region == "eu1").unwrap().weight = 3;
        snapshot.policies.push(restored);
        snapshot.proposal_generation = 2;
        snapshot.active_generation = Some(2);
        snapshot.endpoints[0].health_status = "healthy".into();
        snapshot.endpoints[0].draining = false;
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_err(), "restoration does not reopen the withdrawn generation");
        request.generation = 2;
        assert!(router.probe_assignment(&spec,&request,true).is_ok());
        snapshot.version += 1;
        snapshot.endpoints[0].draining = true;
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_err());
        snapshot.version += 1;
        snapshot.endpoints[0].draining = false;
        snapshot.endpoints[0].health_status = "unknown".into();
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_err());
        snapshot.version += 1;
        snapshot.endpoints[0].health_status = "healthy".into();
        snapshot.closed_through_generation = 2;
        router.apply(snapshot,&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.probe_assignment(&spec,&request,true).is_err(), "a closed serving generation cannot be health-probed");
    }

    fn active_probe_fixture() -> (Arc<Router>,RegionalSpec,SecretStore,Snapshot,Vec<Arc<VmBackend>>,ActiveProbeRequest) {
        let (router,spec,secrets,mut snapshot,local) = setup();
        let mut pending = snapshot.policies[0].clone(); pending.generation=2;
        pending.policy.regions[0].weight=0; pending.policy.regions[1].weight=7;
        snapshot.policies.push(pending); snapshot.proposal_generation=2; snapshot.version=2;
        snapshot.endpoints.push(Endpoint {deployment_id:"old-eu".into(),backend_server_id:"host-eu".into(),region:"eu1".into(),
            revision:Some("old-revision".into()),url:"http://127.0.0.1:8888".into(),health_status:"healthy".into(),draining:false});
        let request = ActiveProbeRequest {operation_id:"different-operation".into(),step_id:"0:rollback_entry:2".into(),epoch:1,
            challenge:"ab".repeat(32),generation:1,version:2,region:"eu1".into(),gateway_id:"eu".into(),gateway_boot_id:router.boot_id.clone(),
            backend_server_id:"host-eu".into(),deployment_id:"old-eu".into(),revision:"old-revision".into()};
        (router,spec,secrets,snapshot,local,request)
    }

    #[test]
    fn active_probe_distinguishes_active_policy_from_pending_proposal_and_requires_eligible_capacity() {
        let (router,spec,secrets,mut snapshot,local,request) = active_probe_fixture();
        router.apply(snapshot.clone(),&spec,"svc","eu1",local.clone()).unwrap();
        assert_eq!(router.status(&spec,true)["report"]["adopted"],false);
        assert!(router.active_probe_assignment(&spec,&request,true).is_ok());
        assert!(router.probe_assignment(&spec,&ProbeRequest {operation_id:snapshot.operation_id.clone(),generation:1,
            region:"eu1".into(),gateway_id:"eu".into(),gateway_boot_id:router.boot_id.clone(),deployment_id:"old-eu".into(),revision:"old-revision".into()},true).is_err());
        for (field,value) in [("generation",json!(2)),("version",json!(1)),("gatewayBootId",json!("other-boot")),
            ("backendServerId",json!("other-host")),("revision",json!("other-revision")),("challenge",json!(""))] {
            let mut invalid=serde_json::to_value(&request).unwrap(); invalid[field]=value;
            assert!(router.active_probe_assignment(&spec,&serde_json::from_value(invalid).unwrap(),true).is_err(),"{field}");
        }
        let mut public_headers=http::HeaderMap::new(); public_headers.insert(ACTIVE_PROBE,"{}".parse().unwrap());
        assert!(router.admit(&spec,"svc","eu1",&public_headers,&secrets).is_err());
        local[0].set_draining(true);
        assert!(router.active_probe_assignment(&spec,&request,true).is_err());
        local[0].set_draining(false);
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![]).unwrap();
        assert!(router.active_probe_assignment(&spec,&request,true).is_err(),"discovery alone is not available local capacity");
        snapshot.closed_through_generation=1;
        router.apply(snapshot,&spec,"svc","eu1",local).unwrap();
        assert!(router.active_probe_assignment(&spec,&request,true).is_err(),"active probes cannot bypass peer closure");
    }

    #[tokio::test]
    async fn active_probe_revalidates_after_full_body_and_releases_both_lifetime_guards() {
        use axum::{Router as HttpRouter,routing::get};
        let (router,spec,secrets,mut snapshot,_,request)=active_probe_fixture();
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer=listener.local_addr().unwrap().to_string();
        snapshot.endpoints[0].url=format!("http://{peer}");
        let backend=Arc::new(VmBackend::for_upstream(peer));
        let entered=Arc::new(tokio::sync::Notify::new()); let release=Arc::new(tokio::sync::Notify::new());
        let (start,finish)=(entered.clone(),release.clone());
        let app=HttpRouter::new().route("/health",get(move |headers:http::HeaderMap| {
            let (start,finish)=(start.clone(),finish.clone()); async move {
                assert!(!headers.contains_key(ACTIVE_PROBE)); assert!(!headers.contains_key(http::header::AUTHORIZATION));
                start.notify_one();
                ([("x-heyo-revision","old-revision")],axum::body::Body::from_stream(futures::stream::once(async move {
                    finish.notified().await; Ok::<_,std::io::Error>("ready")
                })))
            }
        }));
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap();});
        router.apply(snapshot.clone(),&spec,"svc","eu1",vec![backend.clone()]).unwrap();
        let health=crate::config::HealthCheck {path:Some("/health".into()),timeout_secs:5,..Default::default()};
        let mut headers=http::HeaderMap::new();
        crate::gateway::write_forward_headers(&mut headers,&GatewaySpec {service:"svc".into(),region:"eu1".into(),auth:spec.auth.clone(),mode:GatewayMode::Local},"test-only").unwrap();
        headers.insert(ACTIVE_PROBE,serde_json::to_string(&request).unwrap().parse().unwrap());
        headers.insert(ENVIRONMENT,"test".parse().unwrap()); headers.insert(GENERATION,"1".parse().unwrap());
        let uri:http::Uri="/health".parse().unwrap();
        assert!(router.active_probe_local(&spec,"svc","eu1",&http::HeaderMap::new(),&http::Method::GET,&uri,"svc.example",&health,&secrets).await.is_err());
        for changed in [false,true] {
            let probe=router.active_probe_local(&spec,"svc","eu1",&headers,&http::Method::GET,&uri,"svc.example",&health,&secrets);
            tokio::pin!(probe);
            tokio::select! {result=&mut probe=>panic!("early probe: {result:?}"),_=entered.notified()=>{}}
            assert_eq!(backend.in_flight(),1); assert_eq!(router.status(&spec,true)["report"]["localTarget"],1);
            if changed {snapshot.version+=1; router.apply(snapshot.clone(),&spec,"svc","eu1",vec![backend.clone()]).unwrap();}
            release.notify_one();
            let result=probe.await;
            if changed {assert_eq!(result,Err(409));} else {assert_eq!(result.unwrap().backend_url,snapshot.endpoints[0].url);}
            assert_eq!(backend.in_flight(),0); assert_eq!(router.status(&spec,true)["report"]["localTarget"],0);
        }
        server.abort();
    }

    #[tokio::test]
    async fn peer_probe_requires_auth_and_revision_and_counts_held_response() {
        use axum::{Router as HttpRouter,routing::get};
        use std::sync::atomic::{AtomicBool,Ordering};
        let (router,spec,secrets,mut snapshot,request) = probe_fixture();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let wrong = Arc::new(AtomicBool::new(false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        snapshot.endpoints[0].url = format!("http://{}",listener.local_addr().unwrap());
        let (start,finish,mismatch) = (entered.clone(),release.clone(),wrong.clone());
        let app = HttpRouter::new().route("/health",get(move |headers:http::HeaderMap| {
            let (start,finish,mismatch) = (start.clone(),finish.clone(),mismatch.clone());
            async move {
                assert_eq!(headers["host"],"svc.example");
                assert!(!headers.contains_key(http::header::AUTHORIZATION));
                assert!(!headers.contains_key(PROBE));
                start.notify_one();
                ([("x-heyo-revision",if mismatch.load(Ordering::SeqCst) {"old-revision"} else {"new-revision"})],
                    axum::body::Body::from_stream(futures::stream::once(async move {
                        finish.notified().await;
                        Ok::<_,std::io::Error>("ready")
                    })))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener,app).await.unwrap(); });
        router.apply(snapshot,&spec,"svc","eu1",vec![]).unwrap();
        let health = crate::config::HealthCheck {path:Some("/health".into()),timeout_secs:5,..Default::default()};
        let mut headers = http::HeaderMap::new();
        crate::gateway::write_forward_headers(&mut headers,&GatewaySpec {service:"svc".into(),region:"eu1".into(),auth:spec.auth.clone(),mode:GatewayMode::Local},"test-only").unwrap();
        headers.insert(PROBE,serde_json::to_string(&request).unwrap().parse().unwrap());
        headers.insert(ENVIRONMENT,"test".parse().unwrap());
        headers.insert(GENERATION,"1".parse().unwrap());
        headers.insert(http::header::AUTHORIZATION,"application-secret".parse().unwrap());
        let uri: http::Uri = "/health".parse().unwrap();
        assert!(router.probe_local(&spec,"svc","eu1",&http::HeaderMap::new(),&http::Method::GET,&uri,"svc.example",&health,&secrets).await.is_err());
        assert!(router.probe_local(&spec,"svc","eu1",&headers,&http::Method::POST,&uri,"svc.example",&health,&secrets).await.is_err());
        assert!(router.probe_local(&spec,"svc","eu1",&headers,&http::Method::GET,&"/health?override=1".parse().unwrap(),"svc.example",&health,&secrets).await.is_err());
        let probe = router.probe_local(&spec,"svc","eu1",&headers,&http::Method::GET,&uri,"svc.example",&health,&secrets);
        tokio::pin!(probe);
        tokio::select! { result = &mut probe => panic!("probe ended early: {result:?}"), _ = entered.notified() => {} }
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],1);
        release.notify_one();
        assert_eq!(probe.await.unwrap(),request);
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],0);
        wrong.store(true,Ordering::SeqCst);
        assert_eq!(router.probe_local(&spec,"svc","eu1",&headers,&http::Method::GET,&uri,"svc.example",&health,&secrets).await,Err(502));
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],0);
        server.abort();
    }

    #[test]
    fn withdrawal_retains_old_assignments_and_monotonic_peer_fence() {
        let (router, spec, secrets, mut snapshot, local) = setup();
        assert!(router.admit(&spec,"svc","eu1",&http::HeaderMap::new(),&secrets).is_err(), "cold start must fail closed");
        router.apply(snapshot.clone(), &spec,"svc","eu1",local.clone()).unwrap();
        let old = router.admit(&spec,"svc","eu1",&http::HeaderMap::new(),&secrets).unwrap();
        let mut next = snapshot.policies[0].clone();
        next.generation = 2;
        next.policy.regions[0].weight = 0;
        next.policy.regions[1].weight = 7;
        snapshot.policies.push(next);
        snapshot.version = 2;
        snapshot.proposal_generation = 2;
        snapshot.active_generation = Some(2);
        router.apply(snapshot.clone(), &spec,"svc","eu1",local.clone()).unwrap();
        let new = router.admit(&spec,"svc","eu1",&http::HeaderMap::new(),&secrets).unwrap();
        assert!(new.forward.is_some());
        assert_eq!(router.status(&spec,true)["report"]["outgoingTarget"],1);
        let peer_spec = GatewaySpec { service:"svc".into(),region:"eu1".into(),auth:spec.auth.clone(),mode:GatewayMode::Local };
        let mut peer = http::HeaderMap::new();
        crate::gateway::write_forward_headers(&mut peer,&peer_spec,"test-only").unwrap();
        peer.insert(GENERATION,"1".parse().unwrap());
        peer.insert(ENVIRONMENT,"test".parse().unwrap());
        let incoming = router.admit(&spec,"svc","eu1",&peer,&secrets).unwrap();
        assert!(incoming.forward.is_none(), "peer traffic cannot bounce regions");
        drop(old);
        assert_eq!(router.status(&spec,true)["report"]["outgoingTarget"],0);
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],1);
        snapshot.version = 3;
        snapshot.closed_through_generation = 2;
        router.apply(snapshot.clone(), &spec,"svc","eu1",local.clone()).unwrap();
        assert_eq!(router.admit(&spec,"svc","eu1",&peer,&secrets).err(),Some(503));
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],1);
        drop(incoming);
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],0);
        snapshot.version = 4;
        snapshot.closed_through_generation = 0;
        assert!(router.apply(snapshot,&spec,"svc","eu1",local).is_err());
        assert_eq!(router.admit(&spec,"svc","eu1",&peer,&secrets).err(),Some(503));
    }

    #[test]
    fn rejects_scope_changes_and_immutable_history_mutation() {
        let (router,spec,secrets,snapshot,local) = setup();
        router.apply(snapshot.clone(),&spec,"svc","eu1",local.clone()).unwrap();
        let mut bad = snapshot.clone();
        bad.environment = "production".into();
        assert!(router.apply(bad,&spec,"svc","eu1",local.clone()).is_err());
        let mut bad = snapshot;
        bad.version += 1;
        bad.policies[0].policy.regions[0].weight = 12;
        assert!(router.apply(bad,&spec,"svc","eu1",local).is_err());
        let assignment = router.admit(&spec,"svc","eu1",&http::HeaderMap::new(),&secrets).unwrap();
        router.fence();
        assert_eq!(router.admit(&spec,"svc","eu1",&http::HeaderMap::new(),&secrets).err(),Some(503));
        assert_eq!(router.status(&spec,true)["report"]["prepared"],false);
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],1);
        drop(assignment);
        assert_eq!(router.status(&spec,true)["report"]["localTarget"],0);
    }
}
