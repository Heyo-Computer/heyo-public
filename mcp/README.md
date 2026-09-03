# heyo-mcp

An MCP server over heyo: **heyo cloud** for sandboxes — boot a microVM, run a
command in it, get files in and out — and the three services that answer
operational questions about a fleet, [app-lb](../app-lb) (deployments and VM
pools), [app-obs](../app-obs) (logs and metrics) and [ci](../ci) (builds).

The sandbox half is the API an agent runs work on. The operational half exists
because its questions span three services: "why is nothing running" is app-lb's
topology *and* app-obs's logs *and* ci's queue, and a tool per endpoint leaves
that join to be redone by hand every time. The diagnostic tools below do the
join and carry what it cost to learn which endpoint answers what.

## Two API keys and nothing else

```bash
HEYO_API_KEY=heyo_api_…      # heyo cloud: sandboxes
APPLB_TOKEN=heyo_api_…       # the managed app-lb, through cloud's namespace door
```

That is a complete configuration. Cloud's base defaults to
`https://server.heyo.computer`; app-lb defaults to the same base and discovers
its namespace from the key on first use — one namespace is the answer, several
is ambiguous and names them, none says how to create one. Through that door the
app-lb credential *is* a heyo API key, so a lone `HEYO_API_KEY` configures both;
the second variable exists for a deployment that wants them separate, and for a
self-hosted app-lb where they genuinely differ.

Everything below is for the cases that need more: a self-hosted app-lb, app-obs,
ci, or a cloud that is not the public one.

## Configuration

| Variable | Purpose |
|---|---|
| `HEYO_API_KEY` | heyo cloud API key — the sandbox tools, and app-lb's default credential |
| `HEYO_BASE_URL` | cloud base URL; defaults to `https://server.heyo.computer` |
| `APPLB_URL` | app-lb base URL — its own admin listener, or heyo cloud (see managed mode). Unset means the managed door at cloud's base |
| `APPLB_NAMESPACE` | managed mode: the namespace to reach app-lb in, through heyo cloud |
| `APPLB_TOKEN` | bearer token (a `heyo_api_*` key in managed mode), or… |
| `APPLB_BASIC` | `user:pass`, or a complete `Basic …` header |
| `APP_OBS_URL` | app-obs base URL |
| `APP_OBS_API_TOKEN` | bearer for its query routes (`/healthz` stays open) |
| `CI_URL` | ci's **own** listener — see above |
| `CI_TOKEN` | bearer, if ci is reached somewhere that wants one |
| `HEYO_MCP_TIMEOUT_MS` | per-request bound, default 30000 |

Each service is independent: configure one and its tools work while the others
report themselves unconfigured. `heyo_status` says which is which — and its
app-lb probe is also what resolves the managed namespace, so an ambiguous one
surfaces there rather than inside some later call.

A `Basic` value is passed through byte for byte, because app-lb compares it that
way — a re-encoded-but-equivalent header is rejected.

## ci needs a direct URL, and no token changes that

`ci` deployed behind an app-lb `AuthGate` **admits browsers and nothing else.**
The gate splits on `Accept: text/html`, and ci's `public_paths` are only:

```json
["/healthz", "/api/submit", "/api/stream/", "/__ui/"]
```

Every page worth reading — runs, jobs, `/networks`, `/runners`, `/vms`,
`/repos` — is outside that list, so a machine client is refused whatever
credential it presents. This is deliberate in ci: minting a submit token is
minting the right to run code on a runner, so those routes are for browsers with
an admin role.

So `CI_URL` wants **ci's own listener**, not its public hostname — from the host
it runs on, or through an SSH tunnel:

```bash
ssh -N -L 8081:127.0.0.1:8081 us2.heyo.work   # then CI_URL=http://127.0.0.1:8081
```

Pointed at the gated host, ci tools fail with that explanation rather than a
bare 401, because a token hunt is the wrong response to it.

app-lb and app-obs are ordinary bearer APIs and need no such arrangement.

## Sandboxes

`sandbox_create` boots a microVM and returns its id; every other sandbox tool
names that id. There is **one endpoint for every sandbox** — no per-sandbox
connection, nothing to re-establish after a restart, and `sandbox_list` recovers
an id that was lost. A sandbox outlives the call that made it: it is a VM with a
TTL, not a request scope.

```
sandbox_create → id
sandbox_exec / sandbox_read_file / sandbox_write_file
sandbox_set_ttl        keep it alive across a conversation
sandbox_stop / start   park it: disk kept, TTL clock stopped
sandbox_kill           destroy it and its disk
```

### The 1 MB body, and the way around it

Cloud's JSON API caps a request body at 1 MB, and file writes cross it base64,
so ~768 KB of payload is already `413 Request Entity Too Large` — 512 KB
succeeds, measured against the live API. MCP inherits that limit because it is
the same API underneath; a photograph from a phone routinely exceeds it.

The archive route is not subject to it, because the bytes never enter a JSON
body:

```
sandbox_upload_url        → { archive_id, upload_url }
PUT the tar.gz to upload_url   ← Content-Type: application/gzip, NO Authorization
sandbox_finalize_upload   → the archive is now usable
sandbox_create { archive_id }  or  sandbox_attach_archive { id, archive_id }
```

The presigned URL belongs to the object store, so its ceiling is the store's,
not the API's — hundreds of megabytes, which is the case it exists for. The
signature is the credential; sending a bearer alongside it is what makes some
stores refuse. `sandbox_attach_archive` is what makes this usable
mid-conversation: it mounts an archive onto a sandbox that is already running,
so a large file reaches an existing sandbox without booting a new one to carry
it.

`sandbox_write_file` refuses more than 512 KiB rather than spending a round trip
to be told 413, and the refusal names this route.

### 503 is capacity, not a fault

`ApiError(503): No available backend in region US supports libvirt` means no
host in that region runs that driver with room to spare. Retrying immediately
fails identically; retrying with backoff does not. So `sandbox_create` retries
exactly that status — three attempts by default, doubling from 2s, `retries: 0`
to disable — and retries nothing else, because a rejected spec only gets
rejected again. Naming a driver the region actually runs (`firecracker`) or the
other region often succeeds where the default did not.

`heyo_capacity` is the pre-flight, and is honest about its reach: it lists the
daemons *this key* has registered, online or not, plus every sandbox already
running. Cloud publishes no per-region or per-driver free capacity, so for
heyo-hosted regions a 503 on create remains the first signal.

## Managed mode

Heyo runs one app-lb as a platform service. Customers do not reach its admin
listener; they reach it through heyo cloud, per namespace, at
`/namespaces/{ns}/lb/…`, with their ordinary `heyo_api_*` key. Cloud pins every
request to that namespace and app-lb resolves the key into a grant, so the
same admin API answers, walled to what the key may see.

This server needs no code for that — only a different base:

```bash
APPLB_URL=https://server.heyo.computer \
APPLB_NAMESPACE=team-a \
APPLB_TOKEN=heyo_api_… \
node dist/index.js
```

`APPLB_NAMESPACE` turns the base into `${APPLB_URL}/namespaces/team-a/lb`;
every tool then appends its path as before. (Spelling the full
`…/namespaces/team-a/lb` URL out in `APPLB_URL` works too and is not rewritten
again.) A namespace is created once, with `POST /namespaces` on cloud or the
SDK's `Namespaces.create`, and deployments registered through this door land
in it whether or not the spec says so.

What the door exposes: `applb_list_deployments`, `applb_get_deployment`,
`applb_create_deployment`, `applb_scale`, `applb_start_build`,
`applb_start_update`, `applb_deployment_jobs`, `applb_delete_deployment`,
`applb_evict_vm`, `applb_exec` and `applb_metrics`. The fleet-wide operator
tools — `applb_disks`, `applb_certs`, `applb_purge_disk`,
`applb_purge_orphan_disks`, `applb_sweep_disks` — answer `404 route not exposed
through the namespace proxy`, and `applb_request` is bounded by the same list.
VMs a managed deployment boots are billed to the namespace's account like any
other sandbox.

A hosted instance can serve many tenants from one process: leave `APPLB_TOKEN`
and `APPLB_BASIC` unset and, in HTTP mode, each request's own `Authorization`
header goes upstream instead, so every caller acts under their own key and
therefore their own namespaces. A configured token always wins over a caller's
header — an instance deployed to act as itself must not be talked into acting
as someone else.

## Running it

```bash
npm install && npm run build
```

Register with Claude Code:

```bash
claude mcp add heyo -- node /home/sarocu/Projects/heyo-public/mcp/dist/index.js
```

Credentials come from the environment the host launches it in, so nothing is
deployed and no secret lives in this directory.

## Running it as a deployment, behind heyo's JWT gate

`deploy/heyo-mcp.json` registers this with app-lb as a static (`proxy_pass`)
deployment — a host process, like app-obs — gated on JWTs issued by the Heyo
auth API:

```jsonc
"auth": {
  "provider": "jwt",
  "jwt": {
    "secret":        {"secret": "heyo-auth", "key": "jwt_secret"},
    "algorithms":    ["HS256"],
    "issuer":        "auth-service",
    "audience":      "heyo-app",
    "subject_claim": "userId",
    "require":       {"role": ["user", "admin"]}
  },
  "public_paths": ["/healthz"],
  "forward_identity": true
}
```

Four things in that block are load-bearing:

- **`secret` is a reference, never a literal.** The same HMAC value that
  verifies a token also *mints* one, so a spec carrying it would hand anyone who
  can read a deployment the ability to issue identities.
- **`algorithms` has no default, by design.** The algorithm is named in the
  token's own header, which is attacker-controlled: a verifier that dispatches
  on it accepts both `alg: none` and a public key used as an HMAC secret.
- **`require`, not `allowed_domains`/`allowed_emails`.** Those two describe a
  *Google* identity and mean nothing against your own issuer — app-lb refuses a
  jwt-only gate that sets them rather than appearing to restrict something it
  does not. `{"role": ["user", "admin"]}` is the equivalent, and an empty
  `require` means "any signed-in user of this product".
- **`public_paths` is only `/healthz`.** Everything else, `/mcp` included, is
  behind the gate.

Install `deploy/supervisor/heyo-mcp.conf`, fill in the two upstream tokens, and
register the deployment. `HEYO_MCP_HTTP_PORT` is what switches the process from
stdio to HTTP.

### How the server treats the identity

It does **not** re-verify the JWT. app-lb checked the signature, issuer,
audience and `require` claims before forwarding anything, and it strips
`x-auth-request-*` unconditionally before setting them, so they cannot be
spoofed. Re-verifying would need the minting secret in a second process — a
larger risk than the one it removes. The signed token is still in the
`Authorization` header if the app ever wants more than the identity.

What the server *does* check is that a forwarded identity is present at all. The
listener binds loopback and app-lb is the only thing expected to reach it, so a
request without one did not come through the gate — a missing `auth` block, a
path wrongly in `public_paths`, or something on the box talking to the port
directly. All three fail with `401` and an explanation, because every tool
behind this can change production.

The check keys on **`x-auth-request-user`**, falling back to email. That
ordering matters: under a JWT gate `user` carries `subject_claim`, which app-lb
refuses a token for missing, while `email` carries `email_claim` and is filled
with `unwrap_or_default()` — so a valid token from an issuer that sends no email
arrives with that header empty. Keying on email would reject those callers while
telling them they had bypassed the gate.

Every call is logged to stderr with the caller's identity, so "who asked for
that" stays answerable after a destructive tool runs.

`HEYO_MCP_REQUIRE_IDENTITY=0` disables the check.

**An app-token gate is the one shape that needs it off.** app-lb admits
`Authorization: Bearer applb_…` with no `Identity` at all — deliberately: "a
token is not a person, and forwarding `x-auth-request-email` for one would put a
name upstream that belongs to nobody" — so nothing is forwarded and the check
above refuses every request that gate admits. A machine client (an agent, a
daemon, anything holding a static bearer) therefore wants either

- `provider: ["app_token"]` on the deployment **and**
  `HEYO_MCP_REQUIRE_IDENTITY=0` here — safe because the gate in front is doing
  exactly the work this check stands in for, and the port is still loopback; or
- `provider: ["jwt"]`, where the caller's own token carries a subject and the
  check keeps working as written.

Pick deliberately. Turning the check off *without* a gate in front leaves an
unauthenticated hole into a process that can delete a deployment.

## Tools

**Diagnostics** — cross-service, shaped like the question:

| Tool | Answers |
|---|---|
| `heyo_status` | which services are reachable, and what each says about itself |
| `fleet_overview` | every deployment, host CPU/memory, app-lb topology, ingest counters |
| `diagnose_deployment` | one deployment: record, jobs, series, recent errors |
| `deployment_logs` | log lines with app-obs's filters and paging |
| `diagnose_empty_pool` | why a pool is empty or will not fill |
| `diagnose_ci_job` | why a ci job is not running |

**Sandboxes** — heyo cloud:

| Tool | Does |
|---|---|
| `sandbox_create` | boot one and wait for it; `archive_id` seeds `/workspace` |
| `sandbox_list` / `sandbox_info` | find one again; poll readiness |
| `sandbox_exec` | run a command, buffered, with its exit code |
| `sandbox_read_file` / `sandbox_write_file` | small files inline; the write refuses what 413 would |
| `sandbox_upload_url` / `sandbox_finalize_upload` / `sandbox_attach_archive` | the route past the 1 MB body |
| `sandbox_set_ttl` | keep a sandbox alive across a conversation |
| `sandbox_stop` / `sandbox_start` / `sandbox_restart` | park and resume; the disk survives all three |
| `sandbox_kill` | DESTRUCTIVE — the VM and its disk |
| `heyo_capacity` | your daemons and your running sandboxes, before booting more |

**The event feed** — app-lb's per-namespace RSS, as data:

| Tool | Answers |
|---|---|
| `applb_feeds` | which namespaces have events |
| `applb_feed` | deployment lifecycle and issues, newest first, since a cursor |

The feed is **polled, never pushed**, and no subscription state exists anywhere:
app-lb tracks no per-reader watermark, so `applb_feed` takes `since_id` and
returns `latest_id` for the caller to keep. The ring is in memory, so an app-lb
restart empties it and ids begin again — a cursor from before that reads as
ahead of everything, and the tool says "feed reset" and returns the lot rather
than reporting nothing new for ever. Nothing publishes unless a deployment's
spec opts in with `feed.announce` or `feed.issues`.

**Actions** — app-lb reads and lifecycle, ci run control, and `*_request` raw
tools covering everything without a dedicated tool.

### Destructive tools are named, not hidden

`applb_delete_deployment`, `applb_evict_vm`, `applb_purge_disk`,
`applb_purge_orphan_disks`, `applb_exec`, `ci_cancel_run`, `ci_destroy_vm` and
`ci_cleanup_failed_vms` each have their own tool and a description that opens
with `DESTRUCTIVE`. Folding them into a generic request tool would hide a
`DELETE` inside a parameter, where it is invisible in a transcript and in an
approval prompt.

`sandbox_kill` is named the same way, for the same reason.

The raw `heyo_request` / `applb_request` / `obs_request` / `ci_request` tools
reach the rest of each API, including destructive methods. Prefer a named tool when one exists —
the raw one's intent cannot be read without reading its arguments.

`applb_purge_orphan_disks` deserves particular care: *orphaned* is app-lb's
inference, and a disk belonging to something app-lb has lost track of looks
exactly like one belonging to nothing.

## Two things the tools cannot see

**A guest's `start_command` failure.** It writes to the guest's own
`/var/log/heyvm-start.log`, which never reaches app-obs. An empty log section
beside a pool that will not fill is a signal, not the absence of one — read the
file with `applb_exec`.

**Anything ci knows, when ci is behind its gate.** See above.
