# Firecracker rootfs that boots straight into Postgres.
#
# Firecracker boots a kernel + a flat rootfs and runs whatever the kernel
# `init=` arg points at as PID 1. There is no systemd/openrc in a microVM, so
# we ship a tiny init script (init.sh) that mounts the kernel pseudo-filesystems,
# prepares the data directory on /workspace, and exec's postgres as PID 1.
#
# The Postgres data directory lives at /workspace, which is expected to be a
# separate block device (a second Firecracker drive) mounted there at boot.
# This keeps the OS rootfs immutable/disposable and the database state on its
# own persistent volume.

FROM debian:bookworm-slim

ARG PG_MAJOR=16
ENV DEBIAN_FRONTEND=noninteractive \
    PGDATA=/workspace/pgdata \
    PG_MAJOR=${PG_MAJOR}

# Debian bookworm's stock repos only carry PostgreSQL 15, so for any other
# major (PG_MAJOR=16 by default) we pull from the PostgreSQL project's own apt
# repo (PGDG, apt.postgresql.org). curl + gnupg fetch the signing key; the repo
# is then pinned by that key via signed-by.
#
# Postgres + the handful of userland tools init.sh relies on.
# - e2fsprogs: mkfs.ext4 to format the /workspace volume on first boot
# - util-linux/mount: mount the volume and pseudo-filesystems
# - gosu: drop privileges to the postgres user without a login shell
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl gnupg ca-certificates \
    && install -d /usr/share/postgresql-common/pgdg \
    && curl -fsSL -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
        https://www.postgresql.org/media/keys/ACCC4CF8.asc \
    && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] http://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" \
        > /etc/apt/sources.list.d/pgdg.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        postgresql-${PG_MAJOR} \
        postgresql-client-${PG_MAJOR} \
        e2fsprogs \
        util-linux \
        mount \
        gosu \
    && rm -rf /var/lib/apt/lists/*

# Put the versioned Postgres binaries on PATH so init.sh stays version-agnostic.
ENV PATH="/usr/lib/postgresql/${PG_MAJOR}/bin:${PATH}"

# Mount point for the persistent data volume. Postgres refuses to run as root,
# so the data dir is owned by the postgres user that the package created.
RUN mkdir -p /workspace \
    && chown postgres:postgres /workspace

# The heyvm/Firecracker builder boots with a fixed kernel arg `init=/init.sh`,
# so the script must live at the rootfs *root* (not /sbin). A mismatch panics
# the kernel with "Requested init /init.sh failed (error -2)" (ENOENT).
COPY init.sh /init.sh
RUN chmod +x /init.sh

# ENTRYPOINT is ignored on the Firecracker boot path (the kernel `init=` arg
# wins), but keep it so the image is still runnable as a normal container.
ENTRYPOINT ["/init.sh"]
