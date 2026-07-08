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
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SectionView {
    pub section: serde_json::Value,
    pub ancestors: Vec<serde_json::Value>,
    pub children: Vec<serde_json::Value>,
    pub references: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProductView {
    pub product: serde_json::Value,
    pub specs: Vec<serde_json::Value>,
    pub toc: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct GraphBuild {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: String,
    pub node_type: String,
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
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
