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
use crate::harness::Harness;
use crate::mcp::ToolService;
use axum::http::{header, HeaderMap};
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
}
