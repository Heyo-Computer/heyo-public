# orchestrator

Heyo's control plane for sandboxes, services, and agent-driven workflows.

The orchestrator owns the source of truth for what should be running where. Other services hand it work — CICD asks it to spin up a sandbox to run a job, Cloud asks it to deploy a service — and the orchestrator plans, persists, and reconciles those requests against a backend (mvm-ctrl) that actually moves VMs. It also drives the agentic workflows used to compile parent jobs (discovery / planning / review / patch) against pluggable LLM providers.

## How it fits with CICD and HeyoSecret

```
                ┌────────────────────┐        ┌─────────────────────┐
   developer    │                    │        │                     │
   ──────────▶ │   cicd  (:4450)    │ ◀────▶ │   native runners    │
   git submit   │  - ingests submits │ lease  │  (Intel Mac, Win)   │
   trigger-build│  - plans CI jobs   │ +runs  │   cicd-runner-agent │
                │  - reports status  │        │                     │
                └─────────┬──────────┘        └─────────────────────┘
                          │ POST /orchestration/resources/deployments
                          │ GET  /orchestration/resources/archives/{id}
                          ▼
                ┌────────────────────┐
                │ orchestrator(:4446)│        ┌─────────────────────┐
                │  - plans & persists│ ──────▶│   mvm-ctrl backend  │
                │  - reconciles      │  POST  │  (libvirt / FC /    │
                │  - agent workflows │  /run  │   apple_container)  │
                └──┬───────┬─────────┘        └─────────────────────┘
                   │       │
       envRefs:    │       │ blue/green cutovers,
       resolve     │       │ deploy state, cloud
       secrets     │       │ internal callbacks
                   ▼       ▼
       ┌───────────────────┐   ┌─────────────────────┐
       │ heyosecret(:port) │   │   cloud (internal)  │
       │  - encrypted KV   │   │  - users / billing  │
       │  - audit history  │   │  - service routes   │
       └────────┬──────────┘   └──────────┬──────────┘
                │                         │
                └───────────┬─────────────┘
                            ▼
                  ┌─────────────────────┐
                  │ platform Postgres   │
                  │ shared database with│
                  │ service-owned tables│
                  └─────────────────────┘
```

The services are independent processes but can share one PostgreSQL database. Each service owns its tables and migrations; separate database URLs remain supported for standalone installations.

- **CICD** is the entry point for source — it receives signed `git submit` payloads, plans `.heyo/ci.yml` jobs, and either leases them to native runners or asks the orchestrator to spin up a sandbox. It calls the orchestrator over HTTP for `resources/archives/*` (upload/download workspace tarballs) and `resources/deployments/*` (launch + exec + stop the sandbox running the job).
- **Orchestrator** never talks to the backend hypervisor directly during a request — it persists the desired state, then a reconciler loop drives `mvm-ctrl` to converge. When deploying a Heyo-managed *service* whose manifest references secrets (`envRefs`), it calls **HeyoSecret** to materialize them just-in-time using the `heyosecret-client` crate.
- **HeyoSecret** is a small KV with versioning, audit history, and AES-GCM encryption at rest. Only the orchestrator (and other internal services) holds the `HEYOSECRET_INTERNAL_API_KEY`; tenant code never sees it.

Service rollouts keep the previous healthy deployment active while the candidate converges. The controller retries Cloud state and health reads with capped backoff under one deployment deadline, requires candidate and app-lb route health to remain successful for 10 seconds, and uses the candidate's public endpoint for stable route cutover even when an internal endpoint answers health first. Only persisted terminal state or the deadline is failure; deployment events are diagnostics, not a liveness signal.

## Public service deployment boundary

The public VM workflow deploys only Orchestrator, HeyoSecret and app-obs.
app-lb runs on the host and is not a VM deployment target, including for
`service=all`. app-obs still connects to the existing app-lb admin endpoint.
Automatic selection requires a change under the service's source paths or its
`.heyo/services/<service>.json` declaration;
shared workflow/environment edits and empty change lists select no services.
Use an explicit service dispatch when only shared deployment settings change.

The receiver-only prerequisite uses the flat request and selects Orchestrator
alone. This JSON caller workflow must wait until that receiver upgrades. Before any
receiver rollout, ensure host app-lb discovery does not depend on the retiring
Orchestrator VM's port. Verify discovery through a stable endpoint and preserve
the existing host proxy; do not recreate app-lb to upgrade the receiver.

## Layout

- `src/main.rs` — boot, route table, reconciler spawn.
- `src/config.rs` — `ORCHESTRATOR_*` env loading, per-phase agent overrides, backend capability defaults.
- `src/handlers/` — `orchestration.rs` (threads, templates, archives, resource deployments, approvals), `service_deploy.rs` (Heyo-service deployment and placement), `service_discovery.rs` (durable endpoint membership and drain intent), `internal.rs` (deploy lifecycle callbacks from the backend).
- `src/orchestration/` — `runtime.rs` (step execution), `reconciler.rs` (background loop that converges desired vs. observed state), `adapters.rs` (backend / cloud / heyosecret glue).
- `src/agent.rs` — agent phase orchestration and per-phase provider routing.
- `src/llm.rs` — public multi-provider LLM and tool-execution adapter for Anthropic, OpenAI, Mistral, and Gemini.
- `src/entities/`, `src/repositories/`, `src/db/` — SeaORM entities and queries against the orchestrator Postgres.
- `migrations/` — SQL migrations applied at startup.

## Getting started

### 1. Postgres

The orchestrator needs access to PostgreSQL. It can use a shared platform database or a dedicated database; the local example defaults to `postgresql://postgres:password@127.0.0.1:5432/orchestrator_db`. Migrations under `migrations/` run automatically at boot via `db::init_database`.

### 2. Configure

```
cp .env.example .env
```

Fill in at least:

- `DATABASE_URL` — orchestrator Postgres.
- `JWT_SECRET` — must match the value CICD and Cloud use to sign internal calls.
- `CLOUD_INTERNAL_API_KEY` + `ORCHESTRATOR_CLOUD_INTERNAL_URL` — for the orchestrator → cloud callbacks.
- `ORCHESTRATOR_AGENT_API_KEY` (or the provider-specific `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `MISTRAL_API_KEY`) — for the agentic workflow phases.

Set `ORCHESTRATOR_PROXY_BASE_DOMAINS` to a comma-separated list of wildcard proxy base domains when backend deployment URLs must be probed through `ORCHESTRATOR_BACKEND_API_URL` instead of public DNS.

Rolling replicas are an explicit discovery-routed traffic mode. Configure `ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES` with a comma-separated allowlist. A replicated request must include the service's stable `route`; Orchestrator verifies that route through the active app-lb backend, rewrites ingress to that backend, and only then drains a previous replica. Asynchronous retirement persists drain intent but does not stop an old replica until the parent rollout has recorded success, so Orchestrator can safely roll itself. `replicaRegions` may assign each desired replica to a region and must contain exactly `desiredReplicas` entries. `placementPool` selects a Cloud-managed host pool without naming physical servers; Cloud additionally scopes that pool to its own configured environment. app-lb itself must never be in the discovery-routing allowlist.

The public-service workflow accepts `HEYO_SERVICE_REPLICA_REGIONS` as a comma-separated list such as `EU,US`, which sets both `replicaRegions` and `desiredReplicas` for every allowlisted service. `HEYO_SERVICE_PLACEMENT_POOL` is optional; use `platform` after the intended shared hosts have registered in that pool. Manual dispatches may provide `discoveryRoutedServices`, `replicaRegions`, `placementPool`, and `serviceDriver` without changing the CICD service environment; omitted inputs retain the configured environment values and existing single-region behavior. `HEYO_SERVICE_REPLICAS` remains available as either one count or a per-service map such as `heyosecret=2,orchestrator=2`; when both settings are present, their counts must agree. The existing host app-lb must already contain the discovery-backed route definitions; this workflow does not install or replace it.

For a first-time activation without a config bootstrap race: first deploy compatible Cloud and heyvm versions, then verify that Cloud records one EU and one US host in the target environment and pool `platform`, with distinct node IDs. Deploy the compatible receiver before activating its discovery allowlist. Then set `ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES=app-obs,heyosecret,orchestrator`, `HEYO_SERVICE_REPLICA_REGIONS=EU,US`, and `HEYO_SERVICE_PLACEMENT_POOL=platform`; workflow-dispatch `orchestrator` once with `bootstrapDiscoveryRouting=true`, then workflow-dispatch `all`. The bootstrap request leaves Orchestrator on its existing singleton route while loading the allowlist into the active process. The full VM rollout then runs app-obs → HeyoSecret → Orchestrator, using the existing host app-lb ingress. Do not repeat this bootstrap for an environment with discovery already active. Phase 1 keeps one app-lb ingress while placing one replica of every discovery-routed service in EU and one in US.

If you plan to deploy services with `envRefs`, also set:

- `ORCHESTRATOR_HEYOSECRET_URL` (or `HEYOSECRET_URL`)
- `ORCHESTRATOR_HEYOSECRET_INTERNAL_API_KEY` (or `HEYOSECRET_INTERNAL_API_KEY`)

If the backend (mvm-ctrl) runs on a different host or OS than the orchestrator, set `ORCHESTRATOR_BACKEND_API_URL` so capabilities (`targetOs`, supported drivers) come from `GET /capabilities` instead of the orchestrator's local defaults.

### 3. Run

```
cargo run --locked --manifest-path orchestrator/Cargo.toml --bin orchestrator
```

Listens on `ORCHESTRATOR_SERVER_PORT` (default `4446`). Health check: `GET /health`.

Container build:

```
docker build -f orchestrator/Dockerfile -t heyo-orchestrator .
```

### 4. Wire CICD to it

In CICD's environment, point `CICD_ORCHESTRATOR_URL` at this service (e.g. `http://127.0.0.1:4446`). CICD will then POST workspace archives and resource deployments into `/orchestration/*` whenever a submit needs a cloud sandbox.

## Key routes

- `POST /orchestration/threads` — start an agent workflow thread.
- `POST /orchestration/parent-jobs/compile` — compile a parent job spec.
- `POST /orchestration/resources/archives` (and `/presign`, `/finalize`) — upload workspace tarballs CICD will run jobs against.
- `GET  /orchestration/resources/archives/{archive_id}` — stream an archive back (used by CICD to fetch debug artifacts).
- `POST /orchestration/resources/deployments` — request a sandbox; reconciler converges it.
- `POST /orchestration/resources/deployments/{id}/exec` — run a command inside.
- `POST /orchestration/services/archives/presign` (and `/finalize`) — authenticated direct upload for large Heyo-managed service archives; pass the finalized `archiveId` to the service deployment request.
- `POST /orchestration/services/deployments` — deploy a Heyo-managed service using the snake_case app-lb-style service format described below. **Breaking change:** old flat camelCase requests are rejected, not converted or accepted through aliases.
- `GET  /orchestration/services/{service_id}/discovery` — authenticated, versioned endpoint membership for app-lb, including each endpoint's region when known. Rolling deploys publish and health-gate one candidate, drain one old replica, and repeat. A failed candidate leaves the remaining healthy set serving. `retirePrevious=false` only adds capacity up to `desiredReplicas`.
- `POST /internal/deployments/lifecycle` — callback from the backend reporting deploy state transitions.
- `POST /orchestration/approvals/{approval_id}/decide` — gate an in-flight workflow.

### Service deployment files

Service configuration lives in [`.heyo/services`](../.heyo/services). The workflow
loads a file, fills in the build artifact, target host/region and revision, then
submits it. Install the receiver-only upgrade before activating this workflow;
see the breaking-change rollout below.
Application environment variables remain application settings; there is no change
to Orchestrator's own process-config loader.

The request has `{ id, user_id, account_id?, vm, routes?, health?, scaling?, deploy }`.
It uses app-lb's field names with an Orchestrator-only `deploy` operation section.
It is a supported subset, not a promise that every app-lb lifecycle feature works
through Orchestrator. Unknown fields and unsupported features fail explicitly.

| Section | Supported behavior |
| --- | --- |
| `vm` | Required `driver`, `image`, primary `port`; optional `open_ports`, `start_command`, `working_directory`, `setup_hooks`, `size_class` (default `small`), `ttl_seconds`, `env_vars`, `env_from` |
| `vm.env_from` | `{ "secret": "orchestrator", "key": "database-url", "as": "DATABASE_URL" }` resolves active HeyoSecret path `orchestrator/database-url`. Default key is `token`, default environment name is uppercased key. Secrets override matching `env_vars` literals, as in app-lb. Explicit namespaces and duplicate secret target names are rejected. No secret values in the file. |
| `routes` | Zero or one exact `host` with optional `path_prefix` and `strip_prefix` (default false). Existing Traefik renderer cannot represent wildcard/hostless/multiple routes, so these are rejected. |
| `health` | HTTP `path` (default `/`), same port as the VM, positive `timeout_secs` (default 2) for each candidate probe. Templates specify 5 to preserve existing deployments. TCP and a different health port are unsupported. |
| `scaling` | Fixed `min_replicas == max_replicas`, 1–16. Omit for direct single-candidate execution. Dynamic autoscaling options are rejected, not silently ignored. |
| `deploy` | Operation metadata: `deployment_id`, `name`, `async`, `archive_id` / `archive_bytes_base64`, `archive_name`, `region` (default `local`), `placement_pool`, `replica_regions`, overall `health_timeout_seconds` (default 180), retirement flags, `drain_seconds` (default 10), `metadata`, `revision_guard` |
| `deploy.ingress` | Traefik-specific `entry_points`, `cert_resolver`, `priority`, `pass_host_header`, `backend_url`; requires a route |
| `deploy.host_mounts` | Existing host bindings `{ host_path, sandbox_path, read_only }`, with absolute, traversal-free paths. CICD's persistent run directory uses this. These are not app-lb's artifact-backed `vm.mounts`, which remain unsupported. |

`deploy.revision_guard` uses `repository_url`, `ref`, `expected_sha`, and optional
`force`. `deploy.retire_previous` defaults to true; `retire_previous_async` and
`delete_previous` default to false. Secrets continue to resolve through HeyoSecret;
Cloud's internal VM allocation protocol and deployment response/status formats are
unchanged. `health.timeout_secs` is not the overall rollout deadline and does not
change the separate post-cutover route health check.

### Breaking-change rollout

The deployment workflow loads host-keyed defaults from
`.heyo/deployment-environments.json` before constructing requests. Staging's
verified existing policy is `heyosecret,orchestrator`, one replica each, with no
explicit replica-region list or placement pool. Nonempty dispatch/CI settings
override these defaults. A pool is optional; when absent, Cloud uses its existing
regional allocation policy. The same discovery-service list is passed into the
replacement Orchestrator, so the upgrade does not silently disable discovery.
Other hosts receive no staging defaults. No server IDs are selected by this file.

The PR #55 receiver deployment failed before cutover: the installed CICD runner
overwrote `GITHUB_ENV` defaults with empty job environment values. Load the
host-keyed defaults inside the deployment Python process, before constructing
the request, so recovery does not require a CICD upgrade first. The prerequisite
recovery PR #56 selects only Orchestrator and uses the flat request; this JSON
caller branch must wait until that receiver upgrade succeeds.
Shared workflow edits alone select no services, and app-lb remains host-managed.
Run `python3 .heyo/test_deployment_environment.py` for offline regression checks.

Both public and private service-deployment workflows must move with this interface.
The public workflow covers HeyoSecret, Orchestrator and app-obs; the private
companion covers Cloud, CICD and Retail. No dual-format server is provided.

1. Submit the receiver-only change first. Its unchanged workflow sends the old
   request to the running old receiver, which deploys the new Orchestrator binary.
   Only `orchestrator/` changes, so the workflow selects only Orchestrator. The
   resource allocation API used by CICD and deployment status responses are unchanged.
2. Wait for that deployment to finish and the public Orchestrator health endpoint
   to report the new revision. Do not run unrelated service deployments during
   this transition: old callers cannot deploy to the new receiver.
3. Submit the public caller migration, then the private caller migration. Both
   require the new receiver. Do not resubmit the receiver-only revision after
   cutover; further Orchestrator deployments must use the migrated caller.

This is a coordinated breaking cutover, not a dual-format compatibility period.
If the first deployment fails before cutover, diagnose it while the old receiver
still serves; do not advance the callers. After cutover, use the new callers.
No deployment or infrastructure change is performed by preparing these PRs.

Receiver validation: `cargo test --locked --manifest-path orchestrator/Cargo.toml`.
Offline validation: `python3 .heyo/services/test_service_specs.py` executes the
workflow with mocked network/build calls. `SERVICE_SPEC_BASELINE_REF` optionally
compares against an old-workflow Git revision. `SERVICE_SPEC_FIXTURE_DIR` exports
synthetic payloads; use the same directory for the private caller tests and the
Rust contract test to validate all six VM service requests.
