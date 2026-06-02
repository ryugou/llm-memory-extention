# syntax=docker/dockerfile:1
#
# Builder stage で両バイナリ (llm-memory-server, vegapunk-memory-server) を一度
# だけビルドし、runtime image を bin ごとに 2 stage に分けて切り出す。
# docker-compose 側で `target:` を指定して必要な image を選ぶ。
# 1 build で 2 image が生えるので、`docker compose build` 時に重複コンパイル
# (workspace の依存解決) は走らない。
FROM rust:1.88-slim AS builder
WORKDIR /app
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*

# Copy manifests first to leverage layer caching.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# Build both server binaries in release mode in a single pass.
# vegapunk-memory-server depends on tonic-build → protoc; the apt install above
# pulls protobuf-compiler so the build script succeeds.
RUN cargo build --release \
      --bin llm-memory-server \
      --bin vegapunk-memory-server

# ---- llm-memory-server runtime image -------------------------------------
FROM gcr.io/distroless/cc-debian12 AS llm-memory-server
COPY --from=builder /app/target/release/llm-memory-server /usr/local/bin/llm-memory-server
COPY --from=builder /app/crates/llm-memory-storage/migrations /migrations
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/llm-memory-server"]

# ---- vegapunk-memory-server runtime image --------------------------------
FROM gcr.io/distroless/cc-debian12 AS vegapunk-memory-server
COPY --from=builder /app/target/release/vegapunk-memory-server /usr/local/bin/vegapunk-memory-server
COPY --from=builder /app/crates/vegapunk-memory-storage/migrations /migrations
EXPOSE 8081
ENTRYPOINT ["/usr/local/bin/vegapunk-memory-server"]
