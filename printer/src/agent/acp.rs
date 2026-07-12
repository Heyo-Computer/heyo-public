//! Agent Client Protocol (ACP) transport.
//!
//! Holds a long-lived `tokio::process::Child` running an ACP server (e.g.
//! `claude-code-acp`, Poolside) and speaks newline-delimited JSON-RPC 2.0 over
//! its stdio. Unlike the one-shot `claude --print` / `opencode run` backends,
//! the same child persists across `Session::turn` calls — `session/new` is
//! sent once on bootstrap and `session/prompt` is sent per turn.
//!
//! T-017 scope: blocking-turn semantics. Streaming-to-stderr, Ctrl-C
//! cancellation via `session/cancel`, and permission-mode mapping are tracked
//! separately on T-020.
//!
//! See https://agentclientprotocol.com for the wire spec.

use crate::agent::{TokenUsage, TurnOutcome};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::IsTerminal;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

/// JSON-RPC response payload returned to a pending request waiter.
#[derive(Debug)]
pub enum RpcResult {
    Ok(Value),
    /// JSON-RPC application-level error (server returned `{ error: … }`).
    Err(Value),
    /// Transport-level failure — the ACP server's stdout closed or the child
    /// exited before we received a response. The string carries diagnostics
    /// (recent non-JSON stdout + recent stderr) so the user can see *why* the
    /// server died (e.g. "read-only file system" from poolside's log setup).
    Transport(String),
}

/// One server-pushed `session/update` notification.
#[derive(Debug, Clone)]
pub struct SessionUpdate {
    pub session_id: String,
    pub update: Value,
}

/// Rolling buffer of the last few non-JSON stdout lines and stderr lines from
/// the ACP child. Surfaced in transport errors so a server that dies before
/// emitting its first valid JSON-RPC message still tells the user what went
/// wrong.
#[derive(Default)]
struct DiagnosticBuf {
    bad_stdout: VecDeque<String>,
    stderr: VecDeque<String>,
}

impl DiagnosticBuf {
    const MAX: usize = 16;

    fn push_bad_stdout(&mut self, line: String) {
        if self.bad_stdout.len() == Self::MAX {
            self.bad_stdout.pop_front();
        }
        self.bad_stdout.push_back(line);
    }

    fn push_stderr(&mut self, line: String) {
        if self.stderr.len() == Self::MAX {
            self.stderr.pop_front();
        }
        self.stderr.push_back(line);
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if !self.bad_stdout.is_empty() {
            out.push_str("\n  recent non-JSON stdout:");
            for l in &self.bad_stdout {
                out.push_str("\n    ");
                out.push_str(l);
            }
        }
        if !self.stderr.is_empty() {
            out.push_str("\n  recent stderr:");
            for l in &self.stderr {
                out.push_str("\n    ");
                out.push_str(l);
            }
        }
        if out.is_empty() {
            out.push_str(" (no diagnostic output captured before exit)");
        }
        out
    }
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>>;

/// Long-lived client for one ACP server child.
pub struct AcpClient {
    writer: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicU64,
    notifications: Mutex<mpsc::UnboundedReceiver<SessionUpdate>>,
    _reader_handle: tokio::task::JoinHandle<()>,
    _watcher_handle: tokio::task::JoinHandle<()>,
    session_id: Mutex<Option<String>>,
    diagnostic: Arc<Mutex<DiagnosticBuf>>,
    /// Set true once the child has exited (or once we've initiated shutdown).
    /// Read under the `pending` lock by `request()` so we fail fast instead of
    /// queueing a request that nothing will ever answer.
    dead: Arc<AtomicBool>,
    /// Set true by `shutdown()` / `Drop` before signalling the watcher; tells
    /// the watcher this exit is intentional, so it should not drain pending
    /// senders with a "server died" error.
    shutting_down: Arc<AtomicBool>,
    /// Signal channel for the watcher task — sending `()` asks it to kill the
    /// child and exit. The watcher owns the `Child` directly.
    kill_tx: mpsc::UnboundedSender<()>,
}

impl AcpClient {
    /// Spawn the ACP server. `bin` and `args` are the launch command. If
    /// `command_wrapper` is `Some`, the argv is shell-quoted and substituted
    /// for `{child}` in the wrapper template, then run via `sh -c` — same
    /// machinery the one-shot backends use to dispatch into a sandbox.
    pub async fn spawn(
        bin: &str,
        args: &[String],
        cwd: Option<&Path>,
        command_wrapper: Option<&str>,
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut argv: Vec<String> = Vec::with_capacity(args.len() + 1);
        argv.push(bin.to_string());
        for a in args {
            argv.push(a.clone());
        }

        let mut cmd = if let Some(template) = command_wrapper {
            let quoted = crate::drivers::shell_quote_argv(&argv);
            let resolved = template.replace("{child}", &quoted);
            let mut c = Command::new("sh");
            c.arg("-c").arg(&resolved);
            c
        } else {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        };
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning ACP server `{bin}`"))?;
        let stdin = child.stdin.take().context("ACP child has no stdin pipe")?;
        let stdout = child
            .stdout
            .take()
            .context("ACP child has no stdout pipe")?;
        let stderr = child.stderr.take();

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, notif_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let diagnostic: Arc<Mutex<DiagnosticBuf>> = Arc::new(Mutex::new(DiagnosticBuf::default()));
        let dead = Arc::new(AtomicBool::new(false));
        let shutting_down = Arc::new(AtomicBool::new(false));
        // Build the writer Arc up-front so the reader can clone it and
        // respond to server-to-client requests inline. (See
        // `dispatch_message` — the ACP spec defines `session/request_permission`
        // and other RPCs that the server *issues* to the client; a server
        // that gets no reply blocks forever.)
        let writer: Arc<Mutex<ChildStdin>> = Arc::new(Mutex::new(stdin));

        let trace = std::env::var("PRINTER_ACP_TRACE")
            .ok()
            .map(|v| v != "0")
            .unwrap_or(false);
        let pending_for_reader = pending.clone();
        let diag_for_reader = diagnostic.clone();
        let dead_for_reader = dead.clone();
        let shutting_down_for_reader = shutting_down.clone();
        let writer_for_reader = writer.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            let mut total_lines: u64 = 0;
            if trace {
                eprintln!("[acp:trace] reader started; awaiting lines from server stdout");
            }
            while let Ok(Some(line)) = reader.next_line().await {
                total_lines += 1;
                let trimmed = line.trim();
                if trace {
                    let preview: String = trimmed.chars().take(120).collect();
                    let suffix = if trimmed.chars().count() > 120 {
                        "…"
                    } else {
                        ""
                    };
                    eprintln!(
                        "[acp:trace] read line #{} ({} bytes): {preview}{suffix}",
                        total_lines,
                        trimmed.len()
                    );
                }
                if trimmed.is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[acp] dropping unparseable line ({e}): {trimmed}");
                        diag_for_reader
                            .lock()
                            .await
                            .push_bad_stdout(trimmed.to_string());
                        continue;
                    }
                };
                dispatch_message(
                    value,
                    &pending_for_reader,
                    &notif_tx,
                    Some(&writer_for_reader),
                )
                .await;
            }
            if trace {
                eprintln!("[acp:trace] reader exiting after {total_lines} line(s) (stdout EOF)");
            }
            // EOF on stdout — server is no longer talking to us. Drain any
            // pending requests so callers don't sit forever waiting for a
            // response that will never arrive. Skip if shutdown is in flight
            // (we initiated the kill).
            if !shutting_down_for_reader.load(Ordering::SeqCst) {
                let reason = {
                    let diag = diag_for_reader.lock().await;
                    format!(
                        "ACP server stdout closed before responding.{}",
                        diag.render()
                    )
                };
                drain_pending(&pending_for_reader, &dead_for_reader, &reason).await;
            }
        });

        // Forward stderr verbatim — surfaces server diagnostics to the user —
        // and tee a copy into the diagnostic buffer so it can be included in
        // transport errors when the server dies before answering.
        if let Some(err) = stderr {
            let diag_for_stderr = diagnostic.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(err).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    eprintln!("[acp:server] {line}");
                    diag_for_stderr.lock().await.push_stderr(line);
                }
            });
        }

        // Watcher: owns the child, awaits its exit, and on unexpected exit
        // drains pending senders with a transport error. Killable via
        // `kill_tx` for shutdown.
        let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<()>();
        let pending_for_watcher = pending.clone();
        let diag_for_watcher = diagnostic.clone();
        let dead_for_watcher = dead.clone();
        let shutting_down_for_watcher = shutting_down.clone();
        let bin_label = bin.to_string();
        let watcher_handle = tokio::spawn(async move {
            let exit_status = tokio::select! {
                _ = kill_rx.recv() => {
                    // Kill the whole process group — the child was spawned
                    // with process_group(0), so SIGKILL on -pid reaches any
                    // grandchildren that would otherwise keep stdio pipes
                    // open and stall reader cleanup. Falls back to
                    // start_kill on non-unix or if pgid lookup fails.
                    #[cfg(unix)]
                    if let Some(pid) = child.id() {
                        // SAFETY: kill() with a negative pid signals a
                        // process group; ESRCH on an invalid pgid is a
                        // no-op. No memory is touched.
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    let _ = child.start_kill();
                    child.wait().await.ok()
                }
                res = child.wait() => res.ok(),
            };
            if !shutting_down_for_watcher.load(Ordering::SeqCst) {
                let reason = {
                    let diag = diag_for_watcher.lock().await;
                    let head = match exit_status {
                        Some(s) => format!("ACP server `{bin_label}` exited unexpectedly ({s})."),
                        None => format!(
                            "ACP server `{bin_label}` exited unexpectedly (status unavailable)."
                        ),
                    };
                    format!("{head}{}", diag.render())
                };
                eprintln!("[acp] {reason}");
                drain_pending(&pending_for_watcher, &dead_for_watcher, &reason).await;
            }
        });

        Ok(Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            notifications: Mutex::new(notif_rx),
            _reader_handle: reader_handle,
            _watcher_handle: watcher_handle,
            session_id: Mutex::new(None),
            diagnostic,
            dead,
            shutting_down,
            kill_tx,
        })
    }

    /// Send `initialize` per ACP spec. Must succeed before `session/new`.
    pub async fn initialize(&self) -> Result<Value> {
        // protocolVersion is the integer schema version; v1 of the spec uses 1.
        let params = json!({
            "protocolVersion": 1,
            "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } },
        });
        self.request("initialize", params).await
    }

    /// Send `session/new` and remember the returned `sessionId`. The optional
    /// `permission_hint` is passed alongside the spec'd params under the
    /// non-standard `_printerPermissionMode` key — JSON-RPC servers must
    /// ignore unknown fields, so this is a best-effort hint that an
    /// ACP-aware server may pick up. The authoritative permission policy is
    /// always whatever the ACP server itself enforces.
    pub async fn session_new(&self, cwd: &Path, permission_hint: Option<&str>) -> Result<String> {
        let mut params = json!({
            "cwd": cwd.to_string_lossy(),
            "mcpServers": [],
        });
        if let Some(mode) = permission_hint
            && let Some(obj) = params.as_object_mut()
        {
            obj.insert(
                "_printerPermissionMode".to_string(),
                Value::String(mode.to_string()),
            );
        }
        let result = self.request("session/new", params).await?;
        let id = result
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("session/new response missing sessionId: {result}"))?
            .to_string();
        *self.session_id.lock().await = Some(id.clone());
        Ok(id)
    }

    /// Send a single `session/prompt` and block until the request resolves.
    /// Drains `session/update` notifications that arrive in the meantime,
    /// concatenates their text content blocks into `result_text`, and emits
    /// per-update progress logs so a stalled poolside is visibly stalled
    /// rather than silently spinning. High-signal updates (`tool_call`,
    /// `tool_call_update`, `plan`) are always logged; per-chunk message and
    /// thought summaries plus a periodic heartbeat are gated on `verbose`.
    ///
    /// Token usage is best-effort: ACP doesn't standardize a usage field, so
    /// `extract_acp_usage` scans the final `session/prompt` result for a
    /// `usage`/`tokens` object (under a few common key conventions, including
    /// `_meta`). When the server omits usage entirely we fall back to
    /// `TokenUsage::default()` and warn once — the compaction trigger then
    /// behaves as if the session never crosses the threshold for that backend.
    pub async fn prompt_blocking(
        &self,
        session_id: &str,
        prompt: &str,
        verbose: bool,
    ) -> Result<TurnOutcome> {
        let params = json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": prompt }],
        });

        let result_fut = self.request("session/prompt", params);
        tokio::pin!(result_fut);

        let started = Instant::now();
        let last_update: Arc<StdMutex<(Instant, String)>> =
            Arc::new(StdMutex::new((started, "starting".to_string())));
        let stop = Arc::new(AtomicBool::new(false));
        let tty = std::io::stderr().is_terminal();

        // Heartbeat is verbose-only — without it, only the always-on per-event
        // logs (tool_call etc.) print, which is plenty for normal runs but too
        // sparse when investigating a hang.
        let heartbeat_handle = if verbose {
            let stop = stop.clone();
            let last = last_update.clone();
            Some(tokio::spawn(async move {
                heartbeat_loop(stop, last, started, tty).await;
            }))
        } else {
            None
        };

        let mut text = String::new();
        // Per-turn rate-limit tracking for noisy chunked content. Reset every
        // turn — we only care about pacing within a single in-flight prompt.
        let mut chunk_state = ChunkLogState::default();

        let mut prompt_result: Option<Value> = None;
        let mut notifications = self.notifications.lock().await;
        let result: Result<()> = loop {
            tokio::select! {
                biased;
                rpc = &mut result_fut => {
                    match rpc {
                        Ok(value) => {
                            prompt_result = Some(value);
                            // Drain any remaining notifications already buffered.
                            while let Ok(update) = notifications.try_recv() {
                                if update.session_id == session_id {
                                    observe_update(
                                        &update.update,
                                        &mut text,
                                        &mut chunk_state,
                                        &last_update,
                                        verbose,
                                        tty,
                                    );
                                }
                            }
                            break Ok(());
                        }
                        Err(e) => break Err(e),
                    }
                }
                Some(update) = notifications.recv() => {
                    if update.session_id == session_id {
                        observe_update(
                            &update.update,
                            &mut text,
                            &mut chunk_state,
                            &last_update,
                            verbose,
                            tty,
                        );
                    }
                }
            }
        };

        stop.store(true, Ordering::Relaxed);
        if let Some(h) = heartbeat_handle {
            let _ = h.await;
            if tty {
                eprint!("\r\x1b[2K");
            }
        }

        result?;
        let usage = prompt_result
            .as_ref()
            .and_then(extract_acp_usage)
            .unwrap_or_else(|| {
                if !ACP_USAGE_MISSING_WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[printer] note: this ACP server did not report token usage; \
                         metrics will show zero and session compaction won't trigger for it."
                    );
                }
                TokenUsage::default()
            });
        Ok(TurnOutcome {
            result_text: text,
            usage,
            backend_session_id: None,
            tools: Vec::new(),
        })
    }

    /// Send a JSON-RPC 2.0 request and await its response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let trace = std::env::var("PRINTER_ACP_TRACE")
            .ok()
            .map(|v| v != "0")
            .unwrap_or(false);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();

        // Insert under the pending lock and check `dead` while holding it.
        // The watcher sets `dead` *before* draining pending under the same
        // lock, so this ordering guarantees we either (a) see `dead` and bail
        // immediately, or (b) insert and have the watcher drain us when it
        // fires.
        {
            let mut map = self.pending.lock().await;
            if self.dead.load(Ordering::SeqCst) {
                drop(map);
                let diag = self.diagnostic.lock().await.render();
                return Err(anyhow!("ACP {method}: server is not running.{diag}"));
            }
            map.insert(id, tx);
        }

        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&frame)?;
        if trace {
            eprintln!(
                "[acp:trace] writing {method} (id={id}, {} bytes + LF)",
                line.len()
            );
        }
        {
            let mut w = self.writer.lock().await;
            if let Err(e) = async {
                w.write_all(line.as_bytes()).await?;
                w.write_all(b"\n").await?;
                w.flush().await?;
                Ok::<_, std::io::Error>(())
            }
            .await
            {
                // Write failed — pipe likely closed because the child died.
                // Remove our pending entry so we surface the I/O error rather
                // than waiting on a sender that may never fire.
                self.pending.lock().await.remove(&id);
                let diag = self.diagnostic.lock().await.render();
                return Err(anyhow!(
                    "ACP {method}: writing to server stdin failed ({e}).{diag}"
                ));
            }
        }
        if trace {
            eprintln!("[acp:trace] {method} (id={id}) flushed; awaiting response");
        }

        match rx.await {
            Ok(RpcResult::Ok(v)) => {
                if trace {
                    eprintln!("[acp:trace] {method} (id={id}) ok");
                }
                Ok(v)
            }
            Ok(RpcResult::Err(e)) => Err(anyhow!("ACP {method} failed: {e}")),
            Ok(RpcResult::Transport(msg)) => Err(anyhow!("ACP {method} aborted: {msg}")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!(
                    "ACP {method} response channel dropped (server exited?)"
                ))
            }
        }
    }

    /// Fire `session/cancel` as a JSON-RPC notification (no `id`, no
    /// response). Per the ACP spec, servers receive this fire-and-forget and
    /// should resolve the in-flight `session/prompt` with a `cancelled`
    /// stopReason. Used on Ctrl-C; safe to call even if the server has
    /// already exited (returns Ok in that case).
    pub async fn cancel(&self) -> Result<()> {
        let id = match self.session_id.lock().await.clone() {
            Some(id) => id,
            None => return Ok(()),
        };
        if self.dead.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.notify("session/cancel", json!({ "sessionId": id }))
            .await
    }

    /// Mark the client shut down and ask the watcher to kill the child's
    /// whole process group. Non-consuming so it can be called from rotate
    /// (where `Session` retains the client by value across turns) as well
    /// as from Ctrl-C. Subsequent calls are no-ops.
    pub async fn shutdown(&self) {
        // Best-effort cancel before tearing down — servers that respect
        // session/cancel get a chance to flush their state.
        let _ = self.cancel().await;
        if !self.shutting_down.swap(true, Ordering::SeqCst) {
            let _ = self.kill_tx.send(());
        }
    }

    /// Send a JSON-RPC notification (no `id`, no response). Used for
    /// fire-and-forget messages like `session/cancel`.
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        if self.dead.load(Ordering::SeqCst) {
            return Ok(());
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&frame)?;
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // Mark this as an intentional teardown so the watcher doesn't drain
        // pending senders with a misleading "server died" error, then ask the
        // watcher to kill the child. If the watcher has already exited (child
        // died on its own), the send is a no-op.
        self.shutting_down.store(true, Ordering::SeqCst);
        let _ = self.kill_tx.send(());
    }
}

/// Drain every pending request, sending each waiter a `Transport` error with
/// `reason`. Sets `dead` true *before* draining so any concurrent `request()`
/// call that grabs the pending lock after us sees the flag and bails instead
/// of queueing a doomed request.
async fn drain_pending(pending: &PendingMap, dead: &Arc<AtomicBool>, reason: &str) {
    let mut map = pending.lock().await;
    dead.store(true, Ordering::SeqCst);
    for (_, tx) in map.drain() {
        let _ = tx.send(RpcResult::Transport(reason.to_string()));
    }
}

async fn dispatch_message(
    value: Value,
    pending: &PendingMap,
    notif_tx: &mpsc::UnboundedSender<SessionUpdate>,
    // `Option` so unit tests that exercise the dispatch logic without a
    // real `Child` (i.e. without a `ChildStdin`) can still call the
    // function. Production callers always pass `Some(&writer)`.
    writer: Option<&Arc<Mutex<ChildStdin>>>,
) {
    // Response: has `id` and (`result` or `error`), no `method`.
    if value.get("method").is_none() {
        let id = value.get("id").and_then(|v| v.as_u64());
        if let Some(id) = id {
            let mut map = pending.lock().await;
            if let Some(tx) = map.remove(&id) {
                if let Some(err) = value.get("error") {
                    let _ = tx.send(RpcResult::Err(err.clone()));
                } else {
                    let result = value.get("result").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(RpcResult::Ok(result));
                }
            }
        }
        return;
    }

    let method = value
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let req_id = value.get("id").cloned();

    // Notification: has `method`, no `id`. ACP server-pushed
    // updates use `session/update` with a `sessionId` + `update` payload.
    if method == "session/update" {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let update = params.get("update").cloned().unwrap_or(Value::Null);
        let _ = notif_tx.send(SessionUpdate { session_id, update });
        return;
    }

    // Server-to-client request: has both `method` *and* `id`. The ACP spec
    // includes RPCs that flow server→client — `session/request_permission`
    // (asks the client to approve a tool call), `fs/read_text_file` /
    // `fs/write_text_file` (filesystem ops gated on the client capability
    // we sent in `initialize`), terminal RPCs, etc. A server that issues
    // one of these and gets no response will block its turn indefinitely;
    // that's exactly what was killing tool execution in poolside before.
    if let Some(id) = req_id {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "session/request_permission" => {
                let outcome = pick_permission_outcome(&params);
                json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": outcome } })
            }
            other => {
                eprintln!(
                    "[acp] unhandled server→client request `{other}` (id={id}); replying method-not-found"
                );
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("printer's ACP client does not handle `{other}`"),
                    },
                })
            }
        };
        if let Some(w) = writer {
            if let Err(e) = send_frame(w, &response).await {
                eprintln!("[acp] failed to reply to server request `{method}`: {e}");
            }
        } else {
            eprintln!("[acp] no writer available to reply to `{method}` (id={id}); test path?");
        }
    }
}

/// Pick a permission-decision outcome for a `session/request_permission`
/// payload. Printer drives ACP non-interactively — there is no human at
/// the keyboard to click "approve" — so we always pick the most
/// permissive `allow_*` option the server offered. Preference order:
/// `allow_always` (don't ask again for this kind of action), then
/// `allow_once`, then any other `allow_*` variant the server invented.
/// Falls back to `cancelled` if no allow option exists, which surfaces
/// the cancel through the server's tool-call result so the agent can
/// recover instead of hanging.
fn pick_permission_outcome(params: &Value) -> Value {
    let options = params.get("options").and_then(|v| v.as_array());
    let pick = |kind: &str, opts: &[Value]| -> Option<String> {
        opts.iter().find_map(|o| {
            (o.get("kind").and_then(|v| v.as_str()) == Some(kind))
                .then(|| o.get("optionId").and_then(|v| v.as_str()).map(String::from))
                .flatten()
        })
    };
    if let Some(opts) = options {
        for kind in ["allow_always", "allow_once"] {
            if let Some(id) = pick(kind, opts) {
                return json!({ "outcome": "selected", "optionId": id });
            }
        }
        // Some servers may use other allow_* kinds — fall back to a prefix
        // match so we still pick "allow" over "reject" / "cancel".
        if let Some(id) = opts.iter().find_map(|o| {
            let kind = o.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind.starts_with("allow") {
                o.get("optionId").and_then(|v| v.as_str()).map(String::from)
            } else {
                None
            }
        }) {
            return json!({ "outcome": "selected", "optionId": id });
        }
    }
    json!({ "outcome": "cancelled" })
}

/// Write one JSON-RPC frame to the server stdin pipe (line-delimited JSON).
/// Warn at most once per process when an ACP server reports no token usage.
static ACP_USAGE_MISSING_WARNED: AtomicBool = AtomicBool::new(false);

/// Best-effort token-usage extraction from a `session/prompt` result. ACP has
/// no standardized usage field, so we look in the conventional spots — the
/// result root, then a `_meta`/`meta` envelope — for a `usage` or `tokens`
/// object, and accept any of the common per-field key spellings. Returns
/// `None` if nothing usage-shaped is present so the caller can fall back.
fn extract_acp_usage(result: &Value) -> Option<TokenUsage> {
    let obj = ["usage", "tokens"]
        .into_iter()
        .find_map(|k| result.get(k))
        .or_else(|| {
            ["_meta", "meta"]
                .into_iter()
                .filter_map(|env| result.get(env))
                .find_map(|m| m.get("usage").or_else(|| m.get("tokens")))
        })?;

    // Accept snake_case, camelCase, and the opencode-style nested `cache`.
    let pick = |keys: &[&str]| -> u64 {
        keys.iter()
            .find_map(|k| obj.get(*k).and_then(|v| v.as_u64()))
            .unwrap_or(0)
    };
    let cache = obj.get("cache");
    let cache_pick = |nested: &str, flat: &[&str]| -> u64 {
        cache
            .and_then(|c| c.get(nested))
            .and_then(|v| v.as_u64())
            .or_else(|| {
                flat.iter()
                    .find_map(|k| obj.get(*k).and_then(|v| v.as_u64()))
            })
            .unwrap_or(0)
    };

    let usage = TokenUsage {
        input_tokens: pick(&["input_tokens", "inputTokens", "input"]),
        output_tokens: pick(&["output_tokens", "outputTokens", "output"]),
        cache_creation_input_tokens: cache_pick(
            "write",
            &[
                "cache_creation_input_tokens",
                "cacheCreationInputTokens",
                "cache_write",
            ],
        ),
        cache_read_input_tokens: cache_pick(
            "read",
            &[
                "cache_read_input_tokens",
                "cacheReadInputTokens",
                "cache_read",
            ],
        ),
    };

    // A usage object that parsed to all-zero is indistinguishable from absent;
    // treat it as present only if at least one field is non-zero.
    if usage.grand_total() == 0 {
        None
    } else {
        Some(usage)
    }
}

/// Used by `dispatch_message` to reply to server→client requests.
async fn send_frame(writer: &Arc<Mutex<ChildStdin>>, frame: &Value) -> anyhow::Result<()> {
    let line = serde_json::to_string(frame)?;
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

/// Walk an `update` payload and append any text content found. ACP content
/// blocks may appear as `agent_message_chunk` / `agent_thought_chunk` /
/// `tool_call` / etc.; we collect anything with a `text` field for now.
fn collect_update_text(update: &Value, sink: &mut String) {
    // Common shapes:
    //   { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "..." } }
    //   { "sessionUpdate": "agent_message_chunk", "content": [{ "type": "text", "text": "..." }] }
    if let Some(content) = update.get("content") {
        collect_content_text(content, sink);
    }
}

/// Classified shape of one `session/update` notification. Drives the per-event
/// logging — `Other` is the catch-all for ACP variants we don't unwrap (mode
/// updates, available-commands updates, etc.) so they still leave a trace.
#[derive(Debug, Clone)]
enum UpdateKind {
    AgentMessage {
        text: String,
        is_thought: bool,
    },
    ToolCall {
        id: String,
        title: String,
        kind: String,
    },
    ToolCallUpdate {
        id: String,
        status: String,
        title: String,
    },
    Plan {
        entry_count: usize,
    },
    Other {
        kind: String,
    },
}

fn classify(update: &Value) -> Option<UpdateKind> {
    let kind = update.get("sessionUpdate").and_then(|v| v.as_str())?;
    let str_field = |name: &str| -> String {
        update
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    Some(match kind {
        "agent_message_chunk" => UpdateKind::AgentMessage {
            text: extract_text(update),
            is_thought: false,
        },
        "agent_thought_chunk" => UpdateKind::AgentMessage {
            text: extract_text(update),
            is_thought: true,
        },
        "tool_call" => UpdateKind::ToolCall {
            id: str_field("toolCallId"),
            title: str_field("title"),
            kind: str_field("kind"),
        },
        "tool_call_update" => UpdateKind::ToolCallUpdate {
            id: str_field("toolCallId"),
            status: str_field("status"),
            title: str_field("title"),
        },
        "plan" => UpdateKind::Plan {
            entry_count: update
                .get("entries")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
        },
        other => UpdateKind::Other {
            kind: other.to_string(),
        },
    })
}

fn extract_text(update: &Value) -> String {
    let mut out = String::new();
    if let Some(content) = update.get("content") {
        collect_content_text(content, &mut out);
    }
    out
}

/// One-line label used by the heartbeat to describe the most recent activity.
fn summary(kind: &UpdateKind) -> String {
    match kind {
        UpdateKind::AgentMessage {
            is_thought: true, ..
        } => "thinking".to_string(),
        UpdateKind::AgentMessage {
            is_thought: false, ..
        } => "writing message".to_string(),
        UpdateKind::ToolCall { kind: k, title, .. } => {
            let label = if k.is_empty() {
                "tool".to_string()
            } else {
                format!("tool/{k}")
            };
            if title.is_empty() {
                label
            } else {
                format!("{label} {}", first_line(title, 60))
            }
        }
        UpdateKind::ToolCallUpdate { id, status, title } => {
            let id_short = id_short(id);
            let label = if title.is_empty() {
                format!("tool {id_short}")
            } else {
                format!("tool {id_short} {}", first_line(title, 40))
            };
            format!("{label} → {status}")
        }
        UpdateKind::Plan { entry_count } => format!("plan ({entry_count} entries)"),
        UpdateKind::Other { kind } => format!("kind={kind}"),
    }
}

#[derive(Default)]
struct ChunkLogState {
    /// Total chars seen across all message chunks this turn — folded into
    /// the verbose log so the user can see how much output has accumulated.
    message_chars: usize,
    thought_chars: usize,
    /// Last time we emitted a chunk-progress line. Used to rate-limit so a
    /// poolside that streams a token at a time doesn't generate one log line
    /// per token.
    last_chunk_log: Option<Instant>,
}

const CHUNK_LOG_INTERVAL_MS: u128 = 1_000;

/// Process one classified update: collect text, update the heartbeat tracker,
/// and emit per-event log lines. High-signal updates always log; chunked
/// message/thought streams only log under verbose, rate-limited.
fn observe_update(
    raw: &Value,
    sink: &mut String,
    chunk_state: &mut ChunkLogState,
    last_update: &Arc<StdMutex<(Instant, String)>>,
    verbose: bool,
    tty: bool,
) {
    // Always collect text into the result (used by the caller as
    // `result_text` for downstream prompts), independent of logging.
    collect_update_text(raw, sink);

    let Some(kind) = classify(raw) else {
        return;
    };

    // Update the heartbeat's "last activity" tracker.
    if let Ok(mut g) = last_update.lock() {
        *g = (Instant::now(), summary(&kind));
    }

    // Log policy:
    //   - tool_call / tool_call_update / plan / other: always one line.
    //   - agent_message / agent_thought chunks: verbose only, rate-limited.
    match &kind {
        UpdateKind::ToolCall { id, title, kind: k } => {
            clear_spinner(verbose, tty);
            let id_short = id_short(id);
            let label = if k.is_empty() {
                "tool".to_string()
            } else {
                format!("tool/{k}")
            };
            let title = if title.is_empty() {
                "(no title)".to_string()
            } else {
                first_line(title, 120)
            };
            eprintln!("[acp:agent] {label} start [{id_short}] {title}");
        }
        UpdateKind::ToolCallUpdate { id, status, title } => {
            clear_spinner(verbose, tty);
            let id_short = id_short(id);
            let title = if title.is_empty() {
                String::new()
            } else {
                format!(" {}", first_line(title, 80))
            };
            eprintln!("[acp:agent] tool [{id_short}]{title} → {status}");
        }
        UpdateKind::Plan { entry_count } => {
            clear_spinner(verbose, tty);
            eprintln!("[acp:agent] plan ({entry_count} entries)");
        }
        UpdateKind::Other { kind } => {
            clear_spinner(verbose, tty);
            eprintln!("[acp:agent] update kind={kind}");
        }
        UpdateKind::AgentMessage { text, is_thought } => {
            // Track running totals regardless of verbose, so the heartbeat
            // summary stays accurate even without per-chunk logs.
            let len = text.chars().count();
            if *is_thought {
                chunk_state.thought_chars += len;
            } else {
                chunk_state.message_chars += len;
            }
            if !verbose {
                return;
            }
            let now = Instant::now();
            let ready = match chunk_state.last_chunk_log {
                Some(prev) => prev.elapsed().as_millis() >= CHUNK_LOG_INTERVAL_MS,
                None => true,
            };
            if !ready {
                return;
            }
            chunk_state.last_chunk_log = Some(now);
            clear_spinner(verbose, tty);
            let label = if *is_thought { "thought" } else { "message" };
            let total = if *is_thought {
                chunk_state.thought_chars
            } else {
                chunk_state.message_chars
            };
            let snippet = first_line(text, 100);
            if snippet.is_empty() {
                eprintln!("[acp:agent] {label} chunk ({total} chars total)");
            } else {
                eprintln!("[acp:agent] {label} ({total} chars total): {snippet}");
            }
        }
    }
}

/// Heartbeat companion to `prompt_blocking`'s select loop. In a TTY, draws an
/// animated spinner with elapsed-time and a freshness marker for the last
/// observed update; in a pipe, emits a textual heartbeat every ~10s. Mirrors
/// the shape of `session::heartbeat_loop` so the two modes feel the same.
async fn heartbeat_loop(
    stop: Arc<AtomicBool>,
    last_update: Arc<StdMutex<(Instant, String)>>,
    started: Instant,
    tty: bool,
) {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let mut i: usize = 0;
    let mut last_text_heartbeat = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let elapsed = started.elapsed().as_secs();
        let (last_at, label) = match last_update.lock() {
            Ok(g) => (g.0, g.1.clone()),
            Err(p) => (p.into_inner().0, "(poisoned)".to_string()),
        };
        let ago = last_at.elapsed().as_secs();

        if tty {
            eprint!(
                "\r\x1b[2K[printer] {} acp turn… {}s (last update {}s ago: {})",
                FRAMES[i % FRAMES.len()],
                elapsed,
                ago,
                label
            );
            i = i.wrapping_add(1);
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        } else if last_text_heartbeat.elapsed().as_secs() >= 10 {
            eprintln!("[printer] still working… {elapsed}s (last update {ago}s ago: {label})");
            last_text_heartbeat = Instant::now();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}

fn clear_spinner(verbose: bool, tty: bool) {
    if verbose && tty {
        eprint!("\r\x1b[2K");
    }
}

fn id_short(id: &str) -> String {
    if id.is_empty() {
        return "?".to_string();
    }
    id.chars().take(8).collect()
}

fn first_line(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

fn collect_content_text(content: &Value, sink: &mut String) {
    match content {
        Value::Array(items) => {
            for item in items {
                collect_content_text(item, sink);
            }
        }
        Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some("text")
                && let Some(t) = map.get("text").and_then(|v| v.as_str())
            {
                sink.push_str(t);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatches_response_to_pending_waiter() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, _notif_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(7, tx);

        let msg = json!({"jsonrpc":"2.0","id":7,"result":{"hello":"world"}});
        dispatch_message(msg, &pending, &notif_tx, None).await;

        let got = rx.await.unwrap();
        match got {
            RpcResult::Ok(v) => assert_eq!(v, json!({"hello":"world"})),
            RpcResult::Err(e) => panic!("expected ok, got err: {e}"),
            RpcResult::Transport(m) => panic!("expected ok, got transport: {m}"),
        }
    }

    #[tokio::test]
    async fn dispatches_error_response() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, _notif_rx) = mpsc::unbounded_channel::<SessionUpdate>();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(3, tx);

        let msg = json!({"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"nope"}});
        dispatch_message(msg, &pending, &notif_tx, None).await;

        match rx.await.unwrap() {
            RpcResult::Err(v) => assert_eq!(v["code"], -32601),
            other => panic!("expected err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forwards_session_update_notification() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<SessionUpdate>();

        let msg = json!({
            "jsonrpc":"2.0",
            "method":"session/update",
            "params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}
        });
        dispatch_message(msg, &pending, &notif_tx, None).await;

        let got = notif_rx.recv().await.unwrap();
        assert_eq!(got.session_id, "sess-1");
        let mut buf = String::new();
        collect_update_text(&got.update, &mut buf);
        assert_eq!(buf, "hi");
    }

    #[test]
    fn permission_outcome_prefers_allow_always() {
        let params = json!({
            "options": [
                {"kind": "reject_once", "optionId": "no", "name": "Reject"},
                {"kind": "allow_once", "optionId": "yes-once", "name": "Allow once"},
                {"kind": "allow_always", "optionId": "yes-always", "name": "Allow always"},
            ]
        });
        let outcome = pick_permission_outcome(&params);
        assert_eq!(outcome["outcome"], "selected");
        assert_eq!(outcome["optionId"], "yes-always");
    }

    #[test]
    fn permission_outcome_falls_back_to_allow_once() {
        let params = json!({
            "options": [
                {"kind": "reject_once", "optionId": "no"},
                {"kind": "allow_once", "optionId": "yes-once"},
            ]
        });
        let outcome = pick_permission_outcome(&params);
        assert_eq!(outcome["optionId"], "yes-once");
    }

    #[test]
    fn permission_outcome_cancels_when_no_allow_option() {
        let params = json!({
            "options": [
                {"kind": "reject_once", "optionId": "no"},
                {"kind": "reject_always", "optionId": "no-always"},
            ]
        });
        let outcome = pick_permission_outcome(&params);
        assert_eq!(outcome["outcome"], "cancelled");
    }

    #[test]
    fn permission_outcome_handles_missing_options_field() {
        let outcome = pick_permission_outcome(&json!({}));
        assert_eq!(outcome["outcome"], "cancelled");
    }

    #[test]
    fn collect_content_text_handles_array_and_object() {
        let mut buf = String::new();
        let v = json!([
            {"type":"text","text":"hello "},
            {"type":"text","text":"world"},
            {"type":"image","data":"…"}
        ]);
        collect_content_text(&v, &mut buf);
        assert_eq!(buf, "hello world");
    }

    #[tokio::test]
    async fn drain_pending_delivers_transport_to_waiters() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        pending.lock().await.insert(1, tx1);
        pending.lock().await.insert(2, tx2);

        drain_pending(&pending, &dead, "server died: read-only fs").await;

        for rx in [rx1, rx2] {
            match rx.await.unwrap() {
                RpcResult::Transport(m) => assert!(m.contains("read-only fs"), "got: {m}"),
                other => panic!("expected transport, got {other:?}"),
            }
        }
        assert!(dead.load(Ordering::SeqCst));
        assert!(pending.lock().await.is_empty());
    }

    #[test]
    fn diagnostic_buf_renders_recent_lines() {
        let mut d = DiagnosticBuf::default();
        d.push_bad_stdout("log setup: read-only file system".into());
        d.push_stderr("poolside: bailing".into());
        let r = d.render();
        assert!(r.contains("recent non-JSON stdout"));
        assert!(r.contains("read-only file system"));
        assert!(r.contains("recent stderr"));
        assert!(r.contains("poolside: bailing"));
    }

    #[test]
    fn classify_recognizes_known_kinds() {
        let v =
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}});
        match classify(&v).unwrap() {
            UpdateKind::AgentMessage {
                text,
                is_thought: false,
            } => assert_eq!(text, "hi"),
            other => panic!("expected AgentMessage, got {other:?}"),
        }

        let v =
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}});
        match classify(&v).unwrap() {
            UpdateKind::AgentMessage {
                text,
                is_thought: true,
            } => assert_eq!(text, "hmm"),
            other => panic!("expected thought, got {other:?}"),
        }

        let v = json!({
            "sessionUpdate":"tool_call",
            "toolCallId":"abcdef0123",
            "title":"Reading src/foo.rs",
            "kind":"read"
        });
        match classify(&v).unwrap() {
            UpdateKind::ToolCall { id, title, kind } => {
                assert_eq!(id, "abcdef0123");
                assert_eq!(title, "Reading src/foo.rs");
                assert_eq!(kind, "read");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }

        let v = json!({
            "sessionUpdate":"tool_call_update",
            "toolCallId":"abcdef0123",
            "status":"completed"
        });
        match classify(&v).unwrap() {
            UpdateKind::ToolCallUpdate { id, status, .. } => {
                assert_eq!(id, "abcdef0123");
                assert_eq!(status, "completed");
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }

        let v = json!({"sessionUpdate":"plan","entries":[1,2,3]});
        match classify(&v).unwrap() {
            UpdateKind::Plan { entry_count } => assert_eq!(entry_count, 3),
            other => panic!("expected Plan, got {other:?}"),
        }

        let v = json!({"sessionUpdate":"current_mode_update","mode":"edit"});
        match classify(&v).unwrap() {
            UpdateKind::Other { kind } => assert_eq!(kind, "current_mode_update"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn classify_returns_none_without_session_update_field() {
        let v = json!({"foo":"bar"});
        assert!(classify(&v).is_none());
    }

    #[test]
    fn summary_describes_each_kind() {
        assert_eq!(
            summary(&UpdateKind::AgentMessage {
                text: "x".into(),
                is_thought: true
            }),
            "thinking"
        );
        assert_eq!(
            summary(&UpdateKind::AgentMessage {
                text: "x".into(),
                is_thought: false
            }),
            "writing message"
        );
        let s = summary(&UpdateKind::ToolCall {
            id: "abc".into(),
            title: "Editing src/foo.rs".into(),
            kind: "edit".into(),
        });
        assert!(s.starts_with("tool/edit"), "got {s}");
        assert!(s.contains("Editing src/foo.rs"));

        let s = summary(&UpdateKind::ToolCallUpdate {
            id: "abcdef0123".into(),
            status: "completed".into(),
            title: String::new(),
        });
        assert!(s.contains("abcdef01"));
        assert!(s.ends_with("→ completed"));

        assert_eq!(
            summary(&UpdateKind::Plan { entry_count: 2 }),
            "plan (2 entries)"
        );
        assert_eq!(
            summary(&UpdateKind::Other {
                kind: "current_mode_update".into()
            }),
            "kind=current_mode_update"
        );
    }

    #[test]
    fn first_line_truncates_with_ellipsis() {
        assert_eq!(first_line("short", 10), "short");
        assert_eq!(first_line("first\nsecond", 20), "first");
        let long = "x".repeat(50);
        let got = first_line(&long, 10);
        assert!(got.ends_with('…'), "got {got}");
        assert_eq!(got.chars().count(), 11);
    }

    #[test]
    fn id_short_truncates_and_handles_empty() {
        assert_eq!(id_short(""), "?");
        assert_eq!(id_short("a"), "a");
        assert_eq!(id_short("0123456789abcdef"), "01234567");
    }

    #[test]
    fn observe_update_tracks_last_activity_and_collects_text() {
        let mut sink = String::new();
        let mut state = ChunkLogState::default();
        let last = Arc::new(StdMutex::new((Instant::now(), "starting".to_string())));
        let v = json!({
            "sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":"hello "}
        });
        observe_update(&v, &mut sink, &mut state, &last, false, false);
        let v2 = json!({
            "sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":"world"}
        });
        observe_update(&v2, &mut sink, &mut state, &last, false, false);

        assert_eq!(sink, "hello world");
        assert_eq!(state.message_chars, 11);
        let label = last.lock().unwrap().1.clone();
        assert_eq!(label, "writing message");
    }

    #[test]
    fn diagnostic_buf_caps_at_max() {
        let mut d = DiagnosticBuf::default();
        for i in 0..(DiagnosticBuf::MAX + 5) {
            d.push_bad_stdout(format!("line-{i}"));
        }
        assert_eq!(d.bad_stdout.len(), DiagnosticBuf::MAX);
        assert_eq!(d.bad_stdout.front().unwrap(), "line-5");
    }

    #[test]
    fn acp_usage_extracted_from_snake_case_root() {
        let result = json!({
            "stopReason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 3
            }
        });
        let u = extract_acp_usage(&result).unwrap();
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 20);
        assert_eq!(u.cache_creation_input_tokens, 5);
        assert_eq!(u.cache_read_input_tokens, 3);
    }

    #[test]
    fn acp_usage_extracted_from_meta_with_nested_cache() {
        // camelCase under a `_meta` envelope, opencode-style nested cache.
        let result = json!({
            "_meta": {
                "tokens": {
                    "input": 200,
                    "output": 40,
                    "cache": { "write": 7, "read": 9 }
                }
            }
        });
        let u = extract_acp_usage(&result).unwrap();
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.output_tokens, 40);
        assert_eq!(u.cache_creation_input_tokens, 7);
        assert_eq!(u.cache_read_input_tokens, 9);
    }

    #[test]
    fn acp_usage_absent_returns_none() {
        assert!(extract_acp_usage(&json!({ "stopReason": "end_turn" })).is_none());
        // An all-zero usage object is treated as absent.
        assert!(extract_acp_usage(&json!({ "usage": { "input_tokens": 0 } })).is_none());
    }
}
