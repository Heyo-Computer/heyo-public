# Orchestrator

The orchestrator is the HWS control plane that records what should be running where. It provisions sandboxes for CI jobs, deploys and rolls long-running services, publishes the service discovery sets app-lb routes from, and runs region-by-region rollouts with durable, restart-safe progress.

## What it is

The orchestrator (`orchestrator/`) is one Rust binary backed by PostgreSQL. Each request persists desired state and returns. Background reconcilers then converge that state by calling the systems that actually move VMs and traffic.

| Responsibility | Routes | Notes |
| --- | --- | --- |
| **Resource sandboxes** for CI and ad-hoc jobs | `/orchestration/resources/*`, `/orchestration/ci/artifacts/*` | Upload a workspace archive, launch a sandbox, `exec` in it, stop or delete it |
| **Service deployments** | `/orchestration/services/deployments`, `/orchestration/services/{id}` | Blue/green replacement of Heyo-managed services, or rolling replicas for discovery-routed services |
| **Service discovery** | `/orchestration/services/{id}/discovery`, `/orchestration/services` | The versioned endpoint membership that [app-lb](app-lb.md) polls, and a shared inventory across regions |
| **Regional rollouts** | `/orchestration/services/regional-rollouts/*` | Drain a region, upgrade it, verify, restore and bake, one region at a time. See [multi-region](multi-region.md). |
| **Application adoption and updates** | `/orchestration/services/adoptions`, `.../updates`, `.../managed-updates` | Bring an existing app-lb-managed application (such as CI) under orchestrator lifecycle control |
| **Agent workflows** | `/orchestration/threads`, `/orchestration/parent-jobs/*`, `/orchestration/approvals/*` | LLM-assisted discovery, planning, patching and deployment, with human approval gates |

It depends on the following:

| Dependency | Used for | Required |
| --- | --- | --- |
| PostgreSQL | All durable state: workflows, service state, discovery sets, rollout plans, events | Yes |
| Heyo Cloud (internal API) | Host allocation, archive storage and VM creation. The orchestrator never talks to heyvm directly. | For anything that creates a VM. Cloud is not part of the hws repository. |
| [HeyoSecret](heyosecret.md) | Resolves `envRefs` and `env_from`, observer tokens, lifecycle tokens and git credentials | When deployments reference secrets |
| [app-lb](app-lb.md) | Consumes discovery. Its admin API is the observer the orchestrator checks for drain. | For discovery-routed and regional services |
| NATS | Optional event transport shared with Cloud | No |

```text
   CI ──(JWT)──► /orchestration/resources/*  ─┐
                                              │   ┌──────────────┐     ┌────────────┐
 operator/CI ─(internal key)─► /services/* ───┼──►│ orchestrator │────►│   Cloud    │──► heyvm (VMs)
                                              │   │  + Postgres  │     └────────────┘
                                              │   └──────┬───────┘
             HeyoSecret ◄── envRefs / tokens ─┘          │ discovery (versioned endpoints)
                                                         ▼
                                           app-lb (one or more per region)
                                           polls discovery, reports drain status
```

## Running it

### Locally

```sh
cp orchestrator/.env.example orchestrator/.env   # then edit; see the configuration notes below
cargo run --locked --manifest-path orchestrator/Cargo.toml --bin orchestrator
curl -si localhost:4446/health
```

At startup the orchestrator does the following, in order:

1. Loads `.env` from the crate directory, falling back to `.env` in the working directory.
2. Loads the configuration.
3. Connects to PostgreSQL and applies the SQL files in `migrations/`.
4. Fetches backend capabilities from `ORCHESTRATOR_BACKEND_API_URL` + `/capabilities` if that is set.
5. Starts four reconcilers: workflow steps, delayed retirement, regional rollouts and application updates.

The migrations directory is resolved from `ORCHESTRATOR_MIGRATIONS_DIR`, then `./migrations`, then the compile-time crate directory.

`GET /health` returns `{"status":"ok", "deploymentGitSha", "deploymentPrNumber", "deploymentId"}`. The body fields come from `HEYO_ORCHESTRATOR_DEPLOYMENT_*` environment variables and are diagnostic only. The `x-heyo-revision` response header holds the build-time `HEYO_BUILD_GIT_SHA`, or `unknown` for unstamped builds. Use the header, not the body, to tell which binary answered.

### Packaging

| Artifact | Built by |
| --- | --- |
| Release binary, migrations and a relocatable `start.sh` | [`.ci/workflows/orchestrator.yml`](../.ci/workflows/orchestrator.yml). It runs the tests, stamps the Git SHA and uploads the `orchestrator-linux` artifact. It does not deploy. |
| Container image | `docker build -f orchestrator/Dockerfile .` from the repository root |
| Firecracker rootfs for app-lb | `orchestrator/Dockerfile.firecracker`, used by the inert template [`orchestrator/app-lb.us3.json`](../orchestrator/app-lb.us3.json) (zero replicas, no routes, commit placeholder) |
| Orchestrator-managed service spec | [`.heyo/services/orchestrator.json`](../.heyo/services/orchestrator.json), submitted by [`.heyo/workflows/deploy-heyo-services.yml`](../.heyo/workflows/deploy-heyo-services.yml) |

A healthy `/health` does not prove that deployments work. Before you accept deployment traffic, check that `CLOUD_INTERNAL_URL` reaches a Cloud that accepts the internal key and can allocate a backend in this region. The orchestrator also reads Cloud's deployment-status table directly, so Cloud and the orchestrator must share a database.

## Configuration

Configuration is layered as follows:

1. **Built-in defaults.**
2. **A TOML file.** The path is `HEYO_ORCHESTRATOR_CONFIG_PATH`, else `HEYO_CONFIG_PATH`, else `~/.heyo/orchestrator/orchestrator.toml`. The file is used if it exists.
3. **An `ORCHESTRATOR_`-prefixed environment layer.** Its keys are split on `_`. This works for nested keys such as `ORCHESTRATOR_NATS_ENABLED` (`nats.enabled`). It does **not** work for field names that contain an underscore: `ORCHESTRATOR_AGENT_MODEL` becomes `agent.model`, not `agent_model`.
4. **Explicitly read environment variables**, listed below. These fill a value only when layers 1–3 left it empty.

**Any setting not in the environment table can only be set in the TOML file.** This covers the agent provider and model settings, the `db_*` pool settings, `target_os` and the driver lists. The `ORCHESTRATOR_AGENT_*`, `ORCHESTRATOR_DB_*` and `ORCHESTRATOR_TARGET_*` lines in `.env.example` and the service workflow have no effect. So do `ORCHESTRATOR_DATABASE_URL`, `ORCHESTRATOR_JWT_SECRET`, `ORCHESTRATOR_INTERNAL_API_KEY` and `ORCHESTRATOR_CLOUD_INTERNAL_URL`. Use the unprefixed names in the table.

### Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `DATABASE_URL` | required | PostgreSQL URL (TOML: `database_url`) |
| `JWT_SECRET` | required | HS256 secret for user and CI tokens (issuer `auth-service`, audience `heyo-app`) (TOML: `jwt_secret`) |
| `CLOUD_INTERNAL_API_KEY`, else `INTERNAL_API_KEY` | required | The orchestrator's **internal API key**. It protects `/orchestration/services/*` and `/internal/*`, and is sent to Cloud (TOML: `internal_api_key`). |
| `CLOUD_INTERNAL_URL` | `http://127.0.0.1:4445` | Cloud internal API base URL |
| `ORCHESTRATOR_SERVER_PORT` | `4446` | Listen port |
| `ORCHESTRATOR_MIGRATIONS_DIR` | see above | Migrations directory |
| `ORCHESTRATOR_HEYOSECRET_URL`, else `HEYOSECRET_URL` | empty | HeyoSecret base URL. Required when a deployment has `envRefs` or `env_from`. |
| `ORCHESTRATOR_HEYOSECRET_INTERNAL_API_KEY`, else `HEYOSECRET_INTERNAL_API_KEY` | internal API key | HeyoSecret bearer. Falls back to the orchestrator's internal key when unset. |
| `ORCHESTRATOR_BACKEND_API_URL`, else `BACKEND_API_URL` | empty | Backend daemon URL. When set, `GET /capabilities` overrides the target OS and driver lists at startup. |
| `ORCHESTRATOR_PROXY_BASE_DOMAINS` | empty | Comma-separated wildcard proxy domains. Deployment URLs under these are probed through the backend API rather than public DNS. |
| `ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES` | empty | Comma-separated allowlist of service IDs whose ingress is app-lb discovery rather than a direct route. Never include `app-lb`. |
| `ORCHESTRATOR_DISCOVERY_OBSERVERS_JSON` | `[]` | JSON array of discovery observers. Used only when the TOML file has no `discovery_observers` key. Invalid JSON fails startup. |
| `ORCHESTRATOR_EXTERNAL_SERVICE_BINDINGS_JSON` | `[]` | JSON array of external service bindings. Used only when the TOML file has no `external_service_bindings` key. |
| `ORCHESTRATOR_TRAEFIK_DYNAMIC_CONFIG_DIR` | empty | Directory where legacy non-discovery cutovers write `heyo-service-<id>.yml` Traefik routes |
| `ORCHESTRATOR_SERVICE_STATE_DIR` | `~/.heyo/orchestrator/services` | Per-service JSON state files |
| `ORCHESTRATOR_REPOSITORY_CLONE_ROOT` | derived from the state dir | Where workflow runs clone repositories |
| `ORCHESTRATOR_RESOURCE_READY_TIMEOUT_SECONDS` | `900` | How long `wait-ready` waits for a resource sandbox |
| `ORCHESTRATOR_AGENT_API_KEY` | empty | API key for the global agent provider |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `MISTRAL_API_KEY`, `GOOGLE_API_KEY` | empty | Per-provider keys, used when a phase's provider differs from the global one |
| `ORCHESTRATOR_GIT_AUTH_TOKEN`, `CI_GIT_AUTH_TOKEN`, `GITHUB_TOKEN` | empty | Git token for cloning. If none is set, the token is read from HeyoSecret. |
| `ORCHESTRATOR_GIT_AUTH_TOKEN_SECRET_PATH` | `cicd/git-auth-token` | HeyoSecret path for the git token |
| `ORCHESTRATOR_GIT_SSH_PRIVATE_KEY`, `CI_GIT_SSH_PRIVATE_KEY` | empty | SSH key for cloning |
| `ORCHESTRATOR_GIT_SSH_PRIVATE_KEY_SECRET_PATH` | `cicd/git-ssh-private-key` | HeyoSecret path for the SSH key |
| `ORCHESTRATOR_GIT_AUTH_USERNAME`, else `CI_GIT_USERNAME` | provider default | Username paired with the git token |
| `ORCHESTRATOR_NATS_URL`, else `CLOUD_NATS_URL`, else `NATS_URL` | `nats://127.0.0.1:4222` | NATS URL |
| `ORCHESTRATOR_NATS_ENABLED`, else `CLOUD_NATS_ENABLED` | `false` | Enable NATS |
| `HEYO_ORCHESTRATOR_DEPLOYMENT_GIT_SHA`, `_PR_NUMBER`, `_ID` | empty | Echoed in the `/health` body only |
| `RUST_LOG` | `warn,orchestrator=info` | Log filter |

### TOML-only settings

| Key | Default | Meaning |
| --- | --- | --- |
| `agent_provider`, `agent_model` | `anthropic`, `claude-opus-4-7` | Global LLM provider and model for agent workflows |
| `agent_provider_{discovery,planning,review,patch}`, `agent_model_{...}` | discovery: `mistral` / `mistral-large-latest` | Per-phase overrides. They apply only when both the provider and the model are set. |
| `agent_timeout_seconds`, `agent_max_iterations` | `900`, `15` | Agent limits |
| `db_max_connections`, `db_min_connections` | `20`, `2` | Pool size, per process. Remember that every replica holds its own pool. |
| `db_connect_timeout_seconds`, `db_acquire_timeout_seconds`, `db_idle_timeout_seconds`, `db_max_lifetime_seconds` | `8`, `30`, `600`, `1800` | Pool timeouts |
| `target_os` | the orchestrator's own OS | Backend OS used for driver selection |
| `target_supported_drivers` | Linux: `firecracker_containerd`, `firecracker`, `libvirt` | Drivers for sandboxes. The first one is the default. |
| `target_archive_supported_drivers` | Linux: `libvirt` | Drivers that accept archive overlays |
| `discovery_observers` | `[]` | Every app-lb that can admit traffic for a discovery-routed service (see below) |
| `external_service_bindings` | `[]` | Retained app-lb applications the orchestrator may adopt (see below) |

A minimal file:

```toml
database_url = "postgres://orchestrator:change-me@127.0.0.1:5432/orchestrator"
jwt_secret = "change-me"
internal_api_key = "change-me"
heyosecret_url = "https://heyosecret.example.com"
discovery_routed_services = "example"

[nats]
enabled = false
```

## Authentication

| Caller | Credential | Routes |
| --- | --- | --- |
| Users and CI | `Authorization: Bearer <JWT>`, HS256 with `JWT_SECRET`, issuer `auth-service`, audience `heyo-app` | `/orchestration/templates`, `threads`, `parent-jobs`, `workflow-runs`, `approvals`, `backend-capabilities`, `resources/*`, `ci/artifacts/*` |
| Platform services and operators | `Authorization: Bearer <internal API key>`, exact match | `/orchestration/services/*` (except the next row) and `/internal/deployments/lifecycle` |
| An adopted application | Its own lifecycle token, read from HeyoSecret at the binding's `lifecycle_token_secret_path` | `/orchestration/services/{id}/updates`, `.../managed-updates`, `.../instances/{deployment_id}/http-request` |

The discovery endpoint uses the internal key. The app-lb secret you register as a discovery reader must therefore hold the orchestrator's internal API key.

## API reference

### Resource sandboxes (CI)

| Method and path | Purpose |
| --- | --- |
| `POST /orchestration/resources/archives` | Upload a workspace archive inline (bodies up to 2 GiB) |
| `POST /orchestration/resources/archives/presign`, `.../finalize` | Direct upload for large archives |
| `GET /orchestration/resources/archives/{archive_id}` | Download an archive |
| `GET /orchestration/resources/archives/{archive_id}/download-url` | Get a presigned download URL |
| `POST /orchestration/resources/deployments` | Request a sandbox: `{deploymentId, name, region, driver, image, sizeClass, archiveBytesBase64?, ports?, env?, envRefs?, startCommand?, workingDirectory?, setupHooks?, ttlSeconds?, metadata?}` |
| `POST /orchestration/resources/deployments/{id}/wait-ready` | Block until ready, up to `ORCHESTRATOR_RESOURCE_READY_TIMEOUT_SECONDS` |
| `POST /orchestration/resources/deployments/{id}/exec` | Run a command synchronously |
| `POST .../{id}/exec-operations`, `GET .../{id}/exec-operations/{op}` | Run a command asynchronously and poll its result |
| `POST /orchestration/resources/deployments/{id}/stop` | Stop the sandbox |
| `DELETE /orchestration/resources/deployments/{id}` | Delete the sandbox |
| `POST /orchestration/ci/artifacts/presign`, `.../finalize` | Upload CI run artifacts |

### Services

| Method and path | Purpose |
| --- | --- |
| `POST /orchestration/services/archives/presign`, `.../finalize` | Upload a service archive and get an immutable `archiveId` |
| `POST /orchestration/services/deployments` | Deploy a service from a spec (below) |
| `GET /orchestration/services/deployments/{deployment_id}` | Status of one deployment run |
| `GET /orchestration/services/{service_id}` | Current service state (active and previous deployment, route, replicas) |
| `GET /orchestration/services?after=<id>` | Shared inventory, up to 100 services per page with `nextCursor`. Each page is one repeatable-read snapshot. A database failure returns `503` and never falls back to local files. |
| `GET /orchestration/services/{service_id}/discovery[?region=]` | Versioned endpoint membership for app-lb. `region` filters endpoints, keeps the shared `version` and echoes `region`. `protocol=regional-v1&region=&gatewayId=&bootId=` returns the hierarchical snapshot. |
| `PUT /orchestration/services/{service_id}/regional-policy` | Store draft regional weights and gateway inventory: `{expectedVersion, policy}`. See [multi-region](multi-region.md). |
| `POST /orchestration/services/regional-rollouts` | Start a regional rollout |
| `GET /orchestration/services/regional-rollouts/{operation_id}` | Plan, slots, items and the latest 100 events |
| `POST .../regional-rollouts/{operation_id}/resume` | Retry a blocked gate |
| `POST .../regional-rollouts/{operation_id}/rollback` | Roll back an active or blocked operation |
| `POST /orchestration/services/adoptions` | Adopt an existing app-lb application (internal key) |
| `POST /orchestration/services/{service_id}/updates` | The application submits `{operationId, intentHash}` for a prepared release |
| `POST /orchestration/services/{service_id}/managed-updates` | The application submits `{operationId, archiveId, archiveSha256, runtimeRevision}` into a regional plan. Returns `202`. |
| `GET /orchestration/services/{service_id}/managed-updates/{operation_id}` | Managed update progress |
| `POST /orchestration/services/{service_id}/instances/{deployment_id}/http-request` | Authenticated forwarding from an application to one of its own instances |
| `POST /internal/deployments/lifecycle` | Cloud reports deployment state transitions |

### Agent workflows

| Method and path | Purpose |
| --- | --- |
| `GET /orchestration/templates` | Built-in templates: `app.integrate_with_heyo_and_deploy`, `app.deploy_to_heyo`, `app.deploy_from_plan`, `app.adhoc_review_plan`, `app.adhoc_revise_plan` |
| `POST /orchestration/threads`, `GET /orchestration/threads/{id}`, `.../timeline`, `POST .../artifacts` | Create and inspect a workflow thread |
| `POST /orchestration/parent-jobs/compile`, `GET /orchestration/parent-jobs/{id}` | Compile and read a parent job |
| `GET /orchestration/workflow-runs` | List runs |
| `POST /orchestration/approvals/{approval_id}/decide` | Approve or reject a gated step |
| `GET /orchestration/backend-capabilities` | Target OS and drivers in effect |

Workflow steps run through adapters: `ai.discovery`, `ai.planning`, `repo.summary`, `repo.key_files`, `repo.framework_detection`, `repo.patch`, `repo.verify`, `heyo.deploy_preflight`, `heyo.deploy` and `heyo.healthcheck`. AI steps never have side effects. Deterministic adapters apply patches, run verification and deploy. Steps are claimed with row locks, so two orchestrator instances cannot run the same step. See the [design notes](design/ORCHESTRATOR_DESIGN.md) for the state machine.

## Service deployment spec

`POST /orchestration/services/deployments` takes an app-lb-style snake_case document. Unknown fields and unsupported features are rejected; they are not ignored. The same format lives in [`.heyo/services/*.json`](../.heyo/services).

```json
{
  "id": "example",
  "user_id": "heyo-system",
  "vm": {
    "driver": "libvirt",
    "image": "ubuntu:24.04",
    "port": 8080,
    "start_command": "cd /workspace && exec ./start.sh",
    "env_vars": {"PORT": "8080"},
    "env_from": [{"secret": "example", "key": "database-url", "as": "DATABASE_URL"}]
  },
  "routes": [{"host": "example.example.com"}],
  "health": {"path": "/health", "timeout_secs": 5},
  "deploy": {"archive_id": "...", "region": "EU", "async": true, "drain_seconds": 30}
}
```

| Section | Supported fields |
| --- | --- |
| top level | `id`, `user_id`, `account_id?`, `vm`, `routes?`, `health?`, `scaling?`, `deploy` |
| `vm` | Required: `driver`, `image`, `port`. Optional: `open_ports`, `start_command`, `working_directory`, `setup_hooks`, `size_class` (default `small`), `ttl_seconds`, `env_vars`, `env_from`. `mounts` is rejected. |
| `vm.env_from` | `{secret, key?, as?}` resolves the **HeyoSecret** path `<secret>/<key>@active`. `key` defaults to `token`. `as` defaults to the key in upper case. A secret overrides an `env_vars` literal of the same name. `namespace` and duplicate targets are rejected. |
| `routes` | Zero or one route with an exact `host`, plus optional `path_prefix` and `strip_prefix` (default `false`) |
| `health` | HTTP `path` (default `/`) on the VM port, with `timeout_secs` (default `2`) per probe |
| `scaling` | Fixed `min_replicas == max_replicas`, from 1 to 16. Autoscaling fields are rejected. |
| `deploy` | `deployment_id`, `name`, `async`, `archive_id` or `archive_bytes_base64`, `archive_name`, `region` (default `local`), `placement_pool`, `replica_regions`, `health_timeout_seconds` (default `180`), `retire_previous` (default `true`), `retire_previous_async`, `delete_previous`, `drain_seconds` (default `10`), `metadata`, `revision_guard {repository_url, ref, expected_sha, force?}`, `ingress` (Traefik options), `host_mounts [{host_path, sandbox_path, read_only}]` |

The orchestrator resolves `env_from` into `envRefs` of the form `NAME=heyosecret://<path>@active`. It reads the values from HeyoSecret just before it asks Cloud for the VM. Resolved values are never stored. Each deployment's creation recipe, including the exact secret versions, is recorded for replay.

### Deployment behaviour

- **Single candidate (default).** The previous healthy deployment keeps serving while the candidate converges. The candidate and the app-lb route must stay healthy for 10 seconds before cutover. The previous deployment is then drained and, if requested, stopped or deleted.
- **Rolling replicas.** This mode applies to services listed in `ORCHESTRATOR_DISCOVERY_ROUTED_SERVICES`. The request must include the service's stable route and `scaling`. `deploy.replica_regions` must have exactly one entry per replica. The orchestrator publishes one health-gated candidate to discovery, drains one old replica, and repeats. A failed candidate leaves the remaining healthy set serving.
- **Retirement is delayed and fenced.** An old replica is stopped only after its drain has expired, its owning rollout is still current and successful, and a per-service PostgreSQL advisory lock is held. A later rollout that reactivates a previous deployment cancels that deployment's pending retirement.
- **Environment binding.** An optional `deployment_environment` scopes Cloud placement. It is bound to the service ID permanently, so use separate service IDs for staging and production.

## Discovery and app-lb

For a discovery-routed service, app-lb holds a static deployment whose `discovery` block points at this orchestrator. app-lb replaces its upstream list from each accepted snapshot and keeps the last good set when the orchestrator is unreachable.

```json
{"id": "example", "routes": [{"host": "example.example.com"}], "upstreams": [],
 "discovery": {"service_id": "example",
   "source": {"url": "https://orchestrator.example.com/orchestration/services/example/discovery",
              "auth": {"secret": "discovery-reader", "key": "token"}}}}
```

`discovery-reader` is an **app-lb** secret. Give it the orchestrator's internal key as its value, using HeyoSecret as the source of truth. See [HeyoSecret](heyosecret.md#app-lb-env_from-and-secret-key-references).

**Observers** are how the orchestrator proves that a drain happened. List **every** app-lb instance that can admit traffic for a service. Each URL must address one instance, not a load balancer in front of several.

```toml
[[discovery_observers]]
service_id = "example"
region = "eu1"
deployment_id = "example"                           # app-lb deployment id
base_url = "https://eu1-app-lb-admin.example.com"   # that app-lb's admin API
token_secret_path = "platform/eu1-app-lb-admin"     # HeyoSecret path of an app-lb admin token
# Optional host-managed ingress bootstrap:
ingress_url = "https://eu1-ingress.example.com"
discovery_url = "https://orchestrator.example.com/orchestration/services/example/discovery"
discovery_token_secret = "discovery-reader"         # app-lb secret id, not a value
# Hierarchical (regional-v1) only:
# gateway_id = "eu1-a"
# regional_peer_token_secret = "gateway-peer"
```

The controller calls `GET /deployments/{deployment_id}/discovery-status` on each observer. A drain gate passes only when every observer reports the persisted discovery version, matching serving membership, and zero in-flight requests to withdrawn peers. Elapsed time alone never counts as a drain.

When `ingress_url`, `discovery_url` and `discovery_token_secret` are set, the first deployment of a new service registers the discovery route on each app-lb. Registration is create-only (`If-None-Match: *`) and is read back to confirm it. The orchestrator never replaces an existing route.

**External service bindings** let the orchestrator adopt an application that app-lb already runs, such as CI, without creating a VM:

```toml
[[external_service_bindings]]
service_id = "ci"
authority = "https://app-lb-admin.example.com/"     # credential-free origin
namespace = "default"
region = "eu1"
deployment_id = "ci-eu1"
health_origin = "https://ci.example.com/"
token_secret_path = "platform/app-lb-admin"         # HeyoSecret path
lifecycle_token_secret_path = "ci/lifecycle-token"  # HeyoSecret path
```

## Relationship to HeyoSecret

The orchestrator is a HeyoSecret client, through the `heyosecret-client` crate. It reads secrets for:

- `envRefs` and `env_from` on service and resource deployments
- `token_secret_path` for each observer and external binding
- lifecycle tokens
- git credentials

It never writes secrets. HeyoSecret has no per-path authorization, so the orchestrator is the policy layer. A deployment names paths, and the orchestrator decides which paths to resolve and passes only the values to Cloud.

HeyoSecret is itself deployed by the orchestrator in the managed setup. Its update path reads `heyosecret/database-url` and `heyosecret/master-key` from the running instance.

## Operations

**Check which binary is serving.**

```sh
curl -sI https://orchestrator.example.com/health | grep -i x-heyo-revision
```

**Watch a regional rollout.**

```sh
curl -s -H "Authorization: Bearer $ORCHESTRATOR_INTERNAL_API_KEY" \
  https://orchestrator.example.com/orchestration/services/regional-rollouts/$OP | jq '.status, .phase, .items[-1]'
```

**Run the tests.**

```sh
cargo test --locked --manifest-path orchestrator/Cargo.toml --bin orchestrator
# PostgreSQL-backed suites need a disposable database:
ORCHESTRATOR_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:54329/postgres \
  cargo test --locked --manifest-path orchestrator/Cargo.toml -- --include-ignored
```

The two-gateway drain fixture, `two_real_gateways_drain_through_authenticated_durable_barriers`, also needs `APP_LB_TEST_BINARY` set to an app-lb binary built with `--features reqwest/rustls-tls-native-roots`.

**Database recovery.** Never edit rollout, retirement or discovery rows while deployments or cleanup are running. Stop them first. A database trigger cannot recall a Cloud deletion that is already in flight.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| Startup: `JWT_SECRET is required` or `DATABASE_URL is required` although `ORCHESTRATOR_JWT_SECRET` or `ORCHESTRATOR_DATABASE_URL` is set | Prefixed multi-word names are not read. Use `JWT_SECRET` and `DATABASE_URL`, or the TOML file. |
| Startup: `ORCHESTRATOR_INTERNAL_API_KEY is required` | Despite the message, set `CLOUD_INTERNAL_API_KEY` or `INTERNAL_API_KEY`, or `internal_api_key` in TOML. |
| Agent model or provider settings are ignored | `ORCHESTRATOR_AGENT_*` environment variables are not read. Set them in TOML. |
| `ORCHESTRATOR_HEYOSECRET_URL or HEYOSECRET_URL is required when envRefs are present` | A deployment uses `env_from` but HeyoSecret is not configured. |
| Service deploy returns `422` | The body is an old flat camelCase request. `/services/deployments` accepts only the snake_case spec. |
| Regional rollout returns `409` | The `operationId` was reused with a different payload, or the service is held by another rollout or its lifecycle lock, or it carries regional policy the legacy executor refuses. |
| app-lb pool for a discovery service stays empty | Check the app-lb secret holds the orchestrator internal key, the service ID matches, and `GET .../discovery` returns endpoints. See [app-lb troubleshooting](app-lb.md). |
| Deployment `/health` passes but deploys fail | Cloud is unreachable, rejects the internal key, or has no backend in the target region or pool. |
