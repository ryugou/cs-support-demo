//! LINE webhook アダプタ（判断ゼロの別バイナリ）。
//!
//! 仕様の正本は `docs/superpowers/specs/2026-08-11-answer-api-line-adapter-design.md`
//! （以下「design doc」）§6。判定・応答文生成はすべて `POST /{project_id}/api/reply`
//! （`cs_support_mcp::api`）側で完結しており、このバイナリは
//!
//! 1. `X-Line-Signature` を検証する
//! 2. テキストイベントの処理中、chat loading API で処理中アニメーションを表示する
//!    （失敗しても warn ログのみで継続する。表示は体験改善であり必須機能ではない）
//! 3. テキストメッセージだけを応答生成 API へ 1 回渡す
//! 4. 返ってきた `reply_text` を LINE へ返信する
//! 5. ユーザ単位の会話履歴・case_id をプロセス内メモリに保持する
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

    /// エントリ上限超過時に破棄する対象を選ぶ。
    ///
    /// 生存判定は `Arc::strong_count(session) == 1`（このマップだけがこの `Arc` を握って
    /// いる）で行う。以前は `try_lock()` の成否で判定していたが、それでは不十分だった:
    /// `get_or_create` が呼び出し元へ `Arc` を返してから、呼び出し元が実際に
    /// `arc.lock_owned().await` してロックを取得するまでの window では、誰もロックを
    /// 保持していないのに処理中である。この window 中は `try_lock()` が成功してしまうため、
    /// ロック取得前の handler が eviction 対象に含まってしまう欠陥があった（Stage 2 レビュー
    /// 指摘・実際に発生した競合）。`strong_count` はロックの有無ではなく「誰かがこの
    /// `Arc` を保持しているか」を直接見るため、この window も保護対象に含む。
    ///
    /// この関数は呼び出し元（[`Self::get_or_create`]）が外側の `users: Mutex<..>` を
    /// 保持したまま呼ぶため、ここで読む `strong_count` は一貫したスナップショットになる
    /// （呼び出し中に他スレッドがこのマップの外へ新たに clone/drop することはない）。
    ///
    /// 候補（`strong_count == 1`）に限って `last_at` を読む。この時点で他に保持者はいない
    /// ので `try_lock()` は必ず成功するが、万一失敗しても `expect` で落とさず安全側に候補
    /// から除外する。全ユーザーが処理中で候補が無い場合は `None` を返し、呼び出し元は
    /// eviction をスキップする＝一時的に上限を超えることを許容する。
    fn pick_lru_key(users: &HashMap<String, Arc<TokioMutex<Session>>>) -> Option<String> {
        users
            .iter()
            .filter(|(_, session)| Arc::strong_count(session) == 1)
            .filter_map(|(key, session)| {
                session
                    .try_lock()
                    .ok()
                    .map(|guard| (key.clone(), guard.last_at))
            })
            .min_by_key(|(_, last_at)| *last_at)
            .map(|(key, _)| key)
    }

    /// 該当ユーザーの一連の処理（chat loading API 呼び出し→get→応答生成 API 呼び出し→(200
    /// なら) case_id + customer ターン保存→LINE 返信→(成功時のみ) assistant ターン追記）を
    /// ロック取得時点からイベント処理全体を通して直列化するロックを取得する（Stage 2 codex
    /// レビュー round 2 Suggestion 4: chat loading API 呼び出しをロック取得後に呼ぶよう
    /// `handle_event` を変更した際、この一覧が旧契約（chat loading API を含まない）のまま
    /// 取り残されていたのを更新）。返り値の `OwnedMutexGuard` を保持し続けている間、同一
    /// ユーザーの他の呼び出しはこの `await` で待たされる。
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
    /// `main` がバックグラウンドタスクで一定間隔ごとに呼ぶ）。
    ///
    /// 生存判定は [`Self::pick_lru_key`] と同じ理由で `Arc::strong_count(session) == 1`
    /// を使う（`try_lock` 単独では、ロック取得前に `Arc` だけを保持している window を
    /// 処理中と認識できず、進行中の会話を誤って削除しうる）。`strong_count != 1`（処理中）
    /// のエントリは TTL を超えていても保持し、次回のスイープで再評価する。
    fn sweep(&self) {
        let mut users = self
            .users
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ttl = self.ttl;
        users.retain(|_, session| {
            if Arc::strong_count(session) != 1 {
                return true;
            }
            match session.try_lock() {
                Ok(guard) => guard.last_at.elapsed() <= ttl,
                Err(_) => true,
            }
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

/// [`SessionStore::lock_session`] で得たロック済みセッションへ、`case_id` と customer
/// ターンを書き込む（design doc §6 手順4: 応答生成 API が 200 を返した時点で、LINE への
/// 返信を試みる**前**に呼ぶ）。
///
/// サーバ側では 200 の時点で case が確定し signal が追記済みのため、以降の発話を同じ case
/// に必ず合流させる必要がある。この保存を LINE 返信の成否に左右させると、返信が失敗した
/// ときに次の発話が新規 case として扱われ、蓄積済みの signal がエスカレーション判定から
/// 脱落してしまう。呼び出し元は `handle_event` を参照。
///
/// **保存前に `/api/reply` の入力契約（design doc §2、`server/src/api.rs`）へ正規化する
/// （F1）**: history テキストは [`MAX_HISTORY_TEXT_CHARS`] へ切り詰め、trim 後に空になる
/// テキストは保存しない。`case_id` は [`MAX_CASE_ID_CHARS`] を超えたら保存しない
/// （切り詰めではなく非保存。理由は同定数のコメント）。これらを怠ると、次回リクエストが
/// 必ず 400 になり、セッションが TTL 経過まで自己回復しない。
fn apply_customer_turn(
    user_id: &str,
    session: &mut Session,
    case_id: String,
    customer_text: String,
) {
    let case_id_chars = case_id.chars().count();
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
        session.case_id = Some(case_id);
    }

    push_history_entry(session, user_id, Role::Customer, customer_text);
    while session.history.len() > MAX_HISTORY_TURNS {
        session.history.pop_front();
    }
    session.last_at = Instant::now();
}

/// [`SessionStore::lock_session`] で得たロック済みセッションへ、assistant ターンを書き込む
/// （design doc §6 手順4: LINE への返信が成功した場合だけ呼ぶ）。
///
/// 顧客が実際に受信していない発話を履歴に残さないため、[`apply_customer_turn`] とは別に
/// 呼び出しタイミングを分けている。呼び出し元は `handle_event` を参照。
///
/// 正規化（F1）は [`apply_customer_turn`] と同じ規律を使う（[`push_history_entry`] 経由で
/// [`MAX_HISTORY_TEXT_CHARS`] へ切り詰め、trim 後に空になるテキストは保存しない）。
fn apply_assistant_turn(user_id: &str, session: &mut Session, assistant_text: String) {
    push_history_entry(session, user_id, Role::Assistant, assistant_text);
    while session.history.len() > MAX_HISTORY_TURNS {
        session.history.pop_front();
    }
    session.last_at = Instant::now();
}

/// [`apply_customer_turn`] / [`apply_assistant_turn`] が保存直前に呼ぶ、history 1 エントリ
/// 分の正規化（F1）。
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

/// `assemble_reply` が返す、応答生成 API が 200 を返した場合にセッションへ保存すべき値を
/// まとめた中間値。`case_id` / `customer_text` は [`apply_customer_turn`] へ、
/// `assistant_text` は [`apply_assistant_turn`] へ渡す。この 2 つの保存は呼び出しタイミングが
/// 異なる（design doc §6 手順4: 前者は LINE 返信前に必ず、後者は返信成功時のみ）ため、
/// 呼び出し元（`handle_event`）でフィールドを分けて使う。
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
///   ただしこの 2 ターンは同時には追記されない。追記タイミングは呼び出し元（`handle_event`）
///   が customer 側（LINE 返信前・[`apply_customer_turn`]）と assistant 側（LINE 返信成功後・
///   [`apply_assistant_turn`]）とで分けて適用する（[`SessionUpdate`] の doc comment 参照）。
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

/// LINE Reply API へ送るメッセージ本文の最大文字数（design doc §6 に記述あり。LINE 自体の
/// 上限は 5,000 字だが、安全マージンを取った値）。
const MAX_LINE_REPLY_CHARS: usize = 4_900;

/// 文字境界を壊さずに `max` で切り詰める（`harness::reply::truncate_chars` と同じ規律。
/// バイト数ではなく文字数で数える）。[`truncate_for_line`] と [`apply_customer_turn`] /
/// [`apply_assistant_turn`]（F1: [`truncate_history_text`]）の両方から使う共通ユーティリティ。
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

/// LINE chat loading API の本番 URL（design doc §6 手順3）。テキストイベント処理の開始前に
/// 呼び、処理中アニメーションを表示する。テストでは `AppStateInner::line_loading_api_url` を
/// ローカルのモックサーバへ差し替える。本番コード（`main()`）は常にこの定数を使う。
const DEFAULT_LINE_LOADING_API_URL: &str = "https://api.line.me/v2/bot/chat/loading/start";

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
    /// LINE chat loading API の呼び出し先。本番は常に [`DEFAULT_LINE_LOADING_API_URL`]
    /// （`main()` が設定する）。テストはローカルのモックサーバ URL に差し替える
    /// （`line_reply_api_url` と同じ位置づけ・同じ理由）。
    line_loading_api_url: String,
    /// chat loading API 呼び出しの per-request timeout。本番は常に
    /// [`DEFAULT_LINE_LOADING_TIMEOUT`]（`main()` が設定する）。テストは、ストールした
    /// chat loading API が応答生成 API 呼び出しをブロックしないことを検証するために短く
    /// 差し替える（`start_loading_indicator` の doc comment 参照。Stage 1 レビュー指摘
    /// Critical 1）。
    line_loading_timeout: Duration,
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
/// テキストメッセージの処理順は design doc §6 手順3・4に準拠する: ロックを取得した直後に
/// ① chat loading API を呼んで処理中アニメーションを表示する（ロック取得**後**に呼ぶ理由は
/// 後述） → ② セッションから case_id・履歴を取得 → ③ 応答生成 API を呼ぶ →
/// ④ `assemble_reply` で reply_text と更新要否を決める → ⑤ 応答生成 API が 200 を返して
/// いれば、LINE への返信を試みる**前**に [`apply_customer_turn`] で `case_id` と customer
/// ターンを保存する → ⑥ LINE Reply API で返信する → ⑦ 返信が成功した場合のみ
/// [`apply_assistant_turn`] で assistant ターンを追記する。
///
/// ⑤を LINE 返信の成否より前に置く理由: サーバ側では 200 の時点で case が確定し signal が
/// 追記済みのため、以降の発話を同じ case に必ず合流させる必要がある。これを返信の成否で
/// 左右させると、返信失敗時に次の発話が新規 case となり、蓄積済みの signal がエスカレー
/// ション判定から脱落する。一方 assistant ターンは顧客が実際に受信していない発話を履歴に
/// 残さないため、返信成功時のみ追記する。
///
/// ①〜⑦の全体は、同一ユーザーの [`SessionStore::lock_session`] のロックを保持したまま
/// 直列に実行される（Stage 2 レビュー指摘: 同一ユーザーからの並行イベントが get と update
/// の間に割り込めないようにするため）。**① の chat loading API 呼び出しもこのロックの内側
/// にある（Stage 2 codex レビュー指摘 Warning 4）**: ロック取得**前**に呼ぶと、同一ユーザー
/// からの並行イベントの処理順が loading API の応答速度で決まってしまう（loading API が遅い
/// 先行イベントを、速い後着イベントが追い越してセッションを先に読み・応答生成 API を先に
/// 呼び・case_id を先に確定させる）。両方の返信は届くため、これは顧客に見える形の障害には
/// ならず、会話履歴・signal 累積・case_id 確定の順序だけがサイレントに入れ替わり回答品質が
/// 劣化する（気づく手段が無い）。トレードオフ: ロックの内側に置くと、同一ユーザーの後続
/// イベントは先行イベントの処理が終わるまで自分のローディング表示が出ない。ただし後続
/// イベントはどのみち返信自体も待たされるため、実際に処理が始まった時点で表示する方が
/// 表示として正確であり、会話順序の整合性を優先する。LINE push の失敗そのものは
/// `send_line_reply` の `Err` を通じて呼び出し元（`webhook_handler`）が error ログを出す。
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
            // ロックはこの分岐を抜けるまで（loading indicator の表示・LINE 返信・(成功時の
            // み)assistant ターンの保存を含めて）保持し続ける。同一ユーザーの次のイベントは、
            // この分岐が終わるまで get すら開始できない（`SessionStore` の doc comment 参照）。
            let mut session = state.sessions.lock_session(&user_id).await;

            // design doc §6 手順3: セッション読み取り・応答生成 API 呼び出しより前に、処理中
            // アニメーションを表示する。失敗しても warn ログのみで後続処理を継続する（呼び出し
            // 先 `start_loading_indicator` の doc comment 参照）。**ロック取得後に呼ぶ**（Stage
            // 2 codex レビュー指摘 Warning 4。理由は `handle_event` の doc comment 参照）。
            start_loading_indicator(state, &user_id).await;

            let case_id = session.case_id.clone();
            let history = session.history.clone();

            let api_response = call_answer_api(state, &user_id, &text, case_id, &history).await;
            let (reply_text, update) =
                assemble_reply(api_response.as_ref(), &text, &state.fallback_text);

            // design doc §6 手順4 / handle_event の doc comment 参照: 応答生成 API が 200 を
            // 返した時点（= update が Some）で、LINE への返信を試みる前に case_id と
            // customer ターンを保存する。返信の成否に左右させない。
            let pending_assistant_text = update.map(|update| {
                apply_customer_turn(&user_id, &mut session, update.case_id, update.customer_text);
                update.assistant_text
            });

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
            // 場合だけ assistant ターンを追記する。
            if reply_result.is_ok() {
                if let Some(assistant_text) = pending_assistant_text {
                    apply_assistant_turn(&user_id, &mut session, assistant_text);
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

/// [`start_loading_indicator`] の per-request timeout の既定値（Stage 1 レビュー指摘 Critical
/// 1）。本番は常にこの定数を使う（`main()` が設定する）。テストは
/// `AppStateInner::line_loading_timeout` を短く差し替える。
const DEFAULT_LINE_LOADING_TIMEOUT: Duration = Duration::from_secs(3);

/// LINE chat loading API（`POST https://api.line.me/v2/bot/chat/loading/start`）を 1 回呼び、
/// 処理中アニメーションを表示する（design doc §6 手順3）。セッションのロックを取得した
/// **直後**、セッション読み取り・応答生成 API 呼び出しより前に呼ぶ（`handle_event` 参照。
/// Stage 2 codex レビュー指摘 Warning 4: ロック取得**前**に呼ぶと、同一ユーザーの並行イベント
/// の処理順が loading API の応答速度で決まってしまい、会話順序が入れ替わりうる）。
///
/// **この呼び出しの失敗は warn ログのみで処理を継続する。** 表示は体験改善であり必須機能では
/// ないため、`Result` を返さず呼び出し元へ何も伝播させない（ネットワークエラー・非 2xx の
/// いずれも同様に warn で continue）。
///
/// **per-request timeout が必須（Stage 1 レビュー指摘 Critical 1）**: `state.http` は
/// `main()` で 50 秒のクライアント既定 timeout を持つ（応答生成 API 用）。これをそのまま
/// 使うと、chat loading API がストールした場合に**応答生成 API を呼ぶ前に最大 50 秒ブロック
/// する**。design doc §6 の「1 イベントあたりの累積タイムアウトは最大で約 103 秒（chat
/// loading API の per-request timeout 3 秒を含む）」という Accepted Risk の予算は、この
/// per-request timeout が無ければ chat loading API 側だけで**さらに +47 秒**（3 秒 → 50 秒）
/// 膨らみ、約 150 秒に達する。これは LINE reply token の実効予算（実測往復 10〜15 秒、§8）を
/// 食い潰して顧客が返信を一切受け取れなくなりうる規模である。「失敗は継続する」という設計の
/// 前提（体験改善であり必須機能ではない）を守るため、ここだけ短い per-request timeout
/// （既定 [`DEFAULT_LINE_LOADING_TIMEOUT`]、`reqwest::RequestBuilder::timeout` はクライアント
/// 既定を上書きする）を明示的に掛ける。
async fn start_loading_indicator(state: &AppState, user_id: &str) {
    let payload = serde_json::json!({
        "chatId": user_id,
        "loadingSeconds": 60,
    });

    // `.json(&payload)` にリクエストボディのシリアライズと `content-type: application/json`
    // ヘッダの設定を任せる（Stage 1 レビュー指摘 Suggestion 1: 文字列・数値だけの `Value` の
    // シリアライズは失敗しえないため、旧実装の `to_vec` エラー分岐は到達不能なデッドコード
    // だった）。
    let response = match state
        .http
        .post(&state.line_loading_api_url)
        .timeout(state.line_loading_timeout)
        .header(
            "authorization",
            format!("Bearer {}", state.channel_access_token),
        )
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(
                user_id,
                error = ?err,
                "line webhook: chat loading api call failed (network/timeout); continuing \
                 without the loading indicator"
            );
            return;
        }
    };

    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            user_id,
            %status,
            "line webhook: chat loading api returned a non-success status; continuing without \
             the loading indicator"
        );
    }
}

/// `CS_ANSWER_API_URL` の形式検証（Stage 2 レビュー指摘、および一次レビュー指摘: 前方一致では
/// `http://127.0.0.1.evil.com` 等の外部ホストが素通りしていた）。
///
/// `require_env` は空白のみのチェックしかしないため、平文 `http://` の外部ホストのような
/// URL でも起動できてしまう。応答生成 API へは顧客の問い合わせ本文（`message` / `history`）
/// を送るため、経路が平文で外部に露出すると盗聴・改竄されうる。許可するのは:
///
/// - `https` scheme で任意ホスト（本番の Cloud Run URL）
/// - `http` scheme で host が `127.0.0.1` / `localhost` に**完全一致**（ポート有無問わず）
///
/// のいずれか。判定は `url::Url::parse` してから `host_str()` の完全一致で行う
/// （`server/src/oauth/authserver.rs` の `is_acceptable_redirect_uri` と同じ規律）。
/// **前方一致（`starts_with`）にしない。** それだと `http://127.0.0.1.evil.com` や
/// `http://localhost.attacker.example`、`http://localhost-evil.com` のような外部ホストが
/// 文字列の先頭だけ一致して素通りしてしまう。ローカル検証用の 2 ホストだけを平文 `http://`
/// の例外として許し、それ以外の `http://`（および `http`/`https` 以外の scheme、パース不能な
/// 値）は起動失敗させる（fail closed。設定ミスで平文外部送信のまま本番稼働に入るのを防ぐ）。
fn validate_answer_api_url(url: &str) -> Result<()> {
    let is_allowed = url::Url::parse(url).is_ok_and(|parsed| match parsed.scheme() {
        "https" => true,
        "http" => matches!(parsed.host_str(), Some("127.0.0.1" | "localhost")),
        _ => false,
    });
    if is_allowed {
        Ok(())
    } else {
        anyhow::bail!(
            "CS_ANSWER_API_URL must be a valid URL with scheme https (any host), or http with \
             host exactly 127.0.0.1 or localhost (got {url:?}); plaintext http:// to a \
             non-local host would send customer inquiry text unencrypted"
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
        // このクライアントが叩く先は応答生成 API と LINE Reply API の 2 つだけで、どちらも
        // redirect を返す正当な理由が無い。既定（最大 10 回追従）のままだと、
        // `validate_answer_api_url` が起動時に強制した `https://` 境界を、応答生成 API が
        // 307/308 で `http://` の外部ホストへ redirect するだけで迂回でき、307/308 は body を
        // 保持して再送するため顧客の問い合わせ本文（`message` / `history`）が平文で
        // 意図しないホストへ送られてしまう（Stage 2 レビュー指摘）。redirect を一切
        // 追従しないことで、起動時検証の境界を実行時にも維持する。
        .redirect(reqwest::redirect::Policy::none())
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
        line_loading_api_url: DEFAULT_LINE_LOADING_API_URL.to_string(),
        line_loading_timeout: DEFAULT_LINE_LOADING_TIMEOUT,
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
    // `store_update`（`lock_session` → `apply_customer_turn` → `apply_assistant_turn`）
    // ヘルパー経由で行い、読み取りは読み取り専用の `snapshot` を使う。

    /// 本番の `handle_event` と同じ経路（`lock_session` → `apply_customer_turn` →
    /// `apply_assistant_turn`）を経由するテスト用ヘルパー。
    async fn store_update(
        store: &SessionStore,
        user_id: &str,
        case_id: &str,
        customer_text: &str,
        assistant_text: &str,
    ) {
        let mut session = store.lock_session(user_id).await;
        apply_customer_turn(
            user_id,
            &mut session,
            case_id.to_string(),
            customer_text.to_string(),
        );
        apply_assistant_turn(user_id, &mut session, assistant_text.to_string());
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

    // ---- SessionStore の eviction/sweep 生存判定（Stage 2 レビュー指摘: `try_lock` では
    // 「`Arc` を取得したがまだロックしていない」window を処理中と認識できず、進行中の
    // handler を eviction/sweep 対象にしてしまう。`Arc::strong_count` ベースの判定へ
    // 置き換えたので、その window を決定論的に再現して検証する（sleep によるタイミング
    // 依存ではなく、Arc を保持し続けることで window そのものを固定する）。----

    #[tokio::test]
    async fn get_or_create_does_not_evict_a_user_whose_arc_is_held_but_not_yet_locked() {
        // 容量 1: 2 人目の get_or_create が必ず eviction を試みる状況を作る。
        let store = SessionStore::with_limits(Duration::from_secs(3600), 1);
        // u1 の Arc を取得するが、ロックはしない
        // （`get_or_create` を呼んでから `arc.lock_owned().await` するまでの window を模す）。
        let u1_arc = store.get_or_create("u1");

        // この間に別ユーザー u2 が上限到達で eviction を試みる。このテストが u1 の Arc を
        // まだ保持しているので strong_count > 1 となり、候補から除外されるはず
        // （`try_lock` ベースの旧実装では、ロック未取得のためここが破れていた）。
        let _u2_arc = store.get_or_create("u2");

        {
            let users = store.users.lock().unwrap();
            assert!(
                users.contains_key("u1"),
                "an entry whose Arc is held (even before its Mutex is locked) must not be \
                 evicted, or the caller holding it becomes an orphaned session"
            );
        }

        // 同一ユーザーに対して複数の mutex が作られていないこと（直列化保証そのもの）を
        // 検証する: 再度 get_or_create すれば最初に取得したのと同じ Arc が返るはず。
        let u1_arc_again = store.get_or_create("u1");
        assert!(
            Arc::ptr_eq(&u1_arc, &u1_arc_again),
            "a held-but-unlocked session must not be replaced by a new Arc/Mutex, or \
             concurrent handlers for the same user would race against different session \
             state"
        );
    }

    #[tokio::test]
    async fn sweep_keeps_ttl_expired_entries_whose_arc_is_held_but_not_locked() {
        let store = SessionStore::with_limits(Duration::from_millis(20), 10);
        store_update(&store, "u1", "case-1", "q", "a").await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Arc だけを保持し、ロックはしない（get_or_create 直後・ロック取得前の window を
        // 模す）。既存の `sweep_keeps_ttl_expired_entries_that_are_currently_locked` は
        // ロック保持版であり、これは未ロック・Arc 保持のみの別ケース。
        let _held = store.get_or_create("u1");

        store.sweep();

        let users = store.users.lock().unwrap();
        assert!(
            users.contains_key("u1"),
            "sweep must not remove an entry whose Arc is held even if its Mutex is not \
             locked and its TTL has elapsed, or an in-flight handler that has not yet \
             locked becomes orphaned"
        );
    }

    // ---- SessionStore::sweep（design doc §6: 定期スイープ側。一次レビュー指摘: 未検証） ----

    #[tokio::test]
    async fn sweep_removes_ttl_expired_entries_that_are_not_locked() {
        let store = SessionStore::with_limits(Duration::from_millis(20), 10);
        store_update(&store, "u1", "case-1", "q", "a").await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        store.sweep();

        let users = store.users.lock().unwrap();
        assert!(
            !users.contains_key("u1"),
            "sweep must remove an unlocked entry once its TTL has elapsed"
        );
    }

    #[tokio::test]
    async fn sweep_keeps_ttl_expired_entries_that_are_currently_locked() {
        let store = SessionStore::with_limits(Duration::from_millis(20), 10);
        store_update(&store, "u1", "case-1", "q", "a").await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        // 直接 Arc をロックして保持する（`lock_session` 経由だと TTL 超過時に `last_at` を
        // リセットしてしまい、「TTL 超過中にロック保持」という前提条件が崩れるため）。
        // 進行中リクエストが `session` を保持したまま await している状態を模す。
        let arc = store.get_or_create("u1");
        let _guard = arc.lock_owned().await;

        store.sweep();

        let users = store.users.lock().unwrap();
        assert!(
            users.contains_key("u1"),
            "sweep must not remove a currently-locked entry even past its TTL, so an \
             in-flight conversation is not dropped out from under the request handling it"
        );
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
            apply_customer_turn(
                "u1",
                &mut session,
                format!("case-{label}"),
                format!("q-{label}"),
            );
            apply_assistant_turn("u1", &mut session, format!("a-{label}"));
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

    // ---- handle_event（item 5: 応答生成 API の 200 で customer ターン+case_id を
    // 即保存し、LINE 返信成功後にだけ assistant ターンを追記する） ----
    //
    // `send_line_reply` は本番では `https://api.line.me/...` を叩くため、成功・失敗を
    // 直接注入する手段が無い。`AppStateInner::line_reply_api_url` をテスト専用に
    // ローカルのモック HTTP サーバへ差し替えることで、実際の `handle_event` の経路
    // （get→応答生成 API 呼び出し→(200 なら) case_id + customer ターン保存→LINE 返信→
    // (成功時のみ) assistant ターン追記）を実 HTTP 呼び出しで検証する（本番コードは
    // `line_reply_api_url` を [`DEFAULT_LINE_REPLY_API_URL`] 固定で使うため、この差し替えは
    // テストにしか効かない）。

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

    /// テスト専用: loading indicator と無関係な呼び出し元に渡す安全な既定値。到達不能な固定
    /// アドレスなので、`start_loading_indicator` が実際に呼ばれても接続エラーで warn する
    /// だけで済む（失敗しても継続するだけなので害はない）。
    const UNREACHABLE_LOADING_API_URL: &str = "http://127.0.0.1:1";

    fn test_app_state(
        answer_api_url: String,
        line_reply_api_url: String,
        line_loading_api_url: String,
    ) -> AppState {
        test_app_state_with_loading_timeout(
            answer_api_url,
            line_reply_api_url,
            line_loading_api_url,
            DEFAULT_LINE_LOADING_TIMEOUT,
        )
    }

    /// [`test_app_state`] に加え、chat loading API の per-request timeout も差し替えられる版。
    /// ストールした chat loading API が応答生成 API 呼び出しをブロックしないことを検証する
    /// テスト（Stage 1 レビュー指摘 Critical 1）だけがこちらを直接使う。
    fn test_app_state_with_loading_timeout(
        answer_api_url: String,
        line_reply_api_url: String,
        line_loading_api_url: String,
        line_loading_timeout: Duration,
    ) -> AppState {
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
            line_loading_api_url,
            line_loading_timeout,
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
            UNREACHABLE_LOADING_API_URL.to_string(),
        );
        let event = text_webhook_event("u1", "rt1", "こんにちは");

        handle_event(&state, &event).await.expect(
            "handle_event must succeed when both the answer api and the line reply succeed",
        );

        let (case_id, history) = state.sessions.snapshot("u1").await;
        assert_eq!(
            case_id,
            Some("case-abc".to_string()),
            "case_id from the answer api must be saved (it is saved as soon as the answer api \
             returns 200, and remains saved once the line reply also succeeds)"
        );
        assert_eq!(
            history.len(),
            2,
            "both the customer and assistant turns must be recorded once the line reply has \
             succeeded (the customer turn is saved immediately on the answer api's 200; the \
             assistant turn is appended only after the line reply succeeds)"
        );
    }

    #[tokio::test]
    async fn handle_event_saves_case_id_and_customer_turn_but_not_assistant_turn_when_the_line_reply_fails(
    ) {
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
            UNREACHABLE_LOADING_API_URL.to_string(),
        );
        let event = text_webhook_event("u1", "rt1", "こんにちは");

        let result = handle_event(&state, &event).await;
        assert!(
            result.is_err(),
            "handle_event must surface the line reply failure as an Err so webhook_handler logs \
             it (F2)"
        );

        let (case_id, history) = state.sessions.snapshot("u1").await;
        assert_eq!(
            case_id,
            Some("case-abc".to_string()),
            "the answer api returned 200, so case_id must be saved even though the line reply \
             failed (design doc §6 step 4: this save must not depend on the line reply's \
             outcome, or a subsequent message would start a new case and drop accumulated \
             signal)"
        );
        assert_eq!(
            history.len(),
            1,
            "only the customer turn must be recorded; the assistant turn is appended only after \
             a successful line reply, which did not happen here"
        );
        assert_eq!(
            history[0],
            (Role::Customer, "こんにちは".to_string()),
            "the recorded turn must be the customer's message, not the assistant's reply"
        );
    }

    /// 応答生成 API のモック: 常に 500 を返す（`api_response` が `None` になる経路を作る）。
    async fn answer_api_failing_handler() -> impl axum::response::IntoResponse {
        StatusCode::INTERNAL_SERVER_ERROR
    }

    #[tokio::test]
    async fn handle_event_leaves_the_session_untouched_when_the_answer_api_fails() {
        let answer_api_base =
            spawn_http_mock(Router::new().route("/reply", post(answer_api_failing_handler))).await;
        let line_ok_base =
            spawn_http_mock(Router::new().route("/reply", post(|| async { StatusCode::OK }))).await;

        let state = test_app_state(
            format!("{answer_api_base}/reply"),
            format!("{line_ok_base}/reply"),
            UNREACHABLE_LOADING_API_URL.to_string(),
        );
        let event = text_webhook_event("u1", "rt1", "こんにちは");

        let result = handle_event(&state, &event).await;
        assert!(
            result.is_ok(),
            "handle_event must succeed here: the answer api failed, so handle_event falls back \
             to the fallback text, and the line reply (which is mocked to succeed) is what \
             determines the Ok/Err of handle_event, not the answer api's status"
        );

        let (case_id, history) = state.sessions.snapshot("u1").await;
        assert_eq!(
            case_id, None,
            "the answer api did not return 200, so no case was confirmed server-side; saving \
             case_id here would let a later message use a case that never received this \
             signal (design doc §6 step 4)"
        );
        assert!(
            history.is_empty(),
            "neither the customer nor the assistant turn may be recorded when the answer api \
             fails: the answer api's 200 is what makes assemble_reply return Some(update) (and \
             thus a customer-turn save) in the first place, so a failure must leave the session \
             exactly as it was before this event"
        );
    }

    // ---- start_loading_indicator（design doc §6 手順3: chat loading API） ----

    /// [`loading_capture_handler`] が受け取ったリクエストのうち、テストが検証する部分だけを
    /// 抜き出したスナップショット。
    #[derive(Debug, Default, Clone)]
    struct CapturedLoadingRequest {
        calls: u32,
        chat_id: Option<String>,
        loading_seconds: Option<i64>,
        authorization: Option<String>,
        content_type: Option<String>,
    }

    /// chat loading API のモックハンドラ。呼び出し回数だけでなく、design doc §6 手順3 が定める
    /// 契約そのもの（`chatId` / `loadingSeconds` / `Authorization` / `Content-Type`）を検証する
    /// （Stage 1 レビュー指摘 Warning 1: 回数しか見ないモックだと、`chatId` の値が誤っている・
    /// `Authorization` が欠けている等の契約違反を検出できず、機能全体が本番で無反応のまま
    /// warn ログ以外に兆候が出ないサイレント never-works になりうる）。
    ///
    /// 中身の妥当性はハンドラ内で `panic!` させず、`Arc<Mutex<CapturedLoadingRequest>>` へ
    /// 記録するだけにとどめる。ハンドラは `spawn_http_mock` が起動する別の tokio task 上で
    /// 動くため、ここで panic してもテスト本体のアサーションとしては伝播しない
    /// （コネクションが切れて `start_loading_indicator` 側がネットワークエラーとして warn する
    /// だけになり、検証したい内容がテスト結果に反映されない）。呼び出し元がロック解放後に
    /// 値を assert する。
    async fn loading_capture_handler(
        State(captured): State<Arc<Mutex<CapturedLoadingRequest>>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        let parsed: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
        let mut captured = captured
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        captured.calls += 1;
        captured.chat_id = parsed
            .as_ref()
            .and_then(|v| v.get("chatId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        captured.loading_seconds = parsed
            .as_ref()
            .and_then(|v| v.get("loadingSeconds"))
            .and_then(|v| v.as_i64());
        captured.authorization = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        captured.content_type = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        StatusCode::OK
    }

    #[tokio::test]
    async fn handle_event_sends_the_chat_loading_api_request_the_contract_requires() {
        let answer_api_base =
            spawn_http_mock(Router::new().route("/reply", post(answer_api_ok_handler))).await;
        let line_ok_base =
            spawn_http_mock(Router::new().route("/reply", post(|| async { StatusCode::OK }))).await;
        let captured = Arc::new(Mutex::new(CapturedLoadingRequest::default()));
        let loading_router = Router::new()
            .route("/loading", post(loading_capture_handler))
            .with_state(captured.clone());
        let loading_base = spawn_http_mock(loading_router).await;

        let state = test_app_state(
            format!("{answer_api_base}/reply"),
            format!("{line_ok_base}/reply"),
            format!("{loading_base}/loading"),
        );
        let event = text_webhook_event("u1", "rt1", "こんにちは");

        handle_event(&state, &event).await.expect(
            "handle_event must succeed when the chat loading api, answer api, and line reply \
             all succeed",
        );

        let captured = captured.lock().unwrap().clone();
        assert_eq!(
            captured.calls, 1,
            "the chat loading api must receive exactly one request per text event (design doc \
             §6 step 3)"
        );
        assert_eq!(
            captured.chat_id,
            Some("u1".to_string()),
            "chatId must be the LINE user id from source.userId"
        );
        assert_eq!(
            captured.loading_seconds,
            Some(60),
            "loadingSeconds must be 60 (design doc §6 step 3)"
        );
        assert_eq!(
            captured.authorization,
            Some("Bearer test-channel-access-token".to_string()),
            "the channel access token must be sent as a Bearer token, same as send_line_reply"
        );
        assert_eq!(
            captured.content_type.as_deref(),
            Some("application/json"),
            "the request body must be sent as application/json"
        );
    }

    #[tokio::test]
    async fn handle_event_does_not_block_on_a_slow_chat_loading_api() {
        // Stage 1 レビュー指摘 Critical 1 の固定テスト: per-request timeout が無いと、
        // 50 秒のクライアント既定 timeout に落ちるまで応答生成 API 呼び出しがブロックされる。
        // per-request timeout を短く（200ms）差し替え、モックの応答をそれより長く（2 秒）
        // 遅延させることで、timeout が実際に効いていることをテスト自体は短時間で検証する。
        const TEST_LOADING_TIMEOUT: Duration = Duration::from_millis(200);
        const MOCK_DELAY: Duration = Duration::from_secs(2);

        let answer_api_base =
            spawn_http_mock(Router::new().route("/reply", post(answer_api_ok_handler))).await;
        let line_ok_base =
            spawn_http_mock(Router::new().route("/reply", post(|| async { StatusCode::OK }))).await;
        let slow_loading_base = spawn_http_mock(Router::new().route(
            "/loading",
            post(|| async move {
                tokio::time::sleep(MOCK_DELAY).await;
                StatusCode::OK
            }),
        ))
        .await;

        let state = test_app_state_with_loading_timeout(
            format!("{answer_api_base}/reply"),
            format!("{line_ok_base}/reply"),
            format!("{slow_loading_base}/loading"),
            TEST_LOADING_TIMEOUT,
        );
        let event = text_webhook_event("u1", "rt1", "こんにちは");

        let started = Instant::now();
        let result = handle_event(&state, &event).await;
        let elapsed = started.elapsed();

        assert!(
            result.is_ok(),
            "handle_event must still succeed: a stalled chat loading api must not fail the \
             overall event handling (design doc §6 step 3, warn-and-continue)"
        );
        // Stage 2 レビュー指摘 Suggestion 3: 上限を MOCK_DELAY（2秒）ではなく設定値
        // （per-request timeout 200ms）に近い 1 秒へ締める。2 秒のままだと timeout が誤って
        // 1〜1.5 秒に延びる回帰が起きてもこのテストは緑のままになり、固定したい境界（200ms
        // で切れること）を検証できていなかった。ロック取得（Warning 4 でこのテストより前に
        // 移動）はマイクロ秒オーダーなので、この上限には実質影響しない。
        assert!(
            elapsed < Duration::from_secs(1),
            "handle_event must return well within 1s: the per-request timeout on the chat \
             loading api call is {TEST_LOADING_TIMEOUT:?}, so a stalled loading api must cut \
             the call short long before the mock's {MOCK_DELAY:?} delay or the http client's \
             50s default. elapsed={elapsed:?}"
        );
    }

    /// 応答生成 API のモック: リクエスト body の `message` を到着順に記録してから、通常の
    /// 200 応答を返す。`handle_event_serializes_the_same_users_concurrent_events...` が
    /// 「先行イベントの loading API が遅くても、応答生成 API への到達順は入れ替わらない」
    /// ことを検証するために使う。
    async fn answer_order_recording_handler(
        State(order): State<Arc<Mutex<Vec<String>>>>,
        body: Bytes,
    ) -> axum::Json<serde_json::Value> {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
            if let Some(message) = value.get("message").and_then(|v| v.as_str()) {
                order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(message.to_string());
            }
        }
        axum::Json(serde_json::json!({
            "reply_text": "こちらが回答です",
            "case_id": "case-abc",
        }))
    }

    /// [`loading_lock_observation_handler`] の router state。`state_cell` はテスト本体が
    /// 構築した `AppState` を後から流し込むための入れ物（`test_app_state` が要求する
    /// `line_loading_api_url` を先に確定させる必要があり、loading モックの router を先に
    /// 起動してから `AppState` を作る、という構築順の都合上こうなっている）。`observed` は
    /// ハンドラが観測した「呼び出し時点でロック済みだったか」を書き戻す先。
    type LoadingLockObservationState = (Arc<Mutex<Option<AppState>>>, Arc<Mutex<Option<bool>>>);

    /// chat loading API のモック: 呼び出された瞬間に、対象ユーザー（`"u1"` 固定）のセッション
    /// mutex が**既にロックされているか**を `try_lock()` で直接観測し、`observed` へ書き込む。
    ///
    /// **Stage 2 codex レビュー round 3 Warning 6(a) の決定論的な主テストが使うハンドラ**。
    /// 並行イベントを 2 つ走らせて到達順を見る（下の
    /// `handle_event_serializes_the_same_users_concurrent_events_despite_a_slow_first_loading_call`）
    /// のとは異なり、**単一イベント**の処理中に「loading API を呼んでいる時点でロックが
    /// held かどうか」を直接見るため、他タスクのスケジューリングにも時間計測にも依存しない。
    ///
    /// `SessionStore::get_or_create` は既存キーがあれば同じ `Arc<TokioMutex<Session>>` を
    /// 返す（`server/src/bin/line_adapter.rs` の実装参照）。`handle_event` は
    /// `start_loading_indicator` を呼ぶ**前**に `lock_session`（内部で `get_or_create` を
    /// 呼ぶ）を完了させているため、正しい実装ではここで呼ぶ `get_or_create("u1")` は
    /// `handle_event` が保持しているのと同じ `Arc` を返し、その `try_lock()` は必ず失敗
    /// する（`is_err() == true`）。ロック取得前に loading API を呼ぶ回帰が起きた場合は、
    /// `"u1"` のエントリがまだ存在せず `get_or_create` が新規作成した未ロックの `Arc` を
    /// 返すため、`try_lock()` は成功し（`is_err() == false`）、テストは決定論的に赤くなる。
    async fn loading_lock_observation_handler(
        State((state_cell, observed)): State<LoadingLockObservationState>,
    ) -> StatusCode {
        let app_state = state_cell
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .expect(
                "the test must populate the AppState cell before calling handle_event, or this \
                 handler cannot see the same SessionStore instance handle_event is using",
            );
        let session_arc = app_state.sessions.get_or_create("u1");
        let is_locked = session_arc.try_lock().is_err();
        *observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(is_locked);
        StatusCode::OK
    }

    /// Stage 2 codex レビュー round 3 Warning 6(a): Warning 4 の固定を、他タスクの
    /// スケジューリングや時間計測に依存せず決定論的に検証する主テスト。
    ///
    /// 並行イベントは使わない。単一のテキストイベントを処理させ、その最中に chat loading
    /// API が呼ばれた瞬間、対象ユーザーのセッション mutex が既にロックされていることだけを
    /// 直接観測する（[`loading_lock_observation_handler`] 参照）。これが緑である限り、
    /// 「loading API 呼び出しはロック取得後」という Warning 4 の不変条件は sleep や
    /// タスクの追い越しに関係なく保証されている。
    ///
    /// 到達順（A→B）を実際に確認する
    /// `handle_event_serializes_the_same_users_concurrent_events_despite_a_slow_first_loading_call`
    /// は、この不変条件が実際に「並行イベントの順序保存」という観測可能な効果につながって
    /// いることを示す end-to-end の補助テストという位置づけに変える（決定論的な回帰検出は
    /// このテストが担う）。
    #[tokio::test]
    async fn handle_event_holds_the_session_lock_while_calling_the_chat_loading_api() {
        let answer_api_base =
            spawn_http_mock(Router::new().route("/reply", post(answer_api_ok_handler))).await;
        let line_ok_base =
            spawn_http_mock(Router::new().route("/reply", post(|| async { StatusCode::OK }))).await;

        let state_cell: Arc<Mutex<Option<AppState>>> = Arc::new(Mutex::new(None));
        let observed: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let loading_router = Router::new()
            .route("/loading", post(loading_lock_observation_handler))
            .with_state((state_cell.clone(), observed.clone()));
        let loading_base = spawn_http_mock(loading_router).await;

        let state = test_app_state(
            format!("{answer_api_base}/reply"),
            format!("{line_ok_base}/reply"),
            format!("{loading_base}/loading"),
        );
        // handle_event を呼ぶ前に、loading モックが参照する AppState を確定させる（この代入は
        // handle_event の呼び出しより前の同期コードなので、ハンドラが実行される時点では必ず
        // Some になっている）。
        *state_cell
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state.clone());

        let event = text_webhook_event("u1", "rt1", "こんにちは");
        handle_event(&state, &event)
            .await
            .expect("handle_event must succeed");

        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            Some(true),
            "the session mutex for the event's user must already be locked at the moment the \
             chat loading api is called; if this is Some(false), the loading call has \
             regressed to before lock acquisition (Warning 4), and if this is None, the loading \
             api was never called at all"
        );
    }

    /// chat loading API のモックが 1 回目の呼び出しだけブロックするためのハンドシェイク。
    /// `tokio::sync::Notify` を 2 本使い、sleep の長さではなく実際のイベントでテスト本体と
    /// モックハンドラの間を同期する（Stage 2 codex レビュー round 2 Warning 5: 「A を spawn
    /// してから 50ms sleep すれば A が先にロックを取得しているはず」という以前の実装は、
    /// sleep が「A がロックを取得した」ことも「A が loading handler に到達した」ことも保証
    /// しないため、CI 高負荷で A のスケジュールが 50ms 以上遅れると、正しい実装でも B が
    /// 先にロックを取ってテストが偽陽性で落ちうる欠陥があった）。
    #[derive(Default)]
    struct LoadingHandshake {
        call_count: Mutex<u32>,
        /// 1 回目の呼び出しが handler の中に入った（＝ chat loading API 呼び出し中、修正後の
        /// 実装ではこの時点でロックを保持中）ことをテスト本体へ知らせる。
        reached: tokio::sync::Notify,
        /// テスト本体が 1 回目の呼び出しを解放してよいと伝える。
        release: tokio::sync::Notify,
    }

    /// chat loading API のモック: **1 回目の呼び出しだけ** `reached.notify_one()` を呼んでから
    /// `release.notified()` で待つ。2 回目以降は即応答する。同一 `chatId`（同一ユーザー）の
    /// 連続呼び出しは body だけでは区別できないため、呼び出し順（＝先着イベントが先に loading
    /// API を叩く）で区別する。`Notify::notify_one()` は permit を 1 つ蓄えるため、テスト本体の
    /// `notified()` 登録より先に呼ばれても取りこぼさない（lost wakeup 対策）。
    async fn loading_handshake_handler(
        State(handshake): State<Arc<LoadingHandshake>>,
    ) -> StatusCode {
        let is_first_call = {
            let mut count = handshake
                .call_count
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let is_first_call = *count == 0;
            *count += 1;
            is_first_call
        };
        if is_first_call {
            handshake.reached.notify_one();
            handshake.release.notified().await;
        }
        StatusCode::OK
    }

    /// end-to-end の補助テスト。決定論的な回帰検出は
    /// `handle_event_holds_the_session_lock_while_calling_the_chat_loading_api`（Warning
    /// 6(a)）が担う。このテストは、そのロック保持という不変条件が実際に「同一ユーザーの
    /// 並行イベントの到達順が保存される」という観測可能な効果につながっていることを示す
    /// （round 2 の Warning 5 で sleep 依存の一部を排除したが、round 3 の Warning 6(b) で
    /// 残る 2 点をさらに直した: (1) 旧実装の検出はなお 50ms sleep に依存しており「必ず」で
    /// はなく「高確率で」しか言えない、(2) `reached`/`join!` に期限が無いとロード呼び出し
    /// 自体が消える等の別種の回帰で無期限にハングしていた。両方とも `tokio::time::timeout`
    /// で期限を付け、期限切れは明示的な `panic`（テスト失敗）にした）。
    ///
    /// 修正前は `start_loading_indicator` がセッションロック取得より前にあったため、同一
    /// ユーザーからの並行イベントの処理順が loading API の応答速度だけで決まっていた。ここでは
    /// 先行イベント A の loading API 呼び出しを [`LoadingHandshake`] で意図的にブロックし、
    /// 後着イベント B の loading API は即応答にする。修正前の実装では B が A を追い越して先に
    /// 応答生成 API へ到達しうる（実際に踏んだ回帰）。修正後（ロック取得を loading API 呼び
    /// 出しより前に置く）では、A がロックを保持したまま loading → 応答生成 API → LINE 返信まで
    /// 一通り終えるまで B は `lock_session` から先に進めないため、到達順は A → B になる。
    ///
    /// `handshake.reached.notified().await` を通過した時点で、A は loading handler の中
    /// （＝修正後の実装ではロックを保持中）にいることが確定するため、「B を spawn するタイミ
    /// ング」は sleep で仮定していない。**ただし B を spawn した後に残した 50ms sleep は、
    /// 旧実装（ロック取得前に loading を呼ぶ）を「高確率で」赤くするためのものであり、CI が
    /// 高負荷でこの 50ms の間に B がまだ応答生成 API へ到達していなければ、旧実装でも緑に
    /// なりうる（＝この sleep は旧実装の検出を保証しない）。旧実装を決定論的に検出したい
    /// 場合は上記の Warning 6(a) テストを見ること。** 正しい実装に対しては、この sleep の
    /// 長短は結果に影響しない（B は `lock_session` で必ずブロックされるため）。
    #[tokio::test]
    async fn handle_event_serializes_the_same_users_concurrent_events_despite_a_slow_first_loading_call(
    ) {
        /// `reached`/`join!` それぞれに掛ける上限。発火しない・完了しない回帰（loading 呼び
        /// 出し自体が消える、モック接続に失敗する、タスクが panic する等）を、無期限ハングでは
        /// なく明示的なテスト失敗にするための期限（Warning 6(b)）。
        const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

        let answer_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let answer_router = Router::new()
            .route("/reply", post(answer_order_recording_handler))
            .with_state(answer_order.clone());
        let answer_api_base = spawn_http_mock(answer_router).await;

        let line_ok_base =
            spawn_http_mock(Router::new().route("/reply", post(|| async { StatusCode::OK }))).await;

        let handshake = Arc::new(LoadingHandshake::default());
        let loading_router = Router::new()
            .route("/loading", post(loading_handshake_handler))
            .with_state(handshake.clone());
        let loading_base = spawn_http_mock(loading_router).await;

        let state = test_app_state(
            format!("{answer_api_base}/reply"),
            format!("{line_ok_base}/reply"),
            format!("{loading_base}/loading"),
        );

        let state_a = state.clone();
        let task_a = tokio::spawn(async move {
            let event = text_webhook_event("u1", "rt-a", "Aの本文");
            handle_event(&state_a, &event).await
        });

        // A が chat loading API 呼び出しの中（修正後の実装ではロックを保持中）に入るまで待つ。
        // sleep の長さに依存しないイベント同期（Warning 5）。期限切れはハングではなく失敗に
        // する（Warning 6(b)）。
        tokio::time::timeout(WAIT_TIMEOUT, handshake.reached.notified())
            .await
            .expect(
                "handshake.reached did not fire within the timeout: the chat loading api call \
                 itself may have disappeared from handle_event's text-event branch (a \
                 different kind of regression than Warning 4)",
            );

        let state_b = state.clone();
        let task_b = tokio::spawn(async move {
            let event = text_webhook_event("u1", "rt-b", "Bの本文");
            handle_event(&state_b, &event).await
        });
        // 旧実装を「高確率で」赤くするための猶予（このテストの doc comment 参照。正しい実装
        // に対する緑判定はこの sleep の長短に依存しない。旧実装の決定論的な検出は保証しない）。
        tokio::time::sleep(Duration::from_millis(50)).await;

        handshake.release.notify_one();

        let (result_a, result_b) = tokio::time::timeout(WAIT_TIMEOUT, async {
            tokio::join!(task_a, task_b)
        })
        .await
        .expect(
            "task A/B did not complete within the timeout: a hang here likely means the chat \
             loading api call is blocking somewhere it should not (e.g. the per-request \
             timeout stopped applying)",
        );
        result_a
            .expect("task A must not panic")
            .expect("handle_event(A) must succeed");
        result_b
            .expect("task B must not panic")
            .expect("handle_event(B) must succeed");

        let order = answer_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            order,
            vec!["Aの本文".to_string(), "Bの本文".to_string()],
            "event A must reach the answer api before event B even though A's chat loading api \
             call is slow: the same-user lock must serialize the whole per-event pipeline \
             (including the loading indicator), not just the session read/write part"
        );
    }

    #[tokio::test]
    async fn handle_event_continues_when_the_chat_loading_api_fails() {
        let answer_api_base =
            spawn_http_mock(Router::new().route("/reply", post(answer_api_ok_handler))).await;
        let line_ok_base =
            spawn_http_mock(Router::new().route("/reply", post(|| async { StatusCode::OK }))).await;
        let loading_fail_base = spawn_http_mock(Router::new().route(
            "/loading",
            post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        ))
        .await;

        let state = test_app_state(
            format!("{answer_api_base}/reply"),
            format!("{line_ok_base}/reply"),
            format!("{loading_fail_base}/loading"),
        );
        let event = text_webhook_event("u1", "rt1", "こんにちは");

        let result = handle_event(&state, &event).await;
        assert!(
            result.is_ok(),
            "a failing chat loading api must not interrupt the rest of handle_event: it is a UX \
             nicety, not a required step (design doc §6 step 3: 'この呼び出しの失敗は warn ログ \
             のみで処理を継続する')"
        );

        let (case_id, _history) = state.sessions.snapshot("u1").await;
        assert_eq!(
            case_id,
            Some("case-abc".to_string()),
            "the answer api call, session save, and line reply must all complete normally even \
             though the chat loading api returned 500"
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

    /// 一次レビュー指摘: 前方一致だと `http://127.0.0.1` で始まる別ホストが素通りする。
    /// `host_str()` の完全一致に退行していないかを検出する回帰テスト。
    #[test]
    fn validate_answer_api_url_rejects_a_subdomain_prefixed_with_127_0_0_1() {
        assert!(validate_answer_api_url("http://127.0.0.1.evil.com/reply").is_err());
    }

    /// 一次レビュー指摘: 前方一致だと `http://localhost` で始まる別ホストが素通りする。
    #[test]
    fn validate_answer_api_url_rejects_a_subdomain_prefixed_with_localhost() {
        assert!(validate_answer_api_url("http://localhost.attacker.example/reply").is_err());
    }

    /// 一次レビュー指摘: 前方一致だと `localhost` に文字列が続くだけの別ホストが素通りする。
    #[test]
    fn validate_answer_api_url_rejects_a_hyphenated_localhost_lookalike_host() {
        assert!(validate_answer_api_url("http://localhost-evil.com/reply").is_err());
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
