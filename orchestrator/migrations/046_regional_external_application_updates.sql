-- Regional retained-workspace release parents. Migrations are replayed on every
-- startup, so every operation in this file is intentionally idempotent.
ALTER TABLE application_updates DROP CONSTRAINT IF EXISTS application_updates_service_id_fkey;
DO $$ BEGIN
    IF (SELECT array_agg(a.attname ORDER BY k.ordinality)
        FROM pg_constraint c
        CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY k(attnum, ordinality)
        JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=k.attnum
        WHERE c.conrelid='external_service_bindings'::regclass AND c.contype='p')
        = ARRAY['service_id']::name[] THEN
        ALTER TABLE external_service_bindings DROP CONSTRAINT external_service_bindings_pkey;
        ALTER TABLE external_service_bindings ADD PRIMARY KEY (service_id, region);
    END IF;
END $$;
CREATE UNIQUE INDEX IF NOT EXISTS external_service_bindings_service_deployment_unique
    ON external_service_bindings(service_id, deployment_id);

CREATE TABLE IF NOT EXISTS regional_application_updates (
    operation_id TEXT PRIMARY KEY,
    service_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    request JSONB NOT NULL,
    release_run_id TEXT NOT NULL,
    target_revision TEXT NOT NULL,
    artifact_digest TEXT NOT NULL,
    binary_sha256 TEXT NOT NULL,
    bake_seconds INTEGER NOT NULL CHECK (bake_seconds >= 0 AND bake_seconds <= 86400),
    targets JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'preparing' CHECK (status IN ('preparing','running','cancelling','passed','failed','cancelled')),
    current_target INTEGER,
    stop_requested BOOLEAN NOT NULL DEFAULT false,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS regional_application_updates_active
    ON regional_application_updates(service_id) WHERE status IN ('preparing','running','cancelling');

CREATE TABLE IF NOT EXISTS regional_application_update_targets (
    parent_operation_id TEXT NOT NULL REFERENCES regional_application_updates(operation_id),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    service_id TEXT NOT NULL,
    region TEXT NOT NULL,
    deployment_id TEXT NOT NULL,
    authority TEXT NOT NULL,
    health_origin TEXT NOT NULL,
    namespace TEXT NOT NULL,
    adoption_evidence JSONB NOT NULL,
    child_operation_id TEXT NOT NULL,
    intent_hash TEXT,
    status TEXT NOT NULL DEFAULT 'frozen' CHECK (status IN ('frozen','preparing','prepared','activating','baking','passed','failed','cancelling','cancelled')),
    activated BOOLEAN NOT NULL DEFAULT false,
    observation JSONB,
    observed_at TIMESTAMPTZ,
    bake_started_at TIMESTAMPTZ,
    error TEXT,
    PRIMARY KEY(parent_operation_id, ordinal),
    UNIQUE(parent_operation_id, region),
    UNIQUE(parent_operation_id, child_operation_id),
    FOREIGN KEY(service_id, region) REFERENCES external_service_bindings(service_id, region)
);
