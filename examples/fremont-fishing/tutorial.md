# Fremont Fishing Tutorial

Rebuild the Fremont Fishing app from `examples/fremont-fishing/spec.md`.
Use the public skills as the working guide:

- `skills/heyvm/SKILL.md` for preview VM, workspace, and deployment commands.
- `skills/git-submit/SKILL.md` for submitting branches when a step is ready.

Each tutorial step is a Git branch. To continue, finish the current branch, push
it, then create the next branch from it.

## Step 1: Start From The Spec

Branch: `step1`

```sh
git switch step1
find examples/fremont-fishing -type f | sort
```

Expected output:

```text
examples/fremont-fishing/spec.md
examples/fremont-fishing/tutorial.md
```

Next step:

```sh
git switch -c step2
```

## Step 2: Log In To Preview

Branch: `step2`

```sh
git switch step1
git switch -c step2

export HEYO_PREVIEW_CLOUD=https://preview.heyo.computer/cloud
export HEYO_PREVIEW_AUTH=https://preview.heyo.computer/auth

heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  login --email bob.wilson@example.com
```

Verify:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  list --all
```

Next step:

```sh
git switch -c step3
```

## Step 3: Create The Preview VM

Branch: `step3`

```sh
git switch step2
git switch -c step3

export FREMONT_VM=fremont-fishing-app

heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  create --cloud \
  --name "$FREMONT_VM" \
  --backend libvirt \
  --region US \
  --image ubuntu:24.04 \
  --size-class small \
  --start-command 'sleep infinity' \
  --port 3000 \
  --format json
```

Expected result:

```text
id: <deployment-id>
name: fremont-fishing-app
status: running
url: https://<subdomain>.preview.heyo.computer
```

List the preview VM:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  list --all
```

Access the VM shell:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  sh "$FREMONT_VM"
```

The VM belongs to preview because it was created with `HEYO_PREVIEW_CLOUD` and
`HEYO_PREVIEW_AUTH` and appears in the preview `list --all` output. It will not
show the app until a later step starts a web server on port 3000.

Next step:

```sh
git switch -c step4
```

## Step 4: Enter The VM And Start Codex

Branch: `step4`

Upload the current tutorial workspace into the VM:

```sh
ARCHIVE_ID=$(heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  archive-dir examples/fremont-fishing \
  --name fremont-fishing-step4 \
  --format json | jq -r '.id')

heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  update "$FREMONT_VM" --archive "$ARCHIVE_ID"
```

Install the tools needed for Codex work inside the preview VM:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  exec "$FREMONT_VM" -- bash -lc '
    set -euo pipefail
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends ca-certificates curl git jq
    if ! command -v node >/dev/null 2>&1; then
      curl -fsSL https://deb.nodesource.com/setup_22.x | sudo bash -
      sudo apt-get install -y nodejs
    fi
    if ! command -v codex >/dev/null 2>&1; then
      sudo npm install -g @openai/codex
    fi
    node --version
    npm --version
    codex --version
  '
```

Verified versions:

```text
node v22.23.1
npm 10.9.8
codex-cli 0.142.5
```

Open a shell in the VM:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  sh "$FREMONT_VM"
```

Inside the VM:

```sh
cd /workspace
codex
```

Use this first prompt:

```text
Read /workspace/spec.md and this tutorial. Do not edit files yet. Summarize the app requirements and the next implementation steps.
```

Next step:

```sh
git switch -c step5
```

## Step 5: Build The App Inside The VM

Branch: `step5`

Inside the VM:

```sh
cd /workspace
codex
```

Use this prompt:

```text
Read spec.md and tutorial.md. Build the Fremont fishing app in /workspace.
Use Vite + React + TypeScript. Keep npm start serving the production build on
0.0.0.0:3000. Verify with npm install, npm run build, npm start, curl -I
http://localhost:3000/, and npm audit --audit-level=moderate.
```

Expected result:

```text
npm install passes
npm run build passes
npm start serves on 0.0.0.0:3000
curl -I http://localhost:3000/ returns HTTP/1.1 200 OK
npm audit --audit-level=moderate reports 0 vulnerabilities
```

Copy the VM workspace back to this branch, excluding generated directories:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  exec "$FREMONT_VM" -- bash -lc \
  'cd /workspace && tar --exclude=node_modules --exclude=dist --exclude=.git -czf - . | base64 -w 0' \
  > /tmp/fremont-fishing-step5.b64

tail -n +2 /tmp/fremont-fishing-step5.b64 > /tmp/fremont-fishing-step5.clean.b64
base64 -D < /tmp/fremont-fishing-step5.clean.b64 > /tmp/fremont-fishing-step5.tar.gz
tar -xzf /tmp/fremont-fishing-step5.tar.gz -C examples/fremont-fishing
```

Next step:

```sh
git switch -c step6
```

## Step 6: Run And Verify The Preview App

Branch: `step6`

Use the same preview VM from Step 3:

```sh
git switch step5
git switch -c step6

export HEYO_PREVIEW_CLOUD=https://preview.heyo.computer/cloud
export HEYO_PREVIEW_AUTH=https://preview.heyo.computer/auth
export FREMONT_VM=fremont-fishing-app
```

Install dependencies and build inside the VM:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  exec "$FREMONT_VM" -- bash -lc \
  'cd /workspace && npm ci && npm run build && npm audit --audit-level=moderate'
```

Make the VM start the production server on port 3000:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  edit-start-command "$FREMONT_VM" \
  --working-directory /workspace \
  --start-command 'npm start'
```

Wait for the app to answer:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  wait-for "$FREMONT_VM" 3000 --path / --timeout 120s
```

Find and verify the preview URL:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  list --all

export FREMONT_URL=https://<subdomain>.preview.heyo.computer
curl -I "$FREMONT_URL"
```

Expected result:

```text
HTTP/2 200
```
