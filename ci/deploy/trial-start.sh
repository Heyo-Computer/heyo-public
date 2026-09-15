#!/usr/bin/env bash
# Single-VM trial bundle: ci, nats-server, nats.conf, and this file as start.sh.
# The orchestrator owns the VM; this supervisor only owns the two app processes.
set -euo pipefail
cd "$(dirname "$0")"
: "${CI_NATS_TOKEN:?CI_NATS_TOKEN is required}"
mkdir -p /workspace/ci-state/{nats,logs,workspaces,artifacts}

pids=()
cleanup() {
    trap - EXIT TERM INT
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT

./nats-server --config ./nats.conf &
pids+=("$!")
ready=0
for ((attempt = 0; attempt < 30; attempt++)); do
    kill -0 "${pids[0]}" 2>/dev/null || { echo 'NATS exited during startup' >&2; exit 1; }
    if curl --fail --silent --max-time 1 http://127.0.0.1:8222/healthz >/dev/null; then
        ready=1
        break
    fi
    sleep 1
done
((ready)) || { echo 'NATS readiness timed out' >&2; exit 1; }

./ci &
pids+=("$!")
status=0
wait -n "${pids[@]}" || status=$?
# Either process exiting ends the whole service, including an unexpected exit 0.
((status != 0)) || status=1
exit "$status"
