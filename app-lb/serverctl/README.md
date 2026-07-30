# serverctl

A kubectl-shaped CLI for the [app-lb](../README.md) admin API.

The verbs are kubectl's because the mental model is the same: declarative specs you `apply`,
imperative helpers (`create`, `scale`, `set`) that write those specs for you, and read commands
(`get`, `describe`, `top`) that render them back. What it drives is app-lb's admin API —
deployments, their microVM pools, and the certificates app-lb issues for their hostnames.

```sh
cargo build --release -p serverctl
install -m 0755 target/release/serverctl ~/.local/bin/
```

It is a separate crate from the load balancer, so installing it doesn't drag in pingora, openssl
or the ACME stack — it shares nothing with app-lb but the wire format.

## Quick start

```sh
serverctl login --server 127.0.0.1:9090        # prompts for the password, if the server wants one
serverctl get deployments
serverctl create deployment web --host web.local --image nginx-fc --port 80 --min 1 --max 4
serverctl rollout status web
serverctl top
```

With no config file at all, commands go to `http://127.0.0.1:9090` — app-lb's default admin
listener — so a local LB needs no setup.

## Connecting

app-lb's admin listener is **plaintext HTTP on loopback** by default. To reach a remote one,
either tunnel it:

```sh
ssh -L 9090:127.0.0.1:9090 lb-host
serverctl --server 127.0.0.1:9090 get deployments
```

…or front it with app-lb's own TLS listener (see [`examples/app-lb-admin.json`](../examples/README.md))
and point serverctl at the HTTPS name:

```sh
serverctl login --server https://lb-admin.example.com --user admin
```

`--insecure-skip-tls-verify` exists for a self-signed admin endpoint you control.

## Authentication

app-lb authenticates with HTTP Basic and has **two independent gates**:

| Server setting | Gates |
| --- | --- |
| `APP_LB_DASHBOARD_PASSWORD` | the dashboard and `/metrics` (so: `top`, `status`) |
| `APP_LB_ADMIN_AUTH=1` | additionally the deployment API (so: `get`, `create`, `scale`, …) |

`serverctl login` probes both, tells you which it found, verifies the credentials against
whichever is actually gated, and saves a **context**. `serverctl whoami` reports what the current
identity is allowed to do — the answer to "why am I getting a 401":

```
$ serverctl whoami
Client:
  Config file:             ~/.config/serverctl/config.json
  Context:                 local
  Server:                  http://127.0.0.1:9090
  User:                    admin
  Password:                stored in the config file
Server:
  Reachable:               yes (GET /healthz)
  Auth required for:       dashboard, /metrics and the deployment API
  Deployment API:          allowed
  Metrics:                 allowed
```

### Where the password comes from

In precedence order:

1. `--password` / `SERVERCTL_PASSWORD`
2. the context's `password_command` — a shell command whose stdout is the password
3. the context's stored `password`

The config file is written `0600` (and its directory `0700`), but a stored password is stored in
the clear — there is no token endpoint to trade it for something shorter-lived. To keep it out of
the file entirely:

```sh
serverctl login --server lb.example.com:9090 --password-command 'pass show app-lb/admin'
serverctl login --server lb.example.com:9090 --no-store-password   # then export SERVERCTL_PASSWORD
```

### Contexts

Several load balancers, kubeconfig-style:

```sh
serverctl config get-contexts
serverctl config use-context prod
serverctl config set-context staging --server https://lb.staging.example.com --user admin
serverctl --context staging get deployments      # one-off, without switching
serverctl logout --keep-context                  # forget just the password
```

`SERVERCTL_CONFIG`, `SERVERCTL_CONTEXT`, `SERVERCTL_SERVER`, `SERVERCTL_USER` and
`SERVERCTL_PASSWORD` override the file for scripts and CI.

## Commands

Resource names take kubectl's forms: `deployments`, `deployment web`, `deployment/web`, `deploy web`,
or a bare `web` where the kind is unambiguous.

### Reading

```sh
serverctl get deployments                 # NAME KIND ROUTES DESIRED READY PENDING IN-FLIGHT
serverctl get deployments -o wide         # + MIN MAX WARM TARGET BACKEND SOURCE AUTH
serverctl get deployment/web -o yaml      # the server's JSON, as YAML
serverctl get vms -d web                  # backends of one deployment
serverctl get certs                       # issued TLS certificates and expiry
serverctl get secrets                     # ids and key *names* — never values
serverctl get jobs -d web                 # builds and updates, newest first
serverctl get job job-3f2a1c8e            # one job in full, with its log
serverctl get deployments -w              # re-render every 2s

serverctl describe deployment web         # spec, pool, backends and traffic in one page
serverctl status                          # uptime, host, fleet and traffic totals
serverctl top                             # per-deployment CPU, memory, latency, 5xx
serverctl top vms
serverctl top host
```

`-o json|yaml` prints the server's own payload untouched, so it round-trips:

```sh
serverctl get deployment web -o json | serverctl --context staging apply -f -
```

### Creating and editing

```sh
# A managed VM pool.
serverctl create deployment web \
  --host web.local --image nginx-fc --port 80 --size mini \
  --min 1 --max 4 --warm 1 --target-concurrency 10 \
  --health-path /healthz -e RUST_LOG=info

# A static (proxy_pass) deployment.
serverctl create deployment legacy --path-prefix /legacy --upstream 10.0.0.9:8080 --health-tcp

# From a file — JSON or YAML, one spec, a JSON array, or a multi-doc YAML stream.
serverctl apply -f deploy.yaml
serverctl apply -f examples/heyosecret.json --dry-run

# In place.
serverctl edit deployment web             # $EDITOR round-trip; a rejected edit is kept on disk
serverctl set image web nginx-fc-v2
serverctl set env web RUST_LOG=debug FEATURE_X-        # `KEY=VALUE` sets, `KEY-` removes
serverctl set upstreams legacy 10.0.0.9:8080 10.0.0.10:8080
serverctl set route web --host web.example.com --path-prefix /api
serverctl set route web --route '*.apps.example.com' --add
```

Routing flags: `--host`, `--host-suffix` and `--path-prefix` describe **one** rule together, so
`--host web.local --path-prefix /api` means "that host under that path". `--route` adds further
rules and is repeatable — `--route host=a.example.com,path=/api`, or the shorthands `--route /api`,
`--route '*.apps.example.com'`.

Every `set` command and `edit` is a read-modify-write against `PUT /deployments/:id`, which
replaces the whole spec. serverctl edits the server's JSON rather than a struct of its own, so
fields it has never heard of survive the round trip.

### Building an image from git

A managed deployment can carry a *build source* — a git repo and a Dockerfile — instead of only
an image name. `serverctl build` checks the repo out on the app-lb host, builds the image with
`heyvm mvm build`, and rolls the pool onto the result.

```sh
# Store the credential first, if the repo is private. The value is never readable back.
serverctl create secret github --from-stdin token < ~/.github-pat
serverctl create secret github --from-env token=GITHUB_TOKEN --description 'CI PAT for acme/*'

# Record where the image comes from. This builds nothing on its own.
serverctl set build web --repo https://github.com/acme/web.git --ref main --secret github
serverctl set build web --dockerfile deploy/Dockerfile --size-mb 768

# Or say it at creation time.
serverctl create deployment web --host web.example.com --port 8080 \
  --repo https://github.com/acme/web.git --ref main --secret github

# Build and roll out.
serverctl build web --wait                 # blocks until it succeeds or fails
serverctl build web --ref v2.1.0 --logs    # a one-off ref; streams the build output
serverctl build web                        # fire and forget; poll with `get job <id>`

serverctl set build web --clear             # stop tracking a source; keep the current image
```

A build is asynchronous server-side, so plain `serverctl build` returns as soon as it is
scheduled and prints the id to follow. `--wait` polls to completion, `--logs` also streams the
output as it arrives; either way a failed build exits non-zero after printing the tail of the
log. One job runs per deployment at a time — a second is refused, not queued.

Each build produces an image named `<deployment>-<short sha>`, so `serverctl describe` and
`get -o wide` say which commit is actually running. Rotating a token is
`serverctl set secret github token=ghp_new…`; keys you don't mention keep their values, which
matters because there is no way to read them back and resend them.

### Updating a static deployment

A static deployment has no image to build — its backend is a process on the app-lb host. Its
update path is a working directory and the commands to run in it: what you would otherwise ssh
in and do.

```sh
serverctl set update app-obs \
  --workdir /home/sarocu/Projects/app-obs \
  -c 'git pull --ff-only' \
  -c 'cargo build --release' \
  -c 'supervisorctl restart app-obs'

serverctl update app-obs --wait --logs     # run them, then check the upstreams answer
serverctl update app-obs                   # fire and forget; poll with `get job <id>`

# Optional extras.
serverctl set update app-obs --secret github            # credential for a private `git pull`
serverctl set update app-obs --secret-env APP_OBS_INGEST_TOKEN=obs/ingest_token
serverctl set update app-obs --verify-timeout 0         # skip the post-update health check
serverctl set update app-obs --clear                    # stop tracking how it updates
```

Each `--command` is a shell line run in `--workdir`, in order, and the first failure stops the
job — `serverctl get jobs` shows how far it got (`1/3 commands`). Afterwards the deployment's
upstreams are re-probed with its own health check: **a job whose commands exited 0 but whose
service never came back is a failure**, and says so rather than reporting success.

Passing `--command` replaces the whole list (as do `--env` and `--secret-env`), so send the
steps you want, not a delta. Everything else you don't pass is kept.

The commands run as app-lb's user. Restarting a service usually needs a grant for exactly that
verb — access to supervisord's socket, or a `sudo -n` entry for one `systemctl restart` — and
nothing broader; the admin API is what triggers this.

### Putting a deployment behind Google sign-in

Any deployment — managed or static — can be gated. The gate runs in app-lb's proxy, so the
application behind it is unchanged and unaware.

```sh
# The client secret is a stored secret, never a spec field.
serverctl create secret google --from-stdin client_secret < ~/.google-oauth-secret

serverctl set auth web \
  --client-id 1234-abc.apps.googleusercontent.com \
  --secret google/client_secret \
  --allow-domain example.com \
  --allow-email contractor@gmail.com \
  --public-path /healthz

serverctl describe deployment web     # prints the redirect URI to register with Google
serverctl get deployments -o wide     # AUTH column: which deployments are gated
serverctl set auth web --clear        # remove the gate
```

Both allow flags are repeatable and take any number of entries, and the two lists are **OR'd** —
one match admits the caller:

```sh
serverctl set auth web \
  --allow-domain sarocu.com --allow-domain heyo.computer \
  --allow-email contractor@gmail.com --allow-email auditor@example.org
```

`--allow-domain` matches Google's `hd` claim — the Workspace that *governs* the account, not the
text after `@` — so a personal account with a lookalike address is refused. That also means a
personal account can only be admitted by `--allow-email`, since it carries no `hd` at all; if
`dig +short MX <domain>` shows something other than Google's servers, every account there is a
personal one as far as this claim goes. A Workspace with several domains needs each domain that
appears in `hd`.

`--allow-domain '*'` admits any Google account, and is the only way to say that: an empty
allow-list is rejected.

**Passing any `--allow-domain`/`--allow-email`/`--public-path` replaces that whole list**, so
growing one means resending all of it. There is no "add one" flag; `serverctl edit deployment web`
is the incremental route — it opens the spec in `$EDITOR` and touches only what you change.

Adding or removing an entry signs every current user out once — they bounce through Google and
straight back in — which is what makes *removing* someone take effect immediately rather than
when their cookie expires. Reordering or re-casing a list is free: the policy fingerprint sorts
and lowercases before hashing, so only a real change to who may enter invalidates a session.

### Scaling and rollouts

```sh
serverctl scale web --replicas 3          # pin: min = max = 3
serverctl scale web --min 1 --max 8 --warm 2 --target-concurrency 20
serverctl scale web --scale-to-zero-after 600

serverctl restart web                     # drain every VM; the autoscaler boots replacements
serverctl restart web --force --wait      # kill now, then block until the pool is healthy
serverctl rollout status web              # poll until desired == ready and nothing is draining
```

`scale` uses the API's partial `PATCH .../scaling`, so fields you don't pass keep their values.
`--replicas` is a pin, not a one-off: it sets both ends of the band, which is what stops the
autoscaler moving off the number. Give it a `--min`/`--max` band again to hand control back.

### Deleting

```sh
serverctl delete deployment web           # deregister, then drain and reap every VM
serverctl delete deployments --all
serverctl delete vm sb-abc123 -d web      # drain one VM
serverctl delete vm sb-abc123 -d web --force   # kill it, dropping in-flight requests
serverctl delete secret github            # refused while a deployment's build refers to it
serverctl delete secret github --force    # delete anyway; those builds stop authenticating
```

Evicting a VM is *recycle*, not *shrink* — the autoscaler boots a replacement on its next tick if
the policy still wants the capacity. Use `scale` to shrink.

### Shell completion

```sh
serverctl completion bash > /etc/bash_completion.d/serverctl
serverctl completion zsh  > ~/.zfunc/_serverctl
```

## Managed vs static deployments

The distinction runs through every command, because app-lb enforces it:

| | managed (`vm`) | static (`upstreams`) |
| --- | --- | --- |
| backends | an autoscaled pool of microVMs | fixed `host:port` addresses |
| `scale` | yes | rejected — the policy is inert |
| `restart` / `delete vm` | yes | rejected — nothing to evict |
| `set image` / `set env` | yes | rejected — no VM template |
| `set build` / `build` | yes | rejected — no guest image to build |
| `set update` / `update` | rejected — its backends are VMs | yes |
| `set auth` | yes | yes — the gate is in the proxy, ahead of either |
| `set upstreams` | rejected | yes |
| `DESIRED` column | the autoscaler's target | `—` |

Where the API rejects one of these, serverctl passes the server's reason through and, where it
knows the answer up front, says which command to use instead.

## Exit codes

`0` success, `1` a failed command (the reason goes to stderr as `error: …`), `2` a usage error
from the argument parser.
