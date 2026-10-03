/**
 * Files an agent hands over, in the two shapes this server can receive them.
 *
 * Over stdio the host launched this process, so a `directory` on this
 * filesystem is the caller's own project, and reading it is the cheapest
 * correct way to move it: nothing passes through the conversation. Over HTTP
 * the caller is elsewhere, so only `files` (content inline, utf8 or base64)
 * can cross, and `directory` is not offered at all.
 *
 * Both feed two consumers: a server-side commit to the git remote, and a
 * `.tar.gz` bundle for the artifact store (the format a site pull unpacks).
 */

import { readdir, readFile, stat } from "node:fs/promises";
import { join, relative, sep } from "node:path";
import { gzipSync } from "node:zlib";

import { z } from "zod";

/** Same caps the git remote enforces, so a refusal happens here first. */
export const MAX_FILES = 10_000;
export const MAX_BYTES = 64 * 1024 * 1024;

/** What a directory read skips unless told otherwise. */
export const DEFAULT_EXCLUDE = [".git", "node_modules"];

export const fileSchema = z.object({
  path: z.string().describe("relative path, '/'-separated, e.g. 'src/index.html'"),
  content: z.string().optional().describe("file contents; omit with delete"),
  encoding: z.enum(["utf8", "base64"]).optional().describe("default utf8"),
  executable: z.boolean().optional(),
  delete: z.boolean().optional().describe("remove this path (repo commits only)"),
});

export type FileInput = z.infer<typeof fileSchema>;

export interface FileBytes {
  path: string;
  bytes: Uint8Array;
  executable: boolean;
}

/** Relative, forward-slash, nothing that escapes the tree or touches `.git`. */
export function validPath(p: string): boolean {
  return (
    p.length > 0 &&
    p.length <= 4096 &&
    !p.startsWith("/") &&
    !p.endsWith("/") &&
    !/[\0\\\n]/.test(p) &&
    p.split("/").every((s) => s !== "" && s !== "." && s !== ".." && s.toLowerCase() !== ".git")
  );
}

function checkPath(p: string): void {
  if (!validPath(p)) {
    throw new Error(
      `invalid path ${JSON.stringify(p)}: paths are relative, use '/', and may not contain ` +
        "'..', '.' segments or '.git'.",
    );
  }
}

/** Decode inline files; deletions are returned separately. */
export function decodeFiles(files: FileInput[]): { entries: FileBytes[]; deletes: string[] } {
  if (files.length > MAX_FILES) throw new Error(`at most ${MAX_FILES} files per call.`);
  const entries: FileBytes[] = [];
  const deletes: string[] = [];
  let total = 0;
  for (const f of files) {
    checkPath(f.path);
    if (f.delete) {
      deletes.push(f.path);
      continue;
    }
    if (f.content === undefined) {
      throw new Error(`${f.path}: \`content\` is required unless \`delete\` is true.`);
    }
    let bytes: Uint8Array;
    if (f.encoding === "base64") {
      const b = Buffer.from(f.content, "base64");
      // Buffer.from silently drops what it cannot decode; round-trip to notice.
      if (b.toString("base64").replace(/=+$/, "") !== f.content.replace(/\s+/g, "").replace(/=+$/, "")) {
        throw new Error(`${f.path}: content is not valid base64.`);
      }
      bytes = new Uint8Array(b);
    } else {
      bytes = new TextEncoder().encode(f.content);
    }
    total += bytes.byteLength;
    if (total > MAX_BYTES) throw tooBig();
    entries.push({ path: f.path, bytes, executable: Boolean(f.executable) });
  }
  return { entries, deletes };
}

function tooBig(): Error {
  return new Error(
    `more than ${MAX_BYTES >> 20} MiB of files. Push with git instead (repo_create returns ` +
      "the URL and a token), or publish a prebuilt bundle with art_publish.",
  );
}

/**
 * Every regular file under `dir`, skipping `exclude` names at any depth.
 * Symlinks are refused, as the git remote and app-lb's unpack both refuse them.
 */
export async function readDirectory(dir: string, exclude: string[] = DEFAULT_EXCLUDE): Promise<FileBytes[]> {
  const root = dir.replace(/\/+$/, "") || "/";
  let info;
  try {
    info = await stat(root);
  } catch (e) {
    throw new Error(
      `cannot read ${root}: ${e instanceof Error ? e.message : String(e)}. A directory is a path ` +
        "on THIS server's filesystem, which is the caller's only over stdio.",
    );
  }
  if (!info.isDirectory()) throw new Error(`${root} is not a directory.`);
  const skip = new Set(exclude);
  const out: FileBytes[] = [];
  let total = 0;
  const walk = async (d: string): Promise<void> => {
    for (const ent of await readdir(d, { withFileTypes: true })) {
      if (skip.has(ent.name)) continue;
      const full = join(d, ent.name);
      const rel = relative(root, full).split(sep).join("/");
      if (ent.isDirectory()) {
        await walk(full);
      } else if (ent.isFile()) {
        const bytes = new Uint8Array(await readFile(full));
        total += bytes.byteLength;
        if (total > MAX_BYTES) throw tooBig();
        if (out.length >= MAX_FILES) throw new Error(`more than ${MAX_FILES} files under ${root}.`);
        const mode = (await stat(full)).mode;
        out.push({ path: rel, bytes, executable: (mode & 0o111) !== 0 });
      } else {
        throw new Error(`${rel} is a symlink or special file; only regular files are accepted.`);
      }
    }
  };
  await walk(root);
  if (out.length === 0) throw new Error(`${root} has no files (after excluding ${exclude.join(", ")}).`);
  return out;
}

/** The files as base64 change entries, for the git remote's commit API. */
export function asChanges(entries: FileBytes[], deletes: string[] = []): FileInput[] {
  return [
    ...entries.map((e) => ({
      path: e.path,
      content: Buffer.from(e.bytes).toString("base64"),
      encoding: "base64" as const,
      ...(e.executable ? { executable: true } : {}),
    })),
    ...deletes.map((path) => ({ path, delete: true })),
  ];
}

// ---------------------------------------------------------------------------
// tar.gz, ustar format: what `tar czf bundle.tgz -C dist .` produces, and what
// app-lb's site pull unpacks.
// ---------------------------------------------------------------------------

function octal(n: number, width: number): string {
  return n.toString(8).padStart(width - 1, "0") + "\0";
}

function header(path: string, size: number, mode: number, mtime: number): Uint8Array {
  const h = new Uint8Array(512);
  const enc = new TextEncoder();
  let name = path;
  let prefix = "";
  if (enc.encode(name).length > 100) {
    // ustar splits a long path at a '/' into prefix (155) and name (100).
    const cut = path.lastIndexOf("/", 155);
    if (cut <= 0 || enc.encode(path.slice(cut + 1)).length > 100) {
      throw new Error(`${path}: path too long for a tar bundle`);
    }
    prefix = path.slice(0, cut);
    name = path.slice(cut + 1);
  }
  const put = (s: string, off: number, len: number) => h.set(enc.encode(s).slice(0, len), off);
  put(name, 0, 100);
  put(octal(mode, 8), 100, 8);
  put(octal(0, 8), 108, 8);
  put(octal(0, 8), 116, 8);
  put(octal(size, 12), 124, 12);
  put(octal(mtime, 12), 136, 12);
  put("        ", 148, 8); // checksum is computed with this field as spaces
  put("0", 156, 1);
  put("ustar\0", 257, 6);
  put("00", 263, 2);
  put(prefix, 345, 155);
  let sum = 0;
  for (const b of h) sum += b;
  put(octal(sum, 7) + " ", 148, 8);
  return h;
}

/** A gzipped ustar archive of `files` at their relative paths, sorted. */
export function tarGz(files: FileBytes[], mtime = 0): Uint8Array {
  const parts: Uint8Array[] = [];
  for (const f of [...files].sort((a, b) => a.path.localeCompare(b.path))) {
    parts.push(header(f.path, f.bytes.byteLength, f.executable ? 0o755 : 0o644, mtime));
    parts.push(f.bytes);
    const pad = (512 - (f.bytes.byteLength % 512)) % 512;
    if (pad) parts.push(new Uint8Array(pad));
  }
  parts.push(new Uint8Array(1024));
  return new Uint8Array(gzipSync(Buffer.concat(parts)));
}
