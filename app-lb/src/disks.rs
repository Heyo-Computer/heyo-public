//! Per-sandbox disk inventory, retention and reclamation.
//!
//! The daemon creates a directory of disk images per sandbox and only removes
//! *some* of it when the sandbox is deleted, so a host that has booted a few
//! hundred VMs accumulates tens of gigabytes that nothing will ever reclaim.
//! Three separate leaks, all of them observed:
//!
//! - `<data>/run/<id>/` — the Firecracker `/workspace` data disk plus a
//!   `snapshot/` directory. The one outright delete-path bug: mvm-ctrl's
//!   `destroy()` unlinks this directory only inside its `data_disk_path
//!   .is_some()` branch, so a sandbox created *without* `disk_size_gb` still
//!   gets the directory (the snapshot dir and the tap allocation live there)
//!   and nothing ever removes it.
//! - `<data>/kvm/<id>/` — the KVM driver's per-VM `rootfs.ext4` and any
//!   `mount*.ext4`, ~1 GiB each. A clean delete *does* remove the whole state
//!   dir; what accumulates is every sandbox that never got one — a crash, a lost
//!   daemon record, a VM nobody ran `heyvm rm` on.
//! - `/tmp/{firecracker,kvm}-<id>-*` — the per-boot scratch. `stop_vm()` removes
//!   it, so it only survives when the VM died without a clean stop — which, for
//!   a hypervisor process, is routine.
//!
//! app-lb is the right place to fix this despite not owning those paths: it is
//! required to run beside a *local* daemon (`guest_ip` exists nowhere else), so
//! it shares the filesystem, and it already writes into the daemon's image
//! directory for artifact pulls. It also has the one piece of knowledge the
//! daemon lacks — which sandboxes a deployment still expects to resume.
//!
//! Two safety rules run through everything below, because the operation this
//! module performs is "delete gigabytes of somebody's data":
//!
//! 1. **A disk is never classified as orphaned unless both daemon listings
//!    succeeded.** A daemon that is restarting answers neither, and every disk
//!    on the host would otherwise look like residue at exactly the moment it is
//!    least safe to act on that belief. See [`Inventory::complete`].
//! 2. **Paths are constructed, never accepted.** A sandbox id arrives from a URL
//!    path segment; it is validated against a strict charset before it is joined
//!    to anything, so no request can point `remove_dir_all` outside the two
//!    directories this module owns.

use crate::config::LbConfig;
use crate::deployment::now_secs;
use crate::registry::Registry;
use crate::vm::VmManager;
use heyo_sdk::{SandboxInfo, SandboxStatus};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long an unclaimed disk lives before the sweep reclaims it.
///
/// Seven days: long enough that a VM retired on Friday is still recoverable on
/// the following Friday, short enough that a fleet churning VMs does not need a
/// human to remember this exists.
pub const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Gap between expiry sweeps. The thing being watched moves on the order of
/// days, so this is deliberately slow — it walks two directories and asks the
/// daemon for its full inactive listing, which is the most expensive question
/// app-lb knows how to ask.
const DEFAULT_SWEEP_SECS: u64 = 3600;

/// Read size when pumping the archive stream. Large enough that the syscall rate
/// stays irrelevant next to gzip, small enough that progress is reported at a
/// useful granularity.
const CHUNK: usize = 1024 * 1024;

/// Archive records kept in memory. Bounded for the same reason the job history
/// is: this is an operator's recent-activity view, not an audit log.
const ARCHIVE_HISTORY: usize = 64;

/// How many archives may be uploading at once.
///
/// Each one is a `tar --sparse --gzip` reading gigabytes off the same disks the
/// running fleet is using, feeding an upload. Two is a background task; twenty
/// is an incident. Excess archives queue rather than being refused — they are
/// still jobs the operator asked for.
const ARCHIVE_CONCURRENCY: usize = 2;

/// How many archives one expiry sweep may start.
///
/// Separate from, and smaller than, the queue above: a sweep that finds a
/// hundred expired disks would otherwise queue a hundred multi-gigabyte uploads
/// from one tick. The rest are picked up by the next sweep, and the count that
/// was deferred is logged rather than silently dropped.
const ARCHIVES_PER_SWEEP: usize = 4;

/// Subdirectories of the daemon's data directory that hold per-sandbox disks.
///
/// Each is `<data_dir>/<root>/<sandbox-id>/…`. Both are scanned regardless of
/// which driver a deployment uses, because a host routinely has sandboxes from
/// both and the leak is per-directory, not per-deployment.
const DISK_ROOTS: [&str; 2] = ["run", "kvm"];

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DiskConfig {
    /// The daemon's data directory — `$MVM_DATA_DIR`, else `<home>/.heyo`.
    pub data_dir: PathBuf,
    /// Where the Firecracker driver writes its per-boot rootfs copy.
    pub tmp_dir: PathBuf,
    /// Where retention decisions persist.
    pub state_path: PathBuf,
    /// How long an unclaimed disk survives. `0` disables expiry entirely; the
    /// inventory and the manual operations stay available.
    pub ttl_secs: u64,
    pub sweep_secs: u64,
    pub aws_bin: String,
    pub tar_bin: String,
    /// Archive destination. Without a bucket the archive routes answer 501 and
    /// `archive_on_expire` is ignored.
    pub bucket: Option<String>,
    /// Key prefix inside the bucket. Never leading- or trailing-slashed.
    pub prefix: String,
    /// `--endpoint-url` for an S3-compatible store.
    pub endpoint: Option<String>,
    /// Whether an expiring disk is archived before it is purged. Off by default:
    /// silently uploading a fleet's disks to S3 is not something to start doing
    /// because a version changed.
    pub archive_on_expire: bool,
    /// Ceiling on one archive, after which both children are killed.
    pub archive_timeout: Duration,
}

impl DiskConfig {
    /// Resolve from the process configuration and the environment.
    ///
    /// Returns `Err` only when the daemon's data directory cannot be located at
    /// all, which is the one thing this module cannot work around. The caller
    /// warns and runs without disk management rather than refusing to start —
    /// the same trade `images_dir` makes.
    pub fn from_env(cfg: &LbConfig) -> Result<Self, String> {
        let data_dir = match std::env::var("APP_LB_VM_DATA_DIR") {
            Ok(v) if !v.trim().is_empty() => PathBuf::from(v.trim()),
            _ => default_data_dir(cfg.heyvm_home.as_deref())?,
        };
        let tmp_dir = std::env::var("APP_LB_VM_TMP_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| PathBuf::from(v.trim()))
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        let state_path = std::env::var("APP_LB_DISKS_PATH")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("app-lb-disks.json"));

        let ttl_secs = parse_secs("APP_LB_DISK_TTL_SECS", DEFAULT_TTL_SECS)?;
        let sweep_secs = parse_secs("APP_LB_DISK_SWEEP_SECS", DEFAULT_SWEEP_SECS)?.max(60);
        let archive_timeout =
            Duration::from_secs(parse_secs("APP_LB_DISK_ARCHIVE_TIMEOUT_SECS", 7200)?.max(60));

        Ok(Self {
            data_dir,
            tmp_dir,
            state_path,
            ttl_secs,
            sweep_secs,
            aws_bin: cfg.aws_bin.clone(),
            tar_bin: std::env::var("APP_LB_TAR_BIN")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "tar".into()),
            bucket: std::env::var("APP_LB_DISK_ARCHIVE_BUCKET")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            prefix: std::env::var("APP_LB_DISK_ARCHIVE_PREFIX")
                .ok()
                .map(|v| v.trim().trim_matches('/').to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "app-lb/disks".into()),
            endpoint: std::env::var("APP_LB_DISK_ARCHIVE_ENDPOINT")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            archive_on_expire: env_flag("APP_LB_DISK_ARCHIVE_ON_EXPIRE", false),
            archive_timeout,
        })
    }
}

/// Where heyvmd keeps its state, resolved the way mvm-ctrl's
/// `get_heyo_data_dir()` does.
///
/// `heyvm_home` wins over the process `HOME` for the same reason it does for the
/// image directory: it names the *daemon's* home, and the daemon is the only
/// process whose view of these paths matters.
fn default_data_dir(heyvm_home: Option<&str>) -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("MVM_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let home = heyvm_home
        .map(str::to_string)
        .or_else(|| std::env::var("HOME").ok())
        .filter(|h| !h.trim().is_empty())
        .ok_or(
            "cannot tell where heyvm keeps its per-sandbox disks: neither MVM_DATA_DIR nor HOME \
             is set. Set APP_LB_VM_DATA_DIR to the daemon's data directory",
        )?;
    Ok(PathBuf::from(home).join(".heyo"))
}

fn parse_secs(var: &str, default: u64) -> Result<u64, String> {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("{var} must be a whole number of seconds, got {v:?}")),
        _ => Ok(default),
    }
}

fn env_flag(var: &str, default: bool) -> bool {
    match std::env::var(var) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

/// Whether a sandbox id is safe to join to a path.
///
/// The id reaches this module as a URL path segment, and every path it produces
/// is passed to `remove_dir_all`. Anything outside this charset — a dot, a
/// slash, a NUL — is refused rather than sanitized, because a request that has
/// to be sanitized is a request that was never legitimate.
pub fn valid_sandbox_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---------------------------------------------------------------------------
// the model
// ---------------------------------------------------------------------------

/// What the daemon thinks of the sandbox a disk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskState {
    /// The sandbox is running. Its disks are in use and are never reclaimed.
    Running,
    /// The daemon still has a record, but the sandbox is not running.
    Stopped,
    /// The daemon has no record of this sandbox at all. Pure residue.
    Orphan,
    /// The daemon could not be reached, so nothing is known. Never reclaimed.
    Unknown,
}

impl DiskState {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Orphan => "orphan",
            Self::Unknown => "unknown",
        }
    }
}

/// What one file on the host is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PartKind {
    /// The persistent `/workspace` disk (`run/<id>/data.ext4`).
    Data,
    /// A guest root filesystem — the KVM driver's persistent one, or a
    /// Firecracker per-boot copy left behind in `/tmp`.
    Rootfs,
    /// An archive-mount disk (`kvm/<id>/mount0.ext4`).
    Mount,
    /// A memory/state checkpoint under `run/<id>/snapshot/`.
    Snapshot,
    /// Logs, sockets, configuration — small, and listed so the totals add up.
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskPart {
    pub kind: PartKind,
    pub path: String,
    /// Blocks actually allocated on the host. This is the number that matters:
    /// a `data.ext4` is created sparse at its full nominal size, so `apparent`
    /// routinely reads 8 GiB for 200 MiB of real occupancy.
    pub bytes: u64,
    /// `st_size` — what the guest sees.
    pub apparent_bytes: u64,
    pub modified_at: u64,
}

/// Everything on this host belonging to one sandbox.
#[derive(Debug, Clone, Serialize)]
pub struct DiskInfo {
    pub sandbox_id: String,
    /// The daemon's name for it, when the daemon still has a record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The deployment that owns it, from an `applb-<deployment>-<nonce>` name.
    /// `None` for a sandbox app-lb did not create, or one whose name is gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<String>,
    pub state: DiskState,
    /// Whether a live deployment lists this sandbox as one it intends to resume.
    /// A claimed disk is never reclaimed, at any age.
    pub claimed: bool,
    /// Set by an operator: never reclaim this, whatever its age.
    pub retain: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub bytes: u64,
    pub apparent_bytes: u64,
    pub modified_at: u64,
    /// When the sweep would reclaim this, or `None` if it never would.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Why it will not be reclaimed, in a phrase the page can show verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_by: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<ArchiveRecord>,
    pub parts: Vec<DiskPart>,
    /// The directories that would be removed by a purge. Shown on the page so
    /// the operator confirms against real paths rather than an id.
    pub roots: Vec<String>,
}

impl DiskInfo {
    /// Whether the expiry sweep may reclaim this disk at `now`.
    ///
    /// Deliberately total and side-effect-free: this is the predicate that
    /// decides whether gigabytes get deleted, so it is exercised directly in the
    /// tests rather than only through a sweep against a live daemon.
    pub fn expired(&self, now: u64, ttl_secs: u64) -> bool {
        if ttl_secs == 0 || self.held_by.is_some() {
            return false;
        }
        now.saturating_sub(self.modified_at) >= ttl_secs
    }
}

/// Why a disk is exempt from expiry, or `None` if it is a candidate.
///
/// Ordered by how strongly each reason holds: a running VM's disk is not merely
/// exempt, it is in use, and saying so is more useful than reporting the first
/// matching rule.
fn hold_reason(state: DiskState, claimed: bool, retain: bool) -> Option<&'static str> {
    match state {
        DiskState::Running => Some("the sandbox is running"),
        DiskState::Unknown => Some("the daemon could not be reached"),
        _ if claimed => Some("a deployment expects to resume it"),
        _ if retain => Some("marked retained"),
        _ => None,
    }
}

/// A completed upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveRecord {
    pub uri: String,
    pub at: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveStatus {
    Running,
    Succeeded,
    Failed,
}

/// One archive, in flight or finished.
#[derive(Debug)]
struct ArchiveJob {
    id: String,
    sandbox_id: String,
    uri: String,
    started_at: u64,
    finished_at: Option<u64>,
    status: ArchiveStatus,
    /// Compressed bytes handed to `aws` so far. Shared with the pump task, so a
    /// poll of `GET /disks` reports live progress without taking its lock.
    sent: Arc<AtomicU64>,
    /// Bytes the archive is expected to cover, for a progress percentage.
    expected: u64,
    error: Option<String>,
    /// Whether the disk was purged after a successful upload.
    purged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveView {
    pub id: String,
    pub sandbox_id: String,
    pub uri: String,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    pub status: ArchiveStatus,
    pub bytes: u64,
    pub expected_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub purged: bool,
}

impl ArchiveJob {
    fn view(&self) -> ArchiveView {
        ArchiveView {
            id: self.id.clone(),
            sandbox_id: self.sandbox_id.clone(),
            uri: self.uri.clone(),
            started_at: self.started_at,
            finished_at: self.finished_at,
            status: self.status,
            bytes: self.sent.load(Ordering::Relaxed),
            expected_bytes: self.expected,
            error: self.error.clone(),
            purged: self.purged,
        }
    }
}

/// The operator's decision about one disk. Only what cannot be re-derived from
/// the filesystem is stored; everything else is rebuilt on each scan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DiskPolicy {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    retain: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived: Option<ArchiveRecord>,
}

impl DiskPolicy {
    /// Whether this entry still says anything. An entry that has gone back to
    /// its defaults is dropped rather than written, so the file tracks decisions
    /// rather than every sandbox that ever existed.
    fn is_empty(&self) -> bool {
        !self.retain && self.note.is_none() && self.archived.is_none()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredPolicies {
    version: u32,
    #[serde(default)]
    disks: BTreeMap<String, DiskPolicy>,
}

/// The answer to `GET /disks`.
#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    /// Whether both daemon listings succeeded. When false, nothing is classified
    /// as an orphan and the sweep declines to run.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_reason: Option<String>,
    pub data_dir: String,
    pub tmp_dir: String,
    /// `0` when expiry is disabled.
    pub ttl_secs: u64,
    pub sweep_secs: u64,
    pub archive_enabled: bool,
    pub archive_on_expire: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_target: Option<String>,
    pub totals: Totals,
    pub disks: Vec<DiskInfo>,
    pub archives: Vec<ArchiveView>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub disks: usize,
    pub bytes: u64,
    pub apparent_bytes: u64,
    pub running: usize,
    pub stopped: usize,
    pub orphan: usize,
    pub retained: usize,
    /// Disks the sweep would reclaim right now, and what that would free.
    pub expiring_now: usize,
    pub reclaimable_bytes: u64,
}

/// What a purge actually did.
#[derive(Debug, Clone, Serialize)]
pub struct PurgeOutcome {
    pub sandbox_id: String,
    /// Whether the daemon was asked to delete the sandbox first.
    pub killed: bool,
    pub removed: Vec<String>,
    /// What was actually reclaimed — the parts under a path that was removed,
    /// not what the sandbox occupied before. The two agree unless some paths
    /// failed, and that is the case where the difference matters.
    pub bytes: u64,
    /// Paths that could not be removed, with the reason. A partial purge is
    /// reported rather than rolled back: the bytes that did go are gone.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
}

#[derive(Debug)]
pub enum DiskError {
    /// The id is not a shape this module will join to a path.
    BadId(String),
    NotFound(String),
    /// The disk's state refuses the operation.
    Held {
        sandbox_id: String,
        reason: &'static str,
        /// Whether `force=1` would get past this. False for a running sandbox,
        /// which has no legitimate override — and the message must not offer one,
        /// because the console reads it back to the operator in a confirm dialog
        /// and then retries with `?force=1` if it mentions force.
        forceable: bool,
    },
    /// No archive bucket is configured.
    NoArchiveTarget,
    /// There is nothing durable to archive — only scratch files in `/tmp`.
    NothingToArchive(String),
    AlreadyArchiving(String),
    /// Every path a purge tried to remove failed, so nothing was reclaimed.
    ///
    /// Distinct from a *partial* failure, which stays a success: the bytes that
    /// did go are gone and the caller needs the outcome, not an error. This is
    /// the case where the operation did not happen at all, and reporting it as
    /// a completed purge is what let a whole fleet's worth of residue look
    /// reclaimed while nothing moved. In practice it is nearly always
    /// permissions — heyvmd's data directory belongs to the user the daemon runs
    /// as, and app-lb is usually somebody else.
    PurgeFailed {
        sandbox_id: String,
        failed: Vec<String>,
    },
    Io(String),
}

impl std::fmt::Display for DiskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadId(id) => write!(
                f,
                "{id:?} is not a sandbox id: only letters, digits, dashes and underscores"
            ),
            Self::NotFound(id) => write!(f, "no disks on this host for sandbox {id:?}"),
            Self::Held {
                sandbox_id,
                reason,
                forceable,
            } => {
                write!(f, "refusing to touch {sandbox_id}: {reason}")?;
                if *forceable {
                    write!(f, ". Pass force=1 to override")?;
                }
                Ok(())
            }
            Self::NoArchiveTarget => write!(
                f,
                "no archive destination: set APP_LB_DISK_ARCHIVE_BUCKET to an S3 bucket"
            ),
            Self::NothingToArchive(id) => write!(
                f,
                "sandbox {id} has no durable disks — only per-boot scratch, which is not \
                 worth archiving"
            ),
            Self::AlreadyArchiving(id) => write!(f, "an archive of {id} is already running"),
            Self::PurgeFailed { sandbox_id, failed } => write!(
                f,
                "nothing was reclaimed for {sandbox_id}: every path failed to delete. \
                 app-lb runs as {}, and these are usually owned by whichever user heyvmd \
                 runs as — check the ownership of the data directory. {}",
                crate::jobs::whoami(),
                failed.join("; "),
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DiskError {}

// ---------------------------------------------------------------------------
// the store
// ---------------------------------------------------------------------------

pub struct DiskStore {
    cfg: DiskConfig,
    vms: VmManager,
    registry: Arc<Registry>,
    policies: Mutex<BTreeMap<String, DiskPolicy>>,
    archives: Mutex<VecDeque<ArchiveJob>>,
    /// Sandboxes with an archive in flight. Held separately from `archives` so
    /// the check is a set lookup and cannot be confused by history.
    archiving: Mutex<HashSet<String>>,
    /// Distinguishes two archives started in the same second.
    archive_seq: AtomicU64,
    /// Bounds simultaneous uploads. See [`ARCHIVE_CONCURRENCY`].
    archive_slots: tokio::sync::Semaphore,
}

impl DiskStore {
    pub fn new(cfg: DiskConfig, vms: VmManager, registry: Arc<Registry>) -> Self {
        Self {
            cfg,
            vms,
            registry,
            policies: Mutex::new(BTreeMap::new()),
            archives: Mutex::new(VecDeque::new()),
            archiving: Mutex::new(HashSet::new()),
            archive_seq: AtomicU64::new(0),
            archive_slots: tokio::sync::Semaphore::new(ARCHIVE_CONCURRENCY),
        }
    }

    pub fn config(&self) -> &DiskConfig {
        &self.cfg
    }

    /// Load retention decisions. A missing file is not an error.
    pub fn load(&self) -> Result<usize, String> {
        let bytes = match std::fs::read(&self.cfg.state_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(format!("{}: {e}", self.cfg.state_path.display())),
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(0);
        }
        let stored: StoredPolicies = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{}: {e}", self.cfg.state_path.display()))?;
        let n = stored.disks.len();
        *self.policies.lock().unwrap() = stored.disks;
        Ok(n)
    }

    fn persist(&self) -> Result<(), String> {
        let disks = self.policies.lock().unwrap().clone();
        let json = serde_json::to_vec_pretty(&StoredPolicies { version: 1, disks })
            .map_err(|e| e.to_string())?;
        if let Some(parent) = self
            .cfg
            .state_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = self.cfg.state_path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.cfg.state_path).map_err(|e| e.to_string())
    }

    // -- inventory ---------------------------------------------------------

    /// The full picture: what is on disk, what the daemon knows, what app-lb
    /// claims.
    pub async fn inventory(&self) -> Inventory {
        let cfg = self.cfg.clone();
        // A host with a few thousand sandboxes is a few thousand `stat` calls.
        // Off the runtime, so a slow or cold page cache cannot stall the admin
        // listener while somebody is looking at the dashboard.
        let mut groups = tokio::task::spawn_blocking(move || scan(&cfg))
            .await
            .unwrap_or_default();

        // Both listings, for the reason `Autoscaler::sweep_suspended` documents:
        // a stopped KVM sandbox appears in the fleet list with a terminal status
        // while a stopped Firecracker one appears only in the inactive listing.
        // Reading one would classify half the fleet's stopped disks as orphans.
        let fleet = self.vms.list().await;
        let inactive = self.vms.list_inactive().await;

        let incomplete_reason = match (&fleet, &inactive) {
            (Err(e), _) => Some(format!("the daemon's sandbox list is unavailable: {e}")),
            (_, Err(e)) => Some(format!("the daemon's inactive listing is unavailable: {e}")),
            _ => None,
        };
        let complete = incomplete_reason.is_none();

        let mut known: HashMap<String, SandboxInfo> = HashMap::new();
        for info in fleet.into_iter().flatten() {
            known.insert(info.id.clone(), info);
        }
        for info in inactive.into_iter().flatten() {
            // The fleet listing wins: it is the one that can say `Running`.
            known.entry(info.id.clone()).or_insert(info);
        }

        // Which sandboxes a live deployment still expects to resume.
        let deployments = self.registry.deployments();
        let mut claimed: HashSet<String> = HashSet::new();
        for d in deployments.values() {
            claimed.extend(d.state().suspended.iter().cloned());
        }

        let policies = self.policies.lock().unwrap().clone();
        let now = now_secs();

        let mut disks: Vec<DiskInfo> = Vec::with_capacity(groups.len());
        for (sandbox_id, group) in groups.drain() {
            let info = known.get(&sandbox_id);
            let state = match (complete, info) {
                (false, _) => DiskState::Unknown,
                (true, Some(i)) if i.status == SandboxStatus::Running => DiskState::Running,
                (true, Some(_)) => DiskState::Stopped,
                (true, None) => DiskState::Orphan,
            };
            let policy = policies.get(&sandbox_id).cloned().unwrap_or_default();
            let is_claimed = claimed.contains(&sandbox_id);
            let held_by = hold_reason(state, is_claimed, policy.retain);

            let deployment = info
                .and_then(|i| crate::vm::owner_of(&i.name).map(str::to_string))
                // A purged daemon record takes the name with it, so fall back to
                // asking the deployments themselves — the only other place the
                // association survives.
                .or_else(|| {
                    deployments
                        .values()
                        .find(|d| d.state().suspended.contains(&sandbox_id))
                        .map(|d| d.spec.id.clone())
                });

            disks.push(DiskInfo {
                sandbox_id,
                name: info.map(|i| i.name.clone()),
                deployment,
                state,
                claimed: is_claimed,
                retain: policy.retain,
                note: policy.note.clone(),
                bytes: group.bytes,
                apparent_bytes: group.apparent_bytes,
                modified_at: group.modified_at,
                expires_at: (held_by.is_none() && self.cfg.ttl_secs > 0)
                    .then(|| group.modified_at.saturating_add(self.cfg.ttl_secs)),
                held_by,
                archived: policy.archived,
                parts: group.parts,
                roots: group.roots.iter().map(|p| p.display().to_string()).collect(),
            });
        }

        // Biggest first: the page exists to answer "what is eating the disk?".
        disks.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.sandbox_id.cmp(&b.sandbox_id)));

        let mut totals = Totals {
            disks: disks.len(),
            ..Default::default()
        };
        for d in &disks {
            totals.bytes += d.bytes;
            totals.apparent_bytes += d.apparent_bytes;
            match d.state {
                DiskState::Running => totals.running += 1,
                DiskState::Stopped => totals.stopped += 1,
                DiskState::Orphan => totals.orphan += 1,
                DiskState::Unknown => {}
            }
            if d.retain {
                totals.retained += 1;
            }
            if d.expired(now, self.cfg.ttl_secs) {
                totals.expiring_now += 1;
                totals.reclaimable_bytes += d.bytes;
            }
        }

        Inventory {
            complete,
            incomplete_reason,
            data_dir: self.cfg.data_dir.display().to_string(),
            tmp_dir: self.cfg.tmp_dir.display().to_string(),
            ttl_secs: self.cfg.ttl_secs,
            sweep_secs: self.cfg.sweep_secs,
            archive_enabled: self.cfg.bucket.is_some(),
            archive_on_expire: self.cfg.archive_on_expire && self.cfg.bucket.is_some(),
            archive_target: self
                .cfg
                .bucket
                .as_ref()
                .map(|b| format!("s3://{b}/{}", self.cfg.prefix)),
            totals,
            disks,
            archives: self.archive_views(),
        }
    }

    /// What the first sweep would find, without talking to the daemon.
    ///
    /// Exists so an operator who turns app-lb on and never opens `/storage`
    /// still learns from the startup log that 15 GiB is scheduled for deletion
    /// in an hour. Synchronous, and safe to call before the runtime exists: it
    /// is a directory walk and some `stat` calls, nothing more.
    ///
    /// An **upper bound**, deliberately. Without the daemon's listings this
    /// cannot tell a running sandbox from residue, so it counts every disk old
    /// enough to be a candidate. The sweep itself never acts on this number.
    pub fn expiry_preview(&self) -> (usize, usize, u64) {
        let groups = scan(&self.cfg);
        let total = groups.len();
        if self.cfg.ttl_secs == 0 {
            return (total, 0, 0);
        }
        let policies = self.policies.lock().unwrap();
        let now = now_secs();
        let mut count = 0;
        let mut bytes = 0;
        for (id, g) in &groups {
            if policies.get(id).is_some_and(|p| p.retain) {
                continue;
            }
            if now.saturating_sub(g.modified_at) >= self.cfg.ttl_secs {
                count += 1;
                bytes += g.bytes;
            }
        }
        (total, count, bytes)
    }

    fn archive_views(&self) -> Vec<ArchiveView> {
        let jobs = self.archives.lock().unwrap();
        jobs.iter().rev().map(ArchiveJob::view).collect()
    }

    /// One disk, or `None`. Goes through the full inventory rather than a
    /// targeted stat, because every decision made about a disk depends on the
    /// daemon's view of it and on what the deployments claim.
    async fn one(&self, sandbox_id: &str) -> Result<(DiskInfo, bool), DiskError> {
        if !valid_sandbox_id(sandbox_id) {
            return Err(DiskError::BadId(sandbox_id.to_string()));
        }
        let inv = self.inventory().await;
        let complete = inv.complete;
        inv.disks
            .into_iter()
            .find(|d| d.sandbox_id == sandbox_id)
            .map(|d| (d, complete))
            .ok_or_else(|| DiskError::NotFound(sandbox_id.to_string()))
    }

    // -- retention ---------------------------------------------------------

    /// Mark a disk retained (or not) and note why.
    ///
    /// Accepted for a sandbox with no disks on this host, so an operator can
    /// pin one *before* it accumulates any — the entry is kept and applies as
    /// soon as the sandbox writes something.
    pub fn set_policy(
        &self,
        sandbox_id: &str,
        retain: Option<bool>,
        note: Option<Option<String>>,
    ) -> Result<(), DiskError> {
        if !valid_sandbox_id(sandbox_id) {
            return Err(DiskError::BadId(sandbox_id.to_string()));
        }
        {
            let mut policies = self.policies.lock().unwrap();
            let entry = policies.entry(sandbox_id.to_string()).or_default();
            if let Some(r) = retain {
                entry.retain = r;
            }
            if let Some(n) = note {
                entry.note = n.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            }
            if entry.is_empty() {
                policies.remove(sandbox_id);
            }
        }
        self.persist().map_err(DiskError::Io)
    }

    fn record_archive(&self, sandbox_id: &str, record: ArchiveRecord) {
        self.policies
            .lock()
            .unwrap()
            .entry(sandbox_id.to_string())
            .or_default()
            .archived = Some(record);
        if let Err(e) = self.persist() {
            tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to record disk archive");
        }
    }

    fn forget(&self, sandbox_id: &str) {
        if self.policies.lock().unwrap().remove(sandbox_id).is_some()
            && let Err(e) = self.persist()
        {
            tracing::warn!(sandbox = %sandbox_id, error = %e, "failed to persist disk policy");
        }
    }

    // -- purge -------------------------------------------------------------

    /// Delete every trace of a sandbox from this host.
    ///
    /// The daemon is asked to delete the sandbox first, so its own cleanup runs
    /// and its record goes away; whatever it leaves behind is then removed
    /// directly. A daemon that refuses (or has already forgotten the sandbox) is
    /// not fatal — the point of this path is the residue, and residue is exactly
    /// what the daemon cannot see.
    pub async fn purge(&self, sandbox_id: &str, force: bool) -> Result<PurgeOutcome, DiskError> {
        let (disk, complete) = self.one(sandbox_id).await?;
        self.purge_resolved(disk, complete, force).await
    }

    /// [`purge`](Self::purge) against a disk the caller has already resolved.
    ///
    /// Split out for the sweep, which has an inventory in hand and must not
    /// rebuild one per disk: `one()` walks the whole data directory *and* makes
    /// two daemon round-trips, so re-resolving inside the loop would turn a
    /// 105-disk sweep into 105 full scans and 210 requests at the daemon.
    async fn purge_resolved(
        &self,
        disk: DiskInfo,
        complete: bool,
        force: bool,
    ) -> Result<PurgeOutcome, DiskError> {
        // First, and under no flag. Deleting the disks out from under a live
        // hypervisor corrupts the guest rather than freeing anything, and the
        // operator's actual intent — stop it, then reclaim it — is two clicks
        // away on the dashboard. Checked *before* the forceable guards so the
        // message never offers an override that would be refused anyway.
        if disk.state == DiskState::Running {
            return Err(DiskError::Held {
                sandbox_id: disk.sandbox_id,
                reason: "the sandbox is running; evict or stop it first",
                forceable: false,
            });
        }

        if !force && let Some(reason) = disk.held_by {
            return Err(DiskError::Held {
                sandbox_id: disk.sandbox_id,
                reason,
                forceable: true,
            });
        }
        // An incomplete inventory needs no check of its own: it makes every disk
        // `Unknown`, which `hold_reason` already holds on. Asserted rather than
        // re-tested, so the two cannot drift apart silently.
        debug_assert!(
            complete || force || disk.held_by.is_some(),
            "an unreachable daemon must leave every disk held",
        );

        let killed = match disk.state {
            DiskState::Stopped | DiskState::Unknown => {
                match self.vms.kill(&disk.sandbox_id).await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(
                            sandbox = %disk.sandbox_id,
                            error = %e,
                            "daemon would not delete the sandbox; removing its disks anyway",
                        );
                        false
                    }
                }
            }
            // Nothing to ask: the daemon has no record of it.
            DiskState::Orphan => false,
            DiskState::Running => unreachable!("refused above"),
        };

        let cfg = self.cfg.clone();
        let id = disk.sandbox_id.clone();
        let (removed, failed) = tokio::task::spawn_blocking(move || remove_all(&cfg, &id))
            .await
            .unwrap_or_else(|e| (Vec::new(), vec![format!("purge task failed: {e}")]));

        // Nothing went. Reported as an error rather than as a purge that freed
        // zero bytes, because the two are not the same claim and only one of
        // them is true: the disks are still there, still counted, and still
        // taking the space this was called to reclaim. Returning `Ok` here is
        // what let a sweep report a thousand disks purged while the host filled
        // up — see [`DiskError::PurgeFailed`].
        //
        // Both empty is *not* this case: it means the paths were already gone,
        // which is a purge that had nothing left to do and succeeded.
        if removed.is_empty() && !failed.is_empty() {
            tracing::error!(
                sandbox = %disk.sandbox_id,
                deployment = disk.deployment.as_deref().unwrap_or("-"),
                state = disk.state.label(),
                failed = failed.len(),
                detail = %failed.join("; "),
                "purge removed nothing: every path failed",
            );
            return Err(DiskError::PurgeFailed {
                sandbox_id: disk.sandbox_id,
                failed,
            });
        }

        // Both of these describe a sandbox whose disks are gone, so neither may
        // run until that is actually true. Dropping the claim first would tell
        // the next scale-up not to resume a sandbox that is still perfectly
        // resumable, and forgetting the policy first would lose a `retain` flag
        // protecting a disk this call failed to delete.
        if disk.claimed {
            // Or the next scale-up spends a resume attempt on a sandbox whose
            // disks are gone and then boots a fresh VM anyway.
            self.drop_claim(&disk.sandbox_id);
        }
        self.forget(&disk.sandbox_id);

        // A partial failure stays a success — the bytes that went are gone — but
        // it is not something to log at the same level as a clean one, and the
        // counts are here either way so that "purged" in the log can be checked
        // against what it actually removed.
        if failed.is_empty() {
            tracing::info!(
                sandbox = %disk.sandbox_id,
                deployment = disk.deployment.as_deref().unwrap_or("-"),
                bytes = disk.bytes,
                state = disk.state.label(),
                killed,
                removed = removed.len(),
                "purged sandbox disks",
            );
        } else {
            tracing::warn!(
                sandbox = %disk.sandbox_id,
                deployment = disk.deployment.as_deref().unwrap_or("-"),
                bytes = disk.bytes,
                state = disk.state.label(),
                killed,
                removed = removed.len(),
                failed = failed.len(),
                detail = %failed.join("; "),
                "purged some of a sandbox's disks; the rest could not be removed",
            );
        }

        Ok(PurgeOutcome {
            sandbox_id: disk.sandbox_id,
            killed,
            bytes: freed(&disk.parts, &removed),
            removed,
            failed,
        })
    }

    /// Remove a sandbox from whichever deployment still expects to resume it.
    fn drop_claim(&self, sandbox_id: &str) {
        for d in self.registry.deployments().values() {
            let changed = d.mutate_state(|s| s.suspended.retain(|id| id != sandbox_id));
            if changed {
                tracing::info!(
                    deployment = %d.spec.id,
                    sandbox = %sandbox_id,
                    "dropped a suspended-VM claim because its disks were purged",
                );
                if let Err(e) = self.registry.persist_one(&d.spec.id) {
                    tracing::error!(deployment = %d.spec.id, error = %e, "failed to persist state");
                }
            }
        }
    }

    // -- archive -----------------------------------------------------------

    /// Start streaming a sandbox's disks to S3, and return the job that is
    /// doing it.
    ///
    /// Returns as soon as the upload task is spawned; progress is reported
    /// through `GET /disks`. `purge_after` reclaims the disks once the upload
    /// *succeeds* — never on failure, which is the whole reason this is one
    /// operation rather than two calls an operator has to sequence correctly.
    pub async fn archive(
        self: &Arc<Self>,
        sandbox_id: &str,
        purge_after: bool,
    ) -> Result<ArchiveView, DiskError> {
        if self.cfg.bucket.is_none() {
            return Err(DiskError::NoArchiveTarget);
        }
        let (disk, _) = self.one(sandbox_id).await?;
        self.archive_resolved(disk, purge_after)
    }

    /// [`archive`](Self::archive) against an already-resolved disk, for the
    /// sweep. See [`purge_resolved`](Self::purge_resolved).
    fn archive_resolved(
        self: &Arc<Self>,
        disk: DiskInfo,
        purge_after: bool,
    ) -> Result<ArchiveView, DiskError> {
        let Some(bucket) = self.cfg.bucket.clone() else {
            return Err(DiskError::NoArchiveTarget);
        };

        // Only the durable directories are worth uploading. A sandbox whose
        // only trace is a `/tmp` rootfs copy has nothing an archive could
        // restore — that file is a copy of a base image plus one boot's writes.
        let relative = self.relative_roots(&disk.sandbox_id);
        if relative.is_empty() {
            return Err(DiskError::NothingToArchive(disk.sandbox_id));
        }

        // Uploading a disk that a running VM is writing to produces a corrupt
        // image and a lot of egress. Refused rather than warned about.
        if disk.state == DiskState::Running {
            return Err(DiskError::Held {
                sandbox_id: disk.sandbox_id,
                reason: "the sandbox is running, so its disks are being written to; an archive \
                         taken now would be a torn copy",
                forceable: false,
            });
        }

        if !self
            .archiving
            .lock()
            .unwrap()
            .insert(disk.sandbox_id.clone())
        {
            return Err(DiskError::AlreadyArchiving(disk.sandbox_id));
        }

        let started_at = now_secs();
        let seq = self.archive_seq.fetch_add(1, Ordering::Relaxed);
        let job_id = format!("arch-{started_at:x}-{seq:x}");
        let key = format!("{}/{}/{started_at}.tar.gz", self.cfg.prefix, disk.sandbox_id);
        let uri = format!("s3://{bucket}/{key}");
        let sent = Arc::new(AtomicU64::new(0));

        let view = {
            let mut jobs = self.archives.lock().unwrap();
            let job = ArchiveJob {
                id: job_id.clone(),
                sandbox_id: disk.sandbox_id.clone(),
                uri: uri.clone(),
                started_at,
                finished_at: None,
                status: ArchiveStatus::Running,
                sent: sent.clone(),
                // The compressed stream is smaller than this, always — it is the
                // ceiling the progress bar is drawn against, and the figure the
                // `aws` CLI uses to pick a part size.
                expected: disk.apparent_bytes,
                error: None,
                purged: false,
            };
            let view = job.view();
            jobs.push_back(job);
            while jobs.len() > ARCHIVE_HISTORY {
                jobs.pop_front();
            }
            view
        };

        tracing::info!(
            sandbox = %disk.sandbox_id,
            job = %job_id,
            uri = %uri,
            bytes = disk.bytes,
            purge_after,
            "archiving sandbox disks",
        );

        let store = self.clone();
        let sandbox = disk.sandbox_id.clone();
        tokio::spawn(async move {
            // Queued rather than refused when the host is already uploading:
            // the operator asked for this one too. The job reads `running` with
            // zero bytes sent while it waits, which is what is happening.
            let _slot = store.archive_slots.acquire().await;
            let result = store.run_archive(&relative, &uri, disk.apparent_bytes, &sent).await;
            let (status, error) = match &result {
                Ok(()) => (ArchiveStatus::Succeeded, None),
                Err(e) => (ArchiveStatus::Failed, Some(e.clone())),
            };

            let mut purged = false;
            if result.is_ok() {
                store.record_archive(
                    &sandbox,
                    ArchiveRecord {
                        uri: uri.clone(),
                        at: now_secs(),
                        bytes: sent.load(Ordering::Relaxed),
                    },
                );
                if purge_after {
                    match store.purge(&sandbox, false).await {
                        Ok(o) => {
                            purged = true;
                            tracing::info!(
                                sandbox = %sandbox,
                                bytes = o.bytes,
                                "purged sandbox disks after a successful archive",
                            );
                        }
                        Err(e) => tracing::warn!(
                            sandbox = %sandbox,
                            error = %e,
                            "archive succeeded but the purge did not; the disks are still here",
                        ),
                    }
                }
            } else if let Err(e) = &result {
                tracing::error!(sandbox = %sandbox, error = %e, "disk archive failed");
            }

            // The in-flight marker is released last, so a retry cannot start
            // while the purge is still running.
            store.finish_archive(&job_id, status, error, purged);
            store.archiving.lock().unwrap().remove(&sandbox);
        });

        Ok(view)
    }

    /// The sandbox directories that exist, as paths relative to `data_dir`.
    ///
    /// Relative because `tar -C <data_dir> run/<id>` stores them that way, which
    /// is what makes an archive restorable by untarring it back into a data
    /// directory without rewriting any paths.
    fn relative_roots(&self, sandbox_id: &str) -> Vec<String> {
        DISK_ROOTS
            .iter()
            .filter(|root| self.cfg.data_dir.join(root).join(sandbox_id).is_dir())
            .map(|root| format!("{root}/{sandbox_id}"))
            .collect()
    }

    /// `tar … | aws s3 cp -`, with app-lb holding the pipe.
    ///
    /// app-lb sits in the middle rather than letting the shell join the two, for
    /// three reasons: there is no shell to quote things into, the byte count for
    /// the progress bar has to come from somewhere, and a `tar` that outlives a
    /// failed upload has to be killed rather than left blocked on a full pipe.
    ///
    /// `--sparse` is what makes this affordable. A `data.ext4` is created at its
    /// full nominal size with holes; without `-S`, `tar` reads and compresses
    /// every zero byte of it.
    async fn run_archive(
        &self,
        relative_roots: &[String],
        uri: &str,
        expected: u64,
        sent: &Arc<AtomicU64>,
    ) -> Result<(), String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::process::Command;
        use std::process::Stdio;

        let mut tar = Command::new(&self.cfg.tar_bin);
        tar.arg("--create")
            .arg("--sparse")
            .arg("--gzip")
            .arg("--directory")
            .arg(&self.cfg.data_dir)
            .args(relative_roots)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            // Without this a failed upload leaves `tar` blocked forever on a
            // pipe nobody is reading.
            .kill_on_drop(true);
        let mut tar = tar
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", self.cfg.tar_bin))?;

        let mut aws = Command::new(&self.cfg.aws_bin);
        aws.arg("s3")
            .arg("cp")
            .arg("-")
            .arg(uri)
            // Progress output on a non-tty is noise; errors still reach stderr.
            .arg("--only-show-errors");
        if expected > 0 {
            // Only ever used to choose a part size. The compressed stream is
            // smaller than this, and over-estimating is the safe direction —
            // under-estimating risks exceeding the 10,000-part ceiling.
            aws.arg("--expected-size").arg(expected.to_string());
        }
        if let Some(endpoint) = &self.cfg.endpoint {
            aws.arg("--endpoint-url").arg(endpoint);
        }
        aws.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut aws = aws
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", self.cfg.aws_bin))?;

        let mut tar_out = tar.stdout.take().ok_or("tar produced no stdout")?;
        let tar_err = tar.stderr.take();
        let mut aws_in = aws.stdin.take().ok_or("aws would not accept stdin")?;

        let pump = async {
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = tar_out
                    .read(&mut buf)
                    .await
                    .map_err(|e| format!("reading the archive stream: {e}"))?;
                if n == 0 {
                    break;
                }
                aws_in
                    .write_all(&buf[..n])
                    .await
                    // A broken pipe here means `aws` died; its stderr is the
                    // real diagnosis, so this reports the symptom and the wait
                    // below supplies the cause.
                    .map_err(|e| format!("writing to the uploader: {e}"))?;
                sent.fetch_add(n as u64, Ordering::Relaxed);
            }
            // Closing stdin is what tells `aws` the object is complete. Without
            // it the upload never finalizes and the wait below hangs until the
            // timeout.
            aws_in
                .shutdown()
                .await
                .map_err(|e| format!("closing the uploader's input: {e}"))?;
            drop(aws_in);
            Ok::<(), String>(())
        };

        let work = async {
            let pumped = pump.await;
            // Both are waited on even when the pump failed: their exit statuses
            // and stderr are the only explanation of *why* it failed.
            let aws_out = aws.wait_with_output().await;
            let tar_status = tar.wait().await;
            let tar_stderr = match tar_err {
                Some(mut e) => {
                    let mut s = String::new();
                    let _ = e.read_to_string(&mut s).await;
                    s
                }
                None => String::new(),
            };

            match aws_out {
                Ok(out) if !out.status.success() => {
                    let msg = String::from_utf8_lossy(&out.stderr);
                    return Err(format!("aws s3 cp failed ({}): {}", out.status, tail(&msg)));
                }
                Err(e) => return Err(format!("waiting for aws: {e}")),
                Ok(_) => {}
            }
            match tar_status {
                // tar exits 1 for "file changed as we read it", which is a
                // warning about a live file rather than a failed archive. 2 is
                // a real error.
                Ok(s) if !s.success() && s.code() != Some(1) => {
                    return Err(format!("tar failed ({s}): {}", tail(&tar_stderr)));
                }
                Err(e) => return Err(format!("waiting for tar: {e}")),
                Ok(_) => {}
            }
            pumped
        };

        match tokio::time::timeout(self.cfg.archive_timeout, work).await {
            Ok(r) => r,
            Err(_) => Err(format!(
                "archive exceeded APP_LB_DISK_ARCHIVE_TIMEOUT_SECS ({}s); both children were \
                 killed and the multipart upload was abandoned. It may leave incomplete parts \
                 in the bucket — a lifecycle rule on `AbortIncompleteMultipartUpload` is the \
                 usual way to clean those up",
                self.cfg.archive_timeout.as_secs()
            )),
        }
    }

    fn finish_archive(
        &self,
        job_id: &str,
        status: ArchiveStatus,
        error: Option<String>,
        purged: bool,
    ) {
        let mut jobs = self.archives.lock().unwrap();
        if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
            job.status = status;
            job.error = error;
            job.purged = purged;
            job.finished_at = Some(now_secs());
        }
    }

    // -- expiry ------------------------------------------------------------

    /// Reclaim every disk whose time is up.
    ///
    /// Declines to do anything at all when the inventory is incomplete. That is
    /// the single most important line in this module: a daemon that is
    /// restarting answers no listing, every disk on the host then looks like
    /// residue, and one sweep would delete the entire fleet's state.
    pub async fn sweep(self: &Arc<Self>) -> SweepOutcome {
        let mut outcome = SweepOutcome::default();
        if self.cfg.ttl_secs == 0 {
            return outcome;
        }

        let inv = self.inventory().await;
        if !inv.complete {
            tracing::warn!(
                reason = inv.incomplete_reason.as_deref().unwrap_or("unknown"),
                "skipping the disk expiry sweep: without the daemon's listings every disk on \
                 this host would look like residue",
            );
            outcome.skipped = true;
            return outcome;
        }

        let now = now_secs();
        // Owned, not borrowed: `purge_resolved` consumes the `DiskInfo` so the
        // paths it acts on come from the inventory it was handed, not from one
        // re-read partway through the loop.
        let due: Vec<DiskInfo> = inv
            .disks
            .into_iter()
            .filter(|d| d.expired(now, self.cfg.ttl_secs))
            .collect();
        if due.is_empty() {
            return outcome;
        }

        let archiving = self.cfg.archive_on_expire && self.cfg.bucket.is_some();
        tracing::info!(
            count = due.len(),
            bytes = due.iter().map(|d| d.bytes).sum::<u64>(),
            ttl_secs = self.cfg.ttl_secs,
            archive = archiving,
            "expiring unclaimed sandbox disks",
        );
        if archiving && due.len() > ARCHIVES_PER_SWEEP {
            // Said out loud rather than left to look like a completed sweep.
            tracing::info!(
                starting = ARCHIVES_PER_SWEEP,
                deferred = due.len() - ARCHIVES_PER_SWEEP,
                "more expired disks than one sweep will archive; the rest wait for the next one",
            );
        }

        for disk in due {
            let age_days = now.saturating_sub(disk.modified_at) / 86_400;
            if archiving {
                if outcome.archived >= ARCHIVES_PER_SWEEP {
                    break;
                }
                // The archive purges on success by itself, so this hands the
                // whole disk over and moves on. A failed upload leaves the disk
                // in place for the next sweep to retry.
                match self.archive_resolved(disk, true) {
                    Ok(_) => outcome.archived += 1,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not archive an expiring disk");
                        outcome.failed += 1;
                    }
                }
                continue;
            }
            tracing::info!(
                sandbox = %disk.sandbox_id,
                deployment = disk.deployment.as_deref().unwrap_or("-"),
                state = disk.state.label(),
                bytes = disk.bytes,
                age_days,
                "expiring sandbox disks",
            );
            // `complete` is true — checked above — and `force` stays false, so
            // every guard still applies to each disk individually.
            match self.purge_resolved(disk, true, false).await {
                Ok(o) => {
                    outcome.purged += 1;
                    outcome.bytes += o.bytes;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not expire a disk");
                    outcome.failed += 1;
                }
            }
        }
        outcome
    }
}

/// What one expiry sweep did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SweepOutcome {
    /// True when the sweep declined to run because the daemon was unreachable.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    pub purged: usize,
    pub archived: usize,
    pub failed: usize,
    pub bytes: u64,
}

/// The last few lines of a child's stderr, for an error message that has to fit
/// on a page.
fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(4);
    lines[start..].join("; ")
}

/// The background service that runs [`DiskStore::sweep`].
pub struct DiskSweeper {
    store: Arc<DiskStore>,
}

impl DiskSweeper {
    pub fn new(store: Arc<DiskStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl pingora_core::services::background::BackgroundService for DiskSweeper {
    async fn start(&self, mut shutdown: pingora_core::server::ShutdownWatch) {
        let period = Duration::from_secs(self.store.cfg.sweep_secs);
        let mut tick = tokio::time::interval(period);
        // A sweep that overran its period must not then fire back-to-back
        // catch-up ticks, which is `Burst`'s default behaviour. Reclaiming
        // gigabytes is not work to do twice in a row because the first pass was
        // slow.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it. A sweep during startup
        // would race the autoscaler's adoption pass, which is what turns a
        // stopped VM back into a claimed one — and a disk purged in that window
        // is one the fleet was about to resume.
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => { self.store.sweep().await; }
                _ = shutdown.changed() => {
                    tracing::info!("disk sweeper shutting down");
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// scanning
// ---------------------------------------------------------------------------

/// One sandbox's files, accumulated across every directory that holds them.
#[derive(Debug, Default)]
struct Group {
    parts: Vec<DiskPart>,
    /// Directories to remove on purge. Loose files in `/tmp` are not roots —
    /// they are listed individually in `parts` and removed by name.
    roots: Vec<PathBuf>,
    bytes: u64,
    apparent_bytes: u64,
    modified_at: u64,
}

/// Walk the daemon's disk directories and group what is there by sandbox.
///
/// Blocking, and called from `spawn_blocking`. Errors are logged and skipped
/// rather than propagated: a directory that cannot be read should cost its own
/// entries, not the whole inventory.
fn scan(cfg: &DiskConfig) -> HashMap<String, Group> {
    let mut groups: HashMap<String, Group> = HashMap::new();

    for root in DISK_ROOTS {
        let dir = cfg.data_dir.join(root);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "cannot read sandbox disk directory");
                continue;
            }
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !valid_sandbox_id(&name) {
                continue;
            }
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let path = entry.path();
            let group = groups.entry(name).or_default();
            group.roots.push(path.clone());
            walk_into(&path, group);
        }
    }

    // The Firecracker per-boot rootfs copy. Present only when the hypervisor
    // died without running its cleanup, which is why it is worth listing: these
    // are pure waste, and they are the largest single files on a busy host.
    match std::fs::read_dir(&cfg.tmp_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let Some(id) = tmp_sandbox_id(&name) else {
                    continue;
                };
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let part = part_of(&entry.path(), &meta, PartKind::Rootfs);
                groups.entry(id.to_string()).or_default().push(part);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(dir = %cfg.tmp_dir.display(), error = %e, "cannot read tmp dir"),
    }

    for group in groups.values_mut() {
        group.recompute();
    }
    groups
}

impl Group {
    fn push(&mut self, part: DiskPart) {
        self.parts.push(part);
    }

    fn recompute(&mut self) {
        self.bytes = self.parts.iter().map(|p| p.bytes).sum();
        self.apparent_bytes = self.parts.iter().map(|p| p.apparent_bytes).sum();
        self.modified_at = self.parts.iter().map(|p| p.modified_at).max().unwrap_or(0);
        // Largest first, so the page's expanded view leads with the file that
        // explains the total.
        self.parts
            .sort_by_key(|p| std::cmp::Reverse((p.bytes, p.apparent_bytes)));
    }
}

/// Recursively account for everything under one sandbox directory.
///
/// Depth is bounded by the daemon's own layout (`<id>/snapshot/<file>`), so this
/// recurses without a depth guard on directories but never follows symlinks —
/// `read_dir` reports the link itself and `symlink_metadata` keeps it that way,
/// so a link planted in the run directory cannot walk the scan out of it.
fn walk_into(dir: &Path, group: &mut Group) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk_into(&path, group);
        } else if meta.is_file() {
            group.push(part_of(&path, &meta, classify(&path)));
        }
    }
}

/// What a file under a sandbox directory is, from its name and location.
fn classify(path: &Path) -> PartKind {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let in_snapshot = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        == Some("snapshot");

    if in_snapshot {
        return PartKind::Snapshot;
    }
    match name {
        "data.ext4" => PartKind::Data,
        "rootfs.ext4" => PartKind::Rootfs,
        n if n.starts_with("mount") && n.ends_with(".ext4") => PartKind::Mount,
        _ => PartKind::Other,
    }
}

fn part_of(path: &Path, meta: &std::fs::Metadata, kind: PartKind) -> DiskPart {
    use std::os::unix::fs::MetadataExt;
    DiskPart {
        kind,
        path: path.display().to_string(),
        // `blocks()` is in 512-byte units by POSIX definition, regardless of the
        // filesystem's block size. This is what makes a sparse 8 GiB data disk
        // report the 200 MiB it actually occupies.
        bytes: meta.blocks().saturating_mul(512),
        apparent_bytes: meta.len(),
        modified_at: mtime_secs(meta),
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Prefixes both drivers use for their per-boot scratch in `tmp_dir`.
const TMP_PREFIXES: [&str; 2] = ["firecracker-", "kvm-"];

/// The sandbox a `/tmp` **disk image** belongs to, if it is one.
///
/// `firecracker-<id>-rootfs.ext4`, `kvm-<id>-rootfs.ext4`, `kvm-<id>-mount0.ext4`.
/// Only the images: the `.socket` and `-vmm.log` siblings are removed by
/// [`remove_all`] but are not worth a row of their own — one is zero bytes and
/// the other a few hundred. `heyvm prune` matches the same two prefixes.
fn tmp_sandbox_id(file_name: &str) -> Option<&str> {
    let rest = TMP_PREFIXES
        .iter()
        .find_map(|p| file_name.strip_prefix(p))?;
    let stem = rest.strip_suffix(".ext4")?;
    let (id, tail) = stem.rsplit_once('-')?;
    let is_image = tail == "rootfs"
        || tail
            .strip_prefix("mount")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    (is_image && valid_sandbox_id(id)).then_some(id)
}

/// Whether a `/tmp` file is scratch belonging to `sandbox_id` — image, socket,
/// log or config alike.
///
/// Broader than [`tmp_sandbox_id`], because a purge must take the siblings too.
/// The boundary check is what keeps `sb-1` from claiming `sb-12`'s files.
fn tmp_belongs_to(file_name: &str, sandbox_id: &str) -> bool {
    TMP_PREFIXES.iter().any(|p| {
        file_name
            .strip_prefix(p)
            .and_then(|rest| rest.strip_prefix(sandbox_id))
            .is_some_and(|tail| tail.is_empty() || tail.starts_with(['-', '.']))
    })
}

/// What a purge actually freed: the parts that were under a path it removed.
///
/// Not simply `disk.bytes`, which is what the sandbox occupied *before* — the
/// two agree on a clean purge and differ on a partial one, and it is exactly
/// the partial one where an operator is trying to work out how much is left.
/// Reporting the size of something still on disk as freed is the same mistake
/// as reporting a failed purge as a success, one level down.
fn freed(parts: &[DiskPart], removed: &[String]) -> u64 {
    parts
        .iter()
        .filter(|p| {
            removed.iter().any(|r| {
                // A removed directory covers everything beneath it; a removed
                // file is only itself. The separator check is what stops
                // `run/sb-1` from claiming `run/sb-12`'s bytes.
                p.path == *r || p.path.strip_prefix(r.as_str()).is_some_and(|t| t.starts_with('/'))
            })
        })
        .map(|p| p.bytes)
        .sum()
}

/// Remove a directory tree, naming the path that actually stopped it.
///
/// `std::fs::remove_dir_all` returns a bare `io::Error` with no path attached,
/// so every failure anywhere under a sandbox directory was reported against the
/// sandbox directory itself. That is the wrong path on the host this feature
/// exists for: heyvmd creates the files, app-lb usually owns only the directory
/// above them, so the denial is a level or two down and the message named a
/// directory app-lb could already write. An operator following it `chown`s that
/// directory, sweeps again, gets the identical line, and reasonably concludes
/// the sweep is broken rather than the permissions.
///
/// `Ok(true)` if something was there and is now gone, `Ok(false)` if there was
/// nothing to remove, `Err` with the first path that could not be — first, and
/// not all of them, because this stops where `remove_dir_all` stops and one
/// accurate path is the whole diagnosis. Every file under a directory nobody
/// can write into is the same fact repeated.
fn remove_tree(path: &Path) -> Result<bool, String> {
    // `symlink_metadata`, so a symlink is unlinked as itself and never
    // followed: nothing under these roots is supposed to be one, and a purge
    // that walked through one would delete outside the two directories this
    // module manages.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };

    if !meta.is_dir() {
        return unlinked(std::fs::remove_file(path), path);
    }

    // Read errors name this directory, which is correct: not being able to list
    // it is what stopped the removal.
    let entries = std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", path.display()))?;
        remove_tree(&entry.path())?;
    }
    unlinked(std::fs::remove_dir(path), path)
}

/// One unlink, with the path attached to whatever it returns.
fn unlinked(result: std::io::Result<()>, path: &Path) -> Result<bool, String> {
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Remove every path this module associates with a sandbox.
///
/// Every path is *constructed* from the validated id and the configured roots,
/// never taken from the inventory — so even a stale or hand-edited inventory
/// cannot direct a delete outside the two directories app-lb manages.
fn remove_all(cfg: &DiskConfig, sandbox_id: &str) -> (Vec<String>, Vec<String>) {
    debug_assert!(valid_sandbox_id(sandbox_id), "purge with an unvalidated id");
    let mut removed = Vec::new();
    let mut failed = Vec::new();

    for root in DISK_ROOTS {
        let dir = cfg.data_dir.join(root).join(sandbox_id);
        match remove_tree(&dir) {
            Ok(true) => removed.push(dir.display().to_string()),
            // Already gone: a purge racing the daemon's own cleanup is a purge
            // with nothing left to do, not a failure.
            Ok(false) => {}
            Err(e) => failed.push(e),
        }
    }

    // Enumerated rather than guessed by name: the KVM driver's mount disks are
    // numbered without an upper bound, so a fixed list would leave whatever it
    // did not think of behind. Each candidate is a direct child of `tmp_dir`
    // from `read_dir`, so joining it cannot leave that directory, and
    // `tmp_belongs_to` is what confines the match to this sandbox.
    match std::fs::read_dir(&cfg.tmp_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if !tmp_belongs_to(&name, sandbox_id) {
                    continue;
                }
                let path = cfg.tmp_dir.join(&name);
                match std::fs::remove_file(&path) {
                    Ok(()) => removed.push(path.display().to_string()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => failed.push(format!("{}: {e}", path.display())),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => failed.push(format!("{}: {e}", cfg.tmp_dir.display())),
    }

    (removed, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("app-lb-disks-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg_at(root: &Path) -> DiskConfig {
        DiskConfig {
            data_dir: root.join("heyo"),
            tmp_dir: root.join("tmp"),
            state_path: root.join("disks.json"),
            ttl_secs: DEFAULT_TTL_SECS,
            sweep_secs: 3600,
            aws_bin: "aws".into(),
            tar_bin: "tar".into(),
            bucket: None,
            prefix: "app-lb/disks".into(),
            endpoint: None,
            archive_on_expire: false,
            archive_timeout: Duration::from_secs(60),
        }
    }

    fn write(path: &Path, len: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let f = std::fs::File::create(path).unwrap();
        f.set_len(len).unwrap();
    }

    fn disk(state: DiskState, claimed: bool, retain: bool, modified_at: u64) -> DiskInfo {
        DiskInfo {
            sandbox_id: "sb-1".into(),
            name: None,
            deployment: None,
            state,
            claimed,
            retain,
            note: None,
            bytes: 1024,
            apparent_bytes: 1024,
            modified_at,
            expires_at: None,
            held_by: hold_reason(state, claimed, retain),
            archived: None,
            parts: vec![],
            roots: vec![],
        }
    }

    /// The id is a URL path segment on its way to `remove_dir_all`. Everything
    /// that could escape the two directories this module owns must be refused
    /// before it is joined to anything.
    mod ids {
        use super::*;

        #[test]
        fn plain_sandbox_ids_are_accepted() {
            assert!(valid_sandbox_id("sb-0226245f"));
            assert!(valid_sandbox_id("sb_1"));
            assert!(valid_sandbox_id("A0"));
        }

        #[test]
        fn traversal_and_separators_are_refused() {
            for bad in [
                "", "..", ".", "../etc", "sb/../..", "sb-1/", "/sb-1", "sb 1", "sb-1\0", "sb.1",
                "~", "*",
            ] {
                assert!(!valid_sandbox_id(bad), "accepted {bad:?}");
            }
            assert!(!valid_sandbox_id(&"x".repeat(129)));
        }

        /// A dot is refused, which is what keeps `..` from ever reaching a join
        /// — the traversal case above is a consequence of this rule, not a
        /// separate check that could drift away from it.
        #[test]
        fn a_dot_alone_is_enough_to_refuse() {
            assert!(!valid_sandbox_id("sb-1.ext4"));
        }
    }

    mod expiry {
        use super::*;

        #[test]
        fn an_old_unclaimed_orphan_expires() {
            let d = disk(DiskState::Orphan, false, false, 0);
            assert!(d.expired(DEFAULT_TTL_SECS, DEFAULT_TTL_SECS));
        }

        #[test]
        fn a_young_disk_does_not() {
            let d = disk(DiskState::Orphan, false, false, 100);
            assert!(!d.expired(100 + DEFAULT_TTL_SECS - 1, DEFAULT_TTL_SECS));
            assert!(d.expired(100 + DEFAULT_TTL_SECS, DEFAULT_TTL_SECS));
        }

        /// The four guards, each on its own. Any one of them holding is enough,
        /// and no age makes it go away — which is the property that matters,
        /// because the sweep only ever consults `expired`.
        #[test]
        fn every_hold_survives_any_age() {
            let ancient = u64::MAX / 2;
            for d in [
                disk(DiskState::Running, false, false, 0),
                disk(DiskState::Unknown, false, false, 0),
                disk(DiskState::Stopped, true, false, 0),
                disk(DiskState::Stopped, false, true, 0),
            ] {
                assert!(d.held_by.is_some(), "expected a hold for {:?}", d.state);
                assert!(!d.expired(ancient, DEFAULT_TTL_SECS), "{:?} expired", d.state);
            }
        }

        /// A running VM's disk reports *why* it is held rather than the first
        /// rule that matched, so the page says something true even when several
        /// reasons apply at once.
        #[test]
        fn the_strongest_hold_is_the_one_reported() {
            let d = disk(DiskState::Running, true, true, 0);
            assert_eq!(d.held_by, Some("the sandbox is running"));
            let d = disk(DiskState::Stopped, true, true, 0);
            assert_eq!(d.held_by, Some("a deployment expects to resume it"));
        }

        #[test]
        fn ttl_zero_disables_expiry_entirely() {
            let d = disk(DiskState::Orphan, false, false, 0);
            assert!(!d.expired(u64::MAX / 2, 0));
        }
    }

    mod scanning {
        use super::*;

        #[test]
        fn groups_a_sandbox_across_every_directory_that_holds_it() {
            let root = scratch("group");
            let cfg = cfg_at(&root);
            std::fs::create_dir_all(&cfg.tmp_dir).unwrap();

            write(&cfg.data_dir.join("run/sb-1/data.ext4"), 4096);
            write(&cfg.data_dir.join("run/sb-1/snapshot/state"), 512);
            write(&cfg.data_dir.join("kvm/sb-1/rootfs.ext4"), 8192);
            write(&cfg.data_dir.join("kvm/sb-1/mount0.ext4"), 2048);
            write(&cfg.tmp_dir.join("firecracker-sb-1-rootfs.ext4"), 1024);
            write(&cfg.data_dir.join("run/sb-2/data.ext4"), 4096);
            // A neighbour whose id shares sb-1's prefix. It must land in its
            // own group, not sb-1's.
            write(&cfg.tmp_dir.join("firecracker-sb-12-rootfs.ext4"), 64);

            let groups = scan(&cfg);
            assert_eq!(groups.len(), 3, "one group per sandbox");
            assert_eq!(groups["sb-12"].parts.len(), 1, "sb-1 must not absorb sb-12");

            let one = &groups["sb-1"];
            assert_eq!(one.parts.len(), 5);
            assert_eq!(one.apparent_bytes, 4096 + 512 + 8192 + 2048 + 1024);
            // Both directories are purge roots; the /tmp file is not — it is
            // removed by name.
            assert_eq!(one.roots.len(), 2);

            let kinds: HashSet<PartKind> = one.parts.iter().map(|p| p.kind).collect();
            assert!(kinds.contains(&PartKind::Data));
            assert!(kinds.contains(&PartKind::Rootfs));
            assert!(kinds.contains(&PartKind::Mount));
            assert!(kinds.contains(&PartKind::Snapshot));
        }

        /// A sparse file is the normal case here — the daemon creates every data
        /// disk at its full nominal size — so reporting `len()` would tell an
        /// operator 8 GiB is recoverable when 200 MiB is.
        #[test]
        fn sizes_are_allocated_blocks_not_nominal_length() {
            let root = scratch("sparse");
            let cfg = cfg_at(&root);
            write(&cfg.data_dir.join("run/sb-1/data.ext4"), 64 * 1024 * 1024);

            let groups = scan(&cfg);
            let g = &groups["sb-1"];
            assert_eq!(g.apparent_bytes, 64 * 1024 * 1024);
            assert!(
                g.bytes < g.apparent_bytes / 2,
                "a hole was counted as occupied: {} of {}",
                g.bytes,
                g.apparent_bytes
            );
        }

        #[test]
        fn foreign_directories_and_files_are_ignored() {
            let root = scratch("foreign");
            let cfg = cfg_at(&root);
            std::fs::create_dir_all(&cfg.tmp_dir).unwrap();
            // Not a valid id, so not a sandbox directory.
            write(&cfg.data_dir.join("run/../stray.ext4"), 16);
            std::fs::create_dir_all(cfg.data_dir.join("run/not.a.sandbox")).unwrap();
            // Somebody else's file in /tmp.
            write(&cfg.tmp_dir.join("firecracker-something-else.log"), 16);
            write(&cfg.tmp_dir.join("qemu-sb-1-rootfs.ext4"), 16);

            assert!(scan(&cfg).is_empty());
        }

        #[test]
        fn a_missing_data_directory_is_not_an_error() {
            let root = scratch("missing");
            let cfg = cfg_at(&root);
            assert!(scan(&cfg).is_empty());
        }

        /// Both drivers' scratch, matching the two prefixes `heyvm prune` uses.
        #[test]
        fn tmp_image_names_round_trip() {
            assert_eq!(
                tmp_sandbox_id("firecracker-sb-19d69295-rootfs.ext4"),
                Some("sb-19d69295")
            );
            assert_eq!(tmp_sandbox_id("kvm-sb-1-rootfs.ext4"), Some("sb-1"));
            assert_eq!(tmp_sandbox_id("kvm-sb-1-mount0.ext4"), Some("sb-1"));
            assert_eq!(tmp_sandbox_id("kvm-sb-1-mount12.ext4"), Some("sb-1"));

            // Not images: listed nowhere, though a purge still removes them.
            assert_eq!(tmp_sandbox_id("firecracker-sb-1-vmm.log"), None);
            assert_eq!(tmp_sandbox_id("firecracker-sb-1.socket"), None);
            assert_eq!(tmp_sandbox_id("kvm-sb-1-mount.ext4"), None);
            assert_eq!(tmp_sandbox_id("firecracker--rootfs.ext4"), None);
            // Somebody else's file.
            assert_eq!(tmp_sandbox_id("qemu-sb-1-rootfs.ext4"), None);
            // The id inside the name is validated too, so a crafted filename
            // cannot smuggle a traversal in through the /tmp scan.
            assert_eq!(tmp_sandbox_id("firecracker-../../etc-rootfs.ext4"), None);
        }

        /// The purge matcher is broader than the listing one, and must not let
        /// one sandbox's purge take another's files.
        #[test]
        fn tmp_ownership_stops_at_a_name_boundary() {
            for name in [
                "firecracker-sb-1-rootfs.ext4",
                "firecracker-sb-1-vmm.log",
                "firecracker-sb-1.socket",
                "kvm-sb-1-mount3.ext4",
            ] {
                assert!(tmp_belongs_to(name, "sb-1"), "{name} should be sb-1's");
            }
            // The prefix-collision case: sb-1 must not claim sb-12's disks.
            for name in [
                "firecracker-sb-12-rootfs.ext4",
                "kvm-sb-1x-rootfs.ext4",
                "firecracker-sb-10.socket",
            ] {
                assert!(!tmp_belongs_to(name, "sb-1"), "{name} is not sb-1's");
            }
            assert!(!tmp_belongs_to("qemu-sb-1-rootfs.ext4", "sb-1"));
        }
    }

    mod removal {
        use super::*;

        #[test]
        fn removes_both_roots_and_the_tmp_scratch() {
            let root = scratch("remove");
            let cfg = cfg_at(&root);
            std::fs::create_dir_all(&cfg.tmp_dir).unwrap();
            write(&cfg.data_dir.join("run/sb-1/data.ext4"), 32);
            write(&cfg.data_dir.join("run/sb-1/snapshot/mem"), 32);
            write(&cfg.data_dir.join("kvm/sb-1/rootfs.ext4"), 32);
            write(&cfg.tmp_dir.join("firecracker-sb-1-rootfs.ext4"), 32);
            write(&cfg.tmp_dir.join("firecracker-sb-1-vmm.log"), 32);
            write(&cfg.tmp_dir.join("firecracker-sb-1.socket"), 0);
            // The KVM driver's mount disks are numbered without an upper bound,
            // so they have to be enumerated rather than guessed by name.
            write(&cfg.tmp_dir.join("kvm-sb-1-mount7.ext4"), 32);
            // Neighbours that must survive, including a prefix collision.
            write(&cfg.data_dir.join("run/sb-2/data.ext4"), 32);
            write(&cfg.tmp_dir.join("firecracker-sb-12-rootfs.ext4"), 32);

            let (removed, failed) = remove_all(&cfg, "sb-1");
            assert!(failed.is_empty(), "{failed:?}");
            // Two directories plus all four `/tmp` files.
            assert_eq!(removed.len(), 6, "{removed:?}");
            assert!(!cfg.data_dir.join("run/sb-1").exists());
            assert!(!cfg.data_dir.join("kvm/sb-1").exists());
            assert!(cfg.data_dir.join("run/sb-2/data.ext4").exists());
            assert!(
                cfg.tmp_dir.join("firecracker-sb-12-rootfs.ext4").exists(),
                "purging sb-1 must not take sb-12's scratch",
            );
            assert!(scan(&cfg).contains_key("sb-2"));
        }

        /// A sandbox that only ever existed in one of the two directories is the
        /// common case, and the absent half must not read as a failure.
        #[test]
        fn absent_paths_are_not_failures() {
            let root = scratch("absent");
            let cfg = cfg_at(&root);
            write(&cfg.data_dir.join("run/sb-1/data.ext4"), 32);

            let (removed, failed) = remove_all(&cfg, "sb-1");
            assert_eq!(removed.len(), 1);
            assert!(failed.is_empty());
        }

        /// The shape that made a fleet's worth of residue look reclaimed: the
        /// disks are readable, so the inventory lists them and reports their
        /// size, but the directory holding them is not writable, so nothing can
        /// be unlinked. This is what a host where heyvmd runs as one user and
        /// app-lb as another looks like from inside `remove_all`.
        ///
        /// Skipped under root, which ignores the permission bits entirely.
        #[test]
        fn an_undeletable_directory_is_a_failure_not_a_silent_no_op() {
            use std::os::unix::fs::PermissionsExt;

            let root = scratch("denied");
            let cfg = cfg_at(&root);
            write(&cfg.data_dir.join("run/sb-1/data.ext4"), 32);

            // Unlinking a file needs write on its *parent*, so this is the
            // directory the purge cannot modify.
            let dir = cfg.data_dir.join("run/sb-1");
            let original = std::fs::metadata(&dir).unwrap().permissions();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            let (removed, failed) = remove_all(&cfg, "sb-1");
            std::fs::set_permissions(&dir, original).unwrap();

            // Detected by outcome rather than by uid: if the unlink went through
            // despite the bits, this is running as root and there is nothing
            // here to assert.
            if failed.is_empty() {
                eprintln!("skipped: the removal succeeded, so this is running as root");
                return;
            }

            assert!(removed.is_empty(), "nothing could be removed: {removed:?}");
            assert_eq!(failed.len(), 1, "and that has to be reported: {failed:?}");
            // The *file*, not the directory holding it. Unlinking needs write on
            // the parent, so the parent is what has to change — but an operator
            // reading this needs to be told which path the kernel refused, and
            // `remove_dir_all` would have named `run/sb-1` here whatever went
            // wrong inside it. See `remove_tree`.
            assert!(failed[0].contains("run/sb-1/data.ext4"), "{failed:?}");
            // Still there, still counted, still taking the space — which is why
            // `purge_resolved` turns exactly this pair into an error rather than
            // a purge that happened to free nothing.
            assert!(dir.join("data.ext4").exists());
            assert!(scan(&cfg).contains_key("sb-1"));
        }

        /// The us2 shape, and the reason `remove_tree` exists: the sandbox
        /// directory is app-lb's and perfectly writable, and the denial is a
        /// level down in the directory heyvmd made inside it. `remove_dir_all`
        /// reported this as a failure on `run/sb-1` — a directory the operator
        /// then owns, chowns, and watches fail again identically.
        ///
        /// Skipped under root, which ignores the permission bits entirely.
        #[test]
        fn a_failure_inside_the_sandbox_directory_names_the_path_inside() {
            use std::os::unix::fs::PermissionsExt;

            let root = scratch("denied-nested");
            let cfg = cfg_at(&root);
            write(&cfg.data_dir.join("run/sb-1/data.ext4"), 32);
            write(&cfg.data_dir.join("run/sb-1/snapshot/mem"), 32);

            let inner = cfg.data_dir.join("run/sb-1/snapshot");
            let original = std::fs::metadata(&inner).unwrap().permissions();
            std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o555)).unwrap();

            let (removed, failed) = remove_all(&cfg, "sb-1");
            std::fs::set_permissions(&inner, original).unwrap();

            if failed.is_empty() {
                eprintln!("skipped: the removal succeeded, so this is running as root");
                return;
            }

            assert!(removed.is_empty(), "the sandbox dir survives: {removed:?}");
            assert_eq!(failed.len(), 1, "{failed:?}");
            assert!(
                failed[0].contains("run/sb-1/snapshot/mem"),
                "the path that was actually refused, not the one two levels up: {failed:?}",
            );
        }

        /// Every path is rebuilt from the roots and the validated id, so nothing
        /// a caller supplies can steer a delete. `remove_all` is only ever
        /// reached through `purge`, which validates first; this pins the
        /// construction itself.
        #[test]
        fn paths_are_confined_to_the_configured_roots() {
            let root = scratch("confined");
            let cfg = cfg_at(&root);
            let outside = root.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            write(&outside.join("precious"), 32);

            // Even if this reached `remove_all`, the joins keep it under the
            // roots — where no such directory exists — rather than escaping.
            let (removed, _) = remove_all(&cfg, "sb-nonexistent");
            assert!(removed.is_empty());
            assert!(outside.join("precious").exists());
        }
    }

    mod policy {
        use super::*;

        fn store(root: &Path) -> DiskStore {
            DiskStore::new(
                cfg_at(root),
                VmManager::new(Some("http://127.0.0.1:1".into())),
                Arc::new(Registry::new(
                    root.join("state.json").to_str().unwrap(),
                )),
            )
        }

        #[test]
        fn retention_round_trips_through_the_file() {
            let root = scratch("policy");
            let s = store(&root);
            s.set_policy("sb-1", Some(true), Some(Some("keeps the demo".into())))
                .unwrap();

            let reloaded = store(&root);
            assert_eq!(reloaded.load().unwrap(), 1);
            let p = reloaded.policies.lock().unwrap();
            assert!(p["sb-1"].retain);
            assert_eq!(p["sb-1"].note.as_deref(), Some("keeps the demo"));
        }

        /// An absent field leaves the other alone, so the page can offer a pin
        /// toggle and a note field without either clobbering the other.
        #[test]
        fn a_partial_update_leaves_the_rest_alone() {
            let root = scratch("partial");
            let s = store(&root);
            s.set_policy("sb-1", Some(true), Some(Some("why".into()))).unwrap();
            s.set_policy("sb-1", Some(false), None).unwrap();

            let p = s.policies.lock().unwrap();
            assert_eq!(p["sb-1"].note.as_deref(), Some("why"));
            assert!(!p["sb-1"].retain);
        }

        /// A partial purge reports what it reclaimed, not what was there.
        ///
        /// The counterpart of the failed-purge case below, one level down: an
        /// operator looking at a purge that could not remove everything is
        /// trying to work out how much is left, and the pre-purge total is the
        /// one number that cannot tell them.
        #[test]
        fn freed_bytes_count_only_what_was_removed() {
            let parts = |paths: &[(&str, u64)]| -> Vec<DiskPart> {
                paths
                    .iter()
                    .map(|(path, bytes)| DiskPart {
                        kind: PartKind::Data,
                        path: (*path).to_string(),
                        bytes: *bytes,
                        apparent_bytes: *bytes,
                        modified_at: 0,
                    })
                    .collect()
            };
            let all = parts(&[
                ("/d/run/sb-1/data.ext4", 100),
                ("/d/kvm/sb-1/rootfs.ext4", 200),
                ("/tmp/firecracker-sb-1-rootfs.ext4", 30),
            ]);

            // A clean purge: every root went, so this is the whole sandbox.
            assert_eq!(
                freed(&all, &["/d/run/sb-1".into(), "/d/kvm/sb-1".into(),
                              "/tmp/firecracker-sb-1-rootfs.ext4".into()]),
                330,
            );
            // A partial one: only the directory that could be removed counts.
            assert_eq!(freed(&all, &["/d/run/sb-1".into()]), 100);
            assert_eq!(freed(&all, &[]), 0);
            // And a prefix must not claim a neighbour's bytes.
            let neighbour = parts(&[("/d/run/sb-12/data.ext4", 999)]);
            assert_eq!(freed(&neighbour, &["/d/run/sb-1".into()]), 0);
        }

        /// The whole failed-purge path, end to end.
        ///
        /// A purge that removed nothing must say so rather than returning an
        /// outcome claiming the size it *would* have freed — that is what let a
        /// sweep report a thousand disks reclaimed while the host filled up. It
        /// must also leave the policy alone: forgetting a `retain` flag that is
        /// still protecting a disk still on disk is the wrong half of the
        /// operation to complete.
        #[tokio::test]
        async fn a_purge_that_removes_nothing_is_an_error_and_keeps_its_policy() {
            use std::os::unix::fs::PermissionsExt;

            let root = scratch("purge-denied");
            let s = store(&root);
            let cfg = cfg_at(&root);
            write(&cfg.data_dir.join("run/sb-1/data.ext4"), 32);
            s.set_policy("sb-1", Some(true), Some(Some("still needed".into())))
                .unwrap();

            let dir = cfg.data_dir.join("run/sb-1");
            let original = std::fs::metadata(&dir).unwrap().permissions();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            // Forced, because this harness points at a dead daemon port, which
            // makes every disk `Unknown` and therefore held.
            let outcome = s.purge("sb-1", true).await;
            std::fs::set_permissions(&dir, original).unwrap();

            match outcome {
                Err(DiskError::PurgeFailed { sandbox_id, failed }) => {
                    assert_eq!(sandbox_id, "sb-1");
                    assert!(failed.iter().any(|f| f.contains("run/sb-1")), "{failed:?}");
                    // The message has to point at the actual cause; an operator
                    // reading "0 bytes freed" learns nothing.
                    let shown = DiskError::PurgeFailed {
                        sandbox_id: "sb-1".into(),
                        failed,
                    }
                    .to_string();
                    assert!(shown.contains("nothing was reclaimed"), "{shown}");
                    assert!(shown.contains("ownership"), "{shown}");
                }
                Ok(_) => {
                    eprintln!("skipped: the removal succeeded, so this is running as root");
                    return;
                }
                Err(e) => panic!("wrong error: {e}"),
            }

            assert!(dir.join("data.ext4").exists(), "the disk is still there");
            assert!(
                s.policies.lock().unwrap()["sb-1"].retain,
                "a retain flag must survive a purge that failed to delete anything",
            );
        }

        /// An entry that says nothing is dropped, so the file tracks decisions
        /// rather than growing one line per sandbox that ever booted.
        #[test]
        fn an_entry_back_at_its_defaults_is_forgotten() {
            let root = scratch("empty-policy");
            let s = store(&root);
            s.set_policy("sb-1", Some(true), None).unwrap();
            assert!(s.policies.lock().unwrap().contains_key("sb-1"));
            s.set_policy("sb-1", Some(false), Some(None)).unwrap();
            assert!(s.policies.lock().unwrap().is_empty());
        }

        #[test]
        fn a_bad_id_never_reaches_the_file() {
            let root = scratch("bad-policy");
            let s = store(&root);
            assert!(matches!(
                s.set_policy("../etc", Some(true), None),
                Err(DiskError::BadId(_))
            ));
            assert!(s.policies.lock().unwrap().is_empty());
        }

        #[test]
        fn a_missing_state_file_loads_as_empty() {
            let root = scratch("no-state");
            assert_eq!(store(&root).load().unwrap(), 0);
        }
    }

    mod config {
        use super::*;

        #[test]
        fn the_default_ttl_is_seven_days() {
            assert_eq!(DEFAULT_TTL_SECS, 604_800);
        }

        /// The archive key carries a timestamp, so re-archiving a disk never
        /// overwrites the copy that is already in the bucket.
        #[test]
        fn tail_keeps_the_last_lines_of_a_childs_stderr() {
            assert_eq!(tail(""), "");
            assert_eq!(tail("one\n\ntwo\n"), "one; two");
            let many = (1..=10).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
            assert_eq!(tail(&many), "7; 8; 9; 10");
        }
    }
}
