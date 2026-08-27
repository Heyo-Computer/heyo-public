# Firecracker microVM rootfs for the artifacts store (`art serve`).
#
# Built from the **repository root**, not this directory:
#
#   heyvm mvm build --local-only -f artifacts/Dockerfile -n artifacts --size-mb 768
#
# and an app-lb `build` block needs `"context": "."` beside
# `"dockerfile": "artifacts/Dockerfile"` for the same reason: `src/lib.rs`
# includes the shared UI kit as `#[path = "../../ui/ui.rs"]`, so the context
# must hold `ui/` as well as this crate, and the crate must sit one level below
# it in the builder for that relative path to land. Without both, the build
# dies with `couldn't read src/../../ui/ui.rs`.
#
# The heyvm build pipeline is docker build -> docker export -> mke2fs, which
# discards OCI metadata (ENTRYPOINT/CMD/ENV are NOT used). The VM boots straight
# into /init.sh via the kernel init= parameter. x86_64 only.

# --- build stage ---
FROM rust:1-bookworm AS builder
WORKDIR /app/artifacts
# The shared UI module, at the path `src/lib.rs` reaches for: /app/ui.
COPY ui /app/ui
COPY artifacts/Cargo.toml artifacts/Cargo.lock* ./
COPY artifacts/src ./src
COPY artifacts/tests ./tests
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

COPY --from=builder /app/artifacts/target/release/art /usr/local/bin/art
COPY artifacts/init.sh /init.sh
RUN chmod +x /init.sh /usr/local/bin/art

# Prove the rootfs can actually serve before it ships. From app-lb's side a
# binary that cannot start is indistinguishable from one that was misconfigured:
# both are a VM that boots, prints HEYVM_READY, and never answers /healthz. This
# turns the first class into a build failure with the error attached.
#
# Deliberately no ART_API_KEY and no dashboard: this checks that the daemon binds
# and answers, not how it is gated in production. `/healthz` is open either way.
RUN set -eu; \
    /usr/local/bin/art --help >/dev/null; \
    ART_ROOT=/tmp/smoke ART_LISTEN=127.0.0.1:8080 /usr/local/bin/art serve \
        >/tmp/smoke.log 2>&1 & \
    pid=$!; ok=0; \
    for _ in $(seq 60); do \
        if curl -fsS -m 1 http://127.0.0.1:8080/healthz >/dev/null 2>&1; then ok=1; break; fi; \
        kill -0 "$pid" 2>/dev/null || break; \
        sleep 0.25; \
    done; \
    kill "$pid" 2>/dev/null || true; \
    if [ "$ok" != 1 ]; then \
        echo "smoke test: art serve never answered GET /healthz"; cat /tmp/smoke.log; exit 1; \
    fi; \
    rm -rf /tmp/smoke /tmp/smoke.log

EXPOSE 22 8080

CMD ["/init.sh"]
