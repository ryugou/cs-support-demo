//! CS サポートモード応答の生成(composition, Issue #50 バッチ1)。
//!
//! design doc `docs/superpowers/specs/2026-08-17-homesec-advisor-design.md` §13
//! (「CS サポートモード(composition)」)のバッチ1実装。**CS 側の実装ファイル
//! (`server/src/api.rs` / `server/src/harness/**` / `server/src/main.rs`)は 1 行も変更しない。**
//! ここは CS の `crate::api::reply_handler`(`server/src/api.rs` 891〜1378行)と**同じ順序**で、
//! 既存の公開部品(`Harness::begin` / `evaluate` / `product_allowlist` /
//! `record_out_of_scope_case` / `load_conv_state` / `save_conv_state` /
//! `demote_case_to_out_of_scope` / `record_conversation_turn` / `audit`、および
//! `crate::api::decide_reply_action` / `product_gate::{confirmed_foreign_reference,
//! out_of_scope_mentions, build_out_of_scope_reply}` 等)を**呼ぶだけ**の薄いオーケストレーション
//! を持つ(design doc §13.1: 「部品の複製はしない。呼び出し順だけを advisor 側に持つ」)。
//!
//! 唯一の例外は、`reply_handler` の中で `api.rs` の module-private 関数(`pub`/`pub(crate)` の
//! どちらも付いていない無印 `fn`)として実装されている小さな状態遷移・整形ロジック
//! (`is_continuation` / `note_time_pref_extraction_failure` / `arm_time_pref_solicitation` /
//! `is_final_clarify_turn` / `missing_to_text` / `build_known_facts` とその内部ヘルパ)で、
//! これらは design doc 自体が「オーケストレーション層の責務」と位置づけているため、
//! 同じ挙動でこのモジュールに再実装している(下記の各関数 doc コメントに
//! `api.rs::<関数名>` と同じ契約である旨を明記する)。`is_clarify_exhausted`
//! (計測用の info ログにしか使われず応答内容に影響しない)はこのバッチでは省略した。
//!
//! `gate_generated_text` / `gate_customer_reply_draft` / `second_stage_out_of_scope_reply` /
//! `second_stage_short_circuit` も同様にこのモジュールへ新規実装しているが、実体の判定ロジック
//! (`product_gate::out_of_scope_mentions` / `product_gate::confirmed_foreign_reference` /
//! `product_gate::build_out_of_scope_reply`)は一切複製せず、`pub` な関数を呼ぶだけの薄い glue
//! である。
//!
//! # フロー表(`crate::api::reply_handler` と同じ順序。§欄は同ハンドラの doc コメント参照)
//!
//! | # | 入力 | 条件 | 呼び出す関数 | 出力 |
//! |---|------|------|--------------|------|
//! | 1 | `identity`/`schema`/`manual_schema` | 常に | [`crate::harness::Harness::begin`] | `RequestContext`(失敗時 `Err` を伝播) |
//! | 2 | `message` | 常に(evaluate より前の質問側ゲート) | [`crate::harness::Harness::product_allowlist`] → `ProductAllowlist::first_out_of_scope_token` | Some なら `product_gate::build_out_of_scope_reply` + `Harness::record_out_of_scope_case` → [`SupportReplyKind::OutOfScope`] で早期return |
//! | 3 | `case_id` の conv state | `case_id` あり かつ `awaiting_time_pref == true` | [`crate::harness::time_pref::extract_time_preference`] → [`crate::harness::time_pref::handle_time_pref`] | `TimePrefAction::Reply` なら [`SupportReplyKind::TimePref`] で早期return。`PassToEvaluate`、または抽出失敗(`note_time_pref_extraction_failure`)は下へ続行 |
//! | 4 | `history`/`case_id` | 常に | [`is_continuation`](自前実装、`api.rs::is_continuation` と同契約) | `bool` |
//! | 5 | 上記すべて | 常に | [`crate::harness::Harness::evaluate`](`UnknownCaseIdPolicy::StartNew`) | `EvaluationOutcome`(失敗時 `Err` を伝播) |
//! | 6 | `outcome.product_references` | 常に(load_conv_state より前の二段目ゲート) | `product_gate::confirmed_foreign_reference` → `product_gate::build_out_of_scope_reply` | Some なら `Harness::demote_case_to_out_of_scope` → [`SupportReplyKind::OutOfScope`] で早期return |
//! | 7 | `outcome.case_id` | 常に | [`crate::harness::Harness::load_conv_state`] | `CaseConvState` |
//! | 8 | `outcome.customer_reply_draft` | 常に(decide_reply_action より前) | `allowlist.out_of_scope_mentions`([`gate_customer_reply_draft`] 経由) | ゲート済み `Option<String>` |
//! | 9 | `outcome`/`conv`/`api_cfg` | 常に | [`crate::api::decide_reply_action`] | `ReplyAction::{Answer,Clarify,EscalationReply}` |
//! | 10a | `Answer(text)` | `Allowed` かつ下書きあり・非truncated(`LexiconFallback` でない) | そのまま | [`SupportReplyKind::Answer`] |
//! | 10b | `Clarify` | `Escalate` かつ `clarification_allowed` かつ `clarify_turns < max` | `clarify::draft_clarify_question`(または `FALLBACK_CLARIFY_TEXT`)→ ゲート → `append_final_turn_suffix`。`conv.clarify_turns += 1` | [`SupportReplyKind::Clarify`] |
//! | 10c | `EscalationReply` | それ以外すべて(`extraction_mode == LexiconFallback` の fail-closed を含む) | `escalation_reply::draft_ack_text`(または `fallback_ack`)→ ゲート → `build_deterministic_block` → `assemble_escalation_reply`。`arm_time_pref_solicitation(&mut conv)` | [`SupportReplyKind::Escalation`] |
//! | 11 | 上記の結果 | 常に | [`crate::harness::Harness::record_conversation_turn`](5秒 timeout、失敗は warn のみで応答は止めない) | [`SupportReplyOutcome`] |

use crate::config::{ApiConfig, ManualSchemaKind};
use crate::harness::decision::{self, AnswerDecision};
use crate::harness::product_gate::{self, ProductAllowlist};
use crate::harness::reply::ReplyHistoryTurn;
use crate::harness::{clarify, escalation_reply, hours, time_pref};
use crate::harness::{
    CaseConvState, EvaluationOutcome, Harness, RequestContext, UnknownCaseIdPolicy,
};
use crate::mcp::ToolService;
use crate::oauth::VerifiedIdentity;
use std::time::Duration;

/// `record_conversation_turn` の完了を待つ上限。`api.rs::CONVERSATION_TURN_WRITE_TIMEOUT` と
/// 同じ 5 秒・同じ理由(vegapunk gRPC の 120 秒タイムアウトまでオーケストレーション呼び出し元を
/// ブロックしない。打ち切り後もバックエンド側で書き込みが継続しうるため、ターン欠落は許容し
/// 応答は止めない)。
const SUPPORT_CONVERSATION_TURN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// `awaiting_time_pref` 中の抽出インフラ失敗を許容する連続回数。`api.rs` の同名定数と同じ値
/// (design doc §5: 3 回連続で `awaiting_time_pref` を自動解除する)。
const TIME_PREF_EXTRACTION_ERROR_LIMIT: u32 = 3;

/// [`run_support_turn`] が返す応答の種別。design doc §13.3 が要求する
/// `support_answer` / `support_clarify` / `support_escalation` / `support_time_pref` /
/// `support_out_of_scope` への写像は、呼び出し側(advisor のターン記録機構)が
/// [`SupportReplyKind::as_str`] の値を見て行う(このバッチでは配線しない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportReplyKind {
    /// `customer_reply_draft` をそのまま返した(`crate::api::ReplyAction::Answer` 相当)。
    Answer,
    /// 聞き返し(ヒアリングループ)を返した。
    Clarify,
    /// エスカレーション応答(受け止め文 + 決定的ブロック)を返した。
    Escalation,
    /// 希望時間帯の受付に対する即時返信を返した(`evaluate()` を呼んでいない)。
    TimePref,
    /// 取扱外製品の定型応答を返した(質問側ゲートまたは二段目ゲート。`evaluate()` を
    /// 呼んでいない、または `evaluate()` の判定を破棄している)。
    OutOfScope,
}

impl SupportReplyKind {
    /// CS 側 `api.rs::ok_reply_response` の呼び出し箇所が実際に使っているリテラル
    /// (`"answer"` / `"clarify"` / `"escalation"` / `"time_pref"` / `"out_of_scope"`)と
    /// 過不足なく一致する。`ConversationTurn.reply_kind` 属性へそのまま書く値。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Clarify => "clarify",
            Self::Escalation => "escalation",
            Self::TimePref => "time_pref",
            Self::OutOfScope => "out_of_scope",
        }
    }
}

/// [`run_support_turn`] の結果。
pub struct SupportReplyOutcome {
    /// `crate::harness::prompt_input::to_plain_text` 正規化済み(`api.rs::build_reply_response`
    /// と同じ規律)。
    pub reply_text: String,
    pub case_id: String,
    pub reply_kind: SupportReplyKind,
    /// 空文字列は「監査記録に失敗したが応答は継続した」ことを表す(Issue #31 design doc §1-c
    /// と同じ規律。`Harness::record_out_of_scope_case` 等の doc コメント参照)。
    pub audit_event_id: String,
}

/// CS サポートモードの 1 ターンを、CS の `crate::api::reply_handler` と同じ順序で実行する。
///
/// project registry(`AppConfig.projects` の `project_id` 引き当て)は一切介さない(design doc
/// §13.1 のとおり、呼び出し元がサポート専用に組み立てた `Harness` / `schema` を直接渡す設計)。
/// `harness.begin(identity, schema, manual_schema)` を直接呼ぶ。
///
/// エラー(`Harness::begin` / `product_allowlist` / `evaluate` / `load_conv_state` /
/// `save_conv_state` の失敗)はそのまま `Err` として呼び出し元へ伝播する。CS 側 HTTP ハンドラが
/// 行っている 503/500 の分類(`crate::api::classify_evaluate_error`)はこの関数の関心事ではない
/// (このバッチは HTTP レスポンスを組み立てない。呼び出し元は必要ならそちらを再利用できる)。
#[allow(clippy::too_many_arguments)]
pub async fn run_support_turn(
    harness: &Harness,
    tools: &ToolService,
    api_cfg: &ApiConfig,
    identity: &VerifiedIdentity,
    schema: &str,
    manual_schema: ManualSchemaKind,
    message: &str,
    history: &[ReplyHistoryTurn],
    case_id: Option<&str>,
    end_user_id: Option<&str>,
) -> anyhow::Result<SupportReplyOutcome> {
    let ctx = harness.begin(identity, schema, manual_schema)?;
    let request_id = ctx.request_id.clone();

    // フロー表 #2: 質問側ゲート(evaluate() より前、LLM 不使用)。
    let allowlist = harness.product_allowlist(&ctx.schema).await?;
    if let Some(out_of_scope_model) = allowlist.first_out_of_scope_token(message) {
        let reply_text = product_gate::build_out_of_scope_reply(&out_of_scope_model, &allowlist);
        let (out_case_id, audit_event_id) = harness
            .record_out_of_scope_case(&ctx, message, case_id, end_user_id)
            .await;
        return Ok(finish_turn(
            harness,
            &ctx,
            SupportReplyKind::OutOfScope,
            message,
            end_user_id,
            reply_text,
            out_case_id,
            audit_event_id,
        )
        .await);
    }

    // フロー表 #3: 希望時間帯の受付。evaluate() を呼ぶ前にすべての state 変更・保存を完了させる
    // (`Harness::save_conv_state` の doc コメントが明記する lost-update 契約)。
    if let Some(id) = case_id {
        let mut conv = harness.load_conv_state(&ctx, id).await?;
        if conv.awaiting_time_pref {
            let extraction = match harness.reply_drafter.as_ref() {
                Some(drafter) => time_pref::extract_time_preference(drafter, message).await,
                None => Err(time_pref::TimePrefExtractionError),
            };
            match extraction {
                Ok(extraction) => {
                    conv.time_pref_extraction_error_count = 0;
                    let action = time_pref::handle_time_pref(
                        &extraction,
                        &mut conv,
                        &api_cfg.business_hours,
                    );
                    harness.save_conv_state(&ctx, id, &conv).await?;
                    if let time_pref::TimePrefAction::Reply(text) = action {
                        let audit_event_id = match harness
                            .audit(&ctx, "time_pref_reply", None, vec![id.to_string()])
                            .await
                        {
                            Ok(audit_id) => audit_id,
                            Err(err) => {
                                tracing::warn!(
                                    error = ?err,
                                    request_id = %request_id,
                                    case_id = id,
                                    "cs_support: time_pref_reply audit failed; continuing with \
                                     an empty audit_event_id"
                                );
                                String::new()
                            }
                        };
                        return Ok(finish_turn(
                            harness,
                            &ctx,
                            SupportReplyKind::TimePref,
                            message,
                            end_user_id,
                            text,
                            id.to_string(),
                            audit_event_id,
                        )
                        .await);
                    }
                    // TimePrefAction::PassToEvaluate: 下の evaluate() へ続行。
                }
                Err(_) => {
                    note_time_pref_extraction_failure(&mut conv);
                    harness.save_conv_state(&ctx, id, &conv).await?;
                    // 通常の evaluate フローへ続行。
                }
            }
        }
    }

    // フロー表 #4。
    let is_continuation = is_continuation(history, case_id);

    // フロー表 #5。
    let mut outcome = harness
        .evaluate(
            &ctx,
            message,
            None,
            case_id,
            tools,
            history,
            is_continuation,
            UnknownCaseIdPolicy::StartNew,
            end_user_id,
        )
        .await?;

    // フロー表 #6: 二段目ゲート。evaluate() の結果より前に判定する。
    if let Some(short_circuit) =
        second_stage_short_circuit(&outcome, message, &allowlist, &request_id)
    {
        let (out_case_id, audit_event_id) = harness
            .demote_case_to_out_of_scope(
                &ctx,
                message,
                &short_circuit.case_id,
                &short_circuit.discarded_signals,
            )
            .await;
        return Ok(finish_turn(
            harness,
            &ctx,
            SupportReplyKind::OutOfScope,
            message,
            end_user_id,
            short_circuit.reply_text,
            out_case_id,
            audit_event_id,
        )
        .await);
    }

    // フロー表 #7。
    let mut conv = harness.load_conv_state(&ctx, &outcome.case_id).await?;

    // フロー表 #8〜#10。
    let (reply_kind, reply_text) = build_post_evaluate_reply(
        harness,
        api_cfg,
        &allowlist,
        &request_id,
        message,
        history,
        is_continuation,
        &mut outcome,
        &mut conv,
    )
    .await;

    // `Answer` は conv 状態を変更しない(元の `reply_handler` も Answer 分岐で
    // `save_conv_state` を呼ばない)。`Clarify` / `Escalation` は `build_post_evaluate_reply`
    // が `conv` を書き換えているので保存する。
    if matches!(
        reply_kind,
        SupportReplyKind::Clarify | SupportReplyKind::Escalation
    ) {
        harness
            .save_conv_state(&ctx, &outcome.case_id, &conv)
            .await?;
    }

    Ok(finish_turn(
        harness,
        &ctx,
        reply_kind,
        message,
        end_user_id,
        reply_text,
        outcome.case_id,
        outcome.audit_event_id,
    )
    .await)
}

/// フロー表 #8〜#10: `evaluate()` 後の応答種別決定と応答文組み立て。
///
/// `run_support_turn` から論理的に切り出しているのは、`evaluate()`(実 vegapunk が必要)を
/// 呼ばずに `EvaluationOutcome` を直接構築してこの関数だけを単体テストするため(`api.rs` 自身が
/// `decide_reply_action` を独立した pub 純関数として切り出しているのと同じ設計思想)。
///
/// `conv` は `Clarify`(`clarify_turns` 加算)・`EscalationReply`(`arm_time_pref_solicitation`)で
/// 書き換える。呼び出し側は返った [`SupportReplyKind`] が `Clarify` / `Escalation` のときだけ
/// `save_conv_state` を呼ぶこと(`Answer` は conv 状態を変更しない)。
#[allow(clippy::too_many_arguments)]
async fn build_post_evaluate_reply(
    harness: &Harness,
    api_cfg: &ApiConfig,
    allowlist: &ProductAllowlist,
    request_id: &str,
    message: &str,
    history: &[ReplyHistoryTurn],
    is_continuation: bool,
    outcome: &mut EvaluationOutcome,
    conv: &mut CaseConvState,
) -> (SupportReplyKind, String) {
    // フロー表 #8: 応答側ゲート(回答下書き)。`decide_reply_action` より前に判定する。
    outcome.customer_reply_draft = gate_customer_reply_draft(
        outcome.customer_reply_draft.take(),
        allowlist,
        request_id,
        &outcome.case_id,
    );

    // フロー表 #9。
    let action = crate::api::decide_reply_action(outcome, conv, api_cfg);

    match action {
        crate::api::ReplyAction::Answer(text) => (SupportReplyKind::Answer, text),
        crate::api::ReplyAction::Clarify => {
            let missing: &[decision::EvidenceRequirement] = match &outcome.decision {
                AnswerDecision::Escalate { missing, .. } => missing,
                other => {
                    tracing::error!(
                        request_id = %request_id,
                        decision = ?other,
                        "cs_support: decide_reply_action returned Clarify for a non-Escalate \
                         decision; this is a bug in decide_reply_action's decision-table logic. \
                         Falling back to an empty missing list so the clarify prompt still \
                         degrades gracefully instead of panicking"
                    );
                    &[]
                }
            };
            let missing_text = missing_to_text(missing);
            let known_facts =
                build_known_facts(&harness.lexicon, &outcome.accumulated_signals, history);
            let is_final_clarify_turn = is_final_clarify_turn(conv, api_cfg);
            let reply_text = match harness.reply_drafter.as_ref() {
                Some(drafter) => {
                    clarify::draft_clarify_question(
                        drafter,
                        &harness.ng,
                        harness.reply_draft_max_tokens,
                        message,
                        &missing_text,
                        &known_facts,
                        is_continuation,
                        allowlist,
                    )
                    .await
                }
                None => clarify::FALLBACK_CLARIFY_TEXT.to_string(),
            };
            let reply_text = gate_generated_text(
                reply_text,
                allowlist,
                request_id,
                &outcome.case_id,
                "clarify",
                "clarify question mentions an out-of-scope product model; falling back to \
                 FALLBACK_CLARIFY_TEXT",
                || clarify::FALLBACK_CLARIFY_TEXT.to_string(),
            );
            let reply_text = clarify::append_final_turn_suffix(reply_text, is_final_clarify_turn);

            conv.clarify_turns += 1;
            (SupportReplyKind::Clarify, reply_text)
        }
        crate::api::ReplyAction::EscalationReply => {
            let ack_text = match harness.reply_drafter.as_ref() {
                Some(drafter) => {
                    escalation_reply::draft_ack_text(
                        drafter,
                        &harness.ng,
                        harness.reply_draft_max_tokens,
                        message,
                        is_continuation,
                    )
                    .await
                }
                None => escalation_reply::fallback_ack(is_continuation)
                    .0
                    .to_string(),
            };
            let ack_text = gate_generated_text(
                ack_text,
                allowlist,
                request_id,
                &outcome.case_id,
                "escalation_ack",
                "escalation ack text mentions an out-of-scope product model; falling back to \
                 the deterministic ack fallback",
                || {
                    escalation_reply::fallback_ack(is_continuation)
                        .0
                        .to_string()
                },
            );
            let out_of_hours_now =
                !hours::is_within_business_hours(&api_cfg.business_hours, chrono::Utc::now());
            let hours_label = hours::business_hours_label(&api_cfg.business_hours);
            let block = escalation_reply::build_deterministic_block(
                &outcome.case_id,
                &hours_label,
                out_of_hours_now,
            );
            let reply_text = escalation_reply::assemble_escalation_reply(&ack_text, &block);

            arm_time_pref_solicitation(conv);
            (SupportReplyKind::Escalation, reply_text)
        }
    }
}

/// フロー表 #11: 応答文の正規化 + `record_conversation_turn` の呼び出し +
/// [`SupportReplyOutcome`] の組み立て。
///
/// `api.rs::ok_reply_response` の置き換え(戻り値が axum の `Response` ではなく
/// [`SupportReplyOutcome`] である点だけが異なる)。書き込み失敗時の扱い ── 応答を止めず
/// warn するだけで継続する ── は同じ契約を踏襲する(ターン欠落は許容し、`audit_event_id` を
/// 使えば監査ログとの突合で検出できるという `api.rs` 側の設計判断をそのまま引き継ぐ)。
#[allow(clippy::too_many_arguments)]
async fn finish_turn(
    harness: &Harness,
    ctx: &RequestContext,
    reply_kind: SupportReplyKind,
    question: &str,
    end_user_id: Option<&str>,
    reply_text: String,
    case_id: String,
    audit_event_id: String,
) -> SupportReplyOutcome {
    let reply_text = crate::harness::prompt_input::to_plain_text(&reply_text);
    match tokio::time::timeout(
        SUPPORT_CONVERSATION_TURN_WRITE_TIMEOUT,
        harness.record_conversation_turn(
            ctx,
            &case_id,
            end_user_id,
            question,
            &reply_text,
            reply_kind.as_str(),
            &audit_event_id,
        ),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                error = ?err,
                case_id = %case_id,
                reply_kind = reply_kind.as_str(),
                audit_event_id = %audit_event_id,
                "cs_support: failed to record a ConversationTurn; continuing without blocking \
                 the reply"
            );
        }
        Err(_elapsed) => {
            tracing::warn!(
                case_id = %case_id,
                reply_kind = reply_kind.as_str(),
                timeout_secs = SUPPORT_CONVERSATION_TURN_WRITE_TIMEOUT.as_secs(),
                audit_event_id = %audit_event_id,
                "cs_support: gave up waiting for a ConversationTurn write at the timeout; \
                 returning the reply anyway (turn loss is tolerated)"
            );
        }
    }
    SupportReplyOutcome {
        reply_text,
        case_id,
        reply_kind,
        audit_event_id,
    }
}

// ---- オーケストレーション層の小さな状態遷移・整形ロジック ----
//
// design doc §13.1 が「オーケストレーション層の責務」と位置づける、`api.rs` の
// module-private 関数群と同じ挙動の再実装(`Harness` の責務ではないためこちらに置く)。

/// `api.rs::is_continuation` と同じ契約: `history` が非空、または `case_id` が渡された場合は
/// 継続。判定は決定論(コード)。文面の出し分けは `clarify.rs` / `escalation_reply.rs` 側が
/// `is_continuation: bool` を受け取って行うだけ。
fn is_continuation(history: &[ReplyHistoryTurn], case_id: Option<&str>) -> bool {
    !history.is_empty() || case_id.is_some()
}

/// `api.rs::note_time_pref_extraction_failure` と同じ契約: 希望時間帯抽出のインフラ失敗
/// (LLM 呼び出しエラー・parse 失敗・`reply_drafter` 未設定)を 1 回分記録する。design doc §5:
/// 3 回連続で `awaiting_time_pref` を自動解除する(`time_pref_false_count` の 2 回連続解除とは
/// 別枠)。
fn note_time_pref_extraction_failure(conv: &mut CaseConvState) {
    conv.time_pref_extraction_error_count += 1;
    if conv.time_pref_extraction_error_count >= TIME_PREF_EXTRACTION_ERROR_LIMIT {
        conv.awaiting_time_pref = false;
        conv.time_pref_false_count = 0;
        conv.time_pref_extraction_error_count = 0;
    }
}

/// `api.rs::arm_time_pref_solicitation` と同じ契約: 新しいエスカレーション応答を送るときに
/// 希望時間帯の伺いを立てる。`time_pref_extraction_error_count` はここでは変更しない
/// (3 回連続到達による自動解除を永久に到達不能にしないため。`api.rs` 側の doc コメントが
/// 明記する理由と同じ)。
fn arm_time_pref_solicitation(conv: &mut CaseConvState) {
    conv.awaiting_time_pref = true;
    conv.time_pref_false_count = 0;
    conv.clarify_turns = 0;
}

/// `api.rs::is_final_clarify_turn` と同じ契約。**前提**: `decide_reply_action` が
/// `ReplyAction::Clarify` を返した後にのみ呼ぶこと(その時点で `conv.clarify_turns <
/// cfg.clarify_max_turns` が保証されている)。
fn is_final_clarify_turn(conv: &CaseConvState, cfg: &ApiConfig) -> bool {
    conv.clarify_turns >= cfg.clarify_max_turns.saturating_sub(1)
}

/// `api.rs::missing_to_text` と同じ契約: `AnswerDecision::Escalate.missing` を
/// `clarify::build_clarify_prompt` の第2引数(不足情報)向けの人間可読テキストへ変換する。
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

/// `api.rs::normalize_customer_turn_to_single_line` と同じ契約: 顧客発話 1 件を「把握済み事項」
/// の 1 行として安全に埋め込むための正規化(改行等の制御文字を単一行へ潰し、100 字へ切り詰め)。
fn normalize_customer_turn_to_single_line(text: &str) -> String {
    let single_spaced = crate::harness::prompt_input::collapse_to_single_line(text);
    crate::harness::prompt_input::truncate_chars(&single_spaced, 100)
}

/// `api.rs::select_customer_history_for_known_facts` と同じ契約: 「把握済み事項リスト」専用の
/// customer 発話選択(`reply::select_history` の共有窓ではなく、customer 発話だけを 100 字へ
/// 正規化してから新しい側最大 [`crate::harness::reply::MAX_HISTORY_TURNS`] 件を採る)。
fn select_customer_history_for_known_facts(history: &[ReplyHistoryTurn]) -> Vec<String> {
    let mut normalized: Vec<String> = history
        .iter()
        .filter(|turn| turn.role == crate::harness::reply::ReplyHistoryRole::Customer)
        .map(|turn| normalize_customer_turn_to_single_line(&turn.text))
        .filter(|text| !text.is_empty())
        .collect();
    let skip = normalized
        .len()
        .saturating_sub(crate::harness::reply::MAX_HISTORY_TURNS);
    normalized.drain(..skip);
    normalized
}

/// `api.rs::build_known_facts` と同じ契約: 「把握済み事項リスト」をコードで組み立てる。
/// 聞き返しのたびに、既に分かっていることを再度質問してしまう退行を防ぐため、
/// `clarify::build_clarify_prompt` の `known_facts` 引数へそのまま渡す。
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

// ---- 安全ゲート glue(実体は product_gate:: の pub 関数。ここでは呼ぶだけ) ----

/// `api.rs::gate_generated_text` と同じ契約: Issue #28 §3.5 応答側ゲート(決定論・最終防衛線)
/// の共通判定。生成文が allowlist 外の型番を 1 つでも言及していれば `fallback()` の結果に
/// 置き換え、`route` ラベル付きで warn する。実体の判定は `allowlist.out_of_scope_mentions`
/// (`product_gate::ProductAllowlist` の `pub` メソッド)。
fn gate_generated_text(
    text: String,
    allowlist: &ProductAllowlist,
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

/// `api.rs::gate_customer_reply_draft` と同じ契約: Issue #28 §3.5 応答側ゲート その 1/3
/// (回答下書き)。allowlist 外の型番言及があれば `None` に落とす。`decide_reply_action` の
/// decision table がそのまま `EscalationReply` へ自動的にフォールバックするため、ここで新しい
/// フォールバック文言を発明しない。
fn gate_customer_reply_draft(
    draft: Option<String>,
    allowlist: &ProductAllowlist,
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
            "customer reply draft mentions an out-of-scope product model; discarding the draft \
             (customer_reply_draft = null), which falls back to an escalation reply"
        );
        None
    } else {
        Some(draft)
    }
}

/// `api.rs::second_stage_out_of_scope_reply` と同じ契約: Issue #28 §3.1 二段目(LLM解釈 +
/// コード判定)。実体の判定は `product_gate::confirmed_foreign_reference`(`pub` 関数)、
/// 応答文の組み立ては `product_gate::build_out_of_scope_reply`(`pub` 関数)。
fn second_stage_out_of_scope_reply(
    product_references: &[product_gate::ProductReference],
    message: &str,
    allowlist: &ProductAllowlist,
    request_id: &str,
    case_id: &str,
) -> Option<String> {
    let reference =
        product_gate::confirmed_foreign_reference(product_references, message, allowlist)?;
    tracing::info!(
        request_id = %request_id,
        case_id = %case_id,
        surface = %reference.surface.trim(),
        "cs_support: question-side gate stage 2 (LLM catalog interpretation) classified the \
         message as an out-of-scope product reference; returning the canned out-of-scope reply \
         and discarding the evaluate() outcome"
    );
    Some(product_gate::build_out_of_scope_reply(
        reference.surface.trim(),
        allowlist,
    ))
}

/// `api.rs::SecondStageShortCircuit` と同じ契約: `run_support_turn` の二段目配線(evaluate()
/// 直後の判定・早期return・`demote_case_to_out_of_scope` への case_id/signal 伝搬)を純関数へ
/// 切り出す。I/O(demote の実呼び出し・応答組み立て)は呼び出し元が行う。
struct SecondStageShortCircuit {
    reply_text: String,
    case_id: String,
    discarded_signals: crate::harness::signal::SignalSet,
}

fn second_stage_short_circuit(
    outcome: &EvaluationOutcome,
    message: &str,
    allowlist: &ProductAllowlist,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::decision::{AnswerSource, DisclosureScope, EscalateReason, Stakes};
    use crate::harness::extraction::ExtractionMode;
    use crate::harness::product_gate::{ProductAllowlist, ProductGate};
    use crate::harness::signal::SignalSet;

    /// テスト用 schema。`api.rs::tests::TEST_SCHEMA` と同じ値(urtect)を使う必然性は無いが、
    /// 実データに近い値にしておく。
    const TEST_SCHEMA: &str = "urtect";

    /// テスト専用の最小 `Harness`。`knowledge: None` なので `evaluate()` を呼べば必ず失敗する。
    ///
    /// `api.rs::tests::test_harness` と同じ構成。あちらは private でこのモジュールから使えない
    /// ため、同じ構成をここで独立に組み立てる(api.rs 自身のテストコメントにも同種の前例がある)。
    fn test_harness(allowlist_models: Vec<&str>) -> Harness {
        let dir =
            std::env::temp_dir().join(format!("cs-support-test-harness-{}", uuid::Uuid::new_v4()));
        let lexicon = std::sync::Arc::new(
            crate::harness::signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap(),
        );
        Harness {
            authenticator: crate::harness::authn::Authenticator::new(vec![TEST_SCHEMA.to_string()]),
            normalizer: lexicon.clone(),
            extractor: std::sync::Arc::new(crate::harness::extraction::HybridExtractor::new(
                lexicon.clone(),
                None,
            )),
            lexicon,
            ng: crate::harness::egress::NgDictionary::from_json(
                r#"{"block_terms":[],"abstain_terms":[]}"#,
            )
            .unwrap(),
            worm: std::sync::Arc::new(
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
            product_gate: Some(ProductGate::seeded_for_test(
                TEST_SCHEMA,
                ProductAllowlist::from_models(
                    allowlist_models.into_iter().map(str::to_string).collect(),
                ),
            )),
        }
    }

    fn test_tool_service() -> ToolService {
        let client = crate::vegapunk::VegapunkClient::connect_lazy_with_limits(
            "http://vegapunk.invalid:6840",
            "",
            crate::vegapunk::GrpcLimits::default(),
        )
        .expect("lazy connect never touches the network");
        ToolService::new(client)
    }

    fn test_identity() -> VerifiedIdentity {
        VerifiedIdentity {
            sub: "cs-support-test-sub".to_string(),
            email: "cs-support-test@sivira.co".to_string(),
        }
    }

    fn default_api_config() -> ApiConfig {
        ApiConfig {
            enabled: true,
            clarify_max_turns: 3,
            ..Default::default()
        }
    }

    fn default_conv_state() -> CaseConvState {
        CaseConvState {
            clarify_turns: 0,
            awaiting_time_pref: false,
            time_pref_false_count: 0,
            preferred_contact_time: None,
            time_pref_extraction_error_count: 0,
        }
    }

    // ---- 質問側ゲート: 取扱外型番 → 定型応答(evaluate() を呼ばない) ----

    #[tokio::test]
    async fn run_support_turn_returns_a_canned_reply_for_an_out_of_scope_model_without_calling_evaluate(
    ) {
        // `knowledge: None` の harness で `evaluate()` が呼ばれれば必ず `Err` になる
        // (`Harness::knowledge()` が `self.knowledge.as_ref().ok_or_else(...)`)。この
        // テストが `Ok` を返すことそのものが「evaluate() を呼んでいない」ことの証明になる。
        let harness = test_harness(vec!["ADC-V724"]);
        let tools = test_tool_service();
        let api_cfg = default_api_config();
        let identity = test_identity();

        let outcome = run_support_turn(
            &harness,
            &tools,
            &api_cfg,
            &identity,
            TEST_SCHEMA,
            ManualSchemaKind::default(),
            "ADC-V999は使えますか",
            &[],
            None,
            None,
        )
        .await
        .expect("the question-side gate must short-circuit before evaluate() is ever called");

        assert_eq!(outcome.reply_kind, SupportReplyKind::OutOfScope);
        assert_eq!(outcome.reply_kind.as_str(), "out_of_scope");
        assert!(
            outcome.reply_text.contains("ADC-V999"),
            "reply must name the out-of-scope model: {}",
            outcome.reply_text
        );
        assert!(
            outcome.reply_text.contains("取り扱いがございません"),
            "reply must be the canned out-of-scope text: {}",
            outcome.reply_text
        );
        assert!(!outcome.case_id.is_empty());
    }

    #[tokio::test]
    async fn run_support_turn_passes_through_an_in_scope_model_to_the_evaluate_error_path() {
        // 対照テスト: allowlist 内の型番だけの発話は質問側ゲートを通過し、evaluate() へ進む。
        // `knowledge: None` の harness では evaluate() が必ず `Err` になるため、ここでは
        // `Err` が返ることそのものが「質問側ゲートで止まらず evaluate() まで進んだ」ことの
        // 確認になる。
        let harness = test_harness(vec!["ADC-V724"]);
        let tools = test_tool_service();
        let api_cfg = default_api_config();
        let identity = test_identity();

        let result = run_support_turn(
            &harness,
            &tools,
            &api_cfg,
            &identity,
            TEST_SCHEMA,
            ManualSchemaKind::default(),
            "ADC-V724の録画が見られません",
            &[],
            None,
            None,
        )
        .await;

        assert!(
            result.is_err(),
            "an in-scope-only message must reach evaluate() (which fails without a knowledge \
             store in this test harness)"
        );
    }

    // ---- build_post_evaluate_reply: evaluate() 後の分岐(実 vegapunk 不要) ----

    fn base_outcome(decision: AnswerDecision, clarification_allowed: bool) -> EvaluationOutcome {
        EvaluationOutcome {
            decision,
            signals: SignalSet::new(),
            accumulated_signals: SignalSet::new(),
            case_id: "case-12345678-abcd".to_string(),
            clarification_allowed,
            hits: Vec::new(),
            audit_event_id: "audit-1".to_string(),
            related_cases: Vec::new(),
            extraction_mode: ExtractionMode::LexiconOnly,
            customer_reply_draft: None,
            customer_reply_draft_truncated: false,
            product_references: Vec::new(),
            new_signals: SignalSet::new(),
        }
    }

    fn allowed_decision() -> AnswerDecision {
        AnswerDecision::Allowed {
            source: AnswerSource::Manual,
            evidence_section_keys: vec!["doc#sec1".to_string()],
            known_resolution_id: None,
            stakes: Stakes::Low,
            threshold: 0.8,
        }
    }

    fn escalate_decision() -> AnswerDecision {
        AnswerDecision::Escalate {
            reason: EscalateReason::InsufficientDirectness,
            layer: 3,
            route_to: "triage".to_string(),
            disclosure_scope: DisclosureScope::NoInternalDetails,
            audit_required: true,
            missing: vec![decision::EvidenceRequirement::DirectManualCoverage {
                required: 0.8,
                best: 0.5,
            }],
        }
    }

    #[tokio::test]
    async fn build_post_evaluate_reply_answers_when_allowed_with_a_non_truncated_draft() {
        let harness = test_harness(vec!["ADC-V724"]);
        let allowlist = harness.product_allowlist(TEST_SCHEMA).await.unwrap();
        let api_cfg = default_api_config();
        let mut conv = default_conv_state();
        let mut outcome = base_outcome(allowed_decision(), false);
        outcome.customer_reply_draft = Some("下書き本文です。".to_string());
        outcome.customer_reply_draft_truncated = false;

        let (kind, text) = build_post_evaluate_reply(
            &harness,
            &api_cfg,
            &allowlist,
            "req-1",
            "質問文",
            &[],
            false,
            &mut outcome,
            &mut conv,
        )
        .await;

        assert_eq!(kind, SupportReplyKind::Answer);
        assert_eq!(text, "下書き本文です。");
        assert_eq!(
            conv,
            default_conv_state(),
            "Answer は conv 状態を変更しない"
        );
    }

    #[tokio::test]
    async fn build_post_evaluate_reply_clarifies_when_gray_escalate_with_turns_remaining() {
        let harness = test_harness(vec!["ADC-V724"]);
        let allowlist = harness.product_allowlist(TEST_SCHEMA).await.unwrap();
        let api_cfg = default_api_config(); // clarify_max_turns = 3
        let mut conv = default_conv_state();
        conv.clarify_turns = 0; // < clarify_max_turns(3) - 1: 最終ターンではない
        let mut outcome = base_outcome(escalate_decision(), true);

        let (kind, text) = build_post_evaluate_reply(
            &harness,
            &api_cfg,
            &allowlist,
            "req-1",
            "質問文",
            &[],
            false,
            &mut outcome,
            &mut conv,
        )
        .await;

        assert_eq!(kind, SupportReplyKind::Clarify);
        assert_eq!(
            text,
            clarify::FALLBACK_CLARIFY_TEXT,
            "reply_drafter が None の harness では FALLBACK_CLARIFY_TEXT を返す。最終ターンでは \
             ないため CLARIFY_FINAL_TURN_SUFFIX は付かない"
        );
        assert_eq!(
            conv.clarify_turns, 1,
            "Clarify 分岐は clarify_turns を加算する"
        );
    }

    #[tokio::test]
    async fn build_post_evaluate_reply_escalates_when_clarify_turns_are_exhausted() {
        let harness = test_harness(vec!["ADC-V724"]);
        let allowlist = harness.product_allowlist(TEST_SCHEMA).await.unwrap();
        let api_cfg = default_api_config(); // clarify_max_turns = 3
        let mut conv = default_conv_state();
        conv.clarify_turns = 3; // == clarify_max_turns: 枯渇
        let mut outcome = base_outcome(escalate_decision(), true);

        let (kind, text) = build_post_evaluate_reply(
            &harness,
            &api_cfg,
            &allowlist,
            "req-1",
            "質問文",
            &[],
            false,
            &mut outcome,
            &mut conv,
        )
        .await;

        assert_eq!(kind, SupportReplyKind::Escalation);
        assert_eq!(
            text,
            escalation_reply::assemble_escalation_reply(
                escalation_reply::fallback_ack(false).0,
                &escalation_reply::build_deterministic_block(
                    &outcome.case_id,
                    &hours::business_hours_label(&api_cfg.business_hours),
                    !hours::is_within_business_hours(&api_cfg.business_hours, chrono::Utc::now()),
                )
            )
        );
        assert!(
            conv.awaiting_time_pref,
            "エスカレーション応答は希望時間帯の伺いを立てる"
        );
    }

    #[tokio::test]
    async fn build_post_evaluate_reply_escalates_when_clarification_is_not_allowed() {
        let harness = test_harness(vec!["ADC-V724"]);
        let allowlist = harness.product_allowlist(TEST_SCHEMA).await.unwrap();
        let api_cfg = default_api_config();
        let mut conv = default_conv_state();
        // 第1・2層起因の escalate は clarification_allowed = false(決定論)。
        let mut outcome = base_outcome(escalate_decision(), false);

        let (kind, _text) = build_post_evaluate_reply(
            &harness,
            &api_cfg,
            &allowlist,
            "req-1",
            "質問文",
            &[],
            false,
            &mut outcome,
            &mut conv,
        )
        .await;

        assert_eq!(kind, SupportReplyKind::Escalation);
    }

    #[tokio::test]
    async fn build_post_evaluate_reply_fails_closed_to_escalation_on_lexicon_fallback() {
        // Issue #28 C2(c): 今ターンの signal 抽出 LLM 呼び出しが失敗し LexiconFallback に
        // 落ちた場合、Allowed/Escalate の判定結果によらず常にエスカレーション応答へ倒す
        // (`crate::api::decide_reply_action` の fail-closed 分岐、CS 側から変更なしで再利用)。
        let harness = test_harness(vec!["ADC-V724"]);
        let allowlist = harness.product_allowlist(TEST_SCHEMA).await.unwrap();
        let api_cfg = default_api_config();
        let mut conv = default_conv_state();
        let mut outcome = base_outcome(allowed_decision(), false);
        outcome.customer_reply_draft = Some("下書き本文です。".to_string());
        outcome.customer_reply_draft_truncated = false;
        outcome.extraction_mode = ExtractionMode::LexiconFallback;

        let (kind, _text) = build_post_evaluate_reply(
            &harness,
            &api_cfg,
            &allowlist,
            "req-1",
            "質問文",
            &[],
            false,
            &mut outcome,
            &mut conv,
        )
        .await;

        assert_eq!(
            kind,
            SupportReplyKind::Escalation,
            "LexiconFallback は Allowed の判定結果を無視して必ずエスカレーションへ倒れる"
        );
    }

    // ---- 小さな純関数の単体テスト ----

    #[test]
    fn is_continuation_is_true_when_history_is_non_empty() {
        let history = vec![ReplyHistoryTurn {
            role: crate::harness::reply::ReplyHistoryRole::Customer,
            text: "前回の発話".to_string(),
        }];
        assert!(is_continuation(&history, None));
    }

    #[test]
    fn is_continuation_is_true_when_case_id_is_present() {
        assert!(is_continuation(&[], Some("case-1")));
    }

    #[test]
    fn is_continuation_is_false_for_a_fresh_conversation() {
        assert!(!is_continuation(&[], None));
    }

    #[test]
    fn note_time_pref_extraction_failure_third_time_clears_all_three_counters() {
        let mut conv = default_conv_state();
        conv.awaiting_time_pref = true;
        conv.time_pref_extraction_error_count = 2;
        conv.time_pref_false_count = 1;

        note_time_pref_extraction_failure(&mut conv);

        assert!(!conv.awaiting_time_pref, "3回連続で自動解除する");
        assert_eq!(conv.time_pref_false_count, 0);
        assert_eq!(conv.time_pref_extraction_error_count, 0);
    }

    #[test]
    fn arm_time_pref_solicitation_sets_awaiting_and_resets_false_and_clarify_counters() {
        let mut conv = default_conv_state();
        conv.time_pref_false_count = 2;
        conv.clarify_turns = 3;

        arm_time_pref_solicitation(&mut conv);

        assert!(conv.awaiting_time_pref);
        assert_eq!(conv.time_pref_false_count, 0);
        assert_eq!(conv.clarify_turns, 0);
    }

    #[test]
    fn missing_to_text_reports_the_required_and_current_scores() {
        let missing = vec![decision::EvidenceRequirement::DirectManualCoverage {
            required: 0.8,
            best: 0.5,
        }];
        let text = missing_to_text(&missing);
        assert!(text.contains("0.80"));
        assert!(text.contains("0.50"));
    }

    /// このモジュールの doc コメントは `missing_to_text` を「`api.rs::missing_to_text` と
    /// 同じ契約」だと宣言している。`contains("0.80")` 系のアサーションだけでは全角/半角括弧の
    /// 違いのような 1 文字差を検出できない(レビュー指摘4)。プロンプト入力にしか使わない
    /// 文字列だが、契約が同一だと主張する以上バイト単位で一致するはずなので、
    /// `api.rs::missing_to_text` が生成する文字列と完全一致させて固定する。
    #[test]
    fn missing_to_text_matches_api_rs_byte_for_byte() {
        let missing = vec![decision::EvidenceRequirement::DirectManualCoverage {
            required: 0.8,
            best: 0.5,
        }];
        let text = missing_to_text(&missing);
        // `api.rs::missing_to_text`(509〜511行目)と同じ全角括弧(U+FF08/U+FF09)を使う。
        assert_eq!(
            text,
            "マニュアルとの一致度が必要水準に届いていません（必要: 0.80 以上、現在: 0.50）"
        );
    }
}
