//! The control plane.
//!
//! Runs as a pingora `BackgroundService` so it shares the server's lifecycle and
//! graceful shutdown. It binds its own listener rather than using a pingora
//! listening service, which trades away zero-downtime socket handoff for that
//! port in exchange for real routing — an acceptable deal for an admin API.

use crate::autoscale::Autoscaler;
use crate::config::DeploymentSpec;
use crate::registry::Registry;
use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone)]
struct AdminState {
    registry: Arc<Registry>,
    autoscaler: Arc<Autoscaler>,
}

pub struct AdminApi {
    addr: String,
    state: AdminState,
}

impl AdminApi {
    pub fn new(addr: String, registry: Arc<Registry>, autoscaler: Arc<Autoscaler>) -> Self {
        Self {
            addr,
            state: AdminState {
                registry,
                autoscaler,
            },
        }
    }
}

#[derive(Serialize)]
struct VmStatus {
    sandbox_id: String,
    addr: String,
    in_flight: usize,
    healthy: bool,
    draining: bool,
}

#[derive(Serialize)]
struct DeploymentStatus {
    spec: DeploymentSpec,
    desired_replicas: u32,
    ready: usize,
    pending: usize,
    total_in_flight: usize,
    vms: Vec<VmStatus>,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn err(code: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        code,
        Json(ApiError {
            error: message.into(),
        }),
    )
}

fn status_of(d: &Arc<crate::deployment::Deployment>) -> DeploymentStatus {
    let backends = d.backends();
    DeploymentStatus {
        spec: d.spec.clone(),
        desired_replicas: d.desired_replicas(),
        ready: backends.len(),
        pending: d.pending().len(),
        total_in_flight: d.total_in_flight(),
        vms: backends
            .iter()
            .map(|b| VmStatus {
                sandbox_id: b.sandbox_id.clone(),
                addr: b.addr.to_string(),
                in_flight: b.in_flight(),
                healthy: b.is_healthy(),
                draining: b.is_draining(),
            })
            .collect(),
    }
}

async fn register(
    State(state): State<AdminState>,
    Json(spec): Json<DeploymentSpec>,
) -> impl IntoResponse {
    // Validation is the gate that keeps unroutable VMs (e.g. libvirt, which has
    // no guest_ip) from ever being booted.
    if let Err(e) = spec.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    let id = spec.id.clone();
    // Replacing a deployment abandons its old pool; tear it down explicitly so
    // the VMs don't linger until their TTL.
    if let Some(old) = state.registry.get(&id) {
        state.autoscaler.teardown(&old).await;
    }

    let deployment = state.registry.upsert(spec);
    if let Err(e) = state.registry.persist() {
        tracing::error!(error = %e, "failed to persist state");
    }
    tracing::info!(deployment = %id, "registered");

    // Let the autoscaler build the warm pool without waiting for the next tick.
    deployment.scale_signal.notify_one();

    (StatusCode::CREATED, Json(status_of(&deployment))).into_response()
}

async fn list(State(state): State<AdminState>) -> impl IntoResponse {
    let deployments = state.registry.deployments();
    let mut out: Vec<_> = deployments.values().map(status_of).collect();
    out.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    Json(out)
}

async fn get_one(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.registry.get(&id) {
        Some(d) => Json(status_of(&d)).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response(),
    }
}

async fn deregister(State(state): State<AdminState>, Path(id): Path<String>) -> impl IntoResponse {
    let Some(d) = state.registry.remove(&id) else {
        return err(StatusCode::NOT_FOUND, format!("no deployment {id:?}")).into_response();
    };
    // Removed from routing first, so the teardown can't race new requests in.
    state.autoscaler.teardown(&d).await;
    if let Err(e) = state.registry.persist() {
        tracing::error!(error = %e, "failed to persist state");
    }
    tracing::info!(deployment = %id, "deregistered");
    StatusCode::NO_CONTENT.into_response()
}

async fn healthz() -> &'static str {
    "ok\n"
}

fn router(state: AdminState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/deployments", post(register).get(list))
        .route("/deployments/:id", get(get_one))
        .route("/deployments/:id", delete(deregister))
        .with_state(state)
}

#[async_trait]
impl BackgroundService for AdminApi {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let listener = match tokio::net::TcpListener::bind(&self.addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(addr = %self.addr, error = %e, "admin API failed to bind");
                return;
            }
        };
        tracing::info!(addr = %self.addr, "admin API listening");

        let served = axum::serve(listener, router(self.state.clone()))
            .with_graceful_shutdown(async move {
                while shutdown.changed().await.is_ok() {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            })
            .await;

        if let Err(e) = served {
            tracing::error!(error = %e, "admin API stopped");
        }
    }
}
