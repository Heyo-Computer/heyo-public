# Efficiency Pass

## Tasks

* [x] Only consider non-cached tokens for rotation `TokenUsage::non_cached_input_tokens()` (input + cache_creation, excluding cache_read) now drives `Session::cumulative_input_tokens` and the compaction trigger in `run.rs`.
* [x] Ensure codegraph tools used for searching `EFFICIENCY_GUIDANCE` in `prompts.rs` states the codegraph-first preference in the planning + nudge prompts unconditionally; verbose mode flags raw grep/find/cat calls (`session::count_inefficient_search_tools`).
* [x] In system prompts, ask for compact responses (only diffs) to ensure efficient output Same `EFFICIENCY_GUIDANCE` block asks for minimal unified diffs (not full-file rewrites) and diff-plus-one-line-status responses.
* [x] Add a "test" command that uses the computer cli to perform a click test `printer test <spec>` (`test.rs` + `prompts::test_prompt`): runs on the host, gates on display + `computer` on PATH, drives one click-test turn, records metrics (`phase=test`), exits non-zero unless verdict is PASS.
