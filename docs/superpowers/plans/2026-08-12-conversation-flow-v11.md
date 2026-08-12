# 会話フロー v1.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `/api/reply` の応答を 3 値化（即答 / 聞き返し最大 3 ターン / 文脈化エスカレーション）し、希望時間帯の受付と営業時間案内を追加する。

**Architecture:** 応答決定を「(EvaluationOutcome, case 状態, config, 現在時刻) → Action」の純関数に切り出し、LLM は「聞き返し文」「受け止め文」「時間帯の構造化抽出」の 3 箇所だけに使う（すべて egress gate または コード判定を通す）。case 状態は vegapunk の case ノードに加算属性 4 つで永続化する。

**Tech Stack:** Rust / 既存の Harness・AnthropicClient・egress gate をそのまま使う。新規依存なし。

**正本:** `docs/superpowers/specs/2026-08-12-conversation-flow-v11-design.md`。本計画と矛盾したら spec が勝つ。

## Global Constraints

- Python / TypeScript 禁止。エラーを握りつぶさない。判断を LLM に委ねない（抽出のみ LLM、判定はコード）
- MCP tool（evaluate_answerability）の入出力・挙動を変更しない
- `clarification_allowed` の設定条件（harness/mod.rs:795-803 の matches!）を変更しない。契約テストで固定する
- 各タスク完了時に `cargo test --manifest-path server/Cargo.toml` と `cargo fmt --check --manifest-path server/Cargo.toml` を実行し、出力確認後にコミット
- commit は Conventional Commits + `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。push・PR 禁止

## 実行分割

- **Part A（vibepod run 1）**: Task 1〜4（config・営業時間・case 属性・生成 2 種）
- **Part B（vibepod run 2）**: Task 5〜7（時間帯受付・オーケストレーション・データ）

---

### Task 1: config 追加と営業時間モジュール

**Files:**
- Modify: `server/src/config.rs`（`ApiConfig` に追加）
- Create: `server/src/harness/hours.rs`（`pub mod hours;` を harness/mod.rs に追加）
- Modify: `server/config.toml` / `server/config.local-https.toml` / `server/config.cloudrun.toml`

**Interfaces:**
- Produces: `ApiConfig` に `clarify_max_turns: u32`（既定 3）、`business_hours: BusinessHoursConfig { days: String（"mon-fri"|"everyday"、既定 "mon-fri"）, start: String（"10:00"）, end: String（"18:00"）, tz: String（"Asia/Tokyo"） }`（すべて serde default）
- `hours::is_within_business_hours(cfg: &BusinessHoursConfig, now_utc: chrono::DateTime<Utc>) -> bool`（tz 変換 → 曜日 → start <= t < end の半開区間）
- `hours::business_hours_label(cfg: &BusinessHoursConfig) -> String`（"mon-fri" → 「平日 10:00〜18:00」）
- `hours::overlaps_business_hours(cfg: &BusinessHoursConfig, days: PrefDays, start: Option<NaiveTime>, end: Option<NaiveTime>) -> bool`（希望時間帯との重なり。start/end 未指定は「終日」とみなす）

- [ ] **Step 1: 失敗するテストを書く**（hours.rs 内 tests）

```rust
#[test]
fn boundary_is_half_open() {
    let cfg = default_hours(); // mon-fri 10:00-18:00 Asia/Tokyo
    // 2026-08-12 は水曜。JST 10:00 = UTC 01:00
    assert!(is_within_business_hours(&cfg, utc("2026-08-12T01:00:00Z")));   // 10:00 は内
    assert!(!is_within_business_hours(&cfg, utc("2026-08-12T09:00:00Z")));  // 18:00 は外
    assert!(!is_within_business_hours(&cfg, utc("2026-08-15T02:00:00Z")));  // 土曜は外
}

#[test]
fn overlap_untimed_preference_counts_as_all_day() {
    let cfg = default_hours();
    assert!(overlaps_business_hours(&cfg, PrefDays::Weekday, None, None));
    assert!(!overlaps_business_hours(&cfg, PrefDays::Weekend, None, None)); // mon-fri 設定なら週末は重ならない
}
```

- [ ] **Step 2: FAIL 確認 → 実装 → PASS**（chrono-tz が依存に無ければ追加。Asia/Tokyo は固定オフセットでも可だが tz 名で解決する実装を優先）
- [ ] **Step 3: config 3 ファイルに `[api]` の新キーを追記（値は既定と同じでよい。cloudrun も同値）**
- [ ] **Step 4: fmt・全テスト → Commit** `feat(api): business hours config and judgment (#17)`

---

### Task 2: case 属性 4 つの読み書き

**Files:**
- Modify: `server/src/harness/mod.rs`（case の load/record 経路）
- Modify: 必要に応じて case を表す struct（`load_case` が返す型）

**Interfaces:**
- Produces: `CaseConvState { clarify_turns: u32, awaiting_time_pref: bool, time_pref_false_count: u32, preferred_contact_time: Option<String> }`
- `Harness::load_conv_state(ctx, case_id) -> Result<CaseConvState>`（属性欠落は既定値。既存 case との後方互換）
- `Harness::save_conv_state(ctx, case_id, &CaseConvState) -> Result<()>`（**case ノードの全属性を読み → 4 属性を差し替え → 全属性を明示再送**。vegapunk 0.2.0 の UpsertNodes は全置換のため、部分送信すると既存属性が消える。backfill_concept_keys と同じ流儀）

- [ ] **Step 1: 失敗するテストを書く**（属性 map ↔ CaseConvState の変換を純関数に切り出してテスト: 欠落 → 既定値 / 保存 → 文字列化の往復）
- [ ] **Step 2: FAIL → 実装 → PASS**（vegapunk への実書き込みはユニットテスト対象外。変換純関数と「全属性再送」の組み立てをテストする）
- [ ] **Step 3: fmt・全テスト → Commit** `feat(harness): conversation state on the case node (#17)`

---

### Task 3: clarification_allowed 契約テストと聞き返し生成

**Files:**
- Modify: `server/src/harness/mod.rs`（契約テスト追加のみ。実装は変更しない）
- Create: `server/src/harness/clarify.rs`

**Interfaces:**
- Produces: `clarify::build_clarify_prompt(question: &str, missing: &str) -> (String, String)`（system, user。検索ヒットの title・本文は**入力に含めない**）
- `clarify::FALLBACK_CLARIFY_TEXT: &str` = 「状況を詳しく教えていただけますか。製品名、いつから発生しているか、画面にエラー表示があるか、が分かると調査が早くなります。」
- 生成 → egress gate → 却下/失敗時 FALLBACK、の関数（reply.rs の draft 生成と同じ構造。同じ AnthropicClient を使う）

- [ ] **Step 1: 契約テストを書く（実装変更なしで PASS するはず。しなければ実装が spec 違反なので差し戻し）**

```rust
#[test]
fn clarification_is_denied_for_layer1_and_layer2_escalations() {
    // decide() を第1層ルールマッチ / 第2層禁止ドメインで通し、
    // 返る Escalate に対して mod.rs の matches! 条件が false になることを検証
}
#[test]
fn clarification_is_allowed_for_layer3_gray() { /* InsufficientDirectness / UnknownAddedSignal で true */ }
```

- [ ] **Step 2: プロンプト組み立てのテスト**（回答禁止の制約文が含まれる / title を渡す引数が存在しない / 質問と不足情報が含まれる）→ 実装 → PASS
- [ ] **Step 3: egress 却下時に FALLBACK へ落ちるテスト**（既存の gate テストパターンを流用）→ 実装 → PASS
- [ ] **Step 4: fmt・全テスト → Commit** `feat(harness): clarify question generation behind the egress gate (#17)`

---

### Task 4: エスカレーション応答の組み立て

**Files:**
- Create: `server/src/harness/escalation_reply.rs`（`pub mod escalation_reply;`）

**Interfaces:**
- Produces: `case_ref(case_id: &str) -> String`（UUID 部分の先頭 8 文字）
- `build_deterministic_block(case_id: &str, hours_label: &str, out_of_hours_now: bool) -> String`（spec §4 の 1〜3。文言は spec の文字列をそのまま使う）
- `build_ack_prompt(question: &str) -> (String, String)`（受け止め文 1〜2 文のみ。回答・期限の約束を禁止する制約文）
- `FALLBACK_ACK_TEXT: &str` = 「お問い合わせありがとうございます。担当者が確認のうえ、あらためてご連絡いたします。」
- 最終形: `escalation_reply = ack_text + "\n\n" + deterministic_block`

- [ ] **Step 1: 決定的ブロックのテスト**（営業時間内 → 時間外案内なし / 時間外 → 案内あり / case_ref 形式 / 伺い文に hours_label が入る）
- [ ] **Step 2: FAIL → 実装 → PASS**
- [ ] **Step 3: ack 生成の gate 却下 → FALLBACK テスト → 実装 → PASS**
- [ ] **Step 4: fmt・全テスト → Commit** `feat(harness): contextual escalation reply assembly (#17)`

---

### Task 5: 希望時間帯の抽出と受付

**Files:**
- Create: `server/src/harness/time_pref.rs`

**Interfaces:**
- Produces: `TimePrefExtraction { is_time_preference: bool, windows: Vec<PrefWindow>, raw: String }`, `PrefWindow { days: PrefDays, start: Option<NaiveTime>, end: Option<NaiveTime> }`, `enum PrefDays { Weekday, Weekend, Any }`
- LLM 構造化抽出関数（JSON 出力を強制するプロンプト + serde parse）。**抽出インフラ失敗（LLM 呼び出しエラー・parse 失敗）は `is_time_preference = false`（真の分類結果）とは型で区別し `Result<TimePrefExtraction, TimePrefExtractionError>` を返す**（design doc §5 追記事項。理由もそちらを参照）。呼び出し側（Task 6）は `Err` の場合 `handle_time_pref` を呼ばず state 変更なしで通常の evaluate フローへ流し、`Err` の連続回数は別カウンタで数えて上限到達時に `awaiting_time_pref` を解除すること（Task 6 の受け入れ条件に含める）
- `handle_time_pref(extraction, state: &mut CaseConvState, cfg: &BusinessHoursConfig) -> TimePrefAction`（純関数）:
  - true かつ重なりあり → `Reply(承りましたテンプレ)` + `preferred_contact_time = raw` + awaiting 解除
  - true かつ重なりなし → `Reply(付記テンプレ)` + `preferred_contact_time = raw +「（対応時間外の希望）」` + awaiting 解除
  - false → `PassToEvaluate` + `time_pref_false_count += 1`、2 に達したら awaiting 解除
- テンプレ文言は spec §5 の文字列をそのまま定数化

- [ ] **Step 1: `handle_time_pref` の失敗するテストを書く**（4 分岐 + false 2 連続で解除 + true が来たら false_count が 0 に戻る）
- [ ] **Step 2: FAIL → 実装 → PASS**
- [ ] **Step 3: 抽出プロンプトの JSON parse（正常 / 壊れた JSON → false 扱い）テスト → 実装 → PASS**
- [ ] **Step 4: fmt・全テスト → Commit** `feat(harness): time preference extraction and intake (#17)`

---

### Task 6: /api/reply オーケストレーション

**Files:**
- Modify: `server/src/api.rs`

**Interfaces:**
- Produces: `enum ReplyAction { Answer(String), Clarify, EscalationReply, TimePrefIntake(...) }` と純関数 `decide_reply_action(outcome: &EvaluationOutcome, conv: &CaseConvState, cfg: &ApiConfig) -> ReplyAction`（spec §2 の決定表 + clarify_turns 上限）
- ハンドラの処理順: (1) case_id ありかつ `awaiting_time_pref` → 時間帯分類（Task 5）。false なら evaluate へ / (2) evaluate → `decide_reply_action` / (3) Clarify なら生成 + `clarify_turns += 1` 保存 / (4) EscalationReply なら組み立て + `awaiting_time_pref = true`・`time_pref_false_count = 0`・`clarify_turns = 0` 保存
- レスポンス契約 `{ reply_text, case_id }` は不変

- [ ] **Step 1: `decide_reply_action` の失敗するテストを書く**（決定表の全行: allowed+draft / allowed+truncated / escalate+clarifiable+残ターンあり / 残ターンなし / clarification_allowed=false / rule_match）
- [ ] **Step 2: FAIL → 実装 → PASS**
- [ ] **Step 3: ハンドラ配線**（evaluate に到達しない範囲の既存統合テストが壊れないこと + 新規分岐は純関数テストで担保。v1 と同じテスト境界）
- [ ] **Step 4: fmt・全テスト・clippy → Commit** `feat(api): three-way reply flow with hearing loop (#17)`

---

### Task 7: 取次依頼のデータ追加とドキュメント

**Files:**
- Modify: `server/data/signal-lexicon.json`（取次依頼 signal。語彙名は `specs/signal-vocabulary.md` の命名規約に従い、同ファイルにも追記）
- Modify: `server/data/urtect/rules.json`（condition = 取次依頼 signal の escalation_rule 1 件、route_to: "support_desk"）
- Modify: `specs/production-cs-mcp.md`（会話フローの節から design doc への参照 1 行）、`CLAUDE.md`（デプロイ後に ingest-rules 再実行が必要な旨を 1 行）

- [ ] **Step 1: lexicon に「担当者につないで」「人間に代わって」等の表層形 → 取次依頼 signal のエントリを追加。既存 lexicon テスト形式があれば合わせてテスト**
- [ ] **Step 2: rules.json に escalation_rule を追記（既存エントリの形式に従う）**
- [ ] **Step 3: ドキュメント 2 箇所を更新 → fmt・全テスト → Commit** `feat(rules): human handoff signal and escalation rule (#17)`

デプロイ後の運用ステップ（計画外・ホスト側）: `gcloud run jobs execute ingest-rules` で rule を投入してから E2E。

---

## Self-Review 済み事項

- spec §2〜§6 の各規定 → Task 1〜7 に対応付け済み（§7 の運用注意は実装対象外）
- clarification_allowed は実装変更せず契約テストのみ（Task 3 Step 1）。変更が必要になった場合は差し戻し
- vegapunk 0.2.0 の UpsertNodes 全置換に対する「全属性再送」を Task 2 に明記
- テスト境界は v1 と同じ（evaluate 到達経路はユニット対象外、E2E で担保）
