#!/usr/bin/env bash
#
# disk-audit.sh — reconcile what's on disk against what heyvmd and the pooler
# think exists. READ ONLY: it makes GET requests and stats files, and never
# deletes, kills, or modifies anything.
#
# THE QUESTION IT ANSWERS. Three systems each hold a partial view of a pooler
# host and they drift apart:
#
#   the run dir   one sb-<id>/ directory per VM, holding data.ext4 (the
#                 schema's database) and, between boots it shouldn't survive,
#                 a rootfs.ext4 clone;
#   heyvmd        TWO views, merged here. The API listing (`GET
#                 /deployed-sandboxes`, what the dashboard shows) reflects only
#                 the daemon's IN-MEMORY state: running VMs it reattached at
#                 boot plus sandboxes created/started since the process last
#                 started — right after a daemon restart it shrinks to ~the
#                 running set. The durable truth is the persisted store
#                 (<heyo-data>/sandboxes/<id>/sandbox.yaml), which the per-id
#                 endpoint — and the pooler's orphan sweep — answer from. A
#                 sandbox is "known" if EITHER knows it; classifying off the
#                 listing alone once mislabeled 7315 healthy stopped schemas
#                 as data-loss orphans an hour after a daemon restart. The
#                 persisted store can still lose records while directories
#                 survive (a DELETE that 404s before cleanup) — that gap is
#                 the real orphan bucket;
#   registry.tsv  the pooler's schema → sandbox-id binding and storage tier.
#                 The only thing that says which disk holds whose data.
#
# A directory the daemon has forgotten is invisible to every count an operator
# normally has, while still occupying its bytes. This prints the size of each
# such gap, and — the part that matters — splits the forgotten directories by
# whether the registry still depends on them.
#
# THE BUCKETS (the summary names them in this order):
#
#   orphan/unowned     directory, no daemon record, no registry row references
#                      the id. Dead weight; the pooler's orphan sweep deletes
#                      these on its own once its per-id probe agrees.
#   orphan/offloaded   directory, no daemon record, registry row says the data
#                      is durably elsewhere (compacted/frozen/archived). Also
#                      dead weight, also swept.
#   orphan/DATA-LOSS   directory, no daemon record, registry row still says
#                      `live` — this disk is that schema's ONLY copy. The
#                      sweep refuses to delete these and logs each at error
#                      level. The next client connect creates a fresh empty VM,
#                      rebinds the schema, and the disk silently becomes
#                      deletable, so the loss shows up as an empty database.
#                      Act on this bucket before it resolves itself.
#   record-only        daemon record with no directory. Usually a create that
#                      never got as far as a disk (provisioning/failed) or a
#                      dir already reclaimed; harmless, but it inflates the
#                      daemon's count against the disk's.
#   unbacked schema    registry row, tier `live`, with neither a directory nor
#                      a daemon record. Nothing left to restore — already lost,
#                      it just hasn't been noticed yet.
#   stale rootfs       rootfs.ext4 next to a VM that isn't running. heyvmd
#                      re-clones it on the next boot, so these are pure
#                      reclaimable waste (see prune-stale-rootfs.sh).
#
# Sizes are ALLOCATED bytes (512-byte blocks, what `du` counts), not the
# apparent size — a data disk is sparse and its apparent size is the
# provisioned ceiling, which would overstate every bucket several-fold.
#
# Usage:
#   ./disk-audit.sh [RUN_DIR]
#
#   REGISTRY_TSV=~/.heyo/pg-vm-pool/registry.tsv   pooler state
#   HEYVMD=http://127.0.0.1:34099                  daemon API
#   DAEMON_SANDBOXES_DIR=/path      heyvmd's persisted store (the directory
#                                   holding <id>/sandbox.yaml). Default: auto —
#                                   `sandboxes/` next to RUN_DIR, then
#                                   ~/.heyo/sandboxes. Without it the audit
#                                   falls back to the API listing alone and
#                                   over-reports orphans after a daemon restart
#                                   (warned loudly).
#   PERSISTED_IDS=/path/to/ids.txt  audit from a captured id list (one sandbox
#                                   id per line) instead of scanning
#                                   DAEMON_SANDBOXES_DIR — pairs with
#                                   DAEMON_JSON for off-host analysis
#   DAEMON_JSON=/path/to/list.json  analyse a saved `GET /deployed-sandboxes`
#                                   instead of calling the daemon (also lets a
#                                   wedged daemon be audited from a capture)
#   OUT_DIR=/path                   where the per-bucket id lists are written
#                                   (default: a fresh mktemp -d)
#
# Requires: curl + jq (unless DAEMON_JSON is set), GNU find/awk/numfmt.
#
set -uo pipefail

RUN_DIR="${1:-${HOME}/.heyo/run}"
REGISTRY_TSV="${REGISTRY_TSV:-$HOME/.heyo/pg-vm-pool/registry.tsv}"
HEYVMD="${HEYVMD:-http://127.0.0.1:34099}"
DAEMON_SANDBOXES_DIR="${DAEMON_SANDBOXES_DIR:-}"
PERSISTED_IDS="${PERSISTED_IDS:-}"
DAEMON_JSON="${DAEMON_JSON:-}"
OUT_DIR="${OUT_DIR:-$(mktemp -d -t disk-audit-XXXXXX)}"

die() { echo "error: $*" >&2; exit 1; }
human() { numfmt --to=iec --suffix=B "${1:-0}" 2>/dev/null || echo "${1:-0}B"; }

[ -d "$RUN_DIR" ] || die "run dir not found: $RUN_DIR (pass it as the first argument)"
for tool in find awk numfmt; do
    command -v "$tool" >/dev/null || die "missing required tool: $tool"
done
mkdir -p "$OUT_DIR" || die "cannot write to OUT_DIR=$OUT_DIR"

# ---- 1. what is on disk ----------------------------------------------------
# One find pass over the whole run dir: %b is the file's ALLOCATED size in
# 512-byte blocks, so this needs no per-file stat and no du. Emitted as
# `<id> <kind> <blocks>` and folded into one row per sandbox below.
echo "scanning $RUN_DIR ..." >&2
find "$RUN_DIR" -mindepth 2 -maxdepth 2 -type f \
     \( -name 'data.ext4' -o -name 'rootfs.ext4' \) \
     -printf '%h\t%f\t%b\n' 2>/dev/null |
    awk -F'\t' '{
        n = split($1, parts, "/"); id = parts[n]
        if ($2 == "data.ext4") data[id] += $3 * 512; else root[id] += $3 * 512
        seen[id] = 1
    }
    END { for (id in seen) printf "%s\t%d\t%d\n", id, data[id], root[id] }' \
    > "$OUT_DIR/disk.tsv"

# Directories with neither file (a create that never provisioned a disk, or an
# already-emptied dir) still count as present on disk.
find "$RUN_DIR" -mindepth 1 -maxdepth 1 -type d -name 'sb-*' -printf '%f\n' 2>/dev/null |
    awk -F'\t' 'NR==FNR { have[$1]=1; next } !($1 in have) { printf "%s\t0\t0\n", $1 }' \
        "$OUT_DIR/disk.tsv" - >> "$OUT_DIR/disk.tsv"

# ---- 2. what the daemon knows ----------------------------------------------
# 2a. The API listing (live statuses, but in-memory only — see header).
if [ -n "$DAEMON_JSON" ]; then
    [ -r "$DAEMON_JSON" ] || die "cannot read DAEMON_JSON=$DAEMON_JSON"
    daemon_src="$DAEMON_JSON"
else
    for tool in curl jq; do
        command -v "$tool" >/dev/null || die "missing required tool: $tool (or set DAEMON_JSON)"
    done
    daemon_src="$OUT_DIR/daemon.json"
    echo "fetching $HEYVMD/deployed-sandboxes ..." >&2
    curl -sS --max-time 120 "$HEYVMD/deployed-sandboxes" -o "$daemon_src" ||
        die "daemon unreachable at $HEYVMD — capture the listing elsewhere and pass DAEMON_JSON="
fi
command -v jq >/dev/null || die "missing required tool: jq"
jq -r '.[] | [.id, (.status // "unknown"), (.name // "")] | @tsv' "$daemon_src" \
    > "$OUT_DIR/listing.tsv" 2>/dev/null ||
    die "could not parse the daemon listing as JSON (truncated response?)"

# 2b. The persisted store (<id>/sandbox.yaml) — the durable record set the
# per-id endpoint and the pooler's orphan sweep answer from. Auto-discover it
# next to the run dir (heyvmd keeps run/ and sandboxes/ under one data dir),
# then under ~/.heyo.
listing_only=0
if [ -n "$PERSISTED_IDS" ]; then
    [ -r "$PERSISTED_IDS" ] || die "cannot read PERSISTED_IDS=$PERSISTED_IDS"
    sort -u "$PERSISTED_IDS" > "$OUT_DIR/persisted.txt"
else
    if [ -z "$DAEMON_SANDBOXES_DIR" ]; then
        for cand in "$(dirname "$RUN_DIR")/sandboxes" "$HOME/.heyo/sandboxes"; do
            [ -d "$cand" ] && { DAEMON_SANDBOXES_DIR="$cand"; break; }
        done
    fi
    if [ -n "$DAEMON_SANDBOXES_DIR" ] && [ -d "$DAEMON_SANDBOXES_DIR" ]; then
        echo "scanning persisted store $DAEMON_SANDBOXES_DIR ..." >&2
        find "$DAEMON_SANDBOXES_DIR" -mindepth 2 -maxdepth 2 -name sandbox.yaml \
             -printf '%h\n' 2>/dev/null | awk -F/ '{print $NF}' | sort -u \
            > "$OUT_DIR/persisted.txt"
    else
        : > "$OUT_DIR/persisted.txt"
        listing_only=1
        echo "warning: heyvmd's persisted store not found (set DAEMON_SANDBOXES_DIR) —" >&2
        echo "         classifying off the API listing ALONE. The listing only shows" >&2
        echo "         in-memory sandboxes, so if the daemon restarted recently every" >&2
        echo "         stopped-but-persisted sandbox below is MISREPORTED as an orphan." >&2
    fi
fi

# Known = listing ∪ persisted. Listing rows keep their live status; persisted
# ids absent from the listing get the synthetic status `persisted` (on disk,
# not loaded — a stopped sandbox the daemon serves by id but doesn't list).
{
    cat "$OUT_DIR/listing.tsv"
    awk -F'\t' 'NR==FNR { listed[$1]=1; next } !($1 in listed) { print $1 "\tpersisted\t" }' \
        "$OUT_DIR/listing.tsv" "$OUT_DIR/persisted.txt"
} > "$OUT_DIR/daemon.tsv"

# ---- 3. what the registry claims -------------------------------------------
# `schema \t sandbox_id \t last_active \t tier`; 2-column legacy rows are live.
if [ -r "$REGISTRY_TSV" ]; then
    awk -F'\t' 'NF >= 2 { print $2 "\t" ($4 == "" ? "live" : $4) "\t" $1 }' \
        "$REGISTRY_TSV" > "$OUT_DIR/registry.tsv"
else
    echo "warning: no registry at $REGISTRY_TSV — every orphan will read as \
'unowned' and the data-loss bucket cannot be computed" >&2
    : > "$OUT_DIR/registry.tsv"
fi

# ---- 4. reconcile ----------------------------------------------------------
# One awk over all three, keyed by sandbox id. Counts and byte totals per
# bucket to stdout; the id lists land in OUT_DIR for follow-up.
awk -F'\t' -v out="$OUT_DIR" '
FILENAME == ARGV[1] { status[$1] = $2; dname[$1] = $3; drecords++; next }
FILENAME == ARGV[2] { tier[$1] = $2; schema[$1] = $3; rrows++; next }
{
    id = $1; data = $2; root = $3
    ondisk[id] = 1; dirs++
    disk_bytes += data; root_bytes += root

    running = (id in status && status[id] == "running")
    if (root > 0) root_files++
    if (root > 0 && !running) { stale_root++; stale_root_bytes += root
        print id "\t" root > out "/stale-rootfs.txt" }

    if (id in status) { both++; both_bytes += data + root; next }

    # No daemon record — the delta. Split by what the registry still wants.
    orph++; orph_bytes += data + root
    if (!(id in tier)) {
        unowned++; unowned_bytes += data + root
        print id "\t" data > out "/orphan-unowned.txt"
    } else if (tier[id] == "live") {
        loss++; loss_bytes += data + root
        print schema[id] "\t" id "\t" data > out "/orphan-DATA-LOSS.txt"
    } else {
        off++; off_bytes += data + root
        print schema[id] "\t" id "\t" tier[id] "\t" data > out "/orphan-offloaded.txt"
    }
}
END {
    for (id in status) {
        if (id in ondisk) continue
        reconly++
        print id "\t" status[id] "\t" dname[id] > out "/record-only.txt"
        if (status[id] == "running") reconly_running++
    }
    for (id in tier) {
        if (tier[id] != "live" || id in ondisk || id in status) continue
        unbacked++
        print schema[id] "\t" id > out "/schema-unbacked.txt"
    }
    for (id in status) {
        if (status[id] == "running") drunning++
        else if (status[id] == "persisted") dpersist++
        else dstopped++
    }
    for (id in tier) tiers[tier[id]]++

    printf "dirs\t%d\t%d\n", dirs, disk_bytes + root_bytes
    printf "dirs.data\t%d\t%d\n", dirs, disk_bytes
    printf "dirs.rootfs\t%d\t%d\n", root_files + 0, root_bytes
    printf "daemon\t%d\t0\n", drecords + 0
    printf "daemon.listed\t%d\t0\n", drecords - dpersist
    printf "daemon.running\t%d\t0\n", drunning + 0
    printf "daemon.stopped\t%d\t0\n", dstopped + 0
    printf "daemon.persisted\t%d\t0\n", dpersist + 0
    printf "registry\t%d\t0\n", rrows + 0
    for (t in tiers) printf "registry.%s\t%d\t0\n", t, tiers[t]
    printf "both\t%d\t%d\n", both + 0, both_bytes + 0
    printf "orphan\t%d\t%d\n", orph + 0, orph_bytes + 0
    printf "orphan.unowned\t%d\t%d\n", unowned + 0, unowned_bytes + 0
    printf "orphan.offloaded\t%d\t%d\n", off + 0, off_bytes + 0
    printf "orphan.dataloss\t%d\t%d\n", loss + 0, loss_bytes + 0
    printf "recordonly\t%d\t0\n", reconly + 0
    printf "recordonly.running\t%d\t0\n", reconly_running + 0
    printf "unbacked\t%d\t0\n", unbacked + 0
    printf "staleroot\t%d\t%d\n", stale_root + 0, stale_root_bytes + 0
}' "$OUT_DIR/daemon.tsv" "$OUT_DIR/registry.tsv" "$OUT_DIR/disk.tsv" > "$OUT_DIR/summary.tsv"

get() { awk -F'\t' -v k="$1" -v f="${2:-1}" '$1 == k { print $(f + 1); found = 1 }
                                             END { if (!found) print 0 }' "$OUT_DIR/summary.tsv"; }
row() { printf '  %-22s %8s  %10s  %s\n' "$1" "$(get "$2")" "$(human "$(get "$2" 2)")" "${3:-}"; }
plain() { printf '  %-22s %8s  %10s  %s\n' "$1" "$(get "$2")" "" "${3:-}"; }

echo
echo "=== disk audit: $RUN_DIR ==="
printf '  %-22s %8s  %10s\n' "" "count" "allocated"
echo "on disk"
row "  sb-<id>/ dirs" dirs
row "  data.ext4" dirs.data
row "  rootfs.ext4" dirs.rootfs
echo "heyvmd records"
plain "  known (list∪store)" daemon
plain "  listed by API" daemon.listed "in-memory: running + touched since daemon start"
plain "    of which running" daemon.running
plain "    stopped/other" daemon.stopped
plain "  persisted only" daemon.persisted "on disk, not loaded — normal after a daemon restart"
echo "registry rows"
plain "  total" registry
for t in live compacted frozen archived; do
    n=$(get "registry.$t")
    [ "$n" != 0 ] && plain "  $t" "registry.$t"
done
echo
echo "reconciliation"
row "  matched" both "directory + daemon record (listed or persisted)"
row "  ORPHAN (no record)" orphan "neither listed nor persisted — truly forgotten"
row "    unowned" orphan.unowned "no schema wants it; the sweep deletes these"
row "    offloaded" orphan.offloaded "data is durably elsewhere; the sweep deletes these"
row "    DATA-LOSS" orphan.dataloss "registry says live — this disk is the only copy"
plain "  record-only" recordonly "daemon record, no directory"
plain "    of which running" recordonly.running
plain "  unbacked live rows" unbacked "no disk, no record — already lost"
echo
row "  stale rootfs copies" staleroot "reclaimable now (prune-stale-rootfs.sh)"
echo
echo "id lists: $OUT_DIR"
ls -1 "$OUT_DIR" | sed 's/^/  /'
echo
if [ "$listing_only" = 1 ]; then
    echo "WARNING: persisted store not scanned — every count above treats the API listing"
    echo "         as heyvmd's whole memory. If the daemon restarted since these sandboxes"
    echo "         were last touched, the orphan/DATA-LOSS buckets are inflated by every"
    echo "         stopped-but-persisted sandbox. Set DAEMON_SANDBOXES_DIR and re-run"
    echo "         before acting on ANY bucket."
    echo
fi
if [ "$(get orphan.dataloss)" != 0 ]; then
    echo "NOTE: $(get orphan.dataloss) data-loss orphan(s). Each is a schema whose registry row"
    echo "      still says 'live' while heyvmd has forgotten its VM. Nothing deletes these —"
    echo "      but the next client connect rebinds the schema to a fresh EMPTY VM, after"
    echo "      which the disk becomes an ordinary orphan and is swept. Spot-check before"
    echo "      acting (column 2 is the sandbox id):"
    echo "        while IFS=\$'\\t' read -r _ id _; do curl -s -o /dev/null -w \"\$id %{http_code}\\n\" \\"
    echo "          $HEYVMD/deployed-sandboxes/\$id; done < $OUT_DIR/orphan-DATA-LOSS.txt"
    echo "      (a 200 means the daemon still knows it — not an orphan). See"
    echo "      $OUT_DIR/orphan-DATA-LOSS.txt"
fi
echo "nothing was modified: this audit only reads."
