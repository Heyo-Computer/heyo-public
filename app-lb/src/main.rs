//! app-lb: an application load balancer for heyvm Firecracker/KVM microVMs.
//!
//! Register a deployment (VM template + routes + scaling policy) against the
//! admin API and the proxy routes traffic to a pool of VMs, booting and reaping
//! them to match load.

mod admin;
mod autoscale;
mod config;
mod deployment;
mod health;
mod proxy;
mod registry;
mod vm;

use crate::admin::AdminApi;
use crate::autoscale::Autoscaler;
use crate::config::LbConfig;
use crate::proxy::LbProxy;
use crate::registry::Registry;
use crate::vm::VmManager;
use pingora_core::server::Server;
use pingora_core::services::background::background_service;
use std::sync::Arc;

fn config_from_env() -> LbConfig {
    let mut cfg = LbConfig::default();
    if let Ok(v) = std::env::var("APP_LB_PROXY_ADDR") {
        cfg.proxy_addr = v;
    }
    if let Ok(v) = std::env::var("APP_LB_ADMIN_ADDR") {
        cfg.admin_addr = v;
    }
    if let Ok(v) = std::env::var("APP_LB_STATE_PATH") {
        cfg.state_path = v;
    }
    if let Ok(v) = std::env::var("APP_LB_DAEMON_URL") {
        cfg.daemon_url = Some(v);
    }
    cfg
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,app_lb=debug".into()),
        )
        .init();

    let cfg = config_from_env();

    let registry = Arc::new(Registry::new(&cfg.state_path));
    match registry.load() {
        Ok(0) => {
            tracing::info!(path = %registry.persist_path().display(), "no persisted deployments")
        }
        Ok(n) => tracing::info!(count = n, "restored deployments"),
        Err(e) => tracing::error!(error = %e, "failed to load persisted state; starting empty"),
    }

    let vms = VmManager::new(cfg.daemon_url.clone());

    // Note: nothing above may spawn threads or build a runtime — `run_forever`
    // may fork for daemonization and anything created before it would be lost.
    let mut server = Server::new(None).expect("failed to create server");
    server.bootstrap();

    // `background_service` hands back an Arc to the same task the service runs,
    // which is how the admin API reaches the autoscaler to tear deployments down.
    let autoscaler_svc = background_service("autoscaler", Autoscaler::new(registry.clone(), vms));
    let autoscaler = autoscaler_svc.task();

    let admin_svc = background_service(
        "admin",
        AdminApi::new(cfg.admin_addr.clone(), registry.clone(), autoscaler),
    );

    let mut proxy_svc =
        pingora_proxy::http_proxy_service(&server.configuration, LbProxy::new(registry));
    proxy_svc.add_tcp(&cfg.proxy_addr);

    tracing::info!(proxy = %cfg.proxy_addr, admin = %cfg.admin_addr, "starting app-lb");

    let autoscaler_handle = server.add_service(autoscaler_svc);
    server.add_service(admin_svc);
    let proxy_handle = server.add_service(proxy_svc);
    // Don't accept traffic until the autoscaler has adopted existing VMs and
    // built the warm pool; otherwise the first requests all eat a cold start.
    proxy_handle.add_dependency(&autoscaler_handle);

    server.run_forever();
}
