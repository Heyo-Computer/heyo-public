use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait, Value as SeaValue};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{auth, db, AppState};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDiscoveryEndpoint {
    pub deployment_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_server_id: Option<String>,
    pub url: String,
    pub health_status: String,
    pub draining: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDiscoverySnapshot {
    pub service_id: String,
    pub version: u64,
    pub endpoints: Vec<ServiceDiscoveryEndpoint>,
    pub updated_at: DateTime<Utc>,
}

pub async fn get_service_discovery(
    headers: HeaderMap,
    State(state): State<AppState>,
    AxumPath(service_id): AxumPath<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(status) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return (status, Json(json!({ "error": "Unauthorized" })));
    }

    match read_snapshot(&service_id).await {
        Ok(Some(snapshot)) => (StatusCode::OK, Json(json!(snapshot))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Service discovery set not found" })),
        ),
        Err(error) => {
            tracing::warn!(service_id, "failed to read service discovery set: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to read service discovery set" })),
            )
        }
    }
}

pub async fn read_snapshot(service_id: &str) -> Result<Option<ServiceDiscoverySnapshot>> {
    let db = db::get_db()?;
    let set = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT version, updated_at FROM service_discovery_sets WHERE service_id = $1",
            [service_id.into()],
        ))
        .await
        .context("failed to query service discovery set")?;
    let Some(set) = set else {
        return Ok(None);
    };

    let version: i64 = set.try_get("", "version")?;
    let updated_at: DateTime<chrono::FixedOffset> = set.try_get("", "updated_at")?;
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT deployment_id, backend_server_id, backend_url, health_status, draining
             FROM service_discovery_endpoints
             WHERE service_id = $1
             ORDER BY deployment_id ASC",
            [service_id.into()],
        ))
        .await
        .context("failed to query service discovery endpoints")?;
    let endpoints = rows
        .into_iter()
        .map(|row| {
            Ok(ServiceDiscoveryEndpoint {
                deployment_id: row.try_get("", "deployment_id")?,
                backend_server_id: row.try_get("", "backend_server_id")?,
                url: row.try_get("", "backend_url")?,
                health_status: row.try_get("", "health_status")?,
                draining: row.try_get("", "draining")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(ServiceDiscoverySnapshot {
        service_id: service_id.to_string(),
        version: u64::try_from(version).context("service discovery version was negative")?,
        endpoints,
        updated_at: updated_at.with_timezone(&Utc),
    }))
}

pub async fn active_backend_server_ids(service_id: &str) -> Result<Vec<String>> {
    Ok(read_snapshot(service_id)
        .await?
        .map(|snapshot| {
            snapshot
                .endpoints
                .into_iter()
                .filter(|endpoint| !endpoint.draining)
                .filter_map(|endpoint| endpoint.backend_server_id)
                .collect()
        })
        .unwrap_or_default())
}

/// Publish a healthy candidate. Replacing a service marks old members draining;
/// adding capacity leaves existing members eligible. Membership and operator
/// drain intent stay durable even when a later health observation changes.
pub async fn publish_healthy_endpoint(
    service_id: &str,
    deployment_id: &str,
    backend_server_id: Option<&str>,
    backend_url: &str,
    replace_existing: bool,
) -> Result<ServiceDiscoverySnapshot> {
    validate_endpoint_url(backend_url)?;
    let db = db::get_db()?;
    let transaction = db.begin().await?;
    ensure_set(&transaction, service_id).await?;
    if replace_existing {
        transaction
            .execute(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE service_discovery_endpoints
                 SET draining = TRUE, updated_at = NOW()
                 WHERE service_id = $1 AND deployment_id <> $2 AND draining = FALSE",
                [service_id.into(), deployment_id.into()],
            ))
            .await?;
    }
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO service_discovery_endpoints (
                service_id, deployment_id, backend_server_id, backend_url,
                health_status, draining, observed_at, updated_at
             ) VALUES ($1, $2, $3, $4, 'healthy', FALSE, NOW(), NOW())
             ON CONFLICT (service_id, deployment_id) DO UPDATE SET
                backend_server_id = EXCLUDED.backend_server_id,
                backend_url = EXCLUDED.backend_url,
                health_status = 'healthy',
                draining = FALSE,
                observed_at = NOW(),
                updated_at = NOW()",
            vec![
                service_id.into(),
                deployment_id.into(),
                backend_server_id.map(str::to_string).into(),
                backend_url.into(),
            ],
        ))
        .await?;
    bump_version(&transaction, service_id).await?;
    transaction.commit().await?;
    read_snapshot(service_id)
        .await?
        .context("published service discovery set disappeared")
}

pub async fn mark_endpoint_draining(service_id: &str, deployment_id: &str) -> Result<()> {
    let db = db::get_db()?;
    let transaction = db.begin().await?;
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE service_discovery_endpoints
             SET draining = TRUE, updated_at = NOW()
             WHERE service_id = $1 AND deployment_id = $2 AND draining = FALSE",
            [service_id.into(), deployment_id.into()],
        ))
        .await?;
    if result.rows_affected() > 0 {
        bump_version(&transaction, service_id).await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn remove_endpoint(service_id: &str, deployment_id: &str) -> Result<()> {
    let db = db::get_db()?;
    let transaction = db.begin().await?;
    let result = transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM service_discovery_endpoints WHERE service_id = $1 AND deployment_id = $2",
            [service_id.into(), deployment_id.into()],
        ))
        .await?;
    if result.rows_affected() > 0 {
        bump_version(&transaction, service_id).await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Restore the membership observed before a failed cutover. The set remains and
/// its version advances even when the old snapshot was absent, so watchers that
/// already saw the failed candidate receive an authoritative empty snapshot
/// rather than keeping that candidate forever after a 404.
pub async fn restore_snapshot(
    service_id: &str,
    snapshot: Option<&ServiceDiscoverySnapshot>,
) -> Result<()> {
    let db = db::get_db()?;
    let transaction = db.begin().await?;
    ensure_set(&transaction, service_id).await?;
    transaction
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM service_discovery_endpoints WHERE service_id = $1",
            [service_id.into()],
        ))
        .await?;
    if let Some(snapshot) = snapshot {
        for endpoint in &snapshot.endpoints {
            transaction
                .execute(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "INSERT INTO service_discovery_endpoints (
                        service_id, deployment_id, backend_server_id, backend_url,
                        health_status, draining, observed_at, updated_at
                     ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
                    vec![
                        service_id.into(),
                        endpoint.deployment_id.clone().into(),
                        endpoint.backend_server_id.clone().into(),
                        endpoint.url.clone().into(),
                        endpoint.health_status.clone().into(),
                        endpoint.draining.into(),
                    ],
                ))
                .await?;
        }
    }
    bump_version(&transaction, service_id).await?;
    transaction.commit().await?;
    Ok(())
}

async fn ensure_set<C: ConnectionTrait>(connection: &C, service_id: &str) -> Result<()> {
    connection
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO service_discovery_sets (service_id) VALUES ($1)
             ON CONFLICT (service_id) DO NOTHING",
            [service_id.into()],
        ))
        .await?;
    Ok(())
}

async fn bump_version<C: ConnectionTrait>(connection: &C, service_id: &str) -> Result<()> {
    connection
        .execute(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE service_discovery_sets
             SET version = version + 1, updated_at = NOW()
             WHERE service_id = $1",
            [SeaValue::String(Some(Box::new(service_id.to_string())))],
        ))
        .await?;
    Ok(())
}

fn validate_endpoint_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).context("service discovery endpoint must be a URL")?;
    // `Url::port()` normalizes an explicit default `:80` to `None`, so inspect
    // the original authority to distinguish it from an omitted port.
    let authority = value
        .split_once("//")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("");
    let has_explicit_port = if authority.starts_with('[') {
        authority.rfind("]:").is_some()
    } else {
        authority.rsplit_once(':').is_some()
    };
    if url.scheme() != "http"
        || url.host_str().is_none()
        || !has_explicit_port
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        anyhow::bail!(
            "service discovery endpoint must be a plaintext, credential-free, pathless URL with an explicit non-default port"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_endpoint_requires_cross_host_proxy_shape() {
        assert!(validate_endpoint_url("http://us3.internal:24001").is_ok());
        assert!(validate_endpoint_url("http://[::1]:80").is_ok());
        for invalid in [
            "https://us3.internal:443",
            "http://us3.internal",
            "http://user@us3.internal:24001",
            "http://us3.internal:24001/health",
        ] {
            assert!(validate_endpoint_url(invalid).is_err(), "accepted {invalid}");
        }
    }
}
