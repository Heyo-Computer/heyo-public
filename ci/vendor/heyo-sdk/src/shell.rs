//! Persistent shell session over a WebSocket. Mirrors the protocol documented
//! in `sdk-ts/src/shell.ts` and implemented by
//! `mvm-ctrl/src/api.rs::shell_stream_ws`.
//!
//! Wire protocol:
//!
//! - Client → server, first frame: JSON
//!   `{type:"init", cols, rows, env?, cwd?, sessionId?}`
//! - Server → client: JSON `{type:"ready", sessionId, lastSeq?}`
//! - stdin: binary `[0x01, ...bytes]`
//! - stdout: binary `[0x02, seq:u64-be, ...bytes]` (PTY merges stdout/stderr)
//! - resize: JSON `{type:"resize", cols, rows}`
//! - ack: JSON `{type:"ack", seq}`
//! - close: JSON `{type:"close"}`
//! - exit: JSON `{type:"exit", code}`
//! - error: JSON `{type:"error", code?, message}`

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::Stream;
use futures_util::{SinkExt, StreamExt};
use http::Request;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;

use crate::client::HeyoClient;
use crate::commands::encode_path;
use crate::errors::HeyoError;

const FRAME_STDIN: u8 = 0x01;
const FRAME_STDOUT: u8 = 0x02;
const ACK_INTERVAL: Duration = Duration::from_millis(100);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);

/// Reconnect tuning.
#[derive(Debug, Clone)]
pub struct ShellReconnectOptions {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}
impl Default for ShellReconnectOptions {
    fn default() -> Self {
        Self {
            max_retries: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Options for [`crate::Sandbox::shell`].
#[derive(Debug, Clone)]
pub struct ShellOptions {
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub cols: u16,
    pub rows: u16,
    /// `None` disables auto-reconnect; `Some(_)` uses the supplied tuning.
    pub reconnect: Option<ShellReconnectOptions>,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
            reconnect: Some(ShellReconnectOptions::default()),
        }
    }
}

/// Lifecycle events emitted on the session's event stream.
#[derive(Debug, Clone)]
pub enum ShellEvent {
    Reconnecting { attempt: u32, delay: Duration },
    Reconnected,
    Closed { exit_code: Option<i32> },
    Error(String),
}

/// A live shell session. Use `output()` for stdout chunks, `events()` for
/// lifecycle events, and `write()` / `resize()` / `close()` to drive it.
pub struct ShellSession {
    inner: Arc<SessionInner>,
    output_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    events_rx: Arc<Mutex<mpsc::Receiver<ShellEvent>>>,
}

struct SessionInner {
    write_tx: mpsc::UnboundedSender<OutboundMessage>,
    session_id: Mutex<Option<String>>,
    closed_tx: watch::Sender<bool>,
    closed_rx: watch::Receiver<bool>,
    exit_code: Mutex<Option<i32>>,
}

enum OutboundMessage {
    Stdin(Vec<u8>),
    Json(serde_json::Value),
    Close,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerControl {
    Ready {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(default, rename = "lastSeq")]
        #[allow(dead_code)]
        last_seq: Option<u64>,
    },
    Exit {
        code: i32,
    },
    Error {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
}

impl ShellSession {
    /// Open and wait for the first `ready` frame.
    pub(crate) async fn open(
        client: HeyoClient,
        sandbox_id: String,
        options: ShellOptions,
    ) -> Result<Self, HeyoError> {
        let path = format!("/deployed-sandboxes/{}/shell-stream", encode_path(&sandbox_id));
        let url = client.ws_url(&path)?;
        let auth = client.ws_authorization();
        let uds = client.uds_socket_path();
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(256);
        let (events_tx, events_rx) = mpsc::channel::<ShellEvent>(64);
        let (write_tx, write_rx) = mpsc::unbounded_channel::<OutboundMessage>();
        let (closed_tx, closed_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<String, HeyoError>>();
        let inner = Arc::new(SessionInner {
            write_tx,
            session_id: Mutex::new(None),
            closed_tx,
            closed_rx,
            exit_code: Mutex::new(None),
        });

        let reconnect = options.reconnect.clone();
        let init_opts = options;
        let url_clone = url.clone();
        let auth_clone = auth.clone();
        let inner_for_task = inner.clone();
        let output_tx_clone = output_tx.clone();
        let events_tx_clone = events_tx.clone();
        tokio::spawn(run_session(
            url_clone,
            auth_clone,
            uds,
            init_opts,
            reconnect,
            inner_for_task,
            write_rx,
            output_tx_clone,
            events_tx_clone,
            Some(ready_tx),
        ));
        drop(output_tx);
        drop(events_tx);

        match ready_rx.await {
            Ok(Ok(_session_id)) => Ok(ShellSession {
                inner,
                output_rx: Arc::new(Mutex::new(output_rx)),
                events_rx: Arc::new(Mutex::new(events_rx)),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HeyoError::Connection("shell session task dropped before ready".into())),
        }
    }

    /// Latest server-assigned session id (set after `open()` resolves; may
    /// change across reconnects only if the server reissues one — usually it
    /// does not).
    pub async fn session_id(&self) -> Option<String> {
        self.inner.session_id.lock().await.clone()
    }

    pub async fn write(&self, bytes: &[u8]) -> Result<(), HeyoError> {
        if *self.inner.closed_rx.borrow() {
            return Err(HeyoError::Connection("session is closed".into()));
        }
        self.inner
            .write_tx
            .send(OutboundMessage::Stdin(bytes.to_vec()))
            .map_err(|_| HeyoError::Connection("session writer is closed".into()))
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), HeyoError> {
        if *self.inner.closed_rx.borrow() {
            return Err(HeyoError::Connection("session is closed".into()));
        }
        self.inner
            .write_tx
            .send(OutboundMessage::Json(json!({
                "type": "resize",
                "cols": cols,
                "rows": rows,
            })))
            .map_err(|_| HeyoError::Connection("session writer is closed".into()))
    }

    /// Send the close frame, then tear down the socket once the server
    /// acknowledges (or after 2s).
    pub async fn close(&self) -> Result<(), HeyoError> {
        if *self.inner.closed_rx.borrow() {
            return Ok(());
        }
        let _ = self.inner.write_tx.send(OutboundMessage::Close);
        // Wait until either the closed flag flips, or we time out.
        let mut rx = self.inner.closed_rx.clone();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        Ok(())
    }

    pub async fn exit_code(&self) -> Option<i32> {
        *self.inner.exit_code.lock().await
    }

    pub fn is_closed(&self) -> bool {
        *self.inner.closed_rx.borrow()
    }

    /// Stream of stdout chunks. Yields until the session closes.
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

    /// Stream of lifecycle events. Yields until the session closes.
    pub fn events(&self) -> impl Stream<Item = ShellEvent> + Send + Unpin {
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

#[allow(clippy::too_many_arguments)]
async fn run_session(
    url: String,
    auth: String,
    uds: Option<std::path::PathBuf>,
    options: ShellOptions,
    reconnect: Option<ShellReconnectOptions>,
    inner: Arc<SessionInner>,
    mut write_rx: mpsc::UnboundedReceiver<OutboundMessage>,
    output_tx: mpsc::Sender<Vec<u8>>,
    events_tx: mpsc::Sender<ShellEvent>,
    mut ready_tx: Option<oneshot::Sender<Result<String, HeyoError>>>,
) {
    let mut attempt: u32 = 0;
    let mut last_seq_received: u64 = 0;
    let mut last_seq_acked: u64 = 0;
    let cols = options.cols;
    let rows = options.rows;

    'outer: loop {
        let session_id_opt = inner.session_id.lock().await.clone();
        let reconnecting = session_id_opt.is_some();

        // Build the WS upgrade request with bearer auth.
        let request = match Request::builder()
            .method("GET")
            .uri(&url)
            .header("Authorization", &auth)
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", tokio_tungstenite::tungstenite::handshake::client::generate_key())
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Host", host_from_url(&url))
            .body(())
        {
            Ok(r) => r,
            Err(e) => {
                let err = HeyoError::Connection(format!("build ws request: {}", e));
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(err));
                    return;
                }
                let _ = events_tx.send(ShellEvent::Error(format!("{}", e))).await;
                break;
            }
        };

        // Dial: unix socket when the client uses the UDS transport (tungstenite
        // handshakes over any AsyncRead+AsyncWrite), TCP/TLS otherwise. The two
        // paths yield different stream types, so both are boxed into the same
        // sink/stream halves and the session loop below stays transport-blind.
        let connect_result: Result<(WsSink, WsSource), WsError> = match uds.as_deref() {
            #[cfg(unix)]
            Some(socket_path) => match tokio::net::UnixStream::connect(socket_path).await {
                Ok(stream) => tokio_tungstenite::client_async(request, stream)
                    .await
                    .map(|(ws, _)| split_ws(ws)),
                Err(e) => Err(WsError::Io(e)),
            },
            #[cfg(not(unix))]
            Some(_) => unreachable!("UDS transport is only constructed on unix"),
            None => tokio_tungstenite::connect_async(request)
                .await
                .map(|(ws, _)| split_ws(ws)),
        };
        let (mut ws_tx, mut ws_rx) = match connect_result {
            Ok(halves) => halves,
            Err(e) => {
                if reconnecting {
                    if let Some(r) = reconnect.as_ref() {
                        attempt += 1;
                        if attempt > r.max_retries {
                            let err = HeyoError::Connection(format!(
                                "gave up after {} reconnect attempts: {}",
                                r.max_retries, e
                            ));
                            let _ = events_tx.send(ShellEvent::Error(err.to_string())).await;
                            break;
                        }
                        let delay = backoff(r, attempt);
                        let _ = events_tx
                            .send(ShellEvent::Reconnecting { attempt, delay })
                            .await;
                        sleep(delay).await;
                        continue;
                    }
                }
                let err = HeyoError::Connection(format!("ws connect: {}", e));
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(err));
                    return;
                }
                let _ = events_tx.send(ShellEvent::Error(format!("{}", e))).await;
                break;
            }
        };

        // Send init frame.
        let mut init = serde_json::Map::new();
        init.insert("type".into(), json!("init"));
        init.insert("cols".into(), json!(cols));
        init.insert("rows".into(), json!(rows));
        if let Some(env) = &options.env {
            init.insert("env".into(), json!(env));
        }
        if let Some(cwd) = &options.cwd {
            init.insert("cwd".into(), json!(cwd));
        }
        if let Some(sid) = &session_id_opt {
            init.insert("sessionId".into(), json!(sid));
        }
        if let Err(e) = ws_tx
            .send(Message::Text(serde_json::Value::Object(init).to_string()))
            .await
        {
            let msg = format!("send init: {}", e);
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Err(HeyoError::Connection(msg.clone())));
                return;
            }
            let _ = events_tx.send(ShellEvent::Error(msg)).await;
            continue;
        }

        let mut got_ready = false;
        let mut ack_due = false;
        let mut graceful_close = false;
        // ack timer
        let mut ack_timer = tokio::time::interval(ACK_INTERVAL);
        ack_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // heartbeat timer — bumped on every received frame
        let mut heartbeat = Box::pin(sleep(HEARTBEAT_TIMEOUT));

        loop {
            tokio::select! {
                _ = ack_timer.tick() => {
                    if ack_due && last_seq_received > last_seq_acked {
                        last_seq_acked = last_seq_received;
                        if ws_tx
                            .send(Message::Text(json!({
                                "type": "ack",
                                "seq": last_seq_acked,
                            }).to_string()))
                            .await
                            .is_err()
                        {
                            // Socket dropped; the read side will surface it.
                        }
                        ack_due = false;
                    }
                }
                _ = &mut heartbeat => {
                    let _ = ws_tx.send(Message::Close(None)).await;
                    break;
                }
                Some(msg) = write_rx.recv() => {
                    match msg {
                        OutboundMessage::Stdin(bytes) => {
                            let mut frame = Vec::with_capacity(bytes.len() + 1);
                            frame.push(FRAME_STDIN);
                            frame.extend_from_slice(&bytes);
                            if ws_tx.send(Message::Binary(frame)).await.is_err() {
                                break;
                            }
                        }
                        OutboundMessage::Json(v) => {
                            if ws_tx.send(Message::Text(v.to_string())).await.is_err() {
                                break;
                            }
                        }
                        OutboundMessage::Close => {
                            graceful_close = true;
                            let _ = ws_tx
                                .send(Message::Text(json!({"type":"close"}).to_string()))
                                .await;
                            // Give the server 2s grace to send `exit`.
                            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                                while let Some(msg) = ws_rx.next().await {
                                    match msg {
                                        Ok(Message::Text(t)) => {
                                            if handle_control(
                                                &t,
                                                &inner,
                                                &output_tx,
                                                &events_tx,
                                                &mut ready_tx,
                                                &mut got_ready,
                                            )
                                            .await
                                            .is_break()
                                            {
                                                break;
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            })
                            .await;
                            break;
                        }
                    }
                }
                Some(msg) = ws_rx.next() => {
                    heartbeat = Box::pin(sleep(HEARTBEAT_TIMEOUT));
                    match msg {
                        Ok(Message::Text(t)) => {
                            if handle_control(
                                &t,
                                &inner,
                                &output_tx,
                                &events_tx,
                                &mut ready_tx,
                                &mut got_ready,
                            )
                            .await
                            .is_break()
                            {
                                break;
                            }
                            if got_ready {
                                attempt = 0;
                            }
                        }
                        Ok(Message::Binary(bytes)) => {
                            if let Some(seq) = handle_binary(
                                &bytes,
                                last_seq_received,
                                &output_tx,
                            )
                            .await
                            {
                                last_seq_received = seq;
                                ack_due = true;
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                else => break,
            }
        }

        // Socket closed. Decide if we should reconnect.
        let exit_set = inner.exit_code.lock().await.is_some();
        if graceful_close || exit_set {
            break;
        }
        let sid = inner.session_id.lock().await.clone();
        match (sid, reconnect.as_ref()) {
            (Some(_), Some(r)) => {
                attempt += 1;
                if attempt > r.max_retries {
                    let err = HeyoError::Connection(format!(
                        "gave up after {} reconnect attempts",
                        r.max_retries
                    ));
                    let _ = events_tx.send(ShellEvent::Error(err.to_string())).await;
                    break 'outer;
                }
                let delay = backoff(r, attempt);
                let _ = events_tx
                    .send(ShellEvent::Reconnecting { attempt, delay })
                    .await;
                sleep(delay).await;
            }
            _ => {
                // First-connect failure that closed without `ready`: surface
                // to the open() waiter if it's still around.
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(HeyoError::Connection(
                        "shell-stream socket closed before ready".into(),
                    )));
                }
                break;
            }
        }
    }

    let _ = inner.closed_tx.send(true);
    let exit = *inner.exit_code.lock().await;
    let _ = events_tx.send(ShellEvent::Closed { exit_code: exit }).await;
}

async fn handle_control(
    text: &str,
    inner: &Arc<SessionInner>,
    _output_tx: &mpsc::Sender<Vec<u8>>,
    events_tx: &mpsc::Sender<ShellEvent>,
    ready_tx: &mut Option<oneshot::Sender<Result<String, HeyoError>>>,
    got_ready: &mut bool,
) -> std::ops::ControlFlow<()> {
    let msg: ServerControl = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(_) => return std::ops::ControlFlow::Continue(()),
    };
    match msg {
        ServerControl::Ready { session_id, last_seq: _ } => {
            let was_reconnect;
            {
                let mut guard = inner.session_id.lock().await;
                was_reconnect = guard.is_some();
                *guard = Some(session_id.clone());
            }
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Ok(session_id));
            }
            *got_ready = true;
            if was_reconnect {
                let _ = events_tx.send(ShellEvent::Reconnected).await;
            }
            std::ops::ControlFlow::Continue(())
        }
        ServerControl::Exit { code } => {
            *inner.exit_code.lock().await = Some(code);
            std::ops::ControlFlow::Break(())
        }
        ServerControl::Error { code, message } => {
            let err_msg = message.unwrap_or_else(|| "shell-stream server error".to_string());
            let _ = events_tx.send(ShellEvent::Error(err_msg)).await;
            if code.as_deref() == Some("session_expired") {
                let _ = inner.closed_tx.send(true);
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        }
    }
}

async fn handle_binary(
    bytes: &[u8],
    last_seq_received: u64,
    output_tx: &mpsc::Sender<Vec<u8>>,
) -> Option<u64> {
    if bytes.is_empty() || bytes[0] != FRAME_STDOUT || bytes.len() < 9 {
        return None;
    }
    let mut seq_bytes = [0u8; 8];
    seq_bytes.copy_from_slice(&bytes[1..9]);
    let seq = u64::from_be_bytes(seq_bytes);
    if seq <= last_seq_received {
        return None;
    }
    let _ = output_tx.send(bytes[9..].to_vec()).await;
    Some(seq)
}

fn backoff(r: &ShellReconnectOptions, attempt: u32) -> Duration {
    let pow = (attempt.saturating_sub(1)).min(30);
    let factor: u64 = 1u64.checked_shl(pow).unwrap_or(u64::MAX);
    let raw_ms = (r.base_delay.as_millis() as u64).saturating_mul(factor);
    let cap_ms = r.max_delay.as_millis() as u64;
    Duration::from_millis(raw_ms.min(cap_ms))
}

type WsError = tokio_tungstenite::tungstenite::Error;
/// Boxed websocket halves so TCP/TLS (`connect_async`) and unix-socket
/// (`client_async`) connections share one session loop.
type WsSink = Pin<Box<dyn futures_util::Sink<Message, Error = WsError> + Send>>;
type WsSource = Pin<Box<dyn Stream<Item = Result<Message, WsError>> + Send>>;

fn split_ws<S>(ws: tokio_tungstenite::WebSocketStream<S>) -> (WsSink, WsSource)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (tx, rx) = ws.split();
    (Box::pin(tx), Box::pin(rx))
}

fn host_from_url(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_string();
            if let Some(port) = u.port() {
                Some(format!("{}:{}", host, port))
            } else {
                Some(host)
            }
        })
        .unwrap_or_default()
}

#[cfg(all(test, unix))]
mod uds_tests {
    use super::*;

    /// End-to-end shell protocol over a unix socket: a real tungstenite
    /// server accepts on a `UnixListener`, speaks init/ready, echoes stdin
    /// back as stdout frames, and answers `close` with `exit`. Proves the
    /// `client_async`-over-`UnixStream` path without a live daemon.
    #[tokio::test]
    async fn shell_session_over_unix_socket() {
        let path = std::env::temp_dir().join(format!(
            "heyo-sdk-shellws-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // First frame must be init.
            let init = loop {
                match ws.next().await.unwrap().unwrap() {
                    Message::Text(t) => break t,
                    _ => continue,
                }
            };
            let v: serde_json::Value = serde_json::from_str(&init).unwrap();
            assert_eq!(v["type"], "init");
            ws.send(Message::Text(
                json!({"type": "ready", "sessionId": "sess-uds-1"}).to_string(),
            ))
            .await
            .unwrap();
            let mut seq: u64 = 0;
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Binary(b) if b.first() == Some(&FRAME_STDIN) => {
                        seq += 1;
                        let mut frame = vec![FRAME_STDOUT];
                        frame.extend_from_slice(&seq.to_be_bytes());
                        frame.extend_from_slice(&b[1..]);
                        ws.send(Message::Binary(frame)).await.unwrap();
                    }
                    Message::Text(t) => {
                        let v: serde_json::Value =
                            serde_json::from_str(&t).unwrap_or_default();
                        if v["type"] == "close" {
                            ws.send(Message::Text(
                                json!({"type": "exit", "code": 0}).to_string(),
                            ))
                            .await
                            .unwrap();
                            break;
                        }
                        // acks / resizes need no reply
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let client = HeyoClient::local_socket(&path).unwrap();
        let session = ShellSession::open(client, "sbx-uds".to_string(), ShellOptions::default())
            .await
            .expect("open shell over unix socket");
        assert_eq!(session.session_id().await.as_deref(), Some("sess-uds-1"));

        session.write(b"echo hi\n").await.unwrap();
        let mut out = session.output();
        let chunk = tokio::time::timeout(Duration::from_secs(5), out.next())
            .await
            .expect("stdout within 5s")
            .expect("stream open");
        assert_eq!(chunk, b"echo hi\n");

        session.close().await.unwrap();
        assert!(session.is_closed());
        assert_eq!(session.exit_code().await, Some(0));
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
