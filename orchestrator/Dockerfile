FROM rust:latest AS builder

WORKDIR /app

# Copy the orchestrator and its public path dependency.
COPY heyosecret-client/ heyosecret-client/
COPY orchestrator/ orchestrator/

# Build the release binary
RUN cargo build --locked --release --manifest-path orchestrator/Cargo.toml

# Runtime image - use same as builder to ensure GLIBC compatibility
FROM rust:latest

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/orchestrator/target/release/orchestrator /usr/local/bin/orchestrator
COPY --from=builder /app/orchestrator/migrations /app/migrations

EXPOSE 4446

CMD ["orchestrator"]
