//! 応答生成 API（`POST /{project_id}/api/reply`）。
//!
//! 仕様の正本は `docs/superpowers/specs/2026-08-11-answer-api-line-adapter-design.md`
//! （以下「design doc」）。既存 MCP インターフェースは変更しない。両者は同じ
//! `Harness::evaluate()`（rmcp 非依存の domain 入口）を共有する薄いアダプタである。
//!
//! 認証は Google OAuth（`oauth::middleware::require_google_auth`）ではなく、
//! env `CS_SUPPORT_ANSWER_API_KEY`（Secret Manager 注入）との定数時間比較。
//! `config.api.enabled`（既定 false）が false のときはこのモジュールのルートを
//! main.rs 側で登録しない（design doc §3）。

use crate::config::AppConfig;
use crate::harness::decision::{self, AnswerDecision};
use crate::harness::reply::{ReplyHistoryRole, ReplyHistoryTurn};
use crate::harness::{clarify, escalation_reply, hours, time_pref, Harness};
use crate::mcp::ToolService;
use crate::oauth::VerifiedIdentity;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// リクエストボディ（design doc §2）。
#[derive(Debug, Clone, Deserialize)]
pub struct ReplyRequest {
    pub message: String,
    #[serde(default)]
    pub history: Option<Vec<HistoryEntry>>,
    #[serde(default)]
    pub case_id: Option<String>,
}

/// `history` 1 要素。時系列昇順（古い→新しい）で渡される想定（design doc §2）。
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryEntry {
    pub role: HistoryRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRole {
    Customer,
    Assistant,
}

/// レスポンスボディ（200 のみ。design doc §2）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReplyResponse {
    pub reply_text: String,
    pub case_id: String,
}

/// エラーレスポンスボディ（design doc §2）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ErrorBody {
    pub error: String,
    pub message: String,
}

/// `message` の trim 後の最小・最大文字数（design doc §2）。
pub const MIN_MESSAGE_CHARS: usize = 1;
pub const MAX_MESSAGE_CHARS: usize = 5_000;
/// `history` の最大要素数（design doc §2）。
pub const MAX_HISTORY_ENTRIES: usize = 20;
/// `history[].text` の trim 後の最小・最大文字数（design doc §2）。
pub const MIN_HISTORY_TEXT_CHARS: usize = 1;
pub const MAX_HISTORY_TEXT_CHARS: usize = 2_000;
/// `case_id` の最大文字数（design doc §2）。
pub const MAX_CASE_ID_CHARS: usize = 128;

/// リクエスト検証（design doc §2 の制約表）。
///
/// 文字数はすべて `chars().count()`（`reply.rs` と同じ規律。UTF-8 バイト数ではなく、
/// 日本語の文字数として数える）。エラーメッセージは 400 レスポンスの `message` に
/// そのまま使う想定で、どのフィールドの何が違反したかを含める。
pub fn validate(req: &ReplyRequest) -> Result<(), String> {
    let message_chars = req.message.trim().chars().count();
    if message_chars < MIN_MESSAGE_CHARS {
        return Err("message must not be empty (after trimming whitespace)".to_string());
    }
    if message_chars > MAX_MESSAGE_CHARS {
        return Err(format!(
            "message must be at most {MAX_MESSAGE_CHARS} characters after trimming, got {message_chars}"
        ));
    }

    if let Some(history) = &req.history {
        if history.len() > MAX_HISTORY_ENTRIES {
            return Err(format!(
                "history must have at most {MAX_HISTORY_ENTRIES} entries, got {}",
                history.len()
            ));
        }
        for (i, entry) in history.iter().enumerate() {
            let chars = entry.text.trim().chars().count();
            if chars < MIN_HISTORY_TEXT_CHARS {
                return Err(format!(
                    "history[{i}].text must not be empty (after trimming whitespace)"
                ));
            }
            if chars > MAX_HISTORY_TEXT_CHARS {
                return Err(format!(
                    "history[{i}].text must be at most {MAX_HISTORY_TEXT_CHARS} characters \
                     after trimming, got {chars}"
                ));
            }
        }
    }

    if let Some(case_id) = &req.case_id {
        let chars = case_id.chars().count();
        if chars > MAX_CASE_ID_CHARS {
            return Err(format!(
                "case_id must be at most {MAX_CASE_ID_CHARS} characters, got {chars}"
            ));
        }
    }

    Ok(())
}

/// タイミング攻撃で API キーを漏らさないための定数時間比較。
///
/// 通常の `==` は不一致バイトを見つけた時点で早期リターンするため、比較にかかる時間から
/// 鍵の先頭一致長を推測されうる（タイミングサイドチャネル）。ここでは長さが一致する場合、
/// 途中に不一致バイトがあっても早期リターンせず、全バイトを XOR して OR で畳み込み、
/// 最後にまとめて 0 判定する。**ただし長さ不一致は早期リターンする**（`a.len() != b.len()`
/// で即 `false`）。したがってこの関数単体では長さそのものは秘匿されない（比較にかかる時間から
/// 長さの違いは判別しうる）。`authorize` はこの性質を踏まえ、比較の前に両辺を SHA-256 で
/// ハッシュしてから渡す。ハッシュ後は常に 32 バイト同士の比較になるため、長さ不一致という
/// 早期リターン可能な情報そのものが比較の手前で消え、上記の長さ秘匿の弱さは経路上で問題に
/// ならない。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization` ヘッダから Bearer トークンを取り出す。
///
/// `oauth::middleware::parse_bearer` と同じ規律（RFC 9110: auth-scheme は
/// case-insensitive、スキームとトークンの間は space/tab、トークン末尾の OWS も trim）。
/// あちらは Google JWT 検証専用の private 関数で、こちらは固定 API キー比較専用の
/// ロジックであり、認証方式ごと（JWT 検証 vs 定数時間文字列比較）に呼び出し元の
/// 責務が異なるため、モジュールをまたいだ共有はせず同じ小さな規律だけを複製する。
fn extract_bearer_token(header_value: &str) -> Option<&str> {
    let idx = header_value.find([' ', '\t'])?;
    let (scheme, rest) = header_value.split_at(idx);
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(rest.trim_matches([' ', '\t']))
}

/// 両辺を SHA-256 でハッシュしてから `constant_time_eq` で比較する。
///
/// **比較されるのは常に 32 バイト（SHA-256 の出力長）同士である。** `a` / `b` の元の長さが
/// どれだけ異なっていても、ハッシュ後は必ず同じ長さになるため、`constant_time_eq` の
/// 早期 return 分岐（`a.len() != b.len()` で即 `false`）は実質的に踏まれず、比較にかかる
/// 時間から元の入力の長さの一致・不一致が漏れない（タイミングサイドチャネル対策）。
///
/// `authorize` から切り出した小さな関数（Stage 2 レビュー指摘: 切り出す前は、`authorize`
/// が実際にハッシュ化しているかどうかをテストが呼び出し結果からしか確認できず、テストが
/// 自前で SHA-256 を計算して `constant_time_eq` に渡すだけでは production の経路を
/// 検証したことにならなかった）。
fn hashed_constant_time_eq(a: &str, b: &str) -> bool {
    let a_hash = Sha256::digest(a.as_bytes());
    let b_hash = Sha256::digest(b.as_bytes());
    constant_time_eq(&a_hash, &b_hash)
}

/// `Authorization: Bearer <key>` を `api_key` と定数時間比較する。
///
/// ヘッダ欠落・スキーム不一致・トークン不一致はすべて `false`（呼び出し側は
/// 401 `{"error":"unauthorized",...}` を返す。design doc §3）。
///
/// `api_key` が空文字の場合は常に `false`（`""` と `""` の一致でトークン無しの
/// リクエストまで通さない）。main.rs の起動時 fail-closed チェック（`[api] enabled = true`
/// かつ env 未設定・空文字は起動失敗）により通常は空文字が渡ることはないが、この関数
/// 単体の契約としても「空鍵は誰も認証されない」を保証する（設定不備の実質無認証化を
/// 多重に防ぐ。`config.rs::read_secret_file` と同じ規律）。
///
/// 実際の比較は [`hashed_constant_time_eq`] に委譲する（Stage 2 レビュー指摘: 比較の直前に
/// 両辺を SHA-256 でハッシュすることで、`presented` / `api_key` の生の長さの違いに由来する
/// タイミングサイドチャネルを消す）。
pub fn authorize(headers: &HeaderMap, api_key: &str) -> bool {
    if api_key.is_empty() {
        return false;
    }
    let Some(presented) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(extract_bearer_token)
    else {
        return false;
    };
    hashed_constant_time_eq(presented, api_key)
}

/// `/api/reply` ルートが共有する状態。
///
/// `tools` / `harness` は main.rs が `/{project_id}/mcp` を組み立てるのと同じ共有物
/// （`ToolService` は内部 `Arc<VegapunkClient>` を持つので `Clone` は安価）。
#[derive(Clone)]
pub struct ApiState {
    pub config: Arc<AppConfig>,
    pub harness: Arc<Harness>,
    pub tools: ToolService,
    /// env `CS_SUPPORT_ANSWER_API_KEY` の値。ログ・Debug 出力に絶対に含めないため、
    /// この構造体は `Debug` を derive しない。
    pub api_key: String,
}

/// `/api/reply` が返す応答の種別（会話フロー v1.1 design doc §2 の決定表）。
///
/// 時間帯受付（design doc §5）は `evaluate()` に到達する前にハンドラ側で解決済みのため、
/// この enum の関心事ではない（4値目の `TimePrefIntake` 相当は存在しない。理由は
/// `reply_handler` の doc コメント参照）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyAction {
    /// `customer_reply_draft` をそのまま顧客へ返す。
    Answer(String),
    /// 聞き返し（ヒアリングループ）を送る。
    Clarify,
    /// 文脈化されたエスカレーション応答を送る。
    EscalationReply,
}

/// `awaiting_time_pref` 中の希望時間帯抽出インフラ失敗（LLM 呼び出しエラー・parse 失敗・
/// `reply_drafter` 未設定）を許容する連続回数。3 回連続で `awaiting_time_pref` を自動解除する
/// （design doc §5）。
const TIME_PREF_EXTRACTION_ERROR_LIMIT: u32 = 3;

/// 希望時間帯抽出のインフラ失敗（LLM 呼び出しエラー・parse 失敗・reply_drafter 未設定）を
/// 1 回分記録する純関数。design doc §5: 3 回連続で `awaiting_time_pref` を自動解除する
/// （`time_pref_false_count` の 2 回連続解除とは別カウンタ）。
///
/// `CaseConvState::time_pref_extraction_error_count` の doc コメント（`harness/mod.rs`）が
/// 明記するとおり、この判定は `Harness` ではなくオーケストレーション層（`api.rs`）の責務。
fn note_time_pref_extraction_failure(conv: &mut crate::harness::CaseConvState) {
    conv.time_pref_extraction_error_count += 1;
    if conv.time_pref_extraction_error_count >= TIME_PREF_EXTRACTION_ERROR_LIMIT {
        conv.awaiting_time_pref = false;
        conv.time_pref_false_count = 0;
        conv.time_pref_extraction_error_count = 0;
    }
}

/// 新しいエスカレーション応答（design doc §4）を送るときに希望時間帯の伺いを立てる純関数。
///
/// design doc §5: 「新しいエスカレーション応答（第 4 節）を送るときは
/// `awaiting_time_pref = true` を上書きセットし、`time_pref_false_count` を 0 に戻す」。
/// `clarify_turns` は聞き返しループとは別の会話段階へ移るため 0 に戻す。
///
/// **`time_pref_extraction_error_count` はここでは変更しない。** design doc §5 が
/// このカウンタのリセット条件として列挙しているのは「分類が成功した場合」
/// （`note_time_pref_extraction_failure` は関知しない）と「3 回到達で自動解除する瞬間」
/// （`note_time_pref_extraction_failure` 内で完結）の 2 つだけで、新しいエスカレーション応答の
/// 送信はそのどちらでもない。ここでリセットすると、`awaiting_time_pref = true` の間に抽出
/// インフラが継続的に失敗しているケースで「失敗 → EscalationReply → 0 に巻き戻る」を繰り返し、
/// 3 回連続到達による自動解除が永久に到達不能になる（他の 3 箇所と揃えて書き忘れに見えても、
/// 意図的に外している）。
fn arm_time_pref_solicitation(conv: &mut crate::harness::CaseConvState) {
    conv.awaiting_time_pref = true;
    conv.time_pref_false_count = 0;
    conv.clarify_turns = 0;
}

/// `evaluate` の結果と会話状態から応答の種別を決める純関数（design doc §2 の決定表そのもの）。
///
/// - `Allowed` かつ下書きあり かつ **非 truncated** → `Answer`
/// - `Escalate` かつ `clarification_allowed` かつ `conv.clarify_turns < cfg.clarify_max_turns`
///   → `Clarify`
/// - それ以外すべて（`Escalate` の残り全部 / `Allowed` で下書き無しか truncated） →
///   `EscalationReply`
///
/// truncated な下書きを `Answer` に使わない理由（`llm.rs` の `ReplyDraft` doc コメント参照）:
/// 生成上限で途中切断された下書きは、**切れ目がたまたま「。」の直後に落ちると完成文に
/// 見える**。日本語のビジネス文は結び・注意書きが末尾に来るため、見た目は完成しているのに
/// 末尾の安全上の但し書きだけが落ちた下書きが成立しうる。MCP 経路（`rmcp_server.rs`）は
/// 人間の CS 担当が下書きを検分してから送るため `truncated=true` を返して警告するだけで
/// 足りるが、`/api/reply` は**人間の検分が一切入らない自動送信経路**であり、同じ扱いにはできない。
///
/// `Escalate` のとき下書きが存在していても無視する（judge が escalate と決めた以上、その判定を
/// 下書きの中身で覆してはならない。下書きは判定を知らずに生成されているため、escalate 判定時に
/// たまたま非空の下書きが残っていても顧客へは出さない）。
pub fn decide_reply_action(
    outcome: &crate::harness::EvaluationOutcome,
    conv: &crate::harness::CaseConvState,
    cfg: &crate::config::ApiConfig,
) -> ReplyAction {
    match &outcome.decision {
        AnswerDecision::Allowed { .. } => match &outcome.customer_reply_draft {
            Some(draft) if !outcome.customer_reply_draft_truncated => {
                ReplyAction::Answer(draft.clone())
            }
            _ => ReplyAction::EscalationReply,
        },
        AnswerDecision::Escalate { .. } => {
            if outcome.clarification_allowed && conv.clarify_turns < cfg.clarify_max_turns {
                ReplyAction::Clarify
            } else {
                ReplyAction::EscalationReply
            }
        }
    }
}

/// 「初回か継続か」をサーバがコードで判定する純関数（会話フロー v1.1 design doc §3）。
///
/// 判定は決定論（`history` が非空、または `case_id` が渡された場合は継続）。文面の出し分けは
/// `clarify.rs` / `escalation_reply.rs` / `reply.rs`（顧客向け回答下書き）側が
/// `is_continuation: bool` を受け取って行うだけで、判定ロジックそのものはこの関数以外に
/// 持たせない。
fn is_continuation(history: &[ReplyHistoryTurn], case_id: Option<&str>) -> bool {
    !history.is_empty() || case_id.is_some()
}

/// `AnswerDecision::Escalate.missing` を `clarify::build_clarify_prompt` の第2引数
/// （不足情報）向けの人間可読テキストへ変換する。このテキストは LLM への入力にのみ使い、
/// 顧客へは出さない（design doc §3: 検索ヒットの title・本文は入力に含めない制約とは別枠。
/// missing は内部スコアの数値であり、egress gate（顧客向け出力の NG 表現検出）はプロンプト
/// 入力までは見ないため、この関数はあくまで LLM 入力向けの体裁を整えるだけで、漏洩を防ぐ
/// 仕組みではない）。
fn missing_to_text(missing: &[decision::EvidenceRequirement]) -> String {
    if missing.is_empty() {
        return "情報が不足しており、現在の内容では回答の根拠が十分ではありません。".to_string();
    }
    missing
        .iter()
        .map(|m| match m {
            decision::EvidenceRequirement::DirectManualCoverage { required, best } => format!(
                "マニュアルとの一致度が必要水準に届いていません（必要: {required:.2} 以上、現在: {best:.2}）"
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `Harness::evaluate` が返す `anyhow::Error` を HTTP ステータスへ分類する純関数
/// （design doc §2 のエラー表: 503 `upstream_unavailable` / 500 `internal`）。
///
/// vegapunk（gRPC）不達由来のエラーは `tonic::Status` または `tonic::transport::Error` として
/// 現れる。`evaluate` の内部は `?` と `.context(...)` を重ねて呼び出し元まで伝播するため、
/// 直接の原因ではなく `anyhow::Error` の context chain のどこかに埋まっている場合がある。
/// `anyhow::Error::downcast_ref` は context chain を辿って探すため（`vegapunk.rs` の
/// `err.downcast_ref::<tonic::Status>()` と同じ規律）、素の `Error::source()` chain を
/// 手で辿る必要はない。
pub fn classify_evaluate_error(err: &anyhow::Error) -> (StatusCode, &'static str) {
    let is_upstream = err.downcast_ref::<tonic::Status>().is_some()
        || err.downcast_ref::<tonic::transport::Error>().is_some();
    if is_upstream {
        (StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable")
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, "internal")
    }
}

/// `/api/reply` が `harness.begin()` に渡す固定のサービス principal（design doc §3）。
///
/// このルートは Google OAuth を経由しない（`CS_SUPPORT_ANSWER_API_KEY` の定数時間比較の
/// み）。したがって個々の呼び出し元（LINE 利用者等）を Google identity として突合する材料が
/// 無く、監査 actor は「answer-api 経由の呼び出しである」ことだけを表す固定値にする。
/// `sub` は `authn::Authenticator::lookup_by_identity` が `ACTOR_ID_PREFIX` を前置して
/// actor id を作るため、ここでは前置前の値を渡す。
fn service_principal() -> VerifiedIdentity {
    VerifiedIdentity {
        sub: "service:answer-api".to_string(),
        email: "answer-api@cs-support.internal".to_string(),
    }
}

/// `ErrorBody` を JSON で返す。
fn error_response(status: StatusCode, error: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: error.to_string(),
            message: message.into(),
        }),
    )
        .into_response()
}

/// `evaluate()` 呼び出し前に conv state を 500 なしで読めなかった場合の共通処理。
fn conv_state_load_failed(err: &anyhow::Error, request_id: &str, case_id: &str) -> Response {
    tracing::error!(
        error = ?err,
        request_id = %request_id,
        case_id = %case_id,
        "answer api: load_conv_state failed"
    );
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "failed to load conversation state; see server logs",
    )
}

/// conv state の保存に失敗した場合の共通処理。
fn conv_state_save_failed(err: &anyhow::Error, request_id: &str, case_id: &str) -> Response {
    tracing::error!(
        error = ?err,
        request_id = %request_id,
        case_id = %case_id,
        "answer api: save_conv_state failed"
    );
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "failed to save conversation state; see server logs",
    )
}

/// `POST /{project_id}/api/reply`（design doc §2〜§4。会話フロー v1.1 design doc §2・§5）。
///
/// 処理順:
/// 1. 認証（`authorize`）→ 401
/// 2. `project_id` を `state.config.projects` から解決 → 無ければ 404
/// 3. body を `ReplyRequest` としてデシリアライズ・`validate` → どちらの失敗も 400
/// 4. `history` を変換し `harness.begin`
/// 5. `req.case_id` があり、その会話が `awaiting_time_pref = true` なら、次の発話を
///    希望時間帯の返信として先に解釈する（会話フロー v1.1 design doc §5）。この段階の
///    state 変更はすべて `evaluate()` を呼ぶ**前**に保存を完了させる（`save_conv_state` の
///    lost-update 契約。`harness::mod::Harness::save_conv_state` の doc コメント参照）。
///    時間帯返信として確定した場合はここで応答を返し、`evaluate()` を呼ばない。
/// 6. `harness.evaluate`。`Err` はエラー分類関数で 503 / 500（500 は必ず `tracing::error!`）
/// 7. `Ok(outcome)` は最新の conv state を取り直し、`decide_reply_action`（design doc §2 の
///    決定表）で `Answer` / `Clarify` / `EscalationReply` を決めて 200 を返す
async fn reply_handler(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ReplyRequest>, JsonRejection>,
) -> Response {
    if !authorize(&headers, &state.api_key) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid Authorization: Bearer <API key> header",
        );
    }

    let Some(project) = state
        .config
        .projects
        .iter()
        .find(|p| p.project_id == project_id)
    else {
        return error_response(
            StatusCode::NOT_FOUND,
            "unknown_project",
            format!("no project configured for project_id={project_id}"),
        );
    };

    let req = match body {
        Ok(Json(req)) => req,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                rejection.to_string(),
            );
        }
    };
    if let Err(message) = validate(&req) {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request", message);
    }

    let history: Vec<ReplyHistoryTurn> = req
        .history
        .unwrap_or_default()
        .into_iter()
        .map(|entry| ReplyHistoryTurn {
            role: match entry.role {
                HistoryRole::Customer => ReplyHistoryRole::Customer,
                HistoryRole::Assistant => ReplyHistoryRole::Assistant,
            },
            text: entry.text,
        })
        .collect();

    let identity = service_principal();
    let ctx = match state
        .harness
        .begin(&identity, &project.schema, project.manual_schema)
    {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::error!(
                error = ?err,
                project_id = %project_id,
                "answer api: harness.begin failed"
            );
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to start the request; see server logs",
            );
        }
    };

    let request_id = ctx.request_id.clone();

    // ステップ 5: 希望時間帯の受付（会話フロー v1.1 design doc §5）。
    // `evaluate()` を呼ぶ前にすべての state 変更・保存を完了させる。
    if let Some(case_id) = req.case_id.as_deref() {
        let mut conv = match state.harness.load_conv_state(&ctx, case_id).await {
            Ok(conv) => conv,
            Err(err) => return conv_state_load_failed(&err, &request_id, case_id),
        };

        if conv.awaiting_time_pref {
            // `reply_drafter` が `None`（`[llm] enabled = false`）のときは時間帯抽出そのものが
            // 実行不能。抽出インフラ失敗と同じ経路（連続回数で自動解除）へ合流させる
            // （design doc §5）。
            let extraction = match state.harness.reply_drafter.as_ref() {
                Some(drafter) => time_pref::extract_time_preference(drafter, &req.message).await,
                None => Err(time_pref::TimePrefExtractionError),
            };

            match extraction {
                Ok(extraction) => {
                    conv.time_pref_extraction_error_count = 0;
                    let action = time_pref::handle_time_pref(
                        &extraction,
                        &mut conv,
                        &state.config.api.business_hours,
                    );
                    if let Err(err) = state.harness.save_conv_state(&ctx, case_id, &conv).await {
                        return conv_state_save_failed(&err, &request_id, case_id);
                    }
                    if let time_pref::TimePrefAction::Reply(text) = action {
                        return (
                            StatusCode::OK,
                            Json(ReplyResponse {
                                reply_text: text,
                                case_id: case_id.to_string(),
                            }),
                        )
                            .into_response();
                    }
                    // TimePrefAction::PassToEvaluate: 下の evaluate() へ続行。
                }
                Err(_) => {
                    note_time_pref_extraction_failure(&mut conv);
                    if let Err(err) = state.harness.save_conv_state(&ctx, case_id, &conv).await {
                        return conv_state_save_failed(&err, &request_id, case_id);
                    }
                    // 通常の evaluate フローへ続行。
                }
            }
        }
    }

    // 「初回か継続か」の判定は決定論（コード）。文面の出し分けは `clarify.rs` /
    // `escalation_reply.rs` / `reply.rs`（回答下書き）側に `is_continuation` として渡すだけ
    // （design doc §3）。`evaluate()` の内部で回答下書き生成（`draft_customer_reply`）まで
    // 完結するため、`evaluate()` を呼ぶ前に計算しておく必要がある。
    let is_continuation = is_continuation(&history, req.case_id.as_deref());

    match state
        .harness
        .evaluate(
            &ctx,
            &req.message,
            None,
            req.case_id.as_deref(),
            &state.tools,
            &history,
            is_continuation,
            // /api/reply は design doc §2 の契約: 未知 case_id はエラーにせず新規 case
            // として処理する（クライアント保存漏れ・再起動由来の未知 id は通常運用）。
            crate::harness::UnknownCaseIdPolicy::StartNew,
        )
        .await
    {
        Ok(outcome) => {
            let mut conv = match state.harness.load_conv_state(&ctx, &outcome.case_id).await {
                Ok(conv) => conv,
                Err(err) => return conv_state_load_failed(&err, &request_id, &outcome.case_id),
            };

            match decide_reply_action(&outcome, &conv, &state.config.api) {
                ReplyAction::Answer(text) => (
                    StatusCode::OK,
                    Json(ReplyResponse {
                        reply_text: text,
                        case_id: outcome.case_id,
                    }),
                )
                    .into_response(),
                ReplyAction::Clarify => {
                    let missing: &[decision::EvidenceRequirement] = match &outcome.decision {
                        AnswerDecision::Escalate { missing, .. } => missing,
                        other => {
                            tracing::error!(
                                request_id = %request_id,
                                decision = ?other,
                                "decide_reply_action returned Clarify for a non-Escalate decision; \
                                 this is a bug in decide_reply_action's decision-table logic. \
                                 Falling back to an empty missing list so the clarify prompt still \
                                 degrades gracefully instead of panicking"
                            );
                            &[]
                        }
                    };
                    let missing_text = missing_to_text(missing);
                    let reply_text = match state.harness.reply_drafter.as_ref() {
                        Some(drafter) => {
                            clarify::draft_clarify_question(
                                drafter,
                                &state.harness.ng,
                                state.harness.reply_draft_max_tokens,
                                &req.message,
                                &missing_text,
                                is_continuation,
                            )
                            .await
                        }
                        None => clarify::FALLBACK_CLARIFY_TEXT.to_string(),
                    };

                    conv.clarify_turns += 1;
                    if let Err(err) = state
                        .harness
                        .save_conv_state(&ctx, &outcome.case_id, &conv)
                        .await
                    {
                        return conv_state_save_failed(&err, &request_id, &outcome.case_id);
                    }
                    (
                        StatusCode::OK,
                        Json(ReplyResponse {
                            reply_text,
                            case_id: outcome.case_id,
                        }),
                    )
                        .into_response()
                }
                ReplyAction::EscalationReply => {
                    let ack_text = match state.harness.reply_drafter.as_ref() {
                        Some(drafter) => {
                            escalation_reply::draft_ack_text(
                                drafter,
                                &state.harness.ng,
                                state.harness.reply_draft_max_tokens,
                                &req.message,
                                is_continuation,
                            )
                            .await
                        }
                        None => escalation_reply::fallback_ack(is_continuation)
                            .0
                            .to_string(),
                    };
                    let out_of_hours_now = !hours::is_within_business_hours(
                        &state.config.api.business_hours,
                        chrono::Utc::now(),
                    );
                    let hours_label = hours::business_hours_label(&state.config.api.business_hours);
                    let block = escalation_reply::build_deterministic_block(
                        &outcome.case_id,
                        &hours_label,
                        out_of_hours_now,
                    );
                    let reply_text = escalation_reply::assemble_escalation_reply(&ack_text, &block);

                    arm_time_pref_solicitation(&mut conv);
                    if let Err(err) = state
                        .harness
                        .save_conv_state(&ctx, &outcome.case_id, &conv)
                        .await
                    {
                        return conv_state_save_failed(&err, &request_id, &outcome.case_id);
                    }
                    (
                        StatusCode::OK,
                        Json(ReplyResponse {
                            reply_text,
                            case_id: outcome.case_id,
                        }),
                    )
                        .into_response()
                }
            }
        }
        Err(err) => {
            let (status, code) = classify_evaluate_error(&err);
            if status == StatusCode::SERVICE_UNAVAILABLE {
                tracing::warn!(
                    request_id = %request_id,
                    error = ?err,
                    "answer api: evaluate failed due to upstream (vegapunk) unavailability"
                );
                error_response(
                    status,
                    code,
                    format!("vegapunk is unavailable (request_id={request_id}); please retry"),
                )
            } else {
                tracing::error!(
                    request_id = %request_id,
                    error = ?err,
                    "answer api: evaluate failed"
                );
                error_response(
                    status,
                    code,
                    format!("internal error (request_id={request_id}); see server logs"),
                )
            }
        }
    }
}

/// `/{project_id}/api/reply` の router を組み立てる。main.rs は `config.api.enabled` の
/// ときだけこれを `app.merge(...)` する（design doc §3。無効時はルート自体を公開しない）。
pub fn api_router(state: ApiState) -> Router {
    Router::new()
        .route("/{project_id}/api/reply", post(reply_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_request() -> ReplyRequest {
        ReplyRequest {
            message: "カメラが反応しません".to_string(),
            history: None,
            case_id: None,
        }
    }

    #[test]
    fn validate_accepts_a_minimal_valid_request() {
        assert!(validate(&valid_request()).is_ok());
    }

    #[test]
    fn validate_rejects_empty_message() {
        let req = ReplyRequest {
            message: "  ".into(),
            history: None,
            case_id: None,
        };
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_message_over_5000_chars() {
        let req = ReplyRequest {
            message: "あ".repeat(MAX_MESSAGE_CHARS + 1),
            history: None,
            case_id: None,
        };
        let err = validate(&req).expect_err("5,001 chars must be rejected");
        assert!(err.contains("message"), "error must name the field: {err}");
    }

    #[test]
    fn validate_accepts_message_at_exactly_5000_chars() {
        let req = ReplyRequest {
            message: "あ".repeat(MAX_MESSAGE_CHARS),
            history: None,
            case_id: None,
        };
        assert!(validate(&req).is_ok());
    }

    #[test]
    fn validate_rejects_history_over_20_entries() {
        let history = (0..MAX_HISTORY_ENTRIES + 1)
            .map(|i| HistoryEntry {
                role: HistoryRole::Customer,
                text: format!("t{i}"),
            })
            .collect();
        let req = ReplyRequest {
            history: Some(history),
            ..valid_request()
        };
        let err = validate(&req).expect_err("21 history entries must be rejected");
        assert!(err.contains("history"), "error must name the field: {err}");
    }

    #[test]
    fn validate_accepts_history_at_exactly_20_entries() {
        let history = (0..MAX_HISTORY_ENTRIES)
            .map(|i| HistoryEntry {
                role: HistoryRole::Customer,
                text: format!("t{i}"),
            })
            .collect();
        let req = ReplyRequest {
            history: Some(history),
            ..valid_request()
        };
        assert!(validate(&req).is_ok());
    }

    #[test]
    fn validate_rejects_history_text_over_2000_chars() {
        let req = ReplyRequest {
            history: Some(vec![HistoryEntry {
                role: HistoryRole::Assistant,
                text: "い".repeat(MAX_HISTORY_TEXT_CHARS + 1),
            }]),
            ..valid_request()
        };
        let err = validate(&req).expect_err("2,001 char history text must be rejected");
        assert!(
            err.contains("history[0]"),
            "error must name the offending entry: {err}"
        );
    }

    #[test]
    fn validate_rejects_empty_history_text() {
        let req = ReplyRequest {
            history: Some(vec![HistoryEntry {
                role: HistoryRole::Customer,
                text: "   ".into(),
            }]),
            ..valid_request()
        };
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_case_id_over_128_chars() {
        let req = ReplyRequest {
            case_id: Some("c".repeat(MAX_CASE_ID_CHARS + 1)),
            ..valid_request()
        };
        let err = validate(&req).expect_err("129 char case_id must be rejected");
        assert!(err.contains("case_id"), "error must name the field: {err}");
    }

    #[test]
    fn validate_accepts_case_id_at_exactly_128_chars() {
        let req = ReplyRequest {
            case_id: Some("c".repeat(MAX_CASE_ID_CHARS)),
            ..valid_request()
        };
        assert!(validate(&req).is_ok());
    }

    // ---- constant_time_eq ----

    #[test]
    fn constant_time_eq_accepts_matching_bytes() {
        assert!(constant_time_eq(b"secret-key", b"secret-key"));
    }

    #[test]
    fn constant_time_eq_rejects_different_bytes_of_same_length() {
        assert!(!constant_time_eq(b"secret-key", b"secret-kez"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"short", b"much-longer-value"));
    }

    #[test]
    fn constant_time_eq_treats_empty_slices_as_equal() {
        assert!(constant_time_eq(b"", b""));
    }

    // ---- hashed_constant_time_eq ----
    //
    // `authorize` が実際に呼ぶ関数を直接テストする（Stage 2 レビュー指摘: 以前はテストが
    // 自前で SHA-256 を計算して `constant_time_eq` に渡すだけで、`authorize` がハッシュ化
    // しているかどうかを一切検証しておらず、ハッシュ化を外す退行があっても検出できなかった）。

    /// 元の長さが大きく異なる不一致入力でも `false` を返す。ハッシュ化を外して生の
    /// バイト列比較へ戻す退行が起きると、この経路自体は生比較でも `false` になるため
    /// この 1 件だけでは退行を検出できない（下の完全一致テストと対で意味を持つ）。
    #[test]
    fn hashed_constant_time_eq_rejects_mismatched_inputs_of_very_different_lengths() {
        assert!(!hashed_constant_time_eq(
            "short",
            "a-much-longer-configured-api-key-value-that-does-not-match"
        ));
    }

    /// 完全一致では `true` を返す。長さが大きく異なる入力同士でもハッシュ化さえしていれば
    /// 一致判定は正しく行われることを確認する。
    #[test]
    fn hashed_constant_time_eq_accepts_identical_inputs() {
        let value = "a-much-longer-configured-api-key-value-that-does-match";
        assert!(hashed_constant_time_eq(value, value));
    }

    /// `authorize` 経由でも上記の性質が実際に成立すること（presented と configured の
    /// 元の長さが大きく異なっていても、不一致は正しく判定される）。
    #[test]
    fn authorize_rejects_mismatched_keys_of_very_different_lengths() {
        let headers = headers_with_bearer("Bearer x");
        assert!(!authorize(
            &headers,
            "a-much-longer-configured-api-key-that-does-not-match"
        ));
    }

    // ---- authorize ----

    fn headers_with_bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn authorize_accepts_matching_bearer_key() {
        let headers = headers_with_bearer("Bearer correct-key");
        assert!(authorize(&headers, "correct-key"));
    }

    #[test]
    fn authorize_rejects_mismatched_key() {
        let headers = headers_with_bearer("Bearer wrong-key");
        assert!(!authorize(&headers, "correct-key"));
    }

    #[test]
    fn authorize_rejects_missing_authorization_header() {
        let headers = HeaderMap::new();
        assert!(!authorize(&headers, "correct-key"));
    }

    #[test]
    fn authorize_rejects_non_bearer_scheme() {
        let headers = headers_with_bearer("Basic correct-key");
        assert!(!authorize(&headers, "correct-key"));
    }

    #[test]
    fn authorize_accepts_lowercase_bearer_scheme() {
        let headers = headers_with_bearer("bearer correct-key");
        assert!(authorize(&headers, "correct-key"));
    }

    #[test]
    fn authorize_rejects_empty_configured_key() {
        // 設定不備（空文字の api_key）で誰でも通ってしまう事故を防ぐ。
        // main.rs の起動時 fail-closed チェックが空文字を弾くため通常は起きないが、
        // この関数単体でも「空 == 空」で一致させない（本来は main.rs 側で防ぐ話だが、
        // 関数の契約として空鍵の実質無認証化を許さないことを明示する）。
        let headers = headers_with_bearer("Bearer ");
        assert!(!authorize(&headers, ""));
    }

    // ---- decide_reply_action（会話フロー v1.1 design doc §2 の決定表） ----

    fn allowed_decision() -> AnswerDecision {
        AnswerDecision::Allowed {
            source: crate::harness::decision::AnswerSource::Manual,
            evidence_section_keys: vec!["doc#sec1".to_string()],
            known_resolution_id: None,
            stakes: crate::harness::decision::Stakes::Low,
            threshold: 0.8,
        }
    }

    /// 第3層グレー相当（`InsufficientDirectness`）の escalate。
    fn gray_escalate_decision() -> AnswerDecision {
        AnswerDecision::Escalate {
            reason: crate::harness::decision::EscalateReason::InsufficientDirectness,
            layer: 3,
            route_to: "triage".to_string(),
            disclosure_scope: crate::harness::decision::DisclosureScope::NoInternalDetails,
            audit_required: true,
            missing: vec![decision::EvidenceRequirement::DirectManualCoverage {
                required: 0.8,
                best: 0.5,
            }],
        }
    }

    /// 第1層（明示エスカレーションルール、rule_match）相当の escalate。
    fn rule_match_escalate_decision() -> AnswerDecision {
        AnswerDecision::Escalate {
            reason: crate::harness::decision::EscalateReason::RegulatedOrSafety,
            layer: 1,
            route_to: "triage".to_string(),
            disclosure_scope: crate::harness::decision::DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: vec![],
        }
    }

    fn base_outcome(
        decision: AnswerDecision,
        clarification_allowed: bool,
    ) -> crate::harness::EvaluationOutcome {
        crate::harness::EvaluationOutcome {
            decision,
            signals: crate::harness::signal::SignalSet::new(),
            accumulated_signals: crate::harness::signal::SignalSet::new(),
            case_id: "case-12345678-abcd".to_string(),
            clarification_allowed,
            hits: Vec::new(),
            audit_event_id: "audit-1".to_string(),
            related_cases: Vec::new(),
            extraction_mode: crate::harness::extraction::ExtractionMode::LexiconOnly,
            customer_reply_draft: None,
            customer_reply_draft_truncated: false,
        }
    }

    fn default_conv_state() -> crate::harness::CaseConvState {
        crate::harness::CaseConvState {
            clarify_turns: 0,
            awaiting_time_pref: false,
            time_pref_false_count: 0,
            preferred_contact_time: None,
            time_pref_extraction_error_count: 0,
        }
    }

    fn default_api_config() -> crate::config::ApiConfig {
        crate::config::ApiConfig {
            enabled: true,
            clarify_max_turns: 3,
            ..Default::default()
        }
    }

    // --- note_time_pref_extraction_failure ---

    #[test]
    fn note_time_pref_extraction_failure_first_time_keeps_awaiting_and_increments_count() {
        let mut conv = default_conv_state();
        conv.awaiting_time_pref = true;

        note_time_pref_extraction_failure(&mut conv);

        assert_eq!(conv.time_pref_extraction_error_count, 1);
        assert!(conv.awaiting_time_pref, "1回目では自動解除しない");
    }

    #[test]
    fn note_time_pref_extraction_failure_second_time_keeps_awaiting_and_increments_count() {
        let mut conv = default_conv_state();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 1;

        note_time_pref_extraction_failure(&mut conv);

        assert_eq!(conv.time_pref_extraction_error_count, 2);
        assert!(conv.awaiting_time_pref, "2回目では自動解除しない");
    }

    #[test]
    fn note_time_pref_extraction_failure_third_time_clears_all_three_counters() {
        let mut conv = default_conv_state();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 2;
        conv.time_pref_false_count = 1; // 解除と同時にリセットされることを確認するため非ゼロにしておく

        note_time_pref_extraction_failure(&mut conv);

        assert!(!conv.awaiting_time_pref, "3回連続で自動解除する");
        assert_eq!(conv.time_pref_false_count, 0);
        assert_eq!(conv.time_pref_extraction_error_count, 0);
    }

    // --- arm_time_pref_solicitation ---

    #[test]
    fn arm_time_pref_solicitation_sets_awaiting_and_resets_false_and_clarify_counters() {
        let mut conv = default_conv_state();
        conv.awaiting_time_pref = false;
        conv.time_pref_false_count = 2;
        conv.clarify_turns = 3;

        arm_time_pref_solicitation(&mut conv);

        assert!(
            conv.awaiting_time_pref,
            "新しいエスカレーション応答は希望時間帯を尋ねる"
        );
        assert_eq!(conv.time_pref_false_count, 0);
        assert_eq!(conv.clarify_turns, 0);
    }

    /// design doc §5 はリセット対象を「分類成功時」と「3 回到達時」の 2 つに限定しており、
    /// 新しいエスカレーション応答の送信はそのどちらでもない。ここで
    /// `time_pref_extraction_error_count` を 0 に戻すと、抽出インフラが継続的に失敗している
    /// 状況で毎ターン `EscalationReply` に倒れるたびカウンタが 0 に巻き戻り、3 回連続到達に
    /// よる自動解除（`note_time_pref_extraction_failure`）が永久に到達不能になる。
    #[test]
    fn arm_time_pref_solicitation_does_not_touch_extraction_error_count() {
        let mut conv = default_conv_state();
        conv.time_pref_extraction_error_count = 2;

        arm_time_pref_solicitation(&mut conv);

        assert_eq!(
            conv.time_pref_extraction_error_count, 2,
            "3回連続の自動解除を到達可能に保つため、ここではリセットしない"
        );
    }

    #[test]
    fn decide_reply_action_answers_when_allowed_with_a_non_truncated_draft() {
        let mut outcome = base_outcome(allowed_decision(), false);
        outcome.customer_reply_draft = Some("下書き本文".to_string());
        outcome.customer_reply_draft_truncated = false;
        let action = decide_reply_action(&outcome, &default_conv_state(), &default_api_config());
        assert_eq!(action, ReplyAction::Answer("下書き本文".to_string()));
    }

    /// truncated な下書きは「見た目は完成しているが安全上の但し書きだけが落ちている」
    /// 可能性があり、`/api/reply` には人間の検分が入らないため、非空の下書きでも
    /// `EscalationReply` へ倒さなければならない（llm.rs の `ReplyDraft` doc コメント参照）。
    #[test]
    fn decide_reply_action_escalates_when_allowed_but_draft_is_truncated() {
        let mut outcome = base_outcome(allowed_decision(), false);
        outcome.customer_reply_draft = Some("途中で切れた下書き本文...".to_string());
        outcome.customer_reply_draft_truncated = true;
        let action = decide_reply_action(&outcome, &default_conv_state(), &default_api_config());
        assert_eq!(action, ReplyAction::EscalationReply);
    }

    #[test]
    fn decide_reply_action_escalates_when_allowed_without_a_draft() {
        let outcome = base_outcome(allowed_decision(), false);
        let action = decide_reply_action(&outcome, &default_conv_state(), &default_api_config());
        assert_eq!(action, ReplyAction::EscalationReply);
    }

    #[test]
    fn decide_reply_action_clarifies_when_gray_escalate_with_turns_remaining() {
        let outcome = base_outcome(gray_escalate_decision(), true);
        let mut conv = default_conv_state();
        conv.clarify_turns = 2; // < clarify_max_turns(3)
        let action = decide_reply_action(&outcome, &conv, &default_api_config());
        assert_eq!(action, ReplyAction::Clarify);
    }

    #[test]
    fn decide_reply_action_escalates_when_clarify_turns_are_exhausted() {
        let outcome = base_outcome(gray_escalate_decision(), true);
        let mut conv = default_conv_state();
        conv.clarify_turns = 3; // == clarify_max_turns(3): 枯渇
        let action = decide_reply_action(&outcome, &conv, &default_api_config());
        assert_eq!(action, ReplyAction::EscalationReply);
    }

    #[test]
    fn decide_reply_action_escalates_when_clarification_is_not_allowed() {
        // 第1・2層起因の escalate は clarification_allowed = false（決定論）。
        let outcome = base_outcome(gray_escalate_decision(), false);
        let action = decide_reply_action(&outcome, &default_conv_state(), &default_api_config());
        assert_eq!(action, ReplyAction::EscalationReply);
    }

    /// `decide_reply_action` 自身は `layer` を見ない（`clarification_allowed` の値だけで
    /// 分岐する）。rule_match（第1層）相当の escalate で `clarification_allowed = false` が
    /// 渡ったとき、turns 消費とは無関係にエスカレーションへ倒れることを固定する。
    /// 「第1層では `clarification_allowed` が常に false になる」こと自体は、この関数ではなく
    /// 呼び出し元（`harness/mod.rs` の `clarification_allowed()` 契約テスト）が保証している。
    #[test]
    fn decide_reply_action_escalates_for_rule_match_even_with_turns_remaining() {
        let outcome = base_outcome(rule_match_escalate_decision(), false);
        let action = decide_reply_action(&outcome, &default_conv_state(), &default_api_config());
        assert_eq!(action, ReplyAction::EscalationReply);
    }

    // ---- is_continuation（会話フロー v1.1 design doc §3: 初回/継続の決定論判定） ----

    #[test]
    fn is_continuation_true_when_history_is_non_empty() {
        let history = vec![ReplyHistoryTurn {
            role: ReplyHistoryRole::Customer,
            text: "前回の発話".to_string(),
        }];
        assert!(is_continuation(&history, None));
    }

    #[test]
    fn is_continuation_true_when_history_is_empty_but_case_id_is_present() {
        assert!(is_continuation(&[], Some("case-12345678-abcd")));
    }

    #[test]
    fn is_continuation_false_when_history_is_empty_and_case_id_is_absent() {
        assert!(!is_continuation(&[], None));
    }

    // ---- missing_to_text ----

    #[test]
    fn missing_to_text_renders_coverage_requirement() {
        let missing = vec![decision::EvidenceRequirement::DirectManualCoverage {
            required: 0.8,
            best: 0.5,
        }];
        let text = missing_to_text(&missing);
        assert!(text.contains("0.80"));
        assert!(text.contains("0.50"));
    }

    #[test]
    fn missing_to_text_has_a_fallback_for_empty_missing() {
        let text = missing_to_text(&[]);
        assert!(!text.is_empty());
    }

    // ---- classify_evaluate_error（design doc §2 のエラー表: 503 / 500） ----

    #[test]
    fn classify_evaluate_error_maps_bare_tonic_status_to_503() {
        let err = anyhow::Error::from(tonic::Status::unavailable("vegapunk down"));
        let (status, code) = classify_evaluate_error(&err);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code, "upstream_unavailable");
    }

    /// `evaluate` は `?` / `.context(...)` を重ねて呼び出し元まで伝播するため、
    /// `tonic::Status` が anyhow の context chain の奥に埋まっているケースも 503 になること
    /// を固定する（`downcast_ref` が chain を辿ることの回帰テスト）。
    #[test]
    fn classify_evaluate_error_finds_tonic_status_wrapped_in_context() {
        let err = anyhow::Error::from(tonic::Status::unavailable("vegapunk down"))
            .context("load known resolutions")
            .context("evaluate_answerability");
        let (status, code) = classify_evaluate_error(&err);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code, "upstream_unavailable");
    }

    #[test]
    fn classify_evaluate_error_maps_non_tonic_error_to_500() {
        let err = anyhow::anyhow!("lexicon file missing");
        let (status, code) = classify_evaluate_error(&err);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "internal");
    }

    // ---- /api/reply ルーティング ----
    //
    // 401（認証）/ 400（入力検証・JSON 不正）/ 404（未知 project）はいずれも
    // `Harness::evaluate` へ到達する前に reject される経路なので、実 vegapunk 無しで
    // router を `tower::ServiceExt::oneshot` で叩いて確認できる。200 経路（evaluate が
    // `Ok` を返す場合）は実 vegapunk が必要なため対象外（前回 run の差し戻し・design doc §8
    // の裁定どおり）。ただし 500 経路（`knowledge: None` により `evaluate` 内部の
    // `self.knowledge()?` が必ず `Err` になる）は実 vegapunk 無しで到達できるため対象に含める
    // （Stage 1 レビュー指摘: 500 応答が内部エラー文字列を漏らさないことの回帰確認）。

    /// テスト専用の最小 `Harness`。`knowledge: None` なので `evaluate` を呼べば必ず失敗する
    /// （このモジュールのルーティングテストの一部はそれを利用して 500 経路を確認する。他の
    /// テストは 401/400/404 のいずれも `evaluate` へ到達する前に reject されるため問題にならない。
    /// `harness::mod::tests::harness_for_test` と同じ構成。
    /// あちらは private でこのモジュールから使えないため、同じ構成をここで独立に組み立てる）。
    fn test_harness() -> Harness {
        let dir = std::env::temp_dir().join(format!("api-test-harness-{}", uuid::Uuid::new_v4()));
        let lexicon = Arc::new(
            crate::harness::signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap(),
        );
        Harness {
            authenticator: crate::harness::authn::Authenticator::new(vec!["urtect".to_string()]),
            normalizer: lexicon.clone(),
            extractor: Arc::new(crate::harness::extraction::HybridExtractor::new(
                lexicon.clone(),
                None,
            )),
            lexicon,
            ng: crate::harness::egress::NgDictionary::from_json(
                r#"{"block_terms":[],"abstain_terms":[]}"#,
            )
            .unwrap(),
            worm: Arc::new(
                crate::harness::audit::WormAuditLog::open(&dir.join("audit.jsonl")).unwrap(),
            ),
            knowledge: None,
            thresholds: crate::harness::decision::Thresholds {
                low: 0.6,
                mid: 0.8,
                high: 0.95,
            },
            grading: crate::harness::grading::GradingThresholds {
                promote_approvals: 3,
                promote_approvers: 2,
                promote_max_rejection_rate: 0.2,
                demote_rejections: 2,
            },
            queue_path: dir.join("queue.jsonl"),
            grade_lock: tokio::sync::Mutex::new(()),
            manual: None,
            corpus: None,
            default_route: "triage".to_string(),
            vector_route_enabled: false,
            reply_drafter: None,
            reply_draft_max_tokens: 700,
        }
    }

    fn test_api_state(api_key: &str) -> ApiState {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://vegapunk.invalid:6840"
[[projects]]
project_id = "urtect"
schema = "urtect"
[api]
enabled = true
fallback_reply_text = "担当者が確認のうえご連絡します"
"#;
        let config: AppConfig = toml::from_str(toml).expect("valid test config");
        let vegapunk = crate::vegapunk::VegapunkClient::connect_lazy_with_limits(
            &config.vegapunk_endpoint,
            "",
            crate::vegapunk::GrpcLimits::default(),
        )
        .expect("lazy connect never touches the network");
        ApiState {
            config: Arc::new(config),
            harness: Arc::new(test_harness()),
            tools: ToolService::new(vegapunk),
            api_key: api_key.to_string(),
        }
    }

    async fn oneshot_json(
        router: Router,
        method: &str,
        uri: &str,
        auth: Option<&str>,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(auth) = auth {
            builder = builder.header("authorization", auth);
        }
        let request = builder
            .body(Body::from(body.to_string()))
            .expect("build request");
        let response = router
            .oneshot(request)
            .await
            .expect("router must not error at the transport level");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).expect("error responses are always JSON");
        (status, value)
    }

    #[tokio::test]
    async fn reply_route_rejects_missing_authorization_with_401() {
        let router = api_router(test_api_state("correct-key"));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/urtect/api/reply",
            None,
            r#"{"message":"こんにちは"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
    }

    #[tokio::test]
    async fn reply_route_rejects_mismatched_api_key_with_401() {
        let router = api_router(test_api_state("correct-key"));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/urtect/api/reply",
            Some("Bearer wrong-key"),
            r#"{"message":"こんにちは"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
    }

    /// project 解決（404）は認証の直後・body 検証より前に行われる（design doc §2 の処理順）。
    /// 未知 project なので、body が壊れていても 404 が返ることも合わせて確認する。
    #[tokio::test]
    async fn reply_route_rejects_unknown_project_with_404_before_validating_body() {
        let router = api_router(test_api_state("correct-key"));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/no-such-project/api/reply",
            Some("Bearer correct-key"),
            "{ not json",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "unknown_project");
    }

    #[tokio::test]
    async fn reply_route_rejects_malformed_json_with_400() {
        let router = api_router(test_api_state("correct-key"));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/urtect/api/reply",
            Some("Bearer correct-key"),
            "{ not json",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn reply_route_rejects_validation_failure_with_400() {
        let router = api_router(test_api_state("correct-key"));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/urtect/api/reply",
            Some("Bearer correct-key"),
            r#"{"message":"   "}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
        assert!(
            body["message"]
                .as_str()
                .expect("message must be a string")
                .contains("message"),
            "error must name the offending field: {body}"
        );
    }

    /// `test_harness()` は `knowledge: None` なので、認証・project・validate をすべて通過した
    /// 正常リクエストは `evaluate()` 内部の `self.knowledge()?` で必ず `Err` になり 500 を返す
    /// （実 vegapunk なしで到達できる唯一の 500 経路）。この 500 応答が内部エラー文字列
    /// （`self.knowledge()?` が返す "knowledge store is not configured" 等）を漏らさず、
    /// `request_id` を含む固定フォーマットのメッセージだけを返すことを確認する。
    #[tokio::test]
    async fn reply_returns_500_without_leaking_internal_error_when_knowledge_unavailable() {
        let router = api_router(test_api_state("correct-key"));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/urtect/api/reply",
            Some("Bearer correct-key"),
            r#"{"message":"カメラが反応しません"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "internal");
        let message = body["message"].as_str().expect("message must be a string");
        assert!(
            message.contains("request_id"),
            "message must carry request_id for operators to correlate with server logs: {body}"
        );
        assert!(
            !message.contains("knowledge"),
            "message must not leak the internal error string returned by self.knowledge(): {body}"
        );
    }
}
