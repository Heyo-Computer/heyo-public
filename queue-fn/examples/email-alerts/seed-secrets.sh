#!/bin/sh
# Write the function's configuration into HeyoSecret.
#
# Everything the fan-out actually needs lives here rather than in the queue-fn
# spec: SMTP host and password, and the routing table that decides who gets
# paged. Both are read at invocation time, so changing who is on call is a
# secret write — no re-registration, no VM rebuild, no queue-fn restart.
#
#   HEYOSECRET_TOKEN=… SMTP_PASSWORD=… ./seed-secrets.sh
#
# Env:
#   HEYOSECRET_TOKEN  required                bearer key for the machine API
#   HEYOSECRET_URL    http://127.0.0.1:4455
#   SMTP_PASSWORD     —                       from the environment, never a flag
#   SMTP_HOST / SMTP_PORT / SMTP_USER / SMTP_FROM
#   ONCALL / SRE / DEFAULT_TO                 comma-separated recipient lists
set -eu

: "${HEYOSECRET_TOKEN:?set HEYOSECRET_TOKEN to HeyoSecret's internal API key}"
export HEYOSECRET_TOKEN
HEYOSECRET_URL="${HEYOSECRET_URL:-http://127.0.0.1:4455}"

# mktemp is 0600, and the trap runs on the error paths too — these files hold
# plaintext secrets for the length of one curl.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM

write_secret() {
    # $1 path, $2 description, $3 file holding the JSON value.
    PATH_ARG="$1" DESC_ARG="$2" VALUE_FILE="$3" python3 - > "$WORK/body.json" <<'PY'
import base64, json, os, sys

with open(os.environ["VALUE_FILE"], "rb") as fh:
    value = fh.read()
json.loads(value)  # fail here, not at 3am inside the guest

json.dump({
    "path": os.environ["PATH_ARG"],
    "valueBase64": base64.b64encode(value).decode(),
    "description": os.environ["DESC_ARG"],
    "owner": "queue-fn/email-alerts",
    "tags": ["queue-fn", "email-alerts"],
    "actor": "seed-secrets.sh",
}, sys.stdout)
PY

    code=$(curl -sS -o /dev/null -w '%{http_code}' \
        -X POST "$HEYOSECRET_URL/v1/secrets/write" \
        -H "authorization: Bearer $HEYOSECRET_TOKEN" \
        -H 'content-type: application/json' \
        --data-binary "@$WORK/body.json")
    rm -f "$WORK/body.json"

    case "$code" in
        2*)  echo "wrote $1" ;;
        401) echo "seed-secrets.sh: HTTP 401 — HEYOSECRET_TOKEN is wrong" >&2; exit 1 ;;
        *)   echo "seed-secrets.sh: $1 failed with HTTP $code" >&2; exit 1 ;;
    esac
}

# --- SMTP relay ------------------------------------------------------------
# `tls` is one of starttls (587), implicit (465), or none. Writing a new version
# retires the previous one, so rotating the password is a single write and a
# running VM picks it up within ALERT_CACHE_TTL_SECS.

SMTP_PASSWORD="${SMTP_PASSWORD:-}" \
SMTP_HOST="${SMTP_HOST:-smtp.example.com}" \
SMTP_PORT="${SMTP_PORT:-587}" \
SMTP_USER="${SMTP_USER:-alerts@example.com}" \
SMTP_FROM="${SMTP_FROM:-Alerts <alerts@example.com>}" \
python3 -c 'import json,os,sys; json.dump({
  "host": os.environ["SMTP_HOST"],
  "port": int(os.environ["SMTP_PORT"]),
  "tls": "starttls",
  "username": os.environ["SMTP_USER"],
  "password": os.environ["SMTP_PASSWORD"],
  "from": os.environ["SMTP_FROM"],
  "subject_prefix": "[ALERT]",
  "connect_timeout_secs": 8,
}, sys.stdout)' > "$WORK/smtp.json"
write_secret "alerts/smtp/relay" "SMTP relay for queue-fn email alerts" "$WORK/smtp.json"

# --- Routing table ---------------------------------------------------------
# Every matching rule contributes recipients — they union rather than
# first-match — and `default` applies only when nothing matched at all.
#
#   retry_undelivered  never | always
#       `never` (default) accepts a gap over duplicating mail that already went
#       out. `always` re-runs the whole fan-out, so everyone reached the first
#       time is reached again.
#   unrouted           drop | fail
#       `drop` exits 0 and records `no_recipients`; `fail` sends the alert to
#       the DLQ instead, which is right when a missed alert is worse than a
#       queue that needs draining.

ONCALL="${ONCALL:-oncall@example.com}" \
SRE="${SRE:-sre@example.com}" \
DEFAULT_TO="${DEFAULT_TO:-alerts-catchall@example.com}" \
python3 -c '
import json, os, sys
split = lambda v: [a.strip() for a in v.split(",") if a.strip()]
json.dump({
  "version": 1,
  "rules": [
    {"name": "pager", "match": {"severity": ["critical", "page"]},
     "to": split(os.environ["ONCALL"])},
    {"name": "prod-warnings", "match": {"severity": "warning", "labels": {"env": "prod"}},
     "to": split(os.environ["SRE"])},
  ],
  "default": split(os.environ["DEFAULT_TO"]),
  "max_recipients": 25,
  "retry_undelivered": "never",
  "unrouted": "drop",
}, sys.stdout)' > "$WORK/routing.json"
write_secret "alerts/routing/email-alerts" "Who gets which alert" "$WORK/routing.json"

# --- Webhook signing key ---------------------------------------------------
# Generated server-side so the plaintext never passes through this script; read
# it back out of the dashboard (or /v1/secrets/read) to configure the sender.

code=$(curl -sS -o /dev/null -w '%{http_code}' \
    -X POST "$HEYOSECRET_URL/v1/secrets/rotate-random" \
    -H "authorization: Bearer $HEYOSECRET_TOKEN" \
    -H 'content-type: application/json' \
    -d '{"path":"alerts/webhook/hmac","bytes":32,"actor":"seed-secrets.sh"}')
case "$code" in
    2*) echo "wrote alerts/webhook/hmac (32 random bytes)" ;;
    *)  echo "seed-secrets.sh: webhook key failed with HTTP $code" >&2; exit 1 ;;
esac

cat <<EOF

Read the signing key back for your webhook sender with:
  curl -sX POST $HEYOSECRET_URL/v1/secrets/read \\
    -H "authorization: Bearer \$HEYOSECRET_TOKEN" \\
    -H 'content-type: application/json' -d '{"path":"alerts/webhook/hmac"}'
EOF
