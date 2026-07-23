//! スケーラブルな graph corpus ローダ。
//!
//! 背景: `GetGraphSnapshot` は `max_nodes` に backend 側ハード上限（実測 5000）があり、
//! 数万ノード規模の schema では truncate する。truncate した snapshot に依存する読み取り
//! （search_manual / evaluate の KR・case 復元）は、辺の欠落で fail-open もしくは fail-closed
//! （不完全データ拒否）に倒れ、機能停止する。
//!
//! ここでは `graph_snapshot(5000)` を撤去し、既存の純関数（`search_with_snapshot` /
//! `product_view_from_snapshot` / `load_known_resolutions_with` / `case_signals_from_snapshot` /
//! `search_cases_from_snapshot`）が食う `GetGraphSnapshotResponse` 相当の corpus を、
//! グラフ規模に依存しない読み取りだけで組み立てる:
//!
//! - ノード: `query_nodes_paged`（offset ページングで全件、上限なし）
//! - 辺: **低カーディナリティ端点からのページング traverse**。数万ある ManualSection / support_case
//!   側からではなく、語彙で上限が決まる Signal や小規模な Product 側から incoming で辿る。
//!   これにより辺の走査回数が `|Signal| + |Product|` に比例し、グラフ全体規模に依存しない。
//!
//! ## キャッシュ境界（安全性の要）
//!
//! corpus を 2 種に分ける:
//!
//! - [`CorpusLoader::manual_corpus`] — ingest（別プロセス）だけが書く材料
//!   （ManualSection / Product / Signal ノード、MENTIONS_SIGNAL / DESCRIBES 辺）。
//!   **schema 単位 TTL キャッシュ（既定 60s）**。ingest からの invalidation 通知は無いため
//!   TTL ベースで、最大 TTL 秒の staleness を許容する。
//! - [`CorpusLoader::live_corpus`] — サーバが会話中に書く材料
//!   （support_case / Signal ノード、HAS_SIGNAL 辺）。**キャッシュしない**。
//!   会話層の累積 signal（`case_signals_from_snapshot`）を毎ターン最新で読めないと、
//!   前ターンで積んだ signal を取りこぼしてエスカレーション条件が成立せず fail-open するため、
//!   ここは必ず都度取得する。support_case / Signal は件数が小さく都度取得で足りる。

use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge, GraphNode, NodeResult};
use crate::vegapunk::VegapunkClient;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// query_nodes / traverse のページサイズ。backend 上限は 1000。
const PAGE_SIZE: i32 = 1000;

/// manual_corpus キャッシュの既定 TTL。
const DEFAULT_TTL: Duration = Duration::from_secs(60);

// corpus が touch するノード種別・辺種別。文字列リテラルの散在を防ぐ。
const NODE_MANUAL_SECTION: &str = "ManualSection";
const NODE_PRODUCT: &str = "Product";
const NODE_SIGNAL: &str = "Signal";
const NODE_SUPPORT_CASE: &str = "support_case";
const NODE_KNOWN_RESOLUTION: &str = "KnownResolution";
const EDGE_MENTIONS_SIGNAL: &str = "MENTIONS_SIGNAL";
const EDGE_DESCRIBES: &str = "DESCRIBES";
const EDGE_HAS_SIGNAL: &str = "HAS_SIGNAL";

struct CachedCorpus {
    stored_at: Instant,
    corpus: Arc<GetGraphSnapshotResponse>,
}

/// 材料 corpus を組み立てて配る共有ローダ。read 経路（ManualStore）と評価経路（Harness::evaluate）で
/// 同一インスタンスを共有し、`manual_corpus` の TTL キャッシュを両者で使い回す。
pub struct CorpusLoader {
    client: Arc<VegapunkClient>,
    /// schema -> 直近取得した manual_corpus。`live_corpus` はキャッシュしない。
    manual_cache: Mutex<HashMap<String, CachedCorpus>>,
    ttl: Duration,
}

impl CorpusLoader {
    pub fn new(client: Arc<VegapunkClient>) -> Self {
        Self::with_ttl(client, DEFAULT_TTL)
    }

    pub fn with_ttl(client: Arc<VegapunkClient>, ttl: Duration) -> Self {
        Self {
            client,
            manual_cache: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// ingest 所有の材料（ManualSection / Product / Signal ノード + MENTIONS_SIGNAL / DESCRIBES 辺）。
    /// TTL 内はキャッシュを返す。数千ノードの再ページングを毎検索で走らせないための最適化。
    pub async fn manual_corpus(&self, schema: &str) -> Result<Arc<GetGraphSnapshotResponse>> {
        if let Some(hit) = self.cached_manual(schema) {
            return Ok(hit);
        }
        let corpus = Arc::new(self.load_manual_corpus(schema).await?);
        self.store_manual(schema, corpus.clone());
        Ok(corpus)
    }

    fn cached_manual(&self, schema: &str) -> Option<Arc<GetGraphSnapshotResponse>> {
        let guard = self
            .manual_cache
            .lock()
            .expect("manual corpus cache poisoned");
        guard
            .get(schema)
            .filter(|c| is_fresh(c.stored_at.elapsed(), self.ttl))
            .map(|c| c.corpus.clone())
    }

    fn store_manual(&self, schema: &str, corpus: Arc<GetGraphSnapshotResponse>) {
        let mut guard = self
            .manual_cache
            .lock()
            .expect("manual corpus cache poisoned");
        guard.insert(
            schema.to_string(),
            CachedCorpus {
                stored_at: Instant::now(),
                corpus,
            },
        );
    }

    async fn load_manual_corpus(&self, schema: &str) -> Result<GetGraphSnapshotResponse> {
        // ノード全件（ページング・上限なし）。
        let sections = self.load_nodes(schema, NODE_MANUAL_SECTION).await?;
        let products = self.load_nodes(schema, NODE_PRODUCT).await?;
        let signals = self.load_nodes(schema, NODE_SIGNAL).await?;

        // 辺: 低カーディナリティ端点（Signal / Product）から incoming traverse で復元する。
        let mut edges: Vec<GraphEdge> = Vec::new();
        // MENTIONS_SIGNAL: ManualSection -> Signal。Signal から incoming で from(=section) を集める。
        self.collect_incoming_edges(
            schema,
            &signals,
            NODE_MANUAL_SECTION,
            EDGE_MENTIONS_SIGNAL,
            &mut edges,
        )
        .await?;
        // DESCRIBES: ManualSection -> Product。Product から incoming で from(=section) を集める。
        self.collect_incoming_edges(
            schema,
            &products,
            NODE_MANUAL_SECTION,
            EDGE_DESCRIBES,
            &mut edges,
        )
        .await?;

        let mut nodes = sections;
        nodes.extend(products);
        nodes.extend(signals);
        Ok(GetGraphSnapshotResponse {
            nodes,
            edges,
            truncated: false,
            total_node_count: 0,
        })
    }

    /// サーバが会話中に書く材料（support_case / Signal ノード + HAS_SIGNAL 辺）。キャッシュしない。
    /// HAS_SIGNAL の from は KnownResolution と support_case の両種があるため、Signal から
    /// 両 neighbor 種別に対して incoming traverse する。
    pub async fn live_corpus(&self, schema: &str) -> Result<GetGraphSnapshotResponse> {
        let cases = self.load_nodes(schema, NODE_SUPPORT_CASE).await?;
        let signals = self.load_nodes(schema, NODE_SIGNAL).await?;

        let mut edges: Vec<GraphEdge> = Vec::new();
        // HAS_SIGNAL: {KnownResolution, support_case} -> Signal。両 from 種別を Signal から incoming で辿る。
        self.collect_incoming_edges(
            schema,
            &signals,
            NODE_KNOWN_RESOLUTION,
            EDGE_HAS_SIGNAL,
            &mut edges,
        )
        .await?;
        self.collect_incoming_edges(
            schema,
            &signals,
            NODE_SUPPORT_CASE,
            EDGE_HAS_SIGNAL,
            &mut edges,
        )
        .await?;

        let mut nodes = cases;
        nodes.extend(signals);
        Ok(GetGraphSnapshotResponse {
            nodes,
            edges,
            truncated: false,
            total_node_count: 0,
        })
    }

    /// 1 ノード種別を offset ページングで全件ロードし、proto GraphNode へ変換する。
    async fn load_nodes(&self, schema: &str, node_type: &str) -> Result<Vec<GraphNode>> {
        let rows = self
            .client
            .query_nodes_paged(schema, node_type, Vec::new(), PAGE_SIZE)
            .await
            .with_context(|| format!("load {node_type} nodes for corpus"))?;
        Ok(rows.into_iter().map(node_from_result).collect())
    }

    /// `endpoints` の各ノードから `edge_type` を incoming に 1-hop traverse し、
    /// neighbor(=from) → endpoint(=to) の辺を `out` に積む。traverse はページングで全件取り切る。
    async fn collect_incoming_edges(
        &self,
        schema: &str,
        endpoints: &[GraphNode],
        neighbor_node_type: &str,
        edge_type: &str,
        out: &mut Vec<GraphEdge>,
    ) -> Result<()> {
        for endpoint in endpoints {
            let from_ids = self
                .client
                .traverse_neighbor_ids(
                    schema,
                    neighbor_node_type,
                    edge_type,
                    "incoming",
                    &endpoint.node_id,
                    PAGE_SIZE,
                )
                .await
                .with_context(|| {
                    format!(
                        "traverse {edge_type} incoming into {} for corpus",
                        endpoint.node_id
                    )
                })?;
            out.extend(incoming_edges(&endpoint.node_id, from_ids, edge_type));
        }
        Ok(())
    }
}

/// TTL キャッシュの鮮度判定。取得からの経過 `age` が `ttl` 未満なら fresh。
/// 境界（`age == ttl`）は stale 扱い（`<`）にして、期限ちょうどで確実に再取得させる。
fn is_fresh(age: Duration, ttl: Duration) -> bool {
    age < ttl
}

/// `NodeResult`（query_nodes / traverse の返り）を snapshot 用 proto `GraphNode` へ変換する。
/// display_text / degree / community は snapshot 消費側が読まないため既定値で埋める。
fn node_from_result(n: NodeResult) -> GraphNode {
    GraphNode {
        node_id: n.node_id,
        node_type: n.node_type,
        display_text: String::new(),
        degree: 0,
        community: None,
        attributes: n.attributes,
    }
}

/// incoming traverse で得た from_id 群を `from -> endpoint` の GraphEdge 列にする。
/// endpoint が traverse の起点(=to_id)、neighbor が from_id。edge_id は snapshot 消費側が
/// 見ないため空。
fn incoming_edges(endpoint_id: &str, from_ids: Vec<String>, edge_type: &str) -> Vec<GraphEdge> {
    from_ids
        .into_iter()
        .map(|from_id| GraphEdge {
            edge_id: String::new(),
            from_id,
            to_id: endpoint_id.to_string(),
            edge_type: edge_type.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_fresh_is_true_below_ttl_and_false_at_or_beyond() {
        let ttl = Duration::from_secs(60);
        assert!(is_fresh(Duration::from_secs(0), ttl));
        assert!(is_fresh(Duration::from_secs(59), ttl));
        // 期限ちょうどは stale（再取得させる）。
        assert!(!is_fresh(Duration::from_secs(60), ttl));
        assert!(!is_fresh(Duration::from_secs(120), ttl));
    }

    #[test]
    fn node_from_result_preserves_id_type_and_attributes() {
        let n = NodeResult {
            node_id: "urtect:gen1:Signal:sd_not_recognized".to_string(),
            node_type: "Signal".to_string(),
            attributes: [("value".to_string(), "sd_not_recognized".to_string())]
                .into_iter()
                .collect(),
        };
        let g = node_from_result(n);
        assert_eq!(g.node_id, "urtect:gen1:Signal:sd_not_recognized");
        assert_eq!(g.node_type, "Signal");
        assert_eq!(
            g.attributes.get("value").map(String::as_str),
            Some("sd_not_recognized")
        );
    }

    #[test]
    fn incoming_edges_point_from_neighbor_to_endpoint() {
        // Signal を端点に MENTIONS_SIGNAL を incoming で辿った結果は section -> signal の辺になる。
        let edges = incoming_edges(
            "urtect:gen1:Signal:sd_not_recognized",
            vec![
                "urtect:gen1:ManualSection:sec-a".to_string(),
                "urtect:gen1:ManualSection:sec-b".to_string(),
            ],
            EDGE_MENTIONS_SIGNAL,
        );
        assert_eq!(edges.len(), 2);
        for e in &edges {
            assert_eq!(e.edge_type, "MENTIONS_SIGNAL");
            assert_eq!(e.to_id, "urtect:gen1:Signal:sd_not_recognized");
            assert!(e.from_id.contains("ManualSection"));
        }
        assert_eq!(edges[0].from_id, "urtect:gen1:ManualSection:sec-a");
    }

    #[test]
    fn incoming_edges_empty_when_no_neighbors() {
        let edges = incoming_edges("urtect:gen1:Product:ADC-V724", Vec::new(), EDGE_DESCRIBES);
        assert!(edges.is_empty());
    }
}
