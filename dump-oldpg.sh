#!/usr/bin/env bash
#
# dump-oldpg.sh — exhume schemas stranded on an old-major Postgres cluster.
#
# The failure this fixes: a schema's data.ext4 holds a pgdata initialized by
# an older Postgres major (e.g. 16), but its VM now boots the newer image —
# the server dies instantly with "database files are incompatible" and every
# archive/freeze attempt burns a ready-timeout forever. The cluster needs
# matching-major binaries exactly once, to dump it; the dump itself is
# version-neutral and restores fine on the new major.
#
# Per candidate schema, entirely host-side (the VM is never booted):
#   1. sparse-COPY the stopped VM's data.ext4 to a work dir (the original is
#      never touched; --in-place skips the copy)
#   2. e2fsck the copy, loop-mount it
#   3. start a throwaway postmaster from the MATCHING major's host binaries
#      (--pg-bin, default /usr/lib/postgresql/<major>/bin) on a private unix
#      socket — no TCP, crash recovery runs exactly as the guest would have
#   4. pg_dump -Fc the schema's database into the pooler's dump dir
#      (verified: size >= 512B and pg_restore --list parses it)
#   5. stop, unmount, delete the copy
#   6. flip the schema to `frozen` in registry.tsv (backup kept) — the next
#      client connect restores it onto the current image via the normal
#      frozen-tier path, and the dashboard purge deletes the old VM
#
# Usage:
#   sudo ./dump-oldpg.sh --list                     # discover candidates only
#   sudo ./dump-oldpg.sh --yes                      # exhume every candidate
#   sudo ./dump-oldpg.sh --schema ogiexFk8 --yes    # just one
#
# Options:
#   --major N          old major to look for              (default: 16)
#   --schema S         limit to this schema (repeatable)
#   --run-dir DIR      heyvmd run dir                     (default: <home>/.heyo/run)
#   --state FILE       pooler registry.tsv                (default: <home>/.heyo/pg-vm-pool/registry.tsv)
#   --dump-dir DIR     pooler local dump dir              (default: <home>/.heyo/pg-vm-pool/dumps)
#   --work-dir DIR     scratch for disk copies + sockets  (default: /var/tmp;
#                      each copy costs the disk's ALLOCATED size, so point it
#                      at the spare disk for big disks. Must be traversable by
#                      --pg-user: a 0700 home directory ("~/scratch") is NOT —
#                      the throwaway postmaster runs as that user)
#   --pg-bin DIR       old-major binaries                 (default: /usr/lib/postgresql/<major>/bin;
#                      `apt install postgresql-16` provides them)
#   --pg-user U        host user to run the postmaster as (default: postgres)
#   --in-place         operate on the original disk (skips the copy; crash
#                      recovery then writes to the only copy — default is safer)
#   --adopt-unbound    also process old-major disks NOT bound to any schema in
#                      the registry (orphans of lost bindings). The owner is
#                      identified from inside the cluster — the pooler names
#                      each database exactly after its schema — then:
#                        · registry has no such schema  -> dump to
#                          <schema>.dump and APPEND a frozen row (the schema
#                          restores normally on its next connect)
#                        · registry already binds that schema elsewhere ->
#                          dump saved as <schema>.recovered-from-<sb-id>.dump,
#                          registry untouched, reported for a human to compare
#                          (the existing binding may be an empty VM created
#                          during an incident — do not assume either side wins)
#                        · cluster has no databases -> reported empty (an old
#                          spare); the disk holds nothing
#   --no-registry      dump only; skip all registry writes
#   --list             report candidates and exit
#   --yes              no confirmation prompt
#
# Requires root (mounts), debugfs, and the old major's server binaries.
# The POOLER MUST BE STOPPED unless --no-registry (the tier flip edits
# registry.tsv, and a running pooler could boot the VM mid-dump).
#
set -uo pipefail

die() { echo "error: $*" >&2; exit 1; }
note() { echo "== $*"; }

if [ -n "${SUDO_USER:-}" ]; then
    USER_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
else
    USER_HOME="$HOME"
fi

MAJOR=16
RUN_DIR="$USER_HOME/.heyo/run"
STATE_FILE="$USER_HOME/.heyo/pg-vm-pool/registry.tsv"
DUMP_DIR="$USER_HOME/.heyo/pg-vm-pool/dumps"
WORK_DIR="/var/tmp"
PG_BIN=""
PG_USER="postgres"
IN_PLACE=0
ADOPT_UNBOUND=0
NO_REGISTRY=0
LIST_ONLY=0
YES=0
SCHEMAS=()

while [ $# -gt 0 ]; do
    case "$1" in
        --major)       MAJOR="$2"; shift 2 ;;
        --schema)      SCHEMAS+=("$2"); shift 2 ;;
        --run-dir)     RUN_DIR="$2"; shift 2 ;;
        --state)       STATE_FILE="$2"; shift 2 ;;
        --dump-dir)    DUMP_DIR="$2"; shift 2 ;;
        --work-dir)    WORK_DIR="$2"; shift 2 ;;
        --pg-bin)      PG_BIN="$2"; shift 2 ;;
        --pg-user)     PG_USER="$2"; shift 2 ;;
        --in-place)    IN_PLACE=1; shift ;;
        --adopt-unbound) ADOPT_UNBOUND=1; shift ;;
        --no-registry) NO_REGISTRY=1; shift ;;
        --list)        LIST_ONLY=1; shift ;;
        --yes)         YES=1; shift ;;
        -h|--help)     sed -n '2,56p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done
[ -n "$PG_BIN" ] || PG_BIN="/usr/lib/postgresql/$MAJOR/bin"

[ -f "$STATE_FILE" ] || die "registry not found: $STATE_FILE"
[ -d "$RUN_DIR" ] || die "run dir not found: $RUN_DIR"
command -v debugfs >/dev/null || die "missing required tool: debugfs"
if [ "$LIST_ONLY" != 1 ]; then
    [ "$(id -u)" = 0 ] || die "exhuming needs root (loop mounts); --list works unprivileged"
    for tool in "$PG_BIN/pg_ctl" "$PG_BIN/pg_dump" "$PG_BIN/pg_restore" "$PG_BIN/psql"; do
        [ -x "$tool" ] || die "missing $tool — install the postgresql-$MAJOR server package or pass --pg-bin"
    done
    id "$PG_USER" >/dev/null 2>&1 || die "host user '$PG_USER' does not exist (--pg-user)"
    [ -d "$DUMP_DIR" ] || [ -L "$DUMP_DIR" ] || die "dump dir $DUMP_DIR missing"
    mkdir -p "$WORK_DIR" 2>/dev/null
    # The postmaster runs as $PG_USER and must reach its socket/log under the
    # work dir; a 0700 home directory in the path breaks everything downstream
    # with confusing per-step errors, so fail it here with the real reason.
    sudo -u "$PG_USER" test -d "$WORK_DIR" 2>/dev/null \
        || die "work dir $WORK_DIR is not traversable by user '$PG_USER' \
(a 0700 \$HOME, e.g. ~/scratch, is the usual cause) — use /var/tmp or a spare-disk path"
    if [ "$NO_REGISTRY" != 1 ] && command -v supervisorctl >/dev/null \
        && supervisorctl status pg-vm-pool 2>/dev/null | grep -q RUNNING; then
        die "pooler is running — stop it first (this edits registry.tsv), or use --no-registry"
    fi
fi

schema_of_id() { awk -F'\t' -v id="$1" '$2==id {print $1; exit}' "$STATE_FILE"; }
tier_of() { awk -F'\t' -v s="$1" '$1==s {print ($4==""?"live":$4); exit}' "$STATE_FILE"; }
schema_known() { awk -F'\t' -v s="$1" '$1==s {f=1} END {exit !f}' "$STATE_FILE"; }
wanted() {
    [ ${#SCHEMAS[@]} -eq 0 ] && return 0
    local s
    for s in "${SCHEMAS[@]}"; do [ "$s" = "$1" ] && return 0; done
    return 1
}
# Is any process holding this file open? (same device:inode scan as
# reclaim-disks.sh, simplified to a single-file check via fuser fallback.)
disk_in_use() {
    local key
    key=$(stat -c '%d:%i' "$1" 2>/dev/null) || return 0
    find /proc/[0-9]*/fd -maxdepth 1 -type l 2>/dev/null \
        | xargs -r stat -L -c '%d:%i' 2>/dev/null | grep -qxF "$key"
}

# ---- discovery --------------------------------------------------------------
note "discovering clusters at major $MAJOR under $RUN_DIR"
CAND_SCHEMAS=(); CAND_IDS=(); CAND_DISKS=()
for dir in "$RUN_DIR"/sb-*/; do
    [ -f "$dir/data.ext4" ] || continue
    id=$(basename "$dir")
    ver=$(debugfs -R "cat /pgdata/PG_VERSION" "$dir/data.ext4" 2>/dev/null | tr -dc '0-9.')
    # A dirty journal (unclean VM kill) fails the normal open with "block
    # bitmap checksum does not match" — the bitmap update is journaled but
    # not replayed. Catastrophic mode (-c) opens without reading bitmaps and
    # still reads file contents; both modes are read-only. The exhume flow
    # replays the journal properly anyway (e2fsck -fp on the work copy).
    [ -z "$ver" ] && ver=$(debugfs -c -R "cat /pgdata/PG_VERSION" "$dir/data.ext4" 2>/dev/null | tr -dc '0-9.')
    [ "$ver" = "$MAJOR" ] || continue
    schema=$(schema_of_id "$id")
    tier=""
    [ -n "$schema" ] && tier=$(tier_of "$schema")
    if [ -z "$schema" ]; then
        if [ "$ADOPT_UNBOUND" = 1 ]; then
            echo "   $id: pgdata v$ver, unbound — CANDIDATE (owner identified from the cluster)"
            CAND_SCHEMAS+=(""); CAND_IDS+=("$id"); CAND_DISKS+=("$dir/data.ext4")
        else
            echo "   $id: pgdata v$ver but not bound to any schema — skipping (use --adopt-unbound)"
        fi
        continue
    fi
    if [ "$tier" != live ]; then
        echo "   $id: schema $schema already $tier — nothing to exhume"
        continue
    fi
    wanted "$schema" || continue
    echo "   $id: schema $schema, pgdata v$ver, tier live — CANDIDATE"
    CAND_SCHEMAS+=("$schema"); CAND_IDS+=("$id"); CAND_DISKS+=("$dir/data.ext4")
done
[ ${#CAND_SCHEMAS[@]} -gt 0 ] || { echo "   no candidates"; exit 0; }
echo "   ${#CAND_SCHEMAS[@]} candidate(s)"
[ "$LIST_ONLY" = 1 ] && exit 0

if [ "$YES" != 1 ]; then
    read -r -p "Exhume ${#CAND_SCHEMAS[@]} schema(s) with $PG_BIN? [y/N] " reply
    [ "$reply" = y ] || [ "$reply" = Y ] || die "aborted"
fi

# ---- per-schema exhumation --------------------------------------------------
PGPORT=5499   # private socket dir makes the port cosmetic; kept off 5432 anyway

# Cleanup for the CURRENT candidate; registered on EXIT so a failure mid-mount
# never strands a postmaster or a loop mount.
CUR_WORK=""; CUR_MNT=""; CUR_PGDATA=""
cleanup_current() {
    if [ -n "$CUR_PGDATA" ] && [ -d "$CUR_PGDATA" ]; then
        sudo -u "$PG_USER" "$PG_BIN/pg_ctl" -D "$CUR_PGDATA" -m fast stop >/dev/null 2>&1
    fi
    [ -n "$CUR_MNT" ] && mountpoint -q "$CUR_MNT" 2>/dev/null && umount "$CUR_MNT"
    [ -n "$CUR_WORK" ] && rm -rf "$CUR_WORK"
    CUR_WORK=""; CUR_MNT=""; CUR_PGDATA=""
}
trap cleanup_current EXIT

ok=0; failed=0; empty=0; recovered=0
for i in "${!CAND_SCHEMAS[@]}"; do
    schema="${CAND_SCHEMAS[$i]}"; id="${CAND_IDS[$i]}"; disk="${CAND_DISKS[$i]}"
    note "exhuming ${schema:+schema $schema }${schema:-unbound disk (owner TBD) }($id)"
    if disk_in_use "$disk"; then
        echo "   disk is held open by a running process — skipping"
        failed=$((failed + 1)); continue
    fi

    CUR_WORK="$WORK_DIR/exhume-$id"
    rm -rf "$CUR_WORK"; mkdir -p "$CUR_WORK"
    CUR_MNT="$CUR_WORK/mnt"; mkdir -p "$CUR_MNT"

    work_disk="$disk"
    if [ "$IN_PLACE" != 1 ]; then
        need=$(du -B1 "$disk" | cut -f1)
        avail=$(df -B1 --output=avail "$WORK_DIR" 2>/dev/null | awk 'NR==2 {print $1}')
        if [ -n "$avail" ] && [ "$avail" -lt $((need + need / 10)) ]; then
            echo "   $WORK_DIR lacks room for a $(du -h "$disk" | cut -f1) copy — skipping"
            cleanup_current; failed=$((failed + 1)); continue
        fi
        echo "   copying disk ($(du -h "$disk" | cut -f1) allocated)…"
        if ! cp --sparse=always "$disk" "$CUR_WORK/data.ext4"; then
            echo "   copy failed — skipping"; cleanup_current; failed=$((failed + 1)); continue
        fi
        work_disk="$CUR_WORK/data.ext4"
    fi

    fsck_out=$(e2fsck -fp "$work_disk" 2>&1)
    fsck_rc=$?
    if [ "$fsck_rc" -ge 4 ]; then
        echo "   preen fsck failed (rc=$fsck_rc):"
        echo "$fsck_out" | grep -v '^$' | head -n 3 | sed 's/^/     /'
        if [ "$IN_PLACE" = 1 ]; then
            echo "   refusing a full repair on the ORIGINAL — drop --in-place or run badfs first; skipping"
            cleanup_current; failed=$((failed + 1)); continue
        fi
        # On the work COPY a full -fy repair is risk-free (the original is
        # untouched); it's also the only way through ENOSPC-era damage.
        echo "   running full repair on the work copy…"
        fsck_out=$(e2fsck -fy "$work_disk" 2>&1)
        fsck_rc=$?
        if [ "$fsck_rc" -ge 4 ]; then
            echo "   full repair failed too (rc=$fsck_rc):"
            echo "$fsck_out" | grep -v '^$' | tail -n 3 | sed 's/^/     /'
            cleanup_current; failed=$((failed + 1)); continue
        fi
        echo "   repaired"
    fi
    if ! mount -o loop "$work_disk" "$CUR_MNT"; then
        echo "   loop mount failed — skipping"; cleanup_current; failed=$((failed + 1)); continue
    fi
    CUR_PGDATA="$CUR_MNT/pgdata"
    if [ "$(cat "$CUR_PGDATA/PG_VERSION" 2>/dev/null)" != "$MAJOR" ]; then
        echo "   mounted pgdata is not v$MAJOR?! — skipping"
        cleanup_current; failed=$((failed + 1)); continue
    fi

    # Host-side adaptations, all on the copy: ownership for the host postgres
    # user; the guest's tmpfs temp-dir symlink dangles here — make it a dir;
    # stale postmaster remnants from the unclean guest kill.
    chown -R "$PG_USER:$PG_USER" "$CUR_PGDATA"
    if [ -L "$CUR_PGDATA/base/pgsql_tmp" ]; then
        # Root does the mkdir (no traversal dependence), then hands it over.
        rm -f "$CUR_PGDATA/base/pgsql_tmp"
        mkdir -p "$CUR_PGDATA/base/pgsql_tmp"
        chown "$PG_USER:$PG_USER" "$CUR_PGDATA/base/pgsql_tmp"
    fi
    rm -f "$CUR_PGDATA/postmaster.pid"
    sock="$CUR_WORK/sock"
    if ! mkdir -p "$sock" || ! chown "$PG_USER:$PG_USER" "$sock" "$CUR_WORK"; then
        echo "   could not prepare $CUR_WORK for user $PG_USER — skipping"
        cleanup_current; failed=$((failed + 1)); continue
    fi

    echo "   starting v$MAJOR postmaster (crash recovery runs now)…"
    start_out=$(sudo -u "$PG_USER" "$PG_BIN/pg_ctl" -D "$CUR_PGDATA" -w -t 180 \
        -l "$CUR_WORK/pg.log" \
        -o "-c listen_addresses='' -c unix_socket_directories='$sock' -p $PGPORT -c archive_mode=off" \
        start 2>&1)
    if [ $? -ne 0 ]; then
        echo "   postmaster failed to start:"
        echo "$start_out" | grep -v '^$' | tail -n 3 | sed 's/^/     /'
        tail -n 8 "$CUR_WORK/pg.log" 2>/dev/null | sed 's/^/     /'
        cleanup_current; failed=$((failed + 1)); continue
    fi

    dbs=$(sudo -u "$PG_USER" "$PG_BIN/psql" -h "$sock" -p "$PGPORT" -d postgres -Atc \
        "SELECT datname FROM pg_database WHERE NOT datistemplate AND datname <> 'postgres'" 2>/dev/null)

    # Decide what to dump and how to record it. actions: flip (tier -> frozen),
    # add (append a new frozen registry row), none (dump only — human decides).
    PLAN_DB=(); PLAN_DEST=(); PLAN_ACT=()
    if [ -n "$schema" ]; then
        if ! grep -qxF "$schema" <<<"$dbs"; then
            echo "   cluster's databases [$(paste -sd, - <<<"$dbs")] do not include '$schema' — refusing to guess; skipping"
            cleanup_current; failed=$((failed + 1)); continue
        fi
        PLAN_DB+=("$schema"); PLAN_DEST+=("$DUMP_DIR/$schema.dump"); PLAN_ACT+=(flip)
    else
        # Unbound disk (--adopt-unbound): the database names ARE the owners —
        # the pooler creates each schema's database under the schema's name.
        if [ -z "$dbs" ]; then
            echo "   cluster is EMPTY (initdb only — an old spare); nothing to recover, disk deletable"
            cleanup_current; empty=$((empty + 1)); continue
        fi
        ndb=$(wc -l <<<"$dbs")
        while IFS= read -r db; do
            case "$db" in
                */*|*[[:space:]]*|.*)
                    echo "   owner db '$db' has an unsafe name — skipping it"
                    continue ;;
            esac
            if schema_known "$db"; then
                # The registry binds this schema elsewhere. That binding may be
                # a fresh EMPTY VM created mid-incident, or a legitimately newer
                # copy — a script cannot know which. Preserve, don't decide.
                echo "   owner identified: '$db' — but the registry already binds it (tier $(tier_of "$db"));"
                echo "     saving as a .recovered dump, registry untouched — compare the two by hand"
                PLAN_DB+=("$db"); PLAN_DEST+=("$DUMP_DIR/$db.recovered-from-$id.dump"); PLAN_ACT+=(none)
            elif [ "$ndb" -gt 1 ]; then
                # A multi-database cluster is not something the pooler builds;
                # recover the bytes but let a human sort out ownership.
                echo "   owner db '$db' (one of $ndb — anomalous): saving as .recovered, registry untouched"
                PLAN_DB+=("$db"); PLAN_DEST+=("$DUMP_DIR/$db.recovered-from-$id.dump"); PLAN_ACT+=(none)
            else
                echo "   owner identified: '$db' — unknown to the registry; will adopt as frozen"
                PLAN_DB+=("$db"); PLAN_DEST+=("$DUMP_DIR/$db.dump"); PLAN_ACT+=(add)
            fi
        done <<<"$dbs"
        if [ ${#PLAN_DB[@]} -eq 0 ]; then
            cleanup_current; failed=$((failed + 1)); continue
        fi
    fi

    this_ok=1
    for j in "${!PLAN_DB[@]}"; do
        db="${PLAN_DB[$j]}"; dest="${PLAN_DEST[$j]}"; act="${PLAN_ACT[$j]}"
        echo "   dumping database $db…"
        if ! sudo -u "$PG_USER" "$PG_BIN/pg_dump" -h "$sock" -p "$PGPORT" -Fc \
            -d "$db" -f "$CUR_WORK/out.dump"; then
            echo "   pg_dump of $db failed; log tail:"
            tail -n 8 "$CUR_WORK/pg.log" 2>/dev/null | sed 's/^/     /'
            this_ok=0; continue
        fi
        sz=$(stat -c %s "$CUR_WORK/out.dump" 2>/dev/null || echo 0)
        if [ "$sz" -lt 512 ] || ! "$PG_BIN/pg_restore" --list "$CUR_WORK/out.dump" >/dev/null 2>&1; then
            echo "   dump of $db failed verification (${sz}B)"
            this_ok=0; continue
        fi
        mv "$CUR_WORK/out.dump" "$dest"
        chown --reference="$STATE_FILE" "$dest" 2>/dev/null || true
        echo "   dump ok ($(numfmt --to=iec --suffix=B "$sz" 2>/dev/null || echo "${sz}B")) -> $dest"

        if [ "$NO_REGISTRY" != 1 ]; then
            case "$act" in
                flip)
                    cp -p "$STATE_FILE" "$STATE_FILE.bak-exhume" 2>/dev/null || true
                    awk -F'\t' -v OFS='\t' -v s="$db" '$1==s {$4="frozen"} {print}' \
                        "$STATE_FILE" >"$STATE_FILE.tmp" \
                        && chown --reference="$STATE_FILE" "$STATE_FILE.tmp" 2>/dev/null \
                        ; mv "$STATE_FILE.tmp" "$STATE_FILE"
                    echo "   schema $db -> frozen (registry backup: $STATE_FILE.bak-exhume)"
                    ;;
                add)
                    cp -p "$STATE_FILE" "$STATE_FILE.bak-exhume" 2>/dev/null || true
                    printf '%s\t%s\t%s\tfrozen\n' "$db" "$id" "$(date +%s)" >>"$STATE_FILE"
                    echo "   schema $db ADDED to the registry as frozen (restores on next connect)"
                    ;;
                none) recovered=$((recovered + 1)) ;;
            esac
        fi
    done

    cleanup_current
    if [ "$this_ok" = 1 ]; then ok=$((ok + 1)); else failed=$((failed + 1)); fi
done

note "done: $ok exhumed, $empty empty (old spares), $recovered saved as .recovered (conflicts — compare by hand), $failed failed/skipped"
if [ "$ok" -gt 0 ]; then
    cat <<EOF
Next steps:
  - start the pooler; each exhumed schema restores from its dump (onto the
    current image) on its next client connection
  - the old v$MAJOR VMs are now leftovers of frozen schemas: the dashboard's
    "purge waste VMs" button (or emergency-drain.sh purge) deletes them
EOF
fi
