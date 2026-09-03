/**
 * heyo cloud: the sandbox control plane.
 *
 * One endpoint, many sandboxes. Every tool here names the sandbox it acts on,
 * so a client holds one connection for its whole life and no per-sandbox
 * session exists to lose on a restart. A sandbox outlives the call that made
 * it — it is a VM with a TTL, not a request scope — which is what lets an agent
 * hold a conversation across replies instead of rebuilding its world each time.
 *
 * # The 1 MB body, and the way around it
 *
 * Cloud's JSON API caps a request body at 1 MB, and file writes travel through
 * it base64-encoded, so ~768 KB of payload is already `413 Request Entity Too
 * Large`. That ceiling is real and it is not MCP's — it is the same limit the
 * SDK meets. What gets past it is the archive path, and its shape matters:
 * {@link uploadUrlTool} hands back a **presigned object-store URL** that the
 * caller PUTs the bytes to directly. Those bytes never enter a JSON body, never
 * pass through this server, and are bounded by the object store rather than by
 * the API — which is why a phone video works and an inline write of the same
 * file cannot. `sandbox_write_file` refuses oversized content rather than
 * spending a round trip to be told 413, and says which tool to use instead.
 *
 * # What is destructive
 *
 * `sandbox_kill` destroys the VM and its disk. `sandbox_stop` does not: a
 * stopped sandbox keeps its disk and starts again with its files, which is the
 * cheap way to park a conversation that may resume. TTL expiry reaps only a
 * *running* sandbox, so parking one is also how you stop the clock.
 */

import { z } from "zod";
import type { Clients } from "../clients/index.js";
import { settle } from "../clients/index.js";
import { json, report, section } from "../format.js";
import { ServiceError } from "../http.js";
import type { Tool } from "./diagnose.js";

const DESTRUCTIVE = "DESTRUCTIVE. ";

/**
 * The most content a write may carry inline, in bytes before encoding.
 *
 * Measured against the live API rather than derived: 512 KB succeeds, 768 KB
 * returns 413, which brackets a 1 MB body with base64's 4/3 expansion —
 * 768 KB × 4/3 is exactly 1024 KB. 512 KiB encodes to ~683 KB and leaves room
 * for the rest of the envelope.
 */
export const INLINE_WRITE_LIMIT = 512 * 1024;

/** Cloud reports these and stops changing; anything else is still moving. */
const TERMINAL = new Set(["running", "failed", "stopped", "paused", "cold-stored"]);

const READY_POLL_MS = 2_000;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function statusOf(info: unknown): string {
  const s = (info as { status?: unknown })?.status;
  return typeof s === "string" ? s : "unknown";
}

/** A 503 from cloud is capacity, and capacity is the one failure worth waiting on. */
function isCapacity(e: unknown): boolean {
  return e instanceof ServiceError && e.status === 503;
}

export function sandboxTools(clients: Clients): Tool[] {
  const enc = encodeURIComponent;
  const sandboxPath = (id: unknown, suffix = "") => `/deployed-sandboxes/${enc(String(id))}${suffix}`;

  const info = (id: unknown) => clients.cloud({ path: sandboxPath(id) });

  /**
   * Poll until the sandbox stops being `provisioning`, or the budget runs out.
   *
   * A 404 inside the budget is tolerated, not fatal: a sandbox can be created
   * and not yet resolvable by id for a moment, and failing there would report a
   * VM that is booting fine as one that does not exist.
   */
  async function waitForReady(id: string, seconds: number): Promise<unknown> {
    const deadline = Date.now() + seconds * 1000;
    for (;;) {
      let current: unknown;
      try {
        current = await info(id);
      } catch (e) {
        if (!(e instanceof ServiceError && e.status === 404) || Date.now() >= deadline) throw e;
        await sleep(READY_POLL_MS);
        continue;
      }
      if (TERMINAL.has(statusOf(current))) return current;
      if (Date.now() >= deadline) return current;
      await sleep(READY_POLL_MS);
    }
  }

  return [
    {
      name: "sandbox_create",
      description:
        "Boot a sandbox (a microVM) and return it once it is running. The id it " +
        "returns is the only handle any other tool needs — sandboxes are addressed by " +
        "id on this one endpoint, not by a per-sandbox connection.\n\n" +
        "It outlives this call. `ttl_seconds` is how long a *running* sandbox survives " +
        "unattended (default is the account's); sandbox_set_ttl renews it at any point, " +
        "and sandbox_stop parks it — disk kept, clock stopped — for a later " +
        "sandbox_start. So a conversation can keep one sandbox rather than rebuilding " +
        "its world per reply.\n\n" +
        "`archive_id` seeds /workspace from an already-uploaded archive: that is how " +
        "content too large for a JSON body gets in. See sandbox_upload_url.\n\n" +
        "A 503 here is region capacity, not a fault. `retries` retries exactly that, " +
        "with exponential backoff; everything else fails immediately, because retrying " +
        "a rejected spec only rejects it again.",
      schema: {
        image: z.string().optional().describe("default 'ubuntu:24.04'"),
        size_class: z
          .enum(["micro", "mini", "small", "medium", "large", "xlarge"])
          .optional()
          .describe("default 'small'"),
        region: z.enum(["US", "EU"]).optional().describe("default 'US'"),
        driver: z
          .enum(["firecracker", "libvirt", "kvm", "firecracker_containerd"])
          .optional()
          .describe(
            "leave unset to let cloud place it; naming one the region does not run is " +
              "the usual cause of a 503",
          ),
        name: z.string().optional(),
        archive_id: z.string().optional().describe("seed /workspace from this archive"),
        start_command: z.string().optional(),
        working_directory: z.string().optional(),
        env_vars: z.record(z.string()).optional(),
        open_ports: z.array(z.number()).optional(),
        setup_hooks: z.array(z.string()).optional(),
        ttl_seconds: z.number().optional().describe("0 means unlimited, if the plan allows"),
        disk_size_gb: z.number().optional(),
        wait_seconds: z
          .number()
          .optional()
          .describe("how long to wait for it to leave 'provisioning'; default 120, 0 returns at once"),
        retries: z.number().optional().describe("capacity (503) retries; default 3, 0 disables"),
      },
      handler: async (a) => {
        // The SDK fills these in before it posts, and a tool that did not would
        // boot something subtly different from what the same call boots in code.
        const body: Record<string, unknown> = {
          region: a.region ?? "US",
          image: a.image ?? "ubuntu:24.04",
          size_class: a.size_class ?? "small",
          open_ports: a.open_ports ?? [],
        };
        for (const k of [
          "name",
          "archive_id",
          "driver",
          "start_command",
          "working_directory",
          "env_vars",
          "setup_hooks",
          "ttl_seconds",
          "disk_size_gb",
        ] as const) {
          if (a[k] !== undefined) body[k] = a[k];
        }

        const retries = Math.max(0, Number(a.retries ?? 3));
        let created: unknown;
        for (let attempt = 0; ; attempt++) {
          try {
            created = await clients.cloud({ method: "POST", path: "/sandbox-deploy", body });
            break;
          } catch (e) {
            if (!isCapacity(e) || attempt >= retries) throw e;
            await sleep(2_000 * 2 ** attempt);
          }
        }

        const id = (created as { id?: unknown })?.id;
        const wait = Number(a.wait_seconds ?? 120);
        if (!id || wait <= 0) return json(created);
        const settled = await waitForReady(String(id), wait);
        const status = statusOf(settled);
        return report(`Sandbox ${id} — ${status}`, [
          { title: "Created", body: created },
          {
            title:
              status === "provisioning"
                ? `Still provisioning after ${wait}s — poll sandbox_info`
                : "Current state",
            body: settled,
          },
        ]);
      },
    },

    {
      name: "sandbox_list",
      description:
        "Every sandbox this key can see, with status, image, uptime and bound URLs. " +
        "The way to find a sandbox again after a restart — the id is the state worth " +
        "keeping, and this recovers it if that was lost.",
      schema: { name: z.string().optional().describe("exact display-name filter") },
      handler: async (a) =>
        json(
          await clients.cloud({
            path: "/deployed-sandboxes",
            query: { name: a.name as string | undefined },
          }),
        ),
    },

    {
      name: "sandbox_info",
      description:
        "One sandbox by id: status, region, size, TTL and bound URLs. Resolves even " +
        "for a stopped sandbox, which can be transiently missing from sandbox_list " +
        "right after it stops. This is the readiness poll after a create that did not " +
        "wait — 'provisioning' is still moving, 'failed' carries the reason.",
      schema: { id: z.string() },
      handler: async (a) => json(await info(a.id)),
    },

    {
      name: "sandbox_exec",
      description:
        "Run a command in the sandbox with `sh -c` and return stdout, stderr and " +
        "exit_code. A non-zero exit is a successful call with a non-zero code in it, " +
        "not an error — read exit_code.\n\n" +
        "There is no streaming and no cancellation: output is buffered until the " +
        "command exits, and a command that outlives this server's request timeout " +
        "keeps running in the guest while the call fails. Long work wants nohup and a " +
        "log file the next exec reads.",
      schema: {
        id: z.string(),
        command: z.string(),
        cwd: z.string().optional(),
        env: z.record(z.string()).optional(),
      },
      handler: async (a) =>
        json(
          await clients.cloud({
            method: "POST",
            path: `/sandbox/${enc(String(a.id))}/exec`,
            body: { command: a.command, cwd: a.cwd, env: a.env },
          }),
        ),
    },

    {
      name: "sandbox_read_file",
      description:
        "Read a file from the sandbox. The content crosses as base64 inside a JSON " +
        "body, so cloud's 1 MB body limit applies to the *response* too — a large file " +
        "is better tarred and served over a bound port, or summarised in the guest " +
        "with sandbox_exec.",
      schema: {
        id: z.string(),
        file_path: z.string(),
        mount_path: z.string().optional().describe("default '/workspace'"),
        encoding: z
          .enum(["text", "base64"])
          .optional()
          .describe("'text' decodes UTF-8; 'base64' returns the wire value. Default 'text'."),
      },
      handler: async (a) => {
        const out = await clients.cloud({
          method: "POST",
          path: `/sandbox/${enc(String(a.id))}/read-file`,
          body: { file_path: a.file_path, mount_path: a.mount_path ?? "/workspace" },
        });
        const content = (out as { content?: unknown })?.content;
        if ((a.encoding ?? "text") !== "text" || typeof content !== "string") return json(out);
        return json(Buffer.from(content, "base64").toString("utf8"));
      },
    },

    {
      name: "sandbox_write_file",
      description:
        "Write a file into the sandbox. Content travels base64 in a JSON body, so this " +
        `is capped at ${INLINE_WRITE_LIMIT / 1024} KiB — measured, not guessed: 512 KB ` +
        "succeeds and 768 KB returns 413 against cloud's 1 MB body limit. Anything " +
        "larger is refused here rather than sent to be rejected, and the refusal names " +
        "the route that has no such limit: sandbox_upload_url → PUT → " +
        "sandbox_finalize_upload → sandbox_attach_archive.",
      schema: {
        id: z.string(),
        file_path: z.string(),
        content: z.string().optional().describe("UTF-8 text"),
        content_base64: z.string().optional().describe("binary, base64-encoded"),
        mount_path: z.string().optional().describe("default '/workspace'"),
      },
      handler: async (a) => {
        const bytes =
          typeof a.content_base64 === "string"
            ? Buffer.from(a.content_base64, "base64")
            : Buffer.from(String(a.content ?? ""), "utf8");
        if (bytes.byteLength > INLINE_WRITE_LIMIT) {
          throw new Error(
            `${bytes.byteLength} bytes is past the ${INLINE_WRITE_LIMIT}-byte inline ` +
              "write limit — base64 of that would exceed cloud's 1 MB body limit and " +
              "come back 413. Use the archive route instead: sandbox_upload_url returns " +
              "a presigned URL to PUT the bytes to directly (no JSON body, no 1 MB " +
              "ceiling), then sandbox_finalize_upload and sandbox_attach_archive put " +
              "them in the sandbox.",
          );
        }
        return json(
          await clients.cloud({
            method: "POST",
            path: `/sandbox/${enc(String(a.id))}/write-file`,
            body: {
              file_path: a.file_path,
              mount_path: a.mount_path ?? "/workspace",
              content: bytes.toString("base64"),
            },
          }),
        );
      },
    },

    {
      name: "sandbox_upload_url",
      description:
        "Reserve an archive and return a presigned URL to PUT its bytes to. **This is " +
        "the way past the 1 MB body limit.** The URL belongs to the object store, not " +
        "to cloud: PUT the bytes straight to it with `Content-Type: application/gzip` " +
        "and *no* Authorization header — the signature is the credential, and a bearer " +
        "alongside it is what makes some stores refuse. The bytes never touch this " +
        "server or any JSON body, so the ceiling is the object store's, not the API's: " +
        "hundreds of MB — phone video — is the case this exists for.\n\n" +
        "The archive is a **tar.gz** whose contents become a mount; a single photo is " +
        "a one-entry tarball. Then call sandbox_finalize_upload with the archive_id, " +
        "and either pass that id to sandbox_create or sandbox_attach_archive it onto a " +
        "sandbox already running.",
      schema: {},
      handler: async () => {
        const out = await clients.cloud({ method: "POST", path: "/sandbox-archives/presign" });
        return report("Presigned archive upload", [
          { title: "Reservation", body: out },
          {
            title: "Next",
            body: [
              "PUT the tar.gz to upload_url with Content-Type: application/gzip and no Authorization header",
              "sandbox_finalize_upload { archive_id }",
              "sandbox_create { archive_id } — or sandbox_attach_archive { id, archive_id } for a running sandbox",
            ],
          },
        ]);
      },
    },

    {
      name: "sandbox_finalize_upload",
      description:
        "Close out an upload started by sandbox_upload_url: the archive is only usable " +
        "once finalized. Returns its id and size — a size of zero means the PUT did not " +
        "land, which is worth checking before wondering why a mount is empty.",
      schema: {
        archive_id: z.string(),
        name: z.string().optional().describe("display name"),
        sandbox_id: z
          .string()
          .optional()
          .describe(
            "a label stored with the archive, not a sandbox to attach it to — the SDK " +
              "passes the source directory's name. Attaching is sandbox_attach_archive.",
          ),
      },
      handler: async (a) =>
        json(
          await clients.cloud({
            method: "POST",
            path: `/sandbox-archives/${enc(String(a.archive_id))}/finalize`,
            body: { sandbox_id: a.sandbox_id ?? String(a.name ?? a.archive_id), name: a.name },
          }),
        ),
    },

    {
      name: "sandbox_attach_archive",
      description:
        "Mount a finalized archive onto a sandbox that is already running, replacing " +
        "what is at `sandbox_path`. This is what makes the archive route usable " +
        "mid-conversation: a large file can reach a sandbox that already exists, " +
        "without booting a new one to carry it.\n\n" +
        "It *replaces* the mount — anything at that path and not in the archive is " +
        "gone. Mount somewhere of its own if the sandbox's existing /workspace matters.",
      schema: {
        id: z.string(),
        archive_id: z.string(),
        sandbox_path: z.string().optional().describe("default '/workspace'"),
      },
      handler: async (a) =>
        json(
          await clients.cloud({
            method: "POST",
            path: sandboxPath(a.id, "/replace-mount"),
            body: { archive_id: a.archive_id, sandbox_path: a.sandbox_path ?? "/workspace" },
          }),
        ),
    },

    {
      name: "sandbox_set_ttl",
      description:
        "Reset how long the sandbox may run unattended, from now. This is the keepalive " +
        "for a sandbox that has to survive between replies: call it whenever the " +
        "conversation touches the sandbox and it never ages out mid-thread. `0` is " +
        "unlimited where the plan allows it.\n\n" +
        "Only a *running* sandbox is reaped, so a stopped one is not on this clock at " +
        "all — sandbox_stop is the other way to keep one indefinitely.",
      schema: { id: z.string(), ttl_seconds: z.number() },
      handler: async (a) =>
        json(
          await clients.cloud({
            method: "POST",
            path: sandboxPath(a.id, "/ttl"),
            body: { ttl_seconds: a.ttl_seconds },
          }),
        ),
    },

    {
      name: "sandbox_stop",
      description:
        "Stop the sandbox without destroying it. The disk survives, so sandbox_start " +
        "brings back the same files; it costs disk and no memory, and a stopped sandbox " +
        "is off the TTL reaper. The way to park a conversation that may resume.",
      schema: { id: z.string() },
      handler: async (a) =>
        json(await clients.cloud({ method: "POST", path: `/sandbox/${enc(String(a.id))}/stop` })),
    },

    {
      name: "sandbox_start",
      description: "Start a stopped sandbox again, with its disk as it was left.",
      schema: { id: z.string() },
      handler: async (a) =>
        json(await clients.cloud({ method: "POST", path: `/sandbox/${enc(String(a.id))}/start` })),
    },

    {
      name: "sandbox_restart",
      description:
        "Reboot the sandbox. The disk survives; anything held only in memory, and any " +
        "process not started by the image, does not.",
      schema: { id: z.string() },
      handler: async (a) => json(await clients.cloud({ method: "POST", path: sandboxPath(a.id, "/restart") })),
    },

    {
      name: "sandbox_kill",
      description:
        DESTRUCTIVE +
        "Permanently deletes the sandbox and its disk. Everything written in it is " +
        "gone and the id stops resolving. Use sandbox_stop if it might be wanted again.",
      schema: { id: z.string() },
      handler: async (a) => json(await clients.cloud({ method: "DELETE", path: sandboxPath(a.id) })),
    },

    {
      name: "heyo_capacity",
      description:
        "What can be told about capacity *before* booting something.\n\n" +
        "Be clear about what this does and does not answer. It lists the daemons this " +
        "key has registered — your own hosts — with online/stale/offline, and every " +
        "sandbox already running against it. If your work lands on your own hosts, an " +
        "offline daemon here is the whole explanation for a failed boot. For " +
        "heyo-hosted regions there is no public capacity endpoint: cloud does not " +
        "publish free capacity per region or driver, so a 503 from sandbox_create is " +
        "the first and only signal, and this call cannot pre-empt it. Treat that 503 " +
        "as capacity, back off exponentially, and try another driver or region.",
      schema: {},
      handler: async () => {
        const r = await settle({
          daemons: clients.cloud({ path: "/me/daemons" }),
          sandboxes: clients.cloud({ path: "/deployed-sandboxes" }),
        });
        const counts = r.sandboxes.ok && Array.isArray(r.sandboxes.value)
          ? (r.sandboxes.value as unknown[]).reduce<Record<string, number>>((acc, s) => {
              const k = statusOf(s);
              acc[k] = (acc[k] ?? 0) + 1;
              return acc;
            }, {})
          : null;
        return report("Capacity, as far as it is knowable", [
          section("Your registered daemons (online / stale / offline)", r.daemons),
          { title: "Sandboxes by status", body: counts },
          {
            title: "Not covered",
            body:
              "heyo-hosted region capacity. Cloud publishes no per-region or per-driver " +
              "free capacity, so a 503 on create is the only signal that a region is " +
              "full — retry with backoff rather than pre-flighting.",
          },
        ]);
      },
    },
  ];
}
