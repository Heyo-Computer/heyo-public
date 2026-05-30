//! End-to-end test of the full `printer exec` loop (spec sync → planning →
//! task loop → review → checkpoint + metrics) driven by a *stub* agent rather
//! than a real LLM. The stub is a tiny shell script installed as `claude` on a
//! PATH we control: it inspects the prompt it is handed and either marks every
//! task done (nudge turn) or returns a PASS verdict (review turn), emitting the
//! single-object JSON `claude --print --output-format json` produces.
//!
//! This is the first test that exercises the loop end-to-end; the rest of the
//! suite is unit-level. It is Unix-only (the stub is `/bin/sh`).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// The stub `claude`. `claude --print --output-format json ... <prompt>` passes
/// the prompt as the final argv, so we branch on its content:
/// - review prompt  → emit a one-line PASS verdict
/// - nudge prompt   → mark every `.printer/tasks/T-*.md` done, emit <<ALL_DONE>>
/// - anything else  → a no-op "planned" reply (planning / bootstrap turns)
///
/// Single-line JSON keeps `parse_claude` happy without newline escaping.
const STUB_CLAUDE: &str = r###"#!/bin/sh
for prompt; do :; done
usage='"usage":{"input_tokens":5,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}'
case "$prompt" in
  *"reviewing the result"*)
    printf '%s' "{\"result\":\"## Verdict PASS\",$usage}"
    ;;
  *"working through a queue of tasks"*)
    for f in .printer/tasks/T-*.md; do
      [ -e "$f" ] || continue
      id=$(basename "$f" .md)
      "$PRINTER_BIN" task done "$id" >/dev/null 2>&1 || true
    done
    printf '%s' "{\"result\":\"<<ALL_DONE>>\",$usage}"
    ;;
  *)
    printf '%s' "{\"result\":\"planned\",$usage}"
    ;;
esac
"###;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn exec_drives_spec_to_done_with_metrics() {
    let printer_bin = env!("CARGO_BIN_EXE_printer");

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // A minimal git repo with a base commit so review's `git diff` has a base.
    git(root, &["init", "-q"]);
    fs::write(root.join("README.md"), "base\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    // Name the default branch `main` so detect_base() finds it.
    git(root, &["branch", "-M", "main"]);

    // Spec with two tasks.
    let spec = root.join("feature.md");
    fs::write(&spec, "# Feature\n\n- [ ] first task\n- [ ] second task\n").unwrap();

    // Install the stub `claude` on a PATH we prepend.
    let bindir = root.join("stubbin");
    fs::create_dir_all(&bindir).unwrap();
    let stub = bindir.join("claude");
    fs::write(&stub, STUB_CLAUDE).unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::new(printer_bin)
        .args([
            "exec",
            spec.to_str().unwrap(),
            "--cwd",
            root.to_str().unwrap(),
            "--no-sandbox",
            "--no-codegraph-watch",
            "--skip-plugin-check",
        ])
        .env("PATH", &path)
        .env("PRINTER_BIN", printer_bin)
        // Keep config/plugin lookups out of the developer's real ~/.printer.
        .env("HOME", root)
        .current_dir(root)
        .output()
        .expect("run printer exec");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "printer exec failed:\n--- stderr ---\n{stderr}"
    );

    // 1. Every task transitioned to done.
    let tasks_dir = root.join(".printer/tasks");
    let mut task_files = 0;
    for entry in fs::read_dir(&tasks_dir).expect("tasks dir exists") {
        let p = entry.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) == Some("md") {
            task_files += 1;
            let body = fs::read_to_string(&p).unwrap();
            assert!(
                body.contains("status = \"done\""),
                "task {} not marked done:\n{body}",
                p.display()
            );
        }
    }
    assert_eq!(task_files, 2, "expected two synced task files");

    // 2. A checkpoint reached Phase::Done.
    let exec_dir = root.join(".printer/exec");
    let mut saw_done = false;
    for entry in fs::read_dir(&exec_dir).expect("exec dir exists") {
        let body = fs::read_to_string(entry.unwrap().path()).unwrap();
        if body.contains("\"phase\": \"done\"") || body.contains("\"phase\":\"done\"") {
            saw_done = true;
        }
    }
    assert!(saw_done, "no checkpoint reached phase=done");

    // 3. Metrics were recorded for each phase of the run.
    let metrics = fs::read_to_string(root.join(".printer/metrics.jsonl"))
        .expect(".printer/metrics.jsonl exists");
    assert!(metrics.contains("\"phase\":\"run\""), "no run metrics row");
    assert!(metrics.contains("\"phase\":\"review\""), "no review metrics row");
    assert!(
        metrics.contains("\"phase\":\"exec-total\""),
        "no exec-total metrics row"
    );
    // Stub reported non-zero usage, so rows must carry token counts.
    assert!(metrics.contains("\"input_tokens\":5"), "token usage not recorded");
}
