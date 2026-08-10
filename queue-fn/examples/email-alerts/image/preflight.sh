#!/bin/sh
# Check, from inside the VM, that the image can actually run the function.
#
#   heyvm exec <sandbox> /opt/email-alerts/preflight.sh
#
# Every check here corresponds to a way the function fails that looks like
# something else from the outside: a missing CA bundle reads as "SMTP is down",
# an unmounted /dev/shm reads as "the cache is not working", a missing default
# route reads as "HeyoSecret is unreachable". Checking them directly turns a
# 3am guess into a one-line answer.
#
# Reads no secrets and sends no mail — it is safe to run against a VM that is
# serving live alerts.
FAIL=0

ok()   { echo "ok    $1"; }
bad()  { echo "FAIL  $1"; FAIL=1; }

command -v python3 >/dev/null 2>&1 \
    && ok "python3: $(python3 -V 2>&1)" \
    || bad "python3 is missing; every invocation would exit 127"

[ -r /opt/email-alerts/fanout.py ] \
    && ok "fanout.py installed" \
    || bad "/opt/email-alerts/fanout.py is missing"

python3 -c "import smtplib, ssl, email.message, urllib.request, json, base64, hashlib" 2>/dev/null \
    && ok "stdlib modules import" \
    || bad "a stdlib module the function needs is unavailable"

python3 -c "
import ssl, sys
ctx = ssl.create_default_context()
stats = ctx.cert_store_stats()
sys.exit(0 if stats['x509_ca'] > 0 else 1)
" 2>/dev/null \
    && ok "CA trust store populated (STARTTLS can verify)" \
    || bad "no CA certificates; STARTTLS fails at handshake on every send"

# fanout.py caches secrets here, and the mount is what keeps a decrypted value
# off the rootfs.
if mountpoint -q /dev/shm 2>/dev/null || grep -q " /dev/shm tmpfs " /proc/mounts; then
    ok "/dev/shm is tmpfs (secret cache stays off disk)"
else
    bad "/dev/shm is not a tmpfs mount; the secret cache would write to disk"
fi

# The route fanout.py reads to find the host when ALERT_SECRETS_URL is unset.
# Deliberately calls fanout.py's own default_gateway() rather than
# re-implementing the /proc/net/route parse: a second implementation that agrees
# with the first is worth nothing, and one that disagrees is worse than nothing.
# (An earlier version of this check used awk's strtonum(), which is a gawk
# extension — Ubuntu ships mawk, so it reported "no default route" on a VM whose
# routing was fine.)
GW=$(python3 -c "
import sys
sys.path.insert(0, '/opt/email-alerts')
import fanout
print(fanout.default_gateway())
" 2>/dev/null)
[ -n "$GW" ] \
    && ok "default gateway: $GW (HeyoSecret is reached here unless ALERT_SECRETS_URL is set)" \
    || bad "no default route; the guest cannot reach the host"

[ -s /etc/resolv.conf ] \
    && ok "resolver: $(awk '/^nameserver/ {print $2; exit}' /etc/resolv.conf)" \
    || bad "/etc/resolv.conf is empty; an SMTP host named by DNS will not resolve"

if [ "$FAIL" -eq 0 ]; then
    echo "preflight: all checks passed"
else
    echo "preflight: FAILED — see above"
fi
exit "$FAIL"
