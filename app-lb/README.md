# app-lb

An application load balancer for [heyvm](https://heyo.computer) Firecracker/KVM microVMs,
built on [Pingora](https://github.com/cloudflare/pingora).

Register a *deployment* — routing rules plus a backend — and app-lb routes HTTP traffic to it.
Deployments are registered at runtime over an admin API; multiple deployments coexist in one
process. A deployment is one of three kinds:

- **Managed** (`vm`): a VM template plus a scaling policy. app-lb boots and reaps a pool of
  microVMs to match load.
- **Static / `proxy_pass`** (`upstreams`): a fixed set of upstream addresses (`host:port` or
  `ip:port`) to forward to — another app or service. No VM lifecycle and no autoscaling; the
  upstreams are load-balanced least-in-flight with failover, and health-re-probed so a
  recovered upstream rejoins. See [Static / proxy_pass deployments](#static--proxy_pass-deployments).
- **Site** (`site`): a directory on the app-lb host, served straight off disk — no backend at
  all. What nginx's `root` or a CloudFront origin does. See [Static sites](#static-sites).

A deployment sets exactly one of `vm`, `upstreams` or `site`.

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
| `APP_LB_STATE_PATH` | `app-lb-state.json` | Names the state *directory* — see below |
| `APP_LB_SECRETS_PATH` | `app-lb-secrets.json` | Where stored secrets persist (written `0600`) |
| `APP_LB_SECRET_KEY` | *(unset)* | 32-byte hex key (or any passphrase) that seals the secrets file with AES-256-GCM |
| `APP_LB_TOKENS_PATH` | `app-lb-tokens.json` | Where minted [app-tokens](#app-tokens) persist (written `0600`; only hashes) |
| `APP_LB_AUTH_KEY` | `app-lb-auth-key` | Signing key for sign-in sessions; generated `0600` on first use |
| `APP_LB_NAME` | `app-lb` | Display name in the dashboard header and page title |
| `APP_LB_DAEMON_URL` | `http://127.0.0.1:34099` | heyvm daemon |
| `APP_LB_DASHBOARD_PASSWORD` | *(unset)* | Set to gate the dashboard behind HTTP Basic Auth |
| `APP_LB_DASHBOARD_USER` | `admin` | Basic Auth username (only used when a password is set) |
| `APP_LB_ADMIN_AUTH` | `false` | `1`/`true` to extend the gate to the deployment CRUD API (needs a password) |
| `APP_LB_TLS_CERT` | *(unset)* | PEM cert path; set with `APP_LB_TLS_KEY`. The fallback cert when ACME is on |
| `APP_LB_TLS_KEY` | *(unset)* | PEM private-key path |
| `APP_LB_PROXY_TLS_ADDR` | `0.0.0.0:6189` | HTTPS listener (bound when ACME is on or cert+key are set) |
| `APP_LB_ACME_EMAIL` | *(unset)* | Let's Encrypt account contact; **setting it enables automatic certificates** |
| `APP_LB_ACME_DIR` | `/var/lib/app-lb/acme` | ACME account key and issued certificates (should be `0700`) |
| `APP_LB_ACME_DIRECTORY` | LE production | ACME directory URL — point at staging for testing |
| `APP_LB_ACME_WILDCARD` | *(unset)* | Comma-separated domains to cover with a **wildcard** cert (`sb.example.com` → also `*.sb.example.com`). Issued over DNS-01; needs the zone id below |
| `APP_LB_ROUTE53_ZONE_ID` | *(unset)* | Route 53 hosted zone where DNS-01 challenge records are written |
| `APP_LB_AWS_BIN` | `aws` | The AWS CLI used for DNS-01. Only reached when a wildcard is configured |
| `APP_LB_BUILD_DIR` | `/var/lib/app-lb/builds` | Git checkouts for image builds (one per deployment, `0700`) |
| `APP_LB_HEYVM_BIN` | `heyvm` | The heyvm CLI that builds guest images |
| `APP_LB_ART_BIN` | `art` | The `art` CLI that materializes a rootfs from a **local** artifact store; unused when `artifact.store` is a URL |
| `APP_LB_IMAGES_DIR` | `$MVM_DATA_DIR/images/firecracker`, else `<home>/.heyo/images/firecracker` | Where a pulled rootfs is written. Must be the directory heyvmd resolves image names in |
| `APP_LB_GIT_BIN` | `git` | The git binary used for checkouts |
| `APP_LB_UPDATE_SHELL` | `/bin/sh` | Shell a static deployment's `update.commands` run through |
| `APP_LB_BUILD_TIMEOUT_SECS` | `1800` | Ceiling on one build step or update command, after which the child is killed |
| `APP_LB_HEYVM_HOME` | *(unset)* | `HOME` for the heyvm child — set it when app-lb and heyvmd run as different users |
| `APP_LB_OBS_URL` | *(unset)* | Where app-obs listens (e.g. `127.0.0.1:9500`); **setting it enables log shipping** |
| `APP_LB_OBS_TOKEN` | *(unset)* | Bearer token for app-obs's `/ingest` — must match its `APP_OBS_INGEST_TOKEN` |
| `APP_LB_OBS_HOST` | `/etc/hostname` | Machine name stamped on every batch |
| `APP_LB_OBS_DEPLOYMENT` | `_lb` | Deployment id in app-obs for app-lb's *own* records — name it per host (`lb-us2`) when several LBs ship to one collector |
| `APP_LB_OBS_ACCESS_LOG` | `true` | `0` to stop shipping the per-request access log |
| `APP_LB_OBS_EVENTS` | `true` | `0` to stop shipping app-lb's own log events (and deploy-job output) |
| `APP_LB_SIEM` | `true` | `0` to turn [security monitoring](#security-monitoring) off entirely |
| `APP_LB_SIEM_QUEUE_CAPACITY` | `4096` | Observations buffered for analysis before new ones are dropped |
| `APP_LB_SIEM_ALERT_CAPACITY` | `512` | Alerts held in memory for `GET /security` |
| `APP_LB_SIEM_WINDOW_SECS` | `60` | Window every rate-based rule counts over |
| `APP_LB_SIEM_MAX_CLIENTS` | `16384` | Source addresses tracked at once (~2 MB at the default) |
| `APP_LB_SIEM_AUTH_THRESHOLD` | `8` | Authentication failures per window from one source before an alert |
| `APP_LB_SIEM_SCAN_THRESHOLD` | `30` | 4xx responses per window from one source before it reads as scanning |
| `APP_LB_SIEM_RATE_THRESHOLD` | `600` | Requests per window from one source before it reads as a spike |
| `APP_LB_SIEM_SUPPRESS_SECS` | `300` | Repeats inside this fold into the open alert instead of raising a new one |
| `APP_LB_SIEM_MAX_ALERTS_PER_MIN` | `60` | Hard ceiling on *new* alerts, for a distributed attack that cannot fold |
| `APP_LB_SIEM_SCAN_QUERY` | `true` | `0` to match signatures against the path only, never the query string |
| `APP_LB_SIEM_SHIP` | `true` | `0` to keep alerts local instead of sending them to app-obs |
| `APP_LB_OBS_QUEUE_CAPACITY` | `8192` | Records buffered before new ones are dropped |
| `APP_LB_OBS_BATCH` | `500` | Records per POST |
| `APP_LB_OBS_FLUSH_SECS` | `2` | How long a record may wait for a fuller batch |
| `RUST_LOG` | `info,app_lb=debug` | Log filter |

### Where state lives

Deployments persist as one file per deployment, in a directory beside the path
`APP_LB_STATE_PATH` names: `app-lb-state.json` → `app-lb-state.d/<id>.json`.
Each file holds the spec and the runtime state app-lb has to remember across a
restart.

One file per deployment rather than one file for all of them, because writing
every spec on every change is quadratic in a fleet: registering the thousandth
sandbox would rewrite the other 999 with it. Registering, editing and
deregistering are each a single file write or unlink, whatever the fleet size.

**Upgrading is automatic and one-way.** On first start with this version an
existing `app-lb-state.json` is imported and then renamed to
`app-lb-state.json.migrated`. It is renamed rather than deleted so a downgrade
still has the data, and so a second start cannot re-import deployments that were
deregistered in between — which would resurrect them. Nothing to run by hand;
the log says how many were migrated.

A file whose spec no longer validates is skipped with a warning and **left
alone**, because that file is the only copy of what somebody wrote. The startup
sweep that removes files no deployment claims stands down entirely when that
happens, for the same reason.

## CLI

[`serverctl`](serverctl/README.md) is a kubectl-shaped CLI over the admin API below — the same
operations without hand-written `curl`, plus saved server/credential contexts, tables, `$EDITOR`
round-trips and rollout waiting. It is a separate crate, so installing it doesn't pull in pingora
or the ACME stack.

```sh
cargo build --release -p serverctl

serverctl login --server 127.0.0.1:9090   # saves a context; prompts if the server is gated
serverctl create deployment demo --host demo.local --image nginx --port 80 --min 0 --max 4
serverctl rollout status demo
serverctl get deployments -o wide
serverctl scale demo --min 2 --max 8
serverctl restart demo                    # drain every VM; the autoscaler replaces them
serverctl top                             # per-deployment CPU, memory, latency, 5xx

# Ship a rootfs through an artifact store, and boot it somewhere else.
serverctl artifact login http://10.0.0.4:8080   # saves a registry; prompts for the API key
serverctl artifact push --image web-v2          # a heyvm image, or a path to an .ext4
serverctl pull demo --wait                      # materialize it here and roll the pool
```

See [`serverctl/README.md`](serverctl/README.md) for the full command set, the context/credential
model, and which commands apply to managed versus static deployments.

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
    "drain_timeout_secs": 30,
    "boot_timeout_secs": 300
  },
  "health": {"path": "/", "timeout_secs": 2}
}'

curl localhost:9090/deployments          # list, with live VM state
curl localhost:9090/deployments/demo     # one deployment
curl -XDELETE localhost:9090/deployments/demo   # drain and reap every VM
curl localhost:9090/healthz
curl localhost:9090/metrics              # metrics snapshot (JSON)
curl localhost:9090/certs                # issued TLS certificates and expiry

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

### Static sites

A deployment with a `site` block has **no backend at all**: app-lb answers the
request itself, out of a directory on its own host. What nginx's `root` or a
CloudFront origin bucket does.

```sh
curl -XPOST localhost:9090/deployments -H 'content-type: application/json' -d '{
  "id": "docs",
  "routes": [{"host": "docs.example.com"}],
  "site": {"root": "/srv/docs/dist", "not_found": "404.html"}
}'
```

```sh
serverctl create deployment docs --host docs.example.com \
  --site-root /srv/docs/dist --site-404 404.html
```

| Field | Default | |
| --- | --- | --- |
| `root` | *(required)* | Absolute path to the directory to serve |
| `index` | `index.html` | Served for a directory. `""` makes those a 404 |
| `not_found` | *(none)* | Body for a 404, relative to `root`. Absent = plain text |
| `spa` | `false` | Serve `index` for any unmatched path, with a 200 |
| `cache_control` | `public, max-age=300` | `Cache-Control` on served files |

What you get: an index, a custom 404, content types, `ETag` with `304` on
`If-None-Match`, `Accept-Ranges` and byte ranges (so a paused download or a
seeking `<video>` works), `HEAD`, and a `301` from `/docs` to `/docs/` so
relative links resolve. Files are streamed in 64 KiB pieces rather than read
whole, so a large asset does not pin its own size in memory per concurrent
request.

What you don't: rewrites, redirects, per-location blocks, directory listings,
compression. A site that needs those wants a real web server behind a
`proxy_pass` deployment.

**Nothing outside `root` is ever served.** Every path component is checked before
the filesystem is touched — which rejects `..`, an encoded `%2e%2e`, an absolute
path and a NUL — and the resolved file is then confirmed to still be under the
root *after* symlinks are followed, which is the only way to catch a symlink
inside the root pointing out of it. A traversal is refused rather than
sanitised: quietly dropping a `..` would turn an attack into a successful read
of a different file.

`spa: true` is for single-page apps: any path matching no file is served the
index with a 200, so a client-side router owns the URL space. Off by default,
because it turns every typo into a 200. It never rescues a traversal.

**Deploying one.** A site takes an `update` block, the same as a static
deployment — commands run in a directory on the app-lb host:

```jsonc
"update": {
  "working_dir": "/srv/docs",
  "commands": ["git pull --ff-only", "npm ci", "npm run build"]
}
```

`serverctl update docs` runs them and then checks the site is still servable —
that `index` is actually in `root`. A build that exits 0 but writes its output
somewhere else fails the job, rather than leaving a deployment that 404s
everything. (`build` and `artifact` are rejected on a site: there is no image and
no pool.)

Scaling, eviction, `exec` and `shell` all do not apply and are rejected with a
message saying so. A site is only reachable through the proxy, so it needs at
least one route.

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

## Running things inside a VM

Two endpoints reach into a deployment's VM without going through the proxy.
They matter most for a deployment with **no routes at all** — an agent sandbox —
where they are the only ways in.

```sh
# One command. The guest's exit code comes back in the JSON.
curl -XPOST localhost:9090/deployments/sb-7f3a9c/exec \
  -H 'content-type: application/json' \
  -d '{"command":"ls -la /workspace","cwd":"/workspace"}'

# An interactive PTY, over a WebSocket.
serverctl shell sb-7f3a9c
```

| | |
| --- | --- |
| `POST /deployments/:id/exec` | `{command, cwd?, env?, timeout_secs?, wake?}` → `{sandbox_id, exit_code, stdout, stderr, output}` |
| `GET /deployments/:id/shell` | WebSocket upgrade; `?cols=&rows=&cwd=&wake=` |

Both are on the CRUD tier, so `APP_LB_ADMIN_AUTH=1` covers them. It should:
running a command in a VM is at least as powerful as editing the spec that boots
it.

**Both will start a VM.** `wake` defaults to true, so `exec` against a sandbox
that has scaled to zero boots or resumes one and waits for it, bounded by
`cold_start_timeout_secs`. That wait is the proxy's own cold-start path, which
means it also gets the autoscaler's preference for resuming a suspended VM over
booting a fresh one. Pass `wake: false` (or `--no-wake`) for a `409` instead,
when you want to know rather than wait.

**Both hold the VM for the session's life.** A command or an open shell takes an
in-flight slot exactly as a proxied request does. Without that, an unrouted
sandbox looks permanently idle — nothing else moves its activity clock — and a
deployment with `scale_to_zero_after_secs` set would reap the VM out from under
a live shell.

Why these are proxied rather than app-lb handing back a daemon address: only
app-lb knows which sandbox is currently serving a deployment id, only app-lb can
apply the admin gate, and only app-lb can wake a suspended VM. A client talking
to heyvmd directly would need daemon reachability and credentials, and would
still find nothing running.

An `exec` distinguishes two failures that look alike. A command that runs and
fails is `200` with a non-zero `exit_code`; app-lb failing to *run* it is a
`502`. The shell endpoint refuses **before** the WebSocket upgrade, so a bad
request is a real status code with a JSON body rather than a socket that closes
without saying why.

## Google sign-in

Any deployment can be put behind Google sign-in by adding an `auth` block. The
gate runs in the proxy, before a backend is chosen, so it works the same for a
managed VM pool and a static `proxy_pass` target — and the application behind it
needs to know nothing about OAuth. It sees only requests that got through.

```jsonc
{
  "id": "web",
  "routes": [{"host": "web.example.com"}],
  "upstreams": ["127.0.0.1:8080"],
  "auth": {
    "client_id": "1234-abc.apps.googleusercontent.com",
    "client_secret": {"secret": "google", "key": "client_secret"},
    "allowed_domains": ["example.com"],       // matched on the Workspace `hd` claim
    "allowed_emails": ["contractor@gmail.com"],
    "public_paths": ["/healthz", "/hooks/"],  // served without the gate
    "session_ttl_secs": 43200,
    "base_path": "/__applb/auth",             // where app-lb's own endpoints live
    "forward_identity": true
  }
}
```

### Setting it up

1. In the Google Cloud console, create an **OAuth 2.0 Client ID** of type *Web
   application*, and register the redirect URI — `https://<hostname><base_path>/callback`,
   so with the defaults above: `https://web.example.com/__applb/auth/callback`.
   `serverctl describe deployment web` prints the exact string to paste.
2. Store the client secret. It is a [secret](#secrets), not a spec field:

   ```sh
   serverctl create secret google --from-stdin client_secret < ~/.google-oauth-secret
   ```
3. Add the gate:

   ```sh
   serverctl set auth web \
     --client-id 1234-abc.apps.googleusercontent.com \
     --secret google/client_secret \
     --allow-domain example.com \
     --public-path /healthz
   ```

The pool is untouched — the gate is proxy configuration, not part of the VM
template, so turning it on or off never restarts anything.

### What a request meets

| | |
| --- | --- |
| No session, browser | `302` to Google, then back to what was originally asked for |
| No session, API client | `401` + `{"error":…,"login_url":…}` — a redirect it would only mis-parse |
| Valid session | proxied, with `x-auth-request-{email,user,name}` |
| Signed in, not on the allow-list | `403` naming the account, and a link to sign in as someone else |
| A path under `public_paths` | proxied, gate skipped |

`<base_path>/login` starts a sign-in (useful as a "sign in" link, and after a
logout); `<base_path>/logout` clears app-lb's cookie. Logging out of Google
itself is deliberately not app-lb's business — it would sign the user out of
every other tab.

### How it works, and what that buys

The flow is the OAuth 2.0 authorization code grant **with PKCE**. Both cookies
are self-describing and signed with HMAC-SHA256 (`APP_LB_AUTH_KEY`, generated
`0600` on first use), so:

- **There is no session store** to grow, replicate or lose. A restart does not
  sign anyone out — the key is persisted.
- **A session is bound to its deployment**, so a cookie from one gated
  deployment is not a cookie for another with a narrower allow-list.
- **Tightening the allow-list signs people out.** Each session carries a
  fingerprint of the policy that admitted it; changing `client_id`,
  `allowed_domains` or `allowed_emails` invalidates every session issued under
  the old one, rather than leaving a removed user signed in until their cookie
  expires.

### Who may enter

Both lists take any number of entries and are **OR'd** — one match admits the
caller:

```jsonc
"allowed_domains": ["sarocu.com", "heyo.computer"],
"allowed_emails": ["contractor@gmail.com", "auditor@example.org"]
```

```sh
serverctl set auth web \
  --allow-domain sarocu.com --allow-domain heyo.computer \
  --allow-email contractor@gmail.com --allow-email auditor@example.org
```

Matching is case-insensitive, and a leading `@` on a domain is tolerated —
`@example.com` and `example.com` are the same rule.

**Domains are matched on the `hd` claim, not the email suffix.** Only `hd` says
the account is *governed by* that Workspace domain. A personal Google account
can carry any address a Workspace admin has not claimed, so trusting the suffix
would let `someone@yourcompany.com` in from an account you do not control.

Two consequences worth knowing:

- **A personal Google account has no `hd` at all**, so no `allowed_domains` entry
  can ever match it — list the address in `allowed_emails` instead. This is also
  the answer when a domain you own is not actually a Workspace domain: check with
  `dig +short MX <domain>`, and if the MX records are not Google's, every account
  there is a personal one as far as this claim is concerned.
- **A Workspace with several domains needs each domain that appears in `hd`.**
  Secondary domains generally report their own; for *alias* domains verify against
  a real sign-in before trusting it, since Google may return the primary domain
  instead. `allowed_emails` sidesteps the question entirely.

**An empty allow-list is rejected.** Gating behind "has a Google account" admits
most of the internet, which is a real thing to want and has to be said out loud:
`"allowed_domains": ["*"]`.

**Editing a list replaces it.** Every `--allow-domain`/`--allow-email` you pass
replaces that whole list rather than adding to it, so growing the list means
resending all of it. `serverctl edit deployment <id>` is the incremental
alternative — it opens the spec in `$EDITOR` and changes only what you change.

**Adding or removing an entry signs everyone out once.** The allow-list is part
of the policy fingerprint each session carries (see above), so a change bounces
every current user through Google and straight back in. Reordering the list or
changing its capitalisation costs nothing: the fingerprint sorts and lowercases
first, so only a real change to *who* may enter invalidates anything.

**The identity headers are stripped from every incoming request** to a gated
deployment before app-lb sets them, so a client cannot present its own
`x-auth-request-email`.

### Limits

- **One provider.** Google only; `provider` exists so a spec written today still
  parses when there is a second.
- **The `Secure` cookie attribute follows the connection.** Over plaintext
  `:6188` the session cookie is not `Secure` — fine for a local trial, but a
  gate is only meaningfully protecting anything over HTTPS. Use the [TLS
  listener](#tls).
- **A path-routed deployment needs `base_path` under its prefix.** A deployment
  routed only at `/app` would 404 the provider's redirect to
  `/__applb/auth/callback`; set `base_path` to something like `/app/__auth`.
  Registration rejects the combination rather than letting the first sign-in
  discover it.
- **Sessions are not revocable individually.** Ending one specific person's
  access means removing them from the allow-list, which ends everyone's sessions
  (they simply sign in again). There is no session list to revoke from — that is
  the trade for having no session store.

- **A gate admits browsers and nothing else.** There is no way for a headless
  client to sign in with Google, so the gate refuses one rather than sending it
  somewhere it cannot go. The split is `Accept: text/html`
  (`proxy.rs`, `wants_html`):

  | Client | Unauthenticated request gets |
  | --- | --- |
  | a browser navigating (`Accept: text/html`) | `302` to Google |
  | anything else — curl, CI, serverctl, **and a page's own `fetch()`** | `401` + `{"error":"authentication required","login_url":"…"}` |

  The `login_url` is only useful to something that can open a browser. Put every
  path a machine calls in `public_paths` and protect those with a credential the
  machine can actually present — an API key, Basic auth — exactly as
  [`examples/artifacts-gated.json`](examples/artifacts-gated.json) does for the
  store's API.

  **`fetch()` from an already-signed-in page counts as a machine.** A browser
  sends `Accept: text/html` when *navigating*, but its XHR does not, so a page
  that loads fine can have every one of its background calls refused. app-lb's
  own dashboard is the worked example: gating it puts the page behind Google
  correctly, and then its `/metrics` poll — sent with `accept: application/json`
  — gets a `401` and no numbers ever appear. See
  [Putting the dashboard behind Google](#putting-the-dashboard-behind-google).

## Building images from git

A managed deployment can say where its guest image *comes from*, not just what it
is called. Add a `build` block naming a git repo and a Dockerfile, and
`POST /deployments/:id/build` will check the repo out on the app-lb host, hand
the Dockerfile to `heyvm mvm build`, and — when that succeeds — rewrite the
deployment's `vm.image` to the image it produced, which recycles the pool onto
it.

```jsonc
{
  "id": "web",
  "routes": [{"host": "web.example.com"}],
  "vm": {"driver": "firecracker", "image": "web-3f2a1c8e9b0d", "port": 8080},
  "build": {
    "repo": "https://github.com/acme/web.git",
    "ref": "main",              // branch, tag or commit; omit for the default branch
    "dockerfile": "Dockerfile", // omit to let app-lb find one
    "context": ".",             // omit to use the Dockerfile's directory
    "image_size_mb": 768,       // omit to let heyvm size it from the image contents
    "auth": {"secret": "github", "key": "token"}   // omit for a public repo
  }
}
```

```sh
# Build the ref in the spec, and roll the pool onto the result.
curl -XPOST localhost:9090/deployments/web/build          # 202 + a job record

# Build a different ref, just this once. The spec's `ref` is left alone.
curl -XPOST localhost:9090/deployments/web/build -H 'content-type: application/json' \
  -d '{"ref": "v2.1.0"}'

curl localhost:9090/jobs                  # every remembered job, newest first
curl localhost:9090/deployments/web/jobs
curl localhost:9090/jobs/job-3f2a1c8e     # status, commit, image and log tail
```

The same job history covers the other two verbs —
[`pull`](#pulling-images-from-an-artifact-store) and
[`update`](#updating-a-static-deployment) — so `GET /jobs` is one listing of
everything that has changed a backend.

Builds are asynchronous — a `docker build` takes minutes, so `POST` returns `202`
with a record and the outcome is polled from `GET /jobs/:id`. One job runs per
deployment at a time; a second request while one is in flight is a `409`. Job
records live in memory, so they do not survive a restart — the durable outcome of
a build is the `image` in the persisted spec.

Retention is **per deployment**: the 20 most recent jobs each, under a 2000-job
ceiling for the whole LB. Per-deployment rather than one global cap, because a
global cap is a race and not a policy — one deployment pulling images in a loop
would evict everybody else's history, and the job you came to investigate is the
one already gone.

**Where it runs.** On the app-lb host, because heyvm has no build API: turning a
Dockerfile into an ext4 rootfs needs a local `docker`, `mke2fs` (e2fsprogs) and
`fakeroot`. The image lands in `$HOME/.heyo/images/firecracker/<name>.ext4`, and
the daemon only looks under *its own* home — so app-lb should run as the same
user as `heyvmd`, or be given `APP_LB_HEYVM_HOME`. Otherwise the build succeeds
and then nothing boots.

**Image names carry the commit.** Each build produces `<name>-<short sha>`
(`<name>` defaults to the deployment id, override with `build.image_name`), so
the running spec answers "what is deployed?" with something you can look up in
the repo. Old images stay on disk — `heyvm mvm images` lists them, and pruning
them is manual.

**Finding the Dockerfile.** With `build.dockerfile` set, that path is used and a
miss is an error. Without it, app-lb looks for `Dockerfile` at the context root,
then searches up to three directories deep (skipping `.git`, `node_modules`,
`target`, `vendor`, `dist`, `.venv`). Exactly one match is used; several is an
error naming them, because picking one would make the deployed image depend on
directory iteration order.

**Editing `build` never disturbs running VMs.** It is not part of the `vm`
template — it says where the *next* image comes from. The pool moves when a build
finishes.

A static (`upstreams`) deployment cannot have a `build` block: it has no guest
image, it forwards to something somebody else runs. Its update path is
[`update`](#updating-a-static-deployment) instead, and declaring the wrong one is
rejected at registration.

## Pulling images from an artifact store

Building is one way to get a `vm.image`. The other is to pull one somebody
already built. An `artifact` block names a store and a reference;
`POST /deployments/:id/pull` resolves that reference to a rootfs blob,
materializes it as an `.ext4` heyvmd can boot, and rewrites `vm.image` — the same
ending a build has, without the build.

The store is [artifacts](https://github.com/sarocu/artifacts): content-addressed,
ext4-native, and already where `art heyvm import` keeps heyvm's base images.

```jsonc
{
  "id": "web",
  "routes": [{"host": "web.example.com"}],
  "vm": {"driver": "firecracker", "image": "web-1b9b737b73e2", "port": 8080},
  "artifact": {
    "store": "http://10.0.0.4:8080",   // an `art serve`, or an absolute store root
    "ref": "web-v2",                    // a tag, or a 64-hex digest
    "grow_gb": 8,                       // omit to keep the image at its stored size
    "auth": {"secret": "art", "key": "api_key"}   // omit for an ungated store
  }
}
```

```sh
# Pull the ref in the spec, and roll the pool onto the result.
curl -XPOST localhost:9090/deployments/web/pull           # 202 + a job record

# Pull a different ref, just this once. The spec's `ref` is left alone, which is
# what makes a digest here a rollback rather than a config change.
curl -XPOST localhost:9090/deployments/web/pull -H 'content-type: application/json' \
  -d '{"ref": "1b9b737b73e26aa4c55d7b609351fa51f0e21b0b6afbaa9ef9f4561dd18337d7"}'

# Re-fetch even if the image is already on disk.
curl -XPOST localhost:9090/deployments/web/pull -H 'content-type: application/json' \
  -d '{"force": true}'
```

Pulls share the job machinery with builds: `202` with a record, polled from
`GET /jobs/:id`, one job per deployment at a time. A record carries the `store`,
the `artifact` reference asked for, the `digest` it resolved to, and the `bytes`
transferred.

**Two transports, chosen by how `store` is spelled.** They are not fallbacks for
each other — they are different situations:

| `artifact.store` | What happens |
| --- | --- |
| `http://host:port` | app-lb resolves and streams the blob itself, verifying the sha256 as it lands. One store feeds a fleet; no `art` binary needed on this host. |
| `/abs/path` | app-lb runs `art heyvm materialize`, which skips the blob's holes instead of copying its zeros. Needs the `art` CLI here (`APP_LB_ART_BIN`), and is dramatically cheaper. |

The difference is not small. Materializing a 48 MiB image from a local store
writes **48 KiB**; the same image over HTTP transfers all 48 MiB. A host running
its own store should name the path.

**The digest is verified on the way in.** A rootfs fetched over a network is what
the kernel boots, so the wire path hashes what arrives and refuses anything that
does not match the digest it asked for — the partial file is removed, the job
fails, and `vm.image` is untouched. The local path gets this for free, because
`art` hashes on materialize.

**Image names carry the digest.** Each pull produces `<name>-<12 hex>` (`<name>`
defaults to the deployment id, override with `artifact.image_name`). That is a
pure function of the content, so an image already on disk is proof the right
bytes are there and the fetch is skipped entirely — a re-pull of unchanged bytes
costs one round trip. The job still rolls the pool, because running VMs hold a
copy of whatever rootfs *they* booted from.

**A tag is resolved at pull time; a digest is not.** A deployment pinned to a tag
follows wherever that tag is moved, which is what makes `serverctl artifact push
--tag web-v2` a deploy. Naming a digest pins the bytes forever, which is what a
rollback should do.

**`grow_gb` is sparse.** The file is extended with `ftruncate`, so it costs no
disk until the guest writes; heyvm runs the `resize2fs` that lets the guest
filesystem use the room. Set it when the image was built small and the workload
needs space on `/`.

**Where it lands.** `APP_LB_IMAGES_DIR`, defaulting to
`$MVM_DATA_DIR/images/firecracker` and then to
`<home>/.heyo/images/firecracker` — where `<home>` honours `APP_LB_HEYVM_HOME`,
because the daemon only finds images under its own home. If none of those can be
worked out, app-lb still starts and warns; only pulls fail.

**`build` and `artifact` are mutually exclusive.** Both rewrite `vm.image` when
they run, so a deployment holding both would have no answer to where the running
image came from. To do both, build on one host and
[`serverctl artifact push`](serverctl/README.md#pushing-an-image-to-an-artifact-store)
the result for the others to pull. A static (`upstreams`) deployment cannot have
an `artifact` block either, for the same reason it cannot have a `build`.

The CLI side is `serverctl pull` and `serverctl set artifact`, plus
`serverctl artifact` for talking to the store itself — see
[`serverctl/README.md`](serverctl/README.md#artifact-stores).

## Updating a static deployment

A static deployment's backend is a process on some host — usually *this* host,
under supervisord or systemd. There is no image to build, so its update path is
the thing a person would otherwise ssh in and do: a working directory, and
commands to run in it.

```jsonc
{
  "id": "app-obs",
  "routes": [{"host": "obs.example.com"}],
  "upstreams": ["127.0.0.1:9600"],
  "health": {"path": "/healthz", "timeout_secs": 2},
  "update": {
    "working_dir": "/home/sarocu/Projects/app-obs",
    "commands": [
      "git pull --ff-only",
      "cargo build --release",
      "supervisorctl restart app-obs"
    ],
    "verify_timeout_secs": 60,   // 0 disables the post-update health check
    "timeout_secs": 1800,        // per command
    "env": {"CARGO_TERM_COLOR": "never"},
    "env_from": [{"secret": "obs", "key": "ingest_token", "as": "APP_OBS_INGEST_TOKEN"}],
    "auth": {"secret": "github", "key": "token"}   // for a private `git pull`
  }
}
```

```sh
curl -XPOST localhost:9090/deployments/app-obs/update   # 202 + a job record
curl localhost:9090/jobs/job-2c9d7d7d          # commands_run, verified, log tail
```

Same shape as a build: `202` immediately, one job per deployment at a time, and
the outcome polled from `GET /jobs/:id`. Each command is run through `sh -c`
(`APP_LB_UPDATE_SHELL`) with `working_dir` as its CWD, in order, and the first
non-zero exit stops the job — `commands_run` says how far it got.

**Nothing in the spec changes.** The upstreams are the same addresses; what moved
is the code answering on them. That is exactly why the job then **re-probes those
addresses** with the deployment's own health check, until they all answer or
`verify_timeout_secs` runs out. A job whose commands exited 0 but whose service
never came back is reported as *failed*, and says so:

```
every command succeeded, but 1 of 1 upstream(s) did not answer within 60s
(127.0.0.1:9600). The host has already been changed — check the service and its logs
```

The probe is a fresh one, not the autoscaler's cached `healthy` flag: a flag set
two seconds ago describes the process that was just replaced.

**The commands run as app-lb's user**, in app-lb's environment. Two things follow:

- The working directory has to be readable and writable by that user. app-lb never
  creates it — a typo that silently created an empty directory and ran `git pull`
  in it would be worse than an error.
- Restarting a service usually needs a little more. `supervisorctl` needs access
  to supervisord's socket (add the app-lb user to the socket's `chown` group in
  `supervisord.conf`); `systemctl restart` needs a polkit rule or a specific
  `sudo -n` entry. Grant exactly the one verb, not general sudo — the admin API
  is what triggers this.

`env_from` pulls values from the [secret store](#secrets) rather than putting them
in the spec, and `auth` supplies a git credential the same way a build does. A
managed (`vm`) deployment cannot declare `update`: its backends are microVMs, and
a directory on this host would update nothing.

### Secrets

A private repo needs a credential, and a deployment spec is the wrong place for
one — the admin API echoes specs back verbatim and the state file holds them in
the clear. So credentials are their own object, stored apart from deployments,
and a spec refers to one by name:

```sh
# Store one. Values go in and are never readable back out.
curl -XPOST localhost:9090/secrets -H 'content-type: application/json' \
  -d '{"id": "github", "description": "CI PAT for acme/*", "data": {"token": "ghp_…"}}'

curl localhost:9090/secrets          # ids, key *names*, and when each changed
curl localhost:9090/secrets/github   # the same, for one secret

# Rotate one key without resending the others (`null` removes a key).
curl -XPATCH localhost:9090/secrets/github -H 'content-type: application/json' \
  -d '{"data": {"token": "ghp_new…"}}'

curl -XDELETE localhost:9090/secrets/github   # 409 while a deployment still refers to it
```

There is deliberately no endpoint that returns a value. Nothing app-lb does
needs one: the builder resolves `build.auth` in-process, and a read-back would
turn the admin API into a credential store with a `GET`.

The token reaches git through `GIT_ASKPASS` and the child's environment, never
through the URL or the command line — a credential in a remote URL lands in
`.git/config` and in every `ps` on the box. `build.auth` only applies to HTTP(S)
remotes; an `ssh://` or `git@` remote authenticates with the host's own key
material and should leave it unset.

**At rest**, secrets live in `APP_LB_SECRETS_PATH` (default
`app-lb-secrets.json`) with mode `0600`. Set `APP_LB_SECRET_KEY` and the file is
sealed with AES-256-GCM instead — ids, key names and values all — so a copied
backup of the state directory is not a copied credential:

```sh
APP_LB_SECRET_KEY=$(openssl rand -hex 32) ./target/release/app-lb
```

A 64-character hex value is used as the key directly; anything else is hashed to
32 bytes, so a passphrase works too. Setting the key on an existing plaintext
file adopts it and encrypts on the next write. Starting **without** the key that
sealed a file is a hard startup failure rather than an empty store — coming up
empty would let the next write destroy secrets that are perfectly good and merely
unreadable.

> A build runs `git` and `docker` on the app-lb host, and an update runs whatever
> the deployment's `commands` say, so an ungated admin API is a remote code
> execution surface. It binds loopback by default; set `APP_LB_ADMIN_AUTH=1`
> (with `APP_LB_DASHBOARD_PASSWORD`) before exposing it. app-lb warns at startup
> when it is open.

## Routing

A deployment's `routes` is an array of **rules**. The rules are the only knob
that decides which requests reach a deployment — matching is identical for
managed and static/`proxy_pass` deployments (the kind only changes what a matched
request is forwarded *to*). A request is routed to a deployment when **any** one
of its rules matches (rules are OR'd); within a single rule, **every** field the
rule sets must match (fields are AND'd).

A rule may set any combination of three fields:

| Field | Matches | Notes |
| --- | --- | --- |
| `host` | exact hostname | case-insensitive, port stripped |
| `host_suffix` | a domain and its subdomains | anchored at a label boundary |
| `path_prefix` | a leading path segment, e.g. `/api` | prefix, not exact; not stripped |

- **`host`** — exact hostname match, e.g. `{"host": "demo.local"}` matches only
  `demo.local` (any port).
- **`host_suffix`** — **subdomain / wildcard** match. `{"host_suffix":
  "apps.example.com"}` matches the apex `apps.example.com` **and** any subdomain
  (`a.apps.example.com`, `x.y.apps.example.com`), anchored at a label boundary so
  `notapps.example.com` does **not** match. A leading dot is accepted and ignored.
- **`path_prefix`** — the request path *starts with* this string, e.g.
  `{"path_prefix": "/api"}` matches `/api`, `/api/v1`, and also `/apidocs` (it is
  a raw string prefix, not a path-segment match). The prefix is **not stripped** —
  the upstream sees the full original path, so front apps that serve their routes
  at the root by `host`/`host_suffix`, not a prefix.

Fields combine within a rule. `{"host": "demo.local", "path_prefix": "/api"}`
matches only requests that are *both* for `demo.local` *and* under `/api`. Use
several rules in the array to express alternatives:

```jsonc
"routes": [
  { "host": "demo.local" },                          // exact host …
  { "host_suffix": "apps.example.com" },             // … OR any *.apps.example.com …
  { "host": "demo.local", "path_prefix": "/admin" }  // … OR /admin on that host
]
```

### Precedence — most specific wins

When more than one deployment's rules could match a request, the **single
most-specific rule across all deployments** decides — not registration order. The
tiers don't overlap:

1. an exact **`host`** rule beats
2. any **`host_suffix`** rule, which beats
3. any bare **`path_prefix`** rule.

Within a tier, a **longer** suffix or a **longer** prefix wins (e.g.
`host_suffix: "eu.apps.example.com"` outranks `host_suffix: "apps.example.com"`;
`path_prefix: "/api/v2"` outranks `/api`). A rule that sets both a host tier and a
path adds the two together, so `{host, path_prefix}` outranks the same host with
no path. Exactly-equal-specificity rules on two deployments are broken by
deployment id (lexicographic) so resolution is deterministic regardless of
registration order. This lets a specific carve-out (`{"host": "x", "path_prefix":
"/legacy"}` → an old backend) sit alongside a catch-all (`{"host": "x"}` → the
new backend) and win for its prefix only.

Host matching (exact or suffix) is case-insensitive, strips the port, and falls
back to HTTP/2's `:authority` when there is no `Host` header. A request that
matches no rule anywhere is a **404**. Every route must set at least one field —
an empty rule `{}` is rejected at registration.

### Deployments with no routes

`"routes": []` is legal for a **managed** deployment and means exactly what it
says: nothing reaches it through the proxy. It is still registered, still
autoscaled, and still reachable by id through the admin API.

That is the normal shape for an agent sandbox. A sandbox is worked on by
`serverctl exec` and `serverctl shell`, not by HTTP, and putting it on a
hostname it doesn't need means putting it on the internet. Exposure becomes an
explicit, reversible step:

```sh
serverctl create deployment sb-7f3a9c --no-route --port 8080 --size medium
serverctl set routes sb-7f3a9c --host sb-7f3a9c.sb.example.com   # expose
serverctl set routes sb-7f3a9c --none                            # withdraw again
```

Withdrawing a route does not disturb the VM — the deployment keeps running and
keeps its shell sessions; it simply stops matching requests.

A **static** (`proxy_pass`) deployment may not do this. The proxy is the only way
in, so an unrouted one would be unreachable by anything, and it is rejected at
registration. A sign-in gate on an unrouted deployment is rejected too: `auth`
runs on a proxied request, and there are none.

## Directory

`http://<admin-addr>/` (default `http://127.0.0.1:9090/`) is a landing page: one
card per routable URL, linking to the deployment that serves it. It is the
answer to "what is running on this box, and where do I click", which the
dashboard answers only incidentally.

```
edge-1                                      Dashboard  Metrics  Theme
4 URLs across 3 deployments. 1 with nothing healthy behind it.

┌────────────────────────────┐ ┌────────────────────────────┐
│ api                 static │ │ docs                  site │
│ http://api.example.com     │ │ http://docs.example.com    │
│ ● 2 of 2 upstreams up      │ │ ● serving /srv/docs        │
└────────────────────────────┘ └────────────────────────────┘
```

**Server-rendered**, unlike the dashboard: the page is complete in its first
response, works with JavaScript off, and holds no polling connection open. The
only script on it is the theme toggle. Reload to refresh.

A card per *URL* rather than per deployment, because a deployment with three
hostnames genuinely offers three places to go. Links point at the **data plane** —
scheme, port and `path_prefix` included — which the page cannot infer from its
own address, since it is served by the admin listener on a different port. Same
derivation as the dashboard's `urls`.

The dot reports whether following the link right now would reach anything:

| Dot | Means |
| --- | --- |
| green | healthy backends, or a site (files on this host, so nothing to be down) |
| amber | a VM is booting, **or** the deployment is scale-to-zero and idle — the first request starts it |
| red | routable with nothing healthy behind it |

Scale-to-zero idling is deliberately not red. It is the configured state, and
colouring it as a fault sends people debugging a system that is working.

A deployment whose routes use only `host_suffix` or `path_prefix` names no single
hostname to link to, so it gets no card — and is listed by id underneath, with
that reason. Omitting it silently would make "why isn't mine here?" unanswerable
from the page.

Gated exactly like the dashboard: open by default, behind
`APP_LB_DASHBOARD_PASSWORD` when one is set, since it lists every hostname this
app-lb routes. A deployment-scoped [app-token](#app-tokens) sees only its own.

## Dashboard

Open `http://<admin-addr>/dashboard` (default `http://127.0.0.1:9090/dashboard`)
for a live view of the fleet — or [`/`](#directory) for a static index of what is
running. The dashboard shows: host and per-VM CPU/memory, per-deployment pool
utilisation and per-VM load, request latency (distribution + p50/p90/p99 and a
client-derived requests/sec), cold-start times, and autoscaling activity. It is
a single self-contained page — no external assets, so it works over an SSH tunnel
to the admin port.

`GET /metrics` returns the same data as JSON (host usage, a global rollup, and a
per-deployment breakdown), suitable for scraping into your own tooling.

The per-deployment breakdown can be scoped, which matters once the fleet is
large — the unfiltered response carries a row per VM and a full set of histograms
per deployment, and the dashboard polls it every two seconds:

| Parameter | Effect |
| --- | --- |
| `deployment=<id>` | Just this one |
| `prefix=<s>` | Only ids starting with `s` |
| `summary=true` | Drop the per-VM rows; keep pool counts and metrics |
| `limit=`, `offset=` | Page |

`host`, `fleet` and `global` always describe the **whole** LB regardless of these
— a filter narrows the table, not the system. `matched` reports how many
deployments passed the filter before paging, so a client can page without
guessing. The dashboard's deployment table has a matching filter box and pager,
which appear only when there is more than one page.

The page updates in place rather than redrawing. Deployment cards are keyed by
id and reused across polls, and the distribution bars and utilization gauges are
mutated rather than rebuilt — which is what lets their CSS transitions animate at
all, since a freshly-created element has no previous width to animate from. It
also means a poll no longer destroys text selection, hover, or the VM table's
horizontal scroll position every two seconds. A card whose numbers did not change
does no DOM writes.

Each deployment carries `urls` — its `host` routes as links a browser can
follow, which the dashboard renders next to the deployment name. They are built
server-side because the dashboard is served by the *admin* listener and knows
nothing about the data plane: the scheme follows whether TLS is enabled, a
non-default port is included, and a `path_prefix` is appended (linking the bare
host would 404 against that same deployment). A deployment with no `host` route —
a sandbox, or a `host_suffix`/path-only route — has no `urls`, and the dashboard
shows no link rather than one that goes nowhere.

`tracked_deployments` is a self-check: it counts deployments holding their own
counters, and should track the number registered. A figure that climbs past it
means metrics for deregistered deployments are not being released. (Their totals
are not lost when they are — a deregistered deployment's counters fold into
`global`, so traffic that happened keeps counting.)

Every object type the [CLI](#cli) addresses has a section on the page, so the
dashboard is a complete view of what app-lb holds rather than a metrics screen:

| Section | Shows | Source |
| --- | --- | --- |
| Security | authentication abuse, attack signatures and traffic anomalies, newest first | `GET /security` |
| Deployments | pool gauges, per-VM load, **booting VMs with their age and daemon status** | `GET /metrics` |
| Certificates | issued hostnames, issuer, expiry, renewal state — plus routed hostnames that have *no* certificate yet | `GET /certs` |
| Secrets | ids, descriptions, key *names*, last update, whether the store is sealed | `GET /secrets` |
| Deploy jobs | recent builds and host updates, their result, and a live transcript | `GET /jobs` |

Two polling loops: the metrics view refreshes every 2s, and certificates, secrets,
jobs and security alerts — which change on human timescales — every 10s. The slow loop tightens to
2s while a job is running, since its record is the only progress there is.

The dashboard is also interactive:

- **Scale** (a form over min/max replicas, warm pool, and target concurrency →
  `PATCH .../scaling`) and **Edit** (a JSON editor over the full spec → `PUT`) on
  each deployment card, and **Drain**/**Kill** on each VM row (→
  `DELETE .../vms/:id`). A booting VM can be killed too.
- **Build** / **Update** on deployments that have a `build` or `update` block,
  which start the job and open its log.
- **New secret**, **Rotate** and **Delete** in the Secrets section. Values are
  write-only throughout: the API returns key names only, so a rotation sets new
  values rather than editing readable ones, and a delete that would break a
  deployment's build asks before forcing.
- **Log** on any job — the tail of its output, refreshing while it runs. This is
  the view for a build that is *hanging*: where it stopped is visible without
  waiting for it to fail.

While a form is open the cards stop re-rendering so an in-progress edit isn't
wiped, though the stat tiles keep updating live. These buttons call the admin CRUD
API — set `APP_LB_ADMIN_AUTH` (see below) to require the dashboard credentials for
them.

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

## Clients

| | |
|---|---|
| [`serverctl`](serverctl/README.md) | Rust — a client library *and* the kubectl-shaped CLI. `cargo install serverctl` for the CLI, `default-features = false` for the library. |
| [`serverctl` (npm)](sdk/typescript/README.md) | TypeScript — Node, Bun, Deno and browsers. |

Both speak the same wire contract, and both are checked against it: the fixtures
in `testdata/wire/` are written by app-lb's own response types, and each client
has a test asserting it understands every field in them. A field app-lb starts
sending fails a test in each client rather than going silently unread.

```rust
let lb = serverctl::Client::builder("127.0.0.1:9090").token(token).build()?;
let out = lb.exec("sb-7f3a9c", &ExecRequest::new("uname -a")).await?;
```

```ts
const lb = new Serverctl({ server: "127.0.0.1:9090", token });
const { stdout } = await lb.exec("sb-7f3a9c", "uname -a");
```

## App-tokens

Basic auth is one shared credential that cannot be scoped, cannot be revoked
without a restart, and cannot ride on a WebSocket upgrade from a browser. That is
fine for a person at a terminal and wrong for a program. An **app-token** is a
credential app-lb mints for itself: scoped, revocable, optionally expiring, and
accepted both by the admin API and by a deployment's own gate.

Tokens are how SDK clients authenticate. Basic auth stays, because you need some
credential to mint the first token.

```sh
curl -u admin:s3cret -XPOST localhost:9090/tokens -H 'content-type: application/json' \
  -d '{"name":"agent-runner","admin":"admin","deployments":["sb-7f3a9c"]}'
```

```json
{
  "id": "7f3a9c2b1e4d",
  "name": "agent-runner",
  "admin": "admin",
  "deployments": ["sb-7f3a9c"],
  "created_at": 1722400000,
  "token": "applb_7f3a9c2b1e4d_kJ8vQ2mN…"
}
```

**`token` is shown once.** Only `sha256(secret)` is stored, so there is no
endpoint that reads it back and no way to recover it — losing one means minting a
replacement and revoking the old one. Present it as a header:

```sh
curl -H "Authorization: Bearer applb_7f3a9c2b1e4d_kJ8vQ2mN…" localhost:9090/deployments/sb-7f3a9c
```

### Scope

Two axes, deliberately coarse.

| `admin` | What it reaches on the admin API |
|---|---|
| `none` | nothing — a credential for a deployment's gate and nothing else |
| `view` | `/metrics` and `/dashboard` |
| `admin` | everything, within `deployments` |

`deployments` is a list of ids, or `["*"]` for all of them. It governs both the
per-deployment admin routes (`exec`, `shell`, scale, evict, delete) and the
data-plane gate.

A token scoped to specific deployments is **refused the fleet-wide routes** —
creating deployments, listing them all, reading the secret store, and minting
tokens. That last one matters: minting is how you escalate, so a narrow token
cannot mint itself a wider one.

`/metrics` is the exception. Rather than refusing a scoped token it *narrows the
answer* — a token minted to drive one sandbox can watch that sandbox, and the
fleet rollup counts only what it can see.

Both scope fields default to nothing, so a forgotten field produces a token that
can do nothing rather than one that can do everything.

### Revoking and expiring

```sh
curl -u admin:s3cret localhost:9090/tokens                      # list; never shows a secret
curl -u admin:s3cret -XDELETE localhost:9090/tokens/7f3a9c2b1e4d
curl -u admin:s3cret -XPATCH localhost:9090/tokens/7f3a9c2b1e4d \
  -H 'content-type: application/json' -d '{"deployments":["sb-other"]}'
```

Revocation takes effect on the next request — verification is a lookup in the
store, not a signature check, which is exactly why tokens are opaque and stored
rather than self-describing and signed. `PATCH` re-scopes a token *without*
changing its secret, so narrowing a credential does not require redistributing
it. Pass `expires_in_secs` at mint for one that retires itself.

`last_used_at` is written when the store is next persisted for another reason
rather than on every request — a token on a hot path would otherwise turn each
call into a file write. So it lags, and a busy token can still read as unused.

Tokens live in `app-lb-tokens.json` (`APP_LB_TOKENS_PATH`), mode `0600`. Only
hashes are in it, so unlike the secret store there is nothing to encrypt.

### Gating a deployment with app-tokens

`auth.provider` takes one provider or several, and any one of them admits a
request — they are alternatives, not requirements:

```json
{
  "auth": {
    "provider": ["google", "app-token"],
    "client_id": "…apps.googleusercontent.com",
    "client_secret": { "secret": "google-oauth" },
    "allowed_domains": ["example.com"]
  }
}
```

That is the common shape for a sandbox hosting a UI: a person signs in with
Google, the agent driving it presents `Authorization: Bearer applb_…`, and
neither has to know the other exists. A token gets in when its `deployments`
scope names the deployment.

For a deployment only programs reach, `"provider": "app-token"` needs no OAuth
configuration at all — no `client_id`, no allow-list, since neither describes a
credential issued to a process. A browser landing on such a deployment gets a
`401` naming what would work rather than a redirect into a sign-in flow that does
not exist. Writing `client_id` on a gate that does not list `google` is a
validation error rather than a field quietly ignored.

A token admitted at the data plane forwards **no** `x-auth-request-*` identity
headers: a token is not a person, and putting a name upstream that belongs to
nobody would be worse than sending none.

### Tokens in a URL

The shell WebSocket — and only the shell WebSocket — also accepts
`?app_token=…`. This exists for exactly one reason: a browser's `WebSocket`
constructor cannot set headers, so a query parameter is the only credential it
can carry. That is what makes a browser terminal possible at all.

Every other route can send a header, so every other route refuses a token in the
query string. A credential in a URL lands in access logs, proxy logs and browser
history, so mint short-lived tokens for this:

```sh
curl -u admin:s3cret -XPOST localhost:9090/tokens -H 'content-type: application/json' \
  -d '{"name":"terminal","admin":"admin","deployments":["sb-7f3a9c"],"expires_in_secs":120}'
```

### Putting the dashboard behind Google

Tempting, and it works for the *page* — but only if you also say which paths the
machines may take, because [a gate admits browsers and nothing
else](#limits).

The recipe is a static deployment fronting the admin listener
([`examples/app-lb-admin.json`](examples/app-lb-admin.json)) carrying an `auth`
block. Measured against exactly that, through the gated hostname:

```
                                          browser      fetch()/curl/serverctl
GET /dashboard   (Accept: text/html)       302 → Google
GET /metrics     (accept: application/json)                          401
GET /deployments (no Accept)                                         401
```

So a gate alone gives you a dashboard that loads, signs you in, and then shows
nothing — its `/metrics` poll is refused — while `serverctl` is locked out
entirely, since it cannot complete an OAuth flow and the `login_url` in the 401
body means nothing to it.

Both are fixed the same way: let the machine paths past the gate, and put a
credential the machine *can* present behind them.

```jsonc
// on the deployment fronting the admin listener
"public_paths": ["/healthz", "/metrics", "/deployments", "/secrets", "/jobs", "/certs"]
```

```sh
# and on app-lb itself, so those paths are not simply open
APP_LB_DASHBOARD_PASSWORD=s3cret APP_LB_ADMIN_AUTH=1
```

`/` and `/dashboard` stay behind Google; everything a machine calls passes the
gate and meets Basic auth instead. **Set `APP_LB_ADMIN_AUTH=1` first and restart,
then add `public_paths`** — the other order leaves a window where a full CRUD
API, `/secrets` included, is on the internet with nothing in front of it.

If you would rather not make that trade, don't: leave the hostname
browser-only and reach the API over an SSH tunnel
(`ssh -L 9090:127.0.0.1:9090 host`), which is what
[`serverctl`](serverctl/README.md#connecting) expects.

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

### TLS

The proxy serves plaintext HTTP on `proxy_addr` by default. The HTTPS listener
binds `APP_LB_PROXY_TLS_ADDR` (default `0.0.0.0:6189`) *in addition to* it —
both run at once — and turns on when either ACME is enabled or a static
`APP_LB_TLS_CERT`/`APP_LB_TLS_KEY` pair is set. Setting only one of cert/key is a
hard startup error rather than a silent plaintext fallback. Upstreams stay
plaintext regardless — the guest IP is on a host-local tap network — so this is
TLS *termination* at the edge.

Certificates are chosen **per handshake from the client's SNI**, not fixed at
startup: the acceptor holds no certificate of its own and asks the cert store for
one on every connection. That is what lets a certificate issued seconds ago serve
without a restart. It also means the TLS stack is openssl rather than rustls —
pingora only supports handshake callbacks under openssl/boringssl — so the build
needs `libssl-dev` and links openssl alongside the rustls that heyo-sdk pulls in
through reqwest.

```sh
APP_LB_TLS_CERT=cert.pem APP_LB_TLS_KEY=key.pem ./target/release/app-lb
curl -k https://localhost:6189/ -H 'Host: demo.local'
```

#### Automatic certificates (Let's Encrypt)

Set `APP_LB_ACME_EMAIL` and app-lb obtains a certificate for every deployment
hostname itself, renewing 30 days before expiry:

```sh
APP_LB_PROXY_ADDR=0.0.0.0:80 \
APP_LB_PROXY_TLS_ADDR=0.0.0.0:443 \
APP_LB_ACME_EMAIL=ops@example.com \
./target/release/app-lb
```

Register a deployment with an exact `host` route pointing at a real DNS name and
the certificate arrives within seconds — no restart, no reload:

```sh
curl -XPOST localhost:9090/deployments -H 'content-type: application/json' \
  -d '{"id":"demo","routes":[{"host":"demo.example.com"}],"upstreams":["127.0.0.1:8080"]}'
curl -s localhost:9090/certs | jq   # hostname, expiry, issuer
```

Certificates and the ACME account key are cached under `APP_LB_ACME_DIR` and
reloaded on boot, so a restart involves no CA traffic at all.

Four things to know before enabling it:

- **Port 80 is required.** Let's Encrypt fetches HTTP-01 challenges on port 80
  and nowhere else, so `APP_LB_PROXY_ADDR` must be `0.0.0.0:80` (app-lb answers
  `/.well-known/acme-challenge/` itself, ahead of routing). app-lb logs a warning
  at startup if ACME is on and the proxy is bound elsewhere. Binding 80 and 443
  as the non-root `app-lb` user needs the bind capability:
  `setcap 'cap_net_bind_service=+ep' /usr/local/bin/app-lb`.
- **Exact `host` routes are covered per host; subtrees need a wildcard.** A
  `host_suffix` rule matches a whole subtree, and a certificate for a subtree is
  a wildcard — see [Wildcard certificates](#wildcard-certificates-for-a-fleet)
  below. A suffix no configured wildcard covers is served the static fallback
  certificate, and the gap is logged once per suffix.
- **Test against staging first.** Set
  `APP_LB_ACME_DIRECTORY=https://acme-staging-v02.api.letsencrypt.org/directory`.
  Production rate limits are per-account per-week; a misconfigured hostname in a
  retry loop can lock out issuance for every other hostname for hours. app-lb
  backs off exponentially per host (1 min doubling to 6 h) and **persists that
  backoff across restarts**, so a supervisor restart loop can't reset it and
  hammer the CA — but staging is still the right place to find out DNS is wrong.

  Switching between staging and production needs no manual cleanup: app-lb
  records which directory the cached state belongs to and discards the account
  and every certificate when it changes, logging why. That matters because
  neither would otherwise correct itself — a saved account reconnects to the
  directory baked into *its own* credentials regardless of this variable, and a
  staging certificate stays valid for months so it never comes up for renewal.
  The result would be untrusted certificates served indefinitely with nothing in
  the log. (State written by a version predating this check is left alone, since
  an upgrade isn't a change; the first run after upgrading records the current
  directory and acts on changes from then on.)
- **`APP_LB_TLS_CERT` becomes the fallback.** It is served for any SNI without an
  issued certificate of its own — a `host_suffix` deployment, or a hostname whose
  first issuance hasn't finished. With no fallback configured, such a handshake
  fails cleanly rather than presenting a certificate for the wrong name.

The `APP_LB_ACME_DIR` holds private keys and the account key; it should be mode
`0700` and owned by the user app-lb runs as. app-lb writes the files it creates
`0600`.

#### Wildcard certificates for a fleet

Per-host issuance stops working at scale, and not gradually: **Let's Encrypt
allows 50 new certificates per registered domain per week.** A fleet of agent
sandboxes on `<id>.sb.example.com` exhausts that on the first afternoon and then
locks out every other hostname on the account for the rest of the week.

One wildcard covers all of them:

```sh
APP_LB_ACME_EMAIL=ops@example.com \
APP_LB_ACME_WILDCARD=sb.example.com \
APP_LB_ROUTE53_ZONE_ID=Z0123456789ABCDEFGHIJ \
./target/release/app-lb
```

That issues **one** certificate covering `sb.example.com` and
`*.sb.example.com`, and it is renewed like any other. Exposing a sandbox is then
free of certificate work entirely — add the route and it is already covered:

```sh
serverctl set routes sb-7f3a9c --host sb-7f3a9c.sb.example.com
```

Pair it with a single wildcard `A` record (`*.sb.example.com → <lb-ip>`) and
exposing a sandbox needs **no DNS write either** — no per-sandbox Route 53 call,
which also keeps you clear of that zone's RRset limit and the 5-requests-per-
second change quota.

What to know:

- **Hosts under a configured wildcard are skipped by the per-host issuer.** That
  suppression is the point; without it the fleet queues an order per sandbox.
- **A wildcard covers exactly one label.** `*.sb.example.com` serves
  `a.sb.example.com` but not `a.b.sb.example.com`. Keep sandbox hostnames to a
  single label under the domain — a deeper one falls through to per-host
  issuance, which works but does not scale.
- **DNS-01 needs the `aws` CLI on the app-lb host** (`APP_LB_AWS_BIN`), the same
  way image builds need `heyvm` and pulls need `art`. It brings its own
  credentials — profile, instance role, `AWS_*` — which is the part worth not
  reimplementing. The IAM policy needs `route53:ChangeResourceRecordSets` and
  `route53:TestDNSAnswer` on the zone.
- **app-lb waits for the record to be answerable before telling the CA to look.**
  Route 53 reporting `INSYNC` is not the same as its nameservers answering, and
  the CA gets one attempt: a failed validation backs off *every* hostname on the
  account for hours. app-lb polls `test-dns-answer` — which asks the zone's own
  nameservers, exactly who the CA will ask — for up to 3 minutes, and refuses to
  proceed rather than spend an attempt on a record that is not there yet.
- **Serving is driven by the certificate, not the configuration.** A subdomain
  with no certificate of its own is served its parent's *only if that
  certificate actually carries a `*.` SAN*. A plain certificate for a parent
  domain is never served for a subdomain, which would be a name mismatch at the
  client.
- **Both names come from one order.** `sb.example.com` and `*.sb.example.com`
  are authorized at the same record, `_acme-challenge.sb.example.com`, so both
  digests are published as two values of one TXT RRset. Publishing them
  separately would have the second overwrite the first.

This HTTPS listener terminates TLS for **proxied deployment traffic** only. The
admin API and dashboard bind a *separate* plaintext listener (`APP_LB_ADMIN_ADDR`)
with no TLS of its own — to serve the dashboard over HTTPS at a DNS name, either
terminate TLS in a reverse proxy (nginx / Caddy) in front of `127.0.0.1:9090`, or
front it through this same proxy with a static/`proxy_pass` deployment: see
[`examples/app-lb-admin.json`](examples/app-lb-admin.json) and
[`examples/README.md`](examples/README.md). To bind the HTTPS listener on `443`
under the non-root `app-lb` user, grant the bind capability:
`setcap 'cap_net_bind_service=+ep' /usr/local/bin/app-lb`.

With ACME enabled that example gets simpler: give the admin deployment an exact
`host` route and its certificate is issued automatically like any other.

### Scaling

Desired replicas is `ceil(demand / target_concurrency) + warm_pool`, clamped to
`[min_replicas, max_replicas]`, where demand counts in-flight requests *plus* requests
waiting on a cold start.

Scale-to-zero applies only when both `min_replicas` and `warm_pool` are 0. A request arriving
at an empty pool is held (up to `cold_start_timeout_secs`) while a VM boots, rather than
failing — in practice a Firecracker VM is serving in ~1–2s. Scaling down marks a VM draining
so it finishes in-flight work, then retires it once idle or at `drain_timeout_secs`.

#### `idle_action` — destroy or retain

What "retire" means is a per-deployment choice, because a *sandbox* is not a
replica. Retiring one of four interchangeable web VMs should reclaim everything
it held. Retiring the single VM that is somebody's working directory should not.

| | `destroy` (default) | `retain` |
| --- | --- | --- |
| The VM is | killed | stopped |
| Sandbox record | gone | kept |
| `/workspace` data disk | **gone** | **kept** |
| Rootfs writes | gone | gone |
| Memory | gone | gone |
| Next scale-up | boots a fresh VM | resumes this one |

**Read the rootfs row again.** `retain` does not preserve the root filesystem,
and cannot: mvm-ctrl gives each VM a private rootfs copy under `/tmp` and
**recopies it from the base image on every cold boot**. The only thing that
survives a stop is the persistent data disk at `~/.heyo/run/<id>/data.ext4`,
attached as `/dev/vdb` and mounted at `/workspace` — and that disk only exists
if `vm.disk_size_gb` is set.

So a `retain` deployment is only meaningfully stateful when both hold:

```jsonc
"vm":      { "disk_size_gb": 20 },      // there is a /workspace to keep
"scaling": { "idle_action": "retain" }  // and it isn't thrown away when idle
```

with the sandbox's real state living under `/workspace`. Setting `retain` without
a data disk is allowed — it still saves a boot — but app-lb logs a warning at
registration saying it keeps nothing, because that combination is almost always
a mistake rather than a choice.

**Suspended VMs are tracked by app-lb, because nothing else can.** mvm-ctrl drops
a stopped sandbox from `GET /sandboxes`, so it is invisible to the fleet list the
autoscaler reconciles against, and its TTL does not run while it is stopped. Its
id is therefore written into the deployment's state file the moment it is
stopped. Deregistering a deployment destroys its suspended VMs, and a slow sweep
(every 5 minutes) destroys any stopped VM of ours that no deployment claims —
the residue of a crash between stopping a VM and recording that we did.

A scale-up prefers resuming a suspended VM over creating one, and not only to
save time: creating a fresh VM while one sits stopped would strand that VM's
`/workspace` and hand the caller an empty sandbox in its place.

#### When a boot never finishes

A VM is only added to the pool once the daemon reports it `Running`, it has a
`guest_ip`, *and* it answers its health check. The failure mode worth knowing about
is the third one: a VM whose guest boots but whose *server* doesn't — a wrong
`start_command`, an env var pointing at a directory that isn't there, a binary that
exits — looks perfectly healthy to the daemon and fails the probe forever.

Two things bound and expose that:

- **Progress logging.** Every still-booting VM is logged with its sandbox id, its
  age, the daemon's status, and what it is waiting on — on first sighting, on every
  status change, and every 30s thereafter. The `waiting_on` field is the diagnosis:
  *"the daemon has not reported it Running yet"* means wait, while *"the guest is up
  but has not answered GET /healthz"* means go and look at the guest. Once a boot
  outlasts `cold_start_timeout_secs` — the point at which it has already cost a
  request a 503 — the line becomes a `WARN`.
- **`boot_timeout_secs`** (default `300`). Past this the autoscaler logs an `ERROR`,
  kills the VM and lets the next tick create a replacement, so a deployment retries
  visibly instead of sitting at zero replicas forever. It is deliberately much
  larger than `cold_start_timeout_secs`: a request gives up long before the VM does,
  because a boot that overran one caller's patience may still be the boot that
  serves the next one. Set it to `0` to wait indefinitely.

The count of abandoned boots is in `/metrics`
(`autoscale.boot_timeouts`) and on the dashboard's cold-start card, and booting VMs
appear as rows in each deployment's VM table with their age and status — so a stall
is visible without reading logs at all.

`ttl_seconds` is a backstop: VMs expire on their own if app-lb dies without reaping them. It
is renewed while app-lb is alive, and VMs from a previous run are re-adopted on startup
(matched by their `applb-<deployment>-<nonce>` name). VMs app-lb did not create are never
touched.

Booting a VM takes long enough that an admin request can delete or rebuild the deployment
while a create is still in flight. The autoscaler therefore re-checks, after every create and
promotion, that the deployment it is working on is still the registry's — and kills any VM the
replacement did not inherit, rather than leaving it running until its TTL. A pool-preserving
edit (one that doesn't change the `vm` block) carries its VMs over and keeps them.

## Fleets of sandboxes

app-lb's usual shape is a few dozen deployments, each a pool of interchangeable
VMs behind a hostname. A fleet of **agent sandboxes** inverts nearly all of that:
thousands of deployments, one VM each, mostly unrouted, stateful, and created and
destroyed all day. The pieces that make it work are documented in their own
sections; this is the shape they add up to, and why each is there.

One deployment per sandbox — see [`examples/sandbox.json`](examples/sandbox.json):

```jsonc
{
  "id": "sb-7f3a9c",
  "routes": [],                                  // no ingress; exec/shell only
  "vm": { "driver": "firecracker", "port": 8080,
          "disk_size_gb": 20,                    // /workspace, the part that survives
          "working_directory": "/workspace" },
  "scaling": { "min_replicas": 0, "max_replicas": 1, "target_concurrency": 1,
               "scale_to_zero_after_secs": 900,
               "idle_action": "retain" }         // stop when idle, don't destroy
}
```

```sh
serverctl create deployment sb-7f3a9c --no-route --port 8080 --disk-gb 20 \
  --idle-action retain --max 1 --target-concurrency 1
serverctl exec sb-7f3a9c -- ls /workspace
serverctl shell sb-7f3a9c

serverctl set routes sb-7f3a9c --host sb-7f3a9c.sb.example.com   # expose it
serverctl set routes sb-7f3a9c --none                             # take it back
```

| Need | What does it | Why not the obvious thing |
| --- | --- | --- |
| No public URL | [`"routes": []`](#deployments-with-no-routes) | A sandbox that serves nobody should not be on a hostname |
| Get inside it | [`exec` / `shell`](#running-things-inside-a-vm) | Proxied, so the gate applies and a sleeping sandbox wakes |
| Survive going idle | [`idle_action: retain`](#idle_action--destroy-or-retain) | Default `destroy` kills the disk, which *is* the sandbox |
| Keep state | `disk_size_gb` + `/workspace` | The rootfs is recopied from the image every boot |
| Expose one, sometimes | `set routes --host …` | Reversible, and disturbs neither the VM nor open shells |
| Certificates for all of them | [one wildcard](#wildcard-certificates-for-a-fleet) | Per-host issuance hits Let's Encrypt's 50/domain/week cap |
| DNS for all of them | one wildcard `A` record | Zero Route 53 calls per expose; no RRset or rate-limit ceiling |

### What scaling to thousands actually required

Four things in app-lb were fine for dozens and quadratic, unbounded, or simply
wrong for thousands. They are fixed, and worth knowing about because each has an
observable consequence:

- **State is one file per deployment**
  ([`app-lb-state.d/`](#where-state-lives)). Registering the thousandth sandbox
  used to rewrite the other 999 with it.
- **Routing is indexed by exact host**, not scanned, and the index is updated
  **incrementally**. Thousands of hostnames no longer cost a linear scan per
  request, and registering one deployment rebuilds only the buckets its own
  routes belong to instead of the whole index — which is what stopped the
  marginal cost of a create tracking the size of the fleet.
- **The reconcile loop runs concurrently** and skips deployments at rest without
  touching the daemon, so one slow VM cannot stall the fleet's tick.
- **`/metrics` is scopeable** (`?prefix=`, `?limit=`, `?summary=`) and the
  dashboard pages, because the unfiltered response is megabytes at this size.
  Per-deployment counters are released when a deployment is deregistered, so the
  metrics map no longer grows by one entry per sandbox ever created —
  `tracked_deployments` in the response is the check on that.

Measured on a release build, registering deployments through the admin API:

| | 100 registered | 2000 | 5000 |
| --- | --- | --- | --- |
| Marginal cost of one create | 0.25 ms | 0.30 ms | 0.56 ms |
| Register that many from empty | — | 0.6 s | 2.0 s |
| Route lookup (proxy) | — | 0.3 ms | 0.3 ms |
| Dashboard poll (`?limit=50`) | — | 1.7 ms / 68 KB | — |
| `/metrics` unfiltered, for contrast | — | 14 ms / 2.6 MB | — |

A write is still O(n) in the *shallowest* sense — copy-on-write means the
deployment map and the route index each get a fresh hash table per write, so a
few tens of nanoseconds per existing deployment. What it is no longer doing is
deep-copying every rule in the fleet and re-sorting the whole index, which is
what made a create storm genuinely quadratic. Readers stay lock-free, which is
the constraint that rules out mutating either structure in place.

### One host, and what that bounds

A single app-lb is a single heyvm host: `guest_ip` is only populated for a local
daemon, so every VM it routes to is on this machine's tap network. Thousands of
*deployments* is a registry question and works; thousands of *simultaneously
running* VMs is a host-RAM question and does not. `idle_action: retain` with
`scale_to_zero_after_secs` is what reconciles the two — a fleet of thousands with
tens awake, each waking in a resume rather than a cold boot.

## Shipping logs to app-obs

[app-obs](../app-obs) polls `/metrics` for numbers, but the *logs* it stores have
to be pushed to it. Set `APP_LB_OBS_URL` and app-lb pushes three streams:

```sh
APP_LB_OBS_URL=127.0.0.1:9500 \
APP_LB_OBS_TOKEN="$(cat /etc/app-obs/ingest-token)" \
app-lb
```

A bare `host:port` is fine — the scheme defaults to `http`, which is what
app-obs's ingest speaks. `0.0.0.0` is accepted too and read as this host, since
that is app-obs's *bind* address rather than anywhere you can send to; the
resolved endpoint is in the startup line, so check it there. A URL that cannot be
salvaged (`ftp://`, no host) logs an error at startup and leaves shipping off —
it never stops app-lb from serving, because a typo in the observability
configuration must not be able to take the data plane down.

- **An access log** — one record per request, attributed to the deployment that
  served it. For a static (`proxy_pass`) deployment this is the only log it will
  ever have, because there is no guest to run a shipper inside; for a VM
  deployment it is the only account of what the *proxy* saw, as opposed to what
  the application chose to write about itself.
- **app-lb's own events** at INFO and above — scaling decisions, boots that are
  taking too long, upstreams going unhealthy, ACME issuance, job outcomes. An event
  that names a deployment lands in that deployment's log, so a scale-up appears
  next to the traffic that caused it; everything else lands under
  `APP_LB_OBS_DEPLOYMENT` (`_lb` by default).
- **Deploy-job output** — every line an image build or host update writes,
  attributed to the deployment being deployed and tagged `source=job`. The job
  record served by `GET /jobs/:id` holds only a bounded tail, in memory, until the
  process restarts; this is the copy that survives, and the one to read when a
  build *hangs* rather than fails.

- **Security alerts** — one record per finding from
  [security monitoring](#security-monitoring), tagged `source=security`, with the
  triggering event's ECS fields flattened alongside. This is the durable copy;
  app-lb's own `GET /security` ring is bounded and does not survive a restart.

Records carry a `source` — `access`, `app-lb`, `job` or `security` — so the four
are separable inside one deployment's log.

### Naming the LB itself

app-lb's own records need a deployment id like everything else app-obs stores, and
by default it is the reserved `_lb`. Override it with `APP_LB_OBS_DEPLOYMENT` when
more than one app-lb ships to the same collector: otherwise both hosts' events
interleave under one name, and "which LB logged this?" is only answerable from the
batch-level `host` field.

```sh
APP_LB_OBS_DEPLOYMENT=lb-us2 app-lb
```

It applies to both streams that can lack a deployment of their own — app-lb's
events *and* the access-log records for requests that matched no route — so the
two never diverge. The value becomes a directory name in app-obs, so it is
validated at startup against the same rule app-obs uses (up to 128 characters of
`[A-Za-z0-9._-]`); a value app-obs would reject is refused here instead, because
app-obs answers a bad id by rejecting the whole *batch* it arrived in, taking
every other deployment's records with it. The resolved id is in the startup line:

```
INFO shipping logs to app-obs endpoint=… lb_deployment=lb-us2
```

The token must match app-obs's `APP_OBS_INGEST_TOKEN`. app-obs leaves ingest open
when *it* has none, so the failure worth anticipating is the other direction: an
app-lb with no token against a collector that wants one loses every record to a
401, which shows up as `failed` below rather than as anything app-obs can report.

### What a request record carries

```
GET /things 200 1.4ms
{"method":"GET","path":"/things","status":200,"duration_ms":1.416,
 "bytes":254,"host":"demo.local","client":"10.1.2.3"}
```

`backend` is the sandbox id for a managed VM and the `host:port` for a static
upstream — the same identity the `x-vm-id` response header carries. `status` is
`null` and the level is `error` when no response was written at all: every
upstream failed, or the cold start timed out. A failed request also carries
`error`.

**The query string is never logged**, only the path. A sign-in callback carries
the OAuth `code` there, and a shared log store is the last place a credential
should come to rest. The signed-in user's email is left out for the same reason —
a gated deployment receives the identity headers and can log what it needs of
them itself.

Requests that matched **no** deployment ship too, under `APP_LB_OBS_DEPLOYMENT`
with `"unrouted": true`. A wall of 404s for a hostname somebody expected to work is
invisible in a per-deployment view by construction, and it is one of the more
common things to have to diagnose. The cost is that internet background noise —
scanners probing `/wp-login.php` — accumulates there against app-obs's retention;
`APP_LB_OBS_ACCESS_LOG=0` turns the access log off entirely if that trade isn't
worth it.

### It is not a dependency of the data plane

Recording is a `try_send` into a bounded queue and nothing else: no lock, no
await, no I/O on the request path. Everything past that point is one background
task, and every way it can fail resolves to *losing telemetry*, never to holding
up a request:

- A **full queue drops** and counts what it dropped. A collector that has fallen
  behind must not turn into latency in somebody's application.
- A **failed POST discards its batch** instead of retrying. A retry queue is a
  memory leak with extra steps, and the records worth having are the ones still
  arriving.
- **app-obs being down is invisible to traffic.** It is logged once when shipping
  starts failing and once when it recovers — not once per batch — and the running
  count sits in `GET /metrics`:

```json
"obs": {"queued": 41201, "dropped": 0, "shipped": 41180, "failed": 21, "healthy": true}
```

`dropped` is the figure to watch, because it is the only trace those records
leave anywhere: raise `APP_LB_OBS_QUEUE_CAPACITY` if it climbs. Asking app-obs
instead cannot answer the question — nothing there can tell a quiet deployment
from a full queue.

### What is deliberately not shipped

**Only app-lb's own events.** pingora's, reqwest's and hyper's stay in stdout:
reqwest and hyper log *inside* the POST that ships the batch, so forwarding them
would be a feedback loop, and pingora's per-connection lines are free text with no
deployment to attribute them to. The supervisord log stays the complete record;
app-obs gets the part worth querying.

**DEBUG and below**, even though the default filter (`info,app_lb=debug`) emits
it — DEBUG is where the per-request routing chatter lives, which is a worse copy
of the access log. `RUST_LOG` still bounds what ships, since it decides what
app-lb logs at all.

## Security monitoring

app-lb sits at the edge of every deployment it fronts, so it already sees the
one stream a SIEM wants most: every request, with its source address, host,
path, status and latency. What it had no notion of was *malice*. Three things
follow, and this is what fixes them:

- **Authentication failure was invisible.** The admin gate answered a bad
  credential with a 401 and logged nothing, so a password spray against
  `/dashboard` left no trace anywhere.
- **Scanner traffic was noticed and thrown away.** `/wp-login.php` probes
  accumulate in the unrouted access log; nothing read them.
- **Anomalies were only visible to somebody watching** the right dashboard card
  at the right moment.

It is **on by default** — unlike log shipping, it needs no external service to
be useful, and a security feature you have to remember to enable protects
nobody. `APP_LB_SIEM=0` turns it off.

Events are normalized through [`u-siem`](https://crates.io/crates/u-siem) into
ECS field names (`source.ip`, `url.path`, `http.response.status_code`), so an
alert stored in app-obs is queryable next to anything else ECS-shaped. Only the
crate's event model is used; the detection is app-lb's own, because `SiemRule`
matches one log at a time and two of the three rule families below need a
window.

### What it watches

**Authentication abuse.** Rejected credentials from the admin gate, the
data-plane app-token gate and the Google sign-in flow.

| Rule | Fires when | Severity |
| --- | --- | --- |
| `auth.brute-force` | `APP_LB_SIEM_AUTH_THRESHOLD` failures from one source in a window | high |
| `auth.spray` | the same, spread across four or more deployments | high |
| `auth.enumeration` | the same, against four or more distinct sign-in identities | high |
| `auth.scope-denied` | repeated 403s — a *valid* credential reaching past its scope | medium |
| `auth.signin-state` | a sign-in state/nonce mismatch; CSRF-shaped, so unwindowed | medium |

**Attack signatures**, matched in one pass over the request path and — see
below — the query string.

| Rule | Catches | Severity |
| --- | --- | --- |
| `web.rce` | `${jndi:`, `/bin/sh`, shell chaining | critical |
| `web.traversal` | `../`, `%2e%2e`, encoded variants | high |
| `web.sqli` | `union select`, `' or 1=1`, `sleep(` | high |
| `web.xss` | `<script`, `javascript:`, `onerror=` | medium |
| `web.secret-probe` | `/.env`, `/.git/`, `/wp-login.php`, `/phpmyadmin` | medium |

**Traffic anomalies**, per source address.

| Rule | Fires when | Severity |
| --- | --- | --- |
| `traffic.scanner` | `APP_LB_SIEM_SCAN_THRESHOLD` 4xx responses *across many distinct paths* | medium |
| `traffic.rate-spike` | `APP_LB_SIEM_RATE_THRESHOLD` requests in a window | medium |

The distinctness requirement on `traffic.scanner` is what separates enumeration
from one broken client retrying a single dead URL.

### Reading it

The dashboard grows a **Security** card above the deployment table, and an
**Alerts** tile beside the fleet totals so a critical finding is visible without
scrolling. `GET /security` is the same data as JSON, behind the same gate as
`/metrics` — it names attacker addresses and the exact probes that reached the
fleet, so it is never open when `/metrics` is not.

```sh
curl -su admin:s3cret localhost:9090/security
curl -su admin:s3cret 'localhost:9090/security?severity=high&limit=20'
```

```json
{"enabled": true, "window_secs": 60,
 "alerts": [{"id": 41, "rule": "traffic.scanner", "severity": "medium",
             "title": "203.0.113.9 is probing for unserved paths",
             "client": "203.0.113.9", "deployment": "demo", "count": 214,
             "ecs": {"source.ip": "203.0.113.9", "url.path": "/wp-login.php",
                     "http.response.status_code": 404}}],
 "totals": {"medium": 11, "high": 3, "critical": 0},
 "stats": {"observed": 918273, "dropped": 0, "raised": 14,
           "suppressed": 4118, "tracked_clients": 118}}
```

Filter with `?severity=`, `?rule=`, `?deployment=` and `?limit=`. A
deployment-scoped [app-token](#app-tokens) gets its own deployments' alerts and
never the ones attributed to the LB itself — the set of addresses attacking the
control plane is fleet information.

**`count` is the occurrence tally, not an alert count.** Repeats with the same
rule, source and deployment fold into the open alert for
`APP_LB_SIEM_SUPPRESS_SECS`, so a scanner making ten thousand requests produces
*one* alert whose count climbs. Without that the ring would hold nothing but
that scanner, and app-obs would store ten thousand records saying the same
thing.

Every new alert also ships to app-obs under `source=security` when
`APP_LB_OBS_URL` is set, with the ECS fields flattened alongside — so the
in-memory ring is the live view and app-obs is the durable one. An alert about
the LB itself lands under `APP_LB_OBS_DEPLOYMENT`, like every other record that
names no deployment.

### The query string

app-lb otherwise **never logs the query string**, because a sign-in callback
carries an OAuth `code` there. Most SQL injection and XSS payloads, though, live
in a parameter *value* — so a path-only detector is blind to the attacks it most
needs to see.

The resolution is to **scan it and never store it**. The raw query reaches the
matcher as its own argument, is read, and is dropped with the observation; it is
not a field on the record type the access log is built from, so no code path
exists that could carry it to app-obs. On a match the alert records the
parameter *name* and nothing else:

```
web.sqli attempt in query parameter "id" from 203.0.113.9
```

The honest cost: the raw query now sits in a bounded in-process queue for a few
milliseconds, which it did not before. It never crosses a process boundary and
never reaches disk. `APP_LB_SIEM_SCAN_QUERY=0` removes even that, at the price
of most of the signature coverage.

### It is not a dependency of the data plane

The same contract as [log shipping](#it-is-not-a-dependency-of-the-data-plane),
enforced the same way: recording an observation is a `try_send` into a bounded
queue and nothing else — no lock, no await, no I/O, and no normalization on the
request path. Analysis happens on one background task, and every way it can fail
resolves to losing findings rather than to holding up a request.

Which makes the failure mode worth naming, because **a SIEM that has stopped
looking is indistinguishable from a quiet network**. Two counters say so out
loud, on `/metrics`, on `/security` and on the dashboard card:

- `dropped` — observations refused because the queue was full. Non-zero means
  detection is *sampling*; raise `APP_LB_SIEM_QUEUE_CAPACITY`.
- `clients_at_capacity` — the per-source table is full, so new addresses are not
  being tracked. Raise `APP_LB_SIEM_MAX_CLIENTS`.

That table is capped because an unbounded one would let an attacker with a
botnet choose how much memory app-lb allocates. At the cap it sweeps whatever
has aged out and then **drops the new observation rather than evicting a live
entry** — evicting the least-recently-seen is exactly what an attacker
engineers, by flooding fresh addresses until the entry counting their real
activity is the one that goes.

### What it deliberately does not do

**No `X-Forwarded-For` trust.** Detectors key on the socket peer only. Keying on
a client-supplied header would be an alert-spoofing and alert-flooding primitive
on an unauthenticated path. The consequence is that behind another proxy every
request appears to come from that proxy, and the per-source rules are only as
useful as that is.

**No credentials, ever.** An auth alert records the *scheme* (`basic`, `bearer`,
`none`) and never the password, the token, or a prefix of either — not even the
Basic username, since decoding it would mean parsing unauthenticated attacker
input on the failure path for no detection gain.

**No configuration auditing.** Who changed which deployment is a different
question with a different answer; the [deploy-job records](#shipping-logs-to-app-obs)
and app-lb's own events already cover it.

**No persistence.** The ring and the windows are in memory, so a restart resets
both. That is the right trade for a load balancer, and the reason
`APP_LB_SIEM_SHIP` defaults on: app-obs holds the copy that outlives the process.

**No IPv6 escape by rotation.** Sources are keyed by /32 for IPv4 and by **/64**
for IPv6, because a /64 is a standard end-site allocation and an attacker
rotating within one would otherwise reset every counter for free.

One caveat about addresses: the admin listener defaults to `127.0.0.1`, so
control-plane alerts usually name a loopback address and it is the *count* that
carries the information. The address only means something when that port is
exposed directly.

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
