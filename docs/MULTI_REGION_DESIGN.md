# Two-region platform: routing, rollout and recovery

Status: implementation target; two-region live acceptance is incomplete. The
decisions and checklist below supersede conflicting next-step statements in the
historical checkpoints. Existing capabilities are identified separately from new
work. This document does not itself change running infrastructure.

## Unified control-plane checkpoint — 2026-09-23

- Live follow-through at 12:34 UTC: [release 3d](https://ci.eu1.heyo.work/runs/01a0ce2b02c1-0000003d)
  validated and published `4b06bf0107821f1a28cea03e3f11703d3389215e` and verified
  both regional app-lb replacements. Both public admin APIs now serve
  `/control-plane/config`, revision 0 with empty bindings: installed, not configured.
  The CI controller operation
  `ci-controller-016fe500b525de0986cecb3dac6adfa343372a75a48389d84dc9906ae8736e93`
  remains draining, waiting for verified cleanup of its job VM `sb-109738e8`.
  Internal read-only inspection found CI PID 421 holding 1018 descriptors with
  soft/hard limits 1024/4096, while its log reports `Too many open files` and
  failing Iroh connections. No cleanup record or maintenance fence was cleared.
  No approved us3 cache has been deleted. Orchestrator replacements remain pending.
  The subsequent 20 GiB CI-cache workflow change is pushed in PR108 but its
  [release 41](https://ci.eu1.heyo.work/runs/01a0ce33a204-00000041) failed validation
  while polling an exec operation (`Missing API key`); it was not published.
  Earlier validation evidence was not substituted for that failed revision.
- Approved runtime recovery: raised only CI PID 421's soft descriptor limit
  from 1024 to its existing hard limit 4096, without restart or persistent
  configuration changes. Inspection counted 1015 sockets, including 919 TCP
  CLOSE-WAIT sockets clustered on old loopback tunnel ports. Exact published
  heyo-sdk 0.1.9 source retains detached forwarding tasks and sends QUIC FIN only
  after joining both copy directions; this is a concrete half-close retention
  path consistent with the live sockets, not normal capacity demand. No SDK
  fix or package release has been made. The count subsequently reached 1034;
  the controller still reported the same cleanup wait after the outstanding
  validation's scheduled retry time. Headroom alone has not completed recovery.
- Gary's chosen model is one logical control plane accessible through either
  region, with a generic DNS name that can switch regions. App-lb is the UI/API
  entry point; Retail is not the implementation surface. Regional entry points
  must show shared application state, not reinterpret their local registries as
  separate global inventories.
- Delivery status at 06:36 UTC: [PR 107](https://github.com/Heyo-Computer/heyo-public/pull/107)
  merged through [release 37](https://ci.eu1.heyo.work/runs/01a0cce793b9-00000037).
  Both Linux validations passed. us3 app-lb is publicly healthy at the new
  revision, but us3 Orchestrator replacement failed; eu1 was skipped. Both
  Orchestrators and eu1 app-lb still serve the previous revision. The first
  submission failed during a CI daemon-tunnel timeout before merge; no failed
  validation evidence was substituted into the fresh submission.
- Concrete deployment blocker: operation
  `ci-service-9e027fc01defe58b9324fb6be666d1d1a79021c67ff008b37e475afe63424171`
  allocated `sb-eb09681b`, but us3 heyvm rejected its creation because `heyo-net`
  has no usable `/30` TAP subnet in `10.88.0.0/24`. Read-only host receipts
  `job-988efb8e0678` and `job-b327fac1f855` show the exact failure and 63 distinct
  persisted allocations under `/var/lib/heyvm/run`; the gateway excludes the
  remaining block. The operation failed in verification, retained predecessor
  `sb-8ae305f1`, and performed no cutover. No network records or VMs were manually
  deleted. A new release alone cannot fix capacity. Changing the global network
  also changes allocation behavior for stopped VMs on restart, so blindly
  enlarging/replacing the existing network is not a verified safe remedy.
- Cleanup unblocker, not deployed: CI now has a local repository-bearer route
  `POST /api/runs/{run_id}/cache/{sandbox_id}/destroy`. Its atomic pool update
  checks idle status, served runner and latest terminal owning job before
  recording durable eviction; it reuses daemon-confirmed deletion. Targeted
  PostgreSQL and HTTP tests passed for claim exclusion, reuse revoking an old
  owner's authority, wrong repository/token and read-HMAC denial. The existing
  rerun integration test still fails before dispatch because its legacy tar.gz
  fixture is rejected; cleanup auth is tested separately without relaxing that
  source-format fence.
  A fresh CI inventory read still shows `sb-0ef175a4` idle on us3, last used by
  successful run `01a0cbf90bfe-0000000d`; that run is readable with this
  checkout's repository bearer. No cache was deleted and no capacity reclaimed.
  Publishing this additional CI change and obtaining a deployed authenticated
  cleanup path remain delivery gates; local test results do not clear them.
- Shared inventory implementation (not yet deployed in Orchestrator):
  Orchestrator now exposes a paginated, internal-key-gated
  shared-database inventory. App-lb's global application view consumes it using
  server-side secret references and read-only regional API fallback. It does not
  replay mutations, merge independent databases, or fall back to local files.
  Independent regional gateway counters remain separately labelled observations.
- Follow-up configuration API is locally verified: fleet-admin GET/PUT
  `/control-plane/config` persists conditional, secret-reference-only bindings
  and updates read snapshots without changing host supervision. Explicit startup
  files retain precedence and refuse API writes. This closes the gap where an
  unchanged-configuration binary rollout could not enable the view. Tests cover
  concurrent/stale writes, persistence/restart, invalid origins, unavailable
  credentials, failed writes and scoped authorization. A compiled local HTTP
  check exercised anonymous denial with both optional gates disabled, successful
  configuration, stale/invalid rejection and restart persistence. This API is
  not yet deployed or configured on either regional gateway.
- Verification: the new PostgreSQL inventory test ran against an isolated schema
  on disposable `heyo-policy-proposals-test`, using two independent connections.
  It covered identical reads, 100-row pagination, region drains, revision identity,
  missing discovery and exclusion of secret-bearing metadata. App-lb fleet tests
  passed, including failed-region fallback versus denied credentials; all 40
  existing admin gate tests passed with the new routes included in scope checks.
- A compiled disposable local app-lb successfully queried both installed public
  HTTPS gateway metrics endpoints. Anonymous `/fleet` and `/services` requests
  returned 401 even with the local dashboard configured public. No remote writes
  were used for this check. Browser checks rendered and inspected desktop/mobile
  views, desired-versus-recorded replica mismatch, and unavailable states that
  clear old values. Application inventory in the browser check was fixture data;
  gateway observations were live. This is not deployed control-plane acceptance.
- Delivery path: the existing `.ci/workflows/regional-release.yml`
  merges the public release before replacing us3/eu1 app-lb and Orchestrator.
  `ci/rollout-host-app-lb` requires the exact confirmed merged revision. Gary
  explicitly approved a new branch push, PR, and merge through that CICD release
  workflow for this change. Release completion still requires deployed evidence;
  approval and local verification are not deployment receipts.
- Remaining platform gates: configure the same authority and canonical operator
  identity in both entry points, install a generic hostname/certificate path,
  verify auth/HeyoSecret/database dependency survival, and run regional-loss and
  writer-recovery checks. pg-fc has replication and fenced-promotion primitives,
  but these are not evidence of a configured automatic regional writer handoff.
  The CI maintenance fence remains intact. Global mutations and full two-region
  application routing/drain acceptance are still incomplete.

## Live deployment checkpoint — 2026-09-23

- Public [release](https://ci.eu1.heyo.work/runs/01a0cc3e2c91-00000021)
  completed; [PR 106](https://github.com/Heyo-Computer/heyo-public/pull/106)
  merged. Both regional Orchestrator `/health` and app-lb admin `/healthz`
  endpoints return HTTP 200 and revision `c2b3f55e923c95070ede7d606960be960f336c39`.
- Private [release](https://ci.eu1.heyo.work/runs/01a0cc4c8eac-0000002c)
  passed its required validations and merged
  [PR 617](https://github.com/Heyo-Computer/heyo/pull/617). Both Cloud deployment
  receipts confirm the exact candidate healthy and predecessor stopped. Both
  `https://cloud.{us3,eu1}.heyo.work/health` endpoints return HTTP 200 and revision
  `87786b295ccb0561ad5dee85d17af08048b2e71c`.
- Host runtime deployment uses us3 CI maintenance operation
  `36fce8c5ce851b3276aec416ec0b745fdcf2d0324526d20b18b0b52ee3ee494c`,
  now in CI phase `failed` after its deadline. Read-only database receipts `job-152eaa7dd82a` and
  `job-b9cc3a8be671` identify five retained attempt-1 `ci_host_work` rows:
  `01a0cbf90bf3-0000000b.release`, `01a0cc3e0737-0000001b.build`,
  `01a0cc3e0739-0000001c.build`, `01a0cc3e2c8b-0000001f.release`, and
  `01a0cc48ece7-00000028.build`. All corresponding jobs finished attempt 2;
  their current VM pool entries were idle with no claimant or lease. Historical
  events (`job-bc04275e6784`) identify four first-attempt start timeouts for the
  same stopped VMs and one credential-resolution failure before VM acquisition.
  After Gary's explicit approval, `job-5f5346ae013d` removed exactly those five
  attempt-1 claims under the runner lock with terminal-job, attempt, pool-lease,
  and host-process guards. No VM or application data was deleted. The existing
  release advanced automatically from CI drain to the Cloud host upgrade.
- us3 host upgrade completed at 04:14 UTC, but Cloud lost the POST response during
  restart and retained `status=maintenance` with an unknown-outcome error.
  `job-05aaebcb84cc` confirms the helper exited successfully and the service
  restarted. Authenticated host status (`job-486205ed299e`) confirms target `us3`,
  `systemdActive=true`, version `0.50.4`, and both installed/running executable
  hashes equal the admitted artifact hash
  `b4522bf593e7065005de63cb805714c9b970ccc65006a887c073cf6344ad18b1`.
  After Gary approved Cloud reconciliation, `job-e57606606bc7` rechecked those
  hashes and atomically completed the exact operation and restored us3 scheduling.
  The live database binding is `orchestrator-us3/database-url`, not the legacy
  `cloud/database-url` or `orchestrator/database-url`. Identity preflight receipt:
  `job-229a1e9f2cee`. Completion committed at 04:37:52 UTC, after CI's 04:35:02
  deadline. CI still retains its independent failed-maintenance runner fence;
  its worker excludes failed operations and has no reconciliation endpoint.
  Gary directed investigation rather than clearing this fence; it remains intact.
  eu1 heyvm and both heyvmd jobs were skipped, not deployed. No second
  host upgrade request was sent, and the original failed run remains unchanged.
- Root-cause investigation: CI `claim_job` inserts attempt-scoped host work before
  source preparation and VM acquisition. Early errors return without reaching VM
  cleanup; a subsequent attempt's cleanup does not reconcile its predecessor.
  Historical events identify four VM-start timeouts and one pre-VM credential
  failure. This explains the retained claims, but not the original start timeouts.
  Host journal receipt `job-8be2e58c1c8c` records helper start at 04:14:15.321555,
  API stop at 04:14:15.333598, new listener at 04:14:17.410330, and helper success
  at 04:14:18.401658 UTC. The handler launches `systemd-run` before returning JSON;
  the helper immediately restarts the same API service. This permits response
  loss during self-restart and matches the observed transport failure; no packet
  capture establishes the precise response-byte boundary. Cloud's error path
  skips `verify_host_heyvm`, and its worker excludes `status=maintenance`, leaving
  no read-only recovery after uncertain delivery. CI excludes `phase=failed`
  after its deadline, while runner admission blocks every phase except `passed`.
  Recovery needs operation-bound host receipts and verification without replay,
  plus explicit late-outcome reconciliation preserving failed-run history—not
  unconditional claim deletion, a longer timeout, or a second upgrade POST.
- The eu1 app-lb update briefly caused CI/HeyoSecret 502s before recovery.
  These releases prove installed code, not zero-downtime maintenance, activated
  hierarchical routing, a unified control panel, or any unchecked live acceptance
  gate below. The authenticated CI Networks page does list both regional runners;
  it is a CI view, not proof of a unified platform control plane.

## Implementation contract — 2026-09-22

Build one platform across us3 and eu1, then deploy CI as an application on it.
Orchestrator owns desired state and durable rollout progress; Cloud allocates hosts;
heyvm owns local VM execution; app-lb routes requests. A gateway never acquires VM
lifecycle ownership by learning a route. Preserve the existing canonical credential
per service role across regions; region identifiers are placement, not identities
requiring new passwords.

### Up comparison: refinements, not a new coordinator

The reviewed Up/Coconut source compiles desired configuration and observed jobs
into dependent plan items, stores the plan/rollback data separately from item
progress, and executes through a distinct deployment controller. Its regional-drain
activity calls AutoTConfig; that successful write alone does not attest zero
in-flight requests. Its rollout-storage clients are selected by regional workflow
domain, so logical ownership does not imply one physical global database.

For Heyo, retain shared PostgreSQL and the existing fixed regional plan. Compile
and persist intent before execution; keep publish, applied-generation observation,
and confirmed drain as separate items. Verify exclusive operation ownership across
both controllers. Do not add Cadence, a general DAG scheduler, or regional database
sharding for this milestone. Gateway-to-gateway forwarding is our independently
tested data-plane choice, not a topology proven by the Up source.

Policy activation uses an immutable proposal owned by a persisted operation/plan
item, plus one active-policy reference. Do not add independently mutable prepared
and active policy slots. The existing `regional_policy` write API stores draft
intent only; it is not an activation or drain acknowledgement. Compile that intent
into an immutable proposal before a routing transition. Weight-only changes use
the same operation-owned transition without creating VMs.

Keep three meanings distinct: schema version (`RegionalPolicy.version`), immutable
policy generation, and discovery snapshot revision. Membership updates may advance
snapshot revision under the same policy generation. Preparation ACKs name the exact
proposal; activation CAS names its expected predecessor, not a changing membership
revision. Commit activation and its plan progress atomically. After restart, an
already-active proposal is reconciled, not allocated or activated again.

Application plan v3 keeps the same policy phase names but reserves separate
occurrence coordinates: slots 0/1 are forward withdrawal/restoration, and 2/3 are
rollback withdrawal/restoration. Candidate ordinals are independent phase IDs.
Rollback enters the current region's persisted `rollback_entry`, restores that
region first, then walks earlier regions in reverse order. Final verification
retains the last real region's coordinate. All entered regions need retained
baseline identities pinned before execution, including failure during preflight.
Application withdrawal/restoration policies derive from an immutable admitted
`regionalPolicy` baseline, not the current draft. Each entered region must have an
explicit positive restore weight; admission cannot invent one for a zero-weight
region. Only the current target may differ from that baseline while withdrawn.
Changing another region's weight or any gateway binding blocks publication rather
than silently restoring or reassigning it. Forward and rollback use new generations
of the same pinned policy through the existing preparation/adoption/drain barriers.
Only that explicit entry jump may cross from the forward to rollback program;
the interrupted forward item is not completed, and subsequent rollback items
must follow exact dependencies. V1/v2 journals retain their existing semantics.

The ordered items are publish proposal → wait target preparation → activate source
policy → wait every source's adoption → wait outgoing assignments zero across all
retained generations → close target peer admission → wait target admission ACK and
local work zero. Pin participant identities/boot IDs; persist report sequence
high-water marks and require fresh server-observed evidence. A restarted process's
zero counters do not prove its predecessor drained. Do not silently remove missing
participants. Targets retain explicitly authorized predecessor generations until
source assignments finish, and each admitted request pins its policy and counter
guard through response/stream completion. Snapshot replacement cannot erase either.

### Route to a region before selecting a VM

```diagram
Application hostname
        │
        ▼
Reachable regional ingress → local app-lb
                                  │
                     authoritative region selection
                         ┌────────┴─────────┐
                         ▼                  ▼
                    local VM       destination app-lb
                                     local-only route
                                            │
                                            ▼
                                      destination VM
```

- Public traffic enters the existing host ingress and app-lb. A service's versioned
  routing policy selects a serving region using explicit weights initially.
- If local, select a healthy accepting local backend. If remote, select a ready
  gateway in that region and forward once. A remote gateway selects only its local
  backends; it never performs another regional selection. No backend means an
  explicit unavailable response, not a bounce to the source.
- The first gateway transport is HTTPS through the existing regional ingress on
  port 443, with a dedicated peer-only route into app-lb. Validate TLS against the
  gateway hostname, independently of the original application Host. Prove this
  route with the retained fixture before activating new discovery consumers.
- Authenticate the peer route using a shared gateway-forwarding service-role
  credential delivered through HeyoSecret. Reuse an existing credential only if
  it belongs to that role; do not reuse an operator/admin token for data traffic.
  References, not credential values, belong in routing configuration. Validate
  service/environment/destination scope and strip external routing metadata.
  Preserve the application's Authorization header separately from peer auth.
- SDN/Iroh port sharing is the fallback transport if regional HTTPS cannot carry
  this traffic. Expose one gateway listener per instance, not a tunnel for every
  remote VM. Ticket renewal, reconnect and readiness would belong to the gateway
  transport adapter; do not silently activate an untested fallback.
- Preserve method, Host, path/query, request/response streaming and WebSocket
  behavior. Never replay a request after its delivery may have begun. During
  transport failure, fail the request explicitly; do not invent a new regional
  policy or retry a non-idempotent application action.

An app-lb on today's single-host region knows local VMs plus remote gateways. With
more hosts, a regional gateway tier may route within its region or through host-local
gateways. Neither case requires every gateway to ingest every VM globally.

### Publish regional policy, not a global VM list to every gateway

Extend existing Orchestrator discovery with an explicit opt-in protocol version;
legacy flattened discovery remains unchanged for legacy deployments. The regional
view contains service/environment identity, policy generation, eligible regions and
weights, and gateway identities/transport addresses. A region-scoped backend view
contains only that region's deployment IDs, addresses, revision and readiness.
Both views reference one committed generation; consumers stage and atomically apply
a complete compatible pair, never mixing old membership with new policy.

Orchestrator keeps the full placement inventory. Consumers report instance ID,
region, boot ID, applied generation, report sequence and admission/drain counters.
Scope discovery reads to the configured service/region. Peer forwarding carries the
service, destination region and policy generation on the authenticated peer route.
A target must prepare and acknowledge a generation before sources may send new
assignments under it. A target behind the sender's generation rejects rather than
guessing policy. Current hard exclusions override older forwarded assignments. A
peer marker from a public client never grants local-only routing access.

Planned exclusion is staged: first all source selectors, including the target's
public ingress, stop choosing the target region and acknowledge that generation.
The target temporarily finishes assignments already admitted by those sources.
After all sources report zero outstanding target assignments, close the target's
peer admission and confirm zero local work. This avoids rejecting requests merely
because snapshots arrive in different orders. "Withdrawn" means the barrier has
completed, not that the first policy write succeeded. Existing request guards remain
alive until response completion or stream closure. Report separately:
ingress requests, outgoing peer requests, and local backend requests. They are
different drain gates, not three units of application demand. A regional drain
requires fresh current-boot ACKs from all admitted gateways plus zero relevant
source-peer and destination-backend requests. An unresponsive gateway blocks the
operation unless positively fenced; time elapsed is not proof of drain.

Health checks must exercise these same paths. A destination gateway probes its
local VM; the controller verifies the destination's local-only peer route and both
public ingress routes. Controller-to-remote-VM host-port access is not a prerequisite
and must not remain the candidate-readiness path for this protocol. An authenticated
candidate probe targets an exact deployment without admitting public traffic to an
excluded region; a healthy predecessor cannot satisfy the candidate's revision check.

### Persist three distinct maintenance operations

All operations pin target identities, source/candidate revisions, policy generations,
observer boot IDs and owned exclusions before execution. Reuse durable Orchestrator
plan items and fencing; no new independent rollout coordinator. Restart reconciles
the exact candidate and switch receipt before retrying an effect. Unknown state
blocks progress rather than creating a second candidate.

| Operation | Ordered gates | Recovery |
| --- | --- | --- |
| Application revision | Prove alternate capacity/dependencies → exclude target region for this service → ACK and drain → create immutable candidates → local and peer-path health → restore through new generation → bake; us3 then eu1 | Retain old replicas; unhealthy candidates remain excluded. Rollback probes retained replicas, drains candidate traffic and restores through a new generation. |
| app-lb binary | Stage a candidate on distinct loopback/admin ports → load current policy and pass local/peer probes → persist switch intent → switch existing host ingress to candidate → close old admission and confirm ingress adoption → drain all old public/peer streams → retire old process → bake; one region at a time | Old process/binary remains available until verification. Reconcile ingress target and both boot IDs after restart. Before rollback, make old instance current and healthy; switch back, then drain candidate. |
| Whole regional host | Inventory affected services, workers and stateful dependencies → establish alternate capacity → withdraw region from application routing AND external entry selection → confirm adoption/drain across both → perform maintenance → verify dependencies and gateways → restore/bake | Block if a database writer, required controller/secret dependency, caller or worker cannot survive evacuation. No forced stop to manufacture completion. |

For app-lb replacement, the stable host ingress (currently Traefik) stays running.
Old and new instances must not concurrently execute managed-VM reconciliation or
mutate the same local registry: candidate preparation is routing-only, with isolated
process state and an explicit single-owner handoff for any legacy managed pools.
The host-update helper runs outside the process being replaced. Existing in-place
restart remains a maintenance-only fallback, not a zero-interruption upgrade path.
Ingress switching must be proven to preserve existing streams. Keep-alive/HTTP2
connections must stop admitting new work to the predecessor without dropping
in-flight work; if the implementation cannot do that, the upgrade gate stays blocked.
Long-lived streams require a declared deadline policy; forced closure is not counted
as a zero-failure drain.

### Entry failover and stateful dependencies are explicit requirements

The chosen full-host-failover topology is a health-aware external HTTPS entry point
with both regional ingresses as origins. Its checks must validate application serving
readiness, not merely process liveness. It must support origin withdrawal and drain
evidence. DNS-only failover is not the zero-interruption maintenance mechanism:
cached answers and existing connections survive a DNS change. Sudden host loss can
still break existing requests; failover availability does not mean exactly-once
application execution or preservation of connections to a dead host.

No such external entry provider/configuration is selected or verified here. That
is a concrete delivery dependency for whole-host acceptance, not a blocker to
implementing application routing or side-by-side app-lb updates. Do not claim the
normal hostname survives regional loss until this gate has been exercised.

Both Orchestrator instances must operate on the same authoritative PostgreSQL state
with fenced operation ownership. Matching names or independent replicas are not
enough. On authority loss, gateways can serve last-valid policy subject to health
and exclusions; controllers stop mutations and maintenance. A gateway cold start
cannot invent policy. Database writer failover needs its own replication, fencing
and recovery proof; this design does not turn the existing database into HA.
Until that exists, maintenance of its host is blocked for dependent services.

CI follows platform acceptance: immutable app revision in both regions, shared run
and artifact metadata, and durable job leases/idempotent transitions so only one
worker owns each action. HTTP draining does not transfer jobs. CI-specific worker
quiescence and recovery belong to CI, not the platform discovery registry.

### One acceptance checklist and execution order

All unchecked rows are incomplete, even where supporting code exists. Link live
receipts here as each gate completes; historical checkpoints below are evidence,
not alternate acceptance lists. Test the same immutable disposable app with
region/revision/instance-identifying responses. Keep failed candidates as diagnostic
references until ownership-safe cleanup is separately performed.

| Gate | Required discriminating test | Current evidence/status |
| --- | --- | --- |
| [ ] Shared authority | Submit through either controller, observe one operation and fenced ownership through restart | Enrollment/shared discovery exist; full authority/restart proof outstanding |
| [ ] Peer path | Existing fixture via us3→eu1 and eu1→us3 local-only forwarding; direct VM ports remain unnecessary | Opt-in HTTPS peer transport implemented and tested with two local processes; no live regional deployment |
| [ ] Scoped discovery | Asymmetric 1-US/3-EU fixture: each gateway gets only its regional backends, both get the same regional policy; reject mixed generations | Coherent hierarchical snapshot/consumer integrated and locally tested with two gateways; asymmetric fleet/live proof outstanding |
| [ ] Forwarding semantics | Forced remote POST executes once; forged metadata rejected; local-only request never bounces; Host/query, streaming and WebSocket preserved | Local two-process test passes request preservation, POST once, WebSocket echo and held-body drain; live regional proof outstanding |
| [ ] Regional app rollout | Continuous requests via both ingresses and normal hostname; hold a request across withdrawal; no new target admissions, zero failed requests; us3 then eu1 | Local real-process fixture proves bidirectional policy withdrawal, held-body barriers and 24 successful survivor requests; candidate lifecycle/normal hostname/live proof outstanding |
| [ ] Failed candidate / rollback | Unhealthy revision stays excluded; retained healthy revision restored without duplicate candidate or lost drain evidence | Mechanism exists for direct backends; hierarchical live proof outstanding |
| [ ] Controller restart | Restart after create intent and after policy publication; exact operation resumes with one candidate and fresh observer evidence | Durable plan exists; live proof outstanding |
| [ ] app-lb update | Continuous public and peer requests plus held stream during old/new switch; no failures; crash helper after switch intent and reconcile exact instance | Current updater restarts in place; replacement/handoff missing |
| [ ] Overload / partition | Alternate region cannot meet budget; block planned drain. Stale observer cannot authorize maintenance; no forwarding loops under split views | Not live-proven |
| [ ] Whole-host maintenance | Withdraw external origin and regional capacity, drain, stop target ingress/host, verify normal hostname and dependencies through survivor, restore | External entry and stateful dependency failover not established |
| [ ] CI as one app | Shared run state, one job owner, controller/worker restart and regional evacuation without duplicate action | Deferred until platform gates pass |

Local evidence, 2026-09-22: app-lb has 854 passing tests (6 ignored); Orchestrator
has 99 passing tests with ignored tests explicitly included against disposable
PostgreSQL. `two_real_gateways_drain_through_authenticated_durable_barriers` exercises
the real app-lb binary, verified HTTPS in both directions, authenticated observer
credentials resolved through a local HeyoSecret fixture, shared PostgreSQL snapshots,
policy activation, both drain barriers and cold-boot invalidation. Held responses
survive each withdrawal, no new application work enters the withdrawn region after
adoption, and 24 requests through both ingresses succeed via surviving capacity.
The separate gateway smoke test passes POST-once, Host/query/body/Authorization,
WebSocket echo, forged-peer rejection and held-body drain across spec replay.
These are local fixtures, not deployed two-region acceptance. Public transition
admission remains closed. Internal routing-only admission now authenticates and pins
configured gateway boots, placement, observer bindings, routes, environment and
shared discovery authority, then publishes the operation/proposal atomically under
the lifecycle lock. The fixture exercises this path through the production v2
dispatcher. Tests reject stale drafts, wrong environments, unavailable/duplicate
observers, ordinary/regional rollout conflicts and observer binding drift; injected
journal failure leaves neither operation nor proposal. A new controller connection
returns the same receipt even when gateways are unavailable. Negative attestation
tests reject conflicting routes, missing credentials and cold predecessor rejoin.

Cold route enrollment now uses managed create-only registration on existing app-lbs,
with an explicit regional capability check, distinct discovery/peer secret refs and
the application health path. The real-process fixture starts without routes, rejects
incomplete configuration before writes, retains the first registration when the
second gateway is unavailable, retries without changing the first boot, and rejects
changed secrets/health configuration. Both gateways return 503 and no proposal exists
until admission. Enrollment cannot replace an active fleet. This is route enrollment
on provisioned gateways, not host provisioning or active process replacement.

Exact-candidate probes now traverse the authenticated HTTPS peer path after a
completed withdrawal. Both gateway boots, operation/generation, target host,
deployment and revision are pinned; the destination requires closed peer admission.
Probe work retains the ordinary lifetime counters but never enters the public pool.
The real-process fixture rejects unhealthy/wrong-revision candidates, stale policy,
forged peers and unauthenticated probe requests while survivor traffic continues.
Logs: `/tmp/heyo-candidate-probe-integration.log` and
`/tmp/heyo-candidate-probe-app-tests.log`. This verifies excluded-candidate health,
not candidate creation/recovery or rollback.

The private Cloud/daemon prerequisite is implemented locally: atomic creation
fingerprints, exact-ID authenticated recovery, a durable one-attempt Cloud fence,
daemon-persisted preallocated IDs, and host-bound mapping receipts. Cloud has 131
passing tests including PostgreSQL/mock-daemon lost-response recovery with exactly
one create POST; one unrelated live Mailcow test lacks its environment and is
explicitly excluded. Daemon library tests pass 247/247; all-target/all-feature
Clippy completes with warnings. Orchestrator validates the shared wire digest and
rejects mismatched recovery identities/mappings. Logs:
`/tmp/heyo-cloud-receipt-combined-tests.log`, `/tmp/heyo-daemon-all-tests.log`,
`/tmp/heyo-daemon-clippy.log`, `/tmp/heyo-orchestrator-receipt-tests.log`.
Missing/torn daemon intents and interrupted pre-launch work remain fenced; no
legacy-create fallback is allowed. This is not a live VM restart/rollout proof.
The Orchestrator candidate journal now persists one-use create permission and
immutable host/runtime bindings. Its PostgreSQL/mock-Cloud tests cover concurrent
claims, crash-before-send fencing, exact read-only recovery, changed intent/runtime
rejection, stale withdrawal evidence and absence of secret values in the journal.
Validated bindings publish with unknown health and draining enabled; endpoint
insertion, discovery revision and candidate item completion commit together.
An injected journal failure rolls back the endpoint insertion. The focused
publication tests pass in `/tmp/heyo-candidate-publication-tests.log`.
Archive SHA and the explicitly pinned application `expectedRuntimeRevision` remain
distinct identities. This input is internal groundwork, not an enabled HTTP API.
The running operation's `probe_candidates` gate requires completed withdrawal and a
discovery endpoint matching its durable candidate receipt; the real-gateway fixture
tests both missing-receipt rejection and successful authenticated peer probes before
the operation completes. A separate PostgreSQL test executes three publications in
one immutable plan and rejects prior-generation evidence at subsequent steps.
The combined fixture now simulates a lost Cloud create response, proves exactly
one create POST across controller connections, recovers the binding read-only,
publishes excluded membership atomically and probes it through both real gateways.
Logs: `/tmp/heyo-candidate-path-final-tests.log` (99 tests),
`/tmp/heyo-owned-probe-all-app-tests.log` (854 passed, six ignored).
V3 compiler/journal tests pass wrong-region entry, dependency bypass, stale forward
worker, interrupted-item, reverse-suffix and terminal-restart cases. Separate
policy tests require new rollback reports and preserve the old admission fence
when restoration activates a later generation. Logs:
`/tmp/heyo-v3-plan-journal-tests.log`, `/tmp/heyo-v3-policy-tests.log`.
The combined v3 suite passed 102 tests in `/tmp/heyo-v3-full-tests.log` before
the retained-probe/staging extension. Application baseline pinning now records all
serving deployment/host/region/revision/URL identities and requires capacity in
every entered region. Rollback probes require those exact immutable identities
and the rollback occurrence's own completed withdrawal. Membership staging probes
the entire desired regional set, then rechecks cursor, active generation, discovery
revision, fresh drain and the five-second probe deadline before committing endpoint
eligibility and journal advancement together. Policy weights do not change during
staging. The extended real-gateway restoration/rollback fixture passes in the full
103-test suite (`/tmp/heyo-v3-staging-verified-tests.log`), including changed membership
during held probes and atomic rollback on an injected journal failure. It explicitly
drives internal primitives and does not execute a complete dispatcher or bake.
Baseline unit tests and four app-lb regional tests pass in
`/tmp/heyo-retained-baseline-tests.log` and `/tmp/heyo-retained-probe-app-tests.log`.
Serving-phase exact-target probes also pass the 103-test Orchestrator suite and
five app-lb regional tests (`/tmp/heyo-serving-probe-full-tests.log`,
`/tmp/heyo-serving-probe-app-tests.log`). They require eligible membership, adopted
policy and a generation newer than the admission fence; they cannot use the
withdrawn-member exception to probe an excluded endpoint during bake.
The bake/final-verification primitive now uses durable, single-active probe epochs
scoped to item, policy generation and discovery revision. Whole-set probes and
claims expire after five seconds. An abandoned claim, failed check, changed scope
or observation gap over five seconds resets the healthy window before another
worker can advance. Both late success and failure are fenced; cursor/status changes
invalidate claims and timing atomically. The full 104-test suite passes, including
actual DB-connection loss during held HTTP work, late success/failure, expired and
abandoned claims, observation gaps, failed completion commit, rollback bake and
final verification (`/tmp/heyo-bake-epoch-full-tests-fixed.log`). Stale activation
rejection now explicitly releases its transaction before returning; its regression
requires actual boot-change invalidation, not an unrelated lifecycle-busy error.
No shared migration was run.
Cloud placement now intersects optional `allowedBackendServerIds` with the existing
region/environment/pool/capacity and exclusion filters. Empty fleets select no host;
omission preserves legacy placement and digests. Candidate claims require the exact
canonical pinned regional fleet before granting creation, and the request digest
binds that restriction. Cloud's full local suite passes 132 tests (live Mailcow
excluded), recorded in `/tmp/heyo-cloud-pinned-fleet-tests.log`.
The internal candidate executor resolves the persisted archive and per-slot runtime,
adds fleet restrictions and sibling host exclusions, and commits its single-use
claim before creating. Existing claims use read-only recovery without resolving
archives/secrets again or repeating POST. The real-gateway fixture now exercises
this executor after a lost create reply, then validates and publishes the recovered
host-bound receipt; all 104 Orchestrator tests pass in
`/tmp/heyo-candidate-executor-tests.log`.
The following 105-test run (`/tmp/heyo-application-policy-tests.log`) also exercises
derived forward withdrawal/restoration and rollback withdrawal/restoration through
both real gateways. Asymmetric-weight regressions reject non-target restoration,
changed active weights, gateway replacement and missing positive restore weights.
V3 application execution still explicitly rejects legacy fallback.
Active-policy preflight now uses a separate authenticated exact-target probe. It
can attest active N while proposal N+1 is pending, without treating N+1 preparation
reports as adoption or weakening withdrawn-member probes. The whole-set claim
pins execution occurrence, epoch, challenge, discovery version, active generation,
survivor identities and gateway boots for five seconds. Completion rechecks those
bindings and atomically journals evidence and publishes the next proposal.
The held-response real-gateway regression passed in
`/tmp/heyo-preflight-real-gateway-tests.log`: changed discovery and expired-claim
replacement both reject late results. Exact receipt-set validation rejects missing
or duplicate sources, stale epochs/challenges, changed boots and backend mappings
(`/tmp/heyo-preflight-receipts-tests.log`). The active-probe app-lb suite previously
passed 857 tests, with six ignored (`/tmp/heyo-active-probe-full-app-tests.log`).
The pending-forward/rollback interleaving passed in the 106-test combined suite
(`/tmp/heyo-preflight-full-tests.log`): active N remains probeable with N+1 pending,
rollback publishes N+2 against N, and the interrupted forward occurrence cannot
activate. The locked rollback-entry primitive also passes the 106-test suite
(`/tmp/heyo-rollback-entry-full-tests.log`), preserving the current region and
refusing to rewind or implicitly resume an already-blocked rollback.
The internal dispatcher passed 106 tests, including a complete us3-then-eu1 forward
rollout with requests through both gateways, alternating controller connections,
and one POST per candidate despite lost create replies
(`/tmp/heyo-full-forward-dispatch-tests.log`). Its durable attempt timer uses
`drain_timeout_seconds` per step, plus `bake_seconds` for bake, and blocks expired
work without changing routing. Restart does not renew that timer; explicit resume
or cursor advancement starts a new attempt. Every cursor/status transition fences
the monotonic probe epoch, including staging responses held across block/resume
(`/tmp/heyo-internal-dispatch-epoch-tests.log`, 106 passed). An earlier dispatcher
fixture failure came from its single-gate polling limit expiring during a multi-gate
rollback; the fixture now has a bounded whole-path allowance without changing the
product's per-phase budgets.
Atomic terminal service-state publication/restoration and retirement cancellation
pass the 106-test suite (`/tmp/heyo-retained-completion-full-tests.log`), including
injected journal failures. Existing state-writer and cancellation SQL run within
the owning transaction, with stable ingress and route preserved. Restored non-scalar
replicas revoke historical retirement authority; terminal forward completion also
protects every retained baseline before releasing regional ownership. Failed
completion rolls back service state, retirement protection and journal together.
Public admission and background v3 execution remain closed. These local checks
are not live two-region acceptance.
Internal application admission now passes the 106-test suite
(`/tmp/heyo-application-admission-verified-tests.log`, 272.68 seconds). The complete
us3-then-eu1 forward fixture uses admission instead of manually inserting its plan.
Admission authenticates and pins predecessor boots, active policy, retained endpoint
identities and immutable archive digest, then rechecks the baseline under the
lifecycle lock. Regression checks reject stale generation/version, discovery drift
during archive retrieval, missing placement pool, insufficient distinct pinned
hosts, unavailable/wrong-namespace gateways, conflicting intent and competing
ownership. A restarted controller returns the same admitted identity with both
gateway and archive dependencies unavailable. Rejected admissions leave no owner.
The debug-build whole-path test and admission future are heap-pinned after an
expanded fixture exposed stack overflow. Do not overlap copies of this fixture in
one database: service advisory locks are database-wide despite schema isolation.
Public admission, background execution and complete lifecycle acceptance remain
incomplete. Candidate creation is still stubbed; this is not a VM lifecycle test.

Remaining local gates: managed fleet lifecycle and explicit gateway boot/host/URL
replacement handoff; external ingress inventory/fencing; candidate lifecycle wiring
and failure/rollback integration; legacy revisionGuard cutover freshness integration
(requests containing it are rejected, not silently accepted). Routing-only admission deliberately rejects fleet
replacement. Remaining live gates: shared controller authority, normal hostname and
both regional ingresses, continuous rollout/rollback/restart, overload, and regional
infrastructure maintenance. Configured observers cannot prove absence of an
unregistered legacy ingress. No production deployment or shared migration was run.
Logs: `/tmp/heyo-enrollment-tests.log` and `/tmp/heyo-enrollment-app-tests.log`.
Reproduction is documented in
`orchestrator/docs/regional-rollouts.md` and `app-lb/README.md`.

Implementation order: peer forwarding and its fixture proof; region-scoped discovery
and readiness; hierarchical rollout/drain and failure tests; side-by-side app-lb
replacement; external entry/stateful dependency gates; then CI adoption. Automatic
capacity-weight tuning is later work, not a prerequisite for explicit-weight routing.
Do not repeat direct-host-port candidate creation to test the new design.

### Local implementation checkpoint — 2026-09-22

`app-lb/testdata/gateway_smoke.py` runs two real app-lb processes with a disposable
CA and two local echo backends. Certificate/hostname checks remain enabled. It
proved one HTTPS peer hop, application Host/query/body/Authorization preservation,
peer-header removal, single POST execution, rejected unauthenticated destination
and second-hop requests, WebSocket echo, and a partially delivered response held
across backend drain and spec replay. Twelve new requests all used the alternate
backend; the held request completed and in-flight reached zero. Health reconciliation
kept the surviving route serving. This exposed and fixed raw HeaderMap mutation
that broke Pingora's separate header bookkeeping; unit tests alone missed it.

Orchestrator's scoped snapshot preserves its authoritative version and echoes
the requested region. app-lb rejects missing/mismatched scope and foreign/unplaced
endpoints before applying membership. Scope changes fence cached old-region
backends and retain their in-flight accounting. Tests cover durable scoped reads
through restart, empty sets, foreign unhealthy endpoints, and scope changes.

Draft weights/gateway inventory are persisted alongside discovery under generation
CAS and the existing lifecycle lock. A disposable PostgreSQL test proved restart
persistence, one winning concurrent writer, stale-write rejection, refusal during
running/blocked rollout ownership, and explicit clearing without resetting history.
Current flat consumers and rollout executors refuse policy-bearing topology instead
of claiming to implement hierarchical routing. No live service has this draft set.

Verification: app-lb full suite 850 passed / 6 ignored; Orchestrator binary suite
88 passed / 0 ignored with all database tests enabled against a newly created,
disposable PostgreSQL container; schema checks 3 passed without golden-update mode;
final two-process gateway smoke passed. These are local tests, not live regional
acceptance. Migration 037 has not been applied to shared databases.

These are uncommitted local changes, not installed/configured capability. Gateway
transport and scoped discovery are deliberately not yet combined. Remaining local
implementation: integrate operation-owned policy transitions with explicit-weight
regional selection, local backend mapping and retained request guards; then side-by-side app-lb
replacement. The existing direct-backend rollout must not be represented as that
hierarchical protocol. All live acceptance gates above remain unchecked.

### Policy persistence and evidence checkpoint — 2026-09-22

Continued in `/Users/guangsongxia/dev/heyo-worktrees/public-heyvm-bootstrap`, branch
`feat/managed-heyvmd-rollout`. Migration 038 adds immutable operation/item-owned
proposals, a separate policy-generation allocator, one active reference, and
gateway report sequence high-water marks. Publication and activation commit with
the existing cursor/item journal under the lifecycle lock. Exact retries preserve
generation/revision; changed intent, premature activation and stale predecessors
fail. Clearing draft intent cannot disable an active policy or reopen legacy rollout.

Version-2 plans distinguish weight-only transitions from regional withdrawal.
Preparation, source adoption, all-generation source assignment drain, admission
closure and destination drain have separate persisted items. Internal report gates
require the complete pinned participant/boot inventory and fresh server-timed
evidence. Replay cannot refresh age; a boot change permanently invalidates the old
boot's evidence. Adoption and close ACKs must follow their corresponding commands.
This is not network fencing or authorization for a replacement boot to serve.

Local Orchestrator binary suite: **91 passed, 0 failed, 0 ignored**, including all
database tests against a disposable local PostgreSQL container. Tests cover a
forced journal failure rolling back activation, membership revision independent
of policy generation, late controller retries, stale/replayed evidence, distinct
source/destination gates, and delayed old-boot responses after restart detection.
These internal fixture reports are not live gateway observations.

No admission/activation HTTP endpoint or legacy reconciler invokes the new
primitives yet. Next integration is the authenticated observer poller plus app-lb's
hierarchical snapshot consumer, explicit weights, generation-pinned streaming
guards and reports. Keep the flat-consumer rejection and legacy lifecycle fences
until that path is complete. Migration 038 has not run on shared databases; no
push, deployment, ingress restart or live acceptance was performed in this slice.

## Current execution checkpoint — 2026-09-22

The descriptor/half-close fix is deployed to **both running heyvmd daemons**.
This supersedes the deployment status in the earlier checkpoints below; full
two-region application acceptance remains incomplete.

- Private [release](https://ci.eu1.heyo.work/runs/01a0c6e2047e-00000002)
  completed successfully after both validations passed. Cloud and host delivery
  completed in both regions, followed by sequential managed heyvmd updates.
  The ordinary us3 maintenance action was skipped in favor of bootstrap.
- Both installed `/usr/local/bin/heyvmd` files and running `/proc/<pid>/exe`
  binaries have SHA-256
  `5dd866bfbce02f21a714f36ffdc4d50823860a0b4ab74c0c0f0864059b6afee3`.
  us3 Supervisor `heyvmd-ci` runs PID 2316194; eu1 systemd
  `heyvmd-eu1.service` runs PID 981269. Final read-only host receipts
  `job-25f14e1c44d3` and `job-f54ab8739b95` recorded 18 and 13 FDs respectively,
  with the unchanged soft limit of 1024. These are short post-deploy observations,
  not a sustained-load leak test.
- Both durable daemon operations passed, including fresh runner reconnection
  before releasing their fences (`job-5aa97418c18d`). Neither daemon operation
  required manual recovery. The one-time HeyoSecret flag
  `ci/heyo/default/US3_HEYVM_BOOTSTRAP_REQUIRED` is now false (version 2),
  with its metadata preserved.
- The release was **not fully unattended**: eu1's heyvm upgrade returned 502
  after replacing/restarting the service. Exact installed/running heyvm hash
  `12a939272662f500e922179ff0c6ea0fcaca556d445947efaa122ad865f11694`
  proved completion; Cloud operation completion was reconciled under its
  advisory lock (`job-9093cf77a5fc`). Two old-attempt us3 host-work rows were
  removed only after proving their VMs stopped/unclaimed and retries successful
  (`job-38c3769701e8`). Automatic lost-response verification and old-attempt
  host-work reconciliation remain product gaps.
- Managed backend enrollment passed on both hosts through `/host/heyvm/enroll`,
  after verifying no active CI work on either host and `KillMode=process` on both
  backend units. Operations `production-enrollment-us3-20260922` and
  `production-enrollment-eu1-20260922` persisted `production` / `platform` and
  the same canonical `cloud/internal-api-key` version 1. Existing region/node
  identities remain `US` / `us3` and `eu1` / `eu1-firecracker`.
  eu1 now persistently advertises `https://ci.eu1.heyo.work/__runner`.
  Applied receipts: `job-538751589c72`, `job-0dc398028504`.
- Fresh heartbeat rows in the shared database confirm both hosts are available,
  active, and in `production` / `platform` (`job-71b0c25286b8`). Both public
  Cloud availability APIs report one schedulable backend in each of `US` and
  `eu1`, with zero active maintenance operations. These are enrollment and
  availability checks, not proof of successful workload placement.
  Both backend `/sandboxes` APIs accept the canonical key and the existing
  `ci-us3-trial/cloud-api-key` credential after restart (HTTP 200); the obsolete
  `eu1/backend-api-key` is rejected (401). No response inventory or credential
  values were printed. Receipts: `job-2bddb0e0baec`, `job-6a0809e7f5f2`.
- With explicit approval, added only `regional-rollout-smoke` to existing CD
  token allowlists: us3 `2c98d0a9fb5a`, eu1 `20863e146024`. Preserved every
  existing entry; no credential was minted or rotated. HeyoSecret references are
  `ci/heyo-public/default/APP_LB_US3_TOKEN` and
  `ci/heyo-public/default/APP_LB_EU1_TOKEN`. Both regional HeyoSecret APIs can
  read both references using the configured service role. Delivered existing
  `orchestrator/internal-api-key` into app-lb secret `regional-rollout-discovery`
  on both hosts for the discovery reader.
- Managed configuration rollout `platform-discovery-observers-us3-20260922`
  succeeded with readiness verified and predecessor stopped. Its active spec
  includes both observers and `regional-rollout-smoke` as discovery-routed.
  Both observers pin the same us3 Orchestrator discovery URL and use their
  existing regional Cloud HTTPS origins with the fixture's Host header.
- eu1 rollout `platform-discovery-observers-eu1-20260922` failed in preparation
  at its readiness deadline; the original Orchestrator remains serving, with
  no discovery configuration applied. **Enrollment missed a credential consumer:**
  `/etc/heyo/eu1/app-lb.secrets.env` still supplies `APP_LB_DAEMON_API_KEY` from
  obsolete `eu1/backend-api-key`, not canonical `cloud/internal-api-key`.
  The backend began rejecting it after enrollment; app-lb logs this misleadingly
  as "Missing API key" and cannot list/create its backend VMs. Secret comparisons
  printed booleans only (`job-bca85069c68d`); first observed failures were at
  02:48:09 UTC, after enrollment. This is an introduced control-path regression,
  not a new discovery-code defect. Both public Orchestrator health endpoints
  still return HTTP 200 and each old/new serving pool has one healthy VM.
- The approved repair is now complete: changed only `APP_LB_DAEMON_API_KEY`
  to canonical `cloud/internal-api-key`, preserving the original environment
  file as `.before-canonical-20260922`, and restarted `app-lb-eu1.service` through
  operation `heyo-applb-canonical-credential-20260922`. Public endpoints returned
  502 during recovery and later 200; this was an interruption, not a successful
  failover test. Retry `platform-discovery-observers-eu1-20260922-retry` succeeded.
  Both active Orchestrators now carry the same discovery observers/authority.
- Fixture archive SHA-256 is
  `33cb340f8ba1b42b1268186a9ca91f3982623a81fd368f196c165d7cac9ca39d`.
  The initial long deployment ID exceeded Cloud's 32-character ID column when
  replica suffixes were added. Shorter `acceptance-v1-20260922` failed health and
  cleaned up its candidate. `acceptance-v1b-20260922` also remained unhealthy;
  its candidate `sb-9be0a51e` was no longer present at the follow-up host check.
  No two-region serving baseline has passed.
- Live diagnosis found asymmetric dual-NIC routing: management host-forwarded
  traffic could reply through the bridge. Lowering the management default metric
  alone did not fix bridge-local clients. Connection marking plus a management
  routing table made the Orchestrator's probe to `161.129.71.178:2223/health`
  return the expected fixture identity (`job-742f5d72af8b`). This temporary guest
  intervention is not a deployed product fix or application acceptance result.
  The last candidate health report still targeted `10.88.0.144:8080`, whereas
  the guest had earlier reported `10.88.0.205`. Lease evidence shows these were
  distinct bridge MACs (`job-b735073f7c7b`), not proof of one stale lease.
  Both regional APIs subsequently reported terminal failure at 03:57:27 UTC,
  with a final probe to `127.0.0.1:8080` and timed-out candidate diagnostics.
- Source inspection found a separate discovery defect: `get_host_port_for_guest`
  conflates a busy handle with an absent mapping; `/internal-url` then switches
  to the private guest address. The local correction returns HTTP 409 for busy
  discovery, preserving Cloud's existing public-proxy fallback. This is a
  demonstrated code path, not proof of every observed live address change.
- Local persistent routing changes are in the private `two-region-platform-config`
  worktree, not installed on either host. Linux libvirt tests passed (19 tests).
  Ubuntu 24.04 Netplan generation and systemd unit validation passed. An isolated
  Linux network-namespace TCP test passed management ingress from bridge-local
  and remote clients, direct bridge ingress, and a non-DNAT SSH-like port, both
  before and after firewall save/flush/restore. The metrics-only negative control
  failed as expected. This is not a full VM reboot test. The final Linux build
  passed all 31 sandbox tests (including busy-handle discovery) and all 19
  libvirt tests. Both regression groups are now included in the CI workflow.
  Local commit `18b58236ad61a79a3826a63187c0592b8a159c45` was submitted through
  the supported exact-patch path, without a GitHub push:
  [validation build](https://ci.eu1.heyo.work/runs/01a0c75afdb5-00000005).
  The run succeeded: release build, Firecracker tests, stop/delete tests,
  routing/discovery regressions, packaging, binary smoke tests and upload.
  Artifact `heyvm` is 62,358,799 bytes with SHA-256
  `a6591eb31434400a9b31825da08b241b99b6865b12c4dde211ee10d2afe38b43`.
  This is an explicitly validation-only submission, not a deployment or merge.
  The supported CI host-maintenance path requires published merged source and
  rejects validation-only provenance; do not bypass that fence to deploy.
- Publication and CICD merge are now explicitly approved. The private branch
  was pushed normally and [PR #616](https://github.com/Heyo-Computer/heyo/pull/616)
  opened. Verified clean worktree, latest trunk ancestry, and matching local,
  remote and PR heads before submission. The zero-patch published revision is
  now in [release submission](https://ci.eu1.heyo.work/runs/01a0c796cadc-00000009),
  gated by [validation](https://ci.eu1.heyo.work/runs/01a0c796cadb-00000008).
  Validation passed and CICD merged PR #616. Both managed heyvm upgrades passed.
  Fresh `/host/heyvm/status` responses prove installed and running SHA-256
  `ccfedcc7c1c4f888522cfdacf478da9d488b2fcf7f6fcc2705b05bae9fdfde43`
  on both hosts. us3 uses `/usr/local/bin/heyvm` / `heyvm.service`; eu1 uses
  `/usr/local/bin/heyvm-eu1` / `heyvm-eu1.service`. The generic eu1
  `/usr/local/bin/heyvm` is a different installation, not the managed target.
  Artifact digest `b027824aa701e9e640ac4d770a48457645487df472bb1a6f7c88fac1f75af8ba`
  was independently downloaded and verified; its heyvmd binary digest is
  `5b6693abf4defe33dde3fe1a11d51ec144c751434e130237b785f013cc88b27b`.
- Release completed successfully after the explicitly approved correction of
  existing us3 CI token `2c98d0a9fb5a`: cleared its historical deployment allowlist,
  preserving namespace `default` and admin role, matching eu1's existing scope.
  No token was rotated. The coordinator resumed and both daemon jobs passed.
  Independent host diagnostics verified both running `/proc/<pid>/exe` digests
  equal the expected heyvmd digest above: us3 PID 2341085 (Supervisor
  `heyvmd-ci`, 20 FDs), eu1 PID 1213801 (13 FDs). These are point-in-time
  observations, not a sustained-load descriptor-leak test. An initial probe
  incorrectly assumed `heyvmd.service`; actual us3 ownership is Supervisor.
- Retried the immutable fixture as `acceptance-v1c-20260922`. us3 replica `r1`
  passed and published healthy discovery version 1 at `http://161.129.71.178:2223`
  without manual guest repair. eu1 replica `r2` failed before VM creation:
  archive `ar-666ec34c` was sought in eu1's local Cloud storage and was absent.
  Both deployed Cloud specs use `CLOUD_STORAGE_DRIVER=s3`. Cloud mirrors only
  local-driver archives to backends; heyvm uses its own separately enabled S3
  client or falls back to local files. The error indicates eu1 lacks a usable
  S3 client for this path. Shared archive delivery/configuration remains a gap;
  do not copy this one archive manually and call acceptance passed.
- Follow-up managed host diagnostics confirmed the configuration mismatch:
  us3 `/etc/heyvm/env` has `S3_ENABLED=true`, a bucket and AWS credentials;
  eu1 `/etc/heyo/eu1/heyvm.env` has `S3_ENABLED=false`, and its final
  `heyvm.secrets.env` overlay has no S3 configuration. Only configuration
  presence was printed; no credential values were exposed or changed.
  Current enrollment manages identity/placement but has no archive-storage
  fields. Extending Cloud mirroring was investigated and discarded: backend
  `/archives/local` files have no retention/deletion path, so mirroring S3
  objects would introduce additional persistent storage accumulation. No Cloud
  code change remains, and no test archive was copied manually.
- Shared S3 configuration correction completed without a new credential or
  binary release. Compared canonical HeyoSecret `cloud/s3-*` values with us3's
  configured bucket, region and both AWS credential fields: all matched
  (`job-bbe661584bd4`). From eu1, those same credentials successfully read the
  existing fixture object's metadata (`job-79c198c4cdb2`, 1460 bytes).
  The approved app-lb host-update path persisted them in eu1's final mode-0600
  `heyvm.secrets.env` overlay, preserving unrelated settings and a rollback
  copy. The existing managed enrollment API applied operation
  `shared-s3-eu1-20260922`; only `heyvm-eu1.service` restarted, with verified
  `KillMode=process` and no in-flight creates observed before the operation.
  This was configuration recovery, not proof of a coordinated traffic drain.
  Readback reports `applied`, PID 1237726, and S3 initialized/enabled.
  Public `https://ci.eu1.heyo.work/__runner/health` and
  `https://cloud.eu1.heyo.work/health` both return 200 (`job-05cf9930620c`
  supplies internal startup evidence). No archive was manually copied.
  The earlier proposed new enrollment API/release is not a prerequisite for
  this correction and is not implemented.
- Fixture retry `acceptance-v1d-20260922` failed before VM creation: app-lb
  namespace tokens return 403 for an absent deployment, whereas Orchestrator
  bootstrap expected 404. Registered the exact discovery-only fixture spec
  through both app-lbs' existing create-only API with the existing CD tokens;
  both returned 201 and exact-spec readback succeeded. No token scopes changed.
  A local Orchestrator fix now handles either 403 or 404 through capability-
  checked create-only registration and mandatory matching readback. Both
  targeted host-ingress tests passed, including forbidden creation and
  protection against replacing an unreadable existing route. This code fix
  is not published/deployed; live registration used the supported app-lb API.
- Retry `acceptance-v1e-20260922` exposed unhealthy ingress. Retained us3 VM
  `sb-49afc11f` was stopped despite TTL 0; its stop cause is not yet established
  (the checked kernel/heyvm logs did not identify OOM, TTL or idle eviction).
  Started it through the backend API; it booted and served the expected revision
  without guest edits. eu1's Traefik front proxy lacked the fixture Host route.
  Its existing authenticated `/service-routes` API registered only that Host
  to `http://127.0.0.1:6189`, the managed eu1 app-lb frontend, preserving Host.
  Both public regional TLS origins with fixture Host now return 200, as does
  `https://regional-rollout-smoke.us3.heyo.work/health`. At this point all
  responses came from us3; this was not yet proof of two-region capacity.
- Retry `acceptance-v1f-20260922` passed the host-ingress gate, then failed
  candidate creation with `Backend Libvirt not available on this platform`.
  eu1's managed runtime allowed only Firecracker; us3 allows libvirt and
  Firecracker. Persisted `MVM_BACKENDS=libvirt,firecracker` in eu1's final
  environment overlay after verifying its libvirt connection and no in-flight
  creates, retaining a rollback copy. Managed enrollment operation
  `shared-runtime-eu1-20260922` applies the restart. No binary release or
  credential change was required.
- Established why retained us3 fixture `sb-49afc11f` repeatedly stopped:
  the attached heyvmd created an independent cleanup manager using `/root/.heyo`,
  while its external API owned metadata under `/var/lib/heyvm`. heyvmd's log
  explicitly records reaping this valid libvirt domain as an orphan. Disabled
  orphan reaping and stale-network sweeping in both attached heyvmd processes
  through managed host updates, verified their live flags, and restarted the
  retained fixture through its backend API. No service disks were deleted.
  A local private-code fix makes attached heyvmd passive and activates cleanup
  only after its own API binds both requested listeners. Three ownership tests
  and `cargo check --bin heyvmd` pass on macOS; this code is not released.
- Retry `acceptance-v1h-20260922` exposed eu1's generic network settings leaking
  into libvirt: DHCP 10.88.0.100–254 was outside configured 10.120.0.1/16.
  Inventory proved Firecracker already uses 10.120.0.0/16 and libvirt's existing
  `heyo-net` uses 10.88.0.0/24. Migrated the former settings to Firecracker-only
  keys and explicitly selected existing `heyo-net` for libvirt. Managed enrollment
  `driver-networks-eu1-20260922` applied; API health and configuration verified.
  No network was destroyed or replaced. Retry `acceptance-v1i-20260922` passed
  network creation but failed QEMU access to its candidate qcow2 under
  `/var/lib/heyo-eu1/heyvm/images`. Retry v1j then exposed the equivalent
  workspace ancestor restriction. `namei` and ACL inspection identified mode
  0700 on the data root, images and sandboxes parents. Granted only traversal
  to `libvirt-qemu` on these three directories, retaining original ACLs and
  verifying access as that user. No recursive permission changes, guest edits
  or data deletion. Fresh immutable retry `acceptance-v1k-20260922` booted
  eu1 `sb-6a270974`, and host-forwarded port 2227 returned the expected revision.
  It nevertheless failed orchestrator readiness: the advertised endpoint was
  `http://ci.eu1.heyo.work:2227`, not a proven cross-host workload address.
  A direct us3 probe to `135.181.222.73:2227` also timed out. QEMU listens on
  0.0.0.0, and inspected eu1 INPUT rules permit this port. Simultaneous packet
  captures at 08:04 UTC show seven SYNs leaving us3's external interface and
  zero packets on port 2227 arriving at eu1's external interface (receipts
  `job-2025bef6a4c6`, `job-8b56350f0c92`). This isolates the failure upstream
  of eu1's host firewall, but does not identify the filtering device. No
  provider-firewall management path or matching HeyoSecret credential metadata
  was found. Restoring this cross-host data path requires upstream network
  administration access; host INPUT changes cannot fix packets that never arrive.
  Do not substitute
  local health for cross-region reachability or repeat candidate creation until
  this data path works. Existing discovery accepts direct HTTP host/port targets,
  not the HTTPS `__runner` control-plane prefix or iroh tickets.
  At 07:44 UTC the normal URL and both regional TLS origins with fixture Host
  returned 200 from retained us3 `sb-49afc11f`; eu1 capacity is not yet proven.
  The retained us3 QEMU process started at 07:25:35 and was still running after
  07:49, beyond the previous false-orphan reap interval.
  Verify a successful fresh candidate and fresh-guest/reboot behavior, then
  continue drain/restore and failure acceptance. Do not substitute a manually
  repaired candidate for an unmodified managed rollout.
  Observer regions must match the actual placement IDs `US` and `eu1`.
  `regional-rollout-smoke.us3.heyo.work` resolves to us3; the equivalent eu1
  hostname and unqualified `regional-rollout-smoke.heyo.work` do not resolve.
  Use existing regional TLS origins with the fixture Host header for ingress
  probes; do not claim global entry-point failover without a real tested route.
  Discovery acceptance, restart/rollback tests and sustained-load daemon
  verification remain outstanding. A green host release does not prove these
  gates or a working two-region CI application.

## Earlier execution checkpoint — 2026-09-22

This section supersedes the historical checkpoints below. Live application
acceptance remains incomplete; CI still serves from eu1 with us3 forwarding.

- Public [PR #103](https://github.com/Heyo-Computer/heyo-public/pull/103) and private
  [PR #614](https://github.com/Heyo-Computer/heyo/pull/614) are merged. The public
  [release](https://ci.eu1.heyo.work/runs/01a0c65e753c-00000016) passed, updating both
  Orchestrators and CI. No app-lb binary update was selected.
- Both private validations of the merged revision passed. Its
  [release](https://ci.eu1.heyo.work/runs/01a0c66b9e7f-00000002) failed before any
  deployment: GitHub semantic-release had advanced main with only Cloud/heyvm
  version bumps. The saved candidate no longer matched the remote tip. CI reports
  this definite publication precondition failure misleadingly as an unknown outcome.
- Submitted the exact new main revision through the normal platform path:
  [replacement release](https://ci.eu1.heyo.work/runs/01a0c683e2b5-00000007), Cloud
  validation `01a0c683e2b2-00000005`, heyvm validation `01a0c683e2b4-00000006`.
  Monitor these; do not replay the superseded candidate or overwrite main.
- Keep `US3_HEYVM_BOOTSTRAP_REQUIRED=true` until both hosts finish verified delivery.
  The replacement release has now passed both Cloud deployments and the us3
  heyvm bootstrap. Twelve eu1 older-attempt host-work records were reconciled only
  after confirming terminal jobs, no pool claims/cleanup, and both remaining CI
  VMs stopped (`job-6dff5ad05b59`). eu1 maintenance then failed direct transport:
  us3 Cloud cannot reach eu1's public port 34099, while eu1 Cloud can. The existing
  HTTPS `https://ci.eu1.heyo.work/__runner` route was verified from us3, then
  registered through Cloud's backend registration API, preserving other fields.
  After proving no helper launched, the Cloud operation was retried once. Its
  response was lost (502), but the new helper launched and the running binary
  matched the expected `2651756c5826714dfe140c2c75d4d854c23191e3e633031c66ccddfd5a8872f2`.
  Cloud completion and CI fence recovery were reconciled from that exact live
  evidence (`job-862f901a7c20`, `job-e0288332a407`); original failed run/job history
  was retained. Persistent managed enrollment must retain the reachable HTTPS
  hostname rather than reverting it to port 34099 during a future heartbeat.
- Read-only database probes from the respective hosts reached the same writable
  PostgreSQL server, database `orchestrator_us3`, and cluster identifier through
  both configured connection paths. Receipts: `job-67bc5af5297b` (us3 result;
  its cross-region eu1 proxy probe timed out) and `job-032e111f450b` (eu1 result).
  Both public Orchestrator APIs accept the canonical service credential. This
  establishes the configured shared database target, not live restart acceptance.
- Both app-lbs advertise create-only registration and managed discovery sources.
  Neither Orchestrator deployment currently configures discovery observers or
  discovery-routed services. Configure those through managed specs after host
  delivery, then deploy the disposable HTTP fixture and execute the acceptance
  gates below. Do not call healthy endpoints evidence of regional drain.
- With the user's cleanup approval, verified ownership/reference checks allowed
  product image eviction of two unused eu1 CI images (20 GiB total). Receipt
  `job-635ffb0cde48`; eu1 then had 53 GiB free (88% used). us3 last measured 35%
  used with 2.3 TiB free. Active build images and all service/database storage
  were retained. These are timestamped observations, not current disk guarantees.
- Removed exactly 21 proven stale CI host-work records in a guarded transaction;
  retained historical jobs, sandbox disks and the runner fence. No storage purge
  or permission bypass was used to make the host drain pass.
- The replacement Cloud validation passed. heyvm attempt 2 resumed after recovery
  of eu1's networking daemon. Its old process reached 1,024 FDs (1,020 sockets),
  with repeated `Too many open files` errors; CI cleanup timed out rather than
  falsely releasing its VM claim. The verified networking-only systemd group
  was restarted (`job-1ad7080b530f`); the backend/VM services were untouched.
  CI then reconciled the cleanup record itself and resumed heyvm validation.
- This is an **undeployed fix**, not a newly discovered proxy-code defect:
  [the September 18 half-close fix](https://github.com/Heyo-Computer/heyo/commit/902311eafa96a3c79eef150c6f29cf1d6be259af)
  is in main, but the earlier thread verified only temporary daemon restarts.
  eu1's running `heyvmd` matches `/usr/local/bin/heyvmd`, SHA-256
  `7c262d9dfa4d8eaa0c7d38964465da87871e348eb73935cd0487ee29e6101927`, file dated
  September 11. us3's running binary matches its installed file, SHA-256
  `361ab3d44c867aeecec26d7d758c1035f3dccd29d853be5785cf7de6e2cde879`, dated
  September 15. Receipts: `job-c85a8ba7b2af`, `job-210bdc91521e`.
  The current host maintenance/bootstrap installer selects only `heyvm`, even
  though CI packages both binaries. Completion must include a managed `heyvmd`
  replacement/restart on both hosts and executable-hash verification against
  the validated artifact. Updating `heyvm` alone does not close this gap.
- Public [PR104](https://github.com/Heyo-Computer/heyo-public/pull/104) adds the
  managed daemon action. A patch-only submission validated but could not publish:
  release requires a fetchable source commit. After branch publication, the
  [release](https://ci.eu1.heyo.work/runs/01a0c6d8086d-0000000e) passed, merged the
  source, and deployed CI revision `2685ffbbd7b1aab441cbfc360afdbd48c54b699f`.
  Both daemon aliases were added to HeyoSecret's existing bootstrap target mapping
  (version 3), preserving old targets and secret metadata. Private
  [PR615](https://github.com/Heyo-Computer/heyo/pull/615) adds sequential us3/eu1
  daemon jobs; its [release](https://ci.eu1.heyo.work/runs/01a0c6e2047e-00000002)
  awaits Cloud validation `01a0c6e2047a-00000000` and host validation
  `01a0c6e2047c-00000001`. Both recovered from first-attempt sandbox-start timeouts
  and are running on us3; their old-attempt host-work rows may require evidence-
  based reconciliation before host drain. Do not clear an active build's rows.
  The Python installer suite passed 12 tests; the CI Rust suite passed 458 tests
  with 85 integration tests ignored. The actual private release workflow passes
  the public CI parser (8 jobs). The downloaded validated artifact was verified
  by its outer digest and the installer selected heyvmd with SHA-256
  `9d93bd26fdaba270d5251c6328403428013dab316bed0d1f760a7b188d922e72`.
  The installer supports the observed systemd and Supervisor layouts; systemd
  control-group mode requires an isolated daemon cgroup. Normal completion and
  explicit recovery both require fresh runner reconnection before unfencing.
  User approved publication and release writes; continue without new milestone
  approval requests. Next: monitor the private release; extend the existing us3
  CD token's deployment allowlist for its exact new bootstrap/daemon launcher IDs
  once step IDs exist (eu1 token already covers its namespace). Preserve the
  current run's immutable plan. Verify both running
  daemon hashes and live FD/connection behavior before claiming the leak is fixed
  on the hosts. No new daemon delivery was executed during this checkpoint.

## Execution checkpoint — 2026-09-21

The platform acceptance gates below remain incomplete. CI migration is downstream
of these gates, not a substitute for them.

| Gate | Evidence and remaining work |
| --- | --- |
| Managed discovery configuration | Public PR #101 deployed; the release completed, but no disposable two-region application or live observer acceptance was executed. |
| Regional backend enrollment | Private companion [PR #612](https://github.com/Heyo-Computer/heyo/pull/612) merged at 11:57 UTC after both validations passed. The [coordinated submission](https://ci.eu1.heyo.work/runs/01a0c3a39e7d-00000031) subsequently failed preparing the us3 Cloud candidate; the old source was retained. Neither host update nor live enrollment has completed. |
| Local verification | Linux x86_64 compilation and Clippy completed; isolated Linux execution passed 3 enrollment and 5 environment tests. Final serial macOS library run passed 279 tests. An earlier parallel run failed `ci_image_eviction_requires_ownership_and_readable_inventory`; that test passed in isolation and serially, and its concurrency failure remains unresolved. These checks do not exercise real systemd restarts or regional traffic. |
| Shared rollout authority | Earlier read-only database probes reached the same writer through both regional paths. Controller restart, ownership fencing and duplicate-candidate acceptance remain unproven. |
| Both-region delivery | The submitted workflow updates Cloud us3 then eu1 before either host. us3 needs the existing native bootstrap bridge because its old updater probes port 3000 instead of 34099; eu1 uses normal maintenance. A public-tagged operator variable gates that first update and must be cleared only after the entire release passes. Delivery is pending, not verified. |
| Application acceptance | Pending: deploy one disposable revision across both regions, configure both discovery observers, continuously exercise both ingresses, withdraw/drain/update/restore/bake each region, and inject an unhealthy candidate plus a controller restart. |
| Infrastructure maintenance | Pending independently of application rollout: prove ingress and database/dependency continuity before a regional daemon or ingress restart. |

Continue with backend configuration and its tests, then managed enrollment and
the disposable-app acceptance above. Preserve the existing large image directories
as evidence; this platform work does not authorize storage cleanup.

### Read-only rollout preflight — 2026-09-21

These are observed targets, not desired placement or proof of regional failover.

| Surface | us3 | eu1 |
| --- | --- | --- |
| Cloud health | 0.39.4; no deployment SHA reported | 0.40.0 |
| Backend identity | `us3`; region `US`, pool `platform`, environment `stage-eu1` | `eu1-firecracker`; region/pool/environment `eu1` |
| Installed heyvm | 0.48.1 | 0.50.1 |
| Managed host target / systemd unit | `stage-eu1-host-heyvm` / `heyvm.service` | `eu1` / `heyvm-eu1.service` |
| Sandbox API with canonical Cloud credential | HTTP 200 | HTTP 401; regional backend credential returns 200 |
| Maintenance status with canonical Cloud credential | HTTP 200 | HTTP 200 |

Both Cloud availability endpoints reported one schedulable backend for `US`
and none for either `eu1` or `EU`. Neither host has PR #612 installed.
The app-lb inventory shows CI serving from eu1; us3 forwards CI traffic to eu1
and its local CI deployment has zero ready instances. This is not CI failover.

The HeyoSecret maintenance mapping contains only eu1 and uses
`hd-g52t-Sb5jFy0YWSt` for both runner and backend IDs. The bootstrap mapping
instead names backend `eu1-firecracker`. Reconcile this against Cloud's registered
backend identity before modifying the mapping; runner IDs are not backend IDs.
These are stored mappings, not proof of effective CI configuration: environment
overrides have not been excluded.

The existing maintenance action requires a confirmed merged release and exact
artifact publication provenance. The successful validation-only artifact does
not satisfy that contract. The private regional release workflow updates only
eu1; do not launch it as a two-region rollout. CI-job/backend-operation drain
also does not establish application ingress or database continuity.

The user has now approved merge of PR #612 and managed deployment / enrollment
of both named hosts, including canonical role credentials and trusted targets.
No storage cleanup, database schema change, or app-lb restart is included.

### Approved execution in progress

- The coordinated submission above validates Cloud in run
  `01a0c3a39e7a-0000002f` and heyvm in `01a0c3a39e7c-00000030` before merge.
  Its exact source is the pushed PR head `cc173699142d8c03d0e2633e4894e70863551e4d`.
- Live read-only host diagnostic jobs `job-647d2e2dec1a` (us3) and
  `job-cc01c92350b4` (eu1), under `platform-rollout-preflight-20260921`, confirm
  both units have `KillMode=process` and no reported stop hooks. Both load two
  environment files. Enrollment now selects the final overlay, with an explicit
  mapping pin required to match it. us3 has 2.4 TB free; eu1 has about 42 GB free
  (90% used). These jobs performed no host configuration writes or restarts.
- Read-only SQL through the canonical Cloud DB configuration found no
  `eu1-firecracker` row. Registered that verified host through Cloud's API with
  `status: unavailable`, not advertised capacity. Enrollment must subsequently
  publish and verify effective placement/authentication.
- HeyoSecret `ci-controller/host-maintenance-targets` version 2 fixes eu1's
  backend ID and adds us3. `ci-controller/host-heyvm-bootstrap-targets` version 2
  adds us3's explicit native mapping. Its target becomes `us3` after bootstrap;
  unit remains `heyvm.service`, executable `/usr/local/bin/heyvm`, state directory
  `/opt/heyo/heyvm`, health port 34099. No effective CI maintenance override was
  present in the running controller.
- `ci/heyo/default/US3_HEYVM_BOOTSTRAP_REQUIRED` version 1 is public-tagged `true`.
  Do not clear it mid-release because each job resolves variables independently.
  The existing us3 operator token was copied into the private workflow's secret
  prefix and its scope extended to `cloud-us3`; it was valid but previously
  restricted to public-repository deployments. Scope its bootstrap launcher to
  the exact current submission ID, not all deployments.
- Follow-up checks: all 11 Cloud maintenance tests passed, including disposable
  PostgreSQL fence/replay tests; all four enrollment tests passed; the installed
  CI parser accepts the three affected workflow plans. The initial disposable
  DB test lacked its password; the corrected invocation passed unchanged tests.
  Earlier full-suite concurrency flake remains unresolved.

Continue monitoring, verify merge and both binary updates, then run managed
enrollment and the disposable-app acceptance gates. Preserve release provenance
and fences; do not install the earlier validation-only artifact directly.

### Capacity and runner investigation — 2026-09-21 11:31 UTC

- heyvm validation `01a0c3a39e7c-00000030` passed, including artifact upload.
  Cloud validation is still in its final 900-second admission backoff after
  attempt 3 of 4: eu1 had 34,009,296,896 free bytes versus 59,055,800,320 required.
  The release coordinator remains queued; no merge or deployment has run.
- The completed heyvm VM `sb-ec97ce2d` is now an idle CI cache on eu1. Let the
  existing admission policy measure and reclaim eligible caches; do not lower
  its disk guard or delete service storage. An individual validation rerun
  cannot replace the original coordinator's frozen validation membership;
  a terminal validation failure requires a fresh full submission.
- us3's CI runner is offline despite its active backend and ample disk. Fresh
  connection-ticket reads succeed through both Cloud origins, but a sandbox
  read through the registered daemon tunnel returns 502. The dashboard's earlier
  connection-ticket 502 is historical, not the current failure.
- Read-only diagnostic jobs `job-0a51605f3909`, `job-50e360289e5f`, and
  `job-1d55cbb7dff0` establish that `/usr/local/bin/heyvmd` PID 1892636 runs as
  Supervisor program `heyvmd-ci`, under `supervisor.service`. There is no
  `heyvmd.service`; its inactive systemd status did **not** mean the daemon was
  stopped. The separate `heyvm.service` owns the backend on port 34099.
- `job-8fe60c0361a2` shows repeated `Too many open files` when the daemon connects
  to that backend, plus failed heartbeat/DNS and inventory operations.
  `job-49e2f49f4b92` measures 1,024 descriptors at the 1,024 soft limit, including
  1,018 sockets; host DNS works outside the exhausted process.
  `job-eef71281dba5` finds 1,013 established TCP connections to port 34099 and no
  child processes. Descriptor exhaustion is proven; the cause of retained
  connections is not yet proven. Do not label it a credential failure.
- Recovery candidate: restart only Supervisor program `heyvmd-ci`, preserving
  its current configuration, backend process, VMs, app-lb, and databases, then
  verify a fresh heartbeat, tunnel reads, CI dispatchability, and descriptor
  growth. No restart, limit change, or storage cleanup was performed during
  this investigation. This additional production-process restart requires
  explicit approval before execution.

### Approved runner recovery — 2026-09-21 15:01 UTC

- The user approved the additional restart. Authenticated host job
  `job-2710f1fbf3d0` restarted only Supervisor program `heyvmd-ci`; new PID
  2248891 replaced 1892636. The backend PID stayed 1357842. No storage cleanup,
  credential changes, app-lb restart, or database changes were performed.
- Cloud reports us3 online with a fresh heartbeat, and the public Cloud daemon
  sandbox endpoint succeeds through its tunnel. CI `/networks` reports both
  runners online and eligible. `job-2c6951c839a3` measured 13 descriptors after
  recovery. This restores operation; it does not prove a permanent leak fix.
- While approval was pending, Cloud validation also passed and the coordinator
  merged PR #612. Its us3 Cloud operation
  `ci-service-3d242c25c9342b7c4482f113ff5f89ff950344917f82afdda01afbe57aa4d6ea`
  is terminal `failed`, phase `preparing`, with source retained and no readiness
  or predecessor-stop success. Do not report a successful Cloud upgrade.
- A concrete preparation defect: the existing Cloud spec has no release mount;
  CI creates one without auth even though the bundle is private (anonymous blob
  GET returns 401). A local public-CI fix inherits the rootfs secret reference
  only for the same artifact store and preserves existing mount auth. Four
  focused unit tests pass; the PostgreSQL reconciliation test was ignored.
  The generic remote preparation error does not expose which substep failed.
- The failed coordinator still has transitive jobs `pending`. The scheduler
  advances one snapshot, and the rerun API rejects nonterminal jobs even when
  the run is failed. Do not bypass that safety gate or modify production rows.
  A supported release recovery path remains necessary alongside publishing and
  deploying the CI mount-auth correction; neither has been executed.

### Public CI recovery release — 2026-09-21

The user explicitly approved implementation, PR publication, coordinated CI
submission, merge and CI controller deployment, followed by continuation of the
private two-region rollout and acceptance. Do not ask again at these milestones.

- [Public PR #102](https://github.com/Heyo-Computer/heyo-public/pull/102) contains
  the same-store artifact credential-reference inheritance fix, fixed-point
  dependency failure/skip scheduling, and the requested AGENTS.md guidance.
  No credentials were created or changed. Both live Cloud specs refer to the
  same artifact store and secret name; a regional-looking name alone was not
  evidence of different credentials.
- Verification passed: 458 CI unit tests and 46 native-runner tests, plus the
  separately executed PostgreSQL/NATS transitive failure test and PostgreSQL/HTTP
  service reconciliation test. Other 85 integration tests remain ignored by
  the default suite. The new test verifies matrix completion, a three-job skip
  chain, independent work, always-run cleanup, and the active-job rerun gate.
- [Coordinated submission](https://ci.eu1.heyo.work/runs/01a0c519aefc-00000035)
  validates CI in `01a0c519aef8-00000034`, then merges and replaces the controller.
  Do not separately merge or deploy while this coordinator owns the operation.
- After controller verification, use a **full** `git submit --submit-empty`
  for the exact already-merged private revision. The client retains its parent
  diff for this supported recovery case; fresh validations establish provenance
  for the new coordinator. Individual reruns cannot authorize release delivery.
- This operational checkpoint remains local rather than being included in the
  public PR. Its earlier version is also preserved in a labelled Git stash.

### CI deployed; private recovery submitted — 2026-09-21 18:03 UTC

- Public PR #102 merged. Its coordinated release passed, and public
  `https://ci.eu1.heyo.work/healthz` returned HTTP 200 with revision
  `09668edf316ba18d872258a33eb008f00f0c8513` and executable SHA-256
  `9d615bb6206ebe2d17e98827d045b50691fc49863eca3872dddcb7683249fab3`.
  The deployment record is `passed`, phase `complete`, submissions reopened.
- Full private recovery submitted from the clean original worktree at the
  unchanged merged revision. [New coordinator](https://ci.eu1.heyo.work/runs/01a0c5237ce6-00000002)
  validates Cloud in `01a0c5237ce2-00000000` and heyvm in
  `01a0c5237ce5-00000001`. Monitor this run, not the historical failure.
- Added this run's exact bootstrap launcher
  `heyvm-bootstrap-e09a94ea47ecf5121ef3ac41716d9b0b` to the existing us3 CD
  token's deployment allowlist, preserving its other entries and namespace.
  No token was minted, rotated, or replaced. Keep the bootstrap-required
  variable true until the entire private release completes.
- Local disposable PostgreSQL test database was dropped and the temporary NATS
  server stopped after verification. Public operational checkpoint remains
  uncommitted; production database/schema and storage remain untouched.

### Acceptance preflight and environment comparison — 2026-09-21 18:32 UTC

- Both private validations passed. The recovery coordinator completed us3 Cloud
  replacement and started eu1 Cloud; host updates and enrollment remain pending.
- Both app-lbs advertise create-only registration and managed discovery-source
  support. Neither has discovery routes, and neither Orchestrator deployment
  configures discovery observers or discovery-routed services. Configure these
  through managed deployment specs before attempting the disposable app.
- Both public Orchestrator health endpoints report the same deployed revision
  `58f926c788c04cd44aa3310bfcfe5613bd3516db`. This is not traffic acceptance.
- Cloud's persisted specs configure `CLOUD_ENVIRONMENT=us3` and `eu1`, while
  each Orchestrator calls its local Cloud. Pool-based placement filters on the
  handling Cloud's environment, so registration/enrollment must not leave the
  two controllers with different eligible capacity. No environment change has
  been made while the current coordinator owns the Cloud rollout.
- Compared the user's local Uber source: Compute's cluster catalog filters
  clusters by its configured runtime environment, independently of zone/cluster.
  Its `prod1` through `prod4` deployment groups all use `production`. The
  recommendation is one environment for this Heyo system, with region retaining
  physical placement meaning, not a new per-region environment choice.
- Read-only us3 diagnostic `job-7a002e95cb4f` found the recovered network daemon
  still running at PID 2248891, with 80 descriptors after 3h18m. The backend PID
  remained 1357842. This does not establish the cause or permanent fix of the
  earlier descriptor accumulation.
- Disposable HTTP fixture preparation is local under
  `/tmp/heyo-regional-acceptance/app.py`. It supports revision/instance identity,
  held requests, admission recording, and an unhealthy revision. Local checks
  passed concurrency, delay, invalid-delay rejection, and unhealthy responses;
  no live app or archive has been deployed yet. Attribute instances to regions
  using authoritative discovery; hostnames alone are not region evidence.

### Shared production environment applied — 2026-09-21

- The user explicitly requested both regions use `production`. After both CI
  Cloud replacements completed, managed configuration rollouts
  `platform-production-environment-20260921-us3` and
  `platform-production-environment-20260921-eu1` changed only each persisted
  Cloud spec's `CLOUD_ENVIRONMENT`. They reused the validated immutable artifacts.
  Both operations succeeded with readiness verified and predecessors stopped.
  Both public Cloud `/health` endpoints report `cloudEnvironment: production`
  and version 0.40.1 at the private recovery revision. Regional hostnames remain
  unchanged. This does not prove backend registration or placement convergence.
- Orchestrator requests region/pool and verifies Cloud's placement response;
  it has no independent placement-environment setting. Auth's application
  environment is separate. app-lb's local autoscaler creates VMs through its
  configured backend, bypassing Cloud placement. Existing managed platform VMs
  are therefore not automatically migrated by the Cloud environment change.
- The private coordinator's us3 bootstrap remained at `draining`; no host
  update success or enrollment has been verified. Its drain predicate checks
  running target-runner jobs, host-work records, and claimed/building/draining
  VM-pool rows. Do not bypass these gates. Read-only database diagnostics via
  the managed `postgres-eu1` VM exec API timed out; no SQL write was attempted.

## Proposed architecture

**Keep app-lb as a regional data plane. Add the global service view and decision
loop to Orchestrator, using Cloud for host allocation.** Do not turn app-lb into a
scheduler and do not build a second service registry beside Orchestrator discovery.

The design has two paths: a control loop that places capacity and assigns traffic,
and a request path that continues without a control-plane call per request.

### Operator experience: one concise JSON service file

The [app-lb deployment file](../app-lb/examples/README.md) style is implemented:
one declarative file with `id`, `routes`, `vm`, `scaling` and `health` is also
accepted by Orchestrator's service deployment endpoint. The regional overrides and
traffic fields below remain proposed extensions; existing app-lb does not accept
them. They avoid separate operator-managed placement, discovery and
traffic-assignment documents.

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

## Existing implementation foundation and next regional change

The opt-in [regional service rollout API](../orchestrator/docs/regional-rollouts.md)
now persists per-region replica slots and runtime overrides, region exclusions,
observed app-lb drain gates, health/bake gates, and explicit rollback. This is a
service deployment operation over already configured discovery ingress, not the
whole-region host maintenance or continuous capacity reconciliation proposed below.
It does not establish CI data replication or background-worker ownership.

Orchestrator service deployment already accepts `desiredReplicas` and
`replicaRegions`, preserves replica regions during rolling replacement, and fails a
rollout that does not establish the requested regional coverage. It also accepts the
app-lb-style service JSON and translates equal `scaling.min_replicas`/
`scaling.max_replicas` values and `deploy.replica_regions` into that deployment
machinery. **The immediate next change is the peer-forwarding path and scoped
discovery in the implementation contract above.** The continuous capacity plan
below follows that work; it must not delay proving regional traffic and drain.

| Part | Concrete change |
| --- | --- |
| Desired state | Extend the implemented concise service JSON and `replicaRegions` representation with the proposed per-region runtime/resource overrides; operators do not maintain separate forms |
| Reconciliation | Generalize the existing rollout-time regional coverage checks into continuous reconciliation: compare ready and pending replicas against those slots, request only missing capacity in the required region through Cloud, and use existing rolling-deployment ownership for retries |
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

The immediate app-lb slice consumes the shared regional decision and implements
the cross-region request path. Phase 2 subsequently changes how weights and extra
replicas are calculated, without replacing the ownership model. The JSON fragment
specifies proposed regional extensions, not the currently deployed wire schema.

## Scope and deployment constraints

The topology statements below are the historical planning snapshot used when this
proposal was drafted, not a verification of current live deployment state:

- The initial staging topology was eu1 and us3; production/us1 was out of scope.
- The plan kept `stage.heyo.computer` entering through eu1; no DNS change was assumed.
- The plan reused the us3 gateway installation and `us3.heyo.computer` address,
  subject to live TLS, authentication, and reachability verification before use.
- The plan retained eu1's libvirt workloads and used Firecracker on us3. Routing
  consumes service endpoints, not hypervisor-specific VM addresses.
- At drafting time, the inventory recorded five database VMs on us3; this PR does
  not re-verify that count. Application evacuation does not evacuate, restart, or
  migrate databases. The production application writer remains on us3; no writer
  failover or application-level writer drain is claimed or verified here. Runtime
  updates must independently prove that database VMs and their network paths remain
  uninterrupted.
- Reuse existing installations. This proposal authorizes no infrastructure
  changes, deployments, cleanup, or database writes.

Two hosts can support planned application maintenance if either has sufficient
capacity. They do not provide complete host-failure resilience: eu1 remains the
public ingress dependency and us3 remains the shared database dependency.

## Existing foundations and gaps

Verified against the repository when preparing this proposal:

| Existing source | Reuse | Missing capability |
| --- | --- | --- |
| [Service deployment](../orchestrator/src/handlers/service_deploy.rs) | `desiredReplicas`, `replicaRegions`, region-preserving replacement, rollout ownership, and final regional-coverage verification | Continuous regional minimum/capacity reconciliation and a coordinated regional maintenance barrier |
| [Service specification](../orchestrator/src/handlers/service_spec.rs) | app-lb-style `.heyo/services` JSON translated into service deployment requests | Proposed per-region runtime/resource overrides and traffic weights |
| [Service discovery](../orchestrator/src/handlers/service_discovery.rs) | PostgreSQL-backed endpoint sets, versions, region, health and draining | Gateway registration, coherent regional assignments and consumer observations |
| [app-lb discovery](../app-lb/src/discovery.rs) | Polling, version comparison, retaining the last good upstream set | Parser drops endpoint region; upstream conversion accepts only plaintext, pathless HTTP with explicit port |
| [app-lb registry](../app-lb/src/registry.rs) | Atomic local snapshots and local JSON persistence | Local files are not an authoritative shared routing store |
| [app-lb selection](../app-lb/src/deployment.rs) | Least-in-flight local backend selection; durable static-upstream cordon state and atomic in-flight admission/drain tracking | Regional selection and coordinated load feedback |

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

Use the shared gateway-role authentication over HTTPS specified in the
implementation contract, and validate destination/service scope. Strip
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

The historical topology described above could not survive complete eu1 loss for the
staging hostname or complete us3 database loss. Current live topology requires fresh
verification. External ingress/DNS failover and database availability are separately
reviewed projects, not promises delivered by this routing protocol.

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
