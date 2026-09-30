# Queue

The queue service is a read-only dashboard for the NATS server that HWS dispatches work through, and this page also covers running that NATS server itself as a managed microVM behind app-lb.

## What it is

Several HWS components share one `nats-server` with JetStream: [CI](ci.md)'s job queue, function-runner work queues, and a control plane's sandbox stream. The `queue` binary is a small host process that shows what is happening on that server:

- **Queue depth**: every account's streams, how many messages each holds, and how much is pending for each consumer.
- **Throughput**: messages and bytes per second in and out, charted over time.
- **Connected clients**: who is attached, from where, and with what subscriptions.
- **Logs**: a live tail of nats-server's log file.

It also flags the two conditions that usually mean a queue is stuck:

| Flag | Meaning |
| --- | --- |
| `no consumer` | A WorkQueue or Interest stream (messages leave only on ack) with no consumer at all. Nothing will ever drain it |
| `stalled` | A consumer holding work whose ack floor has not moved for longer than its redelivery deadline allows |

Both roll up into a banner at the top of the page.

It runs as a static (`proxy_pass`) deployment behind [app-lb](app-lb.md), the same way [app-obs](app-obs.md) does.

### What it does not do

- **It never connects to NATS as a client.** It only makes HTTP `GET` requests to nats-server's monitoring port (`/varz`, `/connz`, `/jsz`). It cannot publish, subscribe, bind consumers, or shut the server down, and it needs no NATS credential.
- **It persists nothing.** Chart history and the log tail live in memory and are lost on restart. For long-term log retention, send nats-server's logs to [app-obs](app-obs.md).

The monitoring port is unauthenticated, which is why it should stay on loopback. The queue dashboard is how you see that data from outside the host, behind app-lb's sign-in.

## Install and run

```sh
cargo build --release --locked --manifest-path queue/Cargo.toml
sudo install -m0755 queue/target/release/queue /usr/local/bin/queue
sudo useradd --system --no-create-home --shell /usr/sbin/nologin queue
sudo install -d -o queue -g queue /var/lib/queue
```

To run it under supervisord, install [`queue/deploy/supervisor/queue.conf`](../queue/deploy/supervisor/queue.conf):

```sh
sudo cp queue/deploy/supervisor/queue.conf /etc/supervisor/conf.d/
sudo supervisorctl reread && sudo supervisorctl update
```

The unit starts after nats-server (`priority=200`). If it loses that race, the first scrape fails and the next one succeeds.

For the log panel, the `queue` user needs read access to nats-server's log file, for example `usermod -aG nats queue` or a group-readable `/var/log/nats`.

To try it locally against a NATS server with monitoring on port 8222:

```sh
QUEUE_NATS_LOG_FILE=/var/log/nats/nats-server.log \
  cargo run --manifest-path queue/Cargo.toml
# then open http://127.0.0.1:9700/dashboard
```

nats-server must have its HTTP monitoring listener enabled (`http: 127.0.0.1:8222` in its config).

## Configuration

Configuration is environment-only; there is no config file and no CLI flags.

| Variable | Default | Meaning |
| --- | --- | --- |
| `QUEUE_NATS_MONITOR_URL` | `http://127.0.0.1:8222` | Base URL of nats-server's HTTP monitoring listener |
| `QUEUE_API_ADDR` | `127.0.0.1:9700` | Where the dashboard binds |
| `QUEUE_API_TOKEN` | unset | Bearer token for the dashboard and its JSON. Unset leaves them open |
| `QUEUE_NATS_LOG_FILE` | unset | nats-server's log file. Unset disables the log panel |
| `QUEUE_POLL_SECS` | `5` | Scrape interval, and the resolution of every rate |
| `QUEUE_HISTORY_POINTS` | `720` | Samples kept for charts (one hour at the default interval) |
| `QUEUE_LOG_LINES` | `2000` | Log lines held in memory |
| `QUEUE_LOG_PRIME_BYTES` | `65536` | How much of an existing log file to read at startup |
| `QUEUE_MAX_CLIENTS` | `256` | Maximum clients pulled from `/connz` |
| `QUEUE_REQUEST_TIMEOUT_SECS` | `4` | Deadline for one monitoring request |
| `QUEUE_UI_COOKIE_DOMAIN` | `HEYO_UI_COOKIE_DOMAIN` | Parent domain for the shared light/dark theme cookie |
| `QUEUE_UI_COOKIE_NAME` | `HEYO_UI_COOKIE_NAME`, else `heyo_theme` | Theme cookie name |
| `RUST_LOG` | | Log filter; the shipped unit uses `info,queue=debug` |

Values that would make the process useless are corrected with a warning instead of refused:

- `QUEUE_POLL_SECS=0` becomes 1 second.
- `QUEUE_HISTORY_POINTS=0` becomes 2.
- `QUEUE_LOG_LINES=0` becomes 1.
- A request timeout at or above the poll interval is capped at 80% of the interval (minimum 500 ms), so scrapes never overlap.

Unparseable numbers are ignored with a warning and the default is used.

### The log panel needs a shared filesystem

NATS writes its own diagnostics (permission violations, slow-consumer disconnects, stream restore failures) only to its log file. Every panel except the log works against a remote monitoring port; the log panel works only when `queue` and nats-server share a filesystem.

The file is followed, not re-read. At each end-of-file the path is re-checked: a new inode is treated as a rotation and a shorter file as a truncation, and both reopen from the start. This keeps the tail working through supervisord's rename-based rotation.

Lines that don't match nats-server's format (`[pid] date time [LVL] message`) are kept whole with no level, and are never hidden by a severity filter, since those are usually panics and stack traces.

## HTTP API

| Route | Auth | Returns |
| --- | --- | --- |
| `GET /dashboard` | token | The dashboard page (`/` redirects here) |
| `GET /api/overview` | token | One scrape of the whole server: server info, throughput rates, totals, per-account streams and consumers, clients, and chart history |
| `GET /api/logs` | token | The log tail |
| `GET /healthz` | open | `ok`. Does not check NATS |
| `GET /__ui/{path}` | open | Shared stylesheet, theme script, and fonts |

"token" means `Authorization: Bearer <QUEUE_API_TOKEN>` is required when the variable is set.

`/healthz` deliberately does not probe NATS. If it did, a NATS outage would take the dashboard out of rotation exactly when you need it.

`/api/overview` reports `connected: false` and an `error` string when the last scrape failed.

### `/api/logs` parameters

| Parameter | Meaning |
| --- | --- |
| `since` | Sequence number; returns only newer lines. The dashboard polls with this |
| `level` | Severity floor: `trace`, `debug`, `info`, `warn`, `error`, `fatal` (`warn` means warn and worse). An unknown level is `400` |
| `q` | Case-insensitive substring |
| `limit` | Default 200, maximum 5000. Applied after `level` and `q`, so "last 20 errors" means 20 errors |

## Register with app-lb

[`queue/examples/queue.json`](../queue/examples/queue.json) is the app-lb deployment spec that ships with the service:

```json
{
  "id": "queue",
  "routes": [{ "host": "queue.example.com" }],
  "upstreams": ["127.0.0.1:9700"],
  "health": { "path": "/healthz", "timeout_secs": 2 },
  "auth": {
    "client_id": "REPLACE.apps.googleusercontent.com",
    "client_secret": { "secret": "google", "key": "client_secret" },
    "allowed_domains": ["example.com"],
    "public_paths": ["/healthz", "/__ui/"],
    "cookie_domain": "example.com",
    "forward_identity": true
  }
}
```

```sh
heyctl apply -f queue/examples/queue.json
```

- Replace the host, client id, allowed domains, and cookie domain. Store the Google client secret with `heyctl create secret google --from-stdin client_secret`. See [app-lb auth](app-lb-auth.md#google).
- `/__ui/` must be public, or the page renders unstyled behind the sign-in redirect.
- Setting `cookie_domain` to the same parent domain as your other HWS dashboards lets one sign-in cover all of them; setting `HEYO_UI_COOKIE_DOMAIN` to the same value shares the theme choice.
- `forward_identity` fills in the name in the top bar. It is display only; the page is read-only.

**This page discloses every account.** Behind the gate it shows all accounts' stream names, subjects, depths, and consumer names, and every connected client's address, name, and subscriptions. Consider that before pointing it at a server shared with tenants. The process logs a warning at startup when `QUEUE_API_TOKEN` is unset.

## Running NATS as a managed microVM

[`app-lb/examples/nats/`](../app-lb/examples/nats/README.md) contains two ways to run nats-server with JetStream inside a Firecracker VM whose lifecycle app-lb owns.

| Variant | Files | Use it for |
| --- | --- | --- |
| Managed | `Dockerfile.managed`, `managed.json`, `image/start-managed.sh`, `image/managed.conf` | A broker whose lifecycle and state are independent of CI. Mandatory authentication, workspace-backed state |
| Standalone | `image/Dockerfile`, `image/init.sh`, `image/nats-server.conf`, `image/preflight.sh`, `build-image.sh`, `nats.json` | A simple single-host broker for development or experiments |

### Things that apply to both

**app-lb cannot proxy the NATS protocol.** app-lb is an HTTP proxy, and NATS is a raw TCP protocol in which the server speaks first. So the deployments split the ports:

| Port | Reached by | How |
| --- | --- | --- |
| `8222` (HTTP monitoring) | app-lb | `vm.port`; the health check probes NATS's own `/healthz` |
| `4222` (client protocol) | NATS clients | `vm.open_ports`, directly at the VM's guest IP |

Find the address from the admin API. `addr` is built from `vm.port`, so take the host part and use 4222:

```sh
NATS_IP=$(curl -s localhost:9090/deployments/nats | jq -r '.vms[0].addr' | cut -d: -f1)
export CLOUD_NATS_URL="nats://$NATS_IP:4222"
```

The guest IP is stable for the life of a sandbox but can change when the VM is recreated, so clients should re-read it rather than bake it in.

**Exactly one replica.** JetStream here is a single server with a file store, not a cluster. Two replicas would be two independent brokers. Both specs set `max_replicas: 1`, `warm_pool: 0`, and `idle_action: "retain"` (a retired VM is stopped with its disk kept, not destroyed). Scaling out means a real NATS cluster, which these examples don't provide.

**Routes are empty.** `"routes": []` keeps the unauthenticated monitoring port off the proxy. To expose it deliberately, add a route with an `auth` block:

```sh
heyctl set routes nats --host nats.internal.example.com
heyctl set routes nats --none    # withdraw
```

Don't expose 4222 or the monitoring port to untrusted networks. Cross-host client access needs a private encrypted network or TLS, not just an open firewall port.

### Managed variant

[`managed.json`](../app-lb/examples/nats/managed.json) is a template, not a ready deployment:

```json
{
  "id": "nats-managed",
  "routes": [],
  "vm": {
    "driver": "firecracker",
    "image": "REPLACE_WITH_VERIFIED_NATS_2_11_17_IMAGE",
    "port": 8222,
    "open_ports": [4222],
    "size_class": "small",
    "disk_size_gb": 20,
    "ttl_seconds": 0,
    "start_command": "setsid nohup /opt/nats/start.sh </dev/null >/tmp/nats-boot.log 2>&1 &",
    "env_from": [
      {"secret": "nats-managed", "key": "token", "as": "NATS_TOKEN"}
    ],
    "workspace": {
      "path": "/workspace",
      "store": "https://REPLACE_WITH_PRIVATE_ARTIFACT_STORE",
      "ref": "workspace-nats-managed",
      "auth": {"secret": "nats-artifacts", "key": "api-key"}
    }
  },
  "build": {
    "repo": "https://github.com/Heyo-Computer/heyo-public.git",
    "ref": "REPLACE_WITH_CI_VERIFIED_COMMIT",
    "dockerfile": "app-lb/examples/nats/Dockerfile.managed",
    "context": ".",
    "image_size_mb": 512
  },
  "scaling": {
    "min_replicas": 0,
    "max_replicas": 1,
    "warm_pool": 0,
    "scale_to_zero_after_secs": 0,
    "boot_timeout_secs": 180,
    "idle_action": "retain"
  },
  "health": {"path": "/healthz?js-enabled-only=true", "timeout_secs": 5}
}
```

How it works:

- The image contains NATS 2.11.17. PID 1 only prepares the guest; app-lb mounts the workspace and injects `NATS_TOKEN` from the secret store before running `/opt/nats/start.sh`.
- `start.sh` refuses to start unless `NATS_TOKEN` is set, `/workspace` is a separate mounted filesystem, `/workspace/.managed-state` contains `nats-state-v1`, and `/workspace/jetstream` is a real directory on that filesystem. It never formats a disk or creates missing state.
- The launcher JSON-quotes the token into `NATS_CONFIG_TOKEN` so NATS's config parser treats it as an opaque string. Store the raw token; don't pre-quote it.
- JetStream data and the NATS log (`/workspace/nats.log`, rotated at 16 MB) live on the workspace. `vm.workspace` is what carries state across VM replacement; `disk_size_gb` alone does not.

To activate it:

1. Build the image through CI or `build`, and set `vm.image` to the verified image.
2. Create the `nats-managed` secret (key `token`) and the `nats-artifacts` secret holding the store's API key. See [heyosecret](heyosecret.md).
3. Point `workspace.store` at a private [artifacts store](artifacts.md) and use a dedicated `ref`. Never reuse CI's workspace or snapshot tag.
4. Seed the workspace with the marker file and an empty `jetstream` directory (or a restored store).
5. Set `min_replicas: 1`.

Limits to be aware of:

- Replacing the broker has a stop, capture, restore gap. It is not a replicated cluster or a zero-downtime upgrade.
- Workspace capture is crash-consistent storage recovery, not a JetStream backup. Before a planned migration, stop producers and consumers and take a JetStream network backup **with consumers**, then restore and compare stream and consumer state before switching clients. Never copy a live JetStream directory as a backup.
- The marker file guards initialization; it does not prove a restore succeeded.

Test the image locally from the repository root (Docker and the `nats` CLI required):

```sh
docker build --platform linux/amd64 -f app-lb/examples/nats/Dockerfile.managed \
  -t heyo-nats-managed-test:2.11.17 .
NATS_TEST_IMAGE=heyo-nats-managed-test:2.11.17 \
  python3 app-lb/examples/nats/test_managed.py -v
```

These tests cover authentication, startup rejection, backup and restore with pending acks, and restart. They don't boot Firecracker or exercise app-lb workspace capture.

### Standalone variant

```sh
cd app-lb/examples/nats
./build-image.sh                        # -> <heyvm images dir>/nats.ext4
heyctl apply -f nats.json
heyctl rollout status nats
heyctl exec nats -- /opt/nats/preflight.sh
```

`build-image.sh` wraps `heyvm mvm build` with this directory as the context. It needs `docker`, `mke2fs`, `heyvm`, and `fakeroot` (unless run as root). Optional environment: `IMAGE_NAME` (default `nats`), `SIZE_MB`, `DNS_SERVER`.

How it differs from the managed variant:

- `nats.json` uses a `disk_size_gb: 20` data disk, not a workspace. `init.sh` formats `/dev/vdb` if it has no filesystem, mounts it at `/workspace`, and refuses to start nats-server if `/workspace` is not a mount. The disk belongs to one sandbox; it survives stop and resume (`idle_action: "retain"`) and image rebuilds, but not VM recreation.
- JetStream lives at `/workspace/jetstream`; the log is at `/workspace/log/nats-server.log`.
- **Authentication is off by default.** Add it by writing a config fragment to the data disk, which `init.sh` includes when present:

```sh
heyctl exec nats -- sh -c 'mkdir -p /workspace/nats && cat > /workspace/nats/auth.conf <<EOF
authorization { token: "<token>" }
EOF'
heyctl restart nats
```

- `ttl_seconds: 86400` is a backstop that stops the VM if app-lb dies and stops renewing it.

The Dockerfile pins a nats-server patch release. Bump it deliberately and check the release notes for store-format changes, since the binary is rebuilt but the store on disk is not.

### Operating NATS

```sh
heyctl get deployments
heyctl exec nats -- /opt/nats/preflight.sh      # standalone: durability and listeners
heyctl shell nats
curl -s "http://$NATS_IP:8222/jsz?streams=1" | jq .
curl -s "http://$NATS_IP:8222/varz" | jq '{uptime, connections, in_msgs, out_msgs}'
```

Point the queue dashboard at a VM-hosted broker with `QUEUE_NATS_MONITOR_URL=http://<guest ip>:8222`. The log panel won't work in that setup, since the log file is inside the guest.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Dashboard says it can't reach the server | `QUEUE_NATS_MONITOR_URL` is wrong, or nats-server has no `http:` listener |
| Log panel says it is disabled | Set `QUEUE_NATS_LOG_FILE` |
| Log panel empty after setting the path | The `queue` user can't read the file, or nats-server logs somewhere else |
| Rates show nothing | Rates need two scrapes; wait one poll interval |
| A stream is flagged `no consumer` | A worker that should consume it isn't running or hasn't created its consumer |
| A consumer is flagged `stalled` | Its worker is attached but not acking; check the worker's logs |
| `/api/logs?level=...` returns `400` | Use `trace`, `debug`, `info`, `warn`, `error`, or `fatal` |
| NATS clients can't connect through the app-lb hostname | Expected; connect directly to `<guest ip>:4222` |
| Managed broker won't start | Check `/tmp/nats-boot.log` in the guest for which `start.sh` precondition failed |

For direct inspection, the monitoring port is the source of truth:

```sh
curl -s 'http://127.0.0.1:8222/jsz?accounts=1&streams=1&consumers=1&config=1' | jq
curl -s http://127.0.0.1:9700/api/overview | jq '.accounts[].streams[]'
```

Stream names, message counts, and consumer numbers should agree between the two; rates exist only in the dashboard.
