//! Anthropic Messages API のテスト用代替（スタブ LLM）。
//!
//! 本番の `AnthropicClient` は `[llm] endpoint` へ HTTP で問い合わせる。そのエンドポイントを
//! この stub（`127.0.0.1` の空きポート）へ向けることで、signal 抽出・製品参照抽出・返信文の
//! 下書き・聞き返し・受け止め文・希望時間帯・KR 適用判定の LLM 呼び出しをすべて本番と同じ
//! 経路で差し替える。外部ネットワークには接続しない。
//!
//! - 受け取ったリクエストは [`LlmRequest`] として記録し、テストから読める。
//! - 応答は呼び出しの種類（[`LlmKind`]、system prompt から判別）ごとの応答関数で決める。
//!   既定の応答は [`default_responder`] を参照。種類ごとに差し替えられる。

use std::sync::{Arc, Mutex};

/// LLM 呼び出しの種類。system prompt の冒頭の文言で判別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LlmKind {
    /// signal 抽出（`{"signals": [...], "product_dependent": bool}` を返す）。
    Classify,
    /// 製品参照抽出（`{"product_references": [...]}` を返す）。
    ProductReferences,
    /// 既知の解決策の適用判定（`{"applicable_ids": [...]}` を返す）。
    KrApplicability,
    /// 聞き返し（確認質問）の下書き。
    Clarify,
    /// 製品が特定できないときの、型番と症状の聞き返しの下書き（`<取扱製品一覧>` を user に持つ）。
    ProductIdentification,
    /// 取次の受け止め文の下書き。
    EscalationAck,
    /// 希望時間帯の抽出（JSON を返す）。
    TimePref,
    /// 顧客向け返信文の下書き。
    Reply,
    /// 名称（型番・製品名・社名）の候補を確認する返信の下書き（Issue #153）。
    NameConfirmation,
    /// 名称の確認への顧客の返答の分類（Issue #153。JSON を返す）。
    NameConfirmationReply,
    /// 上のどれにも当てはまらない呼び出し。
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

/// stub が受け取った 1 回の呼び出し。
#[derive(Debug, Clone)]
pub(crate) struct LlmRequest {
    pub kind: LlmKind,
    /// リクエスト本文の `system`。
    pub system: String,
    /// `messages` の最初の user メッセージ（本文）。
    pub user: String,
    pub max_tokens: u64,
}

/// 応答関数。受け取ったリクエストから、モデルが返すテキスト（`content[0].text`）を決める。
pub(crate) type Responder = Arc<dyn Fn(&LlmRequest) -> String + Send + Sync>;

#[derive(Default)]
struct StubState {
    requests: Vec<LlmRequest>,
    responders: std::collections::HashMap<LlmKind, Responder>,
}

/// 起動中のスタブ LLM。drop すると止める。
pub(crate) struct LlmStub {
    /// `[llm] endpoint` に設定する URL。
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

    /// これまでに受け取った全リクエスト（受け取った順）。
    pub(crate) fn requests(&self) -> Vec<LlmRequest> {
        self.lock().requests.clone()
    }

    /// 種類で絞ったリクエスト。
    pub(crate) fn requests_of(&self, kind: LlmKind) -> Vec<LlmRequest> {
        self.lock()
            .requests
            .iter()
            .filter(|r| r.kind == kind)
            .cloned()
            .collect()
    }

    /// 記録済みリクエストの件数。ターンごとの差分を取るための目印にする。
    pub(crate) fn request_count(&self) -> usize {
        self.lock().requests.len()
    }

    /// 種類ごとの応答関数を差し替える。
    pub(crate) fn respond_with(
        &self,
        kind: LlmKind,
        responder: impl Fn(&LlmRequest) -> String + Send + Sync + 'static,
    ) {
        self.lock().responders.insert(kind, Arc::new(responder));
    }

    /// 種類に固定のテキストを返させる。
    pub(crate) fn respond_text(&self, kind: LlmKind, text: &str) {
        let text = text.to_string();
        self.respond_with(kind, move |_| text.clone());
    }

    /// signal 抽出に固定の結果を返させる。
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
    // 応答関数はロックの外で呼ぶ（関数の中でスタブの状態を読んでもデッドロックしない）。
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

/// 種類ごとの既定の応答。
///
/// - signal 抽出: signal なし、`product_dependent = true`。
/// - 製品参照抽出: system prompt の取扱製品一覧のうち、顧客の発話に型番（大小文字・ハイフン
///   の違いを除いて）がそのまま含まれるものを `matched` で返す。含まれなければ空配列。
///   タイポした型番は一致しないため、製品未特定になる。
/// - KR 適用判定: 該当なし。
/// - 希望時間帯: 時間帯の指定ではない。
/// - 下書き系: 種類を示す固定文。これが `reply_text` に現れても、文面の合格の根拠にしない。
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

/// 名称の確認の下書き要求（user の `<確認する名称>` の各行「n. 顧客の表記: X／候補: Y、Z（注記）」）
/// から、顧客の表記と候補をそのまま並べた確認文を作る。本番の検査（表記と候補が一字一句含まれる
/// こと）を通る下書きにするため。
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

/// 取扱製品一覧（system prompt の「当社の取扱製品一覧: …」の 1 行）のうち、発話に含まれる
/// 型番を `matched` の製品参照にする。
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
            // 一覧の表記は「型番」または「型番（製品名）」。型番だけを取り出す。
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
