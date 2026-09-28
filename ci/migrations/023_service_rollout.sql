-- Store immutable conditional rollout inputs, never credentials or live secrets.
CREATE TABLE IF NOT EXISTS ci_service_rollout (
    id TEXT PRIMARY KEY REFERENCES ci_service_deployment(id),
    intent JSONB NOT NULL,
    deadline TIMESTAMPTZ NOT NULL
);
