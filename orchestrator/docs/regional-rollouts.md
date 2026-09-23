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

### V3 application execution remains gated

The hierarchical application program has an internal dispatcher for preflight,
policy barriers, correlated candidate creation/recovery, membership staging,
restoration, bake and final verification. Public v3 admission and background
execution remain closed pending complete lifecycle/failure integration; local
fixture results do not authorize activation or establish live acceptance.

Internal application admission requires the expected active generation and
discovery version, an explicit placement pool, immutable archive, runtime revision
and exposed guest port. It pins every retained healthy endpoint and authenticates
all configured gateway participants against the predecessor's boot identities.
Application regions must match the positive-weight baseline regions, and each
region must have enough distinct pinned hosts for its candidate slots. Mutable
draft policy does not authorize application traffic changes.

Archive retrieval and gateway inspection happen outside the service lifecycle
lock. Admission rechecks the complete baseline under that lock and requires fleet
observations no older than five seconds before atomically saving the v3 plan.
An identical operation retry returns its persisted identity even if the archive
or gateways are unavailable; changed intent and concurrent lifecycle owners are
rejected. These are internal integration contracts, not a newly exposed endpoint.
Requests containing the legacy `revisionGuard` are rejected until its cutover
freshness check is integrated; it must not be silently treated as satisfied by
the archive digest or the application's runtime-revision health response.

For an existing v3 operation, rollback enters the persisted current region's
`rollback_entry`, not the legacy region-zero rollback cursor. Repeated rollback
requests do not rewind an entered rollback or resume it if blocked. Explicit
resume preserves its cursor and invalidates outstanding probe work. Late health
responses cannot stage membership across block/resume, even when discovery and
the active policy have not changed.

The internal dispatcher persists an attempt start time independently of health
samples. Each step has `drainTimeoutSeconds` to progress; bake additionally gets
`bakeSeconds`. Expiry blocks the operation without restoring traffic or dropping
its service ownership. A controller restart does not reset this budget. Explicit
resume or advancement to the next step begins a fresh attempt; elapsed downtime
never counts as a healthy bake interval. These additive fields are in migration
`041_add_application_probe_claims.sql`, which is tested locally, not applied to
shared infrastructure by this implementation.

## Regional routing intent (not activated routing)

`PUT /orchestration/services/{id}/regional-policy` stores explicit regional weights
and gateway inventory in the same PostgreSQL discovery set. It uses the existing
internal API-key role and service lifecycle lock; there are no regional credentials.
Apply migrations `037_add_regional_routing_policy.sql` and
`038_add_regional_policy_proposals.sql` through the normal managed upgrade before
using this capability.

```json
{"expectedVersion":17,"policy":{"version":1,"regions":[{"region":"us3","weight":2,"gateways":[{"id":"us-a","backendServerId":"host-us","url":"https://us.example"}]},{"region":"eu1","weight":1,"gateways":[{"id":"eu-a","backendServerId":"host-eu","url":"https://eu.example"}]}]}}
```

`expectedVersion` compares against the current discovery generation (0 for a new
set). An accepted write increments that generation atomically with policy storage.
A stale generation, held lifecycle lock, running ordinary rollout, or running/blocked
regional rollout returns 409 without changing intent. On an ambiguous response,
read discovery and compare policy; do not blindly retry with a newer generation.
Explicit `policy: null` clears intent under the same gates and advances, never resets,
the generation. Omitting `policy` is rejected. Regional snapshots return the same
complete `regionalPolicy` alongside their scoped VM membership.

This stores desired topology only. It does not attest gateway health, policy
adoption, capacity, or drain. **Do not configure it on a live service yet:** the
current flat app-lb consumer refuses policy-bearing snapshots rather than falsely
acknowledging hierarchical routing, and legacy deployment/rollout executors reject
services with regional policy before starting deployment effects. The hierarchical
consumer and staged rollout gates must be integrated before activation. Clearing
unused intent restores the legacy path without erasing its generation history.

### Immutable proposal storage (executor integration pending)

Migration 038 separates policy generation allocation from discovery revision.
An immutable proposal belongs to a service, operation and publication plan item,
and pins its expected active predecessor. PostgreSQL rejects proposal updates and
deletes. One active reference selects a proposal; there are no mutable prepared and
active copies. Schema version remains `RegionalPolicy.version`.

The version-2 routing-only plan separates publication, preparation, activation,
source adoption, outgoing-assignment drain, peer-admission closure and destination
drain. Weight-only changes without withdrawal finish after source adoption;
withdrawing a serving region requires its drain plan and zero target weight while
retaining target inventory. Neither variant creates VMs or reinterprets version-1 plans.
Internal storage primitives commit publication/activation and their cursor/item
journal together under the service lifecycle lock. Exact retries return the
existing generation/receipt; changed intent and stale predecessors fail. Endpoint
membership revision changes do not invalidate the pinned policy predecessor.
Clearing draft intent is refused while an active policy exists, and active policy
also fences legacy executors independently of the draft.

Transition admission/publication is not exposed through HTTP. The reconciler
dispatches already-published version-2 operations to the hierarchical report/gate
executor; version-1 operations retain their legacy path and safety fences.
Internal routing-only admission authenticates every configured participant, checks
its cold/active-predecessor state, environment, placement, exact whole-host route,
namespace and shared discovery authority, and pins boot IDs and observer bindings.
The lifecycle lock protects the final draft/predecessor/conflict recheck and atomic
operation/proposal publication. Exact operation retries return the durable receipt
without polling unavailable gateways; changed intent is rejected. Reconciliation
rejects observer/configuration drift. Routing-only transitions cannot change the
gateway inventory or boots; those require a separately fenced fleet handoff.
Cold route enrollment is also internal. It uses existing host-managed app-lb admin
APIs with `If-None-Match: *`, requires `x-app-lb-regional-admission: 1`, and never
updates an existing deployment. All observer bindings and secret references are
validated before writes. The service lifecycle lock excludes admission/deployment
while routes are registered; partial registrations remain cold and retry reads
them back without changing their boot identity. Enrollment itself publishes no
proposal and cannot be used after an active policy exists.

For enrollment, each observer supplies `discovery_token_secret` and
`regional_peer_token_secret`: distinct existing app-lb secret IDs in the requested
namespace, not raw credentials. The source URL has exactly `?region=<region>`;
the registered source omits that query because app-lb supplies its runtime scope.
The application health path is explicit and must match on retries. Successful
enrollment verifies authenticated cold status and credential readiness, not serving
health. Fresh admission remains a separate step. Provisioning hosts/secrets and
replacing active processes are not part of this route-registration primitive.

Publication validates the union of predecessor and proposed gateway participants
against the operation's immutable boot/region inventory. Report storage retains
per-service/gateway/boot sequence high-water marks across operations. Duplicates do
not refresh evidence; observing a different boot permanently invalidates the old
boot's reports, including delayed higher-sequence responses. This invalidation is
not proof of network fencing and cannot authorize a replacement process to serve.

Gate evaluation uses server timestamps and a five-second maximum sample age,
including poll duration. Activation rechecks fresh preparation within its ownership
transaction. Source-adoption samples must follow activation, and destination
admission ACKs must follow the close command. Source assignment totals cover all
retained generations; zero outgoing assignments does not substitute for closed
destination admission and zero local work. Each successful gate advances one
persisted dependency and its item journal atomically.

Hierarchical discovery (`protocol=regional-v1`) reads policy history, active reference,
regional endpoints and admission fence in one repeatable-read transaction. Each
discovery observer must explicitly bind `gateway_id`, region, deployment and the exact
applied `discovery_url` (including its region query). Observer credentials come from
HeyoSecret; redirects and cached reports are rejected. Scope is checked before report
storage. An authenticated observation of a cold boot that has no routing authorization
still invalidates its predecessor's evidence; it never proves predecessor drain.

The local end-to-end fixture uses actual app-lb processes and authenticated polls
against disposable PostgreSQL; storage tests also inject reports to discriminate
stale/replayed/foreign evidence. Run the integrated fixture after building app-lb
with `reqwest/rustls-tls-native-roots`, setting its absolute path in
`APP_LB_TEST_BINARY` and the disposable database in `ORCHESTRATOR_TEST_DATABASE_URL`:

```sh
cargo test --locked --manifest-path orchestrator/Cargo.toml --bin orchestrator two_real_gateways -- --include-ignored --nocapture
```

This verifies local integration, not live drain or safe candidate/host replacement.
Keep public admission/activation APIs and legacy flat-consumer fences closed until
managed fleet lifecycle, external ingress inventory/fencing, replacement handoff
and live acceptance gates are explicitly satisfied. The configured observer list
alone cannot prove that no external legacy ingress still admits work.

## Ingress observers

Discovery reads support `GET /orchestration/services/{id}/discovery?region=eu1`.
The response contains only that region's endpoints, retains the shared version,
and echoes `region`, including for empty sets. Omitting the parameter preserves
the complete endpoint set. app-lb can opt in with `discovery.region`; it rejects
responses that omit or contradict the scope. This does not change the current
rollout executor into a hierarchical gateway coordinator: regional weights,
gateway inventory and staged source-assignment/destination-drain gates are still
required before using that topology for coordinated upgrades.

Configure **every** app-lb instance that can admit traffic for the service in the
orchestrator TOML file (`HEYO_ORCHESTRATOR_CONFIG_PATH`). At least one observer
must be configured in each target region. Each URL addresses a specific app-lb
instance, not a load-balanced URL that can hide an unobserved ingress.

Managed VM deployments can instead set `ORCHESTRATOR_DISCOVERY_OBSERVERS_JSON`
to a JSON array with the same observer fields in their persisted `vm.env_vars`.
TOML takes precedence, including an explicitly empty `discovery_observers = []`.
Invalid JSON fails startup; these settings contain secret references, never tokens.

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
the ingress URL hostname. The `discovery_url` must be identical across observers.
Set `discovery_token_secret = "discovery-reader"` on each observer to register
the authority with the route through app-lb's managed API. Provision this app-lb
secret in the default namespace with a `token` key through the existing secrets
API, using HeyoSecret as the source of truth. This is the discovery reader token,
not the observer's app-lb admin token. Bootstrap checks support and persists only
the reference. No host environment edit is required. If the field is omitted,
the legacy `APP_LB_DISCOVERY_URL/TOKEN` host configuration remains required.
Namespace-scoped app-lb tokens can return 403 rather than 404 for an absent
deployment. Bootstrap handles either response with capability-checked,
create-only registration (`If-None-Match: *`), followed by an authenticated
exact-spec read-back. It does not widen the token or replace an existing route;
denied creation or denied read-back still fails the rollout.
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
