# CI for this monorepo

Workflows for [`ci`](https://ci.us2.heyo.work), the heyvm-backed orchestrator.
One file per buildable thing under `.ci/workflows/`; each file is an independent
run and an independent answer to "did this commit pass".

| Workflow | Builds | Artifact |
| --- | --- | --- |
| `codegraph.yml` | `codegraph` release binary | `codegraph` — the binary, `SHA256SUMS`, `BUILD-INFO` |

The other eight crates here (`app-lb`, `app-obs`, `artifacts`, `computer`,
`heyosecret`, `heyosecret-client`, `orchestrator`, `printer`) have no workflow
yet. Adding one is the recipe at the bottom.

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

Copy `codegraph.yml` and change five things:

1. **`paths:`** — the crate's subtree, **plus every path dependency of it**.
   Cargo does not record a path dep's contents in the lockfile, so a sibling
   crate's source is a change to your build even though nothing in your own
   directory moved. `codegraph` has none, which is why its list is short; most
   of the others here do.
2. **`cache_key_files:`** — the lockfile, plus any sibling `Cargo.toml`, for the
   same reason. These are hashed to fingerprint the warm VM: get them wrong and
   a run reuses a cache that was resolved against different dependencies. List
   `rust-toolchain.toml` even when there is none — a missing file hashes to an
   explicit marker, so adding one later busts the pool at the moment it most
   needs busting.
3. **`working-directory:` on the build step — absolute.** Unlike GitHub, `ci`
   renders `cd <value> && …` verbatim rather than joining it onto the workspace,
   so a bare `codegraph` resolves against the login shell's home directory and
   the step dies on `cd: no such file`. Write `/workspace/codegraph`.
4. **The image**, if the toolchain differs. `image/codegraph/` is a minimal Rust
   rootfs — `build-essential` and `git`, nothing else, because `cargo tree`
   reaches no system library. A crate that speaks HTTP needs `pkg-config` and
   `libssl-dev` (reqwest's default TLS links OpenSSL rather than bundling it);
   one that reaches `aws-lc-sys` needs `cmake` and `perl`. Read it off
   `cargo tree`, do not guess: an unnecessary package costs image build time on
   every runner and hides what the build actually depends on.
5. **The `.ci/` entries in `paths:`** — name your own workflow and image
   directory rather than `.ci/**`, so adding a workflow for another crate does
   not rebuild yours.

`ci` builds the image itself, on whichever runner the job lands on, from the
hash of the Dockerfile plus its context plus `size_mb`. An unchanged image is
reused and any edit builds a new one, so there is nothing to bump and nothing to
run by hand on a runner.

Both the `path` in `ci/upload-artifact` and everything in `cache_key_files` are
relative to the **repository root**, not to the step's working directory.
`CARGO_TARGET_DIR` points outside the workspace entirely (`/var/cache/ci/target`,
on the data disk) because `ci` wipes `/workspace` on every checkout — so a
binary has to be copied back in before it can be uploaded.
