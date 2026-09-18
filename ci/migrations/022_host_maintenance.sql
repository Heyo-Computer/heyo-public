-- A failed/unknown operation deliberately retains its runner fence.
CREATE TABLE IF NOT EXISTS ci_host_maintenance (
    id VARCHAR(64) PRIMARY KEY REFERENCES ci_service_deployment(id),
    runner_hd_id TEXT NOT NULL,
    request JSONB NOT NULL,
    phase TEXT NOT NULL DEFAULT 'releasing' CHECK (phase IN ('releasing','draining','submitting','polling','failed','passed')),
    deadline TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS ci_host_maintenance_fence ON ci_host_maintenance(runner_hd_id) WHERE phase <> 'passed';
ALTER TABLE ci_service_archive ADD COLUMN IF NOT EXISTS archive_user_id TEXT;
ALTER TABLE ci_service_archive ADD COLUMN IF NOT EXISTS archive_sha256 TEXT;
ALTER TABLE ci_service_archive ADD COLUMN IF NOT EXISTS heyvm_sha256 TEXT;
-- Cancellation changes job status before its executor necessarily stops.
-- Track each delivery/runner separately: retries must not erase old host work.
DO $$ BEGIN
    IF to_regclass('ci_host_work') IS NULL THEN
        CREATE TABLE ci_host_work (
            job_id TEXT NOT NULL REFERENCES ci_job(id),
            runner_hd_id TEXT NOT NULL,
            attempt INTEGER NOT NULL,
            PRIMARY KEY(job_id,runner_hd_id,attempt)
        );
        INSERT INTO ci_host_work(job_id,runner_hd_id,attempt)
        SELECT id,runner_hd_id,attempt FROM ci_job WHERE status='running' AND runner_hd_id IS NOT NULL;
    END IF;
END $$;
CREATE INDEX IF NOT EXISTS ci_host_work_runner ON ci_host_work(runner_hd_id);
