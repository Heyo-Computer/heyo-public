#!/bin/sh
# PID 1 for the NATS microVM.
#
# Firecracker boots straight into this via `init=/init.sh` on the kernel command
# line — there is no systemd, no cloud-init and no container runtime. Nothing is
# mounted, nothing is configured, and `/dev` from a Docker-exported rootfs is
# empty. Everything below has to happen before the VM is usable.
#
# The one hard contract with the host: print `HEYVM_READY` on the serial console
# when the VM is up, then leave a shell reading from it. heyvmd drives commands
# by writing `echo <START>; (cmd) 2>&1; echo <END> $?` into that shell and
# reading back the lines between the markers — that is how `serverctl exec` and
# app-lb's own exec path work.
#
# Anything printed to the serial console after HEYVM_READY that is not part of a
# marked command lands in the middle of that protocol, so nats-server's output
# is redirected to a file, never inherited.

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null

# A Docker-exported rootfs has an empty /dev. If devtmpfs is unavailable for any
# reason, sshd needs these nodes to exist — and so does nats-server, which reads
# /dev/urandom to seed the nonces it hands every connecting client.
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

mkdir -p /tmp && chmod 1777 /tmp

# Kernel messages would otherwise interleave with the serial command protocol.
dmesg -n 1 2>/dev/null

if [ -f /etc/heyo/resolv.conf ]; then
    cp /etc/heyo/resolv.conf /etc/resolv.conf
else
    echo "nameserver 8.8.8.8" > /etc/resolv.conf
fi

hostname nats

# The kernel's `ip=` parameter configures eth0's address and default route, but
# nothing brings the link up. The fallback re-parses the same parameter by hand
# for the case where the interface was not fully up by the time init ran.
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

# --- The data disk --------------------------------------------------------
#
# This is the part that makes the VM a broker rather than a cache. mvm-ctrl
# attaches the persistent disk as /dev/vdb and creates it as a *raw, unformatted
# sparse file*, explicitly leaving the filesystem to the guest
# (mvm-ctrl/src/driver/firecracker.rs:1760). It also recopies the rootfs from
# the base image on every cold boot, so /workspace is the only writable path
# whose contents outlive this boot — and JetStream's store is on it.
#
# Formatting is therefore first-boot-only and must be decided by inspection, not
# by a flag file (a flag file would live on the rootfs and vanish, and we would
# reformat the store on every boot).
DATA_READY=0
if [ -b /dev/vdb ]; then
    if ! blkid /dev/vdb >/dev/null 2>&1; then
        echo "init: /dev/vdb has no filesystem, creating ext4 (first boot)"
        # -F: the device is a raw sparse file with no partition table, which
        # mke2fs would otherwise stop to ask about on a non-interactive console.
        # -m0: no root reserve. This filesystem has exactly one consumer and
        # 5% of a 20GiB disk is a gigabyte JetStream could have used.
        mkfs.ext4 -F -m0 -L heyo-data /dev/vdb >/tmp/mkfs.log 2>&1 \
            || echo "init: mkfs.ext4 failed, see /tmp/mkfs.log"
    fi

    mkdir -p /workspace
    if mount -t ext4 -o rw,noatime /dev/vdb /workspace 2>/tmp/mount.log; then
        DATA_READY=1
    else
        echo "init: mounting /dev/vdb on /workspace failed, see /tmp/mount.log"
    fi
else
    echo "init: no /dev/vdb — the deployment is missing vm.disk_size_gb"
fi

# --- sshd -----------------------------------------------------------------
#
# What `serverctl shell` and `serverctl exec` use. /run does not survive the
# export→ext4 conversion as a populated directory, so the privilege-separation
# dir is recreated here even though the Dockerfile also makes it.
mkdir -p /run/sshd
chown root:root /run/sshd
chmod 755 /run/sshd
chown root:root /etc/ssh/ssh_host_* 2>/dev/null
chmod 600 /etc/ssh/ssh_host_*_key 2>/dev/null
chmod 644 /etc/ssh/ssh_host_*_key.pub 2>/dev/null
# Errors to a file, never to stderr: stray output on the serial console corrupts
# the marker protocol the host drives commands with.
/usr/sbin/sshd -D -e 2>/tmp/sshd.log &

# --- nats-server ----------------------------------------------------------
#
# Started only when /workspace is a real mount. Refusing to start is the whole
# point: a NATS that comes up with its store on the rootfs passes every health
# check, serves traffic correctly for as long as the VM lives, and loses the
# entire stream the first time app-lb recreates it. A VM that fails its health
# check is visible in `serverctl get deployments` within a minute; silent
# non-durability is discovered when something needs the queue to have survived.
if [ "$DATA_READY" = "1" ]; then
    mkdir -p /workspace/jetstream /workspace/log

    # An operator-supplied fragment on the data disk — credentials, extra
    # accounts, a leafnode block. On the disk rather than in the image so a
    # secret is never baked into a rootfs that gets copied between hosts. The
    # generated config lives on tmpfs because it may now contain that secret.
    CONF=/etc/nats/nats-server.conf
    if [ -f /workspace/nats/auth.conf ]; then
        echo "init: including /workspace/nats/auth.conf"
        mkdir -p /run/nats
        chmod 700 /run/nats
        cp /etc/nats/nats-server.conf /run/nats/nats-server.conf
        # The fragment is copied next to the generated config and included by a
        # *relative* name. nats-server resolves an include against the including
        # file's directory even when the path is absolute — an
        # `include "/workspace/nats/auth.conf"` from /run/nats is looked up at
        # /run/nats/workspace/nats/auth.conf and the server exits 1 on boot.
        cp /workspace/nats/auth.conf /run/nats/auth.conf
        printf '\ninclude "auth.conf"\n' >> /run/nats/nats-server.conf
        chmod 600 /run/nats/nats-server.conf /run/nats/auth.conf
        CONF=/run/nats/nats-server.conf
    fi

    # Output goes to the config's log_file; this redirect only catches what the
    # server writes before it has parsed that far — a config error, most often.
    /usr/local/bin/nats-server -c "$CONF" >/tmp/nats-boot.log 2>&1 &
else
    echo "init: refusing to start nats-server without a mounted /workspace;" \
         "JetStream would write to the rootfs and lose every stream on the next boot"
fi

echo "HEYVM_READY"

# A loop, not `exec /bin/sh`. The serial console is the only exec channel this
# VM has; if the shell ever exits — a command that calls `exit`, an EOF on the
# console — `exec` would leave the VM alive with no way to run anything in it,
# and app-lb would keep routing to a black hole until the TTL reaped it.
while :; do /bin/bash --login; sleep 0.1; done
