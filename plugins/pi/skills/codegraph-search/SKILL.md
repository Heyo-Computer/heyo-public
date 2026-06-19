---
name: codegraph-search
description: Use this skill to navigate and search a codebase efficiently with the codegraph tools instead of repeated greps and full-file reads. Triggers when the user asks to "find a symbol", "look up a definition", "list functions in a file", "show an outline", "find references", "search the codebase for X", or any time you need to locate code by name/signature/kind across a repo. Tree-sitter-backed; supports Rust, Python, JavaScript, and TypeScript.
version: 0.1.0
---

# codegraph-search

The `codegraph_*` tools are a tree-sitter-backed code navigator. Prefer them over `grep` + `find` + reading whole files when you need to locate a symbol, list what's in a file, or pull a single function out of a large file. They use far fewer tokens than reading entire files.

## When to use

- Locating a function/struct/class by name across the repo → `codegraph_search` or `codegraph_definition`.
- Skimming what a file contains without reading the body → `codegraph_outline`.
- Pulling the source of one symbol out of a large file → `codegraph_snippet`.
- Finding lexical references to a name → `codegraph_references`.

## Workflow

1. **Search by name or signature substring:**
   - `codegraph_search` with `query: "foo"` matches names and signatures.
   - Set `name_only: true` to match the qualified name only.
   - Filter with `kind`: `function`, `method`, `class`, `struct`, `enum`, `trait`, `interface`, `module`, `type`, `constant`, `variable`.
   - Cap noisy queries with `limit`.

2. **Jump to a definition:** `codegraph_definition` with `symbol: "Foo::bar"` (qualified) or `symbol: "handle_request"` (bare).

3. **Outline a file** (signatures only, no bodies — cheap to read): `codegraph_outline` with `file: "src/server.rs"`.

4. **Pull one symbol's source** instead of reading the whole file: `codegraph_snippet` with `file` and either `symbol: "handle_request"` or `lines: "120:180"` (never both).

5. **Find references:** `codegraph_references` with `symbol: "handle_request"`.

## Token-saving rules

- **Outline before reading.** If you don't already know what's in a file, run `codegraph_outline` first. Read the full file only if the outline isn't enough.
- **Snippet, don't read.** When you need one function from a large file, `codegraph_snippet` is almost always cheaper than the built-in `read`.
- **Use `limit`** on `codegraph_search` to cap noisy queries.

## Caveats

- Only Rust / Python / JavaScript / TypeScript are parsed. Other languages won't appear in the index — fall back to the built-in tools for those.
- `codegraph_references` is a lexical word-boundary scan, not a true semantic xref — it may include comments/strings and miss dynamic dispatch.
- The index lives at `.codegraph/index.json`; a watch daemon launched at session start keeps it fresh as files change.
