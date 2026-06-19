/**
 * computer extension for pi.
 *
 * Registers the `computer` CLI (Wayland Linux + macOS desktop automation) as
 * first-class tools: screenshot, outputs, windows, mouse, keyboard, type,
 * browse. Mirrors the tool surface of `computer mcp`, including returning
 * screenshots as inline image content (downscaled to a 1568px long edge by
 * default to bound the payload).
 *
 * Registration is skipped when the binary is missing or no display is
 * present (e.g. headless sandbox VMs).
 */
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { resizeImage } from "@earendil-works/pi-coding-agent";
import { StringEnum } from "@earendil-works/pi-ai";
import { Type } from "typebox";

const BIN = "computer";
const EXEC_TIMEOUT_MS = 30_000;
const DEFAULT_MAX_EDGE = 1568;

const MOUSE_BUTTONS = ["left", "right", "middle", "side", "extra"] as const;

export default async function (pi: ExtensionAPI) {
	const probe = await pi.exec(BIN, ["--version"], { timeout: 5_000 }).catch(() => undefined);
	const available = probe !== undefined && probe.code === 0;
	const headless =
		process.platform === "linux" && !process.env.WAYLAND_DISPLAY && !process.env.DISPLAY;

	if (!available || headless) {
		if (available && headless) {
			pi.on("session_start", (_event, ctx) => {
				ctx.ui.notify("computer: no display detected — desktop tools disabled for this session.", "info");
			});
		}
		return;
	}

	/** Run a computer subcommand; throw on failure so isError is set. */
	async function run(args: string[], signal?: AbortSignal) {
		const result = await pi.exec(BIN, args, { signal, timeout: EXEC_TIMEOUT_MS });
		if (result.killed) {
			throw new Error(`computer ${args[0]} timed out or was aborted`);
		}
		if (result.code !== 0) {
			throw new Error(result.stderr.trim() || `computer ${args[0]} failed (exit ${result.code})`);
		}
		return result.stdout.trim();
	}

	const ack = (text: string) => ({ content: [{ type: "text" as const, text }], details: undefined });

	pi.registerTool({
		name: "computer_screenshot",
		label: "computer screenshot",
		description:
			"Capture a monitor to a PNG image (returned inline). Downscaled to a max long edge by default to keep the payload small.",
		promptSnippet: "computer_screenshot: capture the desktop as an inline image",
		promptGuidelines: [
			"Use computer_screenshot to verify desktop/UI state by visual evidence after sending input.",
		],
		parameters: Type.Object({
			output: Type.Optional(Type.String({ description: "Monitor name (see computer_outputs). Defaults to the first output." })),
			max_width: Type.Optional(Type.Integer({ description: `Cap the long edge to this many pixels (aspect preserved). Defaults to ${DEFAULT_MAX_EDGE}. Pass a large value for full resolution.` })),
		}),
		async execute(_toolCallId, params, signal) {
			const dir = fs.mkdtempSync(path.join(os.tmpdir(), "computer-shot-"));
			const file = path.join(dir, "screenshot.png");
			try {
				const args = ["screenshot", "-o", file];
				if (params.output) args.push("--output", params.output);
				await run(args, signal);
				const png = fs.readFileSync(file);
				const maxEdge = params.max_width ?? DEFAULT_MAX_EDGE;
				const resized = await resizeImage(new Uint8Array(png), "image/png", {
					maxWidth: maxEdge,
					maxHeight: maxEdge,
				});
				const data = resized?.data ?? png.toString("base64");
				const mimeType = resized?.mimeType ?? "image/png";
				const note = resized?.wasResized
					? `Screenshot captured (${resized.originalWidth}x${resized.originalHeight}, downscaled to ${resized.width}x${resized.height} — scale coordinates accordingly).`
					: "Screenshot captured at native resolution.";
				return {
					content: [
						{ type: "image" as const, data, mimeType },
						{ type: "text" as const, text: note },
					],
					details: undefined,
				};
			} finally {
				fs.rmSync(dir, { recursive: true, force: true });
			}
		},
	});

	pi.registerTool({
		name: "computer_outputs",
		label: "computer outputs",
		description: "List connected monitors/displays with geometry and scale.",
		promptSnippet: "computer_outputs: list monitors/displays",
		parameters: Type.Object({}),
		async execute(_toolCallId, _params, signal) {
			return ack(await run(["outputs", "--json"], signal));
		},
	});

	pi.registerTool({
		name: "computer_windows",
		label: "computer windows",
		description: "List visible top-level windows (identifier, title, app id).",
		promptSnippet: "computer_windows: list visible windows",
		parameters: Type.Object({}),
		async execute(_toolCallId, _params, signal) {
			return ack(await run(["windows", "--json"], signal));
		},
	});

	pi.registerTool({
		name: "computer_mouse_move",
		label: "computer mouse move",
		description:
			"Move the pointer to an absolute position. On Linux these are pixels on the chosen output; on macOS, points in the global display space.",
		promptSnippet: "computer_mouse_move: move the pointer to absolute coordinates",
		parameters: Type.Object({
			x: Type.Integer(),
			y: Type.Integer(),
			output: Type.Optional(Type.String({ description: "Monitor name; defaults to the global bounding box." })),
		}),
		async execute(_toolCallId, params, signal) {
			const args = ["mouse", "move", String(params.x), String(params.y)];
			if (params.output) args.push("--output", params.output);
			await run(args, signal);
			return ack(`Pointer moved to (${params.x}, ${params.y}).`);
		},
	});

	pi.registerTool({
		name: "computer_mouse_click",
		label: "computer mouse click",
		description:
			"Click a mouse button, optionally moving to (x,y) first. button: left|right|middle|side|extra. count: clicks (2 = double-click).",
		promptSnippet: "computer_mouse_click: click (optionally moving to x,y first)",
		parameters: Type.Object({
			button: Type.Optional(StringEnum(MOUSE_BUTTONS, { description: "left (default), right, middle, side, extra." })),
			count: Type.Optional(Type.Integer({ description: "Number of clicks. Default 1." })),
			x: Type.Optional(Type.Integer({ description: "Optional: move here before clicking." })),
			y: Type.Optional(Type.Integer({ description: "Optional: move here before clicking." })),
			output: Type.Optional(Type.String()),
		}),
		async execute(_toolCallId, params, signal) {
			if ((params.x === undefined) !== (params.y === undefined)) {
				throw new Error("Pass both x and y, or neither.");
			}
			if (params.x !== undefined && params.y !== undefined) {
				const move = ["mouse", "move", String(params.x), String(params.y)];
				if (params.output) move.push("--output", params.output);
				await run(move, signal);
			}
			const click = ["mouse", "click"];
			if (params.button) click.push("--button", params.button);
			if (params.count !== undefined) click.push("--count", String(params.count));
			await run(click, signal);
			return ack(
				`Clicked ${params.button ?? "left"}${params.count && params.count > 1 ? ` x${params.count}` : ""}${
					params.x !== undefined ? ` at (${params.x}, ${params.y})` : ""
				}.`,
			);
		},
	});

	pi.registerTool({
		name: "computer_mouse_scroll",
		label: "computer mouse scroll",
		description: "Scroll by (dx, dy) ticks. Positive dy scrolls down; positive dx scrolls right.",
		promptSnippet: "computer_mouse_scroll: scroll by ticks",
		parameters: Type.Object({
			dx: Type.Integer(),
			dy: Type.Integer(),
		}),
		async execute(_toolCallId, params, signal) {
			await run(["mouse", "scroll", String(params.dx), String(params.dy)], signal);
			return ack(`Scrolled (${params.dx}, ${params.dy}).`);
		},
	});

	pi.registerTool({
		name: "computer_mouse_drag",
		label: "computer mouse drag",
		description:
			"Press a button at (from_x,from_y), drag to (to_x,to_y), and release — one gesture (text selection, sliders, moving windows).",
		promptSnippet: "computer_mouse_drag: drag from one point to another",
		parameters: Type.Object({
			from_x: Type.Integer(),
			from_y: Type.Integer(),
			to_x: Type.Integer(),
			to_y: Type.Integer(),
			button: Type.Optional(StringEnum(MOUSE_BUTTONS, { description: "left (default), right, middle, side, extra." })),
			output: Type.Optional(Type.String()),
		}),
		async execute(_toolCallId, params, signal) {
			const button = params.button ?? "left";
			const out = params.output ? ["--output", params.output] : [];
			await run(["mouse", "move", String(params.from_x), String(params.from_y), ...out], signal);
			await run(["mouse", "down", "--button", button], signal);
			await run(["sleep", "50"], signal);
			await run(["mouse", "move", String(params.to_x), String(params.to_y), ...out], signal);
			await run(["sleep", "50"], signal);
			await run(["mouse", "up", "--button", button], signal);
			return ack(`Dragged ${button} from (${params.from_x}, ${params.from_y}) to (${params.to_x}, ${params.to_y}).`);
		},
	});

	pi.registerTool({
		name: "computer_key",
		label: "computer key",
		description:
			'Tap a key, or a chord. A value containing `+` (e.g. "ctrl+shift+t") is sent as a chord; otherwise it\'s a single key tap (e.g. "Return", "Escape", "a").',
		promptSnippet: "computer_key: tap a key or send a chord",
		parameters: Type.Object({
			keys: Type.String({ description: 'Key name or chord like "ctrl+c".' }),
		}),
		async execute(_toolCallId, params, signal) {
			const mode = params.keys.includes("+") ? "chord" : "tap";
			await run(["key", mode, params.keys], signal);
			return ack(`Sent ${mode === "chord" ? "chord" : "key"} ${params.keys}.`);
		},
	});

	pi.registerTool({
		name: "computer_type",
		label: "computer type",
		description: "Type a literal string of text (US keyboard layout on Linux).",
		promptSnippet: "computer_type: type literal text into the focused app",
		parameters: Type.Object({
			text: Type.String(),
			delay_ms: Type.Optional(Type.Integer({ description: "Inter-keystroke delay in ms. Default 8." })),
		}),
		async execute(_toolCallId, params, signal) {
			const args = ["type"];
			if (params.delay_ms !== undefined) args.push("--delay-ms", String(params.delay_ms));
			args.push("--", params.text);
			await run(args, signal);
			return ack(`Typed ${params.text.length} characters.`);
		},
	});

	pi.registerTool({
		name: "computer_browse",
		label: "computer browse",
		description: "Open a URL in the default web browser (fire-and-forget).",
		promptSnippet: "computer_browse: open a URL in the default browser",
		parameters: Type.Object({
			url: Type.String({ description: "http://, https://, or file:// URL." }),
		}),
		async execute(_toolCallId, params, signal) {
			await run(["browse", params.url], signal);
			return ack(`Opened ${params.url}.`);
		},
	});
}
