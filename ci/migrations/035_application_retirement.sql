-- Generic managed-app retirement is independent of CI jobs/releases. A process
-- may retire only its own boot; immutable receipts survive lost HTTP replies.
CREATE TABLE IF NOT EXISTS ci_application_retirement (
    command_id TEXT PRIMARY KEY,
    target_boot UUID NOT NULL REFERENCES ci_executor_boot(boot_id),
    request_hash TEXT NOT NULL,
    request JSONB NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('pending','draining','safe')),
    receipt JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((phase='safe') = (receipt IS NOT NULL))
);
CREATE UNIQUE INDEX IF NOT EXISTS ci_application_retirement_active_boot
    ON ci_application_retirement(target_boot) WHERE phase <> 'safe';
