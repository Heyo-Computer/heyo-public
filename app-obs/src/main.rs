//! app-obs: logs and metrics for the deployments app-lb manages.
//!
//! Collects logs pushed by guests (HTTP and syslog) and metrics polled from
//! app-lb's admin API, stores them as partitioned parquet, and ages partitions
//! out. Runs as a static `proxy_pass` deployment registered with app-lb — it is
//! a host process, not a microVM, so it is fronted the same way the pg-fc
//! dashboard is.

mod api;
mod config;
mod ingest;
mod query;
mod retention;
mod sources;
mod store;

use api::ApiState;
use config::Config;
use ingest::Sink;
use ingest::http::IngestState;
use query::Engine;
use retention::Retention;
use sources::applb::Poller;
use sources::nats::{Config as NatsConfig, Publisher as NatsPublisher, Stats as NatsStats};
use std::sync::Arc;
use store::schema::Record;
use store::writer::Writer;
use tokio::sync::mpsc;

/// How many records to take off the queue per blocking excursion.
///
/// Everything below the ingest queue is synchronous file work. Handling records
/// one at a time would mean one `block_in_place` per record, whose overhead
/// dwarfs the `Vec::push` that most records actually cost. Draining in batches
/// amortises that down to once per batch.
const DRAIN_BATCH: usize = 1024;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,app_obs=debug".into()),
        )
        .init();

    let cfg = Config::from_env();

    // Fail before binding anything if the data directory isn't usable. A
    // collector that accepts records it cannot store is worse than one that
    // refuses to start, because the sender believes they were kept.
    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        panic!("cannot create data directory {}: {e}", cfg.data_dir);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime");

    runtime.block_on(run(cfg));
}

async fn run(cfg: Config) {
    tracing::info!(
        data_dir = %cfg.data_dir,
        ingest = %cfg.ingest_addr,
        syslog = %cfg.syslog_addr,
        api = %cfg.api_addr,
        retain_days = cfg.retain_days,
        "starting app-obs",
    );
    if cfg.ingest_token.is_none() {
        tracing::warn!(
            "ingest is unauthenticated (set APP_OBS_INGEST_TOKEN); anything that can \
             reach the ingest port can write records",
        );
    }

    let (sink, rx) = Sink::new(cfg.queue_capacity);

    // The writer owns everything below the queue and is the only thing that
    // touches the data directory, so it needs no locking.
    let writer = Writer::new(&cfg.data_dir, cfg.flush_rows, cfg.flush_interval);
    // Taken before the writer moves into the drain task; it is the only window
    // onto how much is buffered but not yet queryable.
    let buffered = writer.buffered_handle();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let drain = tokio::spawn(drain(rx, writer, cfg.flush_interval, shutdown_rx));

    let ingest_state = IngestState {
        sink: sink.clone(),
        token: cfg.ingest_token.clone().map(Arc::new),
    };
    let ingest_addr = cfg.ingest_addr.clone();
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(&ingest_addr).await {
            Ok(listener) => {
                tracing::info!(addr = %ingest_addr, "ingest listening");
                if let Err(e) = axum::serve(listener, ingest::http::router(ingest_state)).await {
                    tracing::error!(error = %e, "ingest server stopped");
                }
            }
            Err(e) => tracing::error!(addr = %ingest_addr, error = %e, "ingest failed to bind"),
        }
    });

    let syslog_config = ingest::syslog::SyslogConfig {
        addr: cfg.syslog_addr.clone(),
        default_deployment: "syslog".into(),
    };
    let syslog_sink = sink.clone();
    tokio::spawn(async move {
        if let Err(e) = ingest::syslog::serve(syslog_config, syslog_sink).await {
            tracing::error!(error = %e, "syslog listener failed to start");
        }
    });

    // The poller tells the daemon log tailer which sandboxes exist and which
    // deployment each serves; the channel starts empty so the tailer idles
    // until the first successful poll.
    let (targets_tx, targets_rx) = tokio::sync::watch::channel(Vec::new());
    let (live_tx, live_rx) = tokio::sync::watch::channel(None);
    let telemetry_stats = cfg
        .nats_url
        .as_ref()
        .map(|_| Arc::new(NatsStats::new(cfg.nats_subject.clone())));
    let telemetry = cfg.nats_url.as_ref().map(|url| {
        NatsPublisher::new(
            NatsConfig {
                url: url.clone(),
                user: cfg.nats_user.clone(),
                password: cfg.nats_password.clone(),
                token: cfg.nats_token.clone(),
                subject: cfg.nats_subject.clone(),
            },
            telemetry_stats
                .as_ref()
                .expect("stats exist whenever NATS is configured")
                .clone(),
        )
        .spawn()
    });
    tokio::spawn(
        Poller::new(
            &cfg.applb_url,
            cfg.applb_user.clone(),
            cfg.applb_password.clone(),
            cfg.poll_interval,
            sink.clone(),
            cfg.source.clone(),
            live_tx,
            telemetry,
            Some(targets_tx),
        )
        .run(),
    );

    match cfg.heyvm_url.clone() {
        Some(heyvm_url) => {
            tracing::info!(url = %heyvm_url, "tailing native sandbox logs from the daemon");
            tokio::spawn(
                sources::heyvm::Tailers::new(
                    heyvm_url,
                    cfg.heyvm_token.clone(),
                    targets_rx,
                    sink.clone(),
                )
                .run(),
            );
        }
        None => tracing::info!(
            "daemon log tailing disabled (set HEYVM_URL to collect sandbox \
             stdout/stderr/console without an in-guest shipper)",
        ),
    }

    tokio::spawn(Retention::new(&cfg.data_dir, cfg.retain_days).run());

    // The query layer and the dashboard it serves. Building it creates the two
    // table directories, which is the same condition the writer needs to flush
    // at all — so a failure here means this process could never have stored
    // anything, and starting anyway would only accept records to lose them.
    let engine = match Engine::new(&cfg.data_dir, cfg.query_concurrency, cfg.query_timeout).await {
        Ok(engine) => Arc::new(engine),
        Err(e) => panic!("cannot open the query layer over {}: {e}", cfg.data_dir),
    };

    let api_state = ApiState {
        engine,
        sink: sink.clone(),
        buffered,
        flush_secs: cfg.flush_interval.as_secs(),
        retain_days: cfg.retain_days,
        live: live_rx,
        stale_after_secs: cfg.poll_interval.as_secs().saturating_mul(3).max(15),
        telemetry: telemetry_stats,
    };
    let api_addr = cfg.api_addr.clone();
    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(&api_addr).await {
            Ok(listener) => {
                tracing::info!(addr = %api_addr, "api listening");
                if let Err(e) = axum::serve(listener, api::router(api_state)).await {
                    tracing::error!(error = %e, "api server stopped");
                }
            }
            Err(e) => tracing::error!(addr = %api_addr, error = %e, "api failed to bind"),
        }
    });

    shutdown_signal().await;
    tracing::info!("shutting down; flushing buffered records");

    // Signal the drain loop explicitly rather than relying on the queue closing.
    // The sink is cloned into the ingest, syslog, poller, and API tasks, all of
    // which are still running here, so dropping this one closes nothing — the
    // loop would wait for senders that never go away and the flush would be lost
    // to the timeout.
    let _ = shutdown_tx.send(());
    drop(sink);

    match tokio::time::timeout(std::time::Duration::from_secs(30), drain).await {
        Ok(Ok(())) => tracing::info!("flushed cleanly"),
        Ok(Err(e)) => tracing::error!(error = %e, "drain task failed"),
        Err(_) => tracing::error!("timed out flushing; some buffered records were lost"),
    }
}

/// Move records from the queue into the writer, flushing on age as well as size.
///
/// Runs as one task so the writer needs no synchronisation. The file work is
/// synchronous, so it happens inside `block_in_place` — without that, a parquet
/// flush would stall an executor thread that is also serving ingest requests.
async fn drain(
    mut rx: mpsc::Receiver<Record>,
    mut writer: Writer,
    flush_interval: Duration,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(flush_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut batch = Vec::with_capacity(DRAIN_BATCH);

    loop {
        tokio::select! {
            received = rx.recv_many(&mut batch, DRAIN_BATCH) => {
                if received == 0 {
                    break; // every sender is gone
                }
                write_batch(&mut writer, &mut batch);
            }
            _ = ticker.tick() => {
                tokio::task::block_in_place(|| {
                    if let Err(e) = writer.flush_expired() {
                        tracing::error!(error = %e, "periodic flush failed");
                    }
                });
            }
            _ = &mut shutdown => break,
        }
    }

    // Take whatever is already queued before flushing. `try_recv` rather than
    // `recv`, so a producer still racing to send can't hold shutdown open.
    let mut remaining = 0;
    while let Ok(record) = rx.try_recv() {
        batch.push(record);
        remaining += 1;
        if batch.len() >= DRAIN_BATCH {
            write_batch(&mut writer, &mut batch);
        }
    }
    write_batch(&mut writer, &mut batch);
    if remaining > 0 {
        tracing::info!(records = remaining, "drained queue on shutdown");
    }

    tokio::task::block_in_place(|| match writer.flush_all() {
        Ok(n) => tracing::info!(partitions = n, "final flush complete"),
        Err(e) => tracing::error!(error = %e, "final flush failed"),
    });
}

/// Push a batch into the writer, emptying it.
///
/// Wrapped in `block_in_place` because everything below here is synchronous file
/// work; without it a parquet flush would stall an executor thread that is also
/// serving ingest requests.
fn write_batch(writer: &mut Writer, batch: &mut Vec<Record>) {
    if batch.is_empty() {
        return;
    }
    tokio::task::block_in_place(|| {
        for record in batch.drain(..) {
            if let Err(e) = writer.push(record) {
                // A rejected record is bad input, not a broken writer — log it
                // and keep going.
                tracing::warn!(error = %e, "dropping unwritable record");
            }
        }
    });
}

use std::time::Duration;

/// Resolve on SIGTERM (what a supervisor sends) or Ctrl-C (what a terminal does).
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
}
