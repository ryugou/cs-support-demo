use crate::{
    ingest::{product_node_id, section_node_id, spec_node_id},
    model::{GraphNode as WriteGraphNode, ProductCandidate, ProductView, SectionHit, SectionView},
    proto::graphrag::{GraphEdge, GraphNode, NodeResult},
    resolve::{fuzzy_score, normalize_key},
    vegapunk::VegapunkClient,
};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{cmp::Ordering, collections::HashMap, sync::Arc};

#[derive(Clone)]
pub struct ToolService {
    client: Arc<VegapunkClient>,
}

impl ToolService {
    pub fn new(client: VegapunkClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    pub async fn resolve_product(&self, schema: &str, text: &str) -> Result<Vec<ProductCandidate>> {
        let products = self
            .client
            .query_nodes(schema, "product", Vec::new(), 1000)
            .await?;
        let mut candidates = products
            .into_iter()
            .filter_map(|node| candidate_from_node(text, node))
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        candidates.truncate(5);
        Ok(candidates)
    }

    pub async fn search_manual(
        &self,
        schema: &str,
        query_ja: &str,
        product_key: Option<&str>,
        top_k: i32,
    ) -> Result<Vec<SectionHit>> {
        let filters = product_key
            .map(|key| vec![("product_key", "eq", key)])
            .unwrap_or_default();
        let sections = self
            .client
            .query_nodes(schema, "section", filters, 1000)
            .await?;
        let snapshot = self.client.graph_snapshot(schema, 5000).await?;
        let graph = SnapshotIndex::new(snapshot.nodes, snapshot.edges);
        let query_norm = normalize_key(query_ja);

        let mut hits = sections
            .into_iter()
            .filter_map(|node| {
                let attrs = node.attributes;
                let text = format!(
                    "{}\n{}",
                    attrs.get("title_ja").cloned().unwrap_or_default(),
                    attrs.get("body_ja").cloned().unwrap_or_default()
                );
                let score = section_score(&query_norm, query_ja, &text);
                (score > 0.0).then(|| SectionHit {
                    section_key: attrs.get("section_key").cloned().unwrap_or_default(),
                    title_ja: attrs.get("title_ja").cloned().unwrap_or_default(),
                    body_ja: attrs.get("body_ja").cloned(),
                    body_en: attrs.get("body_en").cloned(),
                    translation_status: attrs.get("translation_status").cloned(),
                    breadcrumb: graph.breadcrumb(
                        schema,
                        attrs
                            .get("section_key")
                            .map(String::as_str)
                            .unwrap_or_default(),
                    ),
                    score,
                })
            })
            .collect::<Vec<_>>();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        hits.truncate(top_k.max(1) as usize);
        Ok(hits)
    }

    pub async fn get_section(&self, schema: &str, section_key: &str) -> Result<SectionView> {
        let snapshot = self.client.graph_snapshot(schema, 5000).await?;
        let graph = SnapshotIndex::new(snapshot.nodes, snapshot.edges);
        let node_id = section_node_id(schema, section_key);
        let section = graph
            .node_json(&node_id)
            .ok_or_else(|| anyhow!("section not found: {section_key}"))?;
        Ok(SectionView {
            section,
            ancestors: graph.ancestor_json(&node_id, 2),
            children: graph.children_json(&node_id, 2),
            references: graph.outgoing_json(&node_id, "REFERENCES"),
        })
    }

    pub async fn get_product(&self, schema: &str, product_key: &str) -> Result<ProductView> {
        let products = self
            .client
            .query_nodes(
                schema,
                "product",
                vec![("product_key", "eq", product_key)],
                10,
            )
            .await?;
        let product = products
            .first()
            .map(node_result_json)
            .ok_or_else(|| anyhow!("product not found: {product_key}"))?;
        let specs = self
            .client
            .query_nodes(
                schema,
                "spec",
                vec![("product_key", "eq", product_key)],
                100,
            )
            .await?
            .iter()
            .map(node_result_json)
            .collect();
        let snapshot = self.client.graph_snapshot(schema, 5000).await?;
        let graph = SnapshotIndex::new(snapshot.nodes, snapshot.edges);
        let toc = graph.children_json(&product_node_id(schema, product_key), 3);
        Ok(ProductView {
            product,
            specs,
            toc,
        })
    }

    pub async fn call_tool(&self, schema: &str, name: &str, arguments: Value) -> Result<Value> {
        match name {
            "resolve_product" => {
                let text = require_str(&arguments, "text")?;
                Ok(json!(self.resolve_product(schema, text).await?))
            }
            "search_manual" => {
                let query = require_str(&arguments, "query_ja")?;
                let product_key = arguments.get("product_key").and_then(Value::as_str);
                let top_k = arguments.get("top_k").and_then(Value::as_i64).unwrap_or(5) as i32;
                Ok(json!(
                    self.search_manual(schema, query, product_key, top_k)
                        .await?
                ))
            }
            "get_section" => {
                let section_key = require_str(&arguments, "section_key")?;
                Ok(json!(self.get_section(schema, section_key).await?))
            }
            "get_product" => {
                let product_key = require_str(&arguments, "product_key")?;
                Ok(json!(self.get_product(schema, product_key).await?))
            }
            "upsert_product" => Ok(json!(self.upsert_product(schema, &arguments).await?)),
            "upsert_section" => Ok(json!(self.upsert_section(schema, &arguments).await?)),
            "upsert_spec" => Ok(json!(self.upsert_spec(schema, &arguments).await?)),
            other => Err(anyhow!("unknown tool: {other}")),
        }
    }

    pub async fn upsert_product(&self, schema: &str, arguments: &Value) -> Result<Value> {
        let product_key = require_str(arguments, "product_key")?;
        let node = WriteGraphNode {
            id: product_node_id(schema, product_key),
            node_type: "product".to_string(),
            attributes: attrs_from_json(
                arguments,
                &[
                    "product_key",
                    "name_en",
                    "name_ja",
                    "model",
                    "status",
                    "description_en",
                    "description_ja",
                ],
            ),
        };
        let upserted = self.client.upsert_nodes(vec![node]).await?;
        Ok(json!({ "upserted_nodes": upserted }))
    }

    pub async fn upsert_section(&self, schema: &str, arguments: &Value) -> Result<Value> {
        let section_key = require_str(arguments, "section_key")?;
        let title_en = require_str(arguments, "title_en")?;
        let body_en = arguments
            .get("body_en")
            .and_then(Value::as_str)
            .unwrap_or("");
        let en_hash = hash_en(title_en, body_en);
        let mut attributes = attrs_from_json(
            arguments,
            &[
                "section_key",
                "doc_id",
                "product_key",
                "order",
                "level",
                "title_en",
                "title_ja",
                "body_en",
                "body_ja",
                "translation_status",
                "anchor",
            ],
        );
        attributes.push(("en_hash".to_string(), en_hash.clone()));
        attributes.push(("translated_from_hash".to_string(), en_hash));
        let node = WriteGraphNode {
            id: section_node_id(schema, section_key),
            node_type: "section".to_string(),
            attributes,
        };
        let upserted = self.client.upsert_nodes(vec![node]).await?;
        Ok(json!({ "upserted_nodes": upserted }))
    }

    pub async fn upsert_spec(&self, schema: &str, arguments: &Value) -> Result<Value> {
        let product_key = require_str(arguments, "product_key")?;
        let key_slug = require_str(arguments, "key_slug")?;
        let mut attributes = attrs_from_json(
            arguments,
            &["product_key", "key_en", "key_ja", "value_en", "value_ja"],
        );
        attributes.push(("spec_key".to_string(), format!("{product_key}:{key_slug}")));
        let node = WriteGraphNode {
            id: spec_node_id(schema, product_key, key_slug),
            node_type: "spec".to_string(),
            attributes,
        };
        let upserted = self.client.upsert_nodes(vec![node]).await?;
        Ok(json!({ "upserted_nodes": upserted }))
    }
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

pub fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "resolve_product",
                "description": "Resolve a Japanese customer expression, English product name, or model number to product candidates. Does not use aliases.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }
            },
            {
                "name": "search_manual",
                "description": "Search Japanese manual text. Pass query_ja in Japanese. Returns section hits with breadcrumb and English fallback body.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query_ja": { "type": "string" },
                        "product_key": { "type": "string" },
                        "top_k": { "type": "integer" }
                    },
                    "required": ["query_ja"]
                }
            },
            {
                "name": "get_section",
                "description": "Get a section plus ancestors, children up to 2 hops, and REFERENCES targets.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "section_key": { "type": "string" } },
                    "required": ["section_key"]
                }
            },
            {
                "name": "get_product",
                "description": "Get product overview, specs, and top-level document table of contents.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "product_key": { "type": "string" } },
                    "required": ["product_key"]
                }
            },
            {
                "name": "upsert_product",
                "description": "Merge a product by stable product_key. Hard delete is not exposed; use status for logical deletion.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "product_key": { "type": "string" },
                        "name_en": { "type": "string" },
                        "name_ja": { "type": "string" },
                        "model": { "type": "string" },
                        "status": { "type": "string" },
                        "description_en": { "type": "string" },
                        "description_ja": { "type": "string" }
                    },
                    "required": ["product_key", "name_en", "name_ja"]
                }
            },
            {
                "name": "upsert_section",
                "description": "Merge a section by stable section_key. English body changes recompute en_hash and translated_from_hash for demo fixture updates.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "section_key": { "type": "string" },
                        "doc_id": { "type": "string" },
                        "product_key": { "type": "string" },
                        "order": { "type": "integer" },
                        "level": { "type": "integer" },
                        "title_en": { "type": "string" },
                        "title_ja": { "type": "string" },
                        "body_en": { "type": "string" },
                        "body_ja": { "type": "string" },
                        "translation_status": { "type": "string" },
                        "anchor": { "type": "string" }
                    },
                    "required": ["section_key", "doc_id", "product_key", "order", "level", "title_en", "title_ja"]
                }
            },
            {
                "name": "upsert_spec",
                "description": "Merge a structured spec by stable product_key + key_slug.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "product_key": { "type": "string" },
                        "key_slug": { "type": "string" },
                        "key_en": { "type": "string" },
                        "key_ja": { "type": "string" },
                        "value_en": { "type": "string" },
                        "value_ja": { "type": "string" }
                    },
                    "required": ["product_key", "key_slug", "key_en", "key_ja", "value_en"]
                }
            }
        ]
    })
}

pub fn success(id: Option<Value>, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

pub fn failure(id: Option<Value>, code: i32, message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
        }),
    }
}

fn require_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string argument: {key}"))
}

fn attrs_from_json(value: &Value, keys: &[&str]) -> Vec<(String, String)> {
    keys.iter()
        .filter_map(|key| {
            value.get(*key).and_then(|value| match value {
                Value::String(s) => Some(((*key).to_string(), s.clone())),
                Value::Number(n) => Some(((*key).to_string(), n.to_string())),
                _ => None,
            })
        })
        .collect()
}

fn hash_en(title_en: &str, body_en: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title_en.as_bytes());
    hasher.update(b"\n");
    hasher.update(body_en.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn candidate_from_node(query: &str, node: NodeResult) -> Option<ProductCandidate> {
    let attrs = node.attributes;
    let product_key = attrs.get("product_key")?.clone();
    let name_ja = attrs.get("name_ja").cloned().unwrap_or_default();
    let name_en = attrs.get("name_en").cloned().unwrap_or_default();
    let model = attrs.get("model").cloned();
    let description = attrs.get("description_ja").cloned().unwrap_or_default();
    let score = [
        fuzzy_score(query, &product_key),
        fuzzy_score(query, &name_ja),
        fuzzy_score(query, &name_en),
        model
            .as_deref()
            .map(|m| fuzzy_score(query, m))
            .unwrap_or(0.0),
        semantic_overlap_score(query, &description),
    ]
    .into_iter()
    .fold(0.0, f32::max);
    (score > 0.1).then(|| ProductCandidate {
        product_key,
        name_ja,
        name_en,
        model,
        score,
        reason: if score >= 1.0 {
            "normalized_match".to_string()
        } else {
            "semantic_nearby".to_string()
        },
    })
}

fn semantic_overlap_score(query: &str, text: &str) -> f32 {
    let q = normalize_key(query);
    let t = normalize_key(text);
    if q.is_empty() || t.is_empty() {
        return 0.0;
    }
    let matched = q.chars().filter(|ch| t.contains(*ch)).count();
    matched as f32 / q.chars().count().max(1) as f32 * 0.6
}

fn section_score(query_norm: &str, query_raw: &str, text: &str) -> f32 {
    if query_raw.trim().is_empty() {
        return 0.0;
    }
    if text.contains(query_raw) {
        return 1.0;
    }
    let text_norm = normalize_key(text);
    if !query_norm.is_empty() && text_norm.contains(query_norm) {
        return 0.95;
    }
    semantic_overlap_score(query_raw, text)
}

fn node_result_json(node: &NodeResult) -> Value {
    json!({
        "node_id": node.node_id,
        "node_type": node.node_type,
        "attributes": node.attributes,
    })
}

struct SnapshotIndex {
    nodes: HashMap<String, GraphNode>,
    incoming: HashMap<String, Vec<GraphEdge>>,
    outgoing: HashMap<String, Vec<GraphEdge>>,
}

impl SnapshotIndex {
    fn new(nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) -> Self {
        let mut incoming: HashMap<String, Vec<GraphEdge>> = HashMap::new();
        let mut outgoing: HashMap<String, Vec<GraphEdge>> = HashMap::new();
        for edge in edges {
            outgoing
                .entry(edge.from_id.clone())
                .or_default()
                .push(edge.clone());
            incoming.entry(edge.to_id.clone()).or_default().push(edge);
        }
        Self {
            nodes: nodes
                .into_iter()
                .map(|node| (node.node_id.clone(), node))
                .collect(),
            incoming,
            outgoing,
        }
    }

    fn node_json(&self, node_id: &str) -> Option<Value> {
        self.nodes.get(node_id).map(|node| {
            json!({
                "node_id": node.node_id,
                "node_type": node.node_type,
                "display_text": node.display_text,
                "attributes": node.attributes,
            })
        })
    }

    fn breadcrumb(&self, schema: &str, section_key: &str) -> Vec<String> {
        let mut current = section_node_id(schema, section_key);
        let mut parts = Vec::new();
        for _ in 0..8 {
            if let Some(node) = self.nodes.get(&current) {
                if let Some(title) = node.attributes.get("title_ja") {
                    parts.push(title.clone());
                } else if let Some(title) = node.attributes.get("title_en") {
                    parts.push(title.clone());
                }
            }
            let Some(parent_edge) = self
                .incoming
                .get(&current)
                .and_then(|edges| edges.iter().find(|edge| edge.edge_type == "CONTAINS"))
            else {
                break;
            };
            current = parent_edge.from_id.clone();
        }
        parts.reverse();
        parts
    }

    fn ancestor_json(&self, node_id: &str, max_hops: usize) -> Vec<Value> {
        let mut current = node_id.to_string();
        let mut result = Vec::new();
        for _ in 0..max_hops {
            let Some(parent_edge) = self
                .incoming
                .get(&current)
                .and_then(|edges| edges.iter().find(|edge| edge.edge_type == "CONTAINS"))
            else {
                break;
            };
            current = parent_edge.from_id.clone();
            if let Some(node) = self.node_json(&current) {
                result.push(node);
            }
        }
        result
    }

    fn children_json(&self, node_id: &str, max_hops: usize) -> Vec<Value> {
        let mut result = Vec::new();
        self.collect_children(node_id, max_hops, &mut result);
        result
    }

    fn collect_children(&self, node_id: &str, remaining: usize, result: &mut Vec<Value>) {
        if remaining == 0 {
            return;
        }
        for edge in self
            .outgoing
            .get(node_id)
            .into_iter()
            .flatten()
            .filter(|edge| edge.edge_type == "CONTAINS" || edge.edge_type == "HAS_DOCUMENT")
        {
            if let Some(node) = self.node_json(&edge.to_id) {
                result.push(node);
            }
            self.collect_children(&edge.to_id, remaining - 1, result);
        }
    }

    fn outgoing_json(&self, node_id: &str, edge_type: &str) -> Vec<Value> {
        self.outgoing
            .get(node_id)
            .into_iter()
            .flatten()
            .filter(|edge| edge.edge_type == edge_type)
            .filter_map(|edge| self.node_json(&edge.to_id))
            .collect()
    }
}
