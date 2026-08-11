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
use crate::harness::decision::AnswerDecision;
use crate::harness::reply::{ReplyHistoryRole, ReplyHistoryTurn};
use crate::harness::Harness;
use crate::mcp::ToolService;
use crate::oauth::VerifiedIdentity;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
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
/// 通常の `==` は不一致箇所で早期リターンするため、比較にかかる時間から鍵の
/// 先頭一致長を推測されうる（タイミングサイドチャネル）。ここでは長さが違っても
/// 早期リターンせず、XOR を OR で畳み込んで最後に 0 判定する。
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
    constant_time_eq(presented.as_bytes(), api_key.as_bytes())
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

/// `evaluate` の結果から顧客向け最終応答文を決める純関数（design doc §4 の決定表）。
///
/// - `Allowed` かつ下書きあり かつ **非 truncated** → 下書きをそのまま使う（`is_fallback = false`）
/// - `Allowed` かつ下書きあり かつ **truncated** → フォールバック文（下記参照）
/// - `Allowed` かつ下書き `None`（LLM 失敗・egress 却下） → フォールバック文
/// - `Escalate`（rule_match 相当の reason も含め全 reason） → フォールバック文。
///   下書きが存在していても **無視する**（judge が escalate と決めた以上、その判定を
///   下書きの中身で覆してはならない。下書きは判定を知らずに生成されているため、
///   escalate 判定時にたまたま非空の下書きが残っていても顧客へは出さない）
///
/// truncated な下書きを捨てる理由（`llm.rs` の `ReplyDraft` doc コメント参照）:
/// 生成上限で途中切断された下書きは、**切れ目がたまたま「。」の直後に落ちると完成文に
/// 見える**。日本語のビジネス文は結び・注意書きが末尾に来るため、見た目は完成しているのに
/// 末尾の安全上の但し書きだけが落ちた下書きが成立しうる。MCP 経路（`rmcp_server.rs`）は
/// 人間の CS 担当が下書きを検分してから送るため `truncated=true` を返して警告するだけで
/// 足りるが、`/api/reply` は**人間の検分が一切入らない自動送信経路**であり、同じ扱いにはできない。
///
/// `is_fallback` は呼び出し側が warn ログを出すかどうかの判定に使う（design doc §4 末尾）。
pub fn reply_text_for<'a>(
    decision: &AnswerDecision,
    draft: Option<&'a str>,
    draft_truncated: bool,
    fallback: &'a str,
) -> (&'a str, bool) {
    match (decision, draft) {
        (AnswerDecision::Allowed { .. }, Some(text)) if !draft_truncated => (text, false),
        _ => (fallback, true),
    }
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

/// `POST /{project_id}/api/reply`（design doc §2〜§4）。
///
/// 処理順:
/// 1. 認証（`authorize`）→ 401
/// 2. `project_id` を `state.config.projects` から解決 → 無ければ 404
/// 3. body を `ReplyRequest` としてデシリアライズ・`validate` → どちらの失敗も 400
/// 4. `history` を変換し `harness.begin` → `harness.evaluate`
/// 5. `evaluate` の `Err` はエラー分類関数で 503 / 500（500 は必ず `tracing::error!`）
/// 6. `Ok(outcome)` は決定表の純関数で `reply_text` を決め、フォールバックなら
///    `tracing::warn!` を出して 200 を返す
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
    match state
        .harness
        .evaluate(
            &ctx,
            &req.message,
            None,
            req.case_id.as_deref(),
            &state.tools,
            &history,
            // /api/reply は design doc §2 の契約: 未知 case_id はエラーにせず新規 case
            // として処理する（クライアント保存漏れ・再起動由来の未知 id は通常運用）。
            crate::harness::UnknownCaseIdPolicy::StartNew,
        )
        .await
    {
        Ok(outcome) => {
            let (reply_text, is_fallback) = reply_text_for(
                &outcome.decision,
                outcome.customer_reply_draft.as_deref(),
                outcome.customer_reply_draft_truncated,
                &state.config.api.fallback_reply_text,
            );
            if is_fallback {
                // `draft_truncated` を分けて出すのは、運用者がここから次のアクションを
                // 判断できるようにするため: truncated=true なら
                // `harness.customer_reply_draft_max_tokens` を上げる余地があるが、
                // false（Escalate や下書き生成失敗）はそれでは直らない。
                tracing::warn!(
                    request_id = %request_id,
                    decision = ?outcome.decision,
                    draft_truncated = outcome.customer_reply_draft_truncated,
                    "answer api fell back to fixed reply"
                );
            }
            (
                StatusCode::OK,
                Json(ReplyResponse {
                    reply_text: reply_text.to_string(),
                    case_id: outcome.case_id,
                }),
            )
                .into_response()
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

    // ---- reply_text_for（design doc §4 の決定表） ----

    fn allowed_decision() -> AnswerDecision {
        AnswerDecision::Allowed {
            source: crate::harness::decision::AnswerSource::Manual,
            evidence_section_keys: vec!["doc#sec1".to_string()],
            known_resolution_id: None,
            stakes: crate::harness::decision::Stakes::Low,
            threshold: 0.8,
        }
    }

    fn escalate_decision() -> AnswerDecision {
        AnswerDecision::Escalate {
            reason: crate::harness::decision::EscalateReason::RegulatedOrSafety,
            layer: 1,
            route_to: "triage".to_string(),
            disclosure_scope: crate::harness::decision::DisclosureScope::NoInternalDetails,
            audit_required: true,
            missing: vec![],
        }
    }

    #[test]
    fn reply_text_for_allowed_with_draft_uses_the_draft_verbatim() {
        let (text, is_fallback) = reply_text_for(
            &allowed_decision(),
            Some("下書き本文"),
            false,
            "フォールバック文",
        );
        assert_eq!(text, "下書き本文");
        assert!(!is_fallback);
    }

    #[test]
    fn reply_text_for_allowed_without_draft_falls_back() {
        let (text, is_fallback) =
            reply_text_for(&allowed_decision(), None, false, "フォールバック文");
        assert_eq!(text, "フォールバック文");
        assert!(is_fallback);
    }

    /// truncated な下書きは「見た目は完成しているが安全上の但し書きだけが落ちている」
    /// 可能性があり、`/api/reply` には人間の検分が入らないため、非空の下書きでも
    /// フォールバックへ倒さなければならない（llm.rs の `ReplyDraft` doc コメント参照）。
    #[test]
    fn reply_text_for_allowed_with_truncated_draft_falls_back() {
        let (text, is_fallback) = reply_text_for(
            &allowed_decision(),
            Some("途中で切れた下書き本文..."),
            true,
            "フォールバック文",
        );
        assert_eq!(text, "フォールバック文");
        assert!(is_fallback);
    }

    #[test]
    fn reply_text_for_escalate_falls_back_even_without_a_draft() {
        let (text, is_fallback) =
            reply_text_for(&escalate_decision(), None, false, "フォールバック文");
        assert_eq!(text, "フォールバック文");
        assert!(is_fallback);
    }

    /// escalate 判定のとき、たまたま非空の下書きが残っていても顧客へは出さない
    /// （下書きは判定を知らずに生成されるため、escalate という判定そのものを覆してはならない）。
    #[test]
    fn reply_text_for_escalate_ignores_a_present_draft() {
        let (text, is_fallback) = reply_text_for(
            &escalate_decision(),
            Some("危険な下書き"),
            false,
            "フォールバック文",
        );
        assert_eq!(text, "フォールバック文");
        assert!(is_fallback);
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
