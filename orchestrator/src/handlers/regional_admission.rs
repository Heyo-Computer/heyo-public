//! Internal admission for hierarchical transitions. No HTTP endpoint: external
//! ingress inventory/fencing and managed fleet replacement remain separate gates.
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::{Duration, Instant}};
use super::{regional_observers, regional_policy::{self, RegionalPolicy}, regional_reports::{self, Participant}};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Request {
    pub operation_id: String,
    pub service_id: String,
    pub environment: String,
    pub namespace: String,
    pub host: String,
    pub expected_version: i64,
    pub expected_predecessor: Option<i64>,
    pub withdraw_region: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PinnedGateway {
    pub participant: Participant,
    /// Configured URL, deployment and secret reference, never the credential value.
    pub observer: Value,
    pub admission: Value,
}

/// Stored in immutable baseline_state before application execution. Health and
/// draining are deliberately not identity: rollback must re-probe retained work.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PinnedEndpoint {
    pub deployment_id: String,
    pub backend_server_id: String,
    pub region: String,
    pub revision: String,
    pub url: String,
}

impl PinnedEndpoint {
    pub fn matches(&self, endpoint: &super::service_discovery::ServiceDiscoveryEndpoint) -> bool {
        self.deployment_id == endpoint.deployment_id && self.url == endpoint.url
            && Some(self.backend_server_id.as_str()) == endpoint.backend_server_id.as_deref()
            && Some(self.region.as_str()) == endpoint.region.as_deref()
            && Some(self.revision.as_str()) == endpoint.revision.as_deref()
    }
}

/// Called under admission's lifecycle lock, before any region is withdrawn.
/// Pin every serving original, not just enough replicas to satisfy the minimum.
pub(super) fn pin_retained(snapshot: &super::service_discovery::ServiceDiscoverySnapshot,
    policy: &RegionalPolicy, regions: &[String], minimum: usize) -> Result<Vec<PinnedEndpoint>> {
    anyhow::ensure!(minimum > 0 && !regions.is_empty()
        && regions.iter().collect::<HashSet<_>>().len() == regions.len(), "invalid retained baseline scope");
    let mut retained = Vec::new();
    let mut identities = HashSet::new();
    for region in regions {
        let inventory = policy.regions.iter().find(|r| &r.region == region).context("baseline region has no gateway inventory")?;
        let start = retained.len();
        for endpoint in snapshot.endpoints.iter().filter(|e| e.region.as_deref() == Some(region.as_str())
            && e.health_status == "healthy" && !e.draining) {
            let host = endpoint.backend_server_id.as_deref().filter(|s| !s.is_empty()).context("baseline host missing")?;
            let revision = endpoint.revision.as_deref().filter(|s| !s.is_empty()).context("baseline revision missing")?;
            let url = reqwest::Url::parse(&endpoint.url)?;
            anyhow::ensure!(!endpoint.deployment_id.is_empty() && identities.insert(endpoint.deployment_id.clone())
                && inventory.gateways.iter().any(|g| g.backend_server_id == host), "baseline identity is duplicate or outside the admitted fleet");
            anyhow::ensure!(url.scheme() == "http" && url.host_str() == Some("127.0.0.1")
                && url.port_or_known_default().is_some_and(|p| p > 0) && url.path() == "/"
                && url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
                "baseline endpoint has no exact host-local mapping");
            retained.push(PinnedEndpoint {deployment_id:endpoint.deployment_id.clone(),backend_server_id:host.into(),
                region:region.clone(),revision:revision.into(),url:endpoint.url.clone()});
        }
        anyhow::ensure!(retained.len() - start >= minimum, "baseline region lacks retained serving capacity");
    }
    retained.sort_by(|a,b| a.deployment_id.cmp(&b.deployment_id));
    Ok(retained)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Receipt { pub generation: i64, pub created: bool }

fn validate(request: &Request) -> Result<()> {
    for value in [&request.operation_id, &request.service_id, &request.environment, &request.namespace] {
        anyhow::ensure!(!value.is_empty() && value.len() <= 128
            && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)), "invalid transition identity");
    }
    anyhow::ensure!(request.service_id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'), "service ID must be canonical");
    anyhow::ensure!(!request.host.is_empty() && request.host == request.host.to_ascii_lowercase(), "host must be canonical");
    anyhow::ensure!(request.expected_version >= 0 && request.expected_predecessor.is_none_or(|g| g > 0), "invalid policy version/predecessor");
    Ok(())
}

async fn receipt(db: &impl ConnectionTrait, request: &Request, hash: &str) -> Result<Option<Receipt>> {
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.service_id,r.request_hash,p.generation FROM regional_service_rollouts r
         LEFT JOIN regional_policy_proposals p USING(service_id,operation_id) WHERE r.operation_id=$1",
        [request.operation_id.clone().into()])).await?;
    row.map(|r| {
        anyhow::ensure!(r.try_get::<String>("", "service_id")? == request.service_id
            && r.try_get::<String>("", "request_hash")? == hash, "operationId already belongs to different intent");
        Ok(Receipt { generation:r.try_get::<Option<i64>>("", "generation")?.context("admission has no atomic publication receipt")?,created:false })
    }).transpose()
}

async fn policy(db: &impl ConnectionTrait, request: &Request) -> Result<(RegionalPolicy, Option<RegionalPolicy>)> {
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT s.version,s.regional_policy,a.generation,p.policy,e.deployment_environment
         FROM service_discovery_sets s LEFT JOIN service_active_regional_policies a USING(service_id)
         LEFT JOIN regional_policy_proposals p ON p.service_id=a.service_id AND p.generation=a.generation
         LEFT JOIN service_deployment_states e ON e.service_id=s.service_id WHERE s.service_id=$1",
        [request.service_id.clone().into()])).await?.context("service discovery is not established")?;
    anyhow::ensure!(row.try_get::<i64>("", "version")? == request.expected_version, "draft/membership version changed");
    anyhow::ensure!(row.try_get::<Option<i64>>("", "generation")? == request.expected_predecessor, "active policy predecessor changed");
    anyhow::ensure!(row.try_get::<Option<String>>("", "deployment_environment")?.as_deref() == Some(&request.environment), "service environment is not bound to this transition");
    let proposed: RegionalPolicy = serde_json::from_value(row.try_get::<Option<Value>>("", "regional_policy")?.context("no draft policy")?)?;
    proposed.validate()?;
    let previous: Option<RegionalPolicy> = row.try_get::<Option<Value>>("", "policy")?.map(serde_json::from_value).transpose()?;
    if let Some(previous) = &previous {
        // A routing-only operation cannot retire/rebind a gateway process.
        // That requires the separate durable gateway handoff protocol.
        let inventory = |p: &RegionalPolicy| {
            let mut entries: Vec<_> = p.regions.iter().flat_map(|r| r.gateways.iter()
                .map(move |g| (r.region.clone(),g.id.clone(),g.backend_server_id.clone(),g.url.clone()))).collect();
            entries.sort(); entries
        };
        anyhow::ensure!(inventory(&proposed) == inventory(previous), "gateway inventory changes require an explicit fleet handoff");
    }
    if let Some(target) = &request.withdraw_region {
        anyhow::ensure!(proposed.regions.iter().any(|r| &r.region == target && r.weight == 0), "withdrawal requires retained zero-weight target inventory");
    }
    for r in &proposed.regions {
        for gateway in &r.gateways {
            anyhow::ensure!(db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
                "SELECT 1 FROM service_discovery_endpoints WHERE service_id=$1 AND backend_server_id=$2 AND region=$3",
                [request.service_id.clone().into(),gateway.backend_server_id.clone().into(),r.region.clone().into()])).await?.is_some(),
                "gateway host has no service placement in its declared region");
        }
    }
    Ok((proposed,previous))
}

fn validate_status(request: &Request, observer: &crate::config::DiscoveryObserver, proposed: &RegionalPolicy,
    status: regional_observers::GatewayStatus) -> Result<PinnedGateway> {
    anyhow::ensure!(status.service_id == request.service_id, "gateway attested a different service");
    let runtime = status.regional.context("gateway does not support hierarchical admission")?;
    let id = observer.gateway_id.as_deref().context("observer has no gateway identity")?;
    anyhow::ensure!(runtime.gateway_id == id, "gateway identity differs from configured observer");
    uuid::Uuid::parse_str(&runtime.boot_id).context("invalid gateway boot UUID")?;
    let gateway = proposed.regions.iter().find(|r| r.region == observer.region)
        .and_then(|r| r.gateways.iter().find(|g| g.id == id)).context("observer is not in the proposed fleet")?;
    let admission = runtime.admission.context("gateway did not attest admission capability")?;
    anyhow::ensure!(admission["protocolVersion"] == 1 && admission["environment"] == request.environment
        && admission["region"] == observer.region && admission["backendServerId"] == gateway.backend_server_id
        && admission["namespace"] == request.namespace, "gateway protocol/environment/placement/namespace mismatch");
    anyhow::ensure!(admission["routeConflict"] == false && admission["maintenance"] == false
        && admission["credentialsReady"] == true, "gateway has conflicting routes, maintenance or missing credentials");
    let routes = admission["routes"].as_array().context("gateway routes missing")?;
    anyhow::ensure!(routes.len() == 1 && routes[0]["host"] == request.host
        && routes[0].get("host_suffix").is_none_or(Value::is_null)
        && routes[0].get("path_prefix").is_none_or(|p| p.is_null() || p == "/")
        && routes[0].get("strip_prefix").is_none_or(|p| p == false), "transition requires the exact unmodified whole-host route");
    let source = super::host_ingress::http_url(admission["sourceUrl"].as_str().context("gateway discovery authority missing")?)?;
    let mut scoped = source;
    scoped.query_pairs_mut().append_pair("region", &observer.region);
    anyhow::ensure!(observer.discovery_url.as_deref() == Some(scoped.as_str()), "gateway discovery authority differs from configured observer");
    match request.expected_predecessor {
        None => anyhow::ensure!(runtime.report.is_none() && status.source_url.is_none(), "initial admission requires cold non-serving gateways"),
        Some(generation) => {
            let report = runtime.report.context("restarted or unprepared gateway cannot rejoin an active fleet")?;
            anyhow::ensure!(report.boot_id == runtime.boot_id && report.gateway_id == id && report.generation == generation
                && report.adopted && report.prepared && status.source_url == observer.discovery_url,
                "gateway has not adopted the active predecessor");
        }
    }
    Ok(PinnedGateway { participant:Participant {gateway_id:id.into(),region:observer.region.clone(),boot_id:runtime.boot_id},
        observer:serde_json::to_value(observer)?,admission })
}

/// Discover, authenticate and pin the configured fleet. Never accept caller-
/// supplied boot IDs or silently skip an unavailable/retired participant.
async fn inspect_fleet(state: &crate::AppState, request: &Request, proposed: &RegionalPolicy) -> Result<Vec<PinnedGateway>> {
    let observers: Vec<_> = state.config.discovery_observers.iter().filter(|o| o.service_id == request.service_id).collect();
    anyhow::ensure!(observers.len() == proposed.regions.iter().map(|r| r.gateways.len()).sum::<usize>(), "configured fleet is incomplete");
    let mut ids = HashSet::new();
    let mut urls = HashSet::new();
    for observer in &observers {
        anyhow::ensure!(ids.insert(observer.gateway_id.as_deref().context("missing gateway identity")?)
            && urls.insert(regional_observers::observer_url(observer)?.to_string()), "duplicate observer binding");
    }
    let mut fleet = Vec::new();
    let mut authority = None;
    for observer in observers {
        let (status, _) = regional_observers::poll_gateway(state, observer).await?;
        let pinned = validate_status(request, observer, proposed, status)?;
        let source = pinned.admission["sourceUrl"].as_str().context("missing source")?.to_owned();
        anyhow::ensure!(authority.as_ref().is_none_or(|previous| previous == &source), "fleet has divergent discovery authorities");
        authority = Some(source);
        fleet.push(pinned);
    }
    fleet.sort_by(|a,b| a.participant.gateway_id.cmp(&b.participant.gateway_id));
    Ok(fleet)
}

/// Register only cold discovery routes on existing managed gateways. Partial
/// progress lives in each app-lb's durable registry: retry must match it exactly,
/// never replace it. This does not publish a proposal or authorize a boot.
pub(super) async fn enroll_cold_fleet(state: &crate::AppState, db: &sea_orm::DatabaseConnection, request: &Request, health_path: &str) -> Result<()> {
    use heyosecret_client::{HeyoSecretClient, HeyoSecretClientOptions};
    validate(request)?;
    anyhow::ensure!(health_path.starts_with('/') && !health_path.starts_with("//"), "health path must be an absolute path");
    anyhow::ensure!(request.expected_predecessor.is_none(), "cold enrollment cannot replace an active fleet");
    let tx = super::service_deploy::try_service_lifecycle_lock(db,&request.service_id).await?.context("service lifecycle busy")?;
    super::service_adoption::ensure_managed(&tx,&request.service_id).await?;
    super::regional_rollout::ensure_no_regional_rollout(&tx,&request.service_id).await?;
    anyhow::ensure!(tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM service_rollouts WHERE service_id=$1 AND status='running'", [request.service_id.clone().into()])).await?.is_none(),
        "ordinary rollout must finish before fleet enrollment");
    let (proposed, _) = policy(&tx,request).await?;
    let observers: Vec<_> = state.config.discovery_observers.iter().filter(|o| o.service_id == request.service_id).collect();
    anyhow::ensure!(observers.len() == proposed.regions.iter().map(|r| r.gateways.len()).sum::<usize>(), "configured fleet is incomplete");
    let mut ids = HashSet::new();
    let mut urls = HashSet::new();
    let mut authority = None;
    // Validate the entire desired inventory before making any remote changes.
    let mut registrations = Vec::new();
    for observer in observers {
        let id = observer.gateway_id.as_deref().context("missing gateway identity")?;
        anyhow::ensure!(ids.insert(id) && urls.insert(regional_observers::observer_url(observer)?.to_string()), "duplicate observer binding");
        let gateway = proposed.regions.iter().find(|r| r.region == observer.region)
            .and_then(|r| r.gateways.iter().find(|g| g.id == id)).context("observer is not in the proposed fleet")?;
        let mut source = reqwest::Url::parse(observer.discovery_url.as_deref().context("discovery authority missing")?)?;
        anyhow::ensure!(source.query_pairs().collect::<Vec<_>>() == vec![("region".into(),observer.region.as_str().into())],
            "regional observer requires exactly its region discovery query");
        source.set_query(None);
        let source = super::host_ingress::http_url(source.as_str())?;
        anyhow::ensure!(authority.as_ref().is_none_or(|previous| previous == &source), "fleet has divergent discovery authorities");
        authority = Some(source.clone());
        let reader = observer.discovery_token_secret.as_deref().context("discovery secret reference missing")?;
        let peer = observer.regional_peer_token_secret.as_deref().context("regional peer secret reference missing")?;
        for secret in [reader,peer] {
            anyhow::ensure!(!secret.is_empty() && secret.len() <= 64 && !secret.contains("..")
                && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)), "invalid app-lb secret reference");
        }
        anyhow::ensure!(reader != peer, "discovery and peer roles require distinct secret references");
        let spec = json!({"id":observer.deployment_id,"namespace":request.namespace,"routes":[{"host":request.host}],
            "health":{"path":health_path,"timeout_secs":2},
            "discovery":{"service_id":request.service_id,"region":observer.region,
                "source":{"url":source.as_str(),"auth":{"secret":reader,"key":"token","namespace":request.namespace}},
                "regional":{"gateway_id":id,"backend_server_id":gateway.backend_server_id,"environment":request.environment,
                    "auth":{"secret":peer,"key":"token","namespace":request.namespace}}}});
        registrations.push((observer,spec));
    }
    let secrets = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url:state.config.heyosecret_url.clone(),
        token:if state.config.heyosecret_internal_api_key.is_empty() {state.config.internal_api_key.clone()}
            else {state.config.heyosecret_internal_api_key.clone()}, timeout:Some(Duration::from_secs(10)),
    })?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build()?;
    for (observer,spec) in registrations {
        let credential = secrets.read_active(&observer.token_secret_path).await?;
        let token = String::from_utf8(credential.value).context("observer credential must be UTF-8")?;
        anyhow::ensure!(!token.trim().is_empty(), "observer credential is empty");
        super::host_ingress::ensure_spec(&client,observer,&spec,token.trim()).await?;
    }
    // Attest capability, configuration and secrets while still cold. Admission
    // re-polls and pins fresh boots later; this observation grants no authority.
    inspect_fleet(state,request,&proposed).await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn admit(state: &crate::AppState, db: &sea_orm::DatabaseConnection, request: &Request) -> Result<Receipt> {
    validate(request)?;
    super::service_adoption::ensure_managed(db,&request.service_id).await?;
    let hash = format!("{:x}",Sha256::digest(serde_json::to_vec(request)?));
    if let Some(existing) = receipt(db,request,&hash).await? { return Ok(existing); }
    super::regional_rollout::ensure_no_regional_rollout(db,&request.service_id).await?;
    let (proposed, predecessor) = policy(db,request).await?;
    let observed = Instant::now();
    let fleet = inspect_fleet(state,request,&proposed).await?;
    let participants: Vec<_> = fleet.iter().map(|p| p.participant.clone()).collect();
    regional_reports::validate_participants(&participants,&proposed,predecessor.as_ref())?;
    let tx = super::service_deploy::try_service_lifecycle_lock(db,&request.service_id).await?.context("service lifecycle busy")?;
    super::service_adoption::ensure_managed(&tx,&request.service_id).await?;
    if let Some(existing) = receipt(&tx,request,&hash).await? { return Ok(existing); }
    anyhow::ensure!(observed.elapsed() <= Duration::from_secs(5), "fleet admission observations expired");
    let (current, _) = policy(&tx,request).await?;
    anyhow::ensure!(current == proposed, "draft policy changed during fleet inspection");
    super::regional_rollout::ensure_no_regional_rollout(&tx,&request.service_id).await?;
    anyhow::ensure!(tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM service_rollouts WHERE service_id=$1 AND status='running'", [request.service_id.clone().into()])).await?.is_none(),
        "ordinary rollout must finish before a policy transition");
    if let Some(generation) = request.expected_predecessor {
        let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT r.observer_topology FROM regional_policy_proposals p JOIN regional_service_rollouts r USING(service_id,operation_id)
             WHERE p.service_id=$1 AND p.generation=$2", vec![request.service_id.clone().into(),generation.into()])).await?.context("predecessor disappeared")?;
        let previous: Vec<Participant> = serde_json::from_str(&row.try_get::<String>("","observer_topology")?)?;
        anyhow::ensure!(previous.len() == participants.len() && previous.iter().all(|old| participants.iter().any(|new|
            old.gateway_id == new.gateway_id && old.region == new.region && old.boot_id == new.boot_id)), "gateway boot changes require explicit predecessor fencing/handoff");
    }
    for participant in &participants {
        anyhow::ensure!(tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT 1 FROM regional_gateway_reports WHERE service_id=$1 AND gateway_id=$2 AND boot_id=$3 AND invalidated",
            [request.service_id.clone().into(),participant.gateway_id.clone().into(),participant.boot_id.clone().into()])).await?.is_none(),
            "gateway boot was permanently invalidated");
    }
    let plan = super::regional_plan::Plan::policy_transition(request.withdraw_region.clone());
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,
         baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
         VALUES($1,$2,$3,$4,'routing-only',$5,$6,$7,'[]',$8,1,1,30,'running','publish_policy')",
        vec![request.operation_id.clone().into(),request.service_id.clone().into(),hash.into(),
            json!({"deploymentEnvironment":request.environment,"policyTransition":request}).into(),serde_json::to_string(&participants)?.into(),
            json!({"regionalFleet":fleet}).into(),json!(proposed.regions.iter().map(|r| &r.region).collect::<Vec<_>>()).into(),serde_json::to_value(plan)?.into()])).await?;
    let generation = regional_policy::publish_proposal_in(&tx,&request.service_id,&request.operation_id,"0:publish_policy:0",&proposed,request.expected_predecessor).await?;
    tx.commit().await?;
    Ok(Receipt {generation,created:true})
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ApplicationRequest {
    pub rollout: super::regional_rollout::RegionalRolloutRequest,
    pub expected_version: i64,
    pub expected_generation: i64,
    pub namespace: String,
    pub runtime_revision: String,
    pub guest_port: u16,
}

async fn application_exists(db: &impl ConnectionTrait, request: &ApplicationRequest, hash: &str) -> Result<bool> {
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT service_id,request_hash,plan FROM regional_service_rollouts WHERE operation_id=$1",
        [request.rollout.operation_id.clone().into()])).await?;
    if let Some(row) = row {
        anyhow::ensure!(row.try_get::<String>("","service_id")? == request.rollout.deployment.service_id
            && row.try_get::<String>("","request_hash")? == hash && row.try_get::<Value>("","plan")?["version"] == 3,
            "operationId already belongs to different intent");
        return Ok(true);
    }
    Ok(false)
}

/// Read only the active policy, never the mutable draft. Caller holds the
/// lifecycle lock; the same frame is checked again after remote inspection.
async fn application_baseline(db: &impl ConnectionTrait, request: &Request, regions: &[String], minimum: usize) -> Result<Value> {
    super::regional_rollout::ensure_no_regional_rollout(db,&request.service_id).await?;
    anyhow::ensure!(db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM service_rollouts WHERE service_id=$1 AND status='running'",[request.service_id.clone().into()])).await?.is_none(),
        "ordinary rollout must finish before application admission");
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT a.generation,p.policy FROM service_active_regional_policies a JOIN regional_policy_proposals p USING(service_id,generation) WHERE a.service_id=$1",
        [request.service_id.clone().into()])).await?.context("application admission requires an active policy")?;
    anyhow::ensure!(Some(row.try_get::<i64>("","generation")?) == request.expected_predecessor,"active policy predecessor changed");
    let policy: RegionalPolicy = serde_json::from_value(row.try_get("","policy")?)?;
    policy.validate()?;
    anyhow::ensure!(regions.iter().all(|name| policy.regions.iter().any(|r| &r.region == name && r.weight > 0))
        && policy.regions.iter().filter(|r| r.weight > 0).all(|r| regions.contains(&r.region)),
        "application regions must match positive-weight baseline regions");
    let snapshot = super::service_discovery::read_snapshot_in(db,&request.service_id,true).await?.context("discovery is missing")?;
    anyhow::ensure!(snapshot.version == request.expected_version as u64,"discovery version changed");
    let retained = pin_retained(&snapshot,&policy,regions,minimum)?;
    let baseline = super::service_deploy::read_service_state_in(db,&request.service_id).await?.context("service baseline is missing")?;
    anyhow::ensure!(baseline.deployment_environment.as_deref() == Some(request.environment.as_str())
        && baseline.active_deployment_id.as_ref().is_some_and(|id| retained.iter().any(|e| &e.deployment_id == id
            && baseline.active_backend_url.as_deref() == Some(e.url.as_str())))
        && baseline.ingress_backend_url.is_some(),"service baseline is not bound to the retained application");
    let route = baseline.route.as_ref().context("baseline route missing")?;
    anyhow::ensure!(route.host == request.host && !route.strip_prefix
        && route.path_prefix.as_deref().is_none_or(|p| p == "/"),"baseline requires the same whole-host route");
    let mut value = serde_json::to_value(baseline)?;
    value["regionalPolicy"] = json!(policy);
    value["regionalRetained"] = json!(retained);
    Ok(value)
}

/// Internal only. A retry returns its durable identity before consulting any
/// remote dependency; no gateway or candidate is created by admission.
pub(super) fn admit_application<'a>(state: &'a crate::AppState, db: &'a sea_orm::DatabaseConnection,
    request: &'a ApplicationRequest) -> impl std::future::Future<Output = Result<bool>> + 'a {
    // Archive and fleet inspection retain substantial request state across
    // awaits. Do not inline that entire future into each controller's stack.
    Box::pin(async move {
    use super::{regional_rollout, service_deploy};
    let hash = format!("{:x}",Sha256::digest(serde_json::to_vec(&serde_json::to_value(request)?)?));
    super::service_adoption::ensure_managed(db,&request.rollout.deployment.service_id).await?;
    if application_exists(db,request,&hash).await? { return Ok(false); }
    let rollout = &request.rollout;
    let deployment = &rollout.deployment;
    regional_rollout::validate_policy(rollout).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(deployment.revision_guard.is_none(),"application revisionGuard cutover checks are not integrated");
    anyhow::ensure!(deployment.placement_pool.as_deref().is_some_and(|p| !p.is_empty()
        && p.trim() == p && p.chars().count() <= 64),"application admission requires an explicit placement pool");
    // The ordinary validator intentionally supports only flat discovery URLs.
    // Hierarchical binding is authenticated by inspect_fleet, not that path.
    service_deploy::validate_service_traffic_mode(&deployment.service_id,deployment.desired_replicas,
        deployment.route.is_some(),state.config.service_uses_discovery_routing(&deployment.service_id))
        .map_err(|(_,message)| anyhow::anyhow!(message))?;
    let route = deployment.route.as_ref().context("route is required")?;
    anyhow::ensure!(!route.strip_prefix && route.path_prefix.as_deref().is_none_or(|p| p == "/")
        && route.backend_url.is_none() && route.entry_points.is_none() && route.cert_resolver.is_none()
        && route.priority.is_none(),"application admission preserves the whole-host gateway route");
    anyhow::ensure!(request.expected_generation > 0 && !request.runtime_revision.is_empty()
        && request.runtime_revision.trim() == request.runtime_revision && request.runtime_revision.len() <= 256
        && request.guest_port > 0 && (deployment.ports.contains(&request.guest_port)
            || deployment.port_mappings.iter().any(|p| p.container == request.guest_port)),"invalid runtime revision or guest port");
    let scope = Request {operation_id:rollout.operation_id.clone(),service_id:deployment.service_id.clone(),
        environment:deployment.deployment_environment.clone().context("deployment environment is required")?,
        namespace:request.namespace.clone(),host:deployment.route.as_ref().context("route is required")?.host.clone(),
        expected_version:request.expected_version,expected_predecessor:Some(request.expected_generation),withdraw_region:None};
    validate(&scope)?;
    let regions = regional_rollout::ordered_regions(&deployment.replica_regions);
    regional_observers::validate_regional_observers(state,&scope.service_id,&regions).await?;
    let tx = service_deploy::try_service_lifecycle_lock(db,&scope.service_id).await?.context("service lifecycle busy")?;
    super::service_adoption::ensure_managed(&tx,&scope.service_id).await?;
    if application_exists(&tx,request,&hash).await? { return Ok(false); }
    let mut baseline = application_baseline(&tx,&scope,&regions,rollout.minimum_serving_replicas.into()).await?;
    tx.commit().await?;
    let active: RegionalPolicy = serde_json::from_value(baseline["regionalPolicy"].clone())?;
    for region in &regions {
        let slots = deployment.replica_regions.iter().filter(|r| *r == region).count();
        anyhow::ensure!(slots >= usize::from(rollout.minimum_serving_replicas),"candidate region cannot satisfy staging capacity");
        let hosts: HashSet<_> = active.regions.iter().filter(|r| &r.region == region)
            .flat_map(|r| r.gateways.iter().map(|g| &g.backend_server_id)).collect();
        anyhow::ensure!(hosts.len() >= slots,
            "pinned regional fleet cannot place distinct candidate slots");
    }
    let target = service_deploy::regional_revision(state,deployment).await?;
    let observed = Instant::now();
    let fleet = inspect_fleet(state,&scope,&active).await?;
    let participants: Vec<_> = fleet.iter().map(|p| p.participant.clone()).collect();
    regional_reports::validate_participants(&participants,&active,None)?;
    let tx = service_deploy::try_service_lifecycle_lock(db,&scope.service_id).await?.context("service lifecycle busy")?;
    super::service_adoption::ensure_managed(&tx,&scope.service_id).await?;
    if application_exists(&tx,request,&hash).await? { return Ok(false); }
    anyhow::ensure!(observed.elapsed() <= Duration::from_secs(5),"fleet admission observations expired");
    anyhow::ensure!(application_baseline(&tx,&scope,&regions,rollout.minimum_serving_replicas.into()).await? == baseline,
        "application baseline changed during fleet inspection");
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.observer_topology FROM regional_policy_proposals p JOIN regional_service_rollouts r USING(service_id,operation_id) WHERE p.service_id=$1 AND p.generation=$2",
        vec![scope.service_id.clone().into(),request.expected_generation.into()])).await?.context("predecessor disappeared")?;
    let previous: Vec<Participant> = serde_json::from_str(&row.try_get::<String>("","observer_topology")?)?;
    anyhow::ensure!(previous.len() == participants.len() && previous.iter().all(|old| participants.iter().any(|new|
        old.gateway_id == new.gateway_id && old.region == new.region && old.boot_id == new.boot_id)),"gateway boot changes require explicit predecessor fencing/handoff");
    for participant in &participants {
        anyhow::ensure!(tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT 1 FROM regional_gateway_reports WHERE service_id=$1 AND gateway_id=$2 AND boot_id=$3 AND invalidated",
            [scope.service_id.clone().into(),participant.gateway_id.clone().into(),participant.boot_id.clone().into()])).await?.is_none(),
            "gateway boot was permanently invalidated");
    }
    let candidates: Vec<_> = deployment.replica_regions.iter().enumerate().map(|(i,r)|
        (r.clone(),regional_rollout::candidate_id(&rollout.operation_id,i))).collect();
    let plan = super::regional_plan::Plan::application(&regions,&candidates)?;
    let slots: Vec<_> = candidates.iter().enumerate().map(|(i,(region,id))| json!({"index":i,"region":region,
        "candidateId":id,"runtime":rollout.runtime_by_region.get(region)})).collect();
    baseline["regionalFleet"] = json!(fleet);
    let mut payload = serde_json::to_value(deployment)?;
    payload["expectedRuntimeRevision"] = json!(request.runtime_revision);
    payload["guestPort"] = json!(request.guest_port);
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,'running','preflight')",
        vec![scope.operation_id.into(),scope.service_id.into(),hash.into(),payload.into(),target.into(),
            serde_json::to_string(&participants)?.into(),baseline.into(),json!(regions).into(),json!(slots).into(),json!(plan).into(),
            (rollout.minimum_serving_replicas as i32).into(),(rollout.bake_seconds as i64).into(),(rollout.drain_timeout_seconds as i64).into()])).await?;
    tx.commit().await?;
    Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_baseline_pins_all_serving_identities_and_requires_each_region() -> Result<()> {
        let policy: RegionalPolicy = serde_json::from_value(json!({"version":1,"regions":[
            {"region":"eu1","weight":3,"gateways":[{"id":"eu","backendServerId":"host-eu","url":"https://eu.example"}]},
            {"region":"us3","weight":7,"gateways":[{"id":"us","backendServerId":"host-us","url":"https://us.example"}]}
        ]}))?;
        let regions = vec!["eu1".into(),"us3".into()];
        let endpoint = |id: &str, region: &str, port| super::super::service_discovery::ServiceDiscoveryEndpoint {
            deployment_id:id.into(),backend_server_id:Some(if region == "eu1" {"host-eu"} else {"host-us"}.into()),
            region:Some(region.into()),revision:Some(format!("revision-{id}")),url:format!("http://127.0.0.1:{port}"),
            health_status:"healthy".into(),draining:false,
        };
        let mut snapshot = super::super::service_discovery::ServiceDiscoverySnapshot {service_id:"smoke".into(),version:1,
            regional_policy:Some(policy.clone()),updated_at:chrono::Utc::now(),endpoints:vec![
                endpoint("eu-b","eu1",18082),endpoint("us-a","us3",19081),endpoint("eu-a","eu1",18081)]};
        let baseline = pin_retained(&snapshot,&policy,&regions,1)?;
        assert_eq!(baseline.iter().map(|e| e.deployment_id.as_str()).collect::<Vec<_>>(),vec!["eu-a","eu-b","us-a"]);
        assert_eq!(baseline[0].revision,"revision-eu-a");
        assert!(baseline[0].matches(&snapshot.endpoints[2]));
        let mut changed = snapshot.endpoints[2].clone();
        changed.health_status = "unknown".into(); changed.draining = true;
        assert!(baseline[0].matches(&changed), "mutable health is not runtime identity");
        for field in ["deploymentId","backendServerId","region","revision","url"] {
            let mut changed = serde_json::to_value(&snapshot.endpoints[2])?;
            changed[field] = json!("different");
            assert!(!baseline[0].matches(&serde_json::from_value(changed)?), "accepted changed {field}");
        }
        assert!(pin_retained(&snapshot,&policy,&regions,2).is_err(), "EU surplus cannot replace missing US capacity");
        snapshot.endpoints[1].draining = true;
        assert!(pin_retained(&snapshot,&policy,&regions,1).is_err());
        snapshot.endpoints[1].draining = false;
        for (field,value) in [("revision",Value::Null),("backendServerId",json!("foreign")),
            ("url",json!("http://127.0.0.1:18081/other")),("deploymentId",json!("eu-b"))] {
            let mut invalid = serde_json::to_value(&snapshot)?;
            invalid["endpoints"][2][field] = value;
            assert!(pin_retained(&serde_json::from_value(invalid)?,&policy,&regions,1).is_err(), "accepted {field}");
        }
        Ok(())
    }

    #[test]
    fn admission_requires_exact_authenticated_cold_binding() {
        let request = Request {operation_id:"initial".into(),service_id:"smoke".into(),environment:"test".into(),
            namespace:"default".into(),host:"smoke.example".into(),expected_version:1,expected_predecessor:None,withdraw_region:None};
        let observer = serde_json::from_value(json!({"service_id":"smoke","region":"eu1","gateway_id":"eu",
            "deployment_id":"smoke","base_url":"https://admin.example","token_secret_path":"test/observer",
            "discovery_url":"https://authority.example/snapshot?region=eu1"})).unwrap();
        let policy = serde_json::from_value(json!({"version":1,"regions":[{"region":"eu1","weight":3,
            "gateways":[{"id":"eu","backendServerId":"host-eu","url":"https://peer.example"}]}]})).unwrap();
        let status = json!({"serviceId":"smoke","regional":{"gatewayId":"eu",
            "bootId":"11111111-1111-4111-8111-111111111111","admission":{"protocolVersion":1,
            "environment":"test","region":"eu1","backendServerId":"host-eu","namespace":"default",
            "routes":[{"host":"smoke.example"}],"sourceUrl":"https://authority.example/snapshot",
            "routeConflict":false,"maintenance":false,"credentialsReady":true}}});
        assert!(validate_status(&request,&observer,&policy,serde_json::from_value(status.clone()).unwrap()).is_ok());
        for (field,value) in [("protocolVersion",json!(2)),("environment",json!("prod")),("region",json!("us3")),
            ("backendServerId",json!("host-us")),("namespace",json!("other")),("routeConflict",json!(true)),
            ("maintenance",json!(true)),("credentialsReady",json!(false)),("sourceUrl",json!("https://other.example/snapshot")),
            ("routes",json!([{"host":"smoke.example","path_prefix":"/partial"}]))] {
            let mut invalid = status.clone();
            invalid["regional"]["admission"][field] = value;
            assert!(validate_status(&request,&observer,&policy,serde_json::from_value(invalid).unwrap()).is_err(),"accepted {field}");
        }
        let mut resumed = request;
        resumed.expected_predecessor = Some(1);
        assert!(validate_status(&resumed,&observer,&policy,serde_json::from_value(status).unwrap()).is_err());
    }
}
