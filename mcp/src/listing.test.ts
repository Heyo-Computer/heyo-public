/**
 * What a client learns before its first call.
 *
 * `tools/list` is the entire tool surface as far as a model is concerned — every
 * name, every description, every schema — and until `toolListing` was split out
 * of `createServer` nothing could assert it. These are the invariants that hold
 * regardless of what any individual tool does, plus the one number that has to
 * be watched rather than merely asserted: how many bytes the listing costs.
 *
 * That budget is not bureaucracy. The listing is re-sent on every connect, and
 * the deployment spec alone is a ~126-field tree; inlining it into a tool schema
 * is a real and easy-to-miss increase in what every client pays. When the budget
 * fires, raise it deliberately, in its own commit, and write the new measured
 * number in — do not round it up to make room.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { z } from "zod";
import { zodToJsonSchema } from "zod-to-json-schema";

import { loadConfig } from "./config.js";
import { buildTools, toolListing } from "./server.js";
import type { Tool } from "./tools/diagnose.js";

/** The full surface: cloud configured, so the sandbox tools are listed too. */
const everything = (): Tool[] =>
  buildTools(loadConfig({ HEYO_API_KEY: "heyo_api_x", APPLB_TOKEN: "heyo_api_lb" }));

/** The fleet-operations shape: no cloud credential, so no sandbox tools. */
const fleetOnly = (): Tool[] => buildTools(loadConfig({ APPLB_URL: "http://127.0.0.1:9090" }));

/**
 * Every prefix a tool name may carry, and the two that carry none.
 *
 * The prefix names the upstream a tool speaks to, which is what makes a
 * 56-name list navigable at all. The exceptions are deliberate: a tool named
 * after the *question* it answers rather than the service it happens to hit.
 */
const PREFIXES = ["applb_", "sandbox_", "art_", "ci_", "obs_", "heyo_", "diagnose_"];
const UNPREFIXED = ["fleet_overview", "deployment_logs"];

test("every listed tool is well formed", () => {
  const listing = toolListing(everything());
  assert.ok(listing.length > 0, "the listing is empty");

  for (const t of listing) {
    assert.ok(t.description.trim().length > 0, `${t.name} has no description`);
    assert.equal(
      t.inputSchema.type,
      "object",
      `${t.name} advertises a non-object input schema, which some hosts refuse`,
    );
    assert.ok(
      PREFIXES.some((p) => t.name.startsWith(p)) || UNPREFIXED.includes(t.name),
      `${t.name} carries no known service prefix — add one, or add it to UNPREFIXED deliberately`,
    );
  }
});

test("tool names are unique", () => {
  const names = toolListing(everything()).map((t) => t.name);
  const seen = new Set<string>();
  const duplicated = names.filter((n) => (seen.has(n) ? true : (seen.add(n), false)));
  assert.deepEqual(duplicated, [], "a duplicate name shadows a tool in the byName map");
});

test("the sandbox tools are gated on a cloud credential", () => {
  const withCloud = toolListing(everything()).map((t) => t.name);
  const without = toolListing(fleetOnly()).map((t) => t.name);

  // Asserted rather than named in a comment. The doc comment on `buildTools`
  // used to say "sixteen" when the real number was fifteen.
  const gated = withCloud.filter((n) => !without.includes(n));
  assert.equal(gated.length, 16, `expected 16 gated tools, got ${gated.length}: ${gated.join(", ")}`);
  assert.ok(gated.includes("sandbox_create"));
  assert.ok(
    gated.includes("heyo_capacity"),
    "heyo_capacity hits cloud despite its name and must be gated with the sandbox tools",
  );
  assert.ok(
    gated.includes("heyo_request"),
    "heyo_request targets cloud and must be gated with the tools that share its credential",
  );
  assert.ok(without.includes("applb_list_deployments"), "app-lb tools survive without cloud");
});

// The destructive set moved to `annotations.test.ts`, which names it rather than
// counting it and checks the prose against the derived hint. A count here would
// be a second thing to bump and no stronger.

test("a tool that advertises its own schema still agrees with the one that validates", () => {
  // Two descriptions of one input, at different resolutions: `inputSchema` is
  // generated from app-lb's types and teaches; `schema` is deliberately
  // permissive and admits. They are allowed to differ in depth — that is the
  // whole point — but not about what the arguments ARE. A tool advertising
  // `spec` while validating `body` would reject every call a client made
  // correctly from the schema it was shown.
  for (const t of everything()) {
    if (!t.inputSchema) continue;
    const advertised = t.inputSchema as { properties?: object; required?: string[] };
    const validating = zodToJsonSchema(z.object(t.schema), { $refStrategy: "none" }) as {
      properties?: object;
      required?: string[];
    };
    assert.deepEqual(
      Object.keys(advertised.properties ?? {}).sort(),
      Object.keys(validating.properties ?? {}).sort(),
      `${t.name} advertises different arguments than it validates`,
    );
    assert.deepEqual(
      [...(advertised.required ?? [])].sort(),
      [...(validating.required ?? [])].sort(),
      `${t.name} disagrees with itself about which arguments are required`,
    );
  }
});

test("the listing stays within its size budget", () => {
  // Measured 2026-09-10: 62,691 bytes across 64 tools, up from 35,287 across 56.
  //
  // Two deliberate increases, in order. The first +13,917 bought the thing the field report ranked highest: the
  // deployment spec is now *on* `applb_create_deployment` — generated from
  // app-lb's Rust types, pruned to a first-paragraph summary with the rarely
  // hand-written blocks collapsed — instead of being an untyped blob a caller
  // had to read our source to fill in. `applb_spec_schema` carries the rest at
  // no listing cost.
  //
  // The second +9,840 is seven new tools and one more generated block: the two
  // job tools that did not exist (`applb_pull`, `applb_pull_mounts`), the edit
  // path that preserves the pool (`applb_update_deployment`), job polling by id,
  // the drain pair, `applb_deploy` (the composite the spec schema now lives on),
  // and `applb_scale`'s body typed from the generated `ScalingPolicy` instead of
  // being an untyped blob. The spec tree moved from `applb_create_deployment`
  // onto `applb_deploy` rather than being copied — `spec.test.ts` asserts that
  // exactly one tool carries it.
  //
  // The last +3,647 is annotations and one rewritten description. The
  // annotations are 1,489 bytes of that, because they emit only what the MCP
  // defaults do not already say — the long form would have been 6,410.
  //
  // The headroom below is for ordinary description edits. It is deliberately
  // NOT enough for a second full spec schema: a tool that wants one shares this
  // one by pointing at it, because two copies of a 12 KB tree is a cost every
  // client pays on every connect.
  const BUDGET = 66_000;
  const bytes = JSON.stringify(toolListing(everything())).length;
  assert.ok(
    bytes <= BUDGET,
    `tools/list is ${bytes} bytes, over the ${BUDGET} budget. ` +
      `Raise it in its own commit with the measured number, or cut what grew.`,
  );
});
