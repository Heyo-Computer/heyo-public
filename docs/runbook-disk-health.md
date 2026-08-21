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

## Draining leftover orphan dirs by hand (2026-08-20 addendum)

Once the offload ladder has run (registry: nearly everything `archived`,
single-digit `live`), what remains on disk is **not** an offload backlog —
it is every kill the old daemon acked without removing the directory. The
audit splits it into two deletable classes; neither needs the daemon:

| audit class | what it is | how to delete |
|---|---|---|
| ORPHAN unowned / offloaded | no daemon record at all (not listed, no `sandboxes/<id>/sandbox.yaml`) | the loop below |
| matched, but registry says archived/compacted | daemon record still exists | dashboard **purge** (needs the patched heyvmd — it probes persisted-only VMs and removes run dirs) |

The loop re-checks the persisted store and open files at deletion time, so
it is safe with VMs running, and it is idempotent — re-run it against a
fresh audit's lists as many times as needed:

```bash
cd /tmp/disk-audit-XXXX            # the newest audit's id lists
RUN=/mnt/md0/heyvm/run; STORE=/mnt/md0/heyvm/sandboxes
del() { id=$1
  [ -e "$STORE/$id/sandbox.yaml" ] && { echo "skip $id (persisted)"; return; }
  sudo fuser -s "$RUN/sb-$id/data.ext4" 2>/dev/null && { echo "skip $id (in use)"; return; }
  sudo rm -rf "$RUN/sb-$id"; }
while IFS=$'\t' read -r id _;     do del "$id"; done < orphan-unowned.txt     # id = col 1
while IFS=$'\t' read -r _ id _ _; do del "$id"; done < orphan-offloaded.txt   # id = col 2
```

Do NOT use the per-id `curl` spot-check the audit prints as the gate: on an
un-patched heyvmd that GET *boots* the VM (~120s each). The local checks
above are the same truth the audit used.

Expect it to be slow on a full or busy array — unlinking a fragmented
multi-GB sparse file walks every extent through the ext4 journal — and to
look like "nothing is happening" while `df` creeps. Stop the pooler during
the drain if it is still archiving through an un-patched daemon (each such
archive inflates the disk it is freeing).

**When a re-audit still shows thousands of orphans afterwards**, decide
between "the loop didn't finish" and "something is minting new orphans":

```bash
comm -12 <(cut -f2 OLD/orphan-offloaded.txt | sort) <(cut -f2 NEW/orphan-offloaded.txt | sort) | wc -l
```

A large overlap = unfinished (interrupted, or still running: `pgrep -af 'rm -rf'`)
→ re-run against the new lists. ~0 overlap = new orphans → a purge or kill
path is still leaking dirs, almost always an **un-patched heyvmd** still in
service (`grep -c 'removed state dir' /var/log/heyvmd/heyvmd.log` is 0 on
the old binary) — fix the deploy first, or every purge re-fills the list.

Why the pooler's own orphan sweep didn't do this: it deletes 100 per pass
and needs `PG_VM_POOL_ORPHAN_SWEEP_SECS` + the drain re-arm build; check
`grep 'orphan-disk sweep' pg-vm-pool.log`. It is the steady-state tool; the
loop is for incident-scale backlogs.

## Disk re-inflating with few sessions (2026-08-20, 36% → 60% in 2h)

Signature: **directory count falls while bytes rise**, stale rootfs copies
jump by hundreds, unowned-orphan average size goes from ~200MB to GBs,
DATA-LOSS rows appear. That is not new orphans — it is *boots nobody asked
for*: each boot clones a rootfs (~190–370MB), re-creates the guest swapfile
(≤2GB) and writes WAL/journal into `data.ext4`; an unclean stop afterwards
leaves the rootfs copy behind. Two sources, both rooted in an un-patched
heyvmd still being in service:

1. **Status probes that boot.** Pre-fix `GET /deployed-sandboxes/{id}` on a
   stopped persisted VM creates+starts it. The pooler's orphan sweep, purge
   probes and kill-verify confirms are all such GETs.
2. **Daemon restarts** (pre-detached-console daemon): every restart kills
   the fleet, the pooler cold-starts it back, repeat.

Discriminators (old code paths log distinctively):

```bash
curl -s localhost:34099/health | grep -c runtimeLagMs     # 1 = patched daemon; 0 = OLD
grep -c 'Restoring sandbox from filesystem' /var/log/heyvmd/heyvmd.log   # boots caused by GETs
grep -c 'Deleting sandbox' /var/log/heyvmd/heyvmd.log; supervisorctl status heyvmd  # deletes; uptime
```

Control, in order: deploy the patched heyvmd; until it is live, **stop the
pooler** (its probes are the boot source). Then `prune-stale-rootfs.sh`
(DELETE=1), the orphan loop above on fresh lists (never DATA-LOSS rows),
and `PRUNE_SWAP=1` on the next reclaim pass for the re-created swapfiles.

**Found and fixed the same evening (both repos, uncommitted):**

- *"High VM count, no sessions, nothing offboarding."* The idle reaper only
  reaps warm entries, so a running VM the pooler isn't tracking (left over
  from a pooler redeploy, a failed idle-stop, or a daemon-side boot) was
  never stopped — and the ladder couldn't pass it either: compaction
  refuses a disk a running VM holds open, backs off 30m→24h, and while
  eligible it blocks the dump-archive fallback. New `untracked-reaper`
  loop stops such VMs after two consecutive sightings; new monitoring tile
  **"running, untracked"** shows the population.
- *Purge killing freshly-claimed spares* (the 67 DATA-LOSS rows): purge
  step 2 used `bound`/`claimed` snapshots from the start of a now-long
  pass. It re-checks both right before each spare kill.
- *Stale rootfs after daemon restarts* (452 of them): a reattached handle's
  `stop()` never removed the rootfs copy (`rootfs_path` empty) — and after
  a restart every handle is reattached. heyvmd's `stop_vm` now removes the
  copy by its canonical path as well.
- 898 daemon deletes vs 162 `removed state dir`: the 736 gap = deletes
  that ran on the old binary before its last restart (or dirs already
  removed by the pooler/loop). `supervisord.log` dates the switch.

**Repeated `pg-<schema>` VMs, ~5 schemas, 70 VMs, all new, no sessions
(2026-08-21):** a retry storm. The pooler log for one schema showed the
chain: heyvmd going unreachable repeatedly (`network error calling
/sandbox/...`, "unknown to heyvmd for 46s — heyvmd restarts drop in-flight
creates"), S3-image restores taking 32–39 minutes per attempt, boots that
reach the guest but miss heyvmd's hard-coded 30s `HEYVM_READY` window under
load ("Received 32 serial lines but no HEYVM_READY"), and a client
reconnecting instantly after every failure. Three pooler fixes landed:
`Provenance` (a VM *created* by a failed bring-up is killed, not left
running), the untracked-reaper stops superseded `pg-<schema>` duplicates,
and a per-schema **bring-up circuit breaker** (1m→15m exponential hold with
an explicit error) so one broken schema can no longer keep a restore storm
running. Root cause to chase on the daemon side: why heyvmd restarts
(`supervisord.log` spawned/exited lines, panics in heyvmd.log) and the
30s readiness ceiling (`wait_for_serial_ready`) being too tight for a
loaded host restoring multi-GB images — the "Last N serial lines" block
in the pooler log shows where init stalls.

Recovering the 67 DATA-LOSS schemas: their data is still in S3 (the
archive is kept on restore). Flip each row back to `archived` in the
registry (tier column, pooler stopped) and the next connect restores it
instead of serving an empty database.

Planned structural levers (pooler): a daemon capability gate (disable
per-id-probe features + dashboard banner when `/health` lacks
`runtimeLagMs`) and `PG_VM_POOL_CREATE_DENY_PCT` (refuse spare builds and
new creates above a disk ceiling — pressure eviction never stopped *adding*).

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
