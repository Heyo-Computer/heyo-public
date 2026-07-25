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
#   Phases (default: halt purge reclaim dumps — everything except drain):
#     halt purge reclaim dumps drain
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
        --keep-spares) KEEP_SPARES=1; shift ;;
        --reap-timeout) REAP_TIMEOUT="$2"; shift 2 ;;
        --yes)         YES=1; shift ;;
        --dry-run)     DRY_RUN=1; shift ;;
        halt|purge|reclaim|dumps|drain) PHASES+=("$1"); shift ;;
        -h|--help)     sed -n '2,69p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

# -------------------------------------------------------------- main ------
echo "emergency-drain: phases [${PHASES[*]}] (dry-run=$DRY_RUN)"
echo "   run-dir:  $RUN_DIR"
echo "   registry: $STATE_FILE"
[ -n "$SPARE_DIR" ] && echo "   spare:    $SPARE_DIR"
df_report

if has_phase purge || has_phase drain; then
    confirm "This will delete VMs / archive schemas on this host. Proceed?" || die "aborted"
fi

for p in "${PHASES[@]}"; do
    case "$p" in
        halt)    phase_halt ;;
        purge)   phase_purge ;;
        reclaim) phase_reclaim ;;
        dumps)   phase_dumps ;;
        drain)   phase_drain ;;
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
