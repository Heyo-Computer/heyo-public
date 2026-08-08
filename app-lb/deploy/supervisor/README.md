# Running app-lb under supervisord

`app-lb.conf` in this directory is a [supervisord](http://supervisord.org/)
program definition that runs app-lb as a managed, auto-restarting service.

app-lb is configured entirely by `APP_LB_*` environment variables (there are no
CLI arguments) and runs in the foreground — `Server::new(None)` does not
daemonize — so supervisord manages the process directly. `SIGTERM` triggers
Pingora's graceful shutdown.

## One-time host setup

```sh
# Build dependency: app-lb terminates TLS with openssl (pingora only supports
# per-SNI certificate callbacks there, which the ACME feature needs).
sudo apt-get install -y libssl-dev pkg-config

# Build and install the binary.
cargo build --release
sudo install -m0755 target/release/app-lb /usr/local/bin/app-lb

# Dedicated non-root service user. app-lb talks to heyvmd over HTTP and never
# touches /dev/kvm itself, so this user does NOT need the kvm group — on a host
# that does not build images it only needs to write its data/log dirs and reach
# the daemon URL.
sudo useradd --system --no-create-home --shell /usr/sbin/nologin app-lb

# ONLY on a host that runs image builds (a deployment with a `build` block).
# `heyvm mvm build` shells out to docker, whose socket is root:docker 0660, so
# without this the first build fails with "permission denied while trying to
# connect to the Docker daemon socket". The group is root-equivalent — that
# socket starts containers as root on request — so skip it on hosts that only
# pull artifacts or serve static deployments.
sudo usermod -aG docker app-lb

# ALSO only on a build host. `--no-create-home` above suppresses the mkdir but
# NOT the passwd field, which useradd still fills in from /etc/default/useradd
# (HOME=/home) as /home/app-lb. supervisord never sets HOME, and the docker CLI
# falls back to reading /etc/passwd when it is unset — so docker finds that
# never-created path, tries to make its state dir under it, and dies before
# reading a line of the Dockerfile: "Docker build failed: ERROR: mkdir
# /home/app-lb: permission denied" (/home is root-owned). Give it a real home.
sudo install -d -o app-lb -g app-lb -m0755 /home/app-lb

# Data dir (persisted deployment state) and log dir.
sudo install -d -o app-lb -g app-lb /var/lib/app-lb /var/log/app-lb

# ACME storage: holds the account key and issued private keys, so 0700.
sudo install -d -m0700 -o app-lb -g app-lb /var/lib/app-lb/acme

# Only if using ACME or binding :80/:443 as the non-root user.
sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/app-lb
```

Requirements (see the top-level `README.md`): a running `heyvmd` daemon
(default `http://127.0.0.1:34099`) and, for VM deployments, KVM on the host.
`heyvmd` must be running before app-lb can boot or adopt VMs.

## Install the unit

```sh
sudo cp deploy/supervisor/app-lb.conf /etc/supervisor/conf.d/app-lb.conf
sudo supervisorctl reread
sudo supervisorctl update
```

Adjust the `environment=` block in the conf for your host (dashboard auth, TLS,
listen addresses). `APP_LB_STATE_PATH` is pinned to an absolute path under
`/var/lib/app-lb` so restored deployments are found regardless of the working
directory.

## Operate

```sh
sudo supervisorctl status app-lb
sudo supervisorctl restart app-lb
sudo supervisorctl tail -f app-lb stderr
```

## Notes

- **`HOME` does double duty on a build host, and both users have a claim on
  it.** `docker` keeps its client state in `$HOME/.docker`, so `HOME` has to be
  a directory app-lb can write or no build starts at all. `heyvm mvm build` then
  saves the finished rootfs to `$HOME/.heyo/images/firecracker/<name>.ext4`, and
  heyvmd only resolves image names under *its own* home — so a `HOME` that
  satisfies only docker gives you the second failure instead of the first: the
  build succeeds and nothing boots. Settle it one of two ways:
  - `APP_LB_HEYVM_HOME=<heyvmd's home>` — one variable covers both, but only
    when app-lb can write that directory. Check with
    `getent passwd $(ps -o user= -C heyvmd | head -1)`.
  - `MVM_DATA_DIR=<shared dir>` set on **both** app-lb and heyvmd, which takes
    image placement out of `HOME` entirely and leaves `/home/app-lb` doing
    nothing but holding `.docker`. Point `APP_LB_IMAGES_DIR` at
    `<shared dir>/images/firecracker` so artifact pulls land there too.
- **A new group needs a program restart, not a config reload.** supervisord
  resolves the service user's group list when it forks the child, so after
  `usermod -aG docker app-lb` run `supervisorctl restart app-lb`. `reread` and
  `update` alone leave the already-running process without the group, and builds
  keep failing on the docker socket with nothing in the config to explain it.
- A startup misconfiguration causes an immediate panic-exit: setting only one of
  `APP_LB_TLS_CERT` / `APP_LB_TLS_KEY`, `APP_LB_ADMIN_AUTH=1` without
  `APP_LB_DASHBOARD_PASSWORD`, or a listen address that cannot be bound.
  `startretries=3` bounds the resulting crash-loop, after which supervisord marks
  the program `FATAL` — check `supervisorctl tail app-lb stderr` for the message.
- **Listen addresses are pre-flighted at startup**, deliberately. Pingora binds
  inside a service task and a failure there panics only *that task*: the process
  survives with a dead proxy, the admin API still answering, and supervisord
  reporting `RUNNING`. app-lb binds each address up front so that becomes a clean
  startup failure naming the port and the cause instead of a half-dead service.
- The default listeners (`6188`, `6189`, `9090`) are unprivileged, so no root is
  needed. If you rebind the proxy to `:80`/`:443`, you must run as root or grant
  `CAP_NET_BIND_SERVICE`.
- **Automatic certificates need port 80.** Let's Encrypt validates HTTP-01 there
  and nowhere else, so `APP_LB_ACME_EMAIL` is only useful together with
  `APP_LB_PROXY_ADDR="0.0.0.0:80"` and the `setcap` above. app-lb logs a warning
  at startup if ACME is enabled while the proxy is bound elsewhere. Test with
  `APP_LB_ACME_DIRECTORY` pointed at Let's Encrypt staging before going live —
  production rate limits are weekly and shared across every hostname.
