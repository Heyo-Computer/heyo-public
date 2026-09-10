/**
 * Scalars that survive a model's idea of a number.
 *
 * Arguments arrive as JSON a language model wrote, and a model that has been
 * told `limit` is a number still sends `"100"` a fair fraction of the time.
 * Until these schemas were parsed at all that cost nothing — every handler
 * coerced by hand, and `String(v)` on the way into a query string does not care
 * what it was given. Parsing makes it matter: a bare `z.number()` would start
 * rejecting calls that have always worked.
 *
 * So the parse is deliberately looser than the type, in one direction only: a
 * string that unambiguously denotes the value is accepted, and anything else is
 * still an error. Both helpers render as their plain type in JSON Schema
 * (`{"type":"number"}`, `{"type":"boolean"}`), so what a client is *told* is
 * unchanged and the tool listing does not grow — the leniency exists to forgive
 * a caller, not to advertise a second accepted form.
 */

import { z } from "zod";

/**
 * A number, or a string that is one.
 *
 * Not `z.coerce.number()`, which is far too eager: it turns `null`, `""` and
 * `[]` into `0` without complaint, so a caller who omitted a value wrongly —
 * `{"limit": null}` — would silently get `limit: 0` and an empty result, which
 * is worse than the error they should have had. This converts only a non-empty
 * string that parses to a finite number, and leaves everything else for
 * `z.number()` to reject by name.
 */
export const num = () =>
  z.preprocess(
    (v) =>
      typeof v === "string" && v.trim() !== "" && Number.isFinite(Number(v)) ? Number(v) : v,
    z.number(),
  );

/**
 * A boolean, or the two strings that spell one.
 *
 * `"true"` and `"false"` only — not `"1"`, `"yes"` or `"on"`. Those are guesses
 * about intent, and a tool argument is not the place to guess: `ci_run_logs`'s
 * `failed_only` decides how much of a build log comes back, and getting it
 * backwards because someone typed `"0"` is a silent wrong answer rather than a
 * loud one.
 */
export const bool = () =>
  z.preprocess(
    (v) => (v === "true" ? true : v === "false" ? false : v),
    z.boolean(),
  );

/**
 * The sentence that opens a destructive tool's description.
 *
 * One constant, because two things depend on the exact bytes: the model, which
 * reads the sentence, and `annotationsFor`, which derives `destructiveHint` by
 * testing for it. It was previously declared separately in two tool modules,
 * which is one copy away from a tool that says DESTRUCTIVE and is not flagged.
 */
export const DESTRUCTIVE_PREFIX = "DESTRUCTIVE. ";
