import { test } from "node:test";
import assert from "node:assert/strict";
import { frameStdin, parseIncoming, shellQuery, shellUrl } from "../dist/shell.js";

const text = (o) => JSON.stringify(o);

// The whole reason frameStdin exists: app-lb silently drops a binary frame that
// does not start with 0x01, with no error and no diagnosis. A client that
// forgets connects perfectly and types into the void.
test("stdin is always prefixed", () => {
  assert.deepEqual([...frameStdin(new TextEncoder().encode("ls\n"))], [0x01, 108, 115, 10]);
  assert.deepEqual([...frameStdin(new Uint8Array())], [0x01], "even an empty write is framed");
  assert.equal(frameStdin(new Uint8Array([0x02]))[0], 0x01, "the payload is not the channel");
});

test("output is unwrapped and other channels are not", () => {
  assert.deepEqual(parseIncoming(new Uint8Array([0x02, 104, 105])), {
    type: "output",
    data: new Uint8Array([104, 105]),
  });
  assert.deepEqual(parseIncoming(new Uint8Array([0x02])), {
    type: "output",
    data: new Uint8Array([]),
  }, "an empty payload is still output");
  // Our own stdin channel echoed back is not output.
  assert.equal(parseIncoming(new Uint8Array([0x01, 120])).type, "ignored");
  assert.equal(parseIncoming(new Uint8Array()).type, "ignored");
});

test("control frames are understood", () => {
  assert.deepEqual(parseIncoming(text({ type: "ready", sandbox_id: "sb-1" })), {
    type: "ready", sandboxId: "sb-1",
  });
  assert.deepEqual(parseIncoming(text({ type: "exit", code: 3 })), { type: "exit", code: 3 });
  assert.deepEqual(parseIncoming(text({ type: "error", message: "boom" })), {
    type: "error", message: "boom",
  });
});

// A newer app-lb adding a frame type must not break an older client.
test("unknown frames are ignored rather than fatal", () => {
  for (const raw of [text({ type: "something-new" }), "not json", "{}", text({})]) {
    assert.equal(parseIncoming(raw).type, "ignored", raw);
  }
});

test("missing fields degrade predictably", () => {
  assert.deepEqual(parseIncoming(text({ type: "exit" })), { type: "exit", code: 0 });
  assert.deepEqual(parseIncoming(text({ type: "ready" })), { type: "ready", sandboxId: "" });
  const e = parseIncoming(text({ type: "error" }));
  assert.equal(e.type, "error");
  assert.ok(e.message.length > 0, "an error must always say something");
});

test("the URL swaps scheme and carries the options", () => {
  assert.equal(
    shellUrl("http://127.0.0.1:9090", "sb-1", shellQuery({ cols: 120, rows: 40 })),
    "ws://127.0.0.1:9090/deployments/sb-1/shell?cols=120&rows=40&wake=true",
  );
  assert.ok(shellUrl("https://lb.example.com", "sb-1", "").startsWith("wss://"));
  // Only `true`/`false` parse server-side.
  assert.ok(shellQuery({ wake: false }).includes("wake=false"));
  assert.ok(shellQuery({ cwd: "/work space" }).includes("cwd=%2Fwork%20space"));
});

test("a deployment id is escaped into the URL", () => {
  assert.ok(shellUrl("http://x:1", "a?b=c", "q=1").startsWith("ws://x:1/deployments/a%3Fb%3Dc/shell?"));
});
