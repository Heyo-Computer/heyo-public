//! Turning an event into a command that runs inside a VM.
//!
//! The contract, which is what a function author actually programs against:
//!
//! | env var | value |
//! |---|---|
//! | `QFN_INVOCATION_ID` | unique per invocation; also the daemon's operation id |
//! | `QFN_FUNCTION_ID`   | the function's id |
//! | `QFN_ATTEMPT`       | delivery attempt, 1-based |
//! | `QFN_SOURCE`        | `invoke` \| `schedule` \| `replay` |
//! | `QFN_PAYLOAD_B64`   | base64 of the payload's JSON; empty when there is none |
//! | `QFN_PAYLOAD_LEN`   | decoded byte length |
//!
//! Guest side is one line: `payload=$(printf %s "$QFN_PAYLOAD_B64" | base64 -d)`.
//!
//! **Why base64, given the daemon already quotes env values?** Transport, not
//! quoting. The value crosses a serial console whose reader frames on `\n`
//! (mvm-ctrl/src/driver/firecracker.rs:1719-1745), so a payload containing a
//! newline would be indistinguishable from the framing that terminates the
//! command's output. Base64 has no newline and survives intact.
//!
//! **Why we quote anyway, in template mode.** heyvm single-quotes env values for
//! us, but `PayloadMode::Template` splices the payload into the command string
//! *before* the daemon sees it — at which point our text is code being handed to
//! `sh -lc`. So template substitution quotes using the daemon's own algorithm.

use crate::bus::InvokeEvent;
use crate::config::{ExecSpec, PayloadMode};
use crate::vm::{ExecOutput, VmError, VmManager};
use base64::Engine;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How often to ask the daemon whether an async operation has finished.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Slack over the function's own timeout before we stop polling. Covers the
/// daemon's own bookkeeping either side of the command.
const POLL_SLACK: Duration = Duration::from_secs(5);

/// Single-quote a value for `sh -c`.
///
/// Deliberately the same algorithm as heyvm's `shell_single_quote`
/// (mvm-ctrl/src/driver/driver.rs:572): wrap in single quotes, and render an
/// embedded single quote as `'"'"'`. Nothing inside single quotes is special to
/// the shell, so a payload cannot escape the command it is spliced into.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Base64 of the payload's JSON serialization, or `""` when there is none.
fn encode_payload(payload: Option<&serde_json::Value>) -> (String, usize) {
    let Some(value) = payload else {
        return (String::new(), 0);
    };
    let bytes = match serde_json::to_vec(value) {
        Ok(b) => b,
        // Unreachable for anything that arrived as JSON, but returning an empty
        // payload beats panicking a dispatcher over one malformed event.
        Err(e) => {
            tracing::error!(error = %e, "could not serialize payload; sending an empty one");
            return (String::new(), 0);
        }
    };
    let len = bytes.len();
    (base64::engine::general_purpose::STANDARD.encode(&bytes), len)
}

/// Build the environment for one invocation.
///
/// Spec-provided env goes in first so the reserved `QFN_*` keys always win.
/// `FunctionSpec::validate` already rejects a spec that tries to set them, so
/// this ordering is a second line of defence rather than the only one.
pub fn build_env(spec: &ExecSpec, event: &InvokeEvent, attempt: u32) -> HashMap<String, String> {
    let mut env = spec.env.clone().unwrap_or_default();
    let (payload_b64, payload_len) = encode_payload(event.payload.as_ref());

    env.insert("QFN_INVOCATION_ID".into(), event.invocation_id.clone());
    env.insert("QFN_FUNCTION_ID".into(), event.function_id.clone());
    env.insert("QFN_ATTEMPT".into(), attempt.to_string());
    env.insert("QFN_SOURCE".into(), event.source.as_str().into());
    env.insert("QFN_PAYLOAD_B64".into(), payload_b64);
    env.insert("QFN_PAYLOAD_LEN".into(), payload_len.to_string());
    env
}

/// Render the command, substituting into it only in template mode.
///
/// In `Env` mode the command is passed through untouched — a `{{payload}}` in
/// the text is left alone rather than silently substituted, so switching modes
/// is an explicit act.
pub fn render_command(spec: &ExecSpec, event: &InvokeEvent) -> String {
    if matches!(spec.payload_mode, PayloadMode::Env) {
        return spec.command.clone();
    }

    let payload_json = event
        .payload
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_default();

    spec.command
        .replace("{{payload}}", &sh_quote(&payload_json))
        .replace("{{invocation_id}}", &sh_quote(&event.invocation_id))
}

/// Run one invocation to completion inside a specific VM.
///
/// Goes through the daemon's async `exec-operations` pair rather than the
/// one-shot exec, keyed by the invocation id. The reason is idempotency, not
/// duration: `start_exec_operation` returns the *existing* record for a known
/// operation id instead of running again (mvm-ctrl/src/api.rs:5098-5106). So if
/// queue-fn dies between "the command finished" and "the message was acked",
/// JetStream's redelivery replays into a result lookup rather than a second
/// execution — which matters a great deal when the function had side effects.
pub async fn run(
    vms: &VmManager,
    spec: &ExecSpec,
    sandbox_id: &str,
    event: &InvokeEvent,
    attempt: u32,
) -> Result<ExecOutput, VmError> {
    debug_assert!(
        crate::vm::valid_operation_id(&event.invocation_id),
        "invocation id {:?} is not a legal operationId; the daemon would 400",
        event.invocation_id,
    );

    let env = build_env(spec, event, attempt);
    let command = render_command(spec, event);

    let record = vms
        .start_exec_operation(sandbox_id, &event.invocation_id, &command, Some(&env))
        .await?;

    // A replay of a finished operation comes back terminal on the first call.
    if record.is_finished() {
        return finish(sandbox_id, &event.invocation_id, record);
    }

    let budget = Duration::from_secs(spec.timeout_secs) + POLL_SLACK;
    let deadline = Instant::now() + budget;

    loop {
        if Instant::now() >= deadline {
            return Err(VmError::ExecTimeout {
                sandbox_id: sandbox_id.to_string(),
                secs: budget.as_secs(),
            });
        }
        tokio::time::sleep(POLL_INTERVAL).await;

        let record = vms
            .poll_exec_operation(sandbox_id, &event.invocation_id)
            .await?;
        if record.is_finished() {
            return finish(sandbox_id, &event.invocation_id, record);
        }
    }
}

/// Convert a terminal operation record into an outcome.
///
/// A `failed` status with a result is a command that ran and exited non-zero —
/// a legitimate result, not an error. A `failed` status with no result means the
/// command never ran, which is an error against the platform.
fn finish(
    sandbox_id: &str,
    invocation_id: &str,
    record: crate::vm::ExecOperationRecord,
) -> Result<ExecOutput, VmError> {
    // The daemon keys operation records by id on disk, so a mismatch here means
    // we are about to attribute someone else's output to this invocation. Cheap
    // to check, and silent corruption if we don't.
    if record.operation_id != invocation_id {
        return Err(VmError::ExecFailed {
            sandbox_id: sandbox_id.to_string(),
            reason: format!(
                "the daemon returned a record for operation {:?} when {:?} was requested",
                record.operation_id, invocation_id
            ),
        });
    }
    if !record.sandbox_id.is_empty() && record.sandbox_id != sandbox_id {
        return Err(VmError::ExecFailed {
            sandbox_id: sandbox_id.to_string(),
            reason: format!(
                "the daemon ran operation {invocation_id:?} on sandbox {:?}, not {sandbox_id:?}",
                record.sandbox_id
            ),
        });
    }

    match record.result {
        Some(r) => Ok(ExecOutput::normalize(
            r.stdout,
            r.stderr,
            r.output,
            r.exit_code,
        )),
        None => Err(VmError::ExecFailed {
            sandbox_id: sandbox_id.to_string(),
            reason: record
                .error
                .unwrap_or_else(|| "the daemon reported no result and no error".into()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventSource;
    use crate::vm::{ExecOperationRecord, ExecOperationResult};

    fn exec_spec(command: &str, mode: PayloadMode) -> ExecSpec {
        ExecSpec {
            command: command.into(),
            working_directory: None,
            env: None,
            timeout_secs: 20,
            max_payload_bytes: 4096,
            payload_mode: mode,
        }
    }

    fn event(payload: Option<serde_json::Value>) -> InvokeEvent {
        InvokeEvent::new("demo", payload, EventSource::Invoke)
    }

    #[test]
    fn the_reserved_env_contract_is_always_present() {
        let ev = event(Some(serde_json::json!({"a": 1})));
        let env = build_env(&exec_spec("true", PayloadMode::Env), &ev, 2);

        assert_eq!(env["QFN_INVOCATION_ID"], ev.invocation_id);
        assert_eq!(env["QFN_FUNCTION_ID"], "demo");
        assert_eq!(env["QFN_ATTEMPT"], "2");
        assert_eq!(env["QFN_SOURCE"], "invoke");
        assert_eq!(env["QFN_PAYLOAD_LEN"], r#"{"a":1}"#.len().to_string());

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&env["QFN_PAYLOAD_B64"])
            .expect("payload must be valid base64");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&decoded).unwrap(),
            serde_json::json!({"a": 1}),
        );
    }

    /// An absent payload must still set the variable. A function doing
    /// `base64 -d <<< "$QFN_PAYLOAD_B64"` should see an empty string, not an
    /// unbound-variable error under `set -u`.
    #[test]
    fn a_missing_payload_is_an_empty_string_not_an_unset_variable() {
        let env = build_env(&exec_spec("true", PayloadMode::Env), &event(None), 1);
        assert_eq!(env["QFN_PAYLOAD_B64"], "");
        assert_eq!(env["QFN_PAYLOAD_LEN"], "0");
    }

    /// Regression: base64 is about *transport*, not quoting. The serial console
    /// reader frames on newlines, so a payload containing one was read as the
    /// end of the command's output and truncated everything after it.
    #[test]
    fn a_payload_containing_newlines_survives_encoding() {
        let ev = event(Some(serde_json::json!({"text": "line one\nline two\n"})));
        let env = build_env(&exec_spec("true", PayloadMode::Env), &ev, 1);

        assert!(
            !env["QFN_PAYLOAD_B64"].contains('\n'),
            "the encoded payload must contain no newline, or the serial framing breaks"
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&env["QFN_PAYLOAD_B64"])
            .unwrap();
        let back: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(back["text"], "line one\nline two\n");
    }

    #[test]
    fn spec_env_is_merged_but_cannot_shadow_the_contract() {
        let mut spec = exec_spec("true", PayloadMode::Env);
        spec.env = Some(HashMap::from([
            ("MY_VAR".into(), "mine".into()),
            // validate() rejects this, but the merge order must defend anyway.
            ("QFN_FUNCTION_ID".into(), "spoofed".into()),
        ]));
        let env = build_env(&spec, &event(None), 1);
        assert_eq!(env["MY_VAR"], "mine");
        assert_eq!(
            env["QFN_FUNCTION_ID"], "demo",
            "the reserved namespace must win over a spec value"
        );
    }

    #[test]
    fn env_mode_leaves_the_command_untouched() {
        let spec = exec_spec("echo {{payload}}", PayloadMode::Env);
        assert_eq!(
            render_command(&spec, &event(Some(serde_json::json!(1)))),
            "echo {{payload}}",
            "switching to template substitution must be an explicit choice",
        );
    }

    #[test]
    fn template_mode_substitutes_payload_and_invocation_id() {
        let spec = exec_spec("run {{payload}} {{invocation_id}}", PayloadMode::Template);
        let ev = event(Some(serde_json::json!({"k": "v"})));
        let rendered = render_command(&spec, &ev);
        assert!(rendered.contains(r#"'{"k":"v"}'"#), "got {rendered}");
        assert!(rendered.contains(&sh_quote(&ev.invocation_id)));
        assert!(!rendered.contains("{{"), "no placeholder may survive");
    }

    /// Regression: template mode spliced the payload in raw, so a payload
    /// containing `'; rm -rf /; '` became a second command running as root
    /// inside the guest. Quoting with the daemon's own algorithm closes it.
    #[test]
    fn a_hostile_payload_cannot_break_out_of_a_template() {
        let spec = exec_spec("echo {{payload}}", PayloadMode::Template);
        for hostile in [
            "'; rm -rf /; echo '",
            "$(whoami)",
            "`id`",
            "a'b",
            "; shutdown now",
            "\" && curl evil.example.com",
        ] {
            let ev = event(Some(serde_json::json!(hostile)));
            let rendered = render_command(&spec, &ev);

            // Everything after `echo ` is one single-quoted literal. Quotes may
            // only appear as the wrapper or in the `'"'"'` escape sequence.
            let arg = rendered.strip_prefix("echo ").expect("prefix");
            assert!(arg.starts_with('\'') && arg.ends_with('\''), "got {rendered}");
            let inner = &arg[1..arg.len() - 1];
            assert!(
                !inner.replace("'\"'\"'", "").contains('\''),
                "an unescaped quote would end the literal early: {rendered}",
            );
        }
    }

    #[test]
    fn sh_quote_escapes_embedded_single_quotes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("it's"), r#"'it'"'"'s'"#);
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn a_finished_operation_with_a_result_is_an_outcome_not_an_error() {
        let record = ExecOperationRecord {
            operation_id: "op".into(),
            sandbox_id: "sb-1".into(),
            status: "failed".into(),
            result: Some(ExecOperationResult {
                output: "nope".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 3,
            }),
            error: None,
        };
        let out = finish("sb-1", "op", record).expect("a non-zero exit is a result, not an error");
        assert_eq!(out.exit_code, 3);
        assert_eq!(out.stdout, "nope", "the serial path's output is recovered");
        assert!(!out.succeeded());
    }

    /// A `failed` record with no result means the command never ran at all —
    /// that implicates the platform, not the function, so it must not be
    /// reported as a clean non-zero exit.
    #[test]
    fn a_finished_operation_with_no_result_is_an_error() {
        let record = ExecOperationRecord {
            operation_id: "op".into(),
            sandbox_id: "sb-1".into(),
            status: "failed".into(),
            result: None,
            error: Some("sandbox went away".into()),
        };
        let err = finish("sb-1", "op", record).expect_err("no result means no execution");
        assert!(matches!(err, VmError::ExecFailed { .. }));
        assert!(err.to_string().contains("sandbox went away"));
    }

    /// The daemon stores operation records as files keyed by id. A record for a
    /// different operation or a different sandbox means we are about to
    /// attribute someone else's output to this invocation — and since the caller
    /// would see a plausible-looking result, nothing downstream would catch it.
    #[test]
    fn a_record_for_the_wrong_operation_is_refused() {
        let record = ExecOperationRecord {
            operation_id: "some-other-op".into(),
            sandbox_id: "sb-1".into(),
            status: "completed".into(),
            result: Some(ExecOperationResult {
                output: "someone else's output".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }),
            error: None,
        };
        let err = finish("sb-1", "my-op", record).expect_err("must not accept a foreign record");
        assert!(err.to_string().contains("some-other-op"), "got {err}");
    }

    #[test]
    fn a_record_from_the_wrong_sandbox_is_refused() {
        let record = ExecOperationRecord {
            operation_id: "my-op".into(),
            sandbox_id: "sb-elsewhere".into(),
            status: "completed".into(),
            result: Some(ExecOperationResult {
                output: "ok".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }),
            error: None,
        };
        let err = finish("sb-1", "my-op", record).expect_err("wrong sandbox");
        assert!(err.to_string().contains("sb-elsewhere"), "got {err}");
    }

    /// Older daemons omit `sandboxId` from the record, so an empty one must be
    /// treated as "not reported" rather than as a mismatch.
    #[test]
    fn an_absent_sandbox_id_is_not_treated_as_a_mismatch() {
        let record = ExecOperationRecord {
            operation_id: "my-op".into(),
            sandbox_id: String::new(),
            status: "completed".into(),
            result: Some(ExecOperationResult {
                output: "ok".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }),
            error: None,
        };
        assert!(finish("sb-1", "my-op", record).is_ok());
    }

    #[test]
    fn an_error_free_failure_still_reports_something_actionable() {
        let record = ExecOperationRecord {
            operation_id: "op".into(),
            sandbox_id: "sb-1".into(),
            status: "failed".into(),
            result: None,
            error: None,
        };
        let err = finish("sb-1", "op", record).expect_err("still an error");
        assert!(
            !err.to_string().is_empty(),
            "an empty error message tells an operator nothing"
        );
    }
}
