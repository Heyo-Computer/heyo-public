# heyo-mcp

An MCP server over the three services that answer operational questions about
this fleet: [app-lb](../app-lb) (deployments and VM pools), [app-obs](../app-obs)
(logs and metrics) and [ci](../ci) (builds).

It exists because those questions span all three. "Why is nothing running" is
app-lb's topology *and* app-obs's logs *and* ci's queue, and a tool per endpoint
leaves that join to be redone by hand every time. The diagnostic tools below do
the join and carry what it cost to learn which endpoint answers what.

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

## Configuration

| Variable | Purpose |
|---|---|
| `APPLB_URL` | app-lb base URL |
| `APPLB_TOKEN` | bearer token, or… |
| `APPLB_BASIC` | `user:pass`, or a complete `Basic …` header |
| `APP_OBS_URL` | app-obs base URL |
| `APP_OBS_API_TOKEN` | bearer for its query routes (`/healthz` stays open) |
| `CI_URL` | ci's **own** listener — see above |
| `CI_TOKEN` | bearer, if ci is reached somewhere that wants one |
| `HEYO_MCP_TIMEOUT_MS` | per-request bound, default 30000 |

Each service is independent: configure one and its tools work while the others
report themselves unconfigured. `heyo_status` says which is which.

A `Basic` value is passed through byte for byte, because app-lb compares it that
way — a re-encoded-but-equivalent header is rejected.

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

`HEYO_MCP_REQUIRE_IDENTITY=0` disables the check, for local testing only.

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

**Actions** — app-lb reads and lifecycle, ci run control, and `*_request` raw
tools covering everything without a dedicated tool.

### Destructive tools are named, not hidden

`applb_delete_deployment`, `applb_evict_vm`, `applb_purge_disk`,
`applb_purge_orphan_disks`, `applb_exec`, `ci_cancel_run`, `ci_destroy_vm` and
`ci_cleanup_failed_vms` each have their own tool and a description that opens
with `DESTRUCTIVE`. Folding them into a generic request tool would hide a
`DELETE` inside a parameter, where it is invisible in a transcript and in an
approval prompt.

The raw `applb_request` / `obs_request` / `ci_request` tools reach the rest of
each API, including destructive methods. Prefer a named tool when one exists —
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
