# Planning Improvement

The code agents should begin by creating a plan, on init and after each compaction.
## Tasks

- [x] Always generate a plan initially; if the spec is valid we should still do a pass at planning first to make it a detailed actionable plan
  `run.rs` runs the `planning_prompt` pass before the nudge loop; only skipped on `--continue` when the checkpoint already moved past `Phase::Planning`.
- [x] create a new command, "printer plan spec.md" which will generate the plan (should be documented as a checkpoint) and allow for the agent to generate questions for the end user to answer - this is an optional step that allows the agent to gather feedback, this is the only time an agent can stop the flow to prompt the user
  `printer plan` (`plan.rs` + `interactive_planning_prompt`); writes `.printer/plan.checkpoint` and supports the `<<QUESTIONS>>`/`<<END_QUESTIONS>>` interaction.
- [x] any time we rotate a session or perform compaction, create an updated plan before writing code
  `run.rs` post-rotation: after `session.rotate()` it sends `rotation_prompt` then `planning_prompt` before resuming the nudge loop.

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
