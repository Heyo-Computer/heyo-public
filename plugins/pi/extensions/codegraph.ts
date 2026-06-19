/**
 * codegraph extension for pi.
 *
 * Registers the tree-sitter-backed `codegraph` CLI as first-class tools
 * (search / definition / outline / snippet / references / patch) and wires
 * the agent lifecycle for token economy:
 *
 * - `session_start`: builds `.codegraph/index.json` if missing and keeps it
 *   live by spawning a detached `codegraph watch` daemon (pidfile-guarded,
 *   same conventions as the Claude Code plugin).
 * - `before_agent_start`: appends a short token-economy block to the system
 *   prompt steering the model toward outline/snippet/search over full reads
 *   and grep sweeps.
 * - `tool_call` (opt-in, PI_CODEGRAPH_ENFORCE=1): blocks the built-in
 *   read/edit tools on supported source files and redirects to codegraph.
 *
 * Supported languages: Rust, Python, JavaScript, TypeScript.
 */
import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { StringEnum } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const BIN = "codegraph";
const QUERY_TIMEOUT_MS = 30_000;
const INDEX_TIMEOUT_MS = 300_000;

const SYMBOL_KINDS = [
	"function",
	"method",
	"class",
	"struct",
	"enum",
	"trait",
	"interface",
	"module",
	"type",
	"constant",
	"variable",
] as const;

/** File extensions codegraph can parse. */
const SUPPORTED_SOURCE = /\.(rs|py|js|jsx|ts|tsx)$/;

const SYSTEM_PROMPT_GUIDANCE = `

## codegraph token economy

This repo has a live codegraph index. To keep context small:
- **Outline before reading.** Use codegraph_outline to see a file's signatures before deciding to read any of it.
- **Snippet, don't read.** Pull one symbol (or line range) with codegraph_snippet instead of reading a whole file.
- **Search, don't grep.** Locate symbols with codegraph_search / codegraph_definition instead of grep sweeps; cap noisy queries with limit.
- **Patch, don't rewrite.** Apply focused unified diffs with codegraph_patch instead of rewriting file contents.
Fall back to the built-in read/grep tools only for unsupported languages (anything other than Rust, Python, JavaScript, TypeScript) or when codegraph comes up empty.`;

function indexPath(cwd: string): string {
	return path.join(cwd, ".codegraph", "index.json");
}

function pidAlive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch {
		return false;
	}
}

export default async function (pi: ExtensionAPI) {
	// Resolve the binary up front; tools must be registered at factory time.
	const probe = await pi.exec(BIN, ["--version"], { timeout: 5_000 }).catch(() => undefined);
	const available = probe !== undefined && probe.code === 0;

	if (!available) {
		pi.on("session_start", (_event, ctx) => {
			ctx.ui.notify(
				"codegraph not found on PATH — codegraph tools disabled. Install with `make install-codegraph` from the printer repo.",
				"warning",
			);
		});
		return;
	}

	/** Run a codegraph subcommand with compact --text output; throw on failure. */
	async function run(args: string[], cwd: string, signal?: AbortSignal, timeout = QUERY_TIMEOUT_MS) {
		const result = await pi.exec(BIN, ["--text", ...args], { cwd, signal, timeout });
		if (result.killed) {
			throw new Error(`codegraph ${args[0]} timed out or was aborted`);
		}
		if (result.code !== 0) {
			throw new Error(result.stderr.trim() || result.stdout.trim() || `codegraph ${args[0]} failed (exit ${result.code})`);
		}
		return result.stdout.trim() || "(no results)";
	}

	// --- Lifecycle: index + watch daemon -----------------------------------

	let watchPid: number | undefined;

	pi.on("session_start", async (_event, ctx) => {
		const dir = path.join(ctx.cwd, ".codegraph");
		const pidfile = path.join(dir, "watch.pid");

		try {
			if (!fs.existsSync(indexPath(ctx.cwd))) {
				ctx.ui.setStatus("codegraph", "indexing…");
				const result = await pi.exec(BIN, ["index", ctx.cwd], { timeout: INDEX_TIMEOUT_MS });
				ctx.ui.setStatus("codegraph", result.code === 0 ? "indexed" : undefined);
				if (result.code !== 0) {
					ctx.ui.notify(`codegraph index failed: ${result.stderr.trim()}`, "warning");
					return;
				}
			}

			// Re-attach gracefully if a watch daemon is already running for this cwd.
			if (fs.existsSync(pidfile)) {
				const prev = Number.parseInt(fs.readFileSync(pidfile, "utf8").trim(), 10);
				if (Number.isFinite(prev) && pidAlive(prev)) {
					ctx.ui.setStatus("codegraph", `watch pid ${prev}`);
					return;
				}
				fs.rmSync(pidfile, { force: true });
			}

			fs.mkdirSync(dir, { recursive: true });
			const log = fs.openSync(path.join(dir, "watch.log"), "a");
			const child = spawn(BIN, ["watch", ctx.cwd], { detached: true, stdio: ["ignore", log, log] });
			child.unref();
			fs.closeSync(log);
			if (child.pid !== undefined) {
				watchPid = child.pid;
				fs.writeFileSync(pidfile, String(child.pid));
				ctx.ui.setStatus("codegraph", `watch pid ${child.pid}`);
			}
		} catch (err) {
			ctx.ui.notify(`codegraph setup failed: ${err instanceof Error ? err.message : String(err)}`, "warning");
		}
	});

	pi.on("session_shutdown", (_event, ctx) => {
		// Only reap a daemon this session spawned; leave pre-existing ones alone.
		if (watchPid === undefined) return;
		const pidfile = path.join(ctx.cwd, ".codegraph", "watch.pid");
		if (pidAlive(watchPid)) {
			try {
				process.kill(watchPid);
			} catch {
				// already gone
			}
		}
		try {
			const recorded = Number.parseInt(fs.readFileSync(pidfile, "utf8").trim(), 10);
			if (recorded === watchPid) fs.rmSync(pidfile, { force: true });
		} catch {
			// pidfile missing — nothing to clean up
		}
		watchPid = undefined;
	});

	// --- Lifecycle: steer the model toward the cheap path -------------------

	pi.on("before_agent_start", (event, ctx) => {
		if (!fs.existsSync(indexPath(ctx.cwd))) return;
		return { systemPrompt: event.systemPrompt + SYSTEM_PROMPT_GUIDANCE };
	});

	// Opt-in hard enforcement: block built-in read/edit on indexed source files.
	pi.on("tool_call", (event, ctx) => {
		if (process.env.PI_CODEGRAPH_ENFORCE !== "1") return;
		if (event.toolName !== "read" && event.toolName !== "edit") return;
		const target = (event.input as { path?: unknown }).path;
		if (typeof target !== "string" || !SUPPORTED_SOURCE.test(target)) return;
		if (!fs.existsSync(indexPath(ctx.cwd))) return;
		return {
			block: true,
			reason:
				event.toolName === "read"
					? `read is disabled for ${path.extname(target)} files in this project. Use codegraph_outline to skim the file, then codegraph_snippet to pull the symbol or line range you need.`
					: `edit is disabled for ${path.extname(target)} files in this project. Build a unified diff and apply it with codegraph_patch (use check=true to dry-run first).`,
		};
	});

	// --- Tools ---------------------------------------------------------------

	pi.registerTool({
		name: "codegraph_search",
		label: "codegraph search",
		description:
			"Search the code index by symbol name or signature substring. Returns matching symbols with file, line range, kind, and signature. Far cheaper than grep for locating code.",
		promptSnippet: "codegraph_search: find symbols by name/signature across the repo (use instead of grep)",
		promptGuidelines: [
			"Prefer codegraph_search over grep when locating functions, types, or other symbols by name.",
		],
		parameters: Type.Object({
			query: Type.String({ description: "Substring to match against symbol names (and signatures unless name_only is set)." }),
			kind: Type.Optional(StringEnum(SYMBOL_KINDS, { description: "Filter by symbol kind." })),
			name_only: Type.Optional(Type.Boolean({ description: "Match the qualified name only, skipping signature text. Default false." })),
			limit: Type.Optional(Type.Integer({ description: "Maximum hits to return. Default 50." })),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const args = ["search", params.query];
			if (params.kind) args.push("--kind", params.kind);
			if (params.name_only) args.push("--name");
			if (params.limit !== undefined) args.push("--limit", String(params.limit));
			const text = await run(args, ctx.cwd, signal);
			return { content: [{ type: "text", text }], details: undefined };
		},
	});

	pi.registerTool({
		name: "codegraph_definition",
		label: "codegraph definition",
		description: "Look up a symbol's definition(s) by exact qualified (e.g. `Foo::bar`) or bare name.",
		promptSnippet: "codegraph_definition: jump to a symbol's definition",
		parameters: Type.Object({
			symbol: Type.String({ description: "Qualified or bare symbol name." }),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const text = await run(["definition", params.symbol], ctx.cwd, signal);
			return { content: [{ type: "text", text }], details: undefined };
		},
	});

	pi.registerTool({
		name: "codegraph_outline",
		label: "codegraph outline",
		description:
			"Hierarchical outline of one file — signatures only, no bodies. Far cheaper than reading the whole file. Use this before deciding whether to read anything.",
		promptSnippet: "codegraph_outline: skim a file's signatures without reading bodies",
		promptGuidelines: [
			"Outline before reading: run codegraph_outline on a source file before reading any of its contents.",
		],
		parameters: Type.Object({
			file: Type.String({ description: "Path to the file, relative to the repo root or absolute." }),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const text = await run(["outline", params.file], ctx.cwd, signal);
			return { content: [{ type: "text", text }], details: undefined };
		},
	});

	pi.registerTool({
		name: "codegraph_snippet",
		label: "codegraph snippet",
		description:
			"Pull the source of one symbol or a line range from a file. Cheaper than reading the whole file. Pass `symbol` or `lines`, not both.",
		promptSnippet: "codegraph_snippet: pull one symbol or line range instead of reading a whole file",
		promptGuidelines: [
			"When you need one function from a large file, codegraph_snippet beats reading the full file.",
		],
		parameters: Type.Object({
			file: Type.String({ description: "Path to the file." }),
			symbol: Type.Optional(Type.String({ description: "Symbol name (qualified `Foo::bar` or bare `bar`)." })),
			lines: Type.Optional(Type.String({ description: "Line range, `start:end` or `start-end`." })),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			if (!params.symbol === !params.lines) {
				throw new Error("Pass exactly one of `symbol` or `lines`.");
			}
			const args = ["snippet", params.file];
			if (params.symbol) args.push(params.symbol);
			if (params.lines) args.push("--lines", params.lines);
			const text = await run(args, ctx.cwd, signal);
			return { content: [{ type: "text", text }], details: undefined };
		},
	});

	pi.registerTool({
		name: "codegraph_references",
		label: "codegraph references",
		description:
			"Find lexical references to a name across indexed files (word-boundary scan; may include comments/strings and miss dynamic dispatch).",
		promptSnippet: "codegraph_references: find usages of a name across the repo",
		parameters: Type.Object({
			symbol: Type.String({ description: "Name to scan for; qualified names are reduced to the bare trailing segment." }),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const text = await run(["references", params.symbol], ctx.cwd, signal);
			return { content: [{ type: "text", text }], details: undefined };
		},
	});

	pi.registerTool({
		name: "codegraph_patch",
		label: "codegraph patch",
		description:
			"Apply a unified diff (≥3 context lines) to one file. Validates context so silent corruption fails loudly; cheaper and more reviewable than rewriting file contents. Set check=true to dry-run in memory first.",
		promptSnippet: "codegraph_patch: apply a focused unified diff instead of rewriting a file",
		promptGuidelines: [
			"Edit source files by applying unified diffs with codegraph_patch; keep one logical change per patch.",
			"If a patch fails, re-pull the region with codegraph_snippet and rebuild the diff — don't fall back to full-file rewrites.",
		],
		parameters: Type.Object({
			file: Type.String({ description: "Path to the file to patch." }),
			diff: Type.String({ description: "Unified diff text with at least 3 lines of context per hunk." }),
			check: Type.Optional(Type.Boolean({ description: "Parse and apply in memory only; report success without modifying the file. Default false." })),
			allow_outside: Type.Optional(Type.Boolean({ description: "Allow patching files outside the working directory. Default false." })),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			// pi.exec has no stdin plumbing, so hand the diff over via temp file.
			const tmp = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "codegraph-patch-")), "change.patch");
			try {
				fs.writeFileSync(tmp, params.diff.endsWith("\n") ? params.diff : `${params.diff}\n`);
				const args = ["patch", params.file, "--diff", tmp];
				if (params.check) args.push("--check");
				if (params.allow_outside) args.push("--allow-outside");
				const text = await run(args, ctx.cwd, signal);
				return { content: [{ type: "text", text }], details: undefined };
			} finally {
				fs.rmSync(path.dirname(tmp), { recursive: true, force: true });
			}
		},
	});
}
