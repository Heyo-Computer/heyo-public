ALTER TABLE service_deployment_states
    ADD COLUMN IF NOT EXISTS active_metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN IF NOT EXISTS previous_metadata JSONB;
