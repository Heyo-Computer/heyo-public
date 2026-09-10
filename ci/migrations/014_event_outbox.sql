-- Durable, repository-scoped execution history and its NATS publication state.
-- revision is the canonical order: transaction timestamps can be equal and UUIDs
-- are intentionally unordered.
CREATE SEQUENCE IF NOT EXISTS ci_event_revision_seq;

CREATE TABLE IF NOT EXISTS ci_event_outbox (
    id              UUID PRIMARY KEY,
    revision        BIGINT      NOT NULL DEFAULT nextval('ci_event_revision_seq'),
    run_id          TEXT        NOT NULL REFERENCES ci_run(id) ON DELETE CASCADE,
    repo_id         TEXT,
    subject         TEXT        NOT NULL,
    event_type      TEXT        NOT NULL,
    job_id          TEXT,
    job_key         TEXT,
    step_id         TEXT,
    status          TEXT        NOT NULL,
    error           TEXT,
    payload         JSONB       NOT NULL,
    transitioned_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    published_at    TIMESTAMPTZ,
    attempts        INTEGER     NOT NULL DEFAULT 0,
    last_error      TEXT,
    UNIQUE (revision)
);

CREATE INDEX IF NOT EXISTS ci_event_outbox_pending_idx
    ON ci_event_outbox (revision) WHERE published_at IS NULL;
CREATE INDEX IF NOT EXISTS ci_event_outbox_run_history_idx
    ON ci_event_outbox (run_id, revision DESC);

-- One upload action publishes one artifact. Old rows have no step identity;
-- retain them without claiming they have a replay-safe publication event.
ALTER TABLE ci_artifact ADD COLUMN IF NOT EXISTS step_id TEXT REFERENCES ci_step(id) ON DELETE SET NULL;
CREATE UNIQUE INDEX IF NOT EXISTS ci_artifact_step_idx ON ci_artifact (step_id);
