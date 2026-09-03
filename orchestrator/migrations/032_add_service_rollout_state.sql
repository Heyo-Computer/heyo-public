ALTER TABLE service_deployment_states
    ADD COLUMN IF NOT EXISTS desired_replicas INTEGER NOT NULL DEFAULT 1
        CHECK (desired_replicas > 0);

ALTER TABLE service_discovery_endpoints
    ADD COLUMN IF NOT EXISTS revision TEXT;

CREATE TABLE IF NOT EXISTS service_rollouts (
    service_id TEXT PRIMARY KEY,
    rollout_id TEXT NOT NULL UNIQUE,
    desired_replicas INTEGER NOT NULL CHECK (desired_replicas > 0),
    target_revision TEXT,
    status TEXT NOT NULL CHECK (status IN ('running', 'passed', 'failed')),
    stage TEXT NOT NULL,
    error_message TEXT,
    lease_expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS service_rollouts_status_lease_idx
    ON service_rollouts (status, lease_expires_at);
