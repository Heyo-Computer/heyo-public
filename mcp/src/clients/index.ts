/**
 * Thin typed accessors over the four HTTP APIs.
 *
 * Deliberately not a full model of each response: these services evolve, and a
 * client that parses every field breaks on additions it does not care about.
 * Tools reach into what they need and pass the rest through.
 */

import { bind, request, type Requester, type ServiceSource } from "../http.js";
import type { Config, ServiceConfig } from "../config.js";

export interface Clients {
  /** heyo cloud — sandboxes, archives, daemons. */
  cloud: Requester;
  applb: Requester;
  obs: Requester;
  ci: Requester;
  /**
   * The managed namespace app-lb calls are confined to, discovering it if that
   * has not happened yet. Tools that must *name* the namespace rather than just
   * reach it — the feed is served at `/feeds/:namespace` — ask here instead of
   * re-deriving it, so one discovery serves them all. `undefined` for a
   * self-hosted app-lb, which has no namespace.
   */
  applbNamespace: () => Promise<string | undefined>;
}

export const CLOUD_SERVICE = "heyo cloud";

export function makeClients(config: Config): Clients {
  // Cloud with no credential at all is not a usable service: unlike app-lb's
  // self-hosted shape, there is no unauthenticated cloud to talk to. Say so as
  // "not configured" rather than letting every call come back 401.
  const cloud = config.cloud?.auth ? config.cloud : undefined;
  const applb = applbSource(config);
  return {
    cloud: bind(CLOUD_SERVICE, cloud, "HEYO_API_KEY", config),
    applb: bind("app-lb", applb, "APPLB_URL or APPLB_TOKEN", config),
    obs: bind("app-obs", config.obs, "APP_OBS_URL", config),
    ci: bind("ci", config.ci, "CI_URL", config),
    applbNamespace: async () =>
      typeof applb === "function" ? (await applb()).namespace : applb?.namespace,
  };
}

/**
 * app-lb's base, resolving the namespace on first use when none was named.
 *
 * The managed door is `/namespaces/{ns}/lb`, so a namespace is not optional —
 * but making the operator supply one when their key reaches exactly one is
 * asking them to repeat something the key already knows. So this reads
 * `GET /namespaces`, and:
 *
 * - one namespace is the answer, and the common case;
 * - several is genuinely ambiguous, and picking one would silently point every
 *   tool at the wrong room, so it fails naming them;
 * - none means there is nothing to point at yet, and says how to make one.
 *
 * Memoized across calls but **not** across failures: a namespace created a
 * minute after the first attempt should work without a restart.
 */
function applbSource(config: Config): ServiceSource | undefined {
  const cfg = config.applb;
  if (!cfg) return undefined;
  if (!cfg.discoverNamespace) return cfg;

  let pending: Promise<ServiceConfig> | undefined;
  return () => {
    pending ??= discoverNamespace(cfg, config).catch((e) => {
      pending = undefined;
      throw e;
    });
    return pending;
  };
}

interface NamespaceRow {
  name?: unknown;
  scope?: unknown;
}

async function discoverNamespace(cfg: ServiceConfig, config: Config): Promise<ServiceConfig> {
  const body = await request(CLOUD_SERVICE, cfg, config.timeoutMs, { path: "/namespaces" });
  const rows: NamespaceRow[] = Array.isArray(body)
    ? body
    : Array.isArray((body as { namespaces?: unknown })?.namespaces)
      ? ((body as { namespaces: NamespaceRow[] }).namespaces)
      : [];
  const names = rows
    .map((r) => (typeof r?.name === "string" ? r.name : undefined))
    .filter((n): n is string => !!n);

  if (names.length === 1) {
    return {
      ...cfg,
      baseUrl: `${cfg.baseUrl}/namespaces/${encodeURIComponent(names[0]!)}/lb`,
      namespace: names[0],
      discoverNamespace: false,
    };
  }
  if (names.length === 0) {
    throw new Error(
      "This key reaches no app-lb namespace, so there is no managed app-lb to " +
        "address. Create one — `heyo_request POST /namespaces {\"name\":\"…\"}`, the " +
        "SDK's Namespaces.create, or the dashboard — or set APPLB_URL to a " +
        "self-hosted app-lb's own admin listener. Sandbox tools do not need this " +
        "and are unaffected.",
    );
  }
  throw new Error(
    `This key reaches ${names.length} app-lb namespaces (${names.join(", ")}), so ` +
      "which one the app-lb tools mean cannot be inferred. Set APPLB_NAMESPACE to " +
      "one of them.",
  );
}

/**
 * Run several reads and keep the failures as values.
 *
 * A cross-service tool must not lose every answer because one service is down —
 * "app-lb says X, app-obs is unreachable" is a diagnosis; a single thrown error
 * is not. This is what lets the tools below join four services without making
 * the weakest one fatal.
 */
export async function settle<T extends Record<string, Promise<unknown>>>(
  jobs: T,
): Promise<{ [K in keyof T]: { ok: true; value: unknown } | { ok: false; error: string } }> {
  const keys = Object.keys(jobs) as (keyof T)[];
  const results = await Promise.allSettled(keys.map((k) => jobs[k]));
  const out = {} as { [K in keyof T]: { ok: true; value: unknown } | { ok: false; error: string } };
  keys.forEach((k, i) => {
    const r = results[i]!;
    out[k] =
      r.status === "fulfilled"
        ? { ok: true, value: r.value }
        : { ok: false, error: r.reason instanceof Error ? r.reason.message : String(r.reason) };
  });
  return out;
}
