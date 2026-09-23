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

pub(super) fn observer_url(observer: &DiscoveryObserver) -> Result<reqwest::Url> {
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GatewayObservation {
    pub gateway_id: String,
    pub boot_id: String,
    pub operation_id: Option<String>,
    pub report: Option<super::regional_reports::Report>,
    pub admission: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GatewayStatus {
    pub service_id: String,
    pub source_url: Option<String>,
    pub regional: Option<GatewayObservation>,
}

pub(super) async fn poll_gateway(state: &AppState, observer: &DiscoveryObserver) -> Result<(GatewayStatus, u32)> {
    let secrets = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url: state.config.heyosecret_url.clone(),
        token: if state.config.heyosecret_internal_api_key.is_empty() { state.config.internal_api_key.clone() }
            else { state.config.heyosecret_internal_api_key.clone() },
        timeout: Some(Duration::from_secs(10)),
    })?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none()).build()?;
    let credential = secrets.read_active(&observer.token_secret_path).await?;
    let token = String::from_utf8(credential.value).context("observer credential must be UTF-8")?;
    anyhow::ensure!(!token.trim().is_empty(), "observer credential is empty");
    let started = std::time::Instant::now();
    let response = client.get(observer_url(observer)?).bearer_auth(token.trim())
        .header("cache-control", "no-cache").send().await?.error_for_status()?;
    anyhow::ensure!(response.headers().get("age").is_none_or(|v| v == "0"), "cached gateway report is not fresh evidence");
    let status = response.json().await?;
    let age = started.elapsed().as_millis().try_into()?;
    anyhow::ensure!(age <= 5_000, "gateway observation is too old");
    Ok((status, age))
}

/// Poll every explicitly bound gateway. Values enter durable report storage only
/// after transport authentication and service/source/operation identity checks.
pub(super) async fn observe_policy(
    state: &AppState, db: &sea_orm::DatabaseConnection, service: &str, operation: &str,
    generation: i64, participants: &[super::regional_reports::Participant],
) -> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let baseline: serde_json::Value = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT baseline_state FROM regional_service_rollouts WHERE service_id=$1 AND operation_id=$2",
        [service.into(), operation.into()])).await?.context("operation disappeared")?.try_get("", "baseline_state")?;
    let fleet: Vec<super::regional_admission::PinnedGateway> = serde_json::from_value(
        baseline.get("regionalFleet").context("operation has no admitted gateway fleet")?.clone())?;
    let observers: Vec<_> = state.config.discovery_observers.iter().filter(|o| o.service_id == service).collect();
    anyhow::ensure!(!participants.is_empty() && observers.len() == participants.len() && fleet.len() == participants.len(), "gateway observer inventory is incomplete");
    let mut ids = HashSet::new();
    let mut urls = HashSet::new();
    for observer in &observers {
        let id = observer.gateway_id.as_deref().context("regional observer requires gateway_id")?;
        anyhow::ensure!(ids.insert(id) && urls.insert(observer_url(observer)?.to_string()), "duplicate regional observer binding");
        anyhow::ensure!(participants.iter().any(|p| p.gateway_id == id && p.region == observer.region), "observer is not a pinned participant");
        anyhow::ensure!(observer.discovery_url.as_ref().is_some_and(|u| !u.is_empty()), "regional observer requires authoritative discovery URL");
        anyhow::ensure!(fleet.iter().any(|p| p.participant.gateway_id == id && p.observer == serde_json::to_value(observer).unwrap()),
            "observer binding changed since admission");
    }
    for observer in observers {
        let (status, age) = poll_gateway(state, observer).await?;
        anyhow::ensure!(status.service_id == service, "gateway returned a different service");
        let regional = status.regional.context("gateway has no hierarchical runtime")?;
        anyhow::ensure!(Some(&regional.gateway_id) == observer.gateway_id.as_ref(), "gateway identity differs from observer binding");
        let pinned = participants.iter().find(|p| p.gateway_id == regional.gateway_id).context("unknown gateway")?;
        if pinned.boot_id != regional.boot_id {
            // Cold-start snapshots are unauthorized for this new boot. It can
            // still invalidate the predecessor; it cannot attest zero work.
            let report = super::regional_reports::Report { gateway_id: regional.gateway_id, boot_id: regional.boot_id,
                sequence: 1, generation, prepared: false, adopted: false, outgoing_target: 0, local_target: 0, peer_admission_closed: false };
            super::regional_reports::record(db, service, operation, &report, age).await?;
            bail!("gateway boot changed");
        }
        let binding = fleet.iter().find(|p| p.participant.gateway_id == regional.gateway_id).context("gateway not admitted")?;
        anyhow::ensure!(regional.admission.as_ref() == Some(&binding.admission), "gateway configuration/readiness changed since admission");
        anyhow::ensure!(status.source_url == observer.discovery_url, "gateway is not consuming its configured discovery authority");
        anyhow::ensure!(regional.operation_id.as_deref() == Some(operation), "gateway has not observed this operation");
        let report = regional.report.context("gateway has not prepared a snapshot")?;
        anyhow::ensure!(report.gateway_id == pinned.gateway_id && report.boot_id == pinned.boot_id && report.generation == generation,
            "gateway report scope differs from its envelope");
        super::regional_reports::record(db, service, operation, &report, age).await?;
    }
    Ok(())
}

async fn inspect_active_fleet(state: &AppState, db: &sea_orm::DatabaseConnection,
    claim: &super::regional_application::PreflightClaim) -> Result<()> {
    use sea_orm::{ConnectionTrait,DbBackend,Statement};
    let observers: Vec<_> = state.config.discovery_observers.iter().filter(|o| o.service_id == claim.service_id).collect();
    anyhow::ensure!(!claim.fleet.is_empty() && observers.len() == claim.fleet.len(), "preflight observer inventory differs from admitted fleet");
    let mut ids=HashSet::new(); let mut urls=HashSet::new();
    for observer in observers {
        let gateway=claim.fleet.iter().find(|g| Some(&g.participant.gateway_id) == observer.gateway_id.as_ref())
            .context("preflight observer is not pinned")?;
        anyhow::ensure!(gateway.observer == serde_json::to_value(observer)? && ids.insert(&gateway.participant.gateway_id)
            && urls.insert(observer_url(observer)?.to_string()), "preflight observer binding changed or is duplicate");
        let (status,_) = poll_gateway(state,observer).await?;
        anyhow::ensure!(status.service_id == claim.service_id, "preflight returned another service");
        let regional=status.regional.context("preflight gateway has no hierarchical runtime")?;
        anyhow::ensure!(regional.gateway_id == gateway.participant.gateway_id, "preflight gateway identity changed");
        uuid::Uuid::parse_str(&regional.boot_id)?;
        if regional.boot_id != gateway.participant.boot_id {
            // An initial preflight may have no proposal of its own. Record only
            // permanent boot invalidation, never synthesized adoption evidence.
            let tx=super::service_deploy::try_service_lifecycle_lock(db,&claim.service_id).await?.context("service lifecycle busy")?;
            tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
                "INSERT INTO regional_gateway_reports(service_id,gateway_id,boot_id,sequence,invalidated,operation_id,generation,report,observed_at)
                 VALUES($1,$2,$3,0,TRUE,$4,$5,'{}',clock_timestamp())
                 ON CONFLICT(service_id,gateway_id,boot_id) DO UPDATE SET invalidated=TRUE",
                vec![claim.service_id.clone().into(),gateway.participant.gateway_id.clone().into(),gateway.participant.boot_id.clone().into(),
                    claim.operation_id.clone().into(),claim.generation.into()])).await?;
            tx.commit().await?;
            bail!("preflight gateway boot changed; predecessor evidence invalidated");
        }
        anyhow::ensure!(regional.admission.as_ref() == Some(&gateway.admission) && status.source_url == observer.discovery_url,
            "preflight gateway configuration/readiness or discovery authority changed");
    }
    Ok(())
}

/// Probe active capacity under a durable execution claim. Proposal preparation
/// reports are deliberately not interpreted as active-policy adoption here.
pub(super) async fn probe_active_capacity(state: &AppState, db: &sea_orm::DatabaseConnection,
    claim: &super::regional_application::PreflightClaim) -> Result<serde_json::Value> {
    inspect_active_fleet(state,db,claim).await?;
    let secrets=HeyoSecretClient::new(HeyoSecretClientOptions {base_url:state.config.heyosecret_url.clone(),
        token:if state.config.heyosecret_internal_api_key.is_empty() {state.config.internal_api_key.clone()}
            else {state.config.heyosecret_internal_api_key.clone()},timeout:Some(Duration::from_secs(5))})?;
    let client=reqwest::Client::builder().timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build()?;
    let mut receipts=Vec::new();
    for source in &claim.fleet {
        let observer: DiscoveryObserver=serde_json::from_value(source.observer.clone())?;
        let credential=secrets.read_active(&observer.token_secret_path).await?;
        let token=String::from_utf8(credential.value).context("observer credential must be UTF-8")?;
        anyhow::ensure!(!token.trim().is_empty(), "observer credential is empty");
        let mut url=observer_url(&observer)?;
        url.path_segments_mut().map_err(|_| anyhow::anyhow!("invalid observer URL"))?.pop().push("regional-active-probe");
        for target in &claim.targets {
            for destination in claim.fleet.iter().filter(|g| g.participant.region == target.region
                && g.admission["backendServerId"] == target.backend_server_id) {
                let request=claim.request(target,destination);
                let response=client.post(url.clone()).bearer_auth(token.trim()).header("cache-control","no-cache, no-store")
                    .json(&request).send().await?.error_for_status()?;
                anyhow::ensure!(response.headers().get("age").is_none_or(|v| v == "0"), "cached active capacity receipt");
                receipts.push(response.json::<serde_json::Value>().await?);
            }
        }
    }
    inspect_active_fleet(state,db,claim).await?;
    Ok(serde_json::Value::Array(receipts))
}

/// Probe an exact deployment through every admitted source and every gateway
/// bound to its host. The caller gets fresh evidence, never a cached readiness
/// bit. A rollback probe must match a retained baseline identity instead of a
/// new creation receipt. Serving checks require adopted policy and eligible
/// membership; they cannot use the excluded-member withdrawal exception.
pub(super) async fn probe_candidate(state: &AppState, db: &sea_orm::DatabaseConnection,
    service: &str, operation: &str, candidate: &str, revision: &str) -> Result<Vec<serde_json::Value>> {
    use sea_orm::{ConnectionTrait,DbBackend,Statement};
    use super::{regional_policy::RegionalPolicy,regional_reports::{self,Participant,Gate}};
    let lock = super::service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    let row = lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT p.generation,p.policy,p.step_id,r.plan,r.observer_topology,r.baseline_state,r.status,r.phase,r.region_index,r.slot_index
         FROM service_active_regional_policies a
         JOIN regional_policy_proposals p USING(service_id,generation)
         JOIN regional_service_rollouts r USING(service_id,operation_id)
         WHERE a.service_id=$1 AND r.operation_id=$2
         AND ((r.status=r.phase AND r.status IN ('passed','rolled_back')) OR
             (r.status='running' AND r.phase IN ('probe_candidates','probe_retained','bake','verify','verify_baseline')))",
        [service.into(),operation.into()])).await?.context("probe requires an active readiness item")?;
    let status: String = row.try_get("","status")?;
    let phase: String = row.try_get("","phase")?;
    let region_index: i32 = row.try_get("","region_index")?;
    let slot_index: i32 = row.try_get("","slot_index")?;
    if status != "running" { super::regional_rollout::ensure_no_regional_rollout(&lock,service).await?; }
    let generation: i64 = row.try_get("","generation")?;
    let policy: RegionalPolicy = serde_json::from_value(row.try_get("","policy")?)?;
    let plan: super::regional_plan::Plan = serde_json::from_value(row.try_get("","plan")?)?;
    let publication = plan.publication(&row.try_get::<String>("","step_id")?)?;
    let serving = matches!(phase.as_str(), "bake" | "verify" | "verify_baseline" | "rolled_back")
        || (phase == "passed" && publication.region.is_none());
    let snapshot = super::service_discovery::read_snapshot_in(&lock,service,true).await?.context("discovery missing")?;
    let endpoint = snapshot.endpoints.iter().find(|e| e.deployment_id == candidate).context("candidate is not in discovery")?;
    let region = if serving { endpoint.region.as_deref().context("serving endpoint has no region")? }
        else { publication.region.as_deref().context("operation is not a regional withdrawal")? };
    anyhow::ensure!(!serving || (publication.region.is_none() && endpoint.health_status == "healthy" && !endpoint.draining),
        "serving probe requires restored eligible membership");
    if status == "running" {
        let current = plan.step(&phase,region_index.try_into()?,slot_index.try_into()?)?;
        anyhow::ensure!(matches!(phase.as_str(), "verify" | "verify_baseline") || current.region.as_deref() == Some(region),
            "probe cursor is outside the owning region");
        anyhow::ensure!(!serving || plan.version == 3, "serving item requires an application plan");
        anyhow::ensure!(plan.version != 3 || (publication.region_index == current.region_index
            && publication.slot_index == current.slot_index), "probe requires its own policy occurrence");
        let prerequisite = plan.step(if serving {"wait_policy_adopted"} else {"wait_admission_drained"},
            publication.region_index,publication.slot_index)?;
        anyhow::ensure!(lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT 1 FROM regional_rollout_items WHERE operation_id=$1 AND step_id=$2 AND status='completed'",
            [operation.into(),prerequisite.id.clone().into()])).await?.is_some(), "probe requires its completed policy prerequisite");
    }
    anyhow::ensure!(!revision.is_empty() && endpoint.revision.as_deref() == Some(revision)
        && endpoint.region.as_deref() == Some(region), "candidate revision/region differs from requested identity");
    if phase == "probe_retained" || (status == "running" && serving && slot_index == 3) {
        anyhow::ensure!(plan.version == 3, "retained probes require an application rollback plan");
        let baseline: serde_json::Value = row.try_get("","baseline_state")?;
        let retained: Vec<super::regional_admission::PinnedEndpoint> = serde_json::from_value(
            baseline.get("regionalRetained").context("rollback has no pinned retained baseline")?.clone())?;
        anyhow::ensure!(retained.iter().filter(|p| p.deployment_id == candidate).count() == 1
            && retained.iter().any(|p| p.matches(endpoint)), "rollback discovery differs from the pinned retained baseline");
    } else if status == "running" {
        anyhow::ensure!(lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT 1 FROM regional_candidate_creations WHERE operation_id=$1 AND deployment_id=$2
             AND intent->>'runtimeRevision'=$3 AND ($4::text IS NULL OR intent->>'withdrawalGeneration'=$4)
             AND receipt->>'backendServerId'=$5 AND receipt->>'hostLocalUrl'=$6 AND intent->>'region'=$7",
            vec![operation.into(),candidate.into(),revision.into(),(!serving).then(|| generation.to_string()).into(),
                endpoint.backend_server_id.clone().into(),endpoint.url.clone().into(),region.into()])).await?.is_some(),
            "candidate discovery is not bound to its durable creation receipt");
    }
    let target = policy.regions.iter().find(|r| r.region == region && (if serving {r.weight > 0} else {r.weight == 0}))
        .context("endpoint region has the wrong serving state")?;
    let gateways: Vec<_> = target.gateways.iter().filter(|g| Some(g.backend_server_id.as_str()) == endpoint.backend_server_id.as_deref()).collect();
    anyhow::ensure!(!gateways.is_empty(), "candidate host has no admitted gateway");
    let participants: Vec<Participant> = serde_json::from_str(&row.try_get::<String>("","observer_topology")?)?;
    // Authenticated observation ingestion owns its own short transaction. Do
    // not hold lifecycle ownership across that path; revalidate after polling.
    lock.commit().await?;
    observe_policy(state,db,service,operation,generation,&participants).await?;
    let gate = if serving {Gate::Adopted} else {Gate::AdmissionDrained};
    anyhow::ensure!(regional_reports::ready(db,service,operation,generation,gate).await?, "probe policy evidence is not fresh and ready");
    let secrets = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url:state.config.heyosecret_url.clone(),token:if state.config.heyosecret_internal_api_key.is_empty() {
            state.config.internal_api_key.clone() } else { state.config.heyosecret_internal_api_key.clone() },timeout:Some(Duration::from_secs(10)),
    })?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(35)).redirect(reqwest::redirect::Policy::none()).build()?;
    let started = std::time::Instant::now();
    let mut receipts = Vec::new();
    for observer in state.config.discovery_observers.iter().filter(|o| o.service_id == service) {
        let source = participants.iter().find(|p| Some(&p.gateway_id) == observer.gateway_id.as_ref()).context("source not pinned")?;
        let credential = secrets.read_active(&observer.token_secret_path).await?;
        let token = String::from_utf8(credential.value).context("observer credential must be UTF-8")?;
        anyhow::ensure!(!token.trim().is_empty(), "observer credential is empty");
        let mut url = observer_url(observer)?;
        url.path_segments_mut().map_err(|_| anyhow::anyhow!("invalid observer URL"))?.pop().push("regional-probe");
        for gateway in &gateways {
            let destination = participants.iter().find(|p| p.gateway_id == gateway.id).context("destination not pinned")?;
            let request = serde_json::json!({"operationId":operation,"generation":generation,"region":region,
                "gatewayId":gateway.id,"gatewayBootId":destination.boot_id,"deploymentId":candidate,"revision":revision});
            let response = client.post(url.clone()).bearer_auth(token.trim()).json(&request).send().await?.error_for_status()?;
            anyhow::ensure!(response.headers().get("age").is_none_or(|v| v == "0"), "cached probe receipt");
            let receipt: serde_json::Value = response.json().await?;
            anyhow::ensure!(receipt["request"] == request && receipt["sourceBootId"] == source.boot_id, "probe receipt identity mismatch");
            receipts.push(receipt);
        }
    }
    observe_policy(state,db,service,operation,generation,&participants).await?;
    let lock = super::service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    if status != "running" { super::regional_rollout::ensure_no_regional_rollout(&lock,service).await?; }
    anyhow::ensure!(lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM regional_service_rollouts WHERE service_id=$1 AND operation_id=$2
         AND status=$3 AND phase=$4 AND region_index=$5 AND slot_index=$6",
        vec![service.into(),operation.into(),status.into(),phase.into(),region_index.into(),slot_index.into()])).await?.is_some(),
        "probe operation cursor changed during probes");
    anyhow::ensure!(lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM service_active_regional_policies WHERE service_id=$1 AND generation=$2",
        vec![service.into(),generation.into()])).await?.is_some(), "active policy changed during probes");
    let current = super::service_discovery::read_snapshot_in(&lock,service,true).await?.context("discovery disappeared")?;
    anyhow::ensure!(current.version == snapshot.version, "candidate membership changed during probes");
    anyhow::ensure!(regional_reports::ready(&lock,service,operation,generation,gate).await?, "probe policy evidence is no longer ready");
    anyhow::ensure!(started.elapsed() <= Duration::from_secs(5), "candidate probe evidence expired");
    lock.commit().await?;
    Ok(receipts)
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
