//! In-process fences backed by the durable controller-rollout phase.

use crate::store::Store;
use sqlx::Row;
use std::sync::Arc;
use tokio::sync::{OwnedRwLockReadGuard, RwLock};

// Shared by final admission/grant transactions and exclusive drain transitions.
// A process-local permit alone cannot fence a request on another HTTP replica.
const DRAIN_LOCK: i64 = 0x0c19_6472;

#[derive(Clone, Default)]
pub struct Lifecycle {
    admission: Arc<RwLock<()>>,
    work: Arc<RwLock<()>>,
}

impl Lifecycle {
    /// Re-prove global quiescence while the executor handoff fence is held.
    /// Terminal errors and expired leases are deliberately insufficient: all
    /// durable remote-effect obligations must have been positively removed.
    pub async fn verify_handoff_quiesced(&self, store: &Store, id: &str) -> Result<(), String> {
        let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(DRAIN_LOCK)
            .execute(&mut *tx).await.map_err(|e| e.to_string())?;
        let phase: Option<String> = sqlx::query_scalar("SELECT phase FROM ci_controller_rollout WHERE id=$1 FOR UPDATE")
            .bind(id).fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
        if !matches!(phase.as_deref(), Some("quiesced" | "submitting" | "verifying")) {
            return Err(format!("rollout {id} is not durably quiesced"));
        }
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ci_job j JOIN ci_run r ON r.id=j.run_id WHERE j.status='running' OR (j.status IN ('pending','queued') AND r.status NOT IN ('success','failure','cancelled'))) OR EXISTS(SELECT 1 FROM ci_native_job WHERE state='leased') OR EXISTS(SELECT 1 FROM ci_host_work) OR EXISTS(SELECT 1 FROM ci_vm_cleanup) OR EXISTS(SELECT 1 FROM ci_vm_pool WHERE status IN ('claimed','building','draining')) OR EXISTS(SELECT 1 FROM ci_service_deployment WHERE id<>$1 AND status NOT IN ('passed','failed'))"
        ).bind(id).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
        // These ledgers explicitly retain fences after a reported failure.
        // Never interpret a failed run or a polling timeout as remote teardown.
        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ci_host_maintenance WHERE phase<>'passed') OR EXISTS(SELECT 1 FROM ci_host_heyvm_bootstrap WHERE phase NOT IN ('passed','superseded')) OR EXISTS(SELECT 1 FROM ci_service_deployment s WHERE s.status<>'passed' AND (EXISTS(SELECT 1 FROM ci_service_rollout r WHERE r.id=s.id) OR EXISTS(SELECT 1 FROM ci_host_app_lb h WHERE h.id=s.id)))"
        ).fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
        if blocked || retained { return Err("durable external-effect obligations remain".into()); }
        tx.commit().await.map_err(|e| e.to_string())
    }

    async fn transaction_phase(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<Option<String>, String> {
        sqlx::query("SELECT pg_advisory_xact_lock_shared($1)").bind(DRAIN_LOCK)
            .execute(&mut **tx).await.map_err(|e| e.to_string())?;
        sqlx::query_scalar("SELECT phase FROM ci_controller_rollout WHERE phase <> 'complete' ORDER BY created_at LIMIT 1")
            .fetch_optional(&mut **tx).await.map_err(|e| e.to_string())
    }

    /// Recheck at the commit boundary after potentially slow source preparation.
    pub async fn admit_in(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), String> {
        match Self::transaction_phase(tx).await? {
            None => Ok(()),
            Some(phase) if matches!(phase.as_str(), "prepared" | "pending") => Ok(()),
            Some(phase) => Err(format!("controller rollout is {phase}; submissions are closed")),
        }
    }

    /// Existing admitted jobs may receive native execution grants while draining.
    pub async fn grant_in(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), String> {
        match Self::transaction_phase(tx).await? {
            None => Ok(()),
            Some(phase) if matches!(phase.as_str(), "prepared" | "pending" | "draining") => Ok(()),
            Some(phase) => Err(format!("controller rollout is {phase}; new work is paused")),
        }
    }

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
            Some(phase) if matches!(phase.as_str(), "prepared" | "pending") => Ok(permit),
            Some(phase) => Err(format!("controller rollout is {phase}; submissions are closed")),
        }
    }

    pub async fn work(&self, store: &Store) -> Result<OwnedRwLockReadGuard<()>, String> {
        let permit = self.work.clone().read_owned().await;
        match Self::phase(store).await? {
            None => Ok(permit),
            Some(phase) if matches!(phase.as_str(), "prepared" | "pending" | "draining") => Ok(permit),
            Some(phase) => Err(format!("controller rollout is {phase}; new work is paused")),
        }
    }

    pub async fn close_admission(&self, store: &Store, id: &str) -> Result<(), String> {
        let _exclusive = self.admission.write().await;
        let mut tx = store.pool().begin().await.map_err(|e| e.to_string())?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(DRAIN_LOCK)
            .execute(&mut *tx).await.map_err(|e| e.to_string())?;
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
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)").bind(DRAIN_LOCK)
            .fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
        if !locked { return Ok(false); }
        let phase: Option<String> = sqlx::query_scalar(
            "SELECT phase FROM ci_controller_rollout WHERE id=$1 FOR UPDATE",
        ).bind(id).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
        if phase.as_deref() == Some("quiesced") { return Ok(true); }
        if phase.as_deref() != Some("draining") {
            return Err(format!("rollout {id} cannot quiesce from phase {phase:?}"));
        }

        let cleanup: Vec<String> = sqlx::query_scalar("SELECT sandbox_id FROM ci_vm_cleanup ORDER BY created_at LIMIT 5")
            .fetch_all(&mut *tx).await.map_err(|e| e.to_string())?;
        if !cleanup.is_empty() {
            sqlx::query("UPDATE ci_service_deployment SET message=$2,updated_at=now() WHERE id=$1")
                .bind(id).bind(format!("Waiting for verified VM cleanup (automatic retries): {}", cleanup.join(", ")))
                .execute(&mut *tx).await.map_err(|e| e.to_string())?;
            tx.commit().await.map_err(|e| e.to_string())?;
            return Ok(false);
        }

        // Expiry and terminal status revoke future writes, but do not prove
        // remote execution ended. Keep native and host-work obligations until
        // their owner has positively completed or handed off cleanup.
        let blocked: bool = sqlx::query(
            "SELECT EXISTS(SELECT 1 FROM ci_job j JOIN ci_run r ON r.id=j.run_id WHERE j.status='running' OR (j.status IN ('pending','queued') AND r.status NOT IN ('success','failure','cancelled'))) AS jobs, (EXISTS(SELECT 1 FROM ci_vm_pool WHERE status IN ('claimed','building')) OR EXISTS(SELECT 1 FROM ci_host_work)) AS vms, EXISTS(SELECT 1 FROM ci_native_job WHERE state='leased') AS native, EXISTS(SELECT 1 FROM ci_service_deployment WHERE id<>$1 AND status NOT IN ('passed','failed')) AS effects"
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
