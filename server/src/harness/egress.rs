use crate::resolve::normalize_key;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// 決定論 NG 辞書（S1-4）。block = 明示 NG 語、abstain = 暗示効能リスク語。
#[derive(Debug, Clone, Deserialize)]
pub struct NgDictionary {
    pub block_terms: Vec<String>,
    pub abstain_terms: Vec<String>,
    /// Terms that must never appear in customer-facing text; `egress_gate` blocks a match and `NgDictionary::near_hit` does not use them.
    pub customer_leak_terms: Vec<String>,
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
            .any(|term| crate::resolve::norm_contains(&norm, term))
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

impl EgressVerdict {
    /// 監査・永続属性用の正本ラベル（serde の tag 名と一致させる）。
    pub fn label(&self) -> &'static str {
        match self {
            EgressVerdict::Pass => "pass",
            EgressVerdict::Block { .. } => "block",
            EgressVerdict::Abstain { .. } => "abstain",
        }
    }
}

/// 出口ゲート（S1-4）。AI 製・人間製を問わず全 outbound がここを通る（egress 位置固定）。
/// `text` は任意の断片（全文でも文単位でも可。入力単位を固定しない = 遵守事項 2）。
/// Step 1 の判定は channel 非依存（水準はチャネルで変えない）。ctx は将来の
/// ハードゲート/勧告分岐（自動送信 vs 有人音声）のための構造予約。
/// 将来 C′（含意判定）+ Ψ に中身が差し替わっても、この関数境界は不変。
pub fn egress_gate(text: &str, _ctx: &EmitContext, ng: &NgDictionary) -> EgressVerdict {
    let norm = normalize_key(text);
    let contains = |term: &String| crate::resolve::norm_contains(&norm, term);
    if let Some(term) = ng.block_terms.iter().find(|t| contains(t)) {
        return EgressVerdict::Block { term: term.clone() };
    }
    if let Some(term) = ng.customer_leak_terms.iter().find(|t| contains(t)) {
        return EgressVerdict::Block { term: term.clone() };
    }
    if let Some(term) = ng.abstain_terms.iter().find(|t| contains(t)) {
        return EgressVerdict::Abstain { term: term.clone() };
    }
    EgressVerdict::Pass
}

#[cfg(test)]
pub(crate) fn production_ng_dictionary() -> NgDictionary {
    NgDictionary::from_path(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/data/urtect/ng-dictionary.json"
    )))
    .expect("production ng dictionary must load and parse")
}

#[cfg(test)]
mod tests;
