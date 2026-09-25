-- Reports outlive VM cleanup and log retention. No cascading run foreign key:
-- an S3 outage must not lose the report or keep a VM alive.
CREATE TABLE IF NOT EXISTS ci_debug_report (
    job_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    sandbox_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    job_key TEXT NOT NULL,
    payload JSONB,
    uploaded_at TIMESTAMPTZ,
    s3_uri TEXT,
    last_error TEXT,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(job_id, attempt, sandbox_id)
);
ALTER TABLE ci_debug_report ALTER COLUMN payload DROP NOT NULL;
CREATE INDEX IF NOT EXISTS ci_debug_report_run_idx ON ci_debug_report(run_id);
CREATE INDEX IF NOT EXISTS ci_debug_report_pending_idx ON ci_debug_report(next_attempt_at) WHERE uploaded_at IS NULL;
