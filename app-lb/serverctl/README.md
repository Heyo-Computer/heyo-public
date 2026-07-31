# serverctl

A client library **and** a kubectl-shaped CLI for the [app-lb](../README.md)
admin API.

One crate, two products. `cargo install serverctl` gets the CLI;
`serverctl = { version = "0.1", default-features = false }` gets the library
with none of clap, rpassword or a terminal linked in. The CLI is the library's
own first consumer, which is the point: a field the client stops understanding
becomes a compile error rather than a silently blank column at somebody's
terminal.

There is a [TypeScript client](../sdk/typescript) with the same name, the same
surface and the same wire contract.

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

## As a library

```toml
[dependencies]
serverctl = { version = "0.1", default-features = false }
```

```rust
use serverctl::{Client, ExecRequest};

let lb = Client::builder("127.0.0.1:9090")
    .token(std::env::var("APP_LB_TOKEN")?)
    .build()?;

let out = lb.exec("sb-7f3a9c", &ExecRequest::new("uname -a")).await?;
println!("{}", out.stdout);
```

Async by default. Under the `blocking` feature, `serverctl::blocking::Client` is
the same surface with the `await`s taken out — it is what the CLI uses, and it
returns a clear error rather than tokio's panic if you call it from inside a
runtime.

**Typed reads, `Value` writes.** Reads come back as structs; writes take
`serde_json::Value`. That asymmetry is load-bearing: `PUT /deployments/:id`
replaces a *whole* spec, so a client that parsed one into a struct it only half
understood and wrote it back would silently delete every field this build has
never heard of. Round-tripping the `Value` cannot lose anything. `client.raw()`
gives the same reads unparsed, for printing a response or for the read half of a
read-modify-write.

**Shells own their framing.** `client.shell()` returns a session whose `write`
and `resize` speak plain bytes — app-lb's wire protocol prefixes stdin with
`0x01` and silently drops a frame that does not, which is the easiest way to
write a shell client that connects perfectly and types nothing.

**`ShellExit::is_clean()`, not `code == 0`.** app-lb reports an *unknown* exit
code as `0`, so a VM dying under a live session and a clean logout are the same
number. `is_clean()` is false when an error preceded the exit.

**Waiting is provided.** `wait_for_job` reports new log lines as they arrive;
`wait_for_ready` waits on *healthy* backends rather than `ready`, which counts
VMs that are in the pool and failing their health check.

```rust
let job = lb.start_build("api", None).await?;
lb.wait_for_job(&job.id)
    .on_log(|line| println!("{line}"))
    .await?;
```

Full API documentation: `cargo doc -p serverctl --no-default-features --open`.

## App-tokens

```sh
serverctl token mint agent-runner --admin admin -d sb-7f3a9c --expires-in 24
serverctl token list
serverctl token set <id> --all-deployments      # re-scope, same secret
serverctl token revoke <id>
```

The secret is printed once and cannot be recovered — app-lb stores only its
hash. `mint` writes it to stdout and everything else to stderr, so capturing it
works with or without `-q`:

```sh
APP_LB_TOKEN=$(serverctl token mint ci --admin admin --all-deployments -q)
```

A token scoped to specific deployments is refused the fleet-wide routes,
*including minting* — so it cannot widen itself. See the
[app-tokens section](../README.md#app-tokens) of app-lb's README.

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

**Do not point a context at a hostname behind a Google sign-in gate.** serverctl
cannot complete an OAuth flow, and the gate knows it: a browser gets a `302` to
Google, everything else gets a `401` carrying a `login_url` only a browser can
use. Every command then fails with *"the server rejected these credentials"* —
which is what it received, but the credentials were never the problem, and
`whoami` will report the deployment API and `/metrics` both `denied` no matter
what you store.

Two ways out, in the order worth trying:

```sh
# 1. Tunnel to the admin listener and bypass the gate entirely.
ssh -L 9090:127.0.0.1:9090 lb-host
serverctl login --server 127.0.0.1:9090 --user "$APP_LB_DASHBOARD_USER"

# 2. Or let the API paths past the gate, server-side, and gate them with Basic
#    auth instead — see the app-lb README, "Putting the dashboard behind Google".
```

While you are there: `login` prompts for a password but never for a *username*,
and defaults to `admin`. If the server sets `APP_LB_DASHBOARD_USER` to anything
else, pass `--user` — a wrong username produces a `401` indistinguishable from a
wrong password.

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
serverctl get jobs -d web                 # builds, pulls and updates, newest first
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

# A managed VM with no ingress — an agent sandbox, reached by exec/shell only.
serverctl create deployment sb-7f3a9c --no-route --port 8080 --size medium

# A static site: no backend at all, files served off disk by app-lb itself.
serverctl create deployment docs --host docs.example.com --site-root /srv/docs/dist

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
serverctl set route sb-7f3a9c --none                  # withdraw from the proxy
```

Routing flags: `--host`, `--host-suffix` and `--path-prefix` describe **one** rule together, so
`--host web.local --path-prefix /api` means "that host under that path". `--route` adds further
rules and is repeatable — `--route host=a.example.com,path=/api`, or the shorthands `--route /api`,
`--route '*.apps.example.com'`.

`--no-route` (on `create`) and `--none` (on `set route`) are the two halves of
leaving a managed deployment off the proxy entirely. Exposing a sandbox is then
one `set route`, and withdrawing it is one more — neither disturbs the running
VM or its shell sessions. Both are refused for a static (`proxy_pass`)
deployment, which has no other door and would become unreachable.

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

### Pulling an image from an artifact store

The other way a managed deployment gets its image: instead of building one, pull one somebody
already built. `serverctl set artifact` records where from, and `serverctl pull` fetches it,
materializes it as an `.ext4` the daemon can boot, and rolls the pool onto it.

```sh
# Store the API key first, if the store is gated. As with a build's, it stays write-only.
serverctl create secret art api_key=…
serverctl create secret art --from-stdin api_key < ~/.art-key

# Record where the image comes from. This pulls nothing on its own.
serverctl set artifact web --store http://10.0.0.4:8080 --ref web-v2 --secret art/api_key
serverctl set artifact web --grow-gb 8         # extend the rootfs (sparsely) on materialize

# A store root on the app-lb host instead of a URL — much cheaper, see below.
serverctl set artifact web --store /srv/artifacts --ref web-v2

# Pull and roll out.
serverctl pull web --wait                      # blocks until it succeeds or fails
serverctl pull web --ref <digest> --logs       # a one-off ref; the spec's is left alone
serverctl pull web --force                     # re-fetch even if the image is already here

serverctl set artifact web --clear             # stop tracking a source; keep the current image
```

A pull is the same kind of job as a build — asynchronous, `--wait`/`--logs`, one per deployment
at a time, listed by `serverctl get jobs` — and its record answers the question a pull exists to
answer: which *bytes* are running.

```
JOB                DEPLOYMENT   KIND            STATUS      TARGET   RESULT             TOOK
job-c628fbe1ef07   web          artifact-pull   succeeded   web-v2   web-1b9b737b73e2   1s
```

Three things worth knowing:

- **A tag resolves at pull time; a digest does not.** `--ref web-v2` follows wherever that tag is
  moved, so pushing over it is a deploy. `--ref <digest>` pins the bytes, which is what a
  rollback should do — and as a one-off flag it does not touch the stored spec.
- **A re-pull of unchanged bytes is free.** Images are named `<deployment>-<12 hex of digest>`,
  so the file already being there proves the content is right and the transfer is skipped. The
  pool still rolls, because the running VMs booted from whatever rootfs *they* were given.
- **A local store is dramatically cheaper than a URL.** `--store /path` runs `art heyvm
  materialize`, which skips the blob's holes — 48 KiB written for a 48 MiB image against 48 MiB
  transferred over HTTP. Use a URL when the store is on another host; use a path when it is not.

`build` and `artifact` are mutually exclusive on one deployment: both rewrite `vm.image`, so
`set artifact` on a deployment that already builds is refused, and vice versa. To do both, build
on one host and push the result for the others to pull.

## Artifact stores

An artifact store (`art serve`) is a separate service from app-lb, so `serverctl artifact` keeps
its own saved *registries* rather than using the `--server` context. A store is authenticated by
a shared key, not a username and password, and `--context` never retargets a push.

```sh
serverctl artifact login http://10.0.0.4:8080          # prompts for the key
serverctl artifact login http://10.0.0.4:8080 --api-key-stdin < ~/.art-key
serverctl artifact login … --api-key-command 'pass show art/prod'   # keep it in a keychain
serverctl artifact login … --no-store-key              # verify only; supply SERVERCTL_ART_API_KEY

serverctl artifact registries                          # CURRENT NAME URL KEY
serverctl artifact use prod-store
serverctl artifact logout --key-only                   # drop the key, keep the url
```

Registries live in the same `0600` config file as the contexts, under their own key, and
`serverctl whoami` reports both identities — which is the answer to "why did my push get a 401
when everything else works".

### Pushing an image to an artifact store

```sh
serverctl artifact push --image web-v2                 # a heyvm image, by name
serverctl artifact push ./rootfs.ext4 --tag web-v2     # or a path
serverctl artifact push ./rootfs.ext4 --no-tag         # upload only; name the manifest digest
serverctl artifact push ./rootfs.ext4 --force          # upload even if the store has the bytes
```

`--image NAME` resolves `~/.heyo/images/firecracker/<name>.ext4` (or `$MVM_DATA_DIR/…`), which is
where `heyvm mvm build` puts one — so building locally and pushing is two commands. The tag
defaults to the filename without `.ext4`.

A push hashes the file, asks the store whether it already holds those bytes, uploads only if not,
then writes a manifest and moves the tag onto it. The manifest matters: it is what makes a pushed
image indistinguishable from one `art heyvm import` put in, and therefore pullable. Re-pushing
unchanged bytes is two round trips and reports `uploaded: false`.

```sh
serverctl artifact ls                                  # the store's tags
serverctl artifact describe web-v2                     # what a tag or digest resolves to
serverctl artifact usage                               # blobs, logical vs stored, free space
serverctl artifact untag web-v2                        # the blob stays until the store's `art gc`
```

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
serverctl scale sb-7f3a9c --idle-action retain   # stop idle VMs instead of killing them

serverctl restart web                     # drain every VM; the autoscaler boots replacements
serverctl restart web --force --wait      # kill now, then block until the pool is healthy
serverctl rollout status web              # poll until desired == ready and nothing is draining
```

`scale` uses the API's partial `PATCH .../scaling`, so fields you don't pass keep their values.
`--replicas` is a pin, not a one-off: it sets both ends of the band, which is what stops the
autoscaler moving off the number. Give it a `--min`/`--max` band again to hand control back.

`--idle-action retain` stops an idle VM instead of killing it, so a later request
or `exec` resumes that VM rather than booting a fresh one. It keeps the sandbox's
`/workspace` data disk and nothing else — **not** the root filesystem, which the
daemon recopies from the base image on every boot. Pair it with
`--disk-gb` at create time and keep the sandbox's state under `/workspace`,
or it only saves boot time; `serverctl describe` reports which mode a deployment
is in under "When idle".

### Static sites

```sh
serverctl create deployment docs --host docs.example.com --site-root /srv/docs/dist
serverctl create deployment app  --host app.example.com  --site-root /srv/app/dist --site-spa
serverctl update docs             # run the build commands, then re-check the site
```

A site has no backend at all: app-lb serves the files itself, out of a directory
on its own host. `--site-root` is what makes a deployment one; `--site-index`,
`--site-404`, `--site-spa` and `--site-cache-control` configure it, and each is
refused without a root rather than silently ignored.

`--site-spa` serves the index for any unmatched path so a client-side router
owns the URL space — for single-page apps only, since it turns every typo into a
200.

Pair it with `set update` for a git-backed deploy: `serverctl update` runs the
build commands in a directory on the app-lb host and then checks that the index
is actually in the root, so a build that writes its output elsewhere fails
loudly instead of leaving a site that 404s everything. `serverctl describe`
shows the root, index, 404 page and cache policy under "Site".

### Getting inside a VM

```sh
serverctl exec sb-7f3a9c -- ls -la /workspace     # one command; its exit code becomes ours
serverctl exec sb-7f3a9c --cwd /workspace -e RUST_LOG=debug -- cargo test
serverctl shell sb-7f3a9c                          # an interactive PTY
```

`exec` is a pass-through: the guest's stdout goes to stdout, its stderr to
stderr, and its exit code becomes serverctl's — so it composes in a pipeline,
not only at a prompt. `-o json` returns the whole record instead, including
which `sandbox_id` ran it.

Both commands **start a VM** for a deployment that has none running, waiting up
to the deployment's `cold_start_timeout_secs`; `--no-wake` asks for an error
instead. Both go through app-lb rather than the heyvm daemon, so they work from
anywhere the admin API does, use the credentials already in your context, and
can wake a sandbox that was suspended by `--idle-action retain`.

Together they are the only way into a deployment created with `--no-route`,
which takes no HTTP traffic at all. An open `shell` holds the VM: it counts as
in-flight work, so a sandbox will not be scaled to zero underneath a live
session.

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

## The three kinds of deployment

The distinction runs through every command, because app-lb enforces it:

| | managed (`vm`) | static (`upstreams`) | site (`site`) |
| --- | --- | --- | --- |
| backends | an autoscaled pool of microVMs | fixed `host:port` addresses | none — files off disk |
| `scale` | yes | rejected — the policy is inert | rejected — nothing to scale |
| `restart` / `delete vm` | yes | rejected — nothing to evict | rejected — nothing to evict |
| `exec` / `shell` | yes | rejected — upstreams are addresses | rejected — a site is files |
| `set image` / `set env` | yes | rejected — no VM template | rejected — no VM template |
| `set build` / `build` | yes | rejected — no guest image to build | rejected — no guest image |
| `set artifact` / `pull` | yes — but not alongside `build` | rejected — no guest image to pull into | rejected — no guest image |
| `set update` / `update` | rejected — its backends are VMs | yes | yes — how a site is deployed |
| `set auth` | yes | yes | yes — the gate is in the proxy, ahead of all three |
| `set upstreams` | rejected | yes | rejected |
| `DESIRED` column | the autoscaler's target | `—` |

Where the API rejects one of these, serverctl passes the server's reason through and, where it
knows the answer up front, says which command to use instead.

## Exit codes

`0` success, `1` a failed command (the reason goes to stderr as `error: …`), `2` a usage error
from the argument parser.
