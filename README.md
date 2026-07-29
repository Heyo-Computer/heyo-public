# app-obs

Logs and metrics for the deployments [app-lb](../app-lb) manages. Collects logs
pushed by applications, polls app-lb for metrics, stores both as partitioned
parquet, and ages partitions out.

Runs as a **static (`proxy_pass`) deployment** registered with app-lb — it is a
host process, not a microVM, so it is fronted the same way the pg-fc dashboard
is.

> **Status: phase 1.** Collection, storage, and retention-by-deletion work. The
> query API, dashboard, S3 tiering, and webhook alerts are not built yet.

## Why push, not pull

The obvious source for VM logs — the daemon's `GET /sandboxes/:id/logs` — cannot
be used. Its store is written from exactly one place, the output of explicit
`execute_command` calls, so an application started through app-lb's
`start_command` writes to a file inside the guest that the daemon never sees. It
is also capped at 1000 in-memory entries per sandbox and discarded when the
sandbox stops. Polling it would additionally mean exec'ing into live VMs.

So applications push to app-obs, and metrics are polled from app-lb, which
already measures everything worth keeping.

## Where guests send logs

Every microVM sits on its **own /30**: the host is at `guest_ip - 1`. There is no
single address that works for every guest, so guests send to their **default
gateway**, which is always this host on their subnet. app-obs binds `0.0.0.0` so
it is reachable on all of them.

```sh
# inside a guest — the gateway is the collector
GW=$(ip route | awk '/^default/ {print $3}')
curl -XPOST "http://$GW:9500/ingest" \
  -H 'content-type: application/json' \
  -H "authorization: Bearer $APP_OBS_INGEST_TOKEN" \
  -d '{
    "deployment": "demo",
    "backend": "sb-abc123",
    "records": [
      {"ts": 1785260096000, "level": "info", "message": "started"},
      {"level": "error", "message": "boom", "fields": {"request_id": "r1"}}
    ]
  }'
```

`deployment`, `backend`, and `host` set at the batch level apply to every
record that doesn't state its own. `backend` identifies which instance produced
the line — a sandbox id for a VM deployment, a `host:port` for a static upstream;
`sandbox_id` is accepted as an alias since a guest knows itself by that name.

`ts` accepts epoch milliseconds, epoch seconds, or RFC 3339, and defaults to arrival time — a shipper that doesn't know
the time is better than a dropped line. A record with no resolvable
`deployment` is rejected; retrying will not help.

Syslog works too, for senders that can't be taught JSON:

```sh
logger -n "$GW" -P 9514 -t demo "hello from syslog"
```

The syslog tag becomes the deployment when it is a usable id; otherwise records
land under `syslog`.

### Dropping is deliberate

Everything funnels through one bounded queue. When it is full, records are
dropped and counted — never blocked. A customer's application must not stall
because the collector fell behind. `GET /stats` on the API port reports
`accepted` and `dropped`.

## Storage

Hive-partitioned parquet, so a query filtered by deployment and day reads only
those directories:

```
data/logs/deployment=<id>/date=YYYY-MM-DD/hour=HH/<ts>-<seq>.parquet
data/metrics/deployment=<id>/date=YYYY-MM-DD/<ts>-<seq>.parquet
```

Logs partition hourly because their volume is unbounded and bursty; metrics
partition daily because they arrive at a fixed poll rate and hourly directories
would just multiply tiny files. `deployment`, `date`, and `hour` live in the
path, not inside the files. Files are written under a temporary name and renamed
into place, so a reader never sees a partial parquet.

Whole-host CPU and memory land under the reserved deployment id `_host`, since
they belong to no deployment.

Metrics report **p50/p90/p99** — the percentiles app-lb's histogram actually
measures. There is no p95; interpolating one would invent a number the source
never produced. A metric that was not reported stays null rather than becoming
zero, because "not measured" and "measured as zero" mean different things on a
chart.

## Configuration

To run it as a managed, auto-restarting service, see the supervisord unit in
[`deploy/supervisor/`](deploy/supervisor/).

Configuration is environment-only:

| Variable | Default | Meaning |
| --- | --- | --- |
| `APP_OBS_DATA_DIR` | `/var/lib/app-obs/data` | Parquet root |
| `APP_OBS_INGEST_ADDR` | `0.0.0.0:9500` | HTTP ingest; must be reachable from every tap gateway |
| `APP_OBS_SYSLOG_ADDR` | `0.0.0.0:9514` | Syslog, UDP and TCP |
| `APP_OBS_API_ADDR` | `127.0.0.1:9600` | Status API; the dashboard will live here |
| `APP_OBS_INGEST_TOKEN` | *(unset)* | Bearer token for `/ingest`; **unset leaves ingest open** |
| `APP_LB_URL` | `http://127.0.0.1:9090` | app-lb admin API to poll |
| `APP_LB_USER` | `admin` | Only used when a password is set |
| `APP_LB_PASSWORD` | *(unset)* | Set when app-lb has `APP_LB_ADMIN_AUTH=1` |
| `APP_OBS_POLL_SECS` | `10` | Metrics poll interval |
| `APP_OBS_RETAIN_DAYS` | `30` | Partitions older than this are deleted |
| `APP_OBS_FLUSH_ROWS` | `10000` | Flush a partition at this many buffered rows... |
| `APP_OBS_FLUSH_SECS` | `60` | ...or this long after its first row |
| `APP_OBS_QUEUE_CAPACITY` | `65536` | Ingest queue depth before records are dropped |

`APP_OBS_RETAIN_DAYS` counts back from and including today: `7` keeps seven
days, and the eighth-oldest day is removed. Future-dated partitions are never
deleted — a sender with a skewed clock shouldn't have its data destroyed before
anyone notices the skew.

## Registering with app-lb

```sh
curl -XPOST localhost:9090/deployments \
  -H 'content-type: application/json' \
  -d @examples/app-obs.json
```

Edit the `host` route to a real DNS name first. With ACME configured on app-lb,
its certificate is then issued automatically like any other deployment.

Keep `APP_OBS_API_ADDR` on loopback so the only external path to it is through
app-lb, and note that the **ingest** listener is deliberately not fronted by
app-lb — guests reach it directly on the tap network.

## Building

```sh
cargo build --release
cargo test
```

app-obs must never become a dependency of the data plane: if it is down, app-lb
keeps serving traffic, and guests shipping to a dead ingest port get a connection
error rather than blocking.
