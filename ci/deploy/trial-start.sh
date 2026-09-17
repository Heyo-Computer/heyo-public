#!/usr/bin/env bash
# CI owns only its process. NATS is an independently managed service.
set -euo pipefail
cd "$(dirname "$0")"
: "${CI_NATS_URL:?configure an independently managed NATS service}"
mkdir -p /workspace/ci-state/{logs,workspaces,artifacts}
exec ./ci
