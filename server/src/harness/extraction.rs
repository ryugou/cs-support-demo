//! signal 抽出のハイブリッド実装（S1-11 改訂: LLM 抽出 ∪ 決定論 lexicon）。
//!
//! `HybridExtractor` は常に lexicon.normalize() を先に評価し（安全床・オフライン動作）、
//! LLM が設定されていれば追加で分類を依頼し、語彙内の signal だけを和集合に加える。
//! LLM 呼び出しが失敗した場合は lexicon 単独の結果にフォールバックする（fail closed
//! ではなく「取りこぼしを許容しない」側の decision-safe フォールバック。判定は必ず
//! 何らかの signal 集合で走る）。どのモードで抽出したかは `ExtractionMode` として
//! 呼び出し側（Harness）が WORM 監査に記録する。

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

/// 抽出結果（signal 集合 + どの経路で得られたか + 製品参照）。
#[derive(Debug, Clone)]
pub struct ExtractionResult {
    pub signals: SignalSet,
    pub mode: ExtractionMode,
    /// 今ターンでLLMが抽出した製品参照（Issue #28 §3.1 二段目）。`catalog` を渡さない呼び出し
    /// （`extract(_, None)`）や lexicon 単独経路では常に空。
    pub product_references: Vec<super::product_gate::ProductReference>,
}

/// Harness が保持する抽出口の trait 境界。`evaluate` / `root_cause_probe` はこれ経由で
/// signal を得る（lexicon 単体の `SignalNormalizer` は admission 検証用に別途残す）。
///
/// `catalog` は Issue #28 §3.1 二段目用の取扱製品一覧（`ProductAllowlist::display_list()`）。
/// `evaluate()` は `Some` を渡し、それ以外（プレビュー系 tool・`root_cause_probe`）は `None` を
/// 渡す（追加の LLM 呼び出しは作らない前提のため、catalog 注入は evaluate の同乗呼び出しのみ）。
#[async_trait::async_trait]
pub trait AsyncSignalExtractor: Send + Sync {
    async fn extract(&self, question: &str, catalog: Option<&str>) -> ExtractionResult;
}

/// LLM 分類の抽象境界。`AnthropicClient::classify_signals` はテストで直接叩けない
/// （実 API 呼び出し）ため、この trait でモック注入できるようにする。
#[async_trait::async_trait]
pub trait ClassifyLlm: Send + Sync {
    async fn classify(
        &self,
        question: &str,
        catalog: Option<&str>,
    ) -> anyhow::Result<crate::llm::ClassificationOutput>;
}

/// `AnthropicClient` を `ClassifyLlm` として使うためのアダプタ。
///
/// system prompt は `vocabulary_prompt`（生の語彙プロンプト、構築時に 1 度だけ保持）から
/// **毎ターン** `crate::llm::build_system_prompt` で組み立て直す。catalog（取扱一覧）は
/// リクエストごとに変わりうる（Issue #28 §3.1 二段目）ため、system_prompt をキャッシュせず
/// 呼び出しのたびに再構築する必要がある。
pub struct AnthropicSignalClassifier {
    client: AnthropicClient,
    vocabulary_prompt: String,
}

impl AnthropicSignalClassifier {
    pub fn new(client: AnthropicClient, vocabulary_prompt: String) -> Self {
        Self {
            client,
            vocabulary_prompt,
        }
    }
}

#[async_trait::async_trait]
impl ClassifyLlm for AnthropicSignalClassifier {
    async fn classify(
        &self,
        question: &str,
        catalog: Option<&str>,
    ) -> anyhow::Result<crate::llm::ClassificationOutput> {
        let system_prompt = crate::llm::build_system_prompt(&self.vocabulary_prompt, catalog);
        self.client.classify_signals(question, &system_prompt).await
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
    async fn extract(&self, question: &str, catalog: Option<&str>) -> ExtractionResult {
        let lexicon_signals = self.lexicon.normalize(question);
        let Some(llm) = &self.llm else {
            return ExtractionResult {
                signals: lexicon_signals,
                mode: ExtractionMode::LexiconOnly,
                product_references: Vec::new(),
            };
        };
        match llm.classify(question, catalog).await {
            Ok(output) => {
                // 語彙内の signal だけ採用してから lexicon 分と和集合する
                // （mod.rs の累積 signal 集合と同じ union/collect の作法に揃える）。
                let llm_signals: SignalSet = output
                    .signals
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
                    // product_references は signals のような語彙フィルタが不要（そもそも
                    // lexicon には無い概念）。妥当性検証（foreign の幻覚ガード等）は
                    // 呼び出し側（product_gate::confirmed_foreign_reference）の役割。
                    product_references: output.product_references,
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
                    product_references: Vec::new(),
                }
            }
        }
    }
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
        product_references: Vec<super::super::product_gate::ProductReference>,
        /// `classify` に渡された catalog をそのまま記録する（Issue #28 §3.1 二段目:
        /// catalog 伝播テスト用）。
        catalog_log: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl MockLlm {
        /// 呼び出し内容の検査（`catalog_log` 等）が必要なテスト用に、trait object へ
        /// 型消去する前の `Arc<MockLlm>` を返す。
        fn new(
            result: Result<Vec<String>, String>,
            product_references: Vec<super::super::product_gate::ProductReference>,
        ) -> Arc<Self> {
            Arc::new(Self {
                result,
                product_references,
                catalog_log: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn ok(signals: Vec<&str>) -> Arc<dyn ClassifyLlm> {
            Self::new(
                Ok(signals.into_iter().map(str::to_string).collect()),
                Vec::new(),
            )
        }

        fn err() -> Arc<dyn ClassifyLlm> {
            Self::new(Err("llm unavailable".to_string()), Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl ClassifyLlm for MockLlm {
        async fn classify(
            &self,
            _question: &str,
            catalog: Option<&str>,
        ) -> anyhow::Result<crate::llm::ClassificationOutput> {
            self.catalog_log
                .lock()
                .expect("catalog_log mutex poisoned")
                .push(catalog.map(str::to_string));
            self.result
                .clone()
                .map(|signals| crate::llm::ClassificationOutput {
                    signals,
                    product_references: self.product_references.clone(),
                })
                .map_err(|msg| anyhow::anyhow!(msg))
        }
    }

    #[tokio::test]
    async fn union_keeps_lexicon_hits_and_validates_vocab() {
        let ex = HybridExtractor::new(
            lexicon(),
            Some(MockLlm::ok(vec!["discoloration", "evil_signal"])),
        );
        let r = ex.extract("カビが生えた", None).await;
        assert!(r.signals.iter().any(|s| s.as_str() == "mold")); // lexicon 床は不変
        assert!(r.signals.iter().any(|s| s.as_str() == "discoloration")); // LLM 追加分（語彙内）
        assert!(r.signals.iter().all(|s| s.as_str() != "evil_signal")); // 語彙外破棄
        assert!(matches!(r.mode, ExtractionMode::Hybrid));
    }

    #[tokio::test]
    async fn llm_failure_falls_back_to_lexicon() {
        let ex = HybridExtractor::new(lexicon(), Some(MockLlm::err()));
        let r = ex.extract("カビが生えた", None).await;
        assert!(r.signals.iter().any(|s| s.as_str() == "mold"));
        assert!(matches!(r.mode, ExtractionMode::LexiconFallback));
    }

    #[tokio::test]
    async fn no_llm_configured_is_lexicon_only() {
        let ex = HybridExtractor::new(lexicon(), None);
        let r = ex.extract("カビが生えた", None).await;
        assert!(r.signals.iter().any(|s| s.as_str() == "mold"));
        assert!(matches!(r.mode, ExtractionMode::LexiconOnly));
    }

    // ---- Issue #28 §3.1 二段目: product_references の伝播 / catalog 引数の透過 ----

    #[tokio::test]
    async fn hybrid_extraction_propagates_product_references_from_the_llm() {
        let product_references = vec![super::super::product_gate::ProductReference {
            surface: "ADC-VDB101".to_string(),
            resolution: super::super::product_gate::ProductReferenceResolution::Foreign,
            matched_model: None,
        }];
        let mock = MockLlm::new(Ok(Vec::new()), product_references.clone());
        let ex = HybridExtractor::new(lexicon(), Some(mock as Arc<dyn ClassifyLlm>));
        let r = ex
            .extract("ADC-VDB101について教えてください", Some("ADC-V724"))
            .await;
        assert_eq!(r.product_references, product_references);
        assert!(matches!(r.mode, ExtractionMode::Hybrid));
    }

    #[tokio::test]
    async fn llm_failure_results_in_empty_product_references() {
        let ex = HybridExtractor::new(lexicon(), Some(MockLlm::err()));
        let r = ex.extract("カビが生えた", Some("ADC-V724")).await;
        assert!(r.product_references.is_empty());
        assert!(matches!(r.mode, ExtractionMode::LexiconFallback));
    }

    #[tokio::test]
    async fn no_llm_configured_results_in_empty_product_references() {
        let ex = HybridExtractor::new(lexicon(), None);
        let r = ex.extract("カビが生えた", Some("ADC-V724")).await;
        assert!(r.product_references.is_empty());
    }

    #[tokio::test]
    async fn extract_propagates_the_catalog_argument_to_classify() {
        let mock = MockLlm::new(Ok(Vec::new()), Vec::new());
        let catalog_log_handle = mock.clone();
        let ex = HybridExtractor::new(lexicon(), Some(mock as Arc<dyn ClassifyLlm>));
        ex.extract("カビが生えた", Some("ADC-V523、ADC-V724")).await;
        assert_eq!(
            *catalog_log_handle
                .catalog_log
                .lock()
                .expect("catalog_log mutex poisoned"),
            vec![Some("ADC-V523、ADC-V724".to_string())]
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
