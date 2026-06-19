//! Append-only token-usage metrics. Each agent phase (run / review) plus the
//! exec-level aggregate writes one NDJSON line to `.printer/metrics.jsonl` so
//! token spend can be tracked over time without scraping stderr. Writes are
//! best-effort: callers log on `Err` and never fail a run over a metrics write.

use crate::agent::TokenUsage;
use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const METRICS_REL: &str = ".printer/metrics.jsonl";

/// One row in `.printer/metrics.jsonl`. `usage` is flattened so the token
/// fields sit at the top level alongside `grand_total`, keeping each line a
/// flat object that's trivial to load into a sheet or `jq`.
#[derive(Debug, Serialize)]
pub struct MetricsRecord<'a> {
    /// RFC3339 timestamp of when the record was written.
    pub ts: String,
    /// Absolute spec path the usage is attributed to.
    pub spec: String,
    /// Which phase produced it: `run`, `review`, or `exec-total`.
    pub phase: &'a str,
    /// Agent backend (e.g. `claude`, `acp:poolside`).
    pub agent: String,
    /// Model override if one was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(flatten)]
    pub usage: TokenUsage,
    /// Convenience denormalization of `usage.grand_total()`.
    pub grand_total: u64,
    /// Raw text-search / file-dump tool calls (Grep/Glob + Bash grep/find/cat…)
    /// the agent made this turn — the codegraph guardrail signal. Only present
    /// on `turn` records from verbose claude runs (tool activity is otherwise
    /// not observable); omitted elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_search_calls: Option<u64>,
    /// codegraph tool calls this turn (native `mcp__codegraph__*` plus
    /// `codegraph` Bash invocations). Pairs with `raw_search_calls`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codegraph_calls: Option<u64>,
}

impl<'a> MetricsRecord<'a> {
    /// Build a record, stamping `ts` with the current time and deriving
    /// `grand_total` from `usage`.
    pub fn new(
        spec: String,
        phase: &'a str,
        agent: String,
        model: Option<String>,
        usage: TokenUsage,
    ) -> Self {
        Self {
            ts: Utc::now().to_rfc3339(),
            grand_total: usage.grand_total(),
            spec,
            phase,
            agent,
            model,
            usage,
            raw_search_calls: None,
            codegraph_calls: None,
        }
    }

    /// Attach the per-turn search-tool histogram. Chainable on `new`.
    pub fn with_tool_counts(mut self, raw_search: u64, codegraph: u64) -> Self {
        self.raw_search_calls = Some(raw_search);
        self.codegraph_calls = Some(codegraph);
        self
    }
}

/// Resolve the metrics file path under a working directory.
pub fn metrics_path(cwd: &Path) -> PathBuf {
    cwd.join(METRICS_REL)
}

/// Append one NDJSON record to `<cwd>/.printer/metrics.jsonl`, creating the
/// `.printer` dir if needed. Best-effort — surface failures to the caller so
/// they can log without aborting the run.
pub fn append(cwd: &Path, rec: &MetricsRecord) -> Result<()> {
    let path = metrics_path(cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(rec).context("serializing metrics record")?;
    line.push('\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening metrics file {}", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("appending to metrics file {}", path.display()))?;
    Ok(())
}

/// Best-effort wrapper: append and log on failure instead of propagating, for
/// call sites that must never fail a run over a metrics write.
pub fn record(cwd: &Path, spec: &str, phase: &str, agent: String, model: Option<String>, usage: TokenUsage) {
    let rec = MetricsRecord::new(spec.to_string(), phase, agent, model, usage);
    if let Err(e) = append(cwd, &rec) {
        eprintln!("[printer] warning: failed to write metrics ({phase}): {e}");
    }
}

/// Best-effort per-turn record carrying the codegraph search-tool histogram.
/// Written from `Session::turn` when tool activity is observable (verbose
/// claude runs); see `MetricsRecord::raw_search_calls`.
#[allow(clippy::too_many_arguments)]
pub fn record_turn(
    cwd: &Path,
    spec: &str,
    agent: String,
    model: Option<String>,
    usage: TokenUsage,
    raw_search: u64,
    codegraph: u64,
) {
    let rec = MetricsRecord::new(spec.to_string(), "turn", agent, model, usage)
        .with_tool_counts(raw_search, codegraph);
    if let Err(e) = append(cwd, &rec) {
        eprintln!("[printer] warning: failed to write turn metrics: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_ndjson_rows() {
        let dir = tempfile::tempdir().unwrap();
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 3,
        };
        record(dir.path(), "/tmp/a.md", "run", "claude".into(), None, usage);
        record(
            dir.path(),
            "/tmp/a.md",
            "review",
            "claude".into(),
            Some("opus".into()),
            usage,
        );

        let body = std::fs::read_to_string(metrics_path(dir.path())).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);

        let r0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(r0["phase"], "run");
        assert_eq!(r0["grand_total"], 20); // 10+5+2+3
        assert_eq!(r0["input_tokens"], 10); // flattened from usage
        assert!(r0.get("model").is_none(), "None model is omitted");

        let r1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r1["phase"], "review");
        assert_eq!(r1["model"], "opus");
        // Phase records without tool counts omit the fields entirely.
        assert!(r0.get("raw_search_calls").is_none());
        assert!(r0.get("codegraph_calls").is_none());
    }

    #[test]
    fn turn_records_carry_search_tool_histogram() {
        let dir = tempfile::tempdir().unwrap();
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        record_turn(dir.path(), "/tmp/s.md", "claude".into(), None, usage, 3, 1);

        let body = std::fs::read_to_string(metrics_path(dir.path())).unwrap();
        let r: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(r["phase"], "turn");
        assert_eq!(r["raw_search_calls"], 3);
        assert_eq!(r["codegraph_calls"], 1);
        assert_eq!(r["grand_total"], 140);
    }
}
