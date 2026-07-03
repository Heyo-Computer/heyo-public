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
