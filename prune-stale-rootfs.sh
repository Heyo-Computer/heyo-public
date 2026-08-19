#!/usr/bin/env bash
#
# prune-stale-rootfs.sh — delete per-VM rootfs copies left behind by unclean
# VM stops. Usually the single largest reclaimable item on a pooler host.
#
# WHAT THESE FILES ARE. heyvmd clones the base image into
# `<run-dir>/sb-<id>/rootfs.ext4` on every *boot* (Firecracker opens the rootfs
# read-write, so the shared base image can't be used directly) and deletes that
# copy again on a clean stop — see `stop_vm` in mvm-ctrl's firecracker driver,
# which removes it unconditionally, and `start_vm`, which re-clones it from the
# base image every time. A copy sitting next to a *stopped* VM's data disk is
# therefore residue from a stop that never ran its cleanup: a daemon restart
# (the healthcheck watchdog does this), a SIGKILL, a host reboot.
#
# WHY IT ADDS UP. ~190MB each on the pg image. A host with a few thousand
# sandboxes that has been watchdog-restarted a few times carries most of a
# terabyte of them (measured: 1.05TB of 3.2TB used on a 5600-sandbox host).
# They are invisible to every other tool here: reclaim-disks.sh only touches
# `data.ext4`, and the pooler's orphan sweep only deletes directories heyvmd
# has *forgotten*, which these are not.
#
# WHY IT IS SAFE. A rootfs copy holds no sandbox state — the schema's data
# lives in `data.ext4`, and the registry binds schema→sandbox-id, neither of
# which this touches. Deleting one is exactly what heyvmd's own `stop` does.
# The cost of being wrong is one image clone (~a second) on that VM's next
# boot. Nothing is orphaned: no VM record, no disk, no registry row.
#
# Note that on a filesystem without reflink support the clone is a full byte
# copy on EVERY boot anyway, so these files are re-created and re-deleted
# constantly in normal operation — keeping them costs storage and saves
# nothing.
#
# GUARDS (all must hold):
#   - the file is `<RUN_DIR>/sb-*/rootfs.ext4` — never a base image, never a
#     data disk, never a snapshot;
#   - no process holds it open (device:inode match against every /proc fd, the
#     same chroot-proof check reclaim-disks.sh uses) — a *running* VM holds its
#     rootfs open, so running VMs are skipped and named;
#   - it is older than MIN_AGE_MINS (default 30), so a VM mid-boot — cloned but
#     not yet opened by Firecracker — can't be caught in the window between the
#     two.
#
# Usage:
#   sudo ./prune-stale-rootfs.sh [RUN_DIR]            # DRY RUN: report only
#   sudo DELETE=1 ./prune-stale-rootfs.sh [RUN_DIR]   # actually delete
#
#   MIN_AGE_MINS=30   age floor, minutes
#   QUIET=1           suppress the per-file lines (summary only)
#
# Run as root: the /proc scan needs it to see every process's fds, and an
# incomplete scan is what would let a running VM's rootfs through.
#
set -uo pipefail

RUN_DIR="${1:-${HOME}/.heyo/run}"
DELETE="${DELETE:-0}"
MIN_AGE_MINS="${MIN_AGE_MINS:-30}"
QUIET="${QUIET:-0}"

die() { echo "error: $*" >&2; exit 1; }
human() { numfmt --to=iec --suffix=B "${1:-0}" 2>/dev/null || echo "${1:-0}B"; }
# Actual on-disk bytes (allocated blocks), not the sparse apparent size.
allocated() { du -B1 "$1" 2>/dev/null | cut -f1; }

[ -d "$RUN_DIR" ] || die "run dir not found: $RUN_DIR"
for tool in find stat du numfmt; do
    command -v "$tool" >/dev/null || die "missing required tool: $tool"
done
if [ "$(id -u)" != 0 ]; then
    [ "$DELETE" = 1 ] && die "must run as root to delete (the /proc scan needs it \
to see every VM's open files; an incomplete scan could delete a running VM's rootfs)"
    echo "warning: not root — the in-use scan is incomplete, so this report may \
overstate what is safe to delete" >&2
fi

# Point-in-time set of every file held open by any process, keyed by
# device:inode. Firecracker usually runs under jailer, which chroots the VM, so
# an fd's *path* is relative to that chroot and never equals the host path —
# matching by path would silently miss running VMs. device:inode is the same
# file object regardless of chroot, bind mount or namespace. Built once (a
# single find|stat over all fds) rather than per file, which on a host with
# thousands of VMs does not finish.
declare -A OPEN_INODES=()
while read -r key path; do
    [ -n "$key" ] || continue
    pid="${path#/proc/}"
    pid="${pid%%/*}"
    OPEN_INODES["$key"]="${OPEN_INODES[$key]:-$pid}"
done < <(find /proc/[0-9]*/fd -maxdepth 1 -type l -exec stat -L -c '%d:%i %n' {} + 2>/dev/null)

echo "prune-stale-rootfs: scanning $RUN_DIR (delete=$DELETE, min-age=${MIN_AGE_MINS}m, \
${#OPEN_INODES[@]} open files seen)"

total=0 candidates=0 in_use=0 too_new=0 freed=0 removed=0 failed=0

# -mmin +N is "modified more than N minutes ago"; a booting VM's fresh clone
# fails it. maxdepth/mindepth pin the shape: only sb-*/rootfs.ext4, never
# sb-*/snapshot/rootfs.ext4 (a checkpoint image, which IS state).
while IFS= read -r -d '' rootfs; do
    total=$((total + 1))
    key=$(stat -c '%d:%i' "$rootfs" 2>/dev/null) || continue
    if [ -n "${OPEN_INODES[$key]:-}" ]; then
        pid="${OPEN_INODES[$key]}"
        comm="?"
        [ -r "/proc/$pid/comm" ] && comm=$(cat "/proc/$pid/comm" 2>/dev/null)
        [ "$QUIET" = 1 ] || echo "skip  (in use by pid $pid/$comm)  $rootfs"
        in_use=$((in_use + 1))
        continue
    fi
    bytes=$(allocated "$rootfs")
    candidates=$((candidates + 1))
    if [ "$DELETE" = 1 ]; then
        if rm -f "$rootfs"; then
            removed=$((removed + 1))
            freed=$((freed + bytes))
            [ "$QUIET" = 1 ] || printf 'freed %-12s %s\n' "-$(human "$bytes")" "$rootfs"
        else
            failed=$((failed + 1))
            echo "FAIL  (rm)  $rootfs" >&2
        fi
    else
        freed=$((freed + bytes))
        [ "$QUIET" = 1 ] || printf 'would-free %-12s %s\n' "$(human "$bytes")" "$rootfs"
    fi
done < <(find "$RUN_DIR" -mindepth 2 -maxdepth 2 -type f -name 'rootfs.ext4' \
             -mmin "+$MIN_AGE_MINS" -print0 2>/dev/null)

# Count the ones the age floor excluded, for an honest denominator.
too_new=$(find "$RUN_DIR" -mindepth 2 -maxdepth 2 -type f -name 'rootfs.ext4' \
              -not -mmin "+$MIN_AGE_MINS" 2>/dev/null | wc -l)

echo "----"
if [ "$DELETE" = 1 ]; then
    echo "deleted $removed/$((total)) stale rootfs cop(ies), freed $(human "$freed"); \
$in_use in use (running VMs), $too_new too new, $failed failed"
    echo "each deleted copy is re-cloned from the base image on that VM's next boot"
else
    echo "dry run: $candidates deletable, $(human "$freed") reclaimable; \
$in_use in use (running VMs), $too_new too new"
    echo "re-run with DELETE=1 to reclaim it"
fi
