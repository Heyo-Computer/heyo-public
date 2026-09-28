-- CI observes the platform's lifecycle; it does not execute replacement itself.
-- This is not a ci_service_deployment external-effect obligation: the release
-- job must finish before Orchestrator asks its boot to drain.
CREATE TABLE IF NOT EXISTS ci_managed_update (
    operation_id TEXT PRIMARY KEY,
    step_id TEXT NOT NULL UNIQUE REFERENCES ci_step(id),
    run_id TEXT NOT NULL REFERENCES ci_run(id),
    job_id TEXT NOT NULL REFERENCES ci_job(id),
    service_id TEXT NOT NULL,
    request JSONB NOT NULL,
    targets JSONB,
    attempted BOOLEAN NOT NULL DEFAULT FALSE,
    result TEXT CHECK (result IN ('passed','failed')),
    observation JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS ci_managed_update_active
    ON ci_managed_update(service_id) WHERE result IS NULL;
