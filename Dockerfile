FROM rust:1-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app/server
COPY server/Cargo.toml server/Cargo.lock server/build.rs ./
COPY server/proto ./proto
COPY server/src ./src
RUN cargo build --release --locked \
    --bin cs-support-mcp \
    --bin ingest_demo \
    --bin verify_demo \
    --bin ingest_rules \
    --bin ingest_urtect \
    --bin ingest_products \
    --bin ingest_alarmcom \
    --bin verify_alarmcom \
    --bin merge_schema

FROM debian:bookworm-slim

WORKDIR /app/server
COPY --from=builder /app/server/target/release/cs-support-mcp /usr/local/bin/cs-support-mcp
COPY --from=builder /app/server/target/release/ingest_demo /usr/local/bin/ingest_demo
COPY --from=builder /app/server/target/release/verify_demo /usr/local/bin/verify_demo
COPY --from=builder /app/server/target/release/ingest_rules /usr/local/bin/ingest_rules
COPY --from=builder /app/server/target/release/ingest_urtect /usr/local/bin/ingest_urtect
COPY --from=builder /app/server/target/release/ingest_products /usr/local/bin/ingest_products
COPY --from=builder /app/server/target/release/ingest_alarmcom /usr/local/bin/ingest_alarmcom
COPY --from=builder /app/server/target/release/verify_alarmcom /usr/local/bin/verify_alarmcom
COPY --from=builder /app/server/target/release/merge_schema /usr/local/bin/merge_schema
COPY server/config.gce.toml ./config.gce.toml
COPY server/config.cloudrun.toml ./config.cloudrun.toml
COPY server/data ./data
COPY schema /app/schema

ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/cs-support-mcp"]
# Cloud Run がデフォルト実行環境。GCE 互換運用は起動コマンドで
# `--config /app/server/config.gce.toml` を明示指定して上書きする。
CMD ["--config", "/app/server/config.cloudrun.toml"]
