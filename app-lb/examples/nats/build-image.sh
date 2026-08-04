#!/bin/sh
# Build the NATS Firecracker rootfs.
#
# A wrapper over `heyvm mvm build` for one reason worth a script: the Dockerfile
# lives in image/ but COPYs paths prefixed with image/, so the build context has
# to be this directory. heyvm defaults the context to the Dockerfile's own
# directory, which would fail on the first COPY.
#
#   ./build-image.sh                          # -> image named nats
#   IMAGE_NAME=nats-staging ./build-image.sh
#   DNS_SERVER=10.0.0.53 ./build-image.sh     # internal resolver for the guest
#
# Env:
#   IMAGE_NAME   nats   name `vm.image` in nats.json must match
#   DNS_SERVER   —      rewrites image/resolv.conf before building
#   SIZE_MB      —      rootfs size; default is auto (tar*1.2 + 64MB)
#   UPLOAD       —      set to 1 to also upload to the cloud (needs auth)
#
# Note this sizes the *rootfs*, which holds only the binary and the userland.
# JetStream's store is on the deployment's data disk (`vm.disk_size_gb` in
# nats.json), so growing the queue never means rebuilding the image.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
IMAGE_NAME="${IMAGE_NAME:-nats}"

for tool in docker mke2fs heyvm; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        if [ "$tool" = "mke2fs" ]; then
            echo "build-image.sh: mke2fs not found; install e2fsprogs" >&2
        else
            echo "build-image.sh: $tool not found on PATH" >&2
        fi
        exit 1
    fi
done

# heyvm shells out to fakeroot unless the build runs as root, so the root
# ownership in the Docker-exported tar survives into the ext4 image.
if [ "$(id -u)" -ne 0 ] && ! command -v fakeroot >/dev/null 2>&1; then
    echo "build-image.sh: fakeroot not found; install fakeroot or build as root" >&2
    exit 1
fi

# The resolver is a COPY'd file, not a build arg: `heyvm mvm build` passes no
# --build-arg through to docker, so an ARG would silently keep its default.
if [ -n "${DNS_SERVER:-}" ]; then
    printf '# Written by build-image.sh (DNS_SERVER=%s)\nnameserver %s\n' \
        "$DNS_SERVER" "$DNS_SERVER" > "$HERE/image/resolv.conf"
    echo "resolver set to $DNS_SERVER"
fi

set -- -f "$HERE/image/Dockerfile" -c "$HERE" -n "$IMAGE_NAME"
if [ -n "${SIZE_MB:-}" ]; then
    set -- "$@" --size-mb "$SIZE_MB"
fi
# --local-only skips the cloud upload, which also skips needing to be logged in.
if [ "${UPLOAD:-}" != "1" ]; then
    set -- "$@" --local-only
fi

echo "building $IMAGE_NAME"
heyvm mvm build "$@"

cat <<EOF

Built. Register the deployment:

  sed -i 's/"image": "[^"]*"/"image": "$IMAGE_NAME"/' $HERE/nats.json
  serverctl apply -f $HERE/nats.json
  serverctl rollout status nats

Then check the parts a health check cannot see — above all that the JetStream
store really is on the data disk, not on a rootfs that the next cold boot
discards:

  serverctl exec nats -- /opt/nats/preflight.sh
EOF
