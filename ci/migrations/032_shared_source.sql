-- Source is committed with run admission, never read from another controller's
-- workspace. Keep bytes separate from metadata queries over ci_run.
CREATE TABLE IF NOT EXISTS ci_run_source (
    run_id TEXT PRIMARY KEY REFERENCES ci_run(id) ON DELETE CASCADE,
    descriptor BYTEA NOT NULL
);
