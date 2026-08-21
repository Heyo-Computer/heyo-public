# heyvmd storm hardening — scope

Companion-repo work (`heyo/mvm-ctrl`), scoped from the pooler side after the
August 2026 disk incident. Goal: a burst of VM lifecycle work must never take
the daemon — and with it every running VM — down.

## Status — 2026-08-20

**P0 and P1 implemented** (both repos, uncommitted):

- P0 instrumentation: `mvm-ctrl/src/runtime_lag.rs` heartbeat, reported as
  `runtimeLagMs {current, p99_60s, max_60s}` in `GET /health`; `timed_write!`
  macro on every `handles`/`metadata` write acquisition in `sandbox.rs`
  (warns ≥500ms with file:line). `heyvmd-healthcheck.sh` logs the lag every
  poll and flags p99 ≥ `HEYVMD_HEALTHCHECK_LAG_WARN_MS` (500).
- P0 watchdog: already patient (8 consecutive, /health-only, 600s cooldown);
  now lag-aware in its logging. Restart is no longer catastrophic (below).
- P1 decouple: `mvm-ctrl/src/driver/vmm_console.rs` — VMM spawned in its own
  session with stdin ← per-VM FIFO (held O_RDWR), stdout → `console.log`
  (tailed), stderr → `vmm.stderr.log`, a reaper task owning the `Child`;
  `Drop` no longer kills a committed VM; reattach reopens FIFO + tail, so a
  restarted daemon gets exec/shell/console back, not the SSH fallback.
  `deploy/supervisor/heyvmd.conf`: `stopasgroup=false`, `killasgroup=false`.
- P1 locks: fixed every guard-across-await on `metadata` (create_link,
  stop_inner, checkpoint, TTL reaper persist, usage poller, TTL scan,
  interactive shell, two get_sandbox blocks, download_file, list_files) and
  the handles→metadata lock-order inversion in the rootfs sweeper.

**First deploy caveat:** VMs started by a pre-detached-console daemon still
hold pipes into that daemon — the restart that brings the new daemon up kills
them one last time. Every VM started by the new daemon survives thereafter.

**Restart drill (acceptance):** with ≥20 running VMs started by the new
daemon, `supervisorctl restart heyvmd` → `pgrep -c firecracker` unchanged,
`/deployed-sandboxes` lists them Running within seconds, `heyvm exec` works
over serial (log: "reattached detached serial console"), console logs keep
flowing, and the pooler's warm entries keep serving.

- P2 (done): run-dir fs work on the start path (state dir, FIFO, console
  logs, snapshot dir, checkpoint/fork dirs) moved to `spawn_blocking` /
  `tokio::fs`; KVM stopped-GET now reports Stopped from metadata (never
  boots); Firecracker delete skips the warm-up GET (handle-less cleanup
  covers running and stopped), and `stop_vm_by_id` now also removes the
  vsock log sockets for parity with the handle path. Audit confirmed the
  big blocking items (rootfs copy, debugfs, fallocate, TAP) were already
  off-runtime; what remained on `/tmp` (socket unlinks) was left sync on
  purpose — tmpfs, not the saturated data array.

Remaining: P3 UDS transport — stashed as `docs/plan-pooler-uds-transport.md`.

## What we verified is NOT the problem (already done in heyo)

The pooler's bring-up gate comment (`vm.rs`, `DEFAULT_BRINGUP_SLOTS`) describes
blocking per-boot work parking the daemon's async workers. That description is
**out of date** — the heavy items are already off the runtime:

| work | where | status |
|---|---|---|
| `debugfs` SSH key injection | `firecracker.rs` `start()` | `spawn_blocking` |
| per-VM rootfs copy | `reflink_or_copy` | FICLONE on `spawn_blocking`, else `tokio::fs::copy` |
| data-disk `fallocate` | `ensure_data_disk` | `spawn_blocking` |
| TAP / iptables / `ip link` | `tap_networking.rs` | `tokio::process::Command` |
| persisted metadata reads, slug lookup | `persistence.rs` | `spawn_blocking` + in-memory index (seeded once) |
| `/health` | `api.rs` `health_check` | pure payload, takes no locks |

So "parked workers" is at most a residual (see item 4). The storm amplifiers
are elsewhere.

## Findings — what actually amplifies a storm

1. **VM lifetime is coupled to daemon lifetime.** Firecracker is spawned with
   `stdin/stdout/stderr` **piped to the daemon — those pipes ARE the guest
   serial console (ttyS0)** — in the daemon's process group, with no shutdown
   handler. A daemon restart (watchdog, deploy, crash) closes the pipes and/or
   delivers the supervisor's group signal: every VM dies. Every active schema
   then cold-starts at once against the fresh daemon, which wedges again.
   This is the loop the pooler gate exists to avoid, and it makes *every*
   other failure catastrophic instead of merely slow. **Highest value item.**
   The reattach machinery for surviving VMs already exists
   (`list_running_vm_ids`, `create_reattach_handle`,
   `reconcile_firecracker_from_running_vms`) — what is missing is VMs that
   survive to be reattached.

2. **Semantic boots on read/delete paths** (fixed 2026-08-20, uncommitted):
   `GET /deployed-sandboxes/{id}` on a stopped persisted VM created+started
   it; `delete` called that GET as a warm-up and so booted VMs to destroy
   them; handle-less deletes never removed the run dir (the root cause of
   the multi-TB orphan fleet). Residuals: KVM's stopped case still falls
   through to a boot in `get_sandbox`; `delete` still issues the warm-up GET
   (now cheap, but needless for Firecracker given `purge_vm_state_dirs`).

3. **Lock convoys on `handles` / `metadata` (tokio `RwLock`).** tokio's
   RwLock is write-preferring: one writer waiting on `handles.write()` blocks
   every *new* reader. Writers are taken on every create/reattach/delete;
   readers include the listing's per-handle status probes. If any reader
   holds its guard across a probe (bounded by `LIST_STATUS_PROBE_TIMEOUT`,
   but per handle), a create burst + a listing = API-wide stall with **zero
   parked threads** — which is indistinguishable from "parked workers" from
   outside. `get_sandbox` already avoids one such window (see its comment
   about `snapshot_sandbox_infos`); the rest needs an audit. *To verify with
   instrumentation before changing.*

4. **Residual synchronous fs ops on lifecycle paths.** `std::fs::write` of the
   VM config JSON (`firecracker.rs:~1875`), virtual-network allocation
   persistence, `std::fs::remove_file` in `stop()` (sockets, vsock paths,
   logs), `create_dir_all` for state dirs. Microseconds on a healthy fs;
   **seconds each on a saturated ext4 journal** (the incident's `rm -rf`
   crawl was the same journal). Each one parks an async worker for exactly
   as long as the journal is busy. Cheap to convert; low priority alone.

5. **Watchdog policy is hair-trigger for a catastrophic action.** A failed
   `/health` within a short `--max-time` restarts a daemon whose restart
   kills the fleet (item 1). Slow ≠ dead; the policy has to know the
   difference, or the restart has to stop being catastrophic.

6. **No runtime-health instrumentation.** Nothing distinguishes parked
   workers (item 4) from lock convoys (item 3) from a slow disk. Every
   incident so far was diagnosed by reading code.

## Work items

Effort: S = hours, M = days, L = a week+.

### P0 — instrumentation (S)
- Scheduler-lag heartbeat: a task sleeps 1s in a loop and records wake lag;
  expose `runtime_lag_ms` (current + p99 over 60s) in `/health`. Parked
  workers show up as lag; lock convoys don't — that's the discriminator.
- Lock-wait timing: wrap `handles.write()` / `metadata.write()` acquisitions
  with elapsed-time logging (warn above 500ms, with the caller's label).
- Lifecycle phase timings already logged (`[+{:?}]` in `start()`); add the
  total to the create/start API response so the pooler can chart it.
- Acceptance: a staged storm (below) shows which of lag / lock-wait / phase
  spikes; the answer decides P1 ordering.

### P0 — watchdog de-escalation (S)
- Require N consecutive `/health` failures (N≥3) over a longer window, and
  treat `runtime_lag_ms` below a threshold as "alive but slow — do not
  restart".
- Until item 1 ships, a restart is fleet-wide data-plane loss; the policy
  must price it that way.

### P1 — decouple VM lifetime from daemon lifetime (L)
- Spawn Firecracker in its own session (`setsid` / `process_group(0)` via
  `pre_exec`) so supervisor group signals and daemon death don't reach it.
- Move the serial console off the daemon's pipes: a tiny detached console
  pump process (or pty owner) per VM that writes `run/<id>/console.log` and
  serves live subscription over a unix socket; the daemon attaches/detaches
  to it instead of owning the pipe. Phase 1 may simply accept "no live
  console for VMs started before the last daemon restart" — the
  reattach handle already has `firecracker_process: None`.
- Daemon shutdown leaves VMs running; startup reconciles through the
  existing running-VM reattach path.
- Supervisor: `stopasgroup=false`, `killasgroup=false` for heyvmd.
- Acceptance: `supervisorctl restart heyvmd` with 50 running VMs → 0 VMs
  die; listing shows them running within seconds; exec/proxy work; the
  pooler's warm entries survive (its `bring_up_existing` treats a running
  VM as a no-op start).

### P1 — lock-convoy audit (M)
- Rule: never hold a `handles`/`metadata` guard across an `.await` that can
  take longer than a map op. Clone the `Arc` cells out under the guard,
  release, then probe.
- Consider per-entry locking or a sharded map if writers remain hot.
- Acceptance: 100 concurrent `GET /deployed-sandboxes/{id}` + 10 concurrent
  creates + a listing loop → p99 GET < 100ms, zero lock-wait warnings.

### P2 — residuals (S each)
- Convert item-4 sync fs ops to `tokio::fs` / `spawn_blocking`.
- KVM: mirror the Firecracker "report Stopped from metadata, never boot"
  branch in `get_sandbox`.
- Firecracker delete: drop the warm-up `get_sandbox` (stop-by-id + state-dir
  purge already cover both running and stopped); removes a lock round-trip
  and the last boot-shaped code path from delete.

### P3 — hygiene
- Pooler over `heyvmd --socket` (UDS): removes loopback TCP churn under
  probe storms. Not a storm fix; do it in a quiet week.

## Storm drill (verification for all of the above)

Staging host with the pooler's `loadtest` harness (`src/loadtest.rs`):
200 cold creates + a purge (thousands of per-id probes) + a listing loop,
concurrently, against a daemon with P0 instrumentation. Record
`runtime_lag_ms`, lock-wait warnings, boot phase totals, watchdog decisions.
Re-run after each P1 item; the drill is the acceptance test for the whole
scope.

## Rollout order

P0 instrumentation → P0 watchdog → P1 decouple (behind a flag, staged host
first) → P1 locks (guided by P0 data) → P2 → P3.

## Related

- `docs/runbook-disk-health.md` — the incident this scope came out of.
- `docs/plan-cold-start-o1.md` — the pooler-side bring-up work.
- heyo commits (uncommitted as of 2026-08-20): `get_sandbox` no-boot branch,
  `purge_vm_state_dirs` + delete call site.
