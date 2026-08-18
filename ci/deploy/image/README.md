# The build image

A Firecracker rootfs with a Rust toolchain, which is what `.ci/workflows/build.yml`
runs in.

**`ci` builds this itself, through the runner's daemon.** The workflow points
`vm.build.dockerfile` at this file, and the first job to land on a host uploads
it (plus the build context) to the daemon's `POST /images/build`, which runs
the same `docker build → docker export → mke2fs` pipeline `heyvm mvm build`
runs, installing the result into that host's image catalog as
`ci-img-<hash of this file, its context and size_mb>`. Later jobs boot straight
from it; editing this file names a new image, so the next run rebuilds — and
the host's docker layer cache makes that rebuild incremental. Nothing has to be
run by hand on a runner. The runner host needs `docker`, `mke2fs` and (when
heyvmd is not root) `fakeroot`, which the build checks for and names if absent.

That is a change. It used to be built out of band:

```bash
heyvm mvm build --local-only -f deploy/image/Dockerfile -c deploy/image \
    -n ci-rust --size-mb 6144
```

— on every machine that would ever run the job, because `ci` passed `vm.image`
straight to the daemon and the daemon resolves a bare name against
`~/.heyo/images/firecracker/{name}.ext4`. A host where that command had never
been run failed every job at VM creation with *"not found locally and no public
base image with that name"*, and nothing recorded the attempt, so it looked
identical to a run no runner had picked up. The command still works and is still
the way to make an image by hand; `heyvm mvm images` lists what a host has.

## What `ci` honours in this file

Everything docker does — the build *is* `docker build`, run by the daemon on
the runner host, so multi-stage builds, `COPY --from=`, `ADD`, `ARG` and
`.dockerignore` all behave exactly as they do locally. `ci` does not parse the
Dockerfile at all; it hashes the bytes and ships them.

What still does not survive is what `docker export` has never carried: **OCI
metadata**. `ENV`, `CMD` and `ENTRYPOINT` build fine and then vanish from the
rootfs, which is why this file writes its environment to `/etc/profile.d` in a
`RUN` and why the VM boots `init=/init.sh` (the kernel's `init=` parameter, not
an entrypoint). An image without an `/init.sh` that prints `HEYVM_READY` builds
successfully and then fails every boot.

## Sizing the rootfs

`vm.build.size_mb` in the workflow is the old `--size-mb`, carried through to
the same `mke2fs` call: this workflow sets `6144`, because the crate registry
under `CARGO_HOME` grows into the rootfs at runtime and the auto-size
(exported tree × 1.2 + 64 MB) would leave it no headroom. It is part of the
image fingerprint — changing it builds a new image. Too small does **not**
show up as anything about the image or as a readable "no space" — a full
filesystem under rustc's mmap'd output is a SIGBUS. Leave headroom.
(Measured August 2026: the built image uses ~1GB, so 6144 leaves ~4.7GB.)

## What the pipeline discards, and what that forces

`docker export` flattens a container filesystem. **OCI metadata does not
survive** — `ENTRYPOINT`, `CMD` and, the one that catches people, `ENV`. The VM
boots straight into `/init.sh` through the kernel's `init=` parameter.

So the toolchain is not on `PATH` because of an `ENV` line. It is on `PATH`
because `/etc/profile.d/10-rust.sh` exists and the daemon renders every step as
`env … sh -lc '<script>'` — `-l` makes that a login shell, which reads
`/etc/profile.d`. Symlinks in `/usr/local/bin` cover anything that execs `cargo`
without a shell.

## Why the cache is not in the workspace

`ci` wipes `/workspace` on every checkout — it runs
`find /workspace -mindepth 1 -maxdepth 1 … -exec rm -rf {} +` before unpacking
the source. A `target/` inside it is therefore destroyed once per run, and a
warm VM would buy nothing but the toolchain being pre-installed.

So `CARGO_TARGET_DIR` is `/var/cache/ci/target`, and `init.sh` mounts the
workflow's `disk_size_gb` disk over `/var/cache/ci`. A second run on the same
warm VM relinks instead of recompiling three hundred crates. A cold VM starts
empty, which is correct — that is what the pool's fingerprint is deciding.

`CARGO_HOME` stays on the rootfs at `/usr/local/cargo`, so the registry cache is
sized into `size_mb` rather than the data disk.

## Changing it

Edit it and submit. The image name is the hash of this file and its build
context, so a change names an image no host has and the next run builds it —
and because the warm VM pool keys on the resolved image name, every pooled VM
built from the old rootfs stops matching at the same moment. Both caches bust
together, which is the thing the old hand-built flow could not do: rebuilding
under the same name left warm VMs running the previous rootfs until they aged
out, because the pool fingerprint had not changed.

To force a rebuild without editing anything, delete the file on the host:

```bash
rm ~/.heyo/images/firecracker/ci-img-*.ext4
```

`ci` notices the next create failing, forgets its record of the image, and
builds it again. Images are otherwise never swept — a rootfs is expensive to
rebuild and carries no state from the run that made it. The docker images and
containers the pipeline makes on the host are cleaned up per build; the layer
cache stays, which is what keeps rebuilds fast.
