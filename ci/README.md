# ci

CI orchestration and a server-rendered dashboard for [heyvm](https://heyo.computer)
microVMs, with NATS JetStream as the job queue.

A machine becomes a runner by running `heyvmd` and joining a heyvm network.
There is no agent to install: this process discovers those hosts, opens an iroh
tunnel to each, and drives builds on them.

It is a sibling of [app-lb](../app-lb) and [queue-fn](../queue-fn) — same
conventions, same house style, different problem. app-lb keeps a pool of VMs warm
behind an HTTP data plane; queue-fn runs a command in one per event; `ci` runs a
workflow's worth of them per commit.

## Requirements

- **heyvmd** on every machine that should build, joined to one heyvm network
  (`heyvm network add-host`). `firecracker` and `kvm` are the supported drivers;
  `libvirt` is rejected at parse time.
- **Postgres** for run history, the job DAG and the VM pool.
- **NATS with JetStream** (`nats-server -js -sd /var/lib/ci-js`).
- Optionally **app-lb** for workflow objects and sign-in, **heyosecret** for
  secrets, and the **artifacts** store.

## Run

```bash
CI_HEYO_API_KEY=… CI_NETWORK=prod-runners \
CI_DATABASE_URL=postgres://…/ci CI_WEBHOOK_SECRET=$(openssl rand -hex 32) \
cargo run
```

Configuration is environment-only; there are no CLI arguments. **A
misconfiguration is a startup exit, not a degraded service** — every error names
the variable to fix. See `deploy/supervisor/ci.conf` for the full set.

## Submitting a build

```bash
./install-git-submit.sh                  # installs `git-submit` onto PATH

# From the dashboard's /repos page, which mints these two lines for you:
git config ci.endpoint https://ci.us2.heyo.work
git config ci.token    cis_019fca648a6e-00000002.…

git submit --dry-run    # show what would be sent
git submit              # submit HEAD
git submit --dirty      # include uncommitted tracked changes
git submit --archive    # send a tree-only tarball instead of a bundle
```

`git submit` sends a **`git bundle`**, which clones in the guest into a real
repository — so `git describe`, `git log` and `git rev-parse` work in a step. Two
consequences of the submitter packing it rather than the server fetching it, and
both are the point:

- **No repository credential exists anywhere in this system.** Not on the
  orchestrator, not in a guest. The submitter already had read access — they ran
  `git bundle` — so nothing else needs its own. A CI system that clones for you
  is a CI system holding a key to every repository it builds.
- **The tree is exactly what the submitter meant.** No re-resolving a ref that
  may have moved, no guessing whether dirty work was included.

The cost is history. A bundle that clones on its own **must reach a root
commit**: `git bundle create --depth` does not exist, and a `--max-count` slice
is refused at clone time with *"Repository lacks these prerequisite commits"*. So
the payload scales with history rather than with one tree, and `--archive` sends
the old tree-only tarball for the repository where that is the wrong trade.

Two practical requirements: a bundle needs `git` on the orchestrator **and** in
the guest image; a tarball needs neither. Each absence is reported by name.

Three shapes of `git bundle` do not work, and the client is built around them:
it refuses a bare sha (*"Refusing to create empty bundle"*), so `--ref <sha>` and
`--dirty` pack through a throwaway bare repo that borrows your object store via
`alternates` rather than writing refs into it; and a bundle carrying **zero
refs** passes `git bundle verify` as "complete" and clones into an empty
repository, so the server counts refs itself rather than trusting the verify.

## Registered repositories, and the token that submits

```bash
# On the dashboard: /repos → register a clone URL → Mint.
# It shows the token exactly once, with the two `git config` lines above.
```

A submit endpoint on the open internet is arbitrary code execution on a runner,
so what stands in front of it matters more than anything else here. There are two
credentials and the difference is not strength, it is **scope**.

`CI_WEBHOOK_SECRET` is one shared secret, HMAC'd over the body, handed to
everyone who submits from anywhere. It cannot be revoked for one repository, and
it cannot say *which* repository is submitting — so a submit's `repository` field
is something the server takes on trust.

A **repository token** is minted per registration, revocable on its own, and
*is* the statement of which repository the submit is for. A submit whose payload
names a different repository than its token is refused, which is what stops a
token for a repository somebody can push to from building any repository at all
— with this installation's secrets.

- **Stored as a SHA-256 digest, and shown once.** Verifying an HMAC would need
  the server to hold every key, and one read of that table is every repository's
  credential. A bearer inside TLS reverses the trade: the secret transits, and
  what is at rest cannot submit.
- **`ci_repo.workflow_path`** overrides `CI_WORKFLOW_PATH` for one repository. A
  workflow object still wins, being the more specific statement.
- **Pausing** a registration refuses its tokens without destroying them. The
  shared secret is unaffected — it belongs to no repository, so nothing about one
  can stop it. `CI_REQUIRE_REPO_TOKEN=true` turns it off entirely, which is where
  an installation lands once every repository has a token.

`/repos` is deliberately **not** in `public_paths`: it is a browser page behind
app-lb's gate, and admin-only on top of it. With `CI_ADMIN_EMAILS` unset it also
accepts a request carrying no identity at all — that is the local loop, where
there is no gate and no accounts — and startup warns about exactly what that
means.

## A workflow

```yaml
name: build
on: [submit]

jobs:
  build:
    uses: prod-runners/bigbox        # <network>/<runner>
    vm:
      driver: firecracker
      build:                         # or `image:` — see "Images" below
        dockerfile: deploy/image/Dockerfile
      size_class: medium
      cache_key_files:               # busts the warm VM when these change
        - Cargo.lock
        - rust-toolchain.toml
    strategy:
      matrix:
        target: [x86_64, aarch64]
      max-parallel: 2
    steps:
      - name: Build
        run: cargo build --release --target ${{ matrix.target }}
        env:
          DATABASE_URL: ${{ secrets.DATABASE_URL }}
          REGION: ${{ vars.REGION }}
      - uses: ci/upload-artifact
        with: { name: bin-${{ matrix.target }}, path: target/release/app }

  deploy:
    uses: prod-runners              # any online host in that network
    needs: [build]
    if: ${{ needs.build.result == 'success' }}
    vm: { driver: firecracker, image: ubuntu:24.04 }
    steps:
      - run: ./deploy.sh
```

GitHub Actions' shape, with two departures.

**`uses:` places the job**, where GitHub has `runs-on:` selecting a label. That
is the point of the system: a job names the heyvm network and the machine it
wants, and membership of that network is what makes a host eligible.

```yaml
uses: default                       # the host this CI is running on
uses: prod-runners                  # any online host in that network
uses: prod-runners/bigbox           # that host; `vm:` builds a VM on it
uses: prod-runners/bigbox/sb-1a34   # that existing VM; `vm:` is unused and
                                    # every step is an exec into it
# absent                            # the repository's assigned network, any host
```

**`uses:` carries everything needed to place the job**, and the third form is
why that matters. A sandbox does not record which host it is on — `SandboxInfo`
has no daemon field and there is no cloud-proxied exec — so `<network>/*/<vm>`
would force the orchestrator to interrogate every host in the network to find one
VM. Naming the node is refused-if-absent rather than guessed.

### Monorepos: not every workflow on every commit

`on: submit:` takes branch and path filters, and a workflow whose filters decline
a submit produces no run at all.

```yaml
on:
  submit:
    branches: [main, 'release/*']   # or branches-ignore
    paths:                          # or paths-ignore
      - 'packages/api/**'
      - Cargo.lock
```

```text
*      any run of characters within one segment; never crosses `/`
**     any number of whole segments, including none
?      exactly one character within one segment
```

`paths` builds when **any** changed path matches. `paths-ignore` skips only when
**every** changed path matches — one interesting file in a commit is enough to
build. `paths`/`paths-ignore` and `branches`/`branches-ignore` are each mutually
exclusive: GitHub allows a mixture and resolves it by pattern order within the
list, which is not readable off the file, so the combination is refused. A
leading `!` is refused for the same reason, naming `paths-ignore` instead.

A misspelled filter is a **parse error**, not a filter that quietly does nothing
— the same rule `stpes:` gets. This block used to be read and discarded, so a
workflow that said `branches: [main]` built every branch with nothing reporting
that it had.

The finer-grained form is a condition on one job, which is what a monorepo
usually wants — one workflow file, one run, and the packages that changed:

```yaml
jobs:
  api:
    if: ${{ changed('packages/api/**') }}
    ...
  web:
    if: ${{ changed('packages/web/**', 'packages/shared/**') }}
```

`changed()` runs the same matcher the `paths:` filter does, so a job condition
and a workflow filter cannot disagree about what a pattern covers. It is not
`contains(ci.changed_files, …)`, which on an array is an equality test and would
need the exact path of every file somebody might touch.

**The diff comes from the submitted bundle's own history**, not from a fetch:
`git diff --name-only --no-renames <before> HEAD` in the unpacked clone, where
`before` is what the client sent (`git rev-parse HEAD^`). Rename detection is
off deliberately — a file moved between two packages must rebuild both, and with
it git reports only the destination.

The **`after` side is the clone's `HEAD`, not the payload's `after`**, because
`git submit --dirty` reports `<sha>-dirty`: a label for a person, not a
resolvable object. The bundle's `HEAD` is the only thing that points at the tree
that actually travelled.

#### When the diff cannot be read

`--archive` sends a tarball with no history. A root commit has no parent. A
`before` from a history the bundle is not part of resolves to nothing. In all of
these there is no answer, and **no answer matches every filter** — the workflow
builds.

The other direction is the failure worth designing against: unknown meaning
"nothing changed" is a CI system that quietly stops building and reports a green
tick on a commit nothing ran on. The same rule holds for `changed()`, which is
true when the diff is unknown, and for `paths-ignore`, which does not skip on
one. `ci.changes_known` and `ci.changes_reason` say which case a run is in.

A submit where every workflow declined is a **success with no runs**, not an
error: in a monorepo most commits touch one package, so most workflows correctly
build nothing. `git submit` prints the reason each one gave. Nothing matching the
glob at all is still an error — that is a mistake, not a decision.

### The `ci` expression scope

What commit a run is for, readable from any `if:` or `${{ }}`:

```text
ci.sha             ci.ref              ci.changed_files    (array)
ci.before          ci.branch           ci.changes_known    (bool)
ci.repository      ci.run_id           ci.changes_reason   (empty when known)
ci.workflow
```

Read from the run row rather than frozen onto each job's plan, unlike the network
assignment beside it. The two are not the same kind of fact: a repository can be
reassigned to another network mid-build, so the plan freezes that; the commit a
run is for is fixed when the bundle is unpacked and cannot move under a
redelivery. Freezing it anyway would copy a monorepo-sized path list onto every
job row.

### Images: `image:` or `build:`

```yaml
vm:
  image: ubuntu:24.04              # a public base, or a name the host already has

vm:
  build:                           # a Dockerfile in the submitted tree
    dockerfile: deploy/image/Dockerfile
    context: deploy/image          # defaults to the Dockerfile's own directory
    size_mb: 6144                  # rootfs size; absent = auto from the tree
```

The two are mutually exclusive and saying both is a parse error: a built image's
name is the hash of its Dockerfile and context, so an author-supplied one could
only disagree with it.

**`build:` builds the image on the runner, once, by the runner's own daemon.**
The first job to want it on a given host uploads the Dockerfile and its context
to heyvmd's `POST /images/build`, which runs the `heyvm mvm build` pipeline —
`docker build → docker export → mke2fs` — on the host and installs the result
into that host's image catalog. Every later job naming the same Dockerfile
boots straight from the image. Edit the Dockerfile, anything in its context, or
`size_mb` and the name changes, so the next run builds a new one — there is
nothing to remember to invalidate, and the host's docker layer cache keeps the
rebuild incremental.

It exists because the alternative was a footgun. `image: ci-rust` names a
rootfs that has to be built by hand with `heyvm mvm build` on every runner, and
a host that never had that run fails every job at VM creation — which, before
the `building` row above, looked exactly like a run nothing had picked up.

Because the build *is* `docker build`, docker's semantics apply in full —
multi-stage, `COPY --from=`, `ADD`, `ARG`, `.dockerignore`. `ci` does not parse
the Dockerfile; it hashes the bytes and ships them. What does not survive is
what `docker export` has never carried: **OCI metadata**. `ENV`, `CMD` and
`ENTRYPOINT` build fine and then vanish from the rootfs — an environment
variable steps need must be written to `/etc/profile.d` by a `RUN` (steps run
under `sh -lc`, which reads it), and the VM boots the kernel's `init=/init.sh`,
which must print `HEYVM_READY`. An image without an init script builds
successfully and then fails every boot; `deploy/image/init.sh` is the contract.

The runner host needs `docker`, `mke2fs` and — when heyvmd does not run as
root — `fakeroot`. Each is checked by the build and named if absent, and the
failure lands on the job and on `/vms` like any other build failure.

Concurrency is settled twice, at two scopes. `ci_vm_image`'s upsert hands
exactly one *job* the build and tells the rest to wait, so ten jobs landing on
a cold host produce one build request and not ten; a claim whose lease has
lapsed is taken over, so a dispatcher that died mid-build does not block the
image for ever. And the daemon's route is idempotent by name — a second request
for an image already in the catalog answers `ready` without building — so even
a lost claim collapses into one docker build rather than two racing for the
same tag.

Nothing sweeps images. A rootfs is expensive to rebuild and cheap to keep, and
unlike a pooled VM it carries no state from the run that made it. To force a
rebuild, delete it on the host (`rm ~/.heyo/images/firecracker/ci-img-*.ext4`);
the next job finds the file gone, forgets the row and builds it again.

**A named VM is somebody else's machine**, and the executor treats it that way.
It is resolved on the pinned node by id or name, started if it is merely stopped,
and then every step execs into it. Nothing else about the normal path applies:
no fingerprint, no warm pool, no creation — and **no teardown**, so a long-lived
VM is not destroyed because the job's `vm:` block happened to say
`reuse: false`. Its TTL is left alone too; renewing it would be this app quietly
extending the life of something it does not own, which is worth knowing if a
build outlasts the TTL somebody else set.

The `vm:` block is inert for such a job. The schema still requires one — it is a
non-optional field — so it is written and ignored, and the run logs that it was.

`default` is the only form that names no network, and it is not the same as
omitting `uses:`: absent means the repository's assignment, while `default`
means this machine regardless.

**A pinned job does not silently migrate.** If its host is offline the job stays
queued for that host and fails after `CI_RUNNER_WAIT_SECS`, because the warm pool
is host-local — moving the job discards the cache the pin asked for and turns a
fast build into a slow one for reasons nothing reports. `fallback: any` opts in.

**A submit against an offline host warns rather than refusing.** The job sits on
that host's subject and runs when the host comes back — messages outlive the
absence of a consumer, and a durable created later binds with `DeliverAll`, so it
receives everything already queued rather than only what arrives after it. A
network blip is survivable by construction; refusing at submit would turn a
recovery into a lost submit. `git submit` prints the warning, and the run page
shows the wait.

That timeout matters more than it looks. Consumers are bound only for hosts that
are **online**, while `route_for` pins a job to its node whatever its status — so
a job pinned to a host that is not online goes to a subject nothing reads. The
wait is what turns that from a run stuck for ever with no steps and no error into
a failure naming the host it was waiting for. It is checked on the lease loop and
re-confirmed against the live pool first, so a host that comes back in the
meantime keeps its job.

**`vm:` describes the machine.** GitHub gives you an opaque runner image; here
the author declares the driver, image, size and setup hooks — and, via
`cache_key_files`, what should invalidate the warm VM the next run would reuse.

`deny_unknown_fields` is on throughout, so `stpes:` or `timeout_minutes:`
(instead of `timeout-minutes:`) is a parse error naming the job, not a field that
quietly does nothing.

### This repository's own

`.ci/workflows/build.yml` is one job that produces one thing: the release binary,
uploaded as the `ci` artifact. `cargo test` parses and plans every file in that
directory, so a typo in it fails here rather than at a submit somebody is waiting
on.

**Adding checks.** `cargo fmt --check` and `cargo clippy` are not in it, and the
reason is a trap worth naming: the setup hook installs rustup with
`--profile minimal`, which ships `rustc`, `cargo` and `rust-std` and **not**
rustfmt or clippy. A `cargo fmt` step against that toolchain fails with
`no such command`, which reads as a broken CI rather than a missing component.
Either drop `--profile minimal`, or add the components explicitly:

```yaml
- curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
- . "$HOME/.cargo/env" && rustup component add rustfmt clippy
```

The integration suite is a separate question: it needs Postgres, NATS and a
`heyvmd`, so it belongs on a runner provisioned with them rather than on a
default image.

## Networks

```bash
CI_NETWORK=prod-runners          # serve one
CI_NETWORK=prod-runners,lab      # serve two; the first is the default
CI_NETWORK='*'                   # serve every network on the account
```

`/networks` carries a **Queue** column: what is waiting on each host's subject
and each network's unpinned subject, read straight from JetStream. Exact rather
than sampled, because `WorkQueue` retention deletes on ack — the count *is* the
backlog.

The reading that matters is **`no consumer`**, flagged in red on a host that is
online. Consumers are bound only for online hosts while `route_for` pins a job to
its node whatever its status, so a job can be routed to a subject nothing reads.
That is what a stuck run looks like from the outside, and it used to be
answerable only by curling the NATS monitoring endpoint. On an *offline* host the
same state is expected and is stated rather than alarmed about. NATS being
unreachable is a banner, not a page of zeroes — an idle queue and an unreadable
one must not look alike.

`/networks` lists **every** heyvm network on the account with the hosts in each,
whether or not this instance builds for it, plus the daemons that joined no
network at all. That last list is there because "my runner isn't picking up
jobs" is otherwise a dead end: a registered daemon that never joined looks
exactly like one that was never registered.

**Add this host** joins the machine running `ci` to a network, which is
`heyvm network add-host` without needing a shell on that box. It is offered only
for a network this host is not already in, and it is admin-only behind the gate —
joining a host to a network unlocks host-shell access to it, so it is not a read.
Joining does not make this instance *serve* that network; the page says so when
the two differ, because finding out from a job that never runs is worse.

The member is posted by hand rather than through the SDK, because
`NetworkMemberKind` is `Local | Deployed` and has no `host` variant at all —
`heyvm network add-host` has the same problem and solves it the same way.

**What an instance serves is configuration, not discovery.** Jobs are sharded
onto one durable JetStream consumer per runner and per network precisely so
several orchestrators can run at once *as long as they own disjoint sets*. An
instance that silently served everything would eat another's work. `*` opts into
serving everything, which is right for the single instance most installations
run — as a decision that was made rather than one that happened.

Members are read per network, concurrently: the control plane has no
"all members everywhere" route and `NetworkInfo` carries no member count, so N+1
reads is the only shape available. Running them together makes a refresh one
round trip's worth of latency instead of N.

### Assigning one to a repository

A registered repository (`/repos`) can name the network its builds run in, stored
in `ci_repo.network`. The order of precedence, most specific first:

1. the job's own `uses: <network>/<runner>`
2. the workflow object's `network`, where app-lb has one
3. the repository's assigned network
4. the installation default — the first entry of `CI_NETWORK`, or the account's
   default network under `*`

The resolved network is **stamped into the stored job plan at submit time**, not
looked up when the job runs. A redelivery therefore runs where the job was
scheduled, and reassigning a repository mid-build does not move work onto
hardware that never warmed a VM for it — the same reason the expanded plan is
stored rather than recomputed.

**A submit naming a network this instance does not serve is refused at the
client**, with the network and the served list in the message. The alternative is
a run that exists, jobs on a queue nobody consumes, and no answer to "why is my
build stuck" short of reading a table.

The assignment is stored as the network's **name**, not its id: a name is what
`heyvm network create` took, what `uses:` spells, and what the dashboard shows,
so a hand-written query stays readable. The cost is that renaming a network in
heyvm orphans the assignment — which surfaces as a refused submit and a warning
on `/repos`, rather than as a build that quietly moves.

## Cancelling a run

`POST /runs/{id}/cancel`, from a button on the run page while there is something
to stop. It marks the run and every unfinished job `cancelled` — and that one
statement is the whole mechanism, because it covers work in each of the three
states it might be in without reaching a runner at all:

- **queued** — dropped when JetStream delivers it, since `run_job` refuses a job
  that is already terminal on entry;
- **about to start** — refused by `start_job`, whose `WHERE` clause excludes
  terminal statuses;
- **running** — noticed at the next step boundary.

**Cancellation is cooperative, and the limit is honest**: the daemon has no route
to abort an exec-operation in flight, so a step that has already started runs to
its own end or its `timeout-minutes`. What stops is everything after it. The page
says so next to the button rather than implying an instant kill.

A cancelled job stays cancelled. `continue-on-error` is about a step failing, not
about somebody stopping the run, so it does not convert a cancellation into a
success — and the executor does not write `failure` over it, which would make a
deliberate stop read as a broken build.

## The warm VM pool

```
fingerprint = sha256( canonical_json(vm block, minus cache_key_files)
                    ‖ for each path in sorted(cache_key_files):
                          path ‖ 0x00 ‖ sha256(contents)  — or an ABSENT marker )
```

A job claims an idle VM on its runner with a matching fingerprint, or builds one.
Two decisions in there:

- **`cache_key_files` is stripped before hashing.** Otherwise editing the *list*
  would rebuild every VM even when every listed file is byte-identical.
- **A missing file hashes to an explicit marker.** Skipping it would make "no
  `Cargo.lock`" and "an empty `Cargo.lock`" indistinguishable, so *adding* a
  lockfile later would not bust the pool — the moment it most needs busting.

`/vms` shows the pool and answers the two questions worth asking of it. **Is
reuse working** — rows are grouped by host and fingerprint, and a fingerprint
appearing twice on one host is flagged `not reused`, because that is two VMs
where one would have done. **What was left behind** — each row carries the
outcome of the run that last used it, so a machine a failed build left in a
strange state is visible rather than inferred, and reusable-by-fingerprint is
exactly why that matters.

### A VM being created is on the page too

A row appears as `building` **before** the create is attempted, not once it
returns. That window is the longest silent stretch of a job — an iroh dial to the
runner, then a `POST /sandbox-deploy` that waits out `BOOT_TIMEOUT` (five
minutes) for a cold machine — and while it was unrecorded, `/vms` said "Nothing
is pooled" and the job row still said `queued` for the whole of it. A build that
was booting, a build whose VM creation was failing and being retried, and a run
nothing had picked up were three different things that all looked like nothing
happening.

The row is keyed on `building-<job_id>` because the daemon has not assigned a
sandbox id yet. **Nothing may treat that as one**: every query that reaches a
daemon filters on status, and `take_one_for_sweep` — the only one that took
anything not `claimed` — excludes `building` explicitly, so the page offers no
Destroy button and the server would refuse it anyway. The creating job clears the
row either way: `register` replaces it with the real one on success, and it is
deleted on failure. It is leased like a claim and renewed on the same timer, so a
slow boot is not swept out from under itself; a process that dies mid-create
stops renewing and the row is deleted by the loop that reclaims expired claims.
The half-created sandbox, if there is one, is left to its TTL, as it was before.

A job reaching a runner is also recorded before it has a VM. `ci_job.status`
becomes `running` with `runner_hd_id` set as soon as a consumer commits to the
job — `sandbox_id` and `fingerprint` stay null until there is a machine, which is
the honest reading. Two things depended on that being wrong: the run page showed
a build that was busy booting as not started, and `fail_jobs_waiting_for_a_runner`
could not tell an unclaimed job from one a live host was working on, so it would
fail the build and blame a host that was online. And each failed delivery now
writes its reason to the job row instead of only the fourth and last one — the
redelivery ladder is 60s, 5 minutes, then 15, so a workflow naming a VM image its
host does not have used to show an empty error for twenty minutes before anything
said why.

Cleanup destroys a single VM, or every idle one whose last run failed. Both go
through `take_for_sweep`'s pattern: the row is marked `draining` first, so a
concurrent claim cannot hand out a machine that is about to be killed, and it is
only forgotten once the daemon confirms the sandbox is gone — a row removed while
the VM survives is a VM nothing will ever clean up again. **A claimed VM is
refused**, in the query rather than only in the page, so cleaning up cannot fail
a live build from underneath. Everything is scoped to the hosts this instance
serves, for the same reason the sweep always was.

`ci_vm_pool.last_job` is kept after a claim is released. `claimed_by_job`
answers "who holds this now" and is nulled on release; it cannot answer "which
run left this behind", which is the question cleanup is asking.

The pool table survives a restart. Without it a crash orphans every VM until its
TTL, and the next run builds a second pool beside the one already sitting there.

### A claimed VM is held by a lease, not by a job

Each instance takes a **random id at startup** and stamps it, with an expiry, on
every VM it claims — renewing on a timer while it holds them. Reclaim keys on
that expiry.

The obvious alternative does not work, and this is the bug it caused: asking
whether the *job* is still `running` cannot distinguish "another instance is
running it" from "the process that was running it died". A restart leaves the row
`running` either way, so reclaim had to leave the VM alone — an orchestrator
could not take back even its own VMs. They stayed `claimed` until the sandbox TTL
reaped them (`CI_VM_TTL_SECONDS`, an hour by default), and the row leaked until
some later restart happened to find the job terminal.

A lease is a fact about the holder rather than an inference from the work. Three
properties follow:

- **A restarted instance reclaims its own previous life**, because the id is
  fresh per process — a stable one would inherit the dead process's leases and
  reclaim nothing.
- **An instance never reclaims what it is holding**, whatever the clock says. A
  slow database must not make a process fight itself; two dispatchers on one
  sandbox is far worse than a VM reclaimed a minute late.
- **Reclaim runs on a timer, not only at startup**, so a dead sibling's VMs come
  back within a lease period instead of waiting for somebody to restart this one.

`uses: default` resolves through **`~/.heyo/daemon.json`** — heyvmd mints
`backend_id` there on first start and registers and heartbeats under it, so it is
the identity the cloud knows the machine by, and it is the same file
`heyvm network add-host` reads. The daemon's `/daemon/name` route is *not* the
authority: it returns `backend_server_id`, a different field fed by the
`BACKEND_SERVER_ID` environment variable, and trusting it pins jobs to an id the
cloud may have no live registration for — a queue with no consumer beside a
daemon that is perfectly healthy. `CI_DEFAULT_NODE` overrides everything.

`CI_VM_LEASE_SECS` (default 180) is the window between an instance dying and its
VMs becoming reclaimable; renewal runs at a third of it.

The same loop **renews the sandbox TTL of every VM it is running a job on**, and
that is a fix rather than a nicety: `CI_VM_TTL_SECONDS` defaults to an hour and
`CI_MAX_JOB_SECONDS` to four, while the TTL was only ever set at creation and
touched again when a VM was claimed or released. A build longer than the TTL had
its machine reaped mid-step, surfacing as a daemon error on a job doing nothing
wrong.

Only *claimed* VMs are kept alive. An idle one in the warm pool is meant to age
out — that is what the TTL is a backstop for, and renewing those would mean
nothing this app creates ever expires. It works while a step is running because
`Vm::renew_ttl` does not take the sandbox lock that `exec` holds; if it did, the
keepalive would queue behind the build it exists to protect.

## Workflow objects

```bash
serverctl create workflow build \
  --repo git@github.com:me/app.git \
  --network prod-runners \
  --path '.ci/workflows/*.yml'

serverctl get workflows
```

Stored by app-lb, polled by `ci`. An object points at a repository and a path
glob — the workflow itself lives in the repository it builds, versioned with the
code, so the object is a pointer rather than a copy that can drift.

Objects are matched on the **repository**, because `git submit` knows what it is a
clone of but not what somebody named the object; `git@github.com:me/app.git` and
`https://github.com/me/app` match. Several objects may name one repository —
`build` and `nightly` with different globs is legitimate — and each gets its own
runs, because each is an independent answer to "did this commit pass".

Without `CI_APP_LB_URL`, submits fall back to `CI_WORKFLOW_PATH` and the system
works with no objects at all.

## Secrets

`${{ secrets.X }}` and `${{ vars.X }}` resolve from heyosecret under
`ci/<workflow>/<environment>/`.

**This process is the policy layer, because heyosecret has none.** Its token can
read, write and revoke every secret at every path; `readAccess`/`writeAccess` are
stored and returned but never enforced. So there is no configuration of
heyosecret that makes handing its token to a build safe. The orchestrator holds
it, resolves what a workflow is entitled to, and injects only the values.

heyosecret makes no secret/variable distinction, so the convention is its
`tags[]`: an entry tagged `public` becomes `vars.*` and is left in plain text;
everything else becomes `secrets.*` and is **masked on the write path** — before
a log line is persisted or streamed, so a secret never reaches disk in plain text
for someone to find later.

## Deploying it

A **static `proxy_pass` deployment with an `update` block**, like app-obs — see
`deploy/ci.json`. The orchestrator holds long-lived iroh tunnels and a Postgres
pool, and app-lb's update flow re-probes upstreams after the commands run, so "it
exited 0 but never came back" is a failed deploy rather than a green one.

```bash
serverctl apply -f deploy/ci.json
serverctl update ci
```

Identity comes from app-lb: `x-auth-request-user` (the stable Google `sub`, and
the primary key), `-email`, `-name`. app-lb strips those unconditionally before
setting them, so they are trustworthy — but only on a gated deployment.

**An app-lb gate admits browsers and nothing else.** The split is
`Accept: text/html`, so curl, `git submit`, *and a page's own `EventSource`* all get
`401 {"error":"authentication required"}`. Hence `public_paths` covers
`/api/submit` (which verifies its own HMAC) and `/api/stream/` (which carries a
short-lived, job-scoped token minted by the page that opens it — and that page
was fetched through the gate).

app-lb has no roles, so `ci` keeps its own `ci_user` table keyed on the subject,
seeded from `CI_ADMIN_EMAILS`. Promotion from that list is sticky; dropping off
it does not demote, so a role granted in the UI survives an env change.

## Design notes

### Steps do not use the SDK's exec

`heyo-sdk`'s `Commands::run` posts `{command, cwd, env}` and never sends
`timeout_secs` — `CommandRunOptions::timeout` bounds the *HTTP client*, not the
guest. The firecracker serial path then caps every command at 30 seconds, which
no build survives. So steps go through the daemon's own
`POST /sandboxes/{id}/exec-operations`, which does take a guest timeout.

That route is also **idempotent by `operationId` and persisted**, and step ids are
derived from the run and job key rather than minted. So a JetStream redelivery
re-posts the same step and *reattaches* to the operation already running instead
of building twice.

### Source reaches the guest through exec, in chunks

Neither `Files::write` nor the daemon's upload route reaches a Firecracker guest:
both write into a host-side *mount*, which a sandbox does not have — the call
fails with `Mount not found: /workspace (available mounts: [])`. They also cap at
10 MB. Exec is the only transport that works on every backend.

The chunk size is measured, not chosen. The daemon renders a command as
`env … sh -lc '<script>'`, so the script is one argv entry and Linux's
`MAX_ARG_STRLEN` bounds it. Probed against a real guest: 32 KiB succeeds, 128 KiB
returns `bash: /usr/bin/env: Argument list too long`.

Anything reading *out* of a guest must end its output with a newline. The serial
path frames output with newline-delimited markers, so `base64 -w0` — one
unterminated line — hangs the operation in `running` forever.

### Job subjects are sharded per runner

`WorkQueue` retention deletes on ack, so the stream's depth *is* the backlog. The
cost is that JetStream permits only one consumer per subject, which is why
queue-fn documents itself as single-instance. Sharding the subject by runner
sidesteps it: one durable consumer per runner, filters that never overlap, and
several orchestrators can run at once as long as they own disjoint runner sets.

Two disjoint spaces, and the `r`/`n` segment is load-bearing:

```
<prefix>.job.r.<runner_id>     pinned to one host
<prefix>.job.n.<network_id>    any online host in that network
```

Without it, a network named like a runner would produce overlapping filters and
two consumers would silently eat each other's work.

**Postgres commits before NATS does, so the publish has a rollback.**
`queue_job` moves a job `pending → queued` and only then publishes; a failure in
between would leave a row claiming to be queued with nothing on the queue, and
nothing reconciling the two — a job that never runs and never errors. So a failed
publish returns the job to `pending`, which makes the scheduler's own retry the
repair. `Nats-Msg-Id` collapses a duplicate, so retrying is safe even when the
message did land after all.

One publish failing does not abort the rest: the run's other jobs are still
scheduled, because one unreachable subject should not hold up a whole run.

`advance_run` is otherwise driven only by a submit and by jobs finishing, so a
run whose jobs *all* failed to publish would have nothing left to nudge it. The
lease loop re-runs the scheduler for any active run with a pending job —
idempotent by construction, since `queue_job` only moves a job that is still
pending and a job waiting on `needs:` simply is not ready.

A queue message carries **ids only**. The expanded plan lives in `ci_job.plan`,
so a redelivery runs exactly what the original delivery would have, even if the
branch moved underneath it.

### Connecting to it

```bash
CI_NATS_URL=nats://127.0.0.1:4222      # comma-separated for a cluster
CI_NATS_SUBJECT_PREFIX=ci              # namespaces both streams and every
                                       # subject and durable consumer name
```

The prefix is interpolated verbatim, so it is charset-checked at startup —
`[A-Za-z0-9_-]`. Two installations sharing a NATS server need different prefixes
or they share a work queue.

**Four ways to authenticate, and exactly one may be set.** They are not a
precedence order: naming two is a startup error, because guessing which an
operator meant is how a process authenticates as the wrong principal.

| | |
|---|---|
| `CI_NATS_USER` + `CI_NATS_PASSWORD` | Both or neither. A user alone is not a token, and either half alone is refused rather than guessed at. |
| `CI_NATS_TOKEN` | A bare token. |
| `CI_NATS_CREDS` | Path to a `.creds` file, read at startup. An unreadable path is a startup error, not a fallback to anonymous. |
| `CI_NATS_NKEY` | An nkey seed. |

Userinfo in the URL (`nats://user:pass@host`) still works and is the last
resort: any of the above overrides it, and startup **warns** when a credential
arrives that way, because a URL is visible in shell history, process listings and
container specs. The credential lives in `nats_auth::NatsEndpoint`, which has a
hand-written `Debug` that cannot print it — so a `{:?}` on `Config` cannot leak a
password. Only the sanitized server list is ever logged.

One trap specific to this deployment: the NATS **system** account is not a
substitute for a real one. JetStream cannot be enabled on it, so a `sys` login
connects successfully and then fails on the first stream.

### `ack_wait` is short, and the job says it is still running

A dispatcher extends its claim on a message with `AckKind::Progress` every 20
seconds for as long as a job runs, so `ack_wait` is 60 seconds rather than a
ceiling derived from `CI_MAX_JOB_SECONDS`. A build of any length is safe while
its dispatcher is alive, and one that *dies* releases its job in about a minute
instead of holding it for the whole job budget.

**`backoff[0]` and `ack_wait` are one setting.** nats-server overrides `ack_wait`
with the first entry of the backoff ladder whenever a ladder is set — verified
against a live server, not inferred. The ladder here used to start at one second
while `ack_wait` was configured as four hours, so the configured value was
discarded and every running job became eligible for redelivery a second after it
started; with `max_deliver` at four, a healthy build could burn all four
deliveries while doing nothing wrong, leaving a dispatcher that died with no
redelivery left to recover it. A unit test now pins the two together and an
integration test pins the server's behaviour.

Binding also **reconciles an existing consumer**: JetStream returns the durable
that is already there and ignores the config passed with it, so an upgrade would
otherwise keep the old window and none of this would take effect.

### Migrations

`migrations/*.sql` are re-executed on every startup with no tracking table —
heyosecret's approach. Every statement must be idempotent; additive changes are
`ALTER TABLE … ADD COLUMN IF NOT EXISTS`, because the `CREATE TABLE` above is a
no-op once the table exists.

Two things make that actually safe. **A Postgres advisory lock**, because
`CREATE TABLE IF NOT EXISTS` is *not* concurrency-safe — two sessions both find
the table absent and the loser dies on `pg_type_typname_nsp_index`, and two
instances starting together is normal here. And **a `lock_timeout` with retries**,
because `ALTER TABLE` needs `ACCESS EXCLUSIVE` on a table a live dispatcher is
inserting into; without a bound, a rolling deploy hangs a starting instance
behind a long build.

### An iroh ticket is bearer-equivalent

`mvm-ctrl/docs/cross-machine-hardening.md` is explicit: the `hey-proxy/tcp/0`
ALPN accepts any peer that knows the ticket, and the daemon cannot verify the
peer. A runner daemon with no `JWT_SECRET` therefore hands a host shell to
anyone who has seen a ticket that may have transited a log. Every tunnel is
probed once, unauthenticated, and a daemon that answers is refused —
`CI_ALLOW_UNAUTHENTICATED_RUNNERS=true` downgrades that to a warning for a
local-only loop.

### Storage

Postgres for runs, jobs, steps, artifacts and the pool; **step logs go to disk**
with the path and byte count on the row. A build log is megabytes, and putting it
in a column means every listing query drags all of it across the wire.

**A run's page also carries each job's VM log** — the machine's own console as
its daemon saw it, read from `GET /sandboxes/{id}/logs` and captured *before* the
VM is released, because a job with `reuse: false` destroys it on the next line.
It is recorded as a step at index `-2`, the same trick checkout uses at `-1`: it
needs a row, a file and a place in the UI, and a step is already all three. That
also puts it inside the retention sweep rather than somewhere the sweep would
miss. Capturing it never fails a job — by then the steps have decided the
outcome, and a green build must not go red because a diagnostic could not be
fetched.

**Logs are swept after `CI_LOG_RETENTION_DAYS`, default 2.** Nothing else prunes
them, and an orchestrator that fills its disk stops being an orchestrator. The
sweep is driven by rows rather than by walking the directory, so it converges —
once a run's paths are nulled it is not offered again — and it is batched, since
a first pass over months of history would otherwise be one burst of unlinks on
the disk a build is writing to. **The rows stay**: a step that ran and its exit
code are the run's history, and dropping those with the bytes would make an old
run look as though it never happened. `CI_LOG_RETENTION_DAYS=0` keeps everything,
which is a decision about disk somebody should make on purpose.

## Tests

```bash
cargo test                                    # unit; no services needed

CI_TEST_DATABASE_URL=postgres://…/ci_test \
CI_TEST_NATS_URL=nats://127.0.0.1:4222 \
  cargo test -- --ignored --test-threads=1    # integration
```

The integration tests want Postgres, NATS with JetStream, and a local `heyvmd`.
The end-to-end test boots a real VM, runs a workflow, proves the VM is reused on
a second run and rebuilt when a `cache_key_files` entry changes, then destroys
it. `CI_TEST_DRIVER=kvm` switches drivers; firecracker is the default because
`kvm` re-execs the daemon's own binary and fails whenever that path has been
rebuilt.

Leftover streams from a run that was killed mid-test:

```bash
CI_TEST_STREAM_PREFIXES=citest cargo test -- --ignored delete_leftover
```

## Status

Working: workflow parsing and planning (matrix, `needs`, `if`, `max-parallel`),
branch and path filters with the `changed()` condition, runner discovery, the VM
pool, the job queue, `git submit` with per-repository tokens, secrets with
masking, disk and `artifacts` sinks, the dashboard with live logs, and workflow
objects.

Not built yet:

- **The S3 artifact sink.** Declared and selectable; fails loudly naming the
  alternatives rather than reporting an artifact stored that is not there.
- **Composite `uses:` actions.** Only `ci/upload-artifact` is built in. Fetching
  an `action.yml` from a repository is a different feature with a different trust
  model.
- **Triggers other than `submit`.** `on: [schedule]` parses and is reported as
  unsupported rather than silently ignored.
- **`serverctl set workflow`.** Create, get and delete exist; editing means
  re-creating with the same id.
