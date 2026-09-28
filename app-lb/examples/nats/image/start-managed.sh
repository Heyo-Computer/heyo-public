#!/bin/sh
# app-lb owns the workspace mount and supplies NATS_TOKEN via env_from.
# Never initialize an empty broker just because restored state is unavailable.
set -eu
: "${NATS_TOKEN:?configure the managed broker credential}"
state=/workspace
if ! mountpoint -q "$state" || [ "$(stat -c %d "$state")" = "$(stat -c %d /)" ]; then
    echo 'NATS refuses to start without its dedicated mounted workspace' >&2
    exit 1
fi
if [ "$(cat "$state/.managed-state" 2>/dev/null)" != nats-state-v1 ]; then
    echo 'NATS refuses to start without an explicitly seeded managed workspace' >&2
    exit 1
fi
# A retained marker alone is not evidence that the store was restored.
if [ ! -d "$state/jetstream" ] || [ -L "$state/jetstream" ] || [ "$(stat -c %d "$state/jetstream")" != "$(stat -c %d "$state")" ]; then
    echo 'NATS refuses to start without the seeded JetStream directory' >&2
    exit 1
fi
# NATS parses environment references as config values, not opaque strings.
# JSON quoting preserves numeric prefixes, quotes and backslashes without
# exposing the credential in process arguments or writing it to the image.
NATS_CONFIG_TOKEN=$(printf '%s' "$NATS_TOKEN" | jq -Rs .)
export NATS_CONFIG_TOKEN
exec nats-server -c /etc/nats/managed.conf
