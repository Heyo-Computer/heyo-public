//! Route table. The Basic-auth layer wraps every route (including the POST
//! actions), so state-changing requests are gated identically to reads.

use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;

use super::{archives, auth, dedicated, handlers, replication, state::DashState};

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
        // Cross-host logical replication: peers, pairings, and the
        // node-to-node endpoints a peer drives. All under the same Basic-auth
        // layer — peering is a full trust relationship, and the credential a
        // peer holds is the one that already runs this dashboard.
        .route("/replication", get(replication::page))
        .route("/replication/enable", post(replication::enable_form))
        .route("/replication/{database}/promote", post(replication::promote_form))
        .route("/replication/{database}/refresh", post(replication::refresh_form))
        .route("/replication/{database}/detach", post(replication::detach_form))
        .route("/replication/{database}/delete", post(replication::delete_form))
        .route("/peers", post(replication::peer_create_form))
        .route("/peers/{name}/delete", post(replication::peer_delete_form))
        .route(
            "/api/replication",
            get(replication::api_list).post(replication::api_enable),
        )
        // Namespaced so what a PEER may drive reads off the route table in one
        // place, and so it cannot collide with a database named "peer".
        .route("/api/replication/peer/node", get(replication::api_node_info))
        .route(
            "/api/replication/peer/replicas",
            post(replication::api_accept_replica),
        )
        .route(
            "/api/replication/{database}",
            get(replication::api_get).delete(replication::api_forget),
        )
        .route("/api/replication/{database}/promote", post(replication::api_promote))
        .route("/api/replication/{database}/refresh", post(replication::api_refresh))
        .route("/api/replication/{database}/detach", post(replication::api_detach))
        .route(
            "/api/peers",
            get(replication::api_peers_list).post(replication::api_peer_create),
        )
        .route("/api/peers/{name}", delete(replication::api_peer_delete))
        // Archive reconciliation: schemas whose data is in S3 but whose
        // registry tier stops the pooler from ever restoring it.
        .route("/archives", get(archives::page))
        .route("/archives/restore", post(archives::restore))
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
        .route("/vm/{id}/restore", post(handlers::action_restore))
        .route("/vm/{id}/archive-image", post(handlers::action_archive_image))
        .route("/stop-idle", post(handlers::action_stop_idle))
        .layer(middleware::from_fn_with_state(state.clone(), auth::basic_auth))
        .with_state(state)
}
