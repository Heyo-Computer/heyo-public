# queue

A dashboard for the NATS server the fleet dispatches through. A function
runner's work queue, [ci]'s job queue and a control plane's sandbox stream all
end up on one `nats-server`, and this is the page that shows what is happening
on it.

Four things, one page:

- **Queue depth** — every account's streams, what each holds, and how much of
  that is pending to a particular consumer.
- **Throughput** — messages and bytes per second, in and out, charted.
- **Connected clients** — who is attached, from where, with what subscriptions.
- **Logs** — a live tail of what nats-server is saying about all of it.

Runs as a **static (`proxy_pass`) deployment** registered with app-lb — a host
process, not a microVM, fronted the same way app-obs is.

## Why this exists next to the monitoring port it reads

`nats-server` already answers all of this on its HTTP monitoring port, and
`curl | jq` is a real option. Three things make the difference.

**The monitoring port takes no credential.** `/varz`, `/connz` and `/jsz` are
unauthenticated by design, which is why every configuration in this fleet binds
them to loopback. That makes them unreachable from a laptop. This app is a thing
app-lb can put behind Google sign-in, so the answer is available to somebody who
is not already SSH'd into the host.

**`/varz` has no rates.** Its message counters are cumulative since the server
started. A single scrape cannot tell a server saturating a link from one that
has been idle since last Tuesday — throughput is a *difference between two
scrapes*, and something has to hold the first one. That is most of what
`src/state.rs` does.

**Depth alone does not say whether a queue is stuck.** A `WorkQueue` stream
holds zero messages when it is healthy *and* when nothing is subscribed to it at
all, because a message leaves the stream only when it is acked. What separates
those two is whether a consumer's ack floor is moving, which is a join across
two endpoints. The page does that join and says so out loud:

- a stream whose retention deletes on ack and that has **no consumer at all** is
  flagged `no consumer` — nothing will ever drain it;
- a consumer holding work whose **ack floor has not moved** for longer than its
  own redelivery deadline allows is flagged `stalled`, with how long.

Both roll up into a banner, because they are the reason to have this open.

## What it cannot do

It never opens a NATS client connection. There is no code path in this binary
that can publish, subscribe, bind a consumer to a stream, or reach
`$SYS.REQ.SERVER.<id>.SHUTDOWN`. It speaks HTTP GET to a read-only port and
nothing else.

That is a deliberate choice over the alternative. The other way to see every
account at once is a credential in the system account — but a system-account
user can stop the server, so that route means writing a permissions allow-list
and getting it exactly right, and the dashboard's env file becomes a kill switch
for the bus if you don't. The monitoring port gives the same cross-account view
with no credential to leak and no permission to get wrong.

Nothing is persisted, either. History and the log tail live in memory and are
gone on restart. That is the right trade for a live view — [app-obs] is the
thing in this repository that *keeps* logs and metrics, and pointing
nats-server's log file at it is the answer to "what happened last Tuesday".

## The log panel is the one part that needs to be on the host

NATS does not publish its own log. `$SYS` carries connection and account events
and `/varz` carries counters, but the lines that say *why* — a permissions
violation, a slow-consumer disconnect, a stream that failed to restore — exist
only in the log file. So this reads the file, which means:

**Every panel but the log works against a remote monitoring port. The log panel
works only where this app and nats-server share a filesystem.** With
`QUEUE_NATS_LOG_FILE` unset the panel says which variable would turn it on
rather than showing an empty box.

The file is followed, not re-read: at every end-of-file the path is re-checked,
a different inode is treated as a rotation and a file shorter than the read
position as a truncation, both reopening from the start. That matters because
supervisord rotates by renaming, and a tailer that only followed its open handle
would go quiet at the first rotation with no error to notice.

On startup the last 64 KiB is read in so the panel has content immediately — a
healthy NATS can go hours without logging a line.

Lines that do not match nats-server's format (`[pid] date time [LVL] message`)
are kept whole, with no level. They are also never hidden by a severity filter,
because the lines this parser does not recognise are panics and stack traces —
exactly what somebody filtering for "warn and above" is looking for.

## Configuration

Environment variables only — no config file, no CLI arguments, so a supervisor
unit is the single source of truth. See `deploy/supervisor/queue.conf`.

| Variable | Default | What it does |
| --- | --- | --- |
| `QUEUE_NATS_MONITOR_URL` | `http://127.0.0.1:8222` | nats-server's `http:` listener |
| `QUEUE_API_ADDR` | `127.0.0.1:9700` | where the dashboard binds |
| `QUEUE_API_TOKEN` | unset | bearer token for the dashboard and its JSON; unset leaves them open |
| `QUEUE_NATS_LOG_FILE` | unset | nats-server's log file; unset disables the log panel |
| `QUEUE_POLL_SECS` | `5` | scrape interval, and so the resolution of every rate |
| `QUEUE_HISTORY_POINTS` | `720` | samples kept for the charts (an hour at the default poll) |
| `QUEUE_LOG_LINES` | `2000` | log lines held in memory |
| `QUEUE_LOG_PRIME_BYTES` | `65536` | how much of an existing log file to read on startup |
| `QUEUE_MAX_CLIENTS` | `256` | ceiling on the client list pulled from `/connz` |
| `QUEUE_REQUEST_TIMEOUT_SECS` | `4` | deadline on one monitoring request |
| `QUEUE_UI_COOKIE_DOMAIN` | `HEYO_UI_COOKIE_DOMAIN` | parent domain for the shared theme cookie |

Values that would make the process useless are clamped with a warning rather
than refused: a zero poll interval becomes one second, a zero-length history
becomes two points, and a request timeout at or above the poll interval is
capped below it so scrapes cannot overlap. A dashboard is not worth a crash
loop, and every one of those has an obviously correct floor.

## HTTP

| Route | Auth | What it is |
| --- | --- | --- |
| `GET /dashboard` | gated | the page (`/` redirects here) |
| `GET /api/overview` | gated | one scrape of the whole server, plus the history |
| `GET /api/logs` | gated | the log tail — `?since=`, `?level=`, `?q=`, `?limit=` |
| `GET /healthz` | open | for app-lb's health check |
| `GET /__ui/{path}` | open | the shared stylesheet, theme script and fonts |

`/healthz` is deliberately open and deliberately not a NATS check: a probe that
failed because the monitoring port was down would take this deployment out of
rotation for reporting exactly the thing it exists to report.

`/api/logs` is incremental. `?since=<seq>` returns only lines newer than that
sequence, which is how the page polls; `?level=` is a severity *floor* (`warn`
means warn and worse) and `?q=` a case-insensitive substring, both applied
before `?limit=` so "the last 20 errors" means twenty errors rather than however
many of the last twenty lines were errors. An unknown level name is a 400, not
an ignored parameter — answering a filtered request with unfiltered lines is the
wrong answer rather than a missing one.

## Registering with app-lb

`examples/queue.json` is the deployment spec. It is a static upstream, so the
build and update blocks other deployments carry are absent:

```json
{
  "id": "queue",
  "routes": [{ "host": "queue.example.com" }],
  "upstreams": ["127.0.0.1:9700"],
  "health": { "path": "/healthz", "timeout_secs": 2 },
  "auth": {
    "client_id": "REPLACE.apps.googleusercontent.com",
    "client_secret": { "secret": "google", "key": "client_secret" },
    "allowed_domains": ["example.com"],
    "public_paths": ["/healthz", "/__ui/"],
    "cookie_domain": "example.com",
    "forward_identity": true
  }
}
```

`/__ui/` has to be public or the page renders unstyled behind the sign-in
redirect. `cookie_domain` set to the same parent domain as the other apps means
one sign-in covers all of them, and `HEYO_UI_COOKIE_DOMAIN` set to the same
value means one choice of light or dark does too.

`forward_identity` fills the name in the top bar. It is displayed and never
authorized on: this page is read-only, and on an ungated deployment those
headers are whatever the caller sent.

### This page is a disclosure

Behind that gate, it shows **every account's** stream names, subjects, depths
and consumer names, and every connected client's address, name and subscription
list. That is the point — a dashboard that showed only the account it
authenticated as could not answer "is the backlog in the stream or in the
worker?" for anything but itself. But it is worth saying out loud before
pointing it at a server with tenants on it who would not expect that, and it is
why the app warns at startup when `QUEUE_API_TOKEN` is unset and no gate is
implied.

## Building

```sh
cargo build --locked --manifest-path queue/Cargo.toml
cargo test  --locked --manifest-path queue/Cargo.toml
```

Then run it against a local NATS with monitoring on `8222`:

```sh
QUEUE_NATS_LOG_FILE=/var/log/nats/nats-server.log cargo run
```

The monitoring port is the oracle for anything on this page:

```sh
curl -s 'http://127.0.0.1:8222/jsz?accounts=1&streams=1&consumers=1&config=1' | jq
curl -s http://127.0.0.1:9700/api/overview | jq '.accounts[].streams[]'
```

The two should agree on stream names, message counts and consumer gauges. What
they will not agree on is the rates, which exist only here.

[ci]: ../ci
[app-obs]: ../app-obs
