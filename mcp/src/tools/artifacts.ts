/**
 * The artifact store: where a deployment's bytes come from.
 *
 * ## Why this file exists
 *
 * Publishing a build is the most common thing anyone asks this server to do,
 * and until now it could not do it at all. `applb_*` can roll a deployment but
 * cannot get new bytes to it, so "update the `marketing` site with this build"
 * dead-ended halfway through every time — the tools covered every step except
 * the one that moves the artifact.
 *
 * ## The three-step sequence, and why it is a composite tool
 *
 * A publish is three requests, not two, and the order and the digests matter:
 *
 * 1. `PUT /blobs/{sha256}` — the bytes, at their own hash.
 * 2. `PUT /manifests` — `{schema:1, kind:"generic", entries:[{name,digest,size}]}`.
 *    Answers `{digest}`, which is the *manifest's* digest, not the blob's.
 * 3. `PUT /tags/{tag}` — that manifest digest, as `text/plain`.
 *
 * **A tag names a manifest, never a blob.** The store does not check this:
 * `set_tag` writes whatever digest it is handed, so tagging a blob digest
 * succeeds, and then every reader fails to resolve it — a tag that looks
 * correct in a listing and works for nobody. It is the single easiest thing to
 * get wrong here, which is exactly why {@link publishTool} exists as one call
 * instead of three primitives with a warning in the description. A composite
 * that always uses the manifest digest cannot make the mistake.
 *
 * The primitives are still exposed below, because the store has uses this
 * composite does not cover, and a tool set that can only do the one blessed
 * workflow is a tool set people work around.
 */

import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

import { z } from "zod";
import type { Clients } from "../clients/index.js";
import { json } from "../format.js";
import type { Tool } from "./diagnose.js";

/** The manifest kind for a plain collection of files. Mirrors `KIND_GENERIC`. */
const KIND_GENERIC = "generic";

/** The schema version the store's readers accept. Mirrors `SCHEMA_VERSION`. */
const SCHEMA_VERSION = 1;

/**
 * How the caller supplied the bytes.
 *
 * Two ways, because this server runs in two shapes. Over stdio the host
 * launched the process and a path on this filesystem is the caller's own file,
 * which is both the natural way to say it and the only way that does not put a
 * whole build through the conversation. Over HTTP there is no shared
 * filesystem, so base64 is the only thing that can cross — at a real cost in
 * tokens, which the description says out loud rather than letting somebody
 * discover it with a 200 MB rootfs.
 */
async function bytesOf(args: Record<string, unknown>): Promise<Uint8Array> {
  const path = typeof args.path === "string" ? args.path.trim() : "";
  const b64 = typeof args.content_base64 === "string" ? args.content_base64.trim() : "";
  if (path && b64) {
    throw new Error("give either `path` or `content_base64`, not both.");
  }
  if (path) {
    try {
      return new Uint8Array(await readFile(path));
    } catch (e) {
      throw new Error(
        `could not read ${path}: ${e instanceof Error ? e.message : String(e)}. ` +
          "A path is this server's filesystem, not the caller's — over HTTP those are " +
          "different machines, and `content_base64` is what crosses.",
      );
    }
  }
  if (b64) {
    const bytes = Buffer.from(b64, "base64");
    // `Buffer.from` never throws on bad base64; it silently drops what it
    // cannot decode. Round-tripping is the only way to notice, and noticing
    // matters here because the digest is the artifact's *name*: a truncated
    // decode publishes real bytes under a name nothing will ever ask for.
    if (bytes.toString("base64").replace(/=+$/, "") !== b64.replace(/\s+/g, "").replace(/=+$/, "")) {
      throw new Error(
        "`content_base64` is not valid base64 — it decoded to something that does not " +
          "re-encode to what was sent. Nothing was published; the digest would have named " +
          "bytes you did not mean.",
      );
    }
    return new Uint8Array(bytes);
  }
  throw new Error("no bytes: give `path` (stdio) or `content_base64` (HTTP).");
}

function sha256(bytes: Uint8Array): string {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

interface ManifestEntry {
  name: string;
  digest: string;
  size: number;
}

function publishTool(clients: Clients): Tool {
  return {
    name: "art_publish",
    description:
      "Publish a bundle to the artifact store and point a tag at it. THE TOOL TO USE for " +
      "'update deployment X with this build' — it is the step applb_start_update cannot do, " +
      "because app-lb rolls a deployment onto bytes that must already be in the store.\n\n" +
      "Does the whole three-request sequence in the right order: PUT the blob at its sha256, " +
      "PUT a manifest naming it, then point the tag at THE MANIFEST'S digest. That last part " +
      "is the one that goes wrong by hand — a tag must name a manifest, the store does not " +
      "check it, and a tag pointing at a blob digest is accepted and then resolves for " +
      "nobody.\n\n" +
      "Give the bytes as `path` (a file on this server — right for stdio, where the host " +
      "launched this process) or `content_base64` (right for HTTP, where the caller's " +
      "filesystem is somewhere else; costs ~4 tokens per 3 bytes, so it is for bundles, not " +
      "rootfs images).\n\n" +
      "Idempotent: the store is content-addressed, so re-publishing identical bytes writes " +
      "nothing new and just moves the tag. Follow with applb_start_update to roll the " +
      "deployment onto it.",
    schema: {
      tag: z.string().describe("the tag to point at this build, e.g. 'marketing-site'"),
      path: z.string().optional().describe("file on THIS server's filesystem"),
      content_base64: z.string().optional().describe("the bundle's bytes, base64"),
      name: z
        .string()
        .optional()
        .describe("entry name inside the manifest; defaults to the tag"),
      kind: z
        .string()
        .optional()
        .describe(`manifest kind; defaults to '${KIND_GENERIC}'`),
      annotations: z
        .record(z.string())
        .optional()
        .describe("free-form manifest annotations, e.g. a git sha"),
    },
    handler: async (a) => {
      const tag = String(a.tag ?? "").trim();
      if (!tag) throw new Error("`tag` is required — a publish nothing names is unreachable.");

      const bytes = await bytesOf(a);
      const digest = sha256(bytes);
      const entry: ManifestEntry = {
        name: (a.name as string | undefined)?.trim() || tag,
        digest,
        size: bytes.byteLength,
      };

      // 1. The blob, at its own hash. Raw bytes: JSON-encoding the body would
      //    both corrupt it and change the digest it is being stored under.
      await clients.art({
        method: "PUT",
        path: `/blobs/${encodeURIComponent(digest)}`,
        rawBody: bytes,
        contentType: "application/octet-stream",
      });

      // 2. The manifest. Its digest is a pure function of its content, so this
      //    is idempotent too — the same manifest re-put is the same address.
      const manifest = {
        schema: SCHEMA_VERSION,
        kind: (a.kind as string | undefined)?.trim() || KIND_GENERIC,
        entries: [entry],
        ...(a.annotations ? { annotations: a.annotations } : {}),
      };
      const created = await clients.art({ method: "PUT", path: "/manifests", body: manifest });
      const manifestDigest =
        created && typeof created === "object" && typeof (created as { digest?: unknown }).digest === "string"
          ? (created as { digest: string }).digest
          : undefined;
      if (!manifestDigest) {
        // Refusing to continue is the whole point: the next request would
        // otherwise be a tag pointing at *something*, and the store would
        // accept it. Better a failed publish than a tag nothing can resolve.
        throw new Error(
          `the store accepted the manifest but did not answer with its digest ` +
            `(got ${json(created, 200)}). The blob at ${digest} is stored and the tag was NOT ` +
            "moved, so nothing is pointing at a half-finished publish.",
        );
      }

      // 3. The tag, naming the MANIFEST. `text/plain`, and the manifest digest
      //    rather than the blob's — see this module's header.
      await clients.art({
        method: "PUT",
        path: `/tags/${encodeURIComponent(tag)}`,
        rawBody: manifestDigest,
        contentType: "text/plain",
      });

      return json({
        published: tag,
        blob: { digest, size: entry.size, name: entry.name },
        manifest: { digest: manifestDigest, kind: manifest.kind },
        tag_points_at: manifestDigest,
        next: "applb_start_update rolls a deployment onto this; applb_deployment_jobs polls it.",
      });
    },
  };
}

export function artifactTools(clients: Clients): Tool[] {
  const enc = encodeURIComponent;

  return [
    publishTool(clients),

    {
      name: "art_list_tags",
      description:
        "Every tag in the store and the digest it points at. The listing to read before " +
        "publishing over a tag, and after, to confirm it moved.",
      schema: {},
      handler: async () => json(await clients.art({ path: "/tags" })),
    },
    {
      name: "art_get_tag",
      description:
        "What one tag points at. Answers a bare digest, not JSON.\n\n" +
        "A 405 here rather than a digest means the store is running a build from before this " +
        "route existed — the fix is to redeploy it, not to work around it. `art_list_tags` " +
        "answers the same question on an old build.",
      schema: { tag: z.string() },
      handler: async (a) =>
        json(
          await clients.art({ path: `/tags/${enc(String(a.tag))}`, expectText: true }),
        ),
    },
    {
      name: "art_get_manifest",
      description:
        "One manifest by digest or by tag: its kind, its entries and their digests and sizes. " +
        "How to check what a tag actually resolves to — a tag pointing at a blob rather than " +
        "a manifest fails HERE, which is the fastest way to confirm that diagnosis.",
      schema: { reference: z.string().describe("a manifest digest, or a tag name") },
      handler: async (a) =>
        json(await clients.art({ path: `/manifests/${enc(String(a.reference))}` })),
    },
    {
      name: "art_list_blobs",
      description:
        "Every blob with its size, its label and the tags pointing at it. Answers 'what is in " +
        "this store' and 'what is taking up the space'.",
      schema: {},
      handler: async () => json(await clients.art({ path: "/blobs" })),
    },
    {
      name: "art_usage",
      description:
        "The store's disk usage. Worth reading before a large publish: the store refuses a " +
        "write that would take it under ART_MIN_FREE_BYTES, and that refusal at the end of " +
        "an upload is an expensive way to find out.",
      schema: {},
      handler: async () => json(await clients.art({ path: "/usage" })),
    },
    {
      name: "art_request",
      description:
        "Raw HTTP against the artifact store, for endpoints without a dedicated tool above. " +
        "Prefer art_publish for publishing — this one can perform the three steps in the " +
        "wrong order or tag a blob digest, both of which the store accepts and no reader can " +
        "resolve.",
      schema: {
        method: z.enum(["GET", "POST", "PUT", "PATCH", "DELETE"]).default("GET"),
        path: z.string().describe("path beginning with '/'"),
        query: z.record(z.string()).optional(),
        body: z.unknown().optional().describe("JSON body"),
        text_body: z.string().optional().describe("body sent verbatim as text/plain"),
      },
      handler: async (a) =>
        json(
          await clients.art({
            method: (a.method as string) ?? "GET",
            path: String(a.path),
            query: a.query as Record<string, string> | undefined,
            body: a.body,
            rawBody: a.text_body as string | undefined,
            contentType: a.text_body === undefined ? undefined : "text/plain",
          }),
        ),
    },
  ];
}
