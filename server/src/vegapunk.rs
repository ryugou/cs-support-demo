use crate::{
    model::{GraphBuild, GraphEdge, GraphNode},
    proto::graphrag::{
        create_schema_request, graph_rag_engine_client::GraphRagEngineClient, AttributeFilter,
        CreateSchemaRequest, Edge, EdgeTraversal, EmbedRequest, GetGraphSnapshotRequest,
        GetSchemaRequest, Node, NodeAttribute, QueryNodesRequest, SearchRequest,
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
            .connect_timeout(Duration::from_secs(10))
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
            .connect_timeout(Duration::from_secs(10))
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
    pub async fn upsert_vectors(
        &self,
        entries: Vec<(String, Vec<f32>, Vec<(String, String)>)>,
    ) -> Result<i32> {
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
            let page = self
                .call(
                    |mut client, request| async move {
                        client
                            .query_nodes(request)
                            .await
                            .map(|resp| resp.into_inner().nodes)
                    },
                    req,
                )
                .await
                .with_context(|| format!("query nodes (paged {node_type}, offset {offset})"))?;
            let returned = page.len();
            all.extend(page);
            if page_is_last(returned, page_size as usize) {
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
    /// 隣接ノードの node_id 一覧を返す（`QueryNodes` の `traverse`）。
    ///
    /// stale-edge 検出で、`graph_snapshot` の全件依存を避けて「変更のあった既存 section 1 件」の
    /// 現行 edge だけをピンポイントに引くために使う。呼び出しは再 ingest で内容が変わった既存
    /// section にだけ発生するため、グラフ規模ではなく変更件数にスケールする。
    /// 返却件数が `limit` に達したら silent truncation の疑いがあるため fail closed
    /// （stale 検出を取りこぼすと除去漏れ edge が検索を汚し続けるため、fail-open を許さない）。
    pub async fn traverse_neighbor_ids(
        &self,
        schema: &str,
        neighbor_node_type: &str,
        edge_type: &str,
        direction: &str,
        source_node_id: &str,
        limit: i32,
    ) -> Result<Vec<String>> {
        let req = QueryNodesRequest {
            schema: schema.to_string(),
            node_type: neighbor_node_type.to_string(),
            filters: Vec::new(),
            sort_by: None,
            sort_order: None,
            limit: Some(limit),
            offset: Some(0),
            traverse: Some(EdgeTraversal {
                edge_type: edge_type.to_string(),
                direction: direction.to_string(),
                source_node_id: source_node_id.to_string(),
            }),
        };
        let nodes = self
            .call(
                |mut client, request| async move {
                    client
                        .query_nodes(request)
                        .await
                        .map(|resp| resp.into_inner().nodes)
                },
                req,
            )
            .await
            .with_context(|| format!("traverse {edge_type} {direction} from {source_node_id}"))?;
        if nodes.len() as i32 >= limit {
            anyhow::bail!(
                "traverse {edge_type} {direction} from {source_node_id} returned {} node(s) at the \
                 limit ({limit}); refusing to proceed with a possibly-truncated neighbor set for \
                 stale-edge detection",
                nodes.len()
            );
        }
        Ok(nodes.into_iter().map(|n| n.node_id).collect())
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

    pub async fn search(
        &self,
        schema: &str,
        query: &str,
        top_k: i32,
    ) -> Result<Vec<crate::proto::graphrag::SearchResultItem>> {
        let req = SearchRequest {
            text: query.to_string(),
            filter: None,
            depth: Some(1),
            top_k: Some(top_k),
            format: None,
            mode: Some("local".to_string()),
            schema: schema.to_string(),
            offset: Some(0),
            limit: Some(top_k),
            structural_weight: Some(0.0),
        };
        self.call(
            |mut client, request| async move {
                client
                    .search(request)
                    .await
                    .map(|resp| resp.into_inner().results)
            },
            req,
        )
        .await
        .context("search")
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
    use super::page_is_last;

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
}
