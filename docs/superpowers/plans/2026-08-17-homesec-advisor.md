# ホームセキュリティアドバイザ AI(デモ)実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 別 LINE OA から使えるホームセキュリティアドバイザ(接地 2 層・営業リード獲得・製品カルーセル付き)を、現行 CS を無変更のまま同一 crate の新バイナリとして実装する。

**Architecture:** 仕様の正本は `docs/superpowers/specs/2026-08-17-homesec-advisor-design.md`(以下「spec」)。会話体験の期待値は `2026-08-17-homesec-advisor-dialogue-examples.md`。新規コードは `server/src/advisor/` モジュールと `server/src/bin/homesec_advisor.rs` に閉じ、共有モジュール(harness / vegapunk / oauth / admin / llm)は原則無変更で流用する。

**Tech Stack:** Rust(axum + rmcp 既存構成)、vegapunk gRPC、Anthropic Messages API、LINE Messaging API(carousel template)。

## Global Constraints

- Python 禁止・TypeScript は `admin-ui/` のみ(CLAUDE.md)
- 既存(urtect)テストは無変更で PASS すること(spec 不変条件 1)。共有ファイルへの変更は「加算 + 静的配信関数の共有化(Task 6)」のみ
- advisor は MCP endpoint・OAuth AS・署名鍵を持たない(spec 不変条件 2)
- コミットは Conventional Commits + `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
- 検証コマンド: `cargo test --manifest-path server/Cargo.toml`、`cargo fmt --manifest-path server/Cargo.toml -- --check`(cd 禁止)
- push / PR 作成 / デプロイは行わない(ユーザーと Fable の作業)

## 主要な既存インターフェース(実装時にこのまま使う)

- `api.rs:212 pub fn authorize(headers: &HeaderMap, api_key: &str) -> bool`(Bearer 定数時間比較)
- `api.rs:32 ReplyRequest { message, history, case_id, end_user_id }` / `api.rs:60 ReplyResponse { reply_text, case_id }` / `api.rs:46 HistoryEntry { role, text }`
- `knowledge.rs:721 record_conversation_turn(&self, schema, case_id, end_user_id, question, reply_text, reply_kind: &str, audit_event_id) -> Result<()>`(reply_kind は自由文字列 = advisor 値をそのまま渡せる)
- `knowledge.rs:876 load_case(&self, schema, case_id) -> Result<Option<HashMap<String,String>>>` / `mod.rs:591 save_conv_state(...)` / `knowledge.rs:642 record(&self, schema, node_type, key, attributes) -> Result<()>`(UpsertNodes 全置換 = 全属性再送必須)
- `time_pref.rs:52 handle_time_pref(&TimePrefExtraction, &mut CaseConvState, &BusinessHoursConfig) -> TimePrefAction` / `time_pref.rs:270 extract_time_preference(...)` / 受付 ON は `api.rs:289 arm_time_pref_solicitation` と同じ属性操作(`awaiting_time_pref=true, time_pref_false_count=0`)を advisor 側で行う
- `hours.rs:65 is_within_business_hours` / `hours.rs:101 business_hours_label` / `hours.rs:116 overlaps_business_hours`
- `egress.rs:8 NgDictionary::from_path` / `egress.rs:73 egress_gate` / `prompt_input.rs:135 apply_draft_gate_or_fallback` / `prompt_input.rs:224 to_plain_text` / 定数 `CONTINUATION_OPENER_RULE` `MARKDOWN_BAN_RULE`(すべて同一 crate 内 `pub(crate)` なので advisor から使用可)
- `llm.rs:169 draft_reply(&self, system_prompt, user_message, max_tokens, route) -> Result<ReplyDraft>`(ReplyDraft { text, truncated }。リトライ無し = advisor 側で 1 回再試行を実装)
- `vegapunk.rs:652 search(&self, schema, query, top_k) -> Result<Vec<SearchResultItem>>` / `vegapunk.rs:171 create_or_update_schema` / `vegapunk.rs:243 upsert_graph_low_level` / `vegapunk.rs:357 query_nodes`
- `admin.rs:31 AdminState { schema, manual_schema, harness }` / `admin.rs:54 admin_router(AdminState) -> Router`(認証は呼び出し側で `require_google_auth` を layer)
- `middleware.rs:55 require_google_auth` + `middleware.rs:34 AuthState { verifier, resource_metadata_url }` + `verifier.rs:238 GoogleTokenVerifier::new(client_id)`(AS 非依存を確認済み)
- `product_gate.rs:129 extract_model_tokens(text) -> Vec<String>` / `ProductAllowlist::from_models` / `is_in_scope`
- `rules.rs:147 match_known_resolution(&[KnownResolution], &SignalSet) -> KrMatch`
- ingest 雛形: `bin/ingest_rules.rs`(`with_schema_name` → `create_or_update_schema` → `upsert_graph_low_level`、ID は `harness_node_id(schema, node_type, key)`)
- SPA 静的配信: `main.rs:384 admin_static_router` / `main.rs:350 resolve_admin_static_dir`(private のため Task 6 で共有モジュールへ移動)

---

### Task 1: schema・config・ビルド配線

**Files:**
- Create: `schema/homesec.yml`
- Create: `server/config.homesec.toml`
- Modify: `server/Cargo.toml`(`[[bin]] homesec_advisor` 追記)
- Modify: `Dockerfile:54-65`(`--bin homesec_advisor` 追加)、`Dockerfile:70-80`(COPY 追加)
- Modify: `schema/cs-schema.yml` / `schema/cs-support.yml`(support_case へ `lead_offered` / `lead_requested` / `shown_product_cards` を加算宣言 — 一致テストが全 schema ファイルを対象にするため)
- Test: `server/src/harness/knowledge.rs` の既存一致テスト方式を homesec.yml にも適用

**Interfaces:**
- Produces: schema `homesec.yml` = `advisor_material`(spec §5.1 の全属性)+ `support_case` + `ConversationTurn` + `KnownResolution` + `Signal`(cs-support.yml の宣言を写し、support_case に上記 3 属性を加算)。edge: `HAS_TURN` のみ
- Produces: `config.homesec.toml` = cloudrun 版の写しから `[[projects]] project_id="homesec" schema="homesec" manual_schema="manual_v1"`、`[advisor]` 新セクション(`handoff_contact_text`、`images_dir = "data/homesec/images"`、`ng_dictionary_path = "data/homesec/ng.json"`)。`[api] enabled=true`、`[llm] enabled=true`
- Produces: `config.rs` に `pub struct AdvisorConfig`(上記 3 キー、`Option<AdvisorConfig>` で AppConfig へ加算 — CS の config には影響しない)

**Steps:**
- [x] `homesec.yml` を書く(cs-support.yml の該当ノード型を複製し advisor_material を追加)
- [x] `written_support_case_attribute_keys()` に 3 属性を加算し、既存 2 テスト(cs-schema / cs-support)が新属性宣言込みで PASS することを確認。同方式で `homesec_yml_declares_...` テストを追加
- [x] `AdvisorConfig` を追加し、`config.homesec.toml` のロードテスト(3 キーが読めること・既存 config で None のこと)を書く → 実装 → PASS
- [x] Cargo.toml / Dockerfile 配線(この時点で bin は `fn main() {}` の空実装で cargo build を通す)
- [x] `cargo test` / `cargo fmt --check` → commit `feat(advisor): schema, config, and build wiring (#34)`

### Task 2: ingest_homesec CLI と materials.json の器

**Files:**
- Create: `server/src/bin/ingest_homesec.rs`
- Create: `server/data/homesec/materials.json`(seed 数件。本番分は Task 8)
- Create: `server/data/homesec/ng.json`(spec §2.1 企業定型句 + §2.2 保証表現・資格作業語)
- Test: バリデーションと GraphBuild 組み立ての単体テスト(bin 内 `#[cfg(test)]`、ingest_rules.rs の流儀)

**Interfaces:**
- Consumes: `with_schema_name` / `create_or_update_schema` / `upsert_graph_low_level` / `harness_node_id`
- Produces: `materials.json` の型 = spec §5.1 属性そのまま(serde struct `MaterialEntry`)。バリデーション: `statistic`・`partner_product` は `source_url` 必須 / `material_key` は `{kind}:{slug}` 形式 / `card_description` があれば card_match_terms 省略時照合語(`title_ja`, `product_key`)が非空

**Steps:**
- [x] `MaterialEntry` の parse + バリデーション失敗ケースのテストを書く → 実装 → PASS
- [x] GraphBuild 組み立て(advisor_material ノード、edge 無し)のテスト → 実装 → PASS
- [x] main: `--config` / `--materials-file` / `--validate-only`(投入せず検証のみ)引数、schema 登録 → 冪等 upsert。同一入力 2 回で同一 GraphBuild になるテスト
- [x] `cargo test` / fmt → commit `feat(advisor): homesec ingest cli and seed materials (#34)`

### Task 3: 理解(LLM Call #1)と条件語彙

**Files:**
- Create: `server/src/advisor/mod.rs`(モジュール宣言)、`server/src/advisor/understand.rs`
- Modify: `server/src/llm.rs`(汎用テキスト補完 `pub(crate) async fn complete_text(&self, system_prompt: &str, user_message: &str, max_tokens: u32, route: &str) -> Result<String>` を draft_reply と同じ経路で追加 — 既存メソッドは無変更)
- Modify: `server/src/main.rs` 側は触らない(`advisor` モジュールは `lib.rs`/`main.rs` のモジュールツリーに `pub mod advisor;` として追加)

**Interfaces:**
- Produces: `pub struct Understanding { pub in_domain: bool, pub emergency: bool, pub urtect_support: bool, pub lead_interest: bool, pub summary_ja: String, pub conditions: Vec<(ConditionKey, String)> }`
- Produces: `pub enum ConditionKey { Housing, Target, Concern, Budget, Install }` + `pub fn normalize_condition(key: &str, value: &str) -> Option<(ConditionKey, String)>`(spec §4.2 の語彙表。語彙外は None + warn)
- Produces: `pub async fn understand(llm: &AnthropicClient, message: &str, history_digest: &str, accumulated: &str) -> Result<Understanding>`(JSON 出力プロンプト + serde parse。失敗時 1 回だけ再試行、再失敗は Err)

**Steps:**
- [x] 語彙正規化のテスト(全 key×代表値、語彙外破棄)→ 実装 → PASS
- [x] JSON parse のテスト(正常 / フィールド欠落 / 非 JSON)→ 実装 → PASS
- [x] プロンプト組み立てのスナップショット的テスト(emergency・urtect_support・lead_interest の定義文言が spec §4.1 と一致)
- [x] `cargo test` / fmt → commit `feat(advisor): understanding call and condition vocabulary (#34)`

### Task 4: 応答種別決定・定型文・リードフロー

**Files:**
- Create: `server/src/advisor/decide.rs`、`server/src/advisor/canned.rs`(safety / out_of_domain / handoff / time_pref 依頼 / lead 確定 / fallback の定型文。文言は dialogue-examples の該当応答に合わせる)
- Test: decide.rs 内の全分岐テスト

**Interfaces:**
- Consumes: `CaseConvState`(clarify_turns / awaiting_time_pref / time_pref_false_count / preferred_contact_time)、`handle_time_pref` / `extract_time_preference`、`hours::*`、`AdvisorConfig.handoff_contact_text`
- Produces: `pub enum AdvisorAction { Safety, TimePrefContinue, OutOfDomain, Handoff, LeadSolicit, Clarify { missing: Vec<ConditionKey> }, Answer, LeadConfirmed { slot: String } }`
- Produces: `pub fn decide(u: &Understanding, conv: &CaseConvState, lead_offered: bool, lead_requested: bool, clarify_max: u32) -> AdvisorAction`(spec §4.3 の優先順そのまま。純関数)
- Produces: case 属性 3 つの read/merge ヘルパ `pub fn parse_advisor_case_attrs(&HashMap<String,String>) -> AdvisorCaseAttrs` / `pub fn advisor_attr_updates(...) -> Vec<(String,String)>`

**Steps:**
- [x] decide の全分岐テスト(優先順の交差ケース: emergency×lead_interest、awaiting_time_pref×out_of_domain 等)→ 実装 → PASS
- [x] 定型文が NG 辞書(企業定型句)に抵触しないテスト
- [x] `cargo test` / fmt → commit `feat(advisor): reply decision, canned replies, and lead flow (#34)`

### Task 5: 材料検索・KR 照合・下書き生成・出口関門・カード

**Files:**
- Create: `server/src/advisor/materials.rs`(検索 + NodeResult → `AdvisorMaterial` struct 変換 + KR 照合)、`server/src/advisor/draftgen.rs`(プロンプト + draft_reply + 関門)、`server/src/advisor/cards.rs`
- Test: 各ファイルの単体テスト

**Interfaces:**
- Produces: `pub struct AdvisorMaterial { material_key, kind, title_ja, body_ja, source_url, category, product_key, price_band, card_description, card_match_terms }`(query_nodes/search の属性 HashMap から parse)
- Produces: `pub async fn gather_materials(vp: &VegapunkClient, schema: &str, query: &str, conditions: &[(ConditionKey,String)]) -> Vec<AdvisorMaterial>`(search top_k=5。失敗は空 vec + warn = spec §8)
- Produces: `pub fn build_advisor_system_prompt(...)` / `pub async fn draft_advisor_reply(...) -> String`(接地 2 層規則・ペルソナ・URTECT 優遇・リード提案規則(lead_offered で抑止)・安全下限・`MARKDOWN_BAN_RULE`・`CONTINUATION_OPENER_RULE` を注入。`apply_draft_gate_or_fallback` 通過後に追加関門)
- Produces: `pub fn url_allowlist_gate(text: &str, allowed: &HashSet<String>) -> bool` / `pub fn model_allowlist_gate(text: &str, allow: &ProductAllowlist) -> bool`(extract_model_tokens 流用)
- Produces: `pub struct ProductCard { material_key, title, description, image_url: Option<String>, button_text, button_message }` / `pub fn select_cards(final_text: &str, injected: &[AdvisorMaterial], shown_csv: &str, images_dir_public: &str) -> Vec<ProductCard>`(spec §7.2: card_description 保有材料のみ、match_terms 照合、own 優先、3 件上限、画像は存在するファイルのみ URL 化)

**Steps:**
- [x] AdvisorMaterial parse テスト(必須欠落・CSV match_terms)→ 実装 → PASS
- [x] URL / 型番 / NG 各関門の違反 → fallback 差し替えテスト → 実装 → PASS
- [x] select_cards テスト(合致 / 非合致 / own 優先 / 再表示抑止 / 画像なし)→ 実装 → PASS
- [x] KR 照合: conditions を SignalSet に写して `match_known_resolution` を呼び、ヒット時は材料先頭に注入するテスト → 実装 → PASS
- [x] 検索非汚染: gather_materials が `advisor_material` / KR 以外(ConversationTurn / support_case)を材料として返さないことの固定テスト(spec 不変条件 6)
- [x] `cargo test` / fmt → commit `feat(advisor): materials, grounded drafting, gates, and cards (#34)`

### Task 6: homesec_advisor バイナリと /homesec/api/reply

**Files:**
- Create: `server/src/advisor/api.rs`(handler とレスポンス型)、`server/src/bin/homesec_advisor.rs`(本実装)
- Modify: `server/src/main.rs` + Create `server/src/staticui.rs`(`admin_static_router` / `resolve_admin_static_dir` を staticui.rs へ移動し main.rs から呼ぶ — 挙動無変更のリファクタ。既存テスト無変更 PASS が条件)
- Test: handler の統合テスト(認可 401 / product_cards 付き 200 / ターン永続化はスタブ)

**Interfaces:**
- Produces: `pub struct AdvisorReplyResponse { reply_text: String, case_id: String, #[serde(skip_serializing_if = "Option::is_none")] product_cards: Option<Vec<ProductCard>> }`(既存 ReplyResponse と同形 + 加算)
- Produces: handler フロー = spec §6 の 11 手順(authorize → project 解決 → understand → decide → gather → draft → 関門 → cards → save_conv_state + record_conversation_turn(5 秒上限、応答優先)→ 返却)
- Produces: bin main = config ロード(`--config config.homesec.toml`)、起動 fail-closed(`CS_SUPPORT_ANSWER_API_KEY` / `CS_SUPPORT_PUBLIC_DOMAIN` / `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` / fallback 文言 / business hours 検証。**署名鍵と CLIENT_SECRET は要求しない**)、router = healthz/livez + advisor api + `/{project}/admin/api`(admin_router + require_google_auth)+ `/admin`(staticui)+ `/static/products`(ServeDir on `AdvisorConfig.images_dir`)

**Steps:**
- [x] staticui.rs 抽出 → 既存テスト無変更 PASS 確認 → commit `refactor: extract static ui router for reuse (#34)`
- [x] handler テスト(401 / 400 / 200 + cards)→ 実装 → PASS
- [x] bin main の起動チェックテスト(env 欠落で Err)→ 実装 → PASS
- [x] `cargo test` / fmt → commit `feat(advisor): homesec advisor service binary (#34)`

### Task 7: line_adapter のカルーセル描画 + admin-ui バッジ

**Files:**
- Modify: `server/src/bin/line_adapter.rs`(`AnswerApiResponse` に `product_cards: Option<Vec<CardPayload>>` 加算、`send_line_carousel` 追加、`send_line_reply` 成功後に非空なら送信)
- Modify: `admin-ui/` の reply_kind バッジ定義(advisor 6 値の表示ラベルと色を追加)
- Test: line_adapter 内の payload 組み立てテスト(画像なし列 / 3 件 / 省略時従来挙動)、admin-ui 既存テストの更新

**Interfaces:**
- Consumes: LINE carousel template(`template.type = "carousel"`、column: `thumbnailImageUrl?`(任意)/ `title`(40 字上限)/ `text`(60 字上限、画像なし時 120 字)/ `actions: [{type:"message", label, text}]`)。上限超過は切り詰め
- Produces: 契約 = spec §3.3 の `product_cards`。フィールド欠落(CS 経路)は完全に従来挙動

**Steps:**
- [x] parse テスト(cards あり / なし)→ 実装 → PASS
- [x] carousel payload テスト(文字数切り詰め・画像なし)→ 実装 → PASS
- [x] 既存 line_adapter テストが無変更 PASS であること確認
- [x] admin-ui: バッジ追加 + `npm --prefix admin-ui run build` 成功確認
- [x] `cargo test` / fmt → commit `feat(advisor): line carousel rendering and admin badges (#34)`

### Task 8: データキュレーション(materials.json 本番分)

**Files:**
- Modify: `server/data/homesec/materials.json`(statistic 20〜30 / own_product 7 / partner_product 12+ / scenario 5)
- Modify: `docs/superpowers/specs/2026-08-17-homesec-advisor-dialogue-examples.md`(仮置き統計値を投入値と一致させる)

**Steps:**
- [ ] statistic: 警察庁「住まいる防犯110番」等の公開ページを WebFetch で実照会し、数値と `source_url` を突合してから記載する(**未照会の数値・URL を書くことを禁止**。照会できなかった項目は入れない)
- [ ] own_product: 既存 `server/data/urtect/products.json` と urtect マニュアルの記載範囲で提案文・card_description を書く(仕様の創作禁止。根拠が無い属性は書かない)
- [ ] partner_product: カテゴリ主体(価格は価格帯)。`source_url` は公式サイトを WebFetch で存在確認
- [ ] `ingest_homesec` のバリデーションが全件 PASS すること(`cargo run --bin ingest_homesec -- --validate-only` 相当のモードを Task 2 に含めておく)
- [ ] commit `feat(advisor): curated homesec materials (#34)`

### Task 9: CI 配線と運用ドキュメント

**Files:**
- Modify: `.github/workflows/deploy.yml:48-49`(`RUN_SERVICES` に `homesec-advisor homesec-line`、`RUN_JOBS` に `ingest-homesec` を追加)
- Modify: `CLAUDE.md`(homesec 構成・secret・「service/job を gcloud create してから CI に足す」手順・製品画像の置き場)

**Steps:**
- [ ] deploy.yml 変更(github-actions-optimize スキルの規約に従う。ループ構造は既存のまま)
- [ ] CLAUDE.md へ運用手順を追記
- [ ] commit `ci(advisor): wire homesec services and job into deploy (#34)`
- [ ] **注意**: この PR のマージは、Fable が `gcloud run services create` / `jobs create` で実体を作った後(未作成のままマージすると CI が NOT_FOUND で全停止)

## タスク順序と依存

1 → 2 → 3 → 4 → 5 → 6(1〜5 に依存)→ 7(6 の契約に依存)→ 8(2 に依存、3〜7 と並行可)→ 9(最後)

## 受け入れ確認(実装完了後、Fable + ユーザー)

- `cargo test` 全 PASS(既存テスト無変更)+ fmt クリーン
- reviewer(opus)PASS / CONDITIONAL PASS
- デプロイ後 E2E(spec §11): 新 LINE OA で 賃貸相談 → 聞き返し → 出典付き提案 + カルーセル → 担当者提案 → 時間帯 → 管理画面でリード確認 / 緊急 → 110 案内 / 操作質問 → handoff
