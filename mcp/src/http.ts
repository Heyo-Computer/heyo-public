/**
 * One HTTP helper for all three services.
 *
 * Two decisions worth naming, both learned from debugging these services rather
 * than guessed:
 *
 * **Every request is bounded.** A service that accepts a connection and then
 * answers nothing is the failure mode this fleet actually has — a tunnel whose
 * QUIC connection is up and whose data path is dead, a database behind a pool
 * that suspends idle instances. An unbounded fetch turns that into a tool call
 * that never returns.
 *
 * **The body is kept on failure.** These APIs put the useful sentence in the
 * response body, and a bare status code discards exactly the part worth
 * reading.
 */

import type { Config, ServiceConfig } from "./config.js";
import { CI_GATE_HINT, CLOUD_CAPACITY_HINT } from "./config.js";

export class ServiceError extends Error {
  constructor(
    readonly service: string,
    readonly status: number,
    readonly path: string,
    readonly body: string,
    hint?: string,
  ) {
    const detail = body.trim().slice(0, 600);
    super(
      `${service} ${status} on ${path}` +
        (detail ? `: ${detail}` : "") +
        (hint ? `\n\n${hint}` : ""),
    );
    this.name = "ServiceError";
  }
}

export class NotConfigured extends Error {
  constructor(service: string, envVar: string) {
    super(
      `${service} is not configured — set ${envVar}. ` +
        `Run the \`heyo_status\` tool to see which services are reachable.`,
    );
    this.name = "NotConfigured";
  }
}

export interface RequestOptions {
  method?: string;
  path: string;
  query?: Record<string, string | number | undefined>;
  body?: unknown;
}

function withQuery(path: string, query?: RequestOptions["query"]): string {
  if (!query) return path;
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined && v !== "") params.set(k, String(v));
  }
  const qs = params.toString();
  return qs ? `${path}${path.includes("?") ? "&" : "?"}${qs}` : path;
}

export async function request(
  service: string,
  cfg: ServiceConfig,
  timeoutMs: number,
  opts: RequestOptions,
): Promise<unknown> {
  const path = withQuery(opts.path, opts.query);
  const headers: Record<string, string> = { accept: "application/json" };
  if (cfg.auth) headers.authorization = cfg.auth;
  if (opts.body !== undefined) headers["content-type"] = "application/json";

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  let res: Response;
  try {
    res = await fetch(`${cfg.baseUrl}${path}`, {
      method: opts.method ?? "GET",
      headers,
      body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
      signal: controller.signal,
    });
  } catch (e) {
    const reason = e instanceof Error && e.name === "AbortError"
      ? `no response within ${timeoutMs}ms — the service accepted the connection and did not answer`
      : e instanceof Error
        ? e.message
        : String(e);
    throw new Error(`${service} ${path}: ${reason}`);
  } finally {
    clearTimeout(timer);
  }

  const text = await res.text();
  if (!res.ok) {
    // Two statuses mean something more specific than they look, and both are
    // read wrong by default: a 401 from ci is the documented gate behaviour far
    // more often than it is a bad token, and a 503 from cloud is region
    // capacity rather than a fault. Attaching the sentence here means every
    // call site carries it, including the ones that only pass a body through.
    const hint =
      service === "ci" && res.status === 401
        ? CI_GATE_HINT
        : service === "heyo cloud" && res.status === 503
          ? CLOUD_CAPACITY_HINT
          : undefined;
    throw new ServiceError(service, res.status, path, text, hint);
  }
  if (!text.trim()) return null;
  try {
    return JSON.parse(text);
  } catch {
    // /metrics and some app-lb consoles answer text, not JSON. Returning the
    // string is more useful than failing on a successful response.
    return text;
  }
}

/**
 * A service's address, either known now or resolvable later.
 *
 * app-lb's managed base is the second case: reaching it means naming a
 * namespace, and a namespace that was not configured has to be read from the
 * key — a network call, which config loading is not allowed to make. The
 * resolver is called on each request and is expected to memoize itself.
 */
export type ServiceSource = ServiceConfig | (() => Promise<ServiceConfig>);

/** Bound a service to the config, so tools do not each re-check it. */
export function bind(service: string, cfg: ServiceSource | undefined, envVar: string, config: Config) {
  return async (opts: RequestOptions): Promise<unknown> => {
    if (!cfg) throw new NotConfigured(service, envVar);
    const resolved = typeof cfg === "function" ? await cfg() : cfg;
    return request(service, resolved, config.timeoutMs, opts);
  };
}

export type Requester = ReturnType<typeof bind>;
