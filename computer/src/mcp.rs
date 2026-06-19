//! Minimal MCP (Model Context Protocol) server over stdio for desktop control.
//!
//! Speaks newline-delimited JSON-RPC 2.0 — the MCP stdio transport: one JSON
//! object per line on stdin, one response object per line on stdout. Exposes
//! the desktop commands as MCP tools so an agent host (e.g. `claude
//! --mcp-config`) can call them natively. The headline win over shelling out:
//! `screenshot` returns an inline base64 PNG **image** content block, so the
//! model sees the pixels in the same turn it called the tool.
//!
//! Mirrors the structure of codegraph's `mcp.rs`. Reuses the same platform
//! functions the CLI subcommands call (`crate::platform::*`).

use std::io::{BufRead, Write};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

use crate::platform::types::{Button, KeyAction, MouseAction};

/// Protocol version advertised when the client doesn't pin one.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// Default downscale cap for screenshots: the captured image's long edge is
/// reduced to at most this many pixels before PNG-encoding, bounding the
/// base64 payload returned into the model's context. Overridable per-call via
/// the `max_width` argument.
const DEFAULT_SCREENSHOT_MAX_WIDTH: u32 = 1568;

/// Run the stdio server loop until stdin closes. Always exits successfully —
/// transport teardown (the host closing the pipe) is normal shutdown.
pub fn serve() -> Result<()> {
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
    Ok(())
}

/// Parse one JSON-RPC line and produce the response value, or `None` for
/// notifications (no `id`) and unparseable input.
fn handle_line(line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    // Notifications (no id) are fire-and-forget — never respond.
    let id = id?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(dispatch(method, &params, id))
}

/// Route a request method to its handler and wrap in a JSON-RPC envelope.
fn dispatch(method: &str, params: &Value, id: Value) -> Value {
    match method {
        "initialize" => ok(id, initialize_result(params)),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tool_definitions() })),
        "tools/call" => match call_tool(params) {
            Ok(result) => ok(id, result),
            // A tool that failed to run (bad args, no display, missing
            // permission) is reported as a successful response whose result
            // has `isError: true` — the MCP convention, so the model sees the
            // message and can recover.
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
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "computer", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// Wrap a tool failure as MCP error content.
fn tool_error(message: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": message } ],
        "isError": true,
    })
}

/// Text content holding a pretty-printed JSON value (outputs/windows).
fn tool_ok_json(value: &Value) -> Result<Value> {
    let text = serde_json::to_string_pretty(value)?;
    Ok(json!({ "content": [ { "type": "text", "text": text } ] }))
}

/// Plain text acknowledgement (input tools that have no data to return).
fn tool_ok_text(text: &str) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ] })
}

/// Inline image content — the screenshot payload as a base64 PNG.
fn tool_ok_image(png: &[u8]) -> Value {
    json!({ "content": [ {
        "type": "image",
        "data": BASE64.encode(png),
        "mimeType": "image/png",
    } ] })
}

/// The tool catalog returned by `tools/list`.
fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "screenshot",
            "description": "Capture a monitor to a PNG image (returned inline). Downscaled to a max long edge by default to keep the payload small.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "output": { "type": "string", "description": "Monitor name (see `outputs`). Defaults to the first output." },
                    "max_width": { "type": "integer", "description": "Cap the long edge to this many pixels (aspect preserved). Defaults to 1568. Pass a large value for full resolution." }
                }
            }
        }),
        json!({
            "name": "outputs",
            "description": "List connected monitors/displays with geometry and scale.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "windows",
            "description": "List visible top-level windows (identifier, title, app id).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "mouse_move",
            "description": "Move the pointer to an absolute position. On Linux these are pixels on the chosen output; on macOS, points in the global display space.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "output": { "type": "string", "description": "Monitor name; defaults to the global bounding box." }
                },
                "required": ["x", "y"]
            }
        }),
        json!({
            "name": "mouse_click",
            "description": "Click a mouse button, optionally moving to (x,y) first. button: left|right|middle|side|extra. count: clicks (2 = double-click).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "button": { "type": "string", "description": "left (default), right, middle, side, extra." },
                    "count": { "type": "integer", "description": "Number of clicks. Default 1." },
                    "x": { "type": "integer", "description": "Optional: move here before clicking." },
                    "y": { "type": "integer", "description": "Optional: move here before clicking." },
                    "output": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "mouse_scroll",
            "description": "Scroll by (dx, dy) ticks. Positive dy scrolls down; positive dx scrolls right.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dx": { "type": "integer" },
                    "dy": { "type": "integer" }
                },
                "required": ["dx", "dy"]
            }
        }),
        json!({
            "name": "mouse_drag",
            "description": "Press a button at (from_x,from_y), drag to (to_x,to_y), and release — one gesture (text selection, sliders, moving windows).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from_x": { "type": "integer" },
                    "from_y": { "type": "integer" },
                    "to_x": { "type": "integer" },
                    "to_y": { "type": "integer" },
                    "button": { "type": "string", "description": "left (default), right, middle, side, extra." },
                    "output": { "type": "string" }
                },
                "required": ["from_x", "from_y", "to_x", "to_y"]
            }
        }),
        json!({
            "name": "key",
            "description": "Tap a key, or a chord. A value containing `+` (e.g. \"ctrl+shift+t\") is sent as a chord; otherwise it's a single key tap (e.g. \"Return\", \"Escape\", \"a\").",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keys": { "type": "string", "description": "Key name or chord like \"ctrl+c\"." }
                },
                "required": ["keys"]
            }
        }),
        json!({
            "name": "type",
            "description": "Type a literal string of text (US keyboard layout on Linux).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "delay_ms": { "type": "integer", "description": "Inter-keystroke delay in ms. Default 8." }
                },
                "required": ["text"]
            }
        }),
        json!({
            "name": "browse",
            "description": "Open a URL in the default web browser (fire-and-forget).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "http://, https://, or file:// URL." }
                },
                "required": ["url"]
            }
        }),
    ]
}

/// Execute a `tools/call`: read `name` + `arguments`, dispatch, pass the
/// already-wrapped content through. (Unlike codegraph's mcp, `run_tool` returns
/// the final `{content:[...]}` object so it can mix text and image content.)
fn call_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("tools/call requires a `name`"))?;
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    run_tool(name, &args)
}

/// Dispatch one tool by name to the platform functions, returning the wrapped
/// MCP content value. Separated from framing so it's the single place that
/// maps tool args onto `crate::platform::*`.
fn run_tool(name: &str, args: &Value) -> Result<Value> {
    match name {
        "screenshot" => {
            let output = args.get("output").and_then(Value::as_str);
            let max_width = args
                .get("max_width")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(DEFAULT_SCREENSHOT_MAX_WIDTH);
            let png = crate::platform::screenshot::capture_png(output, Some(max_width))?;
            Ok(tool_ok_image(&png))
        }
        "outputs" => tool_ok_json(&serde_json::to_value(crate::platform::outputs::collect()?)?),
        "windows" => tool_ok_json(&serde_json::to_value(crate::platform::windows::collect()?)?),
        "mouse_move" => {
            let (x, y) = (i32_arg(args, "x")?, i32_arg(args, "y")?);
            let output = args.get("output").and_then(Value::as_str).map(str::to_string);
            crate::platform::input::mouse(MouseAction::Move { x, y, output })?;
            Ok(tool_ok_text("moved"))
        }
        "mouse_click" => {
            let button = button_arg(args)?;
            let count = args
                .get("count")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(1);
            // Optional move-before-click when both x and y are supplied.
            if let (Some(x), Some(y)) = (
                args.get("x").and_then(Value::as_i64),
                args.get("y").and_then(Value::as_i64),
            ) {
                let output = args.get("output").and_then(Value::as_str).map(str::to_string);
                crate::platform::input::mouse(MouseAction::Move {
                    x: x as i32,
                    y: y as i32,
                    output,
                })?;
            }
            crate::platform::input::mouse(MouseAction::Click { button, count })?;
            Ok(tool_ok_text("clicked"))
        }
        "mouse_scroll" => {
            let (dx, dy) = (i32_arg(args, "dx")?, i32_arg(args, "dy")?);
            crate::platform::input::mouse(MouseAction::Scroll { dx, dy })?;
            Ok(tool_ok_text("scrolled"))
        }
        "mouse_drag" => {
            let from = (i32_arg(args, "from_x")?, i32_arg(args, "from_y")?);
            let to = (i32_arg(args, "to_x")?, i32_arg(args, "to_y")?);
            let button = button_arg(args)?;
            let output = args.get("output").and_then(Value::as_str);
            crate::platform::input::drag(from, to, button, output)?;
            Ok(tool_ok_text("dragged"))
        }
        "key" => {
            let keys = str_arg(args, "keys")?;
            // A `+` means a chord (e.g. "ctrl+shift+t"); otherwise a single tap.
            let action = if keys.contains('+') {
                KeyAction::Chord { combo: keys.to_string() }
            } else {
                KeyAction::Tap { key: keys.to_string() }
            };
            crate::platform::input::key(action)?;
            Ok(tool_ok_text("ok"))
        }
        "type" => {
            let text = str_arg(args, "text")?;
            let delay_ms = args.get("delay_ms").and_then(Value::as_u64).unwrap_or(8);
            crate::platform::input::type_text(text, delay_ms)?;
            Ok(tool_ok_text("typed"))
        }
        "browse" => {
            let url = str_arg(args, "url")?;
            crate::platform::browse::run(url)?;
            Ok(tool_ok_text("opened"))
        }
        _ => bail!("unknown tool `{name}`"),
    }
}

/// Required string argument.
fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("`{key}` (string) is required"))
}

/// Required integer argument.
fn i32_arg(args: &Value, key: &str) -> Result<i32> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("`{key}` (integer) is required"))
        .map(|n| n as i32)
}

/// Optional `button` argument, defaulting to left.
fn button_arg(args: &Value) -> Result<Button> {
    match args.get("button").and_then(Value::as_str) {
        Some(s) => parse_button(s),
        None => Ok(Button::Left),
    }
}

fn parse_button(s: &str) -> Result<Button> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "left" => Button::Left,
        "right" => Button::Right,
        "middle" => Button::Middle,
        "side" => Button::Side,
        "extra" => Button::Extra,
        other => bail!("unknown button `{other}` (left|right|middle|side|extra)"),
    })
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
        assert_eq!(resp["result"]["serverInfo"]["name"], "computer");
    }

    #[test]
    fn initialize_falls_back_to_default_version() {
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn tools_list_exposes_expected_catalog() {
        let req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = handle_line(req).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "screenshot",
                "outputs",
                "windows",
                "mouse_move",
                "mouse_click",
                "mouse_scroll",
                "mouse_drag",
                "key",
                "type",
                "browse",
            ]
        );
        // No raw CLI-only verbs leak through.
        assert!(!names.contains(&"sleep"));
    }

    #[test]
    fn notifications_get_no_response() {
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(handle_line(note).is_none());
    }

    #[test]
    fn unparseable_line_is_dropped() {
        assert!(handle_line("not json").is_none());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let req = r#"{"jsonrpc":"2.0","id":9,"method":"bogus/thing"}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn missing_required_arg_is_tool_error() {
        // `mouse_move` with no `x` → isError content mentioning the arg, not a
        // protocol error or panic. (Validation happens before any device I/O.)
        let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mouse_move","arguments":{"y":5}}}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains('x'), "got: {text}");
    }

    #[test]
    fn unknown_tool_is_tool_error() {
        let req = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"format_disk","arguments":{}}}"#;
        let resp = handle_line(req).unwrap();
        assert_eq!(resp["result"]["isError"], true);
    }

    #[test]
    fn parse_button_maps_and_rejects() {
        assert!(matches!(parse_button("right").unwrap(), Button::Right));
        assert!(matches!(parse_button("LEFT").unwrap(), Button::Left));
        assert!(parse_button("nope").is_err());
    }
}
