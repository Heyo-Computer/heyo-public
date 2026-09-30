-- Transport redeliveries (including drain handoffs) are not execution failures.
-- Count only failed pre-claim dispatches, atomically with their diagnostics.
ALTER TABLE ci_job ADD COLUMN IF NOT EXISTS preclaim_failures INTEGER NOT NULL DEFAULT 0;
