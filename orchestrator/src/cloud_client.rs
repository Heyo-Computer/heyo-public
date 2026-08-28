use std::collections::HashMap;

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::header::LOCATION;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresignedArchiveUploadSlot {
    #[serde(alias = "archive_id")]
    pub archive_id: String,
    #[serde(default)]
    #[serde(alias = "s3_key")]
    pub s3_key: String,
    #[serde(alias = "upload_url")]
    pub upload_url: String,
    #[serde(default)]
    #[serde(alias = "expires_in_secs")]
    pub expires_in_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PresignArchiveUploadHttpRequest {
    user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FinalizeArchiveHttpRequest {
    user_id: String,
    sandbox_id: String,
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FinalizedArchiveRecord {
    pub id: String,
    #[serde(default)]
    #[serde(alias = "sandbox_id")]
    pub sandbox_id: String,
    #[serde(alias = "s3_key")]
    pub s3_key: String,
    #[serde(alias = "size_bytes")]
    pub size_bytes: i64,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CreateDeploymentRequest {
    pub deployment_id: String,
    pub user_id: String,
    pub account_id: String,
    pub name: String,
    /// DNS-safe slug for the sandbox; used as the hostname in sibling /etc/hosts
    /// injection so multi-sandbox plans can address each other by plan key.
    pub slug: Option<String>,
    pub target: String,
    pub archive_name: Option<String>,
    pub archive_bytes: Vec<u8>,
    pub region: String,
    pub backend_type: String,
    pub image: String,
    pub ports: Vec<u16>,
    pub port_mappings: Vec<PortMapping>,
    pub mounts: Vec<MountConfig>,
    pub env: Option<HashMap<String, String>>,
    pub env_refs: Vec<String>,
    pub start_command: Option<String>,
    pub working_directory: Option<String>,
    pub setup_hooks: Option<Vec<String>>,
    pub size_class: String,
    pub ttl_seconds: Option<u64>,
    pub excluded_backend_server_ids: Vec<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PortMapping {
    pub host: u16,
    pub container: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MountConfig {
    pub host_path: String,
    pub sandbox_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpsertServiceRouteRequest {
    pub service_id: String,
    pub backend_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_backend_url: Option<String>,
    pub route: ServiceRouteRequest,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceRouteRequest {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_points: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert_resolver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
    pub strip_prefix: bool,
    pub pass_host_header: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpsertServiceRouteResponse {
    pub service_id: String,
    pub config_path: String,
    pub backend_url: String,
    pub proxy_subdomain: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeployPreflightRequest {
    pub account_id: String,
    pub requested_sandbox_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeployPreflightResponse {
    pub allowed: bool,
    pub active_sandboxes: u64,
    pub max_active_sandboxes: u64,
    pub remaining_sandbox_capacity: u64,
    pub requested_sandbox_count: u64,
    pub blocking_reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateDeploymentHttpRequest {
    deployment_id: String,
    user_id: String,
    account_id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<String>,
    target: String,
    archive_name: Option<String>,
    archive_bytes_base64: String,
    region: String,
    #[serde(rename = "driver")]
    backend_type: String,
    image: String,
    ports: Vec<u16>,
    port_mappings: Vec<PortMapping>,
    mounts: Vec<MountConfig>,
    env: Option<HashMap<String, String>>,
    env_refs: Vec<String>,
    start_command: Option<String>,
    working_directory: Option<String>,
    setup_hooks: Option<Vec<String>>,
    size_class: String,
    ttl_seconds: Option<u64>,
    excluded_backend_server_ids: Vec<String>,
    metadata: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateDeploymentResponse {
    pub deployment_id: String,
    #[serde(default)]
    pub archive_id: Option<String>,
    #[serde(default)]
    pub backend_server_id: Option<String>,
    #[serde(default)]
    pub backend_server_hostname: Option<String>,
    #[serde(default)]
    pub backend_sandbox_id: Option<String>,
    pub status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateArchiveHttpRequest {
    user_id: String,
    sandbox_id: String,
    name: Option<String>,
    archive_bytes_base64: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateArchiveResponse {
    pub archive_id: String,
    pub s3_key: String,
    pub size_bytes: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HealthcheckUrlResponse {
    url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxExecRequest {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxExecOperationStartRequest {
    pub operation_id: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct SandboxExecResponse {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxExecOperationRecord {
    pub operation_id: String,
    pub sandbox_id: String,
    pub status: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub result: Option<SandboxExecResponse>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReconcileDeploymentHostsRequest {
    deployment_ids: Vec<String>,
}

pub(crate) async fn create_deployment(
    state: &AppState,
    request: &CreateDeploymentRequest,
) -> Result<CreateDeploymentResponse> {
    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/internal/orchestration/deployments",
            state.config.cloud_internal_url.trim_end_matches('/'),
        )),
    )
    .json(&CreateDeploymentHttpRequest {
        deployment_id: request.deployment_id.clone(),
        user_id: request.user_id.clone(),
        account_id: request.account_id.clone(),
        name: request.name.clone(),
        slug: request.slug.clone(),
        target: request.target.clone(),
        archive_name: request.archive_name.clone(),
        archive_bytes_base64: base64::engine::general_purpose::STANDARD
            .encode(&request.archive_bytes),
        region: request.region.clone(),
        backend_type: request.backend_type.clone(),
        image: request.image.clone(),
        ports: request.ports.clone(),
        port_mappings: request.port_mappings.clone(),
        mounts: request.mounts.clone(),
        env: request.env.clone(),
        env_refs: request.env_refs.clone(),
        start_command: request.start_command.clone(),
        working_directory: request.working_directory.clone(),
        setup_hooks: request.setup_hooks.clone(),
        size_class: request.size_class.clone(),
        ttl_seconds: request.ttl_seconds,
        excluded_backend_server_ids: request.excluded_backend_server_ids.clone(),
        metadata: request.metadata.clone(),
    })
    .send()
    .await
    .context("Failed to call cloud deploy API")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud deploy API returned {}: {}", status, body);
    }

    response
        .json::<CreateDeploymentResponse>()
        .await
        .context("Failed to parse cloud deploy API response")
}

pub(crate) async fn create_archive(
    state: &AppState,
    user_id: &str,
    sandbox_id: &str,
    name: Option<String>,
    archive_bytes: Vec<u8>,
) -> Result<CreateArchiveResponse> {
    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/internal/orchestration/archives",
            state.config.cloud_internal_url.trim_end_matches('/'),
        )),
    )
    .json(&CreateArchiveHttpRequest {
        user_id: user_id.to_string(),
        sandbox_id: sandbox_id.to_string(),
        name,
        archive_bytes_base64: base64::engine::general_purpose::STANDARD.encode(&archive_bytes),
    })
    .send()
    .await
    .context("Failed to call cloud archive API")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud archive API returned {}: {}", status, body);
    }

    response
        .json::<CreateArchiveResponse>()
        .await
        .context("Failed to parse cloud archive API response")
}

pub(crate) async fn upsert_service_route(
    state: &AppState,
    request: &UpsertServiceRouteRequest,
) -> Result<UpsertServiceRouteResponse> {
    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/service-routes",
            state.config.backend_api_url.trim_end_matches('/'),
        )),
    )
    .json(request)
    .send()
    .await
    .context("Failed to call cloud service route API")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud service route API returned {}: {}", status, body);
    }

    response
        .json::<UpsertServiceRouteResponse>()
        .await
        .context("Failed to parse cloud service route API response")
}

pub(crate) async fn presign_archive_upload(
    state: &AppState,
    user_id: &str,
    storage_path: Option<&str>,
) -> Result<PresignedArchiveUploadSlot> {
    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/internal/orchestration/archives/presign",
            state.config.cloud_internal_url.trim_end_matches('/'),
        )),
    )
    .json(&PresignArchiveUploadHttpRequest {
        user_id: user_id.to_string(),
        storage_path: storage_path.map(ToOwned::to_owned),
    })
    .send()
    .await
    .context("Failed to call cloud archive presign API")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud archive presign API returned {}: {}", status, body);
    }

    response
        .json::<PresignedArchiveUploadSlot>()
        .await
        .context("Failed to parse cloud archive presign response")
}

pub(crate) async fn finalize_archive_upload(
    state: &AppState,
    archive_id: &str,
    user_id: &str,
    sandbox_id: &str,
    name: Option<String>,
    storage_path: Option<&str>,
) -> Result<FinalizedArchiveRecord> {
    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/internal/orchestration/archives/{}/finalize",
            state.config.cloud_internal_url.trim_end_matches('/'),
            archive_id,
        )),
    )
    .json(&FinalizeArchiveHttpRequest {
        user_id: user_id.to_string(),
        sandbox_id: sandbox_id.to_string(),
        name,
        storage_path: storage_path.map(ToOwned::to_owned),
    })
    .send()
    .await
    .context("Failed to call cloud archive finalize API")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud archive finalize API returned {}: {}", status, body);
    }

    response
        .json::<FinalizedArchiveRecord>()
        .await
        .context("Failed to parse cloud archive finalize response")
}

pub(crate) async fn download_archive(
    state: &AppState,
    archive_id: &str,
    user_id: &str,
) -> Result<Vec<u8>> {
    let response = authorized_request(
        state,
        state.http_client.get(format!(
            "{}/internal/orchestration/archives/{}",
            state.config.cloud_internal_url.trim_end_matches('/'),
            archive_id,
        )),
    )
    .query(&[("userId", user_id)])
    .send()
    .await
    .with_context(|| format!("Failed to call cloud archive download API for {archive_id}"))?;

    if response.status().is_redirection() {
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow::anyhow!("Cloud archive download redirect missing Location"))?;
        let redirected = state
            .http_client
            .get(&location)
            .send()
            .await
            .with_context(|| {
                format!("Failed to follow cloud archive download redirect to {location}")
            })?;
        if !redirected.status().is_success() {
            let status = redirected.status();
            let body = redirected.text().await.unwrap_or_default();
            anyhow::bail!(
                "Cloud archive redirected download returned {}: {}",
                status,
                body
            );
        }
        return redirected
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .context("Failed to read redirected cloud archive download response body");
    }

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud archive download API returned {}: {}", status, body);
    }

    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .context("Failed to read cloud archive download response body")
}

pub(crate) async fn archive_download_url(
    state: &AppState,
    archive_id: &str,
    user_id: &str,
) -> Result<String> {
    let response = authorized_request(
        state,
        state.http_client.get(format!(
            "{}/internal/orchestration/archives/{}",
            state.config.cloud_internal_url.trim_end_matches('/'),
            archive_id,
        )),
    )
    .query(&[("userId", user_id)])
    .send()
    .await
    .with_context(|| format!("Failed to call cloud archive download API for {archive_id}"))?;

    if response.status().is_redirection() {
        return response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow::anyhow!("Cloud archive download redirect missing Location"));
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::bail!("Cloud archive download API did not issue a redirect: {status} {body}");
}

pub(crate) async fn stop_deployment(state: &AppState, deployment_id: &str) -> Result<()> {
    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/sandbox/{}/stop",
            state.config.cloud_internal_url.trim_end_matches('/'),
            deployment_id,
        )),
    )
    .send()
    .await
    .with_context(|| format!("Failed to call cloud sandbox stop API for {deployment_id}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud sandbox stop API returned {}: {}", status, body);
    }

    Ok(())
}

pub(crate) async fn delete_deployment(state: &AppState, deployment_id: &str) -> Result<()> {
    let response = authorized_request(
        state,
        state.http_client.delete(format!(
            "{}/internal/orchestration/deployments/{}",
            state.config.cloud_internal_url.trim_end_matches('/'),
            deployment_id,
        )),
    )
    .send()
    .await
    .with_context(|| format!("Failed to call cloud deployment delete API for {deployment_id}"))?;

    if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud deployment delete API returned {}: {}", status, body);
    }

    Ok(())
}

pub(crate) async fn reconcile_deployment_hosts(
    state: &AppState,
    deployment_ids: &[String],
) -> Result<()> {
    if deployment_ids.is_empty() {
        return Ok(());
    }

    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/internal/orchestration/deployments/reconcile-hosts",
            state.config.cloud_internal_url.trim_end_matches('/'),
        )),
    )
    .json(&ReconcileDeploymentHostsRequest {
        deployment_ids: deployment_ids.to_vec(),
    })
    .send()
    .await
    .context("Failed to call cloud deployment hosts reconcile API")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Cloud deployment hosts reconcile API returned {}: {}",
            status,
            body
        );
    }

    Ok(())
}

pub(crate) async fn deployment_healthcheck_url(
    state: &AppState,
    deployment_id: &str,
) -> Result<Option<String>> {
    let response = authorized_request(
        state,
        state.http_client.get(format!(
            "{}/internal/orchestration/deployments/{}/healthcheck-url",
            state.config.cloud_internal_url.trim_end_matches('/'),
            deployment_id,
        )),
    )
    .send()
    .await
    .with_context(|| format!("Failed to call cloud healthcheck URL API for {deployment_id}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("Deployed sandbox {deployment_id} was not found for healthcheck");
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud healthcheck URL API returned {}: {}", status, body);
    }

    let payload: HealthcheckUrlResponse = response.json().await.with_context(|| {
        format!("Failed to parse cloud healthcheck URL response for {deployment_id}")
    })?;
    Ok(payload.url)
}

pub(crate) async fn exec_in_deployment(
    state: &AppState,
    deployment_id: &str,
    command: &str,
) -> Result<SandboxExecResponse> {
    exec_in_deployment_with_env(state, deployment_id, command, None).await
}

pub(crate) async fn exec_in_deployment_with_env(
    state: &AppState,
    deployment_id: &str,
    command: &str,
    env: Option<HashMap<String, String>>,
) -> Result<SandboxExecResponse> {
    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/sandbox/{}/exec",
            state.config.cloud_internal_url.trim_end_matches('/'),
            deployment_id,
        )),
    )
    .json(&SandboxExecRequest {
        command: command.to_string(),
        env,
    })
    .send()
    .await
    .with_context(|| format!("Failed to call cloud sandbox exec API for {deployment_id}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud sandbox exec API returned {}: {}", status, body);
    }

    response
        .json::<SandboxExecResponse>()
        .await
        .with_context(|| format!("Failed to parse cloud sandbox exec response for {deployment_id}"))
}

pub(crate) async fn start_exec_operation(
    state: &AppState,
    deployment_id: &str,
    request: &SandboxExecOperationStartRequest,
) -> Result<SandboxExecOperationRecord> {
    let response = authorized_request(
        state,
        state.http_client.post(format!(
            "{}/sandbox/{}/exec-operations",
            state.config.cloud_internal_url.trim_end_matches('/'),
            deployment_id,
        )),
    )
    .json(request)
    .send()
    .await
    .with_context(|| format!("Failed to call cloud sandbox async exec API for {deployment_id}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud sandbox async exec API returned {}: {}", status, body);
    }

    response
        .json::<SandboxExecOperationRecord>()
        .await
        .with_context(|| format!("Failed to parse cloud sandbox async exec response for {deployment_id}"))
}

pub(crate) async fn get_exec_operation(
    state: &AppState,
    deployment_id: &str,
    operation_id: &str,
) -> Result<SandboxExecOperationRecord> {
    let response = authorized_request(
        state,
        state.http_client.get(format!(
            "{}/sandbox/{}/exec-operations/{}",
            state.config.cloud_internal_url.trim_end_matches('/'),
            deployment_id,
            operation_id,
        )),
    )
    .send()
    .await
    .with_context(|| format!("Failed to call cloud sandbox async exec status API for {deployment_id}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud sandbox async exec status API returned {}: {}", status, body);
    }

    response
        .json::<SandboxExecOperationRecord>()
        .await
        .with_context(|| format!("Failed to parse cloud sandbox async exec status response for {deployment_id}"))
}

pub(crate) async fn deployment_preflight(
    state: &AppState,
    request: &DeployPreflightRequest,
) -> Result<DeployPreflightResponse> {
    let preflight_url = format!(
        "{}/internal/orchestration/deployments/preflight",
        state.config.cloud_internal_url.trim_end_matches('/'),
    );
    let response = authorized_request(state, state.http_client.post(&preflight_url))
        .json(request)
        .send()
        .await
        .context("Failed to call cloud deploy preflight API")?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Cloud deploy preflight API returned 404 at {}. Verify ORCHESTRATOR_CLOUD_INTERNAL_URL/CLOUD_INTERNAL_URL points to a cloud service that exposes /internal/orchestration/deployments/preflight. Response body: {}",
            preflight_url,
            body
        );
    }

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Cloud deploy preflight API returned {}: {}", status, body);
    }

    response
        .json::<DeployPreflightResponse>()
        .await
        .context("Failed to parse cloud deploy preflight response")
}

fn authorized_request(
    state: &AppState,
    builder: reqwest::RequestBuilder,
) -> reqwest::RequestBuilder {
    builder.header(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", state.config.internal_api_key),
    )
}
