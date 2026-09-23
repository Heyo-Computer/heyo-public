//! Candidate creation evidence owned by the regional plan, not a second queue.
//! Admission/executor wiring remains closed until the complete lifecycle exists.
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use crate::{cloud_client::{self, CreateDeploymentRequest, DeploymentCreationReceipt}, AppState};
use super::{regional_plan::Plan, regional_policy::RegionalPolicy, regional_reports::{self, Gate}, service_deploy};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Intent {
    pub deployment_id: String,
    pub request_digest: String,
    pub archive_sha256: String,
    pub archive_id: Option<String>,
    pub runtime_revision: String,
    pub region: String,
    pub environment: String,
    pub placement_pool: String,
    pub guest_port: u16,
    pub withdrawal_generation: i64,
    pub allowed_backend_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Binding {
    pub backend_server_id: String,
    pub node_id: String,
    pub backend_sandbox_id: String,
    pub archive_id: String,
    pub host_local_url: String,
}

/// Execute the persisted candidate item. Once an intent exists, recovery never
/// reloads archives/secrets and never POSTs again, even after an uncertain create.
pub(super) async fn create_or_recover(state: &AppState, db: &sea_orm::DatabaseConnection,
    service: &str, operation: &str) -> Result<()> {
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.plan,r.region_index,r.slot_index,r.deployment_request,r.slots,c.intent
         FROM regional_service_rollouts r LEFT JOIN regional_candidate_creations c
         ON c.operation_id=r.operation_id AND c.step_id=r.region_index || ':' || r.phase || ':' || r.slot_index
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running' AND r.phase='create_candidate'",
        [service.into(),operation.into()])).await?.context("candidate has no owning create item")?;
    let plan: Plan = serde_json::from_value(row.try_get("","plan")?)?;
    anyhow::ensure!(plan.version == 3, "candidate execution requires an application plan");
    let item = plan.step("create_candidate",row.try_get::<i32>("","region_index")?.try_into()?,
        row.try_get::<i32>("","slot_index")?.try_into()?)?;
    if row.try_get::<Option<serde_json::Value>>("","intent")?.is_none() {
        let input: serde_json::Value = row.try_get("","deployment_request")?;
        let revision = input["expectedRuntimeRevision"].as_str().filter(|v| !v.is_empty()).context("runtime revision missing")?;
        let port = input["guestPort"].as_u64().and_then(|v| u16::try_from(v).ok()).filter(|v| *v > 0)
            .context("application probe port missing")?;
        let mut request: service_deploy::ServiceDeployRequest = serde_json::from_value(input.clone())?;
        anyhow::ensure!(request.service_id == service, "candidate service differs from owner");
        let deployment = item.candidate_id.as_deref().context("candidate ID missing")?;
        request.region = item.region.clone().context("candidate region missing")?;
        let slots: serde_json::Value = row.try_get("","slots")?;
        let slot = slots.as_array().context("invalid application slots")?.iter()
            .find(|s| s["candidateId"].as_str() == Some(deployment)).context("candidate slot missing")?;
        anyhow::ensure!(slot["region"].as_str() == Some(&request.region), "candidate slot region differs from plan");
        if let Some(runtime) = slot.get("runtime").filter(|v| !v.is_null()) {
            let runtime: super::regional_rollout::RegionalRuntime = serde_json::from_value(runtime.clone())?;
            request.driver = runtime.driver; request.image = runtime.image; request.size_class = runtime.size_class;
        }
        let (_,policy) = withdrawal(db,service,operation,&request.region).await?;
        let mut hosts: Vec<_> = policy.regions.iter().filter(|r| r.region == request.region)
            .flat_map(|r| r.gateways.iter().map(|g| g.backend_server_id.clone())).collect();
        hosts.sort(); hosts.dedup();
        let excluded = db.query_all(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT receipt->>'backendServerId' AS host FROM regional_candidate_creations
             WHERE operation_id=$1 AND intent->>'region'=$2 AND receipt IS NOT NULL ORDER BY step_id",
            [operation.into(),request.region.clone().into()])).await?.into_iter()
            .map(|r| r.try_get::<String>("","host")).collect::<std::result::Result<Vec<_>,_>>()?;
        let (mut prepared,archive,_) = service_deploy::prepare_cloud_candidate(state,&mut request,deployment,excluded).await?;
        prepared.allowed_backend_server_ids = Some(hosts);
        let (_,send) = claim(db,service,operation,&prepared,&archive,revision,port).await?;
        if send { cloud_client::create_deployment(state,&prepared).await?; }
    }
    recover(state,db,service,operation,&item.id).await?;
    publish(db,service,operation).await
}

/// Returns the pinned intent and a one-use permission to send create, only after
/// commit. A replay never grants that permission, even if Cloud has no receipt.
pub(super) async fn claim(
    db: &sea_orm::DatabaseConnection, service: &str, operation: &str,
    request: &CreateDeploymentRequest, archive_sha256: &str, runtime_revision: &str, guest_port: u16,
) -> Result<(Intent, bool)> {
    let tx = service_deploy::try_service_lifecycle_lock(db, service).await?.context("service lifecycle busy")?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT plan,region_index,slot_index,phase,target_revision,deployment_request
         FROM regional_service_rollouts WHERE service_id=$1 AND operation_id=$2 AND status='running' FOR UPDATE",
        [service.into(), operation.into()])).await?.context("candidate has no running owner")?;
    let plan: Plan = serde_json::from_value(row.try_get("", "plan")?)?;
    let step = plan.step(&row.try_get::<String>("", "phase")?,
        row.try_get::<i32>("", "region_index")?.try_into()?, row.try_get::<i32>("", "slot_index")?.try_into()?)?;
    anyhow::ensure!(step.phase == "create_candidate" && step.candidate_id.as_deref() == Some(&request.deployment_id)
        && step.region.as_deref() == Some(&request.region), "candidate differs from persisted plan item");
    let input: serde_json::Value = row.try_get("", "deployment_request")?;
    let environment = request.deployment_environment.as_deref().filter(|v| !v.is_empty()).context("candidate environment missing")?;
    let pool = request.placement_pool.as_deref().filter(|v| !v.is_empty()).context("candidate placement pool missing")?;
    anyhow::ensure!(input["deploymentEnvironment"].as_str() == Some(environment)
        && input["placementPool"].as_str() == Some(pool), "candidate placement differs from admitted request");
    anyhow::ensure!(archive_sha256.len() == 64 && archive_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        && row.try_get::<String>("", "target_revision")? == archive_sha256, "candidate archive differs from admitted revision");
    anyhow::ensure!(!runtime_revision.is_empty() && input["expectedRuntimeRevision"].as_str() == Some(runtime_revision) && guest_port > 0
        && (request.ports.contains(&guest_port) || request.port_mappings.iter().any(|p| p.container == guest_port)),
        "candidate requires an explicit application revision and exposed probe port");
    let (generation, policy) = withdrawal(&tx, service, operation, &request.region).await?;
    let mut hosts: Vec<_> = policy.regions.iter().filter(|r| r.region == request.region)
        .flat_map(|r| r.gateways.iter().map(|g| g.backend_server_id.clone())).collect();
    hosts.sort(); hosts.dedup();
    if hosts.is_empty() || request.allowed_backend_server_ids.as_ref() != Some(&hosts) {
        tx.rollback().await?;
        anyhow::bail!("candidate placement must be restricted to the canonical pinned regional fleet");
    }
    let intent = Intent { deployment_id: request.deployment_id.clone(), request_digest: cloud_client::deployment_request_digest(request)?,
        archive_sha256: archive_sha256.into(), archive_id: request.archive_id.clone(), runtime_revision: runtime_revision.into(),
        region: request.region.clone(), environment: environment.into(), placement_pool: pool.into(), guest_port,
        withdrawal_generation: generation, allowed_backend_ids: hosts };
    let existing = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT intent FROM regional_candidate_creations WHERE operation_id=$1 AND step_id=$2",
        [operation.into(), step.id.clone().into()])).await?;
    if let Some(existing) = existing {
        anyhow::ensure!(serde_json::from_value::<Intent>(existing.try_get("", "intent")?)? == intent,
            "candidate creation retry has different intent");
        tx.rollback().await?;
        return Ok((intent, false));
    }
    anyhow::ensure!(regional_reports::ready(&tx, service, operation, generation, Gate::AdmissionDrained).await?,
        "candidate creation requires fresh completed withdrawal evidence");
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO regional_candidate_creations(operation_id,step_id,deployment_id,intent) VALUES($1,$2,$3,$4)",
        vec![operation.into(), step.id.clone().into(), request.deployment_id.clone().into(), serde_json::to_value(&intent)?.into()])).await?;
    tx.commit().await?;
    Ok((intent, true))
}

async fn withdrawal(db: &impl ConnectionTrait, service: &str, operation: &str, region: &str) -> Result<(i64, RegionalPolicy)> {
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT p.generation,p.policy,p.step_id,r.plan FROM service_active_regional_policies a
         JOIN regional_policy_proposals p USING(service_id,generation)
         JOIN regional_service_rollouts r USING(service_id,operation_id)
         WHERE a.service_id=$1 AND p.operation_id=$2 AND r.status='running'",
        [service.into(), operation.into()])).await?.context("candidate requires its owner's active withdrawal")?;
    let plan: Plan = serde_json::from_value(row.try_get("", "plan")?)?;
    let publication = plan.publication(&row.try_get::<String>("", "step_id")?)?;
    anyhow::ensure!(publication.region.as_deref() == Some(region), "candidate is outside the withdrawn region");
    let drained = plan.step("wait_admission_drained", publication.region_index, publication.slot_index)?;
    anyhow::ensure!(db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM regional_rollout_items WHERE operation_id=$1 AND step_id=$2 AND status='completed'",
        [operation.into(), drained.id.clone().into()])).await?.is_some(), "candidate withdrawal journal is incomplete");
    let policy: RegionalPolicy = serde_json::from_value(row.try_get("", "policy")?)?;
    anyhow::ensure!(policy.regions.iter().any(|r| r.region == region && r.weight == 0), "candidate region is serving");
    Ok((row.try_get("", "generation")?, policy))
}

fn binding(intent: &Intent, receipt: &DeploymentCreationReceipt) -> Result<Binding> {
    let deployment = &receipt.deployment;
    let placement = deployment.placement.as_ref().context("Cloud placement receipt missing")?;
    let host = deployment.backend_server_id.as_deref().filter(|s| !s.is_empty()).context("Cloud host identity missing")?;
    let runtime = deployment.backend_sandbox_id.as_deref().filter(|s| !s.is_empty()).context("Cloud runtime identity missing")?;
    let archive = deployment.archive_id.as_deref().filter(|s| !s.is_empty()).context("Cloud archive identity missing")?;
    anyhow::ensure!(deployment.deployment_id == intent.deployment_id && receipt.request_digest == intent.request_digest
        && deployment.status == "running" && !placement.node_id.is_empty() && intent.allowed_backend_ids.iter().any(|h| h == host)
        && placement.region == intent.region && placement.deployment_environment == intent.environment
        && placement.placement_pool.as_deref() == Some(&intent.placement_pool)
        && intent.archive_id.as_deref().is_none_or(|id| id == archive) && receipt.guest_port == Some(intent.guest_port),
        "Cloud candidate receipt differs from pinned creation/placement");
    let url = receipt.host_local_url.as_deref().context("Cloud host-local mapping missing")?;
    let parsed = reqwest::Url::parse(url)?;
    anyhow::ensure!(parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1")
        && parsed.port_or_known_default().is_some_and(|p| p > 0) && parsed.path() == "/"
        && parsed.query().is_none() && parsed.fragment().is_none() && parsed.username().is_empty() && parsed.password().is_none(),
        "Cloud candidate mapping is not host-local");
    Ok(Binding { backend_server_id: host.into(), node_id: placement.node_id.clone(),
        backend_sandbox_id: runtime.into(), archive_id: archive.into(), host_local_url: url.into() })
}

/// Recovery is read-only at Cloud. Persist a validated binding only; this does
/// not publish discovery, infer health, advance the plan, or create a replacement.
pub(super) async fn recover(
    state: &AppState, db: &sea_orm::DatabaseConnection, service: &str, operation: &str, step_id: &str,
) -> Result<Binding> {
    let row = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT c.intent FROM regional_candidate_creations c JOIN regional_service_rollouts r USING(operation_id)
         WHERE r.service_id=$1 AND c.operation_id=$2 AND c.step_id=$3",
        [service.into(), operation.into(), step_id.into()])).await?.context("candidate intent missing")?;
    let intent: Intent = serde_json::from_value(row.try_get("", "intent")?)?;
    let receipt = cloud_client::recover_deployment(state, &intent.deployment_id, &intent.request_digest, Some(intent.guest_port)).await?;
    let binding = binding(&intent, &receipt)?;
    let tx = service_deploy::try_service_lifecycle_lock(db, service).await?.context("service lifecycle busy")?;
    let (generation, _) = withdrawal(&tx, service, operation, &intent.region).await?;
    anyhow::ensure!(generation == intent.withdrawal_generation, "candidate recovery no longer owns its withdrawal");
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_candidate_creations SET receipt=$3,observed_at=clock_timestamp() WHERE operation_id=$1 AND step_id=$2",
        vec![operation.into(), step_id.into(), serde_json::to_value(&binding)?.into()])).await?;
    tx.commit().await?;
    Ok(binding)
}

/// Publish only the validated host-local binding, excluded from serving, and
/// advance the owning item in the same transaction. Never use the ordinary
/// healthy-endpoint publisher for a candidate that has not passed peer probes.
pub(super) async fn publish(
    db: &sea_orm::DatabaseConnection, service: &str, operation: &str,
) -> Result<()> {
    let tx = service_deploy::try_service_lifecycle_lock(db, service).await?.context("service lifecycle busy")?;
    let row = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT r.plan,r.region_index,r.slot_index,c.step_id,c.intent,c.receipt
         FROM regional_service_rollouts r JOIN regional_candidate_creations c USING(operation_id)
         WHERE r.service_id=$1 AND r.operation_id=$2 AND r.status='running' AND r.phase='create_candidate'
         AND c.step_id=r.region_index || ':' || r.phase || ':' || r.slot_index FOR UPDATE OF r",
        [service.into(), operation.into()])).await?.context("candidate publication has no owning item")?;
    let intent: Intent = serde_json::from_value(row.try_get("", "intent")?)?;
    let receipt: Binding = serde_json::from_value(row.try_get::<Option<serde_json::Value>>("", "receipt")?
        .context("candidate has no validated runtime receipt")?)?;
    let plan: Plan = serde_json::from_value(row.try_get("", "plan")?)?;
    let item = plan.step("create_candidate", row.try_get::<i32>("", "region_index")?.try_into()?,
        row.try_get::<i32>("", "slot_index")?.try_into()?)?;
    anyhow::ensure!(item.candidate_id.as_deref() == Some(&intent.deployment_id)
        && item.region.as_deref() == Some(&intent.region), "candidate publication differs from plan");
    let (generation, _) = withdrawal(&tx, service, operation, &intent.region).await?;
    anyhow::ensure!(generation == intent.withdrawal_generation, "candidate publication no longer owns withdrawal");
    let next = plan.steps.iter().find(|s| s.depends_on.as_deref() == Some(&item.id)).context("candidate has no successor")?;
    anyhow::ensure!(matches!(next.phase.as_str(), "create_candidate" | "probe_candidates"), "candidate publication cannot bypass probing");
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO service_discovery_endpoints(service_id,deployment_id,backend_server_id,region,revision,backend_url,health_status,draining)
         VALUES($1,$2,$3,$4,$5,$6,'unknown',TRUE)",
        vec![service.into(),intent.deployment_id.into(),receipt.backend_server_id.into(),intent.region.into(),
            intent.runtime_revision.into(),receipt.host_local_url.into()])).await?;
    let version: i64 = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE service_discovery_sets SET version=version+1,updated_at=clock_timestamp() WHERE service_id=$1 RETURNING version",
        [service.into()])).await?.context("discovery disappeared")?.try_get("", "version")?;
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE regional_service_rollouts SET phase=$2,region_index=$3,slot_index=$4,discovery_version=$5,updated_at=clock_timestamp()
         WHERE operation_id=$1",vec![operation.into(),next.phase.clone().into(),i32::try_from(next.region_index)?.into(),
            i32::try_from(next.slot_index)?.into(),version.into()])).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn request() -> CreateDeploymentRequest {
        CreateDeploymentRequest {
            deployment_id: "candidate-eu".into(), user_id: "operator".into(), account_id: "account".into(),
            name: "candidate".into(), slug: None, target: "service".into(), archive_id: Some("archive-a".into()),
            archive_name: None, archive_bytes: vec![], region: "eu1".into(), backend_type: "libvirt".into(),
            image: "ubuntu".into(), ports: vec![8080], port_mappings: vec![], mounts: vec![],
            env: Some(std::collections::HashMap::from([("TOKEN".into(), "test-only-secret-value".into())])),
            env_refs: vec![], start_command: None, working_directory: None, setup_hooks: None,
            size_class: "small".into(), ttl_seconds: None, deployment_environment: Some("test".into()),
            placement_pool: Some("shared".into()), excluded_backend_server_ids: vec![], metadata: None,
            allowed_backend_server_ids: Some(vec!["backend-eu".into()]),
        }
    }

    fn receipt(digest: &str) -> Value {
        json!({"deploymentId":"candidate-eu","requestDigest":digest,"archiveId":"archive-a",
            "backendServerId":"backend-eu","backendSandboxId":"runtime-a","status":"running",
            "placement":{"nodeId":"physical-eu","region":"eu1","deploymentEnvironment":"test","placementPool":"shared"},
            "hostLocalUrl":"http://127.0.0.1:18081","guestPort":8080})
    }

    #[test]
    fn candidate_binding_rejects_wrong_host_scope_runtime_and_mapping() -> Result<()> {
        let intent = Intent { deployment_id:"candidate-eu".into(),request_digest:"digest".into(),archive_sha256:"a".repeat(64),
            archive_id:Some("archive-a".into()),runtime_revision:"app-revision".into(),region:"eu1".into(),environment:"test".into(),
            placement_pool:"shared".into(),guest_port:8080,withdrawal_generation:7,allowed_backend_ids:vec!["backend-eu".into()] };
        let value = receipt("digest");
        assert_eq!(binding(&intent, &serde_json::from_value(value.clone())?)?.node_id, "physical-eu",
            "Cloud backend ID and physical node ID are distinct identities");
        for (pointer, replacement) in [
            ("/deploymentId",json!("another")), ("/requestDigest",json!("another")),
            ("/archiveId",json!("another")), ("/backendServerId",json!("backend-us")),
            ("/backendSandboxId",json!("")), ("/status",json!("stopped")),
            ("/placement/nodeId",json!("")), ("/placement/region",json!("us3")),
            ("/placement/deploymentEnvironment",json!("production")), ("/placement/placementPool",json!("other")),
            ("/hostLocalUrl",json!("https://eu.example:18081")), ("/hostLocalUrl",json!("http://127.0.0.1:18081/private")),
            ("/guestPort",json!(9090)),
        ] {
            let mut bad = value.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            assert!(binding(&intent, &serde_json::from_value(bad)?).is_err(), "accepted {pointer}");
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires ORCHESTRATOR_TEST_DATABASE_URL (disposable PostgreSQL)"]
    async fn candidate_claim_restart_and_read_only_recovery_postgres() -> Result<()> {
        use super::super::{regional_plan::Step, regional_policy, regional_reports::{Participant, Report}};
        let url = std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let root = sea_orm::Database::connect(url.clone()).await?;
        let schema = format!("candidate_receipt_{}", uuid::Uuid::new_v4().simple());
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
            include_str!("../../migrations/039_add_regional_candidate_receipts.sql"),
            include_str!("../../migrations/039_add_regional_candidate_receipts.sql"),
        ] { db.execute_unprepared(migration).await?; }
        // The fixture adds an internal candidate item; production still only
        // admits routing-only plans and cannot reach this item.
        let mut plan = Plan::policy_transition(Some("eu1".into()));
        let mut passed = plan.steps.pop().unwrap();
        let step_id = "0:create_candidate:1";
        plan.steps.push(Step { id:step_id.into(),depends_on:passed.depends_on.clone(),phase:"create_candidate".into(),
            region:Some("eu1".into()),region_index:0,slot_index:1,candidate_id:Some("candidate-eu".into()) });
        plan.steps.push(Step { id:"0:probe_candidates:1".into(),depends_on:Some(step_id.into()),phase:"probe_candidates".into(),
            region:Some("eu1".into()),region_index:0,slot_index:1,candidate_id:None });
        passed.depends_on = Some("0:probe_candidates:1".into());
        plan.steps.push(passed);
        let participants = vec![
            Participant { gateway_id:"eu".into(),region:"eu1".into(),boot_id:uuid::Uuid::new_v4().to_string() },
            Participant { gateway_id:"us".into(),region:"us3".into(),boot_id:uuid::Uuid::new_v4().to_string() },
        ];
        let policy: RegionalPolicy = serde_json::from_value(json!({"version":1,"regions":[
            {"region":"eu1","weight":0,"gateways":[{"id":"eu","backendServerId":"backend-eu","url":"https://eu.example"}]},
            {"region":"us3","weight":7,"gateways":[{"id":"us","backendServerId":"backend-us","url":"https://us.example"}]}
        ]}))?;
        regional_policy::store(&db,"candidate-svc",&regional_policy::PutPolicy {expected_version:0,policy:Some(policy.clone())}).await?;
        let archive = "a".repeat(64);
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_service_rollouts(operation_id,service_id,request_hash,deployment_request,target_revision,observer_topology,baseline_state,regions,slots,plan,minimum_serving_replicas,bake_seconds,drain_timeout_seconds,status,phase)
             VALUES('op','candidate-svc','hash',$1,$2,$3,'{}','[\"eu1\"]','[]',$4,1,1,30,'running','publish_policy')",
            vec![json!({"deploymentEnvironment":"test","placementPool":"shared","expectedRuntimeRevision":"app-revision"}).into(),
                archive.clone().into(),serde_json::to_string(&participants)?.into(),serde_json::to_value(plan)?.into()])).await?;
        let request = request();
        assert!(claim(&db,"candidate-svc","op",&request,&archive,"app-revision",8080).await.is_err());
        regional_policy::publish_proposal(&db,"candidate-svc","op",&policy,None).await?;
        for tick in 1..=6 {
            for participant in &participants {
                regional_reports::record(&db,"candidate-svc","op",&Report {gateway_id:participant.gateway_id.clone(),
                    boot_id:participant.boot_id.clone(),sequence:tick,generation:1,prepared:true,adopted:true,
                    outgoing_target:0,local_target:0,peer_admission_closed:true},0).await?;
            }
            if tick == 2 { regional_policy::activate_proposal(&db,"candidate-svc","op",1).await?; }
            else { assert!(regional_reports::advance(&db,"candidate-svc","op",1).await?); }
        }
        db.execute_unprepared("UPDATE regional_gateway_reports SET observed_at=clock_timestamp()-interval '1 minute'").await?;
        assert!(claim(&db,"candidate-svc","op",&request,&archive,"app-revision",8080).await.is_err(), "stale drain cannot grant create");
        for participant in &participants {
            regional_reports::record(&db,"candidate-svc","op",&Report {gateway_id:participant.gateway_id.clone(),
                boot_id:participant.boot_id.clone(),sequence:7,generation:1,prepared:true,adopted:true,
                outgoing_target:0,local_target:0,peer_admission_closed:true},0).await?;
        }
        for allowed in [None, Some(vec![]), Some(vec!["backend-other".into()])] {
            let mut changed = request.clone(); changed.allowed_backend_server_ids = allowed;
            let error = claim(&db,"candidate-svc","op",&changed,&archive,"app-revision",8080).await.unwrap_err();
            assert!(error.to_string().contains("canonical pinned regional fleet"), "{error:#}");
        }
        let restarted = sea_orm::Database::connect(options).await?;
        let (a,b) = tokio::join!(claim(&db,"candidate-svc","op",&request,&archive,"app-revision",8080),
            claim(&restarted,"candidate-svc","op",&request,&archive,"app-revision",8080));
        assert_eq!([a.as_ref(),b.as_ref()].iter().filter(|r| r.is_ok_and(|(_,send)| *send)).count(),1);
        let (intent,send) = claim(&restarted,"candidate-svc","op",&request,&archive,"app-revision",8080).await?;
        assert!(!send, "restart must never resend, including a crash before the first POST");
        assert!(publish(&db,"candidate-svc","op").await.is_err(), "no discovery before a validated receipt");
        let mut changed = request.clone(); changed.image = "other".into();
        assert!(claim(&db,"candidate-svc","op",&changed,&archive,"app-revision",8080).await.is_err());
        assert!(claim(&db,"candidate-svc","op",&request,&archive,"wrong-runtime-revision",8080).await.is_err());
        let stored = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT intent FROM regional_candidate_creations WHERE operation_id='op'")).await?.unwrap().try_get::<Value>("","intent")?;
        assert!(!stored.to_string().contains("test-only-secret-value"));
        assert!(db.execute_unprepared("UPDATE regional_candidate_creations SET intent='{}'").await.is_err());
        assert!(db.execute_unprepared("DELETE FROM regional_candidate_creations").await.is_err());
        let payload = Arc::new(tokio::sync::Mutex::new(Value::Null));
        let app = axum::Router::new().route("/internal/orchestration/deployments/{id}",axum::routing::get(
            |axum::extract::State(payload):axum::extract::State<Arc<tokio::sync::Mutex<Value>>>,headers:axum::http::HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(),"Bearer test");
                axum::Json(payload.lock().await.clone())
            })).with_state(payload.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}",listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener,app).await.unwrap() });
        let state = AppState { config:Arc::new(serde_json::from_value(json!({"server_port":0,"database_url":"unused",
            "agent_provider":"test","agent_model":"test","agent_api_key":"","agent_timeout_seconds":1,
            "agent_max_iterations":1,"jwt_secret":"test","cloud_internal_url":base,"internal_api_key":"test"}))?),
            http_client:reqwest::Client::new(),worker_id:Arc::new("test".into()),ci_workspace_cache:Default::default() };
        assert!(recover(&state,&restarted,"candidate-svc","op",step_id).await.is_err(), "unknown Cloud outcome stays fenced");
        *payload.lock().await = receipt(&intent.request_digest);
        let first = recover(&state,&restarted,"candidate-svc","op",step_id).await?;
        assert_eq!(first.backend_sandbox_id,"runtime-a");
        assert_eq!(recover(&state,&db,"candidate-svc","op",step_id).await?,first);
        payload.lock().await["backendSandboxId"] = json!("replacement-runtime");
        assert!(recover(&state,&restarted,"candidate-svc","op",step_id).await.is_err(), "a later runtime must not rewrite the receipt");
        let stored = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT receipt FROM regional_candidate_creations WHERE operation_id='op'")).await?.unwrap().try_get::<Value>("","receipt")?;
        assert_eq!(stored["backendSandboxId"],"runtime-a");
        db.execute_unprepared("CREATE FUNCTION fail_candidate_journal() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'injected journal failure'; END; $$ LANGUAGE plpgsql;
            CREATE TRIGGER fail_candidate_journal BEFORE INSERT ON regional_rollout_events FOR EACH ROW EXECUTE FUNCTION fail_candidate_journal()").await?;
        assert!(publish(&db,"candidate-svc","op").await.is_err());
        assert!(db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT 1 FROM service_discovery_endpoints WHERE service_id='candidate-svc'")).await?.is_none(), "journal failure rolls back discovery");
        db.execute_unprepared("DROP TRIGGER fail_candidate_journal ON regional_rollout_events").await?;
        publish(&restarted,"candidate-svc","op").await?;
        let endpoint = db.query_one(Statement::from_string(DbBackend::Postgres,
            "SELECT e.*,r.phase,i.status AS item_status FROM service_discovery_endpoints e
             JOIN regional_service_rollouts r USING(service_id) JOIN regional_rollout_items i USING(operation_id)
             WHERE e.service_id='candidate-svc' AND i.step_id='0:create_candidate:1'")).await?.unwrap();
        assert_eq!(endpoint.try_get::<String>("","backend_server_id")?,"backend-eu");
        assert_eq!(endpoint.try_get::<String>("","backend_url")?,"http://127.0.0.1:18081");
        assert_eq!(endpoint.try_get::<String>("","revision")?,"app-revision", "archive SHA is not the application revision");
        assert_eq!(endpoint.try_get::<String>("","health_status")?,"unknown");
        assert!(endpoint.try_get::<bool>("","draining")?);
        assert_eq!(endpoint.try_get::<String>("","phase")?,"probe_candidates");
        assert_eq!(endpoint.try_get::<String>("","item_status")?,"completed");
        assert!(publish(&db,"candidate-svc","op").await.is_err(), "old item cannot republish after cursor advancement");
        server.abort();
        root.execute_unprepared(&format!("DROP SCHEMA {schema} CASCADE")).await?;
        Ok(())
    }
}
