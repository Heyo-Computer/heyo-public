//! End-to-end test for the `codegraph mcp` stdio server: index a temp repo,
//! spawn the real binary, drive a canned initialize + tools/call exchange over
//! its stdio, and assert the framed JSON-RPC responses.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_codegraph");

/// Write a tiny Rust source file and build the on-disk index for it.
fn setup_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn handle_request(n: u32) -> u32 {\n    n + 1\n}\n\npub struct Worker;\n",
    )
    .unwrap();
    let status = Command::new(BIN)
        .arg("index")
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "indexing failed");
    dir
}

#[test]
fn serves_initialize_and_tool_calls_over_stdio() {
    let dir = setup_repo();

    let mut child = Command::new(BIN)
        .arg("mcp")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"query":"handle_request"}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"snippet","arguments":{"file":"lib.rs","symbol":"handle_request"}}}"#,
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

    // The notification produces no response, so the four id-bearing requests
    // yield four frames.
    assert_eq!(responses.len(), 4, "responses: {responses:?}");

    // initialize
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");

    // tools/list
    assert_eq!(responses[1]["id"], 2);
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);

    // tools/call search → text content is JSON holding the hit.
    assert_eq!(responses[2]["id"], 3);
    let text = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let hits: Value = serde_json::from_str(text).unwrap();
    assert_eq!(hits[0]["symbol"], "handle_request");
    assert_eq!(hits[0]["file"], "lib.rs");

    // tools/call snippet → text content is JSON holding the source body.
    assert_eq!(responses[3]["id"], 4);
    let text = responses[3]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let snip: Value = serde_json::from_str(text).unwrap();
    assert_eq!(snip["symbol"], "handle_request");
    assert!(snip["source"].as_str().unwrap().contains("n + 1"));
}

#[test]
fn tool_call_against_missing_index_reports_iserror() {
    // A fresh dir with no index: the search tool should fail gracefully as
    // isError content rather than crashing the server.
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(BIN)
        .arg("mcp")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"search","arguments":{{"query":"x"}}}}}}"#
        )
        .unwrap();
    }
    let stdout = child.stdout.take().unwrap();
    let line = BufReader::new(stdout).lines().next().unwrap().unwrap();
    child.wait().unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no index"), "got: {text}");
}
