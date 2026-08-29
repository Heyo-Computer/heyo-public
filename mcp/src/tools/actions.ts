/**
 * The mutating half, plus raw access for full coverage.
 *
 * **Destructive operations get their own named tools.** Folding them into a
 * generic `request` tool would hide a `DELETE` inside a parameter, where it is
 * invisible in a transcript and in an approval prompt. `applb_delete_deployment`
 * cannot be misread; `applb_request({method:"DELETE", ...})` can. The raw tools
 * exist below for coverage of everything not named here, and they say what they
 * are.
 *
 * Nothing here asks for confirmation on its own: MCP has no such affordance, and
 * approval belongs to the host. What this file can do is make the name and the
 * description tell the truth about what the call does, which it does.
 */

import { z } from "zod";
import type { Clients } from "../clients/index.js";
import { json } from "../format.js";
import type { Tool } from "./diagnose.js";

const DESTRUCTIVE = "DESTRUCTIVE. ";

export function actionTools(clients: Clients): Tool[] {
  const enc = encodeURIComponent;

  return [
    // ---- app-lb reads -------------------------------------------------
    {
      name: "applb_list_deployments",
      description: "Every deployment app-lb manages, with its backends and current state.",
      schema: {},
      handler: async () => json(await clients.applb({ path: "/deployments" })),
    },
    {
      name: "applb_get_deployment",
      description:
        "One deployment in full: its spec, desired and ready replica counts, and every VM " +
        "with its health. The record to read before scaling, updating or deleting it.",
      schema: { id: z.string() },
      handler: async (a) => json(await clients.applb({ path: `/deployments/${enc(String(a.id))}` })),
    },
    {
      name: "applb_metrics",
      description:
        "app-lb's live metrics: per-deployment pool counters, request stats, and create/boot " +
        "outcomes. The endpoint that answers 'why is this pool empty'.",
      schema: {},
      handler: async () => json(await clients.applb({ path: "/metrics" })),
    },
    {
      name: "applb_disks",
      description:
        "Disk inventory and usage. Answers 'what is using space' and 'are there orphans'; " +
        "does not answer pool questions — use applb_metrics for those.",
      schema: {},
      handler: async () => json(await clients.applb({ path: "/disks" })),
    },
    {
      name: "applb_certs",
      description: "TLS certificates app-lb holds, with their expiry.",
      schema: {},
      handler: async () => json(await clients.applb({ path: "/certs" })),
    },

    // ---- app-lb lifecycle ---------------------------------------------
    {
      name: "applb_create_deployment",
      description:
        "Register a deployment from a full spec (`id`, `routes`, and one of `vm`, `upstreams` " +
        "or `site`; optional `scaling`, `health`, `build`). Through the managed service the " +
        "namespace is the configured one and need not be given. A POST whose `id` already " +
        "exists REPLACES that deployment and recycles its VM pool — use applb_get_deployment " +
        "first if unsure, and applb_scale for a scaling-only change.",
      schema: { spec: z.record(z.unknown()).describe("the deployment spec app-lb expects") },
      handler: async (a) =>
        json(await clients.applb({ method: "POST", path: "/deployments", body: a.spec })),
    },
    {
      name: "applb_scale",
      description:
        "Change a deployment's scaling parameters. Takes effect on the next reconcile; " +
        "scaling to zero lets the pool drain, which is not instant.",
      schema: {
        id: z.string(),
        scaling: z.record(z.unknown()).describe("the PATCH body app-lb expects"),
      },
      handler: async (a) =>
        json(
          await clients.applb({
            method: "PATCH",
            path: `/deployments/${enc(String(a.id))}/scaling`,
            body: a.scaling,
          }),
        ),
    },
    {
      name: "applb_start_build",
      description: "Start a build for a deployment. Returns a job; poll applb_deployment_jobs.",
      schema: { id: z.string(), body: z.record(z.unknown()).optional() },
      handler: async (a) =>
        json(
          await clients.applb({
            method: "POST",
            path: `/deployments/${enc(String(a.id))}/build`,
            body: a.body ?? {},
          }),
        ),
    },
    {
      name: "applb_start_update",
      description:
        "Start an update for a deployment — this rolls its VMs. Not destructive of data, but " +
        "it does replace running machines.",
      schema: { id: z.string(), body: z.record(z.unknown()).optional() },
      handler: async (a) =>
        json(
          await clients.applb({
            method: "POST",
            path: `/deployments/${enc(String(a.id))}/update`,
            body: a.body ?? {},
          }),
        ),
    },
    {
      name: "applb_deployment_jobs",
      description: "Recent build/pull/update jobs for a deployment, with their outcomes.",
      schema: { id: z.string() },
      handler: async (a) => json(await clients.applb({ path: `/deployments/${enc(String(a.id))}/jobs` })),
    },

    // ---- app-lb destructive -------------------------------------------
    {
      name: "applb_delete_deployment",
      description:
        DESTRUCTIVE +
        "Deregisters a deployment from app-lb and tears down its backends. The deployment " +
        "stops serving immediately. Not reversible from here — it must be registered again.",
      schema: { id: z.string() },
      handler: async (a) =>
        json(await clients.applb({ method: "DELETE", path: `/deployments/${enc(String(a.id))}` })),
    },
    {
      name: "applb_evict_vm",
      description:
        DESTRUCTIVE +
        "Removes one VM from a deployment's pool and destroys it. In-flight requests on that " +
        "VM are lost. The pool replaces it only if scaling allows.",
      schema: { id: z.string(), sandbox_id: z.string() },
      handler: async (a) =>
        json(
          await clients.applb({
            method: "DELETE",
            path: `/deployments/${enc(String(a.id))}/vms/${enc(String(a.sandbox_id))}`,
          }),
        ),
    },
    {
      name: "applb_purge_disk",
      description:
        DESTRUCTIVE + "Permanently deletes one disk and everything on it. There is no undo.",
      schema: { disk_id: z.string() },
      handler: async (a) =>
        json(await clients.applb({ method: "DELETE", path: `/disks/${enc(String(a.disk_id))}` })),
    },
    {
      name: "applb_purge_orphan_disks",
      description:
        DESTRUCTIVE +
        "Deletes every disk app-lb considers orphaned, in one call. Read applb_disks first and " +
        "confirm the list is what you expect — 'orphaned' is app-lb's inference, not a fact, " +
        "and a disk belonging to something app-lb has lost track of looks identical.",
      schema: {},
      handler: async () => json(await clients.applb({ method: "POST", path: "/disks/purge-orphans" })),
    },
    {
      name: "applb_sweep_disks",
      description: "Reclaims space by sweeping disks app-lb considers reclaimable.",
      schema: {},
      handler: async () => json(await clients.applb({ method: "POST", path: "/disks/sweep" })),
    },
    {
      name: "applb_exec",
      description:
        DESTRUCTIVE +
        "Runs a command inside a deployment's guest and returns its output. Arbitrary code " +
        "execution on a production VM — the same power as a shell. Useful for reading a guest " +
        "file nothing exports, notably /var/log/heyvm-start.log when a start_command fails and " +
        "app-obs has nothing.",
      schema: { id: z.string(), body: z.record(z.unknown()).describe("app-lb's exec body") },
      handler: async (a) =>
        json(
          await clients.applb({
            method: "POST",
            path: `/deployments/${enc(String(a.id))}/exec`,
            body: a.body,
          }),
        ),
    },

    // ---- ci ------------------------------------------------------------
    {
      name: "ci_cancel_run",
      description:
        DESTRUCTIVE +
        "Cancels a ci run and every unfinished job in it. Work already running is stopped at " +
        "its next step boundary.",
      schema: { run_id: z.string() },
      handler: async (a) =>
        json(await clients.ci({ method: "POST", path: `/runs/${enc(String(a.run_id))}/cancel` })),
    },
    {
      name: "ci_destroy_vm",
      description: DESTRUCTIVE + "Destroys one pooled ci VM. A claimed VM is refused.",
      schema: { sandbox_id: z.string() },
      handler: async (a) =>
        json(await clients.ci({ method: "POST", path: `/vms/${enc(String(a.sandbox_id))}/destroy` })),
    },
    {
      name: "ci_cleanup_failed_vms",
      description:
        DESTRUCTIVE + "Destroys every idle ci VM whose last run failed. Claimed VMs are refused.",
      schema: {},
      handler: async () => json(await clients.ci({ method: "POST", path: "/vms/cleanup-failed" })),
    },

    // ---- raw, for everything not named above ---------------------------
    ...(["applb", "obs", "ci"] as const).map((svc) => ({
      name: `${svc}_request`,
      description:
        `Raw HTTP against ${svc === "applb" ? "app-lb" : svc === "obs" ? "app-obs" : "ci"}, for ` +
        "endpoints without a dedicated tool above. Full surface, including methods that " +
        "destroy things — prefer a named tool when one exists, because this one's intent is " +
        "invisible until the arguments are read.",
      schema: {
        method: z.enum(["GET", "POST", "PUT", "PATCH", "DELETE"]).default("GET"),
        path: z.string().describe("path beginning with '/'"),
        query: z.record(z.string()).optional(),
        body: z.unknown().optional(),
      },
      handler: async (a: Record<string, unknown>) =>
        json(
          await clients[svc === "applb" ? "applb" : svc]({
            method: (a.method as string) ?? "GET",
            path: String(a.path),
            query: a.query as Record<string, string> | undefined,
            body: a.body,
          }),
        ),
    })),
  ];
}
