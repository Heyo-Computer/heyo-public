---
name: heyvm
description: Use the heyvm CLI to log in, upgrade, create and manage local or cloud sandboxes, run commands inside VMs, deploy or update apps, expose/connect/share ports and shells, manage cloud databases, build VM images, and diagnose backend readiness.
---

# heyvm

Use this skill for any Heyo VM, sandbox, preview/production deployment, proxy,
database, image, or backend-health task.

## Source Of Truth

Start with the CLI help, then use the chosen subcommand's help before acting:

```sh
heyvm --help
heyvm <command> --help
heyvm <command> <subcommand> --help
```

Prefer the CLI over raw API calls. Use `--format json` when scripting if the
subcommand supports it. Never print, log, or paste passwords, API keys, or
tokens; ask the user to run interactive login when credentials are needed.

## Environment

Global options can be passed before the command or on most subcommands:

```sh
heyvm --cloud-url "$HEYO_CLOUD_URL" --auth-url "$HEYO_AUTH_URL" <command> ...
```

Common preview values:

```sh
export HEYO_PREVIEW_CLOUD=https://preview.heyo.computer/cloud
export HEYO_PREVIEW_AUTH=https://preview.heyo.computer/auth
```

Preview and production backends should be Linux. For cloud dev sandboxes, use
`--cloud --backend libvirt --image ubuntu:24.04` unless the task explicitly
requires a different Linux backend or image.

## Login And Upgrade

```sh
heyvm --version
heyvm --upgrade
heyvm login --email <email>
heyvm --auth-url "$HEYO_PREVIEW_AUTH" --cloud-url "$HEYO_PREVIEW_CLOUD" login --email <email>
```

If login fails with TLS/certificate errors, do not bypass verification. Report
the exact host and certificate problem; that is a server-side route/cert issue.

## Local Sandboxes

Check host/backend readiness first when the user reports VM failures:

```sh
heyvm system
heyvm backends --format json
```

Create and work in a local sandbox:

```sh
heyvm create --name <name> --backend <backend> --image ubuntu:24.04 \
  --mount "$PWD:/workspace" --open-port 3000
heyvm list --all
heyvm exec <id-or-slug> --cwd /workspace -- <command> <args>
heyvm exec <id-or-slug> --session dev --cwd /workspace -- <command> <args>
heyvm sh <id-or-slug>
heyvm stop <id-or-slug>
heyvm rm --yes <id-or-slug>
```

Use `--agent codex` when the sandbox should be provisioned for Codex CLI work.
Use `--needs-network` when package installs, external APIs, or agent CLIs need
network access.

## Cloud Linux Sandboxes

Create a fresh Linux cloud sandbox:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" \
  create --cloud \
  --name <name> \
  --backend libvirt \
  --region US \
  --image ubuntu:24.04 \
  --size-class small \
  --agent codex \
  --setup-hook 'apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git jq' \
  --port 3000
```

Cloud sandboxes do not bind-mount the host working directory at creation time.
Ship local source to `/workspace` with an archive:

```sh
archive_id=$(heyvm archive-dir . --name <name>-$(date +%Y%m%d%H%M%S) --format json | jq -r '.id')
heyvm update <sandbox-id-or-name> --archive "$archive_id"
```

On macOS, prefer `create --cloud` plus `archive-dir`/`update`. Do not use
`--deploy-from` from macOS unless `heyvm create --help` says the selected source
backend can publish to the cloud; local macOS VM backends are not Linux cloud
images.

## Networking And Access

```sh
heyvm bind <sandbox> 3000
heyvm bind --private <sandbox> 3000
heyvm proxy start 3000
heyvm proxy list
heyvm share <sandbox> --name <shortname>
heyvm ssh <heyo-ticket-or-shortname> -- <command>
heyvm connect <heyo-ticket-or-shortname>
heyvm mount <sandbox>
heyvm port-forward <sandbox> <local-port>:<guest-port>
```

Use `bind` for deployed sandbox ports and `proxy`/`share`/`connect`/`ssh` for
P2P access.

## Databases

Use `heyvm db` for cloud databases. Check help for the current engine and SQL
surface:

```sh
heyvm db --help
heyvm db create --help
heyvm db list
heyvm db exec <database> -- <sql>
heyvm db shell <database>
```

Select DuckDB only when the task calls for OLAP/columnar workflows; default to
SQLite for app state and transactional data.

## Images And Diagnostics

```sh
heyvm images --help
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" images list
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" images list --libvirt
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" images list --kvm
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" images list --firecracker
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" images supported
heyvm mvm --help
heyvm mvm build --help
heyvm test-firecracker
heyvm test-kvm
heyvm test-apple-virt
heyvm prune --help
```

Use `heyvm mvm` only for Firecracker/rootfs image work. For ordinary preview or
production app work, a cloud Ubuntu image plus setup hooks is usually simpler.

Always list images against the target environment explicitly. Do not infer that
preview, stage, and production expose the same catalog just because they share a
database or backend host. Treat image readiness as environment plus backend
specific until `images list --<backend>` and a smoke VM prove it.

Current public cloud image families observed on preview/production include:

- `ubuntu:24.04`, `debian:12`, `alpine:3.23`: built-in supported images for
  `libvirt` and `firecracker`.
- `python-v3.12.3`, `bun`: published `libvirt` qcow2 images.
- `agents-v1`, `ubuntu`, `debian`, `kvm-bun`: published `kvm` ext4 images.
- `nginx-firecracker`: published `firecracker` ext4 image.
- `ubuntu-24.04-node`, `ubuntu-24.04-rust`, `ubuntu-24.04-elixir`,
  `ubuntu-24.04`: local `apple_virt` macOS images, not Linux cloud images.

For app work on preview or production, prefer `--backend libvirt --image
ubuntu:24.04` unless there is a tested published image for that backend. For
fast repeatable fishing app VMs, create separate image roles:

- App/runtime image: Ubuntu 24.04 plus Node 22, git, ca-certificates, jq, and
  the app's stable system packages. Keep source code in the workspace/archive,
  not baked into the base image unless the image is versioned for a release.
- Collector/browser image: Ubuntu 24.04 plus Node 22, Chrome, Xvfb, fluxbox,
  x11vnc/noVNC, websockify, and OpenClaw. Bake tools only; never bake Facebook
  credentials, cookies, raw private-group exports, or member-identifying data.
- Firecracker runtime image: use `heyvm mvm build` and a small rootfs only when
  the app can run without systemd and without a browser collector. Include SSH,
  an `/init.sh`, exposed ports, and the `HEYVM_READY` marker.

When a setup hook becomes stable and slow enough to justify reuse, provision a
sandbox once, publish it as a private image if appropriate, then verify it:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" images publish <sandbox-id> \
  --name <image-name> \
  --description "<short purpose>" \
  --private
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" images list --libvirt
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" --auth-url "$HEYO_PREVIEW_AUTH" create --cloud \
  --name <smoke-name> \
  --backend libvirt \
  --region US \
  --image <image-name> \
  --port 3000
```

If an image published in one environment is needed in another, list it in the
destination environment before using it. If it is missing, publish/import it
there or use setup hooks until the image promotion path is explicit.

## Operational Rules

- Confirm the target environment before creating cloud resources: production,
  preview, stage, or local.
- If there is only one backend, list and clean up retained or stale sandboxes
  before retrying capacity-heavy work.
- Stop or remove sandboxes only when they are clearly created for the current
  task or the user asks for cleanup.
- For long setup hooks, prefer a custom image only after the setup is stable and
  repeated often enough to justify maintaining it.
- Record commands and resource IDs in the task notes or tutorial when the user
  asks for reproducible steps.
