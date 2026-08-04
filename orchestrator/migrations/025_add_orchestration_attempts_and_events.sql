CREATE TABLE IF NOT EXISTS orchestration_step_attempts (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT NOT NULL REFERENCES orchestration_workflow_runs(id) ON DELETE CASCADE,
    step_run_id TEXT NOT NULL REFERENCES orchestration_step_runs(id) ON DELETE CASCADE,
    attempt_number INTEGER NOT NULL,
    worker_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'blocked', 'completed', 'failed', 'abandoned')),
    lease_expires_at TIMESTAMPTZ NOT NULL,
    heartbeat_at TIMESTAMPTZ,
    failure_reason TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (step_run_id, attempt_number)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_orch_step_attempts_active_step_run_id
    ON orchestration_step_attempts(step_run_id)
    WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_orch_step_attempts_status_lease
    ON orchestration_step_attempts(status, lease_expires_at);

CREATE TABLE IF NOT EXISTS orchestration_external_events (
    id TEXT PRIMARY KEY,
    workflow_run_id TEXT REFERENCES orchestration_workflow_runs(id) ON DELETE SET NULL,
    step_run_id TEXT REFERENCES orchestration_step_runs(id) ON DELETE SET NULL,
    external_ref TEXT NOT NULL,
    event_type TEXT NOT NULL,
    event_status TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    message_id TEXT,
    payload JSONB NOT NULL,
    processing_status TEXT NOT NULL CHECK (processing_status IN ('pending', 'processing', 'processed', 'ignored')),
    processing_error TEXT,
    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_orch_external_events_idempotency
    ON orchestration_external_events(external_ref, event_type, idempotency_key);

CREATE INDEX IF NOT EXISTS idx_orch_external_events_processing_status
    ON orchestration_external_events(processing_status, received_at);

CREATE INDEX IF NOT EXISTS idx_orch_external_events_external_ref
    ON orchestration_external_events(external_ref);
