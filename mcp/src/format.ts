/**
 * Turning API responses into something worth reading.
 *
 * Tool output is context, and context is finite. A raw dump of every field is
 * both expensive and harder to read than the six values that answer the
 * question, so sections are labelled and payloads are capped — with the cap
 * stated, because silently truncated output reads as complete output.
 */

const MAX_CHARS = 12_000;

export function json(value: unknown, max = MAX_CHARS): string {
  if (value === null || value === undefined) return "(no content)";
  const text = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  if (text.length <= max) return text;
  return `${text.slice(0, max)}\n… truncated at ${max} of ${text.length} characters.`;
}

export interface Section {
  title: string;
  body: unknown;
  /** Rendered instead of the body when a read failed, so a partial answer is still an answer. */
  error?: string;
}

export function report(headline: string, sections: Section[]): string {
  const parts = [headline, ""];
  for (const s of sections) {
    parts.push(`## ${s.title}`);
    parts.push(s.error ? `unavailable — ${s.error}` : json(s.body));
    parts.push("");
  }
  return parts.join("\n").trimEnd();
}

/**
 * Fold a settled read into a section, so the failure survives as text.
 *
 * `errorTitle` exists because a heading may assert an *outcome*, and an
 * outcome-asserting heading is a claim that has to be withdrawn when the probe
 * it describes fails. `heyo_status` printed "reachable, and the key is good"
 * directly above the 401 disproving it — in the tool advertised as where to
 * start when something is wrong.
 *
 * A heading that only names what was probed ("app-lb /metrics") stays true
 * either way and passes no third argument. The parameter is optional so the
 * neutral majority is unchanged, and typed so the next assertive heading has an
 * obvious place to put its retraction.
 */
export function section(
  title: string,
  r: { ok: true; value: unknown } | { ok: false; error: string },
  errorTitle?: string,
): Section {
  return r.ok
    ? { title, body: r.value }
    : { title: errorTitle ?? title, body: null, error: r.error };
}
