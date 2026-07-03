---
name: codegraph-edit
description: Use this skill whenever you need to modify source files in a repo that has the `codegraph` CLI available. Apply changes by sending a unified diff to `codegraph patch` instead of using Edit/Write to rewrite file contents. Triggers when the user asks to "edit a file", "modify code", "apply a patch", "make a change to <symbol>", or any time you would otherwise reach for Edit/Write on source code.
version: 0.1.0
---

# codegraph-edit

When this skill is active, **edit files by emitting a unified diff to `codegraph patch`**, not by calling Edit or Write on source files. Patches are smaller, easier for the user to review, and the patch tool validates context lines so silent corruption fails loudly.

## The rule

- **DO** produce a unified diff and apply it with `codegraph patch <file> --diff <patchfile>` (or via stdin).
- **DO NOT** use the `Edit` or `Write` tools on tracked source files. Reserve those for new non-source files (configs the patch tool can't reach, generated artifacts) or when the user explicitly asks for a full rewrite.
- **DO NOT** hand-edit by re-reading the whole file and calling `Write` with a near-identical body. That wastes tokens.

## Producing a patch

Use standard unified-diff format with at least 3 lines of context:

```
--- a/src/server.rs
+++ b/src/server.rs
@@ -42,7 +42,7 @@
 fn handle_request(req: Request) -> Response {
     let id = req.id();
-    log::info!("got request {}", id);
+    log::debug!("got request {}", id);
     dispatch(req)
 }
```

Tips for keeping patches reliable:

- Pull the current source with `codegraph snippet <file> <symbol>` (or `Read` a small range) so the context lines match exactly — whitespace and trailing chars matter.
- Keep one logical change per patch. Multiple unrelated hunks across a file are fine; multiple unrelated *changes* should be separate patches so a failure leaves the rest unaffected.
- Re-index after large structural edits: `codegraph index`.

## Applying a patch

Two equivalent ways:

```
# from a file
codegraph patch src/server.rs --diff /tmp/change.patch

# from stdin (preferred for ephemeral patches — no temp file)
printf '%s\n' "$DIFF" | codegraph patch src/server.rs
```

**Always dry-run first** when you're unsure the context will line up:

```
codegraph patch src/server.rs --diff /tmp/change.patch --check
```

`--check` parses and applies the diff in memory only. If it fails, fix the patch (usually stale context) and re-run before applying for real.

## Output

Default output is JSON: `{ ok, file, hunks_applied, hunks_total, bytes_written, failure }`. Pass `--text` for a one-line human summary. Exit code is non-zero on failure.

## When the patch fails

- Re-read the target region with `codegraph snippet` and rebuild the diff against the actual current bytes.
- Don't fall back to `Write` to "just overwrite the file" — that loses concurrent edits and defeats the review benefit. Fix the patch.
- If the file is truly outside the working directory and you have permission, pass `--allow-outside`.

## What still uses Edit/Write

- Creating brand-new files (codegraph patch is for modifying existing ones).
- Non-source artifacts where a diff is awkward (binary files, generated output).
- Cases where the user explicitly asks for a full-file rewrite.

For everything else: **diff first, patch second, no direct writes.**
