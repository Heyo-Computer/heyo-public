//! End-to-end test for the `computer mcp` stdio server: spawn the real binary
//! and drive a canned initialize + tools/list + tools/call exchange over its
//! stdio, asserting the framed JSON-RPC responses.
//!
//! Desktop I/O (real screenshots / input) needs a display, so this test only
//! asserts protocol framing and the tool catalog. The `outputs` call is
//! asserted as a well-formed frame whether it succeeds (on a headed host) or
//! returns `isError` (on a headless CI host) — never a crash or a malformed
//! frame.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_computer");

#[test]
fn serves_initialize_list_and_tool_call_over_stdio() {
    let mut child = Command::new(BIN)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"outputs","arguments":{}}}"#,
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in requests {
            writeln!(stdin, "{r}").unwrap();
        }
        // Drop stdin to close the pipe so the server loop terminates.
    }

    let stdout = child.stdout.take().unwrap();
    let responses: Vec<Value> = BufReader::new(stdout)
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    child.wait().unwrap();

    // The notification produces no response → three id-bearing requests, three
    // frames.
    assert_eq!(responses.len(), 3, "responses: {responses:?}");

    // initialize
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "computer");

    // tools/list — full catalog of 10 tools.
    assert_eq!(responses[1]["id"], 2);
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 10);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"screenshot"));
    assert!(names.contains(&"mouse_drag"));

    // tools/call outputs — a well-formed result frame either way (success on a
    // headed host, isError on a headless one). Never a protocol error.
    assert_eq!(responses[2]["id"], 3);
    assert!(responses[2].get("error").is_none(), "got protocol error: {:?}", responses[2]);
    let content = responses[2]["result"]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
}
