//! Schema -> VM registry. One entry per schema, created once and reused.
//! A background reaper stops VMs that go idle (no connections) for too long.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use deadpool_postgres::Pool;
use heyo_sdk::{HeyoError, P2pTunnel, Sandbox};
use tokio::sync::{Mutex, OnceCell, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, info, warn};

use crate::config::{Config, PressureConfig};
use crate::dumpsrv::DumpServer;
use crate::reclaim::{POST_STOP_RECLAIM_DELAY, RECLAIM_FIRST_DELAY, Reclaimer};
use crate::spares::SparePool;
use crate::store::{Store, StoreRecord, Tier};
use crate::vm;
use crate::vm::RestoreSource;

/// Bound on the pre-stop CHECKPOINT the reaper issues before killing an idle
/// VM. An immediate checkpoint flushes at most shared_buffers of dirty pages
/// to virtio-SSD storage — seconds on any size class — so a longer wait means
/// something is wedged and the stop should proceed.
const PRE_STOP_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a supervised background loop (reaper, eviction sweep) logs an
/// info-level "still alive" heartbeat when nothing else is happening. Every pass
/// also logs at debug; this throttles the visible-at-info line so a healthy,
/// idle loop proves liveness roughly this often without spamming the log.
const SUPERVISOR_HEARTBEAT: Duration = Duration::from_secs(900);

/// Delay before the S3-eviction loop's first sweep after startup. Short enough
/// that redeploys don't starve eviction (see [`supervise`]), long enough to let
/// the pooler finish coming up first.
const ARCHIVE_FIRST_SWEEP_DELAY: Duration = Duration::from_secs(120);

/// Cap on freezes per sweep pass. Each freeze costs a VM bring-up (a cold
/// boot) + dump + kill — minutes — and the sweeps share one single-flight
/// lock, so an uncapped pass over a large idle backlog (first enable on an
/// existing fleet: hundreds of candidates) would monopolize eviction for
/// hours. Capped, the backlog drains a batch per sweep interval and the S3 /
/// pressure sweeps get windows in between; the pressure reaper remains the
/// urgent path if disk demands faster.
const FREEZE_MAX_PER_SWEEP: usize = 25;

/// Delay before the orphan-disk sweep's first pass after startup — long enough
/// to let the pooler finish coming up and reattach warm VMs (so their disks are
/// held open and never misread as orphans), short enough that frequent
/// redeploys can't starve the sweep (see [`supervise`]).
const ORPHAN_FIRST_DELAY: Duration = Duration::from_secs(180);

/// A disk directory must be at least this old (by mtime) to be an orphan
/// candidate. A VM mid-create/mid-boot has a brand-new `sb-<id>/` that the
/// daemon may not report yet (`get` 404s during provisioning) and whose disk
/// isn't open yet — deleting it would be catastrophic. A genuine orphan is
/// stale by definition (its schema was offloaded long ago), so an age floor
/// costs nothing and closes the create race outright.
const ORPHAN_MIN_AGE: Duration = Duration::from_secs(1800);

/// Cap on directory deletions per orphan sweep. Bounds the blast radius of any
/// misclassification to a batch, and keeps a first run over a large backlog
/// from doing hundreds of `remove_dir_all`s (and daemon round-trips) in one
/// pass; the rest drain over subsequent sweeps.
const ORPHAN_MAX_DELETES_PER_SWEEP: usize = 100;

/// Abort an orphan sweep after this many consecutive daemon errors while
/// checking sandbox liveness: a flaking/restarting heyvmd must never let a
/// "gone?" ambiguity turn into a deletion, so we stop and try again next sweep.
const ORPHAN_MAX_DAEMON_ERRORS: usize = 5;

/// Circuit breaker for one eviction sweep: after this many *consecutive*
/// archive failures the pass aborts instead of grinding on. Each failed archive
/// can cost a full ready-timeout (~5 min of a wedged bring-up), and a run of
/// them means the environment is sick — daemon flaking, host disk full,
/// Postgres unable to start — not that these particular schemas are odd.
/// Sweeping on multiplies a systemic outage by the candidate count; stopping
/// costs nothing, since every remaining candidate is retried next sweep.
const SWEEP_MAX_CONSECUTIVE_FAILURES: usize = 3;

/// A ready, warm VM for one schema. `target` is where client bytes are spliced
/// — either the VM's guest IP directly (same-host, no tunnel) or the local end
/// of an iroh tunnel. Holding `tunnel` (when present) keeps that forward alive;
/// holding `pool` keeps a bootstrap/health connection warm.
pub struct SchemaEntry {
    pub sandbox: Sandbox,
    /// Splice destination for this schema's Postgres.
    pub target: SocketAddr,
    /// Some in tunnel mode (kept alive for the entry's lifetime); None when
    /// dialing the guest IP directly.
    #[allow(dead_code)]
    pub tunnel: Option<P2pTunnel>,
    #[allow(dead_code)]
    pub pool: Pool,
    /// Exempt from idle reaping (a permanent keep-alive schema).
    pub keepalive: bool,
    /// Admission control for the VM's Postgres. The pooler splices client
    /// connections 1:1, so without a bound here the guest's `max_connections`
    /// is enforced by *Postgres*, as a `FATAL: sorry, too many clients
    /// already` on the (N+1)th client. That FATAL is what an application sees
    /// as a hard connection error mid-import, and the usual reaction — tear
    /// the pool down and retry — strands every transaction already in flight.
    ///
    /// Holding a permit for each spliced connection converts that rejection
    /// into a wait: over-eager clients queue at the pooler instead of being
    /// refused by the database. This bounds the guest; it does not multiplex
    /// (see `checkout`).
    slots: Arc<Semaphore>,
    /// What `slots` started with, for reporting (a `Semaphore` only exposes
    /// what's currently free).
    slot_limit: usize,
    /// Number of client connections currently spliced through this entry.
    active: AtomicUsize,
    /// Last time a connection started or ended. `active == 0` plus a stale
    /// `last_active` is what marks the VM idle. Refreshed at checkout so an
    /// entry handed out but not yet counted in `active` isn't reaped mid-race.
    last_active: StdMutex<Instant>,
}

impl SchemaEntry {
    pub fn new(
        sandbox: Sandbox,
        target: SocketAddr,
        tunnel: Option<P2pTunnel>,
        pool: Pool,
        keepalive: bool,
        slots: usize,
    ) -> Self {
        Self {
            sandbox,
            target,
            tunnel,
            pool,
            keepalive,
            slots: Arc::new(Semaphore::new(slots)),
            slot_limit: slots,
            active: AtomicUsize::new(0),
            last_active: StdMutex::new(Instant::now()),
        }
    }

    /// Free client slots right now (0 = the next client will queue).
    pub fn free_slots(&self) -> usize {
        self.slots.available_permits()
    }

    /// Total client slots this VM's Postgres was measured to allow.
    pub fn slot_limit(&self) -> usize {
        self.slot_limit
    }

    fn touch(&self) {
        *self.last_active.lock().unwrap() = Instant::now();
    }

    /// Live client connections currently spliced through this entry. Read-only
    /// view of the private `active` counter for the dashboard; the proxy path
    /// mutates it only through `ConnGuard`.
    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// How long since the last connect/disconnect on this entry.
    pub fn idle_for(&self) -> Duration {
        self.last_active.lock().unwrap().elapsed()
    }

    /// The sandbox id of the VM backing this entry.
    pub fn sandbox_id(&self) -> String {
        self.sandbox.sandbox_id().to_string()
    }

    /// True when reached over an iroh tunnel rather than a direct guest IP.
    pub fn is_tunneled(&self) -> bool {
        self.tunnel.is_some()
    }

    /// Idle = not keep-alive, no live connections, and quiet for `>= timeout`.
    fn is_idle(&self, timeout: Duration) -> bool {
        !self.keepalive
            && self.active.load(Ordering::SeqCst) == 0
            && self.last_active.lock().unwrap().elapsed() >= timeout
    }
}

/// RAII marker for one in-flight client connection. Bumps the entry's active
/// count for its lifetime and refreshes activity on both ends, so the reaper
/// never stops a VM with (or that just had) a live connection.
///
/// Also owns the entry's admission permit, so the guest's connection budget is
/// released on exactly the same event that ends the splice — including an
/// error or a panic on the proxy path. A permit leak here would silently
/// shrink the VM's usable connection count until a restart, so it must not be
/// released anywhere but `Drop`.
pub struct ConnGuard(Arc<SchemaEntry>, #[allow(dead_code)] OwnedSemaphorePermit);

impl ConnGuard {
    /// Take an admission permit, then mark the entry active. Waits up to
    /// `timeout` for a free slot; `None` means every slot is busy and the
    /// caller should fail this client rather than queue forever.
    async fn acquire(entry: Arc<SchemaEntry>, timeout: Duration) -> Option<Self> {
        let permit = match entry.slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                // Queueing is the whole point, but it is also the moment the
                // client stops getting what it asked for — say so. Silent
                // backpressure reads as "the pooler is slow"; this names it.
                let waited = Instant::now();
                warn!(
                    "all {} client slots busy on this VM; client is queueing \
                     (up to {timeout:?}) instead of being refused by Postgres",
                    entry.slot_limit()
                );
                let p = tokio::time::timeout(timeout, entry.slots.clone().acquire_owned())
                    .await
                    .ok()?
                    .ok()?;
                info!("client admitted after queueing {:?}", waited.elapsed());
                p
            }
        };
        entry.active.fetch_add(1, Ordering::SeqCst);
        entry.touch();
        Some(Self(entry, permit))
    }

    pub fn entry(&self) -> &SchemaEntry {
        &self.0
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

/// Bound on the dashboard's per-VM stat queries (DB and guest-OS stats) so a
/// wedged VM can't hang a detail-page render.
const STATS_TIMEOUT: Duration = Duration::from_secs(3);

/// Live database usage for a warm entry, read over its warm pool.
pub struct DbStats {
    pub db_size_bytes: i64,
    pub backends: i32,
}

/// Live guest-OS stats for a warm, pooler-managed VM, read over the same warm
/// PG pool as [`DbStats`] — never the guest console. `/proc` reads use
/// `pg_read_file` (needs superuser or `pg_read_server_files`; the default
/// `postgres` user qualifies); disk usage runs `df` as an ordinary fork under
/// the Postgres backend via `COPY FROM PROGRAM` (`pg_execute_server_program`).
/// Each piece degrades to `None` independently, so a locked-down role still
/// shows whatever it can.
pub struct GuestStats {
    /// Guest RAM (total, available) in bytes, from `/proc/meminfo`.
    pub mem: Option<(u64, u64)>,
    /// 1/5/15-minute load averages, from `/proc/loadavg`.
    pub load: Option<(f64, f64, f64)>,
    /// Filesystem holding the Postgres data directory: (total, used,
    /// available) bytes, from `df -kP` on `current_setting('data_directory')`.
    pub disk: Option<(u64, u64, u64)>,
}

/// A plain, owned point-in-time view of one warm schema entry — no `Sandbox`,
/// `Pool`, or lock handles — safe to hand to the dashboard's render layer.
pub struct EntrySnapshot {
    pub schema: String,
    pub sandbox_id: String,
    pub target: SocketAddr,
    pub active: usize,
    /// Client slots free / total on this VM's Postgres. `free == 0` means new
    /// clients are queueing at the pooler.
    pub free_slots: usize,
    pub slot_limit: usize,
    pub idle_secs: u64,
    pub keepalive: bool,
    pub tunneled: bool,
}

pub struct SchemaRegistry {
    cfg: Config,
    // Outer Mutex guards the map only; the per-schema OnceCell serializes the
    // (slow) first VM bring-up without blocking other schemas. A failed init
    // leaves the cell empty so the next client retries.
    entries: Mutex<HashMap<String, Arc<OnceCell<Arc<SchemaEntry>>>>>,
    // Persistent schema → sandbox-id map. Outlives entry eviction and process
    // restarts, so a reconnect after a stop/reap/restart reattaches to the same
    // VM (by id) rather than creating a duplicate with a fresh, empty data disk.
    store: Store,
    // Schemas whose VM is mid-archive (dump + kill in flight). A checkout for a
    // schema in this set waits until it clears, then cold-starts — which
    // restores from S3. Guards against a client bringing a VM back up while the
    // archiver is dumping and killing it. Held for the whole archive operation.
    archiving: StdMutex<HashSet<String>>,
    // True while an eviction sweep is running. Single-flights the sweep so the
    // periodic timer and a manual "sweep now" can't stack overlapping passes over
    // the same candidates.
    sweeping: AtomicBool,
    // True while an orphan-disk sweep is running. Its own single-flight (not
    // shared with `sweeping`) because it touches no VMs or checkouts — it only
    // deletes directories the daemon confirms gone — so it may run alongside an
    // eviction sweep; this just stops two orphan passes from racing.
    orphan_sweeping: AtomicBool,
    // Offline-trims stopped VMs' data disks (Firecracker has no discard
    // passthrough, so freed guest blocks never return to the host on their
    // own). `Some` when PG_VM_POOL_RECLAIM_CMD is configured.
    reclaimer: Option<Arc<Reclaimer>>,
    // Warm-spare pool: pre-booted empty VMs a cold bring-up claims instead of
    // paying create + boot + initdb. `Some` when PG_VM_POOL_WARM_SPARES > 0.
    spares: Option<Arc<SparePool>>,
    // Local dump store + token registry for the frozen tier. `Some` when
    // PG_VM_POOL_FREEZE_AFTER_SECS is configured.
    dumps: Option<Arc<DumpServer>>,
}

impl SchemaRegistry {
    pub fn new(cfg: Config) -> Self {
        let store = Store::load(cfg.state_file.clone());
        let reclaimer = cfg
            .reclaim
            .as_ref()
            .map(|r| Arc::new(Reclaimer::new(r.cmd.clone())));
        let spares = (cfg.warm_spares > 0).then(|| Arc::new(SparePool::new(cfg.warm_spares)));
        let dumps = cfg
            .freeze
            .as_ref()
            .map(|f| Arc::new(DumpServer::new(f.dump_dir.clone())));
        Self {
            cfg,
            entries: Mutex::new(HashMap::new()),
            store,
            archiving: StdMutex::new(HashSet::new()),
            sweeping: AtomicBool::new(false),
            orphan_sweeping: AtomicBool::new(false),
            reclaimer,
            spares,
            dumps,
        }
    }

    /// Sandbox ids currently bound to a schema — the exclusion set that keeps
    /// the spare pool from handing out a VM some schema already owns (a spare
    /// keeps its `spare-pg-*` name after being claimed, so the name alone
    /// can't tell).
    fn bound_ids(&self) -> HashSet<String> {
        self.store_records()
            .into_iter()
            .map(|(_, r)| r.sandbox_id)
            .collect()
    }

    /// Password clients must present before the pooler proxies them anywhere;
    /// `None` if `PG_VM_POOL_PASSWORD` is unset (no client auth gate).
    pub fn client_password(&self) -> Option<&str> {
        self.cfg.pg_password.as_deref()
    }

    /// The configured idle-reaping timeout (`None` when reaping is disabled), so
    /// a dashboard can label how close a warm VM is to being stopped.
    pub fn idle_timeout(&self) -> Option<Duration> {
        self.cfg.idle_timeout
    }

    /// Whether the S3 eviction tier is configured — gates the dashboard's manual
    /// "reap to S3" control.
    pub fn archive_enabled(&self) -> bool {
        self.cfg.archive.is_some()
    }

    /// Whether automatic disk reclamation is configured — gates the dashboard's
    /// manual "reclaim disk slack" control.
    pub fn reclaim_enabled(&self) -> bool {
        self.reclaimer.is_some()
    }

    /// Point-in-time view of every *warm* schema entry (VMs the pooler currently
    /// holds). Takes the same map lock the reaper/checkout use, but holds it only
    /// for a fast, await-free read — no meaningful contention. Stopped/reaped
    /// schemas aren't warm; pair with [`Self::store_records`] for those.
    pub async fn snapshot(&self) -> Vec<EntrySnapshot> {
        let map = self.entries.lock().await;
        map.iter()
            .filter_map(|(schema, cell)| {
                cell.get().map(|e| EntrySnapshot {
                    schema: schema.clone(),
                    sandbox_id: e.sandbox_id(),
                    target: e.target,
                    active: e.active_count(),
                    free_slots: e.free_slots(),
                    slot_limit: e.slot_limit(),
                    idle_secs: e.idle_for().as_secs(),
                    keepalive: e.keepalive,
                    tunneled: e.is_tunneled(),
                })
            })
            .collect()
    }

    /// The durable per-schema records the pooler has ever backed, surviving
    /// eviction and restarts — used to recover the schema name for a VM that's
    /// currently stopped (not warm) and to surface archived (killed) schemas
    /// that no longer appear in the daemon's inventory at all.
    pub fn store_records(&self) -> Vec<(String, StoreRecord)> {
        self.store.records()
    }

    /// Live database stats for a warm, pooler-managed VM, read over the pooler's
    /// own warm Postgres pool — the *same* safe TCP path the liveness probe uses,
    /// **not** a guest console exec, so it never disturbs the VM. `None` when the
    /// VM isn't warm or the query fails/times out.
    pub async fn db_stats(&self, sandbox_id: &str, schema: &str) -> Option<DbStats> {
        let entry = self.warm_entry(sandbox_id).await?;
        let query = async {
            let client = entry.pool.get().await.ok()?;
            let row = client
                .query_opt(
                    "SELECT pg_database_size(datname), numbackends \
                     FROM pg_stat_database WHERE datname = $1",
                    &[&schema],
                )
                .await
                .ok()??;
            Some(DbStats {
                db_size_bytes: row.get(0),
                backends: row.get(1),
            })
        };
        tokio::time::timeout(STATS_TIMEOUT, query).await.ok()?
    }

    /// Live guest-OS memory/load/disk for a warm, pooler-managed VM (see
    /// [`GuestStats`] for how each piece is read and degrades). `None` when
    /// the VM isn't warm or nothing could be read within [`STATS_TIMEOUT`].
    pub async fn guest_stats(&self, sandbox_id: &str) -> Option<GuestStats> {
        let entry = self.warm_entry(sandbox_id).await?;
        let query = async {
            let mut client = entry.pool.get().await.ok()?;
            // /proc reads: no fork, just the backend reading two pseudo-files.
            // The explicit (offset, length) form is required — /proc files
            // stat as 0 bytes, so the whole-file form reads nothing.
            let mem_load = client
                .query_opt(
                    "SELECT pg_read_file('/proc/meminfo', 0, 8192), \
                            pg_read_file('/proc/loadavg', 0, 256)",
                    &[],
                )
                .await
                .ok()
                .flatten();
            let (mem, load) = mem_load
                .map(|row| (parse_meminfo(row.get(0)), parse_loadavg(row.get(1))))
                .unwrap_or((None, None));
            let disk = df_data_dir(&mut client).await;
            (mem.is_some() || load.is_some() || disk.is_some()).then_some(GuestStats {
                mem,
                load,
                disk,
            })
        };
        tokio::time::timeout(STATS_TIMEOUT, query).await.ok()?
    }

    /// The warm entry backing `sandbox_id`, if any (brief map-lock read).
    async fn warm_entry(&self, sandbox_id: &str) -> Option<Arc<SchemaEntry>> {
        let map = self.entries.lock().await;
        map.values().find_map(|cell| {
            let e = cell.get()?;
            (e.sandbox_id() == sandbox_id).then(|| e.clone())
        })
    }

    /// Check out the entry for `schema`, bringing the VM up on first request.
    /// The returned guard keeps the VM off the reaper's radar until dropped.
    /// Concurrent callers for the same schema share one bring-up.
    pub async fn checkout(&self, schema: &str) -> Result<ConnGuard> {
        // Refresh durable activity up front so the S3 eviction sweep sees this
        // schema as recently used even long after its VM leaves the warm map
        // (the in-memory `SchemaEntry::last_active` doesn't survive that).
        self.store.touch(schema);
        loop {
            // If this schema is mid-archive (dump + kill in flight), don't race
            // the archiver by bringing the VM back up. Wait for it to clear; the
            // subsequent cold start restores from S3.
            if self.is_archiving(schema) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Warm path: claim the entry under the map lock, which the reaper
            // also takes — so it can't evict this entry between its idle-check
            // and our claim. `touch()` is what does the claiming: it makes the
            // entry non-idle for a full idle_timeout, which covers the window
            // between dropping the lock and `active` actually being
            // incremented. The permit is deliberately NOT taken here — it can
            // block for `admit_timeout`, and holding the map lock across that
            // would stall checkouts for every *other* schema behind this one.
            let (cell, warm) = {
                let mut map = self.entries.lock().await;
                let cell = map
                    .entry(schema.to_string())
                    .or_insert_with(|| Arc::new(OnceCell::new()))
                    .clone();
                let warm = cell.get().inspect(|e| e.touch()).cloned();
                (cell, warm)
            };

            if let Some(entry) = warm {
                let Some(guard) = ConnGuard::acquire(entry, self.cfg.admit_timeout).await else {
                    bail!(
                        "schema {schema}: all client connection slots busy after {:?}; \
                         the VM's Postgres is at its connection limit",
                        self.cfg.admit_timeout
                    );
                };
                // A VM stopped out-of-band (manual stop) or a tunnel dropped by a
                // network change leaves a cached entry whose local tunnel still
                // accepts but never reaches Postgres — splicing to it would hang.
                // Probe first; reuse only if it actually answers, else evict and
                // fall through to re-init (which restarts the VM).
                if self.entry_alive(guard.entry()).await {
                    return Ok(guard);
                }
                warn!("schema {schema}: cached VM unreachable, restarting it");
                drop(guard);
                self.evict(schema, &cell).await;
                continue;
            }

            // Cold path: bring the VM up without holding the map lock. Concurrent
            // first-connects share one bring-up via the OnceCell (one runs
            // ensure_vm, the rest await it here). Log entry/exit with timing so a
            // slow or stuck bring-up is visible — otherwise a client just parks
            // here silently until ready_timeout, which reads as a hang in the log.
            let started = Instant::now();
            info!("schema {schema}: cold start, bringing up VM (or awaiting in-progress bring-up)");
            // Reattach to the VM we last used for this schema (survives eviction
            // and process restarts), else find-or-create by name. If the schema
            // was archived to S3 (VM killed), bring up a fresh VM and restore the
            // dump into it before serving — the stored id is dead so we don't
            // reattach.
            let record = self.store.record(schema);
            let known_id = record.as_ref().map(|r| r.sandbox_id.clone());
            let restore = match record.as_ref().map(|r| r.tier) {
                Some(Tier::Archived) => match self.cfg.archive.as_ref() {
                    Some(a) => Some(RestoreSource::S3(a.s3.clone())),
                    None => bail!(
                        "schema {schema} is archived to S3, but the eviction tier is not \
                         configured (set PG_VM_POOL_ARCHIVE_AFTER_SECS + PG_VM_POOL_S3_*) — \
                         cannot restore it"
                    ),
                },
                Some(Tier::Frozen) => match (&self.dumps, &self.cfg.freeze) {
                    (Some(srv), Some(f)) => Some(RestoreSource::Local {
                        srv: srv.clone(),
                        port: f.listen.port(),
                    }),
                    _ => bail!(
                        "schema {schema} is frozen to a local dump, but the frozen tier is \
                         not configured (set PG_VM_POOL_FREEZE_AFTER_SECS) — cannot restore it"
                    ),
                },
                _ => None,
            };
            let bound = self.spares.as_ref().map(|_| self.bound_ids()).unwrap_or_default();
            match cell
                .get_or_try_init(|| {
                    vm::ensure_vm(
                        &self.cfg,
                        schema,
                        known_id.as_deref(),
                        restore.as_ref(),
                        self.spares.as_deref().map(|p| (p, &bound)),
                    )
                })
                .await
            {
                Ok(entry) => {
                    // Remember which VM now backs this schema so a later restart
                    // reattaches to it instead of creating a duplicate. `put`
                    // also clears any `archived` flag (this is a fresh VM id), so
                    // a just-restored schema is durably marked live again.
                    self.store.put(schema, entry.sandbox.sandbox_id());
                    info!("schema {schema}: VM ready in {:?}", started.elapsed());
                    let Some(guard) =
                        ConnGuard::acquire(entry.clone(), self.cfg.admit_timeout).await
                    else {
                        bail!(
                            "schema {schema}: all client connection slots busy after {:?}; \
                             the VM's Postgres is at its connection limit",
                            self.cfg.admit_timeout
                        );
                    };
                    return Ok(guard);
                }
                Err(e) => {
                    warn!(
                        "schema {schema}: bring-up failed after {:?}: {e:#}",
                        started.elapsed()
                    );
                    return Err(e);
                }
            }
        }
    }

    /// Remove `cell` from the map iff it's still the current cell for `schema`,
    /// so a concurrent re-init that already installed a fresh cell isn't lost.
    async fn evict(&self, schema: &str, cell: &Arc<OnceCell<Arc<SchemaEntry>>>) {
        let mut map = self.entries.lock().await;
        if matches!(map.get(schema), Some(cur) if Arc::ptr_eq(cur, cell)) {
            map.remove(schema);
        }
    }

    /// Liveness probe on the warm path: is anything still listening for this
    /// entry? Catches a VM stopped out-of-band and a dead tunnel forward.
    /// Cheap on a healthy VM (a local round-trip), so it's safe per checkout.
    ///
    /// Only a *refusal* counts as dead. This used to require a successful
    /// `SELECT 1` within 3s and treat everything else — a slow answer, a
    /// server error, connection saturation — as a dead VM, which then evicted
    /// the entry and dropped into a re-init that power-cycles it. Every one of
    /// those is survivable on its own; the reboot isn't, since the pooler stops
    /// VMs with an unclean kill and takes any in-flight ingest with it. A
    /// stalled probe in particular is the *expected* reading of a VM under a
    /// heavy load, so the old check reliably killed VMs for being busy.
    async fn entry_alive(&self, entry: &SchemaEntry) -> bool {
        !matches!(
            crate::vm::probe_pg(&entry.pool).await,
            crate::vm::PgProbe::Unreachable(_)
        )
    }

    /// Spawn the background idle-reaper if an idle timeout is configured.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let Some(timeout) = self.cfg.idle_timeout else {
            info!("idle reaping disabled (PG_VM_POOL_IDLE_TIMEOUT_SECS=0)");
            return;
        };
        info!("idle reaper: stopping VMs after {timeout:?} without connections");
        let registry = self.clone();
        // Check a few times per timeout window so shutdown lands close to the
        // deadline, but not so often it busies the daemon.
        let tick = (timeout / 4).max(Duration::from_secs(5));
        // Reaper `tick` is already short, so first pass and steady state match.
        tokio::spawn(supervise("idle-reaper", tick, tick, move || {
            let registry = registry.clone();
            async move { registry.reap_idle(timeout).await }
        }));
    }

    /// Evict and stop every idle VM. Eviction (removing the map cell) happens
    /// under the lock so a concurrent `checkout` either sees the entry before
    /// eviction (and bumps `active`, sparing it) or misses it and brings up a
    /// fresh VM. The actual stop happens after the lock is released.
    ///
    /// Returns how many VMs were stopped, for the supervisor's heartbeat.
    async fn reap_idle(&self, timeout: Duration) -> usize {
        let mut victims: Vec<(String, Arc<SchemaEntry>)> = Vec::new();
        {
            let mut map = self.entries.lock().await;
            map.retain(|schema, cell| match cell.get() {
                Some(entry) if entry.is_idle(timeout) => {
                    victims.push((schema.clone(), entry.clone()));
                    false
                }
                _ => true,
            });
        }
        let stopped = victims.len();
        for (schema, entry) in victims {
            info!("idle-stopping VM for schema {schema} (no connections for >= {timeout:?})");
            // Checkpoint before the kill. `sandbox.stop()` is an unclean
            // power-off (Postgres never sees a shutdown signal), and the VMs
            // run synchronous_commit=off — so without this, up to the last
            // ~600ms of acked commits ride on luck and every restart replays
            // WAL back to the previous checkpoint. One CHECKPOINT over the
            // warm pool flushes everything acked and empties the replay
            // queue, making the next boot's recovery a no-op. Best-effort:
            // if it fails or times out we stop anyway — crash recovery
            // handles it, that's the design.
            let checkpoint = async {
                let client = entry.pool.get().await?;
                client.batch_execute("CHECKPOINT").await?;
                Ok::<(), anyhow::Error>(())
            };
            match tokio::time::timeout(PRE_STOP_CHECKPOINT_TIMEOUT, checkpoint).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("pre-stop CHECKPOINT for schema {schema} failed: {e:#}"),
                Err(_) => warn!(
                    "pre-stop CHECKPOINT for schema {schema} timed out after {PRE_STOP_CHECKPOINT_TIMEOUT:?}"
                ),
            }
            if let Err(e) = entry.sandbox.stop().await {
                warn!("failed to stop idle VM for schema {schema}: {e:#}");
            }
            // Dropping the last Arc here tears down the tunnel + pool. Data on
            // the VM's /dev/vdb persists; a later connect restarts the VM.
        }
        // The disks just released are prime reclaim candidates — without a trim
        // each keeps its full high-water allocation on the host. Trigger a run
        // once the Firecracker processes have fully exited (the script skips
        // any disk still held open, so an early fire is safe, just less useful).
        if stopped > 0
            && let Some(reclaimer) = &self.reclaimer
        {
            reclaimer.spawn_soon(POST_STOP_RECLAIM_DELAY);
        }
        stopped
    }

    // ---- disk-slack reclamation ---------------------------------------------

    /// Spawn the periodic disk-reclaim loop if `PG_VM_POOL_RECLAIM_CMD` is
    /// configured. Complements the post-reap trigger in [`Self::reap_idle`]:
    /// that one returns a just-stopped VM's slack promptly; this one is the
    /// backstop for VMs stopped out-of-band (dashboard, heyvm CLI, crashes).
    pub fn spawn_reclaimer(self: &Arc<Self>) {
        let Some(rc) = self.cfg.reclaim.clone() else {
            info!("automatic disk reclaim disabled (PG_VM_POOL_RECLAIM_CMD unset)");
            return;
        };
        // `reclaimer` is always Some when the config is.
        let Some(reclaimer) = self.reclaimer.clone() else {
            return;
        };
        info!(
            "disk reclaim: running `{}` every {:?} (and after idle reaps)",
            rc.cmd, rc.interval
        );
        let first = RECLAIM_FIRST_DELAY.min(rc.interval);
        tokio::spawn(supervise("disk-reclaim", first, rc.interval, move || {
            let reclaimer = reclaimer.clone();
            async move { reclaimer.run_once().await }
        }));
    }

    /// Kick off one disk-reclaim run now, in the background — the dashboard's
    /// "reclaim disk slack" control. Errors if reclamation isn't configured or
    /// a run is already in progress.
    pub fn spawn_reclaim_now(&self) -> Result<()> {
        let Some(reclaimer) = &self.reclaimer else {
            bail!("automatic disk reclaim is not configured (set PG_VM_POOL_RECLAIM_CMD)");
        };
        reclaimer.spawn_now()
    }

    // ---- S3 eviction tier ---------------------------------------------------

    /// Spawn the background archive sweep if the S3 eviction tier is configured.
    /// This is the slow, disk-reclaiming counterpart to the idle reaper: it
    /// offloads a long-idle schema's data to S3 and *kills* the VM (freeing its
    /// data disk), where the reaper only stops it.
    pub fn spawn_archiver(self: &Arc<Self>) {
        let Some(archive) = self.cfg.archive.clone() else {
            info!("S3 eviction disabled (PG_VM_POOL_ARCHIVE_AFTER_SECS unset/0)");
            return;
        };
        info!(
            "S3 eviction: offloading VMs idle >= {:?} to s3://{}/{} (sweep every {:?})",
            archive.archive_after, archive.s3.bucket, archive.s3.prefix, archive.sweep_interval
        );
        let registry = self.clone();
        let (interval, after) = (archive.sweep_interval, archive.archive_after);
        // Run the first sweep soon after startup — never a full `interval` later —
        // so frequent redeploys can't indefinitely postpone eviction. Capped by
        // the interval itself in case someone configures a very short sweep.
        let first = ARCHIVE_FIRST_SWEEP_DELAY.min(interval);
        tokio::spawn(supervise("s3-eviction", first, interval, move || {
            let registry = registry.clone();
            async move { registry.sweep_archive(after).await }
        }));
    }

    /// Spawn the warm-spare replenisher if `PG_VM_POOL_WARM_SPARES` > 0: keeps
    /// the pool of pre-booted claimable VMs topped up (see `spares`).
    pub fn spawn_spare_replenisher(self: &Arc<Self>) {
        let Some(pool) = self.spares.clone() else {
            info!("warm-spare pool disabled (PG_VM_POOL_WARM_SPARES unset/0)");
            return;
        };
        info!(
            "warm-spare pool: keeping {} pre-booted VM(s) ready for claiming",
            self.cfg.warm_spares.min(crate::spares::MAX_SPARES)
        );
        let registry = self.clone();
        tokio::spawn(supervise(
            "warm-spares",
            Duration::from_secs(15),
            Duration::from_secs(60),
            move || {
                let registry = registry.clone();
                let pool = pool.clone();
                async move {
                    let bound = registry.bound_ids();
                    pool.replenish(&registry.cfg, &bound).await
                }
            },
        ));
    }

    /// Spawn the disk-pressure watchdog if `PG_VM_POOL_PRESSURE_PATH` is
    /// configured: when the VM-disk filesystem crosses the high-water mark,
    /// emergency-archive the oldest-idle schemas — TTL ignored — until it
    /// drops below the low-water mark. The backstop against the disk-full
    /// outage where VM creates, Postgres, and the dumps themselves all fail.
    pub fn spawn_pressure_reaper(self: &Arc<Self>) {
        let Some(pressure) = self.cfg.archive.as_ref().and_then(|a| a.pressure.clone()) else {
            info!("disk-pressure eviction disabled (PG_VM_POOL_PRESSURE_PATH unset)");
            return;
        };
        info!(
            "disk-pressure eviction: watching {} — archiving oldest-idle schemas at \
             >= {:.0}% full until < {:.0}% (checked every {:?}; TTL is overridden \
             under pressure)",
            pressure.path.display(),
            pressure.high_pct,
            pressure.low_pct,
            pressure.check_interval
        );
        let registry = self.clone();
        let tick = pressure.check_interval;
        tokio::spawn(supervise("disk-pressure", tick, tick, move || {
            let registry = registry.clone();
            let pressure = pressure.clone();
            async move { registry.pressure_pass(&pressure).await }
        }));
    }

    /// One pressure check: no-op below the high-water mark; above it, archive
    /// oldest-idle schemas one at a time, re-reading usage after each, until
    /// below the low-water mark or out of candidates. Claims the same
    /// single-flight flag as the periodic sweep, so the two never interleave
    /// over the same schemas. Returns how many schemas were archived.
    async fn pressure_pass(&self, p: &PressureConfig) -> usize {
        let Some(pct) = disk_used_pct(&p.path).await else {
            warn!(
                "disk-pressure: could not read filesystem usage of {}; skipping this check",
                p.path.display()
            );
            return 0;
        };
        if pct < p.high_pct {
            debug!("disk-pressure: {} at {pct:.1}% (< {:.1}%), ok", p.path.display(), p.high_pct);
            return 0;
        }
        if self.sweeping.swap(true, Ordering::SeqCst) {
            info!(
                "disk-pressure: {} at {pct:.1}% but an eviction sweep is already running; \
                 will re-check in {:?}",
                p.path.display(),
                p.check_interval
            );
            return 0;
        }
        let _sweeping = SweepGuard(&self.sweeping);
        warn!(
            "disk-pressure: {} is {pct:.1}% full (>= {:.1}%) — emergency-archiving \
             oldest-idle schemas until < {:.1}%",
            p.path.display(),
            p.high_pct,
            p.low_pct
        );

        // Live view for skipping busy schemas without paying a bring-up.
        let active: HashMap<String, usize> = self
            .snapshot()
            .await
            .into_iter()
            .map(|e| (e.schema, e.active))
            .collect();
        // Oldest last-active first. The TTL is deliberately not consulted:
        // under pressure, "least recently used" is the whole policy.
        let mut candidates: Vec<(String, u64)> = self
            .store_records()
            .into_iter()
            .filter(|(schema, rec)| rec.tier == Tier::Live && !self.cfg.is_keepalive(schema))
            .map(|(schema, rec)| (schema, rec.last_active))
            .collect();
        candidates.sort_by_key(|(_, last_active)| *last_active);

        let now = now_unix();
        let mut archived = 0usize;
        let mut consecutive_failures = 0usize;
        for (schema, last_active) in candidates {
            match disk_used_pct(&p.path).await {
                Some(cur) if cur < p.low_pct => {
                    info!(
                        "disk-pressure: {} down to {cur:.1}% (< {:.1}%) after {archived} \
                         emergency archive(s); standing down",
                        p.path.display(),
                        p.low_pct
                    );
                    return archived;
                }
                _ => {}
            }
            if active.get(&schema).copied().unwrap_or(0) > 0 {
                continue; // live sessions — archive_schema would refuse anyway
            }
            let idle_hours = now.saturating_sub(last_active) / 3600;
            info!(
                "disk-pressure: emergency-archiving schema {schema} (idle ~{idle_hours}h, \
                 TTL overridden)"
            );
            match self.archive_schema(&schema).await {
                Ok(()) => {
                    archived += 1;
                    consecutive_failures = 0;
                }
                Err(e) => {
                    warn!("disk-pressure: archiving schema {schema} failed: {e:#}");
                    consecutive_failures += 1;
                    if consecutive_failures >= SWEEP_MAX_CONSECUTIVE_FAILURES {
                        error!(
                            "disk-pressure: aborting after {consecutive_failures} consecutive \
                             failures — environment unhealthy; re-checking in {:?}",
                            p.check_interval
                        );
                        return archived;
                    }
                }
            }
        }
        if let Some(cur) = disk_used_pct(&p.path).await
            && cur >= p.low_pct
        {
            error!(
                "disk-pressure: exhausted every candidate schema with {} still at {cur:.1}% — \
                 the remaining usage is running/keepalive VMs or non-VM data; eviction alone \
                 cannot relieve this",
                p.path.display()
            );
        }
        archived
    }

    /// Kick off one eviction sweep now, in the background, instead of waiting for
    /// the periodic timer — the dashboard's "sweep now" control. Returns as soon
    /// as the sweep is launched (it can take a long time for a big backlog); the
    /// outcome shows up in the pooler log and the VMs' "Archived (S3)" status.
    /// Errors if the eviction tier isn't configured, or if a sweep is already
    /// running (the sweep itself is single-flighted, so this only reports it).
    pub fn spawn_sweep_now(self: &Arc<Self>) -> Result<()> {
        let Some(archive) = self.cfg.archive.clone() else {
            bail!(
                "S3 eviction tier is not configured (set PG_VM_POOL_ARCHIVE_AFTER_SECS + PG_VM_POOL_S3_*)"
            );
        };
        if self.sweeping.load(Ordering::SeqCst) {
            bail!("an eviction sweep is already running");
        }
        let registry = self.clone();
        let after = archive.archive_after;
        tokio::spawn(async move {
            let n = registry.sweep_archive(after).await;
            info!("manual eviction sweep finished: archived {n} schema(s)");
        });
        Ok(())
    }

    /// One eviction pass: archive every non-keepalive schema untouched for at
    /// least `threshold`, skipping any that is currently warm-and-busy.
    ///
    /// Returns how many schemas were successfully archived this pass, for the
    /// supervisor's heartbeat.
    ///
    /// Single-flighted: if a sweep (periodic or manual) is already in progress
    /// this returns immediately having done nothing, so triggers can't stack
    /// overlapping passes racing over the same candidates.
    async fn sweep_archive(&self, threshold: Duration) -> usize {
        if self.sweeping.swap(true, Ordering::SeqCst) {
            info!("S3 eviction: a sweep is already running; skipping this one");
            return 0;
        }
        let _sweeping = SweepGuard(&self.sweeping);

        let now = now_unix();
        let threshold_secs = threshold.as_secs();

        // Live cross-check: a schema warm with active connections, or one whose
        // in-memory idle clock is younger than the threshold, is not really cold
        // even if its durable `last_active` drifted stale (one long-lived
        // connection with no new checkouts). Refresh those and skip them.
        let live: HashMap<String, (usize, u64)> = self
            .snapshot()
            .await
            .into_iter()
            .map(|e| (e.schema, (e.active, e.idle_secs)))
            .collect();

        let mut candidates: Vec<String> = Vec::new();
        // Frozen schemas past the archive threshold get promoted local-file →
        // S3 without any VM (the dump already exists on the host).
        let mut frozen_candidates: Vec<String> = Vec::new();
        let mut total = 0usize;
        let (mut refreshed, mut keepalive, mut already, mut not_idle) = (0usize, 0usize, 0usize, 0usize);
        for (schema, rec) in self.store_records() {
            total += 1;
            let ka = self.cfg.is_keepalive(&schema);
            if rec.tier == Tier::Frozen {
                if !ka && now.saturating_sub(rec.last_active) >= threshold_secs {
                    frozen_candidates.push(schema);
                } else {
                    not_idle += 1;
                }
                continue;
            }
            match classify_candidate(&rec, ka, now, threshold_secs, live.get(&schema).copied()) {
                SweepAction::Skip => {
                    // classify_candidate skips for exactly these reasons; tally
                    // them so a sweep that archives nothing still says why.
                    if rec.offloaded() {
                        already += 1;
                    } else if ka {
                        keepalive += 1;
                    } else {
                        not_idle += 1;
                    }
                }
                // Warm-and-busy but durably stale: keep its clock honest so it
                // isn't re-flagged every sweep.
                SweepAction::Refresh => {
                    refreshed += 1;
                    self.store.touch(&schema);
                }
                SweepAction::Archive => candidates.push(schema),
            }
        }

        // Always log the evaluation, so a sweep that archives nothing is
        // explained ("all skipped as not-idle") rather than silent — the manual
        // "sweep now" button and the periodic pass both surface here.
        info!(
            "S3 eviction sweep: evaluated {total} schema(s) — {} live candidate(s) + \
             {} frozen promotion(s), {refreshed} refreshed (warm), skipped {} \
             ({keepalive} keepalive, {already} already archived, {not_idle} idle < \
             {threshold_secs}s)",
            candidates.len(),
            frozen_candidates.len(),
            keepalive + already + not_idle,
        );

        // Promote frozen dumps first: cheap (a file upload, no VM), and every
        // success frees local disk.
        let mut archived_frozen = 0usize;
        for schema in frozen_candidates {
            match self.archive_frozen_schema(&schema).await {
                Ok(()) => archived_frozen += 1,
                Err(e) => warn!(
                    "promoting frozen schema {schema} to S3 failed (will retry next sweep): {e:#}"
                ),
            }
        }

        if candidates.is_empty() {
            return archived_frozen;
        }
        let mut archived = archived_frozen;
        let mut consecutive_failures = 0usize;
        for schema in candidates {
            match self.archive_schema(&schema).await {
                Ok(()) => {
                    archived += 1;
                    consecutive_failures = 0;
                }
                Err(e) => {
                    warn!("archiving schema {schema} to S3 failed (will retry next sweep): {e:#}");
                    consecutive_failures += 1;
                    if consecutive_failures >= SWEEP_MAX_CONSECUTIVE_FAILURES {
                        error!(
                            "S3 eviction sweep: aborting after {consecutive_failures} consecutive \
                             archive failures — the environment looks unhealthy (daemon, host disk, \
                             or S3), and each failure burns minutes of wedged bring-up; remaining \
                             candidates will be retried next sweep"
                        );
                        break;
                    }
                }
            }
        }
        archived
    }

    /// Offload one schema's database to S3 and kill its VM to reclaim the disk.
    /// Also the target of the dashboard's manual "reap" button. Refuses if the
    /// VM has live client sessions. Serializes against [`Self::checkout`] via the
    /// `archiving` set so a client can't bring the VM back up mid-operation.
    pub async fn archive_schema(&self, schema: &str) -> Result<()> {
        let Some(archive) = self.cfg.archive.clone() else {
            bail!("S3 eviction tier is not configured (set PG_VM_POOL_ARCHIVE_AFTER_SECS + PG_VM_POOL_S3_*)");
        };
        match self.store.record(schema).map(|r| r.tier) {
            Some(Tier::Archived) => bail!("schema {schema} is already archived to S3"),
            // Frozen: no VM to dump — promote the existing local dump file.
            Some(Tier::Frozen) => return self.archive_frozen_schema(schema).await,
            _ => {}
        }

        // Claim the archiving slot; a Drop guard clears it on every exit path so
        // checkouts stuck waiting are released even on error/panic.
        let _guard = match ArchivingGuard::claim(&self.archiving, schema) {
            Some(g) => g,
            None => bail!("schema {schema} is already being archived"),
        };

        // Evict the warm entry under the map lock, refusing if it has live
        // sessions. Because the `archiving` set was inserted *before* this lock,
        // a checkout that slips in either grabbed the entry first (active > 0 →
        // we refuse) or will see the set and wait.
        {
            let mut map = self.entries.lock().await;
            if let Some(entry) = map.get(schema).and_then(|c| c.get()) {
                let active = entry.active_count();
                if active > 0 {
                    bail!("schema {schema} has {active} live session(s); refusing to archive");
                }
            }
            map.remove(schema);
        }

        // Bring the VM up ready for pg_dump (starts it if idle-stopped, reattaches
        // by id if it's still the same VM), dump to S3, then mark archived and
        // kill. Mark before kill: if we crash between them the data is safely in
        // S3 and the store says "archived", so the next connect restores — the
        // reverse order could lose the mapping to a killed VM.
        let known_id = self.store.record(schema).map(|r| r.sandbox_id);
        // No spare pool here: this bring-up exists to dump an *existing* VM's
        // data — a fresh spare would have nothing to dump.
        let entry = vm::ensure_vm(&self.cfg, schema, known_id.as_deref(), None, None)
            .await
            .with_context(|| format!("bringing up VM for schema {schema} to archive it"))?;

        vm::dump_to_s3(&self.cfg, &entry.sandbox, schema, &archive.s3)
            .await
            .with_context(|| format!("dumping schema {schema} to S3"))?;

        self.store.set_tier(schema, Tier::Archived).await;
        let accomplished = format!("dumped to s3://{}/{}", archive.s3.bucket, archive.s3.object_key(schema));

        // Kill and confirm the disk directory is actually gone — a kill the
        // daemon acks but doesn't act on strands the disk (see kill_and_reclaim).
        // The dump is safe in S3 and the store is marked archived, so a kill
        // *failure* only orphans a (stopped) VM + disk — undesirable, not data loss.
        if let Err(e) = vm::kill_and_reclaim(&self.cfg, &entry.sandbox, schema, &accomplished).await {
            warn!("schema {schema}: archived to S3 but killing the VM failed (orphaned): {e:#}");
        }
        // Dropping `entry` tears down its pool/tunnel.
        Ok(())
    }

    /// Promote a frozen schema's local dump file to S3 — no VM involved: the
    /// dump already exists on the host, so this is a pooler-side upload,
    /// verified with a HEAD, then tier flip and local-file cleanup.
    async fn archive_frozen_schema(&self, schema: &str) -> Result<()> {
        let Some(archive) = self.cfg.archive.clone() else {
            bail!("S3 eviction tier is not configured (set PG_VM_POOL_ARCHIVE_AFTER_SECS + PG_VM_POOL_S3_*)");
        };
        let Some(dumps) = self.dumps.clone() else {
            bail!("schema {schema} is frozen but the frozen tier is not configured");
        };
        let _guard = match ArchivingGuard::claim(&self.archiving, schema) {
            Some(g) => g,
            None => bail!("schema {schema} is already being archived"),
        };
        anyhow::ensure!(
            self.store.record(schema).map(|r| r.tier) == Some(Tier::Frozen),
            "schema {schema} is no longer frozen; not promoting"
        );

        let path = dumps.dump_path(schema);
        let meta = std::fs::metadata(&path)
            .with_context(|| format!("reading local dump {}", path.display()))?;
        let len = meta.len();
        anyhow::ensure!(
            len >= 512,
            "local dump {} is only {len} bytes — refusing to promote a failed dump",
            path.display()
        );
        // The whole object rides through memory (the SDK's reqwest has no
        // streaming-body feature enabled). Bound it; an oversized dump simply
        // stays frozen locally, which is safe.
        const MAX_POOLER_UPLOAD: u64 = 512 * 1024 * 1024;
        anyhow::ensure!(
            len <= MAX_POOLER_UPLOAD,
            "local dump {} is {len} bytes (> {MAX_POOLER_UPLOAD}); leaving it frozen \
             locally rather than buffering it through the pooler",
            path.display()
        );
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;

        let key = archive.s3.object_key(schema);
        let http = reqwest::Client::builder()
            .build()
            .context("building HTTP client for S3 upload")?;
        // HEAD first so a wrong-region bucket is discovered (and latched)
        // before the PUT is presigned.
        let _ = archive.s3.head_object(&http, &key, Duration::from_secs(10)).await;
        archive
            .s3
            .put_object(&http, &key, bytes, Duration::from_secs(600))
            .await?;
        // Verify: the object must exist with exactly the file's size.
        match archive.s3.head_object(&http, &key, Duration::from_secs(10)).await {
            Ok(Some(id)) if id.content_length == len => {}
            Ok(Some(id)) => bail!(
                "uploaded s3://{}/{key} reports {} bytes but the local dump is {len} — \
                 refusing to trust it",
                archive.s3.bucket,
                id.content_length
            ),
            Ok(None) => bail!("uploaded s3://{}/{key} but a HEAD finds nothing", archive.s3.bucket),
            Err(e) => return Err(e.context("verifying the uploaded archive")),
        }

        self.store.set_tier(schema, Tier::Archived).await;
        if let Err(e) = tokio::fs::remove_file(&path).await {
            warn!("schema {schema}: promoted to S3 but deleting {} failed: {e}", path.display());
        }
        info!(
            "schema {schema}: frozen dump promoted to s3://{}/{key} ({len} bytes), local file removed",
            archive.s3.bucket
        );
        Ok(())
    }

    // ---- local frozen tier --------------------------------------------------

    /// Start the local dump HTTP server (frozen tier). Serves guest uploads
    /// and downloads; without it, freezing and thawing are inert.
    pub fn spawn_dump_server(self: &Arc<Self>) {
        let (Some(srv), Some(freeze)) = (self.dumps.clone(), self.cfg.freeze.clone()) else {
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = srv.serve(freeze.listen).await {
                error!("local dump server exited: {e:#} — freezing and thawing are down");
            }
        });
    }

    /// Spawn the freeze sweep if `PG_VM_POOL_FREEZE_AFTER_SECS` is configured:
    /// dump long-idle schemas to local files and delete their VMs, shrinking a
    /// cold schema's footprint from a filesystem image to dump-file bytes.
    pub fn spawn_freezer(self: &Arc<Self>) {
        let Some(freeze) = self.cfg.freeze.clone() else {
            info!("local freeze tier disabled (PG_VM_POOL_FREEZE_AFTER_SECS unset/0)");
            return;
        };
        info!(
            "local freeze tier: dumping schemas idle >= {:?} to {} and deleting their \
             VMs (sweep every {:?})",
            freeze.freeze_after,
            freeze.dump_dir.display(),
            freeze.sweep_interval
        );
        let registry = self.clone();
        let (interval, after) = (freeze.sweep_interval, freeze.freeze_after);
        let first = ARCHIVE_FIRST_SWEEP_DELAY.min(interval);
        tokio::spawn(supervise("local-freeze", first, interval, move || {
            let registry = registry.clone();
            async move { registry.sweep_freeze(after).await }
        }));
    }

    /// One freeze pass: freeze every live, non-keepalive schema untouched for
    /// at least `threshold`. Shares the sweep single-flight with the S3 sweep
    /// and the pressure reaper, so the passes never interleave.
    async fn sweep_freeze(&self, threshold: Duration) -> usize {
        if self.sweeping.swap(true, Ordering::SeqCst) {
            info!("local freeze: another sweep is running; skipping this pass");
            return 0;
        }
        let _sweeping = SweepGuard(&self.sweeping);

        let now = now_unix();
        let threshold_secs = threshold.as_secs();
        let live: HashMap<String, (usize, u64)> = self
            .snapshot()
            .await
            .into_iter()
            .map(|e| (e.schema, (e.active, e.idle_secs)))
            .collect();

        let mut candidates: Vec<String> = Vec::new();
        for (schema, rec) in self.store_records() {
            let ka = self.cfg.is_keepalive(&schema);
            if classify_candidate(&rec, ka, now, threshold_secs, live.get(&schema).copied())
                == SweepAction::Archive
            {
                candidates.push(schema);
            }
        }
        if candidates.is_empty() {
            return 0;
        }
        let backlog = candidates.len().saturating_sub(FREEZE_MAX_PER_SWEEP);
        candidates.truncate(FREEZE_MAX_PER_SWEEP);
        info!(
            "local freeze sweep: {} candidate(s) this pass{}",
            candidates.len(),
            if backlog > 0 {
                format!(" ({backlog} more deferred to later sweeps)")
            } else {
                String::new()
            }
        );
        let mut frozen = 0usize;
        let mut consecutive_failures = 0usize;
        for schema in candidates {
            match self.freeze_schema(&schema).await {
                Ok(()) => {
                    frozen += 1;
                    consecutive_failures = 0;
                }
                Err(e) => {
                    warn!("freezing schema {schema} failed (will retry next sweep): {e:#}");
                    consecutive_failures += 1;
                    if consecutive_failures >= SWEEP_MAX_CONSECUTIVE_FAILURES {
                        error!(
                            "local freeze sweep: aborting after {consecutive_failures} \
                             consecutive failures — environment looks unhealthy; remaining \
                             candidates will be retried next sweep"
                        );
                        break;
                    }
                }
            }
        }
        frozen
    }

    /// Dump one schema to the local dump store and delete its VM. The frozen
    /// twin of [`Self::archive_schema`], with the same guards: `archiving`
    /// claim (checkouts wait), live-session refusal, dump verified complete
    /// (size-checked by `dump_to_local`) *before* the durable tier flip, and
    /// the tier flip durable *before* the kill.
    pub async fn freeze_schema(&self, schema: &str) -> Result<()> {
        let (Some(freeze), Some(dumps)) = (self.cfg.freeze.clone(), self.dumps.clone()) else {
            bail!("local freeze tier is not configured (set PG_VM_POOL_FREEZE_AFTER_SECS)");
        };
        if self.store.record(schema).map(|r| r.offloaded()).unwrap_or(false) {
            bail!("schema {schema} is already frozen or archived");
        }
        let _guard = match ArchivingGuard::claim(&self.archiving, schema) {
            Some(g) => g,
            None => bail!("schema {schema} is already being frozen/archived"),
        };
        {
            let mut map = self.entries.lock().await;
            if let Some(entry) = map.get(schema).and_then(|c| c.get()) {
                let active = entry.active_count();
                if active > 0 {
                    bail!("schema {schema} has {active} live session(s); refusing to freeze");
                }
            }
            map.remove(schema);
        }

        let known_id = self.store.record(schema).map(|r| r.sandbox_id);
        let entry = vm::ensure_vm(&self.cfg, schema, known_id.as_deref(), None, None)
            .await
            .with_context(|| format!("bringing up VM for schema {schema} to freeze it"))?;

        let bytes = vm::dump_to_local(
            &self.cfg,
            &entry.sandbox,
            schema,
            &dumps,
            freeze.listen.port(),
        )
        .await
        .with_context(|| format!("dumping schema {schema} to the local dump store"))?;

        self.store.set_tier(schema, Tier::Frozen).await;
        let accomplished = format!("frozen to a {bytes}-byte local dump");
        if let Err(e) = vm::kill_and_reclaim(&self.cfg, &entry.sandbox, schema, &accomplished).await {
            warn!("schema {schema}: frozen locally but killing the VM failed (orphaned): {e:#}");
        }
        Ok(())
    }

    // ---- orphan-disk reclamation --------------------------------------------

    /// Spawn the orphan-disk sweep if `PG_VM_POOL_ORPHAN_SWEEP_SECS` (and a run
    /// dir) are configured: periodically delete `sb-<id>/` directories heyvmd no
    /// longer knows about, reclaiming disks a kill acked but didn't remove.
    pub fn spawn_orphan_reaper(self: &Arc<Self>) {
        let Some(interval) = self.cfg.orphan_sweep else {
            info!("orphan-disk sweep disabled (PG_VM_POOL_ORPHAN_SWEEP_SECS unset/0)");
            return;
        };
        // config guarantees run_dir is Some whenever orphan_sweep is Some.
        let Some(run_dir) = self.cfg.run_dir.clone() else {
            return;
        };
        info!(
            "orphan-disk sweep: reclaiming forgotten sb-<id>/ dirs under {} every {:?} \
             (min age {:?}, ≤{} deletions/pass)",
            run_dir.display(),
            interval,
            ORPHAN_MIN_AGE,
            ORPHAN_MAX_DELETES_PER_SWEEP,
        );
        let registry = self.clone();
        let first = ORPHAN_FIRST_DELAY.min(interval);
        tokio::spawn(supervise("orphan-reaper", first, interval, move || {
            let registry = registry.clone();
            async move { registry.sweep_orphans().await }
        }));
    }

    /// One orphan-disk pass. Returns the number of directories deleted (for the
    /// supervisor heartbeat). Single-flighted on its own flag.
    ///
    /// A directory is deleted only when **all** of these hold, checked in
    /// cheapest-first order so a live disk is ruled out before any daemon
    /// round-trip or destructive call:
    ///   1. it's older than [`ORPHAN_MIN_AGE`] (not a VM mid-create);
    ///   2. no process holds a file in it open (not a running VM — the
    ///      chroot-proof device:inode check in [`crate::orphans`]);
    ///   3. heyvmd's per-id endpoint returns 404 (the daemon truly forgot it —
    ///      the *list* is unreliable, so we never classify off it);
    ///   4. its schema is offloaded (`frozen`/`archived`, data safe elsewhere)
    ///      or the id is unreferenced by any registry entry (dead).
    /// A `live`-tier schema whose VM is gone is a data-loss orphan: its disk is
    /// the only copy, so it is reported at error level and never deleted.
    /// Conditions 2 and 3 are re-checked immediately before each `remove_dir_all`
    /// against a fresh snapshot, since classification does many awaits.
    async fn sweep_orphans(&self) -> usize {
        let Some(run_dir) = self.cfg.run_dir.clone() else {
            return 0;
        };
        if self.orphan_sweeping.swap(true, Ordering::SeqCst) {
            info!("orphan-disk sweep: a pass is already running; skipping");
            return 0;
        }
        let _guard = SweepGuard(&self.orphan_sweeping);

        // Registry view: sandbox-id → (schema, tier). The authoritative map of
        // which disks a schema still depends on.
        let by_id: HashMap<String, (String, Tier)> = self
            .store_records()
            .into_iter()
            .map(|(schema, r)| (r.sandbox_id, (schema, r.tier)))
            .collect();

        // Enumerate old-enough sb-* dirs. Fresh dirs (a VM mid-create) are
        // skipped by the age floor.
        let entries = match std::fs::read_dir(&run_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("orphan-disk sweep: cannot read run dir {}: {e}", run_dir.display());
                return 0;
            }
        };
        let now = SystemTime::now();
        let mut dirs: Vec<(PathBuf, String)> = Vec::new();
        let mut too_new = 0usize;
        for ent in entries.flatten() {
            let name = ent.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("sb-") {
                continue;
            }
            let path = ent.path();
            if !path.is_dir() {
                continue;
            }
            if let Ok(md) = ent.metadata()
                && let Ok(mtime) = md.modified()
                && now
                    .duration_since(mtime)
                    .map(|age| age < ORPHAN_MIN_AGE)
                    .unwrap_or(true)
            {
                too_new += 1;
                continue;
            }
            dirs.push((path, name.to_string()));
        }
        if dirs.is_empty() {
            debug!("orphan-disk sweep: no candidate dirs ({too_new} too new)");
            return 0;
        }

        let open = crate::orphans::open_inodes();

        // Classify. Held-open first (a local filesystem check), then the daemon
        // round-trip only for dirs that survive it.
        let mut deletable: Vec<(PathBuf, String, String)> = Vec::new(); // (path, id, why)
        let mut dataloss: Vec<(String, String)> = Vec::new(); // (schema, id)
        let (mut alive, mut held, mut daemon_errs) = (0usize, 0usize, 0usize);
        let mut aborted = false;
        for (path, id) in dirs {
            if crate::orphans::dir_held_open(&path, &open) {
                held += 1;
                continue;
            }
            match self.daemon_state(&id).await {
                DaemonState::Present => alive += 1,
                DaemonState::Error => {
                    daemon_errs += 1;
                    if daemon_errs >= ORPHAN_MAX_DAEMON_ERRORS {
                        warn!(
                            "orphan-disk sweep: aborting after {daemon_errs} consecutive daemon \
                             errors — heyvmd looks unhealthy; a 'gone?' ambiguity must never \
                             become a deletion. Retrying next sweep"
                        );
                        aborted = true;
                        break;
                    }
                    continue;
                }
                DaemonState::Gone => {
                    daemon_errs = 0;
                    match by_id.get(&id) {
                        Some((schema, Tier::Live)) => dataloss.push((schema.clone(), id.clone())),
                        Some((schema, _)) => {
                            deletable.push((path, id, format!("offloaded schema {schema}")))
                        }
                        None => deletable.push((path, id, "unreferenced by any schema".into())),
                    }
                }
            }
        }

        // Data-loss orphans: never delete — shout so an operator acts.
        for (schema, id) in &dataloss {
            error!(
                "orphan-disk sweep: schema {schema}'s VM {id} is gone from heyvmd but its tier \
                 is still `live` — its data disk {}/{id} is the ONLY copy and the next connect \
                 will serve an EMPTY database. NOT deleting. Restore it, or mark it \
                 archived/frozen if the data is durable elsewhere.",
                run_dir.display(),
            );
        }

        let backlog = deletable.len().saturating_sub(ORPHAN_MAX_DELETES_PER_SWEEP);
        deletable.truncate(ORPHAN_MAX_DELETES_PER_SWEEP);
        info!(
            "orphan-disk sweep: {} deletable, {alive} live, {held} in use, {} data-loss orphan(s), \
             {too_new} too new{}{}",
            deletable.len() + backlog,
            dataloss.len(),
            if backlog > 0 { format!(", {backlog} deferred to later passes") } else { String::new() },
            if aborted { " (classification aborted early — daemon unhealthy)" } else { "" },
        );

        // Fresh open-file snapshot for the destructive phase: classification did
        // many awaits, during which a VM could have grabbed one of these disks.
        let open = crate::orphans::open_inodes();
        let (mut removed, mut freed, mut failed) = (0usize, 0u64, 0usize);
        for (path, id, why) in deletable {
            // Re-confirm not-held and still-gone immediately before deleting.
            if crate::orphans::dir_held_open(&path, &open) {
                continue;
            }
            if !matches!(self.daemon_state(&id).await, DaemonState::Gone) {
                continue;
            }
            let bytes = crate::orphans::dir_allocated_bytes(path.clone()).await;
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    removed += 1;
                    freed += bytes;
                    info!(
                        "orphan-disk sweep: reclaimed {} ({}) — {why}",
                        path.display(),
                        crate::orphans::human_iec(bytes),
                    );
                }
                Err(e) => {
                    failed += 1;
                    warn!(
                        "orphan-disk sweep: failed to remove {} ({why}): {e} — \
                         (jailer may have left root-owned files; the pooler needs \
                         delete permission on the run dir)",
                        path.display(),
                    );
                }
            }
        }
        if removed > 0 || failed > 0 {
            info!(
                "orphan-disk sweep: reclaimed {removed} disk(s), {} freed{}",
                crate::orphans::human_iec(freed),
                if failed > 0 { format!(", {failed} could not be removed") } else { String::new() },
            );
        }
        removed
    }

    /// What heyvmd's per-id endpoint says about sandbox `id`. Uses `get`
    /// (`GET /deployed-sandboxes/:id`), which resolves by id and reports even a
    /// stopped sandbox — unlike the list, which is unreliable. `Gone` is the
    /// only state that permits deletion; `Error` (daemon down/flaking) is
    /// deliberately distinct from `Gone` so ambiguity never deletes.
    async fn daemon_state(&self, id: &str) -> DaemonState {
        let sb = match Sandbox::connect(id.to_string(), vm::local_opts()) {
            Ok(sb) => sb,
            Err(e) => {
                debug!("orphan-disk sweep: cannot connect to check {id}: {e:#}");
                return DaemonState::Error;
            }
        };
        match sb.get().await {
            Ok(_) => DaemonState::Present,
            Err(HeyoError::NotFound(_)) => DaemonState::Gone,
            Err(e) => {
                debug!("orphan-disk sweep: daemon error checking {id}: {e:#}");
                DaemonState::Error
            }
        }
    }

    fn is_archiving(&self, schema: &str) -> bool {
        self.archiving.lock().unwrap().contains(schema)
    }
}

/// Run a periodic background pass forever, surviving panics.
///
/// The pooler's reaper and eviction sweep are long-lived `loop { sleep; pass }`
/// tasks. A bare `tokio::spawn`ed loop that panics mid-pass simply vanishes —
/// no restart, only a generic task-drop — so reaping would silently stop for the
/// rest of the process with nothing in the log pointing at it. That is exactly
/// the class of failure we most want to avoid here.
///
/// So each pass runs in its own child task: a panic surfaces as a `JoinError`
/// this supervisor logs loudly and then *continues* from, rather than an abort
/// that kills the loop. Passes stay strictly sequential (the child is awaited
/// before the next tick), so this changes nothing about concurrency — only
/// survivability. Each pass also emits a heartbeat: `debug` every time, and an
/// `info` "still alive" line at most every [`SUPERVISOR_HEARTBEAT`], so a healthy
/// idle loop is visibly live without flooding the log.
///
/// `make_pass` returns the future for one pass; its `usize` output is a count of
/// work done (VMs stopped / schemas archived), surfaced in the heartbeat.
///
/// `first_delay` is how long to wait before the *first* pass; `tick` is the gap
/// between every pass after that. They differ because a long `tick` (the hourly
/// eviction sweep) combined with restarts would otherwise starve the loop: every
/// restart resets the timer, so a pooler redeployed more often than `tick` never
/// runs a single pass. A short `first_delay` makes the first pass land soon after
/// startup regardless. The reaper, whose `tick` is already short, just passes
/// `tick` for both.
async fn supervise<F, Fut>(
    name: &'static str,
    first_delay: Duration,
    tick: Duration,
    mut make_pass: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = usize> + Send + 'static,
{
    let mut passes: u64 = 0;
    let mut actions: u64 = 0;
    let mut last_beat: Option<Instant> = None;
    loop {
        tokio::time::sleep(if passes == 0 { first_delay } else { tick }).await;
        passes += 1;
        let started = Instant::now();
        match tokio::spawn(make_pass()).await {
            Ok(n) => {
                actions += n as u64;
                let elapsed = started.elapsed();
                debug!("{name}: pass {passes} ok in {elapsed:?} (acted on {n})");
                // Throttle the info-level heartbeat; always beat on the first pass
                // so startup shows the loop is running.
                let due = last_beat.is_none_or(|t| t.elapsed() >= SUPERVISOR_HEARTBEAT);
                if due {
                    info!(
                        "{name}: alive — {passes} pass(es), {actions} action(s) total; \
                         last pass acted on {n} in {elapsed:?}"
                    );
                    last_beat = Some(Instant::now());
                }
            }
            // A panicked pass is isolated to its child task; recover and keep the
            // loop alive so one bad pass never disables reaping for good.
            Err(e) if e.is_panic() => error!(
                "{name}: pass {passes} PANICKED after {:?} — supervisor recovering, \
                 reaping continues: {}",
                started.elapsed(),
                panic_message(e.into_panic()),
            ),
            // Cancellation only happens on runtime shutdown; nothing to recover.
            Err(e) => warn!("{name}: pass {passes} did not complete: {e}"),
        }
    }
}

/// Best-effort human text from a caught panic payload (`&str`/`String`, else a
/// placeholder) for the supervisor's error log.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Clears the `sweeping` flag on drop, so a panic or early return in the middle
/// of a sweep can't leave the registry permanently believing a sweep is running.
/// heyvmd's verdict on one sandbox id, from the authoritative per-id endpoint.
/// `Error` is kept distinct from `Gone` on purpose: only `Gone` may delete, so
/// a flaking daemon can never turn "I can't tell" into a destructive action.
enum DaemonState {
    Present,
    Gone,
    Error,
}

struct SweepGuard<'a>(&'a AtomicBool);

impl Drop for SweepGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// RAII claim on the `archiving` set: inserts on `claim`, removes on `Drop`, so
/// a schema is never left stuck "archiving" if the operation errors or panics.
struct ArchivingGuard<'a> {
    set: &'a StdMutex<HashSet<String>>,
    schema: String,
}

impl<'a> ArchivingGuard<'a> {
    /// `Some` if this call inserted the schema; `None` if it was already present
    /// (another archive is in flight).
    fn claim(set: &'a StdMutex<HashSet<String>>, schema: &str) -> Option<Self> {
        if set.lock().unwrap().insert(schema.to_string()) {
            Some(Self {
                set,
                schema: schema.to_string(),
            })
        } else {
            None
        }
    }
}

impl Drop for ArchivingGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.schema);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the archive sweep should do with one schema.
#[derive(Debug, PartialEq)]
enum SweepAction {
    /// Not a candidate (keepalive, already archived, or not idle long enough).
    Skip,
    /// Durably stale but actually live (warm with sessions, or a young in-memory
    /// idle clock) — refresh its `last_active` and leave it running.
    Refresh,
    /// Genuinely cold: offload to S3 and kill the VM.
    Archive,
}

/// Pure decision for one schema, factored out of [`SchemaRegistry::sweep_archive`]
/// so the cross-check between durable and live state is testable. `live` is the
/// schema's warm `(active_sessions, in_memory_idle_secs)` if it's in the map.
fn classify_candidate(
    rec: &StoreRecord,
    keepalive: bool,
    now: u64,
    threshold_secs: u64,
    live: Option<(usize, u64)>,
) -> SweepAction {
    if rec.offloaded() || keepalive {
        return SweepAction::Skip;
    }
    if now.saturating_sub(rec.last_active) < threshold_secs {
        return SweepAction::Skip;
    }
    if let Some((active, idle_secs)) = live
        && (active > 0 || idle_secs < threshold_secs)
    {
        return SweepAction::Refresh;
    }
    SweepAction::Archive
}

#[cfg(test)]
mod archive_tests {
    use super::*;

    fn rec(last_active: u64, archived: bool) -> StoreRecord {
        StoreRecord {
            sandbox_id: "sb-x".into(),
            last_active,
            tier: if archived { Tier::Archived } else { Tier::Live },
        }
    }

    #[test]
    fn classify_candidate_covers_the_cross_check() {
        let now = 1_000_000;
        let week = 604_800;

        // Cold and stopped (not in the warm map) → archive.
        assert_eq!(
            classify_candidate(&rec(now - week - 1, false), false, now, week, None),
            SweepAction::Archive
        );
        // Not idle long enough → skip.
        assert_eq!(
            classify_candidate(&rec(now - 10, false), false, now, week, None),
            SweepAction::Skip
        );
        // Already archived → skip (don't re-archive).
        assert_eq!(
            classify_candidate(&rec(now - week - 1, true), false, now, week, None),
            SweepAction::Skip
        );
        // Keepalive schema → never archived, however stale.
        assert_eq!(
            classify_candidate(&rec(0, false), true, now, week, None),
            SweepAction::Skip
        );
        // Durably stale but warm with a live session → refresh, don't archive
        // (a long-lived single connection with no new checkouts).
        assert_eq!(
            classify_candidate(&rec(now - week - 1, false), false, now, week, Some((1, week + 5))),
            SweepAction::Refresh
        );
        // Durably stale, warm, no sessions, but in-memory idle clock is young →
        // refresh (it's genuinely been used recently).
        assert_eq!(
            classify_candidate(&rec(now - week - 1, false), false, now, week, Some((0, 30))),
            SweepAction::Refresh
        );
        // Durably stale, warm, no sessions, and in-memory idle also past the
        // threshold → archive.
        assert_eq!(
            classify_candidate(&rec(now - week - 1, false), false, now, week, Some((0, week + 5))),
            SweepAction::Archive
        );
    }
}

/// Filesystem usage of the guest's Postgres data directory via
/// `COPY FROM PROGRAM 'df -kP …'` — `df` needs statvfs, which no `/proc`
/// read can provide. Runs in one transaction with an `ON COMMIT DROP` temp
/// table so nothing leaks onto the pooled session; any failure (no superuser,
/// no `df` in the image) rolls back and yields `None`.
async fn df_data_dir(client: &mut deadpool_postgres::Object) -> Option<(u64, u64, u64)> {
    let tx = client.transaction().await.ok()?;
    let datadir: String = tx
        .query_one("SELECT current_setting('data_directory')", &[])
        .await
        .ok()?
        .get(0);
    // The path is trusted (it's the server's own data_directory) but still
    // SQL-quoted (' → '') and shell-double-quoted for hygiene.
    let sql = format!(
        "CREATE TEMP TABLE _dash_df(line text) ON COMMIT DROP; \
         COPY _dash_df FROM PROGRAM 'df -kP \"{}\"'",
        datadir.replace('\'', "''")
    );
    tx.batch_execute(&sql).await.ok()?;
    let rows = tx.query("SELECT line FROM _dash_df", &[]).await.ok()?;
    let _ = tx.commit().await;
    parse_df(rows.iter().map(|r| r.get(0)))
}

/// Pull `MemTotal`/`MemAvailable` out of `/proc/meminfo` text → bytes.
fn parse_meminfo(s: &str) -> Option<(u64, u64)> {
    let kb = |line: &str| line.split_whitespace().nth(1)?.parse::<u64>().ok();
    let mut total = None;
    let mut avail = None;
    for line in s.lines() {
        if line.starts_with("MemTotal:") {
            total = kb(line);
        } else if line.starts_with("MemAvailable:") {
            avail = kb(line);
        }
    }
    Some((total? * 1024, avail? * 1024))
}

/// First three fields of `/proc/loadavg` (1/5/15-minute load).
fn parse_loadavg(s: &str) -> Option<(f64, f64, f64)> {
    let mut it = s.split_whitespace();
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

/// Parse `df -kP` (POSIX portable) output → (total, used, available) bytes.
/// Finds the first data line by its all-numeric 1024-block column, so the
/// header (whose second field is "1024-blocks") is skipped regardless of row
/// order.
fn parse_df<'a>(lines: impl Iterator<Item = &'a str>) -> Option<(u64, u64, u64)> {
    for line in lines {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 6 && !f[1].is_empty() && f[1].bytes().all(|b| b.is_ascii_digit()) {
            let total = f[1].parse::<u64>().ok()?;
            let used = f[2].parse::<u64>().ok()?;
            let avail = f[3].parse::<u64>().ok()?;
            return Some((total * 1024, used * 1024, avail * 1024));
        }
    }
    None
}

/// Percent-full of the filesystem holding `path`, read on the host via
/// `df -kP` (same basis as df's own Use%: `used / (used + avail)`, excluding
/// root-reserved blocks). `None` on any failure — the pressure loop treats
/// that as "can't tell", never as pressure.
async fn disk_used_pct(path: &std::path::Path) -> Option<f64> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("df").arg("-kP").arg(path).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (_total, used, avail) = parse_df(text.lines())?;
    used_pct(used, avail)
}

/// `used / (used + avail)` as a percentage; `None` when the denominator is 0.
fn used_pct(used: u64, avail: u64) -> Option<f64> {
    let denom = (used + avail) as f64;
    if denom <= 0.0 {
        return None;
    }
    Some(used as f64 / denom * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_pct_matches_df_semantics() {
        // 850 used / 1000 (used+avail) → 85%, independent of `total` (which
        // includes root-reserved blocks df's Use% ignores).
        assert_eq!(used_pct(850, 150), Some(85.0));
        assert_eq!(used_pct(0, 100), Some(0.0));
        assert_eq!(used_pct(100, 0), Some(100.0));
        // An empty df line must read as "unknown", not 0% (which would
        // silently disable pressure eviction forever).
        assert_eq!(used_pct(0, 0), None);
    }

    #[test]
    fn meminfo_yields_total_and_available_bytes() {
        let s = "MemTotal:        8028896 kB\n\
                 MemFree:          734500 kB\n\
                 MemAvailable:    7600004 kB\n\
                 Buffers:           12345 kB\n";
        assert_eq!(parse_meminfo(s), Some((8_028_896 * 1024, 7_600_004 * 1024)));
        // Missing MemAvailable (ancient kernel) → None rather than garbage.
        assert_eq!(parse_meminfo("MemTotal: 100 kB\n"), None);
    }

    #[test]
    fn loadavg_yields_three_floats() {
        assert_eq!(
            parse_loadavg("0.52 0.30 0.18 2/213 4189\n"),
            Some((0.52, 0.30, 0.18))
        );
        assert_eq!(parse_loadavg(""), None);
    }

    #[test]
    fn df_skips_header_and_parses_first_data_line() {
        let out = [
            "Filesystem     1024-blocks    Used Available Capacity Mounted on",
            "/dev/vdb           4062912  950000   3112912      24% /workspace",
        ];
        assert_eq!(
            parse_df(out.into_iter()),
            Some((4_062_912 * 1024, 950_000 * 1024, 3_112_912 * 1024))
        );
        // Header only (df failed mid-flight) → None.
        assert_eq!(parse_df(out[..1].iter().copied()), None);
    }

    /// The whole point of the supervisor: a pass that panics must not kill the
    /// loop. If it did, `calls` would freeze at 1 (the panicking pass) and every
    /// later tick would never fire.
    #[tokio::test(start_paused = true)]
    async fn supervisor_survives_a_panicking_pass() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let task = tokio::spawn(supervise(
            "test",
            Duration::from_millis(10),
            Duration::from_millis(10),
            move || {
                let c = c.clone();
                async move {
                    // Panic on the first pass only; succeed forever after.
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        panic!("boom on the first pass");
                    }
                    0usize
                }
            },
        ));
        // Paused clock: advancing time drives the ticks deterministically without
        // real waiting. Several ticks should elapse past the initial panic.
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
        }
        task.abort();
        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "loop stalled after the panic (only {} pass(es) ran)",
            calls.load(Ordering::SeqCst)
        );
    }
}
