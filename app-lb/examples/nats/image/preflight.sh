#!/bin/bash
# Check a booted NATS VM from the inside.
#
#     serverctl exec nats -- /opt/nats/preflight.sh
#
# Two properties are worth a script, because neither is visible from outside and
# both fail silently:
#
#   1. Whether the JetStream store is really on the data disk. `/healthz` answers
#      200 either way, so a VM that will discard every stream on its next cold
#      boot is indistinguishable from a healthy one until it is too late.
#   2. Whether the listeners are reachable at the VM's *guest_ip* rather than
#      only on loopback. A loopback-only bind serves this script perfectly and is
#      unreachable from app-lb, which is the only client that matters.
#
# bash, not sh: the connectivity checks use /dev/tcp, which is a bash builtin.
# That is deliberate — it tests the thing app-lb actually does (open a socket and
# talk) instead of inferring it from /proc, and it needs no curl in the image.
#
# Exits non-zero on the first failure, so it is usable in a script.
set -u

fail() { echo "FAIL: $*"; exit 1; }
ok()   { echo "ok:   $*"; }

# --- Durability -----------------------------------------------------------

grep -q ' /workspace ' /proc/mounts \
    || fail "/workspace is not a mount — the store is on the rootfs and will not survive a cold boot"
ok "/workspace is mounted from $(awk '$2=="/workspace"{print $1}' /proc/mounts)"

# nats-server appends `jetstream` to store_dir, so this is the real store path
# for `store_dir: "/workspace"`.
STORE=/workspace/jetstream
[ -d "$STORE" ] || fail "$STORE does not exist — nats-server never started, or store_dir is wrong"

# Prove it by writing rather than by reading the config: a store_dir pointing at
# a path that happens to exist on the rootfs would pass every check above.
probe="$STORE/.preflight"
: > "$probe" 2>/dev/null || fail "$STORE is not writable"
rm -f "$probe"
ok "$STORE is writable and on the data disk"

# --- The process ----------------------------------------------------------

pgrep -x nats-server >/dev/null 2>&1 || fail "nats-server is not running (see /tmp/nats-boot.log)"
ok "nats-server is running"

# --- Reachability ---------------------------------------------------------
#
# Against the guest_ip, not 127.0.0.1. That is the whole point: app-lb reaches
# this VM across the tap network, so a loopback bind is the failure to catch and
# testing loopback would hide it.
GUEST_IP=$(ip -4 -o addr show eth0 2>/dev/null | awk '{split($4,a,"/"); print a[1]}')
[ -n "$GUEST_IP" ] || fail "eth0 has no IPv4 address — init.sh could not bring the network up"
ok "guest_ip is $GUEST_IP"

# NATS is server-speaks-first: on connect it sends `INFO {...}`. Reading that
# back proves the server is accepting and speaking the protocol, which a bound
# socket alone does not — a wedged server still holds its listener open.
# The braces matter: a failed /dev/tcp redirect is reported by the shell itself,
# not by the command, so the redirection has to be inside the group being
# silenced or bash's "Connection refused" lands on top of the real diagnostic.
if ! { exec 3<>"/dev/tcp/$GUEST_IP/4222"; } 2>/dev/null; then
    fail "cannot connect to $GUEST_IP:4222 — bound to loopback, or the server is not accepting"
fi
if ! read -r -t 5 line <&3; then
    exec 3<&- 3>&-
    fail "connected to $GUEST_IP:4222 but the server sent no INFO — it is not healthy"
fi
exec 3<&- 3>&-
case "$line" in
    INFO\ *) ok "client port 4222 answers with INFO on $GUEST_IP" ;;
    *)       fail "unexpected greeting on 4222: ${line:0:60}" ;;
esac

# The monitoring endpoint, which is what app-lb health-checks. Same reasoning:
# request it at the guest_ip, over the port the deployment's `vm.port` names.
if ! { exec 3<>"/dev/tcp/$GUEST_IP/8222"; } 2>/dev/null; then
    fail "cannot connect to $GUEST_IP:8222 — app-lb's health check will never pass"
fi
printf 'GET /healthz HTTP/1.0\r\nHost: %s\r\n\r\n' "$GUEST_IP" >&3
status=""
read -r -t 5 status <&3 || true
exec 3<&- 3>&-
case "$status" in
    *"200"*) ok "monitoring endpoint returns 200 for /healthz on $GUEST_IP:8222" ;;
    "")      fail "no response from $GUEST_IP:8222/healthz" ;;
    *)       fail "unexpected status from /healthz: ${status%$'\r'}" ;;
esac

echo
echo "preflight passed"
