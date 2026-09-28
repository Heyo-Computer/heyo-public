-- Explicit operator recovery preserves the predecessor and all effect ledgers.
CREATE TABLE IF NOT EXISTS ci_executor_recovery (
    operation_id UUID PRIMARY KEY,
    source_boot UUID NOT NULL REFERENCES ci_executor_boot(boot_id),
    source_generation BIGINT NOT NULL,
    candidate_boot UUID NOT NULL UNIQUE REFERENCES ci_executor_boot(boot_id),
    plan JSONB NOT NULL,
    evidence JSONB,
    phase TEXT NOT NULL CHECK (phase IN ('holding','reconciling','complete')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS ci_executor_recovery_active
    ON ci_executor_recovery ((TRUE)) WHERE phase <> 'complete';
