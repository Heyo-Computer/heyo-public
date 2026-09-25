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
        Self::register_in(pool,deployment_id,true).await
    }

    /// An empty table does not prove that an older, non-participating executor
    /// has stopped. Managed bootstrap requires a separately verified cutover;
    /// there is deliberately no environment switch to assert fencing here.
    pub async fn register_managed(pool: PgPool, deployment_id: &str) -> Result<Self, String> {
        Self::register_in(pool,deployment_id,false).await
    }

    async fn register_in(pool: PgPool, deployment_id: &str, initialize: bool) -> Result<Self, String> {
        if deployment_id.is_empty() { return Err("executor deployment identity must not be empty".into()); }
        let boot_id = Uuid::new_v4();
        let mut tx = pool.begin().await.map_err(db)?;
        sqlx::query("INSERT INTO ci_executor_boot(boot_id,deployment_id) VALUES($1,$2)")
            .bind(boot_id).bind(deployment_id).execute(&mut *tx).await.map_err(db)?;
        if initialize {
            sqlx::query("INSERT INTO ci_executor_owner(singleton,boot_id) VALUES(TRUE,$1) ON CONFLICT(singleton) DO NOTHING")
                .bind(boot_id).execute(&mut *tx).await.map_err(db)?;
        }
        let (owner, owner_deployment): (Uuid, String) = sqlx::query_as(
            "SELECT o.boot_id,b.deployment_id FROM ci_executor_owner o JOIN ci_executor_boot b ON b.boot_id=o.boot_id WHERE o.singleton=TRUE",
        ).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(||
            "managed CI bootstrap requires verified legacy-executor fencing and preserved shared state; empty ownership is not permission to schedule".to_owned())?;
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

    /// Called by the process-local lifecycle worker, never while holding an
    /// effect permit. Receipt, owner generation and retirement commit together.
    pub async fn retire_application(&self, request: &crate::application_lifecycle::Retirement) -> anyhow::Result<()> {
        anyhow::ensure!(request.target.boot_id == self.boot_id && request.target.deployment_id == self.deployment_id,
            "retirement target is not this process");
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(crate::lifecycle::DRAIN_LOCK).execute(&mut *tx).await?;
        let (owner, mut generation): (Uuid, i64) = sqlx::query_as(
            "SELECT boot_id,generation FROM ci_executor_owner WHERE singleton=TRUE FOR UPDATE",
        ).fetch_one(&mut *tx).await?;
        let (phase, hash): (String, String) = sqlx::query_as(
            "SELECT phase,request_hash FROM ci_application_retirement WHERE command_id=$1 AND target_boot=$2 FOR UPDATE",
        ).bind(&request.command_id).bind(self.boot_id).fetch_one(&mut *tx).await?;
        anyhow::ensure!(hash == request.hash()?, "retirement payload changed");
        if phase == "safe" { return Ok(()); }
        if owner == self.boot_id && phase == "pending" {
            sqlx::query("UPDATE ci_application_retirement SET phase='draining' WHERE command_id=$1")
                .bind(&request.command_id).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(());
        }
        // Never wait for a local effect while holding the shared admission
        // transaction. That effect may itself need the database drain lock.
        let Ok(mut local) = self.local.clone().try_write_owned() else { return Ok(()); };
        anyhow::ensure!(!local.retired, "retired process cannot produce another acknowledgment");
        let mut successor = None;
        if owner == self.boot_id {
            let obligations: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM ci_job j JOIN ci_run r ON r.id=j.run_id WHERE j.status='running' OR (j.status IN ('pending','queued') AND r.status NOT IN ('success','failure','cancelled'))) OR EXISTS(SELECT 1 FROM ci_native_job WHERE state='leased') OR EXISTS(SELECT 1 FROM ci_host_work) OR EXISTS(SELECT 1 FROM ci_vm_cleanup) OR EXISTS(SELECT 1 FROM ci_vm_pool WHERE status IN ('claimed','building','draining')) OR EXISTS(SELECT 1 FROM ci_service_deployment WHERE status NOT IN ('passed','failed')) OR EXISTS(SELECT 1 FROM ci_host_maintenance WHERE phase<>'passed') OR EXISTS(SELECT 1 FROM ci_host_heyvm_bootstrap WHERE phase NOT IN ('passed','superseded')) OR EXISTS(SELECT 1 FROM ci_service_deployment s WHERE s.status<>'passed' AND (EXISTS(SELECT 1 FROM ci_service_rollout r WHERE r.id=s.id) OR EXISTS(SELECT 1 FROM ci_host_app_lb h WHERE h.id=s.id))) OR EXISTS(SELECT 1 FROM ci_controller_rollout WHERE phase<>'complete')"
            ).fetch_one(&mut *tx).await?;
            if obligations { return Ok(()); }
            for candidate in &request.survivors {
                anyhow::ensure!(candidate.region != request.target.region && candidate.deployment_id != self.deployment_id,
                    "successor belongs to the retiring region or deployment");
                let ready: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM ci_executor_boot WHERE boot_id=$1 AND deployment_id=$2 AND NOT retired AND ready_at>now()-interval '30 seconds')",
                ).bind(candidate.boot_id).bind(&candidate.deployment_id).fetch_one(&mut *tx).await?;
                if ready { successor = Some(candidate.boot_id); break; }
            }
            let next = successor.ok_or_else(|| anyhow::anyhow!("no platform-approved surviving boot is ready; ownership retained"))?;
            generation = sqlx::query_scalar("UPDATE ci_executor_owner SET boot_id=$1,generation=generation+1,continuation_operation_id=NULL,transferred_at=now() WHERE singleton=TRUE RETURNING generation")
                .bind(next).fetch_one(&mut *tx).await?;
        }
        sqlx::query("UPDATE ci_executor_boot SET retired=TRUE WHERE boot_id=$1")
            .bind(self.boot_id).execute(&mut *tx).await?;
        let receipt = serde_json::json!({"commandId":request.command_id,"operationId":request.operation_id,
            "stepId":request.step_id,"serviceId":request.service_id,"target":request.target,
            "requestHash":hash,"successorBootId":successor,"ownerGeneration":generation});
        sqlx::query("UPDATE ci_application_retirement SET phase='safe',receipt=$2 WHERE command_id=$1 AND receipt IS NULL")
            .bind(&request.command_id).bind(receipt).execute(&mut *tx).await?;
        tx.commit().await?;
        local.retired = true;
        Ok(())
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
        // Lock before inspecting successor eligibility. An UPDATE's statement
        // snapshot can predate a wait on this row and otherwise miss retirement
        // committed by the lock holder. Retirement takes this same lock first.
        let owner: Uuid = sqlx::query_scalar(
            "SELECT boot_id FROM ci_executor_owner WHERE singleton=TRUE FOR UPDATE",
        ).fetch_one(&mut *tx).await.map_err(db)?;
        if owner != self.boot_id {
            return Err("executor ownership changed".into());
        }
        let row = sqlx::query(
            "UPDATE ci_executor_owner o SET boot_id=$2,generation=generation+1,continuation_operation_id=$3,transferred_at=now() \
             WHERE singleton=TRUE AND boot_id=$1 AND EXISTS (SELECT 1 FROM ci_executor_boot b JOIN ci_executor_boot source ON source.boot_id=$1 WHERE b.boot_id=$2 AND b.deployment_id<>source.deployment_id AND b.ready_at>now()-interval '30 seconds' AND NOT b.retired) \
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
    async fn managed_bootstrap_does_not_claim_empty_ownership() {
        let pool=fixture().await;
        assert!(ExecutorOwner::register_managed(pool.clone(),"managed-us").await.is_err());
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM ci_executor_owner").fetch_one(&pool).await.unwrap(),0);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM ci_executor_boot").fetch_one(&pool).await.unwrap(),0);
        let owner=ExecutorOwner::register(pool.clone(),"protocol-owner").await.unwrap();
        let standby=ExecutorOwner::register_managed(pool.clone(),"managed-us").await.unwrap();
        assert!(owner.effect_permit().await.is_ok());
        assert!(standby.effect_permit().await.is_err());
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
    async fn managed_retirement_receipt_replays_once_and_keeps_failed_effects_fenced() {
        use crate::application_lifecycle::{Instance, Retirement, status};
        let pool=fixture().await;
        for migration in crate::store::embedded_migrations() {
            sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        }
        let us=ExecutorOwner::register(pool.clone(),"dep-us-old").await.unwrap();
        let eu=ExecutorOwner::register(pool.clone(),"dep-eu-old").await.unwrap();
        let unapproved=ExecutorOwner::register(pool.clone(),"dep-unapproved").await.unwrap();
        eu.mark_ready().await.unwrap(); unapproved.mark_ready().await.unwrap();
        let instance=|owner:&ExecutorOwner,region:&str| Instance {deployment_id:owner.deployment_id.clone(),
            backend_server_id:format!("host-{region}"),backend_sandbox_id:format!("sb-{}",owner.boot_id()),
            region:region.into(),boot_id:owner.boot_id()};
        let first=Retirement {command_id:"op-us".into(),operation_id:"op".into(),step_id:"us-retire".into(),service_id:"ci".into(),
            target:instance(&us,"us3"),survivors:vec![instance(&eu,"eu1")]};
        async fn record(pool:&PgPool,request:&Retirement) {
            sqlx::query("INSERT INTO ci_application_retirement(command_id,target_boot,request_hash,request,phase) VALUES($1,$2,$3,$4,'pending')")
                .bind(&request.command_id).bind(request.target.boot_id).bind(request.hash().unwrap())
                .bind(serde_json::to_value(request).unwrap()).execute(pool).await.unwrap();
        }
        record(&pool,&first).await;
        assert!(eu.retire_application(&first).await.is_err(),"a peer cannot fabricate the target's local acknowledgment");
        let plan=crate::plan::Plan::build(&crate::workflow::Workflow::parse("test.yml","jobs:\n  build:\n    steps: [{run: 'true'}]\n").unwrap()).unwrap();
        let mut tx=pool.begin().await.unwrap();
        crate::store::Store::create_run_in(&mut tx,"failed-run",&crate::store::RunRequest::default(),&plan).await.unwrap();
        sqlx::query("UPDATE ci_run SET status='failure' WHERE id='failed-run'").execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE ci_job SET status='failure' WHERE run_id='failed-run'").execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO ci_host_work SELECT id,'host-unresolved',1 FROM ci_job WHERE run_id='failed-run'").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        us.retire_application(&first).await.unwrap(); // closes admission
        let mut tx=pool.begin().await.unwrap();
        assert!(crate::lifecycle::Lifecycle::admit_in(&mut tx).await.is_err());
        assert!(crate::lifecycle::Lifecycle::grant_in(&mut tx).await.is_ok(),"admitted work can still drain");
        tx.rollback().await.unwrap();
        us.retire_application(&first).await.unwrap();
        assert!(us.is_owner().await.unwrap(),"a failed parent does not resolve remote effects");
        assert_eq!(status(&pool,&first.command_id,&first.hash().unwrap()).await.unwrap()["status"],"pending");
        // This fixture now supplies positive completion of the retained effect.
        sqlx::query("DELETE FROM ci_host_work WHERE runner_hd_id='host-unresolved'").execute(&pool).await.unwrap();
        let permit=us.effect_permit().await.unwrap();
        us.retire_application(&first).await.unwrap();
        assert!(us.is_owner().await.unwrap());
        drop(permit);
        us.retire_application(&first).await.unwrap();
        let receipt=status(&pool,&first.command_id,&first.hash().unwrap()).await.unwrap();
        assert_eq!(receipt["receipt"]["successorBootId"],eu.boot_id().to_string());
        assert_eq!(receipt["receipt"]["ownerGeneration"],2);
        for _ in 0..3 {us.retire_application(&first).await.unwrap();}
        assert_eq!(status(&pool,&first.command_id,&first.hash().unwrap()).await.unwrap(),receipt);
        assert!(us.effect_permit().await.is_err()); assert!(unapproved.effect_permit().await.is_err());
        let mut tx=pool.begin().await.unwrap();
        crate::lifecycle::Lifecycle::admit_in(&mut tx).await.unwrap(); tx.rollback().await.unwrap();
        let candidate=ExecutorOwner::register(pool.clone(),"dep-us-candidate").await.unwrap();
        candidate.mark_ready().await.unwrap();
        let second=Retirement {command_id:"op-eu".into(),step_id:"eu-retire".into(),target:instance(&eu,"eu1"),
            survivors:vec![instance(&candidate,"us3")],..first.clone()};
        record(&pool,&second).await;
        eu.retire_application(&second).await.unwrap(); eu.retire_application(&second).await.unwrap();
        assert_eq!(status(&pool,&second.command_id,&second.hash().unwrap()).await.unwrap()["receipt"]["ownerGeneration"],3);
        candidate.effect_permit().await.unwrap(); assert!(eu.effect_permit().await.is_err());
        let mut changed=second.clone(); changed.operation_id="different".into();
        assert!(eu.retire_application(&changed).await.is_err());
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
    async fn standby_retirement_serializes_with_transfer() {
        use crate::application_lifecycle::{Instance, Retirement, status};
        let pool = fixture().await;
        for migration in crate::store::embedded_migrations() {
            sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        }
        let owner = ExecutorOwner::register(pool.clone(), "us3").await.unwrap();
        let standby = ExecutorOwner::register(pool.clone(), "eu1").await.unwrap();
        standby.mark_ready().await.unwrap();
        let request = Retirement {command_id:"retire-standby".into(), operation_id:"op".into(),
            step_id:"eu-retire".into(), service_id:"ci".into(), survivors:vec![],
            target:Instance {deployment_id:"eu1".into(), backend_server_id:"host-eu".into(),
                backend_sandbox_id:"sandbox-eu".into(), region:"eu1".into(), boot_id:standby.boot_id()}};
        let hash = request.hash().unwrap();
        sqlx::query("INSERT INTO ci_application_retirement(command_id,target_boot,request_hash,request,phase) VALUES($1,$2,$3,$4,'pending')")
            .bind(&request.command_id).bind(standby.boot_id()).bind(&hash)
            .bind(serde_json::to_value(&request).unwrap()).execute(&pool).await.unwrap();
        let (retirement, transfer) = tokio::join!(
            standby.retire_application(&request),
            async { owner.handoff_fence().await.unwrap()
                .transfer_to(standby.boot_id(), VerifiedDurableContinuation::FullyQuiesced).await },
        );
        retirement.unwrap();
        let receipt = status(&pool, &request.command_id, &hash).await.unwrap();
        if transfer.is_err() {
            assert_eq!(receipt["status"], "safe-to-retire");
            standby.retire_application(&request).await.unwrap();
            standby.mark_ready().await.unwrap();
            assert!(owner.effect_permit().await.is_ok());
            assert!(standby.effect_permit().await.is_err());
            assert_eq!(owner.ready_successor().await.unwrap(), None);
        } else {
            // Promotion won the owner-row lock: no standby acknowledgment is
            // allowed. With no approved survivor this owner must remain fenced
            // against retirement, not manufacture a safe receipt.
            assert_eq!(receipt["status"], "pending");
            assert!(standby.effect_permit().await.is_ok());
            assert!(owner.effect_permit().await.is_err());
            assert!(standby.retire_application(&request).await.is_err());
        }
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn transfer_rechecks_retirement_after_waiting_for_owner_lock() {
        let pool = fixture().await;
        let owner = ExecutorOwner::register(pool.clone(), "us3").await.unwrap();
        let standby = ExecutorOwner::register(pool.clone(), "eu1").await.unwrap();
        standby.mark_ready().await.unwrap();
        // Hold the retirement transaction open until transfer is demonstrably
        // waiting on it. This reproduces the stale statement-snapshot boundary.
        let mut retirement = pool.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *retirement).await.unwrap();
        sqlx::query("SELECT boot_id FROM ci_executor_owner WHERE singleton=TRUE FOR UPDATE")
            .execute(&mut *retirement).await.unwrap();
        let successor = standby.boot_id();
        let moving = owner.clone();
        let transfer = tokio::spawn(async move {
            moving.handoff_fence().await.unwrap()
                .transfer_to(successor, VerifiedDurableContinuation::FullyQuiesced).await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",
                ).bind(blocker).fetch_one(&pool).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        sqlx::query("UPDATE ci_executor_boot SET retired=TRUE WHERE boot_id=$1")
            .bind(successor).execute(&mut *retirement).await.unwrap();
        retirement.commit().await.unwrap();
        assert!(transfer.await.unwrap().unwrap_err().contains("retired"));
        assert!(owner.effect_permit().await.is_ok());
        assert!(standby.effect_permit().await.is_err());
        let generation: i64 = sqlx::query_scalar("SELECT generation FROM ci_executor_owner")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(generation, 1);
    }

    #[tokio::test]
    #[ignore = "needs disposable CI_TEST_DATABASE_URL"]
    async fn transfer_cannot_return_to_another_boot_of_the_same_deployment() {
        let pool = fixture().await;
        let first = ExecutorOwner::register(pool.clone(), "us3").await.unwrap();
        let second = ExecutorOwner::register(pool.clone(), "eu1").await.unwrap();
        let duplicate = ExecutorOwner::register(pool, "eu1").await.unwrap();
        second.mark_ready().await.unwrap();
        duplicate.mark_ready().await.unwrap();
        first.handoff_fence().await.unwrap()
            .transfer_to(second.boot_id(), VerifiedDurableContinuation::FullyQuiesced).await.unwrap();
        assert!(second.handoff_fence().await.unwrap()
            .transfer_to(duplicate.boot_id(), VerifiedDurableContinuation::FullyQuiesced).await.is_err());
        assert!(second.effect_permit().await.is_ok());
        assert!(duplicate.effect_permit().await.is_err());
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
