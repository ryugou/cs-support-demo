use crate::{
    model::{GraphBuild, GraphEdge, GraphNode},
    proto::graphrag::{
        create_schema_request, graph_rag_engine_client::GraphRagEngineClient, AttributeFilter,
        CreateSchemaRequest, Edge, EdgeTraversal, EmbedRequest, GetGraphSnapshotRequest,
        GetSchemaRequest, GetStatsRequest, GetStatsResponse, ListJobsRequest, ListJobsResponse,
        MergeRequest, Node, NodeAttribute, QueryNodesRequest, SearchDegradation, SearchExecution,
        SearchRequest, SearchResultItem, UpdateSchemaRequest, UpsertEdgesRequest,
        UpsertNodesRequest, UpsertVectorsRequest, VectorEntry,
    },
};
use anyhow::{Context, Result};
use std::time::Duration;
use tonic::{
    metadata::MetadataValue,
    transport::{Channel, Endpoint},
    Code, Request,
};

/// `upsert_vectors` に渡す 1 ベクトル分の投入単位: `(node_id, vector, metadata)`。
/// metadata は proto の `map<string, string>` に落ちる `(key, value)` 群。
/// `vector_entry` ヘルパの戻り値・各 ingest CLI の組み立てバッファと型を共有し、
/// 同じ 3 段ネストのタプルが複数箇所に散らばる（clippy::type_complexity）のを避ける。
pub type VectorUpsertEntry = (String, Vec<f32>, Vec<(String, String)>);

/// `Search` の応答から、プロダクトが使う 2 つを取り出したもの。
/// `execution` は degrade（Merge 未実行で global が落ちた等）の可視化に使う。
pub struct SearchOutcome {
    pub results: Vec<SearchResultItem>,
    pub execution: Option<SearchExecution>,
}

#[derive(Clone)]
pub struct VegapunkClient {
    inner: GraphRagEngineClient<Channel>,
    auth_header: MetadataValue<tonic::metadata::Ascii>,
}

/// h2 PING keepalive の設定。**この既定は常駐サーバ向け**で、
/// 「長寿命チャネルがアイドル後に死んだ接続を掴んだまま 120s ハングする」事象への対策
/// として入っている（詳細は `connect_lazy_with_limits` のコメント）。
///
/// 一方、サーバが応答を返さないまま長時間走る同期 RPC（`Merge`）では、この死活検知が
/// **正常な処理を誤検知で切断する**。そのため `GrpcLimits.keep_alive = None` で
/// 無効化できるようにしてある。無効化の判断は呼び出し側（現状 merge_schema CLI のみ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepAlive {
    /// h2 PING を送る間隔。
    pub interval: Duration,
    /// PING 応答の待ち時間。超えると接続を切って再接続させる。
    pub timeout: Duration,
    /// 進行中のリクエストが無いときも PING を送るか。
    pub while_idle: bool,
}

impl Default for KeepAlive {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
            while_idle: true,
        }
    }
}

/// gRPC channel の運用上限。マニュアル本文込みの graph snapshot が tonic 既定の
/// 4MiB decode 上限・短い timeout を超えるため、既定を引き上げている
/// （URTECT 実測 ~6MB / snapshot 読みで 30s 超）。環境ごとに締められるよう
/// config（vegapunk_timeout_secs / vegapunk_max_decode_mb）から上書き可能。
///
/// `keep_alive` は `None` で h2 PING keepalive を一切設定しない（TCP keepalive は残る）。
/// `Default` は常駐サーバの現行挙動そのままなので、既定を変えないこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrpcLimits {
    pub timeout_secs: u64,
    pub max_decode_bytes: usize,
    pub keep_alive: Option<KeepAlive>,
}

impl Default for GrpcLimits {
    fn default() -> Self {
        Self {
            timeout_secs: 120,
            max_decode_bytes: 64 * 1024 * 1024,
            keep_alive: Some(KeepAlive::default()),
        }
    }
}

/// `connect_lazy_with_limits` / `connect_with_limits` が共有する Endpoint 組み立て。
/// 片方だけ設定が漏れると「lazy 接続だけ挙動が違う」という追いにくい差になるため、
/// 1 箇所に閉じている。
fn build_endpoint(endpoint: &str, limits: GrpcLimits) -> Result<Endpoint> {
    let mut builder = Endpoint::from_shared(endpoint.to_string())?
        // connect_timeout は 30s。本番規模で TCP+TLS+h2 の確立が 10s では間に合わず
        // 常駐サーバのコールドな最初の 1 発が connect timeout に化ける事象への余裕。
        // 呼び出し自体の上限は別途 `timeout(limits.timeout_secs)` が握る。
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(limits.timeout_secs))
        // TCP keepalive は h2 keepalive の有無に関わらず常に張る。h2 を切った呼び出し側でも
        // OS 層の死活検知だけは残しておく。
        .tcp_keepalive(Some(Duration::from_secs(60)));
    // 長寿命チャネルがアイドル後に死んだ接続を掴んだまま 120s ハングする事象への対策
    // （実測: 新規接続の grpcurl は常に高速なのに、常駐サーバの呼び出しだけ停滞する）。
    // h2 PING keepalive で死活を検知し、切断時は再接続させる。
    // `None` の呼び出し側は、応答を返さない長時間 RPC を誤検知で切られるのを避けている。
    if let Some(keep_alive) = limits.keep_alive {
        builder = builder
            .http2_keep_alive_interval(keep_alive.interval)
            .keep_alive_timeout(keep_alive.timeout)
            .keep_alive_while_idle(keep_alive.while_idle);
    }
    Ok(builder)
}

impl VegapunkClient {
    pub fn connect_lazy(endpoint: &str, bearer_token: &str) -> Result<Self> {
        Self::connect_lazy_with_limits(endpoint, bearer_token, GrpcLimits::default())
    }

    pub fn connect_lazy_with_limits(
        endpoint: &str,
        bearer_token: &str,
        limits: GrpcLimits,
    ) -> Result<Self> {
        let channel = build_endpoint(endpoint, limits)?.connect_lazy();
        let auth_header = MetadataValue::try_from(format!("Bearer {bearer_token}"))
            .context("invalid bearer token metadata")?;
        Ok(Self {
            inner: GraphRagEngineClient::new(channel)
                .max_decoding_message_size(limits.max_decode_bytes),
            auth_header,
        })
    }

    pub async fn connect(endpoint: &str, bearer_token: &str) -> Result<Self> {
        Self::connect_with_limits(endpoint, bearer_token, GrpcLimits::default()).await
    }

    pub async fn connect_with_limits(
        endpoint: &str,
        bearer_token: &str,
        limits: GrpcLimits,
    ) -> Result<Self> {
        let channel = build_endpoint(endpoint, limits)?
            .connect()
            .await
            .with_context(|| format!("connect vegapunk endpoint {endpoint}"))?;
        let auth_header = MetadataValue::try_from(format!("Bearer {bearer_token}"))
            .context("invalid bearer token metadata")?;
        Ok(Self {
            inner: GraphRagEngineClient::new(channel)
                .max_decoding_message_size(limits.max_decode_bytes),
            auth_header,
        })
    }

    pub async fn create_or_update_schema(&self, name: &str, schema_yaml: String) -> Result<()> {
        match self.get_schema(name).await {
            Ok(Some(current)) if current == schema_yaml => Ok(()),
            Ok(Some(_)) => {
                let req = UpdateSchemaRequest {
                    name: name.to_string(),
                    schema_yaml,
                    dry_run: false,
                };
                self.call(|mut client, request| async move {
                    client.update_schema(request).await.map(|_| ())
                }, req)
                .await
                .context("update schema")
            }
            Ok(None) => self.create_schema(name, schema_yaml).await,
            Err(err) => Err(err),
        }
    }

    async fn create_schema(&self, name: &str, schema_yaml: String) -> Result<()> {
        let req = CreateSchemaRequest {
            name: name.to_string(),
            source: Some(create_schema_request::Source::SchemaYaml(schema_yaml)),
        };
        let result = self
            .call(
                |mut client, request| async move { client.create_schema(request).await.map(|_| ()) },
                req,
            )
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(err)
                if err
                    .downcast_ref::<tonic::Status>()
                    .is_some_and(|s| s.code() == Code::AlreadyExists) =>
            {
                Ok(())
            }
            Err(err) => Err(err).context("create schema"),
        }
    }

    pub async fn get_schema(&self, name: &str) -> Result<Option<String>> {
        let req = GetSchemaRequest {
            name: name.to_string(),
        };
        let result = self
            .call(
                |mut client, request| async move {
                    client
                        .get_schema(request)
                        .await
                        .map(|resp| resp.into_inner().schema_yaml)
                },
                req,
            )
            .await;
        match result {
            Ok(schema_yaml) => Ok(Some(schema_yaml)),
            Err(err)
                if err
                    .downcast_ref::<tonic::Status>()
                    .is_some_and(|s| s.code() == Code::NotFound) =>
            {
                Ok(None)
            }
            Err(err) => Err(err).context("get schema"),
        }
    }

    pub async fn upsert_graph_low_level(&self, graph: GraphBuild) -> Result<(i32, i32)> {
        let node_count = self.upsert_nodes(graph.nodes).await?;
        let edge_count = self.upsert_edges(graph.edges).await?;
        Ok((node_count, edge_count))
    }

    pub async fn upsert_nodes(&self, nodes: Vec<GraphNode>) -> Result<i32> {
        if nodes.is_empty() {
            return Ok(0);
        }
        let req = UpsertNodesRequest {
            nodes: nodes.into_iter().map(to_proto_node).collect(),
        };
        self.call(
            |mut client, request| async move {
                client
                    .upsert_nodes(request)
                    .await
                    .map(|resp| resp.into_inner().upserted_count)
            },
            req,
        )
        .await
        .context("upsert nodes")
    }

    pub async fn upsert_edges(&self, edges: Vec<GraphEdge>) -> Result<i32> {
        if edges.is_empty() {
            return Ok(0);
        }
        let req = UpsertEdgesRequest {
            edges: edges.into_iter().map(to_proto_edge).collect(),
        };
        self.call(
            |mut client, request| async move {
                client
                    .upsert_edges(request)
                    .await
                    .map(|resp| resp.into_inner().upserted_count)
            },
            req,
        )
        .await
        .context("upsert edges")
    }

    /// vegapunk 側の Embedding-as-a-service (`Embed` RPC) でテキストをベクトル化する。
    /// 生成物は `upsert_vectors` の `VectorEntry.vector` にそのまま渡せる。
    /// backend 契約違反（空 vector、`dimension` と `vector.len()` の不一致）は
    /// 呼び出し側に不完全なベクトルを渡す前にここで弾く。
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let req = EmbedRequest {
            text: text.to_string(),
        };
        let resp = self
            .call(
                |mut client, request| async move {
                    client.embed(request).await.map(|resp| resp.into_inner())
                },
                req,
            )
            .await
            .context("embed")?;
        if resp.vector.is_empty() {
            anyhow::bail!("embed returned an empty vector (model={})", resp.model);
        }
        if resp.dimension <= 0 {
            anyhow::bail!(
                "embed returned a non-positive dimension ({}) (model={})",
                resp.dimension,
                resp.model
            );
        }
        if resp.dimension as usize != resp.vector.len() {
            anyhow::bail!(
                "embed dimension mismatch: response says {} but vector has {} elements (model={})",
                resp.dimension,
                resp.vector.len(),
                resp.model
            );
        }
        Ok(resp.vector)
    }

    /// `(id, vector, metadata)` のタプルを `VectorEntry`（`metadata` は proto の
    /// `map<string, string>`）へ変換して一括 upsert する。空なら RPC を発行せず 0 を返す
    /// （`upsert_nodes` / `upsert_edges` と同じ規約）。
    pub async fn upsert_vectors(&self, entries: Vec<VectorUpsertEntry>) -> Result<i32> {
        if entries.is_empty() {
            return Ok(0);
        }
        let req = UpsertVectorsRequest {
            vectors: entries
                .into_iter()
                .map(|(id, vector, metadata)| VectorEntry {
                    id,
                    vector,
                    metadata: metadata.into_iter().collect(),
                })
                .collect(),
        };
        self.call(
            |mut client, request| async move {
                client
                    .upsert_vectors(request)
                    .await
                    .map(|resp| resp.into_inner().upserted_count)
            },
            req,
        )
        .await
        .context("upsert vectors")
    }

    pub async fn query_nodes(
        &self,
        schema: &str,
        node_type: &str,
        filters: Vec<(&str, &str, &str)>,
        limit: i32,
    ) -> Result<Vec<crate::proto::graphrag::NodeResult>> {
        let req = QueryNodesRequest {
            schema: schema.to_string(),
            node_type: node_type.to_string(),
            filters: filters
                .into_iter()
                .map(|(key, op, value)| AttributeFilter {
                    key: key.to_string(),
                    op: op.to_string(),
                    value: value.to_string(),
                })
                .collect(),
            sort_by: None,
            sort_order: None,
            limit: Some(limit),
            offset: Some(0),
            traverse: None,
        };
        self.call(
            |mut client, request| async move {
                client
                    .query_nodes(request)
                    .await
                    .map(|resp| resp.into_inner().nodes)
            },
            req,
        )
        .await
        .context("query nodes")
    }

    /// `query_nodes` の `sort_by`/`sort_order` を明示できる版（管理 API の時系列一覧向け、
    /// Issue #31）。`QueryNodesRequest` にはこの 2 フィールドが元々存在するが、既存の
    /// `query_nodes` は両方 `None` 固定で呼んでいるため、新規 RPC を足さずに既存フィールドを
    /// 使うだけの薄いオーバーロードとしてここに分離する（既存呼び出し元の挙動は変えない）。
    /// `offset` は呼び出し元（`harness::knowledge::load_conversation_turns_page`）がページング
    /// カーソルとして明示する（Issue #31 reviewer 指摘5: 当初は offset 固定 `Some(0)` のまま
    /// `created_at < cursor` という値ベースの filter でページングしていたが、`created_at` が
    /// 完全一致する境界でエントリを取りこぼす・重複させる欠陥があったため、offset ベースの
    /// ページングへ切り替えた）。
    #[allow(clippy::too_many_arguments)]
    pub async fn query_nodes_sorted(
        &self,
        schema: &str,
        node_type: &str,
        filters: Vec<(&str, &str, &str)>,
        sort_by: &str,
        sort_order: &str,
        offset: i32,
        limit: i32,
    ) -> Result<Vec<crate::proto::graphrag::NodeResult>> {
        let req = QueryNodesRequest {
            schema: schema.to_string(),
            node_type: node_type.to_string(),
            filters: filters
                .into_iter()
                .map(|(key, op, value)| AttributeFilter {
                    key: key.to_string(),
                    op: op.to_string(),
                    value: value.to_string(),
                })
                .collect(),
            sort_by: Some(sort_by.to_string()),
            sort_order: Some(sort_order.to_string()),
            limit: Some(limit),
            offset: Some(offset),
            traverse: None,
        };
        self.call(
            |mut client, request| async move {
                client
                    .query_nodes(request)
                    .await
                    .map(|resp| resp.into_inner().nodes)
            },
            req,
        )
        .await
        .context("query nodes sorted")
    }

    /// `query_nodes` を offset ページングで最後まで読み切り、一致ノードを全件返す。
    ///
    /// `GetGraphSnapshot` は `max_nodes` に backend 側ハード上限 5000 があり、数万ノード規模の
    /// schema では truncate して差分/stale 判定を壊す。`QueryNodes` は `offset`/`limit`(max 1000)
    /// で任意件数までページングできるため、既存状態の全件ロードはこちらを使う。
    /// 1 ページが `page_size` 未満になった時点で終端とみなす（`page_is_last`）。読み取り中に
    /// グラフを書き換えない前提で使うこと（ingest は書き込みより前にこれで読み切る）。
    ///
    /// 完全性は 2 系統で担保し `traverse_neighbors_paged` と対称化する:
    /// (1) 短ページ終端（`page_is_last`）、(2) backend 申告の `QueryNodesResponse.total_count`
    /// （post-filter・pre-pagination の全件数）と収集数の最終照合（`pagination_is_complete`）。
    /// backend が「まだ残るのに短ページ」を返すと (1) だけでは取りこぼしを検出できず、差分/stale
    /// 判定が fail-open するため、届かないまま終端したら bail する。`total_count` が非正
    /// （backend 未申告）の場合は (2) を skip し、従来どおり (1) の短ページ終端に委ねる。
    pub async fn query_nodes_paged(
        &self,
        schema: &str,
        node_type: &str,
        filters: Vec<(&str, &str, &str)>,
        page_size: i32,
    ) -> Result<Vec<crate::proto::graphrag::NodeResult>> {
        anyhow::ensure!(
            page_size > 0,
            "query_nodes_paged page_size must be positive (got {page_size})"
        );
        let proto_filters: Vec<AttributeFilter> = filters
            .into_iter()
            .map(|(key, op, value)| AttributeFilter {
                key: key.to_string(),
                op: op.to_string(),
                value: value.to_string(),
            })
            .collect();
        let mut all: Vec<crate::proto::graphrag::NodeResult> = Vec::new();
        let mut offset = 0i32;
        loop {
            let req = QueryNodesRequest {
                schema: schema.to_string(),
                node_type: node_type.to_string(),
                filters: proto_filters.clone(),
                sort_by: None,
                sort_order: None,
                limit: Some(page_size),
                offset: Some(offset),
                traverse: None,
            };
            let resp = self
                .call(
                    |mut client, request| async move {
                        client.query_nodes(request).await.map(|r| r.into_inner())
                    },
                    req,
                )
                .await
                .with_context(|| format!("query nodes (paged {node_type}, offset {offset})"))?;
            let total_count = resp.total_count;
            let returned = resp.nodes.len();
            all.extend(resp.nodes);
            if page_is_last(returned, page_size as usize) {
                // backend 申告の全件数（post-filter・pre-pagination）に収集数が届かないまま
                // 短ページで終端したら、ページングが不完全＝ノードを取りこぼしている。差分/stale
                // 判定を不完全データで進めると fail-open するため fail closed（traverse と対称）。
                if !pagination_is_complete(all.len(), total_count) {
                    anyhow::bail!(
                        "query_nodes_paged for {node_type} collected {} node(s) but backend \
                         reported total_count={total_count}; pagination is incomplete, refusing \
                         to proceed on a truncated node set",
                        all.len()
                    );
                }
                break;
            }
            // offset は毎ページ page_size 進むため必ず前進し、総数を超えれば空ページで終端する。
            offset = offset
                .checked_add(page_size)
                .context("query_nodes_paged offset overflowed i32")?;
        }
        Ok(all)
    }

    /// `source_node_id` から `edge_type` を `direction`（"outgoing"|"incoming"）へ 1-hop 辿った
    /// 隣接ノードを **offset ページングで全件** 返す（`QueryNodes` の `traverse`）。
    ///
    /// `QueryNodesResponse.total_count`（フィルタ後・ページング前の全件数）を完全性の権威として使い、
    /// 収集数がそれに届かないまま短ページで終端したら fail closed する。旧実装は非ページングで
    /// `limit` 到達を truncation とみなして即エラーにしていたが、これは hot signal（3000 辺 等）で
    /// 常にエラーになり読み取りを止めていた。offset ページングで取り切ることで、辺数がグラフ規模に
    /// 依存して大きくても全件を欠落なく得る。
    pub async fn traverse_neighbors_paged(
        &self,
        schema: &str,
        neighbor_node_type: &str,
        edge_type: &str,
        direction: &str,
        source_node_id: &str,
        page_size: i32,
    ) -> Result<Vec<crate::proto::graphrag::NodeResult>> {
        anyhow::ensure!(
            (1..=1000).contains(&page_size),
            "traverse page_size must be in 1..=1000 (got {page_size})"
        );
        let mut all: Vec<crate::proto::graphrag::NodeResult> = Vec::new();
        let mut offset = 0i32;
        loop {
            let req = QueryNodesRequest {
                schema: schema.to_string(),
                node_type: neighbor_node_type.to_string(),
                filters: Vec::new(),
                sort_by: None,
                sort_order: None,
                limit: Some(page_size),
                offset: Some(offset),
                traverse: Some(EdgeTraversal {
                    edge_type: edge_type.to_string(),
                    direction: direction.to_string(),
                    source_node_id: source_node_id.to_string(),
                }),
            };
            let resp = self
                .call(
                    |mut client, request| async move {
                        client.query_nodes(request).await.map(|r| r.into_inner())
                    },
                    req,
                )
                .await
                .with_context(|| {
                    format!(
                        "traverse {edge_type} {direction} from {source_node_id} (offset {offset})"
                    )
                })?;
            let total_count = resp.total_count;
            let returned = resp.nodes.len();
            all.extend(resp.nodes);
            // 半端ページ（要求 page_size 未満、0 件を含む）＝最終ページ。
            if page_is_last(returned, page_size as usize) {
                // backend 申告の全件数に収集数が届かないなら、ページングが不完全＝辺を取りこぼしている。
                // stale 検出・材料組み立てを不完全データで進めると fail-open するため fail closed。
                if !pagination_is_complete(all.len(), total_count) {
                    anyhow::bail!(
                        "traverse {edge_type} {direction} from {source_node_id} collected {} \
                         neighbor(s) but backend reported total_count={total_count}; pagination is \
                         incomplete, refusing to proceed on a truncated neighbor set",
                        all.len()
                    );
                }
                break;
            }
            offset = offset
                .checked_add(page_size)
                .context("traverse offset overflowed i32")?;
        }
        Ok(all)
    }

    /// [`traverse_neighbors_paged`] の node_id だけを返す薄いラッパ。stale-edge 検出や
    /// 材料 corpus の辺復元のように、隣接ノードの属性ではなく id 集合だけが要る呼び出し用。
    pub async fn traverse_neighbor_ids(
        &self,
        schema: &str,
        neighbor_node_type: &str,
        edge_type: &str,
        direction: &str,
        source_node_id: &str,
        page_size: i32,
    ) -> Result<Vec<String>> {
        Ok(self
            .traverse_neighbors_paged(
                schema,
                neighbor_node_type,
                edge_type,
                direction,
                source_node_id,
                page_size,
            )
            .await?
            .into_iter()
            .map(|n| n.node_id)
            .collect())
    }

    pub async fn graph_snapshot(
        &self,
        schema: &str,
        max_nodes: i32,
    ) -> Result<crate::proto::graphrag::GetGraphSnapshotResponse> {
        let req = GetGraphSnapshotRequest {
            schema: schema.to_string(),
            max_nodes: Some(max_nodes),
            node_types: Vec::new(),
            min_degree: Some(0),
        };
        self.call(
            |mut client, request| async move {
                client
                    .get_graph_snapshot(request)
                    .await
                    .map(|resp| resp.into_inner())
            },
            req,
        )
        .await
        .context("get graph snapshot")
    }

    /// 既存呼び出し元互換の local 検索。retrieval / verify CLI はこちらを使う。
    pub async fn search(
        &self,
        schema: &str,
        query: &str,
        top_k: i32,
    ) -> Result<Vec<SearchResultItem>> {
        Ok(self
            .search_with_mode(schema, query, top_k, "local")
            .await?
            .results)
    }

    /// mode を明示する検索。`global` は Merge 未実行だと FAILED_PRECONDITION、
    /// `hybrid` は global 部分だけ local へ degrade する（落ちない）。
    /// degrade したときは warn で理由を出す（黙って degrade させない）。
    pub async fn search_with_mode(
        &self,
        schema: &str,
        query: &str,
        top_k: i32,
        mode: &str,
    ) -> Result<SearchOutcome> {
        let req = SearchRequest {
            text: query.to_string(),
            filter: None,
            depth: Some(1),
            top_k: Some(top_k),
            format: None,
            mode: Some(mode.to_string()),
            schema: schema.to_string(),
            offset: Some(0),
            limit: Some(top_k),
            structural_weight: Some(0.0),
        };
        let resp = self
            .call(
                |mut client, request| async move {
                    client.search(request).await.map(|resp| resp.into_inner())
                },
                req,
            )
            .await
            // Merge 固有のヒントは付けない。`mode=global` は Merge 前に FAILED_PRECONDITION を
            // 返すのが正常であり、そこへ「Merge が同時実行中」と書くと原因を誤誘導する。
            .with_context(|| format!("search schema {schema}"))?;
        if let Some(execution) = resp.execution.as_ref() {
            if execution.degraded {
                tracing::warn!(
                    schema,
                    requested_mode = %execution.requested_mode,
                    effective_mode = %execution.effective_mode,
                    degradations = %execution
                        .degradations
                        .iter()
                        .map(degradation_summary)
                        .collect::<Vec<_>>()
                        .join("; "),
                    "vegapunk search degraded"
                );
            }
        }
        Ok(SearchOutcome {
            results: resp.results,
            execution: resp.execution,
        })
    }

    /// Leiden コミュニティ検出 + CommunitySummary 生成 + Node2Vec を schema 全体に対して
    /// 実行する（vegapunk `Merge` RPC）。**admin ロール必須・同期実行・同一 schema で同時 1 本のみ。**
    /// 応答は空（`MergeResponse {}`）で進捗もジョブ ID も返らないため、成否の確認は
    /// `stats()` の `community_count` で行う。10 万ノード規模では長時間化するので、
    /// 呼び出し側は `GrpcLimits.timeout_secs` を十分長く張り替えてから使うこと。
    pub async fn merge(&self, schema: &str) -> Result<()> {
        let req = MergeRequest {
            schema: schema.to_string(),
        };
        self.call(
            |mut client, request| async move {
                client.merge(request).await.map(|resp| resp.into_inner())
            },
            req,
        )
        .await
        .map(|_| ())
        .map_err(|err| annotate_grpc_error(err, &format!("merge schema {schema}")))
    }

    /// schema のノード / エッジ / ベクトル / コミュニティ件数。`community_count` は
    /// Merge を実行したかどうかの一次証跡になる（Merge 前は 0 のはず）。
    pub async fn stats(&self, schema: &str) -> Result<GetStatsResponse> {
        let req = GetStatsRequest {
            schema: Some(schema.to_string()),
            node_type: None,
            filters: Vec::new(),
        };
        self.call(
            |mut client, request| async move {
                client
                    .get_stats(request)
                    .await
                    .map(|resp| resp.into_inner())
            },
            req,
        )
        .await
        // Merge 固有のヒントは付けない（GetStats の失敗は Merge の同時実行とは無関係）。
        .with_context(|| format!("get stats for schema {schema}"))
    }

    /// vegapunk の Admin API `ListJobs`（read ロール）。ジョブキューを問い合わせて
    /// job_id / status / error 等を取得する。Merge のような「進捗もジョブ ID も返らない
    /// 同期 RPC」が失敗したとき、失敗の具体的な理由（サーバ側でどのジョブがどう落ちたか）を
    /// 得る唯一の経路として使う。
    ///
    /// `status` の有効値は `"pending" | "running" | "completed" | "failed"`（vegapunk 側に
    /// 2026-07-27 に確認済み。proto / 統合仕様書 / 実装の 3 つで一貫している）。かつて
    /// vendor 済み proto のコメントだけが `"dead_letter"` を挙げていたが、それは**こちらの
    /// コピーが古かった**もので、vegapunk 側の不整合ではない。
    ///
    /// それでも既定は `None`（フィルタ無し、全件）にしている。診断用途では「失敗ジョブが
    /// 1 件も無い」ことと「フィルタ値を間違えて 0 件だった」ことを取り違えたくないため。
    /// `until_ms` / `offset` / `job_type` は現状の呼び出し元（`merge_schema` CLI）が
    /// 使わないため引数を増やさない。必要になったら追加する。
    ///
    /// `since_ms` は cross-schema・`job_type` 無フィルタで返ることの緩和策。`ListJobs` は
    /// schema を絞る手段が proto に無いため、他 schema の大量の `entity_extraction` ジョブに
    /// 目的のジョブが `created_at DESC` の先頭 `limit` 件から押し出されうる。`since_ms` で
    /// 時間窓を絞ることで、少なくとも「窓の外」を明示的に切り離せる（呼び出し元は
    /// `merge_schema` CLI の `--jobs-since-hours` 参照）。
    pub async fn list_jobs(
        &self,
        status: Option<&str>,
        since_ms: Option<i64>,
        limit: i32,
    ) -> Result<ListJobsResponse> {
        let req = ListJobsRequest {
            status: status.map(str::to_string),
            since_ms,
            until_ms: None,
            offset: None,
            limit: Some(limit),
            job_type: None,
        };
        self.call(
            |mut client, request| async move {
                client
                    .list_jobs(request)
                    .await
                    .map(|resp| resp.into_inner())
            },
            req,
        )
        .await
        // Merge 専用のヒント（annotate_grpc_error / merge_error_hint）は流用しない。
        // ListJobs 自体の失敗は診断取得の失敗であって、Merge の同時実行やタイムアウトとは
        // 無関係。ListJobs 固有のヒント（cross-schema observability RPC の権限境界）は
        // annotate_list_jobs_error が別途付ける。
        .map_err(|err| annotate_list_jobs_error(err, "list jobs"))
    }

    async fn call<T, F, Fut, R>(&self, f: F, body: T) -> Result<R>
    where
        F: FnOnce(GraphRagEngineClient<Channel>, Request<T>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<R, tonic::Status>>,
    {
        let mut request = Request::new(body);
        request
            .metadata_mut()
            .insert("authorization", self.auth_header.clone());
        f(self.inner.clone(), request).await.map_err(Into::into)
    }
}

/// offset ページングの終端判定: 1 ページで返った件数が要求 `page_size` 未満なら最終ページ。
/// `<`（`<=` ではない）である点が重要 — ちょうど `page_size` 件返ったときはさらに次ページが
/// 存在しうるため継続する。境界を誤ると早期終了（取りこぼし）か無限ループを招く。
fn page_is_last(returned: usize, page_size: usize) -> bool {
    returned < page_size
}

/// gRPC の status code を、運用者が次のアクションを判断できる文言に写像する。
/// 分類できない code は `None` を返し、元の `Status` の message をそのまま見せる
/// （当てずっぽうの説明を足して原因を誤誘導しない）。
pub fn merge_error_hint(code: Code) -> Option<&'static str> {
    match code {
        Code::FailedPrecondition => Some(
            "同一 schema で Merge が同時実行中か、サーバ側の前提未達（embedding / LLM 設定等）。\
             実行中なら完了を待つ。リトライでは解決しない",
        ),
        Code::PermissionDenied => {
            Some("Merge は admin ロール必須。使用中の bearer token の権限を確認する")
        }
        Code::DeadlineExceeded => Some(
            "クライアント側 timeout。--timeout-secs を引き上げる。\
             サーバ側では Merge が継続している可能性があるため、再実行前に stats の community_count を確認する",
        ),
        // tonic 0.12.3 実測（tonic-0.12.3/src/status.rs の find_status_in_source_chain）:
        // `Endpoint::timeout`（--timeout-secs）の満了は `TimeoutExpired` → `Status::cancelled`
        // に写像され、Code::DeadlineExceeded にはならない。h2 keepalive を無効化した
        // merge_cli_limits() の下では、接続が本当に死んだときの唯一の出口がこの
        // per-request timeout（既定 6h）なので、ここにヒントが無いと「6 時間待った末に
        // "Timeout expired" だけが出る」ことになる。DeadlineExceeded と同趣旨のヒントを返す
        // （どちらの code に転んでも運用者に届くよう両方を扱う）。
        Code::Cancelled => Some(
            "クライアント側 timeout（tonic 0.12 では Endpoint::timeout の満了が \
             Code::Cancelled に写像される。DeadlineExceeded にはならない）。\
             --timeout-secs を引き上げる。再実行の前に --probe-only で community_count を確認し、\
             サーバ側の Merge が継続しているかを見る",
        ),
        // 実測（本番 Cloud Run job）: h2 keepalive が Merge 実行中の接続を 40 秒で切り、
        // Unavailable("http2 error: keep-alive timed out") になった。keepalive を無効化した
        // 後に残る Unavailable の主因は、それとは別に (a) 長時間 RPC 中の何らかの切断
        // （その場合サーバ側の Merge は継続している）、(b) vegapunk の再起動・LB ドレイン・
        // 到達不能（その場合 Merge も中断している）のいずれかであり、どちらかは実行前には
        // 分からない。運用者が取るべき行動は同じ（--probe-only で確認）なので、断定せず
        // 両分岐と、確認後にどう動くかまで書く。
        Code::Unavailable => Some(
            "接続断（h2 / transport エラー）。原因は (a) 長時間 RPC 中の切断でサーバ側の \
             Merge は継続している、(b) vegapunk の再起動・LB ドレイン・到達不能で Merge も \
             中断している、のいずれか。再実行の前に --probe-only で community_count を確認する: \
             前回観測より増えていれば (a) と判断して完了を待ち、変化が無ければ (b) と判断して \
             そのまま再実行してよい。走行中に再実行すると FAILED_PRECONDITION（同時実行不可）になる",
        ),
        _ => None,
    }
}

/// `list_jobs` 専用のヒント関数。**`merge_error_hint` を流用しない** — 意味が異なるため。
/// `merge_error_hint(Code::PermissionDenied)` は「Merge は admin ロール必須」だが、
/// `ListJobs` は vegapunk 統合仕様書 §2.3 の cross-schema observability RPC であり、
/// 権限境界が別物: **サービストークン（`vgp_`）からは常に拒否され、無制限資格情報
/// （ルート Bearer / JWT admin）でのみ許可される**。本番トークンがサービストークンの
/// 場合、この診断は毎回 PermissionDenied で終わるため、その理由をヒントで明示する。
pub fn list_jobs_error_hint(code: Code) -> Option<&'static str> {
    match code {
        Code::PermissionDenied => Some(
            "ListJobs は cross-schema observability RPC。サービストークン（vgp_）では常に \
             拒否される。ルート Bearer / JWT admin で実行する",
        ),
        _ => None,
    }
}

/// `list_jobs` が返す anyhow エラーに、[`list_jobs_error_hint`] 由来の運用ヒントを付ける。
/// `annotate_grpc_error` と同じ downcast の型だが、**`list_jobs` 専用**。
/// `annotate_grpc_error`（Merge 専用）と統合すると、Merge のヒント文言（同時実行中 等）が
/// ListJobs の失敗に紛れ込み、原因を誤誘導する。
fn annotate_list_jobs_error(err: anyhow::Error, context: &str) -> anyhow::Error {
    let hint = err
        .downcast_ref::<tonic::Status>()
        .and_then(|status| list_jobs_error_hint(status.code()));
    match hint {
        Some(hint) => err.context(format!("{context}: {hint}")),
        None => err.context(context.to_string()),
    }
}

/// `SearchDegradation` を 1 行のログ文字列にする。prost の enum は i32 なので、
/// 既知値は名前、未知値は数値のまま残す（proto にフィールドが増えても情報を落とさない）。
pub fn degradation_summary(degradation: &SearchDegradation) -> String {
    use crate::proto::graphrag::{SearchComponent, SearchDegradedReason};
    let component = SearchComponent::try_from(degradation.component)
        .map(|c| c.as_str_name().to_string())
        .unwrap_or_else(|_| format!("UNKNOWN({})", degradation.component));
    let reason = SearchDegradedReason::try_from(degradation.reason)
        .map(|r| r.as_str_name().to_string())
        .unwrap_or_else(|_| format!("UNKNOWN({})", degradation.reason));
    format!("{component}/{reason}: {}", degradation.message)
}

/// `call` が返す anyhow エラーに、gRPC code 由来の運用ヒントを付ける。
/// `call` は `tonic::Status` を `Into` で anyhow 化しているので downcast で code を取り出す。
///
/// **`merge` 専用**。ヒント文言は Merge の失敗を前提に書かれているため、他の RPC に付けると
/// 誤った原因説明になる（例: `mode=global` が Merge 前に返す FAILED_PRECONDITION は正常応答で、
/// 「Merge が同時実行中」は嘘になる）。他の RPC は素の `Context` を使うこと。
fn annotate_grpc_error(err: anyhow::Error, context: &str) -> anyhow::Error {
    let hint = err
        .downcast_ref::<tonic::Status>()
        .and_then(|status| merge_error_hint(status.code()));
    match hint {
        Some(hint) => err.context(format!("{context}: {hint}")),
        None => err.context(context.to_string()),
    }
}

/// offset ページングの完全性判定: 収集件数 `collected` が backend 申告の `total_count`
/// （post-filter・pre-pagination の全件数）以上なら完全。`total_count` が負（backend が値を
/// 埋めない異常時）は検証をスキップして完全とみなす（`returned < page_size` の終端に委ねる）。
/// `total_count == 0` も「未申告 or 実際に 0 件」の両義で、`collected >= 0` が常に真なので
/// 完全扱い（従来の短ページ終端に委ねる）＝非正なら実質 no-op として degrade する。
/// `traverse_neighbors_paged`（隣接辺）と `query_nodes_paged`（ノード）の双方が共有する。
/// `i128` 経由で比較し `usize`/`i32` の境界で溢れさせない。
fn pagination_is_complete(collected: usize, total_count: i32) -> bool {
    total_count < 0 || collected as i128 >= total_count as i128
}

fn to_proto_node(node: GraphNode) -> Node {
    Node {
        id: node.id,
        r#type: node.node_type,
        attributes: node
            .attributes
            .into_iter()
            .map(|(key, value)| NodeAttribute { key, value })
            .collect(),
    }
}

fn to_proto_edge(edge: GraphEdge) -> Edge {
    Edge {
        from_id: edge.from_id,
        to_id: edge.to_id,
        r#type: edge.edge_type,
        attributes: edge
            .attributes
            .into_iter()
            .map(|(key, value)| NodeAttribute { key, value })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        annotate_grpc_error, annotate_list_jobs_error, degradation_summary, list_jobs_error_hint,
        merge_error_hint, page_is_last, pagination_is_complete, GrpcLimits,
    };
    use std::time::Duration;
    use tonic::Code;

    #[test]
    fn grpc_limits_default_keeps_resident_server_keepalive() {
        // 常駐サーバ（main.rs）と merge_schema 以外の CLI は、この既定のまま動いている。
        // h2 keepalive を既定から外すと「長寿命チャネルがアイドル後に死んだ接続を掴んだまま
        // 120s ハングする」事象（対策コメントは connect_lazy_with_limits 参照）が再発する。
        // keepalive を切ってよいのは、一発の長時間 RPC しか投げない merge_schema CLI だけ。
        let limits = GrpcLimits::default();
        assert_eq!(limits.timeout_secs, 120, "既定の per-request timeout");
        assert_eq!(
            limits.max_decode_bytes,
            64 * 1024 * 1024,
            "既定の decode 上限"
        );
        let keep_alive = limits
            .keep_alive
            .expect("既定では h2 PING keepalive が有効であること");
        assert_eq!(keep_alive.interval, Duration::from_secs(30));
        assert_eq!(keep_alive.timeout, Duration::from_secs(10));
        assert!(
            keep_alive.while_idle,
            "アイドル中の死活検知が既定の目的なので while_idle は true"
        );
    }

    #[test]
    fn degradation_summary_renders_component_and_reason() {
        use crate::proto::graphrag::{SearchComponent, SearchDegradation, SearchDegradedReason};
        let degradation = SearchDegradation {
            component: SearchComponent::Global as i32,
            reason: SearchDegradedReason::NotReady as i32,
            message: "No community summaries found".to_string(),
        };
        let rendered = degradation_summary(&degradation);
        assert!(
            rendered.contains("GLOBAL"),
            "component が読める: {rendered}"
        );
        assert!(
            rendered.contains("NOT_READY"),
            "reason が読める: {rendered}"
        );
        assert!(
            rendered.contains("No community summaries found"),
            "サーバの message を落とさない: {rendered}"
        );
    }

    #[test]
    fn degradation_summary_keeps_unknown_enum_values_visible() {
        use crate::proto::graphrag::SearchDegradation;
        // 未知の enum 値（proto 追加時）でも数値を残し、握りつぶさない。
        let degradation = SearchDegradation {
            component: 9999,
            reason: 8888,
            message: String::new(),
        };
        let rendered = degradation_summary(&degradation);
        assert!(
            rendered.contains("9999"),
            "未知 component を数値で残す: {rendered}"
        );
        assert!(
            rendered.contains("8888"),
            "未知 reason を数値で残す: {rendered}"
        );
    }

    #[test]
    fn merge_error_hint_maps_operational_codes() {
        // 運用者が「次に何をすればよいか」を判断できる文言であること。
        let precondition = merge_error_hint(Code::FailedPrecondition).expect("hint");
        assert!(precondition.contains("同時実行"));
        let denied = merge_error_hint(Code::PermissionDenied).expect("hint");
        assert!(denied.contains("admin"));
        let deadline = merge_error_hint(Code::DeadlineExceeded).expect("hint");
        assert!(deadline.contains("--timeout-secs"));
        // 未分類の code はヒント無し（元の Status をそのまま見せる）
        assert!(merge_error_hint(Code::Internal).is_none());
    }

    #[test]
    fn merge_error_hint_explains_transport_disconnect() {
        // 本番 Cloud Run job 実測: h2 keepalive が Merge 実行中の接続を 40 秒で切り、
        // Unavailable("http2 error: keep-alive timed out") になった。keepalive を無効化した
        // 今、残る Unavailable の主因は「サーバ側 Merge は継続中」と「vegapunk 再起動・到達不能で
        // Merge も中断」のいずれかで、実行前にはどちらか分からない。断定せず両分岐を示し、
        // --probe-only の結果でどう動くか（増えていれば待つ、変化が無ければ再実行）まで伝えること。
        let unavailable = merge_error_hint(Code::Unavailable).expect("hint");
        assert!(
            unavailable.contains("--probe-only"),
            "再実行前の確認手段を示す: {unavailable}"
        );
        assert!(
            unavailable.contains("community_count"),
            "何を見れば継続中か分かるかを示す: {unavailable}"
        );
        assert!(
            unavailable.contains("継続している"),
            "分岐 (a): 長時間 RPC 中の切断でサーバ側 Merge は継続、を伝える: {unavailable}"
        );
        assert!(
            unavailable.contains("再起動") && unavailable.contains("中断している"),
            "分岐 (b): vegapunk 再起動・到達不能で Merge も中断、を伝える: {unavailable}"
        );
        assert!(
            unavailable.contains("増えていれば") && unavailable.contains("再実行してよい"),
            "probe 後にどう動くか（待つ / そのまま再実行）まで伝える: {unavailable}"
        );
    }

    #[test]
    fn merge_error_hint_explains_client_side_cancellation() {
        // tonic 0.12.3 実測（status.rs の find_status_in_source_chain）: Endpoint::timeout の
        // 満了は Status::cancelled（Code::Cancelled）に写像され、Code::DeadlineExceeded には
        // ならない。h2 keepalive を無効化した merge_schema CLI では、接続が本当に死んだときの
        // 唯一の出口が per-request timeout なので、Cancelled にヒントが無いと「6 時間待った末に
        // "Timeout expired" だけが出る」ことになる。DeadlineExceeded と同趣旨のヒントが要る。
        let cancelled = merge_error_hint(Code::Cancelled).expect("hint");
        assert!(
            cancelled.contains("--timeout-secs"),
            "timeout 値を引き上げる手段を示す: {cancelled}"
        );
        assert!(
            cancelled.contains("--probe-only") && cancelled.contains("community_count"),
            "再実行前の確認手段を DeadlineExceeded と同趣旨で示す: {cancelled}"
        );
    }

    #[test]
    fn annotate_grpc_error_attaches_hint_by_downcasting_tonic_status() {
        // `call` は tonic::Status を Into で anyhow 化する。その downcast 経路が生きていないと
        // ヒントが黙って消え、Merge 失敗時に運用者へ次のアクションが伝わらない。
        let err = anyhow::Error::from(tonic::Status::failed_precondition("merge already running"));
        let annotated = annotate_grpc_error(err, "merge schema test");
        let rendered = format!("{annotated:#}");
        assert!(
            rendered.contains("merge schema test"),
            "呼び出し文脈を残す: {rendered}"
        );
        assert!(
            rendered.contains("同時実行"),
            "FAILED_PRECONDITION のヒントが付く: {rendered}"
        );
        assert!(
            rendered.contains("merge already running"),
            "サーバの message を落とさない: {rendered}"
        );
    }

    #[test]
    fn annotate_grpc_error_keeps_context_only_for_unclassified_errors() {
        // tonic::Status でない（= downcast できない）エラーに、当てずっぽうの Merge 説明を足さない。
        let err = anyhow::anyhow!("channel closed");
        let rendered = format!("{:#}", annotate_grpc_error(err, "merge schema test"));
        assert!(rendered.contains("merge schema test"), "{rendered}");
        assert!(rendered.contains("channel closed"), "{rendered}");
        assert!(
            !rendered.contains("同時実行"),
            "分類できないエラーにヒントを付けない: {rendered}"
        );
    }

    #[test]
    fn list_jobs_error_hint_explains_service_token_cannot_call_cross_schema_rpc() {
        // merge_error_hint(PermissionDenied) と混同すると「Merge は admin ロール必須」という
        // 誤った理由が出る。ListJobs は別の権限境界（サービストークンは常に拒否）なので、
        // 専用のヒントであることを保証する。
        let denied = list_jobs_error_hint(Code::PermissionDenied).expect("hint");
        assert!(
            denied.contains("vgp_"),
            "サービストークンが常に拒否されることを示す: {denied}"
        );
        assert!(
            denied.contains("cross-schema"),
            "cross-schema observability RPC であることを示す: {denied}"
        );
        assert!(
            !denied.contains("admin ロール必須"),
            "merge_error_hint の文言（Merge 専用）を混同していない: {denied}"
        );
        // 未分類の code はヒント無し（merge_error_hint と同じ規約）。
        assert!(list_jobs_error_hint(Code::Internal).is_none());
    }

    #[test]
    fn annotate_list_jobs_error_attaches_hint_by_downcasting_tonic_status() {
        let err = anyhow::Error::from(tonic::Status::permission_denied("rbac: read denied"));
        let annotated = annotate_list_jobs_error(err, "list jobs");
        let rendered = format!("{annotated:#}");
        assert!(
            rendered.contains("list jobs"),
            "呼び出し文脈を残す: {rendered}"
        );
        assert!(
            rendered.contains("vgp_"),
            "PermissionDenied のヒントが付く: {rendered}"
        );
        assert!(
            rendered.contains("rbac: read denied"),
            "サーバの message を落とさない: {rendered}"
        );
    }

    #[test]
    fn annotate_list_jobs_error_keeps_context_only_for_unclassified_errors() {
        let err = anyhow::anyhow!("channel closed");
        let rendered = format!("{:#}", annotate_list_jobs_error(err, "list jobs"));
        assert!(rendered.contains("list jobs"), "{rendered}");
        assert!(rendered.contains("channel closed"), "{rendered}");
        assert!(
            !rendered.contains("vgp_"),
            "分類できないエラーにヒントを付けない: {rendered}"
        );
    }

    #[test]
    fn page_is_last_is_true_only_when_returned_below_page_size() {
        // 半端ページ（< page_size）＝最終ページ。
        assert!(page_is_last(0, 1000));
        assert!(page_is_last(999, 1000));
        // ちょうど埋まったページはさらに続きがありうるので継続する。
        assert!(!page_is_last(1000, 1000));
        // 空グラフ（1 ページ目が 0 件）も即終端する。
        assert!(page_is_last(0, 1));
    }

    #[test]
    fn pagination_is_complete_requires_reaching_total_count() {
        // 収集が申告全件に届いていれば完全（3000 辺の hot signal を 3 ページで取り切ったケース）。
        assert!(pagination_is_complete(3000, 3000));
        assert!(pagination_is_complete(3001, 3000));
        // 届かないまま終端したら不完全（取りこぼし）。query_nodes_paged が backend の
        // 「まだ残るのに短ページ」に対して fail closed する根拠でもある。
        assert!(!pagination_is_complete(2999, 3000));
        // 空（total 0）は完全。
        assert!(pagination_is_complete(0, 0));
        // total_count が非正（backend 未申告）なら検証スキップ＝完全扱い（短ページ終端に委ねる）。
        assert!(pagination_is_complete(0, -1));
        assert!(pagination_is_complete(5, -1));
    }
}
