# Production CS MCP Step 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `specs/production-cs-mcp.md` の Step 1 実装仕様（S1-0〜S1-11）を既存 `cs-support-mcp`（Rust / axum / rmcp / tonic）に最小差分で実装する。

**Architecture:** MCP サーバ内に Harness 層（`server/src/harness/`）を新設し、AuthN(JWT) → (A) scope 強制 → 取得 → signal 正規化（決定論 lexicon）→ (B) 3 層判定 → egress gate → WORM 監査、のパイプラインを純関数の集合として実装する。知識（KnownResolution / Signal / EscalationRule / ProhibitedDomain / support 系 record）は PunkRecord（vegapunk gRPC）に加算スキーマで格納し、判定は一切 PunkRecord に載せない（I4）。監査は MCP 側の別建て WORM（append-only JSONL + hash chain）。

**Tech Stack:** Rust (edition 2021), axum 0.8, rmcp 1.7 (streamable HTTP), tonic/prost (vegapunk gRPC), 追加 crate: `jsonwebtoken = "9"`, `uuid = { version = "1", features = ["v4"] }`, `chrono = { version = "0.4", features = ["serde"] }`。

## Global Constraints

- Python / TypeScript は一切使わない。Python ファイル・tsx・npm・package.json を追加しない。
- vegapunk API を推測しない。使ってよい RPC は既存 `VegapunkClient` で検証済みのもののみ: `QueryNodes` / `GetGraphSnapshot` / `UpsertNodes` / `UpsertEdges` / `Search` / `CreateSchema` / `UpdateSchema` / `GetSchema`。新規 RPC が必要になったら `// TODO: bind to vegapunk <api>` + config 境界に閉じ込める。
- ローカルで vegapunk を起動しない。SSH tunnel（`ssh -L 16840:...`）を張らない。gRPC 接続先は `http://vegapunk.local:6840`（token: `/private/tmp/vegapunk-bearer-token`）。
- 判断ロジック（可否・stakes・訂正分類）を tool handler に直書きしない。`egress_gate` / `answerability_threshold` / `correction_intake` は独立した関数境界（S1-0 三原則 1, 3）。
- known_resolution の signal_set を JSON 配列属性に畳まない。`Signal` ノード + `HAS_SIGNAL` 辺で持つ（I2、アンチパターン 3）。
- client が渡す tenant / label / sensitivity / schema を検索に到達させない。scope はサーバ導出のみ（I1）。tool の入力スキーマに scope 系フィールドを追加しない。
- スキーマ変更は加算のみ。既存 node/edge 型・属性を削除しない（I3）。
- 第1層・第2層にメモ化・学習を適用しない。known_resolution は第1層・第2層をバイパスできない（短絡順序）。
- エラーを握りつぶさない。すべての `Result` はログまたは呼び出し元へ伝搬。
- secret（JWT 鍵・vegapunk token）を設定ファイルやコードに平文で置かない。env / file 注入のみ。
- コミットは Conventional Commits。main へ直接 push しない（Task 0 の baseline commit のみ例外: リポジトリに commit が 1 件も無いため）。
- タイムゾーンは JST。ただし監査レコードの timestamp は RFC3339 UTC で記録する（`chrono::Utc`）。
- 会話・実装で仕様の結論が変わったら同じターンで `specs/production-cs-mcp.md` を更新する。

## 確定済みの設計判断（2026-07-03、spec S1-11 に記録済み）

1. `normalize_to_signals` は Step 1 では決定論 lexicon（`SignalNormalizer` trait の `LexiconNormalizer` 実装）。LLM 実装は後段で同 trait に追加。
2. actor 認証は JWT HS256（`Claims { sub, role, exp, iss }`）+ config actor 表。JWT secret 未設定時は `default_actor` フォールバック（dev 専用・warn ログ必須）。
3. **会話層（spec 改訂 2026-07-03 で Step 1 スコープに追加）**: 累積 signal 集合はサーバ側で `support_case -[HAS_SIGNAL]-> Signal` 辺に永続し、`evaluate_answerability` は `case_id` で毎ターン累積集合に対して再判定する。client から prior signals は受け取らない（入力不信を会話層にも適用）。聞き返しは `clarification_allowed`（決定論）で表現し、文面は client が生成する。
4. **grade（昇格・降格）は Step 1 から運用**（spec ロードマップ遵守事項 3）: `regrade` 純関数 + config しきい値（`[harness.grading]`、初期値 N=3 / M=2 / r=0.2 / K=2 は仮置き・業務確認要）。
5. **emit セマンティクス（spec 改訂で変更）**: egress `pass` は担当者へ直接応答してよい（Step 1 の利用者は担当者）。「全件承認フロー」ではない。block / abstain / グレーはエスカレーション応答（未検証 AI 草案の添付可、承認すれば `add_known_resolution` で知識蓄積）。
6. **EmitContext のチャネル種別 `{operator, customer_chat, customer_voice}` を Step 1 から持つ**（operator 固定）。`egress_gate` は任意テキスト断片を受けられるシグネチャにする（Step 3 の文単位呼び出しへの前方互換）。

## File Structure（作成・変更ファイル一覧）

```text
server/
  Cargo.toml                     # 変更: jsonwebtoken / uuid / chrono 追加
  src/
    lib.rs                       # 変更: pub mod harness 追加
    config.rs                    # 変更: AuthConfig / ActorConfig / HarnessConfig 追加
    main.rs                      # 変更: Harness 構築・注入
    rmcp_server.rs               # 変更: 全 tool を Harness 経由化 + 新 tool 8 本
    harness/
      mod.rs                     # 新規: Harness 本体・RequestContext・pipeline orchestration
      signal.rs                  # 新規: Signal / SignalSet / SignalNormalizer / LexiconNormalizer
      authn.rs                   # 新規: JWT 検証 → Actor
      scope.rs                   # 新規: AccessScope / resolve_scope（I1）
      rules.rs                   # 新規: EscalationRule / ProhibitedDomain / KnownResolution + 3 層照合
      decision.rs                # 新規: stakes / threshold / evidence_sufficient / decide（B）
      egress.rs                  # 新規: NgDictionary / egress_gate
      correction.rs              # 新規: correction_intake
      audit.rs                   # 新規: WORM（append-only JSONL + hash chain）
      knowledge.rs               # 新規: PunkRecord 読み書き（KnowledgeStore）
    bin/
      ingest_rules.rs            # 新規: schema 加算登録 + 第1/2層ルール・NG 辞書由来ノード投入 CLI
  data/
    signal-lexicon.json          # 新規: signal 語彙 + 同義語（要ユーザレビュー）
    ng-dictionary.json           # 新規: 決定論 NG 辞書 + abstain 語（要ユーザレビュー）
    rules.sample.json            # 新規: 第1層ルール・第2層領域の初期データ
schema/
  cs-schema.yml                  # 変更: 加算 node/edge/traceable_pairs
specs/
  production-cs-mcp.md           # 同期済み（このターンで Desktop 改訂版に更新 + S1-11 追記）
  signal-vocabulary.md           # 新規: signal 語彙仕様（最優先確定事項の文書化）
```

各ファイルの責務は 1 つ。判定系（rules/decision/egress/correction）はすべて **純関数 + 単体テスト**、I/O 系（authn/audit/knowledge）は薄い шеll。`harness/mod.rs` だけが両者を接続する。

## Interfaces 総覧（タスク間契約）

後続タスクはここに定義された名前・型のみを参照する。

```rust
// harness/signal.rs
pub struct Signal(String);                       // Signal::new(&str), .as_str()
pub type SignalSet = std::collections::BTreeSet<Signal>;
pub enum SignalClass { Hazard, Context }
pub trait SignalNormalizer: Send + Sync { fn normalize(&self, text: &str) -> SignalSet; }
pub struct LexiconNormalizer;                    // from_path(&Path) -> Result<Self>, class_of(&Signal) -> Option<SignalClass>

// harness/authn.rs
pub struct Claims { pub sub: String, pub role: String, pub exp: usize, pub iss: String }
pub enum Role { Operator, Supervisor, Admin }    // FromStr 実装
pub struct Actor { pub sub: String, pub role: Role, pub allowed_schemas: Vec<String> }
pub struct Authenticator;                        // new(secret: Option<Vec<u8>>, actors: &[ActorConfig], default_actor: Option<String>)
                                                 // authenticate(&self, authorization: Option<&str>) -> anyhow::Result<Actor>

// harness/scope.rs
pub struct AccessScope { pub allowed_schemas: Vec<String>, pub max_sensitivity: Option<String>, pub label_allowlist: Option<Vec<String>> }
pub fn resolve_scope(actor: &Actor, project_schema: &str) -> anyhow::Result<AccessScope>;
impl AccessScope { pub fn enforced_schema(&self) -> &str; }

// harness/rules.rs
pub enum Binding { Mandatory, Advisory }
pub enum Grade { ApprovalRequired, AutoAnswerAudited, Demoted }
pub enum SourceAuthority { Authoritative, NonAuthoritative }
pub enum RootCause { KnowledgeError, RetrievalMiss }
pub struct EscalationRule { pub id: String, pub condition: SignalSet, pub route: String, pub owner: Option<String>, pub binding: Binding }
pub struct ProhibitedDomain { pub id: String, pub domain_signals: SignalSet, pub text_patterns: Vec<String>, pub route: String, pub binding: Binding }
pub struct KnownResolution { /* S1-3 全フィールド + signal_specificity() */ }
pub fn match_layer1<'a>(&'a [EscalationRule], &SignalSet) -> Option<&'a EscalationRule>;
pub fn match_layer2<'a>(&'a [ProhibitedDomain], &SignalSet, raw_text: &str) -> Option<&'a ProhibitedDomain>;
pub enum KrMatch<'a> { Applicable(&'a KnownResolution), BlockedByAddedSignal { leftover: SignalSet }, None }
pub fn match_known_resolution<'a>(&'a [KnownResolution], &SignalSet) -> KrMatch<'a>;

// harness/decision.rs
pub enum Stakes { Low, Mid, High }
pub struct StakesInput { pub mandatory_domain_near: bool, pub ng_near_hit: bool, pub hazard_signal_count: usize }
pub fn classify_stakes(&StakesInput) -> Stakes;
pub struct Thresholds { pub low: f32, pub mid: f32, pub high: f32 }   // Default: 0.6 / 0.8 / 0.95
pub fn answerability_threshold(&Thresholds, Stakes) -> f32;
pub enum EvidenceRequirement { DirectManualCoverage { required: f32, best: f32 } }
pub enum Sufficiency { Sufficient, Insufficient { missing: Vec<EvidenceRequirement> } }
pub fn evidence_sufficient(threshold: f32, best_hit_score: Option<f32>) -> Sufficiency;
pub enum AnswerSource { KnownResolution, Manual }
pub enum EscalateReason { PermissionDenied, RegulatedOrSafety, RequiresHumanApproval, InsufficientDirectness, UnknownAddedSignal }
pub enum DisclosureScope { ConfirmingWithTeam, NoInternalDetails }
pub enum AnswerDecision { Allowed { .. }, Escalate { .. } }           // Task 6 に全定義
pub struct DecisionInput<'a>;                                          // Task 6 に全定義
pub fn decide(&DecisionInput) -> AnswerDecision;                       // 3 層短絡・純関数

// harness/egress.rs
pub struct NgDictionary;                          // from_path(&Path) -> Result<Self>, near_hit(&str) -> bool
pub enum EmitChannel { Operator, CustomerChat, CustomerVoice }   // Step 1 は Operator 固定
pub struct EmitContext { pub channel: EmitChannel }
pub enum EgressVerdict { Pass, Block { term: String }, Abstain { term: String } }
// text は全文でも文単位でも受けられる（呼び出し粒度の自由を確保。ロードマップ遵守事項 2）
pub fn egress_gate(text: &str, ctx: &EmitContext, ng: &NgDictionary) -> EgressVerdict;

// harness/correction.rs
pub enum CorrectionRouting { ConversationOnly, SearchImprovementQueue, KnownResolutionCandidate }
pub fn correction_intake(SourceAuthority, RootCause) -> CorrectionRouting;

// harness/audit.rs
pub struct AuditDraft { pub request_id: String, pub schema: String, pub actor: String, pub used_scope: AccessScope,
                        pub retrieved_node_ids: Vec<String>, pub decision: String, pub route: Option<String>,
                        pub governing_norm_ids: Vec<String> }
pub struct WormAuditLog;                          // open(&Path) -> Result<Self>, append(&self, AuditDraft) -> Result<String /*event_id*/>

// harness/grading.rs（spec ロードマップ遵守事項 3: grade を Step 1 から運用）
pub struct GradingThresholds { pub promote_approvals: u32, pub promote_approvers: u32, pub promote_max_rejection_rate: f32, pub demote_rejections: u32 }
// Default: 3 / 2 / 0.2 / 2（S1-11 の仮置き。config [harness.grading] で上書き）
pub fn regrade(current: Grade, approval_count: u32, rejection_count: u32, approver_count: usize, t: &GradingThresholds) -> Grade;  // 純関数

// harness/knowledge.rs
pub struct KnowledgeStore;                        // new(Arc<VegapunkClient>)
// load_escalation_rules / load_prohibited_domains / load_known_resolutions(schema) -> Result<Vec<..>>
// insert_known_resolution(schema, &NewKnownResolution) -> Result<String /*kr_id*/>
// record(schema, node_type: &str, key: &str, attrs: Vec<(String,String)>) -> Result<()>
// load_case_signals(schema, case_id) -> Result<SignalSet>          // support_case の HAS_SIGNAL 辺を復元
// append_case_signals(schema, case_id, &SignalSet) -> Result<()>   // support_case ノード upsert + HAS_SIGNAL 辺追加
// update_known_resolution_grade(schema, kr_id, approval_count, rejection_count, approver_set, grade) -> Result<()>

// harness/mod.rs
pub struct Harness;                               // 上記全部を束ねる。build(&AppConfig, Arc<VegapunkClient>) -> Result<Self>
pub struct RequestContext { pub actor: Actor, pub scope: AccessScope, pub schema: String, pub request_id: String }
// begin(&self, authorization: Option<&str>, project_schema: &str) -> Result<RequestContext>
// evaluate(&self, &RequestContext, question: &str, product_key: Option<&str>, case_id: Option<&str>, &ToolService) -> Result<EvaluationOutcome>
//   会話層: case_id 有 → 累積 signal 集合をロードし今ターン分と union、新規 signal を永続、累積集合で判定（毎ターン再判定）
pub struct EvaluationOutcome {
    pub decision: AnswerDecision,
    pub signals: SignalSet,               // 今ターンで抽出した signal
    pub accumulated_signals: SignalSet,   // 判定に使った累積集合（= 判定根拠）
    pub case_id: String,                  // 新規作成時は採番して返す
    pub clarification_allowed: bool,      // 第3層 insufficient_directness / unknown_added_signal のみ true（決定論）
    pub hits: Vec<SectionHit>,
    pub audit_event_id: String,
}
```

---

### Task 0: baseline commit とブランチ作成

**Files:**
- 変更なし（git 操作のみ）

このリポジトリには commit が 1 件も存在しない（全ファイル untracked）。まず現状を main に baseline commit し、feature ブランチを切る。

- [ ] **Step 1: `.gitignore` の確認**

`server/target/`, `server/certs/`, `.DS_Store` が ignore されていることを確認する。

Run: `cat .gitignore`
Expected: `server/target` 相当の行が存在する。無ければ追記する。

- [ ] **Step 2: baseline commit（main、初回のみ例外）**

```bash
git add -A
git commit -m "chore: baseline commit of existing cs-support-mcp demo implementation"
```

- [ ] **Step 3: feature ブランチ作成**

```bash
git switch -c feat/production-cs-mcp-step1
```

- [ ] **Step 4: ビルドが通る状態であることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo check 2>&1 | tail -5`
Expected: `Finished` 行が出る（既存コードがビルド可能であることの確認）。

---

### Task 1: signal 語彙仕様 + lexicon / NG 辞書 fixture

**Files:**
- Create: `specs/signal-vocabulary.md`
- Create: `server/data/signal-lexicon.json`
- Create: `server/data/ng-dictionary.json`

**Interfaces:**
- Produces: `signal-lexicon.json` の JSON 形式（Task 2 の `LexiconNormalizer` が読む）、`ng-dictionary.json` の JSON 形式（Task 7 の `NgDictionary` が読む）。

S1-9 の最優先確定事項。化粧品・健康食品カテゴリに限定した初版語彙。**このタスクの成果物はユーザレビュー必須**（語彙・NG 語の妥当性は業務判断）。実装を止めないため初版はここで定義した内容で進め、レビュー結果は後続コミットで反映する。

- [ ] **Step 1: `specs/signal-vocabulary.md` を書く**

```markdown
# Signal 語彙仕様（初版 / 化粧品・健康食品カテゴリ限定）

Production CS MCP の 3 層判定・照合・egress は全てこの語彙の上に乗る（specs/production-cs-mcp.md S1-9）。
語彙は PunkRecord の `Signal` ノードの `value` として格納される。表現ゆれの吸収（軸1）は
`server/data/signal-lexicon.json` の surface_forms で行う（Step 1 は決定論 lexicon、S1-11）。

## class の意味

- `hazard`: 安全に関わる条件語。stakes 判定（S1-6 の mid 昇格）に使う。
- `context`: 質問型・文脈の条件語。stakes には効かないが照合の条件にはなる。

## 語彙（初版 12 語）

| signal | class | 意味 | surface_forms 例 |
|---|---|---|---|
| discoloration | hazard | 変色 | 変色, 色が変わ, 色味がおかし, 茶色くな, 黒ずん |
| mold | hazard | カビ | カビ, かび, 白い綿, ふわふわした付着 |
| foreign_substance | hazard | 異物混入 | 異物, 虫が入, 髪の毛が入, 破片 |
| odor_abnormality | hazard | 異臭 | 異臭, 変な匂い, 変なにおい, 酸っぱい匂い |
| post_ingestion_symptom | hazard | 摂取後の体調異常 | 食べたら, 飲んだら気分, 腹痛, 吐き気, 下痢 |
| skin_irritation | hazard | 肌トラブル | ピリピリ, かぶれ, 赤み, かゆみ, 湿疹, ヒリヒリ |
| allergy_concern | hazard | アレルギー懸念 | アレルギー, アレルゲン |
| efficacy_claim | hazard | 効果効能への言及 | 効果, 効能, 治る, 痩せ, 改善します |
| dosage_for_condition | hazard | 症状・状態に応じた用量相談 | 妊娠中, 授乳中, 持病, 服薬中, 子供に飲ませ |
| continue_use_question | context | 継続使用可否の相談 | 使い続けて, 続けて大丈夫, 食べてもいい, 飲んでもいい |
| expiry_question | context | 期限・保存の相談 | 賞味期限, 消費期限, 保存方法, 開封後 |
| refund_request | context | 返金・返品の要望 | 返金, 返品, 交換して |

## 運用ルール

- 語彙の追加は加算のみ。既存 signal の削除・意味変更をしない（I3 と同じ規律）。
- surface_forms は `resolve::normalize_key` 正規化後の部分一致で照合される。
- 既知の限界: 辞書外の表現は取りこぼす。第2層の raw text パターン照合と全件人承認で吸収する（S1-11）。
- **本初版は 2026-07-03 時点のドラフト。業務担当のレビューで確定させること。**
```

- [ ] **Step 2: `server/data/signal-lexicon.json` を書く**

```json
{
  "signals": [
    { "signal": "discoloration", "class": "hazard", "surface_forms": ["変色", "色が変わ", "色味がおかし", "茶色くな", "黒ずん"] },
    { "signal": "mold", "class": "hazard", "surface_forms": ["カビ", "かび", "白い綿", "ふわふわした付着"] },
    { "signal": "foreign_substance", "class": "hazard", "surface_forms": ["異物", "虫が入", "髪の毛が入", "破片"] },
    { "signal": "odor_abnormality", "class": "hazard", "surface_forms": ["異臭", "変な匂い", "変なにおい", "酸っぱい匂い"] },
    { "signal": "post_ingestion_symptom", "class": "hazard", "surface_forms": ["食べたら", "飲んだら気分", "腹痛", "吐き気", "下痢"] },
    { "signal": "skin_irritation", "class": "hazard", "surface_forms": ["ピリピリ", "かぶれ", "赤み", "かゆみ", "湿疹", "ヒリヒリ"] },
    { "signal": "allergy_concern", "class": "hazard", "surface_forms": ["アレルギー", "アレルゲン"] },
    { "signal": "efficacy_claim", "class": "hazard", "surface_forms": ["効果", "効能", "治る", "痩せ", "改善します"] },
    { "signal": "dosage_for_condition", "class": "hazard", "surface_forms": ["妊娠中", "授乳中", "持病", "服薬中", "子供に飲ませ"] },
    { "signal": "continue_use_question", "class": "context", "surface_forms": ["使い続けて", "続けて大丈夫", "食べてもいい", "飲んでもいい"] },
    { "signal": "expiry_question", "class": "context", "surface_forms": ["賞味期限", "消費期限", "保存方法", "開封後"] },
    { "signal": "refund_request", "class": "context", "surface_forms": ["返金", "返品", "交換して"] }
  ]
}
```

- [ ] **Step 3: `server/data/ng-dictionary.json` を書く**

block = 明示 NG 語（外向き文面に絶対に出さない）。abstain = 暗示効能リスク語（S1-4: 疑わしきは出さない）。

```json
{
  "block_terms": ["必ず治ります", "完治します", "医薬品と同等", "副作用はありません", "誰でも痩せます", "病気が治る"],
  "abstain_terms": ["治る", "効きます", "痩せる", "アンチエイジング効果", "症状が改善", "医学的に証明"]
}
```

- [ ] **Step 4: JSON の構文検証**

Run: `jq . server/data/signal-lexicon.json > /dev/null && jq . server/data/ng-dictionary.json > /dev/null && echo OK`
Expected: `OK`

- [ ] **Step 5: Commit**

```bash
git add specs/signal-vocabulary.md server/data/signal-lexicon.json server/data/ng-dictionary.json
git commit -m "feat: add signal vocabulary spec and lexicon/NG dictionary fixtures (draft, pending business review)"
```

---

### Task 2: harness モジュール骨格 + signal.rs（LexiconNormalizer）

**Files:**
- Create: `server/src/harness/mod.rs`（このタスクでは sub-module 宣言のみ）
- Create: `server/src/harness/signal.rs`
- Modify: `server/src/lib.rs`（`pub mod harness;` 追加）
- Modify: `server/Cargo.toml`（依存追加）
- Test: `server/src/harness/signal.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::resolve::normalize_key(&str) -> String`（既存）
- Produces: `Signal` / `SignalSet` / `SignalClass` / `SignalNormalizer` trait / `LexiconNormalizer`（総覧どおり）

- [ ] **Step 1: Cargo.toml に依存を追加**

`server/Cargo.toml` の `[dependencies]` に追記:

```toml
chrono = { version = "0.4", features = ["serde"] }
jsonwebtoken = "9"
uuid = { version = "1", features = ["v4"] }
```

- [ ] **Step 2: 失敗するテストを書く**

`server/src/lib.rs` に `pub mod harness;` を追加し、`server/src/harness/mod.rs` を作る:

```rust
pub mod signal;
```

`server/src/harness/signal.rs` に型定義より先にテストを書く（コンパイルを通すため最小の型スタブと同時でよいが、実装ロジックは書かない）:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn lexicon() -> LexiconNormalizer {
        LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "discoloration", "class": "hazard", "surface_forms": ["変色", "色が変わ"] },
                { "signal": "mold", "class": "hazard", "surface_forms": ["カビ", "かび"] },
                { "signal": "continue_use_question", "class": "context", "surface_forms": ["食べてもいい"] }
            ] }"#,
        )
        .expect("lexicon parses")
    }

    #[test]
    fn normalize_absorbs_surface_variation() {
        let n = lexicon();
        let a = n.normalize("商品が変色しています");
        let b = n.normalize("色が変わってしまった");
        assert_eq!(a, b);
        assert!(a.contains(&Signal::new("discoloration")));
    }

    #[test]
    fn normalize_extracts_multiple_signals() {
        let n = lexicon();
        let s = n.normalize("変色していてカビも生えているが食べてもいいか");
        let expected: Vec<&str> = vec!["continue_use_question", "discoloration", "mold"];
        assert_eq!(s.iter().map(Signal::as_str).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn normalize_returns_empty_for_unrelated_text() {
        let n = lexicon();
        assert!(n.normalize("送料はいくらですか").is_empty());
    }

    #[test]
    fn class_of_distinguishes_hazard_and_context() {
        let n = lexicon();
        assert_eq!(n.class_of(&Signal::new("mold")), Some(SignalClass::Hazard));
        assert_eq!(n.class_of(&Signal::new("continue_use_question")), Some(SignalClass::Context));
        assert_eq!(n.class_of(&Signal::new("unknown")), None);
    }
}
```

- [ ] **Step 3: テストが失敗（コンパイルエラー）することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::signal 2>&1 | tail -5`
Expected: FAIL（`LexiconNormalizer` 等が未定義）

- [ ] **Step 4: 実装を書く**

`server/src/harness/signal.rs` の実装部:

```rust
use crate::resolve::normalize_key;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// 標準化された条件語（specs/signal-vocabulary.md の語彙）。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct Signal(String);

impl Signal {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub type SignalSet = BTreeSet<Signal>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalClass {
    Hazard,
    Context,
}

/// 軸1（表現ゆれの吸収）。Step 1 は決定論 lexicon、将来 LLM 実装が同 trait に載る(S1-11)。
pub trait SignalNormalizer: Send + Sync {
    fn normalize(&self, text: &str) -> SignalSet;
}

#[derive(Debug, Deserialize)]
struct LexiconEntry {
    signal: String,
    class: SignalClass,
    surface_forms: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LexiconFile {
    signals: Vec<LexiconEntry>,
}

pub struct LexiconNormalizer {
    entries: Vec<LexiconEntry>,
}

impl LexiconNormalizer {
    pub fn from_path(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read signal lexicon {}", path.display()))?;
        Self::from_json(&body)
    }

    pub fn from_json(body: &str) -> Result<Self> {
        let file: LexiconFile = serde_json::from_str(body).context("parse signal lexicon json")?;
        Ok(Self { entries: file.signals })
    }

    pub fn class_of(&self, signal: &Signal) -> Option<SignalClass> {
        self.entries
            .iter()
            .find(|entry| entry.signal == signal.as_str())
            .map(|entry| entry.class)
    }
}

impl SignalNormalizer for LexiconNormalizer {
    fn normalize(&self, text: &str) -> SignalSet {
        let normalized = normalize_key(text);
        self.entries
            .iter()
            .filter(|entry| {
                entry.surface_forms.iter().any(|form| {
                    let form_norm = normalize_key(form);
                    !form_norm.is_empty() && normalized.contains(&form_norm)
                })
            })
            .map(|entry| Signal::new(&entry.signal))
            .collect()
    }
}
```

- [ ] **Step 5: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::signal 2>&1 | tail -5`
Expected: `test result: ok. 4 passed`

注意: `normalize_key` の正規化仕様（全半角・記号除去）によりテストの期待値がずれた場合は、`resolve.rs` の実装を読み、surface_forms 側を調整する（`normalize_key` は変更しない）。

- [ ] **Step 6: 実ファイル読み込みの確認テストを 1 本追加**

```rust
    #[test]
    fn loads_bundled_lexicon_file() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/signal-lexicon.json");
        let n = LexiconNormalizer::from_path(&path).expect("bundled lexicon loads");
        assert!(n.normalize("変色して色味がおかしい").contains(&Signal::new("discoloration")));
    }
```

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::signal 2>&1 | tail -5`
Expected: `test result: ok. 5 passed`

- [ ] **Step 7: Commit**

```bash
git add server/Cargo.toml server/Cargo.lock server/src/lib.rs server/src/harness/
git commit -m "feat: add harness signal module with deterministic lexicon normalizer"
```

---

### Task 3: authn.rs（JWT HS256 → Actor）+ config 拡張

**Files:**
- Create: `server/src/harness/authn.rs`
- Modify: `server/src/harness/mod.rs`（`pub mod authn;` 追加）
- Modify: `server/src/config.rs`
- Test: `server/src/harness/authn.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::config::ActorConfig`
- Produces: `Claims` / `Role` / `Actor` / `Authenticator::new` / `Authenticator::authenticate`（総覧どおり）

- [ ] **Step 1: config.rs に設定型を追加**

`server/src/config.rs` に追記（既存 `AppConfig` にフィールド追加）:

```rust
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AuthConfig {
    /// HS256 共有鍵ファイルパス。env CS_SUPPORT_JWT_SECRET_FILE で上書き可。
    pub jwt_secret_file: Option<String>,
    /// JWT 未設定時の dev 専用フォールバック actor（sub）。
    pub default_actor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActorConfig {
    pub sub: String,
    pub role: String,
    pub allowed_schemas: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThresholdsConfig {
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

impl Default for ThresholdsConfig {
    fn default() -> Self {
        Self { low: 0.6, mid: 0.8, high: 0.95 }
    }
}

/// grade 昇格・降格のしきい値（S1-11 追記 4）。具体値は S1-9 の未決事項のため
/// config 注入とし、既定値は仮置き。業務確認で確定させる。
#[derive(Debug, Clone, Deserialize)]
pub struct GradingConfig {
    pub promote_approvals: u32,
    pub promote_approvers: u32,
    pub promote_max_rejection_rate: f32,
    pub demote_rejections: u32,
}

impl Default for GradingConfig {
    fn default() -> Self {
        Self {
            promote_approvals: 3,
            promote_approvers: 2,
            promote_max_rejection_rate: 0.2,
            demote_rejections: 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HarnessConfig {
    #[serde(default = "default_audit_log_path")]
    pub audit_log_path: String,
    #[serde(default = "default_queue_path")]
    pub search_improvement_queue_path: String,
    #[serde(default = "default_lexicon_path")]
    pub signal_lexicon_path: String,
    #[serde(default = "default_ng_path")]
    pub ng_dictionary_path: String,
    /// escalate_unless_answerable / answer_unless_blocked
    #[serde(default = "default_policy")]
    pub policy: String,
    #[serde(default)]
    pub thresholds: ThresholdsConfig,
    #[serde(default)]
    pub grading: GradingConfig,
}

fn default_audit_log_path() -> String { "data/audit/audit.jsonl".to_string() }
fn default_queue_path() -> String { "data/audit/search-improvement-queue.jsonl".to_string() }
fn default_lexicon_path() -> String { "data/signal-lexicon.json".to_string() }
fn default_ng_path() -> String { "data/ng-dictionary.json".to_string() }
fn default_policy() -> String { "escalate_unless_answerable".to_string() }

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            audit_log_path: default_audit_log_path(),
            search_improvement_queue_path: default_queue_path(),
            signal_lexicon_path: default_lexicon_path(),
            ng_dictionary_path: default_ng_path(),
            policy: default_policy(),
            thresholds: ThresholdsConfig::default(),
            grading: GradingConfig::default(),
        }
    }
}
```

`AppConfig` に追加:

```rust
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub actors: Vec<ActorConfig>,
    #[serde(default)]
    pub harness: HarnessConfig,
```

`AppConfig::load` 末尾（`Ok(config)` の前）に env 上書きを追加:

```rust
        if let Ok(path) = env::var("CS_SUPPORT_JWT_SECRET_FILE") {
            config.auth.jwt_secret_file = Some(path);
        }
```

- [ ] **Step 2: 失敗するテストを書く**

`server/src/harness/authn.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ActorConfig;
    use jsonwebtoken::{encode, EncodingKey, Header};

    const SECRET: &[u8] = b"test-secret";

    fn actors() -> Vec<ActorConfig> {
        vec![ActorConfig {
            sub: "op-001".to_string(),
            role: "operator".to_string(),
            allowed_schemas: vec!["sivira-cs-demo".to_string()],
        }]
    }

    fn token(sub: &str, exp_offset_secs: i64) -> String {
        let exp = (chrono::Utc::now().timestamp() + exp_offset_secs) as usize;
        let claims = Claims { sub: sub.to_string(), role: "operator".to_string(), exp, iss: "test".to_string() };
        encode(&Header::default(), &claims, &EncodingKey::from_secret(SECRET)).unwrap()
    }

    #[test]
    fn valid_jwt_resolves_actor() {
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None);
        let actor = auth.authenticate(Some(&format!("Bearer {}", token("op-001", 3600)))).unwrap();
        assert_eq!(actor.sub, "op-001");
        assert_eq!(actor.role, Role::Operator);
        assert_eq!(actor.allowed_schemas, vec!["sivira-cs-demo".to_string()]);
    }

    #[test]
    fn expired_jwt_is_rejected() {
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None);
        assert!(auth.authenticate(Some(&format!("Bearer {}", token("op-001", -3600)))).is_err());
    }

    #[test]
    fn unknown_sub_is_rejected() {
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None);
        assert!(auth.authenticate(Some(&format!("Bearer {}", token("ghost", 3600)))).is_err());
    }

    #[test]
    fn missing_header_is_rejected_when_secret_configured() {
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None);
        assert!(auth.authenticate(None).is_err());
    }

    #[test]
    fn tampered_jwt_is_rejected() {
        let auth = Authenticator::new(Some(b"other-secret".to_vec()), &actors(), None);
        assert!(auth.authenticate(Some(&format!("Bearer {}", token("op-001", 3600)))).is_err());
    }

    #[test]
    fn default_actor_fallback_only_without_secret() {
        let auth = Authenticator::new(None, &actors(), Some("op-001".to_string()));
        let actor = auth.authenticate(None).unwrap();
        assert_eq!(actor.sub, "op-001");
        // secret も default_actor も無ければエラー
        let strict = Authenticator::new(None, &actors(), None);
        assert!(strict.authenticate(None).is_err());
    }
}
```

- [ ] **Step 3: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::authn 2>&1 | tail -5`
Expected: FAIL（コンパイルエラー）

- [ ] **Step 4: 実装を書く**

```rust
use crate::config::ActorConfig;
use anyhow::{anyhow, bail, Context, Result};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub role: String,
    pub exp: usize,
    pub iss: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Operator,
    Supervisor,
    Admin,
}

impl std::str::FromStr for Role {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "operator" => Ok(Role::Operator),
            "supervisor" => Ok(Role::Supervisor),
            "admin" => Ok(Role::Admin),
            other => bail!("unknown actor role: {other}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Actor {
    pub sub: String,
    pub role: Role,
    pub allowed_schemas: Vec<String>,
}

pub struct Authenticator {
    decoding_key: Option<DecodingKey>,
    actors: HashMap<String, ActorConfig>,
    default_actor: Option<String>,
}

impl Authenticator {
    pub fn new(secret: Option<Vec<u8>>, actors: &[ActorConfig], default_actor: Option<String>) -> Self {
        Self {
            decoding_key: secret.map(|s| DecodingKey::from_secret(&s)),
            actors: actors.iter().map(|a| (a.sub.clone(), a.clone())).collect(),
            default_actor,
        }
    }

    /// Authorization ヘッダから actor を確定する（S1-1 の [認証]）。
    pub fn authenticate(&self, authorization: Option<&str>) -> Result<Actor> {
        match (&self.decoding_key, authorization) {
            (Some(key), Some(header)) => {
                let token = header
                    .strip_prefix("Bearer ")
                    .ok_or_else(|| anyhow!("authorization header is not a bearer token"))?;
                let mut validation = Validation::new(Algorithm::HS256);
                validation.set_required_spec_claims(&["exp", "sub", "iss"]);
                let data = decode::<Claims>(token, key, &validation).context("invalid jwt")?;
                self.lookup(&data.claims.sub)
            }
            (Some(_), None) => Err(anyhow!("missing authorization header")),
            (None, _) => {
                let sub = self
                    .default_actor
                    .as_deref()
                    .ok_or_else(|| anyhow!("jwt secret is not configured and no default_actor is set"))?;
                tracing::warn!(sub, "jwt secret not configured; falling back to default_actor (dev only)");
                self.lookup(sub)
            }
        }
    }

    fn lookup(&self, sub: &str) -> Result<Actor> {
        let config = self
            .actors
            .get(sub)
            .ok_or_else(|| anyhow!("actor not registered: {sub}"))?;
        Ok(Actor {
            sub: config.sub.clone(),
            role: config.role.parse()?,
            allowed_schemas: config.allowed_schemas.clone(),
        })
    }
}
```

`server/src/harness/mod.rs` に `pub mod authn;` を追加。

- [ ] **Step 5: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::authn 2>&1 | tail -5`
Expected: `test result: ok. 6 passed`

- [ ] **Step 6: Commit**

```bash
git add server/src/harness/ server/src/config.rs
git commit -m "feat: add JWT HS256 actor authentication with config actor table"
```

---

### Task 4: scope.rs（AccessScope / resolve_scope、I1）

**Files:**
- Create: `server/src/harness/scope.rs`
- Modify: `server/src/harness/mod.rs`（`pub mod scope;` 追加）
- Test: `server/src/harness/scope.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `harness::authn::{Actor, Role}`
- Produces: `AccessScope` / `resolve_scope` / `AccessScope::enforced_schema`（総覧どおり）

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::authn::{Actor, Role};

    fn actor(schemas: &[&str]) -> Actor {
        Actor {
            sub: "op-001".to_string(),
            role: Role::Operator,
            allowed_schemas: schemas.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn allowed_schema_yields_scope() {
        let scope = resolve_scope(&actor(&["sivira-cs-demo"]), "sivira-cs-demo").unwrap();
        assert_eq!(scope.enforced_schema(), "sivira-cs-demo");
        // Step 1 の構造予約: 空で存在する（S1-1）
        assert!(scope.max_sensitivity.is_none());
        assert!(scope.label_allowlist.is_none());
    }

    #[test]
    fn disallowed_schema_is_rejected() {
        assert!(resolve_scope(&actor(&["other-tenant"]), "sivira-cs-demo").is_err());
    }

    #[test]
    fn scope_is_deterministic() {
        let a = resolve_scope(&actor(&["sivira-cs-demo"]), "sivira-cs-demo").unwrap();
        let b = resolve_scope(&actor(&["sivira-cs-demo"]), "sivira-cs-demo").unwrap();
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::scope 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 3: 実装を書く**

```rust
use crate::harness::authn::Actor;
use anyhow::{anyhow, Result};
use serde::Serialize;

/// サーバ導出の認可 scope（I1）。client 入力からは決して作らない。
/// Step 1 の実効 scope は allowed_schemas のみ。max_sensitivity / label_allowlist は
/// 構造予約であり判定に使わない（S1-1）。
#[derive(Debug, Clone, Serialize)]
pub struct AccessScope {
    pub allowed_schemas: Vec<String>,
    pub max_sensitivity: Option<String>,
    pub label_allowlist: Option<Vec<String>>,
}

impl AccessScope {
    /// PunkRecord 検索に必ず注入する schema。tenant=schema 隔離（S1-9 確定 (b)）。
    pub fn enforced_schema(&self) -> &str {
        &self.allowed_schemas[0]
    }
}

/// actor + project から deterministic に scope を算出する（(A) 経路封鎖ハーネス）。
pub fn resolve_scope(actor: &Actor, project_schema: &str) -> Result<AccessScope> {
    if !actor.allowed_schemas.iter().any(|s| s == project_schema) {
        return Err(anyhow!(
            "actor {} is not allowed to access schema {project_schema}",
            actor.sub
        ));
    }
    Ok(AccessScope {
        allowed_schemas: vec![project_schema.to_string()],
        max_sensitivity: None,
        label_allowlist: None,
    })
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::scope 2>&1 | tail -5`
Expected: `test result: ok. 3 passed`

- [ ] **Step 5: Commit**

```bash
git add server/src/harness/
git commit -m "feat: add deterministic access scope resolution (server-derived, I1)"
```

---

### Task 5: rules.rs（論理型 + 第1/2層照合 + match_known_resolution）

**Files:**
- Create: `server/src/harness/rules.rs`
- Modify: `server/src/harness/mod.rs`（`pub mod rules;` 追加）
- Test: `server/src/harness/rules.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `harness::signal::{Signal, SignalSet}`、`crate::resolve::normalize_key`
- Produces: `Binding` / `Grade` / `SourceAuthority` / `RootCause` / `EscalationRule` / `ProhibitedDomain` / `KnownResolution` / `match_layer1` / `match_layer2` / `KrMatch` / `match_known_resolution`（総覧どおり）

- [ ] **Step 1: 失敗するテストを書く**

仕様の急所（S1-3 照合アルゴリズム・変色/変色+カビの例・例外ルール優先）をテストで固定する。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::signal::Signal;

    fn signals(values: &[&str]) -> SignalSet {
        values.iter().map(|v| Signal::new(*v)).collect()
    }

    fn kr(id: &str, set: &[&str], answer: &str) -> KnownResolution {
        KnownResolution {
            id: id.to_string(),
            signal_set: signals(set),
            applicability: "全ロット".to_string(),
            answer: answer.to_string(),
            source_authority: SourceAuthority::Authoritative,
            root_cause: RootCause::KnowledgeError,
            grade: Grade::ApprovalRequired,
            approval_count: 0,
            rejection_count: 0,
            approver_set: Vec::new(),
            origin: "test".to_string(),
            binding: Binding::Advisory,
            registration_trigger: "single_ruling".to_string(),
            knowledge_class: "commercial".to_string(),
            outcome_ref: Vec::new(),
        }
    }

    // --- 第1層 ---

    #[test]
    fn layer1_matches_when_all_condition_signals_present() {
        let rules = vec![EscalationRule {
            id: "r1".to_string(),
            condition: signals(&["post_ingestion_symptom"]),
            route: "safety_team".to_string(),
            owner: None,
            binding: Binding::Mandatory,
        }];
        assert!(match_layer1(&rules, &signals(&["post_ingestion_symptom", "discoloration"])).is_some());
        assert!(match_layer1(&rules, &signals(&["discoloration"])).is_none());
    }

    #[test]
    fn layer1_empty_condition_never_matches() {
        let rules = vec![EscalationRule {
            id: "r0".to_string(),
            condition: SignalSet::new(),
            route: "x".to_string(),
            owner: None,
            binding: Binding::Advisory,
        }];
        assert!(match_layer1(&rules, &signals(&["discoloration"])).is_none());
    }

    // --- 第2層 ---

    #[test]
    fn layer2_matches_by_domain_signal() {
        let domains = vec![ProhibitedDomain {
            id: "d1".to_string(),
            domain_signals: signals(&["skin_irritation"]),
            text_patterns: Vec::new(),
            route: "derm_liaison".to_string(),
            binding: Binding::Mandatory,
        }];
        assert!(match_layer2(&domains, &signals(&["skin_irritation"]), "肌がピリピリする").is_some());
        assert!(match_layer2(&domains, &signals(&["expiry_question"]), "賞味期限は").is_none());
    }

    #[test]
    fn layer2_matches_by_raw_text_pattern_even_without_signal() {
        // lexicon 取りこぼし時のセーフティネット（S1-11）
        let domains = vec![ProhibitedDomain {
            id: "d2".to_string(),
            domain_signals: SignalSet::new(),
            text_patterns: vec!["飲み合わせ".to_string()],
            route: "pharmacist".to_string(),
            binding: Binding::Mandatory,
        }];
        assert!(match_layer2(&domains, &SignalSet::new(), "薬との飲み合わせは大丈夫？").is_some());
    }

    // --- 第3層(a) match_known_resolution ---

    #[test]
    fn kr_exact_match_applies() {
        let resolutions = vec![kr("kr1", &["discoloration"], "自然変色なので問題ありません")];
        match match_known_resolution(&resolutions, &signals(&["discoloration"])) {
            KrMatch::Applicable(found) => assert_eq!(found.id, "kr1"),
            other => panic!("expected Applicable, got {other:?}"),
        }
    }

    #[test]
    fn kr_added_signal_blocks_reuse() {
        // 大前提: 「変色 + カビ」は「変色」ルールの射程外。必ずエスカレーション（S1-3）。
        let resolutions = vec![kr("kr1", &["discoloration"], "自然変色なので問題ありません")];
        match match_known_resolution(&resolutions, &signals(&["discoloration", "mold"])) {
            KrMatch::BlockedByAddedSignal { leftover } => {
                assert!(leftover.contains(&Signal::new("mold")));
            }
            other => panic!("expected BlockedByAddedSignal, got {other:?}"),
        }
    }

    #[test]
    fn kr_more_specific_exception_rule_wins() {
        // 進化: 「変色 + カビ → 廃棄」専用ルールが追加されたら、そちらが優先で適用される。
        let resolutions = vec![
            kr("kr1", &["discoloration"], "自然変色なので問題ありません"),
            kr("kr2", &["discoloration", "mold"], "カビの可能性があるため廃棄してください"),
        ];
        match match_known_resolution(&resolutions, &signals(&["discoloration", "mold"])) {
            KrMatch::Applicable(found) => assert_eq!(found.id, "kr2"),
            other => panic!("expected Applicable(kr2), got {other:?}"),
        }
        // 「変色」だけなら一般ルールは無傷のまま使える
        match match_known_resolution(&resolutions, &signals(&["discoloration"])) {
            KrMatch::Applicable(found) => assert_eq!(found.id, "kr1"),
            other => panic!("expected Applicable(kr1), got {other:?}"),
        }
    }

    #[test]
    fn kr_no_candidate_returns_none() {
        let resolutions = vec![kr("kr1", &["discoloration"], "a")];
        assert!(matches!(
            match_known_resolution(&resolutions, &signals(&["expiry_question"])),
            KrMatch::None
        ));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::rules 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 3: 実装を書く**

```rust
use crate::harness::signal::SignalSet;
use crate::resolve::normalize_key;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    Mandatory,
    Advisory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    ApprovalRequired,
    AutoAnswerAudited,
    Demoted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAuthority {
    Authoritative,
    NonAuthoritative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootCause {
    KnowledgeError,
    RetrievalMiss,
}

/// 第1層: 明示エスカレーションルール（具体条件 → 固有ルーティング）。学習・メモ化しない。
#[derive(Debug, Clone)]
pub struct EscalationRule {
    pub id: String,
    pub condition: SignalSet,
    pub route: String,
    pub owner: Option<String>,
    pub binding: Binding,
}

/// 第2層: 禁止領域（面のブラックリスト）。学習で緩まない絶対線。
#[derive(Debug, Clone)]
pub struct ProhibitedDomain {
    pub id: String,
    pub domain_signals: SignalSet,
    pub text_patterns: Vec<String>,
    pub route: String,
    pub binding: Binding,
}

/// 第3層のルール（S1-3）。正本は PunkRecord の KnownResolution + Signal ノード。
#[derive(Debug, Clone)]
pub struct KnownResolution {
    pub id: String,
    pub signal_set: SignalSet,
    pub applicability: String,
    pub answer: String,
    pub source_authority: SourceAuthority,
    pub root_cause: RootCause,
    pub grade: Grade,
    pub approval_count: u32,
    pub rejection_count: u32,
    pub approver_set: Vec<String>,
    pub origin: String,
    // --- 予約フィールド（Step 1 は既定値のまま。S1-3）---
    pub binding: Binding,
    pub registration_trigger: String,
    pub knowledge_class: String,
    pub outcome_ref: Vec<String>,
}

impl KnownResolution {
    pub fn signal_specificity(&self) -> usize {
        self.signal_set.len()
    }
}

/// 第1層照合: rule.condition ⊆ question のとき確定ルーティング。空条件はマッチしない。
pub fn match_layer1<'a>(rules: &'a [EscalationRule], question: &SignalSet) -> Option<&'a EscalationRule> {
    rules
        .iter()
        .find(|rule| !rule.condition.is_empty() && rule.condition.is_subset(question))
}

/// 第2層照合: signal 一致 or raw text パターン一致で必ず止める（面で塞ぐ）。
pub fn match_layer2<'a>(
    domains: &'a [ProhibitedDomain],
    question: &SignalSet,
    raw_text: &str,
) -> Option<&'a ProhibitedDomain> {
    let raw_norm = normalize_key(raw_text);
    domains.iter().find(|domain| {
        domain.domain_signals.iter().any(|s| question.contains(s))
            || domain.text_patterns.iter().any(|pattern| {
                let p = normalize_key(pattern);
                !p.is_empty() && raw_norm.contains(&p)
            })
    })
}

#[derive(Debug)]
pub enum KrMatch<'a> {
    Applicable(&'a KnownResolution),
    /// 既存ルールの subset は一致したが、未知の追加 signal が残った（＝学習の入口）。
    BlockedByAddedSignal { leftover: SignalSet },
    None,
}

/// 第3層(a) 照合（S1-3・Rust 決定論）。包含方向のみ・条件増加で再利用しない。
pub fn match_known_resolution<'a>(
    resolutions: &'a [KnownResolution],
    question: &SignalSet,
) -> KrMatch<'a> {
    let mut applicable: Vec<&KnownResolution> = Vec::new();
    let mut best_blocked: Option<SignalSet> = None;
    for kr in resolutions {
        if kr.signal_set.is_empty() || !kr.signal_set.is_subset(question) {
            continue;
        }
        let leftover: SignalSet = question.difference(&kr.signal_set).cloned().collect();
        if leftover.is_empty() {
            applicable.push(kr);
        } else {
            // より小さい leftover（より具体的な部分一致）を記録する
            let smaller = best_blocked
                .as_ref()
                .is_none_or(|current| leftover.len() < current.len());
            if smaller {
                best_blocked = Some(leftover);
            }
        }
    }
    applicable.sort_by(|a, b| b.signal_specificity().cmp(&a.signal_specificity()));
    if let Some(kr) = applicable.first() {
        return KrMatch::Applicable(kr);
    }
    if let Some(leftover) = best_blocked {
        return KrMatch::BlockedByAddedSignal { leftover };
    }
    KrMatch::None
}
```

注意: `is_none_or` が MSRV で使えない場合は `map_or(true, ...)` に置き換える。

- [ ] **Step 4: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::rules 2>&1 | tail -5`
Expected: `test result: ok. 8 passed`

- [ ] **Step 5: Commit**

```bash
git add server/src/harness/
git commit -m "feat: add layer1/layer2 rule matching and known_resolution set-inclusion matching"
```

---

### Task 6: decision.rs（stakes / threshold / evidence_sufficient / decide）+ grading.rs（regrade）

**Files:**
- Create: `server/src/harness/decision.rs`
- Create: `server/src/harness/grading.rs`
- Modify: `server/src/harness/mod.rs`（`pub mod decision;` / `pub mod grading;` 追加）
- Test: `server/src/harness/decision.rs` / `server/src/harness/grading.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `harness::signal::SignalSet`、`harness::rules::{EscalationRule, ProhibitedDomain, KnownResolution, KrMatch, Grade, match_layer1, match_layer2, match_known_resolution}`
- Produces: `Stakes` / `StakesInput` / `classify_stakes` / `Thresholds` / `answerability_threshold` / `EvidenceRequirement` / `Sufficiency` / `evidence_sufficient` / `AnswerSource` / `EscalateReason` / `DisclosureScope` / `AnswerDecision` / `DecisionInput` / `decide` / `GradingThresholds` / `regrade`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::rules::{
        Binding, EscalationRule, Grade, KnownResolution, ProhibitedDomain, RootCause, SourceAuthority,
    };
    use crate::harness::signal::{Signal, SignalSet};

    fn signals(values: &[&str]) -> SignalSet {
        values.iter().map(|v| Signal::new(*v)).collect()
    }

    fn thresholds() -> Thresholds {
        Thresholds { low: 0.6, mid: 0.8, high: 0.95 }
    }

    fn kr(id: &str, set: &[&str]) -> KnownResolution {
        KnownResolution {
            id: id.to_string(),
            signal_set: signals(set),
            applicability: "全ロット".to_string(),
            answer: "answer".to_string(),
            source_authority: SourceAuthority::Authoritative,
            root_cause: RootCause::KnowledgeError,
            grade: Grade::ApprovalRequired,
            approval_count: 0,
            rejection_count: 0,
            approver_set: Vec::new(),
            origin: "test".to_string(),
            binding: Binding::Advisory,
            registration_trigger: "single_ruling".to_string(),
            knowledge_class: "commercial".to_string(),
            outcome_ref: Vec::new(),
        }
    }

    fn input<'a>(
        question_signals: &'a SignalSet,
        rules: &'a [EscalationRule],
        domains: &'a [ProhibitedDomain],
        resolutions: &'a [KnownResolution],
        best_manual_score: Option<f32>,
        stakes_input: StakesInput,
        thresholds: &'a Thresholds,
        sections: &'a [String],
    ) -> DecisionInput<'a> {
        DecisionInput {
            question_signals,
            question_raw: "質問",
            rules,
            domains,
            resolutions,
            best_manual_score,
            best_manual_sections: sections,
            stakes_input,
            thresholds,
        }
    }

    fn calm() -> StakesInput {
        StakesInput { mandatory_domain_near: false, ng_near_hit: false, hazard_signal_count: 0 }
    }

    // --- stakes / threshold ---

    #[test]
    fn stakes_ladder() {
        assert_eq!(classify_stakes(&StakesInput { mandatory_domain_near: true, ng_near_hit: false, hazard_signal_count: 0 }), Stakes::High);
        assert_eq!(classify_stakes(&StakesInput { mandatory_domain_near: false, ng_near_hit: true, hazard_signal_count: 0 }), Stakes::High);
        assert_eq!(classify_stakes(&StakesInput { mandatory_domain_near: false, ng_near_hit: false, hazard_signal_count: 1 }), Stakes::Mid);
        assert_eq!(classify_stakes(&calm()), Stakes::Low);
    }

    #[test]
    fn threshold_is_monotonic_staircase() {
        let t = thresholds();
        assert!(answerability_threshold(&t, Stakes::Low) < answerability_threshold(&t, Stakes::Mid));
        assert!(answerability_threshold(&t, Stakes::Mid) < answerability_threshold(&t, Stakes::High));
    }

    // --- evidence_sufficient（数でなく直接性・missing を返す純関数）---

    #[test]
    fn evidence_sufficient_returns_missing_details() {
        match evidence_sufficient(0.8, Some(0.5)) {
            Sufficiency::Insufficient { missing } => {
                assert_eq!(missing.len(), 1);
                let EvidenceRequirement::DirectManualCoverage { required, best } = &missing[0];
                assert_eq!(*required, 0.8);
                assert_eq!(*best, 0.5);
            }
            Sufficiency::Sufficient => panic!("expected insufficient"),
        }
        assert!(matches!(evidence_sufficient(0.8, Some(0.9)), Sufficiency::Sufficient));
        assert!(matches!(evidence_sufficient(0.8, None), Sufficiency::Insufficient { .. }));
    }

    // --- decide: 3 層短絡 ---

    #[test]
    fn layer1_short_circuits_everything() {
        // 第1層マッチ時は KR が完全一致でも回答に進まない（バイパス不可）
        let rules = vec![EscalationRule {
            id: "r1".to_string(),
            condition: signals(&["post_ingestion_symptom"]),
            route: "safety_team".to_string(),
            owner: None,
            binding: Binding::Mandatory,
        }];
        let resolutions = vec![kr("kr1", &["post_ingestion_symptom"])];
        let q = signals(&["post_ingestion_symptom"]);
        let d = decide(&input(&q, &rules, &[], &resolutions, Some(1.0), calm(), &thresholds(), &[]));
        match d {
            AnswerDecision::Escalate { layer, route_to, reason, audit_required, .. } => {
                assert_eq!(layer, 1);
                assert_eq!(route_to, "safety_team");
                assert_eq!(reason, EscalateReason::RegulatedOrSafety);
                assert!(audit_required);
            }
            other => panic!("expected layer1 escalate, got {other:?}"),
        }
    }

    #[test]
    fn layer2_blocks_before_layer3() {
        let domains = vec![ProhibitedDomain {
            id: "d1".to_string(),
            domain_signals: signals(&["skin_irritation"]),
            text_patterns: Vec::new(),
            route: "derm_liaison".to_string(),
            binding: Binding::Mandatory,
        }];
        let resolutions = vec![kr("kr1", &["skin_irritation"])];
        let q = signals(&["skin_irritation"]);
        let d = decide(&input(&q, &[], &domains, &resolutions, Some(1.0), calm(), &thresholds(), &[]));
        match d {
            AnswerDecision::Escalate { layer, route_to, .. } => {
                assert_eq!(layer, 2);
                assert_eq!(route_to, "derm_liaison");
            }
            other => panic!("expected layer2 escalate, got {other:?}"),
        }
    }

    #[test]
    fn layer3_reuses_known_resolution() {
        let resolutions = vec![kr("kr1", &["discoloration"])];
        let q = signals(&["discoloration"]);
        let d = decide(&input(&q, &[], &[], &resolutions, None, calm(), &thresholds(), &[]));
        match d {
            AnswerDecision::Allowed { source, known_resolution_id, .. } => {
                assert_eq!(source, AnswerSource::KnownResolution);
                assert_eq!(known_resolution_id.as_deref(), Some("kr1"));
            }
            other => panic!("expected allowed via KR, got {other:?}"),
        }
    }

    #[test]
    fn layer3_added_signal_escalates_with_unknown_added_signal() {
        let resolutions = vec![kr("kr1", &["discoloration"])];
        let q = signals(&["discoloration", "mold"]);
        let d = decide(&input(&q, &[], &[], &resolutions, Some(0.1), calm(), &thresholds(), &[]));
        match d {
            AnswerDecision::Escalate { layer, reason, route_to, .. } => {
                assert_eq!(layer, 3);
                assert_eq!(reason, EscalateReason::UnknownAddedSignal);
                assert_eq!(route_to, "triage");
            }
            other => panic!("expected UnknownAddedSignal escalate, got {other:?}"),
        }
    }

    #[test]
    fn layer3_direct_manual_answers() {
        let q = SignalSet::new();
        let sections = vec!["doc-1#storage".to_string()];
        let d = decide(&input(&q, &[], &[], &[], Some(0.95), calm(), &thresholds(), &sections));
        match d {
            AnswerDecision::Allowed { source, evidence_section_keys, .. } => {
                assert_eq!(source, AnswerSource::Manual);
                assert_eq!(evidence_section_keys, sections);
            }
            other => panic!("expected allowed via manual, got {other:?}"),
        }
    }

    #[test]
    fn high_stakes_raises_threshold_and_escalates() {
        // S1-8 Done 条件 5: 第2層列挙に無くても stakes=high でしきい値が上がり escalate に倒れる
        let q = SignalSet::new();
        let high = StakesInput { mandatory_domain_near: false, ng_near_hit: true, hazard_signal_count: 0 };
        let d = decide(&input(&q, &[], &[], &[], Some(0.9), high, &thresholds(), &[]));
        match d {
            AnswerDecision::Escalate { layer, reason, missing, .. } => {
                assert_eq!(layer, 3);
                assert_eq!(reason, EscalateReason::InsufficientDirectness);
                assert!(!missing.is_empty());
            }
            other => panic!("expected high-stakes escalate, got {other:?}"),
        }
        // 同じ根拠でも low stakes なら答えられる（実用性のダイヤル）
        let d2 = decide(&input(&q, &[], &[], &[], Some(0.9), calm(), &thresholds(), &[]));
        assert!(matches!(d2, AnswerDecision::Allowed { .. }));
    }

    #[test]
    fn decide_is_deterministic() {
        let resolutions = vec![kr("kr1", &["discoloration"])];
        let q = signals(&["discoloration"]);
        let a = decide(&input(&q, &[], &[], &resolutions, Some(0.5), calm(), &thresholds(), &[]));
        let b = decide(&input(&q, &[], &[], &resolutions, Some(0.5), calm(), &thresholds(), &[]));
        assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::decision 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 3: 実装を書く**

```rust
use crate::harness::rules::{
    match_known_resolution, match_layer1, match_layer2, EscalationRule, KnownResolution, KrMatch,
    ProhibitedDomain,
};
use crate::harness::signal::SignalSet;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stakes {
    Low,
    Mid,
    High,
}

/// classify_stakes の入力。3 フラグの算出は Harness（呼び出し側）が決定論に行う。
#[derive(Debug, Clone)]
pub struct StakesInput {
    /// binding=mandatory の禁止領域に近接（signal 交差 or パターン部分ヒット）
    pub mandatory_domain_near: bool,
    /// 決定論 NG 辞書（block/abstain 語）への近接ヒット
    pub ng_near_hit: bool,
    /// lexicon class=hazard の signal 数
    pub hazard_signal_count: usize,
}

/// S1-6: stakes 3 段離散。将来 Π 連続変調に差し替わる（呼び出し側は不変）。
pub fn classify_stakes(input: &StakesInput) -> Stakes {
    if input.mandatory_domain_near || input.ng_near_hit {
        Stakes::High
    } else if input.hazard_signal_count > 0 {
        Stakes::Mid
    } else {
        Stakes::Low
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Thresholds {
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

/// S1-6: 3 段の階段関数。第3層の可否比較は「threshold を受け取って比較」のみ。
pub fn answerability_threshold(thresholds: &Thresholds, stakes: Stakes) -> f32 {
    match stakes {
        Stakes::Low => thresholds.low,
        Stakes::Mid => thresholds.mid,
        Stakes::High => thresholds.high,
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EvidenceRequirement {
    DirectManualCoverage { required: f32, best: f32 },
}

#[derive(Debug)]
pub enum Sufficiency {
    Sufficient,
    Insufficient { missing: Vec<EvidenceRequirement> },
}

/// 数でなく直接性で測る。何が足りないか（missing）を返す純関数（spec「evidence_sufficient の定義」）。
pub fn evidence_sufficient(threshold: f32, best_hit_score: Option<f32>) -> Sufficiency {
    let best = best_hit_score.unwrap_or(0.0);
    if best >= threshold {
        Sufficiency::Sufficient
    } else {
        Sufficiency::Insufficient {
            missing: vec![EvidenceRequirement::DirectManualCoverage { required: threshold, best }],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnswerSource {
    KnownResolution,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EscalateReason {
    PermissionDenied,
    RegulatedOrSafety,
    RequiresHumanApproval,
    InsufficientDirectness,
    UnknownAddedSignal,
}

/// 顧客に開示してよい情報の範囲。文面そのものは client（生成側）が作る（spec「message_policy の扱い」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DisclosureScope {
    /// 「担当部署に確認する」旨のみ開示可
    ConfirmingWithTeam,
    /// 内部事情（権限・根拠不足の詳細）を開示しない
    NoInternalDetails,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum AnswerDecision {
    Allowed {
        source: AnswerSource,
        evidence_section_keys: Vec<String>,
        known_resolution_id: Option<String>,
        stakes: Stakes,
        threshold: f32,
    },
    Escalate {
        reason: EscalateReason,
        layer: u8,
        route_to: String,
        disclosure_scope: DisclosureScope,
        audit_required: bool,
        missing: Vec<EvidenceRequirement>,
    },
}

pub struct DecisionInput<'a> {
    pub question_signals: &'a SignalSet,
    pub question_raw: &'a str,
    pub rules: &'a [EscalationRule],
    pub domains: &'a [ProhibitedDomain],
    pub resolutions: &'a [KnownResolution],
    pub best_manual_score: Option<f32>,
    pub best_manual_sections: &'a [String],
    pub stakes_input: StakesInput,
    pub thresholds: &'a Thresholds,
}

/// (B) 3 層判定の decision function。LLM 非介在・同じ入力なら必ず同じ判定（純関数）。
/// 先に止まった層で確定し、後段は評価しない。第1・2層にメモ化を適用しない。
pub fn decide(input: &DecisionInput) -> AnswerDecision {
    // 第1層: 明示エスカレーションルール
    if let Some(rule) = match_layer1(input.rules, input.question_signals) {
        return AnswerDecision::Escalate {
            reason: EscalateReason::RegulatedOrSafety,
            layer: 1,
            route_to: rule.route.clone(),
            disclosure_scope: DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
        };
    }
    // 第2層: 禁止領域
    if let Some(domain) = match_layer2(input.domains, input.question_signals, input.question_raw) {
        return AnswerDecision::Escalate {
            reason: EscalateReason::RegulatedOrSafety,
            layer: 2,
            route_to: domain.route.clone(),
            disclosure_scope: DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
        };
    }
    // 第3層: 回答可能性
    let stakes = classify_stakes(&input.stakes_input);
    let threshold = answerability_threshold(input.thresholds, stakes);
    let kr_match = match_known_resolution(input.resolutions, input.question_signals);
    if let KrMatch::Applicable(kr) = kr_match {
        return AnswerDecision::Allowed {
            source: AnswerSource::KnownResolution,
            evidence_section_keys: Vec::new(),
            known_resolution_id: Some(kr.id.clone()),
            stakes,
            threshold,
        };
    }
    match evidence_sufficient(threshold, input.best_manual_score) {
        Sufficiency::Sufficient => AnswerDecision::Allowed {
            source: AnswerSource::Manual,
            evidence_section_keys: input.best_manual_sections.to_vec(),
            known_resolution_id: None,
            stakes,
            threshold,
        },
        Sufficiency::Insufficient { missing } => {
            let reason = if matches!(kr_match, KrMatch::BlockedByAddedSignal { .. }) {
                EscalateReason::UnknownAddedSignal
            } else {
                EscalateReason::InsufficientDirectness
            };
            AnswerDecision::Escalate {
                reason,
                layer: 3,
                route_to: "triage".to_string(),
                disclosure_scope: DisclosureScope::ConfirmingWithTeam,
                audit_required: true,
                missing,
            }
        }
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::decision 2>&1 | tail -5`
Expected: `test result: ok. 9 passed`

- [ ] **Step 5: grading.rs の失敗するテストを書く**

spec ロードマップ遵守事項 3: grade を Step 1 から運用する。しきい値は config 注入（S1-11 追記 4 の仮置き値が既定）。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::rules::Grade;

    fn t() -> GradingThresholds {
        GradingThresholds {
            promote_approvals: 3,
            promote_approvers: 2,
            promote_max_rejection_rate: 0.2,
            demote_rejections: 2,
        }
    }

    #[test]
    fn promotes_when_all_conditions_met() {
        // 承認 3・承認者 2 名・却下率 0 → 自動回答(事後監査)へ格上げ
        assert_eq!(regrade(Grade::ApprovalRequired, 3, 0, 2, &t()), Grade::AutoAnswerAudited);
    }

    #[test]
    fn does_not_promote_on_single_approver() {
        // 承認者多様性が閾値未満なら量が積もっても昇格しない
        assert_eq!(regrade(Grade::ApprovalRequired, 10, 0, 1, &t()), Grade::ApprovalRequired);
    }

    #[test]
    fn does_not_promote_on_high_rejection_rate() {
        // 承認 3・却下 1 → 却下率 0.25 > 0.2 で昇格しない
        assert_eq!(regrade(Grade::ApprovalRequired, 3, 1, 2, &t()), Grade::ApprovalRequired);
    }

    #[test]
    fn demotes_promoted_resolution_on_rejections() {
        // 一方通行にしない: 格上げ済みでも却下が閾値に達したら承認必須へ戻す
        assert_eq!(regrade(Grade::AutoAnswerAudited, 5, 2, 3, &t()), Grade::Demoted);
    }

    #[test]
    fn demoted_stays_until_repromoted() {
        // 降格中は昇格条件を満たし直すまで approval_required 相当として扱う
        assert_eq!(regrade(Grade::Demoted, 3, 1, 2, &t()), Grade::Demoted);
    }

    #[test]
    fn regrade_is_deterministic() {
        assert_eq!(
            regrade(Grade::ApprovalRequired, 3, 0, 2, &t()),
            regrade(Grade::ApprovalRequired, 3, 0, 2, &t())
        );
    }
}
```

- [ ] **Step 6: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::grading 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 7: grading.rs を実装する**

```rust
use crate::harness::rules::Grade;

/// 昇格・降格しきい値（S1-11 追記 4）。具体値は未決のため config [harness.grading] で注入。
#[derive(Debug, Clone)]
pub struct GradingThresholds {
    pub promote_approvals: u32,
    pub promote_approvers: u32,
    pub promote_max_rejection_rate: f32,
    pub demote_rejections: u32,
}

/// grade の昇格・降格判定（決定論・純関数）。
/// 深さ方向（経験済みパターンの自動化）は量で進み、降格で一方通行にしない（spec「昇格・降格」）。
/// Step 1 の利用者は担当者のため応答セマンティクスは変わらないが、Step 2 の
/// 「顧客直に即答してよいか」の判定材料としてここから運用する（遵守事項 3）。
pub fn regrade(
    current: Grade,
    approval_count: u32,
    rejection_count: u32,
    approver_count: usize,
    thresholds: &GradingThresholds,
) -> Grade {
    // 降格条件を先に評価する（安全側優先）
    if matches!(current, Grade::AutoAnswerAudited) && rejection_count >= thresholds.demote_rejections {
        return Grade::Demoted;
    }
    if matches!(current, Grade::Demoted) {
        // 降格中の再昇格は Step 1 では自動化しない（人手の見直しを経る）
        return Grade::Demoted;
    }
    let total = approval_count + rejection_count;
    let rejection_rate = if total == 0 { 0.0 } else { rejection_count as f32 / total as f32 };
    if approval_count >= thresholds.promote_approvals
        && approver_count >= thresholds.promote_approvers as usize
        && rejection_rate <= thresholds.promote_max_rejection_rate
    {
        return Grade::AutoAnswerAudited;
    }
    current
}
```

`server/src/harness/mod.rs` に `pub mod grading;` を追加。

- [ ] **Step 8: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::grading harness::decision 2>&1 | tail -5`
Expected: grading 6 passed + decision 9 passed

- [ ] **Step 9: Commit**

```bash
git add server/src/harness/
git commit -m "feat: add deterministic 3-layer answer decision, stakes staircase, and grade promotion/demotion"
```

---

### Task 7: egress.rs（出口ゲート + NG 辞書）

**Files:**
- Create: `server/src/harness/egress.rs`
- Modify: `server/src/harness/mod.rs`（`pub mod egress;` 追加）
- Test: `server/src/harness/egress.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::resolve::normalize_key`
- Produces: `NgDictionary` / `EmitChannel` / `EmitContext` / `EgressVerdict` / `egress_gate`

設計上の遵守（spec ロードマップ遵守事項 2・4）: `egress_gate` の第 1 引数は**任意のテキスト断片**（全文でも文単位でも可）。`EmitContext` はチャネル種別 `{operator, customer_chat, customer_voice}` を最初から持つが、Step 1 の呼び出しは全て operator・全文。channel によるゲート挙動の分岐は Step 1 では入れない（判定水準はフェーズ・チャネルで変えないのが不変条件）。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ng() -> NgDictionary {
        NgDictionary::from_json(
            r#"{ "block_terms": ["必ず治ります", "副作用はありません"],
                 "abstain_terms": ["治る", "痩せる"] }"#,
        )
        .expect("ng dictionary parses")
    }

    fn operator() -> EmitContext {
        EmitContext { channel: EmitChannel::Operator }
    }

    #[test]
    fn explicit_ng_word_blocks() {
        match egress_gate("この商品で必ず治りますのでご安心ください", &operator(), &ng()) {
            EgressVerdict::Block { term } => assert_eq!(term, "必ず治ります"),
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn implied_efficacy_abstains() {
        // 暗示効能は「検出して通す」でなく「疑わしきは出さない」（S1-4）
        match egress_gate("継続すると治ると言われています", &operator(), &ng()) {
            EgressVerdict::Abstain { term } => assert_eq!(term, "治る"),
            other => panic!("expected abstain, got {other:?}"),
        }
    }

    #[test]
    fn block_takes_precedence_over_abstain() {
        assert!(matches!(
            egress_gate("必ず治りますし痩せます", &operator(), &ng()),
            EgressVerdict::Block { .. }
        ));
    }

    #[test]
    fn clean_draft_passes() {
        assert!(matches!(
            egress_gate("保存方法は直射日光を避けて常温で保管してください", &operator(), &ng()),
            EgressVerdict::Pass
        ));
    }

    #[test]
    fn gate_accepts_sentence_fragments() {
        // 入力単位を全文に固定しない（Step 3 は文単位で呼ぶ。遵守事項 2）
        assert!(matches!(egress_gate("治る", &operator(), &ng()), EgressVerdict::Abstain { .. }));
        assert!(matches!(egress_gate("", &operator(), &ng()), EgressVerdict::Pass));
    }

    #[test]
    fn verdict_is_channel_invariant() {
        // 判定水準はチャネルで変えない（フェーズ不変条件）
        for channel in [EmitChannel::Operator, EmitChannel::CustomerChat, EmitChannel::CustomerVoice] {
            let ctx = EmitContext { channel };
            assert!(matches!(
                egress_gate("必ず治ります", &ctx, &ng()),
                EgressVerdict::Block { .. }
            ));
        }
    }

    #[test]
    fn near_hit_detects_question_proximity() {
        assert!(ng().near_hit("これを飲むと治るのでしょうか"));
        assert!(!ng().near_hit("送料はいくらですか"));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::egress 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 3: 実装を書く**

```rust
use crate::resolve::normalize_key;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// 決定論 NG 辞書（S1-4）。block = 明示 NG 語、abstain = 暗示効能リスク語。
#[derive(Debug, Clone, Deserialize)]
pub struct NgDictionary {
    pub block_terms: Vec<String>,
    pub abstain_terms: Vec<String>,
}

impl NgDictionary {
    pub fn from_path(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read ng dictionary {}", path.display()))?;
        Self::from_json(&body)
    }

    pub fn from_json(body: &str) -> Result<Self> {
        serde_json::from_str(body).context("parse ng dictionary json")
    }

    /// 質問文が NG 語に近接しているか（stakes=high 判定用、S1-6）。
    pub fn near_hit(&self, text: &str) -> bool {
        let norm = normalize_key(text);
        self.block_terms
            .iter()
            .chain(self.abstain_terms.iter())
            .any(|term| {
                let t = normalize_key(term);
                !t.is_empty() && norm.contains(&t)
            })
    }
}

/// チャネル種別（spec S1-4 / ロードマップ遵守事項 4）。Step 1 は Operator 固定。
/// Step 2 で CustomerChat、Step 3 で CustomerVoice が使われる。判定水準は変えない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmitChannel {
    Operator,
    CustomerChat,
    CustomerVoice,
}

#[derive(Debug, Clone)]
pub struct EmitContext {
    pub channel: EmitChannel,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum EgressVerdict {
    Pass,
    Block { term: String },
    Abstain { term: String },
}

/// 出口ゲート（S1-4）。AI 製・人間製を問わず全 outbound がここを通る（egress 位置固定）。
/// `text` は任意の断片（全文でも文単位でも可。入力単位を固定しない = 遵守事項 2）。
/// Step 1 の判定は channel 非依存（水準はチャネルで変えない）。ctx は将来の
/// ハードゲート/勧告分岐（自動送信 vs 有人音声）のための構造予約。
/// 将来 C′（含意判定）+ Ψ に中身が差し替わっても、この関数境界は不変。
pub fn egress_gate(text: &str, _ctx: &EmitContext, ng: &NgDictionary) -> EgressVerdict {
    let norm = normalize_key(text);
    let contains = |term: &String| {
        let t = normalize_key(term);
        !t.is_empty() && norm.contains(&t)
    };
    if let Some(term) = ng.block_terms.iter().find(|t| contains(t)) {
        return EgressVerdict::Block { term: term.clone() };
    }
    if let Some(term) = ng.abstain_terms.iter().find(|t| contains(t)) {
        return EgressVerdict::Abstain { term: term.clone() };
    }
    EgressVerdict::Pass
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::egress 2>&1 | tail -5`
Expected: `test result: ok. 7 passed`

- [ ] **Step 5: Commit**

```bash
git add server/src/harness/
git commit -m "feat: add deterministic egress gate with NG dictionary block/abstain"
```

---

### Task 8: correction.rs（訂正インテーク）

**Files:**
- Create: `server/src/harness/correction.rs`
- Modify: `server/src/harness/mod.rs`（`pub mod correction;` 追加）
- Test: `server/src/harness/correction.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `harness::rules::{SourceAuthority, RootCause}`
- Produces: `CorrectionRouting` / `correction_intake`

root_cause の切り分け（再検索）は Task 11 の Harness 層が行い、この関数は純関数の分岐のみを持つ（単一の入口関数・将来 CIRG の判定軸をここに足す、S1-5）。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::rules::{RootCause, SourceAuthority};

    #[test]
    fn non_authoritative_never_persists() {
        // 最優先・例外なし（S1-5 不変条件）: エンド顧客の訂正は永続層に書かない
        assert_eq!(
            correction_intake(SourceAuthority::NonAuthoritative, RootCause::KnowledgeError),
            CorrectionRouting::ConversationOnly
        );
        assert_eq!(
            correction_intake(SourceAuthority::NonAuthoritative, RootCause::RetrievalMiss),
            CorrectionRouting::ConversationOnly
        );
    }

    #[test]
    fn retrieval_miss_goes_to_search_improvement() {
        // S1-8 Done 条件 4: retrieval_miss は known_resolution を増やさない
        assert_eq!(
            correction_intake(SourceAuthority::Authoritative, RootCause::RetrievalMiss),
            CorrectionRouting::SearchImprovementQueue
        );
    }

    #[test]
    fn authoritative_knowledge_error_becomes_kr_candidate() {
        assert_eq!(
            correction_intake(SourceAuthority::Authoritative, RootCause::KnowledgeError),
            CorrectionRouting::KnownResolutionCandidate
        );
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::correction 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 3: 実装を書く**

```rust
use crate::harness::rules::{RootCause, SourceAuthority};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionRouting {
    /// 会話内修復のみ。永続層に一切書かない。
    ConversationOnly,
    /// 検索改善キューへ（表記追加 / re-rank）。known_resolution を増やさない。
    SearchImprovementQueue,
    /// known_resolution の追加候補（例外ルールの離散 insert）。
    KnownResolutionCandidate,
}

/// 訂正インテーク（S1-5）。CIRG 6 判定のうち Step 1 は source_authority / root_cause の 2 軸。
/// 将来の判定軸（error_axis / binding / direction / owner / route）はこの関数に足す。入口の位置は変えない。
pub fn correction_intake(authority: SourceAuthority, root_cause: RootCause) -> CorrectionRouting {
    match authority {
        // source_authority=non_authoritative はいかなる永続層へも書けない（最優先・例外なし）
        SourceAuthority::NonAuthoritative => CorrectionRouting::ConversationOnly,
        SourceAuthority::Authoritative => match root_cause {
            RootCause::RetrievalMiss => CorrectionRouting::SearchImprovementQueue,
            RootCause::KnowledgeError => CorrectionRouting::KnownResolutionCandidate,
        },
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::correction 2>&1 | tail -5`
Expected: `test result: ok. 3 passed`

- [ ] **Step 5: Commit**

```bash
git add server/src/harness/
git commit -m "feat: add correction intake with source authority and root cause routing"
```

---

### Task 9: audit.rs（別建て WORM: append-only JSONL + hash chain）

**Files:**
- Create: `server/src/harness/audit.rs`
- Modify: `server/src/harness/mod.rs`（`pub mod audit;` 追加）
- Test: `server/src/harness/audit.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `harness::scope::AccessScope`
- Produces: `AuditDraft` / `WormAuditLog::open` / `WormAuditLog::append`

I5: opaque blob にしない。provenance キー（`event_id / timestamp / request_id / schema / generation / actor / used_scope / retrieved_node_ids[] / decision / route / governing_norm_ids[]`）を構造フィールドで持ち、`graph_provenance_linked: bool` を予約する。改竄検知のため hash chain（`prev_hash` / `hash`）を付ける。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::scope::AccessScope;

    fn scope() -> AccessScope {
        AccessScope {
            allowed_schemas: vec!["sivira-cs-demo".to_string()],
            max_sensitivity: None,
            label_allowlist: None,
        }
    }

    fn draft(request_id: &str, decision: &str) -> AuditDraft {
        AuditDraft {
            request_id: request_id.to_string(),
            schema: "sivira-cs-demo".to_string(),
            actor: "op-001".to_string(),
            used_scope: scope(),
            retrieved_node_ids: vec!["sivira-cs-demo#gen1/section:doc-1#storage".to_string()],
            decision: decision.to_string(),
            route: None,
            governing_norm_ids: Vec::new(),
        }
    }

    #[test]
    fn append_writes_provenance_keyed_jsonl_with_hash_chain() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open worm log");
        let id1 = log.append(draft("req-1", "allowed")).expect("append 1");
        let id2 = log.append(draft("req-2", "escalate")).expect("append 2");
        assert_ne!(id1, id2);

        let body = std::fs::read_to_string(&path).expect("read log");
        let lines: Vec<serde_json::Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is json"))
            .collect();
        assert_eq!(lines.len(), 2);
        // provenance キーが構造化されている（I5、opaque blob でない）
        for line in &lines {
            for key in [
                "event_id", "timestamp", "request_id", "schema", "actor", "used_scope",
                "retrieved_node_ids", "decision", "governing_norm_ids",
                "graph_provenance_linked", "prev_hash", "hash",
            ] {
                assert!(line.get(key).is_some(), "missing key {key}");
            }
        }
        // hash chain: 2 行目の prev_hash は 1 行目の hash
        assert_eq!(lines[1]["prev_hash"], lines[0]["hash"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_continues_hash_chain() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let first_hash;
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append");
            let body = std::fs::read_to_string(&path).unwrap();
            let v: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
            first_hash = v["hash"].as_str().unwrap().to_string();
        }
        // 再オープン（プロセス再起動相当）でもチェーンが繋がる
        let log = WormAuditLog::open(&path).expect("reopen");
        log.append(draft("req-2", "escalate")).expect("append");
        let body = std::fs::read_to_string(&path).unwrap();
        let last: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
        assert_eq!(last["prev_hash"].as_str().unwrap(), first_hash);
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::audit 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 3: 実装を書く**

```rust
use crate::harness::scope::AccessScope;
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 監査イベントの入力（Harness が組み立てる）。
#[derive(Debug, Clone)]
pub struct AuditDraft {
    pub request_id: String,
    pub schema: String,
    pub actor: String,
    pub used_scope: AccessScope,
    pub retrieved_node_ids: Vec<String>,
    pub decision: String,
    pub route: Option<String>,
    pub governing_norm_ids: Vec<String>,
}

/// WORM に書かれる 1 行（I5: provenance キー付き構造化レコード）。
#[derive(Debug, Serialize)]
struct AuditEvent<'a> {
    event_id: &'a str,
    timestamp: &'a str,
    request_id: &'a str,
    schema: &'a str,
    /// PunkRecord generation。Step 1 では node_id に gen prefix が含まれるため None。
    generation: Option<i64>,
    actor: &'a str,
    used_scope: &'a AccessScope,
    retrieved_node_ids: &'a [String],
    decision: &'a str,
    route: Option<&'a str>,
    governing_norm_ids: &'a [String],
    /// 将来 A / traceable_pairs へ結線するための予約（駆動は後段）。
    graph_provenance_linked: bool,
    prev_hash: &'a str,
    hash: &'a str,
}

/// 別建て WORM ストア（S1-2 / S1-8 条件 8）。append-only JSONL + hash chain。
pub struct WormAuditLog {
    path: PathBuf,
    state: Mutex<(File, String)>, // (append-only file, prev_hash)
}

impl WormAuditLog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create audit dir {}", parent.display()))?;
        }
        // 既存ログの末尾から hash chain を復元する
        let prev_hash = match File::open(path) {
            Ok(existing) => BufReader::new(existing)
                .lines()
                .filter_map(|line| line.ok())
                .filter(|line| !line.trim().is_empty())
                .last()
                .and_then(|line| {
                    serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|v| v.get("hash").and_then(|h| h.as_str()).map(ToString::to_string))
                })
                .unwrap_or_else(genesis_hash),
            Err(_) => genesis_hash(),
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open audit log {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new((file, prev_hash)),
        })
    }

    /// イベントを追記し event_id を返す。削除・更新 API は存在しない（WORM）。
    pub fn append(&self, draft: AuditDraft) -> Result<String> {
        let event_id = uuid::Uuid::new_v4().to_string();
        let timestamp = chrono::Utc::now().to_rfc3339();
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("audit log mutex poisoned"))?;
        let (file, prev_hash) = &mut *guard;
        // hash = SHA256(prev_hash + 本文 JSON) — 改竄検知用チェーン
        let payload = serde_json::json!({
            "event_id": event_id,
            "timestamp": timestamp,
            "request_id": draft.request_id,
            "schema": draft.schema,
            "generation": serde_json::Value::Null,
            "actor": draft.actor,
            "used_scope": draft.used_scope,
            "retrieved_node_ids": draft.retrieved_node_ids,
            "decision": draft.decision,
            "route": draft.route,
            "governing_norm_ids": draft.governing_norm_ids,
            "graph_provenance_linked": false,
        });
        let payload_text = serde_json::to_string(&payload)?;
        let mut hasher = Sha256::new();
        hasher.update(prev_hash.as_bytes());
        hasher.update(payload_text.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let event = AuditEvent {
            event_id: &event_id,
            timestamp: &timestamp,
            request_id: &draft.request_id,
            schema: &draft.schema,
            generation: None,
            actor: &draft.actor,
            used_scope: &draft.used_scope,
            retrieved_node_ids: &draft.retrieved_node_ids,
            decision: &draft.decision,
            route: draft.route.as_deref(),
            governing_norm_ids: &draft.governing_norm_ids,
            graph_provenance_linked: false,
            prev_hash,
            hash: &hash,
        };
        let line = serde_json::to_string(&event)?;
        writeln!(file, "{line}").with_context(|| format!("append audit log {}", self.path.display()))?;
        file.flush().context("flush audit log")?;
        *prev_hash = hash;
        Ok(event_id)
    }
}

fn genesis_hash() -> String {
    format!("{:x}", Sha256::digest(b"cs-support-mcp-worm-genesis"))
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::audit 2>&1 | tail -5`
Expected: `test result: ok. 2 passed`

- [ ] **Step 5: `.gitignore` に監査ログ出力先を追加**

`.gitignore` に追記:

```text
server/data/audit/
```

- [ ] **Step 6: Commit**

```bash
git add server/src/harness/ .gitignore
git commit -m "feat: add append-only WORM audit log with provenance keys and hash chain"
```

---

### Task 10: schema 加算 + knowledge.rs（KnowledgeStore）+ rules fixture + ingest_rules CLI

**Files:**
- Modify: `schema/cs-schema.yml`（加算のみ）
- Create: `server/src/harness/knowledge.rs`
- Create: `server/data/rules.sample.json`
- Create: `server/src/bin/ingest_rules.rs`
- Modify: `server/Cargo.toml`（`[[bin]] ingest_rules` 追加）
- Modify: `server/src/harness/mod.rs`（`pub mod knowledge;` 追加）
- Test: `server/src/harness/knowledge.rs` 内 `#[cfg(test)]`（純粋部分のみ。gRPC 統合は Task 14 で実機検証）

**Interfaces:**
- Consumes: `crate::vegapunk::VegapunkClient`（`query_nodes` / `graph_snapshot` / `upsert_nodes` / `upsert_edges` / `create_or_update_schema`）、`crate::ingest::schema_generation_prefix`、`harness::rules::*`、`harness::signal::{Signal, SignalSet}`
- Produces: `KnowledgeStore::new(Arc<VegapunkClient>)`、`load_escalation_rules` / `load_prohibited_domains` / `load_known_resolutions`、`NewKnownResolution` / `insert_known_resolution`、`record`、node id ヘルパ `harness_node_id(schema, kind, key)`

**設計メモ:**
- 新 node type 名は spec S1-2 どおり CamelCase: `KnownResolution` / `Signal` / `EscalationRule` / `ProhibitedDomain`。support 系 record は snake_case: `support_case` / `answer_attempt` / `answer_evidence` / `operator_feedback` / `escalation_event`。
- KnownResolution の signal_set は **属性に畳まない**。`Signal` ノード（`value` 属性）+ `HAS_SIGNAL` 辺が正本（I2）。読み出しは `query_nodes("KnownResolution")` + `graph_snapshot` の HAS_SIGNAL 辺走査（既存 `SnapshotIndex` と同じ手法）。
- EscalationRule / ProhibitedDomain は学習対象外（第1・2層）なので条件を属性（カンマ区切り文字列）で持ってよい。
- 根拠は `KnownResolution -[BECAUSE]-> section` で結線し traceable_pairs に追加。

- [ ] **Step 1: `schema/cs-schema.yml` に加算**

既存 `nodes:` の末尾に追加（既存定義は一切変更しない）:

```yaml
  KnownResolution:
    attributes:
      kr_id: { type: string, required: true }
      answer_text: { type: string, required: true }
      applicability: { type: string }
      grade: { type: string }
      status: { type: string }
      source_authority: { type: string }
      approval_count: { type: int }
      rejection_count: { type: int }
      approver_set: { type: string }
      origin: { type: string }
      created_by: { type: string }
      verified_at: { type: string }
      # --- CIS 互換の予約フィールド（Step 1 は空、将来 CIRG が書く。別紙 §3.2）---
      error_axis: { type: string }
      root_cause: { type: string }
      owner: { type: string }
      binding: { type: string }
      direction: { type: string }
      route: { type: string }
      registration_trigger: { type: string }
      knowledge_class: { type: string }
      outcome_ref: { type: string }
      search_text_ja: { type: string }
  Signal:
    attributes:
      value: { type: string, required: true }
  EscalationRule:
    attributes:
      rule_id: { type: string, required: true }
      condition: { type: string, required: true }   # カンマ区切りの signal 値
      owner: { type: string }
      sensitivity: { type: string }
      route: { type: string, required: true }
      binding: { type: string }
  ProhibitedDomain:
    attributes:
      domain_id: { type: string, required: true }
      pattern: { type: string, required: true }     # カンマ区切りの raw text パターン
      domain_signals: { type: string }              # カンマ区切りの signal 値
      route: { type: string, required: true }
      binding: { type: string }
  support_case:
    attributes:
      case_id: { type: string, required: true }
      request_id: { type: string }
      actor: { type: string }
      question: { type: string }
      product_key: { type: string }
      created_at: { type: string }
  answer_attempt:
    attributes:
      attempt_id: { type: string, required: true }
      case_id: { type: string }
      request_id: { type: string }
      actor: { type: string }
      draft: { type: string }
      decision: { type: string }
      egress_verdict: { type: string }
      audit_event_id: { type: string }
      created_at: { type: string }
  answer_evidence:
    attributes:
      evidence_id: { type: string, required: true }
      attempt_id: { type: string }
      section_key: { type: string }
      kind: { type: string }
  operator_feedback:
    attributes:
      feedback_id: { type: string, required: true }
      attempt_id: { type: string }
      request_id: { type: string }
      actor: { type: string }
      feedback_source: { type: string }
      corrected_answer: { type: string }
      routing: { type: string }
      created_at: { type: string }
  escalation_event:
    attributes:
      escalation_id: { type: string, required: true }
      request_id: { type: string }
      actor: { type: string }
      layer: { type: int }
      reason: { type: string }
      route_to: { type: string }
      question: { type: string }
      created_at: { type: string }
```

既存 `edges:` に追加（`HAS_SIGNAL` の from に `support_case` を含めるのは会話層の累積 signal 集合をグラフネイティブに持つため。S1-11 追記 3）:

```yaml
  HAS_SIGNAL: { from: [KnownResolution, support_case], to: Signal }
  BECAUSE: { from: KnownResolution, to: section }
```

既存 `traceable_pairs:` に追加:

```yaml
  - claim: KnownResolution
    evidence: section
    edge: BECAUSE
```

`version: 1` は変更しない（加算 = `is_additive` = gen bump 不要、別紙 §2）。

- [ ] **Step 2: `server/data/rules.sample.json` を書く**

```json
{
  "escalation_rules": [
    {
      "rule_id": "health-food-abnormal-ingestion",
      "condition": ["post_ingestion_symptom"],
      "owner": "safety",
      "route": "safety_team",
      "binding": "mandatory"
    },
    {
      "rule_id": "cosmetic-skin-reaction-continue",
      "condition": ["skin_irritation", "continue_use_question"],
      "owner": "quality",
      "route": "dermatology_liaison",
      "binding": "mandatory"
    }
  ],
  "prohibited_domains": [
    {
      "domain_id": "health-judgement",
      "domain_signals": ["post_ingestion_symptom", "dosage_for_condition", "allergy_concern"],
      "pattern": ["飲み合わせ", "持病があって", "医師に相談すべき"],
      "route": "medical_escalation_desk",
      "binding": "mandatory"
    },
    {
      "domain_id": "body-symptom",
      "domain_signals": ["skin_irritation"],
      "pattern": ["肌に異常", "体調が悪"],
      "route": "safety_team",
      "binding": "mandatory"
    }
  ]
}
```

- [ ] **Step 3: 失敗するテストを書く（属性 ⇔ 論理型変換の純粋部分）**

`server/src/harness/knowledge.rs` の `#[cfg(test)]`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::signal::Signal;
    use std::collections::HashMap;

    fn attrs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn escalation_rule_from_attributes_parses_condition_csv() {
        let rule = escalation_rule_from_attributes(&attrs(&[
            ("rule_id", "r1"),
            ("condition", "skin_irritation,continue_use_question"),
            ("route", "dermatology_liaison"),
            ("binding", "mandatory"),
        ]))
        .expect("parses");
        assert_eq!(rule.id, "r1");
        assert!(rule.condition.contains(&Signal::new("skin_irritation")));
        assert!(rule.condition.contains(&Signal::new("continue_use_question")));
        assert_eq!(rule.route, "dermatology_liaison");
        assert_eq!(rule.binding, Binding::Mandatory);
    }

    #[test]
    fn escalation_rule_missing_route_is_error() {
        assert!(escalation_rule_from_attributes(&attrs(&[("rule_id", "r1"), ("condition", "mold")])).is_err());
    }

    #[test]
    fn prohibited_domain_from_attributes_parses() {
        let domain = prohibited_domain_from_attributes(&attrs(&[
            ("domain_id", "d1"),
            ("domain_signals", "post_ingestion_symptom"),
            ("pattern", "飲み合わせ,持病があって"),
            ("route", "medical_escalation_desk"),
            ("binding", "mandatory"),
        ]))
        .expect("parses");
        assert_eq!(domain.text_patterns, vec!["飲み合わせ".to_string(), "持病があって".to_string()]);
    }

    #[test]
    fn known_resolution_node_build_uses_signal_nodes_not_json_attr() {
        // I2 / アンチパターン 3: signal_set が KR ノード属性に存在しないこと
        let new_kr = NewKnownResolution {
            signal_set: [Signal::new("discoloration"), Signal::new("mold")].into_iter().collect(),
            applicability: "全ロット".to_string(),
            answer: "廃棄してください".to_string(),
            origin: "escalation:esc-1".to_string(),
            created_by: "sup-001".to_string(),
            rationale_section_keys: vec!["doc-1#storage".to_string()],
        };
        let build = build_known_resolution_graph("sivira-cs-demo", "kr-test", &new_kr);
        let kr_node = build.nodes.iter().find(|n| n.node_type == "KnownResolution").expect("kr node");
        assert!(kr_node.attributes.iter().all(|(k, _)| k != "signal_set"));
        // Signal ノード 2 個 + HAS_SIGNAL 辺 2 本 + BECAUSE 辺 1 本
        assert_eq!(build.nodes.iter().filter(|n| n.node_type == "Signal").count(), 2);
        assert_eq!(build.edges.iter().filter(|e| e.edge_type == "HAS_SIGNAL").count(), 2);
        assert_eq!(build.edges.iter().filter(|e| e.edge_type == "BECAUSE").count(), 1);
        // 予約フィールドが空でも存在する（S1-8 条件 6）
        for key in ["binding", "registration_trigger", "knowledge_class", "outcome_ref"] {
            assert!(kr_node.attributes.iter().any(|(k, _)| k == key), "missing reserved {key}");
        }
    }
}
```

- [ ] **Step 4: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::knowledge 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 5: knowledge.rs を実装する**

```rust
use crate::harness::rules::{
    Binding, EscalationRule, Grade, KnownResolution, ProhibitedDomain, RootCause, SourceAuthority,
};
use crate::harness::signal::{Signal, SignalSet};
use crate::ingest::schema_generation_prefix;
use crate::model::{GraphBuild, GraphEdge, GraphNode};
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::Arc;

pub fn harness_node_id(schema: &str, kind: &str, key: &str) -> String {
    format!("{}{kind}:{key}", schema_generation_prefix(schema))
}

fn csv_signals(value: &str) -> SignalSet {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(Signal::new)
        .collect()
}

fn csv_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_binding(value: Option<&String>) -> Binding {
    match value.map(String::as_str) {
        Some("mandatory") => Binding::Mandatory,
        _ => Binding::Advisory,
    }
}

pub fn escalation_rule_from_attributes(attrs: &HashMap<String, String>) -> Result<EscalationRule> {
    Ok(EscalationRule {
        id: attrs.get("rule_id").cloned().ok_or_else(|| anyhow!("escalation_rule missing rule_id"))?,
        condition: csv_signals(attrs.get("condition").map(String::as_str).unwrap_or("")),
        route: attrs.get("route").cloned().ok_or_else(|| anyhow!("escalation_rule missing route"))?,
        owner: attrs.get("owner").cloned(),
        binding: parse_binding(attrs.get("binding")),
    })
}

pub fn prohibited_domain_from_attributes(attrs: &HashMap<String, String>) -> Result<ProhibitedDomain> {
    Ok(ProhibitedDomain {
        id: attrs.get("domain_id").cloned().ok_or_else(|| anyhow!("prohibited_domain missing domain_id"))?,
        domain_signals: csv_signals(attrs.get("domain_signals").map(String::as_str).unwrap_or("")),
        text_patterns: csv_list(attrs.get("pattern").map(String::as_str).unwrap_or("")),
        route: attrs.get("route").cloned().ok_or_else(|| anyhow!("prohibited_domain missing route"))?,
        binding: parse_binding(attrs.get("binding")),
    })
}

/// 担当者が追加する新ルール（add_known_resolution / correction_intake の出口）。
#[derive(Debug, Clone)]
pub struct NewKnownResolution {
    pub signal_set: SignalSet,
    pub applicability: String,
    pub answer: String,
    pub origin: String,
    pub created_by: String,
    pub rationale_section_keys: Vec<String>,
}

/// KR 1 件をグラフ表現（KR ノード + Signal ノード + HAS_SIGNAL / BECAUSE 辺）に組み立てる。
/// signal_set を JSON 属性に畳まない（I2）。予約フィールドは空で持たせる（S1-3）。
pub fn build_known_resolution_graph(schema: &str, kr_id: &str, kr: &NewKnownResolution) -> GraphBuild {
    let kr_node_id = harness_node_id(schema, "KnownResolution", kr_id);
    let mut nodes = vec![GraphNode {
        id: kr_node_id.clone(),
        node_type: "KnownResolution".to_string(),
        attributes: vec![
            ("kr_id".to_string(), kr_id.to_string()),
            ("answer_text".to_string(), kr.answer.clone()),
            ("applicability".to_string(), kr.applicability.clone()),
            ("grade".to_string(), "approval_required".to_string()),
            ("status".to_string(), "active".to_string()),
            ("source_authority".to_string(), "authoritative".to_string()),
            ("root_cause".to_string(), "knowledge_error".to_string()),
            ("approval_count".to_string(), "0".to_string()),
            ("rejection_count".to_string(), "0".to_string()),
            ("approver_set".to_string(), String::new()),
            ("origin".to_string(), kr.origin.clone()),
            ("created_by".to_string(), kr.created_by.clone()),
            ("verified_at".to_string(), chrono::Utc::now().to_rfc3339()),
            // --- 予約（空で存在させる）---
            ("error_axis".to_string(), String::new()),
            ("owner".to_string(), String::new()),
            ("binding".to_string(), "advisory".to_string()),
            ("direction".to_string(), String::new()),
            ("route".to_string(), String::new()),
            ("registration_trigger".to_string(), "single_ruling".to_string()),
            ("knowledge_class".to_string(), "commercial".to_string()),
            ("outcome_ref".to_string(), String::new()),
            ("search_text_ja".to_string(), kr.answer.clone()),
        ],
    }];
    let mut edges = Vec::new();
    for signal in &kr.signal_set {
        let signal_node_id = harness_node_id(schema, "Signal", signal.as_str());
        nodes.push(GraphNode {
            id: signal_node_id.clone(),
            node_type: "Signal".to_string(),
            attributes: vec![("value".to_string(), signal.as_str().to_string())],
        });
        edges.push(GraphEdge {
            from_id: kr_node_id.clone(),
            to_id: signal_node_id,
            edge_type: "HAS_SIGNAL".to_string(),
            attributes: Vec::new(),
        });
    }
    for section_key in &kr.rationale_section_keys {
        edges.push(GraphEdge {
            from_id: kr_node_id.clone(),
            to_id: crate::ingest::section_node_id(schema, section_key),
            edge_type: "BECAUSE".to_string(),
            attributes: Vec::new(),
        });
    }
    GraphBuild { nodes, edges }
}

pub struct KnowledgeStore {
    client: Arc<VegapunkClient>,
}

impl KnowledgeStore {
    pub fn new(client: Arc<VegapunkClient>) -> Self {
        Self { client }
    }

    pub async fn load_escalation_rules(&self, schema: &str) -> Result<Vec<EscalationRule>> {
        self.client
            .query_nodes(schema, "EscalationRule", Vec::new(), 1000)
            .await
            .context("load escalation rules")?
            .into_iter()
            .map(|node| escalation_rule_from_attributes(&node.attributes))
            .collect()
    }

    pub async fn load_prohibited_domains(&self, schema: &str) -> Result<Vec<ProhibitedDomain>> {
        self.client
            .query_nodes(schema, "ProhibitedDomain", Vec::new(), 1000)
            .await
            .context("load prohibited domains")?
            .into_iter()
            .map(|node| prohibited_domain_from_attributes(&node.attributes))
            .collect()
    }

    /// KnownResolution を Signal ノード経由で復元する（HAS_SIGNAL 辺の走査）。
    pub async fn load_known_resolutions(&self, schema: &str) -> Result<Vec<KnownResolution>> {
        let kr_nodes = self
            .client
            .query_nodes(schema, "KnownResolution", Vec::new(), 1000)
            .await
            .context("load known resolutions")?;
        if kr_nodes.is_empty() {
            return Ok(Vec::new());
        }
        let snapshot = self.client.graph_snapshot(schema, 5000).await?;
        // node_id -> Signal.value
        let signal_values: HashMap<String, String> = snapshot
            .nodes
            .iter()
            .filter(|n| n.node_type == "Signal")
            .filter_map(|n| n.attributes.get("value").map(|v| (n.node_id.clone(), v.clone())))
            .collect();
        // KR node_id -> SignalSet
        let mut kr_signals: HashMap<String, SignalSet> = HashMap::new();
        for edge in snapshot.edges.iter().filter(|e| e.edge_type == "HAS_SIGNAL") {
            if let Some(value) = signal_values.get(&edge.to_id) {
                kr_signals.entry(edge.from_id.clone()).or_default().insert(Signal::new(value));
            }
        }
        kr_nodes
            .into_iter()
            .map(|node| {
                let attrs = &node.attributes;
                let get = |key: &str| attrs.get(key).cloned().unwrap_or_default();
                Ok(KnownResolution {
                    id: attrs.get("kr_id").cloned().ok_or_else(|| anyhow!("KnownResolution missing kr_id"))?,
                    signal_set: kr_signals.remove(&node.node_id).unwrap_or_default(),
                    applicability: get("applicability"),
                    answer: get("answer_text"),
                    source_authority: match get("source_authority").as_str() {
                        "non_authoritative" => SourceAuthority::NonAuthoritative,
                        _ => SourceAuthority::Authoritative,
                    },
                    root_cause: match get("root_cause").as_str() {
                        "retrieval_miss" => RootCause::RetrievalMiss,
                        _ => RootCause::KnowledgeError,
                    },
                    grade: match get("grade").as_str() {
                        "auto_answer_audited" => Grade::AutoAnswerAudited,
                        "demoted" => Grade::Demoted,
                        _ => Grade::ApprovalRequired,
                    },
                    approval_count: get("approval_count").parse().unwrap_or(0),
                    rejection_count: get("rejection_count").parse().unwrap_or(0),
                    approver_set: csv_list(&get("approver_set")),
                    origin: get("origin"),
                    binding: parse_binding(attrs.get("binding")),
                    registration_trigger: get("registration_trigger"),
                    knowledge_class: get("knowledge_class"),
                    outcome_ref: csv_list(&get("outcome_ref")),
                })
            })
            .collect()
    }

    pub async fn insert_known_resolution(&self, schema: &str, kr: &NewKnownResolution) -> Result<String> {
        let kr_id = format!("kr-{}", uuid::Uuid::new_v4());
        let build = build_known_resolution_graph(schema, &kr_id, kr);
        self.client.upsert_graph_low_level(build).await?;
        Ok(kr_id)
    }

    /// support 系 record（support_case / answer_attempt など）を 1 ノードとして書く。
    pub async fn record(
        &self,
        schema: &str,
        node_type: &str,
        key: &str,
        attributes: Vec<(String, String)>,
    ) -> Result<()> {
        let node = GraphNode {
            id: harness_node_id(schema, node_type, key),
            node_type: node_type.to_string(),
            attributes,
        };
        self.client.upsert_nodes(vec![node]).await?;
        Ok(())
    }

    /// 会話層: support_case の累積 signal 集合を HAS_SIGNAL 辺から復元する（S1-11 追記 3）。
    pub async fn load_case_signals(&self, schema: &str, case_id: &str) -> Result<SignalSet> {
        let case_node_id = harness_node_id(schema, "support_case", case_id);
        let snapshot = self.client.graph_snapshot(schema, 5000).await?;
        let signal_values: HashMap<String, String> = snapshot
            .nodes
            .iter()
            .filter(|n| n.node_type == "Signal")
            .filter_map(|n| n.attributes.get("value").map(|v| (n.node_id.clone(), v.clone())))
            .collect();
        Ok(snapshot
            .edges
            .iter()
            .filter(|e| e.edge_type == "HAS_SIGNAL" && e.from_id == case_node_id)
            .filter_map(|e| signal_values.get(&e.to_id).map(Signal::new))
            .collect())
    }

    /// 会話層: 今ターンの signal を support_case に加算する（Signal ノード + HAS_SIGNAL 辺 upsert）。
    pub async fn append_case_signals(&self, schema: &str, case_id: &str, signals: &SignalSet) -> Result<()> {
        if signals.is_empty() {
            return Ok(());
        }
        let case_node_id = harness_node_id(schema, "support_case", case_id);
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for signal in signals {
            let signal_node_id = harness_node_id(schema, "Signal", signal.as_str());
            nodes.push(GraphNode {
                id: signal_node_id.clone(),
                node_type: "Signal".to_string(),
                attributes: vec![("value".to_string(), signal.as_str().to_string())],
            });
            edges.push(GraphEdge {
                from_id: case_node_id.clone(),
                to_id: signal_node_id,
                edge_type: "HAS_SIGNAL".to_string(),
                attributes: Vec::new(),
            });
        }
        self.client.upsert_graph_low_level(GraphBuild { nodes, edges }).await?;
        Ok(())
    }

    /// grade 運用: 承認/却下カウントと格付けを KnownResolution ノードに反映する（遵守事項 3）。
    pub async fn update_known_resolution_grade(
        &self,
        schema: &str,
        kr_id: &str,
        approval_count: u32,
        rejection_count: u32,
        approver_set: &[String],
        grade: Grade,
    ) -> Result<()> {
        let grade_value = match grade {
            Grade::ApprovalRequired => "approval_required",
            Grade::AutoAnswerAudited => "auto_answer_audited",
            Grade::Demoted => "demoted",
        };
        // upsert merge: 既存ノードに対して該当属性のみ更新する
        let node = GraphNode {
            id: harness_node_id(schema, "KnownResolution", kr_id),
            node_type: "KnownResolution".to_string(),
            attributes: vec![
                ("kr_id".to_string(), kr_id.to_string()),
                ("approval_count".to_string(), approval_count.to_string()),
                ("rejection_count".to_string(), rejection_count.to_string()),
                ("approver_set".to_string(), approver_set.join(",")),
                ("grade".to_string(), grade_value.to_string()),
            ],
        };
        self.client.upsert_nodes(vec![node]).await?;
        Ok(())
    }
}
```

注意: vegapunk の `UpsertNodes` が部分属性 merge でなく全属性置換だった場合、`update_known_resolution_grade` は先に `query_nodes` で現属性を読み、全属性を再送する実装に変える（挙動は Task 14 の実機検証で確認する）。

注意: `node.attributes` の型は `crate::proto::graphrag::NodeResult` の生成型に依存する（`HashMap<String, String>` を想定。mcp.rs の `attrs.get("title_ja")` と同じアクセスパターン）。実際の生成型が `Vec<NodeAttribute>` 等だった場合は `mcp.rs` の既存アクセスに合わせて調整する。

- [ ] **Step 6: テストが通ることを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::knowledge 2>&1 | tail -5`
Expected: `test result: ok. 4 passed`

- [ ] **Step 7: `server/src/bin/ingest_rules.rs` を書く**

`ingest_demo.rs` の CLI 構造（clap / endpoint / schema / token 読み込み）を踏襲する。まず `ingest_demo.rs` を読み、同じ引数体系にする。本体:

```rust
use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::harness::knowledge::harness_node_id;
use cs_support_mcp::model::{GraphBuild, GraphNode};
use cs_support_mcp::vegapunk::VegapunkClient;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    schema: String,
    #[arg(long)]
    schema_file: PathBuf,
    #[arg(long)]
    rules_file: PathBuf,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN")]
    vegapunk_bearer_token: Option<String>,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN_FILE")]
    vegapunk_bearer_token_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct RulesFile {
    escalation_rules: Vec<RuleInput>,
    prohibited_domains: Vec<DomainInput>,
}

#[derive(Debug, Deserialize)]
struct RuleInput {
    rule_id: String,
    condition: Vec<String>,
    owner: Option<String>,
    route: String,
    binding: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DomainInput {
    domain_id: String,
    #[serde(default)]
    domain_signals: Vec<String>,
    #[serde(default)]
    pattern: Vec<String>,
    route: String,
    binding: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();
    let token = read_token(&args)?;
    let client = VegapunkClient::connect(&args.endpoint, &token).await?;

    // 1. 加算スキーマ登録（既存 schema に node/edge type を足す）
    let schema_yaml = std::fs::read_to_string(&args.schema_file)
        .with_context(|| format!("read schema file {}", args.schema_file.display()))?;
    client.create_or_update_schema(&args.schema, schema_yaml).await?;
    tracing::info!(schema = %args.schema, "schema updated (additive)");

    // 2. 第1層ルール・第2層領域をノードとして投入
    let rules: RulesFile = serde_json::from_str(
        &std::fs::read_to_string(&args.rules_file)
            .with_context(|| format!("read rules file {}", args.rules_file.display()))?,
    )?;
    let mut nodes = Vec::new();
    for rule in &rules.escalation_rules {
        nodes.push(GraphNode {
            id: harness_node_id(&args.schema, "EscalationRule", &rule.rule_id),
            node_type: "EscalationRule".to_string(),
            attributes: vec![
                ("rule_id".to_string(), rule.rule_id.clone()),
                ("condition".to_string(), rule.condition.join(",")),
                ("owner".to_string(), rule.owner.clone().unwrap_or_default()),
                ("route".to_string(), rule.route.clone()),
                ("binding".to_string(), rule.binding.clone().unwrap_or_else(|| "advisory".to_string())),
            ],
        });
    }
    for domain in &rules.prohibited_domains {
        nodes.push(GraphNode {
            id: harness_node_id(&args.schema, "ProhibitedDomain", &domain.domain_id),
            node_type: "ProhibitedDomain".to_string(),
            attributes: vec![
                ("domain_id".to_string(), domain.domain_id.clone()),
                ("domain_signals".to_string(), domain.domain_signals.join(",")),
                ("pattern".to_string(), domain.pattern.join(",")),
                ("route".to_string(), domain.route.clone()),
                ("binding".to_string(), domain.binding.clone().unwrap_or_else(|| "mandatory".to_string())),
            ],
        });
    }
    let build = GraphBuild { nodes, edges: Vec::new() };
    let (node_count, edge_count) = client.upsert_graph_low_level(build).await?;
    tracing::info!(node_count, edge_count, "ingested layer1 rules and layer2 domains");
    Ok(())
}

fn read_token(args: &Args) -> Result<String> {
    if let Some(token) = &args.vegapunk_bearer_token {
        return Ok(token.clone());
    }
    if let Some(path) = &args.vegapunk_bearer_token_file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("read token file {}", path.display()))
            .map(|s| s.trim().to_string());
    }
    anyhow::bail!("vegapunk bearer token is required (--vegapunk-bearer-token or VEGAPUNK_BEARER_TOKEN_FILE)")
}
```

`server/Cargo.toml` に追加:

```toml
[[bin]]
name = "ingest_rules"
path = "src/bin/ingest_rules.rs"
```

- [ ] **Step 8: コンパイル確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo check --bins 2>&1 | tail -5`
Expected: `Finished`（gRPC 実機投入は Task 14 で行う。ここではビルドまで）

- [ ] **Step 9: Commit**

```bash
git add schema/cs-schema.yml server/src/harness/ server/data/rules.sample.json server/src/bin/ingest_rules.rs server/Cargo.toml server/Cargo.lock
git commit -m "feat: add additive knowledge schema, PunkRecord-backed knowledge store, and rules ingest CLI"
```

---

### Task 11: Harness 本体（mod.rs）+ main.rs 配線

**Files:**
- Modify: `server/src/harness/mod.rs`（Harness / RequestContext / EvaluationOutcome 追加）
- Modify: `server/src/main.rs`（Harness 構築・rmcp サーバへ注入）
- Test: `server/src/harness/mod.rs` 内 `#[cfg(test)]`（begin の scope 強制のみ。evaluate は gRPC 依存のため Task 14 で実機検証）

**Interfaces:**
- Consumes: Task 2〜10 の全 API、`crate::mcp::ToolService`、`crate::config::AppConfig`
- Produces: `Harness::build` / `Harness::begin` / `Harness::evaluate` / `Harness::root_cause_probe` / `RequestContext` / `EvaluationOutcome`（総覧どおり）

- [ ] **Step 1: 失敗するテストを書く（begin の認証・scope 強制）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ActorConfig;

    fn harness_for_test() -> Harness {
        let dir = std::env::temp_dir().join(format!("harness-test-{}", uuid::Uuid::new_v4()));
        Harness {
            authenticator: authn::Authenticator::new(
                None,
                &[ActorConfig {
                    sub: "op-001".to_string(),
                    role: "operator".to_string(),
                    allowed_schemas: vec!["sivira-cs-demo".to_string()],
                }],
                Some("op-001".to_string()),
            ),
            normalizer: std::sync::Arc::new(
                signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap(),
            ),
            lexicon: std::sync::Arc::new(
                signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap(),
            ),
            ng: egress::NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":[]}"#).unwrap(),
            worm: audit::WormAuditLog::open(&dir.join("audit.jsonl")).unwrap(),
            knowledge: None,
            thresholds: decision::Thresholds { low: 0.6, mid: 0.8, high: 0.95 },
            grading: grading::GradingThresholds {
                promote_approvals: 3,
                promote_approvers: 2,
                promote_max_rejection_rate: 0.2,
                demote_rejections: 2,
            },
            queue_path: dir.join("queue.jsonl"),
        }
    }

    #[test]
    fn begin_produces_request_context_with_enforced_schema() {
        let harness = harness_for_test();
        let ctx = harness.begin(None, "sivira-cs-demo").expect("begin");
        assert_eq!(ctx.schema, "sivira-cs-demo");
        assert_eq!(ctx.actor.sub, "op-001");
        assert!(!ctx.request_id.is_empty());
    }

    #[test]
    fn begin_rejects_out_of_scope_project() {
        let harness = harness_for_test();
        assert!(harness.begin(None, "other-tenant").is_err());
    }
}
```

注意: `knowledge: None` にするため、Harness の `knowledge` フィールドは `Option<knowledge::KnowledgeStore>` とする（テスト・stdio 検証用に gRPC 接続なしで構築できる形。実行時は必ず `Some`）。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness::tests 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 3: mod.rs に Harness を実装する**

```rust
pub mod audit;
pub mod authn;
pub mod correction;
pub mod decision;
pub mod egress;
pub mod grading;
pub mod knowledge;
pub mod rules;
pub mod scope;
pub mod signal;

use crate::config::AppConfig;
use crate::mcp::ToolService;
use crate::model::SectionHit;
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Harness {
    pub authenticator: authn::Authenticator,
    pub normalizer: Arc<dyn signal::SignalNormalizer>,
    pub lexicon: Arc<signal::LexiconNormalizer>,
    pub ng: egress::NgDictionary,
    pub worm: audit::WormAuditLog,
    pub knowledge: Option<knowledge::KnowledgeStore>,
    pub thresholds: decision::Thresholds,
    pub grading: grading::GradingThresholds,
    pub queue_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub actor: authn::Actor,
    pub scope: scope::AccessScope,
    pub schema: String,
    pub request_id: String,
}

pub struct EvaluationOutcome {
    pub decision: decision::AnswerDecision,
    /// 今ターンで抽出した signal
    pub signals: signal::SignalSet,
    /// 判定に使った累積 signal 集合（会話層。判定根拠は常にこちら）
    pub accumulated_signals: signal::SignalSet,
    /// 会話の継続キー。新規なら採番して返す
    pub case_id: String,
    /// 聞き返し可否（第3層グレーのみ true。第1・2層は false = 問答無用でルーティング）
    pub clarification_allowed: bool,
    pub hits: Vec<SectionHit>,
    pub audit_event_id: String,
}

impl Harness {
    pub fn build(config: &AppConfig, client: Arc<VegapunkClient>, config_dir: &Path) -> Result<Self> {
        let resolve_path = |p: &str| {
            let path = Path::new(p);
            if path.is_absolute() { path.to_path_buf() } else { config_dir.join(path) }
        };
        let secret = match &config.auth.jwt_secret_file {
            Some(path) => Some(
                std::fs::read(resolve_path(path))
                    .with_context(|| format!("read jwt secret file {path}"))?
                    .trim_ascii()
                    .to_vec(),
            ),
            None => None,
        };
        let lexicon = Arc::new(signal::LexiconNormalizer::from_path(
            &resolve_path(&config.harness.signal_lexicon_path),
        )?);
        Ok(Self {
            authenticator: authn::Authenticator::new(secret, &config.actors, config.auth.default_actor.clone()),
            normalizer: lexicon.clone(),
            lexicon,
            ng: egress::NgDictionary::from_path(&resolve_path(&config.harness.ng_dictionary_path))?,
            worm: audit::WormAuditLog::open(&resolve_path(&config.harness.audit_log_path))?,
            knowledge: Some(knowledge::KnowledgeStore::new(client)),
            thresholds: decision::Thresholds {
                low: config.harness.thresholds.low,
                mid: config.harness.thresholds.mid,
                high: config.harness.thresholds.high,
            },
            grading: grading::GradingThresholds {
                promote_approvals: config.harness.grading.promote_approvals,
                promote_approvers: config.harness.grading.promote_approvers,
                promote_max_rejection_rate: config.harness.grading.promote_max_rejection_rate,
                demote_rejections: config.harness.grading.demote_rejections,
            },
            queue_path: resolve_path(&config.harness.search_improvement_queue_path),
        })
    }

    fn knowledge(&self) -> Result<&knowledge::KnowledgeStore> {
        self.knowledge.as_ref().ok_or_else(|| anyhow!("knowledge store is not configured"))
    }

    /// S1-1 パイプライン前半: [認証] → [(A) 権限]。全 tool がここを通る。
    pub fn begin(&self, authorization: Option<&str>, project_schema: &str) -> Result<RequestContext> {
        let actor = self.authenticator.authenticate(authorization)?;
        let access = scope::resolve_scope(&actor, project_schema)?;
        Ok(RequestContext {
            schema: access.enforced_schema().to_string(),
            actor,
            scope: access,
            request_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    /// S1-1 パイプライン後半: [取得] → [正規化] → [会話層 累積] → [(B) 3 層判定] → [記録]。
    /// 会話層（S1-0 / 遵守事項 1）: case_id 単位の累積 signal 集合をサーバ側で維持し、
    /// **毎ターン累積集合で再判定**する。条件が増えたら（変色 → 変色+カビ）再判定が
    /// 自動的にエスカレーションへ倒れる。会話履歴の言質は判定入力にしない。
    pub async fn evaluate(
        &self,
        ctx: &RequestContext,
        question: &str,
        product_key: Option<&str>,
        case_id: Option<&str>,
        tools: &ToolService,
    ) -> Result<EvaluationOutcome> {
        let knowledge = self.knowledge()?;
        // [取得] scope は ctx.schema として全検索に注入済み（tenant=schema）
        let rules = knowledge.load_escalation_rules(&ctx.schema).await?;
        let domains = knowledge.load_prohibited_domains(&ctx.schema).await?;
        let resolutions = knowledge.load_known_resolutions(&ctx.schema).await?;
        let hits = tools.search_manual(&ctx.schema, question, product_key, 5).await?;
        // [正規化] 決定論 lexicon（S1-11）。今ターン分。
        let signals = self.normalizer.normalize(question);
        // [会話層] 累積 signal 集合の維持。client 供給の prior signals は受けない（入力不信）。
        let (case_id, prior_signals) = match case_id {
            Some(id) => (id.to_string(), knowledge.load_case_signals(&ctx.schema, id).await?),
            None => {
                let new_id = format!("case-{}", uuid::Uuid::new_v4());
                knowledge
                    .record(
                        &ctx.schema,
                        "support_case",
                        &new_id,
                        vec![
                            ("case_id".to_string(), new_id.clone()),
                            ("request_id".to_string(), ctx.request_id.clone()),
                            ("actor".to_string(), ctx.actor.sub.clone()),
                            ("question".to_string(), question.to_string()),
                            ("product_key".to_string(), product_key.unwrap_or_default().to_string()),
                            ("created_at".to_string(), chrono::Utc::now().to_rfc3339()),
                        ],
                    )
                    .await?;
                (new_id, signal::SignalSet::new())
            }
        };
        let accumulated: signal::SignalSet = prior_signals.union(&signals).cloned().collect();
        let new_signals: signal::SignalSet = signals.difference(&prior_signals).cloned().collect();
        knowledge.append_case_signals(&ctx.schema, &case_id, &new_signals).await?;
        // stakes 入力の決定論算出（累積集合に対して）
        let stakes_input = decision::StakesInput {
            mandatory_domain_near: domains.iter().any(|d| {
                d.binding == rules::Binding::Mandatory
                    && rules::match_layer2(std::slice::from_ref(d), &accumulated, question).is_some()
            }),
            ng_near_hit: self.ng.near_hit(question),
            hazard_signal_count: accumulated
                .iter()
                .filter(|s| self.lexicon.class_of(s) == Some(signal::SignalClass::Hazard))
                .count(),
        };
        // [(B) 3 層判定] 純関数。判定根拠は常に「累積 signal 集合 + known_resolution」。
        let best = hits.first();
        let section_keys: Vec<String> = hits.iter().map(|h| h.section_key.clone()).collect();
        let decision_result = decision::decide(&decision::DecisionInput {
            question_signals: &accumulated,
            question_raw: question,
            rules: &rules,
            domains: &domains,
            resolutions: &resolutions,
            best_manual_score: best.map(|h| h.score),
            best_manual_sections: &section_keys,
            stakes_input,
            thresholds: &self.thresholds,
        });
        // 聞き返し可否（決定論）: 第3層グレーのみ。第1・2層は問答無用でルーティング。
        let clarification_allowed = matches!(
            &decision_result,
            decision::AnswerDecision::Escalate {
                layer: 3,
                reason: decision::EscalateReason::InsufficientDirectness
                    | decision::EscalateReason::UnknownAddedSignal,
                ..
            }
        );
        // [記録] WORM（S1-8 条件 8）
        let (decision_label, route) = match &decision_result {
            decision::AnswerDecision::Allowed { source, .. } => (format!("allowed:{source:?}"), None),
            decision::AnswerDecision::Escalate { layer, route_to, .. } => {
                (format!("escalate:layer{layer}"), Some(route_to.clone()))
            }
        };
        let mut retrieved_node_ids: Vec<String> = hits
            .iter()
            .map(|h| crate::ingest::section_node_id(&ctx.schema, &h.section_key))
            .collect();
        retrieved_node_ids.push(knowledge::harness_node_id(&ctx.schema, "support_case", &case_id));
        let audit_event_id = self.worm.append(audit::AuditDraft {
            request_id: ctx.request_id.clone(),
            schema: ctx.schema.clone(),
            actor: ctx.actor.sub.clone(),
            used_scope: ctx.scope.clone(),
            retrieved_node_ids,
            decision: decision_label,
            route,
            governing_norm_ids: Vec::new(),
        })?;
        Ok(EvaluationOutcome {
            decision: decision_result,
            signals,
            accumulated_signals: accumulated,
            case_id,
            clarification_allowed,
            hits,
            audit_event_id,
        })
    }

    /// 訂正時の root_cause 切り分け（S1-5）: 正しい根拠がグラフ内に存在したかを再検索で判定。
    pub async fn root_cause_probe(
        &self,
        ctx: &RequestContext,
        corrected_answer: &str,
        tools: &ToolService,
    ) -> Result<rules::RootCause> {
        let hits = tools.search_manual(&ctx.schema, corrected_answer, None, 3).await?;
        let found = hits.first().map(|h| h.score >= self.thresholds.mid).unwrap_or(false);
        Ok(if found { rules::RootCause::RetrievalMiss } else { rules::RootCause::KnowledgeError })
    }

    /// 検索改善キューへの追記（retrieval_miss の受け皿。known_resolution を増やさない）。
    pub fn enqueue_search_improvement(&self, ctx: &RequestContext, corrected_answer: &str) -> Result<()> {
        if let Some(parent) = self.queue_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.queue_path)?;
        let entry = serde_json::json!({
            "request_id": ctx.request_id,
            "schema": ctx.schema,
            "actor": ctx.actor.sub,
            "corrected_answer": corrected_answer,
            "queued_at": chrono::Utc::now().to_rfc3339(),
        });
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        Ok(())
    }
}
```

注意: `trim_ascii` が安定化前の toolchain ではエラーになる。その場合は `String::from_utf8` + `trim` に置き換える。

- [ ] **Step 4: main.rs に配線する**

`server/src/main.rs` の変更点:

```rust
// ToolService 構築の直後に追加
let vegapunk_arc = std::sync::Arc::new(vegapunk.clone());
let config_dir = args.config.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();
let harness = std::sync::Arc::new(
    cs_support_mcp::harness::Harness::build(&config, vegapunk_arc, &config_dir)
        .context("build harness")?,
);
```

`CsSupportRmcpServer::new(...)` の呼び出し（stdio / HTTP 両方）を `CsSupportRmcpServer::new(project.schema.clone(), tools.clone(), harness.clone())` に変える（rmcp_server 側の変更は Task 12）。

- [ ] **Step 5: テスト・ビルド確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test harness 2>&1 | tail -5`
Expected: 全 harness テスト PASS（Task 12 完了まで `cargo check` が rmcp_server で失敗する場合は、main.rs の配線変更を Task 12 のコミットに含めてよい）

- [ ] **Step 6: Commit**

```bash
git add server/src/harness/ server/src/main.rs
git commit -m "feat: add harness orchestration pipeline (authn, scope, retrieve, normalize, decide, audit)"
```

---

### Task 12: 既存 read tool の Harness 経由化 + evaluate_answerability

**Files:**
- Modify: `server/src/rmcp_server.rs`
- Modify: `server/src/main.rs`（Task 11 Step 4 の配線を完成させる）
- Modify: `server/config.local-https.toml` / `server/config.toml` / `server/config.gce.toml`（actors / auth / harness 設定を追加）
- Test: `cargo check` + 既存 harness テスト（rmcp tool の統合検証は Task 14）

**Interfaces:**
- Consumes: `Harness::begin` / `Harness::evaluate`、`rmcp::model::Extensions`（HTTP Parts の取得。streamable HTTP transport が `http::request::Parts` を extensions に注入する — rmcp ソースで確認済み。stdio では Parts が無く `None` になるので default actor にフォールバック）
- Produces: 全 tool が Harness 経由で動く rmcp サーバ。新 tool `evaluate_answerability`。

**認証ヘッダの取得方法（確認済み）:** tool メソッドの引数に `extensions: rmcp::model::Extensions` を取る（この extractor は失敗しない）。`extensions.get::<http::request::Parts>()` が `Some(parts)` なら `parts.headers` から `authorization` を読む。`None`（stdio）なら `None` を渡し、Authenticator の default_actor 経路に任せる。

- [ ] **Step 1: CsSupportRmcpServer に Harness を持たせ、共通ヘルパを書く**

```rust
use crate::harness::{Harness, RequestContext};
use std::sync::Arc;

#[derive(Clone)]
pub struct CsSupportRmcpServer {
    schema: String,
    tools: ToolService,
    harness: Arc<Harness>,
}

impl CsSupportRmcpServer {
    pub fn new(schema: String, tools: ToolService, harness: Arc<Harness>) -> Self {
        Self { schema, tools, harness }
    }

    /// 全 tool の共通入口。認証 → scope 強制 → RequestContext。
    fn begin(&self, extensions: &rmcp::model::Extensions) -> Result<RequestContext, ErrorData> {
        let authorization = extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.headers.get(http::header::AUTHORIZATION))
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        self.harness
            .begin(authorization.as_deref(), &self.schema)
            .map_err(|err| ErrorData::invalid_request(err.to_string(), None))
    }
}
```

- [ ] **Step 2: 既存 4 tool を Harness 経由に変更する**

各 tool メソッドに `extensions: rmcp::model::Extensions` 引数を追加し、冒頭で `let ctx = self.begin(&extensions)?;` を呼び、`&self.schema` を直接使っていた箇所を `&ctx.schema` に変える。例（`search_manual`。他 3 tool も同型）:

```rust
    #[tool(
        name = "search_manual",
        description = "日本語 query_ja で日本語マニュアル本文 body_ja を検索し、breadcrumb と英語原文 fallback を返す。認証 actor の scope 内のみ検索される。"
    )]
    async fn search_manual(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<SearchManualRequest>,
    ) -> Result<Json<SearchManualResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        self.tools
            .search_manual(&ctx.schema, &req.query_ja, req.product_key.as_deref(), req.top_k.unwrap_or(5))
            .await
            .map(|hits| Json(SearchManualResponse { hits }))
            .map_err(to_error)
    }
```

`resolve_product` / `get_section` / `get_product` も同様に変更する。

- [ ] **Step 3: `evaluate_answerability` tool を追加する**

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvaluateAnswerabilityRequest {
    /// 顧客質問（日本語）。今ターンの発話。signal 正規化と 3 層判定の対象。
    pub question: String,
    pub product_key: Option<String>,
    /// 会話の継続キー。同一問い合わせの 2 ターン目以降は必ず前回返された case_id を渡す。
    /// サーバは case の累積 signal 集合に今ターン分を加算し、累積集合で再判定する。
    /// （prior signals を client から直接受け取ることはしない）
    pub case_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EvaluateAnswerabilityResponse {
    pub decision: crate::harness::decision::AnswerDecision,
    /// 今ターンで抽出した signal
    pub signals: Vec<String>,
    /// 判定に使った累積 signal 集合（判定根拠）
    pub accumulated_signals: Vec<String>,
    /// 次ターンで渡す会話キー
    pub case_id: String,
    /// true のとき、不足条件（decision.missing）について利用者へ聞き返してよい。
    /// 文面は client（LLM）が生成する。第1・2層エスカレーションでは常に false。
    pub clarification_allowed: bool,
    pub hits: Vec<SectionHit>,
    pub audit_event_id: String,
    pub request_id: String,
}
```

```rust
    #[tool(
        name = "evaluate_answerability",
        description = "顧客質問を 3 層判定（明示ルール → 禁止領域 → 回答可能性）にかけ、回答可否・エスカレーション判定・根拠を返す。回答系フローの必須入口。マルチターンの問い合わせでは前回の case_id を渡すこと（累積条件で毎回再判定される）。"
    )]
    async fn evaluate_answerability(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<EvaluateAnswerabilityRequest>,
    ) -> Result<Json<EvaluateAnswerabilityResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let outcome = self
            .harness
            .evaluate(&ctx, &req.question, req.product_key.as_deref(), req.case_id.as_deref(), &self.tools)
            .await
            .map_err(to_error)?;
        Ok(Json(EvaluateAnswerabilityResponse {
            decision: outcome.decision,
            signals: outcome.signals.iter().map(|s| s.as_str().to_string()).collect(),
            accumulated_signals: outcome.accumulated_signals.iter().map(|s| s.as_str().to_string()).collect(),
            case_id: outcome.case_id,
            clarification_allowed: outcome.clarification_allowed,
            hits: outcome.hits,
            audit_event_id: outcome.audit_event_id,
            request_id: ctx.request_id,
        }))
    }
```

- [ ] **Step 4: config 3 ファイルに設定を追加する**

`server/config.local-https.toml` に追記（`config.toml` / `config.gce.toml` も同じ節。GCE は `default_actor` を置かず JWT 必須にし、secret は env `CS_SUPPORT_JWT_SECRET_FILE` 経由）:

```toml
[auth]
# jwt_secret_file = "/path/to/jwt-secret"   # env CS_SUPPORT_JWT_SECRET_FILE で上書き
default_actor = "op-001"                      # dev 専用フォールバック

[[actors]]
sub = "op-001"
role = "operator"
allowed_schemas = ["sivira-cs-demo"]

[[actors]]
sub = "sup-001"
role = "supervisor"
allowed_schemas = ["sivira-cs-demo"]

[harness]
audit_log_path = "data/audit/audit.jsonl"
search_improvement_queue_path = "data/audit/search-improvement-queue.jsonl"
signal_lexicon_path = "data/signal-lexicon.json"
ng_dictionary_path = "data/ng-dictionary.json"
policy = "escalate_unless_answerable"

[harness.thresholds]
low = 0.6
mid = 0.8
high = 0.95

# S1-11 追記 4 の仮置き値。業務確認で確定させる
[harness.grading]
promote_approvals = 3
promote_approvers = 2
promote_max_rejection_rate = 0.2
demote_rejections = 2
```

- [ ] **Step 5: ビルド・テスト確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo check 2>&1 | tail -5 && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test 2>&1 | tail -5`
Expected: check `Finished`、test 全 PASS

`extensions: rmcp::model::Extensions` の引数位置で `#[tool]` マクロがエラーを出す場合は、rmcp 1.7 の `tool` マクロのドキュメント（`~/.cargo/registry/src/*/rmcp-1.7*/src/handler/server/tool.rs` 相当）を読み、`Parameters` より前/後の正しい位置に調整する。

- [ ] **Step 6: Commit**

```bash
git add server/src/rmcp_server.rs server/src/main.rs server/config*.toml server/config.toml
git commit -m "feat: route all read tools through harness and add evaluate_answerability tool"
```

---

### Task 13: record 系 / search 系 / add_known_resolution tool

**Files:**
- Modify: `server/src/rmcp_server.rs`（tool 追加）
- Modify: `server/src/mcp.rs`（`tools_list()` 旧 JSON-RPC 定義は未使用のため変更不要。`upsert_*` は rmcp tool 面に出ていないことを確認するのみ）
- Test: `cargo check` + harness テスト（統合検証は Task 14）

**Interfaces:**
- Consumes: `Harness` の全 API、`KnowledgeStore::{record, insert_known_resolution}`、`correction_intake` / `egress_gate` / `root_cause_probe`
- Produces: S1-7 の残り 7 tool: `search_past_cases` / `search_known_resolutions` / `record_answer_attempt` / `record_answer_outcome` / `record_operator_feedback` / `create_escalation_event` / `add_known_resolution`

各 tool の要点（全 tool 冒頭で `let ctx = self.begin(&extensions)?;`）:

- [ ] **Step 1: `search_known_resolutions` / `search_past_cases` を実装する**

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchKnownResolutionsRequest {
    /// 顧客質問（日本語）。signal 正規化して照合する。
    pub question: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct KnownResolutionHit {
    pub kr_id: String,
    pub signals: Vec<String>,
    pub answer: String,
    pub applicability: String,
    pub grade: String,
    pub match_kind: String, // "applicable" / "blocked_by_added_signal"
}
```

`search_known_resolutions`: `harness.normalizer.normalize(question)` → `knowledge.load_known_resolutions(&ctx.schema)` → `match_known_resolution`。`Applicable` は 1 件返し、`BlockedByAddedSignal` は leftover を含めて返す（担当者が「なぜ再利用不可か」を見える化）。判定は Rust、tool は結果を返すだけ。

`search_past_cases`: `query_nodes(&ctx.schema, "support_case", filters, 50)` を `KnowledgeStore` 経由で叩き、`question` の `normalize_key` 部分一致でスコアリング（`section_score` と同じ手法）して返す。scope は `ctx.schema` 固定。

- [ ] **Step 2: `record_answer_attempt` を実装する（egress gate の座る場所）**

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordAnswerAttemptRequest {
    pub case_id: Option<String>,
    /// 顧客に出す予定の draft 全文。AI 草案・担当者修正文の区別なく必ずここを通す。
    pub draft: String,
    pub question: String,
    pub product_key: Option<String>,
    /// 判定済み evaluate_answerability の request_id（lineage 接続用、任意）
    pub evaluation_request_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordAnswerAttemptResponse {
    pub attempt_id: String,
    pub egress: crate::harness::egress::EgressVerdict,
    /// pass のときのみ true。Step 1 の利用者は担当者なので pass は直接応答してよい。
    /// block / abstain はエスカレーション応答へ（未検証 AI 草案の参考添付は可。
    /// 担当者が検証・承認すれば add_known_resolution で知識蓄積になる）
    pub emit_allowed: bool,
    pub audit_event_id: String,
}
```

処理: `ctx` → `egress_gate(&req.draft, &EmitContext { channel: EmitChannel::Operator }, &harness.ng)`（Step 1 は operator 固定）→ verdict に関わらず `answer_attempt` ノードを `KnowledgeStore::record` で書く（attributes: attempt_id = uuid, case_id, request_id = ctx.request_id, actor, draft, decision = evaluation_request_id.unwrap_or_default(), egress_verdict, audit_event_id, created_at）→ WORM に `decision: "egress:{verdict}"` で append → `emit_allowed = matches!(verdict, EgressVerdict::Pass)`。tool description には「pass = 担当者へ応答可 / block・abstain = エスカレーション応答（草案は参考添付のみ）。誤答になるかもしれないものはエスカレーションに倒す」ことを明記する（spec S1-1 [emit or escalate]・フェーズ不変条件）。

- [ ] **Step 3: `record_answer_outcome` / `create_escalation_event` を実装する（grade 運用を含む）**

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordAnswerOutcomeRequest {
    pub attempt_id: String,
    /// resolved / unresolved / re_inquiry / wrong_answer
    pub outcome: String,
    /// この応答が known_resolution 由来だった場合に渡す（承認/却下カウントと格付けを更新）
    pub known_resolution_id: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordAnswerOutcomeResponse {
    /// grade 更新が行われた場合の新しい格付け
    pub grade: Option<String>,
    pub audit_event_id: String,
}
```

処理:
1. `answer_attempt` の attributes 更新（同一 node_id への upsert merge で outcome / note を追記）+ WORM append（`decision: "outcome:{outcome}"`）。
2. **grade 運用（遵守事項 3）**: `known_resolution_id` が渡された場合、`load_known_resolutions` から該当 KR を引き、`outcome` を承認/却下に写像する — `resolved` → `approval_count += 1` かつ `approver_set` に `ctx.actor.sub` を追加 / `wrong_answer` → `rejection_count += 1`（`unresolved` / `re_inquiry` はカウント変更なし）。
3. `let new_grade = grading::regrade(kr.grade, approval_count, rejection_count, approver_set.len(), &harness.grading);` → `knowledge.update_known_resolution_grade(...)` で永続化 → WORM append（`decision: "regrade:{old}->{new}"`, `governing_norm_ids: [kr_id]`）。
4. 判定はすべて `regrade` 純関数（Task 6）。tool handler にしきい値比較を直書きしない。

`create_escalation_event(question, layer, reason, route_to, case_id?)`: `escalation_event` ノードを書き、WORM に `decision: "escalation_event"` で append。`escalation_id` を返す。

`create_escalation_event(question, layer, reason, route_to)`: `escalation_event` ノードを書き、WORM に `decision: "escalation_event"` で append。`escalation_id` を返す。

- [ ] **Step 4: `record_operator_feedback` を実装する（correction_intake の feeder）**

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordOperatorFeedbackRequest {
    pub attempt_id: Option<String>,
    /// "operator"（担当者自身の訂正）か "customer"（顧客からの「違う」の中継）
    pub feedback_source: String,
    pub corrected_answer: String,
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordOperatorFeedbackResponse {
    pub routing: crate::harness::correction::CorrectionRouting,
    pub root_cause: Option<String>,
    pub audit_event_id: String,
}
```

処理:
1. `authority = if req.feedback_source == "customer" { NonAuthoritative } else { Authoritative }`（staff の authn は begin で済んでいる。顧客の主張の中継は non_authoritative）。
2. `NonAuthoritative` の場合: **永続層に書かない**。`operator_feedback` ノードも書かず、WORM にのみ `decision: "correction:conversation_only"` を append して `ConversationOnly` を返す（S1-8 条件 3。監査は永続「知識」層ではない）。
3. `Authoritative` の場合: `root_cause = harness.root_cause_probe(&ctx, &req.corrected_answer, &self.tools).await?` → `routing = correction_intake(authority, root_cause)`。
4. `SearchImprovementQueue` → `harness.enqueue_search_improvement(...)`（known_resolution を増やさない。S1-8 条件 4）。`KnownResolutionCandidate` → `operator_feedback` ノードに routing を記録（KR の実 insert は `add_known_resolution` で担当者が明示的に行う。適用条件は人間が書く）。
5. いずれも WORM append。

- [ ] **Step 5: `add_known_resolution` を実装する（GMR の進化 = 例外ルールの離散 insert）**

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddKnownResolutionRequest {
    /// 適用条件となる signal 値の集合（specs/signal-vocabulary.md の語彙）
    pub signals: Vec<String>,
    /// 商品/ロット/契約/時期などの適用条件。人間が明示的に書く（LLM に推測させない）
    pub applicability: String,
    /// その条件下での正しい答え（OK 回答も NG 回答も同枠）
    pub answer: String,
    /// どの escalation 起点か
    pub origin_escalation_id: Option<String>,
    /// 根拠となる manual section（BECAUSE 辺で結線）
    pub rationale_section_keys: Vec<String>,
}
```

ガード（tool handler でなく先頭の検証として明示。判断は Harness 関数の組合せ）:
1. `ctx.actor.role` が `Supervisor` / `Admin` でなければ `permission_denied` エラー（authoritative の担い手）。
2. `signals` が空、または lexicon に無い signal 値が含まれていればエラー（語彙外の条件は照合不能）。
3. `answer` を `egress_gate` に通し、`Block` ならエラー（NG 語を含む知識を登録させない）。
4. binding は常に `advisory` で書く（mandatory は自動で書けない。S1-5 不変条件）。

処理: `NewKnownResolution { signal_set, applicability, answer, origin, created_by: ctx.actor.sub, rationale_section_keys }` → `knowledge.insert_known_resolution(&ctx.schema, &kr)` → WORM append（`decision: "kr_insert"`, `governing_norm_ids: [kr_id]`）→ `kr_id` を返す。

- [ ] **Step 6: ビルド・テスト確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo check 2>&1 | tail -3 && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test 2>&1 | tail -3`
Expected: check `Finished`、test 全 PASS

- [ ] **Step 7: Commit**

```bash
git add server/src/rmcp_server.rs server/src/mcp.rs
git commit -m "feat: add record/search/known-resolution tools with egress gate and correction intake"
```

---

### Task 14: 受け入れ検証（S1-8 Done 条件）+ 実機検証 + PR

**Files:**
- Modify: `specs/production-cs-mcp.md`（実装で判明した差分があれば同ターンで反映）
- Modify: `README.md`（新 tool・ingest_rules・JWT 設定の起動手順を追記）

- [ ] **Step 1: fmt / check / test をフル実行**

Run:
```bash
cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo fmt --check && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo check --bins && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test 2>&1 | tail -5
```
Expected: fmt 差分なし、check `Finished`、test 全 PASS。失敗したら `superpowers:systematic-debugging` を起動して修正（推測修正禁止）。

- [ ] **Step 2: schema 加算 + ルール投入（実機: vegapunk.local）**

```bash
cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= VEGAPUNK_BEARER_TOKEN_FILE=/private/tmp/vegapunk-bearer-token cargo run --bin ingest_rules -- --endpoint http://vegapunk.local:6840 --schema sivira-cs-demo --schema-file ../schema/cs-schema.yml --rules-file data/rules.sample.json
```
Expected: `schema updated (additive)` と `ingested layer1 rules and layer2 domains`（node_count=4）。token が無い場合は CLAUDE.md の手順で取得。ネットワーク不通で実行できない場合は、その旨と理由を完了報告に明記する。

- [ ] **Step 3: ローカル MCP サーバを起動して E2E 確認**

CLAUDE.md「ローカル MCP 起動手順」どおり起動（config.local-https.toml / 3443 / vegapunk.local:6840）。その後:

```bash
curl -ksS https://127.0.0.1:3443/healthz
```
Expected: `ok`

`initialize` → `mcp-session-id` 取得 → `tools/list` で 12 tool（resolve_product / search_manual / get_section / get_product / search_past_cases / search_known_resolutions / evaluate_answerability / record_answer_attempt / record_answer_outcome / record_operator_feedback / create_escalation_event / add_known_resolution）が出ることを確認。`upsert_product` / `upsert_section` / `upsert_spec` が **出ていない**ことも確認。

- [ ] **Step 4: S1-8 Done 条件の突合（実機 tools/call）**

各条件を `tools/call` で検証し、結果を記録する:

1. **3 層短絡**: `evaluate_answerability {"question":"サプリを食べたら腹痛がする"}` → `escalate layer=1 route_to=safety_team`。`{"question":"肌がピリピリするが使い続けてよいか"}` → layer 1（skin_irritation + continue_use_question）。`{"question":"薬との飲み合わせは？"}` → layer 2。
2. **egress_gate**: `record_answer_attempt {"draft":"必ず治りますのでご安心ください","question":"..."}` → `block` / `emit_allowed=false`。`{"draft":"継続すると治ると言われています"}` → `abstain`。
3. **non_authoritative 非永続**: `record_operator_feedback {"feedback_source":"customer","corrected_answer":"..."}` → `conversation_only`。その後 `query_nodes`（verify 手段がなければ `search_past_cases`）で operator_feedback ノードが増えていないこと。
4. **retrieval_miss**: マニュアルに存在する記述を corrected_answer に渡す → `search_improvement_queue` になり、`data/audit/search-improvement-queue.jsonl` に 1 行増え、KnownResolution が増えないこと。
5. **stakes=high の fail-open 補足**: `evaluate_answerability {"question":"これを飲むと治るのでしょうか"}`（第2層の列挙に無い表現・NG 近接）→ `escalate layer=3`（threshold=0.95 を満たせない）。
6. **予約フィールド**: `add_known_resolution`（supervisor JWT で）→ 返った kr_id のノードに binding/registration_trigger/knowledge_class/outcome_ref 属性が存在（`verify_demo` または snapshot で確認）。
7. **関数境界**: コードレビュー観点（rmcp_server.rs に判定 if 文が直書きされていないこと）。
8. **WORM**: `data/audit/audit.jsonl` に上記全操作の行があり、provenance キーが揃い、hash chain が連続していること。
9. **client scope 不達**: tool 入力スキーマに schema/tenant/label/sensitivity フィールドが存在しないこと（tools/list の inputSchema を目視）+ 他 schema を allowed_schemas に持たない actor の JWT で 403 相当になること。
10. **グラフ格納**: 6 の kr について HAS_SIGNAL 辺 / BECAUSE 辺が張られていること（`verify_demo` の snapshot 出力で確認）。

KR 再利用の進化シナリオも通す: 「変色」KR 追加 → `evaluate_answerability("色が変わったが食べてもいいか")` = KR 再利用 / `("変色していてカビもあるが食べてもいいか")` = escalate（UnknownAddedSignal）→ 「変色+カビ」KR 追加 → 同じ質問が KR 適用に変わる。

**ロードマップ遵守事項（spec 改訂 2026-07-03 追加分）の突合:**

11. **会話層のマルチターン再判定（遵守事項 1）**: ターン 1 `evaluate_answerability {"question":"商品の色が変わったのですが"}` →（「変色」KR 登録済み前提で）KR 適用・返却された `case_id` を控える。ターン 2 同じ `case_id` で `{"question":"よく見るとカビも生えていました"}` → 累積集合 {discoloration, mold} で再判定され escalate（UnknownAddedSignal）に**自動的に倒れる**こと。`accumulated_signals` に両 signal が入っていること。第1・2層エスカレーション時に `clarification_allowed=false`、第3層グレー時に `true` であること。
12. **grade 運用（遵守事項 3）**: `record_answer_outcome` を supervisor と operator の 2 actor で `resolved` × 3 回（`known_resolution_id` 付き）→ 該当 KR の `grade` が `auto_answer_audited` に昇格。その後 `wrong_answer` × 2 回 → `demoted` に降格。各遷移が WORM に `regrade:` として残ること。
13. **EmitContext / 断片入力（遵守事項 2・4）**: コードレビュー観点 — `EmitChannel` に operator / customer_chat / customer_voice の 3 値があり Step 1 の呼び出しが operator 固定であること、`egress_gate` が文単位の断片でも同じ verdict を返すこと（単体テスト `gate_accepts_sentence_fragments` / `verdict_is_channel_invariant` で担保済みを確認）。

- [ ] **Step 5: 仕様書・README の更新**

実装と spec の差分（あれば）を `specs/production-cs-mcp.md` に反映。`README.md` に ingest_rules / JWT 設定 / 新 tool 一覧を追記。

- [ ] **Step 6: Commit**

```bash
git add specs/ README.md
git commit -m "docs: record step1 acceptance results and update runbook"
```

- [ ] **Step 7: レビューと PR**

CLAUDE.md のコミット前チェックリストに従う:
1. `simplify` スキルで変更コードを見直す。
2. `code-review` スキルのフローを実行する（省略・代替禁止）。
3. `superpowers:requesting-code-review` / `superpowers:finishing-a-development-branch` で PR 作成。
4. 完了宣言前に `superpowers:verification-before-completion` を起動し、test / check / 実機出力の実物を確認する。

---

## Self-Review 結果（計画作成時に実施済み）

**Spec coverage（S1-0〜S1-11 + フェーズロードマップ遵守事項 → タスク対応。spec 改訂 2026-07-03 反映済み）:**
- S1-0 三原則: 関数境界 = Task 6/7/8、グラフ格納 = Task 10、handler 直書き禁止 = Task 12/13 + Done 条件 7
- S1-0 会話層（最小）: Task 11（累積 signal 集合・毎ターン再判定）+ Task 12（case_id / accumulated_signals / clarification_allowed）+ Task 14 検証 11
- S1-1 パイプライン: Task 11（begin / evaluate）、client 入力破棄 = Task 4 + tool 入力に scope 系フィールド無し（Task 12/13）、[emit or escalate] の担当者直接応答セマンティクス = Task 13 Step 2
- S1-2 record type と格納先: Task 10（schema 加算）+ Task 13（record 系 tool）+ Task 9（WORM 別建て）
- S1-3 known_resolution 型・照合: Task 5（照合）+ Task 10（グラフ表現・予約フィールド）
- S1-4 egress_gate: Task 7（EmitContext channel 3 値・断片入力・チャネル非依存の判定水準）+ Task 13 Step 2（egress 位置固定）
- S1-5 correction_intake: Task 8 + Task 13 Step 4/5（mandatory 自動書込み禁止は Task 13 Step 5 ガード 4）
- S1-6 stakes: Task 6（classify_stakes / answerability_threshold、prohibited_domain.binding 参照は Task 11 の stakes_input 算出）
- S1-7 12 tool: Task 12（5 本）+ Task 13（7 本）
- S1-8 Done 条件 10 項: Task 14 Step 4 で 1:1 突合
- S1-9 確定事項: signal 語彙 = Task 1、NG 辞書 = Task 1、prohibited_domain 初期リスト = Task 10 Step 2
- S1-11: lexicon 実装 = Task 2、JWT = Task 3、会話層のサーバ側永続 = Task 10/11、grading config = Task 3/6
- ロードマップ遵守事項 1（会話層）: Task 11 + 検証 11 / 遵守事項 2（egress 断片）: Task 7 / 遵守事項 3（grade 運用）: Task 6 Step 5-8 + Task 13 Step 3 + 検証 12 / 遵守事項 4（EmitContext チャネル）: Task 7 + 検証 13

**既知の限界（意図的スコープ外、spec の「今回作らない」+ フェーズ計画準拠）:**
- C′ 含意判定 / Π 連続スコア / CIRG フル / L・Λ / 共有-規制層 / KPI 帰属: 器（予約フィールド・関数境界）のみ。
- grade しきい値の具体値: config 注入 + 仮置き既定値（N=3 / M=2 / r=0.2 / K=2）。業務確認で確定させる（S1-9 未決 → S1-11 追記 4）。降格中 KR の再昇格は Step 1 では自動化しない（人手見直し）。
- Step 2 / Step 3 の実行系（顧客直チャネル・ASR/TTS・逐次処理系）: 作らない。前方互換の構造（EmitChannel 3 値・egress 断片入力・会話層・grade）だけ Step 1 に置く。
- sensitivity / label 軸: 構造予約のみ（S1-9 確定 (a)）。
- 音声チャネルの勧告警告: 分岐 enum のみ。

**Type consistency:** Interfaces 総覧の型名・関数名を全タスクで統一済み（`match_known_resolution` の戻りは `KrMatch`、`Harness.knowledge` は `Option<KnowledgeStore>`、config は `ThresholdsConfig` → `decision::Thresholds`、`GradingConfig` → `grading::GradingThresholds` へ変換、`egress_gate(text, &EmitContext, &NgDictionary)` は Task 7 / 13 / add_known_resolution ガードで同一シグネチャ）。

**Placeholder scan:** 「TBD / 後で実装 / 適切に処理」なし。Task 13 は tool ごとの完全なリクエスト型と処理手順を記載（コード量削減のため処理は番号付き手順で規定し、型は全て明示）。
