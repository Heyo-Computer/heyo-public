//! App-scoped revision submission into the normal managed regional plan. There
//! is no second dispatcher and the app cannot supply runtime/secret recipes.
use anyhow::{Context,Result};
use axum::{extract::{Path,State},http::{HeaderMap,StatusCode},Json};
use sea_orm::{ConnectionTrait,DbBackend,Statement};
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use super::{instance_http::Contract,regional_admission,regional_plan::Plan,regional_rollout,service_deploy,service_discovery,service_recipe};
use crate::{auth,db,AppState};

#[derive(Clone,Deserialize,Serialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct Update {
    pub operation_id:String,
    pub archive_id:String,
    pub archive_sha256:String,
    pub runtime_revision:String,
}

async fn authorize(state:&AppState,service:&str,headers:&HeaderMap) -> Result<()> {
    let stored=service_deploy::read_service_state_in(db::get_db()?,service).await?.context("managed app missing")?;
    let contract=Contract::from_metadata(&stored.active_metadata["source"])?.context("managed lifecycle missing")?;
    let token=contract.token(state).await?;
    auth::require_internal_api_key(headers,&token).map_err(|_|anyhow::anyhow!("application authentication refused"))?;
    super::service_adoption::ensure_managed(db::get_db()?,service).await
}

pub async fn create(State(state):State<AppState>,Path(service):Path<String>,headers:HeaderMap,Json(request):Json<Update>) -> (StatusCode,Json<Value>) {
    if authorize(&state,&service,&headers).await.is_err() {return (StatusCode::UNAUTHORIZED,Json(json!({"error":"application authorization refused"})))}
    match accept(&state,db::get_db().unwrap(),&service,&request).await {
        Ok(value)=>(StatusCode::ACCEPTED,Json(value)),
        Err(error)=>{tracing::warn!(%error,"managed update not admitted");(StatusCode::CONFLICT,Json(json!({"error":"managed update not admitted"})))}
    }
}

pub(super) async fn accept(state:&AppState,db:&sea_orm::DatabaseConnection,service:&str,request:&Update) -> Result<Value> {
    anyhow::ensure!(!request.operation_id.is_empty() && request.operation_id.len()<=128
        && request.operation_id.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_".contains(&b))
        && !request.archive_id.is_empty() && !request.runtime_revision.is_empty()
        && request.archive_sha256.len()==64 && request.archive_sha256.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase()),
        "invalid managed revision command");
    super::service_adoption::ensure_managed(db,service).await?;
    let command=serde_json::to_value(request)?;
    if let Some(row)=db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT service_id,deployment_request FROM regional_service_rollouts WHERE operation_id=$1",[request.operation_id.clone().into()])).await? {
        let input:Value=row.try_get("","deployment_request")?;
        anyhow::ensure!(row.try_get::<String>("","service_id")?==service && input["metadata"]["managedUpdate"]==command,
            "managed update changed on replay");
        return observation(state,db,service,&request.operation_id,false).await;
    }
    let stored=service_deploy::read_service_state_in(db,service).await?.context("managed app missing")?;
    let contract=Contract::from_metadata(&stored.active_metadata["source"])?.context("managed lifecycle missing")?;
    let snapshot=service_discovery::read_snapshot_in(db,service,false).await?.context("managed discovery missing")?;
    let primary=stored.active_deployment_id.as_deref().context("managed app has no baseline")?;
    let mut deployment=service_recipe::load(db,service,primary).await?.request;
    deployment.archive_id=Some(request.archive_id.clone());
    deployment.archive_name=None;
    deployment.route=stored.route;
    deployment.desired_replicas=Some(stored.desired_replicas);
    deployment.replica_regions=stored.replica_regions;
    deployment.metadata=Some(stored.active_metadata["source"].clone());
    deployment.metadata.as_mut().unwrap()["managedUpdate"]=command;
    let policy=db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT a.generation,r.baseline_state FROM service_active_regional_policies a
         JOIN regional_policy_proposals p USING(service_id,generation)
         JOIN regional_service_rollouts r ON r.operation_id=p.operation_id WHERE a.service_id=$1",[service.into()])).await?.context("active regional authority missing")?;
    let baseline:Value=policy.try_get("","baseline_state")?;
    let fleet=baseline["regionalFleet"].as_array().context("pinned gateway fleet missing")?;
    let namespace=fleet.first().and_then(|g|g["admission"]["namespace"].as_str()).context("pinned namespace missing")?;
    anyhow::ensure!(fleet.iter().all(|g|g["admission"]["namespace"]==namespace),"gateway namespaces differ");
    let application=regional_admission::ApplicationRequest {rollout:regional_rollout::RegionalRolloutRequest {
        operation_id:request.operation_id.clone(),deployment,minimum_serving_replicas:1,bake_seconds:30,
        drain_timeout_seconds:3600,runtime_by_region:Default::default()},expected_version:snapshot.version.try_into()?,
        expected_generation:policy.try_get("","generation")?,namespace:namespace.into(),runtime_revision:request.runtime_revision.clone(),guest_port:contract.port};
    regional_admission::admit_application(state,db,&application).await?;
    observation(state,db,service,&request.operation_id,false).await
}

pub async fn get(State(state):State<AppState>,Path((service,operation)):Path<(String,String)>,headers:HeaderMap) -> (StatusCode,Json<Value>) {
    if authorize(&state,&service,&headers).await.is_err() {return (StatusCode::UNAUTHORIZED,Json(json!({"error":"application authorization refused"})))}
    match observation(&state,db::get_db().unwrap(),&service,&operation,true).await {
        Ok(value)=>(StatusCode::OK,Json(value)),
        Err(error)=>{tracing::warn!(%error,"managed completion unavailable");(StatusCode::SERVICE_UNAVAILABLE,Json(json!({"error":"managed completion unavailable"})))}
    }
}

pub(super) async fn observation(state:&AppState,db:&impl ConnectionTrait,service:&str,operation:&str,verify:bool) -> Result<Value> {
    let row=db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT status,phase,plan,deployment_request,baseline_state FROM regional_service_rollouts WHERE service_id=$1 AND operation_id=$2",
        [service.into(),operation.into()])).await?.context("managed update missing")?;
    let status:String=row.try_get("","status")?;
    let input:Value=row.try_get("","deployment_request")?;
    let plan:Plan=serde_json::from_value(row.try_get("","plan")?)?;
    anyhow::ensure!(plan.version==3 && input["metadata"]["managedUpdate"]["operationId"]==operation,"not a managed revision command");
    let mut targets=Vec::new();
    let snapshot=if status=="passed" && verify {service_discovery::read_snapshot_in(db,service,false).await?} else {None};
    let baseline:Value=row.try_get("","baseline_state")?;
    let contract=Contract::from_metadata(&baseline["activeMetadata"]["source"])?.context("retiring contract missing")?;
    let token=if snapshot.is_some(){Some(contract.token(state).await?)}else{None};
    for step in plan.steps.iter().filter(|s|s.phase=="create_candidate") {
        let id=step.candidate_id.as_deref().context("candidate missing")?;
        let mut target=json!({"deploymentId":id,"region":step.region,"revision":input["expectedRuntimeRevision"]});
        if let Some(snapshot)=&snapshot {
            let endpoint=snapshot.endpoints.iter().find(|e|e.deployment_id==id && !e.draining && e.health_status=="healthy")
                .context("completed candidate is not serving")?;
            anyhow::ensure!(endpoint.region==step.region && endpoint.revision.as_deref()==input["expectedRuntimeRevision"].as_str(),"completed candidate differs from plan");
            let identity=super::regional_lifecycle::identify(state,&contract,token.as_deref().unwrap(),service,endpoint).await?;
            target["bootId"]=identity["bootId"].clone();
            target["backendServerId"]=identity["backendServerId"].clone();
            target["backendSandboxId"]=identity["backendSandboxId"].clone();
        }
        targets.push(target);
    }
    Ok(json!({"operationId":operation,"serviceId":service,"request":input["metadata"]["managedUpdate"],
        "status":status,"phase":row.try_get::<String>("","phase")?,"targets":targets,"verified":snapshot.is_some()}))
}
