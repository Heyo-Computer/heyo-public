#!/usr/bin/env bash
# provision-pooler-host.sh — build a bare Ubuntu box into a pg-vm-pool host.
#
# Derived from the mia3.pool.flatfile.com build (2026-09-08). Every step here
# was executed and verified on that host; the comments record the failures hit
# on the way, because most of them are silent or misleading in isolation.
#
# Usage (runs ON the target host, as a sudo-capable non-root user):
#
#   POOL_HOSTNAME=mia4.pool.flatfile.com \
#   ACME_EMAIL=sam@heyo.computer \
#   RAID_CREATE=yes RAID_DEVICES="/dev/nvme2n1 /dev/nvme3n1" RAID_LEVEL=0 \
#   HEYO_API_KEY=... \
#   PG_VM_POOL_S3_BUCKET=... PG_VM_POOL_S3_ACCESS_KEY_ID=... \
#   PG_VM_POOL_S3_SECRET_ACCESS_KEY=... \
#   ./provision-pooler-host.sh
#
# It is idempotent: re-running skips work already done. The ONLY destructive
# step is the RAID create, which refuses to touch a device that is not blank
# and requires RAID_CREATE=yes explicitly.
#
# What it deliberately does NOT do: firewall. See the SECURITY section below.
set -euo pipefail

# ---- configuration ----------------------------------------------------------

POOL_HOSTNAME="${POOL_HOSTNAME:?set POOL_HOSTNAME, e.g. mia4.pool.flatfile.com}"
POOL_USER="${POOL_USER:-$(id -un)}"
POOL_HOME="${POOL_HOME:-/home/${POOL_USER}}"

# Storage. RAID_LEVEL 0 = capacity (a drive loss is total loss), 1 = mirror.
# RAID_CREATE=no + DATA_MOUNT on an existing filesystem is fully supported —
# the array is only one way to get a big filesystem at DATA_MOUNT.
RAID_CREATE="${RAID_CREATE:-no}"
RAID_DEVICES="${RAID_DEVICES:-}"
RAID_LEVEL="${RAID_LEVEL:-0}"
MD_DEVICE="${MD_DEVICE:-/dev/md1}"
DATA_MOUNT="${DATA_MOUNT:-/mnt/md1}"

# heyvm state root. The run dir is $MVM_DATA_DIR/run and MUST match the
# pooler's PG_VM_POOL_RUN_DIR and the sudoers pin — see the RUN_DIR note below.
MVM_DATA_DIR="${MVM_DATA_DIR:-${DATA_MOUNT}/heyvm}"
RUN_DIR="${MVM_DATA_DIR}/run"

ACME_EMAIL="${ACME_EMAIL:-}"                      # empty => register w/o email
REPO_URL="${REPO_URL:-https://github.com/Heyo-Computer/heyo-public.git}"
REPO_REF="${REPO_REF:-main}"
CHECKOUT="${CHECKOUT:-${POOL_HOME}/Projects/heyo-public}"
PGFC="${CHECKOUT}/pg-fc"
FC_VERSION="${FC_VERSION:-}"                      # empty => latest release
API_PORT="${API_PORT:-34099}"
DASHBOARD_LISTEN="${DASHBOARD_LISTEN:-127.0.0.1:34199}"
POOL_LISTEN="${POOL_LISTEN:-0.0.0.0:6432}"
POOL_IMAGE="${POOL_IMAGE:-pg}"
DOCKERFILE="${DOCKERFILE:-Dockerfile}"            # Dockerfile.pg18 for PG 18

STATE_DIR="${POOL_HOME}/.heyo/pg-vm-pool"
TLS_DIR="${STATE_DIR}/tls"
BIN_DIR="${POOL_HOME}/.heyo/bin"
# The pooler shells out to this exact string, and /etc/sudoers.d/pg-vm-pool
# pins the same argument list byte for byte. Keep the two in step.
RECLAIM_CMD="sudo -n ${BIN_DIR}/reclaim-disks.sh ${RUN_DIR} --shrink --prune-swap"

# Secrets. Both are optional, and each one unlocks a different capability.
HEYO_API_KEY="${HEYO_API_KEY:-}"
PG_VM_POOL_S3_BUCKET="${PG_VM_POOL_S3_BUCKET:-}"
PG_VM_POOL_S3_ACCESS_KEY_ID="${PG_VM_POOL_S3_ACCESS_KEY_ID:-}"
PG_VM_POOL_S3_SECRET_ACCESS_KEY="${PG_VM_POOL_S3_SECRET_ACCESS_KEY:-}"
PG_VM_POOL_S3_ENDPOINT="${PG_VM_POOL_S3_ENDPOINT:-}"   # R2/MinIO only
PG_VM_POOL_S3_REGION="${PG_VM_POOL_S3_REGION:-}"

# Pooler tunables (defaults match the shipped conf).
WARM_SPARES="${WARM_SPARES:-12}"
ARCHIVE_AFTER_SECS="${ARCHIVE_AFTER_SECS:-86400}"
COMPACT_AFTER_SECS="${COMPACT_AFTER_SECS:-3600}"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
# Group membership needs a new process to take effect, so the script re-execs
# itself under `sg`. Bound that to one hop: if a second pass still lacks the
# group, something is wrong with the group database and looping forever would
# hide it.
PROVISION_REEXEC="${PROVISION_REEXEC:-0}"
export PROVISION_REEXEC
reexec_under() {
  _grp="$1"; shift   # the rest is the script's own argv, which must survive
  [ "$PROVISION_REEXEC" -lt 1 ] || die "still missing group '$_grp' after re-exec — check /etc/group and re-login"
  PROVISION_REEXEC=1
  log "re-running under group '$_grp'"
  exec sg "$_grp" -c "$(printf '%q ' "$0" "$@")"
}

[ "$(id -u)" -ne 0 ] || die "run as the pooler user with sudo available, not as root"
sudo -n true 2>/dev/null || die "passwordless sudo is required"

# Decide the two conditional features up front so the summary is honest.
if [ -n "$HEYO_API_KEY" ]; then HEYVM_MODE=cloud; else HEYVM_MODE=local; fi
if [ -n "$PG_VM_POOL_S3_BUCKET" ] && [ -n "$PG_VM_POOL_S3_ACCESS_KEY_ID" ] \
   && [ -n "$PG_VM_POOL_S3_SECRET_ACCESS_KEY" ]; then S3_MODE=on; else S3_MODE=off; fi

log "host=${POOL_HOSTNAME} user=${POOL_USER} data=${MVM_DATA_DIR} heyvm=${HEYVM_MODE} s3=${S3_MODE}"

# ---- 1. base packages -------------------------------------------------------

log "installing base packages"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
# fakeroot is NOT optional: `heyvm mvm build` shells out to it to assemble the
# rootfs and fails with a bare "Failed to run fakeroot: No such file or
# directory" that reads like a heyvm bug. cpu-checker gives kvm-ok. psmisc
# gives fuser. postgresql-client is for the verification phase.
sudo apt-get install -y -qq \
  tmux git jq curl ca-certificates gnupg build-essential pkg-config libssl-dev \
  psmisc unzip acl mdadm fakeroot e2fsprogs zstd cpu-checker net-tools \
  supervisor certbot postgresql-client

# ---- 2. KVM sanity ----------------------------------------------------------

log "checking KVM"
[ -e /dev/kvm ] || die "/dev/kvm missing — this host cannot run Firecracker"
sudo kvm-ok >/dev/null 2>&1 || warn "kvm-ok unhappy; continuing (check virtualization in BIOS)"

# ---- 3. storage -------------------------------------------------------------

if [ "$RAID_CREATE" = "yes" ]; then
  if [ -e "$MD_DEVICE" ]; then
    log "array ${MD_DEVICE} already exists — skipping create"
  else
    [ -n "$RAID_DEVICES" ] || die "RAID_CREATE=yes needs RAID_DEVICES"
    # Refuse to destroy anything. wipefs -n is a dry run: any output means the
    # device already carries a filesystem/partition/RAID signature.
    for d in $RAID_DEVICES; do
      [ -b "$d" ] || die "$d is not a block device"
      if [ -n "$(sudo wipefs -n "$d" 2>/dev/null)" ]; then
        die "$d is NOT blank (has a filesystem or partition signature). Refusing to overwrite. Wipe it deliberately if that is really what you want."
      fi
      if lsblk -no MOUNTPOINT "$d" 2>/dev/null | grep -q .; then
        die "$d (or a child) is mounted. Refusing."
      fi
    done
    # shellcheck disable=SC2086
    dev_count=$(echo $RAID_DEVICES | wc -w)
    log "creating RAID${RAID_LEVEL} ${MD_DEVICE} over ${RAID_DEVICES}"
    # shellcheck disable=SC2086
    sudo mdadm --create "$MD_DEVICE" --level="$RAID_LEVEL" \
         --raid-devices="$dev_count" --name=vmdata --run $RAID_DEVICES
    sleep 2
  fi

  # Persist assembly. Without the mdadm.conf line + initramfs rebuild the array
  # does not come back after a reboot and the fstab mount silently no-ops
  # (that is what `nofail` buys: the host still boots).
  if ! sudo grep -q "$(sudo mdadm --detail --scan "$MD_DEVICE" | awk '{print $4}')" /etc/mdadm/mdadm.conf 2>/dev/null; then
    log "recording array in /etc/mdadm/mdadm.conf"
    sudo mdadm --detail --scan "$MD_DEVICE" | sudo tee -a /etc/mdadm/mdadm.conf >/dev/null
    sudo update-initramfs -u
  fi

  if ! sudo blkid -s UUID -o value "$MD_DEVICE" >/dev/null 2>&1; then
    log "formatting ${MD_DEVICE} ext4"
    # -m 0: no root-reserved blocks. On a 3.5T array the default 5% reserve is
    # ~175G of pure waste on a filesystem root never writes to.
    sudo mkfs.ext4 -q -m 0 -L VMDATA -E lazy_itable_init=1,lazy_journal_init=1 "$MD_DEVICE"
  fi
fi

if ! mountpoint -q "$DATA_MOUNT"; then
  log "mounting ${DATA_MOUNT}"
  sudo mkdir -p "$DATA_MOUNT"
  if [ -e "$MD_DEVICE" ]; then
    fsuuid=$(sudo blkid -s UUID -o value "$MD_DEVICE")
    grep -q "$fsuuid" /etc/fstab || \
      echo "UUID=$fsuuid $DATA_MOUNT ext4 defaults,noatime,nofail 0 2" | sudo tee -a /etc/fstab >/dev/null
    sudo systemctl daemon-reload
    sudo mount "$DATA_MOUNT"
  else
    die "$DATA_MOUNT is not mounted and $MD_DEVICE does not exist — set RAID_CREATE=yes or mount storage there yourself"
  fi
fi
sudo install -d -o "$POOL_USER" -g "$POOL_USER" "$MVM_DATA_DIR"
mkdir -p "$STATE_DIR" "$TLS_DIR" "$BIN_DIR"

# ---- 4. docker --------------------------------------------------------------

if ! have docker; then
  log "installing docker"
  sudo install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
    | sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg --yes
  sudo chmod a+r /etc/apt/keyrings/docker.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
    | sudo tee /etc/apt/sources.list.d/docker.list >/dev/null
  sudo apt-get update -qq
  sudo apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-buildx-plugin
fi
sudo systemctl enable --now docker >/dev/null 2>&1 || true

# The image build needs docker, and the VMs need /dev/kvm. Group changes do not
# apply to the current shell — the script re-execs itself once via `sg` rather
# than failing later with a confusing permission error.
if ! id -nG | tr ' ' '\n' | grep -qx docker; then
  log "adding ${POOL_USER} to docker,kvm"
  sudo usermod -aG docker,kvm "$POOL_USER"
  reexec_under docker "$@"
fi
if ! id -nG | tr ' ' '\n' | grep -qx kvm; then
  sudo usermod -aG kvm "$POOL_USER"
  reexec_under kvm "$@"
fi

# ---- 5. firecracker ---------------------------------------------------------

if ! have firecracker; then
  ver="$FC_VERSION"
  [ -n "$ver" ] || ver=$(curl -fsSL https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest | jq -r .tag_name)
  log "installing firecracker ${ver}"
  tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
  arch=$(uname -m)
  curl -fsSL -o "$tmp/fc.tgz" \
    "https://github.com/firecracker-microvm/firecracker/releases/download/${ver}/firecracker-${ver}-${arch}.tgz"
  tar -xzf "$tmp/fc.tgz" -C "$tmp"
  sudo install -m 0755 "$tmp/release-${ver}-${arch}/firecracker-${ver}-${arch}" /usr/local/bin/firecracker
  sudo install -m 0755 "$tmp/release-${ver}-${arch}/jailer-${ver}-${arch}" /usr/local/bin/jailer 2>/dev/null || true
fi
firecracker --version 2>&1 | head -1

# ---- 6. heyvm ---------------------------------------------------------------

if ! have /usr/local/bin/heyvm; then
  log "installing heyvm"
  # The installer writes to /usr/local/bin only if the *invoking user* can, and
  # it does not use sudo — so as a normal user it lands in ~/.local/bin. It also
  # re-execs the shell, which kills a non-interactive parent; run it detached.
  curl -fsSL https://heyo.computer/heyvm/install.sh -o /tmp/heyvm-install.sh
  sh /tmp/heyvm-install.sh </dev/null >/tmp/heyvm-install.log 2>&1 || true
  src=""
  for c in "$POOL_HOME/.local/bin/heyvm" /usr/local/bin/heyvm; do
    [ -x "$c" ] && src="$c" && break
  done
  [ -n "$src" ] || { cat /tmp/heyvm-install.log; die "heyvm install failed"; }
  sudo install -m 0755 "$src" /usr/local/bin/heyvm
  sudo install -m 0755 "$(dirname "$src")/heyvmd" /usr/local/bin/heyvmd
fi
/usr/local/bin/heyvm --version 2>&1 | head -1

# ---- 7. rust + build --------------------------------------------------------

if ! have "$POOL_HOME/.cargo/bin/cargo"; then
  log "installing rust"
  curl -fsSL https://sh.rustup.rs -o /tmp/rustup.sh
  sh /tmp/rustup.sh -y --profile minimal --default-toolchain stable --no-modify-path
fi
export PATH="$POOL_HOME/.cargo/bin:$PATH"

if [ -d "$CHECKOUT/.git" ]; then
  log "updating checkout"
  git -C "$CHECKOUT" fetch --depth 50 origin "$REPO_REF"
  git -C "$CHECKOUT" checkout -q FETCH_HEAD
else
  log "cloning ${REPO_URL}"
  mkdir -p "$(dirname "$CHECKOUT")"
  git clone --depth 50 --branch "$REPO_REF" "$REPO_URL" "$CHECKOUT"
fi
log "building pg-vm-pool (release) at $(git -C "$CHECKOUT" rev-parse --short HEAD)"
( cd "$PGFC" && cargo build --release --locked )

grep -q MVM_DATA_DIR "$POOL_HOME/.bashrc" || \
  printf '\n# heyvm VM state lives on the data array, not ~/.heyo\nexport MVM_DATA_DIR=%s\nexport PATH="$HOME/.cargo/bin:$PATH"\n' \
    "$MVM_DATA_DIR" >> "$POOL_HOME/.bashrc"

# ---- 8. heyvm login (only with a key) ---------------------------------------

if [ "$HEYVM_MODE" = cloud ]; then
  log "configuring heyvm cloud credentials"
  # heyvmd reads ~/.heyo/.env. Written 0600: it is a durable credential.
  mkdir -p "$POOL_HOME/.heyo"
  touch "$POOL_HOME/.heyo/.env"; chmod 600 "$POOL_HOME/.heyo/.env"
  if grep -q '^HEYO_API_KEY=' "$POOL_HOME/.heyo/.env" 2>/dev/null; then
    sed -i "s|^HEYO_API_KEY=.*|HEYO_API_KEY=${HEYO_API_KEY}|" "$POOL_HOME/.heyo/.env"
  else
    echo "HEYO_API_KEY=${HEYO_API_KEY}" >> "$POOL_HOME/.heyo/.env"
  fi
fi

# ---- 9. guest image ---------------------------------------------------------

if [ ! -f "${MVM_DATA_DIR}/images/firecracker/${POOL_IMAGE}.ext4" ]; then
  log "building the ${POOL_IMAGE} guest image (docker build + flatten; takes a few minutes)"
  ( cd "$PGFC" && MVM_DATA_DIR="$MVM_DATA_DIR" \
      PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
      heyvm mvm build --local-only -f "$DOCKERFILE" --name "$POOL_IMAGE" )
else
  log "guest image ${POOL_IMAGE} already present — skipping build"
fi

# ---- 10. TLS ----------------------------------------------------------------

if [ ! -d "/etc/letsencrypt/live/${POOL_HOSTNAME}" ]; then
  log "issuing certificate for ${POOL_HOSTNAME}"
  # Standalone needs :80 free and public DNS already pointing here. Check both
  # first — certbot's own failure for a wrong DNS record is not obvious.
  resolved=$(getent hosts "$POOL_HOSTNAME" | awk '{print $1}' | head -1)
  [ -n "$resolved" ] || die "$POOL_HOSTNAME does not resolve — point DNS at this host first"
  if sudo ss -lnt | awk '{print $4}' | grep -qE '(^|:)80$'; then
    die "port 80 is in use; free it for the ACME challenge"
  fi
  if [ -n "$ACME_EMAIL" ]; then
    sudo certbot certonly --standalone -d "$POOL_HOSTNAME" -m "$ACME_EMAIL" \
         --agree-tos --non-interactive --no-eff-email
  else
    sudo certbot certonly --standalone -d "$POOL_HOSTNAME" \
         --register-unsafely-without-email --agree-tos --non-interactive
  fi
fi

log "installing certbot deploy hook"
# The pooler stats the cert before each handshake and rebuilds its acceptor when
# it changes, so renewals need no restart — but it runs as $POOL_USER and cannot
# read /etc/letsencrypt/live. Hence the copy.
sudo tee /etc/letsencrypt/renewal-hooks/deploy/pg-vm-pool.sh >/dev/null <<HOOK
#!/bin/sh
d=${TLS_DIR}
mkdir -p "\$d"
install -o ${POOL_USER} -g ${POOL_USER} -m 600 "\$RENEWED_LINEAGE/fullchain.pem" "\$d/fullchain.pem"
install -o ${POOL_USER} -g ${POOL_USER} -m 600 "\$RENEWED_LINEAGE/privkey.pem"  "\$d/privkey.pem"
HOOK
sudo chmod +x /etc/letsencrypt/renewal-hooks/deploy/pg-vm-pool.sh
sudo env RENEWED_LINEAGE="/etc/letsencrypt/live/${POOL_HOSTNAME}" \
     /etc/letsencrypt/renewal-hooks/deploy/pg-vm-pool.sh

# ---- 11. reclaim script + sudoers pin ---------------------------------------

log "deploying reclaim-disks.sh outside the checkout"
# Root-owned and outside the repo on purpose: the sudoers entry must not be
# repointable by editing a user-writable file, and the path must survive a
# rebuild or a move of the checkout.
sudo install -D -m 0755 -o root -g root "$PGFC/reclaim-disks.sh" "$BIN_DIR/reclaim-disks.sh"
echo "${POOL_USER} ALL=(root) NOPASSWD: ${BIN_DIR}/reclaim-disks.sh ${RUN_DIR} --shrink --prune-swap" \
  | sudo tee /etc/sudoers.d/pg-vm-pool >/dev/null
sudo chmod 0440 /etc/sudoers.d/pg-vm-pool
sudo visudo -c -f /etc/sudoers.d/pg-vm-pool >/dev/null || die "sudoers entry is invalid"
# sudoers matches the argument list byte for byte. A drifted pin fails every
# hourly reclaim with nothing obviously broken, so assert the match now.
sudo -n -l "$BIN_DIR/reclaim-disks.sh" "$RUN_DIR" --shrink --prune-swap >/dev/null \
  || die "sudoers pin does not match the reclaim command"

# ---- 12. supervisor configs -------------------------------------------------

log "writing supervisor configs"
sudo install -d -o "$POOL_USER" -g "$POOL_USER" /var/log/pg-vm-pool /var/log/heyvmd
chmod +x "$PGFC/deploy/supervisor/heyvmd-healthcheck.sh"

if [ -f "$STATE_DIR/admin-password.txt" ]; then
  POOL_PASSWORD=$(cat "$STATE_DIR/admin-password.txt")
else
  # tr -d '=+/' keeps the generated value alphanumeric on purpose — see the
  # guard below for why the character set matters.
  POOL_PASSWORD="${POOL_PASSWORD:-$(openssl rand -base64 24 | tr -d '=+/' | cut -c1-32)}"
  echo "$POOL_PASSWORD" > "$STATE_DIR/admin-password.txt"
  chmod 600 "$STATE_DIR/admin-password.txt"
fi
# The password is written into an .ini value that supervisord parses. `%` is
# expansion syntax there (%(here)s), and `;` starts a comment — either one
# silently corrupts the environment= block rather than erroring, which would
# leave the pooler running with a DIFFERENT password than the one on file.
# Reject anything outside a safe set instead of guessing at escaping.
case "$POOL_PASSWORD" in
  *[!A-Za-z0-9._@~-]*)
    die "POOL_PASSWORD contains a character unsafe in a supervisord .ini value (allowed: A-Z a-z 0-9 . _ @ ~ -). Pick another, or let the script generate one." ;;
esac
[ -n "$POOL_PASSWORD" ] || die "POOL_PASSWORD resolved empty"

# heyvmd vs heyvm --api. This is the fork the HEYO_API_KEY secret controls, and
# it is a security difference, not just a feature difference:
#
#   with a key:  heyvmd --socket-only  -> TCP binds 127.0.0.1 ONLY, and the
#                iroh tunnel still works because it dials loopback. The
#                unauthenticated API is then unreachable from the network.
#   without:     heyvm --api           -> binds 0.0.0.0 with auth disabled and
#                there is no flag to change it (api.rs start_api_server hard-
#                codes loopback_only:false). Must be blocked upstream.
#
# heyvmd resolves a user JWT before doing anything and exits with "run `heyvm
# login` first" when there is no credential; --disable-network-agent does NOT
# skip that, which is why the keyless path cannot use heyvmd at all.
if [ "$HEYVM_MODE" = cloud ]; then
  HEYVM_COMMAND="/usr/local/bin/heyvmd --api-port ${API_PORT} --socket-only --network-node-kind cloud_host"
  HEYVM_NOTE="heyvmd with a cloud login; --socket-only keeps TCP on loopback."
else
  HEYVM_COMMAND="/usr/local/bin/heyvm --api ${API_PORT:+--port ${API_PORT}}"
  HEYVM_NOTE="heyvm --api (no cloud credential). WARNING: binds 0.0.0.0:${API_PORT} with NO auth — block it upstream."
fi

gen_heyvmd_conf() {
{
  cat <<'HDR'
; supervisord config for the heyo VM control-plane API that pg-vm-pool drives.
; GENERATED by deploy/provision-pooler-host.sh — edit there, not here, or your
; change is lost on the next provision run.
;
; ⚠ MVM_DATA_DIR is what puts VM disks on the data array instead of ~/.heyo.
; It MUST match PG_VM_POOL_RUN_DIR's parent in pg-vm-pool.conf and the run dir
; pinned in /etc/sudoers.d/pg-vm-pool. A mismatch silently disables the
; pooler's orphan sweep, reclaim, compaction and pressure eviction — no error,
; it just treats an empty directory as "nothing to do".
;
; HEYVM_DISABLE_SIBLING_HOSTS: after every create the daemon syncs /etc/hosts
; into every OTHER running VM, holding its sandbox-handles write lock across
; guest execs (~25s per running VM per create). On a pooler host that turns a
; deploy burst into minutes-long API stalls, and pg VMs never address each
; other by hostname, so the sync buys nothing.
HDR
  printf ';\n; MODE: %s\n\n' "$HEYVM_NOTE"
  printf '[program:heyvmd]\ncommand=%s\ndirectory=%s\nuser=%s\n' \
         "$HEYVM_COMMAND" "$POOL_HOME" "$POOL_USER"
  # supervisord children get a minimal environment; heyvm invokes `firecracker`
  # by bare name and VM setup shells out to sbin tools, so PATH must be explicit.
  printf 'environment=PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",HOME="%s",USER="%s",MVM_DATA_DIR="%s",HEYVM_DISABLE_SIBLING_HOSTS="1",HEYVMD_READY_TIMEOUT_SECS="60",HEYVMD_CREATE_TIMEOUT_SECS="180"\n' \
         "$POOL_HOME" "$POOL_USER" "$MVM_DATA_DIR"
  cat <<'TAIL'
autostart=true
autorestart=true
startsecs=5
startretries=3
stopsignal=TERM
stopwaitsecs=15
; VMs SURVIVE a daemon restart: Firecracker VMMs run in their own session with
; a detached serial console, so they are not in the daemon's process group and
; the fresh daemon reattaches on startup. These MUST stay false — with `true`
; supervisor signals the whole group and kills every running VM on restart.
stopasgroup=false
killasgroup=false
redirect_stderr=true
stdout_logfile=/var/log/heyvmd/heyvmd.log
stdout_logfile_maxbytes=20MB
stdout_logfile_backups=5

; Watchdog: autorestart only fires when the process EXITS, not when it is alive
; but wedged. This polls /health and restarts after 8 consecutive failures
; (~2-4.5 min). Deliberately patient: a restart is disruptive and a hasty
; watchdog turns one stall into a restart loop. No `user=` — supervisor.sock is
; 0700 root-owned, so an unprivileged process cannot call supervisorctl.
TAIL
  printf '[program:heyvmd-healthcheck]\ncommand=%s/deploy/supervisor/heyvmd-healthcheck.sh\n' "$PGFC"
  cat <<'TAIL2'
autostart=true
autorestart=true
startsecs=5
TAIL2
  printf 'environment=\n    HEYVMD_HEALTHCHECK_PORT="%s",\n' "$API_PORT"
  cat <<'TAIL3'
    HEYVMD_HEALTHCHECK_INTERVAL_SECS="15",
    HEYVMD_HEALTHCHECK_CURL_TIMEOUT_SECS="20",
    HEYVMD_HEALTHCHECK_FAIL_THRESHOLD="8",
    HEYVMD_HEALTHCHECK_COOLDOWN_SECS="600"
redirect_stderr=true
stdout_logfile=/var/log/heyvmd/heyvmd-healthcheck.log
stdout_logfile_maxbytes=5MB
stdout_logfile_backups=3
TAIL3
}
}

# The S3 tier and pressure eviction are one unit. PG_VM_POOL_PRESSURE_PATH,
# ARCHIVE_AFTER_SECS and IMAGE_ARCHIVE each fail startup if set without a
# bucket + credentials ("PRESSURE_PATH is set but the S3 eviction tier is not
# configured"), so they go in together or not at all.
gen_pool_conf() {
{
  cat <<'HDR'
; supervisord config for the pg-vm-pool Postgres pooler.
; GENERATED by deploy/provision-pooler-host.sh — edit there, not here.
;
; Change runtime config in the environment= block, then
;   supervisorctl reread && supervisorctl update pg-vm-pool
; a plain `restart` does NOT reload environment=; `update` does.
;
; ⚠ RUN_DIR must be this host's real heyvmd run dir. Verify after every deploy:
;     tr '\0' '\n' < /proc/$(pgrep -f pg-vm-pool | head -1)/environ | grep RUN_DIR
;     ls -d <run dir>/sb-* >/dev/null && echo run-dir populated
HDR
  printf '\n[program:pg-vm-pool]\ncommand=%s/target/release/pg-vm-pool\ndirectory=%s\nuser=%s\n\n' \
         "$PGFC" "$PGFC" "$POOL_USER"
  printf 'environment=\n'
  printf '    HOME="%s",\n' "$POOL_HOME"
  printf '    PG_VM_POOL_LISTEN="%s",\n' "$POOL_LISTEN"
  printf '    PG_VM_POOL_IMAGE="%s",\n' "$POOL_IMAGE"
  printf '    PG_VM_POOL_USER="postgres",\n'
  printf '    PG_VM_POOL_IDLE_TIMEOUT_SECS="300",\n'
  printf '    PG_VM_POOL_IDLE_TIMEOUT_FAST_SECS="60",\n'
  printf '    PG_VM_POOL_IDLE_DRAIN_WINDOW_SECS="600",\n'
  printf '    PG_VM_POOL_READY_TIMEOUT_SECS="300",\n'
  printf '    PG_VM_POOL_MAX_CONCURRENT_BRINGUPS="3",\n'
  printf '    PG_VM_POOL_WARM_SPARES="%s",\n' "$WARM_SPARES"
  printf '    PG_VM_POOL_DATA_DISK_GB="2",\n'
  printf '    PG_VM_POOL_DISK_GROW_PCT="85",\n'
  printf '    PG_VM_POOL_DISK_MAX_GB="25",\n'
  printf '    PG_VM_POOL_KEEPALIVE_SCHEMAS="",\n'
  printf '    PG_VM_POOL_RUN_DIR="%s",\n' "$RUN_DIR"
  printf '    PG_VM_POOL_ORPHAN_SWEEP_SECS="900",\n'
  printf '    PG_VM_POOL_RECLAIM_CMD="%s",\n' "$RECLAIM_CMD"
  printf '    PG_VM_POOL_COMPACT_AFTER_SECS="%s",\n' "$COMPACT_AFTER_SECS"
  printf '    PG_VM_POOL_OFFLOAD_WORKERS="4",\n'
  printf '    PG_VM_POOL_OFFLOAD_MAX_HOLDOFF_SECS="300",\n'
  if [ "$S3_MODE" = on ]; then
    printf '    PG_VM_POOL_ARCHIVE_AFTER_SECS="%s",\n' "$ARCHIVE_AFTER_SECS"
    printf '    PG_VM_POOL_IMAGE_ARCHIVE="1",\n'
    printf '    PG_VM_POOL_PRESSURE_PATH="%s",\n' "$RUN_DIR"
    printf '    PG_VM_POOL_PRESSURE_HIGH_PCT="85",\n'
    printf '    PG_VM_POOL_PRESSURE_LOW_PCT="75",\n'
    printf '    PG_VM_POOL_S3_BUCKET="%s",\n' "$PG_VM_POOL_S3_BUCKET"
    printf '    PG_VM_POOL_S3_ACCESS_KEY_ID="%s",\n' "$PG_VM_POOL_S3_ACCESS_KEY_ID"
    printf '    PG_VM_POOL_S3_SECRET_ACCESS_KEY="%s",\n' "$PG_VM_POOL_S3_SECRET_ACCESS_KEY"
    if [ -n "$PG_VM_POOL_S3_ENDPOINT" ]; then
      printf '    PG_VM_POOL_S3_ENDPOINT="%s",\n' "$PG_VM_POOL_S3_ENDPOINT"
    fi
    if [ -n "$PG_VM_POOL_S3_REGION" ]; then
      printf '    PG_VM_POOL_S3_REGION="%s",\n' "$PG_VM_POOL_S3_REGION"
    fi
  fi
  printf '    PG_VM_POOL_DASHBOARD_LISTEN="%s",\n' "$DASHBOARD_LISTEN"
  printf '    PG_VM_POOL_TLS_CERT="%s/fullchain.pem",\n' "$TLS_DIR"
  printf '    PG_VM_POOL_TLS_KEY="%s/privkey.pem",\n' "$TLS_DIR"
  printf '    RUST_LOG="info,pg_vm_pool=info",\n'
  printf '    PG_VM_POOL_PASSWORD="%s"\n\n' "$POOL_PASSWORD"
  if [ "$S3_MODE" = on ]; then
    printf ';; storage tiers: FULL ladder (idle reap -> freeze -> compact ->\n'
    printf ';; reclaim -> S3 archive -> pressure eviction). Idle schemas leave the host.\n'
  else
    printf ';; storage tiers: LOCAL ONLY — no S3 credentials were provided.\n'
    printf ';; ARCHIVE_AFTER_SECS, IMAGE_ARCHIVE and PRESSURE_PATH are all omitted:\n'
    printf ';; each is a startup error without a bucket. Idle schemas shrink on-host\n'
    printf ';; (freeze + compact + reclaim) but never leave it, so the data filesystem\n'
    printf ';; is a hard ceiling — nothing sheds disk automatically. Watch it.\n'
  fi
  cat <<'TAIL'

; TLS terminates at the pooler; the pooler->VM hop stays plaintext over the
; host-local tap. PG_VM_POOL_PASSWORD is answered as a CLEARTEXT challenge, so
; a public LISTEN is only safe with TLS configured — never widen one without
; the other.
autostart=true
autorestart=true
startsecs=5
startretries=3
stopsignal=TERM
stopwaitsecs=15
stopasgroup=true
killasgroup=true
redirect_stderr=true
stdout_logfile=/var/log/pg-vm-pool/pg-vm-pool.log
stdout_logfile_maxbytes=20MB
stdout_logfile_backups=5
TAIL
}
}

gen_heyvmd_conf > /tmp/heyvmd.conf
gen_pool_conf   > /tmp/pg-vm-pool.conf

sudo install -m 0644 -o root -g root /tmp/heyvmd.conf /etc/supervisor/conf.d/heyvmd.conf
# 0600: this one carries the pooler password and any S3 secret.
sudo install -m 0600 -o root -g root /tmp/pg-vm-pool.conf /etc/supervisor/conf.d/pg-vm-pool.conf
rm -f /tmp/heyvmd.conf /tmp/pg-vm-pool.conf

# ---- 13. start --------------------------------------------------------------

log "starting services"
sudo systemctl enable supervisor >/dev/null 2>&1 || true
sudo supervisorctl reread
sudo supervisorctl update
sleep 15
sudo supervisorctl status || true

# ---- 14. verify -------------------------------------------------------------

log "verifying"
fail=0
check() { if eval "$2" >/dev/null 2>&1; then printf '  ok    %s\n' "$1"; else printf '  FAIL  %s\n' "$1"; fail=1; fi; }

check "supervisor: all programs RUNNING" \
      '[ "$(sudo supervisorctl status | grep -c RUNNING)" -ge 3 ]'
check "heyvm API answers /health" \
      "curl -sf --max-time 10 http://127.0.0.1:${API_PORT}/health"
check "pooler RUN_DIR matches ${RUN_DIR}" \
      "tr '\\000' '\\n' < /proc/\$(pgrep -f target/release/pg-vm-pool | head -1)/environ | grep -qx 'PG_VM_POOL_RUN_DIR=${RUN_DIR}'"
check "VM disks land on ${DATA_MOUNT}" \
      "mountpoint -q ${DATA_MOUNT}"
check "sudoers reclaim pin matches" \
      "sudo -n -l ${BIN_DIR}/reclaim-disks.sh ${RUN_DIR} --shrink --prune-swap"
check "cert renewal is wired" \
      "sudo test -x /etc/letsencrypt/renewal-hooks/deploy/pg-vm-pool.sh"
check "certbot.timer enabled" \
      "systemctl is-enabled certbot.timer"

# End-to-end: a real client, over real TLS, against the public name. This is the
# only check that proves the whole chain (DNS -> TLS -> password -> tunnel ->
# a booted Firecracker VM running Postgres).
log "end-to-end TLS connect (boots a VM; may take ~60s on a cold host)"
if PGPASSWORD="$POOL_PASSWORD" psql \
     "host=${POOL_HOSTNAME} port=${POOL_LISTEN##*:} user=postgres dbname=provision_check sslmode=verify-full sslrootcert=system connect_timeout=180" \
     -tAc "select 'e2e-ok'" 2>/dev/null | grep -q e2e-ok; then
  printf '  ok    end-to-end TLS connect\n'
  # Leave no test residue: drop the VM and its registry row.
  sb=$(awk '/^provision_check\t/ {print $2}' "$STATE_DIR/registry.tsv" 2>/dev/null || true)
  sudo supervisorctl stop pg-vm-pool >/dev/null 2>&1
  [ -n "$sb" ] && curl -s -X DELETE --max-time 60 "http://127.0.0.1:${API_PORT}/sandboxes/${sb}" >/dev/null 2>&1
  sed -i '/^provision_check\t/d' "$STATE_DIR/registry.tsv" 2>/dev/null || true
  sudo supervisorctl start pg-vm-pool >/dev/null 2>&1
else
  printf '  FAIL  end-to-end TLS connect\n'; fail=1
fi

# ---- 15. summary ------------------------------------------------------------

echo
log "provisioned ${POOL_HOSTNAME}"
cat <<SUMMARY

  data           ${DATA_MOUNT}  ($(df -h "$DATA_MOUNT" | awk 'NR==2{print $2" total, "$4" free"}'))
  run dir        ${RUN_DIR}
  checkout       ${CHECKOUT} @ $(git -C "$CHECKOUT" rev-parse --short HEAD)
  pooler         ${POOL_LISTEN}  (TLS, password)
  dashboard      ${DASHBOARD_LISTEN}
  heyvm mode     ${HEYVM_MODE} — ${HEYVM_NOTE}
  S3 tier        ${S3_MODE}
  password       ${STATE_DIR}/admin-password.txt (0600)

  psql "host=${POOL_HOSTNAME} port=${POOL_LISTEN##*:} user=postgres \\
        dbname=<schema> sslmode=verify-full sslrootcert=system"

SUMMARY

if [ "$HEYVM_MODE" != cloud ]; then
  cat <<'SEC'
  SECURITY: heyvm --api binds 0.0.0.0 with authentication disabled — it will
  create, exec into and delete VMs for anyone who reaches the port. The bind is
  hardcoded; there is no flag for it. Block that port upstream, or provide
  HEYO_API_KEY so this host can run `heyvmd --socket-only`, which binds the TCP
  listener to loopback instead.

SEC
fi

[ "$fail" -eq 0 ] || die "one or more verification checks FAILED — see above"
log "all checks passed"
