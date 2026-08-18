-- When a job was put on the queue.
--
-- `created_at` is when the row was made, which for a job with `needs:` can be
-- long before anything could run it — so it cannot answer "has this been waiting
-- for a runner too long". That question is what `CI_RUNNER_WAIT_SECS` is meant
-- to bound, and answering it with `created_at` would fail a job that was
-- legitimately waiting on a dependency.
ALTER TABLE ci_job ADD COLUMN IF NOT EXISTS queued_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS ci_job_queued_idx ON ci_job (status, queued_at);
