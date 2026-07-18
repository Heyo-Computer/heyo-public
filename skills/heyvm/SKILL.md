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
heyvm --heyo-env preview <command> ...
heyvm --heyo-env stage <command> ...
heyvm --heyo-env production <command> ...
```

Use the named environment selector first. It resolves auth/cloud URLs as:

- `preview`: `https://preview.heyo.computer/auth` and `https://preview.heyo.computer/cloud`
- `stage`: `https://stage.heyo.computer/auth` and `https://stage.heyo.computer/cloud`
- `production`: legacy split hosts until production front-door routes exist

Raw URL flags and URL environment variables override the named environment:
`--cloud-url`, `--auth-url`, `HEYO_CLOUD_URL`, `HEYO_AUTH_SERVER_URL`, and
`AUTH_SERVER_URL`. Unset those when verifying `--heyo-env`.

Preview and production backends should be Linux. For cloud dev sandboxes, use
`--cloud --backend libvirt --image ubuntu:24.04` unless the task explicitly
requires a different Linux backend or image.

## Login And Upgrade

```sh
heyvm --version
heyvm --upgrade
heyvm login --email <email>
heyvm --heyo-env preview login --email <email>
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

On macOS, choose the image according to the Apple backend:

- `apple_container` accepts standard OCI registry references such as `node:22`
  from Docker Hub or `ghcr.io/org/image:tag` from GHCR. `heyvm create` pulls a
  missing image automatically.
- `apple_virt` requires a bootable VM image name or ID from the heyvm image
  catalog. It cannot boot an OCI container image directly.

```sh
heyvm create --name node-box --backend apple_container --image node:22 \
  --mount "$PWD:/workspace"
heyvm create --name app-box --backend apple_container \
  --image ghcr.io/org/image:tag
heyvm create --name linux-vm --backend apple_virt \
  --image ubuntu-24.04-node --mount "$PWD:/workspace"
```

Use `heyvm pull` to prefetch or refresh an Apple Container image. It is
optional before `create` because sandbox creation also pulls missing images.

```sh
heyvm pull node:22
heyvm pull ghcr.io/org/image:tag
heyvm pull --force node:22
```

## Cloud Linux Sandboxes

Create a fresh Linux cloud sandbox:

```sh
heyvm --heyo-env preview create --cloud \
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

For a new deployment, pass the archive during creation so the backend
materializes `/workspace` before it runs the start command:

```sh
archive_id=$(heyvm --heyo-env stage archive-dir examples/fremont-fishing --name fremont-fishing-$(date +%Y%m%d%H%M%S) --format json | jq -r '.id')
heyvm --heyo-env stage create --cloud --archive "$archive_id" --name <name> --backend <backend> --image <general-image> --region EU --working-directory /workspace --start-command 'npm ci && npm run build && exec npm start' --port 3000 --health-path /
```

Use a reusable runtime image, not an app-specific image. Image formats remain
backend-specific even though every backend receives the same source archive:

| Backend | General Node image | Source delivery |
| --- | --- | --- |
| `libvirt` | compatible Node-enabled `qcow2` catalog image | archive mounted at `/workspace` |
| `kvm` | Node-enabled `ext4` catalog image | archive converted to a workspace disk |
| `firecracker` | the same compatible Node-enabled `ext4` image | archive converted to a workspace disk |
| `firecracker_containerd` | standard OCI image such as Docker Hub `node:22` | archive copied into the container at `/workspace` |

`firecracker_containerd` pulls ordinary Docker Hub, GHCR, and other OCI
registry references automatically, just like `apple_container`. Do not publish
a Fishing-specific image, and do not use `--mount` with
`firecracker_containerd`; use `--archive` for cloud application source.

To compare the same application concurrently, reuse one archive ID and create
four deployments with distinct names and the image appropriate for each row.
Each deployment gets its own public URL even when all four expose guest port
3000. Verify `/` and the JavaScript and CSS asset URLs before declaring the
comparison successful.

On macOS, prefer `create --cloud` plus `archive-dir`/`update`. Do not use
`--deploy-from` from macOS unless `heyvm create --help` says the selected source
backend can publish to the cloud; local macOS VM backends are not Linux cloud
images.

## Images

Start image work by checking the current CLI surface and inventory:

```sh
heyvm pull --help
heyvm images --help
heyvm images list --help
heyvm images publish --help
heyvm images supported
heyvm --heyo-env stage images list
heyvm --heyo-env preview images list
heyvm images list --local
```

Image concepts:

- Apple Container images are OCI registry references managed by Apple's
  `container` runtime. They can be pulled explicitly with `heyvm pull`, but do
  not need to appear in the heyvm bootable-VM or cloud image catalogs before
  use.
- Apple Virt images are bootable VM artifacts from the heyvm image catalog;
  standard OCI references such as `node:22` are not Apple Virt images.
- Built-in supported images come from the CLI/backend catalog and are not cloud
  catalog rows. Examples: `ubuntu:24.04`, `debian:12`, `alpine:3.23`.
- Backend-local files under paths such as `/var/lib/heyvm/images` are only a
  node cache. They do not show up in `heyvm images list` until bytes are in the
  cloud image store and a cloud DB `sandbox_images` row exists.
- Published image bytes live in S3. The cloud DB `sandbox_images` row controls
  name, owner, public/private status, backend, format, arch, and S3 key.
- Images are not environment-scoped in the image schema today. Catalog sharing
  follows DB/storage topology: stage has its own DB, while preview and
  production currently share a DB. If storage buckets differ, verify the S3 key
  is readable from the cloud environment before treating an image as usable.
- Private images are scoped to the publishing user and deployable immediately.
  Public images require admin review before showing in `images list`.
- First-party reusable images should be registered as admin base images with a
  stable name, backend, arch, and version. User-submitted public images should
  go through `heyvm images publish` and admin review.

Use this workflow when comparing app behavior across images:

1. Inventory built-in and published images in the target environment.
2. Run the app on built-in Linux images first, using `--setup-hook` only for
   missing runtime packages.
3. If the same setup hook is repeated and stable, publish a private reusable
   image from a configured sandbox.
4. Run the app again with the private image.
5. Promote to public only after the image is small, documented, and reusable by
   more than one app.

Example Node app baseline:

```sh
heyvm --heyo-env preview create --cloud \
  --name <app>-ubuntu-node-setup \
  --backend libvirt \
  --region US \
  --image ubuntu:24.04 \
  --size-class small \
  --setup-hook 'apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git jq' \
  --setup-hook 'curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && apt-get install -y nodejs' \
  --start-command 'cd /workspace && npm ci && npm run build && npm start' \
  --port 3000
```

Publish a reusable private image after validating the configured sandbox:

```sh
heyvm --heyo-env preview images publish <sandbox-id-or-name> \
  --name ubuntu-24.04-node22 \
  --description 'Ubuntu 24.04 with Node.js 22 and common app build tools' \
  --private \
  --sysprep
```

Promote a verified backend-local file to a first-party public base image only
after integrity and boot checks pass. Use the target environment intentionally;
stage has a separate catalog, while preview and production share the current
catalog.

```sh
name=ubuntu-24.04-rust-libvirt-20260522
backend=libvirt
arch=linux-amd64
version=20260522
format=qcow2
file=/var/lib/heyvm/images/ubuntu-24.04-rust-libvirt-20260522.qcow2
sha256=$(sha256sum "$file" | awk '{print $1}')

heyvm --heyo-env stage login --email <admin-email>
token=$(jq -r '.access_token' ~/.heyo/token.json)
cloud=https://stage.heyo.computer/cloud

slot=$(curl -fsS -X POST "$cloud/public-images/base/presign" \
  -H "Authorization: Bearer $token" \
  -H "Content-Type: application/json" \
  -d "{\"name\":\"$name\",\"backend\":\"$backend\",\"arch\":\"$arch\",\"version\":\"$version\",\"format\":\"$format\"}")

upload_url=$(printf '%s' "$slot" | jq -r '.upload_url')
curl -fS -X PUT --upload-file "$file" "$upload_url"

curl -fsS -X POST "$cloud/public-images/register-base" \
  -H "Authorization: Bearer $token" \
  -H "Content-Type: application/json" \
  -d "{\"name\":\"$name\",\"backend\":\"$backend\",\"arch\":\"$arch\",\"version\":\"$version\",\"format\":\"$format\",\"sha256\":\"$sha256\"}" | jq .

heyvm --heyo-env stage images list --libvirt
heyvm --heyo-env stage create --cloud --name image-smoke \
  --backend libvirt --region US --image "$name" \
  --start-command 'python3 -m http.server 3000' --port 3000
```

The `sha256` value is client-computed metadata. The server verifies that the
object exists and records the supplied hash; it does not recompute the uploaded
object hash during registration.

Do not register unknown local image IDs such as `im-*` or `img-*` as public
base images until their contents, installed toolchain, boot path, backend,
format, arch, version, integrity hash, and cleanup behavior are recorded.

To make a first-party base image available in every environment, register it in
each distinct catalog DB and upload it to each distinct storage bucket. Today
that means registering once in stage and once in the shared preview/production
catalog. If production uses a different storage bucket from preview, upload the
same deterministic base-image key to production storage too, but do not create a
second DB row when preview and production share the catalog DB.

```sh
set -euo pipefail

name=ubuntu-24.04-rust-libvirt-20260522
backend=libvirt
arch=linux-amd64
version=20260522
format=qcow2
file=/var/lib/heyvm/images/ubuntu-24.04-rust-libvirt-20260522.qcow2
sha256=$(sha256sum "$file" | awk '{print $1}')

upload_base_image() {
  slot=$(curl -fsS -X POST "$cloud/public-images/base/presign" \
    -H "Authorization: Bearer $token" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$name\",\"backend\":\"$backend\",\"arch\":\"$arch\",\"version\":\"$version\",\"format\":\"$format\"}")

  upload_url=$(printf '%s' "$slot" | jq -er '.upload_url')
  curl -fS -X PUT --upload-file "$file" "$upload_url"
}

register_base_image_row() {
  curl -fsS -X POST "$cloud/public-images/register-base" \
    -H "Authorization: Bearer $token" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"$name\",\"backend\":\"$backend\",\"arch\":\"$arch\",\"version\":\"$version\",\"format\":\"$format\",\"sha256\":\"$sha256\"}" | jq .
}

# Stage catalog.
heyvm --heyo-env stage login --email <admin-email>
token=$(jq -er '.access_token' ~/.heyo/token.json)
cloud=https://stage.heyo.computer/cloud
upload_base_image
register_base_image_row

# Shared preview/production catalog.
heyvm --heyo-env preview login --email <admin-email>
token=$(jq -er '.access_token' ~/.heyo/token.json)
cloud=https://preview.heyo.computer/cloud
upload_base_image
register_base_image_row

# If production storage is different from preview storage, repeat the upload
# against production storage too. No additional catalog row is needed when
# production and preview share the same catalog DB; the deterministic S3 key is
# the same, but the object must exist in the bucket production reads from.
heyvm --heyo-env production login --email <admin-email>
token=$(jq -er '.access_token' ~/.heyo/token.json)
cloud=https://server.heyo.computer
upload_base_image
```

Verify visibility and boot in every environment that should consume the image:

```sh
heyvm --heyo-env stage images list --libvirt
heyvm --heyo-env preview images list --libvirt
heyvm --heyo-env production images list --libvirt

heyvm --heyo-env stage create --cloud --name image-smoke-stage \
  --backend libvirt --region US --image "$name" \
  --start-command 'python3 -m http.server 3000' \
  --health-path / \
  --ttl-seconds 600 \
  --port 3000

heyvm --heyo-env preview create --cloud --name image-smoke-preview \
  --backend libvirt --region US --image "$name" \
  --start-command 'python3 -m http.server 3000' \
  --health-path / \
  --ttl-seconds 600 \
  --port 3000

heyvm --heyo-env production create --cloud --name image-smoke-production \
  --backend libvirt --region US --image "$name" \
  --start-command 'python3 -m http.server 3000' \
  --health-path / \
  --ttl-seconds 600 \
  --port 3000

heyvm --heyo-env stage rm --yes image-smoke-stage
heyvm --heyo-env preview rm --yes image-smoke-preview
heyvm --heyo-env production rm --yes image-smoke-production
```

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

## Diagnostics

```sh
heyvm images --help
heyvm mvm --help
heyvm mvm build --help
heyvm test-firecracker
heyvm test-kvm
heyvm test-apple-virt
heyvm prune --help
```

Use `heyvm mvm` only for Firecracker/rootfs image work. For ordinary preview,
stage, or production app work, a cloud Ubuntu image plus setup hooks is usually
simpler until the setup is worth publishing as a reusable image.

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
