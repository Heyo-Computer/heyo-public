# Firecracker rootfs for the queue-fn email-alerts function.
#
# Built with `heyvm mvm build`, which does NOT run this as a container. It runs
# `docker build`, then `docker create` + `docker export`, then `mke2fs -d` over
# the exported tree. Two consequences shape everything below:
#
#   1. **Only the filesystem survives.** `docker export` writes a tar of the
#      container's files and nothing else — ENV, CMD, ENTRYPOINT, WORKDIR, and
#      USER are all image *config*, and they are dropped on the floor. Nothing
#      here may depend on them. The `CMD` at the bottom is documentation; the
#      real entry point is the kernel's `init=/init.sh`.
#   2. **There is no runtime to fix anything up.** No systemd, no cloud-init, no
#      docker-entrypoint. `/proc`, `/sys`, `/dev`, `/dev/shm` and the network are
#      init.sh's problem, and a package that only works because its postinst ran
#      under a real init will not work here.
#
# Build (from the example root, so the context contains guest/):
#
#     ./build-image.sh
#     # or: heyvm mvm build --local-only -f image/Dockerfile -c . -n email-alerts
#
# The result lands at ~/.heyo/images/firecracker/email-alerts.ext4 and is what
# `vm.image` in function.json names.

FROM ubuntu:24.04

# python3 carries the whole function: smtplib, email, ssl, urllib, json, base64
# and hashlib are all standard library, so there is no pip step and nothing to
# go stale between image builds.
#
# ca-certificates is load-bearing, not incidental. fanout.py connects with
# `ssl.create_default_context()`, which verifies the relay's certificate against
# the system trust store — without this package STARTTLS fails at handshake on
# every single invocation, and it fails identically on the tenth retry.
#
# iproute2 is what init.sh uses to bring eth0 up. openssh-server is the heyvm
# convention: it is how `heyvm exec` and `heyvm sh` get into the VM, which is
# how you debug this thing when an alert does not arrive.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        iproute2 \
        openssh-server \
        python3 \
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

# The guest's resolver, which init.sh copies over /etc/resolv.conf at boot. An
# alerting VM almost always has to resolve an internal SMTP relay by name, and
# there is no DHCP in this network to learn a resolver from.
#
# A COPY'd file rather than an `ARG` + `--build-arg`, because `heyvm mvm build`
# shells out to `docker build -t … -f … <ctx>` and passes no build args through —
# an ARG here would silently keep its default no matter what was set. Change it
# by editing image/resolv.conf, or let build-image.sh rewrite it from
# DNS_SERVER.
#
# It also cannot simply be written to /etc/resolv.conf at build time: Docker
# bind-mounts that path during a build, so what lands in the exported layer is
# unreliable. Staging it elsewhere and copying at boot is deterministic.
COPY image/resolv.conf /etc/heyo/resolv.conf

# The function itself. install.sh is the same script used for any other image
# build, so there is one install path rather than two that can drift — and it
# fails the build on a syntax error or a missing stdlib module instead of
# letting a broken image reach the point where an alert needs it.
COPY guest/fanout.py /tmp/fanout.py
COPY guest/install.sh /tmp/install.sh
RUN chmod +x /tmp/install.sh \
    && /tmp/install.sh /tmp/fanout.py \
    && rm -f /tmp/fanout.py /tmp/install.sh

# Callable from the host with:  heyvm exec <sandbox> /opt/email-alerts/preflight.sh
COPY image/preflight.sh /opt/email-alerts/preflight.sh
RUN chmod +x /opt/email-alerts/preflight.sh

# PID 1. Copied rather than heredoc'd so it stays readable and reviewable; the
# kernel command line points at this exact path.
COPY image/init.sh /init.sh
RUN chmod +x /init.sh

# Build-time proof that the pieces the function depends on are actually present.
# Every one of these has a failure mode that only shows up at 3am otherwise: no
# CA bundle means STARTTLS never completes, no python3 means every invocation
# exits 127 into the DLQ.
RUN test -x /init.sh \
    && test -x /opt/email-alerts/fanout.py \
    && test -s /etc/ssl/certs/ca-certificates.crt \
    && python3 -c "import ssl; ssl.create_default_context()" \
    && echo "image checks passed"

# Documentation only — docker export drops both, and neither is consulted at
# boot. Listed so the image reads correctly to `docker inspect` and to anyone
# who runs it as a container to poke around.
EXPOSE 22
CMD ["/init.sh"]
