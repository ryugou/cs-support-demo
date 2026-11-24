use super::fake_vegapunk::GraphStore;
use super::PROJECT_ID;
use crate::harness::knowledge::harness_node_id;
use crate::manual::ingest_model::{
    build_document_node, build_product_node, build_section_graph, content_hash, ManualProductInput,
    ManualSectionInput,
};
use crate::manual::schema_ids::manual_node_id;
use crate::model::{GraphBuild, GraphNode};

const F_P7_MODELS: [&str; 7] = [
    "ADC-V523",
    "ADC-V523X",
    "ADC-V724",
    "ADC-V724X",
    "ADC-VC729P",
    "ADC-VC727P",
    "ADC-VC827P",
];
const MANUAL_DOC_KEY: &str = "reply-test-harness-doc";

#[derive(serde::Deserialize)]
struct BundledRules {
    escalation_rules: Vec<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    prohibited_domains: Vec<serde_json::Map<String, serde_json::Value>>,
}

pub(crate) struct Fixture {
    schema: String,
    graph: GraphBuild,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self::for_schema(PROJECT_ID)
    }

    pub(crate) fn schema(&self) -> &str {
        &self.schema
    }

    pub(crate) fn for_schema(schema: &str) -> Self {
        Self {
            schema: schema.to_string(),
            graph: GraphBuild {
                nodes: Vec::new(),
                edges: Vec::new(),
            },
        }
    }

    pub(crate) fn product(mut self, model: &str, name: &str, aliases: &[&str]) -> Self {
        self.graph.nodes.push(build_product_node(
            &self.schema,
            &ManualProductInput {
                model: model.to_string(),
                name: name.to_string(),
                aliases: aliases.iter().map(ToString::to_string).collect(),
            },
        ));
        self
    }

    pub(crate) fn products_f_p7(mut self) -> Self {
        for model in F_P7_MODELS {
            self = self.product(model, model, &[]);
        }
        self
    }

    pub(crate) fn products_f_name(mut self) -> Self {
        self = self.product("ADC-V724", "みまもりドームカメラ", &["見守りドーム"]);
        self = self.product("ADC-VC729P", "おでかけパノラマカメラ", &[]);
        for model in F_P7_MODELS
            .into_iter()
            .filter(|model| !["ADC-V724", "ADC-VC729P"].contains(model))
        {
            self = self.product(model, model, &[]);
        }
        self
    }

    pub(crate) fn node(mut self, kind: &str, key: &str, attributes: &[(&str, &str)]) -> Self {
        self.graph.nodes.push(GraphNode {
            id: manual_node_id(&self.schema, kind, key),
            node_type: kind.to_string(),
            attributes: attributes
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
        self
    }

    pub(crate) fn manual_section(
        mut self,
        slug: &str,
        title: &str,
        body: &str,
        describes_models: &[&str],
        signals: &[&str],
    ) -> Self {
        let doc = build_document_node(
            &self.schema,
            MANUAL_DOC_KEY,
            "テスト用マニュアル",
            "https://example.invalid/manual",
            "2026-01-01T00:00:00Z",
        );
        if !self.graph.nodes.iter().any(|n| n.id == doc.id) {
            self.graph.nodes.push(doc);
        }
        let section = ManualSectionInput {
            slug: slug.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            source_url: format!("https://example.invalid/manual/{slug}"),
            breadcrumb: title.to_string(),
            section_no: None,
            order: 0,
            parent_slug: None,
            product_models: describes_models.iter().map(ToString::to_string).collect(),
            signal_values: signals.iter().map(ToString::to_string).collect(),
            body_original: None,
            original_hash: None,
            concept_keys: Vec::new(),
        };
        let build =
            build_section_graph(&self.schema, MANUAL_DOC_KEY, &section, &content_hash(body));
        self.graph.nodes.extend(build.nodes);
        self.graph.edges.extend(build.edges);
        self
    }

    pub(crate) fn bundled_rules(mut self) -> Self {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/urtect/rules.json");
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read bundled rules {path:?}: {e}"));
        let rules: BundledRules = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("parse bundled rules {path:?}: {e}"));
        for rule in &rules.escalation_rules {
            let key = rule["rule_id"].as_str().expect("rule_id").to_string();
            self.graph.nodes.push(GraphNode {
                id: harness_node_id(&self.schema, "EscalationRule", &key),
                node_type: "EscalationRule".to_string(),
                attributes: stored_rule_attributes(rule),
            });
        }
        for domain in &rules.prohibited_domains {
            let key = domain["domain_id"].as_str().expect("domain_id").to_string();
            self.graph.nodes.push(GraphNode {
                id: harness_node_id(&self.schema, "ProhibitedDomain", &key),
                node_type: "ProhibitedDomain".to_string(),
                attributes: stored_rule_attributes(domain),
            });
        }
        self
    }

    pub(crate) fn seed(&self, store: &GraphStore) {
        for node in &self.graph.nodes {
            store.upsert_node(&node.id, &node.node_type, node.attributes.clone());
        }
        for edge in &self.graph.edges {
            store.upsert_edge(&edge.from_id, &edge.to_id, &edge.edge_type);
        }
    }
}

fn stored_rule_attributes(
    entry: &serde_json::Map<String, serde_json::Value>,
) -> Vec<(String, String)> {
    entry
        .iter()
        .map(|(key, value)| {
            let text = match value {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(items) => items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                other => other.to_string(),
            };
            (key.clone(), text)
        })
        .collect()
}
