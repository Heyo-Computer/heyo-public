//! Persisted regional routing intent; gateway adoption is a separate gate.
use anyhow::{Context, Result};
use axum::{extract::{Path, State}, http::{HeaderMap, StatusCode}, Json};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

use crate::{auth, db, AppState};
use super::service_deploy;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegionalPolicy {
    pub version: u32,
    pub regions: Vec<Region>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Region {
    pub region: String,
    /// Explicit relative weight, independent of the number of VMs or gateways.
    pub weight: u32,
    pub gateways: Vec<Gateway>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Gateway {
    pub id: String,
    pub backend_server_id: String,
    /// Transport authority; application Host is carried independently.
    pub url: String,
}

impl RegionalPolicy {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.version == 1, "unsupported regional policy version");
        anyhow::ensure!(!self.regions.is_empty(), "regional policy has no regions");
        let mut regions = HashSet::new();
        let mut gateways = HashSet::new();
        let mut addresses = HashSet::new();
        let mut total = 0u64;
        for region in &self.regions {
            validate_id(&region.region)?;
            anyhow::ensure!(regions.insert(&region.region), "duplicate region");
            anyhow::ensure!(!region.gateways.is_empty(), "region has no gateway inventory");
            total += u64::from(region.weight);
            for gateway in &region.gateways {
                validate_id(&gateway.id)?;
                validate_id(&gateway.backend_server_id)?;
                anyhow::ensure!(gateways.insert(&gateway.id), "duplicate gateway identity");
                let url = reqwest::Url::parse(&gateway.url).context("invalid gateway URL")?;
                anyhow::ensure!(url.scheme() == "https" && url.domain().is_some()
                    && url.username().is_empty() && url.password().is_none()
                    && url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
                    "gateway URL must be pathless credential-free HTTPS with a DNS hostname");
                anyhow::ensure!(addresses.insert(url.to_string()), "duplicate gateway address");
            }
        }
        anyhow::ensure!(total > 0, "regional policy must assign positive serving weight");
        Ok(())
    }
}

fn validate_id(value: &str) -> Result<()> {
    anyhow::ensure!(!value.is_empty() && value.len() <= 128
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "region, gateway and backend identities must be bounded identifiers");
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PutPolicy {
    pub expected_version: u64,
    /// Explicit null removes desired policy without resetting the generation.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub policy: Option<RegionalPolicy>,
}

pub async fn put(
    headers: HeaderMap, State(state): State<AppState>, Path(service_id): Path<String>,
    Json(request): Json<PutPolicy>,
) -> (StatusCode, Json<Value>) {
    if let Err(status) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return (status, Json(json!({"error":"Unauthorized"})));
    }
    if service_id.is_empty() || !service_id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"service ID must be canonical lowercase letters, digits and hyphens"})));
    }
    if let Some(policy) = &request.policy {
        if let Err(error) = policy.validate() {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":error.to_string()})));
        }
    }
    if i64::try_from(request.expected_version).is_err() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"expectedVersion exceeds database range"})));
    }
    let result = async { store(db::get_db()?, &service_id, &request).await }.await;
    match result {
        Ok(Some(version)) => (StatusCode::OK, Json(json!({"serviceId":service_id,"version":version,"regionalPolicy":request.policy}))),
        Ok(None) => (StatusCode::CONFLICT, Json(json!({"error":"service lifecycle busy or discovery version changed"}))),
        Err(error) => {
            tracing::warn!(%service_id, %error, "regional policy write failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"regional policy write failed"})))
        }
    }
}

/// Legacy executors cannot interpret regional policy or attest its drain gates.
/// Call while holding the service lifecycle lock, before any deployment effect.
pub(super) async fn require_legacy_topology(db: &impl ConnectionTrait, service_id: &str) -> Result<()> {
    let configured = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM service_discovery_sets WHERE service_id=$1 AND regional_policy IS NOT NULL
         UNION ALL SELECT 1 FROM service_active_regional_policies WHERE service_id=$1",
        [service_id.into()])).await?.is_some();
    anyhow::ensure!(!configured, "service has regional routing policy; legacy direct-backend rollout cannot execute it");
    Ok(())
}

pub(super) async fn store(db: &sea_orm::DatabaseConnection, service_id: &str, request: &PutPolicy) -> Result<Option<u64>> {
    let Some(tx) = service_deploy::try_service_lifecycle_lock(db, service_id).await? else { return Ok(None); };
    if tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM regional_service_rollouts WHERE service_id=$1 AND status IN ('running','blocked')
         UNION ALL SELECT 1 FROM service_rollouts WHERE service_id=$1 AND status='running'
         UNION ALL SELECT 1 FROM service_active_regional_policies WHERE service_id=$1 AND $2",
        vec![service_id.into(), request.policy.is_none().into()])).await?.is_some() {
        return Ok(None);
    }
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO service_discovery_sets(service_id) VALUES($1) ON CONFLICT DO NOTHING", [service_id.into()])).await?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE service_discovery_sets SET regional_policy=$3, version=version+1, updated_at=NOW()
         WHERE service_id=$1 AND version=$2 RETURNING version",
        vec![service_id.into(), i64::try_from(request.expected_version)?.into(), request.policy.as_ref().map(serde_json::to_value).transpose()?.into()])).await?;
    let Some(row) = row else { return Ok(None); };
    let version: i64 = row.try_get("", "version")?;
    tx.commit().await?;
    Ok(Some(u64::try_from(version)?))
}

/// Storage primitive for the hierarchical executor, not a public activation API.
/// Proposal publication and cursor advancement share the lifecycle transaction.
pub(super) async fn publish_proposal(
    db: &sea_orm::DatabaseConnection, service_id: &str, operation: &str,
    policy: &RegionalPolicy, expected_predecessor: Option<i64>,
) -> Result<i64> {
    let tx = service_deploy::try_service_lifecycle_lock(db, service_id).await?
        .context("service lifecycle busy")?;
    let generation = publish_proposal_in(&tx, service_id, operation, "0:publish_policy:0", policy, expected_predecessor).await?;
    tx.commit().await?;
    Ok(generation)
}

pub(super) async fn publish_proposal_in(
    tx: &sea_orm::DatabaseTransaction, service_id: &str, operation: &str,
    step_id: &str, policy: &RegionalPolicy, expected_predecessor: Option<i64>,
) -> Result<i64> {
    policy.validate()?;
    let stored = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT generation, policy, expected_predecessor FROM regional_policy_proposals
         WHERE service_id=$1 AND operation_id=$2 AND step_id=$3",
        [service_id.into(), operation.into(), step_id.into()])).await?;
    let value = serde_json::to_value(policy)?;
    if let Some(stored) = stored {
        anyhow::ensure!(stored.try_get::<Value>("", "policy")? == value
            && stored.try_get::<Option<i64>>("", "expected_predecessor")? == expected_predecessor,
            "operation proposal retry has different intent");
        return Ok(stored.try_get("", "generation")?);
    }
    let owner = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT plan,observer_topology FROM regional_service_rollouts WHERE service_id=$1 AND operation_id=$2
         AND status='running' AND phase='publish_policy'
         AND region_index || ':' || phase || ':' || slot_index=$3 FOR UPDATE",
        [service_id.into(), operation.into(), step_id.into()])).await?.context("operation does not own policy publication")?;
    let plan: super::regional_plan::Plan = serde_json::from_value(owner.try_get("", "plan")?)?;
    let target = plan.publication(step_id)?.region.as_deref();
    if let Some(target) = target {
        anyhow::ensure!(policy.regions.iter().any(|r| r.region == target && r.weight == 0),
            "a drain proposal must retain the target inventory with zero serving weight");
    }
    let participants = serde_json::from_str::<Vec<super::regional_reports::Participant>>(
        &owner.try_get::<String>("", "observer_topology")?)?;
    let predecessor = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT policy FROM regional_policy_proposals WHERE service_id=$1 AND generation=$2",
        vec![service_id.into(), expected_predecessor.into()])).await?
        .map(|r| -> Result<RegionalPolicy> { Ok(serde_json::from_value(r.try_get("", "policy")?)?) })
        .transpose()?;
    super::regional_reports::validate_participants(&participants, policy, predecessor.as_ref())?;
    if let Some(predecessor) = &predecessor {
        for old in &predecessor.regions {
            if old.weight > 0 && !policy.regions.iter().any(|r| r.region == old.region && r.weight > 0) {
                anyhow::ensure!(target == Some(old.region.as_str()), "withdrawing a serving region requires its drain plan");
            }
        }
    }
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE service_discovery_sets s SET policy_generation=policy_generation+1,version=version+1,updated_at=NOW()
         WHERE service_id=$1 AND (SELECT generation FROM service_active_regional_policies a
             WHERE a.service_id=s.service_id) IS NOT DISTINCT FROM $2::bigint RETURNING policy_generation",
        vec![service_id.into(), expected_predecessor.into()])).await?.context("active policy predecessor changed")?;
    let generation: i64 = row.try_get("", "policy_generation")?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO regional_policy_proposals(service_id,generation,operation_id,step_id,expected_predecessor,policy)
         VALUES($1,$2,$3,$4,$5,$6)",
        vec![service_id.into(), generation.into(), operation.into(), step_id.into(), expected_predecessor.into(), value.into()])).await?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET phase='wait_policy_prepared',updated_at=NOW() WHERE operation_id=$1",
        [operation.into()])).await?;
    Ok(generation)
}

/// Invoke only after the executor has durably completed the preparation gate.
/// The pointer CAS and item journal commit together; replay is a read-only receipt.
/// This is intentionally not wired to HTTP or the legacy reconciler.
pub(super) async fn activate_proposal(
    db: &sea_orm::DatabaseConnection, service_id: &str, operation: &str, generation: i64,
) -> Result<()> {
    let tx = service_deploy::try_service_lifecycle_lock(db, service_id).await?
        .context("service lifecycle busy")?;
    match activate_proposal_in(&tx, service_id, operation, generation).await {
        Ok(()) => { tx.commit().await?; Ok(()) }
        Err(error) => { tx.rollback().await?; Err(error) }
    }
}

async fn activate_proposal_in(
    tx: &sea_orm::DatabaseTransaction, service_id: &str, operation: &str, generation: i64,
) -> Result<()> {
    let proposal = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT p.expected_predecessor,p.step_id,r.plan FROM regional_policy_proposals p
         JOIN regional_service_rollouts r USING(service_id,operation_id)
         WHERE p.service_id=$1 AND p.operation_id=$2 AND p.generation=$3",
        vec![service_id.into(), operation.into(), generation.into()])).await?.context("proposal does not belong to operation")?;
    let plan: super::regional_plan::Plan = serde_json::from_value(proposal.try_get("", "plan")?)?;
    let publication = plan.publication(&proposal.try_get::<String>("", "step_id")?)?;
    let activation = plan.step("activate_policy", publication.region_index, publication.slot_index)?;
    let preparation = plan.step("wait_policy_prepared", publication.region_index, publication.slot_index)?;
    let active = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT generation FROM service_active_regional_policies WHERE service_id=$1",
        [service_id.into()])).await?.map(|r| r.try_get::<i64>("", "generation")).transpose()?;
    if active == Some(generation) {
        let completed = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT 1 FROM regional_rollout_items WHERE operation_id=$1 AND step_id=$2 AND status='completed'",
            [operation.into(), activation.id.clone().into()])).await?.is_some();
        anyhow::ensure!(completed, "active policy has no committed activation receipt");
        return Ok(());
    }
    anyhow::ensure!(active == proposal.try_get::<Option<i64>>("", "expected_predecessor")?,
        "active policy predecessor changed");
    let owner = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM regional_service_rollouts r JOIN regional_rollout_items i USING(operation_id)
         WHERE r.operation_id=$1 AND r.service_id=$2 AND r.status='running' AND r.phase='activate_policy'
         AND r.region_index || ':' || r.phase || ':' || r.slot_index=$3 AND i.step_id=$4 AND i.status='completed'
         FOR UPDATE OF r", [operation.into(), service_id.into(), activation.id.clone().into(), preparation.id.clone().into()])).await?;
    anyhow::ensure!(owner.is_some(), "operation has not completed target preparation");
    anyhow::ensure!(super::regional_reports::ready(tx, service_id, operation, generation,
        super::regional_reports::Gate::Prepared).await?, "target preparation reports are missing or stale");
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO service_active_regional_policies(service_id,generation) VALUES($1,$2)
         ON CONFLICT(service_id) DO UPDATE SET generation=EXCLUDED.generation",
        vec![service_id.into(), generation.into()])).await?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE service_discovery_sets SET version=version+1,updated_at=NOW() WHERE service_id=$1 RETURNING version",
        [service_id.into()])).await?.context("discovery set disappeared")?;
    let revision: i64 = row.try_get("", "version")?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET phase='wait_policy_adopted',discovery_version=$2,updated_at=NOW()
         WHERE operation_id=$1", vec![operation.into(), revision.into()])).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires ORCHESTRATOR_TEST_DATABASE_URL (disposable PostgreSQL)"]
    async fn rollback_policy_occurrences_require_new_evidence_and_keep_forward_interrupted_postgres() -> Result<()> {
        use super::super::{regional_plan::Plan, regional_reports::{self, Participant, Report}};
        let url = std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let root = sea_orm::Database::connect(url.clone()).await?;
        let schema = format!("rollback_policy_{}",uuid::Uuid::new_v4().simple());
        root.execute_unprepared(&format!("CREATE SCHEMA {schema}")).await?;
        let mut options = sea_orm::ConnectOptions::new(url);
        options.set_schema_search_path(schema.clone());
        let db = sea_orm::Database::connect(options.clone()).await?;
        for migration in [include_str!("../../migrations/028_add_service_deployment_state.sql"),
            include_str!("../../migrations/031_add_service_discovery.sql"),include_str!("../../migrations/032_add_service_rollout_state.sql"),
            include_str!("../../migrations/033_add_service_replica_placement.sql"),include_str!("../../migrations/035_add_regional_service_rollouts.sql"),
            include_str!("../../migrations/037_add_regional_routing_policy.sql"),include_str!("../../migrations/038_add_regional_policy_proposals.sql"),
            include_str!("../../migrations/040_add_application_plan_journal.sql")] { db.execute_unprepared(migration).await?; }
        let plan = Plan::application(&["eu1".into(),"us3".into()],&[("eu1".into(),"eu-new".into()),("us3".into(),"us-new".into())])?;
        let participants = vec![
            Participant {gateway_id:"eu-a".into(),region:"eu1".into(),boot_id:uuid::Uuid::new_v4().to_string()},
            Participant {gateway_id:"us-a".into(),region:"us3".into(),boot_id:uuid::Uuid::new_v4().to_string()},
        ];
        store(&db,"rollback-svc",&PutPolicy {expected_version:0,policy:Some(policy())}).await?;
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
             VALUES('op','rollback-svc','hash','{\"deploymentEnvironment\":\"test\"}','rev',$1,'{}','[\"eu1\",\"us3\"]','[]',$2,1,1,30,'running','preflight')",
            vec![serde_json::to_string(&participants)?.into(),serde_json::to_value(plan)?.into()])).await?;
        db.execute_unprepared("UPDATE regional_service_rollouts SET phase='publish_policy' WHERE operation_id='op'").await?;
        let restarted = sea_orm::Database::connect(options).await?;
        for (index,slot) in [0,2,3].into_iter().enumerate() {
            let generation = index as i64 + 1;
            let mut proposed = policy();
            if slot != 3 { proposed.regions[1].weight=0; }
            let step = format!("0:publish_policy:{slot}");
            for connection in [&db,&restarted] {
                let tx = service_deploy::try_service_lifecycle_lock(connection,"rollback-svc").await?.unwrap();
                assert_eq!(publish_proposal_in(&tx,"rollback-svc","op",&step,&proposed,(index>0).then_some(generation-1)).await?,generation);
                tx.commit().await?;
            }
            assert!(!regional_reports::advance(&db,"rollback-svc","op",generation).await?,"earlier leg's evidence cannot prepare rollback");
            if index>0 { assert!(regional_reports::advance(&db,"rollback-svc","op",generation-1).await.is_err()); }
            for tick in 1..=if slot==3 {3} else {6} {
                for participant in &participants {
                    assert!(regional_reports::record(&db,"rollback-svc","op",&Report {
                        gateway_id:participant.gateway_id.clone(),boot_id:participant.boot_id.clone(),sequence:index as u64*10+tick,
                        generation,prepared:true,adopted:true,outgoing_target:0,local_target:0,peer_admission_closed:true,
                    },0).await?);
                }
                if tick==2 { activate_proposal(&restarted,"rollback-svc","op",generation).await?; }
                else { assert!(regional_reports::advance(&restarted,"rollback-svc","op",generation).await?); }
            }
            if slot==0 {
                db.execute_unprepared("UPDATE regional_service_rollouts SET status='blocked' WHERE operation_id='op'").await?;
                db.execute_unprepared("UPDATE regional_service_rollouts SET status='running',phase='rollback_entry',slot_index=2 WHERE operation_id='op'").await?;
                db.execute_unprepared("UPDATE regional_service_rollouts SET phase='publish_policy' WHERE operation_id='op'").await?;
            } else if slot==2 {
                // Cursor-only fixture for retained-replica probing: this test
                // verifies policy/journal identity, not application readiness.
                db.execute_unprepared("UPDATE regional_service_rollouts SET phase='publish_policy',slot_index=3 WHERE operation_id='op'").await?;
            }
        }
        assert!(activate_proposal(&db,"rollback-svc","op",1).await.is_err());
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT r.phase,i.status,a.generation,f.closed_through_generation FROM regional_service_rollouts r
             JOIN regional_rollout_items i USING(operation_id) JOIN service_active_regional_policies a USING(service_id)
             JOIN service_regional_admission_fences f USING(service_id)
             WHERE r.operation_id='op' AND i.step_id='0:create_candidate:0' AND f.region='eu1'")).await?.unwrap();
        assert_eq!(row.try_get::<String>("","phase")?,"bake");
        assert_eq!(row.try_get::<String>("","status")?,"interrupted");
        assert_eq!(row.try_get::<i64>("","generation")?,3);
        assert_eq!(row.try_get::<i64>("","closed_through_generation")?,2,"restoration cannot reopen old-generation peer admission");
        root.execute_unprepared(&format!("DROP SCHEMA {schema} CASCADE")).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ORCHESTRATOR_TEST_DATABASE_URL (disposable PostgreSQL)"]
    async fn successive_publications_keep_generation_and_step_evidence_separate_postgres() -> Result<()> {
        use super::super::{regional_plan::Plan, regional_reports::{self, Gate, Participant, Report}};
        let url = std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let root = sea_orm::Database::connect(url.clone()).await?;
        let schema = format!("policy_steps_{}", uuid::Uuid::new_v4().simple());
        root.execute_unprepared(&format!("CREATE SCHEMA {schema}")).await?;
        let mut options = sea_orm::ConnectOptions::new(url);
        options.set_schema_search_path(schema.clone());
        let db = sea_orm::Database::connect(options.clone()).await?;
        for migration in [
            include_str!("../../migrations/028_add_service_deployment_state.sql"),
            include_str!("../../migrations/031_add_service_discovery.sql"),
            include_str!("../../migrations/032_add_service_rollout_state.sql"),
            include_str!("../../migrations/033_add_service_replica_placement.sql"),
            include_str!("../../migrations/035_add_regional_service_rollouts.sql"),
            include_str!("../../migrations/037_add_regional_routing_policy.sql"),
            include_str!("../../migrations/038_add_regional_policy_proposals.sql"),
        ] { db.execute_unprepared(migration).await?; }
        // A test-only composite program exercises the primitives without opening
        // application-rollout admission or pretending candidate steps are wired.
        let targets = [Some("eu1"), None, Some("us3")];
        let mut plan = Plan { version: 2, steps: vec![], rollback_steps: vec![] };
        for (index, target) in targets.iter().enumerate() {
            for mut step in Plan::policy_transition(target.map(str::to_owned)).steps {
                if step.phase == "passed" && index < 2 { continue; }
                step.region_index = index;
                step.slot_index = index + 1;
                step.id = format!("{index}:{}:{}", step.phase, index + 1);
                step.depends_on = plan.steps.last().map(|s| s.id.clone());
                plan.steps.push(step);
            }
        }
        let participants = vec![
            Participant { gateway_id: "us-a".into(), region: "us3".into(), boot_id: uuid::Uuid::new_v4().to_string() },
            Participant { gateway_id: "eu-a".into(), region: "eu1".into(), boot_id: uuid::Uuid::new_v4().to_string() },
        ];
        store(&db, "steps", &PutPolicy { expected_version: 0, policy: Some(policy()) }).await?;
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase,slot_index)
             VALUES('op','steps','hash','{\"deploymentEnvironment\":\"test\"}','rev',$1,'{}','[\"eu1\",\"us3\"]','[]',$2,1,1,30,'running','publish_policy',1)",
            vec![serde_json::to_string(&participants)?.into(), serde_json::to_value(plan)?.into()])).await?;
        let restarted = sea_orm::Database::connect(options).await?;
        let mut reports: Vec<_> = participants.iter().map(|p| Report {
            gateway_id: p.gateway_id.clone(), boot_id: p.boot_id.clone(), sequence: 1, generation: 1,
            prepared: true, adopted: true, outgoing_target: 0, local_target: 0, peer_admission_closed: true,
        }).collect();
        for (index, target) in targets.iter().enumerate() {
            let generation = index as i64 + 1;
            let predecessor = (index > 0).then_some(generation - 1);
            let mut proposed = policy();
            for region in &mut proposed.regions {
                if Some(region.region.as_str()) == *target { region.weight = 0; }
            }
            let step_id = format!("{index}:publish_policy:{}", index + 1);
            for connection in [&db, &restarted] {
                let tx = service_deploy::try_service_lifecycle_lock(connection, "steps").await?.unwrap();
                assert_eq!(publish_proposal_in(&tx, "steps", "op", &step_id, &proposed, predecessor).await?, generation);
                tx.commit().await?;
            }
            assert!(!regional_reports::advance(&db, "steps", "op", generation).await?,
                "earlier generation reports cannot prepare this publication");
            if index > 0 {
                assert!(regional_reports::advance(&db, "steps", "op", generation - 1).await.is_err());
            }
            for report in &mut reports {
                report.generation = generation;
                report.sequence += 1;
                assert!(regional_reports::record(&db, "steps", "op", report, 0).await?);
            }
            assert!(regional_reports::advance(&restarted, "steps", "op", generation).await?);
            activate_proposal(&restarted, "steps", "op", generation).await?;
            activate_proposal(&db, "steps", "op", generation).await?;
            assert!(!regional_reports::advance(&db, "steps", "op", generation).await?,
                "even an adopted report must postdate this step's activation receipt");
            for report in &mut reports {
                report.sequence += 1;
                regional_reports::record(&db, "steps", "op", report, 0).await?;
            }
            assert!(regional_reports::advance(&db, "steps", "op", generation).await?);
            if let Some(target) = target {
                assert!(regional_reports::advance(&db, "steps", "op", generation).await?);
                assert!(regional_reports::advance(&db, "steps", "op", generation).await?);
                assert!(!regional_reports::ready(&db, "steps", "op", generation, Gate::AdmissionDrained).await?,
                    "the target's closed/zero report must postdate this closure, not a previous closure");
                for report in &mut reports {
                    report.sequence += 1;
                    regional_reports::record(&db, "steps", "op", report, 0).await?;
                }
                let participant = participants.iter().find(|p| &p.region == target).unwrap();
                let snapshot = super::super::service_discovery::read_regional_snapshot(
                    &db, "steps", target, &participant.gateway_id, &participant.boot_id).await?;
                assert_eq!(snapshot["drainTarget"], *target);
                assert_eq!(snapshot["closedThroughGeneration"], generation);
                assert_eq!(snapshot["activeGeneration"], generation);
                assert!(regional_reports::advance(&restarted, "steps", "op", generation).await?);
            }
        }
        assert!(activate_proposal(&restarted, "steps", "op", 1).await.is_err());
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT status,region_index,slot_index FROM regional_service_rollouts WHERE operation_id='op'")).await?.unwrap();
        assert_eq!(row.try_get::<String>("", "status")?, "passed");
        assert_eq!(row.try_get::<i32>("", "region_index")?, 2);
        assert_eq!(row.try_get::<i32>("", "slot_index")?, 3);
        root.execute_unprepared(&format!("DROP SCHEMA {schema} CASCADE")).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ORCHESTRATOR_TEST_DATABASE_URL (disposable PostgreSQL)"]
    async fn proposal_publication_activation_and_journal_are_atomic_postgres() -> Result<()> {
        let url = std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let root = sea_orm::Database::connect(url.clone()).await?;
        let schema = format!("policy_proposal_{}", uuid::Uuid::new_v4().simple());
        root.execute_unprepared(&format!("CREATE SCHEMA {schema}")).await?;
        let mut options = sea_orm::ConnectOptions::new(url);
        options.set_schema_search_path(schema.clone());
        let db = sea_orm::Database::connect(options.clone()).await?;
        for migration in [
            include_str!("../../migrations/028_add_service_deployment_state.sql"),
            include_str!("../../migrations/031_add_service_discovery.sql"),
            include_str!("../../migrations/032_add_service_rollout_state.sql"),
            include_str!("../../migrations/033_add_service_replica_placement.sql"),
            include_str!("../../migrations/035_add_regional_service_rollouts.sql"),
            include_str!("../../migrations/037_add_regional_routing_policy.sql"),
            include_str!("../../migrations/038_add_regional_policy_proposals.sql"),
            include_str!("../../migrations/038_add_regional_policy_proposals.sql"),
        ] { db.execute_unprepared(migration).await?; }
        let mut p = policy();
        p.regions[1].weight = 0;
        assert_eq!(store(&db, "proposal-svc", &PutPolicy { expected_version: 0, policy: Some(p.clone()) }).await?, Some(1));
        let plan = super::super::regional_plan::Plan::policy_transition(Some("eu1".into()));
        use super::super::regional_reports::{self, Gate, Participant, Report};
        let participants = vec![
            Participant { gateway_id: "us-a".into(), region: "us3".into(), boot_id: uuid::Uuid::new_v4().to_string() },
            Participant { gateway_id: "eu-a".into(), region: "eu1".into(), boot_id: uuid::Uuid::new_v4().to_string() },
        ];
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
             VALUES('op','proposal-svc','hash','{\"deploymentEnvironment\":\"test\"}','rev',$2,'{}','[\"eu1\"]','[]',$1,1,1,30,'running','publish_policy')",
            vec![serde_json::to_value(plan)?.into(), serde_json::to_string(&participants)?.into()])).await?;
        assert!(publish_proposal(&db, "proposal-svc", "op", &p, Some(99)).await.is_err());
        assert_eq!(publish_proposal(&db, "proposal-svc", "op", &p, None).await?, 1);
        let restarted = sea_orm::Database::connect(options).await?;
        assert_eq!(publish_proposal(&restarted, "proposal-svc", "op", &p, None).await?, 1);
        let mut changed = p.clone();
        changed.regions[0].weight = 7;
        assert!(publish_proposal(&db, "proposal-svc", "op", &changed, None).await.is_err());
        assert!(db.execute_unprepared("UPDATE regional_policy_proposals SET policy='{}'").await.is_err());
        assert!(db.execute_unprepared("DELETE FROM regional_policy_proposals").await.is_err());
        assert!(activate_proposal(&db, "proposal-svc", "other", 1).await.is_err());
        assert!(activate_proposal(&db, "proposal-svc", "op", 1).await.is_err(), "publication is not preparation");
        assert!(db.execute_unprepared("UPDATE regional_service_rollouts SET phase='wait_policy_adopted' WHERE operation_id='op'").await.is_err(), "cannot skip preparation/activation");
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT version,policy_generation,(SELECT count(*) FROM service_active_regional_policies) AS active FROM service_discovery_sets WHERE service_id='proposal-svc'")).await?.unwrap();
        assert_eq!(row.try_get::<i64>("", "version")?, 2);
        assert_eq!(row.try_get::<i64>("", "policy_generation")?, 1);
        assert_eq!(row.try_get::<i64>("", "active")?, 0);
        let snapshot = super::super::service_discovery::read_regional_snapshot(&db, "proposal-svc", "eu1", "eu-a", &participants[1].boot_id).await?;
        assert_eq!(snapshot["environment"], "test");
        assert_eq!(snapshot["proposalGeneration"], 1);
        assert!(snapshot["activeGeneration"].is_null(), "a proposal is not active routing");
        assert!(super::super::service_discovery::read_regional_snapshot(&db, "proposal-svc", "us3", "eu-a", &participants[1].boot_id).await.is_err());
        assert!(super::super::service_discovery::read_regional_snapshot(&db, "proposal-svc", "eu1", "eu-a", &uuid::Uuid::new_v4().to_string()).await.is_err());

        // Simulate the future executor completing its preparation gate. This
        // fixture does not claim to validate gateway ACKs or live drain.
        db.execute_unprepared("UPDATE regional_service_rollouts SET phase='activate_policy' WHERE operation_id='op'").await?;
        assert!(activate_proposal(&db, "proposal-svc", "op", 1).await.is_err(), "cursor alone is not fresh evidence");
        let mut reports: Vec<_> = participants.iter().map(|p| Report {
            gateway_id: p.gateway_id.clone(), boot_id: p.boot_id.clone(), sequence: 10, generation: 1,
            prepared: true, adopted: false, outgoing_target: 3, local_target: 7, peer_admission_closed: false,
        }).collect();
        assert!(regional_reports::record(&db, "proposal-svc", "op", &reports[0], 0).await?);
        assert!(!regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::Prepared).await?, "one participant cannot stand in for both");
        assert!(regional_reports::record(&db, "proposal-svc", "op", &reports[1], 0).await?);
        assert!(regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::Prepared).await?);
        assert!(!regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::Adopted).await?);
        db.execute_unprepared("UPDATE regional_gateway_reports SET observed_at=clock_timestamp()-interval '1 minute'").await?;
        assert!(!regional_reports::record(&restarted, "proposal-svc", "op", &reports[0], 0).await?, "replay cannot refresh age");
        assert!(activate_proposal(&db, "proposal-svc", "op", 1).await.is_err(), "stale evidence must block activation");
        for report in &mut reports {
            report.sequence += 1;
            assert!(regional_reports::record(&db, "proposal-svc", "op", report, 0).await?);
        }
        db.execute_unprepared("UPDATE service_discovery_sets SET version=40 WHERE service_id='proposal-svc'").await?;
        db.execute_unprepared("CREATE FUNCTION fail_policy_journal() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected journal failure'; END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER fail_policy_journal BEFORE INSERT ON regional_rollout_events FOR EACH ROW EXECUTE FUNCTION fail_policy_journal()").await?;
        assert!(activate_proposal(&db, "proposal-svc", "op", 1).await.is_err());
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT version,(SELECT count(*) FROM service_active_regional_policies) AS active FROM service_discovery_sets WHERE service_id='proposal-svc'")).await?.unwrap();
        assert_eq!(row.try_get::<i64>("", "version")?, 40);
        assert_eq!(row.try_get::<i64>("", "active")?, 0, "journal failure must roll back activation");
        db.execute_unprepared("DROP TRIGGER fail_policy_journal ON regional_rollout_events").await?;
        let lock = service_deploy::try_service_lifecycle_lock(&db, "proposal-svc").await?.unwrap();
        assert!(activate_proposal(&restarted, "proposal-svc", "op", 1).await.is_err());
        lock.commit().await?;
        activate_proposal(&db, "proposal-svc", "op", 1).await?;
        activate_proposal(&restarted, "proposal-svc", "op", 1).await?;
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT s.version,a.generation,r.phase,i.status FROM service_discovery_sets s
             JOIN service_active_regional_policies a USING(service_id)
             JOIN regional_service_rollouts r USING(service_id)
             JOIN regional_rollout_items i USING(operation_id)
             WHERE s.service_id='proposal-svc' AND i.step_id='0:activate_policy:0'")).await?.unwrap();
        assert_eq!(row.try_get::<i64>("", "version")?, 41, "replay must not allocate another revision");
        assert_eq!(row.try_get::<i64>("", "generation")?, 1, "membership revision is not policy generation");
        assert_eq!(row.try_get::<String>("", "phase")?, "wait_policy_adopted");
        assert_eq!(row.try_get::<String>("", "status")?, "completed");
        assert!(!regional_reports::advance(&db, "proposal-svc", "op", 1).await?);
        for report in &mut reports {
            report.sequence += 1;
            report.adopted = true;
            regional_reports::record(&db, "proposal-svc", "op", report, 0).await?;
        }
        assert!(regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::Adopted).await?);
        assert!(regional_reports::advance(&db, "proposal-svc", "op", 1).await?);
        assert!(!regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::AssignmentsDrained).await?);
        assert!(!regional_reports::advance(&db, "proposal-svc", "op", 1).await?);
        for report in &mut reports {
            report.sequence += 1;
            report.outgoing_target = 0;
            regional_reports::record(&db, "proposal-svc", "op", report, 0).await?;
        }
        assert!(regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::AssignmentsDrained).await?);
        assert!(regional_reports::advance(&db, "proposal-svc", "op", 1).await?);
        assert!(regional_reports::advance(&db, "proposal-svc", "op", 1).await?);
        assert!(!regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::AdmissionDrained).await?);
        let snapshot = super::super::service_discovery::read_regional_snapshot(&db, "proposal-svc", "eu1", "eu-a", &participants[1].boot_id).await?;
        assert_eq!(snapshot["activeGeneration"], 1);
        assert_eq!(snapshot["closedThroughGeneration"], 1);
        assert_eq!(snapshot["phase"], "wait_admission_drained");
        let other = super::super::service_discovery::read_regional_snapshot(&db, "proposal-svc", "us3", "us-a", &participants[0].boot_id).await?;
        assert_eq!(other["closedThroughGeneration"], 0, "only the withdrawn region closes peer admission");
        reports[1].sequence += 1;
        reports[1].peer_admission_closed = true;
        regional_reports::record(&db, "proposal-svc", "op", &reports[1], 0).await?;
        assert!(!regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::AdmissionDrained).await?, "closed admission is not zero local work");
        reports[1].sequence += 1;
        reports[1].local_target = 0;
        regional_reports::record(&db, "proposal-svc", "op", &reports[1], 0).await?;
        assert!(regional_reports::ready(&db, "proposal-svc", "op", 1, Gate::AdmissionDrained).await?);
        db.execute_unprepared("UPDATE regional_gateway_reports SET observed_at=(SELECT started_at-interval '1 millisecond'
            FROM regional_rollout_items WHERE operation_id='op' AND step_id='0:close_peer_admission:0') WHERE gateway_id='eu-a'").await?;
        assert!(!regional_reports::advance(&db, "proposal-svc", "op", 1).await?, "admission ACK must follow the close command");
        reports[1].sequence += 1;
        regional_reports::record(&db, "proposal-svc", "op", &reports[1], 0).await?;
        assert!(regional_reports::advance(&db, "proposal-svc", "op", 1).await?);
        assert_eq!(store(&db, "proposal-svc", &PutPolicy { expected_version: 45, policy: None }).await?, None,
            "clearing draft must not disable an active regional policy");
        assert!(require_legacy_topology(&db, "proposal-svc").await.is_err());
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
             SELECT 'op-2',service_id,'hash-2',deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,'running','publish_policy'
             FROM regional_service_rollouts WHERE operation_id=$1", ["op".into()])).await?;
        assert!(publish_proposal(&db, "proposal-svc", "op-2", &changed, None).await.is_err());
        assert_eq!(publish_proposal(&db, "proposal-svc", "op-2", &changed, Some(1)).await?, 2);
        db.execute_unprepared("UPDATE regional_service_rollouts SET phase='activate_policy' WHERE operation_id='op-2'").await?;
        assert!(activate_proposal(&db, "proposal-svc", "op-2", 2).await.is_err(), "previous operation reports do not acknowledge a new proposal");
        for report in &mut reports {
            report.generation = 2;
            assert!(!regional_reports::record(&db, "proposal-svc", "op-2", report, 0).await?, "sequence high-water survives operation changes");
            report.sequence += 1;
            assert!(regional_reports::record(&db, "proposal-svc", "op-2", report, 0).await?);
        }
        activate_proposal(&db, "proposal-svc", "op-2", 2).await?;
        assert!(activate_proposal(&restarted, "proposal-svc", "op", 1).await.is_err(),
            "a late old-controller retry must not restore its predecessor policy");
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT generation FROM service_active_regional_policies WHERE service_id='proposal-svc'")).await?.unwrap();
        assert_eq!(row.try_get::<i64>("", "generation")?, 2);
        assert!(regional_reports::record(&db, "proposal-svc", "op-2", &reports[0], 5_001).await.is_err());
        let mut rebooted = reports[0].clone();
        rebooted.boot_id = uuid::Uuid::new_v4().to_string();
        rebooted.outgoing_target = 0;
        let error = regional_reports::record(&db, "proposal-svc", "op-2", &rebooted, 0).await.unwrap_err();
        assert!(error.to_string().contains("gateway boot changed"), "{error:#}");
        assert!(!regional_reports::ready(&restarted, "proposal-svc", "op-2", 2, Gate::Prepared).await?,
            "a new boot must immediately invalidate even fresh predecessor evidence");
        reports[0].sequence += 1;
        assert!(!regional_reports::record(&restarted, "proposal-svc", "op-2", &reports[0], 0).await?,
            "a delayed old-boot response cannot undo restart invalidation");
        root.execute_unprepared(&format!("DROP SCHEMA {schema} CASCADE")).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ORCHESTRATOR_TEST_DATABASE_URL (disposable PostgreSQL)"]
    async fn regional_policy_cas_restart_and_lifecycle_fencing_postgres() -> Result<()> {
        let url = std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let root = sea_orm::Database::connect(url.clone()).await?;
        let schema = format!("regional_policy_{}", uuid::Uuid::new_v4().simple());
        root.execute_unprepared(&format!("CREATE SCHEMA {schema}")).await?;
        let mut options = sea_orm::ConnectOptions::new(url);
        options.set_schema_search_path(schema.clone());
        let db = sea_orm::Database::connect(options.clone()).await?;
        for migration in [
            include_str!("../../migrations/028_add_service_deployment_state.sql"),
            include_str!("../../migrations/031_add_service_discovery.sql"),
            include_str!("../../migrations/032_add_service_rollout_state.sql"),
            include_str!("../../migrations/033_add_service_replica_placement.sql"),
            include_str!("../../migrations/035_add_regional_service_rollouts.sql"),
            include_str!("../../migrations/037_add_regional_routing_policy.sql"),
            include_str!("../../migrations/037_add_regional_routing_policy.sql"),
            include_str!("../../migrations/038_add_regional_policy_proposals.sql"),
        ] { db.execute_unprepared(migration).await?; }
        let mut request = PutPolicy { expected_version: 0, policy: Some(policy()) };
        require_legacy_topology(&db, "svc").await?;
        assert_eq!(store(&db, "svc", &request).await?, Some(1));
        assert!(require_legacy_topology(&db, "svc").await.is_err());
        assert_eq!(store(&db, "svc", &request).await?, None, "stale retry must not mutate generation");
        let restarted = sea_orm::Database::connect(options).await?;
        let row = restarted.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT version, regional_policy FROM service_discovery_sets WHERE service_id='svc'")).await?.unwrap();
        assert_eq!(row.try_get::<i64>("", "version")?, 1);
        assert_eq!(row.try_get::<Value>("", "regional_policy")?, serde_json::to_value(&request.policy)?);
        request.expected_version = 1;
        let lock = service_deploy::try_service_lifecycle_lock(&db, "svc").await?.unwrap();
        assert_eq!(store(&restarted, "svc", &request).await?, None);
        lock.commit().await?;
        db.execute_unprepared("INSERT INTO service_rollouts(service_id,rollout_id,desired_replicas,status,stage,lease_expires_at) VALUES('svc','ordinary',1,'running','creating',NOW())").await?;
        assert_eq!(store(&restarted, "svc", &request).await?, None);
        db.execute_unprepared("UPDATE service_rollouts SET status='passed' WHERE service_id='svc'").await?;
        let mut other = PutPolicy { expected_version: 1, policy: Some(policy()) };
        other.policy.as_mut().unwrap().regions[0].weight = 9;
        let (a, b) = tokio::join!(store(&db, "svc", &request), store(&restarted, "svc", &other));
        let (a, b) = (a?, b?);
        assert_eq!(usize::from(a.is_some()) + usize::from(b.is_some()), 1);
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT version, regional_policy FROM service_discovery_sets WHERE service_id='svc'")).await?.unwrap();
        assert_eq!(row.try_get::<i64>("", "version")?, 2);
        let winner = if a.is_some() { &request.policy } else { &other.policy };
        assert_eq!(row.try_get::<Value>("", "regional_policy")?, serde_json::to_value(winner)?);
        let clear = PutPolicy { expected_version: 2, policy: None };
        assert_eq!(store(&db, "svc", &clear).await?, Some(3));
        let row = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT version, regional_policy FROM service_discovery_sets WHERE service_id='svc'")).await?.unwrap();
        assert_eq!(row.try_get::<i64>("", "version")?, 3);
        assert_eq!(row.try_get::<Option<Value>>("", "regional_policy")?, None);
        require_legacy_topology(&db, "svc").await?;
        let plan = super::super::regional_plan::Plan::compile(&["us3".into()], &[]);
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
             VALUES('blocked','blocked','hash','{}','rev','[]','{}','[\"us3\"]','[]',$1,1,1,30,'blocked','preflight')",
            [serde_json::to_value(plan)?.into()])).await?;
        assert_eq!(store(&db, "blocked", &PutPolicy { expected_version: 0, policy: Some(policy()) }).await?, None);
        assert_eq!(store(&db, "blocked", &PutPolicy { expected_version: 0, policy: None }).await?, None);
        root.execute_unprepared(&format!("DROP SCHEMA {schema} CASCADE")).await?;
        Ok(())
    }

    fn policy() -> RegionalPolicy {
        serde_json::from_value(json!({"version":1,"regions":[
            {"region":"us3","weight":2,"gateways":[{"id":"us-a","backendServerId":"host-us","url":"https://us.example"}]},
            {"region":"eu1","weight":1,"gateways":[{"id":"eu-a","backendServerId":"host-eu","url":"https://eu.example"}]}
        ]})).unwrap()
    }

    #[test]
    fn policy_validates_explicit_weights_unique_inventory_and_transport() {
        assert!(serde_json::from_value::<PutPolicy>(json!({"expectedVersion":0})).is_err());
        assert!(serde_json::from_value::<PutPolicy>(json!({"expectedVersion":0,"policy":null})).unwrap().policy.is_none());
        let original = policy();
        original.validate().unwrap();
        let mut p = original.clone();
        p.regions[0].weight = 0;
        p.validate().unwrap();
        p.regions[1].weight = 0;
        assert!(p.validate().is_err());
        for bad in ["http://eu.example", "https://user:password@eu.example", "https://eu.example/path", "https://eu.example?x=1", "https://127.0.0.1", "https://[::1]"] {
            let mut p = original.clone();
            p.regions[1].gateways[0].url = bad.into();
            assert!(p.validate().is_err(), "accepted {bad}");
        }
        let mut p = original.clone();
        p.regions[1].gateways[0].id = "us-a".into();
        assert!(p.validate().is_err());
        let mut p = original.clone();
        p.regions[1].gateways[0].url = "https://us.example:443/".into();
        assert!(p.validate().is_err());
        let mut p = original;
        p.version = 2;
        assert!(p.validate().is_err());
    }
}
