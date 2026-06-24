---
name: heyvm-sandbox
description: Use this skill any time you are running inside a heyvm sandbox (i.e. a `printer run` / `printer exec` turn dispatched through the heyvm sandbox driver). Explains what is mounted, what is ephemeral, and how to inspect the sandbox from inside it. Triggers when the agent needs to know "where am I", "is this state persistent", "what host paths are visible", or wants to check the sandbox's resources.
version: 0.5.0
---

# heyvm-sandbox

You are running inside a **heyvm sandbox** — an isolated VM-backed
environment provisioned by `printer` through the `heyvm` plugin's driver.
Every command you run is executed via
`heyvm exec <handle> --session printer -- <argv>` against this sandbox
(argv is passed straight through to the persistent session shell — no
extra `sh -c` layer).

## What's mounted

- **The host's `printer exec` cwd is bind-mounted at `/workspace` inside
  the sandbox**, and `/workspace` is your starting cwd (set once via the
  driver's `post_create` step and retained by the persistent `printer`
  session). File reads and writes under `/workspace` are live on the
  host's filesystem — that's how the implementation phase's edits become
  real changes on the host repo, with no separate copy step.
- **`~/.claude` is bind-mounted RW** so claude code can read host
  credentials and persist per-session state (conversation logs,
  `session-env/<uuid>` for Bash tool calls). Writes here also land on the
  host, so don't delete or rewrite anything you didn't create yourself.
- **`~/.codex` and `~/.config/amp` are bind-mounted RW** so Codex and Amp
  can read host auth/config and persist their own session state. Writes
  here also land on the host.
- **`~/.local/state`, `~/.local/share`, and `~/.cache` are bind-mounted
  RW** so agents that follow the standard XDG-state / XDG-data /
  XDG-cache convention (or hardcode `~/.local/state/<name>/` for logs
  and runtime state, or live under `~/.local/share/<name>/` like
  claude code) can read and write without hitting `EROFS` or empty
  tmpfs overlays. Writes land on the host, so the same care applies
  as with `~/.claude`.
- **`~/.local/bin` and `~/.cargo/bin` are on the session `PATH`** (set
  once in `post_create` and retained by the persistent `printer` heyvm
  session). Host-installed CLIs that live there — claude, codex, amp, cargo
  binaries, npm globals via `prefix` — resolve by bare name without you
  having to spell out the absolute path.
- **The rest of `$HOME` is read-only** (heyvm's bubblewrap default).
  Reads work; mkdir/touch outside `~/.claude`, `~/.codex`,
  `~/.config/amp`, `~/.local/state`, `~/.cache`, and `/workspace` will fail with `EROFS`. If you need
  scratch space, use `/tmp` or a fresh dir under `/workspace`.
- **Anything outside `/workspace` is ephemeral relative to the sandbox**
  but `~/.local/state` and `~/.cache` round-trip to the host, so
  artifacts written there persist across sandboxes. Installed packages
  (apt, pip without --user, etc.) and `/tmp/` do not survive.

## When in doubt

- `pwd` — confirm you're in `/workspace`.
- `findmnt /workspace` — confirm the bind mount and see the host source path.
- `heyvm get <handle>` (from outside the sandbox) — show status and
  metadata for the running sandbox.

## Things to avoid

- **Don't `rm -rf` outside `/workspace` "to free space"** — host paths may
  be bind-mounted in unexpected places, and the sandbox itself is cheap to
  recreate.
- **Don't expect long-running background processes to survive** beyond the
  current `printer` invocation. Each `printer exec` provisions a fresh
  sandbox and tears it down at the end.
- **Don't use `sudo` to install global tooling** unless the spec calls for
  it; the sandbox is recreated on every run, so durable setup belongs in
  the base image (`sandbox.base_image` in `~/.printer/config.toml`) instead.

## Skipping the sandbox

If a step legitimately can't run inside the sandbox (for example, it needs
access to host devices), surface that to the user — don't try to escape.
They can re-run with `--no-sandbox` to dispatch on the host directly.
