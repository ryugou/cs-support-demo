use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status};

use crate::proto::graphrag as pb;
use pb::graph_rag_engine_server::{GraphRagEngine, GraphRagEngineServer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeRecord {
    pub node_id: String,
    pub node_type: String,
    pub attributes: BTreeMap<String, String>,
}

impl NodeRecord {
    pub(crate) fn attr(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EdgeKey {
    from_id: String,
    to_id: String,
    edge_type: String,
}

#[derive(Default)]
struct GraphState {
    nodes: BTreeMap<String, NodeRecord>,
    edges: BTreeSet<EdgeKey>,
    unavailable_node_types: BTreeSet<String>,
    queried_node_types: Vec<String>,
}

#[derive(Clone, Default)]
pub(crate) struct GraphStore {
    state: Arc<Mutex<GraphState>>,
}

impl GraphStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, GraphState> {
        self.state
            .lock()
            .expect("fake vegapunk graph state poisoned")
    }

    pub(crate) fn upsert_node(
        &self,
        node_id: &str,
        node_type: &str,
        attributes: impl IntoIterator<Item = (String, String)>,
    ) {
        self.lock().nodes.insert(
            node_id.to_string(),
            NodeRecord {
                node_id: node_id.to_string(),
                node_type: node_type.to_string(),
                attributes: attributes.into_iter().collect(),
            },
        );
    }

    pub(crate) fn upsert_edge(&self, from_id: &str, to_id: &str, edge_type: &str) {
        self.lock().edges.insert(EdgeKey {
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            edge_type: edge_type.to_string(),
        });
    }

    pub(crate) fn node(&self, node_id: &str) -> Option<NodeRecord> {
        self.lock().nodes.get(node_id).cloned()
    }

    pub(crate) fn nodes_of_type(&self, schema: &str, node_type: &str) -> Vec<NodeRecord> {
        let prefix = schema_prefix(schema);
        self.lock()
            .nodes
            .values()
            .filter(|n| n.node_type == node_type && n.node_id.starts_with(&prefix))
            .cloned()
            .collect()
    }

    pub(crate) fn make_queries_unavailable_for(&self, node_type: &str) {
        self.lock()
            .unavailable_node_types
            .insert(node_type.to_string());
    }

    pub(crate) fn queried_node_types(&self) -> Vec<String> {
        self.lock().queried_node_types.clone()
    }

    fn record_query(&self, node_type: &str) -> bool {
        let mut state = self.lock();
        state.queried_node_types.push(node_type.to_string());
        !state.unavailable_node_types.contains(node_type)
    }

    fn query(&self, req: &pb::QueryNodesRequest) -> Result<pb::QueryNodesResponse, String> {
        let prefix = schema_prefix(&req.schema);
        let state = self.lock();
        let mut matched: Vec<&NodeRecord> = match &req.traverse {
            Some(traverse) => {
                let outgoing = match traverse.direction.as_str() {
                    "outgoing" => true,
                    "incoming" => false,
                    other => return Err(format!("unsupported traverse direction {other:?}")),
                };
                state
                    .edges
                    .iter()
                    .filter(|e| e.edge_type == traverse.edge_type)
                    .filter_map(|e| {
                        let (anchor, neighbor) = if outgoing {
                            (&e.from_id, &e.to_id)
                        } else {
                            (&e.to_id, &e.from_id)
                        };
                        (anchor == &traverse.source_node_id).then_some(neighbor)
                    })
                    .filter_map(|id| state.nodes.get(id))
                    .filter(|n| n.node_type == req.node_type)
                    .collect()
            }
            None => state
                .nodes
                .values()
                .filter(|n| n.node_type == req.node_type && n.node_id.starts_with(&prefix))
                .collect(),
        };
        matched.retain(|n| req.filters.iter().all(|f| filter_matches(n, f)));
        if let Some(sort_by) = req.sort_by.as_deref().filter(|s| !s.is_empty()) {
            let ascending = req.sort_order.as_deref() == Some("asc");
            matched.sort_by(|a, b| {
                let ord = compare_values(a.attr(sort_by), b.attr(sort_by));
                if ascending {
                    ord
                } else {
                    ord.reverse()
                }
            });
        }
        let total_count = matched.len() as i32;
        let offset = req.offset.unwrap_or(0).max(0) as usize;
        let limit = req.limit.unwrap_or(50).clamp(0, 1000) as usize;
        let nodes = matched
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|n| pb::NodeResult {
                node_id: n.node_id.clone(),
                node_type: n.node_type.clone(),
                attributes: n.attributes.clone().into_iter().collect(),
            })
            .collect();
        Ok(pb::QueryNodesResponse { nodes, total_count })
    }

    fn snapshot(&self, req: &pb::GetGraphSnapshotRequest) -> pb::GetGraphSnapshotResponse {
        let prefix = schema_prefix(&req.schema);
        let state = self.lock();
        let nodes: Vec<pb::GraphNode> = state
            .nodes
            .values()
            .filter(|n| n.node_id.starts_with(&prefix))
            .filter(|n| req.node_types.is_empty() || req.node_types.contains(&n.node_type))
            .map(|n| pb::GraphNode {
                node_id: n.node_id.clone(),
                node_type: n.node_type.clone(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: n.attributes.clone().into_iter().collect(),
            })
            .collect();
        let ids: BTreeSet<&str> = nodes.iter().map(|n| n.node_id.as_str()).collect();
        let edges = state
            .edges
            .iter()
            .filter(|e| ids.contains(e.from_id.as_str()) && ids.contains(e.to_id.as_str()))
            .map(|e| pb::GraphEdge {
                edge_id: format!("{}|{}|{}", e.from_id, e.edge_type, e.to_id),
                from_id: e.from_id.clone(),
                to_id: e.to_id.clone(),
                edge_type: e.edge_type.clone(),
            })
            .collect();
        let total_node_count = nodes.len() as i32;
        pb::GetGraphSnapshotResponse {
            nodes,
            edges,
            truncated: false,
            total_node_count,
        }
    }
}

fn schema_prefix(schema: &str) -> String {
    crate::ingest::schema_generation_prefix(schema)
}

fn compare_values(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => match (a.parse::<f64>(), b.parse::<f64>()) {
            (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            _ => a.cmp(b),
        },
    }
}

fn filter_matches(node: &NodeRecord, filter: &pb::AttributeFilter) -> bool {
    let Some(actual) = node.attr(&filter.key) else {
        return false;
    };
    let ord = compare_values(Some(actual), Some(&filter.value));
    match filter.op.as_str() {
        "eq" => ord.is_eq(),
        "gt" => ord.is_gt(),
        "gte" => ord.is_ge(),
        "lt" => ord.is_lt(),
        "lte" => ord.is_le(),
        _ => false,
    }
}

fn attrs_of(attributes: Vec<pb::NodeAttribute>) -> Vec<(String, String)> {
    attributes.into_iter().map(|a| (a.key, a.value)).collect()
}

struct FakeEngine {
    store: GraphStore,
}

impl FakeEngine {
    fn put_nodes(&self, nodes: Vec<pb::Node>) -> i32 {
        let count = nodes.len() as i32;
        for node in nodes {
            self.store
                .upsert_node(&node.id, &node.r#type, attrs_of(node.attributes));
        }
        count
    }

    fn put_edges(&self, edges: Vec<pb::Edge>) -> i32 {
        let count = edges.len() as i32;
        for edge in edges {
            self.store
                .upsert_edge(&edge.from_id, &edge.to_id, &edge.r#type);
        }
        count
    }
}

macro_rules! engine_impl {
    (
        impl { $($implemented:tt)* }
        unimplemented { $($method:ident: $req:ident => $resp:ident,)* }
    ) => {
        #[tonic::async_trait]
        impl GraphRagEngine for FakeEngine {
            $($implemented)*
            $(
                async fn $method(
                    &self,
                    _request: Request<pb::$req>,
                ) -> Result<Response<pb::$resp>, Status> {
                    Err(Status::unimplemented(concat!(
                        "fake vegapunk does not implement ",
                        stringify!($method),
                        "; the reply path is not expected to call it"
                    )))
                }
            )*
        }
    };
}

engine_impl! {
    impl {
    async fn upsert_nodes(
        &self,
        request: Request<pb::UpsertNodesRequest>,
    ) -> Result<Response<pb::UpsertNodesResponse>, Status> {
        let upserted_count = self.put_nodes(request.into_inner().nodes);
        Ok(Response::new(pb::UpsertNodesResponse { upserted_count }))
    }

    async fn upsert_edges(
        &self,
        request: Request<pb::UpsertEdgesRequest>,
    ) -> Result<Response<pb::UpsertEdgesResponse>, Status> {
        let upserted_count = self.put_edges(request.into_inner().edges);
        Ok(Response::new(pb::UpsertEdgesResponse { upserted_count }))
    }

    async fn upsert_graph(
        &self,
        request: Request<pb::UpsertGraphRequest>,
    ) -> Result<Response<pb::UpsertGraphResponse>, Status> {
        let graph = request.into_inner();
        let upserted_nodes = self.put_nodes(graph.nodes);
        let upserted_edges = self.put_edges(graph.edges);
        Ok(Response::new(pb::UpsertGraphResponse {
            upserted_nodes,
            upserted_edges,
            upserted_vectors: 0,
        }))
    }

    async fn query_nodes(
        &self,
        request: Request<pb::QueryNodesRequest>,
    ) -> Result<Response<pb::QueryNodesResponse>, Status> {
        let req = request.get_ref();
        if !self.store.record_query(&req.node_type) {
            return Err(Status::unavailable(format!(
                "fake vegapunk makes queries for {} unavailable",
                req.node_type
            )));
        }
        self.store
            .query(req)
            .map(Response::new)
            .map_err(Status::invalid_argument)
    }

    async fn get_graph_snapshot(
        &self,
        request: Request<pb::GetGraphSnapshotRequest>,
    ) -> Result<Response<pb::GetGraphSnapshotResponse>, Status> {
        Ok(Response::new(self.store.snapshot(request.get_ref())))
    }

    async fn search(
        &self,
        _request: Request<pb::SearchRequest>,
    ) -> Result<Response<pb::SearchResponse>, Status> {
        Ok(Response::new(pb::SearchResponse::default()))
    }
    }
    unimplemented {
        ingest: IngestRequest => IngestResponse,
        upsert_vectors: UpsertVectorsRequest => UpsertVectorsResponse,
        delete_nodes: DeleteNodesRequest => DeleteNodesResponse,
        delete_edges: DeleteEdgesRequest => DeleteEdgesResponse,
        delete_vectors: DeleteVectorsRequest => DeleteVectorsResponse,
        merge: MergeRequest => MergeResponse,
        rebuild: RebuildRequest => RebuildResponse,
        backup: BackupRequest => BackupResponse,
        migrate: MigrateRequest => MigrateResponse,
        feedback: FeedbackRequest => FeedbackResponse,
        get_needs_review: GetNeedsReviewRequest => GetNeedsReviewResponse,
        resolve_match: ResolveMatchRequest => ResolveMatchResponse,
        get_job_status: GetJobStatusRequest => GetJobStatusResponse,
        get_ingest_status: GetIngestStatusRequest => GetIngestStatusResponse,
        ingest_raw: IngestRawRequest => IngestRawResponse,
        ingest_file: IngestFileRequest => IngestFileResponse,
        delete_schema: DeleteSchemaRequest => DeleteSchemaResponse,
        reingest: ReingestRequest => ReingestResponse,
        improve_prompts: ImprovePromptsRequest => ImprovePromptsResponse,
        create_schema: CreateSchemaRequest => CreateSchemaResponse,
        get_schema: GetSchemaRequest => GetSchemaResponse,
        list_schemas: ListSchemasRequest => ListSchemasResponse,
        update_schema: UpdateSchemaRequest => UpdateSchemaResponse,
        list_schema_templates: ListSchemaTemplatesRequest => ListSchemaTemplatesResponse,
        get_schema_migration_status: GetSchemaMigrationStatusRequest => GetSchemaMigrationStatusResponse,
        purge_raw_messages: PurgeRawMessagesRequest => PurgeRawMessagesResponse,
        set_maintenance_mode: SetMaintenanceModeRequest => MaintenanceModeResponse,
        get_maintenance_mode: GetMaintenanceModeRequest => MaintenanceModeResponse,
        list_jobs: ListJobsRequest => ListJobsResponse,
        get_stats: GetStatsRequest => GetStatsResponse,
        get_evaluation_history: GetEvaluationHistoryRequest => GetEvaluationHistoryResponse,
        list_recent_searches: ListRecentSearchesRequest => ListRecentSearchesResponse,
        get_vectors: GetVectorsRequest => GetVectorsResponse,
        get_traceable_chain: GetTraceableChainRequest => GetTraceableChainResponse,
        embed: EmbedRequest => EmbedResponse,
    }
}

pub(crate) struct FakeVegapunk {
    pub store: GraphStore,
    pub endpoint: String,
    task: tokio::task::JoinHandle<()>,
}

impl FakeVegapunk {
    pub(crate) async fn start(store: GraphStore) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port for the fake vegapunk");
        let addr = listener.local_addr().expect("read the fake vegapunk addr");
        let incoming = TcpIncoming::from_listener(listener, true, None)
            .expect("accept connections on the fake vegapunk listener");
        let service = GraphRagEngineServer::new(FakeEngine {
            store: store.clone(),
        });
        let task = tokio::spawn(async move {
            if let Err(err) = tonic::transport::Server::builder()
                .add_service(service)
                .serve_with_incoming(incoming)
                .await
            {
                eprintln!("fake vegapunk server stopped: {err}");
            }
        });
        Self {
            store,
            endpoint: format!("http://{addr}"),
            task,
        }
    }
}

impl Drop for FakeVegapunk {
    fn drop(&mut self) {
        self.task.abort();
    }
}
