# Codegraph CLI

Tree-sitter-backed code navigation and patching for agents. Cheaper than
grep + full-file reads when locating a symbol, skimming a file, pulling one
function out of a large file, or applying a focused edit.

Supported languages: Rust, Python, JavaScript, TypeScript.

## Example usage

### Index the repo (do this first)

```sh
codegraph index                          # build/update .codegraph/index.json at the repo root
codegraph index --force                  # rebuild from scratch
codegraph index path/to/other/repo       # index a different root
```

Re-run after substantial edits so search results stay accurate.

### Keep the index live with `watch`

```sh
codegraph watch                          # foreground daemon: refresh on every change
codegraph watch --debounce-ms 500        # coalesce bursts before re-indexing
codegraph watch path/to/other/repo
```

`watch` does an initial indexing pass, then subscribes to filesystem events
and re-indexes affected files (or removes deleted ones) as they change. It
honors the same exclusion list as `index` (`.git`, `target`, `node_modules`,
`dist`, `build`, `.next`, `.venv`, `__pycache__`, `.cache`, …) plus the
repo's `.gitignore`. Run it under your supervisor of choice (`tmux`,
`systemd --user`, `&`) to give agents an always-fresh `.codegraph/index.json`.

### Search the index

```sh
codegraph search foo                              # name + signature match
codegraph search foo --name                       # name only
codegraph search "fn handle_" --kind function     # filter by kind
codegraph search Worker --kind struct --limit 20  # cap noisy queries
```

Kinds: `function`, `method`, `class`, `struct`, `enum`, `trait`, `interface`, `module`, `type`, `constant`, `variable`.

### Jump to a definition

```sh
codegraph definition handle_request
codegraph definition Foo::bar             # qualified
```

### Outline a file (signatures only — no bodies)

```sh
codegraph outline src/server.rs
codegraph outline --text src/server.rs    # compact tab-separated output
```

### Pull one symbol's source

Far cheaper than reading the whole file:

```sh
codegraph snippet src/server.rs handle_request    # by symbol
codegraph snippet src/server.rs --lines 120:180   # by line range
```

### Find references

```sh
codegraph references handle_request
```

(Lexical word-boundary scan — may include comments/strings and miss dynamic dispatch.)

### List symbols in a file

```sh
codegraph symbols src/lib.rs
codegraph --text symbols src/lib.rs       # tab-separated for shell pipelines
```

### Apply a patch

Build a unified diff with at least 3 lines of context, then:

```sh
codegraph patch src/server.rs --diff /tmp/change.patch
codegraph patch src/server.rs --diff /tmp/change.patch --check  # dry-run, in memory only
printf '%s\n' "$DIFF" | codegraph patch src/server.rs           # diff via stdin
codegraph patch /etc/hosts --diff a.patch --allow-outside       # opt in to paths outside cwd
```

Exit code is non-zero on failure; default output is JSON, pass `--text` for a one-line summary.

### Serve as an MCP server

```sh
codegraph mcp
```

Speaks newline-delimited JSON-RPC 2.0 over stdin/stdout (the MCP stdio
transport) and exposes the **read-only** query commands as native tools:
`search`, `definition`, `outline`, `snippet`, `references`. Mutating commands
(`patch`, `index`, `watch`) are deliberately not served. Each tool reads
`.codegraph/index.json` relative to the working directory, so launch it with
cwd set to the repo root. Intended to be wired into an agent host, not run by
hand — e.g. for Claude:

```sh
claude --mcp-config '{"mcpServers":{"codegraph":{"type":"stdio","command":"codegraph","args":["mcp"]}}}' \
       --strict-mcp-config \
       --allowedTools mcp__codegraph__search,mcp__codegraph__definition,mcp__codegraph__outline,mcp__codegraph__snippet,mcp__codegraph__references
```

The Printer CLI wires this automatically for the Claude backend when the
`codegraph` binary is on PATH (see `printer run`/`exec`), using the absolute
binary path so it resolves inside sandbox VMs too.

## Output format

Default is JSON for easy parsing. `--text` flips every command to a compact
tab-separated form intended for shell pipelines and human inspection:

```sh
codegraph --text search foo
codegraph --text outline src/lib.rs
codegraph --text snippet src/server.rs handle_request
```

## Token-saving rules of thumb

- **Outline before reading.** `codegraph outline <file>` shows signatures only.
- **Snippet, don't Read.** Pulling one function from a large file beats reading the whole thing.
- Use `--limit` on `search` to cap noisy queries; use `--name` to skip signature text.
- `--text` for human inspection; default JSON for tooling.
