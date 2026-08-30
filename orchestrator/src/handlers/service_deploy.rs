use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use base64::Engine;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use heyosecret_client::{HeyoSecretClient, HeyoSecretClientOptions};
use sea_orm::{ConnectionTrait, DbBackend, Statement, Value as SeaValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth;
use crate::cloud_client::{self, CreateDeploymentRequest, MountConfig, PortMapping};
use crate::db;
use crate::handlers::service_discovery;
use crate::AppState;

const DEFAULT_HEALTH_TIMEOUT_SECONDS: u64 = 180;
const DEFAULT_DRAIN_SECONDS: u64 = 10;
const SERVICE_CANDIDATE_CREATE_TIMEOUT_SECONDS: u64 = 900;
const SERVICE_ROUTE_WRITE_TIMEOUT_SECONDS: u64 = 30;
const CANDIDATE_CLEANUP_TIMEOUT_SECONDS: u64 = 60;
const CANDIDATE_DIAGNOSTIC_TIMEOUT_SECONDS: u64 = 20;
const CANDIDATE_DIAGNOSTIC_MAX_BYTES: usize = 12 * 1024;
const SERVICE_CANDIDATE_EVENT_SUBSCRIBE_TIMEOUT_SECONDS: u64 = 10;
const SERVICE_CANDIDATE_STATUS_POLL_INTERVAL_SECONDS: u64 = 5;
const SERVICE_HEALTH_REQUEST_TIMEOUT_SECONDS: u64 = 5;
const SERVICE_HEALTH_MAX_BACKOFF_SECONDS: u64 = 10;
const SERVICE_REVISION_CHECK_TIMEOUT_SECONDS: u64 = 30;
const SERVICE_RETIREMENT_RECONCILE_INTERVAL_SECONDS: u64 = 15;
const DEFAULT_GIT_AUTH_TOKEN_SECRET_PATH: &str = "cicd/git-auth-token";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDeployRequest {
    pub service_id: String,
    pub user_id: String,
    #[serde(default, alias = "async")]
    pub async_deploy: bool,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub archive_id: Option<String>,
    #[serde(default)]
    pub archive_name: Option<String>,
    #[serde(default)]
    pub archive_bytes_base64: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default = "default_driver")]
    pub driver: String,
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub port_mappings: Vec<PortMapping>,
    #[serde(default)]
    pub mounts: Vec<MountConfig>,
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
    #[serde(default = "default_size_class")]
    pub size_class: String,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default = "default_health_path")]
    pub health_path: String,
    #[serde(default = "default_health_timeout_seconds")]
    pub health_timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub retire_previous: bool,
    #[serde(default)]
    pub retire_previous_async: bool,
    #[serde(default)]
    pub delete_previous: bool,
    #[serde(default = "default_drain_seconds")]
    pub drain_seconds: u64,
    #[serde(default)]
    pub route: Option<ServiceRouteRequest>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub revision_guard: Option<ServiceRevisionGuard>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRevisionGuard {
    pub repository_url: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub expected_sha: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRouteRequest {
    pub host: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub backend_url: Option<String>,
    #[serde(default)]
    pub entry_points: Option<Vec<String>>,
    #[serde(default)]
    pub cert_resolver: Option<String>,
    #[serde(default)]
    pub priority: Option<u32>,
    #[serde(default = "default_true")]
    pub strip_prefix: bool,
    #[serde(default)]
    pub pass_host_header: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDeploymentState {
    pub service_id: String,
    pub active_deployment_id: Option<String>,
    pub active_archive_id: Option<String>,
    pub active_backend_url: Option<String>,
    pub previous_deployment_id: Option<String>,
    pub previous_archive_id: Option<String>,
    #[serde(default)]
    pub active_metadata: serde_json::Value,
    #[serde(default)]
    pub previous_metadata: Option<serde_json::Value>,
    pub route: Option<ServiceRouteRequest>,
    pub updated_at: Option<DateTime<Utc>>,
    /// Versioned endpoint membership consumed by app-lb. This is assembled from
    /// the normalized discovery tables when state is read; legacy scalar fields
    /// remain for existing service-route and maintenance consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<service_discovery::ServiceDiscoverySnapshot>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceDeployResponse {
    service_id: String,
    deployment_id: String,
    archive_id: Option<String>,
    backend_url: String,
    health_url: String,
    previous_deployment_id: Option<String>,
    previous_retired: bool,
    route_updated: bool,
    state: ServiceDeploymentState,
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

pub async fn deploy_service(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(mut request): Json<ServiceDeployRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(status) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return (status, Json(json!({ "error": "Unauthorized" })));
    }

    if request.async_deploy {
        let service_id = match sanitize_service_id(&request.service_id) {
            Ok(service_id) => service_id,
            Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error.to_string() }))),
        };
        let deployment_id = request
            .deployment_id
            .clone()
            .unwrap_or_else(|| format!("svc-{service_id}-{}", Uuid::new_v4()));
        request.deployment_id = Some(deployment_id.clone());
        let request_payload = service_deployment_request_metadata(&request);
        if let Err(error) = record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "accepted",
            "running",
            "Service deployment accepted for asynchronous execution.",
            Some(request_payload),
            None,
            None,
        )
        .await
        {
            warn!(service_id, deployment_id, "failed to persist service deployment acceptance: {error:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to persist service deployment run" })),
            );
        }

        let async_state = state.clone();
        let async_service_id = service_id.clone();
        let async_deployment_id = deployment_id.clone();
        tokio::spawn(async move {
            if let Err(error) = deploy_service_inner(async_state.clone(), request).await {
                let message = format!("Service deployment failed: {error:#}");
                warn!(
                    service_id = %async_service_id,
                    deployment_id = %async_deployment_id,
                    "async service deployment failed: {error:#}"
                );
                let _ = record_service_deployment_event(
                    &async_state,
                    &async_deployment_id,
                    &async_service_id,
                    "failed",
                    "failed",
                    &message,
                    None,
                    None,
                    Some(error.to_string()),
                )
                .await;
            }
        });

        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "serviceId": service_id,
                "deploymentId": deployment_id,
                "status": "running",
                "phase": "accepted",
                "statusUrl": format!("/orchestration/services/deployments/{deployment_id}"),
            })),
        );
    }

    match deploy_service_inner(state, request).await {
        Ok(response) => (StatusCode::OK, Json(json!(response))),
        Err(error) => {
            warn!("service deploy failed: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.to_string() })),
            )
        }
    }
}

pub async fn get_service_deployment_run(
    headers: HeaderMap,
    State(state): State<AppState>,
    AxumPath(deployment_id): AxumPath<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(status) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return (status, Json(json!({ "error": "Unauthorized" })));
    }

    match read_service_deployment_run(&state, &deployment_id).await {
        Ok(Some(run)) => (StatusCode::OK, Json(run)),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Service deployment run not found" })),
        ),
        Err(error) => {
            warn!(deployment_id, "failed to read service deployment run: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.to_string() })),
            )
        }
    }
}

pub async fn get_service_deployment(
    headers: HeaderMap,
    State(state): State<AppState>,
    AxumPath(service_id): AxumPath<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(status) = auth::require_internal_api_key(&headers, &state.config.internal_api_key) {
        return (status, Json(json!({ "error": "Unauthorized" })));
    }

    match read_service_state(&state, &service_id).await {
        Ok(state) => (StatusCode::OK, Json(json!(state))),
        Err(error) => {
            warn!("failed to read service deployment state: {error:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.to_string() })),
            )
        }
    }
}

async fn deploy_service_inner(
    state: AppState,
    mut request: ServiceDeployRequest,
) -> Result<ServiceDeployResponse> {
    let service_id = sanitize_service_id(&request.service_id)?;
    let mut current_state = read_service_state(&state, &service_id).await?;
    let previous_discovery = service_discovery::read_snapshot(&service_id).await?;
    let excluded_backend_server_ids = if request.retire_previous {
        Vec::new()
    } else {
        service_discovery::active_backend_server_ids(&service_id).await?
    };
    let deployment_id = request
        .deployment_id
        .clone()
        .unwrap_or_else(|| format!("svc-{service_id}-{}", Uuid::new_v4()));

    let archive_bytes = load_archive_bytes(&state, &request).await?;
    let archive_sha256 = format!("{:x}", Sha256::digest(&archive_bytes));
    let account_id = request
        .account_id
        .clone()
        .unwrap_or_else(|| request.user_id.clone());
    let ports = if request.ports.is_empty() && request.port_mappings.is_empty() {
        vec![8080]
    } else {
        request.ports.clone()
    };
    let resolved_secrets = resolve_env_refs(&state, &mut request).await?;

    record_service_deployment_event(
        &state,
        &deployment_id,
        &service_id,
        "started",
        "running",
        "Service deployment started.",
        Some(service_deployment_request_metadata(&request)),
        None,
        None,
    )
    .await?;

    let mut lifecycle_events = subscribe_to_candidate_lifecycle_events(
        &state,
        &service_id,
        &deployment_id,
    )
    .await;

    info!(service_id, deployment_id, "creating service deployment candidate");
    record_service_deployment_event(
        &state,
        &deployment_id,
        &service_id,
        "candidate-create",
        "running",
        "Creating service deployment candidate.",
        None,
        None,
        None,
    )
    .await?;
    let create_response = match timeout(
        Duration::from_secs(SERVICE_CANDIDATE_CREATE_TIMEOUT_SECONDS),
        cloud_client::create_deployment(
            &state,
            &CreateDeploymentRequest {
                deployment_id: deployment_id.clone(),
                user_id: request.user_id.clone(),
                account_id,
                name: request
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("service-{service_id}")),
                slug: Some(format!("{service_id}-candidate")),
                target: "service".to_string(),
                archive_name: request.archive_name.clone(),
                archive_bytes,
                region: request.region.clone(),
                backend_type: request.driver.clone(),
                image: request.image.clone(),
                ports,
                port_mappings: request.port_mappings.clone(),
                mounts: request.mounts.clone(),
                env: request.env.clone(),
                env_refs: request.env_refs.clone(),
                start_command: request.start_command.clone(),
                working_directory: request.working_directory.clone(),
                setup_hooks: request.setup_hooks.clone(),
                size_class: request.size_class.clone(),
                ttl_seconds: Some(request.ttl_seconds.unwrap_or(0)),
                excluded_backend_server_ids,
                metadata: request.metadata.clone(),
            },
        ),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            let error_message = format!(
                "timed out after {SERVICE_CANDIDATE_CREATE_TIMEOUT_SECONDS}s creating service deployment candidate {deployment_id}"
            );
            schedule_failed_candidate_cleanup(
                state.clone(),
                service_id.clone(),
                deployment_id.clone(),
            );
            let _ = record_service_deployment_event(
                &state,
                &deployment_id,
                &service_id,
                "candidate-create",
                "failed",
                &error_message,
                None,
                None,
                Some(error_message.clone()),
            )
            .await;
            anyhow::bail!(error_message);
        }
    };
    record_service_deployment_event(
        &state,
        &deployment_id,
        &service_id,
        "candidate-created",
        "running",
        "Service deployment candidate create request accepted.",
        None,
        Some(json!({
            "cloudStatus": create_response.status,
            "archiveId": create_response.archive_id,
            "backendServerId": create_response.backend_server_id,
            "backendSandboxId": create_response.backend_sandbox_id,
        })),
        None,
    )
    .await?;

    let previous_state = current_state.clone();
    let mut route_updated = false;
    let mut discovery_updated = false;

    let deployment_result = async {
        if create_response.status == "failed" {
            anyhow::bail!("service deployment candidate {deployment_id} failed during creation");
        }
        if create_response.status != "running" {
            if let Some(subscriber) = lifecycle_events.as_mut() {
                record_service_deployment_event(
                    &state,
                    &deployment_id,
                    &service_id,
                    "candidate-ready-wait",
                    "running",
                    "Waiting for service deployment candidate lifecycle readiness.",
                    None,
                    None,
                    None,
                )
                .await?;
                wait_for_candidate_lifecycle_ready(
                    subscriber,
                    &deployment_id,
                    request.health_timeout_seconds,
                )
                .await
                .with_context(|| {
                    format!(
                        "service deployment candidate {deployment_id} did not become ready"
                    )
                })?;
                record_service_deployment_event(
                    &state,
                    &deployment_id,
                    &service_id,
                    "candidate-ready",
                    "running",
                    "Service deployment candidate reported ready.",
                    None,
                    None,
                    None,
                )
                .await?;
            } else {
                info!(
                    service_id,
                    deployment_id,
                    status = %create_response.status,
                    "service deployment candidate is provisioning without NATS lifecycle events; falling back to backend URL polling"
                );
            }
        }

        record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "backend-url",
            "running",
            "Resolving service deployment candidate backend URL.",
            None,
            None,
            None,
        )
        .await?;
        record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "health-check",
            "running",
            "Checking service deployment candidate health.",
            None,
            None,
            None,
        )
        .await?;
        let health_deadline = tokio::time::Instant::now()
            + Duration::from_secs(request.health_timeout_seconds.max(1));
        let (backend_url, health_url) = wait_for_candidate_health(
            &state,
            &service_id,
            &deployment_id,
            &request.health_path,
            health_deadline,
        )
        .await?;
        record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "healthy",
            "running",
            "Service deployment candidate health check passed.",
            None,
            Some(json!({ "backendUrl": backend_url, "healthUrl": health_url })),
            None,
        )
        .await?;

        verify_service_revision_guard(
            &state,
            &service_id,
            &deployment_id,
            request.revision_guard.as_ref(),
        )
        .await?;

        let previous_deployment_id = current_state.active_deployment_id.clone();
        let previous_archive_id = current_state.active_archive_id.clone();
        let previous_metadata = Some(current_state.active_metadata.clone());
        let mut previous_deployment_ids: Vec<String> = previous_discovery
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .endpoints
                    .iter()
                    .filter(|endpoint| endpoint.deployment_id != deployment_id)
                    .map(|endpoint| endpoint.deployment_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if previous_deployment_ids.is_empty() {
            if let Some(previous) = previous_deployment_id
                .as_ref()
                .filter(|previous| *previous != &deployment_id)
            {
                previous_deployment_ids.push(previous.clone());
            }
        }
        previous_deployment_ids.sort();
        previous_deployment_ids.dedup();

        if let Some(route) = request.route.clone() {
            let effective_backend_url = route
                .backend_url
                .clone()
                .unwrap_or_else(|| backend_url.clone());
            route_updated = true;
            record_service_deployment_event(
                &state,
                &deployment_id,
                &service_id,
                "route-write",
                "running",
                "Updating service route to candidate backend.",
                None,
                Some(json!({ "backendUrl": effective_backend_url, "route": route })),
                None,
            )
            .await?;
            write_traefik_service_route(&state, &service_id, &route, &effective_backend_url)
                .await?;
            record_service_deployment_event(
                &state,
                &deployment_id,
                &service_id,
                "route-updated",
                "running",
                "Service route updated to candidate backend.",
                None,
                Some(json!({ "backendUrl": effective_backend_url })),
                None,
            )
            .await?;
        }

        record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "discovery-publish",
            "running",
            "Publishing the healthy service endpoint set.",
            None,
            None,
            None,
        )
        .await?;
        let discovery = service_discovery::publish_healthy_endpoint(
            &service_id,
            &deployment_id,
            create_response.backend_server_id.as_deref(),
            &backend_url,
            request.retire_previous,
        )
        .await?;
        discovery_updated = true;
        record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "discovery-published",
            "running",
            "Healthy service endpoint set published.",
            None,
            Some(json!({
                "version": discovery.version,
                "endpointCount": discovery.endpoints.len(),
            })),
            None,
        )
        .await?;

        current_state = ServiceDeploymentState {
            service_id: service_id.clone(),
            active_deployment_id: Some(deployment_id.clone()),
            active_archive_id: create_response.archive_id.clone(),
            active_backend_url: Some(backend_url.clone()),
            previous_deployment_id: previous_deployment_id.clone(),
            previous_archive_id,
            active_metadata: build_deployment_metadata(
                &request,
                &deployment_id,
                &archive_sha256,
                &resolved_secrets,
            ),
            previous_metadata,
            route: request.route.clone(),
            updated_at: Some(Utc::now()),
            discovery: Some(discovery),
        };
        record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "state-write",
            "running",
            "Persisting active service deployment state.",
            None,
            None,
            None,
        )
        .await?;
        write_service_state(&state, &current_state).await?;
        record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "state-written",
            "running",
            "Active service deployment state persisted.",
            None,
            None,
            None,
        )
        .await?;

        let previous_retired = if request.retire_previous && request.retire_previous_async {
            for previous in previous_deployment_ids {
                schedule_previous_service_deployment_retirement(
                    state.clone(),
                    service_id.clone(),
                    deployment_id.clone(),
                    previous,
                    request.drain_seconds,
                    request.delete_previous,
                );
            }
            false
        } else if request.retire_previous {
            let mut retired = !previous_deployment_ids.is_empty();
            for previous in previous_deployment_ids {
                retired &= retire_previous_service_deployment(
                    &state,
                    &service_id,
                    &deployment_id,
                    &previous,
                    request.drain_seconds,
                    request.delete_previous,
                )
                .await;
            }
            retired
        } else {
            false
        };

        let response = ServiceDeployResponse {
            service_id: service_id.clone(),
            deployment_id: deployment_id.clone(),
            archive_id: create_response.archive_id.clone(),
            backend_url,
            health_url,
            previous_deployment_id,
            previous_retired,
            route_updated,
            state: current_state,
        };
        record_service_deployment_event(
            &state,
            &deployment_id,
            &service_id,
            "completed",
            "passed",
            "Service deployment completed and is active.",
            None,
            Some(serde_json::to_value(&response).unwrap_or_else(|_| json!({}))),
            None,
        )
        .await?;
        Ok(response)
    }
    .await;

    match deployment_result {
        Ok(response) => Ok(response),
        Err(error) => {
            let _ = record_service_deployment_event(
                &state,
                &deployment_id,
                &service_id,
                "failed",
                "failed",
                &format!("Service deployment failed: {error:#}"),
                None,
                None,
                Some(error.to_string()),
            )
            .await;
            let diagnostics =
                collect_failed_candidate_diagnostics(&state, &service_id, &deployment_id).await;
            compensate_failed_candidate(
                &state,
                &service_id,
                &deployment_id,
                &request,
                &previous_state,
                route_updated,
                discovery_updated,
                previous_discovery.as_ref(),
            )
            .await;
            match diagnostics {
                Some(diagnostics) if !diagnostics.trim().is_empty() => {
                    anyhow::bail!("{error:#}\n\ncandidate diagnostics for {deployment_id}:\n{diagnostics}")
                }
                _ => Err(error),
            }
        }
    }
}

fn service_deployment_request_metadata(request: &ServiceDeployRequest) -> serde_json::Value {
    let env_keys: Vec<String> = request
        .env
        .as_ref()
        .map(|env| {
            let mut keys: Vec<String> = env.keys().cloned().collect();
            keys.sort();
            keys
        })
        .unwrap_or_default();

    json!({
        "serviceId": request.service_id,
        "userId": request.user_id,
        "accountId": request.account_id,
        "deploymentId": request.deployment_id,
        "name": request.name,
        "archiveId": request.archive_id,
        "archiveName": request.archive_name,
        "archiveBytesBase64Length": request.archive_bytes_base64.as_ref().map(|value| value.len()),
        "region": request.region,
        "driver": request.driver,
        "image": request.image,
        "ports": request.ports,
        "portMappings": request.port_mappings,
        "mounts": request.mounts,
        "envKeys": env_keys,
        "envRefCount": request.env_refs.len(),
        "startCommand": request.start_command,
        "workingDirectory": request.working_directory,
        "setupHookCount": request.setup_hooks.as_ref().map(|hooks| hooks.len()).unwrap_or(0),
        "sizeClass": request.size_class,
        "ttlSeconds": request.ttl_seconds,
        "healthPath": request.health_path,
        "healthTimeoutSeconds": request.health_timeout_seconds,
        "retirePrevious": request.retire_previous,
        "retirePreviousAsync": request.retire_previous_async,
        "deletePrevious": request.delete_previous,
        "drainSeconds": request.drain_seconds,
        "route": request.route,
        "metadata": request.metadata,
        "revisionGuard": request.revision_guard,
    })
}

async fn verify_service_revision_guard(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
    guard: Option<&ServiceRevisionGuard>,
) -> Result<()> {
    let Some(guard) = guard else {
        return Ok(());
    };
    let repository_url = validate_revision_repository_url(&guard.repository_url)?;
    validate_revision_ref(&guard.git_ref)?;
    let expected_sha = validate_revision_sha(&guard.expected_sha)?;

    if guard.force {
        record_service_deployment_event(
            state,
            deployment_id,
            service_id,
            "revision-verified",
            "running",
            "Service revision freshness check was explicitly overridden.",
            None,
            Some(json!({
                "repositoryUrl": repository_url,
                "ref": guard.git_ref,
                "expectedSha": expected_sha,
                "forced": true,
            })),
            None,
        )
        .await?;
        return Ok(());
    }

    record_service_deployment_event(
        state,
        deployment_id,
        service_id,
        "revision-check",
        "running",
        "Verifying the service revision immediately before cutover.",
        None,
        Some(json!({
            "repositoryUrl": repository_url,
            "ref": guard.git_ref,
            "expectedSha": expected_sha,
        })),
        None,
    )
    .await?;

    let token = service_revision_git_token(state).await?;
    let temp_dir = std::env::temp_dir().join(format!(
        "heyo-service-revision-{}",
        Uuid::new_v4().simple()
    ));
    tokio::fs::create_dir_all(&temp_dir).await?;
    let askpass_path = temp_dir.join("git-askpass.sh");
    let username = std::env::var("ORCHESTRATOR_GIT_AUTH_USERNAME")
        .or_else(|_| std::env::var("CI_GIT_USERNAME"))
        .unwrap_or_else(|_| "x-access-token".to_string());
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n*Username*) printf '%s\\n' {} ;;\n*Password*) printf '%s\\n' {} ;;\n*) printf '\\n' ;;\nesac\n",
        shell_single_quote(&username),
        shell_single_quote(&token),
    );
    write_secret_file(&askpass_path, &script, 0o700).await?;

    let result = timeout(
        Duration::from_secs(SERVICE_REVISION_CHECK_TIMEOUT_SECONDS),
        Command::new("git")
            .env("GIT_ASKPASS", &askpass_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["ls-remote", "--exit-code", "--refs"])
            .arg(&repository_url)
            .arg(&guard.git_ref)
            .output(),
    )
    .await;
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    let output = result
        .with_context(|| {
            format!(
                "timed out after {SERVICE_REVISION_CHECK_TIMEOUT_SECONDS}s verifying current service revision"
            )
        })?
        .context("failed to execute git ls-remote for service revision guard")?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to resolve guarded service ref {} (git exited with {})",
            guard.git_ref,
            output.status
        );
    }
    let current_sha = parse_ls_remote_revision(&output.stdout, &guard.git_ref)?;
    if current_sha != expected_sha {
        anyhow::bail!(
            "refusing stale service cutover: expected {} at {}, but current revision is {}",
            expected_sha,
            guard.git_ref,
            current_sha
        );
    }

    record_service_deployment_event(
        state,
        deployment_id,
        service_id,
        "revision-verified",
        "running",
        "Service revision is current immediately before cutover.",
        None,
        Some(json!({
            "repositoryUrl": repository_url,
            "ref": guard.git_ref,
            "expectedSha": expected_sha,
            "currentSha": current_sha,
            "forced": false,
        })),
        None,
    )
    .await?;
    Ok(())
}

fn validate_revision_repository_url(repository_url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(repository_url.trim())
        .context("revisionGuard.repositoryUrl must be a valid URL")?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        anyhow::bail!("revisionGuard.repositoryUrl must be a canonical https://github.com URL");
    }
    let segments: Vec<_> = parsed
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    if segments.len() != 2 {
        anyhow::bail!("revisionGuard.repositoryUrl must identify one GitHub owner and repository");
    }
    Ok(format!(
        "https://github.com/{}/{}",
        segments[0], segments[1]
    ))
}

fn validate_revision_ref(git_ref: &str) -> Result<()> {
    let branch = git_ref
        .strip_prefix("refs/heads/")
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| anyhow::anyhow!("revisionGuard.ref must be a full branch ref"))?;
    if branch.starts_with('-')
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains('\\')
        || branch.chars().any(|character| character.is_control() || character.is_whitespace())
    {
        anyhow::bail!("revisionGuard.ref is not a valid branch ref");
    }
    Ok(())
}

fn validate_revision_sha(expected_sha: &str) -> Result<String> {
    let expected_sha = expected_sha.trim().to_ascii_lowercase();
    if expected_sha.len() != 40 || !expected_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("revisionGuard.expectedSha must be a full 40-character Git SHA");
    }
    Ok(expected_sha)
}

fn parse_ls_remote_revision(output: &[u8], expected_ref: &str) -> Result<String> {
    let output = std::str::from_utf8(output).context("git ls-remote returned non-UTF-8 output")?;
    let mut matches = output.lines().filter_map(|line| {
        let (sha, git_ref) = line.split_once('\t')?;
        (git_ref == expected_ref).then_some(sha)
    });
    let sha = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("guarded service ref {expected_ref} did not resolve"))?;
    if matches.next().is_some() {
        anyhow::bail!("guarded service ref {expected_ref} resolved ambiguously");
    }
    validate_revision_sha(sha)
}

async fn service_revision_git_token(state: &AppState) -> Result<String> {
    for name in ["ORCHESTRATOR_GIT_AUTH_TOKEN", "CI_GIT_AUTH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }
    if state.config.heyosecret_url.trim().is_empty() {
        anyhow::bail!("Git authentication is required to verify the service revision");
    }
    let secret_path = std::env::var("ORCHESTRATOR_GIT_AUTH_TOKEN_SECRET_PATH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GIT_AUTH_TOKEN_SECRET_PATH.to_string());
    let token = if state.config.heyosecret_internal_api_key.trim().is_empty() {
        state.config.internal_api_key.clone()
    } else {
        state.config.heyosecret_internal_api_key.clone()
    };
    let client = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url: state.config.heyosecret_url.clone(),
        token,
        timeout: Some(Duration::from_secs(10)),
    })
    .context("failed to create HeyoSecret client for Git authentication")?;
    let secret = client
        .read_active(&secret_path)
        .await
        .with_context(|| format!("failed to read Git authentication from HeyoSecret path {secret_path}"))?;
    let value = String::from_utf8(secret.value).context("Git authentication token is not valid UTF-8")?;
    if value.trim().is_empty() {
        anyhow::bail!("Git authentication token is empty");
    }
    Ok(value.trim().to_string())
}

async fn write_secret_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    tokio::fs::write(path, contents).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    let _ = mode;
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn record_service_deployment_event(
    _state: &AppState,
    deployment_id: &str,
    service_id: &str,
    phase: &str,
    status: &str,
    message: &str,
    request: Option<serde_json::Value>,
    response: Option<serde_json::Value>,
    error_message: Option<String>,
) -> Result<()> {
    let db = db::get_db()?;
    let metadata = json!({
        "request": request,
        "response": response,
    });
    let request_value = match request {
        Some(value) => SeaValue::Json(Some(Box::new(value))),
        None => SeaValue::Json(None),
    };
    let response_value = match response {
        Some(value) => SeaValue::Json(Some(Box::new(value))),
        None => SeaValue::Json(None),
    };
    let metadata_value = SeaValue::Json(Some(Box::new(metadata)));

    db.execute(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO service_deployment_runs (
            deployment_id, service_id, status, phase, message, error_message, request, response,
            completed_at, updated_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
            CASE WHEN $3 IN ('passed', 'failed') THEN NOW() ELSE NULL END,
            NOW()
        )
        ON CONFLICT (deployment_id) DO UPDATE SET
            service_id = EXCLUDED.service_id,
            status = CASE
                WHEN service_deployment_runs.status IN ('passed', 'failed')
                    AND EXCLUDED.status NOT IN ('passed', 'failed')
                THEN service_deployment_runs.status
                ELSE EXCLUDED.status
            END,
            phase = EXCLUDED.phase,
            message = EXCLUDED.message,
            error_message = EXCLUDED.error_message,
            request = COALESCE(EXCLUDED.request, service_deployment_runs.request),
            response = COALESCE(EXCLUDED.response, service_deployment_runs.response),
            completed_at = CASE
                WHEN EXCLUDED.status IN ('passed', 'failed') THEN NOW()
                ELSE service_deployment_runs.completed_at
            END,
            updated_at = NOW()",
        vec![
            deployment_id.into(),
            service_id.into(),
            status.into(),
            phase.into(),
            message.into(),
            error_message.clone().into(),
            request_value,
            response_value,
        ],
    ))
    .await
    .context("failed to upsert service deployment run")?;

    db.execute(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO service_deployment_events (
            deployment_id, service_id, phase, status, message, metadata, error_message
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        vec![
            deployment_id.into(),
            service_id.into(),
            phase.into(),
            status.into(),
            message.into(),
            metadata_value,
            error_message.into(),
        ],
    ))
    .await
    .context("failed to insert service deployment event")?;

    Ok(())
}

#[derive(Debug)]
struct PendingServiceRetirement {
    deployment_id: String,
    service_id: String,
    previous_deployment_id: String,
    delete_previous: bool,
}

pub async fn run_retirement_reconciler(state: AppState) {
    let mut ticker = interval(Duration::from_secs(
        SERVICE_RETIREMENT_RECONCILE_INTERVAL_SECONDS,
    ));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        if let Err(error) = reconcile_pending_service_retirements(&state).await {
            warn!("failed to reconcile pending service retirements: {error:#}");
        }
    }
}

async fn reconcile_pending_service_retirements(state: &AppState) -> Result<()> {
    for retirement in list_pending_service_retirements().await? {
        let retired = if retirement.delete_previous {
            // A previous instance can stop itself before it records the stop.
            // Retrying delete is the durable operation: Cloud treats an absent
            // deployment as success and destroys a still-running sandbox.
            finish_previous_service_deployment_retirement(
                state,
                &retirement.service_id,
                &retirement.deployment_id,
                &retirement.previous_deployment_id,
                true,
            )
            .await
        } else {
            stop_previous_service_deployment(
                state,
                &retirement.service_id,
                &retirement.deployment_id,
                &retirement.previous_deployment_id,
                retirement.delete_previous,
            )
            .await
        };
        if !retired {
            warn!(
                service_id = retirement.service_id,
                deployment_id = retirement.deployment_id,
                previous = retirement.previous_deployment_id,
                "pending service retirement did not complete successfully"
            );
        }
    }

    Ok(())
}

async fn list_pending_service_retirements() -> Result<Vec<PendingServiceRetirement>> {
    let db = db::get_db()?;
    let rows = db
        .query_all(Statement::from_string(
            DbBackend::Postgres,
            "WITH retirement_intents AS (
                SELECT DISTINCT ON (
                    deployment_id,
                    metadata->'response'->>'previousDeploymentId'
                )
                    deployment_id,
                    service_id,
                    metadata->'response'->>'previousDeploymentId' AS previous_deployment_id,
                    COALESCE(
                        (metadata->'response'->>'deletePrevious')::BOOLEAN,
                        FALSE
                    ) AS delete_previous,
                    created_at,
                    COALESCE(
                        NULLIF(metadata->'response'->>'drainSeconds', '')::BIGINT,
                        0
                    ) AS drain_seconds
                FROM service_deployment_events
                WHERE phase = 'previous-retire-wait'
                    AND metadata->'response'->>'previousDeploymentId' IS NOT NULL
                ORDER BY
                    deployment_id,
                    metadata->'response'->>'previousDeploymentId',
                    created_at DESC,
                    id DESC
            )
            SELECT
                intent.deployment_id,
                intent.service_id,
                intent.previous_deployment_id,
                intent.delete_previous
            FROM retirement_intents intent
            WHERE intent.created_at
                    + GREATEST(intent.drain_seconds, 0) * INTERVAL '1 second' <= NOW()
                AND NOT EXISTS (
                    SELECT 1
                    FROM service_deployment_states state
                    WHERE state.active_deployment_id = intent.previous_deployment_id
                )
                AND NOT EXISTS (
                    SELECT 1
                    FROM service_deployment_events completed
                    WHERE completed.deployment_id = intent.deployment_id
                        AND completed.status = 'passed'
                        AND completed.metadata->'response'->>'previousDeploymentId'
                            = intent.previous_deployment_id
                        AND (
                            (
                                intent.delete_previous
                                AND completed.phase = 'previous-deleted'
                            )
                            OR (
                                NOT intent.delete_previous
                                AND completed.phase IN ('previous-stopped', 'previous-retained')
                            )
                        )
                )
            ORDER BY intent.created_at ASC
            LIMIT 20"
                .to_string(),
        ))
        .await
        .context("failed to list pending service deployment retirements")?;

    rows.into_iter()
        .map(|row| {
            Ok(PendingServiceRetirement {
                deployment_id: row.try_get("", "deployment_id")?,
                service_id: row.try_get("", "service_id")?,
                previous_deployment_id: row.try_get("", "previous_deployment_id")?,
                delete_previous: row.try_get("", "delete_previous")?,
            })
        })
        .collect()
}

fn schedule_previous_service_deployment_retirement(
    state: AppState,
    service_id: String,
    deployment_id: String,
    previous_deployment_id: String,
    drain_seconds: u64,
    delete_previous: bool,
) {
    tokio::spawn(async move {
        let retired = retire_previous_service_deployment(
            &state,
            &service_id,
            &deployment_id,
            &previous_deployment_id,
            drain_seconds,
            delete_previous,
        )
        .await;
        if !retired {
            warn!(
                service_id,
                deployment_id,
                previous = previous_deployment_id,
                "async previous service deployment retirement did not complete successfully"
            );
        }
    });
}

async fn retire_previous_service_deployment(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
    previous_deployment_id: &str,
    drain_seconds: u64,
    delete_previous: bool,
) -> bool {
    if let Err(error) =
        service_discovery::mark_endpoint_draining(service_id, previous_deployment_id).await
    {
        warn!(
            service_id,
            previous = previous_deployment_id,
            "failed to persist service endpoint drain intent: {error:#}"
        );
        return false;
    }
    let metadata = json!({
        "previousDeploymentId": previous_deployment_id,
        "drainSeconds": drain_seconds,
        "deletePrevious": delete_previous,
    });
    let _ = record_service_deployment_event(
        state,
        deployment_id,
        service_id,
        "previous-retire-wait",
        "running",
        "Waiting before retiring previous service deployment.",
        None,
        Some(metadata),
        None,
    )
    .await;

    if drain_seconds > 0 {
        sleep(Duration::from_secs(drain_seconds)).await;
    }

    stop_previous_service_deployment(
        state,
        service_id,
        deployment_id,
        previous_deployment_id,
        delete_previous,
    )
    .await
}

async fn stop_previous_service_deployment(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
    previous_deployment_id: &str,
    delete_previous: bool,
) -> bool {
    let stop_metadata = json!({ "previousDeploymentId": previous_deployment_id });
    let _ = record_service_deployment_event(
        state,
        deployment_id,
        service_id,
        "previous-stop",
        "running",
        "Stopping previous service deployment.",
        None,
        Some(stop_metadata.clone()),
        None,
    )
    .await;
    if let Err(error) = cloud_client::stop_deployment(state, previous_deployment_id).await {
        let error_message = error.to_string();
        warn!(
            service_id,
            deployment_id,
            previous = previous_deployment_id,
            "failed to stop previous service deployment: {error:#}"
        );
        let _ = record_service_deployment_event(
            state,
            deployment_id,
            service_id,
            "previous-stop",
            "failed",
            "Failed to stop previous service deployment.",
            None,
            Some(stop_metadata),
            Some(error_message),
        )
        .await;
        return false;
    }
    let _ = record_service_deployment_event(
        state,
        deployment_id,
        service_id,
        "previous-stopped",
        "passed",
        "Previous service deployment stopped.",
        None,
        Some(json!({ "previousDeploymentId": previous_deployment_id })),
        None,
    )
    .await;

    finish_previous_service_deployment_retirement(
        state,
        service_id,
        deployment_id,
        previous_deployment_id,
        delete_previous,
    )
    .await
}

async fn finish_previous_service_deployment_retirement(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
    previous_deployment_id: &str,
    delete_previous: bool,
) -> bool {
    if !delete_previous {
        if let Err(error) = service_discovery::remove_endpoint(service_id, previous_deployment_id).await {
            warn!(
                service_id,
                previous = previous_deployment_id,
                "previous deployment stopped but its discovery membership could not be removed: {error:#}"
            );
            return false;
        }
        let _ = record_service_deployment_event(
            state,
            deployment_id,
            service_id,
            "previous-retained",
            "passed",
            "Previous service deployment retained after stop.",
            None,
            Some(json!({ "previousDeploymentId": previous_deployment_id })),
            None,
        )
        .await;
        return true;
    }

    let delete_metadata = json!({ "previousDeploymentId": previous_deployment_id });
    let _ = record_service_deployment_event(
        state,
        deployment_id,
        service_id,
        "previous-delete",
        "running",
        "Deleting previous service deployment.",
        None,
        Some(delete_metadata.clone()),
        None,
    )
    .await;
    if let Err(error) = cloud_client::delete_deployment(state, previous_deployment_id).await {
        let error_message = error.to_string();
        warn!(
            service_id,
            deployment_id,
            previous = previous_deployment_id,
            "failed to delete previous service deployment: {error:#}"
        );
        let _ = record_service_deployment_event(
            state,
            deployment_id,
            service_id,
            "previous-delete",
            "failed",
            "Failed to delete previous service deployment.",
            None,
            Some(delete_metadata),
            Some(error_message),
        )
        .await;
        return false;
    }
    if let Err(error) = service_discovery::remove_endpoint(service_id, previous_deployment_id).await {
        warn!(
            service_id,
            previous = previous_deployment_id,
            "previous deployment deleted but its discovery membership could not be removed: {error:#}"
        );
        return false;
    }
    let _ = record_service_deployment_event(
        state,
        deployment_id,
        service_id,
        "previous-deleted",
        "passed",
        "Previous service deployment deleted.",
        None,
        Some(json!({ "previousDeploymentId": previous_deployment_id })),
        None,
    )
    .await;

    true
}

async fn read_service_deployment_run(
    _state: &AppState,
    deployment_id: &str,
) -> Result<Option<serde_json::Value>> {
    let db = db::get_db()?;
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT deployment_id, service_id, status, phase, message, error_message,
                    request, response, created_at, updated_at, completed_at
             FROM service_deployment_runs WHERE deployment_id = $1 LIMIT 1",
            [deployment_id.into()],
        ))
        .await
        .context("failed to query service deployment run")?;

    let Some(row) = row else {
        return Ok(None);
    };

    let events = db
        .query_all(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id, phase, status, message, metadata, error_message, created_at
             FROM service_deployment_events
             WHERE deployment_id = $1
             ORDER BY created_at ASC, id ASC",
            [deployment_id.into()],
        ))
        .await
        .context("failed to query service deployment events")?;

    let created_at: chrono::DateTime<chrono::FixedOffset> = row
        .try_get("", "created_at")
        .context("failed to read service deployment created_at")?;
    let updated_at: chrono::DateTime<chrono::FixedOffset> = row
        .try_get("", "updated_at")
        .context("failed to read service deployment updated_at")?;
    let completed_at: Option<chrono::DateTime<chrono::FixedOffset>> = row
        .try_get("", "completed_at")
        .context("failed to read service deployment completed_at")?;

    let mut event_values = Vec::with_capacity(events.len());
    for event in events {
        let created_at: chrono::DateTime<chrono::FixedOffset> = event
            .try_get("", "created_at")
            .context("failed to read service deployment event created_at")?;
        event_values.push(json!({
            "id": event.try_get::<i64>("", "id").context("failed to read service deployment event id")?,
            "phase": event.try_get::<String>("", "phase").context("failed to read service deployment event phase")?,
            "status": event.try_get::<String>("", "status").context("failed to read service deployment event status")?,
            "message": event.try_get::<String>("", "message").context("failed to read service deployment event message")?,
            "metadata": event.try_get::<serde_json::Value>("", "metadata").unwrap_or_else(|_| json!({})),
            "errorMessage": event.try_get::<Option<String>>("", "error_message").context("failed to read service deployment event error_message")?,
            "createdAt": created_at.with_timezone(&Utc),
        }));
    }

    Ok(Some(json!({
        "deploymentId": row.try_get::<String>("", "deployment_id").context("failed to read deployment_id")?,
        "serviceId": row.try_get::<String>("", "service_id").context("failed to read service_id")?,
        "status": row.try_get::<String>("", "status").context("failed to read status")?,
        "phase": row.try_get::<String>("", "phase").context("failed to read phase")?,
        "message": row.try_get::<Option<String>>("", "message").context("failed to read message")?,
        "errorMessage": row.try_get::<Option<String>>("", "error_message").context("failed to read error_message")?,
        "request": row.try_get::<Option<serde_json::Value>>("", "request").context("failed to read request")?,
        "response": row.try_get::<Option<serde_json::Value>>("", "response").context("failed to read response")?,
        "createdAt": created_at.with_timezone(&Utc),
        "updatedAt": updated_at.with_timezone(&Utc),
        "completedAt": completed_at.map(|value| value.with_timezone(&Utc)),
        "events": event_values,
    })))
}

async fn subscribe_to_candidate_lifecycle_events(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
) -> Option<async_nats::Subscriber> {
    if !state.config.nats.enabled {
        return None;
    }

    info!(
        service_id,
        deployment_id,
        "subscribing to service candidate lifecycle events"
    );

    let subscribe = async {
        let client = async_nats::connect(&state.config.nats.url)
            .await
            .context("failed to connect to NATS")?;
        client
            .subscribe("sandbox.evt.>")
            .await
            .context("failed to subscribe to sandbox lifecycle events")
    };

    match timeout(
        Duration::from_secs(SERVICE_CANDIDATE_EVENT_SUBSCRIBE_TIMEOUT_SECONDS),
        subscribe,
    )
    .await
    {
        Ok(Ok(subscriber)) => Some(subscriber),
        Ok(Err(error)) => {
            warn!(
                service_id,
                deployment_id,
                "service deploy could not subscribe to NATS lifecycle events; falling back to polling: {error:#}"
            );
            None
        }
        Err(_) => {
            warn!(
                service_id,
                deployment_id,
                "timed out subscribing to NATS lifecycle events; falling back to polling"
            );
            None
        }
    }
}

async fn wait_for_candidate_lifecycle_ready(
    subscriber: &mut async_nats::Subscriber,
    deployment_id: &str,
    timeout_seconds: u64,
) -> Result<()> {
    if candidate_deployment_status_is_ready(deployment_id).await? {
        return Ok(());
    }

    let wait = async {
        let status_poll = tokio::time::sleep(Duration::from_secs(
            SERVICE_CANDIDATE_STATUS_POLL_INTERVAL_SECONDS,
        ));
        tokio::pin!(status_poll);

        loop {
            tokio::select! {
                _ = &mut status_poll => {
                    if candidate_deployment_status_is_ready(deployment_id).await? {
                        return Ok(());
                    }
                    status_poll.as_mut().reset(
                        tokio::time::Instant::now()
                            + Duration::from_secs(SERVICE_CANDIDATE_STATUS_POLL_INTERVAL_SECONDS),
                    );
                }
                message = subscriber.next() => {
                    let Some(message) = message else {
                        anyhow::bail!(
                            "NATS subscription closed before deployment {deployment_id} reported ready"
                        );
                    };

                    let envelope = match serde_json::from_slice::<SandboxLifecycleEnvelope>(&message.payload) {
                        Ok(envelope) => envelope,
                        Err(_) => continue,
                    };
                    if envelope.payload.deployment_id != deployment_id {
                        continue;
                    }

                    info!(
                        deployment_id,
                        event_type = %envelope.event_type,
                        status = %envelope.payload.status,
                        "service candidate lifecycle event received"
                    );

                    match envelope.payload.status.as_str() {
                        "running" => return Ok(()),
                        "failed" => {
                            anyhow::bail!(
                                "sandbox lifecycle event `{}` reported failure for deployment {}: {}",
                                envelope.event_type,
                                deployment_id,
                                envelope
                                    .payload
                                    .error
                                    .unwrap_or_else(|| "candidate failed without an error message".to_string())
                            );
                        }
                        _ => continue,
                    }
                }
            }
        }
    };

    timeout(Duration::from_secs(timeout_seconds.max(1)), wait)
        .await
        .with_context(|| {
            format!(
                "timed out after {}s waiting for service candidate {deployment_id} lifecycle readiness",
                timeout_seconds.max(1)
            )
        })?
}

async fn candidate_deployment_status_is_ready(deployment_id: &str) -> Result<bool> {
    match current_candidate_deployment_status(deployment_id).await? {
        Some((status, _)) if status == "running" => Ok(true),
        Some((status, error_message)) if status == "failed" => {
            anyhow::bail!(
                "deployment {deployment_id} is marked failed: {}",
                error_message.unwrap_or_else(|| "no error message recorded".to_string())
            );
        }
        _ => Ok(false),
    }
}

async fn current_candidate_deployment_status(
    deployment_id: &str,
) -> Result<Option<(String, Option<String>)>> {
    let db = db::get_db()?;
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT status, error_message FROM deployed_sandboxes WHERE id = $1 LIMIT 1",
            [deployment_id.into()],
        ))
        .await
        .context("failed to query candidate deployment status")?;

    let Some(row) = row else {
        return Ok(None);
    };

    let status: String = row
        .try_get("", "status")
        .context("failed to read candidate deployment status")?;
    let error_message: Option<String> = row
        .try_get("", "error_message")
        .context("failed to read candidate deployment error message")?;

    Ok(Some((status, error_message)))
}

async fn collect_failed_candidate_diagnostics(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
) -> Option<String> {
    let command = r#"set +e
echo "=== service candidate ==="
echo "hostname: $(hostname 2>/dev/null || true)"
echo "pwd: $(pwd 2>/dev/null || true)"
echo
echo "=== processes ==="
ps -eo pid,ppid,stat,etime,comm 2>/dev/null | tail -n 80
echo
echo "=== workspace files ==="
ls -la /workspace 2>/dev/null | tail -n 80
echo
echo "=== service log ==="
if [ -f /workspace/service.log ]; then
  tail -n 400 /workspace/service.log
else
  echo "/workspace/service.log not found"
fi
"#;

    let result = timeout(
        Duration::from_secs(CANDIDATE_DIAGNOSTIC_TIMEOUT_SECONDS),
        cloud_client::exec_in_deployment(state, deployment_id, command),
    )
    .await;

    let output = match result {
        Ok(Ok(response)) => {
            let mut parts = Vec::new();
            if !response.stdout.trim().is_empty() {
                parts.push(format!("stdout:\n{}", response.stdout.trim()));
            }
            if !response.stderr.trim().is_empty() {
                parts.push(format!("stderr:\n{}", response.stderr.trim()));
            }
            if !response.output.trim().is_empty() {
                parts.push(format!("output:\n{}", response.output.trim()));
            }
            if parts.is_empty() {
                format!("diagnostic command exited with {:?}", response.exit_code)
            } else {
                parts.join("\n\n")
            }
        }
        Ok(Err(error)) => {
            warn!(
                service_id,
                deployment_id,
                "failed to collect failed service deployment diagnostics: {error:#}"
            );
            return Some(format!("failed to collect diagnostics: {error:#}"));
        }
        Err(_) => {
            warn!(
                service_id,
                deployment_id,
                "timed out collecting failed service deployment diagnostics"
            );
            return Some(format!(
                "timed out after {CANDIDATE_DIAGNOSTIC_TIMEOUT_SECONDS}s collecting diagnostics"
            ));
        }
    };

    Some(truncate_diagnostic_output(&output, CANDIDATE_DIAGNOSTIC_MAX_BYTES))
}

fn truncate_diagnostic_output(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let mut start = output.len().saturating_sub(max_bytes);
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[truncated to last {max_bytes} bytes]\n{}",
        output.get(start..).unwrap_or("")
    )
}

async fn compensate_failed_candidate(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
    request: &ServiceDeployRequest,
    previous_state: &ServiceDeploymentState,
    route_updated: bool,
    discovery_updated: bool,
    previous_discovery: Option<&service_discovery::ServiceDiscoverySnapshot>,
) {
    warn!(
        service_id,
        deployment_id,
        route_updated,
        "service deployment failed after candidate creation; attempting compensation"
    );

    if discovery_updated {
        if let Err(error) =
            service_discovery::restore_snapshot(service_id, previous_discovery).await
        {
            warn!(
                service_id,
                deployment_id,
                "failed to restore previous service discovery membership; leaving candidate running for manual recovery: {error:#}"
            );
            return;
        }
    }

    if route_updated {
        match (
            previous_state.route.as_ref().or(request.route.as_ref()),
            previous_state.active_backend_url.as_deref(),
        ) {
            (Some(previous_route), Some(previous_backend_url)) => {
                if let Err(error) = write_traefik_service_route(
                    state,
                    service_id,
                    previous_route,
                    previous_backend_url,
                )
                .await
                {
                    warn!(
                        service_id,
                        deployment_id,
                        previous_backend_url,
                        "failed to restore previous service route; leaving candidate running for manual recovery: {error:#}"
                    );
                    return;
                }
            }
            _ => {
                warn!(
                    service_id,
                    deployment_id,
                    "route may have moved to failed candidate and no previous route/backend is known; leaving candidate running for manual recovery"
                );
                return;
            }
        }
    }

    let cleanup = async {
        if let Err(error) = cloud_client::stop_deployment(state, deployment_id).await {
            warn!(
                service_id,
                deployment_id,
                "failed to stop failed service deployment candidate: {error:#}"
            );
        }
        if let Err(error) = cloud_client::delete_deployment(state, deployment_id).await {
            warn!(
                service_id,
                deployment_id,
                "failed to delete failed service deployment candidate: {error:#}"
            );
        }
    };

    if timeout(
        Duration::from_secs(CANDIDATE_CLEANUP_TIMEOUT_SECONDS),
        cleanup,
    )
    .await
    .is_err()
    {
        warn!(
            service_id,
            deployment_id,
            "timed out cleaning up failed service deployment candidate"
        );
    }
}

fn schedule_failed_candidate_cleanup(state: AppState, service_id: String, deployment_id: String) {
    tokio::spawn(async move {
        for delay_seconds in [0, 30, 120] {
            if delay_seconds > 0 {
                sleep(Duration::from_secs(delay_seconds)).await;
            }
            cleanup_failed_candidate_once(&state, &service_id, &deployment_id).await;
        }
    });
}

async fn cleanup_failed_candidate_once(state: &AppState, service_id: &str, deployment_id: &str) {
    let cleanup = async {
        if let Err(error) = cloud_client::stop_deployment(state, deployment_id).await {
            warn!(
                service_id,
                deployment_id,
                "failed to stop failed service deployment candidate: {error:#}"
            );
        }
        if let Err(error) = cloud_client::delete_deployment(state, deployment_id).await {
            warn!(
                service_id,
                deployment_id,
                "failed to delete failed service deployment candidate: {error:#}"
            );
        }
    };

    if timeout(
        Duration::from_secs(CANDIDATE_CLEANUP_TIMEOUT_SECONDS),
        cleanup,
    )
    .await
    .is_err()
    {
        warn!(
            service_id,
            deployment_id,
            "timed out cleaning up failed service deployment candidate"
        );
    }
}

async fn load_archive_bytes(state: &AppState, request: &ServiceDeployRequest) -> Result<Vec<u8>> {
    match (&request.archive_bytes_base64, &request.archive_id) {
        (Some(encoded), _) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("archiveBytesBase64 is not valid base64"),
        (None, Some(archive_id)) => {
            cloud_client::download_archive(state, archive_id, &request.user_id).await
        }
        (None, None) => anyhow::bail!("archiveId or archiveBytesBase64 is required"),
    }
}

async fn wait_for_candidate_health(
    state: &AppState,
    service_id: &str,
    deployment_id: &str,
    health_path: &str,
    deadline: tokio::time::Instant,
) -> Result<(String, String)> {
    let mut backend_urls = Vec::new();
    let mut route_backend_url = None;
    let mut attempts: u64 = 0;
    let mut retry_delay = Duration::from_secs(1);
    let mut last_error = "candidate endpoint is not available yet".to_string();

    loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for healthy deployment {deployment_id}: {last_error}");
        }

        match current_candidate_deployment_status(deployment_id).await {
            Ok(Some((status, error_message))) if status == "failed" => {
                anyhow::bail!(
                    "deployment {deployment_id} is marked failed: {}",
                    error_message.unwrap_or_else(|| "no error message recorded".to_string())
                );
            }
            Ok(_) => {}
            Err(error) => {
                last_error = format!("candidate status is temporarily unavailable: {error:#}");
                warn!(deployment_id, "{last_error}");
            }
        }

        let previous_url_count = backend_urls.len();
        match cloud_client::deployment_healthcheck_urls(state, deployment_id).await {
            Ok(urls) => {
                if let Some(url) = urls.probe_url() {
                    route_backend_url = Some(url);
                }
                for url in urls.candidates() {
                    if !backend_urls.contains(&url) {
                        backend_urls.push(url);
                    }
                }
            }
            Err(error) => {
                last_error =
                    format!("candidate health endpoints are temporarily unavailable: {error:#}");
                warn!(deployment_id, "{last_error}");
            }
        }

        attempts += 1;
        for backend_url in &backend_urls {
            let health_url = join_url_path(backend_url, health_path);
            let request = state
                .http_client
                .get(&health_url)
                .timeout(Duration::from_secs(SERVICE_HEALTH_REQUEST_TIMEOUT_SECONDS));

            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    return Ok((
                        route_backend_url
                            .clone()
                            .unwrap_or_else(|| backend_url.clone()),
                        health_url,
                    ));
                }
                Ok(response) => {
                    last_error = format!("{} returned HTTP {}", health_url, response.status());
                    warn!(health_url, status = %response.status(), "candidate health check returned non-success");
                }
                Err(error) => {
                    last_error = format!("{health_url} could not be reached: {error}");
                    warn!(health_url, "candidate health check failed: {error}");
                }
            }
        }

        if attempts == 1 || attempts % 15 == 0 {
            let _ = record_service_deployment_event(
                state,
                deployment_id,
                service_id,
                "health-check",
                "running",
                "Waiting for service deployment candidate health.",
                None,
                Some(json!({
                    "attempt": attempts,
                    "backendUrls": backend_urls,
                    "lastError": last_error,
                })),
                None,
            )
            .await;
        }

        if backend_urls.len() > previous_url_count {
            retry_delay = Duration::from_secs(1);
        } else {
            retry_delay =
                (retry_delay * 2).min(Duration::from_secs(SERVICE_HEALTH_MAX_BACKOFF_SECONDS));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        sleep(retry_delay.min(remaining)).await;
    }
}

fn service_backend_api_url(state: &AppState) -> String {
    let configured = state.config.backend_api_url.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    std::env::var("ORCHESTRATOR_BACKEND_API_URL")
        .or_else(|_| std::env::var("BACKEND_API_URL"))
        .unwrap_or_default()
}

async fn write_traefik_service_route(
    state: &AppState,
    service_id: &str,
    route: &ServiceRouteRequest,
    backend_url: &str,
) -> Result<()> {
    let proxy_backend_url = if proxy_subdomain_and_path(
        backend_url,
        &state.config.proxy_base_domains,
    )
    .is_some()
    {
        let proxy_url = service_backend_api_url(state)
            .trim()
            .trim_end_matches('/')
            .to_string();
        if proxy_url.is_empty() {
            anyhow::bail!(
                "ORCHESTRATOR_BACKEND_API_URL must be set to route service proxy subdomains"
            );
        }
        Some(proxy_url)
    } else {
        None
    };

    timeout(
        Duration::from_secs(SERVICE_ROUTE_WRITE_TIMEOUT_SECONDS),
        cloud_client::upsert_service_route(
            state,
            &cloud_client::UpsertServiceRouteRequest {
                service_id: service_id.to_string(),
                backend_url: backend_url.to_string(),
                proxy_backend_url,
                route: cloud_client::ServiceRouteRequest {
                    host: route.host.clone(),
                    path_prefix: route.path_prefix.clone(),
                    entry_points: route.entry_points.clone(),
                    cert_resolver: route.cert_resolver.clone(),
                    priority: route.priority,
                    strip_prefix: route.strip_prefix,
                    pass_host_header: route.pass_host_header,
                },
            },
        ),
    )
    .await
    .with_context(|| {
        format!(
            "timed out after {SERVICE_ROUTE_WRITE_TIMEOUT_SECONDS}s writing service route for {service_id}"
        )
    })??;

    Ok(())
}

async fn read_service_state(state: &AppState, service_id: &str) -> Result<ServiceDeploymentState> {
    let service_id = sanitize_service_id(service_id)?;

    if let Some(mut service_state) = read_service_state_from_db(&service_id).await? {
        service_state.discovery = service_discovery::read_snapshot(&service_id).await?;
        return Ok(service_state);
    }

    let path = service_state_path(state, &service_id)?;
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut service_state: ServiceDeploymentState = serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse service state {}", path.display()))?;
            service_state.discovery = service_discovery::read_snapshot(&service_id).await?;
            Ok(service_state)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ServiceDeploymentState {
            service_id,
            active_metadata: json!({}),
            ..Default::default()
        }),
        Err(error) => Err(error).with_context(|| format!("failed to read service state {}", path.display())),
    }
}

async fn write_service_state(_state: &AppState, service_state: &ServiceDeploymentState) -> Result<()> {
    write_service_state_to_db(service_state).await?;
    Ok(())
}

async fn read_service_state_from_db(service_id: &str) -> Result<Option<ServiceDeploymentState>> {
    let db = db::get_db()?;
    let row = db
        .query_one(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT service_id, active_deployment_id, active_archive_id, active_backend_url, \
             previous_deployment_id, previous_archive_id, active_metadata, previous_metadata, \
             route, updated_at \
             FROM service_deployment_states WHERE service_id = $1 LIMIT 1",
            [service_id.into()],
        ))
        .await
        .context("failed to query service deployment state")?;

    let Some(row) = row else {
        return Ok(None);
    };

    let route_value: Option<serde_json::Value> = row
        .try_get("", "route")
        .context("failed to read service deployment route")?;
    let active_metadata: Option<serde_json::Value> = row
        .try_get("", "active_metadata")
        .context("failed to read active service deployment metadata")?;
    let previous_metadata: Option<serde_json::Value> = row
        .try_get("", "previous_metadata")
        .context("failed to read previous service deployment metadata")?;
    let updated_at: chrono::DateTime<chrono::FixedOffset> = row
        .try_get("", "updated_at")
        .context("failed to read service deployment updated_at")?;

    Ok(Some(ServiceDeploymentState {
        service_id: row
            .try_get("", "service_id")
            .context("failed to read service_id")?,
        active_deployment_id: row
            .try_get("", "active_deployment_id")
            .context("failed to read active_deployment_id")?,
        active_archive_id: row
            .try_get("", "active_archive_id")
            .context("failed to read active_archive_id")?,
        active_backend_url: row
            .try_get("", "active_backend_url")
            .context("failed to read active_backend_url")?,
        previous_deployment_id: row
            .try_get("", "previous_deployment_id")
            .context("failed to read previous_deployment_id")?,
        previous_archive_id: row
            .try_get("", "previous_archive_id")
            .context("failed to read previous_archive_id")?,
        active_metadata: active_metadata.unwrap_or_else(|| json!({})),
        previous_metadata,
        route: route_value
            .map(serde_json::from_value)
            .transpose()
            .context("failed to parse service deployment route")?,
        updated_at: Some(updated_at.with_timezone(&Utc)),
        discovery: None,
    }))
}

async fn write_service_state_to_db(service_state: &ServiceDeploymentState) -> Result<()> {
    let db = db::get_db()?;
    let route = service_state
        .route
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .context("failed to serialize service route")?;
    let route_value = match route {
        Some(value) => SeaValue::Json(Some(Box::new(value))),
        None => SeaValue::Json(None),
    };
    let active_metadata = SeaValue::Json(Some(Box::new(service_state.active_metadata.clone())));
    let previous_metadata = match service_state.previous_metadata.clone() {
        Some(value) => SeaValue::Json(Some(Box::new(value))),
        None => SeaValue::Json(None),
    };

    db.execute(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO service_deployment_states (
            service_id, active_deployment_id, active_archive_id, active_backend_url,
            previous_deployment_id, previous_archive_id, active_metadata, previous_metadata,
            route, updated_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())
        ON CONFLICT (service_id) DO UPDATE SET
            active_deployment_id = EXCLUDED.active_deployment_id,
            active_archive_id = EXCLUDED.active_archive_id,
            active_backend_url = EXCLUDED.active_backend_url,
            previous_deployment_id = EXCLUDED.previous_deployment_id,
            previous_archive_id = EXCLUDED.previous_archive_id,
            active_metadata = EXCLUDED.active_metadata,
            previous_metadata = EXCLUDED.previous_metadata,
            route = EXCLUDED.route,
            updated_at = NOW()",
        vec![
            service_state.service_id.clone().into(),
            service_state.active_deployment_id.clone().into(),
            service_state.active_archive_id.clone().into(),
            service_state.active_backend_url.clone().into(),
            service_state.previous_deployment_id.clone().into(),
            service_state.previous_archive_id.clone().into(),
            active_metadata,
            previous_metadata,
            route_value,
        ],
    ))
    .await
    .context("failed to write service deployment state")?;

    Ok(())
}

fn build_deployment_metadata(
    request: &ServiceDeployRequest,
    deployment_id: &str,
    archive_sha256: &str,
    resolved_secrets: &[ResolvedSecretRef],
) -> serde_json::Value {
    let mut env_keys = request
        .env
        .as_ref()
        .map(|env| env.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    env_keys.sort();
    env_keys.dedup();

    let mut env_refs = request.env_refs.clone();
    env_refs.sort();
    env_refs.dedup();

    json!({
        "deploymentId": deployment_id,
        "archiveId": request.archive_id,
        "archiveName": request.archive_name,
        "archiveSha256": archive_sha256,
        "requestedAt": Utc::now(),
        "source": request.metadata,
        "runtime": {
            "driver": request.driver,
            "image": request.image,
            "region": request.region,
            "sizeClass": request.size_class,
            "ports": request.ports,
            "portMappings": request.port_mappings,
            "mounts": request.mounts,
            "workingDirectory": request.working_directory,
            "startCommand": request.start_command,
            "healthPath": request.health_path,
        },
        "environment": {
            "envKeys": env_keys,
            "envRefs": env_refs,
            "resolvedSecrets": resolved_secrets,
            "valuesRedacted": true,
        }
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedSecretRef {
    env: String,
    path: String,
    version: i32,
    status: String,
}

async fn resolve_env_refs(
    state: &AppState,
    request: &mut ServiceDeployRequest,
) -> Result<Vec<ResolvedSecretRef>> {
    if request.env_refs.is_empty() {
        return Ok(Vec::new());
    }
    if state.config.heyosecret_url.trim().is_empty() {
        anyhow::bail!("ORCHESTRATOR_HEYOSECRET_URL or HEYOSECRET_URL is required when envRefs are present");
    }

    let heyosecret_token = if state.config.heyosecret_internal_api_key.trim().is_empty() {
        state.config.internal_api_key.clone()
    } else {
        state.config.heyosecret_internal_api_key.clone()
    };

    let client = HeyoSecretClient::new(HeyoSecretClientOptions {
        base_url: state.config.heyosecret_url.clone(),
        token: heyosecret_token,
        timeout: Some(Duration::from_secs(10)),
    })
    .context("failed to create HeyoSecret client")?;

    let mut env = request.env.clone().unwrap_or_default();
    let mut resolved = Vec::with_capacity(request.env_refs.len());
    for reference in &request.env_refs {
        let (env_key, secret_ref) = parse_env_ref(reference)?;
        let (secret_path, version_selector) = parse_secret_ref(&secret_ref)?;
        let secret = read_heyosecret_ref_with_retry(&client, &secret_path, version_selector)
            .await
            .with_context(|| format!("failed to read HeyoSecret ref {secret_ref}"))?;
        let value = String::from_utf8(secret.value)
            .with_context(|| format!("secret {secret_path} is not valid UTF-8 for env injection"))?;
        env.insert(env_key.clone(), value);
        resolved.push(ResolvedSecretRef {
            env: env_key,
            path: secret.path,
            version: secret.version,
            status: format!("{:?}", secret.status).to_ascii_lowercase(),
        });
    }
    request.env = Some(env);
    Ok(resolved)
}

async fn read_heyosecret_ref_with_retry(
    client: &HeyoSecretClient,
    secret_path: &str,
    version_selector: SecretVersionSelector,
) -> Result<heyosecret_client::SecretValue> {
    let mut last_error = None;
    for attempt in 1..=12 {
        let result = match version_selector {
            SecretVersionSelector::Active => client.read_active(secret_path).await,
            SecretVersionSelector::Version(version) => client.read(secret_path, Some(version)).await,
        };
        match result {
            Ok(secret) => return Ok(secret),
            Err(error) => {
                warn!(
                    secret_path,
                    attempt,
                    "failed to read HeyoSecret ref; retrying: {error:#}"
                );
                last_error = Some(error);
                if attempt < 12 {
                    sleep(Duration::from_secs(attempt)).await;
                }
            }
        }
    }
    Err(last_error
        .expect("retry loop should record at least one HeyoSecret error")
        .into())
}

#[derive(Debug, Clone, Copy)]
enum SecretVersionSelector {
    Active,
    Version(i32),
}

fn parse_env_ref(reference: &str) -> Result<(String, String)> {
    let (key, value) = reference
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("envRef must be KEY=heyosecret://path[@active|@version]"))?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        || key.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        anyhow::bail!("envRef key must be an uppercase environment variable name: {key}");
    }
    Ok((key.to_string(), value.trim().to_string()))
}

fn parse_secret_ref(reference: &str) -> Result<(String, SecretVersionSelector)> {
    let rest = reference
        .strip_prefix("heyosecret://")
        .ok_or_else(|| anyhow::anyhow!("envRef value must start with heyosecret://"))?;
    let (path, selector) = match rest.rsplit_once('@') {
        Some((path, "active")) => (path, SecretVersionSelector::Active),
        Some((path, version)) => {
            let version = version
                .parse::<i32>()
                .with_context(|| format!("invalid HeyoSecret version selector @{version}"))?;
            (path, SecretVersionSelector::Version(version))
        }
        None => (rest, SecretVersionSelector::Active),
    };
    if path.trim().is_empty() {
        anyhow::bail!("HeyoSecret ref path cannot be empty");
    }
    Ok((path.to_string(), selector))
}

fn service_state_path(state: &AppState, service_id: &str) -> Result<PathBuf> {
    let service_id = sanitize_service_id(service_id)?;
    let base = state.config.service_state_dir.trim();
    if base.is_empty() {
        anyhow::bail!("ORCHESTRATOR_SERVICE_STATE_DIR must not be empty");
    }
    Ok(Path::new(base).join(format!("{service_id}.json")))
}

fn sanitize_service_id(service_id: &str) -> Result<String> {
    let trimmed = service_id.trim();
    if trimmed.is_empty() {
        anyhow::bail!("serviceId is required");
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        anyhow::bail!("serviceId must contain only lowercase letters, digits, and '-' characters");
    }
    Ok(trimmed.to_string())
}

fn join_url_path(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim();
    if path.is_empty() || path == "/" {
        base.to_string()
    } else if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

fn proxy_subdomain_and_path(url: &str, base_domains: &str) -> Option<(String, String)> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = match without_scheme.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (without_scheme, "/".to_string()),
    };
    let host = host.split(':').next().unwrap_or(host);
    let subdomain = proxy_subdomain_for_host(host, base_domains)?;
    if subdomain.is_empty() || subdomain.contains('.') {
        return None;
    }
    Some((subdomain, path))
}

fn proxy_subdomain_for_host(host: &str, base_domains: &str) -> Option<String> {
    for base_domain in base_domains.split(',').map(str::trim) {
        let base_domain = base_domain.trim_matches('.');
        if base_domain.is_empty() {
            continue;
        }
        let suffix = format!(".{base_domain}");
        if let Some(subdomain) = host.strip_suffix(&suffix) {
            if subdomain.is_empty() || subdomain.contains('.') {
                return None;
            }
            return Some(subdomain.to_string());
        }
    }
    None
}

fn default_region() -> String {
    "local".to_string()
}

fn default_driver() -> String {
    "firecracker_containerd".to_string()
}

fn default_image() -> String {
    "ubuntu:24.04".to_string()
}

fn default_size_class() -> String {
    "small".to_string()
}

fn default_health_path() -> String {
    "/health".to_string()
}

fn default_health_timeout_seconds() -> u64 {
    DEFAULT_HEALTH_TIMEOUT_SECONDS
}

fn default_drain_seconds() -> u64 {
    DEFAULT_DRAIN_SECONDS
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{
        parse_env_ref, parse_ls_remote_revision, parse_secret_ref, proxy_subdomain_and_path,
        validate_revision_ref, validate_revision_repository_url, validate_revision_sha,
        SecretVersionSelector, ServiceDeployRequest,
    };

    #[test]
    fn parses_env_secret_refs() {
        let (key, reference) = parse_env_ref("JWT_SECRET=heyosecret://platform/jwt/key@active").unwrap();
        assert_eq!(key, "JWT_SECRET");
        let (path, selector) = parse_secret_ref(&reference).unwrap();
        assert_eq!(path, "platform/jwt/key");
        assert!(matches!(selector, SecretVersionSelector::Active));

        let (path, selector) = parse_secret_ref("heyosecret://cicd/runner-token@3").unwrap();
        assert_eq!(path, "cicd/runner-token");
        assert!(matches!(selector, SecretVersionSelector::Version(3)));
    }

    #[test]
    fn rejects_invalid_env_secret_refs() {
        assert!(parse_env_ref("jwt=heyosecret://platform/jwt/key").is_err());
        assert!(parse_secret_ref("env://platform/jwt/key").is_err());
        assert!(parse_secret_ref("heyosecret://platform/jwt/key@nope").is_err());
    }

    #[test]
    fn parses_configured_proxy_health_urls() {
        assert_eq!(
            proxy_subdomain_and_path(
                "https://oj8wf6.stage.example.com/health",
                "stage.example.com,example.com"
            ),
            Some(("oj8wf6".to_string(), "/health".to_string()))
        );
        assert_eq!(
            proxy_subdomain_and_path(
                "https://oj8wf6.example.com/health",
                "stage.example.com,example.com"
            ),
            Some(("oj8wf6".to_string(), "/health".to_string()))
        );
        assert_eq!(
            proxy_subdomain_and_path(
                "https://foo.oj8wf6.stage.example.com/health",
                "stage.example.com,example.com"
            ),
            None
        );
        assert_eq!(
            proxy_subdomain_and_path("https://oj8wf6.example.com/health", ""),
            None
        );
    }

    #[test]
    fn parses_service_revision_guard_contract() {
        let request: ServiceDeployRequest = serde_json::from_value(serde_json::json!({
            "serviceId": "cloud",
            "userId": "heyo-system",
            "revisionGuard": {
                "repositoryUrl": "https://github.com/example/acme-service.git",
                "ref": "refs/heads/main",
                "expectedSha": "8265558143fa2da97be77aecb93a584575c919d1"
            }
        }))
        .unwrap();
        let guard = request.revision_guard.unwrap();
        assert_eq!(guard.git_ref, "refs/heads/main");
        assert!(!guard.force);
    }

    #[test]
    fn validates_service_revision_guard_inputs() {
        assert_eq!(
            validate_revision_repository_url("https://github.com/example/acme-service.git").unwrap(),
            "https://github.com/example/acme-service.git"
        );
        for invalid in [
            "http://github.com/example/acme-service.git",
            "https://token@github.com/example/acme-service.git",
            "https://github.com.attacker.example/example/acme-service.git",
            "https://github.com:8443/example/acme-service.git",
            "https://github.com/example/acme-service.git?token=secret",
        ] {
            assert!(validate_revision_repository_url(invalid).is_err(), "{invalid}");
        }
        assert!(validate_revision_ref("refs/heads/main").is_ok());
        assert!(validate_revision_ref("main").is_err());
        assert!(validate_revision_ref("refs/heads/bad..branch").is_err());
        assert_eq!(
            validate_revision_sha("8265558143FA2DA97BE77AECB93A584575C919D1").unwrap(),
            "8265558143fa2da97be77aecb93a584575c919d1"
        );
        assert!(validate_revision_sha("82655581").is_err());
    }

    #[test]
    fn parses_exact_service_revision_from_ls_remote() {
        let output = b"8265558143fa2da97be77aecb93a584575c919d1\trefs/heads/main\n";
        assert_eq!(
            parse_ls_remote_revision(output, "refs/heads/main").unwrap(),
            "8265558143fa2da97be77aecb93a584575c919d1"
        );
        assert!(parse_ls_remote_revision(output, "refs/heads/release").is_err());
        let ambiguous = [output.as_slice(), output.as_slice()].concat();
        assert!(parse_ls_remote_revision(&ambiguous, "refs/heads/main").is_err());
    }
}
