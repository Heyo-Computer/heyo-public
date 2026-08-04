use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use futures::StreamExt;
use heyosecret_client::{HeyoSecretClient, HeyoSecretClientOptions};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::Command;
use tracing::{error, warn};

use crate::auth;
use crate::cloud_client::{
    archive_download_url, create_archive, create_deployment, delete_deployment, download_archive,
    exec_in_deployment_with_env, finalize_archive_upload, get_exec_operation,
    presign_archive_upload, start_exec_operation, stop_deployment, CreateDeploymentRequest,
    PortMapping, SandboxExecOperationStartRequest,
};
use crate::db;
use crate::orchestration::{
    builtin_templates, find_builtin_template, spawn_execute_until_blocked, DEFAULT_TEMPLATE_VERSION,
};
use crate::repositories::{OrchestrationRepository, ToolCallLogCreateInput};
use crate::AppState;

const DEFAULT_RESOURCE_READY_TIMEOUT_SECONDS: u64 = 900;
const RESOURCE_READY_STATUS_POLL_INTERVAL_SECONDS: u64 = 5;
const RESOURCE_READY_TIMEOUT_ENV: &str = "ORCHESTRATOR_RESOURCE_READY_TIMEOUT_SECONDS";
const DEFAULT_GIT_AUTH_TOKEN_SECRET_PATH: &str = "cicd/git-auth-token";
const DEFAULT_GIT_SSH_PRIVATE_KEY_SECRET_PATH: &str = "cicd/git-ssh-private-key";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateThreadRequest {
    pub title: Option<String>,
    pub context: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileParentJobRequest {
    pub thread_id: String,
    pub template_id: String,
    pub template_version: Option<i32>,
    pub goal: String,
    pub target: Option<String>,
    pub inputs: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecideApprovalRequest {
    pub decision: String,
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestedCiArtifactUpload {
    pub kind: String,
    #[serde(default)]
    pub storage_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiArtifactPresignRequest {
    pub artifacts: Vec<RequestedCiArtifactUpload>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CiArtifactUploadSlot {
    pub kind: String,
    pub archive_id: String,
    pub upload_url: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiRepositoryRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiFinalizeArtifactRequest {
    pub kind: String,
    pub archive_id: String,
    #[serde(default)]
    pub base_rev: String,
    #[serde(default)]
    pub storage_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiArtifactFinalizeRequest {
    pub repository: CiRepositoryRef,
    pub after: String,
    pub artifacts: Vec<CiFinalizeArtifactRequest>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CiFinalizedArtifact {
    pub kind: String,
    pub archive_id: String,
    pub user_id: String,
    pub s3_key: String,
    pub size_bytes: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub base_rev: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CiArtifactFinalizeResponse {
    pub artifacts: Vec<CiFinalizedArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceArchiveCreateRequest {
    pub sandbox_id: String,
    pub name: Option<String>,
    pub archive_bytes_base64: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceArchiveDownloadQuery {
    #[serde(default)]
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcePortMapping {
    pub host: u16,
    pub container: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDeploymentCreateRequest {
    pub deployment_id: String,
    #[serde(default)]
    pub owner_user_id: Option<String>,
    #[serde(default)]
    pub owner_account_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default = "default_resource_target")]
    pub target: String,
    #[serde(default)]
    pub archive_name: Option<String>,
    #[serde(default)]
    pub archive_bytes_base64: String,
    pub region: String,
    #[serde(alias = "backendType")]
    pub driver: String,
    pub image: String,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub port_mappings: Vec<ResourcePortMapping>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub env_refs: Vec<String>,
    #[serde(default)]
    pub start_command: Option<String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub setup_hooks: Option<Vec<String>>,
    pub size_class: String,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDeploymentExecRequest {
    pub command: String,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDeploymentExecOperationRequest {
    pub operation_id: String,
    pub command: String,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub callback_url: Option<String>,
    #[serde(default)]
    pub callback_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadArtifactCreateRequest {
    pub kind: String,
    #[serde(default = "default_artifact_format")]
    pub format: String,
    #[serde(default = "default_artifact_schema_version")]
    pub schema_version: i32,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SandboxLifecycleEnvelope {
    event_type: String,
    payload: SandboxLifecyclePayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandboxLifecyclePayload {
    deployment_id: String,
    status: String,
    #[serde(default)]
    error: Option<String>,
}

fn default_resource_target() -> String {
    "heyo-sandbox".to_string()
}

fn deployment_owner_from_claims(
    claims: &auth::Claims,
    owner_user_id: Option<&str>,
    owner_account_id: Option<&str>,
) -> (String, String) {
    let service_override_allowed = claims.role.as_deref() == Some("service");
    let user_id = if service_override_allowed {
        owner_user_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| claims.user_id.clone())
    } else {
        claims.user_id.clone()
    };
    let account_id = if service_override_allowed {
        owner_account_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| claims.account_id.clone())
            .unwrap_or_else(|| "local".to_string())
    } else {
        claims.account_id.clone().unwrap_or_else(|| "local".to_string())
    };

    (user_id, account_id)
}

fn default_artifact_format() -> String {
    "json".to_string()
}

fn default_artifact_schema_version() -> i32 {
    1
}

pub async fn list_templates(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> (StatusCode, Json<Value>) {
    if auth::extract_bearer_token(&headers, &state.config.jwt_secret).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "templates": builtin_templates(),
        })),
    )
}

pub async fn create_thread(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<CreateThreadRequest>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            error!("Database not available: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    match repo
        .create_thread(
            &claims.user_id,
            req.title,
            req.context.unwrap_or_else(|| json!({})),
        )
        .await
    {
        Ok(thread) => (
            StatusCode::CREATED,
            Json(json!({
                "thread": thread,
            })),
        ),
        Err(e) => {
            error!("Failed to create orchestration thread: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "Failed to create orchestration thread",
                    "message": e.to_string(),
                })),
            )
        }
    }
}

pub async fn presign_ci_artifact_uploads(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<CiArtifactPresignRequest>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    if req.artifacts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "At least one artifact upload must be requested" })),
        );
    }

    let mut artifacts = Vec::with_capacity(req.artifacts.len());
    for artifact in req.artifacts {
        match presign_archive_upload(&state, &claims.user_id, artifact.storage_path.as_deref()).await {
            Ok(slot) => artifacts.push(CiArtifactUploadSlot {
                kind: artifact.kind,
                archive_id: slot.archive_id,
                upload_url: slot.upload_url,
            }),
            Err(error) => {
                error!("Failed to presign CI artifact upload: {}", error);
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": "Failed to prepare artifact upload",
                        "message": error.to_string(),
                    })),
                );
            }
        }
    }

    (StatusCode::OK, Json(json!({ "artifacts": artifacts })))
}

pub async fn presign_resource_archives(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<CiArtifactPresignRequest>,
) -> (StatusCode, Json<Value>) {
    presign_ci_artifact_uploads(headers, State(state), Json(req)).await
}

pub async fn finalize_ci_artifacts(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<CiArtifactFinalizeRequest>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    if req.artifacts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "At least one artifact must be finalized" })),
        );
    }

    let short_sha = req.after.chars().take(7).collect::<String>();
    let mut artifacts = Vec::with_capacity(req.artifacts.len());
    for artifact in req.artifacts {
        match finalize_archive_upload(
            &state,
            &artifact.archive_id,
            &claims.user_id,
            &format!("cicd:{}:{}:{}", req.repository.id, short_sha, artifact.kind),
            Some(format!(
                "{} {} {}",
                req.repository.name, short_sha, artifact.kind
            )),
            artifact.storage_path.as_deref(),
        )
        .await
        {
            Ok(finalized) => artifacts.push(CiFinalizedArtifact {
                kind: artifact.kind,
                archive_id: finalized.id,
                user_id: claims.user_id.clone(),
                s3_key: finalized.s3_key,
                size_bytes: finalized.size_bytes,
                base_rev: artifact.base_rev,
            }),
            Err(error) => {
                error!("Failed to finalize CI artifact: {}", error);
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": "Failed to finalize artifact",
                        "message": error.to_string(),
                    })),
                );
            }
        }
    }

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(CiArtifactFinalizeResponse { artifacts })
                .unwrap_or_else(|_| json!({})),
        ),
    )
}

pub async fn finalize_resource_archives(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<CiArtifactFinalizeRequest>,
) -> (StatusCode, Json<Value>) {
    finalize_ci_artifacts(headers, State(state), Json(req)).await
}

pub async fn create_resource_archive(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<ResourceArchiveCreateRequest>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    if req.sandbox_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "sandboxId is required" })),
        );
    }

    let archive_bytes = match base64::engine::general_purpose::STANDARD
        .decode(req.archive_bytes_base64.as_bytes())
    {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "Invalid archive payload",
                    "message": error.to_string(),
                })),
            )
        }
    };

    match create_archive(
        &state,
        &claims.user_id,
        &req.sandbox_id,
        req.name,
        archive_bytes,
    )
    .await
    {
        Ok(response) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(response).unwrap_or_else(|_| json!({}))),
        ),
        Err(error) => {
            error!("Failed to create orchestrated archive: {}", error);
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to create archive",
                    "message": error.to_string(),
                })),
            )
        }
    }
}

pub async fn download_resource_archive(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(archive_id): Path<String>,
    Query(query): Query<ResourceArchiveDownloadQuery>,
) -> Response {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))).into_response(),
    };

    let requested_user_id = query.user_id.unwrap_or_else(|| claims.user_id.clone());
    if requested_user_id != claims.user_id && claims.role.as_deref() != Some("service") {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "Forbidden" }))).into_response();
    }

    match download_archive(&state, &archive_id, &requested_user_id).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            )],
            bytes,
        )
            .into_response(),
        Err(error) => {
            error!(
                "Failed to download orchestrated archive {}: {}",
                archive_id, error
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to download archive",
                    "message": error.to_string(),
                })),
            )
                .into_response()
        }
    }
}

pub async fn get_resource_archive_download_url(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(archive_id): Path<String>,
    Query(query): Query<ResourceArchiveDownloadQuery>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let requested_user_id = query.user_id.unwrap_or_else(|| claims.user_id.clone());
    if requested_user_id != claims.user_id && claims.role.as_deref() != Some("service") {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "Forbidden" })));
    }

    match archive_download_url(&state, &archive_id, &requested_user_id).await {
        Ok(download_url) => (StatusCode::OK, Json(json!({ "downloadUrl": download_url }))),
        Err(error) => {
            error!("Failed to prepare archive download URL: {}", error);
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to prepare archive download URL",
                    "message": error.to_string(),
                })),
            )
        }
    }
}

pub async fn create_resource_deployment(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<ResourceDeploymentCreateRequest>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let archive_bytes = match base64::engine::general_purpose::STANDARD
        .decode(req.archive_bytes_base64.as_bytes())
    {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "Invalid archive payload",
                    "message": error.to_string(),
                })),
            )
        }
    };

    let (user_id, account_id) = deployment_owner_from_claims(
        &claims,
        req.owner_user_id.as_deref(),
        req.owner_account_id.as_deref(),
    );

    let request = CreateDeploymentRequest {
        deployment_id: req.deployment_id,
        user_id,
        account_id,
        name: req.name,
        slug: req.slug,
        target: req.target,
        archive_name: req.archive_name,
        archive_bytes,
        region: req.region,
        backend_type: req.driver,
        image: req.image,
        ports: req.ports,
        port_mappings: req
            .port_mappings
            .into_iter()
            .map(|mapping| PortMapping {
                host: mapping.host,
                container: mapping.container,
            })
            .collect(),
        mounts: Vec::new(),
        env: req.env,
        env_refs: req.env_refs,
        start_command: req.start_command,
        working_directory: req.working_directory,
        setup_hooks: req.setup_hooks,
        size_class: req.size_class,
        ttl_seconds: req.ttl_seconds,
        metadata: req.metadata,
    };

    match create_deployment(&state, &request).await {
        Ok(response) => (
            StatusCode::ACCEPTED,
            Json(serde_json::to_value(response).unwrap_or_else(|_| json!({}))),
        ),
        Err(error) => {
            error!("Failed to create orchestrated deployment: {}", error);
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to create deployment",
                    "message": error.to_string(),
                })),
            )
        }
    }
}

pub async fn exec_resource_deployment(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(deployment_id): Path<String>,
    Json(req): Json<ResourceDeploymentExecRequest>,
) -> (StatusCode, Json<Value>) {
    if auth::extract_bearer_token(&headers, &state.config.jwt_secret).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }

    match exec_in_deployment_with_env(&state, &deployment_id, &req.command, req.env).await {
        Ok(response) => (
            StatusCode::OK,
            Json(serde_json::to_value(response).unwrap_or_else(|_| json!({}))),
        ),
        Err(error) => {
            error!(
                "Failed to exec in orchestrated deployment {}: {}",
                deployment_id, error
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to exec in deployment",
                    "message": error.to_string(),
                })),
            )
        }
    }
}

pub async fn start_resource_deployment_exec_operation(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(deployment_id): Path<String>,
    Json(req): Json<ResourceDeploymentExecOperationRequest>,
) -> (StatusCode, Json<Value>) {
    if auth::extract_bearer_token(&headers, &state.config.jwt_secret).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }

    let request = SandboxExecOperationStartRequest {
        operation_id: req.operation_id,
        command: req.command,
        env: req.env,
        callback_url: req.callback_url,
        callback_token: req.callback_token,
    };
    match start_exec_operation(&state, &deployment_id, &request).await {
        Ok(response) => (
            StatusCode::ACCEPTED,
            Json(serde_json::to_value(response).unwrap_or_else(|_| json!({}))),
        ),
        Err(error) => {
            error!(
                "Failed to start async exec in orchestrated deployment {}: {}",
                deployment_id, error
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to start async exec in deployment",
                    "message": error.to_string(),
                })),
            )
        }
    }
}

pub async fn get_resource_deployment_exec_operation(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path((deployment_id, operation_id)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    if auth::extract_bearer_token(&headers, &state.config.jwt_secret).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }

    match get_exec_operation(&state, &deployment_id, &operation_id).await {
        Ok(response) => (
            StatusCode::OK,
            Json(serde_json::to_value(response).unwrap_or_else(|_| json!({}))),
        ),
        Err(error) => {
            error!(
                "Failed to get async exec in orchestrated deployment {}: {}",
                deployment_id, error
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to get async exec in deployment",
                    "message": error.to_string(),
                })),
            )
        }
    }
}

pub async fn stop_resource_deployment(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(deployment_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if auth::extract_bearer_token(&headers, &state.config.jwt_secret).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }

    match stop_deployment(&state, &deployment_id).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "stopped" }))),
        Err(error) => {
            error!(
                "Failed to stop orchestrated deployment {}: {}",
                deployment_id, error
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to stop deployment",
                    "message": error.to_string(),
                })),
            )
        }
    }
}

pub async fn wait_resource_deployment_ready(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(deployment_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if auth::extract_bearer_token(&headers, &state.config.jwt_secret).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }

    let wait_result = if state.config.nats.enabled {
        match subscribe_to_sandbox_lifecycle_events(&state).await {
            Ok(mut subscriber) => {
                wait_for_resource_deployment_ready_on_subscription(&mut subscriber, &deployment_id)
                    .await
            }
            Err(error) => {
                warn!(
                    "Failed to subscribe to deployment lifecycle events; falling back to polling for {}: {}",
                    deployment_id, error
                );
                wait_for_resource_deployment_ready_by_polling(&deployment_id).await
            }
        }
    } else {
        warn!(
            "NATS disabled; polling deployment status while waiting for {} readiness",
            deployment_id
        );
        wait_for_resource_deployment_ready_by_polling(&deployment_id).await
    };

    match wait_result {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "running" }))),
        Err(error) => {
            (StatusCode::BAD_GATEWAY, Json(json!({
                "error": "Deployment did not become ready",
                "message": error,
            })))
        }
    }
}

pub async fn delete_resource_deployment(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(deployment_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if auth::extract_bearer_token(&headers, &state.config.jwt_secret).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }

    match delete_deployment(&state, &deployment_id).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "deleted" }))),
        Err(error) => {
            error!(
                "Failed to delete orchestrated deployment {}: {}",
                deployment_id, error
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "Failed to delete deployment",
                    "message": error.to_string(),
                })),
            )
        }
    }
}

pub async fn get_thread(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            error!("Database not available: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    match repo.find_thread_for_user(&thread_id, &claims.user_id).await {
        Ok(Some(thread)) => (StatusCode::OK, Json(json!({ "thread": thread }))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Thread not found" })),
        ),
        Err(e) => {
            error!("Failed to fetch orchestration thread: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to fetch orchestration thread" })),
            )
        }
    }
}

pub async fn get_thread_timeline(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            error!("Database not available: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    match repo
        .find_thread_timeline_for_user(&thread_id, &claims.user_id)
        .await
    {
        Ok(Some(timeline)) => (
            StatusCode::OK,
            Json(json!({
                "thread": timeline.thread,
                "messages": timeline.messages,
                "toolCalls": timeline.tool_call_logs,
                "artifacts": timeline.artifacts,
            })),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Thread not found" })),
        ),
        Err(e) => {
            error!("Failed to fetch orchestration thread timeline: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to fetch orchestration thread timeline" })),
            )
        }
    }
}

pub async fn create_thread_artifact(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(req): Json<ThreadArtifactCreateRequest>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let kind = req.kind.trim();
    if kind.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Artifact kind is required" })),
        );
    }

    let format = req.format.trim();
    if format.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Artifact format is required" })),
        );
    }

    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            error!("Database not available: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    let Some(thread) = (match repo.find_thread_for_user(&thread_id, &claims.user_id).await {
        Ok(thread) => thread,
        Err(e) => {
            error!("Failed to validate orchestration thread: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to validate orchestration thread" })),
            );
        }
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Thread not found" })),
        );
    };

    let artifact = match repo
        .create_artifact(
            &thread.id,
            None,
            None,
            kind,
            format,
            req.schema_version,
            req.title,
            req.body,
            req.metadata.clone(),
        )
        .await
    {
        Ok(artifact) => artifact,
        Err(e) => {
            error!("Failed to create orchestration thread artifact: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to persist artifact" })),
            );
        }
    };

    let mut message = None;
    if let Some(content) = req
        .message
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        match repo
            .create_message(
                &thread.id,
                "assistant",
                json!(content),
                Some(json!({
                    "artifactId": artifact.id,
                    "artifactKind": artifact.kind,
                })),
            )
            .await
        {
            Ok(created) => message = Some(created),
            Err(e) => warn!("Failed to create artifact summary message: {}", e),
        }
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "artifact": artifact,
            "message": message,
        })),
    )
}

pub async fn compile_parent_job(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<CompileParentJobRequest>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let version = req.template_version.unwrap_or(DEFAULT_TEMPLATE_VERSION);
    let Some(template) = find_builtin_template(&req.template_id, version) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "Template not found",
                "templateId": req.template_id,
                "templateVersion": version,
            })),
        );
    };

    let target = req.target.unwrap_or_else(|| "heyo-sandbox".to_string());
    if !template
        .policy
        .allowed_targets
        .iter()
        .any(|value| value == &target)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "Target not allowed by template policy",
                "target": target,
            })),
        );
    }

    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            error!("Database not available: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    let Some(thread) = (match repo
        .find_thread_for_user(&req.thread_id, &claims.user_id)
        .await
    {
        Ok(thread) => thread,
        Err(e) => {
            error!("Failed to validate orchestration thread: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to validate orchestration thread" })),
            );
        }
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Thread not found" })),
        );
    };

    if let Err(e) = repo.ensure_template(&template).await {
        error!("Failed to ensure orchestration template: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "Failed to persist orchestration template" })),
        );
    }

    let mut inputs = req.inputs;
    if let Value::Object(ref mut map) = inputs {
        map.entry("userId".to_string())
            .or_insert_with(|| Value::String(claims.user_id.clone()));
        if let Some(account_id) = claims.account_id.clone() {
            map.entry("accountId".to_string())
                .or_insert_with(|| Value::String(account_id));
        }
        map.entry("deployTarget".to_string())
            .or_insert_with(|| Value::String(target.clone()));
    }

    let repo_archive_id = read_string_input(&inputs, "repoArchiveId");
    if let Some(repo_root) = read_repo_root_input(&inputs) {
        if looks_like_git_url(&repo_root) {
            let clone_started_at: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
            let materialized = if let Some(repo_archive_id) = repo_archive_id.as_deref() {
                materialize_repository_archive_for_thread(
                    &state,
                    &claims.user_id,
                    &state.config.service_state_dir,
                    &thread.id,
                    repo_archive_id,
                )
                .await
            } else {
                clone_repository_for_thread(
                    &state,
                    &state.config.service_state_dir,
                    &thread.id,
                    &repo_root,
                )
                .await
            };
            match materialized {
                Ok(clone_path) => {
                    let clone_completed_at: chrono::DateTime<chrono::FixedOffset> =
                        chrono::Utc::now().into();
                    maybe_record_tool_call(
                        &repo,
                        ToolCallLogCreateInput {
                            thread_id: thread.id.clone(),
                            workflow_run_id: None,
                            step_run_id: None,
                            tool_name: "git.clone".to_string(),
                            input: json!({
                                "repositoryUrl": repo_root,
                                "repoArchiveId": repo_archive_id,
                            }),
                            output: Some(json!({
                                "repoRoot": clone_path,
                            })),
                            status: "completed".to_string(),
                            started_at: clone_started_at,
                            completed_at: Some(clone_completed_at),
                        },
                    )
                    .await;
                    if let Value::Object(ref mut map) = inputs {
                        map.entry("repositorySource".to_string())
                            .or_insert_with(|| Value::String(repo_root));
                        map.insert("repoRoot".to_string(), Value::String(clone_path));
                    }
                }
                Err(e) => {
                    maybe_record_tool_call(
                        &repo,
                        ToolCallLogCreateInput {
                            thread_id: thread.id.clone(),
                            workflow_run_id: None,
                            step_run_id: None,
                            tool_name: "git.clone".to_string(),
                            input: json!({
                                "repositoryUrl": repo_root,
                                "repoArchiveId": repo_archive_id,
                            }),
                            output: Some(json!({
                                "error": e,
                            })),
                            status: "failed".to_string(),
                            started_at: clone_started_at,
                            completed_at: Some(chrono::Utc::now().into()),
                        },
                    )
                    .await;
                    error!("Failed to clone orchestration repository: {}", e);
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": "Failed to clone repository for orchestration",
                            "message": e,
                        })),
                    );
                }
            }
        }
    }

    maybe_record_message(
        &repo,
        &thread.id,
        "user",
        req.goal.clone(),
        Some(json!({
            "templateId": req.template_id,
            "templateVersion": version,
            "appName": inputs.get("appName").cloned().unwrap_or(Value::Null),
            "repository": inputs
                .get("repositorySource")
                .cloned()
                .or_else(|| inputs.get("repoRoot").cloned())
                .unwrap_or(Value::Null),
        })),
    )
    .await;

    match repo
        .create_compiled_parent_job(&thread, &template, req.goal, target, inputs)
        .await
    {
        Ok((parent_job, step_runs)) => {
            let _executor = spawn_execute_until_blocked(
                state.clone(),
                OrchestrationRepository::new(db.clone()),
                parent_job.id.clone(),
            );
            let details = crate::repositories::ParentJobDetails {
                workflow_run: parent_job,
                step_runs,
                artifacts: Vec::new(),
                approvals: Vec::new(),
            };
            (
                StatusCode::CREATED,
                Json(parent_job_response(&details, Some(template))),
            )
        }
        Err(e) => {
            error!("Failed to compile parent job: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "Failed to compile parent job",
                    "message": e.to_string(),
                })),
            )
        }
    }
}

pub async fn get_backend_capabilities(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> (StatusCode, Json<Value>) {
    if auth::extract_bearer_token(&headers, &state.config.jwt_secret).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }
    let caps = crate::orchestration::current_backend_caps();
    let default_driver = caps.supported_drivers.first().cloned().unwrap_or_default();
    (
        StatusCode::OK,
        Json(json!({
            "targetOs": caps.target_os,
            "supportedDrivers": caps.supported_drivers,
            "archiveSupportedDrivers": caps.archive_supported_drivers,
            "defaultDriver": default_driver,
            "supportsWaitReady": state.config.nats.enabled,
        })),
    )
}

async fn subscribe_to_sandbox_lifecycle_events(
    state: &AppState,
) -> Result<async_nats::Subscriber, String> {
    let client = async_nats::connect(&state.config.nats.url)
        .await
        .map_err(|error| {
            format!(
                "Failed to connect to NATS at {}: {error}",
                state.config.nats.url
            )
        })?;
    client
        .subscribe("sandbox.evt.>")
        .await
        .map_err(|error| format!("Failed to subscribe to sandbox lifecycle events: {error}"))
}

async fn current_resource_deployment_status(
    deployment_id: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    let db = db::get_db().map_err(|error| error.to_string())?;
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT status, error_message FROM deployed_sandboxes WHERE id = $1 LIMIT 1",
            [deployment_id.into()],
        ))
        .await
        .map_err(|error| format!("Failed to query deployment status: {error}"))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let status: String = row
        .try_get("", "status")
        .map_err(|error| format!("Failed to read deployment status: {error}"))?;
    let error_message: Option<String> = row
        .try_get("", "error_message")
        .map_err(|error| format!("Failed to read deployment error message: {error}"))?;

    Ok(Some((status, error_message)))
}

async fn deployment_status_is_ready(deployment_id: &str) -> Result<bool, String> {
    match current_resource_deployment_status(deployment_id).await? {
        Some((status, _)) if status == "running" => Ok(true),
        Some((status, error_message)) if status == "failed" => {
            Err(error_message
                .unwrap_or_else(|| format!("deployment {deployment_id} is marked failed")))
        }
        _ => Ok(false),
    }
}

async fn wait_for_resource_deployment_ready_by_polling(deployment_id: &str) -> Result<(), String> {
    let wait = async {
        loop {
            if deployment_status_is_ready(deployment_id).await? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(
                RESOURCE_READY_STATUS_POLL_INTERVAL_SECONDS,
            ))
            .await;
        }
    };

    let timeout_seconds = resource_ready_timeout_seconds();
    tokio::time::timeout(Duration::from_secs(timeout_seconds), wait)
        .await
        .map_err(|_| {
            format!(
                "Timed out after {}s waiting for sandbox {} to become ready by polling",
                timeout_seconds, deployment_id
            )
        })?
}

async fn wait_for_resource_deployment_ready_on_subscription(
    subscriber: &mut async_nats::Subscriber,
    deployment_id: &str,
) -> Result<(), String> {
    if deployment_status_is_ready(deployment_id).await? {
        return Ok(());
    }

    let wait = async {
        let status_poll = tokio::time::sleep(Duration::from_secs(
            RESOURCE_READY_STATUS_POLL_INTERVAL_SECONDS,
        ));
        tokio::pin!(status_poll);

        loop {
            tokio::select! {
                    _ = &mut status_poll => {
                        if deployment_status_is_ready(deployment_id).await? {
                            return Ok(());
                        }
                        status_poll.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(RESOURCE_READY_STATUS_POLL_INTERVAL_SECONDS));
                    }
                    message = subscriber.next() => {
                        let Some(message) = message else {
                            return Err(format!(
                                "NATS subscription closed before deployment {deployment_id} reported ready"
                            ));
                        };

                let envelope =
                    match serde_json::from_slice::<SandboxLifecycleEnvelope>(&message.payload) {
                        Ok(envelope) => envelope,
                        Err(_) => continue,
                    };
                if envelope.payload.deployment_id != deployment_id {
                    continue;
                }

                match envelope.payload.status.as_str() {
                    "running" => return Ok(()),
                    "failed" => {
                        return Err(
                            envelope
                                .payload
                                .error
                                .unwrap_or_else(|| {
                                    format!(
                                        "sandbox lifecycle event `{}` reported failure for deployment {deployment_id}",
                                        envelope.event_type
                                    )
                                }),
                        )
                    }
                    _ => continue,
                }
            }
                }
        }
    };

    let timeout_seconds = resource_ready_timeout_seconds();

    tokio::time::timeout(Duration::from_secs(timeout_seconds), wait)
    .await
    .map_err(|_| {
        format!(
            "Timed out after {}s waiting for sandbox {} to become ready via NATS",
            timeout_seconds, deployment_id
        )
    })?
}

fn resource_ready_timeout_seconds() -> u64 {
    std::env::var(RESOURCE_READY_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_RESOURCE_READY_TIMEOUT_SECONDS)
}

pub async fn list_workflow_runs(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            error!("Database not available: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    let runs = match repo.list_workflow_runs_for_user(&claims.user_id, 100).await {
        Ok(runs) => runs,
        Err(e) => {
            error!("Failed to list workflow runs: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to list workflow runs" })),
            );
        }
    };

    let mut summaries = Vec::with_capacity(runs.len());
    for run in runs {
        let plan_artifact_id = repo
            .list_artifacts_by_kind_for_workflow(&run.id, "deploy-plan")
            .await
            .ok()
            .and_then(|artifacts| artifacts.into_iter().next().map(|a| a.id));
        let app_name = run
            .inputs
            .get("appName")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let repository = run
            .inputs
            .get("repositorySource")
            .or_else(|| run.inputs.get("repoRoot"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let driver = run
            .inputs
            .get("deployConfig")
            .and_then(|v| v.get("driver"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        summaries.push(json!({
            "id": run.id,
            "threadId": run.thread_id,
            "templateId": run.template_id,
            "templateVersion": run.template_version,
            "goal": run.goal,
            "status": run.status,
            "phase": run.phase,
            "target": run.target,
            "appName": app_name,
            "repository": repository,
            "driver": driver,
            "createdAt": run.created_at,
            "completedAt": run.completed_at,
            "planArtifactId": plan_artifact_id,
        }));
    }

    (StatusCode::OK, Json(json!({ "workflowRuns": summaries })))
}

pub async fn get_parent_job(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(parent_job_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            error!("Database not available: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    match repo
        .find_parent_job_details_for_user(&parent_job_id, &claims.user_id)
        .await
    {
        Ok(Some(details)) => (StatusCode::OK, Json(parent_job_response(&details, None))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Parent job not found" })),
        ),
        Err(e) => {
            error!("Failed to fetch parent job: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to fetch parent job" })),
            )
        }
    }
}

pub async fn decide_approval(
    headers: HeaderMap,
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Json(req): Json<DecideApprovalRequest>,
) -> (StatusCode, Json<Value>) {
    let claims = match auth::extract_bearer_token(&headers, &state.config.jwt_secret) {
        Ok(claims) => claims,
        Err(status) => return (status, Json(json!({ "error": "Unauthorized" }))),
    };

    let decision = req.decision.trim().to_lowercase();
    if !matches!(decision.as_str(), "approved" | "rejected") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "Invalid decision",
                "message": "decision must be approved or rejected",
            })),
        );
    }

    let db = match db::get_db() {
        Ok(db) => db,
        Err(e) => {
            error!("Database not available: {}", e);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    let Some(approval) = (match repo
        .find_approval_for_user(&approval_id, &claims.user_id)
        .await
    {
        Ok(approval) => approval,
        Err(e) => {
            error!("Failed to fetch orchestration approval: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to fetch approval" })),
            );
        }
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Approval not found" })),
        );
    };

    if approval.status != "pending" {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "Approval already decided",
                "status": approval.status,
            })),
        );
    }

    let step_status = if decision == "approved" {
        "completed"
    } else {
        "failed"
    };
    let workflow_status = if decision == "approved" {
        "running"
    } else {
        "failed"
    };
    let workflow_phase = if decision == "approved" {
        match repo.find_step_run(&approval.step_run_id).await {
            Ok(Some(step)) => step.phase,
            _ => "planning".to_string(),
        }
    } else {
        "failed".to_string()
    };

    let decision_comment = req.comment.clone();
    let approval = match repo
        .decide_approval(
            &approval.id,
            &claims.user_id,
            &decision,
            decision_comment.clone(),
            step_status,
            workflow_status,
            &workflow_phase,
            json!({
                "decision": decision,
                "approvalId": approval.id,
            }),
        )
        .await
    {
        Ok(Some(approval)) => approval,
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "Approval already decided" })),
            )
        }
        Err(e) => {
            error!("Failed to decide orchestration approval: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to persist approval decision" })),
            );
        }
    };

    maybe_record_message(
        &repo,
        &approval.thread_id,
        "user",
        format!(
            "{} {} approval{}",
            if decision == "approved" {
                "Approved"
            } else {
                "Rejected"
            },
            approval.kind,
            decision_comment
                .as_deref()
                .map(|comment| format!(": {comment}"))
                .unwrap_or_default(),
        ),
        Some(json!({
            "approvalId": approval.id,
            "decision": decision,
            "workflowRunId": approval.workflow_run_id,
            "stepRunId": approval.step_run_id,
        })),
    )
    .await;

    if decision == "rejected" {
        return match repo
            .find_parent_job_details_for_user(&approval.workflow_run_id, &claims.user_id)
            .await
        {
            Ok(Some(details)) => (StatusCode::OK, Json(parent_job_response(&details, None))),
            Ok(None) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Parent job not found" })),
            ),
            Err(e) => {
                error!("Failed to fetch parent job after rejection: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Failed to fetch parent job" })),
                )
            }
        };
    }

    let _executor = spawn_execute_until_blocked(
        state.clone(),
        OrchestrationRepository::new(db.clone()),
        approval.workflow_run_id.clone(),
    );
    match repo
        .find_parent_job_details_for_user(&approval.workflow_run_id, &claims.user_id)
        .await
    {
        Ok(Some(details)) => (StatusCode::OK, Json(parent_job_response(&details, None))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Parent job not found" })),
        ),
        Err(e) => {
            error!("Failed to fetch parent job after approval: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to fetch parent job" })),
            )
        }
    }
}

fn parent_job_response(
    details: &crate::repositories::ParentJobDetails,
    template: Option<crate::orchestration::WorkflowTemplateDefinition>,
) -> Value {
    let mut response = json!({
        "parentJob": details.workflow_run,
        "childJobs": details.step_runs,
        "artifacts": details.artifacts,
        "approvals": details.approvals,
    });
    if let Some(template) = template {
        response["template"] = serde_json::to_value(template).unwrap_or_else(|_| json!({}));
    }
    response
}

fn read_repo_root_input(inputs: &Value) -> Option<String> {
    read_string_input(inputs, "repoRoot")
}

fn read_string_input(inputs: &Value, key: &str) -> Option<String> {
    inputs
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn looks_like_git_url(value: &str) -> bool {
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("ssh://")
        || value.starts_with("git://")
        || value.starts_with("git@")
}

async fn clone_repository_for_thread(
    state: &AppState,
    service_state_dir: &str,
    thread_id: &str,
    repository_url: &str,
) -> Result<String, String> {
    let clone_root = repository_clone_root(service_state_dir)?;
    tokio::fs::create_dir_all(&clone_root)
        .await
        .map_err(|error| {
            format!(
                "Failed to create clone root {}: {error}",
                clone_root.display()
            )
        })?;

    let destination = clone_root.join(thread_id);
    if destination.exists() {
        tokio::fs::remove_dir_all(&destination)
            .await
            .map_err(|error| {
                format!(
                    "Failed to clear existing checkout {}: {error}",
                    destination.display()
                )
            })?;
    }

    let git_auth = prepare_git_clone_auth(state, repository_url).await?;
    let output = Command::new("git")
        .envs(&git_auth.env)
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg(&git_auth.repository_url)
        .arg(&destination)
        .output()
        .await;
    git_auth.cleanup().await;
    let output = output.map_err(|error| format!("Failed to execute git clone: {error}"))?;

    if output.status.success() {
        return Ok(destination.display().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let details = if stderr.is_empty() { stdout } else { stderr };

    Err(format!(
        "git clone failed for {}: {}",
        repository_url,
        if details.is_empty() {
            "unknown error".to_string()
        } else {
            details
        }
    ))
}

struct GitCloneAuth {
    repository_url: String,
    env: HashMap<String, String>,
    temp_dir: Option<PathBuf>,
}

impl GitCloneAuth {
    async fn cleanup(&self) {
        if let Some(temp_dir) = &self.temp_dir {
            let _ = tokio::fs::remove_dir_all(temp_dir).await;
        }
    }
}

async fn prepare_git_clone_auth(
    state: &AppState,
    repository_url: &str,
) -> Result<GitCloneAuth, String> {
    let mut env = HashMap::new();
    env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());

    let token = git_auth_token(state).await?;
    let token_rewrite_url = token
        .as_ref()
        .and_then(|_| github_ssh_url_to_https(repository_url));
    let ssh_key = if token_rewrite_url.is_none() && is_ssh_git_url(repository_url) {
        git_ssh_private_key(state).await?
    } else {
        None
    };
    let repository_url = token_rewrite_url.unwrap_or_else(|| repository_url.to_string());

    let temp_dir = if ssh_key.is_some() || token.is_some() {
        let dir = std::env::temp_dir().join(format!(
            "heyo-orchestrator-git-auth-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&dir).await.map_err(|error| {
            format!(
                "Failed to create temporary git credential directory {}: {error}",
                dir.display()
            )
        })?;
        Some(dir)
    } else {
        None
    };

    if let (Some(key), Some(temp_dir)) = (ssh_key.as_deref(), temp_dir.as_ref()) {
        let key_path = temp_dir.join("git-identity");
        write_secret_file(&key_path, key, 0o600).await?;
        let known_hosts = temp_dir.join("known_hosts");
        env.insert(
            "GIT_SSH_COMMAND".to_string(),
            format!(
                "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
                shell_arg_path(&key_path),
                shell_arg_path(&known_hosts)
            ),
        );
    }

    if let (Some(token), Some(temp_dir)) = (token.as_deref(), temp_dir.as_ref()) {
        let username = std::env::var("ORCHESTRATOR_GIT_AUTH_USERNAME")
            .or_else(|_| std::env::var("CI_GIT_USERNAME"))
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "x-access-token".to_string());
        let askpass_path = temp_dir.join("git-askpass.sh");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n*Username*) printf '%s\\n' {} ;;\n*Password*) printf '%s\\n' {} ;;\n*) printf '\\n' ;;\nesac\n",
            shell_single_quote(&username),
            shell_single_quote(token),
        );
        write_secret_file(&askpass_path, &script, 0o700).await?;
        env.insert("GIT_ASKPASS".to_string(), askpass_path.display().to_string());
    }

    Ok(GitCloneAuth {
        repository_url,
        env,
        temp_dir,
    })
}

async fn git_auth_token(state: &AppState) -> Result<Option<String>, String> {
    if let Some(value) = first_non_empty_env(&[
        "ORCHESTRATOR_GIT_AUTH_TOKEN",
        "CI_GIT_AUTH_TOKEN",
        "GITHUB_TOKEN",
    ]) {
        return Ok(Some(value));
    }
    read_optional_heyosecret(state, git_auth_token_secret_path()).await
}

async fn git_ssh_private_key(state: &AppState) -> Result<Option<String>, String> {
    if let Some(value) =
        first_non_empty_env(&["ORCHESTRATOR_GIT_SSH_PRIVATE_KEY", "CI_GIT_SSH_PRIVATE_KEY"])
    {
        return Ok(Some(value));
    }
    read_optional_heyosecret(state, git_ssh_private_key_secret_path()).await
}

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn git_auth_token_secret_path() -> String {
    std::env::var("ORCHESTRATOR_GIT_AUTH_TOKEN_SECRET_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_GIT_AUTH_TOKEN_SECRET_PATH.to_string())
}

fn git_ssh_private_key_secret_path() -> String {
    std::env::var("ORCHESTRATOR_GIT_SSH_PRIVATE_KEY_SECRET_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_GIT_SSH_PRIVATE_KEY_SECRET_PATH.to_string())
}

async fn read_optional_heyosecret(
    state: &AppState,
    secret_path: String,
) -> Result<Option<String>, String> {
    if state.config.heyosecret_url.trim().is_empty() {
        return Ok(None);
    }
    let token = if state.config.heyosecret_internal_api_key.trim().is_empty() {
        state.config.internal_api_key.trim()
    } else {
        state.config.heyosecret_internal_api_key.trim()
    };
    if token.is_empty() {
        return Ok(None);
    }
    let client = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url: state.config.heyosecret_url.clone(),
        token: token.to_string(),
        timeout: Some(Duration::from_secs(10)),
    })
    .map_err(|error| format!("Failed to create HeyoSecret client for git credential: {error}"))?;
    match client.read_active(&secret_path).await {
        Ok(secret) => {
            let value = String::from_utf8(secret.value).map_err(|error| {
                format!("HeyoSecret git credential {secret_path} is not valid UTF-8: {error}")
            })?;
            Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
        }
        Err(heyosecret_client::HeyoSecretError::Api { status: 404, .. }) => Ok(None),
        Err(error) => Err(format!(
            "Failed to read HeyoSecret git credential {secret_path}: {error}"
        )),
    }
}

async fn write_secret_file(path: &FsPath, contents: &str, mode: u32) -> Result<(), String> {
    tokio::fs::write(path, contents).await.map_err(|error| {
        format!(
            "Failed to write temporary git credential file {}: {error}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .await
            .map_err(|error| {
                format!(
                    "Failed to chmod temporary git credential file {}: {error}",
                    path.display()
                )
            })?;
    }
    let _ = mode;
    Ok(())
}

fn github_ssh_url_to_https(repository_url: &str) -> Option<String> {
    let rest = repository_url
        .strip_prefix("git@github.com:")
        .or_else(|| repository_url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{rest}"))
}

fn is_ssh_git_url(repository_url: &str) -> bool {
    repository_url.starts_with("git@") || repository_url.starts_with("ssh://")
}

fn shell_arg_path(path: &FsPath) -> String {
    shell_single_quote(&path.display().to_string())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn materialize_repository_archive_for_thread(
    state: &AppState,
    user_id: &str,
    service_state_dir: &str,
    thread_id: &str,
    archive_id: &str,
) -> Result<String, String> {
    let clone_root = repository_clone_root(service_state_dir)?;
    tokio::fs::create_dir_all(&clone_root)
        .await
        .map_err(|error| {
            format!(
                "Failed to create clone root {}: {error}",
                clone_root.display()
            )
        })?;

    let destination = clone_root.join(thread_id);
    if destination.exists() {
        tokio::fs::remove_dir_all(&destination)
            .await
            .map_err(|error| {
                format!(
                    "Failed to clear existing checkout {}: {error}",
                    destination.display()
                )
            })?;
    }
    tokio::fs::create_dir_all(&destination)
        .await
        .map_err(|error| {
            format!(
                "Failed to create checkout directory {}: {error}",
                destination.display()
            )
        })?;

    let archive_bytes = download_archive(state, archive_id, user_id)
        .await
        .map_err(|error| format!("Failed to download repository archive {archive_id}: {error}"))?;
    let archive_path = clone_root.join(format!("{thread_id}.tar.gz"));
    tokio::fs::write(&archive_path, archive_bytes)
        .await
        .map_err(|error| {
            format!(
                "Failed to write repository archive {}: {error}",
                archive_path.display()
            )
        })?;

    let output = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&destination)
        .output()
        .await
        .map_err(|error| format!("Failed to execute tar extraction: {error}"));
    let _ = tokio::fs::remove_file(&archive_path).await;
    let output = output?;

    if output.status.success() {
        return Ok(destination.display().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let details = if stderr.is_empty() { stdout } else { stderr };

    Err(format!(
        "repository archive extraction failed for {}: {}",
        archive_id,
        if details.is_empty() {
            "unknown error".to_string()
        } else {
            details
        }
    ))
}

fn repository_clone_root(service_state_dir: &str) -> Result<PathBuf, String> {
    let configured = std::env::var("ORCHESTRATOR_REPOSITORY_CLONE_ROOT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    repository_clone_root_with_override(service_state_dir, configured.as_deref())
}

fn repository_clone_root_with_override(
    service_state_dir: &str,
    configured_clone_root: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(configured) = configured_clone_root {
        return Ok(PathBuf::from(configured));
    }

    let service_state_dir = service_state_dir.trim();
    if service_state_dir.is_empty() {
        return Err("ORCHESTRATOR_SERVICE_STATE_DIR must not be empty".to_string());
    }

    let state_dir = PathBuf::from(service_state_dir);
    let orchestrator_dir = state_dir.parent().unwrap_or(state_dir.as_path());
    Ok(orchestrator_dir.join("repos"))
}

async fn maybe_record_message(
    repo: &OrchestrationRepository,
    thread_id: &str,
    role: &str,
    text: String,
    metadata: Option<Value>,
) {
    if let Err(error) = repo
        .create_message(thread_id, role, json!({ "text": text }), metadata)
        .await
    {
        warn!(
            "Failed to append orchestration message for thread {}: {}",
            thread_id, error
        );
    }
}

async fn maybe_record_tool_call(repo: &OrchestrationRepository, input: ToolCallLogCreateInput) {
    let thread_id = input.thread_id.clone();
    if let Err(error) = repo.create_tool_call_log(input).await {
        warn!(
            "Failed to append orchestration tool call log for thread {}: {}",
            thread_id, error
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_clone_root_is_sibling_of_service_state_dir() {
        let root = repository_clone_root_with_override(
            "/opt/heyo-deploy/orchestrator/service-state",
            None,
        )
        .unwrap();
        assert_eq!(root, PathBuf::from("/opt/heyo-deploy/orchestrator/repos"));
    }

    #[test]
    fn repository_clone_root_rejects_empty_service_state_dir() {
        let error = repository_clone_root_with_override("  ", None).unwrap_err();
        assert_eq!(error, "ORCHESTRATOR_SERVICE_STATE_DIR must not be empty");
    }

    #[test]
    fn repository_clone_root_uses_explicit_env_override() {
        let root = repository_clone_root_with_override(
            "/opt/heyo-deploy/orchestrator/service-state",
            Some("/workspace/repos"),
        )
        .unwrap();
        assert_eq!(root, PathBuf::from("/workspace/repos"));
    }

    #[test]
    fn github_ssh_url_rewrites_to_https_for_token_auth() {
        assert_eq!(
            github_ssh_url_to_https("git@github.com:example/internal-stack.git"),
            Some("https://github.com/example/internal-stack.git".to_string())
        );
        assert_eq!(
            github_ssh_url_to_https("ssh://git@github.com/example/internal-stack.git"),
            Some("https://github.com/example/internal-stack.git".to_string())
        );
        assert_eq!(github_ssh_url_to_https("git@gitlab.com:org/repo.git"), None);
    }

    #[test]
    fn service_claims_can_attribute_deployments_to_submitter() {
        let claims = auth::Claims {
            user_id: "cicd".to_string(),
            email: None,
            username: None,
            role: Some("service".to_string()),
            account_id: Some("local".to_string()),
            exp: 0,
            iat: 0,
            aud: None,
            iss: None,
        };

        let (user_id, account_id) =
            deployment_owner_from_claims(&claims, Some("user-123"), Some("acct-456"));

        assert_eq!(user_id, "user-123");
        assert_eq!(account_id, "acct-456");
    }

    #[test]
    fn service_claims_ignore_blank_deployment_owner_overrides() {
        let claims = auth::Claims {
            user_id: "cicd".to_string(),
            email: None,
            username: None,
            role: Some("service".to_string()),
            account_id: Some("local".to_string()),
            exp: 0,
            iat: 0,
            aud: None,
            iss: None,
        };

        let (user_id, account_id) = deployment_owner_from_claims(&claims, Some(" "), Some(""));

        assert_eq!(user_id, "cicd");
        assert_eq!(account_id, "local");
    }

    #[test]
    fn non_service_claims_cannot_spoof_deployment_owner() {
        let claims = auth::Claims {
            user_id: "user-real".to_string(),
            email: None,
            username: None,
            role: Some("user".to_string()),
            account_id: Some("acct-real".to_string()),
            exp: 0,
            iat: 0,
            aud: None,
            iss: None,
        };

        let (user_id, account_id) =
            deployment_owner_from_claims(&claims, Some("user-spoof"), Some("acct-spoof"));

        assert_eq!(user_id, "user-real");
        assert_eq!(account_id, "acct-real");
    }
}
