# printer

A small Rust CLI that drives a child `claude`, `codex`, `amp`, or `opencode` session through a
markdown spec — it asks the agent to build a plan into the spec file, then
nudges it to keep executing, resumes the session across turns, and rotates to a
fresh session when the context fills up. A separate `review` subcommand spawns
another session to grade the resulting work against the original spec.

## Install

```sh
cd printer
cargo build --release
# binary at ./target/release/printer
```

You'll need `claude`, `codex`, `amp`, and/or `opencode` on your `PATH`.

If `codegraph` is also on `PATH` (or installed at
`~/.printer/plugins/codegraph/bin/codegraph`), `printer run` and
`printer exec` automatically launch a `codegraph watch` daemon for the
duration of the operation so the on-disk index stays fresh as the agent
edits files. Logs land in `.printer/codegraph-watch.log`. Pass
`--no-codegraph-watch` to opt out.

## How it works

`printer run` materializes the spec as **tasks in the on-disk task store**
(see `## Task tracking` below), then drives the agent through that queue:

1. **Sync** — the spec is parsed into a flat list of checklist items. Each
   item becomes a task in `.printer/tasks/T-NNN.md`. Re-runs are idempotent
   (each item has a stable anchor); already-done items are created with
   `status = done`.
2. **Bootstrap (only if needed)** — if the spec contains no checklist items
   at all, the agent gets one turn to write a `- [ ]` checklist back into the
   spec, then the driver re-syncs.
3. **Execute loop** — each turn the agent runs `printer task ready`, claims
   the top item with `printer task start <ID>`, does the work, comments as
   it goes, and finishes with `printer task done <ID>`.
4. **Compaction** — when cumulative input tokens cross `--compact-at`,
   `printer` mints a new session id and starts a fresh session. The new
   session just reads the task store; nothing is lost.
5. **Termination** — done when every task is `done`, blocked when the agent
   emits `<<BLOCKED: ...>>`, or aborted after `--max-turns` or 3 stalls in a
   row (a stall = no task transitioned this turn).

The task store is the source of truth for status. The original spec stays as
the human-authored intent; you can re-edit it and re-run to add new items.

## Spec format

A spec is a plain markdown file. The driver only cares about checklist items
at column 0. Everything else (headings, paragraphs, indented bullets that are
not checklist lines, etc.) is preserved as documentation but does not affect
the task graph.

### Canonical form

```markdown
# Project: anything you want here

Optional preamble. Anything before the first checklist item is context for
humans and is ignored by the driver.

## Tasks

- [ ] Short imperative title for one unit of work
  Optional indented description, 2-space indented. Multiple lines are fine.
  Blank lines inside the description are preserved.

- [ ] Next task
  Description.

- [x] Pre-completed item — created in the task store with status = done
```

### Rules the parser applies

- **Task lines** are `- [ ]`, `- [x]`, `* [ ]`, `* [x]`, `+ [ ]`, `+ [x]` at
  **column 0**. (Capital `X` is also accepted.) The text after the checkbox
  becomes the task title.
- **Descriptions** are the lines following a task line, indented by **2
  spaces or one tab**. The leading indent is stripped before storing. Blank
  lines inside a description are kept.
- **Unindented non-task lines** (a heading, a paragraph) end the current
  task's description. They are not part of any task.
- **Content above the first task line** is project preamble — ignored.
- **`[x]` items** are created in the task store with `status = done`. On
  re-sync, an existing task is transitioned to `done` if the spec marks it
  `[x]` and the store still has it open.
- **Indented (sub-)checklists** are *not* parsed as separate tasks; they
  become description text under their parent. Keep one logical unit per
  top-level task.
- **Re-runs are idempotent.** Items are matched by a stable anchor derived
  from the spec path + title, so renaming an item creates a new task — if
  you want to rename without losing history, edit the task file directly.

### Tiny example

```markdown
- [ ] Create hello.txt containing "hello"
- [ ] Create bye.txt containing "bye"
```

```sh
printer run hello.md
```

`printer` will create `T-001` and `T-002` in `.printer/tasks/`, then drive
`claude` to claim and finish each one.

## Examples

### 0. Start a new spec from a template

```sh
printer init                       # fresh repo: writes ./spec.md
printer init plans/auth.md -t "Auth refactor"
printer init feat-deploy-assets    # initialized repo: writes specs/NNN-feat-deploy-assets.md
```

`init` writes a starter spec in the canonical format (heading, preamble,
example checklist items, an HTML-comment cheatsheet for the format). Refuses
to overwrite an existing file unless you pass `--force`. Parent directories
are created if missing.

The argument is interpreted based on the repo state:

- **Fresh repo** (no `.printer/` yet): treated as a path. Defaults to
  `spec.md`.
- **Initialized repo** (`.printer/` already present from a prior run):
  treated as a slug, and the spec is written to `specs/NNN-<slug>.md` with
  `NNN` auto-incremented past the highest existing numbered spec. The slug
  is required in this mode.

### 1. Run with a non-trivial spec in another directory

```sh
printer run ~/specs/refactor-auth.md --cwd ~/code/myapp
```

`--cwd` is the working directory the child agent runs in. The spec path is
resolved to an absolute path before being handed to the agent, so it doesn't
need to live inside `--cwd`.

### 2. Pick a model and cap turns

```sh
printer run plan.md --model opus --max-turns 20
```

### 3. Force aggressive compaction

```sh
printer run plan.md --compact-at 50000
```

Rotates to a fresh session every time cumulative input tokens cross 50k. Useful
on long runs to keep individual turns cheap.

### 4. Watch it work

```sh
printer run plan.md --verbose
```

Adds per-turn timing/token-count heartbeats and, when stderr is a TTY, a
braille-spinner animation that ticks while the child agent is running. On
non-TTY stderr (logs, CI), `--verbose` falls back to a textual heartbeat every
~10 seconds so you can still confirm the process is alive.

### 5. Review the result against the spec

After a `run`, ask a fresh agent session to grade the work:

```sh
printer review plan.md --base main
```

Output is a concise markdown report (Verdict / Per-item findings / Out-of-scope
changes / Suggested follow-ups) printed to stdout. Add `--out review.md` to
also write it to a file.

If `--base` is omitted, `printer` tries `main`, then `master`, then `HEAD~1`.

### 6. Use codex, amp, or opencode instead of claude

```sh
printer run plan.md --agent codex
```

```sh
printer run plan.md --agent amp
```

```sh
printer run plan.md --agent opencode
```

(Note: amp and opencode support are best-effort. Amp uses its
Claude-compatible stream JSON mode; opencode token-based compaction is disabled,
so only `--max-turns` will bound opencode runs.)

## Flags

### `printer run <SPEC>`

| Flag | Default | Meaning |
| --- | --- | --- |
| `--agent claude\|codex\|amp\|opencode` | `claude` | Which agent binary to drive. |
| `--model <name>` | (agent default) | Forwarded to the agent. |
| `--max-turns <N>` | `40` | Hard cap on execution turns. |
| `--compact-at <tokens>` | `150000` | Rotate session at this cumulative input-token count. |
| `--cwd <path>` | current dir | Working dir for the child agent. |
| `--permission-mode <mode>` | `bypassPermissions` | Forwarded to Claude as `--permission-mode`; for Codex, `bypassPermissions` maps to `--dangerously-bypass-approvals-and-sandbox`. Amp and opencode enforce permissions through their own config. Default bypasses prompts because there is no human at the keyboard during a driver run. |
| `-v`, `--verbose` | off | Live spinner + per-turn timing/token heartbeats on stderr. |

### `printer review <SPEC>`

| Flag | Default | Meaning |
| --- | --- | --- |
| `--agent claude\|codex\|amp\|opencode` | `claude` | Which agent to drive. |
| `--model <name>` | (agent default) | Forwarded to the agent. |
| `--base <git-ref>` | autodetected | Ref to diff against (`git diff <base>...HEAD`). |
| `--cwd <path>` | current dir | Working dir for the child agent. |
| `--out <path>` | — | Also write the review report to this path. |
| `--permission-mode <mode>` | `bypassPermissions` | Forwarded to Claude as `--permission-mode`; for Codex, `bypassPermissions` maps to `--dangerously-bypass-approvals-and-sandbox`. Amp and opencode enforce permissions through their own config. |
| `-v`, `--verbose` | off | Live spinner + heartbeat during the review turn. |

## Task tracking

`printer task ...` is a small file-based issue tracker, similar in spirit to
[beads](https://github.com/gastownhall/beads), designed so a long agent run
can survive crashes and be handed off between sessions. There is no database
— every task is one markdown file under `.printer/tasks/T-NNN.md`, with a
TOML frontmatter block. Files are inspectable, hand-editable, git-friendly,
and survive whatever the terminal does.

### Lifecycle example

```sh
printer task create "Refactor auth module" --priority 2 --labels auth
printer task create "Add session expiry tests" --depends-on T-001 --priority 3

printer task ready                 # T-001 (T-002 is gated on it)
printer task start T-001
printer task comment T-001 "found a circular import in middleware.rs"
printer task done T-001 --note "shipped, see PR #42"

printer task ready                 # now T-002
```

### Crash recovery and handoff

Because all state is on disk, a `kill -9` / terminal close / power loss leaves
the world in a sane state — a task you were working on is just stuck in
`in_progress` with your name on it.

```sh
# After the crash, inspect what was in flight:
printer task list --status in_progress --mine

# Either reclaim it as-is and keep going (no command needed — the file
# already says it's yours), or hand it off to someone else:
printer task release T-007
printer task start  T-007 --owner alice

# If your previous session is dead but a stale claim is in the way:
printer task start T-007 --force
```

### File format

```
+++
id = "T-001"
title = "Refactor auth module"
status = "open"               # open | in_progress | blocked | done
priority = 2                  # 1 (highest) – 5 (lowest)
created_at = "2026-04-26T19:30:00Z"
updated_at = "2026-04-26T19:35:00Z"
owner = "sam"                 # empty when unowned
labels = ["auth"]
depends_on = ["T-005"]
blocked_reason = ""
+++

The free-text body is yours. Conventionally it starts with a description and
ends with a `## Notes` section that `printer task comment` appends to.
```

### Subcommands

| Subcommand | Purpose |
| --- | --- |
| `task create <TITLE>` | Create a task. Flags: `--description -` (stdin), `--priority N`, `--depends-on T-001,T-002`, `--labels a,b`. |
| `task list` | Filterable table. Flags: `--status`, `--label`, `--owner`, `--mine`. |
| `task show <ID>` | Full detail dump including body. |
| `task ready` | Open tasks whose every `depends_on` is `done`. |
| `task start <ID>` | Claim it (status → `in_progress`, owner → `$USER`). `--owner` to override, `--force` to steamroll an existing claim. |
| `task done <ID>` | Mark complete. `--note "..."` appends to the body. |
| `task block <ID> --reason "..."` | Freeze with a reason; `unblock` reopens. |
| `task release <ID>` | Drop the claim and return the task to `open`. |
| `task comment <ID> "text"` | Append a timestamped line under `## Notes`. |
| `task depends <ID> --add T-002 --remove T-005` | Edit the dependency list. |

`--tasks-dir <path>` overrides the default location (`./.printer/tasks/`) on
any subcommand.

### Concurrency notes

Fresh-id allocation uses `O_EXCL`, so concurrent `printer task create`
invocations are race-free — twenty parallel creates produce twenty
contiguous, unique ids. Updates use atomic `rename(2)` but **do not**
compare-and-swap, so two simultaneous updates to the same task last-writer-win.
This is fine for the typical single-user case; if you need stronger
guarantees, take an external lock (e.g. `flock`) around your updates.

## Plugins

`printer` is a plugin host: external CLIs that live in the same ecosystem
can be installed once into a per-user data directory and then invoked as
`printer <plugin-name> <args>`.

State lives under `~/.printer/`:

```
~/.printer/
  plugins/
    <name>/
      plugin.toml      # name, version, binary path, source, install timestamp
      bin/<name>       # the installed binary
      src/             # cached source tree (kept so updates can `git pull`)
```

### Installing

A plugin can come from one of three places:

```sh
# 1. registered name → printer knows how to install it
printer add-plugin heyvm

# 2. a Rust crate from a git URL or a local path → cargo install
printer add-plugin https://github.com/Heyo-Computer/heyvm-rs
printer add-plugin path:/home/me/dev/heyvm

# 3. an arbitrary install command (vendor's own curl|sh installer, etc.)
printer add-plugin heyvm \
  --install-cmd "curl -fsSL https://heyo.computer/heyvm/install.sh | sh" \
  --binary '~/.local/bin/heyvm'

# extras
printer add-plugin heyvm --rev v0.3.0   # pin a git ref (cargo source only)
printer add-plugin heyvm --force        # reinstall over an existing one
```

Resolution order: explicit `--install-cmd` → `path:` prefix → registry name
→ git-URL heuristic. `printer add-plugin` refuses to clobber an installed
plugin without `--force`.

#### Cargo source (registry name, git URL, or `path:`)

1. `git clone` (or `git checkout <rev>`) into `~/.printer/plugins/<name>/src`,
   or use the local path directly.
2. `cargo install --path <src> --root ~/.printer/plugins/<name>`, producing
   `~/.printer/plugins/<name>/bin/<binary>`.
3. Write `plugin.toml` with the resolved version and source.

#### Shell installer (`--install-cmd` + `--binary`)

For plugins that ship their own installer — `curl … | sh`, `brew install`,
prebuilt-archive download scripts — printer just runs the command verbatim
and trusts it to land the binary at the path you provide. `~` in `--binary`
is expanded.

1. `sh -c "<your install command>"`.
2. Verify a regular file exists at `--binary`.
3. Best-effort detect version with `<binary> --version`.
4. Write `plugin.toml` recording the command and the resolved absolute
   binary path.

Use this when the plugin author already publishes an installer you trust;
printer doesn't move or symlink the binary, so subsequent `add-plugin
--force` re-runs the same command (idempotency is the installer's
responsibility).

### Listing

```sh
printer plugins
```

```
NAME         VERSION  SOURCE
heyvm        0.3.0    git https://github.com/Heyo-Computer/heyvm@a1b2c3d4
fake-plugin  0.1.0    path /home/me/dev/fake-plugin
```

### Invoking

Any subcommand `printer` doesn't recognize as built-in is forwarded to a
matching plugin:

```sh
printer heyvm up        # exec's ~/.printer/plugins/heyvm/bin/heyvm with `up`
```

On Unix this is a real `execve` — the plugin replaces `printer` in the
process tree, so signals and exit codes flow through cleanly.

If no such plugin is installed, `printer` reports it and exits non-zero
without falling through.

### Bundled registry

`printer` ships with a small list of well-known plugins so the bare name
just works:

| Name    | Installer                                                                | Purpose |
| ---     | ---                                                                      | --- |
| `heyvm` | shell — `curl -fsSL https://heyo.computer/heyvm/install.sh \| sh` → `~/.local/bin/heyvm` | Spin up agent VMs and dev-preview environments. |

To add more known plugins, edit `printer/src/plugins/registry.rs`. Registry
entries can be either `KnownInstaller::Cargo { git }` (clone-and-build) or
`KnownInstaller::Shell { command, binary }` (vendor installer).

### Limitations

- Only single-binary crates are supported in v1; multi-bin crates need a
  `--bin` flag (not implemented).
- No `remove-plugin` / `update` yet — for now, blow away
  `~/.printer/plugins/<name>/` and reinstall.
- Plugin binaries are not added to your shell `PATH` automatically; invoke
  them through `printer <name>`.

## Conventions used in prompts

`printer` instructs the agent to emit these literal sentinels on their own
line:

- `<<PLAN_READY>>` — bootstrap turn finished writing the plan.
- `<<ALL_DONE>>` — every checklist item is done.
- `<<BLOCKED: reason>>>` — the agent cannot proceed; `printer` aborts and
  surfaces the reason.

You generally don't need to think about these — they're internal to the
driver loop — but if you write your own spec template and want to short-circuit
the bootstrap step, you can have your spec already be a clean `- [ ]` checklist
and the bootstrap turn becomes a near no-op.
