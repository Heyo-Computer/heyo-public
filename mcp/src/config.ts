/**
 * Where the three services are, and what proves us to them.
 *
 * All three speak `Authorization: Bearer`, so this is mostly uniform. The one
 * asymmetry is `ci`, and it is load-bearing enough to state here rather than
 * leave to a 401: **`ci` behind an app-lb gate admits browsers and nothing
 * else.** The gate splits on `Accept: text/html`, and only `/healthz`,
 * `/api/submit`, `/api/stream/` and `/__ui/` are in its `public_paths`. Every
 * page worth reading — runs, jobs, networks, runners, vms, repos — is outside
 * that list, so a token-carrying client is refused no matter which token it
 * carries.
 *
 * `CI_URL` therefore wants ci's own listener, not its public hostname: reach it
 * from the box it runs on, or through an SSH tunnel. Pointing it at the gated
 * host is a supported mistake — {@link ciGateRefusal} turns the resulting 401
 * into the sentence above instead of an authentication red herring.
 */

export interface ServiceConfig {
  readonly baseUrl: string;
  /** `Authorization` header value, already assembled, or undefined. */
  readonly auth?: string;
  /**
   * The managed namespace this app-lb config is confined to, when it reaches
   * app-lb through heyo cloud's `/namespaces/{ns}/lb` door rather than an
   * admin listener. Informational — the base URL already carries it.
   */
  readonly namespace?: string;
}

export interface Config {
  readonly applb?: ServiceConfig;
  readonly obs?: ServiceConfig;
  readonly ci?: ServiceConfig;
  readonly timeoutMs: number;
}

function trimUrl(raw: string): string {
  return raw.trim().replace(/\/+$/, "");
}

/**
 * Bearer, or Basic when a username:password pair is given instead.
 *
 * app-lb compares a Basic header **byte for byte** against a value it was
 * configured with, so it is passed through exactly as supplied rather than
 * re-encoded — an equivalent-but-differently-encoded header is rejected.
 */
function authHeader(token?: string, basic?: string): string | undefined {
  if (basic) {
    return basic.startsWith("Basic ") ? basic : `Basic ${Buffer.from(basic).toString("base64")}`;
  }
  if (token) return token.startsWith("Bearer ") ? token : `Bearer ${token}`;
  return undefined;
}

function service(url?: string, token?: string, basic?: string): ServiceConfig | undefined {
  if (!url || !url.trim()) return undefined;
  return { baseUrl: trimUrl(url), auth: authHeader(token, basic) };
}

/**
 * Where app-lb is, in either of its two shapes.
 *
 * Self-hosted, `APPLB_URL` is app-lb's own admin listener and every path is
 * appended to it. Managed, the same tools reach the platform's single app-lb
 * through heyo cloud, which exposes it per namespace at
 * `/namespaces/{ns}/lb/…` and forwards the caller's `heyo_api_*` key to be
 * resolved into that namespace's grant. Nothing downstream knows which shape it
 * is talking to: the difference is entirely in the base URL, which is why this
 * is the only place that mentions it.
 *
 * `APPLB_NAMESPACE` is what selects the managed shape. A URL that already ends
 * in `/lb` is taken as spelled out by hand (`…/namespaces/team-a/lb`) and is
 * not rewritten, so the two ways of saying it cannot compound into
 * `…/lb/namespaces/team-a/lb`.
 */
export function applbService(
  url?: string,
  namespace?: string,
  token?: string,
  basic?: string,
): ServiceConfig | undefined {
  const base = service(url, token, basic);
  if (!base) return undefined;
  const ns = namespace?.trim();
  if (!ns) return base;
  if (base.baseUrl.endsWith("/lb")) return { ...base, namespace: ns };
  return {
    ...base,
    baseUrl: `${base.baseUrl}/namespaces/${encodeURIComponent(ns)}/lb`,
    namespace: ns,
  };
}

/**
 * The config to serve one HTTP request with.
 *
 * When app-lb is configured without a credential of its own, the caller's
 * `Authorization` header is forwarded instead — so one hosted instance serves
 * every tenant, each under their own key and therefore their own namespace
 * grant. A configured `APPLB_TOKEN`/`APPLB_BASIC` always wins: that instance
 * was deployed to act as itself, and a caller's header must not be able to
 * change who it acts as. Returns the same object when nothing applies, so the
 * per-process tool set can be reused.
 */
export function withForwardedAuth(
  config: Config,
  headers: Record<string, string | string[] | undefined>,
): Config {
  if (!config.applb || config.applb.auth) return config;
  const raw = headers["authorization"];
  const value = Array.isArray(raw) ? raw[0] : raw;
  if (!value || !value.trim()) return config;
  return { ...config, applb: { ...config.applb, auth: value } };
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  const timeout = Number(env.HEYO_MCP_TIMEOUT_MS ?? "30000");
  return {
    applb: applbService(env.APPLB_URL, env.APPLB_NAMESPACE, env.APPLB_TOKEN, env.APPLB_BASIC),
    obs: service(env.APP_OBS_URL, env.APP_OBS_API_TOKEN),
    ci: service(env.CI_URL, env.CI_TOKEN),
    // Generous, but bounded. Every call here is a diagnostic and a hung one is
    // worse than a failed one: it stalls the conversation with no output at all.
    timeoutMs: Number.isFinite(timeout) && timeout > 0 ? timeout : 30_000,
  };
}

/** Which services are usable, for the startup banner and for `heyo_status`. */
export function configured(config: Config): string[] {
  const on: string[] = [];
  if (config.applb) {
    on.push(config.applb.namespace ? `app-lb (namespace ${config.applb.namespace})` : "app-lb");
  }
  if (config.obs) on.push("app-obs");
  if (config.ci) on.push("ci");
  return on;
}

export const CI_GATE_HINT =
  "ci returned 401 for a machine request. If CI_URL points at a gated host " +
  "(ci.us2.heyo.work), that is expected and no token will fix it: app-lb's gate " +
  "admits browsers only, and ci's public_paths list just /healthz, /api/submit, " +
  "/api/stream/ and /__ui/. Point CI_URL at ci's own listener instead — from the " +
  "host it runs on, or through an SSH tunnel.";
