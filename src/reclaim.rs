//! Automatic reclamation of stranded disk slack on stopped VMs.
//!
//! Firecracker's virtio-blk doesn't pass discard through to the host, so blocks
//! freed inside a guest are never punched out of its sparse `data.ext4`: each
//! disk ratchets toward its provisioned max (`PG_VM_POOL_DATA_DISK_GB`) and
//! stays there, and the machine's real VM capacity becomes
//! `host_disk / provisioned_max` instead of `host_disk / live_data`. The space
//! can only be returned offline — loop-mount a *stopped* VM's disk on the host
//! and `fstrim` it (the loop device does translate discard into hole punches on
//! the backing file), which is what `reclaim-disks.sh` does.
//!
//! This module makes the pooler run that command itself instead of a human:
//! periodically, shortly after the idle reaper stops VMs (so a just-reaped VM's
//! slack comes back within a minute rather than at the next interval), and on
//! demand from the dashboard. The command needs root for loop-setup/mount, so a
//! non-root pooler runs it through a `NOPASSWD` sudoers entry, e.g.
//!
//! ```text
//! PG_VM_POOL_RECLAIM_CMD="sudo -n /opt/pg-vm-pool/reclaim-disks.sh /workbooks/heyvm/run"
//! ```
//!
//! Safety is layered. The script skips any disk a running VM holds open
//! (device:inode match against every open fd on the host) — but it takes that
//! snapshot *once, at pass start*, so a VM booted mid-pass is invisible to it
//! and its filesystem could be fscked/shrunk underneath the running guest,
//! which destroys it. All VM boots go through this process, so the pooler
//! closes that window itself. There are two mechanisms for it, and which one
//! is in play depends on what the deployed script supports.
//!
//! # Per-disk locks (preferred)
//!
//! The hazard is per-*disk*: a boot of VM A is only endangered by work on A's
//! disk. So the exclusion is keyed by disk. Both sides `flock` a well-known
//! file per sandbox — `<run dir>/.reclaim-locks/<id>.lock` — the pooler across
//! `start()`, the script across one disk's whole pipeline, skipping any disk it
//! cannot lock. A boot of an untouched VM therefore never waits at all, and a
//! pass keeps running through cold starts and warm-spare restarts instead of
//! surrendering its progress to them.
//!
//! The lock alone does not close the snapshot race, because a permit is only
//! held across `start()`: a VM that booted mid-pass holds its disk open but is
//! invisible to a scan taken before it existed, and the gate used to rule that
//! out only by making mid-pass boots impossible. So the script asks the in-use
//! question twice — once against the snapshot and then, for whatever survives
//! that, once *live while holding the disk's lock*, which is the only moment
//! the answer is guaranteed to stay true. Together they make the exclusion
//! exact rather than brute-force: the snapshot covers VMs already running
//! (including across a pooler restart, which drops every lock), the live check
//! covers VMs that booted during the pass, and the lock is what makes the live
//! check's answer hold for as long as that disk is being worked on.
//!
//! Both sides have to agree, so the mode is negotiated, never assumed. A script
//! that implements locking writes [`LOCK_PROTOCOL`] into the lock dir's
//! [`LOCK_MARKER`] at every pass start; [`Reclaimer::run_once`] deletes that
//! marker before launching the child and re-reads it once the child has exited.
//! Present and recognised ⇒ per-disk mode; absent ⇒ the deployed script
//! predates locking, and the global gate below is used instead. The latch is
//! self-correcting in both directions, so a rollback to an older script drops
//! the pooler back to the gate on that script's first pass. (One narrow hole is
//! accepted deliberately: a rollback landing between a boot taking a disk lock
//! and the older script reaching that same disk. That needs an operator
//! rollback inside a sub-second window, and is not worth a config knob.)
//!
//! # The global boot gate (fallback)
//!
//! With no run dir (nowhere to put lock files), or against a script that does
//! not honour them, boots and passes are instead kept disjoint in time:
//! [`boot_permit`] is the read side of a gate whose write side is held for the
//! full duration of every reclaim run. Correct, but coarse — every boot on the
//! host waits on work being done to a disk that isn't its own.
//!
//! **Boots make passes yield.** On a large fleet a pass is not "seconds": it
//! fscks and trims every stopped disk, and [`RECLAIM_TIMEOUT`] lets it run for
//! half an hour. Because tokio's `RwLock` is fair, a boot that arrives one
//! second into such a pass waits for all of it — so every warm-spare restart,
//! every client cold start and every thaw stalls for minutes at a time, which
//! is indistinguishable from an outage and (with each waiter holding a bring-up
//! slot, as they used to) starves the whole pooler.
//!
//! Serving a database beats reclaiming slack, so a waiting boot asks the pass
//! to stop: it registers in [`BOOTS_WAITING`], [`Reclaimer::run_once`] creates
//! the script's **stop file**, and the script yields at its next safe point —
//! between disks, at stage boundaries inside a disk, or by killing its own
//! discard-stage fsck (safe: it only punches free blocks on a verified-clean
//! filesystem). Only a journal-recovery fsck or an in-progress shrink must
//! run to completion, so the boot's worst case is one fsck/resize on the
//! largest disk, not a whole pass or even a whole per-disk pipeline. The
//! gate is released only once the child has actually exited.
//!
//! Preemption is not exclusive to the fallback: in per-disk mode a boot that
//! collides with the script on its *own* disk registers the same way. The disk
//! the pass is on is precisely the one that boot wants, so asking it to yield
//! is the shortest path to the lock.
//!
//! It is deliberately *not* a kill. The command runs through `sudo`, so the
//! pooler can only signal the shell it spawned — `sudo` and the `e2fsck` under
//! it are root-owned and survive. Killing would therefore hand the gate back
//! while a root fsck is still writing to a disk the pooler is about to boot:
//! exactly the corruption the gate exists to prevent. A cooperative stop is
//! also the only kind that is safe mid-fsck at all. If the deployed script
//! predates the stop file (or the run dir isn't configured, so there is nowhere
//! to put it) nothing breaks — the pass simply runs to completion as before,
//! and the boot waits, which is the old behaviour and is still correct.
//!
//! Progress across yields is the script's job: it records a cursor and the next
//! pass resumes after the last disk it finished, so a host that yields often
//! still walks the whole fleet.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::sync::{RwLock, RwLockReadGuard};
use tracing::{debug, error, info, warn};

/// Hard bound on one reclaim run. A run fscks + mounts + trims every stopped
/// disk, so a big fleet legitimately takes minutes — but a wedged mount must
/// not pin the single-flight flag forever. On expiry the child is killed
/// (`kill_on_drop`); the script cleans up per disk, so nothing is left mounted.
const RECLAIM_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Delay between the idle reaper stopping VMs and the follow-up reclaim run.
/// Gives the daemon time to fully tear the Firecracker processes down so their
/// disks no longer show as open — if one is still closing, the script's in-use
/// guard skips it and the periodic run catches it later.
pub const POST_STOP_RECLAIM_DELAY: Duration = Duration::from_secs(30);

/// Delay before the first periodic run after startup — long enough to let the
/// pooler finish coming up and restore any warm VMs, short enough that frequent
/// redeploys can't starve reclamation (see `registry::supervise`).
pub const RECLAIM_FIRST_DELAY: Duration = Duration::from_secs(300);

/// Directory under the run dir holding one lock file per sandbox, and the
/// marker the script writes to declare that it honours them. Created
/// world-writable: a non-root pooler and a root (`sudo`) script both create
/// files here, and whichever gets there first must not lock the other out.
pub const LOCK_DIR: &str = ".reclaim-locks";
/// Marker file inside [`LOCK_DIR`]; its contents are the negotiated protocol.
pub const LOCK_MARKER: &str = ".protocol";
/// The only per-disk locking protocol this pooler understands. Bump it in both
/// this file and `reclaim-disks.sh` if the lock layout ever changes, so a
/// mismatched pair degrades to the boot gate instead of to corruption.
pub const LOCK_PROTOCOL: &str = "perdisk-1";

/// Poll interval while waiting on a contended disk lock. Contention is rare
/// (only a boot of the exact VM the script is working on) and the wait is one
/// disk long at worst, so polling tightly costs nothing measurable and keeps
/// the wait cancel-safe — a blocking `flock` would park a runtime thread that
/// no timeout could reclaim.
const LOCK_POLL: Duration = Duration::from_millis(50);

/// `<run dir>/.reclaim-locks`, or `None` when no run dir is configured. Set
/// once at startup by [`set_run_dir`] because the boot path is a free function
/// with no access to the config.
static LOCK_ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Whether the deployed script has been observed to honour per-disk locks.
/// False until proven otherwise: the fallback is slow, the alternative is a
/// destroyed filesystem.
static PER_DISK: AtomicBool = AtomicBool::new(false);

/// Publish the run dir for the boot path and prime the per-disk latch from any
/// marker a previous pooler's last pass left behind — otherwise every redeploy
/// would spend its first [`RECLAIM_FIRST_DELAY`] back on the global gate.
/// [`Reclaimer::run_once`] re-validates the latch on the next pass either way.
pub fn set_run_dir(run_dir: Option<PathBuf>) {
    let dir = run_dir.map(|d| d.join(LOCK_DIR));
    if let Some(d) = &dir {
        ensure_lock_dir(d);
        let per_disk = refresh_per_disk(&d.join(LOCK_MARKER));
        info!(
            "disk reclaim: per-disk boot locks in {} ({})",
            d.display(),
            if per_disk {
                "active — VM boots don't wait on passes"
            } else {
                "not yet negotiated; boots use the global gate until the first pass"
            }
        );
    }
    let _ = LOCK_ROOT.set(dir);
}

/// Create the lock dir 0777. Best effort: if it fails, `open_lock` fails too
/// and the boot falls back to the gate, which is slow but safe.
fn ensure_lock_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777));
}

/// Re-read the marker and latch what it says. Returns the new state.
fn refresh_per_disk(marker: &Path) -> bool {
    let ok = std::fs::read_to_string(marker)
        .map(|s| s.trim() == LOCK_PROTOCOL)
        .unwrap_or(false);
    PER_DISK.store(ok, Ordering::Release);
    ok
}

/// Is the pooler excluding boots per disk rather than with the global gate?
/// Dashboard copy only — the boot path reads the latch directly.
pub fn per_disk_locks() -> bool {
    PER_DISK.load(Ordering::Acquire)
}

/// This sandbox's lock file, or `None` when there is no run dir. The id is
/// pasted into a path, so it is checked: an id carrying `/` or `..` would
/// otherwise let a bad daemon response name a file outside the lock dir.
fn lock_path(sandbox_id: &str) -> Option<PathBuf> {
    if sandbox_id.is_empty() || sandbox_id.contains('/') || sandbox_id.contains("..") {
        return None;
    }
    LOCK_ROOT
        .get()?
        .as_ref()
        .map(|d| d.join(format!("{sandbox_id}.lock")))
}

/// Open a lock file for `flock`, creating it if this is the VM's first boot.
fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(d) = path.parent() {
                ensure_lock_dir(d);
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                // The file is a name to lock, never a place to keep anything;
                // truncating it would be pointless and, if the script has it
                // open, rude.
                .truncate(false)
                .mode(0o666)
                .open(path)
        }
        // Created by the root script under a stricter umask than it intended.
        // `flock(2)` works on any open descriptor whatever its access mode, so
        // a read-only handle still takes a fully exclusive lock.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            OpenOptions::new().read(true).open(path)
        }
        Err(e) => Err(e),
    }
}

/// `Ok(true)` when this descriptor now holds the lock, `Ok(false)` when the
/// reclaim script holds it. The lock is released when the file is dropped.
fn try_flock(f: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    // EAGAIN and EWOULDBLOCK are the same value on Linux; match both rather
    // than relying on that.
    match e.raw_os_error() {
        Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN => Ok(false),
        _ => Err(e),
    }
}

/// Boot↔reclaim mutual exclusion, fallback flavour. Readers are VM boots
/// (start of a stopped VM, power-cycle, dashboard start/reboot); the writer is
/// a reclaim run, held for the run's whole duration. Rationale: the script's
/// in-use scan is a snapshot taken at pass start, so only keeping boots and
/// passes disjoint in time makes "stopped disk" a stable fact for the length of
/// a pass. Freshly *created* disks don't need a permit at all — they didn't
/// exist when the pass enumerated.
///
/// In per-disk mode nothing takes the read side and this is uncontended; a pass
/// still takes the write side, so the fallback remains correct the moment the
/// latch flips back.
///
/// tokio's RwLock is fair: a waiting pass blocks later boots until it finishes,
/// and a waiting boot blocks later passes. Unbounded stalls are avoided not by
/// the lock but by [`BOOTS_WAITING`] — a boot that has to wait cancels the pass.
static BOOT_GATE: RwLock<()> = RwLock::const_new(());

/// How many VM boots are parked on reclaim work right now, in either mode.
///
/// This — not the [`preempt`] notification — is what a running pass consults,
/// and it is why a request raised while the pass was merely *queued* on the
/// gate can no longer be dropped. tokio's `RwLock` is fair, so `try_read` fails
/// from the instant a writer queues: a boot arriving in the window between
/// `write().await` being called and being granted parks behind that writer. The
/// old code treated the notification such a boot had already sent as stale and
/// drained it, and the pass then ran to completion — up to [`RECLAIM_TIMEOUT`]
/// — with a client-facing boot waiting on it, which is the exact failure the
/// yield exists to prevent. A count read *after* acquiring has no such window.
/// The notification is now only a wakeup; the count is the truth.
static BOOTS_WAITING: AtomicUsize = AtomicUsize::new(0);

/// Wakes a running pass so it can re-read [`BOOTS_WAITING`]. `notify_one` (not
/// `notify_waiters`) so a signal that lands in the instant between the pass
/// taking the gate and arming its watch is *stored* rather than lost; a stored
/// signal that outlives its boot is harmless, because the pass re-reads the
/// count before acting on it.
fn preempt() -> &'static tokio::sync::Notify {
    static PREEMPT: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    PREEMPT.get_or_init(tokio::sync::Notify::new)
}

/// Registers a parked boot for the length of its wait. Drop-based so a
/// cancelled bring-up (a timed-out spare restart, a dropped client) leaves the
/// count accurate instead of pinning every future pass into yielding.
struct WaitingGuard;

impl WaitingGuard {
    fn new() -> Self {
        BOOTS_WAITING.fetch_add(1, Ordering::SeqCst);
        preempt().notify_one();
        Self
    }
}

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        BOOTS_WAITING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Is any boot parked on reclaim work? Read by the running pass.
fn boots_waiting() -> bool {
    BOOTS_WAITING.load(Ordering::SeqCst) > 0
}

/// A boot's claim on a stopped VM's disk: an `flock` on that VM's lock file in
/// per-disk mode, the read side of [`BOOT_GATE`] in the fallback. Either is
/// released on drop.
pub enum BootPermit {
    /// Both fields exist purely for their `Drop`: closing the file releases the
    /// `flock`, dropping the guard releases the gate. Nothing ever reads them.
    Disk { _lock: std::fs::File },
    Gate { _gate: RwLockReadGuard<'static, ()> },
}

/// Take the boot side of the exclusion for `sandbox_id`. In per-disk mode this
/// resolves immediately unless a reclaim pass is working on *this VM's* disk;
/// in the fallback, unless a pass is running at all, in which case it is asked
/// to yield and this waits for it to finish the disk it is on.
///
/// Hold the guard across the daemon call that (re)opens the stopped VM's disk,
/// and no longer: once the VM process holds the disk open, the script's in-use
/// checks are what protect it — the pass-start snapshot when the boot happened
/// between passes, and the live re-check it runs under this same lock when the
/// boot happened during one.
///
/// Take this *before* a bring-up slot, never after: a boot parked here while
/// holding one of the (three) slots converts a reclaim pass into a fleet-wide
/// bring-up stall, which is the failure this preemption exists to prevent.
pub async fn boot_permit(sandbox_id: &str) -> BootPermit {
    match acquire(sandbox_id, None).await {
        Some(p) => p,
        // Only a bounded wait can come back empty; stay total rather than
        // panicking on a case the signature allows.
        None => BootPermit::Gate { _gate: gate_wait().await },
    }
}

/// [`boot_permit`] that gives up after `limit` instead of waiting indefinitely.
/// `None` means the permit was NOT taken and the caller must not boot — for
/// background work that would rather retry on its next pass than block it (see
/// `spares::restart_spare`).
pub async fn boot_permit_within(sandbox_id: &str, limit: Duration) -> Option<BootPermit> {
    acquire(sandbox_id, Some(limit)).await
}

async fn acquire(sandbox_id: &str, limit: Option<Duration>) -> Option<BootPermit> {
    if per_disk_locks()
        && let Some(path) = lock_path(sandbox_id)
    {
        match disk_permit(&path, sandbox_id, limit).await {
            Ok(Some(f)) => return Some(BootPermit::Disk { _lock: f }),
            Ok(None) => return None,
            // Never boot unprotected: an unusable lock file means we cannot
            // tell what the script is doing, so fall back to the gate, which
            // needs nothing from the filesystem.
            Err(e) => warn!(
                "disk reclaim: per-disk lock {} unusable ({e}) — falling back to the global \
                 boot gate for {sandbox_id}",
                path.display()
            ),
        }
    }
    match limit {
        None => Some(BootPermit::Gate {
            _gate: gate_wait().await,
        }),
        Some(limit) => {
            // Bound to a `let` rather than returned from the tail, so the
            // abandoned `gate_wait` is dropped at this statement — the point we
            // gave up — instead of at whatever temporary scope the caller
            // happens to end it in. What it carries is worth being deliberate
            // about: a `WaitingGuard` still raised would make every later pass
            // yield the instant it started, and a `read()` still queued on a
            // fair `RwLock` would make the next pass wait behind a boot that
            // has gone home.
            let attempt = tokio::time::timeout(limit, gate_wait()).await;
            attempt.ok().map(|g| BootPermit::Gate { _gate: g })
        }
    }
}

/// Lock just this VM's disk. `Ok(None)` only when `limit` expired.
async fn disk_permit(
    path: &Path,
    sandbox_id: &str,
    limit: Option<Duration>,
) -> std::io::Result<Option<std::fs::File>> {
    let f = open_lock(path)?;
    if try_flock(&f)? {
        return Ok(Some(f));
    }
    // The script is on exactly this disk. The disk it is on is the one we want,
    // so asking it to yield is the shortest path to the lock.
    let waited = Instant::now();
    let _waiting = WaitingGuard::new();
    loop {
        if let Some(limit) = limit
            && waited.elapsed() >= limit
        {
            return Ok(None);
        }
        tokio::time::sleep(LOCK_POLL).await;
        if try_flock(&f)? {
            info!(
                "VM boot waited {:?} for the reclaim pass to release {sandbox_id}'s disk",
                waited.elapsed()
            );
            return Ok(Some(f));
        }
    }
}

/// Fallback: the read side of the global gate, registering as a waiter (and so
/// preempting the pass) only if it isn't free.
async fn gate_wait() -> RwLockReadGuard<'static, ()> {
    if let Ok(guard) = BOOT_GATE.try_read() {
        return guard;
    }
    let waited = Instant::now();
    let _waiting = WaitingGuard::new();
    let guard = BOOT_GATE.read().await;
    info!(
        "VM boot waited {:?} for the disk-reclaim pass to yield",
        waited.elapsed()
    );
    guard
}

/// Name of the file `reclaim-disks.sh` checks between disks; creating it in the
/// run dir is how a waiting boot asks an in-flight pass to stop early.
pub const STOP_FILE: &str = ".reclaim-stop";

/// Is a reclaim pass running (or about to start)? Background work that would
/// boot a VM checks this and defers: even in per-disk mode such a job may
/// collide with the disk the pass is on and make it yield, trading a whole
/// pass's progress for work nobody is waiting on. Every pass takes the write
/// side of [`BOOT_GATE`] in both modes, and tokio's `RwLock` is fair, so a
/// queued writer also reads as "running" here — which is the answer we want.
pub fn pass_running() -> bool {
    BOOT_GATE.try_read().is_err()
}

/// Runs the configured reclaim command, at most one instance at a time.
pub struct Reclaimer {
    cmd: String,
    /// Where to write the script's stop file (`<run dir>/.reclaim-stop`).
    /// `None` when no run dir is configured — then a pass cannot be asked to
    /// yield and boots wait it out, which is slow but never unsafe.
    stop_file: Option<std::path::PathBuf>,
    /// The per-disk locking marker (`<run dir>/.reclaim-locks/.protocol`) this
    /// pass renegotiates. `None` with no run dir, where per-disk locks are
    /// impossible anyway.
    marker: Option<PathBuf>,
    running: AtomicBool,
}

impl Reclaimer {
    pub fn new(cmd: String, run_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            cmd,
            stop_file: run_dir.as_ref().map(|d| d.join(STOP_FILE)),
            marker: run_dir.map(|d| d.join(LOCK_DIR).join(LOCK_MARKER)),
            running: AtomicBool::new(false),
        }
    }

    /// Ask the running pass to stop after its current disk. Best effort: the
    /// script polls for this file between disks, and an older script that
    /// doesn't simply runs to completion.
    fn request_stop(&self) {
        match &self.stop_file {
            Some(path) => match std::fs::File::create(path) {
                Ok(_) => info!(
                    "disk reclaim: a VM needs to boot — asked the pass to yield at its \
                     next safe point ({})",
                    path.display()
                ),
                Err(e) => warn!(
                    "disk reclaim: cannot write the stop file {} ({e}) — the boot waits for \
                     the whole pass",
                    path.display()
                ),
            },
            None => warn!(
                "disk reclaim: a VM is waiting on this pass, but no run dir is configured \
                 (PG_VM_POOL_RUN_DIR) so it cannot be asked to yield — the boot waits for \
                 the whole pass"
            ),
        }
    }

    /// Remove the stop file, whatever became of it. Called before a pass (a
    /// leftover from a crashed run would make every future pass a no-op) and
    /// after one.
    fn clear_stop(&self) {
        if let Some(path) = &self.stop_file {
            let _ = std::fs::remove_file(path);
        }
    }

    /// One reclaim run: execute the command via `sh -c`, bounded by
    /// [`RECLAIM_TIMEOUT`], and log its outcome. Returns the number of disks
    /// the script reported trimming (its `trim  -<freed>  <disk>` lines), for
    /// the supervisor's heartbeat. Single-flighted: returns 0 immediately if a
    /// run is already in progress.
    pub async fn run_once(&self) -> usize {
        if self.running.swap(true, Ordering::SeqCst) {
            info!("disk reclaim: a run is already in progress; skipping");
            return 0;
        }
        let _guard = RunningGuard(&self.running);

        // Exclusive with VM boots for the whole run — see BOOT_GATE. In
        // per-disk mode nothing takes the read side, so this is uncontended;
        // it is still taken so the fallback is correct the instant the latch
        // flips back.
        let waited = Instant::now();
        let _exclusive = BOOT_GATE.write().await;
        if waited.elapsed() > Duration::from_secs(1) {
            info!(
                "disk reclaim: waited {:?} for in-flight VM boots",
                waited.elapsed()
            );
        }
        self.clear_stop();
        // Renegotiate per-disk mode from scratch. The marker is re-created by
        // the script itself when it supports locking, so clearing it here is
        // what makes a rollback to an older script drop us back to the gate
        // rather than trust a stale latch (see the module docs).
        if let Some(marker) = &self.marker {
            let _ = std::fs::remove_file(marker);
        }

        debug!("disk reclaim: running `{}`", self.cmd);
        let started = Instant::now();
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&self.cmd)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let run = tokio::time::timeout(RECLAIM_TIMEOUT, command.output());
        tokio::pin!(run);
        // A boot that arrived while this pass was still QUEUED on the gate is
        // already parked, and the notification it sent landed before there was
        // anyone to hear it. Read the count, which has no such window.
        let mut yielding = boots_waiting();
        if yielding {
            self.request_stop();
        }
        let result = loop {
            tokio::select! {
                res = &mut run => break res,
                // A VM needs to boot: ask the script to stop after the disk it
                // is on, then keep waiting for it to actually exit — the gate
                // must not be handed back while root-owned work is still
                // writing to a disk. Only the first request does anything;
                // later signals are left stored, and re-checking the count is
                // what keeps a stored signal that outlived its boot from
                // cutting a later pass short.
                _ = preempt().notified(), if !yielding => {
                    if boots_waiting() {
                        yielding = true;
                        self.request_stop();
                    }
                }
            }
        };
        self.clear_stop();
        let output = match result {
            Err(_) => {
                error!(
                    "disk reclaim: `{}` did not finish within {RECLAIM_TIMEOUT:?} — abandoned; \
                     note the command runs under sudo, so its root-owned children may still \
                     be working (VM boots stay blocked until they exit)",
                    self.cmd
                );
                return 0;
            }
            Ok(Err(e)) => {
                error!("disk reclaim: could not launch `{}`: {e}", self.cmd);
                return 0;
            }
            Ok(Ok(out)) => out,
        };
        // The child has exited, so the marker (if any) is this script's answer
        // about per-disk locking, not a leftover. Only re-latch on a run that
        // actually happened: a launch failure or a timeout says nothing about
        // what the script supports, and a timeout's root children may still be
        // holding disks, so the previous answer stands.
        if let Some(marker) = &self.marker {
            let was = per_disk_locks();
            let now = refresh_per_disk(marker);
            if now != was && now {
                info!(
                    "disk reclaim: the deployed script honours per-disk locks ({LOCK_PROTOCOL}) \
                     — VM boots no longer wait on passes"
                );
            } else if now != was {
                warn!(
                    "disk reclaim: the deployed script did not claim per-disk locking \
                     ({LOCK_PROTOCOL}); falling back to the global boot gate — every VM boot \
                     now waits for a pass to yield"
                );
            }
        }
        if yielding {
            info!(
                "disk reclaim: pass yielded to a VM boot after {:?}; it resumes from where it \
                 stopped on the next trigger",
                started.elapsed()
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stdout
            .lines()
            .filter(|l| l.starts_with("trim ") || l.starts_with("would-trim"))
            .count();
        // The script ends with a one-line summary ("trimmed N disk(s),
        // reclaimed X; ..."); surface that instead of the whole per-disk log.
        let summary = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty() && !l.starts_with("----"))
            .unwrap_or("(no output)");
        // Per-disk FAIL lines name the disks whose fsck failed — without this
        // the summary's "N failed" count is unactionable.
        for line in stdout.lines().filter(|l| l.starts_with("FAIL")) {
            warn!("disk reclaim: {line}");
        }
        if output.status.success() {
            info!("disk reclaim: {summary}");
            if !stderr.trim().is_empty() {
                warn!("disk reclaim stderr: {}", tail(&stderr, 500));
            }
        } else {
            error!(
                "disk reclaim: `{}` exited with {}: {} — stderr: {}",
                self.cmd,
                output.status,
                summary,
                tail(&stderr, 500),
            );
        }
        trimmed
    }

    /// Fire-and-forget run after `delay` — the idle reaper's post-stop trigger.
    /// Quietly does nothing if a run is already in progress (the running sweep
    /// or the next periodic one will pick the freshly stopped disks up).
    pub fn spawn_soon(self: &Arc<Self>, delay: Duration) {
        if self.running.load(Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            this.run_once().await;
        });
    }

    /// Kick off one run right now, in the background — the dashboard's
    /// "reclaim disk slack" control. Errors if a run is already in progress so
    /// the button gives feedback instead of silently queueing.
    pub fn spawn_now(self: &Arc<Self>) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            bail!("a disk reclaim run is already in progress");
        }
        let this = self.clone();
        tokio::spawn(async move {
            let n = this.run_once().await;
            info!("manual disk reclaim finished: trimmed {n} disk(s)");
        });
        Ok(())
    }
}

/// Clears the single-flight flag on drop, so an early return (timeout, launch
/// failure) or panic can't leave the reclaimer permanently "running".
struct RunningGuard<'a>(&'a AtomicBool);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Last `n` bytes of `s` (on a char boundary), for bounded error logs.
fn tail(s: &str, n: usize) -> &str {
    let s = s.trim();
    let mut start = s.len().saturating_sub(n);
    while start > 0 && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`BOOT_GATE`], [`BOOTS_WAITING`] and [`PER_DISK`] are process-wide, and
    /// cargo runs a test binary's tests concurrently in one process — so every
    /// test that touches them takes this first. It is the same lock the
    /// `loadtest` stub takes, because a bring-up there takes a boot permit
    /// here: two separate locks left this module's assertions reading another
    /// module's waiting boots.
    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        crate::vm::test_exclusive().await
    }

    /// Pin the fallback mode for a test about the gate. (`lock_path` already
    /// returns `None` while `LOCK_ROOT` is unset, which no test sets — this is
    /// belt and braces against ordering.)
    fn use_gate_mode() {
        PER_DISK.store(false, Ordering::Release);
    }

    /// A run dir for one test, cleaned up by the caller.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pgfc-reclaim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn run_once_counts_trimmed_disks() {
        let _serial = serial().await;
        let r = Reclaimer::new(
            "printf 'reclaim-disks: 3 disk(s)\ntrim  -1.0GB  /a\ntrim  -2.0GB  /b\n\
             skip  (in use)  /c\n----\ntrimmed 2 disk(s), reclaimed 3.0GB\n'"
                .to_string(),
            None,
        );
        assert_eq!(r.run_once().await, 2);
    }

    #[tokio::test]
    async fn run_once_single_flights() {
        let r = Reclaimer::new("echo 'trim  -1.0GB  /a'".to_string(), None);
        r.running.store(true, Ordering::SeqCst);
        // A concurrent caller must bail out immediately, not run the command.
        assert_eq!(r.run_once().await, 0);
        // And it must not have cleared the original holder's flag.
        assert!(r.running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_once_reports_failure_as_zero() {
        let _serial = serial().await;
        let r = Reclaimer::new("echo boom >&2; exit 3".to_string(), None);
        assert_eq!(r.run_once().await, 0);
        // Flag released for the next run.
        assert!(!r.running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn spawn_now_rejects_while_running() {
        let r = Arc::new(Reclaimer::new("true".to_string(), None));
        r.running.store(true, Ordering::SeqCst);
        assert!(r.spawn_now().is_err());
    }

    /// The stop file is a request, not state: it lands where the script looks
    /// for it, and a leftover from a crashed run is cleared (otherwise every
    /// future pass would stop before its first disk).
    #[test]
    fn the_stop_file_is_written_and_cleared_in_the_run_dir() {
        let dir = scratch("stop-file");
        let r = Reclaimer::new("true".to_string(), Some(dir.clone()));
        let stop = dir.join(STOP_FILE);

        r.request_stop();
        assert!(stop.exists(), "the script's stop file is created in the run dir");
        r.clear_stop();
        assert!(!stop.exists(), "and removed again");

        // A leftover from a crashed run is cleared too, not merely ignored.
        std::fs::write(&stop, b"").unwrap();
        r.clear_stop();
        assert!(!stop.exists());

        // No run dir: nothing to write, and no panic.
        Reclaimer::new("true".to_string(), None).request_stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole point of the yield: a boot that arrives during a long pass
    /// gets the gate back in about the time one disk takes, not one pass —
    /// and only after the script has actually exited.
    #[tokio::test]
    async fn a_waiting_boot_makes_the_pass_stop_early() {
        let _serial = serial().await;
        use_gate_mode();
        let dir = scratch("yield");
        let stop = dir.join(STOP_FILE);
        // Stands in for the script's between-disks check: run "forever", stop
        // when asked. Timeout-bounded so a broken yield fails the test rather
        // than hanging it.
        let r = Arc::new(Reclaimer::new(
            format!(
                "i=0; while [ ! -e {} ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done; \
                 echo 'trim  -1.0GB  /a'",
                stop.display()
            ),
            Some(dir.clone()),
        ));
        let run = tokio::spawn({
            let r = r.clone();
            async move { r.run_once().await }
        });
        // Let the run acquire the write side of the gate and arm its watch.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let waited = Instant::now();
        let permit = boot_permit("sb-yield").await;
        assert!(
            waited.elapsed() < Duration::from_secs(5),
            "boot waited {:?} for the pass — the stop request did not land",
            waited.elapsed()
        );
        // The pass finished its work and reported it; it was not killed.
        assert_eq!(run.await.unwrap(), 1);
        assert!(!stop.exists(), "the stop file is cleaned up after the pass");
        drop(permit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_yield_signal_does_not_cut_the_next_pass_short() {
        let _serial = serial().await;
        use_gate_mode();
        // Signal with nobody waiting: `notify_one` stores the permit.
        preempt().notify_one();
        // The next pass wakes on it, sees a waiting count of zero, and runs to
        // completion — the signal is a wakeup, not the decision.
        let r = Reclaimer::new("echo 'trim  -1.0GB  /a'".to_string(), None);
        assert_eq!(r.run_once().await, 1);
        assert!(!boots_waiting());
    }

    /// Regression. tokio's `RwLock` is fair, so `try_read` fails from the
    /// instant a pass *queues* for the write side — a boot arriving in the
    /// window between `write().await` being called and being granted therefore
    /// raised its stop request before the pass existed to hear it. The pass
    /// then drained that request as "stale" and ran to completion with a
    /// client-facing boot parked behind it, for as long as `RECLAIM_TIMEOUT`.
    #[tokio::test]
    async fn a_boot_that_arrives_while_the_pass_is_queued_still_makes_it_yield() {
        let _serial = serial().await;
        use_gate_mode();
        let dir = scratch("queued");
        let stop = dir.join(STOP_FILE);
        let r = Arc::new(Reclaimer::new(
            format!(
                "i=0; while [ ! -e {} ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done; \
                 echo 'trim  -1.0GB  /a'",
                stop.display()
            ),
            Some(dir.clone()),
        ));

        // An in-flight boot, so the pass has to queue for the gate.
        let in_flight = boot_permit("sb-inflight").await;
        let run = tokio::spawn({
            let r = r.clone();
            async move { r.run_once().await }
        });
        // Let run_once reach `BOOT_GATE.write().await` and park there.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The boot this test is about: it arrives while the pass is queued.
        let waiter = tokio::spawn(async { boot_permit("sb-waiter").await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(boots_waiting(), "the queued boot registered itself");

        // Hand the gate to the pass. It must read the count it could not have
        // heard about, and yield at once.
        drop(in_flight);
        let permit = tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("the boot waited out the whole pass — its request was dropped")
            .unwrap();
        assert_eq!(run.await.unwrap(), 1, "the pass yielded, it was not killed");
        drop(permit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cancelled boot (a timed-out spare restart) must not leave the count
    /// raised — every later pass would yield the instant it started and the
    /// fleet would never be trimmed — nor stay queued as a reader on the fair
    /// gate, where the next pass would wait behind a boot that gave up.
    #[tokio::test]
    async fn a_boot_that_gives_up_waiting_stops_preempting() {
        let _serial = serial().await;
        use_gate_mode();
        let held = BOOT_GATE.write().await;
        assert!(
            boot_permit_within("sb-impatient", Duration::from_millis(100))
                .await
                .is_none(),
            "a bounded wait against a held gate gives up"
        );
        assert!(!boots_waiting(), "and deregisters on the way out");
        drop(held);
    }

    /// The whole point of per-disk locks: a boot of a VM the pass is not
    /// touching does not wait, and one of the VM it *is* touching does.
    #[tokio::test]
    async fn per_disk_locks_only_hold_up_the_disk_being_worked_on() {
        // Contending on a lock registers in BOOTS_WAITING, which is global.
        let _serial = serial().await;
        let dir = scratch("perdisk");
        let a = dir.join("sb-a.lock");
        let b = dir.join("sb-b.lock");

        // Stand in for the script holding sb-a's lock across its pipeline.
        let script = open_lock(&a).unwrap();
        assert!(try_flock(&script).unwrap());

        // A boot of sb-b is untouched by that.
        let other = disk_permit(&b, "sb-b", Some(Duration::from_millis(100)))
            .await
            .unwrap();
        assert!(other.is_some(), "an unrelated VM's boot does not wait at all");

        // A boot of sb-a waits, and gets the disk as soon as the script lets go.
        let contended = disk_permit(&a, "sb-a", Some(Duration::from_millis(100)))
            .await
            .unwrap();
        assert!(contended.is_none(), "the disk under the pass is held");
        drop(script);
        let after = disk_permit(&a, "sb-a", Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert!(after.is_some(), "and is handed over once the pass releases it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A boot blocked on its own disk still asks the pass to yield — the disk
    /// the pass is on is exactly the one that boot wants.
    #[tokio::test]
    async fn a_contended_disk_lock_preempts_the_pass() {
        let _serial = serial().await;
        let dir = scratch("perdisk-preempt");
        let path = dir.join("sb-c.lock");
        let script = open_lock(&path).unwrap();
        assert!(try_flock(&script).unwrap());

        let boot = tokio::spawn({
            let path = path.clone();
            async move { disk_permit(&path, "sb-c", Some(Duration::from_secs(5))).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(boots_waiting(), "a colliding boot asks the pass to yield");

        drop(script);
        assert!(boot.await.unwrap().unwrap().is_some());
        assert!(!boots_waiting());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mode is negotiated, not assumed: only the protocol this pooler
    /// speaks counts, and anything else means "older script, use the gate".
    #[tokio::test]
    async fn the_marker_latches_only_a_protocol_this_pooler_speaks() {
        let _serial = serial().await;
        let dir = scratch("marker");
        let marker = dir.join(LOCK_MARKER);

        assert!(!refresh_per_disk(&marker), "no marker at all: fall back");
        std::fs::write(&marker, b"perdisk-99\n").unwrap();
        assert!(!refresh_per_disk(&marker), "a protocol we don't speak: fall back");
        std::fs::write(&marker, format!("{LOCK_PROTOCOL}\n")).unwrap();
        assert!(refresh_per_disk(&marker), "ours, trailing newline and all");

        // A rollback to a script that doesn't write the marker drops us back.
        std::fs::remove_file(&marker).unwrap();
        assert!(!refresh_per_disk(&marker));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sandbox id is pasted into a path, so it is checked.
    #[test]
    fn lock_path_rejects_ids_that_would_escape_the_lock_dir() {
        for bad in ["", "../../etc/shadow", "sb-a/../../x", "a/b"] {
            assert!(lock_path(bad).is_none(), "{bad:?} must not name a lock file");
        }
    }

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail("héllo", 3), "llo");
        assert_eq!(tail("héllo", 100), "héllo");
    }
}
