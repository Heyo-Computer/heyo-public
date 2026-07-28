# Example deployments

Ready-to-`POST` deployment specs for the app-lb admin API.

```sh
curl -XPOST localhost:9090/deployments \
  -H 'content-type: application/json' \
  -d @examples/pg-fc-dashboard.json
```

## `artifacts.json` — managed VM pool for the artifacts store

[artifacts](../../artifacts) is a content-addressed blob store (`art`) built for
ext4. Its `art serve` daemon is a plain HTTP API — blob upload/download,
manifests, tags, and an SSR dashboard — so unlike the pg-fc pooler it *is* a
microVM, and this is a **managed (`vm`) deployment** rather than a static one.

### Prerequisites

Build the image once, on the host running heyvmd:

```sh
cd ~/Projects/artifacts
heyvm mvm build --local-only -f Dockerfile -n artifacts --size-mb 768
```

That writes `~/.heyo/images/firecracker/artifacts.ext4`, which is the `image`
name the spec refers to. Then register the deployment and point a hostname at
app-lb:

```sh
curl -XPOST localhost:9090/deployments \
  -H 'content-type: application/json' \
  -d @examples/artifacts.json

curl -H 'Host: artifacts.local' localhost:6188/healthz
curl -H 'Host: artifacts.local' -H 'Authorization: Bearer change-me' \
     localhost:6188/usage
```

### Notes on the spec

- **Pinned to one replica, deliberately.** `min_replicas` and `max_replicas` are
  both `1`, and this is the one setting you should not raise. Each VM gets its
  own data disk, so a pool of *N* replicas is *N* independent stores: a blob
  `PUT` to one is invisible to the others, and app-lb's least-in-flight routing
  would answer `GET /blobs/{digest}` with a `200` or a `404` depending on which
  replica the request landed on. A content-addressed store is only horizontally
  scalable behind shared storage, which heyvm does not give a Firecracker guest.
  Scale it with a bigger `size_class`, not more replicas.

- **`scale_to_zero_after_secs: 0`** — with `min_replicas: 1` the VM never idles
  out. Scaling to zero would be safe for the *data* (that lives on `/dev/vdb`,
  not the rootfs) but every cold start pays a full base-image copy plus a boot,
  and a store that answers `503` while it wakes up is not much of a store.

- **The app is started by `start_command`, not by `init.sh`.** Same pattern as
  the vault/`tk` image: `env_vars` reach only the `start_command` process, so
  that is the sole channel carrying `ART_API_KEY`. `init.sh` brings up
  networking, formats and mounts `/dev/vdb` at `/workspace`, starts sshd, and
  prints `HEYVM_READY`.

- **`start_command` must daemonize itself.** It is run to completion, not
  supervised, so a foreground `art serve` would hang the boot step and the VM
  would never come ready. Hence the
  `setsid nohup … </dev/null >/var/log/art.log 2>&1 &` wrapper, exactly as the
  `vault-*` deployments do for `tk`. Redirecting stdout is part of that: a
  process still holding the console can wedge the serial protocol heyvm uses.

- **`ART_ROOT` must be on the data disk.** The base rootfs is re-copied from the
  image on every cold boot, so a store under `/` would silently lose every blob
  on restart. `/workspace/store` is on `/dev/vdb`, which `disk_size_gb` creates.
  Note the disk is **per-sandbox**: it does not survive the VM being *replaced*
  (a `vm`-template edit, or a `POST` that rebuilds the pool). Treat the VM as a
  cache that `art heyvm import` can repopulate, not as the only copy of anything.

- **`disk_size_gb: 4`** is deliberately modest — the store keeps images sparse,
  so 4 GB holds far more than 4 GB of nominal image (a 20 GiB rootfs that is 94%
  empty costs about 600 MB). Raise it for a larger catalogue. heyvm preallocates
  the disk with `fallocate` on first boot, so the host needs that much free space
  up front; set `HEYO_FC_PREALLOC_DATA_DISK=0` in heyvmd's environment to leave
  it sparse if the host is tight.

- **Health is `GET /healthz`**, which is the one route that stays open when
  `ART_API_KEY` is set. A readiness probe carries no credentials, so a health
  endpoint behind auth reports the deployment unhealthy the day the key rotates.

- **Change `ART_API_KEY`.** Every API route except `/healthz` is behind it,
  compared in constant time. Leaving it as `change-me` means anyone who can reach
  the hostname can write blobs and rewrite tags. Unsetting it disables auth
  entirely, which is only reasonable on a listener nothing else can reach.

- **The dashboard has its own credentials.** `ART_ADMIN_USER` /
  `ART_ADMIN_PASSWORD` gate `/dashboard`, separately from the API key — the key
  is for machines and rides in a header no browser sends, so handing it to a
  person just to let them look at a usage page is how machine keys end up in
  browser histories. Holding one does not grant the other. **Omit
  `ART_ADMIN_PASSWORD` and the dashboard is not served at all**, which is the
  right setting for a VM that only feeds other machines.

  Reach it at `http://artifacts.local:6188/dashboard` once the hostname points at
  app-lb. Sessions are an HttpOnly `SameSite=Strict` cookie holding a random
  per-process token, so a VM replacement signs everyone out — which is correct,
  since that VM's store went with it.

- **Host routing, not a path prefix.** The dashboard and API mount at the root
  and app-lb forwards the path unchanged, so a `path_prefix` route would break
  the links. Route by host, as here.

- **Add `"ART_READ_ONLY": "1"`** to `env_vars` for a pull-only mirror: reads
  still serve, and every mutating route answers `403`. Useful when one writable
  instance feeds several read replicas — which, per the first note, is the only
  correct way to have more than one of these.

### Firewall

app-lb reaches the guest at `guest_ip:8080` over the tap interface. A host
firewall that rejects forwarded traffic will make every VM look permanently
unhealthy — and confusingly, `ping` still works, because ufw allows ICMP echo
while rejecting TCP with `icmp-host-prohibited`, which surfaces as
`No route to host`. If health checks never go green, check this first:

```sh
sudo ufw status                       # is it active?
sudo ufw allow in on tap-fc-+         # or scope to the guest subnet
```

## `pg-fc-dashboard.json` — static / proxy_pass to the pg-fc admin dashboard

[pg-fc](../../heyo/pg-fc) (the `pg-vm-pool` Postgres pooler) ships an optional
server-side-rendered **admin dashboard** — an axum HTTP server that runs inside
the pooler process. Because the pooler relies on host-side APIs (heyvmd, `df`,
the live registry), it isn't itself a microVM, so it's fronted as a **static
(proxy_pass) upstream** rather than a managed VM pool.

### Prerequisites

The dashboard is **off by default**; enable it on the pooler by setting a listen
address (and, recommended, Basic auth):

```sh
PG_VM_POOL_DASHBOARD_LISTEN=127.0.0.1:8080 \
PG_VM_POOL_DASHBOARD_USER=admin \
PG_VM_POOL_DASHBOARD_PASSWORD=secret \
target/release/pg-vm-pool
```

The example upstream `127.0.0.1:8080` matches that listen address — change it if
you bind the dashboard elsewhere. app-lb and the pooler must be on the same host
(or the dashboard bound to a host-reachable address app-lb can reach).

### Notes on the spec

- **Host routing, not a path prefix.** The dashboard mounts its routes at the
  root (`/`, `/monitoring`, `/vm/{id}`, `/logs/…`) with root-relative links.
  app-lb forwards the path unchanged (it does not strip a prefix), so routing by
  a `path_prefix` would break those links. Route by host instead — point a
  hostname (here `pg-dashboard.local`; use a real DNS name or a `/etc/hosts`
  entry) at app-lb and reach the dashboard through it:

  ```sh
  curl -H 'Host: pg-dashboard.local' localhost:6188/
  ```

- **Health check is a bare TCP connect** (`"path": null`). The dashboard has no
  dedicated health endpoint, and `GET /` sits behind Basic auth — a TCP connect
  cleanly proves the HTTP listener is up without depending on auth. (app-lb also
  treats a `401` as healthy, so `"path": "/"` would work too.) app-lb re-probes
  static upstreams each tick, so if the pooler restarts the dashboard rejoins
  routing automatically.

- **Basic auth passes through.** app-lb forwards the `Authorization` header
  unchanged, so the browser's Basic-auth prompt from the dashboard works through
  the proxy. (app-lb's own dashboard auth, if enabled, is separate and gates only
  app-lb's admin API/dashboard, not proxied traffic.)

### Why not the pooler itself?

The pg-fc **pooler** (default `127.0.0.1:6432`) is **not** HTTP — it is a raw
Postgres v3 wire-protocol proxy (it reads the `StartupMessage`, answers
`SSLRequest`, and byte-splices the connection to a per-schema VM's Postgres).
app-lb is an **HTTP** proxy (Pingora `http_proxy_service`): it routes by
`Host`/path and speaks HTTP to the upstream, which a `psql` client never does.
So the pooler **cannot** be fronted by app-lb.

Front the pooler's Postgres port directly, or with an L4/TCP proxy (HAProxy in
`mode tcp`, nginx `stream`, etc.) — not app-lb. Only the dashboard (HTTP) belongs
here.

## `app-lb-admin.json` — put app-lb's own dashboard behind TLS

app-lb's admin API and dashboard bind their own **plaintext HTTP** listener
(`APP_LB_ADMIN_ADDR`, default `127.0.0.1:9090`) and have no TLS of their own. To
reach the dashboard over HTTPS at a DNS name, front it through app-lb's *own* TLS
proxy listener with a static deployment whose upstream is the admin address:

```sh
curl -XPOST localhost:9090/deployments \
  -H 'content-type: application/json' \
  -d @examples/app-lb-admin.json
```

Point a DNS record (here `lb-admin.example.com`) at the app-lb host and reach the
dashboard at `https://lb-admin.example.com/dashboard`. app-lb terminates TLS on
the HTTPS proxy port and forwards to the loopback admin listener; the proxy and
the admin listener are separate Pingora/axum services, so this loopback hop is a
normal upstream.

### Prerequisites & notes

- **The HTTPS proxy listener must be enabled** — set `APP_LB_TLS_CERT`,
  `APP_LB_TLS_KEY`, and (to be on 443) `APP_LB_PROXY_TLS_ADDR=0.0.0.0:443`. See
  the root README's TLS section; binding 443 as the non-root `app-lb` user needs
  `setcap 'cap_net_bind_service=+ep'` on the binary.
- **Keep the admin listener on loopback.** Leave `APP_LB_ADMIN_ADDR` on
  `127.0.0.1` so the *only* external path to the dashboard is the TLS one; don't
  also expose `:9090` on `0.0.0.0`.
- **Gate the admin API.** Set `APP_LB_ADMIN_AUTH=1` (+ `APP_LB_DASHBOARD_PASSWORD`)
  so registering/editing/deleting deployments requires the dashboard credentials —
  otherwise anyone who reaches the hostname can rewrite your routes. The Basic-auth
  header passes through the proxy unchanged.
- **Health is `GET /healthz`** — the admin API's always-open, unauthenticated
  probe endpoint (returns `200` regardless of `APP_LB_ADMIN_AUTH`), so the static
  re-probe stays green without credentials.
- **Trade-off vs. an external proxy.** Because the control plane now rides the
  data plane, a bad proxy/deployment edit can in principle disrupt the very
  dashboard you'd use to fix it. If you want the dashboard reachable independent
  of proxy state, terminate TLS in a dedicated reverse proxy (nginx / Caddy) on
  `443` → `127.0.0.1:9090` instead of using this deployment.

## `heyosecret.json` — managed Firecracker pool for the HeyoSecret store

[heyosecret](../../heyo-public/heyosecret) is a single-tenant encrypted secrets
store: a machine API at `/v1/secrets/*` (Bearer key) plus a server-rendered
admin dashboard at `/` (admin-password session cookie), both backed by Postgres.
Unlike the two examples above it is a **managed** deployment — app-lb boots and
autoscales a pool of Firecracker microVMs from a rootfs image.

### Prerequisites

Build the rootfs image on the app-lb host from the crate's `Dockerfile`:

```sh
cd ../../heyo-public/heyosecret
heyvm mvm build --local-only -f Dockerfile -n heyosecret --size-mb 512
heyvm mvm images     # expect a `heyosecret` row
```

The `vm.image` field names that local image. Then fill in the four `CHANGE_ME`
values (`HEYOSECRET_DATABASE_URL` password, `HEYOSECRET_INTERNAL_API_KEY`,
`HEYOSECRET_MASTER_KEY` — ≥32 bytes — and `HEYOSECRET_ADMIN_PASSWORD`) and post it:

```sh
curl -XPOST localhost:9090/deployments \
  -H 'content-type: application/json' \
  -d @examples/heyosecret.json
curl -H 'Host: secrets.local' localhost:6188/health
```

### Notes on the spec

- **Reaching Postgres is the one thing that needs care.** Each Firecracker VM
  gets its *own* `/30` tap subnet out of `172.16.0.0/12`, so the host-side
  gateway address is different for every VM — a hardcoded tap address works for
  one replica and breaks the next. Point the database URL at an address the
  guest reaches over its default route: the host's LAN/bridge IP (the example
  uses `192.168.4.56`, which on this host is a DHCP lease — pin it, or use a
  stable bridge address or DNS name). Postgres must also be listening on that
  address (`listen_addresses`) and allow the guest range in `pg_hba.conf`.

- **Secrets live in `env_vars`, which the admin API echoes back.** `GET
  /deployments` returns the full spec, master key included. Run app-lb with
  `APP_LB_DASHBOARD_PASSWORD` and `APP_LB_ADMIN_AUTH=1` so the CRUD API and its
  spec reads are gated, and keep real values out of this file — it is a
  template, not a place to commit credentials.

- **Host routing, not a path prefix.** The dashboard mounts at `/` and the
  machine API at `/v1/secrets/*`, with no base-path option; app-lb forwards the
  path unchanged, so a `path_prefix` rule would leave every route mismatched.
  Point a hostname (here `secrets.local`; use a real DNS name or an
  `/etc/hosts` entry) at app-lb instead.

- **Migrations are read from disk at startup**, not embedded in the binary, and
  the compile-time fallback path only exists in the Docker build stage. The
  image installs them at `/opt/heyosecret/migrations` and
  `HEYOSECRET_MIGRATIONS_DIR` points there; the `start_command` also `cd`s to
  `/opt/heyosecret` so the plain `./migrations` lookup resolves too.

- **`/health` is unauthenticated and means what it says.** The HTTP listener
  binds only *after* the store connects and migrations apply, so a green probe
  proves the database is actually reachable — not just that the VM booted. The
  flip side: with an unreachable database the process exits immediately, nothing
  ever listens on 4455, and app-lb recycles the VM on a loop. If a deployment
  never goes ready, `heyvm sh` into a VM and read `/var/log/heyosecret.log`.

- **`min_replicas: 1`, no scale-to-zero.** Other services call this store on
  their own request paths, so paying a cold start to answer a secret read is a
  bad trade; the floor of 1 keeps a VM warm. `target_concurrency: 32` is higher
  than the other examples because requests are short IO-bound database round
  trips, not CPU work — a `mini` VM (0.5 CPU, 1 GB) absorbs many at once, and a
  lower target would just churn a second VM for no latency benefit.

- **Replicas are safe, one caveat at first boot.** No local state (values never
  touch guest disk, hence no `disk_size_gb`), and the session-cookie signing key
  is derived from `master_key + admin_password`, so a cookie minted by one
  replica verifies on another. The migrations are all `IF NOT EXISTS` and
  idempotent, but two VMs booting *simultaneously* against an empty database can
  still collide inside `CREATE TABLE IF NOT EXISTS`. `warm_pool: 0` with
  `min_replicas: 1` means the first VM boots alone, which sidesteps it; if you
  raise the floor, let one VM initialize the schema before scaling up.

- **Set `HEYOSECRET_COOKIE_SECURE=true` once it is served over HTTPS** (app-lb's
  TLS listener, per the root README). app-lb terminates TLS and forwards
  plaintext to the guest, so the app cannot infer the scheme itself — the
  browser sees HTTPS and the `Secure` attribute is correct, but it has to be set
  explicitly. Leave it `false` while testing over plaintext `:6188`, or the
  dashboard login cookie is dropped and sign-in silently loops.
