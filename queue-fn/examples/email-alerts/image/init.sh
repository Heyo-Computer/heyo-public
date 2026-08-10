#!/bin/sh
# PID 1 for the email-alerts microVM.
#
# Firecracker boots straight into this via `init=/init.sh` on the kernel command
# line — there is no systemd, no cloud-init, and no container runtime. Nothing
# is mounted, nothing is configured, and `/dev` from a Docker-exported rootfs is
# empty. Everything below has to happen before the VM is usable.
#
# The one hard contract with the host: print `HEYVM_READY` on the serial console
# when the VM is up, then leave a shell reading from it. heyvmd drives commands
# by writing `echo <START>; (cmd) 2>&1; echo <END> $?` into that shell and
# reading back the lines between the markers, which is how queue-fn's dispatcher
# invokes the function.
#
# Anything printed to the serial console after HEYVM_READY that is not part of a
# marked command lands in the middle of that protocol, so background services
# get their output redirected, not inherited.

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null

# A Docker-exported rootfs has an empty /dev. If devtmpfs is unavailable for any
# reason, sshd and Python's ssl module both need these nodes to exist.
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

# fanout.py caches fetched secrets here so a VM warm through a burst of alerts
# does not re-read the same two secrets once per alert. tmpfs specifically: a
# decrypted secret must never reach the rootfs image. Without this mount the
# cache silently writes to the underlying directory on disk, so it is not
# optional — it is the reason the cache is safe.
mkdir -p /dev/shm && mount -t tmpfs -o mode=1777,nosuid,nodev tmpfs /dev/shm

mkdir -p /tmp && chmod 1777 /tmp

# Kernel messages would otherwise interleave with the serial command protocol.
dmesg -n 1 2>/dev/null

# Baked at build time from the DNS_SERVER build arg. An alerting VM usually has
# to resolve an internal SMTP relay, and there is no DHCP here to learn a
# resolver from.
if [ -f /etc/heyo/resolv.conf ]; then
    cp /etc/heyo/resolv.conf /etc/resolv.conf
else
    echo "nameserver 8.8.8.8" > /etc/resolv.conf
fi

hostname email-alerts

# The kernel's `ip=` parameter configures eth0 and installs the default route —
# which is what fanout.py reads out of /proc/net/route to find the host, since
# heyvm gives every VM its own /30 and the host's address is therefore not a
# constant. The fallback re-parses the same parameter by hand for the case where
# the interface was not fully up by the time init ran.
ip link set eth0 up 2>/dev/null
if ! ip addr show eth0 2>/dev/null | grep -q "inet "; then
    for param in $(cat /proc/cmdline); do
        case "$param" in
            ip=*)
                GUEST_IP="${param#ip=}"; GUEST_IP="${GUEST_IP%%::*}"
                TAIL="${param#*::}"; GW="${TAIL%%:*}"
                ip addr add "$GUEST_IP/30" dev eth0 2>/dev/null
                [ -n "$GW" ] && ip route add default via "$GW" dev eth0 2>/dev/null
                ;;
        esac
    done
fi

# sshd is what `heyvm exec` and `heyvm sh` use. /run is a fresh tmpfs-less
# directory after the export→ext4 conversion, so the privilege-separation dir
# has to be recreated here even though the Dockerfile also makes it.
mkdir -p /run/sshd
chown root:root /run/sshd
chmod 755 /run/sshd
chown root:root /etc/ssh/ssh_host_* 2>/dev/null
chmod 600 /etc/ssh/ssh_host_*_key 2>/dev/null
chmod 644 /etc/ssh/ssh_host_*_key.pub 2>/dev/null
# Errors to a file, never to stderr: stray output on the serial console corrupts
# the marker protocol that queue-fn's dispatcher depends on.
/usr/sbin/sshd -D -e 2>/tmp/sshd.log &

# No long-running application service. The function is a command, not a daemon —
# queue-fn runs `python3 /opt/email-alerts/fanout.py` per invocation, and having
# nothing resident is the point: a VM that is idle is costing only its memory.

echo "HEYVM_READY"

# A loop, not `exec /bin/sh`. The serial console is the only exec channel this
# VM has; if the shell ever exits — a command that calls `exit`, an EOF on the
# console — `exec` would leave the VM alive with no way to run anything in it,
# and the pool would keep dispatching to a black hole until the TTL reaped it.
while :; do /bin/bash --login; sleep 0.1; done
