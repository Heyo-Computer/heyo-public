/**
 * The configuration that is present, well-formed, and cannot work.
 *
 * A customer was handed a namespace-scoped app-lb token and put it in
 * `HEYO_API_KEY`. Everything they tried answered
 * `heyo cloud 401 on /namespaces: {"error":"Unauthorized"}` — a cloud path, for
 * questions they had asked app-lb, with no indication that the variable was the
 * problem. The server could already tell: `isApplbToken` existed and was applied
 * only to credentials arriving from callers, never to the one it was configured
 * with, on a transport (stdio) where that check never ran at all.
 *
 * These tests pin the whole path: what is detected, who is told, what is *not*
 * advertised, and what is no longer attempted over the network.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { cloudUsable, credentialFaults, faultBanner, loadConfig } from "./config.js";
import { buildTools, toolListing } from "./server.js";
import type { Tool } from "./tools/diagnose.js";

interface Call {
  url: string;
  method: string;
}

function stubFetch(responder: (c: Call) => { status?: number; body?: unknown }) {
  const calls: Call[] = [];
  const original = globalThis.fetch;
  globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    const call: Call = { url: String(input), method: init?.method ?? "GET" };
    calls.push(call);
    const { status = 200, body = {} } = responder(call);
    return new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json" },
    });
  }) as typeof fetch;
  return { calls, restore: () => void (globalThis.fetch = original) };
}

function tool(tools: Tool[], name: string): Tool {
  const found = tools.find((t) => t.name === name);
  assert.ok(found, `no such tool: ${name}`);
  return found;
}

/** Exactly what the customer had: one variable, holding the wrong kind of token. */
const theirConfig = () => loadConfig({ HEYO_API_KEY: "applb_dep_secret" });
const goodConfig = () => loadConfig({ HEYO_API_KEY: "heyo_api_x", APPLB_TOKEN: "heyo_api_lb" });

test("an app-lb token in HEYO_API_KEY is two faults, not a mystery", () => {
  const faults = credentialFaults(theirConfig());
  assert.equal(faults.length, 2, "both the cloud key and the door it opens are wrong");

  const [cloud, applb] = faults;
  assert.equal(cloud?.service, "heyo cloud");
  assert.equal(applb?.service, "app-lb");

  // Each names the variable and the configuration that works.
  for (const f of faults) {
    assert.match(f.detail, /APPLB_TOKEN/, `${f.service} does not say where the token belongs`);
    assert.match(f.detail, /APPLB_URL/, `${f.service} does not name the working shape`);
  }
  assert.match(cloud!.detail, /HEYO_API_KEY/);
  assert.match(cloud!.detail, /heyo_api_/, "does not say what cloud actually wants");
});

test("a fault never prints the token it is complaining about", () => {
  const banner = faultBanner(loadConfig({ HEYO_API_KEY: "applb_dep_HUNTER2SECRET" }));
  assert.ok(banner.length > 0, "no banner at all");
  assert.doesNotMatch(banner, /HUNTER2SECRET/, "the secret leaked into a log line");
  assert.match(banner, /applb_dep_…/, "not recognisable enough to match against the environment");
});

test("a correct configuration produces no faults and no banner", () => {
  assert.deepEqual(credentialFaults(goodConfig()), []);
  assert.equal(faultBanner(goodConfig()), "");
  assert.equal(cloudUsable(goodConfig()), true);
  assert.equal(cloudUsable(theirConfig()), false);
});

test("an unusable cloud key withholds the sandbox tools rather than listing them to fail", () => {
  const theirs = toolListing(buildTools(theirConfig())).map((t) => t.name);
  const good = toolListing(buildTools(goodConfig())).map((t) => t.name);

  assert.ok(!theirs.some((n) => n.startsWith("sandbox_")), "sandbox tools listed but unreachable");
  assert.ok(!theirs.includes("heyo_capacity"), "heyo_capacity hits cloud and must be withheld too");
  assert.ok(!theirs.includes("heyo_request"), "heyo_request targets cloud and must be withheld too");
  assert.equal(good.length - theirs.length, 16, "the gated set changed size unexpectedly");

  // The app-lb half of the server is still a complete, useful thing.
  assert.ok(theirs.includes("applb_list_deployments"));
  assert.ok(theirs.includes("heyo_status"), "the tool that explains this must always be present");
});

test("an app-lb call names the variables instead of a cloud path nobody asked about", async () => {
  const stub = stubFetch(() => ({ status: 401, body: { error: "Unauthorized" } }));
  try {
    const tools = buildTools(theirConfig());
    await assert.rejects(
      () => tool(tools, "applb_list_deployments").handler({}),
      (e: Error) => {
        assert.match(e.message, /HEYO_API_KEY/, "does not name the variable that is wrong");
        assert.match(e.message, /APPLB_TOKEN/, "does not name where the token belongs");
        assert.doesNotMatch(e.message, /401 on \/namespaces/, "still leads with the cloud path");
        return true;
      },
    );
    // And it never went to the network: the fault is knowable from config alone.
    assert.equal(stub.calls.length, 0, `sent ${stub.calls.length} requests it already knew would fail`);
  } finally {
    stub.restore();
  }
});

test("a refused namespace lookup is attempted once, not once per tool call", async () => {
  // A well-formed cloud key that cloud rejects. No config-level fault applies,
  // so this exercises the memoization split rather than the pre-flight: nothing
  // about the next call differs, so nothing should be re-sent.
  const stub = stubFetch(() => ({ status: 401, body: { error: "Unauthorized" } }));
  try {
    const tools = buildTools(loadConfig({ HEYO_API_KEY: "heyo_api_rejected" }));
    for (const name of ["applb_list_deployments", "applb_metrics", "applb_certs"]) {
      await assert.rejects(() => tool(tools, name).handler({}));
    }
    const lookups = stub.calls.filter((c) => c.url.endsWith("/namespaces"));
    assert.equal(lookups.length, 1, `re-asked cloud ${lookups.length} times for the same refusal`);
  } finally {
    stub.restore();
  }
});

test("a lookup that found nothing is retried, because a namespace can be created", async () => {
  // The case the original always-retry rule was written for, and the reason the
  // split is by permanence rather than by "did it fail".
  let created = false;
  const stub = stubFetch((c) =>
    c.url.endsWith("/namespaces")
      ? { body: { namespaces: created ? [{ name: "team-a" }] : [] } }
      : { body: [] },
  );
  try {
    const tools = buildTools(loadConfig({ HEYO_API_KEY: "heyo_api_x" }));
    await assert.rejects(() => tool(tools, "applb_list_deployments").handler({}), /reaches no app-lb namespace/);
    created = true;
    await tool(tools, "applb_list_deployments").handler({});
    assert.equal(stub.calls.filter((c) => c.url.endsWith("/namespaces")).length, 2);
  } finally {
    stub.restore();
  }
});

test("heyo_status leads with the fault and withdraws its own claim", async () => {
  const stub = stubFetch(() => ({ status: 401, body: { error: "Unauthorized" } }));
  try {
    const out = await tool(buildTools(theirConfig()), "heyo_status").handler({});

    assert.match(out, /CREDENTIAL FAULT/, "the cause is not stated");
    assert.ok(
      out.indexOf("CREDENTIAL FAULT") < out.indexOf("/me/daemons"),
      "the fault must come before the probes it explains",
    );
    // The bug: a static heading asserting the opposite of the body under it.
    assert.doesNotMatch(out, /reachable, and the key is good/);
    assert.match(out, /FAILED\. This is where a bad HEYO_API_KEY shows up/);
    assert.match(out, /NO usable key/, "the Configured line still calls cloud healthy");
  } finally {
    stub.restore();
  }
});

test("a working cloud key still gets the affirmative heading", async () => {
  const stub = stubFetch(() => ({ body: { daemons: [] } }));
  try {
    const out = await tool(buildTools(goodConfig()), "heyo_status").handler({});
    assert.match(out, /reachable, and the key is good/, "the claim is useful when it is true");
    assert.doesNotMatch(out, /CREDENTIAL FAULT/);
  } finally {
    stub.restore();
  }
});
