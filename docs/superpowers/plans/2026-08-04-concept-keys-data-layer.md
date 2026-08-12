# B2-0/B2-1: concept_keys データ層 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ManualSection.concept_keys`（MENTIONS_CONCEPT 辺の読み取り最適化射影）を新規 ingest と backfill の両方で書けるようにし、本番 `urtect` の Concept 分布を実測する。

**Architecture:** spec は `docs/superpowers/specs/2026-08-02-concept-expansion-design.md`（正本。本計画と食い違ったら spec が正）。書き込みは (a) `ingest_alarmcom` の新規/変更記事経路、(b) 新 bin `backfill_concept_keys` の既存データ経路の 2 本。読み取り経路（2-hop 拡張）は本計画のスコープ外（B2-2 で別計画。B2-0 の実測値 → `CONCEPT_FAN_IN_MAX_RATIO` / `CONCEPT_SEED_LIMIT` の決定が前提のため）。

**Tech Stack:** Rust（`clap`, `serde_json`, `tokio`, `tracing`）、既存 `VegapunkClient`（`query_nodes_paged` / `traverse_neighbors_paged` / `upsert_nodes` / `create_or_update_schema`）。

## Global Constraints

- Python / TypeScript を使わない。Python ファイルを作らない
- cargo は必ず `--manifest-path server/Cargo.toml`。`cd` / `git -C` 禁止
- コマンド内でシェル展開（`$?` / `$(...)` / 変数参照）を使わない。パイプ失敗検知が要るときだけ先頭に `set -o pipefail;`
- エラーを握りつぶさない。skip は必ず理由付き warn、閾値超過は fail closed
- 認証情報ハードコード禁止。bearer token はファイル / env 経由
- Conventional Commits。ブランチ `feat/8-ingest-alarmcom`。push しない（commit まで）
- 既存の構成・命名・型に合わせ最小差分。新 bin は既存 `ingest_*` CLI の流儀（`clap::Parser`、bin 内複製の `read_token`）を踏襲
- 実 vegapunk への接続を要するテストは書かない（純関数の単体テストのみ。実機検証は Cloud Run job のログを evidence にする）

---

### Task 1: `concept_keys` 属性の書き込み口（schema + ingest_model）

**Files:**
- Modify: `schema/cs-support.yml`（`ManualSection` の `original_hash` 行の直後）
- Modify: `server/src/manual/ingest_model.rs`（`ManualSectionInput` / `build_section_graph`）
- Modify: コンパイルエラーが出る全 `ManualSectionInput` 構築サイト（`ingest_urtect.rs` 等。値は `Vec::new()`）
- Test: `server/src/manual/ingest_model.rs` の `#[cfg(test)]` モジュール（無ければ新設）

**Interfaces:**
- Produces: `ManualSectionInput.concept_keys: Vec<String>`（空なら属性を書かない）。属性 `concept_keys` は JSON 配列文字列（例 `["firstpersonin","geofence"]`）

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn build_section_graph_writes_concept_keys_json_when_non_empty() {
    let s = ManualSectionInput {
        slug: "sec-a".into(),
        title: "t".into(),
        body: "b".into(),
        source_url: "https://example.com".into(),
        breadcrumb: "A > B".into(),
        section_no: None,
        order: 1,
        parent_slug: None,
        product_models: Vec::new(),
        signal_values: Vec::new(),
        body_original: None,
        original_hash: None,
        concept_keys: vec!["firstpersonin".into(), "geofence".into()],
    };
    let build = build_section_graph("urtect", "doc-x", &s, "hash");
    let attr = build.nodes[0]
        .attributes
        .iter()
        .find(|(k, _)| k == "concept_keys")
        .map(|(_, v)| v.clone())
        .expect("concept_keys attribute must be written");
    let parsed: Vec<String> = serde_json::from_str(&attr).unwrap();
    assert_eq!(parsed, vec!["firstpersonin".to_string(), "geofence".to_string()]);
}

#[test]
fn build_section_graph_omits_concept_keys_when_empty() {
    let s = ManualSectionInput { /* 上と同じで concept_keys: Vec::new() */ };
    let build = build_section_graph("urtect", "doc-x", &s, "hash");
    assert!(build.nodes[0].attributes.iter().all(|(k, _)| k != "concept_keys"));
}
```

- [ ] **Step 2: 失敗を確認** — `cargo test --manifest-path server/Cargo.toml build_section_graph_writes_concept` → コンパイルエラー（フィールド未定義）

- [ ] **Step 3: 実装**

`ManualSectionInput` に doc コメント付きでフィールド追加（`body_original` の流儀に合わせる）:

```rust
    /// この section が言及する concept_key の一覧（MENTIONS_CONCEPT 辺の読み取り最適化射影。
    /// 辺がグラフの正で、この属性は射影。食い違ったら辺が正）。空なら属性自体を書かない。
    pub concept_keys: Vec<String>,
```

`build_section_graph` の予約属性ブロックの直後に:

```rust
    if !s.concept_keys.is_empty() {
        attributes.push((
            "concept_keys".to_string(),
            serde_json::to_string(&s.concept_keys).unwrap_or_else(|_| "[]".to_string()),
        ));
    }
```

`schema/cs-support.yml` の `original_hash` 行の直後（インデント 6 スペース）:

```yaml
      # Issue #8 B2: MENTIONS_CONCEPT 辺の読み取り最適化射影。JSON 配列文字列（型は string 固定のため）。
      concept_keys: { type: string }
```

コンパイルエラーが出た構築サイト（`ingest_urtect.rs` 等）はすべて `concept_keys: Vec::new(),` を足す。

- [ ] **Step 4: 全テスト pass 確認** — `cargo test --manifest-path server/Cargo.toml` / `cargo fmt --manifest-path server/Cargo.toml -- --check`

- [ ] **Step 5: Commit** — `feat(ingest): add concept_keys projection attribute to ManualSection (#8 Phase B2-1)`

---

### Task 2: `ingest_alarmcom` の配線

**Files:**
- Modify: `server/src/bin/ingest_alarmcom.rs`（`ManualSectionInput` 構築リテラル。`input` 直前で作っている per-article の `concept_keys` Vec をそのまま渡す）

**Interfaces:**
- Consumes: Task 1 の `ManualSectionInput.concept_keys`
- Produces: 新規/変更記事の ManualSection ノードに `concept_keys` 属性が乗る

- [ ] **Step 1: 配線** — `ingest_alarmcom.rs` の `let input = ManualSectionInput { ... }` 構築（`body_original: Some(body_en)` を含むリテラル）に 1 行足す。直前のループで組んだ重複排除済み `concept_keys: Vec<String>` は後段（`build_mentions_concept_edge` ループ）でも使うので **clone を渡す**:

```rust
            concept_keys: concept_keys.clone(),
```

Task 1 で仮に `Vec::new()` を入れていた場合はそれを置き換える。**辺を張るループ（`build_mentions_concept_edge`）は変更しない**（辺が正、属性は射影）。

- [ ] **Step 2: 検証** — `cargo test --manifest-path server/Cargo.toml --bin ingest_alarmcom`（既存 22 件が壊れないこと）と `cargo check --manifest-path server/Cargo.toml --all-targets`

- [ ] **Step 3: Commit** — `feat(ingest): write concept_keys from alarmcom article extraction (#8 Phase B2-1)`

---

### Task 3: `backfill_concept_keys` bin の骨格 + `--probe-only`（B2-0 の成果物）

**Files:**
- Create: `server/src/bin/backfill_concept_keys.rs`
- Modify: `server/Cargo.toml`（`[[bin]]` 追記が必要な場合のみ。既存 bin が自動検出なら不要）

**Interfaces:**
- Consumes: `VegapunkClient::query_nodes_paged(schema, "ManualSection", vec![], 1000)`、`VegapunkClient::traverse_neighbors_paged(schema, "Concept", "MENTIONS_CONCEPT", "outgoing", &node_id, 1000)`、`VegapunkClient::create_or_update_schema`
- Produces: `--probe-only` の JSON（spec の固定形）。純関数 `probe_summary(rows: &[SectionConcepts], truncated: bool) -> serde_json::Value` と型 `struct SectionConcepts { section_key: String, node_id: String, concept_keys: Vec<String> /* ソート済み */, attr_concept_keys: Option<String> }`

- [ ] **Step 1: 失敗するテストを書く**（probe JSON の形。spec の固定形そのまま）

```rust
#[test]
fn probe_summary_reports_fan_in_with_declared_denominator() {
    let rows = vec![
        row("s1", &["a", "b"]),   // helper: SectionConcepts を作る（attr は None）
        row("s2", &["a"]),
        row("s3", &[]),           // concept 無し section
    ];
    let v = probe_summary(&rows, false);
    assert_eq!(v["total_sections"], 3);
    assert_eq!(v["sections_with_concepts"], 2);
    assert_eq!(v["total_concepts"], 2);
    assert_eq!(v["ratio_denominator"], "sections_with_concepts");
    assert_eq!(v["truncated"], false);
    // concepts は section_count 降順 → concept_key 昇順。ratio の分母は sections_with_concepts
    assert_eq!(v["concepts"][0]["concept_key"], "a");
    assert_eq!(v["concepts"][0]["section_count"], 2);
    assert!((v["concepts"][0]["ratio"].as_f64().unwrap() - 1.0).abs() < 1e-9);
    assert_eq!(v["concepts"][1]["concept_key"], "b");
    assert!((v["concepts"][1]["ratio"].as_f64().unwrap() - 0.5).abs() < 1e-9);
}

#[test]
fn probe_summary_handles_zero_sections_without_division() {
    let v = probe_summary(&[], false);
    assert_eq!(v["total_sections"], 0);
    assert_eq!(v["sections_with_concepts"], 0);
    assert_eq!(v["concepts"].as_array().unwrap().len(), 0);
}
```

`name_ja` は Concept ノードから引けるときだけ埋める（traverse の `NodeResult.attributes` から取得して `SectionConcepts` とは別の `HashMap<String, String>` で渡す実装でもよい。テストは `concept_key` / `section_count` / `ratio` を主に固定する）。

- [ ] **Step 2: 失敗を確認** — `cargo test --manifest-path server/Cargo.toml --bin backfill_concept_keys` → コンパイルエラー

- [ ] **Step 3: bin を実装**

構成（既存 `ingest_urtect.rs` / `merge_schema.rs` の流儀に合わせる）:

```rust
#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "VEGAPUNK_ENDPOINT", default_value = "http://vegapunk.local:6840")]
    endpoint: String,
    #[arg(long, default_value = "urtect")]
    schema: String,
    #[arg(long, default_value = "../schema/cs-support.yml")]
    schema_file: String,
    #[arg(long)]
    token_file: Option<String>,
    #[arg(long, default_value = "VEGAPUNK_BEARER_TOKEN")]
    token_env: String,
    /// 書き込まず、Concept 分布（fan-in）を JSON で出す（B2-0 の実測）
    #[arg(long)]
    probe_only: bool,
    /// 書き込まず、MENTIONS_CONCEPT 辺と concept_keys 属性の乖離を出す
    #[arg(long)]
    verify: bool,
    /// 指定 1 件に concept_keys だけの最小ノードを upsert して UpsertNodes の意味論を実測する
    #[arg(long)]
    probe_one: Option<String>,
    /// この section_key より後（辞書順）から再開する
    #[arg(long)]
    start_after: Option<String>,
}
```

処理の流れ（main）:
1. `read_token`（`ingest_alarmcom.rs` の同名関数を複製。関数コメントに複製元を明記）
2. `create_or_update_schema(schema, fs::read_to_string(schema_file))`
3. `query_nodes_paged(schema, "ManualSection", vec![], 1000)` → 取得件数を必ずログ・summary に出す
4. `start_after` があれば `section_key` 辞書順でフィルタ
5. 各 section: `traverse_neighbors_paged(schema, "Concept", "MENTIONS_CONCEPT", "outgoing", &node_id, 1000)` → `concept_key` 属性を集めて**ソート**し `SectionConcepts` を作る（100 件ごとに進捗ログ）
6. モード分岐: `--probe-only` → `probe_summary` を stdout に JSON 出力して終了 / `--verify` → Task 6 / `--probe-one` → Task 5 / 既定 → Task 4 の書き込み

`--probe-only` / `--verify` / `--probe-one` は相互排他（同時指定は clap の `conflicts_with` で拒否）。失敗時は非 0 終了（fail closed）。

- [ ] **Step 4: pass 確認** — `cargo test --manifest-path server/Cargo.toml --bin backfill_concept_keys` / `cargo fmt -- --check` / `cargo check --all-targets`

- [ ] **Step 5: Commit** — `feat(backfill): add backfill_concept_keys probe mode (#8 Phase B2-0)`

---

### Task 4: 書き込み本体（既定モード）

**Files:**
- Modify: `server/src/bin/backfill_concept_keys.rs`

**Interfaces:**
- Consumes: Task 3 の `SectionConcepts`、`VegapunkClient::upsert_nodes(Vec<GraphNode>) -> Result<i32>`
- Produces: 純関数 `missing_required_attrs(attrs: &HashMap<String, String>) -> Vec<&'static str>`、`needs_update(attr_concept_keys: Option<&str>, computed_sorted: &[String]) -> bool`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn missing_required_attrs_lists_each_absent_key() {
    let mut attrs = full_attrs(); // helper: 8 required 属性が全部入った HashMap
    attrs.remove("body");
    attrs.remove("order");
    assert_eq!(missing_required_attrs(&attrs), vec!["body", "order"]);
    assert!(missing_required_attrs(&full_attrs()).is_empty());
}

#[test]
fn needs_update_compares_as_set_not_string() {
    // ingest は出現順、backfill はソート順で書くため、順序差は「同じ」と扱う
    assert!(!needs_update(Some(r#"["b","a"]"#), &["a".into(), "b".into()]));
    assert!(needs_update(Some(r#"["a"]"#), &["a".into(), "b".into()]));
    assert!(needs_update(None, &["a".into()]));
    assert!(!needs_update(None, &[]));            // 両方空は書かない
    assert!(needs_update(Some("not json"), &[])); // 不正 JSON は書き直して修復する
}
```

required 属性 = `section_key` / `title` / `body` / `source_url` / `breadcrumb` / `order` / `source_lang` / `content_hash`（spec の安全規定）。

- [ ] **Step 2: 失敗を確認** — 同上

- [ ] **Step 3: 実装**

- `needs_update` が true の section だけを対象に、**読み出した全属性 + `concept_keys`（ソート済み JSON）** で `GraphNode` を作り直し、50 件ごとにバッチして `upsert_nodes`
- upsert 前に `missing_required_attrs` を通し、欠けていれば `tracing::warn!(section_key, missing, "required attrs missing; skipping to avoid destructive resend")` で skip。**skip 率が対象件数の 10% を超えたら fail closed**（`anyhow::bail!` に skip 件数・総数・代表 3 件の section_key を含める。`site_skip_bail` と同じ規律）
- summary（stdout JSON）: `{"total_sections", "updated", "skipped_missing_attrs", "skipped_up_to_date", "batches", "truncated": false}`

- [ ] **Step 4: pass 確認**（fmt / check / test）

- [ ] **Step 5: Commit** — `feat(backfill): write concept_keys projection with fail-closed attr guard (#8 Phase B2-1)`

---

### Task 5: `--probe-one`（UpsertNodes 意味論の実測）

**Files:**
- Modify: `server/src/bin/backfill_concept_keys.rs`

**Interfaces:**
- Produces: 純関数 `probe_one_verdict(before: &HashMap<String, String>, after: &HashMap<String, String>) -> &'static str`（`"merge"` = 他属性が残った / `"replace"` = 消えた / `"inconclusive"` = before に required が無い等で判定不能）

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn probe_one_verdict_detects_merge_and_replace() {
    let before = full_attrs();
    let mut after_merge = full_attrs();
    after_merge.insert("concept_keys".into(), "[\"a\"]".into());
    assert_eq!(probe_one_verdict(&before, &after_merge), "merge");
    let mut after_replace = HashMap::new();
    after_replace.insert("concept_keys".into(), "[\"a\"]".into());
    assert_eq!(probe_one_verdict(&before, &after_replace), "replace");
    assert_eq!(probe_one_verdict(&HashMap::new(), &HashMap::new()), "inconclusive");
}
```

- [ ] **Step 2: 失敗を確認** → **Step 3: 実装**

1. 対象 section の現属性を読み、退避（メモリ）
2. `concept_keys` **だけ**を持つ最小 `GraphNode` を 1 件 upsert
3. 読み戻して `probe_one_verdict`
4. **どちらの結果でも、退避した全属性 + `concept_keys` で即座に再送して復旧する**（復旧の upsert 結果もログ）
5. stdout に `{"section_key", "verdict", "restored": true}` を出す

- [ ] **Step 4: pass 確認** → **Step 5: Commit** — `feat(backfill): add probe-one upsert semantics check (#8 Phase B2-1)`

---

### Task 6: `--verify`（辺と射影の乖離検出）

**Files:**
- Modify: `server/src/bin/backfill_concept_keys.rs`

**Interfaces:**
- Produces: 純関数 `divergences(rows: &[SectionConcepts]) -> Vec<serde_json::Value>`（各要素 `{"section_key", "edges": [...], "attr": [...]}`。**集合比較**で一致すれば出さない）

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn divergences_reports_set_difference_only() {
    let rows = vec![
        row_with_attr("s1", &["a", "b"], Some(r#"["b","a"]"#)), // 順序差 = 一致
        row_with_attr("s2", &["a"], Some(r#"["a","zzz"]"#)),    // 乖離
        row_with_attr("s3", &[], None),                          // 両方空 = 一致
    ];
    let d = divergences(&rows);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["section_key"], "s2");
}
```

- [ ] **Step 2〜4: 失敗確認 → 実装 → pass 確認**（stdout JSON: `{"total_sections", "diverged", "items": [...]}`。乖離 0 でも `"diverged": 0` を明示して出す）

- [ ] **Step 5: Commit** — `feat(backfill): add edge-vs-projection verify mode (#8 Phase B2-1)`

---

### Task 7: 実行手段（Dockerfile + CLAUDE.md）

**Files:**
- Modify: `Dockerfile`（`RUN cargo build ... --bin` の列挙と `COPY --from=builder` の列挙、**2 箇所とも**に `backfill_concept_keys` を追加）
- Modify: `CLAUDE.md`（Cloud Run 節: job 一覧・tag 揃えの対象・ingest 実行順序の注記に `backfill-concept-keys` を追記。「backfill は schema 更新後・読み取り経路有効化前に必ず実行する」を 1 行）

- [ ] **Step 1: Dockerfile 2 箇所へ追記**（既存 bin の列挙形式をそのまま踏襲）
- [ ] **Step 2: CLAUDE.md 追記**
- [ ] **Step 3: 検証** — `set -o pipefail; grep -c backfill_concept_keys Dockerfile` が 2 を返すこと。`cargo check --manifest-path server/Cargo.toml --all-targets`
- [ ] **Step 4: Commit** — `build: add backfill_concept_keys to image and ops docs (#8 Phase B2-0)`

---

## 実装完了後の運用ステップ（コード外。オーケストレーターが gcloud で実施）

1. reviewer レビュー → 最終受理 → push
2. イメージビルド（tag = HEAD short SHA）、service + 全 job（`ingest-rules` / `ingest-urtect` / `merge-schema` / **`backfill-concept-keys`**）を同 tag で update。job 新設は `merge-schema` と同じ VPC connector / SA / Secret injection で `gcloud run jobs create backfill-concept-keys --args="--schema=urtect,--probe-only"`（`--task-timeout 1800`）
3. **B2-0 実測**: `--probe-only` 実行 → fan-in 分布 JSON を取得 → ryugo さんに `CONCEPT_FAN_IN_MAX_RATIO` / `CONCEPT_SEED_LIMIT` の決定を依頼
4. **B2-1 実行**: `--probe-one <section_key>`（意味論実測・即復旧）→ 既定モードで全件 backfill → `--verify` で乖離 0 を確認
5. B2-2（2-hop 拡張 + eval 改修）の実装計画を別途作成

## Self-Review 済み事項

- spec の B2-0 / B2-1 該当節（スキーマ変更・書き込み経路・backfill CLI・実行手段）は Task 1〜7 で全部カバー。読み取り経路・提示・不変条件・受理条件の節は B2-2 スコープで本計画対象外
- `needs_update` / `divergences` を**集合比較**にする理由（ingest は出現順・backfill はソート順で書くため、文字列比較だと永久に「乖離」になる）を明記済み
- 型名・関数名は Task 間で一致（`SectionConcepts` / `probe_summary` / `missing_required_attrs` / `needs_update` / `probe_one_verdict` / `divergences`）
