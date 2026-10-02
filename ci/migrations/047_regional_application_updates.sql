-- Keep one generic receipt per real release step. Regional children are not
-- additional jobs or receipts. NULL preserves writes from older CI binaries.
ALTER TABLE ci_controller_rollout DROP CONSTRAINT IF EXISTS ci_controller_rollout_id_fkey;
ALTER TABLE ci_controller_rollout ADD COLUMN IF NOT EXISTS deployment_record_id TEXT REFERENCES ci_service_deployment(id);
ALTER TABLE ci_controller_rollout ADD COLUMN IF NOT EXISTS receipt_id TEXT
    GENERATED ALWAYS AS (COALESCE(deployment_record_id,id)) STORED REFERENCES ci_service_deployment(id);
ALTER TABLE ci_controller_rollout ADD COLUMN IF NOT EXISTS result TEXT CHECK (result IN ('passed','failed'));
ALTER TABLE ci_controller_rollout ADD COLUMN IF NOT EXISTS message TEXT;
ALTER TABLE ci_controller_rollout ADD COLUMN IF NOT EXISTS activated_at TIMESTAMPTZ;

CREATE TABLE IF NOT EXISTS ci_regional_update (
    id TEXT PRIMARY KEY REFERENCES ci_service_deployment(id),
    application_id TEXT NOT NULL,
    authority TEXT NOT NULL,
    request JSONB NOT NULL,
    artifact_name TEXT NOT NULL,
    workflow TEXT,
    attempted BOOLEAN NOT NULL DEFAULT FALSE,
    observation JSONB,
    result TEXT CHECK (result IN ('passed','failed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS ci_regional_update_active
    ON ci_regional_update(application_id) WHERE result IS NULL;

CREATE TABLE IF NOT EXISTS ci_regional_cancelled_child (
    id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL REFERENCES ci_regional_update(id)
);
