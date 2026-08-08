# Firecracker rootfs for a NATS server with JetStream.
#
# Built with `heyvm mvm build`, which does NOT run this as a container. It runs
# `docker build`, then `docker create` + `docker export`, then `mke2fs -d` over
# the exported tree. Two consequences shape everything below:
#
#   1. **Only the filesystem survives.** `docker export` writes a tar of the
#      container's files and nothing else — ENV, CMD, ENTRYPOINT, WORKDIR and
#      USER are image *config*, and they are dropped on the floor. Nothing here
#      may depend on them. The `CMD` at the bottom is documentation; the real
#      entry point is the kernel's `init=/init.sh`.
#   2. **There is no runtime to fix anything up.** No systemd, no cloud-init,
#      no docker-entrypoint. `/proc`, `/sys`, `/dev`, the network and the data
#      disk are all init.sh's problem.
#
# And one that is specific to a *stateful* service, which is the whole reason
# this image is not just `FROM nats`:
#
#   3. **Writes to the rootfs do not survive a cold boot.** mvm-ctrl recopies
#      the rootfs from the base image every time the VM boots
#      (`mvm-ctrl/src/driver/firecracker.rs:1763`). The only durable storage is
#      the data disk, attached as `/dev/vdb` and mounted by init.sh at
#      /workspace. The JetStream store directory is therefore under /workspace
#      and nowhere else — see image/nats-server.conf. A default `store_dir` on
#      the rootfs would work perfectly in testing and silently discard every
#      stream the first time the VM cold-booted.
#
# Build (from the example root, so the context contains image/):
#
#     ./build-image.sh
#     # or: heyvm mvm build --local-only -f image/Dockerfile -c . -n nats
#
# The result lands at ~/.heyo/images/firecracker/nats.ext4, which is what
# `vm.image` in nats.json names.

# The official binary, rather than a distro package: there is no nats-server in
# Ubuntu's archive, and the alpine image is the smallest thing that carries an
# authentic build. It is a statically linked Go ELF, so it runs unmodified on
# the glibc userland below — verified with `file`, not assumed.
#
# Pinned to a patch version on purpose. `nats:latest` would let a major-version
# change ride in on an unrelated rebuild, and JetStream's on-disk store lives on
# a data disk that outlives the rootfs — so the new binary would meet the old
# server's files with no upgrade step in between.
FROM nats:2.11.17-alpine AS upstream

FROM ubuntu:24.04

COPY --from=upstream /usr/local/bin/nats-server /usr/local/bin/nats-server

# iproute2 is what init.sh uses to bring eth0 up; the kernel's `ip=` parameter
# configures the address, but nothing brings the link up on its own.
#
# openssh-server is the heyvm convention: it is how `serverctl shell` and
# `serverctl exec` get into the VM, which is how you inspect a broker that is
# up but not accepting.
#
# e2fsprogs is load-bearing and not incidental. The data disk arrives as a raw,
# *unformatted* sparse file — mvm-ctrl creates it with `set_len` and explicitly
# leaves `mkfs` to the guest ("init.sh finds no filesystem and runs mkfs.ext4
# itself", firecracker.rs:1760). Without this package the first boot cannot
# create the filesystem, /workspace stays unmounted, and JetStream silently
# writes its store onto the rootfs — where the next cold boot discards it.
# noble already ships mkfs.ext4, but a dependency init.sh cannot run without is
# named here rather than inherited by luck.
#
# ca-certificates is only needed if this server ever dials out over TLS — a
# leafnode or gateway link to another cluster. Cheap enough to include, and its
# absence fails at handshake time on a link you configured months earlier.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        e2fsprogs \
        iproute2 \
        openssh-server \
    && rm -rf /var/lib/apt/lists/*

# SSH access, per the heyvm image convention. Host keys are generated at build
# time because generating them at boot is slow on a microVM with little entropy,
# and it would be paid on every cold start. Ubuntu 24.04 ships with password
# auth disabled in a drop-in and there is no cloud-init here to re-enable it, so
# it is set explicitly.
RUN mkdir -p /run/sshd /etc/ssh/sshd_config.d \
    && echo "PermitRootLogin yes" >> /etc/ssh/sshd_config \
    && echo "PermitEmptyPasswords yes" >> /etc/ssh/sshd_config \
    && echo "PasswordAuthentication yes" > /etc/ssh/sshd_config.d/50-heyo.conf \
    && chmod 644 /etc/ssh/sshd_config.d/50-heyo.conf \
    && passwd -d root \
    && useradd -m -s /bin/bash heyo \
    && echo 'heyo:heyo' | chpasswd \
    && ssh-keygen -A

# The guest's resolver, which init.sh copies over /etc/resolv.conf at boot.
# There is no DHCP on the tap network to learn one from. A plain NATS server
# resolves nothing, but a leafnode or gateway link names its remote by host.
#
# A COPY'd file rather than an `ARG` + `--build-arg`, because `heyvm mvm build`
# shells out to `docker build -t … -f … <ctx>` and passes no build args through —
# an ARG here would silently keep its default no matter what was set. Change it
# by editing image/resolv.conf, or let build-image.sh rewrite it from DNS_SERVER.
#
# It also cannot simply be written to /etc/resolv.conf at build time: Docker
# bind-mounts that path during a build, so what lands in the exported layer is
# unreliable. Staging it elsewhere and copying at boot is deterministic.
COPY image/resolv.conf /etc/heyo/resolv.conf

# The server configuration. Read-only at runtime and identical on every replica,
# so it belongs on the rootfs; only the *store* goes on the data disk.
COPY image/nats-server.conf /etc/nats/nats-server.conf

# Callable from the host with:  serverctl exec nats -- /opt/nats/preflight.sh
COPY image/preflight.sh /opt/nats/preflight.sh
RUN chmod +x /opt/nats/preflight.sh

# PID 1. Copied rather than heredoc'd so it stays readable and reviewable; the
# kernel command line points at this exact path.
COPY image/init.sh /init.sh
RUN chmod +x /init.sh

# Build-time proof that the pieces this image depends on are present, and that
# the config parses. Each has a failure mode that otherwise only appears once
# the VM is booted and app-lb is waiting on a health check that will never pass:
# a config typo makes nats-server exit 1 immediately, and a missing mkfs.ext4
# makes the store silently non-durable.
#
# `--signal reload=0`-style dry runs are not a thing, so `-t` is used: it parses
# the config, reports errors, and exits without binding a port.
RUN test -x /init.sh \
    && test -x /usr/local/bin/nats-server \
    && test -x /usr/sbin/mkfs.ext4 \
    && /usr/local/bin/nats-server --help >/dev/null \
    && /usr/local/bin/nats-server -c /etc/nats/nats-server.conf -t \
    && echo "image checks passed"

# Documentation only — docker export drops both, and neither is consulted at
# boot. 4222 is the client protocol; 8222 is the HTTP monitoring endpoint that
# app-lb proxies and health-checks. Listed so the image reads correctly to
# `docker inspect` and to anyone who runs it as a container to poke around.
EXPOSE 4222 8222
CMD ["/init.sh"]
