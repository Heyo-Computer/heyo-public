---
name: heyvm-firecracker
description: Build Dockerfiles for Firecracker microVM rootfs images and manage them with `heyvm mvm`. Use when the user wants to create a Firecracker image, write a Dockerfile for a Firecracker VM, or build/list/manage Firecracker rootfs images.
argument-hint: "[build|images|fetch-kernel] [args...]"
allowed-tools: Bash, Read, Write, Edit, Grep, Glob
---

# firecracker — Build & Manage Firecracker VM Images

> **See also:** load the **heyvm-docs** skill for an overview of the heyvm
> platform and an index of the other heyvm-* skills (sandbox, deploy,
> proxy, api, login, system).

Write Dockerfiles that produce Firecracker-compatible ext4 rootfs images and build them with `heyvm mvm`.

## Commands

### Build an image from a Dockerfile

```bash
heyvm mvm build --local-only -f <Dockerfile> -n <name>
```

| Flag | Description | Default |
|------|-------------|---------|
| `-f, --file <FILE>` | Path to Dockerfile | `Dockerfile` |
| `-c, --context <DIR>` | Build context directory | Dockerfile's parent dir |
| `-n, --name <NAME>` | Image name/tag | derived from Dockerfile |
| `--size-mb <SIZE>` | Rootfs size in MB | auto (tar size * 1.2 + 64MB, min 128MB) |
| `--local-only` | Skip upload, build locally only | false |
| `--token <TOKEN>` | JWT for cloud upload | `HEYO_ARCHIVE_TOKEN` env var |

### List local images

```bash
heyvm mvm images
```

### Download the Firecracker kernel

```bash
heyvm mvm fetch-kernel
```

The kernel is also auto-downloaded on first sandbox start.

## Build pipeline

The build process (implemented in `mvm-ctrl/src/image_builder.rs`):

1. `docker build` — builds a Docker image from the Dockerfile
2. `docker create` + `docker export` — exports the container filesystem to a tar
3. `tar -xf` — extracts contents to a staging directory
4. `mke2fs -t ext4 -d <contents>` — creates an ext4 rootfs image from the extracted tree
5. Cleanup — removes the temporary Docker container and image

**Prerequisites:** Docker and `mke2fs` (from `e2fsprogs`) must be installed.

Images are stored in `~/.heyo/images/firecracker/<name>/rootfs.ext4`.

## Writing Dockerfiles for Firecracker

Firecracker VMs boot directly into a rootfs with no systemd, no cloud-init, and no container runtime. The Dockerfile must produce a filesystem that can boot standalone with a custom `/init.sh` entry point.

### Required structure

Every Firecracker Dockerfile MUST include:

1. **Base image** — Use `ubuntu:24.04` (or another distro with standard userspace tools).

2. **openssh-server** — Required for `heyvm exec` and `heyvm sh` to work. Pre-generate host keys at build time so sshd doesn't block on entropy at boot.

3. **`/init.sh` boot script** — This is the entry point. It must:
   - Mount essential virtual filesystems (`proc`, `sysfs`, `devtmpfs`, `devpts`)
   - Suppress kernel console messages with `dmesg -n 1`
   - Configure DNS (`/etc/resolv.conf`)
   - Set hostname
   - Bring up the network interface (`ip link set eth0 up`)
   - Start sshd
   - Start your application service(s)
   - Print `HEYVM_READY` — **this marker is required**, the host waits for it to confirm the VM is ready
   - End with a bash respawn loop on the serial console (below) — keeps the VM alive and makes `heyvm sh` land on a usable prompt that survives `exit` / Ctrl-D without panicking the kernel

4. **`EXPOSE` ports** — Always expose 22 (SSH) plus any application ports.

5. **`CMD ["/init.sh"]`** — Set the init script as the default command.

### Template

Use this as a starting point when writing new Firecracker Dockerfiles:

```dockerfile
FROM ubuntu:24.04

# Install openssh-server (required) + your application packages
RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    iproute2 \
    # --- add your packages here --- \
    && rm -rf /var/lib/apt/lists/*

# Configure SSH for Firecracker access.
# Pre-generate host keys so sshd doesn't block on entropy at boot.
# Explicitly enable PasswordAuthentication — Ubuntu 24.04 may disable it
# in /etc/ssh/sshd_config.d/ and Firecracker has no cloud-init to fix it.
RUN mkdir -p /run/sshd /etc/ssh/sshd_config.d \
    && echo "PermitRootLogin yes" >> /etc/ssh/sshd_config \
    && echo "PermitEmptyPasswords yes" >> /etc/ssh/sshd_config \
    && echo "PasswordAuthentication yes" > /etc/ssh/sshd_config.d/50-heyo.conf \
    && chmod 644 /etc/ssh/sshd_config.d/50-heyo.conf \
    && passwd -d root \
    && useradd -m -s /bin/bash heyo \
    && echo 'heyo:heyo' | chpasswd \
    && ssh-keygen -A

# Nicer serial-console prompt. `heyvm sh` lands on this PID-1 bash, and the
# default PS1 from Ubuntu's skeleton isn't loaded for a non-interactive parent.
RUN printf '%s\n' \
    'PS1="\[\033[1;32m\]\h\[\033[0m\]:\[\033[1;34m\]\w\[\033[0m\]# "' \
    'export HISTFILE=/root/.bash_history HISTSIZE=2000 HISTCONTROL=ignoredups' \
    > /etc/profile.d/heyvm-prompt.sh

# --- Application setup (config files, build steps, etc.) ---

# Boot script — runs as PID 1 (no systemd).
RUN printf '#!/bin/sh\n\
mount -t proc proc /proc\n\
mount -t sysfs sysfs /sys\n\
mount -t devtmpfs devtmpfs /dev 2>/dev/null\n\
if [ ! -c /dev/null ]; then\n\
    echo "init: devtmpfs unavailable, creating device nodes manually"\n\
    mknod -m 666 /dev/null    c 1 3\n\
    mknod -m 666 /dev/zero    c 1 5\n\
    mknod -m 444 /dev/random  c 1 8\n\
    mknod -m 444 /dev/urandom c 1 9\n\
    mknod -m 666 /dev/tty     c 5 0\n\
    mknod -m 666 /dev/ptmx    c 5 2\n\
    ln -sf /proc/self/fd /dev/fd\n\
fi\n\
mkdir -p /dev/pts && mount -t devpts devpts /dev/pts\n\
dmesg -n 1 2>/dev/null\n\
echo "nameserver 8.8.8.8" > /etc/resolv.conf\n\
hostname firecracker\n\
ip link set eth0 up 2>/dev/null\n\
if ! ip addr show eth0 2>/dev/null | grep -q "inet "; then\n\
    for param in $(cat /proc/cmdline); do\n\
        case "$param" in\n\
            ip=*)\n\
                GUEST_IP="${param#ip=}"; GUEST_IP="${GUEST_IP%%%%::*}"\n\
                TAIL="${param#*::}"; GW="${TAIL%%%%:*}"\n\
                ip addr add "$GUEST_IP/30" dev eth0 2>/dev/null\n\
                [ -n "$GW" ] && ip route add default via "$GW" dev eth0 2>/dev/null\n\
                ;;\n\
        esac\n\
    done\n\
fi\n\
mkdir -p /run/sshd\n\
chown root:root /run/sshd\n\
chmod 755 /run/sshd\n\
chown root:root /etc/ssh/ssh_host_* 2>/dev/null\n\
chmod 600 /etc/ssh/ssh_host_*_key 2>/dev/null\n\
chmod 644 /etc/ssh/ssh_host_*_key.pub 2>/dev/null\n\
/usr/sbin/sshd -D -e 2>/tmp/sshd.log &\n\
# --- start your services here --- \n\
echo "HEYVM_READY"\n\
# Interactive shell on ttyS0 for `heyvm sh`. The loop keeps the VM alive\n\
# even if the user exits the shell (PID 1 exit = kernel panic).\n\
while :; do /bin/bash --login; sleep 0.1; done\n' > /init.sh \
    && chmod +x /init.sh

EXPOSE 22 # add your ports

CMD ["/init.sh"]
```

### Key constraints

- **No systemd.** Firecracker boots directly to `/init.sh`. All services must be started manually in the init script.
- **No docker-entrypoint.** There is no Docker runtime — `ENTRYPOINT` and `CMD` are only used by `heyvm mvm build` to know the default command. The actual boot command is the kernel `init=` parameter, which points to `/init.sh`.
- **Filesystem mounts are manual.** `/proc`, `/sys`, `/dev`, and `/dev/pts` do not exist at boot. The init script must mount them. Docker-exported rootfs has an empty `/dev` — you MUST mount devtmpfs or create device nodes manually (`/dev/null`, `/dev/urandom`, `/dev/ptmx`) or sshd will fail to start.
- **Network is partially configured.** The kernel `ip=` boot parameter sets an IP, but the interface may need `ip link set eth0 up` to be fully usable. Include the fallback IP parsing from `/proc/cmdline` (see template) in case the kernel param didn't fully configure the interface before init runs.
- **Create `/run/sshd` at boot.** sshd requires this privilege-separation directory. Even if created in the Dockerfile, it may not survive the Docker export → ext4 conversion. Always `mkdir -p /run/sshd` before starting sshd.
- **Use `sshd -D -e 2>/tmp/sshd.log &`** (foreground mode, errors to log file). Do NOT let sshd write to stderr/serial — stray output corrupts the marker-delimited serial command protocol used by `heyvm exec`.
- **`HEYVM_READY` is mandatory.** The host process watches the serial console for this exact string. Without it, the VM will appear to hang during startup.
- **Keep images small.** The ext4 image is sized automatically (tar * 1.2 + 64MB, min 128MB). Large images slow down boot. Use `--no-install-recommends` and clean apt caches.
- **Pre-generate SSH host keys.** Run `ssh-keygen -A` at build time. Generating keys at boot time on Firecracker can be very slow due to limited entropy.
- **The `heyo` user** (password: `heyo`) is the standard non-root user. Always create it for compatibility with `heyvm exec` and `heyvm sh`.
- **Explicitly enable `PasswordAuthentication yes`.** Write `/etc/ssh/sshd_config.d/50-heyo.conf` at build time. Ubuntu 24.04 may ship with password auth disabled and Firecracker has no cloud-init to fix it at runtime. Without this, deployed sandboxes cannot get a shell.
- **Don't end init.sh with `exec /bin/sh`.** That gives you dash with no prompt on the serial console, and `exit` / Ctrl-D panics the kernel (PID 1 exit). Use the `while :; do /bin/bash --login; sleep 0.1; done` loop from the template instead.

### Reference example

See `third-party/Dockerfile.firecracker-nginx` for a working example that runs nginx inside a Firecracker VM.

## Using built images

After building, create a Firecracker sandbox:

```bash
heyvm create --name my-vm --backend-type firecracker --image <name>
```

Where `<name>` matches the `-n` flag used during `heyvm mvm build`.

## Workflow

When the user asks to build a Firecracker image or write a Dockerfile for Firecracker:

1. **Understand the service** — what application/services should run in the VM.
2. **Write the Dockerfile** — follow the template above, adding the required packages and service startup to the init script.
3. **Save the Dockerfile** — place it at `third-party/Dockerfile.firecracker-<name>` by convention.
4. **Build it** — run `heyvm mvm build --local-only -f third-party/Dockerfile.firecracker-<name> -n <name>`.
5. **Verify** — run `heyvm mvm images` to confirm the image was created.

When the user provides `$ARGUMENTS`, interpret them as arguments to `heyvm mvm`. For example, `/firecracker images` should run `heyvm mvm images`, and `/firecracker build -f Dockerfile -n myimg --local-only` should run `heyvm mvm build -f Dockerfile -n myimg --local-only`.

If `$ARGUMENTS` is empty, ask the user what service they want to run in Firecracker and write the Dockerfile for them.
