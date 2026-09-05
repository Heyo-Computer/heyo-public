#!/usr/bin/env bash
#
# cleanup-never-booted.sh — delete sandboxes whose data disk was never
# formatted: the debris of creates that never reached first boot.
#
# Why these exist: the daemon allocates a VM's data.ext4 but it's the GUEST's
# init.sh that formats it on first boot. A create that was accepted and then
# died before its VM ever booted (the 2026-08-18 deploy-burst failure mode)
# leaves a fully-allocated, all-zeros data disk — e2fsck says "Bad magic
# number in super-block" — plus a full rootfs copy, in a sandbox dir no
# schema is bound to. reclaim-disks.sh can't trim them (no filesystem to
# read) and can never touch the rootfs; only deleting the sandbox returns
# the bytes.
#
# A sandbox dir is deleted only when ALL of these hold:
#   - its data.ext4 exists, has no readable superblock (dumpe2fs fails), AND
#     its first 1MB is all zeros — i.e. never formatted, provably empty. A
#     bad-magic disk with nonzero content is reported as SUSPECT and never
#     touched (that could be real corruption of real data).
#   - its dir name (the sandbox id) appears in NO registry.tsv row. A BOUND
#     sandbox with an unformatted disk shouldn't be possible; it's reported
#     loudly and never touched.
#   - no process holds any file in the dir open (device:inode match against
#     every open fd, same chroot-proof check as reclaim-disks.sh).
# All three are re-checked immediately before each deletion.
#
# Deletion goes through the daemon when it still knows the id
# (DELETE /deployed-sandboxes/<id> — removes the record and the dir); a 404
# means the daemon already forgot it and the dir is removed directly.
#
# Usage:
#   sudo ./cleanup-never-booted.sh [RUN_DIR]              # DRY RUN: report only
#   sudo DELETE=1 ./cleanup-never-booted.sh [RUN_DIR]     # actually delete
#
#   RUN_DIR       default ~/.heyo/run (pass explicitly on prod, e.g.
#                 /mnt/md0/heyvm/run)
#   REGISTRY_TSV  default ~/.heyo/pg-vm-pool/registry.tsv — under sudo, HOME
#                 is root's: pass the real path explicitly.
#   HEYVMD        default http://127.0.0.1:34099
#
# Requires: curl, jq, dumpe2fs, stat, find, du, numfmt.
set -uo pipefail

RUN_DIR="${1:-${HOME}/.heyo/run}"
REGISTRY_TSV="${REGISTRY_TSV:-${HOME}/.heyo/pg-vm-pool/registry.tsv}"
HEYVMD="${HEYVMD:-http://127.0.0.1:34099}"
DELETE="${DELETE:-0}"

die() { echo "error: $*" >&2; exit 1; }
human() { numfmt --to=iec --suffix=B "${1:-0}" 2>/dev/null || echo "${1:-0}B"; }

for tool in curl jq dumpe2fs stat find du numfmt; do
    command -v "$tool" >/dev/null || die "missing required tool: $tool"
done
[ -d "$RUN_DIR" ] || die "run dir not found: $RUN_DIR"
[ -r "$REGISTRY_TSV" ] || die "cannot read registry at $REGISTRY_TSV (set REGISTRY_TSV) — \
without it nothing is provably unbound; refusing to guess"
if [ "$(id -u)" != 0 ]; then
    [ "$DELETE" = 1 ] && die "DELETE=1 must run as root (full /proc scan + jailer-owned files)"
    echo "warning: not root — dry-run in-use detection may be incomplete" >&2
fi

# Bound sandbox ids (registry column 2) — the ownership whitelist.
declare -A BOUND=()
while IFS=$'\t' read -r _schema id _rest; do
    [ -n "$id" ] && BOUND["$id"]=1
done < "$REGISTRY_TSV"

# Ids the daemon still tracks (for choosing DELETE-via-API vs plain rm).
# A down daemon degrades to "unknown": we then refuse to delete anything,
# since we can't rule out a record (and a VM about to boot) behind an id.
daemon_ids_file=$(mktemp)
trap 'rm -f "$daemon_ids_file"' EXIT
daemon_up=1
if ! curl -fsS --max-time 30 "$HEYVMD/deployed-sandboxes" 2>/dev/null \
        | jq -r '.[].id' > "$daemon_ids_file"; then
    daemon_up=0
    echo "warning: cannot list $HEYVMD/deployed-sandboxes — daemon down?" >&2
    [ "$DELETE" = 1 ] && die "refusing to delete while the daemon is unreachable"
fi
declare -A DAEMON=()
while read -r id; do
    [ -n "$id" ] && DAEMON["$id"]=1
done < "$daemon_ids_file"

# Point-in-time open-file snapshot, keyed device:inode (chroot-proof; see
# reclaim-disks.sh for the full rationale).
declare -A OPEN_INODES=()
snapshot_open_files() {
    OPEN_INODES=()
    local key path pid
    while read -r key path; do
        [ -n "$key" ] || continue
        pid="${path#/proc/}"
        pid="${pid%%/*}"
        OPEN_INODES["$key"]="${OPEN_INODES[$key]:-$pid}"
    done < <(find /proc/[0-9]*/fd -maxdepth 1 -type l -exec stat -L -c '%d:%i %n' {} + 2>/dev/null)
}

dir_held_open() {
    local f key
    while IFS= read -r -d '' f; do
        key=$(stat -c '%d:%i' "$f" 2>/dev/null) || return 0  # unstat-able: fail closed
        [ -n "${OPEN_INODES[$key]:-}" ] && return 0
    done < <(find "$1" -maxdepth 1 -type f -print0 2>/dev/null)
    return 1
}

# Never formatted: no superblock AND first 1MB all zeros.
unformatted() {
    dumpe2fs -h "$1" >/dev/null 2>&1 && return 1
    [ -z "$(head -c 1048576 "$1" 2>/dev/null | tr -d '\0')" ]
}

snapshot_open_files

candidates=()     # dirs safe to delete
cand_bytes=0
suspects=0 bound_bad=0 held=0
shopt -s nullglob
for disk in "$RUN_DIR"/sb-*/data.ext4; do
    dir=$(dirname "$disk")
    id=$(basename "$dir")
    dumpe2fs -h "$disk" >/dev/null 2>&1 && continue   # formatted: not ours
    if ! unformatted "$disk"; then
        echo "SUSPECT   $id — bad superblock but nonzero content; NOT touching" \
             "(if it matters, try: e2fsck -b 32768 $disk)"
        suspects=$((suspects + 1))
        continue
    fi
    if [ -n "${BOUND[$id]:-}" ]; then
        echo "ANOMALY   $id — bound to a schema in $REGISTRY_TSV yet its data disk was never" \
             "formatted; NOT touching. This shouldn't be possible — investigate before deleting."
        bound_bad=$((bound_bad + 1))
        continue
    fi
    if dir_held_open "$dir"; then
        echo "skip      $id — a process holds its files open"
        held=$((held + 1))
        continue
    fi
    bytes=$(du -sB1 "$dir" 2>/dev/null | cut -f1)
    where=$([ -n "${DAEMON[$id]:-}" ] && echo daemon || echo dir-only)
    printf 'candidate %s  %-10s (%s)\n' "$id" "$(human "${bytes:-0}")" "$where"
    candidates+=("$dir")
    cand_bytes=$((cand_bytes + ${bytes:-0}))
done

echo "----"
echo "${#candidates[@]} never-booted sandbox(es), $(human "$cand_bytes") reclaimable;" \
     "$suspects suspect, $bound_bad bound-anomaly, $held in use (all untouched)"
[ "$daemon_up" = 1 ] || echo "(daemon unreachable: candidacy shown, deletion refused)"

if [ "$DELETE" != 1 ]; then
    [ "${#candidates[@]}" -gt 0 ] && echo "DRY RUN — re-run with DELETE=1 to delete."
    exit 0
fi
[ "${#candidates[@]}" -gt 0 ] || exit 0

# Fresh open-file snapshot for the destructive phase; candidacy did many
# reads, during which a VM could have started.
snapshot_open_files

deleted=0 freed=0 failed=0
for dir in "${candidates[@]}"; do
    id=$(basename "$dir")
    # Re-verify everything immediately before acting.
    if [ ! -e "$dir/data.ext4" ] || ! unformatted "$dir/data.ext4" \
        || [ -n "${BOUND[$id]:-}" ] || dir_held_open "$dir"; then
        echo "skip      $id — state changed since candidacy; leaving alone"
        continue
    fi
    bytes=$(du -sB1 "$dir" 2>/dev/null | cut -f1)
    if [ -n "${DAEMON[$id]:-}" ]; then
        code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 60 -X DELETE \
            "$HEYVMD/deployed-sandboxes/$id")
        case "$code" in
            2*|404) ;;  # deleted, or already gone — either way ours to finish
            *)
                echo "FAILED    $id — daemon DELETE returned HTTP $code; dir kept"
                failed=$((failed + 1))
                continue
                ;;
        esac
    fi
    # The daemon usually removes the dir with the record; clean up whatever
    # is left (jailer can leave root-owned files, hence root).
    if [ -d "$dir" ] && ! rm -rf "$dir" 2>/dev/null; then
        echo "FAILED    $id — daemon record cleared but rm -rf $dir failed"
        failed=$((failed + 1))
        continue
    fi
    echo "deleted   $id  ($(human "${bytes:-0}"))"
    deleted=$((deleted + 1))
    freed=$((freed + ${bytes:-0}))
done

echo "----"
echo "deleted $deleted sandbox(es), freed $(human "$freed"); $failed failed"
[ "$failed" -eq 0 ]
