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
use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};
use tower_http::trace::TraceLayer;

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

/// 会話履歴 1 ターンの本文を**保存する際**の上限文字数。
///
/// `server/src/api.rs` の `MAX_HISTORY_TEXT_CHARS`（design doc §2）と一致させる。
/// ここを超える履歴を `/api/reply` へ送ると、その API は 400 `invalid_request` を返す。
/// `call_answer_api` はそれを `None` として扱い `assemble_reply` がフォールバック文へ倒し、
/// かつセッションは更新しない（design doc §6 手順4）。つまり一度 2,000 字超の
/// customer_text / assistant_text が history に混入すると、以後そのユーザの**全**
/// メッセージが TTL（60 分）経過までフォールバック文しか返せなくなり、`last_at` も
/// 成功時にしか進まないため自己回復しない（F1）。`message` 自体は 5,000 字まで
/// 許されるため、この事故は「2,001〜5,000 字の問い合わせを 1 通送る」だけで発生する。
/// これを防ぐため、応答生成 API へ送る直前ではなく**保存時**に切り詰める。
///
/// 切り詰めても意味的な損失が実質無い根拠: サーバ側は生成プロンプトへ注入する際に
/// 新しい側から最大 6 ターン・合計 4,000 字しか採用しない
/// （`server/src/harness/reply.rs` の `select_history`、`MAX_HISTORY_TURNS = 6` /
/// `MAX_HISTORY_CHARS = 4000`）。1 ターンで 2,000 字を超える発話は、その予算の半分以上を
/// 単独で使い切る想定外の長さであり、末尾を切り詰めても生成プロンプトに載る実効情報量への
/// 影響は小さい。
const MAX_HISTORY_TEXT_CHARS: usize = 2_000;

/// `case_id` を**保存する際**の上限文字数。`server/src/api.rs` の `MAX_CASE_ID_CHARS`
/// （design doc §2）と一致させる。これを超える case_id を保存すると、次回以降の
/// リクエストが必ず 400 になり、history と同じ理由でセッションが自己回復不能になる。
/// **切り詰めは行わない**（case_id は不透明な識別子であり、切り詰めると別の case を指す
/// 壊れた id になるため）。超過時は保存せず、`session.case_id` を**直前の値のまま**にする。
/// この後の挙動は直前の値の有無で分かれる:
///
/// - 直前に有効な case_id があった場合: 次回リクエストもその case_id が送られ、**同じ case
///   へ継続する**（新規 case にはならない）。case の累積 signal を失わせて新規 case にする
///   より、既知の有効な case へ継続する方が安全側の判断。
/// - 直前が `None` だった場合（新規セッションの初回ターン、TTL 失効直後、エントリ上限
///   超過による eviction 直後）: `session.case_id` は `None` のままなので、次回リクエストは
///   case_id 無しで送られ、`/api/reply` 側で**新規 case になる**。128 字超が構造的に
///   起きる場合（応答生成 API が返す case_id の形式自体が契約を超えている場合）、これは
///   全ユーザで初回ターンから発生するため、この分岐こそが常態になりうる。
const MAX_CASE_ID_CHARS: usize = 128;

/// LINE user 1 人分のセッション。
struct Session {
    case_id: Option<String>,
    /// 新しい側が末尾。上限 [`MAX_HISTORY_TURNS`] を超えたら頭（古い側）から破棄する。
    history: VecDeque<(Role, String)>,
    last_at: Instant,
}

impl Session {
    /// 新規ユーザー・TTL 失効・エントリ eviction 後のいずれでも同じ「空のセッション」を作る。
    fn fresh() -> Self {
        Self {
            case_id: None,
            history: VecDeque::new(),
            last_at: Instant::now(),
        }
    }
}

/// LINE user_id ごとの会話状態を保持するプロセス内メモリストア（design doc §6）。
///
/// プロセス再起動で消える（design doc §6: 「その場合は新規 case として継続する」）。
/// TTL・上限は本番では [`SessionStore::new`] の既定値を使うが、テストは
/// [`SessionStore::with_limits`] で小さい値に差し替え、実際に 60 分待たずに
/// 期限切れ・上限超過の挙動を確認する。
///
/// **同一ユーザーの get→呼び出し→update を直列化する（Stage 2 レビュー指摘）**: 外側は
/// `std::sync::Mutex<HashMap<String, Arc<TokioMutex<Session>>>>` でユーザーごとに独立した
/// 非同期ロックを管理する。外側ロックは該当ユーザーの `Arc` を取得/挿入する一瞬だけ保持し、
/// 実際のセッション読み取りから更新まで（`handle_event` が応答生成 API 呼び出し・LINE
/// 返信を含む一連の処理全体）は、そのユーザー専用の `TokioMutex` を保持したまま直列に実行
/// する（[`SessionStore::lock_session`]）。同一ユーザーの次のイベントは、前のイベントが
/// このロックを解放するまで get すら開始できない。別ユーザー同士は別の `Arc` を得るため
/// 互いにブロックしない。
struct SessionStore {
    ttl: Duration,
    max_entries: usize,
    users: Mutex<HashMap<String, Arc<TokioMutex<Session>>>>,
}

impl SessionStore {
    fn new() -> Self {
        Self::with_limits(DEFAULT_SESSION_TTL, DEFAULT_MAX_SESSIONS)
    }

    fn with_limits(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            users: Mutex::new(HashMap::new()),
        }
    }

    /// 該当ユーザーの `Arc<TokioMutex<Session>>` を返す（無ければ新規作成する）。
    ///
    /// 新規作成時、既にエントリ数が上限に達していれば `last_at` が最古のエントリを 1 件
    /// 破棄してから挿入する（design doc §6: 「エントリ上限 10,000、超過時は `last_at`
    /// 最古を破棄」）。LRU 選定は [`Self::pick_lru_key`] を参照。
    fn get_or_create(&self, user_id: &str) -> Arc<TokioMutex<Session>> {
        // ロック保持中のコードは短く panic 要因も乏しいが、万一 panic してもミューテックス
        // を毒で恒久停止させない（F7）。セッション状態は失われても design doc §6 の
        // 想定範囲内（「プロセス再起動で消える。その場合は新規 case として継続する」）
        // なので、毒された内容をそのまま引き継いで復旧する方が「以後 500 が返り続ける」
        // より安全。以下の他の `.lock()` 呼び出しも同じ方針。
        let mut users = self
            .users
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = users.get(user_id) {
            return existing.clone();
        }
        if users.len() >= self.max_entries {
            if let Some(oldest_key) = Self::pick_lru_key(&users) {
                users.remove(&oldest_key);
            }
        }
        let session = Arc::new(TokioMutex::new(Session::fresh()));
        users.insert(user_id.to_string(), session.clone());
        session
    }

    /// エントリ上限超過時に破棄する対象を選ぶ。`last_at` は各セッションの内側
    /// （`TokioMutex`）にあるため、`try_lock`（非同期ランタイムを介さない同期呼び出し）で
    /// 読む。現在処理中（他のイベントがロックを保持中）のユーザーは `try_lock` が失敗する
    /// ので対象から除外する（進行中の会話を破棄しないための安全側の判断。次回のエントリ
    /// 追加時に再評価される。全ユーザーが処理中で候補が無い場合は `None` を返し、呼び出し元
    /// は eviction をスキップする＝一時的に上限を超えることを許容する）。
    fn pick_lru_key(users: &HashMap<String, Arc<TokioMutex<Session>>>) -> Option<String> {
        users
            .iter()
            .filter_map(|(key, session)| {
                session
                    .try_lock()
                    .ok()
                    .map(|guard| (key.clone(), guard.last_at))
            })
            .min_by_key(|(_, last_at)| *last_at)
            .map(|(key, _)| key)
    }

    /// 該当ユーザーの一連の処理（get→応答生成 API 呼び出し→LINE 返信→(成功時のみ)update）
    /// を直列化するロックを取得する。返り値の `OwnedMutexGuard` を保持し続けている間、
    /// 同一ユーザーの他の呼び出しはこの `await` で待たされる。
    ///
    /// TTL を超過していた場合はここでセッションを空にリセットする（design doc §6:
    /// 「アクセス時 + 定期スイープ」の「アクセス時」側。以前は該当エントリをマップから
    /// 削除していたが、ロックを保持したまま新規同然の状態を返せば呼び出し側から見た挙動は
    /// 同じであり、マップからの実削除は定期スイープ（[`Self::sweep`]）に任せる）。
    async fn lock_session(&self, user_id: &str) -> OwnedMutexGuard<Session> {
        let arc = self.get_or_create(user_id);
        let mut guard = arc.lock_owned().await;
        if guard.last_at.elapsed() > self.ttl {
            *guard = Session::fresh();
        }
        guard
    }

    /// TTL を超過した全エントリを破棄する（design doc §6: 「定期スイープ」側。呼び出し元の
    /// `main` がバックグラウンドタスクで一定間隔ごとに呼ぶ）。現在処理中のユーザーは
    /// `try_lock` が失敗するため対象から除外する（[`Self::pick_lru_key`] と同じ判断:
    /// 進行中の会話は消さない。次回のスイープで再評価される）。
    fn sweep(&self) {
        let mut users = self
            .users
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ttl = self.ttl;
        users.retain(|_, session| match session.try_lock() {
            Ok(guard) => guard.last_at.elapsed() <= ttl,
            Err(_) => true,
        });
    }

    /// テスト専用: 該当ユーザーの現在の `case_id` と会話履歴のスナップショットを返す。
    /// 存在しない、または TTL 超過なら「無いもの」として扱う。本番コードは
    /// [`Self::lock_session`] 経由で読み書きを直列化するため、この読み取り専用の
    /// スナップショットは使わない。
    #[cfg(test)]
    async fn snapshot(&self, user_id: &str) -> (Option<String>, VecDeque<(Role, String)>) {
        let arc = {
            let users = self
                .users
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            users.get(user_id).cloned()
        };
        let Some(arc) = arc else {
            return (None, VecDeque::new());
        };
        let guard = arc.lock().await;
        if guard.last_at.elapsed() > self.ttl {
            (None, VecDeque::new())
        } else {
            (guard.case_id.clone(), guard.history.clone())
        }
    }
}

/// [`SessionStore::lock_session`] で得たロック済みセッションへ、customer/assistant の
/// 2 ターンと `case_id` を書き込む（design doc §6 手順4: 応答生成 API が 200 を返し、かつ
/// LINE への返信が成功した場合だけ呼ばれる。呼び出し元は `handle_event` を参照）。
///
/// **保存前に `/api/reply` の入力契約（design doc §2、`server/src/api.rs`）へ正規化する
/// （F1）**: history テキストは [`MAX_HISTORY_TEXT_CHARS`] へ切り詰め、trim 後に空になる
/// テキストは保存しない。`case_id` は [`MAX_CASE_ID_CHARS`] を超えたら保存しない
/// （切り詰めではなく非保存。理由は同定数のコメント）。これらを怠ると、次回リクエストが
/// 必ず 400 になり、セッションが TTL 経過まで自己回復しない。
fn apply_session_update(user_id: &str, session: &mut Session, update: SessionUpdate) {
    let case_id_chars = update.case_id.chars().count();
    if case_id_chars > MAX_CASE_ID_CHARS {
        // `session.case_id` への代入より前に評価すること: これから discard する
        // case_id ではなく、直前まで保持していた値の有無を報告するフィールドなので、
        // 代入後に読むと常に「代入されなかった」ことしか分からず意味が無い。
        let kept_previous_case_id = session.case_id.is_some();
        tracing::warn!(
            user_id,
            case_id_chars,
            max_chars = MAX_CASE_ID_CHARS,
            kept_previous_case_id,
            "line webhook: case_id returned by the answer api exceeds the /api/reply \
             contract; discarding it without truncation. if a previous case_id was held \
             (kept_previous_case_id=true), the next message from this user resumes that \
             existing case. otherwise (kept_previous_case_id=false: new session, post-TTL, \
             or post-eviction) session.case_id stays None and the next message starts a \
             new case"
        );
    } else {
        session.case_id = Some(update.case_id);
    }

    push_history_entry(session, user_id, Role::Customer, update.customer_text);
    push_history_entry(session, user_id, Role::Assistant, update.assistant_text);
    while session.history.len() > MAX_HISTORY_TURNS {
        session.history.pop_front();
    }
    session.last_at = Instant::now();
}

/// [`apply_session_update`] が保存直前に呼ぶ、history 1 エントリ分の正規化（F1）。
///
/// 1. [`MAX_HISTORY_TEXT_CHARS`] へ切り詰める（切り詰め発生時は `tracing::warn!`。
///    本文そのものはログに出さない — 顧客の問い合わせ内容のため）。
/// 2. trim 後に空になった場合は保存しない（`/api/reply` は `history[].text` を trim 後
///    1 字以上必須にしている。空文字列を積むと次回リクエストが必ず 400 になる。通常の
///    経路では起きないが、応答生成 API が `reply_text: ""` を返した場合に発生しうるので
///    ここで防御する）。
fn push_history_entry(session: &mut Session, user_id: &str, role: Role, text: String) {
    let role_label = match role {
        Role::Customer => "customer",
        Role::Assistant => "assistant",
    };
    let text = truncate_history_text(user_id, role_label, text);
    if text.trim().is_empty() {
        tracing::warn!(
            user_id,
            role = role_label,
            "line webhook: text is empty after trimming; not storing it in history (an empty \
             history entry would make every subsequent /api/reply request from this user fail \
             with 400)"
        );
        return;
    }
    session.history.push_back((role, text));
}

/// [`MAX_HISTORY_TEXT_CHARS`] へ切り詰める。バイト境界ではなく文字数で切り詰める規律は
/// [`truncate_chars`] に共通化してある（`truncate_for_line` も同じ関数を使う）。
fn truncate_history_text(user_id: &str, role: &'static str, text: String) -> String {
    let original_chars = text.chars().count();
    if original_chars <= MAX_HISTORY_TEXT_CHARS {
        return text;
    }
    tracing::warn!(
        user_id,
        role,
        original_chars,
        max_chars = MAX_HISTORY_TEXT_CHARS,
        "line webhook: history text exceeds the /api/reply contract; truncating before storing \
         it in the session"
    );
    truncate_chars(&text, MAX_HISTORY_TEXT_CHARS)
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
    /// message 以外のイベント（follow 等）。**想定内**の無視。LINE の webhook は
    /// message 以外のイベント種別を頻繁に送ってくるため、ログは出さない
    /// （呼び出し側で無言 `Ok(())` にする）。
    Ignore,
    /// message イベントだが `replyToken` / `source.userId` / `message.text` を欠く。
    /// LINE の実仕様では起こらない想定の契約違反であり、`unwrap` で panic させるより
    /// 当該イベントだけ無視して他のイベント処理を継続する方が安全だが、これは
    /// **想定外**の無視なので呼び出し側が `tracing::warn!` を出す（F3: グループ会話等で
    /// `source.userId` が欠けたテキストメッセージを無言で握りつぶすと、顧客には
    /// 「Bot が無反応」に見え、運用者にも気づく手段が無くなるため）。
    IgnoreMalformed {
        event_type: String,
        /// 欠けていたフィールド（`"replyToken"` / `"message"` / `"source.userId"` /
        /// `"message.text"` / 両方欠落時は `"source.userId and message.text"`）。
        missing_field: &'static str,
    },
}

/// `WebhookEvent` を [`RoutedEvent`] に振り分ける純関数（design doc §6 手順 2）。
fn route_event(event: &WebhookEvent) -> RoutedEvent {
    if event.event_type != "message" {
        return RoutedEvent::Ignore;
    }
    let Some(reply_token) = &event.reply_token else {
        return RoutedEvent::IgnoreMalformed {
            event_type: event.event_type.clone(),
            missing_field: "replyToken",
        };
    };
    let Some(message) = &event.message else {
        return RoutedEvent::IgnoreMalformed {
            event_type: event.event_type.clone(),
            missing_field: "message",
        };
    };
    if message.message_type != "text" {
        return RoutedEvent::NonText {
            reply_token: reply_token.clone(),
        };
    }
    let user_id = event.source.as_ref().and_then(|s| s.user_id.clone());
    let text = message.text.clone();
    match (user_id, text) {
        (Some(user_id), Some(text)) => RoutedEvent::Text {
            reply_token: reply_token.clone(),
            user_id,
            text,
        },
        (None, Some(_)) => RoutedEvent::IgnoreMalformed {
            event_type: event.event_type.clone(),
            missing_field: "source.userId",
        },
        (Some(_), None) => RoutedEvent::IgnoreMalformed {
            event_type: event.event_type.clone(),
            missing_field: "message.text",
        },
        (None, None) => RoutedEvent::IgnoreMalformed {
            event_type: event.event_type.clone(),
            missing_field: "source.userId and message.text",
        },
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

/// LINE Reply API へ送るメッセージ本文の最大文字数（実行計画
/// `docs/superpowers/plans/2026-08-11-answer-api-line-adapter.md` Task 7: 「4,900 字超は
/// 末尾切り詰め」。LINE 自体の上限は 5,000 字だが、安全マージンを取った値。design doc §6
/// 自体にはこの切り詰め値の記述は無い）。
const MAX_LINE_REPLY_CHARS: usize = 4_900;

/// 文字境界を壊さずに `max` で切り詰める（`harness::reply::truncate_chars` と同じ規律。
/// バイト数ではなく文字数で数える）。[`truncate_for_line`] と `SessionStore::update`
/// （F1: [`truncate_history_text`]）の両方から使う共通ユーティリティ。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

/// [`MAX_LINE_REPLY_CHARS`] で切り詰める。
fn truncate_for_line(text: &str) -> String {
    truncate_chars(text, MAX_LINE_REPLY_CHARS)
}

/// ログに出す reply_token の先頭文字数（Stage 2 レビュー指摘）。
const REPLY_TOKEN_LOG_PREFIX_CHARS: usize = 8;

/// `send_line_reply` 失敗時のエラーコンテキストへ reply_token を載せる際のフォーマット。
///
/// 以前は reply_token をログへ全文含めていた（「秘匿値ではないので出してよい」という判断）が、
/// 返信対象を一意に特定できる値を必要以上に全文ログへ残さない方針に変更する（今回のレビュー
/// 指摘採用）。先頭 [`REPLY_TOKEN_LOG_PREFIX_CHARS`] 文字と、`chars().count()` による全体の
/// 長さだけを残せば、運用者は「同じイベントか別イベントか」の判別や webhook payload との
/// 突き合わせに十分な情報を得られる。
fn reply_token_log_fragment(reply_token: &str) -> String {
    let prefix = truncate_chars(reply_token, REPLY_TOKEN_LOG_PREFIX_CHARS);
    let len = reply_token.chars().count();
    format!("reply_token_prefix={prefix} reply_token_len={len}")
}

/// LINE Reply API の本番 URL。テスト（item 5: LINE 返信失敗時にセッションが更新されない
/// ことの統合テスト）では `AppStateInner::line_reply_api_url` をローカルのモックサーバへ
/// 差し替える。本番コード（`main()`）は常にこの定数を使う。
const DEFAULT_LINE_REPLY_API_URL: &str = "https://api.line.me/v2/bot/message/reply";

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
    /// LINE Reply API の呼び出し先。本番は常に [`DEFAULT_LINE_REPLY_API_URL`]
    /// （`main()` が設定する）。テストだけがローカルのモックサーバ URL に差し替える
    /// （Stage 2 レビュー指摘: LINE 返信失敗時の挙動を実 HTTP 呼び出しで検証するため）。
    line_reply_api_url: String,
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
/// テキストメッセージの処理順は design doc §6 手順4に準拠する（Stage 2 レビュー指摘。
/// 以前は「LINE 返信が失敗してもセッションは更新する」順序を採っていたが、design doc の
/// 記述どおりに改めた）: ① セッションから case_id・履歴を取得 → ② 応答生成 API を呼ぶ →
/// ③ `assemble_reply` で reply_text と更新要否を決める → ④ LINE Reply API で返信する →
/// ⑤ 返信が成功した場合のみ [`apply_session_update`] でセッション（history・case_id）を
/// 更新する。LINE 返信が失敗した場合、顧客には返信が届いていないため会話としては完結して
/// おらず、セッション側の履歴・case_id も進めるべきではない（進めてしまうと、顧客が返信を
/// 受け取れないまま次のメッセージを送ったときに、サーバ側だけ会話が1ターン先に進んだ
/// 状態になり局所的に食い違う）。
///
/// ①〜⑤の全体は、同一ユーザーの [`SessionStore::lock_session`] のロックを保持したまま
/// 直列に実行される（Stage 2 レビュー指摘: 同一ユーザーからの並行イベントが get と update
/// の間に割り込めないようにするため）。LINE push の失敗そのものは `send_line_reply` の
/// `Err` を通じて呼び出し元（`webhook_handler`）が error ログを出す。
async fn handle_event(state: &AppState, event: &WebhookEvent) -> Result<()> {
    match route_event(event) {
        RoutedEvent::Ignore => Ok(()),
        // F3: 想定外の無視（message イベントなのに必須フィールドを欠く）。無言で握りつぶすと
        // 顧客には「Bot が無反応」に見え、運用者にも気づく手段が無くなるため warn する。
        RoutedEvent::IgnoreMalformed {
            event_type,
            missing_field,
        } => {
            tracing::warn!(
                event_type,
                missing_field,
                "line webhook: a message event is missing a required field; ignoring this \
                 event (the customer will see no reply)"
            );
            Ok(())
        }
        RoutedEvent::NonText { reply_token } => {
            send_line_reply(state, &reply_token, &state.nontext_text)
                .await
                // F2 / Stage 2 レビュー指摘: reply_token は全文ではなく先頭 8 文字+長さのみ
                // ログへ残す（返信対象を一意に特定できる値を必要以上に全文残さない方針へ変更。
                // reply_token_log_fragment のコメント参照）。
                .with_context(|| {
                    format!(
                        "send line reply (non-text) {}",
                        reply_token_log_fragment(&reply_token)
                    )
                })
        }
        RoutedEvent::Text {
            reply_token,
            user_id,
            text,
        } => {
            // ロックはこの分岐を抜けるまで（LINE 返信・(成功時のみ)update を含めて）保持し
            // 続ける。同一ユーザーの次のイベントは、この分岐が終わるまで get すら開始
            // できない（`SessionStore` の doc comment 参照）。
            let mut session = state.sessions.lock_session(&user_id).await;
            let case_id = session.case_id.clone();
            let history = session.history.clone();

            let api_response = call_answer_api(state, &user_id, &text, case_id, &history).await;
            let (reply_text, update) =
                assemble_reply(api_response.as_ref(), &text, &state.fallback_text);

            let reply_result = send_line_reply(state, &reply_token, &reply_text)
                .await
                // F2: どのユーザ/イベントの返信が失敗したか webhook_handler の error ログで
                // 分かるようにする。reply_token は全文ではなく先頭 8 文字+長さのみ
                // （Stage 2 レビュー指摘: reply_token_log_fragment のコメント参照）。
                .with_context(|| {
                    format!(
                        "send line reply user_id={user_id} {}",
                        reply_token_log_fragment(&reply_token)
                    )
                });

            // design doc §6 手順4 / handle_event の doc comment 参照: LINE 返信が成功した
            // 場合だけセッションを進める。
            if reply_result.is_ok() {
                if let Some(update) = update {
                    apply_session_update(&user_id, &mut session, update);
                }
            }

            reply_result
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
///
/// **F2**: すべてのログに `user_id` を含める（どの利用者の会話が壊れているか特定できるように
/// する）。非 200 の warn には応答生成 API が返したエラーボディ（先頭 500 字。こちらの API が
/// 生成したエラーメッセージであり秘密情報を含まない）も含める。
async fn call_answer_api(
    state: &AppState,
    user_id: &str,
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
            tracing::error!(
                user_id,
                error = ?err,
                "line webhook: failed to serialize answer api request"
            );
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
                user_id,
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
            tracing::error!(
                user_id,
                error = ?err,
                "line webhook: failed to read answer api response body"
            );
            return None;
        }
    };
    if !status.is_success() {
        tracing::warn!(
            user_id,
            %status,
            response_body = %truncate_chars(&text, 500),
            "line webhook: answer api returned a non-success status; falling back to the fixed \
             reply"
        );
        return None;
    }
    match serde_json::from_str::<AnswerApiResponse>(&text) {
        Ok(resp) => Some(resp),
        Err(err) => {
            tracing::error!(
                user_id,
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
        .post(&state.line_reply_api_url)
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
        // Stage 2 レビュー指摘: 本文読み取り自体が失敗した場合（コネクション切断等）を
        // 空文字列へ握りつぶさず、読み取り失敗の事実をログとエラーメッセージの両方に残す。
        // 空文字列のままだと「LINE が空のエラーボディを返した」のか「読み取りに失敗した」
        // のか運用者が区別できない。
        let body_text = match response.text().await {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(
                    error = ?err,
                    %status,
                    "line webhook: failed to read the line reply api's non-success response body"
                );
                "<failed to read response body>".to_string()
            }
        };
        anyhow::bail!("line reply api returned {status}: {body_text}");
    }
    Ok(())
}

/// `CS_ANSWER_API_URL` の形式検証（Stage 2 レビュー指摘）。
///
/// `require_env` は空白のみのチェックしかしないため、平文 `http://` の外部ホストのような
/// URL でも起動できてしまう。応答生成 API へは顧客の問い合わせ本文（`message` / `history`）
/// を送るため、経路が平文で外部に露出すると盗聴・改竄されうる。許可するのは:
///
/// - `https://` で始まる任意ホスト（本番の Cloud Run URL）
/// - `http://127.0.0.1`（ポート有無問わず）
/// - `http://localhost`（ポート有無問わず）
///
/// のいずれか。ローカル検証用の 2 パターンだけを平文 `http://` の例外として許し、それ以外の
/// `http://` は起動失敗させる（fail closed。設定ミスで平文外部送信のまま本番稼働に入るのを防ぐ）。
fn validate_answer_api_url(url: &str) -> Result<()> {
    let is_allowed = url.starts_with("https://")
        || url.starts_with("http://127.0.0.1")
        || url.starts_with("http://localhost");
    if is_allowed {
        Ok(())
    } else {
        anyhow::bail!(
            "CS_ANSWER_API_URL must start with https://, or http://127.0.0.1, or \
             http://localhost (got {url:?}); plaintext http:// to a non-local host would send \
             customer inquiry text unencrypted"
        )
    }
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

/// 任意 env（`CS_LINE_FALLBACK_TEXT` / `CS_LINE_NONTEXT_TEXT`）の値を確定する。
/// 未設定・空文字列・空白のみのいずれも既定文へ正規化する（fail closed にはしない —
/// 呼び出し元は必須の4つとは違い、この2つは無くても起動を続けてよい任意設定のため）。
///
/// これが必要な理由: 空文字列がそのまま LINE Reply API に渡ると 400 で拒否され
/// `send_line_reply` が `bail!` する。しかもこの2値は「応答生成APIが落ちている最中」
/// （フォールバックが最も必要な場面）に使われるため、設定ミスが最悪のタイミングで顕在化し、
/// 顧客への応答が完全に無くなる。
///
/// `env::var` を直接受けず `Option<String>` を引数分離しているのは、`require_env` の
/// コメントと同じ理由（実プロセスの環境変数を書き換えるテストは並列実行で不安定になる）。
fn resolve_optional_text_env(raw: Option<String>, default: &str) -> String {
    raw.map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

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
    validate_answer_api_url(&answer_api_url)?;
    let answer_api_key = require_env("CS_ANSWER_API_KEY")?;
    let fallback_text = resolve_optional_text_env(
        env::var("CS_LINE_FALLBACK_TEXT").ok(),
        DEFAULT_FALLBACK_TEXT,
    );
    let nontext_text =
        resolve_optional_text_env(env::var("CS_LINE_NONTEXT_TEXT").ok(), DEFAULT_NONTEXT_TEXT);

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
        line_reply_api_url: DEFAULT_LINE_REPLY_API_URL.to_string(),
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
        .merge(cs_support_mcp::health::health_router())
        // F5: 正常系のリクエストが 1 行もログに残らないと、webhook が届いているか自体を
        // 運用時に確認できない。main.rs のトップレベル app と同じパターン。
        .layer(TraceLayer::new_for_http());

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
    //
    // Stage 2 レビュー指摘により、同一ユーザーの get→update は `lock_session` の
    // ロックを保持したまま直列に行う構造になった。テストは書き込みを
    // `store_update`（`lock_session` → `apply_session_update`）ヘルパー経由で行い、
    // 読み取りは読み取り専用の `snapshot` を使う。

    /// 本番の `handle_event` と同じ経路（`lock_session` → `apply_session_update`）を
    /// 経由するテスト用ヘルパー。
    async fn store_update(
        store: &SessionStore,
        user_id: &str,
        case_id: &str,
        customer_text: &str,
        assistant_text: &str,
    ) {
        let mut session = store.lock_session(user_id).await;
        apply_session_update(
            user_id,
            &mut session,
            SessionUpdate {
                case_id: case_id.to_string(),
                customer_text: customer_text.to_string(),
                assistant_text: assistant_text.to_string(),
            },
        );
    }

    #[tokio::test]
    async fn session_store_expires_entries_after_ttl() {
        let store = SessionStore::with_limits(Duration::from_millis(20), 10);
        store_update(&store, "u1", "case-1", "q", "a").await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        let (case_id, history) = store.snapshot("u1").await;
        assert!(
            case_id.is_none(),
            "an entry past its TTL must be treated as absent"
        );
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn session_store_evicts_the_oldest_entry_when_over_capacity() {
        let store = SessionStore::with_limits(Duration::from_secs(3600), 2);
        store_update(&store, "u1", "case-1", "q1", "a1").await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        store_update(&store, "u2", "case-2", "q2", "a2").await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        // store is now full (max_entries = 2); inserting a 3rd user must evict the
        // least-recently-touched one (u1).
        store_update(&store, "u3", "case-3", "q3", "a3").await;

        assert!(
            store.snapshot("u1").await.0.is_none(),
            "the oldest entry must be evicted when the store is at capacity"
        );
        assert!(store.snapshot("u2").await.0.is_some());
        assert!(store.snapshot("u3").await.0.is_some());
    }

    #[tokio::test]
    async fn session_store_caps_history_at_20_turns_dropping_the_oldest() {
        let store = SessionStore::with_limits(Duration::from_secs(3600), 10);
        // 15 updates * 2 turns (customer + assistant) = 30 turns pushed; only the
        // newest 20 must survive.
        for i in 0..15 {
            store_update(&store, "u1", "case-1", &format!("q{i}"), &format!("a{i}")).await;
        }
        let (_, history) = store.snapshot("u1").await;
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

    // ---- SessionStore の保存前正規化（F1: /api/reply の入力契約に必ず適合させる） ----

    #[tokio::test]
    async fn session_store_truncates_history_text_over_2000_chars_on_update() {
        let store = SessionStore::with_limits(Duration::from_secs(3600), 10);
        let long_customer_text = "a".repeat(MAX_HISTORY_TEXT_CHARS + 500);
        store_update(&store, "u1", "case-1", &long_customer_text, "short reply").await;
        let (_, history) = store.snapshot("u1").await;
        let (role, text) = &history[0];
        assert_eq!(*role, Role::Customer);
        assert_eq!(
            text.chars().count(),
            MAX_HISTORY_TEXT_CHARS,
            "text over the /api/reply history text limit must be truncated at save time, or \
             the next request from this user will get a 400 and the session can never recover"
        );
    }

    #[tokio::test]
    async fn session_store_truncates_multibyte_history_text_without_panicking_on_a_char_boundary() {
        let store = SessionStore::with_limits(Duration::from_secs(3600), 10);
        let long_ja_text = "あ".repeat(2_500);
        store_update(&store, "u1", "case-1", &long_ja_text, "assistant reply").await;
        let (_, history) = store.snapshot("u1").await;
        let (_, text) = &history[0];
        assert_eq!(text.chars().count(), MAX_HISTORY_TEXT_CHARS);
        assert_eq!(text, &"あ".repeat(MAX_HISTORY_TEXT_CHARS));
    }

    #[tokio::test]
    async fn session_store_leaves_history_text_at_or_under_2000_chars_untouched() {
        let store = SessionStore::with_limits(Duration::from_secs(3600), 10);
        let exact_limit_text = "b".repeat(MAX_HISTORY_TEXT_CHARS);
        store_update(&store, "u1", "case-1", &exact_limit_text, "reply").await;
        let (_, history) = store.snapshot("u1").await;
        assert_eq!(history[0], (Role::Customer, exact_limit_text));
    }

    #[tokio::test]
    async fn session_store_does_not_store_history_text_that_is_empty_after_trimming() {
        // 応答生成 API が `reply_text: ""` を返した場合の防御(reviewer 追加指摘)。
        let store = SessionStore::with_limits(Duration::from_secs(3600), 10);
        store_update(&store, "u1", "case-1", "question", "   ").await;
        let (_, history) = store.snapshot("u1").await;
        assert_eq!(
            history.len(),
            1,
            "the whitespace-only assistant turn must not be stored, but the customer turn must"
        );
        assert_eq!(history[0], (Role::Customer, "question".to_string()));
    }

    #[tokio::test]
    async fn session_store_does_not_store_a_case_id_over_128_chars() {
        // reviewer 追加指摘: 超過値を保存すると次回リクエストが必ず 400 になる。
        let store = SessionStore::with_limits(Duration::from_secs(3600), 10);
        let too_long_case_id = "c".repeat(MAX_CASE_ID_CHARS + 1);
        store_update(&store, "u1", &too_long_case_id, "q", "a").await;
        let (case_id, _) = store.snapshot("u1").await;
        assert!(
            case_id.is_none(),
            "a case_id over the /api/reply limit must not be stored"
        );
    }

    #[tokio::test]
    async fn session_store_keeps_the_previous_case_id_when_a_new_one_exceeds_128_chars() {
        let store = SessionStore::with_limits(Duration::from_secs(3600), 10);
        store_update(&store, "u1", "case-1", "q1", "a1").await;
        let too_long_case_id = "c".repeat(MAX_CASE_ID_CHARS + 1);
        store_update(&store, "u1", &too_long_case_id, "q2", "a2").await;
        let (case_id, _) = store.snapshot("u1").await;
        assert_eq!(
            case_id,
            Some("case-1".to_string()),
            "an over-limit case_id must not overwrite the previous valid one"
        );
    }

    // ---- 同一ユーザーの直列化（item 4: get→呼び出し→update をロックで直列化） ----

    /// `lock_session` を保持したまま "start" を記録し、擬似的な非同期処理（応答生成 API
    /// 呼び出し + LINE 返信に相当する遅延）を挟んでから "end" を記録する。実行順序ログが
    /// interleave していなければ、同一ユーザーの 2 つの処理が直列化されていることになる。
    #[tokio::test]
    async fn same_user_events_are_processed_serially_not_interleaved() {
        let store = Arc::new(SessionStore::with_limits(Duration::from_secs(3600), 10));
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

        async fn one_event(
            store: Arc<SessionStore>,
            log: Arc<Mutex<Vec<&'static str>>>,
            label: &'static str,
            start_mark: &'static str,
            end_mark: &'static str,
            simulated_work: Duration,
        ) {
            let mut session = store.lock_session("u1").await;
            log.lock().unwrap().push(start_mark);
            // 応答生成 API 呼び出し + LINE 返信の待ち時間に相当する擬似的な非同期処理。
            // ロック（`session`）を保持したまま await することが直列化の要。
            tokio::time::sleep(simulated_work).await;
            apply_session_update(
                "u1",
                &mut session,
                SessionUpdate {
                    case_id: format!("case-{label}"),
                    customer_text: format!("q-{label}"),
                    assistant_text: format!("a-{label}"),
                },
            );
            log.lock().unwrap().push(end_mark);
        }

        let task_a = tokio::spawn(one_event(
            store.clone(),
            log.clone(),
            "a",
            "a-start",
            "a-end",
            Duration::from_millis(40),
        ));
        // A が確実に先にロックを取得できるよう、B の起床をわずかに遅らせる
        // （B が先に spawn 側でスケジューリングされても、ロック取得順を保証するため）。
        tokio::time::sleep(Duration::from_millis(10)).await;
        let task_b = tokio::spawn(one_event(
            store.clone(),
            log.clone(),
            "b",
            "b-start",
            "b-end",
            Duration::from_millis(5),
        ));

        task_a.await.expect("task a must not panic");
        task_b.await.expect("task b must not panic");

        let recorded = log.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec!["a-start", "a-end", "b-start", "b-end"],
            "b must not start (acquire the lock) until a's full get-through-update cycle has \
             finished, and vice versa if the ordering were reversed: {recorded:?}"
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
    fn route_event_flags_a_message_event_missing_reply_token_as_malformed() {
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
        assert_eq!(
            route_event(&event),
            RoutedEvent::IgnoreMalformed {
                event_type: "message".to_string(),
                missing_field: "replyToken",
            }
        );
    }

    #[test]
    fn route_event_flags_a_message_event_missing_the_message_field_as_malformed() {
        let event = WebhookEvent {
            event_type: "message".to_string(),
            reply_token: Some("rt1".to_string()),
            source: Some(EventSource {
                user_id: Some("U1".to_string()),
            }),
            message: None,
        };
        assert_eq!(
            route_event(&event),
            RoutedEvent::IgnoreMalformed {
                event_type: "message".to_string(),
                missing_field: "message",
            }
        );
    }

    #[test]
    fn route_event_flags_a_text_message_missing_source_user_id_as_malformed() {
        // グループ会話等で source.userId が欠けたテキストメッセージ（F3）。
        let event = WebhookEvent {
            event_type: "message".to_string(),
            reply_token: Some("rt1".to_string()),
            source: Some(EventSource { user_id: None }),
            message: Some(EventMessage {
                message_type: "text".to_string(),
                text: Some("hi".to_string()),
            }),
        };
        assert_eq!(
            route_event(&event),
            RoutedEvent::IgnoreMalformed {
                event_type: "message".to_string(),
                missing_field: "source.userId",
            }
        );
    }

    #[test]
    fn route_event_flags_a_text_message_missing_text_as_malformed() {
        let event = WebhookEvent {
            event_type: "message".to_string(),
            reply_token: Some("rt1".to_string()),
            source: Some(EventSource {
                user_id: Some("U1".to_string()),
            }),
            message: Some(EventMessage {
                message_type: "text".to_string(),
                text: None,
            }),
        };
        assert_eq!(
            route_event(&event),
            RoutedEvent::IgnoreMalformed {
                event_type: "message".to_string(),
                missing_field: "message.text",
            }
        );
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

    // ---- handle_event（item 5: LINE 返信成功後にだけセッションを更新する） ----
    //
    // `send_line_reply` は本番では `https://api.line.me/...` を叩くため、成功・失敗を
    // 直接注入する手段が無い。`AppStateInner::line_reply_api_url` をテスト専用に
    // ローカルのモック HTTP サーバへ差し替えることで、実際の `handle_event` の経路
    // （get→応答生成 API 呼び出し→LINE 返信→(成功時のみ)update）を実 HTTP 呼び出しで
    // 検証する（本番コードは `line_reply_api_url` を [`DEFAULT_LINE_REPLY_API_URL`]
    // 固定で使うため、この差し替えはテストにしか効かない）。

    /// `127.0.0.1:0`（OS が空きポートを割り当てる）でモック HTTP サーバを起動し、
    /// ベース URL（`http://127.0.0.1:<port>`）を返す。
    async fn spawn_http_mock(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock http listener");
        let addr = listener.local_addr().expect("mock listener local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock http server must not error");
        });
        format!("http://{addr}")
    }

    /// 応答生成 API のモック: 常に 200 + 固定の `{reply_text, case_id}` を返す。
    async fn answer_api_ok_handler() -> impl axum::response::IntoResponse {
        axum::Json(serde_json::json!({
            "reply_text": "こちらが回答です",
            "case_id": "case-abc",
        }))
    }

    fn test_app_state(answer_api_url: String, line_reply_api_url: String) -> AppState {
        Arc::new(AppStateInner {
            channel_secret: "test-channel-secret".to_string(),
            channel_access_token: "test-channel-access-token".to_string(),
            answer_api_url,
            answer_api_key: "test-answer-api-key".to_string(),
            fallback_text: DEFAULT_FALLBACK_TEXT.to_string(),
            nontext_text: DEFAULT_NONTEXT_TEXT.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("build test reqwest client"),
            sessions: SessionStore::with_limits(Duration::from_secs(3600), 10),
            line_reply_api_url,
        })
    }

    fn text_webhook_event(user_id: &str, reply_token: &str, text: &str) -> WebhookEvent {
        WebhookEvent {
            event_type: "message".to_string(),
            reply_token: Some(reply_token.to_string()),
            source: Some(EventSource {
                user_id: Some(user_id.to_string()),
            }),
            message: Some(EventMessage {
                message_type: "text".to_string(),
                text: Some(text.to_string()),
            }),
        }
    }

    #[tokio::test]
    async fn handle_event_updates_the_session_only_after_a_successful_line_reply() {
        let answer_api_base =
            spawn_http_mock(Router::new().route("/reply", post(answer_api_ok_handler))).await;
        let line_ok_base =
            spawn_http_mock(Router::new().route("/reply", post(|| async { StatusCode::OK }))).await;

        let state = test_app_state(
            format!("{answer_api_base}/reply"),
            format!("{line_ok_base}/reply"),
        );
        let event = text_webhook_event("u1", "rt1", "こんにちは");

        handle_event(&state, &event).await.expect(
            "handle_event must succeed when both the answer api and the line reply succeed",
        );

        let (case_id, history) = state.sessions.snapshot("u1").await;
        assert_eq!(
            case_id,
            Some("case-abc".to_string()),
            "case_id from the answer api must be saved once the line reply has succeeded"
        );
        assert_eq!(
            history.len(),
            2,
            "both the customer and assistant turns must be recorded"
        );
    }

    #[tokio::test]
    async fn handle_event_leaves_the_session_untouched_when_the_line_reply_fails() {
        let answer_api_base =
            spawn_http_mock(Router::new().route("/reply", post(answer_api_ok_handler))).await;
        let line_fail_base = spawn_http_mock(Router::new().route(
            "/reply",
            post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        ))
        .await;

        let state = test_app_state(
            format!("{answer_api_base}/reply"),
            format!("{line_fail_base}/reply"),
        );
        let event = text_webhook_event("u1", "rt1", "こんにちは");

        let result = handle_event(&state, &event).await;
        assert!(
            result.is_err(),
            "handle_event must surface the line reply failure as an Err so webhook_handler logs \
             it (F2)"
        );

        let (case_id, history) = state.sessions.snapshot("u1").await;
        assert!(
            case_id.is_none(),
            "the answer api returned 200 but the line reply failed, so the session must remain \
             untouched (design doc §6 step 4 / handle_event doc comment)"
        );
        assert!(history.is_empty());
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

    // ---- validate_answer_api_url（Stage 2 レビュー指摘: 平文外部送信を起動時に拒否） ----

    #[test]
    fn validate_answer_api_url_accepts_https() {
        assert!(
            validate_answer_api_url("https://cs-support-mcp-xxx.run.app/urtect/api/reply").is_ok()
        );
    }

    #[test]
    fn validate_answer_api_url_accepts_http_127_0_0_1_with_port() {
        assert!(validate_answer_api_url("http://127.0.0.1:3000/urtect/api/reply").is_ok());
    }

    #[test]
    fn validate_answer_api_url_accepts_http_127_0_0_1_without_port() {
        assert!(validate_answer_api_url("http://127.0.0.1/urtect/api/reply").is_ok());
    }

    #[test]
    fn validate_answer_api_url_accepts_http_localhost_with_port() {
        assert!(validate_answer_api_url("http://localhost:3000/urtect/api/reply").is_ok());
    }

    #[test]
    fn validate_answer_api_url_accepts_http_localhost_without_port() {
        assert!(validate_answer_api_url("http://localhost/urtect/api/reply").is_ok());
    }

    #[test]
    fn validate_answer_api_url_rejects_plaintext_http_to_external_host() {
        let err = validate_answer_api_url("http://evil.example.com/urtect/api/reply")
            .expect_err("plaintext http to a non-local host must be rejected");
        assert!(
            err.to_string().contains("CS_ANSWER_API_URL"),
            "error must name the offending env var: {err}"
        );
    }

    #[test]
    fn validate_answer_api_url_rejects_a_value_with_no_scheme() {
        assert!(validate_answer_api_url("cs-support-mcp-xxx.run.app/urtect/api/reply").is_err());
    }

    // ---- reply_token_log_fragment（Stage 2 レビュー指摘: ログへ全文を出さない） ----

    #[test]
    fn reply_token_log_fragment_shows_only_the_first_8_chars_and_the_length() {
        let full = "0f3779fba3b349968c5d07db31eabf65";
        let fragment = reply_token_log_fragment(full);
        assert_eq!(
            fragment,
            format!(
                "reply_token_prefix=0f3779fb reply_token_len={}",
                full.chars().count()
            )
        );
        assert!(
            !fragment.contains(full),
            "the full reply_token must not appear in the log fragment: {fragment}"
        );
    }

    #[test]
    fn reply_token_log_fragment_does_not_panic_on_a_token_shorter_than_the_prefix() {
        let fragment = reply_token_log_fragment("ab");
        assert_eq!(fragment, "reply_token_prefix=ab reply_token_len=2");
    }

    // ---- resolve_optional_text_env ----
    //
    // W3 (CONFIRMED, reviewer): CS_LINE_FALLBACK_TEXT / CS_LINE_NONTEXT_TEXT are optional
    // env vars, but "unset -> default" alone let an empty or whitespace-only value pass
    // straight through as the reply text. LINE's Reply API rejects an empty message body
    // with 400, so send_line_reply would bail! at exactly the moment the fallback text is
    // needed most (the answer API is already down). Both "unset" and "blank" must resolve
    // to the default. We test the pure decision function directly instead of env::var,
    // per the note on require_env above: mutating real process env vars is flaky under
    // parallel test execution.

    #[test]
    fn resolve_optional_text_env_uses_default_when_unset() {
        assert_eq!(resolve_optional_text_env(None, "default"), "default");
    }

    #[test]
    fn resolve_optional_text_env_uses_default_when_empty() {
        assert_eq!(
            resolve_optional_text_env(Some(String::new()), "default"),
            "default"
        );
    }

    #[test]
    fn resolve_optional_text_env_uses_default_when_whitespace_only() {
        assert_eq!(
            resolve_optional_text_env(Some("   ".to_string()), "default"),
            "default"
        );
    }

    #[test]
    fn resolve_optional_text_env_uses_trimmed_value_when_present() {
        assert_eq!(
            resolve_optional_text_env(Some("  カスタム文言  ".to_string()), "default"),
            "カスタム文言"
        );
    }
}
