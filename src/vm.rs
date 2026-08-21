//! Per-schema VM control loop: find-or-create-or-restart the `pg-<schema>`
//! microVM, open a raw-TCP tunnel to its Postgres, and bootstrap the database.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use deadpool_postgres::{Config as PgConfig, Pool, Runtime};
use heyo_sdk::{
    CommandResult, CommandRunOptions, DEFAULT_LOCAL_BASE_URL, HeyoClient, HeyoClientOptions,
    HeyoError, P2pTunnel, RequestOptions, Sandbox, SandboxCreateOptions, SandboxDriver,
    SandboxInfo,
};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::Config;
use crate::registry::SchemaEntry;
use crate::s3::S3Config;

const VM_PG_PORT: u16 = 5432;

/// How long Postgres gets to answer (or at least speak) before we conclude the
/// server process is dead inside a live VM. Generous on purpose: a healthy VM
/// answers in milliseconds, and a freshly booted one binds its port within a
/// couple of seconds of HEYVM_READY — only a crashed/absent postmaster stays
/// silent this long.
const PG_PROBE_WINDOW: Duration = Duration::from_secs(15);

/// Poll interval while waiting for Postgres to come up, two-speed. init.sh
/// signals HEYVM_READY ~1s *before* the postmaster binds 5432, so nearly
/// every bring-up spends its first probes hitting a not-yet-open port — at
/// the old flat 500ms quantum that rounded up to half a second of pure sleep
/// on EVERY cold start. Probe fast (a refused connect on the local bridge is
/// ~free) while the wait still looks like a normal boot, then back off to the
/// old cadence for the pathological waits `wait_pg_ready`'s 300s timeout
/// exists for.
const PG_POLL_FAST: Duration = Duration::from_millis(100);
const PG_POLL_SLOW: Duration = Duration::from_millis(500);
const PG_POLL_FAST_WINDOW: Duration = Duration::from_secs(5);

/// The poll interval for an in-flight Postgres wait that started `elapsed`
/// ago (see [`PG_POLL_FAST`]).
fn pg_poll_interval(elapsed: Duration) -> Duration {
    if elapsed < PG_POLL_FAST_WINDOW { PG_POLL_FAST } else { PG_POLL_SLOW }
}

/// Per-attempt bound inside that window. Only guards against a connect that
/// hangs forever (the pool has no create timeout); it is not a health
/// threshold — exceeding it yields `PgProbe::Stalled`, never `Unreachable`.
const PG_PROBE_ATTEMPT: Duration = Duration::from_secs(3);

/// The heyvmd daemon every call in this crate addresses. Defaults to the SDK's
/// local daemon (`http://127.0.0.1:34099`); `PG_VM_POOL_DAEMON_URL` overrides
/// it.
///
/// Read once and cached, because it is on the bring-up hot path and because a
/// per-call `env::var` takes a process-wide lock — which would show up as
/// contention in exactly the concurrent bring-up measurements this indirection
/// exists to make possible. The override has two uses: pointing the pooler at
/// a daemon on another port, and letting `crate::loadtest` aim the *whole*
/// pooler at an in-process daemon stub — the only way to put a
/// thousand-sandbox fleet under it without a thousand VMs.
pub(crate) fn daemon_base_url() -> &'static str {
    static URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URL.get_or_init(|| {
        std::env::var("PG_VM_POOL_DAEMON_URL")
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_LOCAL_BASE_URL.to_string())
    })
}

/// Fresh options targeting the local heyvmd daemon. Built per call so we don't
/// rely on `HeyoClientOptions: Clone`. Shared with the dashboard so its control
/// actions hit the same daemon.
pub(crate) fn local_opts() -> HeyoClientOptions {
    HeyoClientOptions {
        base_url: Some(daemon_base_url().to_string()),
        ..Default::default()
    }
}

/// HTTP timeout for the deploy POST specifically. Current heyvmd 202-accepts
/// the deploy and builds in the background, so this normally answers in
/// milliseconds — but an older daemon blocks the POST until the VM has fully
/// booted, and either way a client-side timeout doesn't cancel the server-side
/// build: the "failed" deploy still finishes and leaves an orphan VM behind.
/// Generous is the safe direction.
const DEPLOY_HTTP_TIMEOUT: Duration = Duration::from_secs(180);

/// How many VM bring-ups (deploys of new VMs, boots of stopped ones) may hit
/// the daemon at once. Overridden by `PG_VM_POOL_MAX_CONCURRENT_BRINGUPS`;
/// `0` disables the gate entirely.
///
/// Why a gate: heyvmd runs blocking work on its async worker threads during
/// every VM start (`debugfs` key injection, `mke2fs` for mounts, full rootfs
/// copies on non-CoW filesystems). Enough simultaneous bring-ups park every
/// worker, at which point the daemon stops answering *anything* — including
/// its own health endpoint, whose watchdog then restarts it. Because the
/// Firecracker VMM processes are the daemon's children (their stdio is the
/// guest serial console), that restart kills every running VM, so every
/// active schema cold-starts at once against the fresh daemon and wedges it
/// again. Queueing the excess here (FIFO, in the pooler) breaks that cycle:
/// the daemon only ever sees a herd it can survive.
const DEFAULT_BRINGUP_SLOTS: usize = 3;

/// The gate itself. `None` inside means the operator disabled it (`0`).
static BRINGUP_GATE: OnceLock<Option<Semaphore>> = OnceLock::new();

fn bringup_gate() -> Option<&'static Semaphore> {
    BRINGUP_GATE
        .get_or_init(|| {
            let slots = std::env::var("PG_VM_POOL_MAX_CONCURRENT_BRINGUPS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(DEFAULT_BRINGUP_SLOTS);
            if slots == 0 {
                warn!("PG_VM_POOL_MAX_CONCURRENT_BRINGUPS=0: bring-up gate disabled");
            }
            (slots > 0).then(|| Semaphore::new(slots))
        })
        .as_ref()
}

/// Take a bring-up slot before any daemon call that builds or boots a VM.
/// Hold the returned permit only across the daemon-heavy call itself (deploy
/// POST, `start()`) — readiness polling is cheap for the daemon and must not
/// pin a slot. Waiting is logged so a queued bring-up is visible in the log
/// instead of reading as a hang.
pub(crate) async fn bringup_slot(what: &str) -> Option<SemaphorePermit<'static>> {
    let gate = bringup_gate()?;
    if let Ok(permit) = gate.try_acquire() {
        return Some(permit);
    }
    let queued = Instant::now();
    let _waiting = Waiting::new();
    info!("{what}: all VM bring-up slots busy; queueing");
    // The gate is never closed, so acquire() cannot fail.
    let permit = gate.acquire().await.expect("bring-up gate is never closed");
    info!("{what}: bring-up slot acquired after {:?} queued", queued.elapsed());
    Some(permit)
}

/// How many bring-ups are queued right now — waiting for an admission slot or
/// for a bring-up slot, i.e. work that has a client parked behind it.
///
/// Read by background work (the offload pacer) as backpressure: moving a cold
/// schema to S3 is never worth making a client wait longer, so that work only
/// runs while this is zero. Counted rather than derived from
/// `Semaphore::available_permits`, which says how many slots are taken but not
/// whether anyone is queued behind them.
static BRINGUPS_WAITING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) fn bringups_waiting() -> usize {
    BRINGUPS_WAITING.load(std::sync::atomic::Ordering::Relaxed)
}

/// RAII counter for [`BRINGUPS_WAITING`]. A guard rather than a pair of
/// fetch_add/fetch_sub calls because these waits sit inside futures a
/// disconnecting client can cancel at any await point — a leaked increment
/// would wedge background offloading permanently.
struct Waiting;

impl Waiting {
    fn new() -> Self {
        BRINGUPS_WAITING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        BRINGUPS_WAITING.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How many whole bring-ups (deploy through ready/restore) may be in flight at
/// once. Overridden by `PG_VM_POOL_MAX_PENDING_BRINGUPS`; `0` disables.
///
/// Why a second gate when [`BRINGUP_GATE`] exists: that one meters the daemon
/// calls that build or boot a VM — but heyvmd 202-accepts deploys and builds
/// in the background, so the deploy POST releases its slot in milliseconds and
/// a burst of N schemas still lands N concurrent builds (plus N ready-poll
/// loops) on the daemon. This gate bounds the *pending population*: request
/// N+1 queues here, at the pooler's front door, where waiting is free — the
/// client just sees a slower connect — instead of inside a daemon that wedges
/// under the herd. Sized well above the bring-up slots so the queue depth, not
/// the daemon-call rate, is what it controls.
const DEFAULT_PENDING_BRINGUPS: usize = 16;

/// The admission gate itself. `None` inside means the operator disabled it.
static ADMISSION_GATE: OnceLock<Option<Semaphore>> = OnceLock::new();

fn admission_gate() -> Option<&'static Semaphore> {
    ADMISSION_GATE
        .get_or_init(|| {
            let slots = std::env::var("PG_VM_POOL_MAX_PENDING_BRINGUPS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(DEFAULT_PENDING_BRINGUPS);
            if slots == 0 {
                warn!("PG_VM_POOL_MAX_PENDING_BRINGUPS=0: bring-up admission gate disabled");
            }
            (slots > 0).then(|| Semaphore::new(slots))
        })
        .as_ref()
}

/// Take an admission slot for one whole bring-up. Held across all of
/// `ensure_vm` — deploy, ready wait, Postgres bootstrap, restore — so it
/// bounds how many bring-ups exist at once, not how fast they start.
async fn admission_slot(schema: &str) -> Option<SemaphorePermit<'static>> {
    let gate = admission_gate()?;
    if let Ok(permit) = gate.try_acquire() {
        return Some(permit);
    }
    let queued = Instant::now();
    let _waiting = Waiting::new();
    info!("schema {schema}: all bring-up admission slots busy; queueing at the pooler");
    let permit = gate.acquire().await.expect("admission gate is never closed");
    info!(
        "schema {schema}: bring-up admitted after {:?} queued",
        queued.elapsed()
    );
    Some(permit)
}

/// How often [`wait_ready`] re-asks the daemon for a pending VM's status.
/// Fast while the VM is young: the per-id endpoint is a cheap lookup, and a
/// Firecracker boot can be ready in low single-digit seconds, so coarse
/// polling here is pure added latency against the upstream caller's deadline.
/// After [`READY_POLL_FAST_WINDOW`] the build is slow anyway — back off so a
/// long provision doesn't hammer the daemon for minutes.
const READY_POLL_FAST_INTERVAL: Duration = Duration::from_millis(250);
const READY_POLL_SLOW_INTERVAL: Duration = Duration::from_secs(2);
const READY_POLL_FAST_WINDOW: Duration = Duration::from_secs(15);

/// How long a *continuous* run of 404s on the per-id endpoint is tolerated
/// before the wait gives up on that sandbox id.
///
/// A 404 right after the deploy's 202 is normal for a moment (the daemon hands
/// out the id, then its provision tracker starts answering for it). A 404 that
/// persists is not a slow build: it means the record backing this id is gone —
/// overwhelmingly because heyvmd restarted (its watchdog does exactly that, and
/// the tracker is in-memory), taking the half-built VM with it. Waiting out the
/// full ready timeout there burns minutes of the caller's deadline plus an
/// admission slot on an id that can never resolve, and — since heyvmd restarts
/// kill every running VM — it happens to every in-flight bring-up at once. Fail
/// fast instead: the caller retries with a fresh create against the fresh
/// daemon, which is the only thing that can work.
const READY_NOTFOUND_GRACE: Duration = Duration::from_secs(45);

/// Ready budget for a warm spare specifically, floored against the (much
/// longer) client-facing `ready_timeout`. A spare has no client waiting on it
/// and no data to lose: one that hasn't booted in this long is a sick build to
/// throw away and retry, not something to keep a replenish pass parked on.
pub(crate) const SPARE_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Exclusive use of the process-wide VM state for the caller's whole test.
///
/// The daemon stub's fleet and counters, the environment a `Config` reads, the
/// bring-up gate, and the reclaim boot gate and its waiting-boot count are all
/// process-wide. `cargo test` runs test functions in parallel, so two tests
/// touching any of them measure each other — which is not a flaky result, it is
/// a wrong one. It cannot be delegated to `--test-threads=1` either: that flag
/// is for readable output, this is for correctness.
///
/// One lock rather than one per module, because the state is shared across
/// them: a `loadtest` bring-up takes a reclaim boot permit, and a `reclaim`
/// test asserting on the waiting-boot count sees it.
#[cfg(test)]
pub(crate) async fn test_exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// Pooler-side readiness wait: poll the *per-id* `GET /deployed-sandboxes/:id`
/// until the sandbox leaves `provisioning`, tolerating transient daemon
/// errors until the deadline.
///
/// Replaces the SDK's `Sandbox::wait_for_ready`, which (a) fetches the FULL
/// `/deployed-sandboxes` listing every poll and filters client-side — during a
/// deploy burst that's O(inventory × in-flight) serialization pressure on
/// exactly the endpoint that goes lock-contended first — and (b) aborts the
/// whole bring-up on the first transient transport error, which under load
/// converts one daemon hiccup into a synchronized batch of failed bring-ups,
/// each leaving an orphan VM. Here a poll error is just a poll that taught us
/// nothing: keep polling until the deadline, and report the last error if the
/// deadline passes.
pub(crate) async fn wait_ready(sandbox: &Sandbox, timeout: Duration, name: &str) -> Result<()> {
    wait_ready_within(sandbox, timeout, name, READY_NOTFOUND_GRACE).await
}

/// [`wait_ready`] with the 404 grace spelled out, so tests can exercise both
/// sides of it without waiting real minutes.
async fn wait_ready_within(
    sandbox: &Sandbox,
    timeout: Duration,
    name: &str,
    notfound_grace: Duration,
) -> Result<()> {
    use heyo_sdk::SandboxStatus;
    let started = Instant::now();
    let deadline = started + timeout;
    let mut last_err: Option<HeyoError> = None;
    let mut error_streak = 0u32;
    // Start of the current uninterrupted run of 404s, if any (see
    // READY_NOTFOUND_GRACE). Cleared by any answer that isn't a 404.
    let mut missing_since: Option<Instant> = None;
    loop {
        match sandbox.get().await {
            Ok(info) => {
                error_streak = 0;
                missing_since = None;
                match info.status {
                    SandboxStatus::Running => return Ok(()),
                    // Terminal: the daemon gave up on the build; waiting longer
                    // can't change the answer.
                    SandboxStatus::Failed => bail!(
                        "VM {name} ({}) failed to provision: {}",
                        sandbox.sandbox_id(),
                        info.error_message
                            .as_deref()
                            .unwrap_or("no reason reported")
                    ),
                    SandboxStatus::Provisioning | SandboxStatus::Unknown => {}
                    // Stopped/Paused/ColdStored: settled but not running. Same
                    // contract as the SDK's wait_for_ready — "no longer
                    // provisioning" ends the wait, and the Postgres probe that
                    // follows every bring-up is the real readiness authority.
                    _ => return Ok(()),
                }
            }
            // NotFound included: right after a 202 the record can lag the id,
            // and during an incident the daemon may briefly answer nonsense.
            // Only the deadline decides.
            Err(e) => {
                error_streak += 1;
                if error_streak == 1 {
                    warn!("{name}: readiness poll failed (retrying until deadline): {e:#}");
                }
                if matches!(e, HeyoError::NotFound(_)) {
                    let since = *missing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= notfound_grace {
                        bail!(
                            "VM {name} ({}) has been unknown to heyvmd for {:?} — the record \
                             behind this id is gone (heyvmd restarts drop in-flight creates, \
                             and take every running VM with them). Not waiting out the ready \
                             timeout; retry with a fresh create. Last daemon error: {e:#}",
                            sandbox.sandbox_id(),
                            since.elapsed(),
                        );
                    }
                } else {
                    missing_since = None;
                }
                last_err = Some(e);
            }
        }
        if Instant::now() >= deadline {
            match last_err {
                Some(e) => bail!(
                    "VM {name} ({}) not ready within {timeout:?}; last daemon error: {e:#}",
                    sandbox.sandbox_id()
                ),
                None => bail!(
                    "VM {name} ({}) not ready within {timeout:?}: status never left provisioning",
                    sandbox.sandbox_id()
                ),
            }
        }
        let interval = if started.elapsed() < READY_POLL_FAST_WINDOW {
            READY_POLL_FAST_INTERVAL
        } else {
            READY_POLL_SLOW_INTERVAL
        };
        sleep(interval).await;
    }
}

/// Run a daemon call with bounded retries on transient failures. A momentarily
/// unreachable or 5xx-ing daemon (deploy bursts make this the norm, not the
/// exception) shouldn't instantly fail a bring-up or a cleanup that merely
/// needed an answer.
async fn with_daemon_retry<T, F, Fut>(what: &str, mut call: F) -> Result<T, HeyoError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, HeyoError>>,
{
    const ATTEMPTS: u32 = 4;
    let mut delay = Duration::from_millis(500);
    for attempt in 1..=ATTEMPTS {
        match call().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < ATTEMPTS && transient(&e) => {
                warn!(
                    "{what} failed (attempt {attempt}/{ATTEMPTS}, \
                     retrying in {delay:?}): {e:#}"
                );
                sleep(delay).await;
                delay *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop returns on the last attempt")
}

/// `Sandbox::list` against the local daemon with bounded retries on transient
/// failures. Every successful listing warms the name→id cache for free.
pub(crate) async fn list_with_retry() -> Result<Vec<SandboxInfo>, HeyoError> {
    let list = with_daemon_retry("listing sandboxes", || Sandbox::list(local_opts())).await?;
    crate::inventory::absorb(&list);
    Ok(list)
}

/// Resolve one sandbox by exact name via `GET /deployed-sandboxes?name=`,
/// with the same transient-retry policy as [`list_with_retry`].
///
/// A daemon that understands the filter (heyvmd ≥0.44) answers with at most
/// one entry — O(1) instead of the O(fleet) inventory pull this call replaces.
/// An *old* daemon ignores the query and returns the full list; the exact
/// match is selected client-side either way (correct against both), and
/// whatever came back is absorbed into the cache — an old daemon's full list
/// warms it for free. Raw `HeyoClient::request` rather than an SDK method
/// because the pinned heyo-sdk 0.1.5 predates `Sandbox::find_by_name`
/// (precedent: the dashboard's inactive-page listing).
pub(crate) async fn find_by_name_with_retry(name: &str) -> Result<Option<SandboxInfo>, HeyoError> {
    let client = HeyoClient::new(local_opts())?;
    let response = with_daemon_retry("by-name sandbox lookup", || {
        let mut opts = RequestOptions::default();
        opts.query.push(("name".to_string(), name.to_string()));
        client.request::<Vec<SandboxInfo>>(
            reqwest::Method::GET,
            "/deployed-sandboxes",
            None::<&()>,
            opts,
        )
    })
    .await?;
    crate::inventory::absorb(&response);
    Ok(response.into_iter().find(|s| s.name == name))
}

/// Errors worth retrying: transport failures (the SDK mints status 0 for those)
/// and 5xx. A 4xx is the daemon answering coherently — retrying can't help.
fn transient(e: &HeyoError) -> bool {
    matches!(e, HeyoError::Api { status, .. } if *status == 0 || *status >= 500)
}

/// Bring up (or reattach to) the VM for `schema` and return a ready entry.
/// `known_id` is the sandbox id from a prior bring-up of this schema (if any);
/// reattaching by id avoids a data-loss race where a just-stopped VM is briefly
/// absent from list-by-name and we'd otherwise create a duplicate with a fresh
/// (empty) data disk.
/// Where a restore's dump bytes come from: the S3 archive tier or the local
/// frozen tier's dump server. Owned so the registry can hand it into the
/// bring-up closure without lifetime gymnastics.
pub enum RestoreSource {
    S3(S3Config),
    /// Raw-disk image archive (`{schema}.img.zst`): the VM is materialized by
    /// downloading the image and booting on it directly — no `pg_restore`, no
    /// spare claim, no guest job. Chosen when the schema's S3 archive is an
    /// image rather than a dump (see `imgarchive::pick_restore`).
    S3Image(S3Config),
    /// Locally compacted image (the `Compacted` tier): same materialize
    /// maneuver as `S3Image` minus the download — the compressed image is
    /// already on this host at the given path.
    LocalImage(std::path::PathBuf),
    Local {
        srv: std::sync::Arc<crate::dumpsrv::DumpServer>,
        port: u16,
    },
}

pub async fn ensure_vm(
    cfg: &Config,
    schema: &str,
    known_id: Option<&str>,
    restore: Option<&RestoreSource>,
    spares: Option<(&crate::spares::SparePool, &std::collections::HashSet<String>)>,
    owner: Option<&crate::dedicated::Credential>,
) -> Result<Arc<SchemaEntry>> {
    // Bound the number of concurrent bring-ups before any daemon traffic. A
    // burst beyond the cap queues here (each waiter is one parked client
    // connection) instead of becoming daemon load. Held to the end of the
    // function: the pending *population* is what the daemon can't survive.
    let mut phase = Instant::now();
    let _admission = admission_slot(schema).await;
    let admission_took = std::mem::replace(&mut phase, Instant::now()).elapsed();
    let name = format!("pg-{schema}");
    let keepalive = cfg.is_keepalive(schema);

    // An archived schema's VM was killed, so its stored id is dead — never try
    // to reattach by id. Note this does NOT guarantee a clean disk: the
    // find-by-name fallback reuses a VM left behind by a previously *failed*
    // restore, whose database is partially loaded — which is why the restore
    // job runs `pg_restore --clean --if-exists` (idempotent over that débris).
    let known_id = if restore.is_some() { None } else { known_id };
    let (sandbox, claimed_spare) = match restore {
        // An image restore builds its own VM (download, disk swap, boot on the
        // real data) — the database is complete before Postgres first starts,
        // so there is nothing to restore into and no spare worth claiming.
        Some(RestoreSource::S3Image(s3)) => (
            crate::imgarchive::materialize_from_image(cfg, schema, s3)
                .await
                .with_context(|| format!("restoring schema {schema} from its S3 disk image"))?,
            false,
        ),
        Some(RestoreSource::LocalImage(path)) => (
            crate::imgarchive::materialize_from_local_image(cfg, schema, path)
                .await
                .with_context(|| format!("thawing schema {schema} from its compacted image"))?,
            false,
        ),
        _ => resolve_sandbox(cfg, &name, keepalive, known_id, spares).await?,
    };
    let resolve_took = std::mem::replace(&mut phase, Instant::now()).elapsed();
    let sandbox_id = sandbox.sandbox_id().to_string();

    // The rest of the bring-up can still fail (ready-timeout, restore error).
    // When the sandbox is a freshly claimed spare, that failure must release
    // the claim — otherwise the id stays in the pool's `claimed` set for the
    // life of the process: a running VM + disk nothing can reclaim, invisible
    // to replenish inventory (which then builds a replacement on top).
    let result = async {
        // Pin keep-alive schemas idempotently: TTL 0 = never auto-stopped. This
        // covers a VM created before its schema was pinned (or created with a
        // non-zero TTL) — a freshly-created keep-alive VM is already TTL 0, so this
        // is a harmless no-op there. Best-effort: a failure here shouldn't block
        // serving the connection, so we warn rather than bail.
        if keepalive {
            if let Err(e) = sandbox.set_ttl(0).await {
                warn!("failed to pin keep-alive VM {name} (set_ttl 0): {e:#}");
            }
        }

        let (target, tunnel, pool) = ready_pg(cfg, &sandbox, &name).await?;
        let pg_ready_took = std::mem::replace(&mut phase, Instant::now()).elapsed();
        ensure_database(&pool, schema, owner).await?;
        let create_db_took = std::mem::replace(&mut phase, Instant::now()).elapsed();

        // Restore into the freshly-created, empty database before the entry is
        // handed to any client. A failure here must abort the bring-up: serving
        // an empty DB in place of a restored one would look like silent data loss.
        match restore {
            Some(RestoreSource::S3(s3)) => restore_from_s3(cfg, &sandbox, schema, s3, owner)
                .await
                .with_context(|| format!("restoring schema {schema} from S3"))?,
            Some(RestoreSource::Local { srv, port }) => {
                restore_from_local(cfg, &sandbox, schema, srv, *port, owner)
                    .await
                    .with_context(|| format!("restoring schema {schema} from the local dump"))?
            }
            // Already restored above — the VM booted on the adopted disk. Counted
            // with the S3 restores on the monitoring charts: same tier, same cost
            // profile, and a fourth chart wouldn't earn its space.
            Some(RestoreSource::S3Image(_)) => crate::events::record(crate::events::Event::RestoreS3),
            // Ditto, but local (a compacted-image thaw): counted with the
            // local-dump restores.
            Some(RestoreSource::LocalImage(_)) => {
                crate::events::record(crate::events::Event::RestoreLocal)
            }
            None => {}
        }
        let restore_took = std::mem::replace(&mut phase, Instant::now()).elapsed();

        let slots = client_slot_budget(&pool, &name).await;
        let bootstrap_took = phase.elapsed();

        // One line per bring-up, next to the registry's total "VM ready in":
        // which phase ate the time is the whole diagnosis when cold starts
        // regress. `resolve+boot` spans create/reattach/spare-claim through
        // the guest boot; `pg-ready` is the wait for the postmaster. The gate
        // waits (admission, bring-up slot, boot gate) also log themselves
        // when they actually queued.
        info!(
            "schema {schema}: bring-up phases — admission {admission_took:?}, resolve+boot \
             {resolve_took:?}, pg-ready {pg_ready_took:?}, create-db {create_db_took:?}, \
             restore {restore_took:?}, slots {bootstrap_took:?}",
        );

        Ok(Arc::new(SchemaEntry::new(
            sandbox, target, tunnel, pool, keepalive, slots,
        )))
    }
    .await;

    if result.is_err()
        && claimed_spare
        && let Some((pool, _)) = spares
    {
        pool.release_failed(&sandbox_id).await;
    }
    result
}

/// Validity window for a presigned S3 URL handed to the guest. Generous enough
/// to cover a slow upload/download of a large dump, short enough that a URL that
/// leaks (e.g. into a guest shell-history) expires quickly.
const PRESIGN_TTL: Duration = Duration::from_secs(3600);

/// Client-side HTTP timeout for a single guest exec round-trip. The guest exec
/// API itself hard-caps any one foreground command at ~30s server-side (and the
/// SDK has no way to raise that — the exec body carries no timeout field), so
/// this only has to comfortably outlast that cap to receive the response,
/// including the 500 the server returns when it kills a command at 30s.
const GUEST_EXEC_HTTP_TIMEOUT: Duration = Duration::from_secs(45);

/// Total wall-clock the pooler will wait for a *detached* dump+upload to finish
/// before giving up. Because the transfer runs detached in the guest (§
/// [`dump_to_s3`]) rather than as one foreground exec, this bounds only the
/// pooler's patience — the guest job is never itself subject to the ~30s exec
/// cap. The sweep retries a timed-out schema next pass.
const ARCHIVE_DEADLINE: Duration = Duration::from_secs(1800);

/// How often we check on a running dump job. Each pass is one S3 HEAD from the
/// pooler plus (at most) one trivial guest exec. Deliberately not tighter: every
/// guest exec contends for the VM's single serial console against a `pg_dump`
/// that is saturating the same one-vCPU guest, and polling harder makes the
/// starvation it is trying to observe worse.
const ARCHIVE_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Per-request timeout for the pooler's own S3 HEAD.
const ARCHIVE_HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Smallest object accepted as a real archive. A `pg_dump -Fc` of even an
/// empty database is ~1.5KB — nothing legitimate is under 512 bytes. Small
/// objects happen for real: with the host disk full, the guest's dump file is
/// torn to zero length by failed writeback while `pg_dump` and `curl` both
/// exit 0, and production accepted such a 0-byte "archive" and then killed the
/// VM — destroying the only copy of the data. Size is checked on the dump side
/// (never report a tiny upload durable) *and* the restore side (fail with the
/// truth instead of feeding `pg_restore` an empty file).
const MIN_ARCHIVE_BYTES: u64 = 512;

/// How many *consecutive* failed guest probes we tolerate before we stop asking
/// the guest and let S3 alone decide. The guest exec channel failing says
/// nothing about the detached job (which does not use that channel), so giving
/// up on it must not give up on the archive — it only costs us the fast,
/// detailed error path.
const ARCHIVE_MAX_PROBE_FAILURES: u32 = 3;

/// The same tolerance for a job with *no* out-of-band signal (the restore),
/// where losing the guest means losing the only way to ever confirm it. Much
/// more generous, because here running out means failing the operation: probes
/// fail exactly when the guest is busiest, and each failure already costs up to
/// a full [`GUEST_EXEC_HTTP_TIMEOUT`], so this is many minutes of silence — not
/// a brief stall — before we conclude the guest is gone.
const UNWITNESSED_MAX_PROBE_FAILURES: u32 = 10;

/// How long the pooler keeps trying to see the object after the guest says it
/// finished uploading. A completed PUT is visible immediately (S3 is
/// read-after-write consistent for new objects), so this only covers a HEAD or
/// two of transient trouble; past it, the disagreement is structural and waiting
/// out [`ARCHIVE_DEADLINE`] would only delay the same error.
const ARCHIVE_CONFIRM_GRACE: Duration = Duration::from_secs(60);

/// Attempts at the pre-dump HEAD. It has to succeed for the dump to be
/// verifiable at all, so a transient blip shouldn't cost a reap — but a
/// persistent failure must, and loudly.
const BASELINE_HEAD_ATTEMPTS: u32 = 3;

/// Fixed in-guest scratch paths for the dump. One VM backs exactly one schema,
/// so a constant name is unambiguous — and unlike a schema-derived name it can't
/// be broken by a schema containing `/` or a quote.
const DUMP_PATH: &str = "/workspace/_archive.dump";
const RESTORE_PATH: &str = "/workspace/_restore.dump";

/// Heredoc delimiter used to plant a job script from its launch exec. Quoted at
/// the use site (`<<'…'`) so the body — which contains `$ec`, `$?` and a
/// presigned URL full of `&`/`=`/`%` — is written through verbatim, with no
/// expansion and no shell-quoting layer to survive.
const JOB_HEREDOC_EOF: &str = "HEYO_JOB_EOF";

/// Framing token for a probe reply. Distinctive enough that no kernel log line
/// or shell echo can be mistaken for one.
const PROBE_TAG: &str = "HEYOJOB:";

/// A long-running guest job that must outlive the exec that starts it, plus the
/// in-guest scratch paths it is watched through: the shell script we plant, the
/// sentinel holding its exit code once done, and its combined log (read back
/// only to surface an error).
///
/// Both transfers need this shape for the same reason — see [`dump_to_s3`] for
/// why a foreground exec cannot carry either one.
#[derive(Clone, Copy)]
struct DetachedJob {
    /// Human name for logs and error messages ("dump", "restore").
    what: &'static str,
    script: &'static str,
    done: &'static str,
    log: &'static str,
}

const ARCHIVE_JOB: DetachedJob = DetachedJob {
    what: "dump",
    script: "/workspace/_archive.job.sh",
    done: "/workspace/_archive.done",
    log: "/workspace/_archive.log",
};

const RESTORE_JOB: DetachedJob = DetachedJob {
    what: "restore",
    script: "/workspace/_restore.job.sh",
    done: "/workspace/_restore.done",
    log: "/workspace/_restore.log",
};

/// Dump `schema`'s database to S3 using the guest's own `pg_dump` + `curl`
/// against a pooler-presigned PUT URL. The dump bytes stream straight from the
/// guest to S3 and never transit the pooler. Dumps to a file first (not a pipe)
/// so `curl -T` sends a `Content-Length` — S3 rejects a chunked PUT.
///
/// The transfer runs **detached** in the guest, not as one foreground exec: the
/// guest exec API hard-caps a single command at 30s server-side, far too short
/// for a multi-workbook dump+upload, and the SDK can't raise it. So we launch
/// the dump under `setsid` (a new session with stdio fully redirected, so it
/// outlives the launch exec) and watch for its completion out-of-band. Each exec
/// we issue — the launch and every probe — is trivially short; only the
/// pooler-side [`ARCHIVE_DEADLINE`] bounds the wait.
///
/// Everything guest-side goes through `exec` rather than the SDK's `files()`
/// API, which is **not usable on these VMs**: `write-file`/`read-file` resolve a
/// path against a *host-side bind mount* declared at create time, so on a
/// Firecracker sandbox whose `/workspace` is a guest block device (`/dev/vdb`)
/// there is no matching mount and the daemon answers `Mount not found:
/// /workspace (available mounts: [])`.
///
/// # Why success is decided in S3, not in the guest
///
/// On these VMs `exec` is not a reliable channel. The daemon drives a **single
/// shared shell on the guest's serial console**, writing a marker-delimited
/// command and reading until the end marker, under a fixed 30s timeout
/// (`execute_via_serial` in mvm-ctrl's Firecracker driver). While `pg_dump` and
/// `curl` saturate the one-vCPU guest and its single virtio disk, even `[ -f x ]`
/// can miss that window — so a probe times out and the daemon answers `500
/// Command timed out after 30 seconds`.
///
/// That failure is about the *channel*, not the job: the detached dump neither
/// uses nor cares about the serial console. Treating it as a dump failure — as
/// this did — aborted archives that were running fine, precisely when the VM was
/// busiest, i.e. for the largest workbooks.
///
/// So the authoritative completion signal is an S3 `HEAD` issued **by the
/// pooler**: it answers over the pooler's own network, needs nothing from the
/// guest, and checks the thing actually at stake — that the object exists —
/// rather than a guest's claim about it. That matters because the caller kills
/// the VM and reclaims its disk on our `Ok`, destroying the only other copy.
/// The guest sentinel is kept as a best-effort fast path: it turns a failed dump
/// into an immediate, explained error instead of a 30-minute timeout.
/// How long to keep looking for a killed VM's `sb-<id>/` directory to vanish
/// before declaring the disk leaked. heyvmd answers the DELETE and *then*
/// tears the sandbox down, so the directory can linger a moment after `kill()`
/// returns; poll briefly and exit the instant it's gone rather than crying leak
/// on the first look. A genuinely stranded disk simply never disappears and we
/// warn after the cap — cheap, since the check is a single `stat`.
const KILL_VERIFY_ATTEMPTS: u32 = 8;
const KILL_VERIFY_INTERVAL: Duration = Duration::from_millis(500);

/// Kill `sandbox` and confirm its data-disk directory is actually gone.
///
/// The kill is `DELETE /deployed-sandboxes/:id`, and the SDK treats a 404 as
/// success — so `Ok` proves only that the daemon no longer holds the record,
/// **not** that the VM's `sb-<id>/` directory (its `data.ext4`, rootfs copy,
/// logs) left the host. We have seen the daemon drop the record while leaving
/// the directory behind: the VM count falls but the bytes don't come back, and
/// nothing else reclaims them (`reclaim-disks.sh` only trims *live* schemas'
/// disks, it never deletes dead sandboxes). Left unchecked this silently
/// strands hundreds of GB. So after a successful kill we poll for the directory
/// and, if it survives, log it loudly with the path and on-disk size instead of
/// the usual "disk reclaimed".
///
/// `accomplished` is the tier-specific thing the caller just did (e.g. "dumped
/// to s3://…" / "frozen to a 12345-byte local dump"), woven into the one log
/// line so it reads naturally. Best-effort and non-fatal: the data is already
/// durable in its new tier before we get here, so a surviving disk is a reclaim
/// task, never a reason to fail the operation. Returns the kill call's own
/// error so the caller can report an outright kill *failure* — the worse case,
/// since the VM may still be running.
pub async fn kill_and_reclaim(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    accomplished: &str,
) -> Result<()> {
    let id = sandbox.sandbox_id().to_string();
    sandbox.kill().await.context("killing the VM")?;

    let Some(dir) = cfg.run_dir.as_ref().map(|d| d.join(&id)) else {
        info!(
            "schema {schema}: {accomplished}; VM {id} killed \
             (disk removal unverified — set PG_VM_POOL_RUN_DIR to confirm)"
        );
        return Ok(());
    };

    for attempt in 0..KILL_VERIFY_ATTEMPTS {
        if !dir.exists() {
            info!("schema {schema}: {accomplished}; VM {id} killed, data disk reclaimed");
            return Ok(());
        }
        if attempt + 1 < KILL_VERIFY_ATTEMPTS {
            sleep(KILL_VERIFY_INTERVAL).await;
        }
    }

    // The daemon acked the kill but the directory survived — its known
    // failure mode (an acked kill that never removes the dir). The data is
    // durably offloaded and the space is needed NOW, not on some later sweep
    // pass — so re-confirm the daemon no longer knows the id, then remove the
    // directory ourselves. The confirm matters: only a daemon-forgotten id is
    // provably nobody's VM (the per-id GET checks the persisted store too).
    // Bounded: this confirm runs against the same per-id endpoint that can
    // wedge on a struggling daemon — an unanswered probe means "not provably
    // gone", which lands in the leave-it-for-the-sweep arm below.
    match tokio::time::timeout(Duration::from_secs(5), sandbox.get()).await {
        Ok(Err(HeyoError::NotFound(_))) => {
            let size = crate::orphans::dir_allocated_bytes(dir.clone()).await;
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => info!(
                    "schema {schema}: {accomplished}; VM {id} killed — the daemon left {} \
                     behind, removed it directly ({} freed)",
                    dir.display(),
                    crate::orphans::human_iec(size),
                ),
                Err(e) => warn!(
                    "schema {schema}: {accomplished}, but removing the surviving disk dir {} \
                     failed ({e}) — {} stranded until the orphan sweep gets it",
                    dir.display(),
                    crate::orphans::human_iec(size),
                ),
            }
        }
        // The daemon still claims the id (or can't answer): removing the dir
        // under it could destroy a VM it still tracks. Leave it to the sweep,
        // which re-checks with the same per-id probe.
        _ => {
            let size = crate::orphans::dir_allocated_bytes(dir.clone()).await;
            warn!(
                "schema {schema}: {accomplished}, but its VM's data disk survived the kill \
                 and the daemon still answers for {id} — {} left for the orphan-disk sweep \
                 ({} stranded)",
                dir.display(),
                crate::orphans::human_iec(size),
            );
        }
    }
    Ok(())
}

/// Archive `schema`'s database to S3. Two routes:
///
/// * **Via the dump server** (default whenever one is running): the guest
///   *streams* `pg_dump` to the host — zero guest-disk writes — and the
///   pooler uploads the landed file to S3 (multipart above 100MB) and deletes
///   it. The file-based route inflated the very disk being archived: the
///   guest's `-f /workspace/_archive.dump` permanently converted sparse holes
///   into allocated host blocks (no discard passthrough), on failures too.
/// * **Legacy, direct from the guest** (`PG_VM_POOL_ARCHIVE_VIA_GUEST=1`, or
///   no dump server running): dump to a guest file, `curl -T` it to a
///   presigned PUT. Kept one release as an escape hatch.
pub async fn dump_to_s3(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    s3: &S3Config,
    dumps: Option<(&crate::dumpsrv::DumpServer, u16)>,
) -> Result<()> {
    let via_guest = std::env::var("PG_VM_POOL_ARCHIVE_VIA_GUEST")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if let Some((srv, port)) = dumps
        && !via_guest
    {
        return dump_to_s3_via_server(cfg, sandbox, schema, s3, srv, port).await;
    }

    let key = s3.object_key(schema);
    let db = shell_squote(schema);
    let user = shell_squote(&cfg.pg_user);

    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client for S3 HEAD")?;

    // What's at this key *before* the dump — the reference point that makes
    // "the object is there" mean something. A schema's archive key is stable, so
    // the previous archive already sits there; presence alone is no evidence,
    // and mistaking it for this run's upload would report success for a dump
    // that never happened and then reclaim the live disk.
    //
    // This runs before anything is presigned, because it is also where a
    // wrong-region bucket gets discovered and corrected — see
    // `S3Config::head_object`. Signing the guest's PUT first would hand it a URL
    // for the wrong host.
    let baseline = baseline_head(&http, s3, &key, schema).await?;

    let url = s3.presign_put(&key, PRESIGN_TTL);
    let resolve = s3_resolve_flag(s3).await;

    ARCHIVE_JOB
        .launch(
            cfg,
            sandbox,
            &archive_job_body(&user, &db, &resolve, &url),
            DUMP_PATH,
        )
        .await?;

    await_archive(cfg, sandbox, schema, s3, &http, &key, baseline.as_ref()).await
}

/// The streaming S3 archive: guest dump → host dump server (committed, so the
/// bytes are a verified-complete archive) → pooler upload to S3 → HEAD-verify
/// against a pre-upload baseline → local file removed. The guest's data disk
/// is never written.
async fn dump_to_s3_via_server(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    s3: &S3Config,
    srv: &crate::dumpsrv::DumpServer,
    port: u16,
) -> Result<()> {
    let db = shell_squote(schema);
    let user = shell_squote(&cfg.pg_user);
    let token = srv.issue(schema, crate::dumpsrv::Mode::Put)?;
    let url = format!("http://$GW:{port}/d/{token}");
    let body = format!(
        "{}{}",
        gw_prelude(ARCHIVE_JOB.done),
        streaming_dump_job_body(&user, &db, &url)
    );
    ARCHIVE_JOB.launch(cfg, sandbox, &body, STREAM_FAIL_MARK).await?;
    let bytes = await_server_upload(cfg, sandbox, schema, srv, &token).await?;

    // The landed file is this tier's scratch: uploaded then removed on every
    // exit path. (On failure it must not linger either — a later *freeze*
    // would trust a file at this exact path as that schema's frozen dump.)
    let path = srv.dump_path(schema);
    let res = upload_dump_to_s3(s3, schema, &path, bytes).await;
    let _ = tokio::fs::remove_file(&path).await;
    res
}

/// Pooler-side upload of a landed dump file to the schema's S3 dump key, with
/// the same baseline-HEAD verification discipline as the guest path: the
/// caller reclaims the source disk on success, so presence-at-key alone is
/// never trusted.
async fn upload_dump_to_s3(s3: &S3Config, schema: &str, path: &std::path::Path, len: u64) -> Result<()> {
    let http = reqwest::Client::builder()
        .build()
        .context("building HTTP client for the S3 upload")?;
    let key = s3.object_key(schema);
    let baseline = baseline_head(&http, s3, &key, schema).await?;
    crate::imgarchive::upload_path(s3, &http, &key, path, len)
        .await
        .with_context(|| format!("uploading {} to s3://{}/{key}", path.display(), s3.bucket))?;
    match s3.head_object(&http, &key, ARCHIVE_HEAD_TIMEOUT).await {
        Ok(Some(id)) if id.content_length == len && Some(&id) != baseline.as_ref() => {
            info!(
                "schema {schema}: archive uploaded to s3://{}/{key} ({len} bytes)",
                s3.bucket
            );
            Ok(())
        }
        Ok(Some(id)) => bail!(
            "uploaded s3://{}/{key} but the HEAD reports {} bytes against a {len}-byte \
             dump (baseline unchanged: {}) — refusing to trust it",
            s3.bucket,
            id.content_length,
            Some(&id) == baseline.as_ref(),
        ),
        Ok(None) => bail!("uploaded s3://{}/{key} but a HEAD finds nothing", s3.bucket),
        Err(e) => Err(e.context("verifying the uploaded archive")),
    }
}

/// Read what is at the archive key before dumping, retrying a few times.
///
/// Failing here aborts the dump before any work is done, which is the point: the
/// caller reclaims the source disk when we report success, and success is only
/// meaningful against this baseline. A pooler that cannot read the bucket cannot
/// establish that any dump landed, so it must not archive into it — the VM
/// simply stays up and the next reap tries again.
async fn baseline_head(
    http: &reqwest::Client,
    s3: &S3Config,
    key: &str,
    schema: &str,
) -> Result<Option<crate::s3::ObjectId>> {
    let mut last = None;
    for attempt in 1..=BASELINE_HEAD_ATTEMPTS {
        match s3.head_object(http, key, ARCHIVE_HEAD_TIMEOUT).await {
            Ok(id) => return Ok(id),
            Err(e) => {
                warn!(
                    "schema {schema}: pre-dump HEAD of s3://{}/{key} failed \
                     (attempt {attempt}/{BASELINE_HEAD_ATTEMPTS}): {e:#}",
                    s3.bucket
                );
                last = Some(e);
            }
        }
        if attempt < BASELINE_HEAD_ATTEMPTS {
            sleep(ARCHIVE_POLL_INTERVAL).await;
        }
    }
    Err(last.expect("at least one attempt ran")).with_context(|| {
        format!(
            "schema {schema}: cannot read s3://{}/{key}, so no dump into it could be \
             verified; refusing to archive (the source VM is reclaimed on success, \
             so an unverifiable archive is a lost workbook)",
            s3.bucket
        )
    })
}

/// tmpfs marker recording that `pg_dump` itself failed inside the streaming
/// pipeline (POSIX sh has no pipefail). `/tmp` is RAM in the guest image, so
/// touching it never allocates data-disk blocks.
const STREAM_FAIL_MARK: &str = "/tmp/_dump_stream.failed";

/// The *streaming* dump job body: `pg_dump` pipes straight into `curl -T -`
/// against the host dump server — no `/workspace/_archive.dump`, so the dump
/// payload allocates zero blocks on the guest's data disk (the few-KB job
/// sentinel/log still land there). That matters because the virtio-blk has no
/// discard passthrough: any block a dump file ever touched stays allocated in
/// the host's sparse `data.ext4` after `rm`, permanently — the file-based job
/// inflated every disk it tried to archive, failed attempts included (the
/// snowball emergency-drain.sh documents).
///
/// A chunked upload whose producer dies just *ends* — the server can't tell
/// torn from complete — so the upload is two-phase: the streamed bytes are
/// held pending, and only this job, which alone knows pg_dump's exit code,
/// commits them (or aborts on failure). The pooler's wait loop trusts nothing
/// before the server records the commit.
fn streaming_dump_job_body(user: &str, db: &str, url: &str) -> String {
    let done = ARCHIVE_JOB.done;
    format!(
        "ec=0\n\
         rm -f {STREAM_FAIL_MARK}\n\
         code=$({{ pg_dump -h 127.0.0.1 -U {user} -Fc -d {db} || echo 1 > {STREAM_FAIL_MARK}; }} \
         | curl -sS -T - -o /dev/null -w '%{{http_code}}' \"{url}\") || ec=$?\n\
         if [ -f {STREAM_FAIL_MARK} ]; then\n\
         \techo 'pg_dump failed before the stream ended' >&2\n\
         \tec=1\n\
         fi\n\
         {}\
         if [ \"$ec\" = 0 ]; then\n\
         \tcode=$(curl -sS -X POST -o /dev/null -w '%{{http_code}}' \"{url}/commit\") || ec=$?\n\
         {}\
         else\n\
         \tcurl -sS -X POST -o /dev/null \"{url}/abort\" >/dev/null 2>&1 || true\n\
         fi\n\
         printf %s \"$ec\" > {done}.tmp && mv {done}.tmp {done}\n",
        require_2xx("upload"),
        require_2xx("commit"),
    )
}

/// The dump job body, planted as a *file* rather than run as `sh -c '…'`, so
/// the presigned URL — full of `&`/`=` query params — never has to survive a
/// layer of shell quoting. `ec` captures the first failing step so a failed
/// pg_dump never uploads a truncated object, and the sentinel records it.
/// `user`/`db` arrive already shell-quoted.
///
/// Only used against *S3* today (presigned PUTs need a Content-Length, so the
/// guest must dump to a file first) — and only as the legacy fallback when no
/// dump server is running (see [`dump_to_s3`]): the `-f {DUMP_PATH}` write is
/// exactly the disk-inflating behavior the streaming body exists to avoid.
fn archive_job_body(user: &str, db: &str, resolve: &str, url: &str) -> String {
    let done = ARCHIVE_JOB.done;
    format!(
        "ec=0\n\
         if pg_dump -h 127.0.0.1 -U {user} -Fc -d {db} -f {DUMP_PATH}; then\n\
         \tsync {DUMP_PATH} 2>/dev/null || true\n\
         \tif [ -s {DUMP_PATH} ]; then\n\
         \t\tcode=$(curl -sS {resolve} -T {DUMP_PATH} -o /dev/null -w '%{{http_code}}' \"{url}\") || ec=$?\n\
         {}\
         \telse\n\
         \t\techo \"dump file empty/missing after pg_dump reported success (disk trouble?)\" >&2\n\
         \t\tec=1\n\
         \tfi\n\
         else\n\
         \tec=$?\n\
         fi\n\
         rm -f {DUMP_PATH}\n\
         printf %s \"$ec\" > {done}.tmp && mv {done}.tmp {done}\n",
        require_2xx("upload")
    )
}

/// Shell prelude for jobs that talk to the *local* dump server: resolve the
/// host's address as the guest's default gateway (the host side of its tap)
/// from `/proc/net/route` — the guest image has no `iproute2`, and the pooler
/// can't know each VM's gateway from outside. Sets `$GW`, which the job's URL
/// references (`http://$GW:port/...`, expanded by the shell inside the curl's
/// double quotes). On failure it writes the job's failure sentinel and exits,
/// so the pooler gets a prompt, explained error instead of a deadline wait.
/// `/proc/net/route` stores the gateway as little-endian hex, hence the
/// byte-reversing `cut`s (POSIX sh has no substring expansion).
fn gw_prelude(done: &str) -> String {
    format!(
        "GW=$(awk '$2==\"00000000\" {{print $3; exit}}' /proc/net/route)\n\
         if [ -n \"$GW\" ]; then\n\
         \tGW=$(printf '%d.%d.%d.%d' \"0x$(echo \"$GW\" | cut -c7-8)\" \"0x$(echo \"$GW\" | cut -c5-6)\" \"0x$(echo \"$GW\" | cut -c3-4)\" \"0x$(echo \"$GW\" | cut -c1-2)\")\n\
         fi\n\
         if [ -z \"$GW\" ]; then\n\
         \techo 'cannot determine host gateway from /proc/net/route' >&2\n\
         \tprintf %s 1 > {done}.tmp && mv {done}.tmp {done}\n\
         \texit 0\n\
         fi\n"
    )
}

/// Fail the job unless S3 answered the transfer with a 2xx.
///
/// `curl -f` is not enough on its own: `--fail` only trips on 4xx/5xx, so a
/// **3xx** — notably the `301 Moved Permanently` S3 returns for a bucket in
/// another region — exits 0 with nothing transferred. That made a dump that
/// uploaded no bytes report success, and the caller then reclaimed the disk it
/// had just "archived". So the status code is checked explicitly, and anything
/// that is not 2xx is a failed job.
///
/// A curl-level failure (`ec` already non-zero) keeps its own exit code; only an
/// otherwise-clean run with a bad status is attributed to 22, curl's own
/// "HTTP error returned". `$code` is empty when curl died before a response.
fn require_2xx(what: &str) -> String {
    format!(
        "\tcase \"$code\" in\n\
         \t2??) ;;\n\
         \t*) echo \"{what} rejected by S3: HTTP $code\" >&2; \
         if [ \"$ec\" = 0 ]; then ec=22; fi ;;\n\
         \tesac\n"
    )
}

/// The restore job body: fetch the archive from S3, load it into the
/// already-created database, drop the scratch copy. Same `ec`/sentinel
/// discipline as the dump, so a failed download never looks like a successful
/// restore of nothing.
///
/// `--clean --if-exists` makes the load idempotent: a *previous* restore
/// attempt that died partway (host outage, kill mid-load) leaves a partially
/// restored database on the reused VM's persistent disk, and without it every
/// retry fails on `relation already exists` — one interrupted restore wedged
/// the schema forever. Object-level clean handles that; `--if-exists` keeps it
/// a no-op on the genuinely fresh, empty database of the common case.
/// `keep_ownership` is set for a *dedicated* database ([`crate::dedicated`]),
/// and it matters: the default `--no-owner --no-privileges` makes every
/// restored object belong to the restoring superuser and drops the dump's
/// grants, which for an ordinary schema is exactly right (the dump may name
/// roles this cluster has never heard of) but for a dedicated one silently
/// hands the tenant back a database it cannot read — its tables would come back
/// owned by `postgres`. The owning role is guaranteed to exist before any
/// restore runs (`ensure_database` creates it first), so here the dump's own
/// ownership and grants can and must be replayed.
fn restore_job_body(user: &str, db: &str, resolve: &str, url: &str, keep_ownership: bool) -> String {
    let done = RESTORE_JOB.done;
    let ownership = if keep_ownership {
        ""
    } else {
        "--no-owner --no-privileges "
    };
    format!(
        "ec=0\n\
         code=$(curl -sS {resolve} -o {RESTORE_PATH} -w '%{{http_code}}' \"{url}\") || ec=$?\n\
         {}\
         if [ \"$ec\" = 0 ]; then\n\
         \tpg_restore -h 127.0.0.1 -U {user} --clean --if-exists {ownership}-j \"$(nproc)\" -d {db} {RESTORE_PATH} || ec=$?\n\
         fi\n\
         rm -f {RESTORE_PATH}\n\
         printf %s \"$ec\" > {done}.tmp && mv {done}.tmp {done}\n",
        require_2xx("download")
    )
}

impl DetachedJob {
    /// One exec that plants `job` and launches it: clear any prior run's
    /// sentinel/scratch (`scratch` is the job's own transfer file), write the
    /// body through a quoted heredoc (verbatim — no expansion, so `$ec`/`$?` and
    /// the URL land intact), then background it in a fresh session so it
    /// survives the exec returning. `echo` keeps the exec itself instant.
    /// PGPASSWORD is set on the launch exec and inherited by the detached job.
    ///
    /// `job` must end in a newline — the heredoc delimiter has to start its own
    /// line or `sh` never closes the redirect.
    fn launch_script(&self, job: &str, scratch: &str) -> String {
        let (script, done, log) = (self.script, self.done, self.log);
        format!(
            "rm -f {done} {scratch} {log}\n\
             cat > {script} <<'{JOB_HEREDOC_EOF}'\n\
             {job}{JOB_HEREDOC_EOF}\n\
             setsid sh {script} </dev/null >{log} 2>&1 &\n\
             echo launched\n"
        )
    }

    async fn launch(
        &self,
        cfg: &Config,
        sandbox: &Sandbox,
        job: &str,
        scratch: &str,
    ) -> Result<()> {
        let what = self.what;
        let res = exec_guest(
            cfg,
            sandbox,
            &self.launch_script(job, scratch),
            true,
            &format!("{what} job (launch)"),
        )
        .await?;
        if res.exit_code != 0 {
            bail!(
                "launching detached {what} failed (exit {}): {}",
                res.exit_code,
                truncate(exec_detail(&res), 800)
            );
        }
        Ok(())
    }

    /// The status probe. Built to be as cheap as a guest command can be, because
    /// it competes for the serial console with a `pg_dump`/`pg_restore`
    /// saturating the VM: `[`, `printf` and `read` are shell builtins, so this
    /// forks nothing.
    fn probe_command(&self) -> String {
        let done = self.done;
        format!(
            "if [ -f {done} ]; then read c < {done}; \
             printf '{PROBE_TAG}D%s\\n' \"$c\"; \
             else printf '{PROBE_TAG}P\\n'; fi"
        )
    }

    async fn probe(&self, cfg: &Config, sandbox: &Sandbox) -> Result<JobState> {
        let what = self.what;
        let res = exec_guest(
            cfg,
            sandbox,
            &self.probe_command(),
            false,
            &format!("probing {what} job"),
        )
        .await?;
        Ok(parse_probe(&res.stdout))
    }

    /// Best-effort tail of the job's guest-side log, so a failure (bad
    /// credentials, S3 4xx, disk-full) is visible in the error instead of dying
    /// with the VM. Read through the guest shell, not `files()` (see
    /// [`dump_to_s3`]); an unreadable log must not mask the job's own failure,
    /// so this degrades to an empty tail.
    async fn log_tail(&self, cfg: &Config, sandbox: &Sandbox) -> String {
        let tail = format!("tail -c 2000 {} 2>/dev/null || true", self.log);
        exec_guest(cfg, sandbox, &tail, false, "reading job log")
            .await
            .map(|r| r.stdout)
            .unwrap_or_default()
    }
}

/// Wait for the detached dump to land, on two independent signals:
///
/// * **S3 `HEAD`** (authoritative, when there is a [`Baseline::Known`]). An
///   object at `key` differing from the baseline means this run's upload
///   completed. Costs nothing from the guest, so it survives a wedged exec
///   channel — and it is the only signal that proves the archive exists before
///   the caller reclaims the source disk.
/// * **the guest sentinel** (best-effort). Turns a *failed* dump into an
///   immediate, explained error rather than a [`ARCHIVE_DEADLINE`]-long wait.
///   Every failure mode of this signal — timeout, garbled read, VM too busy to
///   answer — is treated as "no information", never as a failed dump.
///
/// Neither signal alone can be dropped: with a [`Baseline::Unknown`] the S3 side
/// cannot distinguish this archive from the previous one at the same key, so the
/// sentinel becomes load-bearing and is never muted.
///
/// Only [`ARCHIVE_DEADLINE`] ends the wait unsuccessfully.
async fn await_archive(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    s3: &S3Config,
    http: &reqwest::Client,
    key: &str,
    baseline: Option<&crate::s3::ObjectId>,
) -> Result<()> {
    let deadline = Instant::now() + ARCHIVE_DEADLINE;
    let mut probe_failures: u32 = 0;
    let mut probes_muted = false;
    // When the guest first claimed the upload was done. Once it has, waiting the
    // full deadline out is pointless — either the object shows up within a few
    // polls or it is not coming — and a prompt, specific error beats a generic
    // half-hour timeout.
    let mut claimed_done: Option<Instant> = None;
    loop {
        sleep(ARCHIVE_POLL_INTERVAL).await;

        // 1. Did the object land? Strongly-consistent for a fresh PUT, so an
        //    object that differs from the baseline is proof, not a hint — but
        //    only where there *is* a baseline to differ from.
        //
        //    The binding records whether this HEAD could not be completed at
        //    all, as opposed to completing and reporting the object
        //    absent/unchanged — the two mean opposite things when the guest
        //    claims success below.
        let head_unavailable = match s3.head_object(http, key, ARCHIVE_HEAD_TIMEOUT).await {
            Ok(now) => {
                if let Some(now) = &now
                    && Some(now) != baseline
                {
                    // A new object landed — but existence is not validity. An
                    // implausibly small object is a failed dump that still
                    // uploaded (torn dump file under disk pressure); reporting
                    // it durable would let the caller destroy the source disk.
                    if now.content_length < MIN_ARCHIVE_BYTES {
                        bail!(
                            "schema {schema}: upload at s3://{}/{key} is only {} bytes — \
                             no pg_dump archive is that small, so the dump itself \
                             produced no data (disk-full tears the dump file this way); \
                             refusing to report this archive durable. Guest log: {}",
                            s3.bucket,
                            now.content_length,
                            truncate(ARCHIVE_JOB.log_tail(cfg, sandbox).await.trim(), 800)
                        );
                    }
                    info!(
                        "schema {schema}: archive present in s3://{}/{key} ({} bytes)",
                        s3.bucket, now.content_length
                    );
                    return Ok(());
                }
                false
            }
            // A HEAD failure is about our S3 path, not the dump. Keep waiting;
            // the deadline still bounds us.
            Err(e) => {
                warn!("schema {schema}: HEAD while waiting for archive failed: {e:#}");
                true
            }
        };

        // 2. Ask the guest, unless it has stopped answering — see
        //    `ARCHIVE_MAX_PROBE_FAILURES`. Never fatal on its own.
        if !probes_muted {
            match ARCHIVE_JOB.probe(cfg, sandbox).await {
                Ok(JobState::Failed(code)) => {
                    let log = ARCHIVE_JOB.log_tail(cfg, sandbox).await;
                    bail!(
                        "detached dump job for schema {schema} failed (exit {code}): {}",
                        truncate(log.trim(), 800)
                    );
                }
                // The job says it uploaded successfully — which is a claim, not
                // proof, and never sufficient on its own. Keep looping and let a
                // HEAD confirm it.
                //
                // This deliberately does *not* fall back to trusting the guest
                // when the pooler can't reach S3. It used to, on the reasoning
                // that a clean `curl` exit meant S3 had accepted the PUT; that
                // reasoning was wrong twice over. `curl -f` also exits 0 on a
                // 3xx, so a `301` for a wrong-region bucket read as success —
                // and when the pooler cannot HEAD the object, the most likely
                // cause is the very same misconfiguration that is breaking the
                // guest's upload, so the two "independent" signals fail
                // together. Accepting the guest's word there archived nothing
                // and then reclaimed the disk.
                //
                // A dump that cannot be confirmed therefore fails. The cost is
                // an unreaped VM and a retry; the cost of the other choice is
                // the workbook.
                Ok(JobState::Succeeded) => {
                    probe_failures = 0;
                    claimed_done.get_or_insert_with(Instant::now);
                }
                Ok(JobState::Running) => probe_failures = 0,
                Err(e) => {
                    // Safe to give up on the guest: S3 decides this one, and the
                    // probe is only here to turn a failed dump into a prompt
                    // error rather than a deadline-long wait.
                    let budget = ARCHIVE_MAX_PROBE_FAILURES;
                    probe_failures += 1;
                    warn!(
                        "schema {schema}: dump-job probe {probe_failures}/{budget} failed \
                         (the dump itself is unaffected — it does not use this channel): {e:#}"
                    );
                    if probe_failures >= budget {
                        probes_muted = true;
                        warn!(
                            "schema {schema}: guest exec channel is not answering; \
                             waiting on S3 alone until {ARCHIVE_DEADLINE:?} elapses"
                        );
                    }
                }
            }
        }

        // The guest finished and S3 still doesn't show the object. Something
        // between the two is lying — a wrong-region redirect the guest read as
        // success, a bucket policy, a key mismatch — and none of it will resolve
        // by waiting. Fail now, naming both halves, rather than at the deadline.
        if let Some(at) = claimed_done
            && at.elapsed() >= ARCHIVE_CONFIRM_GRACE
        {
            bail!(
                "dump job for schema {schema} reported success but s3://{}/{key} \
                 {} {ARCHIVE_CONFIRM_GRACE:?} later — refusing to report this \
                 archive as durable while the source VM is about to be reclaimed. \
                 Guest log: {}",
                s3.bucket,
                if head_unavailable {
                    "could not be checked"
                } else {
                    "still holds no new object"
                },
                truncate(ARCHIVE_JOB.log_tail(cfg, sandbox).await.trim(), 800)
            );
        }

        if Instant::now() >= deadline {
            bail!(
                "dump of schema {schema} did not appear at s3://{}/{key} within \
                 {ARCHIVE_DEADLINE:?}",
                s3.bucket
            );
        }
    }
}

/// Dump `schema` to the local dump server (the frozen tier's counterpart of
/// [`dump_to_s3`]). Same detached guest job — `pg_dump` then `curl -T` — but
/// the upload lands on the host, and completion is confirmed *in-process* by
/// the server having fully written, fsync'd, and renamed the file: an even
/// stronger signal than the S3 tier's HEAD, with the same refusal to trust
/// the guest's word alone. Returns the completed dump's byte count.
pub async fn dump_to_local(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    srv: &crate::dumpsrv::DumpServer,
    port: u16,
) -> Result<u64> {
    let db = shell_squote(schema);
    let user = shell_squote(&cfg.pg_user);
    let token = srv.issue(schema, crate::dumpsrv::Mode::Put)?;
    let url = format!("http://$GW:{port}/d/{token}");
    let body = format!(
        "{}{}",
        gw_prelude(ARCHIVE_JOB.done),
        streaming_dump_job_body(&user, &db, &url)
    );
    ARCHIVE_JOB.launch(cfg, sandbox, &body, STREAM_FAIL_MARK).await?;
    await_server_upload(cfg, sandbox, schema, srv, &token).await
}

/// Wait out a detached dump against *our* dump server, on two signals — the
/// server's own completion record (authoritative: bytes fully written,
/// fsync'd, renamed after the guest's commit) and the guest sentinel
/// (best-effort, for prompt explained failures). Mirrors [`await_archive`]'s
/// trust posture: an unconfirmed dump is a failed dump.
async fn await_server_upload(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    srv: &crate::dumpsrv::DumpServer,
    token: &str,
) -> Result<u64> {
    let deadline = Instant::now() + ARCHIVE_DEADLINE;
    let mut probe_failures: u32 = 0;
    let mut probes_muted = false;
    let mut claimed_done: Option<Instant> = None;
    loop {
        sleep(ARCHIVE_POLL_INTERVAL).await;
        // Authoritative: our own server fully wrote and renamed the upload.
        if let Some(bytes) = srv.upload_completed(token) {
            if bytes < MIN_ARCHIVE_BYTES {
                bail!(
                    "schema {schema}: local dump is only {bytes} bytes — no pg_dump \
                     archive is that small, so the dump itself produced no data; \
                     refusing to trust it. Guest log: {}",
                    truncate(ARCHIVE_JOB.log_tail(cfg, sandbox).await.trim(), 800)
                );
            }
            info!("schema {schema}: local dump complete ({bytes} bytes)");
            return Ok(bytes);
        }
        // Guest sentinel: turns a failed dump into a prompt, explained error.
        if !probes_muted {
            match ARCHIVE_JOB.probe(cfg, sandbox).await {
                Ok(JobState::Failed(code)) => {
                    let log = ARCHIVE_JOB.log_tail(cfg, sandbox).await;
                    bail!(
                        "detached local-dump job for schema {schema} failed (exit {code}): {}",
                        truncate(log.trim(), 800)
                    );
                }
                Ok(JobState::Succeeded) => {
                    probe_failures = 0;
                    claimed_done.get_or_insert_with(Instant::now);
                }
                Ok(JobState::Running) => probe_failures = 0,
                Err(e) => {
                    probe_failures += 1;
                    warn!(
                        "schema {schema}: local-dump probe {probe_failures}/\
                         {ARCHIVE_MAX_PROBE_FAILURES} failed (the dump itself is \
                         unaffected): {e:#}"
                    );
                    if probe_failures >= ARCHIVE_MAX_PROBE_FAILURES {
                        probes_muted = true;
                    }
                }
            }
        }
        // The guest claims success but our server never saw a committed upload
        // land — same trust posture as the S3 path: unconfirmed means failed.
        if let Some(at) = claimed_done
            && at.elapsed() >= ARCHIVE_CONFIRM_GRACE
        {
            bail!(
                "local-dump job for schema {schema} reported success but the dump \
                 server never received a committed upload — refusing to trust it"
            );
        }
        if Instant::now() >= deadline {
            bail!("local dump of schema {schema} did not complete within {ARCHIVE_DEADLINE:?}");
        }
    }
}

/// Restore `schema` from the local dump server (frozen tier). The preflight is
/// a direct file check — stronger than the S3 HEAD — then the same detached
/// guest job as the S3 restore, against a tokened local URL.
pub async fn restore_from_local(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    srv: &crate::dumpsrv::DumpServer,
    port: u16,
    owner: Option<&crate::dedicated::Credential>,
) -> Result<()> {
    let path = srv.dump_path(schema);
    match crate::dumpsrv::dump_size(&path) {
        None => bail!(
            "schema {schema} is marked frozen but its local dump {} does not exist — \
             nothing to restore (was PG_VM_POOL_DUMP_DIR changed or the file removed?)",
            path.display()
        ),
        Some(n) if n < MIN_ARCHIVE_BYTES => bail!(
            "schema {schema}: local dump {} is only {n} bytes — produced by a failed \
             dump; it holds no data and cannot be restored",
            path.display()
        ),
        Some(_) => {}
    }
    let db = shell_squote(schema);
    let user = shell_squote(&cfg.pg_user);
    let token = srv.issue(schema, crate::dumpsrv::Mode::Get)?;
    let url = format!("http://$GW:{port}/d/{token}");
    let body = format!(
        "{}{}",
        gw_prelude(RESTORE_JOB.done),
        restore_job_body(&user, &db, "", &url, owner.is_some())
    );
    RESTORE_JOB.launch(cfg, sandbox, &body, RESTORE_PATH).await?;
    await_detached_job(cfg, sandbox, schema, RESTORE_JOB).await?;
    crate::events::record(crate::events::Event::RestoreLocal);
    Ok(())
}

/// What the guest says about a detached job.
enum JobState {
    Running,
    Succeeded,
    Failed(i32),
}

/// Locate and read a probe reply in whatever the shared console handed back.
///
/// The reply is framed with a distinctive token and located *anywhere* in the
/// output rather than at its start: the console is shared, so a line of kernel
/// log or the tail of a previously timed-out command can precede ours. An
/// unparseable reply is `Running` — the safe reading, since the sentinel's own
/// zero grants success and its non-zero declares failure; neither should be
/// inferred from noise.
fn parse_probe(stdout: &str) -> JobState {
    // Last match wins: stale framing from an earlier, abandoned command sits
    // ahead of this reply in the stream.
    let Some(reply) = stdout
        .lines()
        .filter_map(|l| l.trim().rsplit_once(PROBE_TAG).map(|(_, r)| r))
        .rfind(|r| r.starts_with('D') || r.starts_with('P'))
    else {
        return JobState::Running;
    };
    match reply.strip_prefix('D') {
        // A sentinel we can't parse is a real completion with an unreadable
        // code — treat it as failed rather than spin until the deadline.
        Some(code) => match code.trim().parse::<i32>() {
            Ok(0) => JobState::Succeeded,
            Ok(c) => JobState::Failed(c),
            Err(_) => JobState::Failed(-1),
        },
        None => JobState::Running,
    }
}

/// Restore `schema`'s database from S3 into the (already-created, empty) target
/// database, using the guest's `curl` + `pg_restore` against a presigned GET.
///
/// Detached and polled, exactly like [`dump_to_s3`] and for the same reason: as
/// one foreground exec this could only ever restore a database small enough to
/// download *and* load inside the guest exec channel's hard 30s cap. Every real
/// workbook exceeds that, so the restore would be killed mid-`pg_restore` and
/// the bring-up would fail — leaving a schema that archives fine and then cannot
/// come back.
///
/// Unlike the dump there is no out-of-band signal to fall back on: S3 can attest
/// that the *source* object exists, not that this guest finished loading it. So
/// the sentinel decides, and a guest that stops answering long enough eventually
/// fails the restore. The asymmetry is deliberate — an unconfirmed restore
/// aborts a bring-up and costs a retry, whereas an unconfirmed dump would have
/// cost the disk it was dumping.
async fn restore_from_s3(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    s3: &S3Config,
    owner: Option<&crate::dedicated::Credential>,
) -> Result<()> {
    let key = s3.object_key(schema);

    // Pre-flight: is there actually a restorable archive at the key? Feeding
    // `pg_restore` a missing or torn (sub-minimum) object costs a full VM
    // bring-up and yields a generic guest error; checking here yields the
    // truth. Best-effort on transport failure — the guest's own download would
    // surface that anyway.
    if let Ok(http) = reqwest::Client::builder().build() {
        match s3.head_object(&http, &key, ARCHIVE_HEAD_TIMEOUT).await {
            Ok(None) => bail!(
                "schema {schema} is marked archived but s3://{}/{key} does not exist — \
                 there is no archive to restore",
                s3.bucket
            ),
            Ok(Some(id)) if id.content_length < MIN_ARCHIVE_BYTES => bail!(
                "schema {schema}: the archive at s3://{}/{key} is only {} bytes — it was \
                 produced by a failed dump (accepted before the size guard existed) and \
                 holds no data; this workbook cannot be restored from S3",
                s3.bucket,
                id.content_length
            ),
            Ok(Some(_)) => {}
            Err(e) => warn!(
                "schema {schema}: pre-restore HEAD failed (continuing — the guest's \
                 own download will decide): {e:#}"
            ),
        }
    }

    let url = s3.presign_get(&key, PRESIGN_TTL);
    let resolve = s3_resolve_flag(s3).await;
    let db = shell_squote(schema);
    let user = shell_squote(&cfg.pg_user);

    RESTORE_JOB
        .launch(
            cfg,
            sandbox,
            &restore_job_body(&user, &db, &resolve, &url, owner.is_some()),
            RESTORE_PATH,
        )
        .await?;
    await_detached_job(cfg, sandbox, schema, RESTORE_JOB).await?;
    crate::events::record(crate::events::Event::RestoreS3);
    Ok(())
}

/// Wait for a detached job whose only completion signal is its own sentinel.
///
/// Probe failures are tolerated — they describe the exec channel, not the job —
/// but not indefinitely: with nothing else to ask, a channel that never comes
/// back means we can never confirm the job, and reporting failure is the honest
/// answer. Bounded by [`ARCHIVE_DEADLINE`] either way.
async fn await_detached_job(
    cfg: &Config,
    sandbox: &Sandbox,
    schema: &str,
    job: DetachedJob,
) -> Result<()> {
    let what = job.what;
    let deadline = Instant::now() + ARCHIVE_DEADLINE;
    let mut probe_failures: u32 = 0;
    loop {
        sleep(ARCHIVE_POLL_INTERVAL).await;
        match job.probe(cfg, sandbox).await {
            Ok(JobState::Succeeded) => return Ok(()),
            Ok(JobState::Failed(code)) => {
                let log = job.log_tail(cfg, sandbox).await;
                bail!(
                    "detached {what} job for schema {schema} failed (exit {code}): {}",
                    truncate(log.trim(), 800)
                );
            }
            Ok(JobState::Running) => probe_failures = 0,
            Err(e) => {
                probe_failures += 1;
                warn!(
                    "schema {schema}: {what}-job probe {probe_failures}/{UNWITNESSED_MAX_PROBE_FAILURES} \
                     failed (the {what} itself does not use this channel): {e:#}"
                );
                if probe_failures >= UNWITNESSED_MAX_PROBE_FAILURES {
                    bail!(
                        "lost contact with the guest while waiting for the {what} of \
                         schema {schema} ({probe_failures} consecutive probe failures); \
                         last error: {e:#}"
                    );
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "detached {what} job for schema {schema} did not finish within {ARCHIVE_DEADLINE:?}"
            );
        }
    }
}

/// Build a `curl --resolve host:443:IP[,IP…]` flag for the S3 host by resolving
/// it **on the pooler's host**, because the guest microVM ships without a DNS
/// resolver (`/etc/resolv.conf` is empty). Only applies to the AWS
/// virtual-hosted path; a custom endpoint (MinIO/R2) is left to the guest to
/// resolve. Returns an empty string when there's nothing to pin (custom
/// endpoint, or resolution fails/times out) — curl then behaves exactly as
/// before, falling back to the guest's own DNS, so this never makes a working
/// setup worse.
///
/// ASSUMPTION: the guest reaches S3 through the host's NAT egress (it shares the
/// host's outbound path), so a public IP the host resolves is reachable from the
/// guest. True for a co-located host talking to public S3; it would misroute
/// under split-horizon DNS — e.g. an S3 VPC endpoint that resolves to
/// subnet-private IPs valid only from certain subnets. Revisit this if the guest
/// ever gets a distinct egress path or S3 moves behind a VPC endpoint.
async fn s3_resolve_flag(s3: &S3Config) -> String {
    let Some(host) = s3.resolve_host() else {
        return String::new();
    };
    const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
    // Cap the number of IPs pinned: enough for curl to fail over across a couple
    // of front-ends at connect time without an unwieldy flag.
    const MAX_IPS: usize = 4;
    let lookup = tokio::net::lookup_host((host.as_str(), 443u16));
    let addrs = match tokio::time::timeout(RESOLVE_TIMEOUT, lookup).await {
        Ok(Ok(addrs)) => addrs,
        Ok(Err(e)) => {
            warn!(
                "resolving {host} on host for guest curl failed ({e}); guest will use its own DNS"
            );
            return String::new();
        }
        Err(_) => {
            warn!("resolving {host} on host for guest curl timed out; guest will use its own DNS");
            return String::new();
        }
    };
    // v4 only: the guest tap/NAT is IPv4 (a v6 address the host prefers would be
    // unreachable from the guest).
    let mut ips: Vec<String> = addrs
        .filter_map(|sa| match sa.ip() {
            std::net::IpAddr::V4(v4) => Some(v4.to_string()),
            std::net::IpAddr::V6(_) => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips.truncate(MAX_IPS);
    build_resolve_flag(&host, &ips)
}

/// Assemble the `--resolve` flag from a host and already-resolved IPv4 strings.
/// Pure (no I/O) so it's unit-testable. Empty when there are no IPs. The value
/// is single-quoted; host is our own `{bucket}.s3.{region}.amazonaws.com` and
/// the IPs are validated v4, so nothing here can break out of the guest shell.
fn build_resolve_flag(host: &str, ips: &[String]) -> String {
    if ips.is_empty() {
        return String::new();
    }
    // curl takes a comma-separated address list in one --resolve entry and tries
    // them in order at connect time (repeating the flag for the same host:port
    // would NOT add addresses — curl keeps the first entry).
    format!("--resolve '{host}:443:{}'", ips.join(","))
}

/// Issue one guest exec and hand back its raw result without judging the exit
/// code — the caller decides. `with_pgpassword` injects `PGPASSWORD` for commands
/// that shell out to `pg_dump`/`pg_restore`; a bare `cat`/`test` poll doesn't need
/// it. The exec's own foreground command must finish inside the guest API's ~30s
/// server-side cap; [`dump_to_s3`] keeps every call it makes trivially short.
async fn exec_guest(
    cfg: &Config,
    sandbox: &Sandbox,
    command: &str,
    with_pgpassword: bool,
    what: &str,
) -> Result<CommandResult> {
    let env = if with_pgpassword {
        cfg.pg_password.as_ref().map(|pw| {
            let mut m = HashMap::new();
            m.insert("PGPASSWORD".to_string(), pw.clone());
            m
        })
    } else {
        None
    };
    let opts = CommandRunOptions {
        timeout: Some(GUEST_EXEC_HTTP_TIMEOUT),
        env,
        ..Default::default()
    };
    sandbox
        .commands()
        .run(command, opts)
        .await
        .with_context(|| format!("{what}: guest exec failed"))
}

/// Best-effort human-readable detail from a failed guest command: the combined
/// output if the backend populated it, else stderr.
fn exec_detail(res: &CommandResult) -> &str {
    if res.output.trim().is_empty() {
        res.stderr.trim()
    } else {
        res.output.trim()
    }
}

/// Single-quote a string for POSIX `sh`, escaping embedded single quotes as
/// `'\''`. Schema/user names are already validated (no control chars) upstream;
/// this is defense in depth so a name with a space or quote can't break out.
fn shell_squote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Trim guest output to `max` bytes (on a char boundary) so an error log can't
/// dump a whole dump-tool backtrace.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// How many client connections this VM's Postgres can actually take, read from
/// the server itself rather than assumed.
///
/// init.sh derives `max_connections` per size class, so the pooler must not
/// hardcode it: the number differs across size classes, and a VM that changes
/// class picks up a new one on its next boot. Ask the server.
///
/// The budget is what's left for *ordinary clients* after the two claims that
/// aren't theirs: `superuser_reserved_connections`, and this pooler's own
/// housekeeping pool (probes, bootstrap, stats, pre-stop CHECKPOINT), which
/// connects as superuser and so draws from the same well.
///
/// On any failure, fall back to a conservative floor rather than refusing to
/// serve — an unknown limit shouldn't take the VM down, and a low guess only
/// costs queueing.
async fn client_slot_budget(pool: &Pool, name: &str) -> usize {
    const FALLBACK_SLOTS: usize = 20;
    let read = async {
        let client = pool.get().await.ok()?;
        let max: i64 = client
            .query_one("SELECT current_setting('max_connections')::int8", &[])
            .await
            .ok()?
            .get(0);
        let reserved: i64 = client
            .query_one(
                "SELECT current_setting('superuser_reserved_connections')::int8",
                &[],
            )
            .await
            .ok()?
            .get(0);
        Some((max, reserved))
    };
    match read.await {
        Some((max, reserved)) => {
            let slots = slots_from_limits(max, reserved);
            info!(
                "{name}: admitting at most {slots} client connections \
                 (max_connections={max}, superuser_reserved={reserved}, \
                 pooler pool={POOL_MAX_SIZE})"
            );
            slots
        }
        None => {
            warn!("{name}: could not read max_connections; admitting at most {FALLBACK_SLOTS}");
            FALLBACK_SLOTS
        }
    }
}

/// Client slots left over from `max_connections` once the reserved superuser
/// slots and the pooler's own housekeeping pool are subtracted.
///
/// Saturates at 1 rather than 0: admitting nobody would make the VM useless,
/// and a guest configured this tightly is better served by letting one client
/// through at a time than by refusing every client. Never returns more than the
/// arithmetic allows — over-admitting is the exact failure this exists to stop.
fn slots_from_limits(max: i64, reserved: i64) -> usize {
    let budget = max - reserved - POOL_MAX_SIZE as i64;
    usize::try_from(budget.max(1)).unwrap_or(1)
}

/// Resolve the splice target and connection pool for a running VM's Postgres:
/// reached either directly over the host tap (guest_ip:5432) when the pooler
/// shares the host with the VM, or via a local iroh tunnel otherwise. Direct
/// connect skips iroh entirely — no relay dependency, lower latency, faster
/// bring-up.
async fn connect_pg(
    cfg: &Config,
    sandbox: &Sandbox,
    name: &str,
) -> Result<(SocketAddr, Option<P2pTunnel>, Pool)> {
    let (target, tunnel) = if cfg.direct_connect {
        match direct_target(sandbox).await {
            Ok(Some(addr)) => {
                info!("direct connection to {name} at {addr} (no tunnel)");
                (addr, None)
            }
            Ok(None) => {
                warn!("{name}: daemon reported no guest_ip; falling back to iroh tunnel");
                let (addr, t) = open_tunnel(cfg, sandbox, name).await?;
                (addr, Some(t))
            }
            Err(e) => {
                warn!("{name}: guest_ip lookup failed ({e:#}); falling back to iroh tunnel");
                let (addr, t) = open_tunnel(cfg, sandbox, name).await?;
                (addr, Some(t))
            }
        }
    } else {
        let (addr, t) = open_tunnel(cfg, sandbox, name).await?;
        (addr, Some(t))
    };

    // deadpool against the VM's default `postgres` db: used to probe readiness
    // (the VM status can be Running before Postgres accepts connections) and to
    // create the per-schema database the client will ask for.
    let host = target.ip().to_string();
    let pool = build_pool(
        &host,
        target.port(),
        "postgres",
        &cfg.pg_user,
        cfg.pg_password.as_deref(),
    )?;
    Ok((target, tunnel, pool))
}

/// Get the VM's Postgres to a ready state, power-cycling the VM if the server
/// process is dead inside it.
///
/// Postgres can crash while its VM stays alive (OOM kill, segfault): init.sh
/// runs Postgres as a background child of the PID-1 shell, so the sandbox
/// still reports Running, `start()` no-ops, and without this check every
/// connect would burn the full `ready_timeout` against a port nobody listens
/// on. Instead, probe briefly and classify what's there:
///   - answers `SELECT 1`      → ready, proceed;
///   - speaks Postgres protocol (e.g. 57P03 "the database system is starting
///     up" during WAL replay)  → the server is alive, wait out `ready_timeout`
///     like before — restarting mid-recovery would only restart recovery;
///   - stalled (accepted but never answered) → ambiguous, and a power-cycle
///     here is destructive: an ingest-loaded VM can hold a connect past the
///     probe bound while perfectly healthy. Treat it like `Responding` and
///     wait. If it really is wedged, the client gets a timeout error and the
///     next connect re-probes from scratch — recoverable, unlike a reboot
///     that kills an in-flight load;
///   - refusing                → the postmaster is gone; stop+start the VM
///     (a fresh boot re-runs init.sh, which relaunches Postgres) and wait for
///     readiness on the rebuilt connection. One cycle per connect attempt —
///     if PG still won't come up on a fresh boot, that's a real error the
///     client should see (and the next connect retries from scratch).
async fn ready_pg(
    cfg: &Config,
    sandbox: &Sandbox,
    name: &str,
) -> Result<(SocketAddr, Option<P2pTunnel>, Pool)> {
    let (target, tunnel, pool) = connect_pg(cfg, sandbox, name).await?;
    match probe_pg_window(&pool, PG_PROBE_WINDOW).await {
        PgProbe::Ready => Ok((target, tunnel, pool)),
        PgProbe::Responding(msg) => {
            info!("{name}: Postgres up but not ready yet ({msg}); waiting");
            wait_pg_ready(&pool, cfg.ready_timeout, name).await?;
            Ok((target, tunnel, pool))
        }
        PgProbe::Stalled(msg) => {
            warn!(
                "{name}: Postgres slow to answer ({msg}); waiting out \
                 ready_timeout before considering a power-cycle"
            );
            // Don't reboot on a stall alone — but don't wedge forever either.
            // A loaded server answers well inside ready_timeout; a black-holed
            // forward never answers at all. Silence for the *whole* window is
            // the evidence that separates them, so the reboot survives for the
            // dead-tunnel case it exists for without firing at a busy VM.
            if wait_pg_ready(&pool, cfg.ready_timeout, name).await.is_ok() {
                return Ok((target, tunnel, pool));
            }
            warn!("{name}: still silent after ready_timeout; power-cycling the VM");
            power_cycle(cfg, sandbox, name, pool, tunnel).await
        }
        PgProbe::Unreachable(msg) => {
            warn!(
                "{name}: Postgres unreachable inside a running VM ({msg}); \
                 power-cycling the VM"
            );
            power_cycle(cfg, sandbox, name, pool, tunnel).await
        }
    }
}

/// Stop+start the VM and reconnect. A fresh boot re-runs init.sh, which
/// relaunches Postgres and rebuilds the tunnel. One cycle per connect attempt —
/// if PG still won't come up on a fresh boot, that's a real error the client
/// should see (and the next connect retries from scratch).
///
/// Destructive: the stop is an unclean kill, so anything in flight on this VM
/// dies with it. Only call this on evidence that nothing is listening — never
/// on evidence that the server is merely slow.
async fn power_cycle(
    cfg: &Config,
    sandbox: &Sandbox,
    name: &str,
    pool: Pool,
    tunnel: Option<P2pTunnel>,
) -> Result<(SocketAddr, Option<P2pTunnel>, Pool)> {
    // Drop the stale pool/tunnel before the restart so nothing holds the old
    // forward open across the reboot.
    drop(pool);
    drop(tunnel);
    // The stop→start gap makes the disk look reclaimable; hold the boot permit
    // across both so a reclaim pass can't start working on it in between.
    // The bring-up slot bounds how many such boots hit the daemon at once, and
    // is taken *after* the permit (see `bring_up_existing`).
    {
        let _permit = crate::reclaim::boot_permit(sandbox.sandbox_id()).await;
        let _slot = bringup_slot(name).await;
        sandbox
            .stop()
            .await
            .with_context(|| format!("stopping {name} for power-cycle"))?;
        sandbox
            .start()
            .await
            .with_context(|| format!("restarting {name} after power-cycle"))?;
    }
    wait_ready(sandbox, cfg.ready_timeout, name)
        .await
        .with_context(|| format!("waiting for {name} after power-cycle"))?;
    // Reconnect from scratch: the guest_ip/tunnel from before the reboot may no
    // longer be valid.
    let (target, tunnel, pool) = connect_pg(cfg, sandbox, name).await?;
    match wait_pg_ready(&pool, cfg.ready_timeout, name).await {
        Ok(()) => {}
        Err(e) => {
            // Postgres didn't come up even on a FRESH boot — a persistently
            // sick guest (unmountable data disk killing init, a crash-looping
            // postmaster, an incompatible pgdata). The timeout alone is a dead
            // end to debug, so grab whatever the guest can still say. Exec
            // rides the serial console, which only answers while PID-1
            // survived — an exec failure here usually means init.sh itself
            // died (e.g. `set -e` at the data-disk mount), which is evidence
            // too, and is reported as such.
            let evidence = boot_evidence(cfg, sandbox).await;
            return Err(e).with_context(|| format!("{name} failed a fresh boot ({evidence})"));
        }
    }
    info!("{name}: Postgres recovered after power-cycle");
    Ok((target, tunnel, pool))
}

/// Best-effort one-line diagnosis of a guest whose Postgres won't start.
/// Ordered by decisiveness:
///
/// 1. cluster major (`PG_VERSION`) vs server major — a mismatch is the
///    instant-death "database files are incompatible" case (a disk adopted
///    under a newer image) and explains everything by itself;
/// 2. live postgres process count (0 = it died, not "it's slow");
/// 3. this boot's startup stderr (`pg-startup.log`, written by init.sh —
///    where pre-logging-collector fatals land);
/// 4. the newest server-log tail — which can be DAYS old on a VM whose
///    current boot never got far enough to log; the timestamps say so.
///
/// Never fails — every failure mode becomes descriptive text.
async fn boot_evidence(cfg: &Config, sandbox: &Sandbox) -> String {
    let cmd = "v=$(cat /workspace/pgdata/PG_VERSION 2>/dev/null || echo '?'); \
               s=$(postgres --version 2>/dev/null | grep -oE '[0-9]+' | head -1 || echo '?'); \
               echo \"pgdata=v$v server=v$s pg-procs=$(pgrep -c postgres 2>/dev/null || echo 0)\"; \
               tail -n 4 /workspace/pg-startup.log 2>/dev/null; \
               tail -n 3 \"$(ls -t /workspace/pgdata/log/*.log 2>/dev/null | head -1)\" 2>/dev/null \
               || dmesg 2>/dev/null | tail -n 3";
    match exec_guest(cfg, sandbox, cmd, false, "collecting boot evidence").await {
        Ok(res) => {
            let text = exec_detail(&res).replace(['\n', '\r'], " | ");
            let text = text.trim();
            if text.is_empty() {
                "guest exec answered but produced no output".to_string()
            } else {
                truncate(text, 400).to_string()
            }
        }
        Err(e) => format!(
            "guest exec unavailable — init likely died during boot (data-disk \
             mount failure?): {e:#}"
        ),
    }
}

/// What a bounded `SELECT 1` attempt tells us about the server behind `pool`.
pub(crate) enum PgProbe {
    Ready,
    /// Got a Postgres protocol response that isn't readiness (server error
    /// with a SQLSTATE, e.g. "starting up") — the process is alive.
    Responding(String),
    /// The attempt ran out of time with no answer either way. Ambiguous: a
    /// loaded server can take seconds to fork a backend, so this is NOT
    /// evidence the postmaster is gone. See `probe_pg`.
    Stalled(String),
    /// No protocol response at all: connection refused or closed. Nothing is
    /// listening on the port.
    Unreachable(String),
}

pub(crate) async fn probe_pg(pool: &Pool) -> PgProbe {
    use deadpool_postgres::PoolError;
    let attempt = async {
        match pool.get().await {
            Ok(client) => match client.simple_query("SELECT 1").await {
                Ok(_) => PgProbe::Ready,
                Err(e) => classify_pg_error(&e),
            },
            Err(PoolError::Backend(e)) => classify_pg_error(&e),
            // Everything else `PoolError` reports (queued past `wait`, pool
            // closed, no runtime) is a fact about *our* pool, not about the
            // VM's postmaster — a probe that never left this process is not
            // evidence the server is gone, and must never reach the verdict
            // that power-cycles it.
            Err(e) => PgProbe::Stalled(format!("pool checkout failed locally: {e}")),
        }
    };
    // The pool has no create timeout, so a black-holed TCP connect (dead iroh
    // tunnel forward) would hang `get()` — bound each attempt.
    //
    // A timeout is deliberately NOT `Unreachable`. A dead postmaster means a
    // closed port, and a closed port answers *fast* (ECONNREFUSED) — it does
    // not hang. Hanging means something accepted the connection and is slow to
    // finish it: a backend fork behind heavy checkpoint I/O, or an allocation
    // stalling under the guest's strict overcommit. Calling that "dead" is how
    // a busy-but-healthy VM used to get power-cycled mid-ingest, which is
    // strictly worse than the slowness it was reacting to.
    match tokio::time::timeout(PG_PROBE_ATTEMPT, attempt).await {
        Ok(probe) => probe,
        Err(_) => PgProbe::Stalled(format!("no answer within {PG_PROBE_ATTEMPT:?}")),
    }
}

/// A SQLSTATE means the *server* composed an error message — the postmaster is
/// alive whatever the code says. No SQLSTATE means we never got a protocol
/// reply (io error, refused, EOF): nothing is listening.
fn classify_pg_error(e: &tokio_postgres::Error) -> PgProbe {
    if e.code().is_some() {
        PgProbe::Responding(e.to_string())
    } else {
        PgProbe::Unreachable(e.to_string())
    }
}

/// Probe until the window closes: `Ready`/`Responding` short-circuit (the
/// server exists — the caller decides how long to wait for readiness). Only a
/// full window of *refusals* returns `Unreachable`; if anything in the window
/// merely stalled, the port was open at least once and `Stalled` wins, since
/// the caller must not take a destructive action on that evidence.
async fn probe_pg_window(pool: &Pool, window: Duration) -> PgProbe {
    let start = Instant::now();
    let deadline = start + window;
    let mut last_err = String::new();
    let mut stalled: Option<String> = None;
    loop {
        match probe_pg(pool).await {
            PgProbe::Unreachable(msg) => last_err = msg,
            PgProbe::Stalled(msg) => stalled = Some(msg),
            verdict => return verdict,
        }
        if Instant::now() >= deadline {
            return match stalled {
                Some(msg) => PgProbe::Stalled(msg),
                None => PgProbe::Unreachable(last_err),
            };
        }
        sleep(pg_poll_interval(start.elapsed())).await;
    }
}

/// Find or bring up the VM. Prefers reattaching by `known_id` (a prior bring-up
/// of this schema): querying a sandbox by id is consistent, whereas a VM that
/// was just stopped is briefly missing from list-by-name — reattaching by name
/// in that window would create a *duplicate* VM with a fresh, empty data disk
/// and silently lose the schema's data. Only when there's no known id (a
/// genuinely new schema) or it was deleted do we list-by-name / create.
/// `pub(crate)` for `crate::loadtest`, which times this phase on its own: it is
/// the whole "silent" window between a client connecting and the daemon seeing
/// any traffic for its VM, and measuring it without a real Postgres behind the
/// VM is the only way to attribute cold-start latency to fleet size.
pub(crate) async fn resolve_sandbox(
    cfg: &Config,
    name: &str,
    keepalive: bool,
    known_id: Option<&str>,
    spares: Option<(&crate::spares::SparePool, &std::collections::HashSet<String>)>,
) -> Result<(Sandbox, bool)> {
    // 1. Reattach to the VM we last used for this schema, by id.
    if let Some(id) = known_id {
        match bring_up_existing(cfg, name, id).await {
            Ok(Some(sb)) => return Ok((sb, false)),
            Ok(None) => info!("known VM {name} ({id}) is gone; find-or-create by name"),
            Err(e) => warn!("reattaching {name} ({id}) failed ({e:#}); find-or-create by name"),
        }
    }

    // 2. Fall back to by-name resolution (first connect on a fresh pooler, or
    //    the known id was deleted). The positive-only cache answers repeat
    //    lookups without any daemon traffic; a hit is verified by id (a stale
    //    entry evicts itself), and a miss goes to the authoritative `?name=`
    //    daemon lookup — never the O(fleet) inventory pull this used to be,
    //    and never straight to create.
    if let Some(id) = crate::inventory::lookup(name) {
        match bring_up_existing(cfg, name, &id).await? {
            Some(sb) => return Ok((sb, false)),
            None => {
                crate::inventory::remove_id(&id);
                info!("cached VM {name} ({id}) is gone; asking the daemon by name");
            }
        }
    }
    if let Some(info) = find_by_name_with_retry(name)
        .await
        .context("by-name sandbox lookup")?
    {
        crate::inventory::insert(name, &info.id);
        match bring_up_existing(cfg, name, &info.id).await? {
            Some(sb) => return Ok((sb, false)),
            None => crate::inventory::remove_id(&info.id),
        }
    }

    // 3. Genuinely new VM needed. Claim a warm spare if one is available —
    //    already booted with initdb done, so the whole create+boot+init cost
    //    is skipped. It keeps its spare name; the registry's id mapping (put
    //    on successful bring-up) is what binds it to the schema. The `true`
    //    tells `ensure_vm` this sandbox is a claim it must release (kill)
    //    if the rest of the bring-up fails.
    if let Some((pool, bound)) = spares
        && let Some(sb) = pool.take(bound).await
    {
        info!("claiming warm spare {} for {name}", sb.sandbox_id());
        return Ok((sb, true));
    }

    // 4. No spare: create from scratch.
    create_vm(cfg, name, keepalive).await.map(|sb| (sb, false))
}

/// Grow a sandbox's persistent data device to `target_gb` through the
/// daemon's offline workspace resize (`POST /deployed-sandboxes/{id}/resize`
/// with `disk_size_gb`; heyvmd's workspace-resize feature, grow-only).
///
/// The daemon takes the sandbox's lifecycle lock, stops it, grows the image
/// and its ext4 in place, cold-boots once to verify the new capacity from
/// inside the guest, and restores the stopped state — so calling this on a
/// VM that was just idle-stopped is free of client disruption. The whole
/// round-trip can take minutes (a cold boot is embedded in it); the timeout
/// reflects that.
///
/// Raw HTTP rather than the SDK: the published heyo-sdk (0.1.5) predates
/// `Sandbox::resize_disk`. Swap to the SDK call once 0.1.6 ships — the wire
/// format here is byte-identical to it.
pub(crate) async fn resize_disk(sandbox_id: &str, target_gb: u64) -> Result<()> {
    resize_disk_at(daemon_base_url(), sandbox_id, target_gb).await
}

/// [`resize_disk`] against an explicit daemon base URL — split out so tests
/// can exercise the real HTTP round-trip against an in-process server.
async fn resize_disk_at(base_url: &str, sandbox_id: &str, target_gb: u64) -> Result<()> {
    anyhow::ensure!(
        (1..=250).contains(&target_gb),
        "disk_size_gb must be within 1–250 GiB (daemon limit)"
    );
    let url = format!("{base_url}/deployed-sandboxes/{sandbox_id}/resize");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .context("building HTTP client for daemon resize")?;
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(format!("{{\"disk_size_gb\":{target_gb}}}"))
        .send()
        .await
        .context("calling the daemon's workspace resize")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "daemon workspace resize returned {status}: {} (a 404/405 means the \
             deployed heyvmd predates the workspace-resize feature)",
            body.trim()
        );
    }
    Ok(())
}

/// Best-effort stop of whatever VM a failed offload bring-up may have left
/// running — the ready-timeout case: the sandbox started, but Postgres never
/// answered (sick disk, wedged boot), so `ensure_vm` errored without ever
/// returning a handle to stop. Nothing else owns such a VM (the warm entry
/// was evicted before the attempt), so without this it runs forever, burning
/// RAM and pinning its disk against reclaim. Matches by the stored id and by
/// `pg-<schema>` name (covers a VM freshly created inside the failed
/// attempt); every step is logged, none propagate.
pub(crate) async fn stop_after_failed_bringup(schema: &str, known_id: Option<&str>) {
    // Act on ids we already hold — the pending ledger names the VM this very
    // attempt created, `known_id` names a reattach target — so the common case
    // needs no listing at all. Listing is the fallback, not the front door: a
    // failed bring-up usually means a struggling daemon, and the old
    // list-first version failed its listing in exactly those moments and
    // silently cleaned up nothing (that's where the stranded-VM pileups came
    // from). The stop (not delete) is deliberate: a post-ready failure can
    // leave partial restore state on the disk, and the next attempt reuses
    // the VM idempotently; a VM that stays unbound is the pending janitor's
    // to delete.
    let mut ids: Vec<String> = crate::pending::get(schema).into_iter().collect();
    if let Some(id) = known_id
        && !ids.iter().any(|i| i == id)
    {
        ids.push(id.to_string());
    }
    if ids.is_empty() {
        let name = format!("pg-{schema}");
        match find_by_name_with_retry(&name).await {
            Ok(found) => ids.extend(found.map(|i| i.id)),
            Err(e) => {
                warn!(
                    "schema {schema}: cannot resolve {name} to stop a leaked bring-up: {e:#}"
                );
                return;
            }
        }
    }
    for id in ids {
        let Ok(sb) = Sandbox::connect(id.clone(), local_opts()) else {
            continue;
        };
        match tokio::time::timeout(Duration::from_secs(30), sb.stop()).await {
            Ok(Ok(())) => info!(
                "schema {schema}: stopped VM {id} left running by the failed bring-up"
            ),
            Ok(Err(e)) => warn!(
                "schema {schema}: stopping leaked VM {id} failed (may already be stopped): {e:#}"
            ),
            Err(_) => warn!("schema {schema}: stopping leaked VM {id} timed out"),
        }
    }
}

/// Create one warm-spare VM (see `spares`): identical to a schema VM — same
/// image, size class, thin data disk, TTL 0 — just parked with an empty
/// cluster until claimed.
pub(crate) async fn create_spare(cfg: &Config, name: &str) -> Result<Sandbox> {
    create_vm_within(cfg, name, false, cfg.ready_timeout.min(SPARE_READY_TIMEOUT)).await
}

/// Is this VM's Postgres accepting connections? A plain TCP connect to the
/// guest's 5432 — enough to prove the guest booted, mounted its data disk,
/// finished `initdb` and started the postmaster, which is the whole point of
/// holding a spare. Cheap (no daemon call beyond the id lookup, no auth, no
/// pool) so the replenisher can run it over the pool every pass.
///
/// `Ok(None)` means the check doesn't apply: the daemon reports no `guest_ip`
/// (non-tap backend, or not assigned yet), so there is no direct address to
/// probe and the caller must fall back to the daemon's own status.
pub(crate) async fn pg_listening(sandbox: &Sandbox) -> Result<Option<bool>> {
    let Some(target) = direct_target(sandbox).await? else {
        return Ok(None);
    };
    let connected = tokio::time::timeout(PG_PROBE_ATTEMPT, tokio::net::TcpStream::connect(target))
        .await
        .is_ok_and(|r| r.is_ok());
    Ok(Some(connected))
}

/// Connect to an existing sandbox by id and force it to a running, ready state.
/// `Ok(None)` means it no longer exists (deleted out-of-band → caller creates).
///
/// Issues `start()` directly rather than checking status first. Two reasons:
/// (1) a status check via `get()` has a *side effect* on the daemon — for a
/// stopped Firecracker VM it rehydrates a handle that reports `running`, which
/// then makes the subsequent `start()` a no-op (VM stays down) and previously
/// deadlocked the daemon. (2) `start()` is the right primitive regardless: it
/// starts a stopped VM and no-ops a genuinely running one. A `NotFound` means
/// the sandbox was deleted, so the caller should create a fresh one.
async fn bring_up_existing(cfg: &Config, name: &str, id: &str) -> Result<Option<Sandbox>> {
    let sb = Sandbox::connect(id.to_string(), local_opts())
        .with_context(|| format!("connecting to VM {name} by id {id}"))?;
    info!("bringing up existing VM {name} ({id})");
    // A stopped VM's disk may be mid-fsck/shrink under a reclaim pass; booting
    // over that destroys the filesystem. Hold the permit only across start():
    // once the VM process has the disk open, the reclaim script's in-use checks
    // protect it — the pass-start snapshot between passes, and the live
    // re-check it runs under this same per-disk lock during one.
    //
    // Permit first, slot second — the order everywhere a boot takes both. The
    // permit can block (it preempts a reclaim pass, but the pass still has to
    // reach a yield point); waiting for it while holding one of the three
    // bring-up slots would spend the pooler's scarcest resource on a wait that
    // needs no daemon at all, and three such boots would freeze every
    // bring-up on the host. Reclaim never takes a bring-up slot, so there is
    // no cycle to deadlock on.
    let started = {
        let _permit = crate::reclaim::boot_permit(id).await;
        let _slot = bringup_slot(name).await;
        sb.start().await
    };
    match started {
        Ok(()) => {}
        Err(HeyoError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("starting VM {name}"))),
    }
    wait_ready(&sb, cfg.ready_timeout, name)
        .await
        .with_context(|| format!("waiting for VM {name}"))?;
    crate::inventory::insert(name, id);
    Ok(Some(sb))
}

/// Create a brand-new VM for a schema (with its persistent data disk).
/// `pub(crate)` for the image-restore path, which creates the VM itself and
/// swaps the restored disk in under it before first use.
pub(crate) async fn create_vm(cfg: &Config, name: &str, keepalive: bool) -> Result<Sandbox> {
    create_vm_within(cfg, name, keepalive, cfg.ready_timeout).await
}

/// [`create_vm`] with an explicit readiness budget — warm spares get a shorter
/// one than a client-facing bring-up (see [`SPARE_READY_TIMEOUT`]).
async fn create_vm_within(
    cfg: &Config,
    name: &str,
    keepalive: bool,
    ready_timeout: Duration,
) -> Result<Sandbox> {
    info!(
        "creating VM {name}{}",
        if keepalive { " (keep-alive)" } else { "" }
    );
    let sandbox = {
        let _slot = bringup_slot(name).await;
        Sandbox::create(
            SandboxCreateOptions {
                name: Some(name.to_string()),
                image: Some(cfg.image.clone()),
                driver: Some(SandboxDriver::Firecracker),
                open_ports: vec![VM_PG_PORT],
                size_class: Some(cfg.size_class),
                // Persistent data disk → /dev/vdb → /workspace → PGDATA, so the
                // schema's data survives VM stop/start/restart.
                disk_size_gb: Some(cfg.data_disk_gb),
                // Always 0: the pooler owns VM lifecycle. Keep-alive schemas stay up;
                // others are stopped by the pooler's idle reaper, which tracks
                // connections — something the daemon's absolute TTL can't do.
                ttl_seconds: Some(0),
                // ZERO = return as soon as the deploy POST answers; the ready
                // poll runs below, outside the bring-up gate, so a slot is held
                // only while the daemon is actually building and booting.
                wait_for_ready: Some(Duration::ZERO),
                ..Default::default()
            },
            HeyoClientOptions {
                timeout: Some(DEPLOY_HTTP_TIMEOUT),
                ..local_opts()
            },
        )
        .await
        .with_context(|| format!("creating VM {name}"))?
    };
    // The daemon 202-accepts deploys, so this id exists (with a daemon-side
    // record behind it) long before the VM is usable — and until the registry
    // binds schema→id on full bring-up success, this variable is the only
    // owner. Ledger it immediately so a failure anywhere downstream leaves a
    // *named* leak the failure path and the pending janitor can kill by id,
    // instead of an anonymous `provisioning` record nothing ever reaps.
    // (Spare creates skip the ledger: `spare-pg-*` are covered by purge.)
    if let Some(schema) = name.strip_prefix("pg-") {
        crate::pending::record(schema, sandbox.sandbox_id()).await;
    }
    crate::inventory::insert(name, sandbox.sandbox_id());
    if let Err(e) = wait_ready(&sandbox, ready_timeout, name).await {
        // Kill the half-built VM now, by the id in hand — no listing, which is
        // exactly what's unreachable when bring-ups fail en masse. It never
        // served a client, so its disk holds nothing worth keeping. Best
        // effort: if the kill can't land either, the ledger entry stays and
        // the janitor retries once the daemon recovers.
        match tokio::time::timeout(Duration::from_secs(30), sandbox.kill()).await {
            Ok(Ok(())) => {
                info!(
                    "{name}: deleted half-provisioned VM {} after failed bring-up",
                    sandbox.sandbox_id()
                );
                crate::inventory::remove_id(sandbox.sandbox_id());
                if let Some(schema) = name.strip_prefix("pg-") {
                    crate::pending::clear(schema).await;
                }
            }
            Ok(Err(kill_err)) => warn!(
                "{name}: deleting half-provisioned VM {} failed (janitor will retry): {kill_err:#}",
                sandbox.sandbox_id()
            ),
            Err(_) => warn!(
                "{name}: deleting half-provisioned VM {} timed out (janitor will retry)",
                sandbox.sandbox_id()
            ),
        }
        return Err(e).with_context(|| format!("waiting for created VM {name}"));
    }
    crate::events::record(crate::events::Event::VmCreated);
    Ok(sandbox)
}

/// Resolve the VM's direct host-reachable Postgres address from the daemon's
/// `guest_ip` (populated for tap backends). `None` when the daemon doesn't
/// report one (non-tap backend, or not yet assigned) so the caller can fall
/// back to a tunnel.
async fn direct_target(sandbox: &Sandbox) -> Result<Option<SocketAddr>> {
    let info = sandbox.get().await.context("fetching sandbox info")?;
    let Some(ip) = info.guest_ip.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let addr: IpAddr = ip
        .parse()
        .with_context(|| format!("parsing guest_ip {ip:?}"))?;
    Ok(Some(SocketAddr::new(addr, VM_PG_PORT)))
}

/// Expose the VM's Postgres over an iroh tunnel and return the local splice
/// address plus the tunnel handle (aborted when dropped, so the caller must
/// hold it for the entry's lifetime). `P2pTunnel::connect` has no internal
/// timeout — when iroh's relays churn (host IP flapping on WiFi) it can block
/// for minutes — so bound the whole handshake and fail fast for a retry.
async fn open_tunnel(
    cfg: &Config,
    sandbox: &Sandbox,
    name: &str,
) -> Result<(SocketAddr, P2pTunnel)> {
    let handshake = async {
        let ticket = sandbox
            .expose_tcp(VM_PG_PORT)
            .await
            .context("exposing VM Postgres port")?;
        P2pTunnel::connect(&ticket, None)
            .await
            .context("connecting P2P tunnel")
    };
    let tunnel = match tokio::time::timeout(cfg.connect_timeout, handshake).await {
        Ok(res) => res?,
        Err(_) => bail!(
            "tunnel setup for {name} timed out after {:?} — iroh relays likely \
             churning (host network unstable); will retry on next connect",
            cfg.connect_timeout
        ),
    };
    let local_port = tunnel.local_port();
    info!("tunnel for {name} ready on 127.0.0.1:{local_port}");
    Ok((SocketAddr::from(([127, 0, 0, 1], local_port)), tunnel))
}

/// Cap on the pooler's own connections to a VM's Postgres.
///
/// This pool is not the client data path — client bytes are spliced straight to
/// the VM — so it only ever serves the pooler's own housekeeping: the liveness
/// probe, the one-time database bootstrap, the dashboard's stat queries, and
/// the pre-stop CHECKPOINT. A handful of slots covers all of that concurrently.
///
/// Left unset, deadpool defaults `max_size` to `logical_cpus * 2`, sized for a
/// pool that *is* the data path. That default is read off the **pooler host**,
/// which has nothing to do with the guest's `max_connections` — a 16-core host
/// yields 32, so the pooler could hold a third of a large VM's 100 connections
/// just to ask "are you alive?". Worse, `entry_alive` probes on every client
/// checkout, so a burst of client connects grows this pool straight to its cap
/// at exactly the moment the VM can least afford it, and the pool connects as
/// superuser — so it eats the reserved slots and survives while the app starves.
const POOL_MAX_SIZE: usize = 4;

fn build_pool(
    host: &str,
    port: u16,
    dbname: &str,
    user: &str,
    password: Option<&str>,
) -> Result<Pool> {
    let mut pg = PgConfig::new();
    pg.host = Some(host.to_string());
    pg.port = Some(port);
    pg.dbname = Some(dbname.to_string());
    pg.user = Some(user.to_string());
    // Only set a password when configured; leaving it None keeps `trust` auth
    // working (an empty-string password would be sent as a real credential).
    pg.password = password.map(str::to_string);
    pg.pool = Some(deadpool_postgres::PoolConfig {
        max_size: POOL_MAX_SIZE,
        // Bound the queue for a slot. Callers treat a local checkout failure as
        // `Stalled` (never `Unreachable`), so this can only cost a probe, never
        // trigger a power-cycle. `create` bounds the TCP connect itself, which
        // otherwise has no timeout at all.
        timeouts: deadpool_postgres::Timeouts {
            wait: Some(PG_PROBE_ATTEMPT),
            create: Some(PG_PROBE_ATTEMPT),
            recycle: Some(PG_PROBE_ATTEMPT),
        },
        ..Default::default()
    });
    pg.create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls)
        .context("building deadpool pool")
}

/// Retry until Postgres answers a trivial query or the timeout elapses. Logs a
/// periodic warning while it waits so a VM that boots but never brings Postgres
/// up (e.g. a missing data disk → no PGDATA) shows the reason in the log
/// instead of the caller silently blocking for the whole `timeout`.
async fn wait_pg_ready(pool: &Pool, timeout: Duration, name: &str) -> Result<()> {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut last_log = start;
    loop {
        let last_err = match pool.get().await {
            Ok(client) => match client.simple_query("SELECT 1").await {
                Ok(_) => return Ok(()),
                Err(e) => e.to_string(),
            },
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            bail!("Postgres on {name} not ready within {timeout:?}: {last_err}");
        }
        if last_log.elapsed() >= Duration::from_secs(15) {
            warn!(
                "still waiting for Postgres on {name} ({:?} elapsed, timeout {timeout:?}): {last_err}",
                start.elapsed()
            );
            last_log = Instant::now();
        }
        sleep(pg_poll_interval(start.elapsed())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_at(port: u16) -> Pool {
        build_pool("127.0.0.1", port, "postgres", "postgres", None).unwrap()
    }

    /// Exhaust the gate, confirm nothing is left, and confirm dropped permits
    /// return to the pool — a leaked permit here would silently serialize all
    /// production bring-ups down to N-1 slots forever. Capacity-agnostic so an
    /// inherited PG_VM_POOL_MAX_CONCURRENT_BRINGUPS doesn't break the test.
    #[tokio::test]
    async fn bringup_gate_bounds_and_recycles_slots() {
        let Some(gate) = bringup_gate() else {
            return; // gate disabled via env — nothing to check
        };
        let cap = gate.available_permits();
        assert!(cap > 0, "an enabled gate must have at least one slot");
        let mut held = Vec::new();
        for i in 0..cap {
            held.push(bringup_slot(&format!("test-{i}")).await);
        }
        assert_eq!(gate.available_permits(), 0, "gate must be exhaustible");
        held.pop();
        assert_eq!(gate.available_permits(), 1, "dropped permits must recycle");
        drop(held);
        assert_eq!(gate.available_permits(), cap);
    }


    /// The launch script is parsed by the guest's `sh`, and its heredoc carries
    /// a presigned URL (`&`, `=`, `%`, `?`) plus literal `$ec`/`$?`. Run the
    /// real thing through a real shell — with `cat`/`setsid`/`pg_dump` shadowed
    /// by stubs on PATH — and assert the file that lands is byte-identical to
    /// the job we asked for. A quoting slip here is invisible until an archive
    /// silently uploads nothing.
    #[test]
    fn launch_script_plants_the_job_verbatim() {
        let url = "https://wb.s3.us-east-2.amazonaws.com/x.dump?X-Amz-Algorithm=AWS4-HMAC-SHA256\
                   &X-Amz-Credential=AK%2F20260721%2Fus-east-2%2Fs3%2Faws4_request\
                   &X-Amz-Signature=deadbeef&x=`whoami`&y=$(id)";
        let user = shell_squote("postgres");
        let db = shell_squote("Kb0s7KwS");
        let resolve = build_resolve_flag("wb.s3.us-east-2.amazonaws.com", &["3.5.130.160".into()]);

        for (job_desc, body, scratch, planted_as) in [
            (
                ARCHIVE_JOB,
                archive_job_body(&user, &db, &resolve, url),
                DUMP_PATH,
                "_archive.job.sh",
            ),
            (
                RESTORE_JOB,
                restore_job_body(&user, &db, &resolve, url, false),
                RESTORE_PATH,
                "_restore.job.sh",
            ),
        ] {
            plants_verbatim(job_desc, &body, scratch, planted_as, url);
        }
    }

    /// The streaming body must be real `sh`, never touch the data disk, and
    /// keep commit strictly on the success path (a commit reachable after a
    /// failed pg_dump would finalize torn bytes as a durable archive).
    #[test]
    fn streaming_dump_job_is_valid_sh_and_commits_only_on_success() {
        let user = shell_squote("postgres");
        let db = shell_squote("Kb0s7KwS");
        let body = streaming_dump_job_body(&user, &db, "http://$GW:6433/d/tok0123");

        // Syntax-check the exact bytes with a real shell.
        let f = std::env::temp_dir().join(format!("pgfc-streamjob-{}.sh", std::process::id()));
        std::fs::write(&f, &body).unwrap();
        let ok = std::process::Command::new("sh")
            .arg("-n")
            .arg(&f)
            .status()
            .unwrap()
            .success();
        let _ = std::fs::remove_file(&f);
        assert!(ok, "streaming job body must parse as sh:\n{body}");

        // No payload writes to the data disk: the whole point of the streaming
        // route. (The job's sentinel/log still land there — a few KB, versus
        // the full dump the file-based route wrote.)
        assert!(!body.contains(DUMP_PATH), "streaming job must not write the dump to the data disk");
        assert!(body.contains(STREAM_FAIL_MARK), "pipefail surrogate present");

        // Commit only inside the ec=0 branch; abort on the other side. The
        // anchor is the branch on its own line — require_2xx's inline
        // `; if [ "$ec" = 0 ]; then ec=22` must not match.
        let branch = "\nif [ \"$ec\" = 0 ]; then\n";
        let success_branch = body.split(branch).nth(1).unwrap();
        let (commit_side, abort_side) = success_branch.split_once("\nelse\n").unwrap();
        assert!(commit_side.contains("/commit"));
        assert!(!commit_side.contains("/abort"));
        assert!(abort_side.contains("/abort"));
        assert!(!abort_side.contains("/commit"));
        // And nothing commits before the branch.
        let preamble = body.split(branch).next().unwrap();
        assert!(!preamble.contains("/commit"));
    }

    /// Run one job's real launch script through a real `sh` — with every command
    /// it calls shadowed by stubs — and assert the file that lands is
    /// byte-identical to the job we asked for.
    fn plants_verbatim(
        job_desc: DetachedJob,
        body: &str,
        scratch: &str,
        planted_as: &str,
        url: &str,
    ) {
        let job = body.to_string();
        let script = job_desc.launch_script(&job, scratch);

        let dir = std::env::temp_dir().join(format!(
            "pgfc-launch-{}-{}",
            std::process::id(),
            job_desc.what
        ));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        // Redirect the guest's absolute /workspace paths into the temp dir, and
        // stub every command the script calls so nothing actually runs. The same
        // rewrite applies to the expected body, since it is embedded in the
        // script we run.
        let root = format!("{}/", dir.display());
        let script = script.replace("/workspace/", &root);
        let job = job.replace("/workspace/", &root);
        for cmd in ["setsid", "pg_dump", "pg_restore", "curl"] {
            let p = dir.join("bin").join(cmd);
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env("PATH", format!("{}/bin:/usr/bin:/bin", dir.display()))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "launch script failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "launched");

        let planted = std::fs::read_to_string(dir.join(planted_as)).unwrap();
        assert_eq!(
            planted, job,
            "heredoc must plant the {} job byte-for-byte — no expansion, no requoting",
            job_desc.what
        );
        // The URL's `&`/`%`/backticks must have survived intact: a mangled URL
        // yields an S3 403 long after the VM that could explain it is gone.
        assert!(
            planted.contains(url),
            "presigned URL was altered in transit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dump whose upload S3 *redirected* must record failure, not success.
    ///
    /// This is the bug that cost a workbook: the bucket lived in a region other
    /// than the configured one, S3 answered the PUT `301 Moved Permanently`, and
    /// `curl -fsS` exited 0 — `--fail` only trips on 4xx/5xx. The job wrote a `0`
    /// sentinel, the pooler reported the archive durable, and the caller killed
    /// the VM and reclaimed the disk. Nothing had been uploaded.
    #[test]
    fn a_redirected_upload_is_a_failed_dump() {
        // Every status a misrouted or rejected transfer can come back as, plus
        // the ones that must still count as success.
        for (code, want_ok) in [
            ("200", true),
            ("204", true),
            ("301", false), // the production failure
            ("307", false),
            ("403", false),
            ("500", false),
            ("000", false), // curl never got a response
        ] {
            let ec = run_archive_job_with_curl_status(code);
            assert_eq!(
                ec == "0",
                want_ok,
                "HTTP {code} upload recorded exit code {ec:?}; \
                 a non-2xx must never write a zero sentinel"
            );
        }
    }

    /// Run the real archive job body under a real `sh`, with `curl` stubbed to
    /// report `code` the way curl's `-w '%{http_code}'` does, and return the exit
    /// code the job wrote to its sentinel.
    fn run_archive_job_with_curl_status(code: &str) -> String {
        let dir = std::env::temp_dir().join(format!("pgfc-status-{}-{code}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let root = format!("{}/", dir.display());

        let body = archive_job_body(
            &shell_squote("postgres"),
            &shell_squote("s"),
            "",
            "https://x/y",
        )
        .replace("/workspace/", &root);

        // pg_dump succeeds and produces a non-empty dump (the empty-file guard
        // would otherwise fail the job before curl runs); curl prints the
        // status to stdout and — as curl does for a redirect it isn't
        // following — exits 0 regardless.
        let stubs = [
            (
                "pg_dump",
                format!("#!/bin/sh\nprintf dumpbytes > {root}_archive.dump\nexit 0\n"),
            ),
            ("curl", format!("#!/bin/sh\nprintf %s '{code}'\nexit 0\n")),
        ];
        for (cmd, script) in stubs {
            let p = dir.join("bin").join(cmd);
            std::fs::write(&p, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&body)
            .env("PATH", format!("{}/bin:/usr/bin:/bin", dir.display()))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "job body itself must not error: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let sentinel =
            std::fs::read_to_string(dir.join(ARCHIVE_JOB.done.trim_start_matches("/workspace/")))
                .expect("job must always write a sentinel");
        let _ = std::fs::remove_dir_all(&dir);
        sentinel
    }

    /// The production failure of 2026-07-23: with the host disk full, the
    /// guest's dump file was torn to zero length while `pg_dump` and `curl`
    /// both exited 0 — a 0-byte "archive" was accepted and the VM (the only
    /// copy of the data) was killed. The job must fail before uploading when
    /// the dump file is empty or missing.
    #[test]
    fn an_empty_dump_file_never_uploads() {
        for create_file in [false, true] {
            let dir = std::env::temp_dir().join(format!(
                "pgfc-empty-{}-{create_file}",
                std::process::id()
            ));
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            let root = format!("{}/", dir.display());
            let body = archive_job_body(
                &shell_squote("postgres"),
                &shell_squote("s"),
                "",
                "https://x/y",
            )
            .replace("/workspace/", &root);

            // pg_dump "succeeds" but leaves the dump empty (or absent); curl
            // records that it ran — it must not.
            let dump_script = if create_file {
                format!("#!/bin/sh\n: > {root}_archive.dump\nexit 0\n")
            } else {
                "#!/bin/sh\nexit 0\n".to_string()
            };
            let stubs = [
                ("pg_dump", dump_script),
                ("curl", format!("#!/bin/sh\ntouch {root}curl-ran\nprintf 200\nexit 0\n")),
            ];
            for (cmd, script) in stubs {
                let p = dir.join("bin").join(cmd);
                std::fs::write(&p, script).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&body)
                .env("PATH", format!("{}/bin:/usr/bin:/bin", dir.display()))
                .output()
                .unwrap();
            assert!(out.status.success(), "job body itself must not error");
            let sentinel = std::fs::read_to_string(
                dir.join(ARCHIVE_JOB.done.trim_start_matches("/workspace/")),
            )
            .expect("job must always write a sentinel");
            assert_ne!(sentinel, "0", "an empty dump must never write a zero sentinel");
            assert!(
                !dir.join("curl-ran").exists(),
                "an empty dump must never be uploaded at all"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The probe shares a serial console with kernel logs and with the leftover
    /// output of commands the daemon already gave up on at its 30s timeout. The
    /// parser must find its own reply in that noise, and — critically — must
    /// never invent a `Succeeded` from it, since the caller destroys the source
    /// disk on success.
    #[test]
    fn probe_parser_survives_a_noisy_shared_console() {
        use JobState::*;
        let s = parse_probe;

        assert!(matches!(s("HEYOJOB:P"), Running));
        assert!(matches!(s("HEYOJOB:D0"), Succeeded));
        assert!(matches!(s("HEYOJOB:D22"), Failed(22)));
        // Trailing CR from the console's line discipline.
        assert!(matches!(s("HEYOJOB:D0\r\n"), Succeeded));

        // Kernel log + the tail of an abandoned earlier command ahead of ours.
        let noisy = "[  512.3] blk_update_request: I/O error\n\
                     HEYOJOB:P\n\
                     __HEYVM_1a2b_END__ 0\n\
                     HEYOJOB:D0\n";
        assert!(matches!(s(noisy), Succeeded), "last reply must win");

        // Nothing recognisable → keep waiting. Never `Succeeded`: an empty or
        // garbled read is exactly what a wedged console produces, and treating
        // it as success would kill a VM whose dump never uploaded.
        for junk in ["", "\n\n", "sh: read: not found", "HEYOJOB:", "D0"] {
            assert!(
                matches!(s(junk), Running),
                "unrecognised probe output {junk:?} must read as Running"
            );
        }
        // A completed job whose code is unreadable is a completion, not a hang.
        assert!(matches!(s("HEYOJOB:Dxx"), Failed(-1)));
    }

    /// Dump and restore must not share scratch paths: a restore reads its
    /// sentinel while the VM may still carry the previous dump's, and crossed
    /// paths would have one job read the other's exit code.
    #[test]
    fn detached_jobs_have_disjoint_scratch_paths() {
        let paths = [
            ARCHIVE_JOB.script,
            ARCHIVE_JOB.done,
            ARCHIVE_JOB.log,
            DUMP_PATH,
            RESTORE_JOB.script,
            RESTORE_JOB.done,
            RESTORE_JOB.log,
            RESTORE_PATH,
        ];
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            paths.len(),
            "scratch paths collide: {paths:?}"
        );
    }

    /// The probe runs on a VM whose single vCPU is saturated by `pg_dump`, so it
    /// must fork nothing — every builtin it uses (`[`, `read`, `printf`) has to
    /// really be a builtin. Run it under a PATH with *no* external commands at
    /// all: if the script reaches for `cat`/`tail`, it fails here.
    #[test]
    fn probe_command_is_fork_free_and_reports_both_states() {
        let dir = std::env::temp_dir().join(format!("pgfc-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let done = dir.join("_archive.done");
        let cmd = ARCHIVE_JOB
            .probe_command()
            .replace(ARCHIVE_JOB.done, done.to_str().unwrap());

        // Point PATH at an empty directory: `sh` itself is spawned by absolute
        // path, so nothing the script names can resolve to an external binary.
        let empty = dir.join("empty-path");
        std::fs::create_dir_all(&empty).unwrap();
        let run = || {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&cmd)
                .env("PATH", &empty)
                .output()
                .unwrap();
            assert!(
                out.stderr.is_empty(),
                "probe must not shell out: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };

        // No sentinel yet → pending.
        assert!(matches!(parse_probe(&run()), JobState::Running));
        // Sentinel written by the job → its exit code comes back intact.
        std::fs::write(&done, "0").unwrap();
        assert!(matches!(parse_probe(&run()), JobState::Succeeded));
        std::fs::write(&done, "7\n").unwrap();
        assert!(matches!(parse_probe(&run()), JobState::Failed(7)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_flag_pins_ips_or_stays_empty() {
        // No IPs → no flag, so curl falls back to the guest's own DNS unchanged.
        assert_eq!(build_resolve_flag("wb.s3.us-east-2.amazonaws.com", &[]), "");
        // One or more IPs → a single quoted, comma-joined --resolve entry.
        let one = build_resolve_flag("wb.s3.us-east-2.amazonaws.com", &["3.5.130.160".into()]);
        assert_eq!(
            one,
            "--resolve 'wb.s3.us-east-2.amazonaws.com:443:3.5.130.160'"
        );
        let many = build_resolve_flag(
            "wb.s3.us-east-2.amazonaws.com",
            &["3.5.130.160".into(), "52.219.0.1".into()],
        );
        assert_eq!(
            many,
            "--resolve 'wb.s3.us-east-2.amazonaws.com:443:3.5.130.160,52.219.0.1'"
        );
    }

    /// The pooler's pool is housekeeping-only and must not scale with the
    /// *pooler host's* core count — that number is unrelated to the guest's
    /// max_connections, and the default (logical_cpus * 2 = 32 on a 16-core
    /// host) would let the pooler hold a third of a large VM's connections
    /// just to run liveness probes.
    #[test]
    fn pool_is_capped_independently_of_host_cores() {
        let p = pool_at(5432);
        assert_eq!(
            p.status().max_size,
            POOL_MAX_SIZE,
            "pool must be explicitly capped, not inherited from host cores"
        );
        assert!(
            POOL_MAX_SIZE * 4 < 100,
            "several schema pools must still fit inside a guest's max_connections"
        );
    }

    /// A checkout that fails inside our own pool (queued past `wait`, pool
    /// closed) says nothing about the VM. It must never reach `Unreachable`,
    /// which is the verdict that power-cycles.
    #[tokio::test]
    async fn local_pool_exhaustion_is_not_unreachable() {
        // A listener that accepts but never speaks: checkouts occupy every slot
        // and stall, so further checkouts queue past `wait` and fail locally.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                std::mem::forget(sock);
            }
        });
        let pool = std::sync::Arc::new(pool_at(port));
        // Saturate every slot, then probe against the exhausted pool.
        for _ in 0..POOL_MAX_SIZE {
            let p = pool.clone();
            tokio::spawn(async move { p.get().await.map(|c| std::mem::forget(c)) });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !matches!(probe_pg(&pool).await, PgProbe::Unreachable(_)),
            "a local pool checkout failure must not be reported as Unreachable"
        );
    }

    /// The budget must never exceed what the server will actually accept —
    /// over-admitting reintroduces the `too many clients` FATAL this exists to
    /// prevent. A guest with a tiny max_connections must clamp down, not fall
    /// back to some default larger than the server allows.
    #[test]
    fn slot_budget_never_over_admits() {
        // The `large` VM in init.sh: 100 max, 5 reserved, 4 for our pool.
        assert_eq!(slots_from_limits(100, 5), 91);
        // The `micro` VM: 25 max.
        assert_eq!(slots_from_limits(25, 5), 16);
        // Degenerate guests: clamp to 1, never to a fallback bigger than the
        // server's own limit.
        for (max, reserved) in [(10, 5), (9, 5), (5, 5), (3, 5), (1, 0), (0, 0)] {
            let slots = slots_from_limits(max, reserved);
            assert!(slots >= 1, "must admit at least one client");
            assert!(
                slots as i64 <= max.max(1),
                "slots_from_limits({max}, {reserved}) = {slots} exceeds max_connections={max}"
            );
        }
    }

    #[tokio::test]
    async fn refused_port_probes_unreachable() {
        // Bind-then-drop to find a port nothing listens on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        // A closed port is the dead-postmaster signal: it refuses immediately.
        // This is the one verdict that may power-cycle, so it must stay exact.
        match probe_pg_window(&pool_at(port), Duration::from_secs(1)).await {
            PgProbe::Unreachable(_) => {}
            PgProbe::Ready => panic!("refused port reported Ready"),
            PgProbe::Responding(m) => panic!("refused port reported Responding: {m}"),
            PgProbe::Stalled(m) => panic!("refused port reported Stalled: {m}"),
        }
    }

    #[tokio::test]
    async fn black_holed_listener_probes_stalled_not_unreachable() {
        // Accepts TCP but never answers. Two very different things share this
        // shape: a tunnel whose far end is dead, and a healthy Postgres too
        // loaded to finish a backend fork inside the probe bound. They are
        // indistinguishable here, so the probe must report the ambiguity
        // (`Stalled`) rather than assert death — `ready_pg` resolves it by
        // waiting out ready_timeout, which only the live server survives.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                std::mem::forget(sock); // hold the socket open, say nothing
            }
        });

        match probe_pg_window(&pool_at(port), Duration::from_secs(1)).await {
            PgProbe::Stalled(_) => {}
            PgProbe::Ready => panic!("black-holed listener reported Ready"),
            PgProbe::Responding(m) => panic!("black-holed listener reported Responding: {m}"),
            PgProbe::Unreachable(m) => {
                panic!(
                    "black-holed listener reported Unreachable ({m}) — this verdict can power-cycle a VM, and an accepted-but-slow connect is exactly what a loaded server looks like"
                )
            }
        }
    }

    /// The regression that motivated `Stalled`: a warm VM that accepts but is
    /// slow must stay in the map. Evicting it drops into a re-init that
    /// power-cycles the VM, killing whatever load made it slow in the first
    /// place.
    #[tokio::test]
    async fn slow_listener_is_not_evicted_from_the_warm_path() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                std::mem::forget(sock);
            }
        });

        // `entry_alive` keeps the entry for anything that isn't Unreachable.
        assert!(
            !matches!(probe_pg(&pool_at(port)).await, PgProbe::Unreachable(_)),
            "a slow-but-listening VM must not be classified Unreachable"
        );
    }

    /// Spin an in-process daemon stub for `GET /deployed-sandboxes/:id` that
    /// 404s the first `misses` calls and reports `running` after that.
    /// Returns the base URL to point a `Sandbox` at.
    async fn status_stub(misses: usize) -> String {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/deployed-sandboxes/{id}",
            axum::routing::get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < misses {
                        return (
                            axum::http::StatusCode::NOT_FOUND,
                            format!("Sandbox not found: {id}"),
                        )
                            .into_response();
                    }
                    axum::Json(serde_json::json!({
                        "id": id,
                        "status": "running",
                        "status_changed_at": "2026-08-19T00:00:00Z",
                    }))
                    .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        base
    }

    fn stub_sandbox(base: &str) -> Sandbox {
        Sandbox::connect(
            "sb-test".to_string(),
            HeyoClientOptions {
                base_url: Some(base.to_string()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// A sandbox id heyvmd has permanently forgotten (its restart dropped the
    /// in-flight create) must fail fast, not hold the caller — and its
    /// admission slot — for the whole ready timeout.
    #[tokio::test]
    async fn a_sustained_404_ends_the_ready_wait_well_inside_the_timeout() {
        let base = status_stub(usize::MAX).await;
        let sb = stub_sandbox(&base);
        let started = Instant::now();
        let err = wait_ready_within(
            &sb,
            Duration::from_secs(30),
            "pg-x",
            Duration::from_millis(200),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("unknown to heyvmd"),
            "the 404 grace, not the deadline, ended the wait: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "gave up after {:?} — should be ~the grace",
            started.elapsed()
        );
    }

    /// The moment after a 202 where the daemon hasn't published the record yet
    /// is normal and must not fail the bring-up.
    #[tokio::test]
    async fn a_brief_404_right_after_the_deploy_is_tolerated() {
        let base = status_stub(3).await;
        let sb = stub_sandbox(&base);
        wait_ready_within(&sb, Duration::from_secs(30), "pg-x", Duration::from_secs(5))
            .await
            .expect("a short 404 window must be ridden out, not treated as a dead id");
    }

    /// Spin an in-process daemon stub that records resize requests and answers
    /// with `status`, returning (base_url, received-request log).
    async fn resize_stub(
        status: axum::http::StatusCode,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        use axum::extract::Path as AxPath;
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
        let log = seen.clone();
        let app = axum::Router::new().route(
            "/deployed-sandboxes/{id}/resize",
            axum::routing::post(move |AxPath(id): AxPath<String>, req_body: String| {
                log.lock().unwrap().push((id, req_body));
                async move { (status, body) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        (base, seen)
    }

    #[tokio::test]
    async fn resize_disk_posts_the_daemon_wire_format() {
        let (base, seen) = resize_stub(axum::http::StatusCode::OK, "{}").await;
        resize_disk_at(&base, "sb-abc123", 8).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        // Exactly the SDK's ResizeDiskRequest shape, addressed to the right VM.
        assert_eq!(seen[0].0, "sb-abc123");
        assert_eq!(seen[0].1, r#"{"disk_size_gb":8}"#);
    }

    #[tokio::test]
    async fn resize_disk_surfaces_daemon_errors_with_body() {
        let (base, _) = resize_stub(
            axum::http::StatusCode::BAD_REQUEST,
            "insufficient host storage for 8 GiB workspace",
        )
        .await;
        let err = resize_disk_at(&base, "sb-x", 8).await.unwrap_err().to_string();
        assert!(err.contains("400"), "status surfaced: {err}");
        assert!(
            err.contains("insufficient host storage"),
            "daemon's own reason surfaced: {err}"
        );
    }

    /// A dedicated database's role must never be able to grow past the one
    /// database it was provisioned for, even from inside its own VM — that is
    /// the guest-side half of the guarantee `dedicated::Credentials::authorize`
    /// makes at the pooler. Pin the attribute set so a future edit can't
    /// quietly hand out CREATEDB or superuser.
    #[test]
    fn dedicated_role_ddl_is_unprivileged_and_quoted() {
        let cred = crate::dedicated::Credential {
            database: "acme".into(),
            role: "acme_app".into(),
            password: "hunter2hunter2".into(),
            created_at: 0,
        };
        let create = role_ddl(&cred, false);
        assert!(create.starts_with("CREATE ROLE \"acme_app\" WITH LOGIN "), "{create}");
        for attr in ["NOSUPERUSER", "NOCREATEDB", "NOCREATEROLE"] {
            assert!(create.contains(attr), "{attr} missing from: {create}");
        }
        assert!(create.contains("PASSWORD 'hunter2hunter2'"), "{create}");
        // An existing role is realigned rather than re-created, so a restore
        // into a fresh VM and a plain restart both converge on the same shape.
        assert!(role_ddl(&cred, true).starts_with("ALTER ROLE \"acme_app\" WITH LOGIN "));

        // Quoting: identifiers double their quotes, password literals double
        // theirs. `dedicated`'s validation rejects both shapes, so this is
        // defense in depth against a hand-edited credential file.
        let odd = crate::dedicated::Credential {
            database: "acme".into(),
            role: "we\"ird".into(),
            password: "it's-fine".into(),
            created_at: 0,
        };
        let ddl = role_ddl(&odd, false);
        assert!(ddl.contains(r#"ROLE "we""ird" WITH"#), "{ddl}");
        assert!(ddl.contains("PASSWORD 'it''s-fine'"), "{ddl}");
    }

    /// A dedicated database restored from the frozen or S3-dump tier must come
    /// back owned by its tenant role. `--no-owner --no-privileges` would land
    /// every table on the restoring superuser instead, so the tenant would
    /// reconnect to its own database and get "permission denied" on all of it —
    /// data that looks lost. Pin that the flags are dropped exactly for the
    /// dedicated case and kept for every other schema.
    #[test]
    fn restore_keeps_ownership_only_for_a_dedicated_database() {
        let shared = restore_job_body("postgres", "tenant1", "", "http://x/y", false);
        assert!(shared.contains("--no-owner --no-privileges"), "{shared}");

        let dedicated = restore_job_body("postgres", "beta", "", "http://x/y", true);
        assert!(!dedicated.contains("--no-owner"), "{dedicated}");
        assert!(!dedicated.contains("--no-privileges"), "{dedicated}");
        // Everything else about the invocation is unchanged — notably the
        // idempotent clean that lets an interrupted restore be retried.
        for expected in ["pg_restore -h 127.0.0.1 -U postgres --clean --if-exists", "-d beta"] {
            assert!(dedicated.contains(expected), "{expected} missing from: {dedicated}");
        }
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("acme"), "\"acme\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[tokio::test]
    async fn resize_disk_rejects_out_of_range_sizes_without_calling_out() {
        let (base, seen) = resize_stub(axum::http::StatusCode::OK, "{}").await;
        assert!(resize_disk_at(&base, "sb-x", 0).await.is_err());
        assert!(resize_disk_at(&base, "sb-x", 251).await.is_err());
        assert!(
            seen.lock().unwrap().is_empty(),
            "invalid sizes must be rejected before any daemon call"
        );
    }
}

/// `CREATE DATABASE` has no `IF NOT EXISTS`, so check the catalog first. The
/// schema name is client-supplied — it's already validated in main, and we
/// double-quote-escape it here as defense in depth (identifiers can't be bound
/// as parameters).
///
/// `owner` is set for a *dedicated* database ([`crate::dedicated`]): its login
/// role is created (or brought back in line) first and the database is created
/// owned by it, so the tenant's own credential can create schemas and tables
/// in it. Run on **every** bring-up rather than only at provisioning time,
/// because it has to be idempotent anyway and because a restore from the
/// frozen or archived tier materializes a *fresh* cluster — `pg_dump` of a
/// single database carries no roles, so without this the restored VM would
/// have the data but no role able to log into it.
async fn ensure_database(
    pool: &Pool,
    schema: &str,
    owner: Option<&crate::dedicated::Credential>,
) -> Result<()> {
    let client = pool.get().await.context("checkout for db bootstrap")?;
    if let Some(cred) = owner {
        ensure_role(&client, cred).await?;
    }
    let exists = client
        .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&schema])
        .await
        .context("checking pg_database")?
        .is_some();
    let quoted = quote_ident(schema);
    if !exists {
        let owned = match owner {
            Some(cred) => format!(" OWNER {}", quote_ident(&cred.role)),
            None => String::new(),
        };
        client
            .batch_execute(&format!("CREATE DATABASE {quoted}{owned}"))
            .await
            .with_context(|| format!("creating database {schema}"))?;
        info!("created database {schema}");
    } else if let Some(cred) = owner {
        // The database predates its credential (provisioned over an existing
        // schema VM) or was recreated by a restore under the bootstrap role.
        // Ownership is what gives the tenant role CREATE on the database and,
        // through `pg_database_owner`, on its `public` schema — so reconcile
        // it rather than leaving a database its own role cannot write to.
        let owned_by_role = client
            .query_opt(
                "SELECT 1 FROM pg_database d JOIN pg_roles r ON r.oid = d.datdba \
                 WHERE d.datname = $1 AND r.rolname = $2",
                &[&schema, &cred.role],
            )
            .await
            .context("checking database ownership")?
            .is_some();
        if !owned_by_role {
            client
                .batch_execute(&format!(
                    "ALTER DATABASE {quoted} OWNER TO {}",
                    quote_ident(&cred.role)
                ))
                .await
                .with_context(|| {
                    format!("giving role {} ownership of database {schema}", cred.role)
                })?;
            info!("database {schema}: owner set to {}", cred.role);
        }
    }
    Ok(())
}

/// Create (or re-align) the login role behind a dedicated database.
///
/// Deliberately unprivileged — `NOSUPERUSER NOCREATEDB NOCREATEROLE` — so the
/// credential can't grow past the one database it was provisioned for even
/// inside its own VM. Ownership of that database (granted by the caller) is
/// what gives it full control of its own data. Note that `NOSUPERUSER` also
/// keeps it away from `COPY ... FROM PROGRAM` and `pg_read_file`, which the
/// pooler's own bootstrap role uses and which would otherwise expose the
/// guest's presigned-URL restore scripts.
///
/// The password is re-applied on every bring-up so the guest role always
/// matches the record the pooler authenticates against — belt-and-braces,
/// since the pg-fc image's `pg_hba.conf` is `trust` and the pooler is the layer
/// that actually checks it.
async fn ensure_role(
    client: &deadpool_postgres::Object,
    cred: &crate::dedicated::Credential,
) -> Result<()> {
    let exists = client
        .query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&cred.role])
        .await
        .context("checking pg_roles")?
        .is_some();
    client
        .batch_execute(&role_ddl(cred, exists))
        .await
        .with_context(|| format!("provisioning role {}", cred.role))?;
    if !exists {
        info!(
            "created role {} for dedicated database {}",
            cred.role, cred.database
        );
    }
    Ok(())
}

/// The `CREATE`/`ALTER ROLE` statement behind a dedicated database. Split out
/// from [`ensure_role`] so the attribute set — the thing that decides how much
/// the credential can do inside its own VM — and the quoting are testable
/// without a live server.
fn role_ddl(cred: &crate::dedicated::Credential, exists: bool) -> String {
    let verb = if exists { "ALTER" } else { "CREATE" };
    // The password is validated to printable, space-free ASCII, so the only
    // character that can end the literal is a quote — double it, and rely on
    // standard_conforming_strings (on by default) for backslashes.
    let literal = cred.password.replace('\'', "''");
    format!(
        "{verb} ROLE {} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD '{literal}'",
        quote_ident(&cred.role)
    )
}

/// Double-quote a Postgres identifier, escaping any embedded quote. Identifiers
/// can't be bound as query parameters, so every interpolated name goes through
/// here.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
