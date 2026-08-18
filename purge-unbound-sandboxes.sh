#!/usr/bin/env bash
#
# purge-unbound-sandboxes.sh — delete daemon sandboxes no schema owns.
#
# The leak this clears (2026-08-18 incident): a bring-up asks heyvmd for a VM
# and gets an id the moment the deploy is 202-accepted, but the pooler only
# binds schema→id in registry.tsv after the WHOLE bring-up succeeds (boot,
# tunnel, CREATE DATABASE, restore). Every failure in between — and a deploy
# burst fails them in batches — leaves a daemon-side record bound to nothing:
# stuck in `provisioning` (or later drained to `stopped`/`failed`), with a
# name like `pg-<schema>` but no registry row, its disk pinned. No pooler
# sweep touches these: purge wants a registry row or a spare name, and the
# orphan-disk sweep only acts once the daemon has *forgotten* the id.
#
# Newer poolers ledger every create and reap this state automatically (see
# src/pending.rs + the pending-bringup janitor); this script is the one-off
# for the backlog that predates them.
#
# What counts as a candidate — ALL of:
#   - the id appears in GET /deployed-sandboxes but in NO registry.tsv row
#     (the registry is the source of truth for data ownership);
#   - the name is `pg-*` (never `spare-pg-*` — purge owns spares), or the
#     name is empty and the status is provisioning/failed (the async-create
#     tracker's promised-id entries);
#   - status is `provisioning` or `failed` — plus `stopped` only with
#     INCLUDE_STOPPED=1 (a stopped unbound pg-* is *probably* the same leak
#     post-drain, but "probably" is why it's opt-in);
#   - older than MIN_AGE_SECS (default 3600), by the larger of uptime and
#     time-since-status-change, so an in-flight legitimate bring-up (unbound
#     by definition until it finishes) is never raced. `running` VMs are
#     never touched at all.
#
# Deleting via the daemon (DELETE /deployed-sandboxes/<id>, 404 = already
# gone) removes the record AND its disk; anything the daemon leaves behind is
# then daemon-Gone, which is exactly what the pooler's orphan-disk sweep
# reclaims on its next passes (rate-limited, so a big backlog drains over a
# few intervals). Follow with a reclaim run if you want the slack back sooner.
#
# Usage:
#   ./purge-unbound-sandboxes.sh                  # DRY RUN: print the table, delete nothing
#   DELETE=1 ./purge-unbound-sandboxes.sh         # actually delete
#   INCLUDE_STOPPED=1 DELETE=1 ./purge-unbound-sandboxes.sh
#
#   HEYVMD=http://127.0.0.1:34099      daemon API
#   REGISTRY_TSV=~/.heyo/pg-vm-pool/registry.tsv
#   MIN_AGE_SECS=3600                  age floor for candidates
#
# Requires: curl, jq.
set -euo pipefail

HEYVMD="${HEYVMD:-http://127.0.0.1:34099}"
REGISTRY_TSV="${REGISTRY_TSV:-$HOME/.heyo/pg-vm-pool/registry.tsv}"
MIN_AGE_SECS="${MIN_AGE_SECS:-3600}"
INCLUDE_STOPPED="${INCLUDE_STOPPED:-0}"
DELETE="${DELETE:-0}"

for tool in curl jq; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done

if [[ ! -r "$REGISTRY_TSV" ]]; then
    # No registry means we cannot prove ANY sandbox is unowned — refuse rather
    # than classify the whole fleet as unbound.
    echo "cannot read registry at $REGISTRY_TSV (set REGISTRY_TSV) — refusing to guess" >&2
    exit 1
fi

sandboxes=$(curl -fsS --max-time 30 "$HEYVMD/deployed-sandboxes") \
    || { echo "cannot list $HEYVMD/deployed-sandboxes — daemon down?" >&2; exit 1; }

# Registry-bound ids (column 2), the ownership whitelist.
bound=$(cut -f2 "$REGISTRY_TSV" | sort -u | jq -R . | jq -s .)

now=$(date +%s)
candidates=$(jq -r \
    --argjson bound "$bound" \
    --argjson now "$now" \
    --argjson min_age "$MIN_AGE_SECS" \
    --argjson stopped "$([[ "$INCLUDE_STOPPED" == 1 ]] && echo true || echo false)" '
    def age:
        # Older of uptime and time-since-status-change. The async-create
        # trackers stamp status_changed_at with "now" on every listing, so
        # uptime_secs is the honest clock there; for settled records it is
        # the status change that measures how long they have sat. fromdate
        # only accepts %SZ, so strip fractional seconds and fold a +00:00
        # offset to Z; an unparseable stamp contributes age 0 (never deletes).
        [ (.uptime_secs // 0),
          ($now - ((.status_changed_at // empty)
                   | try (sub("\\.[0-9]+"; "") | sub("\\+00:00$"; "Z") | fromdate) catch $now))
        ] | max;
    .[]
    | select((.id // "") != "" and ([.id] | inside($bound) | not))
    | select(
        ((.name // "") | startswith("pg-")) or
        (((.name // "") == "") and ((.status // "") | IN("provisioning", "failed")))
      )
    | select((.name // "") | startswith("spare-pg-") | not)
    | select((.status // "") | IN("provisioning", "failed") or ($stopped and . == "stopped"))
    | select(age >= $min_age)
    | [.id, (if (.name // "") == "" then "-" else .name end), .status, (age | tostring)]
    | @tsv
' <<<"$sandboxes")

total=$(jq 'length' <<<"$sandboxes")
if [[ -z "$candidates" ]]; then
    echo "no unbound candidates among $total daemon sandbox(es) — nothing to do"
    exit 0
fi

count=$(wc -l <<<"$candidates")
echo "unbound candidates ($count of $total daemon sandboxes):"
printf '%-14s %-28s %-14s %s\n' ID NAME STATUS AGE_SECS
while IFS=$'\t' read -r id name status age; do
    printf '%-14s %-28s %-14s %s\n' "$id" "$name" "$status" "$age"
done <<<"$candidates"

if [[ "$DELETE" != 1 ]]; then
    echo
    echo "DRY RUN — nothing deleted. Re-run with DELETE=1 to delete these ${count} sandbox(es)."
    [[ "$INCLUDE_STOPPED" == 1 ]] || echo "(stopped unbound sandboxes were excluded; add INCLUDE_STOPPED=1 to see them)"
    exit 0
fi

deleted=0; failed=0
while IFS=$'\t' read -r id name status _age; do
    # DELETE is permanent; the SDK treats 404 as success and so do we.
    code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 60 -X DELETE \
        "$HEYVMD/deployed-sandboxes/$id")
    if [[ "$code" == 2* || "$code" == 404 ]]; then
        echo "deleted  $id  ($name, was $status)"
        deleted=$((deleted + 1))
    else
        echo "FAILED   $id  ($name): HTTP $code" >&2
        failed=$((failed + 1))
    fi
done <<<"$candidates"

echo "----"
echo "deleted $deleted, failed $failed (of $count candidates)"
echo "leftover sb-<id>/ dirs are now daemon-Gone: the pooler's orphan-disk sweep"
echo "reclaims them over its next passes; run a disk reclaim to return the slack sooner."
[[ "$failed" -eq 0 ]]
