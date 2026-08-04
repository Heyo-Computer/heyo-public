use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::error;

use crate::auth;
use crate::db;
use crate::orchestration::spawn_execute_until_blocked;
use crate::repositories::{
    ExternalEventConsumeOutcome, ExternalEventCreateInput, OrchestrationRepository,
};
use crate::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployLifecycleEventRequest {
    pub deployment_id: String,
    pub status: String,
    pub backend_sandbox_id: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub occurred_at: Option<String>,
}

pub async fn apply_deploy_lifecycle_event(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(req): Json<DeployLifecycleEventRequest>,
) -> (StatusCode, Json<Value>) {
    if auth::require_internal_api_key(&headers, &state.config.internal_api_key).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Unauthorized" })),
        );
    }

    let db = match db::get_db() {
        Ok(db) => db,
        Err(error) => {
            error!("Database not available: {}", error);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Database not available" })),
            );
        }
    };

    let repo = OrchestrationRepository::new(db.clone());
    let event_type = req
        .event_type
        .clone()
        .unwrap_or_else(|| format!("deployment.lifecycle.{}", req.status));
    let idempotency_key = req
        .idempotency_key
        .clone()
        .unwrap_or_else(|| format!("{}:{}", event_type, req.deployment_id));
    let payload = json!({
        "deploymentId": req.deployment_id,
        "status": req.status,
        "backendSandboxId": req.backend_sandbox_id,
        "error": req.error,
        "eventType": event_type,
        "idempotencyKey": idempotency_key,
        "messageId": req.message_id,
        "occurredAt": req.occurred_at,
    });
    let event = match repo
        .ingest_external_event(ExternalEventCreateInput {
            external_ref: req.deployment_id.clone(),
            event_type: event_type.clone(),
            event_status: req.status.clone(),
            idempotency_key: idempotency_key.clone(),
            message_id: req.message_id.clone(),
            payload,
        })
        .await
    {
        Ok(event) => event,
        Err(error) => {
            error!(
                "Failed to ingest orchestration deploy lifecycle event: {}",
                error
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to ingest deploy lifecycle event" })),
            );
        }
    };

    let outcome = match repo.consume_pending_external_event(&event.id).await {
        Ok(outcome) => outcome,
        Err(error) => {
            error!(
                "Failed to consume orchestration deploy lifecycle event: {}",
                error
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to consume deploy lifecycle event" })),
            );
        }
    };

    if let ExternalEventConsumeOutcome::Processed {
        workflow_run_id: Some(ref workflow_run_id),
        resume_workflow: true,
    } = outcome
    {
        let _executor = spawn_execute_until_blocked(
            state.clone(),
            OrchestrationRepository::new(db.clone()),
            workflow_run_id.clone(),
        );
    }

    let status = match outcome {
        ExternalEventConsumeOutcome::PendingRetry => StatusCode::ACCEPTED,
        _ => StatusCode::OK,
    };

    (status, Json(json!({ "ok": true, "eventId": event.id })))
}
