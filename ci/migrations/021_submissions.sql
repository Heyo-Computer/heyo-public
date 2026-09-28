-- A submission is admitted as one immutable set of validation runs and one
-- release run.  It has no coordinator status of its own: ci_run remains the
-- source of truth for execution and ci_release for publication.
ALTER TABLE ci_run ADD COLUMN IF NOT EXISTS validation_only BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE IF NOT EXISTS ci_submission (
    release_run_id  TEXT PRIMARY KEY REFERENCES ci_run(id) ON DELETE CASCADE,
    validation_count INTEGER NOT NULL CHECK (validation_count > 0),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS ci_submission_validation (
    release_run_id    TEXT NOT NULL REFERENCES ci_submission(release_run_id) ON DELETE CASCADE,
    validation_run_id TEXT NOT NULL REFERENCES ci_run(id) ON DELETE RESTRICT,
    ordinal           INTEGER NOT NULL CHECK (ordinal >= 0),
    PRIMARY KEY (release_run_id, validation_run_id),
    UNIQUE (release_run_id, ordinal),
    -- A validation run is evidence for exactly one admitted submission.
    UNIQUE (validation_run_id)
);

CREATE INDEX IF NOT EXISTS ci_submission_validation_release_idx
    ON ci_submission_validation (release_run_id, ordinal);
