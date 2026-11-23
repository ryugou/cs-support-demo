//! 返信API（`POST /{project_id}/api/reply`）の複数ターン検査基盤（Issue #153）。
//!
//! 本番と同じ `api_router` → `reply_handler` → `Harness::evaluate()` の経路を、外部へ接続せずに
//! 複数ターン駆動する。本番コードは差し替えず、接続先だけをテスト用の代替へ向ける。
//!
//! | 本番の接続先 | この基盤の代替 |
//! |---|---|
//! | vegapunk（GraphRAG gRPC） | [`fake_vegapunk::FakeVegapunk`]（メモリ上のグラフ。`VegapunkClient` がそのまま接続する） |
//! | Anthropic Messages API | [`llm_stub::LlmStub`]（呼び出しの種類ごとに応答を決め、受け取ったリクエストを記録する） |
//! | 監査ログ（WORM） | 一時ディレクトリの JSONL（本番と同じ `WormAuditLog`） |
//!
//! `Harness` は `Harness::build` で本番と同じ配線（lexicon・NG 辞書・しきい値・製品ゲート・
//! 材料 corpus・各 LLM クライアント）を組み立てる。設定は同梱の `config.cloudrun.toml` を読み、
//! 接続先・出力先・Jev・ベクトル経路だけを上書きする。
//!
//! 使い方は [`ReplyHarness::start`] と [`Conversation::say`] を参照。製品データや材料は
//! [`Fixture`] で与える。

pub(crate) mod fake_vegapunk;
pub(crate) mod llm_stub;

mod tests;

use self::fake_vegapunk::{FakeVegapunk, GraphStore, NodeRecord};
use self::llm_stub::{LlmKind, LlmRequest, LlmStub};
use crate::api::{api_router, ApiState};
use crate::config::AppConfig;
use crate::harness::Harness;
use crate::manual::ingest_model::{
    build_document_node, build_product_node, build_section_graph, content_hash, ManualProductInput,
    ManualSectionInput,
};
use crate::manual::schema_ids::manual_node_id;
use crate::mcp::ToolService;
use crate::model::{GraphBuild, GraphNode};
use crate::vegapunk::{GrpcLimits, VegapunkClient};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

/// 基盤が使う project（`config.cloudrun.toml` の唯一の project と同じ）。
pub(crate) const PROJECT_ID: &str = "urtect";
/// 返信APIの認証に使うキー（テスト内だけで通用する値）。
const API_KEY: &str = "reply-test-harness-key";
/// LINE アダプタ（`server/src/bin/line_adapter.rs` の `MAX_HISTORY_TURNS`）と同じ履歴の保持上限。
const MAX_HISTORY_TURNS: usize = 20;

// ---------------------------------------------------------------------------
// フィクスチャ
// ---------------------------------------------------------------------------

/// vegapunk 代替へ投入するデータ（製品、マニュアル節、エスカレーションルール、任意のノード）。
///
/// 本番データ（`server/data/urtect/products.json` など）は使わない。ノード・辺は本番の投入
/// 処理と同じ関数（`build_product_node`・`build_section_graph`）で組み立てるので、本番の
/// 読み出し処理が読む形になる。
pub(crate) struct Fixture {
    graph: GraphBuild,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self {
            graph: GraphBuild {
                nodes: Vec::new(),
                edges: Vec::new(),
            },
        }
    }

    /// 製品ノード 1 件（`model`・`name`・`aliases`）。`aliases` はカンマ区切りで保存される。
    pub(crate) fn product(mut self, model: &str, name: &str, aliases: &[&str]) -> Self {
        self.graph.nodes.push(build_product_node(
            PROJECT_ID,
            &ManualProductInput {
                model: model.to_string(),
                name: name.to_string(),
                aliases: aliases.iter().map(ToString::to_string).collect(),
            },
        ));
        self
    }

    /// 検査設計 §1.4 の F-P7: 現行の製品データと同じ 7 型番（`name` は `model` と同じ、別名なし）。
    pub(crate) fn products_f_p7(mut self) -> Self {
        for model in [
            "ADC-V523",
            "ADC-V523X",
            "ADC-V724",
            "ADC-V724X",
            "ADC-VC729P",
            "ADC-VC727P",
            "ADC-VC827P",
        ] {
            self = self.product(model, model, &[]);
        }
        self
    }

    /// 検査設計 §1.4 の F-NAME: 製品名と別名を持つテスト用の製品。F-P7 と同じ型番に付ける
    /// （`products_f_p7` の代わりに使う。両方を呼ぶと同じ型番のノードを上書きする）。
    pub(crate) fn products_f_name(mut self) -> Self {
        self = self.product("ADC-V724", "みまもりドームカメラ", &["見守りドーム"]);
        self = self.product("ADC-VC729P", "おでかけパノラマカメラ", &[]);
        for model in [
            "ADC-V523",
            "ADC-V523X",
            "ADC-V724X",
            "ADC-VC727P",
            "ADC-VC827P",
        ] {
            self = self.product(model, model, &[]);
        }
        self
    }

    /// 任意の種類のノード（社名など）。`node_id` は `manual_node_id(project, kind, key)`。
    pub(crate) fn node(mut self, kind: &str, key: &str, attributes: &[(&str, &str)]) -> Self {
        self.graph.nodes.push(GraphNode {
            id: manual_node_id(PROJECT_ID, kind, key),
            node_type: kind.to_string(),
            attributes: attributes
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
        self
    }

    /// マニュアル節 1 件（`DESCRIBES` で製品へ、`MENTIONS_SIGNAL` で signal へ結ぶ）。
    /// 節の見出し・本文・パンくずは材料の検索に使われる。
    pub(crate) fn manual_section(
        mut self,
        slug: &str,
        title: &str,
        body: &str,
        describes_models: &[&str],
        signals: &[&str],
    ) -> Self {
        let doc_key = "reply-test-harness-doc";
        let doc = build_document_node(
            PROJECT_ID,
            doc_key,
            "テスト用マニュアル",
            "https://example.invalid/manual",
            "2026-01-01T00:00:00Z",
        );
        if !self.graph.nodes.iter().any(|n| n.id == doc.id) {
            self.graph.nodes.push(doc);
        }
        let section = ManualSectionInput {
            slug: slug.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            source_url: format!("https://example.invalid/manual/{slug}"),
            breadcrumb: title.to_string(),
            section_no: None,
            order: 0,
            parent_slug: None,
            product_models: describes_models.iter().map(ToString::to_string).collect(),
            signal_values: signals.iter().map(ToString::to_string).collect(),
            body_original: None,
            original_hash: None,
            concept_keys: Vec::new(),
        };
        let build = build_section_graph(PROJECT_ID, doc_key, &section, &content_hash(body));
        self.graph.nodes.extend(build.nodes);
        self.graph.edges.extend(build.edges);
        self
    }

    /// 本番の `server/data/urtect/rules.json` のエスカレーションルールと禁止領域を投入する。
    /// 本番と同じ判定ルール（取次の条件）で動かしたいときに使う。
    pub(crate) fn bundled_rules(mut self) -> Self {
        #[derive(serde::Deserialize)]
        struct Rules {
            escalation_rules: Vec<serde_json::Map<String, serde_json::Value>>,
            #[serde(default)]
            prohibited_domains: Vec<serde_json::Map<String, serde_json::Value>>,
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/urtect/rules.json");
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read bundled rules {path:?}: {e}"));
        let rules: Rules = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("parse bundled rules {path:?}: {e}"));
        let flatten = |entry: &serde_json::Map<String, serde_json::Value>| {
            entry
                .iter()
                .map(|(key, value)| {
                    let text = match value {
                        serde_json::Value::String(s) => s.clone(),
                        // `condition` / `domain_signals` / `pattern` は配列。本番の投入処理と
                        // 同じカンマ区切りで保存する。
                        serde_json::Value::Array(items) => items
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                        other => other.to_string(),
                    };
                    (key.clone(), text)
                })
                .collect::<Vec<_>>()
        };
        for rule in &rules.escalation_rules {
            let attributes = flatten(rule);
            let key = rule["rule_id"].as_str().expect("rule_id").to_string();
            self.graph.nodes.push(GraphNode {
                id: crate::harness::knowledge::harness_node_id(PROJECT_ID, "EscalationRule", &key),
                node_type: "EscalationRule".to_string(),
                attributes,
            });
        }
        for domain in &rules.prohibited_domains {
            let attributes = flatten(domain);
            let key = domain["domain_id"].as_str().expect("domain_id").to_string();
            self.graph.nodes.push(GraphNode {
                id: crate::harness::knowledge::harness_node_id(
                    PROJECT_ID,
                    "ProhibitedDomain",
                    &key,
                ),
                node_type: "ProhibitedDomain".to_string(),
                attributes,
            });
        }
        self
    }

    fn seed(&self, store: &GraphStore) {
        for node in &self.graph.nodes {
            store.upsert_node(&node.id, &node.node_type, node.attributes.clone());
        }
        for edge in &self.graph.edges {
            store.upsert_edge(&edge.from_id, &edge.to_id, &edge.edge_type);
        }
    }
}

// ---------------------------------------------------------------------------
// 基盤本体
// ---------------------------------------------------------------------------

/// 起動済みの検査基盤。1 インスタンスが 1 つの vegapunk 代替・スタブ LLM・`Harness` を持つ。
/// 会話は [`ReplyHarness::conversation`] で作る（会話ごとに履歴と `case_id` を持つ）。
pub(crate) struct ReplyHarness {
    fake: FakeVegapunk,
    llm: LlmStub,
    state: ApiState,
    audit_log_path: PathBuf,
    _dir: tempfile_dir::TempDir,
}

impl ReplyHarness {
    /// 代替を起動し、`fixture` を投入して、本番と同じ配線の `Harness` と `api_router` を作る。
    /// `#[tokio::test]` の中で呼ぶこと。
    pub(crate) async fn start(fixture: Fixture) -> Self {
        let store = GraphStore::default();
        fixture.seed(&store);
        let fake = FakeVegapunk::start(store).await;
        let llm = LlmStub::start().await;
        let dir = tempfile_dir::TempDir::new();

        let api_key_file = dir.path().join("llm-api-key");
        std::fs::write(&api_key_file, "stub-key\n").expect("write the stub LLM key file");
        let audit_log_path = dir.path().join("audit.jsonl");

        let mut config: AppConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/config.cloudrun.toml"
        )))
        .expect("parse config.cloudrun.toml");
        config.vegapunk_endpoint = fake.endpoint.clone();
        config.llm.endpoint = llm.endpoint.clone();
        config.llm.api_key_file = Some(api_key_file.to_string_lossy().to_string());
        config.harness.audit_log_path = audit_log_path.to_string_lossy().to_string();
        config.harness.search_improvement_queue_path =
            dir.path().join("queue.jsonl").to_string_lossy().to_string();
        // ベクトル（意味検索）は vegapunk 代替が持たないため使わない。Jev は外部 API。
        config.harness.vector_route_enabled = false;
        config.jev.enabled = false;
        assert_eq!(
            config.projects.len(),
            1,
            "config.cloudrun.toml must keep exactly one project"
        );
        assert_eq!(config.projects[0].project_id, PROJECT_ID);

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

    /// スタブ LLM。応答の決め方の変更と、受け取ったリクエストの検証に使う。
    pub(crate) fn llm(&self) -> &LlmStub {
        &self.llm
    }

    /// vegapunk 代替のグラフ。保存された状態の読み出しと、追加のデータ投入に使う。
    pub(crate) fn store(&self) -> &GraphStore {
        &self.fake.store
    }

    /// 新しい会話（履歴なし・`case_id` なし）。
    pub(crate) fn conversation(&self) -> Conversation<'_> {
        Conversation {
            harness: self,
            case_id: None,
            history: VecDeque::new(),
        }
    }

    /// `POST /{project_id}/api/reply` を 1 回呼ぶ（`api_router` の HTTP ルートを入口にする）。
    /// `body` はリクエスト本文の JSON をそのまま渡す。認証済みで送る。
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
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("read the reply body");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// 案件ノード（`support_case`）の全属性。`product_models`（確定した型番の CSV）、
    /// 会話状態（`clarify_turns` など）、`last_decision` などが読める。
    pub(crate) fn case_attributes(&self, case_id: &str) -> Option<NodeRecord> {
        self.fake
            .store
            .node(&crate::harness::knowledge::harness_node_id(
                PROJECT_ID,
                "support_case",
                case_id,
            ))
    }

    /// 案件の確定した型番（`product_models`、辞書順）。
    pub(crate) fn product_models(&self, case_id: &str) -> Vec<String> {
        self.case_attributes(case_id)
            .and_then(|case| {
                case.attr("product_models")
                    .map(crate::harness::knowledge::csv_list)
            })
            .unwrap_or_default()
    }

    /// 案件の会話状態（聞き返し回数、時間帯待ち、取次確定など）。
    pub(crate) async fn conv_state(&self, case_id: &str) -> crate::harness::CaseConvState {
        let identity = crate::oauth::VerifiedIdentity {
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

    /// 案件の会話ターン（`seq` 昇順）。`reply_kind`・`question`・`reply_text`・`audit_event_id` が読める。
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

    /// 監査ログ（WORM）の全イベント（書かれた順）。`decision`・`route`・`extraction_mode` などが読める。
    pub(crate) fn audit_events(&self) -> Vec<serde_json::Value> {
        let Ok(body) = std::fs::read_to_string(&self.audit_log_path) else {
            return Vec::new();
        };
        body.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("parse an audit log line"))
            .collect()
    }

    /// 監査イベント id から `decision` を引く。
    pub(crate) fn audit_decision(&self, audit_event_id: &str) -> Option<String> {
        self.audit_events()
            .into_iter()
            .find(|event| event["event_id"] == audit_event_id)
            .and_then(|event| event["decision"].as_str().map(ToString::to_string))
    }
}

// ---------------------------------------------------------------------------
// 会話ドライバ
// ---------------------------------------------------------------------------

/// 会話の 1 ターンの結果。応答と、そのターンの観測点をまとめる。
#[derive(Debug, Clone)]
pub(crate) struct Turn {
    /// この会話の何ターン目か（1 始まり）。
    pub index: usize,
    /// 送った顧客発話。
    pub message: String,
    pub status: StatusCode,
    /// 応答の `reply_text`（200 以外は空）。
    pub reply_text: String,
    /// 応答の `case_id`（200 以外は空）。
    pub case_id: String,
    /// 応答の `case_reset`。
    pub case_reset: bool,
    /// 200 以外のときの応答本文。
    pub error_body: Option<String>,
    /// このターンの会話ターン記録の `reply_kind`（`answer`・`clarify`・`escalation`・
    /// `out_of_scope`・`time_pref` など）。
    pub reply_kind: Option<String>,
    /// このターンの監査イベントの `decision`（例: `escalate:layer3:product_unidentified`）。
    pub decision: Option<String>,
    /// このターンの終了時点の、案件の確定した型番（`product_models`）。
    pub product_models: Vec<String>,
    /// このターンで LLM が受け取ったリクエスト（受け取った順）。
    pub llm_requests: Vec<LlmRequest>,
}

impl Turn {
    /// このターンの LLM リクエストのうち、種類が `kind` のもの。
    pub(crate) fn llm_requests_of(&self, kind: LlmKind) -> Vec<&LlmRequest> {
        self.llm_requests
            .iter()
            .filter(|r| r.kind == kind)
            .collect()
    }

    /// ログ 1 行（`l2-reply-multiturn.log` の形式）。
    pub(crate) fn log_line(&self) -> String {
        format!(
            "turn={} status={} message={:?} reply_kind={:?} decision={:?} case_id={:?} \
             case_reset={} product_models={:?} reply_text={:?}",
            self.index,
            self.status.as_u16(),
            self.message,
            self.reply_kind,
            self.decision,
            self.case_id,
            self.case_reset,
            self.product_models,
            self.reply_text,
        )
    }
}

/// 1 つの会話。LINE アダプタと同じく、応答の `case_id` を次のターンへ渡し、`history` に
/// それまでの顧客発話と応答の `reply_text` を時系列の昇順で渡す。`case_reset: true` の応答を
/// 受けたら、それまでの履歴を捨て、そのターンの顧客発話と応答から履歴を作り直す。
pub(crate) struct Conversation<'h> {
    harness: &'h ReplyHarness,
    case_id: Option<String>,
    /// `(role, text)`。role は API の値（`customer` / `assistant`）。
    history: VecDeque<(&'static str, String)>,
}

impl Conversation<'_> {
    /// 顧客発話を 1 ターン送り、応答と観測点を返す。200 のときだけ `case_id` と履歴を更新する
    /// （LINE アダプタは 200 以外では状態を変えない）。
    pub(crate) async fn say(&mut self, message: &str) -> Turn {
        let index = self
            .history
            .iter()
            .filter(|(r, _)| *r == "customer")
            .count()
            + 1;
        let history: Vec<serde_json::Value> = self
            .history
            .iter()
            .map(|(role, text)| serde_json::json!({"role": role, "text": text}))
            .collect();
        let mut body = serde_json::json!({"message": message});
        if !history.is_empty() {
            body["history"] = serde_json::Value::Array(history);
        }
        if let Some(case_id) = &self.case_id {
            body["case_id"] = serde_json::Value::String(case_id.clone());
        }

        let llm_before = self.harness.llm.request_count();
        let (status, text) = self.harness.post_reply(body).await;
        let llm_requests = self.harness.llm.requests()[llm_before..].to_vec();

        if status != StatusCode::OK {
            return Turn {
                index,
                message: message.to_string(),
                status,
                reply_text: String::new(),
                case_id: String::new(),
                case_reset: false,
                error_body: Some(text),
                reply_kind: None,
                decision: None,
                product_models: Vec::new(),
                llm_requests,
            };
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("a 200 reply body is JSON");
        let reply_text = parsed["reply_text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let case_id = parsed["case_id"].as_str().unwrap_or_default().to_string();
        let case_reset = parsed["case_reset"].as_bool().unwrap_or(false);

        let turns = self.harness.conversation_turns(&case_id);
        let recorded = turns.last();
        let reply_kind = recorded
            .and_then(|t| t.attr("reply_kind"))
            .map(ToString::to_string);
        let decision = recorded
            .and_then(|t| t.attr("audit_event_id"))
            .and_then(|id| self.harness.audit_decision(id));
        let product_models = self.harness.product_models(&case_id);

        // LINE アダプタ（`handle_event`）と同じ更新: `case_reset` なら履歴を捨て、顧客発話と
        // 応答を積む。上限を超えたら古い側から捨てる。
        if case_reset {
            self.history.clear();
        }
        self.case_id = Some(case_id.clone());
        self.history.push_back(("customer", message.to_string()));
        self.history.push_back(("assistant", reply_text.clone()));
        while self.history.len() > MAX_HISTORY_TURNS {
            self.history.pop_front();
        }

        Turn {
            index,
            message: message.to_string(),
            status,
            reply_text,
            case_id,
            case_reset,
            error_body: None,
            reply_kind,
            decision,
            product_models,
            llm_requests,
        }
    }

    /// 現在の `case_id`（最初のターンの前は `None`）。
    pub(crate) fn case_id(&self) -> Option<&str> {
        self.case_id.as_deref()
    }

    /// 次のターンで送る `history`（`(role, text)`、時系列の昇順）。
    pub(crate) fn history(&self) -> Vec<(&'static str, String)> {
        self.history.iter().cloned().collect()
    }
}

/// 一時ディレクトリ（drop で削除）。`tempfile` クレートを足さないための最小実装。
mod tempfile_dir {
    use std::path::{Path, PathBuf};

    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("reply-test-harness-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create a temp dir for the reply test harness");
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
