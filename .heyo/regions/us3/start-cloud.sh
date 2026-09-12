#!/usr/bin/env bash
# Run a verified, already-built Cloud release in an app-lb-managed guest.
# S3 signing happens here; no expiring URL or staging service is needed at boot.
set -euo pipefail
umask 077

: "${CLOUD_SOURCE_ARCHIVE_URL:?}"
: "${CLOUD_SOURCE_ARCHIVE_SHA256:?}"
: "${CLOUD_S3_REGION:?}"
: "${CLOUD_S3_ACCESS_KEY_ID:?}"
: "${CLOUD_S3_SECRET_ACCESS_KEY:?}"
[[ "$CLOUD_SOURCE_ARCHIVE_URL" == https://* ]]
[[ "$CLOUD_SOURCE_ARCHIVE_SHA256" =~ ^[0-9a-f]{64}$ ]]

mkdir -p /workspace/cloud-service
cd /workspace/cloud-service
curl --fail --silent --show-error --retry 3 --connect-timeout 15 --max-time 300 \
    --aws-sigv4 "aws:amz:${CLOUD_S3_REGION}:s3" \
    --user "${CLOUD_S3_ACCESS_KEY_ID}:${CLOUD_S3_SECRET_ACCESS_KEY}" \
    "$CLOUD_SOURCE_ARCHIVE_URL" --output release.tar.gz
printf '%s  release.tar.gz\n' "$CLOUD_SOURCE_ARCHIVE_SHA256" | sha256sum --check --status
tar --extract --gzip --file release.tar.gz --no-same-owner
rm release.tar.gz
test -x ./cloud
test -d ./migrations

# The existing Postgres allowlist admits each service VM's IP/tap pair. Wait
# while that narrow grant is established instead of exiting during provisioning.
for attempt in $(seq 90); do
    if timeout 2 bash -c '</dev/tcp/pg.us3.heyo.work/6432' 2>/dev/null; then
        export CLOUD_MIGRATIONS_DIR=/workspace/cloud-service/migrations
        exec ./cloud
    fi
    sleep 2
done
echo 'us3 Postgres access was not ready before the startup deadline' >&2
exit 1
