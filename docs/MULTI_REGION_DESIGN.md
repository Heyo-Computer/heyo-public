# Multi-region serving: Phase 0 design

Status: proposed; documentation only. This document defines the replacement
rollout plan for coordinated multi-region application serving. It does not claim
that installing gateways or registering two backends completes any serving phase.

## Proposed architecture

**Keep app-lb as a regional data plane. Add the global service view and decision
loop to Orchestrator, using Cloud for host allocation.** Do not turn app-lb into a
scheduler and do not build a second service registry beside Orchestrator discovery.

The design has two paths: a control loop that places capacity and assigns traffic,
and a request path that continues without a control-plane call per request.

### Operator experience: one concise JSON service file

Adopt the style of [app-lb deployment files](../app-lb/examples/README.md): one
declarative file with `id`, `routes`, `vm`, `scaling` and `health`. Extend it with
regional overrides instead of exposing separate operator-managed placement,
discovery and traffic-assignment documents. This is a proposed authoring format,
not an assertion that existing app-lb accepts the new fields.

Illustrative fragment (images are placeholders; application routes, launch settings
and secret references are omitted):

```json
{
  "id": "cloud",
  "scaling": {
    "min_replicas": 1,
    "max_replicas": 2,
    "target_concurrency": 32
  },
  "health": { "path": "/health" },
  "regions": {
    "EU": { "vm": { "driver": "libvirt", "image": "cloud-libvirt-release" } },
    "US": { "vm": { "driver": "firecracker", "image": "cloud-fc-release" } }
  },
  "traffic": { "weights": { "EU": 50, "US": 50 } }
}
```

For a regional service, common settings apply to each listed region. Here the
minimum is one replica **per region**, not one replica shared across both; the
maximum is two per region. Region-specific `vm` and `scaling` fields override common
fields individually. Arrays replace rather than append, and ambiguous/unknown
regional fields are rejected. Only replica/resource/runtime settings can vary by
region initially; routes and application identity remain common. Driver and image
must be a compatible pair for each region.

Weights describe explicit relative traffic shares; they are not replica counts or
host resource percentages. `target_concurrency` retains app-lb's meaning of target
in-flight requests per instance, not a measured hard safety limit. Feedback-based
weighting and admission budgets remain Phase 2 work. Do not inherit legacy forced
VM termination on drain timeout into the regional maintenance safety barrier.

Orchestrator validates and applies this file as desired state, using its existing
deployment machinery. It derives Cloud allocation requests and app-lb routing
snapshots; operators do not write those generated objects. Resource observations,
deployment IDs, health, pending reservations and controller ownership remain runtime
state, not fields operators must maintain in Git. Applying unchanged intent must
not restart healthy replicas. Omitting a region from an updated file requests an
explicit reviewed drain/removal, never immediate deletion.

Use the file as the sole editable service intent for an opted-in deployment;
generated app-lb configuration is not independently editable. Existing legacy
deployments keep their current input format and behavior. Adoption means reusing
app-lb's familiar vocabulary, not importing every local lifecycle feature or
rewriting Cloud/Orchestrator around a second configuration engine. Exact field
validation and serialization will be settled in the implementation contract.

```diagram
                         Operator's service intent
                       “Cloud in both EU and US”
                                    │
                                    ▼
                     ┌────────────────────────────┐
                     │ Orchestrator               │
                     │ Regional service plan      │
                     │ Observed service discovery │
                     │ Traffic distribution       │
                     └──────┬─────────────┬───────┘
                            │             │
                   Place in region        │ Routing assignments
                            ▼             ▼
                     ┌─────────────┐ ┌───────────────────┐
                     │ Cloud       │ │ eu1 / us3 app-lbs │
                     │ Host choice │ │ Route and measure │
                     │ Reservation │ └─────────┬─────────┘
                     └──────┬──────┘           │
                            │                  │ Service load feedback
                            ▼                  └──────▶ Orchestrator
                     ┌─────────────┐
                     │ heyvm       │──Host resources──▶ Cloud
                     │ Execute VM  │
                     └─────────────┘
```

The boxes are responsibilities in existing services, not new standalone services.
Multiple Cloud or Orchestrator instances share authoritative state; they do not
each run an independent global scheduler. A single active owner reconciles a given
service at a time, with durable ownership that rejects a former owner's writes.

### Decision A: where must this service run?

Orchestrator owns a regional service plan: required presence in EU and US, runtime
and resource requirements for each region, and limits on additional capacity.
It compares the plan with ready deployments in its service discovery.

For each missing regional replica, Orchestrator asks Cloud for capacity **in that
region**. Cloud filters hosts by eligibility and resource fit, reserves resources,
and provisions through heyvm. It reports the resulting deployment back; Orchestrator
publishes it as serving capacity only after readiness and dependency checks.

Cloud owns host selection and reservations because all VM allocations must compete
against the same resource inventory. Orchestrator owns regional coverage because
only it knows that a second US replica cannot replace a required EU replica.
Insufficient EU capacity leaves the plan visibly unsatisfied; it does not change
the requested topology. Hostnames are inventory, not placement policy.

### Decision B: how much traffic should each region receive?

Orchestrator computes distribution **per service**, not per host. It uses three
inputs with different meanings:

| Input | Source | Meaning |
| --- | --- | --- |
| Ready service instances and configured serving budgets | Orchestrator discovery and service plan | Capacity actually available for this service |
| Active work, queueing, latency, errors and rejections | Destination app-lb | Whether that service is approaching its serving limit |
| Available resources and host pressure | heyvm through Cloud | Whether co-located workloads constrain that capacity or another replica can fit |

Begin with explicit weights. Once feedback control is introduced, derive the
capacity-balanced target from each region's usable serving budget, excluding
unready/draining capacity and retaining configured headroom. Apply bounded changes
toward that target rather than chasing every sample. Missing reports freeze automatic
increases; they do not mean the region is idle. Local health and admission limits
protect the service between controller updates.

Do not use “remaining idle request slots” alone as the weight: directing traffic to
an idle region would immediately make it look less attractive and cause oscillation.
Use a stable tested capacity baseline, sustained pressure to adjust it, and a slower
replica-scaling loop. When both regions are saturated, add ready capacity if possible
or reject excess work; moving the same overload between regions does not solve it.

### Worked example: eu1 has 100 VMs; us3 has 2

1. The Cloud service plan requires EU >= 1 and US >= 1. Orchestrator requests one
   replica in each region. The global VM-count difference does not change this.
2. Cloud checks actual reserved/available resources in each region. If eu1 cannot
   fit its replica, the EU requirement remains blocked. If it can, both replicas start.
3. Suppose measured safe Cloud-serving budgets, after headroom, are 40 concurrent
   requests in EU and 120 in US. A capacity-balanced policy targets 25% EU / 75% US.
   These are illustrative service budgets, not measurements of the current hosts.
4. If both Cloud instances instead have the same usable budget, that policy targets
   50% / 50%, despite the 100-versus-2 VM count. A locality-preferred policy is a
   separate explicit choice, not an undocumented override of those weights.
5. More replicas may fit in US, but adding them cannot erase EU's minimum. Before
   EU maintenance, US must demonstrate enough capacity for the entire affected
   service demand; simply changing its weight to 100% is insufficient.

### Decision C: where does this request go?

```diagram
Client → stage.heyo.computer → eu1 app-lb
                                   │
                         Assigned regional selection
                            ┌──────┴──────┐
                            ▼             ▼
                      EU instance    us3.heyo.computer
                                          │
                                      us3 app-lb
                                      Local-only selection
                                          │
                                          ▼
                                      US instance
```

app-lb reads the latest valid assignment from memory. It selects a region, then
either a local instance or that region's authenticated gateway. The remote gateway
selects only a local instance; it cannot forward the same request back across regions.
Both entry points use the same authoritative policy. Regional forwarding does not
require exposing each VM's private address across servers.

The ingress records offered service demand; the destination records execution load.
These are not added together as two requests. Existing streams stay on their selected
instance until completion or an explicitly defined termination policy. Changing
weights changes new assignments, not the location of existing work.

### Discovery and control-plane availability

Cloud's registry answers “which hosts can run this VM?” Orchestrator's discovery
answers “which ready instances serve this application?” Service discovery is updated
by deployment/readiness reconciliation; app-lb observations supplement it but cannot
create an authoritative deployment by reporting an arbitrary endpoint.

Gateways use local snapshots during control-plane outages, subject to health and
admission checks. New global policy, placement and maintenance decisions stop when
their authority is unavailable. Replacing Cloud or Orchestrator itself uses the
existing healthy instance to create its replacement, then verifies the replacement
before removing the old instance. Shared durable state and exclusive operation
ownership are prerequisites to active replicas, not consequences of adding a gateway.

## First implementation change: Orchestrator's regional service plan

**Extend the existing regional replica placement and discovery into one reconciled
service plan.** This is the first implementation slice, not the whole Phase 1 and
not an app-lb forwarding patch.

| Part | Concrete change |
| --- | --- |
| Desired state | Accept the concise service JSON, expand its regional intent into the existing `replicaRegions` placement representation, and persist regional runtime/resource requirements; operators do not maintain both forms |
| Reconciliation | Compare ready and pending replicas against those slots; request only missing capacity in the required region through Cloud; use existing rolling-deployment ownership for retries |
| Observed state | Derive a region-grouped view from existing service discovery; show missing capacity separately from ready capacity |
| Routing output | Compile explicit operator weights and eligible regional membership into one versioned decision; never route to merely planned capacity |
| Incomplete topology | Preserve the coverage failure visibly. Traffic can use other ready regions only when the service's explicit failover policy allows it; do not silently renormalize a missing required region |
| Compatibility | Existing single-region deployments keep their behavior; nothing consumes the new regional decision until explicitly enabled |

Primary owning modules are Orchestrator's existing service deployment and discovery
modules. The first slice adds no automatic capacity balancing, no app-lb VM creation,
and no runtime update. Its tests prove that EU/US intent cannot produce two US
placements, repeated reconciliation does not duplicate pending replicas, and only
ready endpoints appear in routing output. Runtime/profile selection must support
EU libvirt and US Firecracker without assuming one global driver/image.

The next slice makes app-lb consume this shared regional decision and implements
the cross-region request path. Phase 2 then changes how weights and extra replicas
are calculated, without replacing the ownership model. The JSON fragment specifies
the proposed authoring experience, not a complete wire schema. Detailed schema and
endpoint design follows agreement on these boundaries.

## Scope and deployment constraints

- Initial staging topology: eu1 and us3. Production/us1 is out of scope.
- Keep `stage.heyo.computer` entering through eu1; no DNS change is assumed.
- Use the existing us3 gateway installation and `us3.heyo.computer` address,
  subject to live TLS, authentication, and reachability verification.
- eu1 retains its existing libvirt workloads; us3 uses Firecracker. Routing
  consumes service endpoints, not hypervisor-specific VM addresses.
- Five database VMs remain on us3. Application evacuation does not evacuate,
  restart, or migrate databases. Runtime updates must independently prove that
  these VMs and their network paths remain uninterrupted.
- Reuse existing installations. This proposal authorizes no infrastructure
  changes, deployments, cleanup, or database writes.

Two hosts can support planned application maintenance if either has sufficient
capacity. They do not provide complete host-failure resilience: eu1 remains the
public ingress dependency and us3 remains the shared database dependency.

## Existing foundations and gaps

Verified against the repository when preparing this proposal:

| Existing source | Reuse | Missing capability |
| --- | --- | --- |
| [Service deployment](../orchestrator/src/handlers/service_deploy.rs) | `desiredReplicas`, `replicaRegions`, region-preserving replacement, rollout ownership | Continuous regional minimum/capacity reconciliation and a coordinated regional maintenance barrier |
| [Service discovery](../orchestrator/src/handlers/service_discovery.rs) | PostgreSQL-backed endpoint sets, versions, region, health and draining | Gateway registration, coherent regional assignments and consumer observations |
| [app-lb discovery](../app-lb/src/discovery.rs) | Polling, version comparison, retaining the last good upstream set | Parser drops endpoint region; upstream conversion accepts only plaintext, pathless HTTP with explicit port |
| [app-lb registry](../app-lb/src/registry.rs) | Atomic local snapshots and local JSON persistence | Local files are not an authoritative shared routing store |
| [app-lb selection](../app-lb/src/deployment.rs) | Least-in-flight local backend selection | Regional selection and coordinated load feedback |

Private companion repository integration: `cloud/src/repositories/mvm_ctrl_backend_server_repository.rs`
currently filters placement by region, driver, environment/pool, physical identity
and memory fit, then orders candidates by heartbeat recency. This is not a
resource-load scoring algorithm. Cloud and heyvm changes require companion PRs;
their implementation is not part of this public documentation PR. Source
availability does not establish which versions are deployed.

## Ownership

| Component | Authoritative responsibility | Must not own |
| --- | --- | --- |
| heyvm | Local VM execution, runtime capabilities, host resource/VM observations | Global service weights or regional replica policy |
| Cloud | Infrastructure registry, constrained host allocation, atomic reservations and provisioning | Global application traffic policy |
| Orchestrator | Desired regional service capacity, deployment reconciliation, service discovery, traffic assignments and maintenance workflow | Per-request routing or duplicate host resource accounting |
| app-lb | Local routing snapshot, regional forwarding, local selection/admission, measured load and drain reports | VM placement or independent global rebalancing |

There are two discovery roles: Cloud discovers infrastructure; Orchestrator
discovers service endpoints. app-lb consumes the latter. Gateways do not replicate
their local JSON stores to one another. Multiple Cloud/Orchestrator processes
coordinate through durable operation ownership, not process-local locks alone.

## Consistency and security boundaries

Reuse existing service/deployment identities and authorization boundaries. Keep
tenant/project and environment isolation. Node, region and gateway identities are
distinct. These are behavioral requirements, not a proposed schema or endpoint list.

- Extend Orchestrator's existing service discovery ownership while preserving legacy
  consumers. Introduce regional decisions and authenticated observations only through
  explicit opt-in; do not make old app-lbs guess the meaning of regional membership.
- Policy mutations carry an expected generation; conflicting writes fail rather
  than overwrite a newer decision. Publish a complete routing snapshot atomically,
  referencing compatible discovery membership. Do not expose half-updated weights
  and endpoints. A rollback is a new generation, never a decreasing version.
- An applied-generation report means the complete snapshot is validated and active,
  not merely fetched. Include boot identity so a pre-restart ACK cannot authorize
  maintenance. Restarted gateways must obtain authorization/current policy before
  rejoining serving membership.
- Report freshness uses server receive time plus bounded observation age; reject
  replayed sequences and implausible windows. Deduplicate forwarded requests when
  aggregating ingress demand versus destination service work.
- Reporters may update only their own authenticated records. Policy changes require
  deployment-management authority. Secrets/certificates use managed service secret
  configuration, not tokens embedded in discovery URLs or committed examples.
- Durable controller leases include fencing generations. A stale worker cannot
  publish policy or execute a runtime update after losing ownership. Cloud allocation
  and provisioning retries use the same idempotency key and reservation.

## Phase 1: region-aware serving

Use explicit, operator-controlled weights initially. No automatic load algorithm
is required to establish the routing contract.

1. Orchestrator publishes healthy regional service endpoints and reachable gateways.
2. The ingress app-lb selects an eligible region using the snapshot's weights.
3. For a local destination it selects a healthy local instance. For a remote
   destination it uses that region's authenticated HTTPS gateway.
4. A forwarded request is local-only at the destination; it cannot select another
   region. If no local instance is eligible, return an explicit unavailable response.

Use mTLS gateway identities and validate destination/service scope. Strip
client-supplied internal routing metadata. Preserve original application host,
path, query and streaming semantics while TLS uses the gateway destination name.
Do not blindly replay non-idempotent requests after application delivery may have
occurred. Health alone never overrides a maintenance exclusion. Any fallback must
stay within the published eligible destinations and retry budget.

Phase 1 acceptance: one real service in both regions; verified local/remote
routing under explicit weights; authenticated forwarding; no forwarding loops;
streaming/WebSocket coverage; malformed or incompatible snapshots rejected;
old generations rejected; restart and disconnect behavior exercised. Remote HTTPS
forwarding alone does not complete this phase.

## Phase 2: capacity placement and traffic distribution

### Placement loop

Require, for example, Cloud EU >= 1 and US >= 1. Resolve those requirements before
scoring hosts. An empty US host cannot satisfy missing EU capacity. Reuse regional
replica slots and rolling replacement; apply runtime/image requirements per region
without assuming the current single-driver request already expresses mixed runtimes.

Cloud filters hard constraints, then evaluates resource fit with atomic reservations
including in-progress allocations. Protect platform headroom from general sandbox
allocation. Score remaining hosts by resource pressure and fit, not total VM count.
Failed allocations release reservations through reconciled terminal state; a timed-out
provisioning response is not proof that the VM does not exist.

### Traffic loop

One fenced controller per service publishes weights. Use configured, tested
per-instance serving budgets initially, reduced by readiness, fresh observed service
pressure and host pressure. Separate CPU/memory feasibility from service throughput.
Do not infer spare serving capacity from idle VM count or missing telemetry.

For a capacity-balanced policy, normalize usable regional serving budgets into
weights, then apply explicit locality, canary and failover-headroom constraints.
This is distinct from a local-preferred policy, which spills only according to its
configured overflow rules. Do not silently switch between these policies.

Smooth observations over bounded windows; limit weight-change rate and use a
deadband/cooldown. Scale replicas on a slower loop than traffic weights, and send
traffic only to ready capacity. With stale telemetry, freeze automatic increases
and flag degraded control; local admission limits remain active. Use bounded queues
and explicit overload responses when all eligible capacity is exhausted.

Existing app-lb-managed VM autoscaling must not also scale Orchestrator-owned
regional services. Choose one lifecycle owner per deployment; preserve local managed
pools for legacy deployments. Extra regional replicas may favor spare US capacity,
but never erase the EU minimum.

Phase 2 acceptance: a 100-VM EU/2-VM US scenario preserves regional minimums;
concurrent allocations cannot overcommit reservations; insufficient regional capacity
is explicit; telemetry loss is not zero load; saturation remains bounded; workload
changes converge without oscillation; controller failover cannot duplicate decisions.

## Phase 3: regional maintenance

Persist this workflow and its target application/dependency inventory:

`preflight → cordon placement → prepare alternate capacity → publish evacuation → verify drain → update → verify recovery → restore gradually`

- Preflight checks every affected application service, revision compatibility,
  dependency access and alternate capacity. Include scheduled/background application
  work: draining HTTP ingress alone does not move workers or internal callers that
  bypass app-lb. Such work needs an owner-specific quiesce/transfer gate; otherwise
  the workflow is blocked and must not claim all application traffic has moved.
- Cordon prevents new application placement on the target while leaving protected
  database VMs untouched. Preserve ownership of pre-existing maintenance exclusions.
- Snapshot the set of all gateways/callers capable of sending affected traffic.
  New members must join at the current evacuation generation. Require fresh applied
  reports from the current boot of every required gateway.
- Observe no new target work and completion of existing requests/streams over an
  agreed quiet window. Connection/session deadlines must be explicit; elapsed time
  alone never turns unknown state into successful drain.
- An unreachable gateway must be positively fenced from sending traffic, or block
  maintenance. Heartbeat expiry is not fencing. This plan does not require implementing
  network fencing to unblock maintenance: initially, block safely.
- Runtime updates execute with durable operation identity and fencing. Prove database
  VM/network continuity and eu1 ingress survival before authorizing the update.
- On failure, persist a blocked/failed state. Before the update, rollback may restore
  verified healthy capacity through a new policy generation. After an uncertain update,
  do not restore traffic until runtime/service health is established. Restore only
  exclusions/cordons owned by this operation, not another operator's maintenance.

Phase 3 acceptance: coordinator restart at every boundary, stale/restarted gateway,
long-lived stream, alternate-region saturation, runtime update failure, background
worker gate and database continuity tests. A successful policy API call or ACK is
not sufficient evidence of drain.

## Phase 4: failure resilience

Exercise control-plane loss, stale snapshots, partitions, total regional loss,
gateway restart and gradual recovery. Routers may continue serving last-valid
assignments subject to local health/admission, but cannot invent new global policy.
Unknown control state blocks destructive maintenance and new capacity assumptions.
Cold start without an authorized snapshot fails closed for multi-region deployments.

The current topology cannot survive complete eu1 loss for the staging hostname,
or complete us3 database loss. External ingress/DNS failover and database availability
are separately reviewed projects, not promises delivered by this routing protocol.

Phase 4 acceptance: documented and exercised behavior for each failure above,
including overload when remaining capacity is inadequate. Availability claims must
identify ingress, database and control-plane dependencies explicitly.

## Compatibility, delivery and review gates

- This PR changes documentation only. It supersedes earlier phase-completion claims,
  not existing running configurations.
- Single-region services and app-lb local managed pools keep current behavior unless
  explicitly opted in. Existing host aliases remain untouched.
- Additive discovery fields alone are insufficient for safety: old consumers can
  ignore them. Require protocol capability registration before admitting a gateway
  to a multi-region service or its maintenance barrier. Legacy consumers cannot
  remain an untracked path to that service during evacuation.
- Implement public Orchestrator/app-lb contracts and private Cloud/heyvm integration
  in separate, linked PRs. Database changes need additive migrations and rollback
  compatibility before activation; this document does not perform those migrations.
- Phase 1 freezes wire schemas and validates forwarding/TLS against an isolated
  service. Phase 2 establishes measured serving budgets, report freshness limits,
  reserve policy and tuning values through load tests rather than guessed defaults.
- Before Phase 3, inventory ingress processes, background workers and runtime update
  behavior. Any unsupported path becomes an explicit blocker, not an omitted workload.
- Review Phase 0 ownership and safety contracts before implementation. Advance each
  later phase only with its acceptance evidence; healthy checks on two servers are
  not a substitute for coordinated multi-region behavior.
