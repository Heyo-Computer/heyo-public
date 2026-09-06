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

/**
 * The tools this configuration can actually serve.
 *
 * Every group but one is unconditional: an unreachable app-lb, app-obs or ci
 * still gets its tools listed, because `bind` turns the absence into a
 * `NotConfigured` error naming the variable to set, and that is a better answer
 * than a tool that silently does not exist.
 *
 * The sandbox tools are the exception, and the reason is that their absence is
 * a *deployment shape* rather than a misconfiguration. An instance behind an
 * app-token gate has no cloud credential to act with and no way to get one — the
 * gate admits `applb_…` tokens, which cloud cannot consume — so listing sixteen
 * sandbox tools there advertises capability the caller can never reach. A
 * fleet-operations instance is a complete thing, not a broken one.
 *
 * Gated on the credential rather than on a switch of its own, because that is
 * exactly the condition: `makeClients` treats a cloud without one as absent for
 * the same reason. And because {@link withForwardedAuth} may supply it
 * per-request, this stays correct for a hosted instance carrying no key of its
 * own — a caller presenting a `heyo_api_*` key gets the sandbox tools, and one
 * presenting an app-lb token does not.
 *
 * `heyo_status` still reports cloud either way, so "this server has no sandbox
 * tools" remains an answerable question rather than a silent gap.
 */
export function buildTools(config: Config): Tool[] {
  const clients = makeClients(config);
  return [
    ...diagnosticTools(clients, config),
    ...(config.cloud?.auth ? sandboxTools(clients) : []),
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
