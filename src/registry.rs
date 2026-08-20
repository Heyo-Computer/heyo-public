//! Schema -> VM registry. One entry per schema, created once and reused.
//! A background reaper stops VMs that go idle (no connections) for too long.

use std::cmp::Reverse;
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

use crate::config::{Config, DiskGrowConfig, PressureConfig};
use crate::dedicated::{Credential, Credentials};
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

/// How often the offload pacer wakes to consider one unit of housekeeping.
/// Short on purpose: the tick is not how fast work happens (one job can take
/// minutes), it is how quickly the pacer notices the host went quiet — a
/// second after the last client bring-up drains, the next cold schema starts
/// moving. A tick with nothing to do costs a couple of atomic loads.
const OFFLOAD_TICK: Duration = Duration::from_secs(1);

/// Grace before the pacer's first job, so a restart's reconnect storm and the
/// warm pool's first fill happen against an otherwise idle host.
const OFFLOAD_FIRST_DELAY: Duration = Duration::from_secs(60);

/// Bounds on how long the pacer waits after a scan that found no work, taken
/// from the configured `*_SWEEP_SECS` (see [`SchemaRegistry::offload_idle_rescan`]).
const OFFLOAD_IDLE_RESCAN_MIN: Duration = Duration::from_secs(5);
const OFFLOAD_IDLE_RESCAN_MAX: Duration = Duration::from_secs(60);

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

/// Cap on leftover boot artefacts pruned per sweep (see the rootfs prune in
/// [`SchemaRegistry::sweep_orphans`]). Higher than the directory cap: removing
/// one file from a stopped VM's directory is cheap and completely reversible —
/// the daemon rebuilds it from the base image on the next boot — where
/// deleting a directory is not.
const ORPHAN_MAX_ROOTFS_PRUNES_PER_SWEEP: usize = 250;

/// Name of the per-VM rootfs copy heyvmd clones into `run/<id>/` at boot and
/// removes again on a clean stop. One that outlives its VM is pure waste
/// (~200MB each on the pg image); see the prune in
/// [`SchemaRegistry::sweep_orphans`].
const VM_ROOTFS_FILE: &str = "rootfs.ext4";

/// Abort an orphan sweep after this many consecutive daemon errors while
/// checking sandbox liveness: a flaking/restarting heyvmd must never let a
/// "gone?" ambiguity turn into a deletion, so we stop and try again next sweep.
const ORPHAN_MAX_DAEMON_ERRORS: usize = 5;

/// Cadence of the pending-bring-up janitor (see [`SchemaRegistry::spawn_pending_janitor`]).
/// Frequent is fine: a pass over an empty ledger is a HashMap read.
const PENDING_JANITOR_TICK: Duration = Duration::from_secs(300);
const PENDING_JANITOR_FIRST_DELAY: Duration = Duration::from_secs(240);

/// First-failure backoff for offloads (archive/freeze) of one schema.
/// Doubles per consecutive failure, capped at [`OFFLOAD_BACKOFF_CAP`].
const OFFLOAD_BACKOFF_BASE: Duration = Duration::from_secs(30 * 60);
/// Ceiling on the offload backoff: even a permanently sick schema is retried
/// this often, so a fixed environment heals without operator action.
const OFFLOAD_BACKOFF_CAP: Duration = Duration::from_secs(24 * 3600);

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
    // Per-schema failure memory so the sweeps skip recently-failed offloads
    // instead of burning a ready-timeout on the same sick schemas every pass.
    offload_backoff: OffloadBackoff,
    // Single-flights the dashboard's purge action.
    purging: AtomicBool,
    // Provisioned dedicated databases: `database → (role, password)`. Consulted
    // on the auth path (which password to challenge for, and whether this
    // client may route here at all) and on every bring-up (so the owning role
    // exists inside the VM). Empty unless an operator has provisioned one.
    dedicated: Arc<Credentials>,
}

impl SchemaRegistry {
    pub fn new(cfg: Config) -> Self {
        let store = Store::load(cfg.state_file.clone());
        let reclaimer = cfg
            .reclaim
            .as_ref()
            .map(|r| Arc::new(Reclaimer::new(r.cmd.clone(), cfg.run_dir.clone())));
        let spares = (cfg.warm_spares > 0).then(|| Arc::new(SparePool::new(cfg.warm_spares)));
        // The dump server has two consumers: the frozen tier (freeze + thaw)
        // and the S3 archive tier, whose dumps *stream* through it so they
        // never touch the guest's data disk. Either one enables it.
        let dumps = (cfg.freeze.is_some() || cfg.archive.is_some())
            .then(|| Arc::new(DumpServer::new(cfg.dump_net.dump_dir.clone())));
        let dedicated = Arc::new(Credentials::load(cfg.dedicated_file.clone()));
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
            offload_backoff: OffloadBackoff::new(),
            purging: AtomicBool::new(false),
            dedicated,
        }
    }

    /// The provisioned dedicated-database credentials — the auth path's lookup
    /// table and what the admin API/dashboard mutate.
    pub fn dedicated(&self) -> &Arc<Credentials> {
        &self.dedicated
    }

    /// The owning credential for `schema`, if it is a dedicated database. Every
    /// bring-up passes this to [`vm::ensure_vm`] so the role exists (and owns
    /// its database) inside whatever VM ends up serving it.
    fn owner_of(&self, schema: &str) -> Option<Credential> {
        self.dedicated.by_database(schema)
    }

    /// Provision a dedicated database: a fixed database name with its own login
    /// role and password, whose credential can never be used to create another
    /// VM (see [`crate::dedicated`]).
    ///
    /// Refuses a name the pooler has *already* backed as an ordinary schema —
    /// that VM holds someone else's data, and provisioning over it would hand
    /// that data to a brand-new credential. Recording the credential is all
    /// this does; the VM is built by the first checkout, exactly like any other
    /// schema (callers that want it warm up front can follow with
    /// [`Self::spawn_provision`]).
    pub fn create_dedicated(
        &self,
        database: &str,
        role: &str,
        password: &str,
    ) -> Result<Credential> {
        if self.store.record(database).is_some() && !self.dedicated.is_dedicated(database) {
            bail!(
                "{database:?} is already an existing pooler schema with its own VM and data; \
                 pick a different name (or drop that schema first)"
            );
        }
        self.dedicated.create(database, role, password)
    }

    /// Bring `schema`'s VM up in the background and let go of it immediately.
    ///
    /// Used right after provisioning so the tenant's first real connection
    /// doesn't pay a cold start. Runs through the ordinary checkout path, so it
    /// shares the bring-up gate, the pending-bring-up ledger and the
    /// failed-bring-up cleanup with every other client — a failure here is
    /// logged and left for the next connect to retry, never fatal to the
    /// provisioning that triggered it.
    pub fn spawn_provision(self: &Arc<Self>, schema: &str) {
        let registry = self.clone();
        let schema = schema.to_string();
        tokio::spawn(async move {
            match registry.checkout(&schema).await {
                // Dropping the guard leaves the VM warm; the idle reaper takes
                // it from here like any other unused schema.
                Ok(_guard) => info!("schema {schema}: pre-provisioned VM is ready"),
                Err(e) => warn!("schema {schema}: pre-provisioning failed (will retry on the first client connect): {e:#}"),
            }
        });
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

    /// Whether the image-level archive is configured — gates the dashboard's
    /// per-VM "archive as image" control.
    pub fn image_archive_enabled(&self) -> bool {
        self.cfg.image_archive.is_some()
    }

    /// Whether automatic disk reclamation is configured — gates the dashboard's
    /// manual "reclaim disk slack" control.
    pub fn reclaim_enabled(&self) -> bool {
        self.reclaimer.is_some()
    }

    /// The port clients dial the pooler on, for rendering an example connection
    /// string. Only the port: the host a client should use is whatever already
    /// reaches this pooler, which a bind address like `0.0.0.0` can't tell us.
    pub fn listen_port(&self) -> u16 {
        self.cfg.listen_addr.port()
    }

    /// The configured heyvmd run dir, if any (`PG_VM_POOL_RUN_DIR`).
    pub fn run_dir(&self) -> Option<PathBuf> {
        self.cfg.run_dir.clone()
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

    /// The durable record for one schema — which VM last backed it and which
    /// storage tier its data is on. `None` when the pooler has never brought it
    /// up (a just-provisioned dedicated database, before its first checkout).
    pub fn store_record(&self, schema: &str) -> Option<StoreRecord> {
        self.store.record(schema)
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
                    // "Archived" covers both archive formats — which S3 key
                    // actually holds data decides the restore strategy (a HEAD
                    // or two on the cold path; transport trouble defaults to
                    // the dump path, exactly the pre-image behavior).
                    Some(a) => Some(
                        match crate::imgarchive::pick_restore(&a.s3, schema).await {
                            crate::imgarchive::RestoreKind::Dump => RestoreSource::S3(a.s3.clone()),
                            crate::imgarchive::RestoreKind::Image => {
                                info!("schema {schema}: restoring from its disk-image archive");
                                RestoreSource::S3Image(a.s3.clone())
                            }
                        },
                    ),
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
                Some(Tier::Compacted) => match &self.cfg.compact {
                    Some(c) => Some(RestoreSource::LocalImage(c.compact_path(schema))),
                    None => bail!(
                        "schema {schema} is compacted to a local image, but the compacted \
                         tier is not configured (set PG_VM_POOL_COMPACT_AFTER_SECS + \
                         PG_VM_POOL_RUN_DIR) — cannot thaw it"
                    ),
                },
                _ => None,
            };
            let bound = self.spares.as_ref().map(|_| self.bound_ids()).unwrap_or_default();
            // A dedicated database's owning role has to exist inside whatever
            // VM serves it — including one a restore has just rebuilt from
            // scratch, which carries the data but no roles.
            let owner = self.owner_of(schema);
            match cell
                .get_or_try_init(|| {
                    vm::ensure_vm(
                        &self.cfg,
                        schema,
                        known_id.as_deref(),
                        restore.as_ref(),
                        self.spares.as_deref().map(|p| (p, &bound)),
                        owner.as_ref(),
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
                    // The bring-up resolved: the id is durably bound, so the
                    // pending ledger's claim on it is settled.
                    crate::pending::clear(schema).await;
                    // A thawed schema's local offload artifact is now dead
                    // weight: the row is durably live, the data lives on the
                    // VM's disk, and the next freeze/compact rewrites the file
                    // from scratch. Left in place it pins those bytes per
                    // thawed schema forever (they are only otherwise deleted
                    // on S3 promotion).
                    let thawed_file = match &restore {
                        Some(RestoreSource::Local { .. }) => {
                            self.dumps.as_ref().map(|srv| srv.dump_path(schema))
                        }
                        Some(RestoreSource::LocalImage(path)) => Some(path.clone()),
                        _ => None,
                    };
                    if let Some(file) = thawed_file {
                        match tokio::fs::remove_file(&file).await {
                            Ok(()) => {
                                info!("schema {schema}: thawed; removed {}", file.display());
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => warn!(
                                "schema {schema}: thawed but removing {} failed: {e}",
                                file.display()
                            ),
                        }
                    }
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
                    crate::events::journal_error(
                        "bring-up",
                        format!(
                            "schema {schema}: bring-up failed after {:?}: {e:#}",
                            started.elapsed()
                        ),
                    );
                    // Same leak guard as the archive/freeze bring-ups: a
                    // ready-timeout leaves a running VM `ensure_vm` never
                    // returned a handle to. Without this it has no owner —
                    // no registry row, not spare-named, daemon still knows
                    // it — so no reaper, purge, or sweep ever reclaims its
                    // RAM or disk.
                    vm::stop_after_failed_bringup(schema, known_id.as_deref()).await;
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
        match self.cfg.disk_grow {
            Some(g) => info!(
                "disk growth: at idle-stop, devices spanned by a >= {:.0}%-full data fs \
                 are doubled offline (cap {}GiB, via the daemon's workspace resize)",
                g.pct, g.max_gb
            ),
            None => info!("disk growth disabled (PG_VM_POOL_DISK_GROW_PCT unset)"),
        }
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
        // Device-growth candidates discovered while stopping: (schema, id,
        // target GiB). Sampled over the still-warm pool BEFORE the stop tears
        // it down — after the stop there's no cheap way to ask the guest, and
        // the resize is cheapest exactly then (the daemon's resize is offline;
        // on an already-stopped VM it costs no client disruption at all).
        let mut grow: Vec<(String, String, u64)> = Vec::new();
        for (schema, entry) in victims {
            info!("idle-stopping VM for schema {schema} (no connections for >= {timeout:?})");
            if let Some(gc) = self.cfg.disk_grow
                && let Some((fs, dev)) = sample_disk(&entry).await
                && let Some(target) = grow_target_gb(fs, dev, &gc)
            {
                info!(
                    "schema {schema}: data fs is >= {:.0}% full and spans its device — \
                     queueing offline device grow to {target}GiB",
                    gc.pct
                );
                grow.push((schema.clone(), entry.sandbox_id(), target));
            }
            checkpoint_and_stop(&entry, &schema).await;
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
        // Run queued device grows in the background, strictly sequential (the
        // daemon single-flights resizes anyway) and each under the boot
        // permit: the daemon's resize fscks and cold-boots the stopped disk,
        // which must never interleave with a reclaim pass fsck'ing the same
        // file (see reclaim::BOOT_GATE).
        if !grow.is_empty() {
            tokio::spawn(async move {
                for (schema, id, target) in grow {
                    let _permit = crate::reclaim::boot_permit().await;
                    match vm::resize_disk(&id, target).await {
                        Ok(()) => {
                            info!("schema {schema}: data device grown to {target}GiB");
                            crate::events::journal_info(
                                "disk-grow",
                                format!("schema {schema}: device grown to {target}GiB ({id})"),
                            );
                        }
                        Err(e) => {
                            warn!("schema {schema}: device grow to {target}GiB failed: {e:#}");
                            crate::events::journal_error(
                                "disk-grow",
                                format!("schema {schema}: grow of {id} to {target}GiB failed: {e:#}"),
                            );
                        }
                    }
                }
            });
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

    // ---- offload tiers (compact / freeze / S3) ------------------------------

    /// Spawn the **offload pacer**: one task that trickles cold schemas down
    /// the storage ladder — compact, freeze, promote-to-S3, archive-to-S3 — one
    /// schema at a time, whenever the host has nothing better to do.
    ///
    /// This replaces the three periodic batch sweeps (S3 eviction, freeze,
    /// compact). Batching was the wrong shape for the work. A sweep woke on its
    /// own timer regardless of what the host was doing, then ran every
    /// candidate it found back to back — each one a VM boot plus a `pg_dump`
    /// plus an upload, minutes apiece, holding a bring-up slot and the shared
    /// sweep lock throughout. On a fleet with a real backlog that is a
    /// multi-hour block of self-inflicted load that lands, by construction, at
    /// an arbitrary moment: exactly when clients are reconnecting, or when the
    /// warm pool is trying to rebuild, is as likely as any other time.
    ///
    /// The pacer inverts it. It wakes every [`OFFLOAD_TICK`], and before taking
    /// *each* job it re-asks whether the host is quiet (see
    /// [`Self::offload_backpressure`]): no client is queued for a bring-up, no
    /// reclaim pass is running, no other offload is in flight. One job runs at
    /// a time, and the next one is re-evaluated against a fresh view of the
    /// host a second later. The total work done is the same; it is spread thin
    /// and yields to anything a user is waiting on.
    ///
    /// Scanning is cheap because it only happens when the pacer is actually
    /// about to act: a tick during a busy host, or during a running job, is an
    /// atomic load. When a scan finds nothing, the next one is deferred by the
    /// configured sweep interval (the `*_SWEEP_SECS` vars keep that meaning),
    /// so an idle fleet costs a scan a minute rather than one a second.
    pub fn spawn_offloader(self: &Arc<Self>) {
        let mut tiers: Vec<String> = Vec::new();
        if let Some(c) = &self.cfg.compact {
            tiers.push(format!(
                "compact >= {:?} into {}",
                c.compact_after,
                c.compact_dir.display()
            ));
        }
        if let Some(f) = &self.cfg.freeze {
            tiers.push(format!(
                "freeze >= {:?} into {}",
                f.freeze_after,
                f.dump_dir.display()
            ));
        }
        if let Some(a) = &self.cfg.archive {
            tiers.push(format!(
                "S3 >= {:?} (s3://{}/{})",
                a.archive_after, a.s3.bucket, a.s3.prefix
            ));
        }
        if tiers.is_empty() {
            info!(
                "offload pacer disabled — no tier configured (PG_VM_POOL_COMPACT_AFTER_SECS / \
                 FREEZE_AFTER_SECS / ARCHIVE_AFTER_SECS all unset)"
            );
            return;
        }
        let idle_rescan = self.offload_idle_rescan();
        info!(
            "offload pacer: {} — one schema at a time, only while the host is quiet \
             (tick {OFFLOAD_TICK:?}, rescan {idle_rescan:?} when there is nothing to do)",
            tiers.join(", "),
        );

        let registry = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(OFFLOAD_FIRST_DELAY).await;
            // When the last scan found nothing, don't scan again until this.
            let mut hold_until: Option<Instant> = None;
            let mut quiet_since: Option<Instant> = None;
            loop {
                tokio::time::sleep(OFFLOAD_TICK).await;
                if hold_until.is_some_and(|t| Instant::now() < t) {
                    continue;
                }
                if let Some(reason) = registry.offload_backpressure() {
                    // Log the first deferral of each busy stretch only: this
                    // loop runs 86 400 times a day and a busy host would
                    // otherwise fill the log with it.
                    if quiet_since.take().is_some() {
                        debug!("offload pacer: holding off — {reason}");
                    }
                    continue;
                }
                quiet_since.get_or_insert_with(Instant::now);
                let policy = registry.offload_policy();
                let Some(job) = registry.next_offload_job(policy).await else {
                    hold_until = Some(Instant::now() + idle_rescan);
                    continue;
                };
                hold_until = None;
                // Run each job in its own task so a panic inside one offload
                // is contained — the pacer must outlive any single schema.
                let r = registry.clone();
                let started = Instant::now();
                let label = format!("{} {}", job.kind.as_str(), job.schema);
                match tokio::spawn(async move { r.run_offload_job(job).await }).await {
                    Ok(_) => debug!("offload pacer: {label} finished in {:?}", started.elapsed()),
                    Err(e) if e.is_panic() => error!(
                        "offload pacer: {label} PANICKED: {}",
                        panic_message(e.into_panic())
                    ),
                    Err(e) => error!("offload pacer: {label} failed to run: {e}"),
                }
            }
        });
    }

    /// Why background offloading should hold off this tick, if it should.
    ///
    /// Every condition here is "something a person is waiting on, or something
    /// that would fight us for the same resource". Offloading is pure
    /// housekeeping: it has no deadline, so it always loses.
    fn offload_backpressure(&self) -> Option<&'static str> {
        if crate::vm::bringups_waiting() > 0 {
            return Some("client bring-ups are queued");
        }
        // A pass holds the boot gate; an offload that boots a VM to dump it
        // would make it yield and lose that pass's progress.
        if crate::reclaim::pass_running() {
            return Some("a disk-reclaim pass is running");
        }
        // A manual batch sweep from the dashboard.
        if self.sweeping.load(Ordering::SeqCst) {
            return Some("a manual sweep is running");
        }
        // Any offload at all — ours, the pressure reaper's, or a dashboard
        // button's. This is what keeps the whole host to one heavy offload at
        // a time without a second lock.
        if !self.archiving.lock().unwrap().is_empty() {
            return Some("another offload is in flight");
        }
        None
    }

    /// How long to wait before re-scanning after a scan that found nothing.
    /// Taken from the shortest configured `*_SWEEP_SECS` — those vars no
    /// longer pace the work itself, only how eagerly an idle pooler looks for
    /// new candidates — and clamped so neither a 1-second nor a 12-hour value
    /// makes the pacer useless.
    fn offload_idle_rescan(&self) -> Duration {
        [
            self.cfg.compact.as_ref().map(|c| c.sweep_interval),
            self.cfg.freeze.as_ref().map(|f| f.sweep_interval),
            self.cfg.archive.as_ref().map(|a| a.sweep_interval),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(OFFLOAD_IDLE_RESCAN_MAX)
        .clamp(OFFLOAD_IDLE_RESCAN_MIN, OFFLOAD_IDLE_RESCAN_MAX)
    }

    /// The single most valuable offload available right now, or `None` when
    /// nothing is eligible. Also keeps `last_active` honest for schemas that
    /// look durably stale but are actually warm, so they aren't re-evaluated
    /// as candidates on every scan.
    async fn next_offload_job(&self, policy: OffloadPolicy) -> Option<OffloadJob> {
        // Live cross-check: a schema warm with active connections, or whose
        // in-memory idle clock is younger than the threshold, is not cold even
        // if its durable `last_active` drifted (one long-lived connection with
        // no new checkouts).
        let live: HashMap<String, (usize, u64)> = self
            .snapshot()
            .await
            .into_iter()
            .map(|e| (e.schema, (e.active, e.idle_secs)))
            .collect();
        let records = self.store_records();
        let at = Instant::now();
        let (job, refresh) = pick_offload_job(
            &records,
            &live,
            policy,
            &|schema| self.cfg.is_keepalive(schema),
            &|schema| self.offload_backoff.active(schema, at).is_some(),
            now_unix(),
        );
        for schema in refresh {
            self.store.touch(&schema);
        }
        job
    }

    /// Routine housekeeping: the configured thresholds, cheapest job first.
    fn offload_policy(&self) -> OffloadPolicy {
        OffloadPolicy {
            compact_after: self.cfg.compact.as_ref().map(|c| c.compact_after.as_secs()),
            freeze_after: self.cfg.freeze.as_ref().map(|f| f.freeze_after.as_secs()),
            archive_after: self.cfg.archive.as_ref().map(|a| a.archive_after.as_secs()),
            image_archive: self.image_archive_enabled(),
            mode: OffloadMode::Routine,
        }
    }

    /// Emergency policy: every idle schema is a candidate whatever its age
    /// (threshold `0`), coldest first, and freezing is dropped entirely — it
    /// costs a boot and leaves the bytes on the host, which is the one thing
    /// that cannot help here. The tiers themselves must still be configured:
    /// pressure eviction overrides the *thresholds*, never the operator's
    /// choice of where data may go.
    fn pressure_policy(&self) -> OffloadPolicy {
        OffloadPolicy {
            compact_after: self.cfg.compact.as_ref().map(|_| 0),
            freeze_after: None,
            archive_after: self.cfg.archive.as_ref().map(|_| 0),
            image_archive: self.image_archive_enabled(),
            mode: OffloadMode::Pressure,
        }
    }

    /// Perform one job. Each arm is the same per-schema entry point the
    /// dashboard's buttons use, so failures journal and enter the per-schema
    /// backoff exactly as they always have.
    async fn run_offload_job(&self, job: OffloadJob) -> Result<()> {
        let OffloadJob { schema, kind } = job;
        info!("offload: {} schema {schema}", kind.as_str());
        let res = match kind {
            // `archive_schema` dispatches on the schema's current tier, so a
            // frozen/compacted schema is promoted (a file upload, no VM) and a
            // live one is dumped and killed.
            OffloadKind::Promote | OffloadKind::Archive => self.archive_schema(&schema).await,
            OffloadKind::Compact => self.compact_schema(&schema).await,
            OffloadKind::ImageArchive => self.archive_schema_as_image(&schema, None).await,
            OffloadKind::Freeze => self.freeze_schema(&schema).await,
        };
        if let Err(e) = &res {
            warn!(
                "offload: {} schema {schema} failed (backing off): {e:#}",
                kind.as_str()
            );
        }
        res
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
        // Claiming (or failure-killing) a spare pokes the wake handle, so the
        // deficit is rebuilt immediately rather than on the next tick.
        let wake = pool.replenish_wake();
        tokio::spawn(supervise_with_wake(
            "warm-spares",
            Duration::from_secs(15),
            Duration::from_secs(60),
            Some(wake),
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
        crate::events::journal_error(
            "sweep.pressure",
            format!(
                "{} at {pct:.1}% (>= {:.1}%) — emergency archiving engaged",
                p.path.display(),
                p.high_pct
            ),
        );

        // The emergency ladder, re-picked from scratch before every job: take
        // whatever frees the most bytes for the least work right now — which,
        // on a nearly-full host, means never booting a VM while any no-boot
        // option remains (see `OffloadKind::rank`). Candidate selection is
        // shared with the routine pacer, so the guards that keep a busy or
        // recently-failed schema out of reach are the same ones.
        //
        // Why re-pick rather than walk a list: each job changes the tier of
        // the schema it touches, and a compaction that frees 180MB is worth
        // more than the promotion of an image that frees 7MB — so the right
        // next job is a function of what just happened, not of an ordering
        // computed before any of it did.
        let policy = self.pressure_policy();
        let mut archived = 0usize;
        let mut consecutive_failures = 0usize;
        loop {
            match disk_used_pct(&p.path).await {
                Some(cur) if cur < p.low_pct => {
                    info!(
                        "disk-pressure: {} down to {cur:.1}% (< {:.1}%) after {archived} \
                         emergency offload(s); standing down",
                        p.path.display(),
                        p.low_pct
                    );
                    crate::events::journal_info(
                        "sweep.pressure",
                        format!(
                            "stood down at {cur:.1}% after {archived} emergency offload(s)"
                        ),
                    );
                    return archived;
                }
                _ => {}
            }
            let Some(job) = self.next_offload_job(policy).await else {
                break;
            };
            let kind = job.kind;
            let schema = job.schema.clone();
            match self.run_offload_job(job).await {
                Ok(()) => {
                    archived += 1;
                    consecutive_failures = 0;
                }
                Err(_) => {
                    // The failure is already logged and journaled by the
                    // operation itself, and the schema is now in backoff — so
                    // the next pick skips it rather than looping on it.
                    consecutive_failures += 1;
                    if consecutive_failures >= SWEEP_MAX_CONSECUTIVE_FAILURES {
                        error!(
                            "disk-pressure: aborting after {consecutive_failures} consecutive \
                             failures (last: {} {schema}) — environment unhealthy; \
                             re-checking in {:?}",
                            kind.as_str(),
                            p.check_interval
                        );
                        crate::events::journal_error(
                            "sweep.pressure",
                            format!(
                                "ABORTED after 3 consecutive failures ({archived} offloaded first)"
                            ),
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
            crate::events::journal_error(
                "sweep.pressure",
                format!(
                    "exhausted all candidates, disk still at {cur:.1}% — eviction alone \
                     cannot relieve this ({archived} archived)"
                ),
            );
        }
        archived
    }

    /// Archive **every** eligible schema now, in the background — the
    /// dashboard's "sweep now" control, and the one place batch behaviour
    /// survives. The offload pacer moves one schema at a time and defers to
    /// client work; this is the operator override for "I want the disk back
    /// now", so it runs the whole candidate list back to back and the pacer
    /// stands down while it does (see [`Self::offload_backpressure`]).
    ///
    /// Returns as soon as the sweep is launched (it can take a long time for a
    /// big backlog); the outcome shows up in the pooler log and the VMs'
    /// "Archived (S3)" status. Errors if the eviction tier isn't configured,
    /// or if a sweep is already running (the sweep itself is single-flighted,
    /// so this only reports it).
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
    /// The operator-triggered path only ([`Self::spawn_sweep_now`]) — routine
    /// eviction is the offload pacer's, one schema at a time. Returns how many
    /// schemas it archived.
    ///
    /// Single-flighted: if a sweep is already in progress this returns
    /// immediately having done nothing, so triggers can't stack overlapping
    /// passes racing over the same candidates.
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
        // Frozen/compacted schemas past the archive threshold get promoted
        // local-file → S3 without any VM (the file already exists on the host).
        let mut frozen_candidates: Vec<String> = Vec::new();
        let mut compact_candidates: Vec<String> = Vec::new();
        let mut total = 0usize;
        let mut backing_off = 0usize;
        let (mut refreshed, mut keepalive, mut already, mut not_idle) = (0usize, 0usize, 0usize, 0usize);
        for (schema, rec) in self.store_records() {
            total += 1;
            let ka = self.cfg.is_keepalive(&schema);
            if rec.tier == Tier::Frozen || rec.tier == Tier::Compacted {
                if !ka && now.saturating_sub(rec.last_active) >= threshold_secs {
                    if rec.tier == Tier::Frozen {
                        frozen_candidates.push(schema);
                    } else {
                        compact_candidates.push(schema);
                    }
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
                // A candidate still in failure backoff is skipped: each retry
                // of a sick schema costs a full wedged bring-up, and three in
                // a row abort the pass for the healthy candidates behind them.
                SweepAction::Archive => {
                    if self.offload_backoff.active(&schema, Instant::now()).is_some() {
                        backing_off += 1;
                    } else {
                        candidates.push(schema);
                    }
                }
            }
        }

        // Always log the evaluation, so a sweep that archives nothing is
        // explained ("all skipped as not-idle") rather than silent — the manual
        // "sweep now" button and the periodic pass both surface here.
        info!(
            "S3 eviction sweep: evaluated {total} schema(s) — {} live candidate(s) + \
             {} frozen + {} compacted promotion(s), {refreshed} refreshed (warm), \
             {backing_off} in failure backoff, skipped {} ({keepalive} keepalive, \
             {already} already archived, {not_idle} idle < {threshold_secs}s)",
            candidates.len(),
            frozen_candidates.len(),
            compact_candidates.len(),
            keepalive + already + not_idle,
        );

        // Promote local files first: cheap (a file upload, no VM), and every
        // success frees local disk.
        let n_frozen = frozen_candidates.len() + compact_candidates.len();
        let mut archived_frozen = 0usize;
        for schema in frozen_candidates {
            match self.archive_frozen_schema(&schema).await {
                Ok(()) => archived_frozen += 1,
                Err(e) => {
                    warn!(
                        "promoting frozen schema {schema} to S3 failed (will retry next sweep): {e:#}"
                    );
                    crate::events::journal_error(
                        "archive",
                        format!("promoting frozen schema {schema} to S3 failed: {e:#}"),
                    );
                }
            }
        }
        for schema in compact_candidates {
            match self.archive_compacted_schema(&schema).await {
                Ok(()) => archived_frozen += 1,
                Err(e) => {
                    warn!(
                        "promoting compacted schema {schema} to S3 failed (will retry next \
                         sweep): {e:#}"
                    );
                    crate::events::journal_error(
                        "archive",
                        format!("promoting compacted schema {schema} to S3 failed: {e:#}"),
                    );
                }
            }
        }

        if candidates.is_empty() {
            // Journal only sweeps that had work; a quiet fleet's no-op passes
            // would drown the events page.
            if n_frozen > 0 {
                crate::events::journal_info(
                    "sweep.archive",
                    format!("promoted {archived_frozen}/{n_frozen} local file(s) to S3"),
                );
            }
            return archived_frozen;
        }
        let n_live = candidates.len();
        let mut archived = archived_frozen;
        let mut consecutive_failures = 0usize;
        let mut aborted = false;
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
                        aborted = true;
                        break;
                    }
                }
            }
        }
        crate::events::journal_info(
            "sweep.archive",
            format!(
                "archived {archived}/{} ({n_live} live + {n_frozen} frozen promotion(s)){}",
                n_live + n_frozen,
                if aborted {
                    " — ABORTED after 3 consecutive failures"
                } else {
                    ""
                }
            ),
        );
        archived
    }

    /// Offload one schema's database to S3 and kill its VM to reclaim the disk.
    /// Also the target of the dashboard's manual "reap" button. Refuses if the
    /// VM has live client sessions. Serializes against [`Self::checkout`] via the
    /// `archiving` set so a client can't bring the VM back up mid-operation.
    ///
    /// Every outcome is journaled for the dashboard's events page — this
    /// wrapper is the single choke point all archive callers (periodic sweep,
    /// pressure reaper, manual reap) go through.
    pub async fn archive_schema(&self, schema: &str) -> Result<()> {
        let res = self.archive_schema_inner(schema).await;
        match &res {
            Ok(()) => {
                self.offload_backoff.clear(schema);
                crate::events::journal_info("archive", format!("schema {schema} → archived (S3)"));
            }
            Err(e) => {
                let (n, delay) = self.offload_backoff.record_failure(schema, Instant::now());
                crate::events::journal_error(
                    "archive",
                    format!(
                        "schema {schema}: {e:#} (failure {n}; sweeps skip this schema for {})",
                        fmt_backoff(delay)
                    ),
                );
            }
        }
        res
    }

    async fn archive_schema_inner(&self, schema: &str) -> Result<()> {
        let Some(archive) = self.cfg.archive.clone() else {
            bail!("S3 eviction tier is not configured (set PG_VM_POOL_ARCHIVE_AFTER_SECS + PG_VM_POOL_S3_*)");
        };
        match self.store.record(schema).map(|r| r.tier) {
            Some(Tier::Archived) => bail!("schema {schema} is already archived to S3"),
            // Frozen: no VM to dump — promote the existing local dump file.
            Some(Tier::Frozen) => return self.archive_frozen_schema(schema).await,
            // Compacted: likewise — promote the existing local image file.
            Some(Tier::Compacted) => return self.archive_compacted_schema(schema).await,
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
        let entry = match vm::ensure_vm(
            &self.cfg,
            schema,
            known_id.as_deref(),
            None,
            None,
            self.owner_of(schema).as_ref(),
        )
        .await {
            Ok(entry) => entry,
            Err(e) => {
                // A bring-up that started the VM but never reached a ready
                // Postgres (ready-timeout on a sick disk) has no handle to
                // clean up through — the same leak class as a failed dump.
                vm::stop_after_failed_bringup(schema, known_id.as_deref()).await;
                // This is exactly the schema the image path exists for: its
                // Postgres won't boot, so no dump will ever succeed — but the
                // disk itself can still be archived. Only the *known* VM's
                // disk qualifies: a fresh VM the failed bring-up may have
                // created holds an empty cluster, and archiving that would
                // durably shadow the real data.
                let e = e.context(format!("bringing up VM for schema {schema} to archive it"));
                return self
                    .image_archive_fallback(schema, known_id.as_deref(), &archive, e)
                    .await;
            }
        };

        // On dump failure, stop the VM this attempt booted before propagating.
        // Without this every failed archive leaks a running VM — nothing else
        // owns it (the warm entry was evicted above, so the idle reaper never
        // sees it), it burns RAM, and it pins its disk open against reclaim.
        // A whole sweep of failures leaks a fleet of them at once. Stop, not
        // kill: the data on its disk is still the only copy.
        let dumps = self
            .dumps
            .as_deref()
            .map(|srv| (srv, self.cfg.dump_net.listen.port()));
        if let Err(e) = vm::dump_to_s3(&self.cfg, &entry.sandbox, schema, &archive.s3, dumps).await {
            warn!("schema {schema}: dump failed; stopping the VM booted for this attempt");
            checkpoint_and_stop(&entry, schema).await;
            // The VM this attempt used is the one whose disk holds the data —
            // the same disk the dump just failed to read a database out of.
            // With the VM checkpointed and stopped, that disk is exactly what
            // the image path archives.
            let sb_id = entry.sandbox.sandbox_id().to_string();
            drop(entry);
            let e = e.context(format!("dumping schema {schema} to S3"));
            return self
                .image_archive_fallback(schema, Some(&sb_id), &archive, e)
                .await;
        }

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
        let key = archive.s3.object_key(schema);
        let http = reqwest::Client::builder()
            .build()
            .context("building HTTP client for S3 upload")?;
        // HEAD first so a wrong-region bucket is discovered (and latched)
        // before anything is presigned.
        let _ = archive.s3.head_object(&http, &key, Duration::from_secs(10)).await;
        // Single PUT for small dumps, 64MB multipart above 100MB — memory is
        // bounded by the part size, so there is no pooler-side cap on how
        // large a frozen dump can be promoted (there used to be a 512MB one,
        // which silently pinned oversized dumps to the local disk forever).
        crate::imgarchive::upload_path(&archive.s3, &http, &key, &path, len)
            .await
            .with_context(|| format!("uploading {} to s3://{}/{key}", path.display(), archive.s3.bucket))?;
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

    /// Promote a compacted schema's local image file to S3 — no VM, no
    /// recompression: the file already IS the archive format the image tier
    /// stores (`{schema}.img.zst`), so this is an upload + verify + tier
    /// flip + local-file cleanup. The compacted twin of the frozen-dump
    /// promotion ([`Self::archive_frozen_schema`]).
    async fn archive_compacted_schema(&self, schema: &str) -> Result<()> {
        let Some(archive) = self.cfg.archive.clone() else {
            bail!("S3 eviction tier is not configured (set PG_VM_POOL_ARCHIVE_AFTER_SECS + PG_VM_POOL_S3_*)");
        };
        let Some(compact) = self.cfg.compact.clone() else {
            bail!("schema {schema} is compacted but the compacted tier is not configured");
        };
        let _guard = match ArchivingGuard::claim(&self.archiving, schema) {
            Some(g) => g,
            None => bail!("schema {schema} is already being archived"),
        };
        let path = compact.compact_path(schema);
        let len = crate::imgarchive::promote_compact(&archive.s3, schema, &path)
            .await
            .with_context(|| format!("promoting compacted schema {schema} to S3"))?;
        self.store.set_tier(schema, Tier::Archived).await;
        if let Err(e) = tokio::fs::remove_file(&path).await {
            warn!("schema {schema}: promoted to S3 but deleting {} failed: {e}", path.display());
        }
        info!(
            "schema {schema}: compacted image promoted to s3://{}/{} ({len} bytes), local \
             file removed",
            archive.s3.bucket,
            archive.s3.image_object_key(schema),
        );
        Ok(())
    }

    /// Try the image-level archive after a failed dump-based attempt.
    /// `Ok(())` means the schema is durably archived (as an image); otherwise
    /// the original dump error comes back — annotated with the image failure
    /// when an attempt was actually made. A missing sandbox id or a disabled
    /// image tier just propagates the original error untouched.
    async fn image_archive_fallback(
        &self,
        schema: &str,
        sandbox_id: Option<&str>,
        archive: &crate::config::ArchiveConfig,
        original: anyhow::Error,
    ) -> Result<()> {
        if self.cfg.image_archive.is_none() {
            return Err(original);
        }
        let Some(id) = sandbox_id else {
            return Err(original);
        };
        info!(
            "schema {schema}: dump-based archive failed; falling back to a \
             disk-image archive of {id}"
        );
        match self.image_archive_now(schema, id, archive).await {
            Ok(()) => Ok(()),
            Err(img_e) => Err(original.context(format!(
                "the disk-image fallback also failed: {img_e:#}"
            ))),
        }
    }

    /// The image-archive core: archive the disk, flip the tier, kill the VM,
    /// journal. The caller must hold the schema's `ArchivingGuard` and have
    /// stopped the VM (a still-running one fails the disk-release wait).
    async fn image_archive_now(
        &self,
        schema: &str,
        sandbox_id: &str,
        archive: &crate::config::ArchiveConfig,
    ) -> Result<()> {
        let done =
            crate::imgarchive::archive_disk(&self.cfg, &archive.s3, schema, sandbox_id).await?;
        // Mark before kill, mirroring the dump path: if we crash between them
        // the image is durable and the store says "archived", so the next
        // connect restores — the reverse order could lose the mapping to a
        // killed VM.
        //
        // An unbound VM (on the daemon but not in the registry — incident-era
        // strays) needs its row *created* first: `set_tier` on a missing row
        // is a silent no-op, and an archive the registry doesn't know about
        // is unreachable — the next connect would serve a fresh empty DB.
        // Crash windows stay safe: after `put` alone the schema is live and
        // its VM still exists (the kill hasn't run), so a connect just boots
        // it; the image in S3 is redundant, never load-bearing.
        if self.store.record(schema).is_none() {
            self.store.put(schema, sandbox_id);
        }
        self.store.set_tier(schema, Tier::Archived).await;
        if let Err(e) = kill_by_id(sandbox_id).await {
            warn!(
                "schema {schema}: image-archived but killing VM {sandbox_id} failed \
                 (orphaned): {e:#}"
            );
        } else if let Some(dir) = self.cfg.run_dir.as_ref().map(|d| d.join(sandbox_id)) {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if dir.exists() {
                warn!(
                    "schema {schema}: image-archived and VM killed, but {} still \
                     exists — the orphan sweep will reclaim it",
                    dir.display()
                );
            }
        }
        // The pgdata version travels with an image (unlike a dump), so a
        // restore needs a matching-major rootfs — put the version on the
        // record now, not when a restore fails on it.
        crate::events::journal_info(
            "archive.image",
            format!(
                "schema {schema} → archived as disk image ({}, pgdata v{})",
                crate::orphans::human_iec(done.bytes),
                done.pg_version.as_deref().unwrap_or("unknown"),
            ),
        );
        Ok(())
    }

    /// Manually archive `schema` as a disk image — the dashboard's per-VM
    /// "archive as image" action. Unlike the sweep's fallback this doesn't
    /// wait for a failed dump: it stops the VM (checkpointing through the
    /// warm entry when there is one) and archives the disk directly. For a
    /// schema whose Postgres is known-unbootable, this skips the futile
    /// minutes of bring-up the dump path would burn first.
    ///
    /// `viewed_id` is the sandbox the dashboard button was pressed on. It
    /// lets a VM the registry has *no row for* (an incident-era stray: a
    /// rescued ghost, a duplicate from the data-loss race, a row lost while
    /// the disk was full) be archived and adopted — the row is created from
    /// the verified archive, so the schema restores like any other. When the
    /// registry *does* know the schema under a different id, this refuses
    /// and names both VMs: two disks claim the same schema and only a human
    /// knows which holds the truth.
    pub async fn archive_schema_as_image(&self, schema: &str, viewed_id: Option<&str>) -> Result<()> {
        let res = self.archive_schema_as_image_inner(schema, viewed_id).await;
        match &res {
            Ok(()) => self.offload_backoff.clear(schema),
            Err(e) => {
                crate::events::journal_error("archive.image", format!("schema {schema}: {e:#}"));
            }
        }
        res
    }

    async fn archive_schema_as_image_inner(&self, schema: &str, viewed_id: Option<&str>) -> Result<()> {
        let Some(archive) = self.cfg.archive.clone() else {
            bail!(
                "S3 eviction tier is not configured (set PG_VM_POOL_ARCHIVE_AFTER_SECS + \
                 PG_VM_POOL_S3_*)"
            );
        };
        anyhow::ensure!(
            self.cfg.image_archive.is_some(),
            "image archiving is not enabled (set PG_VM_POOL_IMAGE_ARCHIVE=1 + PG_VM_POOL_RUN_DIR)"
        );
        match self.store.record(schema).map(|r| r.tier) {
            Some(Tier::Archived) => bail!("schema {schema} is already archived to S3"),
            Some(Tier::Frozen) => bail!(
                "schema {schema} is frozen to a local dump — the archive sweep promotes that \
                 to S3; there is no VM disk to image"
            ),
            Some(Tier::Compacted) => bail!(
                "schema {schema} is already compacted to a local image — the archive sweep \
                 promotes that to S3; there is no VM disk to image"
            ),
            _ => {}
        }
        let recorded = self.store.record(schema).map(|r| r.sandbox_id);
        let (id, adopting) =
            resolve_image_target(schema, recorded.as_deref(), viewed_id)?;
        if adopting {
            info!(
                "schema {schema}: VM {id} is not in the registry — archiving its disk \
                 and adopting the schema from the verified image"
            );
        }
        let _guard = match ArchivingGuard::claim(&self.archiving, schema) {
            Some(g) => g,
            None => bail!("schema {schema} is already being archived"),
        };
        // Evict the warm entry under the map lock, refusing live sessions —
        // same serialization against checkout as the dump path.
        let warm = {
            let mut map = self.entries.lock().await;
            let warm = map.get(schema).and_then(|c| c.get()).cloned();
            if let Some(entry) = &warm {
                let active = entry.active_count();
                if active > 0 {
                    bail!("schema {schema} has {active} live session(s); refusing to archive");
                }
            }
            map.remove(schema);
            warm
        };
        match warm {
            // A warm entry means a live pool on a running VM: checkpoint
            // through it, then stop — the image should carry as little
            // unreplayed WAL as possible.
            Some(entry) => checkpoint_and_stop(&entry, schema).await,
            // No warm entry, but the VM may still be running (idle, or left
            // over from before a pooler restart): best-effort stop by id.
            None => {
                if let Ok(sb) = heyo_sdk::Sandbox::connect(id.clone(), vm::local_opts()) {
                    match tokio::time::timeout(Duration::from_secs(30), sb.stop()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => info!(
                            "schema {schema}: pre-image stop of {id}: {e:#} \
                             (it may already be stopped)"
                        ),
                        Err(_) => warn!("schema {schema}: pre-image stop of {id} timed out"),
                    }
                }
            }
        }
        self.image_archive_now(schema, &id, &archive).await
    }

    /// Kick off one purge pass now, in the background — the dashboard's
    /// double-opt-in "purge" button. Deletes sandboxes that are pure waste:
    ///
    /// - VMs still backing schemas whose tier is `frozen`/`archived` — the
    ///   tier flip is only ever written after a size-verified durable dump,
    ///   so these are leftovers of a failed post-offload kill;
    /// - unclaimed spares: spare-named sandboxes neither bound to a schema in
    ///   the registry nor mid-claim in this process (an empty initdb cluster
    ///   by construction). A spare being restored into when the pooler last
    ///   crashed can look unclaimed and get deleted — its source dump is
    ///   still durable, so the restore just retries; never data loss.
    ///
    /// Errors if a purge is already running.
    pub fn spawn_purge_now(self: &Arc<Self>) -> Result<()> {
        if self.purging.swap(true, Ordering::SeqCst) {
            bail!("a purge is already running");
        }
        let registry = self.clone();
        tokio::spawn(async move {
            let _guard = SweepGuard(&registry.purging);
            registry.purge_pass().await;
        });
        Ok(())
    }

    async fn purge_pass(&self) {
        let infos = match heyo_sdk::Sandbox::list(vm::local_opts()).await {
            Ok(l) => l,
            Err(e) => {
                crate::events::journal_error("purge", format!("listing sandboxes failed: {e:#}"));
                return;
            }
        };
        crate::inventory::absorb(&infos);
        let live_ids: HashSet<String> = infos.iter().map(|s| s.id.clone()).collect();
        let bound: HashSet<String> = self
            .store_records()
            .into_iter()
            .map(|(_, r)| r.sandbox_id)
            .collect();
        let claimed = self
            .spares
            .as_ref()
            .map(|p| p.claimed_ids())
            .unwrap_or_default();

        let (mut offloaded, mut spares, mut failed) = (0usize, 0usize, 0usize);
        // 1. Leftover VMs of durably offloaded schemas.
        for (schema, rec) in self.store_records() {
            if rec.tier == Tier::Live || !live_ids.contains(&rec.sandbox_id) {
                continue;
            }
            info!("purge: schema {schema} is {} but VM {} still exists — deleting",
                rec.tier.as_str(), rec.sandbox_id);
            match kill_by_id(&rec.sandbox_id).await {
                Ok(()) => offloaded += 1,
                Err(e) => {
                    failed += 1;
                    warn!("purge: deleting {} failed: {e:#}", rec.sandbox_id);
                }
            }
        }
        // 2. Unclaimed spares.
        for s in &infos {
            if !s.name.starts_with(crate::spares::SPARE_PREFIX)
                || bound.contains(&s.id)
                || claimed.contains(&s.id)
            {
                continue;
            }
            info!("purge: unclaimed spare {} — deleting", s.id);
            match kill_by_id(&s.id).await {
                Ok(()) => spares += 1,
                Err(e) => {
                    failed += 1;
                    warn!("purge: deleting spare {} failed: {e:#}", s.id);
                }
            }
        }
        let msg = format!(
            "deleted {offloaded} offloaded-schema VM(s) + {spares} unclaimed spare(s){}",
            if failed > 0 {
                format!(", {failed} failed")
            } else {
                String::new()
            }
        );
        info!("purge: {msg}");
        crate::events::journal_info("purge", msg);
    }

    // ---- local frozen tier --------------------------------------------------

    /// Start the local dump HTTP server. Serves guest uploads and downloads
    /// for the frozen tier AND the S3 archive tier's streamed dumps; without
    /// it, freezing, thawing, and streamed archiving are inert.
    pub fn spawn_dump_server(self: &Arc<Self>) {
        let Some(srv) = self.dumps.clone() else {
            return;
        };
        let listen = self.cfg.dump_net.listen;
        tokio::spawn(async move {
            if let Err(e) = srv.serve(listen).await {
                error!(
                    "local dump server exited: {e:#} — freezing, thawing, and streamed \
                     archive dumps are down"
                );
            }
        });
    }

    /// Dump one schema to the local dump store and delete its VM. The frozen
    /// twin of [`Self::archive_schema`], with the same guards: `archiving`
    /// claim (checkouts wait), live-session refusal, dump verified complete
    /// (size-checked by `dump_to_local`) *before* the durable tier flip, and
    /// the tier flip durable *before* the kill.
    ///
    /// Journaled like [`Self::archive_schema`], as the choke point for all
    /// freeze callers.
    pub async fn freeze_schema(&self, schema: &str) -> Result<()> {
        let res = self.freeze_schema_inner(schema).await;
        match &res {
            Ok(()) => {
                self.offload_backoff.clear(schema);
                crate::events::journal_info(
                    "freeze",
                    format!("schema {schema} → frozen (local dump)"),
                );
            }
            Err(e) => {
                let (n, delay) = self.offload_backoff.record_failure(schema, Instant::now());
                crate::events::journal_error(
                    "freeze",
                    format!(
                        "schema {schema}: {e:#} (failure {n}; sweeps skip this schema for {})",
                        fmt_backoff(delay)
                    ),
                );
            }
        }
        res
    }

    async fn freeze_schema_inner(&self, schema: &str) -> Result<()> {
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
        let entry = match vm::ensure_vm(
            &self.cfg,
            schema,
            known_id.as_deref(),
            None,
            None,
            self.owner_of(schema).as_ref(),
        )
        .await {
            Ok(entry) => entry,
            Err(e) => {
                // Same leak guard as archive_schema_inner's bring-up.
                vm::stop_after_failed_bringup(schema, known_id.as_deref()).await;
                return Err(e)
                    .with_context(|| format!("bringing up VM for schema {schema} to freeze it"));
            }
        };

        // Same leak guard as archive_schema_inner: a failed dump must not
        // leave the VM it booted running and unowned.
        let bytes = match vm::dump_to_local(
            &self.cfg,
            &entry.sandbox,
            schema,
            &dumps,
            freeze.listen.port(),
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!("schema {schema}: dump failed; stopping the VM booted for this attempt");
                checkpoint_and_stop(&entry, schema).await;
                return Err(e)
                    .with_context(|| format!("dumping schema {schema} to the local dump store"));
            }
        };

        self.store.set_tier(schema, Tier::Frozen).await;
        let accomplished = format!("frozen to a {bytes}-byte local dump");
        if let Err(e) = vm::kill_and_reclaim(&self.cfg, &entry.sandbox, schema, &accomplished).await {
            warn!("schema {schema}: frozen locally but killing the VM failed (orphaned): {e:#}");
        }
        Ok(())
    }

    // ---- compacted tier -----------------------------------------------------

    /// Compact one schema's stopped disk to a local image and delete its VM —
    /// the compacted twin of [`Self::freeze_schema`], with the same guards:
    /// `archiving` claim (checkouts wait), live-session refusal, image
    /// verified complete (zstd -t + ext4 magic, atomic rename) *before* the
    /// durable tier flip, and the tier flip durable *before* the kill.
    pub async fn compact_schema(&self, schema: &str) -> Result<()> {
        let res = self.compact_schema_inner(schema).await;
        match &res {
            Ok(()) => {
                self.offload_backoff.clear(schema);
                crate::events::journal_info(
                    "compact",
                    format!("schema {schema} → compacted (local image)"),
                );
            }
            Err(e) => {
                let (n, delay) = self.offload_backoff.record_failure(schema, Instant::now());
                crate::events::journal_error(
                    "compact",
                    format!(
                        "schema {schema}: {e:#} (failure {n}; sweeps skip this schema for {})",
                        fmt_backoff(delay)
                    ),
                );
            }
        }
        res
    }

    async fn compact_schema_inner(&self, schema: &str) -> Result<()> {
        let Some(compact) = self.cfg.compact.clone() else {
            bail!("compacted tier is not configured (set PG_VM_POOL_COMPACT_AFTER_SECS)");
        };
        if self.store.record(schema).map(|r| r.offloaded()).unwrap_or(false) {
            bail!("schema {schema} is already compacted, frozen, or archived");
        }
        let _guard = match ArchivingGuard::claim(&self.archiving, schema) {
            Some(g) => g,
            None => bail!("schema {schema} is already being offloaded"),
        };
        {
            let mut map = self.entries.lock().await;
            if let Some(entry) = map.get(schema).and_then(|c| c.get()) {
                // A warm entry appeared since candidate selection: the VM may
                // be running (disk open). Leave it to the idle reaper.
                let active = entry.active_count();
                bail!(
                    "schema {schema} has a warm VM ({active} session(s)); refusing to compact"
                );
            }
            map.remove(schema);
        }
        let Some(rec) = self.store.record(schema) else {
            bail!("schema {schema} has no registry row — nothing to compact");
        };

        // Image the stopped disk. compact_disk's fd-scan wait refuses a disk
        // anything still holds open, so a VM started out-of-band fails this
        // rather than being imaged mid-write.
        let bytes =
            crate::imgarchive::compact_disk(&self.cfg, &compact, schema, &rec.sandbox_id)
                .await
                .with_context(|| format!("compacting schema {schema}'s data disk"))?;

        // Tier flip durable before the kill (crash between them = data safely
        // in the compact file, row says compacted, next connect thaws it —
        // the VM/disk becomes purge/orphan fodder, not data loss).
        self.store.set_tier(schema, Tier::Compacted).await;
        let accomplished = format!(
            "compacted to a {}-byte local image",
            bytes
        );
        match Sandbox::connect(rec.sandbox_id.clone(), vm::local_opts()) {
            Ok(sb) => {
                if let Err(e) = vm::kill_and_reclaim(&self.cfg, &sb, schema, &accomplished).await {
                    warn!("schema {schema}: compacted but killing the VM failed (orphaned): {e:#}");
                }
            }
            Err(e) => warn!(
                "schema {schema}: compacted but connecting to VM {} to kill it failed \
                 (orphaned): {e:#}",
                rec.sandbox_id
            ),
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

    /// Reap bring-ups that started (the daemon accepted a create and handed out
    /// an id — recorded in the pending ledger) but never resolved into a
    /// registry binding: pooler died mid-bring-up, or the failure-path kill
    /// couldn't reach the daemon. These VMs are the "stuck in `provisioning`,
    /// bound to nothing" records no other sweep can touch — purge needs a
    /// registry row or a spare name, the orphan-disk sweep needs the daemon to
    /// have forgotten the id. Always on: the ledger only has entries if
    /// bring-ups actually leaked.
    pub fn spawn_pending_janitor(self: &Arc<Self>) {
        let registry = self.clone();
        info!(
            "pending-bringup janitor: reaping unresolved bring-ups every {:?}",
            PENDING_JANITOR_TICK
        );
        tokio::spawn(supervise(
            "pending-janitor",
            PENDING_JANITOR_FIRST_DELAY,
            PENDING_JANITOR_TICK,
            move || {
                let registry = registry.clone();
                async move { registry.pending_pass().await }
            },
        ));
    }

    /// One janitor pass; returns the number of entries resolved. An entry is
    /// only acted on well after any legitimate bring-up would have finished,
    /// and deletion needs the daemon to positively confirm the record — the
    /// same ambiguity-never-deletes rule as the orphan-disk sweep.
    async fn pending_pass(&self) -> usize {
        // Twice the ready budget plus slack: a slow-but-alive bring-up (ready
        // wait + restore) must never race its own janitor.
        let min_age = self.cfg.ready_timeout * 2 + Duration::from_secs(300);
        let stale = crate::pending::stale(min_age);
        if stale.is_empty() {
            return 0;
        }
        let mut resolved = 0usize;
        for (schema, id) in stale {
            // Bound after all — the bring-up won and only the ledger clear was
            // lost (e.g. a crash between store.put and clear). Settled.
            if self
                .store
                .record(&schema)
                .is_some_and(|r| r.sandbox_id == id)
            {
                crate::pending::clear(&schema).await;
                resolved += 1;
                continue;
            }
            // A bring-up for this schema is in flight right now (cell present
            // but uninitialized) — it may be reattaching to this very VM by
            // name; hands off until it settles.
            {
                let map = self.entries.lock().await;
                if map.get(&schema).is_some_and(|cell| cell.get().is_none()) {
                    continue;
                }
            }
            match self.daemon_state(&id).await {
                DaemonState::Gone => {
                    // Deleted out-of-band (or our failure-path kill did land) —
                    // if a disk dir lingers, the orphan-disk sweep owns it now
                    // that the daemon reports the id gone.
                    crate::pending::clear(&schema).await;
                    resolved += 1;
                }
                DaemonState::Present { .. } => match kill_by_id(&id).await {
                    Ok(()) => {
                        let msg = format!(
                            "schema {schema}: deleted VM {id} stranded by a failed bring-up \
                             (never bound to the registry)"
                        );
                        info!("pending-bringup janitor: {msg}");
                        crate::events::journal_info("pending-janitor", msg);
                        crate::pending::clear(&schema).await;
                        resolved += 1;
                    }
                    Err(e) => warn!(
                        "pending-bringup janitor: deleting stranded VM {id} \
                         (schema {schema}) failed; retrying next pass: {e:#}"
                    ),
                },
                // Daemon down or flaking: ambiguity never deletes.
                DaemonState::Error => {}
            }
        }
        resolved
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
    ///
    /// The same pass also **prunes leftover boot artefacts** from directories
    /// it is keeping. heyvmd clones the base image into `<dir>/rootfs.ext4`
    /// on every boot and deletes it on a clean stop, so a copy sitting next to
    /// a *stopped* VM's data disk is the residue of an unclean one — a daemon
    /// restart, a watchdog kill, a host reboot. It is ~200MB per VM, nothing
    /// reads it (the next boot overwrites it), and on a large fleet it is the
    /// single biggest reclaimable item in the run dir. Deleting it needs the
    /// same in-use and age guards as a directory, plus the daemon reporting the
    /// VM not running; the cost of being wrong is one extra image clone.
    async fn sweep_orphans(&self) -> usize {
        let Some(run_dir) = self.cfg.run_dir.clone() else {
            return 0;
        };
        if self.orphan_sweeping.swap(true, Ordering::SeqCst) {
            info!("orphan-disk sweep: a pass is already running; skipping");
            return 0;
        }
        let _guard = SweepGuard(&self.orphan_sweeping);

        // The restore scratch dir is dot-named, so the sb-* scan below never
        // sees it — GC its crash leftovers here, on the sweep's cadence.
        crate::imgarchive::gc_restore_scratch(&run_dir);

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
        let mut stale_rootfs: Vec<(PathBuf, String)> = Vec::new(); // (rootfs path, id)
        let (mut alive, mut held, mut daemon_errs) = (0usize, 0usize, 0usize);
        let mut aborted = false;
        for (path, id) in dirs {
            if crate::orphans::dir_held_open(&path, &open) {
                held += 1;
                continue;
            }
            match self.daemon_state(&id).await {
                DaemonState::Present { running } => {
                    alive += 1;
                    // The VM is stopped (or the daemon says so while nothing
                    // holds its files open, which amounts to the same thing):
                    // any rootfs clone left in its directory is dead weight.
                    if !running {
                        let rootfs = path.join(VM_ROOTFS_FILE);
                        if file_older_than(&rootfs, ORPHAN_MIN_AGE) {
                            stale_rootfs.push((rootfs, id));
                        }
                    }
                }
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
        let rootfs_backlog = stale_rootfs
            .len()
            .saturating_sub(ORPHAN_MAX_ROOTFS_PRUNES_PER_SWEEP);
        stale_rootfs.truncate(ORPHAN_MAX_ROOTFS_PRUNES_PER_SWEEP);
        info!(
            "orphan-disk sweep: {} deletable, {} stale rootfs cop(ies), {alive} live, {held} in \
             use, {} data-loss orphan(s), {too_new} too new{}{}",
            deletable.len() + backlog,
            stale_rootfs.len() + rootfs_backlog,
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

        // Boot-artefact prune. Every guard re-confirmed per file: nothing in
        // the directory is open, the daemon still reports the VM not running,
        // and the file itself is still old. A VM that booted since
        // classification fails all three — the boot re-clones the rootfs, so
        // even the mtime alone gives it away.
        let (mut pruned, mut pruned_bytes) = (0usize, 0u64);
        for (rootfs, id) in stale_rootfs {
            let Some(dir) = rootfs.parent() else { continue };
            if crate::orphans::dir_held_open(dir, &open) {
                continue;
            }
            if !file_older_than(&rootfs, ORPHAN_MIN_AGE) {
                continue;
            }
            if !matches!(self.daemon_state(&id).await, DaemonState::Present { running: false }) {
                continue;
            }
            let bytes = crate::orphans::file_allocated_bytes(&rootfs);
            match std::fs::remove_file(&rootfs) {
                Ok(()) => {
                    pruned += 1;
                    pruned_bytes += bytes;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(
                    "orphan-disk sweep: pruning stale rootfs {} failed: {e}",
                    rootfs.display()
                ),
            }
        }
        if pruned > 0 {
            info!(
                "orphan-disk sweep: pruned {pruned} stale rootfs cop(ies) of stopped VMs, {} \
                 freed (each is re-cloned from the base image on that VM's next boot)",
                crate::orphans::human_iec(pruned_bytes),
            );
        }
        removed + pruned
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
            Ok(info) => DaemonState::Present {
                running: info.status == heyo_sdk::SandboxStatus::Running,
            },
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

/// Does `path` exist and has it been untouched for at least `age`? False for a
/// missing file, and false when the mtime can't be read or lies in the future —
/// the callers use this as a "safe to delete" gate, so unknown means no.
fn file_older_than(path: &std::path::Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|md| md.modified())
        .ok()
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .is_some_and(|elapsed| elapsed >= age)
}

/// One GiB, for device-size math.
const GIB: u64 = 1024 * 1024 * 1024;

/// Per-schema failure memory for offloads, so the archive/freeze/pressure
/// sweeps stop re-trying the same sick schemas every pass. Each failed
/// attempt on a schema costs up to a full ready-timeout (~5 min of wedged
/// bring-up), and the sweeps' consecutive-failure breaker means a handful of
/// permanently sick schemas can abort pass after pass — starving every
/// healthy candidate behind them. With backoff, a failed schema is skipped by
/// the sweeps for an exponentially growing window (30m, 1h, 2h … capped at
/// 24h) and retried after; any success clears it. Deliberately NOT consulted
/// by the dashboard's manual per-schema reap — a human clicking the button is
/// an explicit retry (and its success resets the backoff).
///
/// In-memory only: a pooler restart retries everything once, which is the
/// behavior you want after deploying a fix.
struct OffloadBackoff {
    map: StdMutex<HashMap<String, (u32, Instant)>>,
}

impl OffloadBackoff {
    fn new() -> Self {
        Self {
            map: StdMutex::new(HashMap::new()),
        }
    }

    /// Record one failure at `now`; returns (consecutive failures, how long
    /// sweeps will now skip this schema).
    fn record_failure(&self, schema: &str, now: Instant) -> (u32, Duration) {
        let mut map = self.map.lock().unwrap();
        let entry = map.entry(schema.to_string()).or_insert((0, now));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = now;
        (entry.0, offload_backoff_delay(entry.0))
    }

    /// A success (or a completed restore) forgets the schema's failures.
    fn clear(&self, schema: &str) {
        self.map.lock().unwrap().remove(schema);
    }

    /// Time remaining in `schema`'s backoff window at `now`, if any.
    fn active(&self, schema: &str, now: Instant) -> Option<Duration> {
        let map = self.map.lock().unwrap();
        let (failures, last) = map.get(schema)?;
        offload_backoff_delay(*failures).checked_sub(now.saturating_duration_since(*last))
            .filter(|d| !d.is_zero())
    }
}

/// 30m, 1h, 2h, 4h, … capped at [`OFFLOAD_BACKOFF_CAP`].
fn offload_backoff_delay(failures: u32) -> Duration {
    let exp = failures.saturating_sub(1).min(6); // 2^6 * 30m > cap already
    OFFLOAD_BACKOFF_BASE
        .saturating_mul(1u32 << exp)
        .min(OFFLOAD_BACKOFF_CAP)
}

/// Compact duration for backoff messages: "30m", "2h".
fn fmt_backoff(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h", s.div_ceil(3600))
    } else {
        format!("{}m", s.div_ceil(60))
    }
}

/// Sample a warm VM's data filesystem (total, used, avail bytes via `df` over
/// the pool, like `guest_stats`) plus its backing *device* size (sysfs sector
/// count × 512 via `pg_read_file` — the same source the daemon's own resize
/// verification reads in-guest). `None` when either read fails within the
/// stats timeout; the caller just skips growth this cycle.
async fn sample_disk(entry: &SchemaEntry) -> Option<((u64, u64, u64), u64)> {
    let query = async {
        let mut client = entry.pool.get().await.ok()?;
        // Explicit (offset, length): sysfs files stat as 0 bytes, so the
        // whole-file form of pg_read_file reads nothing (same as /proc).
        let sectors: String = client
            .query_one(
                "SELECT pg_read_file('/sys/class/block/vdb/size', 0, 64)",
                &[],
            )
            .await
            .ok()?
            .get(0);
        let device = sectors.trim().parse::<u64>().ok()?.checked_mul(512)?;
        let fs = df_data_dir(&mut client).await?;
        Some((fs, device))
    };
    tokio::time::timeout(STATS_TIMEOUT, query).await.ok()?
}

/// Decide whether (and to what) an idle-stopping VM's data device should
/// grow. `fs` is the guest data filesystem's (total, used, avail) and
/// `device_bytes` its backing device size.
///
/// Grows only when BOTH hold:
/// - used% (df semantics: used / (used + avail)) is at/above the trigger, and
/// - the filesystem spans (>= 90% of) the device — a thin fs below its cap is
///   the guest grow-watcher's job (it extends the fs online long before the
///   device matters); the device is only the binding constraint once the fs
///   has nowhere left to grow inside it.
///
/// The step doubles the device (whole GiB, current size rounded up), capped
/// at `cfg.max_gb` — the same amortization policy as the guest fs watcher.
fn grow_target_gb(fs: (u64, u64, u64), device_bytes: u64, cfg: &DiskGrowConfig) -> Option<u64> {
    let (total, used, avail) = fs;
    if used_pct(used, avail)? < cfg.pct {
        return None;
    }
    if total < device_bytes.saturating_mul(9) / 10 {
        return None;
    }
    let current_gb = device_bytes.div_ceil(GIB).max(1);
    if current_gb >= cfg.max_gb {
        return None;
    }
    Some((current_gb * 2).min(cfg.max_gb))
}

/// Permanently delete sandbox `id` (kill = sandbox + disk; the SDK treats an
/// already-gone sandbox as success).
/// Which VM a manual image archive should target. `recorded` is the registry
/// binding; `viewed` is the VM the dashboard button was pressed on. Returns
/// `(sandbox_id, adopting)`, where `adopting` means the registry has no row
/// for the schema — the caller creates one from the verified archive. A
/// registry binding that contradicts the viewed VM is refused outright: two
/// disks claim the same schema, only one holds the real data, and choosing
/// automatically could archive (and then purge) the wrong bytes.
fn resolve_image_target(
    schema: &str,
    recorded: Option<&str>,
    viewed: Option<&str>,
) -> Result<(String, bool)> {
    match (recorded, viewed) {
        (Some(rec), Some(v)) if rec != v => bail!(
            "schema {schema} is bound to VM {rec} in the registry, but this is VM {v} — \
             two VMs claim this schema and only one disk holds the real data; compare \
             them (or archive from {rec}'s detail page) before touching either"
        ),
        (Some(rec), _) => Ok((rec.to_string(), false)),
        (None, Some(v)) => Ok((v.to_string(), true)),
        (None, None) => bail!(
            "schema {schema} has no VM in the registry and no VM was named — nothing to image"
        ),
    }
}

async fn kill_by_id(id: &str) -> Result<()> {
    let sb = heyo_sdk::Sandbox::connect(id.to_string(), vm::local_opts())
        .context("connecting to sandbox")?;
    sb.kill().await.context("killing sandbox")?;
    crate::inventory::remove_id(id);
    Ok(())
}

/// Best-effort CHECKPOINT over the warm pool, then stop the VM.
///
/// `sandbox.stop()` is an unclean power-off (Postgres never sees a shutdown
/// signal), and the VMs run `synchronous_commit=off` — so without the
/// checkpoint, up to the last ~600ms of acked commits ride on luck and every
/// restart replays WAL back to the previous checkpoint. One CHECKPOINT
/// flushes everything acked and empties the replay queue, making the next
/// boot's recovery a no-op. Best-effort throughout: if the checkpoint fails
/// or times out we stop anyway (crash recovery handles it — that's the
/// design), and a failed stop is logged, not propagated.
///
/// Used by the idle reaper and by the archive/freeze failure paths (a failed
/// dump must not leak the VM it booted).
async fn checkpoint_and_stop(entry: &SchemaEntry, schema: &str) {
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
        warn!("failed to stop VM for schema {schema}: {e:#}");
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
    make_pass: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = usize> + Send + 'static,
{
    supervise_with_wake(name, first_delay, tick, None, make_pass).await
}

/// [`supervise`] with an optional early-wake handle: a `notify_one` on `wake`
/// ends the current inter-pass wait immediately (or, if it lands mid-pass,
/// the next one — `Notify` stores the permit). Passes stay serialized through
/// this single loop, so an early wake can never race a tick into concurrent
/// passes.
async fn supervise_with_wake<F, Fut>(
    name: &'static str,
    first_delay: Duration,
    tick: Duration,
    wake: Option<Arc<tokio::sync::Notify>>,
    mut make_pass: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = usize> + Send + 'static,
{
    let mut passes: u64 = 0;
    let mut actions: u64 = 0;
    let mut last_beat: Option<Instant> = None;
    loop {
        let delay = if passes == 0 { first_delay } else { tick };
        match &wake {
            Some(n) => {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = n.notified() => {}
                }
            }
            None => tokio::time::sleep(delay).await,
        }
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
    /// The daemon knows this sandbox. `running` distinguishes a live VM (whose
    /// files are in use) from a stopped record (whose boot-time artefacts are
    /// dead weight — see the rootfs prune in [`SchemaRegistry::sweep_orphans`]).
    Present { running: bool },
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

/// One unit of offload work: move this schema one step down the ladder.
#[derive(Debug, PartialEq)]
struct OffloadJob {
    schema: String,
    kind: OffloadKind,
}

/// One step down the storage ladder.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum OffloadKind {
    /// Upload an already-offloaded schema's local file to S3 and delete it.
    /// No VM, no dump — a file upload that frees local disk outright.
    Promote,
    /// Image a stopped VM's disk to a local zstd file and delete the VM.
    /// Seconds of CPU, no boot, and it frees the whole ext4.
    Compact,
    /// Trim + compress a stopped VM's disk straight to S3 and delete the VM.
    /// Like `Compact` in cost and in needing no boot, but it leaves nothing
    /// behind on the host at all. Pressure-only, and only where the image
    /// tier is configured.
    ImageArchive,
    /// Dump a schema to S3 and delete its VM. Frees the most, costs the most
    /// (boot + `pg_dump` + upload).
    Archive,
    /// Dump a schema to a local file and delete its VM. Same cost as an
    /// archive but keeps the bytes on this host.
    Freeze,
}

impl OffloadKind {
    fn as_str(self) -> &'static str {
        match self {
            OffloadKind::Promote => "promoting",
            OffloadKind::Compact => "compacting",
            OffloadKind::ImageArchive => "image-archiving",
            OffloadKind::Archive => "archiving",
            OffloadKind::Freeze => "freezing",
        }
    }

    /// Preference order (lower wins) when several schemas are eligible at
    /// once. It differs by mode because the two modes optimise for different
    /// things — see [`OffloadMode`].
    fn rank(self, mode: OffloadMode) -> u8 {
        match mode {
            // Routine: cheapest-first. A promotion is a file upload that frees
            // local disk outright, so it goes before anything that touches a
            // VM; a dump-based archive beats a freeze because it gets the
            // bytes off the host.
            OffloadMode::Routine => match self {
                OffloadKind::Promote => 0,
                OffloadKind::Compact => 1,
                OffloadKind::ImageArchive => 2,
                OffloadKind::Archive => 3,
                OffloadKind::Freeze => 4,
            },
            // Pressure: bytes-per-second-of-work, and never boot a VM if
            // there is any alternative. Compacting frees ~96% of a schema's
            // footprint in about three seconds of CPU with no daemon call at
            // all; a promotion frees only the (already tiny) image; the
            // boot-and-dump path goes last because on a nearly-full host it is
            // both the slowest and the likeliest to fail — booting needs a
            // ~200MB rootfs clone that the disk may not have room for.
            OffloadMode::Pressure => match self {
                OffloadKind::Compact => 0,
                OffloadKind::ImageArchive => 1,
                OffloadKind::Promote => 2,
                OffloadKind::Archive => 3,
                OffloadKind::Freeze => 4,
            },
        }
    }
}

/// Why the pacer is picking work, which changes both what is eligible and
/// which job wins.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum OffloadMode {
    /// Routine housekeeping against the configured idle thresholds.
    #[default]
    Routine,
    /// The disk is past its high-water mark. Thresholds are ignored (every
    /// schema without live sessions is a candidate, coldest first) and the
    /// ranking shifts to whatever frees the most, fastest, without a boot.
    Pressure,
}

/// What the picker is allowed to propose: the configured idle thresholds in
/// seconds (`None` = that tier is off), plus the mode.
#[derive(Debug, Default, Clone, Copy)]
struct OffloadPolicy {
    compact_after: Option<u64>,
    freeze_after: Option<u64>,
    archive_after: Option<u64>,
    /// Whether the no-boot image archive is available (`PG_VM_POOL_IMAGE_ARCHIVE`
    /// + the S3 tier + a run dir). Only ever offered under pressure.
    image_archive: bool,
    mode: OffloadMode,
}

/// Pick the single best offload job across the whole registry, plus the
/// schemas whose durable `last_active` should be refreshed (they look stale
/// but are actually warm). Pure, so the tier ladder and its precedence are
/// testable without a registry, a daemon, or a clock.
///
/// Ties break toward the *coldest* schema: with a backlog, the one nobody has
/// touched in longest is both the least likely to be needed back and the one
/// whose disk has been dead weight longest.
fn pick_offload_job(
    records: &[(String, StoreRecord)],
    live: &HashMap<String, (usize, u64)>,
    t: OffloadPolicy,
    keepalive: &dyn Fn(&str) -> bool,
    backing_off: &dyn Fn(&str) -> bool,
    now: u64,
) -> (Option<OffloadJob>, Vec<String>) {
    let mut best: Option<(u8, u64, &str, OffloadKind)> = None;
    let mut refresh: Vec<String> = Vec::new();
    for (schema, rec) in records {
        if keepalive(schema) || backing_off(schema) {
            continue;
        }
        let idle = now.saturating_sub(rec.last_active);
        let warm = live.get(schema).copied();
        let kind = match rec.tier {
            // Already in S3 — the bottom of the ladder.
            Tier::Archived => continue,
            // Offloaded locally: the only remaining step is promotion, and
            // only once it is cold enough for the S3 threshold.
            Tier::Frozen | Tier::Compacted => match t.archive_after {
                Some(after) if idle >= after => OffloadKind::Promote,
                _ => continue,
            },
            Tier::Live => {
                let eligible = |after: Option<u64>, cross_check: Option<(usize, u64)>| {
                    after.is_some_and(|after| {
                        classify_candidate(rec, false, now, after, cross_check)
                            == SweepAction::Archive
                    })
                };
                // Compaction additionally requires no warm entry at all: a
                // warm entry means a VM process may still hold the disk open,
                // and the idle reaper (timeout far shorter than any sensible
                // compact threshold) stops those first. Waiting a scan is
                // cheaper than stalling in the disk-release wait.
                let compactable = warm.is_none() && eligible(t.compact_after, None);
                // The image archive is the other no-boot route off a stopped
                // disk, and the only one that leaves nothing behind. Same
                // warm-entry rule, for the same reason.
                let imageable = t.image_archive
                    && t.mode == OffloadMode::Pressure
                    && warm.is_none()
                    && eligible(t.archive_after, None);
                if compactable {
                    OffloadKind::Compact
                } else if imageable {
                    OffloadKind::ImageArchive
                } else if eligible(t.archive_after, warm) {
                    OffloadKind::Archive
                } else if eligible(t.freeze_after, warm) {
                    OffloadKind::Freeze
                } else {
                    // Durably stale but actually live: keep its clock honest
                    // so it isn't re-evaluated as a candidate every scan.
                    let stale_but_warm = [t.compact_after, t.archive_after, t.freeze_after]
                        .into_iter()
                        .flatten()
                        .any(|after| {
                            classify_candidate(rec, false, now, after, warm) == SweepAction::Refresh
                        });
                    if stale_but_warm {
                        refresh.push(schema.clone());
                    }
                    continue;
                }
            }
        };
        let rank = kind.rank(t.mode);
        if best.is_none_or(|(br, bi, _, _)| (rank, Reverse(idle)) < (br, Reverse(bi))) {
            best = Some((rank, idle, schema, kind));
        }
    }
    let job = best.map(|(_, _, schema, kind)| OffloadJob {
        schema: schema.to_string(),
        kind,
    });
    (job, refresh)
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

    // ---- offload pacer job selection ----------------------------------------

    const NOW: u64 = 10_000_000;
    /// compact at 1h, archive at 1d — the shipped shape.
    const LADDER: OffloadPolicy = OffloadPolicy {
        compact_after: Some(3600),
        freeze_after: None,
        archive_after: Some(86_400),
        image_archive: false,
        mode: OffloadMode::Routine,
    };

    fn tiered(tier: Tier, idle: u64) -> StoreRecord {
        StoreRecord {
            sandbox_id: "sb-x".into(),
            last_active: NOW - idle,
            tier,
        }
    }

    fn pick(
        records: &[(String, StoreRecord)],
        live: &HashMap<String, (usize, u64)>,
        t: OffloadPolicy,
    ) -> Option<OffloadJob> {
        pick_offload_job(records, live, t, &|_| false, &|_| false, NOW).0
    }

    fn schemas(names: &[(&str, Tier, u64)]) -> Vec<(String, StoreRecord)> {
        names
            .iter()
            .map(|(n, tier, idle)| ((*n).to_string(), tiered(*tier, *idle)))
            .collect()
    }

    /// The pacer takes exactly one job per scan, and takes them in
    /// cost-to-benefit order: free local disk with a file upload before
    /// spending a boot on a `pg_dump`.
    #[test]
    fn the_cheapest_most_valuable_job_is_picked_first() {
        let ready = schemas(&[
            ("hot-dump", Tier::Frozen, 90_000),  // promote: file upload, no VM
            ("cold-live", Tier::Live, 200_000),  // compact: no boot
            ("older-live", Tier::Live, 300_000), // compact, colder
        ]);
        let live = HashMap::new();

        let job = pick(&ready, &live, LADDER).expect("work is available");
        assert_eq!(job.kind, OffloadKind::Promote);
        assert_eq!(job.schema, "hot-dump");

        // With promotions done, the coldest compactable schema goes next.
        let job = pick(&ready[1..], &live, LADDER).unwrap();
        assert_eq!(job.kind, OffloadKind::Compact);
        assert_eq!(job.schema, "older-live", "ties break toward the coldest");
    }

    /// A tier that isn't configured is never chosen — and with only the S3
    /// tier on, a cold live schema is archived directly rather than sitting
    /// there waiting for a compaction that will never come.
    #[test]
    fn only_configured_tiers_produce_jobs() {
        let live = HashMap::new();
        let cold = schemas(&[("cold", Tier::Live, 200_000)]);

        let s3_only = OffloadPolicy {
            archive_after: Some(86_400),
            ..Default::default()
        };
        assert_eq!(pick(&cold, &live, s3_only).unwrap().kind, OffloadKind::Archive);

        let compact_only = OffloadPolicy {
            compact_after: Some(3600),
            ..Default::default()
        };
        assert_eq!(
            pick(&cold, &live, compact_only).unwrap().kind,
            OffloadKind::Compact
        );

        // Nothing configured, nothing to do — and an already-archived schema
        // is never work under any configuration.
        assert!(pick(&cold, &live, OffloadPolicy::default()).is_none());
        assert!(pick(&schemas(&[("done", Tier::Archived, 999_999)]), &live, LADDER).is_none());
    }

    /// The guards that keep the pacer off schemas someone is using: keepalive,
    /// too-recent, warm-with-sessions, and per-schema failure backoff.
    #[test]
    fn in_use_and_backing_off_schemas_are_never_picked() {
        let recs = schemas(&[("s", Tier::Live, 200_000)]);
        let no_live = HashMap::new();

        assert!(
            pick_offload_job(&recs, &no_live, LADDER, &|_| true, &|_| false, NOW)
                .0
                .is_none(),
            "keepalive"
        );
        assert!(
            pick_offload_job(&recs, &no_live, LADDER, &|_| false, &|_| true, NOW)
                .0
                .is_none(),
            "in failure backoff"
        );
        assert!(
            pick(&schemas(&[("s", Tier::Live, 10)]), &no_live, LADDER).is_none(),
            "not idle long enough"
        );

        // Warm with a live session: not compactable (warm at all), not
        // archivable (the live cross-check refuses), and its durable clock is
        // refreshed so the next scan doesn't reconsider it.
        let warm: HashMap<String, (usize, u64)> = [("s".to_string(), (1usize, 5u64))].into();
        let (job, refresh) = pick_offload_job(&recs, &warm, LADDER, &|_| false, &|_| false, NOW);
        assert!(job.is_none());
        assert_eq!(refresh, vec!["s".to_string()]);
    }

    /// Under pressure the thresholds stop mattering: a schema idle for a
    /// minute is as evictable as one idle for a week, coldest first. That is
    /// the whole point — the disk is full *now*.
    #[test]
    fn pressure_ignores_the_idle_thresholds() {
        let reg = OffloadPolicy {
            compact_after: Some(3600),
            archive_after: Some(86_400),
            ..Default::default()
        };
        let pressure = OffloadPolicy {
            mode: OffloadMode::Pressure,
            ..reg
        };
        // Far too recent for either configured threshold.
        let fresh = schemas(&[("s", Tier::Live, 60)]);
        let live = HashMap::new();
        assert!(pick(&fresh, &live, reg).is_none(), "routine leaves it alone");

        let pressed = OffloadPolicy {
            compact_after: Some(0),
            archive_after: Some(0),
            ..pressure
        };
        assert_eq!(
            pick(&fresh, &live, pressed).unwrap().kind,
            OffloadKind::Compact
        );
    }

    /// The ordering that matters at 99% full: never spend a VM boot while a
    /// no-boot job is available. Compacting frees ~96% of a schema's
    /// footprint in seconds of CPU; booting to dump needs a ~200MB rootfs
    /// clone the disk may not have room for.
    #[test]
    fn pressure_prefers_no_boot_work_over_dumping() {
        let pressed = OffloadPolicy {
            compact_after: Some(0),
            freeze_after: None,
            archive_after: Some(0),
            image_archive: true,
            mode: OffloadMode::Pressure,
        };
        // A compactable (stopped, not warm) schema and a warm one that could
        // only be reached by the boot-and-dump path.
        let recs = schemas(&[("stopped", Tier::Live, 1000), ("warm", Tier::Live, 5000)]);
        let live: HashMap<String, (usize, u64)> = [("warm".to_string(), (0usize, 5000u64))].into();

        let job = pick(&recs, &live, pressed).unwrap();
        assert_eq!(job.kind, OffloadKind::Compact);
        assert_eq!(
            job.schema, "stopped",
            "the colder schema loses to the one that needs no boot"
        );

        // With compaction unconfigured, the no-boot route is the image
        // archive — still ahead of dumping, and it leaves nothing on the host.
        let no_compact = OffloadPolicy {
            compact_after: None,
            ..pressed
        };
        assert_eq!(
            pick(&recs, &live, no_compact).unwrap().kind,
            OffloadKind::ImageArchive
        );

        // With neither, the dump path is all that's left — and it can serve
        // the warm schema the no-boot routes could not.
        let dumps_only = OffloadPolicy {
            compact_after: None,
            image_archive: false,
            ..pressed
        };
        assert_eq!(
            pick(&dumps_only_recs(), &live, dumps_only).unwrap().kind,
            OffloadKind::Archive
        );
    }

    fn dumps_only_recs() -> Vec<(String, StoreRecord)> {
        schemas(&[("warm", Tier::Live, 5000)])
    }

    /// Freezing is dropped under pressure however it is configured: it costs a
    /// boot *and* leaves the bytes on the host, which cannot relieve a full
    /// disk. And the image archive is never offered routinely — a dump is
    /// smaller and version-independent, so it stays the normal path.
    #[test]
    fn pressure_drops_freezing_and_routine_never_image_archives() {
        let recs = schemas(&[("s", Tier::Live, 999_999)]);
        let live = HashMap::new();

        let freeze_only_pressure = OffloadPolicy {
            freeze_after: Some(0),
            mode: OffloadMode::Pressure,
            ..Default::default()
        };
        // Pressure policy construction drops freeze; even handed one, there is
        // no S3/compact target, so nothing frees the disk.
        assert_eq!(
            pick(&recs, &live, freeze_only_pressure).unwrap().kind,
            OffloadKind::Freeze,
            "the picker honours what it is given — pressure_policy() is what drops freezing"
        );

        let routine_with_images = OffloadPolicy {
            archive_after: Some(0),
            image_archive: true,
            mode: OffloadMode::Routine,
            ..Default::default()
        };
        assert_eq!(
            pick(&recs, &live, routine_with_images).unwrap().kind,
            OffloadKind::Archive,
            "routine archiving dumps; images are the pressure-only shortcut"
        );
    }

    /// A stopped-but-warm entry (VM stopped, entry still cached) must not be
    /// compacted — the disk may still be held open — but is fair game for the
    /// S3 tier, whose cross-check handles it.
    #[test]
    fn a_warm_entry_blocks_compaction_but_not_archiving() {
        let recs = schemas(&[("s", Tier::Live, 200_000)]);
        let idle_warm: HashMap<String, (usize, u64)> = [("s".to_string(), (0usize, 200_000u64))].into();
        let job = pick(&recs, &idle_warm, LADDER).expect("still archivable");
        assert_eq!(job.kind, OffloadKind::Archive);
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

    /// The age gate on the rootfs prune is what keeps a VM that is booting
    /// right now (fresh clone, not yet opened by Firecracker) out of the
    /// candidate set — so "can't tell" and "brand new" must both read false.
    #[test]
    fn file_age_gate_is_false_for_fresh_and_missing_files() {
        let dir = std::env::temp_dir().join(format!("pgfc-age-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("rootfs.ext4");
        std::fs::write(&f, b"x").unwrap();

        assert!(!file_older_than(&f, Duration::from_secs(1800)), "just written");
        assert!(file_older_than(&f, Duration::ZERO), "any age passes a zero floor");
        assert!(
            !file_older_than(&dir.join("nope.ext4"), Duration::ZERO),
            "a missing file is never a deletion candidate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The manual image archive must work for a registry-less stray (adopt
    /// the viewed VM), follow the registry when it agrees, and refuse — never
    /// guess — when the registry names a *different* VM for the schema.
    #[test]
    fn image_target_adopts_strays_and_refuses_conflicts() {
        // Normal: registry binding, viewed from that VM's page.
        assert_eq!(
            resolve_image_target("s", Some("sb-a"), Some("sb-a")).unwrap(),
            ("sb-a".to_string(), false)
        );
        // Non-dashboard caller with no viewed VM: the registry decides.
        assert_eq!(
            resolve_image_target("s", Some("sb-a"), None).unwrap(),
            ("sb-a".to_string(), false)
        );
        // Stray: the daemon knows pg-s, the registry has no row → adopt the
        // VM the button was pressed on.
        assert_eq!(
            resolve_image_target("s", None, Some("sb-b")).unwrap(),
            ("sb-b".to_string(), true)
        );
        // Conflict: two VMs claim the schema — refuse with both ids named.
        let err = resolve_image_target("s", Some("sb-a"), Some("sb-b"))
            .expect_err("a conflicting binding must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("sb-a") && msg.contains("sb-b"), "must name both VMs: {msg}");
        // Nothing to go on at all.
        assert!(resolve_image_target("s", None, None).is_err());
    }

    #[test]
    fn offload_backoff_doubles_caps_and_clears() {
        let b = OffloadBackoff::new();
        let t0 = Instant::now();
        assert!(b.active("s", t0).is_none(), "no failures yet");

        let (n, d) = b.record_failure("s", t0);
        assert_eq!((n, d), (1, Duration::from_secs(30 * 60)));
        assert!(b.active("s", t0 + Duration::from_secs(29 * 60)).is_some());
        assert!(b.active("s", t0 + Duration::from_secs(30 * 60)).is_none(), "window elapsed");

        // Second failure doubles; other schemas are unaffected.
        let (_, d2) = b.record_failure("s", t0);
        assert_eq!(d2, Duration::from_secs(60 * 60));
        assert!(b.active("other", t0).is_none());

        // Many failures clamp to the cap.
        for _ in 0..10 {
            b.record_failure("s", t0);
        }
        let (_, dcap) = b.record_failure("s", t0);
        assert_eq!(dcap, OFFLOAD_BACKOFF_CAP);

        // A success forgets everything.
        b.clear("s");
        assert!(b.active("s", t0).is_none());
        let (n, _) = b.record_failure("s", t0);
        assert_eq!(n, 1, "counter restarts after a clear");

        assert_eq!(fmt_backoff(Duration::from_secs(30 * 60)), "30m");
        assert_eq!(fmt_backoff(Duration::from_secs(2 * 3600)), "2h");
    }

    #[test]
    fn grow_target_doubles_capped_and_gates_correctly() {
        let cfg = DiskGrowConfig { pct: 80.0, max_gb: 100 };
        let gib = |n: u64| n * GIB;

        // 4GiB device, fs spans it, 90% used → double to 8.
        assert_eq!(
            grow_target_gb((gib(4), gib(4) * 9 / 10, gib(4) / 10), gib(4), &cfg),
            Some(8)
        );
        // Below the trigger → no growth.
        assert_eq!(
            grow_target_gb((gib(4), gib(2), gib(2)), gib(4), &cfg),
            None
        );
        // Full fs but THIN under a bigger device → the guest watcher's job.
        assert_eq!(
            grow_target_gb((gib(4), gib(4) * 9 / 10, gib(4) / 10), gib(16), &cfg),
            None
        );
        // Doubling past the cap clamps to it.
        assert_eq!(
            grow_target_gb((gib(64), gib(60), gib(4)), gib(64), &cfg),
            Some(100)
        );
        // Already at the cap → never grows.
        assert_eq!(
            grow_target_gb((gib(100), gib(95), gib(5)), gib(100), &cfg),
            None
        );
        // Unreadable df (0/0) → no decision.
        assert_eq!(grow_target_gb((0, 0, 0), gib(4), &cfg), None);
    }

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
