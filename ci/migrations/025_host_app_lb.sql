-- Immutable exact artifact/source intent; credentials never enter the ledger.
CREATE TABLE IF NOT EXISTS ci_host_app_lb (
    id TEXT PRIMARY KEY REFERENCES ci_service_deployment(id),
    intent JSONB NOT NULL,
    deadline TIMESTAMPTZ NOT NULL
);
