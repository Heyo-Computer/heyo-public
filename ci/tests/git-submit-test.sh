#!/usr/bin/env bash
set -euo pipefail

submit="$(cd "$(dirname "$0")/.." && pwd)/bin/git-submit"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/git-submit-test.XXXXXX")"
cleanup() { [ -z "${server_pid:-}" ] || kill "$server_pid" 2>/dev/null || true; rm -rf "$tmp"; }
trap cleanup EXIT

git init -q --bare "$tmp/origin.git"
git clone -q "$tmp/origin.git" "$tmp/client"
git -C "$tmp/client" config user.name Test
git -C "$tmp/client" config user.email test@example.com
mkdir -p "$tmp/client/.ci/workflows"
printf 'name: build\non: [submit]\njobs: {}\n' > "$tmp/client/.ci/workflows/build.yml"
printf 'initial\n' > "$tmp/client/text"
git -C "$tmp/client" add .
git -C "$tmp/client" commit -qm initial
git -C "$tmp/client" push -q origin HEAD:main
published="$(git -C "$tmp/client" rev-parse HEAD)"

cat > "$tmp/receiver.py" <<'PY'
import http.server, json, os
class H(http.server.BaseHTTPRequestHandler):
  def do_POST(self):
    body=self.rfile.read(int(self.headers['content-length']))
    open(os.environ['CAPTURE'],'wb').write(body)
    out=b'{"runs":["test"],"url":"http://example/"}'
    self.send_response(200); self.send_header('content-length',str(len(out))); self.end_headers(); self.wfile.write(out)
  def log_message(self,*args): pass
s=http.server.HTTPServer(('127.0.0.1',0),H)
open(os.environ['PORT'],'w').write(str(s.server_port)); s.serve_forever()
PY
CAPTURE="$tmp/payload.json" PORT="$tmp/port" python3 "$tmp/receiver.py" & server_pid=$!
while [ ! -s "$tmp/port" ]; do sleep .05; done
endpoint="http://127.0.0.1:$(cat "$tmp/port")"
run_submit() { (cd "$tmp/client" && CI_ENDPOINT="$endpoint" CI_TOKEN=test "$submit" "$@"); }

# A published checkout is pinned to that SHA and carries no source repository.
run_submit --submit-empty --only build >/dev/null
PUBLISHED="$published" python3 - "$tmp/payload.json" <<'PY'
import base64,json,os,sys
p=json.load(open(sys.argv[1])); d=json.loads(base64.b64decode(p['source']['contentBase64']))
assert p['source']['format']=='git-patch' and d['baseRevision']==os.environ['PUBLISHED']
assert base64.b64decode(d['patchBase64'])==b'' and p['only']==['build']
assert list(d['workflows'])==['.ci/workflows/build.yml']
assert 'initial\n' not in json.dumps(p) # ordinary source content was not archived
PY

# Advance the remote from another clone, then submit divergent local work. The
# selected common base is determined from current remote state, not origin/main.
git clone -q "$tmp/origin.git" "$tmp/other"
git -C "$tmp/other" config user.name Other; git -C "$tmp/other" config user.email other@example.com
git -C "$tmp/other" checkout -q main
printf 'remote\n' > "$tmp/other/remote"; git -C "$tmp/other" add remote; git -C "$tmp/other" commit -qm remote
git -C "$tmp/other" push -q origin main
printf '\x00\x01binary\xff' > "$tmp/client/blob"
printf '#!/bin/sh\necho yes\n' > "$tmp/client/tool"; chmod +x "$tmp/client/tool"
rm "$tmp/client/text"; git -C "$tmp/client" add -A; git -C "$tmp/client" commit -qm local
target_tree="$(git -C "$tmp/client" rev-parse HEAD^{tree})"
run_submit >/dev/null
ORIGIN="$tmp/origin.git" TREE="$target_tree" python3 - "$tmp/payload.json" "$tmp/check" <<'PY'
import base64,json,os,subprocess,sys
p=json.load(open(sys.argv[1])); d=json.loads(base64.b64decode(p['source']['contentBase64']))
patch=base64.b64decode(d['patchBase64']); assert b'GIT binary patch' in patch and b'deleted file mode' in patch and b'new file mode 100755' in patch
subprocess.run(['git','clone','-q',os.environ['ORIGIN'],sys.argv[2]],check=True)
subprocess.run(['git','-C',sys.argv[2],'checkout','-q',d['baseRevision']],check=True)
subprocess.run(['git','-C',sys.argv[2],'apply','--index'],input=patch,check=True)
tree=subprocess.check_output(['git','-C',sys.argv[2],'write-tree'],text=True).strip()
assert tree==d['targetTree']==os.environ['TREE']
PY

# Dirty tracked content travels, but neither index nor HEAD changes.
head_before="$(git -C "$tmp/client" rev-parse HEAD)"; index_before="$(git -C "$tmp/client" write-tree)"
printf 'dirty\n' >> "$tmp/client/tool"
run_submit --dirty >/dev/null
test "$head_before" = "$(git -C "$tmp/client" rev-parse HEAD)"
test "$index_before" = "$(git -C "$tmp/client" write-tree)"

if run_submit --archive >"$tmp/archive.out" 2>&1; then echo '--archive unexpectedly succeeded' >&2; exit 1; fi
grep -q 'full-repository bundles or archives' "$tmp/archive.out"

# A history with no remotely reachable ancestor is rejected instead of packed.
git -C "$tmp/client" checkout -q --orphan unpublished
git -C "$tmp/client" rm -qrf .
printf 'root\n' > "$tmp/client/root"; git -C "$tmp/client" add root; git -C "$tmp/client" commit -qm root
if run_submit >"$tmp/root.out" 2>&1; then echo 'unpublished root unexpectedly succeeded' >&2; exit 1; fi
grep -q 'Push a base commit/branch' "$tmp/root.out"
printf 'git-submit tests passed\n'
