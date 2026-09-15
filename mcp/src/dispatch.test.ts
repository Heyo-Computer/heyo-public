/**
 * The protocol layer, exercised as a client actually reaches it.
 *
 * Every other test in this package calls `tool.handler(args)` directly, which is
 * the right way to test what a tool *does* and the wrong way to learn what a
 * client *sees*. Nothing covered `createServer`: not the listing, not the
 * unknown-name path, and not the decision to return a failed call as `isError`
 * text rather than letting it become a JSON-RPC error.
 *
 * That last one is load-bearing and easy to regress. A tool that throws is
 * usually a service answering 401 or 503, and the message is the diagnosis — it
 * belongs in the transcript where the model can read it, not in a protocol-level
 * error the host swallows. Two lines of `try`/`catch` in `createServer` are all
 * that stands between those two behaviours.
 *
 * `InMemoryTransport` speaks the real protocol over a linked pair of transports,
 * so this needs no network, no ports and no new dependency.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";

import { loadConfig } from "./config.js";
import { buildTools, createServer } from "./server.js";
import type { Tool } from "./tools/diagnose.js";

/**
 * A connected client/server pair over the given tools, and the teardown.
 *
 * The server is built the way both entrypoints build it, so a capability this
 * server forgets to declare is a capability the client cannot reach here either.
 */
async function connected(tools: Tool[]) {
  const config = loadConfig({ HEYO_API_KEY: "heyo_api_x" });
  const server = createServer(config, tools);
  const client = new Client({ name: "test", version: "0" }, { capabilities: {} });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await Promise.all([server.connect(serverTransport), client.connect(clientTransport)]);
  return { client, close: async () => void (await Promise.all([client.close(), server.close()])) };
}

const realTools = () =>
  buildTools(loadConfig({ HEYO_API_KEY: "heyo_api_x", APPLB_TOKEN: "heyo_api_lb" }));

test("tools/list round-trips every tool over the protocol", async () => {
  const { client, close } = await connected(realTools());
  try {
    const { tools } = await client.listTools();
    assert.equal(tools.length, realTools().length, "the protocol dropped or added a tool");

    const status = tools.find((t) => t.name === "heyo_status");
    assert.ok(status, "heyo_status is missing from the listing");
    assert.equal(status.inputSchema.type, "object");
    assert.ok((status.description ?? "").length > 0);
  } finally {
    await close();
  }
});

test("an unknown tool name is an isError result, not a protocol error", async () => {
  const { client, close } = await connected(realTools());
  try {
    const res = await client.callTool({ name: "no_such_tool", arguments: {} });
    assert.equal(res.isError, true);
    assert.match(String((res.content as { text: string }[])[0]?.text), /No such tool: no_such_tool/);
  } finally {
    await close();
  }
});

test("a throwing handler returns its message as isError text", async () => {
  // The shape that matters: "app-obs 401" has to survive into the transcript.
  const boom: Tool = {
    name: "heyo_boom",
    description: "always throws",
    schema: {},
    handler: async () => {
      throw new Error("app-obs 401 on /query: {}");
    },
  };
  const { client, close } = await connected([boom]);
  try {
    const res = await client.callTool({ name: "heyo_boom", arguments: {} });
    assert.equal(res.isError, true);
    assert.match(String((res.content as { text: string }[])[0]?.text), /app-obs 401 on \/query/);
  } finally {
    await close();
  }
});

test("a handler's text is returned verbatim as a single text block", async () => {
  const echo: Tool = {
    name: "heyo_echo",
    description: "returns what it was given",
    schema: {},
    handler: async (a) => `got ${String(a.value)}`,
  };
  const { client, close } = await connected([echo]);
  try {
    const res = await client.callTool({ name: "heyo_echo", arguments: { value: 42 } });
    assert.notEqual(res.isError, true);
    const content = res.content as { type: string; text: string }[];
    assert.equal(content.length, 1);
    assert.equal(content[0]?.type, "text");
    assert.equal(content[0]?.text, "got 42");
  } finally {
    await close();
  }
});
