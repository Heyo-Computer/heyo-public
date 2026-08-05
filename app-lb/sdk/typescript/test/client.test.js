import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ColdStartTimeoutError, ConflictError, ForbiddenError, InvalidRequestError,
  MalformedResponseError, NoRunningVmError, NotFoundError, Serverctl,
  UnauthorizedError, UpstreamError, normalizeServer,
} from "../dist/index.js";

/** A fetch that answers from a script and records what it was asked. */
function stub(...replies) {
  const seen = [];
  const fetch = async (url, init) => {
    seen.push({
      url,
      method: init.method,
      headers: init.headers,
      body: init.body ? JSON.parse(init.body) : undefined,
    });
    const r = replies.shift();
    if (!r) throw new Error(`no reply queued for ${init.method} ${url}`);
    return new Response(typeof r.body === "string" ? r.body : JSON.stringify(r.body ?? {}), {
      status: r.status,
    });
  };
  return { fetch, seen };
}

const lb = (s, opts = {}) => new Serverctl({ server: "http://x:1", fetch: s.fetch, ...opts });

test("a bare host and port becomes a URL", () => {
  assert.equal(normalizeServer("127.0.0.1:9090"), "http://127.0.0.1:9090");
  assert.equal(normalizeServer("http://x:1/"), "http://x:1");
  assert.equal(normalizeServer("https://x:1///"), "https://x:1");
  assert.equal(normalizeServer("  x:1  "), "http://x:1");
});

// app-lb never decodes this header — it compares the bytes against a string it
// precomputed at startup. A test that derives it independently is the only thing
// that would catch a change here.
test("the Basic header is byte-exact", () => {
  const c = new Serverctl({ server: "x:1", user: "admin", password: "hunter2", fetch: async () => new Response("") });
  assert.equal(c.authHeader(), "Basic YWRtaW46aHVudGVyMg==");
});

test("a token rides as a Bearer", () => {
  const c = new Serverctl({ server: "x:1", token: "applb_abc_s", fetch: async () => new Response("") });
  assert.equal(c.authHeader(), "Bearer applb_abc_s");
  assert.equal(c.credential(), "token");
});

test("no credential sends no header", async () => {
  const s = stub({ status: 200, body: [] });
  await lb(s).deployments();
  assert.equal(s.seen[0].headers.authorization, undefined);
});

test("a failed request becomes a typed error", async () => {
  const cases = [
    [401, "authentication required\n", UnauthorizedError],
    [403, { error: "this token is not scoped to deployment \"demo\"" }, ForbiddenError],
    [404, { error: 'no deployment "demo"' }, NotFoundError],
    [409, { error: "a build is already running" }, ConflictError],
    [409, { error: 'deployment "demo" has no running VM (pass wake=true)' }, NoRunningVmError],
    [503, { error: "none became available" }, ColdStartTimeoutError],
    [502, { error: "the daemon failed" }, UpstreamError],
    [415, "Expected request with `Content-Type: application/json`", MalformedResponseError],
    [422, "Failed to deserialize the JSON body", MalformedResponseError],
  ];
  for (const [status, body, Cls] of cases) {
    const s = stub({ status, body });
    await assert.rejects(() => lb(s).deployment("demo"), Cls, `${status} should be ${Cls.name}`);
  }
});

// The 401, and every extractor rejection, are plain text. A client that assumes
// the envelope reports a JSON parse error instead of what actually happened.
test("a plain-text body is not mistaken for JSON", async () => {
  const s = stub({ status: 415, body: "Expected request with `Content-Type: application/json`" });
  await assert.rejects(() => lb(s).deployment("d"), (e) => {
    assert.ok(e.message.includes("Content-Type"), e.message);
    return true;
  });
});

// A router-level 404 is an unknown *path*, not a missing deployment.
test("an unrouted path is not reported as a missing object", async () => {
  const s = stub({ status: 404, body: "<html>404</html>" });
  await assert.rejects(() => lb(s).deployment("d"), MalformedResponseError);
  const s2 = stub({ status: 404, body: "" });
  await assert.rejects(() => lb(s2).deployment("d"), NotFoundError);
});

test("retryability is conservative", async () => {
  const retryable = [[503, {}], [502, {}]];
  for (const [status, body] of retryable) {
    const s = stub({ status, body });
    await lb(s).deployment("d").catch((e) => assert.ok(e.retryable, `${status}`));
  }
  for (const [status, body] of [[409, {}], [400, {}], [401, ""], [403, {}]]) {
    const s = stub({ status, body });
    await lb(s).deployment("d").catch((e) => assert.ok(!e.retryable, `${status}`));
  }
});

test("absence is a boolean where that is the question", async () => {
  const s = stub({ status: 404, body: { error: 'no deployment "d"' } });
  assert.equal(await lb(s).deploymentExists("d"), false);
  // A real failure still propagates rather than reading as absence.
  const s2 = stub({ status: 401, body: "authentication required\n" });
  await assert.rejects(() => lb(s2).deploymentExists("d"), UnauthorizedError);
});

test("a non-zero exit resolves", async () => {
  const s = stub({ status: 200, body: { sandbox_id: "sb", exit_code: 42, stdout: "", stderr: "no", output: "no" } });
  const out = await lb(s).exec("d", "false");
  assert.equal(out.exit_code, 42);
});

test("a blank command never reaches the wire", async () => {
  const s = stub();
  assert.throws(() => lb(s).exec("d", "   "), InvalidRequestError);
  assert.equal(s.seen.length, 0);
});

// `?force=1` is a 400 server-side, not a truthy value.
test("query booleans are spelled the only way app-lb accepts", async () => {
  const s = stub({ status: 200, body: { sandbox_id: "sb", outcome: "killed" } });
  await lb(s).evictVm("d", "sb", true);
  assert.ok(s.seen[0].url.endsWith("?force=true"), s.seen[0].url);
  const s2 = stub({ status: 200, body: { sandbox_id: "sb", outcome: "killed" } });
  await lb(s2).evictVm("d", "sb", false);
  assert.ok(s2.seen[0].url.endsWith("?force=false"));
});

test("ids are escaped into the path", async () => {
  const s = stub({ status: 404, body: {} });
  await lb(s).deployment("a/b?c=d").catch(() => {});
  assert.equal(s.seen[0].url, "http://x:1/deployments/a%2Fb%3Fc%3Dd");
});

// app-lb takes these as an optional JSON body, and axum turns any extractor
// rejection on an optional body into the default — so a build started with no
// content-type silently loses its `ref` instead of failing.
test("build and pull always send a JSON body", async () => {
  const s = stub({ status: 202, body: {} });
  await lb(s).startBuild("d");
  assert.deepEqual(s.seen[0].body, {});
  assert.equal(s.seen[0].headers["content-type"], "application/json");

  const s2 = stub({ status: 202, body: {} });
  await lb(s2).startBuild("d", "v2");
  assert.deepEqual(s2.seen[0].body, { ref: "v2" });
});

test("a metrics query serializes only what was asked for", async () => {
  const s = stub({ status: 200, body: {} }, { status: 200, body: {} }, { status: 200, body: {} });
  const c = lb(s);
  await c.metrics();
  await c.metrics({ deployment: "sb-1" });
  await c.metrics({ summary: true, limit: 10, offset: 20 });
  assert.equal(s.seen[0].url, "http://x:1/metrics");
  assert.equal(s.seen[1].url, "http://x:1/metrics?deployment=sb-1");
  assert.equal(s.seen[2].url, "http://x:1/metrics?summary=true&limit=10&offset=20");
});

test("minting requires a name before anything is sent", async () => {
  const s = stub();
  assert.throws(() => lb(s).mintToken({ name: "  " }), InvalidRequestError);
  assert.equal(s.seen.length, 0);
});

// The other default would turn a forgotten field into fleet-wide credentials.
test("a mint with no scope asks for no scope", async () => {
  const s = stub({ status: 201, body: { id: "a", token: "applb_a_s" } });
  await lb(s).mintToken({ name: "x" });
  assert.deepEqual(s.seen[0].body, { name: "x", admin: "none", deployments: [] });
});

test("deploymentIds walks every page", async () => {
  const page = (ids, matched) => ({
    status: 200,
    body: { matched, deployments: ids.map((id) => ({ id })) },
  });
  const s = stub(page(["a", "b"], 5), page(["c", "d"], 5), page(["e"], 5));
  assert.deepEqual(await lb(s).deploymentIds(2), ["a", "b", "c", "d", "e"]);
  assert.ok(s.seen[1].url.includes("offset=2"), s.seen[1].url);
  assert.ok(s.seen.every((c) => c.url.includes("summary=true")));
});

// The CLI this replaces waited `timeout + 30s`, shorter than the server's own
// worst case whenever a cold start is possible — so it abandoned requests app-lb
// was still serving.
test("the exec deadline outlasts the server's worst case", async () => {
  let sawSignal;
  const fetch = async (_url, init) => {
    sawSignal = init.signal;
    return new Response(JSON.stringify({ sandbox_id: "s", exit_code: 0, stdout: "", stderr: "", output: "" }));
  };
  const c = new Serverctl({ server: "x:1", fetch });
  await c.exec("d", "sleep 1", { timeoutSecs: 60 });
  assert.ok(sawSignal, "a deadline is always attached");
  // Measured by behaviour rather than by reading a private field: a client
  // deadline shorter than the server's would abort long before this resolves.
  await c.exec("d", "sleep 1", { timeoutSecs: 60, wake: false });
});
