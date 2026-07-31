import { test } from "node:test";
import assert from "node:assert/strict";
import { Serverctl, TimeoutError, waitForJob, waitForReady } from "../dist/index.js";

function stub(...replies) {
  const seen = [];
  const fetch = async (url) => {
    seen.push(url);
    const r = replies.shift();
    if (!r) throw new Error(`no reply queued for ${url}`);
    return new Response(JSON.stringify(r), { status: 200 });
  };
  return { fetch, seen, client: new Serverctl({ server: "http://x:1", fetch }) };
}

const job = (status, log) => ({ id: "j1", deployment: "api", kind: "image-build", status, started_at: 0, log });

test("a job is polled until it stops running", async () => {
  const s = stub(
    job("running", ["cloning"]),
    job("running", ["cloning", "building"]),
    job("succeeded", ["cloning", "building", "done"]),
  );
  const seen = [];
  const done = await waitForJob(s.client, "j1", {
    pollMs: 1,
    onProgress: (p) => seen.push(...p.newLog),
  });
  assert.equal(done.status, "succeeded");
  // Each line reported exactly once, in order — the caller does not have to
  // deduplicate a growing log.
  assert.deepEqual(seen, ["cloning", "building", "done"]);
  assert.equal(s.seen.length, 3);
});

// A failed job is an answer. Rejecting would throw away the log and the error,
// which are the only reason to have waited.
test("a failed job resolves rather than rejecting", async () => {
  const s = stub({ ...job("failed", ["boom"]), error: "exit 1" });
  const done = await waitForJob(s.client, "j1", { pollMs: 1 });
  assert.equal(done.status, "failed");
  assert.equal(done.error, "exit 1");
});

// app-lb keeps a bounded log, so it can shrink between polls.
test("a truncated log does not produce a negative slice", async () => {
  const s = stub(job("running", ["a", "b", "c", "d"]), job("succeeded", ["d"]));
  const counts = [];
  await waitForJob(s.client, "j1", { pollMs: 1, onProgress: (p) => counts.push(p.newLog.length) });
  assert.deepEqual(counts, [4, 0]);
});

test("a job that never finishes times out", async () => {
  const s = stub(job("running", []), job("running", []));
  await assert.rejects(() => waitForJob(s.client, "j1", { pollMs: 1, timeoutMs: 0 }), TimeoutError);
});

const pool = (desired, pending, vms) => ({
  spec: { id: "api" }, kind: "vm", desired_replicas: desired,
  ready: vms.length, pending, total_in_flight: 0, vms,
});

// `ready` counts every backend including one failing its health check, so
// waiting on it reports success for a deployment that cannot serve a request.
test("a pool converges only when its VMs are healthy", async () => {
  const s = stub(
    pool(1, 1, []),
    pool(1, 0, [{ sandbox_id: "a", healthy: false, draining: false }]),
    pool(1, 0, [{ sandbox_id: "a", healthy: true, draining: false }]),
  );
  const ticks = [];
  await waitForReady(s.client, "api", { pollMs: 1, onProgress: (p) => ticks.push([p.healthy, p.pending]) });
  assert.deepEqual(ticks, [[0, 1], [0, 0], [1, 0]]);
  assert.equal(s.seen.length, 3);
});

test("a draining VM holds convergence open", async () => {
  const s = stub(
    pool(1, 0, [
      { sandbox_id: "a", healthy: true, draining: false },
      { sandbox_id: "b", healthy: true, draining: true },
    ]),
    pool(1, 0, [{ sandbox_id: "a", healthy: true, draining: false }]),
  );
  await waitForReady(s.client, "api", { pollMs: 1 });
  assert.equal(s.seen.length, 2);
});

// A site serves off disk and a static deployment proxies to fixed addresses;
// neither has a pool, so waiting for one would never return.
test("a deployment with no pool is immediately ready", async () => {
  for (const kind of ["site", "static"]) {
    const s = stub({ spec: { id: "docs" }, kind, desired_replicas: 0, ready: 0, pending: 0, total_in_flight: 0, vms: [] });
    await waitForReady(s.client, "docs", { pollMs: 1 });
    assert.equal(s.seen.length, 1, kind);
  }
});
