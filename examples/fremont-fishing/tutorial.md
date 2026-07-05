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

heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  create --cloud \
  --name fremont-fishing-app \
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

Verified in this run:

```text
id: dep-0a30245b
name: fremont-fishing-app
status: running
url: https://vxze11.preview.heyo.computer
```

Access the VM shell:

```sh
heyvm --cloud-url "$HEYO_PREVIEW_CLOUD" \
  --auth-url "$HEYO_PREVIEW_AUTH" \
  sh dep-0a30245b
```

The VM belongs to preview because it was created with `HEYO_PREVIEW_CLOUD` and
`HEYO_PREVIEW_AUTH` and appears in the preview `list --all` output. It will not
show the app until a later step starts a web server on port 3000.

Known issue observed in this run: `create --format json` printed
`https://vxze11.heyo.computer`, while `list --all` returned the correct preview
URL:

```text
https://vxze11.preview.heyo.computer
```

Next step:

```sh
git switch -c step4
```
