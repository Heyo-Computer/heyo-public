-- Explicit executor handoff, not an inference from cancellation or lease age.
CREATE TABLE IF NOT EXISTS ci_vm_cleanup (
    sandbox_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL REFERENCES ci_job(id),
    runner_hd_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    destroy BOOLEAN NOT NULL,
    last_error TEXT,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
