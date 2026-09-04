/**
 * The server definition, independent of how it is reached.
 *
 * Split from the entrypoints because the same tools serve two transports:
 * stdio, where the host launches the process and identity is whoever ran it,
 * and HTTP, where app-lb's gate has already decided who the caller is.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { zodToJsonSchema } from "zod-to-json-schema";
import { z } from "zod";

import type { Config } from "./config.js";
import { makeClients } from "./clients/index.js";
import { diagnosticTools, type Tool } from "./tools/diagnose.js";
import { actionTools } from "./tools/actions.js";
import { sandboxTools } from "./tools/sandbox.js";
import { feedTools } from "./tools/feed.js";

export function buildTools(config: Config): Tool[] {
  const clients = makeClients(config);
  return [
    ...diagnosticTools(clients, config),
    ...sandboxTools(clients),
    ...feedTools(clients),
    ...actionTools(clients),
  ];
}

/**
 * A fresh `Server` per call.
 *
 * The HTTP transport runs statelessly — one server and one transport per
 * request — because app-lb balances across a pool. A session pinned to one
 * backend would work until the pool scaled, then fail for whichever requests
 * landed elsewhere, which is a bug that only appears under load.
 */
export function createServer(config: Config, tools: Tool[]): Server {
  const byName = new Map(tools.map((t) => [t.name, t]));
  const server = new Server(
    { name: "heyo-mcp", version: "0.1.0" },
    { capabilities: { tools: {} } },
  );

  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: tools.map((t) => ({
      name: t.name,
      description: t.description,
      inputSchema: zodToJsonSchema(z.object(t.schema), { $refStrategy: "none" }) as {
        type: "object";
      },
    })),
  }));

  server.setRequestHandler(CallToolRequestSchema, async (req) => {
    const tool = byName.get(req.params.name);
    if (!tool) {
      return {
        isError: true,
        content: [{ type: "text" as const, text: `No such tool: ${req.params.name}` }],
      };
    }
    try {
      const text = await tool.handler((req.params.arguments ?? {}) as Record<string, unknown>);
      return { content: [{ type: "text" as const, text }] };
    } catch (e) {
      // Returned, not thrown: a failed diagnostic is itself diagnostic, and
      // "app-obs 401" belongs in the transcript rather than in a dead call.
      return {
        isError: true,
        content: [{ type: "text" as const, text: e instanceof Error ? e.message : String(e) }],
      };
    }
  });

  return server;
}
