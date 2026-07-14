use crate::{
    model::{GraphBuild, GraphEdge, GraphNode},
    proto::graphrag::{
        create_schema_request, graph_rag_engine_client::GraphRagEngineClient, AttributeFilter,
        CreateSchemaRequest, Edge, EmbedRequest, GetGraphSnapshotRequest, GetSchemaRequest, Node,
        NodeAttribute, QueryNodesRequest, SearchRequest, UpdateSchemaRequest, UpsertEdgesRequest,
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
