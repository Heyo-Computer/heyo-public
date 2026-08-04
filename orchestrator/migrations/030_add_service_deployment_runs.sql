CREATE TABLE IF NOT EXISTS service_deployment_runs (
    deployment_id TEXT PRIMARY KEY,
    service_id TEXT NOT NULL,
    status TEXT NOT NULL,
    phase TEXT NOT NULL,
    message TEXT,
    error_message TEXT,
    request JSONB,
    response JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS service_deployment_runs_service_updated_at_idx
    ON service_deployment_runs (service_id, updated_at DESC);

CREATE INDEX IF NOT EXISTS service_deployment_runs_status_updated_at_idx
    ON service_deployment_runs (status, updated_at DESC);

CREATE TABLE IF NOT EXISTS service_deployment_events (
    id BIGSERIAL PRIMARY KEY,
    deployment_id TEXT NOT NULL REFERENCES service_deployment_runs(deployment_id) ON DELETE CASCADE,
    service_id TEXT NOT NULL,
    phase TEXT NOT NULL,
    status TEXT NOT NULL,
    message TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    error_message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS service_deployment_events_deployment_created_at_idx
    ON service_deployment_events (deployment_id, created_at ASC, id ASC);
