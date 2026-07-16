# app-lb

An application load balancer for [heyvm](https://heyo.computer) Firecracker/KVM microVMs,
built on [Pingora](https://github.com/cloudflare/pingora).

Register a *deployment* — a VM template plus routing rules and a scaling policy — and app-lb
routes HTTP traffic to a pool of VMs, booting and reaping them to match load. Deployments are
registered at runtime over an admin API; multiple deployments coexist in one process.

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

Configuration is environment-only:

| Variable | Default | Meaning |
| --- | --- | --- |
| `APP_LB_PROXY_ADDR` | `0.0.0.0:6188` | Proxy listener |
| `APP_LB_ADMIN_ADDR` | `127.0.0.1:9090` | Admin API listener |
| `APP_LB_STATE_PATH` | `app-lb-state.json` | Where deployment specs persist |
| `APP_LB_DAEMON_URL` | `http://127.0.0.1:34099` | heyvm daemon |
| `RUST_LOG` | `info,app_lb=debug` | Log filter |

## Admin API

```sh
# Register (or replace) a deployment.
curl -XPOST localhost:9090/deployments -H 'content-type: application/json' -d '{
  "id": "demo",
  "routes": [{"host": "demo.local"}, {"path_prefix": "/demo"}],
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
```

Then: `curl -H 'Host: demo.local' localhost:6188/`

Responses carry an `x-vm-id` header naming the VM that served them.

### Routing

A request matches a rule when every field the rule sets matches. Rules are tried
most-specific-first: a `host` beats a bare `path_prefix`, and a longer prefix beats a shorter
one. `host` matching is case-insensitive, strips the port, and falls back to HTTP/2's
`:authority` (which carries no `Host` header). No match is a 404.

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
