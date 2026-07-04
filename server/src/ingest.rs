use crate::{
    model::{
        DocumentInput, GraphBuild, GraphEdge, GraphNode, ManualCatalog, ProductInput, SectionInput,
        SpecInput,
    },
    translate::{validate_fixture_translation, Glossary},
};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

pub fn load_catalog(path: &Path) -> Result<ManualCatalog> {
    let body = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body)?)
}

pub fn build_graph(
    schema: &str,
    catalog: &ManualCatalog,
    glossary: &Glossary,
) -> Result<GraphBuild> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for product in &catalog.products {
        let product_id = product_node_id(schema, &product.product_key);
        nodes.push(product_node(schema, product));

        for doc in &product.documents {
            let doc_id = document_node_id(schema, &doc.doc_id);
            nodes.push(document_node(schema, product, doc));
            edges.push(edge(&product_id, &doc_id, "HAS_DOCUMENT"));

            for section in &doc.sections {
                add_section_tree(
                    schema, product, doc, None, section, glossary, &mut nodes, &mut edges,
                )?;
            }

            for spec in &doc.specs {
                let spec_id = spec_node_id(schema, &product.product_key, &spec.key_slug);
                nodes.push(spec_node(schema, product, spec));
                edges.push(edge(&product_id, &spec_id, "HAS_SPEC"));
                if let Some(section_key) = &spec.defined_in {
                    edges.push(edge(
                        &spec_id,
                        &section_node_id(schema, section_key),
                        "DEFINED_IN",
                    ));
                }
            }
        }
    }

    Ok(GraphBuild { nodes, edges })
}

fn add_section_tree(
    schema: &str,
    product: &ProductInput,
    doc: &DocumentInput,
    parent_section_key: Option<&str>,
    section: &SectionInput,
    glossary: &Glossary,
    nodes: &mut Vec<GraphNode>,
    edges: &mut Vec<GraphEdge>,
) -> Result<()> {
    validate_fixture_translation(section, glossary)?;
    let section_key = section_key(&doc.doc_id, &section.anchor);
    let section_id = section_node_id(schema, &section_key);
    nodes.push(section_node(schema, product, doc, section));

    let parent_id = match parent_section_key {
        Some(key) => section_node_id(schema, key),
        None => document_node_id(schema, &doc.doc_id),
    };
    edges.push(edge(&parent_id, &section_id, "CONTAINS"));

    for target_key in &section.references {
        edges.push(edge(
            &section_id,
            &section_node_id(schema, target_key),
            "REFERENCES",
        ));
    }

    for child in &section.children {
        add_section_tree(
            schema,
            product,
            doc,
            Some(&section_key),
            child,
            glossary,
            nodes,
            edges,
        )?;
    }

    Ok(())
}

pub fn section_key(doc_id: &str, anchor: &str) -> String {
    format!("{doc_id}#{anchor}")
}

pub fn product_node_id(schema: &str, product_key: &str) -> String {
    format!("{}product:{product_key}", schema_generation_prefix(schema))
}

pub fn document_node_id(schema: &str, doc_id: &str) -> String {
    format!("{}document:{doc_id}", schema_generation_prefix(schema))
}

pub fn section_node_id(schema: &str, section_key: &str) -> String {
    format!("{}section:{section_key}", schema_generation_prefix(schema))
}

pub fn spec_node_id(schema: &str, product_key: &str, key_slug: &str) -> String {
    format!(
        "{}spec:{product_key}:{key_slug}",
        schema_generation_prefix(schema)
    )
}

pub fn strip_schema_prefix<'a>(schema: &str, node_id: &'a str) -> &'a str {
    node_id
        .strip_prefix(&schema_generation_prefix(schema))
        .unwrap_or(node_id)
}

pub fn schema_generation_prefix(schema: &str) -> String {
    // vegapunk read APIs scope graph rows by the current generation prefix.
    // A newly created additive demo schema starts at generation 1.
    format!("{schema}:gen1:")
}

fn product_node(schema: &str, product: &ProductInput) -> GraphNode {
    GraphNode {
        id: product_node_id(schema, &product.product_key),
        node_type: "product".to_string(),
        attributes: attrs([
            ("product_key", Some(product.product_key.clone())),
            ("name_en", Some(product.name_en.clone())),
            ("name_ja", Some(product.name_ja.clone())),
            ("model", product.model.clone()),
            ("status", product.status.clone()),
            ("description_en", product.description_en.clone()),
            ("description_ja", product.description_ja.clone()),
            (
                "search_text_ja",
                Some(join_search_text([
                    Some(product.name_ja.clone()),
                    product.model.clone(),
                    product.description_ja.clone(),
                ])),
            ),
        ]),
    }
}

fn document_node(schema: &str, product: &ProductInput, doc: &DocumentInput) -> GraphNode {
    GraphNode {
        id: document_node_id(schema, &doc.doc_id),
        node_type: "document".to_string(),
        attributes: attrs([
            ("doc_id", Some(doc.doc_id.clone())),
            ("product_key", Some(product.product_key.clone())),
            ("title_en", Some(doc.title_en.clone())),
            ("title_ja", Some(doc.title_ja.clone())),
            ("version", Some(doc.version.clone())),
            ("source_url", doc.source_url.clone()),
            ("origin_lang", doc.origin_lang.clone()),
            ("search_text_ja", Some(doc.title_ja.clone())),
        ]),
    }
}

fn section_node(
    schema: &str,
    product: &ProductInput,
    doc: &DocumentInput,
    section: &SectionInput,
) -> GraphNode {
    let key = section_key(&doc.doc_id, &section.anchor);
    let en_hash = en_hash(
        &section.title_en,
        section.body_en.as_deref().unwrap_or_default(),
    );
    let status = if section
        .body_ja
        .as_deref()
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        "missing"
    } else {
        "current"
    };
    GraphNode {
        id: section_node_id(schema, &key),
        node_type: "section".to_string(),
        attributes: attrs([
            ("section_key", Some(key)),
            ("doc_id", Some(doc.doc_id.clone())),
            ("product_key", Some(product.product_key.clone())),
            ("order", Some(section.order.to_string())),
            ("level", Some(section.level.to_string())),
            ("title_en", Some(section.title_en.clone())),
            ("title_ja", Some(section.title_ja.clone())),
            ("body_en", section.body_en.clone()),
            ("body_ja", section.body_ja.clone()),
            ("en_hash", Some(en_hash.clone())),
            ("translated_from_hash", Some(en_hash)),
            ("translation_status", Some(status.to_string())),
            ("anchor", Some(section.anchor.clone())),
            (
                "search_text_ja",
                Some(join_search_text([
                    Some(section.title_ja.clone()),
                    section.body_ja.clone(),
                ])),
            ),
        ]),
    }
}

fn spec_node(schema: &str, product: &ProductInput, spec: &SpecInput) -> GraphNode {
    GraphNode {
        id: spec_node_id(schema, &product.product_key, &spec.key_slug),
        node_type: "spec".to_string(),
        attributes: attrs([
            (
                "spec_key",
                Some(format!("{}:{}", product.product_key, spec.key_slug)),
            ),
            ("product_key", Some(product.product_key.clone())),
            ("key_en", Some(spec.key_en.clone())),
            ("key_ja", Some(spec.key_ja.clone())),
            ("value_en", Some(spec.value_en.clone())),
            ("value_ja", spec.value_ja.clone()),
            (
                "search_text_ja",
                Some(join_search_text([
                    Some(spec.key_ja.clone()),
                    spec.value_ja.clone(),
                    Some(spec.value_en.clone()),
                ])),
            ),
        ]),
    }
}

fn edge(from_id: &str, to_id: &str, edge_type: &str) -> GraphEdge {
    GraphEdge {
        from_id: from_id.to_string(),
        to_id: to_id.to_string(),
        edge_type: edge_type.to_string(),
        attributes: Vec::new(),
    }
}

fn attrs<const N: usize>(values: [(&str, Option<String>); N]) -> Vec<(String, String)> {
    values
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key.to_string(), v)))
        .collect()
}

fn en_hash(title_en: &str, body_en: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title_en.as_bytes());
    hasher.update(b"\n");
    hasher.update(body_en.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn join_search_text<const N: usize>(parts: [Option<String>; N]) -> String {
    parts
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn builds_prefixed_graph_without_issues() {
        let catalog = load_catalog(Path::new("data/manual.sample.json")).unwrap();
        let graph = build_graph("sivira-cs-demo", &catalog, &HashMap::new()).unwrap();
        assert!(graph
            .nodes
            .iter()
            .all(|n| n.id.starts_with("sivira-cs-demo:")));
        assert!(graph.nodes.iter().all(|n| n.node_type != "issue"));
        assert!(graph.edges.iter().any(|e| e.edge_type == "CONTAINS"));
    }
}
