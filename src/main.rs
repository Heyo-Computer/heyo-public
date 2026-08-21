//! pg-vm-pool — a per-schema Postgres pooler over Heyo microVMs.
//!
//! Listens on one Postgres endpoint. The database name in each client's startup
//! packet selects a schema; the pooler lazily boots (or restarts/reuses) the
//! `pg-<schema>` Firecracker VM, tunnels to its Postgres, and splices the
//! connection through. One isolated VM per schema, behind a single URL.

mod auth;
mod config;
mod dashboard;
mod dedicated;
mod dumpsrv;
mod events;
mod imgarchive;
mod inventory;
/// Cold-start load harness. Test-only: it aims the whole pooler at an
/// in-process daemon stub, which is not something a running pooler should be
/// able to do by accident.
#[cfg(test)]
mod loadtest;
mod orphans;
mod pending;
mod proxy;
mod reclaim;
mod registry;
mod s3;
mod spares;
mod startup;
mod store;
mod tls;
mod vm;

use std::sync::Arc;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;
use registry::SchemaRegistry;
use tls::TlsReloader;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,pg_vm_pool=info")),
        )
        .init();

    let cfg = Config::from_env()?;
    // Fail startup, loudly and with the fix in the message, if any configured
    // offload tier's output directory can't be created or written — an
    // unwritable dir otherwise degrades every offload into its most expensive
    // fallback path at job time (see the method's docs for the full story).
    cfg.preflight_offload_dirs()?;
    let listen_addr = cfg.listen_addr;
    // File-backed event metrics (daily partitions) for the monitoring charts;
    // memory-only if the dir can't be created.
    events::init(cfg.metrics_dir.clone());
    // Client-facing TLS: certbot (or any external renewer) owns the PEM files;
    // the reloader picks up rotations without a restart. Built before the
    // registry so a bad cert fails startup fast.
    let tls = match (cfg.tls_cert.clone(), cfg.tls_key.clone()) {
        (Some(cert), Some(key)) => {
            info!(
                "TLS enabled (cert={}, hot-reload on change)",
                cert.display()
            );
            Some(Arc::new(TlsReloader::new(cert, key)?))
        }
        _ => {
            info!("TLS disabled (set PG_VM_POOL_TLS_CERT/KEY to enable)");
            None
        }
    };
    if cfg.pg_password.is_some() && tls.is_none() && !listen_addr.ip().is_loopback() {
        warn!(
            "PG_VM_POOL_PASSWORD is set but TLS is not and PG_VM_POOL_LISTEN \
             ({listen_addr}) is not loopback — client passwords will cross \
             the network in cleartext; set PG_VM_POOL_TLS_CERT/KEY"
        );
    }
    // Pull the dashboard settings out before `cfg` is moved into the registry.
    let dashboard_cfg = cfg.dashboard.clone();
    // Pending-bring-up ledger (in-flight creates awaiting their registry
    // binding) lives beside the registry file; load any crash leftovers before
    // the first connection can race them.
    pending::init(
        cfg.state_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("pending-bringups.tsv"),
    );
    let registry = Arc::new(SchemaRegistry::new(cfg));
    registry.spawn_reaper();
    // Stops running VMs nothing tracks (left over from a pooler restart, a
    // failed idle-stop, or a daemon-side boot) so the ladder can reclaim them.
    registry.spawn_untracked_reaper();
    // Offload pacer: trickles cold schemas down the storage ladder (compact →
    // freeze → S3), one schema at a time and only while no client is waiting
    // on a bring-up, instead of waking on a timer and running a long batch.
    // No-op unless at least one of COMPACT/FREEZE/ARCHIVE_AFTER_SECS is set.
    registry.spawn_offloader();
    // Emergency disk-pressure eviction: when the VM-disk filesystem crosses the
    // high-water mark, archive oldest-idle schemas (TTL overridden) until it
    // recovers. No-op unless PG_VM_POOL_PRESSURE_PATH is configured.
    registry.spawn_pressure_reaper();
    // Warm-spare pool: pre-booted empty VMs that cold bring-ups (notably S3
    // restores) claim instead of paying create + boot + initdb. No-op unless
    // PG_VM_POOL_WARM_SPARES > 0.
    registry.spawn_spare_replenisher();
    // Persists debounced activity bumps to the registry file (mapping changes
    // flush immediately inside the store; this loop only owes idle clocks).
    registry.spawn_store_flusher();
    // Carries the frozen tier's dump bytes both ways (and the S3 tier's
    // streamed dumps). No-op unless one of those tiers is configured.
    registry.spawn_dump_server();
    // Offline-trim stopped VMs' data disks so freed guest blocks return to the
    // host (Firecracker has no discard passthrough). No-op unless
    // PG_VM_POOL_RECLAIM_CMD is configured.
    registry.spawn_reclaimer();
    // Orphan-disk sweep: delete sb-<id>/ directories heyvmd has forgotten (a
    // kill it acked but didn't act on), reclaiming the stranded disk. No-op
    // unless PG_VM_POOL_ORPHAN_SWEEP_SECS (and PG_VM_POOL_RUN_DIR) are set.
    registry.spawn_orphan_reaper();
    // Delete VMs whose bring-up handed out an id but never reached a registry
    // binding — the "stuck in provisioning, bound to nothing" leak no other
    // sweep covers. Always on; idle when the pending ledger is empty.
    registry.spawn_pending_janitor();
    // A pooler that died mid-image-restore left up to a full raw image in the
    // run dir's dot-named scratch (invisible to the sb-* sweep). Clean that
    // up-front — this startup is the first moment we know no restore is
    // running — and let the sweep keep it clean from here.
    if let Some(run_dir) = registry.run_dir() {
        check_run_dir(&run_dir);
        crate::imgarchive::gc_restore_scratch(&run_dir);
    }

    // Optional admin dashboard: enabled only when PG_VM_POOL_DASHBOARD_LISTEN is
    // set. Runs in its own task sharing the registry, so it never blocks the PG
    // accept loop below.
    if let Some(dash) = dashboard_cfg {
        if !dash.listen.ip().is_loopback() && dash.basic_auth.is_none() {
            warn!(
                "dashboard bound to non-loopback {} without basic auth — anyone who \
                 can reach it can stop/resize every VM; set PG_VM_POOL_DASHBOARD_USER/\
                 PASSWORD or bind a loopback address",
                dash.listen
            );
        }
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = dashboard::serve(dash, registry).await {
                warn!("dashboard exited: {e:#}");
            }
        });
    }

    let listener = TcpListener::bind(listen_addr).await?;
    info!("pg-vm-pool listening on {listen_addr}");

    loop {
        let (sock, peer) = listener.accept().await?;
        // Disable Nagle on the client->pooler leg (mirrors the pooler->VM leg in
        // proxy::splice). The Postgres wire protocol is request/response, so
        // Nagle + delayed-ACK adds per-round-trip latency on the many small
        // statements a chatty client sends; a direct libpq connection sets
        // TCP_NODELAY itself, so the pooler must too to match it. Best-effort:
        // set on the raw socket before TLS wrapping (the option rides the fd).
        if let Err(e) = sock.set_nodelay(true) {
            warn!("could not set TCP_NODELAY on client connection {peer}: {e}");
        }
        // Arm keepalive on the same raw socket, for the same reason it has to
        // happen here: the option rides the fd, so it survives the TLS wrap.
        // Without it a client that vanishes without a FIN parks this task —
        // and the connection slot it is about to take — forever. See
        // `proxy::arm_keepalive`.
        proxy::arm_keepalive(&sock, "client->pooler");
        let registry = registry.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(sock, registry, tls).await {
                warn!("connection {peer} closed: {e:#}");
            }
        });
    }
}

async fn handle_conn(
    client: TcpStream,
    registry: Arc<SchemaRegistry>,
    tls: Option<Arc<TlsReloader>>,
) -> Result<()> {
    let (mut client, info) = startup::read_startup(client, tls.as_deref()).await?;
    let creds = registry.dedicated();
    // Which password this client must prove, decided from its *role* alone: a
    // provisioned role is challenged with its own, everyone else with the
    // shared `PG_VM_POOL_PASSWORD`. Keeping the requested database out of this
    // step means the handshake looks the same either way, so the challenge
    // can't be used to enumerate which database names are dedicated.
    if let Some(password) = creds.challenge_password(&info.user, registry.client_password()) {
        auth::require_password(&mut client, &password).await?;
    }
    // Authenticated — now, may this client route where it asked? A dedicated
    // credential may open only its own database (so it can never provision a
    // second VM), and a shared-password client may not open a dedicated one.
    if let Err(reason) = creds.authorize(&info.user, &info.database) {
        // Tell the client why rather than dropping the socket: "cannot open any
        // other database" is exactly the feedback that stops someone retrying a
        // typo'd database name forever.
        auth::send_fatal(&mut client, auth::SQLSTATE_INSUFFICIENT_PRIVILEGE, &reason).await?;
        anyhow::bail!("refused {}@{}: {reason}", info.user, info.database);
    }
    let schema = info.database.clone();
    if !is_valid_schema(&schema) {
        anyhow::bail!("rejecting invalid schema name {schema:?}");
    }
    info!("client requested schema {schema}");

    // Hold the guard for the whole connection: it keeps the VM off the idle
    // reaper's radar until the client disconnects.
    let guard = registry.checkout(&schema).await?;
    proxy::splice(client, guard.entry(), &info.raw).await
}

/// Sanity-check `PG_VM_POOL_RUN_DIR` at startup: it must be *heyvmd's* run dir
/// (the one holding `sb-<id>/`), not merely a directory that exists.
///
/// Everything that returns disk — the orphan sweep, the rootfs prune, the
/// reclaim command, compaction, image archiving, pressure eviction — resolves
/// paths under it, and every one of them treats "nothing there" as "nothing to
/// do". So a run dir pointed at the wrong path (the default `~/.heyo/run` on a
/// host whose daemon actually runs out of, say, a mounted array) doesn't fail:
/// it silently turns off disk reclamation while every log line still reads
/// normal. That is worth a loud line at startup.
fn check_run_dir(run_dir: &std::path::Path) {
    let sandboxes = std::fs::read_dir(run_dir).map(|entries| {
        entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("sb-"))
            .count()
    });
    match sandboxes {
        Ok(0) => warn!(
            "PG_VM_POOL_RUN_DIR={} holds no sb-<id>/ directories — if heyvmd's run dir is \
             elsewhere, every disk-reclaiming feature (orphan sweep, rootfs prune, reclaim \
             command, compaction, image archive, pressure eviction) is silently doing nothing. \
             Check the daemon's data dir and PG_VM_POOL_RECLAIM_CMD's argument too",
            run_dir.display()
        ),
        Ok(n) => info!("run dir {} holds {n} VM directories", run_dir.display()),
        Err(e) => warn!(
            "PG_VM_POOL_RUN_DIR={} is not readable ({e}) — disk verification, the orphan \
             sweep and the local offload tiers all need it",
            run_dir.display()
        ),
    }
}

/// Conservative guard on the client-supplied schema name: it becomes both a
/// Postgres database identifier and part of the VM name, so cap length (PG's
/// 63-byte identifier limit) and reject control characters.
pub(crate) fn is_valid_schema(s: &str) -> bool {
    !s.is_empty() && s.len() <= 63 && s.chars().all(|c| !c.is_control())
}
