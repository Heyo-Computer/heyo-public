# Recursive printing pattern

Add an option for recursive behavior, e.g. "printer prints more printers". This is combined with heyvm worktree sandboxes such that each tasks gets its own agent and printer instance.
## Tasks
- [x] option integrates with the heyvm plugin for sandboxes and worktrees
  `--recursive` on `printer exec`; `exec_for_task` acquires a sandbox via the configured driver per task.
- [x] spawn a worktree and a heyvm sandbox for each task
  `ensure_worktree` (git worktree add, mkdir fallback) + `acquire_exec_sandbox` per task in `exec_for_task`.
- [x] spawn an agent to create a plan and then execute for each task
  Each task runs an isolated `printer exec T-<id>.md` subprocess (its own plan + execute loop).
- [x] tasks with dependencies should be linked (requires pre-planning step)
  Ready set computed via `store::compute_ready` (open tasks with satisfied `depends_on`); planning pass records `depends` links.
- [x] a codegraph instance should run in each worktree directory to ensure it picks up changes for each agent
  `codegraph_watch::try_spawn(&worktree_abs)` per task; the inner subprocess runs with `--no-codegraph-watch` so it isn't double-spawned.
- [x] use the "stacked pr" pattern to merge commits into the branch
  Each task commits to its own `task-<id>` branch after the run. NOTE: fixed a prior ordering bug where the worktree was removed *before* the commit (the commit `cd`'d into a deleted dir and silently lost all work via `|| true`); commit now runs before worktree removal and stages new files with `git add -A`. True multi-level stacking (rebasing each task branch onto the previous) is not yet implemented — branches are currently independent off the base.

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
