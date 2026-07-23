# Example deployments

Ready-to-`POST` deployment specs for the app-lb admin API.

```sh
curl -XPOST localhost:9090/deployments \
  -H 'content-type: application/json' \
  -d @examples/pg-fc-dashboard.json
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
