# pg-fc

This repo has two parts that fit together: a **Firecracker build** that
produces a rootfs image booting straight into Postgres, and **pg-vm-pool**, a
connection pooler that runs many of those microVMs behind a single Postgres
endpoint — one VM per schema, created and stopped/restarted on demand. The
Firecracker image is the unit the pooler manages; the pooler is what a real
client actually connects to.

## Prerequisites

Linux only — Firecracker needs KVM, so there's no macOS host support.

**To build the Postgres rootfs image** (`build-rootfs.sh`):

| dependency | why |
|---|---|
| Docker | builds the image from `Dockerfile` before it's flattened |
| `e2fsprogs` (`mkfs.ext4`) | formats the output image |
| root/`sudo` | needed to loopback-mount the image while the container fs is exported into it |

**To run `pg-vm-pool` and boot the VMs it manages:**

| dependency | why |
|---|---|
| Rust (edition 2024 toolchain) | builds `pg-vm-pool` (`cargo build --release`) |
| Firecracker + KVM access (`/dev/kvm`, user in the `kvm` group) | actually boots each per-schema microVM |
| `heyvm` / `heyvmd` (from the sibling `heyo` project) | the VM control plane: `heyvmd` (or `heyvm --api --port 34099`) serves the local sandbox HTTP API pg-vm-pool drives, and the `heyvm` CLI builds the `pg` image (`heyvm mvm build`) |
| `heyo-sdk` crate, `>= 0.1.5` | Rust client for that API; pulled automatically by `cargo build` from crates.io, already pinned in `Cargo.toml` — no separate install needed |

## Postgres VM

A Firecracker rootfs that boots straight into Postgres, with the data directory
on a separate volume mounted at `/workspace`.

### Files

| file | purpose |
|------|---------|
| `Dockerfile` | Debian + Postgres 16 image; ships `init.sh` as `/init.sh` |
| `Dockerfile.pg18` | Same image built with Postgres 18 (`heyvm mvm build -f Dockerfile.pg18 --name pg18`); a data volume initdb'd by one major can't be opened by the other |
| `init.sh` | PID 1 inside the microVM: mounts pseudo-fs + data volume, init's the cluster, exec's postgres |
| `build-rootfs.sh` | Builds the image and flattens it into a bootable ext4 rootfs |

### Design

Firecracker boots a kernel + a flat rootfs and runs `init=` as PID 1 — there's
no systemd. `init.sh` does the minimal init work and then `exec`s `postgres` so
it inherits PID 1 and gets clean SIGTERM shutdown.

The OS rootfs stays disposable; **all database state lives on `/workspace`**,
which is a second Firecracker drive (`/dev/vdb` by default). On first boot the
volume is formatted ext4 and the cluster is `initdb`'d into
`/workspace/pgdata`; subsequent boots just mount and start.

### Build

`build-rootfs.sh` needs Linux (mkfs.ext4 + loopback mount):

```sh
./build-rootfs.sh pg-rootfs.ext4 2G
```

### Boot

```sh
firecracker --api-sock /tmp/fc.sock   # then configure via the API, or use a config:
```

Key settings:
- `boot-source.boot_args`: include `init=/sbin/init.sh console=ttyS0 reboot=k panic=1`
- `drives`: `vda` = `pg-rootfs.ext4` (root), `vdb` = your persistent data disk
- override the data device with the kernel arg `pgdata_dev=/dev/vdc` if needed

Postgres listens on `0.0.0.0:5432`. Reach it over the VM's tap interface.

## pg-vm-pool (per-schema pooler)

This repo also contains `pg-vm-pool` (`src/`), a connection pooler that fronts
many of these microVMs behind a single Postgres endpoint — **one VM per
schema**. The database name in the client's connection string selects the
schema; the pooler lazily creates/restarts the `pg-<schema>` VM, opens a raw-TCP
iroh tunnel to its Postgres, and splices the connection through.

### Using the pooler

Prereqs: a running local heyvmd (`heyvm --api --port 34099`) with the
`POST /sandboxes/:id/tcp-tunnel` endpoint, `heyo-sdk` ≥ 0.1.5, and the `pg`
image built (`heyvm mvm build --local-only -f Dockerfile --name pg`).

```sh
cargo build --release
target/release/pg-vm-pool       # listens on 127.0.0.1:6432 (PG_VM_POOL_LISTEN)

# The dbname selects (and lazily creates) the VM — one per schema:
psql "host=127.0.0.1 port=6432 user=postgres dbname=tenant1"   # -> VM pg-tenant1
psql "host=127.0.0.1 port=6432 user=postgres dbname=tenant2"   # -> VM pg-tenant2
```

First connect to a new schema boots a VM (~2s); reconnects reuse or restart it.
If Postgres dies while its VM stays up (OOM kill, segfault — the VM's PID 1 is
a shell, so the sandbox still reports running), the pooler notices on the next
connect: a short probe distinguishes a dead postmaster (silent port) from one
that's alive but recovering (answers `57P03` during WAL replay), and only the
former triggers an automatic stop/start of the VM — a fresh boot re-runs
`init.sh`, which relaunches Postgres.
Each schema's data lives on its VM's persistent disk and survives stops,
restarts, and idle reaping. Before an idle stop the pooler issues a
`CHECKPOINT` over its warm connection, so the unclean VM kill loses no
acknowledged commits (the VMs run `synchronous_commit=off`) and the next boot
skips WAL replay entirely. The schema→VM binding is persisted in
`PG_VM_POOL_STATE_FILE` (default `~/.heyo/pg-vm-pool/registry.tsv`) so it also
survives pooler restarts.

Connect as `user=postgres`; the VM image's `trust` host auth needs no password
(see the auth note in `init.sh`). `PG_VM_POOL_PASSWORD` does double duty:

- it's what the pooler itself uses for its readiness probe and per-schema
  bootstrap connection, if a VM's Postgres requires password auth (scram/md5)
  instead of `trust`;
- and, separately, if set it's also the password the pooler **requires from
  clients** (a plain `AuthenticationCleartextPassword` challenge) before it
  proxies them anywhere — see "Client auth" below. Unset means no client auth
  gate at all: fine on a loopback-only `PG_VM_POOL_LISTEN`, not once it's
  reachable from elsewhere.

Config via env (all optional):

| var | default | meaning |
|-----|---------|---------|
| `PG_VM_POOL_LISTEN` | `127.0.0.1:6432` | client listen address |
| `PG_VM_POOL_IMAGE` | `pg` | Firecracker image per schema |
| `PG_VM_POOL_SIZE_CLASS` | `micro` | VM resource tier for every schema's VM: `micro` (0.25 CPU, 512MB), `mini` (0.5 CPU, 1GB), `small` (1 CPU, 2GB), `medium` (2 CPU, 4GB), `large` (4 CPU, 8GB) |
| `PG_VM_POOL_USER` / `PG_VM_POOL_PASSWORD` | `postgres` / unset | probe+bootstrap credentials, and (if set) the required client password |
| `PG_VM_POOL_IDLE_TIMEOUT_SECS` | `900` | stop a VM after this long with no connections; `0` disables |
| `PG_VM_POOL_KEEPALIVE_SCHEMAS` | none | comma-separated schemas exempt from idle reaping |
| `PG_VM_POOL_DATA_DISK_GB` | `4` | persistent per-schema disk size — a *cap*, not an upfront allocation: the guest formats a small (4GB) filesystem inside it and grows it online as the database grows (see "Reclaiming disk slack") |
| `PG_VM_POOL_READY_TIMEOUT_SECS` | `300` | max wait for VM+Postgres readiness |
| `PG_VM_POOL_CONNECT_TIMEOUT_SECS` | `30` | iroh tunnel handshake cap |
| `PG_VM_POOL_DIRECT_CONNECT` | on | dial guest IP directly; `0` forces the tunnel |
| `PG_VM_POOL_STATE_FILE` | `~/.heyo/pg-vm-pool/registry.tsv` | persisted schema→VM map |
| `PG_VM_POOL_TLS_CERT` / `PG_VM_POOL_TLS_KEY` | unset (TLS off) | PEM cert chain + key; see TLS below |
| `PG_VM_POOL_DASHBOARD_LISTEN` | unset (dashboard off) | HTTP listen address for the admin dashboard; setting it enables the dashboard — see Dashboard below |
| `PG_VM_POOL_DASHBOARD_USER` / `PG_VM_POOL_DASHBOARD_PASSWORD` | unset (no auth) | HTTP Basic auth credentials for the dashboard (must be set together) |
| `PG_VM_POOL_POOLER_LOG` | `/var/log/pg-vm-pool/pg-vm-pool.log` | pooler log file the dashboard tails |
| `PG_VM_POOL_HEYVMD_LOG` | `/var/log/heyvmd/heyvmd.log` | heyvmd log file the dashboard tails |
| `PG_VM_POOL_DASHBOARD_LOG_LINES` | `200` | how many trailing lines the dashboard shows per log |
| `PG_VM_POOL_DASHBOARD_ALERTS_FILE` | `~/.heyo/pg-vm-pool/alerts.tsv` | where the monitoring page's webhook alert rules persist |
| `PG_VM_POOL_DASHBOARD_ALERT_INTERVAL_SECS` | `60` | how often the alert evaluator samples host metrics and fires crossed alerts |
| `PG_VM_POOL_ARCHIVE_AFTER_SECS` | `0` (off) | S3 eviction: offload a schema untouched this long to S3 and kill its VM; e.g. `604800` = 1 week — see "S3 eviction tier" |
| `PG_VM_POOL_ARCHIVE_SWEEP_SECS` | `3600` | how often the eviction sweep scans for candidates |
| `PG_VM_POOL_S3_BUCKET` | unset | S3 bucket for dumps (required when eviction is on) |
| `PG_VM_POOL_S3_PREFIX` | `pg-vm-pool/` | key prefix; the object per schema is `{prefix}{schema}.dump` |
| `PG_VM_POOL_S3_REGION` | `us-east-1` | region for SigV4 signing |
| `PG_VM_POOL_S3_ENDPOINT` | unset (AWS) | custom endpoint for an S3-compatible store (MinIO/R2); path-style addressing |
| `PG_VM_POOL_S3_ACCESS_KEY_ID` / `PG_VM_POOL_S3_SECRET_ACCESS_KEY` | unset | S3 credentials (fall back to `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`) |
| `PG_VM_POOL_FREEZE_AFTER_SECS` | `0` (off) | local freeze tier: dump a schema idle this long to a local file and delete its VM — see "Local freeze tier" |
| `PG_VM_POOL_FREEZE_SWEEP_SECS` | `900` | how often the freeze sweep scans for candidates |
| `PG_VM_POOL_DUMP_DIR` | `~/.heyo/pg-vm-pool/dumps` | where local dump files live |
| `PG_VM_POOL_DUMP_LISTEN` | `0.0.0.0:6433` | local dump server bind; guests reach it at their default gateway, access is token-gated |
| `PG_VM_POOL_WARM_SPARES` | `0` (off) | keep N pre-booted, initdb-complete spare VMs (`spare-pg-*`) for cold bring-ups to claim — an S3 restore skips create+boot+initdb and goes straight to download+load; capped at 16, each parked spare holds its size class's RAM |
| `PG_VM_POOL_PRESSURE_PATH` | unset (off) | filesystem to watch (the heyvmd run dir); setting it enables emergency disk-pressure eviction — see "S3 eviction tier" |
| `PG_VM_POOL_PRESSURE_HIGH_PCT` / `PG_VM_POOL_PRESSURE_LOW_PCT` | `85` / `75` | start emergency-archiving oldest-idle schemas at/above high; stop below low |
| `PG_VM_POOL_PRESSURE_CHECK_SECS` | `60` | how often the pressure watchdog reads disk usage |
| `PG_VM_POOL_RECLAIM_CMD` | unset (off) | shell command that offline-trims stopped VMs' disks (normally `sudo -n .../reclaim-disks.sh <run-dir>`); setting it enables automatic disk reclamation — see "Reclaiming disk slack" |
| `PG_VM_POOL_RECLAIM_INTERVAL_SECS` | `3600` | how often the periodic reclaim run fires (extra runs also fire right after idle reaps) |
| `PG_VM_POOL_RUN_DIR` | falls back to `PG_VM_POOL_PRESSURE_PATH` | the heyvmd run dir (holds each VM's `sb-<id>/`). Used to verify a killed VM's disk directory is actually gone after archive/freeze, and to locate orphaned directories for the sweep below. When a kill leaves the directory behind (stranding the disk) the pooler logs it loudly instead of reporting "disk reclaimed". Unset ⇒ that removal is left unverified and the orphan sweep is disabled |
| `PG_VM_POOL_ORPHAN_SWEEP_SECS` | unset (off) | how often to sweep the run dir for **orphaned** disk directories — an `sb-<id>/` heyvmd has forgotten (a kill it acked but didn't act on). Requires `PG_VM_POOL_RUN_DIR`. Deletes only directories the daemon confirms gone (per-id 404) that are also not held open and belong to an offloaded/unreferenced schema; a `live` schema whose VM vanished is logged as a data-loss orphan and never deleted — see "Reclaiming disk slack" |

Postgres inside each VM **tunes itself to the VM's resources at every boot**:
`init.sh` reads live RAM/vCPUs/disk and regenerates
`$PGDATA/heyvm-tuning.conf` (`shared_buffers` = ¼ RAM, `work_mem`,
`maintenance_work_mem`, WAL sizing from the data disk, parallel workers from
vCPUs), so one image serves every size class and a VM that changes size class
picks up correct values on its next start. The profile is single-tenant and
ingest-friendly: `wal_level=minimal` + `wal_compression=lz4` (no per-VM
replicas), `synchronous_commit=off` (commits already ride WAL crash recovery
— the pooler stop-kills VMs), SSD plan costs, JIT off. Manual overrides go in
`postgresql.conf`, which is read after the include and therefore wins.

The image also hardens against ingest memory spikes (the classic "big upload
OOM-kills Postgres inside a live VM" failure): `init.sh` creates a swapfile on
the data disk (sized to RAM, capped at 2GB / an eighth of the disk) as an
emergency spillway, switches to strict overcommit (`vm.overcommit_memory=2`,
per the Postgres docs) so an oversized allocation fails just that one query
with a clean `out of memory` instead of summoning the OOM killer, and shields
the postmaster (`oom_score_adj -900`, with `PG_OOM_ADJUST_FILE` resetting
backends to 0) so if the killer runs anyway it takes a recoverable backend,
not the whole cluster. `temp_file_limit` (¼ disk) keeps a runaway sort's
spill files from filling the disk — disk-full is a cluster-wide PANIC, the
limit is a one-query error.

**Direct connect (default):** when the pooler shares the host with the VMs (the
local-daemon deployment), it dials each VM's Postgres directly at its `guest_ip`
over the host tap and skips the iroh tunnel entirely — no relay dependency,
lower latency, faster bring-up. It falls back to a tunnel automatically if the
daemon reports no `guest_ip`. Set `PG_VM_POOL_DIRECT_CONNECT=0` to force the
tunnel path (e.g. if the pooler ever runs on a different machine than the VMs).

### Local freeze tier

Between "idle-stopped VM" (full filesystem image on disk) and "archived to S3"
(off-host, slow to restore) sits the **frozen** tier: a schema idle for
`PG_VM_POOL_FREEZE_AFTER_SECS` is dumped to a local file
(`PG_VM_POOL_DUMP_DIR/<schema>.dump`) and its **VM is deleted**. A cold schema
then costs dump-file bytes (~1–5MB for a typical workbook) instead of a
filesystem image (~200MB+ floor) — roughly an order of magnitude more cold
schemas per host disk. The next client connect restores it: with
`PG_VM_POOL_WARM_SPARES` set it claims a pre-booted spare and goes straight to
download + parallel `pg_restore` (seconds for small workbooks).

The dump bytes move exactly like the S3 tier's — the guest streams
`pg_dump`/`pg_restore` through `curl` — but against a tiny token-gated HTTP
server the pooler runs on the host (`PG_VM_POOL_DUMP_LISTEN`), reached in-guest
at the VM's default gateway. Every guard from the S3 pipeline applies: the VM
is only killed after the server has fully received, fsync'd, and renamed the
dump (size-checked); the tier flip is durable before the kill; restores are
idempotent (`--clean --if-exists`).

The tiers ladder: after `PG_VM_POOL_ARCHIVE_AFTER_SECS`, a *frozen* schema's
dump is **promoted to S3 by the pooler itself** — a file upload, no VM bring-up
at all — and the local file is deleted. `frozen` schemas appear in the
dashboard with a "frozen (local)" badge and a `frozen` state filter.

### S3 eviction tier

The idle reaper (`PG_VM_POOL_IDLE_TIMEOUT_SECS`) only **stops** an idle VM — its
data disk still occupies host storage forever. On a host accumulating thousands
of rarely-touched workbooks, disk is the binding constraint. The **eviction
tier** is a second, slower reclamation stage: a background sweep (hourly by
default) finds any non-keepalive schema untouched for a long window (e.g. a
week), dumps its database to S3, and **kills** the VM — freeing the disk. The
next client connection restores the dump into a fresh VM transparently.

Enable it by setting `PG_VM_POOL_ARCHIVE_AFTER_SECS` to a positive number and
providing an S3 bucket + credentials (the pooler fails fast at startup if the
threshold is set but the bucket/credentials are missing):

```
PG_VM_POOL_ARCHIVE_AFTER_SECS=604800        # 1 week
PG_VM_POOL_S3_BUCKET=my-pg-vm-pool-dumps
PG_VM_POOL_S3_REGION=us-west-2
PG_VM_POOL_S3_ACCESS_KEY_ID=...
PG_VM_POOL_S3_SECRET_ACCESS_KEY=...
# optional, for MinIO/R2/other S3-compatible stores:
# PG_VM_POOL_S3_ENDPOINT=https://minio.internal:9000
```

How it moves the data: the pooler never handles dump bytes itself. It generates
a short-lived **SigV4 presigned URL** and the guest VM streams straight to/from
S3 with its own `pg_dump`/`pg_restore` + `curl` (`pg_dump -Fc | curl -T` on the
way out, `curl | pg_restore` on the way back). The dump bytes never transit the
pooler and the S3 secret key never leaves it. This requires the guest VMs to
have outbound network egress to the S3 endpoint. Each schema maps to one object,
`s3://{bucket}/{prefix}{schema}.dump`; a single `PUT` caps at 5 GB, which is
ample for one-workbook databases.

**Disk-pressure eviction (emergency tier):** the TTL-based sweep can't help
when load outruns it — a filesystem that hits `No space left on device` takes
everything down at once (VM creates fail, Postgres PANICs, even the rescue
dumps fail). Set `PG_VM_POOL_PRESSURE_PATH` to the filesystem holding the VM
disks and a watchdog checks usage every `PG_VM_POOL_PRESSURE_CHECK_SECS`: at or
above `PG_VM_POOL_PRESSURE_HIGH_PCT` it archives the **oldest-idle** schemas —
ignoring `archive_after`; under pressure, least-recently-used is the policy —
one at a time, re-reading usage after each, until below
`PG_VM_POOL_PRESSURE_LOW_PCT`. Keepalive schemas and schemas with live sessions
are never touched, it shares the sweep's single-flight lock, and it aborts
after 3 consecutive failures (an unhealthy environment shouldn't be ground
through). If every candidate is exhausted while still above the low-water mark
it says so loudly — at that point the pressure is running VMs or non-VM data.

Archived schemas show up in the dashboard with an **"archived (S3)"** status
(filterable via the `archived` state pill) even though no VM backs them, and any
idle running schema VM has a **reap → S3** button on its detail page to offload
it on demand. `PG_VM_POOL_KEEPALIVE_SCHEMAS` are exempt from eviction, same as
from idle reaping.

### Reclaiming disk slack (`reclaim-disks.sh`)

A VM's data disk (`data.ext4`) is a **sparse** file provisioned at
`PG_VM_POOL_DATA_DISK_GB`. When Postgres frees blocks inside the guest — recycled
WAL, vacuumed heap, dropped temp/tables, a reinitialised cluster — ext4 marks
them free, but with no TRIM/discard reaching the host those blocks are never
punched out of the backing file. A disk therefore ratchets toward its full
provisioned size and never shrinks, even when the live database is tiny (a 1 GB
database routinely pins tens of GB on disk after a transient bulk load).

**Thin provisioning (first line of defense):** the image formats only a small
(4GB, `pgdata_init_mb` cmdline override) filesystem inside the provisioned
device on first boot, and a watcher in the guest grows it online with
`resize2fs` — doubling up to the device cap — whenever free space drops below
1GB (or ⅛ of the fs). Since ext4 never touches blocks past its own end, the
host allocation can never ratchet past the *current filesystem* size: the
provisioned max is a cap, not the de-facto footprint. The disk-derived Postgres
knobs (`max_wal_size`, `temp_file_limit`, swap sizing) key off the live
filesystem size and are recomputed + reloaded on each growth step.

Eviction reclaims the whole disk once a schema is *long* idle; `reclaim-disks.sh`
reclaims the **slack** from disks whose VMs are merely stopped, without deleting
anything. For every `data.ext4` whose VM is not currently running it recovers
the journal (`e2fsck -fp`) and then punches all free blocks and unused
inode-table blocks straight out of the backing file (`e2fsck -fp -E discard` —
file-level hole punch, no loop device or mount, which keeps working on hosts
where loop-device discard doesn't). Disks a live Firecracker still has open are
skipped (writing to those would corrupt them), and skips name the holding
process. `PRUNE_SWAP=1` additionally deletes each stopped VM's swapfile (dead
weight — swap never survives a boot, and init.sh recreates it right-sized):

```
sudo DRY_RUN=1 ./reclaim-disks.sh ~/.heyo/run   # list candidates, change nothing
sudo ./reclaim-disks.sh ~/.heyo/run             # actually reclaim
sudo SHRINK=1 PRUNE_SWAP=1 ./reclaim-disks.sh ~/.heyo/run   # maximum reclaim (below)
```

**`SHRINK=1` — retro-fit thin provisioning onto legacy disks.** Disks formatted
before thin provisioning have a full-device filesystem, so even after a trim
their next growth ratchets straight back toward the provisioned max, and ~1GB
of full-device ext4 metadata stays allocated forever. With `SHRINK=1` the
script also *shrinks* each stopped VM's filesystem to `used × 1.25` (floored at
`MIN_FS_MB`, default 4096 to match the image's initial size) and hole-punches
the backing file past the new end. The guest's grow watcher re-extends the
filesystem online if the database later needs the space. Shrinking relocates
blocks, so it's slower than a plain trim and the script re-fscks each shrunk
filesystem before mounting it — run it once during a quiet window to convert
the existing fleet, then let the pooler's periodic (non-shrink) runs maintain
it.

This only reclaims *stopped* VMs; reclaiming a **live** VM's disk would need the
guest to issue discards (the image already mounts `/workspace` with `-o discard`)
**and** the Firecracker drive to pass them through to the backing file — which
Firecracker's virtio-blk does not (in-guest `fstrim` reports "the discard
operation is not supported"). Until that changes, offline trim is the only
reclaim path, so the pooler automates it.

**Automatic reclamation:** set `PG_VM_POOL_RECLAIM_CMD` and the pooler runs it
itself — every `PG_VM_POOL_RECLAIM_INTERVAL_SECS` (default hourly), **plus a run
~30 s after the idle reaper stops VMs**, so a just-reaped VM's slack returns
within a minute instead of waiting for a human or the next interval. Runs are
single-flighted and time-bounded (30 min), the output summary lands in the
pooler log, and the dashboard's monitoring page gets a **"reclaim disk slack
now"** button. The command needs root for loop-setup/mount, so a non-root pooler
invokes the script through a `NOPASSWD` sudoers entry:

```
# /etc/sudoers.d/pg-vm-pool-reclaim  (chmod 0440; adjust user + paths)
pooler ALL=(root) NOPASSWD: /opt/pg-vm-pool/reclaim-disks.sh /workbooks/heyvm/run --shrink --prune-swap
```

```
PG_VM_POOL_RECLAIM_CMD="sudo -n /opt/pg-vm-pool/reclaim-disks.sh /workbooks/heyvm/run --shrink --prune-swap"
PG_VM_POOL_RECLAIM_INTERVAL_SECS=3600
```

The flags exist as *arguments* (equivalent to the `SHRINK=1`/`PRUNE_SWAP=1` env
vars) because a pinned sudoers entry can match an exact argument list, while
env assignments are silently refused by `sudo -n` without a `SETENV` tag.
Including them in the periodic command is self-limiting: an already-thin
filesystem skips the shrink and a right-sized swapfile costs only its own
recreation on the next boot, so in steady state the pass degenerates to a plain
trim — but every legacy VM gets fully converted at its first idle stop.

Pin the script at a root-owned path (`chown root:root`, `chmod 0755`) so the
sudoers entry can't be repointed by editing a user-writable file, and pass the
run dir in the sudoers line exactly as in the command so `sudo -n` matches.

#### Orphaned disks (`PG_VM_POOL_ORPHAN_SWEEP_SECS`)

Slack reclamation trims a *live* schema's disk; it does nothing for a disk whose
VM is **gone**. When a schema is archived to S3 or frozen, the pooler kills its
VM to reclaim the whole `sb-<id>/` directory — but the kill is a
`DELETE /deployed-sandboxes/:id` the SDK treats as success on a 404, and heyvmd
has been observed to drop the sandbox record while leaving the directory on
disk. The VM count falls, the bytes don't, and nothing above reclaims them
(`reclaim-disks.sh` only trims live schemas' disks). Left alone this strands
hundreds of GB — a fleet can archive most of its schemas and barely move total
storage.

Two mechanisms address it. First, after every archive/freeze the pooler now
**verifies** the directory is actually gone (needs `PG_VM_POOL_RUN_DIR`) and
logs a loud warning with the path and size instead of a false "disk reclaimed"
when it isn't. Second, set `PG_VM_POOL_ORPHAN_SWEEP_SECS` to have the pooler
periodically **sweep and delete** the orphans itself:

```
PG_VM_POOL_RUN_DIR=/workbooks/heyvm/run
PG_VM_POOL_ORPHAN_SWEEP_SECS=3600
```

A directory is deleted only when every one of these holds, checked cheapest-first
so a live disk is ruled out before any daemon call or destructive action:

1. it's older than 30 min (not a VM mid-create, whose fresh dir the daemon may
   not report yet);
2. no process holds a file in it open — the same chroot-proof `device:inode`
   check `reclaim-disks.sh` uses (not a running VM);
3. heyvmd's **per-id** endpoint returns 404 — the daemon truly forgot it. The
   *list* endpoint is unreliable (it can omit stopped VMs and is truncated on a
   large fleet), so classification never uses it;
4. its schema is offloaded (`frozen`/`archived`, data safe elsewhere) or the id
   is unreferenced by any registry entry (dead).

Conditions 2–3 are re-checked against a fresh snapshot immediately before each
delete. A `live`-tier schema whose VM vanished is a **data-loss orphan** — its
disk is the only copy — so it is reported at error level and *never* deleted;
resolve those (restore, or mark archived/frozen) before they serve an empty DB.
Deletions are capped per pass and a flaking daemon aborts the sweep, so a "gone?"
ambiguity can never become a destructive action. Unlike slack reclamation this
needs no root — just delete permission on the run dir (the pooler runs as the
same user heyvmd creates the directories as); if jailer left root-owned files a
removal fails and is logged rather than silently half-done.

### Managing with supervisord

`deploy/supervisor/pg-vm-pool.conf` runs the release binary under supervisord
and is the single place to manage the pooler's environment:

```sh
cargo build --release
sudo ln -s /home/sam/Projects/pg-fc/deploy/supervisor/pg-vm-pool.conf \
           /etc/supervisor/conf.d/pg-vm-pool.conf
sudo mkdir -p /var/log/pg-vm-pool
sudo supervisorctl reread && sudo supervisorctl update
sudo supervisorctl status pg-vm-pool          # start/stop/restart/tail work too
```

Edit the `environment=` block in the conf to change any `PG_VM_POOL_*` var,
then `supervisorctl reread && supervisorctl update pg-vm-pool` — note a plain
`restart` does **not** reload `environment=`; `update` does. Comma-containing
values (like `KEEPALIVE_SCHEMAS`) must be double-quoted. Logs land in
`/var/log/pg-vm-pool/pg-vm-pool.log`.

### Client auth

The pooler has no client auth gate by default — any client that can reach
`PG_VM_POOL_LISTEN` is proxied straight through to a VM, whatever the VM's
Postgres itself would accept. Set `PG_VM_POOL_PASSWORD` to close that: the
pooler then answers each client's `StartupMessage` with an
`AuthenticationCleartextPassword` challenge and rejects (`28P01`, "password
authentication failed") anyone who doesn't send it back before ever dialing
the backend VM. This is deliberately a separate layer from backend auth — the
VM's own Postgres can (and by default does) stay on `trust`, since gating
access is now the pooler's job.

Because it's cleartext, the password crosses the network unencrypted unless
the connection is also TLS — required reading if `PG_VM_POOL_LISTEN` binds to
anything other than `127.0.0.1` (the pooler logs a startup warning in that
case). Set `PG_VM_POOL_TLS_CERT`/`KEY` alongside it; see TLS below.

### TLS

TLS is **off by default** and fully optional: without it the pooler answers the
Postgres `SSLRequest` with `N` and clients proceed in plaintext exactly as
before (`sslmode=prefer` falls back silently; `sslmode=disable` is unaffected).

To enable, point the pooler at a PEM cert chain + private key:

```sh
PG_VM_POOL_TLS_CERT=/path/fullchain.pem \
PG_VM_POOL_TLS_KEY=/path/privkey.pem \
target/release/pg-vm-pool
```

Both must be set together (setting only one is a startup error). With TLS on,
clients that ask get an encrypted session (`sslmode=require` works) and
plaintext clients are **still accepted** — nothing breaks for existing local
consumers. TLS terminates at the pooler; the pooler→VM hop stays plaintext over
the host-local tap.

The cert files are **hot-reloaded**: the pooler stats them before each
handshake and rebuilds its acceptor when they change, so an external renewer
can rotate certs with no pooler restart. With Let's Encrypt/certbot:

```sh
# one-time issuance (needs public DNS -> this host, port 80 free for the challenge)
sudo certbot certonly --standalone -d pg.example.com

# deploy hook: copy renewed certs somewhere the pooler user can read
sudo tee /etc/letsencrypt/renewal-hooks/deploy/pg-vm-pool.sh >/dev/null <<'EOF'
#!/bin/sh
d=/home/sam/.heyo/pg-vm-pool/tls
mkdir -p "$d"
install -o sam -g sam -m 600 "$RENEWED_LINEAGE/fullchain.pem" "$d/fullchain.pem"
install -o sam -g sam -m 600 "$RENEWED_LINEAGE/privkey.pem"  "$d/privkey.pem"
EOF
sudo chmod +x /etc/letsencrypt/renewal-hooks/deploy/pg-vm-pool.sh
# run the copy once by hand after the first issuance, then renewals are automatic
```

Then set `PG_VM_POOL_TLS_CERT`/`KEY` to those copies (see the commented lines
in the supervisor conf). For clients beyond localhost also set
`PG_VM_POOL_LISTEN=0.0.0.0:6432`, open the firewall, and have clients dial the
certificate's hostname (`sslmode=verify-full host=pg.example.com`).

### Dashboard

An optional server-side-rendered admin dashboard runs **inside the pooler
process** (a background task sharing the live registry), so it can show the
pooler's in-memory session counts alongside the daemon's VM inventory. It's
**off by default** and enabled purely by setting a listen address:

```sh
PG_VM_POOL_DASHBOARD_LISTEN=127.0.0.1:8080 \
PG_VM_POOL_DASHBOARD_USER=admin \
PG_VM_POOL_DASHBOARD_PASSWORD=secret \
target/release/pg-vm-pool
```

What it gives you (browse to the listen address):

- **VM/session overview** (`/`) — every heyvmd sandbox, with power state,
  allocated size (vCPU/RAM), uptime, and live pooler sessions. Pooler-managed
  `pg-<schema>` VMs are grouped first and link to a detail page.
- **Monitoring** (`/monitoring`) — whole-**host** health: total CPU % and
  memory % (from heyvmd's own `/system/usage` sampler) and **disk saturation**
  per host filesystem (read directly on the host with `df`, since the pooler
  runs alongside heyvmd), each shown as a color-banded meter. Below that,
  pooler-fleet aggregates (running VMs, warm/queueing, live sessions, allocated
  vCPU/RAM, guest CPU) rolled up from the same inventory the overview uses —
  still no guest access. This page also configures **webhook alerts** (below).
- **Detail page** — full daemon config (size class + resources, image, region,
  guest IP, TTL, status) plus live **database size and backend count**, read
  over the pooler's own warm Postgres connection (a normal query, not a guest
  command).
- **Logs** — tail the pooler log (`/logs/pooler`), the heyvmd log
  (`/logs/heyvmd`), and any VM's in-guest Postgres log (`/logs/vm/<id>`).
- **Controls** — stop / start / reboot / resize any VM from its detail page.
  Note that a pooler-managed VM stopped here auto-restarts on the next client
  connection, and a resize takes effect on the VM's next boot.

The browsable pages (index + detail) perform **no in-guest command execution** —
they read only the daemon inventory and the pooler's own PG pool, so viewing or
refreshing a VM never disturbs it. The one exception is the per-VM Postgres log
page (`/logs/vm/<id>`), which runs `tail` inside the guest and is therefore a
deliberate, explicitly-navigated action rather than part of the detail view.
Every daemon and guest call is timeout-bounded, so one wedged VM can't hang a
page. Access is gated by HTTP **Basic auth** when
`PG_VM_POOL_DASHBOARD_USER`/`PASSWORD` are set (they must be set together, or
startup fails). The dashboard can stop and resize **every** VM on the host, so
prefer a loopback/private `PG_VM_POOL_DASHBOARD_LISTEN`; binding it to a
non-loopback address without Basic auth logs a startup warning. The two log
paths default to the supervisord locations above and are overridable with
`PG_VM_POOL_POOLER_LOG` / `PG_VM_POOL_HEYVMD_LOG`.

#### Webhook alerts

The monitoring page can watch the basic host metrics and POST a webhook when one
crosses a threshold. Add a rule (metric = host CPU %, host memory %, or disk
saturation %; a threshold; and a URL) from the page's **alerts** panel. A
background task samples the same host metrics every
`PG_VM_POOL_DASHBOARD_ALERT_INTERVAL_SECS` (default 60) and, on a crossing,
`POST`s a small JSON body to the URL — **once** on the rising edge
(`"state":"triggered"`) and once when it falls back (`"state":"resolved"`), not
every interval while it stays over. The disk rule watches the fullest host
filesystem. Example body:

```json
{"source":"pg-vm-pool","host":"pool-1","rule_id":"q7m2…","metric":"disk",
 "state":"triggered","threshold_pct":90.0,"value_pct":93.4,"detail":"/"}
```

Delivery shells out to `curl` (no extra HTTP dependency); a failed or slow
endpoint is logged and never blocks the pooler. Rules persist to
`PG_VM_POOL_DASHBOARD_ALERTS_FILE` (default `~/.heyo/pg-vm-pool/alerts.tsv`, a
sibling of the schema registry) and survive restarts; the firing state is
in-memory, so a restart re-evaluates cleanly rather than replaying a stale edge.

### Testing

`examples/e2e.rs` and `examples/e2e_concurrent.rs` are end-to-end tests that
exercise the full stack through a real client connection — not mocks: pooler
routing, daemon VM create/stop/restart, and per-VM persistent disks.

- `e2e.rs` drives one schema through several stop/restart cycles and
  hard-asserts each restart actually comes back healthy (the `/dev/vdb` data
  drive is still attached, and the guest's Postgres port is reachable) before
  checking the rows written earlier survived.
- `e2e_concurrent.rs` runs that same create/write/stop/restart/verify cycle for
  several schemas (default 5) **at the same time**, each with distinct data,
  to prove concurrent VMs don't cross-wire and the pooler restarts them all in
  parallel.

Prereqs: a running pooler (`target/release/pg-vm-pool`, default
`127.0.0.1:6432`) and a running local heyvmd daemon. Then:

```sh
cargo run --release --example e2e
cargo run --release --example e2e_concurrent
```

Useful env vars: `E2E_ROWS`, `E2E_CYCLES` (e2e.rs), `E2E_VMS` (e2e_concurrent.rs),
`E2E_STOP_MODE=cli|sdk` (`cli` reproduces a manual/out-of-band stop via
`heyvm stop`, the default and the path that catches the restart-silently-no-ops
bug; `sdk` is the cooperative stop path), and `E2E_KEEP=1` to keep the test
VM(s) around instead of deleting them at the end. See the doc comments at the
top of each file for the full list.
