# HeyoSecret

A small, single-tenant secrets store. Values are encrypted at rest (AES-256-GCM),
versioned, and served over two independent surfaces:

- **Machine API** (`/v1/secrets/*`) — JSON, authenticated by a shared internal
  API key (`Authorization: Bearer <key>`). Used by other Heyo services via the
  `heyosecret-client` crate.
- **Web dashboard** (`/`) — a server-rendered admin UI for humans to inspect and
  manage secrets, authenticated by an admin password that mints a signed
  session cookie. Enabled only when `HEYOSECRET_ADMIN_PASSWORD` is set.

Both surfaces share the same Postgres-backed store; secret values never touch
local disk. There is no tenant/org isolation — secrets are namespaced by `path`
under a single trust boundary.

## Data model

- **Secret** (`heyosecret_secrets`): logical secret keyed by unique `path`, with
  `owner`, `description`, `tags[]`, and `read_access[]`/`write_access[]` metadata
  (stored/returned but **not enforced**).
- **Version** (`heyosecret_secret_versions`): the encrypted value plus lifecycle
  `status` (`active | retiring | revoked | deleted`), `value_sha256`,
  `created_by`, optional `expires_at`, and a free-form JSONB `metadata` bag.
  Writing a new version auto-retires the previous active one.
- **Audit event** (`heyosecret_audit_events`): append-only log of write/revoke
  actions.

Values are opaque bytes — the store never interprets them.

## Configuration

Configured via environment (prefix `HEYOSECRET_`), an optional
`heyosecret.toml`, or `HEYOSECRET_CONFIG_PATH`.

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `HEYOSECRET_DATABASE_URL` (or `DATABASE_URL`) | yes | — | Postgres connection string |
| `HEYOSECRET_INTERNAL_API_KEY` (or `PLATFORM_INTERNAL_API_KEY`) | yes | — | Bearer key for the machine API |
| `HEYOSECRET_MASTER_KEY` | yes | — | ≥32 bytes; derives the value-encryption key |
| `HEYOSECRET_ADMIN_PASSWORD` | no | — | Enables the dashboard; login password |
| `HEYOSECRET_COOKIE_SECURE` | no | `false` | Set `true` behind TLS to add `Secure` to the session cookie |
| `HEYOSECRET_SESSION_TTL_SECONDS` | no | `43200` (12h) | Dashboard session lifetime |
| `HEYOSECRET_SERVER_PORT` | no | `4455` | Listen port |

The session-cookie signing key is derived from `master_key + admin_password`
with a domain-separation tag, so rotating either invalidates existing sessions
and it never overlaps the value-encryption key.

## Running

```sh
export HEYOSECRET_DATABASE_URL="postgres://user:pass@localhost:5432/heyosecret"
export HEYOSECRET_INTERNAL_API_KEY="…"
export HEYOSECRET_MASTER_KEY="…at least 32 bytes…"
export HEYOSECRET_ADMIN_PASSWORD="…"     # omit to disable the dashboard
cargo run
```

Migrations under `migrations/` are applied automatically on startup. Then open
<http://localhost:4455/> and sign in with the admin password.

## Dashboard

The dashboard (`src/assets/dashboard.html`, embedded at compile time) is a
self-contained page — no build step, no external assets. It talks to
cookie-authenticated JSON endpoints under `/dashboard/api/*` and lets you:

- list and filter secrets by path prefix;
- inspect metadata, tags, access lists, and full version history;
- reveal a version's decrypted value (text or base64 for binary);
- write/create a secret (new versions retire the prior active one);
- generate a random value (`rotate-random`);
- revoke a specific version.

The dashboard is kept same-origin (no permissive CORS) and its cookie is
`HttpOnly; SameSite=Strict`. The permissive CORS policy applies only to the
machine `/v1/secrets/*` API.

## Endpoints

Machine API (Bearer): `POST /v1/secrets/read|write|rotate-random|revoke`,
`GET /v1/secrets/metadata|history`, `GET /v1/secrets`, `GET /health`.

Dashboard (session cookie): `GET /`, `POST /dashboard/login`,
`POST /dashboard/logout`, `GET /dashboard/api/session`, and
`/dashboard/api/secret*` mirrors of the machine operations.

## Tests

```sh
cargo test    # session token signing/expiry, cookie parsing, path normalization
```
