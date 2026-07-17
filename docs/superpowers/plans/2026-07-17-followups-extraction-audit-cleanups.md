# Follow-ups: 抽出一貫性 / audit 型統一 / embed 並行化 / micro-cleanups 実装計画

> **For agentic workers:** 実装は kaneko エージェント、レビューは reviewer エージェントで 1 タスクずつ回す（kaneko → reviewer → Fable 受理）。

**Goal:** PR #3 マージ時に記録した follow-up 群を解消する。判定の決定論・WORM 互換・I1〜I5 は不変。

**Branch:** `feat/extraction-consistency-cleanups`（main = 89061f8 から）

## Global Constraints

- Python / TypeScript 禁止。新規 crate 依存の追加禁止（tokio / futures 既存範囲で解決。futures crate が依存に無ければ JoinSet を使う）。
- 判断ロジックを tool handler に直書きしない。decide() に触れない。
- **WORM 行の serialize 結果を変えない**: `extraction_mode` の文字列は従来どおり `"not_applicable"` / `ExtractionMode::as_str()` の既存値（実装の as_str が正。勝手に改名しない）。`AuditDraft.extraction_mode: String` フィールド自体は維持。
- テスト: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo <cmd>`（以降 CARGO。RUSTC_WRAPPER unset 必須）。コミット前に `CARGO fmt`（フォーマッタ実行）→ `CARGO fmt --check` クリーン → `CARGO test --lib` 全 pass → `CARGO check --bins` Finished。
- コミットは Conventional Commits。push はユーザが行う（エージェントは push しない）。
- sivira legacy 経路の挙動を変えない（legacy search_manual は signal 抽出を使っていないので影響なし、を維持）。

---

## Task 1: read tool の抽出統一 + audit extraction_mode の型統一

**背景**: `search_manual`(ManualV1, rmcp_server.rs:367) と `search_known_resolutions`(:589) が `harness.normalizer.normalize()`（lexicon 単独）直呼びのまま。evaluate は LLM ハイブリッド抽出のため、「evaluate では適用される KR が search_known_resolutions のプレビューでは当たらない」不整合がある。また audit 側は `audit_with_nodes` → `audit_with_nodes_and_extraction_mode` の並列 2 メソッド + `AUDIT_EXTRACTION_MODE_NOT_APPLICABLE` const という歪んだ形（stringly-typed）。

**変更**（対象: `server/src/harness/mod.rs`, `server/src/harness/extraction.rs`(必要なら), `server/src/rmcp_server.rs`）:

1. `harness/extraction.rs` の `ExtractionMode` に `Clone, Copy, PartialEq, Eq, Debug` を derive（不足分のみ）。`as_str()` の既存文字列は**絶対に変えない**。
2. `Harness::audit_with_nodes` のシグネチャを `(..., retrieved_node_ids: Vec<String>, extraction_mode: Option<ExtractionMode>)` に変更し、内部で `extraction_mode.map(|m| m.as_str().to_string()).unwrap_or_else(|| "not_applicable".to_string())` を `AuditDraft.extraction_mode` に入れる。`audit_with_nodes_and_extraction_mode` メソッドと `AUDIT_EXTRACTION_MODE_NOT_APPLICABLE` const を削除。`audit()`（薄いラッパ）は `None` を渡す。
3. 全呼び出し箇所を grep して更新（rmcp_server.rs に 6 箇所前後 + mod.rs evaluate）。抽出を行わない tool（resolve_product / get_section / get_product / search_past_cases / legacy search_manual 等）は `None`、evaluate は `Some(outcome の mode)`。
4. `search_manual` ManualV1 branch: `let extraction = self.harness.extractor.extract(&req.query_ja).await;` に置換し `extraction.signals` を検索に、`Some(extraction.mode)` を audit に渡す。
5. `search_known_resolutions`: 同様に `extractor.extract(&req.question).await`。この handler の audit 呼び出し（2 箇所ある場合は両方）に `Some(mode)`。
6. レスポンス schema は変更しない（YAGNI。mode の可視化は WORM で足りる）。

**完了条件**:
- `grep -rn "normalizer.normalize" server/src/rmcp_server.rs` が 0 件。
- `grep -rn "audit_with_nodes_and_extraction_mode\|AUDIT_EXTRACTION_MODE_NOT_APPLICABLE" server/src` が 0 件。
- 既存テスト全 pass（audit 系テストが文字列 "not_applicable" 等を assert している場合、WORM 文字列は不変なので期待値変更は不要のはず。変更が必要になったら設計違反なので差し戻し）。
- extraction.rs に「Option<ExtractionMode> → 監査文字列」の変換のテストを 1 本追加（None → "not_applicable"、Some(Hybrid) → 既存 as_str 値）。

**検証**: `CARGO test --lib` / `CARGO check --bins` / fmt クリーン。

**コミット**: `refactor: unify read-tool signal extraction via extractor; type audit extraction_mode as Option<ExtractionMode>`

---

## Task 2: ingest embed の bounded concurrency

**背景**: `server/src/bin/ingest_urtect.rs` の embed が逐次（sections :694, products :715）。~72 件 × RTT で ingest 時間が線形に伸びる。

**変更**:
1. 共通ヘルパ `async fn embed_all(client: &VegapunkClient, items: Vec<(String /*label*/, String /*text*/)>, concurrency: usize) -> Result<Vec<Vec<f32>>>` を ingest_urtect.rs 内に追加。`tokio::task::JoinSet` + `VegapunkClient: Clone` で並行実行、**同時実行数は 4 に制限**（backend 負荷配慮。セマフォまたはチャンク投入）。結果は入力順を保って返す（index 付き spawn → 位置格納）。
2. **fail-closed 維持**: 1 件でも失敗したら残りを中断し、失敗した label を含む context 付きで bail（部分的なベクトル状態を作らない、従来のセマンティクス）。
3. sections / products の両ループをこのヘルパ経由に置換。`--no-vectors` とレポート項目（upserted_vectors / vectors_skipped）は不変。
4. 純関数部分（順序保証・fail-closed）はネットワーク不要の形にできないため、単体テストは不要。既存 bin テスト（extract 系 3 本）が green のままであること。

**完了条件**: 逐次 `for` + `.await` の embed が消えている。挙動（fail-closed・レポート・順序）は同一。

**検証**: `CARGO test --bins` / `CARGO check --bins` / fmt クリーン。

**コミット**: `perf: bounded-concurrency embeds at ingest (JoinSet, limit 4, fail-closed preserved)`

---

## Task 3: micro-cleanups（5 件）

**変更**:

1. **system prompt の構築を 1 回に**（`server/src/llm.rs`, `server/src/harness/extraction.rs`）: `AnthropicClient::classify_signals(&self, question, system_prompt: &str)` に変更（内部の `build_system_prompt` 呼び出しを除去）。`build_system_prompt` は `pub(crate)` として残し、`AnthropicSignalClassifier::new` が構築時に 1 回だけ呼んでフィールド `system_prompt: String` に保持（既存の `vocabulary_prompt` フィールドを置換）。injection 対策文言のテスト（build_system_prompt の内容 assert）は維持。
2. **top_k 定数化**（`server/src/harness/mod.rs`）: `:567`（vector_hits の 5）と `:574`（search_with_snapshot の 5）を `const EVALUATE_TOP_K: usize = 5;` に統一（この 2 箇所のみ。tool handler の `unwrap_or(5)` はリクエスト既定値なので対象外）。
3. **score_source の是正**（`server/src/manual/retrieval.rs:573-579`）: `(false, false) => "text"` を `(false, false) => "signal"` に変更（signal 絞り込みのみで候補に残った節の正直なラベル）。doc コメント更新 + このケースのテストを 1 本追加（既存テストの期待値に影響があれば確認の上更新）。
4. **DESCRIBES 分割ヘルパ**（`server/src/manual/retrieval.rs`）: `product_view_from_snapshot`(:293-299) と `search_with_snapshot`(:519-530) の重複する集合計算を `fn describes_sets(edges: &[proto GraphEdge], product_node_id: &str) -> (HashSet<&str> /*any*/, HashSet<&str> /*target*/)` に抽出し両所から使用。挙動不変（既存テストが差分検知）。
5. **secret 読み込み共通化**（`server/src/config.rs` に `pub(crate) fn read_secret_file(path: &str) -> anyhow::Result<String>`（read → trim → 空なら bail）を追加）: `llm.rs::resolve_api_key` の file 分岐と `harness/mod.rs` の jwt_secret_file 読み込みから使用。各呼び出し側は `.with_context()` で従来相当のエラー文脈（"jwt secret file …" / "llm api_key_file …"）を保つ。**既存のエラーメッセージを assert しているテストがあれば、文言互換を保つ方向で実装**（テスト期待値の緩和はしない）。

**完了条件**: 5 件すべて反映、既存テスト全 pass + 追加テスト（score_source signal ケース、read_secret_file の空拒否）pass。

**検証**: `CARGO test --lib` / `CARGO test --bins` / `CARGO check --bins` / fmt クリーン。

**コミット**: `refactor: cache system prompt, dedupe describes-partition and secret reads, honest signal score_source, top_k const`

---

## Task 4: 仕上げ（Fable 主導）

1. 全テスト・fmt・check の最終確認。
2. `.superpowers/sdd/progress.md` 更新。
3. PR 作成（push はユーザ）→ Copilot レビューを指摘ゼロまでループ。

## Out of scope（本計画でやらない）

- A6 ベクトル経路の有効化（vegapunk backend 回答待ち）
- signal 語彙 / NG 辞書 / grading しきい値の確定（業務レビュー待ち）
- UpsertGraph（atomic）への統合、`futures` crate 導入、LLM 抽出の read tool 以外への拡張
