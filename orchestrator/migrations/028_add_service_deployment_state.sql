CREATE TABLE IF NOT EXISTS service_deployment_states (
    service_id TEXT PRIMARY KEY,
    active_deployment_id TEXT,
    active_archive_id TEXT,
    active_backend_url TEXT,
    previous_deployment_id TEXT,
    previous_archive_id TEXT,
    active_metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    previous_metadata JSONB,
    route JSONB,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS service_deployment_states_updated_at_idx
    ON service_deployment_states (updated_at DESC);
