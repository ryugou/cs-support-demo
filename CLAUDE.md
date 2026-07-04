# CLAUDE.md

このリポジトリは、カスタマーサポート担当向け Production CS MCP を設計・実装するためのリポジトリである。対象は日本語で問い合わせ対応を行う CS 担当者で、バックエンドの文書グラフ検索・構造取得・更新には vegapunk / PunkRecord を使う。

本件以外のプロジェクト設計や既存アーキテクチャを持ち込まないこと。

## Production CS MCP Phase

Production CS MCP の設計・実装では、必ず `specs/production-cs-mcp.md` を先に読む。

このフェーズのゴールは次の 3 課題の解決である。

- ノウハウの蓄積
- 権限管理
- 確実なエスカレーション

`cs-support-mcp` は AuthN / AuthZ Harness を内包する CS domain MCP として扱う。LLM client と MCP の間に任意の外部 Harness を置き、その外部 Harness だけに認可・回答可否・エスカレーションを任せる設計は採用しない。

このフェーズに関する議論・設計・実装判断は、会話履歴ではなく `specs/production-cs-mcp.md` を source of truth とする。結論が更新されたら同じターンで同ファイルを更新する。

## 目的

- 日本語の顧客問い合わせに対し、Claude / ChatGPT が日本語で回答根拠を組み立てられる CS domain MCP を実装する。
- 英語マニュアルを翻訳しながら grep するのではなく、文書構造をグラフとして保持し、構造 traversal で根拠を取得する。
- Production では MCP サーバが AuthN / AuthZ Harness、scope enforcement、回答可能性判定、エスカレーション判定、監査、ノウハウ蓄積を担う。

## 技術スタック

- 常駐 MCP サーバ: Rust
- Rust サーバ: `axum` + `rmcp`
- Rust の基本依存: `tokio`, `serde`, `serde_json`, `anyhow`, `tracing`
- vegapunk 接続: 専用クライアント層に隔離する
- vegapunk 接続は `tonic` + `prost` による gRPC client を使う
- `vegapunk-proto` の `graphrag.proto` を `build.rs` で生成する
- ingest / schema 登録 / 検証 CLI: Rust
- 翻訳: 初期検証では fixture の日本語訳を使ってよい。Production では翻訳状態と再翻訳境界を明示する。

Python は使用しない。提案もしない。Python ファイルを作らない。
TypeScript も使用しない。tsx / npm / package.json を追加しない。

## 現行サンプル実装メモ

この節以降の既存 schema / ingest / tool / acceptance / 起動手順のうち、`sivira-cs-demo`、サンプル商品、fixture、`ingest_demo`、`verify_demo`、検証用 GCE domain に依存するものは、現行サンプル実装の記録である。

Production CS MCP の設計判断では `specs/production-cs-mcp.md` を優先する。現行サンプル実装から流用する場合も、AuthN / AuthZ Harness、scope enforcement、ノウハウ蓄積、確実なエスカレーションを前提に再設計する。

## 予定ディレクトリ構成

```text
cs-support-mcp/
  server/
    Cargo.toml
    build.rs
    config.toml
    proto/
      graphrag.proto
    src/
      main.rs
      config.rs
      project.rs
      mcp.rs
      ingest.rs
      translate.rs
      resolve.rs
      vegapunk.rs
      model.rs
    data/
      manual.sample.json
      glossary.json
    src/bin/
      ingest_demo.rs
      verify_demo.rs
  schema/
    cs-schema.yml
    cs-schema.md
  README.md
```

このリポジトリ直下を `cs-support-mcp/` 相当として扱ってよい。不要な親ディレクトリを追加しない。

## 言語と翻訳方針

- 英語原文を source of truth とする。
- 日本語は英語原文の投影であり、検索対象は日本語本文 `body_ja` とする。
- 英語を残す理由:
  - 原文変更の差分検知
  - 再翻訳のソース
  - 訳が無い、または stale のときの回答フォールバック
- section ごとに `en_hash` と `translated_from_hash` を持つ。
- `en_hash != translated_from_hash` の section は stale とみなす。
- マニュアル再投入時は、`en_hash` が変わった section だけ再翻訳する。
- glossary を翻訳工程に注入し、型番・製品名・専門語の訳ブレを防ぐ。
- 現行実装では訳の手動オーバーライド保護は実装しない。`translation_status = manual_override` は予約値としてのみ扱う。

## Graph Schema 方針

論理スキーマは以下を満たすこと。ただし vegapunk への登録 DDL / 定義ファイル構文は、必ず vegapunk 現行の schema 定義を正とする。推測で構文を作らない。

### Node Types

- `product`
  - `product_key` string required, stable upsert key
  - `name_en` string required
  - `name_ja` string required
  - `model` string optional
  - `status` string optional, `active` / `discontinued`
  - `description_en` string optional
  - `description_ja` string optional
- `document`
  - `doc_id` string required, stable upsert key
  - `product_key` string required
  - `title_en` string required
  - `title_ja` string required
  - `version` string required
  - `source_url` string optional
  - `origin_lang` string optional, usually `en`
- `section`
  - `section_key` string required, stable upsert key, `{doc_id}#{anchor}`
  - `doc_id` string required
  - `order` int required
  - `level` int required
  - `title_en` string required
  - `title_ja` string required
  - `body_en` string optional
  - `body_ja` string optional
  - `en_hash` string required
  - `translated_from_hash` string optional
  - `translation_status` string optional, `current` / `stale` / `missing` / `manual_override`
- `spec`
  - `spec_key` string required, stable upsert key, `{product_key}:{key_slug}`
  - `product_key` string required
  - `key_en` string required
  - `key_ja` string required
  - `value_en` string required
  - `value_ja` string optional

### Edge Types

- `HAS_DOCUMENT`: `product -> document`
- `CONTAINS`: `document -> section` and `section -> section`
- `REFERENCES`: `section -> section`
- `HAS_SPEC`: `product -> spec`
- `DEFINED_IN`: `spec -> section`

`issue` は今回のスキーマに入れない。

## Product Resolution

別名テーブルや aliases リストは持たない。`resolve_product` は次の組み合わせで実装する。

- 正規化マッチ:
  - `name_en`, `name_ja`, `model` を対象にする
  - 大小文字、全半角、空白、記号を無視する
  - 部分一致と編集距離を使う
- 意味マッチ:
  - product の `name` + `description` 相当の埋め込み類似を使う
- 結果:
  - 0 件: 意味的近傍を候補として返す
  - 1 件: 確定候補として返す
  - 複数件: 候補配列を返し、クライアントに絞り込ませる

## MCP Tools

### Read Tools

- `resolve_product(text)`
  - 日本語・英語・型番らしき入力から product 候補を返す。
- `search_manual(query_ja, product_key?, top_k?)`
  - `query_ja` は日本語で渡すことを tool description に明記する。
  - 日本語本文 `body_ja` を検索する。
  - 返却には `section_key`, `title_ja`, `body_ja`, `body_en`, `translation_status`, `breadcrumb` を含める。
  - `product_key` 指定時は該当 product の document 部分木に限定する。
- `get_section(section_key, expand?)`
  - 当該 section、祖先、子、参照先を返す。
  - traversal は最大 2 hop に制限する。
- `get_product(product_key)`
  - product 概要、HAS_SPEC の仕様、document の TOC を返す。

### Write Tools

- `upsert_product`
- `upsert_section`
- `upsert_spec`

安定キーで merge する。`*_en` 更新時は glossary 注入で `*_ja` を再生成し、`en_hash`, `translated_from_hash`, `translation_status` も更新する。hard delete は公開せず、`status` による論理削除を使う。

## VegapunkClient

`server/src/vegapunk.rs` に `VegapunkClient` を定義する。vegapunk の実体 API は推測しない。

必要メソッド:

- `search(schema, query_ja, product_key, top_k) -> Vec<SectionHit>`
- `get_subgraph(schema, section_key, max_hops) -> Subgraph`
- `get_product_view(schema, product_key) -> ProductView`
- `resolve_product(schema, text) -> Vec<ProductCandidate>`
- `upsert_nodes(schema, nodes)`
- `upsert_edges(schema, edges)`

不明な API 境界は `// TODO: bind to vegapunk <api>` を残し、endpoint / proto / tool name / request shape は config 化する。

すべての VegapunkClient メソッドは `schema` を引数に取る。呼び出し側は `project.rs` で URL の `project_id` から schema を解決して必ず渡す。CS 担当の入力に schema を含めない。

## Project Routing and Auth

- 1 project = 1 vegapunk schema とする。
- cross-schema 検索はしない。
- MCP endpoint は `/{project_id}/mcp`。
- `project_id` から schema を解決し、vegapunk 呼び出しへ注入する。
- 認証はプロジェクトごとの静的 Bearer token とする。
- 現行サンプル project は `sivira-cs-demo` の 1 件のみ。
- mapping は 1 件でも、将来別 schema を引ける構造にする。

## Ingest

- `server/src/ingest.rs` は構造化入力から `product`, `document`, `section`, `spec` と edge を組み立てる。
- 入力サンプルは `server/data/manual.sample.json`。
- glossary は `server/data/glossary.json`。
- `server/src/translate.rs` は fixture 日本語訳の検証、glossary 適用、将来の外部翻訳境界を担当する。
- `server/src/bin/ingest_demo.rs` は schema 登録と low-level graph upsert を行う。
- `server/src/bin/verify_demo.rs` は `Search`, `QueryNodes`, `GetGraphSnapshot` による検証を行う。
- 初期投入では issue を投入しない。
- 再投入では `section_key` が一致し、かつ `en_hash` が変わった section だけ再翻訳して upsert する。
- `product_key = SKU`
- `doc_id = 文書ID`
- `section_key = {doc_id}#{anchor}`
- `spec_key = {product_key}:{key_slug}`

## 実装順序

1. `schema/cs-schema.md` に vegapunk 登録手順と論理スキーマを具体化する。
2. Rust ingest を作り、サンプル英語マニュアルを文書ツリー化し、翻訳済み fixture を検証して投入する。
3. MCP 読み取り系を実装する。
4. MCP 更新系を実装する。
5. 再投入の差分翻訳を実装・確認する。
6. `/{project_id}/mcp` に接続し、日本語相談から文書グラフ根拠を取得できることを確認する。

## Acceptance Criteria

- 初期サンプルデータに issue が 1 件もない。
- product + document + section だけで日本語質問に回答できる。
- `resolve_product` が部分一致と複数候補返却を満たす。
- aliases テーブルに依存しない。
- `search_manual` が日本語クエリで `body_ja` を検索する。
- `search_manual` が breadcrumb と `body_en` を返す。
- `get_section` が祖先、子、参照を返す。
- マニュアル再投入で変更 section のみ再翻訳される。
- `translation_status` が `missing` / `stale` の section でも `body_en` から回答できる情報を返す。
- URL の `project_id` を変えると別 schema を引ける構造になっている。
- Python ファイルが存在しない。

## 現行サンプル実装の Out of Scope

この節は既存サンプル実装の制約であり、Production CS MCP のスコープ定義ではない。Production CS MCP の source of truth は `specs/production-cs-mcp.md` とする。

- issue / トラブルログの蓄積や活用
- issue node / edge のスキーマ追加
- 訳の手動オーバーライド保護
- OIDC
- ウォレット認証
- 管理 UI
- プロビジョニング自動化
- 10 万ノード超のスケール最適化
- 監査ログ

## Agent Working Rules

- まずこの AGENTS.md を読み、ここに書かれた制約を優先する。
- vegapunk API の不明点を推測で実装しない。
- 不明な vegapunk 接続仕様は TODO と config 境界に閉じ込める。
- スコープ外機能を先回りして作らない。
- 既存ファイルができた後は、既存の構成・命名・型に合わせて最小差分で変更する。
- Rust の検証コマンドを優先して実行する。
- 変更後は可能な範囲で `cargo test`、`cargo fmt`、`cargo check`、ingest / verify CLI を実行する。
- ネットワークや外部 API が必要な検証は、実行できなかった理由を明記する。

## ローカル MCP 起動手順

ローカルでは `vegapunk` を起動しない。既存ホスト `vegapunk` / `vegapunk.local` 上で動作している GraphRAG gRPC backend に接続する。

次回「ローカル起動して」と言われたら、必ずこの状態にする。

- MCP server listen: `https://127.0.0.1:3443`
- MCP endpoint: `https://127.0.0.1:3443/sivira-cs-demo/mcp`
- project id: `sivira-cs-demo`
- vegapunk gRPC endpoint: `http://vegapunk.local:6840`
- bearer token file: `/private/tmp/vegapunk-bearer-token`
- config file: `server/config.local-https.toml`
- TLS cert/key: `server/certs/cert.pem`, `server/certs/key.pem`
- 使わない port: `3000`
- 使わない tunnel port: `16840`
- 禁止: ローカルで `vegapunk` を起動しない
- 禁止: `ssh -L 16840:...` の tunnel を張らない

`.mcp.json` は次を指す。

```json
{
  "mcpServers": {
    "cs-support-demo": {
      "type": "http",
      "url": "https://127.0.0.1:3443/sivira-cs-demo/mcp"
    }
  }
}
```

前提:

- `server/config.local-https.toml` の `vegapunk_endpoint` は `http://vegapunk.local:6840`
- TLS 証明書は `server/certs/cert.pem` と `server/certs/key.pem`
- vegapunk bearer token は `/private/tmp/vegapunk-bearer-token` に置く
- `vegapunk` をローカルで起動しない。SSH tunnel も不要
- `server/config.toml` の HTTP `127.0.0.1:3000` 起動は `.mcp.json` と一致しないため使わない

token が無い場合だけ、既存 `vegapunk` ホストから取得する。

```sh
ssh vegapunk 'ruby -ryaml -e "c=YAML.load_file(File.expand_path(%q[~/.config/vegapunk/config.yml])); print c.dig(%q[server],%q[auth],%q[token])"' > /private/tmp/vegapunk-bearer-token
```

ローカル MCP サーバを起動する。

```sh
cd server
env -u RUSTC_WRAPPER \
  CARGO_BUILD_RUSTC_WRAPPER= \
  RUST_LOG=info \
  VEGAPUNK_BEARER_TOKEN_FILE=/private/tmp/vegapunk-bearer-token \
  cargo run --bin cs-support-mcp -- --config config.local-https.toml
```

すでに別の `cs-support-mcp` が `3443` を掴んでいる場合は、古いプロセスを止めてから上記で起動し直す。`3000` で起動しているプロセスがあれば、それは `.mcp.json` から使われない古い起動なので止める。

起動確認:

```sh
curl -ksS https://127.0.0.1:3443/healthz
```

MCP の疎通確認は `Accept: application/json, text/event-stream` を付けて行う。通常の GET や `Accept` なしの POST は Streamable HTTP transport に拒否される。

```sh
curl -kNsS -H 'Accept: application/json, text/event-stream' \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
  https://127.0.0.1:3443/sivira-cs-demo/mcp
```

`search_manual` がクライアント側で失敗する場合は、まずクライアントが古い MCP セッションを掴んでいないか確認し、MCP 接続を再読み込みする。サーバ側の直叩きで `structuredContent.hits` が返るなら、MCP サーバ本体ではなくクライアントの接続状態を疑う。

## GCE デプロイ手順

このリポジトリは、共有 GCE VM `llm-memory` 上の既存 `llm-memory-extention` stack に `cs-support-mcp` service としてデプロイする。Caddy も同じ stack に同居している。

- VM: `llm-memory`
- zone: `asia-northeast1-a`
- remote source dir: `/home/ryugo/cs-support-demo`
- stack dir: `/home/ryugo/llm-memory-extention/docker`
- deploy wrapper: `/home/ryugo/llm-memory-extention/deploy/gce/run.sh`
- public domain: `cs-support-136-110-78-245.nip.io`
- public MCP endpoint: `https://cs-support-136-110-78-245.nip.io/sivira-cs-demo/mcp`
- internal port: `8080`
- bind addr: `BIND_ADDR=0.0.0.0:8080`
- config file in container: `/app/server/config.gce.toml`
- required env: `VEGAPUNK_BEARER_TOKEN`, `VEGAPUNK_ENDPOINT`, `CS_SUPPORT_PUBLIC_DOMAIN`
- vegapunk endpoint on GCE: `VEGAPUNK_ENDPOINT=${VEGAPUNK_GRPC_ENDPOINT}`
- secret source: 既存 stack の Secret Manager injection。平文 token を `.env` に置かない
- 禁止: `docker compose` を直接実行しない

既存 stack 側の設定:

- `/home/ryugo/llm-memory-extention/docker/docker-compose.override.yml`
  - `cs-support-mcp` service を定義する
  - `BIND_ADDR=0.0.0.0:8080`
  - `VEGAPUNK_ENDPOINT=${VEGAPUNK_GRPC_ENDPOINT:?VEGAPUNK_GRPC_ENDPOINT is required}`
  - `CS_SUPPORT_PUBLIC_DOMAIN=${CS_SUPPORT_PUBLIC_DOMAIN}`
  - `VEGAPUNK_BEARER_TOKEN` は Secret Manager injection で渡す
- `/home/ryugo/llm-memory-extention/docker/Caddyfile`
  - `{$CS_SUPPORT_PUBLIC_DOMAIN}` の site block を追加する
  - `reverse_proxy cs-support-mcp:8080`
- `/home/ryugo/llm-memory-extention/.env`
  - `VEGAPUNK_GRPC_ENDPOINT=http://10.10.0.2:6840`
  - `CS_SUPPORT_PUBLIC_DOMAIN=cs-support-136-110-78-245.nip.io`

ローカルの作業ツリーを VM に転送して service を build / restart する。デプロイ/再起動は必ず wrapper を使う。

```sh
COPYFILE_DISABLE=1 tar \
  --exclude .git \
  --exclude server/target \
  --exclude .DS_Store \
  --exclude server/certs \
  -czf /private/tmp/cs-support-demo-src.tar.gz .

gcloud compute scp \
  --zone asia-northeast1-a \
  /private/tmp/cs-support-demo-src.tar.gz \
  llm-memory:~/cs-support-demo-src.tar.gz

gcloud compute ssh llm-memory --zone asia-northeast1-a --command '
set -e
rm -rf ~/cs-support-demo
mkdir -p ~/cs-support-demo
tar -xzf ~/cs-support-demo-src.tar.gz -C ~/cs-support-demo
rm -f ~/cs-support-demo-src.tar.gz
~/llm-memory-extention/deploy/gce/run.sh up -d --build cs-support-mcp
'
```

Caddyfile または Caddy service 側の env / depends_on を変えた場合だけ、Caddy も wrapper で反映する。

```sh
gcloud compute ssh llm-memory --zone asia-northeast1-a --command '
set -e
~/llm-memory-extention/deploy/gce/run.sh up -d --build caddy
'
```

初回またはサンプルデータを入れ直すときは、起動済み container 内の CLI で ingest / verify する。`VEGAPUNK_ENDPOINT` は container 内で展開させるため、必ず `/bin/sh -lc` を使う。

```sh
gcloud compute ssh llm-memory --zone asia-northeast1-a --command '
set -e
docker exec llm-memory-extention-cs-support-mcp-1 /bin/sh -lc '\''/usr/local/bin/ingest_demo --endpoint "$VEGAPUNK_ENDPOINT" --schema sivira-cs-demo --schema-file /app/schema/cs-schema.yml --manual-file /app/server/data/manual.sample.json --glossary-file /app/server/data/glossary.json'\''
docker exec llm-memory-extention-cs-support-mcp-1 /bin/sh -lc '\''/usr/local/bin/verify_demo --endpoint "$VEGAPUNK_ENDPOINT" --schema sivira-cs-demo'\''
'
```

デプロイ後の公開確認:

```sh
curl -sS https://cs-support-136-110-78-245.nip.io/healthz
```

MCP の確認は `Accept: application/json, text/event-stream` を付ける。`initialize` の response header `mcp-session-id` を、以降の `tools/list` / `tools/call` に渡す。

```sh
curl -NsS -D /private/tmp/cs-support-public-headers.txt \
  -H 'Accept: application/json, text/event-stream' \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
  https://cs-support-136-110-78-245.nip.io/sivira-cs-demo/mcp

SESSION=$(awk 'BEGIN{IGNORECASE=1} /^mcp-session-id:/ {gsub("\r", "", $2); print $2}' /private/tmp/cs-support-public-headers.txt)

curl -NsS \
  -H 'Accept: application/json, text/event-stream' \
  -H 'Content-Type: application/json' \
  -H "mcp-session-id: $SESSION" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_manual","arguments":{"product_key":"SVR-HB100","query_ja":"ボタンが反応しない 無反応 再起動 電源 トラブルシューティング","top_k":5}}}' \
  https://cs-support-136-110-78-245.nip.io/sivira-cs-demo/mcp
```

現在の正常系確認結果:

- `healthz`: `200 ok`
- `ingest_demo`: `upserted_nodes=42`, `upserted_edges=54`
- `verify_demo`: `products=3`, `sections=27`, `specs=9`, `snapshot_nodes=42`, `snapshot_edges=54`
- `resolve_product("SVR-HB100")`: `SVR-HB100` が `normalized_match`, score `1.0`
- `search_manual` の SVR-HB100 問い合わせ: `structuredContent.hits` が返る
