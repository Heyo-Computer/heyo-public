# `nats` — a NATS server with JetStream, in a microVM

A Firecracker rootfs carrying [NATS](https://nats.io) with JetStream enabled, and
the app-lb deployment that owns its lifecycle. This is the bus
[queue-fn](../../../queue-fn) and heyo's cloud service both run on.

```sh
./build-image.sh                              # -> ~/.heyo/images/firecracker/nats.ext4
serverctl apply -f nats.json
serverctl rollout status nats
serverctl exec nats -- /opt/nats/preflight.sh # checks what a health check cannot
```

| File | What it is |
| --- | --- |
| `image/Dockerfile` | The rootfs. Official `nats-server` binary on an Ubuntu userland |
| `image/init.sh` | PID 1: mounts the data disk, then starts sshd and nats-server |
| `image/nats-server.conf` | Listeners, JetStream store, logging |
| `image/preflight.sh` | In-guest check, run over `serverctl exec` |
| `build-image.sh` | `heyvm mvm build` wrapper (the context has to be this directory) |
| `nats.json` | The deployment |

## The two things this example is really about

Everything else is boilerplate shared with any other heyvm image. These two are
what make it a *broker* rather than a web app, and both are easy to get wrong in
a way that works in testing.

### 1. The JetStream store has to be on the data disk

mvm-ctrl recopies the rootfs from the base image on **every cold boot**
(`mvm-ctrl/src/driver/firecracker.rs:1763`). The only storage that outlives a
boot is the data disk, which arrives as a raw unformatted `/dev/vdb` and which
the guest is expected to format and mount itself. So:

- `vm.disk_size_gb` is set in `nats.json` — without it there is no `/dev/vdb` at
  all.
- `init.sh` runs `mkfs.ext4` when it finds no filesystem, then mounts
  `/workspace`.
- `store_dir` is `/workspace`, and the store therefore lands at
  `/workspace/jetstream` — nats-server appends `jetstream` to whatever it is
  given, so spelling the subdirectory out here would produce
  `/workspace/jetstream/jetstream`.

A default `store_dir` on the rootfs would pass every test you could think to
run. JetStream would come up, streams would work, `/healthz` would return 200 —
and the first time app-lb recreated the VM, every stream would be gone, silently,
because JetStream recreates an empty store without complaint. What that costs is
concrete: everything queue-fn published and has not yet acked, and every cloud
sandbox create still sitting in `HEYO_SANDBOX`.

`init.sh` therefore **refuses to start nats-server at all** if `/workspace` is
not a mount. A VM that fails its health check shows up in `serverctl get
deployments` within a minute; silent non-durability shows up the day you need the
queue to have survived something.

`preflight.sh` is the other half of that: it proves durability by writing to the
mounted store, which is the one property no external check can see.

### 2. app-lb cannot carry the NATS client protocol

app-lb is `pingora_proxy::http_proxy_service` — an HTTP proxy, and only that
(`src/main.rs:631`). The NATS client protocol is a raw TCP protocol in which the
**server speaks first** (it sends `INFO {...}` on connect), so there is no
request line for an HTTP proxy to parse. There is no CONNECT handler and no
WebSocket-upgrade proxying either. A NATS client will never reach 4222 through
the proxy, and no configuration changes that.

So the deployment splits the two ports deliberately:

| Port | Who reaches it | How |
| --- | --- | --- |
| `8222` — HTTP monitoring | app-lb | `vm.port`, so it is what the proxy forwards to and what `health.path: /healthz` probes |
| `4222` — client protocol | queue-fn, cloud | `vm.open_ports`, reached **directly** at the VM's `guest_ip` |

This is not a workaround; it is app-lb doing the part it is good at. What you get
from the deployment is the VM's lifecycle — boot, health, restart, disk
retention, TTL renewal, the dashboard row — plus a real HTTP health check
(`/healthz` is NATS's own, not a TCP connect that a wedged server would still
pass). What you do not get is proxying for the bus itself, and nothing pretends
otherwise.

### Pointing clients at it

The VM's address comes out of the admin API. `addr` is `guest_ip:8222` (it is
built from `vm.port`), so take the host and use 4222:

```sh
NATS_IP=$(curl -s localhost:9090/deployments/nats | jq -r '.vms[0].addr' | cut -d: -f1)

# queue-fn
QFN_NATS_URL="nats://$NATS_IP:4222" ./queue-fn

# heyo cloud
CLOUD_NATS_URL="nats://$NATS_IP:4222"
```

A guest_ip is stable for the life of a sandbox but not across a recreate, so
anything long-lived should re-read it on start rather than bake it into a unit
file. With `idle_action: "retain"` a stop/resume keeps the same sandbox — and the
same address — so this changes far less often than it might.

## Why the deployment is pinned to exactly one replica

```json
"min_replicas": 1, "max_replicas": 1
```

JetStream is stateful, and this is a single server with a file store — not a
cluster. Two replicas behind one address would be two independent brokers with
two independent stores: a message published to one would be invisible to the
other, and a consumer would see roughly half its work. queue-fn is explicitly
single-instance for the related reason that `WorkQueue` retention permits one
consumer per subject.

`min_replicas: 1` also makes scale-to-zero structurally impossible —
`desired_replicas()` only considers it when `min_replicas == 0`
(`src/deployment.rs:394`) — which is what you want for a bus that other services
are holding connections to. `scale_to_zero_after_secs: 0` in the manifest is
belt-and-braces and does nothing on its own.

Scaling this horizontally means a real NATS cluster: three servers with a
`cluster {}` block, routes between them, and `R3` streams. That is a different
manifest — three deployments, or one with the routes baked in — and it is not
what a `max_replicas: 3` here would produce.

`idle_action: "retain"` means a VM app-lb retires is *stopped*, keeping its data
disk, and resumed later rather than rebuilt. For a broker whose whole value is
the contents of that disk, `destroy` would be the wrong default.

`ttl_seconds: 86400` is the backstop that kills the VM if app-lb dies and never
reaps it; app-lb renews it while it is alive. A day rather than the default hour
because an app-lb outage should not take the bus down with it.

## Authentication

There is none by default. The tap network is a host-local `/30` per VM and app-lb
is required to run beside a *local* daemon, so an unauthenticated listener here
sits behind the same boundary as a `127.0.0.1` bind.

If anything routes to the guest network from off-host, that stops being true. Add
credentials without rebuilding the image by dropping a config fragment on the
data disk — `init.sh` includes it when it exists:

```sh
serverctl exec nats -- sh -c 'mkdir -p /workspace/nats && cat > /workspace/nats/auth.conf <<EOF
authorization { token: "s3cret" }
EOF'
serverctl restart nats
```

Then `QFN_NATS_TOKEN=s3cret` for queue-fn, or userinfo in cloud's
`CLOUD_NATS_URL`. It lives on the data disk rather than in the image so a
credential is never baked into a rootfs that gets copied between hosts.

## Exposing the monitoring endpoint

`"routes": []` means nothing reaches it through the proxy, which is the right
default: `/varz` and `/connz` describe your whole messaging topology. To put the
monitoring API on a hostname — behind app-lb's TLS, sign-in gate and access log —
it is a reversible step:

```sh
serverctl set routes nats --host nats.internal.example.com
serverctl set routes nats --none    # withdraw again
```

Consider an `auth` block or an app-token gate at the same time; there is no
authentication on NATS's monitoring port itself.

## Operating it

```sh
serverctl get deployments                     # ready/desired, in-flight
serverctl exec nats -- /opt/nats/preflight.sh # durability + listeners
serverctl shell nats                          # a PTY in the guest
serverctl exec nats -- tail -50 /workspace/log/nats-server.log
curl -s localhost:9090/deployments/nats | jq '.vms'
```

Reading JetStream's own state is easiest straight off the monitoring port:

```sh
NATS_IP=$(curl -s localhost:9090/deployments/nats | jq -r '.vms[0].addr' | cut -d: -f1)
curl -s "http://$NATS_IP:8222/jsz?streams=1" | jq .
curl -s "http://$NATS_IP:8222/varz" | jq '{uptime, connections, in_msgs, out_msgs}'
```

queue-fn's dashboard renders the same stream and consumer state in context, with
the enqueued/processing/succeeded/failed view over what is actually flowing
through it.

## A note on rebuilding

`build-image.sh` rebuilds the rootfs. It does **not** touch the data disk, so a
rebuild is safe for the store: the new rootfs boots, `init.sh` finds an existing
filesystem on `/dev/vdb`, mounts it, and JetStream picks up the streams it left
there.

The one thing to be careful about is the nats-server version. The Dockerfile pins
a patch release rather than tracking `latest` precisely because the binary and
the store have different lifetimes — a rebuild that quietly crossed a major
version would put a new server in front of an old server's files with no upgrade
step in between. Bump it deliberately, and read the release notes for the store
format when you do.
