use crate::model::SectionInput;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::Path};

pub type Glossary = HashMap<String, String>;

pub fn load_glossary(path: &Path) -> Result<Glossary> {
    let body = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body)?)
}

pub fn validate_fixture_translation(section: &SectionInput, glossary: &Glossary) -> Result<()> {
    if section
        .body_en
        .as_deref()
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return Ok(());
    }
    let body_ja = section.body_ja.as_deref().unwrap_or_default().trim();
    if body_ja.is_empty() {
        return Err(anyhow!("missing Japanese fixture for {}", section.anchor));
    }
    for (en, ja) in glossary {
        if section.body_en.as_deref().unwrap_or_default().contains(en) && !body_ja.contains(ja) {
            // Some glossary entries are product names or generic terms that do not need to
            // appear in every translated sentence. Keep this as a soft validation boundary.
            tracing::debug!(anchor = %section.anchor, en, ja, "glossary entry not present in fixture translation");
        }
    }
    Ok(())
}

// --- Issue #8 v2: 自前翻訳 + Concept 抽出（answers.alarm.com、Gemini Flash 3.6 想定）---
//
// 同じ本文を翻訳用と Concept 抽出用で 2 回 LLM に渡さない設計（design spec の要）。
// 実際の HTTP クライアントは本タスクのスコープ外（Gemini 非依存部分のみ実装）。ここでは
// 呼び出し境界（`translate_and_extract`）と、実装時に使う応答 JSON のパース
// （`parse_translation_response`）だけを確定させ、stub でコンパイル・テストが通る状態にする。

/// Gemini が抽出する Concept 1 件（正規化・別ページ間 fuzzy マージの前段。生の抽出結果）。
/// マージ・グラフノード化は `manual::concept` が担う。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ConceptExtract {
    pub name_en: String,
    pub name_ja: String,
    #[serde(default)]
    pub aliases_ja: Vec<String>,
    /// "feature" | "rule" | "setting" | "term"（プロンプト規律で閉じるが、パース時点では
    /// 自由文字列として受ける。想定外の値が来ても ingest 全体を止めない）。
    pub kind: String,
}

/// 翻訳対象本文そのものではなく、翻訳の一貫性と Concept 抽出精度のためだけに添える
/// 非翻訳の page 文脈（design spec §「翻訳 + Concept 抽出」）。
#[derive(Debug, Clone, Default)]
pub struct TranslationContext {
    pub breadcrumb: String,
    pub parent_title: Option<String>,
}

/// `translate_and_extract` の出力（1 LLM パスで両方を返す契約）。
#[derive(Debug, Clone, PartialEq)]
pub struct TranslationOutput {
    pub body_ja: String,
    pub concepts: Vec<ConceptExtract>,
}

/// Gemini 応答本体の期待 JSON 形状 `{ "body_ja": "...", "concepts": [...] }` のパース。
/// 実 API 呼び出しはここでは行わない（呼び出し側が HTTP レスポンスボディを渡す想定）。
/// 壊れた JSON は Err で返す。呼び出し側（ingest_alarmcom）はこれを記事 skip の判断に使う。
pub fn parse_translation_response(json: &str) -> Result<TranslationOutput> {
    #[derive(Deserialize)]
    struct RawResponse {
        body_ja: String,
        #[serde(default)]
        concepts: Vec<ConceptExtract>,
    }
    let raw: RawResponse = serde_json::from_str(json).context(
        "parse translation response JSON (expected {\"body_ja\": ..., \"concepts\": [...]})",
    )?;
    Ok(TranslationOutput {
        body_ja: raw.body_ja,
        concepts: raw.concepts,
    })
}

/// 英語本文の翻訳 + Concept 抽出を 1 LLM パスで行う境界（Gemini Flash 3.6 を想定）。
///
/// // TODO(#8 Phase A): replace with a real Gemini Flash 3.6 HTTP client — send
/// `en_body` + `context`（非翻訳の page 文脈）+ `glossary`（訳ブレ防止の用語注入）を
/// プロンプトに組み立てて呼び出し、レスポンス本体を `parse_translation_response` に通す。
/// モデル ID / エンドポイント / API キー env 名は config 境界に置き、ここではハードコードしない
/// （spec: 認証情報のハードコード禁止）。
///
/// 現時点は stub 実装: `body_ja` は `en_body` のパススルー、`concepts` は空。
/// `ingest_alarmcom` はこの関数を呼ぶ形に既に配線済みで、上記 TODO を実装するだけで
/// 翻訳・Concept 抽出が有効になる（呼び出し側の変更は不要な設計）。
pub fn translate_and_extract(
    en_body: &str,
    context: &TranslationContext,
    glossary: &Glossary,
) -> Result<TranslationOutput> {
    tracing::debug!(
        breadcrumb = %context.breadcrumb,
        parent_title = ?context.parent_title,
        glossary_terms = glossary.len(),
        body_len = en_body.len(),
        "translate_and_extract stub: passthrough (Gemini not yet wired; Issue #8 Phase A)"
    );
    Ok(TranslationOutput {
        body_ja: en_body.to_string(),
        concepts: Vec::new(),
    })
}

#[cfg(test)]
mod translation_seam_tests {
    use super::*;

    #[test]
    fn parse_translation_response_parses_body_ja_and_concepts() {
        let json = r#"{
            "body_ja": "最初に人が入るとロックが解除されます。",
            "concepts": [
                {
                    "name_en": "First Person In rule",
                    "name_ja": "ファーストパーソンインルール",
                    "aliases_ja": ["ファーストパーソンイン", "最初に人が入る"],
                    "kind": "rule"
                }
            ]
        }"#;
        let out = parse_translation_response(json).expect("valid response parses");
        assert_eq!(out.body_ja, "最初に人が入るとロックが解除されます。");
        assert_eq!(out.concepts.len(), 1);
        assert_eq!(out.concepts[0].name_en, "First Person In rule");
        assert_eq!(out.concepts[0].kind, "rule");
        assert_eq!(
            out.concepts[0].aliases_ja,
            vec![
                "ファーストパーソンイン".to_string(),
                "最初に人が入る".to_string()
            ]
        );
    }

    #[test]
    fn parse_translation_response_defaults_missing_concepts_to_empty() {
        let json = r#"{ "body_ja": "本文のみ" }"#;
        let out = parse_translation_response(json).expect("missing concepts defaults to empty");
        assert_eq!(out.body_ja, "本文のみ");
        assert!(out.concepts.is_empty());
    }

    #[test]
    fn parse_translation_response_errors_on_malformed_json() {
        let broken = r#"{ "body_ja": "#;
        assert!(parse_translation_response(broken).is_err());
    }

    #[test]
    fn parse_translation_response_errors_when_body_ja_missing() {
        // body_ja は必須（Option ではない）。欠落は壊れた応答として Err にする。
        let json = r#"{ "concepts": [] }"#;
        assert!(parse_translation_response(json).is_err());
    }

    #[test]
    fn translate_and_extract_stub_passes_through_body_and_returns_no_concepts() {
        let context = TranslationContext {
            breadcrumb: "Partner > Video Devices > Wi-Fi Setup".to_string(),
            parent_title: Some("Video Devices".to_string()),
        };
        let glossary: Glossary = HashMap::new();
        let out = translate_and_extract("Reinsert the SD card.", &context, &glossary)
            .expect("stub never fails");
        assert_eq!(out.body_ja, "Reinsert the SD card.");
        assert!(out.concepts.is_empty());
    }
}
