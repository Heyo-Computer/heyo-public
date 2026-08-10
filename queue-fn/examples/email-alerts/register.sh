#!/bin/sh
# Register the email-alerts function with queue-fn.
#
# The HeyoSecret bearer token is substituted in from the environment rather than
# living in function.json, because that file is checked in. It still ends up in
# the spec — see the README's "Where the credential lives" — but it never ends
# up in git.
#
#   HEYOSECRET_TOKEN=… ./register.sh
#
# Env:
#   HEYOSECRET_TOKEN  required             bearer key for HeyoSecret's machine API
#   QFN_URL           http://127.0.0.1:9494  queue-fn admin API
#   QFN_AUTH          —                    `user:password` if QFN_ADMIN_AUTH is on
#   SECRETS_URL       —                    full HeyoSecret URL; omit to let the
#                                          guest derive it from its default route
#   SPEC              function.json
set -eu

: "${HEYOSECRET_TOKEN:?set HEYOSECRET_TOKEN to HeyoSecret's internal API key}"
export HEYOSECRET_TOKEN
QFN_URL="${QFN_URL:-http://127.0.0.1:9494}"
SPEC="${SPEC:-$(dirname "$0")/function.json}"

if [ "$HEYOSECRET_TOKEN" = "__HEYOSECRET_TOKEN__" ]; then
    echo "register.sh: HEYOSECRET_TOKEN is still the placeholder" >&2
    exit 1
fi

# python3 rather than jq or envsubst: the token is passed through the
# environment and never becomes a shell word, so it cannot land in the process
# listing or in `set -x` output.
BODY=$(SPEC_PATH="$SPEC" SECRETS_URL="${SECRETS_URL:-}" python3 - <<'PY'
import json, os, sys

with open(os.environ["SPEC_PATH"]) as fh:
    spec = json.load(fh)

env = spec.setdefault("exec", {}).setdefault("env", {})
env["ALERT_SECRETS_TOKEN"] = os.environ["HEYOSECRET_TOKEN"]

url = os.environ.get("SECRETS_URL", "").strip()
if url:
    # An explicit URL wins; the port is only used by the gateway-derived form.
    env["ALERT_SECRETS_URL"] = url
    env.pop("ALERT_SECRETS_PORT", None)

json.dump(spec, sys.stdout)
PY
)

ID=$(printf %s "$BODY" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

set -- -sS -H 'content-type: application/json'
[ -n "${QFN_AUTH:-}" ] && set -- "$@" -u "$QFN_AUTH"

# PUT when it already exists, POST when it does not. Both accept the whole spec,
# but POST tears the pool down and rebuilds it, while PUT keeps the running VMs
# whenever the `vm` block is unchanged — so rotating the token is a live edit
# rather than a cold start for every replica.
if [ "$(curl "$@" -o /dev/null -w '%{http_code}' "$QFN_URL/functions/$ID")" = "200" ]; then
    METHOD=PUT; TARGET="$QFN_URL/functions/$ID"; ACTION=updated
else
    METHOD=POST; TARGET="$QFN_URL/functions"; ACTION=registered
fi

CODE=$(curl "$@" -o /dev/null -w '%{http_code}' -X "$METHOD" -d "$BODY" "$TARGET")

case "$CODE" in
    2*) echo "$ACTION $ID (HTTP $CODE)" ;;
    502)
        # Registration fails if the JetStream consumer cannot be created, which
        # is the failure worth naming: without a consumer the function would
        # accept events that nothing ever pulls.
        echo "register.sh: HTTP 502 — NATS is unreachable or JetStream is off" >&2
        exit 1 ;;
    *)  echo "register.sh: queue-fn returned HTTP $CODE" >&2
        curl "$@" -X "$METHOD" -d "$BODY" "$TARGET" >&2
        exit 1 ;;
esac
