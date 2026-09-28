-- A published release may be retried without publishing it again. The retry
-- remains a coordinated submission, but reuses the original immutable
-- validation evidence. A release attempt has at most one child; retrying the
-- child forms a chain and prevents duplicate requests and stale-ancestor
-- replays.
ALTER TABLE ci_submission ADD COLUMN IF NOT EXISTS retry_of TEXT REFERENCES ci_submission(release_run_id);
CREATE UNIQUE INDEX IF NOT EXISTS ci_submission_retry_of_unique
    ON ci_submission(retry_of) WHERE retry_of IS NOT NULL;

-- Validation evidence is immutable and may authorize retries of the release
-- it originally authorized. It is still frozen by each submission's ordered
-- membership and cannot be selected by a partial submit.
ALTER TABLE ci_submission_validation
    DROP CONSTRAINT IF EXISTS ci_submission_validation_validation_run_id_key;

-- Completed deployment steps inside a failed regional job must not execute
-- again. Preserve the original receipt, including across repeated retries.
CREATE TABLE ci_release_carried_deployment (
    job_id TEXT NOT NULL REFERENCES ci_job(id),
    step_index INTEGER NOT NULL,
    deployment_id TEXT NOT NULL REFERENCES ci_service_deployment(id),
    PRIMARY KEY (job_id, step_index)
);
