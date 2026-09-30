-- Admission belongs to a process boot, never the entire CI application.
ALTER TABLE ci_executor_boot ADD COLUMN IF NOT EXISTS draining BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE ci_executor_boot ADD COLUMN IF NOT EXISTS maintenance_operation UUID;
ALTER TABLE ci_executor_boot ADD COLUMN IF NOT EXISTS execution_protocol TEXT NOT NULL DEFAULT 'legacy-singleton';

-- Keep the historical singleton/handoff records for diagnostics during the
-- upgrade. New processes neither read nor transfer their execution authority.
