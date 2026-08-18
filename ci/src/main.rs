//! CI orchestration and dashboard for heyvm networks.
//!
//! A machine becomes a runner by running `heyvmd` and joining a heyvm network —
//! there is no agent to install. This process discovers those hosts, opens an
//! iroh tunnel to each, and drives workflow jobs on them: claiming or creating a
//! VM from a fingerprinted pool, running each step through the daemon's async
//! exec-operation API, and streaming the output back.
//!
//! Startup order is deliberate. Config is resolved and *fully* validated before
//! anything binds a port or dials NATS, so a misconfiguration is a non-zero exit
//! with a message naming the variable — not a process that supervisord reports
//! as `RUNNING` while every job fails.

mod artifacts;
mod bus;
mod config;
mod dispatch;
mod expr;
mod image;
mod nats_auth;
mod objects;
mod paths;
mod plan;
mod pool;
mod repos;
mod runners;
mod secrets;
mod store;
mod trigger;
mod vm;
mod web;
mod workflow;

use bus::Bus;
use config::Config;
use dispatch::Dispatcher;
use pool::Pool;
use runners::{RunnerError, Runners};
use std::sync::Arc;
use std::time::Duration;
use store::Store;
use vm::Vms;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ci=debug".into()),
        )
        .init();

    let config = match Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            // stderr as well as the log: a startup failure has to be visible in
            // `supervisorctl tail` even when the log filter is set to `error`.
            eprintln!("ci: refusing to start — {e}");
            tracing::error!("refusing to start: {e}");
            std::process::exit(1);
        }
    };

    if config.nats.credential_from_url {
        tracing::warn!(
            "the NATS credential arrived inside CI_NATS_URL, which is visible in \
             shell history and process listings; prefer CI_NATS_TOKEN, \
             CI_NATS_USER/CI_NATS_PASSWORD, CI_NATS_CREDS or CI_NATS_NKEY"
        );
    }

    tracing::info!("{} starting — {}", config.name, config.summary());

    let secrets_client = secrets::Secrets::new(&config);
    let objects = Arc::new(objects::Workflows::new(&config));
    let runners = Arc::new(Runners::new(config.clone()));
    // The first read of the pool is awaited so a network that does not exist is
    // reported now rather than by the first job. Which failures are fatal is the
    // distinction that matters: naming a network wrongly is a configuration
    // error and behaves like one, while a cloud that is merely unreachable
    // right now must not stop the dashboard from serving — the refresh loop
    // will pick the pool up when it recovers.
    match runners.refresh().await {
        Ok(()) => {
            let pool = runners.snapshot();
            for set in pool.served() {
                tracing::info!(
                    "serving network {} ({}): {} runner(s), {} online{}",
                    set.network_name,
                    set.network_id,
                    set.runners.len(),
                    set.dispatchable().count(),
                    if set.network_id == pool.default_network_id {
                        " — the default for a repository that names none"
                    } else {
                        ""
                    },
                );
            }
            // Named, because "I assigned that network and nothing built" is
            // otherwise answered only by reading a page nobody thought to open.
            let unserved: Vec<&str> = pool
                .networks
                .iter()
                .filter(|n| !n.served)
                .map(|n| n.network_name.as_str())
                .collect();
            if !unserved.is_empty() {
                tracing::info!(
                    "not serving {} other network(s) on this account: {}. Add one to \
                     CI_NETWORK, or set CI_NETWORK=*, to build for it.",
                    unserved.len(),
                    unserved.join(", ")
                );
            }
        }
        Err(e @ (RunnerError::UnknownNetwork { .. } | RunnerError::AmbiguousNetwork { .. })) => {
            eprintln!("ci: refusing to start — {e}");
            tracing::error!("refusing to start: {e}");
            std::process::exit(1);
        }
        Err(e) => {
            tracing::warn!("could not read the runner pool at startup, will retry: {e}");
        }
    }
    runners.clone().spawn_refresh_loop();

    // Postgres before NATS: a bad connection string is far more common than a
    // bad NATS one, and failing on the likelier cause first makes the message
    // people actually see the useful one.
    let store = match Store::connect(&config.database_url, config.log_dir.clone()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ci: refusing to start — {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = store
        .migrate(std::path::Path::new(&config.migrations_dir))
        .await
    {
        eprintln!("ci: refusing to start — {e}");
        std::process::exit(1);
    }
    tracing::info!("database ready");

    let bus = match Bus::connect(&config.nats, &config.nats_prefix).await {
        Ok(b) => Arc::new(b),
        Err(e) => {
            eprintln!("ci: refusing to start — {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(
        "jetstream ready: {} / {}",
        bus.jobs_stream(),
        bus.events_stream()
    );

    let artifacts = match artifacts::sink_for(&config) {
        Ok(s) => Arc::from(s),
        Err(e) => {
            eprintln!("ci: refusing to start — {e}");
            std::process::exit(1);
        }
    };
    // Said once, at startup, because it is the one configuration where the
    // repositories page has no gate of its own — and minting a submit token is
    // minting the right to run code on a runner.
    if config.admin_emails.is_empty() {
        tracing::warn!(
            "CI_ADMIN_EMAILS is empty, so /repos accepts a request that carries no app-lb \
             identity: on a deployment reachable by anyone, anyone can register a \
             repository and mint a submit token. Set CI_ADMIN_EMAILS."
        );
    }

    if !secrets_client.is_configured() {
        tracing::warn!(
            "heyosecret is not configured, so `${{{{ secrets.* }}}}` will resolve empty. \
             Set CI_HEYOSECRET_URL and CI_HEYOSECRET_TOKEN."
        );
    }

    let dispatcher = Arc::new(Dispatcher {
        config: config.clone(),
        store: store.clone(),
        pool: Pool::new(store.pool().clone()),
        images: image::Catalog::new(store.pool().clone()),
        bus: bus.clone(),
        runners: runners.clone(),
        vms: Arc::new(Vms::new()),
        secrets: secrets_client,
        artifacts,
        objects: objects.clone(),
    });

    // One eager read so a misconfigured CI_APP_LB_URL is visible at startup
    // rather than at the first submit.
    if objects.is_configured() {
        match objects.refresh().await {
            Ok(()) => tracing::info!(
                "{} workflow object(s) registered in app-lb",
                objects.snapshot().workflows.len()
            ),
            Err(e) => tracing::warn!("could not read workflow objects at startup: {e}"),
        }
    }
    objects.clone().spawn_refresh_loop();

    spawn_log_sweeper(config.clone(), store.clone());

    // A previous process may have died holding VMs. Reclaim before taking work,
    // or the pool leaks its capacity one restart at a time. This instance's own
    // id is fresh, so VMs leased by the process this one replaced no longer look
    // like somebody's live work.
    if let Err(e) = dispatcher.reclaim_pool().await {
        tracing::warn!("could not reclaim the VM pool: {e}");
    }
    dispatcher.clone().spawn_lease_loop();
    dispatcher.clone().spawn_consumers();

    // Bind before announcing readiness. A listener that cannot bind is a hard
    // failure here rather than a task that dies quietly and leaves the process
    // up with no data plane.
    let listener = match tokio::net::TcpListener::bind(config.listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "ci: cannot bind CI_LISTEN_ADDR={} — {e}",
                config.listen_addr
            );
            std::process::exit(1);
        }
    };

    let app = web::router(
        config.clone(),
        runners.clone(),
        store.clone(),
        dispatcher.clone(),
    );
    tracing::info!("listening on http://{}", config.listen_addr);

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("server stopped: {e}");
        std::process::exit(1);
    }
    tracing::info!("shut down cleanly");
}

/// Delete step and VM logs older than `CI_LOG_RETENTION_DAYS`.
///
/// Logs are the bulk of what this process writes and nothing else prunes them —
/// a build log is megabytes, and an orchestrator that fills its disk stops being
/// an orchestrator. The rows stay: a step that ran and its exit code are the
/// run's history, and losing those with the bytes would make an old run look as
/// though it never happened.
///
/// Bounded per pass rather than deleting everything found. A first sweep against
/// months of history would otherwise be one enormous burst of unlink syscalls on
/// the same disk a build is writing to.
fn spawn_log_sweeper(config: Arc<Config>, store: Store) {
    let Some(retention) = config.log_retention else {
        tracing::info!("CI_LOG_RETENTION_DAYS=0, so logs are kept forever; watch the disk");
        return;
    };
    /// How often to look. Logs age in days; checking hourly is prompt enough and
    /// keeps each pass small.
    const EVERY: Duration = Duration::from_secs(3600);
    /// Runs per pass.
    const BATCH: i64 = 200;

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(EVERY);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Ok(cutoff) = chrono::Duration::from_std(retention) else {
                tracing::error!("CI_LOG_RETENTION_DAYS is out of range; not sweeping");
                return;
            };
            let cutoff = chrono::Utc::now() - cutoff;

            let runs = match store.runs_with_logs_before(cutoff, BATCH).await {
                Ok(runs) => runs,
                Err(e) => {
                    tracing::warn!("could not list runs to sweep: {e}");
                    continue;
                }
            };
            if runs.is_empty() {
                continue;
            }

            let mut swept = 0u64;
            for run_id in &runs {
                let dir = store.run_log_dir(run_id);
                // A missing directory is not an error: the files may have been
                // removed by hand, or the run may have written none. The rows
                // are still cleared so it is not offered again.
                if let Err(e) = tokio::fs::remove_dir_all(&dir).await
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!("could not remove {}: {e}", dir.display());
                    continue;
                }
                match store.forget_logs_of(run_id).await {
                    Ok(n) => swept += n,
                    Err(e) => tracing::warn!("swept {run_id} on disk but not in the database: {e}"),
                }
            }
            tracing::info!(
                "log sweep: {} run(s) older than {} day(s), {swept} step log(s) discarded",
                runs.len(),
                retention.as_secs() / 86_400
            );
        }
    });
}

/// SIGTERM as well as ctrl-c: supervisord stops a program with `TERM`, and a
/// process that only handles ctrl-c gets killed after `stopwaitsecs` instead of
/// draining.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            // Nothing useful to do if the handler cannot be installed; fall
            // back to ctrl-c alone rather than refusing to serve.
            Err(e) => {
                tracing::warn!("could not install a SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = ctrl_c => tracing::info!("received ctrl-c, draining"),
        _ = terminate => tracing::info!("received SIGTERM, draining"),
    }
}
