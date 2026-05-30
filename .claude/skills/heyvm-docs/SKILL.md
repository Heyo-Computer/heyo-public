---
name: heyvm-docs
description: High-level overview of the Heyo platform and the heyvm CLI — what it is, the core features (sandboxes, deploy, build, proxy/connect, image management), and an index of the other heyvm-* skills so the right one can be picked for a given task. Use when the user asks "what is heyvm", "what can heyvm do", "which skill should I use", or any first-orientation question.
allowed-tools: Read, Grep
---

# heyvm — Overview & Skill Index

`heyvm` is the CLI and management surface for the Heyo platform's sandbox runtime. It creates and runs isolated VM-backed sandboxes across multiple backends (Firecracker on Linux, Apple Virtualization Framework / Apple Container on macOS, Docker, Microsandbox), manages their images and lifecycle, deploys apps to the Heyo cloud, and exposes P2P networking primitives for connecting to remote sandboxes.

This skill is an **index and orientation page**, not a how-to. For any concrete task, defer to the per-area skill listed below.

## What heyvm is

- A Rust binary built from `mvm-ctrl/` (`cargo build --release -p heyvm`).
- Ships a CLI (`heyvm <subcommand>`), an HTTP API (`heyvm --api`), and a Terminal UI (default when run with no args).
- Backed by per-platform VM drivers under `mvm-ctrl/src/driver/`: Firecracker (Linux/KVM), Apple Virtualization Framework (`apple_virt`, macOS arm64), Apple Container (`apple_container`, macOS), Docker (any host), Microsandbox (msb), Hyper-V (Windows).
- Talks to the Heyo cloud at `https://server.heyo.computer` for deploy, image distribution, and shared proxy endpoints. Auth lives at `~/.heyo/token.json` after `heyvm login`.

## Core features

| Area | What it does |
|------|--------------|
| **Sandboxes** | Create, start, stop, restart, list, exec — local VM sandboxes across multiple backends. Persistent COW rootfs clones per sandbox, mounts, port bindings, snapshots/checkpoints (per-backend). |
| **Deploy** | One-shot publish of a local app to a cloud sandbox: archives the source, creates the sandbox, binds ports, sets up custom domains, waits for readiness. |
| **Image build** | Build Firecracker / apple_virt rootfs images from Dockerfiles via `heyvm mvm build` / `heyvm images build`. Local-only or push-to-cloud. |
| **Image management** | Pre-built public images (e.g. `ubuntu-24.04`, `ubuntu-24.04-rust`, `alpine:3.21`) auto-downloaded from S3 on first use. Custom images cached at `~/.heyo/images/<backend>/<name>/`. |
| **P2P proxy / connect** | Expose local ports to the internet over iroh (`heyvm proxy`), connect to remote sandboxes (`heyvm connect`, `heyvm sh`, `heyvm bind`), share interactive shells (`heyvm share`), mount remote workspaces. No public IP required. |
| **Host diagnostics** | `heyvm test-apple-virt` / `test-firecracker` plus per-platform readiness checks. |
| **TUI** | Live dashboard of running sandboxes, metrics, logs, exec. Default surface when `heyvm` runs with no args. |

## Skill index — which skill for which task

When the user's request matches one of these triggers, invoke that skill (`Skill` tool with `skill: heyvm-...`). For ambiguous requests, ask which they mean before guessing — these surfaces overlap.

| Skill | Use it when the user wants to… |
|-------|-------------------------------|
| **heyvm-login** | Authenticate to the Heyo platform (`heyvm login`, email/password or API key). Always the first step before any cloud operation. |
| **heyvm-system** | Diagnose host setup — KVM/Firecracker on Linux, Apple Virt entitlements + images on macOS — and run end-to-end smoke tests (`heyvm test-apple-virt` / `test-firecracker`). First stop when nothing is working. |
| **heyvm-sandbox** | Manage **local** sandbox lifecycle: `heyvm create / start / stop / restart / list / exec`, configure mounts, ports, backend selection (`--backend-type`). Day-to-day local sandbox work. |
| **heyvm-firecracker** | Author Dockerfiles that produce Firecracker-compatible ext4 rootfs images and build them with `heyvm mvm build`. Linux-only image authoring. |
| **heyvm-deploy** | Push code to a Heyo cloud sandbox. **Happy path:** `heyvm create --cloud --name X --backend libvirt --image ubuntu:24.04 --start-command '...' --port P` then `heyvm archive-dir <dir> --name X-v1` then `heyvm update X --archive ar-...`. Works on macOS and Linux. **Advanced (Linux only):** `heyvm create --deploy-from <local-id>` snapshots a local sandbox's rootfs and ships it. The legacy `heyvm deploy <directory>` form is rejected with a prescriptive error. |
| **heyvm-proxy** | P2P networking: expose local ports (`heyvm proxy`), connect/SSH/share to remote sandboxes (`heyvm connect`, `heyvm sh`, `heyvm share`), mount remote workspaces (`heyvm bind`). All powered by iroh — no public IP needed. |
| **heyvm-database** | Manage cloud SQLite databases: `heyvm sqlite create / list / get / delete / regions`, run SQL with `exec` / `shell`, or mint libsql-compatible connection tokens via the `Database` SDK class for sustained traffic from external clients. |

### Quick decision tree

- "I can't run anything / setup is broken" → **heyvm-system**.
- "Log me in" / "what account am I on" → **heyvm-login**.
- "Make / start / stop / shell into a sandbox **on this machine**" → **heyvm-sandbox**.
- "Build a custom Firecracker image" / "write a Dockerfile for a microVM" → **heyvm-firecracker**.
- "Ship this app to the cloud" / "deploy" / "publish" / "make this URL public" → **heyvm-deploy**.
- "Connect to / share / expose / port-forward / mount a remote sandbox" → **heyvm-proxy**.
- "Run a command on a deployed sandbox" → **heyvm-proxy** (`heyvm sh <id>`) or **heyvm-sandbox** (`heyvm exec`).
- "Create a SQLite database" / "run SQL against a Heyo db" / "give my agent a database" → **heyvm-database**.

## Backend cheatsheet

heyvm supports several backends. Pick by platform and use case:

| Backend | Platform | When to pick it |
|---------|----------|-----------------|
| `firecracker` | Linux (KVM) | Default for cloud / Linux hosts. Fast boot, strong isolation, ext4 rootfs. |
| `apple_virt` | macOS arm64 | Default for local macOS dev. Native Apple Virtualization Framework, full VM with ext4 + grub-efi. Boots Ubuntu/Alpine. |
| `apple_container` | macOS | Apple's native container CLI (must be installed separately). Lighter weight than apple_virt. |
| `docker` | any | Quick local containers. **No SSH** — `heyvm share` / `heyvm ssh-proxy` are not supported on docker sandboxes. |
| `msb` (microsandbox) | any | When the microsandbox runtime is appropriate. Limited sandbox types (no `Shell`). |
| `hyperv` | Windows | Hyper-V Linux VMs. Less commonly used. |

## Conventions

- **Auth token**: `~/.heyo/token.json`, populated by `heyvm login`. Most cloud commands fail with a clear "log in first" message if missing.
- **Local image cache**: `~/.heyo/images/<backend>/<name>/` (`vmlinuz`, `rootfs.img`, optional `initrd.img` and `grubaa64.efi`).
- **Per-sandbox state**: `~/.heyo/sandboxes/<sb-id>/` (COW rootfs clone, EFI vars, sandbox.yaml).
- **Cloud base URL**: `https://server.heyo.computer` (override via `--cloud-url` or `HEYO_CLOUD_URL`).
- **TUI port**: 34099 (always, can't change). API server port: 3000 (configurable via `--port`).
- **Backends env var**: `MVM_BACKENDS=firecracker,apple_virt` to restrict which backends register at startup. Useful for tests.

## Source-of-truth pointers

When this skill's content goes stale, the canonical sources are:

- `mvm-ctrl/README.md` — module overview.
- `mvm-ctrl/src/driver/mod.rs::SUPPORTED_IMAGES` — current public image list per backend.
- `mvm-ctrl/src/cli.rs` — full CLI surface (subcommands, flags).
- `mvm-ctrl/src/api.rs` — local HTTP API routes.
- `cloud/src/handlers/` — cloud API routes (deploy, exec, files, shell-session).
- `docs/content/docs/` — long-form user docs.
- `mvm-ctrl/install/` — image build + upload scripts (`build-apple-virt-image.sh`, `build-apple-virt-ubuntu-images.sh`, `upload-apple-virt-image.sh`).

Always check those before answering specifics — image names, flag spellings, and S3 layouts have shifted multiple times.
