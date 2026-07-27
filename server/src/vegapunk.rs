use crate::{
    model::{GraphBuild, GraphEdge, GraphNode},
    proto::graphrag::{
        create_schema_request, graph_rag_engine_client::GraphRagEngineClient, AttributeFilter,
        CreateSchemaRequest, Edge, EdgeTraversal, EmbedRequest, GetGraphSnapshotRequest,
        GetSchemaRequest, GetStatsRequest, GetStatsResponse, MergeRequest, Node, NodeAttribute,
        QueryNodesRequest, SearchDegradation, SearchExecution, SearchRequest, SearchResultItem,
        UpdateSchemaRequest, UpsertEdgesRequest, UpsertNodesRequest, UpsertVectorsRequest,
        VectorEntry,
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

/// gRPC channel の運用上限。マニュアル本文込みの graph snapshot が tonic 既定の
/// 4MiB decode 上限・短い timeout を超えるため、既定を引き上げている
/// （URTECT 実測 ~6MB / snapshot 読みで 30s 超）。環境ごとに締められるよう
/// config（vegapunk_timeout_secs / vegapunk_max_decode_mb）から上書き可能。
#[derive(Debug, Clone, Copy)]
pub struct GrpcLimits {
    pub timeout_secs: u64,
    pub max_decode_bytes: usize,
}

impl Default for GrpcLimits {
    fn default() -> Self {
        Self {
            timeout_secs: 120,
            max_decode_bytes: 64 * 1024 * 1024,
        }
    }
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
        let channel = Endpoint::from_shared(endpoint.to_string())?
            // connect_timeout は 30s。本番規模で TCP+TLS+h2 の確立が 10s では間に合わず
            // 常駐サーバのコールドな最初の 1 発が connect timeout に化ける事象への余裕。
            // 呼び出し自体の上限は別途 `timeout(limits.timeout_secs)` が握る。
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(limits.timeout_secs))
            // 長寿命チャネルがアイドル後に死んだ接続を掴んだまま 120s ハングする事象への対策
            // （実測: 新規接続の grpcurl は常に高速なのに、常駐サーバの呼び出しだけ停滞する）。
            // h2 PING keepalive で死活を検知し、切断時は再接続させる。
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .connect_lazy();
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
        let channel = Endpoint::from_shared(endpoint.to_string())?
            // connect_timeout は 30s。本番規模で TCP+TLS+h2 の確立が 10s では間に合わず
            // 常駐サーバのコールドな最初の 1 発が connect timeout に化ける事象への余裕。
            // 呼び出し自体の上限は別途 `timeout(limits.timeout_secs)` が握る。
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(limits.timeout_secs))
            // 長寿命チャネルがアイドル後に死んだ接続を掴んだまま 120s ハングする事象への対策
            // （実測: 新規接続の grpcurl は常に高速なのに、常駐サーバの呼び出しだけ停滞する）。
            // h2 PING keepalive で死活を検知し、切断時は再接続させる。
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .tcp_keepalive(Some(Duration::from_secs(60)))
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
            .map_err(|err| annotate_grpc_error(err, &format!("search schema {schema}")))?;
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
        .map_err(|err| annotate_grpc_error(err, &format!("get stats for schema {schema}")))
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
        _ => None,
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
    use super::{degradation_summary, merge_error_hint, page_is_last, pagination_is_complete};
    use tonic::Code;

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
