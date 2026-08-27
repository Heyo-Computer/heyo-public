# CI for this monorepo

Workflows for [`ci`](https://ci.us2.heyo.work), the heyvm-backed orchestrator.
Each file under `.ci/workflows/` is an independent run and an independent answer
to "did this commit pass"; every file here holds one job. See
[One file or two?](#one-file-or-two) below for why.

| Workflow | Job | Builds | Artifact |
| --- | --- | --- | --- |
| `codegraph.yml` | `release` | `codegraph` | `codegraph` — binary, `SHA256SUMS`, `BUILD-INFO` |
| `app-lb.yml` | `release` | `app-lb`, `heyctl` | `app-lb` — both binaries, `app-lb.conf`, `SHA256SUMS`, `BUILD-INFO` |
| `app-obs.yml` | `release` | `app-obs`, `app-obs-dump` | `app-obs` — both binaries, `app-obs.conf`, `SHA256SUMS`, `BUILD-INFO` |
| `ci.yml` | `release` | `ci` | `ci` — binary, `ci.conf`, `migrations/`, `SHA256SUMS`, `BUILD-INFO` |

The other six crates here (`artifacts`, `computer`, `heyosecret`,
`heyosecret-client`, `orchestrator`, `printer`) have no workflow yet. Adding one
is the recipe at the bottom.

[`install.sh`](install.sh) is the other end of that table: it pulls the newest
of each back out of the store and installs it. See
[Installing what these build](#installing-what-these-build).

`ci.yml` builds the orchestrator that reads these files. It only builds — the
artifact is deployed by hand or by `ci/deploy/ci.json`'s `update` block, never by
the run itself — and a restart mid-run is redelivered off the JetStream work
queue, so the self-reference costs less than it looks like it should.

## One file or two?

One buildable thing per file, and one job per file. `app-lb.yml` and
`app-obs.yml` used to be two jobs of one `apps.yml`, on the theory that the load
balancer and the observability service it feeds are one deployed thing and "did
the platform build" deserves one run page. It was the wrong trade, and the
reasons are worth knowing before you put two jobs in one file here:

- **A shared run page is a shared log.** app-obs is the slowest and most
  fault-prone build in the repository, and every one of its failures sat on a
  page under a 75-minute app-lb build. Debugging one thing meant scrolling past
  another.
- **Ordering became a tax.** The two jobs were pinned to one host, so the file
  ran app-obs `needs: [app-lb]` to keep two big builds off the same cores at
  once. Every attempt to reproduce an app-obs fault therefore waited for app-lb
  first. A `needs` cannot cross files, so the split gives that ordering up —
  a commit touching both now runs both side by side, and each is slower for
  it — in exchange for `git submit --only app-obs` running one build alone.
- **Two-level gating is a footgun.** A multi-job file gates on `paths:` and
  again on each job's `if: changed(...)`, and the second list has to repeat the
  shared image and the workflow file or editing the Dockerfile starts the
  workflow and skips every job in it. One job per file needs only `paths:`.

What the two files still share is `.ci/image/apps/`, because their system
requirements are the same set, and a second Dockerfile would mean a second
10-to-20-minute image build on every runner for byte-identical contents. Every
field under `vm.build` — Dockerfile, context, `size_mb` — must stay equal in
both files, because those are what the image is named by. They get separate
warm VM pools regardless: the pool fingerprint includes `size_class`,
`disk_size_gb` and `cache_key_files`, and those differ. The workflow and job
names are *not* in the fingerprint, so the rename did not retire anybody's
warm VM.

Put jobs in one file only when they genuinely answer one question *and* one
would never be run without the other — a build and the test of that build,
say. Independent binaries get independent files.

## Installing what these build

```sh
ART_URL=https://art.us2.heyo.work ART_API_KEY=… sh .ci/install.sh
```

That installs `app-lb`, `app-obs` and `ci` — binaries into `/usr/local/bin`,
supervisor programs into `/etc/supervisor/conf.d`, and `ci`'s migrations into
`/var/lib/ci/migrations`, which is where the shipped `ci.conf` points
`CI_MIGRATIONS_DIR`. Nothing is restarted unless you pass `--restart`.

```sh
sh .ci/install.sh --list            # what the store has, and what each thing is
sh .ci/install.sh --dry-run         # fetch and verify, install nothing
sh .ci/install.sh app-lb            # just one
sh .ci/install.sh --ref ci-app-lb-019fca…-release-app-lb app-lb   # roll back
PREFIX=~/.local SUPERVISOR_DIR= sh .ci/install.sh   # unprivileged, no units
```

**How it finds anything.** `ci` flattens an artifact's coordinates into one tag,
because the store's tag charset has no `/`:

```
ci-<workflow>-<run>-<job>-<name>
```

which for the table above is `ci-app-lb-<run>-release-app-lb`,
`ci-app-obs-<run>-release-app-obs`, `ci-ci-<run>-release-ci` and
`ci-codegraph-<run>-release-codegraph`. The workflow name being in the tag
means renaming a workflow starts a new series: app-lb and app-obs were built
by `apps.yml` until 2026-08-27, and their older builds are tagged
`ci-apps-<run>-app-lb-app-lb` and `ci-apps-<run>-app-obs-app-obs`. The script
falls back to those only while nothing has been published under the new
name, and `--ref` takes either. The run id is `%012x-%08x` — epoch
milliseconds in hex, then a sequence — so it is fixed-width and zero-padded, and
**sorting the tags lexicographically sorts them chronologically**. That is the
one load-bearing coincidence in the whole script, so it matches the exact hex
widths rather than a wildcard: a tag that does not have them is not considered
at all, instead of sorting somewhere arbitrary.

**What it checks.** A blob's name *is* the sha256 of its bytes, so verifying the
download against the digest it was fetched by is free, and it covers the
transfer and the store in one comparison. Then `sha256sum -c SHA256SUMS` runs
inside the unpacked tree — the build's own statement about what it produced. A
mismatch on either refuses that app and names both hashes.

**What it will not do.** It never overwrites a supervisor config. Those hold
edited secrets — the shipped `app-lb.conf` carries
`APP_LB_DASHBOARD_PASSWORD="change-me"` and every real deployment has changed it
— so an existing file is kept and the new one written beside it as
`<name>.conf.new`, with a warning that a new setting will not apply until you
merge it.

The `description:` each workflow sets on its upload is what `--list` prints, and
it is stored as an artifact *label* rather than a manifest annotation — a
manifest is addressed by its own hash, so wording inside one would fork the
manifest for byte-identical builds the first time somebody edited it.

## Submitting

From the **repository root**, not from a subdirectory:

```bash
git submit
```

No `--archive`. This repository's whole history bundles to **3.2 MB** against a
64 MB `CI_MAX_SOURCE_BYTES`, so the default payload fits easily — which is worth
stating because the private `heyo` monorepo is 322 MB and cannot. Two things
follow from being on the good side of that line:

- **`.git` exists in the guest.** A bundle is cloned inside the VM by the
  checkout step, so `git describe`, `git log` and `git rev-parse` work in a
  step. `codegraph.yml` uses that for its version stamp.
- **Path filters actually filter.** `ci` reads a submit's changed-file set out
  of the bundle's own history (`git diff --name-only <before> HEAD`). A commit
  touching only `app-lb/` starts no `codegraph` run at all.

If you ever do submit with `--archive`, both go away: a tarball has no history,
so the change set is *unknown*, and an unknown change set **matches every filter
by design** — the alternative is skipping a build for a commit nobody proved was
irrelevant, which is a green tick on work that never ran. That is the safe
direction, not a bug; `${{ ci.changes_reason }}` says which case a run is in.

## Adding a workflow

Copy `codegraph.yml`, or `app-obs.yml` if the build is big enough to need a
log file and an in-guest timeout. Then change these:

1. **`paths:`** — the crate's subtree, **plus every path dependency of it**.
   Cargo does not record a path dep's contents in the lockfile, so a sibling
   crate's source is a change to your build even though nothing in your own
   directory moved. `codegraph` has none, which is why its list is short; most
   of the others here do.
2. **`cache_key_files:`** — the lockfile, plus every `Cargo.toml` that can change
   what gets compiled without moving the lockfile: sibling path deps, and
   **workspace members**. `heyctl` is a member of the `app-lb` workspace, so
   cargo records its version in `Cargo.lock` but not its feature list; the root
   manifest carries `[workspace.dependencies]` for the same reason. These are
   hashed to fingerprint the warm VM — get them wrong and a run reuses a cache
   resolved against different dependencies. List `rust-toolchain.toml` even when
   there is none: a missing file hashes to an explicit marker, so adding one
   later busts the pool at the moment it most needs busting.
3. **`working-directory:` on the build step — absolute.** Unlike GitHub, `ci`
   renders `cd <value> && …` verbatim rather than joining it onto the workspace,
   so a bare `codegraph` resolves against the login shell's home directory and
   the step dies on `cd: no such file`. Write `/workspace/codegraph`.
4. **`cargo build` flags.** `--locked`, always, because the VM's fingerprint is
   a claim about the lockfile. `--bins` to skip test and example targets. And
   `--workspace` **if the crate has members** — without it cargo builds only the
   root package, so a member like `heyctl` silently never gets built and the
   artifact is quietly missing a binary.
5. **The image**, if the toolchain differs. There are three here and none was
   guessed — each package was read off `cargo tree`:
   - `image/codegraph/`: `build-essential` and `git` only. Nothing in that graph
     links a system library.
   - `image/apps/`: adds `cmake`, `perl` and `pkg-config`, because both app
     graphs reach `aws-lc-sys` (cmake) and app-lb also builds OpenSSL and
     zlib-ng from source (perl, cmake).
   - `image/ci/`: adds `pkg-config` and **`libssl-dev`** — the counterexample
     to the trap below. Nothing in that graph vendors OpenSSL, so it links
     the distribution's copy. It has no `cmake` or `perl`, because the only
     build-adjacent crates it reaches are `cc` and `pkg-config`.

   The trap worth knowing: **two images here link OpenSSL and only one installs
   `libssl-dev`.** app-lb does not need the distribution's copy, because
   `pingora-openssl` turns on `openssl/vendored` and `openssl-src` compiles
   OpenSSL into the binary — which is also the only reason `perl` is there.
   `ci` does need it: nothing in its graph vendors OpenSSL, and there is no
   `openssl-src` entry in `ci/Cargo.lock` to prove it. So neither "it uses
   OpenSSL, add libssl-dev" nor its opposite is the reflex — read the lockfile
   for `openssl-src` and let that decide. An unnecessary package costs image
   build time on every runner and hides what the build actually depends on; a
   missing one fails the link at the very end of a cold build.
6. **The `.ci/` entries in `paths:`** — name your own workflow and image
   directory rather than `.ci/**`, so adding a workflow for another crate does
   not rebuild yours. If you added a job to an existing file, add them to that
   job's `if:` too.
7. **`size_class`, and `ttl_seconds` if the build is long.** The classes are
   micro/mini/small (1 CPU) through medium (2 CPU, 4 GB), large (4/8) and
   xlarge (8/16). Pick on **memory**, not crate count: `app-obs` has fewer
   crates than `app-lb` and needs a class more, because datafusion and arrow
   generate code that makes individual rustc invocations enormous. There is no
   swap in a microVM, so a build that wants more is OOM-killed rather than slow
   — `CARGO_BUILD_JOBS` in the job's `env` is the knob that trades wall-clock
   for headroom.

   Set `ttl_seconds` on anything whose `timeout-minutes` exceeds 60. The default
   VM TTL is `CI_VM_TTL_SECONDS` (3600), and whether the orchestrator's renewal
   loop can extend it depends on the runner's heyvmd being new enough — older
   builds computed it as `started_at + ttl`, an uptime cap that renewal could
   not move, and a build past it was reaped mid-compile. A create-time TTL is
   honoured either way.

   Also worth remembering: a step with no `timeout-minutes` gets 30 minutes,
   which a cold build will blow through long before the job timeout matters.

`ci` builds the image itself, on whichever runner the job lands on, from the
hash of the Dockerfile plus its context plus `size_mb`. An unchanged image is
reused and any edit builds a new one, so there is nothing to bump and nothing to
run by hand on a runner.

Both the `path` in `ci/upload-artifact` and everything in `cache_key_files` are
relative to the **repository root**, not to the step's working directory.
`CARGO_TARGET_DIR` points outside the workspace entirely (`/var/cache/ci/target`,
on the data disk) because `ci` wipes `/workspace` on every checkout — so a
binary has to be copied back in before it can be uploaded.
