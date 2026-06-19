// Smoke test: load both extensions with a mocked ExtensionAPI and exercise
// the codegraph tools against the real repo index. Run from plugins/pi:
//   node scripts/smoke.mjs
// Requires: `npm install` here, codegraph on PATH, an index at the repo root.
import { execFile } from "node:child_process";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");

function makeMockPi() {
	const tools = new Map();
	const handlers = new Map();
	return {
		tools,
		handlers,
		registerTool(tool) {
			tools.set(tool.name, tool);
		},
		on(event, handler) {
			if (!handlers.has(event)) handlers.set(event, []);
			handlers.get(event).push(handler);
		},
		exec(command, args, options = {}) {
			return new Promise((resolve) => {
				execFile(
					command,
					args,
					{ cwd: options.cwd, timeout: options.timeout, signal: options.signal, maxBuffer: 64 * 1024 * 1024 },
					(error, stdout, stderr) => {
						resolve({
							stdout: stdout ?? "",
							stderr: stderr ?? "",
							code: error ? (typeof error.code === "number" ? error.code : 1) : 0,
							killed: Boolean(error?.killed),
						});
					},
				);
			});
		},
	};
}

const ctx = {
	cwd: repoRoot,
	ui: {
		notify: (msg, type) => console.log(`  [notify:${type ?? "info"}] ${msg}`),
		setStatus: () => {},
	},
};

let failures = 0;
async function check(name, fn) {
	try {
		await fn();
		console.log(`ok   ${name}`);
	} catch (err) {
		failures++;
		console.error(`FAIL ${name}: ${err.message}`);
	}
}

function firstText(result) {
	const block = result.content.find((c) => c.type === "text");
	if (!block) throw new Error("no text content in result");
	return block.text;
}

// --- codegraph extension ----------------------------------------------------

const codegraph = (await import("../extensions/codegraph.ts")).default;
const cgPi = makeMockPi();
await codegraph(cgPi);

await check("codegraph registers 6 tools", () => {
	const expected = ["codegraph_search", "codegraph_definition", "codegraph_outline", "codegraph_snippet", "codegraph_references", "codegraph_patch"];
	const missing = expected.filter((n) => !cgPi.tools.has(n));
	if (missing.length) throw new Error(`missing: ${missing.join(", ")}`);
});

await check("codegraph_search finds main functions", async () => {
	const result = await cgPi.tools.get("codegraph_search").execute("t1", { query: "main", kind: "function", limit: 5 }, undefined, undefined, ctx);
	const text = firstText(result);
	if (!text.includes("fn main")) throw new Error(`unexpected output: ${text.slice(0, 200)}`);
});

await check("codegraph_outline outlines a file", async () => {
	const result = await cgPi.tools.get("codegraph_outline").execute("t2", { file: "codegraph/src/mcp.rs" }, undefined, undefined, ctx);
	if (!firstText(result).includes("tool_definitions")) throw new Error("outline missing expected symbol");
});

await check("codegraph_snippet pulls one symbol", async () => {
	const result = await cgPi.tools.get("codegraph_snippet").execute("t3", { file: "codegraph/src/mcp.rs", symbol: "tool_error" }, undefined, undefined, ctx);
	if (!firstText(result).includes("fn tool_error")) throw new Error("snippet missing function source");
});

await check("codegraph_snippet rejects symbol+lines", async () => {
	let threw = false;
	try {
		await cgPi.tools.get("codegraph_snippet").execute("t4", { file: "x.rs", symbol: "a", lines: "1:2" }, undefined, undefined, ctx);
	} catch {
		threw = true;
	}
	if (!threw) throw new Error("expected validation error");
});

await check("codegraph_patch --check dry-runs a diff", async () => {
	const snippet = await cgPi.tools.get("codegraph_snippet").execute("t5", { file: "codegraph/src/mcp.rs", lines: "107:110" }, undefined, undefined, ctx);
	const lines = firstText(snippet).split("\n");
	// --text snippet output is "path<TAB>range" header then source lines.
	const src = lines.slice(1, 5);
	const diff = [
		"--- a/codegraph/src/mcp.rs",
		"+++ b/codegraph/src/mcp.rs",
		"@@ -107,4 +107,5 @@",
		...src.map((l) => ` ${l}`),
		"+// smoke-test trailing comment",
		"",
	].join("\n");
	const result = await cgPi.tools.get("codegraph_patch").execute("t6", { file: "codegraph/src/mcp.rs", diff, check: true }, undefined, undefined, ctx);
	const text = firstText(result);
	if (!/hunks applied/.test(text)) throw new Error(`unexpected patch output: ${text}`);
});

await check("before_agent_start appends guidance", async () => {
	const handler = cgPi.handlers.get("before_agent_start")?.[0];
	if (!handler) throw new Error("no before_agent_start handler");
	const out = await handler({ type: "before_agent_start", prompt: "x", systemPrompt: "BASE", systemPromptOptions: {} }, ctx);
	if (!out?.systemPrompt?.startsWith("BASE") || !out.systemPrompt.includes("codegraph token economy")) {
		throw new Error("system prompt not augmented");
	}
});

await check("tool_call enforcement blocks read when PI_CODEGRAPH_ENFORCE=1", async () => {
	const handler = cgPi.handlers.get("tool_call")?.[0];
	if (!handler) throw new Error("no tool_call handler");
	process.env.PI_CODEGRAPH_ENFORCE = "1";
	const blocked = await handler({ type: "tool_call", toolCallId: "t", toolName: "read", input: { path: "printer/src/main.rs" } }, ctx);
	delete process.env.PI_CODEGRAPH_ENFORCE;
	if (!blocked?.block) throw new Error("read was not blocked");
	const allowed = await handler({ type: "tool_call", toolCallId: "t", toolName: "read", input: { path: "README.md" } }, ctx);
	if (allowed?.block) throw new Error("non-source read was blocked");
});

// --- computer extension -------------------------------------------------------

const computer = (await import("../extensions/computer.ts")).default;
const cpPi = makeMockPi();
await computer(cpPi);

const hasDisplay = process.platform !== "linux" || process.env.WAYLAND_DISPLAY || process.env.DISPLAY;
if (hasDisplay) {
	await check("computer registers 10 tools", () => {
		if (cpPi.tools.size !== 10) throw new Error(`got ${cpPi.tools.size}: ${[...cpPi.tools.keys()].join(", ")}`);
	});
	await check("computer_outputs returns JSON", async () => {
		const result = await cpPi.tools.get("computer_outputs").execute("t7", {}, undefined, undefined, ctx);
		JSON.parse(firstText(result));
	});
	await check("computer_screenshot returns an inline image", async () => {
		const result = await cpPi.tools.get("computer_screenshot").execute("t8", {}, undefined, undefined, ctx);
		const img = result.content.find((c) => c.type === "image");
		if (!img || !img.data || img.mimeType !== "image/png") throw new Error("no image block");
		const bytes = Buffer.from(img.data, "base64");
		if (bytes.length < 1000) throw new Error("implausibly small PNG");
	});
} else {
	console.log("skip computer tool checks (no display)");
}

console.log(failures ? `\n${failures} failure(s)` : "\nall checks passed");
process.exit(failures ? 1 : 0);
