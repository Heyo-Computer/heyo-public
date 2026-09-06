/**
 * Where the services are, and what proves us to them.
 *
 * There are four: **heyo cloud** (the sandbox control plane — create a VM, run
 * a command in it, read and write its files), and the three that answer
 * operational questions about a fleet — app-lb, app-obs and ci. All of them
 * speak `Authorization: Bearer`, so this is mostly uniform. Two asymmetries are
 * load-bearing enough to state here rather than leave to a 401.
 *
 * **The minimal configuration is two API keys and nothing else.**
 * `HEYO_API_KEY` reaches cloud at its default base, and `APPLB_TOKEN` reaches
 * the managed app-lb through cloud's per-namespace door — whose namespace is
 * discovered from the key when it is not named (see `clients/index.ts`). Every
 * URL below has a working default or is genuinely optional; a self-hosted
 * app-lb, app-obs or ci is the case that needs one. Through cloud's door the
 * app-lb credential *is* a heyo API key, so a single `HEYO_API_KEY` configures
 * both — `APPLB_TOKEN` exists for the deployment that wants them separate, and
 * for a self-hosted app-lb where they are genuinely different credentials.
 *
 * **`ci` behind an app-lb gate admits browsers and nothing else.** The gate
 * splits on `Accept: text/html`, and only `/healthz`, `/api/submit`,
 * `/api/stream/` and `/__ui/` are in its `public_paths`. Every page worth
 * reading — runs, jobs, networks, runners, vms, repos — is outside that list,
 * so a token-carrying client is refused no matter which token it carries.
 * `CI_URL` therefore wants ci's own listener, not its public hostname: reach it
 * from the box it runs on, or through an SSH tunnel. Pointing it at the gated
 * host is a supported mistake — {@link CI_GATE_HINT} turns the resulting 401
 * into that sentence instead of an authentication red herring.
 */

/** Cloud's public base. The default for both cloud and the managed app-lb. */
export const CLOUD_BASE_URL = "https://server.heyo.computer";

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
  /**
   * Set when `baseUrl` is cloud's root and no namespace was named: the door is
   * not yet complete, and the first app-lb call resolves it from the key's own
   * namespace list. Kept as a flag rather than resolved here because that is a
   * network call and this function is not allowed to be one.
   */
  readonly discoverNamespace?: boolean;
}

export interface Config {
  readonly cloud?: ServiceConfig;
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
 * Where heyo cloud is. `HEYO_BASE_URL` exists for a staging or local cloud;
 * everyone else gets {@link CLOUD_BASE_URL} without saying so.
 *
 * Unlike the three fleet services, an absent key does not make this
 * unconfigured in every mode: a hosted instance may carry no key of its own and
 * act as whoever calls it, exactly as app-lb does below. What makes cloud
 * unconfigured is having neither a key nor a caller to borrow one from, which
 * only {@link withForwardedAuth} can decide.
 */
export function cloudService(url?: string, apiKey?: string): ServiceConfig {
  return { baseUrl: trimUrl(url?.trim() ? url : CLOUD_BASE_URL), auth: authHeader(apiKey) };
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
 * With no `APPLB_URL` at all the managed shape is assumed, because that is the
 * one a customer has: cloud's base, and the key already in hand. A URL that
 * already ends in `/lb` is taken as spelled out by hand
 * (`…/namespaces/team-a/lb`) and is not rewritten, so the two ways of saying it
 * cannot compound into `…/lb/namespaces/team-a/lb`.
 *
 * A namespace that was not named is left to be discovered rather than guessed —
 * but only against cloud, because an admin listener has no `/namespaces` to
 * ask.
 */
export function applbService(
  url?: string,
  namespace?: string,
  token?: string,
  basic?: string,
  cloud?: ServiceConfig,
): ServiceConfig | undefined {
  const explicit = service(url, token, basic);
  // No URL: the managed door, with app-lb's own credential if it has one and
  // cloud's otherwise — through that door they are the same kind of key.
  const base =
    explicit ??
    (cloud && (token?.trim() || basic?.trim() || cloud.auth)
      ? { baseUrl: cloud.baseUrl, auth: authHeader(token, basic) ?? cloud.auth }
      : undefined);
  if (!base) return undefined;

  const ns = namespace?.trim();
  if (base.baseUrl.endsWith("/lb")) return ns ? { ...base, namespace: ns } : base;
  if (!ns) {
    const cloudRooted = base.baseUrl === (cloud?.baseUrl ?? CLOUD_BASE_URL);
    return cloudRooted ? { ...base, discoverNamespace: true } : base;
  }
  return {
    ...base,
    baseUrl: `${base.baseUrl}/namespaces/${encodeURIComponent(ns)}/lb`,
    namespace: ns,
  };
}

/**
 * Prefix on a token app-lb minted for itself (`applb_<id>_<secret>`).
 *
 * The prefix is load-bearing, not cosmetic: it is what distinguishes a
 * credential **app-lb issued and will scope-check** from every other bearer a
 * caller might present — a `heyo_api_*` cloud key, or a JWT from some other
 * issuer. See {@link withForwardedAuth}.
 */
const APPLB_TOKEN_PREFIX = "applb_";

/**
 * The bearer's value, without the scheme, or undefined if it isn't a bearer.
 *
 * Looser than app-lb's own `strip_prefix("Bearer ")` — case-insensitive, any
 * run of spaces — on purpose. The two ways of being wrong here are not
 * symmetric: a header this accepts that app-lb will not parse ends in a 401,
 * while one app-lb would accept and this missed falls back to the configured
 * credential, which is exactly the escalation {@link withForwardedAuth} exists
 * to prevent. So the detector errs wide and the strict parser downstream
 * decides.
 */
function bearerToken(header: string): string | undefined {
  const match = /^Bearer[ \t]+(\S.*)$/i.exec(header.trim());
  return match?.[1]?.trim();
}

/** Whether the caller presented a token app-lb minted. */
function isApplbToken(header: string): boolean {
  return bearerToken(header)?.startsWith(APPLB_TOKEN_PREFIX) ?? false;
}

/**
 * The config to serve one HTTP request with.
 *
 * Two rules, and the second one exists because the first is not sufficient.
 *
 * **A service with no credential of its own borrows the caller's.** That is
 * what lets one hosted instance serve every tenant, each under their own key
 * and therefore their own sandboxes and namespace grant.
 *
 * **An app-lb-minted token always speaks for itself, configured credential or
 * not.** A token carries a scope — an `admin` level and a `deployments` list —
 * and app-lb enforces it on every route. Falling back to this process's own
 * `APPLB_TOKEN` for a caller who presented one would hand a token scoped to a
 * single deployment the reach of whatever fleet-wide credential the operator
 * configured. That is a confused deputy: the gate in front authenticated a
 * narrow principal and the process would then act for it with broad authority.
 * The caller's token is therefore preferred over the configured one whenever it
 * is app-lb's own kind, so a scope is never widened by passing through here.
 *
 * The prefix test is what keeps that from breaking the other gate shapes. A
 * `heyo_api_*` key or a JWT is not an app-lb token — it means nothing to
 * app-lb's admin API — so those still fall to the configured credential and a
 * JWT-gated deployment behaves exactly as before. And preferring a caller's
 * `applb_…` is never an escalation in the other direction either: it is a
 * credential they already hold, and app-lb re-checks its scope regardless of
 * who relayed it.
 *
 * Cloud is left out of the second rule and, for an app-lb token, out of the
 * first one too. An `applb_…` token is not a cloud credential: cloud has never
 * heard of it, so forwarding one could only produce a 401 on every sandbox call.
 * So a configured `HEYO_API_KEY` is kept (every caller admitted by an app-token
 * gate shares that one cloud account), and where there is no key the caller's
 * app-lb token is *not* substituted for one — cloud stays unconfigured, and
 * `buildTools` lists no sandbox tools rather than sixteen that always fail.
 *
 * Only that prefix is excluded. A `heyo_api_*` key is exactly what cloud wants
 * and is still forwarded, which is what makes managed mode multi-tenant; a JWT
 * is left alone as before.
 *
 * Returns the same object when nothing applies, so the per-process tool set can
 * be reused.
 */
export function withForwardedAuth(
  config: Config,
  headers: Record<string, string | string[] | undefined>,
): Config {
  const raw = headers["authorization"];
  const value = Array.isArray(raw) ? raw[0] : raw;
  if (!value || !value.trim()) return config;

  // An app-lb token is the one bearer that must not stand in for a cloud key:
  // see above. Every other credential keeps the borrow-when-empty rule.
  const needsCloud = Boolean(config.cloud && !config.cloud.auth && !isApplbToken(value));
  // Either app-lb has nothing of its own, or the caller presented a credential
  // that carries its own scope and must not be traded up for this one's.
  const needsApplb = Boolean(config.applb && (!config.applb.auth || isApplbToken(value)));
  if (!needsCloud && !needsApplb) return config;

  return {
    ...config,
    cloud: needsCloud ? { ...config.cloud!, auth: value } : config.cloud,
    applb: needsApplb ? { ...config.applb!, auth: value } : config.applb,
  };
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  const timeout = Number(env.HEYO_MCP_TIMEOUT_MS ?? "30000");
  const cloud = cloudService(env.HEYO_BASE_URL, env.HEYO_API_KEY);
  return {
    cloud,
    applb: applbService(
      env.APPLB_URL,
      env.APPLB_NAMESPACE,
      env.APPLB_TOKEN,
      env.APPLB_BASIC,
      cloud,
    ),
    obs: service(env.APP_OBS_URL, env.APP_OBS_API_TOKEN),
    ci: service(env.CI_URL, env.CI_TOKEN),
    // Generous, but bounded. Every call here is a diagnostic or a sandbox
    // operation, and a hung one is worse than a failed one: it stalls the
    // conversation with no output at all.
    timeoutMs: Number.isFinite(timeout) && timeout > 0 ? timeout : 30_000,
  };
}

/** Which services are usable, for the startup banner and for `heyo_status`. */
export function configured(config: Config): string[] {
  const on: string[] = [];
  if (config.cloud?.auth) on.push(`heyo cloud (${config.cloud.baseUrl})`);
  if (config.applb) {
    on.push(
      config.applb.namespace
        ? `app-lb (namespace ${config.applb.namespace})`
        : config.applb.discoverNamespace
          ? "app-lb (managed; namespace discovered on first use)"
          : "app-lb",
    );
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

/**
 * What a 503 from cloud means, said once here rather than guessed at each call
 * site. It is a *placement* answer — "no host in this region runs that driver
 * and has room" — not a fault, so it is retryable in the sense that capacity
 * comes back, and not retryable in the sense that hammering it changes nothing.
 */
export const CLOUD_CAPACITY_HINT =
  "cloud answered 503: this is region capacity, not a fault. No backend in the " +
  "requested region could take the sandbox — usually every host that runs the " +
  "requested driver is full, or none runs it at all. Retrying the same request " +
  "immediately will fail the same way; retry with backoff (a few seconds, " +
  "doubling, ~5 attempts), and call heyo_capacity first to see whether any " +
  "daemon is online at all. Naming a driver the fleet actually runs " +
  "(driver: \"firecracker\") or the other region often succeeds where the " +
  "default did not.";
