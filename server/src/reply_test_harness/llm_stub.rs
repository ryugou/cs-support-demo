use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LlmKind {
    Classify,
    ProductReferences,
    KrApplicability,
    Clarify,
    ProductIdentification,
    EscalationAck,
    TimePref,
    Reply,
    NameConfirmation,
    NameConfirmationReply,
    Other,
}

impl LlmKind {
    fn detect(system: &str) -> Self {
        let markers: [(&str, LlmKind); 9] = [
            (
                "直前の返信で、顧客が書いた名称",
                LlmKind::NameConfirmationReply,
            ),
            (
                "登録名称のどれのことかを確認する返信を書きます",
                LlmKind::NameConfirmation,
            ),
            ("CS 問い合わせの分類器", LlmKind::Classify),
            ("製品への言及を抽出する担当", LlmKind::ProductReferences),
            (
                "承認済みの回答（候補）がそのまま使えるかを判定する分類器",
                LlmKind::KrApplicability,
            ),
            ("発話分類器です。顧客の発話が『希望連絡", LlmKind::TimePref),
            ("受け止め文だけを 1〜2 文で書きます", LlmKind::EscalationAck),
            (
                "顧客の相談が、当社のどの取扱製品についてのものかをまだ特定できていません",
                LlmKind::ProductIdentification,
            ),
            ("状況を把握するための確認の返信", LlmKind::Clarify),
        ];
        for (marker, kind) in markers {
            if system.contains(marker) {
                return kind;
            }
        }
        if system.contains("返信文の下書きを 1 つだけ書きます") {
            return LlmKind::Reply;
        }
        LlmKind::Other
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LlmRequest {
    pub kind: LlmKind,
    pub system: String,
    pub user: String,
    pub max_tokens: u64,
}

pub(crate) type Responder = Arc<dyn Fn(&LlmRequest) -> String + Send + Sync>;

#[derive(Default)]
struct StubState {
    requests: Vec<LlmRequest>,
    responders: std::collections::HashMap<LlmKind, Responder>,
}

pub(crate) struct LlmStub {
    pub endpoint: String,
    state: Arc<Mutex<StubState>>,
    task: tokio::task::JoinHandle<()>,
}

impl LlmStub {
    pub(crate) async fn start() -> Self {
        let state = Arc::new(Mutex::new(StubState::default()));
        let app = axum::Router::new()
            .route("/v1/messages", axum::routing::post(handle))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port for the LLM stub");
        let addr = listener.local_addr().expect("read the LLM stub addr");
        let task = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                eprintln!("LLM stub stopped: {err}");
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/messages"),
            state,
            task,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StubState> {
        self.state.lock().expect("LLM stub state poisoned")
    }

    pub(crate) fn requests(&self) -> Vec<LlmRequest> {
        self.lock().requests.clone()
    }

    pub(crate) fn requests_of(&self, kind: LlmKind) -> Vec<LlmRequest> {
        self.lock()
            .requests
            .iter()
            .filter(|r| r.kind == kind)
            .cloned()
            .collect()
    }

    pub(crate) fn request_count(&self) -> usize {
        self.lock().requests.len()
    }

    pub(crate) fn respond_with(
        &self,
        kind: LlmKind,
        responder: impl Fn(&LlmRequest) -> String + Send + Sync + 'static,
    ) {
        self.lock().responders.insert(kind, Arc::new(responder));
    }

    pub(crate) fn respond_text(&self, kind: LlmKind, text: &str) {
        let text = text.to_string();
        self.respond_with(kind, move |_| text.clone());
    }

    pub(crate) fn respond_classify(&self, signals: &[&str], product_dependent: bool) {
        let body = serde_json::json!({
            "signals": signals,
            "product_dependent": product_dependent,
        })
        .to_string();
        self.respond_text(LlmKind::Classify, &body);
    }
}

impl Drop for LlmStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(
    axum::extract::State(state): axum::extract::State<Arc<Mutex<StubState>>>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let system = body["system"].as_str().unwrap_or_default().to_string();
    let user = body["messages"]
        .as_array()
        .and_then(|messages| messages.first())
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    let request = LlmRequest {
        kind: LlmKind::detect(&system),
        system,
        user,
        max_tokens: body["max_tokens"].as_u64().unwrap_or(0),
    };
    let responder = {
        let mut guard = state.lock().expect("LLM stub state poisoned");
        guard.requests.push(request.clone());
        guard.responders.get(&request.kind).cloned()
    };
    let text = match responder {
        Some(responder) => responder(&request),
        None => default_responder(&request),
    };
    axum::Json(serde_json::json!({
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": text}],
    }))
}

pub(crate) fn default_responder(request: &LlmRequest) -> String {
    match request.kind {
        LlmKind::Classify => {
            serde_json::json!({"signals": [], "product_dependent": true}).to_string()
        }
        LlmKind::ProductReferences => {
            serde_json::json!({"product_references": matched_references(request)}).to_string()
        }
        LlmKind::KrApplicability => serde_json::json!({"applicable_ids": []}).to_string(),
        LlmKind::TimePref => serde_json::json!({
            "is_time_preference": false,
            "windows": [],
            "raw": "",
        })
        .to_string(),
        LlmKind::Clarify => "（スタブ）確認の返信です。".to_string(),
        LlmKind::ProductIdentification => "（スタブ）製品を確認する返信です。".to_string(),
        LlmKind::EscalationAck => "（スタブ）受け止め文です。".to_string(),
        LlmKind::Reply => "（スタブ）返信文の下書きです。".to_string(),
        LlmKind::NameConfirmation => confirmation_echo(request),
        LlmKind::NameConfirmationReply => {
            serde_json::json!({"reply_type": "unrelated", "answers": []}).to_string()
        }
        LlmKind::Other => "（スタブ）".to_string(),
    }
}

fn confirmation_echo(request: &LlmRequest) -> String {
    let items: Vec<String> = request
        .user
        .lines()
        .filter_map(|line| line.split_once("顧客の表記: "))
        .filter_map(|(_, rest)| rest.split_once("／候補: "))
        .map(|(surface, candidates)| {
            let candidates = candidates.split('（').next().unwrap_or(candidates);
            format!("{surface} は {candidates} のことでしょうか？")
        })
        .collect();
    format!("（スタブ）{}", items.join(""))
}

fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect()
}

fn matched_references(request: &LlmRequest) -> Vec<serde_json::Value> {
    let Some(catalog) = request
        .system
        .lines()
        .find_map(|line| line.strip_prefix("当社の取扱製品一覧: "))
    else {
        return Vec::new();
    };
    let question = normalize(&request.user);
    catalog
        .split(['、', ','])
        .filter_map(|entry| {
            let model = entry.split(['（', '(']).next().unwrap_or(entry).trim();
            (!model.is_empty() && question.contains(&normalize(model))).then(|| {
                serde_json::json!({
                    "surface": model,
                    "resolution": "matched",
                    "matched_model": model,
                })
            })
        })
        .collect()
}
