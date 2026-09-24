//! Durable application commands. CI executes its own job drain; only app-lb
//! replaces its retained workspace. This dispatcher never creates a second VM.
use anyhow::{Context, Result};
use axum::{extract::{Path, State}, http::{HeaderMap, StatusCode}, Json};
use heyosecret_client::{HeyoSecretClient, HeyoSecretClientOptions};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use crate::{auth, config::ExternalServiceBinding, db, AppState};
use super::service_deploy;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateRequest { operation_id: String, intent_hash: String }

fn binding<'a>(state: &'a AppState, service: &str) -> Result<&'a ExternalServiceBinding> {
    state.config.external_service_bindings.iter().find(|b| b.service_id == service)
        .context("application lifecycle is not configured")
}

async fn token(state: &AppState, binding: &ExternalServiceBinding) -> Result<String> {
    anyhow::ensure!(!binding.lifecycle_token_secret_path.trim().is_empty(), "application lifecycle credential is not configured");
    let secrets = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url:state.config.heyosecret_url.clone(),
        token:if state.config.heyosecret_internal_api_key.is_empty() {state.config.internal_api_key.clone()}
            else {state.config.heyosecret_internal_api_key.clone()}, timeout:Some(Duration::from_secs(10)) })?;
    let value = String::from_utf8(secrets.read_active(&binding.lifecycle_token_secret_path).await?.value)?;
    anyhow::ensure!(!value.is_empty(), "application lifecycle credential is empty");
    Ok(value)
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10)).build()?)
}

pub(super) async fn verify_ready(state: &AppState, binding: &ExternalServiceBinding) -> Result<()> {
    let bearer = token(state,binding).await?;
    let url = endpoint(binding,"probe")?.join("/api/lifecycle")?;
    let response = client()?.get(url).bearer_auth(bearer).send().await?;
    anyhow::ensure!(response.status().is_success(), "application lifecycle endpoint is not ready");
    let identity: Value = response.json().await?;
    anyhow::ensure!(identity["applicationId"] == binding.service_id
        && identity["deploymentId"] == binding.deployment_id
        && identity["capabilities"].as_array().is_some_and(|v|v.iter().any(|c|c == "release-update")),
        "application lifecycle identity or capability mismatch");
    Ok(())
}

fn endpoint(binding: &ExternalServiceBinding, id: &str) -> Result<reqwest::Url> {
    anyhow::ensure!(!id.is_empty() && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)), "invalid operation ID");
    let url = reqwest::Url::parse(&binding.health_origin)?;
    anyhow::ensure!(matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
        && url.path() == "/" && url.query().is_none() && url.fragment().is_none()
        && url.username().is_empty() && url.password().is_none(), "invalid application origin");
    Ok(url.join(&format!("api/lifecycle/updates/{id}"))?)
}

fn verify(binding: &ExternalServiceBinding, id: &str, hash: &str, intent: &Value) -> Result<()> {
    anyhow::ensure!(intent["operationId"] == id && intent["applicationId"] == binding.service_id
        && intent["intentHash"] == hash && intent["deploymentId"] == binding.deployment_id,
        "application update identity mismatch");
    anyhow::ensure!(intent["authority"].as_str().map(|s| s.trim_end_matches('/'))
        == Some(binding.authority.trim_end_matches('/')), "application runtime authority mismatch");
    Ok(())
}

async fn read(binding: &ExternalServiceBinding, id: &str, bearer: &str) -> Result<Value> {
    let response = client()?.get(endpoint(binding,id)?).bearer_auth(bearer).send().await?;
    anyhow::ensure!(response.status().is_success(), "application update status unavailable");
    Ok(response.json().await?)
}

async fn accept(binding: &ExternalServiceBinding, r: &UpdateRequest, bearer: &str) -> Result<Value> {
    let intent = read(binding,&r.operation_id,bearer).await?;
    verify(binding,&r.operation_id,&r.intent_hash,&intent)?;
    let tx = service_deploy::try_service_lifecycle_lock(db::get_db()?,&binding.service_id).await?
        .context("application lifecycle is busy")?;
    let registered = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT authority,deployment_id,namespace FROM external_service_bindings WHERE service_id=$1",[binding.service_id.clone().into()])).await?
        .context("application has not been adopted")?;
    anyhow::ensure!(registered.try_get::<String>("","authority")?.trim_end_matches('/') == binding.authority.trim_end_matches('/')
        && registered.try_get::<String>("","deployment_id")? == binding.deployment_id
        && registered.try_get::<String>("","namespace")? == binding.namespace, "application binding changed after adoption");
    let receipt = json!({"operationId":r.operation_id,"intentHash":r.intent_hash});
    if let Some(row) = tx.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT service_id,intent_hash FROM application_updates WHERE operation_id=$1",[r.operation_id.clone().into()])).await? {
        anyhow::ensure!(row.try_get::<String>("","service_id")? == binding.service_id
            && row.try_get::<String>("","intent_hash")? == r.intent_hash, "application update changed on replay");
        tx.commit().await?;
        return Ok(receipt);
    }
    anyhow::ensure!(intent["phase"] == "prepared", "application update must be a prepared release intent");
    // The FK requires prior attested adoption; the partial index serializes updates.
    tx.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO application_updates(operation_id,service_id,intent_hash,intent) VALUES($1,$2,$3,$4)",
        vec![r.operation_id.clone().into(),binding.service_id.clone().into(),r.intent_hash.clone().into(),intent.into()])).await?;
    tx.commit().await?;
    Ok(receipt)
}

pub async fn create(State(state): State<AppState>, Path(service): Path<String>, headers: HeaderMap,
    Json(r): Json<UpdateRequest>) -> (StatusCode,Json<Value>) {
    let Ok(binding) = binding(&state,&service) else {
        return (StatusCode::NOT_FOUND,Json(json!({"error":"application lifecycle is not configured"})));
    };
    let Ok(bearer) = token(&state,binding).await else {
        return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"error":"application credential unavailable"})));
    };
    if let Err(status) = auth::require_internal_api_key(&headers,&bearer) {
        return (status,Json(json!({"error":"Unauthorized"})));
    }
    match accept(binding,&r,&bearer).await {
        Ok(receipt) => (StatusCode::ACCEPTED,Json(receipt)),
        Err(error) => { tracing::warn!(%error,service,"application update not accepted");
            (StatusCode::CONFLICT,Json(json!({"error":"application update was not accepted"}))) }
    }
}

async fn reconcile(state: &AppState, service: &str, id: &str, hash: &str) -> Result<()> {
    let binding = binding(state,service)?;
    let bearer = token(state,binding).await?;
    let mut observed = read(binding,id,&bearer).await?;
    verify(binding,id,hash,&observed)?;
    if observed["phase"] == "prepared" {
        let response = client()?.post(endpoint(binding,id)?).bearer_auth(&bearer)
            .json(&json!({"intentHash":hash})).send().await?;
        anyhow::ensure!(response.status().is_success(), "application activation unresolved");
        observed = read(binding,id,&bearer).await?;
        verify(binding,id,hash,&observed)?;
    }
    let status = match observed["status"].as_str() {
        Some("passed") if observed["phase"] == "complete" => "passed",
        Some("failed") if observed["phase"] == "complete" => "failed",
        _ => "running",
    };
    db::get_db()?.execute(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE application_updates SET status=$2,observation=$3,observed_at=now(),updated_at=now(),error=NULL WHERE operation_id=$1 AND status IN ('accepted','running')",
        vec![id.into(),status.into(),observed.into()])).await?;
    Ok(())
}

pub async fn run_reconciler(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let result: Result<()> = async {
            let database = db::get_db()?;
            let rows = database.query_all(Statement::from_string(DbBackend::Postgres,
                "SELECT operation_id,service_id,intent_hash FROM application_updates WHERE status IN ('accepted','running') ORDER BY updated_at LIMIT 100")).await?;
            for row in rows {
                let id: String = row.try_get("","operation_id")?;
                let service: String = row.try_get("","service_id")?;
                let hash: String = row.try_get("","intent_hash")?;
                if let Err(error) = reconcile(&state,&service,&id,&hash).await {
                    tracing::warn!(%error,%id,"application update reconciliation blocked");
                    database.execute(Statement::from_sql_and_values(DbBackend::Postgres,
                        "UPDATE application_updates SET error='Application lifecycle unavailable; outcome unknown',updated_at=now() WHERE operation_id=$1 AND status IN ('accepted','running')",[id.into()])).await?;
                }
            }
            Ok(())
        }.await;
        if let Err(error) = result { tracing::warn!(%error,"application reconciler unavailable"); }
    }
}

/// Runs in the adoption test's disposable database after installing the binding.
#[cfg(test)]
pub(super) async fn test_durable_updates(state: &AppState) -> Result<()> {
    use axum::{Router, routing::get};
    use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};
    let binding = binding(state,"ci")?.clone();
    let observed = Arc::new(Mutex::new(json!({"applicationId":"ci","deploymentId":"ci-eu1",
        "authority":binding.authority,"intentHash":"exact-hash","phase":"prepared","status":"running",
        "targetRevision":"next-revision","artifactDigest":"next-artifact","runId":"release-run"})));
    let activations = Arc::new(AtomicUsize::new(0));
    let read_state = observed.clone();
    let write_state = observed.clone();
    let count = activations.clone();
    let app = Router::new().route("/api/lifecycle/updates/{id}", get(move |Path(id): Path<String>, headers: HeaderMap| {
        let status = read_state.clone();
        async move {
            assert_eq!(headers["authorization"],"Bearer test-admin");
            let mut status = status.lock().unwrap().clone(); status["operationId"] = json!(id);
            Json(status)
        }
    }).post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let status = write_state.clone(); let count = count.clone();
        async move {
            assert_eq!(headers["authorization"],"Bearer test-admin");
            assert_eq!(body["intentHash"],"exact-hash");
            count.fetch_add(1,Ordering::SeqCst);
            status.lock().unwrap()["phase"] = json!("draining");
            StatusCode::BAD_GATEWAY // Mutation committed but its response was lost.
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut binding = binding;
    binding.health_origin = format!("http://{}",listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener,app).await.unwrap(); });
    let mut config = (*state.config).clone();
    config.external_service_bindings = vec![binding.clone()];
    let state = AppState { config:Arc::new(config), ..state.clone() };
    let r = UpdateRequest { operation_id:"app-update-1".into(),intent_hash:"exact-hash".into() };
    let (status, _) = create(State(state.clone()),Path("ci".into()),HeaderMap::new(),
        Json(UpdateRequest { operation_id:r.operation_id.clone(),intent_hash:r.intent_hash.clone() })).await;
    assert_eq!(status,StatusCode::UNAUTHORIZED);
    observed.lock().unwrap()["applicationId"] = json!("another-app");
    assert!(accept(&binding,&r,"test-admin").await.is_err());
    observed.lock().unwrap()["applicationId"] = json!("ci");
    let receipt = accept(&binding,&r,"test-admin").await?;
    assert_eq!(activations.load(Ordering::SeqCst),0,"acceptance cannot replace the controller inline");
    assert_eq!(accept(&binding,&r,"test-admin").await?,receipt);
    let changed = UpdateRequest { operation_id:r.operation_id.clone(),intent_hash:"changed".into() };
    assert!(accept(&binding,&changed,"test-admin").await.is_err());
    let competing = UpdateRequest { operation_id:"app-update-2".into(),intent_hash:r.intent_hash.clone() };
    assert!(accept(&binding,&competing,"test-admin").await.is_err());
    assert!(reconcile(&state,"ci",&r.operation_id,&r.intent_hash).await.is_err());
    reconcile(&state.clone(),"ci",&r.operation_id,&r.intent_hash).await?;
    assert_eq!(activations.load(Ordering::SeqCst),1);
    let inventory = super::service_discovery::read_inventory(db::get_db()?,None).await?;
    assert_eq!(inventory["services"][0]["update"]["phase"],"draining");
    {
        let mut status = observed.lock().unwrap();
        status["phase"] = json!("complete"); status["status"] = json!("passed");
    }
    reconcile(&state,"ci",&r.operation_id,&r.intent_hash).await?;
    let row = db::get_db()?.query_one(Statement::from_string(DbBackend::Postgres,
        "SELECT status FROM application_updates WHERE operation_id='app-update-1'")).await?.unwrap();
    assert_eq!(row.try_get::<String>("","status")?,"passed");
    assert_eq!(activations.load(Ordering::SeqCst),1);
    server.abort();
    Ok(())
}
