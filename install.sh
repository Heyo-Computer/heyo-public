#!/bin/sh
# printer installer — downloads prebuilt binaries from a release bucket and
# drops them into ${PREFIX}/bin (default ~/.local/bin).
#
# Usage:
#   curl -fsSL https://<your-bucket>/install.sh | sh
#   curl -fsSL https://<your-bucket>/install.sh | PRINTER_VERSION=v0.2.0 sh
#   curl -fsSL https://<your-bucket>/install.sh | PRINTER_PREFIX=/usr/local sh
#
# Environment variables:
#   PRINTER_BASE_URL  Base URL of the GitHub releases endpoint. Defaults to
#                     this repo's releases; override to install from a fork:
#                     https://github.com/<owner>/<repo>/releases
#   PRINTER_VERSION   "latest" (default) or a tag like "v0.2.0".
#   PRINTER_PREFIX    Install prefix (default: $HOME/.local). Binaries land
#                     in $PRINTER_PREFIX/bin.
#   PRINTER_BINS      Space-separated subset of binaries to install
#                     (default: "printer computer codegraph").
#   PRINTER_NO_VERIFY If non-empty, skip sha256 verification.
#
# Expected layout (published by .github/workflows/build.yml on release):
#   latest:  ${PRINTER_BASE_URL}/latest/download/printer-${OS}-${ARCH}.tar.gz
#   tagged:  ${PRINTER_BASE_URL}/download/${VERSION}/printer-${OS}-${ARCH}.tar.gz
# plus a sibling "<tarball>.sha256". OS is "linux" or "darwin"; ARCH is
# "x86_64" or "aarch64". Each tarball contains the bare binaries at the top.

set -eu

# ---- defaults ---------------------------------------------------------------

DEFAULT_BASE_URL="https://github.com/Heyo-Computer/printer/releases"
BASE_URL="${PRINTER_BASE_URL:-$DEFAULT_BASE_URL}"
VERSION="${PRINTER_VERSION:-latest}"
PREFIX="${PRINTER_PREFIX:-$HOME/.local}"
BINS="${PRINTER_BINS:-printer computer codegraph}"
BINDIR="$PREFIX/bin"

# ---- output helpers ---------------------------------------------------------

info()  { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
warn()  { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()   { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ---- platform detection -----------------------------------------------------

detect_os() {
    case "$(uname -s)" in
        Linux)  echo linux ;;
        Darwin) echo darwin ;;
        *)      die "unsupported OS: $(uname -s)" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)         echo x86_64 ;;
        aarch64|arm64)        echo aarch64 ;;
        *)                    die "unsupported arch: $(uname -m)" ;;
    esac
}

# ---- prerequisites ----------------------------------------------------------

require() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

require uname
require tar
require mkdir
require install
if command -v curl >/dev/null 2>&1; then
    DOWNLOADER=curl
elif command -v wget >/dev/null 2>&1; then
    DOWNLOADER=wget
else
    die "curl or wget is required"
fi

download() {
    # download <url> <dest>
    if [ "$DOWNLOADER" = curl ]; then
        curl -fsSL --retry 3 -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

# ---- sha256 -----------------------------------------------------------------

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo ""
    fi
}

# ---- main -------------------------------------------------------------------

OS="$(detect_os)"
ARCH="$(detect_arch)"
TARBALL="printer-${OS}-${ARCH}.tar.gz"
# GitHub releases serve "latest" and tagged assets under different paths.
if [ "$VERSION" = latest ]; then
    URL="${BASE_URL}/latest/download/${TARBALL}"
else
    URL="${BASE_URL}/download/${VERSION}/${TARBALL}"
fi
SUM_URL="${URL}.sha256"

TMPDIR="$(mktemp -d 2>/dev/null || mktemp -d -t printer-install)"
trap 'rm -rf "$TMPDIR"' EXIT INT TERM

info "platform: ${OS}/${ARCH}"
info "version:  ${VERSION}"
info "source:   ${URL}"
info "target:   ${BINDIR}"

info "downloading ${TARBALL}"
download "$URL" "$TMPDIR/$TARBALL" \
    || die "failed to download $URL (does ${VERSION} exist for ${OS}/${ARCH}?)"

if [ -z "${PRINTER_NO_VERIFY:-}" ]; then
    if download "$SUM_URL" "$TMPDIR/$TARBALL.sha256" 2>/dev/null; then
        EXPECTED="$(awk '{print $1}' "$TMPDIR/$TARBALL.sha256")"
        ACTUAL="$(sha256_of "$TMPDIR/$TARBALL")"
        if [ -z "$ACTUAL" ]; then
            warn "no sha256 tool available, skipping verification"
        elif [ "$EXPECTED" != "$ACTUAL" ]; then
            die "sha256 mismatch: expected $EXPECTED, got $ACTUAL"
        else
            info "sha256 verified"
        fi
    else
        warn "no .sha256 published; skipping verification (set PRINTER_NO_VERIFY=1 to silence)"
    fi
fi

info "extracting"
tar -xzf "$TMPDIR/$TARBALL" -C "$TMPDIR" \
    || die "failed to extract $TARBALL"

mkdir -p "$BINDIR"
INSTALLED=""
for bin in $BINS; do
    src="$TMPDIR/$bin"
    if [ ! -f "$src" ]; then
        warn "tarball did not contain '$bin', skipping"
        continue
    fi
    install -m 0755 "$src" "$BINDIR/$bin"
    INSTALLED="$INSTALLED $bin"
    info "installed $bin -> $BINDIR/$bin"
done

if [ -z "$INSTALLED" ]; then
    die "nothing was installed (tarball had none of: $BINS)"
fi

# ---- PATH check -------------------------------------------------------------

case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *)
        warn "$BINDIR is not on your PATH"
        warn "add this to your shell rc:"
        warn "    export PATH=\"$BINDIR:\$PATH\""
        ;;
esac

info "done. try: printer --help"
