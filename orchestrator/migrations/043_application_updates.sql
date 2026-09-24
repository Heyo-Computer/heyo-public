CREATE TABLE IF NOT EXISTS application_updates (
    operation_id TEXT PRIMARY KEY,
    service_id TEXT NOT NULL REFERENCES external_service_bindings(service_id),
    intent_hash TEXT NOT NULL,
    intent JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'accepted' CHECK (status IN ('accepted','running','passed','failed')),
    observation JSONB,
    observed_at TIMESTAMPTZ,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS application_updates_active
    ON application_updates(service_id) WHERE status IN ('accepted','running');
