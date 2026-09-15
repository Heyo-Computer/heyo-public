# Firecracker rootfs for heyo-mcp.
#
# Built from the `mcp/` subtree, not the repo root — the app-lb `build` block in
# deployments/us2/mcp.json pairs "dockerfile": "mcp/deploy/image/Dockerfile"
# with "context": "mcp", so every COPY below is relative to mcp/. Locally:
#
#   heyvm mvm build --local-only -f deploy/image/Dockerfile -c . -n heyo-mcp
#
# `heyvm mvm build` runs docker build -> docker create -> docker export ->
# mke2fs. Only the *filesystem* survives that: ENV, CMD, ENTRYPOINT, WORKDIR and
# USER are image config and are dropped. The kernel boots init=/init.sh, and the
# node process itself is started afterwards by app-lb's `start_command`, which is
# the only channel that carries this deployment's env vars. The CMD at the bottom
# is documentation.
#
# There is no data disk (`vm.disk_size_gb` is unset) and no workspace: this
# server holds no state between requests — `serve-http.ts` builds a fresh
# transport per call — so a rootfs recopied on every cold boot loses nothing.

# --- build stage ---------------------------------------------------------
# Full image, not -slim: `npm ci` compiles nothing here today, but the toolchain
# staying available is what keeps a future native dependency from turning into a
# build failure nobody can read.
FROM node:22-bookworm AS builder
WORKDIR /build

# Lockfile first so a dependency change is the only thing that busts this layer.
COPY package.json package-lock.json ./
RUN npm ci

COPY tsconfig.json ./
COPY src ./src
RUN npm run build

# The tests compile into dist/ alongside the server (tsconfig `include` is
# src/**/*.ts). Run them here: a rootfs is a bad place to discover that
# withForwardedAuth stopped preferring a caller's applb_ token, because the
# symptom in production is not an error — it is this process quietly acting with
# more authority than the caller had.
RUN node --test dist/*.test.js

# Runtime dependencies only. Done after the build so devDependencies (typescript,
# @types/node) never reach the rootfs.
RUN npm ci --omit=dev

# --- runtime rootfs ------------------------------------------------------
# node:22-bookworm-slim rather than ubuntu:24.04 (which the artifacts and nats
# images use): those ship their runtime as a single static-ish binary, while this
# one needs a full node. Taking Debian's node image is more honest than copying
# /usr/local/bin/node onto a foreign glibc and hoping.
FROM node:22-bookworm-slim

# iproute2 brings eth0 up — the kernel's ip= parameter sets the address but
# nothing raises the link. ca-certificates is load-bearing here and not
# incidental: unlike the loopback deployment this replaces, every call this
# server makes is now HTTPS to admin/obs/ci.us2.heyo.work, so an image without a
# trust store fails every tool with a TLS error. openssh-server is the heyvm
# convention for `heyvm exec` / `heyvm sh`. iptables backs the INPUT rules in
# init.sh. curl is for the smoke test below and for debugging from inside.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        iproute2 \
        iptables \
        openssh-server \
    && rm -rf /var/lib/apt/lists/*

# SSH host keys at build time, not boot: a microVM has little entropy and
# ssh-keygen would be paid on every cold start. Debian ships password auth
# disabled and there is no cloud-init here to re-enable it, so it is explicit.
# init.sh firewalls port 22 to the host, which is what keeps `heyo:heyo` from
# being an open door to the other VMs sharing this bridge.
RUN mkdir -p /run/sshd /etc/ssh/sshd_config.d \
    && echo "PermitRootLogin yes" >> /etc/ssh/sshd_config \
    && echo "PermitEmptyPasswords yes" >> /etc/ssh/sshd_config \
    && echo "PasswordAuthentication yes" > /etc/ssh/sshd_config.d/50-heyo.conf \
    && chmod 644 /etc/ssh/sshd_config.d/50-heyo.conf \
    && passwd -d root \
    && useradd -m -s /bin/bash heyo \
    && echo 'heyo:heyo' | chpasswd \
    && ssh-keygen -A

COPY --from=builder /build/dist /opt/heyo-mcp/dist
COPY --from=builder /build/node_modules /opt/heyo-mcp/node_modules
COPY --from=builder /build/package.json /opt/heyo-mcp/package.json

COPY deploy/image/init.sh /init.sh
RUN chmod +x /init.sh

# Prove the rootfs serves before it ships. From app-lb's side a server that
# cannot start is indistinguishable from one that is misconfigured: both are a VM
# that boots, prints HEYVM_READY, and never answers /healthz.
#
# Deliberately no credentials. `/healthz` is answered before any client is
# touched (serve-http.ts), so this checks that node, the SDK and the module graph
# load — not how the thing is gated in production.
RUN set -eu; \
    HEYO_MCP_HTTP_PORT=9650 HEYO_MCP_HTTP_HOST=127.0.0.1 \
        node /opt/heyo-mcp/dist/index.js >/tmp/smoke.log 2>&1 & \
    pid=$!; ok=0; \
    for _ in $(seq 60); do \
        if curl -fsS -m 1 http://127.0.0.1:9650/healthz >/dev/null 2>&1; then ok=1; break; fi; \
        kill -0 "$pid" 2>/dev/null || break; \
        sleep 0.25; \
    done; \
    kill "$pid" 2>/dev/null || true; \
    if [ "$ok" != 1 ]; then \
        echo "smoke test: heyo-mcp never answered GET /healthz"; cat /tmp/smoke.log; exit 1; \
    fi; \
    rm -f /tmp/smoke.log

# Documentation only — docker export drops both. 9650 is what app-lb proxies and
# health-checks; 22 is the heyvm shell.
EXPOSE 22 9650

CMD ["/init.sh"]
