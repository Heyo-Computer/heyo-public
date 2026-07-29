# Firecracker microVM rootfs for the artifacts store (`art serve`).
#
# Built with:  heyvm mvm build --local-only -f Dockerfile -n artifacts --size-mb 768
#
# The heyvm build pipeline is docker build -> docker export -> mke2fs, which
# discards OCI metadata (ENTRYPOINT/CMD/ENV are NOT used). The VM boots straight
# into /init.sh via the kernel init= parameter. x86_64 only.

# --- build stage ---
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY tests ./tests
# The `daemon` feature is on by default and is the whole point of this image;
# without it there is no listener for app-lb to route to.
RUN cargo build --release --locked || cargo build --release

# --- runtime rootfs ---
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    iproute2 \
    e2fsprogs \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# SSH access for `heyvm exec` / `heyvm sh`. Pre-generate host keys at build time
# so sshd doesn't block on entropy at boot. Ubuntu 24.04 may ship password auth
# disabled, and Firecracker has no cloud-init to fix it — enable it explicitly.
RUN mkdir -p /run/sshd /etc/ssh/sshd_config.d \
    && echo "PermitRootLogin yes" >> /etc/ssh/sshd_config \
    && echo "PermitEmptyPasswords yes" >> /etc/ssh/sshd_config \
    && echo "PasswordAuthentication yes" > /etc/ssh/sshd_config.d/50-heyo.conf \
    && chmod 644 /etc/ssh/sshd_config.d/50-heyo.conf \
    && passwd -d root \
    && useradd -m -s /bin/bash heyo \
    && echo 'heyo:heyo' | chpasswd \
    && ssh-keygen -A

COPY --from=builder /app/target/release/art /usr/local/bin/art
COPY init.sh /init.sh
RUN chmod +x /init.sh /usr/local/bin/art

EXPOSE 22 8080

CMD ["/init.sh"]
