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

Service rollouts keep the previous healthy deployment active while the candidate converges. The controller retries Cloud state and health reads with capped backoff under one deployment deadline, probes each distinct internal and public candidate endpoint, and uses the candidate's public endpoint for stable route cutover even when an internal endpoint answers health first. Only persisted terminal state or the deadline is failure; deployment events are diagnostics, not a liveness signal.

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

Rolling replicas are an explicit discovery-routed traffic mode. Configure `ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES` with a comma-separated allowlist. A replicated request must include the service's stable `route`; Orchestrator verifies that route through the active app-lb backend, rewrites ingress to that backend, and only then drains a previous replica. `replicaRegions` may assign each desired replica to a region and must contain exactly `desiredReplicas` entries. `placementPool` selects a Cloud-managed host pool without naming physical servers; Cloud additionally scopes that pool to its own configured environment. app-lb itself must never be in the discovery-routing allowlist.

The public-service workflow accepts `HEYO_SERVICE_REPLICA_REGIONS` as a comma-separated list such as `EU,US`, which sets both `replicaRegions` and `desiredReplicas` for every allowlisted service. `HEYO_SERVICE_PLACEMENT_POOL` is required for those allowlisted deployments; use `platform` after the intended shared hosts have registered in that pool. `HEYO_SERVICE_REPLICAS` remains available as either one count or a per-service map such as `heyosecret=2,orchestrator=2`; when both settings are present, their counts must agree. app-lb candidates contain the discovery-backed route definitions and an app-lb replacement health-gates those routes before moving them from the old app-lb backend.

Activate the mode without a config bootstrap race: first deploy compatible Cloud and heyvm versions, then verify that Cloud records one EU and one US host in the target environment and pool `platform`, with distinct node IDs. Merge and deploy this version while the discovery allowlist is unset. Then set `ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES=app-obs,heyosecret,orchestrator`, `HEYO_SERVICE_REPLICA_REGIONS=EU,US`, and `HEYO_SERVICE_PLACEMENT_POOL=platform`; workflow-dispatch `orchestrator` once with `bootstrapDiscoveryRouting=true`, then workflow-dispatch `all`. The bootstrap request leaves Orchestrator on its existing singleton route while loading the allowlist into the active process. The full rollout then runs app-lb → app-obs → HeyoSecret → Orchestrator. Phase 1 keeps one app-lb ingress while placing one replica of every discovery-routed service in EU and one in US.

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
- `POST /orchestration/services/deployments` — deploy a Heyo-managed service; resolves `envRefs` against HeyoSecret. Set `desiredReplicas` (1–16), optionally `replicaRegions`, and a Cloud `placementPool` for an allowlisted discovery-routed service to converge a revision with health-gated, one-at-a-time rolling replacement. Without them, the legacy direct-route single-candidate behavior is preserved.
- `GET  /orchestration/services/{service_id}/discovery` — authenticated, versioned endpoint membership for app-lb, including each endpoint's region when known. Rolling deploys publish and health-gate one candidate, drain one old replica, and repeat. A failed candidate leaves the remaining healthy set serving. `retirePrevious=false` only adds capacity up to `desiredReplicas`.
- `POST /internal/deployments/lifecycle` — callback from the backend reporting deploy state transitions.
- `POST /orchestration/approvals/{approval_id}/decide` — gate an in-flight workflow.
