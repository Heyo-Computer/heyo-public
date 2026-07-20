use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::config::{resolve_migrations_dir, Config};
use crate::crypto::SecretCrypto;
use crate::models::{
    SecretHistoryResponse, SecretMetadataResponse, SecretStatus, SecretValueResponse,
    SecretVersionMetadata, WriteSecretResponse,
};

#[derive(Clone)]
pub struct SecretStore {
    pool: PgPool,
    crypto: SecretCrypto,
}

struct VersionRow {
    path: String,
    version: i32,
    status: SecretStatus,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
    created_at: DateTime<Utc>,
    metadata: Value,
}

impl SecretStore {
    pub async fn connect(config: &Config) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(config.db_max_connections)
            .min_connections(config.db_min_connections)
            .acquire_timeout(config.acquire_timeout())
            .idle_timeout(config.idle_timeout())
            .max_lifetime(config.max_lifetime())
            .connect(&config.database_url)
            .await
            .context("connect HeyoSecret database")?;

        run_migrations(&pool).await?;

        Ok(Self {
            pool,
            crypto: SecretCrypto::new(config.encryption_key()),
        })
    }

    pub async fn put_secret(&self, input: PutSecretInput) -> Result<WriteSecretResponse> {
        let path = normalize_secret_path(&input.path)?;
        let (nonce, ciphertext, value_sha256) = self.crypto.encrypt(&input.value)?;
        let mut tx = self.pool.begin().await?;
        let secret_id = upsert_secret_metadata(&mut tx, &path, &input).await?;
        let version = next_version(&mut tx, secret_id).await?;

        sqlx::query(
            "UPDATE heyosecret_secret_versions SET status = 'retiring' WHERE secret_id = $1 AND status = 'active'",
        )
        .bind(secret_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO heyosecret_secret_versions (id, secret_id, version, status, nonce, ciphertext, value_sha256, created_by, metadata) VALUES ($1, $2, $3, 'active', $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::new_v4())
        .bind(secret_id)
        .bind(version)
        .bind(nonce)
        .bind(ciphertext)
        .bind(value_sha256)
        .bind(input.actor.as_deref())
        .bind(input.metadata)
        .execute(&mut *tx)
        .await?;

        audit(&mut tx, "write", &path, Some(version), input.actor.as_deref(), json!({})).await?;
        tx.commit().await?;

        Ok(WriteSecretResponse {
            path,
            version,
            status: SecretStatus::Active,
        })
    }

    pub async fn rotate_random(
        &self,
        path: &str,
        value: Vec<u8>,
        actor: Option<String>,
        metadata: Value,
    ) -> Result<WriteSecretResponse> {
        self.put_secret(PutSecretInput {
            path: path.to_string(),
            value,
            owner: None,
            description: None,
            tags: None,
            read_access: None,
            write_access: None,
            metadata,
            actor,
        })
        .await
    }

    pub async fn read_secret(&self, path: &str, version: Option<i32>) -> Result<SecretValueResponse> {
        let path = normalize_secret_path(path)?;
        let row = if let Some(version) = version {
            sqlx::query(
                "SELECT s.path, v.version, v.status, v.nonce, v.ciphertext, v.created_at, v.metadata FROM heyosecret_secrets s JOIN heyosecret_secret_versions v ON v.secret_id = s.id WHERE s.path = $1 AND v.version = $2 AND v.status <> 'deleted'",
            )
            .bind(&path)
            .bind(version)
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT s.path, v.version, v.status, v.nonce, v.ciphertext, v.created_at, v.metadata FROM heyosecret_secrets s JOIN heyosecret_secret_versions v ON v.secret_id = s.id WHERE s.path = $1 AND v.status = 'active'",
            )
            .bind(&path)
            .fetch_optional(&self.pool)
            .await?
        }
        .ok_or_else(|| anyhow!("secret not found: {path}"))?;

        let row = VersionRow {
            path: row.try_get("path")?,
            version: row.try_get("version")?,
            status: parse_status(row.try_get::<String, _>("status")?)?,
            nonce: row.try_get("nonce")?,
            ciphertext: row.try_get("ciphertext")?,
            created_at: row.try_get("created_at")?,
            metadata: row.try_get("metadata")?,
        };
        let value = self.crypto.decrypt(&row.nonce, &row.ciphertext)?;

        Ok(SecretValueResponse {
            path: row.path,
            version: row.version,
            status: row.status,
            value_base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, value),
            created_at: row.created_at,
            metadata: row.metadata,
        })
    }

    pub async fn metadata(&self, path: &str) -> Result<SecretMetadataResponse> {
        let path = normalize_secret_path(path)?;
        let row = sqlx::query(
            "SELECT s.path, s.owner, s.description, s.tags, s.read_access, s.write_access, s.created_at, s.updated_at, active.version AS active_version FROM heyosecret_secrets s LEFT JOIN heyosecret_secret_versions active ON active.secret_id = s.id AND active.status = 'active' WHERE s.path = $1",
        )
        .bind(&path)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow!("secret not found: {path}"))?;
        metadata_from_row(row)
    }

    pub async fn list(&self, prefix: Option<&str>) -> Result<Vec<SecretMetadataResponse>> {
        let prefix = match prefix {
            Some(value) if !value.trim().is_empty() => Some(normalize_secret_path(value)?),
            _ => None,
        };
        let rows = if let Some(prefix) = prefix {
            sqlx::query(
                "SELECT s.path, s.owner, s.description, s.tags, s.read_access, s.write_access, s.created_at, s.updated_at, active.version AS active_version FROM heyosecret_secrets s LEFT JOIN heyosecret_secret_versions active ON active.secret_id = s.id AND active.status = 'active' WHERE s.path = $1 OR s.path LIKE $2 ORDER BY s.path",
            )
            .bind(&prefix)
            .bind(format!("{prefix}/%"))
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT s.path, s.owner, s.description, s.tags, s.read_access, s.write_access, s.created_at, s.updated_at, active.version AS active_version FROM heyosecret_secrets s LEFT JOIN heyosecret_secret_versions active ON active.secret_id = s.id AND active.status = 'active' ORDER BY s.path",
            )
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter().map(metadata_from_row).collect()
    }

    pub async fn history(&self, path: &str) -> Result<SecretHistoryResponse> {
        let path = normalize_secret_path(path)?;
        let rows = sqlx::query(
            "SELECT v.version, v.status, v.value_sha256, v.created_by, v.created_at, v.expires_at, v.metadata FROM heyosecret_secrets s JOIN heyosecret_secret_versions v ON v.secret_id = s.id WHERE s.path = $1 ORDER BY v.version DESC",
        )
        .bind(&path)
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Err(anyhow!("secret not found: {path}"));
        }

        let mut versions = Vec::with_capacity(rows.len());
        for row in rows {
            versions.push(SecretVersionMetadata {
                version: row.try_get("version")?,
                status: parse_status(row.try_get::<String, _>("status")?)?,
                value_sha256: row.try_get("value_sha256")?,
                created_by: row.try_get("created_by")?,
                created_at: row.try_get("created_at")?,
                expires_at: row.try_get("expires_at")?,
                metadata: row.try_get("metadata")?,
            });
        }

        Ok(SecretHistoryResponse { path, versions })
    }

    pub async fn revoke(&self, path: &str, version: i32, actor: Option<String>) -> Result<()> {
        let path = normalize_secret_path(path)?;
        let result = sqlx::query(
            "UPDATE heyosecret_secret_versions v SET status = 'revoked' FROM heyosecret_secrets s WHERE v.secret_id = s.id AND s.path = $1 AND v.version = $2 AND v.status <> 'deleted'",
        )
        .bind(&path)
        .bind(version)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(anyhow!("secret version not found: {path}@{version}"));
        }
        let mut tx = self.pool.begin().await?;
        audit(&mut tx, "revoke", &path, Some(version), actor.as_deref(), json!({})).await?;
        tx.commit().await?;
        Ok(())
    }
}

pub struct PutSecretInput {
    pub path: String,
    pub value: Vec<u8>,
    pub owner: Option<String>,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub read_access: Option<Vec<String>>,
    pub write_access: Option<Vec<String>>,
    pub metadata: Value,
    pub actor: Option<String>,
}

async fn run_migrations(pool: &PgPool) -> Result<()> {
    let migrations_dir = resolve_migrations_dir()?;
    let mut files: Vec<_> = std::fs::read_dir(&migrations_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().map_or(false, |ext| ext == "sql"))
        .collect();
    files.sort_by_key(|entry| entry.file_name());

    for entry in files {
        let path = entry.path();
        let sql = std::fs::read_to_string(&path)?;
        let sql = sql.trim();
        if sql.is_empty() {
            continue;
        }
        sqlx::raw_sql(sql)
            .execute(pool)
            .await
            .with_context(|| format!("apply HeyoSecret migration {}", path.display()))?;
    }
    Ok(())
}

async fn upsert_secret_metadata(
    tx: &mut Transaction<'_, Postgres>,
    path: &str,
    input: &PutSecretInput,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let tags = json!(input.tags.clone().unwrap_or_default());
    let read_access = json!(input.read_access.clone().unwrap_or_default());
    let write_access = json!(input.write_access.clone().unwrap_or_default());

    let row = sqlx::query(
        "INSERT INTO heyosecret_secrets (id, path, owner, description, tags, read_access, write_access) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (path) DO UPDATE SET owner = COALESCE(EXCLUDED.owner, heyosecret_secrets.owner), description = COALESCE(EXCLUDED.description, heyosecret_secrets.description), tags = CASE WHEN EXCLUDED.tags = '[]'::jsonb THEN heyosecret_secrets.tags ELSE EXCLUDED.tags END, read_access = CASE WHEN EXCLUDED.read_access = '[]'::jsonb THEN heyosecret_secrets.read_access ELSE EXCLUDED.read_access END, write_access = CASE WHEN EXCLUDED.write_access = '[]'::jsonb THEN heyosecret_secrets.write_access ELSE EXCLUDED.write_access END, updated_at = NOW() RETURNING id",
    )
    .bind(id)
    .bind(path)
    .bind(input.owner.as_deref())
    .bind(input.description.as_deref())
    .bind(tags)
    .bind(read_access)
    .bind(write_access)
    .fetch_one(&mut **tx)
    .await?;
    Ok(row.try_get("id")?)
}

async fn next_version(tx: &mut Transaction<'_, Postgres>, secret_id: Uuid) -> Result<i32> {
    let row = sqlx::query(
        "SELECT COALESCE(MAX(version), 0) + 1 AS next_version FROM heyosecret_secret_versions WHERE secret_id = $1",
    )
    .bind(secret_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(row.try_get("next_version")?)
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    action: &str,
    path: &str,
    version: Option<i32>,
    actor: Option<&str>,
    details: Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO heyosecret_audit_events (id, action, path, version, actor, details) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(action)
    .bind(path)
    .bind(version)
    .bind(actor)
    .bind(details)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn metadata_from_row(row: sqlx::postgres::PgRow) -> Result<SecretMetadataResponse> {
    Ok(SecretMetadataResponse {
        path: row.try_get("path")?,
        owner: row.try_get("owner")?,
        description: row.try_get("description")?,
        tags: json_array_to_strings(row.try_get("tags")?),
        read_access: json_array_to_strings(row.try_get("read_access")?),
        write_access: json_array_to_strings(row.try_get("write_access")?),
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        active_version: row.try_get("active_version")?,
    })
}

fn json_array_to_strings(value: Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_status(value: String) -> Result<SecretStatus> {
    match value.as_str() {
        "active" => Ok(SecretStatus::Active),
        "retiring" => Ok(SecretStatus::Retiring),
        "revoked" => Ok(SecretStatus::Revoked),
        "deleted" => Ok(SecretStatus::Deleted),
        other => Err(anyhow!("unknown secret status: {other}")),
    }
}

pub fn normalize_secret_path(path: &str) -> Result<String> {
    let path = path.trim().trim_matches('/');
    if path.is_empty() {
        return Err(anyhow!("secret path cannot be empty"));
    }
    if path.len() > 1024 {
        return Err(anyhow!("secret path cannot exceed 1024 characters"));
    }
    if path.contains("//") || path.split('/').any(|part| part == "." || part == "..") {
        return Err(anyhow!("secret path contains invalid path segments"));
    }
    if !path.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ':' | '@')
    }) {
        return Err(anyhow!(
            "secret path supports only alphanumeric characters plus / - _ . : @"
        ));
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::normalize_secret_path;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_secret_path("/platform/jwt/key/").unwrap(), "platform/jwt/key");
        assert!(normalize_secret_path("").is_err());
        assert!(normalize_secret_path("../key").is_err());
        assert!(normalize_secret_path("platform//key").is_err());
    }
}
