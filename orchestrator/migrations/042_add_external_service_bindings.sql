CREATE TABLE IF NOT EXISTS external_service_bindings (
    service_id TEXT PRIMARY KEY,
    authority TEXT NOT NULL,
    namespace TEXT NOT NULL,
    deployment_id TEXT NOT NULL,
    region TEXT NOT NULL,
    source_rollout_revision TEXT NOT NULL,
    spec_etag TEXT NOT NULL,
    artifact_digest TEXT NOT NULL,
    application_revision TEXT NOT NULL,
    runtime_sandbox_id TEXT NOT NULL,
    runtime_port INTEGER NOT NULL CHECK (runtime_port BETWEEN 1 AND 65535),
    observed_at TIMESTAMPTZ NOT NULL,
    evidence JSONB NOT NULL,
    lifecycle_owner TEXT NOT NULL DEFAULT 'app-lb' CHECK (lifecycle_owner = 'app-lb'),
    capabilities JSONB NOT NULL DEFAULT '["observation-only"]'::jsonb,
    UNIQUE (authority, namespace, deployment_id)
);
