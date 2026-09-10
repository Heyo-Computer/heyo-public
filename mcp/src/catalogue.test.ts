/**
 * The README's tool catalogue is current.
 *
 * It was hand-maintained until 2026-09-10 and had drifted exactly the way every
 * other hand-maintained mirror in this repository drifted: the largest group —
 * app-lb's whole lifecycle surface — was a single sentence instead of a table,
 * and the destructive list named eight tools when there were eleven. That last
 * one was a contradiction with `annotations.test.ts`, which already asserted the
 * set by name; nothing compared the two.
 *
 * So the catalogue is generated from `toolListing` — the same function that
 * answers `tools/list` — and this test fails when the checked-in README is
 * behind it. Adding a tool without running `npm run catalogue` is a failing
 * test rather than a stale document.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// @ts-expect-error - a plain .mjs build script, deliberately not compiled
import { BEGIN, END, catalogueBlock, spliceCatalogue } from "../scripts/gen-catalogue.mjs";

import { loadConfig } from "./config.js";
import { buildTools, toolListing } from "./server.js";
import { DESTRUCTIVE_PREFIX } from "./tools/schema.js";

// dist/ at runtime, so the package root is one level up.
const readmePath = join(dirname(fileURLToPath(import.meta.url)), "..", "README.md");
const readme = () => readFileSync(readmePath, "utf8");

test("the checked-in catalogue matches the server's own listing", () => {
  const current = readme();
  const regenerated = spliceCatalogue(current, catalogueBlock() as string);
  assert.equal(
    current,
    regenerated,
    "README.md's tool catalogue is stale — run `npm run build && npm run catalogue` " +
      "and commit the result.",
  );
});

test("every tool appears in the catalogue exactly once", () => {
  const text = readme();
  const block = text.slice(text.indexOf(BEGIN as string), text.indexOf(END as string));
  const tools = buildTools(
    loadConfig({
      HEYO_API_KEY: "heyo_api_x",
      APPLB_TOKEN: "heyo_api_lb",
      APP_OBS_URL: "http://o",
      CI_URL: "http://c",
      ART_URL: "http://a",
    }),
  );

  for (const t of tools) {
    const rows = block.split("\n").filter((l) => l.startsWith(`| \`${t.name}\` |`));
    assert.equal(rows.length, 1, `${t.name} appears ${rows.length} times in the catalogue`);
  }
});

test("the catalogue marks exactly the tools that call themselves destructive", () => {
  // The contradiction that used to exist: prose in one place, a different set in
  // another, and nothing comparing them. Here they are compared.
  const text = readme();
  const block = text.slice(text.indexOf(BEGIN as string), text.indexOf(END as string));
  const marked = new Set(
    block
      .split("\n")
      .filter((l) => l.includes("**destructive**"))
      .map((l) => /^\| `([^`]+)`/.exec(l)?.[1])
      .filter((n): n is string => !!n),
  );

  const listed = toolListing(
    buildTools(loadConfig({ HEYO_API_KEY: "heyo_api_x", APPLB_TOKEN: "heyo_api_lb" })),
  );
  const destructive = new Set(
    listed.filter((t) => t.description.startsWith(DESTRUCTIVE_PREFIX)).map((t) => t.name),
  );

  assert.deepEqual([...marked].sort(), [...destructive].sort());
});
