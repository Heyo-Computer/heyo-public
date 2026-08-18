//! The warm VM pool, and what busts it.
//!
//! A job would rather inherit a VM that already ran `apt-get install
//! build-essential` and already has a populated `~/.cargo` than boot a fresh
//! one. The question the pool answers is when that inheritance stops being
//! safe.
//!
//! ## The fingerprint
//!
//! ```text
//! sha256( canonical_json(vm block, minus cache_key_files)
//!       ‖ for each path in sorted(cache_key_files):
//!             path ‖ 0x00 ‖ sha256(contents)  — or the ABSENT marker
//!       )[..6]                                → 12 hex characters
//! ```
//!
//! Two decisions in there are worth stating.
//!
//! **`cache_key_files` is removed from the serialized spec before hashing.**
//! Otherwise editing the *list* would change the fingerprint even when every
//! listed file's content is identical, and a workflow that adds a file to the
//! list would rebuild every VM for no reason.
//!
//! **A missing file hashes to an explicit `ABSENT` marker, not to nothing.**
//! Skipping it would make "no `Cargo.lock`" and "an empty `Cargo.lock`"
//! indistinguishable, so *adding* a lockfile later would not bust the pool —
//! which is exactly the moment it most needs busting.
//!
//! ## The pool table is the source of truth, not the daemon
//!
//! `ci_vm_pool` survives a restart on purpose. Without it a crash orphans every
//! VM until its TTL expires, and the next run builds a second pool alongside the
//! one already sitting there.

use crate::vm::VmSpec;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// Who is holding a VM, and how long the claim is good for without a renewal.
///
/// One type because the two never travel apart: a holder with no expiry cannot
/// be reclaimed, and an expiry with no holder cannot say whose it is.
#[derive(Debug, Clone, Copy)]
pub struct Lease<'a> {
    /// The instance id — random per process, so a restart does not inherit its
    /// own previous life's claims.
    pub instance: &'a str,
    pub ttl: Duration,
}

/// Recorded for a `cache_key_files` entry that does not exist.
///
/// A distinct marker rather than an omission, so adding the file later changes
/// the fingerprint.
const ABSENT: &str = "\u{0}ABSENT";

/// Hex characters kept from the digest. 12 gives 48 bits — collision-free in
/// any realistic number of concurrent VM shapes, and short enough to read in a
/// sandbox name.
const FINGERPRINT_LEN: usize = 12;

/// Compute the pool key for a job's VM.
///
/// `workspace` is the materialized checkout; `cache_key_files` are resolved
/// relative to it. They are validated as relative, `..`-free paths when the
/// workflow is parsed, and re-checked here because this function reads files.
pub fn fingerprint(spec: &VmSpec, workspace: &Path) -> Result<String, PoolError> {
    let mut hashable = spec.clone();
    // Removed before serializing — see the module doc.
    hashable.cache_key_files = Vec::new();

    let mut h = Sha256::new();
    let json = serde_json::to_vec(&hashable).map_err(|e| PoolError::Encode(e.to_string()))?;
    h.update(&json);

    // Sorted, so the order the author listed them in does not change the key.
    let mut files = spec.cache_key_files.clone();
    files.sort();
    files.dedup();

    for rel in &files {
        if rel.starts_with('/') || rel.split('/').any(|s| s == "..") {
            return Err(PoolError::EscapingPath(rel.clone()));
        }
        h.update(rel.as_bytes());
        h.update([0u8]);
        match std::fs::read(workspace.join(rel)) {
            Ok(bytes) => {
                let mut fh = Sha256::new();
                fh.update(&bytes);
                h.update(fh.finalize());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => h.update(ABSENT.as_bytes()),
            Err(e) => {
                return Err(PoolError::UnreadableFile {
                    path: rel.clone(),
                    reason: e.to_string(),
                });
            }
        }
    }

    Ok(hex::encode(h.finalize())[..FINGERPRINT_LEN].to_string())
}

/// A VM this orchestrator owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PooledVm {
    pub sandbox_id: String,
    pub runner_hd_id: String,
    pub fingerprint: String,
    pub workflow_id: String,
    pub status: String,
    pub claimed_by_job: Option<String>,
}

impl PooledVm {
    fn from_row(r: &sqlx::postgres::PgRow) -> Self {
        Self {
            sandbox_id: r.get("sandbox_id"),
            runner_hd_id: r.get("runner_hd_id"),
            fingerprint: r.get("fingerprint"),
            workflow_id: r.get("workflow_id"),
            status: r.get("status"),
            claimed_by_job: r.get("claimed_by_job"),
        }
    }
}

/// A pooled VM as the dashboard shows it: the row, plus what its last job did.
///
/// The join is what makes the page answer the two questions worth asking of a
/// warm pool — "is anything being reused" (same fingerprint, same runner, more
/// than one row) and "is this left over from a build that failed".
#[derive(Debug, Clone)]
pub struct PooledVmView {
    pub sandbox_id: String,
    pub runner_hd_id: String,
    pub fingerprint: String,
    pub workflow_id: String,
    pub status: String,
    pub claimed_by_job: Option<String>,
    pub last_job: Option<String>,
    pub last_run_id: Option<String>,
    pub last_run_status: Option<String>,
    pub leased_by: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone)]
pub struct Pool {
    db: PgPool,
}

impl Pool {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }

    /// Take an idle VM on `runner` matching `fingerprint`, or `None`.
    ///
    /// One statement, and that is the point. `FOR UPDATE SKIP LOCKED` makes the
    /// read-and-claim atomic without a transaction the caller has to hold across
    /// a network round trip: two dispatchers racing for the same warm VM each
    /// get a different one, or one gets nothing, but never both the same. A
    /// select-then-update would hand one sandbox to two jobs, and then the
    /// second exec on it fails with `SandboxNotFound` for reasons nothing
    /// explains.
    ///
    /// Most-recently-used first: that VM has the warmest page cache and the most
    /// recently populated build caches.
    pub async fn claim(
        &self,
        runner: &str,
        fingerprint: &str,
        job_id: &str,
        lease: Lease<'_>,
    ) -> Result<Option<String>, PoolError> {
        let row = sqlx::query(
            "UPDATE ci_vm_pool
                SET status = 'claimed', claimed_by_job = $3, last_used_at = now(),
                    last_job = $3,
                    leased_by = $4, leased_until = now() + make_interval(secs => $5)
              WHERE sandbox_id = (
                    SELECT sandbox_id FROM ci_vm_pool
                     WHERE runner_hd_id = $1 AND fingerprint = $2 AND status = 'idle'
                     ORDER BY last_used_at DESC
                     FOR UPDATE SKIP LOCKED
                     LIMIT 1
              )
             RETURNING sandbox_id",
        )
        .bind(runner)
        .bind(fingerprint)
        .bind(job_id)
        .bind(lease.instance)
        .bind(lease.ttl.as_secs() as f64)
        .fetch_optional(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(row.map(|r| r.get("sandbox_id")))
    }

    /// The key a not-yet-created VM is recorded under.
    ///
    /// Derived from the job rather than minted, so a queue redelivery addresses
    /// the row the previous attempt left instead of adding a second one. The
    /// `building-` prefix cannot collide with a daemon-assigned id, which is
    /// `sb-…` or `dep-…`.
    pub fn building_id(job_id: &str) -> String {
        format!("building-{job_id}")
    }

    /// Record that a VM is being created, before there is one.
    ///
    /// This is the row that makes /vms say something during the longest silent
    /// stretch of a job: the iroh dial and the boot wait inside
    /// `Sandbox::create`. Written before the attempt and removed by whoever made
    /// it — replaced by [`Self::register`] when the daemon answers, deleted by
    /// [`Self::forget`] when it does not.
    ///
    /// Leased like a claim, because it needs the same guarantee: if this process
    /// dies mid-create nothing is renewing, and [`Self::sweep_stale_builds`]
    /// clears the row. Without a lease a crash would leave a VM that is for ever
    /// "building" on a page whose whole job is to say what is happening.
    ///
    /// Returns the placeholder key, so a caller cannot pass a different one to
    /// the cleanup than it used here.
    pub async fn begin_build(
        &self,
        job_id: &str,
        runner: &str,
        fingerprint: &str,
        workflow_id: &str,
        lease: Lease<'_>,
    ) -> Result<String, PoolError> {
        let id = Self::building_id(job_id);
        sqlx::query(
            "INSERT INTO ci_vm_pool
                (sandbox_id, runner_hd_id, fingerprint, workflow_id, status, claimed_by_job,
                 last_job, leased_by, leased_until)
             VALUES ($1,$2,$3,$4,'building',$5,$5,$6, now() + make_interval(secs => $7))
             ON CONFLICT (sandbox_id) DO UPDATE
                SET status='building', runner_hd_id=$2, fingerprint=$3, workflow_id=$4,
                    claimed_by_job=$5, last_job=$5, last_used_at=now(),
                    leased_by=$6, leased_until=now() + make_interval(secs => $7)",
        )
        .bind(&id)
        .bind(runner)
        .bind(fingerprint)
        .bind(workflow_id)
        .bind(job_id)
        .bind(lease.instance)
        .bind(lease.ttl.as_secs() as f64)
        .execute(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(id)
    }

    /// Drop `building` rows whose holder stopped renewing.
    ///
    /// The counterpart to [`Self::release_orphans`], and a delete rather than a
    /// release because there is nothing to release: the row stands for an
    /// attempt, not for a machine. A sandbox the dead process did manage to
    /// create before it died is left to its TTL, which is what happened before
    /// this state existed too.
    ///
    /// Scoped to `runners` for the reason every sweep here is: instances own
    /// disjoint hosts, and one must not clear another's in-flight work.
    pub async fn sweep_stale_builds(
        &self,
        runners: &[String],
        instance: &str,
    ) -> Result<u64, PoolError> {
        if runners.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "DELETE FROM ci_vm_pool
              WHERE status = 'building'
                AND runner_hd_id = ANY($1)
                AND leased_by IS DISTINCT FROM $2
                AND (leased_until IS NULL OR leased_until < now())",
        )
        .bind(runners)
        .bind(instance)
        .execute(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(result.rows_affected())
    }

    /// Record a freshly created VM as claimed by the job that made it.
    ///
    /// Registered `claimed` rather than `idle`: the creating job is about to use
    /// it, and a window where it is idle is a window where another job takes it.
    pub async fn register(
        &self,
        sandbox_id: &str,
        runner: &str,
        fingerprint: &str,
        workflow_id: &str,
        job_id: &str,
        lease: Lease<'_>,
    ) -> Result<(), PoolError> {
        sqlx::query(
            "INSERT INTO ci_vm_pool
                (sandbox_id, runner_hd_id, fingerprint, workflow_id, status, claimed_by_job,
                 last_job, leased_by, leased_until)
             VALUES ($1,$2,$3,$4,'claimed',$5,$5,$6, now() + make_interval(secs => $7))
             ON CONFLICT (sandbox_id) DO UPDATE
                SET status='claimed', claimed_by_job=$5, last_job=$5, last_used_at=now(),
                    leased_by=$6, leased_until=now() + make_interval(secs => $7)",
        )
        .bind(sandbox_id)
        .bind(runner)
        .bind(fingerprint)
        .bind(workflow_id)
        .bind(job_id)
        .bind(lease.instance)
        .bind(lease.ttl.as_secs() as f64)
        .execute(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(())
    }

    /// Hand a VM back for the next job with the same fingerprint.
    pub async fn release(&self, sandbox_id: &str) -> Result<(), PoolError> {
        sqlx::query(
            "UPDATE ci_vm_pool
                SET status='idle', claimed_by_job=NULL, last_used_at=now(),
                    leased_by=NULL, leased_until=NULL
              WHERE sandbox_id = $1",
        )
        .bind(sandbox_id)
        .execute(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(())
    }

    /// Forget a VM — it has been destroyed, or is being destroyed.
    pub async fn forget(&self, sandbox_id: &str) -> Result<(), PoolError> {
        sqlx::query("DELETE FROM ci_vm_pool WHERE sandbox_id = $1")
            .bind(sandbox_id)
            .execute(&self.db)
            .await
            .map_err(PoolError::sql)?;
        Ok(())
    }

    pub async fn get(&self, sandbox_id: &str) -> Result<Option<PooledVm>, PoolError> {
        let row = sqlx::query("SELECT * FROM ci_vm_pool WHERE sandbox_id = $1")
            .bind(sandbox_id)
            .fetch_optional(&self.db)
            .await
            .map_err(PoolError::sql)?;
        Ok(row.as_ref().map(PooledVm::from_row))
    }

    pub async fn on_runner(&self, runner: &str) -> Result<Vec<PooledVm>, PoolError> {
        let rows = sqlx::query(
            "SELECT * FROM ci_vm_pool WHERE runner_hd_id = $1 ORDER BY last_used_at DESC",
        )
        .bind(runner)
        .fetch_all(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(rows.iter().map(PooledVm::from_row).collect())
    }

    pub async fn all(&self) -> Result<Vec<PooledVm>, PoolError> {
        let rows = sqlx::query("SELECT * FROM ci_vm_pool ORDER BY runner_hd_id, last_used_at DESC")
            .fetch_all(&self.db)
            .await
            .map_err(PoolError::sql)?;
        Ok(rows.iter().map(PooledVm::from_row).collect())
    }

    /// Every pooled VM on the runners this instance serves, with the outcome of
    /// the run that last used each.
    pub async fn inventory(&self, runners: &[String]) -> Result<Vec<PooledVmView>, PoolError> {
        if runners.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT p.sandbox_id, p.runner_hd_id, p.fingerprint, p.workflow_id, p.status,
                    p.claimed_by_job, p.last_job, p.leased_by, p.created_at, p.last_used_at,
                    r.id AS last_run_id, r.status AS last_run_status
               FROM ci_vm_pool p
               LEFT JOIN ci_job j ON j.id = COALESCE(p.claimed_by_job, p.last_job)
               LEFT JOIN ci_run r ON r.id = j.run_id
              WHERE p.runner_hd_id = ANY($1)
              ORDER BY p.runner_hd_id, p.fingerprint, p.last_used_at DESC",
        )
        .bind(runners)
        .fetch_all(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(rows
            .iter()
            .map(|r| PooledVmView {
                sandbox_id: r.get("sandbox_id"),
                runner_hd_id: r.get("runner_hd_id"),
                fingerprint: r.get("fingerprint"),
                workflow_id: r.get("workflow_id"),
                status: r.get("status"),
                claimed_by_job: r.get("claimed_by_job"),
                last_job: r.get("last_job"),
                last_run_id: r.get("last_run_id"),
                last_run_status: r.get("last_run_status"),
                leased_by: r.get("leased_by"),
                created_at: r.get("created_at"),
                last_used_at: r.get("last_used_at"),
            })
            .collect())
    }

    /// Take one idle VM out of circulation so it can be destroyed.
    ///
    /// `draining` rather than a straight delete, for the reason
    /// [`Self::take_for_sweep`] gives: the row should only go away once the
    /// daemon confirms the sandbox is gone, and marking it first stops a
    /// concurrent `claim` handing out a machine that is about to be killed.
    ///
    /// A *claimed* VM is refused. It has a job on it, and destroying it would
    /// fail that build from underneath — cleaning up is for what was left
    /// behind, not for what is running.
    ///
    /// A `building` one is refused for a harder reason: its `sandbox_id` is a
    /// placeholder, so draining it would send the daemon a kill for a machine
    /// that does not exist and strand the row in `draining` when that failed.
    pub async fn take_one_for_sweep(
        &self,
        sandbox_id: &str,
        runners: &[String],
    ) -> Result<Option<PooledVm>, PoolError> {
        if runners.is_empty() {
            return Ok(None);
        }
        let row = sqlx::query(
            "UPDATE ci_vm_pool
                SET status = 'draining'
              WHERE sandbox_id = $1
                AND runner_hd_id = ANY($2)
                AND status NOT IN ('claimed','building')
             RETURNING *",
        )
        .bind(sandbox_id)
        .bind(runners)
        .fetch_optional(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(row.as_ref().map(PooledVm::from_row))
    }

    /// Take every idle VM whose last run failed.
    ///
    /// The cleanup somebody actually wants after a bad build: those machines are
    /// reusable as far as the fingerprint is concerned, which is exactly the
    /// problem — a VM left in a strange state by a failed build is handed to the
    /// next job that matches it.
    pub async fn take_failed_for_sweep(
        &self,
        runners: &[String],
    ) -> Result<Vec<PooledVm>, PoolError> {
        if runners.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "UPDATE ci_vm_pool
                SET status = 'draining'
              WHERE sandbox_id IN (
                    SELECT p.sandbox_id FROM ci_vm_pool p
                      JOIN ci_job j ON j.id = p.last_job
                      JOIN ci_run r ON r.id = j.run_id
                     WHERE p.status = 'idle'
                       AND p.runner_hd_id = ANY($1)
                       AND r.status IN ('failure','cancelled')
                     FOR UPDATE SKIP LOCKED
              )
             RETURNING *",
        )
        .bind(runners)
        .fetch_all(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(rows.iter().map(PooledVm::from_row).collect())
    }

    /// Idle VMs whose fingerprint is no longer wanted, or which have sat unused
    /// too long.
    ///
    /// **Scoped to `runners`, and that scope is not optional.** Jobs are sharded
    /// per runner precisely so several orchestrators can run at once, each
    /// owning a disjoint set of hosts. An unscoped sweep would let instance A
    /// mark instance B's VMs `draining` — taking them out of B's pool — and then
    /// try to destroy machines it has no tunnel to. The rows would be stranded
    /// in `draining` forever and the VMs would leak until their TTL.
    ///
    /// Returns them rather than destroying them: destroying needs a tunnel to
    /// the right runner, and the row should only go away once the daemon
    /// confirms. Marking `draining` first is what stops a concurrent `claim`
    /// from handing out a VM that is about to be killed.
    pub async fn take_for_sweep(
        &self,
        runners: &[String],
        live_fingerprints: &[String],
        idle_for_secs: i64,
    ) -> Result<Vec<PooledVm>, PoolError> {
        if runners.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "UPDATE ci_vm_pool
                SET status = 'draining'
              WHERE sandbox_id IN (
                    SELECT sandbox_id FROM ci_vm_pool
                     WHERE status = 'idle'
                       AND runner_hd_id = ANY($1)
                       AND (NOT (fingerprint = ANY($2))
                            OR last_used_at < now() - make_interval(secs => $3))
                     FOR UPDATE SKIP LOCKED
              )
             RETURNING *",
        )
        .bind(runners)
        .bind(live_fingerprints)
        .bind(idle_for_secs as f64)
        .fetch_all(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(rows.iter().map(PooledVm::from_row).collect())
    }

    /// Release VMs held by a job that is no longer running.
    ///
    /// Run at startup: a crash leaves rows `claimed` by a job that died with it,
    /// and without this those VMs are never handed out again — the pool leaks
    /// its whole capacity one restart at a time.
    ///
    /// Scoped to `runners` for the same reason [`take_for_sweep`] is. The job
    /// status check alone would in fact keep another instance's *running* work
    /// safe, but relying on that means a bug in status bookkeeping turns into
    /// two jobs sharing one sandbox — and the symptom of that is a
    /// `SandboxNotFound` from a exec that looks like a daemon fault.
    ///
    /// [`take_for_sweep`]: Self::take_for_sweep
    /// Renew the leases this instance holds.
    ///
    /// One statement for every VM in flight here, on a timer. What it publishes
    /// is not "this job is fine" but "this process is alive and still holding
    /// these" — which is the only claim another instance can safely act on.
    ///
    /// `building` rows are renewed alongside `claimed` ones. A cold boot can
    /// take minutes and the lease defaults to three, so without this a create
    /// that is merely slow would have its own placeholder swept out from under
    /// it and vanish from /vms halfway through.
    pub async fn renew_leases(&self, lease: Lease<'_>) -> Result<u64, PoolError> {
        let result = sqlx::query(
            "UPDATE ci_vm_pool
                SET leased_until = now() + make_interval(secs => $2)
              WHERE leased_by = $1 AND status IN ('claimed','building')",
        )
        .bind(lease.instance)
        .bind(lease.ttl.as_secs() as f64)
        .execute(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(result.rows_affected())
    }

    /// The VMs this instance is holding right now, with the host each is on.
    ///
    /// Claimed rows only: a claimed VM has a job on it, and that is the one that
    /// must not be reaped out from under a long build. An *idle* pooled VM is
    /// left to age out, which is what the sandbox TTL is for — renewing those
    /// too would mean a VM this app created never expires at all.
    pub async fn leased_by(&self, instance: &str) -> Result<Vec<(String, String)>, PoolError> {
        let rows = sqlx::query(
            "SELECT sandbox_id, runner_hd_id FROM ci_vm_pool
              WHERE leased_by = $1 AND status = 'claimed'",
        )
        .bind(instance)
        .fetch_all(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(rows
            .iter()
            .map(|r| (r.get("sandbox_id"), r.get("runner_hd_id")))
            .collect())
    }

    /// Return VMs whose holder has stopped renewing their lease.
    ///
    /// **The lease is the authority, not the job's status.** A job left
    /// `running` by a process that died is indistinguishable, from a row, from a
    /// job another instance is running right now — so keying on it meant an
    /// orchestrator could not reclaim even its own VMs after a restart. They
    /// stayed `claimed` until the sandbox TTL reaped them, and the row leaked
    /// until some later restart found the job terminal. An expired lease says
    /// something the job status cannot: nobody is holding this.
    ///
    /// A row with **no** lease is one written before this existed, so it falls
    /// back to the old job-status test. That matters for exactly one deploy —
    /// the one that introduces leases, where a previous build may still be
    /// running beside this one — and costs a clause to be safe through it.
    ///
    /// Still scoped to `runners`: a VM on a host this instance does not serve
    /// belongs to whichever instance does, however stale its lease looks.
    pub async fn release_orphans(
        &self,
        runners: &[String],
        instance: &str,
    ) -> Result<u64, PoolError> {
        if runners.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE ci_vm_pool p
                SET status='idle', claimed_by_job=NULL, leased_by=NULL, leased_until=NULL
              WHERE p.status = 'claimed'
                AND p.runner_hd_id = ANY($1)
                AND p.leased_by IS DISTINCT FROM $2
                AND (
                     p.leased_until < now()
                     OR (p.leased_until IS NULL
                         AND (p.claimed_by_job IS NULL
                              OR NOT EXISTS (
                                 SELECT 1 FROM ci_job j
                                  WHERE j.id = p.claimed_by_job
                                    AND j.status IN ('pending','queued','running'))))
                )",
        )
        .bind(runners)
        .bind(instance)
        .execute(&self.db)
        .await
        .map_err(PoolError::sql)?;
        Ok(result.rows_affected())
    }
}

#[derive(Debug)]
pub enum PoolError {
    Encode(String),
    EscapingPath(String),
    UnreadableFile { path: String, reason: String },
    Sql(String),
}

impl PoolError {
    fn sql(e: sqlx::Error) -> Self {
        Self::Sql(e.to_string())
    }
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(e) => write!(f, "could not serialize a vm spec for hashing: {e}"),
            Self::EscapingPath(p) => write!(
                f,
                "cache_key_files entry {p:?} escapes the checkout; it must be a \
                 relative path with no `..` segments"
            ),
            Self::UnreadableFile { path, reason } => write!(
                f,
                "could not read cache_key_files entry {path:?}: {reason}. A missing \
                 file is fine and busts the pool when it appears; this one exists \
                 but could not be read."
            ),
            Self::Sql(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for PoolError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in instance id and lease for tests that are not about leasing.
    const INSTANCE: &str = "ci-test-instance";
    const LEASE: Duration = Duration::from_secs(180);

    /// The common case: this test instance, with a lease that has time left.
    fn held() -> Lease<'static> {
        Lease {
            instance: INSTANCE,
            ttl: LEASE,
        }
    }

    /// A lease that is already spent, standing in for a holder that stopped.
    fn lapsed(instance: &str) -> Lease<'_> {
        Lease {
            instance,
            ttl: Duration::from_secs(0),
        }
    }
    use heyo_sdk::{SandboxDriver, SandboxSize};
    use std::collections::BTreeMap;

    fn spec() -> VmSpec {
        VmSpec {
            driver: SandboxDriver::Firecracker,
            image: Some("ubuntu:24.04".into()),
            build: None,
            size_class: Some(SandboxSize::Medium),
            disk_size_gb: Some(20),
            working_directory: Some("/workspace".into()),
            env_vars: BTreeMap::new(),
            setup_hooks: vec!["apt-get install -y build-essential".into()],
            cache_key_files: vec![],
            reuse: true,
            ttl_seconds: None,
        }
    }

    fn workspace() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ci-fp-{}", crate::vm::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_fingerprint_is_twelve_hex_characters() {
        let ws = workspace();
        let fp = fingerprint(&spec(), &ws).unwrap();
        assert_eq!(fp.len(), 12);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        std::fs::remove_dir_all(&ws).ok();
    }

    /// The same spec must produce the same key in every process and on every
    /// restart, or the pool is never hit twice.
    #[test]
    fn the_same_spec_hashes_the_same_way_every_time() {
        let ws = workspace();
        let a = fingerprint(&spec(), &ws).unwrap();
        for _ in 0..10 {
            assert_eq!(fingerprint(&spec(), &ws).unwrap(), a);
        }
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Every field of the vm block is part of the machine, so every one of them
    /// has to bust the pool.
    #[test]
    fn any_change_to_the_vm_block_busts_the_pool() {
        let ws = workspace();
        let base = fingerprint(&spec(), &ws).unwrap();

        let mut s = spec();
        s.image = Some("ubuntu:22.04".into());
        assert_ne!(fingerprint(&s, &ws).unwrap(), base, "image");

        let mut s = spec();
        s.size_class = Some(SandboxSize::Large);
        assert_ne!(fingerprint(&s, &ws).unwrap(), base, "size_class");

        let mut s = spec();
        s.driver = SandboxDriver::Kvm;
        assert_ne!(fingerprint(&s, &ws).unwrap(), base, "driver");

        let mut s = spec();
        s.setup_hooks.push("apt-get install -y cmake".into());
        assert_ne!(fingerprint(&s, &ws).unwrap(), base, "setup_hooks");

        let mut s = spec();
        s.env_vars.insert("RUSTFLAGS".into(), "-C lto".into());
        assert_ne!(fingerprint(&s, &ws).unwrap(), base, "env_vars");

        let mut s = spec();
        s.disk_size_gb = Some(40);
        assert_ne!(fingerprint(&s, &ws).unwrap(), base, "disk_size_gb");

        std::fs::remove_dir_all(&ws).ok();
    }

    /// The headline behaviour: touch a declared file's *contents* and the next
    /// run gets a fresh VM.
    #[test]
    fn changing_a_cache_key_files_content_busts_the_pool() {
        let ws = workspace();
        std::fs::write(ws.join("Cargo.lock"), "version = 3\n").unwrap();
        let mut s = spec();
        s.cache_key_files = vec!["Cargo.lock".into()];

        let before = fingerprint(&s, &ws).unwrap();
        std::fs::write(ws.join("Cargo.lock"), "version = 4\n").unwrap();
        let after = fingerprint(&s, &ws).unwrap();
        assert_ne!(before, after);

        // And putting the content back returns the original key, so a revert
        // reuses the VM that was already warm for it.
        std::fs::write(ws.join("Cargo.lock"), "version = 3\n").unwrap();
        assert_eq!(fingerprint(&s, &ws).unwrap(), before);
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Reordering the list is not a change to the machine.
    #[test]
    fn the_order_of_cache_key_files_does_not_matter() {
        let ws = workspace();
        std::fs::write(ws.join("a.lock"), "a").unwrap();
        std::fs::write(ws.join("b.lock"), "b").unwrap();

        let mut s1 = spec();
        s1.cache_key_files = vec!["a.lock".into(), "b.lock".into()];
        let mut s2 = spec();
        s2.cache_key_files = vec!["b.lock".into(), "a.lock".into()];

        assert_eq!(
            fingerprint(&s1, &ws).unwrap(),
            fingerprint(&s2, &ws).unwrap()
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Two files with swapped contents must not hash the same — the path is
    /// mixed in, so `a=x, b=y` differs from `a=y, b=x`.
    #[test]
    fn swapping_contents_between_two_files_is_a_change() {
        let ws = workspace();
        let mut s = spec();
        s.cache_key_files = vec!["a.lock".into(), "b.lock".into()];

        std::fs::write(ws.join("a.lock"), "x").unwrap();
        std::fs::write(ws.join("b.lock"), "y").unwrap();
        let before = fingerprint(&s, &ws).unwrap();

        std::fs::write(ws.join("a.lock"), "y").unwrap();
        std::fs::write(ws.join("b.lock"), "x").unwrap();
        assert_ne!(fingerprint(&s, &ws).unwrap(), before);
        std::fs::remove_dir_all(&ws).ok();
    }

    /// The reason a missing file gets an explicit marker: adding the file must
    /// bust the pool, and it only can if "absent" and "empty" differ.
    #[test]
    fn adding_a_previously_missing_file_busts_the_pool() {
        let ws = workspace();
        let mut s = spec();
        s.cache_key_files = vec!["rust-toolchain.toml".into()];

        let missing = fingerprint(&s, &ws).unwrap();
        std::fs::write(ws.join("rust-toolchain.toml"), "").unwrap();
        let empty = fingerprint(&s, &ws).unwrap();
        assert_ne!(missing, empty, "absent must differ from present-but-empty");

        std::fs::write(ws.join("rust-toolchain.toml"), "[toolchain]\n").unwrap();
        assert_ne!(fingerprint(&s, &ws).unwrap(), empty);
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Editing the *list* while every listed file's content stays the same must
    /// not rebuild the VM — that is why the list is stripped before hashing.
    #[test]
    fn listing_a_file_whose_content_is_unchanged_is_still_a_change_but_reordering_is_not() {
        let ws = workspace();
        std::fs::write(ws.join("a.lock"), "a").unwrap();

        let none = fingerprint(&spec(), &ws).unwrap();
        let mut s = spec();
        s.cache_key_files = vec!["a.lock".into()];
        let listed = fingerprint(&s, &ws).unwrap();
        // Declaring a file *does* change the key, because the pool now depends
        // on something it did not before. What must not change it is the list's
        // order or duplicates.
        assert_ne!(none, listed);

        let mut dup = spec();
        dup.cache_key_files = vec!["a.lock".into(), "a.lock".into()];
        assert_eq!(
            fingerprint(&dup, &ws).unwrap(),
            listed,
            "a duplicate entry is not a change"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn an_escaping_cache_key_path_is_refused_even_here() {
        let ws = workspace();
        let mut s = spec();
        s.cache_key_files = vec!["../../etc/passwd".into()];
        assert!(matches!(
            fingerprint(&s, &ws),
            Err(PoolError::EscapingPath(_))
        ));
        std::fs::remove_dir_all(&ws).ok();
    }

    /// The key goes into a sandbox name, which the daemon parses as hex to
    /// derive a tap subnet.
    #[test]
    fn a_fingerprint_is_safe_in_a_sandbox_name() {
        let ws = workspace();
        let fp = fingerprint(&spec(), &ws).unwrap();
        let name = crate::vm::sandbox_name("build", &fp, 7);
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        std::fs::remove_dir_all(&ws).ok();
    }

    // ---- integration ----------------------------------------------------
    //
    //   CI_TEST_DATABASE_URL=... cargo test -- --ignored pool::

    async fn test_pool() -> (Pool, crate::store::Store) {
        let url = std::env::var("CI_TEST_DATABASE_URL").expect("CI_TEST_DATABASE_URL");
        let dir = std::env::temp_dir().join(format!("ci-pool-logs-{}", crate::vm::new_id()));
        let store = crate::store::Store::connect(&url, dir).await.unwrap();
        store
            .migrate(Path::new("migrations"))
            .await
            .expect("migrations");
        let pool = Pool::new(store.pool().clone());
        (pool, store)
    }

    /// A distinct instance per test, for the tests that count rows this
    /// instance holds — `renew_leases` is deliberately not runner-scoped, so a
    /// shared id would make it pick up every other test's rows.
    fn instance_id() -> String {
        format!("ci-{}", crate::vm::new_id())
    }

    /// A distinct runner per test so they never contend.
    fn runner_id() -> String {
        format!("hd-{}", crate::vm::new_id().replace('-', ""))
    }

    /// `sandbox_id` is the pool's primary key and therefore global, while the
    /// runner id is per-test. Deriving one from the other keeps concurrent test
    /// runs from overwriting each other's rows.
    fn sb(runner: &str, name: &str) -> String {
        format!("sb-{name}-{}", runner.trim_start_matches("hd-"))
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn an_idle_vm_with_a_matching_fingerprint_is_reused() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();

        pool.register(&sb(&runner, "1"), &runner, "fp-a", "wf", "job-1", held())
            .await
            .unwrap();
        // Claimed by its creator, so nobody else can take it yet.
        assert_eq!(
            pool.claim(&runner, "fp-a", "job-2", held()).await.unwrap(),
            None
        );

        pool.release(&sb(&runner, "1")).await.unwrap();
        assert_eq!(
            pool.claim(&runner, "fp-a", "job-2", held()).await.unwrap(),
            Some(sb(&runner, "1")),
            "the next job with the same fingerprint inherits it"
        );
        // And now it is taken again.
        assert_eq!(
            pool.claim(&runner, "fp-a", "job-3", held()).await.unwrap(),
            None
        );
    }

    /// The busting behaviour, at the pool level: a different fingerprint must
    /// not match a warm VM.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_different_fingerprint_does_not_match() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        pool.register(&sb(&runner, "2"), &runner, "fp-a", "wf", "job-1", held())
            .await
            .unwrap();
        pool.release(&sb(&runner, "2")).await.unwrap();
        assert_eq!(
            pool.claim(&runner, "fp-b", "job-2", held()).await.unwrap(),
            None
        );
        assert_eq!(
            pool.claim(&runner, "fp-a", "job-2", held()).await.unwrap(),
            Some(sb(&runner, "2"))
        );
    }

    /// The pool is host-local: a warm VM on one host is no use to a job pinned
    /// to another.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_warm_vm_on_another_runner_is_not_reused() {
        let (pool, _s) = test_pool().await;
        let a = runner_id();
        let b = runner_id();
        pool.register(&sb(&a, "3"), &a, "fp-a", "wf", "job-1", held())
            .await
            .unwrap();
        pool.release(&sb(&a, "3")).await.unwrap();
        assert_eq!(pool.claim(&b, "fp-a", "job-2", held()).await.unwrap(), None);
    }

    /// Two dispatchers racing must never be handed the same sandbox — the
    /// second exec on it would fail with `SandboxNotFound`.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn concurrent_claims_never_hand_out_the_same_vm() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        for i in 0..4 {
            let id = format!("sb-race-{i}-{}", crate::vm::new_id());
            pool.register(&id, &runner, "fp-race", "wf", "job-0", held())
                .await
                .unwrap();
            pool.release(&id).await.unwrap();
        }

        // Eight claimers for four VMs.
        let mut handles = Vec::new();
        for i in 0..8 {
            let p = pool.clone();
            let r = runner.clone();
            handles.push(tokio::spawn(async move {
                p.claim(&r, "fp-race", &format!("job-{i}"), held())
                    .await
                    .unwrap()
            }));
        }
        let mut claimed = Vec::new();
        for h in handles {
            if let Some(id) = h.await.unwrap() {
                claimed.push(id);
            }
        }

        assert_eq!(claimed.len(), 4, "exactly the four available VMs");
        let unique: std::collections::BTreeSet<_> = claimed.iter().collect();
        assert_eq!(unique.len(), 4, "no VM was handed out twice: {claimed:?}");
    }

    /// The lease that a dead instance stopped renewing is what makes its VMs
    /// reclaimable — not the state of the job it was running.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_lapsed_lease_returns_the_vm_to_the_pool() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        // Registered by a process that is now gone, with a lease that has run
        // out because nobody is renewing it.
        pool.register(
            &sb(&runner, "orphan"),
            &runner,
            "fp-o",
            "wf",
            "job-gone",
            lapsed("ci-previous-life"),
        )
        .await
        .unwrap();
        assert_eq!(
            pool.claim(&runner, "fp-o", "job-x", held()).await.unwrap(),
            None,
            "claimed rows are not handed out"
        );

        let released = pool
            .release_orphans(std::slice::from_ref(&runner), INSTANCE)
            .await
            .unwrap();
        assert_eq!(released, 1, "only this runner's orphan");
        assert_eq!(
            pool.claim(&runner, "fp-o", "job-x", held()).await.unwrap(),
            Some(sb(&runner, "orphan")),
            "a VM whose holder stopped renewing must come back"
        );
    }

    /// The bug this lease exists to fix.
    ///
    /// A restart leaves the job row `running` — nothing marks it otherwise,
    /// because the process that would have died with it. Keying reclaim on the
    /// job's status therefore refused to take the VM back, and it stayed
    /// `claimed` until its sandbox TTL reaped it. The lease says what the job
    /// status cannot: the holder is gone.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_restart_reclaims_its_own_vm_even_with_the_job_still_running() {
        let (pool, store) = test_pool().await;
        let runner = runner_id();

        // A real job row, left exactly as a killed process leaves one.
        let run_id = crate::vm::new_id();
        let wf = crate::workflow::Workflow::parse(
            "wf.yml",
            "name: t\njobs:\n  build:\n    vm: { driver: firecracker }\n    \
             steps: [{ run: \"true\" }]\n",
        )
        .expect("workflow");
        let plan = crate::plan::Plan::build(&wf).expect("plan");
        store
            .create_run(&run_id, &crate::store::RunRequest::default(), &plan)
            .await
            .unwrap();
        let job = crate::store::job_id(&run_id, "build");
        store
            .start_job(&job, &runner, &sb(&runner, "live"), "fp-l", 1)
            .await
            .unwrap();
        let row = store.get_job(&job).await.unwrap().unwrap();
        assert_eq!(
            row.status, "running",
            "the fixture must reproduce the state"
        );

        // Held by the process that has just been replaced.
        pool.register(
            &sb(&runner, "live"),
            &runner,
            "fp-l",
            "wf",
            &job,
            lapsed("ci-previous-life"),
        )
        .await
        .unwrap();

        let released = pool
            .release_orphans(std::slice::from_ref(&runner), INSTANCE)
            .await
            .unwrap();
        assert_eq!(
            released, 1,
            "a running job must not pin a VM whose holder is gone"
        );
        assert_eq!(
            pool.claim(&runner, "fp-l", "job-next", held())
                .await
                .unwrap(),
            Some(sb(&runner, "live"))
        );
    }

    /// The other half, and the one that must never regress: a VM another
    /// instance is genuinely holding stays held. Two orchestrators on one
    /// sandbox is far worse than a VM reclaimed a minute late.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_live_instances_vm_is_left_alone() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        pool.register(
            &sb(&runner, "theirs"),
            &runner,
            "fp-t",
            "wf",
            "job-theirs",
            Lease {
                instance: "ci-other-instance",
                ttl: LEASE,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            pool.release_orphans(std::slice::from_ref(&runner), INSTANCE)
                .await
                .unwrap(),
            0,
            "a lease with time left is somebody still working"
        );

        // And my own leases are never taken by me, even if the clock says they
        // lapsed — a slow database must not make this process fight itself.
        pool.register(
            &sb(&runner, "mine"),
            &runner,
            "fp-m",
            "wf",
            "job-mine",
            lapsed(INSTANCE),
        )
        .await
        .unwrap();
        assert_eq!(
            pool.release_orphans(std::slice::from_ref(&runner), INSTANCE)
                .await
                .unwrap(),
            0,
            "an instance must never reclaim a VM it is holding"
        );
    }

    /// The two selections the cleanup page offers, and the guard that matters:
    /// a VM with a job on it is never taken.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn sweeping_takes_what_failed_runs_left_and_never_a_running_job() {
        let (pool, store) = test_pool().await;
        let runner = runner_id();
        let ours = std::slice::from_ref(&runner);

        // Two runs on this runner: one that failed, one still going.
        let mut ids = Vec::new();
        for status in [
            crate::store::RunStatus::Failure,
            crate::store::RunStatus::Running,
        ] {
            let run_id = crate::vm::new_id();
            let wf = crate::workflow::Workflow::parse(
                "wf.yml",
                "name: t\njobs:\n  build:\n    vm: { driver: firecracker }\n    \
                 steps: [{ run: \"true\" }]\n",
            )
            .unwrap();
            let plan = crate::plan::Plan::build(&wf).unwrap();
            store
                .create_run(&run_id, &crate::store::RunRequest::default(), &plan)
                .await
                .unwrap();
            store.set_run_status(&run_id, status, None).await.unwrap();
            ids.push(crate::store::job_id(&run_id, "build"));
        }

        // The failed run's VM went back to the pool, which is the problem: it is
        // reusable as far as the fingerprint is concerned.
        pool.register(
            &sb(&runner, "failed"),
            &runner,
            "fp-f",
            "wf",
            &ids[0],
            held(),
        )
        .await
        .unwrap();
        pool.release(&sb(&runner, "failed")).await.unwrap();
        // The running one is still claimed.
        pool.register(&sb(&runner, "busy"), &runner, "fp-b", "wf", &ids[1], held())
            .await
            .unwrap();

        let taken = pool.take_failed_for_sweep(ours).await.unwrap();
        assert_eq!(taken.len(), 1, "only the failed run's idle VM");
        assert_eq!(taken[0].sandbox_id, sb(&runner, "failed"));

        // Draining, so a concurrent claim cannot take it back while it is being
        // destroyed.
        assert_eq!(
            pool.claim(&runner, "fp-f", "job-next", held())
                .await
                .unwrap(),
            None
        );

        // And a claimed VM is refused outright, however it is asked for.
        assert!(
            pool.take_one_for_sweep(&sb(&runner, "busy"), ours)
                .await
                .unwrap()
                .is_none(),
            "a VM running a job must never be swept"
        );

        // The inventory carries the run outcome the page groups on.
        let inv = pool.inventory(ours).await.unwrap();
        let failed = inv
            .iter()
            .find(|v| v.sandbox_id == sb(&runner, "failed"))
            .unwrap();
        assert_eq!(failed.last_run_status.as_deref(), Some("failure"));
        assert_eq!(failed.last_job.as_deref(), Some(ids[0].as_str()));
    }

    /// The keepalive's input: which VMs this instance is running a job on, and
    /// where each lives. Claimed only — an idle pooled VM is meant to age out.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn held_vms_are_listed_for_the_ttl_keepalive() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        let me = instance_id();

        pool.register(
            &sb(&runner, "busy"),
            &runner,
            "fp-k",
            "wf",
            "job-1",
            Lease {
                instance: &me,
                ttl: LEASE,
            },
        )
        .await
        .unwrap();
        pool.register(
            &sb(&runner, "idle"),
            &runner,
            "fp-k",
            "wf",
            "job-2",
            Lease {
                instance: &me,
                ttl: LEASE,
            },
        )
        .await
        .unwrap();
        pool.release(&sb(&runner, "idle")).await.unwrap();
        // Another instance's VM is not this one's to keep alive.
        pool.register(
            &sb(&runner, "theirs"),
            &runner,
            "fp-k",
            "wf",
            "job-3",
            Lease {
                instance: "ci-other-instance",
                ttl: LEASE,
            },
        )
        .await
        .unwrap();

        let held = pool.leased_by(&me).await.unwrap();
        assert_eq!(
            held,
            vec![(sb(&runner, "busy"), runner.clone())],
            "only the claimed VM this instance holds"
        );
    }

    /// Renewal is what keeps a lease alive, and it must only touch this
    /// instance's rows.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn renewing_extends_only_this_instances_leases() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        let me = instance_id();
        pool.register(
            &sb(&runner, "mine"),
            &runner,
            "fp-r",
            "wf",
            "job-1",
            lapsed(&me),
        )
        .await
        .unwrap();
        pool.register(
            &sb(&runner, "theirs"),
            &runner,
            "fp-r",
            "wf",
            "job-2",
            lapsed("ci-other-instance"),
        )
        .await
        .unwrap();

        assert_eq!(
            pool.renew_leases(Lease {
                instance: &me,
                ttl: LEASE
            })
            .await
            .unwrap(),
            1
        );

        // Theirs stayed lapsed, so it is reclaimable; mine was renewed, so it
        // is not.
        assert_eq!(
            pool.release_orphans(std::slice::from_ref(&runner), &me)
                .await
                .unwrap(),
            1
        );
    }

    /// Sweeping marks VMs `draining` so a concurrent claim cannot take one that
    /// is about to be destroyed.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn sweeping_takes_stale_fingerprints_out_of_circulation() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        pool.register(&sb(&runner, "live"), &runner, "fp-live", "wf", "j", held())
            .await
            .unwrap();
        pool.register(&sb(&runner, "dead"), &runner, "fp-dead", "wf", "j", held())
            .await
            .unwrap();
        pool.release(&sb(&runner, "live")).await.unwrap();
        pool.release(&sb(&runner, "dead")).await.unwrap();

        let swept = pool
            .take_for_sweep(
                std::slice::from_ref(&runner),
                &["fp-live".to_string()],
                86_400,
            )
            .await
            .unwrap();
        let ids: Vec<&str> = swept.iter().map(|v| v.sandbox_id.as_str()).collect();
        assert!(ids.contains(&sb(&runner, "dead").as_str()), "{ids:?}");
        assert!(
            !ids.contains(&sb(&runner, "live").as_str()),
            "a wanted fingerprint survives"
        );

        // Draining, so no longer claimable.
        assert_eq!(
            pool.claim(&runner, "fp-dead", "j2", held()).await.unwrap(),
            None
        );
        // And the wanted one is still there.
        assert_eq!(
            pool.claim(&runner, "fp-live", "j2", held()).await.unwrap(),
            Some(sb(&runner, "live"))
        );

        pool.forget(&sb(&runner, "dead")).await.unwrap();
        assert!(pool.get(&sb(&runner, "dead")).await.unwrap().is_none());
    }

    /// An idle VM nobody has wanted for a long time is swept even when its
    /// fingerprint is still current — otherwise a workflow that stops running
    /// keeps a machine's worth of VMs alive forever.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_long_idle_vm_is_swept_even_if_its_fingerprint_is_current() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        pool.register(&sb(&runner, "stale"), &runner, "fp-cur", "wf", "j", held())
            .await
            .unwrap();
        pool.release(&sb(&runner, "stale")).await.unwrap();

        // Nothing swept while it is fresh.
        assert!(
            pool.take_for_sweep(std::slice::from_ref(&runner), &["fp-cur".to_string()], 3600)
                .await
                .unwrap()
                .is_empty()
        );
        // Zero idle window: everything idle is stale.
        let swept = pool
            .take_for_sweep(std::slice::from_ref(&runner), &["fp-cur".to_string()], 0)
            .await
            .unwrap();
        assert!(swept.iter().any(|v| v.sandbox_id == sb(&runner, "stale")));
        pool.forget(&sb(&runner, "stale")).await.unwrap();
    }

    /// A VM being created is visible, and is not a VM: nothing may claim it,
    /// sweep it, or try to destroy it, because there is no sandbox behind the
    /// key yet.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_building_vm_is_shown_but_is_never_handed_out_or_destroyed() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        let ours = std::slice::from_ref(&runner);

        let placeholder = pool
            .begin_build("job-1", &runner, "fp-a", "wf", held())
            .await
            .unwrap();
        assert_eq!(placeholder, Pool::building_id("job-1"));

        // On the page, which is the whole point of the state.
        let seen = pool.inventory(ours).await.unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].status, "building");
        assert_eq!(seen[0].fingerprint, "fp-a");

        // But not a machine anything may take.
        assert_eq!(
            pool.claim(&runner, "fp-a", "job-2", held()).await.unwrap(),
            None,
            "a VM that does not exist yet must never be claimed"
        );
        assert!(
            pool.take_for_sweep(ours, &[], 0).await.unwrap().is_empty(),
            "the idle sweep must not take a build in flight"
        );
        assert!(
            pool.take_one_for_sweep(&placeholder, ours)
                .await
                .unwrap()
                .is_none(),
            "destroying a placeholder would post a kill for a sandbox that does not exist"
        );

        // And a lease still held is not abandoned.
        assert_eq!(
            pool.sweep_stale_builds(ours, "somebody-else")
                .await
                .unwrap(),
            0
        );

        // The creator clears it either way — replaced by the real row on
        // success, dropped on failure.
        pool.forget(&placeholder).await.unwrap();
        assert!(pool.inventory(ours).await.unwrap().is_empty());
    }

    /// A cold boot outlasts a lease period, so a build in flight has to be
    /// renewed like a claim — and one whose process died has to be cleared, or
    /// it says "creating…" for ever.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_build_is_renewed_while_it_runs_and_swept_once_its_holder_stops() {
        let (pool, _s) = test_pool().await;
        let runner = runner_id();
        let ours = std::slice::from_ref(&runner);
        let mine = instance_id();

        // Written with a lease that has already run out, standing in for a
        // process that stopped renewing.
        pool.begin_build("job-dead", &runner, "fp-a", "wf", lapsed(&mine))
            .await
            .unwrap();
        // Its own instance never reclaims from itself — that is what stops a
        // slow renewal from pulling a live build out from under itself.
        assert_eq!(pool.sweep_stale_builds(ours, &mine).await.unwrap(), 0);

        // Renewing puts it back in the future, so a sibling leaves it alone.
        assert_eq!(
            pool.renew_leases(Lease {
                instance: &mine,
                ttl: LEASE
            })
            .await
            .unwrap(),
            1,
            "a building row is renewed alongside claimed ones"
        );
        assert_eq!(pool.sweep_stale_builds(ours, "sibling").await.unwrap(), 0);

        // Stop renewing, and it goes.
        pool.begin_build("job-dead", &runner, "fp-a", "wf", lapsed(&mine))
            .await
            .unwrap();
        assert_eq!(pool.sweep_stale_builds(ours, "sibling").await.unwrap(), 1);
        assert!(pool.inventory(ours).await.unwrap().is_empty());
    }
}
