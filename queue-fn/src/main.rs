//! queue-fn: serverless functions on heyvm Firecracker microVMs.
//!
//! Register a function (a VM template plus a command), and events published to
//! NATS JetStream run it inside a microVM, with the fleet sized to the queue.
//!
//! Unlike app-lb — which runs axum inside a pingora `BackgroundService` because
//! pingora owns its runtime — queue-fn has no data plane and so owns its own:
//! plain `#[tokio::main]`, `tokio::spawn`ed workers, and a `watch` channel
//! standing in for pingora's `ShutdownWatch` (which is a `watch::Receiver<bool>`
//! underneath, so every select-loop shape ports over unchanged).

mod admin;
mod autoscale;
mod bus;
mod cloud;
mod config;
mod dispatch;
mod function;
mod invoke;
mod metrics;
mod nats_auth;
mod registry;
mod results;
mod schedule;
mod vm;

use crate::admin::AdminApi;
use crate::autoscale::Autoscaler;
use crate::bus::Bus;
use crate::cloud::CloudState;
use crate::config::QfnConfig;
use crate::dispatch::Dispatcher;
use crate::metrics::Metrics;
use crate::nats_auth::{EnvCredentials, NatsEndpoint};
use crate::registry::Registry;
use crate::results::Results;
use crate::vm::VmManager;
use std::sync::Arc;
use std::time::Duration;

fn env_flag(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Parse a numeric env var, or keep the default.
///
/// A malformed value warns and falls back rather than failing to start: a typo
/// in a tuning knob should not take a running service down, and the default it
/// falls back to is by construction a working one.
fn env_num<T: std::str::FromStr>(name: &str) -> Option<T> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().parse() {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(var = name, value = %raw, "ignoring unparseable value; using the default");
            None
        }
    }
}

fn config_from_env() -> QfnConfig {
    let mut cfg = QfnConfig::default();
    if let Ok(v) = std::env::var("QFN_ADMIN_ADDR") {
        cfg.admin_addr = v;
    }
    if let Ok(v) = std::env::var("QFN_STATE_PATH") {
        cfg.state_path = v;
    }
    if let Ok(v) = std::env::var("QFN_NAME") {
        cfg.name = v;
    }
    if let Ok(v) = std::env::var("QFN_DAEMON_URL") {
        cfg.daemon_url = Some(v);
    }
    if let Ok(v) = std::env::var("QFN_NATS_URL") {
        cfg.nats_url = v;
    }
    if let Ok(v) = std::env::var("QFN_NATS_SUBJECT_PREFIX") {
        cfg.subject_prefix = v;
    }
    if let Some(v) = env_num("QFN_MAX_PAYLOAD_BYTES") {
        cfg.max_payload_bytes = v;
    }
    if let Some(v) = env_num("QFN_RESULT_RING") {
        cfg.result_ring = v;
    }
    if let Some(v) = env_num("QFN_SYNC_INVOKE_TIMEOUT_SECS") {
        cfg.sync_invoke_timeout_secs = v;
    }
    if let Ok(v) = std::env::var("QFN_DASHBOARD_USER") {
        cfg.dashboard_user = Some(v);
    }
    if let Ok(v) = std::env::var("QFN_DASHBOARD_PASSWORD") {
        cfg.dashboard_password = Some(v);
    }
    if let Some(v) = env_flag("QFN_ADMIN_AUTH") {
        cfg.admin_auth = v;
    }
    if let Some(v) = env_flag("QFN_INVOKE_AUTH") {
        cfg.invoke_auth = v;
    }
    if let Some(v) = env_flag("QFN_CLOUD_OBSERVE") {
        cfg.cloud_observe = v;
    }
    if let Ok(v) = std::env::var("QFN_CLOUD_STREAM") {
        cfg.cloud_stream = v;
    }
    if let Ok(v) = std::env::var("QFN_CLOUD_SUBJECT") {
        cfg.cloud_subject = v;
    }
    if let Some(v) = env_num("QFN_CLOUD_JOB_RING") {
        cfg.cloud_job_ring = v;
    }
    if let Some(v) = env_num("QFN_CLOUD_POLL_SECS") {
        cfg.cloud_poll_secs = v;
    }
    cfg
}

/// Read the discrete NATS credential vars.
///
/// Separate from `config_from_env` because these never land in `QfnConfig`: it
/// is serialized into the state file and returned by the admin API, and a
/// credential that is only ever held in a `Credential` cannot be leaked by
/// either.
fn nats_credentials_from_env() -> EnvCredentials {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    EnvCredentials {
        user: var("QFN_NATS_USER"),
        password: var("QFN_NATS_PASSWORD"),
        token: var("QFN_NATS_TOKEN"),
        creds_file: var("QFN_NATS_CREDS"),
        nkey_seed: var("QFN_NATS_NKEY"),
    }
}

/// Reject a configuration that asks for a guarantee it cannot deliver.
///
/// Returned rather than panicked inline so it is testable; `main` panics on it.
fn check_config(cfg: &QfnConfig) -> Result<(), String> {
    // A gate with no credential enforces nothing. Failing to start is the only
    // honest response — the alternative is a service that reports itself
    // protected while standing wide open.
    if (cfg.admin_auth || cfg.invoke_auth) && cfg.dashboard_password.is_none() {
        return Err(
            "QFN_ADMIN_AUTH/QFN_INVOKE_AUTH is set but QFN_DASHBOARD_PASSWORD is not; \
             the gate reuses the dashboard credentials, so set a password or unset the flag"
                .into(),
        );
    }
    if cfg.max_payload_bytes > config::PAYLOAD_CEILING_BYTES {
        return Err(format!(
            "QFN_MAX_PAYLOAD_BYTES ({}) exceeds the host ceiling of {}: the payload \
             crosses a serial console as one command line, and the tty line \
             discipline buffers {} bytes",
            cfg.max_payload_bytes,
            config::PAYLOAD_CEILING_BYTES,
            config::PAYLOAD_CEILING_BYTES,
        ));
    }
    if cfg.result_ring == 0 {
        return Err("QFN_RESULT_RING must be greater than 0".into());
    }
    if cfg.cloud_observe {
        if cfg.cloud_job_ring == 0 {
            return Err("QFN_CLOUD_JOB_RING must be greater than 0".into());
        }
        if cfg.cloud_poll_secs == 0 {
            return Err(
                "QFN_CLOUD_POLL_SECS must be greater than 0; a zero interval would spin \
                 the cloud's JetStream management API as fast as the loop can run"
                    .into(),
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,queue_fn=debug".into()),
        )
        .init();

    let cfg = config_from_env();
    if let Err(e) = check_config(&cfg) {
        panic!("{e}");
    }

    let registry = Arc::new(Registry::new(&cfg.state_path));
    match registry.load() {
        Ok(0) => {
            tracing::info!(path = %registry.persist_path().display(), "no persisted functions")
        }
        Ok(n) => tracing::info!(count = n, "restored functions"),
        Err(e) => tracing::error!(error = %e, "failed to load persisted state; starting empty"),
    }

    // Resolved before anything else touches the URL, so the credential is
    // separated from the address once and every later use is of the sanitized
    // form. `cfg.nats_url` must not be logged after this point.
    let endpoint = match NatsEndpoint::resolve(&cfg.nats_url, &nats_credentials_from_env()) {
        Ok(e) => e,
        Err(e) => panic!("{e}"),
    };
    if endpoint.credential_from_url {
        tracing::warn!(
            "the NATS credential came from QFN_NATS_URL; prefer QFN_NATS_TOKEN, \
             QFN_NATS_CREDS, or QFN_NATS_USER/QFN_NATS_PASSWORD, since a URL is \
             visible in shell history and process listings"
        );
    }

    // NATS is not optional: without it there is no queue, and every invocation
    // path runs through one. Failing to start beats coming up in a state where
    // registration succeeds and nothing ever runs.
    let bus = match Bus::connect(&endpoint, cfg.subject_prefix.clone()).await {
        Ok(b) => Arc::new(b),
        Err(e) => panic!(
            "could not connect to NATS at {} using {} auth: {e}",
            endpoint.redacted(),
            endpoint.credential,
        ),
    };
    if let Err(e) = bus.ensure_streams().await {
        panic!("could not create the JetStream streams: {e}");
    }

    // Restored functions need their consumers back before anything is published
    // to them — a subject with no consumer accepts events nothing will pull.
    for spec in registry.specs() {
        if let Err(e) = bus.ensure_consumer(&spec).await {
            tracing::error!(function = %spec.id, error = %e, "could not restore the consumer");
        }
    }

    let vms = VmManager::new(cfg.daemon_url.clone());
    let metrics = Arc::new(Metrics::new());
    let results = Arc::new(Results::new(cfg.result_ring));

    let autoscaler = Arc::new(Autoscaler::new(
        registry.clone(),
        vms.clone(),
        metrics.clone(),
        bus.clone(),
    ));

    // Adopt before anything can dispatch. app-lb gets this ordering from
    // pingora's service dependencies; here it is an explicit await, so no
    // invocation is handed to a VM the pool has not yet accounted for, and a
    // restart doesn't boot a second fleet alongside the first.
    autoscaler.adopt_existing().await;

    // Shutdown fans out to every spawned worker. `watch` rather than broadcast:
    // a late subscriber still sees the final value, so a task spawned during
    // shutdown exits immediately instead of blocking forever.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // heyo cloud's queue, if it shares this NATS. Read-only in both directions;
    // `None` when disabled, which the dashboard renders as an absent section
    // rather than an empty one.
    let cloud = cfg.cloud_observe.then(|| {
        Arc::new(CloudState::new(
            cfg.cloud_stream.clone(),
            cfg.cloud_subject.clone(),
            cfg.cloud_job_ring,
        ))
    });

    tracing::info!(
        admin = %cfg.admin_addr,
        nats = %endpoint.redacted(),
        nats_auth = %endpoint.credential,
        functions = registry.functions().len(),
        "starting queue-fn",
    );

    let mut tasks = vec![
        tokio::spawn({
            let a = autoscaler.clone();
            let sd = shutdown_rx.clone();
            async move { a.run(sd).await }
        }),
        tokio::spawn(
            Dispatcher {
                registry: registry.clone(),
                bus: bus.clone(),
                vms,
                metrics: metrics.clone(),
                results: results.clone(),
            }
            .supervise(shutdown_rx.clone()),
        ),
        tokio::spawn(schedule::run(
            registry.clone(),
            bus.clone(),
            cfg.max_payload_bytes,
            shutdown_rx.clone(),
        )),
        tokio::spawn(
            AdminApi::new(
                &cfg,
                registry,
                autoscaler,
                metrics,
                results,
                bus.clone(),
                cloud.clone(),
            )
            .serve(shutdown_rx.clone()),
        ),
    ];

    if let Some(cloud) = cloud {
        tracing::info!(
            stream = %cfg.cloud_stream,
            subject = %cfg.cloud_subject,
            poll_secs = cfg.cloud_poll_secs,
            "watching the cloud queue",
        );
        tasks.push(tokio::spawn(cloud::run(
            cloud,
            bus,
            Duration::from_secs(cfg.cloud_poll_secs),
            shutdown_rx.clone(),
        )));
    }

    tokio::signal::ctrl_c().await.ok();
    tracing::info!("shutting down");
    let _ = shutdown_tx.send(true);

    // Bounded: an invocation in flight gets a chance to finish and ack rather
    // than being abandoned into a redelivery, but a wedged task cannot hold the
    // process open forever.
    for task in tasks {
        if tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .is_err()
        {
            tracing::warn!("a worker did not stop in time");
        }
    }
    tracing::info!("stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gate_without_a_credential_is_rejected() {
        let mut cfg = QfnConfig {
            admin_auth: true,
            ..Default::default()
        };
        assert!(check_config(&cfg).is_err(), "admin gate with no password");

        cfg.admin_auth = false;
        cfg.invoke_auth = true;
        assert!(check_config(&cfg).is_err(), "invoke gate with no password");

        cfg.dashboard_password = Some("s3cret".into());
        assert!(check_config(&cfg).is_ok());
    }

    #[test]
    fn an_ungated_default_config_is_valid() {
        assert!(check_config(&QfnConfig::default()).is_ok());
    }

    #[test]
    fn a_payload_ceiling_above_the_host_limit_is_rejected() {
        let cfg = QfnConfig {
            max_payload_bytes: config::PAYLOAD_CEILING_BYTES + 1,
            ..Default::default()
        };
        assert!(check_config(&cfg).is_err());
    }

    #[test]
    fn a_zero_length_result_ring_is_rejected() {
        let cfg = QfnConfig {
            result_ring: 0,
            ..Default::default()
        };
        assert!(check_config(&cfg).is_err());
    }

    /// A zero poll interval would hammer somebody else's JetStream management
    /// API, and a zero ring would keep no jobs to render — both are rejected at
    /// startup rather than discovered from the cloud's request counters.
    #[test]
    fn a_degenerate_cloud_observer_is_rejected() {
        let bad_ring = QfnConfig {
            cloud_job_ring: 0,
            ..Default::default()
        };
        assert!(check_config(&bad_ring).is_err());

        let bad_interval = QfnConfig {
            cloud_poll_secs: 0,
            ..Default::default()
        };
        assert!(check_config(&bad_interval).is_err());
    }

    /// …but only when the observer is switched on. Values that would be
    /// degenerate are irrelevant when nothing reads them, and failing to start
    /// over an unused knob would be a false alarm.
    #[test]
    fn cloud_settings_are_unchecked_when_observation_is_off() {
        let cfg = QfnConfig {
            cloud_observe: false,
            cloud_job_ring: 0,
            cloud_poll_secs: 0,
            ..Default::default()
        };
        assert!(check_config(&cfg).is_ok());
    }

    /// Regression: a typo'd numeric env var parsed as `0` and silently disabled
    /// the thing it configured. Falling back to the default keeps the process
    /// running with working settings and says so.
    #[test]
    fn an_unparseable_numeric_env_var_falls_back_to_the_default() {
        // SAFETY: single-threaded test, and the var is unique to this test.
        unsafe { std::env::set_var("QFN_TEST_BOGUS_NUM", "not-a-number") };
        assert_eq!(env_num::<usize>("QFN_TEST_BOGUS_NUM"), None);
        unsafe { std::env::set_var("QFN_TEST_BOGUS_NUM", "42") };
        assert_eq!(env_num::<usize>("QFN_TEST_BOGUS_NUM"), Some(42));
        unsafe { std::env::remove_var("QFN_TEST_BOGUS_NUM") };
    }

    #[test]
    fn flag_parsing_accepts_the_usual_spellings() {
        for (raw, want) in [
            ("1", true),
            ("true", true),
            ("TRUE", true),
            ("yes", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("", false),
        ] {
            // SAFETY: single-threaded test, and the var is unique to this test.
            unsafe { std::env::set_var("QFN_TEST_FLAG", raw) };
            assert_eq!(env_flag("QFN_TEST_FLAG"), Some(want), "raw {raw:?}");
        }
        unsafe { std::env::remove_var("QFN_TEST_FLAG") };
        assert_eq!(env_flag("QFN_TEST_FLAG"), None, "unset stays unset");
    }
}
