---
name: heyvm-sandbox
description: Create, start, stop, restart, list, and exec commands in heyvm sandboxes. Use when the user wants to manage sandbox lifecycle, run commands in isolated environments, or configure sandbox settings like mounts, ports, and backend types.
argument-hint: "[subcommand] [args...]"
allowed-tools: Bash, Read, Grep
---

# heyvm CLI — Sandbox Manager

> **See also:** load the **heyvm-docs** skill for an overview of the heyvm
> platform and an index of the other heyvm-* skills (deploy, proxy, api,
> firecracker, login, system).

You are interacting with the `heyvm` CLI, the sandbox management tool for the Heyo platform. Use it to create and manage isolated sandbox environments backed by multiple runtimes.

## Binary Location

The binary is `heyvm`. If it is not on PATH, build it from `mvm-ctrl/`:

```bash
cargo build --release -p heyvm
```

## Global Options

| Flag | Description |
|------|-------------|
| `--api` | Run only the HTTP API server (no TUI). Default port 3000 |
| `--port <PORT>` | API server port when using `--api` (default: 3000). TUI always uses 34099 |
| `--msb-host <HOST>` | Microsandbox server host (default: localhost) |
| `--dev` | Development mode: uses localhost for auth |
| `--cloud-url <URL>` | Cloud server URL |
| `--auth-url <URL>` | Auth server URL |
| `--debug` | Enable verbose logging |
| `--upgrade` | Self-upgrade to the latest version |

## Available Subcommands

### Sandbox Lifecycle

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm create` | Create a new sandbox | `heyvm create --name my-sandbox --type shell` |
| `heyvm create --print-id` | Create and print only the sandbox id (for scripts) | `ID=$(heyvm create --name s --print-id)` |
| `heyvm start <id>` | Start an inactive sandbox | `heyvm start my-sandbox` |
| `heyvm stop <id>` | Stop a running sandbox | `heyvm stop my-sandbox` |
| `heyvm restart <id>` | Restart a sandbox | `heyvm restart my-sandbox` |
| `heyvm rm <id>` | Remove a sandbox after confirmation | `heyvm rm my-sandbox` |
| `heyvm rm <id> --yes` | Remove without prompting (for scripts) | `heyvm rm my-sandbox --yes` |

### Execution

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm exec <id> <cmd...>` | Run a command in a sandbox | `heyvm exec my-sandbox -- python -c "print('hello')"` |
| `heyvm exec <id> --cwd <dir> <cmd...>` | Run a command from a specific directory (stateless) | `heyvm exec my-sandbox --cwd /workspace -- pytest` |
| `heyvm exec <id> --user <name|uid> <cmd...>` | Run as a non-root user inside the sandbox | `heyvm exec my-sandbox --user agent -- claude` |
| `heyvm exec <id> --session <name> <cmd...>` | Persistent shell (cwd/env carry across calls) | `heyvm exec my-sandbox --session work -- export FOO=bar` |
| `heyvm session reset <id> <session>` | Reset a session's saved cwd/env without destroying the sandbox | `heyvm session reset my-sandbox work` |
| `heyvm sh <id>` | Open interactive shell | `heyvm sh my-sandbox` |
| `heyvm run-host <id> <cmd...>` | Run a host command in the sandbox mount dir | `heyvm run-host my-sandbox -- npm install` |

### Compound Commands

When running compound commands with `heyvm exec`, wrap them in `sh -c` with a quoted string:

```bash
# Compound commands via sh -c
heyvm exec my-sandbox -- sh -c 'echo hello && echo world'

# Pipes
heyvm exec my-sandbox -- sh -c 'cat /etc/os-release | head -5'

# Redirects
heyvm exec my-sandbox -- sh -c 'echo data > /tmp/file.txt'

# Simple commands work directly
heyvm exec my-sandbox -- python -c "print('hello')"
```

The `--` separator is required before the command to prevent flag parsing.

### Listing

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm list` | List all sandboxes (local + deployed) | `heyvm list` |
| `heyvm list-inactive` | List stopped sandboxes | `heyvm list-inactive --count 20` |
| `heyvm images list --local` | List pulled docker images and local sandbox snapshots | `heyvm images list --local` |

### Mounts & Storage

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm mount-add` | Add mount to a sandbox | `heyvm mount-add -i my-sandbox --host-path /tmp/data --sandbox-path /data` |
| `heyvm archive <id>` | Archive sandbox mounts to S3 | `heyvm archive my-sandbox --name backup` |
| `heyvm archive-dir [path]` | Archive a local directory | `heyvm archive-dir ./my-project --name v1` |

### Networking & Sharing

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm bind <id> <port>` | Proxy a **local** sandbox port publicly (requires `API_HOSTNAME` env var) | `API_HOSTNAME=heyo.computer heyvm bind my-sandbox 8080` |
| `heyvm proxy start <port>` | Expose local port over iroh P2P | `heyvm proxy start 3000` |
| `heyvm proxy list` | List saved proxy endpoints | `heyvm proxy list` |
| `heyvm proxy sync` | Sync proxy endpoints with cloud | `heyvm proxy sync` |
| `heyvm proxy add <name> <url>` | Add a saved endpoint by shortname | `heyvm proxy add my-server heyo://...` |
| `heyvm connect <ticket>` | Connect to remote proxy | `heyvm connect heyo://...` |
| `heyvm share <id>` | Share sandbox shell over P2P | `heyvm share my-sandbox --name pair-session` |
| `heyvm ssh <ticket>` | SSH into a shared sandbox | `heyvm ssh my-shared-sandbox` |

**Important:** `heyvm bind` only works for **local** sandboxes. For
deployed (cloud) sandboxes, expose ports at deploy time with `heyvm
deploy ... --port <p>` (or via the cloud API) — see the **heyvm-deploy**
skill.

### Deployed Sandbox Management

These commands work for both local and cloud-deployed sandboxes. The CLI resolves deployed sandboxes by name, slug, or ID via the cloud API.

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm mount <id>` | Mount deployed sandbox workspace locally via SSHFS | `heyvm mount my-deployed-app` |
| `heyvm update <id>` | Replace deployed sandbox mount from archive | `heyvm update my-app --archive abc123` |
| `heyvm resize <id>` | Resize deployed sandbox compute resources | `heyvm resize my-app --size-class medium` |
| `heyvm exec <id> -- <cmd>` | Run a command in a deployed sandbox | `heyvm exec my-app -- ls /workspace` |
| `heyvm sh <id>` | Interactive shell into a deployed sandbox | `heyvm sh my-app` |
| `heyvm restart <id>` | Restart a deployed sandbox (stop VM, redefine, start, wait for cloud-init) | `heyvm restart my-app` |
| `heyvm rm <id>` | Remove a deployed sandbox after confirmation | `heyvm rm my-app` |
| `heyvm rm <id> --yes` | Remove without prompting (for scripts) | `heyvm rm my-app --yes` |
| `heyvm list-archives` | List available archives | `heyvm list-archives` |
| `heyvm delete-archive <id>` | Delete an archive | `heyvm delete-archive ar-abc123` |
| `heyvm checkpoint <id>` | Save VM state for fast resume | `heyvm checkpoint my-app` |

**Note:** Cloud deployment is a 3-command flow: `heyvm create --cloud
... --backend libvirt --image ubuntu:24.04 --start-command '...' --port
P` boots an empty cloud sandbox, then `heyvm archive-dir <dir>` +
`heyvm update <name> --archive <id>` push your code in. Works on macOS
and Linux. For details (and the Linux-only `--deploy-from` advanced
form), load the **heyvm-deploy** skill.

### Git & Development

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm wt <branch>` | Create git worktree sandbox | `heyvm wt feat/cool-feature -b` |
| `heyvm pull <image>` | Pull a docker image | `heyvm pull ubuntu:24.04` |

### Skills & Setup

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm install-skills` | Download and install Claude Code skills | `heyvm install-skills` |

### Maintenance

| Command | Description | Example |
|---------|-------------|---------|
| `heyvm normalize-wasix-images` | Normalize WASIX image values | `heyvm normalize-wasix-images --dry-run` |
| `heyvm test-proxy` | End-to-end proxy test | `heyvm test-proxy --keep` |

## Create Options

```
--name <NAME>              Sandbox name (required)
--image <IMAGE>            Container image (default from settings)
--slug <SLUG>              URL-safe slug (default: slugified name)
--type <TYPE>              shell | python | node
--mount <HOST:SANDBOX>     Mount path (repeatable)
--ttl-seconds <SECS>       Time-to-live
--start-command <CMD>      Custom start command
--backend <TYPE>           msb | wasix | wasip2 | docker | apple_container | apple_virt | sandbox_exec | bubblewrap | libvirt | firecracker
                           (alias: --backend-type. See backend constraints below.)
--env <KEY=VALUE>          Environment variable (repeatable)
--setup-hook <CMD>         Shell command to run after creation or mount replacement (repeatable)
```

## Connect Options

```
<TICKET_URL>               heyo:// connection URL or shortname
-p, --port <PORT>          Local port to listen on (default: random)
-r, --relay <URL>          Relay server URL for short ticket lookup
--save <NAME>              Save connection under a shortname
--shell                    Open interactive SSH shell after connecting
--run-host                 Mount remote workspace via SSHFS and run a host command
--mount-path <PATH>        Sandbox path to mount (default: /workspace, used with --run-host)
```

## Archive-dir Options

```
[PATH]                     Local directory to archive (default: current directory)
--name <NAME>              Optional archive name
--mount-path <PATH>        Mount path prefix in the archive (default: /workspace)
--token <TOKEN>            JWT token (or set HEYO_ARCHIVE_TOKEN env var)
--no-ignore                Include build assets (node_modules, target, dist, etc.)
```

## Mount Options

```
<ID>                       Deployed sandbox ID, slug, or name
--mount-path <PATH>        Remote path to mount (default: /workspace)
--local-path <PATH>        Local directory to mount into (default: auto-generated)
[COMMAND]                  Host command to run after mounting (use -- before command)
```

## Update Options

```
<ID>                       Deployed sandbox ID, slug, or name
--archive <ARCHIVE_ID>     Archive ID to replace the mount with (required)
--mount-path <PATH>        Mount path to replace (default: /workspace)
```

## Bind-Mount Semantics (bubblewrap)

`heyvm create --mount HOST:SANDBOX` (and the implicit workspace mount) under
the bubblewrap backend uses a kernel bind mount inside the sandbox's mount
namespace. The host directory is the source of truth; the sandbox sees the
same files via the same inodes.

What you can rely on:

- **Atomic rename across the boundary.** Renames inside the sandbox land
  atomically on the host because both ends share an inode under one
  filesystem. Editors that write `.tmp` + `rename` work normally; `git`
  index/lockfile churn is safe.
- **Inode preservation.** A file's inode number stays stable for tools that
  stat-match across writes (some agents, `make`, content-addressed caches).
- **Live round-trip.** Reads on the host see writes from the sandbox without
  any sync step, and vice versa.

Caveats:

- **Delete mid-run.** If the sandbox is deleted (`heyvm rm`) while a
  process inside has open writes, those writes flush to the host normally —
  the host directory is not touched by sandbox teardown, only the sandbox's
  mount namespace is. Long-tail in-flight writes (e.g. fsync still pending in
  the kernel) follow normal Linux semantics: open fds remain valid until the
  killed process exits, and buffered data is flushed.
- **Project snapshot mode is different.** `--project-snapshot DIR` switches
  the workspace to overlayfs (lower=DIR, upper=per-sandbox). In that mode
  changes do **not** round-trip live to the host — they live in the upper
  layer until you copy them out.

## Backend Types

> **CRITICAL — local vs cloud backends.** All of the backends below are
> **LOCAL ONLY** except `libvirt`, `firecracker`, and `kvm`, which are the
> only backends the Heyo cloud (`heyvm create --cloud` / `--deploy-from`, regions `US` / `EU`) can
> run. `apple_virt`, `apple_container`, `sandbox_exec`, `bubblewrap`,
> `docker`, `msb`, `wasix`, `wasip2` are host-only — passing them to
> `heyvm create --cloud --backend X` will fail with a clear error. See **heyvm-deploy** for
> the cloud side.

- **msb** *(local only)* — Microsandbox (default)
- **wasix** *(local only)* — WASIX WebAssembly
- **wasip2** *(local only)* — WASI Preview 2
- **docker** *(local only)* — Docker container
- **apple_container** *(local only, macOS)* — Apple Container, uses Apple's `container` CLI
- **apple_virt** *(local only, macOS arm64)* — Apple Virtualization native VM, uses Virtualization.framework directly
- **sandbox_exec** *(local only, macOS)* — macOS sandbox-exec
- **bubblewrap** *(local only, Linux)* — Linux namespaces
- **libvirt** *(local + cloud, Linux)* — QEMU/KVM via libvirt
- **firecracker** *(local + cloud, Linux)* — Firecracker microVMs
- **kvm** *(local + cloud, Linux)* — Direct KVM via `kvmbind`

## Common pitfalls — wrong-platform backend

`heyvm create --backend X` will reject `X` if `X` isn't available on the
current host, with a message that names the available backends and a
copy-pasteable command. The following invocations are *intentionally
invalid* and the CLI is expected to reject them:

<!-- e2e: must-fail -->
```bash
heyvm create --backend nonexistent_backend --name testbad
```

This catches the most common confusion: agents that pick a Linux-only
backend (firecracker / bubblewrap / libvirt / kvm) on a macOS dev host,
or a macOS-only backend (apple_virt / apple_container / sandbox_exec)
on a Linux host. The error tells the agent what to use instead.

## Workflow

When the user asks to interact with sandboxes:

1. **First, check what's running**: `heyvm list` to see active sandboxes.
2. **Create if needed**: Use `heyvm create` with appropriate `--type` and `--backend-type`.
3. **Execute commands**: Use `heyvm exec <id-or-slug> -- <command>` for non-interactive work.
4. **Use slug or ID**: All commands that take `<id>` also accept the sandbox slug.

When the user provides `$ARGUMENTS`, interpret them as a subcommand and arguments to pass directly to `heyvm`. For example, `/heyvm list` should run `heyvm list`.

If `$ARGUMENTS` is empty, ask the user what they want to do with their sandboxes.
