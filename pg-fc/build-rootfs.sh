#!/usr/bin/env bash
# Build the Docker image and flatten its filesystem into an ext4 rootfs image
# that Firecracker can boot. Firecracker wants a raw block image, not a Docker
# image, so we export the container's filesystem into a sparse ext4 file.
#
# Usage: ./build-rootfs.sh [output.ext4] [size]
#   output.ext4  destination image path (default: pg-rootfs.ext4)
#   size         rootfs size, mkfs-style (default: 2G)
#   PG_MAJOR=NN  Postgres major to build (default: 18)
#
# Must run on Linux (or a Linux VM) — mkfs.ext4 + loopback mount aren't
# available on macOS hosts. Requires: docker, e2fsprogs, root (for mount).

set -euo pipefail

IMAGE=pg-fc:latest
OUTPUT="${1:-pg-rootfs.ext4}"
SIZE="${2:-2G}"
HERE="$(cd "$(dirname "$0")" && pwd)"

# Postgres major to build. This MUST be >= the major that wrote the data
# directories already in the fleet — a server cannot open a pgdata newer than
# itself, and that failure is invisible from outside the VM: the guest boots
# normally, init.sh emits HEYVM_READY (it does so right after backgrounding
# the postmaster, deliberately), and Postgres dies a second later. The pooler
# sees only "Postgres unreachable inside a running VM" and power-cycles
# forever, with `pgdata=vNN` in its boot-evidence line as the sole clue.
#
# This script used to run a bare `docker build "$HERE"`, which silently picked
# ./Dockerfile's own default of PG_MAJOR=16 — so a rebuild quietly downgraded
# the image while NET-NEW schemas kept working (a fresh v16 initdb is
# self-consistent) and every S3 restore of a v18 cluster died. That asymmetry
# is what makes it so hard to spot; hence the explicit arg and the check
# after the export below.
#
# ./Dockerfile is fully parameterized on PG_MAJOR — Dockerfile.pg18 is that
# same file with a different default — so one build arg covers both.
PG_MAJOR="${PG_MAJOR:-18}"

echo ">> building docker image $IMAGE (PostgreSQL $PG_MAJOR)"
docker build --build-arg "PG_MAJOR=$PG_MAJOR" -t "$IMAGE" "$HERE"

echo ">> creating $SIZE ext4 image at $OUTPUT"
rm -f "$OUTPUT"
truncate -s "$SIZE" "$OUTPUT"
mkfs.ext4 -q "$OUTPUT"

MNT="$(mktemp -d)"
CID=""
cleanup() {
    [ -n "$CID" ] && docker rm -f "$CID" >/dev/null 2>&1 || true
    mountpoint -q "$MNT" && sudo umount "$MNT" || true
    rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

echo ">> exporting image filesystem into rootfs"
sudo mount -o loop "$OUTPUT" "$MNT"

# `docker create` + `docker export` gives us the full flattened filesystem
# (all layers squashed) without needing the image to actually run.
CID="$(docker create "$IMAGE")"
docker export "$CID" | sudo tar -x -C "$MNT"

# The kernel cmdline runs `init=/init.sh`; the Dockerfile already placed it
# there (COPY init.sh /init.sh — a mismatch panics the kernel). Make sure the
# mount points the init script needs exist in the rootfs.
sudo mkdir -p "$MNT/proc" "$MNT/sys" "$MNT/dev" "$MNT/run" "$MNT/tmp" "$MNT/workspace"

# Prove the rootfs carries the major it claims, while the image is still
# mounted and before anything can boot it. A wrong PG_MAJOR is trivial to
# introduce and impossible to notice later — by then it presents as a schema
# that will not come up, three layers away from this script.
if ! sudo test -x "$MNT/usr/lib/postgresql/$PG_MAJOR/bin/postgres"; then
    echo "!! rootfs has no postgres $PG_MAJOR binary; majors present:" >&2
    sudo ls "$MNT/usr/lib/postgresql" >&2 2>/dev/null || echo "   (none at all)" >&2
    exit 1
fi

sync
echo ">> rootfs ready: $OUTPUT (PostgreSQL $PG_MAJOR)"
echo "   boot with: init=/sbin/init.sh  (data volume on /dev/vdb -> /workspace)"
