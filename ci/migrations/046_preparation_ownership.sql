-- Old binaries and existing claims have no proof of a pre-VM boundary.
ALTER TABLE ci_host_work ADD COLUMN IF NOT EXISTS phase TEXT NOT NULL DEFAULT 'execution'
    CHECK (phase IN ('preparing', 'execution', 'detached_preparation'));
-- Retain provenance when a terminal preparation releases its CI-app owner.
ALTER TABLE ci_host_work ADD COLUMN IF NOT EXISTS executor_boot UUID;
