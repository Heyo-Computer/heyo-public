//! Non-expiring, explicitly transferred ownership of CI external effects.
//!
//! There is intentionally no heartbeat, expiry, or recovery/takeover path.
//! A process registers as a standby without changing the current owner. Every
//! external-effect operation must retain [`EffectPermit`] until the operation
//! is completely finished; checking ownership and then dropping the permit is
//! not sufficient.

use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use uuid::Uuid;

#[derive(Debug)]
struct LocalState {
    retired: bool,
}

/// A registered process boot. Clones share the same local transfer fence.
#[derive(Clone, Debug)]
pub struct ExecutorOwner {
    pool: PgPool,
    boot_id: Uuid,
    deployment_id: String,
    local: Arc<RwLock<LocalState>>,
}

/// Proof that this boot remained the durable owner while an effect was begun.
/// Keep this value alive across the *entire* external-effect operation.
#[derive(Debug)]
pub struct EffectPermit {
    _local: OwnedRwLockReadGuard<LocalState>,
    continuation_operation_id: Option<String>,
}

/// Holds the local effect boundary closed while the supervisor verifies durable
/// obligations. Verification must happen AFTER obtaining this fence.
pub struct HandoffFence {
    pool: PgPool,
    boot_id: Uuid,
    local: OwnedRwLockWriteGuard<LocalState>,
}

impl EffectPermit {
    /// Exact durable operation this owner must resume, if the handoff named one.
    pub fn continuation_operation_id(&self) -> Option<&str> {
        self.continuation_operation_id.as_deref()
    }
}

/// The narrow caller attestation required for an ownership handoff.
///
/// `FullyQuiesced` means shared durable state proves that no work owned by any
/// process remains in flight. `ContinueExactOperation` names the one durable,
/// idempotently resumable operation the successor must continue. Draining this
/// process's local tasks alone does not establish either condition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifiedDurableContinuation<'a> {
    FullyQuiesced,
    ContinueExactOperation(&'a str),
}

impl ExecutorOwner {
    /// Register this unique process boot. The first registration initializes
    /// ownership; every later registration is a standby and cannot steal it.
    pub async fn register(pool: PgPool, deployment_id: &str) -> Result<Self, String> {
        if deployment_id.is_empty() { return Err("executor deployment identity must not be empty".into()); }
        let boot_id = Uuid::new_v4();
        let mut tx = pool.begin().await.map_err(db)?;
        sqlx::query("INSERT INTO ci_executor_boot(boot_id,deployment_id) VALUES($1,$2)")
            .bind(boot_id).bind(deployment_id).execute(&mut *tx).await.map_err(db)?;
        sqlx::query("INSERT INTO ci_executor_owner(singleton,boot_id) VALUES(TRUE,$1) ON CONFLICT(singleton) DO NOTHING")
            .bind(boot_id).execute(&mut *tx).await.map_err(db)?;
        let (owner, owner_deployment): (Uuid, String) = sqlx::query_as(
            "SELECT o.boot_id,b.deployment_id FROM ci_executor_owner o JOIN ci_executor_boot b ON b.boot_id=o.boot_id WHERE o.singleton=TRUE",
        ).fetch_one(&mut *tx).await.map_err(db)?;
        if owner != boot_id && owner_deployment == deployment_id {
            return Err("another boot of this deployment still owns execution; refusing readiness because an ordinary replacement would strand non-expiring ownership".into());
        }
        tx.commit().await.map_err(db)?;
        Ok(Self { pool, boot_id, deployment_id: deployment_id.into(), local: Arc::new(RwLock::new(LocalState { retired: false })) })
    }

    pub fn boot_id(&self) -> Uuid {
        self.boot_id
    }

    /// For deciding whether to poll an owner-only reconciler, not permission to
    /// issue an effect. An effect still requires a retained EffectPermit.
    pub async fn is_owner(&self) -> Result<bool, String> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_executor_owner WHERE singleton=TRUE AND boot_id=$1)")
            .bind(self.boot_id).fetch_one(&self.pool).await.map_err(db)
    }

    /// Readiness only filters planned handoff candidates. Its age can never
    /// revoke ownership or authorize takeover after a crash.
    pub async fn mark_ready(&self) -> Result<(), String> {
        sqlx::query("UPDATE ci_executor_boot SET ready_at=now() WHERE boot_id=$1 AND NOT retired")
            .bind(self.boot_id).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    pub async fn ready_successor(&self) -> Result<Option<Uuid>, String> {
        sqlx::query_scalar("SELECT boot_id FROM ci_executor_boot WHERE boot_id<>$1 AND deployment_id<>$2 AND ready_at>now()-interval '30 seconds' AND NOT retired ORDER BY registered_at DESC LIMIT 1")
            .bind(self.boot_id).bind(&self.deployment_id).fetch_optional(&self.pool).await.map_err(db)
    }

    /// Acquire before beginning an external effect and retain through its
    /// completion. A standby or a transferred-away boot is refused.
    pub async fn effect_permit(&self) -> Result<EffectPermit, String> {
        self.effect_permit_for(None).await
    }

    pub async fn effect_permit_for(&self, operation: Option<&str>) -> Result<EffectPermit, String> {
        let local = self.local.clone().read_owned().await;
        if local.retired {
            return Err("executor boot has transferred ownership".into());
        }
        let (owner, continuation): (Uuid, Option<String>) = sqlx::query_as(
            "SELECT boot_id,continuation_operation_id FROM ci_executor_owner WHERE singleton=TRUE",
        )
            .fetch_one(&self.pool).await.map_err(db)?;
        if owner != self.boot_id {
            return Err("executor boot is a standby".into());
        }
        if let Some(id) = continuation.as_deref() {
            if Some(id) != operation {
                return Err(format!("executor is restricted to continuation {id}"));
            }
        }
        Ok(EffectPermit { _local: local, continuation_operation_id: continuation })
    }

    /// Wait for local effects, then keep them fenced while the caller verifies
    /// durable quiescence. Calling from inside an effect permit would deadlock.
    pub async fn handoff_fence(&self) -> Result<HandoffFence, String> {
        let local = self.local.clone().write_owned().await;
        if local.retired {
            return Err("executor boot has already transferred ownership".into());
        }
        Ok(HandoffFence { pool: self.pool.clone(), boot_id: self.boot_id, local })
    }
}

impl HandoffFence {
    /// CAS to a named successor after durable verification under this fence.
    /// Local draining alone never proves remote work settled. The supervisor
    /// must keep the shared admission/grant drain closed through this commit.
    pub async fn transfer_to(
        mut self,
        successor: Uuid,
        continuation: VerifiedDurableContinuation<'_>,
    ) -> Result<(), String> {
        if successor == self.boot_id {
            return Err("executor successor must be a different boot".into());
        }
        let operation = match continuation {
            VerifiedDurableContinuation::FullyQuiesced => None,
            VerifiedDurableContinuation::ContinueExactOperation(id) if !id.is_empty() => Some(id),
            VerifiedDurableContinuation::ContinueExactOperation(_) => {
                return Err("continuation operation identity must not be empty".into());
            }
        };
        let mut tx = self.pool.begin().await.map_err(db)?;
        let row = sqlx::query(
            "UPDATE ci_executor_owner o SET boot_id=$2,generation=generation+1,continuation_operation_id=$3,transferred_at=now() \
             WHERE singleton=TRUE AND boot_id=$1 AND EXISTS (SELECT 1 FROM ci_executor_boot b WHERE b.boot_id=$2 AND b.ready_at>now()-interval '30 seconds' AND NOT b.retired) \
             RETURNING generation",
        )
        .bind(self.boot_id).bind(successor).bind(operation)
        .fetch_optional(&mut *tx).await.map_err(db)?;
        if row.is_none() {
            return Err("executor ownership changed or successor is unregistered or retired".into());
        }
        sqlx::query("UPDATE ci_executor_boot SET retired=TRUE WHERE boot_id=$1")
            .bind(self.boot_id).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        // Set while holding the exclusive fence. Any delayed local wakeup can
        // only acquire the read side after this bit becomes visible.
        self.local.retired = true;
        Ok(())
    }
}

fn db(error: sqlx::Error) -> String {
    format!("executor ownership database error: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn fixture() -> PgPool {
        let base = std::env::var("CI_TEST_DATABASE_URL").expect("disposable CI_TEST_DATABASE_URL");
        let admin = PgPool::connect(&base).await.unwrap();
        let schema = format!("executor_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();
        admin.close().await;
        let mut url = reqwest::Url::parse(&base).unwrap();
        url.query_pairs_mut().append_pair("options", &format!("-c search_path={schema}"));
        let pool = PgPool::connect(url.as_str()).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/034_executor_owner.sql"))
            .execute(&pool).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/034_executor_owner.sql"))
            .execute(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn concurrent_initial_registration_selects_one_owner() {
        let pool = fixture().await;
        let (a, b) = tokio::join!(ExecutorOwner::register(pool.clone(), "us3"), ExecutorOwner::register(pool.clone(), "eu1"));
        let (a, b) = (a.unwrap(), b.unwrap());
        let admitted = usize::from(a.effect_permit().await.is_ok()) + usize::from(b.effect_permit().await.is_ok());
        assert_eq!(admitted, 1);
        let registrations: i64 = sqlx::query_scalar("SELECT count(*) FROM ci_executor_boot").fetch_one(&pool).await.unwrap();
        assert_eq!(registrations, 2);
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn standby_cannot_produce_effects() {
        let pool = fixture().await;
        let owner = ExecutorOwner::register(pool.clone(), "us3").await.unwrap();
        let standby = ExecutorOwner::register(pool, "eu1").await.unwrap();
        let _permit = owner.effect_permit().await.unwrap();
        assert!(standby.effect_permit().await.unwrap_err().contains("standby"));
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn stale_readiness_neither_selects_a_successor_nor_revokes_ownership() {
        let pool = fixture().await;
        let owner = ExecutorOwner::register(pool.clone(), "us3/ci").await.unwrap();
        let standby = ExecutorOwner::register(pool.clone(), "eu1/ci").await.unwrap();
        assert!(ExecutorOwner::register(pool.clone(), "us3/ci").await.is_err());
        standby.mark_ready().await.unwrap();
        assert_eq!(owner.ready_successor().await.unwrap(), Some(standby.boot_id()));
        sqlx::query("UPDATE ci_executor_boot SET ready_at=now()-interval '31 seconds'")
            .execute(&pool).await.unwrap();
        assert_eq!(owner.ready_successor().await.unwrap(), None);
        assert!(owner.handoff_fence().await.unwrap().transfer_to(standby.boot_id(), VerifiedDurableContinuation::FullyQuiesced).await.is_err());
        assert!(owner.effect_permit().await.is_ok());
        assert!(standby.effect_permit().await.is_err());
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn in_flight_permit_blocks_transfer() {
        let pool = fixture().await;
        let owner = ExecutorOwner::register(pool.clone(), "us3").await.unwrap();
        let standby = ExecutorOwner::register(pool, "eu1").await.unwrap();
        standby.mark_ready().await.unwrap();
        let permit = owner.effect_permit().await.unwrap();
        let moving = owner.clone();
        let successor = standby.boot_id();
        let mut transfer = tokio::spawn(async move {
            moving.handoff_fence().await?.transfer_to(successor, VerifiedDurableContinuation::FullyQuiesced).await
        });
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut transfer).await.is_err());
        drop(permit);
        transfer.await.unwrap().unwrap();
        standby.effect_permit().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn old_boot_cannot_resume_after_transfer() {
        let pool = fixture().await;
        let owner = ExecutorOwner::register(pool.clone(), "us3").await.unwrap();
        let standby = ExecutorOwner::register(pool, "eu1").await.unwrap();
        standby.mark_ready().await.unwrap();
        owner.handoff_fence().await.unwrap().transfer_to(standby.boot_id(), VerifiedDurableContinuation::ContinueExactOperation("replace-op-7"))
            .await.unwrap();
        assert!(owner.effect_permit().await.unwrap_err().contains("transferred"));
        assert!(standby.effect_permit().await.is_err());
        assert!(standby.effect_permit_for(Some("another-operation")).await.is_err());
        let permit = standby.effect_permit_for(Some("replace-op-7")).await.unwrap();
        assert_eq!(permit.continuation_operation_id(), Some("replace-op-7"));
        drop(permit);
        assert!(standby.handoff_fence().await.unwrap().transfer_to(owner.boot_id(), VerifiedDurableContinuation::FullyQuiesced)
            .await.unwrap_err().contains("retired"));
    }
}
