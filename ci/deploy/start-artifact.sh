#!/usr/bin/env bash
# Boot an immutable CI build without starting or stopping its external broker.
# Arguments: read-only release directory, runtime directory, persistent state directory.
set -euo pipefail
release=${1:?release directory required}
runtime=${2:?runtime directory required}
state=${3:?persistent state directory required}
: "${CI_EXPECTED_SHA:?expected source revision required}"
: "${CI_NATS_URL:?configure an independently managed NATS service}"

[[ "$(stat -c %d "$state")" != "$(stat -c %d /)" ]] || {
    echo 'CI state must be on a persistent mounted filesystem, not the rootfs' >&2
    exit 1
}
[[ "$(cat "$state/.managed-state")" == 'ci-state-v1' ]] || {
    echo 'CI state migration marker is missing or invalid' >&2
    exit 1
}
[[ "$(cat "$release/REVISION")" == "$CI_EXPECTED_SHA" ]] || {
    echo 'CI artifact revision does not match the authorized deployment' >&2
    exit 1
}
(cd "$release" && sha256sum --strict --check SHA256SUMS)
install -m 755 "$release/ci" "$runtime/ci"
install -m 644 "$release/REVISION" "$runtime/REVISION"
echo "Starting CI revision $CI_EXPECTED_SHA"
mkdir -p "$state"/{logs,workspaces,artifacts}
cd "$runtime"
# Older rootfs images contain a start.sh that couples CI to NATS. Never invoke it.
exec ./ci
