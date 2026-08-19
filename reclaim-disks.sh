#!/usr/bin/env bash
#
# reclaim-disks.sh — return stranded free space from idle VM data disks.
#
# Each per-schema VM has a sparse `data.ext4` (provisioned at
# PG_VM_POOL_DATA_DISK_GB, mostly holes). When Postgres frees blocks inside the
# guest — recycled WAL, vacuumed heap, dropped temp/tables, a reinitialised
# cluster — ext4 marks them free, but with no TRIM/discard reaching the host the
# blocks are never punched out of the backing file. So a disk ratchets toward its
# full provisioned size and never shrinks, even though the live database is tiny
# (we've seen 1.1 GB of data pinning 51 GB on disk).
#
# This offline-reclaims every data disk whose VM is NOT currently running:
# recover the journal left dirty by an unclean VM kill (`e2fsck -fp`), then
# punch every free block and unused inode-table block out of the backing file
# with `e2fsck -fp -E discard`. Both operate on the image file directly — no
# loop device, no mount, no fstrim. (An earlier version went
# loop-mount+fstrim; that path silently stopped punching holes on at least one
# production host — fstrim errors were swallowed and runs "reclaimed" ~0B from
# hundreds of GB of slack — while file-level discard keeps working, since it's
# a plain fallocate(PUNCH_HOLE) on the file.)
#
# SAFETY: a disk a live Firecracker still has open is skipped — writing to a
# filesystem a running guest has mounted would corrupt it. Everything else is
# best-effort per disk: one failure never aborts the run.
#
# SHRINK=1 additionally *shrinks the filesystem* toward its used size before
# trimming. Legacy disks were formatted across the whole device, so even after
# an fstrim their next growth ratchets straight back toward the full
# provisioned size, and the ~1GB of full-device ext4 metadata stays allocated
# forever. Shrinking retro-fits the thin-provisioning cap the image now applies
# at first format (see init.sh): target = used * 1.25, floored at MIN_FS_MB
# (default 2048, matching init.sh's initial size); the guest's grow watcher
# re-extends the filesystem online if the database needs more. Blocks past the
# new filesystem end are hole-punched out of the backing file directly (fstrim
# can't reach past the fs). The shrunk fs is re-fscked before anything mounts
# it; a disk that fails that check is reported and left unmounted.
#
# YIELDING: the caller (normally pg-vm-pool) must keep VM boots and this script
# disjoint in time — the in-use scan below is a snapshot taken at pass start, so
# a VM booted mid-pass would be invisible to it and its filesystem could be
# fscked underneath the running guest. The pooler enforces that by blocking
# boots for the whole run, which on a big fleet is minutes. So when a boot is
# waiting it creates $STOP_FILE ("$RUN_DIR/.reclaim-stop"): this script finishes
# the disk it is on, records where it stopped in $CURSOR_FILE, and exits, and
# the next run resumes after that disk. Stopping between disks (never inside
# one) is the only safe point, and cooperation is the only safe mechanism — the
# script runs under sudo, so a caller that "killed" it would only reap the shell
# while the root e2fsck kept writing.
#
# Usage:
#   sudo ./reclaim-disks.sh [RUN_DIR]        # default RUN_DIR: ~/.heyo/run
#   sudo DRY_RUN=1 ./reclaim-disks.sh [DIR]  # report candidates, change nothing
#   sudo SHRINK=1 ./reclaim-disks.sh [DIR]   # also shrink filesystems (slower)
#   sudo PRUNE_SWAP=1 ./reclaim-disks.sh [DIR]  # also delete guest swapfiles
#
# PRUNE_SWAP=1 deletes each stopped VM's /swapfile (via debugfs, no mount —
# but only AFTER the journal-recovery fsck: debugfs surgery under a dirty
# journal gets overwritten by the replay and strands the swapfile as an
# "Unattached inode" preen can't fix; the script also auto-repairs disks
# damaged that way by an earlier version). Swap contents are dead the moment a VM stops
# (swap never survives a boot), and init.sh recreates the file — sized to the
# filesystem — on the next boot, so this is pure reclaim: the swapfile is
# fully allocated (fallocate/dd, not sparse), up to 2GB per VM on large-RAM
# fleets, and a plain free-block discard can never touch it because it's a
# live file.
#
set -uo pipefail

RUN_DIR="${1:-${HOME}/.heyo/run}"
DRY_RUN="${DRY_RUN:-0}"
SHRINK="${SHRINK:-0}"
MIN_FS_MB="${MIN_FS_MB:-2048}"
PRUNE_SWAP="${PRUNE_SWAP:-0}"
# Cooperative early stop. A pass over a big fleet takes minutes, and the pooler
# must not boot a VM while this script might fsck its disk — so it blocks boots
# for the whole run. When a boot is waiting, the pooler creates STOP_FILE; this
# script finishes the disk it is on and exits. It can't just be killed: it runs
# under sudo, so the pooler can only signal the shell it spawned while the root
# e2fsck under it keeps writing — which is exactly the corruption the mutual
# exclusion exists to prevent.
STOP_FILE="${STOP_FILE:-$RUN_DIR/.reclaim-stop}"
# Where a stopped pass left off, so the next one resumes after that disk instead
# of re-walking the same prefix forever. On a host that yields often, this is
# what makes fleet-wide progress possible at all.
CURSOR_FILE="${CURSOR_FILE:-$RUN_DIR/.reclaim-cursor}"

die() { echo "error: $*" >&2; exit 1; }
human() { numfmt --to=iec --suffix=B "${1:-0}" 2>/dev/null || echo "${1:-0}B"; }
# Actual on-disk bytes (allocated blocks), not the sparse apparent size.
allocated() { du -B1 "$1" 2>/dev/null | cut -f1; }

# Flags may also be passed as arguments after RUN_DIR (equivalent to the env
# vars). This exists for `sudo -n` callers behind a pinned sudoers entry —
# sudoers can pin an exact argument list, while env assignments are silently
# refused without a SETENV tag (a trap that has produced "did nothing" runs).
[ $# -gt 0 ] && shift
for arg in "$@"; do
    case "$arg" in
        --shrink) SHRINK=1 ;;
        --prune-swap) PRUNE_SWAP=1 ;;
        --dry-run) DRY_RUN=1 ;;
        *) die "unknown argument: $arg (expected --shrink, --prune-swap, --dry-run)" ;;
    esac
done

if [ "$(id -u)" != 0 ]; then
    # A dry run only reads, so allow it — but warn: without root the /proc scan
    # can't see other users' open files, so the in-use check may under-report.
    [ "$DRY_RUN" = 1 ] || die "must run as root (needs the full /proc scan + write access to the disks)"
    echo "warning: not root — dry-run in-use detection may be incomplete" >&2
fi
[ -d "$RUN_DIR" ] || die "run dir not found: $RUN_DIR"
for tool in e2fsck dumpe2fs find stat du numfmt; do
    command -v "$tool" >/dev/null || die "missing required tool: $tool"
done
if [ "$SHRINK" = 1 ]; then
    for tool in resize2fs fallocate; do
        command -v "$tool" >/dev/null || die "missing required tool for SHRINK=1: $tool"
    done
fi
if [ "$PRUNE_SWAP" = 1 ]; then
    command -v debugfs >/dev/null || die "missing required tool for PRUNE_SWAP=1: debugfs"
fi

# Point-in-time set of every file held open by any process, keyed by
# device:inode. A live Firecracker keeps its data disk open as a plain fd under
# /proc/<pid>/fd — but Firecracker usually runs under *jailer*, which chroots the
# VM, so the fd's path is relative to that chroot and will NOT equal the host
# path. Matching by path therefore silently misses running VMs and trims them
# (corrupting a disk the guest has mounted RW). device:inode is the same file
# object regardless of chroot, bind mount, or mount namespace, so it is the
# safe identity to compare.
#
# Built once (a single `find | stat` over all fds) rather than re-scanning /proc
# per disk, which is O(disks x fds) and does not finish on a busy host. Root sees
# every process's fds; without it some are unreadable and silently skipped (hence
# the not-root warning above).
#
# Still a snapshot: a VM that *starts* mid-run won't be seen — run during low
# traffic. The blast radius of a miss is one disk.
declare -A OPEN_INODES=()
snapshot_open_files() {
    local key path pid
    while read -r key path; do
        [ -n "$key" ] || continue
        # Remember (one of) the holding PIDs so a skip can say *who* — the
        # difference between "that VM is running" and "something unexpected
        # holds every disk" is the whole diagnosis.
        pid="${path#/proc/}"
        pid="${pid%%/*}"
        OPEN_INODES["$key"]="${OPEN_INODES[$key]:-$pid}"
    done < <(find /proc/[0-9]*/fd -maxdepth 1 -type l -exec stat -L -c '%d:%i %n' {} + 2>/dev/null)
}

# In use if some process holds this exact file (device:inode) open. Fails closed:
# if the disk can't be stat'd we treat it as in use and skip it.
disk_in_use() {
    local key
    key=$(stat -c '%d:%i' "$1" 2>/dev/null) || return 0
    [ -n "${OPEN_INODES[$key]:-}" ]
}

shopt -s nullglob
disks=("$RUN_DIR"/sb-*/data.ext4)
[ "${#disks[@]}" -gt 0 ] || die "no sb-*/data.ext4 disks under $RUN_DIR"

# Resume after the disk the last (stopped) pass finished. The glob order is
# stable, so rotating the list is enough — no state beyond one path.
resume_from=""
[ "$DRY_RUN" = 1 ] || resume_from=$(cat "$CURSOR_FILE" 2>/dev/null || true)
if [ -n "$resume_from" ]; then
    for i in "${!disks[@]}"; do
        if [ "${disks[$i]}" = "$resume_from" ]; then
            start=$(( (i + 1) % ${#disks[@]} ))
            [ "$start" -gt 0 ] && disks=("${disks[@]:$start}" "${disks[@]:0:$start}")
            echo "resuming after $resume_from"
            break
        fi
    done
fi

reclaimed=0 trimmed=0 skipped=0 failed=0 shrunk=0 dry_reclaimable=0

# SHRINK=1: shrink the (freshly fsck'd) filesystem inside image file $1 toward
# its used size, then hole-punch the backing file past the new fs end (blocks
# there are dead — old metadata/data from the full-size format that a
# free-block discard can't reach, since they're outside the fs). Everything
# operates on the file directly. Returns: 0 = shrunk (counted) or nothing to
# do, 1 = shrink failed (fs untouched — reclaim may proceed), 2 = post-shrink
# fsck failed (leave the disk alone and report).
shrink_fs() {
    local disk="$1" geom bs blocks free used target floor_blk
    geom=$(dumpe2fs -h "$disk" 2>/dev/null) || return 1
    bs=$(awk -F: '/^Block size:/ {gsub(/ /,"",$2); print $2}' <<<"$geom")
    blocks=$(awk -F: '/^Block count:/ {gsub(/ /,"",$2); print $2}' <<<"$geom")
    free=$(awk -F: '/^Free blocks:/ {gsub(/ /,"",$2); print $2}' <<<"$geom")
    { [ -n "$bs" ] && [ -n "$blocks" ] && [ -n "$free" ]; } || return 1
    used=$((blocks - free))
    target=$((used + used / 4))
    floor_blk=$((MIN_FS_MB * 1024 * 1024 / bs))
    [ "$target" -lt "$floor_blk" ] && target=$floor_blk
    # Not worth a slow, block-moving resize unless it retires at least 256MB
    # of future ratchet room.
    [ "$blocks" -le $((target + 268435456 / bs)) ] && return 0
    resize2fs "$disk" "$target" >/dev/null 2>&1 || return 1
    # A shrink relocates data; verify the result before going further.
    e2fsck -fp "$disk" >/dev/null 2>&1
    [ $? -ge 4 ] && return 2
    local fs_bytes=$((target * bs)) file_bytes
    file_bytes=$(stat -c %s "$disk" 2>/dev/null || echo 0)
    if [ "$file_bytes" -gt "$fs_bytes" ]; then
        fallocate -p -o "$fs_bytes" -l $((file_bytes - fs_bytes)) "$disk" 2>/dev/null
    fi
    shrunk=$((shrunk + 1))
    return 0
}

# Trim one disk with fully local cleanup. Returns non-zero on any failure but the
# caller keeps going — a single bad disk must not stop the sweep.
trim_one() {
    local disk="$1"
    # Guard: never touch a disk a running VM has open. Name the holder — an
    # expected skip is a firecracker/jailer PID; anything else (or *every*
    # disk skipped) points at a process unexpectedly pinning the fleet.
    if disk_in_use "$disk"; then
        local key pid comm="?"
        key=$(stat -c '%d:%i' "$disk" 2>/dev/null)
        pid="${OPEN_INODES[${key:-none}]:-?}"
        [ -r "/proc/$pid/comm" ] && comm=$(cat "/proc/$pid/comm" 2>/dev/null)
        echo "skip  (in use by pid $pid/$comm)  $disk"
        skipped=$((skipped + 1))
        return 0
    fi

    local before after
    before=$(allocated "$disk")

    if [ "$DRY_RUN" = 1 ]; then
        # Estimate what a trim would actually free: allocated bytes minus the
        # filesystem's used bytes. Without this, "would-trim" reads as savings
        # when it's only candidacy — an already-lean disk would-trims ~0B.
        local est=""
        if command -v dumpe2fs >/dev/null; then
            local geom bs total_b free_b
            geom=$(dumpe2fs -h "$disk" 2>/dev/null)
            bs=$(awk -F: '/^Block size:/ {gsub(/ /,"",$2); print $2}' <<<"$geom")
            total_b=$(awk -F: '/^Block count:/ {gsub(/ /,"",$2); print $2}' <<<"$geom")
            free_b=$(awk -F: '/^Free blocks:/ {gsub(/ /,"",$2); print $2}' <<<"$geom")
            if [ -n "$bs" ] && [ -n "$total_b" ] && [ -n "$free_b" ]; then
                local gain=$((before - (total_b - free_b) * bs))
                [ "$gain" -lt 0 ] && gain=0
                est="  ~$(human "$gain") reclaimable"
                dry_reclaimable=$((dry_reclaimable + gain))
            fi
        fi
        echo "would-trim$([ "$SHRINK" = 1 ] && echo ' (+shrink)')  $disk  ($(human "$before") allocated$est)"
        return 0
    fi

    # Recover the journal FIRST (VMs are killed uncleanly, so it's usually
    # dirty) — before any debugfs surgery. An earlier version pruned the
    # swapfile before this fsck; the journal replay then overwrote the
    # modified metadata and left an "Unattached inode 12" (the swapfile) that
    # preen refuses to fix, FAILing the whole fleet. -p auto-fixes and exits 1
    # when it did — expected, not a failure. Only a code >= 4 means the
    # filesystem is still bad. Output is captured and surfaced on failure: a
    # whole fleet FAILing for one systemic reason (host e2fsck too old for the
    # guest's ext4 features, OOM, permissions) is undiagnosable from a bare
    # exit code.
    local fsck_out
    fsck_out=$(e2fsck -fp "$disk" 2>&1)
    local fsck_rc=$?

    # Remediate the damage that old ordering left behind: preen reports
    # "Unattached inode N" and gives up. Strictly NAMELESS inodes (no dirent
    # anywhere — ncheck prints nothing) are unreachable dead weight, i.e. the
    # ex-swapfile; kill_file frees their blocks, clri zeroes the inode so
    # nothing resurrects it, and a re-preen leaves a clean filesystem. An
    # unattached inode that still has a name anywhere is a real file and is
    # never touched (the FAIL stands, for badfs/manual recovery).
    if [ "$fsck_rc" -ge 4 ] && echo "$fsck_out" | grep -q "Unattached inode"; then
        local ino killed=0
        for ino in $(echo "$fsck_out" | grep -oE 'Unattached (zero-length )?inode [0-9]+' \
                     | grep -oE '[0-9]+$' | sort -un); do
            if ! debugfs -R "ncheck $ino" "$disk" 2>/dev/null \
                 | awk 'NR>1 && NF>=2 {f=1} END {exit !f}'; then
                debugfs -w -R "kill_file <$ino>" "$disk" >/dev/null 2>&1
                debugfs -w -R "clri <$ino>" "$disk" >/dev/null 2>&1
                killed=$((killed + 1))
            fi
        done
        if [ "$killed" -gt 0 ]; then
            fsck_out=$(e2fsck -fp "$disk" 2>&1)
            fsck_rc=$?
            [ "$fsck_rc" -lt 4 ] \
                && echo "note  (freed $killed nameless unattached inode(s): stale pruned swap)  $disk"
        fi
    fi

    if [ "$fsck_rc" -ge 4 ]; then
        echo "FAIL  (fsck=$fsck_rc)     $disk"
        [ -n "$fsck_out" ] && echo "$fsck_out" | grep -v '^$' | head -n 2 | sed 's/^/      /'
        case "$fsck_out" in
            *"unsupported feature"*|*"Get a newer version"*|*"unknown ROCOMPAT"*|*"unknown INCOMPAT"*)
                echo "      hint: the guest's mkfs enabled ext4 features this host's e2fsprogs" \
                     "($(e2fsck -V 2>&1 | head -n1)) doesn't know — upgrade e2fsprogs on the host" ;;
        esac
        failed=$((failed + 1))
        return 1
    fi

    # Drop the dead swapfile — only now, on a recovered journal (see above).
    # On a clean filesystem debugfs's rm fully deallocates (1.47+); older
    # debugfs leaves a zero-dtime deleted inode, which the preen right after
    # (and the discard fsck below) auto-fix.
    if [ "$PRUNE_SWAP" = 1 ] \
        && debugfs -R "stat /swapfile" "$disk" >/dev/null 2>&1; then
        debugfs -w -R "rm /swapfile" "$disk" >/dev/null 2>&1
        e2fsck -fp "$disk" >/dev/null 2>&1
        if [ $? -ge 4 ]; then
            echo "FAIL  (fsck after swap prune)  $disk"
            failed=$((failed + 1))
            return 1
        fi
    fi

    # Optional filesystem shrink (see shrink_fs).
    if [ "$SHRINK" = 1 ]; then
        shrink_fs "$disk"
        case $? in
            1) echo "note  (shrink failed, reclaim only)  $disk" ;;
            2)
                echo "FAIL  (fsck after shrink)  $disk"
                failed=$((failed + 1))
                return 1
                ;;
        esac
    fi

    # The reclaim itself: punch every free block and unused inode-table block
    # out of the backing file. File-level fallocate(PUNCH_HOLE) — works even
    # where loop-device discard doesn't.
    fsck_out=$(e2fsck -fp -E discard "$disk" 2>&1)
    if [ $? -ge 4 ]; then
        echo "FAIL  (discard fsck)  $disk"
        [ -n "$fsck_out" ] && echo "$fsck_out" | grep -v '^$' | head -n 2 | sed 's/^/      /'
        failed=$((failed + 1))
        return 1
    fi

    after=$(allocated "$disk")
    local freed=$((before - after))
    [ "$freed" -lt 0 ] && freed=0
    reclaimed=$((reclaimed + freed))
    trimmed=$((trimmed + 1))
    printf 'trim  %-12s %s\n' "-$(human "$freed")" "$disk"
    return 0
}

# Announce the active flags: env vars silently stripped by sudo's env_reset
# (`SHRINK=1 sudo …` instead of `sudo SHRINK=1 …`) have twice produced runs
# that "did nothing" — make a flagless run visible in the first line.
echo "reclaim-disks: ${#disks[@]} disk(s) under $RUN_DIR (dry-run=$DRY_RUN shrink=$SHRINK prune-swap=$PRUNE_SWAP; $(e2fsck -V 2>&1 | head -n1))"
snapshot_open_files
stopped_early=0 done_count=0 last_done=""
for disk in "${disks[@]}"; do
    # Between disks — never inside one — is the only safe place to stop: a
    # half-finished e2fsck is exactly what must not be left behind.
    if [ -e "$STOP_FILE" ]; then
        stopped_early=1
        break
    fi
    trim_one "$disk"
    done_count=$((done_count + 1))
    last_done="$disk"
done
if [ "$DRY_RUN" != 1 ]; then
    if [ "$stopped_early" = 1 ] && [ -n "$last_done" ]; then
        printf '%s\n' "$last_done" >"$CURSOR_FILE" 2>/dev/null || true
    else
        # A complete pass has no remainder to resume from.
        rm -f "$CURSOR_FILE" 2>/dev/null || true
    fi
fi

echo "----"
if [ "$DRY_RUN" = 1 ]; then
    echo "dry-run: $((${#disks[@]} - skipped)) candidate(s), ~$(human "$dry_reclaimable") reclaimable by trim, $skipped in use (skipped)"
else
    shrink_note=""
    [ "$SHRINK" = 1 ] && shrink_note=" ($shrunk filesystem(s) shrunk)"
    stop_note=""
    [ "$stopped_early" = 1 ] && stop_note=" — stopped early after $done_count/${#disks[@]} disk(s) (a VM needed to boot); next run resumes here"
    echo "trimmed $trimmed disk(s), reclaimed $(human "$reclaimed")$shrink_note; $skipped in use, $failed failed$stop_note"
fi
