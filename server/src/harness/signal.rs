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

pub struct LexiconNormalizer {
    entries: Vec<CompiledEntry>,
    classes: std::collections::HashMap<String, SignalClass>,
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
        let entries = file
            .signals
            .into_iter()
            .map(|entry| CompiledEntry {
                signal: entry.signal,
                normalized_forms: entry
                    .surface_forms
                    .iter()
                    .map(|form| normalize_key(form))
                    .filter(|form| !form.is_empty())
                    .collect(),
            })
            .collect();
        Ok(Self { entries, classes })
    }

    pub fn class_of(&self, signal: &Signal) -> Option<SignalClass> {
        self.classes.get(signal.as_str()).copied()
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
    fn loads_bundled_lexicon_file() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/signal-lexicon.json");
        let n = LexiconNormalizer::from_path(&path).expect("bundled lexicon loads");
        assert!(n
            .normalize("変色して色味がおかしい")
            .contains(&Signal::new("discoloration")));
    }
}
