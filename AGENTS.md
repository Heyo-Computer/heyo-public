# Heyo Public Monorepo

## Pull Request Completion

- When the user asks to create or update a PR, completion requires both an open PR and **`git submit` readiness**, not GitHub Actions or PR-check status.
- After every authorized branch push, create or update the open PR targeting the intended trunk and report its URL. A merged or closed PR from an earlier use of the branch does not count.
- Before reporting a PR change ready, ensure the intended changes are committed, the worktree is clean, the branch contains the latest trunk, its net diff applies cleanly to that trunk exactly as `git submit` requires, `HEAD` matches the remote branch, and the PR's head SHA matches `HEAD`.
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
- paths: `heyosecret/`, `heyosecret-client/`, `orchestrator/`, `app-lb/`, `app-obs/`, `artifacts/`, `ci/`
- build: `cargo build --locked --manifest-path <path>/Cargo.toml`
- check: `cargo check --locked --manifest-path <path>/Cargo.toml`
- test: `cargo test --locked --manifest-path <path>/Cargo.toml`
- format check: `cargo fmt --manifest-path <path>/Cargo.toml -- --check`
