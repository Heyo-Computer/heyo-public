-- Process ownership is deliberately not a lease. A missing heartbeat or an old
-- registration never authorizes another process to perform CI side effects.
CREATE TABLE IF NOT EXISTS ci_executor_boot (
    boot_id UUID PRIMARY KEY,
    deployment_id TEXT NOT NULL,
    registered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ready_at TIMESTAMPTZ,
    retired BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS ci_executor_owner (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    boot_id UUID NOT NULL REFERENCES ci_executor_boot(boot_id),
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation > 0),
    continuation_operation_id TEXT,
    transferred_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
