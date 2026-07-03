---
name: codegraph-search
description: Use this skill to navigate and search a codebase efficiently with the `codegraph` CLI instead of repeated greps and full-file reads. Triggers when the user asks to "find a symbol", "look up a definition", "list functions in a file", "show an outline", "find references", "search the codebase for X", or any time you need to locate code by name/signature/kind across a repo. Tree-sitter-backed; supports Rust, Python, JavaScript, and TypeScript.
version: 0.1.0
---

# codegraph-search

`codegraph` is a tree-sitter-backed code navigator. Prefer it over `grep` + `find` + reading whole files when you need to locate a symbol, list what's in a file, or pull a single function out of a large file. It uses far fewer tokens than reading entire files.

## When to use

- Locating a function/struct/class by name across the repo → `codegraph search` or `codegraph definition`.
- Skimming what a file contains without reading the body → `codegraph outline` or `codegraph symbols`.
- Pulling the source of one symbol out of a large file → `codegraph snippet`.
- Finding lexical references to a name → `codegraph references`.

## Workflow

1. **Index once per session** (or after substantial edits):
   ```
   codegraph index
   ```
   Writes `.codegraph/index.json` at the repo root. Re-run with `--force` to rebuild from scratch.

2. **Search by name or signature substring:**
   ```
   codegraph search foo                       # name + signature match
   codegraph search foo --name                # name only
   codegraph search "fn handle_" --kind function
   codegraph search Worker --kind struct --limit 20
   ```
   Kinds: `function`, `method`, `class`, `struct`, `enum`, `trait`, `interface`, `module`, `type`, `constant`, `variable`.

3. **Jump to a definition:**
   ```
   codegraph definition Foo::bar
   codegraph definition handle_request
   ```

4. **Outline a file** (signatures only, no bodies — cheap to read):
   ```
   codegraph outline src/server.rs
   ```

5. **Pull one symbol's source** instead of reading the whole file:
   ```
   codegraph snippet src/server.rs handle_request
   codegraph snippet src/server.rs --lines 120:180
   ```

6. **Find references:**
   ```
   codegraph references handle_request
   ```

## Output format

Default output is JSON — easy to parse, but verbose. Pass `--text` for compact tab-separated lines when you only need to read it yourself:

```
codegraph --text search foo
codegraph --text outline src/lib.rs
```

## Token-saving rules

- **Outline before reading.** If you don't already know what's in a file, run `codegraph outline <file>` first. Read the full file only if the outline isn't enough.
- **Snippet, don't Read.** When you need one function from a large file, `codegraph snippet <file> <symbol>` is almost always cheaper than `Read`.
- **Use `--text` for human-only inspection** to avoid JSON overhead.
- **Use `--limit`** on `search` to cap noisy queries.

## Caveats

- Only Rust / Python / JavaScript / TypeScript are parsed. Other languages won't appear in the index.
- `references` is a lexical word-boundary scan, not a true semantic xref — it may include comments/strings and miss dynamic dispatch.
- The index is on disk at `.codegraph/index.json`; re-run `codegraph index` after large refactors so search results stay accurate.
