//! In-process fences backed by the durable controller-rollout phase.

use crate::store::Store;
use sqlx::Row;
use std::sync::Arc;
use tokio::sync::{OwnedRwLockReadGuard, RwLock};

#[derive(Clone, Default)]
pub struct Lifecycle {
    admission: Arc<RwLock<()>>,
    work: Arc<RwLock<()>>,
}

impl Lifecycle {
    async fn phase(store: &Store) -> Result<Option<String>, String> {
        sqlx::query_scalar(
            "SELECT phase FROM ci_controller_rollout WHERE phase <> 'complete' ORDER BY created_at LIMIT 1",
        )
        .fetch_optional(store.pool())
        .await
        .map_err(|e| format!("could not read controller rollout phase: {e}"))
    }

    pub async fn admission(
        &self,
        store: &Store,
    ) -> Result<OwnedRwLockReadGuard<()>, String> {
        let permit = self.admission.clone().read_owned().await;
        match Self::phase(store).await? {
            None => Ok(permit),
            Some(phase) if phase == "pending" => Ok(permit),
            Some(phase) => Err(format!("controller rollout is {phase}; submissions are closed")),
        }
    }

    pub async fn work(&self, store: &Store) -> Result<OwnedRwLockReadGuard<()>, String> {
        let permit = self.work.clone().read_owned().await;
        match Self::phase(store).await? {
            None => Ok(permit),
            Some(phase) if matches!(phase.as_str(), "pending" | "draining") => Ok(permit),
            Some(phase) => Err(format!("controller rollout is {phase}; new work is paused")),
        }
    }

    pub async fn close_admission(&self, store: &Store, id: &str) -> Result<(), String> {
        let _exclusive = self.admission.write().await;
        let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
        let phase: Option<String> = sqlx::query_scalar(
            "SELECT phase FROM ci_controller_rollout WHERE id=$1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if phase.as_deref() != Some("pending") {
            return Err(format!("rollout {id} is not pending (phase: {phase:?})"));
        }
        sqlx::query("UPDATE ci_controller_rollout SET phase='draining',updated_at=now() WHERE id=$1 AND phase='pending'")
            .bind(id).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Attempt a non-blocking drain check. Existing work keeps its read permit;
    /// callers retry rather than queueing a writer that would stop the drain.
    pub async fn quiesce(&self, store: &Store, id: &str) -> Result<bool, String> {
        let Ok(_exclusive) = self.work.clone().try_write_owned() else {
            return Ok(false);
        };
        let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
        let phase: Option<String> = sqlx::query_scalar(
            "SELECT phase FROM ci_controller_rollout WHERE id=$1 FOR UPDATE",
        ).bind(id).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
        if phase.as_deref() == Some("quiesced") { return Ok(true); }
        if phase.as_deref() != Some("draining") {
            return Err(format!("rollout {id} cannot quiesce from phase {phase:?}"));
        }

        // Running jobs count even when their parent failed, except a native
        // lease that expired on a terminal run: native endpoints fence every
        // heartbeat/completion/upload, and poll cannot lease that run again.
        // Pending/queued jobs count while their run can still schedule them.
        let blocked: bool = sqlx::query(
            "SELECT EXISTS(SELECT 1 FROM ci_job j JOIN ci_run r ON r.id=j.run_id WHERE (j.status='running' AND NOT (r.status IN ('success','failure','cancelled') AND EXISTS (SELECT 1 FROM ci_native_job n WHERE n.job_id=j.id AND n.state='leased' AND n.lease_expires_at<=now()))) OR (j.status IN ('pending','queued') AND r.status NOT IN ('success','failure','cancelled'))) AS jobs, EXISTS(SELECT 1 FROM ci_vm_pool WHERE status IN ('claimed','building')) AS vms, EXISTS(SELECT 1 FROM ci_native_job WHERE state='leased' AND lease_expires_at>now()) AS native, EXISTS(SELECT 1 FROM ci_service_deployment WHERE id<>$1 AND status NOT IN ('passed','failed')) AS effects"
        ).bind(id).fetch_one(&mut *tx).await.map_err(|e|e.to_string())
        .map(|r| r.get::<bool,_>("jobs") || r.get::<bool,_>("vms") || r.get::<bool,_>("native") || r.get::<bool,_>("effects"))?;
        if blocked { tx.rollback().await.map_err(|e|e.to_string())?; return Ok(false); }
        let changed = sqlx::query("UPDATE ci_controller_rollout SET phase='quiesced',updated_at=now() WHERE id=$1 AND phase='draining'")
            .bind(id).execute(&mut *tx).await.map_err(|e|e.to_string())?.rows_affected();
        if changed != 1 { return Err(format!("rollout {id} changed while quiescing")); }
        tx.commit().await.map_err(|e|e.to_string())?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exclusive_fences_existing_and_future_readers() {
        let lifecycle = Lifecycle::default();
        let first = lifecycle.admission.clone().read_owned().await;
        let lock = lifecycle.admission.clone();
        let writer = tokio::spawn(async move { lock.write_owned().await });
        tokio::task::yield_now().await;
        assert!(lifecycle.admission.clone().try_read_owned().is_err());
        drop(first);
        let writer = writer.await.unwrap();
        assert!(lifecycle.admission.clone().try_read_owned().is_err());
        drop(writer);
        assert!(lifecycle.admission.clone().try_read_owned().is_ok());
    }

    #[tokio::test]
    async fn in_flight_work_makes_quiesce_lock_nonblocking() {
        let lifecycle = Lifecycle::default();
        let _work = lifecycle.work.clone().read_owned().await;
        assert!(lifecycle.work.clone().try_write_owned().is_err());
    }
}
