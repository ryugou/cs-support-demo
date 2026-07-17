use crate::resolve::normalize_key;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// 標準化された条件語（specs/signal-vocabulary.md の語彙）。
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
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

/// 軸1（表現ゆれの吸収）。Step 1 は決定論 lexicon、将来 LLM 実装が同 trait に載る（S1-11）。
pub trait SignalNormalizer: Send + Sync {
    fn normalize(&self, text: &str) -> SignalSet;
}

#[derive(Debug, Deserialize)]
struct LexiconEntry {
    signal: String,
    class: SignalClass,
    surface_forms: Vec<String>,
    /// LLM 抽出専用の signal。文字列照合（surface_forms）には使わず、
    /// 分類（classes マップ）と vocabulary_for_prompt にのみ登録する。
    #[serde(default)]
    llm_only: bool,
    /// LLM プロンプト向けの語義。vocabulary_for_prompt に埋め込まれる。
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct LexiconFile {
    signals: Vec<LexiconEntry>,
}

/// ロード時に正規化済みの照合エントリ（hot path で normalize_key を再計算しない）。
struct CompiledEntry {
    signal: String,
    normalized_forms: Vec<String>,
}

/// vocabulary_for_prompt 向けに保持する語彙 1 件分（signal, class, description）。
struct VocabularyEntry {
    signal: String,
    class: SignalClass,
    description: String,
}

pub struct LexiconNormalizer {
    entries: Vec<CompiledEntry>,
    classes: std::collections::HashMap<String, SignalClass>,
    vocabulary: Vec<VocabularyEntry>,
}

impl LexiconNormalizer {
    pub fn from_path(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read signal lexicon {}", path.display()))?;
        Self::from_json(&body)
    }

    pub fn from_json(body: &str) -> Result<Self> {
        let file: LexiconFile = serde_json::from_str(body).context("parse signal lexicon json")?;
        let classes = file
            .signals
            .iter()
            .map(|entry| (entry.signal.clone(), entry.class))
            .collect();
        let vocabulary = file
            .signals
            .iter()
            .map(|entry| VocabularyEntry {
                signal: entry.signal.clone(),
                class: entry.class,
                description: entry.description.clone(),
            })
            .collect();
        let entries = file
            .signals
            .into_iter()
            // llm_only の signal は文字列照合の対象にしない（classes / vocabulary には登録済み）。
            .filter(|entry| !entry.llm_only)
            .map(|entry| {
                let normalized_forms: Vec<String> = entry
                    .surface_forms
                    .iter()
                    .map(|form| normalize_key(form))
                    .filter(|form| !form.is_empty())
                    .collect();
                // 有効な surface form が 1 つも無い signal は「存在するのに決して
                // 抽出されない」サイレント never-match になるため、ロード時に拒否する。
                if normalized_forms.is_empty() {
                    anyhow::bail!(
                        "signal {} has no usable surface forms after normalization",
                        entry.signal
                    );
                }
                Ok(CompiledEntry {
                    signal: entry.signal,
                    normalized_forms,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            entries,
            classes,
            vocabulary,
        })
    }

    pub fn class_of(&self, signal: &Signal) -> Option<SignalClass> {
        self.classes.get(signal.as_str()).copied()
    }

    /// signal がこの lexicon に登録されているか（llm_only も含む）。
    pub fn contains_signal(&self, name: &str) -> bool {
        self.classes.contains_key(name)
    }

    /// LLM 分類プロンプトに埋め込む語彙一覧。1 signal 1 行、
    /// `signal (class): description` 形式（description は空文字の場合あり）。
    pub fn vocabulary_for_prompt(&self) -> String {
        self.vocabulary
            .iter()
            .map(|entry| {
                let class = match entry.class {
                    SignalClass::Hazard => "hazard",
                    SignalClass::Context => "context",
                };
                format!("{} ({}): {}", entry.signal, class, entry.description)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl SignalNormalizer for LexiconNormalizer {
    fn normalize(&self, text: &str) -> SignalSet {
        let normalized = normalize_key(text);
        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .normalized_forms
                    .iter()
                    .any(|form| normalized.contains(form))
            })
            .map(|entry| Signal::new(&entry.signal))
            .collect()
    }
}

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
        assert_eq!(
            n.class_of(&Signal::new("continue_use_question")),
            Some(SignalClass::Context)
        );
        assert_eq!(n.class_of(&Signal::new("unknown")), None);
    }

    #[test]
    fn rejects_signal_with_no_usable_surface_forms() {
        // 記号のみ（正規化で空になる）の signal はサイレント never-match になるため拒否
        let result = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "broken", "class": "hazard", "surface_forms": ["!!!", "…"] }
            ] }"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn llm_only_entry_allows_empty_surface_forms_and_is_not_string_matched() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
            { "signal": "unclassified_risk", "class": "hazard", "surface_forms": [], "llm_only": true,
              "description": "既存のどの signal にも分類できないが、安全・契約・法務上の不安がある発話" },
            { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"] }
        ] }"#,
        )
        .unwrap();
        assert!(lex
            .normalize("カビが生えた unclassified_risk")
            .iter()
            .all(|s| s.as_str() != "unclassified_risk"));
        assert!(lex.contains_signal("unclassified_risk"));
        assert!(lex.class_of(&Signal::new("unclassified_risk")).is_some());
        let prompt = lex.vocabulary_for_prompt();
        assert!(prompt.contains("unclassified_risk") && prompt.contains("分類できない"));
    }

    #[test]
    fn loads_bundled_lexicon_file() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/signal-lexicon.json");
        let n = LexiconNormalizer::from_path(&path).expect("bundled lexicon loads");
        assert!(n
            .normalize("変色して色味がおかしい")
            .contains(&Signal::new("discoloration")));
    }
}
