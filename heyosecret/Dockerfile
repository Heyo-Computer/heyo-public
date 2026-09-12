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
RUN cargo test --locked && cargo build --release --locked

# --- runtime rootfs ---
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    iproute2 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# SSH accepts installed public keys only. The serial command protocol does not
# require SSH passwords. Generate host keys without waiting at guest boot.
RUN mkdir -p /run/sshd /etc/ssh/sshd_config.d \
    && printf 'PermitRootLogin prohibit-password\nPermitEmptyPasswords no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n' > /etc/ssh/sshd_config.d/00-heyo.conf \
    && passwd -d root \
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
