# printer-plugin: heyvm

Sandbox driver for [printer] that dispatches every agent turn through a
[heyvm] worktree, plus a `before_run` skill that briefs the agent on the
worktree it lands in.

[printer]: https://github.com/Heyo-Computer/printer
[heyvm]: https://docs.heyo.computer

## Prerequisites

The `heyvm` CLI must be on `$PATH`. The vendor installer puts it at
`~/.local/bin/heyvm` when `/usr/local/bin` is not writable. See
<https://docs.heyo.computer> for full install docs.

## Install

This plugin contributes a `[driver]` block and a skill — there is no
binary to build. You can install it from the public printer repo or from a
local checkout.

### From the printer git remote (no checkout needed)

`--subdir` points at the plugin inside the monorepo; the inferred plugin
name comes from the subdirectory's basename (`heyvm`):

```
printer add-plugin https://github.com/heyo-computer/printer \
    --subdir plugins/heyvm
```

Pin a specific revision with `--rev <branch|tag|sha>`. To pull the latest
`main` over an existing install, append `--force`.

### From a local checkout

`path:` specs are resolved against your **current working directory**, so
either run from the printer repo root or pass an absolute path:

```
# from the printer repo root:
printer add-plugin path:plugins/heyvm

# or from anywhere:
printer add-plugin path:/abs/path/to/printer/plugins/heyvm
```

### Two-step (driver manifest + heyvm CLI)

The registry name (`printer add-plugin heyvm`) currently runs only the
vendor's `curl … | sh` installer to drop the `heyvm` CLI at
`~/.local/bin/heyvm`; it does not yet bundle the `[driver]` manifest. Until
that is wired up, install in two steps if you also need the CLI:

```
printer add-plugin heyvm                                        # heyvm CLI -> ~/.local/bin
printer add-plugin https://github.com/heyo-computer/printer \   # driver + skill manifest
    --subdir plugins/heyvm --force
```

`--force` only clears `~/.printer/plugins/heyvm/`, not `~/.local/bin/heyvm`.

That copies the `[driver]` block and the `heyvm-sandbox` skill into
`~/.printer/plugins/heyvm/`. Verify with:

```
printer plugins                       # heyvm should appear with ROLES=hooks+driver
printer hooks list --event before_run # heyvm-sandbox skill listed
```

If `heyvm` itself is not on `$PATH`, the install still succeeds (the manifest
is the only thing that lands), but the next `printer exec` will fail at
`create` with heyvm's stderr surfaced.

## What it does

- `[driver]` — `printer exec` runs every agent invocation as
  `heyvm exec <handle> --session printer --env IS_SANDBOX=1 -- <agent argv>`
  (no extra `sh -c` layer; heyvm passes argv verbatim into the session
  shell). A heyvm sandbox is created at the top of `exec` with host cwd
  bind-mounted at `/workspace`, and is destroyed when `exec` finishes (or
  early-returns / panics — `destroy` runs from `Drop`). The `IS_SANDBOX=1`
  env tells claude code to allow `--dangerously-skip-permissions` under
  the sandbox's root user.
- `before_run` skill — registers `skills/heyvm-sandbox/SKILL.md` so the
  implementer agent understands `/workspace` is bind-mounted from the host
  while everything else is ephemeral.

## Lifecycle of one `printer exec`

A single sandbox spans both run and review phases (provisioned in
`exec.rs::acquire_exec_sandbox`). The lifecycle is:

1. **`create`** — `heyvm create --name printer-{spec_slug} --image {base_image} --no-ttl --needs-network --mount {cwd}:/workspace --mount $HOME/.claude:$HOME/.claude --mount $HOME/.codex:$HOME/.codex --mount $HOME/.config/amp:$HOME/.config/amp`.
   The host cwd is bind-mounted into the sandbox at `/workspace`, and
   the host's `~/.claude`, `~/.codex`, and `~/.config/amp` are bind-mounted
   RW at the same paths inside the sandbox so agent CLIs can persist session
   state and reuse host credentials/config. We redirect heyvm's normal multi-line output to stderr
   and `echo` the deterministic slug, so printer captures
   `printer-{spec_slug}` as `{handle}`. These mounts **do** share state with
   the host: the sandbox can mutate your local agent credentials/conversations.
   Override `[sandbox.commands] create` to
   drop the mount if you need stricter isolation (and pair it with a
   `post_create` that copies a credential file into a sandbox-local HOME).
2. **`post_create`** — `cd /workspace`, wrapped through `enter` so it runs
   inside the sandbox. The persistent heyvm session named `printer` retains
   cwd across subsequent exec calls.
3. **Each agent turn** — both the implementer (`run`) and the reviewer
   (`review`) are wrapped via `enter`:
   `heyvm exec {handle} --session printer --env IS_SANDBOX=1 -- {child}`.
   The agent's argv reaches the session shell with quoting preserved (no
   extra `sh -c` layer). File edits happen at `/workspace` inside the
   sandbox, which is the same inode as the host cwd thanks to the bind
   mount. `IS_SANDBOX=1` is a documented opt-in that claude code reads to
   allow its bypass-permissions mode under root — without it claude
   exits early with a safety check.
4. **`destroy`** — `heyvm rm -y {handle}`. Runs from `Drop`, so it
   fires on panic, early return, or Ctrl-C.

`sync_in` and `sync_out` are intentionally not set: the bind mount means
host and sandbox share the same files, so there is nothing to copy.

### Files round-trip live (no git merge, no copy)

Because cwd is bind-mounted, every write the agent makes lands directly on
the host's filesystem. There is no `sync_out` step copying things back, and
no `git merge` either. Treat the host cwd as actively-mutated for the
duration of `printer exec` — editing the same files from another process
will race the agent.

Only `/workspace` (i.e. `{cwd}`) is shared. Anything the agent installed
outside cwd (`~/.cache/`, `/tmp/`, system packages) lives in the sandbox's
upper layer and is discarded with the sandbox — durable tooling belongs in
`sandbox.base_image`.

To inspect the sandbox after the run finishes, override `destroy` to a no-op
in `[sandbox.commands]` and use `heyvm sh <handle>` to attach a shell.

## Configuration

Override any driver step in `~/.printer/config.toml`:

```toml
[sandbox]
driver = "heyvm"             # or "auto" if heyvm is the only installed driver
base_image = "ubuntu:24.04"  # passed to `heyvm create --image …`

[sandbox.commands]
# create  = "heyvm create --name printer-{spec_slug} --image {base_image} --no-ttl --needs-network --mount {cwd}:/workspace >&2 && echo printer-{spec_slug}"
# enter   = "heyvm exec {handle} --session printer --env IS_SANDBOX=1 -- {child}"
# destroy = "heyvm delete -y {handle}"
# post_create = "cd /workspace"
```

See `printer/HOOKS.md` ("Sandbox drivers" + "Global config") for the full
schema and the variables available to driver templates.

## Bypassing the sandbox

```
printer exec spec.md --no-sandbox
```

Equivalent to `sandbox.driver = "off"` for one invocation. Useful for local
debugging when the driver itself is misbehaving.
