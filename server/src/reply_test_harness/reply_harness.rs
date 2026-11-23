use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::conversation::Conversation;
use super::fake_vegapunk::{FakeVegapunk, GraphStore, NodeRecord};
use super::fixture::Fixture;
use super::llm_stub::LlmStub;
use super::temp_dir::TempDir;
use super::PROJECT_ID;
use crate::api::{api_router, ApiState};
use crate::config::AppConfig;
use crate::harness::audit::WormAuditLog;
use crate::harness::knowledge::{csv_list, harness_node_id};
use crate::harness::{CaseConvState, Harness};
use crate::mcp::ToolService;
use crate::oauth::VerifiedIdentity;
use crate::vegapunk::{GrpcLimits, VegapunkClient};

const API_KEY: &str = "reply-test-harness-key";
const REPLY_BODY_LIMIT_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LlmBackend {
    Stub,
    Anthropic,
}

pub(crate) struct ReplyHarness {
    fake: FakeVegapunk,
    llm: LlmStub,
    state: ApiState,
    audit_log_path: PathBuf,
    _dir: TempDir,
}

impl ReplyHarness {
    pub(crate) async fn start(fixture: Fixture) -> Self {
        Self::start_with(fixture, LlmBackend::Stub).await
    }

    pub(crate) async fn start_with(fixture: Fixture, backend: LlmBackend) -> Self {
        let store = GraphStore::default();
        fixture.seed(&store);
        let fake = FakeVegapunk::start(store).await;
        let llm = LlmStub::start().await;
        let dir = TempDir::new();
        let audit_log_path = dir.path().join("audit.jsonl");
        let config = harness_config(&fake, &llm, &dir, &audit_log_path, backend);
        let client = VegapunkClient::connect_lazy_with_limits(
            &config.vegapunk_endpoint,
            "",
            GrpcLimits::default(),
        )
        .expect("lazy connect never touches the network");
        let harness = Harness::build(
            &config,
            Arc::new(client.clone()),
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .expect("build the production harness against the test doubles");
        let state = ApiState {
            config: Arc::new(config),
            harness: Arc::new(harness),
            tools: ToolService::new(client),
            api_key: API_KEY.to_string(),
        };
        Self {
            fake,
            llm,
            state,
            audit_log_path,
            _dir: dir,
        }
    }

    pub(crate) fn llm(&self) -> &LlmStub {
        &self.llm
    }

    pub(crate) fn store(&self) -> &GraphStore {
        &self.fake.store
    }

    pub(crate) fn api_state(&self) -> ApiState {
        self.state.clone()
    }

    pub(crate) fn harness(&self) -> &Harness {
        &self.state.harness
    }

    pub(crate) fn audit_log(&self) -> Arc<WormAuditLog> {
        self.state.harness.worm.clone()
    }

    pub(crate) fn vegapunk_client(&self) -> VegapunkClient {
        VegapunkClient::connect_lazy_with_limits(&self.fake.endpoint, "", GrpcLimits::default())
            .expect("lazy connect never touches the network")
    }

    pub(crate) fn conversation(&self) -> Conversation<'_> {
        Conversation::new(self)
    }

    pub(crate) async fn post_reply(&self, body: serde_json::Value) -> (StatusCode, String) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/{PROJECT_ID}/api/reply"))
            .header("authorization", format!("Bearer {API_KEY}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build the reply request");
        let response = api_router(self.state.clone())
            .oneshot(request)
            .await
            .expect("the router never fails");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), REPLY_BODY_LIMIT_BYTES)
            .await
            .expect("read the reply body");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    pub(crate) fn case_attributes(&self, case_id: &str) -> Option<NodeRecord> {
        self.fake
            .store
            .node(&harness_node_id(PROJECT_ID, "support_case", case_id))
    }

    pub(crate) fn product_models(&self, case_id: &str) -> Vec<String> {
        self.case_attributes(case_id)
            .and_then(|case| case.attr("product_models").map(csv_list))
            .unwrap_or_default()
    }

    pub(crate) async fn conv_state(&self, case_id: &str) -> CaseConvState {
        let identity = VerifiedIdentity {
            sub: "service:answer-api".to_string(),
            email: "answer-api@cs-support.internal".to_string(),
        };
        let ctx = self
            .state
            .harness
            .begin(
                &identity,
                &self.state.config.projects[0].schema,
                self.state.config.projects[0].manual_schema,
            )
            .expect("begin a request context for reading the conversation state");
        self.state
            .harness
            .load_conv_state(&ctx, case_id)
            .await
            .expect("load the conversation state from the fake vegapunk")
    }

    pub(crate) fn conversation_turns(&self, case_id: &str) -> Vec<NodeRecord> {
        let mut turns: Vec<NodeRecord> = self
            .fake
            .store
            .nodes_of_type(PROJECT_ID, "ConversationTurn")
            .into_iter()
            .filter(|turn| turn.attr("case_id") == Some(case_id))
            .collect();
        turns.sort_by_key(|turn| {
            turn.attr("seq")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0)
        });
        turns
    }

    pub(crate) fn audit_events(&self) -> Vec<serde_json::Value> {
        let Ok(body) = std::fs::read_to_string(&self.audit_log_path) else {
            return Vec::new();
        };
        body.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("parse an audit log line"))
            .collect()
    }

    pub(crate) fn audit_decision(&self, audit_event_id: &str) -> Option<String> {
        self.audit_events()
            .into_iter()
            .find(|event| event["event_id"] == audit_event_id)
            .and_then(|event| event["decision"].as_str().map(ToString::to_string))
    }
}

fn harness_config(
    fake: &FakeVegapunk,
    llm: &LlmStub,
    dir: &TempDir,
    audit_log_path: &std::path::Path,
    backend: LlmBackend,
) -> AppConfig {
    let mut config: AppConfig = toml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/config.cloudrun.toml"
    )))
    .expect("parse config.cloudrun.toml");
    config.vegapunk_endpoint = fake.endpoint.clone();
    match backend {
        LlmBackend::Stub => {
            let api_key_file = dir.path().join("llm-api-key");
            std::fs::write(&api_key_file, "stub-key\n").expect("write the stub LLM key file");
            config.llm.endpoint = llm.endpoint.clone();
            config.llm.api_key_file = Some(api_key_file.to_string_lossy().to_string());
        }
        LlmBackend::Anthropic => config.llm.api_key_file = None,
    }
    config.harness.audit_log_path = audit_log_path.to_string_lossy().to_string();
    config.harness.search_improvement_queue_path =
        dir.path().join("queue.jsonl").to_string_lossy().to_string();
    config.harness.vector_route_enabled = false;
    config.jev.enabled = false;
    assert_eq!(
        config.projects.len(),
        1,
        "config.cloudrun.toml must keep exactly one project"
    );
    assert_eq!(config.projects[0].project_id, PROJECT_ID);
    config
}
