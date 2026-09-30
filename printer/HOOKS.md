# Plugin hooks

Plugins are no longer just "an extra binary on `$PATH`-via-printer". They can
register **hooks** that fire at specific points in the `printer init` /
`printer exec` lifecycle. A single plugin can ship as many hooks as it wants;
the existing `printer <plugin-name> <args>` dispatch still works alongside.

There are two hook **types**:

- **CLI hooks** — printer runs an arbitrary shell command, with context
  variables interpolated. Use these to notify, log, post artifacts,
  precondition the workspace, etc.
- **Agent hooks** — printer enriches the prompt or available skills for
  the agent session that's about to run. Use these to register a Claude
  skill, force a slash-command-like instruction, or inject phase-specific
  guidance.

## Where hooks come from

Hooks live in installed plugin manifests at:

```
~/.printer/plugins/<plugin-name>/plugin.toml
```

Any plugin manifest may include `[[hooks]]` entries (see schema below). When
you run `printer init` or `printer exec`, every installed plugin's manifest is
read and its hooks are fired at the matching events.

## Hook events

Events fire in this order. `before_*` events run *before* the phase begins;
`after_*` events run *after* it ends, with `{exit_status}` set to either
`ok` or `err`.

### `before_init` / `after_init`

Fires around `printer init`. The spec file has just been written (or is about
to be); `.printer/tasks/` and the codegraph index get bootstrapped between
the two. Useful for:

- CLI: dropping additional config files into the new project, kicking off a
  one-off bootstrap.
- Agent: N/A (no agent runs during init).

### `before_exec` / `after_exec`

Outermost wrapping of `printer exec`. Fires once per invocation, before any
phase decision is made and after the whole pipeline finishes (success **or**
failure). Useful for:

- CLI: notify-on-start / notify-on-finish, push artifacts up to a server.
- Agent: N/A (no single agent session spans exec).

### `before_run` / `after_run`

Wraps the implementation phase (the `run::run` loop). `before_run` is the
right place to **augment every nudge prompt** in the run loop. Agent
hook contributions to `before_run` are folded into the per-turn nudge prompt
in `printer/src/prompts.rs::nudge_prompt`, so they apply for every turn until
the phase ends. Useful for:

- CLI: pre-/post-flight checks (lint, tests, branch state).
- Agent: pin coding-style guidance for the run loop, register a skill the
  implementer should consult, or inject "always run /lint after edits".

### `before_review` / `after_review`

Wraps the review phase (single agent turn). Agent hook contributions to
`before_review` are appended to the review prompt and skill list. Useful for:

- CLI: copy the produced report into a docs site, post the verdict to chat.
- Agent: load a custom security-review skill, force the reviewer to grade
  against an extra rubric.

## Hook schema

Inside a plugin manifest:

```toml
# Existing fields:
name = "..."
version = "..."
binary = "bin/..."
installed_at = "..."

[source]
type = "git" | "path" | "shell"
# ...

# NEW: hooks the plugin registers.

[[hooks]]
type = "cli"
event = "after_review"
command = "notify-slack '#releases' 'review for {spec} finished: {exit_status}'"
# Optional. How to react to a non-zero exit from the hook:
#   "fail"   — abort the printer run (default for before_* events)
#   "warn"   — log a warning, keep going (default for after_* events)
#   "ignore" — silent
on_failure = "warn"

[[hooks]]
type = "agent"
event = "before_review"
# Inject a slash-command-style instruction into the review prompt.
command = "Run /security-review and incorporate its findings into the review."

[[hooks]]
type = "agent"
event = "before_run"
# Path to a SKILL.md (or skill directory containing one). Resolved relative
# to the plugin's directory. The skill is exposed to the agent in the same
# way `printer review --skill` exposes review-time skills.
skill = "skills/our-coding-style/"
```

A single `[[hooks]]` entry must specify exactly one of `command` (CLI or
agent) or `skill` (agent only).

## Available `{variables}` for interpolation

Substituted into CLI `command` strings and agent `command` text:

| Variable          | Available in                          | Meaning                                   |
|-------------------|---------------------------------------|-------------------------------------------|
| `{cwd}`           | all events                            | Working directory printer is operating in |
| `{spec}`          | all events except `before_init`       | Absolute path to the spec file            |
| `{event}`         | all events                            | The event name (`before_run`, …)          |
| `{phase}`         | run / review events                   | `run` or `review`                         |
| `{exit_status}`   | `after_*` events                      | `ok` or `err`                             |
| `{base_ref}`      | review events                        | Git ref used for the review diff          |
| `{report_path}`   | `after_review` (when `--out` set)    | Path where the review report was written  |

Unknown `{vars}` are left in place (so a hook command that uses `{name}` for
its own templating won't be mangled).

## CLI environment

CLI hooks are spawned via `sh -c "<command>"` in the working directory. They
inherit printer's environment plus the following injected vars (mirrors the
interpolation table above; useful when you'd rather read env than template):

```
PRINTER_HOOK_EVENT=after_review
PRINTER_HOOK_PHASE=review
PRINTER_HOOK_CWD=/abs/path
PRINTER_HOOK_SPEC=/abs/path/spec.md
PRINTER_HOOK_EXIT_STATUS=ok
PRINTER_HOOK_BASE_REF=main
PRINTER_HOOK_REPORT_PATH=/abs/path/review.md     # if --out was set
PRINTER_PLUGIN=my-plugin                          # which plugin owns this hook
```

## Authoring a plugin

A plugin's source crate may include an optional `printer-plugin.toml` at
its root. When `printer add-plugin` installs the plugin, this file is read
and its contents are merged into the installed `plugin.toml` and copied
into the install directory. Without this file, plugins install with no
hooks and no extra assets — the historical behaviour.

```toml
# plugins/<name>/printer-plugin.toml

# Files or directories (relative paths only, no `..`) to copy verbatim
# from the source root into the installed plugin dir alongside the
# binary. Hook fields like `skill = "skills/foo/SKILL.md"` resolve
# against the install dir, so anything they reference must be listed here.
assets = ["skills"]

# Hooks have the same schema as `[[hooks]]` in the installed manifest;
# they are validated at install time and refused if malformed.

[[hooks]]
type = "cli"
event = "before_run"
command = "codegraph index"
on_failure = "warn"

[[hooks]]
type = "agent"
event = "before_run"
skill = "skills/codegraph-search/SKILL.md"

[[hooks]]
type = "agent"
event = "before_run"
command = "Prefer `codegraph` over grep + Read for navigation."
```

Notes:

- **Source dir requirement** — `printer-plugin.toml` is only read for
  plugins installed via `path:` or a git URL (cargo crates with a real
  source tree). Shell-installer plugins (`--install-cmd`) have no source
  dir under printer's control, so they cannot ship hooks via this
  mechanism; their hooks must be hand-edited into `plugin.toml`.
- **`cargo install` discards non-binary files** — the only reason your
  `skills/` directory survives the install is the `assets` list. Forget
  to include it and the hook's `skill = "..."` paths will dangle.
- **Validation is strict** — install fails if a hook references an
  unknown event or violates the type/command/skill constraints. Fix the
  source manifest and re-run with `--force`.
- **Path safety** — assets must be relative paths without `..`
  components. Symlinks inside an asset directory are refused. The install
  refuses to clobber pre-existing files in the install dir.

## Listing what's wired up

```
printer hooks list             # all hooks across all installed plugins
printer hooks list --event after_review
```

Shows event, plugin name, kind, and resolved command/skill for each hook.

## Sandbox drivers

In addition to `[[hooks]]`, a plugin manifest may declare a `[driver]` block.
A driver is the plugin role that lets printer dispatch the agent inside an
isolated environment — typically a heyvm worktree — instead of the host cwd.

Only one driver runs at a time. If exactly one plugin contributes a driver
the default `sandbox.driver = "auto"` picks it. If multiple plugins contribute
drivers, printer will refuse to run until you set `sandbox.driver` in
`~/.printer/config.toml` (see "Global config" below) to pick between them.

### Schema

```toml
[driver]
kind = "vm"

# Provision the sandbox. Must print the handle (id / name / path) on stdout —
# printer captures stdout and stores it as `{handle}` for subsequent steps.
# heyvm's normal output is multi-line, so we redirect it to stderr and echo
# the deterministic slug ourselves.
create = "heyvm create --name printer-{spec_slug} --image {base_image} --no-ttl --needs-network --mount {cwd}:/workspace --mount $HOME/.claude:$HOME/.claude >&2 && echo printer-{spec_slug}"

# Wrap each child agent invocation. The `{child}` placeholder is required;
# printer shell-quotes the agent's argv token-by-token and substitutes it
# in. The whole template runs under printer's outer `sh -c`, which parses
# those tokens back into argv. **Do not add another `sh -c` around
# `{child}`** — that would double-shell and pass the agent's flags as
# positional parameters to the inner sh instead of to the agent. `--session
# printer` keeps cwd and exported env consistent across consecutive
# `enter` calls.
enter = "heyvm exec {handle} --session printer --env IS_SANDBOX=1 -- {child}"

# (Optional) sync_in / sync_out push/pull the host cwd to/from the sandbox.
# The bundled heyvm plugin omits both because cwd is bind-mounted via
# `--mount` at create time, so file edits round-trip live and there is
# nothing to copy. Drivers backed by transports that *do* need an explicit
# copy step (e.g. an SSH-only remote) would set them.

# (Optional) Tear the sandbox down. Runs from `Drop`, so it fires on panic
# and on early returns too. Failures are logged and swallowed — sync_out and
# destroy are best-effort cleanup.
destroy = "heyvm rm -y {handle}"

# (Optional) Preflight script run *inside* the sandbox right after `create`
# succeeds. printer wraps it through `enter` automatically. Failure here
# tears the sandbox down and aborts the run; use shell short-circuits
# (`|| true`) if you want a step to be best-effort. The bundled heyvm
# manifest uses this to `cd /workspace` once so subsequent `enter` calls
# inherit cwd via the persistent `--session`.
post_create = "cd /workspace"
```

### Lifecycle

For `printer exec`, the sandbox covers both phases — one `create` per exec,
one `destroy` at the end:

1. `create`   — once, at the top of `exec` (or `run` / `review` standalone).
2. `sync_in`  — once, after create.
3. `enter`    — wraps every agent turn for both run and review phases.
4. `sync_out` — once, after both phases finish.
5. `destroy`  — fires from the sandbox guard's `Drop`.

`printer run` and `printer review` invoked directly (without `exec`) each
manage their own create/destroy lifecycle.

### `{variables}` for driver templates

| Variable       | Available in              | Meaning                                          |
|----------------|---------------------------|--------------------------------------------------|
| `{cwd}`        | all steps                 | Working directory printer is operating in        |
| `{spec}`       | all steps                 | Absolute path to the spec file                   |
| `{spec_slug}`  | all steps                 | Spec basename, sanitized for use as a sandbox name (alphanumerics, `-`, `_` only) |
| `{base_image}` | all steps                 | `sandbox.base_image` from `~/.printer/config.toml` |
| `{handle}`     | all steps after `create`  | Whatever `create` printed on stdout              |
| `{child}`      | `enter` only (required)   | Shell-quoted argv of the wrapped agent command   |

### Skipping the sandbox

`printer run`, `printer review`, and `printer exec` all accept `--no-sandbox`
to dispatch on the host even when a driver is installed. Useful for local
debugging when the driver itself is misbehaving.

## Global config

User-level preferences live in `~/.printer/config.toml`. The file is optional;
anything you omit falls back to built-in defaults. Use `printer config show`
to print the resolved values, and `printer config edit` to open the file in
`$EDITOR` (it is seeded from a default template if missing).

```toml
[sandbox]
# Which driver-contributing plugin to dispatch through.
#   "auto" — pick the only installed driver (errors if more than one).
#   "off"  — never sandbox, even if a driver is installed.
#   "<plugin-name>" — pick a specific driver by plugin name.
driver = "auto"

# Forwarded to the driver's templates as {base_image}. For the bundled heyvm
# plugin this is the image string passed to `heyvm create --image …` (e.g.
# "ubuntu:24.04", "alpine:3.19", or any image heyvm knows about).
base_image = "ubuntu:24.04"

# Names of env vars to forward into the sandbox. Driver-specific.
env = []

# Extra read/write mounts (host:guest), beyond cwd which is mounted by default.
mounts = []

# Per-step overrides on top of the active driver's manifest. Any key you set
# here replaces that step's template; anything you omit falls through to the
# plugin's default. Same `{var}` interpolation as the plugin's [driver] block.
[sandbox.commands]
# create  = "heyvm create --name printer-{spec_slug} --image {base_image} --no-ttl --needs-network --mount {cwd}:/workspace --mount $HOME/.claude:$HOME/.claude >&2 && echo printer-{spec_slug}"
# enter   = "heyvm exec {handle} --session printer --env IS_SANDBOX=1 -- {child}"
# destroy = "heyvm rm -y {handle}"
# sync_in / sync_out are unset for the heyvm driver: cwd is bind-mounted at
# create time, so file edits round-trip live.

# Optional preflight inside the sandbox, run right after `create`. Wrapped via
# `enter`. Failure aborts the run; use shell short-circuits to make a step
# best-effort.
# post_create = "bash -lc 'cargo fetch || true'"
```

The override merge happens before any sandbox is created, and the merged
spec is re-validated — so a config typo (`enter` missing `{child}`, an empty
`create`) fails fast with a clear error rather than silently breaking the run.

`--no-sandbox` on the CLI is equivalent to `sandbox.driver = "off"` for the
duration of one command.

## ACP agents

`printer run` / `review` / `exec` / `plan` / `spec-from-followups` accept
`--agent acp` alongside the existing `claude` and `opencode` choices. ACP is
the [Agent Client Protocol](https://agentclientprotocol.com) — a long-lived
JSON-RPC 2.0 stdio transport spoken by agents like `claude-code-acp` and
Poolside, where one process serves the whole printer session instead of being
re-spawned per turn.

### Selecting an ACP server

Two ways to point printer at the server binary:

1. **Inline flags** — pass `--acp-bin <command>` (and optionally repeated
   `--acp-arg <arg>` to append extra argv tokens):

   ```
   printer run spec.md --agent acp --acp-bin claude-code-acp
   ```

2. **Plugin-contributed agent** — an installed plugin may declare one or
   more `[[agent]]` blocks that name a launch command. Pick one by name:

   ```
   printer run spec.md --agent acp:poolside
   ```

   `--acp-bin` (if also passed) overrides the manifest's `command`;
   `--acp-arg` tokens are appended after the manifest's `args`.

#### Plugin manifest schema

Inside a plugin manifest (or the `printer-plugin.toml` shipped at the source
root) declare one block per agent:

```toml
[[agent]]
kind = "acp"
# Lookup name. Must be unique across every installed plugin's manifests.
# Reserved names (`acp`, `claude`, `codex`, `amp`, `opencode`) are refused at install time
# because they would shadow the built-in --agent choices.
name = "poolside"
# Launch command (binary on $PATH, or absolute path). Required.
command = "pool"
# Argv tokens appended to `command`. Optional.
args = ["acp"]
# Env vars passed to the spawned child. Values are taken as literal strings
# — no shell expansion. Optional.
env = { POOLSIDE_LOG = "info" }
```

`printer plugins` shows `agent` in the `ROLES` column for any plugin that
contributes at least one `[[agent]]` block.

#### Worked example: bundled `plugins/poolside/`

The repo ships a reference ACP plugin at `plugins/poolside/`. Its
`printer-plugin.toml` is exactly the schema above — `command = "pool"`,
`args = ["acp"]`, plus a `before_run` agent skill (`skills/poolside/`)
that briefs the implementer to follow host-repo conventions instead of
imposing Poolside defaults. Install it with
`printer add-plugin path:plugins/poolside` and dispatch with
`printer run --agent acp:poolside <spec>`. See
`plugins/poolside/README.md` for the full install/use story and
prerequisites (the `pool` CLI must be on `$PATH`).

### Sandbox interaction

If a sandbox driver is active, the same `enter = "... {child}"` template that
wraps one-shot agents also wraps the ACP server launch — the long-lived child
runs inside the sandbox just like a per-turn child would. Skip with
`--no-sandbox` for host-side debugging.

### Streaming

While a `session/prompt` is in flight, every `session/update` notification
for the active sessionId is observed by the transport and routed two ways:
its text content is appended to the turn's `result_text`, and a one-line
log is emitted to stderr (prefix `[acp:agent]`). High-signal updates
(`tool_call`, `tool_call_update`, `plan`, and unknown `sessionUpdate`
kinds) always log; chunked `agent_message_chunk` / `agent_thought_chunk`
updates only log under `--verbose`, rate-limited to ~1 line/sec so a
server that streams a token at a time doesn't flood the log. A spinner
heartbeat in TTY mode and a 10-second textual heartbeat in pipe mode
print the most recent activity summary so a stalled server is visibly
stalled. Set `PRINTER_ACP_TRACE=1` to additionally dump byte-level
JSON-RPC line traces.

### Cancellation

Ctrl-C during an ACP turn fires a `session/cancel` notification (no
`id`, fire-and-forget per the ACP spec) so well-behaved servers can
resolve the in-flight `session/prompt` with `stopReason: cancelled` and
flush state cleanly. Printer waits up to 500 ms for that resolution; if
it doesn't land, the ACP server's whole process group is sent SIGKILL —
the child was spawned with `process_group(0)`, so `kill(-pgid, SIGKILL)`
reaches any grandchildren that would otherwise hold stdio pipes open.
The printer turn returns an interrupt error in either case, the ACP
client is dropped, and the next turn (or rotation) re-bootstraps a fresh
server. Compaction-by-rotation also calls `shutdown()` on the previous
ACP client before nulling it, so the prior server's pgid is reaped
instead of leaking until printer exits.

### Permission modes (advisory)

ACP doesn't standardize a permission policy — the server enforces its
own, and there's no spec'd field that maps cleanly to Claude's
`--permission-mode <mode>` flag. Printer therefore treats
`--permission-mode` as **advisory** when used with `--agent acp:*`:

| Mode                | Behavior                                    |
| ------------------- | ------------------------------------------- |
| `bypassPermissions` | no-op (server still applies its own policy) |
| `acceptEdits`       | no-op (advisory)                            |
| `default`           | no-op (advisory)                            |
| `plan`              | no-op (advisory)                            |

The value is surfaced two ways: as an env var `PRINTER_PERMISSION_MODE`
on the spawned ACP child (so a server *can* read it and adapt if it
chooses), and as a non-standard `_printerPermissionMode` key alongside
the spec'd `session/new` params (JSON-RPC servers must ignore unknown
fields, so this is harmless to non-aware servers). At session bootstrap
printer prints a one-line advisory:

```
[printer] acp: --permission-mode is advisory; the ACP server enforces its own policy (mode=…)
```

This is intentionally conservative — don't pretend a Claude-CLI flag
controls a Poolside (or any other ACP server's) sandbox. If you need
strict permission semantics, configure them on the server side.

### What's wired in this release

T-017 shipped the **blocking-turn** transport: `initialize` → `session/new`
→ `session/prompt` per turn, with all `session/update` text content blocks
concatenated into the agent's reply. T-020 layers on streaming logs
(see "Streaming" above), Ctrl-C cancellation (see "Cancellation"), and
the advisory permission-mode mapping documented above. Token usage is
still not surfaced (ACP doesn't standardize a usage shape), so the
compaction-by-rotation trigger will not fire mid-session for ACP
agents — that remains a follow-up.

## Backwards compatibility

- `printer add-plugin <spec>` works exactly as before. A plugin without
  `[[hooks]]` or `[driver]` in its manifest contributes nothing (and
  dispatches via `printer <name> <args>` as it always has).
- `printer plugins` keeps listing installed plugins; the table now shows a
  `ROLES` column distinguishing `bin` / `hooks` / `driver` contributions.
- The new hook and driver systems are purely additive: a manifest with no
  `[[hooks]]` and no `[driver]` recovers the old behaviour.
