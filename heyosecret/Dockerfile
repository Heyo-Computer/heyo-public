# Firecracker microVM rootfs for HeyoSecret.
#
# Built with:  heyvm mvm build --local-only -f Dockerfile -n heyosecret --size-mb 512
#
# The heyvm build pipeline is docker build -> docker export -> mke2fs, which
# discards OCI metadata (ENTRYPOINT/CMD/ENV are NOT used). The VM boots straight
# into /init.sh via the kernel init= parameter. x86_64 only.
#
# HeyoSecret keeps no local state — every secret lives in Postgres — so this
# image needs no persistent data disk. It does need outbound network to reach
# the database.

# --- build stage ---
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release --locked || cargo build --release

# --- runtime rootfs ---
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    iproute2 \
    ca-certificates \
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

COPY --from=builder /app/target/release/heyosecret /usr/local/bin/heyosecret

# Migrations are read from disk at startup (store::run_migrations), not embedded
# in the binary. The compile-time CARGO_MANIFEST_DIR fallback points at the
# builder stage's /app, which does not exist here — so they are installed at a
# fixed path and the deployment sets HEYOSECRET_MIGRATIONS_DIR to it. Keeping
# them under the working directory also satisfies the plain `./migrations`
# lookup when the process is started from /opt/heyosecret.
COPY migrations /opt/heyosecret/migrations

COPY init.sh /init.sh
RUN chmod +x /init.sh /usr/local/bin/heyosecret

EXPOSE 22 4455

CMD ["/init.sh"]
