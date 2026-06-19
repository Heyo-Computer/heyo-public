---
name: codegraph-edit
description: Use this skill whenever you need to modify source files in a repo where the codegraph tools are available. Apply changes by sending a unified diff to codegraph_patch instead of using edit/write to rewrite file contents. Triggers when the user asks to "edit a file", "modify code", "apply a patch", "make a change to <symbol>", or any time you would otherwise reach for edit/write on source code.
version: 0.1.0
---

# codegraph-edit

When this skill is active, **edit source files by emitting a unified diff to `codegraph_patch`**, not by calling the built-in `edit` or `write` tools. Patches are smaller, easier for the user to review, and the patch tool validates context lines so silent corruption fails loudly.

## The rule

- **DO** produce a unified diff and apply it with `codegraph_patch`.
- **DO NOT** use the built-in `edit` or `write` tools on tracked source files. Reserve those for new files, non-source artifacts, or when the user explicitly asks for a full rewrite.
- **DO NOT** re-read a whole file and `write` a near-identical body. That wastes tokens.

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

- Pull the current source with `codegraph_snippet` first so the context lines match exactly — whitespace and trailing characters matter.
- Keep one logical change per patch. Multiple unrelated hunks across a file are fine; multiple unrelated *changes* should be separate patches so a failure leaves the rest unaffected.

## Applying a patch

Call `codegraph_patch` with `file` and `diff`. **Dry-run first** when you're unsure the context will line up: set `check: true` — it parses and applies the diff in memory only. If it fails, fix the patch (usually stale context) and re-run before applying for real.

## When the patch fails

- Re-pull the target region with `codegraph_snippet` and rebuild the diff against the actual current bytes.
- Don't fall back to `write` to "just overwrite the file" — fix the patch.
- If the file is outside the working directory and you have permission, set `allow_outside: true`.

## What still uses edit/write

- Creating brand-new files (`codegraph_patch` modifies existing ones).
- Non-source artifacts where a diff is awkward (binary files, generated output).
- Cases where the user explicitly asks for a full-file rewrite.

For everything else: **diff first, patch second, no direct writes.**
