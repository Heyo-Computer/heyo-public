-- A self-update outlives the process that requested it. Credentials stay in
-- service configuration; request contains only immutable rollout coordinates.
CREATE TABLE IF NOT EXISTS ci_controller_rollout (
    id TEXT PRIMARY KEY REFERENCES ci_service_deployment(id),
    request JSONB NOT NULL,
    phase TEXT NOT NULL DEFAULT 'pending'
        CHECK (phase IN ('pending', 'draining', 'quiesced', 'submitting', 'verifying', 'complete')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The controller has one workspace and one writer. Serialize self-updates;
-- never let a second run replace the durable admission barrier's owner.
CREATE UNIQUE INDEX IF NOT EXISTS ci_controller_rollout_active
    ON ci_controller_rollout ((true)) WHERE phase <> 'complete';
