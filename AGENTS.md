# Heyo Public Monorepo

## Pull Request Completion

- When the user asks to create or update a PR, the completion criterion is **`git submit` ready**, not GitHub Actions or PR-check status.
- Before reporting a PR change ready, ensure the intended changes are committed, the worktree is clean, and the net branch diff applies cleanly to the latest trunk exactly as `git submit` requires.
- Do not tell the user to wait for GitHub PR CI and do not use GitHub-hosted checks as the readiness signal unless the user explicitly asks for them.
- Do not run `git submit` unless the user explicitly asks; report that it is ready for the user to submit.

## General
- build and install CLIs: `make install`

## Computer
- path: `computer/`
- build: `cargo build`
- lint: `cargo clippy`
- format: `cargo fmt`

## Codegraph
- path: `codegraph/`
- build: `cargo build`
- lint: `cargo clippy`
- format: `cargo fmt`

## Printer
- path: `printer/`
- build: `cargo build`
- lint: `cargo clippy`
- format: `cargo fmt`
- test: `cargo test`

## Platform services
- paths: `heyosecret/`, `heyosecret-client/`, `orchestrator/`, `app-lb/`
- build: `cargo build --locked --manifest-path <path>/Cargo.toml`
- check: `cargo check --locked --manifest-path <path>/Cargo.toml`
- test: `cargo test --locked --manifest-path <path>/Cargo.toml`
- format check: `cargo fmt --manifest-path <path>/Cargo.toml -- --check`
