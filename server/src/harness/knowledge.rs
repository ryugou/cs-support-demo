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

pub(crate) fn csv_list(value: &str) -> Vec<String> {
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

/// graph_snapshot に渡す上限。到達＝切り詰めの可能性があり、HAS_SIGNAL 辺の欠落は
/// 照合の誤判定（fail open）につながるため、到達時はエラーにする（fail closed）。
// TODO: bind to vegapunk traversal API — snapshot 全取得でなく
// KnownResolution/support_case -> HAS_SIGNAL -> Signal の隣接取得に置き換える。
const SNAPSHOT_MAX_NODES: i32 = 5000;

fn guard_snapshot_complete(
    snapshot: &crate::proto::graphrag::GetGraphSnapshotResponse,
) -> Result<()> {
    if snapshot.nodes.len() >= SNAPSHOT_MAX_NODES as usize {
        anyhow::bail!(
            "graph snapshot reached the {SNAPSHOT_MAX_NODES}-node limit; signal edges may be \
             truncated, refusing to match on incomplete data"
        );
    }
    Ok(())
}

/// 取得済み snapshot から support_case の累積 signal 集合を復元する（純関数・追加 RPC なし）。
pub fn case_signals_from_snapshot(
    schema: &str,
    case_id: &str,
    snapshot: &crate::proto::graphrag::GetGraphSnapshotResponse,
) -> SignalSet {
    let case_node_id = harness_node_id(schema, "support_case", case_id);
    let signal_values = signal_value_index(snapshot);
    snapshot
        .edges
        .iter()
        .filter(|e| e.edge_type == "HAS_SIGNAL" && e.from_id == case_node_id)
        .filter_map(|e| signal_values.get(&e.to_id).map(Signal::new))
        .collect()
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

/// 過去事例を取得済み snapshot から検索する（追加 RPC なし。evaluate の hot path 用）。
/// scoring は `KnowledgeStore::search_cases` と同じ `crate::mcp::section_score` を再利用する。
/// `exclude_case_id` は現在進行中の case（呼び出し元が自身の case_id を知っている）を
/// 除外するためのもの。S1-1 の取得段が返す参考情報であり、判定入力にはしない（呼び出し元で
/// decide() に渡さないこと。この関数自体も decide() を一切参照しない・純関数）。
pub fn search_cases_from_snapshot(
    snapshot: &crate::proto::graphrag::GetGraphSnapshotResponse,
    question: &str,
    top_k: usize,
    exclude_case_id: Option<&str>,
) -> Vec<(PastCase, f32)> {
    let query_norm = crate::resolve::normalize_key(question);
    let mut hits: Vec<(PastCase, f32)> = snapshot
        .nodes
        .iter()
        .filter(|n| n.node_type == "support_case")
        .filter_map(|n| {
            let case_id = n.attributes.get("case_id")?.clone();
            if exclude_case_id == Some(case_id.as_str()) {
                return None;
            }
            let case = PastCase {
                case_id,
                question: n.attributes.get("question").cloned().unwrap_or_default(),
                product_key: n.attributes.get("product_key").cloned().unwrap_or_default(),
                actor: n.attributes.get("actor").cloned().unwrap_or_default(),
                created_at: n.attributes.get("created_at").cloned().unwrap_or_default(),
                last_decision: n
                    .attributes
                    .get("last_decision")
                    .cloned()
                    .unwrap_or_default(),
            };
            let score = crate::mcp::section_score(&query_norm, question, &case.question);
            (score > 0.3).then_some((case, score))
        })
        .collect();
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(top_k.max(1));
    hits
}

fn parse_binding(value: Option<&String>) -> Binding {
    match value.map(String::as_str) {
        Some("mandatory") => Binding::Mandatory,
        _ => Binding::Advisory,
    }
}

pub fn escalation_rule_from_attributes(attrs: &HashMap<String, String>) -> Result<EscalationRule> {
    let id = attrs
        .get("rule_id")
        .cloned()
        .ok_or_else(|| anyhow!("escalation_rule missing rule_id"))?;
    // condition は required（schema）。欠落・空を空集合に落とすと第1層がサイレントに
    // 無効化される（fail open）ため、設定ミスとしてエラーにする（fail closed）。
    let condition = csv_signals(
        attrs
            .get("condition")
            .ok_or_else(|| anyhow!("escalation_rule {id} missing condition"))?,
    );
    if condition.is_empty() {
        return Err(anyhow!("escalation_rule {id} has an empty condition"));
    }
    Ok(EscalationRule {
        route: attrs
            .get("route")
            .cloned()
            .ok_or_else(|| anyhow!("escalation_rule {id} missing route"))?,
        owner: attrs.get("owner").cloned().filter(|v| !v.is_empty()),
        binding: parse_binding(attrs.get("binding")),
        id,
        condition,
    })
}

pub fn prohibited_domain_from_attributes(
    attrs: &HashMap<String, String>,
) -> Result<ProhibitedDomain> {
    let id = attrs
        .get("domain_id")
        .cloned()
        .ok_or_else(|| anyhow!("prohibited_domain missing domain_id"))?;
    // pattern は required（schema）。欠落を空リストに落とすと禁止領域が素通りする
    // （fail open）ため、設定ミスとしてエラーにする（fail closed）。
    let text_patterns = csv_list(
        attrs
            .get("pattern")
            .ok_or_else(|| anyhow!("prohibited_domain {id} missing pattern"))?,
    );
    let domain_signals = csv_signals(
        attrs
            .get("domain_signals")
            .map(String::as_str)
            .unwrap_or(""),
    );
    if text_patterns.is_empty() && domain_signals.is_empty() {
        return Err(anyhow!(
            "prohibited_domain {id} has neither text patterns nor domain signals"
        ));
    }
    Ok(ProhibitedDomain {
        route: attrs
            .get("route")
            .cloned()
            .ok_or_else(|| anyhow!("prohibited_domain {id} missing route"))?,
        binding: parse_binding(attrs.get("binding")),
        id,
        domain_signals,
        text_patterns,
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
    /// evaluate が最後に記録した判定（"allowed" / "escalate"）。旧行には無いことがあり、
    /// その場合は空文字を許容する（fail closed にはしない。参考情報のため）。
    #[serde(default)]
    pub last_decision: String,
}

/// 担当者が追加する新ルール（add_known_resolution / correction_intake の出口）。
#[derive(Debug, Clone)]
pub struct NewKnownResolution {
    pub signal_set: SignalSet,
    pub applicability: String,
    pub answer: String,
    pub origin: String,
    pub created_by: String,
    /// 担当者の判断理由（任意）。BECAUSE → Rationale で残す
    pub rationale_text: Option<String>,
    /// マニュアル出典 section（BASED_ON → ManualSection で結線）
    pub manual_section_keys: Vec<String>,
}

/// KR 1 件をグラフ表現（KR ノード + Signal ノード + HAS_SIGNAL / BECAUSE 辺）に組み立てる。
/// signal_set を JSON 属性に畳まない（I2）。予約フィールドは空で持たせる（S1-3）。
///
/// `schema_kind` でスキーマ形状を分岐する:
/// - `ManualV1`: Rationale ノード + BECAUSE→Rationale（rationale_text がある場合）、
///   BASED_ON→ManualSection（manual_section_keys 分）。
/// - `LegacySection`: Rationale ノード型もBASED_ON辺も持たないため、KR から
///   section へ直接 BECAUSE 辺を張る（Step 1 の旧形状）。rationale_text は
///   admission（Harness::admit_known_resolution）側で legacy を拒否済みの前提のため、
///   ここでは無視する。
pub fn build_known_resolution_graph(
    schema: &str,
    kr_id: &str,
    kr: &NewKnownResolution,
    schema_kind: crate::config::ManualSchemaKind,
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
    // 判断理由 → Rationale ノード + BECAUSE（ManualV1 のみ。legacy には Rationale ノード型が無い。
    // rationale_text は admission = Harness::admit_known_resolution 側で legacy を拒否済みの
    // 前提のため、ここでは schema_kind で見るだけで足りる）。
    if schema_kind == crate::config::ManualSchemaKind::ManualV1 {
        if let Some(text) = &kr.rationale_text {
            let rationale_id = harness_node_id(schema, "Rationale", &format!("{kr_id}-r"));
            nodes.push(GraphNode {
                id: rationale_id.clone(),
                node_type: "Rationale".to_string(),
                attributes: vec![
                    ("rationale_id".to_string(), format!("{kr_id}-r")),
                    ("text".to_string(), text.clone()),
                ],
            });
            edges.push(GraphEdge {
                from_id: kr_node_id.clone(),
                to_id: rationale_id,
                edge_type: "BECAUSE".to_string(),
                attributes: Vec::new(),
            });
        }
    }
    // マニュアル出典: ManualV1 は BASED_ON→ManualSection、legacy には ManualSection/BASED_ON が
    // 無いため KR → section へ直接 BECAUSE 辺を張る（Step 1 の旧形状）。
    for section_key in &kr.manual_section_keys {
        let (to_id, edge_type) = match schema_kind {
            crate::config::ManualSchemaKind::ManualV1 => (
                crate::manual::schema_ids::manual_node_id(schema, "ManualSection", section_key),
                "BASED_ON",
            ),
            crate::config::ManualSchemaKind::LegacySection => (
                crate::ingest::section_node_id(schema, section_key),
                "BECAUSE",
            ),
        };
        edges.push(GraphEdge {
            from_id: kr_node_id.clone(),
            to_id,
            edge_type: edge_type.to_string(),
            attributes: Vec::new(),
        });
    }
    GraphBuild { nodes, edges }
}

/// answer_evidence をキー・種別ペアからグラフ表現に組み立てる（S1-2: emit した回答の証跡）。
/// items は `(section_key, kind)` のペア。kind は `"manual"` | `"known_resolution"`。
/// evidence_id はここで新規採番するため、呼び出す度に異なるノードが生成される（追記専用・上書きなし）。
pub fn build_answer_evidence_graph(
    schema: &str,
    attempt_id: &str,
    items: &[(&str, &str)],
) -> GraphBuild {
    let nodes = items
        .iter()
        .map(|(section_key, kind)| {
            let evidence_id = format!("ev-{}", uuid::Uuid::new_v4());
            GraphNode {
                id: harness_node_id(schema, "answer_evidence", &evidence_id),
                node_type: "answer_evidence".to_string(),
                attributes: vec![
                    ("evidence_id".to_string(), evidence_id),
                    ("attempt_id".to_string(), attempt_id.to_string()),
                    ("section_key".to_string(), section_key.to_string()),
                    ("kind".to_string(), kind.to_string()),
                ],
            }
        })
        .collect();
    GraphBuild {
        nodes,
        edges: Vec::new(),
    }
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

    /// 完全性ガード付きで graph snapshot を 1 回取得する。
    /// evaluate のように複数箇所で snapshot が要る場合はこれを 1 回呼んで共有する
    /// （1 リクエスト中の重複取得を避ける）。
    pub async fn fetch_snapshot(
        &self,
        schema: &str,
    ) -> Result<crate::proto::graphrag::GetGraphSnapshotResponse> {
        let snapshot = self
            .client
            .graph_snapshot(schema, SNAPSHOT_MAX_NODES)
            .await?;
        guard_snapshot_complete(&snapshot)?;
        Ok(snapshot)
    }

    /// KnownResolution を Signal ノード経由で復元する（HAS_SIGNAL 辺の走査）。
    pub async fn load_known_resolutions(&self, schema: &str) -> Result<Vec<KnownResolution>> {
        let snapshot = self.fetch_snapshot(schema).await?;
        self.load_known_resolutions_with(schema, &snapshot).await
    }

    /// 取得済み snapshot を使う変種（evaluate の hot path 用）。
    pub async fn load_known_resolutions_with(
        &self,
        schema: &str,
        snapshot: &crate::proto::graphrag::GetGraphSnapshotResponse,
    ) -> Result<Vec<KnownResolution>> {
        let kr_nodes = self
            .client
            .query_nodes(schema, "KnownResolution", Vec::new(), 1000)
            .await
            .context("load known resolutions")?;
        if kr_nodes.is_empty() {
            return Ok(Vec::new());
        }
        let signal_values = signal_value_index(snapshot);
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
        schema_kind: crate::config::ManualSchemaKind,
    ) -> Result<String> {
        let kr_id = format!("kr-{}", uuid::Uuid::new_v4());
        let build = build_known_resolution_graph(schema, &kr_id, kr, schema_kind);
        self.client.upsert_graph_low_level(build).await?;
        Ok(kr_id)
    }

    /// answer_attempt が実際に emit した根拠（manual section / known_resolution）を
    /// answer_evidence として追記する（S1-2）。items が空なら何もしない
    /// （escalate 済みの case は emit 経路に乗らないため呼び出し元も空で来る）。
    pub async fn append_answer_evidence(
        &self,
        schema: &str,
        attempt_id: &str,
        items: &[(String, String)],
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let refs: Vec<(&str, &str)> = items
            .iter()
            .map(|(section_key, kind)| (section_key.as_str(), kind.as_str()))
            .collect();
        let build = build_answer_evidence_graph(schema, attempt_id, &refs);
        self.client.upsert_graph_low_level(build).await?;
        Ok(())
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
        let snapshot = self.fetch_snapshot(schema).await?;
        Ok(case_signals_from_snapshot(schema, case_id, &snapshot))
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
                    last_decision: attrs.get("last_decision").cloned().unwrap_or_default(),
                })
            })
            .collect())
    }

    /// support_case を 1 件読む（存在検証・lineage 検証用）。
    pub async fn load_case(
        &self,
        schema: &str,
        case_id: &str,
    ) -> Result<Option<HashMap<String, String>>> {
        Ok(self
            .client
            .query_nodes(schema, "support_case", vec![("case_id", "eq", case_id)], 1)
            .await
            .context("load support case")?
            .into_iter()
            .next()
            .map(|node| node.attributes))
    }

    /// 指定 KR に紐づく answer_attempt を全件読む（grade カウントの導出元）。
    pub async fn load_attempts_for_kr(
        &self,
        schema: &str,
        kr_id: &str,
    ) -> Result<Vec<HashMap<String, String>>> {
        Ok(self
            .client
            .query_nodes(
                schema,
                "answer_attempt",
                vec![("known_resolution_id", "eq", kr_id)],
                1000,
            )
            .await
            .context("load attempts for known resolution")?
            .into_iter()
            .map(|node| node.attributes)
            .collect())
    }

    /// answer_attempt を 1 件読む（outcome / feedback の provenance 検証用）。
    pub async fn load_attempt(
        &self,
        schema: &str,
        attempt_id: &str,
    ) -> Result<Option<HashMap<String, String>>> {
        Ok(self
            .client
            .query_nodes(
                schema,
                "answer_attempt",
                vec![("attempt_id", "eq", attempt_id)],
                1,
            )
            .await
            .context("load answer attempt")?
            .into_iter()
            .next()
            .map(|node| node.attributes))
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
    /// read-merge-write: 既存属性を読み出して重ねるため、UpsertNodes が
    /// merge / 全属性置換のどちらのセマンティクスでも既存属性（answer_text 等）を失わない。
    pub async fn update_known_resolution_grade(
        &self,
        schema: &str,
        kr_id: &str,
        approval_count: u32,
        rejection_count: u32,
        approver_set: &[String],
        grade: Grade,
    ) -> Result<()> {
        let mut merged: HashMap<String, String> = self
            .client
            .query_nodes(schema, "KnownResolution", vec![("kr_id", "eq", kr_id)], 1)
            .await
            .context("load known resolution for grade update")?
            .into_iter()
            .next()
            .map(|node| node.attributes)
            .ok_or_else(|| anyhow!("known_resolution not found: {kr_id}"))?;
        merged.extend([
            ("kr_id".to_string(), kr_id.to_string()),
            ("approval_count".to_string(), approval_count.to_string()),
            ("rejection_count".to_string(), rejection_count.to_string()),
            ("approver_set".to_string(), approver_set.join(",")),
            ("grade".to_string(), grade.as_str().to_string()),
        ]);
        let node = GraphNode {
            id: harness_node_id(schema, "KnownResolution", kr_id),
            node_type: "KnownResolution".to_string(),
            attributes: merged.into_iter().collect(),
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
    fn kr_graph_splits_rationale_and_manual_basis() {
        use crate::harness::signal::Signal;
        let new_kr = NewKnownResolution {
            signal_set: [Signal::new("sd_not_recognized")].into_iter().collect(),
            applicability: "全モデル".to_string(),
            answer: "推奨は東芝製です。".to_string(),
            origin: "escalation:esc-1".to_string(),
            created_by: "sup-001".to_string(),
            rationale_text: Some("メーカー動作確認リストに基づく".to_string()),
            manual_section_keys: vec!["sec-sd-not-recognized".to_string()],
        };
        let build = build_known_resolution_graph(
            "urtect",
            "kr-1",
            &new_kr,
            crate::config::ManualSchemaKind::ManualV1,
        );
        // Rationale ノード + BECAUSE 辺
        assert_eq!(
            build
                .nodes
                .iter()
                .filter(|n| n.node_type == "Rationale")
                .count(),
            1
        );
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "BECAUSE")
                .count(),
            1
        );
        // BASED_ON → ManualSection 辺
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "BASED_ON")
                .count(),
            1
        );
        let based = build
            .edges
            .iter()
            .find(|e| e.edge_type == "BASED_ON")
            .unwrap();
        assert!(based.to_id.ends_with("ManualSection:sec-sd-not-recognized"));
        // HAS_SIGNAL は従来どおり
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "HAS_SIGNAL")
                .count(),
            1
        );
    }

    #[test]
    fn kr_without_rationale_text_has_no_rationale_node() {
        use crate::harness::signal::Signal;
        let new_kr = NewKnownResolution {
            signal_set: [Signal::new("sd_not_recognized")].into_iter().collect(),
            applicability: "x".to_string(),
            answer: "y".to_string(),
            origin: "manual".to_string(),
            created_by: "sup".to_string(),
            rationale_text: None,
            manual_section_keys: vec![],
        };
        let build = build_known_resolution_graph(
            "urtect",
            "kr-2",
            &new_kr,
            crate::config::ManualSchemaKind::ManualV1,
        );
        assert_eq!(
            build
                .nodes
                .iter()
                .filter(|n| n.node_type == "Rationale")
                .count(),
            0
        );
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "BASED_ON")
                .count(),
            0
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
            rationale_text: Some("doc-1#storage の保管条件に基づく".to_string()),
            manual_section_keys: vec!["doc-1#storage".to_string()],
        };
        // ManualV1 経路のテストなので schema 名も ManualV1 テナント（urtect）に揃える
        let build = build_known_resolution_graph(
            "urtect",
            "kr-test",
            &new_kr,
            crate::config::ManualSchemaKind::ManualV1,
        );
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

    #[test]
    fn answer_evidence_nodes_built_per_key() {
        let build = build_answer_evidence_graph(
            "urtect",
            "att-1",
            &[("sec-a", "manual"), ("kr-1", "known_resolution")],
        );
        assert_eq!(build.nodes.len(), 2);
        for node in &build.nodes {
            assert_eq!(node.node_type, "answer_evidence");
            let get = |key: &str| {
                node.attributes
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
            };
            assert!(
                get("evidence_id").filter(|v| !v.is_empty()).is_some(),
                "evidence_id must be non-empty"
            );
            assert_eq!(get("attempt_id").as_deref(), Some("att-1"));
        }
        let pairs: Vec<(String, String)> = build
            .nodes
            .iter()
            .map(|n| {
                let get = |key: &str| {
                    n.attributes
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.clone())
                        .unwrap()
                };
                (get("section_key"), get("kind"))
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("sec-a".to_string(), "manual".to_string()),
                ("kr-1".to_string(), "known_resolution".to_string()),
            ]
        );
        // evidence_id は各ノードで異なる（衝突しない一意キー）
        let ids: Vec<String> = build
            .nodes
            .iter()
            .map(|n| {
                n.attributes
                    .iter()
                    .find(|(k, _)| k == "evidence_id")
                    .unwrap()
                    .1
                    .clone()
            })
            .collect();
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn legacy_schema_kr_graph_uses_because_edges_to_sections_no_rationale_or_based_on() {
        // legacy (sivira) schema には Rationale ノード型も BASED_ON 辺も無い。
        // KR → section へ直接 BECAUSE 辺を張る Step 1 の旧形状に一致すること（regression 回避）。
        let new_kr = NewKnownResolution {
            signal_set: [Signal::new("mold")].into_iter().collect(),
            applicability: "全ロット".to_string(),
            answer: "廃棄してください".to_string(),
            origin: "escalation:esc-1".to_string(),
            created_by: "sup-001".to_string(),
            rationale_text: None,
            manual_section_keys: vec!["doc-1#storage".to_string()],
        };
        let build = build_known_resolution_graph(
            "sivira-cs-demo",
            "kr-legacy",
            &new_kr,
            crate::config::ManualSchemaKind::LegacySection,
        );
        assert_eq!(
            build
                .nodes
                .iter()
                .filter(|n| n.node_type == "Rationale")
                .count(),
            0
        );
        assert_eq!(
            build
                .edges
                .iter()
                .filter(|e| e.edge_type == "BASED_ON")
                .count(),
            0
        );
        let because_edges: Vec<_> = build
            .edges
            .iter()
            .filter(|e| e.edge_type == "BECAUSE")
            .collect();
        assert_eq!(because_edges.len(), 1);
        assert_eq!(
            because_edges[0].to_id,
            crate::ingest::section_node_id("sivira-cs-demo", "doc-1#storage")
        );
    }

    fn support_case_node(
        case_id: &str,
        question: &str,
        last_decision: &str,
    ) -> crate::proto::graphrag::GraphNode {
        use crate::proto::graphrag::GraphNode as ProtoNode;
        ProtoNode {
            node_id: format!("urtect:gen1:support_case:{case_id}"),
            node_type: "support_case".to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: [
                ("case_id".to_string(), case_id.to_string()),
                ("question".to_string(), question.to_string()),
                ("last_decision".to_string(), last_decision.to_string()),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn search_cases_from_snapshot_excludes_current_case_orders_and_limits() {
        use crate::proto::graphrag::GetGraphSnapshotResponse;
        let current = support_case_node("case-current", "電源が入らない 起動しない", "escalate");
        let strong_match = support_case_node("case-strong", "電源が入らない", "allowed");
        let weak_match = support_case_node("case-weak", "電源 ランプ 点滅", "escalate");
        let no_match = support_case_node("case-none", "配送先の変更方法", "allowed");
        let snapshot = GetGraphSnapshotResponse {
            nodes: vec![
                current.clone(),
                strong_match.clone(),
                weak_match.clone(),
                no_match.clone(),
            ],
            edges: vec![],
            truncated: false,
            total_node_count: 4,
        };

        let hits = search_cases_from_snapshot(&snapshot, "電源が入らない", 3, Some("case-current"));

        // 現在の case は自己引用にならないよう除外される
        assert!(hits.iter().all(|(c, _)| c.case_id != "case-current"));
        // スコア降順（強い一致が先頭）
        assert_eq!(hits.first().unwrap().0.case_id, "case-strong");
        for pair in hits.windows(2) {
            assert!(pair[0].1 >= pair[1].1, "hits must be sorted by score desc");
        }
    }

    #[test]
    fn search_cases_from_snapshot_respects_top_k() {
        use crate::proto::graphrag::GetGraphSnapshotResponse;
        let nodes: Vec<crate::proto::graphrag::GraphNode> = (0..5)
            .map(|i| support_case_node(&format!("case-{i}"), "電源が入らない", "allowed"))
            .collect();
        let snapshot = GetGraphSnapshotResponse {
            nodes,
            edges: vec![],
            truncated: false,
            total_node_count: 5,
        };

        let hits = search_cases_from_snapshot(&snapshot, "電源が入らない", 2, None);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_cases_from_snapshot_tolerates_missing_last_decision() {
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphNode as ProtoNode};
        let node = ProtoNode {
            node_id: "urtect:gen1:support_case:case-old".to_string(),
            node_type: "support_case".to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: [
                ("case_id".to_string(), "case-old".to_string()),
                ("question".to_string(), "電源が入らない".to_string()),
                // last_decision は古い行に無いことがある
            ]
            .into_iter()
            .collect(),
        };
        let snapshot = GetGraphSnapshotResponse {
            nodes: vec![node],
            edges: vec![],
            truncated: false,
            total_node_count: 1,
        };

        let hits = search_cases_from_snapshot(&snapshot, "電源が入らない", 3, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0.case_id, "case-old");
    }
}
