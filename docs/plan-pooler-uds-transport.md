# Plan: pooler → heyvmd over a unix socket (P3 of the storm-hardening scope)

Hygiene item, not a storm fix — see `plan-heyvmd-storm-hardening.md` for why
(every storm bottleneck was behind the socket, not in front of it). Do this
in a quiet week.

## What it buys

- No loopback TCP churn under probe storms (purge's thousands of per-id
  GETs, orphan-sweep confirms, replenisher listings): no ephemeral ports,
  no TIME_WAIT pile-up, a little less latency per call.
- Filesystem permissions on the socket instead of a TCP port — defence in
  depth if `heyvmd --api-port` is ever bound wider than loopback.

## What exists already

- heyvmd: `heyvmd --socket` serves the same axum app on a UDS (default
  `~/.heyo/heyvmd.sock`); `daemon.json` records `socket_path`.
- Rust SDK: `HeyoClient::local_socket(_with)` and `local_auto(_with)` —
  discovery via `HEYVM_SOCKET` or `daemon.json`, connect-verified, TCP
  fallback. Requests carry `http://localhost` + `Host: localhost`, so the
  daemon's middleware is unchanged. Interactive shell WS works over UDS.
- Pooler: every daemon call builds a client from `vm::local_opts()` →
  `HeyoClientOptions { base_url: daemon_base_url() }` → TCP only.

## Changes

1. **Config** (`src/config.rs`): `PG_VM_POOL_HEYVMD_SOCKET` (path; unset =
   TCP as today). Add to `KNOWN_VARS`. Document in README config table +
   `deploy/supervisor/pg-vm-pool.conf`.
2. **Client construction** (`src/vm.rs`): a single `daemon_client()` that
   returns `HeyoClient::local_socket_with(path, opts)` when the var is set,
   else the current TCP client. Route `Sandbox::connect`, `Sandbox::list`,
   `Sandbox::deploy`, and the dashboard's inventory / `/system/usage`
   fetches (`src/dashboard/model.rs`, `host.rs`) through it. `local_opts()`
   stays as the options builder.
3. **Startup preflight**: if the var is set, connect-verify the socket at
   boot and fail loudly (same shape as `preflight_offload_dirs`) — a stale
   socket path must not silently degrade to "daemon unreachable" at job
   time.
4. **Scripts**: `disk-audit.sh` / `reclaim-disks.sh` keep HTTP (`HEYVMD=`);
   add `curl --unix-socket` support only if operators ask.
5. **Supervisor**: `heyvmd.conf` command gains `--socket
   /mnt/md0/heyvm/heyvmd.sock` (or the default path), pooler conf sets the
   var; both programs run as the same user so no chmod dance.

## Verification

- `tr '\0' '\n' < /proc/$(pgrep -f pg-vm-pool)/environ | grep HEYVMD_SOCKET`
  and a startup log line naming the transport.
- `ss -tn state time-wait '( dport = :34099 )' | wc -l` before/after a purge
  run: should drop to ~0.
- Loadtest harness (`src/loadtest.rs`) end-to-end over UDS: identical
  checkout latency, zero daemon errors.
- Kill the socket file while running: calls fail fast with a clear error
  (no silent TCP fallback inside the pooler — fallback is a *startup*
  decision, not a per-call one, so behaviour is predictable).

## Effort

Half a day including docs and the preflight; no daemon changes required.
