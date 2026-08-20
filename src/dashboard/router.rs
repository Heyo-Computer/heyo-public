//! Route table. The Basic-auth layer wraps every route (including the POST
//! actions), so state-changing requests are gated identically to reads.

use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;

use super::{auth, dedicated, handlers, state::DashState};

pub fn build(state: DashState) -> Router {
    Router::new()
        .route("/", get(handlers::databases))
        // Dedicated databases: the HTML surface and the JSON admin API for
        // provisioning a database with its own role + password. Both sit under
        // the same Basic-auth layer as everything else.
        .route("/dedicated", get(dedicated::page).post(dedicated::create))
        .route("/dedicated/{database}/delete", post(dedicated::delete))
        .route(
            "/api/databases",
            get(dedicated::api_list).post(dedicated::api_create),
        )
        .route("/api/databases/{database}", delete(dedicated::api_delete))
        .route("/monitoring", get(handlers::monitoring))
        .route("/events", get(handlers::events))
        .route("/monitoring/alerts", post(handlers::alert_add))
        .route("/monitoring/alerts/{id}/delete", post(handlers::alert_delete))
        .route("/monitoring/alerts/{id}/update", post(handlers::alert_update))
        .route("/monitoring/alerts/{id}/pause", post(handlers::alert_pause))
        .route("/monitoring/alerts/{id}/resume", post(handlers::alert_resume))
        .route("/monitoring/sweep", post(handlers::action_sweep_now))
        .route("/monitoring/ttl-sweep", post(handlers::action_ttl_sweep))
        .route("/monitoring/reclaim", post(handlers::action_reclaim_now))
        .route("/monitoring/purge", post(handlers::action_purge))
        .route("/vm/{id}", get(handlers::vm_detail))
        .route("/logs/pooler", get(handlers::logs_pooler))
        .route("/logs/heyvmd", get(handlers::logs_heyvmd))
        .route("/logs/vm/{id}", get(handlers::logs_vm))
        .route("/vm/{id}/start", post(handlers::action_start))
        .route("/vm/{id}/stop", post(handlers::action_stop))
        .route("/vm/{id}/reboot", post(handlers::action_reboot))
        .route("/vm/{id}/resize", post(handlers::action_resize))
        .route("/vm/{id}/reap", post(handlers::action_reap))
        .route("/vm/{id}/archive-image", post(handlers::action_archive_image))
        .route("/stop-idle", post(handlers::action_stop_idle))
        .layer(middleware::from_fn_with_state(state.clone(), auth::basic_auth))
        .with_state(state)
}
