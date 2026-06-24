/// Sentinel emitted by the agent when every task is done.
pub const SENTINEL_DONE: &str = "<<ALL_DONE>>";
/// Sentinel prefix emitted when the agent is blocked.
pub const SENTINEL_BLOCKED: &str = "<<BLOCKED:";
/// Sentinel emitted after the bootstrap turn writes a checklist back to the
/// spec.
pub const SENTINEL_PLAN_READY: &str = "<<PLAN_READY>>";
/// Sentinel that opens a block of questions the agent wants the user to
/// answer during the standalone `printer plan` flow.
pub const SENTINEL_QUESTIONS_OPEN: &str = "<<QUESTIONS>>";
/// Sentinel that closes a block of questions opened with `<<QUESTIONS>>`.
pub const SENTINEL_QUESTIONS_CLOSE: &str = "<<END_QUESTIONS>>";

/// Shared token-efficiency guidance appended to the planning and execution
/// prompts. States the codegraph-first / diffs-only preference unconditionally
/// so it holds even when the codegraph plugin's agent hook isn't installed
/// (Spec 010, tasks 2 & 3).
const EFFICIENCY_GUIDANCE: &str = "\nToken efficiency:\n\
- Navigate code with the `codegraph` CLI (search / definition / references / outline / snippet) \
instead of raw `grep`/`find`/`cat` + full-file reads — it is tree-sitter-indexed and far cheaper \
in tokens. Reach for `grep`/`find` only when codegraph genuinely can't answer.\n\
- When changing code, apply a minimal unified diff (e.g. `codegraph patch`, or the codegraph-edit \
skill) rather than rewriting whole files, and do not paste large file bodies back into the turn.\n\
- Keep responses to the diff plus a one-line status; let the task log and the diff carry the rest.\n";

/// Prompt for the empty-spec bootstrap turn: ask the agent to write a clean
/// checklist into the spec file. The driver will then parse it and create
/// tasks. The agent does NOT execute work here.
pub fn bootstrap_prompt(spec_path: &str) -> String {
    format!(
        "The file `{spec}` does not currently contain a markdown checklist that the driver can \
parse into tasks. Read it, infer the work it implies, and rewrite it as a flat checklist using \
GitHub-flavored markdown task list lines at column 0:\n\
\n\
  - [ ] Short imperative title for one unit of work\n\
    (optional indented description, 2-space indented, multiple lines OK)\n\
  - [ ] Next item\n\
\n\
Rules:\n\
- Preserve any items that are already `- [x]` (treat them as already done).\n\
- Each top-level `- [ ]` line becomes one task.\n\
- Indented lines beneath an item become its description.\n\
- Do NOT execute any work in this turn — only write the checklist back to `{spec}`.\n\
- Output the literal sentinel {plan_ready} on its own line when finished.\n\
\n\
Be terse. No preamble, no recap, no explanation of what you wrote. Edit the file and emit the \
sentinel. Nothing else.\n",
        spec = spec_path,
        plan_ready = SENTINEL_PLAN_READY,
    )
}

/// Planning-pass prompt: the spec already produced tasks in the store, but
/// before any code work begins we ask the agent to review them and refine
/// each into a detailed, actionable plan (expanded descriptions, dependencies,
/// splits if needed). The agent updates the task store via `printer task`
/// commands and does NOT execute any implementation work in this turn.
pub fn planning_prompt(printer_bin: &str, spec_path: &str) -> String {
    format!(
        "The spec at `{spec}` has been parsed into the task store. Before any code is written, \
do a planning pass: turn each open task into a detailed, actionable plan entry.\n\
\n\
Use these `printer task` commands:\n\
\n\
  {printer} task list                                       — see all tasks\n\
  {printer} task show <ID>                                  — read one task\n\
  {printer} task comment <ID> \"...\"                          — append a planning note \
under the task's `## Notes` section (use this to record the detailed plan)\n\
  {printer} task create \"<title>\" [--description \"...\"]     — split a task by adding a \
new sub-task if the original is too coarse\n\
  {printer} task depends <ID> --on <OTHER_ID>               — record a dependency between \
tasks when ordering matters\n\
\n\
For each open task:\n\
1. Read it with `{printer} task show <ID>` and read any files it touches so the plan is \
grounded in the actual code, not guesswork.\n\
2. Append a `comment` to that task that contains a concrete, step-by-step plan: the files \
to change, the functions/types involved, and the verification you will use (build, tests, \
manual check). Keep it tight — bullets, not prose.\n\
3. If a task is too coarse to land in one pass, `create` follow-up tasks for the missing \
pieces and link them with `depends`.\n\
\n\
Rules:\n\
- Do NOT modify source code, configs, or anything outside the task store in this turn.\n\
- Do NOT mark tasks done, started, or blocked in this turn — leave statuses alone.\n\
- If a task is already detailed enough (clear plan, scoped, actionable), leave it; do not \
add fluff.\n\
- When every open task has either a planning comment or is already detailed enough, output \
the literal sentinel {plan_ready} on its own line and stop.\n\
\n\
Be terse. No preamble, no recap. Plan the work, then emit the sentinel.\n\
{efficiency}",
        printer = printer_bin,
        spec = spec_path,
        plan_ready = SENTINEL_PLAN_READY,
        efficiency = EFFICIENCY_GUIDANCE,
    )
}

/// Interactive planning prompt for the standalone `printer plan` command.
/// Same shape as [`planning_prompt`] but explicitly invites the agent to
/// optionally ask the user clarifying questions before finalizing the plan —
/// this is the only place in the printer flow where an agent may stop and
/// request user input.
pub fn interactive_planning_prompt(printer_bin: &str, spec_path: &str) -> String {
    format!(
        "The spec at `{spec}` has been parsed into the task store. You are running in \
standalone planning mode (`printer plan`) — do NOT execute any code or modify source \
files. Your only outputs are updates to the task store and (optionally) a single block of \
questions for the user.\n\
\n\
Use these `printer task` commands:\n\
\n\
  {printer} task list                                       — see all tasks\n\
  {printer} task show <ID>                                  — read one task\n\
  {printer} task comment <ID> \"...\"                          — append a planning note \
under the task's `## Notes` section (use this to record the detailed plan)\n\
  {printer} task create \"<title>\" [--description \"...\"]     — split a task by adding a \
new sub-task\n\
  {printer} task depends <ID> --on <OTHER_ID>               — record a dependency between \
tasks when ordering matters\n\
\n\
For each open task:\n\
1. Read it with `{printer} task show <ID>` and read any files it touches so the plan is \
grounded in the actual code, not guesswork.\n\
2. Append a `comment` to that task that contains a concrete, step-by-step plan (files, \
functions, verification). Bullets, not prose.\n\
3. If a task is too coarse, `create` follow-up tasks and link them with `depends`.\n\
\n\
If — and ONLY if — there is genuine ambiguity in the spec that you cannot resolve from the \
code, you may ask the user clarifying questions. To do so, output a single block of the \
form:\n\
\n\
  {q_open}\n\
  1. <question one>\n\
  2. <question two>\n\
  {q_close}\n\
\n\
Keep questions tight, numbered, and answerable in one or two lines each. Do NOT ask \
questions you can answer yourself by reading the code or the spec. After you emit the \
block, stop the turn — the driver will collect answers from the user and resume you.\n\
\n\
When the plan is complete and you have no further questions, output the literal sentinel \
{plan_ready} on its own line and stop.\n\
\n\
Rules:\n\
- Do NOT modify source code, configs, or anything outside the task store.\n\
- Do NOT mark tasks done, started, or blocked.\n\
- Do NOT both ask questions AND emit {plan_ready} in the same turn.\n\
\n\
Be terse. No preamble, no recap.\n",
        printer = printer_bin,
        spec = spec_path,
        plan_ready = SENTINEL_PLAN_READY,
        q_open = SENTINEL_QUESTIONS_OPEN,
        q_close = SENTINEL_QUESTIONS_CLOSE,
    )
}

/// Resume prompt for the planning flow after the user has answered the
/// agent's questions. The answers are inlined into the prompt and the agent
/// is asked to continue refining the plan.
pub fn plan_resume_with_answers_prompt(printer_bin: &str, answers: &str) -> String {
    format!(
        "The user has answered your questions. Their responses are below between the \
markers. Incorporate them into the plan by updating task descriptions / comments via \
`{printer} task comment` or `create` / `depends` as appropriate, then either ask another \
focused question block (using {q_open} ... {q_close}) if something is still unclear, or \
emit {plan_ready} if the plan is complete.\n\
\n\
Rules unchanged: no code edits, no status changes, do not both ask and finalize in the \
same turn.\n\
\n\
--- BEGIN USER ANSWERS ---\n\
{answers}\n\
--- END USER ANSWERS ---\n",
        printer = printer_bin,
        answers = answers.trim(),
        q_open = SENTINEL_QUESTIONS_OPEN,
        q_close = SENTINEL_QUESTIONS_CLOSE,
        plan_ready = SENTINEL_PLAN_READY,
    )
}

/// One-turn prompt for `printer spec-from-followups`. The agent reads a
/// follow-ups dump (verbatim block included) and writes a fresh canonical
/// spec to `out_path`. It does NOT execute any of the work and does NOT edit
/// any other files.
pub fn spec_from_followups_prompt(out_path: &str, followups: &str) -> String {
    format!(
        "You are converting a previously-saved review follow-ups file into a new printer \
spec. Your only task is to write a clean markdown spec at `{out}` and then stop. Do NOT \
edit any other file, run any builds, or execute any of the work described.\n\
\n\
The canonical printer spec format is:\n\
\n\
  # <Project / change title>\n\
\n\
  A short paragraph (1–3 sentences) describing what this spec is and why it exists.\n\
\n\
  ## Tasks\n\
\n\
  - [ ] First task — short imperative title for one unit of work\n\
    Optional 2-space-indented description, multi-line allowed.\n\
\n\
  - [ ] Second task — …\n\
\n\
Rules for writing the new spec:\n\
- Each follow-up bullet that represents real work becomes one `- [ ]` task.\n\
- Skip bullets explicitly marked out-of-scope or that duplicate work.\n\
- Group related follow-ups into one task only when they truly are one unit of work; \
otherwise keep them separate.\n\
- Titles should be short and imperative (\"add X\", \"fix Y\", \"refactor Z\").\n\
- Add a 2-space-indented description under any task that needs more context than its title.\n\
- Do NOT carry over status markers from the source — every new task is `- [ ]`.\n\
- Preserve any concrete file paths or function names from the follow-ups in the \
descriptions so the next agent has them.\n\
\n\
After writing the file, output the literal sentinel {plan_ready} on its own line and stop. \
Be terse — no preamble, no recap.\n\
\n\
--- BEGIN FOLLOW-UPS ---\n\
{body}\n\
--- END FOLLOW-UPS ---\n",
        out = out_path,
        body = followups.trim(),
        plan_ready = SENTINEL_PLAN_READY,
    )
}

/// Per-turn nudge: the agent uses `printer task` to manage its work queue.
/// `extra_block` is appended verbatim before the skill list (used for
/// hook-contributed instructions); `skills` are rendered as a reference list.
pub fn nudge_prompt_with(
    printer_bin: &str,
    extra_block: Option<&str>,
    skills: &[crate::skills::Skill],
) -> String {
    let mut out = render_nudge_body(printer_bin);
    if let Some(extra) = extra_block {
        out.push_str(extra);
    }
    append_skills(&mut out, skills);
    out
}

fn render_nudge_body(printer_bin: &str) -> String {
    format!(
        "You are working through a queue of tasks tracked by `printer task`. The task store is the \
source of truth for status — not the original spec file. Use these commands:\n\
\n\
  {printer} task ready              — list ready tasks (highest priority first)\n\
  {printer} task list               — full table; add `--status in_progress` etc. for filtering\n\
  {printer} task show <ID>          — full description and notes for one task\n\
  {printer} task start <ID>         — claim a task before working on it\n\
  {printer} task comment <ID> \"...\"  — log progress / findings\n\
  {printer} task done <ID>          — mark fully complete\n\
  {printer} task block <ID> --reason \"...\"  — if you are stuck on something external\n\
\n\
This turn:\n\
1. Run `{printer} task ready`. If it is empty AND `{printer} task list --status in_progress` is \
also empty, every task is finished — output the literal sentinel {done} on its own line and stop.\n\
2. Otherwise pick the top ready task. Claim it with `{printer} task start <ID>`. Read its \
description with `{printer} task show <ID>`.\n\
3. Actually do the work — edit code, run commands, etc. Do not just describe what you would do. \
If the task touches a desktop UI or web app, do not call it complete on the strength of unit \
tests alone — run the app you just changed (e.g. `computer browse <local URL>` for a web app, \
or launch the desktop binary) and use the `computer` skill to click through the affected flow \
before marking the task done. If `$WAYLAND_DISPLAY` is unset, note that in a `task comment` \
instead of silently skipping the click-test.\n\
4. When the task is fully complete, `{printer} task done <ID>`. If you finish quickly you may \
claim the next ready task and continue; otherwise stop and the driver will call you again.\n\
5. If you cannot proceed, `{printer} task block <ID> --reason \"…\"` and emit \
{blocked} <one-line reason>>> to stop.\n\
\n\
Output style: be terse. Don't narrate, recap, or explain what you changed — the diff and task \
log already say that. Status updates belong in `{printer} task comment`, not in chat. End the \
turn with the sentinel or a short one-line status. No preamble, no closing summary.\n\
{efficiency}",
        printer = printer_bin,
        done = SENTINEL_DONE,
        blocked = SENTINEL_BLOCKED,
        efficiency = EFFICIENCY_GUIDANCE,
    )
}

/// One-off ingest prompt used by the review cycle: the prior review produced
/// a non-PASS verdict, so we hand the report to the coding agent and ask it
/// to translate the findings into concrete tasks (new or reopened) in the
/// task store. The normal loop runs immediately after this turn.
pub fn fix_from_review_prompt(printer_bin: &str, review_report: &str) -> String {
    format!(
        "The previous implementation pass was reviewed and the verdict was NOT a clean PASS. \
The full review report is included below between the markers. Read it carefully.\n\
\n\
Your job this turn is to translate the report's findings into concrete units of work in the \
task store, NOT to fix the code yet. After this turn, the normal task loop will resume and you \
will work each task you queue here.\n\
\n\
Use these `printer task` commands as needed:\n\
\n\
  {printer} task list                                       — see existing tasks\n\
  {printer} task create \"<title>\" [--description \"...\"]     — queue a new task\n\
  {printer} task comment <ID> \"...\"                          — log review-derived \
context on an existing task\n\
\n\
Rules:\n\
- Every MISSING or PARTIAL item in the report must map to at least one open task. If an \
existing task already covers it, just `comment` to add the new context; otherwise `create` a \
new task.\n\
- Every 'Suggested follow-up' that is in scope of the original spec should also become a task.\n\
- Out-of-scope follow-ups can be ignored.\n\
- Do NOT modify source code in this turn — only update the task store.\n\
- Be terse. No preamble, no recap. End the turn with a one-line status.\n\
\n\
--- BEGIN REVIEW REPORT ---\n\
{report}\n\
--- END REVIEW REPORT ---\n",
        printer = printer_bin,
        report = review_report.trim(),
    )
}

/// Stronger nudge after a stalled turn (no task transitions observed).
pub fn unstall_prompt(printer_bin: &str) -> String {
    format!(
        "Last turn made no observable progress in the task store. Re-read `{printer} task ready`, \
claim the top item with `{printer} task start <ID>`, and either finish it now and run `{printer} \
task done <ID>`, or run `{printer} task block <ID> --reason \"…\"` and emit \
{blocked} <reason>>>. Do not stall again. Be terse — no preamble, no recap.\n",
        printer = printer_bin,
        blocked = SENTINEL_BLOCKED,
    )
}

/// Prompt at the start of a freshly-rotated session (compaction).
/// This turn is for orientation only — the driver follows it with a planning
/// pass before any code work resumes, so do not start implementing yet.
pub fn rotation_prompt(printer_bin: &str, spec_path: &str) -> String {
    format!(
        "This is a fresh session — the previous session was rotated for context-window reasons. \
The state of the world is captured in:\n\
\n\
  - the original specification at `{spec}` (read-only reference)\n\
  - the task store, which is authoritative for status\n\
\n\
Orient yourself: run `{printer} task list` and `{printer} task list --status in_progress` to see \
where things stand. Skim the spec. Read the most recent comments on any in_progress or open tasks \
so you know what the previous session was thinking.\n\
\n\
Do NOT start implementing in this turn — the driver will run a planning pass next so you can \
refresh the plan before writing code. If everything is already done emit {done}; if you are \
stuck emit {blocked} <reason>>>; otherwise just acknowledge briefly and stop.\n\
\n\
Be terse — no preamble, no recap of what the previous session did.\n",
        printer = printer_bin,
        spec = spec_path,
        done = SENTINEL_DONE,
        blocked = SENTINEL_BLOCKED,
    )
}

/// Single-turn review prompt. `extra_block` is appended after the
/// instructions and before the skill list; pass `None` for the bare prompt.
pub fn review_prompt_with(
    spec_path: &str,
    base_ref: &str,
    skills: &[crate::skills::Skill],
    extra_block: Option<&str>,
) -> String {
    let mut out = render_review_body(spec_path, base_ref);
    if let Some(extra) = extra_block {
        out.push_str(extra);
    }
    if !skills.is_empty() {
        out.push_str(SKILLS_HEADER);
        for s in skills {
            out.push_str(&format!(
                "- `{name}` — {desc}\n  Skill file: `{path}`\n",
                name = s.name,
                desc = s.description,
                path = s.skill_file.display(),
            ));
        }
    }
    out
}

/// Prompt for `printer test`: a focused click-test turn. Unlike review, this
/// is exclusively about exercising the running app through the `computer` tool
/// — no static reading, no per-item spec audit. Reuses the same verification
/// vocabulary as `render_review_body` step 4 so the two stay consistent.
pub fn test_prompt(
    spec_path: &str,
    base_ref: &str,
    url: Option<&str>,
    skills: &[crate::skills::Skill],
    extra_block: Option<&str>,
) -> String {
    let launch = match url {
        Some(u) => format!(
            "Start the app and open it with `computer browse {u}` (the change's local URL)."
        ),
        None => "Start the app under test (e.g. `computer browse <local URL>` for a web app, or \
launch the desktop binary). Infer the start command from AGENTS.md / README / package.json scripts."
            .to_string(),
    };
    let mut out = format!(
        "You are click-testing a UI/web change end-to-end with the `computer` tool. A real display \
is available — exercise the running app, do not stop at reading code or unit tests.\n\
\n\
1. Read `{spec}` to understand what behavior the change was supposed to deliver, and skim \
`git diff {base}...HEAD` to see which UI surface changed.\n\
2. {launch}\n\
3. Use the `computer` skill to drive the affected flow: synthesize the clicks/keystrokes a user \
would make, and capture before/after screenshots as evidence. Input synthesis is allowed — the \
app, not the repo, is being mutated; do NOT edit project files, build artifacts excepted.\n\
4. If no display is actually reachable (`$WAYLAND_DISPLAY`/`$XDG_SESSION_TYPE` unset, or \
`/dev/uinput` missing), say so explicitly and set the verdict to FAIL rather than silently \
skipping.\n\
5. Produce a concise markdown report on stdout with these sections:\n\
   - `## Verdict` — PASS if the flow worked as the spec intended, PARTIAL if it partly worked or \
you could only verify some of it, FAIL if it broke or no display was available.\n\
   - `## Verification` — bullet list of the exact click-test steps you performed and what you \
observed (cite screenshots). \n\
   - `## Suggested follow-ups` — short bulleted list, or 'none'.\n\
\n\
Be terse and concrete. No preamble, no closing summary.\n",
        spec = spec_path,
        base = base_ref,
        launch = launch,
    );
    if let Some(extra) = extra_block {
        out.push_str(extra);
    }
    append_skills(&mut out, skills);
    out
}

fn append_skills(out: &mut String, skills: &[crate::skills::Skill]) {
    if skills.is_empty() {
        return;
    }
    out.push_str(SKILLS_HEADER);
    for s in skills {
        out.push_str(&format!(
            "- `{name}` — {desc}\n  Skill file: `{path}`\n",
            name = s.name,
            desc = s.description,
            path = s.skill_file.display(),
        ));
    }
}

const SKILLS_HEADER: &str = "\nYou also have skills available — bundled reference docs that explain how to use \
specific tools for exercising or verifying the change you just made (e.g. driving the desktop or \
a web app to click-test a UI change end-to-end). Read a skill's SKILL.md only when its description \
matches what you actually need to do; do not load skills you will not use. Each skill is read-only \
reference — using a skill must not modify any project files.\n\
\nAvailable skills:\n";

fn render_review_body(spec_path: &str, base_ref: &str) -> String {
    format!(
        "You are reviewing the result of an agent-driven implementation against its original \
specification.\n\
\n\
1. Read `{spec}`. Treat its checklist as the requirements that were supposed to be delivered.\n\
2. Inspect the working tree changes against the git ref `{base}`. Use `git diff {base}...HEAD` and \
`git status` (and `git diff` for unstaged work) to see what actually changed. Read the relevant \
changed files.\n\
3. Look for an `AGENTS.md` at the repo root (and in the immediate subdirectory containing the \
changed files, if applicable). If present, treat it as authoritative for how to build, test, and \
lint this project — its `Build`, `Test`, and `Lint` (or equivalently named) sections list the \
commands to run. Follow it before falling back to inferring commands from `CLAUDE.md`, \
`README.md`, `Makefile`, `justfile`, `package.json` scripts, `Cargo.toml`, `pyproject.toml`, \
`go.mod`, etc. If `AGENTS.md` exists but is silent on a specific check, note that and infer.\n\
4. Verify the change actually works — do not stop at static reading. Run the build/test/lint \
commands identified in step 3 via your shell tool: typecheck/build, the test suite (or the \
targeted tests covering the changed code), and any linter the repo already uses. If the change \
touches a desktop UI or web app, do NOT stop at tests — exercise the running app end-to-end. \
Start the app (or `computer browse <local URL>` for a web app), then use the `computer` skill \
to drive the affected flow and capture before/after screenshots as evidence in the report. \
Input synthesis is allowed here because the app, not the repo, is being mutated; the read-only \
contract still applies to project files. If `$WAYLAND_DISPLAY` is unset (no display in this \
sandbox), say so explicitly in the report instead of silently skipping the click-test. \
Capture the actual exit codes and key output lines — do not assume green. If the build/tests \
do not exist or cannot run in this environment, say so explicitly in the report rather than \
silently skipping.\n\
5. Produce a concise markdown review report on stdout with these sections:\n\
   - `## Verdict` — one of: PASS, PARTIAL, FAIL. A change that does not build or whose tests \
fail is at most PARTIAL, and FAIL if the failure is in code the spec asked for. If the change \
has a UI/web surface and no display was available to click-test it, the verdict is at most \
PARTIAL.\n\
   - `## Verification` — bullet list of the build/test/lint commands you actually ran and their \
result (pass/fail + the meaningful line of output). If the change has a UI/web surface, also list \
the click-test steps you actually performed (or an explicit 'no UI surface' / 'no display \
available' line) so absence of UI verification is visible, not silent. If you skipped a check, \
say why.\n\
   - `## Per-item findings` — for each checklist item in `{spec}`, mark MET / PARTIAL / MISSING and \
explain in one line why, citing files/lines.\n\
   - `## Out-of-scope or risky changes` — anything modified that is not justified by the spec, \
including possible regressions.\n\
   - `## Suggested follow-ups` — short bulleted list, or 'none'.\n\
\n\
Be terse and concrete. Cite file paths. No preamble, no closing summary, no restating of the \
verdict outside the `## Verdict` section. You may run read-only and build/test commands; do NOT \
edit source files, create commits, or otherwise mutate the working tree (build artifacts and \
caches under `target/`, `node_modules/`, `dist/`, etc. are fine).\n",
        spec = spec_path,
        base = base_ref,
    )
}
