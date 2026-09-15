#!/bin/sh
# PID 1 for the heyo-mcp microVM (no systemd). The kernel boots this via
# init=/init.sh. It sets up the environment, firewalls the listener, starts
# sshd, prints the HEYVM_READY marker, and never exits.
#
# The node process is NOT started here. app-lb launches it after boot with
# `start_command`, which is the only channel carrying this deployment's env
# vars — the credentials and the three service URLs reach that process and not
# this script.

mount -t proc proc /proc
mount -t sysfs sysfs /sys

# Populate /dev via devtmpfs. A docker-exported rootfs has an empty /dev, so
# sshd would fail without device nodes. Fall back to manual mknod.
mount -t devtmpfs devtmpfs /dev 2>/dev/null
if [ ! -c /dev/null ]; then
    echo "init: devtmpfs unavailable, creating device nodes manually"
    mknod -m 666 /dev/null    c 1 3
    mknod -m 666 /dev/zero    c 1 5
    mknod -m 444 /dev/random  c 1 8
    mknod -m 444 /dev/urandom c 1 9
    mknod -m 666 /dev/tty     c 5 0
    mknod -m 666 /dev/ptmx    c 5 2
    ln -sf /proc/self/fd /dev/fd
fi
mkdir -p /dev/pts && mount -t devpts devpts /dev/pts

dmesg -n 1 2>/dev/null
echo "nameserver 8.8.8.8" > /etc/resolv.conf
hostname heyo-mcp

# Network: the kernel ip= param may not be fully applied before init runs. GW is
# also the host, which the firewall below needs, so it is captured either way.
GW=""
ip link set eth0 up 2>/dev/null
for param in $(cat /proc/cmdline); do
    case "$param" in
        ip=*)
            GUEST_IP="${param#ip=}"; GUEST_IP="${GUEST_IP%%::*}"
            TAIL="${param#*::}"; GW="${TAIL%%:*}"
            if ! ip addr show eth0 2>/dev/null | grep -q "inet "; then
                ip addr add "$GUEST_IP/30" dev eth0 2>/dev/null
                [ -n "$GW" ] && ip route add default via "$GW" dev eth0 2>/dev/null
            fi
            ;;
    esac
done
[ -n "$GW" ] || GW="$(ip route show default 2>/dev/null | awk '/default/ {print $3; exit}')"

# The listener is firewalled to the host, and this is the rule that replaces a
# loopback bind.
#
# The supervisord deployment this image replaces bound 127.0.0.1 so that the
# only way in was through app-lb's gate. A VM cannot do that: app-lb reaches the
# guest over the tap, so the server must bind 0.0.0.0 — and heyvmd installs a
# blanket `-s 172.16.0.0/12 -d 172.16.0.0/12 -j ACCEPT` in the host's FORWARD
# chain (mvm-ctrl/src/driver/tap_networking.rs:509-534), so *every other VM on
# this host, customer sandboxes included*, can route to this guest's address.
# Without the rules below, port 9650 would be reachable from all of them with no
# gate in front — the exact hole the loopback bind existed to close, relocated.
#
# $GW is the host end of this VM's /30 (host_ip = base+1), which is where both
# app-lb's proxy and `heyvm exec` connect from. Everything else is dropped.
#
# Best-effort: iptables here is nft-backed and a kernel without the modules
# would fail. A boot that cannot firewall itself is not worth wedging, so this
# logs and continues — the real defence is that this deployment carries no
# fleet-wide app-lb credential to steal (see deployments/us2/mcp.json), and
# these rules are the second layer, not the first.
if [ -n "$GW" ] && command -v iptables >/dev/null 2>&1; then
    # Order matters, and so does the conntrack rule specifically. Every tool
    # call this server makes is an *outbound* HTTPS request to
    # admin/obs/ci.us2.heyo.work, and the replies arrive on INPUT. Setting the
    # policy to DROP without RELATED,ESTABLISHED would leave the listener
    # perfectly reachable and every tool broken — so that rule is the gate for
    # the whole block, not one line inside it.
    iptables -F INPUT 2>/dev/null
    if iptables -A INPUT -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null; then
        iptables -A INPUT -i lo -j ACCEPT
        # ICMP from the host only, so it can still probe reachability.
        iptables -A INPUT -p icmp -s "$GW" -j ACCEPT
        # The MCP listener and the heyvm shell, both host-only.
        iptables -A INPUT -p tcp --dport 9650 -s "$GW" -j ACCEPT
        iptables -A INPUT -p tcp --dport 22   -s "$GW" -j ACCEPT
        iptables -P INPUT DROP
        echo "init: INPUT firewalled to $GW (tcp/9650, tcp/22)"
    else
        # No conntrack match available. Staying open is the lesser failure: the
        # credential posture above is the primary defence, and a VM that boots
        # but can reach nothing is a deployment that never becomes healthy.
        iptables -P INPUT ACCEPT 2>/dev/null
        echo "init: WARNING no conntrack match; leaving INPUT open — tcp/9650 is reachable from every VM on this host"
    fi
else
    echo "init: WARNING no gateway or no iptables; tcp/9650 is reachable from every VM on this host"
fi

# sshd for `heyvm exec` / `heyvm sh`. Log to a file, never to the serial console,
# which carries the marker-delimited command protocol.
mkdir -p /run/sshd
chown root:root /run/sshd
chmod 755 /run/sshd
chown root:root /etc/ssh/ssh_host_* 2>/dev/null
chmod 600 /etc/ssh/ssh_host_*_key 2>/dev/null
chmod 644 /etc/ssh/ssh_host_*_key.pub 2>/dev/null
/usr/sbin/sshd -D -e 2>/tmp/sshd.log &

echo "HEYVM_READY"

# Keep PID 1 alive with an interactive shell for `heyvm sh`. The loop survives
# the user exiting the shell (a PID 1 exit would panic the kernel).
while :; do /bin/bash --login; sleep 0.1; done
