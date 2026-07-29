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
#   --work-dir DIR     scratch for disk copies + sockets  (default: /tmp; put it
#                      on the spare disk if /tmp is tight — each copy is the
#                      disk's allocated size)
#   --pg-bin DIR       old-major binaries                 (default: /usr/lib/postgresql/<major>/bin;
#                      `apt install postgresql-16` provides them)
#   --pg-user U        host user to run the postmaster as (default: postgres)
#   --in-place         operate on the original disk (skips the copy; crash
#                      recovery then writes to the only copy — default is safer)
#   --no-registry      dump only; skip the tier flip
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
WORK_DIR="/tmp"
PG_BIN=""
PG_USER="postgres"
IN_PLACE=0
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
    if [ "$NO_REGISTRY" != 1 ] && command -v supervisorctl >/dev/null \
        && supervisorctl status pg-vm-pool 2>/dev/null | grep -q RUNNING; then
        die "pooler is running — stop it first (this edits registry.tsv), or use --no-registry"
    fi
fi

schema_of_id() { awk -F'\t' -v id="$1" '$2==id {print $1; exit}' "$STATE_FILE"; }
tier_of() { awk -F'\t' -v s="$1" '$1==s {print ($4==""?"live":$4); exit}' "$STATE_FILE"; }
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
        echo "   $id: pgdata v$ver but not bound to any schema — skipping (unknown owner)"
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

ok=0; failed=0
for i in "${!CAND_SCHEMAS[@]}"; do
    schema="${CAND_SCHEMAS[$i]}"; id="${CAND_IDS[$i]}"; disk="${CAND_DISKS[$i]}"
    note "exhuming schema $schema ($id)"
    if disk_in_use "$disk"; then
        echo "   disk is held open by a running process — skipping"
        failed=$((failed + 1)); continue
    fi

    CUR_WORK="$WORK_DIR/exhume-$id"
    rm -rf "$CUR_WORK"; mkdir -p "$CUR_WORK"
    CUR_MNT="$CUR_WORK/mnt"; mkdir -p "$CUR_MNT"

    work_disk="$disk"
    if [ "$IN_PLACE" != 1 ]; then
        echo "   copying disk ($(du -h "$disk" | cut -f1) allocated)…"
        if ! cp --sparse=always "$disk" "$CUR_WORK/data.ext4"; then
            echo "   copy failed — skipping"; cleanup_current; failed=$((failed + 1)); continue
        fi
        work_disk="$CUR_WORK/data.ext4"
    fi

    e2fsck -fp "$work_disk" >/dev/null 2>&1
    if [ $? -ge 4 ]; then
        echo "   e2fsck failed on the work copy — repair the disk first (badfs); skipping"
        cleanup_current; failed=$((failed + 1)); continue
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
        rm -f "$CUR_PGDATA/base/pgsql_tmp"
        sudo -u "$PG_USER" mkdir -p "$CUR_PGDATA/base/pgsql_tmp"
    fi
    rm -f "$CUR_PGDATA/postmaster.pid"
    sock="$CUR_WORK/sock"; mkdir -p "$sock"; chown "$PG_USER:$PG_USER" "$sock" "$CUR_WORK"

    echo "   starting v$MAJOR postmaster (crash recovery runs now)…"
    if ! sudo -u "$PG_USER" "$PG_BIN/pg_ctl" -D "$CUR_PGDATA" -w -t 180 \
        -l "$CUR_WORK/pg.log" \
        -o "-c listen_addresses='' -c unix_socket_directories='$sock' -p $PGPORT -c archive_mode=off" \
        start >/dev/null 2>&1; then
        echo "   postmaster failed to start; log tail:"
        tail -n 8 "$CUR_WORK/pg.log" 2>/dev/null | sed 's/^/     /'
        cleanup_current; failed=$((failed + 1)); continue
    fi

    dbs=$(sudo -u "$PG_USER" "$PG_BIN/psql" -h "$sock" -p "$PGPORT" -d postgres -Atc \
        "SELECT datname FROM pg_database WHERE NOT datistemplate AND datname <> 'postgres'" 2>/dev/null)
    if ! grep -qxF "$schema" <<<"$dbs"; then
        echo "   cluster's databases [$(paste -sd, - <<<"$dbs")] do not include '$schema' — refusing to guess; skipping"
        cleanup_current; failed=$((failed + 1)); continue
    fi

    echo "   dumping database $schema…"
    if ! sudo -u "$PG_USER" "$PG_BIN/pg_dump" -h "$sock" -p "$PGPORT" -Fc \
        -d "$schema" -f "$CUR_WORK/out.dump"; then
        echo "   pg_dump failed; log tail:"
        tail -n 8 "$CUR_WORK/pg.log" 2>/dev/null | sed 's/^/     /'
        cleanup_current; failed=$((failed + 1)); continue
    fi
    sz=$(stat -c %s "$CUR_WORK/out.dump" 2>/dev/null || echo 0)
    if [ "$sz" -lt 512 ] || ! "$PG_BIN/pg_restore" --list "$CUR_WORK/out.dump" >/dev/null 2>&1; then
        echo "   dump failed verification (${sz}B) — skipping"
        cleanup_current; failed=$((failed + 1)); continue
    fi

    out="$DUMP_DIR/$schema.dump"
    mv "$CUR_WORK/out.dump" "$out"
    chown --reference="$STATE_FILE" "$out" 2>/dev/null || true
    echo "   dump ok ($(numfmt --to=iec --suffix=B "$sz" 2>/dev/null || echo "${sz}B")) -> $out"

    if [ "$NO_REGISTRY" != 1 ]; then
        cp -p "$STATE_FILE" "$STATE_FILE.bak-exhume" 2>/dev/null || true
        awk -F'\t' -v OFS='\t' -v s="$schema" '$1==s {$4="frozen"} {print}' \
            "$STATE_FILE" >"$STATE_FILE.tmp" \
            && chown --reference="$STATE_FILE" "$STATE_FILE.tmp" 2>/dev/null \
            ; mv "$STATE_FILE.tmp" "$STATE_FILE"
        echo "   schema $schema -> frozen (registry backup: $STATE_FILE.bak-exhume)"
    fi

    cleanup_current
    ok=$((ok + 1))
done

note "done: $ok exhumed, $failed failed/skipped"
if [ "$ok" -gt 0 ]; then
    cat <<EOF
Next steps:
  - start the pooler; each exhumed schema restores from its dump (onto the
    current image) on its next client connection
  - the old v$MAJOR VMs are now leftovers of frozen schemas: the dashboard's
    "purge waste VMs" button (or emergency-drain.sh purge) deletes them
EOF
fi
