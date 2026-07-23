# app-lb

An application load balancer for [heyvm](https://heyo.computer) Firecracker/KVM microVMs,
built on [Pingora](https://github.com/cloudflare/pingora).

Register a *deployment* — routing rules plus a backend — and app-lb routes HTTP traffic to it.
Deployments are registered at runtime over an admin API; multiple deployments coexist in one
process. A deployment is one of two kinds:

- **Managed** (`vm`): a VM template plus a scaling policy. app-lb boots and reaps a pool of
  microVMs to match load.
- **Static / `proxy_pass`** (`upstreams`): a fixed set of upstream addresses (`host:port` or
  `ip:port`) to forward to — another app or service. No VM lifecycle and no autoscaling; the
  upstreams are load-balanced least-in-flight with failover, and health-re-probed so a
  recovered upstream rejoins. See [Static / proxy_pass deployments](#static--proxy_pass-deployments).

A deployment sets exactly one of `vm` or `upstreams`.

Only Firecracker and KVM are supported. This is not a limitation of taste: app-lb routes
directly to `SandboxInfo.guest_ip`, which the daemon only populates for tap-networked
Firecracker/KVM backends on a local daemon. A Libvirt VM would boot fine and then be
unroutable, so the driver is rejected at registration.

## Requirements

- Linux with KVM (`/dev/kvm`)
- A running `heyvmd` daemon (default `http://127.0.0.1:34099`)
- `cmake` — a hard build dependency of `pingora-core`, via `flate2`'s `zlib-ng` backend

## Run

```sh
cargo build --release
./target/release/app-lb
```

To run it as a managed, auto-restarting service, see the supervisord unit in
[`deploy/supervisor/`](deploy/supervisor/).

Configuration is environment-only:

| Variable | Default | Meaning |
| --- | --- | --- |
| `APP_LB_PROXY_ADDR` | `0.0.0.0:6188` | Proxy listener |
| `APP_LB_ADMIN_ADDR` | `127.0.0.1:9090` | Admin API listener |
| `APP_LB_STATE_PATH` | `app-lb-state.json` | Where deployment specs persist |
| `APP_LB_NAME` | `app-lb` | Display name in the dashboard header and page title |
| `APP_LB_DAEMON_URL` | `http://127.0.0.1:34099` | heyvm daemon |
| `APP_LB_DASHBOARD_PASSWORD` | *(unset)* | Set to gate the dashboard behind HTTP Basic Auth |
| `APP_LB_DASHBOARD_USER` | `admin` | Basic Auth username (only used when a password is set) |
| `APP_LB_ADMIN_AUTH` | `false` | `1`/`true` to extend the gate to the deployment CRUD API (needs a password) |
| `APP_LB_TLS_CERT` | *(unset)* | PEM cert path; set with `APP_LB_TLS_KEY` to enable HTTPS |
| `APP_LB_TLS_KEY` | *(unset)* | PEM private-key path |
| `APP_LB_PROXY_TLS_ADDR` | `0.0.0.0:6189` | HTTPS listener (only bound when cert+key are set) |
| `RUST_LOG` | `info,app_lb=debug` | Log filter |

## Admin API

```sh
# Register (or replace) a deployment.
curl -XPOST localhost:9090/deployments -H 'content-type: application/json' -d '{
  "id": "demo",
  "routes": [{"host": "demo.local"}, {"host_suffix": "apps.example.com"}, {"path_prefix": "/demo"}],
  "vm": {
    "driver": "firecracker",
    "image": "nginx",
    "port": 80,
    "size_class": "small",
    "ttl_seconds": 900
  },
  "scaling": {
    "min_replicas": 0,
    "max_replicas": 4,
    "warm_pool": 1,
    "target_concurrency": 10,
    "scale_to_zero_after_secs": 300,
    "cold_start_timeout_secs": 120,
    "drain_timeout_secs": 30
  },
  "health": {"path": "/", "timeout_secs": 2}
}'

curl localhost:9090/deployments          # list, with live VM state
curl localhost:9090/deployments/demo     # one deployment
curl -XDELETE localhost:9090/deployments/demo   # drain and reap every VM
curl localhost:9090/healthz
curl localhost:9090/metrics              # metrics snapshot (JSON)

# Edit a deployment in place (full spec). The pool is preserved unless the `vm`
# template changes, in which case the VMs are rebuilt.
curl -XPUT localhost:9090/deployments/demo -H 'content-type: application/json' -d @demo.json

# Scale: partial update of just the scaling policy (fields omitted are kept).
curl -XPATCH localhost:9090/deployments/demo/scaling -H 'content-type: application/json' \
  -d '{"min_replicas": 2, "max_replicas": 8}'

# Evict (delete) a single VM. The x-vm-id header / metrics give the sandbox id.
curl -XDELETE localhost:9090/deployments/demo/vms/sb-abc123            # graceful drain
curl -XDELETE 'localhost:9090/deployments/demo/vms/sb-abc123?force=true'  # kill now
```

Then: `curl -H 'Host: demo.local' localhost:6188/`

Responses carry an `x-vm-id` header naming the VM (or, for a static deployment, the upstream
address) that served them.

### Static / proxy_pass deployments

A deployment with an `upstreams` list (instead of a `vm` template) forwards matched requests to
a fixed set of upstream addresses — another app or service — like nginx `proxy_pass`. There is
no VM lifecycle and no autoscaling: the upstreams are load-balanced least-in-flight with
per-request failover, and the autoscaler health-re-probes them each tick (using the
deployment's `health` check) so a recovered upstream rejoins routing and a dead one is skipped.

```sh
curl -XPOST localhost:9090/deployments -H 'content-type: application/json' -d '{
  "id": "legacy-api",
  "routes": [{"path_prefix": "/legacy"}],
  "upstreams": ["10.0.0.9:8080", "backend.internal:8080"],
  "health": {"path": "/healthz", "timeout_secs": 2}
}'
```

Each upstream is a `host:port` (or `ip:port`); a hostname is re-resolved per connection. To
change the targets, `PUT` the deployment with a new `upstreams` list (the backends are rebuilt).
Scaling (`PATCH .../scaling`) and per-VM eviction (`DELETE .../vms/...`) do not apply to a
static deployment and are rejected. Upstreams are proxied over **plaintext HTTP**.

### Editing & scaling a deployment

`PUT /deployments/:id` replaces a deployment's spec **in place**. The path id
wins (the body's id can't retarget another deployment). Crucially, the running
pool is *preserved* whenever the `vm` template is unchanged — a scaling, route,
or health edit never disturbs live VMs; only a change to the `vm` block reboots
them, because the existing VMs were built from the old template. (This is unlike
`POST /deployments`, which always replaces and tears the pool down.)

`PATCH /deployments/:id/scaling` is a **partial** update of just the scaling
policy: fields you omit keep their current values, so `{"min_replicas": 2}`
raises the floor without resetting `target_concurrency` or the timeouts. It
always preserves the pool; the autoscaler grows or drains it to match. Both
endpoints validate (e.g. `min_replicas > max_replicas` is a `400`).

### Evicting a VM

`DELETE /deployments/:id/vms/:sandbox_id` removes one VM from a deployment's
pool, as opposed to `DELETE /deployments/:id` which tears the whole deployment
down. The autoscaler boots a replacement on its next tick if the scaling policy
still wants the capacity, so this is "recycle this instance", not "shrink the
deployment" — to shrink, lower `max_replicas` instead.

- Default (**graceful**): the VM stops taking new requests, finishes its
  in-flight ones, and is reaped once idle or at `drain_timeout_secs`. Returns
  `202 Accepted` with `{"outcome":"draining"}`.
- `?force=true` (**immediate**): the VM is killed now; its in-flight requests
  fail over to another VM via the proxy's retry. Returns `200 OK` with
  `{"outcome":"killed"}`.

A still-booting (pending) VM is simply killed in either mode. Evicting the sole
VM of a `max_replicas: 1` deployment leaves a brief capacity gap until the
replacement boots (a request arriving in that window eats a cold start).
Unknown deployment or VM id is a `404`.

## Dashboard

Open `http://<admin-addr>/dashboard` (default `http://127.0.0.1:9090/dashboard`)
for a live view of the fleet: host and per-VM CPU/memory, per-deployment pool
utilisation and per-VM load, request latency (distribution + p50/p90/p99 and a
client-derived requests/sec), cold-start times, and autoscaling activity. It is
a single self-contained page that polls `GET /metrics` every 2s — no external
assets, so it works over an SSH tunnel to the admin port.

`GET /metrics` returns the same data as JSON (host usage, a global rollup, and a
per-deployment breakdown), suitable for scraping into your own tooling.

The dashboard is also interactive. Each deployment card has **Scale** (a form
over min/max replicas, warm pool, and target concurrency → `PATCH .../scaling`)
and **Edit** (a JSON editor over the full spec → `PUT`), and each VM row has
**Drain**/**Kill** buttons (→ `DELETE .../vms/:id`). While a form is open the
cards stop re-rendering so an in-progress edit isn't wiped, though the stat tiles
keep updating live. These buttons call the admin CRUD API — set
`APP_LB_ADMIN_AUTH` (see below) to require the dashboard credentials for them.

### Auth

The dashboard is open by default. Set `APP_LB_DASHBOARD_PASSWORD` to put HTTP
Basic Auth in front of both `/dashboard` and its `/metrics` data source (gating
the page alone would be pointless — the JSON carries the same data). The
username defaults to `admin`; override it with `APP_LB_DASHBOARD_USER`. A
browser prompts once and reuses the credentials for the metric polls.

```sh
APP_LB_DASHBOARD_PASSWORD=s3cret ./target/release/app-lb
curl -u admin:s3cret localhost:9090/metrics
```

By default only the dashboard view (`/dashboard` + `/metrics`) is gated;
deployment CRUD stays open. Set `APP_LB_ADMIN_AUTH=1` to extend the same gate to
the CRUD API — register/edit/scale/delete/evict **and** the reads that expose a
spec (env vars can hold secrets like API keys). It reuses the dashboard
credentials, so the dashboard's own write buttons keep working (the browser
replays the cached creds), and `curl` needs `-u`:

```sh
APP_LB_DASHBOARD_PASSWORD=s3cret APP_LB_ADMIN_AUTH=1 ./target/release/app-lb
curl -u admin:s3cret -XDELETE localhost:9090/deployments/demo/vms/sb-abc123
```

`APP_LB_ADMIN_AUTH` requires a password — enabling it without one is a hard
startup error, never a silently-open gate. `/healthz` is always open so probes
keep working. The credentials are compared in constant time, but the admin
listener is plain HTTP — terminate TLS in front of it, or reach it over an SSH
tunnel, if it leaves localhost.

Two data sources feed it:

- **What the LB observes directly** — request latency and status from the proxy
  path; cold-start duration and scale up/down/reap from the reconcile loop;
  in-flight concurrency, pool occupancy, and serving uptime per VM. These
  counters are cumulative since process start; rates (requests/sec, VMs
  created/reaped per second) are derived by the dashboard by diffing polls.
- **What the daemon reports** — the autoscaler reads `GET /system/usage` once
  per reconcile tick (a daemon-side cached sample, so it's a cache read rather
  than a per-VM probe) for host CPU/memory and per-VM CPU% (percent of a core)
  and RSS. Only backends with a local host process are covered, which for
  app-lb's Firecracker/KVM-on-local-daemon constraint is all of them. Per-VM
  disk and network throughput are **not** exposed by the daemon and so are
  absent here.

### Routing

A rule matches a request when every field it sets matches. A rule may set:

- `host` — exact hostname (case-insensitive, port stripped).
- `host_suffix` — **subdomain / wildcard** match. A rule with `host_suffix:
  "apps.example.com"` matches the apex `apps.example.com` and any subdomain
  (`a.apps.example.com`, `x.y.apps.example.com`), anchored at a label boundary
  so `notapps.example.com` does **not** match. A leading dot is accepted and
  ignored.
- `path_prefix` — path prefix, e.g. `/api`.

Rules are tried most-specific-first, and the tiers don't overlap: an exact
`host` beats any `host_suffix`, which beats any bare `path_prefix`. Within a tier
a longer suffix or a longer prefix wins. Host matching (exact or suffix) is
case-insensitive, strips the port, and falls back to HTTP/2's `:authority`
(which carries no `Host` header). No match is a 404.

### TLS

The proxy serves plaintext HTTP on `proxy_addr` by default. Set `APP_LB_TLS_CERT`
and `APP_LB_TLS_KEY` (PEM paths) to *also* bind an HTTPS listener on
`APP_LB_PROXY_TLS_ADDR` (default `0.0.0.0:6189`); both listeners then run at once.
Setting only one of cert/key is a hard startup error rather than a silent
plaintext fallback. Upstreams stay plaintext regardless — the guest IP is on a
host-local tap network — so this is TLS *termination* at the edge. The TLS stack
is rustls (the same one heyo-sdk already links through reqwest), enabled via the
`rustls` feature on `pingora-core`.

```sh
APP_LB_TLS_CERT=cert.pem APP_LB_TLS_KEY=key.pem ./target/release/app-lb
curl -k https://localhost:6189/ -H 'Host: demo.local'
```

### Scaling

Desired replicas is `ceil(demand / target_concurrency) + warm_pool`, clamped to
`[min_replicas, max_replicas]`, where demand counts in-flight requests *plus* requests
waiting on a cold start.

Scale-to-zero applies only when both `min_replicas` and `warm_pool` are 0. A request arriving
at an empty pool is held (up to `cold_start_timeout_secs`) while a VM boots, rather than
failing — in practice a Firecracker VM is serving in ~1–2s. Scaling down marks a VM draining
so it finishes in-flight work, then kills it once idle or at `drain_timeout_secs`.

`ttl_seconds` is a backstop: VMs expire on their own if app-lb dies without reaping them. It
is renewed while app-lb is alive, and VMs from a previous run are re-adopted on startup
(matched by their `applb-<deployment>-<nonce>` name). VMs app-lb did not create are never
touched.

## Design notes

Three constraints shaped this, each verified against the dependencies' source rather than
their docs:

- **Pingora fixes its service set at startup.** `Server::run_forever(self)` consumes the
  server, so dynamic registration cannot mean adding services at runtime. Every deployment
  lives in one `Registry` behind `ArcSwap`, and a single `ProxyHttp` routes across it.
- **`Sandbox::wait_for_ready` returning `Ok` does not mean healthy.** Its match has a
  `_ => return Ok(info)` arm, so `Stopped`/`Paused`/`ColdStored` all return `Ok`, and against
  a local daemon a broken VM surfaces as `Stopped` rather than `Failed`. A VM only joins the
  pool once it reports `Running`, has a `guest_ip`, *and* answers a probe.
- **`pingora-load-balancing` is deliberately not used.** Its selection algorithms
  (RoundRobin/Random/FNVHash/Ketama) cannot see in-flight counts, which is the signal both
  selection and autoscaling need here; `Backend::ext` is ignored for identity and
  `hash_key()` is `pub(crate)`. app-lb keeps its own pool with least-in-flight selection.

There is no event stream on the daemon, so the autoscaler polls (~2s), calling
`Sandbox::list()` once per tick — `Sandbox::info()` fetches that same full list and filters
client-side, so per-VM polling would be quadratic. A cold-start request nudges the autoscaler
directly rather than waiting for the next tick.
