//! Exact-instance targeting. Authentication remains in the handlers; this layer
//! only enforces an exact explicitly targeted boot identity.
use axum::{extract::{Request, State}, http::StatusCode, middleware::Next, response::{IntoResponse, Response}};
use super::AppState;

pub(super) async fn route(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(target) = request.headers().get("x-ci-target-boot") {
        if target.to_str().ok().and_then(|value| value.parse::<uuid::Uuid>().ok())
            != Some(state.dispatcher.executor.boot_id()) {
            return StatusCode::CONFLICT.into_response();
        }
    }
    next.run(request).await
}
