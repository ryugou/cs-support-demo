use crate::harness::rules::{
    Binding, EscalationRule, Grade, KnownResolution, ProhibitedDomain, RootCause, SourceAuthority,
};
use crate::harness::signal::{Signal, SignalSet};
use crate::ingest::schema_generation_prefix;
use crate::model::{GraphBuild, GraphEdge, GraphNode};
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::Arc;

pub fn harness_node_id(schema: &str, kind: &str, key: &str) -> String {
    format!("{}{kind}:{key}", schema_generation_prefix(schema))
}

fn csv_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn csv_signals(value: &str) -> SignalSet {
    csv_list(value).into_iter().map(Signal::new).collect()
}

/// snapshot から Signal ノードの node_id → value 索引を作る（HAS_SIGNAL 復元の共通部品）。
fn signal_value_index(
    snapshot: &crate::proto::graphrag::GetGraphSnapshotResponse,
) -> HashMap<String, String> {
    snapshot
        .nodes
        .iter()
        .filter(|n| n.node_type == "Signal")
        .filter_map(|n| {
            n.attributes
                .get("value")
                .map(|v| (n.node_id.clone(), v.clone()))
        })
        .collect()
}

fn parse_binding(value: Option<&String>) -> Binding {
    match value.map(String::as_str) {
        Some("mandatory") => Binding::Mandatory,
        _ => Binding::Advisory,
    }
}

pub fn escalation_rule_from_attributes(attrs: &HashMap<String, String>) -> Result<EscalationRule> {
    Ok(EscalationRule {
        id: attrs
            .get("rule_id")
            .cloned()
            .ok_or_else(|| anyhow!("escalation_rule missing rule_id"))?,
        condition: csv_signals(attrs.get("condition").map(String::as_str).unwrap_or("")),
        route: attrs
            .get("route")
            .cloned()
            .ok_or_else(|| anyhow!("escalation_rule missing route"))?,
        owner: attrs.get("owner").cloned().filter(|v| !v.is_empty()),
        binding: parse_binding(attrs.get("binding")),
    })
}

pub fn prohibited_domain_from_attributes(
    attrs: &HashMap<String, String>,
) -> Result<ProhibitedDomain> {
    Ok(ProhibitedDomain {
        id: attrs
            .get("domain_id")
            .cloned()
            .ok_or_else(|| anyhow!("prohibited_domain missing domain_id"))?,
        domain_signals: csv_signals(
            attrs
                .get("domain_signals")
                .map(String::as_str)
                .unwrap_or(""),
        ),
        text_patterns: csv_list(attrs.get("pattern").map(String::as_str).unwrap_or("")),
        route: attrs
            .get("route")
            .cloned()
            .ok_or_else(|| anyhow!("prohibited_domain missing route"))?,
        binding: parse_binding(attrs.get("binding")),
    })
}

/// 過去事例の論理ビュー（search_past_cases 用）。
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct PastCase {
    pub case_id: String,
    pub question: String,
    pub product_key: String,
    pub actor: String,
    pub created_at: String,
}

/// 担当者が追加する新ルール（add_known_resolution / correction_intake の出口）。
#[derive(Debug, Clone)]
pub struct NewKnownResolution {
    pub signal_set: SignalSet,
    pub applicability: String,
    pub answer: String,
    pub origin: String,
    pub created_by: String,
    pub rationale_section_keys: Vec<String>,
}

/// KR 1 件をグラフ表現（KR ノード + Signal ノード + HAS_SIGNAL / BECAUSE 辺）に組み立てる。
/// signal_set を JSON 属性に畳まない（I2）。予約フィールドは空で持たせる（S1-3）。
pub fn build_known_resolution_graph(
    schema: &str,
    kr_id: &str,
    kr: &NewKnownResolution,
) -> GraphBuild {
    let kr_node_id = harness_node_id(schema, "KnownResolution", kr_id);
    let mut nodes = vec![GraphNode {
        id: kr_node_id.clone(),
        node_type: "KnownResolution".to_string(),
        attributes: vec![
            ("kr_id".to_string(), kr_id.to_string()),
            ("answer_text".to_string(), kr.answer.clone()),
            ("applicability".to_string(), kr.applicability.clone()),
            (
                "grade".to_string(),
                Grade::ApprovalRequired.as_str().to_string(),
            ),
            ("status".to_string(), "active".to_string()),
            ("source_authority".to_string(), "authoritative".to_string()),
            ("root_cause".to_string(), "knowledge_error".to_string()),
            ("approval_count".to_string(), "0".to_string()),
            ("rejection_count".to_string(), "0".to_string()),
            ("approver_set".to_string(), String::new()),
            ("origin".to_string(), kr.origin.clone()),
            ("created_by".to_string(), kr.created_by.clone()),
            ("verified_at".to_string(), chrono::Utc::now().to_rfc3339()),
            // --- 予約（空で存在させる。S1-8 条件 6）---
            ("error_axis".to_string(), String::new()),
            ("owner".to_string(), String::new()),
            ("binding".to_string(), "advisory".to_string()),
            ("direction".to_string(), String::new()),
            ("route".to_string(), String::new()),
            (
                "registration_trigger".to_string(),
                "single_ruling".to_string(),
            ),
            ("knowledge_class".to_string(), "commercial".to_string()),
            ("outcome_ref".to_string(), String::new()),
            ("search_text_ja".to_string(), kr.answer.clone()),
        ],
    }];
    let mut edges = Vec::new();
    for signal in &kr.signal_set {
        let signal_node_id = harness_node_id(schema, "Signal", signal.as_str());
        nodes.push(GraphNode {
            id: signal_node_id.clone(),
            node_type: "Signal".to_string(),
            attributes: vec![("value".to_string(), signal.as_str().to_string())],
        });
        edges.push(GraphEdge {
            from_id: kr_node_id.clone(),
            to_id: signal_node_id,
            edge_type: "HAS_SIGNAL".to_string(),
            attributes: Vec::new(),
        });
    }
    for section_key in &kr.rationale_section_keys {
        edges.push(GraphEdge {
            from_id: kr_node_id.clone(),
            to_id: crate::ingest::section_node_id(schema, section_key),
            edge_type: "BECAUSE".to_string(),
            attributes: Vec::new(),
        });
    }
    GraphBuild { nodes, edges }
}

/// PunkRecord（vegapunk）を材料ストアとして読み書きする層。判定は載せない（I4）。
pub struct KnowledgeStore {
    client: Arc<VegapunkClient>,
}

impl KnowledgeStore {
    pub fn new(client: Arc<VegapunkClient>) -> Self {
        Self { client }
    }

    pub async fn load_escalation_rules(&self, schema: &str) -> Result<Vec<EscalationRule>> {
        self.client
            .query_nodes(schema, "EscalationRule", Vec::new(), 1000)
            .await
            .context("load escalation rules")?
            .into_iter()
            .map(|node| escalation_rule_from_attributes(&node.attributes))
            .collect()
    }

    pub async fn load_prohibited_domains(&self, schema: &str) -> Result<Vec<ProhibitedDomain>> {
        self.client
            .query_nodes(schema, "ProhibitedDomain", Vec::new(), 1000)
            .await
            .context("load prohibited domains")?
            .into_iter()
            .map(|node| prohibited_domain_from_attributes(&node.attributes))
            .collect()
    }

    /// KnownResolution を Signal ノード経由で復元する（HAS_SIGNAL 辺の走査）。
    pub async fn load_known_resolutions(&self, schema: &str) -> Result<Vec<KnownResolution>> {
        let kr_nodes = self
            .client
            .query_nodes(schema, "KnownResolution", Vec::new(), 1000)
            .await
            .context("load known resolutions")?;
        if kr_nodes.is_empty() {
            return Ok(Vec::new());
        }
        let snapshot = self.client.graph_snapshot(schema, 5000).await?;
        let signal_values = signal_value_index(&snapshot);
        // KR node_id -> SignalSet
        let mut kr_signals: HashMap<String, SignalSet> = HashMap::new();
        for edge in snapshot
            .edges
            .iter()
            .filter(|e| e.edge_type == "HAS_SIGNAL")
        {
            if let Some(value) = signal_values.get(&edge.to_id) {
                kr_signals
                    .entry(edge.from_id.clone())
                    .or_default()
                    .insert(Signal::new(value));
            }
        }
        kr_nodes
            .into_iter()
            .map(|node| {
                let attrs = &node.attributes;
                let get = |key: &str| attrs.get(key).cloned().unwrap_or_default();
                Ok(KnownResolution {
                    id: attrs
                        .get("kr_id")
                        .cloned()
                        .ok_or_else(|| anyhow!("KnownResolution missing kr_id"))?,
                    signal_set: kr_signals.remove(&node.node_id).unwrap_or_default(),
                    applicability: get("applicability"),
                    answer: get("answer_text"),
                    source_authority: match get("source_authority").as_str() {
                        "non_authoritative" => SourceAuthority::NonAuthoritative,
                        _ => SourceAuthority::Authoritative,
                    },
                    root_cause: match get("root_cause").as_str() {
                        "retrieval_miss" => RootCause::RetrievalMiss,
                        _ => RootCause::KnowledgeError,
                    },
                    grade: Grade::parse_label(&get("grade")),
                    approval_count: get("approval_count").parse().unwrap_or(0),
                    rejection_count: get("rejection_count").parse().unwrap_or(0),
                    approver_set: csv_list(&get("approver_set")),
                    origin: get("origin"),
                    binding: parse_binding(attrs.get("binding")),
                    registration_trigger: get("registration_trigger"),
                    knowledge_class: get("knowledge_class"),
                    outcome_ref: csv_list(&get("outcome_ref")),
                })
            })
            .collect()
    }

    pub async fn insert_known_resolution(
        &self,
        schema: &str,
        kr: &NewKnownResolution,
    ) -> Result<String> {
        let kr_id = format!("kr-{}", uuid::Uuid::new_v4());
        let build = build_known_resolution_graph(schema, &kr_id, kr);
        self.client.upsert_graph_low_level(build).await?;
        Ok(kr_id)
    }

    /// support 系 record（support_case / answer_attempt など）を 1 ノードとして書く。
    pub async fn record(
        &self,
        schema: &str,
        node_type: &str,
        key: &str,
        attributes: Vec<(String, String)>,
    ) -> Result<()> {
        let node = GraphNode {
            id: harness_node_id(schema, node_type, key),
            node_type: node_type.to_string(),
            attributes,
        };
        self.client.upsert_nodes(vec![node]).await?;
        Ok(())
    }

    /// 会話層: support_case の累積 signal 集合を HAS_SIGNAL 辺から復元する（S1-11 追記 3）。
    pub async fn load_case_signals(&self, schema: &str, case_id: &str) -> Result<SignalSet> {
        let case_node_id = harness_node_id(schema, "support_case", case_id);
        let snapshot = self.client.graph_snapshot(schema, 5000).await?;
        let signal_values = signal_value_index(&snapshot);
        Ok(snapshot
            .edges
            .iter()
            .filter(|e| e.edge_type == "HAS_SIGNAL" && e.from_id == case_node_id)
            .filter_map(|e| signal_values.get(&e.to_id).map(Signal::new))
            .collect())
    }

    /// 会話層: 今ターンの signal を support_case に加算する（Signal ノード + HAS_SIGNAL 辺 upsert）。
    pub async fn append_case_signals(
        &self,
        schema: &str,
        case_id: &str,
        signals: &SignalSet,
    ) -> Result<()> {
        if signals.is_empty() {
            return Ok(());
        }
        let case_node_id = harness_node_id(schema, "support_case", case_id);
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for signal in signals {
            let signal_node_id = harness_node_id(schema, "Signal", signal.as_str());
            nodes.push(GraphNode {
                id: signal_node_id.clone(),
                node_type: "Signal".to_string(),
                attributes: vec![("value".to_string(), signal.as_str().to_string())],
            });
            edges.push(GraphEdge {
                from_id: case_node_id.clone(),
                to_id: signal_node_id,
                edge_type: "HAS_SIGNAL".to_string(),
                attributes: Vec::new(),
            });
        }
        self.client
            .upsert_graph_low_level(GraphBuild { nodes, edges })
            .await?;
        Ok(())
    }

    /// 過去事例（support_case）を読み出す。scope は schema 引数で強制済み。
    pub async fn load_cases(&self, schema: &str, limit: i32) -> Result<Vec<PastCase>> {
        Ok(self
            .client
            .query_nodes(schema, "support_case", Vec::new(), limit)
            .await
            .context("load support cases")?
            .into_iter()
            .filter_map(|node| {
                let attrs = node.attributes;
                Some(PastCase {
                    case_id: attrs.get("case_id")?.clone(),
                    question: attrs.get("question").cloned().unwrap_or_default(),
                    product_key: attrs.get("product_key").cloned().unwrap_or_default(),
                    actor: attrs.get("actor").cloned().unwrap_or_default(),
                    created_at: attrs.get("created_at").cloned().unwrap_or_default(),
                })
            })
            .collect())
    }

    /// 過去事例を日本語クエリで検索する（scoring は mcp.rs の共有関数を再利用）。
    pub async fn search_cases(
        &self,
        schema: &str,
        query_ja: &str,
        top_k: usize,
    ) -> Result<Vec<(PastCase, f32)>> {
        let query_norm = crate::resolve::normalize_key(query_ja);
        let mut hits: Vec<(PastCase, f32)> = self
            .load_cases(schema, 500)
            .await?
            .into_iter()
            .filter_map(|case| {
                let score = crate::mcp::section_score(&query_norm, query_ja, &case.question);
                (score > 0.3).then_some((case, score))
            })
            .collect();
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k.max(1));
        Ok(hits)
    }

    /// grade 運用: 承認/却下カウントと格付けを KnownResolution ノードに反映する（遵守事項 3）。
    pub async fn update_known_resolution_grade(
        &self,
        schema: &str,
        kr_id: &str,
        approval_count: u32,
        rejection_count: u32,
        approver_set: &[String],
        grade: Grade,
    ) -> Result<()> {
        // upsert merge を前提に該当属性のみ送る。全属性置換だった場合は Task 14 の
        // 実機検証で判明するため、そのときは query_nodes で現属性を読み全属性を再送する。
        let node = GraphNode {
            id: harness_node_id(schema, "KnownResolution", kr_id),
            node_type: "KnownResolution".to_string(),
            attributes: vec![
                ("kr_id".to_string(), kr_id.to_string()),
                ("approval_count".to_string(), approval_count.to_string()),
                ("rejection_count".to_string(), rejection_count.to_string()),
                ("approver_set".to_string(), approver_set.join(",")),
                ("grade".to_string(), grade.as_str().to_string()),
            ],
        };
        self.client.upsert_nodes(vec![node]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::signal::Signal;
    use std::collections::HashMap;

    fn attrs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn escalation_rule_from_attributes_parses_condition_csv() {
        let rule = escalation_rule_from_attributes(&attrs(&[
            ("rule_id", "r1"),
            ("condition", "skin_irritation,continue_use_question"),
            ("route", "dermatology_liaison"),
            ("binding", "mandatory"),
        ]))
        .expect("parses");
        assert_eq!(rule.id, "r1");
        assert!(rule.condition.contains(&Signal::new("skin_irritation")));
        assert!(rule
            .condition
            .contains(&Signal::new("continue_use_question")));
        assert_eq!(rule.route, "dermatology_liaison");
        assert_eq!(rule.binding, Binding::Mandatory);
    }

    #[test]
    fn escalation_rule_missing_route_is_error() {
        assert!(escalation_rule_from_attributes(&attrs(&[
            ("rule_id", "r1"),
            ("condition", "mold")
        ]))
        .is_err());
    }

    #[test]
    fn prohibited_domain_from_attributes_parses() {
        let domain = prohibited_domain_from_attributes(&attrs(&[
            ("domain_id", "d1"),
            ("domain_signals", "post_ingestion_symptom"),
            ("pattern", "飲み合わせ,持病があって"),
            ("route", "medical_escalation_desk"),
            ("binding", "mandatory"),
        ]))
        .expect("parses");
        assert_eq!(
            domain.text_patterns,
            vec!["飲み合わせ".to_string(), "持病があって".to_string()]
        );
    }

    #[test]
    fn known_resolution_node_build_uses_signal_nodes_not_json_attr() {
        // I2 / アンチパターン 3: signal_set が KR ノード属性に存在しないこと
        let new_kr = NewKnownResolution {
            signal_set: [Signal::new("discoloration"), Signal::new("mold")]
                .into_iter()
                .collect(),
            applicability: "全ロット".to_string(),
            answer: "廃棄してください".to_string(),
            origin: "escalation:esc-1".to_string(),
            created_by: "sup-001".to_string(),
            rationale_section_keys: vec!["doc-1#storage".to_string()],
        };
        let build = build_known_resolution_graph("sivira-cs-demo", "kr-test", &new_kr);
        let kr_node = build
            .nodes
            .iter()
            .find(|n| n.node_type == "KnownResolution")
            .expect("kr node");
        assert!(kr_node.attributes.iter().all(|(k, _)| k != "signal_set"));
        // Signal ノード 2 個 + HAS_SIGNAL 辺 2 本 + BECAUSE 辺 1 本
        assert_eq!(
            build
                .nodes
                .iter()
                .filter(|n| n.node_type == "Signal")
                .count(),
            2
        );
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "HAS_SIGNAL")
                .count(),
            2
        );
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "BECAUSE")
                .count(),
            1
        );
        // 予約フィールドが空でも存在する（S1-8 条件 6）
        for key in [
            "binding",
            "registration_trigger",
            "knowledge_class",
            "outcome_ref",
        ] {
            assert!(
                kr_node.attributes.iter().any(|(k, _)| k == key),
                "missing reserved {key}"
            );
        }
    }
}
