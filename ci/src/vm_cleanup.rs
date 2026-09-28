//! Durable post-execution cleanup. Terminal job status alone never authorizes it.
use crate::{dispatch::Dispatcher, store::{JobStatus, Store}, vm::VmError};
use anyhow::{Result, ensure};
use heyo_sdk::{HeyoError, SandboxStatus};
use sqlx::Row;
use std::time::Duration;

/// Called only after this executor has stopped issuing commands to its owned VM.
/// Publish the job result and cleanup obligation together, before attempting IO.
pub async fn handoff(d: &Dispatcher, job: &str, runner: &str, attempt: i32,
    sandbox: &str, status: JobStatus, error: Option<&str>) -> Result<()> {
    ensure!(status.is_terminal(), "cleanup requires a terminal executor outcome");
    let mut tx = d.store.pool().begin().await?;
    let j = sqlx::query("SELECT run_id,job_key,attempt,status FROM ci_job WHERE id=$1 FOR UPDATE")
        .bind(job).fetch_one(&mut *tx).await?;
    ensure!(j.get::<i32,_>("attempt") == attempt, "cleanup attempt changed");
    let p = sqlx::query("SELECT status,claimed_by_job,runner_hd_id,leased_by FROM ci_vm_pool WHERE sandbox_id=$1 FOR UPDATE")
        .bind(sandbox).fetch_one(&mut *tx).await?;
    ensure!(p.get::<String,_>("status") == "claimed"
        && p.get::<Option<String>,_>("claimed_by_job").as_deref() == Some(job)
        && p.get::<String,_>("runner_hd_id") == runner
        && p.get::<Option<String>,_>("leased_by").as_deref() == Some(&d.config.instance_id), "cleanup ownership changed");
    let work: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_host_work WHERE job_id=$1 AND runner_hd_id=$2 AND attempt=$3)")
        .bind(job).bind(runner).bind(attempt).fetch_one(&mut *tx).await?;
    ensure!(work, "cleanup has no executor ownership evidence");
    sqlx::query("INSERT INTO ci_vm_cleanup(sandbox_id,job_id,runner_hd_id,attempt,destroy) VALUES($1,$2,$3,$4,$5)")
        .bind(sandbox).bind(job).bind(runner).bind(attempt).bind(true).execute(&mut *tx).await?;
    // Change the pool tuple too: an orphan UPDATE already waiting on this row
    // must recheck a non-expiring lease, even if its snapshot predates the intent.
    // Cleanup now owns release; process heartbeats no longer own this lease.
    sqlx::query("UPDATE ci_vm_pool SET leased_until='infinity'::timestamptz WHERE sandbox_id=$1")
        .bind(sandbox).execute(&mut *tx).await?;
    // Acquisition may finish after cancellation or another terminal outcome.
    // Cleanup must preserve that outcome, not overwrite it.
    let existing = j.get::<String,_>("status");
    let final_status = if JobStatus::parse(&existing).is_some_and(|s| s.is_terminal()) { existing.as_str() } else { status.as_str() };
    sqlx::query("UPDATE ci_job SET status=$2,error=COALESCE($3,error),finished_at=now() WHERE id=$1")
        .bind(job).bind(final_status).bind(error).execute(&mut *tx).await?;
    Store::add_event(&mut tx, &j.get::<String,_>("run_id"), Some(job), Some(&j.get::<String,_>("job_key")),
        None, "ci.job.status.v1", final_status, error).await?;
    crate::debug_report::enqueue(&mut tx, job, sandbox).await?;
    tx.commit().await?;
    Ok(())
}

pub fn spawn(d: std::sync::Arc<Dispatcher>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Ok(_effect) = d.executor.effect_permit().await else { continue };
            let Ok(_work) = d.lifecycle.work(&d.store).await else { continue };
            if let Err(e) = reconcile(&d).await {
                tracing::warn!("could not reconcile VM cleanup: {e}");
            }
        }
    });
}

/// Run during ordinary operation and drain. Independent of the originating
/// process lifetime; the durable handoff is the authority after restart.
pub async fn reconcile(d: &Dispatcher) -> Result<()> {
    let runners: Vec<String> = d.runners.snapshot().all_runners().map(|r| r.id.clone()).collect();
    // Bound pool-lock contention per pass so lease renewal is not held behind
    // a whole batch of unreachable daemons. Retry timestamps keep this fair.
    let pending = sqlx::query("SELECT sandbox_id,job_id,runner_hd_id,attempt FROM ci_vm_cleanup WHERE runner_hd_id=ANY($1) AND next_attempt_at<=now() ORDER BY next_attempt_at LIMIT 1")
        .bind(&runners).fetch_all(d.store.pool()).await?;
    for row in pending {
        let sandbox: String = row.get("sandbox_id");
        let result = tokio::time::timeout(Duration::from_secs(20), finish(d, &sandbox)).await;
        let error = match result { Ok(Ok(())) => continue, Ok(Err(e)) => e.to_string(), Err(_) => "cleanup verification timed out".into() };
        let runner: String = row.get("runner_hd_id");
        // Existing operations own their tunnel independently (PR77).
        d.runners.evict(&runner).await;
        sqlx::query("UPDATE ci_vm_cleanup SET last_error=$4,next_attempt_at=now()+interval '30 seconds' WHERE sandbox_id=$1 AND job_id=$2 AND attempt=$3")
            .bind(&sandbox).bind(row.get::<String,_>("job_id")).bind(row.get::<i32,_>("attempt")).bind(&error)
            .execute(d.store.pool()).await?;
        tracing::warn!(%sandbox, %error, "VM cleanup unresolved; retaining claim and retrying");
    }
    Ok(())
}

async fn finish(d: &Dispatcher, sandbox: &str) -> Result<()> {
    let mut tx = d.store.pool().begin().await?;
    let Some(c) = sqlx::query("SELECT * FROM ci_vm_cleanup WHERE sandbox_id=$1 FOR UPDATE SKIP LOCKED")
        .bind(sandbox).fetch_optional(&mut *tx).await? else { return Ok(()) };
    let job: String = c.get("job_id"); let runner: String = c.get("runner_hd_id"); let attempt: i32 = c.get("attempt");
    let p = sqlx::query("SELECT status,claimed_by_job,runner_hd_id FROM ci_vm_pool WHERE sandbox_id=$1 FOR UPDATE")
        .bind(sandbox).fetch_one(&mut *tx).await?;
    ensure!(p.get::<String,_>("status") == "claimed"
        && p.get::<Option<String>,_>("claimed_by_job").as_deref() == Some(job.as_str())
        && p.get::<String,_>("runner_hd_id") == runner, "cleanup pool ownership changed");
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_job WHERE id=$1 AND attempt=$2 AND status IN ('success','failure','skipped','cancelled')) AND NOT EXISTS(SELECT 1 FROM ci_service_deployment s JOIN ci_host_maintenance h ON h.id=s.id WHERE s.job_id=$1)")
        .bind(&job).bind(attempt).fetch_one(&mut *tx).await?;
    ensure!(valid, "cleanup job identity or lifecycle owner changed");
    // Covers cleanup intents created by older controllers that requested a
    // stopped warm cache. CI-owned job VMs are now always ephemeral.
    crate::debug_report::enqueue(&mut tx, &job, sandbox).await?;
    let vm = d.vms.open(d.runners.options_for(&runner).await?, sandbox.to_string()).await?;
    let already_removed = match vm.info().await {
        Err(VmError::Daemon { source: HeyoError::NotFound(_), .. }) => true,
        result => {
            let info = result?;
            ensure!(info.id == sandbox, "cleanup daemon returned another VM");
            if info.status != SandboxStatus::Stopped { vm.stop().await?; }
            let stopped = vm.info().await?;
            ensure!(stopped.id == sandbox && stopped.status == SandboxStatus::Stopped, "daemon has not confirmed the exact VM stopped");
            false
        }
    };
    if !already_removed {
        vm.destroy().await?;
        ensure!(matches!(vm.info().await, Err(VmError::Daemon { source: HeyoError::NotFound(_), .. })), "daemon has not confirmed VM removal");
    }
    sqlx::query("DELETE FROM ci_vm_pool WHERE sandbox_id=$1").bind(sandbox).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM ci_host_work WHERE job_id=$1 AND runner_hd_id=$2 AND attempt=$3")
        .bind(&job).bind(&runner).bind(attempt).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM ci_vm_cleanup WHERE sandbox_id=$1").bind(sandbox).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
