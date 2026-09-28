//! One-time, operator-driven recovery. No expiry, automatic takeover, or ledger reset.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::{path::Path, time::Duration};
use uuid::Uuid;
use crate::{config::Config, executor::ExecutorOwner, lifecycle::DRAIN_LOCK, store::Store};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub operation_id: Uuid,
    pub source_boot: Uuid,
    pub source_generation: i64,
    pub deployment: String,
    pub source_sandbox: String,
    pub revision: String,
    pub maintenance_run: String,
    pub maintenance_operation: String,
    pub operator: String,
}

pub fn load(path: &Path) -> Result<Plan> {
    let p: Plan = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(!p.operation_id.is_nil() && !p.source_boot.is_nil() && p.source_generation > 0, "invalid recovery identity");
    for value in [&p.deployment, &p.source_sandbox, &p.maintenance_operation] {
        ensure!(!value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)), "invalid recovery target");
    }
    ensure!(p.source_sandbox.starts_with("sb-") && p.revision.len() == 40 && p.revision.bytes().all(|b| b.is_ascii_hexdigit()), "invalid runtime or revision");
    ensure!(!p.operator.is_empty() && p.operator.len() <= 256 && !p.maintenance_run.is_empty(), "operator and original run required");
    Ok(p)
}

pub fn identity(config: &Config) -> String {
    config.managed_deployment.clone().unwrap_or_else(|| match (&config.controller_deployment, &config.controller_app_lb_url) {
        (Some(id), Some(base)) => format!("{}/deployments/{id}", base.trim_end_matches('/')),
        _ => config.instance_id.clone(),
    })
}

/// Runs before imports, NATS, consumers, or infrastructure reconcilers. A held
/// process serves only its identity; HTTP 200 here means alive, not an executor.
pub async fn hold(config: &Config, plan: &Plan) -> Result<ExecutorOwner> {
    ensure!(config.managed_deployment.is_none() && config.controller_deployment.as_deref() == Some(plan.deployment.as_str())
        && config.expected_sha.as_deref() == Some(plan.revision.as_str()), "recovery plan does not name this artifact/deployment");
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    let store = Store::connect(&config.database_url, config.log_dir.clone(), config.db_statement_timeout).await?;
    store.migrate().await?;
    let executor = ExecutorOwner::register_recovery_candidate(store.pool().clone(), &identity(config), plan).await.map_err(anyhow::Error::msg)?;
    let boot = executor.boot_id();
    let body = json!({"mode":"recovery-only","operationId":plan.operation_id,"bootId":boot,"revision":plan.revision});
    let app = axum::Router::new().route("/healthz", axum::routing::get(move || {
        let body = body.clone(); async move { axum::Json(body) }
    }));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(async move {
        axum::serve(listener, app).with_graceful_shutdown(async { let _ = stopped.await; }).await
    });
    loop {
        if serving.is_finished() { anyhow::bail!("held recovery listener stopped"); }
        let transferred: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_executor_recovery r JOIN ci_executor_owner o ON o.boot_id=r.candidate_boot WHERE r.operation_id=$1 AND r.candidate_boot=$2 AND r.phase='reconciling' AND o.continuation_operation_id=$3)")
            .bind(plan.operation_id).bind(boot).bind(&plan.maintenance_operation).fetch_one(store.pool()).await?;
        if transferred { break; }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let _ = stop.send(());
    serving.await??;
    Ok(executor)
}

fn env(name: &str) -> Result<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty()).with_context(|| format!("{name} is required for operator recovery"))
}

/// Credentials are supplied through the operator's HeyoSecret-backed exec
/// environment, never stored in the plan or receipt. No redirects or retries.
async fn evidence(plan: &Plan, candidate: Uuid, sandbox: &str, etag: &str) -> Result<Value> {
    ensure!(sandbox != plan.source_sandbox && sandbox.starts_with("sb-") && sandbox.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)), "invalid candidate runtime");
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never()).timeout(Duration::from_secs(20)).build()?;
    let base = crate::cd::app_lb_endpoint(&env("CI_RECOVERY_APP_LB_URL")?).map_err(anyhow::Error::msg)?;
    let response = client.get(format!("{base}/deployments/{}", plan.deployment))
        .basic_auth(env("CI_RECOVERY_APP_LB_USER")?, Some(env("CI_RECOVERY_APP_LB_PASSWORD")?)).send().await?;
    ensure!(response.status().is_success() && response.headers().get(reqwest::header::ETAG).and_then(|v|v.to_str().ok()) == Some(etag), "replacement spec changed or cannot be durably read");
    let snapshot: Value = response.json().await?;
    let vms = snapshot["vms"].as_array().context("missing runtime inventory")?;
    ensure!(snapshot["spec"]["maintenance"] == true && snapshot["pending"] == 0 && vms.len() == 1
        && vms[0]["sandbox_id"] == sandbox && vms[0]["healthy"] == true && vms[0]["draining"] == false,
        "replacement must be the sole healthy runtime behind the maintenance gate");
    let ws = &snapshot["workspace"];
    ensure!(ws["phase"] == "idle" && ws["push_pending"] == false && ws["captured_from"] == plan.source_sandbox
        && ws["digest"].as_str().is_some_and(|s| !s.is_empty()) && ws["digest"] == ws["pushed"], "predecessor workspace capture is not durably stored");
    ensure!(snapshot["spec"]["vm"]["env_vars"]["CI_EXPECTED_SHA"] == plan.revision, "replacement revision changed");
    let port = snapshot["spec"]["vm"]["port"].as_u64().filter(|p| *p > 0 && *p <= 65535).context("candidate port missing")?;
    let response = client.post(format!("{base}/deployments/{}/exec", plan.deployment))
        .basic_auth(env("CI_RECOVERY_APP_LB_USER")?, Some(env("CI_RECOVERY_APP_LB_PASSWORD")?))
        .json(&json!({"wake":false,"sandbox_id":sandbox,"command":format!("curl -fsS --max-time 5 http://127.0.0.1:{port}/healthz"),"timeout_secs":10})).send().await?;
    ensure!(response.status().is_success(), "exact candidate identity unavailable");
    let result: Value = response.json().await?;
    ensure!(result["sandbox_id"] == sandbox && result["exit_code"] == 0, "candidate identity execution failed");
    let health: Value = serde_json::from_str(result["stdout"].as_str().context("candidate health missing")?)?;
    ensure!(health["mode"] == "recovery-only" && health["bootId"] == candidate.to_string()
        && health["operationId"] == plan.operation_id.to_string() && health["revision"] == plan.revision, "candidate boot identity differs");
    let backend = crate::cd::app_lb_endpoint(&env("CI_RECOVERY_BACKEND_URL")?).map_err(anyhow::Error::msg)?;
    let response = client.get(format!("{backend}/sandboxes/{}/firecracker-reclamation", plan.source_sandbox))
        .bearer_auth(env("CI_RECOVERY_BACKEND_TOKEN")?).send().await?;
    ensure!(response.status() == reqwest::StatusCode::OK, "predecessor reclamation is unverified");
    let reclaimed: Value = response.json().await?;
    ensure!(reclaimed["protocol"] == "firecracker-reclamation-v1" && reclaimed["sandbox_id"] == plan.source_sandbox
        && reclaimed["reclaimed"] == true, "predecessor still has runtime resources");
    // Do not persist the full deployment spec: it may contain credentials.
    Ok(json!({"specEtag":etag,"candidateSandbox":sandbox,"workspace":ws,"reclamation":reclaimed}))
}

pub async fn transfer(store: &Store, plan: &Plan, candidate: Uuid, sandbox: &str, etag: &str) -> Result<()> {
    let saved: Option<Value> = sqlx::query_scalar("SELECT evidence FROM ci_executor_recovery WHERE operation_id=$1 AND candidate_boot=$2 AND phase='reconciling'")
        .bind(plan.operation_id).bind(candidate).fetch_optional(store.pool()).await?.flatten();
    let proof = if let Some(proof) = saved {
        ensure!(proof["candidateSandbox"] == sandbox && proof["specEtag"] == etag, "recovery replay target differs");
        proof
    } else { evidence(plan, candidate, sandbox, etag).await? };
    transfer_verified(store, plan, candidate, &proof).await
}

async fn transfer_verified(store: &Store, plan: &Plan, candidate: Uuid, proof: &Value) -> Result<()> {
    let mut tx = store.pool().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(DRAIN_LOCK).execute(&mut *tx).await?;
    let owner = sqlx::query("SELECT boot_id,generation,continuation_operation_id FROM ci_executor_owner WHERE singleton=TRUE FOR UPDATE")
        .fetch_one(&mut *tx).await?;
    let record = sqlx::query("SELECT plan,candidate_boot,phase,evidence FROM ci_executor_recovery WHERE operation_id=$1 FOR UPDATE")
        .bind(plan.operation_id).fetch_one(&mut *tx).await?;
    ensure!(record.get::<Value,_>("plan") == serde_json::to_value(plan)? && record.get::<Uuid,_>("candidate_boot") == candidate, "recovery intent changed");
    if record.get::<String,_>("phase") == "reconciling" {
        ensure!(owner.get::<Uuid,_>("boot_id") == candidate && owner.get::<i64,_>("generation") == plan.source_generation + 1
            && record.get::<Option<Value>,_>("evidence").as_ref() == Some(proof), "recovery replay differs");
        return Ok(());
    }
    ensure!(record.get::<String,_>("phase") == "holding" && owner.get::<Uuid,_>("boot_id") == plan.source_boot
        && owner.get::<i64,_>("generation") == plan.source_generation && owner.get::<Option<String>,_>("continuation_operation_id").is_none(), "predecessor ownership changed or already has a continuation");
    let intended: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_host_maintenance h JOIN ci_service_deployment s ON s.id=h.id WHERE h.id=$1 AND s.run_id=$2 AND h.phase IN ('failed','passed'))")
        .bind(&plan.maintenance_operation).bind(&plan.maintenance_run).fetch_one(&mut *tx).await?;
    ensure!(intended, "expected maintenance obligation is missing");
    sqlx::query("UPDATE ci_executor_owner SET boot_id=$1,generation=generation+1,continuation_operation_id=$2,transferred_at=now() WHERE singleton=TRUE")
        .bind(candidate).bind(&plan.maintenance_operation).execute(&mut *tx).await?;
    sqlx::query("UPDATE ci_executor_boot SET retired=TRUE WHERE boot_id=$1").bind(plan.source_boot).execute(&mut *tx).await?;
    sqlx::query("UPDATE ci_executor_recovery SET phase='reconciling',evidence=$2,updated_at=now() WHERE operation_id=$1")
        .bind(plan.operation_id).bind(proof).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn activate(d: &crate::dispatch::Dispatcher, id: Uuid) -> Result<()> {
    let _fence = d.executor.handoff_fence().await.map_err(anyhow::Error::msg)?;
    let _work = d.lifecycle.maintenance_fence().map_err(anyhow::Error::msg)?;
    activate_state(&d.store, d.executor.boot_id(), id).await
}

async fn activate_state(store: &Store, boot: Uuid, id: Uuid) -> Result<()> {
    let mut tx = store.pool().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(DRAIN_LOCK).execute(&mut *tx).await?;
    let owner = sqlx::query("SELECT boot_id,generation,continuation_operation_id FROM ci_executor_owner WHERE singleton=TRUE FOR UPDATE")
        .fetch_one(&mut *tx).await?;
    let record = sqlx::query("SELECT plan,candidate_boot,phase FROM ci_executor_recovery WHERE operation_id=$1 FOR UPDATE")
        .bind(id).fetch_one(&mut *tx).await?;
    let plan: Plan = serde_json::from_value(record.get("plan"))?;
    ensure!(record.get::<Uuid,_>("candidate_boot") == boot && owner.get::<Uuid,_>("boot_id") == boot
        && owner.get::<i64,_>("generation") == plan.source_generation + 1, "activation must run on the pinned replacement owner");
    if record.get::<String,_>("phase") == "complete" {
        ensure!(owner.get::<Option<String>,_>("continuation_operation_id").is_none(), "execution restriction changed");
        return Ok(());
    }
    ensure!(record.get::<String,_>("phase") == "reconciling" && owner.get::<Option<String>,_>("continuation_operation_id").as_deref() == Some(plan.maintenance_operation.as_str()), "recovery restriction changed");
    let settled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_host_maintenance h JOIN ci_service_deployment s ON s.id=h.id WHERE h.id=$1 AND h.phase='passed' AND s.status='passed')")
        .bind(&plan.maintenance_operation).fetch_one(&mut *tx).await?;
    ensure!(settled, "named maintenance obligation remains unresolved");
    let mut remaining = crate::maintenance::blockers(&mut tx).await.map_err(anyhow::Error::msg)?;
    // Explicitly quarantined native hosts cannot receive grants. Their retained
    // work is not Linux executor work, and is not falsely reported as stopped.
    let unisolated: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_host_work w WHERE NOT EXISTS(SELECT 1 FROM ci_native_job n JOIN ci_job j ON j.id=n.job_id JOIN ci_native_quarantine q ON q.runner_id=n.runner_id WHERE n.job_id=w.job_id AND n.runner_id=w.runner_hd_id AND j.attempt=w.attempt AND j.status='cancelled' AND n.state='cancelled' AND n.lease_token IS NULL))")
        .fetch_one(&mut *tx).await?;
    if !unisolated { remaining.retain(|category| category != "host_work"); }
    ensure!(remaining.is_empty(), "unresolved obligations: {}", remaining.join(", "));
    sqlx::query("UPDATE ci_executor_owner SET continuation_operation_id=NULL WHERE singleton=TRUE").execute(&mut *tx).await?;
    sqlx::query("UPDATE ci_executor_recovery SET phase='complete',updated_at=now() WHERE operation_id=$1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn recovery_cas_is_one_shot_restricted_and_preserves_failed_run() {
        let base = std::env::var("CI_TEST_DATABASE_URL").unwrap();
        let admin = sqlx::PgPool::connect(&base).await.unwrap();
        let schema = format!("recovery_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();
        let mut url = reqwest::Url::parse(&base).unwrap();
        url.query_pairs_mut().append_pair("options", &format!("-c search_path={schema}"));
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(url.as_str(), dir.path().join("logs"), Duration::from_secs(10)).await.unwrap();
        store.migrate().await.unwrap();
        // Held boot and normal startup each migrate the same database.
        store.migrate().await.unwrap();
        let old = ExecutorOwner::register(store.pool().clone(), "ci-eu1").await.unwrap();
        let plan = Plan { operation_id: Uuid::new_v4(), source_boot: old.boot_id(), source_generation: 1,
            deployment: "ci-eu1".into(), source_sandbox: "sb-old".into(), revision: "a".repeat(40),
            maintenance_run: "run".into(), maintenance_operation: "maintenance".into(), operator: "admin".into() };
        sqlx::raw_sql("INSERT INTO ci_run(id,workflow_id,workflow_path,status) VALUES('run','tests','ci.yml','failure');
            INSERT INTO ci_job(id,run_id,job_key,base_id,display,status) VALUES('job','run','job','job','Job','failure');
            INSERT INTO ci_step(id,job_id,idx,name,uses,status) VALUES('step','job',0,'Maintain','ci/host-maintenance','failure');
            INSERT INTO ci_service_deployment(id,step_id,run_id,job_id,service_id,request_hash,status,sha,git_ref) VALUES('maintenance','step','run','job','host','hash','failed','source','refs/heads/main');
            INSERT INTO ci_host_maintenance(id,runner_hd_id,request,phase,deadline) VALUES('maintenance','host','{}','failed',now());")
            .execute(store.pool()).await.unwrap();
        let candidate = ExecutorOwner::register_recovery_candidate(store.pool().clone(), "ci-eu1", &plan).await.unwrap();
        assert!(old.is_owner().await.unwrap());
        assert!(candidate.effect_permit().await.is_err());
        assert!(ExecutorOwner::register_recovery_candidate(store.pool().clone(), "ci-eu1", &plan).await.is_err());
        assert!(crate::lifecycle::Lifecycle::default().admission(&store).await.is_err());
        let proof = json!({"candidateSandbox":"sb-new","specEtag":"exact","reclamation":{"reclaimed":true}});
        let mut wrong = plan.clone(); wrong.source_generation = 2;
        assert!(transfer_verified(&store, &wrong, candidate.boot_id(), &proof).await.is_err());
        assert!(transfer_verified(&store, &plan, Uuid::new_v4(), &proof).await.is_err());
        transfer_verified(&store, &plan, candidate.boot_id(), &proof).await.unwrap();
        transfer_verified(&store, &plan, candidate.boot_id(), &proof).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT generation FROM ci_executor_owner").fetch_one(store.pool()).await.unwrap(), 2);
        assert!(old.effect_permit().await.is_err());
        assert!(candidate.effect_permit().await.is_err());
        assert!(candidate.effect_permit_for(Some("other")).await.is_err());
        assert!(candidate.effect_permit_for(Some("maintenance")).await.is_ok());
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT status FROM ci_run WHERE id='run'").fetch_one(store.pool()).await.unwrap(), "failure");
        assert!(activate_state(&store, candidate.boot_id(), plan.operation_id).await.is_err());
        // Model positive receipt reconciliation; unrelated host work still blocks.
        sqlx::raw_sql("UPDATE ci_host_maintenance SET phase='passed'; INSERT INTO ci_host_work(job_id,runner_hd_id,attempt) VALUES('job','host',1);")
            .execute(store.pool()).await.unwrap();
        assert!(activate_state(&store, candidate.boot_id(), plan.operation_id).await.is_err());
        sqlx::query("UPDATE ci_service_deployment SET status='passed'").execute(store.pool()).await.unwrap();
        assert!(activate_state(&store, candidate.boot_id(), plan.operation_id).await.is_err());
        store.end_host_work("job", "host", 1).await.unwrap();
        // An isolated native runner remains recorded, but cannot block Linux
        // recovery once the job and its credential are explicitly revoked.
        sqlx::raw_sql("INSERT INTO ci_native_runner(id,name,labels,platform,arch) VALUES('windows','Windows','{}','windows','x86_64');
            INSERT INTO ci_job(id,run_id,job_key,base_id,display,status) VALUES('native','run','native','native','Native','cancelled');
            INSERT INTO ci_native_job(job_id,run_id,required_labels,state,runner_id) VALUES('native','run','{}','cancelled','windows');
            INSERT INTO ci_host_work(job_id,runner_hd_id,attempt) VALUES('native','windows',1);")
            .execute(store.pool()).await.unwrap();
        assert!(activate_state(&store, candidate.boot_id(), plan.operation_id).await.is_err());
        sqlx::query("INSERT INTO ci_native_quarantine(runner_id,report_uri,requested_by) VALUES('windows','s3://reports/retired.json','admin')")
            .execute(store.pool()).await.unwrap();
        assert!(activate_state(&store, old.boot_id(), plan.operation_id).await.is_err());
        activate_state(&store, candidate.boot_id(), plan.operation_id).await.unwrap();
        activate_state(&store, candidate.boot_id(), plan.operation_id).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM ci_host_work WHERE job_id='native'").fetch_one(store.pool()).await.unwrap(), 1);
        assert!(crate::native::poll(&store, crate::native::Poll { runner_id: "windows".into(), protocol_version: 1 }, "http://localhost", &crate::secrets::Secrets::unconfigured()).await.is_err());
        assert!(candidate.effect_permit().await.is_ok());
        assert!(old.effect_permit().await.is_err());
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT status FROM ci_run WHERE id='run'").fetch_one(store.pool()).await.unwrap(), "failure");
        assert!(ExecutorOwner::register(store.pool().clone(), "ci-eu1").await.is_err(), "a crashed candidate cannot silently re-use its receipt");
        store.pool().close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE")).execute(&admin).await.unwrap();
        admin.close().await;
    }
}
