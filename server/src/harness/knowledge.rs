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

pub(crate) fn csv_signals(value: &str) -> SignalSet {
    csv_list(value).into_iter().map(Signal::new).collect()
}

/// レビュー修正5: `SignalSet` は `BTreeSet<Signal>`（`harness::signal::SignalSet` の型エイリアス）
/// であり、`.iter()` は常に `Signal` の `Ord`（内部 `String` の辞書順）で昇順を返す。したがって
/// 同じ集合であれば挿入順・呼び出しタイミングに関わらず本関数の出力は常に同じ文字列になり、
/// 追加のソート処理は不要（誤って `HashSet` ベースの型に置き換わった場合の回帰は
/// `signals_to_csv_is_deterministic_regardless_of_insertion_order` が検出する）。
pub(crate) fn signals_to_csv(signals: &SignalSet) -> String {
    signals
        .iter()
        .map(Signal::as_str)
        .collect::<Vec<_>>()
        .join(",")
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
    /// `created_by` は安定 ID（`google-sub:{sub}`）で人間には読めないため、
    /// ノウハウの出所を追う担当者向けに登録時点の email も併記する。
    /// 主識別子はあくまで `created_by`（email は変更・再割当てされうる「当時の値」）。
    pub created_by_email: String,
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
            ("created_by_email".to_string(), kr.created_by_email.clone()),
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

/// support_case の既存属性から次の ConversationTurn の `seq` を算出する純関数（1 起点）。
/// `turn_count` 属性が未設定、または int としてパースできない場合は 0 件として扱う
/// （旧データ・ターン未記録の case でも fail closed にしない。read の結果をここへ渡すのは
/// 呼び出し元 [`KnowledgeStore::record_conversation_turn`] の責務）。
fn next_turn_seq(existing_case_attrs: &HashMap<String, String>) -> u32 {
    existing_case_attrs
        .get("turn_count")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        + 1
}

/// support_case の read-merge-write 用属性を組み立てる純関数。既存属性を全て保持しつつ
/// `turn_count` だけ新しい seq へ上書きする（`harness::mod::merge_conv_state_attributes` /
/// `merge_out_of_scope_demotion_attributes` と同じ規律。`UpsertNodes` は全置換のため部分送信
/// すると既存属性が消える）。
fn merge_turn_count_attribute(
    existing_case_attrs: &HashMap<String, String>,
    seq: u32,
) -> Vec<(String, String)> {
    let mut merged = existing_case_attrs.clone();
    merged.insert("turn_count".to_string(), seq.to_string());
    merged.into_iter().collect()
}

/// `ConversationTurn` の属性 map 列を `created_at` 降順・同値時は `turn_id` 昇順で安定
/// ソートする純関数（Issue #31 codex レビュー W2(a)）。vegapunk 側の `sort_by=created_at` は
/// 完全一致する境界の順序を保証しないため、この二次キーで決定的にする（`turn_id` は
/// ConversationTurn の一意 upsert key）。`created_at` / `turn_id` が欠けている行は空文字列
/// 扱いで並べる（不完全な行自体は `TurnRow::from_attrs` 側で別途弾かれるため、ここでは
/// 並び順の決定性だけを保証すれば足りる）。
fn stable_sort_by_created_at_desc_then_turn_id(rows: &mut [HashMap<String, String>]) {
    rows.sort_by(|a, b| {
        let created_a = a.get("created_at").map(String::as_str).unwrap_or("");
        let created_b = b.get("created_at").map(String::as_str).unwrap_or("");
        created_b.cmp(created_a).then_with(|| {
            let turn_a = a.get("turn_id").map(String::as_str).unwrap_or("");
            let turn_b = b.get("turn_id").map(String::as_str).unwrap_or("");
            turn_a.cmp(turn_b)
        })
    });
}

/// `load_conversation_turns_page` が返す 1 ページ分。管理 API（`admin.rs::build_threads_page`）
/// が短ページ終端時に backend 申告の `total_count` と実消費件数を突合して fail closed する
/// ための材料（Issue #31 codex レビュー W2(b)）。
pub struct ConversationTurnsPage {
    pub rows: Vec<HashMap<String, String>>,
    pub total_count: i64,
}

/// `KnownResolution` の型明示クエリで vegapunk へ渡す node_type（`load_known_resolutions_with`
/// が実際に使う検索エントリポイント）。ConversationTurn 等の新規ノード型を誤って KR 検索の
/// 対象へ紛れ込ませていないかを固定する回帰テスト
/// (`known_resolution_query_type_is_not_conversation_turn`) の対象。
const KIND_KNOWN_RESOLUTION: &str = "KnownResolution";

/// ConversationTurn 1 件分のグラフ表現（ノード + `case -HAS_TURN-> turn` 辺）を組み立てる
/// 純関数（Issue #31 design doc §2）。`record_conversation_turn` から network I/O を分離して
/// あるのは、属性組み立て・6 種の reply_kind 分類を実 vegapunk 無しでテストするため
/// （`build_known_resolution_graph` / `build_answer_evidence_graph` と同じ規律）。
///
/// **検索非汚染（design doc §2 受け入れ条件）**: 戻り値は `GraphBuild`（nodes/edges のみ）で
/// あり、ベクトルを一切含まない型そのものが「この関数が `upsert_vectors` を呼びうる経路を
/// 持たない」ことを構造的に保証する。呼び出し元 `record_conversation_turn` もこの `GraphBuild`
/// を `upsert_graph_low_level`（nodes/edges の upsert のみ）に渡すだけで、`upsert_vectors` は
/// 一切呼ばない。ConversationTurn にベクトルが無ければ、marker フィルタで絞る意味検索
/// （`search_ids_with_scores`）の候補にすらそもそも挙がらない。
#[allow(clippy::too_many_arguments)]
pub fn build_conversation_turn_graph(
    schema: &str,
    turn_id: &str,
    case_id: &str,
    end_user_id: Option<&str>,
    seq: u32,
    created_at: &str,
    question: &str,
    reply_text: &str,
    reply_kind: &str,
    audit_event_id: &str,
) -> GraphBuild {
    let turn_node_id = harness_node_id(schema, "ConversationTurn", turn_id);
    let case_node_id = harness_node_id(schema, "support_case", case_id);
    let mut attributes = vec![
        ("turn_id".to_string(), turn_id.to_string()),
        ("case_id".to_string(), case_id.to_string()),
        ("seq".to_string(), seq.to_string()),
        ("created_at".to_string(), created_at.to_string()),
        ("question".to_string(), question.to_string()),
        ("reply_text".to_string(), reply_text.to_string()),
        ("reply_kind".to_string(), reply_kind.to_string()),
        ("audit_event_id".to_string(), audit_event_id.to_string()),
    ];
    if let Some(id) = end_user_id {
        attributes.push(("end_user_id".to_string(), id.to_string()));
    }
    let node = GraphNode {
        id: turn_node_id.clone(),
        node_type: "ConversationTurn".to_string(),
        attributes,
    };
    let edge = GraphEdge {
        from_id: case_node_id,
        to_id: turn_node_id,
        edge_type: "HAS_TURN".to_string(),
        attributes: Vec::new(),
    };
    GraphBuild {
        nodes: vec![node],
        edges: vec![edge],
    }
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
            .query_nodes(schema, KIND_KNOWN_RESOLUTION, Vec::new(), 1000)
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

    /// 会話ターン 1 件を書き切る（Issue #31 design doc §2）。immutable・部分更新なし。
    /// `seq` は support_case 属性 `turn_count` を read-merge-write して採番する（1 起点。
    /// Issue #31 codex レビュー W1: 旧実装は `query_nodes` の固定 limit=1000 で既存ターン数を
    /// 数える方式で、1000 件超の会話で seq が重複していた）。
    ///
    /// **書き込み順序（turn_count → ConversationTurn）**: support_case.turn_count を先に
    /// 進めてから ConversationTurn ノードを書く。逆順だと「ConversationTurn の書き込みは
    /// 成功したが turn_count の書き戻しが失敗する」場合に、次回呼び出しが同じ seq を再採番して
    /// 重複した seq を持つ 2 つの ConversationTurn が生まれる。turn_count を先に進めておけば、
    /// 同じ失敗パターンは「turn_count だけ進んで対応する ConversationTurn が存在しない seq の
    /// 欠番」に倒れる。欠番はスレッド詳細の表示（`seq` 昇順ソート）を壊さないが、重複 seq は
    /// 2 つの実在ターンの順序があいまいになる。欠落より重複を避ける（安全側）。
    ///
    /// **契約: 呼び出し時点で case が既に存在すること。** `/api/reply` の応答確定点は必ず
    /// `evaluate()` 等で case が作成された後に到達するため通常は満たされるが、満たされない
    /// まま呼ぶと `turn_count` だけを持つ `case_id` 属性なしの support_case ノードを書いて
    /// しまう（`harness::mod::require_existing_case_attrs` が `save_conv_state` に対して防いで
    /// いるのと同じ壊れ方）。そのため case が見つからない場合は fail closed する。
    ///
    /// **呼び出し元の契約**: 書き込み失敗（case 読み取り・turn_count 書き戻し・
    /// ConversationTurn upsert のいずれも）は `Err` をそのまま返す。応答を止めない（warn ログ
    /// のみで継続する）かどうかは呼び出し元（`api.rs::ok_reply_response`）の責務であり、ここ
    /// では判断しない。
    #[allow(clippy::too_many_arguments)]
    pub async fn record_conversation_turn(
        &self,
        schema: &str,
        case_id: &str,
        end_user_id: Option<&str>,
        question: &str,
        reply_text: &str,
        reply_kind: &str,
        audit_event_id: &str,
    ) -> Result<()> {
        let existing_case = self.load_case(schema, case_id).await?.ok_or_else(|| {
            anyhow!(
                "record_conversation_turn: case {case_id} not found in schema {schema}; \
                 refusing to write a turn_count-only support_case attribute set for a case that \
                 does not exist yet — investigate why the caller reached the reply confirmation \
                 point without first creating the case"
            )
        })?;
        let seq = next_turn_seq(&existing_case);
        self.record(
            schema,
            "support_case",
            case_id,
            merge_turn_count_attribute(&existing_case, seq),
        )
        .await
        .context("advance support_case.turn_count before writing the conversation turn")?;
        let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
        let created_at = chrono::Utc::now().to_rfc3339();
        let build = build_conversation_turn_graph(
            schema,
            &turn_id,
            case_id,
            end_user_id,
            seq,
            &created_at,
            question,
            reply_text,
            reply_kind,
            audit_event_id,
        );
        // 検索非汚染: `build_conversation_turn_graph` の doc コメント参照。
        // `upsert_graph_low_level` は nodes/edges の upsert のみで、`upsert_vectors` は
        // 呼ばない。
        self.client.upsert_graph_low_level(build).await?;
        Ok(())
    }

    /// 管理 API（`admin.rs`）向け: `ConversationTurn` を `created_at` 降順（同値時は `turn_id`
    /// 昇順で決定的、Issue #31 codex レビュー W2(a)）に 1 ページ取得する（design doc §4 の
    /// スレッド一覧）。`extra_filter` は `end_user_id` 絞り込み等の追加条件。`offset` は
    /// 「これまでに消費した raw 件数」（次ページ取得用の内部カーソル。`vegapunk::query_nodes_paged`
    /// と同じ offset ページング方式。Issue #31 reviewer 指摘5: 値ベースの `created_at < cursor`
    /// フィルタは `created_at` が完全一致する境界でエントリを取りこぼす・重複させる欠陥が
    /// あったため、この codebase が既に信頼している offset ページングに統一した）。戻り値の
    /// `total_count`（Issue #31 codex レビュー W2(b)）は `admin.rs::build_threads_page` が
    /// 短ページ終端時の完全性突合に使う。
    pub async fn load_conversation_turns_page(
        &self,
        schema: &str,
        extra_filter: Option<(&str, &str, &str)>,
        offset: i32,
        page_size: i32,
    ) -> Result<ConversationTurnsPage> {
        let filters: Vec<(&str, &str, &str)> = extra_filter.into_iter().collect();
        let page = self
            .client
            .query_nodes_sorted(
                schema,
                "ConversationTurn",
                filters,
                "created_at",
                "desc",
                offset,
                page_size,
            )
            .await
            .context("load conversation turns page")?;
        let mut rows: Vec<HashMap<String, String>> =
            page.nodes.into_iter().map(|n| n.attributes).collect();
        stable_sort_by_created_at_desc_then_turn_id(&mut rows);
        Ok(ConversationTurnsPage {
            rows,
            total_count: page.total_count,
        })
    }

    /// 管理 API 向け: 指定 case の `ConversationTurn` を全件取得する（スレッド詳細）。
    /// `seq` 昇順への並べ替えは呼び出し側の責務（読み取り専用のここでは行わない）。
    pub async fn load_conversation_turns_for_case(
        &self,
        schema: &str,
        case_id: &str,
    ) -> Result<Vec<HashMap<String, String>>> {
        Ok(self
            .client
            .query_nodes(
                schema,
                "ConversationTurn",
                vec![("case_id", "eq", case_id)],
                1000,
            )
            .await
            .context("load conversation turns for case")?
            .into_iter()
            .map(|n| n.attributes)
            .collect())
    }

    /// 管理 API 向け: 期間内（`created_at >= cutoff_rfc3339`）の `ConversationTurn` を全件取得する
    /// （Issue #31 design doc §4 の利用状況サマリ）。`query_nodes_paged` で取り切るため、
    /// 期間内の件数が `QueryNodes` の単発 limit（1000）を超えても欠落しない。
    pub async fn load_conversation_turns_since(
        &self,
        schema: &str,
        cutoff_rfc3339: &str,
    ) -> Result<Vec<HashMap<String, String>>> {
        Ok(self
            .client
            .query_nodes_paged(
                schema,
                "ConversationTurn",
                vec![("created_at", "gte", cutoff_rfc3339)],
                500,
            )
            .await
            .context("load conversation turns since cutoff")?
            .into_iter()
            .map(|n| n.attributes)
            .collect())
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

    // ---- signals_to_csv（レビュー修正5: 出力順の決定性） ----

    #[test]
    fn signals_to_csv_is_deterministic_regardless_of_insertion_order() {
        // SignalSet は BTreeSet なので、挿入順を variant.rs 側では制御できないが、
        // `Signal::new` を異なる順序で呼んでも同じ集合を組み立てれば同じ CSV になることを固定する。
        let ascending: SignalSet = ["hazard_x", "mold", "smoke"]
            .into_iter()
            .map(Signal::new)
            .collect();
        let descending: SignalSet = ["smoke", "mold", "hazard_x"]
            .into_iter()
            .map(Signal::new)
            .collect();
        assert_eq!(signals_to_csv(&ascending), "hazard_x,mold,smoke");
        assert_eq!(signals_to_csv(&ascending), signals_to_csv(&descending));
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
            created_by_email: "sup-001@sivira.co".to_string(),
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
            created_by_email: "sup@sivira.co".to_string(),
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

    /// W2: `created_by` は安定 ID（`google-sub:{sub}`）で人間には読めない。
    /// ノウハウの出所を担当者が追えるよう、登録時点の email も併記する。
    /// 加算属性であり、`created_by` を置き換えない（主識別子は安定 ID のまま）。
    #[test]
    fn known_resolution_node_records_creator_email_alongside_stable_id() {
        let new_kr = NewKnownResolution {
            signal_set: [Signal::new("mold")].into_iter().collect(),
            applicability: "全ロット".to_string(),
            answer: "廃棄してください".to_string(),
            origin: "manual".to_string(),
            created_by: "google-sub:101572111487015263315".to_string(),
            created_by_email: "alice@sivira.co".to_string(),
            rationale_text: None,
            manual_section_keys: Vec::new(),
        };
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
        let attr = |key: &str| {
            kr_node
                .attributes
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            attr("created_by"),
            Some("google-sub:101572111487015263315".to_string()),
            "stable actor id must remain the primary identifier"
        );
        assert_eq!(
            attr("created_by_email"),
            Some("alice@sivira.co".to_string()),
            "creator email must be recorded alongside the stable id"
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
            created_by_email: "sup-001@sivira.co".to_string(),
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

    // ---- build_conversation_turn_graph（Issue #31 design doc §2） ----

    fn attr<'a>(node: &'a crate::model::GraphNode, key: &str) -> Option<&'a str> {
        node.attributes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn conversation_turn_graph_builds_one_node_and_the_has_turn_edge() {
        let build = build_conversation_turn_graph(
            "urtect",
            "turn-1",
            "case-1",
            None,
            1,
            "2026-08-16T00:00:00+00:00",
            "電源が入りません",
            "電源ケーブルをご確認ください。",
            "answer",
            "audit-1",
        );
        assert_eq!(build.nodes.len(), 1);
        assert_eq!(build.edges.len(), 1);
        let node = &build.nodes[0];
        assert_eq!(node.node_type, "ConversationTurn");
        assert_eq!(
            node.id,
            harness_node_id("urtect", "ConversationTurn", "turn-1")
        );

        let edge = &build.edges[0];
        assert_eq!(edge.edge_type, "HAS_TURN");
        assert_eq!(
            edge.from_id,
            harness_node_id("urtect", "support_case", "case-1")
        );
        assert_eq!(edge.to_id, node.id);
    }

    #[test]
    fn conversation_turn_graph_records_all_required_attributes() {
        let build = build_conversation_turn_graph(
            "urtect",
            "turn-2",
            "case-2",
            None,
            3,
            "2026-08-16T01:02:03+00:00",
            "設定方法を教えてください",
            "手順は以下のとおりです。",
            "clarify",
            "audit-2",
        );
        let node = &build.nodes[0];
        assert_eq!(attr(node, "turn_id"), Some("turn-2"));
        assert_eq!(attr(node, "case_id"), Some("case-2"));
        assert_eq!(attr(node, "seq"), Some("3"));
        assert_eq!(attr(node, "created_at"), Some("2026-08-16T01:02:03+00:00"));
        assert_eq!(attr(node, "question"), Some("設定方法を教えてください"));
        assert_eq!(attr(node, "reply_text"), Some("手順は以下のとおりです。"));
        assert_eq!(attr(node, "reply_kind"), Some("clarify"));
        assert_eq!(attr(node, "audit_event_id"), Some("audit-2"));
        // end_user_id を渡していないので属性そのものが存在しない
        assert_eq!(attr(node, "end_user_id"), None);
    }

    #[test]
    fn conversation_turn_graph_includes_end_user_id_only_when_provided() {
        let build = build_conversation_turn_graph(
            "urtect",
            "turn-3",
            "case-3",
            Some("a1b2c3"),
            1,
            "2026-08-16T00:00:00+00:00",
            "q",
            "r",
            "out_of_scope",
            "audit-3",
        );
        assert_eq!(attr(&build.nodes[0], "end_user_id"), Some("a1b2c3"));
    }

    /// design doc §2 が列挙する 6 種の reply_kind のうち、実際に到達しうる 5 種（`fallback` は
    /// api.rs のどの分岐からも到達しないため対象外。api.rs の doc コメント参照）が、すべて
    /// そのまま `reply_kind` 属性に載ることを固定する（属性組み立ての回帰）。
    #[test]
    fn conversation_turn_graph_accepts_all_reachable_reply_kinds() {
        for kind in [
            "answer",
            "clarify",
            "escalation",
            "out_of_scope",
            "time_pref",
        ] {
            let build = build_conversation_turn_graph(
                "urtect",
                "turn-x",
                "case-x",
                None,
                1,
                "2026-08-16T00:00:00+00:00",
                "q",
                "r",
                kind,
                "audit-x",
            );
            assert_eq!(
                attr(&build.nodes[0], "reply_kind"),
                Some(kind),
                "reply_kind={kind} must round-trip unchanged"
            );
        }
    }

    /// 検索非汚染（design doc §2 受け入れ条件）: `GraphBuild` は nodes/edges のみを持つ型であり、
    /// ベクトルという概念自体が無い。この型を返す `build_conversation_turn_graph` は構造的に
    /// `upsert_vectors` を呼びうる経路を持たない（本テストはその不変条件を明示するドキュメント
    /// テストで、リグレッションを検出するというより「なぜベクトルが作られないか」を将来の
    /// 読者に示す）。
    #[test]
    fn conversation_turn_graph_never_produces_vectors() {
        let build = build_conversation_turn_graph(
            "urtect",
            "turn-4",
            "case-4",
            None,
            1,
            "2026-08-16T00:00:00+00:00",
            "q",
            "r",
            "answer",
            "audit-4",
        );
        // GraphBuild { nodes, edges } には vectors フィールドが存在しない。
        let GraphBuild { nodes, edges } = build;
        assert_eq!(nodes.len(), 1);
        assert_eq!(edges.len(), 1);
    }

    // ---- next_turn_seq / merge_turn_count_attribute（Issue #31 codex レビュー W1: seq 採番の
    // 1000 件超重複是正） ----

    #[test]
    fn next_turn_seq_increments_past_a_large_existing_turn_count() {
        let existing = attrs(&[("turn_count", "1500")]);
        assert_eq!(next_turn_seq(&existing), 1501);
    }

    #[test]
    fn next_turn_seq_starts_at_one_when_turn_count_is_absent() {
        let existing = attrs(&[("case_id", "case-1")]);
        assert_eq!(next_turn_seq(&existing), 1);
    }

    #[test]
    fn next_turn_seq_starts_at_one_when_turn_count_is_unparseable() {
        let existing = attrs(&[("turn_count", "not-a-number")]);
        assert_eq!(next_turn_seq(&existing), 1);
    }

    #[test]
    fn merge_turn_count_attribute_preserves_existing_attributes() {
        let existing = attrs(&[
            ("case_id", "case-1"),
            ("question", "質問"),
            ("turn_count", "3"),
        ]);
        let merged: HashMap<String, String> = merge_turn_count_attribute(&existing, 4)
            .into_iter()
            .collect();
        assert_eq!(merged.get("case_id"), Some(&"case-1".to_string()));
        assert_eq!(merged.get("question"), Some(&"質問".to_string()));
        assert_eq!(merged.get("turn_count"), Some(&"4".to_string()));
    }

    #[test]
    fn merge_turn_count_attribute_adds_the_key_when_absent() {
        let existing = attrs(&[("case_id", "case-1")]);
        let merged: HashMap<String, String> = merge_turn_count_attribute(&existing, 1)
            .into_iter()
            .collect();
        assert_eq!(merged.get("turn_count"), Some(&"1".to_string()));
    }

    // ---- stable_sort_by_created_at_desc_then_turn_id（Issue #31 codex レビュー W2(a)） ----

    #[test]
    fn stable_sort_breaks_ties_on_created_at_by_turn_id_ascending() {
        let mut rows = vec![
            attrs(&[
                ("turn_id", "turn-b"),
                ("created_at", "2026-08-16T00:00:00+00:00"),
            ]),
            attrs(&[
                ("turn_id", "turn-a"),
                ("created_at", "2026-08-16T00:00:00+00:00"),
            ]),
        ];
        stable_sort_by_created_at_desc_then_turn_id(&mut rows);
        assert_eq!(rows[0].get("turn_id"), Some(&"turn-a".to_string()));
        assert_eq!(rows[1].get("turn_id"), Some(&"turn-b".to_string()));
    }

    #[test]
    fn stable_sort_orders_distinct_created_at_descending_regardless_of_turn_id() {
        let mut rows = vec![
            attrs(&[
                ("turn_id", "turn-a"),
                ("created_at", "2026-08-16T00:00:00+00:00"),
            ]),
            attrs(&[
                ("turn_id", "turn-z"),
                ("created_at", "2026-08-16T01:00:00+00:00"),
            ]),
        ];
        stable_sort_by_created_at_desc_then_turn_id(&mut rows);
        assert_eq!(
            rows[0].get("turn_id"),
            Some(&"turn-z".to_string()),
            "newer created_at first"
        );
        assert_eq!(rows[1].get("turn_id"), Some(&"turn-a".to_string()));
    }

    #[test]
    fn stable_sort_is_deterministic_across_repeated_runs_with_many_ties() {
        let mut rows: Vec<HashMap<String, String>> = (0..10)
            .rev()
            .map(|i| {
                attrs(&[
                    ("turn_id", &format!("turn-{i}")),
                    ("created_at", "2026-08-16T00:00:00+00:00"),
                ])
            })
            .collect();
        stable_sort_by_created_at_desc_then_turn_id(&mut rows);
        let ids: Vec<&str> = rows
            .iter()
            .map(|r| r.get("turn_id").unwrap().as_str())
            .collect();
        let expected: Vec<String> = (0..10).map(|i| format!("turn-{i}")).collect();
        assert_eq!(ids, expected);
    }

    // ---- 検索非汚染: KnownResolution 型明示クエリのリテラル固定 ----

    #[test]
    fn known_resolution_query_type_is_not_conversation_turn() {
        assert_ne!(KIND_KNOWN_RESOLUTION, "ConversationTurn");
        assert_eq!(KIND_KNOWN_RESOLUTION, "KnownResolution");
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
            created_by_email: "sup-001@sivira.co".to_string(),
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

    // ---- schema/*.yml の support_case.attributes 網羅性（Issue #31 codex レビュー C1） ----
    //
    // `harness::mod` の support_case への書き込み箇所（`grep -n '"support_case"'
    // server/src/harness/mod.rs` で列挙される全 8 箇所）と、この場所（`record_conversation_turn`
    // の turn_count 書き戻し）が実際に書き込む属性キーの完全な集合。新しい属性を support_case
    // へ書き足したら、このリストと両 schema ファイルの `support_case.attributes` を同時に
    // 更新すること（更新を忘れると、このテストが両者の不一致を検出して落ちる）。
    //
    // vegapunk 側が未宣言属性の書き込みを拒否しうる（C1: end_user_id が未宣言のまま本番投入
    // されていた）ため、「書き込みキー ⊆ schema 宣言キー」を固定する。
    fn written_support_case_attribute_keys() -> std::collections::HashSet<String> {
        [
            "case_id",
            "request_id",
            "actor",
            "actor_email",
            "question",
            "product_key",
            "created_at",
            "end_user_id",
            "last_request_id",
            "last_decision",
            "last_kr_id",
            "last_evidence_keys",
            "last_evidence_kind",
            "clarify_turns",
            "awaiting_time_pref",
            "time_pref_false_count",
            "preferred_contact_time",
            "time_pref_extraction_error_count",
            "excluded_signals",
            "turn_count",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    /// `schema_path` の `nodes.support_case.attributes` から宣言済みキー集合を取り出す。
    /// cwd 非依存にするため呼び出し側は `CARGO_MANIFEST_DIR` 起点の絶対パスを渡すこと
    /// （`harness::mod::production_ng_dictionary` と同じ規律）。
    fn declared_support_case_attribute_keys(
        schema_path: &std::path::Path,
    ) -> std::collections::HashSet<String> {
        let text = std::fs::read_to_string(schema_path)
            .unwrap_or_else(|e| panic!("read schema file {schema_path:?}: {e}"));
        let value: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|e| panic!("parse schema file {schema_path:?} as YAML: {e}"));
        let attributes = value["nodes"]["support_case"]["attributes"]
            .as_mapping()
            .unwrap_or_else(|| {
                panic!("{schema_path:?}: nodes.support_case.attributes is not a mapping")
            });
        attributes
            .keys()
            .map(|k| {
                k.as_str()
                    .unwrap_or_else(|| panic!("{schema_path:?}: non-string attribute key"))
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn cs_schema_yml_declares_every_support_case_attribute_that_code_writes() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../schema/cs-schema.yml"
        ));
        let declared = declared_support_case_attribute_keys(path);
        let written = written_support_case_attribute_keys();
        let missing: Vec<&String> = written.difference(&declared).collect();
        assert!(
            missing.is_empty(),
            "schema/cs-schema.yml support_case.attributes is missing keys that code writes: \
             {missing:?}"
        );
    }

    #[test]
    fn cs_support_yml_declares_every_support_case_attribute_that_code_writes() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../schema/cs-support.yml"
        ));
        let declared = declared_support_case_attribute_keys(path);
        let written = written_support_case_attribute_keys();
        let missing: Vec<&String> = written.difference(&declared).collect();
        assert!(
            missing.is_empty(),
            "schema/cs-support.yml support_case.attributes is missing keys that code writes: \
             {missing:?}"
        );
    }
}
