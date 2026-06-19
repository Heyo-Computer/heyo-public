# Improve Review Agent

Improve the handoff between coding and review to improve the quality of the finished work. 

## Tasks
- [x] implement a review cycle in which the feedback from review is passed to a coding agent to work on
  Implemented in `exec.rs` (review → `fix_from_review_prompt` → task loop, repeated up to `--max-review-passes`).
- [x] the CLI should have an option for max number of review passes to prevent an infinite loop
  `ExecArgs::max_review_passes` (default 3).
- [x] ensure that the review agent has the tools and instructions to actually test the changes
  `prompts::render_review_body` step 4 mandates running build/test/lint and click-testing UI surfaces.
- [x] have the review agent check for a "AGENTS.md" file for instructions like building and testing
  `render_review_body` step 3 treats `AGENTS.md` as authoritative; `AGENTS.md` exists at repo root.
- [x] if plugins are not installed when "exec" or "run" is invoked, prompt the user to install (or suppress with a flag for CI systems)
  `plugins::prompt_if_no_plugins` wired into `run.rs`/`exec.rs`; `--skip-plugin-check` suppresses for CI (non-TTY bails with guidance).

<!--
Spec format reference (full docs in the printer README):
  * Lines starting with `- [ ]`, `- [x]`, `* [ ]`, `+ [ ]` (etc.) at
    column 0 are tasks. The text after the checkbox is the title.
  * Lines indented by 2 spaces or one tab below a task become its
    description body.
  * Any unindented non-task line ends the current task's description.
  * Re-runs of `printer run <this-file>` are idempotent — items are
    matched to existing tasks by a stable anchor derived from this
    file's path + the task title.
-->
