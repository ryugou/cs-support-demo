# Manual-Domain Template + URTECT Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** cs-support-mcp に「汎用マニュアル領域スキーマ＋そのスキーマに対する retrieval」を実装し、URTECT（防犯カメラ）マニュアルを最初の実データとして ingest・検証できる状態にする。

**Architecture:** マニュアル領域は商材非依存の固定スキーマ形（`ManualDocument` / `ManualSection` / `Product` + `HAS_SECTION` / `PARENT_OF` / `DESCRIBES` / `MENTIONS_SIGNAL` / `BASED_ON`）を「汎用テンプレート」として 1 ファイルに定義し、テナントごとに schema 名だけ変えて登録する（URTECT は schema 名 `urtect`）。retrieval（search_manual / get_section / resolve_product / add_known_resolution / evaluate の直接性判定）はこの固定スキーマ形にバインドして書き換え、特定商材にハードコードしない。既存 `sivira-cs-demo`（旧 `section`/`body_ja` バイリンガルデモ）は legacy として別コードパスに凍結し壊さない。ingest はサイト固有スクレイパ `ingest_urtect`（Rust）。

**Tech Stack:** Rust (edition 2021), axum 0.8, rmcp 1.7, tonic/prost gRPC, 追加: `reqwest = { version = "0.12", features = ["blocking"] }` は使わず tokio 非同期の `reqwest = "0.12"`、`scraper = "0.20"`（HTML 抽出）、`url = "2"`（URL 正規化）。既存: `sha2`（content_hash）, `uuid`, `chrono`。

## Global Constraints

- Python / TypeScript を使わない。tsx / npm / package.json を追加しない。ingest は Rust bin。
- vegapunk API を推測しない。使ってよい RPC は検証済みのみ: `QueryNodes` / `GetGraphSnapshot` / `UpsertNodes` / `UpsertEdges` / `Search` / `CreateSchema` / `UpdateSchema` / `GetSchema`。新規 RPC が要れば `// TODO: bind to vegapunk <api>` + config 境界。
- ローカルで vegapunk を起動しない。SSH tunnel を張らない。gRPC 接続は `http://vegapunk.local:6840`（token: `/private/tmp/vegapunk-bearer-token`）。
- スキーマ変更は加算のみ（I3）。既存 node/edge/属性を削除・改名しない。
- scope はサーバ導出（I1）。tool 入力に schema/tenant/label/sensitivity を足さない。
- 判断ロジックを tool handler に直書きしない。判定は Harness 経由。汎用化で tool handler の直接検索パスを作らない（Harness 経由原則）。
- signal_set を JSON 配列属性に畳まない（I2）。Signal ノード + 辺で持つ。
- 翻訳予約 `body_original` / `original_hash` は**純予約**。ingest は空のまま書き、retrieval・判定コードは**一切読まない**。使う駆動コードを書かない。
- **テンプレートの形は「確定（v1 固定）」しない**。URTECT 検証で改訂しうる前提で、加算のみを守りつつ revisable に保つ。過剰な汎用化（config 駆動の node type 差し替え等）はしない（YAGNI）— スキーマ形は固定・retrieval はその形にバインド、という 1 段の汎用性のみ。
- テストの Rust 検証コマンドは `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo <cmd>`（RUSTC_WRAPPER の外し必須。以降 `CARGO` と略記）。
- 過剰エスカレーションの調整は第3層（直接性しきい値・ManualSection 分割・MENTIONS_SIGNAL 付与）のみで行う。第1・2層と NG 辞書は緩めない。
- コミットは Conventional Commits。main 直接 push 禁止。作業は `feat/manual-domain-urtect` ブランチ。
- 実装/仕様の結論が変わったら同ターンで `/Users/ryugo/Desktop/urtect-pilot-design.md`（本フェーズ spec）と齟齬がないか確認する（Desktop の spec 本体は勝手に書き換えない。差分が要れば提示する）。

## 設計判断（本計画で確定するもの）

- **retrieval のスキーマ二系統をコードで分岐する**: `ToolService` に「旧 `section` スキーマ用（sivira-cs-demo legacy）」と「マニュアル領域スキーマ用（ManualSection）」の 2 実装を持つ。分岐は project/schema 設定で選ぶ（`ProjectConfig.manual_schema` = `legacy_section` | `manual_v1`）。これにより sivira-cs-demo を壊さず、URTECT は新実装を通る。config 駆動の「node type/field 名差し替え」はしない（形は 2 つに固定）。
- **直接性 retrieval は 2 経路の max**: (A) signal → `MENTIONS_SIGNAL` 逆辺で絞った候補への `section_score(question, title+body)`、(B) body テキスト全文検索の `section_score`。`best_manual_score = max(A, B)`。signal が立たない質問でも (B) で拾える（recall を落とさない）。
- **KR の根拠を 2 辺に分離**: `add_known_resolution` は「担当者の判断理由テキスト → `Rationale` ノード + `BECAUSE` 辺」と「マニュアル出典 → `BASED_ON` → ManualSection 辺」を別々に張る。tool 契約を `rationale_text: Option<String>` + `manual_section_keys: Vec<String>` に変更する（旧 `rationale_section_keys` は廃止）。
- **第3層 default 振り先を config 化**: `[harness] default_escalation_route`（既定 `"triage"`）を追加し、URTECT config で `"support_desk"` にする。decide() のハードコード `"triage"` を置換。

## File Structure

```text
server/
  Cargo.toml                         # 変更: reqwest / scraper / url 追加
  src/
    lib.rs                           # 変更: pub mod manual; pub bin 追加なし
    config.rs                        # 変更: ProjectConfig.manual_schema, HarnessConfig.default_escalation_route
    model.rs                         # 変更: ManualHit / ManualSectionView / ManualProductCandidate 追加（既存 SectionHit 等は温存）
    mcp.rs                           # 変更: legacy retrieval はそのまま。新実装は manual.rs へ委譲する薄い分岐
    manual/
      mod.rs                         # 新規: pub mod schema_ids; retrieval; ingest_model;
      schema_ids.rs                  # 新規: manual_node_id / doc/section/product id 規約（urtect:gen1:sec-<slug> 等）
      retrieval.rs                   # 新規: ManualStore（search / get_section / resolve_product / signal→MENTIONS_SIGNAL 絞り込み）
      ingest_model.rs                # 新規: ManualSection/Document/Product → GraphNode/Edge 組み立て（純関数・テスト対象）
    harness/
      mod.rs                         # 変更: evaluate の manual 取得を manual_schema 分岐、直接性 max(A,B)、default route config
      knowledge.rs                   # 変更: NewKnownResolution に rationale_text/manual_section_keys、Rationale+BASED_ON 生成
      decision.rs                    # 変更: layer3 route_to をハードコードでなく入力から受ける
    rmcp_server.rs                   # 変更: add_known_resolution 契約変更、search_manual/get_section/resolve_product/evaluate を manual_schema 分岐
    bin/
      ingest_urtect.rs               # 新規: Google Sites 自動列挙・fetch・抽出・ManualSection グラフ upsert・網羅率レポート
  schema/
    cs-support.yml                   # 新規: 汎用マニュアルテンプレ（knowledge 型 + Product/ManualDocument/ManualSection + 辺）。schema 名は登録時に指定
  data/
    urtect/
      signal-lexicon.json            # 新規: §4 URTECT 語彙（class + surface_forms）
      ng-dictionary.json             # 新規: §5 URTECT NG 辞書
      rules.json                     # 新規: §5 第1層5・第2層3
    (既存 signal-lexicon.json 等 = sivira 用は温存)
```

各ファイルは 1 責務。`manual/retrieval.rs` は I/O（vegapunk 読み）、`manual/ingest_model.rs` は純関数（グラフ組み立て・テスト対象）、`manual/schema_ids.rs` は id 規約。判定は harness に残す。

## Interfaces 総覧（タスク間契約）

```rust
// manual/schema_ids.rs
pub fn manual_node_id(schema: &str, kind: &str, key: &str) -> String;  // "{schema}:gen1:{kind}:{key}"
pub fn section_slug(url: &str) -> String;                              // URL末尾 → "sec-1-4-sd-not-recognized"
pub const KIND_DOC: &str; pub const KIND_SECTION: &str; pub const KIND_PRODUCT: &str;

// manual/ingest_model.rs
pub struct ManualSectionInput { pub slug: String, pub title: String, pub body: String,
  pub source_url: String, pub breadcrumb: String, pub section_no: Option<String>, pub order: i32,
  pub parent_slug: Option<String>, pub product_models: Vec<String>, pub signal_values: Vec<String> }
pub struct ManualProductInput { pub model: String, pub name: String, pub aliases: Vec<String> }
pub fn build_document_node(schema, doc_key, title, source_url, fetched_at) -> GraphNode;
pub fn build_product_node(schema, &ManualProductInput) -> GraphNode;
pub fn build_section_graph(schema, doc_key, &ManualSectionInput, content_hash) -> GraphBuild;  // ManualSection node + HAS_SECTION + PARENT_OF + DESCRIBES + MENTIONS_SIGNAL 辺
pub fn content_hash(normalized_body: &str) -> String;                 // sha256 hex

// manual/retrieval.rs
pub struct ManualStore; // new(Arc<VegapunkClient>)
pub struct ManualHit { pub section_key: String, pub title: String, pub body: String,
  pub source_url: String, pub breadcrumb: String, pub score: f32 }
// signal 絞り込み + body 全文の max。snapshot 共有版も持つ。
pub async fn search(&self, schema, question, signals: &SignalSet, top_k) -> Result<Vec<ManualHit>>;
pub async fn search_with_snapshot(&self, schema, question, signals, top_k, &Snapshot) -> Result<Vec<ManualHit>>;
pub async fn get_section(&self, schema, section_key) -> Result<ManualSectionView>;
pub async fn resolve_product(&self, schema, text) -> Result<Vec<ManualProductCandidate>>;

// model.rs（新規・schemars 対応）
pub struct ManualProductCandidate { pub model: String, pub name: String, pub score: f32, pub reason: String }
pub struct ManualSectionView { pub section: serde_json::Value, pub ancestors: Vec<Value>, pub children: Vec<Value>, pub based_on_rationale: Vec<Value> }

// harness/knowledge.rs（NewKnownResolution 変更）
pub struct NewKnownResolution { pub signal_set: SignalSet, pub applicability: String, pub answer: String,
  pub origin: String, pub created_by: String,
  pub rationale_text: Option<String>,        // → Rationale ノード + BECAUSE
  pub manual_section_keys: Vec<String> }      // → BASED_ON → ManualSection

// config.rs
pub enum ManualSchemaKind { LegacySection, ManualV1 }   // serde snake_case
// ProjectConfig.manual_schema: ManualSchemaKind (default LegacySection)
// HarnessConfig.default_escalation_route: String (default "triage")
```

---

### Task 0: ブランチ作成 + 依存追加 + 検証

**Files:**
- Modify: `server/Cargo.toml`

- [ ] **Step 1: ブランチ作成**

```bash
cd /Users/ryugo/Developer/src/AI-x-EC/cs-support-demo
git switch -c feat/manual-domain-urtect
```

- [ ] **Step 2: 依存追加**

`server/Cargo.toml` の `[dependencies]` に追記:

```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
scraper = "0.20"
url = "2"
```

- [ ] **Step 3: `ingest_urtect` bin 宣言を追加**

`server/Cargo.toml` に追記:

```toml
[[bin]]
name = "ingest_urtect"
path = "src/bin/ingest_urtect.rs"
```

- [ ] **Step 4: 既存ビルドが通ることを確認（bin 実体は後続タスクで作成するので一旦コメントアウト or 空ファイル）**

まず `server/src/bin/ingest_urtect.rs` を最小の空 main で作る:

```rust
fn main() {
    eprintln!("ingest_urtect: not yet implemented");
}
```

Run: `cd server && CARGO check 2>&1 | tail -3`
Expected: `Finished`

- [ ] **Step 5: Commit**

```bash
git add server/Cargo.toml server/Cargo.lock server/src/bin/ingest_urtect.rs
git commit -m "chore: add reqwest/scraper/url deps and ingest_urtect bin scaffold"
```

---

### Task 1: 汎用マニュアルテンプレスキーマ `schema/cs-support.yml`

**Files:**
- Create: `server/../schema/cs-support.yml`（リポジトリ直下 `schema/cs-support.yml`）
- Test: 手動 yaml lint + Task 8 の実機登録で検証

**Interfaces:**
- Produces: 汎用マニュアルテンプレ。knowledge 型（既存 cs-schema.yml と同一定義）+ Product/ManualDocument/ManualSection + 辺。schema 名は登録時に `--schema urtect` で与える。

このスキーマは**商材非依存の形**。URTECT 固有値はデータ（signal/rules/section）側にあり、ここには無い。

- [ ] **Step 1: `schema/cs-support.yml` を書く**

既存 `schema/cs-schema.yml` の **knowledge 型（KnownResolution / Signal / EscalationRule / ProhibitedDomain / support_case / answer_attempt / answer_evidence / operator_feedback / escalation_event）と `Rationale`（無ければ追加）** をコピーし、マニュアル領域型を加える。旧デモの product/document/section/spec は含めない（テンプレはマニュアル v1 形）。

```yaml
name: cs-support-manual
version: 1
min_compatible: 1
description: "Generic manual-domain CS schema (knowledge types + manual document/section). Registered per tenant by schema name."

nodes:
  # --- knowledge types (Step 1 harness。cs-schema.yml と同一) ---
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
  Rationale:
    attributes:
      rationale_id: { type: string, required: true }
      text: { type: string, required: true }
  EscalationRule:
    attributes:
      rule_id: { type: string, required: true }
      condition: { type: string, required: true }
      owner: { type: string }
      sensitivity: { type: string }
      route: { type: string, required: true }
      binding: { type: string }
  ProhibitedDomain:
    attributes:
      domain_id: { type: string, required: true }
      pattern: { type: string, required: true }
      domain_signals: { type: string }
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
      last_request_id: { type: string }
      last_decision: { type: string }
      last_kr_id: { type: string }
  answer_attempt:
    attributes:
      attempt_id: { type: string, required: true }
      case_id: { type: string }
      request_id: { type: string }
      actor: { type: string }
      draft: { type: string }
      decision: { type: string }
      evaluation_request_id: { type: string }
      known_resolution_id: { type: string }
      egress_verdict: { type: string }
      outcome: { type: string }
      outcome_actor: { type: string }
      outcome_note: { type: string }
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
      case_id: { type: string }
      request_id: { type: string }
      actor: { type: string }
      layer: { type: int }
      reason: { type: string }
      route_to: { type: string }
      question: { type: string }
      created_at: { type: string }
  # --- manual domain (template v1) ---
  Product:
    attributes:
      product_key: { type: string, required: true }   # = model（安定 upsert キー）
      name: { type: string, required: true }
      model: { type: string }
      aliases: { type: string }
  ManualDocument:
    attributes:
      doc_key: { type: string, required: true }
      title: { type: string, required: true }
      source_url: { type: string, required: true }
      fetched_at: { type: string, required: true }
  ManualSection:
    attributes:
      section_key: { type: string, required: true }
      doc_key: { type: string }
      title: { type: string, required: true }
      body: { type: string, required: true }
      source_url: { type: string, required: true }
      breadcrumb: { type: string, required: true }
      section_no: { type: string }
      order: { type: int, required: true }
      source_lang: { type: string, required: true }
      content_hash: { type: string, required: true }
      # 多言語予約（純予約・空可・駆動コードは読まない）
      body_original: { type: string }
      original_hash: { type: string }

edges:
  HAS_SIGNAL: { from: [KnownResolution, support_case], to: Signal }
  BECAUSE: { from: KnownResolution, to: Rationale }
  HAS_SECTION: { from: ManualDocument, to: ManualSection }
  PARENT_OF: { from: ManualSection, to: ManualSection }
  DESCRIBES: { from: ManualSection, to: Product }
  MENTIONS_SIGNAL: { from: ManualSection, to: Signal }
  BASED_ON: { from: KnownResolution, to: ManualSection }

traceable_pairs:
  - claim: KnownResolution
    evidence: Rationale
    edge: BECAUSE
  - claim: KnownResolution
    evidence: ManualSection
    edge: BASED_ON
```

注: `BECAUSE` の to を `Rationale` に確定（旧 cs-schema.yml は `BECAUSE: {from: KnownResolution, to: section}` だが、これは別 schema ファイル。cs-support.yml は独立定義なので衝突しない）。

- [ ] **Step 2: yaml 構文確認**

Run: `cd /Users/ryugo/Developer/src/AI-x-EC/cs-support-demo && ruby -ryaml -e "YAML.load_file('schema/cs-support.yml'); puts 'yaml ok'"`
Expected: `yaml ok`

- [ ] **Step 3: Commit**

```bash
git add schema/cs-support.yml
git commit -m "feat: add generic manual-domain schema template (manual v1 shape)"
```

---

### Task 2: config 拡張（manual_schema 分岐 + default_escalation_route）

**Files:**
- Modify: `server/src/config.rs`
- Test: `server/src/config.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Produces: `ManualSchemaKind` / `ProjectConfig.manual_schema` / `HarnessConfig.default_escalation_route`

- [ ] **Step 1: 失敗するテストを書く**

`server/src/config.rs` 末尾に:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_defaults_to_legacy_section() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(matches!(cfg.projects[0].manual_schema, ManualSchemaKind::LegacySection));
    }

    #[test]
    fn project_can_select_manual_v1() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "urtect"
manual_schema = "manual_v1"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(matches!(cfg.projects[0].manual_schema, ManualSchemaKind::ManualV1));
    }

    #[test]
    fn default_escalation_route_defaults_to_triage() {
        assert_eq!(HarnessConfig::default().default_escalation_route, "triage");
    }
}
```

- [ ] **Step 2: テスト失敗を確認**

Run: `cd server && CARGO test --lib config:: 2>&1 | tail -5`
Expected: FAIL（`ManualSchemaKind` 未定義）

- [ ] **Step 3: 実装**

`ProjectConfig` に追加、`ManualSchemaKind` を定義:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ManualSchemaKind {
    #[default]
    LegacySection,
    ManualV1,
}
```

`ProjectConfig` に:

```rust
    #[serde(default)]
    pub manual_schema: ManualSchemaKind,
```

`HarnessConfig` に:

```rust
    #[serde(default = "default_escalation_route")]
    pub default_escalation_route: String,
```

`default_escalation_route` fn と `HarnessConfig::default()` の該当行:

```rust
fn default_escalation_route() -> String {
    "triage".to_string()
}
```

`impl Default for HarnessConfig` に `default_escalation_route: default_escalation_route(),` を追加。

- [ ] **Step 4: テスト通過を確認**

Run: `cd server && CARGO test --lib config:: 2>&1 | tail -3`
Expected: `test result: ok. 3 passed`

- [ ] **Step 5: Commit**

```bash
git add server/src/config.rs
git commit -m "feat: add manual_schema selector and configurable default escalation route"
```

---

### Task 3: manual/schema_ids.rs + ingest_model.rs（グラフ組み立て・純関数）

**Files:**
- Create: `server/src/manual/mod.rs`, `server/src/manual/schema_ids.rs`, `server/src/manual/ingest_model.rs`
- Modify: `server/src/lib.rs`（`pub mod manual;`）
- Test: 各ファイル内 `#[cfg(test)]`

**Interfaces:**
- Consumes: `crate::model::{GraphNode, GraphEdge, GraphBuild}`
- Produces: 総覧の schema_ids / ingest_model API

- [ ] **Step 1: `lib.rs` に `pub mod manual;` を追加、`manual/mod.rs` を作る**

`server/src/manual/mod.rs`:

```rust
pub mod ingest_model;
pub mod retrieval;
pub mod schema_ids;
```

（`retrieval` は Task 4 で実装。まず空ファイル `server/src/manual/retrieval.rs` に `// filled in Task 4` を置く）

- [ ] **Step 2: schema_ids.rs の失敗テスト**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn node_id_has_generation_prefix() {
        assert_eq!(manual_node_id("urtect", KIND_SECTION, "sec-1-4-sd"),
                   "urtect:gen1:ManualSection:sec-1-4-sd");
    }
    #[test]
    fn slug_from_url_tail_is_stable_and_ascii_kebab() {
        assert_eq!(section_slug("https://sites.google.com/view/urtect-manual/1-4/sd-not-recognized"),
                   "sec-sd-not-recognized");
        // 同一 URL は同一 slug（冪等キー）
        assert_eq!(section_slug("https://x/a/b/"), section_slug("https://x/a/b"));
    }
}
```

- [ ] **Step 3: schema_ids.rs 実装**

```rust
pub const KIND_DOC: &str = "ManualDocument";
pub const KIND_SECTION: &str = "ManualSection";
pub const KIND_PRODUCT: &str = "Product";

/// vegapunk read API は generation prefix でスコープする。新規スキーマは gen1 始まり。
pub fn manual_node_id(schema: &str, kind: &str, key: &str) -> String {
    format!("{schema}:gen1:{kind}:{key}")
}

/// URL 末尾セグメントから安定 slug（再 ingest の冪等 upsert キー）。
pub fn section_slug(url: &str) -> String {
    let tail = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_lowercase();
    let kebab: String = tail
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let squeezed = kebab
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("sec-{squeezed}")
}
```

- [ ] **Step 4: schema_ids テスト通過確認**

Run: `cd server && CARGO test --lib manual::schema_ids 2>&1 | tail -3`
Expected: `test result: ok. 2 passed`

- [ ] **Step 5: ingest_model.rs の失敗テスト**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ManualSectionInput {
        ManualSectionInput {
            slug: "sec-sd-not-recognized".into(),
            title: "SDカードが認識されない".into(),
            body: "SDカードを一度抜き差ししてください。".into(),
            source_url: "https://x/1-4/sd".into(),
            breadcrumb: "1.4 こんなときは > SDカードが認識されない".into(),
            section_no: Some("1.4".into()),
            order: 12,
            parent_slug: Some("sec-1-4".into()),
            product_models: vec!["ADC-V724".into()],
            signal_values: vec!["sd_not_recognized".into()],
        }
    }

    #[test]
    fn section_graph_builds_manual_section_and_edges() {
        let build = build_section_graph("urtect", "doc-manual", &sample(), "abc123");
        let sec = build.nodes.iter().find(|n| n.node_type == "ManualSection").unwrap();
        // body は属性、source_url / order / content_hash が入る。翻訳予約は空
        assert!(sec.attributes.iter().any(|(k, v)| k == "source_url" && v == "https://x/1-4/sd"));
        assert!(sec.attributes.iter().any(|(k, v)| k == "content_hash" && v == "abc123"));
        assert!(sec.attributes.iter().any(|(k, v)| k == "order" && v == "12"));
        assert!(sec.attributes.iter().any(|(k, v)| k == "source_lang" && v == "ja"));
        assert!(sec.attributes.iter().any(|(k, _)| k == "body_original") == false); // 純予約は書かない
        // 辺: PARENT_OF（親）/ DESCRIBES（Product）/ MENTIONS_SIGNAL（Signal）
        assert_eq!(build.edges.iter().filter(|e| e.edge_type == "PARENT_OF").count(), 1);
        assert_eq!(build.edges.iter().filter(|e| e.edge_type == "DESCRIBES").count(), 1);
        assert_eq!(build.edges.iter().filter(|e| e.edge_type == "MENTIONS_SIGNAL").count(), 1);
        // Signal ノードも作る（第一級ノード・I2）
        assert_eq!(build.nodes.iter().filter(|n| n.node_type == "Signal").count(), 1);
    }

    #[test]
    fn root_section_has_no_parent_edge() {
        let mut s = sample();
        s.parent_slug = None;
        let build = build_section_graph("urtect", "doc-manual", &s, "h");
        assert_eq!(build.edges.iter().filter(|e| e.edge_type == "PARENT_OF").count(), 0);
    }

    #[test]
    fn content_hash_is_stable() {
        assert_eq!(content_hash("同じ本文"), content_hash("同じ本文"));
        assert_ne!(content_hash("A"), content_hash("B"));
    }
}
```

- [ ] **Step 6: ingest_model.rs 実装**

```rust
use crate::manual::schema_ids::{manual_node_id, KIND_DOC, KIND_PRODUCT, KIND_SECTION};
use crate::model::{GraphBuild, GraphEdge, GraphNode};
use sha2::{Digest, Sha256};

pub struct ManualSectionInput {
    pub slug: String,
    pub title: String,
    pub body: String,
    pub source_url: String,
    pub breadcrumb: String,
    pub section_no: Option<String>,
    pub order: i32,
    pub parent_slug: Option<String>,
    pub product_models: Vec<String>,
    pub signal_values: Vec<String>,
}

pub struct ManualProductInput {
    pub model: String,
    pub name: String,
    pub aliases: Vec<String>,
}

pub fn content_hash(normalized_body: &str) -> String {
    format!("{:x}", Sha256::digest(normalized_body.as_bytes()))
}

pub fn build_document_node(
    schema: &str,
    doc_key: &str,
    title: &str,
    source_url: &str,
    fetched_at: &str,
) -> GraphNode {
    GraphNode {
        id: manual_node_id(schema, KIND_DOC, doc_key),
        node_type: KIND_DOC.to_string(),
        attributes: vec![
            ("doc_key".to_string(), doc_key.to_string()),
            ("title".to_string(), title.to_string()),
            ("source_url".to_string(), source_url.to_string()),
            ("fetched_at".to_string(), fetched_at.to_string()),
        ],
    }
}

pub fn build_product_node(schema: &str, p: &ManualProductInput) -> GraphNode {
    GraphNode {
        id: manual_node_id(schema, KIND_PRODUCT, &p.model),
        node_type: KIND_PRODUCT.to_string(),
        attributes: vec![
            ("product_key".to_string(), p.model.clone()),
            ("name".to_string(), p.name.clone()),
            ("model".to_string(), p.model.clone()),
            ("aliases".to_string(), p.aliases.join(",")),
        ],
    }
}

/// ManualSection ノード + HAS_SECTION/PARENT_OF/DESCRIBES/MENTIONS_SIGNAL 辺 + Signal ノード。
/// 翻訳予約 body_original/original_hash は書かない（純予約）。
pub fn build_section_graph(
    schema: &str,
    doc_key: &str,
    s: &ManualSectionInput,
    content_hash_hex: &str,
) -> GraphBuild {
    let sec_id = manual_node_id(schema, KIND_SECTION, &s.slug);
    let mut nodes = vec![GraphNode {
        id: sec_id.clone(),
        node_type: KIND_SECTION.to_string(),
        attributes: vec![
            ("section_key".to_string(), s.slug.clone()),
            ("doc_key".to_string(), doc_key.to_string()),
            ("title".to_string(), s.title.clone()),
            ("body".to_string(), s.body.clone()),
            ("source_url".to_string(), s.source_url.clone()),
            ("breadcrumb".to_string(), s.breadcrumb.clone()),
            ("section_no".to_string(), s.section_no.clone().unwrap_or_default()),
            ("order".to_string(), s.order.to_string()),
            ("source_lang".to_string(), "ja".to_string()),
            ("content_hash".to_string(), content_hash_hex.to_string()),
        ],
    }];
    let mut edges = vec![GraphEdge {
        from_id: manual_node_id(schema, KIND_DOC, doc_key),
        to_id: sec_id.clone(),
        edge_type: "HAS_SECTION".to_string(),
        attributes: Vec::new(),
    }];
    if let Some(parent) = &s.parent_slug {
        edges.push(GraphEdge {
            from_id: manual_node_id(schema, KIND_SECTION, parent),
            to_id: sec_id.clone(),
            edge_type: "PARENT_OF".to_string(),
            attributes: Vec::new(),
        });
    }
    for model in &s.product_models {
        edges.push(GraphEdge {
            from_id: sec_id.clone(),
            to_id: manual_node_id(schema, KIND_PRODUCT, model),
            edge_type: "DESCRIBES".to_string(),
            attributes: Vec::new(),
        });
    }
    for sig in &s.signal_values {
        let sig_id = manual_node_id(schema, "Signal", sig);
        nodes.push(GraphNode {
            id: sig_id.clone(),
            node_type: "Signal".to_string(),
            attributes: vec![("value".to_string(), sig.clone())],
        });
        edges.push(GraphEdge {
            from_id: sec_id.clone(),
            to_id: sig_id,
            edge_type: "MENTIONS_SIGNAL".to_string(),
            attributes: Vec::new(),
        });
    }
    GraphBuild { nodes, edges }
}
```

- [ ] **Step 7: ingest_model テスト通過確認**

Run: `cd server && CARGO test --lib manual::ingest_model 2>&1 | tail -3`
Expected: `test result: ok. 3 passed`

- [ ] **Step 8: Commit**

```bash
git add server/src/lib.rs server/src/manual/
git commit -m "feat: add manual schema id helpers and graph builders (pure)"
```

---

### Task 4: manual/retrieval.rs（ManualStore: search / get_section / resolve_product）

**Files:**
- Modify: `server/src/manual/retrieval.rs`, `server/src/model.rs`
- Test: `server/src/manual/retrieval.rs` 内 `#[cfg(test)]`（純粋な scoring/絞り込み部分のみ。gRPC は Task 8 実機）

**Interfaces:**
- Consumes: `crate::vegapunk::VegapunkClient`, `crate::mcp::{semantic 用}`, `crate::resolve::normalize_key`, `crate::harness::signal::SignalSet`, `crate::proto::graphrag::GetGraphSnapshotResponse`
- Produces: 総覧の `ManualStore` / `ManualHit` / `ManualSectionView` / `ManualProductCandidate`、および純関数 `score_section(question, title, body) -> f32`, `sections_for_signals(snapshot, signals) -> HashSet<section_node_id>`

- [ ] **Step 1: model.rs に返却型を追加**

`server/src/model.rs` に（既存 SectionHit 等は残す）:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManualHit {
    pub section_key: String,
    pub title: String,
    pub body: String,
    pub source_url: String,
    pub breadcrumb: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManualSectionView {
    pub section: serde_json::Value,
    pub ancestors: Vec<serde_json::Value>,
    pub children: Vec<serde_json::Value>,
    pub based_on_rationale: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManualProductCandidate {
    pub model: String,
    pub name: String,
    pub score: f32,
    pub reason: String,
}
```

- [ ] **Step 2: 失敗テスト（純関数 score_section / sections_for_signals）**

`server/src/manual/retrieval.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::signal::Signal;

    #[test]
    fn score_exact_substring_is_one() {
        // 質問がタイトル/本文の部分文字列 → 1.0（トラブルシュート系: タイトル=症状名）
        let s = score_section("SDカードが認識されない", "SDカードが認識されない", "抜き差ししてください");
        assert_eq!(s, 1.0);
    }

    #[test]
    fn score_unrelated_is_low() {
        let s = score_section("送料はいくら", "SDカードが認識されない", "抜き差し");
        assert!(s < 0.6);
    }

    #[test]
    fn sections_for_signals_follows_mentions_signal_reverse() {
        // Signal ノード + MENTIONS_SIGNAL 辺 から section を逆引き
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphNode as PN, GraphEdge as PE};
        let snap = GetGraphSnapshotResponse {
            nodes: vec![
                PN { node_id: "urtect:gen1:Signal:sd_not_recognized".into(), node_type: "Signal".into(), display_text: String::new(), attributes: [("value".to_string(), "sd_not_recognized".to_string())].into_iter().collect() },
            ],
            edges: vec![
                PE { from_id: "urtect:gen1:ManualSection:sec-sd".into(), to_id: "urtect:gen1:Signal:sd_not_recognized".into(), edge_type: "MENTIONS_SIGNAL".into(), attributes: Default::default() },
            ],
            truncated: false,
        };
        let want: SignalSet = [Signal::new("sd_not_recognized")].into_iter().collect();
        let got = sections_for_signals(&snap, &want);
        assert!(got.contains("urtect:gen1:ManualSection:sec-sd"));
    }
}
```

（注: `GetGraphSnapshotResponse` / `GraphNode` / `GraphEdge` の proto フィールド名・型は `server/src/proto.rs` 経由の生成型に合わせる。上のテストで型が合わなければ生成型を Read して修正する — フィールド名は proto 定義が正。）

- [ ] **Step 3: テスト失敗確認**

Run: `cd server && CARGO test --lib manual::retrieval 2>&1 | tail -5`
Expected: FAIL

- [ ] **Step 4: 実装**

`score_section` は既存 `mcp::section_score` と同じ規則を再利用する（DRY）。`mcp::section_score(query_norm, query_raw, text)` は `pub(crate)`。ここでは title+body を text にして呼ぶ薄いラッパにする。

```rust
use crate::harness::signal::SignalSet;
use crate::manual::schema_ids::manual_node_id;
use crate::model::{GraphBuild, GraphEdge, GraphNode, ManualHit, ManualProductCandidate, ManualSectionView};
use crate::proto::graphrag::GetGraphSnapshotResponse;
use crate::resolve::normalize_key;
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

const SNAPSHOT_MAX_NODES: i32 = 5000;

/// title+body への直接性（mcp::section_score と同一規則。DRY）。
pub fn score_section(question: &str, title: &str, body: &str) -> f32 {
    let text = format!("{title}\n{body}");
    crate::mcp::section_score(&normalize_key(question), question, &text)
}

/// snapshot の MENTIONS_SIGNAL 辺から、質問 signal に結線された ManualSection node_id 集合を返す。
pub fn sections_for_signals(
    snapshot: &GetGraphSnapshotResponse,
    signals: &SignalSet,
) -> HashSet<String> {
    // Signal.value → node_id
    let wanted: HashSet<String> = snapshot
        .nodes
        .iter()
        .filter(|n| n.node_type == "Signal")
        .filter_map(|n| n.attributes.get("value").map(|v| (v.clone(), n.node_id.clone())))
        .filter(|(v, _)| signals.iter().any(|s| s.as_str() == v))
        .map(|(_, id)| id)
        .collect();
    snapshot
        .edges
        .iter()
        .filter(|e| e.edge_type == "MENTIONS_SIGNAL" && wanted.contains(&e.to_id))
        .map(|e| e.from_id.clone())
        .collect()
}

pub struct ManualStore {
    client: Arc<VegapunkClient>,
}

impl ManualStore {
    pub fn new(client: Arc<VegapunkClient>) -> Self {
        Self { client }
    }

    async fn snapshot(&self, schema: &str) -> Result<GetGraphSnapshotResponse> {
        let snap = self.client.graph_snapshot(schema, SNAPSHOT_MAX_NODES).await?;
        if snap.nodes.len() >= SNAPSHOT_MAX_NODES as usize {
            anyhow::bail!("manual snapshot reached node limit; refusing on incomplete data");
        }
        Ok(snap)
    }

    /// signal 絞り込み(A) と body 全文(B) の max スコアで ManualSection を返す。
    pub async fn search(
        &self,
        schema: &str,
        question: &str,
        signals: &SignalSet,
        top_k: usize,
    ) -> Result<Vec<ManualHit>> {
        let snap = self.snapshot(schema).await?;
        self.search_with_snapshot(schema, question, signals, top_k, &snap)
    }

    pub fn search_with_snapshot(
        &self,
        _schema: &str,
        question: &str,
        signals: &SignalSet,
        top_k: usize,
        snapshot: &GetGraphSnapshotResponse,
    ) -> Result<Vec<ManualHit>> {
        let signal_narrowed = sections_for_signals(snapshot, signals);
        let mut hits: Vec<ManualHit> = snapshot
            .nodes
            .iter()
            .filter(|n| n.node_type == "ManualSection")
            .map(|n| {
                let a = n.attributes.clone();
                let title = a.get("title").cloned().unwrap_or_default();
                let body = a.get("body").cloned().unwrap_or_default();
                // (B) body 全文スコア。(A) signal で絞られた候補は同じ score だがヒット保証で残す。
                let base = score_section(question, &title, &body);
                // signal 絞り込みに入っていれば最低 0.6 を下限にせず、base をそのまま使う（過剰応答を防ぐ）。
                // A/B の max は「A の候補集合に入るか」で候補を残し、score は section_score を使う。
                let in_signal = signal_narrowed.contains(&n.node_id);
                let score = base; // A・B とも直接性は section_score で測る（max は候補集合の和）
                (in_signal, score, ManualHit {
                    section_key: a.get("section_key").cloned().unwrap_or_default(),
                    title,
                    body,
                    source_url: a.get("source_url").cloned().unwrap_or_default(),
                    breadcrumb: a.get("breadcrumb").cloned().unwrap_or_default(),
                    score,
                })
            })
            // 候補: signal 絞り込みに入る or body スコアが立つ（0 超）ものを残す
            .filter(|(in_signal, score, _)| *in_signal || *score > 0.0)
            .map(|(_, _, h)| h)
            .collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k.max(1));
        Ok(hits)
    }

    /// ManualSection + 祖先(PARENT_OF)・子・BASED_ON 経由の Rationale を返す。
    pub async fn get_section(&self, schema: &str, section_key: &str) -> Result<ManualSectionView> {
        let snap = self.snapshot(schema).await?;
        let node_id = manual_node_id(schema, "ManualSection", section_key);
        let node_json = |id: &str| -> Option<serde_json::Value> {
            snap.nodes.iter().find(|n| n.node_id == id).map(|n| {
                serde_json::json!({"node_id": n.node_id, "node_type": n.node_type, "attributes": n.attributes})
            })
        };
        let section = node_json(&node_id).ok_or_else(|| anyhow!("manual section not found: {section_key}"))?;
        // 祖先: PARENT_OF の to=node_id を辿って from を親に
        let mut ancestors = Vec::new();
        let mut cur = node_id.clone();
        for _ in 0..8 {
            let Some(parent) = snap.edges.iter().find(|e| e.edge_type == "PARENT_OF" && e.to_id == cur) else { break };
            if let Some(j) = node_json(&parent.from_id) { ancestors.push(j); }
            cur = parent.from_id.clone();
        }
        // 子: PARENT_OF の from=node_id
        let children: Vec<_> = snap.edges.iter()
            .filter(|e| e.edge_type == "PARENT_OF" && e.from_id == node_id)
            .filter_map(|e| node_json(&e.to_id)).collect();
        // BASED_ON でこの section を根拠にする KR → その Rationale（参考情報）
        let based_on_rationale = Vec::new(); // Step 1 では section 視点の逆引きは省略（KR 側で辿れる）
        Ok(ManualSectionView { section, ancestors, children, based_on_rationale })
    }

    /// Product ノードを name/model/aliases の正規化一致で解決する。
    pub async fn resolve_product(&self, schema: &str, text: &str) -> Result<Vec<ManualProductCandidate>> {
        let products = self.client.query_nodes(schema, "Product", Vec::new(), 1000).await
            .context("query Product")?;
        let q = normalize_key(text);
        let mut cands: Vec<ManualProductCandidate> = products.into_iter().filter_map(|p| {
            let a = p.attributes;
            let model = a.get("model").cloned().unwrap_or_default();
            let name = a.get("name").cloned().unwrap_or_default();
            let aliases = a.get("aliases").cloned().unwrap_or_default();
            let score = [model.as_str(), name.as_str()]
                .into_iter()
                .chain(aliases.split(',').map(str::trim))
                .map(|c| crate::resolve::fuzzy_score(&q, &normalize_key(c)))
                .fold(0.0_f32, f32::max);
            (score > 0.1).then_some(ManualProductCandidate {
                model, name, score,
                reason: if score >= 1.0 { "normalized_match".into() } else { "semantic_nearby".into() },
            })
        }).collect();
        cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        cands.truncate(5);
        Ok(cands)
    }
}
```

（注: `query_nodes` の戻り `NodeResult.attributes` は `HashMap<String,String>`。`fuzzy_score` は `crate::resolve` の既存関数。proto の snapshot 型フィールド名が異なれば Read して合わせる。）

- [ ] **Step 5: テスト通過確認**

Run: `cd server && CARGO test --lib manual::retrieval 2>&1 | tail -3`
Expected: `test result: ok. 3 passed`

- [ ] **Step 6: Commit**

```bash
git add server/src/model.rs server/src/manual/retrieval.rs
git commit -m "feat: add manual-domain retrieval (signal-narrowed + body-text, get_section, resolve_product)"
```

---

### Task 5: knowledge.rs — Rationale + BASED_ON 分離、tool 契約変更

**Files:**
- Modify: `server/src/harness/knowledge.rs`
- Test: `server/src/harness/knowledge.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: 既存 `GraphBuild` 生成、`crate::manual::schema_ids`
- Produces: `NewKnownResolution { rationale_text: Option<String>, manual_section_keys: Vec<String> }`、`build_known_resolution_graph` が Rationale ノード + BECAUSE + BASED_ON を張る

- [ ] **Step 1: 失敗テスト**

`knowledge.rs` の既存 KR ビルドテストを置き換え/追加:

```rust
#[test]
fn kr_graph_splits_rationale_and_manual_basis() {
    use crate::harness::signal::Signal;
    let new_kr = NewKnownResolution {
        signal_set: [Signal::new("sd_not_recognized")].into_iter().collect(),
        applicability: "全モデル".to_string(),
        answer: "推奨は東芝製です。".to_string(),
        origin: "escalation:esc-1".to_string(),
        created_by: "sup-001".to_string(),
        rationale_text: Some("メーカー動作確認リストに基づく".to_string()),
        manual_section_keys: vec!["sec-sd-not-recognized".to_string()],
    };
    let build = build_known_resolution_graph("urtect", "kr-1", &new_kr);
    // Rationale ノード + BECAUSE 辺
    assert_eq!(build.nodes.iter().filter(|n| n.node_type == "Rationale").count(), 1);
    assert_eq!(build.edges.iter().filter(|e| e.edge_type == "BECAUSE").count(), 1);
    // BASED_ON → ManualSection 辺
    assert_eq!(build.edges.iter().filter(|e| e.edge_type == "BASED_ON").count(), 1);
    let based = build.edges.iter().find(|e| e.edge_type == "BASED_ON").unwrap();
    assert!(based.to_id.ends_with("ManualSection:sec-sd-not-recognized"));
    // HAS_SIGNAL は従来どおり
    assert_eq!(build.edges.iter().filter(|e| e.edge_type == "HAS_SIGNAL").count(), 1);
}

#[test]
fn kr_without_rationale_text_has_no_rationale_node() {
    use crate::harness::signal::Signal;
    let new_kr = NewKnownResolution {
        signal_set: [Signal::new("sd_not_recognized")].into_iter().collect(),
        applicability: "x".to_string(), answer: "y".to_string(), origin: "manual".to_string(),
        created_by: "sup".to_string(), rationale_text: None, manual_section_keys: vec![],
    };
    let build = build_known_resolution_graph("urtect", "kr-2", &new_kr);
    assert_eq!(build.nodes.iter().filter(|n| n.node_type == "Rationale").count(), 0);
    assert_eq!(build.edges.iter().filter(|e| e.edge_type == "BASED_ON").count(), 0);
}
```

- [ ] **Step 2: テスト失敗確認**

Run: `cd server && CARGO test --lib harness::knowledge 2>&1 | tail -5`
Expected: FAIL（`rationale_text` フィールド未定義）

- [ ] **Step 3: 実装**

`NewKnownResolution` の `rationale_section_keys` を削除し `rationale_text: Option<String>` と `manual_section_keys: Vec<String>` を追加。`build_known_resolution_graph` の BECAUSE 生成部を差し替え:

```rust
    // 判断理由 → Rationale ノード + BECAUSE
    if let Some(text) = &kr.rationale_text {
        let rationale_id = harness_node_id(schema, "Rationale", &format!("{kr_id}-r"));
        nodes.push(GraphNode {
            id: rationale_id.clone(),
            node_type: "Rationale".to_string(),
            attributes: vec![
                ("rationale_id".to_string(), format!("{kr_id}-r")),
                ("text".to_string(), text.clone()),
            ],
        });
        edges.push(GraphEdge {
            from_id: kr_node_id.clone(),
            to_id: rationale_id,
            edge_type: "BECAUSE".to_string(),
            attributes: Vec::new(),
        });
    }
    // マニュアル出典 → BASED_ON → ManualSection
    for section_key in &kr.manual_section_keys {
        edges.push(GraphEdge {
            from_id: kr_node_id.clone(),
            to_id: crate::manual::schema_ids::manual_node_id(schema, "ManualSection", section_key),
            edge_type: "BASED_ON".to_string(),
            attributes: Vec::new(),
        });
    }
```

旧 `rationale_section_keys` → `section_node_id` → `BECAUSE` のループを削除する。

- [ ] **Step 4: テスト通過確認**

Run: `cd server && CARGO test --lib harness::knowledge 2>&1 | tail -3`
Expected: `test result: ok.`（全 knowledge テスト pass）

- [ ] **Step 5: Commit**

```bash
git add server/src/harness/knowledge.rs
git commit -m "feat: split KR rationale (BECAUSE->Rationale) from manual basis (BASED_ON->ManualSection)"
```

---

### Task 6: harness/mod.rs + decision.rs — evaluate の manual 分岐・default route config・admission 契約

**Files:**
- Modify: `server/src/harness/mod.rs`, `server/src/harness/decision.rs`
- Test: `server/src/harness/decision.rs` 内既存テストの route 期待値更新 + `mod.rs` 内

**Interfaces:**
- Consumes: `ManualStore`, `config.default_escalation_route`, `ProjectConfig.manual_schema`
- Produces: `Harness` が manual_schema に応じて manual retrieval を使う。`decide` の layer3 route を入力から受ける。`admit_known_resolution` は変更なし（signals/answer 検証）だが返却に rationale/manual を通す経路は rmcp 側で組む。

- [ ] **Step 1: decision.rs の layer3 route をハードコードから入力へ**

`DecisionInput` に `pub default_route: &'a str,` を追加。`decide` の layer3 escalate 2 箇所 `route_to: "triage".to_string()` を `route_to: input.default_route.to_string()` に置換。既存テストの `input(...)` ヘルパに `default_route: "triage"` を追加し、`assert_eq!(route_to, "triage")` はそのまま通す。

- [ ] **Step 2: decision テスト更新・通過確認**

Run: `cd server && CARGO test --lib harness::decision 2>&1 | tail -3`
Expected: `test result: ok.`

- [ ] **Step 3: Harness に ManualStore と manual_schema を持たせる**

`Harness` 構造体に:

```rust
    pub manual: Option<crate::manual::retrieval::ManualStore>,
    pub default_route: String,
```

`Harness::build` で `manual: Some(ManualStore::new(client.clone()))`, `default_route: config.harness.default_escalation_route.clone()`。`build` は `client: Arc<VegapunkClient>` を既に受けている（ToolService と共有）。ManualStore にも同じ Arc を渡す。

`RequestContext` に `manual_schema` を載せる（begin で project から解決）:

```rust
    pub manual_schema: crate::config::ManualSchemaKind,
```

`begin` に `project_manual_schema: ManualSchemaKind` 引数を足し、ctx に格納。呼び出し側（rmcp_server）が project.manual_schema を渡す。

- [ ] **Step 4: evaluate の manual 取得を分岐**

`evaluate` 内の manual hits 取得を manual_schema で分岐する。`ManualV1` のとき ManualStore を使い、直接性は signal 絞り込み + body の max（ManualStore.search が既に候補集合の和を返す）。`LegacySection` は従来の `tools.search_manual_with_snapshot`。

decision 入力へは共通の `best_manual_score` / `best_manual_sections` に落とす:

```rust
        let (best_score, best_sections, retrieved_manual_ids): (Option<f32>, Vec<String>, Vec<String>) =
            match ctx.manual_schema {
                crate::config::ManualSchemaKind::ManualV1 => {
                    let store = self.manual.as_ref().ok_or_else(|| anyhow!("manual store not configured"))?;
                    let hits = store.search_with_snapshot(&ctx.schema, question, &accumulated, 5, &snapshot)?;
                    let best = hits.first().map(|h| h.score);
                    let keys = hits.iter().map(|h| h.section_key.clone()).collect::<Vec<_>>();
                    let ids = hits.iter().map(|h| crate::manual::schema_ids::manual_node_id(&ctx.schema, "ManualSection", &h.section_key)).collect();
                    (best, keys, ids)
                }
                crate::config::ManualSchemaKind::LegacySection => {
                    let hits = tools.search_manual_with_snapshot(&ctx.schema, question, product_key, 5, snapshot.clone()).await?;
                    let best = hits.first().map(|h| h.score);
                    let keys = hits.iter().map(|h| h.section_key.clone()).collect::<Vec<_>>();
                    let ids = hits.iter().map(|h| crate::ingest::section_node_id(&ctx.schema, &h.section_key)).collect();
                    (best, keys, ids)
                }
            };
```

`decide` 呼び出しに `default_route: &self.default_route`, `best_manual_score: best_score`, `best_manual_sections: &best_sections`。WORM の `retrieved_node_ids` に `retrieved_manual_ids` を使う。`EvaluationOutcome.hits` は `Vec<SectionHit>` のままだと manual と型が合わないため、`hits` を汎用化するか、manual 経路では `SectionHit` に詰め替える薄い変換を入れる（`SectionHit { section_key, title_ja: title, body_ja: Some(body), body_en: None, translation_status: None, breadcrumb: vec![breadcrumb], score }`）。この詰め替えを `mod.rs` 内のローカル関数 `manual_hit_to_section_hit` にする。

- [ ] **Step 5: ビルド確認（rmcp_server 未更新なので lib のみ確認）**

Run: `cd server && CARGO test --lib harness:: 2>&1 | tail -3`
Expected: `test result: ok.`（既存 harness テストが通る。begin の引数追加でテストヘルパ `harness_for_test` の begin 呼び出しに `ManualSchemaKind::LegacySection` を渡す修正が要る）

- [ ] **Step 6: Commit**

```bash
git add server/src/harness/mod.rs server/src/harness/decision.rs
git commit -m "feat: route evaluate through manual retrieval when manual_schema=manual_v1; config-driven default escalation route"
```

---

### Task 7: rmcp_server.rs — tool を manual_schema 分岐・add_known_resolution 契約変更

**Files:**
- Modify: `server/src/rmcp_server.rs`, `server/src/main.rs`（begin 呼び出しに project.manual_schema）

**Interfaces:**
- Consumes: `ManualStore`（harness.manual）, `RequestContext.manual_schema`
- Produces: search_manual / get_section / resolve_product / add_known_resolution が manual_schema=ManualV1 で ManualStore/新契約を通る。

- [ ] **Step 1: begin で manual_schema を解決**

`CsSupportRmcpServer` は `schema` を持つが manual_schema も要る。`new(schema, tools, harness, manual_schema)` に拡張し、`begin` が `harness.begin(auth, &self.schema, self.manual_schema)` を呼ぶ。`main.rs` の生成箇所（stdio / HTTP ループ）で `project.manual_schema` を渡す。

- [ ] **Step 2: search_manual / get_section / resolve_product を分岐**

各 read tool で `match ctx.manual_schema`。`ManualV1` は `harness.manual` の対応メソッド、`LegacySection` は既存 `self.tools` メソッド。返却型は tool ごとに 1 つなので、manual 経路の `ManualHit`/`ManualSectionView`/`ManualProductCandidate` をレスポンス型に載せる（tool の Response 構造体に `manual` バリアントを足すのでなく、共通の JSON にする）。最小変更として、レスポンスを `serde_json::Value` で返す薄い共通化にせず、tool ごとに manual/legacy それぞれの Response struct を用意し、`Json<serde_json::Value>` で返す。監査（audit_with_nodes）は両経路で必ず呼ぶ。

- [ ] **Step 3: add_known_resolution の入力契約変更**

`AddKnownResolutionRequest` の `rationale_section_keys` を削除し:

```rust
    /// 担当者の判断理由（任意）。BECAUSE → Rationale で残す
    pub rationale_text: Option<String>,
    /// マニュアル出典 section（BASED_ON → ManualSection で結線）
    pub manual_section_keys: Vec<String>,
```

handler で `NewKnownResolution { ..., rationale_text: req.rationale_text, manual_section_keys: req.manual_section_keys }` に組み替え。admission（`harness.admit_known_resolution`）はそのまま。

- [ ] **Step 4: ビルド + 全テスト**

Run: `cd server && CARGO fmt && CARGO test 2>&1 | tail -3 && CARGO check --bins 2>&1 | tail -1`
Expected: `test result: ok.` / `Finished`

- [ ] **Step 5: Commit**

```bash
git add server/src/rmcp_server.rs server/src/main.rs
git commit -m "feat: branch tools by manual_schema; change add_known_resolution to rationale_text + manual_section_keys"
```

---

### Task 8: URTECT 語彙・NG・ルール fixture（レビュー対象データ）

**Files:**
- Create: `server/data/urtect/signal-lexicon.json`, `server/data/urtect/ng-dictionary.json`, `server/data/urtect/rules.json`
- Create: `server/config.urtect.toml`（urtect 用ローカル config: manual_schema=manual_v1, default_escalation_route=support_desk, lexicon/ng パスを data/urtect/*）

**Interfaces:**
- Produces: URTECT テナントデータ。spec §4/§5 準拠。**業務レビュー待ちのドラフト**。

- [ ] **Step 1: `server/data/urtect/signal-lexicon.json`（§4 class + surface_forms）**

危険系は厚く（spec §9-1）。全 signal に class（hazard/context）。抜粋を完全に書く:

```json
{
  "signals": [
    { "signal": "camera_offline", "class": "context", "surface_forms": ["オフライン", "接続が切れ", "映らなくなった", "つながらなくなった"] },
    { "signal": "wifi_connection_failure", "class": "context", "surface_forms": ["wifiにつながらない", "ワイファイ", "無線が繋がらない", "ネットワークに接続できない"] },
    { "signal": "camera_not_detected", "class": "context", "surface_forms": ["カメラが見つからない", "カメラが検出されない", "デバイスが表示されない"] },
    { "signal": "sd_not_recognized", "class": "context", "surface_forms": ["sdカードが認識されない", "エスディーカード", "sdが読み込まれない", "カードが認識しない"] },
    { "signal": "sd_format_request", "class": "context", "surface_forms": ["フォーマット", "初期化して", "sdを初期化"] },
    { "signal": "playback_failure", "class": "context", "surface_forms": ["再生できない", "録画が見れない", "映像が再生されない"] },
    { "signal": "app_malfunction", "class": "context", "surface_forms": ["アプリが落ちる", "アプリが動かない", "強制終了", "フリーズ"] },
    { "signal": "notification_not_received", "class": "context", "surface_forms": ["通知が来ない", "お知らせが届かない", "プッシュ通知が来ない"] },
    { "signal": "login_failure", "class": "context", "surface_forms": ["ログインできない", "サインインできない", "入れない"] },
    { "signal": "password_reset_request", "class": "context", "surface_forms": ["パスワードを忘れた", "パスワードをリセット", "パスワード再設定"] },
    { "signal": "network_environment_changed", "class": "context", "surface_forms": ["ルーターを変えた", "回線を変更", "引っ越し", "wifiを変えた"] },
    { "signal": "recording_rule_config", "class": "context", "surface_forms": ["録画ルール", "録画設定", "録画の設定"] },
    { "signal": "continuous_recording", "class": "context", "surface_forms": ["常時録画", "連続録画", "ずっと録画"] },
    { "signal": "clip_saving", "class": "context", "surface_forms": ["クリップ", "録画を保存", "動画を保存"] },
    { "signal": "dashboard_edit", "class": "context", "surface_forms": ["ホーム画面", "ダッシュボード", "画面のカスタマイズ"] },
    { "signal": "user_creation", "class": "context", "surface_forms": ["ユーザーを追加", "ユーザー作成", "アカウントを増やす"] },
    { "signal": "user_edit", "class": "context", "surface_forms": ["ユーザー編集", "ユーザー情報の変更"] },
    { "signal": "notification_config", "class": "context", "surface_forms": ["通知設定", "通知をオフ", "お知らせの設定"] },
    { "signal": "two_factor_setup", "class": "context", "surface_forms": ["二要素認証", "2段階認証", "二段階認証", "2ファクタ"] },
    { "signal": "camera_registration", "class": "context", "surface_forms": ["カメラを登録", "カメラの追加", "初期登録"] },
    { "signal": "calibration", "class": "context", "surface_forms": ["キャリブレーション", "調整", "位置合わせ"] },
    { "signal": "camera_flip_setting", "class": "context", "surface_forms": ["上下反転", "映像を反転", "天井取り付け"] },
    { "signal": "camera_installation", "class": "context", "surface_forms": ["設置", "取り付け", "設置方法", "取り付け方"] },
    { "signal": "video_quality_setting", "class": "context", "surface_forms": ["画質", "解像度", "画質設定"] },
    { "signal": "login_user_addition", "class": "context", "surface_forms": ["ログインユーザー追加", "共有ユーザー"] },
    { "signal": "account_management", "class": "context", "surface_forms": ["アカウント管理", "アカウント設定"] },
    { "signal": "app_install", "class": "context", "surface_forms": ["アプリのインストール", "アプリを入れる", "ダウンロード"] },
    { "signal": "model_adc_v724", "class": "context", "surface_forms": ["adc-v724", "adcv724", "v724"] },
    { "signal": "model_adc_v724x", "class": "context", "surface_forms": ["adc-v724x", "v724x"] },
    { "signal": "model_adc_vc727p", "class": "context", "surface_forms": ["adc-vc727p", "vc727p", "727p"] },
    { "signal": "channel_web_only", "class": "context", "surface_forms": ["web", "ブラウザ", "パソコンから"] },
    { "signal": "channel_app_ios", "class": "context", "surface_forms": ["iphone", "ios", "アイフォン"] },
    { "signal": "channel_app_android", "class": "context", "surface_forms": ["android", "アンドロイド"] },
    { "signal": "contract_billing_question", "class": "context", "surface_forms": ["解約", "契約", "料金", "プラン変更", "月額", "支払い"] },
    { "signal": "warranty_hardware_failure", "class": "context", "surface_forms": ["故障", "壊れた", "破損", "交換してほしい", "保証", "動かなくなった"] },
    { "signal": "physical_construction_risk", "class": "hazard", "surface_forms": ["高所", "電気工事", "配線工事", "自分で設置していい", "取り付け工事"] },
    { "signal": "security_incident", "class": "hazard", "surface_forms": ["不正アクセス", "乗っ取り", "身に覚えのない", "知らないログイン", "勝手に", "ハッキング", "パスワードが変わって"] },
    { "signal": "security_guarantee", "class": "hazard", "surface_forms": ["安全ですか", "防犯として十分", "これで安心", "セキュリティは大丈夫"] },
    { "signal": "legal_privacy_question", "class": "hazard", "surface_forms": ["警察に提出", "第三者に提供", "プライバシー", "録画を証拠", "肖像権", "法的に"] },
    { "signal": "physical_damage_smell_heat", "class": "hazard", "surface_forms": ["焦げ臭い", "こげくさい", "変な匂い", "異臭", "熱い", "熱を持つ", "煙", "発煙", "溶けて", "パチパチ音"] }
  ]
}
```

- [ ] **Step 2: `server/data/urtect/ng-dictionary.json`（§5）**

```json
{
  "block_terms": ["絶対に安全", "100%防犯", "必ず検知", "交換します", "返金します", "無償で対応します", "提出して問題ありません"],
  "abstain_terms": ["どんな環境でも録画できます", "見逃しはありません", "完全にブロック", "確実に防げます"]
}
```

- [ ] **Step 3: `server/data/urtect/rules.json`（§5 第1層5・第2層3）**

```json
{
  "escalation_rules": [
    { "rule_id": "security-incident", "condition": ["security_incident"], "owner": "security", "route": "support_desk", "binding": "mandatory" },
    { "rule_id": "physical-damage", "condition": ["physical_damage_smell_heat"], "owner": "hardware", "route": "support_desk", "binding": "mandatory" },
    { "rule_id": "warranty-failure", "condition": ["warranty_hardware_failure"], "owner": "hardware", "route": "support_desk", "binding": "advisory" },
    { "rule_id": "contract-billing", "condition": ["contract_billing_question"], "owner": "contract", "route": "support_desk", "binding": "advisory" },
    { "rule_id": "construction-risk", "condition": ["physical_construction_risk"], "owner": "installation", "route": "support_desk", "binding": "mandatory" }
  ],
  "prohibited_domains": [
    { "domain_id": "legal-privacy", "domain_signals": ["legal_privacy_question"], "pattern": [], "route": "support_desk", "binding": "mandatory" },
    { "domain_id": "security-guarantee", "domain_signals": ["security_guarantee"], "pattern": [], "route": "support_desk", "binding": "mandatory" },
    { "domain_id": "electrical-work", "domain_signals": ["physical_construction_risk"], "pattern": ["電気工事", "高所作業"], "route": "support_desk", "binding": "mandatory" }
  ]
}
```

（振り先は §9-3 のとおり確認取得までは全て `support_desk` 仮置き。§10-A で実部署に差し替える。）

- [ ] **Step 4: `server/config.urtect.toml`**

`config.local-https.toml` をベースに、`[[projects]]` に `manual_schema = "manual_v1"`、`[harness]` に `default_escalation_route = "support_desk"`、lexicon/ng パスを `data/urtect/*`、schema/project_id を `urtect` にする。

- [ ] **Step 5: JSON 構文確認**

Run: `cd server && for f in data/urtect/*.json; do jq . "$f" >/dev/null && echo "$f ok"; done`
Expected: 3 ファイル ok

- [ ] **Step 6: Commit**

```bash
git add server/data/urtect/ server/config.urtect.toml
git commit -m "feat: add URTECT signal lexicon, NG dictionary, rules, and urtect config (draft, pending business review)"
```

---

### Task 9: ingest_urtect（Google Sites 自動列挙・fetch・抽出・upsert）

**Files:**
- Modify: `server/src/bin/ingest_urtect.rs`
- Test: 抽出・slug・content_hash の純関数は Task 3 で済。本 bin は実機 fetch のため統合検証（Task 10）。ただし HTML 抽出関数だけ小テストを付ける。

**Interfaces:**
- Consumes: `manual::ingest_model`, `manual::schema_ids`, `VegapunkClient`, ingest 済み signal 語彙（MENTIONS_SIGNAL 機械照合用に data/urtect/signal-lexicon.json を読む）
- Produces: `urtect` schema を作成 + ManualDocument/ManualSection/Product/edges を upsert + URL 網羅率レポート

- [ ] **Step 1: HTML 抽出の純関数 + テスト**

`ingest_urtect.rs` に `fn extract_main_text(html: &str) -> (String /*title*/, String /*body*/)`（scraper で `<main>` or 本文コンテナからテキスト、nav/header/footer 除去、定型文除去）と `fn normalize_body(s: &str) -> String`（空白正規化）を実装し、小テスト:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_boilerplate_and_normalizes() {
        let html = r#"<html><head><title>SDカードが認識されない</title></head>
          <body><nav>目次</nav><main><h1>SDカードが認識されない</h1><p>抜き差し。</p>
          <footer>マニュアルの内容や画面は予告なく変更になる場合があります</footer></main></body></html>"#;
        let (title, body) = extract_main_text(html);
        assert_eq!(title, "SDカードが認識されない");
        assert!(body.contains("抜き差し"));
        assert!(!body.contains("予告なく変更"));
        assert!(!body.contains("目次"));
    }
}
```

- [ ] **Step 2: テスト失敗→実装→通過**

Run: `cd server && CARGO test --bin ingest_urtect 2>&1 | tail -3`
Expected: 実装後 `test result: ok.`

- [ ] **Step 3: 本体（clap args, nav 列挙, fetch, グラフ upsert, 差分, レポート）**

clap で `--endpoint` `--schema urtect` `--schema-file ../schema/cs-support.yml` `--lexicon-file data/urtect/signal-lexicon.json` `--top-url https://sites.google.com/view/urtect-manual/top` `--token-env`/`--token-file`。処理:
1. `create_or_update_schema(urtect, cs-support.yml)`
2. top ページ fetch → nav から全ページ URL 列挙（`scraper` で nav リンク抽出、同一ホスト・manual 配下のみ）
3. 既存 ManualSection を `query_nodes(urtect, "ManualSection")` で読み `content_hash` マップ化
4. 各 URL: fetch → extract → `normalize_body` → `content_hash`。既存と一致ならスキップ（差分 ingest）。slug=`section_slug(url)`, order=列挙順, breadcrumb=nav パス, parent_slug=nav 親, product_models=body の型番出現, signal_values=lexicon surface_forms 照合（`normalize_key` 部分一致）
5. `build_document_node` 1 回 + `build_product_node`（3 型番）+ 各 `build_section_graph` を集約し `upsert_graph_low_level`
6. レポート出力: 列挙 URL 数 / ingest ManualSection 数 / スキップ数（未変更）/ 生成エッジ数 / 型番別 DESCRIBES 数 / MENTIONS_SIGNAL 付与ゼロの section 一覧（要語彙補強）

エラーは握りつぶさず `anyhow` で伝搬。fetch 失敗 URL はレポートに列挙して継続。

- [ ] **Step 4: ビルド確認**

Run: `cd server && CARGO check --bins 2>&1 | tail -1`
Expected: `Finished`

- [ ] **Step 5: Commit**

```bash
git add server/src/bin/ingest_urtect.rs
git commit -m "feat: add ingest_urtect (Google Sites crawl, extract, ManualSection graph upsert, coverage report)"
```

---

### Task 10: 実機投入 + §6 検証（A/B/C/D 群・Done 条件）

**Files:**
- Modify: `docs/superpowers/plans/2026-07-08-manual-domain-template-urtect.md`（検証結果を追記）
- Modify: `README.md`（urtect 起動・ingest 手順追記）

- [ ] **Step 1: fmt / check / test フル**

Run: `cd server && CARGO fmt --check && CARGO test 2>&1 | tail -3 && CARGO check --bins 2>&1 | tail -1`
Expected: fmt 差分なし / test 全 pass / Finished。失敗は `superpowers:systematic-debugging` で修正。

- [ ] **Step 2: URTECT ingest（実機 vegapunk.local）**

```bash
cd server && CARGO run --bin ingest_urtect -- --endpoint http://vegapunk.local:6840 --schema urtect --schema-file ../schema/cs-support.yml --lexicon-file data/urtect/signal-lexicon.json --token-file /private/tmp/vegapunk-bearer-token
```
Expected: レポートに ManualSection 数（nav 列挙数とほぼ一致）・エッジ数・MENTIONS_SIGNAL ゼロ section の一覧。ネットワーク不通なら理由を明記。ゼロ section が多ければ Task 8 の surface_forms を補強して再 ingest（差分）。

- [ ] **Step 3: ルール投入（既存 ingest_rules を urtect data/schema で流用）**

`ingest_rules` は `--schema-file` / `--rules-file` を取るので:

```bash
cd server && CARGO run --bin ingest_rules -- --endpoint http://vegapunk.local:6840 --schema urtect --schema-file ../schema/cs-support.yml --rules-file data/urtect/rules.json --token-file /private/tmp/vegapunk-bearer-token
```
Expected: `upserted_nodes` = 8（第1層5 + 第2層3）

- [ ] **Step 4: MCP 起動（urtect config）**

CLAUDE.md ローカル起動手順を urtect config で。JWT は E2E 用に secret 設定（Step 1 実装の E2E と同手順）。healthz `ok`。tools/list で 12 tool 確認。

- [ ] **Step 5: §6 A〜D 群 + Done 条件 1〜5 の突合**

`evaluate_answerability` / `record_answer_attempt` / `add_known_resolution`（rationale_text + manual_section_keys 新契約）/ `record_answer_outcome` を使い:
- A 群: source_url 付き回答（Allowed source=manual、hits に source_url）。**過剰エスカレーションが多ければ Task 8 surface_forms / ManualSection 分割 / 直接性しきい値（[harness.thresholds].low）を第3層のみ調整**（第1・2層・NG は触らない）
- B 群: 第1・2層 100% escalate
- C 群: 第3層 escalate・**創作回答ゼロ**（ingest で未記載を確認済みの質問）
- D 群: マルチターン累積再判定で physical_damage_smell_heat が立ち第1層へ切替
- Done 4: C 群 1 問に `add_known_resolution`（manual_section_keys 付き）→ 再質問で BASED_ON/BECAUSE 付き回答
- Done 5: その KR に条件追加質問 → 再エスカレーション（包含方向のみ）
- Done 3: WORM に provenance キー + retrieved_node_ids から ManualSection 辿れる（grpcurl で確認）

結果を本計画末尾に記録。**Done 条件 1「記載範囲を超えない」は人手でドラフト目視**（サーバは強制しない）。

- [ ] **Step 6: README + 計画に結果追記、Commit**

```bash
git add docs/superpowers/plans/2026-07-08-manual-domain-template-urtect.md README.md
git commit -m "docs: record URTECT ingest and acceptance results"
```

- [ ] **Step 7: レビューと PR**

CLAUDE.md コミット前チェックリスト: `simplify` → `code-review` フロー → PR 作成（`superpowers:requesting-code-review`）。完了宣言前に `superpowers:verification-before-completion`。

---

## Self-Review

**Spec coverage（urtect-pilot-design.md → タスク対応）:**
- §2 スキーマ（Product/ManualDocument/ManualSection + 5 辺 + traceable_pair）= Task 1。翻訳予約 = Task 1（純予約・Task 3 で書かない）
- §2.3 2 経路（signal graph + body） = Task 4（max 候補集合）+ Task 6（evaluate 配線）
- §2.4 テンプレv1（形固定・確定はしない・legacy 凍結）= Task 1（cs-support.yml 汎用形）+ Task 2（manual_schema 分岐で legacy 温存）+ Global Constraints（確定しない）
- §3 ingest = Task 9（Rust・自動列挙・差分・レポート）+ Task 3（グラフ組み立て純関数）
- §4 語彙（class + surface_forms）= Task 8
- §5 第1/2層・NG（照合手段固定・support_desk）= Task 8 + Task 6（default route config）+ Task 2
- §6 A〜D・Done 1〜5 = Task 10
- §7-0 (a)〜(e) = Task 4（a,b,d）/ Task 6（a,b,c,e）/ Task 5（BASED_ON 分離）
- §7-1 は cs-support 側スキーマ = Task 1（vegapunk repo 触らない）
- §9 server/data 差し替え = Task 8。grade 昇格させない = 検証で auto 昇格に達しないため自然に満たす（config はデフォルト値のまま、Task 10 で N=3 に到達しない）

**既知の限界（意図的）:**
- テンプレ形の「確定」はしない（revisable）。config 駆動の node type 差し替えはしない（YAGNI）
- ベクトル検索は本計画で配線しない（§2.3 のベクトル経路は将来。Step 1 は signal graph + body text の 2 経路で A 群を狙う。早期検証 Task 10 Step 5 で不足なら surface_forms/分割/しきい値で調整、それでも足りなければ別途ベクトル配線を次フェーズ課題化）
- get_section の BASED_ON 逆引き（section→KR）は省略（KR 側から辿れる）
- sivira-cs-demo legacy は manual_schema=LegacySection で従来コードパス維持（壊さない）

**Type consistency:** `NewKnownResolution` は Task 5 で `rationale_text`/`manual_section_keys` に統一（Task 7 handler も同名）。`ManualSchemaKind` は Task 2 定義を Task 6/7 で参照。`ManualHit`/`ManualSectionView`/`ManualProductCandidate` は Task 4 model.rs 定義を Task 6/7 で参照。`score_section` は Task 4 で定義し `mcp::section_score`（pub(crate)）を再利用。

**Placeholder scan:** 各コード step に実コードあり。fixture は完全な JSON。実機 fetch 依存（Task 9 本体・Task 10）は統合検証として明示、純関数は単体テスト化。

---

## 検証結果（2026-07-11 実機・vegapunk backend）

Task 10 + scorer 改良（Task 11: bigram カバレッジ / Task 12: run 単位 IDF + 漢字限定 bigram 救済）後の最終結果。

- ingest: 69 ページ / 252 nodes / 313 edges / fetch 失敗 0 / nav 階層検出
- A群（回答されるべき）: **6/6 allowed**、全て score 1.0、source_url 付き
- B群（第1・2層）: **5/5 escalate**（「警察に出したい」は語彙 gap により第3層経由 — 安全側）
- C群（未記載）: NAS 0.27 / 他社カメラ 0.15 で escalate。「SD推奨メーカー」はマニュアル実記載
  （製品ページ「推奨するSDカード: WD Purple」）のため allowed が正解 = §6 の未記載確認で C 群から除外
- D群（マルチターン）: Wi-Fi→焦げ臭い で累積再判定 → 第1層 escalate
- Done3: WORM に provenance（gov=[kr_id] / retrieved_node_ids=ManualSection）
- Done4: 学習ループ一周（escalate → add_known_resolution → 再質問で KR 回答。
  BASED_ON→ManualSection / BECAUSE→Rationale / HAS_SIGNAL 全て結線）
- Done5: 「iPhoneで…浴室に設置…」→ KR blocked（leftover channel_app_ios）+ manual 0.47 < 0.6
  → escalate reason=unknown_added_signal（包含方向のみ再利用の実証）
- 分離マージン: answerable=1.0 vs 記載なし 0.15〜0.47（threshold 0.6）

実機で発見し修正した統合バグ: schema name 差し替え / read tool の出力スキーマ panic（Json<Value> 不可）/
gRPC 4MB decode 上限 / 30s timeout / Google Sites JS 混入 / bigram 断片の希釈・カタカナ誤救済。

チューニング残（fixture 加算のみ・業務レビュー枠）: legal_privacy_question の surface_forms 追補
（「警察に出」等）/ MENTIONS_SIGNAL ゼロの 10 節への語彙追補 / ADC-V724X の DESCRIBES 0 件 /
lexicon 更新時は content_hash 不変のため差分 ingest では MENTIONS_SIGNAL が張り直されない
（--force 再 ingest か hash への語彙版数混入が次フェーズ課題）/ evaluate 毎の全 snapshot 取得の最適化。
