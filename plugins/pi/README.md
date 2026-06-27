# pi-printer-tools (pi package)

[pi](https://pi.dev) package that adds the `codegraph` and `computer` CLIs as
first-class agent tools, and integrates with the agent lifecycle to save
tokens by searching with codegraph instead of grep + full-file reads.

- **`extensions/codegraph.ts`** — registers `codegraph_search`,
  `codegraph_definition`, `codegraph_outline`, `codegraph_snippet`,
  `codegraph_references`, and `codegraph_patch`. On `session_start` it builds
  `.codegraph/index.json` if missing and spawns a detached `codegraph watch`
  daemon (pidfile at `.codegraph/watch.pid`, logs at `.codegraph/watch.log`)
  so the index stays fresh; the daemon is killed on `session_shutdown` if this
  session started it. On `before_agent_start` it appends a token-economy
  block to the system prompt (outline before reading, snippet instead of full
  reads, search instead of grep, patch instead of rewrites).
- **`extensions/computer.ts`** — registers `computer_screenshot` (inline PNG,
  long edge downscaled to 1568px by default), `computer_outputs`,
  `computer_windows`, `computer_mouse_move`, `computer_mouse_click`,
  `computer_mouse_scroll`, `computer_mouse_drag`, `computer_key`,
  `computer_type`, and `computer_browse` — the same surface as `computer mcp`.
  Skipped automatically when the binary is missing or no display is present.
- **Skills** (`codegraph-search`, `codegraph-edit`) that nudge the model to
  prefer the codegraph tools over the built-in `read` / `grep` / `edit` /
  `write` for source files.

Supported codegraph languages: Rust, Python, JavaScript, TypeScript.

## Prerequisites

The `codegraph` binary is required; `computer` is optional (only needed for the
`computer_*` desktop tools). Both must be on `PATH`. From the
[printer](https://github.com/sarocu/printer) repo root:

```sh
make install-codegraph install-computer   # → ~/.local/bin
```

No manual indexing is needed — the extension indexes on first session start —
but you can warm it up yourself with `codegraph index` at the repo root.

## Install

This package lives at `plugins/pi/` in the printer repo. Install from a local
checkout:

```sh
pi install /absolute/path/to/printer/plugins/pi
```

Or try it for one session without installing:

```sh
pi -e /path/to/printer/plugins/pi/extensions/codegraph.ts \
   -e /path/to/printer/plugins/pi/extensions/computer.ts
```

## What you get out of the box

- All six `codegraph_*` tools and ten `computer_*` tools registered and
  described to the model, with per-tool guidelines in the system prompt.
- A live index: `codegraph watch` runs for the project while the session is
  open (re-attaches gracefully if a daemon is already running, e.g. one
  started by `printer run` or the Claude Code plugin).
- A "codegraph token economy" section appended to the system prompt whenever
  an index exists for the project.

## Opt-in: hard-enforce the redirect

Skills and prompt guidance only nudge — the model can still call the built-in
`read` / `edit`. To **block** those tools on supported source files
(`.rs .py .js .jsx .ts .tsx`) so the agent has no choice but to use the
codegraph tools:

```sh
PI_CODEGRAPH_ENFORCE=1 pi
```

Blocked calls return a reason redirecting the model to `codegraph_outline` /
`codegraph_snippet` (reads) or `codegraph_patch` (edits). `write` is never
blocked, so new-file creation still works.

## Development

```sh
cd plugins/pi
npm install            # dev-only typings (pi provides the runtime)
npm run typecheck      # tsc --noEmit
node scripts/smoke.mjs # exercises the tools against the repo's real index
                       # needs Node >= 23.6 for .ts imports, or on 22.x:
                       #   node --experimental-strip-types scripts/smoke.mjs
```

## Troubleshooting

- **Tools missing from the session.** `codegraph`/`computer` not on `PATH`
  when pi started, or (computer) no `WAYLAND_DISPLAY`/`DISPLAY` present. A
  notification is shown at session start in the codegraph case.
- **Search returns nothing for code you just wrote.** The watch daemon
  re-indexes on file events; check `.codegraph/watch.log`. If the daemon
  isn't running, delete a stale `.codegraph/watch.pid` and reload (`/reload`).
- **Patch keeps failing.** Usually stale context lines — pull the region with
  `codegraph_snippet` and rebuild the diff; dry-run with `check: true`.

## Files

```
plugins/pi/
├── package.json                       # pi manifest (extensions + skills)
├── README.md                          # this file
├── tsconfig.json
├── extensions/
│   ├── codegraph.ts
│   └── computer.ts
├── skills/
│   ├── codegraph-search/SKILL.md
│   └── codegraph-edit/SKILL.md
└── scripts/
    └── smoke.mjs                      # mocked-ExtensionAPI smoke test
```
