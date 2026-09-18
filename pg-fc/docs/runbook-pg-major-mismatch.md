# Runbook: an archive and the image disagree on the Postgres major

A schema's archived cluster was initialized by one Postgres major and the host
restoring it runs another. Postgres majors are never on-disk compatible, in
either direction, so the restore can't work however many times it's retried.
This runbook covers how to recognize it, how it happens, how to recover one
schema, and how to move a host from one major to another without leaving any
archive behind.

## Recognize it

Before the restore-time gate (`swap_and_boot` in `imgarchive.rs`), every
attempt burned about 5.5 minutes and ended with:

```
pg-<schema> failed a fresh boot (pgdata=v16 server=v18 pg-procs=0 | ... FATAL:
database files are incompatible with server | DETAIL: The data directory was
initialized by PostgreSQL version 16, which is not compatible with this version 18.x
```

(Builds before that fix print `server=v` with no number. The guest has no
`postgres` on its PATH.) With the gate, the attempt fails in seconds, before the
disk swap:

```
schema <schema>: the archived cluster was initialized by PostgreSQL 16, but this
host's image serves PostgreSQL 18 — refusing to adopt it
```

In both cases the archive itself is untouched.

To count it on a host:

```sh
cat /var/log/pg-vm-pool/pg-vm-pool.log* | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E 'are incompatible with server|refusing to adopt it' \
  | grep -oE 'schema [A-Za-z0-9_]+' | sort -u
```

To see which major each host's image serves:

```sh
debugfs -c -R "ls /usr/lib/postgresql" <heyvm-data>/images/firecracker/pg.ext4
```

## How it happens

Two things have to line up.

**1. Hosts built on different majors.** `./Dockerfile` defaults to
`PG_MAJOR=16`. `build-rootfs.sh` passes `PG_MAJOR=18` explicitly, but
`deploy/provision-pooler-host.sh` used to build the bare `Dockerfile`. So a
freshly provisioned host served 16 while older hosts served 18. Net-new schemas
work fine on either host, because a fresh `initdb` always matches its own
server. That's what hides the problem until an archive crosses hosts.

**2. One S3 key per schema, shared by every host.** Archive keys are
`{PG_VM_POOL_S3_PREFIX}{schema}.img.zst` and `{prefix}{schema}.dump`, with
nothing host-specific in them. When two hosts with the same bucket and prefix
both have a registry row for a schema, whichever archives last overwrites the
other's object. Nothing checks who wrote the existing object. An image upload
also deletes the schema's `.dump` key, which is meant to stop an older dump from
shadowing the newer image.

A schema gets registered on two hosts whenever a client builds it on more than
one. A typical cause is an application that fails over to another pooler
instance after a timeout, while the first build keeps running. After that:

- **If the two hosts run different majors,** the loser fails loudly (this
  runbook).
- **If they run the same major,** the loser *silently serves the other host's
  copy*. Nothing errors, and the stale data looks like the real data. Treat
  every multi-host schema as suspect, not just the ones that fail.

To find multi-host schemas, collect `cut -f1,4 registry.tsv` from every host
that shares the bucket and prefix, then look for schemas that appear more than
once.

## Before recovering anything: find the authoritative copy

A schema that failed this way almost always had **two** copies, and the object
now in S3 is only the most recent upload. It isn't necessarily the copy the
application was writing to. Before restoring anything:

1. **Ask the application which host serves the schema.** Whatever it connects to
   (for example its per-database connection URL) is the authoritative host. The
   copy the application wrote to is the real one, and a copy on another host is a
   leftover of a build it abandoned.
2. **Check the pooler logs on the authoritative host** for its own upload:
   `schema <s>: compacted image promoted to s3://… (N bytes)`. If that upload
   comes *before* the first failure, another host overwrote it.
3. **Check whether the bucket keeps object versions.** If it does, the
   authoritative host's upload is still there as a noncurrent version (match it
   by size and time). Recovery A below uses it. If it doesn't, the only surviving
   data is the other host's copy, which can be stale. Decide with the data's
   owner before serving it.

Registry rows for the schema on the non-authoritative hosts should be removed
(with the pooler stopped, and a backup of `registry.tsv` kept). That way those
hosts never upload over the key again.

## Recovery A: a noncurrent version holds the right copy

This needs bucket-admin rights and no host work.

```sh
aws s3api list-object-versions --bucket <bucket> --prefix <prefix><schema>.img.zst
aws s3api copy-object --bucket <bucket> --key <prefix><schema>.img.zst \
  --copy-source '<bucket>/<prefix><schema>.img.zst?versionId=<version>'
```

The copy becomes the current version, and the bad object stays behind as a
noncurrent one. The pooler's per-schema breaker may still be holding the schema
off for up to 15 minutes after its last failure. Wait it out, or restart the
pooler, which clears the in-memory breaker. Then the next connect restores
normally.

## Recovery B: convert the cluster with an old-major server

Use this when the only usable copy is on the other major. The cluster needs
binaries of *its* major exactly once, to `pg_dump` it. A custom-format dump
restores fine onto a newer server, but **not** onto an older one.

It runs on any host that has the old major's server binaries
(`/usr/lib/postgresql/<major>/bin`, from `apt install postgresql-<major>`) plus
`zstd`, `debugfs` and `e2fsck`. Nothing is booted.

`dump-oldpg.sh` exhumes a stopped VM's `sb-<id>/data.ext4` for any registry row
whose tier is `live`. An archived schema has no disk and the wrong tier, so give
the script a scratch run dir and a one-row scratch registry, and use
`--no-registry` so it never touches the real one:

```sh
S=<schema>; W=/var/tmp/exhume-$S; mkdir -p $W/run/sb-exhume $W/dumps
# Fetch the archive (by whatever means holds bucket credentials) to $W/$S.img.zst
zstd -d --sparse $W/$S.img.zst -o $W/run/sb-exhume/data.ext4
debugfs -c -R "cat /pgdata/PG_VERSION" $W/run/sb-exhume/data.ext4   # expect the OLD major
printf '%s\tsb-exhume\t0\tlive\t2\n' "$S" > $W/registry.tsv
sudo ./dump-oldpg.sh --major 16 --schema "$S" --run-dir $W/run \
  --state $W/registry.tsv --dump-dir $W/dumps --work-dir $W --in-place --no-registry --yes
pg_restore --list $W/dumps/$S.dump >/dev/null && ls -l $W/dumps/$S.dump
```

`--in-place` is safe here because the disk is already a scratch copy, and it
avoids copying it again. Then, on the host that should serve the schema:

1. Copy the dump to that host's `PG_VM_POOL_DUMP_DIR` as `<schema>.dump`. That
   host needs the frozen tier configured (`PG_VM_POOL_FREEZE_AFTER_SECS`).
2. Stop the pooler, back up `registry.tsv`, and change the schema's tier column
   (field 4) to `frozen`. Start the pooler again.
3. Connect once. The pooler restores the dump onto its current image through the
   frozen tier, and its offload ladder archives it again later as usual.

## Moving a host to a different major

Every archive the host has written is in the old major. A host with thousands of
archived schemas can't convert them all inside one maintenance window, so the
move comes in three stages.

**Before the window: leave the offload ladder alone.** It's tempting to stop
the host minting old-major images first, but neither lever is safe on a busy
host:

- **Unsetting `PG_VM_POOL_COMPACT_AFTER_SECS` strands every compacted schema.**
  With the compacted tier unconfigured, a thaw fails with "compacted tier is not
  configured … cannot thaw it".
- **Raising it past any realistic idle time turns every offload into a boot
  job.** Idle schemas then go to `Freeze`, and the pacer runs at most one
  VM-booting job at a time. A host that idles out hundreds of schemas an hour
  can't keep up, so stopped disks pile up until disk pressure. Under pressure the
  ladder falls back to *image* archives anyway (`OffloadKind::ImageArchive`), or
  the run dir fills.

Instead, schedule the window soon, since each day on the old major adds to the
archived set, and convert the archives after the switch.

**The window: the hot set.** Take the host out of the application's rotation
first.

1. **Live and stopped VMs:** `dump-oldpg.sh --list`, then `--yes`, with the
   pooler stopped. It flips each one to `frozen`.
2. **Compacted schemas** (`<compact_dir>/<schema>.img.zst`): Recovery B per
   schema, using the local file instead of a download. Flip each to `frozen`.
3. **Warm spares:** delete them. They're `initdb`'d on the old major, and the
   replenisher rebuilds them.
4. **Swap the image:** `PG_MAJOR=<new> ./build-rootfs.sh` (it verifies the
   result), replace the host's `images/firecracker/pg.ext4`, then restart
   heyvmd and the pooler.
5. **Verify** with a net-new schema and with one frozen schema, then put the host
   back into rotation.

**After the window: the archived set.** Each old-major archive the host owns
now fails fast on access (the gate) until it's converted. There are two ways to
handle them:

- **Batch conversion.** Run Recovery B in the background, most recently used
  first (by registry field 3). Skip any object whose `PG_VERSION` already matches
  the new major: another host wrote it, so it isn't this host's to convert. This
  needs bucket credentials on the host and throughput sized by a pilot, and it
  converts many archives nobody will ever open again.
- **Convert on access.** Keep the old major's server binaries on the host and do
  Recovery B at restore time, when the gate finds an older cluster. Only schemas
  that someone actually opens pay the conversion. The pooler doesn't do this
  today.

## Keeping it from coming back

- **One major across every host sharing a bucket and prefix.** The provisioner
  now defaults to `Dockerfile.pg18`, and `build-rootfs.sh` verifies the major it
  built.
- **Empty clusters never reach the bucket.** Every S3 write path refuses a
  database with no user relations, so a copy that was never filled — a
  failed-over create, a bring-up that fell through to a fresh VM — can no longer
  replace the copy that holds the data. It stays on its host as a compacted
  image or local dump instead, and the events journal says so.
- **One registry row per schema across the fleet.** The application should never
  build a schema on a second pooler instance while the first build can still
  finish.
- **A prefix per host.** A distinct `PG_VM_POOL_S3_PREFIX` per host (for
  example `pg-vm-pool/<host>/`, with the trailing slash — the prefix is joined to
  the schema as plain text) makes cross-host overwrites impossible from then on.
  It is safe to set on a host that already has archives: every upload and delete
  goes under the new prefix, and a restore reads the old layout
  (`PG_VM_POOL_S3_LEGACY_PREFIX`, default `pg-vm-pool/`) only when the new prefix
  is known to hold nothing for the schema — both keys answered "not found". A
  HEAD that fails in transit or a torn object under the new prefix keeps the
  restore there, so a schema this host has re-archived never comes back as the
  older shared copy. Nothing is copied or deleted in the old layout; each schema
  moves across the next time it is archived. The startup log's offload line
  shows the fallback (`restores fall back to …`), and a restore that used it logs
  `restoring from the legacy prefix`.

  The fallback reads the shared layout, so it inherits whatever another host last
  wrote there: a cross-major copy is refused by the restore gate, but a
  same-major one restores as it is. Change the prefix on every host that shares
  the bucket, not just some, or the hosts left on the shared prefix keep writing
  where the others read.
