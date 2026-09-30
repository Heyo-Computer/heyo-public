# HeyoSecret

HeyoSecret is the HWS secrets store: a small, single-tenant service that keeps versioned, AES-256-GCM-encrypted values in Postgres and serves them to other services over a bearer-authenticated JSON API, with an optional web dashboard for people.

## What it is

HeyoSecret is one Rust binary (`heyosecret/`) with two independent surfaces over one Postgres-backed store:

| Surface | Paths | Who uses it | Authentication |
| --- | --- | --- | --- |
| Machine API | `/v1/secrets/*`, `/health` | Other services, through the [`heyosecret-client`](../heyosecret-client/src/lib.rs) crate or plain HTTP | `Authorization: Bearer <internal API key>` |
| Dashboard | `/`, `/dashboard`, `/dashboard/*`, `/__ui/*` | Operators in a browser | An admin password that mints a signed session cookie, or an upstream app-lb auth gate |

Things to know before you rely on it:

- **One key can do everything.** The machine API has a single shared bearer that can read, write, rotate and revoke every secret at every path. There is no per-path authorization.
- **Access lists are metadata only.** Each secret carries `readAccess` and `writeAccess` lists. They are stored and returned, but never enforced.
- **No tenants.** Secrets are namespaced only by `path`, for example `orchestrator/database-url` or `ci/myapp/prod/DATABASE_URL`. Everything sits inside one trust boundary.
- **Values are opaque bytes.** The API moves them as base64 and never interprets them.
- **Nothing is written to local disk.** Values live only in Postgres, encrypted. A HeyoSecret VM needs no data disk.

Because of the first two points, services that hand secrets on to less-trusted code do their own policy. The [CI controller](ci.md) and the [orchestrator](orchestrator.md) hold the HeyoSecret key. They resolve only the paths a job or deployment is entitled to, and pass on the resulting values, never the key.

## Data model

The single migration, [`migrations/001_init.sql`](../heyosecret/migrations/001_init.sql), creates three tables:

| Table | Holds |
| --- | --- |
| `heyosecret_secrets` | One row per logical secret: unique `path`, `owner`, `description`, `tags[]`, `read_access[]`, `write_access[]`, timestamps |
| `heyosecret_secret_versions` | One row per version: `version` number, `status`, `nonce`, `ciphertext`, `value_sha256`, `created_by`, `expires_at`, JSONB `metadata` |
| `heyosecret_audit_events` | Append-only log of `write` and `revoke` actions, with path, version and actor |

A version's `status` is one of `active`, `retiring`, `revoked` or `deleted`. A partial unique index allows at most one `active` version per secret.

- Each write creates version `max + 1` as `active` and moves the previous active version to `retiring`, in one transaction.
- `revoke` sets one version to `revoked`. If you revoke the active version, the secret has no active version until the next write, so a read without a version number returns 404.
- A read that names a version returns any status except `deleted`, including `retiring` and `revoked`. Revoking a version stops it from being the default read. It does not make the value unreadable.
- No API sets `deleted` or `expires_at`. They exist in the schema but nothing in the service uses them yet.

### Paths

Paths are normalized before use. Leading and trailing `/` are stripped. The path must be 1 to 1024 characters of ASCII letters, digits and `/ - _ . : @`, with no empty, `.` or `..` segments. `/platform/jwt/key/` is stored as `platform/jwt/key`.

`GET /v1/secrets?prefix=a/b` matches `a/b` itself and everything under `a/b/`. It matches whole segments, so `ci/app` does not match `ci/app2`.

### Encryption

- The value-encryption key is `SHA-256(master_key || "heyo-secret-v1")`. Each value gets a random 12-byte nonce, and `value_sha256` records a digest of the plaintext.
- The dashboard session-signing key is `SHA-256(master_key || admin_password || "heyo-secret-session-v1")`. It never overlaps the value key. Changing either the master key or the admin password invalidates every existing session.
- There is no key-rotation tooling. **If you change `HEYOSECRET_MASTER_KEY`, you can no longer decrypt existing values.** Keep an independent, secured copy of the master key.

## Running it

### Locally

```sh
export HEYOSECRET_DATABASE_URL="postgres://user:pass@localhost:5432/heyosecret"
export HEYOSECRET_INTERNAL_API_KEY="$(openssl rand -hex 32)"
export HEYOSECRET_MASTER_KEY="$(openssl rand -hex 32)"   # at least 32 bytes
export HEYOSECRET_ADMIN_PASSWORD="choose-one"             # omit to disable the dashboard
cargo run --locked --manifest-path heyosecret/Cargo.toml
```

On startup the service applies every `*.sql` file in the migrations directory, in filename order. It does not keep a migration-tracking table, so every migration must be idempotent (`CREATE ... IF NOT EXISTS`). The migrations directory is resolved in this order:

1. `HEYOSECRET_MIGRATIONS_DIR`, if that path exists
2. `./migrations` relative to the working directory
3. the crate's `migrations/` directory at compile time

It listens on `0.0.0.0:4455` by default. Open `http://localhost:4455/` and sign in with the admin password.

### On a host

Three ways to run it are checked in. Pick one:

| How | Files | Notes |
| --- | --- | --- |
| Directly on a host under supervisord | [`deploy/supervisor/heyosecret.conf`](../heyosecret/deploy/supervisor/heyosecret.conf) | The release binary and migrations are published by [`.ci/workflows/heyosecret.yml`](../.ci/workflows/heyosecret.yml) and installed by the repository installers. See [installation](installation.md). |
| As an app-lb managed microVM | [`heyosecret/Dockerfile`](../heyosecret/Dockerfile), [`heyosecret/init.sh`](../heyosecret/init.sh), [`app-lb/examples/heyosecret.json`](../app-lb/examples/heyosecret.json) | Build from the **repository root** (`"context": "."`), because the binary includes the shared `ui/` kit. `init.sh` does not start the service. app-lb's `start_command` does, because that is the only process that receives `env_vars` and resolved `env_from` values. |
| As an orchestrator-managed service | [`.heyo/services/heyosecret.json`](../.heyo/services/heyosecret.json), [`.heyo/workflows/deploy-heyo-services.yml`](../.heyo/workflows/deploy-heyo-services.yml) | The orchestrator creates a replacement VM, checks `/health`, cuts over and drains the previous VM. See [orchestrator](orchestrator.md). |

When HeyoSecret is deployed through app-lb or the orchestrator, its own credentials come from `env_from` references rather than literals in the spec. For example, `{"secret": "heyosecret", "key": "master-key", "as": "HEYOSECRET_MASTER_KEY"}`.

For orchestrator-managed updates, the running HeyoSecret must already contain `heyosecret/database-url` and `heyosecret/master-key`. The orchestrator reads them from the running instance to start the replacement. This self-reference supports zero-downtime updates. It does not let you recover from a complete HeyoSecret outage, which is another reason to keep a separate copy of the master key.

Keep an existing store's database and master key when you redeploy. Do not start an empty replacement store or rotate keys as part of a deployment.

## Configuration

Settings come from three sources:

- **Explicitly read environment variables.** These are listed in the table below.
- **An optional TOML file.** HeyoSecret reads the file at `HEYOSECRET_CONFIG_PATH` if that is set. Otherwise it looks for `heyosecret.toml` in the crate source directory, which only exists under `cargo run`.
- **A `HEYOSECRET_`-prefixed environment layer.** Its keys are split on `_`, so it only works for single-word keys. The config struct has none, so in practice this layer sets nothing.

**Settings whose names contain an underscore can only come from the TOML file unless the table lists an environment variable for them.** `HEYOSECRET_COOKIE_SECURE`, `HEYOSECRET_SESSION_TTL_SECONDS` and `HEYOSECRET_DB_*` have no effect. Set `cookie_secure`, `session_ttl_seconds` and `db_*` in the TOML file instead.

A value set in the TOML file wins over the matching environment variable. The one exception is `HEYOSECRET_SERVER_PORT`, which overrides the file.

| Environment variable | TOML key | Default | Meaning |
| --- | --- | --- | --- |
| `HEYOSECRET_DATABASE_URL`, else `DATABASE_URL` | `database_url` | required | Postgres connection string |
| `HEYOSECRET_INTERNAL_API_KEY`, else `PLATFORM_INTERNAL_API_KEY` | `internal_api_key` | required | Bearer key for the machine API |
| `HEYOSECRET_MASTER_KEY` | `master_key` | required | At least 32 bytes. The value-encryption key is derived from it. |
| `HEYOSECRET_ADMIN_PASSWORD` | `admin_password` | empty | Enables the dashboard with a password login |
| `HEYOSECRET_DASHBOARD_GATE` (`1`, `true`, `yes`, `on`) | `dashboard_gate` | `false` | Enables the dashboard and trusts the identity app-lb forwards. Startup fails if an admin password is also set. |
| `HEYOSECRET_SERVER_PORT` | `server_port` | `4455` | Listen port |
| `HEYOSECRET_MIGRATIONS_DIR` | none | see above | Migrations directory |
| `HEYOSECRET_CONFIG_PATH` | none | none | Path to the TOML file |
| none | `cookie_secure` | `false` | Adds `Secure` to the session cookie. Set it to `true` behind TLS. |
| none | `session_ttl_seconds` | `43200` (12 h) | Dashboard session lifetime |
| none | `db_max_connections`, `db_min_connections` | `20`, `2` | Postgres pool size |
| none | `db_acquire_timeout_seconds`, `db_idle_timeout_seconds`, `db_max_lifetime_seconds` | `30`, `600`, `1800` | Postgres pool timeouts |
| `HEYOSECRET_UI_COOKIE_DOMAIN`, else `HEYO_UI_COOKIE_DOMAIN` | none | host-only | Parent domain for the shared light/dark theme cookie |
| `HEYOSECRET_UI_COOKIE_NAME`, else `HEYO_UI_COOKIE_NAME` | none | `heyo_theme` | Theme cookie name |
| `RUST_LOG` | none | `info` | Log filter |

An example TOML file:

```toml
database_url = "postgres://heyosecret:change-me@127.0.0.1:5432/heyosecret"
internal_api_key = "change-me"
master_key = "change-me-at-least-32-bytes-long-000000"
admin_password = "change-me"
cookie_secure = true
session_ttl_seconds = 28800
db_max_connections = 10
```

Keep this file `0600`, because it holds every credential the service has.

## Machine API

Every route except `/health` requires `Authorization: Bearer <internal API key>`. The key is compared in constant time. A missing or wrong key returns `401 {"error":"Unauthorized"}`. JSON field names are camelCase. These routes send permissive CORS headers. The dashboard routes do not.

| Method and path | Body or query | Returns |
| --- | --- | --- |
| `GET /health` | none | `{"status":"ok"}`, no authentication |
| `POST /v1/secrets/write` | `{path, valueBase64, owner?, description?, tags?, readAccess?, writeAccess?, metadata?, actor?}` | `{path, version, status:"active"}` |
| `POST /v1/secrets/read` | `{path, version?}` | `{path, version, status, valueBase64, createdAt, metadata}`. Without `version`, returns the active version. `404` if there is none. |
| `POST /v1/secrets/rotate-random` | `{path, bytes? (1-4096, default 32), actor?, metadata?}` | Writes a new random value as the active version and returns it like `write` does |
| `POST /v1/secrets/revoke` | `{path, version, actor?}` | `{"status":"revoked"}` |
| `GET /v1/secrets/metadata?path=` | none | `{path, owner, description, tags, readAccess, writeAccess, createdAt, updatedAt, activeVersion}` |
| `GET /v1/secrets/history?path=` | none | `{path, versions:[{version, status, valueSha256, createdBy, createdAt, expiresAt, metadata}]}`, newest first, with no values |
| `GET /v1/secrets?prefix=` | none | `{secrets:[metadata...]}`, sorted by path. Omit `prefix` to list everything. |

On a write to an existing path, a non-empty `owner`, `description`, `tags`, `readAccess` or `writeAccess` replaces the stored value. An omitted field or an empty list keeps the stored value, so you cannot clear a list back to empty through the API.

```sh
H="Authorization: Bearer $HEYOSECRET_INTERNAL_API_KEY"

# write
curl -sX POST localhost:4455/v1/secrets/write -H "$H" -H 'content-type: application/json' \
  -d "{\"path\":\"myapp/database-url\",\"valueBase64\":\"$(printf %s 'postgres://...' | base64 -w0)\",\"actor\":\"me\"}"

# read the active value
curl -sX POST localhost:4455/v1/secrets/read -H "$H" -H 'content-type: application/json' \
  -d '{"path":"myapp/database-url"}' | jq -r .valueBase64 | base64 -d

# generate a 48-byte random value as a new version
curl -sX POST localhost:4455/v1/secrets/rotate-random -H "$H" -H 'content-type: application/json' \
  -d '{"path":"myapp/session-key","bytes":48}'

# list and inspect
curl -s "localhost:4455/v1/secrets?prefix=myapp" -H "$H"
curl -s "localhost:4455/v1/secrets/history?path=myapp/database-url" -H "$H"
```

### Rust client

`heyosecret-client` wraps the machine API. `HeyoSecretClient::from_env()` reads `HEYOSECRET_URL` and `HEYOSECRET_INTERNAL_API_KEY`, falling back to `PLATFORM_INTERNAL_API_KEY`. `HeyoSecretClient::new(HeyoSecretClientOptions { base_url, token, timeout })` takes them explicitly. The default request timeout is 10 seconds.

| Method | Calls |
| --- | --- |
| `read_active(path)`, `read(path, Some(version))` | `POST /v1/secrets/read`. Returns decoded bytes in `SecretValue.value`. |
| `put(path, value, PutSecretOptions)` | `POST /v1/secrets/write` |
| `rotate_random(path, bytes, actor)` | `POST /v1/secrets/rotate-random` |
| `revoke(path, version, actor)` | `POST /v1/secrets/revoke` |
| `metadata(path)`, `history(path)`, `list(prefix)` | The corresponding `GET` routes |

Errors are `HeyoSecretError::{MissingConfig, Encoding, Api { status, message }, Network}`.

## Dashboard

The dashboard is one self-contained page, [`src/assets/dashboard.html`](../heyosecret/src/assets/dashboard.html), embedded at compile time. It has no build step and loads no external assets. Its JSON endpoints under `/dashboard/api/` mirror the machine API:

| Dashboard route | Machine equivalent |
| --- | --- |
| `GET /dashboard/api/session` | Reports `{dashboardEnabled, authenticated, gated}` |
| `POST /dashboard/login` (`{"password": "..."}`), `POST /dashboard/logout` | none |
| `GET /dashboard/api/secrets?prefix=` | `GET /v1/secrets` |
| `GET /dashboard/api/secret/metadata?path=` | `GET /v1/secrets/metadata` |
| `GET /dashboard/api/secret/history?path=` | `GET /v1/secrets/history` |
| `POST /dashboard/api/secret/read` | `POST /v1/secrets/read`. This reveals the value. |
| `POST /dashboard/api/secret/write` | `POST /v1/secrets/write` |
| `POST /dashboard/api/secret/rotate-random` | `POST /v1/secrets/rotate-random` |
| `POST /dashboard/api/secret/revoke` | `POST /v1/secrets/revoke` |

The dashboard has two mutually exclusive authentication modes:

- **Password mode** (`HEYOSECRET_ADMIN_PASSWORD`). `POST /dashboard/login` sets the `heyosecret_session` cookie (`HttpOnly; SameSite=Strict; Path=/`, plus `Secure` when `cookie_secure = true`). The cookie holds an HMAC-signed expiry, so the server keeps no session state.
- **Gate mode** (`HEYOSECRET_DASHBOARD_GATE=1`). HeyoSecret shows no login of its own. It trusts the `x-auth-request-*` identity headers that app-lb forwards after its own auth gate. A request with no forwarded identity gets `401`, and there is no sign-out button because the session belongs to app-lb. Use this mode only behind app-lb or another proxy that strips those headers before setting them. On a listener anyone can reach, gate mode is an open dashboard.

If neither mode is configured, the page loads but every dashboard API call returns `503 dashboard is disabled`. The machine API is unaffected.

To put HeyoSecret behind an app-lb gate, leave the machine API and health check outside the gate. Add `/health`, `/v1/` and `/__ui/` to the gate's `public_paths`, as [`app-lb/examples/heyosecret.json`](../app-lb/examples/heyosecret.json) does, and make sure those paths are reachable by machine callers. See [app-lb auth](app-lb-auth.md). The machine API still requires its own bearer key. After you publish the route, check three things:

- an anonymous request to `/` is rejected
- an anonymous request to `/v1/secrets` returns `401`
- an authenticated read succeeds (don't print the value while checking)

## How other services use it

| Consumer | What it reads | Configuration |
| --- | --- | --- |
| [Orchestrator](orchestrator.md) | `envRefs` of the form `NAME=heyosecret://<path>@active` or `@<version>`, resolved just before a service VM is created. It also reads credentials for observers, lifecycle tokens, git auth and similar, by HeyoSecret path. | `ORCHESTRATOR_HEYOSECRET_URL` or `HEYOSECRET_URL`, and `ORCHESTRATOR_HEYOSECRET_INTERNAL_API_KEY` or `HEYOSECRET_INTERNAL_API_KEY`. If no HeyoSecret key is set, the orchestrator uses its own internal API key. |
| [CI](ci.md) | Every secret under `ci/<workflow>/<environment>/`, listed once per job. Entries tagged `public` become `${{ vars.* }}` in plain text. Everything else becomes `${{ secrets.* }}` and is masked in logs. Controller settings live under `ci-controller/`. | `CI_HEYOSECRET_URL` or `HEYOSECRET_URL`, and `CI_HEYOSECRET_TOKEN` or `HEYOSECRET_INTERNAL_API_KEY`. Builds never see the HeyoSecret key. |
| [app-lb](app-lb.md) | Nothing directly. See below. | none |

### app-lb `env_from` and `{secret, key}` references

app-lb does **not** call HeyoSecret. Its `{"secret": ..., "key": ...}` references, used in `vm.env_from`, `build.auth`, `discovery.source.auth`, gateway `auth` and elsewhere, resolve against **app-lb's own secret store**. That store is a separate set of namespaced key/value bags, managed through app-lb's `/secrets` admin API or `heyctl`, persisted `0600`, and optionally sealed with `APP_LB_SECRET_KEY`. See [app-lb](app-lb.md).

The convention in HWS deployments is that HeyoSecret is the source of truth and app-lb holds a delivery copy:

1. Store the canonical value in HeyoSecret, for example `heyosecret/master-key`.
2. Copy it into an app-lb secret, for example secret `heyosecret` with key `master-key`, through app-lb's secrets API.
3. Reference it from the deployment spec:

```json
"env_from": [
  {"secret": "heyosecret", "key": "database-url", "as": "HEYOSECRET_DATABASE_URL"},
  {"secret": "heyosecret", "key": "master-key",   "as": "HEYOSECRET_MASTER_KEY"}
]
```

The orchestrator's service spec uses the same `env_from` shape but resolves it differently. There, `{"secret": "orchestrator", "key": "database-url"}` means the active version of the HeyoSecret path `orchestrator/database-url`. `key` defaults to `token`, and `as` defaults to the key in upper case. When a rotated value lives in two places, update both. The app-lb copy is what running app-lb VMs receive.

## Operations

**Rotate a credential.** Write a new version with `write` or `rotate-random`. Readers of the active version get it on their next read. The old version stays readable by its version number until you `revoke` it. Services that cached the value, including app-lb delivery copies and running VMs, need their own refresh or restart.

**Audit.** Writes and revokes are recorded in `heyosecret_audit_events`. Reads are not audited. There is no API for the audit log, so query the table directly.

**Back up.** Back up the Postgres tables and, separately, the master key. Either one alone is useless.

**Test.**

```sh
cargo test --locked --manifest-path heyosecret/Cargo.toml
```

The tests cover session signing and expiry, cookie parsing, path normalization and the dashboard template.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| Startup fails with `HEYOSECRET_MASTER_KEY must be at least 32 bytes` | The key is shorter than 32 bytes before derivation. |
| Startup fails with `HEYOSECRET_DASHBOARD_GATE and HEYOSECRET_ADMIN_PASSWORD are both set` | Pick one dashboard mode. |
| Startup fails with `No HeyoSecret migrations directory found` | Set `HEYOSECRET_MIGRATIONS_DIR` to the shipped `migrations/` directory. Release VMs use `/opt/heyosecret/migrations` or `/workspace/migrations`. |
| `failed to decrypt secret value` on read | The master key differs from the one that encrypted the value. |
| Session cookie missing `Secure` although `HEYOSECRET_COOKIE_SECURE=true` | That variable is not read. Set `cookie_secure = true` in the TOML file. |
| Dashboard shows the login but every call returns `503` | Neither `HEYOSECRET_ADMIN_PASSWORD` nor `HEYOSECRET_DASHBOARD_GATE` is set. |
| Gated dashboard returns `no forwarded identity` | The request did not pass through app-lb's auth gate, for example a direct hit on the VM or an app-token caller. |
| A read returns 404 right after a revoke | You revoked the active version. Write a new version, or read a specific one. |
