# heyo-sdk

Rust SDK for the Heyo cloud sandbox API. Mirrors the TypeScript SDK
(`sdk-ts/` → `@heyocomputer/sdk`) so the same patterns translate across
languages.

P2P tunnels own their accepted forwarding connections: dropping the tunnel
cancels those tasks and releases their sockets. A stream EOF shuts down the
opposite writer while allowing the remaining response direction to finish;
I/O errors terminate both directions. This prevents half-closed connections
from accumulating across CI jobs and tunnel reconnects.

```rust
use heyo_sdk::{Sandbox, SandboxCreateOptions, SandboxSize, HeyoClientOptions, CommandRunOptions};

#[tokio::main]
async fn main() -> Result<(), heyo_sdk::HeyoError> {
    let sandbox = Sandbox::create(
        SandboxCreateOptions {
            image: Some("ubuntu:24.04".into()),
            size_class: Some(SandboxSize::Small),
            ttl_seconds: Some(600),
            ..Default::default()
        },
        HeyoClientOptions::default(),  // reads HEYO_API_KEY
    )
    .await?;

    let out = sandbox.commands().run("uname -a", CommandRunOptions::default()).await?;
    println!("{}", out.stdout);

    sandbox.kill().await?;
    Ok(())
}
```

## Surface

- `Sandbox` — VM lifecycle (`create`, `connect`, `list`, `info`,
  `wait_for_ready`, `kill`, `stop/start/restart`, `set_ttl`, `resize`,
  `resize_disk`, `checkpoint/restore`, `replace_mount`, `bind_port`, `shell`).
- `Sandbox::commands()` — `.run(cmd, opts)` against `/sandbox/:id/exec`.
- `Sandbox::files()` — `.read` / `.write` against `/sandbox/:id/read-file` /
  `/write-file` (base64 over the wire, exposed as `Vec<u8>` / `String`).
- `ShellSession` — persistent interactive shell over the WebSocket protocol,
  with `output()` and `events()` streams, auto-reconnect, and graceful
  `close()`.
- `archive_dir(path, opts)` — tar+gz a local directory, presign + PUT + finalize.
- `Daemon` — the heyvm daemon's own routes, for a controller that manages
  what lives on its host (app-lb is the reference consumer): the typed
  create body (`mounts[{host_path|tree_id}]`, `account_id`, image source,
  workspace archive), `list` with host-only fields, `list_inactive`, `logs`,
  `system_usage`, proxy binds, `storage`/`purge_disks`/`archive_disks`,
  `export_mount`, `upload_tree`/`list_trees`/`tree`/`delete_tree`,
  `upload_image`/`list_images`/`image`. Streams over both transports
  (`HeyoClient::send_stream` / `stream_get`).
- `Namespace` / `Deployments` — the managed app-lb: `Namespace::create/list/get`,
  then `ns.deployments()` for `list/get/create/replace/delete/scale/evict_vm/
  build/update/jobs/exec/shell/metrics` over cloud's `/namespaces/{ns}/lb/…` door.

## Managed app-lb: namespaces and deployments

Heyo runs one app-lb as a platform service. A namespace is your room in it;
deployments registered there are app-lb's — autoscaled VM pools, static
upstreams, or sites — reached through cloud with the same API key. VMs a
deployment boots are billed to the namespace's account like any other sandbox.

```rust
use heyo_sdk::{DeploymentExecOptions, DeploymentSpec, HeyoClientOptions, Namespace, RouteRule, ScalingPolicy, VmSpec};

let opts = HeyoClientOptions { api_key: Some(key), ..Default::default() };
let ns = Namespace::connect("team-a", opts)?;          // or Namespace::create(..) once
let deployments = ns.deployments();

let mut spec = DeploymentSpec::new("web", vec![RouteRule { host: Some("web.example.com".into()), ..Default::default() }]);
spec.vm = Some(serde_json::from_value(serde_json::json!({
    "driver": "firecracker", "image": "my-app", "port": 3000, "size_class": "small"
}))?);
spec.scaling = Some(ScalingPolicy { min_replicas: Some(1), max_replicas: Some(4), ..Default::default() });
deployments.create(&spec).await?;

let run = deployments.exec("web", &DeploymentExecOptions::new("uname -a")).await?;
println!("{}", run.stdout);

// An interactive PTY in one of the deployment's VMs (app-lb picks or wakes
// one; set `sandbox_id` for a specific VM). Needs `admin` on the namespace.
use futures_util::StreamExt;
let shell = deployments.shell("web", heyo_sdk::DeploymentShellOptions::default()).await?;
shell.write(b"ls -la\nexit\n")?;
let mut out = shell.output();
while let Some(chunk) = out.next().await { print!("{}", String::from_utf8_lossy(&chunk)); }
println!("shell on {} exited {:?}", shell.sandbox_id(), shell.exit_code().await);
```

Deployment documents are app-lb's own format and go over the wire verbatim
(snake_case; unknown fields are kept in `extra`). `create` with an existing
`id` *replaces* it and recycles its pool; `replace` (PUT) keeps the pool
unless `vm`/`upstreams` changed.

## Errors

Everything returns `Result<T, HeyoError>`. Notable variants:

- `Authentication` — no API key.
- `InvalidArgument(String)` — 400/422.
- `NotFound(String)` — 404.
- `Api { status, message, body }` — 5xx and friends.
- `Timeout(Duration, String)` — `wait_for_*` budget exceeded.
- `SandboxFailed { sandbox_id, reason }` — provisioning ended in `failed`.
- `SessionExpired { session_id }`, `Connection(String)`, `ShellExit(i32)` —
  shell session failures.

## Configuration

`HeyoClient::new(HeyoClientOptions { .. })` (or any high-level
`Sandbox::create` etc. that takes a `HeyoClientOptions`)
resolves config in this order:

1. Fields set on `HeyoClientOptions` win.
2. `HEYO_API_KEY` env var supplies `api_key` when unset.
3. `base_url` defaults to `https://server.heyo.computer`.
4. `timeout` defaults to 60s.

## Running integration tests

All tests under `tests/` are marked `#[ignore]` because they need a live
cloud API + an API key. Set up `.env` next to `Cargo.toml`:

```env
HEYO_API_KEY=heyo_api_xxxxxxxxxxxx
# optional — defaults to localhost:4445 ("local")
HEYO_ENV=local
# or HEYO_BASE_URL=http://localhost:4445
```

Then run individual tests:

```bash
cargo test --test smoke               -- --ignored --nocapture
cargo test --test shell_protocol      -- --ignored --nocapture
HEYO_DB_ID=db-xxxx cargo test --test db_select_one        -- --ignored --nocapture
HEYO_DB_ID=db-xxxx cargo test --test db_checkout_checkin  -- --ignored --nocapture
```

Tests skip cleanly with an `eprintln!` message when required env is missing,
so plain `cargo test` (no `--ignored`) is a no-op.

## Status / limitations

- `archive_dir` v0 excludes a fixed deny-list (`.git`, `node_modules`,
  `target`, …) plus any caller-supplied directory names. `.gitignore`
  parsing is a follow-up.
- `Sandbox::list_public_images` matches the TS surface but the underlying
  `/public-images` endpoint may not be wired everywhere; expect 404s on
  older clouds.
- The shell test exercises open / IO / resize / close. Forced-reconnect
  testing exists in the TS SDK by reaching into a private socket field;
  Rust integration tests rely on natural disconnects rather than poking
  internals.
