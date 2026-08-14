# Running app-obs under supervisord

`app-obs.conf` in this directory is a [supervisord](http://supervisord.org/)
program definition that runs app-obs as a managed, auto-restarting service.

app-obs is configured entirely by environment variables (there is no config file
and there are no CLI arguments) and runs in the foreground, so supervisord manages
the process directly. `SIGTERM` makes it stop accepting, drain its queue, and
flush buffered records to parquet before exiting.

## One-time host setup

```sh
# Build and install the binary.
cargo build --release
sudo install -m0755 target/release/app-obs /usr/local/bin/app-obs

# Optional: the partition dumper, for inspecting what actually landed on disk.
sudo install -m0755 target/release/dump /usr/local/bin/app-obs-dump

# Dedicated non-root service user. app-obs only writes its data and log dirs and
# makes outbound HTTP to app-lb's admin API.
sudo useradd --system --no-create-home --shell /usr/sbin/nologin app-obs

# Data dir (parquet root) and log dir. The process creates data/ itself at
# startup, but it needs a writable parent to create it in.
sudo install -d -o app-obs -g app-obs /var/lib/app-obs /var/log/app-obs
```

No `setcap` is needed: `9500`, `9514`, and `9600` are all unprivileged.

app-lb does not have to be running first. The metrics poller logs a warning and
retries on the next tick, and log ingest is unaffected — app-obs must never become
a dependency of the data plane, and the reverse holds too.

## Install the unit

```sh
sudo cp deploy/supervisor/app-obs.conf /etc/supervisor/conf.d/app-obs.conf
sudo supervisorctl reread
sudo supervisorctl update
```

Adjust the `environment=` block for your host — at minimum decide about
`APP_OBS_INGEST_TOKEN`, set `APP_OBS_API_TOKEN` before exposing the API listener
directly, and set `APP_LB_PASSWORD` if app-lb runs with `APP_LB_ADMIN_AUTH=1`.

## Operate

```sh
sudo supervisorctl status app-obs
sudo supervisorctl restart app-obs
sudo supervisorctl tail -f app-obs stderr

curl -s localhost:9600/healthz
curl -s localhost:9600/stats     # accepted / dropped counters
```

## Notes

- **A listener that fails to bind does not stop the process.** Unlike app-lb,
  app-obs does not pre-flight its listen addresses: each of ingest, syslog, and
  the API binds inside its own task, and a failure there is logged as an error
  while everything else keeps running. supervisord will report `RUNNING` with, say,
  a dead ingest port. After changing a listen address, confirm with
  `supervisorctl tail app-obs stderr` that all three logged `listening`, or check
  `ss -lntup | grep app-obs`. The only startup condition that exits the process is
  an unusable `APP_OBS_DATA_DIR`.
- **Unset `APP_OBS_INGEST_TOKEN` leaves ingest open.** The ingest listener binds
  `0.0.0.0` because guests reach it on their own tap gateways, so "unreachable
  from outside" is a property of your firewall, not of the bind address. If you do
  set a token, the conf file now holds a secret: `sudo chown root:root` and
  `sudo chmod 0640 /etc/supervisor/conf.d/app-obs.conf`. The same applies to
  `APP_LB_PASSWORD`.
- **`stopwaitsecs` must stay above 30.** On `SIGTERM` app-obs gives itself 30
  seconds to drain the queue and flush open partitions; whatever is still buffered
  when supervisord's patience runs out is lost, because buffered rows exist only in
  memory until a partition is flushed. `35` leaves margin for the flush itself.
- Restarts lose at most the current buffers, not written partitions. Files are
  written under a temporary name and renamed into place, so a reader never sees a
  partial parquet, and a kill mid-write leaves a temp file that `app-obs-dump`
  and the partition reader both skip.
- Log volume is bounded by supervisord's rotation (10MB × 5 per stream), not by
  app-obs. Turn down `RUST_LOG` to `info` alone if the debug lines are noisy —
  per-batch ingest logging is on the `app_obs` target.
- Disk is bounded by `APP_OBS_RETAIN_DAYS` (default 30), counted back from and
  including today. Retention deletes whole partition directories; it never touches
  future-dated ones, so a sender with a skewed clock can grow the data dir past
  what the retention window suggests.
