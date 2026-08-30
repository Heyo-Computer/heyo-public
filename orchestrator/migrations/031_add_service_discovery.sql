CREATE TABLE IF NOT EXISTS service_discovery_sets (
    service_id TEXT PRIMARY KEY,
    version BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS service_discovery_endpoints (
    service_id TEXT NOT NULL REFERENCES service_discovery_sets(service_id) ON DELETE CASCADE,
    deployment_id TEXT NOT NULL,
    backend_server_id TEXT,
    backend_url TEXT NOT NULL,
    health_status TEXT NOT NULL DEFAULT 'unknown'
        CHECK (health_status IN ('healthy', 'unhealthy', 'unknown')),
    draining BOOLEAN NOT NULL DEFAULT FALSE,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (service_id, deployment_id)
);

CREATE INDEX IF NOT EXISTS service_discovery_endpoints_backend_idx
    ON service_discovery_endpoints (backend_server_id)
    WHERE backend_server_id IS NOT NULL;
