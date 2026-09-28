-- Part of the existing regional operation, not a second application dispatcher.
CREATE TABLE IF NOT EXISTS regional_lifecycle_barriers (
    operation_id TEXT NOT NULL REFERENCES regional_service_rollouts(operation_id),
    step_id TEXT NOT NULL,
    contract JSONB NOT NULL,
    commands JSONB NOT NULL,
    receipts JSONB NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY(operation_id, step_id)
);
