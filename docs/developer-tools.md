# Developer tools

The hws repository ships three agent-facing CLIs (`printer`, `codegraph`, `computer`), a set of printer plugins, and reusable agent skills; this page covers installing and using them.

These tools are independent of the HWS services. They run on a developer
machine, not on a fleet host, and you can use them on any repository.

| Tool | What it does |
| --- | --- |
| [`printer`](#printer) | Drives a coding agent (Claude Code, Codex, Amp, OpenCode, or an ACP server) through a markdown spec: plan, execute, review, fix |
| [`codegraph`](#codegraph) | Tree-sitter code index, search, outline, snippet, and patch tool; also an MCP server |
| [`computer`](#computer) | Mouse, keyboard, screenshot, and window control for Wayland Linux and macOS; also an MCP server |
| [Plugins](#printer-plugins) | Extend printer with lifecycle hooks, skills, sandbox drivers, and ACP agents |
| [Skills](#skills) | `SKILL.md` files for heyvm and `git submit` |

## Install

### From source

You need a Rust toolchain. From the repository root:

```sh
make install                       # printer, computer, codegraph -> ~/.local/bin
make install PREFIX=/usr/local     # elsewhere (may need sudo)
make install-codegraph             # just one: install-printer, install-computer, install-codegraph
make uninstall
```

| Target | Meaning |
| --- | --- |
| `build`, `build-<crate>` | `cargo build --release` |
| `install`, `install-<crate>` | Build and copy to `$(PREFIX)/bin` |
| `check`, `test`, `clean` | Run across all three crates |
| `install-plugins` | `printer add-plugin path:plugins/codegraph --force` (needs `printer` on `PATH`) |
| `uninstall-plugins` | Remove `~/.printer/plugins/codegraph` |

`make install` copies from `<crate>/target/release/`, so unset
`CARGO_TARGET_DIR` when you run it.

### Prebuilt binaries

The root [`install.sh`](../install.sh) downloads a release tarball for
Linux or macOS (x86_64 or aarch64) and verifies its sha256:

```sh
curl -fsSL <release-url>/install.sh | sh
curl -fsSL <release-url>/install.sh | PRINTER_VERSION=v0.2.0 PRINTER_PREFIX=/usr/local sh
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `PRINTER_BASE_URL` | `https://github.com/Heyo-Computer/printer/releases` | GitHub releases endpoint |
| `PRINTER_VERSION` | `latest` | `latest` or a tag such as `v0.2.0` |
| `PRINTER_PREFIX` | `$HOME/.local` | Binaries land in `$PRINTER_PREFIX/bin` |
| `PRINTER_BINS` | `printer computer codegraph` | Subset to install |
| `PRINTER_NO_VERIFY` | | Non-empty skips sha256 verification |

The tarballs are built by `.github/workflows/build.yml`.

### Agent CLIs

printer drives an agent CLI that must already be installed and authenticated:
`claude`, `codex`, `amp`, or `opencode` on `PATH`, or an ACP server such as
`opencode acp` or Poolside's `pool acp`.

## printer

printer runs a coding agent against a markdown spec without a human at the
keyboard. It turns the spec's checklist into tasks on disk, drives the agent
turn by turn until every task is done, rotates to a fresh session when the
context fills, then has a second session review the diff against the spec and
fix what it finds.

### Quick start

```sh
printer init                         # writes ./spec.md from a template
$EDITOR spec.md
printer exec spec.md --verbose       # plan, run, review, fix
printer exec --continue              # resume after a crash or Ctrl-C
```

In a repository that already has `.printer/`, `printer init <slug>` writes
`specs/NNN-<slug>.md` with the next free number.

### Spec format

Only checklist lines at column 0 become tasks:

```markdown
# Project: auth refactor

Preamble for humans; ignored by the driver.

## Tasks

- [ ] Extract session handling into its own module
  Indented lines (2 spaces or a tab) are the task's description.

- [ ] Add expiry tests
- [x] Already done; created with status = done
```

- Task lines are `- [ ]`, `* [ ]`, or `+ [ ]` (and `[x]`/`[X]`) at column 0.
- Indented sub-checklists are description text, not separate tasks.
- Items are matched by a stable anchor from the spec path and title, so
  re-running is idempotent. Renaming an item creates a new task.
- If the spec has no checklist, the agent gets one turn to write one.

### How a run works

1. The spec is synced into `.printer/tasks/T-NNN.md`, one file per item.
2. Each turn the agent runs `printer task ready`, claims a task with
   `printer task start`, does the work, and closes it with `printer task done`.
3. When cumulative input tokens pass `--compact-at`, printer starts a fresh
   session. The new session reads the task store, so nothing is lost.
4. The run ends when every task is done, when the agent emits
   `<<BLOCKED: reason>>`, after `--max-turns`, or after 3 turns in a row with
   no task transition.

`exec` then runs `review`. A non-PASS verdict triggers a fix pass and another
review, up to `--max-review-passes` (default 3).

### Commands

| Command | Purpose |
| --- | --- |
| `printer init [PATH\|SLUG]` | Write a starter spec. `-t/--title`, `--force` |
| `printer plan <SPEC>` | Generate a plan without executing; writes `.printer/plan.checkpoint`. `--no-questions`, `--max-question-rounds` (default 3) |
| `printer run <SPEC>` | Plan and execute |
| `printer review <SPEC>` | Grade the working tree against the spec. `--base`, `--out`, `--skill`, `--no-ui-host` |
| `printer exec [SPEC]` | Run then review, with fix cycles and crash-safe `--continue` |
| `printer test <SPEC>` | Click-test a UI change with `computer` on the host. `--url`; exits non-zero unless PASS |
| `printer history` | Completed execs from `.printer/history.json`. `--json` |
| `printer spec complete\|cancel <SPEC>` | Mark a spec's exec checkpoint done or cancelled without running an agent |
| `printer spec-from-followups <SLUG>` | Turn a review's follow-ups (`.printer/followups/`) into `specs/NNN-<slug>.md`. `--from` |
| `printer task ...` | The task tracker (below) |
| `printer add-plugin`, `reinstall-plugin`, `plugins`, `hooks list` | Plugin management |
| `printer config show\|edit` | Show or edit `~/.printer/config.toml` |
| `printer <plugin> [args]` | Run an installed plugin's binary |

### Common flags

These apply to `run`, `exec`, `review`, `plan`, `test`, and
`spec-from-followups` unless noted.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--agent` | `claude` | `claude`, `codex`, `amp`, `opencode`, `acp`, or `acp:<name>` |
| `--model` | agent default | Passed to the agent |
| `--cwd` | current directory | Working directory for the agent |
| `--permission-mode` | `bypassPermissions` | Passed to Claude; for Codex, `bypassPermissions` maps to `--dangerously-bypass-approvals-and-sandbox`. Advisory for ACP |
| `-v`, `--verbose` | off | Spinner and per-turn heartbeats on stderr |
| `--max-turns` | `40` | `run`/`exec`: cap on execution turns |
| `--compact-at` | `150000` | `run`/`exec`: rotate the session at this many cumulative input tokens |
| `--base` | `main`, then `master`, then `HEAD~1` | `review`/`exec`/`test`: ref to diff against |
| `--out` | | `review`/`exec`: also write the report to a file |
| `--skill PATH` | auto-discovers `skills/` | `review`/`exec`/`test`: make a skill available. Repeatable |
| `--no-sandbox` | off | `run`/`review`/`exec`: run on the host even if a sandbox driver is installed |
| `--no-codegraph-watch` | off | `run`/`exec`: don't start a `codegraph watch` daemon |
| `--skip-plugin-check` | off | `run`/`exec`: skip the "no plugins installed" prompt (for CI) |
| `--commit-each-task` | off | `run`/`exec`: commit (excluding `.printer/`) each time tasks complete |
| `--push-each-task` | off | `run`/`exec`: push after each per-task commit |
| `--recursive` | off | `exec`: run each open task in its own sandbox (needs the heyvm plugin) |
| `--max-review-passes` | `3` | `exec`: cap on review/fix cycles; `1` disables fixing |
| `--acp-bin`, `--acp-arg` | | Launch command and extra args for an ACP server |

Codex, Amp, and OpenCode support is best-effort. OpenCode and ACP agents do not
report token usage, so only `--max-turns` bounds their runs.

### Task tracker

`printer task` is a file-based tracker the agent uses during a run, and you can
use directly. Each task is `.printer/tasks/T-NNN.md` with TOML front matter.

```sh
printer task create "Refactor auth module" --priority 2 --labels auth
printer task create "Add expiry tests" --depends-on T-001
printer task ready                     # open tasks whose dependencies are done
printer task start T-001               # claim: status in_progress, owner $USER
printer task comment T-001 "found a circular import"
printer task done T-001 --note "merged"
printer task list --status in_progress --mine
printer task release T-007             # drop a stale claim
printer task start T-007 --force       # take over a claim
```

| Subcommand | Flags |
| --- | --- |
| `create <TITLE>` | `-d/--description` (`-` for stdin), `-p/--priority` (1-5, default 3), `--depends-on`, `--labels` |
| `list` | `--status`, `--label`, `--owner`, `--mine` |
| `show`, `unblock`, `release` `<ID>` | |
| `ready` | |
| `start <ID>` | `--owner`, `--force` |
| `done <ID>` | `--note` |
| `block <ID>` | `--reason` |
| `comment <ID> <TEXT>` | |
| `depends <ID>` | `--add`, `--remove` |

`--tasks-dir` overrides `./.printer/tasks/` on any subcommand. Creates are
race-free; concurrent updates to the same task are last-writer-wins.

### State on disk

| Path | Contents |
| --- | --- |
| `.printer/tasks/` | One file per task |
| `.printer/exec/` | Per-spec exec checkpoints used by `--continue` |
| `.printer/history.json` | Completed execs |
| `.printer/followups/` | Follow-ups from reviews |
| `.printer/plan.checkpoint` | Output of `printer plan` |
| `.printer/codegraph-watch.log` | Log of the auto-started `codegraph watch` |
| `~/.printer/plugins/<name>/` | Installed plugins |
| `~/.printer/config.toml` | Global config (optional) |

### Global config

`~/.printer/config.toml` currently holds sandbox preferences:

```toml
[sandbox]
driver = "auto"            # "auto", "off", or a plugin name
base_image = "ubuntu:24.04"
env = []                   # env var names to forward into the sandbox
mounts = []                # extra host:guest mounts

[sandbox.commands]         # per-step overrides of the driver's templates
# create = "..."
# enter = "... {child}"
# destroy = "..."
# post_create = "..."
```

`printer config edit` seeds the file from a template. Overrides are
re-validated before any sandbox is created.

### UI review on the host

A sandbox has no display, so `computer` cannot click-test inside it. When a
standalone `printer review` sees a diff touching UI files and the host has a
display (`WAYLAND_DISPLAY`/`XDG_SESSION_TYPE` set and `/dev/uinput` present),
it runs on the host. `--no-ui-host` forces the sandbox. `exec` shares one
sandbox across run and review, so for UI work use `--no-sandbox` or a separate
`printer review`.

## codegraph

codegraph parses a repository with tree-sitter and answers structural
questions more cheaply than grep plus full-file reads. Supported languages:
Rust (`.rs`), Python (`.py`, `.pyi`), JavaScript (`.js`, `.mjs`, `.cjs`,
`.jsx`), and TypeScript (`.ts`, `.mts`, `.cts`, `.tsx`).

```sh
codegraph index                            # build .codegraph/index.json in the current directory
codegraph watch                            # re-index on file changes (foreground)
codegraph search handle_ --kind function --limit 20
codegraph definition Foo::bar
codegraph outline src/server.rs            # signatures only
codegraph snippet src/server.rs handle_request
codegraph snippet src/server.rs --lines 120:180
codegraph references handle_request
codegraph patch src/server.rs --diff change.patch --check
```

| Command | Arguments and flags |
| --- | --- |
| `index [PATH]` | `--force` to rebuild from scratch |
| `watch [PATH]` | `--debounce-ms` (default 300) |
| `symbols <FILE>` | All symbols in one file |
| `outline <FILE>` | Hierarchical outline, no bodies |
| `snippet <FILE> [SYMBOL]` | Or `--lines start:end` (not both) |
| `search <QUERY>` | `--kind`, `--name` (name only), `--limit` (default 50) |
| `definition <SYMBOL>` | Exit code 1 if not found |
| `references <SYMBOL>` | Lexical word-boundary scan; may include comments and strings |
| `patch <FILE>` | `--diff PATH` (else stdin), `--check`, `--allow-outside` |
| `mcp` | Serve read-only tools over stdio |

- Output is JSON by default. The global `--text` flag switches every command
  to compact tab-separated text.
- `search`, `definition`, and `references` read the index for the current
  directory and fail if you have not run `codegraph index`.
- `patch` takes a unified diff with at least 3 lines of context, refuses files
  outside the working directory unless `--allow-outside`, and exits non-zero
  on failure.
- `index` and `watch` skip `.git`, `target`, `node_modules`, `dist`, `build`,
  and similar directories, and honour `.gitignore`.
- Kinds for `--kind`: `function`, `method`, `class`, `struct`, `enum`,
  `trait`, `interface`, `module`, `type`, `constant`, `variable`.

`codegraph mcp` exposes `search`, `definition`, `outline`, `snippet`, and
`references` as MCP tools over stdio. Mutating commands are not served. Start
it with the repository root as the working directory:

```sh
claude --mcp-config '{"mcpServers":{"codegraph":{"type":"stdio","command":"codegraph","args":["mcp"]}}}'
```

printer wires this up automatically for the Claude backend when `codegraph` is
on `PATH`, and starts `codegraph watch` for the duration of `run` and `exec`.

## computer

computer gives agents mouse, keyboard, screenshots, and window lists. It
targets the active Wayland session on Linux and the desktop on macOS.

```sh
computer outputs --json
computer windows --json
computer screenshot -o /tmp/desk.png           # or --file; stdout if omitted
computer screenshot --output DP-1 -o shot.png
computer mouse move 960 540
computer mouse click --button right --count 2
computer mouse scroll 0 5
computer key tap Return
computer key chord ctrl+shift+t
computer type --delay-ms 30 "hello"
computer browse https://example.com
computer sleep 250
```

| Command | Arguments and flags |
| --- | --- |
| `outputs`, `windows` | `--json` (human text otherwise) |
| `screenshot` | `--output NAME` (default first output), `-o/--file PATH` (default stdout) |
| `mouse move <X> <Y>` | `--output NAME` |
| `mouse move-rel <DX> <DY>` | |
| `mouse click` | `--button left\|right\|middle\|side\|extra` (default left), `--count` (default 1) |
| `mouse down`, `mouse up` | `--button` |
| `mouse scroll <DX> <DY>` | Positive y scrolls down |
| `key tap\|down\|up <KEY>` | |
| `key chord <COMBO>` | e.g. `ctrl+c`, `cmd+space` |
| `type <TEXT>` | `--delay-ms` (default 8) |
| `browse <URL>` | Opens in the default browser |
| `sleep <MS>` | |
| `mcp` | Serve the desktop tools over stdio |

Platform differences:

- Coordinates are pixels on Linux and points on macOS.
- `--output` is a `wl_output` name (`HDMI-A-1`) on Linux and
  `display-<id>` on macOS; list them with `computer outputs`.
- On Linux, typing uses a US keymap. On macOS any Unicode text works.
- On macOS, grant the binary Accessibility and Screen Recording permissions.
  Sign it ad hoc once (`codesign --force --sign - <path>`) so rebuilds keep the
  grant.

`computer mcp` exposes `screenshot` (inline PNG, long edge downscaled to 1568 px
unless `max_width` is given), `outputs`, `windows`, `mouse_move`,
`mouse_click`, `mouse_scroll`, `mouse_drag`, `key`, `type`, and `browse`.
printer wires it up for the Claude backend when a display is present and the
run is not sandboxed.

## printer plugins

A plugin is a directory with a `printer-plugin.toml` (and optionally a Rust
crate). Installing copies it to `~/.printer/plugins/<name>/`. Plugins can
contribute:

- **CLI hooks** — shell commands run at lifecycle events.
- **Agent hooks** — prompt text or a skill injected into the agent session.
- **A sandbox driver** — templates for creating, entering, and destroying an
  isolated environment for the agent.
- **ACP agents** — named ACP servers selectable with `--agent acp:<name>`.
- **A binary** — run as `printer <name> <args>`.

### Managing plugins

```sh
printer add-plugin path:plugins/codegraph            # local directory
printer add-plugin https://github.com/<org>/<repo> --subdir plugins/heyvm --rev main
printer add-plugin heyvm                             # registry name
printer add-plugin mytool --install-cmd "curl -fsSL https://... | sh" --binary ~/.local/bin/mytool
printer plugins                                      # list, with a ROLES column
printer hooks list --event before_run
printer reinstall-plugin codegraph                   # refresh from recorded source
printer reinstall-plugin --all
```

`add-plugin` refuses to replace an installed plugin without `--force`. `path:`
is resolved against the current directory. A directory with a `Cargo.toml` is
built with `cargo install`; one without is installed as a skill-only plugin.
There is no remove command; delete `~/.printer/plugins/<name>/`.

### Bundled plugins

| Plugin | Contributes | Install |
| --- | --- | --- |
| `codegraph` | `before_run`: `codegraph index`, a "prefer codegraph" instruction, and the codegraph-search/edit skills; `before_review`: codegraph-search skill | `printer add-plugin path:plugins/codegraph` |
| `computer` | computer skill on `before_run` and `before_review` | `printer add-plugin path:plugins/computer` |
| `heyvm` | Sandbox driver running each agent turn in a heyvm VM, plus a sandbox skill | `printer add-plugin path:plugins/heyvm` (needs `heyvm` on `PATH`) |
| `acp-runtime` | Shared skill for ACP-driven agents | `printer add-plugin path:plugins/acp-runtime` |
| `opencode` | ACP agent `opencode-acp` (`opencode acp`) plus a skill | `printer add-plugin path:plugins/opencode`, then `--agent acp:opencode-acp` |
| `poolside` | ACP agent `poolside` (`pool acp`) plus a skill | `printer add-plugin path:plugins/poolside`, then `--agent acp:poolside` |
| `printer-docs` | A skill describing printer itself | Skill directory only |

Integrations for other agent hosts, not installed through printer:

| Directory | For | Install |
| --- | --- | --- |
| `plugins/codegraph-claude` | Claude Code: `/cg-*` commands, codegraph skills, SessionStart/End hooks that manage `codegraph watch` | `claude --plugin-dir <abs path>/plugins/codegraph-claude` |
| `plugins/codegraph-opencode` | OpenCode: a `codegraph` agent and `/cg-*` commands; disables built-in read/edit/write | Copy `agent/` and `command/` into `.opencode/` or `~/.config/opencode/`, merge `opencode.json` |
| `plugins/pi` | pi: `codegraph_*` and `computer_*` tools, skills, and index management | `pi install <abs path>/plugins/pi` |

The registry name `heyvm` installs only the heyvm CLI via its vendor installer.
To get the sandbox driver, also install the plugin directory.

### Hooks

Hooks are declared in `printer-plugin.toml`:

```toml
assets = ["skills"]          # copied into the install dir; skill paths resolve against it

[[hooks]]
type = "cli"
event = "after_review"
command = "notify '{spec} reviewed: {exit_status}'"
on_failure = "warn"          # fail | warn | ignore

[[hooks]]
type = "agent"
event = "before_run"
skill = "skills/our-style/SKILL.md"
```

Events, in order: `before_init`/`after_init`, `before_exec`/`after_exec`,
`before_run`/`after_run`, `before_review`/`after_review`. A hook has exactly
one of `command` or `skill` (`skill` is agent-only). `on_failure` defaults to
`fail` for `before_*` and `warn` for `after_*`.

Template variables: `{cwd}`, `{spec}`, `{event}`, `{phase}`, `{exit_status}`,
`{base_ref}`, `{report_path}`. CLI hooks run under `sh -c` in the working
directory and also receive `PRINTER_HOOK_<NAME>` environment variables and
`PRINTER_PLUGIN`.

### Sandbox drivers

```toml
[driver]
kind = "vm"
create = "heyvm create --name printer-{spec_slug} --image {base_image} ... >&2 && echo printer-{spec_slug}"
enter = "heyvm exec {handle} --session printer --env IS_SANDBOX=1 -- {child}"
destroy = "heyvm rm -y {handle}"
post_create = "cd /workspace"
```

`create` must print the handle on stdout. `enter` must contain `{child}` and
must not wrap it in another `sh -c`. `sync_in`/`sync_out` are optional (the
heyvm driver bind-mounts the working directory instead). `destroy` runs on
drop, including on panic. Variables: `{cwd}`, `{spec}`, `{spec_slug}`,
`{base_image}`, `{agent_setup}`, `{handle}`, `{child}`.

If more than one installed plugin has a driver, set `sandbox.driver` in
`~/.printer/config.toml`.

### ACP agents

```toml
[[agent]]
kind = "acp"
name = "poolside"            # unique; acp, claude, codex, amp, opencode are reserved
command = "pool"
args = ["acp"]
env = { POOLSIDE_LOG = "info" }
```

Select it with `--agent acp:poolside`, or skip the plugin with
`--agent acp --acp-bin <cmd>`. ACP servers ignore `--permission-mode`; printer
passes it only as `PRINTER_PERMISSION_MODE`. Set `PRINTER_ACP_TRACE=1` for
JSON-RPC traces.

The full schema is in [`printer/HOOKS.md`](../printer/HOOKS.md).

## Skills

The top-level `skills/` directory holds two agent skills that defer to each
CLI's own `--help` as the source of truth:

| Skill | Covers |
| --- | --- |
| [`skills/heyvm`](../skills/heyvm/SKILL.md) | heyvm login, local and cloud sandboxes, exec, deployments, ports and sharing, databases, images, backend health |
| [`skills/git-submit`](../skills/git-submit/SKILL.md) | Installing and upgrading the `git submit` client, submitting, submodules, inspecting and cleaning up CI runs |

`printer review` and `printer test` auto-discover a `skills/` directory in the
agent's working directory. Plugin-specific skills live under
`plugins/*/skills/`. To add these skills to another agent host:

```sh
npx skills add heyo-computer/printer
```

## Embedding in an app

The three CLIs are meant to be called as subprocesses: read JSON from stdout,
stream progress from stderr, and read durable state from `.printer/`.
[`INTEGRATION.md`](../INTEGRATION.md) describes the contract for desktop apps.
