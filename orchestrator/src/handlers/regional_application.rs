//! V3 application lifecycle primitives. Public admission and automatic execution
//! remain closed until the complete restore/bake/rollback program is integrated.
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::Value;
use std::{collections::HashSet, time::{Duration, Instant}};
use super::{regional_admission::PinnedEndpoint, regional_observers, regional_plan::Plan,
    regional_reports::{self, Gate}, service_deploy, service_discovery};

/// Phase timeout is independent of health-claim epochs and bake resets. A
/// restarted controller cannot give unresolved work another unbounded interval.
async fn dispatch_phase(db: &sea_orm::DatabaseConnection, service: &str, operation: &str) -> Result<Option<String>> {
    let Some(tx) = service_deploy::try_service_lifecycle_lock(db,service).await? else {return Ok(None)};
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT plan,phase,region_index,slot_index,
         clock_timestamp() >= attempt_started_at + (drain_timeout_seconds +
             CASE WHEN phase='bake' THEN bake_seconds ELSE 0 END) * interval '1 second' AS expired
         FROM regional_service_rollouts WHERE service_id=$1 AND operation_id=$2 AND status='running' FOR UPDATE",
        [service.into(),operation.into()])).await?;
    let Some(row) = row else {tx.rollback().await?;return Ok(None)};
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    anyhow::ensure!(plan.version == 3, "application dispatcher requires a v3 plan");
    let phase: String = row.try_get("","phase")?;
    plan.step(&phase,row.try_get::<i32>("","region_index")?.try_into()?,row.try_get::<i32>("","slot_index")?.try_into()?)?;
    if row.try_get::<bool>("","expired")? {
        tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE regional_service_rollouts SET status='blocked',error_message='application phase timed out: ' || phase,
             updated_at=clock_timestamp() WHERE operation_id=$1",[operation.into()])).await?;
        tx.commit().await?; return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(phase))
}

/// Internal integration path only. Public admission and the background runner
/// stay fenced until full candidate lifecycle and failure acceptance is proven.
pub(super) async fn tick(state: &crate::AppState, db: &sea_orm::DatabaseConnection, service: &str, operation: &str) -> Result<()> {
    let Some(phase) = dispatch_phase(db,service,operation).await? else {return Ok(())};
    match phase.as_str() {
        "preflight" | "rollback_entry" => {preflight(state,db,service,operation).await?;}
        "publish_policy" => {publish_policy(db,service,operation).await?;}
        "wait_policy_prepared" | "activate_policy" | "wait_policy_adopted" | "wait_assignments_drained"
            | "close_peer_admission" | "wait_admission_drained" => regional_reports::reconcile(state,db,service,operation).await?,
        "create_candidate" => super::regional_candidates::create_or_recover(state,db,service,operation).await?,
        "probe_candidates" | "probe_retained" => probe_and_stage(state,db,service,operation).await?,
        "bake" | "verify" | "verify_baseline" => {verify_serving(state,db,service,operation).await?;}
        _ => anyhow::bail!("unsupported application execution phase: {phase}"),
    }
    Ok(())
}

/// Interrupt the current forward occurrence, retaining the entered region.
/// Retries within rollback never rewind it or implicitly resume blocked work.
pub(super) async fn begin_rollback(db: &sea_orm::DatabaseConnection, service: &str, operation: &str) -> Result<String> {
    let tx = service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT plan,phase,region_index,slot_index FROM regional_service_rollouts
         WHERE service_id=$1 AND operation_id=$2 AND status IN ('running','blocked') FOR UPDATE",
        [service.into(),operation.into()])).await?.context("only an active or blocked application can roll back")?;
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    anyhow::ensure!(plan.version == 3, "application rollback requires a v3 plan");
    let phase: String = row.try_get("","phase")?;
    let region = row.try_get::<i32>("","region_index")?.try_into()?;
    let current = plan.step(&phase,region,row.try_get::<i32>("","slot_index")?.try_into()?)?;
    if plan.rollback_steps.iter().any(|s| s.id == current.id) {
        tx.commit().await?; return Ok(phase);
    }
    let entry = plan.step("rollback_entry",region,2)?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET status='running',phase=$2,slot_index=$3,
         error_message=NULL,updated_at=clock_timestamp() WHERE operation_id=$1",
        vec![operation.into(),entry.phase.clone().into(),i32::try_from(entry.slot_index)?.into()])).await?;
    tx.commit().await?;
    Ok(entry.phase.clone())
}

fn publication_policy(baseline: &super::regional_policy::RegionalPolicy, active: &super::regional_policy::RegionalPolicy,
    plan: &Plan, step: &super::regional_plan::Step) -> Result<super::regional_policy::RegionalPolicy> {
    anyhow::ensure!(plan.version == 3, "application policy requires a v3 plan");
    plan.publication(&step.id)?;
    baseline.validate()?;
    let target = plan.steps.iter().find(|s| s.phase == "preflight" && s.region_index == step.region_index)
        .and_then(|s| s.region.as_deref()).context("publication has no application region")?;
    anyhow::ensure!(plan.steps.iter().filter(|s| s.phase == "preflight").all(|s|
        baseline.regions.iter().any(|r| Some(&r.region) == s.region.as_ref() && r.weight > 0)),
        "application baseline requires positive restore weights in every entered region");
    let mut expected = baseline.clone();
    let previous = active.regions.iter().find(|r| r.region == target).context("active target missing")?;
    let regional = expected.regions.iter_mut().find(|r| r.region == target).context("baseline target missing")?;
    anyhow::ensure!(previous.weight == 0 || previous.weight == regional.weight,
        "active target weight differs from the pinned application policy");
    regional.weight = previous.weight;
    anyhow::ensure!(&expected == active, "active fleet or non-target weights differ from application baseline");
    let mut proposed = baseline.clone();
    if let Some(withdrawn) = &step.region {
        anyhow::ensure!(withdrawn == target, "publication withdraws a different application region");
        proposed.regions.iter_mut().find(|r| &r.region == withdrawn).unwrap().weight = 0;
    }
    proposed.validate()?;
    Ok(proposed)
}

/// Derive from the admission baseline, never a mutable discovery draft. Publish
/// under the same lifecycle lock and active-predecessor CAS as the item journal.
pub(super) async fn publish_policy(db: &sea_orm::DatabaseConnection, service: &str, operation: &str) -> Result<i64> {
    let tx = service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    let generation = publish_policy_in(&tx,service,operation).await?;
    tx.commit().await?;
    Ok(generation)
}

async fn publish_policy_in(tx: &sea_orm::DatabaseTransaction, service: &str, operation: &str) -> Result<i64> {
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.plan,r.region_index,r.slot_index,r.baseline_state,a.generation,p.policy
         FROM regional_service_rollouts r JOIN service_active_regional_policies a USING(service_id)
         JOIN regional_policy_proposals p ON p.service_id=a.service_id AND p.generation=a.generation
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running' AND r.phase='publish_policy' FOR UPDATE OF r",
        [service.into(),operation.into()])).await?.context("application has no owning publication item")?;
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    let step = plan.step("publish_policy",row.try_get::<i32>("","region_index")?.try_into()?,
        row.try_get::<i32>("","slot_index")?.try_into()?)?;
    let baseline: Value = row.try_get("","baseline_state")?;
    let baseline = serde_json::from_value(baseline.get("regionalPolicy").context("application policy baseline missing")?.clone())?;
    let active = serde_json::from_value(row.try_get("","policy")?)?;
    let proposed = publication_policy(&baseline,&active,&plan,step)?;
    super::regional_policy::publish_proposal_in(tx,service,operation,&step.id,&proposed,
        Some(row.try_get("","generation")?)).await
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PreflightClaim {
    pub service_id: String, pub operation_id: String, pub step: super::regional_plan::Step,
    pub epoch: i64, pub generation: i64, pub version: u64, pub challenge: String,
    pub targets: Vec<PinnedEndpoint>, pub fleet: Vec<super::regional_admission::PinnedGateway>,
}

impl PreflightClaim {
    pub fn request(&self, target: &PinnedEndpoint, gateway: &super::regional_admission::PinnedGateway) -> Value {
        serde_json::json!({"operationId":self.operation_id,"stepId":self.step.id,"epoch":self.epoch,"challenge":self.challenge,
            "generation":self.generation,"version":self.version,"region":target.region,"gatewayId":gateway.participant.gateway_id,
            "gatewayBootId":gateway.participant.boot_id,"backendServerId":target.backend_server_id,
            "deploymentId":target.deployment_id,"revision":target.revision})
    }

    fn accepts(&self, receipts: &Value) -> bool {
        let Some(receipts) = receipts.as_array() else {return false};
        let mut expected = 0;
        for source in &self.fleet {
            for target in &self.targets {
                let destinations: Vec<_> = self.fleet.iter().filter(|g| g.participant.region == target.region
                    && g.admission["backendServerId"] == target.backend_server_id).collect();
                if destinations.is_empty() {return false;}
                for destination in destinations {
                    let request = self.request(target,destination);
                    if receipts.iter().filter(|r| r["request"] == request && r["sourceGatewayId"] == source.participant.gateway_id
                        && r["sourceBootId"] == source.participant.boot_id && r["destination"]["request"] == request
                        && r["destination"]["gatewayId"] == destination.participant.gateway_id
                        && r["destination"]["gatewayBootId"] == destination.participant.boot_id
                        && r["destination"]["backendServerId"] == target.backend_server_id
                        && r["destination"]["backendUrl"] == target.url).count() != 1 {return false;}
                    expected += 1;
                }
            }
        }
        expected > 0 && receipts.len() == expected
    }
}

async fn claim_preflight(db: &sea_orm::DatabaseConnection, service: &str, operation: &str) -> Result<Option<PreflightClaim>> {
    let tx = service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.*,a.generation,p.policy,clock_timestamp() AS observed_now FROM regional_service_rollouts r
         JOIN service_active_regional_policies a USING(service_id)
         JOIN regional_policy_proposals p ON p.service_id=a.service_id AND p.generation=a.generation
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running' AND r.phase IN ('preflight','rollback_entry') FOR UPDATE OF r",
        [service.into(),operation.into()])).await?.context("application has no preflight owner")?;
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    let step = plan.step(&row.try_get::<String>("","phase")?,row.try_get::<i32>("","region_index")?.try_into()?,
        row.try_get::<i32>("","slot_index")?.try_into()?)?.clone();
    let next = plan.successor(&step.id)?;
    let baseline: Value = row.try_get("","baseline_state")?;
    let pinned_policy = serde_json::from_value(baseline.get("regionalPolicy").context("application policy baseline missing")?.clone())?;
    let active: super::regional_policy::RegionalPolicy = serde_json::from_value(row.try_get("","policy")?)?;
    publication_policy(&pinned_policy,&active,&plan,next)?;
    let snapshot = service_discovery::read_snapshot_in(&tx,service,true).await?.context("discovery missing")?;
    let generation: i64 = row.try_get("","generation")?;
    let now: chrono::DateTime<chrono::Utc> = row.try_get("","observed_now")?;
    if row.try_get::<Option<String>>("","probe_step_id")?.as_deref() == Some(&step.id)
        && row.try_get::<Option<i64>>("","probe_policy_generation")? == Some(generation)
        && row.try_get::<Option<i64>>("","probe_discovery_version")? == Some(snapshot.version.try_into()?)
        && row.try_get::<Option<chrono::DateTime<chrono::Utc>>>("","probe_expires_at")?.is_some_and(|t| t > now) {
        tx.rollback().await?; return Ok(None);
    }
    let retained: Vec<PinnedEndpoint> = serde_json::from_value(baseline.get("regionalRetained").context("retained baseline missing")?.clone())?;
    let mut targets = Vec::new();
    for endpoint in snapshot.endpoints.iter().filter(|e| e.region.as_ref() != step.region.as_ref()
        && e.health_status == "healthy" && !e.draining
        && active.regions.iter().any(|r| Some(&r.region) == e.region.as_ref() && r.weight > 0)) {
        if !retained.iter().any(|p| p.matches(endpoint)) {
            anyhow::ensure!(tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
                "SELECT 1 FROM regional_candidate_creations WHERE operation_id=$1 AND deployment_id=$2
                 AND intent->>'runtimeRevision'=$3 AND intent->>'region'=$4
                 AND receipt->>'backendServerId'=$5 AND receipt->>'hostLocalUrl'=$6",
                vec![operation.into(),endpoint.deployment_id.clone().into(),endpoint.revision.clone().into(),endpoint.region.clone().into(),
                    endpoint.backend_server_id.clone().into(),endpoint.url.clone().into()])).await?.is_some(), "survivor is outside the pinned application identities");
        }
        targets.push(PinnedEndpoint {deployment_id:endpoint.deployment_id.clone(),backend_server_id:endpoint.backend_server_id.clone().context("survivor host missing")?,
            region:endpoint.region.clone().context("survivor region missing")?,revision:endpoint.revision.clone().context("survivor revision missing")?,url:endpoint.url.clone()});
    }
    let minimum: i32 = row.try_get("","minimum_serving_replicas")?;
    anyhow::ensure!(minimum > 0 && targets.len() >= minimum as usize
        && targets.iter().map(|e| &e.deployment_id).collect::<HashSet<_>>().len() == targets.len(), "insufficient distinct survivor capacity");
    let claim = PreflightClaim {service_id:service.into(),operation_id:operation.into(),step,
        epoch:row.try_get::<i64>("","probe_epoch")?.checked_add(1).context("probe epoch exhausted")?,generation,version:snapshot.version,
        challenge:format!("{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple()),targets,
        fleet:serde_json::from_value(baseline.get("regionalFleet").context("admitted fleet missing")?.clone())?};
    let participants: Vec<_> = claim.fleet.iter().map(|p| p.participant.clone()).collect();
    regional_reports::validate_participants(&participants,&active,None)?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET probe_epoch=$2,probe_step_id=$3,probe_policy_generation=$4,
         probe_discovery_version=$5,probe_context=$6,probe_expires_at=clock_timestamp()+interval '5 seconds' WHERE operation_id=$1",
        vec![operation.into(),claim.epoch.into(),claim.step.id.clone().into(),generation.into(),i64::try_from(claim.version)?.into(),
            serde_json::to_value(&claim)?.into()])).await?;
    tx.commit().await?;
    Ok(Some(claim))
}

async fn finish_preflight(db: &sea_orm::DatabaseConnection, claim: &PreflightClaim, receipts: Option<Value>) -> Result<bool> {
    let tx = service_deploy::try_service_lifecycle_lock(db,&claim.service_id).await?.context("service lifecycle busy")?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.plan,a.generation,s.version,r.probe_expires_at > clock_timestamp() AS fresh
         FROM regional_service_rollouts r JOIN service_active_regional_policies a USING(service_id)
         JOIN service_discovery_sets s USING(service_id) WHERE r.service_id=$1 AND r.operation_id=$2
         AND r.status='running' AND r.region_index || ':' || r.phase || ':' || r.slot_index=$3
         AND r.probe_step_id=$3 AND r.probe_epoch=$4 AND r.probe_context=$5 AND r.probe_expires_at IS NOT NULL FOR UPDATE OF r",
        vec![claim.service_id.clone().into(),claim.operation_id.clone().into(),claim.step.id.clone().into(),claim.epoch.into(),
            serde_json::to_value(claim)?.into()])).await?;
    let Some(row) = row else {tx.rollback().await?;return Ok(false)};
    let mut valid = receipts.as_ref().is_some_and(|r| claim.accepts(r)) && row.try_get::<bool>("","fresh")?
        && row.try_get::<i64>("","generation")? == claim.generation && row.try_get::<i64>("","version")? == i64::try_from(claim.version)?;
    for gateway in &claim.fleet {
        valid &= tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT 1 FROM regional_gateway_reports WHERE service_id=$1 AND gateway_id=$2 AND boot_id=$3 AND invalidated",
            [claim.service_id.clone().into(),gateway.participant.gateway_id.clone().into(),gateway.participant.boot_id.clone().into()])).await?.is_none();
    }
    if !valid {
        tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE regional_service_rollouts SET probe_expires_at=NULL,error_message='preflight failed or evidence expired' WHERE operation_id=$1",
            [claim.operation_id.clone().into()])).await?;
        tx.commit().await?; return Ok(false);
    }
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    let next = plan.successor(&claim.step.id)?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_rollout_items SET evidence=jsonb_build_object('claim',$3::jsonb,'receipts',$4::jsonb,'acceptedAt',clock_timestamp())
         WHERE operation_id=$1 AND step_id=$2",
        vec![claim.operation_id.clone().into(),claim.step.id.clone().into(),serde_json::to_value(claim)?.into(),receipts.unwrap().into()])).await?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET phase=$2,slot_index=$3,error_message=NULL,updated_at=clock_timestamp() WHERE operation_id=$1",
        vec![claim.operation_id.clone().into(),next.phase.clone().into(),i32::try_from(next.slot_index)?.into()])).await?;
    publish_policy_in(&tx,&claim.service_id,&claim.operation_id).await?;
    tx.commit().await?;
    Ok(true)
}

pub(super) async fn preflight(state: &crate::AppState, db: &sea_orm::DatabaseConnection, service: &str, operation: &str) -> Result<bool> {
    let Some(claim) = claim_preflight(db,service,operation).await? else {return Ok(false)};
    let result = tokio::time::timeout(Duration::from_secs(5),regional_observers::probe_active_capacity(state,db,&claim))
        .await.context("preflight probe set timed out").and_then(|r| r);
    let advanced = finish_preflight(db,&claim,result.as_ref().ok().cloned()).await?;
    result?;
    Ok(advanced)
}

/// Probe every desired member through all pinned gateways, then change regional
/// eligibility and complete the probe item atomically. No cached/caller-supplied
/// receipt can authorize restoration, and no policy weight changes here.
pub(super) async fn probe_and_stage(state: &crate::AppState, db: &sea_orm::DatabaseConnection,
    service: &str, operation: &str) -> Result<()> {
    let lock = service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    let row = lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.plan,r.phase,r.region_index,r.slot_index,r.baseline_state,r.deployment_request,r.minimum_serving_replicas,r.probe_epoch,a.generation
         FROM regional_service_rollouts r JOIN service_active_regional_policies a USING(service_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running' AND r.phase IN ('probe_candidates','probe_retained')",
        [service.into(),operation.into()])).await?.context("application has no owning probe item")?;
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    anyhow::ensure!(plan.version == 3, "membership staging requires an application plan");
    let phase: String = row.try_get("","phase")?;
    let region_index: i32 = row.try_get("","region_index")?;
    let slot_index: i32 = row.try_get("","slot_index")?;
    let current = plan.step(&phase,region_index.try_into()?,slot_index.try_into()?)?;
    let region = current.region.as_deref().context("probe item has no region")?;
    let next = plan.successor(&current.id)?;
    anyhow::ensure!(next.phase == "publish_policy" && next.region.is_none()
        && next.region_index == current.region_index && next.slot_index == current.slot_index + 1,
        "probe successor is not its policy restoration");
    let targets: Vec<(String,String)> = if phase == "probe_retained" {
        let baseline: Value = row.try_get("","baseline_state")?;
        let retained: Vec<PinnedEndpoint> = serde_json::from_value(
            baseline.get("regionalRetained").context("rollback has no retained baseline")?.clone())?;
        retained.into_iter().filter(|e| e.region == region).map(|e| (e.deployment_id,e.revision)).collect()
    } else {
        let input: Value = row.try_get("","deployment_request")?;
        let revision = input["expectedRuntimeRevision"].as_str().filter(|s| !s.is_empty()).context("application revision missing")?;
        plan.steps.iter().filter(|s| s.phase == "create_candidate" && s.region_index == current.region_index)
            .map(|s| Ok((s.candidate_id.clone().context("candidate identity missing")?,revision.to_owned())))
            .collect::<Result<_>>()?
    };
    let minimum: i32 = row.try_get("","minimum_serving_replicas")?;
    anyhow::ensure!(minimum > 0 && targets.len() >= minimum as usize
        && targets.iter().map(|(id,_)| id).collect::<HashSet<_>>().len() == targets.len(),
        "probe targets lack distinct regional serving capacity");
    let version = service_discovery::read_snapshot_in(&lock,service,true).await?.context("discovery missing")?.version;
    let generation: i64 = row.try_get("","generation")?;
    let epoch: i64 = row.try_get("","probe_epoch")?;
    lock.commit().await?;

    let started = Instant::now();
    for (deployment,revision) in &targets {
        let receipts = regional_observers::probe_candidate(state,db,service,operation,deployment,revision).await?;
        anyhow::ensure!(!receipts.is_empty() && receipts.iter().all(|r| r["request"]["generation"] == generation),
            "probe receipts no longer belong to staging generation");
    }
    let lock = service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    anyhow::ensure!(started.elapsed() <= Duration::from_secs(5), "regional probe set expired before staging");
    anyhow::ensure!(lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM regional_service_rollouts r JOIN service_active_regional_policies a USING(service_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running' AND r.phase=$3
         AND r.region_index=$4 AND r.slot_index=$5 AND a.generation=$6 AND r.probe_epoch=$7 FOR UPDATE OF r",
        vec![service.into(),operation.into(),phase.into(),region_index.into(),slot_index.into(),generation.into(),epoch.into()])).await?.is_some(),
        "application cursor, attempt or policy changed during probes");
    anyhow::ensure!(service_discovery::read_snapshot_in(&lock,service,true).await?.context("discovery disappeared")?.version == version,
        "membership changed during the regional probe set");
    anyhow::ensure!(regional_reports::ready(&lock,service,operation,generation,Gate::AdmissionDrained).await?,
        "staging requires fresh completed drain evidence");
    for (deployment,_) in &targets {
        service_discovery::cancel_endpoint_retirement(&lock,service,deployment).await?;
    }
    let ids = serde_json::json!(targets.iter().map(|(id,_)| id).collect::<Vec<_>>());
    lock.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE service_discovery_endpoints SET
         draining=NOT (deployment_id IN (SELECT jsonb_array_elements_text($3::jsonb))),
         health_status=CASE WHEN deployment_id IN (SELECT jsonb_array_elements_text($3::jsonb)) THEN 'healthy' ELSE health_status END,
         updated_at=clock_timestamp() WHERE service_id=$1 AND region=$2",
        vec![service.into(),region.into(),ids.into()])).await?;
    let version: i64 = lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE service_discovery_sets SET version=version+1,updated_at=clock_timestamp() WHERE service_id=$1 RETURNING version",
        [service.into()])).await?.context("discovery disappeared")?.try_get("","version")?;
    lock.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET phase=$2,slot_index=$3,discovery_version=$4,updated_at=clock_timestamp() WHERE operation_id=$1",
        vec![operation.into(),next.phase.clone().into(),i32::try_from(next.slot_index)?.into(),version.into()])).await?;
    lock.commit().await?;
    Ok(())
}

#[derive(Clone)]
struct ServingProbe {
    epoch: i64,
    step: super::regional_plan::Step,
    generation: i64,
    version: i64,
    targets: Vec<(String,String)>,
    membership_ready: bool,
}

/// Commit ownership before issuing HTTP. An expired/abandoned claim invalidates
/// the old healthy window even if the last good observation is still recent.
async fn claim_serving_probe(db: &sea_orm::DatabaseConnection, service: &str,
    operation: &str) -> Result<Option<ServingProbe>> {
    let lock = service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    let row = lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.*,a.generation,s.version,clock_timestamp() AS observed_now
         FROM regional_service_rollouts r JOIN service_active_regional_policies a USING(service_id)
         JOIN service_discovery_sets s USING(service_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running' AND r.phase IN ('bake','verify','verify_baseline') FOR UPDATE OF r",
        [service.into(),operation.into()])).await?.context("application has no serving verification item")?;
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    anyhow::ensure!(plan.version == 3, "serving verification requires an application plan");
    let phase: String = row.try_get("","phase")?;
    let step = plan.step(&phase,row.try_get::<i32>("","region_index")?.try_into()?,
        row.try_get::<i32>("","slot_index")?.try_into()?)?.clone();
    let generation: i64 = row.try_get("","generation")?;
    let version: i64 = row.try_get("","version")?;
    let now: chrono::DateTime<chrono::Utc> = row.try_get("","observed_now")?;
    let expires: Option<chrono::DateTime<chrono::Utc>> = row.try_get("","probe_expires_at")?;
    let same_scope = row.try_get::<Option<String>>("","probe_step_id")?.as_deref() == Some(&step.id)
        && row.try_get::<Option<i64>>("","probe_policy_generation")? == Some(generation)
        && row.try_get::<Option<i64>>("","probe_discovery_version")? == Some(version);
    if same_scope && expires.is_some_and(|t| t > now) {
        lock.rollback().await?;
        return Ok(None);
    }
    let regions: Vec<String> = if phase == "bake" {vec![step.region.clone().context("bake has no region")?]}
        else {serde_json::from_value(row.try_get("","regions")?)?};
    let mut members: Vec<(String,String,String)> = if step.slot_index == 3 {
        let baseline: Value = row.try_get("","baseline_state")?;
        let retained: Vec<PinnedEndpoint> = serde_json::from_value(
            baseline.get("regionalRetained").context("rollback has no retained baseline")?.clone())?;
        retained.into_iter().map(|e| (e.deployment_id,e.revision,e.region)).collect()
    } else {
        let input: Value = row.try_get("","deployment_request")?;
        let revision = input["expectedRuntimeRevision"].as_str().filter(|s| !s.is_empty()).context("application revision missing")?;
        plan.steps.iter().filter(|s| s.phase == "create_candidate")
            .map(|s| Ok((s.candidate_id.clone().context("candidate identity missing")?,revision.to_owned(),
                s.region.clone().context("candidate region missing")?))).collect::<Result<_>>()?
    };
    members.retain(|(_,_,region)| regions.contains(region));
    let minimum: i32 = row.try_get("","minimum_serving_replicas")?;
    anyhow::ensure!(!regions.is_empty() && minimum > 0
        && regions.iter().all(|r| members.iter().filter(|(_,_,region)| region == r).count() >= minimum as usize)
        && members.iter().map(|(id,_,_)| id).collect::<HashSet<_>>().len() == members.len(), "verification lacks distinct regional capacity");
    let snapshot = service_discovery::read_snapshot_in(&lock,service,true).await?.context("discovery missing")?;
    let membership_ready = snapshot.endpoints.iter().filter(|e| e.region.as_ref().is_some_and(|r| regions.contains(r))
        && e.health_status == "healthy" && !e.draining).all(|e| members.iter().any(|(id,revision,region)|
            id == &e.deployment_id && Some(revision) == e.revision.as_ref() && Some(region) == e.region.as_ref()));
    let last: Option<chrono::DateTime<chrono::Utc>> = row.try_get("","last_observed_at")?;
    let reset = !same_scope || expires.is_some() || last.is_none_or(|t| t > now || now-t > chrono::Duration::seconds(5));
    let epoch: i64 = lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET probe_epoch=probe_epoch+1,probe_step_id=$2,probe_policy_generation=$3,
         probe_discovery_version=$4,probe_expires_at=clock_timestamp()+interval '5 seconds',
         deadline_at=CASE WHEN $5 THEN NULL ELSE deadline_at END,last_observed_at=CASE WHEN $5 THEN NULL ELSE last_observed_at END
         WHERE operation_id=$1 RETURNING probe_epoch",
        vec![operation.into(),step.id.clone().into(),generation.into(),version.into(),reset.into()])).await?
        .context("application disappeared")?.try_get("","probe_epoch")?;
    lock.commit().await?;
    Ok(Some(ServingProbe {epoch,step,generation,version,targets:members.into_iter().map(|(id,revision,_)| (id,revision)).collect(),membership_ready}))
}

/// Final service bookkeeping shares the verification/journal transaction. The
/// stable ingress and route stay pinned; no host-local URL becomes an ingress.
async fn complete_service_state(db: &sea_orm::DatabaseTransaction, service: &str, operation: &str,
    row: &sea_orm::QueryResult, plan: &Plan, rollback: bool) -> Result<()> {
    let baseline: Value = row.try_get("","baseline_state")?;
    let retained: Vec<PinnedEndpoint> = serde_json::from_value(baseline.get("regionalRetained").context("retained baseline missing")?.clone())?;
    let mut completed: service_deploy::ServiceDeploymentState = serde_json::from_value(baseline)?;
    anyhow::ensure!(completed.service_id == service && completed.route.is_some()
        && completed.ingress_backend_url.as_ref().is_some_and(|url| !url.is_empty()), "application has no pinned service ingress baseline");
    if !rollback {
        let request: service_deploy::ServiceDeployRequest = serde_json::from_value(row.try_get("","deployment_request")?)?;
        anyhow::ensure!(request.service_id == service && request.deployment_environment == completed.deployment_environment,
            "application completion identity differs from its baseline");
        let mut replicas = Vec::new();
        let mut regions = Vec::new();
        for step in plan.steps.iter().filter(|s| s.phase == "create_candidate") {
            let creation = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
                "SELECT intent,receipt FROM regional_candidate_creations WHERE operation_id=$1 AND step_id=$2",
                [operation.into(),step.id.clone().into()])).await?.context("completion candidate missing")?;
            let intent: super::regional_candidates::Intent = serde_json::from_value(creation.try_get("","intent")?)?;
            let binding: super::regional_candidates::Binding = serde_json::from_value(creation.try_get("","receipt")?)?;
            anyhow::ensure!(step.candidate_id.as_deref() == Some(&intent.deployment_id)
                && step.region.as_deref() == Some(&intent.region)
                && intent.archive_sha256 == row.try_get::<String>("","target_revision")?, "completion candidate differs from the pinned plan");
            replicas.push(serde_json::json!({"deploymentId":intent.deployment_id,"region":intent.region,
                "revision":intent.runtime_revision,"archiveId":binding.archive_id,"backendServerId":binding.backend_server_id,
                "backendSandboxId":binding.backend_sandbox_id,"backendUrl":binding.host_local_url}));
            regions.push(intent.region);
        }
        let primary = replicas.first().context("application has no completed candidates")?;
        completed.previous_deployment_id = completed.active_deployment_id.take();
        completed.previous_archive_id = completed.active_archive_id.take();
        completed.previous_metadata = Some(completed.active_metadata.clone());
        completed.active_deployment_id = Some(primary["deploymentId"].as_str().unwrap().into());
        completed.active_archive_id = Some(primary["archiveId"].as_str().unwrap().into());
        completed.active_backend_url = Some(primary["backendUrl"].as_str().unwrap().into());
        completed.desired_replicas = replicas.len().try_into()?;
        completed.replica_regions = regions;
        completed.active_metadata = serde_json::json!({"regionalOperationId":operation,
            "deploymentId":completed.active_deployment_id,"archiveId":completed.active_archive_id,
            "archiveSha256":row.try_get::<String>("","target_revision")?,"source":request.metadata,
            "runtime":{"healthPath":request.health_path,"replicaRegions":completed.replica_regions},"regionalReplicas":replicas});
    }
    // Releasing regional ownership must not revive a historical cleanup intent
    // for the retained baseline, including after a successful forward rollout.
    for endpoint in retained {
        service_discovery::cancel_endpoint_retirement(db,service,&endpoint.deployment_id).await?;
    }
    service_deploy::write_service_state_in(db,&completed).await
}

/// Both success and failure are epoch-fenced. A stale failure cannot reset a
/// newer worker's interval, and an unresolved claim cannot be mistaken for zero
/// work after restart. Claim resolution and journal progress share one commit.
async fn finish_serving_probe(db: &sea_orm::DatabaseConnection, service: &str, operation: &str,
    claim: &ServingProbe, healthy: bool) -> Result<bool> {
    let lock = service_deploy::try_service_lifecycle_lock(db,service).await?.context("service lifecycle busy")?;
    let row = lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.*,a.generation,s.version,clock_timestamp() AS observed_now
         FROM regional_service_rollouts r JOIN service_active_regional_policies a USING(service_id)
         JOIN service_discovery_sets s USING(service_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running'
         AND r.region_index || ':' || r.phase || ':' || r.slot_index=$3 AND r.probe_step_id=$3
         AND r.probe_epoch=$4 AND r.probe_expires_at IS NOT NULL FOR UPDATE OF r",
        vec![service.into(),operation.into(),claim.step.id.clone().into(),claim.epoch.into()])).await?;
    let Some(row) = row else {lock.rollback().await?;return Ok(false)};
    let now: chrono::DateTime<chrono::Utc> = row.try_get("","observed_now")?;
    let valid = healthy && claim.membership_ready && row.try_get::<chrono::DateTime<chrono::Utc>>("","probe_expires_at")? > now
        && row.try_get::<i64>("","generation")? == claim.generation && row.try_get::<i64>("","version")? == claim.version
        && regional_reports::ready(&lock,service,operation,claim.generation,Gate::Adopted).await?;
    if !valid {
        lock.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE regional_service_rollouts SET probe_expires_at=NULL,deadline_at=NULL,last_observed_at=NULL,
             error_message='serving probe failed or evidence expired',updated_at=clock_timestamp() WHERE operation_id=$1",
            [operation.into()])).await?;
        lock.commit().await?;
        return Ok(false);
    }
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    let last: Option<chrono::DateTime<chrono::Utc>> = row.try_get("","last_observed_at")?;
    let deadline: Option<chrono::DateTime<chrono::Utc>> = row.try_get("","deadline_at")?;
    let continuous = last.is_some_and(|t| t <= now && now-t <= chrono::Duration::seconds(5)) && deadline.is_some();
    let advance = claim.step.phase != "bake" || (continuous && deadline.is_some_and(|t| now >= t));
    if advance {
        let next = plan.successor(&claim.step.id)?;
        let status = match next.phase.as_str() {"passed" => "passed", "rolled_back" => "rolled_back", _ => "running"};
        if status != "running" {
            complete_service_state(&lock,service,operation,&row,&plan,status == "rolled_back").await?;
        }
        let version: i64 = lock.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE service_discovery_sets SET version=version+1,updated_at=clock_timestamp() WHERE service_id=$1 RETURNING version",
            [service.into()])).await?.context("discovery disappeared")?.try_get("","version")?;
        lock.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE regional_service_rollouts SET phase=$2,region_index=$3,slot_index=$4,status=$5,probe_expires_at=NULL,
             deadline_at=NULL,last_observed_at=NULL,error_message=NULL,discovery_version=$6,updated_at=clock_timestamp(),
             completed_at=CASE WHEN $5='running' THEN NULL ELSE clock_timestamp() END WHERE operation_id=$1",
            vec![operation.into(),next.phase.clone().into(),i32::try_from(next.region_index)?.into(),
                i32::try_from(next.slot_index)?.into(),status.into(),version.into()])).await?;
    } else {
        let deadline = if continuous {deadline.unwrap()} else {now + chrono::Duration::seconds(row.try_get("","bake_seconds")?)};
        lock.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE regional_service_rollouts SET probe_expires_at=NULL,deadline_at=$2,last_observed_at=$3,
             error_message=NULL,updated_at=clock_timestamp() WHERE operation_id=$1",
            vec![operation.into(),deadline.into(),now.into()])).await?;
    }
    lock.commit().await?;
    Ok(advance)
}

pub(super) async fn verify_serving(state: &crate::AppState, db: &sea_orm::DatabaseConnection,
    service: &str, operation: &str) -> Result<bool> {
    let Some(claim) = claim_serving_probe(db,service,operation).await? else {return Ok(false)};
    // This bounds the whole set, including credentials, report polls and DB
    // revalidation. Cancellation is not a fence; the durable epoch is.
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        anyhow::ensure!(claim.membership_ready, "unexpected eligible members in verification scope");
        for (deployment,revision) in &claim.targets {
            let receipts = regional_observers::probe_candidate(state,db,service,operation,deployment,revision).await?;
            anyhow::ensure!(!receipts.is_empty() && receipts.iter().all(|r| r["request"]["generation"] == claim.generation),
                "serving receipts belong to another generation");
        }
        Ok::<_,anyhow::Error>(())
    }).await.context("serving probe set timed out").and_then(|r| r);
    let advanced = finish_serving_probe(db,service,operation,&claim,result.is_ok()).await?;
    result?;
    Ok(advanced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preflight_requires_complete_exact_boot_pinned_receipts() -> Result<()> {
        let plan = Plan::application(&["eu1".into(),"us3".into()],&[("eu1".into(),"new-eu".into()),("us3".into(),"new-us".into())])?;
        let fleet = serde_json::from_value(json!([
            {"participant":{"gatewayId":"eu","region":"eu1","bootId":"eu-boot"},"observer":{},"admission":{"backendServerId":"host-eu"}},
            {"participant":{"gatewayId":"us","region":"us3","bootId":"us-boot"},"observer":{},"admission":{"backendServerId":"host-us"}}
        ]))?;
        let claim = PreflightClaim {service_id:"svc".into(),operation_id:"op".into(),step:plan.step("preflight",0,0)?.clone(),
            epoch:7,generation:13,version:19,challenge:"fresh-challenge".into(),fleet,
            targets:vec![PinnedEndpoint {deployment_id:"old-us".into(),backend_server_id:"host-us".into(),
                region:"us3".into(),revision:"rev-1".into(),url:"http://127.0.0.1:9081".into()}]};
        // Construct wire evidence independently of PreflightClaim::request.
        let request = json!({"operationId":"op","stepId":"0:preflight:0","epoch":7,"challenge":"fresh-challenge",
            "generation":13,"version":19,"region":"us3","gatewayId":"us","gatewayBootId":"us-boot",
            "backendServerId":"host-us","deploymentId":"old-us","revision":"rev-1"});
        let receipts = Value::Array([("eu","eu-boot"),("us","us-boot")].into_iter().map(|(id,boot)| json!({
            "request":request,"sourceGatewayId":id,"sourceBootId":boot,"destination":{"request":request,
            "gatewayId":"us","gatewayBootId":"us-boot","backendServerId":"host-us","backendUrl":"http://127.0.0.1:9081"}})).collect());
        assert!(claim.accepts(&receipts));
        assert!(!claim.accepts(&json!([])));
        assert!(!claim.accepts(&json!([receipts[0]])), "every source must observe the survivor");
        assert!(!claim.accepts(&json!([receipts[0],receipts[0]])), "duplicates cannot replace a missing source");
        assert!(!claim.accepts(&json!([receipts[0],receipts[1],receipts[0]])), "extra evidence is rejected");
        for (path,value) in [
            ("/0/request/epoch",json!(6)),("/0/request/challenge",json!("replayed")),
            ("/0/request/generation",json!(12)),("/0/request/version",json!(18)),
            ("/0/sourceBootId",json!("restarted")),("/0/sourceGatewayId",json!("untracked")),
            ("/0/destination/gatewayBootId",json!("restarted")),("/0/destination/gatewayId",json!("eu")),
            ("/0/destination/backendServerId",json!("host-eu")),("/0/destination/backendUrl",json!("http://127.0.0.1:9082")),
            ("/0/destination/request/revision",json!("rev-2")),("/0/destination/request/deploymentId",json!("new-us")),
        ] {
            let mut wrong = receipts.clone();
            *wrong.pointer_mut(path).unwrap() = value;
            assert!(!claim.accepts(&wrong), "accepted changed {path}");
        }
        Ok(())
    }

    #[test]
    fn application_publications_preserve_pinned_weights_and_never_restore_other_regions() -> Result<()> {
        let baseline: super::super::regional_policy::RegionalPolicy = serde_json::from_value(json!({"version":1,"regions":[
            {"region":"eu1","weight":3,"gateways":[{"id":"eu","backendServerId":"host-eu","url":"https://eu.example"}]},
            {"region":"us3","weight":11,"gateways":[{"id":"us","backendServerId":"host-us","url":"https://us.example"}]}
        ]}))?;
        let plan = Plan::application(&["us3".into(),"eu1".into()],&[("eu1".into(),"new-eu".into()),("us3".into(),"new-us".into())])?;
        for (withdraw,restore) in [(0,1),(2,3)] {
            let target = plan.step("publish_policy",0,withdraw)?;
            let withdrawn = publication_policy(&baseline,&baseline,&plan,target)?;
            assert_eq!(withdrawn.regions[0],baseline.regions[0]);
            assert_eq!(withdrawn.regions[1].weight,0);
            assert_eq!(withdrawn.regions[1].gateways,baseline.regions[1].gateways);
            assert_eq!(publication_policy(&baseline,&withdrawn,&plan,plan.step("publish_policy",0,restore)?)?,baseline);
            let mut wrong = baseline.clone(); wrong.regions[0].weight=0;
            assert!(publication_policy(&baseline,&wrong,&plan,target).is_err(), "must not restore another drained region");
            wrong = baseline.clone(); wrong.regions[1].weight=7;
            assert!(publication_policy(&baseline,&wrong,&plan,target).is_err(), "must not adopt edited active weights");
            wrong = baseline.clone(); wrong.regions[1].gateways[0].url="https://replacement.example".into();
            assert!(publication_policy(&baseline,&wrong,&plan,target).is_err(), "must not rebind a gateway");
            wrong = baseline.clone(); wrong.regions[1].weight=0;
            assert!(publication_policy(&wrong,&wrong,&plan,target).is_err(), "must not invent a restore weight");
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ORCHESTRATOR_TEST_DATABASE_URL (disposable PostgreSQL)"]
    async fn bake_epochs_fence_late_outcomes_and_reset_unknown_windows_postgres() -> Result<()> {
        let url = std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let root = sea_orm::Database::connect(url.clone()).await?;
        let schema = format!("bake_epochs_{}",uuid::Uuid::new_v4().simple());
        root.execute_unprepared(&format!("CREATE SCHEMA {schema}")).await?;
        let mut options = sea_orm::ConnectOptions::new(url);
        options.set_schema_search_path(schema.clone());
        let db = sea_orm::Database::connect(options.clone()).await?;
        let restarted = sea_orm::Database::connect(options).await?;
        for migration in [include_str!("../../migrations/028_add_service_deployment_state.sql"),
            include_str!("../../migrations/031_add_service_discovery.sql"),include_str!("../../migrations/032_add_service_rollout_state.sql"),
            include_str!("../../migrations/033_add_service_replica_placement.sql"),include_str!("../../migrations/035_add_regional_service_rollouts.sql"),
            include_str!("../../migrations/037_add_regional_routing_policy.sql"),include_str!("../../migrations/038_add_regional_policy_proposals.sql"),
            include_str!("../../migrations/039_add_regional_candidate_receipts.sql"),include_str!("../../migrations/040_add_application_plan_journal.sql"),
            include_str!("../../migrations/041_add_application_probe_claims.sql")] {db.execute_unprepared(migration).await?;}
        let plan = Plan::application(&["eu1".into(),"us3".into()],&[("eu1".into(),"eu-new".into()),("us3".into(),"us-new".into())])?;
        let participants: Vec<_> = ["eu1","us3"].into_iter().map(|region| regional_reports::Participant {
            gateway_id:region.into(),region:region.into(),boot_id:uuid::Uuid::new_v4().to_string()}).collect();
        db.execute_unprepared("INSERT INTO service_discovery_sets(service_id,version,policy_generation) VALUES('bake-epoch',1,1);
            INSERT INTO service_discovery_endpoints(service_id,deployment_id,backend_server_id,region,revision,backend_url,health_status)
            VALUES('bake-epoch','eu-new','eu1','eu1','new','http://127.0.0.1:18081','healthy')").await?;
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,
             baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
             VALUES('op','bake-epoch','hash','{\"expectedRuntimeRevision\":\"new\"}','archive',$1,'{}','[\"eu1\",\"us3\"]','[]',$2,1,10,30,'running','preflight')",
            vec![serde_json::to_string(&participants)?.into(),serde_json::to_value(&plan)?.into()])).await?;
        // This fixture isolates the lease protocol, not deployment execution.
        // Advance the journal and seed the already-adopted restoration policy.
        for step in plan.steps.iter().skip(1).take_while(|s| s.region_index == 0) {
            db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
                "UPDATE regional_service_rollouts SET phase=$1,slot_index=$2 WHERE operation_id='op'",
                vec![step.phase.clone().into(),i32::try_from(step.slot_index)?.into()])).await?;
        }
        let policy = json!({"version":1,"regions":[
            {"region":"eu1","weight":3,"gateways":[{"id":"eu1","backendServerId":"eu1","url":"https://eu.example"}]},
            {"region":"us3","weight":7,"gateways":[{"id":"us3","backendServerId":"us3","url":"https://us.example"}]}]});
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_policy_proposals(service_id,generation,operation_id,step_id,policy)
             VALUES('bake-epoch',1,'op','0:publish_policy:1',$1)",[policy.into()])).await?;
        db.execute_unprepared("INSERT INTO service_active_regional_policies(service_id,generation) VALUES('bake-epoch',1)").await?;
        for participant in &participants {
            regional_reports::record(&db,"bake-epoch","op",&regional_reports::Report {gateway_id:participant.gateway_id.clone(),
                boot_id:participant.boot_id.clone(),sequence:1,generation:1,prepared:true,adopted:true,
                outgoing_target:0,local_target:0,peer_admission_closed:false},0).await?;
        }
        // Bake gets its ten-second interval plus the thirty-second failure
        // budget. Controller replacement must not restart that budget.
        db.execute_unprepared("UPDATE regional_service_rollouts SET attempt_started_at=clock_timestamp()-interval '35 seconds' WHERE operation_id='op'").await?;
        assert_eq!(dispatch_phase(&restarted,"bake-epoch","op").await?,Some("bake".into()));
        let timed_out = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        db.execute_unprepared("UPDATE regional_service_rollouts SET attempt_started_at=clock_timestamp()-interval '41 seconds' WHERE operation_id='op'").await?;
        assert!(dispatch_phase(&restarted,"bake-epoch","op").await?.is_none());
        assert!(db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT 1 FROM regional_service_rollouts WHERE operation_id='op' AND phase='bake' AND status='blocked'
             AND probe_expires_at IS NULL AND error_message='application phase timed out: bake'")).await?.is_some());
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&timed_out,true).await?,"timeout fences in-flight completion");
        db.execute_unprepared("UPDATE regional_service_rollouts SET status='running',error_message=NULL WHERE operation_id='op' AND status='blocked'").await?;
        assert_eq!(dispatch_phase(&db,"bake-epoch","op").await?,Some("bake".into()),"explicit resume grants a new attempt without moving the cursor");
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&timed_out,true).await?,"resume cannot revive the old claim");
        let first = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&first,true).await?);
        let abandoned = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        assert!(claim_serving_probe(&restarted,"bake-epoch","op").await?.is_none());
        db.execute_unprepared("UPDATE regional_service_rollouts SET probe_expires_at=clock_timestamp()-interval '1 second',
            deadline_at=clock_timestamp()-interval '1 second',last_observed_at=clock_timestamp() WHERE operation_id='op'").await?;
        let replacement = claim_serving_probe(&restarted,"bake-epoch","op").await?.unwrap();
        assert!(replacement.epoch > abandoned.epoch);
        assert!(db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT 1 FROM regional_service_rollouts WHERE operation_id='op' AND deadline_at IS NULL AND last_observed_at IS NULL
             AND probe_expires_at IS NOT NULL")).await?.is_some(), "unknown epoch resets before replacement probes");
        for healthy in [true,false] {
            assert!(!finish_serving_probe(&db,"bake-epoch","op",&abandoned,healthy).await?, "late outcome must be fenced");
        }
        assert!(!finish_serving_probe(&restarted,"bake-epoch","op",&replacement,true).await?,"expired prior deadline cannot finish a fresh interval");
        let observed = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT deadline_at FROM regional_service_rollouts WHERE operation_id='op'")).await?.unwrap()
            .try_get::<chrono::DateTime<chrono::Utc>>("","deadline_at")?;
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&abandoned,false).await?);
        assert_eq!(observed,db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT deadline_at FROM regional_service_rollouts WHERE operation_id='op'")).await?.unwrap().try_get::<chrono::DateTime<chrono::Utc>>("","deadline_at")?);
        let changed = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        db.execute_unprepared("UPDATE service_discovery_sets SET version=version+1 WHERE service_id='bake-epoch'").await?;
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&changed,true).await?,"changed membership invalidates successful probes");
        let failed = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&failed,false).await?);
        let expired = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        db.execute_unprepared("UPDATE regional_service_rollouts SET probe_expires_at=clock_timestamp()-interval '1 second' WHERE operation_id='op'").await?;
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&expired,true).await?,"expired healthy evidence cannot start a window");
        let valid = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&valid,true).await?);
        db.execute_unprepared("UPDATE regional_service_rollouts SET deadline_at=clock_timestamp()-interval '1 second',
            last_observed_at=clock_timestamp()-interval '6 seconds' WHERE operation_id='op'").await?;
        let gap = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        assert!(!finish_serving_probe(&db,"bake-epoch","op",&gap,true).await?,"unobserved time cannot finish bake");
        db.execute_unprepared("UPDATE regional_service_rollouts SET deadline_at=clock_timestamp()-interval '1 second',
            last_observed_at=clock_timestamp() WHERE operation_id='op'").await?;
        let complete = claim_serving_probe(&db,"bake-epoch","op").await?.unwrap();
        db.execute_unprepared("CREATE FUNCTION fail_bake_journal() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected bake failure'; END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER fail_bake_journal BEFORE INSERT ON regional_rollout_events FOR EACH ROW EXECUTE FUNCTION fail_bake_journal()").await?;
        assert!(finish_serving_probe(&db,"bake-epoch","op",&complete,true).await.is_err());
        assert!(claim_serving_probe(&restarted,"bake-epoch","op").await?.is_none(),"failed completion commit leaves ownership unresolved");
        db.execute_unprepared("DROP TRIGGER fail_bake_journal ON regional_rollout_events").await?;
        assert!(finish_serving_probe(&db,"bake-epoch","op",&complete,true).await?);
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT phase,region_index,deadline_at,last_observed_at,probe_expires_at FROM regional_service_rollouts WHERE operation_id='op'")).await?.unwrap();
        assert_eq!(row.try_get::<String>("","phase")?,"preflight");
        assert_eq!(row.try_get::<i32>("","region_index")?,1);
        for field in ["deadline_at","last_observed_at","probe_expires_at"] {
            assert!(row.try_get::<Option<chrono::DateTime<chrono::Utc>>>("",field)?.is_none(),"new item inherited {field}");
        }
        assert!(!finish_serving_probe(&restarted,"bake-epoch","op",&complete,false).await?);
        // Failure in the second region must enter its rollback suffix, not
        // restart region zero. Repeated rollback requests are read-only.
        db.execute_unprepared("UPDATE regional_service_rollouts SET status='blocked',error_message='injected failure' WHERE operation_id='op'").await?;
        assert!(begin_rollback(&db,"different-service","op").await.is_err());
        assert_eq!(begin_rollback(&restarted,"bake-epoch","op").await?,"rollback_entry");
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT region_index,slot_index,status,probe_expires_at FROM regional_service_rollouts WHERE operation_id='op'")).await?.unwrap();
        assert_eq!(row.try_get::<i32>("","region_index")?,1);
        assert_eq!(row.try_get::<i32>("","slot_index")?,2);
        assert_eq!(row.try_get::<String>("","status")?,"running");
        assert!(row.try_get::<Option<chrono::DateTime<chrono::Utc>>>("","probe_expires_at")?.is_none());
        db.execute_unprepared("UPDATE regional_service_rollouts SET phase='publish_policy' WHERE operation_id='op';
            UPDATE regional_service_rollouts SET status='blocked' WHERE operation_id='op'").await?;
        for _ in 0..2 {assert_eq!(begin_rollback(&db,"bake-epoch","op").await?,"publish_policy");}
        assert!(db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT 1 FROM regional_service_rollouts WHERE operation_id='op' AND status='blocked' AND region_index=1 AND slot_index=2")).await?.is_some(),
            "retry cannot resume or rewind rollback");
        assert!(dispatch_phase(&restarted,"bake-epoch","op").await?.is_none());
        db.execute_unprepared("UPDATE regional_service_rollouts SET status='running' WHERE operation_id='op';
            UPDATE regional_service_rollouts SET attempt_started_at=clock_timestamp()-interval '31 seconds' WHERE operation_id='op'").await?;
        assert!(dispatch_phase(&db,"bake-epoch","op").await?.is_none(),"publication has no bake extension");
        db.close().await?;
        restarted.close().await?;
        root.execute_unprepared(&format!("DROP SCHEMA {schema} CASCADE")).await?;
        Ok(())
    }
}
