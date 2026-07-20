CREATE TABLE IF NOT EXISTS heyosecret_secrets (
    id UUID PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    owner TEXT,
    description TEXT,
    tags JSONB NOT NULL DEFAULT '[]'::jsonb,
    read_access JSONB NOT NULL DEFAULT '[]'::jsonb,
    write_access JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS heyosecret_secret_versions (
    id UUID PRIMARY KEY,
    secret_id UUID NOT NULL REFERENCES heyosecret_secrets(id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'retiring', 'revoked', 'deleted')),
    nonce BYTEA NOT NULL,
    ciphertext BYTEA NOT NULL,
    value_sha256 TEXT NOT NULL,
    created_by TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE(secret_id, version)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_heyosecret_one_active_version
    ON heyosecret_secret_versions(secret_id)
    WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_heyosecret_secrets_path
    ON heyosecret_secrets(path);

CREATE INDEX IF NOT EXISTS idx_heyosecret_secret_versions_secret_created
    ON heyosecret_secret_versions(secret_id, created_at DESC);

CREATE TABLE IF NOT EXISTS heyosecret_audit_events (
    id UUID PRIMARY KEY,
    action TEXT NOT NULL,
    path TEXT NOT NULL,
    version INTEGER,
    actor TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    details JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS idx_heyosecret_audit_path_created
    ON heyosecret_audit_events(path, created_at DESC);
