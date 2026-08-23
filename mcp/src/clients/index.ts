/**
 * Thin typed accessors over the three HTTP APIs.
 *
 * Deliberately not a full model of each response: these services evolve, and a
 * client that parses every field breaks on additions it does not care about.
 * Tools reach into what they need and pass the rest through.
 */

import { bind, type Requester } from "../http.js";
import type { Config } from "../config.js";

export interface Clients {
  applb: Requester;
  obs: Requester;
  ci: Requester;
}

export function makeClients(config: Config): Clients {
  return {
    applb: bind("app-lb", config.applb, "APPLB_URL", config),
    obs: bind("app-obs", config.obs, "APP_OBS_URL", config),
    ci: bind("ci", config.ci, "CI_URL", config),
  };
}

/**
 * Run several reads and keep the failures as values.
 *
 * A cross-service tool must not lose every answer because one service is down —
 * "app-lb says X, app-obs is unreachable" is a diagnosis; a single thrown error
 * is not. This is what lets the tools below join three services without making
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
