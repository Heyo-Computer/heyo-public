//! app-lb: an application load balancer for heyvm Firecracker/KVM microVMs.
//!
//! Register a deployment (VM template + routes + scaling policy) against the
//! admin API and the proxy routes traffic to a pool of VMs, booting and reaping
//! them to match load.

mod acme;
mod admin;
mod autoscale;
mod config;
mod deployment;
mod health;
mod metrics;
mod proxy;
mod registry;
mod tls;
mod vm;

use crate::acme::{AcmeConfig, AcmeManager, ChallengeTable};
use crate::admin::AdminApi;
use crate::autoscale::Autoscaler;
use crate::config::LbConfig;
use crate::metrics::Metrics;
use crate::proxy::LbProxy;
use crate::registry::Registry;
use crate::tls::{CertStore, SniResolver};
use crate::vm::VmManager;
use pingora_core::listeners::tls::TlsSettings;
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
    if let Ok(v) = std::env::var("APP_LB_NAME") {
        cfg.name = v;
    }
    if let Ok(v) = std::env::var("APP_LB_DAEMON_URL") {
        cfg.daemon_url = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_DASHBOARD_USER") {
        cfg.dashboard_user = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_DASHBOARD_PASSWORD") {
        cfg.dashboard_password = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_ADMIN_AUTH") {
        cfg.admin_auth = matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    }
    if let Ok(v) = std::env::var("APP_LB_PROXY_TLS_ADDR") {
        cfg.tls_addr = v;
        cfg.tls_addr_explicit = true;
    }
    if let Ok(v) = std::env::var("APP_LB_TLS_CERT") {
        cfg.tls_cert_path = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_TLS_KEY") {
        cfg.tls_key_path = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_ACME_EMAIL") {
        cfg.acme_email = Some(v);
    }
    if let Ok(v) = std::env::var("APP_LB_ACME_DIR") {
        cfg.acme_dir = v;
    }
    if let Ok(v) = std::env::var("APP_LB_ACME_DIRECTORY") {
        cfg.acme_directory = v;
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

    // rustls needs a process-level `CryptoProvider` chosen explicitly whenever
    // more than one is compiled in, and app-lb's graph has both `ring` (iroh,
    // hickory) and `aws-lc-rs` (instant-acme). Without this the *first* rustls
    // handshake panics rather than erroring — which lands in the ACME background
    // service, the only part of app-lb that speaks TLS as a client.
    //
    // pingora installed this itself under its `rustls` feature; the `openssl`
    // feature app-lb now uses never runs that code, so it belongs here. Must
    // happen before any service starts.
    //
    // `Err` means a provider is already installed, which is the desired end
    // state either way.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("rustls crypto provider was already installed");
    }

    let cfg = config_from_env();

    // Fail fast on a gate that can't enforce anything: asking to protect the
    // admin API while giving it no credential would silently leave it open.
    if cfg.admin_auth && cfg.dashboard_password.is_none() {
        panic!(
            "APP_LB_ADMIN_AUTH is set but APP_LB_DASHBOARD_PASSWORD is not; the admin \
             gate reuses the dashboard credentials, so set a password or unset APP_LB_ADMIN_AUTH"
        );
    }

    // Pre-flight the listener addresses.
    //
    // Pingora binds inside a service task, and a failure there panics *that task
    // only* — the process survives with a dead proxy, the admin API still
    // answering, and the supervisor reporting a healthy service. Checking here
    // turns that silent half-dead state into a startup failure naming the port
    // and the reason.
    for addr in [Some(&cfg.proxy_addr), cfg.tls_enabled().then_some(&cfg.tls_addr)]
        .into_iter()
        .flatten()
    {
        if let Err(e) = std::net::TcpListener::bind(addr) {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                "; binding a port below 1024 as a non-root user needs \
                 `setcap 'cap_net_bind_service=+ep'` on the binary (re-run it after every \
                 reinstall — capabilities do not survive replacing the file), or \
                 `sysctl net.ipv4.ip_unprivileged_port_start=80`"
            } else {
                ""
            };
            panic!("cannot bind {addr}: {e}{hint}");
        }
    }

    // Setting the HTTPS address is a clear statement of intent, but it only says
    // *where* to bind, not whether to. Without ACME or a static cert there is
    // nothing to serve, and the listener is skipped — previously in silence,
    // which looks identical to a bind that failed.
    if cfg.tls_addr_explicit && !cfg.tls_enabled() {
        tracing::warn!(
            tls = %cfg.tls_addr,
            "APP_LB_PROXY_TLS_ADDR is set but no HTTPS listener will be bound: TLS needs \
             either APP_LB_ACME_EMAIL (automatic certificates) or an APP_LB_TLS_CERT / \
             APP_LB_TLS_KEY pair. Set one of them, or unset APP_LB_PROXY_TLS_ADDR.",
        );
    }

    // HTTP-01 validation is fetched on port 80 and nowhere else, so a proxy
    // bound anywhere else can only be validated if something in front forwards
    // that path. Worth saying loudly at startup rather than leaving it to be
    // diagnosed from a CA error much later.
    if cfg.acme_enabled() && !cfg.proxy_addr.ends_with(":80") {
        tracing::warn!(
            proxy = %cfg.proxy_addr,
            "ACME is enabled but the plaintext proxy is not on port 80; Let's Encrypt \
             validates http-01 on port 80 only, so issuance will fail unless something \
             forwards /.well-known/acme-challenge/ to this listener",
        );
    }

    let registry = Arc::new(Registry::new(&cfg.state_path));
    match registry.load() {
        Ok(0) => {
            tracing::info!(path = %registry.persist_path().display(), "no persisted deployments")
        }
        Ok(n) => tracing::info!(count = n, "restored deployments"),
        Err(e) => tracing::error!(error = %e, "failed to load persisted state; starting empty"),
    }

    let vms = VmManager::new(cfg.daemon_url.clone());

    // The static cert pair, if configured. With ACME on it is the *fallback*,
    // served for any SNI without an issued cert of its own; with ACME off it is
    // the only certificate. Half-configured TLS stays a hard error rather than a
    // silent fallback to plaintext, so nobody thinks a listener is encrypted
    // when it isn't.
    let fallback = match (&cfg.tls_cert_path, &cfg.tls_key_path) {
        (Some(cert), Some(key)) => Some(Arc::new(
            CertStore::load_pair(cert, key)
                .unwrap_or_else(|e| panic!("failed to load TLS cert {cert} / key {key}: {e}")),
        )),
        (None, None) => None,
        _ => panic!("TLS is half-configured: set both APP_LB_TLS_CERT and APP_LB_TLS_KEY, or neither"),
    };

    // Before anything reads the cert directory: a change of ACME directory makes
    // every cached certificate and the saved account invalid, and neither
    // announces that itself.
    if cfg.acme_enabled() {
        acme::reset_if_directory_changed(
            std::path::Path::new(&cfg.acme_dir),
            &cfg.acme_directory,
        );
    }

    // Loaded from disk *before* the listener binds: the acceptor holds no
    // certificate of its own, so anything not in the store at bind time gets the
    // fallback (or a failed handshake) until ACME reissues it.
    let certs = Arc::new(CertStore::new(
        std::path::Path::new(&cfg.acme_dir).join("certs"),
        fallback,
    ));
    if cfg.tls_enabled() {
        match certs.load_from_disk() {
            0 => tracing::info!("no cached certificates"),
            n => tracing::info!(count = n, "loaded cached certificates"),
        }
    }

    // Shared between the ACME manager (which publishes challenge responses) and
    // the proxy (which serves them).
    let challenges = Arc::new(ChallengeTable::new());

    // One metrics registry, shared by the proxy (request latency), the
    // autoscaler (cold-start timing, scaling activity), and the admin API
    // (which serves the dashboard from it).
    let metrics = Arc::new(Metrics::new());

    // Note: nothing above may spawn threads or build a runtime — `run_forever`
    // may fork for daemonization and anything created before it would be lost.
    let mut server = Server::new(None).expect("failed to create server");
    server.bootstrap();

    // `background_service` hands back an Arc to the same task the service runs,
    // which is how the admin API reaches the autoscaler to tear deployments down.
    let autoscaler_svc = background_service(
        "autoscaler",
        Autoscaler::new(registry.clone(), vms, metrics.clone()),
    );
    let autoscaler = autoscaler_svc.task();

    // ACME runs only when a contact address is configured. Its `Notify` goes to
    // the admin API so registering a deployment starts issuance immediately
    // rather than at the next 12-hour sweep.
    let acme_svc = cfg.acme_email.clone().map(|email| {
        background_service(
            "acme",
            AcmeManager::new(
                registry.clone(),
                certs.clone(),
                challenges.clone(),
                AcmeConfig {
                    email,
                    dir: cfg.acme_dir.clone().into(),
                    directory_url: cfg.acme_directory.clone(),
                    proxy_addr: cfg.proxy_addr.clone(),
                },
            ),
        )
    });
    let acme_signal = acme_svc.as_ref().map(|svc| svc.task().signal());

    let admin_svc = background_service(
        "admin",
        AdminApi::new(
            cfg.admin_addr.clone(),
            registry.clone(),
            autoscaler,
            metrics.clone(),
            cfg.name.clone(),
            cfg.dashboard_user.clone(),
            cfg.dashboard_password.clone(),
            cfg.admin_auth,
            certs.clone(),
            acme_signal,
        ),
    );

    let mut proxy_svc = pingora_proxy::http_proxy_service(
        &server.configuration,
        LbProxy::new(registry, metrics, challenges),
    );
    proxy_svc.add_tcp(&cfg.proxy_addr);

    // HTTPS listener, alongside the plaintext one. The acceptor is built with no
    // certificate attached: `CertStore` supplies one per handshake keyed on SNI,
    // which is what lets a certificate issued moments ago serve without a
    // restart. See `src/tls.rs`.
    if cfg.tls_enabled() {
        let settings = TlsSettings::with_callbacks(Box::new(SniResolver::new(certs)))
            .expect("failed to build TLS settings");
        proxy_svc.add_tls_with_settings(&cfg.tls_addr, None, settings);
        tracing::info!(tls = %cfg.tls_addr, "HTTPS listener enabled");
    }

    tracing::info!(proxy = %cfg.proxy_addr, admin = %cfg.admin_addr, "starting app-lb");

    let autoscaler_handle = server.add_service(autoscaler_svc);
    server.add_service(admin_svc);
    if let Some(acme_svc) = acme_svc {
        server.add_service(acme_svc);
    }
    let proxy_handle = server.add_service(proxy_svc);
    // Don't accept traffic until the autoscaler has adopted existing VMs and
    // built the warm pool; otherwise the first requests all eat a cold start.
    proxy_handle.add_dependency(&autoscaler_handle);

    server.run_forever();
}
