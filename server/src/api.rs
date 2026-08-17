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
use crate::harness::product_gate;
use crate::harness::reply::{ReplyHistoryRole, ReplyHistoryTurn};
use crate::harness::{clarify, escalation_reply, hours, time_pref, Harness, RequestContext};
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

/// リクエストボディ（design doc §2、`end_user_id` は 2026-08-16 admin dashboard design doc §3
/// の加算フィールド）。
#[derive(Debug, Clone, Deserialize)]
pub struct ReplyRequest {
    pub message: String,
    #[serde(default)]
    pub history: Option<Vec<HistoryEntry>>,
    #[serde(default)]
    pub case_id: Option<String>,
    /// 匿名化済みエンドユーザー識別子（1〜64 字。`[a-f0-9]` 想定だが形式は強制しない）。
    /// 未提供でも従来どおり動作する（後方互換、design doc §3）。
    #[serde(default)]
    pub end_user_id: Option<String>,
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
/// `end_user_id` の最小・最大文字数（2026-08-16 admin dashboard design doc §3）。
pub const MIN_END_USER_ID_CHARS: usize = 1;
pub const MAX_END_USER_ID_CHARS: usize = 64;

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

    if let Some(end_user_id) = &req.end_user_id {
        let chars = end_user_id.chars().count();
        if !(MIN_END_USER_ID_CHARS..=MAX_END_USER_ID_CHARS).contains(&chars) {
            return Err(format!(
                "end_user_id must be between {MIN_END_USER_ID_CHARS} and \
                 {MAX_END_USER_ID_CHARS} characters, got {chars}"
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
    // Issue #28 C2(c): 今ターンの signal 抽出 LLM 呼び出しが失敗し LexiconFallback に
    // 落ちた場合、Allowed/Escalate の判定結果によらず常にエスカレーション応答へ倒す
    // (fail-closed)。抽出 LLM が不調な状況では同一 API 経由の回答下書き LLM も同様に
    // 不調である可能性が高く、通常フロー（自動回答・聞き返し）を維持するより安全側へ
    // 倒す方が実質的な追加コストなしで成立する（design doc §5）。
    if outcome.extraction_mode == crate::harness::extraction::ExtractionMode::LexiconFallback {
        // レビュー修正3: Anthropic API 障害時は `/api/reply` の全トラフィックがここを通って
        // 人手エスカレーションへ倒れる。ログが無いと運用者は「なぜ全件エスカレーションに
        // なったか」を切り分けられない（抽出 LLM 障害なのか、他の判定ロジックの変化なのか
        // 区別できない）ため、握りつぶした元の判定ラベルとあわせて warn する。
        let suppressed_decision = match &outcome.decision {
            AnswerDecision::Allowed { .. } => "allowed",
            AnswerDecision::Escalate { .. } => "escalate",
        };
        tracing::warn!(
            case_id = %outcome.case_id,
            audit_event_id = %outcome.audit_event_id,
            suppressed_decision,
            "extraction LLM fell back to ExtractionMode::LexiconFallback; forcing \
             EscalationReply regardless of the underlying decision (fail-closed, Issue #28 \
             design doc §5)"
        );
        return ReplyAction::EscalationReply;
    }
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

/// 顧客発話 1 件を「把握済み事項」の 1 行として安全に埋め込むための正規化（Critical 2）。
///
/// `<把握済み事項>` は `build_clarify_prompt` の system prompt が「サーバが機械的に組み立てた
/// 記録」として扱う信頼ブロックである。しかし中身は顧客発話（信頼できない入力）そのものであり、
/// `neutralize_delimiters` は `<` `>` の全角化しか行わないため、山括弧を使わない改行だけで
/// 「サーバ由来に見える偽の箇条書き行」を挿し込める（例: 顧客発話に改行を含め、次の行を
/// `- 把握済みの条件語: ...` のように偽装する）。
///
/// 改行・復帰・その他の制御文字を半角スペースへ潰し、連続空白を 1 つにまとめてから切り詰める
/// ことで、「1 顧客発話 = 必ず 1 行」という不変条件をコードで保証する。
fn normalize_customer_turn_to_single_line(text: &str) -> String {
    let single_spaced = crate::harness::prompt_input::collapse_to_single_line(text);
    crate::harness::prompt_input::truncate_chars(&single_spaced, 100)
}

/// 「把握済み事項リスト」専用の customer 発話選択（Warning 1 修正、2 巡目の Warning で
/// 選択と正規化の順序を修正）。
///
/// **なぜ `reply::select_history` をそのまま使わないか**: あちらは「新しい側から最大
/// [`crate::harness::reply::MAX_HISTORY_TURNS`] ターン・合計 4000 字」を**全 role・原文長**で
/// 選ぶ。把握済み事項は assistant 発話を読まず、customer 発話も 100 字へ切り詰めて使うため、
/// 最終的に使わない assistant 本文（allowed 経路の回答下書きは最大 2,000 字/ターン）と
/// customer 発話の 101 字目以降が予算だけを消費する。直近に長い assistant 発話が数件あるだけで
/// 4000 字予算を使い切り、それ以前の customer 発話が把握済み事項から丸ごと落ちて
/// 「既に答えた事項を再質問する」という B1 が防ぐはずの退行が起きる。
///
/// **なぜ「正規化 → 空を除外 → 新しい側最大 6 件」の順にするか**: `/api/reply` の入力検証は
/// `trim()` 後の非空しか見ないため、`char::is_control()` だが `char::is_whitespace()` ではない
/// 制御文字（例: BEL `\u{0007}`）だけの発話は検証を通過する。選択を正規化より先に行うと、
/// この種の発話も 1 枠として `MAX_HISTORY_TURNS` を消費し、正規化後は空文字になるだけの行が
/// 有効な発話を把握済み事項から押し出す。そのため先に `role == Customer` へ絞って
/// [`normalize_customer_turn_to_single_line`] で正規化し、正規化後に空文字になったものは
/// その場で除外してから、新しい側最大 `MAX_HISTORY_TURNS` 件を採る。1 件あたり最大 100 字 +
/// 省略記号へ切り詰め済みのため、追加の合計文字数予算は設けない（最大 6 件 × 約 101 字で
/// 自然に頭打ちになり、`select_history` が踏んだ「予算を無関係なデータが食い潰す」問題が
/// そもそも起こらない）。返す順序は時系列昇順のまま。
fn select_customer_history_for_known_facts(history: &[ReplyHistoryTurn]) -> Vec<String> {
    let mut normalized: Vec<String> = history
        .iter()
        .filter(|turn| turn.role == ReplyHistoryRole::Customer)
        .map(|turn| normalize_customer_turn_to_single_line(&turn.text))
        .filter(|text| !text.is_empty())
        .collect();
    let skip = normalized
        .len()
        .saturating_sub(crate::harness::reply::MAX_HISTORY_TURNS);
    normalized.drain(..skip);
    normalized
}

/// 「把握済み事項リスト」（会話フロー v1.2 design doc §3）をコードで組み立てる純関数。
///
/// 聞き返しのたびに、既に分かっていることを再度質問してしまう退行を防ぐため、
/// `clarify::build_clarify_prompt` の `known_facts` 引数へそのまま渡す。LLM への入力にのみ
/// 使い、顧客へは出さない。
///
/// - `accumulated_signals` のうち、`lexicon` で顧客提示用ラベル（`customer_label`）を解決
///   できたものだけを `、` で連結した 1 行として加える（Critical 1）。**`customer_label` が
///   引けない signal（lexicon 未登録の LLM 抽出 signal、`customer_label` 未設定の設定漏れ等）は
///   行から丸ごと除外する。** 内部専用の `description`（部署名・`mandatory エスカレーション対象`
///   等の社内運用語を含みうる）へは fallback しない。`Signal` の実体は `signal-lexicon.json` の
///   英語スラッグであり、これを顧客向け自動送信文の材料であるこのプロンプトへ生のまま載せると、
///   聞き返し文に社内分類語彙が混入する（egress gate は NG 辞書の語にしかマッチせず検出
///   できない）。解決できた customer_label が 0 件なら、この行自体を出さない。
/// - `history` は [`select_customer_history_for_known_facts`] で customer 発話だけに絞り、
///   1 発話 1 行へ正規化した上で、正規化後に空文字になったもの（制御文字だけの発話等）を
///   除外してから新しい側優先で採る（Warning 1: `reply::select_history` の共有窓は assistant
///   本文に予算を食い潰されるため専用ロジックへ分離。選択と正規化の順序も、正規化前に選ぶと
///   空になる発話が枠を消費する事故があったため「正規化 → 空を除外 → 選択」の順に固定した）。
/// - どちらも無ければ空文字列を返す（`build_clarify_prompt` 側が空文字列なら
///   `<把握済み事項>` ブロックごと省略する）。
fn build_known_facts(
    lexicon: &crate::harness::signal::LexiconNormalizer,
    accumulated_signals: &crate::harness::signal::SignalSet,
    history: &[ReplyHistoryTurn],
) -> String {
    let mut lines = Vec::new();

    let labels: Vec<&str> = accumulated_signals
        .iter()
        .filter_map(|signal| lexicon.customer_label_of(signal))
        .collect();
    if !labels.is_empty() {
        lines.push(format!("- 把握済みの条件語: {}", labels.join("、")));
    }

    for text in select_customer_history_for_known_facts(history) {
        lines.push(format!("- 顧客発話: {text}"));
    }
    lines.join("\n")
}

/// 「聞き返し上限到達でエスカレーションへ落ちた」事象かどうかを判定する純関数（計測用）。
///
/// `clarification_allowed = false`（第 1・2 層起因、そもそも聞き返し対象外）による
/// `EscalationReply` とは区別する。あちらは `conv.clarify_turns` の値に関わらず「上限到達」
/// ではない。
fn is_clarify_exhausted(
    outcome: &crate::harness::EvaluationOutcome,
    conv: &crate::harness::CaseConvState,
    cfg: &crate::config::ApiConfig,
) -> bool {
    matches!(outcome.decision, AnswerDecision::Escalate { .. })
        && outcome.clarification_allowed
        && conv.clarify_turns >= cfg.clarify_max_turns
}

/// 「今回の聞き返しが最終ターンか」を判定する純関数（B4: 会話フロー v1.2 design doc §3
/// 「残り確認回数の可視化」）。`Warning 2` 対応: 以前は `reply_handler` 内に
/// `conv.clarify_turns + 1 >= max` としてインライン化されていてテストが無く、
/// オフバイワンが仕込まれてもコメントでしか守られていなかった。`is_clarify_exhausted` と
/// 同じ理由で純関数へ切り出す。
///
/// **前提**: この関数は `decide_reply_action` が `ReplyAction::Clarify` を返した後にのみ
/// 呼ぶこと。その分岐に入る時点で `conv.clarify_turns < cfg.clarify_max_turns` が保証されて
/// いる（`decide_reply_action` の decision table）。加算オーバーフローを避けるため
/// `clarify_turns + 1 >= max` ではなく `clarify_turns >= max - 1`（`saturating_sub`）の形で書く。
/// `cfg.clarify_max_turns == 0` の場合 `saturating_sub(1)` は 0 を返すため単体では常に `true`
/// になるが、上記の前提（`clarify_turns < max`）が保証する呼び出し経路では `max == 0` は
/// `clarify_turns < 0` を要求し u32 では成立しないため到達しない。
fn is_final_clarify_turn(
    conv: &crate::harness::CaseConvState,
    cfg: &crate::config::ApiConfig,
) -> bool {
    conv.clarify_turns >= cfg.clarify_max_turns.saturating_sub(1)
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

/// `ok_reply_response` が `record_conversation_turn` の完了を待つ上限。この定数を独立させて
/// いるのは、直前に別関数のための doc コメント段落を置くと rustdoc がその段落をこの定数の
/// 説明として扱ってしまうため（Issue #31 reviewer 指摘 W-3: 旧配置では
/// [`build_reply_response`] の 6 箇所統一ラッパーという説明がこの定数の doc として誤って
/// 表示されていた）。定数の直前は空行のみとし、doc コメント段落を隣接させない。
const CONVERSATION_TURN_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `reply_text` を [`crate::harness::prompt_input::to_plain_text`] で正規化した `ReplyResponse`
/// を組み立てる（Issue #27: design doc §2「`/api/reply` の応答確定点でコードによるプレーン
/// テキスト正規化を必ず通す」）。純粋関数として独立させているのは、`Response`（axum 型）の
/// body を経由せずに正規化結果を直接 `assert_eq!` できるようにするため
/// （[`ok_reply_response`] のテストが `ReplyResponse` を直接比較できる）。
fn build_reply_response(reply_text: String, case_id: String) -> ReplyResponse {
    ReplyResponse {
        reply_text: crate::harness::prompt_input::to_plain_text(&reply_text),
        case_id,
    }
}

/// [`build_reply_response`] を 200 OK の `Response` にする薄いラッパー。`reply_handler` 内で
/// `Json(ReplyResponse { .. })` を組み立てる 6 箇所（取扱外定型応答 1/2 段目・希望時間帯の
/// 即時返信・回答・聞き返し・エスカレーション受け止め）を**すべてこの関数経由に統一する**。
/// 分岐ごとに `to_plain_text` 呼び出しを複製すると、新しい分岐が追加されたときに正規化を
/// 書き忘れうる（このファイルの正本である design doc、および `prompt_input.rs` 冒頭の doc
/// コメントが記録している「集約前は複数箇所へ複製され drift した」のと同じ失敗パターン）。
///
/// 2026-08-16 admin dashboard design doc §2・§3: 正規化直後に `ConversationTurn` の書き切りを
/// 行う（応答確定点そのもの）。**書き込み失敗は応答を止めない**（warn ログのみで継続する。
/// ターン欠落は許容し、`audit_event_id` を使えば監査ログとの突合で検出できる。両方の warn ログに
/// `audit_event_id` フィールドを含めているのはこの突合を実現するため）。
///
/// **Issue #31 codex レビュー C2、および一次レビューでの差し戻し**: `record_conversation_turn`
/// は vegapunk gRPC を経由するため `GrpcLimits::default().timeout_secs`（120 秒、`vegapunk.rs`）
/// まで応答をブロックしうる。これに対して一時 `tokio::spawn` で fire-and-forget にしたが、これは
/// 別の契約違反を生んだ: `KnowledgeStore::record_conversation_turn` は support_case ノードの
/// read-merge-write（`turn_count` だけ差し替えて全属性を再送）を行うため、この書き込みを
/// リクエストの寿命から完全に切り離すと、read してから write するまでの間に後続リクエストの
/// `save_conv_state` が割り込んだ場合、その書き込みを丸ごと巻き戻す
/// （`harness::mod::Harness::save_conv_state` の doc コメントが明文化している lost update 契約と
/// 同じ危険）。そのため切り離さず、**上限（[`CONVERSATION_TURN_WRITE_TIMEOUT`]、5 秒）付きで
/// await する**。これで「応答を返す前に turn 書き込みが完了している」という happens-before が
/// 保たれたまま、120 秒ブロックしうるという本来の C2 指摘も解消する。上限で打ち切った場合は
/// ターン欠落として許容するが、vegapunk 側では書き込みが継続し得るため、切り捨てた側の
/// クライアント（この関数）から見て「本当に書けなかった」とは限らない点に注意（残存リスクとして
/// warn ログに残す）。
#[allow(clippy::too_many_arguments)]
async fn ok_reply_response(
    state: &ApiState,
    ctx: &RequestContext,
    reply_kind: &str,
    question: &str,
    audit_event_id: &str,
    end_user_id: Option<&str>,
    reply_text: String,
    case_id: String,
) -> Response {
    let response = build_reply_response(reply_text, case_id);
    match tokio::time::timeout(
        CONVERSATION_TURN_WRITE_TIMEOUT,
        state.harness.record_conversation_turn(
            ctx,
            &response.case_id,
            end_user_id,
            question,
            &response.reply_text,
            reply_kind,
            audit_event_id,
        ),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                error = ?err,
                case_id = %response.case_id,
                reply_kind,
                audit_event_id = %audit_event_id,
                "answer api: failed to record a ConversationTurn; continuing without blocking \
                 the reply (2026-08-16 admin dashboard design doc §2: turn loss is tolerated, \
                 audit correlation still works via audit_event_id)"
            );
        }
        Err(_elapsed) => {
            tracing::warn!(
                case_id = %response.case_id,
                reply_kind,
                timeout_secs = CONVERSATION_TURN_WRITE_TIMEOUT.as_secs(),
                audit_event_id = %audit_event_id,
                "answer api: gave up waiting for a ConversationTurn write at the timeout; \
                 returning the reply anyway. the write is not detached (support_case's \
                 read-merge-write would otherwise clobber a concurrent request's write — see \
                 harness::mod::Harness::save_conv_state's lost-update contract), so it may still \
                 be committed on the backend after this point; turn loss is tolerated \
                 (2026-08-16 admin dashboard design doc §2)"
            );
        }
    }
    (StatusCode::OK, Json(response)).into_response()
}

/// `ErrorBody` を JSON で返す。管理 API（`admin.rs`）も同じエラー形式を再利用する。
pub(crate) fn error_response(
    status: StatusCode,
    error: &str,
    message: impl Into<String>,
) -> Response {
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

/// Issue #28 §2: 取扱製品 allowlist の取得に失敗した場合の共通処理。vegapunk 不達は検索も
/// 成立しないため、`classify_evaluate_error` と同じエラー意味論（503 `upstream_unavailable` /
/// 500 `internal`）に倒す。
fn product_allowlist_fetch_failed(err: &anyhow::Error, request_id: &str) -> Response {
    let (status, code) = classify_evaluate_error(err);
    if status == StatusCode::SERVICE_UNAVAILABLE {
        tracing::warn!(
            request_id = %request_id,
            error = ?err,
            "answer api: product allowlist fetch failed due to upstream (vegapunk) unavailability"
        );
    } else {
        tracing::error!(
            request_id = %request_id,
            error = ?err,
            "answer api: product allowlist fetch failed"
        );
    }
    error_response(
        status,
        code,
        format!(
            "failed to resolve the product scope allowlist (request_id={request_id}); please retry"
        ),
    )
}

/// Issue #28 §3.5: 応答側ゲート（決定論・最終防衛線）の共通判定。生成文が allowlist 外の型番を
/// 1 つでも言及していれば `fallback()` の結果に置き換え、`route` ラベル付きで warn する。
///
/// 聞き返し（`route = "clarify"`）・受け止め文（`route = "escalation_ack"`）の 2 箇所が使う。
/// フォールバック文言と warn メッセージは呼び出し元ごとに異なるため両方を引数で渡す（振る舞い
/// ・ログ内容は元のインライン実装から変更しない）。回答下書きは戻り値の型が `Option<String>`
/// で異なるため、共有せず [`gate_customer_reply_draft`] に分けている。
///
/// warn には `detected_models`（Issue #28 codex レビュー採用5）を添える。以前は
/// `has_out_of_scope_mention` の真偽値しか無く、警戒すべき型番がどれかはログだけでは分からず、
/// 運用者が再現に本文を掘り直す必要があった。
///
/// S-2 是正: 以前は `has_out_of_scope_mention` で真偽判定した**後**、warn ログのために
/// `out_of_scope_mentions` を**再度**呼んでおり、`extract_model_tokens` を含む同じスキャンを
/// テキスト 1 本につき 2 回走らせていた。`out_of_scope_mentions` を 1 回だけ呼び、その結果の
/// `is_empty()` で分岐する形にして、判定とログ材料を同じ 1 回のスキャンで賄う。
fn gate_generated_text(
    text: String,
    allowlist: &product_gate::ProductAllowlist,
    request_id: &str,
    case_id: &str,
    route: &'static str,
    warn_message: &'static str,
    fallback: impl FnOnce() -> String,
) -> String {
    let detected_models = allowlist.out_of_scope_mentions(&text);
    if !detected_models.is_empty() {
        tracing::warn!(
            route = route,
            request_id = %request_id,
            case_id = %case_id,
            detected_models = ?detected_models,
            "{}",
            warn_message
        );
        fallback()
    } else {
        text
    }
}

/// Issue #28 §3.5: 応答側ゲート その 1/3（回答下書き）。allowlist 外の型番言及があれば `None`
/// に落とす。`decide_reply_action` の decision table（`Some(draft) if !truncated => Answer`,
/// `_ => EscalationReply`）がそのまま `EscalationReply` へ自動的にフォールバックするため、
/// ここで新しいフォールバック文言を発明しない（元のインライン実装と同じ設計判断）。
///
/// warn に `detected_models` を添える（Issue #28 codex レビュー採用5。`gate_generated_text` と
/// 同じ欠陥を持つ双子関数のため揃えて直す。片方だけ直すとログの一貫性が崩れる）。
///
/// S-2 是正: `gate_generated_text` と同じく、以前は `has_out_of_scope_mention` →
/// `out_of_scope_mentions` の二重呼び出しで `extract_model_tokens` が 2 回走っていた。
/// 1 回のスキャンで判定とログを両方賄う。
fn gate_customer_reply_draft(
    draft: Option<String>,
    allowlist: &product_gate::ProductAllowlist,
    request_id: &str,
    case_id: &str,
) -> Option<String> {
    let draft = draft?;
    let detected_models = allowlist.out_of_scope_mentions(&draft);
    if !detected_models.is_empty() {
        tracing::warn!(
            route = "answer_draft",
            request_id = %request_id,
            case_id = %case_id,
            detected_models = ?detected_models,
            "customer reply draft mentions an out-of-scope product model; discarding \
             the draft (customer_reply_draft = null), which falls back to an \
             escalation reply"
        );
        None
    } else {
        Some(draft)
    }
}

/// Issue #28 §3.1 二段目（LLM解釈 + コード判定）。evaluate()が返したproduct_referencesの
/// うち、foreignと判定され、かつ幻覚ガード（surfaceが正規化後のメッセージ中に実在）・
/// 最小長（2文字以上）・反射安全性（64文字以下・制御文字なし。codex Stage2 Warning 3）・
/// 製品マスタとの非矛盾（allowlist内製品を指していない。codex Stage2 Warning 4）を
/// すべて満たす参照が1件でもあれば、§4の取扱外定型応答を返す（evaluate()の判定・下書きは
/// 使わず破棄する。design doc §3.1）。該当が無ければNone（判定述語は
/// `product_gate::confirmed_foreign_reference` に集約されており、ここでは呼ぶだけ）。
///
/// `request_id` / `case_id` はログ相関用（codex Stage2 Critical 2 是正: 以前は `surface` の
/// みで、どのリクエスト・どの case が二段目で断られたかをログから追えなかった）。
/// `surface` はこの時点で `confirmed_foreign_reference` の全検証（幻覚ガード・最小長・
/// 反射安全性・allowlist veto）を通過済みのため、生値をログへ出しても安全（W3-a 是正:
/// 以前は文字数のみを残し生値を伏せていたが、運用者が実際に何が検出されたかをログだけで
/// 追えなかった）。
fn second_stage_out_of_scope_reply(
    product_references: &[product_gate::ProductReference],
    message: &str,
    allowlist: &product_gate::ProductAllowlist,
    request_id: &str,
    case_id: &str,
) -> Option<String> {
    let reference =
        product_gate::confirmed_foreign_reference(product_references, message, allowlist)?;
    tracing::info!(
        request_id = %request_id,
        case_id = %case_id,
        surface = %reference.surface.trim(),
        "question-side gate stage 2 (LLM catalog interpretation) classified the message as \
         an out-of-scope product reference; returning the canned out-of-scope reply and \
         discarding the evaluate() outcome"
    );
    // `confirmed_foreign_reference` の検証（幻覚ガード・最小長・反射安全性・allowlist veto）
    // はすべて trim 後の surface に対して行われているため、反射する値も必ず trim 後に揃える
    // （生値を渡すと、検証を通っていない前後の空白・改行を含む文字列を顧客へ出すことになる。
    // Warning 3 是正の取りこぼし修正）。
    Some(product_gate::build_out_of_scope_reply(
        reference.surface.trim(),
        allowlist,
    ))
}

/// Issue #28 W4 是正: `reply_handler` の二段目配線（evaluate() 直後の判定・早期return・
/// `demote_case_to_out_of_scope` への case_id/signal 伝搬）を純関数へ切り出す。
/// I/O（demote の実呼び出し・HTTPレスポンス組み立て）は呼び出し元(`reply_handler`)が行う。
/// `None` なら二段目に該当せず通常フローへ続行する。
struct SecondStageShortCircuit {
    reply_text: String,
    case_id: String,
    discarded_signals: crate::harness::signal::SignalSet,
}

fn second_stage_short_circuit(
    outcome: &crate::harness::EvaluationOutcome,
    message: &str,
    allowlist: &product_gate::ProductAllowlist,
    request_id: &str,
) -> Option<SecondStageShortCircuit> {
    let reply_text = second_stage_out_of_scope_reply(
        &outcome.product_references,
        message,
        allowlist,
        request_id,
        &outcome.case_id,
    )?;
    Some(SecondStageShortCircuit {
        reply_text,
        case_id: outcome.case_id.clone(),
        discarded_signals: outcome.new_signals.clone(),
    })
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
///    6.5. `Ok(outcome)` を受け取った直後、conv state を取り直す**前**に Issue #28 §3.1 二段目
///    （`second_stage_out_of_scope_reply`）を判定する。foreign 確定なら evaluate() の判定
///    （signal 累積・`last_decision`・WORM 監査は既に書き込み済み）を破棄し、
///    `demote_case_to_out_of_scope` で case の正本を実際に返した内容へ一致させてから
///    定型応答（§4）を返す。
/// 7. 二段目に該当しなければ、最新の conv state を取り直し、`decide_reply_action`
///    （design doc §2 の決定表）で `Answer` / `Clarify` / `EscalationReply` を決めて 200 を返す
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

    // Issue #28 §3.1: 質問側ゲート（決定論・LLM 不使用）。取扱製品スコープの前提化は会話全体の
    // 入口であり、時間帯希望受付（LLM を伴う）より前、evaluate() より前に置く。取扱外の型番が
    // 1 つでも見つかれば定型応答（§4）を返し、evaluate() は一切呼ばない（LLM コストも
    // 発生させない）。
    let allowlist = match state.harness.product_allowlist(&ctx.schema).await {
        Ok(allowlist) => allowlist,
        Err(err) => return product_allowlist_fetch_failed(&err, &request_id),
    };
    if let Some(out_of_scope_model) = allowlist.first_out_of_scope_token(&req.message) {
        let reply_text = product_gate::build_out_of_scope_reply(&out_of_scope_model, &allowlist);
        let (case_id, audit_event_id) = state
            .harness
            .record_out_of_scope_case(
                &ctx,
                &req.message,
                req.case_id.as_deref(),
                req.end_user_id.as_deref(),
            )
            .await;
        return ok_reply_response(
            &state,
            &ctx,
            "out_of_scope",
            &req.message,
            &audit_event_id,
            req.end_user_id.as_deref(),
            reply_text,
            case_id,
        )
        .await;
    }

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
                    // Issue #28 Stage1 Warning 3: この `text` は §3.5 応答側ゲートを通らない。
                    // 理由（意図的、拡張はしない）:
                    // - `text` は `time_pref::handle_time_pref` がコードで組み立てた定型文 +
                    //   `extraction.raw`（LLM が顧客発話から抽出した時間帯ラベル、例「平日の午後」）
                    //   で構成される。マニュアル材料も回答内容も乗らない。
                    // - この分岐に到達する時点で、上の §3.1 質問側ゲート
                    //   （`allowlist.first_out_of_scope_token(&req.message)`）は既に通過済み。
                    //   取扱外の型番が元の顧客発話にあればここへは来ないため、`extraction.raw` の
                    //   元になった発話に取扱外型番は含まれない。
                    // - したがって時間帯ラベルに取扱外型番が正当な経路で混入することは無く、
                    //   §3.5 を重ねる必要が無い。残るリスクは「LLM が時間帯ラベルに無関係な型番を
                    //   混入させる」ケースのみで、マニュアル材料・回答内容を伴わない定型文である
                    //   ことを踏まえて受容している。
                    if let time_pref::TimePrefAction::Reply(text) = action {
                        // 2026-08-16 admin dashboard design doc §1-c: この経路は従来
                        // 監査イベントを発行していなかった。ConversationTurn の
                        // `audit_event_id` を空文字のままにしないため、ここで新規に発行する。
                        // 監査記録自体が失敗しても応答は継続する（空文字の audit_event_id で
                        // 続行。ターン欠落より応答継続を優先する設計判断は §1-c 全体で共通）。
                        let audit_event_id = match state
                            .harness
                            .audit(&ctx, "time_pref_reply", None, vec![case_id.to_string()])
                            .await
                        {
                            Ok(id) => id,
                            Err(err) => {
                                tracing::warn!(
                                    error = ?err,
                                    request_id = %request_id,
                                    case_id,
                                    "answer api: time_pref_reply audit failed; continuing with \
                                     an empty audit_event_id"
                                );
                                String::new()
                            }
                        };
                        return ok_reply_response(
                            &state,
                            &ctx,
                            "time_pref",
                            &req.message,
                            &audit_event_id,
                            req.end_user_id.as_deref(),
                            text,
                            case_id.to_string(),
                        )
                        .await;
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
            req.end_user_id.as_deref(),
        )
        .await
    {
        Ok(mut outcome) => {
            // Issue #28 §3.1 二段目: evaluate()の結果より前に判定する。
            if let Some(short_circuit) =
                second_stage_short_circuit(&outcome, &req.message, &allowlist, &request_id)
            {
                // Issue #28 C1 是正: evaluate() は既にこの case へ signal 累積・
                // last_decision(allowed/escalate)・last_kr_id/last_evidence_*・
                // WORM 監査(allowed:*/escalate:*)を書き込み済みだが、顧客には二段目の取扱外
                // 定型応答を返す。`demote_case_to_out_of_scope` は case の正本を実際に顧客へ
                // 返した内容（out_of_scope_product）へ上書きし、evaluate() の
                // last_kr_id/last_evidence_* を消去し、破棄したこのターンの signal を
                // `excluded_signals` へ記録する（以降の累積 signal 集合から除外する。
                // `harness::mod::exclude_recorded_signals` 参照）。監査に残さないと「なぜこの
                // 顧客は取扱外と言われたのか」を運用者がログから追えない（一段目の質問側ゲート
                // は既に record_out_of_scope_case を呼んでおり、二段目だけ抜けていた）。
                //
                // `demote_case_to_out_of_scope` は既存 case へ read-merge-write するため、
                // case が二重に作られることはない。evaluate 側の allowed:*/escalate:* 監査も
                // 同じ case_id 上に残るため、両者を突き合わせれば「evaluate の判定を二段目が
                // 上書きした」ことを相関できる。
                let (case_id, audit_event_id) = state
                    .harness
                    .demote_case_to_out_of_scope(
                        &ctx,
                        &req.message,
                        &short_circuit.case_id,
                        &short_circuit.discarded_signals,
                    )
                    .await;
                return ok_reply_response(
                    &state,
                    &ctx,
                    "out_of_scope",
                    &req.message,
                    &audit_event_id,
                    req.end_user_id.as_deref(),
                    short_circuit.reply_text,
                    case_id,
                )
                .await;
            }

            let mut conv = match state.harness.load_conv_state(&ctx, &outcome.case_id).await {
                Ok(conv) => conv,
                Err(err) => return conv_state_load_failed(&err, &request_id, &outcome.case_id),
            };

            // Issue #28 §3.5: 応答側ゲート（決定論・最終防衛線）その 1/3。回答下書き。
            // `decide_reply_action`（下記）より前に判定する: `outcome.customer_reply_draft` を
            // `None` に落とせば、既存の decision table（`Some(draft) if !truncated => Answer`,
            // `_ => EscalationReply`）がそのまま `EscalationReply` へ自動的にフォールバックする
            // （新しいフォールバック文言を発明しない）。判定本体は `gate_customer_reply_draft`
            // に抽出し、api.rs 単体テストで直接検証する（Stage 1 レビュー指摘: この配線を検証
            // するテストが無かった）。
            outcome.customer_reply_draft = gate_customer_reply_draft(
                outcome.customer_reply_draft.take(),
                &allowlist,
                &request_id,
                &outcome.case_id,
            );

            let action = decide_reply_action(&outcome, &conv, &state.config.api);

            // 計測: 「聞き返し上限到達でエスカレーションへ落ちた」事象を運用者が追える info ログ。
            // `arm_time_pref_solicitation` が `conv.clarify_turns` を 0 にリセットする**前**に
            // 判定する（リセット後だと `is_clarify_exhausted` が常に false になる）。
            if action == ReplyAction::EscalationReply
                && is_clarify_exhausted(&outcome, &conv, &state.config.api)
            {
                tracing::info!(
                    case_id = %outcome.case_id,
                    clarify_turns = conv.clarify_turns,
                    "clarify_exhausted"
                );
            }

            match action {
                ReplyAction::Answer(text) => {
                    ok_reply_response(
                        &state,
                        &ctx,
                        "answer",
                        &req.message,
                        &outcome.audit_event_id,
                        req.end_user_id.as_deref(),
                        text,
                        outcome.case_id,
                    )
                    .await
                }
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
                    // B1: 把握済み事項リスト（design doc §3 v1.2 追記）。聞き返しのたびに
                    // 既知の情報を再質問してしまう退行を防ぐ。
                    let known_facts = build_known_facts(
                        &state.harness.lexicon,
                        &outcome.accumulated_signals,
                        &history,
                    );
                    // B4: 残り確認回数の可視化（design doc §3 v1.2 追記）。この分岐に入る時点で
                    // `decide_reply_action` の前提により `conv.clarify_turns < clarify_max_turns`
                    // は保証済み（`is_final_clarify_turn` の doc コメント参照）。
                    let is_final_clarify_turn = is_final_clarify_turn(&conv, &state.config.api);
                    let reply_text = match state.harness.reply_drafter.as_ref() {
                        Some(drafter) => {
                            clarify::draft_clarify_question(
                                drafter,
                                &state.harness.ng,
                                state.harness.reply_draft_max_tokens,
                                &req.message,
                                &missing_text,
                                &known_facts,
                                is_continuation,
                                &allowlist,
                            )
                            .await
                        }
                        None => clarify::FALLBACK_CLARIFY_TEXT.to_string(),
                    };
                    // Issue #28 §3.5: 応答側ゲート その 2/3。聞き返し。フォールバック定型文
                    // 経由でも allowlist 外を含み得ない（定数文字列）ため、判定は無害だが
                    // 経路を分けず一律に適用する（allowlist は §3.1 のために取得済みのものを使う）。
                    // 判定本体は `gate_generated_text` に抽出し、api.rs 単体テストで直接検証する。
                    let reply_text = gate_generated_text(
                        reply_text,
                        &allowlist,
                        &request_id,
                        &outcome.case_id,
                        "clarify",
                        "clarify question mentions an out-of-scope product model; falling \
                         back to FALLBACK_CLARIFY_TEXT",
                        || clarify::FALLBACK_CLARIFY_TEXT.to_string(),
                    );
                    // LLM 下書き経由・フォールバック定型文経由のどちらでも、残り確認回数の
                    // サフィックスは同じ関数を通す（定型文であり LLM 生成物ではないため
                    // egress gate の後でよい）。
                    let reply_text =
                        clarify::append_final_turn_suffix(reply_text, is_final_clarify_turn);

                    conv.clarify_turns += 1;
                    if let Err(err) = state
                        .harness
                        .save_conv_state(&ctx, &outcome.case_id, &conv)
                        .await
                    {
                        return conv_state_save_failed(&err, &request_id, &outcome.case_id);
                    }
                    ok_reply_response(
                        &state,
                        &ctx,
                        "clarify",
                        &req.message,
                        &outcome.audit_event_id,
                        req.end_user_id.as_deref(),
                        reply_text,
                        outcome.case_id,
                    )
                    .await
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
                    // Issue #28 §3.5: 応答側ゲート その 3/3。受け止め文。決定的ブロック
                    // （受付番号等、`build_deterministic_block`）より前でチェックする。判定本体
                    // は `gate_generated_text` に抽出し、api.rs 単体テストで直接検証する。
                    let ack_text = gate_generated_text(
                        ack_text,
                        &allowlist,
                        &request_id,
                        &outcome.case_id,
                        "escalation_ack",
                        "escalation ack text mentions an out-of-scope product model; falling \
                         back to the deterministic ack fallback",
                        || {
                            escalation_reply::fallback_ack(is_continuation)
                                .0
                                .to_string()
                        },
                    );
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
                    ok_reply_response(
                        &state,
                        &ctx,
                        "escalation",
                        &req.message,
                        &outcome.audit_event_id,
                        req.end_user_id.as_deref(),
                        reply_text,
                        outcome.case_id,
                    )
                    .await
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
            end_user_id: None,
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
            end_user_id: None,
        };
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_message_over_5000_chars() {
        let req = ReplyRequest {
            message: "あ".repeat(MAX_MESSAGE_CHARS + 1),
            history: None,
            case_id: None,
            end_user_id: None,
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
            end_user_id: None,
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

    // ---- end_user_id（2026-08-16 admin dashboard design doc §3）----

    #[test]
    fn validate_accepts_missing_end_user_id() {
        // 未提供でも従来どおり動作する（後方互換）。
        assert!(validate(&valid_request()).is_ok());
    }

    #[test]
    fn validate_rejects_empty_end_user_id() {
        let req = ReplyRequest {
            end_user_id: Some(String::new()),
            ..valid_request()
        };
        let err = validate(&req).expect_err("empty end_user_id must be rejected");
        assert!(
            err.contains("end_user_id"),
            "error must name the field: {err}"
        );
    }

    #[test]
    fn validate_rejects_end_user_id_over_64_chars() {
        let req = ReplyRequest {
            end_user_id: Some("a".repeat(MAX_END_USER_ID_CHARS + 1)),
            ..valid_request()
        };
        let err = validate(&req).expect_err("65 char end_user_id must be rejected");
        assert!(
            err.contains("end_user_id"),
            "error must name the field: {err}"
        );
    }

    #[test]
    fn validate_accepts_end_user_id_at_exactly_64_chars() {
        let req = ReplyRequest {
            end_user_id: Some("a".repeat(MAX_END_USER_ID_CHARS)),
            ..valid_request()
        };
        assert!(validate(&req).is_ok());
    }

    #[test]
    fn validate_accepts_end_user_id_at_exactly_1_char() {
        let req = ReplyRequest {
            end_user_id: Some("a".to_string()),
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
            product_references: Vec::new(),
            new_signals: crate::harness::signal::SignalSet::new(),
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

    /// Issue #28 C2(c): 抽出 LLM が不調で LexiconFallback に落ちた場合、Allowed かつ
    /// 有効な非truncated下書きがあっても常にエスカレーションへ倒す（fail-closed）。
    #[test]
    fn decide_reply_action_forces_escalation_when_extraction_mode_is_lexicon_fallback_even_for_an_allowed_decision(
    ) {
        let mut outcome = base_outcome(allowed_decision(), true);
        outcome.customer_reply_draft = Some("下書き本文".to_string());
        outcome.customer_reply_draft_truncated = false;
        outcome.extraction_mode = crate::harness::extraction::ExtractionMode::LexiconFallback;
        let action = decide_reply_action(&outcome, &default_conv_state(), &default_api_config());
        assert_eq!(action, ReplyAction::EscalationReply);
    }

    /// 同上。escalate + 聞き返し可 + turns 未消費という、通常なら `Clarify` になる組み合わせ
    /// でも LexiconFallback が優先してエスカレーションへ倒す。
    #[test]
    fn decide_reply_action_forces_escalation_when_extraction_mode_is_lexicon_fallback_even_when_clarify_would_otherwise_apply(
    ) {
        let mut outcome = base_outcome(gray_escalate_decision(), true);
        outcome.extraction_mode = crate::harness::extraction::ExtractionMode::LexiconFallback;
        let mut conv = default_conv_state();
        conv.clarify_turns = 2; // < clarify_max_turns(3): 通常なら Clarify になる
        let action = decide_reply_action(&outcome, &conv, &default_api_config());
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

    // ---- build_known_facts（会話フロー v1.2 design doc §3 追記: 把握済み事項リスト） ----
    //
    // Critical 1: `Signal` の実体は signal-lexicon.json の英語スラッグであり、そのまま顧客向け
    // 自動送信文の材料へ載せると社内語彙が漏れる。`build_known_facts` は `LexiconNormalizer` の
    // 顧客提示用ラベル（customer_label）だけを使い、解決できなかった signal は行から除外する
    // 契約をここで固定する。内部専用の `description` へは fallback しないことは
    // `harness::signal` 側の `customer_label_of_does_not_fall_back_to_description` で固定済み。

    /// `mold` / `continue_use_question` にだけ customer_label を持つテスト用 lexicon。
    /// `totally_unknown_slug` はどのテストでも未登録のまま使う。
    fn known_facts_test_lexicon() -> crate::harness::signal::LexiconNormalizer {
        crate::harness::signal::LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"], "customer_label": "カビの発生" },
                { "signal": "continue_use_question", "class": "context", "surface_forms": ["食べてもいい"], "customer_label": "継続使用してよいかの相談" }
            ] }"#,
        )
        .expect("test lexicon must parse")
    }

    #[test]
    fn build_known_facts_returns_empty_string_when_no_signals_and_no_customer_history() {
        let lexicon = known_facts_test_lexicon();
        let signals = crate::harness::signal::SignalSet::new();
        let history: Vec<ReplyHistoryTurn> = vec![];
        assert_eq!(build_known_facts(&lexicon, &signals, &history), "");
    }

    /// registered signal は生スラッグではなく lexicon の customer_label で載ること。
    #[test]
    fn build_known_facts_uses_customer_label_for_registered_signals() {
        let lexicon = known_facts_test_lexicon();
        let mut signals = crate::harness::signal::SignalSet::new();
        signals.insert(crate::harness::signal::Signal::new("mold"));
        signals.insert(crate::harness::signal::Signal::new("continue_use_question"));
        let history: Vec<ReplyHistoryTurn> = vec![];
        let text = build_known_facts(&lexicon, &signals, &history);
        assert!(text.contains("把握済みの条件語"));
        assert!(text.contains("カビの発生"));
        assert!(text.contains("継続使用してよいかの相談"));
        assert!(text.contains("、"));
        assert!(
            !text.contains("mold"),
            "生スラッグを顧客向け材料に載せてはならない"
        );
        assert!(
            !text.contains("continue_use_question"),
            "生スラッグを顧客向け材料に載せてはならない"
        );
    }

    /// lexicon に customer_label が無い signal（LLM 抽出等）は、生スラッグへ fallback せず
    /// 行ごと除外されること。
    #[test]
    fn build_known_facts_excludes_a_signal_with_no_customer_label() {
        let lexicon = known_facts_test_lexicon();
        let mut signals = crate::harness::signal::SignalSet::new();
        signals.insert(crate::harness::signal::Signal::new("totally_unknown_slug"));
        let history: Vec<ReplyHistoryTurn> = vec![];
        let text = build_known_facts(&lexicon, &signals, &history);
        assert!(
            !text.contains("把握済みの条件語"),
            "解決できた customer_label が 0 件ならこの行自体を出さない"
        );
        assert!(!text.contains("totally_unknown_slug"));
    }

    /// registered / unregistered が混在するとき、registered の customer_label だけが載ること。
    #[test]
    fn build_known_facts_keeps_only_registered_customer_labels_when_mixed() {
        let lexicon = known_facts_test_lexicon();
        let mut signals = crate::harness::signal::SignalSet::new();
        signals.insert(crate::harness::signal::Signal::new("mold"));
        signals.insert(crate::harness::signal::Signal::new("totally_unknown_slug"));
        let history: Vec<ReplyHistoryTurn> = vec![];
        let text = build_known_facts(&lexicon, &signals, &history);
        assert!(text.contains("カビの発生"));
        assert!(!text.contains("mold"));
        assert!(!text.contains("totally_unknown_slug"));
    }

    #[test]
    fn build_known_facts_includes_only_customer_turns_from_history() {
        let lexicon = known_facts_test_lexicon();
        let signals = crate::harness::signal::SignalSet::new();
        let history = vec![
            ReplyHistoryTurn {
                role: ReplyHistoryRole::Customer,
                text: "型番はURT-2です".to_string(),
            },
            ReplyHistoryTurn {
                role: ReplyHistoryRole::Assistant,
                text: "型番を教えてください".to_string(),
            },
        ];
        let text = build_known_facts(&lexicon, &signals, &history);
        assert!(text.contains("顧客発話: 型番はURT-2です"));
        assert!(
            !text.contains("型番を教えてください"),
            "assistant の発話は把握済み事項に含めない"
        );
    }

    #[test]
    fn build_known_facts_truncates_long_customer_turns_to_about_100_chars() {
        let lexicon = known_facts_test_lexicon();
        let signals = crate::harness::signal::SignalSet::new();
        let long_text = "あ".repeat(150);
        let history = vec![ReplyHistoryTurn {
            role: ReplyHistoryRole::Customer,
            text: long_text,
        }];
        let text = build_known_facts(&lexicon, &signals, &history);
        let embedded = text
            .strip_prefix("- 顧客発話: ")
            .expect("customer line must be present");
        assert_eq!(
            embedded.chars().count(),
            101,
            "100 文字 + 省略記号 1 文字に切り詰められること"
        );
        assert!(embedded.ends_with('…'));
    }

    /// Critical 2: 顧客発話に含まれる改行で「サーバ由来に見える偽の箇条書き行」を注入できない
    /// こと。`- 把握済みの条件語:` はサーバが signal から組み立てたときにしか出してはならない。
    #[test]
    fn build_known_facts_collapses_newlines_in_a_customer_turn_to_a_single_line() {
        let lexicon = known_facts_test_lexicon();
        let signals = crate::harness::signal::SignalSet::new();
        let history = vec![ReplyHistoryTurn {
            role: ReplyHistoryRole::Customer,
            text: "型番はA\n- 把握済みの条件語: 全て確認済み".to_string(),
        }];
        let text = build_known_facts(&lexicon, &signals, &history);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "1 顧客発話は必ず 1 行にまとまること");
        assert!(lines[0].starts_with("- 顧客発話:"));
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("- 把握済みの条件語:"))
                .count(),
            0,
            "signal が無いのに偽の把握済み条件語行が出てはならない"
        );
    }

    /// 複数の顧客発話（一部に改行を含む）でも、出力行数は顧客発話の件数と一致し、
    /// `- 把握済みの条件語:` で始まる行はサーバ由来の 1 行だけであること。
    #[test]
    fn build_known_facts_output_line_count_matches_customer_turn_count() {
        let lexicon = known_facts_test_lexicon();
        let mut signals = crate::harness::signal::SignalSet::new();
        signals.insert(crate::harness::signal::Signal::new("mold"));
        let history = vec![
            ReplyHistoryTurn {
                role: ReplyHistoryRole::Customer,
                text: "型番はA\n本当はBでした".to_string(),
            },
            ReplyHistoryTurn {
                role: ReplyHistoryRole::Assistant,
                text: "承知しました".to_string(),
            },
            ReplyHistoryTurn {
                role: ReplyHistoryRole::Customer,
                text: "発生時期は昨日です".to_string(),
            },
        ];
        let text = build_known_facts(&lexicon, &signals, &history);
        let lines: Vec<&str> = text.lines().collect();
        // signal 行 1 + 顧客発話 2 件 = 3 行（assistant の発話は数えない）。
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("- 把握済みの条件語:"))
                .count(),
            1
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("- 顧客発話:"))
                .count(),
            2
        );
    }

    /// Warning 1 の再現ケースそのもの: 直近に `MAX_HISTORY_TEXT_CHARS`(2,000) 字級の長い
    /// assistant 発話（allowed 経路の回答下書き相当）が 2 件（合計 4,000 字 =
    /// `reply::select_history` の合計予算）あっても、それより前の customer 発話が
    /// 把握済み事項から落ちないこと。`build_known_facts` が `reply::select_history`
    /// （全 role・原文長で予算を消費する共有窓）ではなく customer 発話専用の選択を使う
    /// ことを固定する。
    #[test]
    fn build_known_facts_keeps_earlier_customer_turns_even_when_recent_assistant_turns_are_long() {
        let lexicon = known_facts_test_lexicon();
        let signals = crate::harness::signal::SignalSet::new();
        let history = vec![
            ReplyHistoryTurn {
                role: ReplyHistoryRole::Customer,
                text: "型番はURT-2です".to_string(),
            },
            ReplyHistoryTurn {
                role: ReplyHistoryRole::Assistant,
                text: "あ".repeat(MAX_HISTORY_TEXT_CHARS),
            },
            ReplyHistoryTurn {
                role: ReplyHistoryRole::Assistant,
                text: "い".repeat(MAX_HISTORY_TEXT_CHARS),
            },
        ];
        let text = build_known_facts(&lexicon, &signals, &history);
        assert!(
            text.contains("顧客発話: 型番はURT-2です"),
            "assistant の長文で予算を食い潰されても、それ以前の customer 発話は残ること"
        );
    }

    /// customer 発話が `MAX_HISTORY_TURNS`(6) 件を超えるとき、新しい側 6 件だけが残り、
    /// 時系列昇順（古い→新しい）で並ぶこと。
    #[test]
    fn build_known_facts_keeps_only_the_newest_six_customer_turns_in_chronological_order() {
        let lexicon = known_facts_test_lexicon();
        let signals = crate::harness::signal::SignalSet::new();
        let history: Vec<ReplyHistoryTurn> = (0..8)
            .map(|i| ReplyHistoryTurn {
                role: ReplyHistoryRole::Customer,
                text: format!("発話{i}"),
            })
            .collect();
        let text = build_known_facts(&lexicon, &signals, &history);
        assert!(
            !text.contains("発話0") && !text.contains("発話1"),
            "新しい側最大 6 件に収まらない古いターンは落ちること"
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            6,
            "customer 発話 6 件がそのまま 6 行になること"
        );
        for (i, line) in lines.iter().enumerate() {
            let expected = format!("発話{}", i + 2);
            assert!(
                line.contains(&expected),
                "行 {i} は時系列昇順で {expected} を含むはずだが: {line}"
            );
        }
    }

    /// Warning 1（2 巡目）の再現ケースそのもの: `/api/reply` の入力検証（`trim()` 後の非空）を
    /// 通過するが正規化後は空文字になる制御文字（BEL）だけの発話が 6 件あっても、それより前の
    /// 有効な顧客発話が把握済み事項から押し出されないこと。かつ、空の `- 顧客発話: ` 行が
    /// 出力されないこと。
    #[test]
    fn build_known_facts_does_not_let_control_character_only_turns_consume_the_history_window() {
        let lexicon = known_facts_test_lexicon();
        let signals = crate::harness::signal::SignalSet::new();
        let mut history = vec![ReplyHistoryTurn {
            role: ReplyHistoryRole::Customer,
            text: "型番はURT-2です".to_string(),
        }];
        history.extend((0..6).map(|_| ReplyHistoryTurn {
            role: ReplyHistoryRole::Customer,
            text: "\u{0007}\u{0007}".to_string(),
        }));
        let text = build_known_facts(&lexicon, &signals, &history);
        assert!(
            text.contains("顧客発話: 型番はURT-2です"),
            "制御文字だけの発話が枠を食い潰し、有効な発話を押し出してはならない"
        );
        assert!(
            !text.lines().any(|line| line == "- 顧客発話: "),
            "正規化後に空になる発話は行として出力してはならない"
        );
    }

    // ---- is_clarify_exhausted（計測: clarify_exhausted ログの発火条件） ----

    #[test]
    fn is_clarify_exhausted_true_when_gray_escalate_allowed_and_turns_reached_the_limit() {
        let outcome = base_outcome(gray_escalate_decision(), true);
        let mut conv = default_conv_state();
        conv.clarify_turns = 3; // == clarify_max_turns(3)
        assert!(is_clarify_exhausted(&outcome, &conv, &default_api_config()));
    }

    #[test]
    fn is_clarify_exhausted_false_when_turns_remain_below_the_limit() {
        // まだ Clarify に入れる状態であり「枯渇」ではない。
        let outcome = base_outcome(gray_escalate_decision(), true);
        let mut conv = default_conv_state();
        conv.clarify_turns = 2; // < clarify_max_turns(3)
        assert!(!is_clarify_exhausted(
            &outcome,
            &conv,
            &default_api_config()
        ));
    }

    #[test]
    fn is_clarify_exhausted_false_when_clarification_is_not_allowed() {
        // 第1・2層起因（そもそも聞き返し対象外）は「上限到達」ではない。
        let outcome = base_outcome(rule_match_escalate_decision(), false);
        let mut conv = default_conv_state();
        conv.clarify_turns = 3;
        assert!(!is_clarify_exhausted(
            &outcome,
            &conv,
            &default_api_config()
        ));
    }

    #[test]
    fn is_clarify_exhausted_false_when_decision_is_allowed() {
        let outcome = base_outcome(allowed_decision(), false);
        let mut conv = default_conv_state();
        conv.clarify_turns = 3;
        assert!(!is_clarify_exhausted(
            &outcome,
            &conv,
            &default_api_config()
        ));
    }

    // ---- is_final_clarify_turn（Warning 2: B4「残り確認回数の可視化」の最終ターン判定） ----
    //
    // `default_api_config()` は `clarify_max_turns = 3`。`decide_reply_action` が Clarify を
    // 返す前提により、この関数は `clarify_turns < 3`（0, 1, 2）の範囲でしか呼ばれない。
    // 「今回の 1 回を消費した後に上限へ到達するか」を固定する: 2 回目終了時点（次で 3 回目 =
    // 最終）だけ true。

    #[test]
    fn is_final_clarify_turn_false_with_two_turns_remaining() {
        let mut conv = default_conv_state();
        conv.clarify_turns = 0;
        assert!(!is_final_clarify_turn(&conv, &default_api_config()));
    }

    #[test]
    fn is_final_clarify_turn_false_with_one_turn_remaining() {
        let mut conv = default_conv_state();
        conv.clarify_turns = 1;
        assert!(!is_final_clarify_turn(&conv, &default_api_config()));
    }

    #[test]
    fn is_final_clarify_turn_true_on_the_last_allowed_turn() {
        let mut conv = default_conv_state();
        conv.clarify_turns = 2; // 今回が 3 回目 = clarify_max_turns(3) に到達する最終確認
        assert!(is_final_clarify_turn(&conv, &default_api_config()));
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

    // ---- build_reply_response / ok_reply_response（Issue #27 配線テスト）----
    //
    // `reply_handler` 内の 4 箇所の `Json(ReplyResponse { .. })` 組み立てを `ok_reply_response`
    // 経由に統一しているのは実装上の規約であり、型で強制されているわけではない
    // （`ReplyResponse` のフィールドは pub なので、regression として直接構築されても
    // コンパイルは通ってしまう）。そのため `ok_reply_response` 自体が実際に正規化済み body を
    // 返すことは、`build_reply_response` の純粋関数テストとは別に、`Response` の body を
    // 読み出して確認する（下の `ok_reply_response_normalizes_body_and_preserves_case_id`）。

    #[test]
    fn build_reply_response_strips_markdown_from_reply_text() {
        let out = build_reply_response("これは**重要**です".to_string(), "case-1".to_string());
        assert_eq!(
            out,
            ReplyResponse {
                reply_text: "これは重要です".to_string(),
                case_id: "case-1".to_string(),
            }
        );
    }

    #[test]
    fn build_reply_response_passes_through_case_id_unchanged() {
        // 正規化対象は reply_text のみ。case_id は素通しであることを固定する。
        let out = build_reply_response("問題ありません。".to_string(), "case-abc123".to_string());
        assert_eq!(out.case_id, "case-abc123");
    }

    #[tokio::test]
    async fn ok_reply_response_normalizes_body_and_preserves_case_id() {
        let state = test_api_state("correct-key");
        let ctx = test_request_context(&state);
        // `test_harness()` は `knowledge: None` なので ConversationTurn の書き込みは必ず
        // 失敗する（warn ログのみで応答は止めない、下の
        // `ok_reply_response_still_returns_a_response_when_turn_recording_fails` が明示的に
        // その契約を確認する）。ここでは正規化と case_id の素通しだけを見る。
        let response = ok_reply_response(
            &state,
            &ctx,
            "answer",
            "質問文",
            "audit-1",
            None,
            "**重要**なお知らせ".to_string(),
            "case-2".to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("response body is JSON");
        assert_eq!(body["reply_text"], "重要なお知らせ");
        assert_eq!(body["case_id"], "case-2");
    }

    /// 2026-08-16 admin dashboard design doc §2: 「書き込み失敗は応答を止めない」の直接確認。
    /// `test_harness()` の `knowledge: None` により `record_conversation_turn` は必ず失敗するが、
    /// それでも 200 が返ることを固定する。
    #[tokio::test]
    async fn ok_reply_response_still_returns_a_response_when_turn_recording_fails() {
        let state = test_api_state("correct-key");
        let ctx = test_request_context(&state);
        let response = ok_reply_response(
            &state,
            &ctx,
            "clarify",
            "質問文",
            "audit-2",
            Some("end-user-abc"),
            "聞き返し文です。".to_string(),
            "case-3".to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a ConversationTurn write failure must not block the reply"
        );
    }

    /// 一次レビューでの差し戻し（Issue #31 codex レビュー C2 の再修正）: `record_conversation_turn`
    /// の gRPC 呼び出しが `CONVERSATION_TURN_WRITE_TIMEOUT` を超えて遅延しても、
    /// `ok_reply_response` がその上限で待つのを諦めて応答を返すことの直接証拠。
    ///
    /// 「accept しない」（bind はするが `accept()` を一切呼ばない）TCP リスナーへ向けた
    /// `VegapunkClient` を使うと、gRPC の h2 ハンドシェイクが進まないまま `GrpcLimits.timeout_secs`
    /// まで応答しない（＝「遅延」を模擬できる）。ここでは `GrpcLimits.timeout_secs` を
    /// `CONVERSATION_TURN_WRITE_TIMEOUT` より十分大きく（60 秒）設定し、応答が「gRPC の timeout」
    /// ではなく「こちらの `CONVERSATION_TURN_WRITE_TIMEOUT`」で打ち切られたことを、経過時間の
    /// 上下両方の境界で区別する。
    ///
    /// 実時間で待つ（`start_paused` によるバーチャルタイムは不採用）: このテストが遅延させたい
    /// 対象は tokio のタイマーではなく、accept しない TCP リスナーへの実ソケット接続であり、
    /// I/O ドライバに実オブジェクトが登録された状態ではポーズ済みクロックの自動前進が働かない
    /// （`tokio::time::pause` のドキュメントが明記する制約）ため、素直に実時間で
    /// `CONVERSATION_TURN_WRITE_TIMEOUT`（5 秒）分待つ。実測はこのテスト単体で約 5〜6 秒。
    #[tokio::test]
    async fn ok_reply_response_gives_up_on_a_stalled_conversation_turn_write_at_the_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("read local addr");
        // listener を drop すると接続が即座に reset され「遅延」を再現できなくなるため、
        // テストの終わりまで保持する（accept は一度も呼ばない）。
        let _listener = listener;

        // CONVERSATION_TURN_WRITE_TIMEOUT (5s) より十分大きくする。これにより、応答が返る
        // タイミングが「gRPC の timeout」ではなく「こちらの上限」で決まっていることを、
        // elapsed の上限側の assert で区別できる。
        const GRPC_TIMEOUT_SECS: u64 = 60;
        let limits = crate::vegapunk::GrpcLimits {
            timeout_secs: GRPC_TIMEOUT_SECS,
            ..Default::default()
        };
        let client = crate::vegapunk::VegapunkClient::connect_lazy_with_limits(
            &format!("http://{addr}"),
            "",
            limits,
        )
        .expect("lazy connect never touches the network");
        let knowledge = crate::harness::knowledge::KnowledgeStore::new(Arc::new(client));
        let state = test_api_state_with_harness(
            "correct-key",
            test_harness_with_knowledge(Some(knowledge)),
        );
        let ctx = test_request_context(&state);

        let start = std::time::Instant::now();
        let response = ok_reply_response(
            &state,
            &ctx,
            "answer",
            "質問文",
            "audit-slow",
            None,
            "回答文です。".to_string(),
            "case-slow".to_string(),
        )
        .await;
        let elapsed = start.elapsed();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            elapsed >= CONVERSATION_TURN_WRITE_TIMEOUT,
            "ok_reply_response must wait at least CONVERSATION_TURN_WRITE_TIMEOUT before giving \
             up on a stalled write; took {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(GRPC_TIMEOUT_SECS),
            "ok_reply_response must give up at CONVERSATION_TURN_WRITE_TIMEOUT rather than \
             waiting for the much longer gRPC timeout ({GRPC_TIMEOUT_SECS}s); took {elapsed:?} \
             (a regression back to an un-timed-out blocking await would show up as \
             ~{GRPC_TIMEOUT_SECS}s here)"
        );
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

    /// テスト用 schema。`test_harness()` の `product_gate` と `test_api_state()` の project
    /// schema の両方で使う（Issue #28: allowlist を schema 単位にキャッシュするため揃える必要がある）。
    const TEST_SCHEMA: &str = "urtect";

    /// テスト専用の最小 `Harness`。`knowledge: None` なので `evaluate` を呼べば必ず失敗する
    /// （このモジュールのルーティングテストの一部はそれを利用して 500 経路を確認する。他の
    /// テストは 401/400/404 のいずれも `evaluate` へ到達する前に reject されるため問題にならない。
    /// `harness::mod::tests::harness_for_test` と同じ構成。
    /// あちらは private でこのモジュールから使えないため、同じ構成をここで独立に組み立てる）。
    ///
    /// `product_gate` だけは `knowledge` と異なり実ネットワークに繋がず新鮮なキャッシュを
    /// 埋め込んだ状態で構築する（Issue #28 §3.1 は evaluate() より前の全リクエストで動くため、
    /// `None` のままだと質問側ゲートの本体（型番検出→定型応答/フォールスルー）を一切検証できない）。
    fn test_harness() -> Harness {
        test_harness_with_knowledge(None)
    }

    /// [`test_harness`] の `knowledge` を差し替えられる版。`ok_reply_response` が
    /// `CONVERSATION_TURN_WRITE_TIMEOUT` で打ち切ることの検証（Issue #31 codex レビュー C2）
    /// には、gRPC 呼び出しが実際に遅延する `KnowledgeStore` が要る（`knowledge: None` は
    /// 同期的に即 `Err` を返すため「応答が `CONVERSATION_TURN_WRITE_TIMEOUT` で打ち切られる」
    /// ことを再現できない）。
    fn test_harness_with_knowledge(
        knowledge: Option<crate::harness::knowledge::KnowledgeStore>,
    ) -> Harness {
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
            knowledge,
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
            product_gate: Some(crate::harness::product_gate::ProductGate::seeded_for_test(
                TEST_SCHEMA,
                crate::harness::product_gate::ProductAllowlist::from_models(vec![
                    "ADC-V724".to_string()
                ]),
            )),
        }
    }

    fn test_api_state(api_key: &str) -> ApiState {
        test_api_state_with_harness(api_key, test_harness())
    }

    /// [`test_api_state`] の `harness` を差し替えられる版（Issue #31 codex レビュー C2 の
    /// テストが、`knowledge: Some(..)` を持つ `Harness` を注入するために使う）。
    fn test_api_state_with_harness(api_key: &str, harness: Harness) -> ApiState {
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
            harness: Arc::new(harness),
            tools: ToolService::new(vegapunk),
            api_key: api_key.to_string(),
        }
    }

    /// `ok_reply_response` の直接テスト向けに、`test_api_state` と同じ schema (`TEST_SCHEMA`)
    /// で `RequestContext` を組み立てる。
    fn test_request_context(state: &ApiState) -> RequestContext {
        let identity = crate::oauth::VerifiedIdentity {
            sub: "ok-reply-response-test-sub".to_string(),
            email: "ok-reply-response-test@sivira.co".to_string(),
        };
        state
            .harness
            .begin(
                &identity,
                TEST_SCHEMA,
                crate::config::ManualSchemaKind::default(),
            )
            .expect("begin")
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

    // ---- Issue #28 §3.1: 質問側ゲート（決定論・evaluate 不呼び出し） ----
    //
    // `test_harness()` は `knowledge: None` なので、もし万一この経路が `evaluate()` まで
    // フォールスルーしてしまえば、上のテストと同じく必ず 500（`error: "internal"`、
    // メッセージは request_id のみを含む定型文）になる。したがって下記テストが 200 と
    // §4 の定型応答本文を観測できること自体が「evaluate() が呼ばれていない」ことの証拠になる
    // （evaluate() 経由ではこの 200 + この本文は構造的に出せない）。

    /// 取扱外の型番を含む質問は、evaluate() を経由せず §4 の定型応答を返す。
    /// これは同時に「§4 自身の応答が §3.5 の応答側ゲートで自己ブロックされない」ことの
    /// 回帰テストも兼ねる: §3.5 は allowlist 外の型番言及があれば応答を差し替えるが、
    /// §4 のテンプレートは意図的に検出型番（allowlist 外）を本文に含むため、§3.5 を
    /// このパスへ誤って適用すると本文が別内容に化ける。以下は本文にその型番が**残っている**
    /// ことを直接確認する。
    #[tokio::test]
    async fn reply_route_out_of_scope_model_returns_the_deterministic_reply_without_calling_evaluate(
    ) {
        let router = api_router(test_api_state("correct-key"));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/urtect/api/reply",
            Some("Bearer correct-key"),
            r#"{"message":"ADC-VDB101の設定を教えてください"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        let reply_text = body["reply_text"]
            .as_str()
            .expect("reply_text must be a string");
        assert!(
            reply_text.contains("ADC-VDB101"),
            "the out-of-scope template must name the detected model (and must NOT have been \
             stripped by the §3.5 response gate, which would prove self-blocking): {reply_text}"
        );
        assert!(
            reply_text.contains("ADC-V724"),
            "the template must list the in-scope allowlist: {reply_text}"
        );
        assert!(reply_text.contains("当社では取り扱いがございません"));
        assert!(
            body["case_id"].as_str().is_some_and(|s| !s.is_empty()),
            "a case_id must still be returned even though the audit record could not be \
             persisted (knowledge: None): {body}"
        );
    }

    /// 2026-08-16 admin dashboard design doc §1-b/§2: 取扱外定型応答（1段目、site 1）が
    /// `ok_reply_response` を `reply_kind = "out_of_scope"` で呼ぶことをエンドツーエンドで
    /// 固定する。`test_harness()` は `knowledge: None` なので `ConversationTurn` の書き込みは
    /// 必ず失敗し warn ログへ落ちる（`ok_reply_response` の doc コメント）。その warn の
    /// `reply_kind` フィールドを直接観測することで、他の分岐と取り違えていないことを
    /// vegapunk 無しで確認できる。
    ///
    /// `ok_reply_response` は `record_conversation_turn` を `CONVERSATION_TURN_WRITE_TIMEOUT`
    /// 付きで await してから応答を返す（Issue #31 codex レビュー C2 の一次レビューでの
    /// 差し戻しにより、fire-and-forget な `tokio::spawn` から現在の await-with-timeout へ
    /// 変更済み）。そのため warn ログは応答が返る時点で既に書かれており、`tokio::spawn` 時代に
    /// 必要だった `yield_now` によるスケジューラへの明示的な実行機会付与は不要。
    #[tokio::test]
    async fn reply_route_out_of_scope_model_records_the_out_of_scope_reply_kind() {
        // tracing capture 機構本体（グローバル subscriber の 1 回インストール + スレッド
        // ローカルバッファ）は `test_support` を参照。このテストは router の oneshot 全体を
        // capture ウィンドウに含める必要があるため、`capture_warnings` ヘルパ（同期版）では
        // なく `test_support::capture_logs_async` を直接使う。
        let ((status, _body), log_text) = crate::test_support::capture_logs_async(async {
            oneshot_json(
                api_router(test_api_state("correct-key")),
                "POST",
                "/urtect/api/reply",
                Some("Bearer correct-key"),
                r#"{"message":"ADC-VDB101の設定を教えてください"}"#,
            )
            .await
        })
        .await;
        assert_eq!(status, StatusCode::OK);

        assert!(
            log_text.contains("failed to record a ConversationTurn"),
            "expected a ConversationTurn write-failure warning, got: {log_text}"
        );
        assert!(
            log_text.contains("reply_kind"),
            "the warning must carry a reply_kind field: {log_text}"
        );
        assert!(
            log_text.contains("out_of_scope"),
            "the warning must name the out_of_scope branch (not some other reply_kind): {log_text}"
        );
    }

    /// 対照テスト: 取扱内の型番だけ、または型番なしの質問は §3.1 のゲートを素通りし、
    /// 通常フロー（evaluate()）へ進む。`knowledge: None` のためここでは必ず 500 になるが、
    /// それは「evaluate() に到達した」ことの証拠であり（上の 200 経路とは非交差の結果になる）、
    /// §4 のテンプレート文言が一切現れないことを確認する。
    #[tokio::test]
    async fn reply_route_in_scope_model_falls_through_to_the_normal_flow() {
        let router = api_router(test_api_state("correct-key"));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/urtect/api/reply",
            Some("Bearer correct-key"),
            r#"{"message":"ADC-V724の設定を教えてください"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "body: {body}");
        let message = body["message"].as_str().expect("message must be a string");
        assert!(
            !message.contains("取り扱いがございません"),
            "an in-scope-only question must not trigger the §4 out-of-scope template: {body}"
        );
    }

    // ---- 応答側ゲート（§3.5、Critical 2）の配線テスト ----
    //
    // `ProductAllowlist::out_of_scope_mentions` 自体の述語は `product_gate.rs` で検証済み。
    // ここでは `gate_generated_text` / `gate_customer_reply_draft`（api.rs 側の配線: どの変数を
    // 見るか・どのフォールバックへ倒すか）を、実 vegapunk・実 LLM 無しで直接検証する。

    /// ADC-V724 のみ取扱内、それ以外（例: ADC-VDB101）は取扱外という固定 fixture。
    /// `test_harness()` の `product_gate` シード（`TEST_SCHEMA` = "ADC-V724" のみ）と揃えてある。
    fn response_gate_fixture_allowlist() -> product_gate::ProductAllowlist {
        product_gate::ProductAllowlist::from_models(vec!["ADC-V724".to_string()])
    }

    #[test]
    fn gate_customer_reply_draft_discards_a_draft_that_mentions_only_an_out_of_scope_model() {
        let allow = response_gate_fixture_allowlist();
        let out = gate_customer_reply_draft(
            Some("ADC-VDB101の初期設定手順です".to_string()),
            &allow,
            "req-1",
            "case-1",
        );
        assert_eq!(
            out, None,
            "a draft mentioning only an out-of-scope model must be discarded (falls back to \
             EscalationReply via decide_reply_action)"
        );
    }

    #[test]
    fn gate_customer_reply_draft_passes_through_a_draft_that_mentions_only_in_scope_models() {
        let allow = response_gate_fixture_allowlist();
        let draft = "ADC-V724の初期設定手順です".to_string();
        let out = gate_customer_reply_draft(Some(draft.clone()), &allow, "req-1", "case-1");
        assert_eq!(
            out,
            Some(draft),
            "a draft mentioning only in-scope models must pass through unchanged"
        );
    }

    #[test]
    fn gate_customer_reply_draft_passes_through_a_draft_with_no_model_mention() {
        let allow = response_gate_fixture_allowlist();
        let draft = "Wi-Fiの再接続手順です".to_string();
        let out = gate_customer_reply_draft(Some(draft.clone()), &allow, "req-1", "case-1");
        assert_eq!(
            out,
            Some(draft),
            "a draft with no model mention at all must pass through unchanged"
        );
    }

    #[test]
    fn gate_customer_reply_draft_discards_a_draft_that_mixes_in_scope_and_out_of_scope_models() {
        // Issue #28 の完了条件が明示する「混在」条件: 1 文の中に取扱内・取扱外の型番が両方
        // 出てくる場合でも、取扱外の言及が 1 つでもあればブロックしなければならない
        // （取扱内の言及があるからといって安全側に倒れて通過させてはいけない）。
        let allow = response_gate_fixture_allowlist();
        let out = gate_customer_reply_draft(
            Some("ADC-V724とADC-VDB101は共通の手順です".to_string()),
            &allow,
            "req-1",
            "case-1",
        );
        assert_eq!(
            out, None,
            "a draft mixing an in-scope and an out-of-scope model must still be blocked"
        );
    }

    #[test]
    fn gate_generated_text_clarify_route_falls_back_to_fallback_clarify_text() {
        let allow = response_gate_fixture_allowlist();
        let out = gate_generated_text(
            "ADC-VDB101の型番を教えてください".to_string(),
            &allow,
            "req-1",
            "case-1",
            "clarify",
            "clarify question mentions an out-of-scope product model; falling back to \
             FALLBACK_CLARIFY_TEXT",
            || clarify::FALLBACK_CLARIFY_TEXT.to_string(),
        );
        assert_eq!(out, clarify::FALLBACK_CLARIFY_TEXT);
    }

    #[test]
    fn gate_generated_text_escalation_ack_route_falls_back_to_fallback_ack() {
        let allow = response_gate_fixture_allowlist();
        let out = gate_generated_text(
            "ADC-VDB101の件、担当者へおつなぎします".to_string(),
            &allow,
            "req-1",
            "case-1",
            "escalation_ack",
            "escalation ack text mentions an out-of-scope product model; falling back to the \
             deterministic ack fallback",
            || escalation_reply::fallback_ack(false).0.to_string(),
        );
        assert_eq!(out, escalation_reply::fallback_ack(false).0);
    }

    #[test]
    fn gate_generated_text_and_gate_customer_reply_draft_do_not_self_trigger_on_fallback_templates()
    {
        // フォールバック定型文自身が §3.5 に引っかかると、無限に同じ文へ落ちるだけの無意味な
        // 防御になる（自己矛盾）。回答下書き・聞き返し・受け止め文（初回/継続）の全フォール
        // バック定型文が、対応するゲート関数を素通りすることを固定する。
        let allow = response_gate_fixture_allowlist();

        let draft_out = gate_customer_reply_draft(
            Some(clarify::FALLBACK_CLARIFY_TEXT.to_string()),
            &allow,
            "req-1",
            "case-1",
        );
        assert_eq!(
            draft_out,
            Some(clarify::FALLBACK_CLARIFY_TEXT.to_string()),
            "FALLBACK_CLARIFY_TEXT must not be blocked when run through the draft gate"
        );

        for fallback_text in [
            clarify::FALLBACK_CLARIFY_TEXT,
            escalation_reply::fallback_ack(false).0,
            escalation_reply::fallback_ack(true).0,
        ] {
            let out = gate_generated_text(
                fallback_text.to_string(),
                &allow,
                "req-1",
                "case-1",
                "regression_probe",
                "unexpected fallback trigger; this fallback template must never mention an \
                 out-of-scope product model",
                || "SHOULD_NOT_BE_USED".to_string(),
            );
            assert_eq!(
                out, fallback_text,
                "fallback template must not itself be blocked by the §3.5 gate: {fallback_text}"
            );
        }
    }

    // ---- 応答側ゲートの warn ログ（Issue #28 codex レビュー採用5: detected_models） ----
    //
    // capture 機構本体（グローバル subscriber の 1 回インストール + スレッドローカル
    // バッファ）は `test_support` を参照。Dispatch を差し替える旧方式は、capture 機構を
    // 使わないテストが先に無介入で同じコールサイトを叩くと interest cache が「無効」に
    // 確定し手遅れになる問題があったため廃止した（詳細は `test_support` の doc コメント）。

    /// `f` の実行中に出た WARN 以上のログを文字列で返す。
    fn capture_warnings(f: impl FnOnce()) -> String {
        crate::test_support::capture_logs(f).1
    }

    #[test]
    fn gate_customer_reply_draft_warn_log_names_the_detected_out_of_scope_model() {
        let allow = response_gate_fixture_allowlist();
        let logs = capture_warnings(|| {
            gate_customer_reply_draft(
                Some("ADC-VDB101の初期設定手順です".to_string()),
                &allow,
                "req-1",
                "case-1",
            );
        });
        assert!(logs.contains("WARN"), "{logs}");
        assert!(
            logs.contains("ADC-VDB101"),
            "the warn log must name the detected out-of-scope model so operators do not have \
             to dig the draft body back out to know what triggered the gate: {logs}"
        );
    }

    #[test]
    fn gate_generated_text_warn_log_names_the_detected_out_of_scope_model() {
        let allow = response_gate_fixture_allowlist();
        let logs = capture_warnings(|| {
            gate_generated_text(
                "ADC-VDB101の型番を教えてください".to_string(),
                &allow,
                "req-1",
                "case-1",
                "clarify",
                "clarify question mentions an out-of-scope product model; falling back to \
                 FALLBACK_CLARIFY_TEXT",
                || clarify::FALLBACK_CLARIFY_TEXT.to_string(),
            );
        });
        assert!(logs.contains("WARN"), "{logs}");
        assert!(
            logs.contains("ADC-VDB101"),
            "the warn log must name the detected out-of-scope model: {logs}"
        );
    }

    // ---- 質問側ゲート 二段目（Issue #28 §3.1、LLM解釈 + コード判定）の配線テスト ----
    //
    // `confirmed_foreign_reference` 自体の述語は `product_gate.rs` で検証済み。ここでは
    // `second_stage_out_of_scope_reply`（api.rs 側の配線: 定型応答文の組み立てと呼び出し）を
    // 直接検証する。HTTP ルーティング経由のテストは不要（`test_harness()` は `knowledge: None`
    // で `evaluate()` が常に失敗するため `Ok(outcome)` 経路に到達できない）。

    #[test]
    fn second_stage_out_of_scope_reply_returns_the_canned_reply_when_foreign_surface_is_present() {
        let allow = response_gate_fixture_allowlist();
        let refs = vec![product_gate::ProductReference {
            surface: "ADC-VDB101".to_string(),
            resolution: product_gate::ProductReferenceResolution::Foreign,
            matched_model: None,
        }];
        let out = second_stage_out_of_scope_reply(
            &refs,
            "ADC-VDB101について教えてください",
            &allow,
            "req-1",
            "case-1",
        );
        let text = out.expect("a confirmed foreign reference must produce the canned reply");
        assert!(text.contains("ADC-VDB101"), "{text}");
        assert!(
            text.contains("ADC-V724"),
            "the canned reply must include the in-scope allowlist: {text}"
        );
    }

    #[test]
    fn second_stage_out_of_scope_reply_is_none_when_foreign_surface_is_not_in_the_message() {
        // LLM の幻覚ガード: 発話に無い表層をモデルが作り出した場合は取扱外へ倒さない。
        let allow = response_gate_fixture_allowlist();
        let refs = vec![product_gate::ProductReference {
            surface: "ADC-VDB101".to_string(),
            resolution: product_gate::ProductReferenceResolution::Foreign,
            matched_model: None,
        }];
        let out =
            second_stage_out_of_scope_reply(&refs, "映像が映りません", &allow, "req-1", "case-1");
        assert_eq!(out, None);
    }

    #[test]
    fn second_stage_out_of_scope_reply_is_none_for_ambiguous_only() {
        let allow = response_gate_fixture_allowlist();
        let refs = vec![product_gate::ProductReference {
            surface: "ドアベル".to_string(),
            resolution: product_gate::ProductReferenceResolution::Ambiguous,
            matched_model: None,
        }];
        let out =
            second_stage_out_of_scope_reply(&refs, "ドアベルの設定は?", &allow, "req-1", "case-1");
        assert_eq!(out, None);
    }

    #[test]
    fn second_stage_out_of_scope_reply_is_none_for_matched_only() {
        let allow = response_gate_fixture_allowlist();
        let refs = vec![product_gate::ProductReference {
            surface: "ADC-V724".to_string(),
            resolution: product_gate::ProductReferenceResolution::Matched,
            matched_model: Some("ADC-V724".to_string()),
        }];
        let out =
            second_stage_out_of_scope_reply(&refs, "ADC-V724の設定は?", &allow, "req-1", "case-1");
        assert_eq!(out, None);
    }

    #[test]
    fn second_stage_out_of_scope_reply_is_none_when_product_references_is_empty() {
        let allow = response_gate_fixture_allowlist();
        let out =
            second_stage_out_of_scope_reply(&[], "映像が映りません", &allow, "req-1", "case-1");
        assert_eq!(out, None);
    }

    #[test]
    fn second_stage_out_of_scope_reply_is_none_when_llm_misclassifies_an_in_scope_model_as_foreign()
    {
        // Issue #28 codex Stage2 Warning 4: LLM が allowlist 内の型番（ここでは
        // `response_gate_fixture_allowlist` が持つ唯一の取扱内型番 "ADC-V724"）を foreign と
        // 誤答しても、決定論の製品マスタが優先され、定型応答は返らない（evaluate() の通常
        // フローに委ねられる。「ADC-V724 は当社では取り扱いがございません。当社で取り扱って
        // いる製品は…ADC-V724…です。」という自己矛盾した応答を防ぐ回帰）。
        let allow = response_gate_fixture_allowlist();
        let refs = vec![product_gate::ProductReference {
            surface: "ADC-V724".to_string(),
            resolution: product_gate::ProductReferenceResolution::Foreign,
            matched_model: None,
        }];
        let out = second_stage_out_of_scope_reply(
            &refs,
            "ADC-V724について教えてください",
            &allow,
            "req-1",
            "case-1",
        );
        assert_eq!(out, None);
    }

    #[test]
    fn second_stage_out_of_scope_reply_reflects_trimmed_surface_not_raw_surface() {
        // Warning 3 の回帰: `confirmed_foreign_reference` の検証（幻覚ガード・最小長・
        // 反射安全性・allowlist veto）はすべて trim 後の surface に対して行われるが、
        // 定型応答へ反射する値が trim 前の生の surface のままだと、検証を一切通っていない
        // 前後の空白・改行がそのまま顧客向け応答に混入する（trim 後は 64 文字以下でも、
        // 空白込みで水増しされた生値には長さ上限が掛からない）。
        let allow = response_gate_fixture_allowlist();
        let refs = vec![product_gate::ProductReference {
            surface: "\n\n  ADC-VDB101  \n".to_string(),
            resolution: product_gate::ProductReferenceResolution::Foreign,
            matched_model: None,
        }];
        let out = second_stage_out_of_scope_reply(
            &refs,
            "ADC-VDB101について教えてください",
            &allow,
            "req-1",
            "case-1",
        );
        let text = out.expect("a confirmed foreign reference must produce the canned reply");
        assert!(
            text.starts_with("申し訳ありません。ADC-VDB101 は当社では取り扱いがございません。"),
            "reflected surface must be trimmed, not the raw value with surrounding whitespace: \
             {text:?}"
        );
    }

    // ---- second_stage_short_circuit（Issue #28 W4 是正: reply_handler 内の配線テスト）----
    //
    // `second_stage_out_of_scope_reply` 自体の述語・応答文組み立ては上のテスト群で検証済み。
    // ここでは reply_handler が evaluate() 直後に行う配線（early return の判定・
    // `outcome.case_id`/`outcome.new_signals` を `demote_case_to_out_of_scope` へそのまま
    // 伝搬すること）を、I/O を伴わない純関数として直接検証する。

    #[test]
    fn second_stage_short_circuit_is_none_when_there_is_no_confirmed_foreign_reference() {
        let allow = response_gate_fixture_allowlist();
        let outcome = base_outcome(allowed_decision(), false);
        assert!(outcome.product_references.is_empty());

        let out = second_stage_short_circuit(&outcome, "映像が映りません", &allow, "req-1");

        assert!(out.is_none());
    }

    #[test]
    fn second_stage_short_circuit_propagates_the_outcome_case_id_and_new_signals_for_demotion() {
        let allow = response_gate_fixture_allowlist();
        let mut outcome = base_outcome(allowed_decision(), false);
        outcome.case_id = "case-42".to_string();
        outcome.product_references = vec![product_gate::ProductReference {
            surface: "ADC-VDB101".to_string(),
            resolution: product_gate::ProductReferenceResolution::Foreign,
            matched_model: None,
        }];
        let mut new_signals = crate::harness::signal::SignalSet::new();
        new_signals.insert(crate::harness::signal::Signal::new("hazard_x"));
        outcome.new_signals = new_signals.clone();
        // `signals` / `accumulated_signals` には意図的に `new_signals` と異なる値を入れる。
        // これにより、配線が `outcome.new_signals` ではなく取り違えて `signals` や
        // `accumulated_signals` を伝搬した場合にこのテストが検知できる（型が一致するため
        // コンパイルは通ってしまう取り違え）。
        let mut decoy_signals = crate::harness::signal::SignalSet::new();
        decoy_signals.insert(crate::harness::signal::Signal::new("decoy_not_new_signal"));
        outcome.signals = decoy_signals.clone();
        outcome.accumulated_signals = decoy_signals;

        let short_circuit = second_stage_short_circuit(
            &outcome,
            "ADC-VDB101について教えてください",
            &allow,
            "req-1",
        )
        .expect("a confirmed foreign reference must short-circuit");

        assert_eq!(short_circuit.case_id, "case-42");
        assert_eq!(short_circuit.discarded_signals, new_signals);
    }

    #[test]
    fn second_stage_short_circuit_reply_text_matches_second_stage_out_of_scope_reply_directly() {
        let allow = response_gate_fixture_allowlist();
        let mut outcome = base_outcome(allowed_decision(), false);
        outcome.case_id = "case-1".to_string();
        outcome.product_references = vec![product_gate::ProductReference {
            surface: "ADC-VDB101".to_string(),
            resolution: product_gate::ProductReferenceResolution::Foreign,
            matched_model: None,
        }];
        let message = "ADC-VDB101について教えてください";

        let direct = second_stage_out_of_scope_reply(
            &outcome.product_references,
            message,
            &allow,
            "req-1",
            &outcome.case_id,
        )
        .expect("direct call must produce the canned reply");

        let via_wiring = second_stage_short_circuit(&outcome, message, &allow, "req-1")
            .expect("wired call must produce the canned reply");

        assert_eq!(
            via_wiring.reply_text, direct,
            "wiring through second_stage_short_circuit must not change the reply text produced \
             by second_stage_out_of_scope_reply"
        );
    }

    #[test]
    fn second_stage_short_circuit_is_none_for_ambiguous_or_matched_only() {
        let allow = response_gate_fixture_allowlist();

        let mut ambiguous_outcome = base_outcome(allowed_decision(), false);
        ambiguous_outcome.product_references = vec![product_gate::ProductReference {
            surface: "ドアベル".to_string(),
            resolution: product_gate::ProductReferenceResolution::Ambiguous,
            matched_model: None,
        }];
        assert!(
            second_stage_short_circuit(&ambiguous_outcome, "ドアベルの設定は?", &allow, "req-1")
                .is_none(),
            "ambiguous のみでは二段目に該当しない"
        );

        let mut matched_outcome = base_outcome(allowed_decision(), false);
        matched_outcome.product_references = vec![product_gate::ProductReference {
            surface: "ADC-V724".to_string(),
            resolution: product_gate::ProductReferenceResolution::Matched,
            matched_model: Some("ADC-V724".to_string()),
        }];
        assert!(
            second_stage_short_circuit(&matched_outcome, "ADC-V724の設定は?", &allow, "req-1")
                .is_none(),
            "matched のみでは二段目に該当しない"
        );
    }
}
