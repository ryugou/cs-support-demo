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

この節以降の既存 schema / ingest / tool / acceptance / 起動手順のうち、`sivira-cs-demo`、サンプル商品、fixture、`ingest_demo`、`verify_demo`、検証用 GCE domain（旧構成、参考。Cloud Run へ移行済み）に依存するものは、現行サンプル実装の記録である。

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
- 認証は OAuth 2.1 とする。`cs-support-mcp` は**認可サーバ（AS）兼リソースサーバ（RS）**として動作し、内部で Google（`accounts.google.com`）に委譲する（OAuth フェデレーション）。無トークンアクセスは `401` + `WWW-Authenticate: Bearer resource_metadata="https://<host>/.well-known/oauth-protected-resource/{project_id}/mcp"` を返し、クライアントはこのメタデータ経由で認可サーバ（= このサービス自身）を発見する。プロジェクトごとの静的 Bearer token・静的 JWT は撤去済み。
- **AS を自前化した理由**: Google は RFC 7591 の DCR（動的クライアント登録）に対応していない。認可サーバを Google 自身にすると、claude.ai は利用者ごとに Google の Client ID / Secret を詳細設定へ手入力させる必要があり、運用不能かつ Client Secret がクライアント側に置かれる。現行は Google の client_id / secret をサーバ側 env に閉じ込め、claude.ai は MCP endpoint の URL のみで接続できる。実装は `server/src/oauth/authserver.rs`、署名基盤は `server/src/oauth/signing.rs`。
- **トークンは自前発行しない。** 一時期は自前のアクセス/リフレッシュトークンを署名付きで発行していたが、デモに対して寿命・失効・鍵管理を自前で抱える設計が過剰と判断され撤回した。現行の `/oauth/token` は **Google が発行した access_token / refresh_token / expires_in をそのままクライアントへ返し**、`grant_type=refresh_token` は受け取った refresh_token を **Google の token endpoint へ中継する**だけである。AS の外殻（DCR / authorize / callback / token）は claude.ai の DCR のために残している。
- **confused deputy 対策は redirect_uri の許可リスト。** `/oauth/register`（DCR）は無認証のため、redirect_uri を無制限に受け付けると第三者が任意ホストの redirect_uri を登録でき、`callback` がその第三者へ認可コードを配送してしまう。`is_acceptable_redirect_uri`（`server/src/oauth/authserver.rs`）が https を `claude.ai` へのホスト完全一致（サフィックス一致ではない）に、http を loopback のみに絞ることでこの経路を閉じている。一時期はこれを自前の同意画面（`/oauth/consent`）で塞いでいたが、許可リスト導入により経路自体が消えたため撤去済み。**2 段目のリダイレクト（この AS → 動的登録クライアント）は Google 側の redirect_uri 設定では一切守られない**点に注意（1 段目の `https://<host>/oauth/callback` 登録とは別物）。詳細: `docs/superpowers/specs/2026-07-22-restrict-redirect-uri-design.md`。
- **失効は Google 側で行う。** このサーバは失効台帳を持たないため、個別のトークン失効手段が無い。利用者単位の失効は Google アカウントのアクセス権限管理から行う。
- 署名が必要なのは client_id / state / 認可コードだけ（改竄されると redirect_uri の書き換えや認可コード偽造が成立するため）。**署名鍵は `CS_SUPPORT_OAUTH_SIGNING_KEY`（Secret Manager 注入）から読む。** 未設定だと起動時に CSPRNG で生成して warn するが、その状態では**再起動・コールドスタートのたびに利用者の接続が切れる**（下記）。
- **再起動時の影響**: **署名鍵が同一なら、進行中のログインフロー（state / 認可コード、最長 600 秒）も DCR 登録も再起動をまたいで有効**。アクセストークンとリフレッシュトークンは Google 発行なのでそもそもプロセス状態に依存しない。
  - **鍵が変わると（＝未注入時は毎回）両方が無効になり、`grant_type=refresh_token` が `unverifiable_client_id` → `invalid_grant` で落ちて全利用者が再ログインになる。** 「DCR は claude.ai が再登録すれば自動回復する」はリフレッシュ経路には当てはまらない（claude.ai はリフレッシュ前に登録をやり直さず、`invalid_grant` を受け取るだけ）。**これが 2026-08 まで実際に起きていた不具合である。**
- リクエスト経路の Bearer 検証は `GoogleTokenVerifier`（tokeninfo 照会）。`aud` の完全一致・`email_verified`・安定した `sub` の取得・TTL キャッシュを行う。Google に到達できない場合は **503** を返す（401 に倒すと Google 障害が全利用者の強制ログアウトに化けるため）。
- **警告**: 現状、Google アカウントで認証さえ通れば誰でも supervisor として `add_known_resolution` を含む全操作を実行できる（`server/src/harness/authn.rs` の `lookup_by_identity` が突合を行わず無条件に supervisor 解決するため）。actor 突合表の DB 実装が入るまで、アクセス制御としては不十分と扱うこと。詳細は `specs/production-cs-mcp.md` の「AuthN 現状」節を参照。
- **警告（上記の規模）**: Google OAuth 同意画面は 2026-07-21 に External（本番公開）へ切替済みで、テストユーザによる制限は無い。したがって上記「誰でも」の母集団は sivira.co 内部ではなく **全世界の任意の Google アカウント**である。OAuth クライアントが Internal（組織限定）だと仮定しないこと。
- 本番 Cloud Run の project 定義は `urtect` の 1 件のみ（`server/config.cloudrun.toml`）。レガシーの `sivira-cs-demo` は露出面を最小化するため外した。`allowed_schemas` は config 全 project の複製で解決されるため（`server/src/harness/authn.rs`）、**project を追加するとその schema も既存の全 Google 利用者へ自動的に公開される**。テナント分離を成立させる認可境界が無い間は、project を安易に増やさないこと。
- **警告: アクセストークンに project 束縛は無い。** 自前トークンを廃止した結果、クライアントが持つのは Google 発行のトークンであり、こちらの project_id を載せる余地が無い。したがって **ある project 向けに取得したトークンは、このサーバの全 project の endpoint で通る**。旧実装が持っていた `aud` 完全一致の境界は失われている。RFC 8707 の `resource` は `/oauth/authorize` と `/oauth/token` で「設定済み project を指しているか」の入力検証にしか使っていない（`server/src/oauth/authserver.rs` の `resolve_resource`）。
- **project を 2 件目以降に増やすと、OAuth の挙動が破壊的に変わる。** `/oauth/authorize` は project が 1 件のときだけ `resource` 省略を許す。2 件以上になると **`resource` が必須**になり、送らないクライアントは `invalid_target` で拒否される（起動時に `tracing::warn!` で 1 回警告する）。ただし上記のとおり `resource` を送っても**テナント分離にはならない**ので、project を増やす前に認可境界そのものを設計し直すこと。
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
- ウォレット認証
- 管理 UI
- プロビジョニング自動化
- 10 万ノード超のスケール最適化

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

`.mcp.json` は使わない（demo 開発時の localhost 登録ごと削除済み）。MCP クライアントからの接続は
claude.ai のカスタムコネクタ経由に一本化する。ローカルサーバへ疎通確認するときは、下記の `curl` を使う。

前提:

- `server/config.local-https.toml` の `vegapunk_endpoint` は `http://vegapunk.local:6840`
- TLS 証明書は `server/certs/cert.pem` と `server/certs/key.pem`
- vegapunk bearer token は `/private/tmp/vegapunk-bearer-token` に置く
- `vegapunk` をローカルで起動しない。SSH tunnel も不要
- `server/config.toml` の HTTP `127.0.0.1:3000` 起動は使わない（ローカルは `3443` の HTTPS に統一する）

token が無い場合だけ、既存 `vegapunk` ホストから取得する。

```sh
ssh vegapunk 'ruby -ryaml -e "c=YAML.load_file(File.expand_path(%q[~/.config/vegapunk/config.yml])); print c.dig(%q[server],%q[auth],%q[token])"' > /private/tmp/vegapunk-bearer-token
```

ローカル MCP サーバを起動する。次の 3 つは `main.rs` の起動時 fail-closed チェックで必須。
無いと起動に失敗する。

- `CS_SUPPORT_PUBLIC_DOMAIN`
- `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID`
- `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET`

`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` / `..._SECRET` は Google Cloud Console で発行済みの
OAuth クライアントの値を設定する（本番と同一のものを使ってよい。client_id は公開識別子だが
**client_secret は真の秘密**なので、コマンド例に直書きせず各自の値に置き換えること）。

OAuth 署名鍵（`CS_SUPPORT_OAUTH_SIGNING_KEY`）はローカルでは未設定でよい。起動時に CSPRNG で生成され、その旨が warn に出る。ローカルは Google の callback（`https://127.0.0.1:3443/oauth/callback` が Google 未登録）が通らずログインフロー自体を完走できないため、鍵が再起動で変わっても支障が無い。**本番では必ず注入すること**（未注入だとデプロイ・コールドスタートのたびに接続が切れる。Cloud Run 節を参照）。

ただし `https://127.0.0.1:3443/oauth/callback` は Google 側に未登録のため、ブラウザ経由の
OAuth ログインフローそのものはローカルで完結しない。ローカルでの疎通確認は、Bearer 無し
アクセスに対する 401 応答と、`/.well-known/oauth-authorization-server` の 200 応答までに留まる。

```sh
cd server
env -u RUSTC_WRAPPER \
  CARGO_BUILD_RUSTC_WRAPPER= \
  RUST_LOG=info \
  VEGAPUNK_BEARER_TOKEN_FILE=/private/tmp/vegapunk-bearer-token \
  CS_SUPPORT_PUBLIC_DOMAIN=127.0.0.1:3443 \
  CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID=<Google Cloud Console で発行済みの OAuth Client ID> \
  CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET=<同 OAuth クライアントの Client Secret> \
  cargo run --bin cs-support-mcp -- --config config.local-https.toml
```

すでに別の `cs-support-mcp` が `3443` を掴んでいる場合は、古いプロセスを止めてから上記で起動し直す。`3000` で起動しているプロセスがあれば、それは古い起動なので止める。

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

Bearer token を付けていないため、上記は `401` + `WWW-Authenticate` が返るのが正常（`require_google_auth`
ミドルウェア。Cloud Run 節の「認証（OAuth 2.1 フェデレーション、実測済み）」と同じ挙動）。
`initialize` が `200` で通ることを期待するコマンドではない。MCP サーバ自体の疎通確認をしたいだけなら
`curl -ksS https://127.0.0.1:3443/livez`、
`curl -ksS https://127.0.0.1:3443/.well-known/oauth-protected-resource/sivira-cs-demo/mcp`、
`curl -ksS https://127.0.0.1:3443/.well-known/oauth-authorization-server` を使う。

`search_manual` がクライアント側で失敗する場合は、まずクライアントが古い MCP セッションを掴んでいないか確認し、MCP 接続を再読み込みする。サーバ側の直叩きで `structuredContent.hits` が返るなら、MCP サーバ本体ではなくクライアントの接続状態を疑う。

## Cloud Run デプロイ手順

このリポジトリは GCP Cloud Run 上に `cs-support-mcp` service としてデプロイされている（GCE VM `llm-memory` 上の旧構成から移行済み。旧構成は下記「旧構成（参考）」を参照）。

- GCP project: `sivira-cs-support`
- region: `asia-northeast1`
- service: `cs-support-mcp`
- image: `asia-northeast1-docker.pkg.dev/sivira-cs-support/cs-support/cs-support-mcp:<tag>`（`<tag>` は git short SHA を使う運用）
- public URL: `https://cs-support-mcp-235108918288.asia-northeast1.run.app`
- MCP endpoint: `https://cs-support-mcp-235108918288.asia-northeast1.run.app/urtect/mcp`
- Cloud Run jobs（service と同一イメージ）: `ingest-products`, `ingest-rules`, `ingest-urtect`, `ingest-alarmcom`, `merge-schema`, `verify-alarmcom`, `backfill-concept-keys`
- **`server/data/urtect/rules.json` / `server/data/urtect/signal-lexicon.json` はどちらも image 内ファイルが正本**（service と `ingest-rules` job は同一イメージ。Product ノードとは扱いが異なる点に注意）。main マージ時は CI（`auto-ingest` job、`.github/workflows/deploy.yml`）が `server/data/**` の変更を検知し、ビルド → デプロイ → `ingest-rules` 実行までを自動で行う。CI 以外の経路で image を反映しない。`server/data/**` を変更していないのに `rules.json` の再投入だけが必要なとき（＝`auto-ingest` の paths filter が発火しないとき）に限り、`gcloud run jobs execute ingest-rules --project sivira-cs-support --region asia-northeast1 --wait` を単発で実行してよい（`jobs execute` は image を更新しないため、手動デプロイ禁止の対象外）。**`signal-lexicon.json` は vegapunk へ投入する対象ではなく、サービスが起動時にイメージ内ファイルから読むだけなので `ingest-rules` では一切反映されない**（反映漏れは signal が本番で永久に立たないサイレント never-match になる。`ingest_rules.rs` は `--rules-file` のみを受け取り lexicon ファイルには一切触れない）。
- **製品マスタの正本は vegapunk の Product ノード**（Issue #6: `KNOWN_MODELS` 定数は廃止済み）。`server/data/urtect/products.json` はコードではなく、`ingest_products` CLI に渡す seed 投入の入力記録である。
  - 製品を追加する手順: `products.json` に `{ "model", "name", "aliases" }` を追記 → `ingest_products` を実行する。**サービスの再ビルド・再デプロイは不要**（Product ノードは vegapunk 側にしか存在しないため）。
  - 全体リセット後の ingest 実行順序は **`ingest_products` → `ingest_urtect` / `ingest_alarmcom`** の順を必ず守ること。`ingest_urtect` / `ingest_alarmcom` はどちらも起動時に vegapunk の Product ノード一覧を取得し、0 件なら「製品マスタが空。先に `ingest_products` を実行せよ」という fail closed で止まる。`ingest_urtect`（Google Sites）と `ingest_alarmcom`（answers.alarm.com）の間に順序依存は無い（両方 products.json 投入後ならどちらを先に走らせてもよい）。
  - **`backfill-concept-keys`（Issue #8 Phase B2-0/B2-1）は schema 更新後・2-hop 拡張の読み取り経路有効化前に必ず実行すること。** `ManualSection.concept_keys`（`MENTIONS_CONCEPT` 辺の読み取り最適化射影）を書く CLI で、`ingest_alarmcom` の差分 ingest は既存 section を再翻訳しないため単独では埋まらない（未変更記事は `existing_hash == hash` で skip される）。`--probe-only`（書き込みなし・B2-0 の fan-in 実測）/ `--verify`（辺と属性の乖離検出）/ `--probe-one <section_key>`（`UpsertNodes` の意味論を実測し即復旧）/ 既定（全件書き込み・冪等）の 4 モードは相互排他。詳細は `docs/superpowers/specs/2026-08-02-concept-expansion-design.md`。
    - **`backfill-concept-keys` を `ingest_alarmcom` / `ingest_urtect` と同時に走らせないこと（`merge-schema` の「同一 schema で同時 1 本」と同じ粒度の制約）。** backfill は全 ManualSection の属性を読み切ってから書き戻す（読み取り段階で section 数と同じ本数の traverse を逐次発行するため、本番規模では読みと書きの間に数十分〜数時間の差が生じる）。この間に ingest が同じ section を更新すると、**backfill が新しい本文を古い本文で上書きする**。全属性を明示再送する設計上、`UpsertNodes` が部分マージでも全置換でも起きる。実行前に `gcloud run jobs executions list` で ingest job が走っていないことを確認する。
    - 既定モードが途中で失敗した場合、エラーメッセージに `--start-after <section_key>` の形で再開点が出る。その値をそのまま `--args` に足して再実行する（冪等なので最初からやり直しても壊れないが、全 section の traverse を再度払うことになる）。
  - **第 2 のマニュアルソース `ingest_alarmcom`（Issue #8）**: answers.alarm.com（MindTouch KB）を `?mt-language=JA` の機械翻訳で ingest する。クロール対象は sitemap.xml と製品マスタ（Product ノード）駆動で絞る。各製品の型番/別名が「ファミリーハブ URL」に現れる記事ファミリーだけを取り込み、1 製品でもハブ未マッチなら fail closed で止まる（`products.json` の aliases に URL 上の表記を足して再投入する）。robots.txt の Crawl-delay=5 秒を守るため全リクエストを 5 秒以上空けて逐次実行し、**実行時間は対象ファミリー数（≒英日 2 リクエスト × 記事数 × 5 秒）に比例する**。Cloud Run job `ingest-alarmcom` は CI の image 更新対象に含まれる（`.github/workflows/deploy.yml` の `RUN_JOBS`）。**job が未作成のまま main にマージすると `Update Cloud Run job images` ステップが `NOT_FOUND` で失敗し、デプロイ経路全体（service 更新を含む）が止まる。**
- **`merge-schema`（Issue #8 Phase B1）**: vegapunk の `Merge` RPC（Leiden コミュニティ検出 + CommunitySummary + Node2Vec）を schema `urtect` に対して実行し、**前後の `GetStats` と global/hybrid 検索の返却物を JSON で出す**。
- Merge は **schema 全体の同期再計算で、同一 schema では同時 1 本しか走らない**。実行中に再実行すると `FAILED_PRECONDITION` で弾かれる。
- job の `--task-timeout` は CLI の `--timeout-secs`（既定 6h = 21600 秒）より長く取ること。**ちょうど同じ値にすると、CLI の per-request timeout と task-timeout が同着し、JSON summary が出力される前に task が kill される。** 実際の job は 7h（25200 秒）で作成済み。短いと Merge の途中で task が殺され、サーバ側だけ処理が続く状態になる。
- ingest とは独立した job にしてある。`ingest_alarmcom` は実測約 6 時間かかるため、その末尾に Merge を積むと Merge だけの再実行ができない。
- env（fail-closed 境界で2群に分けて扱うこと）:
  - **未設定だと起動に失敗する**: `CS_SUPPORT_PUBLIC_DOMAIN`、`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID`、`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET`、`CS_SUPPORT_LLM_API_KEY`（`config.cloudrun.toml` が `[llm] enabled = true` のため。鍵を解決できないと `server/src/llm.rs` の `AnthropicClient::from_config` で起動時 fail closed）、`CS_SUPPORT_ANSWER_API_KEY`（`config.cloudrun.toml` が `[api] enabled = true` のため。`/{project_id}/api/reply` の Bearer 認証キーを解決できないと `server/src/main.rs` の起動時チェックで fail closed。これは `cs-support-mcp` 本体側の話で、`cs-support-line`（LINE アダプタ）側の必須 env は別立て。下記「LINE アダプタ」節を参照）
  - **未設定でも起動する**: `VEGAPUNK_ENDPOINT`（`config.cloudrun.toml` の `vegapunk_endpoint` キーの値にフォールバック。env があれば `server/src/config.rs` の `AppConfig::load` が上書き）、`VEGAPUNK_BEARER_TOKEN`
  - **未設定でも起動するが、設定しないと接続が切れ続ける**: `CS_SUPPORT_OAUTH_SIGNING_KEY`（OAuth 署名鍵）。未設定なら起動時に CSPRNG で生成し warn する（`server/src/main.rs` の `resolve_signing_key`）。**設定されているが 32 バイト未満の場合は起動時 fail closed**（設定したつもりで脆い鍵を使い続けないため）。
- Secret Manager injection で `cs-support-mcp` 本体に注入するのは **`VEGAPUNK_BEARER_TOKEN` / `CS_SUPPORT_LLM_API_KEY` / `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET` / `CS_SUPPORT_OAUTH_SIGNING_KEY` / `CS_SUPPORT_ANSWER_API_KEY` の 5 つのみ**（真に秘密の値）。`CS_SUPPORT_PUBLIC_DOMAIN` は公開ホスト名、`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` は公開識別子であり、平文 env で構わない。非機密値まで Secret Manager に入れると「どれが本当の秘密か」の判断基準が失われる。`cs-support-line`（LINE アダプタ）側の Secret Manager 注入は別立て。下記「LINE アダプタ」節を参照。
- **署名鍵を設定していれば、再デプロイ・再起動で利用者はログアウトしない。** アクセストークン／リフレッシュトークンは Google 発行の値をそのまま中継しているのでプロセス状態に依存せず、DCR 登録と進行中のログインフローは署名鍵さえ同じなら再起動をまたいで有効。
  - **警告（2026-08 まで実際に起きていた不具合）**: 署名鍵をプロセスごとに生成していた頃は、**デプロイのたび・アイドル明けのコールドスタートのたびに接続が切れていた**。`/oauth/token` の `grant_type=refresh_token` は毎回 `client_id`（署名鍵で封緘した DCR 登録ブロブ）を検証しており、鍵が変わると `unverifiable_client_id` → `invalid_grant` を返す。OAuth クライアントは `invalid_grant` を受けると仕様どおり refresh_token を破棄するため、Google のトークンが有効でも再ログインになる。`minScale` 未設定でゼロスケールするので、**放置しておくだけで切れる**。「Google 発行だから再起動に強い」という以前の説明はこの経路を見落としていた。
- **一括失効は署名鍵のローテーションで行う。** Secret Manager の `CS_SUPPORT_OAUTH_SIGNING_KEY` を差し替えて再デプロイすると、全 DCR 登録と進行中のログインフローが無効になり、全クライアントが再接続を要求される。個別利用者の失効は従来どおり Google 側（アカウントのアクセス権限管理）で行う。
- `[llm] enabled = true` のため、**顧客問い合わせ本文が Anthropic API へ送信される**。運用上の注意点として認識しておくこと。
  さらに `[harness] customer_reply_draft_enabled = true`（デモ用の返信文下書き）のときは、**evaluate 1 回につき Anthropic 呼び出しが 1 回増え、Allowed 時はマニュアル抜粋（最大 2,500 字 × 3 件）または known_resolution の回答本文も送信される**（質問本文 2,000 字と合わせて、**抜粋が上限まで埋まった場合の最大で** 1 下書きあたり概ね 8〜11k トークン。短い記事ではこれを大きく下回る）。切り戻しは `server/config.cloudrun.toml` のこの行を `false` にして再デプロイするだけ。下書きは `egress_gate` を通っており、NG 表現が出た場合は `customer_reply_draft` が `null` になる（理由は warn ログに出る）。
  `VEGAPUNK_BEARER_TOKEN` 未設定時は起動自体は成功するが、vegapunk 呼び出し（`search_manual` 等）だけが
  失敗する（`server/src/main.rs` の `read_bearer_token`。空文字がそのまま使われるため fail-closed にならない点に注意）。
- **`[harness] manual_scoring_v2_enabled = true`（manual 検索スコア v2）**: 型番 run 除外 / TF / 長さ正規化 / 密度 tiebreak の **4 つをまとめて**切り替える kill switch（既定 false、Cloud Run 構成でのみ true）。**切り戻しは `server/config.cloudrun.toml` のこの行を `false` にして再デプロイするだけ**で、スコアも順位も従来へ完全に戻る。
  - 入れた理由: 型番を書いて質問すると、**製品非依存で書かれた正解記事が構造的に減点され**、型番をたまたま含む無関係な長文が上位に来ていた（本番実測。詳細と実測値は `docs/superpowers/specs/2026-08-05-manual-scoring-tf-lengthnorm-design.md`）。順位は `evaluate_answerability` の返信文下書きが使う材料（上位 3 件）を決めるため、**順位汚染はそのまま下書きの品質に出る**。
  - **corpus 全体の recall は未測定**（vegapunk 不介入のため）。保証しているのは実データ 5 記事に対する順位と、スコアが 0〜1 に収まることだけ。vegapunk 復旧後に `verify_alarmcom` で before/after を測り、閾値の妥当性を再確認すること。
  - **既知の限界**: カテゴリのハブページ（`/Partner` 等の目次的な記事）は語彙統計では正解記事と区別できず、上位に残りうる。誤った手順は持ち込まないが、材料の 1 枠を消費する。
- **抜粋の切り詰めは warn に出る**（`server/src/harness/reply.rs` の `truncate_material`）。実データでは記事 5 件中 2 件が `MAX_EXCERPT_CHARS = 2,500` を超え（5,175 字 / 5,477 字）、**半分近くが落ちる**。下書きが「資料に記載がありません」と答えたら、まずこの warn（`route` / `material_id` / `original_chars`）を確認すること。

ビルド & デプロイ:

デプロイは main マージ → CI（`.github/workflows/deploy.yml` の `build-deploy` job）のみで行う。**イメージ tag を更新する目的の手動デプロイ**（`--image` を伴う `gcloud run services update` / `gcloud run jobs update`、および `gcloud builds submit` による手動ビルド・デプロイ）は禁止する。secret 注入・鍵ローテート（`--update-secrets`）、`gcloud run jobs create`、`gcloud run jobs execute` は禁止対象外である。

CI の `build-deploy` job が、`cs-support-mcp` / `cs-support-line` の両 service と、`ingest-products` / `ingest-rules` / `ingest-urtect` / `ingest-alarmcom` / `merge-schema` / `verify-alarmcom` / `backfill-concept-keys` の全 7 job を、同一 tag（コミット SHA、`steps.image.outputs.tag`）へ更新する。全対象が同一 workflow run 内の同一 tag を参照するため、**成功した run の後は** tag が揃う（手動での tag 統一運用は不要）。ただし更新ループの**途中失敗時は部分更新のまま止まる**（先に成功した対象は新 tag、残りは旧 tag。ロールバックはされず、run が赤くなる）。この状態は原因修正後に同 run を re-run するか次のマージで収束させる。jobs → services の順で更新するため、途中失敗時に本番 service が新 tag・jobs が旧 tag という組み合わせにはならない。

`backfill-concept-keys` job は初回のみ `merge-schema` と同じ VPC connector / service account / Secret Manager injection で `gcloud run jobs create` が必要（未作成の場合、CI の `Update Cloud Run job images` ステップが失敗する）。

**CI の更新対象（`.github/workflows/deploy.yml` の `RUN_SERVICES` / `RUN_JOBS`）は、全て事前に Cloud Run 上に作成済みであること。** 1 つでも未作成だと `Update Cloud Run job images` が `NOT_FOUND` で失敗し、その後の service 更新に到達しないためデプロイ経路全体が止まる。対象を増やすときは、先に `gcloud run jobs create` / `gcloud run services create` で実体を作ってから `RUN_JOBS` / `RUN_SERVICES` に追加する。既存の確認は次で行う（1 行で実行すること）:

```sh
gcloud run jobs list --project sivira-cs-support --region asia-northeast1 --format='value(metadata.name)'
gcloud run services list --project sivira-cs-support --region asia-northeast1 --format='value(metadata.name)'
```

#### OAuth 署名鍵（初回のみ）

`CS_SUPPORT_OAUTH_SIGNING_KEY` が未注入だと、デプロイ・コールドスタートのたびに利用者の接続が切れる（上記「警告」を参照）。secret を 1 度だけ作り、service に注入する。

**鍵材料は必ず `openssl rand -base64 32` の出力を使うこと。** 実装の長さ検査（32 バイト）は長さしか見ておらず、覚えやすい 32 文字も通る。鍵が推測されると redirect_uri 許可リストの迂回と、認可コードの復号（＝利用者の Google トークン取得）が両方成立する（下記「blast radius」）。

```sh
openssl rand -base64 32 | gcloud secrets create cs-support-oauth-signing-key --project sivira-cs-support --data-file=-

# **IAM 付与を忘れないこと。** ランタイム SA がこの secret を読めないと revision の起動に
# 失敗し、デプロイごとサービスが落ちる（新 secret を足すときの定番の踏み外し）。
# <runtime-sa> は既存 3 secret と同じ SA。次のコマンドで確認する（1 行で実行すること）:
#   gcloud run services describe cs-support-mcp --project sivira-cs-support --region asia-northeast1 --format='value(spec.template.spec.serviceAccountName)'
gcloud secrets add-iam-policy-binding cs-support-oauth-signing-key --project sivira-cs-support --member serviceAccount:<runtime-sa> --role roles/secretmanager.secretAccessor

# バージョンは :latest ではなく番号で固定する（理由は下記ローテート手順）。
gcloud run services update cs-support-mcp --project sivira-cs-support --region asia-northeast1 --update-secrets CS_SUPPORT_OAUTH_SIGNING_KEY=cs-support-oauth-signing-key:1
```

注入後の 1 回だけは全クライアントが再接続を要求される（鍵が変わるため）。以降は切れない。

**デプロイ後に必ず確認すること**（これをやらないと「直った」と言えない）:

1. ログに `CS_SUPPORT_OAUTH_SIGNING_KEY is not set` の warn が**出ていない**こと。出ていたら注入が効いておらず何も直っていない
2. ログに `google did not return a refresh_token` の warn が**出ていない**こと。出ていたら原因は別で、この修正では解決しない
3. **コールドスタートを 1 回はさんで（15 分以上アイドル → 再アクセス）接続が維持されること。** これが受け入れ基準そのもの

鍵をローテートする（＝全 DCR 登録と進行中ログインフローを一括無効化する）場合:

```sh
openssl rand -base64 32 | gcloud secrets versions add cs-support-oauth-signing-key --project sivira-cs-support --data-file=-

# **バージョンを番号で指定する。** `:latest` のままだと service spec が変化せず
# `gcloud run services update` が no-op になり、新 revision が作られない。Cloud Run が
# `:latest` を解決するのはインスタンス起動時なので、鍵が実際に切り替わるのは「たまたま
# 次にコールドスタートしたとき」になり、失効したつもりで失効していない状態が生まれる。
gcloud run services update cs-support-mcp --project sivira-cs-support --region asia-northeast1 --update-secrets CS_SUPPORT_OAUTH_SIGNING_KEY=cs-support-oauth-signing-key:<new-version>
```

**この secret の blast radius（受容したリスクとして記録）**: `authorize` は署名済み `Blob::Client` 内の `redirect_uris` へのメンバシップ照合しか行わず、`is_acceptable_redirect_uri`（claude.ai へのホスト完全一致）を再適用しない。したがって**この鍵を読める者は任意の redirect_uri を持つ client_id を自分で鋳造でき、許可リストを完全に迂回できる**。`/oauth/callback` が封緘済み認可コードを攻撃者ホストへ配送し、同じ鍵から派生した AEAD 鍵で復号すれば、ログインした利用者の Google access_token / refresh_token が手に入る。

- 鍵をプロセスメモリのみに置いていた頃は、奪取に稼働中コンテナへの侵入が必要だった。**永続化により、Secret Manager・Cloud Run の env 設定・`gcloud secrets create` を叩いた端末のシェル履歴に残る長寿命の値になった**
- したがって `roles/secretmanager.secretAccessor` は**この secret 単体に対してランタイム SA のみ**へ付与する（プロジェクト全体付与にしない）
- 恒久対処は `authorize` とリダイレクト直前で `is_acceptable_redirect_uri` を**再適用**すること。鍵漏洩だけでは confused deputy が成立しなくなる（別途対応）

### 認証（OAuth 2.1 フェデレーション、実測済み）

> **AuthN は機能しているが AuthZ は実質無い。** 以下は「認証が正しく動いている」証跡であって、
> 認可が効いていることの証跡ではない。同意画面は External（本番公開）で、認証を通した任意の
> Google アカウントが supervisor 全権を得る。デプロイや公開範囲を触る前に、上記
> 「Project Routing and Auth」節の警告2点を必ず読むこと。

静的 Bearer token・静的 JWT は撤去済み。`cs-support-mcp` は OAuth 2.1 の**認可サーバ兼リソースサーバ**として動作し、内部で Google（`accounts.google.com`）へ委譲する。

- 無トークン `POST /urtect/mcp` → `401` + `WWW-Authenticate: Bearer resource_metadata="https://cs-support-mcp-235108918288.asia-northeast1.run.app/.well-known/oauth-protected-resource/urtect/mcp"`
- `GET /.well-known/oauth-protected-resource/urtect/mcp` → `200 application/json`。`authorization_servers` は Google ではなく**このサービス自身**:
  ```json
  {"resource":"https://cs-support-mcp-235108918288.asia-northeast1.run.app/urtect/mcp","authorization_servers":["https://cs-support-mcp-235108918288.asia-northeast1.run.app"]}
  ```
- `GET /.well-known/oauth-authorization-server` → **200 が正常**（RFC 8414 AS メタデータ）。旧構成では 404 が正常だったが、DCR 非対応の Google を AS にすると claude.ai が接続できないため、このサービス自身が AS になった。**404 が返るなら旧イメージが動いている**と疑うこと。
  ```json
  {"issuer":"https://cs-support-mcp-235108918288.asia-northeast1.run.app","authorization_endpoint":"…/oauth/authorize","token_endpoint":"…/oauth/token","registration_endpoint":"…/oauth/register","response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none"]}
  ```
- `GET /livez` → `200`
- Google Cloud Console の OAuth クライアントには、承認済みリダイレクト URI として `https://cs-support-mcp-235108918288.asia-northeast1.run.app/oauth/callback` を**必ず登録する**（未登録だとログインが `redirect_uri_mismatch` で必ず失敗する）。claude.ai 側の redirect URI を Google に登録する必要はもう無い。

### 確認コマンド

```sh
curl -sS https://cs-support-mcp-235108918288.asia-northeast1.run.app/livez

curl -sS -o /dev/null -w '%{http_code}\n' -X POST https://cs-support-mcp-235108918288.asia-northeast1.run.app/urtect/mcp

curl -sS https://cs-support-mcp-235108918288.asia-northeast1.run.app/.well-known/oauth-protected-resource/urtect/mcp

# 200 が正常（旧構成では 404 が正常だった。404 なら旧イメージを疑う）
curl -sS https://cs-support-mcp-235108918288.asia-northeast1.run.app/.well-known/oauth-authorization-server

# DCR が動くことの確認（201 + client_id が返る。client_secret は返らないのが正しい）
curl -sS -X POST -H 'Content-Type: application/json' \
  -d '{"redirect_uris":["https://claude.ai/api/mcp/auth_callback"],"client_name":"probe"}' \
  https://cs-support-mcp-235108918288.asia-northeast1.run.app/oauth/register
```

MCP tool 呼び出しは claude.ai のカスタムコネクタ経由で行う。**URL = 上記 MCP endpoint を入力するだけでよい**（Client ID / Secret の手入力は不要になった。claude.ai が DCR で自動登録する）。E2E のクライアントは claude.ai。

### LINE アダプタ（`cs-support-line` service）

`cs-support-mcp` 本体が内蔵する `POST /{project_id}/api/reply` を LINE から呼べるようにする webhook アダプタを、**同一イメージの別 Cloud Run service**として運用する。判定・応答文生成のロジックは一切持たず、署名検証・応答生成 API への 1 コール・LINE への返信だけを行う薄いアダプタ（`server/src/bin/line_adapter.rs`）。契約の正本は `docs/superpowers/specs/2026-08-11-answer-api-line-adapter-design.md` §6・§7（ここには複製しない）。

- service: `cs-support-line`。`cs-support-mcp` と同一イメージを使い、起動コマンドだけ `/usr/local/bin/line_adapter` に上書きする（`Dockerfile` は両バイナリを同梱済み）
- **`--max-instances=1` 必須**。セッションストア（userId → case_id / 履歴）がプロセス内メモリのため、複数インスタンスに分散すると同一ユーザーの会話が非決定的に分裂する。スケールが必要になったらセッション永続化（design doc §9 の次フェーズ）を先に実装する
- ingress: 公開（LINE Platform からの webhook を受けるため）。**VPC connector 不要**（vegapunk への直接到達が要らず、応答生成 API へは `cs-support-mcp` の公開 URL 経由で到達するため）
- 新規 Secret Manager 3 件（既存の `openssl rand -base64 32` 相当以上の強度、または LINE Developers console 発行値をそのまま使う）:
  - `cs-support-answer-api-key` → `cs-support-mcp` 本体の `CS_SUPPORT_ANSWER_API_KEY`（上記「未設定だと起動に失敗する」参照）と、`cs-support-line` の `CS_ANSWER_API_KEY` の**両方**に同じ値を注入する（応答生成 API 側は「この鍵を提示したリクエストを受理する」、LINE アダプタ側は「この鍵を `Authorization: Bearer` として送る」で対になっている必要がある）
  - `line-channel-secret` → `cs-support-line` の `LINE_CHANNEL_SECRET`（webhook 署名検証。LINE Developers console で発行）
  - `line-channel-access-token` → `cs-support-line` の `LINE_CHANNEL_ACCESS_TOKEN`（LINE Reply API 呼び出し。LINE Developers console で発行）
- `cs-support-line` の必須 env（未設定・空文字は起動失敗。design doc §6）: `LINE_CHANNEL_SECRET` / `LINE_CHANNEL_ACCESS_TOKEN` / `CS_ANSWER_API_URL`（`https://cs-support-mcp-235108918288.asia-northeast1.run.app/urtect/api/reply`）/ `CS_ANSWER_API_KEY`。任意 env: `CS_LINE_FALLBACK_TEXT` / `CS_LINE_NONTEXT_TEXT`（既定値は design doc §6）
- LINE Developers console の webhook URL に `https://<cs-support-line の URL>/line/webhook` を設定する

## 旧構成（参考、Cloud Run へ移行済み）

本番は GCE VM `llm-memory` 上の既存 `llm-memory-extention` stack（Caddy 同居、`cs-support-136-110-78-245.nip.io`）から、上記 Cloud Run 構成へ移行済み。GCE 版のサービス定義・Caddyfile 差分・デプロイ手順の詳細は git 履歴（このファイルの旧版）を参照すること。VM `llm-memory` 自体は他サービスと共用で存在し続けているが、`cs-support-mcp` はもう乗っていない。
