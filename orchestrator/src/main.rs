use std::sync::Arc;

use axum::{
    extract::DefaultBodyLimit,
    http::Method,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use uuid::Uuid;

mod agent;
mod auth;
mod cloud_client;
mod config;
mod db;
mod entities;
mod handlers;
mod llm;
mod orchestration;
mod repositories;
mod types;

pub use types::AppState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    if dotenvy::from_path(&env_path).is_err() {
        let _ = dotenvy::dotenv();
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,orchestrator=info")),
        )
        .init();

    info!("Starting Heyo Orchestrator");

    let config = Arc::new(config::Config::load()?);
    let server_port = config.server_port;
    db::init_database(&config).await?;

    let mut backend_caps = orchestration::BackendCaps {
        target_os: config.target_os.clone(),
        supported_drivers: config.target_supported_drivers.clone(),
        archive_supported_drivers: config.target_archive_supported_drivers.clone(),
    };
    let backend_url = if !config.backend_api_url.trim().is_empty() {
        config.backend_api_url.clone()
    } else {
        std::env::var("ORCHESTRATOR_BACKEND_API_URL").unwrap_or_default()
    };
    if !backend_url.trim().is_empty() {
        let url = format!("{}/capabilities", backend_url.trim_end_matches('/'));
        match reqwest::Client::new()
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(value) => {
                        if let Some(os) = value.get("targetOs").and_then(|v| v.as_str()) {
                            backend_caps.target_os = os.to_string();
                        }
                        if let Some(arr) = value.get("supportedDrivers").and_then(|v| v.as_array())
                        {
                            let drivers: Vec<String> = arr
                                .iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect();
                            if !drivers.is_empty() {
                                backend_caps.supported_drivers = drivers;
                            }
                        }
                        if let Some(arr) = value
                            .get("archiveSupportedDrivers")
                            .and_then(|v| v.as_array())
                        {
                            backend_caps.archive_supported_drivers = arr
                                .iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect();
                        }
                        info!("Loaded backend capabilities from {}", url);
                    }
                    Err(e) => tracing::warn!("Backend /capabilities returned non-JSON: {e}"),
                }
            }
            Ok(resp) => tracing::warn!(
                "Backend /capabilities returned status {}; using config defaults",
                resp.status()
            ),
            Err(e) => {
                tracing::warn!("Failed to fetch backend /capabilities ({e}); using config defaults")
            }
        }
    }
    orchestration::init_backend_caps(backend_caps.clone());
    info!(
        "Target backend caps: os={}, drivers={:?}, default_driver={}",
        backend_caps.target_os,
        backend_caps.supported_drivers,
        backend_caps
            .supported_drivers
            .first()
            .cloned()
            .unwrap_or_default()
    );

    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let state = AppState {
        config,
        http_client,
        worker_id: Arc::new(format!(
            "orchestrator-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        )),
        ci_workspace_cache: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    };

    tokio::spawn(orchestration::run_runtime_reconciler(state.clone()));
    tokio::spawn(handlers::service_deploy::run_retirement_reconciler(
        state.clone(),
    ));

    let app = Router::new()
        .route("/health", get(health_check))
        .route(
            "/orchestration/templates",
            get(handlers::orchestration::list_templates),
        )
        .route(
            "/orchestration/threads",
            post(handlers::orchestration::create_thread),
        )
        .route(
            "/orchestration/ci/artifacts/presign",
            post(handlers::orchestration::presign_ci_artifact_uploads),
        )
        .route(
            "/orchestration/ci/artifacts/finalize",
            post(handlers::orchestration::finalize_ci_artifacts),
        )
        .route(
            "/orchestration/resources/archives/presign",
            post(handlers::orchestration::presign_resource_archives),
        )
        .route(
            "/orchestration/resources/archives/finalize",
            post(handlers::orchestration::finalize_resource_archives),
        )
        .route(
            "/orchestration/resources/archives",
            post(handlers::orchestration::create_resource_archive)
                .layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024)),
        )
        .route(
            "/orchestration/resources/archives/{archive_id}",
            get(handlers::orchestration::download_resource_archive),
        )
        .route(
            "/orchestration/resources/archives/{archive_id}/download-url",
            get(handlers::orchestration::get_resource_archive_download_url),
        )
        .route(
            "/orchestration/resources/deployments",
            post(handlers::orchestration::create_resource_deployment)
                .layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024)),
        )
        .route(
            "/orchestration/resources/deployments/{deployment_id}/exec",
            post(handlers::orchestration::exec_resource_deployment),
        )
        .route(
            "/orchestration/resources/deployments/{deployment_id}/exec-operations",
            post(handlers::orchestration::start_resource_deployment_exec_operation),
        )
        .route(
            "/orchestration/resources/deployments/{deployment_id}/exec-operations/{operation_id}",
            get(handlers::orchestration::get_resource_deployment_exec_operation),
        )
        .route(
            "/orchestration/resources/deployments/{deployment_id}/wait-ready",
            post(handlers::orchestration::wait_resource_deployment_ready),
        )
        .route(
            "/orchestration/resources/deployments/{deployment_id}/stop",
            post(handlers::orchestration::stop_resource_deployment),
        )
        .route(
            "/orchestration/resources/deployments/{deployment_id}",
            axum::routing::delete(handlers::orchestration::delete_resource_deployment),
        )
        .route(
            "/orchestration/services/deployments",
            post(handlers::service_deploy::deploy_service)
                .layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024)),
        )
        .route(
            "/orchestration/services/deployments/{deployment_id}",
            get(handlers::service_deploy::get_service_deployment_run),
        )
        .route(
            "/orchestration/services/{service_id}",
            get(handlers::service_deploy::get_service_deployment),
        )
        .route(
            "/orchestration/services/{service_id}/discovery",
            get(handlers::service_discovery::get_service_discovery),
        )
        .route(
            "/orchestration/threads/{thread_id}",
            get(handlers::orchestration::get_thread),
        )
        .route(
            "/orchestration/threads/{thread_id}/timeline",
            get(handlers::orchestration::get_thread_timeline),
        )
        .route(
            "/orchestration/threads/{thread_id}/artifacts",
            post(handlers::orchestration::create_thread_artifact),
        )
        .route(
            "/orchestration/parent-jobs/compile",
            post(handlers::orchestration::compile_parent_job),
        )
        .route(
            "/orchestration/workflow-runs",
            get(handlers::orchestration::list_workflow_runs),
        )
        .route(
            "/orchestration/backend-capabilities",
            get(handlers::orchestration::get_backend_capabilities),
        )
        .route(
            "/orchestration/parent-jobs/{parent_job_id}",
            get(handlers::orchestration::get_parent_job),
        )
        .route(
            "/orchestration/approvals/{approval_id}/decide",
            post(handlers::orchestration::decide_approval),
        )
        .route(
            "/internal/deployments/lifecycle",
            post(handlers::internal::apply_deploy_lifecycle_event),
        )
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers(Any),
        );

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{server_port}")).await?;
    info!("Orchestrator listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_check() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "deploymentGitSha": std::env::var("HEYO_ORCHESTRATOR_DEPLOYMENT_GIT_SHA").unwrap_or_default(),
        "deploymentPrNumber": std::env::var("HEYO_ORCHESTRATOR_DEPLOYMENT_PR_NUMBER").unwrap_or_default(),
        "deploymentId": std::env::var("HEYO_ORCHESTRATOR_DEPLOYMENT_ID").unwrap_or_default(),
    }))
}
