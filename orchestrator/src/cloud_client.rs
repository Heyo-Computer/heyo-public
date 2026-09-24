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
    pub archive_id: Option<String>,
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
    pub deployment_environment: Option<String>,
    pub placement_pool: Option<String>,
    pub excluded_backend_server_ids: Vec<String>,
    pub allowed_backend_server_ids: Option<Vec<String>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_id: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    deployment_environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    placement_pool: Option<String>,
    excluded_backend_server_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_backend_server_ids: Option<Vec<String>>,
    metadata: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeploymentPlacement {
    pub deployment_environment: String,
    pub node_id: String,
    #[serde(default)]
    pub placement_pool: Option<String>,
    pub region: String,
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
    #[serde(default)]
    pub placement: Option<DeploymentPlacement>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeploymentHealthcheckUrls {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    internal_url: Option<String>,
    #[serde(default)]
    public_url: Option<String>,
}

impl DeploymentHealthcheckUrls {
    pub(crate) fn probe_url(&self) -> Option<String> {
        // Readiness is a backend check; the public URL can require end-user
        // authentication even when the candidate itself is healthy.
        [&self.internal_url, &self.url, &self.public_url]
            .into_iter()
            .flatten()
            .map(|url| url.trim())
            .find(|url| !url.is_empty())
            .map(ToOwned::to_owned)
    }

    pub(crate) fn internal_url(&self) -> Option<String> {
        self.internal_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(ToOwned::to_owned)
    }
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

fn deployment_http_request(request: &CreateDeploymentRequest) -> CreateDeploymentHttpRequest {
    let archive_id = request
        .archive_id
        .clone()
        .filter(|_| request.archive_bytes.is_empty());
    CreateDeploymentHttpRequest {
        deployment_id: request.deployment_id.clone(),
        user_id: request.user_id.clone(),
        account_id: request.account_id.clone(),
        name: request.name.clone(),
        slug: request.slug.clone(),
        target: request.target.clone(),
        archive_id: archive_id.clone(),
        archive_name: request.archive_name.clone(),
        archive_bytes_base64: if archive_id.is_some() {
            String::new()
        } else {
            base64::engine::general_purpose::STANDARD.encode(&request.archive_bytes)
        },
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
        deployment_environment: request.deployment_environment.clone(),
        placement_pool: request.placement_pool.clone(),
        excluded_backend_server_ids: request.excluded_backend_server_ids.clone(),
        allowed_backend_server_ids: request.allowed_backend_server_ids.clone(),
        metadata: request.metadata.clone(),
    }
}

/// Persist this fingerprint before sending create; it contains no secret values.
pub(crate) fn deployment_request_digest(request: &CreateDeploymentRequest) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut value = serde_json::to_value(deployment_http_request(request))?;
    // Cloud hashes its parsed request, including absent optional fields as null.
    for key in ["slug", "archiveId", "deploymentEnvironment", "placementPool"] {
        value.as_object_mut().unwrap().entry(key).or_insert(Value::Null);
    }
    value.sort_all_objects();
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&value)?)))
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
    .json(&deployment_http_request(request))
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeploymentCreationReceipt {
    #[serde(flatten)]
    pub deployment: CreateDeploymentResponse,
    pub request_digest: String,
    pub host_local_url: Option<String>,
    pub guest_port: Option<u16>,
}

/// Current retained-runtime observation, never proof that a create was executed.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RetainedDeploymentBinding {
    pub deployment_id: String,
    pub archive_id: String,
    pub backend_server_id: String,
    pub backend_sandbox_id: String,
    pub node_id: String,
    pub region: String,
    pub deployment_environment: String,
    pub placement_pool: String,
    pub guest_port: u16,
    pub host_local_url: String,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

/// Non-starting exact-runtime HTTP transport. Ordinary Cloud exec/proxy is not
/// a fallback: those APIs may wake a retained VM and do not pin its runtime.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExactRuntimeHttpRequest {
    pub expected_backend_server_id: String,
    pub expected_backend_sandbox_id: String,
    pub port: u16,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExactRuntimeHttpResponse {
    pub backend_server_id: String,
    pub backend_sandbox_id: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ExactRuntimeHttpRequest {
    pub(crate) fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.expected_backend_server_id.is_empty()
            && !self.expected_backend_sandbox_id.is_empty() && self.port > 0,
            "exact-runtime HTTP target is incomplete");
        anyhow::ensure!(matches!(self.method.as_str(), "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"), "unsupported exact-runtime HTTP method");
        anyhow::ensure!(self.path.starts_with('/') && !self.path.starts_with("//")
            && !self.path.contains(['#', '\\', '\r', '\n']), "exact-runtime HTTP path must be origin-form");
        for (name, value) in &self.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())?;
            reqwest::header::HeaderValue::from_str(value)?;
            anyhow::ensure!(!matches!(name.as_str(), "host" | "connection" | "keep-alive"
                | "proxy-authenticate" | "proxy-authorization" | "te" | "trailer"
                | "transfer-encoding" | "upgrade" | "content-length"), "invalid exact-runtime HTTP header");
        }
        Ok(())
    }
}

pub(crate) async fn exact_runtime_http(
    state: &AppState, deployment: &str, request: &ExactRuntimeHttpRequest,
    body: reqwest::Body,
) -> Result<(ExactRuntimeHttpResponse, reqwest::Response)> {
    request.validate()?;
    let metadata = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(request)?);
    anyhow::ensure!(metadata.len() <= 16 * 1024, "exact-runtime HTTP request metadata too large");
    anyhow::ensure!(!deployment.is_empty(), "exact-runtime deployment identity missing");
    let mut url = reqwest::Url::parse(&format!("{}/internal/orchestration/deployments/",
        state.config.cloud_internal_url.trim_end_matches('/')))?;
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("Invalid Cloud URL"))?
        .pop_if_empty().push(deployment).push("http-request");
    // Never follow a redirect with the inner application's credentials or
    // replay an ambiguous mutation against another runtime.
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30)).build()?;
    let response = authorized_request(state, client.post(url))
        .header("content-type", "application/octet-stream")
        .header("x-heyo-runtime-request", metadata).body(body).send().await?;
    anyhow::ensure!(response.status() == reqwest::StatusCode::OK, "exact-runtime HTTP transport unavailable ({})", response.status());
    let metadata = response.headers().get("x-heyo-runtime-response")
        .context("exact-runtime HTTP response metadata missing")?.as_bytes();
    anyhow::ensure!(metadata.len() <= 16 * 1024, "exact-runtime HTTP response metadata too large");
    let metadata: ExactRuntimeHttpResponse = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(metadata)?)?;
    anyhow::ensure!(metadata.backend_server_id == request.expected_backend_server_id
        && metadata.backend_sandbox_id == request.expected_backend_sandbox_id,
        "exact-runtime HTTP response identity mismatch");
    let status = reqwest::StatusCode::from_u16(metadata.status)?;
    anyhow::ensure!(!status.is_informational(), "exact-runtime HTTP upgrade is unsupported");
    Ok((metadata, response))
}

pub(crate) async fn observe_retained_deployment(
    state: &AppState, deployment_id: &str, guest_port: u16,
) -> Result<RetainedDeploymentBinding> {
    anyhow::ensure!(guest_port > 0, "A nonzero guest port is required");
    let mut url = reqwest::Url::parse(&format!("{}/internal/orchestration/deployments/",
        state.config.cloud_internal_url.trim_end_matches('/')))?;
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("Invalid Cloud URL"))?
        .pop_if_empty().push(deployment_id).push("binding");
    url.query_pairs_mut().append_pair("port", &guest_port.to_string());
    let binding: RetainedDeploymentBinding = authorized_request(state, state.http_client.get(url))
        .send().await?.error_for_status()?.json().await?;
    anyhow::ensure!(binding.deployment_id == deployment_id && binding.guest_port == guest_port
        && [&binding.archive_id, &binding.backend_server_id, &binding.backend_sandbox_id,
            &binding.node_id, &binding.region, &binding.deployment_environment, &binding.placement_pool]
            .into_iter().all(|value| !value.trim().is_empty()), "Cloud retained binding identity mismatch");
    let local = reqwest::Url::parse(&binding.host_local_url)?;
    anyhow::ensure!(local.scheme() == "http" && local.host_str() == Some("127.0.0.1")
        && local.path() == "/" && local.query().is_none() && local.fragment().is_none()
        && local.username().is_empty() && local.password().is_none()
        && local.port_or_known_default() != Some(0), "Invalid retained host-local mapping");
    Ok(binding)
}

/// Read-only after an uncertain create; never retry creation with a fresh identity.
pub(crate) async fn recover_deployment(
    state: &AppState,
    deployment_id: &str,
    request_digest: &str,
    guest_port: Option<u16>,
) -> Result<DeploymentCreationReceipt> {
    let mut url = reqwest::Url::parse(&format!("{}/internal/orchestration/deployments/",
        state.config.cloud_internal_url.trim_end_matches('/')))?;
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("Invalid Cloud URL"))?
        .pop_if_empty().push(deployment_id);
    if let Some(port) = guest_port {
        url.query_pairs_mut().append_pair("port", &port.to_string());
    }
    let response = authorized_request(state, state.http_client.get(url)).send().await?
        .error_for_status()?;
    let receipt: DeploymentCreationReceipt = response.json().await?;
    anyhow::ensure!(receipt.deployment.deployment_id == deployment_id
        && receipt.request_digest == request_digest, "Cloud creation receipt identity mismatch");
    if let Some(port) = guest_port {
        anyhow::ensure!(receipt.guest_port == Some(port)
            && receipt.deployment.status == "running"
            && receipt.deployment.backend_server_id.is_some()
            && receipt.deployment.backend_sandbox_id.is_some(), "Cloud runtime binding unavailable");
        let local = reqwest::Url::parse(receipt.host_local_url.as_deref()
            .context("Cloud did not attest a host-local mapping")?)?;
        anyhow::ensure!(local.scheme() == "http" && local.host_str() == Some("127.0.0.1")
            && local.path() == "/" && local.query().is_none() && local.fragment().is_none()
            && local.username().is_empty() && local.password().is_none()
            && local.port_or_known_default() != Some(0), "Invalid host-local mapping");
    }
    Ok(receipt)
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

pub(crate) async fn deployment_healthcheck_urls(
    state: &AppState,
    deployment_id: &str,
) -> Result<DeploymentHealthcheckUrls> {
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

    let payload: DeploymentHealthcheckUrls = response.json().await.with_context(|| {
        format!("Failed to parse cloud healthcheck URL response for {deployment_id}")
    })?;
    Ok(payload)
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

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use anyhow::Result;
    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::{json, Value};

    use super::{create_deployment, CreateDeploymentRequest, DeploymentHealthcheckUrls};
    use crate::AppState;

    #[tokio::test]
    async fn exact_runtime_http_streams_and_never_falls_back_or_follows_redirects() -> Result<()> {
        use base64::Engine;
        use axum::{body::{Body, Bytes}, http::{HeaderMap, StatusCode}, response::IntoResponse};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let mode = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/internal/orchestration/deployments/dep-a/http-request", post({
            let calls = calls.clone(); let mode = mode.clone();
            move |headers: HeaderMap, body: Bytes| {
                let calls = calls.clone(); let mode = mode.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(headers["authorization"], "Bearer test");
                    let request: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(headers["x-heyo-runtime-request"].as_bytes()).unwrap()).unwrap();
                    assert_eq!(request["headers"][0], json!(["authorization","Bearer original-caller"]));
                    assert_eq!(request["expectedBackendSandboxId"], "sb-original");
                    assert_eq!(body.as_ref(), b"\0binary\xffbody");
                    match mode.load(Ordering::SeqCst) {
                        1 => StatusCode::NOT_FOUND.into_response(),
                        2 => (StatusCode::TEMPORARY_REDIRECT, [("location", "/ordinary-exec")]).into_response(),
                        value => {
                            let metadata = json!({"backendServerId":"host-a",
                                "backendSandboxId": if value == 3 {"sb-other"} else {"sb-original"},
                                "status":409,"headers":[["content-type","application/octet-stream"]]});
                            axum::response::Response::builder().status(200)
                                .header("x-heyo-runtime-response", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&metadata).unwrap()))
                                .body(Body::from(body)).unwrap()
                        }
                    }
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = serde_json::from_value(json!({"server_port":0,"database_url":"postgres://unused",
            "agent_provider":"test","agent_model":"test","agent_api_key":"","agent_timeout_seconds":1,
            "agent_max_iterations":1,"jwt_secret":"test","cloud_internal_url":base,"internal_api_key":"test"}))?;
        let state = AppState { config: Arc::new(config), http_client: reqwest::Client::new(),
            worker_id: Arc::new("test".into()), ci_workspace_cache: Default::default() };
        let mut request = super::ExactRuntimeHttpRequest { expected_backend_server_id:"host-a".into(),
            expected_backend_sandbox_id:"sb-original".into(), port:8080, method:"POST".into(),
            path:"/api/mutate?x=1".into(), headers:vec![("authorization".into(),"Bearer original-caller".into())] };
        let (metadata, response) = super::exact_runtime_http(&state,"dep-a",&request,b"\0binary\xffbody".to_vec().into()).await?;
        assert_eq!(metadata.status,409);
        assert_eq!(response.bytes().await?.as_ref(),b"\0binary\xffbody");
        for value in 1..=3 {
            mode.store(value,Ordering::SeqCst);
            assert!(super::exact_runtime_http(&state,"dep-a",&request,b"\0binary\xffbody".to_vec().into()).await.is_err());
        }
        assert_eq!(calls.load(Ordering::SeqCst),4);
        for path in ["https://elsewhere/", "//elsewhere/path", "/bad#fragment", "/bad\\path", "/bad\r\nheader"] {
            request.path=path.into(); assert!(request.validate().is_err());
        }
        request.path="/valid?x=1".into();
        for method in ["GET","HEAD","POST","PUT","PATCH","DELETE"] { request.method=method.into(); request.validate()?; }
        for method in ["CONNECT","TRACE"] { request.method=method.into(); assert!(request.validate().is_err()); }
        request.method="GET".into();
        for header in ["Host","Content-Length","Connection","Transfer-Encoding","Upgrade"] {
            request.headers=vec![(header.into(),"value".into())]; assert!(request.validate().is_err());
        }
        server.abort();
        Ok(())
    }

    #[test]
    fn creation_digest_matches_cloud_wire_contract() -> Result<()> {
        let request = CreateDeploymentRequest {
            deployment_id: "dep-candidate".into(), user_id: "operator".into(), account_id: "account".into(),
            name: "candidate".into(), slug: None, target: "service".into(), archive_id: Some("archive-a".into()),
            archive_name: None, archive_bytes: vec![], region: "eu1".into(), backend_type: "libvirt".into(),
            image: "ubuntu".into(), ports: vec![], port_mappings: vec![], mounts: vec![],
            env: Some(HashMap::from([("Z".into(), "last".into()), ("A".into(), "first".into())])),
            env_refs: vec![], start_command: None, working_directory: None, setup_hooks: None,
            size_class: "small".into(), ttl_seconds: None, deployment_environment: None,
            placement_pool: None, excluded_backend_server_ids: vec![], metadata: Some(json!({"operationId": "operation-a"})),
            allowed_backend_server_ids: None,
        };
        // Independently derived SHA-256 of sorted, compact JSON with explicit defaults;
        // the private Cloud request test uses the same wire fixture.
        assert_eq!(super::deployment_request_digest(&request)?,
            "cd2a152a5d5f791413cc10c4d8073cd8d2317428c1dfde1cdc5214efe6aa84cf");
        Ok(())
    }

    #[tokio::test]
    async fn recovery_is_read_only_and_rejects_wrong_identity_or_mapping() -> Result<()> {
        let payload = Arc::new(tokio::sync::Mutex::new(json!({
            "deploymentId":"dep-a", "requestDigest":"digest-a", "status":"provisioning"
        })));
        let app = Router::new().route("/internal/orchestration/deployments/{id}",
            axum::routing::get(|State(payload): State<Arc<tokio::sync::Mutex<Value>>>, headers: axum::http::HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer test");
                Json(payload.lock().await.clone())
            })).route("/internal/orchestration/deployments/{id}/binding",
            axum::routing::get(|State(payload): State<Arc<tokio::sync::Mutex<Value>>>, headers: axum::http::HeaderMap,
                axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String,String>>| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer test");
                assert!(query.contains_key("port"));
                Json(payload.lock().await.clone())
            })).with_state(payload.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = serde_json::from_value(json!({
            "server_port":0,"database_url":"postgres://unused","agent_provider":"test",
            "agent_model":"test","agent_api_key":"","agent_timeout_seconds":1,
            "agent_max_iterations":1,"jwt_secret":"test","cloud_internal_url":base_url,
            "internal_api_key":"test"
        }))?;
        let state = AppState { config: Arc::new(config), http_client: reqwest::Client::new(),
            worker_id: Arc::new("test".into()), ci_workspace_cache: Default::default() };
        let receipt = super::recover_deployment(&state, "dep-a", "digest-a", None).await?;
        assert_eq!(receipt.deployment.status, "provisioning");
        assert!(super::recover_deployment(&state, "dep-a", "digest-other", None).await.is_err());
        assert!(super::recover_deployment(&state, "dep-other", "digest-a", None).await.is_err());
        assert!(super::recover_deployment(&state, "dep-a", "digest-a", Some(8080)).await.is_err());
        *payload.lock().await = json!({"deploymentId":"dep-a", "requestDigest":"digest-a", "status":"running",
            "backendServerId":"eu1-host", "backendSandboxId":"sb-exact", "guestPort":8080,
            "hostLocalUrl":"http://127.0.0.1:18081"});
        assert_eq!(super::recover_deployment(&state, "dep-a", "digest-a", Some(8080)).await?
            .deployment.backend_sandbox_id.as_deref(), Some("sb-exact"));
        assert!(super::recover_deployment(&state, "dep-a", "digest-a", Some(9090)).await.is_err());
        payload.lock().await["hostLocalUrl"] = json!("http://10.0.0.2:18081");
        assert!(super::recover_deployment(&state, "dep-a", "digest-a", Some(8080)).await.is_err());
        assert!(super::observe_retained_deployment(&state, "dep-a", 8080).await.is_err());
        let retained = json!({"deploymentId":"dep-a","archiveId":"archive-old",
            "backendServerId":"us-host","backendSandboxId":"sb-retained","nodeId":"node-us",
            "region":"US","deploymentEnvironment":"production","placementPool":"platform",
            "guestPort":8080,"hostLocalUrl":"http://127.0.0.1:18081","observedAt":"2026-09-23T00:00:00Z"});
        *payload.lock().await = retained.clone();
        assert_eq!(super::observe_retained_deployment(&state, "dep-a", 8080).await?.archive_id, "archive-old");
        assert!(super::recover_deployment(&state, "dep-a", "digest-a", None).await.is_err(),
            "a retained observation is not creation evidence");
        assert!(super::observe_retained_deployment(&state, "dep-other", 8080).await.is_err());
        assert!(super::observe_retained_deployment(&state, "dep-a", 9090).await.is_err());
        assert!(super::observe_retained_deployment(&state, "dep-a", 0).await.is_err());
        for (key,value) in [("nodeId",json!("")), ("region",Value::Null),
            ("hostLocalUrl",json!("http://remote.example:18081")),
            ("hostLocalUrl",json!("http://127.0.0.1:18081/other"))] {
            let mut bad = retained.clone(); bad[key] = value;
            *payload.lock().await = bad;
            assert!(super::observe_retained_deployment(&state, "dep-a", 8080).await.is_err(), "{key}");
        }
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn create_deployment_sends_environment_without_placement_pool() -> Result<()> {
        let (body_tx, body_rx) = tokio::sync::oneshot::channel();
        let sender = Arc::new(tokio::sync::Mutex::new(Some(body_tx)));
        let app = Router::new()
            .route("/internal/orchestration/deployments", post(|State(sender): State<Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<Value>>>>>, Json(body): Json<Value>| async move {
                sender.lock().await.take().unwrap().send(body).unwrap();
                Json(json!({"deploymentId":"dep-1","status":"running","placement":{"deploymentEnvironment":"production","nodeId":"node-1","region":"US"}}))
            }))
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = serde_json::from_value(json!({
            "server_port":0,"database_url":"postgres://unused","agent_provider":"test",
            "agent_model":"test","agent_api_key":"","agent_timeout_seconds":1,
            "agent_max_iterations":1,"jwt_secret":"test","cloud_internal_url":base_url,
            "internal_api_key":"test"
        }))?;
        let state = AppState { config: Arc::new(config), http_client: reqwest::Client::new(),
            worker_id: Arc::new("test".into()), ci_workspace_cache: Default::default() };
        let response = create_deployment(&state, &CreateDeploymentRequest {
            deployment_id: "dep-1".into(), user_id: "user".into(), account_id: "account".into(),
            name: "service".into(), slug: None, target: "linux".into(), archive_id: Some("archive".into()),
            archive_name: None, archive_bytes: vec![], region: "US".into(), backend_type: "libvirt".into(),
            image: "image".into(), ports: vec![8080], port_mappings: vec![], mounts: vec![],
            env: Some(HashMap::new()), env_refs: vec![], start_command: None, working_directory: None,
            setup_hooks: None, size_class: "small".into(), ttl_seconds: None,
            deployment_environment: Some("production".into()), placement_pool: None,
            excluded_backend_server_ids: vec![], metadata: None,
            allowed_backend_server_ids: Some(vec!["pinned-host".into()]),
        }).await?;
        let body = body_rx.await?;
        assert_eq!(body["allowedBackendServerIds"], json!(["pinned-host"]));
        assert_eq!(body["deploymentEnvironment"], "production");
        assert!(body.get("placementPool").is_none());
        assert_eq!(response.placement.unwrap().deployment_environment, "production");
        server.abort();
        Ok(())
    }

    #[test]
    fn preserves_distinct_internal_and_public_healthcheck_urls() {
        let urls: DeploymentHealthcheckUrls = serde_json::from_value(serde_json::json!({
            "url": "https://legacy-candidate.stage.heyo.computer",
            "internalUrl": "http://10.88.0.1:2238",
            "publicUrl": "https://candidate.stage.heyo.computer"
        }))
        .unwrap();

        assert_eq!(
            urls.internal_url().as_deref(),
            Some("http://10.88.0.1:2238")
        );
        assert_eq!(
            urls.probe_url().as_deref(),
            Some("http://10.88.0.1:2238")
        );
    }

    #[test]
    fn falls_back_when_internal_healthcheck_url_is_blank() {
        let urls: DeploymentHealthcheckUrls = serde_json::from_value(serde_json::json!({
            "internalUrl": "  ",
            "url": " http://backend.example:2238 ",
            "publicUrl": "https://candidate.stage.heyo.computer"
        }))
        .unwrap();

        assert_eq!(urls.probe_url().as_deref(), Some("http://backend.example:2238"));
    }

    #[test]
    fn supports_legacy_healthcheck_url_responses() {
        let urls: DeploymentHealthcheckUrls = serde_json::from_value(serde_json::json!({
            "url": "https://candidate.stage.heyo.computer"
        }))
        .unwrap();

        assert_eq!(urls.internal_url(), None);
        assert_eq!(
            urls.probe_url().as_deref(),
            Some("https://candidate.stage.heyo.computer")
        );
    }
}
