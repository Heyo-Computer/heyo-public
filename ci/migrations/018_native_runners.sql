-- Durable native runner registration and job leases. Lease tokens fence stale
-- agents: only the current token may heartbeat or complete a job.
CREATE TABLE IF NOT EXISTS ci_native_runner (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    labels TEXT[] NOT NULL,
    platform TEXT NOT NULL CHECK (platform IN ('macos','windows')),
    arch TEXT NOT NULL CHECK (arch = 'x86_64'),
    max_concurrent INTEGER NOT NULL DEFAULT 1 CHECK (max_concurrent > 0),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS ci_native_job (
    job_id TEXT PRIMARY KEY REFERENCES ci_job(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL REFERENCES ci_run(id) ON DELETE CASCADE,
    required_labels TEXT[] NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','leased','completed','cancelled')),
    runner_id TEXT REFERENCES ci_native_runner(id) ON DELETE SET NULL,
    lease_token UUID,
    lease_expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    completion_hash TEXT,
    advancement_pending BOOLEAN NOT NULL DEFAULT false
);
ALTER TABLE ci_native_job ADD COLUMN IF NOT EXISTS completion_hash TEXT;
ALTER TABLE ci_native_job ADD COLUMN IF NOT EXISTS advancement_pending BOOLEAN NOT NULL DEFAULT false;
CREATE INDEX IF NOT EXISTS ci_native_job_queue_idx ON ci_native_job (state, created_at);
CREATE INDEX IF NOT EXISTS ci_native_job_runner_idx ON ci_native_job (runner_id, state);
CREATE INDEX IF NOT EXISTS ci_native_job_advancement_idx ON ci_native_job (advancement_pending) WHERE advancement_pending;
