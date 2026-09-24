//! Exact-instance lifecycle barrier owned by the persisted regional operation.
//! The application supplies its own quiescence proof; this module knows no CI
//! database tables, jobs, leases, or executor election rules.
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use crate::{cloud_client, AppState};
use super::{instance_http::Contract, service_discovery::ServiceDiscoverySnapshot};

fn hash(value: &Value) -> Result<String> {
    let mut value = value.clone(); value.sort_all_objects();
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
}

async fn call(state: &AppState, contract: &Contract, token: &str, target: &Value,
    method: &str, path: String, body: Value) -> Result<(u16, Value)> {
    let mut headers = vec![("authorization".into(),format!("Bearer {token}")),("content-type".into(),"application/json".into())];
    if let Some(boot) = target["bootId"].as_str() { headers.push(("x-ci-target-boot".into(),boot.into())); }
    let request = cloud_client::ExactRuntimeHttpRequest {
        expected_backend_server_id: target["backendServerId"].as_str().context("target backend missing")?.into(),
        expected_backend_sandbox_id: target["backendSandboxId"].as_str().context("target sandbox missing")?.into(),
        port: contract.port, method: method.into(), path, headers,
    };
    let bytes = if method == "GET" {Vec::new()} else {serde_json::to_vec(&body)?};
    let (metadata, mut response) = cloud_client::exact_runtime_http(state,
        target["deploymentId"].as_str().context("target deployment missing")?, &request, bytes.into()).await?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(body.len()+chunk.len() <= 1024*1024, "application lifecycle response exceeds 1MiB");
        body.extend_from_slice(&chunk);
    }
    Ok((metadata.status,serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

async fn identify(state: &AppState, contract: &Contract, token: &str, service: &str,
    endpoint: &super::service_discovery::ServiceDiscoveryEndpoint) -> Result<Value> {
    let binding = cloud_client::observe_retained_deployment(state,&endpoint.deployment_id,contract.port).await?;
    anyhow::ensure!(endpoint.backend_server_id.as_deref() == Some(&binding.backend_server_id)
        && endpoint.region.as_deref() == Some(&binding.region), "application runtime membership changed");
    let mut target = json!({"deploymentId":binding.deployment_id,"backendServerId":binding.backend_server_id,
        "backendSandboxId":binding.backend_sandbox_id,"region":binding.region});
    let (status, identity) = call(state,contract,token,&target,"GET","/api/lifecycle".into(),Value::Null).await?;
    anyhow::ensure!(status == 200 && identity["applicationId"] == service && identity["deploymentId"] == target["deploymentId"]
        && identity["capabilities"].as_array().is_some_and(|v|v.iter().any(|c|c == "managed-retirement-v1")),
        "application exact-instance lifecycle capability unavailable");
    let boot: uuid::Uuid = identity["bootId"].as_str().context("application boot missing")?.parse()?;
    anyhow::ensure!(!boot.is_nil(), "application boot missing");
    target["bootId"] = json!(boot);
    Ok(target)
}

/// Caller holds the existing service lifecycle lock. Pin the whole target set
/// and retiring revision's contract before sending the first retirement command.
pub(super) async fn before_withdrawal(state: &AppState, db: &impl ConnectionTrait,
    operation: &str, step: &str, service: &str, region: &str, metadata: &Value,
    snapshot: &ServiceDiscoverySnapshot) -> Result<bool> {
    let Some(contract) = Contract::from_metadata(metadata)? else { return Ok(true); };
    let existing = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT contract,commands,receipts FROM regional_lifecycle_barriers WHERE operation_id=$1 AND step_id=$2",
        [operation.into(),step.into()])).await?;
    let (contract, commands, mut receipts): (Contract, Vec<Value>, Value) = if let Some(row) = existing {
        (serde_json::from_value(row.try_get("","contract")?)?,serde_json::from_value(row.try_get("","commands")?)?,row.try_get("","receipts")?)
    } else {
        let token = contract.token(state).await?;
        let mut targets = Vec::new(); let mut survivors = Vec::new();
        for endpoint in snapshot.endpoints.iter().filter(|e| !e.draining) {
            let identity = identify(state,&contract,&token,service,endpoint).await?;
            if endpoint.region.as_deref() == Some(region) {targets.push(identity);} else {survivors.push(identity);}
        }
        anyhow::ensure!(!targets.is_empty() && !survivors.is_empty(), "lifecycle barrier has no retiring or surviving instances");
        let commands = targets.into_iter().map(|target| -> Result<Value> {
            let id = hash(&json!([operation,step,target]))?;
            let request = json!({"commandId":id,"operationId":operation,"stepId":step,"serviceId":service,
                "target":target,"survivors":survivors});
            Ok(json!({"requestHash":hash(&request)?,"request":request}))
        }).collect::<Result<Vec<_>>>()?;
        db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
            "INSERT INTO regional_lifecycle_barriers(operation_id,step_id,contract,commands) VALUES($1,$2,$3,$4)",
            vec![operation.into(),step.into(),serde_json::to_value(&contract)?.into(),serde_json::to_value(&commands)?.into()])).await?;
        (contract,commands,json!({}))
    };
    let token = contract.token(state).await?;
    for command in &commands {
        let request=&command["request"]; let target=&request["target"];
        let id=request["commandId"].as_str().context("command missing")?;
        if receipts.get(id).is_some() {continue;}
        let path=format!("/api/lifecycle/retirements/{id}");
        let status_path=format!("{path}?requestHash={}",command["requestHash"].as_str().context("hash missing")?);
        let mut observed=None;
        // Shared receipts may be read from a survivor after a lost target reply.
        // Only the addressed boot can create its local-fence acknowledgment.
        for reader in std::iter::once(target).chain(request["survivors"].as_array().context("survivors missing")?) {
            if let Ok((200,status))=call(state,&contract,&token,reader,"GET",status_path.clone(),Value::Null).await {
                observed=Some(status); break;
            }
        }
        if let Some(status)=observed {
            anyhow::ensure!(status["commandId"] == request["commandId"] && status["requestHash"] == command["requestHash"],
                "lifecycle status identity mismatch");
            if status["status"] == "safe-to-retire" {
                let receipt=&status["receipt"];
                anyhow::ensure!(receipt["target"] == *target && receipt["operationId"] == operation
                    && receipt["stepId"] == step && receipt["serviceId"] == service
                    && receipt["requestHash"] == command["requestHash"], "lifecycle receipt identity mismatch");
                receipts[id]=receipt.clone();
                db.execute(Statement::from_sql_and_values(DbBackend::Postgres,
                    "UPDATE regional_lifecycle_barriers SET receipts=$3 WHERE operation_id=$1 AND step_id=$2",
                    vec![operation.into(),step.into(),receipts.clone().into()])).await?;
                continue;
            }
            anyhow::ensure!(status["status"] == "pending", "application lifecycle is blocked");
        }
        // Retrying this immutable command is allowed; an ambiguous response is
        // pending, never permission to select another boot or perform teardown.
        if let Ok((status,_))=call(state,&contract,&token,target,"POST",path,command.clone()).await {
            anyhow::ensure!(matches!(status,200|202), "application retirement command rejected");
        }
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Bytes, extract::Path, http::{HeaderMap, StatusCode}, response::IntoResponse, routing::{get,post}, Json, Router};
    use base64::Engine;
    use std::sync::{Arc, Mutex};

    /// PostgreSQL-backed controller restart/lost-response fixture. Application
    /// receipts are protocol doubles; CI's real ownership SQL is tested in CI.
    #[tokio::test]
    #[ignore = "requires disposable ORCHESTRATOR_TEST_DATABASE_URL"]
    async fn lifecycle_barrier_pins_targets_and_recovers_lost_reply_after_restart() -> Result<()> {
        let base=std::env::var("ORCHESTRATOR_TEST_DATABASE_URL")?;
        let admin=sea_orm::Database::connect(&base).await?;
        let schema=format!("lifecycle_{}",uuid::Uuid::new_v4().simple());
        admin.execute_unprepared(&format!("CREATE SCHEMA {schema}")).await?;
        let mut url=reqwest::Url::parse(&base)?;
        url.query_pairs_mut().append_pair("options",&format!("-c search_path={schema}"));
        let db=sea_orm::Database::connect(url.as_str()).await?;
        db.execute_unprepared("CREATE TABLE regional_service_rollouts(operation_id TEXT PRIMARY KEY); INSERT INTO regional_service_rollouts VALUES('op')").await?;
        db.execute_unprepared(include_str!("../../migrations/044_regional_lifecycle_barriers.sql")).await?;
        let observations=Arc::new(Mutex::new(std::collections::HashMap::<String,Value>::new()));
        let posts=Arc::new(Mutex::new(Vec::<Value>::new()));
        let boots=Arc::new(std::collections::HashMap::from([("us-old".to_owned(),uuid::Uuid::new_v4()),
            ("eu-old".to_owned(),uuid::Uuid::new_v4()),("us-new".to_owned(),uuid::Uuid::new_v4())]));
        let cloud=Router::new()
            .route("/v1/secrets/read",post(|| async {Json(json!({"path":"apps/ci/lifecycle","version":1,"status":"active",
                "valueBase64":base64::engine::general_purpose::STANDARD.encode("app-token"),"createdAt":chrono::Utc::now(),"metadata":{}}))}))
            .route("/internal/orchestration/deployments/{id}/binding",get(|Path(id):Path<String>| async move {
                let region=if id.starts_with("us") {"us3"} else {"eu1"};
                Json(json!({"deploymentId":id,"archiveId":"archive","backendServerId":format!("host-{region}"),
                    "backendSandboxId":format!("sb-{id}"),"nodeId":"node","region":region,"deploymentEnvironment":"prod",
                    "placementPool":"platform","guestPort":8080,"hostLocalUrl":"http://127.0.0.1:18080","observedAt":chrono::Utc::now()}))
            }))
            .route("/internal/orchestration/deployments/{id}/http-request",post({
                let observations=observations.clone(); let posts=posts.clone(); let boots=boots.clone();
                move |Path(id):Path<String>,headers:HeaderMap,body:Bytes| {
                    let observations=observations.clone(); let posts=posts.clone(); let boots=boots.clone();
                    async move {
                        let meta:Value=serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(headers["x-heyo-runtime-request"].as_bytes()).unwrap()).unwrap();
                        assert_eq!(meta["headers"][0],json!(["authorization","Bearer app-token"]));
                        let path=meta["path"].as_str().unwrap();
                        let (status,response)=if path == "/api/lifecycle" {
                            (200,json!({"applicationId":"ci","deploymentId":id,"bootId":boots[&id],"capabilities":["managed-retirement-v1"]}))
                        } else if meta["method"] == "POST" {
                            let command:Value=serde_json::from_slice(&body).unwrap();
                            let request=&command["request"]; let key=request["commandId"].as_str().unwrap().to_owned();
                            assert_eq!(hash(request).unwrap(),command["requestHash"].as_str().unwrap());
                            let mut observations=observations.lock().unwrap();
                            assert!(!observations.contains_key(&key),"lost reply must read the receipt, not transfer twice");
                            posts.lock().unwrap().push(command.clone());
                            let mut receipt=request.clone(); receipt.as_object_mut().unwrap().remove("survivors");
                            receipt["requestHash"]=command["requestHash"].clone();
                            observations.insert(key.clone(),json!({"commandId":key,"requestHash":command["requestHash"],"status":"safe-to-retire","receipt":receipt}));
                            return StatusCode::BAD_GATEWAY.into_response(); // committed, reply lost
                        } else {
                            let key=path.split('/').next_back().unwrap().split('?').next().unwrap();
                            observations.lock().unwrap().get(key).cloned().map(|v|(200,v)).unwrap_or((404,Value::Null))
                        };
                        let envelope=json!({"backendServerId":meta["expectedBackendServerId"],"backendSandboxId":meta["expectedBackendSandboxId"],"status":status,"headers":[]});
                        ([("x-heyo-runtime-response",base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).unwrap()))],Json(response)).into_response()
                    }
                }
            }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base=format!("http://{}",listener.local_addr()?);
        let server=tokio::spawn(async move {axum::serve(listener,cloud).await.unwrap()});
        let config: Arc<crate::config::Config>=Arc::new(serde_json::from_value(json!({"server_port":0,"database_url":"postgres://unused","agent_provider":"test",
            "agent_model":"test","agent_api_key":"","agent_timeout_seconds":1,"agent_max_iterations":1,"jwt_secret":"test",
            "cloud_internal_url":base,"heyosecret_url":base,"internal_api_key":"test"}))?);
        let make_state=|| AppState {config:config.clone(),http_client:reqwest::Client::new(),worker_id:Arc::new("controller".into()),ci_workspace_cache:Default::default()};
        let endpoint=|id:&str,region:&str| super::super::service_discovery::ServiceDiscoveryEndpoint {deployment_id:id.into(),
            backend_server_id:Some(format!("host-{region}")),region:Some(region.into()),revision:Some("revision".into()),
            url:"http://127.0.0.1:18080".into(),health_status:"healthy".into(),draining:false};
        let mut snapshot=ServiceDiscoverySnapshot {service_id:"ci".into(),version:1,regional_policy:None,
            endpoints:vec![endpoint("us-old","us3"),endpoint("eu-old","eu1")],updated_at:chrono::Utc::now()};
        let metadata=json!({"applicationLifecycle":{"port":8080,"tokenSecretPath":"apps/ci/lifecycle"}});
        assert!(!before_withdrawal(&make_state(),&db,"op","us-step","ci","us3",&metadata,&snapshot).await?);
        // A changed inventory after restart must not replace the pinned set.
        snapshot.endpoints.push(endpoint("must-not-be-selected","us3"));
        assert!(before_withdrawal(&make_state(),&db,"op","us-step","ci","us3",&metadata,&snapshot).await?);
        snapshot.endpoints=vec![endpoint("us-new","us3"),endpoint("eu-old","eu1")];
        assert!(!before_withdrawal(&make_state(),&db,"op","eu-step","ci","eu1",&metadata,&snapshot).await?);
        assert!(before_withdrawal(&make_state(),&db,"op","eu-step","ci","eu1",&metadata,&snapshot).await?);
        let posts=posts.lock().unwrap(); assert_eq!(posts.len(),2);
        assert_eq!(posts[0]["request"]["survivors"][0]["deploymentId"],"eu-old");
        assert_eq!(posts[1]["request"]["survivors"][0]["deploymentId"],"us-new");
        drop(posts);
        assert!(before_withdrawal(&make_state(),&db,"op","plain-step","plain","us3",&Value::Null,&snapshot).await?);
        server.abort(); Ok(())
    }
}
