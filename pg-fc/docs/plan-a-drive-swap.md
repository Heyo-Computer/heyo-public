# Plan A: instant thaw via drive swap under warm spares

Status: **planned, not started** — requires heyvmd + heyo-sdk changes (cross-repo).
Complements the shipped Plan B (local freeze tier): B makes cold schemas dense
(dump files, no VM); A makes *warm-ish* schemas instant (existing `data.ext4`
attached to an already-booted VM, no pg_restore, no index rebuilds). A large
workbook whose logical restore would take minutes thaws in ~2–5s under A.

## Mechanism

Firecracker cannot hot-*add* drives, but it **can repoint an existing drive on a
running VM**: `PATCH /drives/{id}` with a new `path_on_host` triggers a virtio
config-change the guest observes. So a spare boots with a small dummy data disk
already attached at `/dev/vdb`, and "attaching a schema's disk" means swapping
the backing file under that same drive slot:

1. Spare boots from the normal image with a dummy `data.ext4`; init.sh runs as
   usual (Postgres up on the dummy workspace).
2. Claim for schema X (stopped VM, disk on host): guest-side "release" script
   stops Postgres, unmounts `/workspace`.
3. Host: heyvmd PATCHes the spare's data drive `path_on_host` →
   `/…/sb-<X>/data.ext4` (the file heyvmd moves/adopts from the old sandbox).
4. Guest-side "adopt" script: re-read device size (`blockdev --rereadpt` not
   needed for virtio-blk; the config change updates capacity), `mount
   /workspace`, run the existing init.sh data-mount tail (swapfile, tuning
   regen from fs size, tmpfs symlink), start Postgres (WAL crash-recovery on
   the adopted pgdata — same recovery path a normal VM restart takes).
5. Pooler: `store.put(schema, spare_id)` — identical binding flow to B.

## Required heyvmd / SDK work

- **heyvmd**: new endpoint, e.g. `POST /sandbox/{id}/swap-data-disk` with
  `{path_on_host}` → drives the Firecracker `PATCH /drives`, updates its own
  sandbox record (disk path + size), returns the observed device size.
  Also: an "orphan disk adopt/detach" notion — the old sandbox is deleted but
  its `data.ext4` must survive as a first-class object heyvmd tracks (or the
  pooler owns a disk directory heyvmd merely mounts from).
- **heyo-sdk**: `Sandbox::swap_data_disk(path) -> Result<u64>`.
- **image (this repo)**: `release.sh`/`adopt.sh` planted in the rootfs, driven
  over the existing detached-job channel (same sentinel discipline as the
  dump/restore jobs — never trust an unconfirmed adopt; on any failure the
  spare is discarded, never reused, since its state is ambiguous).

## Pooler-side design (this repo)

- Tier stays `Live` — A changes *how* a stopped schema comes back, not where
  its data lives. `resolve_sandbox` grows a step between "reattach by id" and
  "find by name": if the stored VM is stopped and a drive-swap spare is
  available, adopt its disk into the spare and delete the old sandbox shell.
- The reap path can then delete the VM *shell* immediately at idle-stop
  (keeping only `data.ext4`), which removes the per-sandbox overhead and makes
  every stopped schema thaw-by-adoption — stopped VMs stop existing as
  sandboxes at all.
- Safety invariants (same shape as B's):
  - the old sandbox is deleted only after the disk file is safely renamed into
    the pooler-owned disk dir (durable move before delete);
  - adopt is confirmed by the guest sentinel *and* a `SELECT 1` through the
    pooler's pool before the schema is served;
  - a spare that fails release/adopt is killed, never returned to the pool.

## Tiering end-state (A + B together)

| State | Artifact on host | Thaw path | Thaw cost |
|---|---|---|---|
| hot | running VM | — | 0 |
| stopped | `data.ext4` only (no sandbox) | drive-swap into spare (A) | ~2–5s any size |
| frozen | `<schema>.dump` local file | restore into spare (B) | seconds, scales with data |
| archived | S3 object | restore into spare (B) | + download |

Policy sketch: stop→(idle_timeout)→frozen for small DBs / stopped-disk for
large ones→(freeze_after)→frozen→(archive_after)→archived.

## Open questions for the heyvmd side

1. Does the deployed Firecracker version accept post-boot `PATCH /drives`
   `path_on_host` updates? (Verify first — this gates everything.)
2. Jailer chroot: the new backing file must be visible inside the spare's
   chroot (hard-link into the jail dir, as heyvmd presumably already does at
   create).
3. Who owns disk GC once disks outlive sandboxes (orphan-disk reaping moves
   from "delete sandbox" to an explicit disk lifecycle).
