-- Registered repositories and the tokens `git submit` authenticates with.
--
-- Same rules as 001: every statement idempotent, because the whole directory is
-- re-executed on every startup.
--
-- The point of a registration is not bookkeeping. It is that a submit can name
-- *which* repository it is for with a credential instead of with a JSON field —
-- so a token stolen from one repository cannot start a build of another, and
-- revoking one repository's access does not mean rotating a secret shared by
-- everybody.

CREATE TABLE IF NOT EXISTS ci_repo (
    id              TEXT PRIMARY KEY,
    -- The clone URL as it was registered, kept verbatim so the dashboard shows
    -- back what someone typed.
    url             TEXT        NOT NULL,
    -- `repos::normalize(url)`. The unique key, because `git@github.com:me/app.git`
    -- and `https://github.com/me/app` are one repository written two ways, and
    -- registering both would make "which token is this repository's" ambiguous.
    normalized      TEXT        NOT NULL UNIQUE,
    name            TEXT        NOT NULL,
    -- Overrides CI_WORKFLOW_PATH for this repository. NULL means the
    -- installation default; a workflow object, where one exists, still wins.
    workflow_path   TEXT,
    enabled         BOOLEAN     NOT NULL DEFAULT TRUE,
    -- app-lb's stable Google `sub`, and the email for display.
    created_by      TEXT,
    created_email   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per token. `id` is the key id carried in the token itself, which is
-- what makes verification an indexed lookup rather than a scan over every
-- digest — this is checked on an unauthenticated public route.
--
-- **Only a digest is stored.** `secret_hash` is SHA-256 of the token's secret
-- half, so a database read discloses nothing that can submit. See `repos.rs`
-- for why this path is a hashed bearer rather than an HMAC over the body.
CREATE TABLE IF NOT EXISTS ci_repo_token (
    id              TEXT PRIMARY KEY,
    repo_id         TEXT        NOT NULL REFERENCES ci_repo(id) ON DELETE CASCADE,
    -- What it is for: a person, a laptop, a machine account.
    name            TEXT        NOT NULL DEFAULT '',
    secret_hash     TEXT        NOT NULL,
    created_by      TEXT,
    created_email   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Answers "is this token still in use", which is the question anyone asks
    -- before revoking one.
    last_used_at    TIMESTAMPTZ,
    -- Revoked rather than deleted: a token that submitted things should still be
    -- nameable after it stops working.
    revoked_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS ci_repo_token_repo_idx ON ci_repo_token (repo_id, created_at DESC);

-- Which registration a run came from, when it came from one. NULL for a run
-- submitted with the shared CI_WEBHOOK_SECRET, which by construction cannot say.
--
-- `ON DELETE SET NULL`, not CASCADE: removing a registration must not delete the
-- history of what it built.
ALTER TABLE ci_run ADD COLUMN IF NOT EXISTS repo_id TEXT REFERENCES ci_repo(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS ci_run_repo_idx ON ci_run (repo_id, created_at DESC);
