# Heyo Public Monorepo

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
