// The contract with app-lb.
//
// `testdata/wire/*.json` is written by app-lb's own response types (see
// `src/wire_golden.rs`). This asserts every key in them is one this package
// declares — because to a JS client an unknown field and an absent one look
// identical, which is exactly how five fields went missing from the Rust client
// without a single test failing.
//
// A field app-lb starts sending fails this test instead of silently going
// unread.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const WIRE = join(here, "..", "..", "..", "testdata", "wire");

/**
 * Keys this package understands, per fixture. Hand-written on purpose: it is the
 * declaration that has to be checked, and generating it from the types would
 * only prove the generator agrees with itself.
 */
const KNOWN = {
  common: {
    DeploymentStatus: ["spec", "kind", "desired_replicas", "ready", "pending", "total_in_flight", "vms"],
    VmStatus: ["sandbox_id", "addr", "in_flight", "healthy", "draining"],
    DeploymentSpec: ["id", "routes", "vm", "scaling", "health", "upstreams", "build", "artifact", "site", "update", "auth"],
    RouteRule: ["host", "host_suffix", "path_prefix"],
    VmSpec: ["driver", "image", "port", "start_command", "size_class", "disk_size_gb", "working_directory", "env_vars", "setup_hooks", "open_ports", "ttl_seconds"],
    ScalingPolicy: ["min_replicas", "max_replicas", "warm_pool", "target_concurrency", "scale_to_zero_after_secs", "cold_start_timeout_secs", "drain_timeout_secs", "boot_timeout_secs", "idle_action"],
    HealthCheck: ["path", "port", "timeout_secs"],
    BuildSpec: ["repo", "ref", "dockerfile", "context", "image_name", "image_size_mb", "auth"],
    ArtifactSpec: ["store", "ref", "auth", "grow_gb", "image_name"],
    SiteSpec: ["root", "index", "not_found", "spa", "cache_control"],
    UpdateSpec: ["working_dir", "commands", "env", "env_from", "auth", "timeout_secs", "verify_timeout_secs"],
    SecretEnv: ["secret", "key", "as"],
    SecretRef: ["secret", "key", "username"],
    AuthGate: ["provider", "client_id", "client_secret", "allowed_domains", "allowed_emails", "public_paths", "base_path", "session_ttl_secs", "cookie_name", "redirect_url", "forward_identity"],
    DeploymentView: ["id", "kind", "upstreams", "hosts", "urls", "site_root", "site_spa", "job_kind", "pool", "vms", "pending_vms", "metrics"],
    PoolStatus: ["desired_replicas", "ready", "draining", "pending", "total_in_flight", "target_concurrency", "min_replicas", "max_replicas", "warm_pool", "utilization", "cpu_percent", "memory_bytes", "boot_timeout_secs", "cold_start_timeout_secs"],
    VmView: ["sandbox_id", "addr", "in_flight", "healthy", "draining", "uptime_secs", "cpu_percent", "memory_bytes"],
    PendingVmView: ["sandbox_id", "age_secs", "status"],
    DeploymentMetrics: ["requests", "latency_ms", "cold_start_s", "autoscale"],
    StatusCounts: ["total", "c2xx", "c3xx", "c4xx", "c5xx", "errors"],
    Histogram: ["count", "sum", "mean", "p50", "p90", "p99", "buckets"],
    Bucket: ["le", "count"],
    AutoscaleCounts: ["vms_created", "vms_drained", "vms_reaped", "scale_up_events", "scale_down_events", "cold_start_waits", "cold_start_hits", "cold_start_timeouts", "boot_timeouts"],
    HostUsage: ["available", "cpu_count", "cpu_percent", "memory_total_bytes", "memory_used_bytes", "sampled_at_ms"],
    FleetPool: ["deployments", "ready", "draining", "pending", "total_in_flight"],
    ObsStats: ["queued", "dropped", "shipped", "failed", "healthy"],
    MetricsResponse: ["generated_at", "uptime_secs", "host", "fleet", "global", "obs", "deployments", "matched", "tracked_deployments"],
    JobRecord: ["id", "deployment", "kind", "status", "started_at", "finished_at", "error", "log", "repo", "ref", "commit", "dockerfile", "image", "rolled_out", "store", "artifact", "digest", "bytes", "reused", "working_dir", "commands_total", "commands_run", "verified"],
    SecretSummary: ["id", "description", "keys", "updated_at", "encrypted_at_rest"],
    CertStatus: ["host", "not_after", "issuer", "needs_renewal"],
    ExecOutput: ["sandbox_id", "exit_code", "stdout", "stderr", "output"],
    EvictOutcome: ["sandbox_id", "outcome"],
    TokenSummary: ["id", "name", "admin", "deployments", "created_at", "expires_at", "last_used_at"],
    MintedToken: ["id", "name", "admin", "deployments", "created_at", "expires_at", "last_used_at", "token"],
    ApiError: ["error"],
  },
};

/** Which declaration each fixture is an instance of. */
const FIXTURES = {
  "deployment-status-vm": "DeploymentStatus",
  "deployment-status-site": "DeploymentStatus",
  "deployment-status-static": "DeploymentStatus",
  "deployment-status-artifact": "DeploymentStatus",
  "deployment-view-site": "DeploymentView",
  "metrics-response": "MetricsResponse",
  "job-build": "JobRecord",
  "job-pull": "JobRecord",
  "job-update": "JobRecord",
  "job-failed": "JobRecord",
  "secret-summary": "SecretSummary",
  "cert-status": "CertStatus",
  "exec-response": "ExecOutput",
  "evict-response": "EvictOutcome",
  "api-error": "ApiError",
  "token-summary": "TokenSummary",
  "token-summary-scoped": "TokenSummary",
  "minted-token": "MintedToken",
};

/** Which declaration governs a nested object, by the key that holds it. */
const NESTED = {
  spec: "DeploymentSpec", vm: "VmSpec", scaling: "ScalingPolicy", health: "HealthCheck",
  build: "BuildSpec", artifact: "ArtifactSpec", site: "SiteSpec", update: "UpdateSpec",
  auth: "AuthGate", client_secret: "SecretRef", pool: "PoolStatus", host: "HostUsage",
  fleet: "FleetPool", obs: "ObsStats", metrics: "DeploymentMetrics",
  global: "DeploymentMetrics", requests: "StatusCounts", latency_ms: "Histogram",
  cold_start_s: "Histogram", autoscale: "AutoscaleCounts",
};

/** Which declaration governs the *elements* of an array, by its key. */
const ELEMENTS = {
  routes: "RouteRule", vms: "VmStatus", deployments: "DeploymentView",
  pending_vms: "PendingVmView", buckets: "Bucket", env_from: "SecretEnv",
};

const K = KNOWN.common;

function check(value, declName, path, unknown) {
  const known = K[declName];
  assert.ok(known, `no declaration named ${declName} (at ${path})`);
  for (const [key, child] of Object.entries(value)) {
    if (!known.includes(key)) {
      unknown.push(`${path}.${key}`);
      continue;
    }
    if (child && typeof child === "object" && !Array.isArray(child)) {
      // Two keys mean different things depending on where they sit, so the
      // declaration we are inside disambiguates them: `vms` is VmStatus under a
      // DeploymentStatus and VmView under a DeploymentView, and `auth` is the
      // sign-in gate on a spec but a secret *reference* on a build, an artifact
      // or an update.
      let nested = NESTED[key];
      if (key === "vms") nested = declName === "DeploymentView" ? "VmView" : "VmStatus";
      if (key === "auth") nested = declName === "DeploymentSpec" ? "AuthGate" : "SecretRef";
      if (nested) check(child, nested, `${path}.${key}`, unknown);
    } else if (Array.isArray(child)) {
      let elem = ELEMENTS[key];
      if (key === "vms") elem = declName === "DeploymentView" ? "VmView" : "VmStatus";
      if (elem) {
        child.forEach((item, i) => {
          if (item && typeof item === "object") check(item, elem, `${path}.${key}[${i}]`, unknown);
        });
      }
    }
  }
}

const files = readdirSync(WIRE).filter((f) => f.endsWith(".json"));

test("every wire fixture is covered by a declaration", () => {
  const covered = files.map((f) => f.replace(/\.json$/, ""));
  const missing = covered.filter((n) => !(n in FIXTURES));
  assert.deepEqual(missing, [], "app-lb writes fixtures this package does not check");
});

for (const file of files) {
  const name = file.replace(/\.json$/, "");
  if (!(name in FIXTURES)) continue;
  test(`${name} has no fields this package would silently drop`, () => {
    const value = JSON.parse(readFileSync(join(WIRE, file), "utf8"));
    const unknown = [];
    check(value, FIXTURES[name], name, unknown);
    assert.deepEqual(
      unknown,
      [],
      `app-lb sends fields this package does not declare — add them to src/types.ts:\n  ${unknown.join("\n  ")}`,
    );
  });
}
