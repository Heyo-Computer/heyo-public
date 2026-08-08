//! An interactive PTY in a sandbox, over a WebSocket.
//!
//! # The protocol
//!
//! app-lb terminates the daemon's own shell protocol and re-presents a much
//! smaller one — no sequence numbers, no acks, no `init` frame, no session id.
//! Six message shapes in total:
//!
//! ```text
//! client → server  binary  [0x01, ...stdin]
//! client → server  text    {"type":"resize","cols":N,"rows":N}
//! server → client  text    {"type":"ready","sandbox_id":"…"}   (once, first)
//! server → client  binary  [0x02, ...stdout]                   (PTY merges stderr)
//! server → client  text    {"type":"exit","code":N}
//! server → client  text    {"type":"error","message":"…"}      (non-terminal)
//! ```
//!
//! [`Shell`] owns that encoding so no caller ever meets `0x01`. **That prefix is
//! mandatory** — app-lb silently drops a binary frame that starts with anything
//! else, with no error and no diagnosis, which is the single easiest way to
//! write a shell client that connects perfectly and types nothing.
//!
//! # Three things that are not smoothed over
//!
//! **`exit` code 0 is ambiguous.** It means both "the shell exited cleanly" and
//! "the VM died": when the daemon connection is exhausted app-lb forwards an
//! `error` frame and then closes with `exit: 0`. So [`Shell::exit`] returns a
//! [`ShellExit`] carrying any error that preceded it, and
//! [`ShellExit::is_clean`] is false when one did. Treating `code == 0` as
//! success on its own will report a crashed sandbox as a normal logout.
//!
//! **There is no resume.** The daemon's protocol supports it; app-lb's does not
//! — the client holds no session id and app-lb offers no path to reattach. A
//! dropped socket means the session is gone, and reconnecting gives a *new*
//! shell. This crate will not silently retry, because a retry that quietly
//! discards a session is worse than an error.
//!
//! **Nothing pings but us.** Neither app-lb nor the client is required to, so an
//! idle socket is free for any intermediary to reap. [`Shell`] sends a
//! WebSocket ping every [`PING_INTERVAL`]; app-lb's stack answers automatically.

use crate::api::{Client, WsConfig};
use crate::error::{Error, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Channel byte on stdin, going out.
const STDIN: u8 = 0x01;
/// Channel byte on stdout, coming in.
const STDOUT: u8 = 0x02;

/// How often to ping. app-lb never originates one, and its own heartbeat with
/// the daemon is invisible here, so this is the only thing keeping an idle
/// session alive through a proxy with an idle timeout.
pub const PING_INTERVAL: Duration = Duration::from_secs(25);

/// How to open a shell.
#[derive(Debug, Clone)]
pub struct ShellOptions {
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    /// Boot or resume a VM if none is running. On by default.
    pub wake: bool,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            cwd: None,
            wake: true,
        }
    }
}

impl ShellOptions {
    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Ask for [`Error::NoRunningVm`] rather than waiting on a cold start.
    pub fn no_wake(mut self) -> Self {
        self.wake = false;
        self
    }

    fn query(&self) -> String {
        let mut q = format!(
            "cols={}&rows={}&wake={}",
            self.cols,
            self.rows,
            // Only `true`/`false` parse server-side.
            if self.wake { "true" } else { "false" }
        );
        if let Some(cwd) = &self.cwd {
            q.push_str("&cwd=");
            q.push_str(&urlencode(cwd));
        }
        q
    }
}

/// Something that happened in a session.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellEvent {
    /// Bytes the PTY produced. stdout and stderr are already merged — a PTY has
    /// one stream, so there is nothing to separate.
    Output(Vec<u8>),
    /// A non-fatal error from app-lb. The session continues; the message is
    /// latched and reported again by [`Shell::exit`].
    Error(String),
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellExit {
    /// The guest's exit code. **`0` alone does not mean success** — app-lb
    /// reports an *unknown* exit code as `0`, which is what a VM dying under a
    /// live session looks like.
    pub code: i32,
    /// The last error before the session ended, if any.
    pub error: Option<String>,
}

impl ShellExit {
    /// Exited zero *and* nothing went wrong on the way.
    pub fn is_clean(&self) -> bool {
        self.code == 0 && self.error.is_none()
    }
}

/// What one incoming frame meant. Split out from the socket so the codec is
/// testable without one.
#[derive(Debug, Clone, PartialEq)]
enum Incoming {
    Ready(String),
    Output(Vec<u8>),
    Error(String),
    Exit(i32),
    /// A frame with no meaning to us — a keepalive, or something a later app-lb
    /// added. Ignored rather than treated as an error, so a newer server does
    /// not break an older client.
    Ignored,
    Closed,
}

/// Frame stdin. The `0x01` is not optional: without it app-lb drops the frame
/// and says nothing.
fn frame_stdin(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(STDIN);
    out.extend_from_slice(bytes);
    out
}

fn parse_incoming(msg: &Message) -> Incoming {
    match msg {
        Message::Binary(b) => match b.split_first() {
            Some((&STDOUT, rest)) => Incoming::Output(rest.to_vec()),
            // Not ours. app-lb only ever sends 0x02, so this is either a newer
            // channel or a corrupted frame; either way, not output.
            _ => Incoming::Ignored,
        },
        Message::Text(t) => {
            let Ok(v) = serde_json::from_str::<Value>(t) else {
                return Incoming::Ignored;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("ready") => Incoming::Ready(
                    v.get("sandbox_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                Some("exit") => Incoming::Exit(
                    v.get("code").and_then(Value::as_i64).unwrap_or(0) as i32,
                ),
                Some("error") => Incoming::Error(
                    v.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the server reported an error with no message")
                        .to_string(),
                ),
                _ => Incoming::Ignored,
            }
        }
        Message::Close(_) => Incoming::Closed,
        _ => Incoming::Ignored,
    }
}

/// Percent-encode a query value, leaving `/` alone so a path stays readable in a
/// log.
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

/// Turn the client's base URL into the WebSocket one.
fn ws_url(base: &str, id: &str, query: &str) -> String {
    let scheme_swapped = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{base}")
    };
    format!("{scheme_swapped}/deployments/{}/shell?{query}", urlencode(id))
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// A live session.
///
/// Dropping one closes the socket, which releases the VM slot app-lb was holding
/// open for it — so a forgotten `Shell` does not pin a deployment at its maximum
/// replica count.
pub struct Shell {
    socket: Socket,
    sandbox_id: String,
    exit: Option<ShellExit>,
    /// The last error seen, so an ambiguous `exit: 0` can be reported honestly.
    last_error: Option<String>,
    ping_at: std::time::Instant,
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell")
            .field("sandbox_id", &self.sandbox_id)
            .field("exit", &self.exit)
            .finish()
    }
}

impl Client {
    /// Attach an interactive shell.
    ///
    /// Everything that can fail with a status does so *before* the upgrade, so a
    /// socket that opens is a shell that attached: a `404`, `403`, `409` or
    /// `503` arrives as an [`Error`], never as an unexplained close.
    pub async fn shell(&self, id: &str, opts: &ShellOptions) -> Result<Shell> {
        let cfg = self.ws().ok_or_else(|| {
            Error::Invalid(
                "this client was built on a custom transport, which has no socket to \
                 open a shell on"
                    .into(),
            )
        })?;
        Shell::connect(cfg, id, opts).await
    }
}

impl Shell {
    async fn connect(cfg: &WsConfig, id: &str, opts: &ShellOptions) -> Result<Self> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let url = ws_url(&cfg.base, id, &opts.query());

        // Certificate verification cannot currently be skipped for a shell: the
        // WebSocket client builds its own TLS connector rather than reusing the
        // HTTP one. Said plainly rather than silently ignored, because silently
        // *verifying* when the caller asked not to would fail confusingly, and
        // silently not verifying would be worse.
        if cfg.insecure && url.starts_with("wss://") {
            return Err(Error::Invalid(
                "insecure TLS is not supported for shell sessions — reach the admin \
                 listener over an SSH tunnel, or terminate TLS with a trusted certificate"
                    .into(),
            ));
        }

        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|e| Error::Shell(format!("{url} is not a usable WebSocket URL: {e}")))?;

        // A header, not `?app_token=`. Rust can always set one, and a credential
        // in a URL lands in access logs; the query parameter exists for browsers,
        // which have no other option.
        if let Some(h) = cfg.auth.header() {
            req.headers_mut().insert(
                "authorization",
                h.parse()
                    .map_err(|_| Error::Invalid("the credential is not a valid header".into()))?,
            );
        }

        // A pre-upgrade rejection carries a real status and body — app-lb checks
        // auth, scope, the deployment and VM availability all *before*
        // upgrading. Surfacing those as the same typed errors the HTTP routes
        // produce means callers need one error vocabulary, not two.
        let (mut socket, _) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(|e| match &e {
                tokio_tungstenite::tungstenite::Error::Http(resp) => {
                    let body = resp
                        .body()
                        .as_ref()
                        .map(|b| String::from_utf8_lossy(b).to_string())
                        .unwrap_or_default();
                    Error::from_response(
                        resp.status().as_u16(),
                        &body,
                        "deployment",
                        id,
                        cfg.auth.credential(),
                    )
                }
                _ => Error::Shell(e.to_string()),
            })?;

        // The first frame is always `ready`; read it here so `sandbox_id` is
        // known before the caller sees a single byte of output.
        let sandbox_id = loop {
            match socket.next().await {
                Some(Ok(msg)) => match parse_incoming(&msg) {
                    Incoming::Ready(id) => break id,
                    Incoming::Error(m) => return Err(Error::Shell(m)),
                    Incoming::Closed | Incoming::Exit(_) => {
                        return Err(Error::Shell(
                            "the shell closed before it was ready".into(),
                        ));
                    }
                    _ => continue,
                },
                Some(Err(e)) => return Err(Error::Shell(e.to_string())),
                None => {
                    return Err(Error::Shell(
                        "the shell closed before it was ready".into(),
                    ));
                }
            }
        };

        Ok(Self {
            socket,
            sandbox_id,
            exit: None,
            last_error: None,
            ping_at: std::time::Instant::now(),
        })
    }

    /// Which sandbox this attached to. After a resume or a rebuild it is a
    /// different one than last time.
    pub fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    /// Send stdin.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.socket
            .send(Message::Binary(frame_stdin(bytes)))
            .await
            .map_err(|e| Error::Shell(e.to_string()))
    }

    /// Tell the guest its terminal changed size.
    ///
    /// Worth wiring to `SIGWINCH`: app-lb has always supported this and no
    /// client used it, so resizing a terminal mid-session left the guest PTY at
    /// whatever geometry it started with.
    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.socket
            .send(Message::Text(
                json!({ "type": "resize", "cols": cols, "rows": rows }).to_string(),
            ))
            .await
            .map_err(|e| Error::Shell(e.to_string()))
    }

    /// The next thing to happen, or `None` once the session has ended.
    ///
    /// Also drives the keepalive, so a caller that stops polling stops pinging —
    /// which is the correct coupling: a session nobody is reading does not need
    /// keeping alive.
    pub async fn next(&mut self) -> Option<ShellEvent> {
        loop {
            if self.exit.is_some() {
                return None;
            }

            if self.ping_at.elapsed() >= PING_INTERVAL {
                self.ping_at = std::time::Instant::now();
                if self.socket.send(Message::Ping(Vec::new())).await.is_err() {
                    self.finish(0);
                    return None;
                }
            }

            let msg = match tokio::time::timeout(PING_INTERVAL, self.socket.next()).await {
                // Idle. Loop round to send a ping.
                Err(_) => continue,
                Ok(None) => {
                    // The socket ended without an `exit` frame.
                    self.finish(0);
                    return None;
                }
                Ok(Some(Err(e))) => {
                    self.last_error = Some(e.to_string());
                    self.finish(0);
                    return None;
                }
                Ok(Some(Ok(m))) => m,
            };

            match parse_incoming(&msg) {
                Incoming::Output(b) => return Some(ShellEvent::Output(b)),
                Incoming::Error(m) => {
                    // Non-terminal, but latched: this is what makes a later
                    // `exit: 0` legible as a failure.
                    self.last_error = Some(m.clone());
                    return Some(ShellEvent::Error(m));
                }
                Incoming::Exit(code) => {
                    self.finish(code);
                    return None;
                }
                Incoming::Closed => {
                    self.finish(0);
                    return None;
                }
                Incoming::Ready(_) | Incoming::Ignored => continue,
            }
        }
    }

    fn finish(&mut self, code: i32) {
        self.exit = Some(ShellExit {
            code,
            error: self.last_error.take(),
        });
    }

    /// How the session ended, or `None` while it is still running.
    pub fn exit(&self) -> Option<&ShellExit> {
        self.exit.as_ref()
    }

    /// Close the session.
    pub async fn close(&mut self) -> Result<()> {
        let _ = self.socket.close(None).await;
        if self.exit.is_none() {
            self.finish(0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_is_always_prefixed() {
        // The whole reason this function exists. An unprefixed frame is dropped
        // by app-lb without an error, so a client that forgets connects fine and
        // then types into the void.
        assert_eq!(frame_stdin(b"ls\n"), vec![0x01, b'l', b's', b'\n']);
        assert_eq!(frame_stdin(b""), vec![0x01], "even an empty write is framed");
        assert_eq!(frame_stdin(&[0x02])[0], 0x01, "the payload is not the channel");
    }

    #[test]
    fn output_is_unwrapped_and_other_channels_are_not() {
        assert_eq!(
            parse_incoming(&Message::Binary(vec![0x02, b'h', b'i'])),
            Incoming::Output(b"hi".to_vec())
        );
        assert_eq!(
            parse_incoming(&Message::Binary(vec![0x02])),
            Incoming::Output(vec![]),
            "an empty payload is still output"
        );
        // Our own stdin channel echoed back is not output.
        assert_eq!(parse_incoming(&Message::Binary(vec![0x01, b'x'])), Incoming::Ignored);
        assert_eq!(parse_incoming(&Message::Binary(vec![])), Incoming::Ignored);
    }

    #[test]
    fn control_frames_are_understood() {
        let t = |s: &str| parse_incoming(&Message::Text(s.to_string()));
        assert_eq!(
            t(r#"{"type":"ready","sandbox_id":"sb-1"}"#),
            Incoming::Ready("sb-1".into())
        );
        assert_eq!(t(r#"{"type":"exit","code":3}"#), Incoming::Exit(3));
        assert_eq!(t(r#"{"type":"exit","code":0}"#), Incoming::Exit(0));
        assert_eq!(
            t(r#"{"type":"error","message":"boom"}"#),
            Incoming::Error("boom".into())
        );
    }

    /// A newer app-lb adding a frame type must not break an older client.
    #[test]
    fn unknown_frames_are_ignored_rather_than_fatal() {
        let t = |s: &str| parse_incoming(&Message::Text(s.to_string()));
        assert_eq!(t(r#"{"type":"something-new","x":1}"#), Incoming::Ignored);
        assert_eq!(t("not json at all"), Incoming::Ignored);
        assert_eq!(t("{}"), Incoming::Ignored);
        assert_eq!(parse_incoming(&Message::Pong(vec![])), Incoming::Ignored);
    }

    /// Malformed control frames get a defined meaning rather than a panic.
    #[test]
    fn missing_fields_degrade_predictably() {
        let t = |s: &str| parse_incoming(&Message::Text(s.to_string()));
        assert_eq!(t(r#"{"type":"exit"}"#), Incoming::Exit(0));
        assert_eq!(t(r#"{"type":"ready"}"#), Incoming::Ready(String::new()));
        match t(r#"{"type":"error"}"#) {
            Incoming::Error(m) => assert!(!m.is_empty(), "an error must always say something"),
            other => panic!("{other:?}"),
        }
    }

    /// The ambiguity that would otherwise report a dead VM as a clean logout.
    #[test]
    fn an_exit_after_an_error_is_not_clean() {
        let died = ShellExit {
            code: 0,
            error: Some("gave up after 5 reconnect attempts".into()),
        };
        assert!(!died.is_clean(), "code 0 after an error is a crash, not a logout");

        let logged_out = ShellExit { code: 0, error: None };
        assert!(logged_out.is_clean());

        let failed = ShellExit { code: 1, error: None };
        assert!(!failed.is_clean());
    }

    #[test]
    fn the_url_swaps_scheme_and_carries_the_options() {
        let o = ShellOptions::default().size(120, 40);
        assert_eq!(
            ws_url("http://127.0.0.1:9090", "sb-1", &o.query()),
            "ws://127.0.0.1:9090/deployments/sb-1/shell?cols=120&rows=40&wake=true"
        );
        assert!(ws_url("https://lb.example.com", "sb-1", "").starts_with("wss://"));

        // Only `true`/`false` parse server-side.
        assert!(ShellOptions::default().no_wake().query().contains("wake=false"));

        let with_cwd = ShellOptions::default().cwd("/work space");
        assert!(with_cwd.query().contains("cwd=/work%20space"), "{}", with_cwd.query());
    }

    #[test]
    fn a_deployment_id_is_escaped_into_the_url() {
        assert!(
            ws_url("http://x:1", "a?b=c", "q=1").starts_with("ws://x:1/deployments/a%3Fb%3Dc/shell?"),
            "{}",
            ws_url("http://x:1", "a?b=c", "q=1")
        );
    }
}
