# Firecracker microVM rootfs for HeyoSecret.
#
# Built from the **repository root**, not this directory:
#
#   heyvm mvm build --local-only -f heyosecret/Dockerfile -n heyosecret --size-mb 512
#
# and an app-lb `build` block needs `"context": "."` beside
# `"dockerfile": "heyosecret/Dockerfile"` for the same reason: `src/main.rs`
# includes the shared UI kit as `#[path = "../../ui/ui.rs"]`, so the context
# must hold `ui/` as well as this crate, and the crate must sit one level below
# it in the builder for that relative path to land. Without both, the build
# dies with `couldn't read src/../../ui/ui.rs`.
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
WORKDIR /app/heyosecret
# The shared UI module, at the path `src/main.rs` reaches for: /app/ui.
COPY ui /app/ui
COPY heyosecret/Cargo.toml heyosecret/Cargo.lock* ./
COPY heyosecret/src ./src
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

COPY --from=builder /app/heyosecret/target/release/heyosecret /usr/local/bin/heyosecret

# Migrations are read from disk at startup (store::run_migrations), not embedded
# in the binary. The compile-time CARGO_MANIFEST_DIR fallback points at the
# builder stage's /app/heyosecret, which does not exist here — so they are installed at a
# fixed path and the deployment sets HEYOSECRET_MIGRATIONS_DIR to it. Keeping
# them under the working directory also satisfies the plain `./migrations`
# lookup when the process is started from /opt/heyosecret.
COPY heyosecret/migrations /opt/heyosecret/migrations

COPY heyosecret/init.sh /init.sh
RUN chmod +x /init.sh /usr/local/bin/heyosecret

EXPOSE 22 4455

CMD ["/init.sh"]
