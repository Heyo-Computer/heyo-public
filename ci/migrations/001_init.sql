-- Run history, the job DAG, step results and the VM pool.
--
-- Every statement must be idempotent. Migrations are applied by re-executing
-- the whole directory on each startup (see `store::migrate`), which is the
-- convention heyosecret already uses: no tracking table, no version column, so
-- `IF NOT EXISTS` everywhere is what makes a second start a no-op rather than
-- an error.
--
-- Ids are the `<epoch_ms:012x>-<seq:08x>` strings minted by `vm::new_id`, not
-- UUIDs. They sort chronologically, they are valid NATS subject tokens, and they
-- are directly usable as the daemon's `operationId` — so a step's id *is* the
-- handle that makes its exec idempotent across a queue redelivery.

CREATE TABLE IF NOT EXISTS ci_run (
    id              TEXT PRIMARY KEY,
    workflow_id     TEXT        NOT NULL,
    workflow_path   TEXT        NOT NULL,
    workflow_name   TEXT,
    repo_url        TEXT        NOT NULL DEFAULT '',
    git_ref         TEXT        NOT NULL DEFAULT '',
    sha             TEXT        NOT NULL DEFAULT '',
    before_sha      TEXT        NOT NULL DEFAULT '',
    -- app-lb's stable Google `sub`; the email is for display and may change.
    actor_subject   TEXT,
    actor_email     TEXT,
    source          TEXT        NOT NULL DEFAULT 'submit',
    status          TEXT        NOT NULL
        CHECK (status IN ('queued','running','success','failure','cancelled')),
    error           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS ci_run_created_idx  ON ci_run (created_at DESC);
CREATE INDEX IF NOT EXISTS ci_run_workflow_idx ON ci_run (workflow_id, created_at DESC);

CREATE TABLE IF NOT EXISTS ci_job (
    id              TEXT PRIMARY KEY,
    run_id          TEXT        NOT NULL REFERENCES ci_run(id) ON DELETE CASCADE,
    -- `job_key` is the matrix-expanded, subject-safe id; `base_id` is what
    -- `needs:` names. Both are required: dispatch routes on the key, dependency
    -- resolution reads the base.
    job_key         TEXT        NOT NULL,
    base_id         TEXT        NOT NULL,
    display         TEXT        NOT NULL,
    network         TEXT,
    runner_hd_id    TEXT,
    fingerprint     TEXT,
    sandbox_id      TEXT,
    status          TEXT        NOT NULL
        CHECK (status IN ('pending','queued','running','success','failure','skipped','cancelled')),
    -- JetStream delivery attempt, 1-based. A redelivery increments it, which is
    -- how a log can say "this is a retry" rather than looking like a rerun.
    attempt         INTEGER     NOT NULL DEFAULT 1,
    matrix          JSONB       NOT NULL DEFAULT '{}'::jsonb,
    outputs         JSONB       NOT NULL DEFAULT '{}'::jsonb,
    error           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ,
    UNIQUE (run_id, job_key)
);

CREATE INDEX IF NOT EXISTS ci_job_run_idx    ON ci_job (run_id);
CREATE INDEX IF NOT EXISTS ci_job_runner_idx ON ci_job (runner_hd_id, status);

-- The expanded `JobPlan` this job runs: steps, vm spec, matrix, target.
--
-- Held here rather than in the queue message so a NATS message is only a
-- doorbell. The database is the source of truth, which means a redelivery runs
-- exactly what the original delivery would have, even if the workflow file
-- changed on the branch in between.
--
-- `ADD COLUMN IF NOT EXISTS` rather than editing the CREATE above: the CREATE is
-- a no-op once the table exists, so an additive change has to be its own
-- idempotent statement. This is the pattern for every future column.
ALTER TABLE ci_job ADD COLUMN IF NOT EXISTS plan JSONB NOT NULL DEFAULT '{}'::jsonb;

CREATE TABLE IF NOT EXISTS ci_step (
    id              TEXT PRIMARY KEY,
    job_id          TEXT        NOT NULL REFERENCES ci_job(id) ON DELETE CASCADE,
    idx             INTEGER     NOT NULL,
    name            TEXT        NOT NULL,
    uses            TEXT,
    status          TEXT        NOT NULL
        CHECK (status IN ('pending','running','success','failure','skipped')),
    exit_code       INTEGER,
    -- The daemon's `operationId`. Kept so a redelivered job can reattach to an
    -- operation still running rather than starting the step a second time.
    operation_id    TEXT,
    -- Log bytes live on disk; this row holds only where and how much. A
    -- multi-megabyte build log in a column would make every listing query drag
    -- it along.
    log_path        TEXT,
    log_bytes       BIGINT      NOT NULL DEFAULT 0,
    error           TEXT,
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ,
    UNIQUE (job_id, idx)
);

CREATE INDEX IF NOT EXISTS ci_step_job_idx ON ci_step (job_id, idx);

CREATE TABLE IF NOT EXISTS ci_artifact (
    id              TEXT PRIMARY KEY,
    run_id          TEXT        NOT NULL REFERENCES ci_run(id) ON DELETE CASCADE,
    job_id          TEXT        REFERENCES ci_job(id) ON DELETE SET NULL,
    name            TEXT        NOT NULL,
    sink            TEXT        NOT NULL,
    -- sha256 for the content-addressed sink; null for disk and S3.
    digest          TEXT,
    size_bytes      BIGINT      NOT NULL DEFAULT 0,
    uri             TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ci_artifact_run_idx ON ci_artifact (run_id);

-- The warm VM pool, one row per sandbox this orchestrator created.
--
-- Survives a restart on purpose: without it a crash orphans every VM until its
-- TTL expires, and the next run rebuilds a pool that is already sitting there.
CREATE TABLE IF NOT EXISTS ci_vm_pool (
    sandbox_id      TEXT PRIMARY KEY,
    runner_hd_id    TEXT        NOT NULL,
    fingerprint     TEXT        NOT NULL,
    workflow_id     TEXT        NOT NULL DEFAULT '',
    status          TEXT        NOT NULL
        CHECK (status IN ('idle','claimed','draining')),
    claimed_by_job  TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The claim query's whole working set: idle VMs on one host with one
-- fingerprint.
CREATE INDEX IF NOT EXISTS ci_vm_pool_claim_idx
    ON ci_vm_pool (runner_hd_id, fingerprint, status);

-- app-lb has no roles, so this app keeps its own. Keyed on the stable Google
-- `sub` rather than the email, which can change under the same person.
CREATE TABLE IF NOT EXISTS ci_user (
    subject         TEXT PRIMARY KEY,
    email           TEXT        NOT NULL,
    name            TEXT,
    role            TEXT        NOT NULL DEFAULT 'viewer'
        CHECK (role IN ('viewer','admin')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
