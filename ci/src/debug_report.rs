//! Durable diagnostic snapshots, independent of the lifetime of a job VM.
use crate::{artifacts::{ArtifactRef, ArtifactSink, S3Sink}, dispatch::Dispatcher};
use anyhow::{Context, Result};
use sqlx::{Postgres, Row, Transaction};
use std::{sync::Arc, time::Duration};

/// Call in the same transaction that hands the VM to cleanup. Persist every
/// available step log before deleting the only machine that could reproduce it.
/// Deliberately exclude environment, secrets, commands and workspace contents.
pub async fn enqueue(tx: &mut Transaction<'_, Postgres>, job: &str, sandbox: &str) -> Result<()> {
    sqlx::query(r#"
        INSERT INTO ci_debug_report(job_id,attempt,sandbox_id,run_id,job_key,payload)
        SELECT j.id,j.attempt,$2,j.run_id,j.job_key,jsonb_build_object(
            'schema','ci-debug-report-v1','captured_at',now(),
            'run',jsonb_build_object('id',r.id,'sha',r.sha,'git_ref',r.git_ref,
                'workflow',r.workflow_path),
            'job',jsonb_build_object('id',j.id,'key',j.job_key,'attempt',j.attempt,
                'status',j.status,'error',j.error,'runner',j.runner_hd_id,
                'sandbox',$2::text,'started_at',j.started_at,'finished_at',j.finished_at),
            'steps',COALESCE((SELECT jsonb_agg(jsonb_build_object(
                'id',s.id,'name',s.name,'status',s.status,'exit_code',s.exit_code,
                'operation_id',s.operation_id,'error',s.error,
                'started_at',s.started_at,'finished_at',s.finished_at,
                'log',COALESCE((SELECT string_agg(convert_from(l.bytes,'UTF8'),'' ORDER BY l.byte_offset)
                    FROM ci_step_log l WHERE l.step_id=s.id),''),
                'log_bytes',s.log_bytes) ORDER BY s.idx)
                FROM ci_step s WHERE s.job_id=j.id),'[]'::jsonb),
            'capture_note','Includes retained step/build/checkout logs and captured VM console. Missing or unreachable diagnostics are recorded in those logs; environments and workspace files are excluded.')
        FROM ci_job j JOIN ci_run r ON r.id=j.run_id WHERE j.id=$1
        ON CONFLICT(job_id,attempt,sandbox_id) DO NOTHING
    "#).bind(job).bind(sandbox).execute(&mut **tx).await?;
    Ok(())
}

pub fn spawn(d: Arc<Dispatcher>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = reconcile(&d).await {
                tracing::warn!(%error, "debug report upload pending; VM cleanup is independent");
            }
        }
    });
}

pub async fn reconcile(d: &Dispatcher) -> Result<()> {
    // Also cover failures before VM acquisition, and native jobs. No ownership
    // inference authorizes VM deletion here: this worker only archives reports.
    let jobs: Vec<(String, String)> = sqlx::query_as("SELECT j.id,COALESCE(j.sandbox_id,'') FROM ci_job j WHERE j.status IN ('success','failure','cancelled','skipped') AND NOT EXISTS(SELECT 1 FROM ci_host_work w WHERE w.job_id=j.id) AND NOT EXISTS(SELECT 1 FROM ci_debug_report r WHERE r.job_id=j.id AND r.attempt=j.attempt) ORDER BY j.finished_at LIMIT 10")
        .fetch_all(d.store.pool()).await?;
    for (job, sandbox) in jobs {
        let mut tx = d.store.pool().begin().await?;
        enqueue(&mut tx, &job, &sandbox).await?;
        tx.commit().await?;
    }
    let config = d.config.s3.clone().context("CI_S3_BUCKET is required for private CI debug reports")?;
    upload_one(&d.store, &S3Sink::new(config)?).await
}

async fn upload_one(store: &crate::store::Store, sink: &dyn ArtifactSink) -> Result<()> {
    let mut tx = store.pool().begin().await?;
    let Some(row) = sqlx::query("SELECT * FROM ci_debug_report WHERE uploaded_at IS NULL AND next_attempt_at<=now() ORDER BY next_attempt_at LIMIT 1 FOR UPDATE SKIP LOCKED")
        .fetch_optional(&mut *tx).await? else { return Ok(()) };
    let job: String = row.get("job_id");
    let attempt: i32 = row.get("attempt");
    let sandbox: String = row.get("sandbox_id");
    let reference = ArtifactRef {
        run_id: row.get("run_id"), job_key: row.get("job_key"), workflow_id: String::new(),
        name: format!("debug-{attempt}-{sandbox}.json"),
        description: Some("CI diagnostics retained after VM deletion".into()),
        public: false, alias: None,
    };
    let payload: serde_json::Value = row.get("payload");
    let result = tokio::time::timeout(Duration::from_secs(10), sink.put(&reference, serde_json::to_vec_pretty(&payload)?)).await;
    match result {
        Ok(Ok(stored)) => {
            sqlx::query("UPDATE ci_debug_report SET uploaded_at=now(),s3_uri=$4,last_error=NULL,payload=NULL WHERE job_id=$1 AND attempt=$2 AND sandbox_id=$3")
                .bind(&job).bind(attempt).bind(&sandbox).bind(&stored.uri).execute(&mut *tx).await?;
            tracing::info!(%job, attempt, uri=%stored.uri, "archived private CI debug report");
        }
        result => {
            let error = match result { Ok(Err(e)) => e.to_string(), Err(_) => "S3 report upload timed out".into(), _ => unreachable!() };
            sqlx::query("UPDATE ci_debug_report SET last_error=$4,next_attempt_at=now()+interval '30 seconds' WHERE job_id=$1 AND attempt=$2 AND sandbox_id=$3")
                .bind(&job).bind(attempt).bind(&sandbox).bind(&error).execute(&mut *tx).await?;
            tracing::warn!(%job, %error, "report retained for S3 retry without retaining VM");
        }
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{artifacts::{ArtifactError, StoredArtifact}, store::{RunRequest, StepStatus, Store}};
    use std::sync::{Mutex, atomic::{AtomicBool, Ordering}};

    struct Sink { fail: AtomicBool, uploaded: Mutex<Vec<Vec<u8>>> }
    #[async_trait::async_trait]
    impl ArtifactSink for Sink {
        fn kind(&self) -> &'static str { "s3" }
        async fn put(&self, r: &ArtifactRef, bytes: Vec<u8>) -> Result<StoredArtifact, ArtifactError> {
            assert!(!r.public);
            assert!(r.alias.is_none());
            if self.fail.load(Ordering::SeqCst) { return Err(ArtifactError::Transport("S3 unavailable".into())); }
            self.uploaded.lock().unwrap().push(bytes.clone());
            Ok(StoredArtifact { sink: "s3", digest: None, size_bytes: bytes.len() as u64,
                uri: format!("s3://private-debug/{}/{}/{}", r.run_id, r.job_key, r.name), public_url: None })
        }
        async fn get(&self, _: &StoredArtifact) -> Result<Vec<u8>, ArtifactError> { unreachable!() }
    }

    #[tokio::test]
    #[ignore = "needs empty disposable CI_TEST_DATABASE_URL"]
    async fn report_survives_s3_failure_restart_and_source_deletion() {
        let db = std::env::var("CI_TEST_DATABASE_URL").unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::connect(&db, root.path().into(), Duration::from_secs(30)).await.unwrap();
        store.migrate().await.unwrap();
        let workflow = crate::workflow::Workflow::parse("debug.yml", "jobs:\n  build:\n    steps: [{run: echo test}]\n").unwrap();
        let plan = crate::plan::Plan::build(&workflow).unwrap();
        let run = crate::vm::new_id();
        store.create_run(&run, &RunRequest { sha: "immutable-revision".into(), ..Default::default() }, &plan).await.unwrap();
        let job = store.jobs_of(&run).await.unwrap().remove(0);
        let sid = format!("{}.compile", job.id);
        store.create_step(&sid, &job.id, 0, "Compile", None).await.unwrap();
        store.start_step(&sid, "durable-operation").await.unwrap();
        store.append_log(&sid, root.path(), "first chunk\n").await.unwrap();
        store.append_log(&sid, root.path(), "different second chunk\n").await.unwrap();
        store.finish_step(&sid, StepStatus::Failure, Some(23), Some("compiler failed")).await.unwrap();
        store.set_job_status(&job.id, crate::store::JobStatus::Failure, Some("compile failed")).await.unwrap();
        let mut tx = store.pool().begin().await.unwrap();
        enqueue(&mut tx, &job.id, "sb-owned").await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM ci_debug_report WHERE job_id=$1").bind(&job.id).fetch_one(store.pool()).await.unwrap(), 0);
        let mut tx = store.pool().begin().await.unwrap();
        enqueue(&mut tx, &job.id, "sb-owned").await.unwrap();
        enqueue(&mut tx, &job.id, "sb-owned").await.unwrap();
        tx.commit().await.unwrap();
        let sink = Sink { fail: AtomicBool::new(true), uploaded: Mutex::new(vec![]) };
        upload_one(&store, &sink).await.unwrap();
        let error: String = sqlx::query_scalar("SELECT last_error FROM ci_debug_report WHERE job_id=$1").bind(&job.id).fetch_one(store.pool()).await.unwrap();
        assert!(error.contains("S3 unavailable"));
        // Deleting the source run/logs cannot delete the upload obligation.
        sqlx::query("DELETE FROM ci_run WHERE id=$1").bind(&run).execute(store.pool()).await.unwrap();
        sqlx::query("UPDATE ci_debug_report SET next_attempt_at=now() WHERE job_id=$1").bind(&job.id).execute(store.pool()).await.unwrap();
        sink.fail.store(false, Ordering::SeqCst);
        let restarted = Store::connect(&db, root.path().join("restart"), Duration::from_secs(30)).await.unwrap();
        upload_one(&restarted, &sink).await.unwrap();
        upload_one(&restarted, &sink).await.unwrap();
        let uploads = sink.uploaded.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        let report: serde_json::Value = serde_json::from_slice(&uploads[0]).unwrap();
        assert_eq!(report["run"]["sha"], "immutable-revision");
        assert_eq!(report["job"]["sandbox"], "sb-owned");
        assert_eq!(report["job"]["status"], "failure");
        assert_eq!(report["steps"][0]["log"], "first chunk\ndifferent second chunk\n");
        assert_eq!(report["steps"][0]["operation_id"], "durable-operation");
        assert_eq!(report["steps"][0]["exit_code"], 23);
        assert!(report["job"].get("plan").is_none());
        drop(uploads);
        sqlx::query("DELETE FROM ci_debug_report WHERE job_id=$1").bind(&job.id).execute(store.pool()).await.unwrap();
    }
}
