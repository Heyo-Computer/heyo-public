---
name: heyvm-database
description: Create, list, connect to, and run SQL against Heyo cloud sqlite databases via the heyvm CLI and SDK. Use when the user wants to manage a sqlite database for an agent, run ad-hoc SQL, or wire an app to a Heyo database.
argument-hint: "[subcommand] [args...]"
allowed-tools: Bash, Read, Grep
---

# heyvm CLI — Cloud SQLite Databases

> **See also:** load the **heyvm-docs** skill for an overview of the heyvm
> platform and an index of the other heyvm-* skills (sandbox, deploy, proxy,
> firecracker, login, system).

You are interacting with the `heyvm sqlite` CLI surface and the
`Database` class in `@heyo/sdk`. Both talk to the same cloud API
(`/sqlite-databases/...`) and let an agent create a managed sqlite
database, run SQL, and mint libsql-compatible connection tokens for
external clients.

## Overview

- **Cloud-only.** SQLite databases are managed by the Heyo cloud — there is
  no `heyvm sqlite` flavor that runs against a local sandbox. `heyvm login`
  first.
- **Backed by the same backend pool as sandboxes.** Each db lives on a
  cloud backend node (libvirt/firecracker) in a single region; the cloud
  routes exec requests to the node currently hosting the db.
- **Durable on S3.** A periodic snapshot plus streaming WAL means a db can
  be cold-restored onto a different backend; expect a brief "starting"
  delay the first time a cold db is touched.
- **Single writer.** All exec calls funnel through one writer; reads are
  served from the same node. There is no read-replica fan-out today.

## Binary Location

The binary is `heyvm`. If it is not on PATH, build it from `mvm-ctrl/`:

```bash
cargo build --release -p heyvm
```

## Subcommands

```
heyvm sqlite <command>

  create      Create a new sqlite database in a cloud region
  list        List sqlite databases for the current account
  get         Show details for a single sqlite database
  delete      Delete a sqlite database
  regions     List cloud regions where sqlite databases can be created
  exec        Run a single SQL statement against a database
  shell       Open an interactive SQL shell against a database
  checkout    Download the canonical sqlite file for offline editing
  checkin     Upload a locally edited file with optimistic concurrency
```

`heyvm sqlite connect` and `heyvm sqlite revoke-token` are not yet
exposed as CLI subcommands — mint connection tokens via the SDK or a
direct `POST /sqlite-databases/{id}/connection` call (see "Connection
strings & auth" below).

## Lifecycle

### Pick a region

```bash
heyvm sqlite regions
```

### Create

```bash
heyvm sqlite create --name notes --region us-east
# optional: --size-class small|medium|large
```

The id (`db-…`) is returned in the JSON response. Capture it for
subsequent commands:

```bash
DB_ID=$(heyvm sqlite create --name notes --region us-east | jq -r .id)
```

### List / get / delete

```bash
heyvm sqlite list
heyvm sqlite get db-abc123
heyvm sqlite delete db-abc123
```

`get` and `list` show `status` (`creating`, `running`, `failed`,
`deleted`) and any `error_message`.

## Running SQL

### One-shot exec

```bash
# Plain SELECT
heyvm sqlite exec db-abc123 "SELECT name FROM sqlite_master WHERE type='table'"

# Bound args. ?-placeholders consume --arg values left to right.
# Values starting with int:, float:, bool:, or null are typed; everything
# else is TEXT.
heyvm sqlite exec db-abc123 \
  "INSERT INTO notes (id, body) VALUES (?, ?)" \
  --arg int:1 --arg "hello world"

heyvm sqlite exec db-abc123 \
  "SELECT * FROM notes WHERE id = ?" --arg int:1 --format json
```

`--format` is `table` (default) or `json`. The server clamps row output
(default 1000, hard cap 10000); when truncated the JSON response carries
`"truncated": true`.

### Multi-statement transactions

`heyvm sqlite exec` only sends one statement per call. For a
BEGIN/COMMIT batch use the SDK (`db.batch([...], { transaction: 'immediate' })`)
or the cloud HTTP `POST /sqlite-databases/{id}/exec` endpoint with a
`statements: [...]` array and `transaction: "immediate"`.

### Interactive shell

```bash
heyvm sqlite shell db-abc123
```

Each line of input is sent as a single statement. Useful for poking at
schema / data; for any scripted flow prefer `exec`.

## Connecting from code

### TypeScript SDK (`@heyo/sdk`)

```ts
import { Database } from "@heyo/sdk";

const db = await Database.create({ name: "notes", region: "us-east" });
await db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)");
await db.exec("INSERT INTO notes (body) VALUES (?)", ["hello"]);

const r = await db.exec("SELECT id, body FROM notes ORDER BY id");
r.rows.forEach(([id, body]) => console.log(id, body));

// Transactional batch — rolled back on any error.
await db.batch(
  [
    { sql: "INSERT INTO notes (body) VALUES (?)", args: ["a"] },
    { sql: "INSERT INTO notes (body) VALUES (?)", args: ["b"] },
  ],
  { transaction: "immediate" },
);

// Open by id from another process:
const same = await Database.get("db-abc123");
```

`Database` re-exported from `@heyo/sdk/index.ts`. `HeyoClientOptions`
takes `{ baseUrl, token }` if you need to override the default cloud
URL or auth (default reads `HEYO_TOKEN` / login session).

### libsql client (any language)

For sustained traffic from outside the SDK, mint a connection token and
plug it into any libsql HTTP client. The cloud serves the Hrana v2/v3
JSON pipeline at `/v2/pipeline` and `/v3/pipeline` under the minted URL,
which is what `@libsql/client` posts to:

```ts
import { createClient } from "@libsql/client";
import { Database } from "@heyo/sdk";

const db = await Database.get("db-abc123");
const conn = await db.connect({ scopes: ["read", "write"], ttlSeconds: 3600 });

const client = createClient({ url: conn.url, authToken: conn.authToken });
await client.execute("SELECT 1");
await client.batch([
  "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT)",
  { sql: "INSERT INTO notes (body) VALUES (?)", args: ["hello"] },
], "write");
```

**Unsupported Hrana features** (return `LibsqlError` with `NOT_IMPLEMENTED`):
- Cursors (`fetch_cursor`)
- `store_sql` / `execute_stored`
- Named args (use positional `?` placeholders instead)
- Blob args (encode as base64 strings and decode in SQL if needed)
- Conditional batch steps
- Stateful transactions spanning multiple pipeline calls — wrap in
  `client.batch(...)` instead.

### Plain HTTP (curl, etc.)

Two endpoints under the minted URL:

```bash
# Mint a token (returns { id, url, auth_token, scopes, expires_at }).
TOKEN_JSON=$(curl -s -X POST \
  -H "Authorization: Bearer $HEYO_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"ttl_seconds": 3600, "scopes": ["read", "write"]}' \
  "$HEYO_CLOUD_URL/sqlite-databases/db-abc123/connection")

URL=$(echo "$TOKEN_JSON" | jq -r .url)
DB_TOKEN=$(echo "$TOKEN_JSON" | jq -r .auth_token)

# Note: $URL ends with a trailing slash so that libsql clients resolve
# relative `v2/pipeline` correctly. Strip it when concatenating by hand.
URL_NOSLASH="${URL%/}"

# Option A: Hrana v2 pipeline (what @libsql/client speaks).
curl -s -X POST "$URL_NOSLASH/v2/pipeline" \
  -H "Authorization: Bearer $DB_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"baton":null,"requests":[
        {"type":"execute","stream_id":0,
         "stmt":{"sql":"SELECT 1","args":[],"want_rows":true}}
      ]}'

# Option B: simple exec proxy — internal ExecRequest shape, no Hrana
# value-tagging. Cheaper to write by hand.
curl -s -X POST "$URL_NOSLASH/v1/execute" \
  -H "Authorization: Bearer $DB_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"statements":[{"sql":"SELECT 1"}]}'
```

## Checkout / checkin (offline editing)

When you want to open the database with vanilla sqlite tooling — `sqlite3`
CLI, DataGrip, the Python `sqlite3` module, etc. — none of which speak
libsql HTTP — use checkout/checkin to round-trip the raw file.

The flow uses **optimistic concurrency**. Every write through cloud bumps
a `data_version` counter on the row. Checkout returns the current version
in an `X-Heyo-Data-Version` header and stashes it in a `<file>.heyo`
sidecar. Checkin sends the expected version back; cloud refuses the
upload (HTTP 409) if the version moved while you were editing — unless
you pass `--force`.

```bash
# 1. Check out. Writes db-abc123.db.gz + db-abc123.db.gz.heyo (sidecar).
heyvm sqlite checkout db-abc123

# 2. Edit locally — gunzip first, then any sqlite tool.
gunzip db-abc123.db.gz
sqlite3 db-abc123.db "UPDATE notes SET body='edited' WHERE id=1"

# 3. Re-gzip and check back in.
gzip db-abc123.db
heyvm sqlite checkin db-abc123 db-abc123.db.gz
# → "Checked in db-abc123 (… bytes) — new data_version=N"

# If another client wrote to the db in the meantime:
# → "Checkin refused: remote moved (expected=N, current=M). Re-run checkout, …"
heyvm sqlite checkin db-abc123 db-abc123.db.gz --force   # overrides, last-write-wins
```

Notes:
- The file is gzipped (mvm-ctrl snapshots use `VACUUM INTO` + gzip). Add
  `gunzip`/`gzip` to your loop or use a tool that handles `.db.gz` directly.
- Concurrent writes via the libsql/Hrana path will bump `data_version`
  and cause your checkin to 409. Pause client writes while editing if
  you want a clean round-trip.
- `--force` skips the version check and overwrites whatever's there.
  Anyone else's intervening writes are lost.
- Cloud writes the new snapshot to a fresh S3 key on every checkin
  rather than overwriting; prior snapshots remain for forensic recovery
  (no automatic GC yet).
- There's a small race at checkout time: writes that land between the
  snapshot trigger and the data_version read won't be in the file but
  will bump the version. Your checkin will succeed (versions match) and
  overwrite that write. Keep workloads paused during edit if this matters.

## Connection strings & auth

Connection tokens are *separate* from the user-level Heyo JWT used for
management calls. They identify as `Bearer heyo_db_<…>`, are scoped per
database, and can be:

- **Minted** with `Database.connect({ ttlSeconds, scopes })` (TS SDK) or
  `POST /sqlite-databases/{id}/connection`. Plaintext is returned **only
  at mint time** — persist it immediately.
- **Listed** with `Database.listConnections()` (no plaintext, just
  metadata) or `GET /sqlite-databases/{id}/connection-tokens`.
- **Revoked** with `Database.revokeConnection(tokenId)` or
  `DELETE /sqlite-databases/{id}/connection-tokens/{token_id}`. After
  revocation, the next request returns `401 Invalid connection token`.
- **Read-only** when scoped `["read"]` — write SQL is rejected by the
  leading-keyword sniff in `cloud/src/handlers/sqlite.rs` (shared by both
  the `/v1/execute` proxy and the `/v2/pipeline` Hrana handler).

Default TTL is 1h; max 24h. The cloud URL the libsql clients hit is
returned in the mint response (`url`), and an unauthenticated UI helper
`GET /sqlite-databases/{id}/connection-info` returns just `{ database_id, url }`.

## Limits & gotchas

- **Region is fixed at create time.** A db in `us-east` can't be moved
  to `eu-west`; recreate + re-import.
- **Row caps.** Default 1000 rows per result, hard cap 10000. The
  response carries `truncated: true` when clamped — paginate with
  `LIMIT`/`OFFSET` for larger queries.
- **Single writer.** No multi-writer; serialize churn at the application
  level if you have many concurrent producers.
- **Cold restore latency.** A db that has been idle long enough to be
  evicted from a backend incurs a one-time "starting" delay (snapshot +
  WAL replay on the new backend) on the first request. Subsequent
  requests are fast.
- **Transactions are scoped to one batch call.** A `BEGIN` in one `exec`
  request and a `COMMIT` in another will not be tied together — they
  hit independent connections. Use `db.batch(..., { transaction })` or
  `POST /sqlite-databases/{id}/exec` with `transaction` set.
- **Tokens leak full DB access** within their scope. Don't paste them
  into shared logs; revoke immediately if exposed.

## When to use which

| Goal | Use |
|------|-----|
| One-off SQL from a shell or CI | `heyvm sqlite exec <id> "..."` |
| Quick poke / schema diagnosis | `heyvm sqlite shell <id>` |
| App code in TS/JS | `Database` from `@heyo/sdk` |
| App code in another language / sustained traffic | `Database.connect()` + libsql client |
| **Editing with sqlite3 CLI, DataGrip, or other vanilla SQLite tools** | `heyvm sqlite checkout <id>` → edit → `heyvm sqlite checkin <id> <path>` |
| UI showing the DB URL | `GET /sqlite-databases/{id}/connection-info` |
| Browser app (no Heyo JWT in the client) | mint a short-lived token server-side, hand the `{ url, authToken }` to the browser |

## Troubleshooting

| Error | Meaning | What to do |
|-------|---------|------------|
| `sqlite is not available in region '<R>'. Cloud regions only.` | The region you passed isn't on the sqlite allow-list. | Run `heyvm sqlite regions`, pick from the result. |
| `Sqlite database is not running` | Cold db + restore in flight, or the backend hosting it bounced. | Retry after a few seconds; if persistent, `heyvm sqlite get <id>` to check `status` / `error_message`. |
| `Unauthorized` on management calls | No / expired Heyo JWT. | `heyvm login`. |
| `Invalid connection token` (401) on `/v1/execute` | Token revoked, expired, or for a different db. | Mint a new one with `db.connect(...)`. |
| `You do not own this sqlite database` (403) | The JWT/account context doesn't match the db's owner. | Confirm `heyvm login` selected the right account; check `account_id` on `heyvm sqlite get`. |
| `truncated: true` on a result | Server clamped rows. | Add `LIMIT` to the query, or paginate with `OFFSET`. |

## Workflow

When the user asks to interact with cloud sqlite:

1. **Login first** if anything 401s (`heyvm-login` skill).
2. **Pick a region** with `heyvm sqlite regions`.
3. **Create or look up** the db (`heyvm sqlite create` / `list` / `get`).
4. **Run SQL** via `heyvm sqlite exec` for one-shots, the SDK for app
   code, or a minted connection token for libsql clients in other
   languages.
5. **Tear down** with `heyvm sqlite delete <id>` when done — managed
   databases keep billing until deleted.

When the user provides `$ARGUMENTS`, interpret them as a subcommand and
arguments to pass directly to `heyvm sqlite`. For example,
`/heyvm sqlite list` should run `heyvm sqlite list`.

If `$ARGUMENTS` is empty, ask the user what they want to do with their
sqlite databases.
