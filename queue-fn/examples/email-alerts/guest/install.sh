#!/bin/sh
# Bake the fan-out script into a Firecracker image.
#
# Run this *inside* the image you are building, with fanout.py alongside it.
# queue-fn deliberately has no `setup_hooks`: a function that installs itself at
# cold start turns every scale-from-zero into a network install, which is the
# slowest possible moment to discover that a package mirror is down.
#
#   ./install.sh            # installs from ./fanout.py
#   ./install.sh /path/to/fanout.py
set -eu

SRC="${1:-$(dirname "$0")/fanout.py}"
DEST_DIR=/opt/email-alerts
DEST="$DEST_DIR/fanout.py"

if [ ! -f "$SRC" ]; then
    echo "install.sh: no such file: $SRC" >&2
    exit 1
fi

# python3 only — the script is standard library throughout, so there is no pip
# step and nothing to go stale between image builds.
if ! command -v python3 >/dev/null 2>&1; then
    echo "install.sh: python3 is not in the image; add it before installing" >&2
    exit 1
fi

mkdir -p "$DEST_DIR"
cp "$SRC" "$DEST"
chmod 0755 "$DEST"

# Fail the build, not the first alert at 3am.
python3 -c "import ast,sys; ast.parse(open(sys.argv[1]).read())" "$DEST"
python3 -c "import smtplib, ssl, email.message, urllib.request, json, base64, hashlib"

echo "installed $DEST"
