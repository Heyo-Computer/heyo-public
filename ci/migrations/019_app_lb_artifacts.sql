-- Provenance boundary between rootfs publication and app-lb deployment. A
-- deployment may consume only a manifest successfully published by this run.
CREATE TABLE IF NOT EXISTS ci_app_lb_artifact (
    step_id TEXT PRIMARY KEY REFERENCES ci_step(id),
    run_id TEXT NOT NULL REFERENCES ci_run(id),
    job_id TEXT NOT NULL REFERENCES ci_job(id),
    sha TEXT NOT NULL,
    store_url TEXT NOT NULL,
    manifest_digest TEXT NOT NULL CHECK (manifest_digest ~ '^[0-9a-f]{64}$'),
    blob_digest TEXT NOT NULL CHECK (blob_digest ~ '^[0-9a-f]{64}$'),
    size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS ci_app_lb_artifact_run_idx ON ci_app_lb_artifact(run_id, created_at);
CREATE INDEX IF NOT EXISTS ci_app_lb_artifact_authorize_idx
    ON ci_app_lb_artifact(run_id, sha, store_url, manifest_digest);
