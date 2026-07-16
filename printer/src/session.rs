use crate::agent::acp::AcpClient;
use crate::agent::{AgentInvocation, TokenUsage, ToolUseEvent, TurnOutcome};
use crate::cli::AgentKind;
use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

pub struct Session<'a> {
    pub id: Uuid,
    pub turn_count: u32,
    /// Largest single-turn *non-cached* input-side token total seen in the
    /// *current* (not yet rotated) session — freshly processed input plus
    /// newly-written cache, excluding cache reads. Used by the compaction
    /// trigger only; cache reads are intentionally ignored so a warm prompt
    /// cache isn't thrown away by premature rotation.
    pub cumulative_input_tokens: u64,
    /// Sum of every turn's token usage for the lifetime of this Session,
    /// including across rotations. This is the operation-level total.
    pub usage_total: TokenUsage,
    pub agent: AgentInvocation<'a>,
    pub verbose: bool,
    /// When set, each turn whose tool activity is observable appends a `turn`
    /// record to `.printer/metrics.jsonl` with the codegraph search-tool
    /// histogram. Left `None` for phases (test/exec-total) that don't track it.
    metrics_ctx: Option<MetricsCtx>,
    fresh: bool,
    backend_session_id: Option<String>,
    acp: Option<AcpClient>,
    acp_session_id: Option<String>,
}

/// Attribution for per-turn metrics rows written by `Session`.
pub struct MetricsCtx {
    pub cwd: std::path::PathBuf,
    pub spec: String,
    pub agent: String,
    pub model: Option<String>,
}

impl<'a> Session<'a> {
    pub fn new(agent: AgentInvocation<'a>) -> Self {
        Self {
            id: Uuid::new_v4(),
            turn_count: 0,
            cumulative_input_tokens: 0,
            usage_total: TokenUsage::default(),
            agent,
            verbose: false,
            metrics_ctx: None,
            fresh: true,
            backend_session_id: None,
            acp: None,
            acp_session_id: None,
        }
    }

    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Enable per-turn metrics rows (codegraph search-tool histogram) attributed
    /// to `ctx`. No-op for turns with no observable tool activity.
    pub fn with_metrics_context(mut self, ctx: MetricsCtx) -> Self {
        self.metrics_ctx = Some(ctx);
        self
    }

    /// Mint a new session id; the next turn will bootstrap a fresh session.
    /// Used for compaction-by-rotation.
    pub async fn rotate(&mut self) {
        self.id = Uuid::new_v4();
        self.fresh = true;
        self.cumulative_input_tokens = 0;
        self.backend_session_id = None;
        // For ACP, shut down the existing client so the previous server's
        // process group is reaped before we drop it; otherwise the watcher
        // task would only fire when the parent process exits. Then re-null
        // so the next turn spawns a fresh server and starts a new
        // `session/new`. Mirrors what one-shot backends do implicitly by
        // spawning a new process per turn.
        if let Some(client) = self.acp.as_ref() {
            client.shutdown().await;
        }
        self.acp = None;
        self.acp_session_id = None;
    }

    pub async fn turn(&mut self, prompt: &str) -> Result<TurnOutcome> {
        if matches!(self.agent.kind, AgentKind::Acp { .. }) {
            return self.turn_acp(prompt).await;
        }
        let mut cmd = if self.fresh {
            self.agent.bootstrap(&self.id, prompt)
        } else {
            let fallback = self.id.to_string();
            let session_id = self.backend_session_id.as_deref().unwrap_or(&fallback);
            self.agent.resume(session_id, prompt)
        };
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(if self.agent.reads_prompt_from_stdin() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        // Put the child into its own process group so we can kill the whole
        // subtree on Ctrl-C — otherwise orphaned grandchildren can keep pipes
        // open and stall our cleanup readers indefinitely.
        #[cfg(unix)]
        cmd.process_group(0);

        let started = Instant::now();
        eprintln!(
            "[printer] turn {} starting (session {}{})",
            self.turn_count + 1,
            short(&self.id),
            if self.fresh { ", fresh" } else { ", resumed" }
        );

        let mut child = cmd.spawn().context("failed to spawn agent process")?;
        if self.agent.reads_prompt_from_stdin() {
            let mut stdin = child.stdin.take().context("child has no stdin pipe")?;
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("failed to write prompt to agent stdin")?;
            stdin
                .shutdown()
                .await
                .context("failed to close agent stdin")?;
        }

        let stop = Arc::new(AtomicBool::new(false));
        // Activity tracking for the heartbeat: last stderr line timestamp
        // (so we can show "last [agent] line Ns ago" instead of binary
        // active/waiting), and a running stdout byte counter (so even an
        // agent that's silent on stderr but streaming a result body still
        // shows progress).
        let last_stderr: Arc<StdMutex<Option<Instant>>> = Arc::new(StdMutex::new(None));
        let stdout_bytes_total = Arc::new(AtomicU64::new(0));

        // Stream stderr from the child to our own stderr in real time so the
        // user can watch progress. Keep a bounded tail so a failing child's
        // diagnostics can be included in the error (its stdout is often empty).
        const STDERR_TAIL_LINES: usize = 40;
        let stderr_tail: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let child_stderr = child.stderr.take();
        let verbose = self.verbose;
        let tty = std::io::stderr().is_terminal();
        let last_stderr_clone = last_stderr.clone();
        let stderr_tail_clone = stderr_tail.clone();
        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = child_stderr {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Ok(mut g) = last_stderr_clone.lock() {
                        *g = Some(Instant::now());
                    }
                    if let Ok(mut g) = stderr_tail_clone.lock() {
                        if g.len() == STDERR_TAIL_LINES {
                            g.remove(0);
                        }
                        g.push(line.clone());
                    }
                    if verbose && tty {
                        // Clear any spinner line, then print the agent line.
                        eprint!("\r\x1b[2K");
                    }
                    eprintln!("[agent] {line}");
                }
            }
        });

        // Optional spinner / heartbeat task.
        let spinner_task = if self.verbose {
            let stop = stop.clone();
            let last_stderr = last_stderr.clone();
            let bytes = stdout_bytes_total.clone();
            Some(tokio::spawn(async move {
                heartbeat_loop(stop, last_stderr, bytes, started, tty).await;
            }))
        } else {
            None
        };

        // Read stdout in chunks concurrently with watching for Ctrl-C and the
        // child's exit. read_to_string would block until the child closes its
        // stdout pipe (i.e. exits), starving the ctrl_c branch — so we drive
        // the read inside the select loop instead.
        let mut stdout = child.stdout.take().context("child has no stdout pipe")?;
        let mut stdout_bytes: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut read_done = false;

        let status = loop {
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    if verbose && tty { eprint!("\r\x1b[2K"); }
                    eprintln!("[printer] interrupt received; killing child agent");
                    stop.store(true, Ordering::Relaxed);
                    kill_subtree(&mut child);
                    let _ = child.wait().await;
                    // Don't await the helper tasks — orphaned grandchildren
                    // can keep their stdio pipes open after the immediate
                    // child dies, which would block stderr_task forever.
                    // Aborting is non-blocking and the runtime will reap them.
                    if let Some(t) = spinner_task { t.abort(); }
                    stderr_task.abort();
                    anyhow::bail!("interrupted by user");
                }
                read = stdout.read(&mut chunk), if !read_done => {
                    match read? {
                        0 => { read_done = true; }
                        n => {
                            stdout_bytes.extend_from_slice(&chunk[..n]);
                            stdout_bytes_total.fetch_add(n as u64, Ordering::Relaxed);
                        }
                    }
                }
                wait = child.wait(), if read_done => {
                    break wait?;
                }
            }
        };

        stop.store(true, Ordering::Relaxed);
        if let Some(t) = spinner_task {
            let _ = t.await;
            if tty && self.verbose {
                eprint!("\r\x1b[2K");
            }
        }
        let _ = stderr_task.await;

        let stdout_buf = String::from_utf8_lossy(&stdout_bytes).into_owned();

        if !status.success() {
            let stderr_buf = stderr_tail.lock().map(|g| g.join("\n")).unwrap_or_default();
            anyhow::bail!(
                "agent exited with status {status}\n--- stdout ---\n{stdout_buf}\n\
                 --- stderr (last {STDERR_TAIL_LINES} lines) ---\n{stderr_buf}"
            );
        }

        let outcome = self.agent.parse_outcome(stdout_buf, &self.id)?;
        if let Some(id) = outcome.backend_session_id.clone() {
            self.backend_session_id = Some(id);
        }
        self.turn_count += 1;
        self.cumulative_input_tokens = outcome
            .non_cached_input_tokens()
            .max(self.cumulative_input_tokens);
        self.usage_total.add(&outcome.usage);
        eprintln!(
            "[printer] turn {} done in {:.1}s (turn: {}; op total: {})",
            self.turn_count,
            started.elapsed().as_secs_f32(),
            outcome.usage,
            self.usage_total,
        );
        // Search-tool histogram for the codegraph guardrail + metrics. Tool
        // activity is only observable on verbose (stream-json) claude turns;
        // other backends leave `tools` empty so both counts are 0.
        let raw_search = count_inefficient_search_tools(&outcome.tools);
        let codegraph = count_codegraph_search_tools(&outcome.tools);
        // Surface per-tool activity when running verbose.
        if self.verbose && !outcome.tools.is_empty() {
            for t in &outcome.tools {
                if t.input_summary.is_empty() {
                    eprintln!("[agent] ⚙ {}", t.name);
                } else {
                    eprintln!("[agent] ⚙ {}({})", t.name, t.input_summary);
                }
            }
            eprintln!("[printer] tools: {}", format_tool_summary(&outcome.tools));
            if raw_search > 0 && codegraph == 0 {
                // The agent did raw search and never touched codegraph — the
                // regression the codegraph wiring exists to prevent. Escalate.
                eprintln!(
                    "[printer] ⚠ {raw_search} raw search call(s), 0 codegraph this turn — \
                     agent bypassed codegraph (mcp__codegraph__* / search / definition / references)."
                );
            } else if raw_search > 0 {
                eprintln!(
                    "[printer] hint: {raw_search} raw grep/find/cat call(s) this turn — \
                     prefer `codegraph` (search/definition/references) for token-efficient search."
                );
            }
        }
        // Per-turn metrics row when attribution is configured and there was
        // observable tool activity (verbose claude turns only).
        if let Some(ctx) = &self.metrics_ctx
            && !outcome.tools.is_empty()
        {
            crate::metrics::record_turn(
                &ctx.cwd,
                &ctx.spec,
                ctx.agent.clone(),
                ctx.model.clone(),
                outcome.usage,
                raw_search as u64,
                codegraph as u64,
            );
        }
        // Subsequent turns resume the same session id (claude requires a new
        // uuid for --session-id, so we keep using --resume from now on).
        self.fresh = false;
        Ok(outcome)
    }

    /// ACP-backed turn. Lazily spawns the long-lived server on the first call
    /// (and again after `rotate()`), then sends one blocking `session/prompt`.
    /// Token usage is not surfaced (T-020 follow-up).
    async fn turn_acp(&mut self, prompt: &str) -> Result<TurnOutcome> {
        let started = Instant::now();
        eprintln!(
            "[printer] turn {} starting (acp session {}{})",
            self.turn_count + 1,
            short(&self.id),
            if self.fresh { ", fresh" } else { ", resumed" }
        );

        if self.acp.is_none() {
            let bin = self.agent.acp_bin.ok_or_else(|| {
                anyhow::anyhow!(
                    "--agent acp requires --acp-bin <command> (or pick a plugin-contributed agent via --agent acp:<name>)"
                )
            })?;

            // Permission-mode is Claude-CLI shaped (`--permission-mode <mode>`).
            // ACP doesn't standardize a permission policy — the server
            // enforces its own. Surface the value as an env var hint
            // (`PRINTER_PERMISSION_MODE`) and mention the limitation once,
            // so users running `--agent acp:* --permission-mode acceptEdits`
            // don't assume printer is overriding the server.
            eprintln!(
                "[printer] acp: --permission-mode is advisory; the ACP server enforces its own policy (mode={})",
                self.agent.permission_mode
            );
            let mut child_env = self.agent.acp_env.clone();
            child_env.insert(
                "PRINTER_PERMISSION_MODE".to_string(),
                self.agent.permission_mode.to_string(),
            );

            // Per-step progress: bootstrap can stall in spawn (slow image
            // pull, sandbox init), initialize (ACP child blocked on the
            // protocol handshake), or session/new (server-side workdir
            // setup — the poolside EROFS regression manifested here). Each
            // step prints both a "starting" and "ok in Ns" line so the
            // exact hung phase is obvious from the log.
            let argv_label = format_argv(bin, self.agent.acp_args);
            let t = Instant::now();
            eprintln!("[printer] acp: spawning server `{argv_label}`");
            let client = AcpClient::spawn(
                bin,
                self.agent.acp_args,
                self.agent.cwd,
                self.agent.command_wrapper,
                &child_env,
            )
            .await
            .context("spawning ACP server")?;
            eprintln!(
                "[printer] acp: server spawned ({:.1}s); sending initialize",
                t.elapsed().as_secs_f32()
            );

            let t = Instant::now();
            with_progress("initialize", self.verbose, client.initialize())
                .await
                .context("ACP initialize failed")?;
            eprintln!(
                "[printer] acp: initialize ok ({:.1}s); sending session/new",
                t.elapsed().as_secs_f32()
            );

            // The cwd we ship to `session/new` is the path the *agent*
            // sees from inside its execution context, not the host path.
            // When a sandbox driver is active (`command_wrapper` Some),
            // the agent runs inside the sandbox where the workspace is
            // bind-mounted at `/workspace` (the host path is visible only
            // through a sibling RO bind, so writes against it EROFS — and
            // poolside writes a `.poolside/` config dir under cwd on
            // first use, which is exactly how that surfaces). For
            // non-sandboxed runs, use the host cwd we were configured
            // with (or the printer process's current dir).
            let cwd = if self.agent.command_wrapper.is_some() {
                std::path::PathBuf::from("/workspace")
            } else {
                self.agent
                    .cwd
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            };
            let t = Instant::now();
            let session_id = with_progress(
                "session/new",
                self.verbose,
                client.session_new(&cwd, Some(self.agent.permission_mode)),
            )
            .await
            .context("ACP session/new failed")?;
            eprintln!(
                "[printer] acp: session/new ok in {:.1}s (id={session_id})",
                t.elapsed().as_secs_f32()
            );
            self.acp = Some(client);
            self.acp_session_id = Some(session_id);
        }

        let session_id = self.acp_session_id.clone().expect("acp session id set");
        // Inner scope so `prompt_fut` (which borrows `client`, which borrows
        // `self.acp`) is dropped before we mutate `self.acp` on the
        // interrupt path. Without the scope, the borrow checker rejects
        // `self.acp = None` inside the select arm.
        let result: Result<TurnOutcome> = {
            let client = self.acp.as_ref().expect("acp client set");
            let prompt_fut = client.prompt_blocking(&session_id, prompt, self.verbose);
            tokio::pin!(prompt_fut);
            tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("[printer] interrupt received; cancelling acp turn");
                    // Fire-and-forget session/cancel so a well-behaved server
                    // resolves the in-flight prompt with stopReason=cancelled
                    // instead of being killed mid-write.
                    let _ = client.cancel().await;
                    // Give the server up to 500ms to land that resolution;
                    // if it doesn't, fall through to SIGKILL on the process
                    // group via shutdown().
                    let drained = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        &mut prompt_fut,
                    )
                    .await;
                    client.shutdown().await;
                    match drained {
                        Ok(_) => Err(anyhow::anyhow!("interrupted by user")),
                        Err(_) => Err(anyhow::anyhow!(
                            "interrupted by user (acp server did not honor session/cancel within 500ms; killed)"
                        )),
                    }
                }
                res = &mut prompt_fut => res,
            }
        };
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                self.acp = None;
                self.acp_session_id = None;
                return Err(e);
            }
        };

        self.turn_count += 1;
        self.usage_total.add(&outcome.usage);
        eprintln!(
            "[printer] turn {} done in {:.1}s (acp; usage not surfaced — see T-020)",
            self.turn_count,
            started.elapsed().as_secs_f32(),
        );
        self.fresh = false;
        Ok(outcome)
    }
}

async fn heartbeat_loop(
    stop: Arc<AtomicBool>,
    last_stderr: Arc<StdMutex<Option<Instant>>>,
    stdout_bytes: Arc<AtomicU64>,
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
        let bytes = stdout_bytes.load(Ordering::Relaxed);
        // Activity label: prefer "last [agent] line Ns ago" when stderr has
        // ever fired, since that's the most user-meaningful signal. Fall
        // back to a stdout-bytes counter when stderr is silent (e.g.
        // claude --print without --verbose only writes the result blob to
        // stdout). If neither has fired yet, say so explicitly so the user
        // knows the agent has produced literally nothing — likely still
        // bootstrapping or thinking before tool use.
        let activity = match last_stderr.lock().ok().and_then(|g| *g) {
            Some(t) => format!("last [agent] line {}s ago", t.elapsed().as_secs()),
            None if bytes > 0 => format!("{bytes} stdout bytes received, no [agent] lines yet"),
            None => "no output yet".to_string(),
        };

        if tty {
            eprint!(
                "\r\x1b[2K[printer] {} working… {}s ({})",
                FRAMES[i % FRAMES.len()],
                elapsed,
                activity
            );
            i = i.wrapping_add(1);
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        } else {
            // Non-TTY: emit a textual heartbeat every ~10s, no animation.
            if last_text_heartbeat.elapsed().as_secs() >= 10 {
                eprintln!("[printer] still working… {elapsed}s ({activity})");
                last_text_heartbeat = Instant::now();
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}

fn short(id: &Uuid) -> String {
    let s = id.to_string();
    s.chars().take(8).collect()
}

/// Wrap an in-flight async operation so the user can see it's still running.
/// While the future is pending and `verbose` is true, prints a one-line
/// "still waiting for <label>… <N>s" every 5s. Returns the future's output
/// unchanged. No-op in non-verbose mode (just awaits the future). Used to
/// give visibility into ACP bootstrap RPCs (`initialize`, `session/new`)
/// that otherwise go silent for as long as the server takes to answer.
async fn with_progress<F, T>(label: &str, verbose: bool, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    if !verbose {
        return fut.await;
    }
    tokio::pin!(fut);
    let started = Instant::now();
    let mut hint_emitted = false;
    loop {
        tokio::select! {
            biased;
            v = &mut fut => return v,
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                let elapsed = started.elapsed().as_secs();
                eprintln!(
                    "[printer] acp: still waiting for {label}… {elapsed}s"
                );
                if !hint_emitted && elapsed >= 30 {
                    hint_emitted = true;
                    eprintln!(
                        "[printer] acp: {label} is taking >30s. Common causes: \
                         (a) the ACP child can't authenticate (missing API key, \
                         expired creds, or auth files unreachable in the sandbox); \
                         (b) the child is writing to a path that's read-only inside \
                         the sandbox and silently retrying (check the sandbox's bind \
                         mounts vs. the agent's data dir); \
                         (c) heyvm older than v0.27.2 (its `heyvm exec` buffered child \
                         stdout instead of streaming, deadlocking any persistent \
                         ACP server — `heyvm --version` will tell you, run \
                         `heyvm --upgrade` if needed). \
                         Set PRINTER_ACP_TRACE=1 for byte-level transport traces, or \
                         retry with `--no-sandbox` to isolate sandbox vs. agent."
                    );
                }
            }
        }
    }
}

/// Render an ACP launch argv as a single human-readable token sequence for
/// progress logs. Truncates to keep the line short.
fn format_argv(bin: &str, args: &[String]) -> String {
    let mut s = bin.to_string();
    for a in args {
        s.push(' ');
        s.push_str(a);
    }
    if s.chars().count() > 80 {
        let head: String = s.chars().take(80).collect();
        format!("{head}…")
    } else {
        s
    }
}

/// Kill the child plus any descendants. We set process_group(0) when spawning,
/// so on Unix the child's pid is also its pgid and `kill(-pgid, SIGKILL)`
/// signals the whole subtree.
fn kill_subtree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: kill() with a negative pid signals a process group; an
            // invalid pgid simply returns ESRCH. No memory is touched.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    // Fallback / non-unix: at minimum kill the immediate child.
    let _ = child.start_kill();
}

/// Aggregate tool-use events into a compact one-line summary like
/// `Read x3, Bash x2, Edit x1`, ordered by descending count then name.
fn format_tool_summary(tools: &[ToolUseEvent]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for t in tools {
        match counts.iter_mut().find(|(n, _)| *n == t.name) {
            Some((_, c)) => *c += 1,
            None => counts.push((t.name.clone(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
        .into_iter()
        .map(|(name, c)| format!("{name} x{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Count tool calls that do raw text search / file dumps the codegraph CLI
/// could answer more cheaply — the `Grep`/`Glob` built-ins and `Bash`
/// invocations whose leading program is grep/rg/find/cat/ag/ack. Drives the
/// verbose codegraph-first nudge (Spec 010, task 2). Advisory only.
fn count_inefficient_search_tools(tools: &[ToolUseEvent]) -> usize {
    tools
        .iter()
        .filter(|t| match t.name.as_str() {
            "Grep" | "Glob" => true,
            "Bash" => {
                let first = t.input_summary.split_whitespace().next().unwrap_or("");
                matches!(first, "grep" | "rg" | "find" | "cat" | "ag" | "ack")
            }
            _ => false,
        })
        .count()
}

/// Count codegraph tool calls: the native MCP tools (`mcp__codegraph__*`) and
/// `Bash` invocations whose leading program is `codegraph`. Paired with
/// `count_inefficient_search_tools` to detect a turn that searched the raw way
/// and never used codegraph (the guardrail warning).
fn count_codegraph_search_tools(tools: &[ToolUseEvent]) -> usize {
    tools
        .iter()
        .filter(|t| {
            t.name.starts_with("mcp__codegraph__")
                || (t.name == "Bash"
                    && t.input_summary.split_whitespace().next() == Some("codegraph"))
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str) -> ToolUseEvent {
        ToolUseEvent {
            name: name.to_string(),
            input_summary: String::new(),
        }
    }

    #[test]
    fn tool_summary_aggregates_and_orders_by_count_then_name() {
        let tools = vec![
            ev("Read"),
            ev("Bash"),
            ev("Read"),
            ev("Edit"),
            ev("Read"),
            ev("Bash"),
        ];
        assert_eq!(format_tool_summary(&tools), "Read x3, Bash x2, Edit x1");
    }

    #[test]
    fn tool_summary_empty_is_blank() {
        assert_eq!(format_tool_summary(&[]), "");
    }

    fn ev_cmd(name: &str, cmd: &str) -> ToolUseEvent {
        ToolUseEvent {
            name: name.to_string(),
            input_summary: cmd.to_string(),
        }
    }

    #[test]
    fn flags_raw_search_tools_and_ignores_codegraph() {
        let tools = vec![
            ev("Grep"),                         // built-in search
            ev("Glob"),                         // built-in file find
            ev_cmd("Bash", "grep -rn foo src"), // raw grep
            ev_cmd("Bash", "find . -name '*.rs'"),
            ev_cmd("Bash", "codegraph search foo"), // efficient — not flagged
            ev_cmd("Bash", "cargo test"),           // unrelated — not flagged
            ev("Read"),                             // reads aren't counted here
        ];
        assert_eq!(count_inefficient_search_tools(&tools), 4);
        assert_eq!(count_inefficient_search_tools(&[]), 0);
    }

    #[test]
    fn counts_codegraph_tools_native_and_bash() {
        let tools = vec![
            ev("mcp__codegraph__search"),             // native MCP tool
            ev("mcp__codegraph__definition"),         // native MCP tool
            ev_cmd("Bash", "codegraph outline x.rs"), // codegraph via Bash
            ev_cmd("Bash", "grep -rn foo"),           // raw — not codegraph
            ev("Grep"),                               // raw — not codegraph
            ev("Read"),                               // unrelated
        ];
        assert_eq!(count_codegraph_search_tools(&tools), 3);
        assert_eq!(count_codegraph_search_tools(&[]), 0);
    }

    #[test]
    fn guardrail_distinguishes_bypass_from_mixed_use() {
        // Raw search with zero codegraph → the escalation condition.
        let bypassed = vec![ev("Grep"), ev_cmd("Bash", "find . -name '*.rs'")];
        assert!(
            count_inefficient_search_tools(&bypassed) > 0
                && count_codegraph_search_tools(&bypassed) == 0
        );
        // Raw search alongside codegraph → only the soft hint, no escalation.
        let mixed = vec![ev("Grep"), ev("mcp__codegraph__search")];
        assert!(
            count_inefficient_search_tools(&mixed) > 0 && count_codegraph_search_tools(&mixed) > 0
        );
    }
}
