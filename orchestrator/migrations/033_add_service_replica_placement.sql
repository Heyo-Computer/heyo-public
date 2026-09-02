ALTER TABLE service_discovery_endpoints
    ADD COLUMN IF NOT EXISTS region TEXT;

ALTER TABLE service_deployment_states
    ADD COLUMN IF NOT EXISTS replica_regions JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN IF NOT EXISTS ingress_backend_url TEXT;
