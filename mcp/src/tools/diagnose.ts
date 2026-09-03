/**
 * The task-oriented half: tools shaped like the questions people actually ask.
 *
 * Each one joins across services, because the answers do. "Why is nothing
 * running" is app-lb's topology *and* app-obs's logs *and* ci's queue, and a
 * tool per endpoint leaves that join to be redone by hand every time. The
 * descriptions carry what took a week to learn — which endpoint answers which
 * question, and which one looks like it should and does not.
 */

import { z } from "zod";
import type { Clients } from "../clients/index.js";
import { settle } from "../clients/index.js";
import { report, section, json } from "../format.js";
import { configured, type Config } from "../config.js";

export interface Tool {
  name: string;
  description: string;
  schema: z.ZodRawShape;
  handler: (args: Record<string, unknown>) => Promise<string>;
}

export function diagnosticTools(clients: Clients, config: Config): Tool[] {
  return [
    {
      name: "heyo_status",
      description:
        "Which of heyo cloud, app-lb, app-obs and ci this server can reach, and what each " +
        "says about itself. Start here when a tool fails with a connection or auth error — " +
        "it distinguishes 'not configured' from 'configured and refusing'. The cloud probe " +
        "doubles as an API-key check, and the app-lb one resolves the managed namespace, so " +
        "a namespace that cannot be worked out surfaces here rather than inside a later call.",
      schema: {},
      handler: async () => {
        const r = await settle({
          cloud: clients.cloud({ path: "/me/daemons" }),
          applb: clients.applb({ path: "/metrics" }),
          obs: clients.obs({ path: "/healthz" }),
          ci: clients.ci({ path: "/healthz" }),
        });
        return report(`Configured: ${configured(config).join(", ") || "nothing"}`, [
          section("heyo cloud /me/daemons — reachable, and the key is good", r.cloud),
          section("app-lb /metrics", r.applb),
          section("app-obs /healthz", r.obs),
          section("ci /healthz", r.ci),
        ]);
      },
    },

    {
      name: "fleet_overview",
      description:
        "The whole managed fleet in one call: app-obs's per-deployment rows with host CPU " +
        "and memory, app-lb's current topology with health and drain state, and app-obs's " +
        "ingest counters. Use this before drilling into one deployment — it is the only view " +
        "that shows a problem affecting several at once. The topology also lists " +
        "`host_sandboxes`: VMs on the host that no deployment owns (made through heyvm, the " +
        "cloud API or the desktop). They share the host's CPU and memory with every pool, so " +
        "a loaded host beside idle pools is usually explained there; their logs live under " +
        "the app-obs deployment `_unmanaged`, filtered by `backend`.",
      schema: { window: z.string().optional().describe("app-obs window, e.g. '15m', '1h', '24h'") },
      handler: async (args) => {
        const window = (args.window as string) ?? "1h";
        const r = await settle({
          fleet: clients.obs({ path: "/api/fleet", query: { window } }),
          platform: clients.obs({ path: "/api/platform-status" }),
          stats: clients.obs({ path: "/stats" }),
        });
        return report(`Fleet over ${window}`, [
          section("Deployments and host resources (app-obs /api/fleet)", r.fleet),
          section("app-lb topology as app-obs last saw it", r.platform),
          section("Ingest counters — dropped rows mean the collector fell behind", r.stats),
        ]);
      },
    },

    {
      name: "diagnose_deployment",
      description:
        "Everything about one deployment at once: app-lb's record and its VM pool, app-obs's " +
        "bucketed series, and the most recent error-level logs. This is the first call for " +
        "'deployment X is unhealthy'.\n\n" +
        "Note what this cannot show. A guest whose `start_command` fails writes to the guest's " +
        "own /var/log/heyvm-start.log and that never reaches app-obs, so an empty log section " +
        "next to a pool that will not fill is a signal, not an absence of one.",
      schema: {
        id: z.string().describe("app-lb deployment id"),
        window: z.string().optional().describe("app-obs window, default '1h'"),
      },
      handler: async (args) => {
        const id = String(args.id);
        const window = (args.window as string) ?? "1h";
        const r = await settle({
          deployment: clients.applb({ path: `/deployments/${encodeURIComponent(id)}` }),
          jobs: clients.applb({ path: `/deployments/${encodeURIComponent(id)}/jobs` }),
          series: clients.obs({ path: `/api/deployments/${encodeURIComponent(id)}`, query: { window } }),
          errors: clients.obs({
            path: `/api/deployments/${encodeURIComponent(id)}/logs`,
            query: { window, level: "error", limit: 50 },
          }),
        });
        return report(`Deployment ${id} over ${window}`, [
          section("app-lb record", r.deployment),
          section("Recent jobs (builds, pulls, updates)", r.jobs),
          section("Series and summary (app-obs)", r.series),
          section("Most recent error logs", r.errors),
        ]);
      },
    },

    {
      name: "deployment_logs",
      description:
        "Log lines for one deployment, newest first, with the filters app-obs supports: " +
        "time window or explicit from/to, level, backend, a substring query, and a cursor for " +
        "paging. Collected from the daemon's native tail of each sandbox's console and its " +
        "start_command's stdout/stderr, so no shipper inside the guest is required.",
      schema: {
        id: z.string().describe("app-lb deployment id"),
        window: z.string().optional().describe("e.g. '15m'; ignored when from/to are given"),
        from: z.string().optional(),
        to: z.string().optional(),
        level: z.string().optional().describe("e.g. 'error', 'warn'"),
        backend: z.string().optional().describe("restrict to one backend/sandbox"),
        q: z.string().optional().describe("substring match on the message"),
        limit: z.number().optional().describe("default 100"),
        before: z.string().optional().describe("cursor from a previous page"),
      },
      handler: async (args) => {
        const id = String(args.id);
        const out = await clients.obs({
          path: `/api/deployments/${encodeURIComponent(id)}/logs`,
          query: {
            window: args.window as string | undefined,
            from: args.from as string | undefined,
            to: args.to as string | undefined,
            level: args.level as string | undefined,
            backend: args.backend as string | undefined,
            q: args.q as string | undefined,
            limit: (args.limit as number | undefined) ?? 100,
            before: args.before as string | undefined,
          },
        });
        return json(out);
      },
    },

    {
      name: "diagnose_empty_pool",
      description:
        "Why a deployment's VM pool is empty or will not fill.\n\n" +
        "Reads app-lb's /metrics, which is the endpoint that answers this — it carries the " +
        "per-deployment pool counters and the create/boot outcomes. /disks looks like it " +
        "should answer it and cannot: it describes storage, not pool state. Both are included " +
        "because a full disk is one of the two common causes, the other being scale-to-zero " +
        "churn.\n\n" +
        "If the counters show creates attempted and zero booted, the failure is inside the " +
        "guest and app-obs will not have it: a start_command that exits non-zero writes to the " +
        "guest's /var/log/heyvm-start.log, which needs a shell on the VM to read.",
      schema: { id: z.string().optional().describe("deployment id; omitted shows every pool") },
      handler: async (args) => {
        const id = args.id ? String(args.id) : undefined;
        const r = await settle({
          metrics: clients.applb({ path: "/metrics" }),
          disks: clients.applb({ path: "/disks" }),
          deployment: id
            ? clients.applb({ path: `/deployments/${encodeURIComponent(id)}` })
            : Promise.resolve(null),
        });
        return report(id ? `Pool diagnosis for ${id}` : "Pool diagnosis (all deployments)", [
          section("app-lb /metrics — the pool counters that answer this", r.metrics),
          section("app-lb /disks — storage, for the full-disk case only", r.disks),
          section("Deployment record", r.deployment),
        ]);
      },
    },

    {
      name: "diagnose_ci_job",
      description:
        "Why a ci job is not running. Joins the run and its jobs with the runner pool and the " +
        "per-subject queue depths, which is the combination that distinguishes the cases: a " +
        "job waiting on a network whose queue has no consumer, a job pinned to an offline " +
        "host, a healthy pool whose queue was consumed by something else, and a message that " +
        "was never published at all.\n\n" +
        "Requires CI_URL to reach ci's own listener. Behind an app-lb gate these routes answer " +
        "401 to any machine client regardless of token.",
      schema: { run_id: z.string().optional(), job_key: z.string().optional() },
      handler: async (args) => {
        const runId = args.run_id ? String(args.run_id) : undefined;
        const r = await settle({
          run: runId ? clients.ci({ path: `/runs/${encodeURIComponent(runId)}` }) : Promise.resolve(null),
          runners: clients.ci({ path: "/runners" }),
          vms: clients.ci({ path: "/vms" }),
        });
        return report(runId ? `ci run ${runId}` : "ci runner and pool state", [
          section("Run and its jobs", r.run),
          section("Networks, hosts, and queue depth per subject", r.runners),
          section("Warm VM pool", r.vms),
        ]);
      },
    },
  ];
}
