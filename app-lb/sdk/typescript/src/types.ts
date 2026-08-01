/**
 * The admin API's JSON, as types.
 *
 * Every field is optional in practice — app-lb omits what is absent and adds
 * fields over time — so these describe what you *may* find rather than what is
 * guaranteed. The runtime never validates them: an unknown field is carried
 * through untouched, which is what makes a client one version behind still work.
 *
 * `test/wire-contract.test.ts` reads `testdata/wire/*.json`, written by app-lb's
 * own response types, and asserts every key in them is declared here. A field
 * app-lb starts sending fails that test instead of silently going unread.
 */

export type DeploymentKind = "vm" | "static" | "site";

export interface RouteRule {
  host?: string;
  host_suffix?: string;
  path_prefix?: string;
}

export interface ScalingPolicy {
  min_replicas?: number;
  max_replicas?: number;
  warm_pool?: number;
  target_concurrency?: number;
  scale_to_zero_after_secs?: number;
  cold_start_timeout_secs?: number;
  drain_timeout_secs?: number;
  /** `destroy` kills an idle VM; `retain` stops it, keeping its data disk. */
  idle_action?: "destroy" | "retain";
  boot_timeout_secs?: number;
}

export interface HealthCheck {
  /** `null` means a bare TCP connect rather than an HTTP probe. */
  path?: string | null;
  port?: number;
  timeout_secs?: number;
}

export interface VmSpec {
  driver: "firecracker" | "kvm" | "libvirt";
  image?: string;
  port: number;
  start_command?: string;
  size_class?: "micro" | "mini" | "small" | "medium" | "large" | "xlarge";
  disk_size_gb?: number;
  working_directory?: string;
  env_vars?: Record<string, string>;
  setup_hooks?: string[];
  open_ports?: number[];
  ttl_seconds?: number;
}

export interface SecretRef {
  secret: string;
  key?: string;
  username?: string;
}

export interface BuildSpec {
  repo: string;
  ref?: string;
  dockerfile?: string;
  context?: string;
  image_name?: string;
  image_size_mb?: number;
  auth?: SecretRef;
}

export interface ArtifactSpec {
  store: string;
  ref: string;
  auth?: SecretRef;
  grow_gb?: number;
  image_name?: string;
}

export interface SiteSpec {
  root: string;
  index?: string;
  not_found?: string;
  spa?: boolean;
  cache_control?: string;
}

export interface SecretEnv {
  secret: string;
  key?: string;
  as?: string;
}

export interface UpdateSpec {
  working_dir: string;
  commands: string[];
  env?: Record<string, string>;
  env_from?: SecretEnv[];
  auth?: SecretRef;
  timeout_secs?: number;
  verify_timeout_secs?: number;
}

/**
 * `provider` is a bare string for one and an array for several — they are
 * alternatives, so any one of them admits a request. A gate written before
 * app-tokens existed omits it entirely and means `"google"`.
 */
export interface AuthGate {
  provider?: AuthProvider | AuthProvider[];
  /** Required for `google`, meaningless without it. */
  client_id?: string;
  client_secret?: SecretRef;
  allowed_domains?: string[];
  allowed_emails?: string[];
  public_paths?: string[];
  base_path?: string;
  session_ttl_secs?: number;
  cookie_name?: string;
  redirect_url?: string;
  forward_identity?: boolean;
}

export type AuthProvider = "google" | "app-token";

export interface DeploymentSpec {
  id: string;
  routes: RouteRule[];
  vm?: VmSpec;
  scaling?: ScalingPolicy;
  health?: HealthCheck;
  upstreams?: string[];
  build?: BuildSpec;
  artifact?: ArtifactSpec;
  site?: SiteSpec;
  update?: UpdateSpec;
  auth?: AuthGate;
  /** Anything app-lb sent that this build has no name for. */
  [extra: string]: unknown;
}

export interface VmStatus {
  sandbox_id: string;
  addr: string;
  in_flight: number;
  healthy: boolean;
  draining: boolean;
}

export interface DeploymentStatus {
  spec: DeploymentSpec;
  kind: DeploymentKind;
  desired_replicas: number;
  ready: number;
  pending: number;
  total_in_flight: number;
  vms: VmStatus[];
}

// -- metrics ---------------------------------------------------------------

export interface StatusCounts {
  total: number;
  c2xx: number;
  c3xx: number;
  c4xx: number;
  c5xx: number;
  errors: number;
}

export interface Bucket {
  /** `null` is the `+Inf` bucket. */
  le: number | null;
  count: number;
}

export interface Histogram {
  count: number;
  sum: number;
  mean: number;
  p50: number;
  p90: number;
  p99: number;
  buckets: Bucket[];
}

export interface AutoscaleCounts {
  vms_created: number;
  vms_drained: number;
  vms_reaped: number;
  scale_up_events: number;
  scale_down_events: number;
  cold_start_waits: number;
  cold_start_hits: number;
  cold_start_timeouts: number;
  boot_timeouts: number;
}

export interface DeploymentMetrics {
  requests: StatusCounts;
  latency_ms: Histogram;
  cold_start_s: Histogram;
  autoscale: AutoscaleCounts;
}

export interface HostUsage {
  available: boolean;
  cpu_count: number;
  cpu_percent: number;
  memory_total_bytes: number;
  memory_used_bytes: number;
  sampled_at_ms: number;
}

export interface FleetPool {
  deployments: number;
  ready: number;
  draining: number;
  pending: number;
  total_in_flight: number;
}

export interface PoolStatus {
  desired_replicas: number;
  ready: number;
  draining: number;
  pending: number;
  total_in_flight: number;
  target_concurrency: number;
  min_replicas: number;
  max_replicas: number;
  warm_pool: number;
  /** `null` means no capacity to divide by — not zero load. */
  utilization: number | null;
  cpu_percent: number | null;
  memory_bytes: number | null;
  boot_timeout_secs: number;
  cold_start_timeout_secs: number;
}

export interface VmView {
  sandbox_id: string;
  addr: string;
  in_flight: number;
  healthy: boolean;
  draining: boolean;
  uptime_secs: number;
  cpu_percent: number | null;
  memory_bytes: number | null;
}

export interface PendingVmView {
  sandbox_id: string;
  age_secs: number;
  status?: string;
}

export interface DeploymentView {
  id: string;
  kind: DeploymentKind;
  upstreams: string[];
  hosts: string[];
  urls?: string[];
  site_root?: string;
  site_spa?: boolean;
  job_kind?: "build" | "update";
  pool: PoolStatus;
  vms: VmView[];
  pending_vms: PendingVmView[];
  metrics: DeploymentMetrics;
}

export interface ObsStats {
  queued: number;
  dropped: number;
  shipped: number;
  failed: number;
  healthy: boolean;
}

export interface MetricsResponse {
  generated_at: number;
  uptime_secs: number;
  host: HostUsage;
  fleet: FleetPool;
  global: DeploymentMetrics;
  obs?: ObsStats;
  /** Absent when `APP_LB_SIEM=0`. */
  security?: SecuritySummary;
  deployments: DeploymentView[];
  /** How many matched before `limit`/`offset`, so you can page. */
  matched: number;
  tracked_deployments: number;
}

// -- security --------------------------------------------------------------

/** The alert counts carried on `/metrics`, for a status tile. */
export interface SecuritySummary {
  /** Alerts currently held in app-lb's in-memory ring. */
  open: number;
  /** How many of those are `high` or `critical`. */
  urgent: number;
  /**
   * Observations dropped because the analysis queue was full. Non-zero means
   * detection is sampling rather than complete — the figure to watch, since a
   * SIEM that has stopped looking is indistinguishable from a quiet network.
   */
  dropped: number;
  /** Whether the per-source table is full, which means the same for addresses. */
  clients_at_capacity: boolean;
}

export type Severity = "info" | "low" | "medium" | "high" | "critical";

export interface SecurityAlert {
  id: number;
  /** Epoch millis of the first occurrence folded into this alert. */
  ts: number;
  last_ts: number;
  /** e.g. `auth.brute-force`, `web.sqli`, `traffic.scanner`. */
  rule: string;
  severity: Severity;
  title: string;
  client?: string;
  /** Absent for admin-plane and unrouted findings. */
  deployment?: string;
  /** Never carries a query string. */
  path?: string;
  /** MITRE ATT&CK technique id, e.g. `T1110`. */
  technique?: string;
  /**
   * Occurrences folded into this alert. A scanner produces one alert whose
   * count climbs, not ten thousand alerts.
   */
  count: number;
  /**
   * The triggering event in ECS field names (`source.ip`, `url.path`, …). A
   * free-form map: app-lb may add fields, and `url.query` is never among them.
   */
  ecs?: Record<string, unknown>;
}

export interface SeverityTotals {
  info: number;
  low: number;
  medium: number;
  high: number;
  critical: number;
}

export interface SiemStats {
  observed: number;
  dropped: number;
  analyzed: number;
  raised: number;
  suppressed: number;
  tracked_clients: number;
  clients_at_capacity: boolean;
}

/** `GET /security`. */
export interface SecurityResponse {
  generated_at: number;
  /** `false` with an empty list when `APP_LB_SIEM=0` — not a 404. */
  enabled: boolean;
  /** Seconds each rate-based rule counts over. */
  window_secs: number;
  /** Newest first. */
  alerts: SecurityAlert[];
  totals: SeverityTotals;
  /** Withheld from a deployment-scoped token, for whom it would describe
   * traffic it cannot see. */
  stats?: SiemStats;
}

// -- jobs, secrets, certs --------------------------------------------------

export type JobKind = "image-build" | "artifact-pull" | "host-update";
export type JobStatus = "running" | "succeeded" | "failed";

export interface JobRecord {
  id: string;
  deployment: string;
  kind: JobKind;
  status: JobStatus;
  started_at: number;
  finished_at?: number | null;
  error?: string | null;
  log: string[];
  repo?: string;
  ref?: string;
  commit?: string;
  dockerfile?: string;
  image?: string;
  rolled_out?: boolean;
  store?: string;
  artifact?: string;
  digest?: string;
  bytes?: number;
  reused?: boolean;
  working_dir?: string;
  commands_total?: number;
  commands_run?: number;
  verified?: boolean;
}

export interface SecretSummary {
  id: string;
  description: string | null;
  /** Key *names*. Values never leave app-lb. */
  keys: string[];
  updated_at: number;
  encrypted_at_rest: boolean;
}

export interface CertStatus {
  host: string;
  not_after: string;
  issuer: string;
  needs_renewal: boolean;
}

export interface ExecOutput {
  sandbox_id: string;
  exit_code: number;
  stdout: string;
  stderr: string;
  /** stdout and stderr interleaved as the guest wrote them. */
  output: string;
}

export interface EvictOutcome {
  sandbox_id: string;
  /** `killed` (immediate) or `draining` (still serving what it has). */
  outcome: "killed" | "draining";
}

// -- app-tokens ------------------------------------------------------------

/** What a token may do on the admin API. */
export type AdminScope = "none" | "view" | "admin";

export interface TokenSummary {
  id: string;
  name: string;
  admin: AdminScope;
  /** Deployment ids, or `["*"]` for all of them. */
  deployments: string[];
  created_at: number;
  expires_at?: number;
  /**
   * `undefined` also means "not used since the store was last written" — the
   * stamp is flushed opportunistically, not per request.
   */
  last_used_at?: number;
}

/**
 * The reply to a mint. **`token` is the only time the secret is returned** —
 * app-lb stores only its hash and no endpoint reads it back.
 */
export interface MintedToken extends TokenSummary {
  token: string;
}
