//! Postgres persistence for runs, jobs, steps, artifacts and the VM pool.
//!
//! Runtime `sqlx::query()` with `.bind()`, not the `query!` macros — so the
//! crate compiles with no database reachable, which is what heyosecret does and
//! what makes a CI build of this CI system possible.
//!
//! **Step logs are not in here.** A row holds a path and a byte count; the bytes
//! are appended to a file under `CI_LOG_DIR`. A build log is megabytes, and
//! putting it in a column means every `SELECT * FROM ci_step` for a status page
//! drags all of it across the wire.
//!
//! Migrations are applied by re-executing `migrations/*.sql` in filename order
//! on every startup, with no tracking table — heyosecret's approach. It puts one
//! obligation on the SQL (every statement idempotent) in exchange for removing a
//! whole class of "the migration table disagrees with the schema" incidents.

use crate::plan::{JobPlan, Plan};
use chrono::{DateTime, Utc};

use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Advisory-lock key serializing migrations across processes. An arbitrary
/// constant; it only has to be the same in every instance and unlikely to
/// collide with another application sharing the database.
const MIGRATION_LOCK_KEY: i64 = 0x0c19_3a7e;

/// How many times to retry a migration that lost a lock race with a running
/// instance. Contention is transient — the other side is a build finishing —
/// so a few short retries beat both failing the start and waiting forever.
const MIGRATION_ATTEMPTS: u32 = 5;

/// Whether a migration failure is the kind that retrying fixes.
///
/// Matched on the message because `sqlx::Error::Database` only exposes the
/// SQLSTATE through a downcast to the driver's error type, and both of these
/// arrive as plain database errors. The two codes are `40P01` (deadlock
/// detected) and `55P03` (lock not available).
fn is_lock_contention(e: &StoreError) -> bool {
    let text = e.to_string();
    text.contains("deadlock detected")
        || text.contains("lock timeout")
        || text.contains("canceling statement due to lock timeout")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Queued,
    Running,
    Success,
    Failure,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Success | Self::Failure | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    Queued,
    Running,
    Success,
    Failure,
    Skipped,
    Cancelled,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Success | Self::Failure | Self::Skipped | Self::Cancelled
        )
    }

    /// What `needs.<job>.result` reports. A skipped dependency is not a failure
    /// — GitHub's `needs` context reports it as `skipped`, and a downstream
    /// `if:` may legitimately want to run anyway.
    pub fn result_name(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
            _ => "pending",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Success,
    Failure,
    Skipped,
}

impl StepStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub workflow_id: String,
    pub workflow_path: String,
    pub workflow_name: Option<String>,
    pub repo_url: String,
    /// The registration this run was submitted under, when the credential (or
    /// the payload's URL) resolved to one. `None` for pre-registration rows
    /// and shared-secret submits of unregistered repositories.
    pub repo_id: Option<String>,
    /// `ci_repo.name`, joined in by every query that builds a `Run` — the
    /// display half of `repo_id`, denormalized here so pages never do a
    /// second lookup per row.
    pub repo_name: Option<String>,
    pub git_ref: String,
    pub sha: String,
    pub before_sha: String,
    /// What this run's commit changed, as the submit worked it out. Read by the
    /// scheduler for the `ci` expression scope, so a job's `if:` can gate on a
    /// subtree of a monorepo.
    pub changes: crate::paths::Changes,
    pub actor_email: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Run {
    fn from_row(r: &PgRow) -> Self {
        Self {
            id: r.get("id"),
            workflow_id: r.get("workflow_id"),
            workflow_path: r.get("workflow_path"),
            workflow_name: r.get("workflow_name"),
            repo_url: r.get("repo_url"),
            repo_id: r.get("repo_id"),
            // Not a column on `ci_run` — an alias every `Run` query joins in.
            // A query that forgets the join loses the display name, which the
            // page already renders as `None`; it does not take the page down.
            repo_name: r.try_get("repo_name").ok().flatten(),
            git_ref: r.get("git_ref"),
            sha: r.get("sha"),
            before_sha: r.get("before_sha"),
            // A row whose JSON does not deserialize falls back to "no answer",
            // which builds everything. The alternative — failing the query —
            // would take the dashboard down over a column nothing renders.
            changes: r
                .try_get::<serde_json::Value, _>("changes")
                .ok()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default(),
            actor_email: r.get("actor_email"),
            status: r.get("status"),
            error: r.get("error"),
            created_at: r.get("created_at"),
            started_at: r.get("started_at"),
            finished_at: r.get("finished_at"),
        }
    }

    /// How long the run took, or has been going.
    pub fn duration(&self) -> Option<Duration> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Utc::now);
        (end - start).to_std().ok()
    }
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub run_id: String,
    pub job_key: String,
    pub base_id: String,
    pub display: String,
    pub network: Option<String>,
    pub runner_hd_id: Option<String>,
    pub fingerprint: Option<String>,
    pub sandbox_id: Option<String>,
    pub status: String,
    pub attempt: i32,
    pub matrix: serde_json::Value,
    pub outputs: serde_json::Value,
    /// The expanded `JobPlan` this job runs. The queue message carries only
    /// ids, so this is what a redelivery executes.
    pub plan: serde_json::Value,
    pub error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl JobRow {
    fn from_row(r: &PgRow) -> Self {
        Self {
            id: r.get("id"),
            run_id: r.get("run_id"),
            job_key: r.get("job_key"),
            base_id: r.get("base_id"),
            display: r.get("display"),
            network: r.get("network"),
            runner_hd_id: r.get("runner_hd_id"),
            fingerprint: r.get("fingerprint"),
            sandbox_id: r.get("sandbox_id"),
            status: r.get("status"),
            attempt: r.get("attempt"),
            matrix: r.get("matrix"),
            outputs: r.get("outputs"),
            plan: r.get("plan"),
            error: r.get("error"),
            started_at: r.get("started_at"),
            finished_at: r.get("finished_at"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StepRow {
    pub id: String,
    pub job_id: String,
    pub idx: i32,
    pub name: String,
    pub uses: Option<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub operation_id: Option<String>,
    pub log_path: Option<String>,
    pub log_bytes: i64,
    pub error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl StepRow {
    fn from_row(r: &PgRow) -> Self {
        Self {
            id: r.get("id"),
            job_id: r.get("job_id"),
            idx: r.get("idx"),
            name: r.get("name"),
            uses: r.get("uses"),
            status: r.get("status"),
            exit_code: r.get("exit_code"),
            operation_id: r.get("operation_id"),
            log_path: r.get("log_path"),
            log_bytes: r.get("log_bytes"),
            error: r.get("error"),
            started_at: r.get("started_at"),
            finished_at: r.get("finished_at"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactRow {
    pub name: String,
    pub sink: String,
    pub digest: Option<String>,
    pub size_bytes: i64,
    pub uri: String,
}

/// A registered repository.
#[derive(Debug, Clone)]
pub struct Repo {
    pub id: String,
    pub url: String,
    pub normalized: String,
    pub name: String,
    /// Overrides `CI_WORKFLOW_PATH` for this repository. A workflow object,
    /// where one exists, still wins.
    pub workflow_path: Option<String>,
    /// The heyvm network this repository's builds run in, by name. `None` is the
    /// installation default; a workflow's `uses:` still overrides it per job.
    pub network: Option<String>,
    pub enabled: bool,
    pub created_email: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Repo {
    fn from_row(r: &PgRow) -> Self {
        Self {
            id: r.get("id"),
            url: r.get("url"),
            normalized: r.get("normalized"),
            name: r.get("name"),
            workflow_path: r.get("workflow_path"),
            network: r.get("network"),
            enabled: r.get("enabled"),
            created_email: r.get("created_email"),
            created_at: r.get("created_at"),
        }
    }
}

/// One submit token, as everything except the authentication path sees it.
///
/// **There is deliberately no `secret_hash` field.** The digest is read by one
/// query, compared, and dropped; keeping it on the struct that the dashboard
/// renders would make leaking it a matter of one careless `(token.hash)` in a
/// template.
#[derive(Debug, Clone)]
pub struct RepoToken {
    pub id: String,
    pub repo_id: String,
    pub name: String,
    pub created_email: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl RepoToken {
    fn from_row(r: &PgRow) -> Self {
        Self {
            id: r.get("id"),
            repo_id: r.get("repo_id"),
            name: r.get("name"),
            created_email: r.get("created_email"),
            created_at: r.get("created_at"),
            last_used_at: r.get("last_used_at"),
            revoked_at: r.get("revoked_at"),
        }
    }

    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// What a run is being started for.
#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub workflow_id: String,
    /// The registration this submit authenticated as, when it authenticated as
    /// one. `None` for the shared-secret path, which cannot say.
    pub repo_id: Option<String>,
    pub repo_url: String,
    pub git_ref: String,
    pub sha: String,
    pub before_sha: String,
    /// What the submit changed, relative to `before_sha`. Defaults to "no
    /// answer", which is what a caller that cannot work it out should leave it
    /// as — that reads as "build everything" downstream.
    pub changes: crate::paths::Changes,
    pub actor_subject: Option<String>,
    pub actor_email: Option<String>,
    pub source: String,
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    log_dir: PathBuf,
}

impl Store {
    pub async fn connect(database_url: &str, log_dir: PathBuf) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            // Fail fast at startup rather than after the default 30s: a wrong
            // `CI_DATABASE_URL` should look like a configuration error, not a
            // hang.
            .acquire_timeout(Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(|e| StoreError::Connect(e.to_string()))?;
        tokio::fs::create_dir_all(&log_dir)
            .await
            .map_err(|e| StoreError::LogDir {
                path: log_dir.clone(),
                reason: e.to_string(),
            })?;
        Ok(Self { pool, log_dir })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Apply `migrations/*.sql` in filename order.
    ///
    /// Every file is re-executed on every startup, so each statement must be
    /// idempotent. There is no tracking table by design — see the module doc.
    ///
    /// **Serialized by a Postgres advisory lock**, and that is not belt and
    /// braces. `CREATE TABLE IF NOT EXISTS` is *not* concurrency-safe: two
    /// sessions both find the table absent, both proceed, and the loser fails
    /// with `duplicate key value violates unique constraint
    /// "pg_type_typname_nsp_index"` from the system catalogue. Since jobs are
    /// sharded per runner precisely so several orchestrators can run at once,
    /// two of them starting together is a normal event rather than a rare race.
    ///
    /// **And bounded by a lock timeout, with retries.** The advisory lock keeps
    /// migrators away from each other, but not away from *running* instances:
    /// `ALTER TABLE … ADD COLUMN` needs `ACCESS EXCLUSIVE` on a table that a
    /// live dispatcher is inserting into. Without a timeout that wait is
    /// unbounded — a rolling deploy would hang a starting instance behind a long
    /// build — and Postgres reports some of those cycles as `deadlock detected`
    /// rather than waiting at all. Failing fast and retrying turns both into a
    /// few seconds of startup delay.
    pub async fn migrate(&self, dir: &Path) -> Result<(), StoreError> {
        let mut last: Option<StoreError> = None;
        for attempt in 1..=MIGRATION_ATTEMPTS {
            match self.migrate_once(dir).await {
                Ok(()) => return Ok(()),
                Err(e) if is_lock_contention(&e) && attempt < MIGRATION_ATTEMPTS => {
                    tracing::warn!(
                        "migration attempt {attempt} hit lock contention with a running \
                         instance, retrying: {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(250 * attempt as u64)).await;
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| StoreError::Sql("migration retries exhausted".into())))
    }

    async fn migrate_once(&self, dir: &Path) -> Result<(), StoreError> {
        let mut conn = self.pool.acquire().await.map_err(StoreError::sql)?;

        // Bounded so a migration never blocks indefinitely behind a live
        // dispatcher's inserts. Session-scoped, and the connection is returned
        // to the pool afterwards, so it is reset explicitly below.
        sqlx::query("SET lock_timeout = '5s'")
            .execute(&mut *conn)
            .await
            .map_err(StoreError::sql)?;

        // The lock is held on this one connection and released explicitly
        // below; a dropped connection also releases it, so a panicking migrator
        // cannot wedge every future start.
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut *conn)
            .await
            .map_err(StoreError::sql)?;

        let result = self.run_migrations(dir, &mut conn).await;

        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut *conn)
            .await;
        // Undo the session setting; this connection goes back into the pool and
        // a 5s lock timeout on ordinary queries would be a surprising default.
        let _ = sqlx::query("SET lock_timeout = DEFAULT")
            .execute(&mut *conn)
            .await;
        result
    }

    async fn run_migrations(
        &self,
        dir: &Path,
        conn: &mut sqlx::PgConnection,
    ) -> Result<(), StoreError> {
        let mut entries = Vec::new();
        let mut rd = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| StoreError::Migrations {
                path: dir.to_path_buf(),
                reason: e.to_string(),
            })?;
        while let Some(entry) = rd.next_entry().await.map_err(|e| StoreError::Migrations {
            path: dir.to_path_buf(),
            reason: e.to_string(),
        })? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sql") {
                entries.push(path);
            }
        }
        entries.sort();

        if entries.is_empty() {
            return Err(StoreError::Migrations {
                path: dir.to_path_buf(),
                reason: "no .sql files found".to_string(),
            });
        }

        for path in entries {
            let sql =
                tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| StoreError::Migrations {
                        path: path.clone(),
                        reason: e.to_string(),
                    })?;
            sqlx::raw_sql(&sql)
                .execute(&mut *conn)
                .await
                .map_err(|e| StoreError::Migrations {
                    path: path.clone(),
                    reason: e.to_string(),
                })?;
            tracing::debug!("applied migration {}", path.display());
        }
        Ok(())
    }

    /// Insert a run and every job of its plan, in one transaction.
    ///
    /// All-or-nothing on purpose: a run whose jobs half-exist is worse than no
    /// run, because the dashboard shows a DAG that can never complete and
    /// nothing owns fixing it.
    pub async fn create_run(
        &self,
        run_id: &str,
        req: &RunRequest,
        plan: &Plan,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::sql)?;

        sqlx::query(
            "INSERT INTO ci_run (id, workflow_id, workflow_path, workflow_name, repo_url,
                                 git_ref, sha, before_sha, actor_subject, actor_email,
                                 source, status, repo_id, changes)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'queued',$12,$13)",
        )
        .bind(run_id)
        .bind(&req.workflow_id)
        .bind(&plan.workflow_path)
        .bind(&plan.workflow_name)
        .bind(&req.repo_url)
        .bind(&req.git_ref)
        .bind(&req.sha)
        .bind(&req.before_sha)
        .bind(&req.actor_subject)
        .bind(&req.actor_email)
        .bind(if req.source.is_empty() {
            "submit"
        } else {
            &req.source
        })
        .bind(&req.repo_id)
        // Serializing a `Changes` cannot fail — it is an enum of owned strings —
        // but the column is NOT NULL, so the fallback is the "no answer" shape
        // rather than a null that would break every read.
        .bind(serde_json::to_value(&req.changes).unwrap_or_else(|_| {
            serde_json::to_value(crate::paths::Changes::default()).unwrap_or_default()
        }))
        .execute(&mut *tx)
        .await
        .map_err(StoreError::sql)?;

        for job in &plan.jobs {
            let matrix = serde_json::to_value(&job.matrix).unwrap_or(serde_json::json!({}));
            let plan_json = serde_json::to_value(job).unwrap_or(serde_json::json!({}));
            sqlx::query(
                "INSERT INTO ci_job (id, run_id, job_key, base_id, display, network,
                                     fingerprint, status, matrix, plan)
                 VALUES ($1,$2,$3,$4,$5,$6,NULL,'pending',$7,$8)",
            )
            .bind(job_id(run_id, &job.key))
            .bind(run_id)
            .bind(&job.key)
            .bind(&job.base_id)
            .bind(&job.display)
            .bind(&job.target.network)
            .bind(&matrix)
            .bind(&plan_json)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::sql)?;
        }

        tx.commit().await.map_err(StoreError::sql)?;
        Ok(())
    }

    pub async fn get_run(&self, run_id: &str) -> Result<Option<Run>, StoreError> {
        let row = sqlx::query(
            "SELECT r.*, rp.name AS repo_name
               FROM ci_run r
               LEFT JOIN ci_repo rp ON rp.id = r.repo_id
              WHERE r.id = $1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(row.as_ref().map(Run::from_row))
    }

    /// Newest first. `repo_id` narrows to one registered repository; `None` is
    /// everything, so the unfiltered page and the filtered one are the same
    /// query rather than two that can drift.
    pub async fn recent_runs(
        &self,
        limit: i64,
        repo_id: Option<&str>,
    ) -> Result<Vec<Run>, StoreError> {
        let rows = sqlx::query(
            "SELECT r.*, rp.name AS repo_name
               FROM ci_run r
               LEFT JOIN ci_repo rp ON rp.id = r.repo_id
              WHERE $2::text IS NULL OR r.repo_id = $2
              ORDER BY r.created_at DESC
              LIMIT $1",
        )
        .bind(limit)
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(rows.iter().map(Run::from_row).collect())
    }

    pub async fn set_run_status(
        &self,
        run_id: &str,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        // The timestamps are set by the same statement that sets the status, so
        // a run can never be `running` with no `started_at` for a reader to trip
        // over.
        sqlx::query(
            "UPDATE ci_run
                SET status = $2,
                    error = COALESCE($3, error),
                    started_at = CASE WHEN $2 = 'running' AND started_at IS NULL
                                      THEN now() ELSE started_at END,
                    finished_at = CASE WHEN $2 IN ('success','failure','cancelled')
                                       THEN now() ELSE finished_at END
              WHERE id = $1",
        )
        .bind(run_id)
        .bind(status.as_str())
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(())
    }

    /// Recompute a run's status from its jobs, and return it.
    ///
    /// The run is the roll-up of its jobs rather than a separately maintained
    /// value, so the two cannot disagree — which is exactly what happens when
    /// a crash lands between "last job finished" and "mark the run done".
    pub async fn roll_up_run(&self, run_id: &str) -> Result<RunStatus, StoreError> {
        let rows = sqlx::query("SELECT status FROM ci_job WHERE run_id = $1")
            .bind(run_id)
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::sql)?;

        let statuses: Vec<String> = rows.iter().map(|r| r.get::<String, _>("status")).collect();
        let status = if statuses.is_empty() {
            RunStatus::Success
        } else if statuses.iter().any(|s| s == "cancelled") {
            RunStatus::Cancelled
        } else if statuses.iter().any(|s| s == "failure") {
            // A failure is terminal for the run even while other jobs are still
            // going: the answer to "did this commit pass" is already no.
            RunStatus::Failure
        } else if statuses
            .iter()
            .all(|s| matches!(s.as_str(), "success" | "skipped"))
        {
            RunStatus::Success
        } else {
            RunStatus::Running
        };

        self.set_run_status(run_id, status, None).await?;
        Ok(status)
    }

    /// Cancel a run and every job of it that has not finished.
    ///
    /// Returns how many jobs were stopped, and `None` if the run was already
    /// terminal — cancelling something that has finished is a no-op worth
    /// reporting rather than a silent success that looks like it did something.
    ///
    /// The jobs are marked first. A queued job is then dropped when JetStream
    /// delivers it, because [`Self::start_job`] refuses a job that is already
    /// terminal and `run_job` checks the same thing on entry; a running one
    /// notices at its next step boundary. So this one statement covers work in
    /// all three states without needing to reach any of them.
    pub async fn cancel_run(&self, run_id: &str) -> Result<Option<u64>, StoreError> {
        let run = sqlx::query(
            "UPDATE ci_run
                SET status='cancelled', finished_at=now(),
                    error=COALESCE(error, 'cancelled')
              WHERE id = $1 AND status NOT IN ('success','failure','cancelled')",
        )
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        if run.rows_affected() == 0 {
            return Ok(None);
        }

        let jobs = sqlx::query(
            "UPDATE ci_job
                SET status='cancelled', finished_at=now(),
                    error=COALESCE(error, 'cancelled')
              WHERE run_id = $1
                AND status NOT IN ('success','failure','skipped','cancelled')",
        )
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(Some(jobs.rows_affected()))
    }

    /// Jobs that have sat on a queue longer than a runner was ever going to
    /// take to pick them up.
    ///
    /// A job is pinned to its host's subject even when that host is not online —
    /// deliberately, because the warm pool is host-local and silently migrating
    /// discards the cache the pin asked for. But consumers are only bound for
    /// hosts that *are* online, so a job pinned to one that is not goes to a
    /// subject nothing reads. Without this it waits for ever.
    pub async fn jobs_waiting_longer_than(
        &self,
        wait: Duration,
        limit: i64,
    ) -> Result<Vec<JobRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT j.* FROM ci_job j
               JOIN ci_run r ON r.id = j.run_id
              WHERE j.status = 'queued'
                AND j.queued_at IS NOT NULL
                AND j.queued_at < now() - make_interval(secs => $1)
                AND r.status NOT IN ('success','failure','cancelled')
              ORDER BY j.queued_at
              LIMIT $2",
        )
        .bind(wait.as_secs() as f64)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(rows.iter().map(JobRow::from_row).collect())
    }

    /// Whether a job has been cancelled out from under whoever is running it.
    ///
    /// Its own query rather than [`Self::get_job`]: this is asked between every
    /// step, and the plan JSONB is the largest column on the row.
    pub async fn is_job_cancelled(&self, job_id: &str) -> Result<bool, StoreError> {
        let row = sqlx::query("SELECT status FROM ci_job WHERE id = $1")
            .bind(job_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(row.map(|r| r.get::<String, _>("status")) == Some("cancelled".to_string()))
    }

    pub async fn jobs_of(&self, run_id: &str) -> Result<Vec<JobRow>, StoreError> {
        let rows =
            sqlx::query("SELECT * FROM ci_job WHERE run_id = $1 ORDER BY created_at, job_key")
                .bind(run_id)
                .fetch_all(&self.pool)
                .await
                .map_err(StoreError::sql)?;
        Ok(rows.iter().map(JobRow::from_row).collect())
    }

    pub async fn get_job(&self, job_id: &str) -> Result<Option<JobRow>, StoreError> {
        let row = sqlx::query("SELECT * FROM ci_job WHERE id = $1")
            .bind(job_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(row.as_ref().map(JobRow::from_row))
    }

    /// Mark a job as picked up by a runner, before it has a VM.
    ///
    /// **The gap this closes is the whole of VM acquisition.** `start_job` runs
    /// only once a sandbox exists, and getting one means an iroh dial and a boot
    /// that together can take minutes — during which the row still said `queued`
    /// with no runner on it. Three things read that as "nobody took this job":
    /// the run page, which showed a build that was busy booting as not started;
    /// [`Self::jobs_waiting_longer_than`], which is a queue-for-an-offline-host
    /// reaper and would fail the job with a message blaming a host that was in
    /// fact online and working on it; and whoever was watching, for whom a
    /// failing-and-retrying create was indistinguishable from silence.
    ///
    /// So the transition to `running` happens when a consumer commits to the
    /// job, and `start_job` below is left to fill in *where* it ended up.
    /// `sandbox_id` and `fingerprint` stay null until then, which is the honest
    /// reading: running, no machine yet.
    ///
    /// Returns false when the job was already terminal — cancelled while it sat
    /// on the queue, or finished by a delivery whose ack was lost.
    pub async fn claim_job(
        &self,
        job_id: &str,
        runner_hd_id: &str,
        attempt: i32,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE ci_job
                SET status = 'running', runner_hd_id = $2, attempt = $3,
                    started_at = COALESCE(started_at, now())
              WHERE id = $1
                AND status NOT IN ('success','failure','skipped','cancelled')",
        )
        .bind(job_id)
        .bind(runner_hd_id)
        .bind(attempt)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(result.rows_affected() > 0)
    }

    /// Record why an attempt failed, without deciding the job's fate.
    ///
    /// A failed delivery is negative-acked and retried on the backoff ladder —
    /// 60s, then 5 minutes, then 15 — and until this existed the reason was
    /// written to the row only by the *last* attempt. So the first twenty
    /// minutes of a job that could never work (a VM image that is not on the
    /// host, a daemon refusing the driver) showed an empty error and a status of
    /// `queued`, and the log on the orchestrator was the only place the cause
    /// appeared at all.
    ///
    /// Guarded on non-terminal so a late-arriving attempt cannot scribble over
    /// the outcome of one that finished.
    pub async fn note_job_error(&self, job_id: &str, error: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE ci_job SET error = $2
              WHERE id = $1 AND status NOT IN ('success','failure','skipped','cancelled')",
        )
        .bind(job_id)
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(())
    }

    /// Record which machine a running job landed on.
    ///
    /// Returns false when the job was already terminal, which is how a
    /// redelivery of work that finished just before the ack is dropped instead
    /// of run twice.
    pub async fn start_job(
        &self,
        job_id: &str,
        runner_hd_id: &str,
        sandbox_id: &str,
        fingerprint: &str,
        attempt: i32,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE ci_job
                SET status = 'running', runner_hd_id = $2, sandbox_id = $3,
                    fingerprint = $4, attempt = $5,
                    started_at = COALESCE(started_at, now())
              WHERE id = $1
                AND status NOT IN ('success','failure','skipped','cancelled')",
        )
        .bind(job_id)
        .bind(runner_hd_id)
        .bind(sandbox_id)
        .bind(fingerprint)
        .bind(attempt)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(result.rows_affected() > 0)
    }

    /// Move a job from `pending` to `queued`.
    ///
    /// Returns false when it was not pending, which is what stops a second
    /// scheduler pass from publishing the same job twice. The queue's own
    /// `Nats-Msg-Id` dedup is the belt; this is the braces, and it also keeps
    /// the row's status honest.
    pub async fn queue_job(&self, job_id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE ci_job SET status = 'queued', queued_at = now()
                  WHERE id = $1 AND status = 'pending'",
        )
        .bind(job_id)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(result.rows_affected() > 0)
    }

    /// Put a job back on the runway after its message failed to publish.
    ///
    /// [`Self::queue_job`] commits `pending → queued` before anything reaches
    /// NATS, so a failed publish leaves a row claiming to be queued with nothing
    /// on the queue — and the two stores then disagree with nothing to notice.
    /// Returning it to `pending` makes the scheduler's own retry the repair.
    ///
    /// Guarded on `queued` so it can never take a job that has since started.
    pub async fn unqueue_job(&self, job_id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE ci_job SET status='pending', queued_at=NULL
              WHERE id = $1 AND status = 'queued'",
        )
        .bind(job_id)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(result.rows_affected() > 0)
    }

    /// Active runs that still have a job waiting to be scheduled.
    ///
    /// The scheduler is otherwise only driven by a submit and by jobs finishing,
    /// so a run whose every job failed to publish has nothing left to nudge it.
    pub async fn runs_with_pending_jobs(&self, limit: i64) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT r.id FROM ci_run r
               JOIN ci_job j ON j.run_id = r.id
              WHERE r.status NOT IN ('success','failure','cancelled')
                AND j.status = 'pending'
              ORDER BY r.id
              LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    pub async fn set_job_status(
        &self,
        job_id: &str,
        status: JobStatus,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE ci_job
                SET status = $2,
                    error = COALESCE($3, error),
                    finished_at = CASE WHEN $2 IN ('success','failure','skipped','cancelled')
                                       THEN now() ELSE finished_at END
              WHERE id = $1",
        )
        .bind(job_id)
        .bind(status.as_str())
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(())
    }

    pub async fn set_job_outputs(
        &self,
        job_id: &str,
        outputs: &serde_json::Value,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE ci_job SET outputs = $2 WHERE id = $1")
            .bind(job_id)
            .bind(outputs)
            .execute(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(())
    }

    /// The `needs` context for a run: `{ "<base_id>": { "result": …, "outputs": … } }`.
    ///
    /// Keyed on `base_id`, and a base with several matrix cells collapses to the
    /// worst result among them — `needs.build.result` has to mean "did build
    /// pass", and it did not if any cell failed.
    pub async fn needs_context(&self, run_id: &str) -> Result<serde_json::Value, StoreError> {
        let jobs = self.jobs_of(run_id).await?;
        let mut map = serde_json::Map::new();
        for job in &jobs {
            let entry = map
                .entry(job.base_id.clone())
                .or_insert_with(|| serde_json::json!({"result": "success", "outputs": {}}));
            let current = entry
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("success")
                .to_string();
            let worst = worse_of(&current, &job.status);
            entry["result"] = serde_json::Value::String(worst);
            if let (Some(dst), Some(src)) =
                (entry["outputs"].as_object_mut(), job.outputs.as_object())
            {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
        }
        Ok(serde_json::Value::Object(map))
    }

    // ---- steps ----------------------------------------------------------

    pub async fn create_step(
        &self,
        step_id: &str,
        job_id: &str,
        idx: i32,
        name: &str,
        uses: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO ci_step (id, job_id, idx, name, uses, status)
             VALUES ($1,$2,$3,$4,$5,'pending')
             ON CONFLICT (job_id, idx) DO NOTHING",
        )
        .bind(step_id)
        .bind(job_id)
        .bind(idx)
        .bind(name)
        .bind(uses)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(())
    }

    pub async fn start_step(&self, step_id: &str, operation_id: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE ci_step
                SET status='running', operation_id=$2, started_at=COALESCE(started_at, now())
              WHERE id = $1",
        )
        .bind(step_id)
        .bind(operation_id)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(())
    }

    pub async fn finish_step(
        &self,
        step_id: &str,
        status: StepStatus,
        exit_code: Option<i32>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE ci_step
                SET status=$2, exit_code=$3, error=$4, finished_at=now()
              WHERE id = $1",
        )
        .bind(step_id)
        .bind(status.as_str())
        .bind(exit_code)
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(())
    }

    pub async fn steps_of(&self, job_id: &str) -> Result<Vec<StepRow>, StoreError> {
        let rows = sqlx::query("SELECT * FROM ci_step WHERE job_id = $1 ORDER BY idx")
            .bind(job_id)
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(rows.iter().map(StepRow::from_row).collect())
    }

    // ---- logs -----------------------------------------------------------

    /// Where a step's log lives: `<log_dir>/<run>/<job_key>/<idx>-<step>.log`.
    pub fn log_path(&self, run_id: &str, job_key: &str, idx: i32, step_id: &str) -> PathBuf {
        self.log_dir
            .join(sanitize_component(run_id))
            .join(sanitize_component(job_key))
            .join(format!("{idx:03}-{}.log", sanitize_component(step_id)))
    }

    /// Append to a step's log and record the new size.
    ///
    /// The path is written on the row every time rather than only on the first
    /// append, so a row created before the file existed still points at it.
    pub async fn append_log(
        &self,
        step_id: &str,
        path: &Path,
        text: &str,
    ) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| StoreError::LogDir {
                    path: parent.to_path_buf(),
                    reason: e.to_string(),
                })?;
        }
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .map_err(|e| StoreError::LogDir {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        f.write_all(text.as_bytes())
            .await
            .map_err(|e| StoreError::LogDir {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;

        sqlx::query("UPDATE ci_step SET log_path = $2, log_bytes = log_bytes + $3 WHERE id = $1")
            .bind(step_id)
            .bind(path.to_string_lossy().as_ref())
            .bind(text.len() as i64)
            .execute(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(())
    }

    pub async fn read_log(&self, step: &StepRow) -> Option<String> {
        let path = step.log_path.as_ref()?;
        tokio::fs::read_to_string(path).await.ok()
    }

    pub async fn record_artifact(
        &self,
        run_id: &str,
        job_id: &str,
        name: &str,
        stored: &crate::artifacts::StoredArtifact,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO ci_artifact (id, run_id, job_id, name, sink, digest, size_bytes, uri)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(crate::vm::new_id())
        .bind(run_id)
        .bind(job_id)
        .bind(name)
        .bind(stored.sink)
        .bind(&stored.digest)
        .bind(stored.size_bytes as i64)
        .bind(&stored.uri)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(())
    }

    pub async fn artifacts_of(&self, run_id: &str) -> Result<Vec<ArtifactRow>, StoreError> {
        let rows = sqlx::query("SELECT * FROM ci_artifact WHERE run_id = $1 ORDER BY created_at")
            .bind(run_id)
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(rows
            .iter()
            .map(|r| ArtifactRow {
                name: r.get("name"),
                sink: r.get("sink"),
                digest: r.get("digest"),
                size_bytes: r.get("size_bytes"),
                uri: r.get("uri"),
            })
            .collect())
    }

    /// Runs old enough to sweep that still have a log recorded.
    ///
    /// Driven by the rows rather than by walking the log directory: a directory
    /// whose run was deleted has nothing to update, and a run whose files were
    /// removed by hand should stop being offered every hour. The `EXISTS`
    /// clause is what makes the sweep converge — once the paths are nulled the
    /// run is not returned again.
    pub async fn runs_with_logs_before(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT r.id FROM ci_run r
              WHERE r.created_at < $1
                AND EXISTS (
                      SELECT 1 FROM ci_job j
                        JOIN ci_step s ON s.job_id = j.id
                       WHERE j.run_id = r.id AND s.log_path IS NOT NULL)
              ORDER BY r.created_at
              LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Forget where a run's logs were, after deleting them.
    ///
    /// The rows survive — a step that ran and its exit code are the run's
    /// history, and losing that because the log aged out would make an old run
    /// look like it never happened. Only the pointer and the byte count go.
    pub async fn forget_logs_of(&self, run_id: &str) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "UPDATE ci_step SET log_path = NULL, log_bytes = 0
              WHERE log_path IS NOT NULL
                AND job_id IN (SELECT id FROM ci_job WHERE run_id = $1)",
        )
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(result.rows_affected())
    }

    /// The directory holding every log of one run.
    pub fn run_log_dir(&self, run_id: &str) -> PathBuf {
        self.log_dir.join(sanitize_component(run_id))
    }

    // ---- registered repositories ----------------------------------------

    /// Register a repository, or update the one already registered at that URL.
    ///
    /// Upsert rather than insert, keyed on the normalized URL: someone
    /// registering `git@github.com:me/app.git` when
    /// `https://github.com/me/app` is already registered means to edit that
    /// registration, not to create a second one that competes with it for the
    /// same submits. Existing tokens keep working, which is the point — the
    /// alternative is that fixing a typo in a display name invalidates
    /// everyone's credential.
    pub async fn register_repo(
        &self,
        url: &str,
        name: &str,
        workflow_path: Option<&str>,
        network: Option<&str>,
        actor: Option<(&str, &str)>,
    ) -> Result<Repo, StoreError> {
        let row = sqlx::query(
            "INSERT INTO ci_repo (id, url, normalized, name, workflow_path, network,
                                  created_by, created_email)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
             ON CONFLICT (normalized) DO UPDATE
                SET url = EXCLUDED.url,
                    name = EXCLUDED.name,
                    workflow_path = EXCLUDED.workflow_path,
                    network = EXCLUDED.network
             RETURNING *",
        )
        .bind(crate::vm::new_id())
        .bind(url)
        .bind(crate::repos::normalize(url))
        .bind(name)
        .bind(workflow_path)
        .bind(network)
        .bind(actor.map(|a| a.0))
        .bind(actor.map(|a| a.1))
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(Repo::from_row(&row))
    }

    /// Point a repository at a heyvm network, or back at the default.
    ///
    /// Its own route rather than a re-registration, because reassigning a
    /// network is the thing an operator does most often here — moving a
    /// repository onto new hardware — and making that go through a form that
    /// also rewrites the name and the workflow path invites clobbering both.
    ///
    /// Takes effect on the next submit. Jobs already scheduled keep the network
    /// stamped into their stored plan, so a build in flight is not rerouted onto
    /// hardware it did not warm a VM on.
    pub async fn set_repo_network(
        &self,
        repo_id: &str,
        network: Option<&str>,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query("UPDATE ci_repo SET network = $2 WHERE id = $1")
            .bind(repo_id)
            .bind(network.map(str::trim).filter(|n| !n.is_empty()))
            .execute(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn repos(&self) -> Result<Vec<Repo>, StoreError> {
        let rows = sqlx::query("SELECT * FROM ci_repo ORDER BY name, normalized")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(rows.iter().map(Repo::from_row).collect())
    }

    pub async fn get_repo(&self, id: &str) -> Result<Option<Repo>, StoreError> {
        let row = sqlx::query("SELECT * FROM ci_repo WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(row.as_ref().map(Repo::from_row))
    }

    /// The registration for a clone URL, in any of its spellings.
    pub async fn repo_by_url(&self, url: &str) -> Result<Option<Repo>, StoreError> {
        let row = sqlx::query("SELECT * FROM ci_repo WHERE normalized = $1")
            .bind(crate::repos::normalize(url))
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(row.as_ref().map(Repo::from_row))
    }

    /// Stop, or resume, a repository submitting.
    ///
    /// Separate from revoking tokens because it answers a different question. A
    /// repository that should not be building right now — a migration, an
    /// incident, a repository that was archived — is not a repository whose
    /// tokens are compromised, and making the operator revoke and re-mint every
    /// one to pause it teaches them to leave it running instead.
    pub async fn set_repo_enabled(&self, id: &str, enabled: bool) -> Result<bool, StoreError> {
        let result = sqlx::query("UPDATE ci_repo SET enabled = $2 WHERE id = $1")
            .bind(id)
            .bind(enabled)
            .execute(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove a registration and every token it issued.
    ///
    /// Its runs survive with a null `repo_id` — deleting the registration must
    /// not delete the history of what it built.
    pub async fn delete_repo(&self, id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM ci_repo WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(StoreError::sql)?;
        Ok(result.rows_affected() > 0)
    }

    /// Mint a token for a repository and return it in plaintext — once.
    ///
    /// The plaintext is returned and not stored. There is no route that can
    /// show it again, because there is nothing left to show it from.
    pub async fn create_repo_token(
        &self,
        repo_id: &str,
        name: &str,
        actor: Option<(&str, &str)>,
    ) -> Result<(RepoToken, String), StoreError> {
        let minted = crate::repos::mint(&crate::vm::new_id());
        let row = sqlx::query(
            "INSERT INTO ci_repo_token (id, repo_id, name, secret_hash, created_by, created_email)
             VALUES ($1,$2,$3,$4,$5,$6)
             RETURNING *",
        )
        .bind(&minted.key_id)
        .bind(repo_id)
        .bind(name)
        .bind(&minted.secret_hash)
        .bind(actor.map(|a| a.0))
        .bind(actor.map(|a| a.1))
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok((RepoToken::from_row(&row), minted.plaintext))
    }

    pub async fn repo_tokens(&self, repo_id: &str) -> Result<Vec<RepoToken>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM ci_repo_token WHERE repo_id = $1
              ORDER BY revoked_at IS NOT NULL, created_at DESC",
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(rows.iter().map(RepoToken::from_row).collect())
    }

    /// Stop a token working, without forgetting it existed.
    ///
    /// Idempotent, and `revoked_at` is only ever set once: revoking twice must
    /// not rewrite when access actually ended.
    pub async fn revoke_repo_token(&self, token_id: &str) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE ci_repo_token SET revoked_at = now()
              WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(token_id)
        .execute(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(result.rows_affected() > 0)
    }

    /// Resolve a presented submit token to the repository it may submit for.
    ///
    /// `Ok(None)` covers every way of not being a valid credential —
    /// malformed, unknown key id, wrong secret, revoked, or a registration that
    /// has been disabled. The caller reports one message for all of them, for
    /// the same reason [`crate::trigger::TriggerError`] gives: telling a caller
    /// *which* part of their credential was wrong tells an attacker how far
    /// they got.
    ///
    /// A malformed presentation never reaches the database — this route is
    /// unauthenticated and on the open internet.
    pub async fn authenticate_repo_token(
        &self,
        presented: &str,
    ) -> Result<Option<Repo>, StoreError> {
        let Some((key_id, secret)) = crate::repos::parse(presented) else {
            return Ok(None);
        };

        let row = sqlx::query(
            "SELECT t.secret_hash AS secret_hash, r.*
               FROM ci_repo_token t
               JOIN ci_repo r ON r.id = t.repo_id
              WHERE t.id = $1 AND t.revoked_at IS NULL AND r.enabled",
        )
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::sql)?;

        let Some(row) = row else { return Ok(None) };
        let stored: String = row.get("secret_hash");
        if !crate::repos::secret_matches(secret, &stored) {
            return Ok(None);
        }

        // Best-effort: a submit that ran is not undone by failing to record
        // that its token was used.
        if let Err(e) = sqlx::query("UPDATE ci_repo_token SET last_used_at = now() WHERE id = $1")
            .bind(key_id)
            .execute(&self.pool)
            .await
        {
            tracing::warn!("could not record use of submit token {key_id}: {e}");
        }

        Ok(Some(Repo::from_row(&row)))
    }

    /// The most recent run of a registered repository, for the dashboard.
    pub async fn last_run_of_repo(&self, repo_id: &str) -> Result<Option<Run>, StoreError> {
        let row = sqlx::query(
            "SELECT r.*, rp.name AS repo_name
               FROM ci_run r
               LEFT JOIN ci_repo rp ON rp.id = r.repo_id
              WHERE r.repo_id = $1
              ORDER BY r.created_at DESC
              LIMIT 1",
        )
        .bind(repo_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(row.as_ref().map(Run::from_row))
    }

    // ---- users ----------------------------------------------------------

    /// Record a person and return their role, seeding admins from config.
    ///
    /// Keyed on the stable subject, so an email change updates the row rather
    /// than creating a second account for the same person.
    pub async fn upsert_user(
        &self,
        subject: &str,
        email: &str,
        name: Option<&str>,
        admin_emails: &[String],
    ) -> Result<String, StoreError> {
        let seed_role = if admin_emails
            .iter()
            .any(|a| a == &email.to_ascii_lowercase())
        {
            "admin"
        } else {
            "viewer"
        };
        let row = sqlx::query(
            "INSERT INTO ci_user (subject, email, name, role)
             VALUES ($1,$2,$3,$4)
             ON CONFLICT (subject) DO UPDATE
                SET email = EXCLUDED.email,
                    name = EXCLUDED.name,
                    last_seen_at = now(),
                    -- Promote on the config list, but never demote: a role
                    -- granted in the UI must survive the list changing.
                    role = CASE WHEN EXCLUDED.role = 'admin' THEN 'admin'
                                ELSE ci_user.role END
             RETURNING role",
        )
        .bind(subject)
        .bind(email)
        .bind(name)
        .bind(seed_role)
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::sql)?;
        Ok(row.get("role"))
    }
}

/// A job's row id, derived from the run and the job key so it is stable across
/// a redelivery — the same job always resolves to the same row.
pub fn job_id(run_id: &str, job_key: &str) -> String {
    format!("{run_id}.{job_key}")
}

/// A step's row id, likewise derived and therefore reusable as the daemon's
/// `operationId`: re-running the same step of the same job reattaches.
pub fn step_id(job_id: &str, idx: usize) -> String {
    format!("{job_id}.{idx}")
}

/// Rank two job statuses and return the worse, for rolling matrix cells up into
/// one `needs.<base>.result`.
fn worse_of(a: &str, b: &str) -> String {
    fn rank(s: &str) -> u8 {
        match s {
            "success" => 0,
            "skipped" => 1,
            "pending" | "queued" | "running" => 2,
            "cancelled" => 3,
            _ => 4, // failure
        }
    }
    if rank(b) > rank(a) {
        b.to_string()
    } else {
        a.to_string()
    }
}

/// Keep a path component inside the log directory.
///
/// Ids are already charset-restricted, but a log path is assembled from values
/// that reach us over HTTP, and one `..` would write outside `CI_LOG_DIR`.
fn sanitize_component(s: &str) -> String {
    // `.` is folded away too, not just `/`. Keeping dots would be safe — a
    // component with no separator cannot traverse — but it produces names like
    // `..-..-etc-passwd` that read as an attempted escape to anyone auditing
    // the directory, and hidden files that `ls` does not show.
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        return "-".to_string();
    }
    trimmed.to_string()
}

/// Steps to create for a job, in order.
pub fn step_rows_for(plan: &JobPlan, job_id: &str) -> Vec<(String, i32, String, Option<String>)> {
    plan.steps
        .iter()
        .enumerate()
        .map(|(i, s)| (step_id(job_id, i), i as i32, s.label(i), s.uses.clone()))
        .collect()
}

#[derive(Debug)]
pub enum StoreError {
    Connect(String),
    Migrations { path: PathBuf, reason: String },
    LogDir { path: PathBuf, reason: String },
    Sql(String),
}

impl StoreError {
    fn sql(e: sqlx::Error) -> Self {
        Self::Sql(e.to_string())
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e) => write!(
                f,
                "could not connect to Postgres: {e}. Check CI_DATABASE_URL."
            ),
            Self::Migrations { path, reason } => {
                write!(f, "migration {} failed: {reason}", path.display())
            }
            Self::LogDir { path, reason } => {
                write!(f, "could not write logs under {}: {reason}", path.display())
            }
            Self::Sql(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_round_trip_through_their_wire_names() {
        assert_eq!(RunStatus::Success.as_str(), "success");
        assert!(RunStatus::Failure.is_terminal());
        assert!(!RunStatus::Running.is_terminal());
        assert!(JobStatus::Skipped.is_terminal());
        assert!(!JobStatus::Queued.is_terminal());
        assert_eq!(StepStatus::Running.as_str(), "running");
    }

    /// A skipped dependency is not a failure — a downstream `if:` may want to
    /// run anyway, and GitHub reports it as `skipped`.
    #[test]
    fn a_skipped_job_reports_skipped_not_failure() {
        assert_eq!(JobStatus::Skipped.result_name(), "skipped");
        assert_eq!(JobStatus::Failure.result_name(), "failure");
        assert_eq!(JobStatus::Running.result_name(), "pending");
    }

    /// `needs.build.result` must mean "did build pass", so one failed matrix
    /// cell has to poison the whole base id.
    #[test]
    fn rolling_matrix_cells_up_takes_the_worst_result() {
        assert_eq!(worse_of("success", "failure"), "failure");
        assert_eq!(worse_of("failure", "success"), "failure");
        assert_eq!(worse_of("success", "skipped"), "skipped");
        assert_eq!(worse_of("skipped", "success"), "skipped");
        assert_eq!(worse_of("success", "running"), "running");
        assert_eq!(worse_of("cancelled", "failure"), "failure");
        assert_eq!(worse_of("success", "success"), "success");
    }

    /// Ids are derived rather than minted so a redelivery addresses the same
    /// rows, and a step id doubles as the daemon's idempotent `operationId`.
    #[test]
    fn ids_are_derived_and_therefore_stable() {
        let run = "019f7c7ef325-00000000";
        let j = job_id(run, "build-x86_64");
        assert_eq!(j, "019f7c7ef325-00000000.build-x86_64");
        assert_eq!(job_id(run, "build-x86_64"), j, "deriving twice agrees");

        let s = step_id(&j, 2);
        assert_eq!(s, "019f7c7ef325-00000000.build-x86_64.2");
        assert!(
            crate::vm::valid_operation_id(&s),
            "a step id must be usable as an operationId: {s}"
        );
    }

    // ---- integration ----------------------------------------------------
    //
    // Need a Postgres. Run with:
    //   CI_TEST_DATABASE_URL=postgres://user:pass@127.0.0.1:5432/ci_test \
    //     cargo test -- --ignored store::
    //
    // Each test uses its own run id, so they are safe to run against a database
    // that already has rows in it and safe to run concurrently.

    async fn test_store() -> Store {
        let url = std::env::var("CI_TEST_DATABASE_URL").expect("CI_TEST_DATABASE_URL");
        let dir = std::env::temp_dir().join(format!("ci-test-logs-{}", crate::vm::new_id()));
        let store = Store::connect(&url, dir).await.expect("connects");
        store
            .migrate(Path::new("migrations"))
            .await
            .expect("migrations apply");
        store
    }

    fn test_plan() -> Plan {
        let wf = crate::workflow::Workflow::parse(
            "wf.yml",
            r#"
name: test
jobs:
  build:
    vm: { driver: firecracker }
    strategy:
      matrix:
        target: [x86_64, aarch64]
    steps:
      - name: Compile
        run: "true"
      - name: Test
        run: "true"
  deploy:
    needs: [build]
    vm: { driver: firecracker }
    steps: [{ name: Ship, run: "true" }]
"#,
        )
        .expect("workflow parses");
        Plan::build(&wf).expect("plan builds")
    }

    /// Re-running every migration on every startup is the whole scheme, so it
    /// has to actually be idempotent rather than merely intended to be.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn migrations_are_idempotent() {
        let store = test_store().await;
        for _ in 0..3 {
            store
                .migrate(Path::new("migrations"))
                .await
                .expect("re-applying migrations is a no-op");
        }
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_run_and_its_jobs_are_created_together() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();

        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .expect("run created");

        let run = store.get_run(&run_id).await.unwrap().expect("run exists");
        assert_eq!(run.status, "queued");
        assert_eq!(run.workflow_name.as_deref(), Some("test"));

        let jobs = store.jobs_of(&run_id).await.unwrap();
        assert_eq!(jobs.len(), 3, "two matrix cells plus deploy");
        assert!(jobs.iter().all(|j| j.status == "pending"));
        let keys: Vec<&str> = jobs.iter().map(|j| j.job_key.as_str()).collect();
        assert!(keys.contains(&"build-x86_64"), "{keys:?}");
        assert!(keys.contains(&"deploy"), "{keys:?}");
    }

    /// The guard that makes a JetStream redelivery safe: work that already
    /// finished must not be restarted by a second delivery.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn starting_a_finished_job_is_refused() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();
        let jid = job_id(&run_id, "deploy");

        assert!(
            store
                .start_job(&jid, "hd-1", "sb-1", "fp", 1)
                .await
                .unwrap(),
            "a pending job starts"
        );
        store
            .set_job_status(&jid, JobStatus::Success, None)
            .await
            .unwrap();
        assert!(
            !store
                .start_job(&jid, "hd-1", "sb-2", "fp", 2)
                .await
                .unwrap(),
            "a finished job must not be restarted by a redelivery"
        );

        let job = store.get_job(&jid).await.unwrap().unwrap();
        assert_eq!(job.status, "success");
        assert_eq!(job.sandbox_id.as_deref(), Some("sb-1"), "not overwritten");
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_run_rolls_up_from_its_jobs() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();

        // Nothing finished yet.
        assert_eq!(
            store.roll_up_run(&run_id).await.unwrap(),
            RunStatus::Running
        );

        for key in ["build-x86_64", "build-aarch64", "deploy"] {
            store
                .set_job_status(&job_id(&run_id, key), JobStatus::Success, None)
                .await
                .unwrap();
        }
        assert_eq!(
            store.roll_up_run(&run_id).await.unwrap(),
            RunStatus::Success
        );
        let run = store.get_run(&run_id).await.unwrap().unwrap();
        assert!(
            run.finished_at.is_some(),
            "a terminal run gets a finish time"
        );

        // One failure poisons the run even though the rest passed.
        store
            .set_job_status(&job_id(&run_id, "deploy"), JobStatus::Failure, Some("boom"))
            .await
            .unwrap();
        assert_eq!(
            store.roll_up_run(&run_id).await.unwrap(),
            RunStatus::Failure
        );
    }

    /// A skipped job must not make the run fail — that is how a conditional
    /// deploy job behaves on a branch that does not deploy.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_skipped_job_still_lets_a_run_succeed() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();
        for key in ["build-x86_64", "build-aarch64"] {
            store
                .set_job_status(&job_id(&run_id, key), JobStatus::Success, None)
                .await
                .unwrap();
        }
        store
            .set_job_status(&job_id(&run_id, "deploy"), JobStatus::Skipped, None)
            .await
            .unwrap();
        assert_eq!(
            store.roll_up_run(&run_id).await.unwrap(),
            RunStatus::Success
        );
    }

    /// `needs.build.result` means "did build pass". One failed cell has to make
    /// it `failure`, or a deploy job runs off a broken build.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn needs_context_collapses_matrix_cells_to_the_worst_result() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();

        store
            .set_job_status(&job_id(&run_id, "build-x86_64"), JobStatus::Success, None)
            .await
            .unwrap();
        store
            .set_job_outputs(
                &job_id(&run_id, "build-x86_64"),
                &serde_json::json!({"sha": "abc123"}),
            )
            .await
            .unwrap();
        store
            .set_job_status(&job_id(&run_id, "build-aarch64"), JobStatus::Failure, None)
            .await
            .unwrap();

        let needs = store.needs_context(&run_id).await.unwrap();
        assert_eq!(needs["build"]["result"], "failure", "{needs}");
        assert_eq!(needs["build"]["outputs"]["sha"], "abc123");

        // And that context makes the guard on a dependent job evaluate false.
        let mut ctx = crate::expr::Context::new();
        ctx.set("needs", needs);
        assert!(
            !ctx.eval_condition("needs.build.result == 'success'")
                .unwrap()
        );
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn steps_record_their_outcome_and_their_logs_land_on_disk() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();

        let jid = job_id(&run_id, "build-x86_64");
        let cell = plan.jobs.iter().find(|j| j.key == "build-x86_64").unwrap();
        for (sid, idx, name, uses) in step_rows_for(cell, &jid) {
            store
                .create_step(&sid, &jid, idx, &name, uses.as_deref())
                .await
                .unwrap();
        }

        let steps = store.steps_of(&jid).await.unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].name, "Compile");
        assert_eq!(steps[1].name, "Test");

        let sid = &steps[0].id;
        store.start_step(sid, sid).await.unwrap();
        let path = store.log_path(&run_id, "build-x86_64", 0, sid);
        store.append_log(sid, &path, "compiling\n").await.unwrap();
        store.append_log(sid, &path, "done\n").await.unwrap();
        store
            .finish_step(sid, StepStatus::Success, Some(0), None)
            .await
            .unwrap();

        let steps = store.steps_of(&jid).await.unwrap();
        assert_eq!(steps[0].status, "success");
        assert_eq!(steps[0].exit_code, Some(0));
        assert_eq!(steps[0].log_bytes, 15, "both appends counted");
        assert_eq!(
            store.read_log(&steps[0]).await.as_deref(),
            Some("compiling\ndone\n")
        );

        tokio::fs::remove_dir_all(path.parent().unwrap().parent().unwrap())
            .await
            .ok();
    }

    /// Creating steps must be safe to repeat — a redelivered job re-creates its
    /// step rows before running anything.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn creating_the_same_step_twice_is_a_no_op() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();
        let jid = job_id(&run_id, "deploy");
        for _ in 0..3 {
            store
                .create_step(&step_id(&jid, 0), &jid, 0, "Ship", None)
                .await
                .expect("repeatable");
        }
        assert_eq!(store.steps_of(&jid).await.unwrap().len(), 1);
    }

    // ---- registered repositories ----------------------------------------

    /// A URL unique to this test run, so the tests are safe to run against a
    /// database that already has registrations and safe to run concurrently.
    fn test_repo_url() -> String {
        format!("git@github.com:test/{}.git", crate::vm::new_id())
    }

    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_token_authenticates_for_its_own_repository_and_nothing_else() {
        let store = test_store().await;
        let a = store
            .register_repo(&test_repo_url(), "a", None, None, None)
            .await
            .expect("registers");
        let b = store
            .register_repo(&test_repo_url(), "b", None, None, None)
            .await
            .expect("registers");

        let (token_row, plaintext) = store
            .create_repo_token(&a.id, "laptop", Some(("sub-1", "sam@sarocu.com")))
            .await
            .expect("mints");

        let resolved = store
            .authenticate_repo_token(&plaintext)
            .await
            .unwrap()
            .expect("the token resolves");
        assert_eq!(resolved.id, a.id);
        assert_ne!(resolved.id, b.id);

        // The plaintext is not recoverable from anything that is stored.
        let listed = store.repo_tokens(&a.id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, token_row.id);
        assert!(listed[0].last_used_at.is_some(), "use is recorded");
        assert!(!format!("{listed:?}").contains(&plaintext));

        store.delete_repo(&a.id).await.unwrap();
        store.delete_repo(&b.id).await.unwrap();
    }

    /// The one thing revocation has to actually do.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_revoked_token_stops_authenticating_but_stays_listed() {
        let store = test_store().await;
        let repo = store
            .register_repo(&test_repo_url(), "app", None, None, None)
            .await
            .unwrap();
        let (token, plaintext) = store.create_repo_token(&repo.id, "ci", None).await.unwrap();

        assert!(
            store
                .authenticate_repo_token(&plaintext)
                .await
                .unwrap()
                .is_some()
        );
        assert!(store.revoke_repo_token(&token.id).await.unwrap());
        assert!(
            store
                .authenticate_repo_token(&plaintext)
                .await
                .unwrap()
                .is_none(),
            "a revoked token must not submit"
        );
        assert!(
            !store.revoke_repo_token(&token.id).await.unwrap(),
            "revoking twice must not rewrite when access ended"
        );

        let listed = store.repo_tokens(&repo.id).await.unwrap();
        assert_eq!(listed.len(), 1, "still nameable after it stopped working");
        assert!(!listed[0].is_active());

        store.delete_repo(&repo.id).await.unwrap();
    }

    /// Pausing a repository has to stop its existing tokens, or it is not a
    /// pause — it is a label.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_paused_repository_refuses_a_valid_token() {
        let store = test_store().await;
        let repo = store
            .register_repo(&test_repo_url(), "app", None, None, None)
            .await
            .unwrap();
        let (_, plaintext) = store.create_repo_token(&repo.id, "ci", None).await.unwrap();

        assert!(store.set_repo_enabled(&repo.id, false).await.unwrap());
        assert!(
            store
                .authenticate_repo_token(&plaintext)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.set_repo_enabled(&repo.id, true).await.unwrap());
        assert!(
            store
                .authenticate_repo_token(&plaintext)
                .await
                .unwrap()
                .is_some()
        );

        store.delete_repo(&repo.id).await.unwrap();
    }

    /// A wrong or forged token must be refused, and a malformed one must not
    /// even reach a query — this is checked on a public route.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_forged_or_malformed_token_resolves_to_nothing() {
        let store = test_store().await;
        let repo = store
            .register_repo(&test_repo_url(), "app", None, None, None)
            .await
            .unwrap();
        let (token, plaintext) = store.create_repo_token(&repo.id, "ci", None).await.unwrap();

        // The right key id with the wrong secret is the interesting forgery:
        // the lookup succeeds and only the digest comparison stops it.
        let forged = format!("{}{}.{}", crate::repos::TOKEN_PREFIX, token.id, "not-it");
        assert!(
            store
                .authenticate_repo_token(&forged)
                .await
                .unwrap()
                .is_none()
        );

        for bad in ["", "garbage", "cis_nosuchkey.secret", &plaintext[..20]] {
            assert!(
                store.authenticate_repo_token(bad).await.unwrap().is_none(),
                "{bad:?} must not authenticate"
            );
        }

        store.delete_repo(&repo.id).await.unwrap();
    }

    /// Registering a URL that is already registered in another spelling must
    /// edit that registration rather than making a second one — and must not
    /// invalidate the tokens already issued for it.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn re_registering_the_other_spelling_updates_and_keeps_tokens() {
        let store = test_store().await;
        let name = crate::vm::new_id();
        let ssh = format!("git@github.com:test/{name}.git");
        let https = format!("https://github.com/test/{name}");

        let first = store
            .register_repo(&ssh, "app", None, None, None)
            .await
            .unwrap();
        let (_, plaintext) = store
            .create_repo_token(&first.id, "ci", None)
            .await
            .unwrap();

        let second = store
            .register_repo(&https, "app (renamed)", Some("ci/*.yml"), None, None)
            .await
            .unwrap();
        assert_eq!(second.id, first.id, "one repository, not two");
        assert_eq!(second.name, "app (renamed)");
        assert_eq!(second.workflow_path.as_deref(), Some("ci/*.yml"));
        assert_eq!(second.url, https, "the newer spelling is what is shown");

        assert!(
            store
                .authenticate_repo_token(&plaintext)
                .await
                .unwrap()
                .is_some(),
            "fixing a name must not invalidate everyone's credential"
        );

        // And either spelling finds it.
        assert_eq!(
            store.repo_by_url(&ssh).await.unwrap().map(|r| r.id),
            Some(first.id.clone())
        );

        store.delete_repo(&first.id).await.unwrap();
    }

    /// The assignment this whole column exists for: a repository points at a
    /// network, and reassigning it is one write rather than a re-registration
    /// that also rewrites the name and the workflow path.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_repository_can_be_pointed_at_a_network_and_back_at_the_default() {
        let store = test_store().await;
        let repo = store
            .register_repo(&test_repo_url(), "app", None, Some("prod-runners"), None)
            .await
            .unwrap();
        assert_eq!(repo.network.as_deref(), Some("prod-runners"));

        assert!(store.set_repo_network(&repo.id, Some("lab")).await.unwrap());
        assert_eq!(
            store.get_repo(&repo.id).await.unwrap().unwrap().network,
            Some("lab".to_string())
        );

        // Back to the installation default, and a blank is that rather than a
        // network named "  ".
        assert!(store.set_repo_network(&repo.id, Some("  ")).await.unwrap());
        assert_eq!(
            store.get_repo(&repo.id).await.unwrap().unwrap().network,
            None
        );

        assert!(
            !store
                .set_repo_network("no-such-repo", Some("lab"))
                .await
                .unwrap(),
            "assigning a network to nothing must report that it did nothing"
        );

        store.delete_repo(&repo.id).await.unwrap();
    }

    /// Removing a registration must not delete the history of what it built.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn deleting_a_repository_keeps_its_runs() {
        let store = test_store().await;
        let repo = store
            .register_repo(&test_repo_url(), "app", None, None, None)
            .await
            .unwrap();
        let run_id = crate::vm::new_id();
        store
            .create_run(
                &run_id,
                &RunRequest {
                    repo_id: Some(repo.id.clone()),
                    repo_url: repo.url.clone(),
                    ..RunRequest::default()
                },
                &test_plan(),
            )
            .await
            .unwrap();

        assert_eq!(
            store
                .last_run_of_repo(&repo.id)
                .await
                .unwrap()
                .map(|r| r.id),
            Some(run_id.clone())
        );

        assert!(store.delete_repo(&repo.id).await.unwrap());
        assert!(
            store.get_run(&run_id).await.unwrap().is_some(),
            "the run survives its registration"
        );
    }

    /// The repair for a publish that failed after the status was committed:
    /// the job goes back on the runway, and the scheduler finds the run again.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_failed_publish_returns_the_job_to_pending_and_the_run_is_found() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();
        let job = job_id(&run_id, "build-x86_64");

        assert!(store.queue_job(&job).await.unwrap());
        assert!(store.unqueue_job(&job).await.unwrap(), "rolled back");

        // Back to pending, with the queue clock cleared — otherwise the
        // runner-wait watchdog would count time it never spent queued.
        let row = store.get_job(&job).await.unwrap().unwrap();
        assert_eq!(row.status, "pending");
        assert!(
            store
                .jobs_waiting_longer_than(Duration::from_secs(0), 50)
                .await
                .unwrap()
                .iter()
                .all(|j| j.id != job),
            "a rolled-back job is not waiting on a runner"
        );

        // And the run is offered to the scheduler again, which is what stops it
        // sitting pending for ever.
        assert!(
            store
                .runs_with_pending_jobs(500)
                .await
                .unwrap()
                .contains(&run_id)
        );

        // Re-queueing works, and rolling back a job that has since started does
        // not: it would take a running job off a runner.
        assert!(store.queue_job(&job).await.unwrap());
        store
            .start_job(&job, "hd-1", "sb-1", "fp", 1)
            .await
            .unwrap();
        assert!(
            !store.unqueue_job(&job).await.unwrap(),
            "a started job must never be rolled back"
        );
        assert_eq!(
            store.get_job(&job).await.unwrap().unwrap().status,
            "running"
        );
    }

    /// A job pinned to a host that never comes online must become findable, or
    /// it waits for ever with no steps and no error — which is what
    /// `CI_RUNNER_WAIT_SECS` was always documented to prevent and never did.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn a_job_waiting_on_a_runner_becomes_visible_after_the_wait() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();
        let job = job_id(&run_id, "build-x86_64");

        // Pending, not queued: a job waiting on `needs:` is not waiting on a
        // runner, and failing it would kill work that has not been offered yet.
        assert!(
            store
                .jobs_waiting_longer_than(Duration::from_secs(0), 50)
                .await
                .unwrap()
                .iter()
                .all(|j| j.id != job),
            "a pending job is not waiting on a runner"
        );

        assert!(store.queue_job(&job).await.unwrap());
        // Long enough that nothing legitimately queued qualifies.
        assert!(
            store
                .jobs_waiting_longer_than(Duration::from_secs(3600), 50)
                .await
                .unwrap()
                .iter()
                .all(|j| j.id != job),
            "a job queued a moment ago is not stuck"
        );

        let stuck = store
            .jobs_waiting_longer_than(Duration::from_secs(0), 50)
            .await
            .unwrap();
        assert!(stuck.iter().any(|j| j.id == job), "queued past the wait");

        // Once it starts, or the run ends, it stops being offered.
        store
            .start_job(&job, "hd-1", "sb-1", "fp", 1)
            .await
            .unwrap();
        assert!(
            store
                .jobs_waiting_longer_than(Duration::from_secs(0), 50)
                .await
                .unwrap()
                .iter()
                .all(|j| j.id != job),
            "a running job is not waiting"
        );
    }

    /// Cancelling has to stop work in all three states a job might be in, and
    /// leave finished work alone.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn cancelling_stops_unfinished_jobs_and_keeps_the_rest() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();

        // One already finished, one running, one still pending.
        let done = job_id(&run_id, "build-x86_64");
        let running = job_id(&run_id, "build-aarch64");
        store
            .set_job_status(&done, JobStatus::Success, None)
            .await
            .unwrap();
        store
            .start_job(&running, "hd-1", "sb-1", "fp", 1)
            .await
            .unwrap();

        let cancelled = store.cancel_run(&run_id).await.unwrap();
        assert_eq!(cancelled, Some(2), "the running and the pending one");

        let by_key: std::collections::HashMap<_, _> = store
            .jobs_of(&run_id)
            .await
            .unwrap()
            .into_iter()
            .map(|j| (j.job_key.clone(), j))
            .collect();
        assert_eq!(
            by_key["build-x86_64"].status, "success",
            "work that finished is not rewritten"
        );
        assert_eq!(by_key["build-aarch64"].status, "cancelled");
        assert_eq!(by_key["deploy"].status, "cancelled");
        assert_eq!(
            store.get_run(&run_id).await.unwrap().unwrap().status,
            "cancelled"
        );

        // What the executor polls between steps.
        assert!(store.is_job_cancelled(&running).await.unwrap());
        assert!(!store.is_job_cancelled(&done).await.unwrap());

        // A redelivery of a cancelled job must not restart it.
        assert!(
            !store
                .start_job(&running, "hd-1", "sb-2", "fp", 2)
                .await
                .unwrap(),
            "a cancelled job must refuse to start"
        );

        // And cancelling twice reports that there was nothing left to stop.
        assert_eq!(store.cancel_run(&run_id).await.unwrap(), None);
    }

    // ---- log retention ---------------------------------------------------

    /// The sweep has to be driven by rows, converge, and leave the run's
    /// history behind — losing the fact that a step ran, along with its bytes,
    /// would make an old run look as though it never happened.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn sweeping_discards_log_bytes_and_keeps_the_history() {
        let store = test_store().await;
        let plan = test_plan();
        let run_id = crate::vm::new_id();
        store
            .create_run(&run_id, &RunRequest::default(), &plan)
            .await
            .unwrap();

        let jid = job_id(&run_id, "deploy");
        let sid = step_id(&jid, 0);
        store
            .create_step(&sid, &jid, 0, "Ship", None)
            .await
            .unwrap();
        let path = store.log_path(&run_id, "deploy", 0, &sid);
        store.append_log(&sid, &path, "output\n").await.unwrap();
        store
            .finish_step(&sid, StepStatus::Success, Some(0), None)
            .await
            .unwrap();
        assert!(path.exists());

        // Not yet old enough: a sweep must not take a run that is still inside
        // the retention window.
        let recent = store
            .runs_with_logs_before(Utc::now() - chrono::Duration::days(1), 100)
            .await
            .unwrap();
        assert!(
            !recent.contains(&run_id),
            "swept a run inside its retention"
        );

        // Old enough now.
        let due = store
            .runs_with_logs_before(Utc::now() + chrono::Duration::days(1), 500)
            .await
            .unwrap();
        assert!(
            due.contains(&run_id),
            "a run past retention must be offered"
        );

        tokio::fs::remove_dir_all(store.run_log_dir(&run_id))
            .await
            .ok();
        assert_eq!(store.forget_logs_of(&run_id).await.unwrap(), 1);

        // The row survives with its result; only the pointer and size go.
        let steps = store.steps_of(&jid).await.unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, "success");
        assert_eq!(steps[0].exit_code, Some(0));
        assert_eq!(steps[0].log_path, None);
        assert_eq!(steps[0].log_bytes, 0);
        assert_eq!(store.read_log(&steps[0]).await, None);
        assert!(store.get_run(&run_id).await.unwrap().is_some());

        // And it converges: a swept run is not offered again, or the sweeper
        // would rescan the same rows every hour forever.
        let again = store
            .runs_with_logs_before(Utc::now() + chrono::Duration::days(1), 500)
            .await
            .unwrap();
        assert!(!again.contains(&run_id), "the sweep must converge");
    }

    /// Being on the admin list promotes, but dropping off it must not demote —
    /// a role granted in the UI has to survive the env var changing.
    #[tokio::test]
    #[ignore = "needs CI_TEST_DATABASE_URL"]
    async fn admin_seeding_promotes_but_never_demotes() {
        let store = test_store().await;
        let subject = format!("sub-{}", crate::vm::new_id());
        let admins = vec!["boss@example.com".to_string()];

        let role = store
            .upsert_user(&subject, "nobody@example.com", None, &admins)
            .await
            .unwrap();
        assert_eq!(role, "viewer");

        let role = store
            .upsert_user(&subject, "boss@example.com", Some("Boss"), &admins)
            .await
            .unwrap();
        assert_eq!(role, "admin", "the config list promotes");

        let role = store
            .upsert_user(&subject, "boss@example.com", Some("Boss"), &[])
            .await
            .unwrap();
        assert_eq!(role, "admin", "an empty list must not demote");
    }

    /// A log path is assembled from values that arrive over HTTP; one `..`
    /// would write outside CI_LOG_DIR.
    #[test]
    fn log_path_components_cannot_escape_the_log_directory() {
        assert_eq!(sanitize_component("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_component(".."), "-");
        assert_eq!(sanitize_component("."), "-");
        assert_eq!(sanitize_component(""), "-");
        assert_eq!(sanitize_component("/"), "-");
        // Ids and job keys survive intact — they are already in the alphabet.
        assert_eq!(sanitize_component("build-x86_64"), "build-x86_64");
        assert_eq!(
            sanitize_component("019f7c7ef325-00000000"),
            "019f7c7ef325-00000000"
        );

        let store_dir = PathBuf::from("/var/lib/ci-logs");
        let joined = store_dir
            .join(sanitize_component("../.."))
            .join(sanitize_component("x"));
        assert!(
            joined.starts_with("/var/lib/ci-logs"),
            "escaped: {}",
            joined.display()
        );
    }
}
