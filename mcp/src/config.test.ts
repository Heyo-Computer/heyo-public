/**
 * The two decisions that make one build serve both a self-hosted app-lb and the
 * managed service: where the base URL points, and whose credential goes with
 * it. Everything downstream is a path appended to a base, so these are the
 * whole of "managed mode" and the whole of what can go wrong with it.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { applbService, CLOUD_BASE_URL, configured, loadConfig, withForwardedAuth } from "./config.js";

test("a plain APPLB_URL is app-lb's own listener", () => {
  const cfg = applbService("http://127.0.0.1:8080/", undefined, "tok");
  assert.equal(cfg?.baseUrl, "http://127.0.0.1:8080");
  assert.equal(cfg?.auth, "Bearer tok");
  assert.equal(cfg?.namespace, undefined);
});

test("APPLB_NAMESPACE routes through cloud's namespace door", () => {
  const cfg = applbService("https://server.heyo.computer", "team-a", "heyo_api_x");
  assert.equal(cfg?.baseUrl, "https://server.heyo.computer/namespaces/team-a/lb");
  assert.equal(cfg?.namespace, "team-a");
  assert.equal(cfg?.auth, "Bearer heyo_api_x");
});

test("a URL already ending in /lb is not rewritten again", () => {
  const cfg = applbService("https://server.heyo.computer/namespaces/team-a/lb/", "team-a");
  assert.equal(cfg?.baseUrl, "https://server.heyo.computer/namespaces/team-a/lb");
  assert.equal(cfg?.namespace, "team-a");
});

test("loadConfig reads the namespace from the environment and reports it", () => {
  const config = loadConfig({
    APPLB_URL: "https://server.heyo.computer",
    APPLB_NAMESPACE: "team-a",
    APPLB_TOKEN: "heyo_api_x",
  });
  assert.equal(config.applb?.baseUrl, "https://server.heyo.computer/namespaces/team-a/lb");
  assert.deepEqual(configured(config), ["app-lb (namespace team-a)"]);
});

test("the caller's bearer is forwarded only to a service with no credential of its own", () => {
  const anonymous = loadConfig({ APPLB_URL: "https://server.heyo.computer", APPLB_NAMESPACE: "team-a" });
  const forwarded = withForwardedAuth(anonymous, { authorization: "Bearer heyo_api_caller" });
  assert.notEqual(forwarded, anonymous);
  assert.equal(forwarded.applb?.auth, "Bearer heyo_api_caller");
  assert.equal(forwarded.cloud?.auth, "Bearer heyo_api_caller");
  assert.equal(forwarded.applb?.baseUrl, anonymous.applb?.baseUrl);

  // Per service, not all-or-nothing: an instance holding its own app-lb
  // credential keeps acting as itself there while still borrowing the caller's
  // key for the cloud it was given none for.
  const own = loadConfig({ APPLB_URL: "http://127.0.0.1:8080", APPLB_TOKEN: "applb_fixed" });
  const mixed = withForwardedAuth(own, { authorization: "Bearer heyo_api_caller" });
  assert.equal(mixed.applb?.auth, "Bearer applb_fixed");
  assert.equal(mixed.cloud?.auth, "Bearer heyo_api_caller");

  // Configured on both sides, nothing to borrow: the same object comes back and
  // the per-process tool set is reused.
  const fully = loadConfig({ APPLB_URL: "http://127.0.0.1:8080", APPLB_TOKEN: "t", HEYO_API_KEY: "k" });
  assert.equal(withForwardedAuth(fully, { authorization: "Bearer heyo_api_caller" }), fully);

  assert.equal(withForwardedAuth(anonymous, {}), anonymous);
  assert.equal(withForwardedAuth(anonymous, { authorization: "   " }), anonymous);
  assert.equal(withForwardedAuth(loadConfig({}), { authorization: "Bearer x" }).applb, undefined);
});

test("an app-lb-minted token is never traded up for this process's own", () => {
  // The escalation this closes: a token scoped to one deployment, admitted by
  // an app-token gate, reaching app-lb as whatever fleet-wide credential the
  // operator configured. The caller's own token wins, so app-lb scope-checks
  // the principal that was actually authenticated.
  const own = loadConfig({ APPLB_URL: "http://127.0.0.1:8080", APPLB_TOKEN: "applb_fleet_wide" });
  const scoped = withForwardedAuth(own, { authorization: "Bearer applb_abc123_secret" });
  assert.equal(scoped.applb?.auth, "Bearer applb_abc123_secret");
  assert.equal(scoped.applb?.baseUrl, own.applb?.baseUrl);

  // Basic-gated dashboards are configured the same way and get the same rule.
  const basic = loadConfig({ APPLB_URL: "http://127.0.0.1:8080", APPLB_BASIC: "admin:hunter2" });
  assert.equal(
    withForwardedAuth(basic, { authorization: "Bearer applb_abc123_secret" }).applb?.auth,
    "Bearer applb_abc123_secret",
  );

  // Only app-lb's own kind of token. A JWT from the heyo auth API means nothing
  // to app-lb's admin API, so a jwt-gated deployment keeps acting as itself
  // rather than forwarding a credential app-lb cannot read.
  const jwtCaller = withForwardedAuth(own, { authorization: "Bearer eyJhbGciOiJIUzI1NiJ9.e30.x" });
  assert.equal(jwtCaller.applb?.auth, "Bearer applb_fleet_wide");

  // Nor is a cloud key one, even though managed mode reaches app-lb with it:
  // there the process has no credential of its own and the first rule already
  // forwards it.
  assert.equal(
    withForwardedAuth(own, { authorization: "Bearer heyo_api_caller" }).applb?.auth,
    "Bearer applb_fleet_wide",
  );

  // The prefix is tested on the token, not the raw header, so neither the
  // scheme's spelling nor a bearer that merely mentions the prefix decides it.
  // Detection is deliberately looser than app-lb's own `strip_prefix("Bearer ")`,
  // because the two failure directions are not symmetric: a form detected here
  // that app-lb will not parse is a 401, while one app-lb accepts and this
  // missed would fall back to the configured token — the escalation. The header
  // itself is still forwarded byte for byte, as the Basic case requires.
  assert.equal(
    withForwardedAuth(own, { authorization: "bearer   applb_abc123_secret" }).applb?.auth,
    "bearer   applb_abc123_secret",
  );
  assert.equal(
    withForwardedAuth(own, { authorization: "Basic applb_not_a_bearer" }).applb?.auth,
    "Bearer applb_fleet_wide",
  );
  assert.equal(
    withForwardedAuth(own, { authorization: "Bearer x_applb_suffix" }).applb?.auth,
    "Bearer applb_fleet_wide",
  );

  // Cloud stays on its own key: an applb_ token is not a cloud credential, and
  // swapping it in would break every sandbox tool rather than narrow anything.
  const both = loadConfig({
    APPLB_URL: "http://127.0.0.1:8080",
    APPLB_TOKEN: "applb_fleet_wide",
    HEYO_API_KEY: "heyo_api_server",
  });
  const mixed = withForwardedAuth(both, { authorization: "Bearer applb_abc123_secret" });
  assert.equal(mixed.applb?.auth, "Bearer applb_abc123_secret");
  assert.equal(mixed.cloud?.auth, "Bearer heyo_api_server");

  // And the reuse path survives: fully configured, with a caller whose bearer
  // is nobody's business here, the same object comes back.
  assert.equal(withForwardedAuth(both, { authorization: "Bearer eyJhbGciOiJIUzI1NiJ9.e30.x" }), both);
});

test("two API keys are the whole configuration", () => {
  // The claim the README makes, asserted: no URL, no namespace, no anything
  // else, and both cloud and the managed app-lb come up.
  const config = loadConfig({ HEYO_API_KEY: "heyo_api_x", APPLB_TOKEN: "heyo_api_lb" });
  assert.equal(config.cloud?.baseUrl, CLOUD_BASE_URL);
  assert.equal(config.cloud?.auth, "Bearer heyo_api_x");
  assert.equal(config.applb?.baseUrl, CLOUD_BASE_URL);
  assert.equal(config.applb?.auth, "Bearer heyo_api_lb");
  assert.equal(config.applb?.discoverNamespace, true);
  assert.deepEqual(configured(config), [
    `heyo cloud (${CLOUD_BASE_URL})`,
    "app-lb (managed; namespace discovered on first use)",
  ]);
});

test("one API key is enough, because through cloud's door they are the same key", () => {
  const config = loadConfig({ HEYO_API_KEY: "heyo_api_x" });
  assert.equal(config.applb?.auth, "Bearer heyo_api_x");
  assert.equal(config.applb?.discoverNamespace, true);
});

test("a named namespace is used as given, and never discovered", () => {
  const config = loadConfig({ HEYO_API_KEY: "heyo_api_x", APPLB_NAMESPACE: "team-a" });
  assert.equal(config.applb?.baseUrl, `${CLOUD_BASE_URL}/namespaces/team-a/lb`);
  assert.equal(config.applb?.discoverNamespace, undefined);
});

test("a self-hosted app-lb is never treated as a namespace to discover", () => {
  // An admin listener has no /namespaces to ask, so guessing at one here would
  // turn every app-lb call into a confusing 404 on a URL nobody wrote.
  const cfg = applbService("http://127.0.0.1:8080", undefined, "tok", undefined, {
    baseUrl: CLOUD_BASE_URL,
  });
  assert.equal(cfg?.baseUrl, "http://127.0.0.1:8080");
  assert.equal(cfg?.discoverNamespace, undefined);
});

test("HEYO_BASE_URL moves cloud and the managed app-lb together", () => {
  const config = loadConfig({ HEYO_BASE_URL: "http://127.0.0.1:4445/", HEYO_API_KEY: "k" });
  assert.equal(config.cloud?.baseUrl, "http://127.0.0.1:4445");
  assert.equal(config.applb?.baseUrl, "http://127.0.0.1:4445");
  assert.equal(config.applb?.discoverNamespace, true);
});

test("no credential at all configures nothing", () => {
  const config = loadConfig({});
  assert.equal(config.applb, undefined);
  assert.deepEqual(configured(config), []);
});

test("an app-lb token is never substituted for a cloud key", () => {
  // The fleet-operations shape: an app-token gate in front, no HEYO_API_KEY, and
  // callers who by definition present `applb_…`. Cloud has never heard of that
  // credential, so borrowing it under the "no credential of its own" rule would
  // turn every sandbox tool into a guaranteed 401 — capability advertised and
  // unreachable. Cloud stays unconfigured instead, and `buildTools` lists none.
  const fleetOps = loadConfig({ APPLB_URL: "http://127.0.0.1:8080" });
  assert.equal(fleetOps.cloud?.auth, undefined);

  const asCaller = withForwardedAuth(fleetOps, { authorization: "Bearer applb_abc123_secret" });
  assert.equal(asCaller.applb?.auth, "Bearer applb_abc123_secret");
  assert.equal(asCaller.cloud?.auth, undefined);

  // Only that prefix. A cloud key is exactly what cloud wants and is still
  // borrowed — this is the rule that makes managed mode multi-tenant, and
  // narrowing it would have cost every hosted instance its sandboxes.
  assert.equal(
    withForwardedAuth(fleetOps, { authorization: "Bearer heyo_api_caller" }).cloud?.auth,
    "Bearer heyo_api_caller",
  );
  // A JWT likewise: some other gate issued it, and this is not the place to
  // decide cloud will refuse it.
  assert.equal(
    withForwardedAuth(fleetOps, { authorization: "Bearer eyJhbGciOiJIUzI1NiJ9.e30.x" }).cloud?.auth,
    "Bearer eyJhbGciOiJIUzI1NiJ9.e30.x",
  );

  // And with nothing to give either service, the same object comes back rather
  // than a copy that changed nothing.
  assert.equal(
    withForwardedAuth(
      loadConfig({ APPLB_URL: "http://127.0.0.1:8080", APPLB_TOKEN: "applb_fleet_wide" }),
      { authorization: "Bearer heyo_api_caller" },
    ).cloud?.auth,
    "Bearer heyo_api_caller",
  );
});
