//! LINE webhook アダプタ（判断ゼロの別バイナリ）。
//!
//! 仕様の正本は `docs/superpowers/specs/2026-08-11-answer-api-line-adapter-design.md`
//! （以下「design doc」）§6。判定・応答文生成はすべて `POST /{project_id}/api/reply`
//! （`cs_support_mcp::api`）側で完結しており、このバイナリは
//!
//! 1. `X-Line-Signature` を検証する
//! 2. テキストメッセージだけを応答生成 API へ 1 回渡す
//! 3. 返ってきた `reply_text` を LINE へ返信する
//! 4. ユーザ単位の会話履歴・case_id をプロセス内メモリに保持する
//!
//! だけを行う。応答生成 API と HTTP JSON でやり取りする独立サービス（別 Cloud Run
//! service, design doc §7）なので、リクエスト/レスポンスの型は `cs_support_mcp::api` の
//! 型を再利用せず、このファイル内に自己完結させる（Rust の型でプロセス境界をまたがせない）。

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type HmacSha256 = Hmac<Sha256>;

/// 会話履歴 1 ターンの発話者。応答生成 API へ送る history エントリの role でもある
/// （design doc §2 の `"customer" | "assistant"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Customer,
    Assistant,
}

/// `SessionStore` の既定 TTL（design doc §6: 60 分）。
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(60 * 60);
/// `SessionStore` の既定エントリ上限（design doc §6: 10,000）。
const DEFAULT_MAX_SESSIONS: usize = 10_000;
/// 保持する会話履歴ターン数の上限（design doc §6: 20 ターンのリング）。
const MAX_HISTORY_TURNS: usize = 20;

/// LINE user 1 人分のセッション。
struct Session {
    case_id: Option<String>,
    /// 新しい側が末尾。上限 [`MAX_HISTORY_TURNS`] を超えたら頭（古い側）から破棄する。
    history: VecDeque<(Role, String)>,
    last_at: Instant,
}

/// LINE user_id ごとの会話状態を保持するプロセス内メモリストア（design doc §6）。
///
/// プロセス再起動で消える（design doc §6: 「その場合は新規 case として継続する」）。
/// TTL・上限は本番では [`SessionStore::new`] の既定値を使うが、テストは
/// [`SessionStore::with_limits`] で小さい値に差し替え、実際に 60 分待たずに
/// 期限切れ・上限超過の挙動を確認する。
struct SessionStore {
    ttl: Duration,
    max_entries: usize,
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    fn new() -> Self {
        Self::with_limits(DEFAULT_SESSION_TTL, DEFAULT_MAX_SESSIONS)
    }

    fn with_limits(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 該当ユーザの `case_id` と会話履歴を返す。
    ///
    /// TTL を超過したエントリはここで破棄し、無いものとして扱う（design doc §6:
    /// 「アクセス時 + 定期スイープ」の「アクセス時」側）。
    fn get(&self, user_id: &str) -> (Option<String>, VecDeque<(Role, String)>) {
        let mut sessions = self.sessions.lock().expect("session store mutex poisoned");
        if let Some(session) = sessions.get(user_id) {
            if session.last_at.elapsed() > self.ttl {
                sessions.remove(user_id);
                return (None, VecDeque::new());
            }
            return (session.case_id.clone(), session.history.clone());
        }
        (None, VecDeque::new())
    }

    /// 顧客発話・応答の 2 ターンを追記し、`case_id` を更新する。
    ///
    /// ユーザが未登録なら新規作成する。新規作成時、既にエントリ数が上限に達していれば
    /// `last_at` が最古のエントリを 1 件破棄してから挿入する（design doc §6:
    /// 「エントリ上限 10,000、超過時は `last_at` 最古を破棄」）。
    fn update(
        &self,
        user_id: &str,
        case_id: String,
        customer_text: String,
        assistant_text: String,
    ) {
        let mut sessions = self.sessions.lock().expect("session store mutex poisoned");
        if !sessions.contains_key(user_id) && sessions.len() >= self.max_entries {
            if let Some(oldest_key) = sessions
                .iter()
                .min_by_key(|(_, session)| session.last_at)
                .map(|(key, _)| key.clone())
            {
                sessions.remove(&oldest_key);
            }
        }
        let session = sessions
            .entry(user_id.to_string())
            .or_insert_with(|| Session {
                case_id: None,
                history: VecDeque::new(),
                last_at: Instant::now(),
            });
        session.case_id = Some(case_id);
        session.history.push_back((Role::Customer, customer_text));
        session.history.push_back((Role::Assistant, assistant_text));
        while session.history.len() > MAX_HISTORY_TURNS {
            session.history.pop_front();
        }
        session.last_at = Instant::now();
    }

    /// TTL を超過した全エントリを破棄する（design doc §6: 「定期スイープ」側。呼び出し元の
    /// `main` がバックグラウンドタスクで一定間隔ごとに呼ぶ）。
    fn sweep(&self) {
        let mut sessions = self.sessions.lock().expect("session store mutex poisoned");
        let ttl = self.ttl;
        sessions.retain(|_, session| session.last_at.elapsed() <= ttl);
    }
}

/// `X-Line-Signature` を channel secret の HMAC-SHA256(base64) で検証する。
///
/// 比較は `hmac::Mac::verify_slice`（定数時間比較。`server/src/oauth/signing.rs` と同じ規律）
/// を使う。`==` で比較すると署名の先頭一致長が応答時間に漏れる。
fn verify_signature(channel_secret: &str, body: &[u8], signature_b64: &str) -> bool {
    use base64::Engine;
    let Ok(signature) = base64::engine::general_purpose::STANDARD.decode(signature_b64) else {
        return false;
    };
    // HMAC は任意長の鍵を受け付けるため `new_from_slice` は失敗しない
    // （`server/src/oauth/signing.rs` の同コメント参照）。
    let mut mac =
        HmacSha256::new_from_slice(channel_secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

/// LINE webhook のリクエストボディ（`events` 配列を持つ）。
#[derive(Debug, Deserialize)]
struct WebhookBody {
    events: Vec<WebhookEvent>,
}

/// webhook イベント 1 件。`message` / `source` は message イベント以外では `None`
/// （例: follow イベントには `message` が無い）。フィールドは permissive にパースする
/// （`timestamp` / `mode` 等、使わない項目は無視する。`deny_unknown_fields` は付けない）。
#[derive(Debug, Deserialize)]
struct WebhookEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(rename = "replyToken")]
    reply_token: Option<String>,
    source: Option<EventSource>,
    message: Option<EventMessage>,
}

#[derive(Debug, Deserialize)]
struct EventSource {
    #[serde(rename = "userId")]
    user_id: Option<String>,
}

/// message イベントの `message` フィールド。`text` はテキストメッセージのときだけ
/// `Some`（画像・スタンプ等では無い）。
#[derive(Debug, Deserialize)]
struct EventMessage {
    #[serde(rename = "type")]
    message_type: String,
    text: Option<String>,
}

// ---- 応答生成 API（`POST /{project_id}/api/reply`）の呼び出し ----
//
// design doc §2 のリクエスト/レスポンス JSON 形状と一致させるが、Rust の型としては
// `cs_support_mcp::api` の型を再利用しない（モジュール doc 参照: 2 つの独立サービスが
// HTTP JSON 越しに合意するだけの契約であり、Rust の型でプロセス境界をまたがせない）。

#[derive(Debug, Serialize)]
struct AnswerApiHistoryEntry {
    role: Role,
    text: String,
}

#[derive(Debug, Serialize)]
struct AnswerApiRequest {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    history: Option<Vec<AnswerApiHistoryEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    case_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnswerApiResponse {
    reply_text: String,
    case_id: String,
}

/// webhook イベント 1 件をどう扱うかの振り分け結果（design doc §6 手順 2）。
///
/// `WebhookEvent`（serde 型）から純粋に導出できる部分だけを切り出した中間表現。
/// HTTP 呼び出しを一切含まないので、フィクスチャ JSON をそのまま使ってテストできる。
#[derive(Debug, PartialEq, Eq)]
enum RoutedEvent {
    /// テキストメッセージ。応答生成 API へ渡す。
    Text {
        reply_token: String,
        user_id: String,
        text: String,
    },
    /// テキスト以外のメッセージ（画像・スタンプ等）。`CS_LINE_NONTEXT_TEXT` を返信する。
    NonText { reply_token: String },
    /// message 以外のイベント（follow 等）、または message イベントだが
    /// `replyToken` / `source.userId` / `message.text` を欠く不正な payload。無視する。
    Ignore,
}

/// `WebhookEvent` を [`RoutedEvent`] に振り分ける純関数（design doc §6 手順 2）。
///
/// `replyToken` / `source.userId` / テキスト本文の欠落は LINE の実仕様では起こらない想定だが、
/// 万一欠けていた場合に `unwrap` で panic させるより、当該イベントだけ無視して他のイベント処理
/// を継続できる方が安全なので `Ignore` に倒す（呼び出し側が warn ログを出す）。
fn route_event(event: &WebhookEvent) -> RoutedEvent {
    if event.event_type != "message" {
        return RoutedEvent::Ignore;
    }
    let Some(reply_token) = &event.reply_token else {
        return RoutedEvent::Ignore;
    };
    let Some(message) = &event.message else {
        return RoutedEvent::Ignore;
    };
    if message.message_type != "text" {
        return RoutedEvent::NonText {
            reply_token: reply_token.clone(),
        };
    }
    let (Some(user_id), Some(text)) = (
        event.source.as_ref().and_then(|s| s.user_id.clone()),
        message.text.clone(),
    ) else {
        return RoutedEvent::Ignore;
    };
    RoutedEvent::Text {
        reply_token: reply_token.clone(),
        user_id,
        text,
    }
}

/// `assemble_reply` が返す、成功時に [`SessionStore::update`] へ渡すべき値。
struct SessionUpdate {
    case_id: String,
    customer_text: String,
    assistant_text: String,
}

/// 応答生成 API の呼び出し結果から、LINE へ返す文面とセッション更新要否を決める純関数
/// （design doc §6 手順 4）。
///
/// - `Some(response)`（200 で受理された）→ `reply_text` をそのまま使い、
///   `history` に customer/assistant の 2 ターンを追記・`case_id` を更新する指示を返す。
/// - `None`（非 200・タイムアウト・パース失敗。呼び出し側が既に warn/error ログ済み）→
///   `fallback_text` を使い、セッションは更新しない（design doc §6:
///   「非 200・タイムアウトなら...履歴と case_id は変更しない」）。
fn assemble_reply(
    api_response: Option<&AnswerApiResponse>,
    customer_text: &str,
    fallback_text: &str,
) -> (String, Option<SessionUpdate>) {
    match api_response {
        Some(resp) => (
            resp.reply_text.clone(),
            Some(SessionUpdate {
                case_id: resp.case_id.clone(),
                customer_text: customer_text.to_string(),
                assistant_text: resp.reply_text.clone(),
            }),
        ),
        None => (fallback_text.to_string(), None),
    }
}

/// LINE Reply API へ送るメッセージ本文の最大文字数（design doc §6: 「4,900 字超は末尾切り詰め」。
/// LINE 自体の上限は 5,000 字だが、安全マージンを取った値）。
const MAX_LINE_REPLY_CHARS: usize = 4_900;

/// 文字境界を壊さずに [`MAX_LINE_REPLY_CHARS`] で切り詰める
/// （`harness::reply::truncate_chars` と同じ規律。バイト数ではなく文字数で数える）。
fn truncate_for_line(text: &str) -> String {
    if text.chars().count() <= MAX_LINE_REPLY_CHARS {
        return text.to_string();
    }
    text.chars().take(MAX_LINE_REPLY_CHARS).collect()
}

/// `line_adapter` のプロセス全体で共有する状態。`Arc` で安価に clone してハンドラへ渡す。
struct AppStateInner {
    channel_secret: String,
    channel_access_token: String,
    answer_api_url: String,
    answer_api_key: String,
    fallback_text: String,
    nontext_text: String,
    http: reqwest::Client,
    sessions: SessionStore,
}

type AppState = Arc<AppStateInner>;

/// `POST /line/webhook`（design doc §6 手順 1・5）。
///
/// 1. 署名検証（`X-Line-Signature` 欠落・不一致は 400）
/// 2. body を `WebhookBody` としてパース（不正 JSON は 400。署名検証を通過した後なので
///    ここで壊れているのは LINE 側の契約違反であり運用者が気付くべき事象。error ログを出す）
/// 3. イベントを順に処理する。1 件の失敗が他のイベント処理を止めないよう、
///    `handle_event` の `Err` はログに残すだけで続行する
/// 4. 全イベント処理後 200
async fn webhook_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(signature) = headers
        .get("x-line-signature")
        .and_then(|v| v.to_str().ok())
    else {
        tracing::warn!("line webhook: missing X-Line-Signature header");
        return StatusCode::BAD_REQUEST;
    };
    if !verify_signature(&state.channel_secret, &body, signature) {
        tracing::warn!("line webhook: signature verification failed");
        return StatusCode::BAD_REQUEST;
    }

    let payload: WebhookBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(
                error = ?err,
                "line webhook: body failed to parse as JSON after signature verification \
                 succeeded (LINE payload contract violation)"
            );
            return StatusCode::BAD_REQUEST;
        }
    };

    for event in &payload.events {
        if let Err(err) = handle_event(&state, event).await {
            tracing::error!(
                error = ?err,
                event_type = %event.event_type,
                "line webhook: failed to handle event; continuing with remaining events"
            );
        }
    }
    StatusCode::OK
}

/// 1 イベント分の処理（design doc §6 手順 2〜4）。
///
/// **セッション更新の順序について（design doc に明記が無いため、ここで判断を固定する）:**
/// 応答生成 API が 200 を返した時点で `case_id` は vegapunk 側に既に永続化されている。
/// そのためセッション更新は「その後の LINE への push が成功したか」に関わらず必ず行う。
/// push だけが失敗した場合にセッション更新も取り止めると、次の顧客メッセージが
/// 古い case_id で送られ、サーバ側で既に進んでいる case と局所的に食い違う
/// （顧客には返信が届いていないので気付けない）。LINE push の失敗そのものは
/// `send_line_reply` の `Err` を通じて呼び出し元（`webhook_handler`）が error ログを出す。
async fn handle_event(state: &AppState, event: &WebhookEvent) -> Result<()> {
    match route_event(event) {
        RoutedEvent::Ignore => Ok(()),
        RoutedEvent::NonText { reply_token } => {
            send_line_reply(state, &reply_token, &state.nontext_text).await
        }
        RoutedEvent::Text {
            reply_token,
            user_id,
            text,
        } => {
            let (case_id, history) = state.sessions.get(&user_id);
            let api_response = call_answer_api(state, &text, case_id, &history).await;
            let (reply_text, update) =
                assemble_reply(api_response.as_ref(), &text, &state.fallback_text);

            if let Some(update) = update {
                state.sessions.update(
                    &user_id,
                    update.case_id,
                    update.customer_text,
                    update.assistant_text,
                );
            }

            send_line_reply(state, &reply_token, &reply_text).await
        }
    }
}

/// 応答生成 API を 1 回コールする（design doc §6 手順 3。timeout は `state.http` に
/// 構築時点で設定済みの 50 秒）。
///
/// 失敗（ネットワークエラー・タイムアウト・非 200・レスポンス parse 失敗）は
/// すべて `None` として呼び出し側へ返す。design doc §4 の決定表と同じく、
/// `/api/reply` を叩けなかった時点で `assemble_reply` がフォールバック文へ倒す。
/// **ここで `Err` にして呼び出し元まで伝播させない**のは、LINE への返信は
/// 「フォールバック文を返す」という具体的なフォールバック動作を持つため、
/// エラーとして扱うより「材料が無かった」という状態として扱う方が呼び出し側の分岐が単純になる。
async fn call_answer_api(
    state: &AppState,
    message: &str,
    case_id: Option<String>,
    history: &VecDeque<(Role, String)>,
) -> Option<AnswerApiResponse> {
    let history_entries: Vec<AnswerApiHistoryEntry> = history
        .iter()
        .map(|(role, text)| AnswerApiHistoryEntry {
            role: *role,
            text: text.clone(),
        })
        .collect();
    let request = AnswerApiRequest {
        message: message.to_string(),
        history: (!history_entries.is_empty()).then_some(history_entries),
        case_id,
    };
    let body = match serde_json::to_vec(&request) {
        Ok(b) => b,
        Err(err) => {
            tracing::error!(error = ?err, "line webhook: failed to serialize answer api request");
            return None;
        }
    };

    let response = match state
        .http
        .post(&state.answer_api_url)
        .header("authorization", format!("Bearer {}", state.answer_api_key))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                error = ?err,
                "line webhook: answer api call failed (network/timeout); falling back to the \
                 fixed reply"
            );
            return None;
        }
    };

    let status = response.status();
    let text = match response.text().await {
        Ok(t) => t,
        Err(err) => {
            tracing::error!(error = ?err, "line webhook: failed to read answer api response body");
            return None;
        }
    };
    if !status.is_success() {
        tracing::warn!(
            %status,
            "line webhook: answer api returned a non-success status; falling back to the fixed \
             reply"
        );
        return None;
    }
    match serde_json::from_str::<AnswerApiResponse>(&text) {
        Ok(resp) => Some(resp),
        Err(err) => {
            tracing::error!(
                error = ?err,
                "line webhook: answer api returned 200 but the body failed to parse as the \
                 expected {{reply_text, case_id}} shape"
            );
            None
        }
    }
}

/// LINE Reply API（`POST https://api.line.me/v2/bot/message/reply`）を 1 回呼ぶ
/// （design doc §6）。失敗時は呼び出し元（`handle_event` 経由 `webhook_handler`）が
/// error ログを出す（design doc §6: 「push 再送は行わない」）。
async fn send_line_reply(state: &AppState, reply_token: &str, text: &str) -> Result<()> {
    let payload = serde_json::json!({
        "replyToken": reply_token,
        "messages": [{"type": "text", "text": truncate_for_line(text)}],
    });
    let body = serde_json::to_vec(&payload).context("serialize line reply api request body")?;

    let response = state
        .http
        .post("https://api.line.me/v2/bot/message/reply")
        .header(
            "authorization",
            format!("Bearer {}", state.channel_access_token),
        )
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .context("call line reply api")?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        anyhow::bail!("line reply api returned {status}: {body_text}");
    }
    Ok(())
}

/// 起動時必須 env を読む。未設定・空白のみは fail closed
/// （`server/src/main.rs` の `require_nonempty_env` と同じ方針: 設定不備を黙って
/// 起動させない。ここでは env 値を直接読むので、あちらのようにテスト用に
/// `Option<String>` を引数分離する必要はない — env アクセス自体を分離するテストは
/// 実プロセスの環境変数を書き換える必要があり並列テスト実行で不安定になるため
/// 追加しない）。
fn require_env(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{name} is required and must not be empty; set it (Secret Manager injection \
                 for secrets) before starting line_adapter"
            )
        })
}

const DEFAULT_FALLBACK_TEXT: &str =
    "申し訳ありません。ただいま応答できません。時間をおいてもう一度お試しください。";
const DEFAULT_NONTEXT_TEXT: &str = "恐れ入りますが、テキストでお送りください。";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // 必須 4 つ（design doc §6）。欠落時は起動失敗させる（誤設定のまま「動いているように
    // 見えて誰にも返信できない」状態で本番運用に入るのを防ぐ）。
    let channel_secret = require_env("LINE_CHANNEL_SECRET")?;
    let channel_access_token = require_env("LINE_CHANNEL_ACCESS_TOKEN")?;
    let answer_api_url = require_env("CS_ANSWER_API_URL")?;
    let answer_api_key = require_env("CS_ANSWER_API_KEY")?;
    let fallback_text =
        env::var("CS_LINE_FALLBACK_TEXT").unwrap_or_else(|_| DEFAULT_FALLBACK_TEXT.to_string());
    let nontext_text =
        env::var("CS_LINE_NONTEXT_TEXT").unwrap_or_else(|_| DEFAULT_NONTEXT_TEXT.to_string());

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(50))
        .build()
        .context("build reqwest client for line_adapter")?;

    let state: AppState = Arc::new(AppStateInner {
        channel_secret,
        channel_access_token,
        answer_api_url,
        answer_api_key,
        fallback_text,
        nontext_text,
        http,
        sessions: SessionStore::new(),
    });

    // TTL 経過エントリの定期スイープ（design doc §6: 「アクセス時 + 定期スイープ」の
    // 「定期スイープ」側。5 分間隔は TTL 60 分に対して十分細かい）。
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                state.sessions.sweep();
            }
        });
    }

    // `.with_state` must run before `.merge`: it bakes `AppState` into the router's
    // handlers and turns `Router<AppState>` into a state-erased `Router` that can be
    // merged with the stateless `health_router()` (same pattern as main.rs merging
    // `api::api_router(api_state)` into the top-level stateless `app`).
    let app = Router::new()
        .route("/line/webhook", post(webhook_handler))
        .with_state(state)
        .merge(cs_support_mcp::health::health_router());

    // Cloud Run はコンテナに `PORT`/`BIND_ADDR` を渡す運用（Dockerfile の
    // `ENV BIND_ADDR=0.0.0.0:8080` を参照）。未設定時のローカル既定も同じ値にする。
    let bind_addr: SocketAddr = env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()
        .context("parse BIND_ADDR")?;
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind {bind_addr}"))?;
    tracing::info!(%bind_addr, "starting line_adapter");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        use base64::Engine;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    #[test]
    fn verify_signature_accepts_correctly_signed_body() {
        let secret = "test-channel-secret";
        let body = br#"{"events":[]}"#;
        let sig = sign(secret, body);
        assert!(verify_signature(secret, body, &sig));
    }

    #[test]
    fn verify_signature_rejects_tampered_body() {
        let secret = "test-channel-secret";
        let body = br#"{"events":[]}"#;
        let sig = sign(secret, body);
        let tampered: &[u8] = br#"{"events":[{"type":"follow"}]}"#;
        assert!(!verify_signature(secret, tampered, &sig));
    }

    #[test]
    fn verify_signature_rejects_invalid_base64() {
        assert!(!verify_signature(
            "test-channel-secret",
            br#"{"events":[]}"#,
            "not-valid-base64!!"
        ));
    }

    // ---- SessionStore（design doc §6） ----

    #[test]
    fn session_store_expires_entries_after_ttl() {
        let store = SessionStore::with_limits(Duration::from_millis(20), 10);
        store.update("u1", "case-1".into(), "q".into(), "a".into());
        std::thread::sleep(Duration::from_millis(60));
        let (case_id, history) = store.get("u1");
        assert!(
            case_id.is_none(),
            "an entry past its TTL must be treated as absent"
        );
        assert!(history.is_empty());
    }

    #[test]
    fn session_store_evicts_the_oldest_entry_when_over_capacity() {
        let store = SessionStore::with_limits(Duration::from_secs(3600), 2);
        store.update("u1", "case-1".into(), "q1".into(), "a1".into());
        std::thread::sleep(Duration::from_millis(5));
        store.update("u2", "case-2".into(), "q2".into(), "a2".into());
        std::thread::sleep(Duration::from_millis(5));
        // store is now full (max_entries = 2); inserting a 3rd user must evict the
        // least-recently-touched one (u1).
        store.update("u3", "case-3".into(), "q3".into(), "a3".into());

        assert!(
            store.get("u1").0.is_none(),
            "the oldest entry must be evicted when the store is at capacity"
        );
        assert!(store.get("u2").0.is_some());
        assert!(store.get("u3").0.is_some());
    }

    #[test]
    fn session_store_caps_history_at_20_turns_dropping_the_oldest() {
        let store = SessionStore::with_limits(Duration::from_secs(3600), 10);
        // 15 updates * 2 turns (customer + assistant) = 30 turns pushed; only the
        // newest 20 must survive.
        for i in 0..15 {
            store.update("u1", "case-1".into(), format!("q{i}"), format!("a{i}"));
        }
        let (_, history) = store.get("u1");
        assert_eq!(history.len(), MAX_HISTORY_TURNS);
        assert_eq!(
            history.front().unwrap(),
            &(Role::Customer, "q5".to_string()),
            "the oldest 10 turns (q0..a4) must have been dropped from the head"
        );
        assert_eq!(
            history.back().unwrap(),
            &(Role::Assistant, "a14".to_string())
        );
    }

    // ---- webhook イベント parse（design doc §6） ----

    /// LINE の実際の webhook payload に準じたフィクスチャ（`docs.line.biz` の例に
    /// 準拠。未使用フィールド `timestamp` / `mode` / `quoteToken` 等も混ぜて、
    /// permissive にパースできることを確認する）。
    const TEXT_MESSAGE_EVENT: &str = r#"{
        "events": [
            {
                "type": "message",
                "message": {
                    "type": "text",
                    "id": "444573844083572737",
                    "quoteToken": "q3Plxr4AgKd...",
                    "text": "Hello, world"
                },
                "timestamp": 1462629479859,
                "source": { "type": "user", "userId": "U4af4980629..." },
                "replyToken": "0f3779fba3b349968c5d07db31eabf65",
                "mode": "active"
            }
        ]
    }"#;

    const IMAGE_MESSAGE_EVENT: &str = r#"{
        "events": [
            {
                "type": "message",
                "message": {
                    "type": "image",
                    "id": "444573844083572738",
                    "contentProvider": { "type": "line" }
                },
                "timestamp": 1462629479859,
                "source": { "type": "user", "userId": "U4af4980629..." },
                "replyToken": "0f3779fba3b349968c5d07db31eabf66",
                "mode": "active"
            }
        ]
    }"#;

    const FOLLOW_EVENT: &str = r#"{
        "events": [
            {
                "type": "follow",
                "timestamp": 1462629479859,
                "source": { "type": "user", "userId": "U4af4980629..." },
                "replyToken": "0f3779fba3b349968c5d07db31eabf67",
                "mode": "active"
            }
        ]
    }"#;

    #[test]
    fn webhook_body_parses_a_text_message_event() {
        let body: WebhookBody = serde_json::from_str(TEXT_MESSAGE_EVENT).unwrap();
        assert_eq!(body.events.len(), 1);
        let event = &body.events[0];
        assert_eq!(event.event_type, "message");
        assert_eq!(
            event.reply_token.as_deref(),
            Some("0f3779fba3b349968c5d07db31eabf65")
        );
        assert_eq!(
            event.source.as_ref().and_then(|s| s.user_id.as_deref()),
            Some("U4af4980629...")
        );
        let message = event.message.as_ref().expect("text event has a message");
        assert_eq!(message.message_type, "text");
        assert_eq!(message.text.as_deref(), Some("Hello, world"));
    }

    #[test]
    fn webhook_body_parses_a_non_text_message_event_with_no_text_field() {
        let body: WebhookBody = serde_json::from_str(IMAGE_MESSAGE_EVENT).unwrap();
        let message = body.events[0]
            .message
            .as_ref()
            .expect("image event has a message");
        assert_eq!(message.message_type, "image");
        assert!(
            message.text.is_none(),
            "non-text messages must not carry a text field"
        );
    }

    #[test]
    fn webhook_body_parses_a_follow_event_with_no_message_field() {
        let body: WebhookBody = serde_json::from_str(FOLLOW_EVENT).unwrap();
        let event = &body.events[0];
        assert_eq!(event.event_type, "follow");
        assert!(
            event.message.is_none(),
            "non-message events must not carry a message field"
        );
        assert!(event.source.is_some());
    }

    // ---- route_event（design doc §6 手順 2: イベント振り分け） ----

    #[test]
    fn route_event_classifies_a_text_message() {
        let body: WebhookBody = serde_json::from_str(TEXT_MESSAGE_EVENT).unwrap();
        let routed = route_event(&body.events[0]);
        assert_eq!(
            routed,
            RoutedEvent::Text {
                reply_token: "0f3779fba3b349968c5d07db31eabf65".to_string(),
                user_id: "U4af4980629...".to_string(),
                text: "Hello, world".to_string(),
            }
        );
    }

    #[test]
    fn route_event_classifies_a_non_text_message() {
        let body: WebhookBody = serde_json::from_str(IMAGE_MESSAGE_EVENT).unwrap();
        let routed = route_event(&body.events[0]);
        assert_eq!(
            routed,
            RoutedEvent::NonText {
                reply_token: "0f3779fba3b349968c5d07db31eabf66".to_string(),
            }
        );
    }

    #[test]
    fn route_event_ignores_non_message_events() {
        let body: WebhookBody = serde_json::from_str(FOLLOW_EVENT).unwrap();
        assert_eq!(route_event(&body.events[0]), RoutedEvent::Ignore);
    }

    #[test]
    fn route_event_ignores_a_message_event_missing_reply_token() {
        let event = WebhookEvent {
            event_type: "message".to_string(),
            reply_token: None,
            source: Some(EventSource {
                user_id: Some("U1".to_string()),
            }),
            message: Some(EventMessage {
                message_type: "text".to_string(),
                text: Some("hi".to_string()),
            }),
        };
        assert_eq!(route_event(&event), RoutedEvent::Ignore);
    }

    // ---- assemble_reply（design doc §6 手順 4: API 応答からの文面決定） ----

    #[test]
    fn assemble_reply_uses_the_api_reply_text_and_requests_a_session_update_on_success() {
        let resp = AnswerApiResponse {
            reply_text: "こちらが回答です".to_string(),
            case_id: "case-99".to_string(),
        };
        let (text, update) = assemble_reply(Some(&resp), "質問です", "フォールバック文");
        assert_eq!(text, "こちらが回答です");
        let update = update.expect("a successful api call must request a session update");
        assert_eq!(update.case_id, "case-99");
        assert_eq!(update.customer_text, "質問です");
        assert_eq!(update.assistant_text, "こちらが回答です");
    }

    #[test]
    fn assemble_reply_falls_back_and_leaves_the_session_untouched_on_failure() {
        let (text, update) = assemble_reply(None, "質問です", "フォールバック文");
        assert_eq!(text, "フォールバック文");
        assert!(
            update.is_none(),
            "a failed (or non-200/timeout) api call must not request a session update"
        );
    }

    // ---- truncate_for_line ----

    #[test]
    fn truncate_for_line_leaves_short_text_untouched() {
        assert_eq!(truncate_for_line("短い返信"), "短い返信");
    }

    #[test]
    fn truncate_for_line_truncates_long_text_on_a_char_boundary() {
        let long = "あ".repeat(MAX_LINE_REPLY_CHARS + 100);
        let truncated = truncate_for_line(&long);
        assert_eq!(truncated.chars().count(), MAX_LINE_REPLY_CHARS);
    }
}
