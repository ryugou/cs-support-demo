//! signal 抽出のハイブリッド実装（S1-11 改訂: LLM 抽出 ∪ 決定論 lexicon）。
//!
//! `HybridExtractor` は常に lexicon.normalize() を先に評価し（安全床・オフライン動作）、
//! LLM が設定されていれば追加で分類を依頼し、語彙内の signal だけを和集合に加える。
//! LLM 呼び出しが失敗した場合は lexicon 単独の結果にフォールバックする（fail closed
//! ではなく「取りこぼしを許容しない」側の decision-safe フォールバック。判定は必ず
//! 何らかの signal 集合で走る）。どのモードで抽出したかは `ExtractionMode` として
//! 呼び出し側（Harness）が WORM 監査に記録する。
//!
//! Issue #52: 取扱製品への言及抽出（`ProductReferenceExtractor`）は、このファイル下部に
//! signal 抽出とは完全に独立したコンポーネントとして定義する。PR #30 では一時的にこの機能が
//! signal 抽出（`HybridExtractor`）に catalog 引数として同乗しており、system prompt の
//! 再構築・出力形式・エラー処理にまで影響していた。両者は別々の LLM 呼び出しであり、
//! **この抽出層の中では**一方の失敗がもう一方の結果に影響することはない。
//!
//! ただし `Harness::evaluate`（`harness/mod.rs`）は、抽出結果を受け取った直後に
//! `adopt_product_references_for_extraction_mode` という別の policy を適用する: signals 抽出が
//! `ExtractionMode::LexiconFallback` へ落ちたターンは `product_references` を採用しない。
//! これは抽出層の独立性とは別レイヤの話で、質問側ゲート二段目が signals 劣化時に
//! `evaluate()` の fail-closed 判定（全件エスカレーション）を破棄しないための、harness 側の
//! 意図的な結合である（Issue #52 フォローアップ）。

use super::signal::{LexiconNormalizer, Signal, SignalNormalizer, SignalSet};
use crate::llm::AnthropicClient;
use std::sync::Arc;

/// このターンの抽出がどの経路を通ったか（WORM 監査に記録する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionMode {
    /// LLM が設定されていない（config.llm.enabled = false）。lexicon 単独。
    LexiconOnly,
    /// LLM 分類に成功し、lexicon との和集合を返した。
    Hybrid,
    /// LLM が設定されていたが分類呼び出しが失敗し、lexicon 単独にフォールバックした。
    LexiconFallback,
}

impl ExtractionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExtractionMode::LexiconOnly => "lexicon_only",
            ExtractionMode::Hybrid => "hybrid",
            ExtractionMode::LexiconFallback => "lexicon_fallback",
        }
    }
}

/// `Option<ExtractionMode>` を WORM 監査の `extraction_mode` 文字列に変換する
/// （S1-11 followup: audit_with_nodes の型を Option<ExtractionMode> に統一した際の
/// 単一の変換窓口）。
///
/// 抽出を行わない tool（resolve_product / get_section / get_product /
/// search_past_cases / legacy search_manual 等）は `None` を渡し `"not_applicable"` に
/// なる。抽出を伴う経路（evaluate、および読み取り系の signal 抽出統一後の
/// search_manual / search_known_resolutions）は `Some` を渡し、
/// `ExtractionMode::as_str()` の既存値をそのまま使う（WORM の serialize 結果は
/// 不変のまま）。
pub fn audit_extraction_mode(mode: Option<ExtractionMode>) -> String {
    mode.map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "not_applicable".to_string())
}

/// 抽出結果（signal 集合 + どの経路で得られたか）。
#[derive(Debug, Clone)]
pub struct ExtractionResult {
    pub signals: SignalSet,
    pub mode: ExtractionMode,
}

/// Harness が保持する抽出口の trait 境界。`evaluate` / `root_cause_probe` はこれ経由で
/// signal を得る（lexicon 単体の `SignalNormalizer` は admission 検証用に別途残す）。
#[async_trait::async_trait]
pub trait AsyncSignalExtractor: Send + Sync {
    async fn extract(&self, question: &str) -> ExtractionResult;
}

/// LLM 分類の抽象境界。`AnthropicClient::classify_signals` はテストで直接叩けない
/// （実 API 呼び出し）ため、この trait でモック注入できるようにする。
#[async_trait::async_trait]
pub trait ClassifyLlm: Send + Sync {
    async fn classify(&self, question: &str) -> anyhow::Result<Vec<String>>;
}

/// `AnthropicClient` を `ClassifyLlm` として使うためのアダプタ。
/// system prompt（injection 対策文言 + signal 語彙）は構築時に 1 度だけ組み立て、
/// 以後の呼び出しで使い回す（毎ターン `build_system_prompt` を再実行しない）。
pub struct AnthropicSignalClassifier {
    client: AnthropicClient,
    system_prompt: String,
}

impl AnthropicSignalClassifier {
    pub fn new(client: AnthropicClient, vocabulary_prompt: String) -> Self {
        Self {
            client,
            system_prompt: crate::llm::build_system_prompt(&vocabulary_prompt),
        }
    }
}

#[async_trait::async_trait]
impl ClassifyLlm for AnthropicSignalClassifier {
    async fn classify(&self, question: &str) -> anyhow::Result<Vec<String>> {
        self.client
            .classify_signals(question, &self.system_prompt)
            .await
    }
}

/// lexicon ∪ LLM のハイブリッド抽出器。
///
/// - `llm = None`: lexicon 単独（`ExtractionMode::LexiconOnly`）。
/// - `llm = Some`: lexicon.normalize() と LLM 分類結果（語彙内のみ）の和集合
///   （`ExtractionMode::Hybrid`）。LLM 呼び出しが失敗したら lexicon 単独に
///   フォールバックする（`ExtractionMode::LexiconFallback`）。
///
/// LLM が返す signal 名は `lexicon.contains_signal` で語彙照合してから採用する
/// （語彙外の signal は 3 層判定・admission のどちらでも意味を持たないため破棄する）。
pub struct HybridExtractor {
    lexicon: Arc<LexiconNormalizer>,
    llm: Option<Arc<dyn ClassifyLlm>>,
}

impl HybridExtractor {
    pub fn new(lexicon: Arc<LexiconNormalizer>, llm: Option<Arc<dyn ClassifyLlm>>) -> Self {
        Self { lexicon, llm }
    }
}

#[async_trait::async_trait]
impl AsyncSignalExtractor for HybridExtractor {
    async fn extract(&self, question: &str) -> ExtractionResult {
        let lexicon_signals = self.lexicon.normalize(question);
        let Some(llm) = &self.llm else {
            return ExtractionResult {
                signals: lexicon_signals,
                mode: ExtractionMode::LexiconOnly,
            };
        };
        match llm.classify(question).await {
            Ok(raw_names) => {
                // 語彙内の signal だけ採用してから lexicon 分と和集合する
                // （mod.rs の累積 signal 集合と同じ union/collect の作法に揃える）。
                let llm_signals: SignalSet = raw_names
                    .into_iter()
                    .filter(|name| {
                        let known = self.lexicon.contains_signal(name);
                        if !known {
                            tracing::debug!(
                                signal = %name,
                                "llm returned an out-of-vocabulary signal; discarding"
                            );
                        }
                        known
                    })
                    .map(Signal::new)
                    .collect();
                ExtractionResult {
                    signals: lexicon_signals.union(&llm_signals).cloned().collect(),
                    mode: ExtractionMode::Hybrid,
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "llm signal classification failed; falling back to lexicon-only extraction"
                );
                ExtractionResult {
                    signals: lexicon_signals,
                    mode: ExtractionMode::LexiconFallback,
                }
            }
        }
    }
}

// ==== Issue #52: 製品参照抽出（signals 抽出から分離した独立コンポーネント） ====

/// 製品参照抽出 LLM 呼び出しの抽象境界。`AnthropicClient::extract_product_references` は
/// テストで直接叩けない（実 API 呼び出し）ため、この trait でモック注入できるようにする
/// （`ClassifyLlm` と同じ設計判断）。
#[async_trait::async_trait]
pub trait ClassifyProductReferences: Send + Sync {
    async fn classify(
        &self,
        question: &str,
        catalog: &str,
    ) -> anyhow::Result<Vec<super::product_gate::ProductReference>>;
}

/// `AnthropicClient` を `ClassifyProductReferences` として使うためのアダプタ。
/// catalog（取扱一覧）はターンごとに変わりうる（`ProductGate` の TTL キャッシュ経由）ため、
/// signals 用の `AnthropicSignalClassifier` と異なり system prompt はキャッシュしない
/// （呼び出しのたびに `build_product_reference_system_prompt` で組み立てる）。
pub struct AnthropicProductReferenceClassifier {
    client: AnthropicClient,
}

impl AnthropicProductReferenceClassifier {
    pub fn new(client: AnthropicClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ClassifyProductReferences for AnthropicProductReferenceClassifier {
    async fn classify(
        &self,
        question: &str,
        catalog: &str,
    ) -> anyhow::Result<Vec<super::product_gate::ProductReference>> {
        self.client
            .extract_product_references(question, catalog)
            .await
    }
}

/// 製品参照抽出器。LLM が未設定、または呼び出しが失敗した場合は空配列に degrade する
/// （signals 抽出が lexicon フォールバックへ倒れるのと同じ「取りこぼしを許容しない」側の
/// decision-safe フォールバックだが、こちらには lexicon のような決定論の床が無いため、
/// 空配列＝「今ターンは製品言及なしとして扱う」が床になる）。
///
/// 呼び出し元（`api.rs::second_stage_out_of_scope_reply` 等）は空配列を「foreign 判定なし」
/// として扱い、通常の評価フローへフォールスルーする。**この抽出層自体は判定を fail closed
/// （＝疑わしきは全件エスカレーション）にはしない**: 製品参照抽出はあくまで質問側ゲートの
/// 二段目（LLM 解釈）であり、一段目の型番ゲート・材料事前除外・応答側ゲートは製品参照抽出の
/// 成否と無関係に独立して機能し続けるため、ここで安全側に倒す必要が無い。
///
/// signals 抽出（`HybridExtractor`）とは完全に独立したコンポーネント: 一方の LLM 呼び出しが
/// 失敗しても、もう一方の結果には一切影響しない（Issue #52 要件3）。**この独立性は抽出層
/// （この struct）の中で閉じている。** `Harness::evaluate` は抽出結果を受け取った直後、
/// signals 抽出が lexicon フォールバックへ落ちていた場合に限りこの抽出器の結果を採用しない
/// （policy は harness 側にあり、この struct自体は持たない。`harness/mod.rs` の
/// `adopt_product_references_for_extraction_mode` を参照）。
pub struct ProductReferenceExtractor {
    llm: Option<Arc<dyn ClassifyProductReferences>>,
}

impl ProductReferenceExtractor {
    pub fn new(llm: Option<Arc<dyn ClassifyProductReferences>>) -> Self {
        Self { llm }
    }

    pub async fn extract(
        &self,
        question: &str,
        catalog: &str,
    ) -> Vec<super::product_gate::ProductReference> {
        let Some(llm) = &self.llm else {
            return Vec::new();
        };
        match llm.classify(question, catalog).await {
            Ok(refs) => refs,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "llm product reference extraction failed; treating this turn as if no \
                     product reference was mentioned (product understanding degrades, the \
                     question-side gate stage 1 / material pre-filter / answer-side gate are \
                     unaffected)"
                );
                Vec::new()
            }
        }
    }
}

/// signals 抽出と製品参照抽出を `tokio::join!` で並列発行し、結果をまとめて返す
/// （Issue #52 要件2: 直列化によるレイテンシ増を防ぐ）。
///
/// 呼び出し元（`Harness::evaluate`）が `resolutions` の読み込みと合わせて更に外側で
/// 並列化できるよう、この関数自体は 2 者の join だけに閉じている。2 つの抽出は完全に独立
/// している（`HybridExtractor` と `ProductReferenceExtractor` は別々の `Arc<dyn ...>` を
/// 経由し、共有状態を持たない）ため、この関数の中で一方の失敗がもう一方に波及することはない
/// （要件3。単体テスト `product_reference_extraction_failure_does_not_affect_signals` /
/// `signal_extraction_failure_does_not_affect_product_references` が独立性を検証する）。
///
/// **この関数が返す `product_references` を呼び出し元が無条件に採用してよいわけではない。**
/// `Harness::evaluate` は戻り値を受け取った直後、signals 抽出が `ExtractionMode::
/// LexiconFallback` へ落ちていた場合に限り `product_references` を空へ落とす
/// （`adopt_product_references_for_extraction_mode`）。質問側ゲート二段目が signals 劣化時に
/// evaluate() の fail-closed 判定を破棄しないための harness 側の policy であり、ここでの
/// 独立性（2 呼び出しが互いに影響しないこと）とは別レイヤの話である。
pub async fn extract_signals_and_product_references(
    signal_extractor: &dyn AsyncSignalExtractor,
    product_reference_extractor: &ProductReferenceExtractor,
    question: &str,
    catalog: &str,
) -> (ExtractionResult, Vec<super::product_gate::ProductReference>) {
    tokio::join!(
        signal_extractor.extract(question),
        product_reference_extractor.extract(question, catalog),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 検証用 lexicon（signal.rs のテストと同じ素の JSON リテラル方式）。
    /// テスト質問「カビが生えた」に一致するのは mold のみで、discoloration は
    /// 語彙には存在するがテキスト一致しない（= LLM 追加分の検証に使う）。
    fn lexicon() -> Arc<LexiconNormalizer> {
        Arc::new(
            LexiconNormalizer::from_json(
                r#"{ "signals": [
                    { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"] },
                    { "signal": "discoloration", "class": "hazard", "surface_forms": ["変色"] }
                ] }"#,
            )
            .expect("test lexicon builds"),
        )
    }

    struct MockLlm {
        result: Result<Vec<String>, String>,
    }

    impl MockLlm {
        fn ok(signals: Vec<&str>) -> Arc<dyn ClassifyLlm> {
            Arc::new(Self {
                result: Ok(signals.into_iter().map(str::to_string).collect()),
            })
        }

        fn err() -> Arc<dyn ClassifyLlm> {
            Arc::new(Self {
                result: Err("llm unavailable".to_string()),
            })
        }
    }

    #[async_trait::async_trait]
    impl ClassifyLlm for MockLlm {
        async fn classify(&self, _question: &str) -> anyhow::Result<Vec<String>> {
            self.result.clone().map_err(|msg| anyhow::anyhow!(msg))
        }
    }

    #[tokio::test]
    async fn union_keeps_lexicon_hits_and_validates_vocab() {
        let ex = HybridExtractor::new(
            lexicon(),
            Some(MockLlm::ok(vec!["discoloration", "evil_signal"])),
        );
        let r = ex.extract("カビが生えた").await;
        assert!(r.signals.iter().any(|s| s.as_str() == "mold")); // lexicon 床は不変
        assert!(r.signals.iter().any(|s| s.as_str() == "discoloration")); // LLM 追加分（語彙内）
        assert!(r.signals.iter().all(|s| s.as_str() != "evil_signal")); // 語彙外破棄
        assert!(matches!(r.mode, ExtractionMode::Hybrid));
    }

    #[tokio::test]
    async fn llm_failure_falls_back_to_lexicon() {
        let ex = HybridExtractor::new(lexicon(), Some(MockLlm::err()));
        let r = ex.extract("カビが生えた").await;
        assert!(r.signals.iter().any(|s| s.as_str() == "mold"));
        assert!(matches!(r.mode, ExtractionMode::LexiconFallback));
    }

    #[tokio::test]
    async fn no_llm_configured_is_lexicon_only() {
        let ex = HybridExtractor::new(lexicon(), None);
        let r = ex.extract("カビが生えた").await;
        assert!(r.signals.iter().any(|s| s.as_str() == "mold"));
        assert!(matches!(r.mode, ExtractionMode::LexiconOnly));
    }

    // ==== Issue #52: ProductReferenceExtractor（signals 抽出から分離した独立コンポーネント）====

    fn sample_reference() -> super::super::product_gate::ProductReference {
        super::super::product_gate::ProductReference {
            surface: "ADC-VDB101".to_string(),
            resolution: super::super::product_gate::ProductReferenceResolution::Foreign,
            matched_model: None,
        }
    }

    struct MockProductReferenceLlm {
        result: Result<Vec<super::super::product_gate::ProductReference>, String>,
    }

    impl MockProductReferenceLlm {
        fn ok(
            refs: Vec<super::super::product_gate::ProductReference>,
        ) -> Arc<dyn ClassifyProductReferences> {
            Arc::new(Self { result: Ok(refs) })
        }

        fn err() -> Arc<dyn ClassifyProductReferences> {
            Arc::new(Self {
                result: Err("llm unavailable".to_string()),
            })
        }
    }

    #[async_trait::async_trait]
    impl ClassifyProductReferences for MockProductReferenceLlm {
        async fn classify(
            &self,
            _question: &str,
            _catalog: &str,
        ) -> anyhow::Result<Vec<super::super::product_gate::ProductReference>> {
            self.result.clone().map_err(|msg| anyhow::anyhow!(msg))
        }
    }

    #[tokio::test]
    async fn product_reference_extractor_returns_the_llm_result_on_success() {
        let refs = vec![sample_reference()];
        let extractor =
            ProductReferenceExtractor::new(Some(MockProductReferenceLlm::ok(refs.clone())));
        let out = extractor
            .extract("ADC-VDB101について教えてください", "ADC-V724")
            .await;
        assert_eq!(out, refs);
    }

    #[tokio::test]
    async fn product_reference_extractor_degrades_to_empty_on_llm_failure() {
        let extractor = ProductReferenceExtractor::new(Some(MockProductReferenceLlm::err()));
        let out = extractor.extract("質問", "ADC-V724").await;
        assert!(
            out.is_empty(),
            "an llm failure must degrade to 'no product reference this turn', not propagate \
             the error (product understanding degrades gracefully; the question-side gate \
             stage 1 / material pre-filter / answer-side gate are unaffected)"
        );
    }

    #[tokio::test]
    async fn product_reference_extractor_is_empty_when_no_llm_configured() {
        let extractor = ProductReferenceExtractor::new(None);
        let out = extractor.extract("質問", "ADC-V724").await;
        assert!(out.is_empty());
    }

    // ==== Issue #52 要件2・3: 並列発行と独立性（extract_signals_and_product_references）====

    struct DelayedMockLlm {
        delay: std::time::Duration,
        result: Result<Vec<String>, String>,
    }

    #[async_trait::async_trait]
    impl ClassifyLlm for DelayedMockLlm {
        async fn classify(&self, _question: &str) -> anyhow::Result<Vec<String>> {
            tokio::time::sleep(self.delay).await;
            self.result.clone().map_err(|msg| anyhow::anyhow!(msg))
        }
    }

    struct DelayedMockProductReferenceLlm {
        delay: std::time::Duration,
        result: Result<Vec<super::super::product_gate::ProductReference>, String>,
    }

    #[async_trait::async_trait]
    impl ClassifyProductReferences for DelayedMockProductReferenceLlm {
        async fn classify(
            &self,
            _question: &str,
            _catalog: &str,
        ) -> anyhow::Result<Vec<super::super::product_gate::ProductReference>> {
            tokio::time::sleep(self.delay).await;
            self.result.clone().map_err(|msg| anyhow::anyhow!(msg))
        }
    }

    /// Issue #52 要件2 の直接証拠: signals 抽出と製品参照抽出が直列ではなく `tokio::join!` で
    /// 並列発行されること。両方の LLM 呼び出しに同じ遅延を仕込み、直列なら delay×2、並列なら
    /// 概ね delay×1 で完了することを、tokio の仮想時計（`start_paused`）で決定論的に検証する
    /// （実時間を待たない。純粋な `tokio::time::sleep` なので paused clock が正しく前進する。
    /// `llm.rs` の messages stub コメントが警告する「実ソケット I/O では pause が効かない」
    /// 制約はここでは当てはまらない）。
    #[tokio::test(start_paused = true)]
    async fn extract_signals_and_product_references_runs_both_extractions_concurrently() {
        const DELAY: std::time::Duration = std::time::Duration::from_millis(100);
        let signal_llm: Arc<dyn ClassifyLlm> = Arc::new(DelayedMockLlm {
            delay: DELAY,
            result: Ok(vec!["mold".to_string()]),
        });
        let signal_extractor = HybridExtractor::new(lexicon(), Some(signal_llm));
        let product_reference_llm: Arc<dyn ClassifyProductReferences> =
            Arc::new(DelayedMockProductReferenceLlm {
                delay: DELAY,
                result: Ok(vec![sample_reference()]),
            });
        let product_reference_extractor =
            ProductReferenceExtractor::new(Some(product_reference_llm));

        let start = tokio::time::Instant::now();
        let (signals_outcome, product_references) = extract_signals_and_product_references(
            &signal_extractor,
            &product_reference_extractor,
            "カビが生えた",
            "ADC-V724",
        )
        .await;
        let elapsed = start.elapsed();

        assert!(signals_outcome.signals.iter().any(|s| s.as_str() == "mold"));
        assert_eq!(product_references, vec![sample_reference()]);
        assert!(
            elapsed < DELAY * 2,
            "the two extractions must run concurrently via tokio::join! (elapsed {elapsed:?} \
             must be well under {:?}, which is what serial (extract-then-extract) execution \
             would take)",
            DELAY * 2
        );
    }

    /// Issue #52 要件3（独立性、方向1）: signals 抽出が失敗しても、製品参照抽出の結果には
    /// 一切影響しない。
    #[tokio::test]
    async fn signal_extraction_failure_does_not_affect_product_references() {
        let signal_extractor = HybridExtractor::new(lexicon(), Some(MockLlm::err()));
        let product_reference_extractor =
            ProductReferenceExtractor::new(Some(MockProductReferenceLlm::ok(vec![
                sample_reference(),
            ])));

        let (signals_outcome, product_references) = extract_signals_and_product_references(
            &signal_extractor,
            &product_reference_extractor,
            "カビが生えた",
            "ADC-V724",
        )
        .await;

        assert!(
            matches!(signals_outcome.mode, ExtractionMode::LexiconFallback),
            "signals extraction must still degrade to lexicon fallback on its own failure"
        );
        assert_eq!(
            product_references,
            vec![sample_reference()],
            "product reference extraction must succeed independently of the signals failure"
        );
    }

    /// Issue #52 要件3（独立性、方向2）: 製品参照抽出が失敗しても、signals 抽出の結果には
    /// 一切影響しない。
    #[tokio::test]
    async fn product_reference_extraction_failure_does_not_affect_signals() {
        let signal_extractor =
            HybridExtractor::new(lexicon(), Some(MockLlm::ok(vec!["discoloration"])));
        let product_reference_extractor =
            ProductReferenceExtractor::new(Some(MockProductReferenceLlm::err()));

        let (signals_outcome, product_references) = extract_signals_and_product_references(
            &signal_extractor,
            &product_reference_extractor,
            "カビが生えた",
            "ADC-V724",
        )
        .await;

        assert!(matches!(signals_outcome.mode, ExtractionMode::Hybrid));
        assert!(
            signals_outcome
                .signals
                .iter()
                .any(|s| s.as_str() == "discoloration"),
            "signals extraction must succeed independently of the product reference failure"
        );
        assert!(
            product_references.is_empty(),
            "product reference extraction must degrade to empty on its own failure"
        );
    }

    #[test]
    fn audit_extraction_mode_none_is_not_applicable() {
        // 抽出を行わない tool（read 系）は WORM に "not_applicable" を記録する。
        assert_eq!(audit_extraction_mode(None), "not_applicable");
    }

    #[test]
    fn audit_extraction_mode_some_uses_as_str() {
        // 抽出を伴う経路（evaluate 等）は ExtractionMode::as_str() の既存値をそのまま使う
        // （WORM の serialize 結果は変えない）。
        assert_eq!(
            audit_extraction_mode(Some(ExtractionMode::Hybrid)),
            ExtractionMode::Hybrid.as_str()
        );
    }

    #[test]
    fn audit_extraction_mode_some_hybrid_is_literal_hybrid_string() {
        // 上のテストは右辺に as_str() を使っており準トートロジー（実装と期待値が同じ関数を
        // 経由するため、as_str() の中身が変わっても検知できない）。WORM に書かれる実際の
        // 文字列値をリテラルで固定し、意図せぬ変更（serialize 結果の破壊的変更）を検知する。
        assert_eq!(
            audit_extraction_mode(Some(ExtractionMode::Hybrid)),
            "hybrid"
        );
    }
}
