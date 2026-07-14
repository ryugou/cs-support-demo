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
/// vocabulary_prompt は signal 語彙から 1 度だけ組み立て、以後の呼び出しで使い回す
/// （毎ターン lexicon から再構築しない）。
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
    async fn classify(&self, question: &str) -> anyhow::Result<Vec<String>> {
        self.client
            .classify_signals(question, &self.vocabulary_prompt)
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
}
