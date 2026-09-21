# Regional service rollouts

`POST /orchestration/services/regional-rollouts` starts the opt-in, platform-owned
drain-before-upgrade policy. `GET /orchestration/services/regional-rollouts/{operation_id}`
returns durable progress and `POST .../{operation_id}/resume` retries a blocked gate.
`POST .../{operation_id}/rollback` restores retained endpoints for an active or
blocked operation, with fresh health checks and observer drain gates. All endpoints
require the internal API-key bearer token. Rollback after a completed rollout is
not exposed by this API; it cannot overwrite a subsequent operation.

The request has `operationId`, `deployment` (`ServiceDeployRequest`),
`minimumServingReplicas`, `bakeSeconds`, and `drainTimeoutSeconds`. The deployment
must reference an immutable archive, contain at least two regions, and use
`envRefs` rather than plaintext environment values. Existing ingress must already
be discovery-routed; this operation never changes ingress configuration.
`deployment.route` must match the established route. `minimumServingReplicas`
is a replica-count budget, not an automatic load/capacity estimate. It must be
satisfiable outside **every** region in the desired placement. `bakeSeconds`
must be 1–86400; `drainTimeoutSeconds` must be 1–3600. Optional `runtimeByRegion`
overrides `driver`, `image`, and `sizeClass` per target region (for example EU
libvirt and US Firecracker). These settings are saved with each replica slot;
regions without an override use the deployment's settings. The archive revision
is shared; runtime compatibility remains subject to Cloud placement validation.

For each region (in first-occurrence order), the reconciler freshly probes enough
capacity outside the region, excludes the whole region, waits for every configured
load-balancer observer to adopt the version and report old upstreams drained,
creates deterministic candidates, probes them, marks old members draining,
restores the region, waits for observer adoption, and bakes with repeated probes.
Old VMs remain running and excluded for an explicit follow-up rollback; this
component never deletes or stops them and does not claim automatic rollback.
Reactivation cancels historical retirement intents so a previous deployment's
cleanup cannot later stop a restored replica. Successful rollback also restores
the saved service state; candidates remain retained and excluded.

## Persisted plan and item progress

Before execution, admission saves a versioned plan with stable item IDs, ordered
forward and rollback steps, dependencies, regions, and candidate identities.
The plan and its pinned revision, runtime slots, observer topology, and policy
are immutable for that operation ID. Changing them requires a new operation;
this version does not support in-place replanning. Controllers load the stored
items and reject unsupported plan versions or execution cursors absent from the
plan rather than compiling a replacement after restart.

Plan-item progress is stored separately: status, attempts, start/completion times,
and last error. PostgreSQL records cursor changes, item progress, and transition
events atomically, including blocks, resumes, rollback, and completion. Ordinary
health-observation heartbeats do not create events. A controller crash cannot
commit a cursor change without its corresponding progress record.

The rollout API returns `plan`, pinned `slots` and policy values, `items`, and the
latest 100 `events` in chronological order. Full event history remains in the
database. Unvisited steps remain pending; rollback steps are conditional, not
evidence of unfinished work after a successful forward rollout. This is a fixed
regional workflow, not Up's general message planner or a dependency-graph scheduler.

State is PostgreSQL-backed and restart safe. In particular, candidate creation is
preceded by a durable `creating` phase. After an ambiguous restart, only an exactly
matching healthy discovery member is adopted; otherwise the rollout blocks rather
than repeating creation. A blocked operation continues reserving its service until
the same operation is resumed. `operationId` retries are idempotent only when the
entire payload matches.
The admitted observer set is persisted: removing an observer from configuration
cannot bypass its gate mid-rollout. Observer errors block progression. Resume
restarts timed gates; bake time is reset after an observation gap, rather than
counting controller downtime as healthy operation.

This differs from the ordinary application rolling replacement, which keeps old
and new endpoints in normal routing while converging capacity and can retire old
deployments. Regional rollout is deliberately stricter: traffic leaves a region
before its new revision is created, observer and bake gates are mandatory, and
rollback capacity is retained.

## Ingress observers

Configure **every** app-lb instance that can admit traffic for the service in the
orchestrator TOML file (`HEYO_ORCHESTRATOR_CONFIG_PATH`). At least one observer
must be configured in each target region. Each URL addresses a specific app-lb
instance, not a load-balanced URL that can hide an unobserved ingress.

```toml
[[discovery_observers]]
service_id = "example"
region = "eu"
deployment_id = "example"
base_url = "https://eu-ingress-admin.example.com"
token_secret_path = "platform/eu-app-lb-admin"

[[discovery_observers]]
service_id = "example"
region = "us"
deployment_id = "example"
base_url = "https://us-ingress-admin.example.com"
token_secret_path = "platform/us-app-lb-admin"
```

Credentials are resolved from HeyoSecret on observation, never stored in the
rollout plan. Use authenticated app-lb admin endpoints. The controller queries
`GET /deployments/{deployment_id}/discovery-status` and requires the persisted
discovery version, matching serving membership, and zero in-flight requests for
withdrawn peers at every observer. A sleep alone is never proof of drain.

## Bootstrap with existing host-managed app-lbs

For a new service, use the ordinary `POST /orchestration/services/deployments`
API with `desiredReplicas`, `replicaRegions`, and a route with `host`,
`pathPrefix`, and `stripPrefix: false`. Include the service in
`discovery_routed_services`. Opt into host-managed ingress by adding both fields
below to **every** observer for that service:

```toml
ingress_url = "https://eu-ingress.example.com"
discovery_url = "https://orchestrator.example.com/orchestration/services/example/discovery"
```

`ingress_url` addresses that specific ingress, not a global load balancer. Health
probes use the service route's Host header and preserve its prefix. TLS validates
the ingress URL hostname. The `discovery_url` must be identical across observers;
each app-lb must already have `APP_LB_DISCOVERY_URL` pointing to that authority's
base URL and `APP_LB_DISCOVERY_TOKEN` configured through its managed secrets.
Controllers executing the same rollout must share the same PostgreSQL state;
this feature does not replicate independent regional databases.

Bootstrap verifies that the authoritative snapshot equals the controller's
snapshot, then creates missing discovery deployments through app-lb. It requires
the app-lb create-only capability and uses `If-None-Match: *`, never a replacing
POST. Existing deployments must have the same discovery service and route.
Use an unclaimed service hostname/prefix; this is not a migration of existing
production routing. Legacy Traefik-specific route options are rejected.

Only after **all** observers attest the applied source, membership, and version,
and all ingress health probes remain successful, does orchestrator persist the
ingress baseline. It does not require an orchestrator service named `app-lb` or
call the legacy `/service-routes` API. The scalar `ingressBackendUrl` retains the
first configured ingress origin for compatibility; the observer configuration
represents the full ingress set. Regional plans pin that set, including source
and probe URLs, so changes cannot silently bypass an admitted plan's gates.

A failed or interrupted bootstrap may leave discovery registrations and healthy
candidates. They are retained, not destructively rolled back; retry the same
deployment revision to reconcile them. Conflicting routes or credentials require
operator correction. After bootstrap, use `regional-rollouts` for subsequent
drain-before-upgrade revisions; the regional API never registers or changes routes.

## Scope and verification

This is a **service** rollout primitive. It does not yet orchestrate a whole-region
host/control-plane upgrade, upgrade app-lb through its own discovery routing,
hand off database primaries, fence background job writers, or replicate CI queues
and artifacts. Applications must already support simultaneous old/new replicas;
HTTP drain does not quiesce their background work. CI can consume this API once
its application-state and worker-ownership requirements are satisfied.

Run unit tests with `cargo test --locked --manifest-path orchestrator/Cargo.toml --bin orchestrator`.
The ignored `regional_rollout_postgres_restart_drain_and_rollback` test requires
`ORCHESTRATOR_TEST_DATABASE_URL` pointing to disposable PostgreSQL. It uses real
database persistence and HTTP calls, with mock observer/health/secret services;
candidate publication simulates a previous worker's success, not actual VM creation.
It covers asymmetric regional completion, stale observers, outstanding requests,
uncertain creation, failed health checks, observation gaps, and rollback.
