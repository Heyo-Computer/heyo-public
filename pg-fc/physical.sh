#!/bin/sh
# Bounded, crash-safe physical standby seeding inside a pg-fc guest.
set -eu

for pgbin in /usr/lib/postgresql/*/bin; do
    [ ! -d "$pgbin" ] || PATH="$pgbin:$PATH"
done
export PATH LC_ALL=C
umask 077
WORKSPACE=${PG_FC_WORKSPACE:-/workspace}
PGDATA=${PG_FC_PGDATA:-$WORKSPACE/pgdata}
ROOT_MARKER=${PG_FC_ROOT_MARKER:-/etc/pg-fc-physical-persistent-required}
STATE_DIR="$WORKSPACE/pg-fc-physical"
PLAN="$STATE_DIR/plan.json"
ACTIVE="$STATE_DIR/activated.json"
STATUS="$STATE_DIR/status.json"
LOCK="$STATE_DIR/seed.lock"
PROMOTION="$STATE_DIR/promotion.json"
COMMAND=${1:-}

say_status() {
    phase=$1 error=${2:-}
    mkdir -p "$STATE_DIR"
    tmp="$STATE_DIR/.status.$$"
    jq -nc --arg phase "$phase" --arg error "$error" \
        '{phase:$phase,error:(if $error == "" then null else $error end)}' >"$tmp"
    chmod 600 "$tmp"; chown postgres:postgres "$tmp" 2>/dev/null || true
    mv -f "$tmp" "$STATUS"
}

fail() {
    say_status failed "$1"; echo "pg-fc-physical: $1" >&2
    [ "$COMMAND" != boot-check ] || exit 20
    exit "${2:-1}"
}

persistent_workspace() {
    [ "${PG_FC_TEST_ALLOW_NONMOUNT:-0}" = 1 ] || mountpoint -q "$WORKSPACE"
}

load_plan() {
    [ -f "$PLAN" ] || return 1
    [ "${PG_FC_TEST_ALLOW_PLAN_OWNER:-0}" = 1 ] || [ "$(stat -c %U "$PLAN")" = postgres ] || fail "plan owner must be postgres"
    [ "${PG_FC_TEST_ALLOW_PLAN_OWNER:-0}" = 1 ] || [ "$(stat -c %a "$PLAN")" = 600 ] || fail "plan mode must be 0600"
    jq -e '. as $plan |
      ($plan|type == "object") and
      ($plan.generation|type=="string" and test("^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")) and
      ($plan.system_identifier|type=="string" and test("^[0-9]+$")) and
      ($plan.pg_major|type=="number" and floor==. and .>=1) and
      ($plan.conninfo|type=="string" and length>0) and
      ($plan.slot|type=="string" and test("^[a-z0-9_]{1,63}$")) and
      ($plan.settings|type=="object") and
      (["max_connections","max_prepared_transactions","max_locks_per_transaction","max_wal_senders","max_worker_processes"] |
       all(. as $k | ($plan.settings[$k]|type=="number" and floor==. and .>=0)))' "$PLAN" >/dev/null \
      || fail "invalid plan"
    generation=$(jq -r .generation "$PLAN")
    system_identifier=$(jq -r .system_identifier "$PLAN")
    pg_major=$(jq -r .pg_major "$PLAN")
    slot=$(jq -r .slot "$PLAN")
}

installed_major() {
    postgres --version | sed -n 's/.* \([0-9][0-9]*\)\..*/\1/p'
}

cluster_system_id() {
    pg_controldata "$1" 2>/dev/null | awk -F: '/Database system identifier/ {gsub(/[[:space:]]/,"",$2); print $2}'
}

verify_cluster() {
    dir=$1
    [ "$(cat "$dir/PG_VERSION" 2>/dev/null)" = "$pg_major" ] || return 1
    [ "$(installed_major)" = "$pg_major" ] || return 1
    [ "$(cluster_system_id "$dir")" = "$system_identifier" ] || return 1
    [ -f "$dir/standby.signal" ] || return 1
    grep -Eq "^[[:space:]]*primary_slot_name[[:space:]]*=[[:space:]]*'${slot}'" "$dir/postgresql.auto.conf" || return 1
    grep -Eq "^[[:space:]]*primary_conninfo[[:space:]]*=" "$dir/postgresql.auto.conf" || return 1
}

verify_identity() {
    dir=$1
    [ "$(cat "$dir/PG_VERSION" 2>/dev/null)" = "$pg_major" ] || return 1
    [ "$(installed_major)" = "$pg_major" ] || return 1
    [ "$(cluster_system_id "$dir")" = "$system_identifier" ] || return 1
}

load_promotion() {
    [ -f "$PROMOTION" ] || return 1
    jq -e '. as $p | ($p|type=="object") and
      (($p|keys|sort)==["barrier_lsn","generation","pg_major","phase","system_identifier","tenant_database"]) and
      ($p.generation|type=="string") and ($p.system_identifier|type=="string") and
      ($p.pg_major|type=="number" and floor==.) and
      ($p.tenant_database|type=="string" and length>0 and length<=63 and (test("[[:cntrl:]]")|not)) and
      ($p.barrier_lsn|type=="string" and test("^[0-9A-F]+/[0-9A-F]+$")) and
      (["prepared","promoting","promoted-but-fenced"]|index($p.phase)!=null)' "$PROMOTION" >/dev/null || fail "invalid promotion record"
    promotion_generation=$(jq -r .generation "$PROMOTION")
    promotion_system_identifier=$(jq -r .system_identifier "$PROMOTION")
    promotion_pg_major=$(jq -r .pg_major "$PROMOTION")
    promotion_database=$(jq -r .tenant_database "$PROMOTION")
    promotion_barrier=$(jq -r .barrier_lsn "$PROMOTION")
    promotion_phase=$(jq -r .phase "$PROMOTION")
    [ "$promotion_generation" = "$generation" ] &&
      [ "$promotion_system_identifier" = "$system_identifier" ] &&
      [ "$promotion_pg_major" = "$pg_major" ] || fail "promotion record identity does not match seed plan"
}

write_promotion() {
    phase=$1 database=$2 barrier=$3
    tmp="$STATE_DIR/.promotion.$$"
    jq -nc --arg generation "$generation" --arg system_identifier "$system_identifier" \
      --argjson pg_major "$pg_major" --arg tenant_database "$database" --arg barrier_lsn "$barrier" --arg phase "$phase" \
      '{generation:$generation,system_identifier:$system_identifier,pg_major:$pg_major,tenant_database:$tenant_database,barrier_lsn:$barrier_lsn,phase:$phase}' >"$tmp"
    chmod 600 "$tmp"; chown postgres:postgres "$tmp" 2>/dev/null || true
    mv -f "$tmp" "$PROMOTION"; sync
}

psql_local() {
    gosu postgres psql -X -v ON_ERROR_STOP=1 -U postgres -d template1 "$@"
}

verify_fence() {
    database=$1
    result=$(psql_local -Atq -v database="$database" <<'SQL'
SELECT count(*) = 1 AND bool_and(NOT datallowconn)
FROM pg_database WHERE datname = :'database';
SQL
    ) || return 1
    [ "$result" = t ]
}

verify_replay_barrier() {
    barrier=$1
    result=$(psql_local -Atq -v barrier="$barrier" <<'SQL'
SELECT pg_is_in_recovery()
   AND pg_last_wal_replay_lsn() IS NOT NULL
   AND pg_wal_lsn_diff(pg_last_wal_replay_lsn(), :'barrier'::pg_lsn) >= 0;
SQL
    ) || return 1
    [ "$result" = t ]
}

validate_promotion_env() {
    : "${PG_FC_GENERATION:?PG_FC_GENERATION is required}"
    : "${PG_FC_SYSTEM_IDENTIFIER:?PG_FC_SYSTEM_IDENTIFIER is required}"
    : "${PG_FC_PG_MAJOR:?PG_FC_PG_MAJOR is required}"
    : "${PG_FC_TENANT_DATABASE:?PG_FC_TENANT_DATABASE is required}"
    : "${PG_FC_BARRIER_LSN:?PG_FC_BARRIER_LSN is required}"
    [ "$PG_FC_GENERATION" = "$generation" ] || fail "expected generation does not match seed plan"
    [ "$PG_FC_SYSTEM_IDENTIFIER" = "$system_identifier" ] || fail "expected system identifier does not match seed plan"
    [ "$PG_FC_PG_MAJOR" = "$pg_major" ] || fail "expected PostgreSQL major does not match seed plan"
    printf '%s' "$PG_FC_TENANT_DATABASE" | grep -Eq '^[^[:cntrl:]]{1,63}$' || fail "invalid tenant database"
    printf '%s' "$PG_FC_BARRIER_LSN" | grep -Eq '^[0-9A-F]+/[0-9A-F]+$' || fail "invalid WAL barrier"
    verify_identity "$PGDATA" || fail "cluster identity does not match seed plan"
}

write_physical_conf() {
    dir=$1
    conf="$dir/pg-fc-physical.conf"
    {
        echo "# Generated by pg-fc-physical; source recovery minima."
        echo "wal_level = replica"
        echo "hot_standby = on"
        for key in max_connections max_prepared_transactions max_locks_per_transaction max_wal_senders max_worker_processes; do
            printf '%s = %s\n' "$key" "$(jq -r ".settings.$key" "$PLAN")"
        done
    } >"$conf.tmp"
    chown postgres:postgres "$conf.tmp" 2>/dev/null || true; chmod 600 "$conf.tmp"
    mv -f "$conf.tmp" "$conf"
    grep -q "^include = 'pg-fc-physical.conf'" "$dir/postgresql.conf" 2>/dev/null || \
        echo "include = 'pg-fc-physical.conf'" >>"$dir/postgresql.conf"
    # Source ALTER SYSTEM settings otherwise override recovery minima.
    grep -q "^include = 'pg-fc-physical.conf'" "$dir/postgresql.auto.conf" || \
        echo "include = 'pg-fc-physical.conf'" >>"$dir/postgresql.auto.conf"
}

mark_active() {
    tmp="$STATE_DIR/.activated.$$"
    jq -nc --arg generation "$generation" --arg system_identifier "$system_identifier" \
      '{generation:$generation,system_identifier:$system_identifier,verified:true}' >"$tmp"
    chmod 600 "$tmp"; chown postgres:postgres "$tmp" 2>/dev/null || true
    mv -f "$tmp" "$ACTIVE"; sync
}

is_active() {
    [ -f "$ACTIVE" ] &&
      [ "$(jq -r '.verified // false' "$ACTIVE" 2>/dev/null)" = true ] &&
      [ "$(jq -r '.generation // ""' "$ACTIVE" 2>/dev/null)" = "$generation" ] &&
      [ "$(jq -r '.system_identifier // ""' "$ACTIVE" 2>/dev/null)" = "$system_identifier" ] &&
      verify_cluster "$PGDATA"
}

boot_check() {
    if [ ! -f "$PLAN" ]; then
        if [ -e "$ROOT_MARKER" ]; then
            fail "physical marker exists without its durable plan" 20
        fi
        echo normal; exit 0
    fi
    persistent_workspace || fail "physical plan requires persistent workspace" 20
    load_plan
    if [ -e "$PROMOTION" ]; then
        load_promotion
        verify_identity "$PGDATA" || fail "promoting cluster identity changed" 20
        case "$promotion_phase" in
            prepared) verify_cluster "$PGDATA" || fail "prepared standby recovery configuration changed" 20 ;;
            promoting)
                [ ! -e "$PGDATA/standby.signal" ] || verify_cluster "$PGDATA" || fail "promoting standby recovery configuration changed" 20
                ;;
            promoted-but-fenced) [ ! -e "$PGDATA/standby.signal" ] || fail "promoted cluster has a standby signal" 20 ;;
        esac
        say_status "$promotion_phase"; echo "$promotion_phase"; exit 10
    fi
    if is_active; then say_status active; echo active; exit 10; fi
    say_status inhibited "physical activation is not verified"
    echo inhibited; exit 20
}

seed() {
    persistent_workspace || fail "physical seed requires persistent workspace"
    load_plan
    : >"$ROOT_MARKER" 2>/dev/null || fail "cannot install persistent-required marker"
    chmod 600 "$ROOT_MARKER" 2>/dev/null || true
    exec 9>"$LOCK"
    if ! flock -n 9; then echo "seed already running" >&2; exit 75; fi
    [ ! -e "$PROMOTION" ] || fail "promotion has started; refusing to reseed"
    # Unexpected helper failures must not leave a dead worker reporting
    # "copying" forever. Do not overwrite a more specific fail() message.
    trap 'rc=$?; if [ "$rc" -ne 0 ] && [ "$(jq -r .phase "$STATUS" 2>/dev/null)" != failed ]; then say_status failed "seed exited unexpectedly (exit $rc); see controller.log"; fi' 0
    sync
    if is_active; then
        if ! gosu postgres pg_ctl -D "$PGDATA" status >/dev/null 2>&1; then
            gosu postgres pg_ctl -D "$PGDATA" -w start 9>&- >"$WORKSPACE/pg-startup.log" 2>&1 || fail "activated standby failed to restart"
        fi
        say_status active; exit 0
    fi
    [ ! -e "$ACTIVE" ] || fail "activated standby identity changed; refusing to reseed its data"

    # A crash after the activation rename is completed by verifying, never by initdb.
    if verify_cluster "$PGDATA"; then
        write_physical_conf "$PGDATA"; mark_active
    else
        scratch="$STATE_DIR/scratch.$generation"
        # A kill between the two activation renames leaves canonical absent.
        # Restore the quarantined disposable cluster before retrying the copy.
        if [ ! -e "$PGDATA" ] && [ -d "$scratch" ]; then mv "$scratch" "$PGDATA"; fi
        if [ -s "$PGDATA/postmaster.pid" ]; then
            say_status stopping_scratch
            if ! gosu postgres pg_ctl -D "$PGDATA" -m fast -w stop >/dev/null 2>&1; then
                gosu postgres pg_ctl -D "$PGDATA" status >/dev/null 2>&1 && fail "scratch postgres did not stop"
                rm -f "$PGDATA/postmaster.pid"
            fi
        fi
        [ ! -s "$PGDATA/postmaster.pid" ] || fail "scratch postgres still appears running"
        stage="$STATE_DIR/staging.$generation"
        rm -rf "$stage"
        mkdir -p "$stage"; chown postgres:postgres "$stage"; chmod 700 "$stage"
        say_status copying
        conninfo=$(jq -r .conninfo "$PLAN")
        export PG_FC_CONNINFO="$conninfo"
        unset conninfo
        service_file="$STATE_DIR/seed.service"
        python3 - "$service_file.tmp" <<'PY'
import ctypes, ctypes.util, os, sys
c = os.environ["PG_FC_CONNINFO"]
if "\n" in c or "\r" in c:
    raise SystemExit("newline in conninfo")
class Option(ctypes.Structure):
    _fields_ = [(k, ctypes.c_char_p) for k in ("keyword", "envvar", "compiled", "val", "label", "dispchar")] + [("dispsize", ctypes.c_int)]
pq = ctypes.CDLL(ctypes.util.find_library("pq"))
pq.PQconninfoParse.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_char_p)]
pq.PQconninfoParse.restype = ctypes.POINTER(Option)
pq.PQconninfoFree.argtypes = [ctypes.POINTER(Option)]
error = ctypes.c_char_p()
options = pq.PQconninfoParse(c.encode(), ctypes.byref(error))
if not options: raise SystemExit("invalid libpq conninfo")
pairs = []
i = 0
while options[i].keyword:
    if options[i].val is not None: pairs.append((options[i].keyword.decode(), options[i].val.decode()))
    i += 1
pq.PQconninfoFree(options)
with open(sys.argv[1], "w") as f:
    f.write("[physical_seed]\n")
    for key, value in pairs:
        if not key.replace("_", "").isalnum() or "\n" in value: raise SystemExit("invalid conninfo key/value")
        f.write(f"{key}={value}\n")
PY
        chown postgres:postgres "$service_file.tmp"; chmod 600 "$service_file.tmp"; mv "$service_file.tmp" "$service_file"
        export PGSERVICEFILE="$service_file"
        gosu postgres pg_basebackup -d service=physical_seed -D "$stage" -R -X stream -S "$slot" --no-password --checkpoint=fast \
          >"$STATE_DIR/basebackup.log" 2>&1 || fail "base backup failed (see basebackup.log)"
        # The seeding service path is not inherited by postgres on a later
        # boot. Persist the original conninfo in auto.conf (0600 PGDATA), with
        # PostgreSQL configuration-string escaping, never on argv or stdout.
        python3 - "$stage/postgresql.auto.conf" <<'PY'
import os, sys
c = os.environ["PG_FC_CONNINFO"].replace("\\", "\\\\").replace("'", "''")
with open(sys.argv[1], "a") as f: f.write("primary_conninfo = '" + c + "'\n")
PY
        unset PG_FC_CONNINFO PGSERVICEFILE
        say_status verifying
        gosu postgres pg_verifybackup "$stage" >"$STATE_DIR/verifybackup.log" 2>&1 || fail "backup manifest verification failed"
        verify_cluster "$stage" || fail "seed identity, version, or recovery configuration mismatch"
        write_physical_conf "$stage"
        sync
        [ "${PG_FC_TEST_FAIL_AT:-}" != after-copy ] || fail "injected failure after copy"
        [ ! -e "$scratch" ] || fail "scratch quarantine already exists"
        mv "$PGDATA" "$scratch" || fail "could not quarantine scratch PGDATA"
        [ "${PG_FC_TEST_FAIL_AT:-}" != after-quarantine ] || fail "injected failure after quarantine"
        if ! mv "$stage" "$PGDATA"; then mv "$scratch" "$PGDATA" 2>/dev/null || true; fail "could not activate standby"; fi
        sync
        verify_cluster "$PGDATA" || fail "activated standby did not verify"
        mark_active
        rm -rf "$scratch"
    fi
    say_status starting
    mkdir -p /var/run/postgresql; chown postgres:postgres /var/run/postgresql
    # postmaster inherits otherwise-open descriptors. Do not let it retain the
    # operation lock for the lifetime of the guest. The parent keeps the lock
    # until startup and its status update finish.
    gosu postgres pg_ctl -D "$PGDATA" -w start 9>&- >"$WORKSPACE/pg-startup.log" 2>&1 || fail "activated standby failed to start"
    say_status active
}

prepare_promotion() {
    persistent_workspace || fail "physical promotion requires persistent workspace"
    load_plan
    exec 9>"$LOCK"; flock -n 9 || { echo "physical operation already running" >&2; exit 75; }
    validate_promotion_env
    if [ -e "$PROMOTION" ]; then
        load_promotion
        [ "$promotion_database" = "$PG_FC_TENANT_DATABASE" ] && [ "$promotion_barrier" = "$PG_FC_BARRIER_LSN" ] || fail "promotion request does not match durable intent"
        if [ "$promotion_phase" = prepared ]; then
            verify_fence "$promotion_database" || fail "tenant database does not exist or still allows connections"
            verify_replay_barrier "$promotion_barrier" || fail "standby has not replayed the required WAL barrier"
        fi
        say_status "$promotion_phase"; exit 0
    fi
    is_active || fail "cluster is not an activated physical standby"
    verify_fence "$PG_FC_TENANT_DATABASE" || fail "tenant database does not exist or still allows connections"
    verify_replay_barrier "$PG_FC_BARRIER_LSN" || fail "standby has not replayed the required WAL barrier"
    write_promotion prepared "$PG_FC_TENANT_DATABASE" "$PG_FC_BARRIER_LSN"
    say_status prepared
}

promote() {
    persistent_workspace || fail "physical promotion requires persistent workspace"
    load_plan
    exec 9>"$LOCK"; flock -n 9 || { echo "physical operation already running" >&2; exit 75; }
    validate_promotion_env
    load_promotion || fail "promotion has not been prepared"
    [ "$promotion_database" = "$PG_FC_TENANT_DATABASE" ] && [ "$promotion_barrier" = "$PG_FC_BARRIER_LSN" ] || fail "promotion request does not match durable intent"
    if [ "$promotion_phase" = prepared ]; then
        verify_fence "$promotion_database" || fail "tenant database does not exist or still allows connections"
        verify_replay_barrier "$promotion_barrier" || fail "standby has not replayed the required WAL barrier"
        write_promotion promoting "$promotion_database" "$promotion_barrier"
        promotion_phase=promoting
    fi
    recovering=$(psql_local -Atqc 'SELECT pg_is_in_recovery()') || fail "cannot inspect promotion state"
    if [ "$recovering" = t ]; then
        [ "$promotion_phase" != promoted-but-fenced ] || fail "promoted cluster unexpectedly returned to recovery"
        verify_fence "$promotion_database" || fail "tenant database does not exist or still allows connections"
        verify_replay_barrier "$promotion_barrier" || fail "standby has not replayed the required WAL barrier"
        psql_local -Atqc 'SELECT pg_promote(true, 60)' >/dev/null || fail "PostgreSQL promotion failed"
    elif [ "$recovering" != f ]; then
        fail "invalid PostgreSQL recovery state"
    fi
    [ "$(psql_local -Atqc 'SELECT pg_is_in_recovery()')" = f ] || fail "PostgreSQL is still in recovery"
    verify_identity "$PGDATA" || fail "cluster identity changed during promotion"
    verify_fence "$promotion_database" || fail "tenant admission opened during promotion"
    : "${PG_FC_ADMIN_ROLE:?PG_FC_ADMIN_ROLE is required}"
    : "${PG_FC_ADMIN_PASSWORD:?PG_FC_ADMIN_PASSWORD is required}"
    printf '%s' "$PG_FC_ADMIN_ROLE" | grep -Eq '^[^[:cntrl:]]{1,63}$' || fail "invalid administrative role"
    if ! psql_local -q >/dev/null 2>/dev/null <<'SQL'
\getenv role PG_FC_ADMIN_ROLE
\getenv password PG_FC_ADMIN_PASSWORD
SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_roles WHERE rolname = :'role' AND rolsuper)
       THEN format('ALTER ROLE %I PASSWORD %L', :'role', :'password')
       ELSE 'DO $$ BEGIN RAISE EXCEPTION ''administrative role missing or is not superuser''; END $$'
       END
\gexec
SQL
    then
        fail "administrative credential reconciliation failed"
    fi
    write_promotion promoted-but-fenced "$promotion_database" "$promotion_barrier"
    say_status promoted-but-fenced
}

status_cmd() {
    if [ -f "$STATUS" ]; then jq -c '{phase,error}' "$STATUS"; else echo '{"phase":"normal","error":null}'; fi
}

case "${1:-}" in
    seed) seed ;;
    prepare-promotion) prepare_promotion ;;
    promote) promote ;;
    boot-check) boot_check ;;
    status) status_cmd ;;
    *) echo "usage: pg-fc-physical {seed|prepare-promotion|promote|boot-check|status}" >&2; exit 64 ;;
esac
