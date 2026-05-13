# syntax=docker/dockerfile:1
FROM rust:1.88-slim AS builder
WORKDIR /app
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Copy manifests first to leverage layer caching.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# Build the server binary in release mode.
RUN cargo build --release --bin llm-memory-server

# ---- Runtime image ----
FROM gcr.io/distroless/cc-debian12
COPY --from=builder /app/target/release/llm-memory-server /usr/local/bin/llm-memory-server
COPY --from=builder /app/crates/llm-memory-storage/migrations /migrations
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/llm-memory-server"]
