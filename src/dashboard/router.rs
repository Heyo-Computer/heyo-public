//! Route table. The Basic-auth layer wraps every route (including the POST
//! actions), so state-changing requests are gated identically to reads.

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;

use super::{auth, handlers, state::DashState};

pub fn build(state: DashState) -> Router {
    Router::new()
        .route("/", get(handlers::databases))
        .route("/monitoring", get(handlers::monitoring))
        .route("/events", get(handlers::events))
        .route("/monitoring/alerts", post(handlers::alert_add))
        .route("/monitoring/alerts/{id}/delete", post(handlers::alert_delete))
        .route("/monitoring/sweep", post(handlers::action_sweep_now))
        .route("/monitoring/reclaim", post(handlers::action_reclaim_now))
        .route("/vm/{id}", get(handlers::vm_detail))
        .route("/logs/pooler", get(handlers::logs_pooler))
        .route("/logs/heyvmd", get(handlers::logs_heyvmd))
        .route("/logs/vm/{id}", get(handlers::logs_vm))
        .route("/vm/{id}/start", post(handlers::action_start))
        .route("/vm/{id}/stop", post(handlers::action_stop))
        .route("/vm/{id}/reboot", post(handlers::action_reboot))
        .route("/vm/{id}/resize", post(handlers::action_resize))
        .route("/vm/{id}/reap", post(handlers::action_reap))
        .route("/stop-idle", post(handlers::action_stop_idle))
        .layer(middleware::from_fn_with_state(state.clone(), auth::basic_auth))
        .with_state(state)
}
