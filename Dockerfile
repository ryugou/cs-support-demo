FROM node:22-bookworm AS admin-ui-builder

WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/package-lock.json ./
RUN npm ci
COPY admin-ui/ ./
# 管理画面（/admin）は Google Identity Services の client_id をビルド時に静的ファイルへ
# 埋め込む（design doc 2026-08-16-admin-dashboard-design.md §5: 公開識別子のため埋め込み可）。
#
# fail closed（reviewer 指摘 Critical 1 で挙動を反転させた）: 当初は「build-arg 未指定なら
# 既定値 placeholder のままビルドされる = 安全側のデフォルト」としていたが、これは誤りだった。
# コミット前のレビューで、CI（.github/workflows/deploy.yml）側の build-arg 受け渡しがまだ
# 未実装であることが判明した。その状態のままマージしていれば、ビルドは成功するのに
# `dist/.../main-*.js` に placeholder 文字列がそのまま焼き込まれ、出荷後の `/admin` は
# 誰もログインできない状態になり、しかもビルド成功として気づく手段が無かった。「壊れた
# 成果物を静かに出荷しうる」より「ビルドを落として運用者に気づかせる」方が安全という
# 判断（`server/src/main.rs` の起動時 fail-closed チェックと同じ方針）で、値が空 or
# 文字種不正なら明確なメッセージとともにビルドを失敗させる。
#
# Google OAuth Client ID は英数字と `-` `.` `_` のみで構成される（Google Cloud Console の
# 発行形式。例: 1234567890-abcdefghijklmnop.apps.googleusercontent.com）。この文字種検証は
# sed の区切り文字 `/` や正規表現メタ文字（`&` 等）の混入も同時に防ぐ（このビルド引数は
# 運用者・CI 変数が Cloud Console の値をそのまま渡す想定で、任意のユーザ入力を受け付ける
# 設計ではないが、設定ミスで意図しない文字列が渡ってもここで弾く）。
ARG CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID
RUN value="${CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID:-}"; \
    if [ -z "$value" ]; then \
        echo "FATAL: CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID build-arg is not set (or empty)." >&2; \
        echo "  /admin (Google Identity Services login) needs the real Google OAuth client id" >&2; \
        echo "  embedded at build time; without it every login attempt fails after deploy." >&2; \
        echo "  Pass it with: docker build --build-arg CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID=<client id> ..." >&2; \
        echo "  In CI this comes from the GitHub Actions variable of the same name (see CLAUDE.md)." >&2; \
        exit 1; \
    fi; \
    case "$value" in \
        *[!A-Za-z0-9._-]*) \
            echo "FATAL: CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID contains characters outside [A-Za-z0-9._-]: '$value'" >&2; \
            exit 1; \
            ;; \
    esac
RUN sed -i "s/__CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID__/${CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID}/" src/environments/environment.prod.ts
RUN npm run build -- --configuration=production --base-href=/admin/

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
    --bin merge_schema \
    --bin backfill_concept_keys \
    --bin line_adapter \
    --bin homesec_advisor \
    --bin ingest_homesec

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
COPY --from=builder /app/server/target/release/backfill_concept_keys /usr/local/bin/backfill_concept_keys
COPY --from=builder /app/server/target/release/line_adapter /usr/local/bin/line_adapter
COPY --from=builder /app/server/target/release/homesec_advisor /usr/local/bin/homesec_advisor
COPY --from=builder /app/server/target/release/ingest_homesec /usr/local/bin/ingest_homesec
COPY server/config.gce.toml ./config.gce.toml
COPY server/config.cloudrun.toml ./config.cloudrun.toml
COPY server/data ./data
COPY schema /app/schema
# 管理 SPA の静的ビルド成果物のみを同梱する（admin-ui-builder ステージの node_modules は
# 持ち込まない）。config.cloudrun.toml / config.gce.toml の admin_static_dir = "admin-ui/browser"
# と対応する（WORKDIR /app/server からの相対で /app/server/admin-ui/browser）。
COPY --from=admin-ui-builder /app/admin-ui/dist/admin-ui/browser ./admin-ui/browser

ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/cs-support-mcp"]
# Cloud Run がデフォルト実行環境。GCE 互換運用は起動コマンドで
# `--config /app/server/config.gce.toml` を明示指定して上書きする。
CMD ["--config", "/app/server/config.cloudrun.toml"]
