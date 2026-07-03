---
name: git-submit
description: Use and troubleshoot Heyo git submit CI/CD: install or upgrade the git-submit client, configure endpoints, submit branches, handle submodules, inspect CI runs, and clean up retained VM-backed jobs.
---

# git-submit

Use this skill for Heyo's `git submit` workflow and its CI/CD runs.

## Source Of Truth

Use the installed client help first:

```sh
git submit -h
git submit --upgrade -h
git submit --version
```

On some systems, `git submit --help` is intercepted by Git's manpage lookup. Use
`-h` when you need the script's actual help text.

## Install And Upgrade

Current clients support:

```sh
git submit --upgrade
git submit --upgrade --remote
git submit --upgrade --local
```

Use `--local` from the private monorepo when the public release channel is not
available or the current checkout contains the desired client fix:

```sh
bash cicd/install-git-submit.sh
git submit --upgrade --local
git submit --version
```

If remote upgrade fails with HTTP 403 or download errors, do not keep retrying.
Report the release URL failure and use `--local` until the public route/bucket
policy is fixed.

## Configure

Check the endpoint before submitting:

```sh
git config --get cicd.endpoint
git config cicd.endpoint https://stage.heyo.computer/cicd/git/push
```

Authentication normally comes from `heyvm login` and `~/.heyo/token.json`. Never
paste token contents into chat or logs. If the server returns 401, ask the user
to run `heyvm login` for the same environment.

## Submit Flow

The normal workflow is:

```sh
git fetch origin main
git status --short --branch
git submit
```

`git submit` submits the current branch against the latest trunk tip
(`origin/main` by default). It creates a patch from the merge base, validates it
in CI, merges the matching PR when eligible, and then starts the post-merge
deploy run.

Use dry-run only for local workflow testing with dirty changes:

```sh
git submit --dry-run
```

Dry-run submits are not eligible for automatic merge.

## Submodules

Submodule changes require `git-submit 0.2.8` or newer. Confirm before
submitting:

```sh
git submit --version
```

Work in this order:

1. Commit and push the submodule repository change first.
2. In the parent repository, stage the updated submodule gitlink.
3. Commit the parent repository change.
4. Run `git submit` from the parent repository.

The parent patch should contain a `160000` gitlink update, not copied submodule
contents. If the server rejects or drops a submodule update, upgrade the client
and retry from a clean branch.

## CI Run Inspection

Keep the run ID from `git submit`, for example `ci-...` or
`ci-...-post-merge-deploy`.

For API inspection, derive the base URL from `cicd.endpoint` and use the Heyo
token without printing it:

```sh
endpoint=$(git config --get cicd.endpoint)
base=${endpoint%/git/push}
token=$(jq -r '.access_token // .token // empty' ~/.heyo/token.json)
curl -fsS -H "Authorization: Bearer $token" "$base/runs/<run-id>" | jq .
```

If a run retained VMs for debugging, clean it up after diagnosis:

```sh
curl -fsS -X POST -H "Authorization: Bearer $token" "$base/runs/<run-id>/cleanup" | jq .
```

If cleanup reports a missing sandbox that was already removed, treat that as
non-blocking. If jobs are queued because no backend is active, inspect backend
health and retained sandboxes before submitting more runs.

## Failure Handling

- Version too old: run `git submit --upgrade --local` from the monorepo or use
  `git submit --upgrade` when the public channel works.
- 401/403 from CI: re-authenticate with `heyvm login` for the configured
  endpoint environment.
- Backend unavailable: do not spam new runs. Clean retained VMs, verify the
  single backend is active, then retry.
- PR closed or merged: check the run and post-merge deploy status; a closed PR
  does not by itself prove production/stage deploy succeeded.
- Submodule missing or empty after merge: verify the submodule commit exists on
  its remote and the parent repository points at that commit.
