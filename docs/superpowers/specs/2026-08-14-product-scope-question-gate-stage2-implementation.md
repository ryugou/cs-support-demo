# Issue #28 質問側ゲート 二段目（LLM解釈 + コード判定）実装指示

正本の設計は `docs/superpowers/specs/2026-08-14-product-scope-design.md` §3.1（二段目）。
§2（取扱一覧の取得）、§4（取扱外定型応答の文言）、§5（適用範囲）、§6（テスト要件）も前提とする。

一段目（決定論の型番検出）は実装済み（`server/src/harness/product_gate.rs`、
`server/src/api.rs` の `first_out_of_scope_token` 呼び出し箇所）。今回はそれに続く**二段目**を
追加する: evaluate() 内で毎ターン走っている既存の signal 抽出 LLM 呼び出し
（`extraction::HybridExtractor` の LLM 経路）の出力スキーマに `product_references` を追加し、
evaluate 完了後にコード側で「foreign かつ実在確認済み」の場合だけ取扱外定型応答へ倒す。
**追加の LLM 呼び出しは作らない**（既存呼び出しに同乗させる）。

> **Issue #52 により supersede された（2026-08-26）。** 上記の「既存呼び出しに同乗させる」
> 方針は撤回した。現行は `server/src/harness/extraction.rs` の `ProductReferenceExtractor`
> による専用の LLM 呼び出しを、signals 抽出（`HybridExtractor`）と `tokio::join!` で並列発行する
> 構成に置き換わっている。詳細は本ファイル末尾の「Issue #52 による変更点」を参照。

作業ブランチは `feat/28-product-scope`（既に切り替わっているはずなのでブランチ操作は不要）。

以下は6ファイルへの変更点を、既存コードを実際に読んだ上で具体的に指示する。指示にない非自明な
判断（例: ログの追加要否に迷う、正規化方式を変える等）が必要になったら、実装を進めず差し戻すこと。

## 変更点

### 1. `server/src/llm.rs`

- `SignalResponse`（314行目付近）に `#[serde(default)] product_references: Vec<ProductReferenceRaw>` を追加。
- 新規 `struct ProductReferenceRaw { surface: String, resolution: String, #[serde(default)] matched_model: Option<String> }`（`#[derive(Debug, Deserialize)]`）。
- 新規 `pub struct ClassificationOutput { pub signals: Vec<String>, pub product_references: Vec<crate::harness::product_gate::ProductReference> }`（`Debug, Clone` 程度。フル `pub` にすること。既存の `ExtractionResult` と同じ可視性方針に揃える）。
- `parse_signal_response` の戻り値を `Result<Vec<String>>` から `Result<ClassificationOutput>` に変更。`product_references` のマッピングでは、`resolution` 文字列が `"matched"` / `"ambiguous"` / `"foreign"` のいずれでもない場合、その1件だけを `tracing::debug!` で捨てる（全体のparseは失敗させない。語彙外signalを捨てる既存の踏襲パターンと同じ思想）。`product_gate::ProductReferenceResolution` へのマッピングはこの関数内で行う。
- `classify_signals`（92行目付近）の戻り値型を `Result<ClassificationOutput>` に変更（内部の `parse_signal_response` 呼び出しはそのまま透過）。
- `build_system_prompt`（274行目付近）のシグネチャを `pub(crate) fn build_system_prompt(vocabulary_prompt: &str, catalog: Option<&str>) -> String` に変更する。`catalog` が `None` のときは**現行のプロンプト文言・JSON出力形式を一切変えない**（既存の呼び出し元・挙動を壊さないため）。`catalog` が `Some(catalog)` のときだけ、以下を追加で指示する:
  - 取扱製品一覧として `catalog` の内容を注入する
  - 発話中の製品への言及（型番・略記・俗称・カテゴリ的言及）を `product_references` として抽出させる。各要素の形は `{"surface": string, "resolution": "matched" | "ambiguous" | "foreign", "matched_model": string | null}`
  - resolution判定基準: 一覧のいずれかの製品でありうるなら `matched`（`matched_model` にその型番）、判断が曖昧なら `ambiguous`、**一覧のどれでもあり得ない別製品への言及だと確信できる場合に限り** `foreign`
  - 言及が無ければ `product_references` は空配列
  - 出力JSONは `{"signals": [...], "product_references": [...]}` の形にする
  - 「確信できる場合のみ foreign」という基準は、テストで文言を直接assertするので、この意味が伝わる日本語表現を必ず残すこと（正確な言い回しは自由）。
- テスト更新: 既存の `parses_signal_array_from_model_text` 等（557〜589行目付近）は戻り値が構造体になるため `.signals` を見るように修正。`system_prompt_contains_injection_defense_and_catch_all`（675行目付近）は `build_system_prompt("dummy_signal (hazard): テスト", None)` に変更（catalog無しでも既存アサーションが全て通ることを確認）。
- 新規テスト（追加、置き換えではない）:
  - `product_references` を含むJSONのparseが成功し、各resolutionが正しくマッピングされること（matched/ambiguous/foreignの3件を1つのJSONに含めて検証）
  - `resolution` が未知の文字列の要素は無視され、他の要素・`signals` のparseは成功すること
  - `product_references` フィールド自体が欠落したJSON（後方互換）でも `product_references: []` でparse成功すること（既存のsignalsのみのJSONでも壊れないことの回帰）
  - `build_system_prompt(vocab, Some("ADC-V523、ADC-V724"))` の出力に、カタログ文字列と「確信できる」旨の基準文言、`product_references` というキー名が含まれること
  - `build_system_prompt(vocab, None)` の出力に `product_references` の指示が含まれないこと（catalog無し呼び出し元の挙動を変えないことの回帰）

### 2. `server/src/harness/product_gate.rs`

- 新規ドメイン型（`ProductAllowlist` の下あたりに配置）:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct ProductReference {
      pub surface: String,
      pub resolution: ProductReferenceResolution,
      pub matched_model: Option<String>,
  }

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum ProductReferenceResolution {
      Matched,
      Ambiguous,
      Foreign,
  }
  ```
- 新規関数（コード判定本体、design doc §3.1 二段目のコード判定に対応）:
  ```rust
  pub fn confirmed_foreign_reference<'a>(
      refs: &'a [ProductReference],
      message: &str,
  ) -> Option<&'a ProductReference>
  ```
  条件: `resolution == Foreign` かつ `surface.trim().chars().count() >= 2` かつ「`surface`（trim後）が正規化後の `message` 中に実在する」。実在チェックは NFKC正規化 + 小文字化した上での部分文字列一致（`unicode_normalization::UnicodeNormalization` は既にこのファイルでimport済みなので使い回すこと）。既存の `normalize_model_token`（ハイフン正規化含む型番専用）とは別に、この用途専用の小さい正規化ヘルパー（NFKC + lowercase）を private関数として追加してよい。複数件あれば最初に条件を満たした1件を返す。
- テスト（`extract_model_tokens` 等の既存テスト群と同じ場所に追加）:
  - foreignかつsurfaceがmessageに実在 → `Some`
  - foreignだがsurfaceがmessageに存在しない（幻覚） → `None`
  - ambiguousのみ → `None`
  - matchedのみ → `None`
  - surfaceが1文字（2文字未満） → `None`
  - 複数参照のうち条件を満たす最初の1件が返る
  - 全角/半角・大文字小文字違いでも実在チェックが一致する（例: surface `"v724"` がmessage中の `"Ｖ724"` に一致）

### 3. `server/src/harness/extraction.rs`

- `ExtractionResult`（51行目付近）に `pub product_references: Vec<crate::harness::product_gate::ProductReference>` を追加。
- `AsyncSignalExtractor` trait（60行目付近）: `async fn extract(&self, question: &str, catalog: Option<&str>) -> ExtractionResult;` にシグネチャ変更。
- `ClassifyLlm` trait（67行目付近）: `async fn classify(&self, question: &str, catalog: Option<&str>) -> anyhow::Result<crate::llm::ClassificationOutput>;` にシグネチャ変更。
- `AnthropicSignalClassifier`（74行目付近）: 現在は構築時に一度だけ `build_system_prompt` を呼んで `system_prompt: String` を保持しているが、catalogはリクエストごとに変わりうるため、代わりに `vocabulary_prompt: String`（生の語彙プロンプト）を保持するように変更する。`new(client, vocabulary_prompt)` のシグネチャ・呼び出し元（`harness/mod.rs` 392行目付近）は変更不要（保持する中身が変わるだけ）。`classify()` 内で毎回 `crate::llm::build_system_prompt(&self.vocabulary_prompt, catalog)` を呼んでsystem_promptを組み立ててから `self.client.classify_signals(question, &system_prompt).await` を呼ぶ。
- `HybridExtractor::extract`（119行目付近）: `catalog` パラメータを追加し `llm.classify(question, catalog)` へそのまま渡す。戻り値の `ExtractionResult` の3経路（LexiconOnly / Hybrid / LexiconFallback）それぞれで `product_references` を設定する: LexiconOnly・LexiconFallbackは空Vec、Hybridは `classify()` が返した `ClassificationOutput.product_references` をそのまま使う（signalsのような語彙フィルタは不要。product_referencesの妥当性検証はproduct_gate側の役割）。
- テスト更新: `MockLlm`（183〜206行目）の `classify` シグネチャを変更し、`MockLlm::ok`/`MockLlm::err` は `product_references: Vec::new()` を返す形にする（既存3テストの目的を壊さない最小変更）。既存3テスト（`union_keeps_lexicon_hits_and_validates_vocab` 等）の `ex.extract("カビが生えた")` 呼び出しに `None` を追加。
- 新規テスト:
  - LLMが `product_references` を含む `ClassificationOutput` を返したとき、`ExtractionResult.product_references` にそのまま反映されること（`MockLlm` を拡張し `product_references` を注入できるようにする）
  - LLM呼び出し失敗時（`LexiconFallback`）は `product_references` が空になること
  - `catalog: Some(..)` を渡すと `classify` に渡る catalog がそのまま伝播すること（`MockLlm` にリクエスト内容を記録させて検証、または `classify` 呼び出し引数を直接assertできる形にする）

### 4. `server/src/harness/mod.rs`

> **Issue #52 により supersede された。** 下記コード例の「追加のLLM呼び出しは発生させず、既存の
> signal抽出に同乗させて取得する」は撤回済み。現行の型・doc は `server/src/harness/mod.rs` の
> `EvaluationOutcome::product_references` を正本とする（本ファイル末尾「Issue #52 による
> 変更点」参照）。

- `EvaluationOutcome`（88行目付近）に以下を追加（既存フィールドは一切変更しない）:
  ```rust
  /// 今ターンでLLMが抽出した製品参照（Issue #28 §3.1 二段目）。追加のLLM呼び出しは発生させず、
  /// 既存のsignal抽出に同乗させて取得する。ここでの判定（foreign→取扱外）は行わない
  /// （判定はapi.rs側。MCP経由の呼び出しでは何も強制しない。design doc §3.1）。
  pub product_references: Vec<product_gate::ProductReference>,
  ```
- `evaluate()`内（806行目〜）: 現在 `let allowlist = self.product_allowlist(&ctx.schema).await?;` は1051行目付近（§3.2の材料選別直前）にある。これを、`tokio::join!(knowledge.load_known_resolutions_with(...), self.extractor.extract(question))`（855行目付近）の**直前**に移動する。catalogをsystem promptに注入するには抽出より前にallowlistが要るため。移動後は:
  ```rust
  let allowlist = self.product_allowlist(&ctx.schema).await?;
  let (resolutions, extraction_outcome) = tokio::join!(
      knowledge.load_known_resolutions_with(&ctx.schema, &live_snapshot),
      self.extractor.extract(question, Some(allowlist.display_list())),
  );
  ```
  1051行目付近の元の `let allowlist = self.product_allowlist(&ctx.schema).await?;` は削除し、以降の参照（`filter_out_of_scope_hits(section_hits, &allowlist)` や `draft_customer_reply` への引数など）は上で先に束縛した `allowlist` をそのまま使う（二重取得しない。TTLキャッシュとはいえ既存の「二重取得を避ける」方針に揃える）。
  - この移動により、evaluate()の途中でallowlist取得が失敗した場合、caseの新規作成（900行目以降）より前に `?` でエラー復帰するようになる（従来は case作成後に失敗しうった）。これは意図的な変更（allowlist取得失敗時にオーファンcaseを作らない）であり、そのように実装してよい。既存テストでこの順序に依存するものは無いはず（`mod.rs`内に `evaluate()` をフルパスで叩く単体テストは無い）だが、`cargo test` で確認すること。
  - `signals`/`extraction_mode` の束縛の並びに `let product_references = extraction_outcome.product_references;` を追加。
  - 関数末尾の `Ok(EvaluationOutcome { ... })`（1195行目付近）に `product_references,` を追加。
- `root_cause_probe`内（1367行目付近）の `self.extractor.extract(corrected_answer).await` は今回のスコープ外（§3.1と無関係）。catalogに `None` を渡すだけに留める: `self.extractor.extract(corrected_answer, None).await`。

### 5. `server/src/rmcp_server.rs`

> **Issue #52 により supersede された。** 下記コード例の「追加のLLM呼び出しは発生させない」は
> 撤回済み。現行の doc は `server/src/rmcp_server.rs` の
> `EvaluateAnswerabilityResponse::product_references` を正本とする（本ファイル末尾
> 「Issue #52 による変更点」参照）。

- `EvaluateAnswerabilityResponse`（71行目付近）に以下を追加:
  ```rust
  /// 今ターンでLLMが抽出した製品参照（Issue #28 §3.1 二段目。追加のLLM呼び出しは発生させない）。
  /// MCP側ではこの判定（foreign→取扱外）は行わない。
  pub product_references: Vec<ProductReferenceJson>,
  ```
- 新規ミラー型（`RelatedCaseJson`と同じパターン、127〜144行目付近を参考にする）:
  ```rust
  #[derive(Debug, Serialize, schemars::JsonSchema)]
  pub struct ProductReferenceJson {
      pub surface: String,
      pub resolution: String,
      pub matched_model: Option<String>,
  }

  impl From<crate::harness::product_gate::ProductReference> for ProductReferenceJson {
      fn from(r: crate::harness::product_gate::ProductReference) -> Self {
          Self {
              surface: r.surface,
              resolution: match r.resolution {
                  crate::harness::product_gate::ProductReferenceResolution::Matched => "matched",
                  crate::harness::product_gate::ProductReferenceResolution::Ambiguous => "ambiguous",
                  crate::harness::product_gate::ProductReferenceResolution::Foreign => "foreign",
              }
              .to_string(),
              matched_model: r.matched_model,
          }
      }
  }
  ```
- `evaluate_answerability`（653行目付近のレスポンス構築）に `product_references: outcome.product_references.into_iter().map(ProductReferenceJson::from).collect(),` を追加。
- 431〜434行目・694〜697行目の2箇所の `ExtractionResult { signals: ..., mode: ... } = self.harness.extractor.extract(&req.query_ja).await;` / `.extract(&req.question).await;` を、引数に `None`（catalog無し。design doc §5「search_manual等のMCP生toolの入出力は変更しない」に従う）を追加した上で、フィールドパターンに `..` を足して新フィールドを無視する形に直す。

### 6. `server/src/api.rs`

- `gate_generated_text` / `gate_customer_reply_draft`（619〜677行目付近）と同じ並びに新規関数を追加:
  ```rust
  /// Issue #28 §3.1 二段目（LLM解釈 + コード判定）。evaluate()が返したproduct_referencesの
  /// うち、foreignと判定され、かつ幻覚ガード（surfaceが正規化後のメッセージ中に実在）と
  /// 最小長（2文字以上）を満たす参照が1件でもあれば、§4の取扱外定型応答を返す
  /// （evaluate()の判定・下書きは使わず破棄する。design doc §3.1）。該当が無ければNone。
  fn second_stage_out_of_scope_reply(
      product_references: &[product_gate::ProductReference],
      message: &str,
      allowlist: &product_gate::ProductAllowlist,
  ) -> Option<String> {
      let reference = product_gate::confirmed_foreign_reference(product_references, message)?;
      tracing::info!(
          surface = %reference.surface,
          "question-side gate stage 2 (LLM catalog interpretation) classified the message as \
           an out-of-scope product reference; returning the canned out-of-scope reply and \
           discarding the evaluate() outcome"
      );
      Some(product_gate::build_out_of_scope_reply(&reference.surface, allowlist))
  }
  ```
- `reply_handler`の`Ok(mut outcome) => { ... }`ブロック（866行目付近）の**先頭**（`let mut conv = ...` より前）に挿入:
  ```rust
  Ok(mut outcome) => {
      // Issue #28 §3.1 二段目: evaluate()の結果より前に判定する。
      if let Some(reply_text) =
          second_stage_out_of_scope_reply(&outcome.product_references, &req.message, &allowlist)
      {
          return ok_reply_response(reply_text, outcome.case_id);
      }

      let mut conv = match state.harness.load_conv_state(&ctx, &outcome.case_id).await {
      ...
  ```
  （`allowlist` はこのスコープで既に774行目で取得済みのものをそのまま使う。新たに取得し直さない。）
- テスト用ヘルパー `base_outcome`（1341〜1360行目付近）に `product_references: Vec::new()` を追加。
- 新規テスト（`second_stage_out_of_scope_reply`を直接呼ぶ単体テスト。HTTPルーティング経由のテストは不要 — `test_harness()`は`knowledge: None`で`evaluate()`が常に失敗するため`Ok(outcome)`経路に到達できない。この制約は変更しなくてよい）:
  - foreign参照のsurfaceがmessageに実在 → `Some`が返り、本文に検出surfaceと取扱一覧が含まれる（`response_gate_fixture_allowlist()` など既存のfixtureヘルパーを再利用してよい）
  - foreign参照のsurfaceがmessageに存在しない（幻覚） → `None`
  - ambiguousのみ → `None`
  - matchedのみ → `None`
  - product_referencesが空 → `None`

## テスト要件（design doc §6 + 依頼元指示のまとめ）

- 抽出スキーマのparse: matched/ambiguous/foreign/欠落フィールドの4パターン（llm.rs）
- foreign + surface実在 → 取扱外判定が返る（product_gate.rs / api.rs）
- surface不在（幻覚）→ 通常フロー（None）（product_gate.rs / api.rs）
- ambiguous → 通常フロー（None）（api.rs）
- プロンプトにカタログ文字列とforeign基準（確信できる場合のみ）が含まれること（llm.rs）
- LLM実呼び出しは対象外（すべてスタブ/モック `MockLlm` 経由）

## 明確な境界（これ以上は実装しない）

- §3.2（材料選別）・§3.3（聞き返しプロンプト）・§3.4（下書きプロンプト）・§3.5（応答側ゲート）は実装済み。触らない。
- `record_out_of_scope_case`（一段目専用）は二段目では**呼ばない**。`outcome.case_id`をそのまま使う（evaluate()が既にcase作成/記録を完了しているため）。
- 監査カウント・ダッシュボード化はscope外（design doc §7）。今回追加するのは上記の`tracing::info!`1行のみでよい。
- `products.json` / Cloud Run / CLAUDE.md は一切変更しない。
- 新しいCargo依存を追加しない。

## 検証

作業完了前に以下を**前景で完走**させ、実際の出力を確認すること（全文をそのまま貼らず、末尾やエラー行を要約すること）:

```
cargo build --manifest-path server/Cargo.toml 2>&1 | tail -60
cargo test --manifest-path server/Cargo.toml 2>&1 | tail -100
cargo fmt --manifest-path server/Cargo.toml -- --check
```

`cargo fmt`が差分を出す場合は`cargo fmt --manifest-path server/Cargo.toml`を実行して整形を確定させてから再度`--check`で確認すること。`cargo clippy`があれば合わせて確認して構わないが必須ではない。

コミットはしないこと（working tree の差分として残す。レビュー後にオーケストレーター側でコミットする）。

## 報告してほしいこと（Output Contract に加えて）

- 変更したファイル一覧
- `cargo test`の合否件数（新規テスト名を含む一覧）
- `cargo fmt --check`の結果
- 上記「明確な境界」を超える判断が必要になった箇所があれば、実装せずにその内容を報告すること

## Issue #52 による変更点（2026-08-26、supersede 注記）

本ファイルの「追加の LLM 呼び出しは作らない（既存呼び出しに同乗させる）」という結論は、
Issue #52 で覆った。現行実装（正本は `server/src/harness/extraction.rs` /
`server/src/harness/mod.rs`）は以下のとおりで、本ファイルの該当コード例は歴史的記録として
残すが、現在の実装を表さない。

- **同乗をやめた理由**: signals 抽出単体のタスクに catalog（取扱一覧）が混入し、system
  prompt の再構築・出力形式・パース処理・truncation 扱いにまで影響していたため。
- **現行構成**: signals 抽出（`extraction::HybridExtractor`）と製品参照抽出
  （`extraction::ProductReferenceExtractor`、専用の LLM 呼び出し）を別コンポーネントとして
  分離し、`extraction::extract_signals_and_product_references` が両者を `tokio::join!` で
  並列発行する。
- **Anthropic 呼び出し回数への影響**: evaluate 1 回あたりの呼び出しが 1 回 → 2 回になった
  （`harness.customer_reply_draft_enabled = true` の構成では 3 回）。**顧客問い合わせ本文が
  Anthropic へ 2 回送信される。**
- **劣化時の対応（Critical 是正）**: signals 抽出が `ExtractionMode::LexiconFallback` へ
  落ちたターンは、製品参照抽出が独立して成功していても `product_references` を採用しない
  （`Harness::evaluate` 内 `adopt_product_references_for_extraction_mode`）。質問側ゲート
  二段目が、signals 抽出の失敗とは無関係な foreign 判定だけを根拠に `evaluate()` の
  fail-closed 判定（全件エスカレーション）を破棄しないための対応であり、PR #30 以前と同一の
  end-to-end 挙動を維持する。
