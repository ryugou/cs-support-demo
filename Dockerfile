FROM rust:1-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app/server
COPY server/Cargo.toml server/Cargo.lock server/build.rs ./
COPY server/proto ./proto
COPY server/src ./src
RUN cargo build --release --bin cs-support-mcp --bin ingest_demo --bin verify_demo

FROM debian:bookworm-slim

WORKDIR /app/server
COPY --from=builder /app/server/target/release/cs-support-mcp /usr/local/bin/cs-support-mcp
COPY --from=builder /app/server/target/release/ingest_demo /usr/local/bin/ingest_demo
COPY --from=builder /app/server/target/release/verify_demo /usr/local/bin/verify_demo
COPY server/config.gce.toml ./config.gce.toml
COPY server/data ./data
COPY schema /app/schema

ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/cs-support-mcp"]
CMD ["--config", "/app/server/config.gce.toml"]
