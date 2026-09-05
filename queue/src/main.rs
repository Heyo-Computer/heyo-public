//! queue: a dashboard for the NATS server the fleet dispatches through.
//!
//! Four questions, one page: how deep are the queues, how fast is anything
//! moving, who is connected, and what is the server saying about it. The first
//! three come from nats-server's HTTP monitoring port; the fourth is its log
//! file, tailed.
//!
//! ## Why this exists next to the monitoring port it reads
//!
//! `/varz`, `/connz` and `/jsz` already answer these questions, and `curl | jq`
//! is a real option. Three things make the difference:
//!
//! 1. **The monitoring port takes no credential.** It is protected by being
//!    unreachable — loopback, in every configuration in this fleet — so there
//!    is no way to look at it from a laptop. This app is a thing app-lb can put
//!    behind Google sign-in, which is what makes the answer available to
//!    somebody who is not already on the host.
//! 2. **`/varz` has no rates.** Its message counters are cumulative since the
//!    server started, so a single scrape cannot distinguish a server saturating
//!    a link from one that has been idle since last Tuesday. Throughput is a
//!    difference between two scrapes, and something has to hold the first.
//! 3. **Depth alone does not say whether a queue is stuck.** A `WorkQueue`
//!    stream holds zero messages when it is healthy *and* when nothing is
//!    subscribed at all; what separates them is whether a consumer's ack floor
//!    is moving. That is a join across two endpoints, and it is the thing
//!    somebody paged at 3am actually needs.
//!
//! ## What it cannot do
//!
//! It never opens a NATS client connection. There is no code path in this
//! binary that can publish, subscribe, bind a consumer to a stream, or reach
//! `$SYS.REQ.SERVER.<id>.SHUTDOWN`. That is a stronger guarantee than a
//! read-only system-account credential, which is the other way to observe every
//! account at once — and it is why this is built on the monitoring port.
//!
//! Nothing is persisted. History lives in memory and is lost on restart, which
//! is the right trade for a live view: app-obs is the thing in this repository
//! that keeps logs and metrics, and pointing nats-server's log at it is the
//! answer to "what happened last Tuesday".

// The platform UI kit — tokens, the theme cookie and forwarded identity —
// shared with app-lb, app-obs, ci, heyosecret and artifacts. Included by path
// rather than depended on as a crate: the apps sit on three axum versions, so
// the shared module names no framework type. See `ui/README.md`.
mod api;
mod config;
#[path = "../../ui/ui.rs"]
pub mod heyo_ui;
mod logs;
mod monitor;
mod state;

use api::ApiState;
use config::Config;
use logs::LogBuffer;
use monitor::Monitor;
use state::Store;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,queue=debug".into()),
        )
        .init();

    let cfg = Config::from_env();
    tracing::info!(
        monitor = %cfg.monitor_url,
        api = %cfg.api_addr,
        poll_secs = cfg.poll_interval.as_secs(),
        log_file = cfg.log_file.as_deref().unwrap_or("(disabled)"),
        "starting queue",
    );
    if cfg.api_token.is_none() {
        tracing::warn!(
            "the dashboard is unauthenticated (set QUEUE_API_TOKEN before exposing \
             QUEUE_API_ADDR anywhere but loopback behind an app-lb auth gate); it shows \
             every account's stream names, depths and connected client addresses",
        );
    }

    let store = Arc::new(Store::new(&cfg));
    let monitor = Monitor::new(&cfg.monitor_url, cfg.request_timeout);
    let log_buffer = Arc::new(LogBuffer::new(cfg.log_lines, cfg.log_file.clone()));

    // One shutdown signal, watched by both background tasks. A `watch` rather
    // than a `oneshot` because two of them need it, and rather than dropping a
    // channel because each task also selects on its own work.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(state::run(
        store.clone(),
        monitor,
        cfg.poll_interval,
        cfg.max_clients,
        shutdown_rx.clone(),
    ));
    tokio::spawn(logs::run(
        log_buffer.clone(),
        cfg.log_prime_bytes,
        shutdown_rx,
    ));

    let api_state = ApiState {
        store,
        logs: log_buffer,
        api_token: cfg.api_token.clone().map(Arc::new),
        ui_cookies: Arc::new(heyo_ui::CookieConfig::from_env("QUEUE")),
    };

    // Binding is the one failure worth exiting for: a dashboard nobody can
    // reach has nothing to do, and supervisord restarting it is more likely to
    // resolve a port conflict than this process retrying in place.
    let listener = match tokio::net::TcpListener::bind(&cfg.api_addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(addr = %cfg.api_addr, error = %e, "cannot bind the API listener");
            std::process::exit(1);
        }
    };
    tracing::info!(addr = %cfg.api_addr, "api listening");

    let served = axum::serve(listener, api::router(api_state))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Err(e) = served {
        tracing::error!(error = %e, "api server stopped");
    }

    // Stop the pollers too, so the process exits rather than lingering on two
    // tasks that would happily scrape forever.
    let _ = shutdown_tx.send(true);
}

/// Resolve on SIGTERM (what a supervisor sends) or Ctrl-C (what a terminal
/// does).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}
