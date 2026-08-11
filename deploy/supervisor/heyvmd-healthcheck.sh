#!/usr/bin/env bash
# Polls heyvmd's API and force-restarts it via supervisorctl — as a LAST
# resort. Runs as its own supervisor program (see heyvmd.conf) because
# supervisor's autorestart only catches a heyvmd process that has actually
# exited, not one that's alive but no longer answering requests.
#
# Restarting heyvmd is DESTRUCTIVE: the Firecracker VMM processes are direct
# children of the daemon (their piped stdio is the guest serial console), and
# the supervisor unit stops the whole process group — so a restart kills every
# running VM. Every active schema then cold-starts at once against the fresh
# daemon, which is exactly the kind of deploy burst that makes the daemon look
# dead in the first place (it runs blocking mkfs/debugfs work on its async
# workers). A trigger-happy watchdog therefore *causes* the outage cycle it's
# meant to fix. Hence two changes from the obvious design:
#
#   1. The restart decision keys on GET /health — a trivial, lock-free
#      handler. If /health answers, the daemon's runtime is scheduling and a
#      restart can only do harm. Only sustained /health silence (the runtime
#      wedged or the process gone deaf) triggers a restart, and only after
#      FAIL_THRESHOLD consecutive failures.
#   2. GET /deployed-sandboxes failing while /health answers means the
#      sandbox manager is lock-contended (e.g. deploys serializing on the
#      handles lock). That is logged for visibility but never acted on: it
#      resolves itself when the deploys drain, and a restart would trade a
#      slow API for dead VMs.
#
# The long COOLDOWN after a restart is deliberate: the post-restart reconnect
# storm makes the fresh daemon legitimately slow, and re-judging it during
# recovery is how one restart becomes a restart loop.
set -u

PORT="${HEYVMD_HEALTHCHECK_PORT:-34099}"
HEALTH_URL="http://127.0.0.1:${PORT}/health"
LIST_URL="http://127.0.0.1:${PORT}/deployed-sandboxes"
INTERVAL="${HEYVMD_HEALTHCHECK_INTERVAL_SECS:-15}"
CURL_TIMEOUT="${HEYVMD_HEALTHCHECK_CURL_TIMEOUT_SECS:-20}"
FAIL_THRESHOLD="${HEYVMD_HEALTHCHECK_FAIL_THRESHOLD:-8}"
COOLDOWN_SECS="${HEYVMD_HEALTHCHECK_COOLDOWN_SECS:-600}"

fails=0
while true; do
  if curl -fsS --max-time "$CURL_TIMEOUT" "$HEALTH_URL" >/dev/null 2>&1; then
    fails=0
    # Runtime is alive. Separately report (log-only, never restart) a
    # slow/wedged sandbox manager so the signal isn't lost.
    if ! curl -fsS --max-time "$CURL_TIMEOUT" "$LIST_URL" >/dev/null 2>&1; then
      echo "$(date -Is) heyvmd /health OK but $LIST_URL slow/failing" \
           "(manager busy or lock-contended — NOT restarting)"
    fi
  else
    fails=$((fails + 1))
    echo "$(date -Is) heyvmd /health check failed (${fails}/${FAIL_THRESHOLD}): $HEALTH_URL"
    if [ "$fails" -ge "$FAIL_THRESHOLD" ]; then
      echo "$(date -Is) restarting heyvmd after ${fails} consecutive failures" \
           "— this kills every running VM"
      supervisorctl restart heyvmd
      fails=0
      sleep "$COOLDOWN_SECS"
    fi
  fi
  sleep "$INTERVAL"
done
