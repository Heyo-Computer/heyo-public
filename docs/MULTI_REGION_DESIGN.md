# Multi-region serving: Phase 0 design

Status: proposed; documentation only. This document defines the replacement
rollout plan for coordinated multi-region application serving. It does not claim
that installing gateways or registering two backends completes any serving phase.

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

## Shared contracts

Names below are proposed logical contracts, not existing wire APIs. Reuse existing
service/deployment IDs and authorization boundaries. All records are scoped by
tenant/project and environment. Node IDs, region IDs and gateway IDs are distinct;
eu1/us3 are inventory entries, never branches in routing code.

| Contract | Minimum fields and semantics |
| --- | --- |
| Backend observation (heyvm → Cloud) | Node/region, runtime capabilities, total/allocatable/reserved resources, available memory, CPU and I/O pressure, observation sequence and timestamp |
| Regional service intent (operator → Orchestrator) | Service/revision, per-region replica minimum/maximum, resource request, pool/runtime constraints, dependencies, serving budget and failover headroom policy |
| Allocation (Orchestrator → Cloud) | Stable idempotency key, service replica slot, region, resources, exclusions; result includes reservation/deployment/node identity or explicit insufficient-capacity error |
| Service membership (Orchestrator) | Endpoint/deployment/node/region/revision, local URL, readiness observation and drain state; existence alone is not readiness |
| Gateway registration (gateway → Orchestrator) | Gateway ID, region, HTTPS address, authenticated identity, boot ID, supported protocol version; registered addresses are validated against deployment/network policy |
| Routing snapshot (Orchestrator → app-lb) | Service, generation, schema version, discovery version, gateway directory, eligible regional weights, permitted failover order and maintenance exclusions |
| Gateway observation (app-lb → Orchestrator) | Gateway/boot ID, report sequence/window, applied generation, per-service/region request counts, latency/error/queue statistics, active requests/streams and drain observations |

### API and consistency rules

- Extend the existing authenticated service discovery path with a versioned
  regional representation; retain its legacy representation. Add authenticated
  gateway registration/reporting and traffic-policy update operations under
  Orchestrator ownership. Freeze exact routes and serialization in Phase 1 tests.
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
