#!/bin/sh
# Install app-lb, app-obs and ci from the artifact store the workflows push to.
#
#   ART_URL=https://art.example.com ART_API_KEY=… sh .ci/install.sh
#   curl -fsSL https://…/install.sh | ART_URL=… ART_API_KEY=… sh
#
# The counterpart of `.ci/workflows/`: those build the binaries and push them to
# an `art serve`, and this pulls the newest of each back out and puts it where
# the shipped supervisor units expect to find it.
#
# ## Usage
#
#   sh .ci/install.sh [--list] [--dry-run] [--restart] [--ref TAG] [app...]
#
#   --list      Show what the store has, with each artifact's description, and
#               install nothing.
#   --dry-run   Resolve and download everything, verify it, and report what
#               would be installed without touching the host.
#   --restart   `supervisorctl update` and restart each installed program.
#               Off by default: when to bounce a running orchestrator is not a
#               decision an installer should make.
#   --ref TAG   Install this exact tag instead of the newest, for one app.
#               A rollback: `--ref ci-apps-019f…-app-lb-app-lb app-lb`.
#
# ## Environment
#
#   ART_URL          Base URL of the `art serve` (required).
#   ART_API_KEY      Bearer token for it (required unless the store is open).
#   PREFIX           Binaries go in $PREFIX/bin. Default /usr/local.
#   STATE_ROOT       Per-service state. Default /var/lib. `ci`'s migrations land
#                    in $STATE_ROOT/ci/migrations, which is what CI_MIGRATIONS_DIR
#                    in the shipped ci.conf points at.
#   SUPERVISOR_DIR   Where supervisor program files go. Default
#                    /etc/supervisor/conf.d. Set empty to skip them entirely.
#   APPS             Which to install. Default "app-lb app-obs ci".
#                    `codegraph` is also published and installable by name.
#
# ## How an artifact is found
#
# `ci` tags every upload `ci-<workflow>-<run>-<job>-<name>` (see
# `ci/src/artifacts.rs`), because the store's tag charset has no `/`. For the
# four things these workflows publish that is:
#
#   ci-apps-<run>-app-lb-app-lb          app-lb + heyctl
#   ci-apps-<run>-app-obs-app-obs        app-obs + app-obs-dump
#   ci-ci-<run>-release-ci               ci + its migrations
#   ci-codegraph-<run>-release-codegraph codegraph
#
# The run id is `%012x-%08x` — epoch milliseconds in hex, then a sequence — so
# it is fixed-width and zero-padded, and **sorting the tags lexicographically
# sorts them chronologically**. That is the one non-obvious thing this script
# depends on; if run ids ever stop being fixed-width hex, `newest_tag` silently
# starts picking the wrong build rather than failing, so the pattern below
# matches the exact widths as a tripwire.
#
# ## What it verifies
#
# A blob's name *is* the sha256 of its bytes, so checking the download against
# the digest it was fetched by is free and catches a truncated transfer, a proxy
# that helpfully rewrote something, or a store serving the wrong content. Then
# `sha256sum -c SHA256SUMS` runs inside the unpacked tree, which is the build's
# own statement about the files it produced.
#
# ## What it will not do
#
# **It never overwrites a supervisor config.** Those hold edited secrets — the
# shipped `app-lb.conf` ships `APP_LB_DASHBOARD_PASSWORD="change-me"` and every
# real deployment has changed it — so an upgrade that replaced them would revert
# production configuration without saying so. An existing file is left alone and
# the new one is written beside it as `<name>.conf.new`.

set -eu

# ---- defaults ---------------------------------------------------------------

ART_URL="${ART_URL:-}"
ART_API_KEY="${ART_API_KEY:-}"
PREFIX="${PREFIX:-/usr/local}"
STATE_ROOT="${STATE_ROOT:-/var/lib}"
# Unset and empty mean different things: unset takes the default, empty skips
# supervisor files. `${X-default}` (no colon) is what tells them apart.
SUPERVISOR_DIR="${SUPERVISOR_DIR-/etc/supervisor/conf.d}"
APPS="${APPS:-app-lb app-obs ci}"

BINDIR="$PREFIX/bin"
DO_LIST=0
DO_DRY=0
DO_RESTART=0
PIN_REF=""
SELECTED=""

# ---- output -----------------------------------------------------------------

info() { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
step() { printf '    %s\n' "$*" >&2; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ---- arguments --------------------------------------------------------------

while [ $# -gt 0 ]; do
  case "$1" in
    --list)    DO_LIST=1 ;;
    --dry-run) DO_DRY=1 ;;
    --restart) DO_RESTART=1 ;;
    --ref)     shift; [ $# -gt 0 ] || die "--ref needs a tag"; PIN_REF="$1" ;;
    --prefix)  shift; [ $# -gt 0 ] || die "--prefix needs a path"; PREFIX="$1"; BINDIR="$PREFIX/bin" ;;
    -h|--help) sed -n '2,70p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*)        die "unknown option $1 (try --help)" ;;
    *)         SELECTED="$SELECTED $1" ;;
  esac
  shift
done
[ -n "$SELECTED" ] && APPS="$SELECTED"

# ---- preflight --------------------------------------------------------------

for tool in curl tar sha256sum sort; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required and was not found"
done
[ -n "$ART_URL" ] || die "ART_URL is required — the base URL of the art serve to install from"
# Trailing slashes would produce `//tags`, which some proxies rewrite and others
# 404 on.
ART_URL="${ART_URL%/}"

if [ -z "$ART_API_KEY" ]; then
  warn "ART_API_KEY is unset; this only works against a store started without ART_API_KEY"
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/art-install.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM

# `die` inside `$( … )` exits the *subshell*, not the script — so a fatal error
# raised while capturing output silently becomes an empty string, and the caller
# carries on with it. That is not hypothetical: it is how a rejected API key
# first showed up here, as four cheerful lines of "nothing published".
#
# So every fatal path writes its reason to this file before dying, and a parent
# that captured output calls `checkpoint` to turn it back into an exit.
FATAL="$TMP/fatal"

fail() { printf '%s\n' "$*" > "$FATAL"; die "$*"; }

checkpoint() {
  if [ -s "$FATAL" ]; then
    die "$(cat "$FATAL")"
  fi
}

# ---- the store --------------------------------------------------------------

# GET a path into a file and print the HTTP status.
#
# The status is returned rather than folded into an exit code because the three
# ways this fails need three different sentences, and the one people will
# actually hit is a mistyped key. `curl -f` would collapse all of them into
# "exit 22", which is how an earlier version of this script reported a rejected
# key as "nothing published".
#
# A connect failure prints `000`, which is curl's own convention for "no
# response", so the caller has one thing to match on.
http_get() {
  path="$1"; out="$2"
  if [ -n "$ART_API_KEY" ]; then
    code="$(curl -sS -L -o "$out" -w '%{http_code}' \
      -H "Authorization: Bearer $ART_API_KEY" "$ART_URL$path" 2>>"$TMP/curl.err")" || true
  else
    code="$(curl -sS -L -o "$out" -w '%{http_code}' "$ART_URL$path" 2>>"$TMP/curl.err")" || true
  fi
  case "$code" in
    ''|*[!0-9]*) echo 000 ;;
    *)           echo "$code" ;;
  esac
}

# GET a path, or die naming the reason. For the requests whose absence is fatal.
api() {
  body="$TMP/body.$$"
  code="$(http_get "$1" "$body")"
  case "$code" in
    200) cat "$body"; rm -f "$body" ;;
    000) rm -f "$body"; fail "could not reach $ART_URL — $(tail -1 "$TMP/curl.err" 2>/dev/null || echo 'no route to the store')" ;;
    401|403) rm -f "$body"; fail "the store rejected the credential (HTTP $code). Check ART_API_KEY against the store's own ART_API_KEY" ;;
    404) rm -f "$body"; fail "$ART_URL$1 is not there (HTTP 404) — is ART_URL the base of an \`art serve\`?" ;;
    *) rm -f "$body"; fail "GET $ART_URL$1 failed with HTTP $code" ;;
  esac
}

# Every tag the store holds, fetched once.
#
# Once because the answer is the same for every app and every question this
# script asks of it, and a listing grows with the store: re-fetching it per app
# turned a four-app install into eight round trips of the largest response the
# store serves.
TAGS_CACHE="$TMP/tags.txt"
load_tags() {
  if [ -f "$TAGS_CACHE" ]; then
    return 0
  fi
  api /tags | tr ',' '\n' | sed -n 's/.*"tag":"\([^"]*\)".*/\1/p' > "$TAGS_CACHE"
  # `api` is the *head of a pipeline*, which is a subshell too — so its `fail`
  # got as far as recording the reason and no further, and without this the
  # empty result below would be reported as "nothing has been published",
  # which is a wrong answer to "your key was rejected".
  checkpoint
  # A genuinely empty store is a legitimate answer, and a confusing one to meet
  # later as four separate "nothing published" lines.
  [ -s "$TAGS_CACHE" ] || die "$ART_URL holds no tags at all — nothing has been published to it yet"
}

# The store's JSON is serde output with a known shape and values from a
# constrained charset — a tag is `[A-Za-z0-9_.-]`, a digest is 64 lowercase hex
# — so these anchored extractions are reliable without a JSON parser, and this
# script stays runnable on a bare VM with nothing but coreutils and curl.
#
# What would break them: a store that pretty-prints its responses, or a `name`
# containing a quote. Neither is a shape this store produces.

# The cached listing, loaded on first use.
all_tags() {
  load_tags
  cat "$TAGS_CACHE"
}

# The blob digest a manifest's single entry names. Anchored on the `"digest"`
# key, which appears only inside `entries` — annotations hold a run id and a
# commit sha, neither of which is 64 hex.
manifest_blob() {
  api "/manifests/$1" \
    | grep -oE '"digest":"[0-9a-f]{64}"' \
    | head -1 \
    | sed 's/.*:"\(.*\)"/\1/'
}

# What somebody called this artifact, from the label the upload set.
#
# Deliberately does *not* use `api`: this is a caption, and every way it can
# fail — a store predating labels, an artifact nobody named, a blip — should
# leave the install working and one line quieter. A missing description is not a
# reason to refuse to install a verified binary.
artifact_description() {
  body="$TMP/label.$$"
  code="$(http_get "/labels/$1" "$body")"
  if [ "$code" = 200 ]; then
    sed -n 's/.*"description":"\([^"]*\)".*/\1/p' "$body" | head -1
  fi
  rm -f "$body"
}

# ---- the apps ---------------------------------------------------------------

# `workflow job artifact-name`, matching the table in .ci/README.md.
app_coords() {
  case "$1" in
    app-lb)    echo "apps app-lb app-lb" ;;
    app-obs)   echo "apps app-obs app-obs" ;;
    ci)        echo "ci release ci" ;;
    codegraph) echo "codegraph release codegraph" ;;
    *) return 1 ;;
  esac
}

# What lands in $BINDIR for each app. The tarball holds more — a supervisor
# config, BUILD-INFO, SHA256SUMS, and for `ci` a migrations directory — which is
# handled separately because none of it is a binary.
app_bins() {
  case "$1" in
    app-lb)    echo "app-lb heyctl" ;;
    app-obs)   echo "app-obs app-obs-dump" ;;
    ci)        echo "ci" ;;
    codegraph) echo "codegraph" ;;
  esac
}

# The newest tag for an app, or empty.
#
# The run id's exact widths are in the pattern deliberately: they are what makes
# `sort` chronological, so a tag that does not have them must not be considered
# rather than silently sorting somewhere arbitrary.
newest_tag() {
  wf="$1"; job="$2"; name="$3"
  all_tags \
    | grep -E "^ci-${wf}-[0-9a-f]{12}-[0-9a-f]{8}-${job}-${name}\$" \
    | sort \
    | tail -1
}

# ---- list -------------------------------------------------------------------

# Eagerly, and in the main shell: every later use is a `$( … )` capture, where a
# failure could not have exited the script. Doing it here means an unreachable
# store or a bad key is one clear line before any work starts.
load_tags

if [ "$DO_LIST" = 1 ]; then
  info "artifacts in $ART_URL"
  for app in $APPS; do
    coords="$(app_coords "$app")" || { warn "unknown app '$app'"; continue; }
    # shellcheck disable=SC2086
    set -- $coords
    tag="$(newest_tag "$1" "$2" "$3")"
    if [ -z "$tag" ]; then
      printf '  %-10s %s\n' "$app" "(nothing published)" >&2
      continue
    fi
    printf '  %-10s %s\n' "$app" "$tag" >&2
    desc="$(artifact_description "$tag" || true)"
    [ -n "$desc" ] && printf '  %-10s %s\n' "" "$desc" >&2
    count="$(all_tags | grep -cE "^ci-${1}-[0-9a-f]{12}-[0-9a-f]{8}-${2}-${3}\$" || true)"
    printf '  %-10s %s\n' "" "$count build(s) kept; --ref <tag> installs an older one" >&2
  done
  exit 0
fi

# ---- install ----------------------------------------------------------------

# Refuse early rather than after a multi-megabyte download, and name the
# directory rather than saying "permission denied".
writable_or_die() {
  d="$1"
  if [ -d "$d" ]; then
    [ -w "$d" ] || die "$d is not writable (run as root, or set PREFIX/STATE_ROOT)"
  else
    parent="$(dirname "$d")"
    [ -w "$parent" ] || die "cannot create $d ($parent is not writable)"
  fi
}

[ "$DO_DRY" = 1 ] || writable_or_die "$BINDIR"

installed=""
skipped_confs=""

for app in $APPS; do
  coords="$(app_coords "$app")" || die "unknown app '$app' (known: app-lb app-obs ci codegraph)"
  # shellcheck disable=SC2086
  set -- $coords
  wf="$1"; job="$2"; name="$3"

  if [ -n "$PIN_REF" ]; then
    tag="$PIN_REF"
  else
    tag="$(newest_tag "$wf" "$job" "$name")"
  fi
  [ -n "$tag" ] || die "no published build of $app found in $ART_URL (looked for ci-$wf-<run>-$job-$name)"

  info "$app  $tag"
  desc="$(artifact_description "$tag" || true)"
  [ -n "$desc" ] && step "$desc"

  digest="$(manifest_blob "$tag")"
  checkpoint
  [ -n "$digest" ] || die "$tag resolves to no blob; the manifest may be malformed"

  work="$TMP/$app"
  mkdir -p "$work/unpacked"
  tarball="$work/artifact.tar.gz"
  step "fetching ${digest%${digest#????????}}…"
  code="$(http_get "/blobs/$digest" "$tarball")"
  [ "$code" = 200 ] || die "$app: GET /blobs/$digest returned HTTP $code — the tag resolves to a blob the store does not hold"

  # The blob's name is the sha256 of its bytes, so this is both free and the
  # strongest check available: it covers the transfer and the store together.
  actual="$(sha256sum "$tarball" | cut -d' ' -f1)"
  [ "$actual" = "$digest" ] || die "$app: downloaded bytes hash to $actual, not the $digest they were fetched by"

  # `dist/` is the directory the workflow tarred, so one component comes off.
  tar -xzf "$tarball" -C "$work/unpacked" --strip-components=1

  # The build's own statement about what it produced. Absent on an artifact
  # older than the workflows that write it, which is a warning and not a stop.
  if [ -f "$work/unpacked/SHA256SUMS" ]; then
    ( cd "$work/unpacked" && sha256sum -c SHA256SUMS >/dev/null ) \
      || die "$app: SHA256SUMS does not match the unpacked files"
    step "SHA256SUMS verified"
  else
    warn "$app: no SHA256SUMS in the artifact; only the blob digest was checked"
  fi

  [ -f "$work/unpacked/BUILD-INFO" ] && sed 's/^/    /' "$work/unpacked/BUILD-INFO" >&2

  if [ "$DO_DRY" = 1 ]; then
    step "dry run: would install $(app_bins "$app" | tr ' ' ',') into $BINDIR"
    continue
  fi

  mkdir -p "$BINDIR"
  for b in $(app_bins "$app"); do
    src="$work/unpacked/$b"
    [ -f "$src" ] || die "$app: the artifact has no $b"
    chmod 0755 "$src"
    # Into place by rename. A running binary cannot be written through
    # (ETXTBSY) but can be renamed over: the old inode stays alive for the
    # process holding it, and the next start picks up the new one.
    mv -f "$src" "$BINDIR/$b.incoming.$$"
    mv -f "$BINDIR/$b.incoming.$$" "$BINDIR/$b"
    step "installed $BINDIR/$b"
  done

  # `ci` re-executes its migrations on every start, so they are a runtime
  # dependency of the binary rather than a development file, and they belong
  # where CI_MIGRATIONS_DIR in the shipped ci.conf points.
  if [ -d "$work/unpacked/migrations" ]; then
    mdir="$STATE_ROOT/$app/migrations"
    writable_or_die "$(dirname "$mdir")"
    mkdir -p "$mdir"
    cp "$work/unpacked/migrations/"*.sql "$mdir/"
    step "installed $(ls -1 "$work/unpacked/migrations" | wc -l) migration(s) into $mdir"
  fi

  # Supervisor programs, if this host uses supervisord and the artifact carries
  # one. Never over an existing file: see the note at the top.
  conf="$work/unpacked/$app.conf"
  if [ -n "$SUPERVISOR_DIR" ] && [ -f "$conf" ]; then
    if [ -d "$SUPERVISOR_DIR" ]; then
      dest="$SUPERVISOR_DIR/$app.conf"
      if [ -e "$dest" ]; then
        cp "$conf" "$dest.new"
        step "kept your $dest; the shipped one is at $dest.new"
        skipped_confs="$skipped_confs $app"
      else
        cp "$conf" "$dest"
        step "installed $dest — edit it before starting: it ships placeholder secrets"
      fi
    else
      step "no $SUPERVISOR_DIR on this host; skipping $app.conf"
    fi
  fi

  installed="$installed $app"
done

# ---- afterwards -------------------------------------------------------------

[ "$DO_DRY" = 1 ] && { info "dry run: nothing was installed"; exit 0; }
[ -n "$installed" ] || { warn "nothing was installed"; exit 0; }

info "installed:$installed"

if [ -n "$skipped_confs" ]; then
  warn "existing supervisor configs kept for:$skipped_confs"
  warn "diff them against the .new files — a new setting will not apply until you merge it"
fi

if [ "$DO_RESTART" = 1 ]; then
  command -v supervisorctl >/dev/null 2>&1 || die "--restart needs supervisorctl on PATH"
  info "reloading supervisord"
  supervisorctl reread >&2 || true
  supervisorctl update >&2 || true
  for app in $installed; do
    supervisorctl restart "$app" >&2 || warn "could not restart $app"
  done
else
  info "not restarting anything — when to bounce these is your call:"
  printf '    supervisorctl reread && supervisorctl update\n' >&2
  for app in $installed; do
    printf '    supervisorctl restart %s\n' "$app" >&2
  done
fi
