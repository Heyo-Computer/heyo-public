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
//! closes that window itself: [`boot_permit`] is the read side of a gate whose
//! write side is held for the full duration of every reclaim run. Boots wait
//! for an in-flight pass instead of corrupting a disk. Runs are additionally
//! single-flighted so the periodic timer, the post-reap trigger, and the
//! dashboard button can't stack overlapping sweeps.
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
//! to stop: [`boot_permit`] signals, [`Reclaimer::run_once`] creates the
//! script's **stop file**, and the script returns after the disk it is on
//! (seconds). The gate is released only once the child has actually exited.
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

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Boot↔reclaim mutual exclusion. Readers are VM boots (start of a stopped VM,
/// power-cycle, dashboard start/reboot); the writer is a reclaim run, held for
/// the run's whole duration. Rationale: the script's in-use scan is a snapshot
/// taken at pass start, so only keeping boots and passes disjoint in time makes
/// "stopped disk" a stable fact for the length of a pass. Freshly *created*
/// disks don't need the permit — they didn't exist when the pass enumerated.
///
/// tokio's RwLock is fair: a waiting pass blocks later boots until it finishes,
/// and a waiting boot blocks later passes. Unbounded stalls are avoided not by
/// the lock but by [`PREEMPT`] — a boot that has to wait cancels the pass.
static BOOT_GATE: RwLock<()> = RwLock::const_new(());

/// Raised by a boot that found the gate held, watched by the running pass.
/// `notify_one` (not `notify_waiters`) so a signal that lands in the instant
/// between the pass taking the gate and arming its watch is *stored* rather
/// than lost — the pass drains any stale permit at its start, so the only
/// effect of that storage is at worst one extra early exit.
fn preempt() -> &'static tokio::sync::Notify {
    static PREEMPT: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    PREEMPT.get_or_init(tokio::sync::Notify::new)
}

/// Take the boot side of the gate: resolves immediately unless a reclaim pass
/// is running, in which case the pass is asked to yield and this waits for it
/// to finish the disk it is on. Hold the guard across the daemon call that
/// (re)opens a stopped VM's disk — once the VM process holds the disk open,
/// the next pass's in-use scan protects it.
///
/// Take this *before* a bring-up slot, never after: a boot parked here while
/// holding one of the (three) slots converts a reclaim pass into a fleet-wide
/// bring-up stall, which is the failure this preemption exists to prevent.
pub async fn boot_permit() -> RwLockReadGuard<'static, ()> {
    if let Ok(guard) = BOOT_GATE.try_read() {
        return guard;
    }
    let waited = Instant::now();
    preempt().notify_one();
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

/// Is a reclaim pass holding (or about to hold) the boot gate? Background work
/// that would boot a VM checks this and defers: taking a boot permit now would
/// make the pass yield, trading a whole pass's progress for a job that has
/// nobody waiting on it. tokio's `RwLock` is fair, so a queued writer also
/// reads as "running" here — which is the answer we want.
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
    running: AtomicBool,
}

impl Reclaimer {
    pub fn new(cmd: String, run_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            cmd,
            stop_file: run_dir.map(|d| d.join(STOP_FILE)),
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
                    "disk reclaim: a VM needs to boot — asked the pass to stop after the \
                     current disk ({})",
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

        // Exclusive with VM boots for the whole run — see BOOT_GATE.
        let waited = Instant::now();
        let _exclusive = BOOT_GATE.write().await;
        if waited.elapsed() > Duration::from_secs(1) {
            info!(
                "disk reclaim: waited {:?} for in-flight VM boots",
                waited.elapsed()
            );
        }
        // Drop a stop request raised before this run owned the gate — it was
        // aimed at the previous pass and would end this one immediately.
        let _ = futures::FutureExt::now_or_never(preempt().notified());
        self.clear_stop();

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
        let mut yielding = false;
        let result = loop {
            tokio::select! {
                res = &mut run => break res,
                // A VM needs to boot: ask the script to stop after the disk it
                // is on, then keep waiting for it to actually exit — the gate
                // must not be handed back while root-owned work is still
                // writing to a disk. Only the first request does anything;
                // later signals are left stored for the next pass to drain.
                _ = preempt().notified(), if !yielding => {
                    yielding = true;
                    self.request_stop();
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

    /// A run dir for one test, cleaned up by the caller.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pgfc-reclaim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn run_once_counts_trimmed_disks() {
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
        let permit = boot_permit().await;
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
        // Signal with nobody running: `notify_one` stores the permit.
        preempt().notify_one();
        // The next pass must drain it and still run to completion.
        let r = Reclaimer::new("echo 'trim  -1.0GB  /a'".to_string(), None);
        assert_eq!(r.run_once().await, 1);
    }

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail("héllo", 3), "llo");
        assert_eq!(tail("héllo", 100), "héllo");
    }
}
