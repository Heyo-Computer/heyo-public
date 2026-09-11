-- Durable ownership of a release publication.  The prepared candidate is
-- committed before the network side effect, so a lost push acknowledgement can
-- only retry the exact same commit.
CREATE TABLE IF NOT EXISTS ci_release (
    run_id          TEXT PRIMARY KEY REFERENCES ci_run(id) ON DELETE CASCADE,
    request_hash    TEXT        NOT NULL,
    source_sha      TEXT        NOT NULL,
    base_sha        TEXT        NOT NULL,
    git_ref         TEXT        NOT NULL,
    versions        JSONB       NOT NULL,
    candidate_sha   TEXT        NOT NULL,
    prepared        JSONB       NOT NULL,
    status          TEXT        NOT NULL CHECK (status IN ('prepared', 'unknown', 'published')),
    error           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ci_release_status_idx ON ci_release(status, updated_at);

-- Set only after a clean checkout of the published release tree in this job.
ALTER TABLE ci_job ADD COLUMN IF NOT EXISTS release_sha TEXT;
CREATE TABLE IF NOT EXISTS ci_service_archive (
    step_id TEXT PRIMARY KEY REFERENCES ci_step(id),
    run_id TEXT NOT NULL REFERENCES ci_run(id),
    job_id TEXT NOT NULL REFERENCES ci_job(id),
    archive_id TEXT NOT NULL,
    sha TEXT NOT NULL,
    orchestrator_url TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
