//! Reversible operator maintenance. Quiescence does not transfer executor ownership.
use crate::{executor::ExecutorOwner, lifecycle::DRAIN_LOCK, store::Store};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

// Do not infer teardown from failure, lease expiry, or an empty queue.
pub(crate) async fn blockers(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<String>, String> {
    sqlx::query_scalar(
        "SELECT name FROM (VALUES
        ('jobs', EXISTS(SELECT 1 FROM ci_job j JOIN ci_run r ON r.id=j.run_id WHERE j.status='running' OR (j.status IN ('pending','queued') AND r.status NOT IN ('success','failure','cancelled')))),
        ('native_leases', EXISTS(SELECT 1 FROM ci_native_job WHERE state='leased')),
        ('host_work', EXISTS(SELECT 1 FROM ci_host_work)),
        ('vm_cleanup', EXISTS(SELECT 1 FROM ci_vm_cleanup)),
        ('vm_operations', EXISTS(SELECT 1 FROM ci_vm_pool WHERE status IN ('claimed','building','draining'))),
        ('deployments', EXISTS(SELECT 1 FROM ci_service_deployment WHERE status NOT IN ('passed','failed'))),
        ('host_maintenance', EXISTS(SELECT 1 FROM ci_host_maintenance WHERE phase<>'passed')),
        ('host_bootstrap', EXISTS(SELECT 1 FROM ci_host_heyvm_bootstrap WHERE phase NOT IN ('passed','superseded'))),
        ('unsettled_deployments', EXISTS(SELECT 1 FROM ci_service_deployment s WHERE s.status<>'passed' AND ((EXISTS(SELECT 1 FROM ci_service_rollout r WHERE r.id=s.id) AND (s.status<>'failed' OR s.phase IS DISTINCT FROM 'settled_failure')) OR EXISTS(SELECT 1 FROM ci_host_app_lb h WHERE h.id=s.id)))),
        ('ci_rollout', EXISTS(SELECT 1 FROM ci_controller_rollout WHERE phase<>'complete')),
        ('ci_retirement', EXISTS(SELECT 1 FROM ci_application_retirement WHERE phase<>'safe'))
        ) AS obligations(name, blocked) WHERE blocked ORDER BY name"
    ).fetch_all(&mut **tx).await.map_err(|e| e.to_string())
}

async fn locked(store: &Store) -> Result<Transaction<'_, Postgres>, String> {
    let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(DRAIN_LOCK)
        .execute(&mut *tx).await.map_err(|e| e.to_string())?;
    Ok(tx)
}

pub async fn pause(store: &Store, id: Uuid, actor: &str) -> Result<(), String> {
    let mut tx = locked(store).await?;
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT phase FROM ci_operator_maintenance WHERE operation_id=$1"
    ).bind(id).fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
    if let Some(phase) = previous {
        return if phase == "running" { Err("maintenance operation already resumed; use a new ID".into()) } else { Ok(()) };
    }
    let active: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_operator_maintenance WHERE phase<>'running')")
        .fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
    if active { return Err("another maintenance operation is active".into()); }
    sqlx::query("INSERT INTO ci_operator_maintenance(operation_id,phase,requested_by) VALUES($1,'draining',$2)")
        .bind(id).bind(actor).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

pub async fn resume(store: &Store, id: Uuid) -> Result<(), String> {
    let mut tx = locked(store).await?;
    let changed = sqlx::query("UPDATE ci_operator_maintenance SET phase='running',updated_at=now() WHERE operation_id=$1")
        .bind(id).execute(&mut *tx).await.map_err(|e| e.to_string())?.rows_affected();
    if changed != 1 { return Err("maintenance operation does not match".into()); }
    tx.commit().await.map_err(|e| e.to_string())
}

/// Explicit transition, not a side effect of GET. The caller must not hold an
/// effect permit: the exclusive fence waits for every local effect to finish.
pub async fn quiesce(store: &Store, executor: &ExecutorOwner, lifecycle: &crate::lifecycle::Lifecycle, id: Uuid) -> Result<(), String> {
    let _fence = executor.handoff_fence().await?;
    let _work = lifecycle.maintenance_fence()?;
    let mut tx = locked(store).await?;
    let owner: Uuid = sqlx::query_scalar("SELECT boot_id FROM ci_executor_owner WHERE singleton=TRUE FOR UPDATE")
        .fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
    if owner != executor.boot_id() { return Err("quiescence must be requested from the execution owner".into()); }
    let phase: Option<String> = sqlx::query_scalar("SELECT phase FROM ci_operator_maintenance WHERE operation_id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
    if !matches!(phase.as_deref(), Some("draining" | "paused")) {
        return Err("maintenance operation is not active".into());
    }
    let remaining = blockers(&mut tx).await?;
    if !remaining.is_empty() { return Err(format!("maintenance is still draining: {}", remaining.join(", "))); }
    sqlx::query("UPDATE ci_operator_maintenance SET phase='paused',updated_at=now() WHERE operation_id=$1")
        .bind(id).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

pub async fn status(store: &Store) -> Result<Value, String> {
    let mut tx = locked(store).await?;
    let current: Option<(Uuid, String)> = sqlx::query_as("SELECT operation_id,phase FROM ci_operator_maintenance ORDER BY (phase<>'running') DESC,created_at DESC,operation_id LIMIT 1")
        .fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
    let remaining = blockers(&mut tx).await?;
    let (id, phase) = current.map(|(id, phase)| (Some(id), phase)).unwrap_or((None, "running".into()));
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(json!({"operationId":id,"phase":phase,"admissionClosed":phase != "running",
        "quiesced":phase == "paused" && remaining.is_empty(),"blockers":remaining,
        "safeToReplace":false,"replacementBlockers":["executor ownership requires a separate verified handoff; maintenance pause does not authorize replacement"]}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::Lifecycle;
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn operator_pause_fences_admission_effects_and_stale_resume() {
        let base = std::env::var("CI_TEST_DATABASE_URL").expect("disposable CI_TEST_DATABASE_URL");
        let admin = sqlx::PgPool::connect(&base).await.unwrap();
        let schema = format!("maintenance_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();
        let mut url = reqwest::Url::parse(&base).unwrap();
        url.query_pairs_mut().append_pair("options", &format!("-c search_path={schema}"));
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(url.as_str(), dir.path().join("logs"), Duration::from_secs(10)).await.unwrap();
        store.migrate().await.unwrap();
        let owner = ExecutorOwner::register(store.pool().clone(), "ci-eu").await.unwrap();
        let standby = ExecutorOwner::register(store.pool().clone(), "ci-us").await.unwrap();
        let lifecycle = Lifecycle::default();
        let id = Uuid::new_v4();
        let mut admitted = store.pool().begin().await.unwrap();
        Lifecycle::admit_in(&mut admitted).await.unwrap();
        let closing = pause(&store, id, "operator");
        tokio::pin!(closing);
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut closing).await.is_err());
        admitted.commit().await.unwrap();
        closing.await.unwrap();
        pause(&store, id, "operator").await.unwrap();
        assert!(pause(&store, Uuid::new_v4(), "operator").await.is_err());
        assert!(Lifecycle::default().admission(&store).await.is_err());
        let mut late = store.pool().begin().await.unwrap();
        assert!(Lifecycle::admit_in(&mut late).await.is_err());
        Lifecycle::grant_in(&mut late).await.unwrap();
        late.rollback().await.unwrap();
        assert!(quiesce(&store, &standby, &lifecycle, id).await.is_err());
        assert_eq!(status(&store).await.unwrap()["phase"], "draining");

        // Terminal parent/job status is not proof of remote completion.
        sqlx::raw_sql("INSERT INTO ci_run(id,workflow_id,workflow_path,status) VALUES('run','tests','ci.yml','failure');
            INSERT INTO ci_job(id,run_id,job_key,base_id,display,status) VALUES('job','run','job','job','Job','failure');
            INSERT INTO ci_host_work(job_id,runner_hd_id,attempt) VALUES('job','host',1);")
            .execute(store.pool()).await.unwrap();
        assert!(quiesce(&store, &owner, &lifecycle, id).await.unwrap_err().contains("host_work"));
        assert_eq!(status(&store).await.unwrap()["blockers"], json!(["host_work"]));
        // Simulate positively completed remote work, not an expiry timer.
        store.end_host_work("job", "host", 1).await.unwrap();
        sqlx::raw_sql("INSERT INTO ci_step(id,job_id,idx,name,uses,status) VALUES('step','job',0,'Maintain','ci/host-maintenance','failure');
            INSERT INTO ci_service_deployment(id,step_id,run_id,job_id,service_id,request_hash,status,sha,git_ref) VALUES('op','step','run','job','host','hash','failed','source','refs/heads/main');
            INSERT INTO ci_host_maintenance(id,runner_hd_id,request,phase,deadline) VALUES('op','host','{}','failed',now()-interval '1 hour');")
            .execute(store.pool()).await.unwrap();
        assert!(quiesce(&store, &owner, &lifecycle, id).await.unwrap_err().contains("host_maintenance"));
        resume(&store, id).await.unwrap();
        assert_eq!(status(&store).await.unwrap()["blockers"], json!(["host_maintenance"]), "resume must not clear retained maintenance");
        let id = Uuid::new_v4();
        pause(&store, id, "operator").await.unwrap();
        // Model the separate recovery protocol positively settling the obligation.
        sqlx::query("UPDATE ci_host_maintenance SET phase='passed' WHERE id='op'")
            .execute(store.pool()).await.unwrap();
        let work = lifecycle.work(&store).await.unwrap();
        assert!(quiesce(&store, &owner, &lifecycle, id).await.unwrap_err().contains("in flight"));
        drop(work);
        let effect = owner.effect_permit().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), quiesce(&store, &owner, &lifecycle, id)).await.is_err());
        assert_eq!(status(&store).await.unwrap()["phase"], "draining");
        drop(effect);
        let mut grant_in_flight = store.pool().begin().await.unwrap();
        Lifecycle::grant_in(&mut grant_in_flight).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), quiesce(&store, &owner, &lifecycle, id)).await.is_err());
        grant_in_flight.commit().await.unwrap();
        quiesce(&store, &owner, &lifecycle, id).await.unwrap();
        let snapshot = status(&store).await.unwrap();
        assert_eq!(snapshot["quiesced"], true);
        assert_eq!(snapshot["safeToReplace"], false);
        assert!(owner.is_owner().await.unwrap());
        assert!(owner.effect_permit().await.is_err());
        assert!(Lifecycle::default().work(&store).await.is_err());
        let mut grant = store.pool().begin().await.unwrap();
        assert!(Lifecycle::grant_in(&mut grant).await.is_err());
        grant.rollback().await.unwrap();
        assert!(ExecutorOwner::register(store.pool().clone(), "ci-eu").await.is_err());
        resume(&store, id).await.unwrap();
        assert!(owner.effect_permit().await.is_ok());
        assert!(Lifecycle::default().admission(&store).await.is_ok());
        let next = Uuid::new_v4();
        pause(&store, next, "operator").await.unwrap();
        resume(&store, id).await.unwrap();
        assert_eq!(status(&store).await.unwrap()["operationId"], next.to_string());
        assert!(Lifecycle::default().admission(&store).await.is_err());
        resume(&store, next).await.unwrap();
        assert!(pause(&store, id, "operator").await.is_err(), "old pause cannot reopen a completed operation");
        store.pool().close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE")).execute(&admin).await.unwrap();
        admin.close().await;
    }
}
