//! `serverctl exec` and `serverctl shell` — the two ways into a VM that are not
//! HTTP, and the only ways into a sandbox with no routes.
//!
//! Both go through app-lb's admin API rather than the heyvm daemon, so they work
//! from anywhere the admin API does, use the credentials already in the context,
//! and can wake a VM that has been scaled to zero or suspended. See the
//! `exec`/`shell` handlers in `src/admin.rs` for the wire protocol.

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::time::Duration;

use crate::Ctx;
use crate::output;
use crate::cmd::deployment_name;

/// Margin the HTTP timeout gets over the command's own, covering a cold start
/// and the round trip. Without it the client gives up on a request the server is
/// still honouring, and a slow-but-fine command looks like a network failure.
const EXEC_PATIENCE: Duration = Duration::from_secs(30);

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

    /// How long the guest may take before the command is killed.
    #[arg(long, value_name = "SECS", default_value = "60")]
    pub timeout: u64,

    /// Fail instead of starting a VM when the deployment has none running.
    #[arg(long)]
    pub no_wake: bool,
}

pub fn exec(ctx: &Ctx, args: &ExecArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    let command = args.command.join(" ");

    let mut body = Map::new();
    body.insert("command".into(), Value::String(command));
    body.insert("timeout_secs".into(), Value::from(args.timeout));
    body.insert("wake".into(), Value::Bool(!args.no_wake));
    if let Some(cwd) = &args.cwd {
        body.insert("cwd".into(), Value::String(cwd.clone()));
    }
    if !args.env.is_empty() {
        let mut env = Map::new();
        for pair in &args.env {
            let (k, v) = pair
                .split_once('=')
                .with_context(|| format!("--env expects KEY=VALUE, got {pair:?}"))?;
            env.insert(k.to_string(), Value::String(v.to_string()));
        }
        body.insert("env".into(), Value::Object(env));
    }

    let patience = Duration::from_secs(args.timeout) + EXEC_PATIENCE;
    let result = ctx.client.exec(&id, &Value::Object(body), patience)?;

    if ctx.out.is_machine() {
        return output::emit(&result, ctx.out, &[]);
    }

    // Human mode is a pass-through: stdout to stdout, stderr to stderr, and the
    // guest's exit code as our own. That is what makes `serverctl exec` usable
    // in a pipeline instead of only at a prompt.
    let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");
    print!("{stdout}");
    eprint!("{stderr}");
    let _ = std::io::stdout().flush();

    let code = result.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        // Not an anyhow error: that would print "Error: …" over a command whose
        // own diagnostics are already on stderr. Just carry the code out.
        std::process::exit(code as i32);
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

pub fn shell(ctx: &Ctx, args: &ShellArgs) -> Result<()> {
    let id = deployment_name(&args.resource)?;
    let (cols, rows) = terminal_size().unwrap_or((80, 24));

    let mut query = format!("cols={cols}&rows={rows}&wake={}", !args.no_wake);
    if let Some(cwd) = &args.cwd {
        query.push_str(&format!("&cwd={}", urlencode(cwd)));
    }
    let (url, auth) = ctx.client.shell_url(&id, &query);

    let mut request = tungstenite::http::Request::builder()
        .method("GET")
        .uri(&url)
        .header("Host", host_of(&url))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key());
    if let Some(auth) = &auth {
        request = request.header("Authorization", auth);
    }
    let request = request.body(()).context("building the shell request")?;

    let (socket, _) = match tungstenite::connect(request) {
        Ok(pair) => pair,
        // The handler refuses before the upgrade, so a real status code and a
        // JSON body come back here — surface those rather than "handshake
        // failed", which says nothing about *why*.
        Err(tungstenite::Error::Http(response)) => {
            let body = response
                .body()
                .as_ref()
                .and_then(|b| String::from_utf8(b.clone()).ok())
                .unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error")?.as_str().map(str::to_string))
                .unwrap_or_else(|| body.trim().to_string());
            let code = response.status().as_u16();
            if detail.is_empty() {
                bail!("the server returned HTTP {code}");
            }
            bail!("{detail} (HTTP {code})");
        }
        Err(e) => bail!("could not open a shell on {id:?}: {e}"),
    };

    pump(socket)
}

/// How long a socket read blocks before the loop goes to look at stdin. Sets the
/// worst-case keystroke latency, and is the whole cost of not having a runtime
/// to select over both.
const POLL: Duration = Duration::from_millis(20);

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// Copy the terminal to the socket and back until either ends.
///
/// stdin is read on its own thread because it is a blocking `read` that cannot
/// be selected over alongside the socket, and there is no async runtime here.
/// The socket gets a short read timeout instead, so a quiet guest still lets
/// keystrokes through.
fn pump(mut socket: Socket) -> Result<()> {
    use std::sync::mpsc;

    const STDIN: u8 = 0x01;
    const STDOUT: u8 = 0x02;

    // Raw mode, so keystrokes reach the guest's PTY unbuffered and un-echoed —
    // the guest is doing the line editing. Restored by the guard on every exit
    // path; leaving a terminal in raw mode makes the user's shell unusable.
    let _restore = RawMode::enter()?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin().lock();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    tcp_of(socket.get_ref()).set_read_timeout(Some(POLL))?;

    let mut exit_code = 0i32;
    let mut stdout = std::io::stdout().lock();
    loop {
        while let Ok(chunk) = rx.try_recv() {
            let mut frame = Vec::with_capacity(chunk.len() + 1);
            frame.push(STDIN);
            frame.extend_from_slice(&chunk);
            if socket.send(tungstenite::Message::Binary(frame)).is_err() {
                return Ok(());
            }
        }

        match socket.read() {
            Ok(tungstenite::Message::Binary(bytes)) => {
                if bytes.first() == Some(&STDOUT) {
                    stdout.write_all(&bytes[1..])?;
                    stdout.flush()?;
                }
            }
            Ok(tungstenite::Message::Text(text)) => {
                let v: Value = serde_json::from_str(&text).unwrap_or(json!({}));
                match v.get("type").and_then(Value::as_str) {
                    Some("exit") => {
                        exit_code = v.get("code").and_then(Value::as_i64).unwrap_or(0) as i32;
                        break;
                    }
                    Some("error") => {
                        let msg = v.get("message").and_then(Value::as_str).unwrap_or("unknown");
                        // Raw mode swallows a bare "\n", hence the carriage
                        // return: without it this lands mid-line and stair-steps.
                        write!(stdout, "\r\nshell error: {msg}\r\n")?;
                        stdout.flush()?;
                    }
                    _ => {} // "ready", or something a later app-lb added
                }
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {}
            // The read timeout expiring, which is the normal idle case: the
            // guest had nothing to say in the last `POLL`. Both spellings,
            // because which one a timed-out read reports is platform-dependent.
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }

    let _ = socket.close(None);
    drop(_restore);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// The TCP socket under a possibly-TLS stream, so its read timeout can be set.
fn tcp_of(
    stream: &tungstenite::stream::MaybeTlsStream<std::net::TcpStream>,
) -> &std::net::TcpStream {
    use tungstenite::stream::MaybeTlsStream;
    match stream {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::NativeTls(s) => s.get_ref(),
        // `MaybeTlsStream` is non_exhaustive and gains variants with features
        // this crate does not enable. Unreachable in this build.
        other => unreachable!("unexpected stream kind: {other:?}"),
    }
}

/// Puts the terminal in raw mode and restores it on drop.
struct RawMode {
    original: Option<libc::termios>,
}

impl RawMode {
    fn enter() -> Result<Self> {
        // Not a terminal (a pipe, a CI job): there is nothing to put in raw mode
        // and nothing to restore, and forcing it would fail. The session still
        // works — it just has no line discipline to change.
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
        if let Some(original) = self.original.take() {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
        }
    }
}

/// The terminal's size, or `None` when stdout is not one.
fn terminal_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row.max(1)))
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("localhost")
        .to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_taken_from_the_url_authority() {
        assert_eq!(host_of("ws://127.0.0.1:9090/deployments/x/shell"), "127.0.0.1:9090");
        assert_eq!(host_of("wss://admin.example.com/deployments/x/shell"), "admin.example.com");
        assert_eq!(host_of("nonsense"), "localhost");
    }

    /// A cwd with a space or a quote must not end the query string early.
    #[test]
    fn cwd_is_escaped_but_stays_readable() {
        assert_eq!(urlencode("/workspace/my project"), "/workspace/my%20project");
        assert_eq!(urlencode("/a&b=c"), "/a%26b%3Dc");
        assert_eq!(urlencode("/workspace"), "/workspace");
    }
}
