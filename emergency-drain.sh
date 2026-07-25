#!/usr/bin/env bash
#
# emergency-drain.sh — clear a disk-full box when the pooler's own tiering
# can't save itself.
#
# The failure mode this exists for: the host disk fills, every freeze/archive
# attempt then *boots a VM to dump it* — which grows that VM's data.ext4
# (in-guest dump file + recreated swapfile) on the already-full disk — the
# attempt fails, the sweep moves to the next candidate, and each failed
# attempt converts more sparse holes into allocated blocks. The built-in
# circuit breakers bound one pass, but every new pass rolls the snowball
# further. Meanwhile nothing can be reclaimed because reclaim needs the
# pooler healthy and the sweeps quiet.
#
# This script breaks the loop by only ever running steps that free space
# WITHOUT first consuming any, in this order:
#
#   halt     stop the pooler (supervisorctl) so no sweep boots anything
#   purge    delete sandboxes whose data is already durably offloaded:
#              - schemas registry.tsv marks `frozen`/`archived` (the pooler
#                flips the tier only AFTER the dump is size-verified durable,
#                so their VM + data.ext4 are pure waste — these are exactly
#                the "killing the VM failed (orphaned)" leftovers)
#              - unclaimed warm spares (spare-pg-*, empty by construction)
#            plus REPORT-ONLY listing of pg-* sandboxes unknown to the
#            registry (never auto-deleted: they might hold data)
#   reclaim  offline-trim every stopped data.ext4 (reclaim-disks.sh with
#            --shrink --prune-swap) — e2fsck hole-punching needs no free space
#   dumps    relocate the local freeze-dump dir to the spare disk and leave a
#            symlink, so every future freeze lands on the disk with room
#   drain    (optional; needs the dashboard + S3 tier) restart the pooler and
#            serially reap every live schema to S3, oldest-idle first — ONE at
#            a time, waiting for the registry tier flip between schemas, with
#            a reclaim pass every few kills and a hard stop on repeated
#            failure. Serial matters: at most one VM's transient boot+dump
#            growth is ever in flight.
#   ghosts   (read-only) reconcile running firecracker processes against
#            heyvmd's inventory and the registry. A "ghost" is a running VM
#            heyvmd has forgotten — typically created/started while the disk
#            was full (heyvmd acked but couldn't persist, then restarted).
#            Ghosts serve Postgres at their guest IP but every control op
#            (exec/stop/kill) 404s, so reap/freeze can never succeed on them,
#            and their held-open disks are invisible to reclaim.
#   rescue   offload each ghost's live schema WITHOUT heyvmd: pg_dump it from
#            the host over the direct guest-IP TCP path (the same `pg_dump
#            -Fc` the in-guest job runs, so the file is a drop-in frozen-tier
#            dump), verify size (+ pg_restore --list when available),
#            CHECKPOINT, kill the firecracker process, flip the schema to
#            `frozen` in registry.tsv (backup kept), and delete the orphaned
#            sb-<id> dir heyvmd no longer tracks. Pooler must be stopped.
#            Needs pg_dump/psql on the host with a major version >= the
#            guest's Postgres; exports PGPASSWORD from --pg-password.
#   orphans  act on sb-* dirs that are NOT running and NOT in heyvmd's
#            inventory (the dirs `ghosts` lists as orphaned — débris of
#            creates/deletes that happened while heyvmd couldn't persist):
#              - schema tier frozen/archived  -> delete (data is offloaded)
#              - tier live, or unbound        -> QUARANTINE: sparse-copy the
#                dir to --spare-dir/quarantine and delete the original. Never
#                deleted: a live-tier orphan dir may hold the ONLY copy of
#                that schema's data (heyvmd forgot the sandbox, so the pooler
#                would rebuild the schema as an EMPTY database on next
#                connect). See the recovery note the phase prints.
#   badfs    repair data disks the reclaim pass reported as FAIL (preen
#            e2fsck exited >= 4 — typical of ENOSPC writes during the
#            incident). Re-identifies them with `e2fsck -fp`, sparse-copies a
#            backup to --spare-dir/fsck-backups when there's room, then runs
#            the full `e2fsck -fy`. Only touches disks that are not held open
#            and whose schema is not already offloaded.
#
# If you skip `drain` (no dashboard, or you prefer the local freeze tier),
# the endgame after `dumps` is: set PG_VM_POOL_FREEZE_AFTER_SECS low (e.g.
# 60), `supervisorctl start pg-vm-pool`, and let the freeze sweep drain the
# box at 25 schemas/sweep onto the spare disk — the headroom created above is
# what lets those boots succeed again.
#
# Usage:
#   sudo ./emergency-drain.sh --spare-dir /mnt/spare/pg-rescue [phases…]
#
#   Phases (default: halt purge reclaim dumps — the read-only ghosts and the
#   heavier drain/rescue/orphans/badfs are opt-in):
#     halt purge reclaim dumps drain ghosts rescue orphans badfs
#
#   Options:
#     --spare-dir DIR    directory on the disk WITH room (required for dumps)
#     --run-dir DIR      heyvmd run dir            (default: <home>/.heyo/run)
#     --state FILE       pooler registry.tsv       (default: <home>/.heyo/pg-vm-pool/registry.tsv)
#     --dump-dir DIR     pooler local dump dir     (default: <home>/.heyo/pg-vm-pool/dumps)
#     --heyvmd URL       heyvmd API                (default: http://127.0.0.1:34099)
#     --dash URL         pooler dashboard base URL (enables drain), e.g. http://127.0.0.1:8080
#     --dash-auth U:P    dashboard basic auth (PG_VM_POOL_DASHBOARD_USER/PASSWORD)
#     --pooler-unit NAME supervisor program name   (default: pg-vm-pool)
#     --pg-user U        guest Postgres user for rescue (default: postgres)
#     --pg-password P    guest Postgres password for rescue (or export PGPASSWORD)
#     --keep-spares      don't delete unclaimed warm spares in `purge`
#     --reap-timeout S   drain: max wait for one schema's tier flip (default 2100,
#                        matching the pooler's 1800s archive deadline + slack)
#     --yes              no confirmation prompts
#     --dry-run          print what would happen; nothing is changed
#
# <home> above is the *invoking* user's home when run under sudo (SUDO_USER),
# not root's — the same trap reclaim-disks.sh documents.
#
# Requires: jq, curl, plus everything reclaim-disks.sh needs. Root for
# reclaim + supervisorctl; a --dry-run works unprivileged.
#
set -uo pipefail

die() { echo "error: $*" >&2; exit 1; }
note() { echo "== $*"; }
human() { numfmt --to=iec --suffix=B "${1:-0}" 2>/dev/null || echo "${1:-0}B"; }

# Resolve the real user's home under sudo, so defaults point at the pooler's
# files rather than /root's.
if [ -n "${SUDO_USER:-}" ]; then
    USER_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
else
    USER_HOME="$HOME"
fi

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

RUN_DIR="$USER_HOME/.heyo/run"
STATE_FILE="$USER_HOME/.heyo/pg-vm-pool/registry.tsv"
DUMP_DIR="$USER_HOME/.heyo/pg-vm-pool/dumps"
HEYVMD="http://127.0.0.1:34099"
SPARE_DIR=""
DASH=""
DASH_AUTH=""
POOLER_UNIT="pg-vm-pool"
PG_USER="postgres"
PG_PASSWORD="${PGPASSWORD:-}"
KEEP_SPARES=0
REAP_TIMEOUT=2100
YES=0
DRY_RUN=0
PHASES=()

while [ $# -gt 0 ]; do
    case "$1" in
        --spare-dir)   SPARE_DIR="$2"; shift 2 ;;
        --run-dir)     RUN_DIR="$2"; shift 2 ;;
        --state)       STATE_FILE="$2"; shift 2 ;;
        --dump-dir)    DUMP_DIR="$2"; shift 2 ;;
        --heyvmd)      HEYVMD="$2"; shift 2 ;;
        --dash)        DASH="$2"; shift 2 ;;
        --dash-auth)   DASH_AUTH="$2"; shift 2 ;;
        --pooler-unit) POOLER_UNIT="$2"; shift 2 ;;
        --pg-user)     PG_USER="$2"; shift 2 ;;
        --pg-password) PG_PASSWORD="$2"; shift 2 ;;
        --keep-spares) KEEP_SPARES=1; shift ;;
        --reap-timeout) REAP_TIMEOUT="$2"; shift 2 ;;
        --yes)         YES=1; shift ;;
        --dry-run)     DRY_RUN=1; shift ;;
        halt|purge|reclaim|dumps|drain|ghosts|rescue|orphans|badfs) PHASES+=("$1"); shift ;;
        -h|--help)     sed -n '2,104p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done
[ ${#PHASES[@]} -gt 0 ] || PHASES=(halt purge reclaim dumps)

for tool in jq curl numfmt df awk; do
    command -v "$tool" >/dev/null || die "missing required tool: $tool"
done
[ -f "$STATE_FILE" ] || die "registry not found: $STATE_FILE (wrong --state? wrong home under sudo?)"
[ -d "$RUN_DIR" ] || die "run dir not found: $RUN_DIR"

has_phase() { local p; for p in "${PHASES[@]}"; do [ "$p" = "$1" ] && return 0; done; return 1; }

confirm() {
    [ "$YES" = 1 ] && return 0
    [ "$DRY_RUN" = 1 ] && return 0
    local reply
    read -r -p "$1 [y/N] " reply
    [ "$reply" = y ] || [ "$reply" = Y ]
}

run() {
    if [ "$DRY_RUN" = 1 ]; then
        echo "DRY-RUN: $*"
    else
        "$@"
    fi
}

# Percent-used of the filesystem holding $1 (df's own Use% basis).
disk_pct() { df -kP "$1" 2>/dev/null | awk 'NR==2 {gsub(/%/,"",$5); print $5}'; }

df_report() {
    echo "-- disk usage --"
    df -hP "$RUN_DIR" | sed 's/^/   /'
    if [ -n "$SPARE_DIR" ] && [ -d "$SPARE_DIR" ]; then
        df -hP "$SPARE_DIR" | awk 'NR==2' | sed 's/^/   /'
    fi
}

# registry.tsv columns: schema \t sandbox_id \t last_active \t tier
# (tier absent in old files = live). Never written by this script — the
# pooler owns it; we only read.
reg_rows() { awk -F'\t' 'NF>=2 && $1!="" && $2!=""' "$STATE_FILE"; }
tier_of() { awk -F'\t' -v s="$1" '$1==s {print ($4==""?"live":$4); exit}' "$STATE_FILE"; }

heyvmd_list() {
    curl -fsS --max-time 10 "$HEYVMD/deployed-sandboxes" \
        || die "heyvmd not answering at $HEYVMD — is it running? (supervisorctl status heyvmd)"
}

heyvmd_delete() {
    # DELETE is permanent; the SDK treats 404 as success and so do we.
    local id="$1" code
    code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 60 -X DELETE \
        "$HEYVMD/deployed-sandboxes/$id")
    case "$code" in
        2*|404) return 0 ;;
        *) echo "   delete $id failed (HTTP $code)" >&2; return 1 ;;
    esac
}

# Point-in-time device:inode → holding-pid map of every open file on the host
# (same technique and caveats as reclaim-disks.sh: jailer chroots make fd
# *paths* useless, device:inode is chroot-proof; needs root to see all fds).
declare -A OPEN_INODES=()
scan_open_files() {
    OPEN_INODES=()
    local key path pid
    while read -r key path; do
        [ -n "$key" ] || continue
        pid="${path#/proc/}"
        pid="${pid%%/*}"
        OPEN_INODES["$key"]="${OPEN_INODES[$key]:-$pid}"
    done < <(find /proc/[0-9]*/fd -maxdepth 1 -type l -exec stat -L -c '%d:%i %n' {} + 2>/dev/null)
}

# PID holding $1 open, or empty. Prints nothing (in-use unknown) if the file
# can't be stat'd.
holder_pid() {
    local key
    key=$(stat -c '%d:%i' "$1" 2>/dev/null) || return 0
    echo "${OPEN_INODES[$key]:-}"
}

# Best-effort guest IP of the firecracker process $1: its --config-file (read
# through /proc/<pid>/root, so the jailer chroot is transparent) carries the
# kernel boot_args, which embed `ip=<guest>:<server>:<gw>:…`.
ghost_ip() {
    local pid="$1" cfg args
    cfg=$(tr '\0' '\n' </proc/"$pid"/cmdline 2>/dev/null \
        | awk '/^--config-file$/ {getline; print; exit}')
    [ -n "$cfg" ] || return 1
    args=$(jq -r '."boot-source".boot_args // empty' "/proc/$pid/root/$cfg" 2>/dev/null)
    if [[ "$args" =~ ip=([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+) ]]; then
        echo "${BASH_REMATCH[1]}"
        return 0
    fi
    return 1
}

# schema for sandbox id $1 per the registry (empty if unbound).
schema_of_id() { awk -F'\t' -v id="$1" '$2==id {print $1; exit}' "$STATE_FILE"; }

# ---------------------------------------------------------------- halt ----
phase_halt() {
    note "halt: stopping the pooler so no sweep can boot VMs under us"
    if command -v supervisorctl >/dev/null; then
        run supervisorctl stop "$POOLER_UNIT" || echo "   (stop failed — already stopped, or not a supervisor unit?)"
    else
        echo "   supervisorctl not found — stop the pooler yourself before continuing"
        confirm "   pooler is stopped; continue?" || die "aborted"
    fi
    # Sanity: with traffic off and the pooler down, no pg-* firecracker should
    # still hold a data disk. Report any that do — running VMs can't be
    # purged, trimmed, or safely dumped from outside.
    local sandboxes held
    sandboxes=$(heyvmd_list) || exit 1
    held=$(jq -r '.[] | select(.status=="running" or .status=="Running") | "\(.id)\t\(.name // "?")"' <<<"$sandboxes")
    if [ -n "$held" ]; then
        echo "   still-running sandboxes (their disks are skipped by every later phase):"
        echo "$held" | sed 's/^/     /'
    else
        echo "   no running sandboxes"
    fi
}

# --------------------------------------------------------------- purge ----
phase_purge() {
    note "purge: deleting sandboxes whose data is already safely offloaded"
    local sandboxes ids_bound
    sandboxes=$(heyvmd_list) || exit 1
    # Every sandbox id the registry binds to some schema, whatever its tier.
    ids_bound=$(reg_rows | cut -f2 | sort -u)

    # 1. Offloaded schemas whose sandbox still exists. Tier frozen/archived is
    #    only ever written AFTER a size-verified durable dump (store.set_tier
    #    is fsync'd before the kill), so the VM is a leftover of a failed
    #    kill — deleting it cannot lose data. Restore ignores the stored id
    #    for offloaded tiers, so the dangling registry row is harmless.
    local count=0 schema id tier
    while IFS=$'\t' read -r schema id _last tier; do
        tier=${tier:-live}
        [ "$tier" = frozen ] || [ "$tier" = archived ] || continue
        # Does the sandbox still exist?
        if echo "$sandboxes" | jq -e --arg id "$id" '.[] | select(.id==$id)' >/dev/null; then
            echo "   schema $schema is $tier but its VM $id still exists — deleting"
            if run heyvmd_delete "$id"; then count=$((count + 1)); fi
        fi
    done < <(reg_rows)
    echo "   deleted $count offloaded-schema VM(s)"

    # 2. Unclaimed warm spares: spare-pg-* sandboxes not bound to any schema.
    #    Spares are pre-booted EMPTY VMs; the replenisher recreates them when
    #    the pooler is healthy again. A claimed spare keeps its spare-pg-*
    #    name, so "unbound in the registry" is the real test, not the name.
    if [ "$KEEP_SPARES" != 1 ]; then
        local spares n=0
        spares=$(echo "$sandboxes" | jq -r '.[] | select(.name // "" | startswith("spare-pg-")) | .id')
        for id in $spares; do
            if ! grep -qxF "$id" <<<"$ids_bound"; then
                echo "   unclaimed spare $id — deleting"
                if run heyvmd_delete "$id"; then n=$((n + 1)); fi
            fi
        done
        echo "   deleted $n unclaimed spare(s)"
    fi

    # 3. Report-only: pg-* sandboxes the registry has never heard of. These
    #    can exist after a crash between VM create and the registry write, and
    #    MAY HOLD REAL DATA — never auto-deleted; a human decides.
    local unknown
    unknown=$(echo "$sandboxes" | jq -r '.[] | select((.name // "" | startswith("pg-")) ) | "\(.id)\t\(.name)"' \
        | while IFS=$'\t' read -r id name; do
            grep -qxF "$id" <<<"$ids_bound" || echo "     $id  $name"
          done)
    if [ -n "$unknown" ]; then
        echo "   NOT touched — pg-* sandboxes unknown to the registry (may hold data):"
        echo "$unknown"
    fi
    df_report
}

# ------------------------------------------------------------- reclaim ----
phase_reclaim() {
    note "reclaim: offline-trimming every stopped data disk (shrink + swap prune)"
    local rd="$SCRIPT_DIR/reclaim-disks.sh"
    [ -x "$rd" ] || die "reclaim-disks.sh not found/executable next to this script ($rd)"
    if [ "$DRY_RUN" = 1 ]; then
        DRY_RUN=1 "$rd" "$RUN_DIR" --shrink --prune-swap --dry-run
    else
        "$rd" "$RUN_DIR" --shrink --prune-swap
    fi
    df_report
}

# --------------------------------------------------------------- dumps ----
phase_dumps() {
    note "dumps: relocating the freeze-dump dir onto the spare disk"
    [ -n "$SPARE_DIR" ] || die "the dumps phase needs --spare-dir"
    run mkdir -p "$SPARE_DIR/dumps"
    if [ -L "$DUMP_DIR" ]; then
        echo "   $DUMP_DIR is already a symlink -> $(readlink "$DUMP_DIR"); nothing to do"
        return 0
    fi
    if [ -d "$DUMP_DIR" ]; then
        local sz
        sz=$(du -sh "$DUMP_DIR" 2>/dev/null | cut -f1)
        echo "   moving existing dumps (${sz:-0}) to $SPARE_DIR/dumps"
        # mv per entry so a partial move is resumable; cross-device mv copies
        # then unlinks, freeing the primary disk file by file.
        if [ "$DRY_RUN" = 1 ]; then
            echo "DRY-RUN: mv $DUMP_DIR/* $SPARE_DIR/dumps/ && rmdir $DUMP_DIR"
        else
            find "$DUMP_DIR" -mindepth 1 -maxdepth 1 -exec mv -t "$SPARE_DIR/dumps/" {} + 2>/dev/null
            rmdir "$DUMP_DIR" || die "could not empty $DUMP_DIR — resolve by hand, then re-run"
        fi
    fi
    run ln -s "$SPARE_DIR/dumps" "$DUMP_DIR"
    echo "   $DUMP_DIR -> $SPARE_DIR/dumps"
    echo "   (equivalent permanent fix: set PG_VM_POOL_DUMP_DIR=$SPARE_DIR/dumps in the"
    echo "    supervisor unit and \`supervisorctl reread && supervisorctl update\`)"
}

# --------------------------------------------------------------- drain ----
phase_drain() {
    note "drain: serially reaping every live schema to S3, oldest-idle first"
    [ -n "$DASH" ] || die "the drain phase needs --dash (and usually --dash-auth)"
    local auth=()
    [ -n "$DASH_AUTH" ] && auth=(-u "$DASH_AUTH")

    if command -v supervisorctl >/dev/null; then
        run supervisorctl start "$POOLER_UNIT"
        [ "$DRY_RUN" = 1 ] || sleep 5
    fi
    curl -fsS "${auth[@]}" --max-time 10 -o /dev/null "$DASH/" \
        || die "dashboard not answering at $DASH (check PG_VM_POOL_DASHBOARD_LISTEN / auth)"

    local start_pct
    start_pct=$(disk_pct "$RUN_DIR")
    echo "   primary disk at ${start_pct}% to start"

    # Oldest last_active first — same order the pressure reaper uses.
    local queue failures=0 reaped=0 schema id
    queue=$(reg_rows | awk -F'\t' '($4==""||$4=="live")' | sort -t$'\t' -k3,3n)
    local total
    total=$(wc -l <<<"$queue")
    [ -n "$queue" ] || { echo "   nothing live to drain"; return 0; }
    echo "   $total live schema(s) queued"

    while IFS=$'\t' read -r schema id _rest; do
        [ -n "$schema" ] || continue
        echo "   [$((reaped + failures + 1))/$total] reaping $schema (VM $id) to S3…"
        if [ "$DRY_RUN" = 1 ]; then
            echo "DRY-RUN: curl -X POST $DASH/vm/$id/reap; then wait for tier flip"
            continue
        fi
        if ! curl -fsS "${auth[@]}" --max-time 30 -o /dev/null -X POST "$DASH/vm/$id/reap"; then
            echo "     reap request refused"
            failures=$((failures + 1))
        else
            # The reap runs in the pooler's background; the registry tier flip
            # (fsync'd before the VM kill) is the durable success signal.
            local waited=0 tier=live
            while [ "$waited" -lt "$REAP_TIMEOUT" ]; do
                sleep 15; waited=$((waited + 15))
                tier=$(tier_of "$schema")
                [ "$tier" = archived ] || [ "$tier" = frozen ] && break
            done
            if [ "$tier" = archived ] || [ "$tier" = frozen ]; then
                reaped=$((reaped + 1)); failures=0
                echo "     $schema -> $tier after ${waited}s ($(disk_pct "$RUN_DIR")% used)"
            else
                failures=$((failures + 1))
                echo "     $schema still '$tier' after ${waited}s — counting as a failure"
            fi
        fi
        if [ "$failures" -ge 3 ]; then
            echo "   3 consecutive failures — the environment is still sick; stopping the drain"
            echo "   (check the pooler log: supervisorctl tail -f $POOLER_UNIT)"
            break
        fi
        # Return the slack of the VMs just killed every few schemas, so the
        # drain gets faster as it goes instead of waiting for the periodic run.
        if [ $((reaped % 5)) -eq 0 ] && [ "$reaped" -gt 0 ]; then
            curl -fsS "${auth[@]}" --max-time 10 -o /dev/null -X POST "$DASH/monitoring/reclaim" || true
        fi
    done <<<"$queue"

    echo "   drained: $reaped reaped, started ${start_pct}% now $(disk_pct "$RUN_DIR")%"
    df_report
}

# -------------------------------------------------------------- ghosts ----
# One reconciliation row per sb-* dir under RUN_DIR. Emits TSV to stdout:
#   id \t held_by_pid \t in_heyvmd(yes/no) \t schema \t tier \t guest_ip
# so `ghosts` can pretty-print it and `rescue` can act on it. Empty fields are
# emitted as "-" — tab is IFS whitespace, so `read` would otherwise collapse
# adjacent tabs and shift every later column left.
reconcile_rows() {
    local sandboxes dir id disk pid known schema tier ip
    sandboxes=$(heyvmd_list 2>/dev/null) || sandboxes="[]"
    scan_open_files
    for dir in "$RUN_DIR"/sb-*/; do
        [ -d "$dir" ] || continue
        id=$(basename "$dir")
        disk="$dir/data.ext4"
        pid=""
        [ -f "$disk" ] && pid=$(holder_pid "$disk")
        if jq -e --arg id "$id" '.[] | select(.id==$id)' >/dev/null 2>&1 <<<"$sandboxes"; then
            known=yes
        else
            known=no
        fi
        schema=$(schema_of_id "$id")
        tier=""
        [ -n "$schema" ] && tier=$(tier_of "$schema")
        ip=""
        [ -n "$pid" ] && ip=$(ghost_ip "$pid" || true)
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$id" "${pid:--}" "$known" "${schema:--}" "${tier:--}" "${ip:--}"
    done
}

phase_ghosts() {
    note "ghosts: running-VM / heyvmd / registry reconciliation (read-only)"
    if [ "$(id -u)" != 0 ]; then
        echo "   warning: not root — the open-fd scan may miss holders (ghosts under-reported)"
    fi
    if ! curl -fsS --max-time 10 -o /dev/null "$HEYVMD/deployed-sandboxes" 2>/dev/null; then
        echo "   warning: heyvmd NOT answering at $HEYVMD — every sandbox below will read"
        echo "   as unknown to heyvmd; fix heyvmd before trusting the GHOST flags"
    fi
    local rows ghosts=0 orphans=0
    rows=$(reconcile_rows)
    printf '   %-24s %-8s %-8s %-14s %-9s %s\n' SANDBOX PID HEYVMD SCHEMA TIER GUEST-IP
    local id pid known schema tier ip flag
    while IFS=$'\t' read -r id pid known schema tier ip; do
        [ -n "$id" ] || continue
        flag=""
        if [ "$pid" != - ] && [ "$known" = no ]; then
            flag="  <-- GHOST (running, heyvmd forgot it)"
            ghosts=$((ghosts + 1))
        elif [ "$pid" = - ] && [ "$known" = no ]; then
            flag="  (orphaned dir: not running, not in heyvmd)"
            orphans=$((orphans + 1))
        fi
        printf '   %-24s %-8s %-8s %-14s %-9s %-15s%s\n' \
            "$id" "$pid" "$known" "$schema" "$tier" "$ip" "$flag"
    done <<<"$rows"
    echo "   $ghosts ghost(s), $orphans orphaned dir(s)"
    if [ "$ghosts" -gt 0 ]; then
        echo "   ghosts can't be reaped through the pooler (guest exec 404s) — use the"
        echo "   rescue phase to dump them host-side over the guest IP and retire them."
    fi
    if [ "$orphans" -gt 0 ]; then
        echo "   orphaned dirs are handled by the orphans phase (delete if offloaded,"
        echo "   quarantine to --spare-dir otherwise)."
    fi
}

# -------------------------------------------------------------- rescue ----
phase_rescue() {
    note "rescue: host-side dump + retire for ghost VMs of live schemas"
    [ "$(id -u)" = 0 ] || [ "$DRY_RUN" = 1 ] || die "rescue needs root (fd scan + kill + chroot config reads)"
    for tool in pg_dump psql; do
        command -v "$tool" >/dev/null || die "rescue needs $tool on the host (matching the guest's Postgres major)"
    done
    # The pooler MUST be down: it would race us on the registry file and could
    # rebind a live schema to a fresh empty VM the moment we kill its ghost.
    if command -v supervisorctl >/dev/null \
        && supervisorctl status "$POOLER_UNIT" 2>/dev/null | grep -q RUNNING; then
        die "pooler is running — run the halt phase first (rescue edits registry.tsv and kills VMs)"
    fi
    # heyvmd MUST be answering: "ghost" means "running but heyvmd forgot it",
    # and with heyvmd down EVERY sandbox looks forgotten — rescue would retire
    # the whole fleet. Refuse rather than guess.
    heyvmd_list >/dev/null || exit 1
    local dump_target="$DUMP_DIR"
    [ -d "$dump_target" ] || [ -L "$dump_target" ] || die "dump dir $dump_target missing (run the dumps phase, or pass --dump-dir)"
    export PGPASSWORD="$PG_PASSWORD"

    local rows rescued=0 skipped=0
    rows=$(reconcile_rows)
    local id pid known schema tier ip
    while IFS=$'\t' read -r id pid known schema tier ip; do
        # Normalize the "-" empty-field placeholders back to empty strings.
        [ "$pid" = - ] && pid=""
        [ "$schema" = - ] && schema=""
        [ "$tier" = - ] && tier=""
        [ "$ip" = - ] && ip=""
        [ -n "$id" ] && [ -n "$pid" ] && [ "$known" = no ] || continue  # ghosts only
        if [ -z "$schema" ]; then
            echo "   $id (pid $pid): not bound to any schema — leaving alone (unknown data)"
            skipped=$((skipped + 1)); continue
        fi
        if [ "$tier" = frozen ] || [ "$tier" = archived ]; then
            # Data already durably offloaded; the ghost is pure waste.
            echo "   $id (pid $pid): schema $schema already $tier — killing and removing dir"
            run kill "$pid"
            [ "$DRY_RUN" = 1 ] || sleep 2
            run rm -rf --one-file-system "$RUN_DIR/$id"
            rescued=$((rescued + 1)); continue
        fi
        if [ -z "$ip" ]; then
            echo "   $id (pid $pid, schema $schema): could not determine guest IP — skipping"
            echo "     (find it in the pooler log: 'direct connection to pg-$schema at <ip>')"
            skipped=$((skipped + 1)); continue
        fi

        echo "   $id (pid $pid): dumping schema $schema from $ip…"
        local out="$dump_target/$schema.dump" tmp="$dump_target/.$schema.dump.rescue"
        if [ "$DRY_RUN" = 1 ]; then
            echo "DRY-RUN: pg_dump -h $ip -U $PG_USER -Fc -d $schema -f $tmp && verify && kill $pid && tier->frozen"
            continue
        fi
        if ! pg_dump -h "$ip" -U "$PG_USER" -Fc -d "$schema" -f "$tmp"; then
            echo "     pg_dump failed — ghost left running, nothing changed"
            rm -f "$tmp"; skipped=$((skipped + 1)); continue
        fi
        local sz
        sz=$(stat -c %s "$tmp" 2>/dev/null || echo 0)
        if [ "$sz" -lt 512 ]; then
            echo "     dump is only ${sz}B — refusing (no real -Fc dump is that small)"
            rm -f "$tmp"; skipped=$((skipped + 1)); continue
        fi
        # Structural integrity: -Fc has a TOC; a truncated file fails --list.
        if command -v pg_restore >/dev/null && ! pg_restore --list "$tmp" >/dev/null 2>&1; then
            echo "     pg_restore --list rejects the dump — refusing"
            rm -f "$tmp"; skipped=$((skipped + 1)); continue
        fi
        mv "$tmp" "$out"
        echo "     dump ok ($(human "$sz")) -> $out"

        # Same courtesy the idle reaper pays before its unclean stop: flush
        # acked commits so the on-disk state is clean too (best-effort — the
        # dump above is the durable copy either way).
        psql -h "$ip" -U "$PG_USER" -d "$schema" -c CHECKPOINT >/dev/null 2>&1 || true
        kill "$pid" 2>/dev/null || true
        local waited=0
        while [ "$waited" -lt 15 ] && kill -0 "$pid" 2>/dev/null; do
            sleep 1; waited=$((waited + 1))
        done
        kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null

        # Durable tier flip so the pooler restores from the dump instead of
        # rebinding the schema to a fresh empty VM. Backup kept alongside.
        cp -p "$STATE_FILE" "$STATE_FILE.bak-rescue" 2>/dev/null || true
        awk -F'\t' -v OFS='\t' -v s="$schema" \
            '$1==s {$4="frozen"} {print}' "$STATE_FILE" >"$STATE_FILE.tmp" \
            && chown --reference="$STATE_FILE" "$STATE_FILE.tmp" 2>/dev/null \
            ; mv "$STATE_FILE.tmp" "$STATE_FILE"
        echo "     schema $schema -> frozen (registry backup: $STATE_FILE.bak-rescue)"

        # heyvmd has no record of this sandbox, so nothing else will ever
        # clean its dir up. The data now lives in the verified dump.
        scan_open_files
        if [ -z "$(holder_pid "$RUN_DIR/$id/data.ext4")" ]; then
            rm -rf --one-file-system "$RUN_DIR/$id"
            echo "     removed orphaned $RUN_DIR/$id"
        else
            echo "     $RUN_DIR/$id/data.ext4 still held open — dir left for a later pass"
        fi
        rescued=$((rescued + 1))
    done <<<"$rows"
    echo "   rescued $rescued ghost(s), skipped $skipped"
    df_report
}

# ------------------------------------------------------------- orphans ----
phase_orphans() {
    note "orphans: sb-* dirs that are not running and unknown to heyvmd"
    [ "$(id -u)" = 0 ] || [ "$DRY_RUN" = 1 ] || die "orphans needs root (fd scan + file moves)"
    # Fail closed exactly like rescue: with heyvmd down, EVERYTHING looks
    # orphaned and this phase would quarantine the whole fleet.
    heyvmd_list >/dev/null || exit 1
    local quarantine=""
    if [ -n "$SPARE_DIR" ]; then
        quarantine="$SPARE_DIR/quarantine"
        run mkdir -p "$quarantine"
    fi

    local rows deleted=0 moved=0 kept=0 freed=0
    local live_quarantined=()
    rows=$(reconcile_rows)
    local id pid known schema tier ip alloc avail
    while IFS=$'\t' read -r id pid known schema tier ip; do
        [ "$pid" = - ] && pid=""
        [ "$schema" = - ] && schema=""
        [ "$tier" = - ] && tier=""
        [ -n "$id" ] && [ -z "$pid" ] && [ "$known" = no ] || continue  # orphan dirs only
        alloc=$(du -sB1 "$RUN_DIR/$id" 2>/dev/null | cut -f1)
        alloc=${alloc:-0}

        if [ "$tier" = frozen ] || [ "$tier" = archived ]; then
            echo "   $id: schema $schema already $tier — deleting ($(human "$alloc"))"
            run rm -rf --one-file-system "$RUN_DIR/$id"
            deleted=$((deleted + 1)); freed=$((freed + alloc))
            continue
        fi

        # Live-tier or unbound: the bytes here may be the only copy of real
        # data, so they are moved intact to the spare disk, never deleted.
        if [ -z "$quarantine" ]; then
            echo "   $id (schema ${schema:-unbound}): would quarantine but no --spare-dir; keeping"
            kept=$((kept + 1)); continue
        fi
        # df against SPARE_DIR, not the quarantine subdir — same filesystem,
        # but the subdir doesn't exist yet under --dry-run.
        avail=$(df -B1 --output=avail "$SPARE_DIR" 2>/dev/null | awk 'NR==2 {print $1}')
        if [ -z "$avail" ] || [ "$avail" -lt $((alloc + alloc / 5)) ]; then
            echo "   $id: spare disk lacks room for $(human "$alloc"); keeping"
            kept=$((kept + 1)); continue
        fi
        echo "   $id (schema ${schema:-unbound}, tier ${tier:-live}): quarantining $(human "$alloc")"
        if [ "$DRY_RUN" = 1 ]; then
            echo "DRY-RUN: cp -a --sparse=always $RUN_DIR/$id $quarantine/ && rm -rf $RUN_DIR/$id"
            moved=$((moved + 1))
            [ -n "$schema" ] && live_quarantined+=("$schema")
            continue
        fi
        # Copy to a .partial name and rename, so an interrupted copy can never
        # be mistaken for a complete quarantine.
        rm -rf "$quarantine/$id.partial"
        if cp -a --sparse=always "$RUN_DIR/$id" "$quarantine/$id.partial" \
            && mv "$quarantine/$id.partial" "$quarantine/$id"; then
            rm -rf --one-file-system "$RUN_DIR/$id"
            moved=$((moved + 1)); freed=$((freed + alloc))
            [ -n "$schema" ] && live_quarantined+=("$schema")
        else
            rm -rf "$quarantine/$id.partial"
            echo "     copy failed — original left in place"
            kept=$((kept + 1))
        fi
    done <<<"$rows"

    echo "   deleted $deleted offloaded orphan(s), quarantined $moved, kept $kept — ~$(human "$freed") freed"
    if [ ${#live_quarantined[@]} -gt 0 ]; then
        echo
        echo "   *** IMPORTANT: these schemas' data now lives ONLY in $quarantine: ***"
        printf '       %s\n' "${live_quarantined[@]}"
        echo "   Their registry tier is still 'live' with a dead sandbox id, so the pooler"
        echo "   would serve them as EMPTY databases on next connect. Before re-enabling"
        echo "   traffic for them, either recover each one:"
        echo "     1. connect once so the pooler creates a fresh pg-<schema> VM, then halt"
        echo "     2. stop that VM; replace its \$RUN_DIR/sb-<new>/data.ext4 with the"
        echo "        quarantined one (fsck it first); start it — data is back under a"
        echo "        sandbox heyvmd tracks"
        echo "   or dump the quarantined disk offline (loop-mount + a matching-major"
        echo "   Postgres) into $DUMP_DIR/<schema>.dump and flip its tier to frozen."
    fi
    df_report
}

# -------------------------------------------------------------- badfs -----
phase_badfs() {
    note "badfs: full-repair pass over data disks that fail preen fsck"
    [ "$(id -u)" = 0 ] || [ "$DRY_RUN" = 1 ] || die "badfs needs root"
    command -v e2fsck >/dev/null || die "badfs needs e2fsck"
    local backups=""
    if [ -n "$SPARE_DIR" ]; then
        backups="$SPARE_DIR/fsck-backups"
        run mkdir -p "$backups"
    fi
    scan_open_files

    local checked=0 bad=0 repaired=0 unfixed=0
    local disk id schema tier rc avail alloc
    for disk in "$RUN_DIR"/sb-*/data.ext4; do
        [ -f "$disk" ] || continue
        id=$(basename "$(dirname "$disk")")
        [ -n "$(holder_pid "$disk")" ] && continue  # running VM — never touch
        schema=$(schema_of_id "$id")
        tier=""
        [ -n "$schema" ] && tier=$(tier_of "$schema")
        if [ "$tier" = frozen ] || [ "$tier" = archived ]; then
            continue  # data offloaded; purge/orphans territory, not worth repairing
        fi
        checked=$((checked + 1))
        # Same probe reclaim-disks.sh uses: preen replays the journal and makes
        # safe fixes; exit >= 4 means real damage preen won't touch. Dry-run
        # substitutes the read-only -fn (it can over-report on a dirty journal,
        # but changes nothing).
        if [ "$DRY_RUN" = 1 ]; then
            e2fsck -fn "$disk" >/dev/null 2>&1
        else
            e2fsck -fp "$disk" >/dev/null 2>&1
        fi
        rc=$?
        [ "$rc" -ge 4 ] || continue
        bad=$((bad + 1))
        echo "   $id (schema ${schema:-unbound}): preen fsck=$rc — attempting full repair"
        if [ "$DRY_RUN" = 1 ]; then
            echo "DRY-RUN: backup to ${backups:-<none>} then e2fsck -fy $disk"
            continue
        fi
        if [ -n "$backups" ]; then
            alloc=$(du -sB1 "$disk" 2>/dev/null | cut -f1)
            alloc=${alloc:-0}
            avail=$(df -B1 --output=avail "$SPARE_DIR" 2>/dev/null | awk 'NR==2 {print $1}')
            if [ -n "$avail" ] && [ "$avail" -ge $((alloc + alloc / 5)) ]; then
                if ! cp -a --sparse=always "$disk" "$backups/$id.data.ext4"; then
                    echo "     backup copy failed — repairing WITHOUT a backup"
                    rm -f "$backups/$id.data.ext4"
                fi
            else
                echo "     spare disk lacks room for a $(human "$alloc") backup — repairing without one"
            fi
        fi
        # -fy answers yes to every fix. It can discard damaged files into
        # lost+found, but preen already refused this disk: the alternative is
        # a filesystem no VM can safely mount at all.
        e2fsck -fy "$disk" >/dev/null 2>&1
        rc=$?
        if [ "$rc" -lt 4 ]; then
            repaired=$((repaired + 1))
            echo "     repaired (fsck=$rc)"
        else
            unfixed=$((unfixed + 1))
            echo "     STILL BAD (fsck=$rc) — leaving disk and backup for manual recovery"
        fi
    done
    echo "   checked $checked live disk(s): $bad bad, $repaired repaired, $unfixed beyond -fy"
    [ "$repaired" -gt 0 ] && echo "   re-run the reclaim phase to trim the freshly repaired disks"
    df_report
}

# -------------------------------------------------------------- main ------
echo "emergency-drain: phases [${PHASES[*]}] (dry-run=$DRY_RUN)"
echo "   run-dir:  $RUN_DIR"
echo "   registry: $STATE_FILE"
[ -n "$SPARE_DIR" ] && echo "   spare:    $SPARE_DIR"
df_report

if has_phase purge || has_phase drain || has_phase rescue || has_phase orphans || has_phase badfs; then
    confirm "This will delete/move VMs, disks, or schemas on this host. Proceed?" || die "aborted"
fi

for p in "${PHASES[@]}"; do
    case "$p" in
        halt)    phase_halt ;;
        purge)   phase_purge ;;
        reclaim) phase_reclaim ;;
        dumps)   phase_dumps ;;
        drain)   phase_drain ;;
        ghosts)  phase_ghosts ;;
        rescue)  phase_rescue ;;
        orphans) phase_orphans ;;
        badfs)   phase_badfs ;;
    esac
done

note "done"
if ! has_phase drain; then
    cat <<EOF
Next steps:
  - To drain the remaining live schemas to S3 one at a time:
      $0 --spare-dir '$SPARE_DIR' --dash <URL> --dash-auth user:pass drain
  - Or to drain via the LOCAL freeze tier (dumps now land on the spare disk):
      set PG_VM_POOL_FREEZE_AFTER_SECS=60 in the supervisor unit, then
      supervisorctl reread && supervisorctl update $POOLER_UNIT
    and the freeze sweep clears 25 schemas per pass (every PG_VM_POOL_FREEZE_SWEEP_SECS).
  - The pooler is currently STOPPED (halt phase); start it when ready:
      supervisorctl start $POOLER_UNIT
EOF
fi
