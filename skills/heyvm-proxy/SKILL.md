---
name: heyvm-proxy
description: Use heyvm proxy, connect, share, ssh, and bind commands to expose ports, connect to remote sandboxes over P2P (iroh), mount remote workspaces, and share sandbox shells.
argument-hint: "[subcommand] [args...]"
allowed-tools: Bash, Read, Grep
---

# heyvm Proxy & Connect — P2P Networking for Sandboxes

> **See also:** load the **heyvm-docs** skill for an overview of the heyvm
> platform and an index of the other heyvm-* skills (sandbox, deploy, api,
> firecracker, login, system).

You are helping the user expose local services, connect to remote sandboxes, share sandbox shells, and mount remote workspaces using the heyvm P2P networking features (powered by iroh).

## Binary Location

The binary is `heyvm`. If it is not on PATH, build it from `mvm-ctrl/`:

```bash
cargo build --release -p heyvm
```

## Authentication

Most proxy/connect commands sync endpoint shortnames to the cloud and require authentication. Ensure the user is logged in first:

```bash
heyvm login
```

Token is loaded automatically from `~/.heyo/token.json`. Commands degrade gracefully (local-only) without auth.

---

## Commands Overview

| Command | Purpose |
|---------|---------|
| `heyvm proxy start` | Expose a local port over iroh P2P |
| `heyvm proxy list` | List saved proxy endpoints |
| `heyvm proxy sync` | Sync endpoints with the cloud |
| `heyvm proxy add` | Save an endpoint shortname |
| `heyvm connect` | Connect to a remote proxy |
| `heyvm ssh` | SSH into a shared sandbox |
| `heyvm share` | Share a sandbox shell over P2P |
| `heyvm bind` | Bind a local sandbox port to a public hostname |

---

## Exposing a Local Port

### `heyvm proxy start <PORT>`

Starts an iroh P2P proxy server on a local port. Prints a `heyo://` connection URL that others can use to connect.

```bash
heyvm proxy start 3000
```

**Options:**

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Save the endpoint under a shortname (synced to cloud) |
| `-r, --relay <URL>` | Relay server URL for short ticket codes |

**Example with shortname:**
```bash
heyvm proxy start 8080 --name my-server
```

Others can then connect by shortname: `heyvm connect my-server`

The server runs until Ctrl+C.

---

## Connecting to a Remote Proxy

### `heyvm connect <TICKET_URL_OR_SHORTNAME>`

Connects to a remote iroh proxy and forwards traffic to a local port.

```bash
# Connect by full URL
heyvm connect heyo://abc123...

# Connect by saved shortname
heyvm connect my-server
```

**Options:**

| Flag | Description |
|------|-------------|
| `-p, --port <PORT>` | Local port to listen on (default: random) |
| `-r, --relay <URL>` | Relay server URL for short ticket lookup |
| `--save <NAME>` | Save this connection under a shortname for future use |
| `--shell` | After connecting, open an interactive SSH shell |
| `--run-host` | Mount remote workspace via SSHFS and run a host command |
| `--mount-path <PATH>` | Sandbox path to mount (default: `/workspace`, used with `--run-host`) |
| `-- <COMMAND...>` | Host command to run (used with `--run-host`) |

### Connect modes

**Default (TCP proxy):**
```bash
heyvm connect my-server -p 8080
# Proxies localhost:8080 -> remote service
```

**Interactive SSH shell:**
```bash
heyvm connect my-server --shell
```

**Mount remote workspace and run a command:**
```bash
heyvm connect my-server --run-host -- code .
```

This mounts the remote `/workspace` via SSHFS, sets up path reconciliation, creates a `CLAUDE_CODE_SHELL` wrapper for remote command execution, and runs the given host command in the mounted directory. Cleanup (unmount, temp files) happens automatically on exit.

---

## SSH into a Shared Sandbox

### `heyvm ssh <TICKET_URL_OR_SHORTNAME>`

Opens an SSH session to a shared sandbox.

```bash
# Interactive shell
heyvm ssh my-sandbox

# Run a specific command
heyvm ssh my-sandbox -- ls /workspace
```

**Options:**

| Flag | Description |
|------|-------------|
| `-p, --port <PORT>` | Local port to listen on (default: random) |
| `-r, --relay <URL>` | Relay server URL for short ticket lookup |
| `--save <NAME>` | Save this connection under a shortname |
| `-- <COMMAND...>` | Command to run in the remote sandbox |

---

## Sharing a Sandbox Shell

### `heyvm share <ID_OR_SLUG>`

Shares a sandbox's SSH over the iroh P2P proxy so others can connect.

```bash
heyvm share my-sandbox --name pair-session
```

**Options:**

| Flag | Description |
|------|-------------|
| `-r, --relay <URL>` | Relay server URL for short ticket codes |
| `--name <NAME>` | Save the connection URL under a shortname |

**What happens:**
1. Ensures the sandbox is running (starts it if stopped)
2. Probes SSH readiness (retries up to 12 times)
3. Starts an iroh proxy server on the sandbox SSH port
4. Prints the `heyo://` connection URL
5. Optionally saves the endpoint under the given shortname
6. Waits for Ctrl+C before shutting down

**Connecting to a shared sandbox:**
```bash
# By shortname (if --name was used)
heyvm ssh pair-session

# By full URL
heyvm ssh heyo://abc123...

# Or with connect for non-SSH use
heyvm connect pair-session --shell
```

---

## Binding a Sandbox Port

### `heyvm bind <ID_OR_SLUG> <PORT>`

Proxies a sandbox port to a public `*.heyo.computer` hostname. Works for both **local** and **deployed (cloud)** sandboxes — the command auto-detects which based on whether the id/slug resolves locally first.

```bash
# Local sandbox: requires API_HOSTNAME (the public hostname your local proxy answers on)
API_HOSTNAME=heyo.computer heyvm bind my-local-vm 8080

# Cloud (deployed) sandbox: no env vars needed, just be logged in
heyvm bind my-deployed-sandbox 8080

# Make the bound endpoint private (requires Heyo auth to reach it)
heyvm bind my-sandbox 8080 --private
```

Behind the scenes, cloud sandboxes hit the `POST /proxy-endpoints/for-deployed` API; local sandboxes use the local proxy daemon. The `--private` flag flips `is_public` to `false` for either path.

**Note on toggling privacy:** there is no in-place toggle. To change `is_public` on an existing binding, delete the endpoint first (`DELETE /proxy-endpoints/<subdomain>` via the cloud API) and then `heyvm bind` again with the desired `--private` flag.

---

## Managing Saved Endpoints

### `heyvm proxy list`

Lists all saved proxy endpoints (local and cloud-synced).

```bash
heyvm proxy list
```

Displays a table with columns: SHORTNAME, TICKET_URL, CREATED.

### `heyvm proxy sync`

Bidirectional sync of endpoint shortnames between local storage and the cloud.

```bash
heyvm proxy sync
```

### `heyvm proxy add <SHORTNAME> <TICKET_URL>`

Manually save a `heyo://` connection URL under a shortname.

```bash
heyvm proxy add my-server heyo://abc123...
```

The ticket URL must start with `heyo://`.

---

## Endpoint Storage

| Location | Contents |
|----------|----------|
| `~/.heyo/proxy-endpoints.json` | Local shortname-to-URL mappings |
| Cloud (via API) | Synced endpoints at `/p2p-proxy-endpoints` |

- Local entries take precedence over cloud entries on conflict
- Add/remove operations are synced to cloud in the background
- Failed connections auto-evict stale endpoints from the store
- Shortname lookup falls back to cloud if not found locally

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `API_HOSTNAME` | Required for `heyvm bind` (public hostname for proxy) |
| `HEYO_RELAY_URL` | Fallback relay server URL for share/connect |

---

## Common Workflows

### Pair programming on a sandbox

```bash
# Person A: share the sandbox
heyvm share my-sandbox --name pair-session

# Person B: connect and open a shell
heyvm ssh pair-session

# Or mount the workspace locally
heyvm connect pair-session --run-host -- code .
```

### Expose a local dev server

```bash
# Start a dev server in a sandbox
heyvm exec my-sandbox -- npm run dev

# Expose it over P2P
heyvm proxy start 3000 --name my-app

# Remote collaborator connects
heyvm connect my-app -p 3000
# Then visit http://localhost:3000
```

### Save and reuse connections

```bash
# Save on first connect
heyvm connect heyo://abc123... --save staging

# Reuse the shortname later
heyvm ssh staging
heyvm connect staging -p 8080
```

---

## Workflow Guidance

When the user provides `$ARGUMENTS`, interpret them as a proxy/connect subcommand. For example:
- `/heyvm-proxy start 8080` should run `heyvm proxy start 8080`
- `/heyvm-proxy connect my-server --shell` should run `heyvm connect my-server --shell`

If `$ARGUMENTS` is empty, ask the user what they want to do:
- Expose a local port? -> `heyvm proxy start`
- Connect to a remote service? -> `heyvm connect`
- Share a sandbox for pair programming? -> `heyvm share`
- SSH into a shared sandbox? -> `heyvm ssh`
