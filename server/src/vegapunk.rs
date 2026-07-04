use crate::{
    model::{GraphBuild, GraphEdge, GraphNode},
    proto::graphrag::{
        create_schema_request, graph_rag_engine_client::GraphRagEngineClient, AttributeFilter,
        CreateSchemaRequest, Edge, GetGraphSnapshotRequest, GetSchemaRequest, Node, NodeAttribute,
        QueryNodesRequest, SearchRequest, UpdateSchemaRequest, UpsertEdgesRequest,
        UpsertNodesRequest,
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

impl VegapunkClient {
    pub fn connect_lazy(endpoint: &str, bearer_token: &str) -> Result<Self> {
        let channel = Endpoint::from_shared(endpoint.to_string())?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .connect_lazy();
        let auth_header = MetadataValue::try_from(format!("Bearer {bearer_token}"))
            .context("invalid bearer token metadata")?;
        Ok(Self {
            inner: GraphRagEngineClient::new(channel),
            auth_header,
        })
    }

    pub async fn connect(endpoint: &str, bearer_token: &str) -> Result<Self> {
        let channel = Endpoint::from_shared(endpoint.to_string())?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .connect()
            .await
            .with_context(|| format!("connect vegapunk endpoint {endpoint}"))?;
        let auth_header = MetadataValue::try_from(format!("Bearer {bearer_token}"))
            .context("invalid bearer token metadata")?;
        Ok(Self {
            inner: GraphRagEngineClient::new(channel),
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
