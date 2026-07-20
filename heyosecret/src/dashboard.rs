//! Web dashboard: a server-rendered admin UI for inspecting and managing
//! secrets. Authenticated with an admin password that mints a signed session
//! cookie (see [`crate::auth`]). This is entirely separate from the
//! machine-facing `/v1/secrets/*` API, which continues to use the shared
//! internal API key.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;

use crate::auth;
use crate::models::{
    ListSecretsResponse, PutSecretRequest, ReadSecretRequest, RevokeSecretRequest,
    RotateRandomRequest,
};
use crate::store::PutSecretInput;
use crate::{AppState, ListQuery, PathQuery};

const DASHBOARD_HTML: &str = include_str!("assets/dashboard.html");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(page))
        .route("/dashboard", get(page))
        .route("/dashboard/login", post(login))
        .route("/dashboard/logout", post(logout))
        .route("/dashboard/api/session", get(session_info))
        .route("/dashboard/api/secrets", get(list_secrets))
        .route("/dashboard/api/secret/metadata", get(secret_metadata))
        .route("/dashboard/api/secret/history", get(secret_history))
        .route("/dashboard/api/secret/read", post(read_secret))
        .route("/dashboard/api/secret/write", post(write_secret))
        .route("/dashboard/api/secret/rotate-random", post(rotate_random))
        .route("/dashboard/api/secret/revoke", post(revoke_secret))
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    password: String,
}

async fn page() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// Reports whether the dashboard is enabled and whether this request is
/// already authenticated. Used by the frontend to decide which view to show.
async fn session_info(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let enabled = state.config.dashboard_enabled();
    let authenticated = enabled
        && auth::request_has_valid_session(&state.config.session_signing_key(), &headers);
    Json(json!({
        "dashboardEnabled": enabled,
        "authenticated": authenticated,
    }))
}

async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.config.dashboard_enabled() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "dashboard is disabled");
    }
    if !auth::verify_admin_password(&state.config.admin_password, &request.password) {
        return json_error(StatusCode::UNAUTHORIZED, "invalid password");
    }
    let ttl = state.config.session_ttl_seconds;
    let token = auth::mint_session(&state.config.session_signing_key(), ttl);
    let cookie = auth::set_session_cookie(&token, ttl, state.config.cookie_secure);

    let mut response = (StatusCode::OK, Json(json!({ "ok": true }))).into_response();
    auth::apply_set_cookie(response.headers_mut(), cookie);
    response
}

async fn logout(State(state): State<AppState>) -> impl IntoResponse {
    let cookie = auth::clear_session_cookie(state.config.cookie_secure);
    let mut response = (StatusCode::OK, Json(json!({ "ok": true }))).into_response();
    auth::apply_set_cookie(response.headers_mut(), cookie);
    response
}

/// Guards a dashboard API handler: the dashboard must be enabled and the
/// request must carry a valid session cookie.
fn require_session(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    if !state.config.dashboard_enabled() {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "dashboard is disabled",
        ));
    }
    if auth::request_has_valid_session(&state.config.session_signing_key(), headers) {
        Ok(())
    } else {
        Err(json_error(StatusCode::UNAUTHORIZED, "session required"))
    }
}

async fn list_secrets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    if let Err(resp) = require_session(&state, &headers) {
        return resp;
    }
    match state.store.list(query.prefix.as_deref()).await {
        Ok(secrets) => {
            (StatusCode::OK, Json(json!(ListSecretsResponse { secrets }))).into_response()
        }
        Err(err) => json_error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn secret_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> Response {
    if let Err(resp) = require_session(&state, &headers) {
        return resp;
    }
    match state.store.metadata(&query.path).await {
        Ok(value) => (StatusCode::OK, Json(json!(value))).into_response(),
        Err(err) => json_error(StatusCode::NOT_FOUND, err.to_string()),
    }
}

async fn secret_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PathQuery>,
) -> Response {
    if let Err(resp) = require_session(&state, &headers) {
        return resp;
    }
    match state.store.history(&query.path).await {
        Ok(value) => (StatusCode::OK, Json(json!(value))).into_response(),
        Err(err) => json_error(StatusCode::NOT_FOUND, err.to_string()),
    }
}

async fn read_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ReadSecretRequest>,
) -> Response {
    if let Err(resp) = require_session(&state, &headers) {
        return resp;
    }
    match state.store.read_secret(&request.path, request.version).await {
        Ok(value) => (StatusCode::OK, Json(json!(value))).into_response(),
        Err(err) => json_error(StatusCode::NOT_FOUND, err.to_string()),
    }
}

async fn write_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PutSecretRequest>,
) -> Response {
    if let Err(resp) = require_session(&state, &headers) {
        return resp;
    }
    let value = match base64::engine::general_purpose::STANDARD.decode(&request.value_base64) {
        Ok(value) => value,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "valueBase64 is not valid base64"),
    };
    let input = PutSecretInput {
        path: request.path,
        value,
        owner: request.owner,
        description: request.description,
        tags: Some(request.tags),
        read_access: Some(request.read_access),
        write_access: Some(request.write_access),
        metadata: request.metadata,
        actor: request.actor,
    };
    match state.store.put_secret(input).await {
        Ok(value) => (StatusCode::OK, Json(json!(value))).into_response(),
        Err(err) => json_error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn rotate_random(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RotateRandomRequest>,
) -> Response {
    if let Err(resp) = require_session(&state, &headers) {
        return resp;
    }
    if request.bytes == 0 || request.bytes > 4096 {
        return json_error(StatusCode::BAD_REQUEST, "bytes must be between 1 and 4096");
    }
    match state
        .store
        .rotate_random(
            &request.path,
            crate::crypto::random_secret_bytes(request.bytes),
            request.actor,
            request.metadata,
        )
        .await
    {
        Ok(value) => (StatusCode::OK, Json(json!(value))).into_response(),
        Err(err) => json_error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

async fn revoke_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RevokeSecretRequest>,
) -> Response {
    if let Err(resp) = require_session(&state, &headers) {
        return resp;
    }
    match state
        .store
        .revoke(&request.path, request.version, request.actor)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "revoked" }))).into_response(),
        Err(err) => json_error(StatusCode::BAD_REQUEST, err.to_string()),
    }
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}
