//! Per-application authenticated forwarding to one managed instance. No peer
//! addresses or Cloud service credentials are exposed to the application.
use anyhow::{Context, Result};
use axum::{body::Body, extract::{Path, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}};
use base64::Engine;
use heyosecret_client::{HeyoSecretClient, HeyoSecretClientOptions};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use crate::{auth, cloud_client, db, AppState};
use super::service_discovery;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Contract {
    pub port: u16,
    pub token_secret_path: String,
}

impl Contract {
    pub(super) fn from_metadata(metadata: &serde_json::Value) -> Result<Option<Self>> {
        let Some(value) = metadata.get("applicationLifecycle") else { return Ok(None); };
        let contract: Self = serde_json::from_value(value.clone())?;
        anyhow::ensure!(contract.port > 0 && !contract.token_secret_path.trim().is_empty(),
            "application lifecycle contract is incomplete");
        Ok(Some(contract))
    }

    pub(super) async fn token(&self, state: &AppState) -> Result<String> {
        let client = HeyoSecretClient::new(HeyoSecretClientOptions {
            base_url: state.config.heyosecret_url.clone(),
            token: if state.config.heyosecret_internal_api_key.is_empty() { state.config.internal_api_key.clone() }
                else { state.config.heyosecret_internal_api_key.clone() },
            timeout: Some(Duration::from_secs(10)),
        })?;
        let token = String::from_utf8(client.read_active(&self.token_secret_path).await?.value)?;
        anyhow::ensure!(!token.is_empty(), "application lifecycle credential is empty");
        Ok(token)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    boot_id: uuid::Uuid,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

pub async fn forward(State(state): State<AppState>, Path((service, deployment)): Path<(String, String)>,
    headers: HeaderMap, body: Body) -> Response {
    match forward_inner(&state, &service, &deployment, &headers, body).await {
        Ok(response) => response,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn forward_inner(state: &AppState, service: &str, deployment: &str, headers: &HeaderMap, body: Body) -> Result<Response> {
    let db = db::get_db()?;
    let stored = db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT active_metadata FROM service_deployment_states WHERE service_id=$1", [service.into()])).await?
        .context("managed service is not registered")?;
    let metadata: serde_json::Value = stored.try_get("", "active_metadata")?;
    let contract = Contract::from_metadata(&metadata["source"])?.context("managed instance transport is not configured")?;
    let token = contract.token(state).await?;
    if let Err(status) = auth::require_internal_api_key(headers, &token) { return Ok(status.into_response()); }
    let metadata = headers.get("x-heyo-instance-request").context("instance request metadata missing")?.as_bytes();
    if metadata.len() > 16 * 1024 { return Ok(StatusCode::PAYLOAD_TOO_LARGE.into_response()); }
    let request: Request = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(metadata)?)?;
    // External adoption does not authorize this managed instance transport.
    anyhow::ensure!(db.query_one(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT 1 FROM external_service_bindings WHERE service_id=$1", [service.into()])).await?.is_none(),
        "external binding is not a managed application");
    let membership = service_discovery::read_stored_snapshot(service).await?.context("managed discovery missing")?;
    let endpoint = membership.endpoints.iter().find(|e| e.deployment_id == deployment)
        .context("deployment is not a member of the managed application")?;
    let binding = cloud_client::observe_retained_deployment(state, deployment, contract.port).await?;
    anyhow::ensure!(endpoint.backend_server_id.as_deref() == Some(&binding.backend_server_id)
        && endpoint.region.as_deref() == Some(&binding.region), "managed instance binding changed");
    let mut inner_headers = request.headers;
    anyhow::ensure!(!inner_headers.iter().any(|(name,_)| name.eq_ignore_ascii_case("x-ci-target-boot")
        || name.eq_ignore_ascii_case("x-ci-forwarded")), "caller cannot override the target boot");
    inner_headers.push(("x-ci-target-boot".into(), request.boot_id.to_string()));
    inner_headers.push(("x-ci-forwarded".into(), "1".into()));
    let transport = cloud_client::ExactRuntimeHttpRequest {
        expected_backend_server_id: binding.backend_server_id,
        expected_backend_sandbox_id: binding.backend_sandbox_id,
        port: contract.port, method: request.method, path: request.path, headers: inner_headers,
    };
    let (metadata, response) = cloud_client::exact_runtime_http(state, deployment, &transport,
        reqwest::Body::wrap_stream(body.into_data_stream())).await?;
    let mut result = Response::builder().status(metadata.status);
    for (name,value) in metadata.headers {
        let name = axum::http::HeaderName::from_bytes(name.as_bytes())?;
        anyhow::ensure!(!matches!(name.as_str(), "connection" | "keep-alive" | "proxy-authenticate"
            | "proxy-authorization" | "te" | "trailer" | "transfer-encoding" | "upgrade" | "content-length"),
            "invalid instance response header");
        result = result.header(name, axum::http::HeaderValue::from_str(&value)?);
    }
    Ok(result.body(Body::from_stream(response.bytes_stream()))?)
}
