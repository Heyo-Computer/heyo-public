#!/bin/sh
# Generate the `versions.json` that `install.sh` reads — the release half of the
# public install story.
#
#   ART_API_KEY=… sh app-lb/heyctl/publish-versions.sh \
#       --from-url https://heyo.computer/heyctl/versions.json \
#       --out versions.json
#
# Then upload the result to https://heyo.computer/heyctl/versions.json.
#
# ## Why this exists
#
# `install.sh` runs for a stranger with no credential, and the store's anonymous
# carve-out is exactly one route: `GET /blobs/{digest}` for a blob marked public
# (`artifacts/src/http.rs`, `authorize`). Tags — which is how a build is
# actually found — stay behind `ART_API_KEY`. So somebody with the key has to
# resolve "the newest build" to a digest *once*, at release time, and write it
# down somewhere anonymous. That is `versions.json`, and this writes it.
#
# The division of labour is the point: this script holds the credential and
# runs when a release happens; `install.sh` holds nothing and runs on strangers'
# machines. Nothing that needs the key is ever on the public side of the line.
#
# ## What it does
#
#   1. Finds the newest `ci-app-lb-<run>-release-app-lb` tag, or the `--ref` you
#      name, and resolves it to a blob digest.
#   2. Downloads that blob and checks it hashes to the digest it was fetched by.
#   3. Unpacks it and reads the version out of `BUILD-INFO` — which the workflow
#      wrote as `heyctl --version`, so the manifest says what the binary says.
#      Refuses to continue if the tarball has no `heyctl` in it.
#   4. Makes sure the blob is public, and marks it public if it is not.
#   5. Merges an entry into the manifest, keeping the versions already there so
#      that `install.sh --version 0.1.6` keeps working after 0.1.7 ships.
#
# ## Usage
#
#   sh publish-versions.sh [options]
#
#   --out FILE        Where to write. Default ./versions.json. `-` is stdout.
#   --from-url URL    Merge into the manifest currently live at URL. This is
#                     the normal way to run it: the live file is the state,
#                     and nobody has to keep a copy in a working tree.
#   --from FILE       Merge into a local manifest instead.
#   --ref TAG         Publish this exact build rather than the newest.
#   --version VER     Override the version from BUILD-INFO. For an artifact old
#                     enough not to carry one.
#   --platform P      Default linux-x86_64, which is all CI builds today.
#   --keep N          Keep at most N versions per platform, newest kept first.
#                     Default: keep everything.
#   --no-latest       Add the entry but leave `"latest"` pointing where it is.
#                     For publishing a build people can opt into by version
#                     before it becomes the default download.
#   --no-public       Fail instead of marking the blob public. For a dry check
#                     that CI already did it.
#   --dry-run         Do everything, write nothing, change nothing in the store.
#   -h, --help        This.
#
# ## Environment
#
#   ART_URL      Base URL of the `art serve`. Default https://art.us2.heyo.work
#   ART_API_KEY  Bearer token for it. Required: tags are not public.

set -eu

# ---- defaults ---------------------------------------------------------------

ART_URL="${ART_URL:-https://art.us2.heyo.work}"
ART_API_KEY="${ART_API_KEY:-}"

OUT="versions.json"
FROM_URL=""
FROM_FILE=""
PIN_REF=""
VERSION=""
PLATFORM="linux-x86_64"
KEEP=0
SET_LATEST=1
ALLOW_PUBLIC=1
DRY=0

# The coordinates `ci` flattens into a tag, from the table in .ci/README.md.
WORKFLOW="app-lb"
JOB="release"
NAME="app-lb"
BIN="heyctl"

# ---- output -----------------------------------------------------------------

info() { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
step() { printf '    %s\n' "$*" >&2; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# The header block, from below the shebang to the first line that is not a
# comment. A line-number range drifts the moment the header grows: this one had
# already started printing `set -eu` at people.
usage() { sed -n '2,$ { /^#/!q; s/^# \{0,1\}//; p; }' "$0"; }

# ---- arguments --------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
        --out)      shift; [ $# -gt 0 ] || die "--out needs a path";     OUT="$1" ;;
        --from-url) shift; [ $# -gt 0 ] || die "--from-url needs a URL"; FROM_URL="$1" ;;
        --from)     shift; [ $# -gt 0 ] || die "--from needs a path";    FROM_FILE="$1" ;;
        --ref)      shift; [ $# -gt 0 ] || die "--ref needs a tag";      PIN_REF="$1" ;;
        --version)  shift; [ $# -gt 0 ] || die "--version needs a value"; VERSION="$1" ;;
        --platform) shift; [ $# -gt 0 ] || die "--platform needs a value"; PLATFORM="$1" ;;
        --keep)     shift; [ $# -gt 0 ] || die "--keep needs a count";   KEEP="$1" ;;
        --no-latest) SET_LATEST=0 ;;
        --no-public) ALLOW_PUBLIC=0 ;;
        --dry-run)  DRY=1 ;;
        -h|--help)  usage; exit 0 ;;
        *)          usage >&2; die "unknown argument: $1" ;;
    esac
    shift
done

[ -n "$FROM_URL" ] && [ -n "$FROM_FILE" ] \
    && die "--from-url and --from both name the manifest to merge into; pick one"

case "$KEEP" in
    *[!0-9]*) die "--keep needs a number" ;;
esac

# ---- preflight --------------------------------------------------------------

for tool in curl tar awk sed sort grep sha256sum mktemp; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required and was not found"
done

# Trailing slashes produce `//tags`, which some proxies rewrite and others 404.
ART_URL="${ART_URL%/}"
[ -n "$ART_API_KEY" ] || die "ART_API_KEY is required — tags are not public, which is the whole reason this script exists"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/heyctl-publish.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

# `die` inside `$( … )` exits the *subshell*, so a fatal error raised while
# capturing output silently becomes an empty string and the caller carries on
# with it. `.ci/install.sh` learned this the hard way — a rejected API key
# surfaced as "nothing published". Every fatal path in a capture writes its
# reason here first, and the parent calls `checkpoint` to turn it into an exit.
FATAL="$TMP/fatal"
fail() { printf '%s\n' "$*" > "$FATAL"; die "$*"; }
checkpoint() { [ -s "$FATAL" ] && die "$(cat "$FATAL")"; return 0; }

# ---- the store --------------------------------------------------------------

# GET a path into a file and print the HTTP status. The status is returned
# rather than folded into an exit code because the ways this fails need
# different sentences, and the one people hit is a mistyped key — `curl -f`
# collapses all of them into "exit 22".
http_get() {
    path="$1"; out="$2"
    code="$(curl -sS -L -o "$out" -w '%{http_code}' \
        -H "Authorization: Bearer $ART_API_KEY" "$ART_URL$path" 2>>"$TMP/curl.err")" || true
    case "$code" in
        ''|*[!0-9]*) echo 000 ;;
        *)           echo "$code" ;;
    esac
}

http_put() {
    path="$1"; out="$2"
    code="$(curl -sS -L -X PUT -o "$out" -w '%{http_code}' \
        -H "Authorization: Bearer $ART_API_KEY" "$ART_URL$path" 2>>"$TMP/curl.err")" || true
    case "$code" in
        ''|*[!0-9]*) echo 000 ;;
        *)           echo "$code" ;;
    esac
}

api() {
    body="$TMP/body.$$"
    code="$(http_get "$1" "$body")"
    case "$code" in
        200) cat "$body"; rm -f "$body" ;;
        000) rm -f "$body"; fail "could not reach $ART_URL — $(tail -1 "$TMP/curl.err" 2>/dev/null || echo 'no route to the store')" ;;
        401|403) rm -f "$body"; fail "the store rejected the credential (HTTP $code). Check ART_API_KEY against the store's own ART_API_KEY" ;;
        404) rm -f "$body"; fail "$ART_URL$1 is not there (HTTP 404) — check the tag name, and that ART_URL is the base of an \`art serve\`" ;;
        *) rm -f "$body"; fail "GET $ART_URL$1 failed with HTTP $code" ;;
    esac
}

# The store's JSON is serde output over a constrained charset — a tag is
# `[A-Za-z0-9_.-]`, a digest is 64 lowercase hex — so anchored extraction is
# reliable without a JSON parser, and this stays runnable on a bare VM.
#
# Whitespace comes out first so the patterns below need not carry `[[:space:]]*`
# around every colon. The store answers compactly today; a proxy that
# pretty-printed on the way through would otherwise turn every one of these
# into a silent empty string. Safe because nothing parsed here — tags, digests,
# booleans — can contain a space.
strip_ws() { tr -d ' \t\r\n'; }
all_tags() {
    api /tags | strip_ws | tr ',' '\n' | sed -n 's/.*"tag":"\([^"]*\)".*/\1/p'
}

# The blob digest a manifest's single entry names. Anchored on `"digest"`,
# which appears only inside `entries` — annotations hold a run id and a commit
# sha, neither of which is 64 hex.
manifest_blob() {
    api "/manifests/$1" | strip_ws | grep -oE '"digest":"[0-9a-f]{64}"' | head -1 | sed 's/.*:"\(.*\)"/\1/'
}

# The run id is `%012x-%08x` — epoch milliseconds in hex then a sequence — so it
# is fixed-width and sorting the tags lexicographically sorts them
# chronologically. That is the one non-obvious thing this depends on, so the
# exact widths are in the pattern as a tripwire: if run ids ever stop being
# fixed-width hex, this finds nothing rather than silently picking the wrong
# build.
newest_tag() {
    all_tags | grep -E "^ci-${WORKFLOW}-[0-9a-f]{12}-[0-9a-f]{8}-${JOB}-${NAME}\$" | sort | tail -1
}

# ---- resolve the build ------------------------------------------------------

info "store: $ART_URL"

if [ -n "$PIN_REF" ]; then
    TAG="$PIN_REF"
    step "using the build you named"
else
    TAG="$(newest_tag)"
    checkpoint
    [ -n "$TAG" ] || die "no published build found (looked for ci-$WORKFLOW-<run>-$JOB-$NAME). Has the app-lb workflow run against this store?"
fi
info "build: $TAG"

DIGEST="$(manifest_blob "$TAG")"
checkpoint
[ -n "$DIGEST" ] || die "$TAG resolves to no blob; the manifest may be malformed, or the tag may not exist"
step "digest: $DIGEST"

# ---- fetch and inspect ------------------------------------------------------

TARBALL="$TMP/artifact.tar.gz"
step "downloading"
code="$(http_get "/blobs/$DIGEST" "$TARBALL")"
[ "$code" = 200 ] || die "GET /blobs/$DIGEST returned HTTP $code — the tag resolves to a blob the store does not hold"

# A blob's name *is* the sha256 of its bytes, so this is free and covers the
# transfer and the store together. It matters more here than anywhere: whatever
# this script writes down is what every future `install.sh` will trust.
ACTUAL="$(sha256sum "$TARBALL" | cut -d' ' -f1)"
[ "$ACTUAL" = "$DIGEST" ] \
    || die "the download hashes to $ACTUAL, not the $DIGEST it was fetched by"
step "digest verified"

mkdir -p "$TMP/unpacked"
tar -xzf "$TARBALL" -C "$TMP/unpacked" || die "could not extract the artifact"

# Publishing a manifest that points at a tarball with no heyctl in it would
# produce an install that fails for everyone, at the last step, with the
# installer blamed. Check here, where it is one line and nobody is watching.
SRC="$(find "$TMP/unpacked" -type f -name "$BIN" -print 2>/dev/null | head -1)"
[ -n "$SRC" ] || die "$TAG contains no '$BIN' — is $NAME the right artifact?"
step "contains $BIN"

BUILD_INFO="$(dirname "$SRC")/BUILD-INFO"
if [ -z "$VERSION" ]; then
    [ -f "$BUILD_INFO" ] \
        || die "$TAG has no BUILD-INFO to read a version from; pass --version"
    # The workflow wrote `version   $(dist/heyctl --version)`, i.e.
    # "version   heyctl 0.1.7", so the last field is the version itself.
    VERSION="$(awk '$1 == "version" { print $NF; exit }' "$BUILD_INFO")"
    [ -n "$VERSION" ] || die "BUILD-INFO in $TAG has no version line; pass --version"
fi

# The installer matches this string literally inside the manifest, and it ends
# up inside a JSON string this script writes by hand. Anything outside this
# charset would either break the JSON or silently fail to match.
case "$VERSION" in
    *[!A-Za-z0-9._+-]* | "") die "version '$VERSION' has characters that do not belong in a version" ;;
esac
case "$PLATFORM" in
    *[!A-Za-z0-9._-]* | "") die "platform '$PLATFORM' has characters that do not belong in a platform" ;;
esac

info "heyctl $VERSION  ($PLATFORM)"
[ -f "$BUILD_INFO" ] && sed 's/^/    /' "$BUILD_INFO" >&2

# ---- make sure it is anonymously fetchable ----------------------------------

# The whole manifest is a promise that `GET /blobs/{digest}` answers without a
# credential. CI already sets this (`public: true` on the upload), so the usual
# outcome is a one-line confirmation — but a manifest published over a private
# blob is an install that 401s for every stranger and works for whoever tests
# it with a key in their environment.
PUBLIC_BODY="$TMP/public.json"
code="$(http_get "/public/$DIGEST" "$PUBLIC_BODY")"
[ "$code" = 200 ] || die "GET /public/$DIGEST returned HTTP $code; cannot tell whether the blob is anonymously fetchable"

if strip_ws < "$PUBLIC_BODY" | grep -q '"public":true'; then
    step "blob is already public"
elif [ "$DRY" = 1 ]; then
    warn "blob is NOT public; would PUT /public/$DIGEST (dry run, not doing it)"
elif [ "$ALLOW_PUBLIC" = 0 ]; then
    die "blob is not public and --no-public was given. Mark it with: art public $DIGEST"
else
    step "blob is not public; marking it"
    code="$(http_put "/public/$DIGEST" "$TMP/put.json")"
    case "$code" in
        200) step "marked public" ;;
        403) die "the store refused to mark it public (HTTP 403) — is it running with ART_READ_ONLY?" ;;
        *)   die "PUT /public/$DIGEST returned HTTP $code" ;;
    esac
fi

# ---- merge ------------------------------------------------------------------

# Existing entries are kept so that a version somebody pinned goes on working
# after a newer one ships. Regenerating the file from the store alone would
# quietly break every pinned install the day it ran.
EXISTING="$TMP/existing.json"
: > "$EXISTING"
if [ -n "$FROM_URL" ]; then
    if curl -fsSL --retry 3 -o "$EXISTING" "$FROM_URL"; then
        step "merging into $FROM_URL"
    else
        # A first release has nothing live yet, and that is not an error — but
        # a typo in the URL looks identical, so say which assumption was made.
        warn "could not fetch $FROM_URL; starting a new manifest"
        : > "$EXISTING"
    fi
elif [ -n "$FROM_FILE" ]; then
    [ -f "$FROM_FILE" ] || die "$FROM_FILE does not exist"
    cp "$FROM_FILE" "$EXISTING"
    step "merging into $FROM_FILE"
elif [ "$OUT" != "-" ] && [ -f "$OUT" ]; then
    cp "$OUT" "$EXISTING"
    step "merging into the existing $OUT"
fi

# Records as `version<TAB>platform<TAB>digest<TAB>bin`, one per line. The
# manifest's flat shape is what makes this a splitting problem rather than a
# parsing one: every `{ … }` under `artifacts` is one record with no nesting,
# so `RS="{"` cuts them apart exactly.
records() {
    [ -s "$1" ] || return 0
    awk '
        BEGIN { RS = "{" }
        {
            rec = $0
            gsub(/[ \t\r\n]/, "", rec)
            v = field(rec, "version"); p = field(rec, "platform")
            d = field(rec, "digest");  b = field(rec, "bin")
            if (v != "" && p != "" && d != "") {
                if (b == "") b = "heyctl"
                print v "\t" p "\t" d "\t" b
            }
        }
        function field(s, k,   m) {
            if (!match(s, "\"" k "\":\"[^\"]*\"")) return ""
            m = substr(s, RSTART, RLENGTH)
            sub("^\"" k "\":\"", "", m)
            sub("\"$", "", m)
            return m
        }
    ' "$1"
}

OLD="$TMP/old.tsv"; NEW="$TMP/new.tsv"
records "$EXISTING" > "$OLD"

# The new record first, then everything else except the (version, platform) it
# supersedes — so re-running for the same version replaces rather than
# duplicates, and the newest published build reads at the top of the file.
printf '%s\t%s\t%s\t%s\n' "$VERSION" "$PLATFORM" "$DIGEST" "$BIN" > "$NEW"
awk -F'\t' -v v="$VERSION" -v p="$PLATFORM" '!($1 == v && $2 == p)' "$OLD" >> "$NEW"

if [ "$KEEP" -gt 0 ]; then
    # Per platform, because "keep 5" means five installable versions on each
    # platform, not five lines in the file.
    awk -F'\t' -v keep="$KEEP" '{ if (++n[$2] <= keep) print }' "$NEW" > "$NEW.kept"
    dropped=$(( $(wc -l < "$NEW") - $(wc -l < "$NEW.kept") ))
    [ "$dropped" -gt 0 ] && step "dropped $dropped entr(ies) beyond --keep $KEEP"
    mv "$NEW.kept" "$NEW"
fi

LATEST="$VERSION"
if [ "$SET_LATEST" = 0 ]; then
    # Keep whatever the merged-in manifest already declared. With nothing to
    # keep, there is no honest answer but this one.
    prev="$(tr -d ' \t\r\n' < "$EXISTING" 2>/dev/null | sed -n 's/.*"latest":"\([^"]*\)".*/\1/p' | head -1)"
    if [ -n "$prev" ]; then
        LATEST="$prev"
        step "leaving \"latest\" at $prev"
    else
        warn "--no-latest, but the manifest declares no latest; using $VERSION"
    fi
fi

# ---- emit -------------------------------------------------------------------

RENDERED="$TMP/versions.json"
{
    printf '{\n'
    printf '  "latest": "%s",\n' "$LATEST"
    printf '  "store": "%s",\n' "$ART_URL"
    printf '  "artifacts": [\n'
    awk -F'\t' '
        {
            printf "%s    { \"version\": \"%s\",\n", (NR > 1 ? ",\n" : ""), $1
            printf "      \"platform\": \"%s\",\n", $2
            printf "      \"digest\": \"%s\",\n", $3
            printf "      \"bin\": \"%s\" }", $4
        }
        END { if (NR > 0) printf "\n" }
    ' "$NEW"
    printf '  ]\n'
    printf '}\n'
} > "$RENDERED"

# Read back what was just written with the same record parse the installer
# uses, and confirm the entry is findable. Cheap, and it is the only check that
# covers the emitter itself — a quoting slip here ships a file that parses as
# empty and takes the download page down.
back="$(records "$RENDERED" | awk -F'\t' -v v="$VERSION" -v p="$PLATFORM" '$1 == v && $2 == p { print $3 }')"
[ "$back" = "$DIGEST" ] \
    || die "internal error: the rendered manifest does not read back (got '$back', wanted '$DIGEST')"

count="$(wc -l < "$NEW" | tr -d ' ')"
info "$count entr(ies), latest = $LATEST"

if [ "$DRY" = 1 ]; then
    info "dry run — this is what would be written to $OUT:"
    cat "$RENDERED"
    exit 0
fi

if [ "$OUT" = "-" ]; then
    cat "$RENDERED"
else
    # Into place by rename, so a reader never sees a half-written manifest —
    # which matters if $OUT is on the box that serves it.
    cp "$RENDERED" "$OUT.incoming.$$"
    mv -f "$OUT.incoming.$$" "$OUT"
    info "wrote $OUT"
    step "upload it to the path install.sh reads: <site>/heyctl/versions.json"
fi
