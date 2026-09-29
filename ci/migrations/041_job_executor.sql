-- The process boot that won the queued -> running transition. NULL is retained
-- for historical rows and for tests/administrative transitions which never
-- represented an executor claim.
ALTER TABLE ci_job ADD COLUMN IF NOT EXISTS executor_boot UUID;

CREATE INDEX IF NOT EXISTS ci_job_executor_boot_running_idx
    ON ci_job(executor_boot)
    WHERE status = 'running' AND executor_boot IS NOT NULL;
