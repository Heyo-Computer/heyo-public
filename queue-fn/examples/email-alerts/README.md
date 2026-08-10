# email-alerts

A queue-fn function that turns one signed webhook into email to whoever the
routing table says should get it. The VM template is a Firecracker image with
`python3` and one script; the configuration — SMTP credentials, who is on call —
lives in [HeyoSecret](../../../heyo-public/heyosecret) and is read at invocation
time, so changing the on-call rotation is a secret write rather than a redeploy.

```
provider ──signed POST──▶ relay ──enqueue──▶ NATS JetStream ──▶ microVM
                            │                                     │
                            └──── HMAC key ────┐        ┌─── SMTP creds
                                               ▼        ▼    + routing table
                                            HeyoSecret ◀┘
```

Three fan-outs, at three different layers, which is the reason the example is
shaped this way:

- An Alertmanager batch of five alerts becomes **five enqueues**, so each one
  routes and scales independently instead of sharing a 25-second exec budget.
- The queue **fans out to VMs**: `desired = ceil(demand / target_concurrency)`,
  so a burst of twenty alerts boots ten microVMs and drains them.
- Each invocation **fans out to recipients**, one SMTP message per address.

## Files

| Path | Runs on | What it is |
|---|---|---|
| `guest/fanout.py` | the microVM | the function itself; stdlib Python, baked into the image |
| `guest/install.sh` | the image build | copies the script to `/opt/email-alerts/` |
| `image/Dockerfile` | the image build | the Firecracker rootfs |
| `image/init.sh` | the microVM | PID 1 — mounts, network, sshd, `HEYVM_READY` |
| `image/preflight.sh` | the microVM | in-VM check that the image can run the function |
| `image/resolv.conf` | the microVM | the guest's resolver, copied into place at boot |
| `build-image.sh` | host | `heyvm mvm build` with the right context |
| `function.json` | — | the queue-fn spec |
| `register.sh` | host | substitutes the HeyoSecret token and registers the function |
| `seed-secrets.sh` | host | writes the SMTP config, routing table, and webhook key |
| `relay/webhook-relay.py` | host | webhook ingress: verify, normalize, enqueue |
| `test/*.py` | anywhere | end-to-end tests with fake SMTP / secret / queue-fn servers |

## Setup

Assumes the [queue-fn requirements](../../README.md#requirements) are met —
heyvmd local, `nats-server -js`, and a Firecracker image — plus a running
HeyoSecret.

**1. Build the image.** queue-fn has no `setup_hooks` on purpose: a function that
installs itself at cold start turns every scale-from-zero into a network
install, which is the slowest possible moment to find out a mirror is down. The
function is baked in instead.

```sh
./build-image.sh                       # -> ~/.heyo/images/firecracker/email-alerts.ext4
DNS_SERVER=10.0.0.53 ./build-image.sh  # if your SMTP relay needs an internal resolver
```

Needs `docker`, `mke2fs` (e2fsprogs), and `fakeroot` unless you build as root —
`heyvm mvm build` runs `docker build`, then `docker export`, then `mke2fs -d`
over the exported tree. The wrapper exists because the Dockerfile lives in
`image/` but COPYs from `guest/`, so the build context has to be the example
root; heyvm would default it to the Dockerfile's own directory and fail on the
first COPY.

Then set `vm.image` in `function.json` to the image name (`email-alerts` by
default), and check it from inside a VM before trusting it with a page:

```sh
heyvm create --backend-type firecracker --image email-alerts --name preflight
heyvm exec preflight /opt/email-alerts/preflight.sh
heyvm rm -y preflight
```

See [Inside the image](#inside-the-image) for what the Dockerfile has to do that
a normal one does not.

**2. Write the configuration into HeyoSecret.**

```sh
export HEYOSECRET_TOKEN=…                   # the internal API key
SMTP_PASSWORD=… SMTP_HOST=smtp.example.com \
  ONCALL=oncall@example.com,pager@example.com \
  SRE=sre@example.com \
  ./seed-secrets.sh
```

That writes three secrets:

| Path | Contents |
|---|---|
| `alerts/smtp/relay` | host, port, TLS mode, username, password, From |
| `alerts/routing/email-alerts` | the rules that decide who gets what |
| `alerts/webhook/hmac` | 32 random bytes, generated server-side, for the relay |

**3. Register the function.**

```sh
HEYOSECRET_TOKEN=… ./register.sh
```

Re-running it `PUT`s rather than `POST`s, which keeps the running pool alive
whenever the `vm` block is unchanged — so rotating the token is a live edit, not
a cold start for every replica.

**4. Start the relay** and point your provider at it.

```sh
HEYOSECRET_TOKEN=… ./relay/webhook-relay.py       # 127.0.0.1:9595
```

## Try it without a webhook

```sh
curl -sX POST localhost:9494/functions/email-alerts/invoke \
  -H 'content-type: application/json' -d '{"payload":{
    "id": "disk-full-db01",
    "severity": "critical",
    "title": "Disk 94% on db-01",
    "body": "Root filesystem crossed the alert threshold.",
    "source": "prometheus",
    "labels": {"env": "prod", "service": "db"},
    "url": "https://runbooks.internal/disk-full"
  }}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["stdout"])'
```

The function writes exactly one line of JSON, which arrives in the sync invoke
response's `stdout`:

```json
{"invocation_id":"…","attempt":1,"status":"sent",
 "delivered":["oncall@example.com","pager@example.com"],
 "undelivered":[],"rejected":{},"matched_rules":["pager"],"resolved":2,
 "duration_ms":840,"log":[{"level":"info","event":"delivered","to":"…"}]}
```

One line because the whole result is one object — outcome, per-recipient detail,
and diagnostics belong together in the single `stdout` string the dashboard
shows. `stderr` is always empty on this backend: the Firecracker exec path runs
`(cmd) 2>&1` and folds both streams into stdout, which is why diagnostics go in
the `log` array rather than to stderr.

## The alert envelope

Small on purpose. queue-fn caps a payload at 4096 bytes because it crosses a
serial console as one command line, and Linux's tty line discipline buffers
exactly that much — an oversized payload would be truncated into something that
looks like corruption rather than a transport failure. `title` or `body` is
required; everything else is optional.

```json
{"id": "…", "severity": "critical", "title": "…", "body": "…",
 "source": "prometheus", "labels": {"env": "prod"},
 "url": "https://…", "ts": 1753600000}
```

## The routing table

Stored at `alerts/routing/email-alerts`. **Every matching rule contributes** —
they union rather than first-match, because a critical production alert should
reach both the pager and the owning team, and expressing that with first-match
means writing the cross product by hand. `default` applies only when nothing
matched at all.

```json
{
  "rules": [
    {"name": "pager", "match": {"severity": ["critical", "page"]},
     "to": ["oncall@example.com"]},
    {"name": "prod-db", "match": {"labels": {"env": "prod", "service": "db"}},
     "to": ["dba@example.com"]},
    {"name": "catch-all-firing", "match": {}, "to": ["alerts@example.com"]}
  ],
  "default": ["alerts-catchall@example.com"],
  "max_recipients": 25,
  "retry_undelivered": "never",
  "unrouted": "drop"
}
```

`match` accepts `severity` and `source` (a string or a list) and `labels` (every
key must match). An empty `match` matches everything.

## Retries, duplicates, and the choice in between

queue-fn maps any non-zero exit to `Failed`: the message is nak'd, JetStream
redelivers it up to `retry.max_attempts` with a backoff ladder, and after that it
lands in the DLQ. So the only question the function has to answer is *would
running again help?* — and for something with a side effect as visible as email,
that question has a real cost on both sides.

| exit | status | when | why |
|---|---|---|---|
| 0 | `sent` | everyone took it | — |
| 0 | `sent_with_rejections` | some address is permanently invalid (5xx) | the server will say the same tomorrow |
| 0 | `partial` | some delivered, some still pending at the deadline | retrying re-mails the ones who already got it |
| 0 | `no_recipients` | nothing matched, no default | a retry cannot invent a route (65 under `"unrouted": "fail"`) |
| 65 | `bad_payload` | not an alert envelope | the DLQ is exactly where a poison message belongs |
| 69 | `secrets_error` | HeyoSecret unreachable | nothing was sent, so a retry is clean |
| 75 | `no_delivery` | SMTP broken, **nobody** got mail | nothing was sent, so a retry is clean |
| 78 | `config_error` | auth rejected, sender refused | a human has to fix it; the DLQ makes that visible |

Two of those are deliberate choices rather than obvious answers:

**`partial` exits zero.** A redelivery that lands on a different VM has no
memory of what the first attempt sent, so retrying a half-finished fan-out
re-mails everyone who already received the alert. The default accepts a gap and
makes it loud — the undelivered addresses are named in the summary, which is in
the dashboard's recent-invocations table. Set `"retry_undelivered": "always"` in
the routing secret if a duplicate page is better than a missed one. It is a real
trade and it belongs to whoever is carrying the pager, not to this script.

**`no_recipients` exits zero.** Retrying cannot invent a route, and DLQ'ing every
unrouted alert fills the DLQ with things that will never be deliverable. Set
`"unrouted": "fail"` to send them to the DLQ instead, which is right when a
missed alert is worse than a queue that needs draining. A catch-all rule is
usually the better answer than either.

**Delivery is at-least-once**, and there is no way around that on this
architecture. What the function does instead is make a duplicate *recognisable*:
`Message-ID` is derived from the invocation id and the recipient, so a
redelivered event produces a byte-identical id that most mail systems collapse.
A DLQ replay mints a fresh invocation id — queue-fn does this deliberately, since
reusing it would hit the daemon's existing `operationId` record and return the
old failure without running anything — so a replay is genuinely new mail and
threads with the original rather than being suppressed.

One case stays ambiguous no matter what: if the connection drops after `DATA`
but before the `250`, the message may or may not have been queued upstream, and
the function counts it undelivered and retries. That is the correct reading of an
unacknowledged send, and it is why `Message-ID` stability matters.

## Where the credential lives

The function spec carries exactly one secret: `ALERT_SECRETS_TOKEN`, the
HeyoSecret bearer key. Everything else — SMTP password, recipient lists —
is read from the secret store at invocation time.

That split is the whole point, because **a queue-fn spec is not a secret store**.
It is persisted to `queue-fn-state.json` and returned verbatim by
`GET /functions/:id`, so anything in `exec.env` is readable by anyone who can
reach the admin API or the file. One narrow, rotatable token there is a much
smaller blast radius than an SMTP password, and rotating it is a `PUT` that
keeps the pool warm. Concretely:

- run queue-fn with `QFN_ADMIN_AUTH=1` and `QFN_DASHBOARD_PASSWORD` set;
- `chmod 600 queue-fn-state.json`;
- give the token a HeyoSecret identity scoped to `alerts/*` if your deployment
  enforces `read_access` (HeyoSecret stores those lists but does **not** enforce
  them today, so treat the token as having full read access until it does).

Rotating anything else needs no queue-fn involvement at all: write a new version
in HeyoSecret and a running VM picks it up within `ALERT_CACHE_TTL_SECS`.

## Inside the image

`heyvm mvm build` is not `docker run`. It runs `docker build`, then
`docker create` + `docker export`, then `mke2fs -d` over the exported tree — and
the microVM boots that filesystem directly with `init=/init.sh` on the kernel
command line. Five consequences, each of which is a way the image silently
fails if you skip it:

**Only the filesystem survives.** `docker export` writes a tar of the container's
files and nothing else. `ENV`, `CMD`, `ENTRYPOINT`, `WORKDIR`, and `USER` are
image *config* and are dropped. Nothing in the guest may depend on them — the
`CMD ["/init.sh"]` at the bottom of the Dockerfile is documentation, and the real
entry point is the kernel parameter. It is also why the resolver is a COPY'd
file rather than an `ARG`: `heyvm mvm build` passes no `--build-arg` through to
docker, so an ARG would silently keep its default no matter what you set.

**`/init.sh` is PID 1, and it must print `HEYVM_READY`.** There is no systemd and
no cloud-init. `/proc`, `/sys`, `/dev`, and `/dev/pts` do not exist at boot; a
Docker-exported rootfs has an empty `/dev`, so sshd and Python's `ssl` module
both fail without the mounts. The host watches the serial console for that exact
marker before considering the VM ready.

**The serial console is the exec channel.** heyvmd drives commands by writing
`echo <START>; (cmd) 2>&1; echo <END> $?` into a shell reading from ttyS0 and
collecting the lines between the markers — that is how queue-fn's dispatcher
invokes the function. Two things follow: background services must redirect their
output (stray writes land in the middle of the protocol, which is why sshd gets
`2>/tmp/sshd.log`), and the shell runs in a `while :;` loop rather than `exec`.
With `exec`, one command that calls `exit` would leave the VM alive with no way
to run anything in it, and the pool would keep dispatching into a black hole
until the TTL reaped it.

**`/dev/shm` has to be mounted, and it is not optional.** `fanout.py` caches
fetched secrets there specifically because it is tmpfs — a decrypted SMTP
password must never reach the rootfs image. Without the mount the cache still
works, writing happily to the underlying directory on disk, which is the failure
you would least like to discover later.

**`ca-certificates` is load-bearing.** `ssl.create_default_context()` verifies the
relay's certificate against the system trust store. Without the package,
STARTTLS fails at handshake on every invocation — and fails identically on every
retry, so the DLQ fills with something that looks like an SMTP outage.

The Dockerfile asserts the ones it can at build time (`/init.sh` executable, the
CA bundle non-empty, `ssl.create_default_context()` succeeding), and
`image/preflight.sh` checks the rest from inside a running VM, including the
default route by calling `fanout.py`'s own `default_gateway()` rather than a
second implementation of it.

Note that nothing is left resident. The function is a command, not a daemon:
queue-fn runs `python3 /opt/email-alerts/fanout.py` per invocation, and an idle
VM costs only its memory.

## Reaching the host from the guest

heyvm gives each VM its own tap subnet — the daemon derives it from the hex
suffix of the replica name — so the host's address *as the guest sees it* is not
a constant and cannot be hard-coded. `fanout.py` reads its default route from
`/proc/net/route` and talks to `http://<gateway>:ALERT_SECRETS_PORT`.

If HeyoSecret is somewhere else on the network, skip all of that:

```sh
SECRETS_URL=https://secrets.internal:4455 HEYOSECRET_TOKEN=… ./register.sh
```

## Tuning

Spec knobs beyond the [usual scaling ones](../../README.md#scaling):

| `exec.env` key | Default | Meaning |
|---|---|---|
| `ALERT_SECRETS_URL` | — | Full HeyoSecret URL; overrides gateway detection |
| `ALERT_SECRETS_PORT` | `4455` | Port on the derived gateway |
| `ALERT_SMTP_SECRET` | `alerts/smtp/relay` | Secret path for the relay config |
| `ALERT_ROUTING_SECRET` | `alerts/routing/email-alerts` | Secret path for the routing table |
| `ALERT_DEADLINE_SECS` | `20` | Budget for the whole invocation |
| `ALERT_CACHE_TTL_SECS` | `60` | Secret cache lifetime inside a warm VM |

**`ALERT_DEADLINE_SECS` must stay under `exec.timeout_secs`,** which queue-fn
already caps at 25 because the Firecracker serial exec path hard-codes a 30s
`timeout()` no request field can raise. A process killed by that timeout prints
nothing at all, so the budget exists to guarantee the summary gets out. 20 under
a 25s timeout leaves room for the summary and the daemon's own bookkeeping.

**`ALERT_CACHE_TTL_SECS` trades propagation delay for load.** A VM warm through a
burst would otherwise read the same two secrets once per alert. The cache lives
in `/dev/shm` at mode 0600 — tmpfs, so a decrypted value never reaches the VM's
disk image, and it dies with the VM. Set it to `0` to read every time.

**`warm_pool: 1` in the shipped spec pins one VM permanently** and, because
`desired_replicas` only scales to zero when `min_replicas` *and* `warm_pool` are
both zero, it also makes `scale_to_zero_after_secs` inert. That is deliberate for
alerting: a cold start is up to `cold_start_timeout_secs` (180s here), which is a
long time to sit on a page. Set `warm_pool: 0` if you would rather pay the cold
start than keep a VM alive.

## Tests

```sh
python3 test/fanout_test.py     # routing, retries, exit codes, output shape
python3 test/relay_test.py      # signatures, normalization, the 4096-byte ceiling
```

Both spin up fake SMTP / HeyoSecret / queue-fn servers on loopback and run the
real scripts against them — no VM, no NATS, no mail. That covers the parts you
will actually edit.

To run the function suite against the guest's own interpreter and trust store
rather than your host's, use the image as a container:

```sh
docker build -t email-alerts-verify -f image/Dockerfile .
docker run --rm email-alerts-verify /opt/email-alerts/preflight.sh
docker run --rm -v "$PWD:/work:ro" -w /work email-alerts-verify python3 test/fanout_test.py
```

What none of this covers is the serial-console exec path, the `init=/init.sh`
boot, and JetStream redelivery — a container shares the host kernel and never
runs `init.sh`. Those need a real VM: `heyvm create … && heyvm exec …
/opt/email-alerts/preflight.sh`, and then the end-to-end invoke above.

## Adapting it

**A different provider.** Edit `normalize()` in `relay/webhook-relay.py` — it is
the only provider-aware code, and it returns a list so a batch becomes several
invocations. Set `RELAY_SIGNATURE_HEADER` to whatever your provider signs with
(`X-Hub-Signature-256` for GitHub). If a routing rule keys on a label outside the
default `RELAY_KEEP_LABELS` list, add it, or a chatty alert can push it past the
trim.

**A different channel.** Slack, PagerDuty, and SMS are the same shape: swap
`send_all()` for the API call and keep the routing table, the exit-code contract,
and the secret layout. The interesting part of this example is not SMTP.

**Per-recipient retry precision.** If `partial` is unacceptable and duplicates
are too, split it in two: a router function that resolves recipients and enqueues
one `email-send` invocation per address, and a sender function that delivers to
exactly one. Each invocation then has a single side effect, so a retry is precise
rather than a re-broadcast — at the cost of one more function, N times the
invocations, and a router whose own retry re-enqueues. Worth it when the
recipient list is long; overkill for a handful of addresses.
