#!/usr/bin/env bash
# Build the Docker image and flatten its filesystem into an ext4 rootfs image
# that Firecracker can boot. Firecracker wants a raw block image, not a Docker
# image, so we export the container's filesystem into a sparse ext4 file.
#
# Usage: ./build-rootfs.sh [output.ext4] [size]
#   output.ext4  destination image path (default: pg-rootfs.ext4)
#   size         rootfs size, mkfs-style (default: 2G)
#
# Must run on Linux (or a Linux VM) — mkfs.ext4 + loopback mount aren't
# available on macOS hosts. Requires: docker, e2fsprogs, root (for mount).

set -euo pipefail

IMAGE=pg-fc:latest
OUTPUT="${1:-pg-rootfs.ext4}"
SIZE="${2:-2G}"
HERE="$(cd "$(dirname "$0")" && pwd)"

echo ">> building docker image $IMAGE"
docker build -t "$IMAGE" "$HERE"

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

sync
echo ">> rootfs ready: $OUTPUT"
echo "   boot with: init=/sbin/init.sh  (data volume on /dev/vdb -> /workspace)"
