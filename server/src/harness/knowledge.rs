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

/// signal 読み書き経路（`KnowledgeStore::load_case_signals` と
/// `KnowledgeStore::append_case_signals`）が共有する support_case ノード id ヘルパ
/// （Issue #39 レビュー残課題3）。この 2 箇所が個別に
/// `harness_node_id(schema, "support_case", case_id)` を書いていたため、片方だけ kind 文字列
/// "support_case" がずれても検出できなかった。読みと書きの対をこのヘルパへ統一することで、
/// signal 経路内の id 組み立てのずれが構造的に起き得ないようにする（signal 以外の経路には
/// 直接組み立てが残っており、全体の唯一箇所ではない）。
fn case_signal_node_id(schema: &str, case_id: &str) -> String {
    harness_node_id(schema, "support_case", case_id)
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
// TODO: bind to vegapunk traversal API — LegacySection の graph_snapshot 依存を撤去する
// （Issue #39 で traversal 化したのは `KnowledgeStore::load_case_signals`（admin
// `GET /admin/api/threads/{case_id}` 経路、1 経路）と `KnowledgeStore::load_known_resolutions`
// （homesec `/api/reply` の `draft_with_materials`、MCP tool `search_known_resolutions`、
// MCP tool `record_answer_outcome` 経由の `recompute_grade` の 3 経路）の計 4 経路のみ。
// ManualV1 の `evaluate()`（`harness/mod.rs` の `case_signals_from_snapshot` /
// `load_known_resolutions_with` 呼び出し）は今回のスコープ外で、`corpus.live_corpus` が
// 返す非 truncate の snapshot 形状データに依存したまま。LegacySection は sivira-cs-demo
// 専用・本番外のため snapshot 依存のまま残置している）。
const SNAPSHOT_MAX_NODES: i32 = 5000;

/// `traverse_neighbors_paged` に渡す 1 ページの上限（backend 上限、`corpus.rs::PAGE_SIZE` と
/// 同じ値）。support_case / KnownResolution 1 件あたりの HAS_SIGNAL 辺は数件〜数十件程度の
/// 想定で、この値を超えても `traverse_neighbors_paged` 自体がページングして全件取り切る
/// （欠落しない。1000 は backend 側の1リクエストあたり上限）。
const SIGNAL_TRAVERSE_PAGE_SIZE: i32 = 1000;

/// `signals_from_traverse` が辿る先のノード種別（Signal ノードのみ）。呼び出し元
/// （`load_case_signals` / `load_known_resolutions`）が値を選ぶ余地を無くし、
/// production で "Signal" を書く場所をこの1箇所に固定するための定数（Issue #39
/// レビュー W1/W2）。
const SIGNAL_NODE_TYPE: &str = "Signal";
/// `signals_from_traverse` が辿る辺種別。理由は `SIGNAL_NODE_TYPE` と同じ。
const SIGNAL_EDGE_TYPE: &str = "HAS_SIGNAL";
/// `signals_from_traverse` が辿る辺の向き。`append_case_signals` が
/// `from_id: case_node_id, to_id: signal_node_id` で書くため、起点（case / KnownResolution）
/// から見て "outgoing" が正しい。理由は `SIGNAL_NODE_TYPE` と同じ。
const SIGNAL_TRAVERSE_DIRECTION: &str = "outgoing";

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

/// `traverse_neighbors_paged(schema, "Signal", "HAS_SIGNAL", ...)` の結果から `SignalSet` を
/// 組み立てる純関数（`signal_value_index` の traversal 版・Issue #39）。呼び出し元が
/// node_type="Signal" 指定で問い合わせている前提だが、backend が `neighbor_node_type` を
/// 正しく honor する保証にコスト0で備えるため、`signal_value_index`（snapshot 版）と同じ
/// `node_type == "Signal"` フィルタをここでも掛ける（レビュー Suggestion 1）。各
/// `NodeResult` は Signal ノードで `attributes.get("value")` が signal 文字列を持つ想定。
/// value 属性が欠けている `NodeResult` は黙って捨てる（`signal_value_index` と同じ欠損
/// データの扱いに揃える）。
fn signal_set_from_node_results(results: &[crate::proto::graphrag::NodeResult]) -> SignalSet {
    results
        .iter()
        .filter(|n| n.node_type == SIGNAL_NODE_TYPE)
        .filter_map(|n| n.attributes.get("value").map(|v| Signal::new(v.as_str())))
        .collect()
}

/// `signals_from_traverse` の `fetch` が返す future の型（`admin.rs::PageFuture` と同じ規律。
/// borrow した引数を跨いで await する必要があるため、素の `impl Future` を返すジェネリック
/// `Fut` 型パラメータでは各呼び出しごとに異なる借用ライフタイムを表現できず、`Box<dyn Future>`
/// で型を固定してライフタイムだけを変えられるようにする）。
type SignalTraverseFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Vec<crate::proto::graphrag::NodeResult>>>
            + Send
            + 'a,
    >,
>;

/// `fetch`（実際は `VegapunkClient::traverse_neighbors_paged`）を注入できる形にした signal
/// traversal の内部ロジック（Issue #39）。`admin.rs::build_threads_page` / `PageFuture` と
/// 同じ規律で、実ネットワーク呼び出しをする薄いラッパー（`KnowledgeStore::load_case_signals` /
/// `load_known_resolutions`）とテスト可能な内部ロジックを分離する。
///
/// `node_type` / `edge_type` / `direction` / `page_size` は呼び出し元から受け取らず、
/// `SIGNAL_NODE_TYPE` / `SIGNAL_EDGE_TYPE` / `SIGNAL_TRAVERSE_DIRECTION` /
/// `SIGNAL_TRAVERSE_PAGE_SIZE` をこの関数の内部でのみ使う（レビュー W1/W2）。以前は
/// これら4値を呼び出し元が引数で渡していたため、テストが「自分で渡した値をフェイクが
/// 受け取ったこと」を assert するだけの同語反復になっていた。固定値化したことで、
/// テストがフェイクへ渡る値を検証すれば、それがそのままこの関数内部の定数の検証になる。
/// `fetch` を `FnOnce` にしているのは、現状の呼び出し元がいずれも 1 起点・1 回だけ呼ぶため。
async fn signals_from_traverse<'a, F>(
    schema: &'a str,
    source_node_id: &'a str,
    fetch: F,
) -> Result<SignalSet>
where
    F: FnOnce(&'a str, &'a str, &'a str, &'a str, &'a str, i32) -> SignalTraverseFuture<'a>,
{
    let results = fetch(
        schema,
        SIGNAL_NODE_TYPE,
        SIGNAL_EDGE_TYPE,
        SIGNAL_TRAVERSE_DIRECTION,
        source_node_id,
        SIGNAL_TRAVERSE_PAGE_SIZE,
    )
    .await?;
    Ok(signal_set_from_node_results(&results))
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

/// `query_nodes(KnownResolution)` の結果と、KR node_id → SignalSet の索引から
/// `KnownResolution` を組み立てる純関数（Issue #39）。`kr_signals` の作り方（snapshot 由来の
/// `load_known_resolutions_with` / traversal 由来の `load_known_resolutions`）に依存しない
/// 共通のマッピングロジックとして両者から呼ばれる。
fn known_resolutions_from_nodes(
    kr_nodes: Vec<crate::proto::graphrag::NodeResult>,
    mut kr_signals: HashMap<String, SignalSet>,
) -> Result<Vec<KnownResolution>> {
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

    /// `KIND_KNOWN_RESOLUTION` ノードを一括取得する共通ヘルパ（W3）。`query_nodes` の
    /// limit=1000 はページング未実装のためのサイレント切り詰め上限であり、
    /// `load_known_resolutions` / `load_known_resolutions_with` の両方がこの上限を持っていた
    /// （2箇所に増殖していた）。ここに集約し、上限ちょうどに達したときだけ warn する。
    /// ページング化自体は今回のスコープ外（KR 件数が現状小さく実害なしとレビュー済み）。
    async fn query_known_resolution_nodes(
        &self,
        schema: &str,
    ) -> Result<Vec<crate::proto::graphrag::NodeResult>> {
        const KNOWN_RESOLUTION_QUERY_LIMIT: i32 = 1000;
        let nodes = self
            .client
            .query_nodes(
                schema,
                KIND_KNOWN_RESOLUTION,
                Vec::new(),
                KNOWN_RESOLUTION_QUERY_LIMIT,
            )
            .await
            .context("load known resolutions")?;
        if nodes.len() == KNOWN_RESOLUTION_QUERY_LIMIT as usize {
            tracing::warn!(
                schema,
                count = nodes.len(),
                "load_known_resolutions: query_nodes hit the {KNOWN_RESOLUTION_QUERY_LIMIT}-node \
                 limit for KnownResolution; results may be silently truncated (pagination not \
                 yet implemented)"
            );
        }
        Ok(nodes)
    }

    /// `self.client.traverse_neighbors_paged` を呼ぶ唯一の箇所（Issue #39 レビュー残課題3）。
    /// `load_case_signals`（case ノード起点）と `load_known_resolutions`（KR ノード起点）が
    /// 個別に同一内容のクロージャを持っており、どちらのクロージャ自体もテストを経由していな
    /// かった（既存テストは `signals_from_traverse` へ直接フェイクを渡していた）。ここへ
    /// 集約し、両呼び出し元をこのメソッド経由に統一する。
    async fn signals_of(&self, schema: &str, source_node_id: &str) -> Result<SignalSet> {
        signals_from_traverse(
            schema,
            source_node_id,
            |schema, node_type, edge_type, direction, source_node_id, page_size| {
                Box::pin(self.client.traverse_neighbors_paged(
                    schema,
                    node_type,
                    edge_type,
                    direction,
                    source_node_id,
                    page_size,
                ))
            },
        )
        .await
    }

    /// KnownResolution を Signal ノード経由で復元する（HAS_SIGNAL 辺の走査）。
    ///
    /// Issue #39: 旧実装は `fetch_snapshot` で schema 全体を取得していたため、本番規模の
    /// schema では `guard_snapshot_complete` の fail closed に引っかかり、呼び出し元
    /// （`advisor::api::draft_with_materials` は warn ログを出して空 `Vec` に縮退、
    /// `rmcp_server.rs` の MCP tool `search_known_resolutions` はエラーをそのまま返す）の
    /// KR 照合がサイレントに常時失敗していた。KR ノードごとに `HAS_SIGNAL` 辺を 1-hop
    /// traversal する実装へ置き換え、snapshot 取得を経路から外す。
    pub async fn load_known_resolutions(&self, schema: &str) -> Result<Vec<KnownResolution>> {
        let kr_nodes = self.query_known_resolution_nodes(schema).await?;
        if kr_nodes.is_empty() {
            return Ok(Vec::new());
        }
        // KR ノードごとに逐次 traversal する（`corpus.rs::collect_incoming_edges` と同じ、
        // このコードベースの既存の流儀）。並列化・別軸取得（例: HAS_SIGNAL 辺をまとめて
        // 引く新規 RPC）は今回のスコープ外（KR 件数が現状小さく、N+1 の実害なしとレビュー
        // 済み。件数が増えた場合の将来の検討事項として残す）。
        let mut kr_signals: HashMap<String, SignalSet> = HashMap::new();
        for node in &kr_nodes {
            let signals = self
                .signals_of(schema, &node.node_id)
                .await
                .with_context(|| {
                    format!(
                        "load known resolution signals: kr_node_id={} schema={schema}",
                        node.node_id
                    )
                })?;
            kr_signals.insert(node.node_id.clone(), signals);
        }
        known_resolutions_from_nodes(kr_nodes, kr_signals)
    }

    /// 取得済み snapshot を使う変種（evaluate の hot path 用、ManualV1 の `live_corpus`
    /// または LegacySection の `fetch_snapshot` の呼び出し元が渡す snapshot を使う）。
    pub async fn load_known_resolutions_with(
        &self,
        schema: &str,
        snapshot: &crate::proto::graphrag::GetGraphSnapshotResponse,
    ) -> Result<Vec<KnownResolution>> {
        let kr_nodes = self.query_known_resolution_nodes(schema).await?;
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
        known_resolutions_from_nodes(kr_nodes, kr_signals)
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
    ///
    /// Issue #39: 旧実装は `fetch_snapshot`（`GetGraphSnapshot`、上限 `SNAPSHOT_MAX_NODES`）で
    /// schema 全体を取得していた。1 case の signal を読むだけなのに schema 全体が必要という
    /// 設計だったため、本番規模（`urtect`）ではノード数が上限を超え `guard_snapshot_complete`
    /// の fail closed に必ず引っかかり、`GET /admin/api/threads/{case_id}` が 500 になっていた。
    /// case ノードを起点に `HAS_SIGNAL` 辺だけを 1-hop traversal する実装へ置き換え、
    /// snapshot 取得を経路から完全に外す。
    pub async fn load_case_signals(&self, schema: &str, case_id: &str) -> Result<SignalSet> {
        let case_node_id = case_signal_node_id(schema, case_id);
        self.signals_of(schema, &case_node_id)
            .await
            .with_context(|| format!("load case signals: case_id={case_id} schema={schema}"))
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
        let case_node_id = case_signal_node_id(schema, case_id);
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
            // homesec advisor 固有加算（design doc `2026-08-17-homesec-advisor-design.md` §4.4）。
            // urtect の cs-schema.yml / cs-support.yml には advisor コードは存在しないが、
            // この一致テストは「コードが書き込むキー ⊆ schema 宣言キー」を全 schema ファイルに
            // 対して固定する方式のため、advisor が書く 3 属性も cs-schema.yml / cs-support.yml
            // 側に加算しておく（advisor 側の homesec.yml だけでなく、両ファイルとも一致テストの
            // 対象になっているため）。
            "lead_offered",
            "lead_requested",
            "shown_product_cards",
            // homesec advisor の累積条件（design doc §4.2）。advisor/decide.rs::
            // CONDITION_ATTR_KEYS が書き込む 5 キー。3 schema ファイル全部が同じ written
            // リストと突き合わされるため、上の 3 属性と同じ理由で 3 ファイルすべてに加算する。
            "advisor_cond_housing",
            "advisor_cond_target",
            "advisor_cond_concern",
            "advisor_cond_budget",
            "advisor_cond_install",
            // 会話のリズム改善（2026-08-21 conversation-rhythm-implementation §要件2）。
            // advisor/decide.rs::next_question_streak が更新する。3 schema ファイル全部が
            // 同じ written リストと突き合わされるため、上と同じ理由で 3 ファイルすべてに加算する。
            "question_streak",
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

    /// homesec advisor（Issue #34、`2026-08-17-homesec-advisor-design.md`）の schema。
    /// advisor は cs-schema.yml / cs-support.yml と同じ support_case 定義を複製したうえで
    /// lead_offered / lead_requested / shown_product_cards を持つため、同じ一致テストを
    /// homesec.yml にも適用する（cs_schema_yml_declares_... / cs_support_yml_declares_... と
    /// 同一パターン）。
    #[test]
    fn homesec_yml_declares_every_support_case_attribute_that_code_writes() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../schema/homesec.yml"
        ));
        let declared = declared_support_case_attribute_keys(path);
        let written = written_support_case_attribute_keys();
        let missing: Vec<&String> = written.difference(&declared).collect();
        assert!(
            missing.is_empty(),
            "schema/homesec.yml support_case.attributes is missing keys that code writes: \
             {missing:?}"
        );
    }

    // ---- schema/homesec.yml の node/edge type 網羅性（reviewer 一次レビュー指摘1・Critical）----
    //
    // homesec advisor 自身のパイプラインではなく、advisor にもマウントされる共有管理画面の
    // corrections 経路（design doc `2026-08-17-homesec-advisor-design.md` §3.2、
    // `admin.rs::create_correction` → `build_known_resolution_graph`）が書き込む node_type /
    // edge_type が、schema/homesec.yml に宣言されていることを固定する。この経路は常に
    // manual_schema = ManualV1（`config.homesec.toml`）かつ rationale_text が Some
    // （`create_correction` が `unwrap_or_else(default_correction_rationale_text)` で埋めるため）
    // で呼ばれるので、Rationale ノード / HAS_SIGNAL 辺 / BECAUSE 辺が必ず発生する。
    //
    // vegapunk 側が未宣言属性・型の書き込みを拒否しうる（上の C1 コメントと同じ障害クラス）
    // ため、「書き込み型 ⊆ 宣言型」をハードコードではなく実際の `build_known_resolution_graph`
    // 呼び出し結果で固定する。

    /// `schema_path` の `nodes:` トップレベルキー集合（宣言済みノード型）。
    /// `declared_support_case_attribute_keys` と同じパース方式（serde_yaml、cwd 非依存の
    /// 絶対パス前提）に合わせる。
    fn declared_node_types(schema_path: &std::path::Path) -> std::collections::HashSet<String> {
        let text = std::fs::read_to_string(schema_path)
            .unwrap_or_else(|e| panic!("read schema file {schema_path:?}: {e}"));
        let value: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|e| panic!("parse schema file {schema_path:?} as YAML: {e}"));
        let nodes = value["nodes"]
            .as_mapping()
            .unwrap_or_else(|| panic!("{schema_path:?}: nodes is not a mapping"));
        nodes
            .keys()
            .map(|k| {
                k.as_str()
                    .unwrap_or_else(|| panic!("{schema_path:?}: non-string node type key"))
                    .to_string()
            })
            .collect()
    }

    /// `schema_path` の `edges:` トップレベルキー集合（宣言済み辺型）。
    fn declared_edge_types(schema_path: &std::path::Path) -> std::collections::HashSet<String> {
        let text = std::fs::read_to_string(schema_path)
            .unwrap_or_else(|e| panic!("read schema file {schema_path:?}: {e}"));
        let value: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|e| panic!("parse schema file {schema_path:?} as YAML: {e}"));
        let edges = value["edges"]
            .as_mapping()
            .unwrap_or_else(|| panic!("{schema_path:?}: edges is not a mapping"));
        edges
            .keys()
            .map(|k| {
                k.as_str()
                    .unwrap_or_else(|| panic!("{schema_path:?}: non-string edge type key"))
                    .to_string()
            })
            .collect()
    }

    // ---- Issue #39: load_case_signals / load_known_resolutions を snapshot 非依存にする ----
    //
    // 本番 `urtect` schema はノード数が `SNAPSHOT_MAX_NODES`（5000）を超えており、1 case の
    // signal を読むためだけに `fetch_snapshot` でグラフ全体を取得する旧実装は
    // `guard_snapshot_complete` の fail closed に必ず引っかかって 500 になっていた
    // （`GET /admin/api/threads/{case_id}`）。case / KnownResolution ノードを起点に
    // `HAS_SIGNAL` 辺を traversal で 1-hop だけ辿る実装に置き換える。この節はその回帰防止。

    fn signal_node_result(node_id: &str, value: &str) -> crate::proto::graphrag::NodeResult {
        crate::proto::graphrag::NodeResult {
            node_id: node_id.to_string(),
            node_type: "Signal".to_string(),
            attributes: attrs(&[("value", value)]),
        }
    }

    #[test]
    fn signal_set_from_node_results_reads_the_value_attribute() {
        let results = vec![
            signal_node_result("urtect:gen1:Signal:power_failure", "power_failure"),
            signal_node_result("urtect:gen1:Signal:smoke", "smoke"),
        ];
        let signals = signal_set_from_node_results(&results);
        assert_eq!(
            signals,
            [Signal::new("power_failure"), Signal::new("smoke")]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn signal_set_from_node_results_ignores_missing_value_attribute() {
        // value 属性を欠いた NodeResult は黙って捨てる（`signal_value_index` の
        // snapshot 版と同じ欠損データの扱いに揃える）。
        let malformed = crate::proto::graphrag::NodeResult {
            node_id: "urtect:gen1:Signal:broken".to_string(),
            node_type: "Signal".to_string(),
            attributes: attrs(&[("not_value", "x")]),
        };
        let signals = signal_set_from_node_results(&[malformed]);
        assert!(signals.is_empty());
    }

    #[test]
    fn signal_set_from_node_results_empty_input_is_empty_set() {
        let signals = signal_set_from_node_results(&[]);
        assert!(signals.is_empty());
    }

    /// テスト専用ヘルパ: `snapshot` から「`edge_type=="HAS_SIGNAL"` かつ
    /// `from_id==source_node_id`」を満たす辺の `to_id` を集め、対応する `snapshot.nodes` から
    /// `NodeResult` 相当を機械的に組み立てる。意図的に `node_type` では絞り込まない
    /// （その絞り込みは production 側の `signal_set_from_node_results` が担う前提を
    /// このテストで検証するため）。traversal 側の入力を手書きの別配列で用意すると、
    /// snapshot 側フィルタ（`case_signals_from_snapshot`）だけを壊す変更を見逃すため、
    /// 両実装の入力を同じ snapshot fixture から導出することで検出力を持たせる
    /// （レビュー W1 後半）。本体コードには追加しない、テスト専用の関数。
    fn derive_traverse_results_from_snapshot(
        snapshot: &crate::proto::graphrag::GetGraphSnapshotResponse,
        source_node_id: &str,
    ) -> Vec<crate::proto::graphrag::NodeResult> {
        let node_index: HashMap<&str, &crate::proto::graphrag::GraphNode> = snapshot
            .nodes
            .iter()
            .map(|n| (n.node_id.as_str(), n))
            .collect();
        snapshot
            .edges
            .iter()
            .filter(|e| e.edge_type == "HAS_SIGNAL" && e.from_id == source_node_id)
            .filter_map(|e| node_index.get(e.to_id.as_str()))
            .map(|n| crate::proto::graphrag::NodeResult {
                node_id: n.node_id.clone(),
                node_type: n.node_type.clone(),
                attributes: n.attributes.clone(),
            })
            .collect()
    }

    /// 等価性テスト: 1つの snapshot fixture に3種のノイズ（別 case からの HAS_SIGNAL 辺・
    /// 対象 case からの HAS_SIGNAL 以外の辺・HAS_SIGNAL の先が Signal ではないノード）を
    /// 混ぜ、旧実装が使っていた `case_signals_from_snapshot`（snapshot 経由）と、新しい
    /// traversal 経由の純関数 `signal_set_from_node_results` が同じ `SignalSet` を返す
    /// ことを固定する。traversal 側の入力は `derive_traverse_results_from_snapshot` で
    /// 同じ fixture から機械的に導出するため、`case_signals_from_snapshot` の3フィルタ
    /// （`edge_type=="HAS_SIGNAL"` / `from_id==case_node_id` / `node_type=="Signal"`）の
    /// どれを壊してもこのテストが検出する（レビュー W1 後半。以前は traversal 側を
    /// 手書きの別配列で用意しており、このいずれのフィルタが壊れても検出できなかった）。
    #[test]
    fn traversal_and_snapshot_paths_agree_on_the_same_case_signal_edges() {
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge, GraphNode as ProtoNode};

        let schema = "urtect";
        let case_id = "case-1";
        let case_node_id = harness_node_id(schema, "support_case", case_id);
        let other_case_node_id = harness_node_id(schema, "support_case", "case-2");
        let signal_a_id = harness_node_id(schema, "Signal", "power_failure");
        let signal_b_id = harness_node_id(schema, "Signal", "smoke");
        let wrong_case_signal_id =
            harness_node_id(schema, "Signal", "should_be_excluded_wrong_case");
        let wrong_edge_signal_id =
            harness_node_id(schema, "Signal", "should_be_excluded_wrong_edge_type");
        let non_signal_node_id =
            harness_node_id(schema, "product", "should_be_excluded_non_signal_node");

        let mk_node = |node_id: &str, node_type: &str, pairs: &[(&str, &str)]| ProtoNode {
            node_id: node_id.to_string(),
            node_type: node_type.to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: attrs(pairs),
        };

        let snapshot = GetGraphSnapshotResponse {
            nodes: vec![
                mk_node(&case_node_id, "support_case", &[("case_id", case_id)]),
                mk_node(
                    &other_case_node_id,
                    "support_case",
                    &[("case_id", "case-2")],
                ),
                mk_node(&signal_a_id, "Signal", &[("value", "power_failure")]),
                mk_node(&signal_b_id, "Signal", &[("value", "smoke")]),
                mk_node(
                    &wrong_case_signal_id,
                    "Signal",
                    &[("value", "should_be_excluded_wrong_case")],
                ),
                mk_node(
                    &wrong_edge_signal_id,
                    "Signal",
                    &[("value", "should_be_excluded_wrong_edge_type")],
                ),
                mk_node(
                    &non_signal_node_id,
                    "product",
                    &[("value", "should_be_excluded_non_signal_node")],
                ),
            ],
            edges: vec![
                GraphEdge {
                    edge_id: String::new(),
                    from_id: case_node_id.clone(),
                    to_id: signal_a_id.clone(),
                    edge_type: "HAS_SIGNAL".to_string(),
                },
                GraphEdge {
                    edge_id: String::new(),
                    from_id: case_node_id.clone(),
                    to_id: signal_b_id.clone(),
                    edge_type: "HAS_SIGNAL".to_string(),
                },
                // ノイズ1: 別 case からの HAS_SIGNAL 辺（`from_id` が対象 case ではない）。
                GraphEdge {
                    edge_id: String::new(),
                    from_id: other_case_node_id,
                    to_id: wrong_case_signal_id,
                    edge_type: "HAS_SIGNAL".to_string(),
                },
                // ノイズ2: 対象 case からの辺だが HAS_SIGNAL ではない。
                GraphEdge {
                    edge_id: String::new(),
                    from_id: case_node_id.clone(),
                    to_id: wrong_edge_signal_id,
                    edge_type: "MENTIONS".to_string(),
                },
                // ノイズ3: 対象 case からの HAS_SIGNAL 辺だが、先が Signal ノードではない。
                GraphEdge {
                    edge_id: String::new(),
                    from_id: case_node_id.clone(),
                    to_id: non_signal_node_id,
                    edge_type: "HAS_SIGNAL".to_string(),
                },
            ],
            truncated: false,
            total_node_count: 7,
        };

        let expected: SignalSet = [Signal::new("power_failure"), Signal::new("smoke")]
            .into_iter()
            .collect();

        let from_snapshot = case_signals_from_snapshot(schema, case_id, &snapshot);
        assert_eq!(
            from_snapshot, expected,
            "sanity: fixture のノイズが snapshot 側の結果に混入していない"
        );

        let derived_nodes = derive_traverse_results_from_snapshot(&snapshot, &case_node_id);
        let from_traversal = signal_set_from_node_results(&derived_nodes);

        assert_eq!(from_snapshot, from_traversal);
    }

    /// 再発防止テスト: `load_case_signals` の内部注入可能関数 `signals_from_traverse` が
    /// `GetGraphSnapshot`/`fetch_snapshot` ではなく traversal 系（`traverse_neighbors_paged`
    /// 相当のフェイク）を呼ぶことを固定する。フェイクへ渡る node_type・edge_type・direction・
    /// page_size は呼び出し元が渡した値ではなく `signals_from_traverse` 内部の
    /// `SIGNAL_NODE_TYPE` / `SIGNAL_EDGE_TYPE` / `SIGNAL_TRAVERSE_DIRECTION` /
    /// `SIGNAL_TRAVERSE_PAGE_SIZE` から来る（呼び出し側は `schema` と `source_node_id` しか
    /// 渡せないシグネチャになった、レビュー W1/W2）。フェイク内の assert は、これら定数の
    /// シンボルではなく期待値をリテラルで書く。定数シンボル同士の比較（
    /// `assert_eq!(node_type, SIGNAL_NODE_TYPE)` 等）だと、production 側の定数の値を
    /// 書き換えてもテストが参照する側も同じ定数を経由するため常に一致してしまい、値の
    /// 変更を一切検出できない（Issue #39 レビュー残課題1）。リテラル比較にすることで、
    /// このテストは「正しい値は何か」を自身の中で独立に主張し、production 定数の書き換えを
    /// 検出できる。このテストのフェイクは `Result<Vec<NodeResult>>` しか返せない型を
    /// 要求されるため、`GetGraphSnapshotResponse` はこのテストのコードパスに一切登場しない
    /// （＝構造的に snapshot 取得が起き得ないことの保証）。
    #[tokio::test]
    async fn load_case_signals_calls_traverse_with_expected_arguments_not_snapshot() {
        let case_node_id = harness_node_id("urtect", "support_case", "case-1");
        let expected_source = case_node_id.clone();
        let results = vec![signal_node_result(
            &harness_node_id("urtect", "Signal", "power_failure"),
            "power_failure",
        )];

        let signals = signals_from_traverse(
            "urtect",
            &case_node_id,
            |schema, node_type, edge_type, direction, source_node_id, page_size| {
                // フェイクは受け取った引数を検証してから、実 vegapunk 呼び出し無しで
                // 結果を返す。「outgoing」であること（`append_case_signals` が
                // `from_id: case_node_id, to_id: signal_node_id` で書くため、case が
                // 辺の起点）が特に重要（"out" ではない）。
                //
                // 期待値はリテラルで書く（`SIGNAL_NODE_TYPE` 等の定数シンボルと比較しない）。
                // 定数同士の比較だと、production 側の定数の値を書き換えても、その定数を
                // 経由して読んだ側の値まで一緒に書き換わるため常に一致してしまい、
                // 値の変更を一切検出できない再発防止テストになる（Issue #39 レビュー残課題1）。
                assert_eq!(schema, "urtect");
                assert_eq!(node_type, "Signal");
                assert_eq!(edge_type, "HAS_SIGNAL");
                assert_eq!(direction, "outgoing");
                assert_eq!(source_node_id, expected_source);
                assert_eq!(page_size, 1000);
                let results = results.clone();
                Box::pin(async move { Ok(results) })
            },
        )
        .await
        .expect("builds signal set from the fake traversal result");

        assert_eq!(
            signals,
            [Signal::new("power_failure")].into_iter().collect()
        );
    }

    #[tokio::test]
    async fn load_case_signals_traversal_propagates_fetch_errors() {
        // フェイクが失敗を返した場合、`load_case_signals` 相当の呼び出し元へエラーが
        // そのまま伝播すること（握りつぶさない）。
        let case_node_id = harness_node_id("urtect", "support_case", "case-1");
        let result = signals_from_traverse("urtect", &case_node_id, |_, _, _, _, _, _| {
            Box::pin(async move { Err(anyhow!("vegapunk unavailable")) })
        })
        .await;
        assert!(result.is_err());
    }

    /// 再発防止テスト: `case_signal_node_id` は `load_case_signals`（読み取り、`signals_of`
    /// 経由）と `append_case_signals`（書き込み）の両方が case ノード id の組み立てに使う
    /// 唯一のヘルパである（Issue #39 レビュー残課題3）。以前はこの2箇所が個別に
    /// `harness_node_id(schema, "support_case", case_id)` を書いており、片方だけ kind 文字列
    /// （"support_case"）がずれても検出できなかった。両呼び出し元は本テストではなく
    /// `case_signal_node_id` を共有することで構造的に一致するが、ここでは
    /// `case_signal_node_id` 自体が期待どおり `harness_node_id` へ委譲していることを
    /// pin しておく（このヘルパの契約が壊れれば読み取り・書き込みの両方が同時に壊れる）。
    #[test]
    fn case_signal_node_id_matches_between_read_and_write_paths() {
        let schema = "urtect";
        let case_id = "case-1";
        assert_eq!(
            case_signal_node_id(schema, case_id),
            harness_node_id(schema, "support_case", case_id)
        );
    }

    /// 変更2の軽量テスト: KnownResolution ノード → Signal traversal → SignalSet が、
    /// 純関数レベルで `KnownResolution.signal_set` に正しく反映されることを確認する
    /// （`load_known_resolutions` 内の kr_signals 組み立てと同じ経路）。
    #[test]
    fn known_resolutions_from_nodes_reflects_traversal_derived_signal_set() {
        let kr_node = crate::proto::graphrag::NodeResult {
            node_id: "urtect:gen1:KnownResolution:kr-1".to_string(),
            node_type: "KnownResolution".to_string(),
            attributes: attrs(&[
                ("kr_id", "kr-1"),
                ("answer_text", "電源ケーブルをご確認ください。"),
                ("applicability", "全モデル"),
            ]),
        };
        // kr ノードごとの traverse_neighbors_paged(schema, "Signal", "HAS_SIGNAL",
        // "outgoing", kr_node.node_id, ...) が返す想定の NodeResult 列。
        let traverse_results = vec![signal_node_result(
            "urtect:gen1:Signal:power_failure",
            "power_failure",
        )];
        let mut kr_signals: HashMap<String, SignalSet> = HashMap::new();
        kr_signals.insert(
            kr_node.node_id.clone(),
            signal_set_from_node_results(&traverse_results),
        );

        let resolutions = known_resolutions_from_nodes(vec![kr_node], kr_signals).expect("builds");

        assert_eq!(resolutions.len(), 1);
        assert_eq!(
            resolutions[0].signal_set,
            [Signal::new("power_failure")].into_iter().collect()
        );
    }

    #[test]
    fn known_resolutions_from_nodes_empty_signal_set_when_kr_has_no_signal_edges() {
        // 走査で HAS_SIGNAL 辺が 0 本だった KR（kr_signals に対応エントリが無い）は
        // 空の SignalSet になる（既存の snapshot 版と同じ `unwrap_or_default` の挙動）。
        let kr_node = crate::proto::graphrag::NodeResult {
            node_id: "urtect:gen1:KnownResolution:kr-2".to_string(),
            node_type: "KnownResolution".to_string(),
            attributes: attrs(&[
                ("kr_id", "kr-2"),
                ("answer_text", "x"),
                ("applicability", "y"),
            ]),
        };
        let resolutions =
            known_resolutions_from_nodes(vec![kr_node], HashMap::new()).expect("builds");
        assert_eq!(resolutions.len(), 1);
        assert!(resolutions[0].signal_set.is_empty());
    }

    #[test]
    fn homesec_yml_declares_every_node_and_edge_type_that_admin_corrections_write() {
        use crate::harness::signal::Signal;
        // admin corrections 経路と同じ入力形: signal_set 非空・rationale_text: Some(..)・
        // manual_section_keys 空（`create_correction` は常に `&[]` を渡すため BASED_ON /
        // ManualSection はこの経路で発生しない。指摘1の事実関係のとおり）。
        let new_kr = NewKnownResolution {
            signal_set: [Signal::new("sd_not_recognized")].into_iter().collect(),
            applicability: "全モデル".to_string(),
            answer: "回答文".to_string(),
            origin: "manual".to_string(),
            created_by: "sup-001".to_string(),
            created_by_email: "sup-001@sivira.co".to_string(),
            rationale_text: Some("判断理由".to_string()),
            manual_section_keys: vec![],
        };
        let build = build_known_resolution_graph(
            "homesec",
            "kr-1",
            &new_kr,
            crate::config::ManualSchemaKind::ManualV1,
        );

        let written_node_types: std::collections::HashSet<String> =
            build.nodes.iter().map(|n| n.node_type.clone()).collect();
        let written_edge_types: std::collections::HashSet<String> =
            build.edges.iter().map(|e| e.edge_type.clone()).collect();

        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../schema/homesec.yml"
        ));
        let declared_nodes = declared_node_types(path);
        let declared_edges = declared_edge_types(path);

        let missing_nodes: Vec<&String> = written_node_types.difference(&declared_nodes).collect();
        assert!(
            missing_nodes.is_empty(),
            "schema/homesec.yml nodes is missing node types that the shared admin corrections \
             path (admin.rs::create_correction) writes: {missing_nodes:?}"
        );
        let missing_edges: Vec<&String> = written_edge_types.difference(&declared_edges).collect();
        assert!(
            missing_edges.is_empty(),
            "schema/homesec.yml edges is missing edge types that the shared admin corrections \
             path (admin.rs::create_correction) writes: {missing_edges:?}"
        );
    }
}
