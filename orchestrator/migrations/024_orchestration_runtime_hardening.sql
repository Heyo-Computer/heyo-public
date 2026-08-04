ALTER TABLE orchestration_step_runs
    ADD COLUMN IF NOT EXISTS external_ref TEXT;

CREATE INDEX IF NOT EXISTS idx_orch_step_runs_external_ref
    ON orchestration_step_runs(external_ref)
    WHERE external_ref IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_orch_approvals_pending_step_run_id
    ON orchestration_approvals(step_run_id)
    WHERE status = 'pending';
