//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! Speaks newline-delimited JSON-RPC 2.0 — the MCP stdio transport: one JSON
//! object per line on stdin, one response object per line on stdout. Exposes
//! the read-only query subcommands (`search`, `definition`, `outline`,
//! `snippet`, `references`) as MCP tools so an agent host can call codegraph
//! natively instead of shelling out. Mutating commands (`patch`, `index`,
//! `watch`) are intentionally not served.
//!
//! Each tool reuses the same library functions the CLI subcommands call and
//! reads `.codegraph/index.json` relative to the current working directory,
//! so the host launches `codegraph mcp` with cwd set to the repo root.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::index::Index;
use crate::languages::SymbolKind;
use crate::{outline, parse, search, snippet, symbols};

/// Protocol version advertised when the client doesn't pin one.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// Run the stdio server loop until stdin closes. Always exits successfully —
/// transport teardown (the host closing the pipe) is normal shutdown, not an
/// error.
pub fn serve() -> Result<ExitCode> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.context("reading from stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line) {
            let mut s = serde_json::to_string(&response)?;
            s.push('\n');
            out.write_all(s.as_bytes())
                .context("writing response to stdout")?;
            out.flush().context("flushing stdout")?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Parse one JSON-RPC line and produce the response value, or `None` for
/// notifications (messages without an `id`) and unparseable input. Pure with
/// respect to the protocol framing; tool calls reach into the filesystem.
fn handle_line(line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        // A malformed line with no id we can echo back → drop it. Per JSON-RPC
        // a parse error response carries a null id, but for a stdio MCP server
        // silently ignoring junk is friendlier than a flood of error frames.
        Err(_) => return None,
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    // Notifications (no id) are fire-and-forget — never respond.
    let id = id?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(dispatch(method, &params, id))
}

/// Route a request method to its handler and wrap the result in a JSON-RPC
/// envelope carrying the original `id`.
fn dispatch(method: &str, params: &Value, id: Value) -> Value {
    match method {
        "initialize" => ok(id, initialize_result(params)),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tool_definitions() })),
        "tools/call" => match call_tool(params) {
            Ok(result) => ok(id, result),
            // A tool that failed to run (bad args, missing index, parse error)
            // is reported as a successful JSON-RPC response whose result has
            // `isError: true` — the MCP convention, so the model sees the
            // message and can recover rather than the host treating it as a
            // protocol fault.
            Err(e) => ok(id, tool_error(&e.to_string())),
        },
        _ => err(id, -32601, &format!("method not found: {method}")),
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize_result(params: &Value) -> Value {
    // Echo the client's requested protocol version when present so we don't
    // advertise one it can't speak; fall back to our default otherwise.
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "codegraph", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// Wrap a tool failure as MCP error content.
fn tool_error(message: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": message } ],
        "isError": true,
    })
}

/// Wrap a successful tool result (a serializable value) as MCP text content,
/// pretty-printed so it reads cleanly in a transcript.
fn tool_ok(value: &Value) -> Result<Value> {
    let text = serde_json::to_string_pretty(value)?;
    Ok(json!({ "content": [ { "type": "text", "text": text } ] }))
}

/// The tool catalog returned by `tools/list`. Schemas mirror the CLI flags.
fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "Search the code index by symbol name or signature substring. Returns matching symbols with file, line range, kind, and signature.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Substring to match against symbol names (and signatures unless `name` is set)." },
                    "kind": { "type": "string", "description": "Filter by symbol kind: function, method, class, struct, enum, trait, interface, module, type, constant, variable." },
                    "name": { "type": "boolean", "description": "Match the qualified name only, skipping signature text. Default false." },
                    "limit": { "type": "integer", "description": "Maximum hits to return. Default 50." }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "definition",
            "description": "Look up a symbol's definition(s) by exact qualified (e.g. `Foo::bar`) or bare name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Qualified or bare symbol name." }
                },
                "required": ["symbol"]
            }
        }),
        json!({
            "name": "outline",
            "description": "Hierarchical outline of one file — signatures only, no bodies. Far cheaper than reading the whole file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Path to the file, relative to the repo root or absolute." }
                },
                "required": ["file"]
            }
        }),
        json!({
            "name": "snippet",
            "description": "Pull the source of one symbol or a line range from a file. Cheaper than reading the whole file. Pass `symbol` or `lines`, not both.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Path to the file." },
                    "symbol": { "type": "string", "description": "Symbol name (qualified `Foo::bar` or bare `bar`)." },
                    "lines": { "type": "string", "description": "Line range, `start:end` or `start-end`." }
                },
                "required": ["file"]
            }
        }),
        json!({
            "name": "references",
            "description": "Find lexical references to a name across indexed files (word-boundary scan; may include comments/strings and miss dynamic dispatch).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Name to scan for; qualified names are reduced to the bare trailing segment." }
                },
                "required": ["symbol"]
            }
        }),
    ]
}

/// Execute a `tools/call`: read `name` + `arguments`, dispatch to the matching
/// query, and wrap the result. Errors propagate to `dispatch`, which renders
/// them as `isError` content.
fn call_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("tools/call requires a `name`"))?;
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let value = run_tool(name, &args)?;
    tool_ok(&value)
}

/// Dispatch one tool by name to the shared library query functions, returning
/// the raw (serializable) result value. Separated from MCP framing so it can
/// be unit-tested directly.
fn run_tool(name: &str, args: &Value) -> Result<Value> {
    match name {
        "search" => {
            let query = str_arg(args, "query")?;
            let by_name = args.get("name").and_then(Value::as_bool).unwrap_or(false);
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(50);
            let kind = match args.get("kind").and_then(Value::as_str) {
                Some(k) => {
                    Some(SymbolKind::parse(k).with_context(|| format!("unknown kind `{k}`"))?)
                }
                None => None,
            };
            let index = load_index()?;
            let hits = search::search(
                &index,
                search::SearchOpts {
                    query,
                    kind,
                    by_name,
                    limit: Some(limit),
                },
            );
            Ok(serde_json::to_value(hits)?)
        }
        "definition" => {
            let symbol = str_arg(args, "symbol")?;
            let index = load_index()?;
            Ok(serde_json::to_value(search::definition(&index, symbol))?)
        }
        "references" => {
            let symbol = str_arg(args, "symbol")?;
            let (root, index) = load_index_with_root()?;
            // Mirror the CLI: a qualified name scans for its bare trailing
            // segment (after `::` or `.`).
            let bare = symbol.rsplit("::").next().unwrap_or(symbol);
            let bare = bare.rsplit('.').next().unwrap_or(bare);
            Ok(serde_json::to_value(search::references(&index, bare, &root))?)
        }
        "outline" => {
            let file = str_arg(args, "file")?;
            let parsed = parse::parse_path(Path::new(file))?;
            let syms = symbols::extract(&parsed);
            Ok(serde_json::to_value(outline::build(&syms))?)
        }
        "snippet" => {
            let file = str_arg(args, "file")?;
            let symbol = args.get("symbol").and_then(Value::as_str);
            let lines = args.get("lines").and_then(Value::as_str);
            let parsed = parse::parse_path(Path::new(file))?;
            let snip = match (symbol, lines) {
                (Some(sym), None) => {
                    let (s, body) = snippet::by_symbol(&parsed, sym)?;
                    snippet::Snippet {
                        file: file.to_string(),
                        symbol: Some(s.qualified),
                        start_line: s.start_line,
                        end_line: s.end_line,
                        source: body,
                    }
                }
                (None, Some(range)) => {
                    let (start, end, body) = snippet::by_lines(&parsed, range)?;
                    snippet::Snippet {
                        file: file.to_string(),
                        symbol: None,
                        start_line: start,
                        end_line: end,
                        source: body,
                    }
                }
                (Some(_), Some(_)) => bail!("pass `lines` or `symbol`, not both"),
                (None, None) => bail!("pass a `symbol` name or a `lines` range"),
            };
            Ok(serde_json::to_value(snip)?)
        }
        _ => bail!("unknown tool `{name}`"),
    }
}

/// Pull a required string argument, erroring with a clear message if missing.
fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("`{key}` (string) is required"))
}

/// Load the index at the current working directory, same as the CLI query
/// subcommands.
fn load_index() -> Result<Index> {
    Ok(load_index_with_root()?.1)
}

fn load_index_with_root() -> Result<(std::path::PathBuf, Index)> {
    let root = std::env::current_dir()?.canonicalize()?;
    let index = Index::load(&root)?.ok_or_else(|| {
        anyhow!(
            "no index at {}; run `codegraph index` first",
            Index::path_for(&root).display()
        )
    })?;
    Ok((root, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_client_protocol_version() {
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "codegraph");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_to_default_version() {
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn tools_list_exposes_five_readonly_tools() {
        let req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = handle_line(req).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["search", "definition", "outline", "snippet", "references"]
        );
        // No mutating tools leak through.
        assert!(!names.contains(&"patch"));
        assert!(!names.contains(&"index"));
    }

    #[test]
    fn notifications_get_no_response() {
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(handle_line(note).is_none());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let req = r#"{"jsonrpc":"2.0","id":9,"method":"bogus/thing"}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn unparseable_line_is_dropped() {
        assert!(handle_line("not json").is_none());
    }

    #[test]
    fn missing_required_arg_is_tool_error() {
        // `search` with no `query` should surface as isError content, not a
        // protocol error or a panic.
        let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{}}}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("query"), "got: {text}");
    }

    #[test]
    fn unknown_tool_is_tool_error() {
        let req = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"patch","arguments":{}}}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }
}
