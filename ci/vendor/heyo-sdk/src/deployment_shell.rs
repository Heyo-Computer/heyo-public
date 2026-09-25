//! An interactive shell in one of a managed deployment's VMs, over a WebSocket
//! through cloud's namespace door:
//! `GET /namespaces/{ns}/lb/deployments/{id}/shell`.
//!
//! This is app-lb's shell, not the daemon's: app-lb picks (or wakes) the VM,
//! holds it for the life of the session so the autoscaler cannot reap it, and
//! speaks a simpler protocol than [`crate::ShellSession`] — no sequence
//! numbers, no acks, no reconnect. A session that drops is over; open another.
//!
//! Wire protocol (app-lb's `admin.rs::shell`):
//!
//! - server → client, first: JSON `{"type":"ready","sandbox_id":…}`.
//! - stdin: binary `[0x01, ...bytes]`.
//! - stdout: binary `[0x02, ...bytes]` (the PTY merges stderr in).
//! - resize: JSON `{"type":"resize","cols":N,"rows":N}`.
//! - exit: JSON `{"type":"exit","code":N}`; error: `{"type":"error","message":…}`.
//!
//! The bearer goes in the upgrade's `Authorization` header, as every other
//! request from this client does. A shell needs `admin` scope on the
//! deployment's namespace.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::Stream;
use futures_util::{SinkExt, StreamExt};
use http::Request;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::client::HeyoClient;
use crate::commands::encode_path;
use crate::errors::HeyoError;

const FRAME_STDIN: u8 = 0x01;
const FRAME_STDOUT: u8 = 0x02;

/// How long to wait for app-lb's `ready` after the upgrade. The wait for a
/// VM happens *before* the upgrade (app-lb refuses with a status code rather
/// than a silent close), so once the socket is up the shell is moments away.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Options for [`crate::Deployments::shell`].
#[derive(Debug, Clone)]
pub struct DeploymentShellOptions {
    /// Initial PTY width. Default 80.
    pub cols: u16,
    /// Initial PTY height. Default 24.
    pub rows: u16,
    /// Working directory the shell starts in.
    pub cwd: Option<String>,
    /// Boot or resume a VM when the deployment has none running (default
    /// true). `false` fails fast instead — the upgrade is refused with a 409.
    pub wake: bool,
    /// Open the shell in this VM of the deployment rather than whichever the
    /// pool offers. A VM not in the deployment refuses the upgrade (404); one
    /// the daemon has not started yet, or that is draining, is a 409. Nothing
    /// is woken when a VM is named.
    pub sandbox_id: Option<String>,
}

impl Default for DeploymentShellOptions {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            cwd: None,
            wake: true,
            sandbox_id: None,
        }
    }
}

impl DeploymentShellOptions {
    /// The query app-lb reads. Only `true`/`false` parse for `wake`.
    pub(crate) fn query(&self) -> Vec<(String, String)> {
        let mut q = vec![
            ("cols".to_string(), self.cols.to_string()),
            ("rows".to_string(), self.rows.to_string()),
            ("wake".to_string(), if self.wake { "true" } else { "false" }.to_string()),
        ];
        if let Some(cwd) = &self.cwd {
            q.push(("cwd".to_string(), cwd.clone()));
        }
        if let Some(id) = &self.sandbox_id {
            q.push(("sandbox_id".to_string(), id.clone()));
        }
        q
    }
}

/// Lifecycle events on a [`DeploymentShell`]'s event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentShellEvent {
    /// A non-fatal error from app-lb; the session continues.
    Error(String),
    /// The session ended: the guest's exit code, or `None` when the socket
    /// dropped without one.
    Closed { exit_code: Option<i32> },
}

/// A live shell in a deployment's VM. Created by
/// [`crate::Deployments::shell`].
pub struct DeploymentShell {
    sandbox_id: String,
    inner: Arc<Inner>,
    output_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    events_rx: Arc<Mutex<mpsc::Receiver<DeploymentShellEvent>>>,
}

struct Inner {
    write_tx: mpsc::UnboundedSender<Outbound>,
    closed_rx: watch::Receiver<bool>,
    exit_code: Mutex<Option<i32>>,
}

enum Outbound {
    Stdin(Vec<u8>),
    Text(String),
    Close,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerControl {
    Ready {
        sandbox_id: String,
    },
    Exit {
        code: i32,
    },
    Error {
        #[serde(default)]
        message: Option<String>,
    },
}

/// The path cloud serves the shell on, plus the query app-lb reads.
pub(crate) fn shell_path(namespace: &str, id: &str, options: &DeploymentShellOptions) -> String {
    let query: Vec<String> = options
        .query()
        .into_iter()
        .map(|(k, v)| format!("{k}={}", encode_path(&v)))
        .collect();
    format!(
        "/namespaces/{}/lb/deployments/{}/shell?{}",
        encode_path(namespace),
        encode_path(id),
        query.join("&")
    )
}

fn host_from_url(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(after_scheme)
        .to_string()
}

impl DeploymentShell {
    /// Connect and wait for app-lb's `ready` frame.
    pub(crate) async fn open(
        client: &HeyoClient,
        namespace: &str,
        id: &str,
        options: DeploymentShellOptions,
    ) -> Result<Self, HeyoError> {
        let url = client.ws_url(&shell_path(namespace, id, &options))?;
        let auth = client.ws_authorization();
        let request = Request::builder()
            .method("GET")
            .uri(&url)
            .header("Authorization", &auth)
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Host", host_from_url(&url))
            .body(())
            .map_err(|e| HeyoError::Connection(format!("build ws request: {e}")))?;

        // The upgrade is where app-lb says no: a 404 for a deployment or VM
        // that is not there, 409 for one that cannot be used, 503 when no VM
        // came up in time. tungstenite reports those as a handshake error
        // carrying the status, which is the message a caller gets.
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| HeyoError::Connection(format!("shell upgrade refused: {e}")))?;
        let (mut ws_tx, mut ws_rx) = ws.split();

        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(256);
        let (events_tx, events_rx) = mpsc::channel::<DeploymentShellEvent>(16);
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Outbound>();
        let (closed_tx, closed_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<String, HeyoError>>();
        let inner = Arc::new(Inner {
            write_tx,
            closed_rx,
            exit_code: Mutex::new(None),
        });

        let task_inner = inner.clone();
        tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);
            let mut last_error: Option<String> = None;
            let mut exit_code: Option<i32> = None;
            loop {
                tokio::select! {
                    Some(out) = write_rx.recv() => {
                        let msg = match out {
                            Outbound::Stdin(bytes) => {
                                let mut frame = Vec::with_capacity(bytes.len() + 1);
                                frame.push(FRAME_STDIN);
                                frame.extend_from_slice(&bytes);
                                Message::Binary(frame)
                            }
                            Outbound::Text(text) => Message::Text(text),
                            Outbound::Close => Message::Close(None),
                        };
                        if ws_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    frame = ws_rx.next() => {
                        match frame {
                            Some(Ok(Message::Binary(bytes))) => {
                                if bytes.first() == Some(&FRAME_STDOUT)
                                    && output_tx.send(bytes[1..].to_vec()).await.is_err()
                                {
                                    break;
                                }
                            }
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<ServerControl>(&text) {
                                    Ok(ServerControl::Ready { sandbox_id }) => {
                                        if let Some(tx) = ready_tx.take() {
                                            let _ = tx.send(Ok(sandbox_id));
                                        }
                                    }
                                    Ok(ServerControl::Exit { code }) => {
                                        exit_code = Some(code);
                                        break;
                                    }
                                    Ok(ServerControl::Error { message }) => {
                                        let message = message.unwrap_or_else(|| "unknown error".into());
                                        last_error = Some(message.clone());
                                        let _ = events_tx.send(DeploymentShellEvent::Error(message)).await;
                                    }
                                    Err(_) => {}
                                }
                            }
                            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                            Some(Ok(_)) => {} // ping/pong: tungstenite answers these itself
                        }
                    }
                }
            }
            if let Some(tx) = ready_tx.take() {
                let why = last_error
                    .unwrap_or_else(|| "socket closed before the shell opened".into());
                let _ = tx.send(Err(HeyoError::Connection(why)));
            }
            *task_inner.exit_code.lock().await = exit_code;
            let _ = events_tx.send(DeploymentShellEvent::Closed { exit_code }).await;
            let _ = closed_tx.send(true);
            let _ = ws_tx.close().await;
        });

        let sandbox_id = match tokio::time::timeout(READY_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(id))) => id,
            Ok(Ok(Err(e))) => return Err(e),
            Ok(Err(_)) => {
                return Err(HeyoError::Connection("shell task ended before ready".into()))
            }
            Err(_) => return Err(HeyoError::Connection("timed out waiting for the shell".into())),
        };
        Ok(Self {
            sandbox_id,
            inner,
            output_rx: Arc::new(Mutex::new(output_rx)),
            events_rx: Arc::new(Mutex::new(events_rx)),
        })
    }

    /// The VM the shell landed in.
    pub fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    /// Send keystrokes / bytes to the shell's stdin.
    pub fn write(&self, bytes: &[u8]) -> Result<(), HeyoError> {
        self.send(Outbound::Stdin(bytes.to_vec()))
    }

    /// Tell the PTY the terminal changed size.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), HeyoError> {
        self.send(Outbound::Text(
            json!({ "type": "resize", "cols": cols, "rows": rows }).to_string(),
        ))
    }

    fn send(&self, out: Outbound) -> Result<(), HeyoError> {
        if self.is_closed() {
            return Err(HeyoError::Connection("shell is closed".into()));
        }
        self.inner
            .write_tx
            .send(out)
            .map_err(|_| HeyoError::Connection("shell writer is closed".into()))
    }

    /// Close the socket; app-lb ends the PTY and releases the VM. Waits up to
    /// two seconds for the session task to wind down.
    pub async fn close(&self) {
        if self.is_closed() {
            return;
        }
        let _ = self.inner.write_tx.send(Outbound::Close);
        let mut rx = self.inner.closed_rx.clone();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }

    /// The guest's exit code once the shell has ended; `None` while it runs
    /// or when the socket dropped without one.
    pub async fn exit_code(&self) -> Option<i32> {
        *self.inner.exit_code.lock().await
    }

    pub fn is_closed(&self) -> bool {
        *self.inner.closed_rx.borrow()
    }

    /// PTY output; ends when the session closes.
    pub fn output(&self) -> impl Stream<Item = Vec<u8>> + Send + Unpin {
        let rx = self.output_rx.clone();
        Box::pin(async_stream::stream! {
            loop {
                let mut guard = rx.lock().await;
                match guard.recv().await {
                    Some(chunk) => yield chunk,
                    None => break,
                }
            }
        })
    }

    /// Lifecycle events; ends when the session closes.
    pub fn events(&self) -> impl Stream<Item = DeploymentShellEvent> + Send + Unpin {
        let rx = self.events_rx.clone();
        Box::pin(async_stream::stream! {
            loop {
                let mut guard = rx.lock().await;
                match guard.recv().await {
                    Some(event) => yield event,
                    None => break,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path is cloud's door for the namespace, and the query is app-lb's:
    /// the terminal size, `wake` as the literal app-lb parses, the working
    /// directory and the VM only when asked for.
    #[test]
    fn the_shell_path_carries_the_terminal_and_the_named_vm() {
        let opts = DeploymentShellOptions {
            cols: 120,
            rows: 40,
            cwd: Some("/srv app".into()),
            wake: false,
            sandbox_id: Some("sb-1".into()),
        };
        assert_eq!(
            shell_path("team-a", "web", &opts),
            "/namespaces/team-a/lb/deployments/web/shell?cols=120&rows=40&wake=false&cwd=%2Fsrv%20app&sandbox_id=sb-1"
        );
        assert_eq!(
            shell_path("team-a", "web", &DeploymentShellOptions::default()),
            "/namespaces/team-a/lb/deployments/web/shell?cols=80&rows=24&wake=true"
        );
    }

    #[test]
    fn app_lb_control_frames_parse() {
        assert!(matches!(
            serde_json::from_str::<ServerControl>(r#"{"type":"ready","sandbox_id":"sb-1"}"#).unwrap(),
            ServerControl::Ready { sandbox_id } if sandbox_id == "sb-1"
        ));
        assert!(matches!(
            serde_json::from_str::<ServerControl>(r#"{"type":"exit","code":3}"#).unwrap(),
            ServerControl::Exit { code: 3 }
        ));
        assert!(matches!(
            serde_json::from_str::<ServerControl>(r#"{"type":"error","message":"no"}"#).unwrap(),
            ServerControl::Error { message: Some(m) } if m == "no"
        ));
        assert_eq!(host_from_url("wss://server.heyo.computer/namespaces/a/lb?x=1"), "server.heyo.computer");
    }
}
