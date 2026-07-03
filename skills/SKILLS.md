# Skills

## Skill: Customer Heyo CI Workflow Authoring

Use this skill when helping a customer add Heyo CI to an existing repository,
especially when `git submit` is accepted but the run is skipped with:

```text
No Heyo/GitHub-compatible workflows matched this push event.
```

The customer should not need Heyo source code to use this skill. Treat Heyo as a
hosted product reached through the stage UI, documented APIs, `heyvm`, and
`git submit`.

### Goal

Create a repo-local workflow under:

```text
.heyo/workflows/<repo-or-app-name>.yml
```

The first workflow should prove CI works. If the customer asks for deployment,
add repo-local deploy metadata and a Heyo-owned deploy action. Do not ask the
customer to paste an orchestrator bearer token into workflow YAML.

### Customer Setup

The customer configures the repository once:

```sh
git config cicd.endpoint https://stage.heyo.computer/cicd/git/push
```

The customer verifies the installed submit client:

```sh
git submit --version
```

If the client is stale, update it from the published Heyo release channel. Until
that channel exists, this is a product gap; do not make customers clone the Heyo
source repo as the normal install/update path.

Authentication should come from the customer's Heyo login or API key. Passing a
raw token through `HEYO_AUTH_TOKEN` is acceptable for debugging, but it is not
the desired customer path.

### Inputs To Inspect In The Customer Repo

Before writing the workflow, inspect the repository's own files:

- top-level `README.md`
- service-level `README.md` files
- `package.json`, `bun.lock`, `pnpm-lock.yaml`, `package-lock.json`, or
  `yarn.lock`
- `Cargo.toml` and `Cargo.lock`
- `go.mod`, `pyproject.toml`, `requirements.txt`, `Dockerfile`, or compose files
- existing `.github/workflows/*.yml`
- existing deploy scripts or manifests

Prefer the customer's existing build and test commands. Do not invent a full
release/deployment pipeline before the basic validation workflow runs.

### Workflow Rules

- Put Heyo-specific workflows in `.heyo/workflows/*.yml`.
- Include `on.push.branches` for the branch that `git submit` targets, usually
  `main`.
- Include `workflow_dispatch` when manual UI/API runs are useful.
- Use `actions/checkout` for repository materialization.
- Use known setup actions only when Heyo CI supports them; otherwise install
  tools with shell commands.
- Keep secrets out of committed workflow files.
- Split unrelated runtimes into separate jobs so failures are easy to diagnose.
- Use deployment jobs only after CI passes.
- Deployment must go through a Heyo-owned action such as
  `heyo/orchestrator-deploy@v1`; do not hand-roll orchestrator `curl`/Python
  calls in customer workflows.

### Internal Stack Example

For Bob's `internal-stack` repository, create:

```text
.heyo/workflows/internal-stack.yml
```

Suggested first workflow:

```yaml
name: Internal Stack CI

on:
  push:
    branches:
      - main
  workflow_dispatch:

jobs:
  bun-services:
    runs-on: namespace-profile-default
    steps:
      - uses: actions/checkout@v4

      - uses: oven-sh/setup-bun@v1
        with:
          bun-version: latest

      - name: Validate Bun services
        run: |
          set -euo pipefail

          for service in auth chat crm simpleoffice; do
            echo "Validating $service"
            cd "$GITHUB_WORKSPACE/$service"
            bun install --frozen-lockfile
            bunx tsc --noEmit
            if bun run | grep -q '^  build'; then
              bun run build
            fi
          done

  rust-services:
    runs-on: namespace-profile-default
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - name: Validate Rust services
        run: |
          set -euo pipefail

          for service in fastkv files webcron; do
            echo "Validating $service"
            cargo test --manifest-path "$service/Cargo.toml"
          done
```

After committing the workflow, submit again:

```sh
git submit
```

The expected response includes `accepted: true`, `runId`, and `statusPath`.
Open the status path under the CICD service and verify the run has at least one
job. If the run still has zero jobs, check workflow location, YAML syntax, and
branch matching.

### Deployment Authoring

Use this section when the customer asks Codex to make the repository deploy on
Heyo after `git submit`.

Create or update:

```text
.heyo/deploy.yml
.heyo/deploy-plans/<target>.json
.heyo/workflows/<repo-or-app-name>.yml
```

The deploy plan belongs to the customer repository. It should describe how to
run that repo on Heyo: app name, repository URL, target, runtime driver/image,
public ports, setup/start commands, health checks, env defaults, and required
secrets. Prefer existing repo scripts over inventing new deployment logic.

The workflow deploy job should depend on validation jobs and submit the saved
plan through Heyo's platform-owned action:

```yaml
  deploy-stage:
    needs:
      - bun-services
      - rust-services
    runs-on: namespace-profile-default
    steps:
      - uses: actions/checkout@v4

      - name: Deploy from saved Heyo plan
        uses: heyo/orchestrator-deploy@v1
        with:
          plan: .heyo/deploy-plans/stage.json
          app-name: my-app
          repository-url: https://github.com/example/my-app.git
          target: heyo-sandbox
```

The action is trusted Heyo runner code. It reads the repo plan, calls
orchestrator with platform auth, and returns `thread-id` and `parent-job-id`.
Do not require customers to know or store `HEYO_ORCHESTRATOR_BEARER_TOKEN`.

Generic action inputs:

- `plan`: repo-relative JSON plan path, default `.heyo/deploy-plans/stage.json`
- `app-name`: public deployment/app name
- `repository-url`: clone URL for the repo
- `template-id`: default `app.deploy_from_plan`
- `target`: default `heyo-sandbox`
- `region`, `driver`, `image`, `size-class`
- `start-command`, `working-directory`, `healthcheck-path`, `expected-status`
- `ttl-seconds`
- `env`: JSON object of deployment env vars
- `env.KEY`: single deployment env var override
- `generate-missing-secrets`: default `true`

For required runtime secrets, prefer repo plan declarations such as:

```json
{
  "environment": {
    "requiredSecrets": [
      { "name": "JWT_SECRET" },
      { "name": "SERVICE_TOKEN" }
    ]
  }
}
```

The built-in action may generate missing deployment-only secrets for sandbox
deploys. Customer-provided production secrets should come from a documented
Heyo secret store or workflow secret surface, not committed files.

### Internal Stack Deployment Example

For Gary/Bob's `internal-stack`, keep app code changes minimal. Use the repo's
existing internal-user deployment scripts:

```text
deploy/internal-user-setup.sh
deploy/internal-user-start.sh
```

Create a stage plan such as:

```text
.heyo/deploy-plans/internal-stack.stage.json
```

The plan should set:

- app/deployment name: `internal-stack-bob`
- repository: `https://github.com/Heyo-Computer/internal-stack.git`
- driver/image/size from the target stage environment
- setup command: `bash /workspace/deploy/internal-user-setup.sh`
- start command: `bash /workspace/deploy/internal-user-start.sh`
- public ports for auth, chat, simpleoffice, and crm
- private ports for fastkv, files, and webcron
- env defaults needed by the scripts
- required secrets such as `JWT_SECRET` and `SERVICE_TOKEN`

Then add a deploy job to `.heyo/workflows/internal-stack.yml`:

```yaml
  deploy-stage:
    needs:
      - bun-services
      - rust-services
    runs-on: namespace-profile-default
    steps:
      - uses: actions/checkout@v4

      - name: Deploy internal-stack to Heyo stage
        uses: heyo/orchestrator-deploy@v1
        with:
          plan: .heyo/deploy-plans/internal-stack.stage.json
          app-name: internal-stack-bob
          repository-url: https://github.com/Heyo-Computer/internal-stack.git
          target: heyo-sandbox
```

After `git submit`, inspect the CI run. The deploy step should show an
orchestrator `thread-id` and `parent-job-id`; follow that job in the Heyo stage
orchestrator UI for approval, deployment status, public URLs, and health checks.

If deployment fails because the action is unsupported, missing in stage, or
cannot resolve the submitter account, treat that as a Heyo product issue. Do not
replace the action with raw bearer-token calls in the customer repo.

### Product Gaps To Close

If a customer cannot complete this skill without internal Heyo knowledge, add or
fix the product surface instead of documenting a private workaround:

- Published `git-submit` install/update channel.
- `git submit` auth through `heyvm login` or a documented Heyo API key.
- Public workflow schema and supported action list.
- Clear runner label documentation.
- Stage UI page with exact repo setup commands.
- Clear entitlement and billing status before deployment.
- Public deploy manifest schema.
- Built-in `heyo/orchestrator-deploy@v1` support in hosted Heyo CICD.
- Submitter identity propagation from `git submit` into trusted deploy actions,
  so deployment can run under the customer's Heyo account without exposing that
  account token to arbitrary workflow steps.
