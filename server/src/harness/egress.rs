use crate::resolve::normalize_key;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// 決定論 NG 辞書（S1-4）。block = 明示 NG 語、abstain = 暗示効能リスク語。
#[derive(Debug, Clone, Deserialize)]
pub struct NgDictionary {
    pub block_terms: Vec<String>,
    pub abstain_terms: Vec<String>,
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
            .any(|term| {
                let t = normalize_key(term);
                !t.is_empty() && norm.contains(&t)
            })
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

/// 出口ゲート（S1-4）。AI 製・人間製を問わず全 outbound がここを通る（egress 位置固定）。
/// `text` は任意の断片（全文でも文単位でも可。入力単位を固定しない = 遵守事項 2）。
/// Step 1 の判定は channel 非依存（水準はチャネルで変えない）。ctx は将来の
/// ハードゲート/勧告分岐（自動送信 vs 有人音声）のための構造予約。
/// 将来 C′（含意判定）+ Ψ に中身が差し替わっても、この関数境界は不変。
pub fn egress_gate(text: &str, _ctx: &EmitContext, ng: &NgDictionary) -> EgressVerdict {
    let norm = normalize_key(text);
    let contains = |term: &String| {
        let t = normalize_key(term);
        !t.is_empty() && norm.contains(&t)
    };
    if let Some(term) = ng.block_terms.iter().find(|t| contains(t)) {
        return EgressVerdict::Block { term: term.clone() };
    }
    if let Some(term) = ng.abstain_terms.iter().find(|t| contains(t)) {
        return EgressVerdict::Abstain { term: term.clone() };
    }
    EgressVerdict::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ng() -> NgDictionary {
        NgDictionary::from_json(
            r#"{ "block_terms": ["必ず治ります", "副作用はありません"],
                 "abstain_terms": ["治る", "痩せる"] }"#,
        )
        .expect("ng dictionary parses")
    }

    fn operator() -> EmitContext {
        EmitContext {
            channel: EmitChannel::Operator,
        }
    }

    #[test]
    fn explicit_ng_word_blocks() {
        match egress_gate(
            "この商品で必ず治りますのでご安心ください",
            &operator(),
            &ng(),
        ) {
            EgressVerdict::Block { term } => assert_eq!(term, "必ず治ります"),
            other => panic!("expected block, got {other:?}"),
        }
    }

    #[test]
    fn implied_efficacy_abstains() {
        // 暗示効能は「検出して通す」でなく「疑わしきは出さない」（S1-4）
        match egress_gate("継続すると治ると言われています", &operator(), &ng()) {
            EgressVerdict::Abstain { term } => assert_eq!(term, "治る"),
            other => panic!("expected abstain, got {other:?}"),
        }
    }

    #[test]
    fn block_takes_precedence_over_abstain() {
        assert!(matches!(
            egress_gate("必ず治りますし痩せます", &operator(), &ng()),
            EgressVerdict::Block { .. }
        ));
    }

    #[test]
    fn clean_draft_passes() {
        assert!(matches!(
            egress_gate(
                "保存方法は直射日光を避けて常温で保管してください",
                &operator(),
                &ng()
            ),
            EgressVerdict::Pass
        ));
    }

    #[test]
    fn gate_accepts_sentence_fragments() {
        // 入力単位を全文に固定しない（Step 3 は文単位で呼ぶ。遵守事項 2）
        assert!(matches!(
            egress_gate("治る", &operator(), &ng()),
            EgressVerdict::Abstain { .. }
        ));
        assert!(matches!(
            egress_gate("", &operator(), &ng()),
            EgressVerdict::Pass
        ));
    }

    #[test]
    fn verdict_is_channel_invariant() {
        // 判定水準はチャネルで変えない（フェーズ不変条件）
        for channel in [
            EmitChannel::Operator,
            EmitChannel::CustomerChat,
            EmitChannel::CustomerVoice,
        ] {
            let ctx = EmitContext { channel };
            assert!(matches!(
                egress_gate("必ず治ります", &ctx, &ng()),
                EgressVerdict::Block { .. }
            ));
        }
    }

    #[test]
    fn near_hit_detects_question_proximity() {
        assert!(ng().near_hit("これを飲むと治るのでしょうか"));
        assert!(!ng().near_hit("送料はいくらですか"));
    }
}
