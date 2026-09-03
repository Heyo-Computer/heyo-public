/**
 * The behaviour that is not obvious from a route table: what the server infers
 * when it was told two API keys and nothing else, what it refuses to send, and
 * what it makes of a feed cursor that has outlived the feed.
 *
 * `fetch` is stubbed rather than a live cloud reached, so these assert the
 * shapes this server produces — the request it builds and the text it hands
 * back — not that cloud agrees with them.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { loadConfig } from "./config.js";
import { buildTools } from "./server.js";
import type { Tool } from "./tools/diagnose.js";

interface Call {
  url: string;
  method: string;
  body?: unknown;
}

type Reply = { status?: number; body?: unknown };

/** Stub `fetch` with a per-URL responder, and record what was asked. */
function stubFetch(responder: (call: Call) => Reply): { calls: Call[]; restore: () => void } {
  const calls: Call[] = [];
  const original = globalThis.fetch;
  globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    const call: Call = {
      url: String(input),
      method: init?.method ?? "GET",
      body: typeof init?.body === "string" ? JSON.parse(init.body) : undefined,
    };
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

const twoKeys = () => buildTools(loadConfig({ HEYO_API_KEY: "heyo_api_x", APPLB_TOKEN: "heyo_api_lb" }));

test("the managed namespace is discovered from the key, once, and reused", async () => {
  const stub = stubFetch((c) =>
    c.url.endsWith("/namespaces")
      ? { body: { namespaces: [{ name: "team-a", scope: "admin" }] } }
      : { body: [{ id: "web" }] },
  );
  try {
    const tools = twoKeys();
    await tool(tools, "applb_list_deployments").handler({});
    await tool(tools, "applb_list_deployments").handler({});

    assert.deepEqual(
      stub.calls.map((c) => c.url),
      [
        "https://server.heyo.computer/namespaces",
        "https://server.heyo.computer/namespaces/team-a/lb/deployments",
        "https://server.heyo.computer/namespaces/team-a/lb/deployments",
      ],
    );
  } finally {
    stub.restore();
  }
});

test("several namespaces are named in the error rather than guessed between", async () => {
  const stub = stubFetch(() => ({ body: { namespaces: [{ name: "team-a" }, { name: "team-b" }] } }));
  try {
    await assert.rejects(
      () => tool(twoKeys(), "applb_list_deployments").handler({}),
      /team-a, team-b.*APPLB_NAMESPACE/s,
    );
  } finally {
    stub.restore();
  }
});

test("sandbox_write_file refuses what the 1 MB body limit would reject", async () => {
  const stub = stubFetch(() => ({ body: {} }));
  try {
    const write = tool(twoKeys(), "sandbox_write_file");
    // 512 KiB is the measured ceiling: it encodes to ~683 KB and lands.
    await write.handler({ id: "sb-1", file_path: "a.bin", content: "x".repeat(512 * 1024) });
    assert.equal(stub.calls.length, 1);

    // One byte more and the round trip is spent to be told 413, so it is not
    // spent — the refusal names the route that has no such limit.
    await assert.rejects(
      () => write.handler({ id: "sb-1", file_path: "a.bin", content: "x".repeat(512 * 1024 + 1) }),
      /sandbox_upload_url/,
    );
    assert.equal(stub.calls.length, 1, "nothing was sent");
  } finally {
    stub.restore();
  }
});

test("sandbox_create sends the SDK's own defaults", async () => {
  const stub = stubFetch((c) =>
    c.url.endsWith("/sandbox-deploy") ? { body: { id: "sb-9" } } : { body: { id: "sb-9", status: "running" } },
  );
  try {
    await tool(twoKeys(), "sandbox_create").handler({ ttl_seconds: 900 });
    assert.deepEqual(stub.calls[0]?.body, {
      region: "US",
      image: "ubuntu:24.04",
      size_class: "small",
      open_ports: [],
      ttl_seconds: 900,
    });
    // Then it polls the sandbox rather than returning something still provisioning.
    assert.equal(stub.calls[1]?.url, "https://server.heyo.computer/deployed-sandboxes/sb-9");
  } finally {
    stub.restore();
  }
});

test("a 503 is retried as capacity; a rejected spec is not retried at all", async () => {
  let creates = 0;
  const stub = stubFetch((c) => {
    if (!c.url.endsWith("/sandbox-deploy")) return { body: { id: "sb-9", status: "running" } };
    creates += 1;
    return creates === 1
      ? { status: 503, body: { error: "No available backend in region US supports libvirt" } }
      : { body: { id: "sb-9" } };
  });
  try {
    const create = tool(twoKeys(), "sandbox_create");
    await create.handler({ retries: 1, wait_seconds: 0 });
    assert.equal(creates, 2, "capacity was waited out");

    creates = 0;
    stub.restore();
  } finally {
    stub.restore();
  }

  const rejected = stubFetch(() => ({ status: 422, body: { error: "unknown size_class" } }));
  try {
    await assert.rejects(
      () => tool(twoKeys(), "sandbox_create").handler({ retries: 3, size_class: "small" }),
      /422/,
    );
    assert.equal(rejected.calls.length, 1, "a bad spec is only rejected once");
  } finally {
    rejected.restore();
  }
});

test("a 503 from cloud arrives carrying what it means", async () => {
  const stub = stubFetch(() => ({ status: 503, body: { error: "No available backend in region US" } }));
  try {
    await assert.rejects(
      () => tool(twoKeys(), "sandbox_create").handler({ retries: 0 }),
      /region capacity, not a fault[\s\S]*heyo_capacity/,
    );
  } finally {
    stub.restore();
  }
});

test("the feed cursor is the caller's, and one from before a restart reads as a reset", async () => {
  const events = [
    { id: 7, kind: "issue", title: "web: cold start timed out" },
    { id: 6, kind: "deployed", title: "web deployed" },
  ];
  const stub = stubFetch((c) =>
    c.url.endsWith("/namespaces") ? { body: { namespaces: [{ name: "team-a" }] } } : { body: events },
  );
  try {
    const feed = tool(twoKeys(), "applb_feed");

    const fresh = await feed.handler({});
    assert.match(fresh, /latest_id 7/);
    // The namespace came from the discovery app-lb already did — the feed is
    // served at /feeds/:namespace, so it has to be named, not merely reached.
    assert.equal(
      stub.calls[1]?.url,
      "https://server.heyo.computer/namespaces/team-a/lb/feeds/team-a?format=json",
    );

    const incremental = await feed.handler({ since_id: 6 });
    assert.match(incremental, /cold start timed out/);
    assert.doesNotMatch(incremental, /web deployed/);

    // app-lb restarted: the ring is empty of everything the caller saw, and its
    // ids began again. Silence here would be permanent, so it says so instead.
    const afterRestart = await feed.handler({ since_id: 4_000 });
    assert.match(afterRestart, /Feed reset/);
    assert.match(afterRestart, /web deployed/);
  } finally {
    stub.restore();
  }
});
