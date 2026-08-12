# 製品マスタの vegapunk 正本化（KNOWN_MODELS 廃止）

GitHub Issue: #6

## 背景 / 問題

`server/src/bin/ingest_urtect.rs:42` の `const KNOWN_MODELS: &[&str] = &["ADC-V724", "ADC-V724X", "ADC-VC727P"]` が2つの役割を兼ねている。

1. マニュアル本文からの型番検出（`detect_product_models`）の語彙
2. Product ノード生成の**マスタそのもの**（`ingest_urtect.rs:702-711` で無条件に 3 件 upsert）

製品マスタがコードに埋まっているため、製品を 1 件追加するたびにコード修正 → ビルド → イメージ再ビルド → 再デプロイが必要になる。これを廃止し、**vegapunk の Product ノードを製品マスタの唯一の正本**にする。

## 設計

### 1. 新規 CLI `server/src/bin/ingest_products.rs`

製品マスタを vegapunk に投入する専用 CLI。全体リセット後の seed 投入と、製品追加の両方に使う。

CLI 引数（`ingest_urtect.rs:261-294` の `Args` と同じ流儀。clap derive）:

- `--endpoint`（env `VEGAPUNK_ENDPOINT`、既定 `http://vegapunk.local:6840`）
- `--schema`（既定 `"urtect"`）
- `--token-env`（既定 `"VEGAPUNK_BEARER_TOKEN"`）
- `--token-file`（env `VEGAPUNK_BEARER_TOKEN_FILE`、既定 `/private/tmp/vegapunk-bearer-token`）
- `--schema-file`（既定 `../schema/cs-support.yml`）。`ingest_urtect` と同様に schema 登録（`create_or_update_schema` 相当）を最初に行う。リセット直後は本 CLI が最初に走るため、schema 登録をここでも担保する
- `--products-file`（既定 `data/urtect/products.json`）
- `--no-vectors`（フラグ。embed / `upsert_vectors` をスキップする明示的 opt-out）

処理:

1. `--products-file` の JSON を読む。形式は配列:
   ```json
   [
     { "model": "ADC-V724", "name": "ADC-V724", "aliases": [] }
   ]
   ```
   - `model` 空文字は即エラー。`model` の重複（大文字小文字無視）は即エラー
   - パース失敗・検証失敗は fail closed（何も upsert せず終了。エラーには対象ファイルパスと原因を含める）
2. 各エントリを `ManualProductInput { model, name, aliases }` に写像し、`build_product_node`（`server/src/manual/ingest_model.rs:47`）で Product ノードを構築する。node id 規約（`{schema}:gen1:Product:{model}`）・attributes（`product_key` / `name` / `model` / `aliases` カンマ結合）は既存関数をそのまま使い、変更しない
3. embed → `upsert_vectors` → `upsert_nodes` の順で投入する（`ingest_urtect` の「vector 先行」の順序に合わせる）。
   - embed テキストは現在 `ingest_urtect` が product embed で使っている組み立てと同一にする。共通化できるなら `server/src/manual/` 側に小さな helper として括り出してよい（`ingest_urtect` 側も同 helper を使う。ただし今回 `ingest_urtect` からは product embed 自体を削除するので、実際の利用者は `ingest_products` のみになる）
   - vector metadata は既存契約に従う: 認識キーは `node_id` / `text` / `source_type` / `timestamp_ms` の 4 つのみ、`node_id` は entry `id`（graph node_id）と同一文字列（`ingest_urtect.rs:135-146` のコメント参照。`vector_metadata` helper を再利用してよい）
   - embed 同時実行は `EMBED_CONCURRENCY = 4` と同じ値。1 件でも embed 失敗したら fail closed（中途半端な状態を作らない）
4. 完了時に投入件数・スキップ有無をログ出力する

### 2. `server/data/urtect/products.json`（新規）

現行 3 型番の seed 記録。**正本は vegapunk であり、このファイルは投入入力の版管理記録**である旨をファイル冒頭コメント…は JSON なので書けないため、本 spec と CLAUDE.md 追記（後述）で明示する。

内容: `ADC-V724` / `ADC-V724X` / `ADC-VC727P` の 3 件。現行挙動と同じく `name` = `model`、`aliases` = 空配列。

### 3. `server/src/bin/ingest_urtect.rs` の変更

- `const KNOWN_MODELS`（`:42`）を削除
- Product ノード生成ループ（`:702-711`）と product embed を削除（`ingest_products` の責務に移動）
- ingest 開始時（クロール前）に vegapunk から製品一覧を取得する:
  - `client.query_nodes(&args.schema, "Product", Vec::new(), 1000)`（`server/src/vegapunk.rs:293` の既存メソッド）
  - **0 件なら bail**: エラーメッセージに「製品マスタが空。先に `ingest_products` を実行せよ」という運用者向け次アクションを含める（fail closed）
  - **返却件数が limit と同数なら bail**: 取りこぼし（silent truncation）の可能性があるため。メッセージに limit 値を含める
- 検出語彙の構築: 各 Product ノードの `model` 属性と `aliases` 属性（カンマ区切りを split・trim・空要素除去）を集め、`(表層形, product model)` の対応表を作る。表層形は大文字化して保持する
- `detect_product_models(body)` を「ハードコード一覧と照合」から「上記対応表と照合」に変更する。**英数字境界チェック（`contains_as_token`）の意味論は一切変えない**（`ADC-V724` が `ADC-V724X` の内部一致で誤爆しない性質を維持）。ヒットした表層形は product model（`product_key`）に解決して返す。同一 product が model と alias の両方でヒットしても 1 回だけ返す
- `DESCRIBES` 辺の構築ロジック自体は変更しない（検出結果の由来が変わるだけ）
- `:34-41` の KNOWN_MODELS 説明コメントを現状に合わせて書き直す。lexicon（`model_adc_*` signal）との二重管理が残っている点は Issue #7 への参照として残す

### 4. `Dockerfile` の変更

builder の `cargo build --bin` 列と runtime stage の `COPY` に `ingest_products` を追加する（将来 Cloud Run job 化できるように。job の新設自体は今回のスコープ外）。

### 5. `CLAUDE.md`（リポジトリ側）の追記

「Cloud Run デプロイ手順」節の ingest 関連記述の近くに、以下を簡潔に追記する:

- 製品マスタの正本は vegapunk の Product ノード。`server/data/urtect/products.json` は seed 投入の入力記録
- 製品追加の手順: `products.json` に追記 → `ingest_products` 実行。**サービスの再ビルド・再デプロイは不要**
- ingest の実行順序: 全体リセット後は `ingest_products` → `ingest_urtect` の順（`ingest_urtect` は製品マスタが空だと fail する）

## テスト

- `detect_product_models`（新シグネチャ）の単体テスト:
  - model 直接一致
  - alias 経由の一致が product model に解決される
  - 境界チェック: `ADC-V724X` を含む本文で `ADC-V724` が誤検出されない（既存テストがあれば新シグネチャに移植）
  - 大文字小文字・model と alias の重複ヒットの重複排除
- products.json のパース・検証の単体テスト: 正常系 / `model` 空 / `model` 重複 / 不正 JSON
- 既存テスト（`cargo test`）が全部通ること

## 検証（evidence として最終出力に含める）

- `cargo fmt --manifest-path server/Cargo.toml -- --check`
- `cargo check --manifest-path server/Cargo.toml --all-targets`
- `cargo test --manifest-path server/Cargo.toml`
- 実 vegapunk への E2E（`ingest_products` → `ingest_urtect` 再実行）はネットワークが必要なためコンテナ内では実行しない。未実行である旨を最終出力に明記する（ホスト側で別途実施する）

## スコープ外（先回りして作らない）

- signal-lexicon の `model_adc_*` 二重管理解消（Issue #7）
- answers.alarm.com の ingest（Issue #8）
- 実行時サーバ側（`server/src/manual/retrieval.rs` 等）の変更。`resolve_product` の limit 1000 ハードコードもここでは触らない
- `ingest_products` の Cloud Run job 新設
- products.json への説明・競合製品などの属性追加

## 制約

- Python / TypeScript を使わない
- エラーを握りつぶさない。全エラーパスに運用者が次アクションを判断できる情報を含める
- 既存の構成・命名・型に合わせ最小差分で変更する
- Conventional Commits。commit は可、push・PR 作成は禁止
- spec にない非自明な判断が必要になったら実装せず差し戻す
