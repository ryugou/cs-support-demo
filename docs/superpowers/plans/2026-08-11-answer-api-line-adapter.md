# 応答生成 API + LINE アダプタ Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 顧客メッセージから顧客向け最終応答文を返す `POST /{project_id}/api/reply` と、それを呼ぶ LINE webhook アダプタ `line_adapter` を実装する。

**Architecture:** 既存 `Harness::evaluate()`（domain 入口、rmcp 非依存）を axum ルートから直接呼ぶ薄いアダプタ。判定・生成・egress gate・フォールバックはサーバ内で完結し、レスポンスは `{reply_text, case_id}` のみ。LINE アダプタは判断ゼロの別バイナリ（署名検証 → API コール → 返信）。

**Tech Stack:** Rust / axum / reqwest（既存依存）/ hmac + sha2 + base64（LINE 署名。Cargo.toml に無ければ追加）

**正本:** 仕様の正本は `docs/superpowers/specs/2026-08-11-answer-api-line-adapter-design.md`（以下「design doc」）。本計画と矛盾したら design doc が勝つ。

## Global Constraints

- Python / TypeScript 禁止（リポジトリ規約）
- エラーを握りつぶさない。全エラーパスに運用者が次のアクションを判断できるログを出す
- 認証情報のハードコード禁止。API キーは env `CS_SUPPORT_ANSWER_API_KEY` のみから読む
- 既存の構成・命名・型に合わせ最小差分で変更する
- コミットは Conventional Commits + `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`
- push・PR 作成は禁止（コンテナ内）。commit は可
- 各タスク完了時に `cargo test --manifest-path server/Cargo.toml` と `cargo fmt --check --manifest-path server/Cargo.toml` を実行し、出力を確認してからコミットする

## 実行分割

- **Part A（vibepod run 1）**: Task 1〜5（API 本体）
- **Part B（vibepod run 2）**: Task 6〜8（LINE アダプタ + ドキュメント）

---

### Task 1: config `[api]` セクションと起動時 fail-closed

**Files:**
- Modify: `server/src/config.rs`（`AppConfig` に `api` セクション追加）
- Modify: `server/src/main.rs`（起動時検査）
- Modify: `server/config.cloudrun.toml`（`[api] enabled = true`）
- Modify: `server/config.toml`, `server/config.local-https.toml`（`[api] enabled = false` を明示）

**Interfaces:**
- Produces: `ApiConfig { enabled: bool, fallback_reply_text: String }`、`AppConfig.api: ApiConfig`。Task 3〜5 が参照する。

- [ ] **Step 1: 失敗するテストを書く**（`config.rs` の既存 tests モジュールに追加）

```rust
#[test]
fn api_config_defaults_to_disabled() {
    // 既存テストが使っている最小 TOML 文字列に [api] 無しで load するパターンに合わせる
    let cfg = load_minimal_config_without_api_section();
    assert!(!cfg.api.enabled);
    assert_eq!(
        cfg.api.fallback_reply_text,
        "お問い合わせありがとうございます。担当者が確認のうえ、あらためてご連絡いたします。"
    );
}
```

- [ ] **Step 2: `cargo test --manifest-path server/Cargo.toml api_config` で FAIL を確認**
- [ ] **Step 3: 実装**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_fallback_reply_text")]
    pub fallback_reply_text: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { enabled: false, fallback_reply_text: default_fallback_reply_text() }
    }
}

fn default_fallback_reply_text() -> String {
    "お問い合わせありがとうございます。担当者が確認のうえ、あらためてご連絡いたします。".to_string()
}
```

`AppConfig` に `#[serde(default)] pub api: ApiConfig` を追加。`main.rs` の起動シーケンス（`Harness::build` の近く、既存 fail-closed 群と同じ場所）に:

```rust
let answer_api_key = std::env::var("CS_SUPPORT_ANSWER_API_KEY").ok().filter(|v| !v.is_empty());
if config.api.enabled && answer_api_key.is_none() {
    anyhow::bail!("[api] enabled = true but CS_SUPPORT_ANSWER_API_KEY is not set");
}
```

- [ ] **Step 4: テスト PASS 確認、config 3 ファイルに `[api]` 節を追記**
- [ ] **Step 5: Commit** `feat(api): add [api] config section with fail-closed key check (#14)`

---

### Task 2: 会話履歴型と生成プロンプトへの注入

**Files:**
- Modify: `server/src/harness/reply.rs`（型・採用規則・`build_reply_user_message` 拡張）
- Modify: `server/src/harness/mod.rs`（`evaluate()` に履歴パラメータ追加）
- Modify: `server/src/rmcp_server.rs`（既存呼び出しに `&[]` を渡す）

**Interfaces:**
- Produces: `ReplyHistoryTurn { role: ReplyHistoryRole, text: String }`, `enum ReplyHistoryRole { Customer, Assistant }`, `fn select_history(history: &[ReplyHistoryTurn]) -> Vec<&ReplyHistoryTurn>`（新しい側から最大 6 ターン・合計 4,000 字、超過は古い側から捨てる）
- `Harness::evaluate(ctx, question, product_key, case_id, tools, history: &[ReplyHistoryTurn])`（既存 5 引数 + 履歴）。履歴は**生成にのみ**使い、signal 抽出・判定には渡さない（design doc §5）

- [ ] **Step 1: `select_history` の失敗するテストを書く**（reply.rs tests）

```rust
#[test]
fn select_history_keeps_newest_six_turns() {
    let h: Vec<ReplyHistoryTurn> = (0..10).map(|i| ReplyHistoryTurn {
        role: ReplyHistoryRole::Customer, text: format!("t{i}"),
    }).collect();
    let picked = select_history(&h);
    assert_eq!(picked.len(), 6);
    assert_eq!(picked[0].text, "t4"); // 古い側が落ち、時系列順は維持
}

#[test]
fn select_history_respects_char_budget() {
    let h = vec![
        ReplyHistoryTurn { role: ReplyHistoryRole::Customer, text: "あ".repeat(3000) },
        ReplyHistoryTurn { role: ReplyHistoryRole::Assistant, text: "い".repeat(1500) },
    ];
    let picked = select_history(&h);
    assert_eq!(picked.len(), 1); // 合計 4,000 字超 → 古い側(3000字)が落ちる
    assert!(picked[0].text.starts_with('い'));
}
```

- [ ] **Step 2: FAIL 確認 → 実装**（文字数は `chars().count()`。6 ターン・4,000 字は reply.rs 内の `const MAX_HISTORY_TURNS: usize = 6; const MAX_HISTORY_CHARS: usize = 4000;`）
- [ ] **Step 3: `build_reply_user_message(question, brief, history)` に拡張し、履歴ブロックを材料ブロックと分離して注入する失敗テスト → 実装**

```rust
#[test]
fn user_message_includes_history_block_when_present() {
    let history = vec![ReplyHistoryTurn { role: ReplyHistoryRole::Customer, text: "前の質問".into() }];
    let msg = build_reply_user_message("今の質問", &empty_brief(), &history);
    assert!(msg.contains("前の質問"));
    assert!(msg.contains("今の質問"));
}

#[test]
fn user_message_unchanged_when_history_empty() {
    let with = build_reply_user_message("q", &empty_brief(), &[]);
    assert!(!with.contains("会話履歴"));
}
```

履歴ブロックの形式（役割ラベルは「顧客」「サポート」）:

```text
## 直近の会話履歴（参考。回答は最新の質問に対して行う）
顧客: <text>
サポート: <text>
```

- [ ] **Step 4: `Harness::evaluate` に `history: &[reply::ReplyHistoryTurn]` を追加し、下書き生成呼び出しへ渡す。`rmcp_server.rs` の呼び出しは `&[]`。既存テストの evaluate 呼び出しも `&[]` で更新**
- [ ] **Step 5: 全テスト PASS・fmt 確認 → Commit** `feat(reply): thread conversation history into draft generation only (#14)`

---

### Task 3: API 型・入力検証・認証ミドルウェア

**Files:**
- Create: `server/src/api.rs`（`pub mod api;` を `lib.rs`/`main.rs` のモジュール宣言に追加。既存の module 宣言方式に従う）

**Interfaces:**
- Produces: `ReplyRequest { message: String, history: Option<Vec<HistoryEntry>>, case_id: Option<String> }`, `HistoryEntry { role: HistoryRole, text: String }`, `HistoryRole::{Customer, Assistant}`（serde rename: `customer` / `assistant`）, `ReplyResponse { reply_text: String, case_id: String }`, `ErrorBody { error: String, message: String }`
- `fn validate(req: &ReplyRequest) -> Result<(), String>`：design doc §2 の表のとおり（message trim 後 1〜5,000 字 / history ≤20 要素・各 text 1〜2,000 字 / case_id ≤128 字）
- `fn constant_time_eq(a: &[u8], b: &[u8]) -> bool`
- `ApiState { config: Arc<AppConfig>, harness: Arc<Harness>, api_key: String, ... }`（ToolService 構築に必要な共有物は main.rs の MCP ルート組み立て（main.rs:200 付近）と同じものを持たせる）

- [ ] **Step 1: validation の失敗するテストを書く**（境界値: 空 message / 5,001 字 / history 21 要素 / text 2,001 字 / case_id 129 字 / 正常系）

```rust
#[test]
fn validate_rejects_empty_message() {
    let req = ReplyRequest { message: "  ".into(), history: None, case_id: None };
    assert!(validate(&req).is_err());
}
// 以下同様に各境界を 1 テストずつ。正常系は Ok(()) を確認
```

- [ ] **Step 2: FAIL → 実装 → PASS**
- [ ] **Step 3: `constant_time_eq` のテスト → 実装**

```rust
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

- [ ] **Step 4: 認証抽出関数のテスト → 実装**（`Authorization: Bearer <key>` を取り出し `constant_time_eq` で照合。欠落・不一致は 401 + `{"error":"unauthorized",...}`）。サービス principal は:

```rust
let identity = crate::oauth::VerifiedIdentity {
    sub: "service:answer-api".to_string(),
    email: "answer-api@cs-support.internal".to_string(),
};
```

- [ ] **Step 5: fmt・テスト確認 → Commit** `feat(api): request types, validation, and bearer auth for /api/reply (#14)`

---

### Task 4: /api/reply ハンドラ本体

**Files:**
- Modify: `server/src/api.rs`

**Interfaces:**
- Consumes: Task 1 の `ApiConfig`、Task 2 の `evaluate(..., history)` と `ReplyHistoryTurn`、Task 3 の型群
- Produces: `pub fn api_router(state: ApiState) -> axum::Router`（`POST /{project_id}/api/reply`）

処理順（design doc §2〜§4 を写像）:

1. 認証（Task 3）→ 失敗 401
2. `project_id` を config.projects から解決 → 無ければ 404 `unknown_project`
3. `validate` → 失敗 400 `invalid_request`
4. `HistoryEntry` → `ReplyHistoryTurn` 変換、`harness.begin(&identity, schema, manual_schema)` → `evaluate(ctx, message, None, case_id, tools, &history)`
5. `reply_text` 決定: `decision` が allowed かつ `customer_reply_draft = Some(d)` → `d`。それ以外 → `config.api.fallback_reply_text`（フォールバック時は `tracing::warn!(request_id, ?decision, "answer api fell back to fixed reply")`）
6. 200 `{ reply_text, case_id: outcome.case_id }`

エラー写像: `evaluate` の `Err(e)` はエラーチェーンに `tonic::transport::Error` または `tonic::Status`（code = Unavailable）を含む場合 503 `upstream_unavailable`、それ以外 500 `internal`（`tracing::error!` 必須）。未知 `case_id` は evaluate 内の case load 失敗を warn + 新規 case で継続（harness 側に既にその挙動が無ければ、load 失敗時に新規発番へフォールバックする変更を harness/mod.rs に入れる。design doc §2）。

- [ ] **Step 1: ハンドラの統合テストを書く**（`tower::ServiceExt::oneshot`。Harness は `harness/mod.rs:1082` の `harness_for_test()` パターンを `pub(crate)` 化するか、api.rs tests に同等のコンストラクタを書いて使う。LLM スタブは `draft_customer_reply_via_stub`（harness/mod.rs:1301）を流用）

```rust
#[tokio::test]
async fn reply_returns_draft_when_allowed() { /* allowed + draft あり → reply_text == draft */ }
#[tokio::test]
async fn reply_falls_back_on_escalate() { /* escalate → fallback_reply_text */ }
#[tokio::test]
async fn reply_requires_auth() { /* ヘッダ無し → 401 */ }
#[tokio::test]
async fn reply_rejects_unknown_project() { /* 404 */ }
```

- [ ] **Step 2: FAIL → 実装 → PASS**（4 テスト + 400 系）
- [ ] **Step 3: fmt・全テスト → Commit** `feat(api): /api/reply handler with decision table and error mapping (#14)`

---

### Task 5: main.rs 配線と Part A 仕上げ

**Files:**
- Modify: `server/src/main.rs`

- [ ] **Step 1: `config.api.enabled` のときだけ `app = app.merge(api::api_router(state))` する配線を追加**（`require_google_auth` はこのルートに適用しない。router merge は main.rs:106-213 の既存並びの末尾）
- [ ] **Step 2: `cargo test` / `cargo fmt --check` / `cargo check` 全 PASS を確認**
- [ ] **Step 3: ローカル起動スモーク**（config.toml は enabled=false のまま起動 → /api/reply が 404 であること、env `CS_SUPPORT_ANSWER_API_KEY` 無しで enabled=true にすると起動失敗すること を確認し、結果を最終出力に含める）
- [ ] **Step 4: Commit** `feat(api): wire /api/reply router behind [api] enabled flag (#14)`

---

### Task 6: LINE アダプタ — 署名検証・セッションストア・イベント処理

**Files:**
- Create: `server/src/bin/line_adapter.rs`
- Modify: `server/Cargo.toml`（`[[bin]] name = "line_adapter"`、`hmac` / `sha2` / `base64` が無ければ追加）

**Interfaces:**
- Produces（bin 内）: `fn verify_signature(channel_secret: &str, body: &[u8], signature_b64: &str) -> bool`、`struct SessionStore`（`get / update / sweep`。`user_id → Session { case_id: Option<String>, history: VecDeque<(Role, String)>（上限 20）, last_at: Instant }`、TTL 60 分、上限 10,000 エントリで `last_at` 最古を破棄）、webhook イベント parse 用 serde 型（`events[].type` / `message.type` / `message.text` / `replyToken` / `source.userId`）

- [ ] **Step 1: `verify_signature` の失敗するテストを書く**（正しい HMAC-SHA256/base64 / 改竄 body / 不正 base64 の 3 ケース。期待値はテスト内で hmac crate により計算して固定）
- [ ] **Step 2: FAIL → 実装 → PASS**（比較は Task 3 と同じ constant-time 方式。bin 内に複製してよい）
- [ ] **Step 3: `SessionStore` のテスト → 実装**（TTL 経過で消える / 上限超過で最古が消える / history が 20 で頭から捨てられる）
- [ ] **Step 4: webhook parse のテスト → 実装**（text メッセージ / 画像（非テキスト）/ follow イベントの 3 fixture JSON）
- [ ] **Step 5: fmt・テスト → Commit** `feat(line): signature verification, session store, and event parsing (#14)`

---

### Task 7: LINE アダプタ — API クライアントと配線

**Files:**
- Modify: `server/src/bin/line_adapter.rs`
- Modify: `Dockerfile`（builder に `--bin line_adapter` 追加、runtime に `COPY --from=builder /app/server/target/release/line_adapter /usr/local/bin/line_adapter` 追加）

処理順は design doc §6 のとおり。env は起動時に読み、必須 4 つ（`LINE_CHANNEL_SECRET` / `LINE_CHANNEL_ACCESS_TOKEN` / `CS_ANSWER_API_URL` / `CS_ANSWER_API_KEY`）欠落なら `anyhow::bail!` で起動失敗。reqwest client は timeout 50 秒。LINE Reply API は `POST https://api.line.me/v2/bot/message/reply`、body `{ "replyToken": ..., "messages": [{"type":"text","text": <reply_text（4,900 字超は末尾切り詰め）>}] }`。API 非 200・タイムアウト時は `CS_LINE_FALLBACK_TEXT` を返信し、セッションは更新しない。非テキスト message は `CS_LINE_NONTEXT_TEXT` を返信。message 以外のイベントは無視。全イベント処理後 200。

- [ ] **Step 1: 応答組み立て（API 200 → 履歴追記 / 非 200 → fallback・履歴不変）をロジック関数に切り出してテスト → 実装**
- [ ] **Step 2: axum ルート（`POST /line/webhook`, `GET /healthz`, `GET /livez`）と main を実装。`cargo build --bin line_adapter` 成功を確認**
- [ ] **Step 3: Dockerfile 更新。fmt・全テスト → Commit** `feat(line): webhook adapter binary calling the answer api (#14)`

---

### Task 8: ドキュメント更新

**Files:**
- Modify: `specs/production-cs-mcp.md`（インターフェース層の節を追加: 応答生成 API の存在・責務分担。契約の正本は design doc を参照し複製しない）
- Modify: `CLAUDE.md`（Cloud Run 節に `cs-support-line` service・新 secret 3 件（`cs-support-answer-api-key` / `line-channel-secret` / `line-channel-access-token`）・`CS_SUPPORT_ANSWER_API_KEY` の fail-closed を追記）

- [ ] **Step 1: 両ファイルを design doc と矛盾なく更新**（document-write-rule 準拠: 正本参照、複製禁止、旧仕様の残置禁止）
- [ ] **Step 2: Commit** `docs: record the answer api and line adapter surfaces (#14)`

---

## Self-Review 済み事項

- design doc §2〜§9 の各要件 → Task 1〜8 に対応付け済み（§7 デプロイの gcloud 実行はホスト側作業のため計画外）
- 型名は `EvaluationOutcome` / `VerifiedIdentity { sub, email }` / `harness_for_test()` を実コードから転記（runbook 調査 2026-08-11）
- 履歴は evaluate の判定に渡さない（生成のみ）を Task 2 Step 4 で固定
