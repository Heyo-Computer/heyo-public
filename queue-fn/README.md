# queue-fn

Serverless functions on [heyvm](https://github.com/heyo/mvm-ctrl) Firecracker
microVMs, with NATS JetStream as the event bus.

Register a function — a VM template plus a command — and events published to
JetStream run it inside a microVM. The fleet sizes itself to the queue: ten
queued events at a target concurrency of two boots five VMs, drains them, and
scales back to zero.

It is a sibling of [app-lb](../app-lb): same conventions, same house style,
different problem. app-lb keeps a pool of VMs warm behind an HTTP data plane;
queue-fn runs a command in one per event.

## Requirements

- **heyvmd** running locally (`http://127.0.0.1:34099` by default). queue-fn
  never talks to the cloud — it needs `guest_ip`, which the daemon only exposes
  for tap-networked Firecracker/KVM backends on a local daemon.
- **NATS with JetStream**: `nats-server -js -sd /var/lib/qfn-js`.
- A **Firecracker image** with a working shell. `heyvm images list` shows what is
  available locally; the spec's `image` must name one of them.

Firecracker and KVM are the only supported drivers. Libvirt is rejected at
registration because it has no `guest_ip`, so a VM would boot and then prove
undrivable.

## Run

```bash
QFN_NATS_URL=nats://127.0.0.1:4222 cargo run
```

| Env var | Default | Meaning |
|---|---|---|
| `QFN_ADMIN_ADDR` | `127.0.0.1:9494` | Admin API and dashboard listener |
| `QFN_STATE_PATH` | `queue-fn-state.json` | Where function specs are persisted |
| `QFN_NAME` | `queue-fn` | Display name in the dashboard header |
| `QFN_DAEMON_URL` | `http://127.0.0.1:34099` | heyvmd base URL |
| `QFN_NATS_URL` | `nats://127.0.0.1:4222` | NATS server; comma-separate for a cluster |
| `QFN_NATS_USER` / `QFN_NATS_PASSWORD` | — | User/password auth; both or neither |
| `QFN_NATS_TOKEN` | — | Token auth |
| `QFN_NATS_CREDS` | — | Path to a `.creds` file (JWT + nkey seed) |
| `QFN_NATS_NKEY` | — | Raw nkey seed |
| `QFN_NATS_SUBJECT_PREFIX` | `qfn` | Subject and stream namespace |
| `QFN_MAX_PAYLOAD_BYTES` | `4096` | Host payload ceiling; a spec may go lower, never higher |
| `QFN_RESULT_RING` | `200` | Recent invocations kept in memory per function |
| `QFN_SYNC_INVOKE_TIMEOUT_SECS` | `180` | How long a sync invoke waits, cold start included |
| `QFN_DASHBOARD_USER` | `admin` | Basic-auth username |
| `QFN_DASHBOARD_PASSWORD` | — | Setting it enables the gate |
| `QFN_ADMIN_AUTH` | `false` | Extend the gate to function CRUD |
| `QFN_INVOKE_AUTH` | `false` | Extend the gate to invoke/enqueue |
| `QFN_CLOUD_OBSERVE` | `true` | Watch heyo cloud's queue on the same NATS |
| `QFN_CLOUD_STREAM` | `HEYO_SANDBOX` | The cloud's stream, for the backlog gauges |
| `QFN_CLOUD_SUBJECT` | `sandbox.>` | Subject the observer subscribes to |
| `QFN_CLOUD_JOB_RING` | `500` | Recent cloud jobs kept in memory |
| `QFN_CLOUD_POLL_SECS` | `5` | How often the cloud's stream is polled |

Setting either auth flag without a password is a startup panic rather than a
silently-open service.

## Pointing at existing NATS infrastructure

queue-fn takes credentials the two ways operators actually supply them, so it
can attach to a NATS that is already deployed — including the one heyo's cloud
service uses, whose `CLOUD_NATS_URL` carries userinfo:

```bash
# In the URL, following the NATS CLI convention (<token>@ or <user>:<pass>@)
QFN_NATS_URL='nats://alice:hunter2@nats.internal:4222' cargo run

# Or as discrete vars, which is preferred — a URL is visible in shell history
# and process listings, and queue-fn warns when the credential arrives that way
QFN_NATS_URL='nats://nats.internal:4222' QFN_NATS_TOKEN=… cargo run
QFN_NATS_URL='tls://nats.internal:4222' QFN_NATS_CREDS=/etc/qfn/nats.creds cargo run

# Clusters: comma-separated, credential taken from the first entry that has one
QFN_NATS_URL='nats://a:4222,nats://b:4222,nats://c:4222'
```

Env vars beat URL userinfo, so a token can be rotated without editing a
deployment's URL. Setting two *kinds* of credential is a startup error rather
than a precedence rule — there is no reading of "token and creds file both set"
that makes one of them the obvious intent.

`tls://` and `wss://` work as-is; scheme detection is async-nats'.

**This is bus compatibility, not protocol compatibility.** queue-fn speaks its
own `qfn.*` subjects on its own streams. It can share a NATS server, an account,
and credentials with heyo's cloud service, but it does not consume cloud's
`HEYO_SANDBOX` stream or its `sandbox.cmd.*` commands — those carry sandbox
*provisioning* requests bound to a Postgres schema and billing entitlements,
which is a different job from running a command in a VM.

It does *watch* them, though, which is a different thing — see below.

## The cloud queue

When queue-fn shares a NATS with heyo's cloud service, the dashboard shows the
cloud's queue alongside its own: enqueued, processing, succeeded, and failed jobs
across `sandbox.cmd.create`, `restart`, `wake`, and the four `sandbox.sqlite.cmd.*`
operations, with the per-consumer backlog, a per-kind breakdown, and a table of
recent jobs. On by default; `QFN_CLOUD_OBSERVE=false` turns it off, and a NATS
with no `HEYO_SANDBOX` on it simply reports the stream as absent.

**Nothing is consumed.** `HEYO_SANDBOX` is `WorkQueue`-retained, which rules out
the obvious approach twice over: a message is deleted when its worker acks it, so
there is no history to read back, and JetStream refuses a second consumer whose
filter overlaps an existing one, so queue-fn could not bind to `sandbox.>` even if
it wanted to — and if it could, it would be stealing the cloud's work rather than
watching it. Two read-only vantage points instead:

- **A core-NATS subscription** to `sandbox.>`. A JetStream publish is an ordinary
  publish that a stream happens to capture, so a plain subscriber gets its own
  copy and consumes nothing. Commands and lifecycle events both cross it, which
  is what makes succeeded/failed knowable.
- **Polling stream and consumer info**, the same management call cloud's own admin
  overview makes. `num_pending` is enqueued-and-undelivered; `num_ack_pending` is
  work a cloud worker is holding right now.

The two have different windows, and the dashboard says so rather than blending
them. The gauges are absolute — they include work enqueued long before queue-fn
started. The outcome counters only cover what crossed the subscription, because a
subscriber never receives a copy of a message published before it subscribed.

**Two commands report no outcome.** `sandbox.cmd.restart` and `sandbox.cmd.wake`
are acked by their workers without publishing anything; only the create paths emit
`evt.ready` / `evt.failed`. Those jobs show as `unreported` rather than being
counted as successes — the bus genuinely does not say how they ended, and a
dashboard that guesses is worse than one that admits the gap.

A wake storm is deduplicated the way the server does it. The proxy publishes many
`sandbox.cmd.wake` commands per 30s bucket on purpose and lets JetStream's
`Nats-Msg-Id` collapse them; queue-fn reads the same header, so the storm is one
job and a `duplicates` counter, not a queue depth the server never had.

## Registering a function

```bash
curl -sX POST localhost:9494/functions -H 'content-type: application/json' -d '{
  "id": "hello",
  "vm": { "driver": "firecracker", "image": "ubuntu",
          "size_class": "small", "ttl_seconds": 3600 },
  "exec": {
    "command": "echo \"got: $(printf %s \"$QFN_PAYLOAD_B64\" | base64 -d)\"",
    "timeout_secs": 20
  },
  "scaling": { "min_replicas": 0, "max_replicas": 5, "target_concurrency": 2,
               "scale_to_zero_after_secs": 120, "cold_start_timeout_secs": 180 }
}'
```

Registration creates the function's JetStream consumer, and **fails** if it
cannot — a function with no consumer would accept events nothing will ever pull.

## The three ways to invoke

All three become the same message on `qfn.invoke.<function>`, pulled by the same
consumer. One dispatch path, one demand signal.

```bash
# Synchronous — blocks through the cold start, returns the result.
curl -sX POST localhost:9494/functions/hello/invoke \
  -H 'content-type: application/json' -d '{"payload":{"hello":"world"}}'

# Asynchronous — returns an invocation id immediately.
curl -sX POST localhost:9494/functions/hello/enqueue \
  -H 'content-type: application/json' -d '{"payload":{"n":1}}'

# Scheduled — declared on the spec.
"triggers": [{ "kind": "interval", "every_secs": 300 },
             { "kind": "daily_at", "times": ["09:00", "17:30"] }]
```

Anything that can publish to the subject works too — queue-fn has no privileged
path of its own:

```bash
nats pub qfn.invoke.hello '{"invocation_id":"abc-1","function_id":"hello",
  "payload":{"n":1},"enqueued_at_ms":0,"source":"invoke"}'
```

## The guest contract

Every invocation receives:

| Env var | Value |
|---|---|
| `QFN_INVOCATION_ID` | Unique per invocation; also the daemon's operation id |
| `QFN_FUNCTION_ID` | The function's id |
| `QFN_ATTEMPT` | Delivery attempt, 1-based |
| `QFN_SOURCE` | `invoke` \| `schedule` \| `replay` |
| `QFN_PAYLOAD_B64` | Base64 of the payload JSON; empty string when there is none |
| `QFN_PAYLOAD_LEN` | Decoded byte length |

Guest side is one line:

```sh
payload=$(printf %s "$QFN_PAYLOAD_B64" | base64 -d)
```

Base64 is about transport, not quoting — the daemon already single-quotes env
values. The value crosses a serial console whose reader frames on newlines, so a
raw payload containing one would be indistinguishable from that framing.

`payload_mode: "template"` additionally substitutes `{{payload}}` and
`{{invocation_id}}` into the command. queue-fn shell-quotes them first, because
after substitution the text is code being handed to `sh -lc`. It is the sharp
option; `env` is the default.

## Admin API

```
GET    /healthz                                     always open
GET    /dashboard | /metrics                        gated by QFN_DASHBOARD_PASSWORD
GET    /cloud[?limit=]                              the cloud queue on its own
POST   /functions                                   register
GET    /functions | /functions/:id
PUT    /functions/:id                               full-spec edit
DELETE /functions/:id                               deregister and tear down
PATCH  /functions/:id/scaling                       partial ScalingPolicy merge
POST   /functions/:id/invoke                        sync, blocks
POST   /functions/:id/enqueue                       async, 202
POST   /functions/:id/pause | /resume
GET    /functions/:id/invocations                   recent results
GET    /functions/:id/dlq | POST …/dlq/replay | DELETE …/dlq
DELETE /functions/:id/vms/:sandbox_id[?force=true]  evict one VM
```

Editing the `vm` block rebuilds the pool; exec and scaling edits keep it live.

## Dashboard

`http://127.0.0.1:9494/dashboard`. One self-contained page, no external fetches,
polling `metrics` every 2s. Fleet tiles, queue-depth and throughput sparklines,
exec/queue-wait/cold-start histograms, and per-function cards with a VM table and
a recent-invocations table. Plus the cloud queue when there is one, which is a
read-only panel — it renders somebody else's queue and offers no controls over it.

Manual controls: invoke (with a payload editor and a sync/async toggle), scale,
edit the spec, pause and resume the consumer, drain or force-kill a VM, and
inspect, replay, or purge the dead-letter queue.

Rates are derived client-side by diffing cumulative counters — the server only
ever exports monotonic totals, plus queue depth, which is a genuine gauge.

## Scaling

```
demand  = queued + max(delivered-unacked, running)
desired = clamp(ceil(demand / target_concurrency) + warm_pool, min, max)
```

`target_concurrency` is **not parallelism**. heyvm serialises exec per sandbox —
it removes the handle for the duration of a call, and a concurrent second exec
gets `SandboxNotFound` rather than queueing — so a VM runs exactly one invocation
at a time. The knob is how deep a backlog queue-fn will line up behind one VM.
Ten queued events at a target of two still means five VMs; each runs its two
back to back.

Counting *delivered-unacked* work as demand is load-bearing. A message the
dispatcher pulled and nak'd is no longer `pending`, so counting only `pending`
lets a function whose whole backlog has been delivered report zero demand — the
autoscaler boots nothing, and the work can never run because nothing can run it.

## Design notes

Verified against the dependencies' source rather than their docs, and in several
cases against a running system, because the docs were wrong.

**The daemon reports `completed`, not `succeeded`.** Terminal exec-operation
statuses are `completed` and `failed` (`mvm-ctrl/src/api.rs:5226,5231`). Matching
a positive list containing `succeeded` made every invocation poll a finished
operation until it hit its own timeout. `is_finished` therefore checks for *not
pending*, so an unfamiliar status surfaces as an error instead of a hang.

**Never pull a message you cannot place.** A nak is a delivery and counts against
`max_deliver`, so pulling-and-naking while waiting for a cold start burns the
retry budget and JetStream discards the work before it ever runs. The dispatcher
fetches only when a VM has a free slot; until then the work stays undelivered,
where `num_pending` shows it to the autoscaler.

**async-nats parses URL credentials and then discards them.** Its connector
reads auth only from `ConnectOptions`; userinfo survives into
`ServerAddr::username`/`password` and nothing sends it. So `nats://token@host`
resolves, dials, completes the TCP handshake, and fails with "Authorization
Violation" — a failure that names the server, not the mistake. `nats_auth`
splits userinfo off the URL and re-threads it through `ConnectOptions`, which is
also why the sanitized address is the only one that reaches a log line. heyo's
cloud service carries the same workaround for the same reason
(`cloud/src/services/nats_auth.rs`).

**A comma-separated cluster URL has to be split by hand.** `ToServerAddrs for
str` parses exactly one address (`async-nats-0.47.0/src/lib.rs:1683`); unlike the
NATS CLI it does not split on commas, so passing `a:4222,b:4222` through whole is
a parse error rather than a failover list.

**`WorkQueue` retention makes the message count the backlog**, which is exactly
the autoscaler's input and self-clearing on ack. Its constraint is one consumer
per subject, so queue-fn is single-instance. app-lb is likewise a single writer.

**Async exec for idempotency, not duration.** `start_exec_operation` returns the
existing record for a known operation id instead of running again, so a crash
between "the command finished" and "the message was acked" replays into a result
lookup rather than a second execution.

**stderr is always empty on Firecracker.** The serial exec path runs
`(cmd) 2>&1` and builds its response with an empty stderr
(`mvm-ctrl/src/driver/firecracker.rs:1752`), so both streams arrive folded into
stdout. The sync invoke response carries a note saying so, because silently
returning `""` reads as "the function wrote nothing to stderr".

**Function timeouts are capped at 25s.** That same path hard-codes a 30s
`timeout()` no request field can raise (`firecracker.rs:1716`). `validate`
rejects anything longer rather than accepting a number the guest will ignore.
Raise `MAX_TIMEOUT_SECS` once the daemon plumbs a per-request timeout through.

**Replica names carry a hex nonce.** The daemon derives a VM's tap subnet with
`from_str_radix(name_suffix, 16).unwrap_or(0)`, so a non-hex name collapses every
VM onto `172.16.0.2`, where they collide.

**No pingora.** app-lb runs axum inside a pingora `BackgroundService` because
pingora owns its runtime. queue-fn has no data plane, so it owns its own:
`#[tokio::main]`, spawned workers, and a `watch` channel in place of pingora's
`ShutdownWatch` — the same type underneath, so the select-loop shapes port over
unchanged.

## Tests

188 inline tests, `cargo test`. No integration harness: the paths that need a
real daemon and a real bus are covered by the end-to-end run above. The cloud
observer's state machine takes subjects and payloads as arguments, so its whole
classification table is tested without a bus.
