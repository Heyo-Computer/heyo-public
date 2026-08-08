/**
 * An interactive PTY in a sandbox, over a WebSocket.
 *
 * # The protocol
 *
 * app-lb terminates the daemon's own shell protocol and re-presents a much
 * smaller one — no sequence numbers, no acks, no `init` frame, no session id:
 *
 * ```text
 * client → server  binary  [0x01, ...stdin]
 * client → server  text    {"type":"resize","cols":N,"rows":N}
 * server → client  text    {"type":"ready","sandbox_id":"…"}   (once, first)
 * server → client  binary  [0x02, ...stdout]                   (PTY merges stderr)
 * server → client  text    {"type":"exit","code":N}
 * server → client  text    {"type":"error","message":"…"}      (non-terminal)
 * ```
 *
 * {@link Shell} owns that encoding so no caller meets `0x01`. **That prefix is
 * mandatory** — app-lb silently drops a binary frame starting with anything
 * else, no error and no diagnosis, which is the easiest way to write a shell
 * client that connects perfectly and types nothing.
 *
 * # Three things that are not smoothed over
 *
 * **`exit` code 0 is ambiguous.** It means both "exited cleanly" and "the VM
 * died": on reconnect exhaustion app-lb forwards an `error` and then closes with
 * `exit: 0`. So {@link ShellExit} carries any error that preceded it and
 * `clean` is false when one did.
 *
 * **There is no resume.** A dropped socket means the session is gone;
 * reconnecting gives a *new* shell. This package will not silently retry.
 *
 * **Nothing pings but us.** app-lb never originates one, so an idle socket is
 * free for any intermediary to reap. Node pings every {@link PING_INTERVAL_MS};
 * browsers cannot send pings, so the runtime's own keepalive applies.
 */

import { ShellError, fromResponse } from "./errors.js";
import type { Serverctl } from "./client.js";

const STDIN = 0x01;
const STDOUT = 0x02;

export const PING_INTERVAL_MS = 25_000;

export interface ShellOptions {
  cols?: number;
  rows?: number;
  cwd?: string;
  /** Boot or resume a VM if none is running. Default true. */
  wake?: boolean;
  signal?: AbortSignal;
}

export interface ShellExit {
  /**
   * The guest's exit code. **`0` alone does not mean success** — app-lb reports
   * an *unknown* code as 0, which is what a VM dying under a live session looks
   * like. Check {@link clean}.
   */
  code: number;
  /** The last error before the session ended, if any. */
  error?: string;
  /** Exited zero *and* nothing went wrong on the way. */
  clean: boolean;
}

/** A minimal structural type, so this works with `ws` and with the global. */
interface SocketLike {
  send(data: string | Uint8Array): void;
  close(code?: number, reason?: string): void;
  addEventListener?(type: string, listener: (ev: any) => void): void;
  on?(type: string, listener: (...args: any[]) => void): void;
  ping?(): void;
}

/** Build the WebSocket URL, swapping the scheme. */
export function shellUrl(server: string, id: string, query: string): string {
  const swapped = server.startsWith("https://")
    ? `wss://${server.slice(8)}`
    : server.startsWith("http://")
      ? `ws://${server.slice(7)}`
      : `ws://${server}`;
  return `${swapped}/deployments/${encodeURIComponent(id)}/shell?${query}`;
}

/** The query string for a set of options. */
export function shellQuery(opts: ShellOptions): string {
  const parts = [
    `cols=${opts.cols ?? 80}`,
    `rows=${opts.rows ?? 24}`,
    // Only `true`/`false` parse server-side.
    `wake=${(opts.wake ?? true) ? "true" : "false"}`,
  ];
  if (opts.cwd) parts.push(`cwd=${encodeURIComponent(opts.cwd)}`);
  return parts.join("&");
}

/** Frame stdin. The `0x01` is not optional. */
export function frameStdin(data: Uint8Array): Uint8Array {
  const out = new Uint8Array(data.length + 1);
  out[0] = STDIN;
  out.set(data, 1);
  return out;
}

/** What one incoming frame meant. Exported so it can be tested without a socket. */
export type Incoming =
  | { type: "ready"; sandboxId: string }
  | { type: "output"; data: Uint8Array }
  | { type: "error"; message: string }
  | { type: "exit"; code: number }
  | { type: "ignored" };

export function parseIncoming(raw: string | Uint8Array): Incoming {
  if (typeof raw !== "string") {
    // app-lb only ever sends 0x02; anything else is a newer channel or a
    // corrupted frame, and either way not output.
    if (raw.length === 0 || raw[0] !== STDOUT) return { type: "ignored" };
    return { type: "output", data: raw.subarray(1) };
  }
  let v: any;
  try {
    v = JSON.parse(raw);
  } catch {
    return { type: "ignored" };
  }
  switch (v?.type) {
    case "ready":
      return { type: "ready", sandboxId: typeof v.sandbox_id === "string" ? v.sandbox_id : "" };
    case "exit":
      return { type: "exit", code: typeof v.code === "number" ? v.code : 0 };
    case "error":
      return {
        type: "error",
        message:
          typeof v.message === "string" && v.message
            ? v.message
            : "the server reported an error with no message",
      };
    default:
      // A newer app-lb adding a frame type must not break an older client.
      return { type: "ignored" };
  }
}

/** Whether this runtime can set headers on a WebSocket upgrade. */
export function canSetWsHeaders(): boolean {
  // Browsers cannot, and that is the entire reason `?app_token=` exists.
  return typeof (globalThis as any).window === "undefined";
}

/** A live session. */
export class Shell {
  private readonly socket: SocketLike;
  private readonly listeners: ((data: Uint8Array) => void)[] = [];
  private readonly errorListeners: ((message: string) => void)[] = [];
  private lastError?: string;
  private settled = false;
  private resolveExit!: (e: ShellExit) => void;
  private pingTimer?: ReturnType<typeof setInterval>;

  /** Resolves when the session ends. */
  readonly exit: Promise<ShellExit>;
  readonly sandboxId: string;

  private constructor(socket: SocketLike, sandboxId: string) {
    this.socket = socket;
    this.sandboxId = sandboxId;
    this.exit = new Promise((resolve) => {
      this.resolveExit = resolve;
    });
  }

  /** @internal */
  static async open(client: Serverctl, id: string, opts: ShellOptions): Promise<Shell> {
    const header = client.authHeader();
    let query = shellQuery(opts);

    // A browser's WebSocket constructor cannot set headers, so a token in the
    // query string is the only credential it can carry. Header everywhere it is
    // possible — a credential in a URL lands in access logs, proxy logs and
    // browser history.
    const useQueryToken = !canSetWsHeaders() && client.auth.kind === "token";
    if (useQueryToken) {
      query += `&app_token=${encodeURIComponent((client.auth as { token: string }).token)}`;
    } else if (!canSetWsHeaders() && client.auth.kind === "basic") {
      throw new ShellError(
        "a browser cannot send Basic credentials on a WebSocket upgrade — use an " +
          "app-token instead, ideally a short-lived one, since it travels in the URL",
      );
    }

    const url = shellUrl(client.server, id, query);
    const socket = await connect(url, useQueryToken ? undefined : header, id, client);

    // The first frame is always `ready`; read it here so `sandboxId` is known
    // before the caller sees a byte of output.
    const sandboxId = await new Promise<string>((resolve, reject) => {
      const onMessage = (data: string | Uint8Array) => {
        const msg = parseIncoming(data);
        if (msg.type === "ready") {
          off();
          resolve(msg.sandboxId);
        } else if (msg.type === "error") {
          off();
          reject(new ShellError(msg.message));
        } else if (msg.type === "exit") {
          off();
          reject(new ShellError("the shell closed before it was ready"));
        }
      };
      const onClose = () => {
        off();
        reject(new ShellError("the shell closed before it was ready"));
      };
      const off = listen(socket, onMessage, onClose);
    });

    const shell = new Shell(socket, sandboxId);
    shell.start();
    if (opts.signal) {
      opts.signal.addEventListener("abort", () => void shell.close(), { once: true });
    }
    return shell;
  }

  private start(): void {
    listen(
      this.socket,
      (data) => {
        const msg = parseIncoming(data);
        if (msg.type === "output") {
          for (const l of this.listeners) l(msg.data);
        } else if (msg.type === "error") {
          // Non-terminal, but latched: this is what makes a later `exit: 0`
          // legible as a failure.
          this.lastError = msg.message;
          for (const l of this.errorListeners) l(msg.message);
        } else if (msg.type === "exit") {
          this.finish(msg.code);
        }
      },
      () => this.finish(0),
    );

    // Browsers have no way to send a ping frame; `ws` does.
    if (typeof this.socket.ping === "function") {
      this.pingTimer = setInterval(() => {
        try {
          this.socket.ping!();
        } catch {
          // The close handler will settle the session.
        }
      }, PING_INTERVAL_MS);
      // Do not hold a Node process open just to keep pinging.
      (this.pingTimer as any).unref?.();
    }
  }

  private finish(code: number): void {
    if (this.settled) return;
    this.settled = true;
    if (this.pingTimer) clearInterval(this.pingTimer);
    this.resolveExit({
      code,
      error: this.lastError,
      clean: code === 0 && this.lastError === undefined,
    });
  }

  /** Called for every chunk the PTY produces. stderr is already merged in. */
  onData(fn: (data: Uint8Array) => void): void {
    this.listeners.push(fn);
  }

  /** Called for a non-fatal error. The session continues. */
  onError(fn: (message: string) => void): void {
    this.errorListeners.push(fn);
  }

  /** Send stdin. */
  write(data: Uint8Array | string): void {
    const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
    this.socket.send(frameStdin(bytes));
  }

  /**
   * Tell the guest its terminal changed size.
   *
   * Worth wiring to `process.stdout.on("resize")` — app-lb has always supported
   * this and no client used it, so resizing a terminal mid-session left the
   * guest PTY at whatever geometry it started with.
   */
  resize(cols: number, rows: number): void {
    this.socket.send(JSON.stringify({ type: "resize", cols, rows }));
  }

  /** Close the session and wait for it to settle. */
  async close(): Promise<ShellExit> {
    try {
      this.socket.close();
    } catch {
      // Already gone.
    }
    this.finish(0);
    return this.exit;
  }
}

/** Attach a message/close listener to either socket flavour. */
function listen(
  socket: SocketLike,
  onMessage: (data: string | Uint8Array) => void,
  onClose: () => void,
): () => void {
  const handleMessage = (payload: unknown) => {
    if (typeof payload === "string") return onMessage(payload);
    if (payload instanceof Uint8Array) return onMessage(payload);
    if (payload instanceof ArrayBuffer) return onMessage(new Uint8Array(payload));
    // Browser `Blob` — only reachable with binaryType left at its default,
    // which `connect` sets to arraybuffer, so this is belt and braces.
    if (typeof (payload as any)?.arrayBuffer === "function") {
      void (payload as Blob).arrayBuffer().then((b) => onMessage(new Uint8Array(b)));
    }
  };

  if (typeof socket.on === "function") {
    const m = (data: unknown) => handleMessage(data);
    const c = () => onClose();
    socket.on("message", m);
    socket.on("close", c);
    return () => {
      (socket as any).off?.("message", m);
      (socket as any).off?.("close", c);
    };
  }
  const m = (ev: MessageEvent) => handleMessage(ev.data);
  const c = () => onClose();
  socket.addEventListener!("message", m);
  socket.addEventListener!("close", c);
  return () => {
    (socket as any).removeEventListener?.("message", m);
    (socket as any).removeEventListener?.("close", c);
  };
}

/**
 * Open the socket, turning a pre-upgrade rejection into the same typed error the
 * HTTP routes produce.
 *
 * Everything that can fail with a status does so *before* the upgrade — auth,
 * scope, the deployment, VM availability — so callers need one error vocabulary
 * rather than two.
 */
async function connect(
  url: string,
  header: string | undefined,
  id: string,
  client: Serverctl,
): Promise<SocketLike> {
  let socket: SocketLike;

  if (canSetWsHeaders()) {
    // Node, Bun, Deno: `ws` can set headers, and the global cannot.
    const mod = await import("ws").catch(() => {
      throw new ShellError(
        "shell sessions on Node need the `ws` package — add it as a dependency",
      );
    });
    const WS = (mod as any).default ?? (mod as any).WebSocket;
    socket = new WS(url, header ? { headers: { authorization: header } } : undefined);
  } else {
    const WS = (globalThis as any).WebSocket;
    if (!WS) throw new ShellError("this runtime has no WebSocket");
    socket = new WS(url);
    (socket as any).binaryType = "arraybuffer";
  }

  await new Promise<void>((resolve, reject) => {
    const ok = () => resolve();
    const bad = async (ev: any) => {
      // `ws` reports the rejected handshake; a browser is told nothing beyond
      // "it failed", by design, so all we can do is say the upgrade was refused
      // and suggest the usual cause.
      const status: number | undefined = ev?.status ?? ev?.target?._req?.res?.statusCode;
      if (typeof status === "number") {
        reject(fromResponse(status, "", "deployment", id, client.credential()));
      } else {
        reject(
          new ShellError(
            `the shell upgrade to ${url.split("?")[0]} was refused — check the ` +
              "credential and that the deployment has a VM",
          ),
        );
      }
    };
    if (typeof socket.on === "function") {
      socket.on("open", ok);
      socket.on("unexpected-response", (_req: unknown, res: { statusCode: number }) =>
        reject(fromResponse(res.statusCode, "", "deployment", id, client.credential())),
      );
      socket.on("error", bad);
    } else {
      socket.addEventListener!("open", ok);
      socket.addEventListener!("error", bad);
    }
  });

  return socket;
}
