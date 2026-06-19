# Follow-ups for /home/sarocu/Projects/printer/specs/011-observability.md

Generated: 2026-05-29T18:30:19.634058518+00:00
Verdict: PASS

## Suggested follow-ups

- `TurnOutcome.tools` is `#[allow(dead_code)]` but is consumed in `session.rs` — the attribute is now stale and can be dropped.
- Consider gating clippy (the new collapsible-if/needless-borrow lints are trivial `cargo clippy --fix` cleanups) or at least fixing the two introduced in `agent/mod.rs:313` and `review.rs:287`.
- `exec` UI review still can't auto-route to host (shared sandbox); a future spec could build the `computer` bin into the heyvm image, as the `computer_on_path` comment notes.
