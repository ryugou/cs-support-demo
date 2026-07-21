use crate::manual::schema_ids::{manual_node_id, KIND_DOC, KIND_PRODUCT, KIND_SECTION};
use crate::model::{GraphBuild, GraphEdge, GraphNode};
use sha2::{Digest, Sha256};

pub struct ManualSectionInput {
    pub slug: String,
    pub title: String,
    pub body: String,
    pub source_url: String,
    pub breadcrumb: String,
    pub section_no: Option<String>,
    pub order: i32,
    pub parent_slug: Option<String>,
    pub product_models: Vec<String>,
    pub signal_values: Vec<String>,
    /// 英語原文の本文（`body` は検索対象の日本語訳）。`Some` のときだけ
    /// ManualSection の `body_original` 属性として書き出す。原文を持たない
    /// ソース（urtect Google Sites 等）は `None` を渡し、属性自体を書かない。
    pub body_original: Option<String>,
    /// 英語原文本文の hash。`Some` のときだけ `original_hash` 属性を書く。
    pub original_hash: Option<String>,
}

pub struct ManualProductInput {
    pub model: String,
    pub name: String,
    pub aliases: Vec<String>,
}

pub fn content_hash(normalized_body: &str) -> String {
    format!("{:x}", Sha256::digest(normalized_body.as_bytes()))
}

pub fn build_document_node(
    schema: &str,
    doc_key: &str,
    title: &str,
    source_url: &str,
    fetched_at: &str,
) -> GraphNode {
    GraphNode {
        id: manual_node_id(schema, KIND_DOC, doc_key),
        node_type: KIND_DOC.to_string(),
        attributes: vec![
            ("doc_key".to_string(), doc_key.to_string()),
            ("title".to_string(), title.to_string()),
            ("source_url".to_string(), source_url.to_string()),
            ("fetched_at".to_string(), fetched_at.to_string()),
        ],
    }
}

pub fn build_product_node(schema: &str, p: &ManualProductInput) -> GraphNode {
    GraphNode {
        id: manual_node_id(schema, KIND_PRODUCT, &p.model),
        node_type: KIND_PRODUCT.to_string(),
        attributes: vec![
            ("product_key".to_string(), p.model.clone()),
            ("name".to_string(), p.name.clone()),
            ("model".to_string(), p.model.clone()),
            ("aliases".to_string(), p.aliases.join(",")),
        ],
    }
}

/// ManualSection ノード + HAS_SECTION/PARENT_OF/DESCRIBES/MENTIONS_SIGNAL 辺 + Signal ノード。
///
/// 多言語予約属性 body_original / original_hash は `Some` のときだけ書き出す
/// （`None` のときは属性自体を書かず、従来の純予約動作を維持する）。source_lang は
/// body_original が `Some`（= 英語原文を持ち日本語訳を body に載せた alarm.com 等）なら
/// `"en"`、`None`（= urtect Google Sites 等の原文が日本語のソース）なら従来どおり `"ja"`。
pub fn build_section_graph(
    schema: &str,
    doc_key: &str,
    s: &ManualSectionInput,
    content_hash_hex: &str,
) -> GraphBuild {
    let sec_id = manual_node_id(schema, KIND_SECTION, &s.slug);
    let source_lang = if s.body_original.is_some() {
        "en"
    } else {
        "ja"
    };
    let mut attributes = vec![
        ("section_key".to_string(), s.slug.clone()),
        ("doc_key".to_string(), doc_key.to_string()),
        ("title".to_string(), s.title.clone()),
        ("body".to_string(), s.body.clone()),
        ("source_url".to_string(), s.source_url.clone()),
        ("breadcrumb".to_string(), s.breadcrumb.clone()),
        (
            "section_no".to_string(),
            s.section_no.clone().unwrap_or_default(),
        ),
        ("order".to_string(), s.order.to_string()),
        ("source_lang".to_string(), source_lang.to_string()),
        ("content_hash".to_string(), content_hash_hex.to_string()),
    ];
    // 予約属性は Some のときだけ書く（None は属性を書かず純予約のまま）。
    if let Some(body_original) = &s.body_original {
        attributes.push(("body_original".to_string(), body_original.clone()));
    }
    if let Some(original_hash) = &s.original_hash {
        attributes.push(("original_hash".to_string(), original_hash.clone()));
    }
    let mut nodes = vec![GraphNode {
        id: sec_id.clone(),
        node_type: KIND_SECTION.to_string(),
        attributes,
    }];
    let mut edges = vec![GraphEdge {
        from_id: manual_node_id(schema, KIND_DOC, doc_key),
        to_id: sec_id.clone(),
        edge_type: "HAS_SECTION".to_string(),
        attributes: Vec::new(),
    }];
    if let Some(parent) = &s.parent_slug {
        edges.push(GraphEdge {
            from_id: manual_node_id(schema, KIND_SECTION, parent),
            to_id: sec_id.clone(),
            edge_type: "PARENT_OF".to_string(),
            attributes: Vec::new(),
        });
    }
    for model in &s.product_models {
        edges.push(GraphEdge {
            from_id: sec_id.clone(),
            to_id: manual_node_id(schema, KIND_PRODUCT, model),
            edge_type: "DESCRIBES".to_string(),
            attributes: Vec::new(),
        });
    }
    for sig in &s.signal_values {
        let sig_id = manual_node_id(schema, "Signal", sig);
        nodes.push(GraphNode {
            id: sig_id.clone(),
            node_type: "Signal".to_string(),
            attributes: vec![("value".to_string(), sig.clone())],
        });
        edges.push(GraphEdge {
            from_id: sec_id.clone(),
            to_id: sig_id,
            edge_type: "MENTIONS_SIGNAL".to_string(),
            attributes: Vec::new(),
        });
    }
    GraphBuild { nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ManualSectionInput {
        ManualSectionInput {
            slug: "sec-sd-not-recognized".into(),
            title: "SDカードが認識されない".into(),
            body: "SDカードを一度抜き差ししてください。".into(),
            source_url: "https://x/1-4/sd".into(),
            breadcrumb: "1.4 こんなときは > SDカードが認識されない".into(),
            section_no: Some("1.4".into()),
            order: 12,
            parent_slug: Some("sec-1-4".into()),
            product_models: vec!["ADC-V724".into()],
            signal_values: vec!["sd_not_recognized".into()],
            body_original: None,
            original_hash: None,
        }
    }

    #[test]
    fn section_graph_builds_manual_section_and_edges() {
        let build = build_section_graph("urtect", "doc-manual", &sample(), "abc123");
        let sec = build
            .nodes
            .iter()
            .find(|n| n.node_type == "ManualSection")
            .unwrap();
        // body は属性、source_url / order / content_hash が入る。翻訳予約は空
        assert!(sec
            .attributes
            .iter()
            .any(|(k, v)| k == "source_url" && v == "https://x/1-4/sd"));
        assert!(sec
            .attributes
            .iter()
            .any(|(k, v)| k == "content_hash" && v == "abc123"));
        assert!(sec
            .attributes
            .iter()
            .any(|(k, v)| k == "order" && v == "12"));
        assert!(sec
            .attributes
            .iter()
            .any(|(k, v)| k == "source_lang" && v == "ja"));
        // body_original / original_hash が None のとき属性自体を書かない
        assert!(!sec.attributes.iter().any(|(k, _)| k == "body_original"));
        assert!(!sec.attributes.iter().any(|(k, _)| k == "original_hash"));
        // 辺: PARENT_OF（親）/ DESCRIBES（Product）/ MENTIONS_SIGNAL（Signal）
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "PARENT_OF")
                .count(),
            1
        );
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "DESCRIBES")
                .count(),
            1
        );
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "MENTIONS_SIGNAL")
                .count(),
            1
        );
        // Signal ノードも作る（第一級ノード・I2）
        assert_eq!(
            build
                .nodes
                .iter()
                .filter(|n| n.node_type == "Signal")
                .count(),
            1
        );
    }

    #[test]
    fn section_graph_writes_body_original_and_marks_source_lang_en_when_present() {
        // 英語原文を持つソース（alarm.com）は body_original/original_hash を Some で渡す。
        // このとき両属性が正しい値で出力され、source_lang は "en" になる。
        let mut s = sample();
        s.body_original = Some("Please reinsert the SD card.".into());
        s.original_hash = Some("deadbeef".into());
        let build = build_section_graph("urtect", "doc-alarmcom", &s, "abc123");
        let sec = build
            .nodes
            .iter()
            .find(|n| n.node_type == "ManualSection")
            .unwrap();
        assert!(sec
            .attributes
            .iter()
            .any(|(k, v)| k == "body_original" && v == "Please reinsert the SD card."));
        assert!(sec
            .attributes
            .iter()
            .any(|(k, v)| k == "original_hash" && v == "deadbeef"));
        assert!(sec
            .attributes
            .iter()
            .any(|(k, v)| k == "source_lang" && v == "en"));
    }

    #[test]
    fn root_section_has_no_parent_edge() {
        let mut s = sample();
        s.parent_slug = None;
        let build = build_section_graph("urtect", "doc-manual", &s, "h");
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "PARENT_OF")
                .count(),
            0
        );
    }

    #[test]
    fn content_hash_is_stable() {
        assert_eq!(content_hash("同じ本文"), content_hash("同じ本文"));
        assert_ne!(content_hash("A"), content_hash("B"));
    }
}
