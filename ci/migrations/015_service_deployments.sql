-- CI owns the operation ledger, not the VM lifecycle. A submitting row is
-- committed BEFORE sending POST to orchestrator. Never automatically POST
-- again after that boundary: orchestrator does not deduplicate submissions.
CREATE TABLE IF NOT EXISTS ci_service_deployment (
    id TEXT PRIMARY KEY,
    step_id TEXT NOT NULL UNIQUE REFERENCES ci_step(id),
    run_id TEXT NOT NULL REFERENCES ci_run(id),
    job_id TEXT NOT NULL REFERENCES ci_job(id),
    service_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('submitting', 'submission_unknown', 'running', 'passed', 'failed')),
    phase TEXT,
    message TEXT,
    error TEXT,
    sha TEXT NOT NULL,
    git_ref TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS ci_service_deployment_run_idx ON ci_service_deployment(run_id, created_at);
