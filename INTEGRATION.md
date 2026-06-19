# Embedding printer / codegraph / computer in a desktop app

The three tools are plain CLIs installed to `~/.local/bin` (`make install`).
A desktop app integrates them as **subprocesses** — there is no library/FFI
surface and no long-lived server to manage (except the optional codegraph
`watch` daemon and `mcp` server, both of which you spawn and own). Capture
stdout for results (JSON by default where applicable), stream stderr for live
progress, and read the durable files under `.printer/` for structured state.

## Install / detect

```sh
make install            # printer, codegraph, computer → ~/.local/bin
```

At startup, probe for each binary on `PATH` and degrade gracefully:

| tool       | required for                          | also needs                                  |
|------------|---------------------------------------|---------------------------------------------|
| `printer`  | running specs (the orchestrator)      | an agent CLI on PATH (`claude` or `opencode`) |
| `codegraph`| code search / patch / MCP tools       | a built index (`codegraph index`)           |
| `computer` | desktop input + screenshots (Linux/mac)| Wayland session **and** `/dev/uinput` (Linux) |

---

## codegraph — embed as MCP tools (recommended)

The cleanest integration: hand codegraph to your app's own agent as a native
MCP server instead of shelling out per query.

```jsonc
// MCP stdio server: newline-delimited JSON-RPC 2.0
{
  "mcpServers": {
    "codegraph": { "type": "stdio", "command": "codegraph", "args": ["mcp"] }
  }
}
```

- Launch it with **cwd set to the repo root** — every tool reads
  `.codegraph/index.json` relative to cwd.
- Use the **absolute** binary path in `command` if your app's working
  directory isn't predictable (and to stay valid inside sandbox VMs).
- Read-only tools exposed: `search`, `definition`, `outline`, `snippet`,
  `references`. Mutating ops (`patch`/`index`/`watch`) are **not** served —
  call those via the CLI.
- Build/refresh the index before first use: `codegraph index`. For a live app,
  spawn `codegraph watch` (foreground daemon) and supervise it; it re-indexes
  on file changes.

Direct CLI use (no MCP) is also fine — default output is JSON, `--text` gives
compact tab-separated lines:

```sh
codegraph search Foo --kind struct --limit 20   # JSON to stdout
codegraph outline src/lib.rs                     # signatures only
codegraph snippet src/lib.rs handle_request      # one symbol's source
codegraph patch src/lib.rs --diff /tmp/x.patch   # apply a unified diff
```

Exit code is non-zero on failure.

---

## printer — drive a spec as a background job

```sh
printer init feat-x                 # writes specs/NNN-feat-x.md, .printer/, .codegraph/
printer exec specs/001-feat-x.md --verbose   # run + review in one shot
```

Run `exec` (or `run`/`review`/`test`) as a long-lived child process from your
app. It is the equivalent of `printer run <spec> && printer review <spec>`.

**Progress (live):** pass `--verbose` and stream **stderr** — it emits
`[printer] turn N …`, per-tool activity (`[agent] ⚙ …`), token usage, and the
codegraph guardrail warnings, line by line.

**State (durable, poll these for UI):** all under the project's `.printer/`:

| file                     | shape                                                            |
|--------------------------|------------------------------------------------------------------|
| `exec.json`              | `{ spec, phase, started_at, updated_at }` — current job status   |
| `tasks/T-*.md`           | one markdown file per task (status in frontmatter) — the plan    |
| `metrics.jsonl`          | NDJSON rows: per-phase (`run`/`review`/`exec-total`) and per-turn (`turn`, with `raw_search_calls`/`codegraph_calls`) token + tool stats |
| `history.json`           | archive of completed execs                                       |
| `followups/*.md`         | follow-up specs written by review                                |
| `codegraph-watch.log`    | output of the auto-spawned watch daemon                          |

**Crash safety:** `printer exec --continue` resumes from the `exec.json`
checkpoint, so your app can relaunch a job after a restart.

**Exit codes:** non-zero on failure or a non-PASS review/test verdict — usable
as a gate.

**Agent backend:** printer needs `claude` or `opencode` on PATH. Select with
`--agent claude` (default) / `--agent opencode` / `--agent acp:<name>` for
persistent ACP sessions. codegraph is auto-wired into the Claude backend as MCP
tools when the `codegraph` binary is present.

---

## computer — desktop control (the app's "hands and eyes")

```sh
computer outputs                         # list monitors (JSON)
computer windows                         # list toplevel windows (JSON)
computer screenshot -o /tmp/d.png        # or `computer screenshot > out.png`
computer mouse move 960 540 && computer mouse click
computer key chord "ctrl+shift+t"
computer type "hello"
```

**Embed as MCP tools (recommended for agents).** `computer mcp` serves the
desktop commands over stdio JSON-RPC — and `screenshot` returns an **inline PNG
image** content block, so an agent sees the pixels in the same turn (no temp
file + read hop). Wire it like codegraph:

```jsonc
{ "mcpServers": { "computer": { "type": "stdio", "command": "computer", "args": ["mcp"] } } }
```

Tools: `screenshot` (downscaled to ≤1568px long edge by default; `max_width`
overrides), `outputs`, `windows`, `mouse_move`, `mouse_click`, `mouse_scroll`,
`mouse_drag`, `key`, `type`, `browse`. Only offer it where a display exists.

Integration notes (direct CLI use):

- **Screenshots** go to a `-o <file>` path or stdout (PNG bytes) — pipe stdout
  straight into your app's image buffer.
- **Linux prerequisites:** a Wayland session (`$WAYLAND_DISPLAY` /
  `$XDG_SESSION_TYPE=wayland`) and write access to `/dev/uinput` (typically a
  udev rule or `input` group membership). Gate the feature on both being
  present. macOS uses native APIs (`CGEvent…`) and needs Accessibility +
  Screen-Recording permissions.
- **Headless = no computer.** Inside the heyvm sandbox (a headless microVM)
  there is no display, so `computer` can't run. `printer review`/`test`
  auto-route UI work to the host when a display is present; for `printer exec`
  use `--no-sandbox` when you need click-testing.
- `printer test <spec> --url http://localhost:PORT` is a ready-made gate that
  drives one agent turn exercising the running app through `computer` and exits
  non-zero unless the verdict is PASS.

---

## Minimal wiring sketch

```
desktop app
 ├─ spawn `codegraph watch` (cwd = repo)        → always-fresh index, supervised
 ├─ embed `codegraph mcp` in the app's agent    → native code-nav tools
 ├─ spawn `printer exec <spec> --verbose`        → stream stderr to a log pane
 │     └─ poll .printer/exec.json + tasks/*.md  → render job/plan status
 └─ call `computer screenshot|mouse|key|type`    → UI automation / visual checks
```

Keep each invocation's cwd at the target repo root, prefer absolute binary
paths, and treat a missing binary or non-zero exit as a recoverable,
surfaced-to-the-user condition rather than a hard failure.
