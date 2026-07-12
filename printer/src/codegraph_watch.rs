//! Auto-spawn a `codegraph watch` daemon for the duration of a `run` or
//! `exec` so the on-disk index stays fresh while the agent works.
//!
//! Best-effort: if the `codegraph` binary isn't installed, we log a one-line
//! note and keep going — no agent run should fail because of the index
//! daemon. A supervisor task owns the child: it `wait()`s on the process so
//! it doesn't zombify, restarts it with backoff if it dies early, and on
//! `Guard` drop sends SIGKILL and reaps before returning.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const RESTART_INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const RESTART_MAX_BACKOFF: Duration = Duration::from_secs(30);
const RESTART_BUDGET: u32 = 5;

pub struct Guard {
    pid: Option<u32>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        // The supervisor task will SIGKILL the child and reap it before
        // returning. Abort as a fallback if the runtime is shutting down.
        if let Some(t) = self.task.take() {
            t.abort();
        }
        if let Some(pid) = self.pid {
            eprintln!("[printer] stopping codegraph watch daemon (pid {pid})");
        }
    }
}

/// Try to spawn `codegraph watch <cwd>`. Returns:
/// - `Ok(Some(Guard))` on success.
/// - `Ok(None)` if the binary isn't installed or the log file can't be opened.
/// - `Err(_)` only if we found the binary but the spawn itself failed.
pub fn try_spawn(cwd: &Path) -> Result<Option<Guard>> {
    let Some(bin) = locate_binary() else {
        eprintln!(
            "[printer] codegraph not found on PATH or in ~/.printer/plugins/codegraph/bin; \
             skipping watch daemon. Install with `make install-codegraph` to enable."
        );
        return Ok(None);
    };

    let log_path = cwd.join(".printer").join("codegraph-watch.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Truncate the log on first spawn of this exec; the supervisor appends
    // on every restart so all output from this session lives in one file.
    if let Err(e) = std::fs::File::create(&log_path) {
        eprintln!(
            "[printer] could not open {} for codegraph watch log: {e}; skipping daemon",
            log_path.display()
        );
        return Ok(None);
    }

    let child = spawn_one(&bin, cwd, &log_path)?;
    let pid = child.id();
    eprintln!(
        "[printer] launched codegraph watch ({}); logs → {}",
        pid.map(|p| format!("pid {p}"))
            .unwrap_or_else(|| "no pid".into()),
        log_path.display()
    );

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(supervise(
        child,
        bin.clone(),
        cwd.to_path_buf(),
        log_path,
        stop_rx,
    ));

    Ok(Some(Guard {
        pid,
        stop_tx: Some(stop_tx),
        task: Some(task),
    }))
}

/// Owns the child process. `wait()`s on it so the kernel reaps it instead of
/// leaving a zombie, restarts it with exponential backoff if it exits early,
/// and on stop signal SIGKILLs and reaps before returning.
async fn supervise(
    mut child: Child,
    bin: PathBuf,
    cwd: PathBuf,
    log_path: PathBuf,
    stop_rx: oneshot::Receiver<()>,
) {
    let mut stop_rx = stop_rx;
    let mut backoff = RESTART_INITIAL_BACKOFF;
    let mut restarts: u32 = 0;
    let mut window_start = Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = &mut stop_rx => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return;
            }
            status = child.wait() => {
                let pid_str = child
                    .id()
                    .map(|p| format!("pid {p}"))
                    .unwrap_or_else(|| "<no pid>".into());
                match status {
                    Ok(s) => eprintln!("[printer] codegraph watch ({pid_str}) exited: {s}"),
                    Err(e) => eprintln!("[printer] codegraph watch ({pid_str}) wait error: {e}"),
                }

                // Reset the budget if it has been quiet for a while — a long
                // healthy run earns fresh attempts.
                if window_start.elapsed() > Duration::from_secs(120) {
                    restarts = 0;
                    backoff = RESTART_INITIAL_BACKOFF;
                    window_start = Instant::now();
                }
                if restarts >= RESTART_BUDGET {
                    eprintln!(
                        "[printer] codegraph watch died {RESTART_BUDGET} times; giving up. \
                         See {} for details.",
                        log_path.display()
                    );
                    return;
                }
                restarts += 1;

                let sleep = tokio::time::sleep(backoff);
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut stop_rx => return,
                    _ = &mut sleep => {}
                }
                backoff = (backoff * 2).min(RESTART_MAX_BACKOFF);

                match spawn_one(&bin, &cwd, &log_path) {
                    Ok(c) => {
                        eprintln!(
                            "[printer] restarted codegraph watch ({})",
                            c.id().map(|p| format!("pid {p}")).unwrap_or_else(|| "no pid".into())
                        );
                        child = c;
                    }
                    Err(e) => {
                        eprintln!("[printer] could not restart codegraph watch: {e}");
                        return;
                    }
                }
            }
        }
    }
}

fn spawn_one(bin: &Path, cwd: &Path, log_path: &Path) -> Result<Child> {
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_path)
        .map_err(|e| anyhow::anyhow!("opening {} for append: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| anyhow::anyhow!("cloning log handle: {e}"))?;

    let mut cmd = Command::new(bin);
    cmd.arg("watch")
        .arg(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn `{} watch`: {e}", bin.display()))
}

/// Resolve the `codegraph` binary: PATH first, then the bundled plugin bin
/// dir under `~/.printer`. Shared by the watch daemon and the Claude MCP
/// wiring (`agent::claude_argv`), which needs an absolute command that
/// resolves identically on the host and inside a sandbox VM.
pub fn locate_binary() -> Option<PathBuf> {
    if let Ok(p) = which("codegraph") {
        return Some(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home)
            .join(".printer")
            .join("plugins")
            .join("codegraph")
            .join("bin")
            .join("codegraph");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Minimal `which` so we don't pull in an extra crate for one lookup.
fn which(name: &str) -> Result<PathBuf> {
    let path_var = std::env::var_os("PATH").ok_or_else(|| anyhow::anyhow!("PATH unset"))?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("`{name}` not found on PATH")
}
