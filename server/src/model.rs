use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ManualCatalog {
    pub products: Vec<ProductInput>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProductInput {
    pub product_key: String,
    pub name_en: String,
    pub name_ja: String,
    pub model: Option<String>,
    pub status: Option<String>,
    pub description_en: Option<String>,
    pub description_ja: Option<String>,
    #[serde(default)]
    pub documents: Vec<DocumentInput>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DocumentInput {
    pub doc_id: String,
    pub title_en: String,
    pub title_ja: String,
    pub version: String,
    pub source_url: Option<String>,
    pub origin_lang: Option<String>,
    #[serde(default)]
    pub sections: Vec<SectionInput>,
    #[serde(default)]
    pub specs: Vec<SpecInput>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SectionInput {
    pub anchor: String,
    pub order: i32,
    pub level: i32,
    pub title_en: String,
    pub title_ja: String,
    pub body_en: Option<String>,
    pub body_ja: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub children: Vec<SectionInput>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SpecInput {
    pub key_slug: String,
    pub key_en: String,
    pub key_ja: String,
    pub value_en: String,
    pub value_ja: Option<String>,
    pub defined_in: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProductCandidate {
    pub product_key: String,
    pub name_ja: String,
    pub name_en: String,
    pub model: Option<String>,
    pub score: f32,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SectionHit {
    pub section_key: String,
    pub title_ja: String,
    pub body_ja: Option<String>,
    pub body_en: Option<String>,
    pub translation_status: Option<String>,
    pub breadcrumb: Vec<String>,
    pub score: f32,
    #[serde(default)]
    pub source_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SectionView {
    pub section: serde_json::Value,
    pub ancestors: Vec<serde_json::Value>,
    pub children: Vec<serde_json::Value>,
    pub references: Vec<serde_json::Value>,
    #[serde(default)]
    pub based_on_rationale: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProductView {
    pub product: serde_json::Value,
    pub specs: Vec<serde_json::Value>,
    pub toc: Vec<serde_json::Value>,
}

// PartialEq は ingest_homesec.rs の冪等性テスト（同一入力から同一 GraphBuild が
// 組み立てられることの検証）のために追加する。既存の GraphBuild/GraphNode/GraphEdge の
// 用途（vegapunk への upsert 入力の一時的な組み立て）はフィールド値の等価性比較で
// 十分に表現でき、他の呼び出し元（ingest_rules.rs / ingest_alarmcom.rs 等）は
// 比較を行わないため既存の振る舞いには影響しない。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphBuild {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    pub id: String,
    pub node_type: String,
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphEdge {
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManualHit {
    pub section_key: String,
    pub title: String,
    pub body: String,
    pub source_url: String,
    pub breadcrumb: String,
    pub score: f32,
    /// score の由来: "text"（fast-path/IDF のみ）| "vector"（意味検索のみ）|
    /// "both"（両方が候補に寄与、score は max）。urtect design §2.3 の監査可能性のため。
    /// additive フィールドなので古いシリアライズ済み値には `#[serde(default)]` で対応する。
    #[serde(default)]
    pub score_source: String,
}

/// score_source は SectionHit に無い（legacy 経路には概念が無いため）ので変換時に捨てる。
/// manual_v1 経路（ManualHit）を LegacySection 経路と同じ `SectionHit` に薄く詰め替える。
/// reserved フィールド body_original / original_hash は読まない（現行実装では未使用）。
impl From<ManualHit> for SectionHit {
    fn from(hit: ManualHit) -> Self {
        SectionHit {
            section_key: hit.section_key,
            title_ja: hit.title,
            body_ja: Some(hit.body),
            body_en: None,
            translation_status: None,
            // legacy 経路は breadcrumb を「階層セグメントごとの Vec」で返すため、
            // ingest が " > " 連結で格納した文字列も同じ意味（1 要素 = 1 階層）に展開する。
            breadcrumb: if hit.breadcrumb.is_empty() {
                Vec::new()
            } else {
                hit.breadcrumb.split(" > ").map(str::to_string).collect()
            },
            score: hit.score,
            source_url: Some(hit.source_url),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManualSectionView {
    pub section: serde_json::Value,
    pub ancestors: Vec<serde_json::Value>,
    pub children: Vec<serde_json::Value>,
    pub based_on_rationale: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManualProductCandidate {
    pub model: String,
    pub name: String,
    pub score: f32,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_hit_breadcrumb_expands_to_hierarchy_segments() {
        let hit = ManualHit {
            section_key: "sec-x".into(),
            title: "SDカードが認識されない".into(),
            body: "本文".into(),
            source_url: "https://x/1-4/sd".into(),
            breadcrumb: "1.4 こんなときは > SDカードが認識されない".into(),
            score: 1.0,
            score_source: "text".into(),
        };
        let s: SectionHit = hit.into();
        // legacy と同じ「1 要素 = 1 階層」の Vec に展開される
        assert_eq!(
            s.breadcrumb,
            vec![
                "1.4 こんなときは".to_string(),
                "SDカードが認識されない".to_string()
            ]
        );
        assert_eq!(s.source_url.as_deref(), Some("https://x/1-4/sd"));
    }

    #[test]
    fn manual_hit_empty_breadcrumb_becomes_empty_vec() {
        let hit = ManualHit {
            section_key: "sec-x".into(),
            title: "t".into(),
            body: "b".into(),
            source_url: "u".into(),
            breadcrumb: String::new(),
            score: 0.5,
            score_source: "text".into(),
        };
        let s: SectionHit = hit.into();
        assert!(s.breadcrumb.is_empty());
    }
}
