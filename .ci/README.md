# CI for this monorepo

Workflows for [`ci`](https://ci.us2.heyo.work), the heyvm-backed orchestrator.
Each file under `.ci/workflows/` is an independent run and an independent answer
to "did this commit pass"; a file holds one job, or several when they answer that
question together. See [One file or two?](#one-file-or-two) below.

| Workflow | Job | Builds | Artifact |
| --- | --- | --- | --- |
| `codegraph.yml` | `release` | `codegraph` | `codegraph` — binary, `SHA256SUMS`, `BUILD-INFO` |
| `apps.yml` | `app-lb` | `app-lb`, `heyctl` | `app-lb` — both binaries, `app-lb.conf`, `SHA256SUMS`, `BUILD-INFO` |
| `apps.yml` | `app-obs` | `app-obs`, `app-obs-dump` | `app-obs` — both binaries, `app-obs.conf`, `SHA256SUMS`, `BUILD-INFO` |

The other six crates here (`artifacts`, `computer`, `heyosecret`,
`heyosecret-client`, `orchestrator`, `printer`) have no workflow yet. Adding one
is the recipe at the bottom.

## One file or two?

`codegraph.yml` is one buildable thing in one file. `apps.yml` is two, and the
split is worth understanding before you copy either shape.

Put jobs in **one file** when they answer one question. `app-lb` and `app-obs`
are the load balancer and the observability service it feeds; "did the platform
build" is a sentence somebody says, so one run page is the right place to read
it. Put them in **separate files** when the answers are independent —
`codegraph` passing or failing tells you nothing about the platform.

Sharing a file must not mean sharing a rebuild, so `apps.yml` gates twice:

- `on: submit: paths:` decides whether the workflow runs at all.
- each job's `if: changed(...)` decides whether that job runs.

A commit touching only `app-obs/` starts the workflow, skips the `app-lb` job,
and the run still goes green — a run of nothing but skipped jobs is a success.
`changed()` runs the same glob matcher as `paths:`, so a job's guard and the
workflow's filter cannot disagree about what a pattern covers, and both are true
when the change set is unknown.

One thing to watch when copying this: each job's `if:` has to list the shared
image and the workflow file alongside its own subtree. Without that, editing the
Dockerfile starts the workflow and then skips every job in it — the one change
that must rebuild everything.

The two jobs share `.ci/image/apps/` because their system requirements are the
same set, and a second Dockerfile would mean a second 10-to-20-minute image
build on every runner for byte-identical contents. They still get separate warm
VM pools: the pool fingerprint includes `cache_key_files`, and those differ.
Both `vm.build.size_mb` values must stay equal, because that number is part of
the hash that names the image.

## Submitting

From the **repository root**, not from a subdirectory:

```bash
git submit
```

No `--archive`. This repository's whole history bundles to **2.7 MB** against a
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

Copy `codegraph.yml` for a standalone crate, or a job out of `apps.yml` if the
new binary belongs with something already built here. Then change these:

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
5. **The image**, if the toolchain differs. There are two here and neither was
   guessed — each package was read off `cargo tree`:
   - `image/codegraph/`: `build-essential` and `git` only. Nothing in that graph
     links a system library.
   - `image/apps/`: adds `cmake`, `perl` and `pkg-config`, because both app
     graphs reach `aws-lc-sys` (cmake) and app-lb also builds OpenSSL and
     zlib-ng from source (perl, cmake).

   The trap worth knowing: **`libssl-dev` is not in either image, and app-lb
   links OpenSSL.** It does not need the distribution's copy, because
   `pingora-openssl` turns on `openssl/vendored` and `openssl-src` compiles
   OpenSSL into the binary — which is also the only reason `perl` is there. So
   "it uses OpenSSL, add libssl-dev" is the wrong reflex; check whether
   something in the graph already vendored it. An unnecessary package costs
   image build time on every runner and hides what the build actually depends
   on.
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
