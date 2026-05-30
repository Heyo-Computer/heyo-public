pub mod acp;

use crate::cli::AgentKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use tokio::process::Command;
use uuid::Uuid;

/// Per-turn token breakdown, normalized across agents.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    /// Sum of all input-side tokens (new input + cache creation + cache read).
    pub fn input_total(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }

    /// Input-side tokens that were *not* served from the prompt cache: freshly
    /// processed input plus newly-written cache. Cache reads are excluded
    /// because they are the cheap, already-amortized part of context — counting
    /// them toward rotation would discard a warm cache and force an expensive
    /// re-creation, the opposite of token-efficient. This is the signal the
    /// compaction trigger uses (see `Session::cumulative_input_tokens`).
    pub fn non_cached_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens
    }

    /// Grand total: input-side + output.
    pub fn grand_total(&self) -> u64 {
        self.input_total() + self.output_tokens
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }
}

impl fmt::Display for TokenUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} total (input: {} new + {} cache-write + {} cache-read; output: {})",
            self.grand_total(),
            self.input_tokens,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
            self.output_tokens,
        )
    }
}

/// A single tool invocation surfaced by an agent during a turn.
/// Captured from `claude --output-format stream-json`; empty for backends
/// that don't expose per-tool activity.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolUseEvent {
    pub name: String,
    /// Compact, human-readable summary of the tool input (best-effort).
    pub input_summary: String,
}

/// Outcome of a single turn, normalized across agents.
#[derive(Debug, Default, Clone)]
pub struct TurnOutcome {
    pub result_text: String,
    pub usage: TokenUsage,
    /// Tool calls the agent made this turn, in order. Empty unless a
    /// streaming output format was parsed (see `parse_claude_stream`).
    /// Consumed by the verbose reporting layer in `session.rs`.
    pub tools: Vec<ToolUseEvent>,
}

impl TurnOutcome {
    /// Non-cached input total — the signal the compaction trigger watches.
    pub fn non_cached_input_tokens(&self) -> u64 {
        self.usage.non_cached_input_tokens()
    }
}

/// Parsed shape of `claude --print --output-format json`.
#[derive(Debug, Deserialize)]
struct ClaudeJsonResult {
    #[serde(default)]
    result: String,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

pub struct AgentInvocation<'a> {
    pub kind: AgentKind,
    pub model: Option<&'a str>,
    pub cwd: Option<&'a Path>,
    pub permission_mode: &'a str,
    /// If set, the constructed agent command is shell-quoted and substituted
    /// for `{child}` in this template, then run via `sh -c`. Used by the
    /// sandbox driver to dispatch the agent inside a VM (see `drivers.rs`).
    pub command_wrapper: Option<&'a str>,
    /// Mirror of the user-facing `-v` flag. When set, the claude backend
    /// switches to `--output-format stream-json --verbose` so per-tool
    /// activity can be captured (see `claude_argv` / `parse_claude_stream`).
    /// ACP transports use the `Session.verbose` direct path instead.
    pub verbose: bool,
    /// Launch command for an ACP agent server. Required when `kind` is
    /// `AgentKind::Acp`. Ignored for one-shot backends. When the user picks
    /// a plugin-contributed agent (`--agent acp:<name>`) the call site
    /// resolves the name to a command via `crate::agents::AgentSet` before
    /// constructing this struct, so the ACP transport stays plugin-agnostic.
    pub acp_bin: Option<&'a str>,
    /// Extra args appended to the ACP server invocation.
    pub acp_args: &'a [String],
    /// Environment to pass to the spawned ACP server child. Empty for the
    /// one-shot backends.
    pub acp_env: &'a BTreeMap<String, String>,
}

impl<'a> AgentInvocation<'a> {
    /// Build a fresh-session command (sets the session id).
    pub fn bootstrap(&self, session_id: &Uuid, prompt: &str) -> Command {
        let argv = match &self.kind {
            AgentKind::Claude => self.claude_argv(Some(session_id), None, prompt),
            AgentKind::Opencode => self.opencode_argv(Some(session_id), false, prompt),
            AgentKind::Acp { .. } => panic!(
                "AgentInvocation::bootstrap() must not be called for ACP agents; use the ACP path in Session::turn"
            ),
        };
        self.build_command(argv)
    }

    /// Build a resume-session command.
    pub fn resume(&self, session_id: &Uuid, prompt: &str) -> Command {
        let argv = match &self.kind {
            AgentKind::Claude => self.claude_argv(None, Some(session_id), prompt),
            AgentKind::Opencode => self.opencode_argv(Some(session_id), true, prompt),
            AgentKind::Acp { .. } => panic!(
                "AgentInvocation::resume() must not be called for ACP agents; use the ACP path in Session::turn"
            ),
        };
        self.build_command(argv)
    }

    fn build_command(&self, argv: Vec<String>) -> Command {
        if let Some(template) = self.command_wrapper {
            let quoted = crate::drivers::shell_quote_argv(&argv);
            let resolved = template.replace("{child}", &quoted);
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&resolved);
            if let Some(cwd) = self.cwd {
                cmd.current_dir(cwd);
            }
            cmd
        } else {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            if let Some(cwd) = self.cwd {
                cmd.current_dir(cwd);
            }
            cmd
        }
    }

    fn claude_argv(&self, session_id: Option<&Uuid>, resume: Option<&Uuid>, prompt: &str) -> Vec<String> {
        let mut v: Vec<String> = vec!["claude".into(), "--print".into(), "--output-format".into()];
        // `claude --output-format json` returns a single result object that
        // `parse_claude` reads. When the user passes `-v` we instead request
        // `stream-json` (which *requires* `--verbose`): stdout becomes
        // newline-delimited events (system init, assistant messages carrying
        // `tool_use` blocks, then a final `result` with usage), parsed by
        // `parse_claude_stream` to surface per-tool activity. The buffered
        // NDJSON is parsed once at the end of the turn; the session-level
        // heartbeat still surfaces liveness in both modes.
        if self.verbose {
            v.push("stream-json".into());
            v.push("--verbose".into());
        } else {
            v.push("json".into());
        }
        v.push("--permission-mode".into());
        v.push(self.permission_mode.to_string());
        if let Some(id) = session_id {
            v.push("--session-id".into());
            v.push(id.to_string());
        }
        if let Some(id) = resume {
            v.push("--resume".into());
            v.push(id.to_string());
        }
        if let Some(model) = self.model {
            v.push("--model".into());
            v.push(model.to_string());
        }
        v.push(prompt.to_string());
        v
    }

    fn opencode_argv(&self, session_id: Option<&Uuid>, resume: bool, prompt: &str) -> Vec<String> {
        let mut v: Vec<String> = vec![
            "opencode".into(),
            "run".into(),
            prompt.to_string(),
        ];
        // `--format json` switches stdout to a newline-delimited event stream
        // (step_start / text / step_finish …). `parse_opencode` reconstructs
        // the assistant text from `text` events and reads token usage from the
        // `step_finish` event's `part.tokens`. Without this, opencode reports
        // no usage and the compaction trigger never fires for this backend.
        v.push("--format".into());
        v.push("json".into());
        if let Some(id) = session_id {
            v.push("--session".into());
            v.push(id.to_string());
        }
        if resume {
            v.push("--continue".into());
        }
        if let Some(model) = self.model {
            v.push("--model".into());
            v.push(model.to_string());
        }
        v
    }

    /// Parse stdout from a completed agent process into a normalized outcome.
    pub fn parse_outcome(&self, stdout: String, fallback_session: &Uuid) -> anyhow::Result<TurnOutcome> {
        match &self.kind {
            // Verbose Claude turns emit stream-json (see `claude_argv`); the
            // default single-object path stays for non-verbose runs.
            AgentKind::Claude if self.verbose => parse_claude_stream(stdout, fallback_session),
            AgentKind::Claude => parse_claude(stdout, fallback_session),
            AgentKind::Opencode => parse_opencode(stdout, fallback_session),
            AgentKind::Acp { .. } => parse_opencode(stdout, fallback_session),
        }
    }
}

fn parse_claude(stdout: String, _fallback_session: &Uuid) -> anyhow::Result<TurnOutcome> {
    let parsed: ClaudeJsonResult = serde_json::from_str(stdout.trim())
        .map_err(|e| anyhow::anyhow!("failed to parse claude JSON output: {e}\n--- stdout ---\n{stdout}"))?;
    let raw = parsed.usage.unwrap_or_default();
    let usage = TokenUsage {
        input_tokens: raw.input_tokens,
        output_tokens: raw.output_tokens,
        cache_creation_input_tokens: raw.cache_creation_input_tokens,
        cache_read_input_tokens: raw.cache_read_input_tokens,
    };
    Ok(TurnOutcome {
        result_text: parsed.result,
        usage,
        tools: Vec::new(),
    })
}

/// Parse `opencode run --format json` output: a newline-delimited event
/// stream. We care about two event kinds (keyed by the top-level `type`):
/// - `text`: `part.text` is a chunk of the assistant's reply → accumulated
///   into `result_text` (so sentinel detection / tail printing still work).
/// - `step_finish`: `part.tokens` = `{input, output, reasoning, cache:{read,
///   write}}` → token usage. `reasoning` is output-side, so it is folded into
///   `output_tokens`; `cache.write`/`cache.read` map to cache creation/read.
///
/// Unrecognized / non-JSON lines are tolerated and skipped. If *no* JSON event
/// is recognized (e.g. an older opencode that ignores `--format json` and
/// prints plain text), we fall back to returning the raw stdout as the result
/// text with empty usage — preserving the previous behavior rather than
/// failing the turn.
fn parse_opencode(stdout: String, _fallback_session: &Uuid) -> anyhow::Result<TurnOutcome> {
    let mut result_text = String::new();
    let mut usage = TokenUsage::default();
    let mut saw_event = false;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(part) = val.get("part") else { continue };
        match val.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                saw_event = true;
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    result_text.push_str(text);
                }
            }
            Some("step_finish") => {
                saw_event = true;
                if let Some(tokens) = part.get("tokens") {
                    let input = tokens.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
                    let output = tokens.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
                    let reasoning = tokens.get("reasoning").and_then(|v| v.as_u64()).unwrap_or(0);
                    let cache = tokens.get("cache");
                    let cache_write = cache
                        .and_then(|c| c.get("write"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read = cache
                        .and_then(|c| c.get("read"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    // A multi-step turn emits several `step_finish` events; sum
                    // them so the turn total reflects every model round-trip.
                    usage.add(&TokenUsage {
                        input_tokens: input,
                        output_tokens: output + reasoning,
                        cache_creation_input_tokens: cache_write,
                        cache_read_input_tokens: cache_read,
                    });
                }
            }
            _ => {}
        }
    }

    if !saw_event {
        // Not the JSON event stream — treat stdout as the plain-text reply.
        return Ok(TurnOutcome {
            result_text: stdout,
            usage: TokenUsage::default(),
            tools: Vec::new(),
        });
    }

    Ok(TurnOutcome {
        result_text,
        usage,
        tools: Vec::new(),
    })
}

/// Parse newline-delimited `claude --output-format stream-json --verbose`
/// output. Each line is one event keyed by `type`:
/// - `assistant`: `message.content[]` may contain `tool_use` blocks → tools.
/// - `result`: final `result` text + `usage` totals.
///
/// Non-JSON / unrecognized lines are tolerated and skipped so transient
/// stderr-on-stdout noise doesn't fail the whole turn.
fn parse_claude_stream(stdout: String, _fallback_session: &Uuid) -> anyhow::Result<TurnOutcome> {
    let mut result_text = String::new();
    let mut usage = TokenUsage::default();
    let mut tools: Vec<ToolUseEvent> = Vec::new();
    let mut saw_result = false;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match val.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if let Some(content) = val
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let input_summary = summarize_tool_input(block.get("input"));
                            tools.push(ToolUseEvent { name, input_summary });
                        }
                    }
                }
            }
            Some("result") => {
                saw_result = true;
                if let Some(r) = val.get("result").and_then(|r| r.as_str()) {
                    result_text = r.to_string();
                }
                if let Some(u) = val.get("usage")
                    && let Ok(raw) = serde_json::from_value::<ClaudeUsage>(u.clone()) {
                        usage = TokenUsage {
                            input_tokens: raw.input_tokens,
                            output_tokens: raw.output_tokens,
                            cache_creation_input_tokens: raw.cache_creation_input_tokens,
                            cache_read_input_tokens: raw.cache_read_input_tokens,
                        };
                    }
            }
            _ => {}
        }
    }

    if !saw_result {
        return Err(anyhow::anyhow!(
            "claude stream-json output had no `result` event\n--- stdout ---\n{stdout}"
        ));
    }

    Ok(TurnOutcome {
        result_text,
        usage,
        tools,
    })
}

/// Best-effort one-line summary of a tool-use input object: prefer a salient
/// field (command/path/pattern/…), else compact JSON. Truncated for display.
fn summarize_tool_input(input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    for key in [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return truncate_summary(s, 120);
        }
    }
    truncate_summary(&serde_json::to_string(input).unwrap_or_default(), 120)
}

fn truncate_summary(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Representative `claude --print --output-format stream-json --verbose`
    // capture: system init, an assistant turn with two tool_use blocks, a
    // tool-result user event, then the final result with usage. Includes a
    // blank line and a junk line to exercise the skip-tolerant parser.
    const SAMPLE: &str = r#"
{"type":"system","subtype":"init","session_id":"abc","tools":["Bash","Read"]}
{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"working"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build","description":"build"}},{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/workspace/src/lib.rs"}}]}}
not json at all
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
{"type":"result","subtype":"success","result":"all done","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":3,"cache_read_input_tokens":4}}
"#;

    fn sid() -> Uuid {
        Uuid::nil()
    }

    #[test]
    fn parses_stream_json_result_usage_and_tools() {
        let out = parse_claude_stream(SAMPLE.to_string(), &sid()).unwrap();
        assert_eq!(out.result_text, "all done");
        assert_eq!(out.usage.input_tokens, 10);
        assert_eq!(out.usage.output_tokens, 20);
        assert_eq!(out.usage.cache_creation_input_tokens, 3);
        assert_eq!(out.usage.cache_read_input_tokens, 4);
        assert_eq!(out.usage.grand_total(), 37);

        // Tools captured in order, with a salient input summary.
        assert_eq!(out.tools.len(), 2);
        assert_eq!(out.tools[0].name, "Bash");
        assert_eq!(out.tools[0].input_summary, "cargo build");
        assert_eq!(out.tools[1].name, "Read");
        assert_eq!(out.tools[1].input_summary, "/workspace/src/lib.rs");
    }

    #[test]
    fn stream_json_without_result_event_errors() {
        let partial = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert!(parse_claude_stream(partial.to_string(), &sid()).is_err());
    }

    #[test]
    fn non_verbose_single_object_path_still_works() {
        let single = r#"{"result":"hi","usage":{"input_tokens":1,"output_tokens":2}}"#;
        let out = parse_claude(single.to_string(), &sid()).unwrap();
        assert_eq!(out.result_text, "hi");
        assert_eq!(out.usage.output_tokens, 2);
        assert!(out.tools.is_empty());
    }

    #[test]
    fn tool_input_summary_falls_back_to_compact_json() {
        let v = serde_json::json!({"foo": 1, "bar": "x"});
        let s = summarize_tool_input(Some(&v));
        assert!(s.contains("foo"));
        assert_eq!(summarize_tool_input(None), "");
    }

    // Real `opencode run --format json` capture (trimmed): a step_start, one
    // text event, and a step_finish carrying the token tallies.
    const OPENCODE_JSON: &str = r#"
{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}
{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"all "}}
{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"done <<ALL_DONE>>"}}
{"type":"step_finish","sessionID":"ses_1","part":{"type":"step-finish","tokens":{"total":1206,"input":1000,"output":150,"reasoning":50,"cache":{"write":4,"read":2}}}}
"#;

    #[test]
    fn opencode_json_parses_text_and_usage() {
        let out = parse_opencode(OPENCODE_JSON.to_string(), &sid()).unwrap();
        // Text events are concatenated; sentinel survives for downstream detection.
        assert_eq!(out.result_text, "all done <<ALL_DONE>>");
        assert_eq!(out.usage.input_tokens, 1000);
        // reasoning is folded into output.
        assert_eq!(out.usage.output_tokens, 200);
        assert_eq!(out.usage.cache_creation_input_tokens, 4);
        assert_eq!(out.usage.cache_read_input_tokens, 2);
        assert_eq!(out.usage.non_cached_input_tokens(), 1004);
    }

    #[test]
    fn opencode_plain_text_falls_back_to_raw_stdout() {
        // Older opencode (or a non-JSON line dump) → treat stdout as the reply.
        let out = parse_opencode("just plain text reply".to_string(), &sid()).unwrap();
        assert_eq!(out.result_text, "just plain text reply");
        assert_eq!(out.usage.grand_total(), 0);
    }

    #[test]
    fn non_cached_input_excludes_cache_reads() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 99,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 1000,
        };
        // Output tokens and cache reads are both excluded.
        assert_eq!(usage.non_cached_input_tokens(), 15);
        // Sanity: the all-in total still counts the cache read.
        assert_eq!(usage.input_total(), 1015);
    }

    #[test]
    fn rotation_signal_ignores_large_cache_reads() {
        // A turn dominated by cache reads must not push the rotation counter:
        // its non-cached contribution is tiny even though the context is huge.
        let outcome = TurnOutcome {
            result_text: String::new(),
            usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 50,
                cache_creation_input_tokens: 100,
                cache_read_input_tokens: 500_000,
            },
            tools: Vec::new(),
        };
        assert_eq!(outcome.non_cached_input_tokens(), 300);
        // Well under a typical compact_at threshold (e.g. 150_000), so no
        // rotation would be triggered by this turn.
        assert!(outcome.non_cached_input_tokens() < 150_000);
    }
}
