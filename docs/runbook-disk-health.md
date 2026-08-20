# Runbook: VM-disk health — recovery and steady state

Written after the 2026-08 investigation that started as "the dashboard says
5000 VMs but the run dir holds 7000 disks" and ended with a 3.1TB run dir,
10,367 `sb-<id>/` directories, and an audit screaming 7315 data-loss orphans.
Almost none of that was data loss. This documents what was actually wrong, the
one-time recovery, and the configuration that keeps the disk healthy.

## What was actually wrong (read this before trusting any count)

**1. heyvmd's API listing is in-memory, not a census.** `GET
/deployed-sandboxes` (what the dashboard and `heyvm list` show) reflects only
sandboxes the current daemon *process* has touched: running VMs reattached at
boot, plus anything created/started since. The durable record set is the
persisted store — `<heyo-data>/sandboxes/<id>/sandbox.yaml` — which the
per-id endpoint (and the pooler's orphan sweep, and the pooler's
reconnect-by-id path) answer from. Consequences:

- Right after a heyvmd restart the listing collapses to ~the running set.
  A fleet-of-5000 dashboard count and a fleet-of-19 count can both be
  "correct" hours apart, with zero data loss in between.
- Any tool that classifies disks off the listing alone (the pre-2026-08
  `disk-audit.sh` did) will mislabel every stopped-but-persisted sandbox as an
  orphan after a restart. The audit now merges the persisted store
  (auto-discovered at `<run-dir>/../sandboxes`, override with
  `DAEMON_SANDBOXES_DIR=`) and prints listed vs persisted-only counts
  separately. If it ever warns "persisted store not found", fix that before
  acting on any bucket.
- The pooler is *safe across daemon restarts*: reconnects resolve by known id
  → per-id GET → filesystem fallback → the VM restarts on its existing data
  disk. No empty-rebind happens for any schema whose record exists.

**2. The disk-reclaiming machinery was pointed at the wrong directory.** The
pooler config carried `PG_VM_POOL_RUN_DIR`/`PRESSURE_PATH`/reclaim-cmd paths of
`~/.heyo/run` while heyvmd ran from `/mnt/md0/heyvm/run`. Every reclaiming
feature treats an empty/absent run dir as "nothing to do", so the orphan
sweep, offline reclaim, compaction, and pressure eviction all silently
no-opped while ~2000 genuinely-forgotten directories and terabytes of disk
slack accumulated. `deploy/supervisor/pg-vm-pool.conf` now pins the real
paths and documents the verification steps; the pooler also warns at startup
when the run dir holds no `sb-<id>/`.

The *genuine* leak (directories with no persisted record) comes from kills the
daemon acked but didn't finish — `DELETE` treated as success on 404, records
dropped while directories survive — concentrated in the cold-start-wedge era
(see `docs/plan-cold-start-o1.md` and the 2026-08-18 incident note in
`purge-unbound-sandboxes.sh`). The orphan sweep exists precisely for that
backlog; it just has to be enabled and pointed at the right directory.

## One-time recovery

Order matters only where noted. Everything here is idempotent.

**1. Get true numbers** (read-only):

```bash
REGISTRY_TSV=~/.heyo/pg-vm-pool/registry.tsv ./disk-audit.sh /mnt/md0/heyvm/run
```

Confirm the header shows a scanned persisted store and a "persisted only"
count in the thousands. Expected shape after the 2026-08 incident: matched ≈
records on disk, ORPHAN ≈ 2–3k (mostly unowned/offloaded), DATA-LOSS small.

**2. Triage what's left in DATA-LOSS** (registry-live disks with *no record
anywhere*). Spot-check per-id first — column 2 of `orphan-DATA-LOSS.txt` is
the sandbox id; a 200 means it's not an orphan (re-run the audit). For real
ones, do NOT let clients connect to those schemas (the pooler would rebind
them to fresh empty VMs): quarantine via `emergency-drain.sh orphans`
(sparse-copies live-tier orphan dirs to a spare disk) or exhume host-side with
the `dump-oldpg.sh` pipeline (`--adopt-unbound` identifies owners from the
database name inside each disk and flags collisions with rebound schemas).

**3. Deploy the corrected pooler config**:

```bash
sudo supervisorctl reread && sudo supervisorctl update pg-vm-pool   # NOT restart: restart skips environment= changes
tr '\0' '\n' < /proc/$(pgrep -f pg-vm-pool | head -1)/environ | grep -E 'RUN_DIR|PRESSURE|RECLAIM'
```

Update the sudoers pin to match the reclaim command's new argument
(`/mnt/md0/heyvm/run`), or every reclaim run fails at `sudo -n`.

**4. Reclaim the easy space now** (safe with the pooler running; the reclaim
script skips disks running VMs hold open):

```bash
./prune-stale-rootfs.sh /mnt/md0/heyvm/run                   # ~45GB of dead rootfs clones
sudo ~/.heyo/bin/reclaim-disks.sh /mnt/md0/heyvm/run --shrink --prune-swap
```

**5. Let the sweeps drain the rest.** The orphan sweep deletes only dirs whose
per-id probe 404s AND whose schema is offloaded/unreferenced, capped at 100
dirs per 900s pass — a 3000-dir backlog drains in ~8 hours. Live-tier orphans
are logged at `error` and never deleted; each one is a step-2 case. The
compact (1h idle) and archive (1d idle) tiers then shrink the *bound* fleet
toward "schemas active in the last hour", and pressure eviction backstops at
85% disk usage.

## Steady state — and how to know it's holding

Five mechanisms, all driven by the supervisor env (see
`deploy/supervisor/pg-vm-pool.conf`):

| mechanism | env switch | what it prevents |
|---|---|---|
| idle reap (stop VM) | `IDLE_TIMEOUT_SECS=300` | RAM/CPU held by idle VMs |
| compact tier | `COMPACT_AFTER_SECS=3600` | stopped full-size ext4 images |
| S3 archive (+image fallback) | `ARCHIVE_AFTER_SECS=86400`, `IMAGE_ARCHIVE=1` | cold data on the host at all |
| orphan sweep + reclaim | `ORPHAN_SWEEP_SECS=900`, `RECLAIM_CMD=…` | forgotten dirs; sparse-disk ratchet |
| pressure eviction | `PRESSURE_PATH`, high/low pct | ENOSPC death spiral |

Ongoing checks:

- **Run `disk-audit.sh` periodically** (weekly cron, or after any incident /
  daemon deploy). Healthy: ORPHAN a few dozen at most (sweep churn),
  DATA-LOSS zero, "persisted only" large — that last one is *normal*, not a
  problem.
- **After every pooler deploy**, verify the environment actually loaded
  (`supervisorctl update`, then read `/proc/<pid>/environ`) and that the
  startup log does not warn about an empty run dir. This is the failure mode
  that caused the 3TB pileup; it is silent by design of everything downstream.
- **After a heyvmd restart**, expect the dashboard VM count to collapse and
  regrow. That is the listing's in-memory semantics, not loss. (Upstream
  improvement, if it ever matters: merge the persisted store into heyvmd's
  listing, metadata-only, behind the existing reconcile interval.)
- **Watch the pooler log for** `data-loss orphan` (error level — a live
  schema's disk with no daemon record; act before a client reconnects) and
  `killing the VM failed (orphaned)` (the leak-in-progress signal the sweep
  cleans up behind).
