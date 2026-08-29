/**
 * The two decisions that make one build serve both a self-hosted app-lb and the
 * managed service: where the base URL points, and whose credential goes with
 * it. Everything downstream is a path appended to a base, so these are the
 * whole of "managed mode" and the whole of what can go wrong with it.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { applbService, configured, loadConfig, withForwardedAuth } from "./config.js";

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

test("the caller's bearer is forwarded only when app-lb has no credential of its own", () => {
  const anonymous = loadConfig({ APPLB_URL: "https://server.heyo.computer", APPLB_NAMESPACE: "team-a" });
  const forwarded = withForwardedAuth(anonymous, { authorization: "Bearer heyo_api_caller" });
  assert.notEqual(forwarded, anonymous);
  assert.equal(forwarded.applb?.auth, "Bearer heyo_api_caller");
  assert.equal(forwarded.applb?.baseUrl, anonymous.applb?.baseUrl);

  const own = loadConfig({ APPLB_URL: "http://127.0.0.1:8080", APPLB_TOKEN: "applb_fixed" });
  assert.equal(withForwardedAuth(own, { authorization: "Bearer heyo_api_caller" }), own);

  assert.equal(withForwardedAuth(anonymous, {}), anonymous);
  assert.equal(withForwardedAuth(anonymous, { authorization: "   " }), anonymous);
  assert.equal(withForwardedAuth(loadConfig({}), { authorization: "Bearer x" }).applb, undefined);
});
