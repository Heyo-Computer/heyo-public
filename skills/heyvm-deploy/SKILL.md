---
name: heyvm-deploy
description: Deploy apps to Heyo cloud sandboxes — archive code, deploy to production, bind ports, set up custom domains, and manage deployed sandboxes. Use when the user wants to deploy, update, or manage a running app.
argument-hint: "[action] [args...]"
allowed-tools: Bash, Read, Grep
---

# Deploy — App Deployment with Heyo Sandboxes

> **See also:** load the **heyvm-docs** skill for an overview of the heyvm
> platform and an index of the other heyvm-* skills (sandbox, proxy, api,
> firecracker, login, system).

You are helping the user deploy applications to Heyo's cloud sandbox infrastructure. **Always prefer the `heyvm` CLI over direct API calls.** The CLI handles authentication, error handling, and retries more robustly than raw `curl` commands. Only fall back to the API when a specific operation has no CLI equivalent.

## Authentication

The `heyvm` CLI handles authentication automatically using the token
stored at `~/.heyo/token.json`. **Every CLI call refreshes the token if
needed.** You do not need to inspect, decode, or refresh the token
yourself.

If a `heyvm` command fails with an auth error, the fix is to ask the
user to run **`heyvm login`** interactively and retry — that's it. Do
not:

- Decode the JWT and try to "check" the expiry — it's just a hint.
- `curl https://auth.heyo.computer/oauth/token` to refresh — that
  endpoint returns 404; there is no public refresh route.
- Run `heyvm login --password "$(cat ~/.heyo/credentials)"` — there is
  no `~/.heyo/credentials` file by default and password-from-file is
  not the supported path. Tell the user to log in.
- Edit `~/.heyo/token.json` by hand.

## Deployment Happy Path — three commands

> **Use this flow first.** It works identically on macOS and Linux,
> requires no local sandbox, and is what the agent should reach for by
> default.

```bash
# 1. Create an empty cloud sandbox with the runtime + start command.
heyvm create --cloud \
  --name my-app \
  --backend libvirt \
  --region US \
  --image ubuntu:24.04 \
  --start-command "cd /workspace && python3 -m http.server 8080" \
  --port 8080

# 2. Archive your local code directory.
heyvm archive-dir ./my-project --name my-app-v1
# -> outputs an archive id like ar-abc123

# 3. Push the archive into the cloud sandbox's workspace.
heyvm update my-app --archive ar-abc123
```

That's it. `--cloud` boots the sandbox from the chosen `--image`, fires
`--start-command`, binds `--port`s publicly, and prints the public URL.
After `heyvm update` the sandbox restarts so the start command picks up
the new files.

### Why `--cloud` instead of `--deploy-from` by default

`--deploy-from <local-sandbox-id>` snapshots a working **local**
sandbox's rootfs and ships it to the cloud. It's the right tool when
you've iterated on a Linux local sandbox and want to deploy *that exact
state*. But:

- **macOS local backends can't be transcoded.** `apple_virt`,
  `apple_container`, `sandbox_exec`, `docker`, `msb`, `wasix`, `wasip2`
  have no path to a cloud `libvirt`/`firecracker`/`kvm` image. On macOS,
  `--deploy-from` errors out (with a hint pointing back here).
- **`--cloud` skips that whole class of failure.** It builds the cloud
  sandbox from a base image, then `heyvm update` swaps in your code.
  No transcoding, no host-platform constraints.

Use `--deploy-from` only when (a) you're on Linux, (b) the source
sandbox has state worth preserving (installed packages, configured
services), and (c) you specifically want the rootfs snapshot, not just
the code.

### `heyvm create --cloud` options

| Option | Description | Default |
|---|---|---|
| `--cloud` | **Required for this path.** Create directly in cloud. | — |
| `--name <NAME>` | Cloud sandbox name. | required |
| `--backend <BACKEND>` | Cloud backend: `libvirt`, `firecracker`, or `kvm`. Local-only backends are rejected. | `libvirt` |
| `--region <REGION>` | `US` or `EU`. | `US` |
| `--image <IMAGE>` | Base image — `ubuntu:24.04` (has python3, node via apt) or `alpine:3.23` (smaller, fewer tools pre-installed). | `ubuntu:24.04` |
| `--start-command <CMD>` | Shell command to run on every boot (after `heyvm update` swap-in too). | — |
| `--port <PORT>` | Public port to expose (repeatable). | — |
| `--size-class <CLASS>` | `micro` / `mini` / `small` / `medium` / `large`. | `small` |
| `--health-path <PATH>` | Health check path (e.g. `/health`); waits for 2xx. | — |
| `--health-timeout <DUR>` | Health check timeout. | `120s` |
| `--private` | Make bound ports private (account members only). | — |
| `--env KEY=VALUE` | Environment variable (repeatable). | — |
| `--setup-hook <CMD>` | Run after first boot (repeatable). | — |
| `--ttl-seconds <N>` | Time-to-live in seconds. | Plan default |
| `--working-directory <PATH>` | Working directory inside the cloud sandbox. | `/workspace` |
| `--format <FMT>` | Output format: `json` or `text`. | `text` |

### Updating the deployed code

After the initial deploy, push new code with `heyvm update`:

```bash
heyvm archive-dir ./my-project --name my-app-v2
heyvm update my-app --archive ar-NEWID
```

The sandbox restarts and the start command picks up the new files. No
need to re-deploy.

### Picking the right `--image`

The default `ubuntu:24.04` includes `python3`, `apt`, and most common
tools. If your `--start-command` needs something specific:

| Need | Use |
|---|---|
| `python3 -m http.server` | `--image ubuntu:24.04` (Alpine has no python3) |
| Node.js | `--image ubuntu:24.04` then `--setup-hook 'apt-get update && apt-get install -y nodejs npm'`, or use a public node image (see Public Images) |
| Static site only (no runtime) | `--image ubuntu:24.04 --start-command "cd /workspace && python3 -m http.server <port>"` |
| Minimal footprint | `--image alpine:3.23` (smaller; remember `apk add` for python3 etc.) |

When in doubt, default to `ubuntu:24.04`.

## Advanced — `heyvm create --deploy-from <local-sandbox>` (Linux only)

When you've iterated on a *Linux* local sandbox and want to ship the
exact rootfs (installed packages + config + services baked in), use
`--deploy-from`. The flow:

1. Create / reuse a Linux local sandbox with your code:
   ```bash
   heyvm create --name my-app-local --backend libvirt \
     --mount ./my-project:/workspace \
     --start-command "cd /workspace && npm install && npm start" \
     --open-port 8080
   ```
2. Verify it works locally (`heyvm sh my-app-local`,
   `heyvm exec my-app-local -- curl http://localhost:8080/health`).
3. Snapshot + ship:
   ```bash
   heyvm create --deploy-from my-app-local \
     --name my-app \
     --backend libvirt \
     --start-command "cd /workspace && npm start" \
     --port 8080
   ```

**Backend transcode rules** (cloud target picked by `--backend`):

| Source backend | Valid `--backend` targets |
|---|---|
| `libvirt` (Linux) | `libvirt` |
| `firecracker` (Linux) | `firecracker`, `kvm` |
| `kvm` (Linux x86_64) | `kvm`, `firecracker` |
| `apple_virt` (macOS) | `kvm` only |
| All others (`apple_container`, `sandbox_exec`, `docker`, `bubblewrap`, `msb`, `wasix`, `wasip2`) | none — use `--cloud` instead |

Get the wrong combination and the CLI rejects with a message that names
the right `--backend` value (or points back at `--cloud`).

`--deploy-from` accepts the same cloud-side flags as `--cloud`
(`--region`, `--port`, `--size-class`, `--health-path`, `--env`, etc.)
plus snapshot-specific ones: `--sysprep` (libvirt only — strips
machine-id and SSH host keys), `--no-restart`, `--publish-name`.

### Archive primitives

`heyvm update` (used in the happy path above) takes an archive id from
`heyvm archive-dir`. Archives are reusable across sandboxes and
deployments.

```bash
heyvm archive-dir ./my-project --name my-app-v1     # archive a directory
heyvm archive <sandbox-id> --name my-app-v1         # archive a sandbox's mounts
heyvm list-archives                                  # list archives
heyvm delete-archive ar-abc123                       # delete one
```

## Public Images

Public images are pre-built sandbox images shared across the platform. Use them as the `--image` when you create the local source sandbox (Step 1 above) so the cloud snapshot inherits a pre-built environment instead of installing tools from scratch via `--setup-hook`.

### Discover public images

```bash
heyvm images list                    # All public images
heyvm images list --libvirt          # Only libvirt-compatible
heyvm images list --firecracker      # Only firecracker-compatible
heyvm images list --local            # Local Docker images instead
```

The list shows each image's ID (`im-...`), name, and supported backends. Match the `--backend` you plan to deploy with.

### Add a public image to your account

Before deploying with a public image, register it locally with a name:

```bash
heyvm images add <IMAGE_ID>                   # Use the image's registered name
heyvm images add <IMAGE_ID> --name my-base    # Give it a custom local name
```

### Deploy with a public image

Pass the image name (from `heyvm images list`, or your custom `--name`
from `heyvm images add`) as `--image` on the cloud-create step:

```bash
heyvm create --cloud \
  --name my-app \
  --backend libvirt \
  --image my-base \
  --start-command "cd /workspace && npm start" \
  --port 3000

heyvm archive-dir ./my-app --name my-app-v1
heyvm update my-app --archive ar-XYZ
```

Make sure `--backend` is compatible with the image (check
`heyvm images list --libvirt` / `--firecracker`).

### Publish your own public image

Snapshot a configured sandbox and submit it for review:

```bash
heyvm images publish <sandbox-id> --name my-image --description "Node 22 + Postgres"
heyvm images publish <sandbox-id> --name my-image --sysprep   # libvirt: strip machine-id, ssh keys, logs
```

Published images go through review before becoming available to others.

## Common pitfalls

### Pitfall 1 — using a local-only backend with `--deploy-from`

Cloud regions (US, EU) only run `libvirt`, `firecracker`, and `kvm`.
Backends like `apple_virt`, `apple_container`, `bubblewrap`, `docker`,
`msb`, `wasix`, `wasip2`, `sandbox_exec` are **local-only** — they exist
on a developer's host, not in the cloud. Passing one as `--backend` on
`heyvm create --cloud` (or `--deploy-from`) fails immediately with a
message naming the right next command.

The following invocations are *intentionally invalid* and the CLI is
expected to reject them with an actionable error:

<!-- e2e: must-fail -->
```bash
heyvm create --cloud --backend apple_virt --name my-app
```

<!-- e2e: must-fail -->
```bash
heyvm create --cloud --backend docker --region EU --name my-app
```

<!-- e2e: must-fail -->
```bash
heyvm create --cloud --backend bubblewrap --name my-app
```

The error explains:
1. *What is wrong* — the bad backend + region.
2. *What is allowed* — `libvirt`, `firecracker`, `kvm`.
3. *What to run instead* — both the local-create form and the cloud
   `--cloud` form with the right `--backend`.

### Pitfall 2 — `--deploy-from` with a non-transcodable source

`--deploy-from` snapshots a local sandbox's rootfs and ships it as a
private cloud image. Each source backend has a fixed set of cloud
targets it can transcode to:

| Source | Valid `--backend` |
|---|---|
| Linux `libvirt` | `libvirt` |
| Linux `firecracker` | `firecracker`, `kvm` |
| Linux `kvm` (x86_64) | `kvm`, `firecracker` |
| macOS `apple_virt` | `kvm` only |
| Everything else | none — use `--cloud` |

Passing the wrong combination errors out with a message that names the
valid targets AND tells the agent to fall back to `--cloud`. This is
the macOS happy-path escape hatch — you can't transcode `apple_virt` to
`libvirt`, but `--cloud` doesn't need a source at all.

### Pitfall 3 — using the legacy directory-archive deploy

`heyvm deploy <directory>` is no longer supported. The cloud-side mount
setup it relied on doesn't exist on the deploy host, so it always
errored with `Invalid mount path: Host path does not exist:
/root/.heyo/sandboxes/dep-…/workspace`. Calling it now returns a
prescriptive error pointing at the `--cloud` flow instead.

<!-- e2e: must-fail -->
```bash
heyvm deploy ./my-project --name my-app --port 8080
```

## Managing Deployed Sandboxes

All management commands below work for both local and deployed sandboxes. The CLI resolves deployed sandboxes via the cloud API automatically.

### Bind a port (expose publicly)

```bash
heyvm bind <id-or-name> <port>
heyvm bind <id-or-name> <port> --private    # Account members only
heyvm bind <id-or-name> <port> --format json # JSON output
```

The app becomes accessible at `https://<subdomain>.heyo.computer/`.

### List sandboxes

```bash
heyvm list                    # Running sandboxes (local + deployed)
heyvm list-inactive           # Stopped sandboxes
```

### Delete a sandbox (local or deployed)

```bash
heyvm rm <id-or-name>         # Prompts before deleting
heyvm rm <id-or-name> --yes   # Skip prompt for scripts
```

`heyvm rm` refuses to prompt when stdin isn't a tty (script / agent /
CI). Always pass `-y` or `--yes` from non-interactive contexts.

### Execute commands

```bash
heyvm exec <id-or-name> -- <command>
heyvm sh <id-or-name>                       # Interactive shell
```

### Mount workspace locally

```bash
heyvm mount <id-or-name>                    # Mount and wait (Ctrl+C to unmount)
heyvm mount <id-or-name> -- code .          # Mount and open in editor
heyvm mount <id-or-name> --mount-path /app  # Mount a specific path
```

### Update a deployment

```bash
heyvm archive-dir ./my-project --name my-app-v2
heyvm update <id-or-name> --archive <new-archive-id>
```

### Resize

```bash
heyvm resize <id-or-name> --size-class <CLASS>
```

Size classes: `micro` (0.25 CPU, 0.5 GB), `mini` (0.5 CPU, 1 GB), `small` (1 CPU, 2 GB), `medium` (2 CPU, 4 GB), `large` (4 CPU, 8 GB).

### Edit TTL

```bash
heyvm edit-ttl <id-or-name> --ttl-seconds <N>   # 0 for unlimited (if plan allows)
```

### Wait for readiness

```bash
heyvm wait-for <id-or-name> <port>                           # Wait for port to be ready
heyvm wait-for <id-or-name> <port> --path /health            # Wait for HTTP 2xx on /health
heyvm wait-for <id-or-name> <port> --timeout 60s             # Custom timeout
heyvm wait-for --url https://slug.heyo.computer/health       # Poll external URL directly
```

### Port forwarding

```bash
heyvm port-forward <id-or-name> <sandbox-port>               # Forward same port locally
heyvm port-forward <id-or-name> <sandbox-port> -p <host-port> # Forward to different local port
```

### Snapshot (create reusable image)

```bash
heyvm snapshot <id-or-name> --name my-snapshot
heyvm snapshot <id-or-name> --name my-snapshot --no-restart   # Don't restart after snapshot
```

### SSH access

```bash
heyvm share <id-or-name> --name my-app
heyvm ssh my-app              # From another machine
```

### Manage archives

```bash
heyvm list-archives           # List all archives
heyvm delete-archive <id>     # Delete an archive
```

## Use the CLI, not raw API

For all deploy operations, use the CLI. The three-command happy path
(`heyvm create --cloud` + `heyvm archive-dir` + `heyvm update`) handles
auth, sandbox creation, archive upload, mount swap, port binding, and
readiness wait without any `curl`. **Do not** fall back to direct API
calls — the CLI exists precisely because the raw endpoints have sharp
edges (see the broken-endpoint note below) and agents reaching for
`curl` consistently end up re-implementing auth, hitting deprecated
paths, and chasing 401s the CLI would have refreshed for them.

### `POST /sandbox-deploy archive_id` is broken — do not call it

The cloud handler at `/sandbox-deploy` with `archive_id` set always
returns:

```
400 Bad Request
{"error":"Invalid mount path: Host path does not exist:
 /root/.heyo/sandboxes/dep-…/workspace"}
```

The handler validates the workspace mount path on the *control* host,
but it only exists on the *backend* host the sandbox actually runs on.
This is a server bug, not an auth or payload issue. **Do not** retry
with different fields, fresh tokens, or different regions — the
response is the same.

The supported workaround **is** the three-command happy path: create
the cloud sandbox empty with `heyvm create --cloud`, then attach the
archive with `heyvm update <name> --archive <id>` (which goes through
`/deployed-sandboxes/<id>/replace-mount` and skips the broken
validator).

### Custom domains

Custom domains are configured via the cloud API (`/custom-domains` endpoint). The domain must have a CNAME record pointing to `heyo.computer`. SSL certificates are provisioned automatically.

## Typical Deployment Examples

All examples use the three-command happy path: `heyvm create --cloud`
to boot the sandbox + `heyvm archive-dir` + `heyvm update` to push code.

### Deploy a static site
```bash
heyvm create --cloud --name my-site --backend libvirt --region US \
  --image ubuntu:24.04 \
  --start-command "cd /workspace && python3 -m http.server 8080" \
  --port 8080
heyvm archive-dir ./public --name my-site-v1
heyvm update my-site --archive ar-XYZ
```

### Deploy a Node.js app
```bash
heyvm create --cloud --name my-app --backend libvirt --region US \
  --image ubuntu:24.04 \
  --setup-hook "apt-get update && apt-get install -y nodejs npm" \
  --start-command "cd /workspace && npm install && npm start" \
  --port 3000 \
  --size-class medium
heyvm archive-dir ./my-node-app --name my-app-v1
heyvm update my-app --archive ar-XYZ
```

### Deploy with health check
```bash
heyvm create --cloud --name my-api --backend libvirt --region US \
  --image ubuntu:24.04 \
  --start-command "cd /workspace && npm start" \
  --port 8080 \
  --health-path /health \
  --health-timeout 60s
heyvm archive-dir ./my-api --name my-api-v1
heyvm update my-api --archive ar-XYZ
```

### Deploy with environment variables
```bash
heyvm create --cloud --name my-app --backend libvirt --region US \
  --image ubuntu:24.04 \
  --start-command "cd /workspace && node server.js" \
  --port 3000 \
  --env NODE_ENV=production \
  --env DATABASE_URL=postgres://...
heyvm archive-dir ./my-app --name my-app-v1
heyvm update my-app --archive ar-XYZ
```

### Update an already-deployed sandbox with new code

Two patterns:

**Re-deploy** (replaces the cloud sandbox; gets a fresh URL unless you
keep `--name`): rebuild the local sandbox with the new code, then
`heyvm create --deploy-from` again with the same `--name` to overwrite.

**Replace mount** (keeps the same cloud sandbox; faster, no boot):
```bash
heyvm archive-dir ./my-app --name my-app-v2     # archive the new code
heyvm update my-app --archive <v2-archive-id>   # swap in place
```

### Develop locally, then ship

```bash
# Develop in a local sandbox
heyvm create --name staging --backend libvirt --mount ./app:/workspace
heyvm exec staging -- npm install
heyvm exec staging -- npm run build
# ... iterate ...

# Ship that sandbox to the cloud
heyvm create --deploy-from staging --name production --backend libvirt --port 8080
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `HEYO_ARCHIVE_TOKEN` | JWT token for archive authentication |
| `HEYO_CLOUD_URL` | Cloud server URL (default: `https://server.heyo.computer`) |
| `API_HOSTNAME` | Required for `heyvm bind` on **local** sandboxes only |

## Workflow Guidance

When the user asks to deploy:

1. **Default to `--cloud`.** Three commands, works on any host:
   - `heyvm create --cloud --name <name> --backend libvirt --region US --image ubuntu:24.04 --start-command '<cmd>' --port <p>`
   - `heyvm archive-dir <dir> --name <name>-v1`
   - `heyvm update <name> --archive <archive-id>`
2. **Verify.** Use the printed URL, or `curl https://<slug>.heyo.computer/`
   from the local terminal. The `--health-path` flag bakes the wait into
   step 1.
3. **Iterate.** New code = `heyvm archive-dir` + `heyvm update <name>`.
   The sandbox restarts and the start command picks up the new files.
4. **Scale.** `heyvm resize <name> --size-class <c>` adjusts compute.
5. **Only use `--deploy-from` if** (a) you're on Linux, (b) you've
   iterated on a local sandbox with installed packages / configured
   services, and (c) you want to ship that exact rootfs.

When the user provides `$ARGUMENTS`:

- `/deploy <local-sandbox>` (Linux only) → `heyvm create --deploy-from <local-sandbox> ...`.
- `/deploy <directory>` or `/deploy <project>` → use the **`--cloud` flow**.
  Ask for the app name, port, and start command if not implied. Never
  invoke `heyvm deploy <directory>` — that form is rejected by the CLI.

If `$ARGUMENTS` is empty, ask the user what they want to deploy or manage.
