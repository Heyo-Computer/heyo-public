#!/bin/sh
# Install Heyo apps from the public release manifests — no credential needed.
#
#   curl -fsSL https://get.us2.heyo.work/install-apps.sh | sh -s -- app-lb heyvm
#   sh install-apps.sh --list
#   sh install-apps.sh --version cloud=0.43.0 cloud
#
# Installable: app-lb app-obs ci queue art heyosecret cloud auth retail heyvm
#
# The keyless counterpart of `.ci/install.sh`. That one resolves *tags*, which
# need `ART_API_KEY`; this one reads `<app>.json` manifests that
# `.ci/publish-releases.sh` wrote at release time (the same design as
# `app-lb/heyctl/install.sh`), downloads the public blob each names, and
# verifies it. It sends no Authorization header and must not learn to: the
# store rejects a *presented* stale key even for a public blob.
#
# ## Options
#
#   --list              Show what the manifests offer; install nothing.
#   --dry-run           Download and verify; install nothing.
#   --version APP=VER   Install VER of APP instead of its latest.
#   --supervisor        Install shipped supervisor units (default when
#                       /etc/supervisor/conf.d exists). Never over an existing
#                       file: the new one lands beside it as `.new`.
#   --no-units          Install no process-manager units at all.
#   --restart           `supervisorctl update` and restart what was installed.
#
# ## Environment
#
#   RELEASES_URL   Where the manifests are. Default https://get.us2.heyo.work
#   STORE_URL      Override the manifests' "store".
#   PREFIX         Binaries go in $PREFIX/bin. Default /usr/local.
#   STATE_ROOT     Migrations go in $STATE_ROOT/<app>/migrations. Default /var/lib.
#   OPT_ROOT       `tree` apps (auth) go in $OPT_ROOT/<app>. Default /opt/heyo.
#   SITE_ROOT      `site` apps (retail) go in $SITE_ROOT/<app>. Default /srv/heyo.
#   SUPERVISOR_DIR Default /etc/supervisor/conf.d.
#
# ## What each kind installs
#
#   bin      the named binaries into $PREFIX/bin (by rename, so a running binary
#            is replaced safely), migrations/ into $STATE_ROOT/<app>/migrations,
#            and <app>.conf into $SUPERVISOR_DIR
#   tree     the whole release into $OPT_ROOT/<app>/releases/<version>, then
#            $OPT_ROOT/<app>/current is repointed at it; run `current/start.sh`
#   site     same layout under $SITE_ROOT/<app>; serve `current/`
#   tarball  heyvm and heyvmd, from the release tarball inside the artifact
#
# ## What it verifies
#
# The blob digest (a blob's name is its sha256), then the build's own
# SHA256SUMS over the unpacked files. The manifest is the trust root, which is
# why it is served over HTTPS from a host we control.

set -eu

# The whole body runs from the last line, so a `curl | sh` cut off mid-transfer
# is a syntax error rather than half an install.
main() {

RELEASES_URL="${RELEASES_URL:-https://get.us2.heyo.work}"
RELEASES_URL="${RELEASES_URL%/}"
STORE_OVERRIDE="${STORE_URL:-}"
PREFIX="${PREFIX:-/usr/local}"
STATE_ROOT="${STATE_ROOT:-/var/lib}"
OPT_ROOT="${OPT_ROOT:-/opt/heyo}"
SITE_ROOT="${SITE_ROOT:-/srv/heyo}"
SUPERVISOR_DIR="${SUPERVISOR_DIR-/etc/supervisor/conf.d}"
PLATFORM="linux-x86_64"
KNOWN="app-lb app-obs ci queue art heyosecret cloud auth retail heyvm"

DO_LIST=0; DO_DRY=0; DO_RESTART=0; UNITS=auto
PINS=""; APPS=""

info() { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
step() { printf '    %s\n' "$*" >&2; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --list)       DO_LIST=1 ;;
        --dry-run)    DO_DRY=1 ;;
        --restart)    DO_RESTART=1 ;;
        --supervisor) UNITS=supervisor ;;
        --no-units)   UNITS=none ;;
        --version)    shift; [ $# -gt 0 ] || die "--version needs APP=VER"; PINS="$PINS $1" ;;
        -h|--help)    sed -n '2,52p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' >&2
                      echo "installable: $KNOWN" >&2; exit 0 ;;
        -*)           die "unknown option $1 (try --help)" ;;
        *)            APPS="$APPS $1" ;;
    esac
    shift
done

[ "$(uname -s)-$(uname -m)" = "Linux-x86_64" ] \
    || die "releases are built for linux-x86_64 only; this is $(uname -s)-$(uname -m)"
for tool in curl tar sha256sum awk sed mktemp; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required and was not found"
done
[ "$DO_LIST" = 1 ] && [ -z "$APPS" ] && APPS="$KNOWN"
[ -n "$APPS" ] || die "name the apps to install (installable: $KNOWN), or --list"

if [ "$UNITS" = auto ]; then
    if [ -n "$SUPERVISOR_DIR" ] && [ -d "$SUPERVISOR_DIR" ]; then UNITS=supervisor; else UNITS=none; fi
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/install-apps.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

field() { tr -d '\r\n' < "$1" | sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -1; }

# The header before `"artifacts"` holds the per-app fields; `field` on the whole
# file would find a record's `version` rather than the header's.
header() { tr -d '\r\n' < "$1" | sed 's/"artifacts".*//' > "$1.head"; field "$1.head" "$2"; }

records() {
    awk '
        BEGIN { RS = "{" }
        {
            rec = $0; gsub(/[ \t\r\n]/, "", rec)
            v = f(rec, "version"); p = f(rec, "platform"); d = f(rec, "digest")
            if (v != "" && p != "" && d != "") print v "\t" p "\t" d
        }
        function f(s, k,   m) {
            if (!match(s, "\"" k "\":\"[^\"]*\"")) return ""
            m = substr(s, RSTART, RLENGTH); sub("^\"" k "\":\"", "", m); sub("\"$", "", m)
            return m
        }
    ' "$1"
}

pin_for() {
    for p in $PINS; do case "$p" in "$1="*) printf '%s\n' "${p#*=}"; return 0 ;; esac; done
    return 0
}

fetch_manifest() {
    out="$TMP/$1.json"
    code="$(curl -sS -L -o "$out" -w '%{http_code}' "$RELEASES_URL/$1.json" 2>"$TMP/curl.err")" || code=000
    case "$code" in
        200) printf '%s\n' "$out" ;;
        404) [ "$DO_LIST" = 1 ] && return 1
             die "$1 has not been published at $RELEASES_URL" ;;
        000) die "could not reach $RELEASES_URL — $(tail -1 "$TMP/curl.err")" ;;
        *)   die "GET $RELEASES_URL/$1.json returned HTTP $code" ;;
    esac
}

writable_or_die() {
    d="$1"
    while [ ! -d "$d" ]; do d="$(dirname "$d")"; done
    [ -w "$d" ] || die "$1 is not writable (run as root, or set PREFIX/STATE_ROOT/OPT_ROOT)"
}

install_bin() {
    src="$1"; dest="$PREFIX/bin/$(basename "$1")"
    chmod 0755 "$src"
    mv -f "$src" "$dest.incoming.$$"
    mv -f "$dest.incoming.$$" "$dest"
    step "installed $dest"
}

install_unit() {
    app="$1"; conf="$2"
    [ "$UNITS" = supervisor ] && [ -f "$conf" ] || return 0
    dest="$SUPERVISOR_DIR/$app.conf"
    if [ -e "$dest" ]; then
        cp "$conf" "$dest.new"
        step "kept your $dest; the shipped one is at $dest.new"
    else
        cp "$conf" "$dest"
        step "installed $dest — it ships placeholder secrets; edit it before starting"
    fi
}

# Unpack into releases/<version> and swap `current` by rename, so a running
# service keeps its files and a rollback is one symlink.
install_tree() {
    app="$1"; root="$2"; version="$3"; src="$4"
    rel="$root/$app/releases/$version"
    writable_or_die "$root"
    mkdir -p "$root/$app/releases"
    rm -rf "$rel.incoming.$$"
    cp -a "$src" "$rel.incoming.$$"
    rm -rf "$rel"
    mv "$rel.incoming.$$" "$rel"
    ln -sfn "releases/$version" "$root/$app/current.incoming.$$"
    mv -Tf "$root/$app/current.incoming.$$" "$root/$app/current"
    step "installed $rel  (current -> releases/$version)"
}

installed=""
for app in $APPS; do
    case " $KNOWN " in *" $app "*) ;; *) die "unknown app '$app' (installable: $KNOWN)" ;; esac
    if ! m="$(fetch_manifest "$app")"; then
        # Outside --list, fetch_manifest has already said why; `die` in a
        # capture only ends the subshell, so end the install here.
        [ "$DO_LIST" = 1 ] || exit 1
        printf '  %-11s (not published)\n' "$app" >&2
        continue
    fi
    kind="$(header "$m" kind)"; bins="$(header "$m" bins)"
    latest="$(header "$m" latest)"; store="${STORE_OVERRIDE:-$(header "$m" store)}"
    store="${store%/}"
    [ -n "$kind" ] && [ -n "$store" ] || die "$RELEASES_URL/$app.json is malformed (no kind or store)"

    if [ "$DO_LIST" = 1 ]; then
        printf '  %-11s latest %-14s %s\n' "$app" "${latest:-?}" \
            "$(records "$m" | awk -F'\t' -v p="$PLATFORM" '$2 == p { printf "%s ", $1 }')" >&2
        continue
    fi

    want="$(pin_for "$app")"; want="${want:-$latest}"
    digest="$(records "$m" | awk -F'\t' -v v="$want" -v p="$PLATFORM" '$1 == v && $2 == p { print $3; exit }')"
    [ -n "$digest" ] || die "$app $want is not published for $PLATFORM (try --list)"
    case "$digest" in *[!0-9a-f]*) die "$app: manifest digest is not hex" ;; esac
    [ "${#digest}" = 64 ] || die "$app: manifest digest is not a sha256"

    info "$app $want"
    work="$TMP/$app"; mkdir -p "$work/u"
    code="$(curl -sS -L -o "$work/a.tar.gz" -w '%{http_code}' "$store/blobs/$digest" 2>"$TMP/curl.err")" || code=000
    case "$code" in
        200) ;;
        401|403) die "$app: the store refused an anonymous download (HTTP $code) — the blob is not public, or a gate in front of the store requires login for /blobs/" ;;
        *) die "$app: GET $store/blobs/$digest returned HTTP $code" ;;
    esac
    [ "$(sha256sum "$work/a.tar.gz" | cut -d' ' -f1)" = "$digest" ] || die "$app: download does not hash to $digest"
    tar -xzf "$work/a.tar.gz" -C "$work/u" --strip-components=1
    [ -f "$work/u/SHA256SUMS" ] || die "$app: the artifact carries no SHA256SUMS"
    ( cd "$work/u" && sha256sum -c SHA256SUMS >/dev/null ) || die "$app: SHA256SUMS does not match the unpacked files"
    step "digest and SHA256SUMS verified"

    if [ "$DO_DRY" = 1 ]; then
        step "dry run: would install $kind ($bins)"
        continue
    fi

    case "$kind" in
        bin)
            writable_or_die "$PREFIX/bin"; mkdir -p "$PREFIX/bin"
            for b in $bins; do
                [ -f "$work/u/$b" ] || die "$app: the artifact has no $b"
                install_bin "$work/u/$b"
            done
            if [ -d "$work/u/migrations" ]; then
                mdir="$STATE_ROOT/$app/migrations"
                writable_or_die "$mdir"; mkdir -p "$mdir"
                cp "$work/u/migrations/"*.sql "$mdir/"
                step "installed $(ls -1 "$work/u/migrations" | wc -l | tr -d ' ') migration(s) into $mdir"
            fi
            install_unit "$app" "$work/u/$app.conf" ;;
        tree)
            install_tree "$app" "$OPT_ROOT" "$want" "$work/u"
            install_unit "$app" "$work/u/$app.conf" ;;
        site)
            install_tree "$app" "$SITE_ROOT" "$want" "$work/u" ;;
        tarball)
            inner="$(ls "$work/u"/*.tar.gz 2>/dev/null | head -1)"
            [ -n "$inner" ] || die "$app: no release tarball inside the artifact"
            mkdir -p "$work/inner"
            tar -xzf "$inner" -C "$work/inner"
            writable_or_die "$PREFIX/bin"; mkdir -p "$PREFIX/bin"
            for b in $bins; do
                [ -f "$work/inner/$b" ] || die "$app: $(basename "$inner") has no $b"
                install_bin "$work/inner/$b"
            done ;;
        *) die "$app: unknown kind '$kind' — this installer is older than the manifest; fetch a new one" ;;
    esac
    installed="$installed $app"
done

[ "$DO_LIST" = 1 ] && exit 0
[ "$DO_DRY" = 1 ] && { info "dry run: nothing was installed"; exit 0; }
[ -n "$installed" ] || exit 0
info "installed:$installed"

if [ "$DO_RESTART" = 1 ] && [ "$UNITS" = supervisor ]; then
    command -v supervisorctl >/dev/null 2>&1 || die "--restart needs supervisorctl"
    supervisorctl reread >&2 || true
    supervisorctl update >&2 || true
    for a in $installed; do supervisorctl restart "$a" >&2 2>/dev/null || true; done
fi
}

main "$@"
