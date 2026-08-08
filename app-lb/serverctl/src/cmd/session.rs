//! `serverctl exec` and `serverctl shell` — the two ways into a VM that are not
//! HTTP, and the only ways into a sandbox with no routes.
//!
//! Both go through app-lb's admin API rather than the heyvm daemon, so they work
//! from anywhere the admin API does, use the credentials already in the context,
//! and can wake a VM that has been scaled to zero or suspended.
//!
//! The protocol lives in [`crate::shell`]; everything here is terminal handling
//! — raw mode, window size, and turning a guest's exit code into this process's.

use anyhow::{Context, Result, bail};
use clap::Args;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::cmd::{Ctx, deployment_name};
use crate::output;
use crate::{ExecRequest, ShellEvent, ShellExit, ShellOptions};

#[derive(Args, Debug)]
pub struct ExecArgs {
    /// The deployment whose VM runs the command, e.g. `sb-7f3a9c`.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// The command, run through `sh -c` in the guest. Everything after `--`.
    #[arg(value_name = "COMMAND", trailing_var_arg = true, required = true)]
    pub command: Vec<String>,

    /// Working directory in the guest.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<String>,

    /// An environment variable for this command only: `KEY=VALUE`. Repeatable.
    #[arg(long = "env", short = 'e', value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// How long app-lb waits on the guest before giving up.
    ///
    /// This does not *kill* anything: app-lb abandons its call to the daemon and
    /// answers 502, and the command carries on running in the guest.
    #[arg(long, value_name = "SECS", default_value = "60")]
    pub timeout: u64,

    /// Fail instead of starting a VM when the deployment has none running.
    #[arg(long)]
    pub no_wake: bool,
}

pub fn exec(ctx: &Ctx, args: &ExecArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;

    let mut req = ExecRequest::new(args.command.join(" ")).timeout_secs(args.timeout);
    if let Some(cwd) = &args.cwd {
        req = req.cwd(cwd);
    }
    if args.no_wake {
        req = req.no_wake();
    }
    for pair in &args.env {
        let (k, v) = pair
            .split_once('=')
            .with_context(|| format!("--env expects KEY=VALUE, got {pair:?}"))?;
        req = req.env(k, v);
    }

    let result = ctx.client.exec(&id, &req)?;

    if ctx.out.is_machine() {
        let rendered = serde_json::json!({
            "sandbox_id": result.sandbox_id,
            "exit_code": result.exit_code,
            "stdout": result.stdout,
            "stderr": result.stderr,
            "output": result.output,
        });
        return output::emit(&rendered, ctx.out, &[]);
    }

    // Human mode is a pass-through: stdout to stdout, stderr to stderr, and the
    // guest's exit code as our own. That is what makes `serverctl exec` usable
    // in a pipeline instead of only at a prompt.
    print!("{}", result.stdout);
    eprint!("{}", result.stderr);
    let _ = std::io::stdout().flush();

    if !result.ok() {
        // Not an anyhow error: that would print "Error: …" over a command whose
        // own diagnostics are already on stderr. Just carry the code out.
        std::process::exit(result.exit_code);
    }
    Ok(())
}

#[derive(Args, Debug)]
pub struct ShellArgs {
    /// The deployment to open a shell on.
    #[arg(value_name = "RESOURCE")]
    pub resource: String,

    /// Working directory the shell starts in.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<String>,

    /// Fail instead of starting a VM when the deployment has none running.
    #[arg(long)]
    pub no_wake: bool,
}

/// How long to wait for guest output before looking at stdin and the window
/// size. Short enough that a keystroke feels immediate.
const TICK: Duration = Duration::from_millis(20);

pub fn shell(ctx: &Ctx, args: &ShellArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    let (cols, rows) = terminal_size().unwrap_or((80, 24));

    let mut opts = ShellOptions::default().size(cols, rows);
    if let Some(cwd) = &args.cwd {
        opts = opts.cwd(cwd);
    }
    if args.no_wake {
        opts = opts.no_wake();
    }

    let mut session = ctx.client.shell(&id, &opts)?;
    eprintln!("connected to {} ({id})", session.sandbox_id());

    // Raw mode *after* connecting, so a pre-upgrade failure prints normally
    // rather than into a terminal with no line discipline.
    let _raw = RawMode::enter()?;
    watch_winch();

    // stdin on its own thread: reading it blocks, and the terminal has to stay
    // responsive to guest output while waiting for a keystroke.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut last_size = (cols, rows);
    let mut stdout = std::io::stdout();

    loop {
        // Guest output first — it is what the person is reading.
        match session.next_timeout(TICK) {
            Some(ShellEvent::Output(bytes)) => {
                let _ = stdout.write_all(&bytes);
                let _ = stdout.flush();
            }
            Some(ShellEvent::Error(msg)) => {
                let _ = write!(stdout, "\r\nshell error: {msg}\r\n");
                let _ = stdout.flush();
            }
            None if session.exit().is_some() => break,
            None => {}
        }

        // The gap app-lb always supported and no client ever used: without this,
        // resizing the terminal leaves the guest PTY at its original geometry
        // and full-screen programs draw into the wrong box.
        //
        // Compared against the last size rather than sent on every signal, so
        // dragging a window edge does not become a burst of frames.
        if RESIZED.swap(false, Ordering::Relaxed)
            && let Some(size) = terminal_size()
            && size != last_size
        {
            last_size = size;
            let _ = session.resize(size.0, size.1);
        }

        for chunk in rx.try_iter() {
            if session.write(&chunk).is_err() {
                break;
            }
        }
    }

    let exit = session
        .exit()
        .cloned()
        .unwrap_or(ShellExit { code: 0, error: None });
    let _ = session.close();
    drop(_raw);

    // An error before the exit means the session *died* rather than ended.
    // app-lb reports an unknown exit code as 0, so without this a sandbox that
    // was killed underneath a live shell looks like a clean logout.
    if let Some(err) = &exit.error {
        eprintln!("shell ended abnormally: {err}");
        if exit.code == 0 {
            std::process::exit(1);
        }
    }
    if exit.code != 0 {
        std::process::exit(exit.code);
    }
    Ok(())
}

/// Restores the terminal on every exit path.
struct RawMode {
    original: Option<libc::termios>,
}

impl RawMode {
    fn enter() -> Result<Self> {
        // Not a terminal (a pipe, a CI job): nothing to put in raw mode and
        // nothing to restore, and forcing it would fail. The session still works
        // — it just has no line discipline to change.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return Ok(Self { original: None });
        }
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } != 0 {
            bail!("could not read the terminal settings");
        }
        let original = term;
        unsafe { libc::cfmakeraw(&mut term) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) } != 0 {
            bail!("could not put the terminal in raw mode");
        }
        Ok(Self {
            original: Some(original),
        })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(original) = self.original {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
        }
    }
}

/// Set by `SIGWINCH`. A signal handler may only touch an atomic, so the `ioctl`
/// that reads the new size happens back in the loop.
static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

fn watch_winch() {
    let handler: extern "C" fn(libc::c_int) = on_winch;
    unsafe { libc::signal(libc::SIGWINCH, handler as usize as libc::sighandler_t) };
    // Start set, so the first pass reconciles the current size in case the
    // window changed between measuring it and connecting.
    RESIZED.store(true, Ordering::Relaxed);
}

/// The current terminal size, if stdout is one.
fn terminal_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resize_flag_latches_and_is_consumed_once() {
        RESIZED.store(false, Ordering::Relaxed);
        on_winch(libc::SIGWINCH);
        assert!(RESIZED.swap(false, Ordering::Relaxed), "the signal sets it");
        assert!(
            !RESIZED.swap(false, Ordering::Relaxed),
            "a burst of signals during a drag is one resize, not many"
        );
    }

    /// The whole point of raw mode is that it is undone. A guard that reported
    /// success without recording the original settings could not restore them.
    #[test]
    fn raw_mode_is_a_no_op_when_stdin_is_not_a_terminal() {
        // Under `cargo test` stdin is not a tty, so this is the path taken.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            let guard = RawMode::enter().unwrap();
            assert!(guard.original.is_none(), "nothing to restore, nothing stored");
        }
    }
}
