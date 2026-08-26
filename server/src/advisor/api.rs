//! `POST /{project_id}/api/reply`（homesec advisor 版、Task 6）。
//!
//! design doc `2026-08-17-homesec-advisor-design.md` §3.3（API 契約）・§6（処理順 11 手順）・
//! §8（障害時挙動）・§9（不変条件）の実装。CS(urtect) の `crate::api`
//! （`ReplyRequest` / `HistoryEntry` / `validate` / `authorize` / `error_response`）を
//! そのまま再利用し、応答型だけ advisor 固有(`AdvisorReplyResponse`)にする
//! （design doc §3.3: `product_cards` を加算するだけで CS 側の契約は変えない）。
//!
//! advisor は `Harness.reply_drafter` / `Harness.ng`（どちらも CS 用に構成される）を使わない。
//! LLM 呼び出しは advisor 専用の [`AdvisorApiState::llm`]、NG 辞書は advisor 専用の
//! [`AdvisorApiState::ng`] を使う（呼び出し元 `bin/homesec_advisor.rs` が
//! `AdvisorConfig` から個別に組み立てる）。

use crate::advisor::canned;
use crate::advisor::cards::{self, ProductCard};
use crate::advisor::cs_support;
use crate::advisor::decide::{self, AdvisorAction, AdvisorCaseAttrs};
use crate::advisor::draftgen::{self, DraftMode};
use crate::advisor::materials::{self, AdvisorMaterial};
use crate::advisor::quick_replies::{self, QuickReplyItem};
use crate::advisor::understand::{self, ConditionKey};
use crate::api::{authorize, validate, HistoryEntry, HistoryRole, ReplyRequest};
use crate::config::AppConfig;
use crate::harness::egress::NgDictionary;
use crate::harness::reply::{ReplyHistoryRole, ReplyHistoryTurn};
use crate::harness::time_pref;
use crate::harness::{CaseConvState, Harness};
use crate::llm::AnthropicClient;
use crate::oauth::VerifiedIdentity;
use crate::vegapunk::VegapunkClient;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `record_conversation_turn` の完了を待つ上限。`crate::api::ok_reply_response` と同じ値
/// （design doc の指示「定数値も5秒で揃えること」）。
const CONVERSATION_TURN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// `/{project_id}/api/reply` が共有する状態（design doc §3.1・Task 6 スコープ）。
///
/// CS の `crate::api::ApiState` とは別物: advisor 自身の通常パイプライン
/// （`understand`/`draftgen`/`decide`）は `Harness.reply_drafter` / `Harness.ng` を使わない。
/// advisor 固有の NG 辞書は CS 用と語彙が異なるため、`llm` / `ng` を専用フィールドに分離して
/// 持つ。`vegapunk` は `materials::gather_materials` が直接必要とする（`Harness` は
/// `VegapunkClient` を公開しないため、呼び出し元 `bin/homesec_advisor.rs` から別途渡す）。
///
/// **Issue #50 バッチ1（design doc §13、CS サポートモード）**: `support_harness` は上記
/// `harness` とは別の `Harness` インスタンスで、`[[projects]]` を `support_schema` 1 件だけに
/// 差し替えた `AppConfig` から構築する（`harness` は homesec 1 projectのまま、`support_harness`
/// の `Authenticator.allowed_schemas` だけが urtect を許可する。`bin/homesec_advisor.rs` の
/// doc コメント参照）。**`[harness]` セクション自体は `harness` と共有**のため、
/// `config.homesec.toml` が `customer_reply_draft_enabled = true` を立てて以降、`harness`
/// （admin 用）の `reply_drafter` も `Some` になるが、admin 経路（`admin.rs`）は
/// `Harness::evaluate` を一切呼ばないため実害は無い。`support_harness.reply_drafter` は
/// `crate::advisor::cs_support::run_support_turn` が CS と同じ聞き返し・受け止め文の下書き
/// 生成に使う（今回のバッチではフィールドを構築するのみで、実ハンドラへの配線はしない）。
#[derive(Clone)]
pub struct AdvisorApiState {
    pub config: Arc<AppConfig>,
    pub harness: Arc<Harness>,
    /// advisor 専用の `AnthropicClient`（`Harness.reply_drafter` は使わない）。
    pub llm: AnthropicClient,
    /// advisor 専用の NG 辞書（`Harness.ng` は使わない）。
    pub ng: Arc<NgDictionary>,
    /// `materials::gather_materials` 用（`Clone` は安価、`vegapunk.rs` 参照）。
    pub vegapunk: VegapunkClient,
    /// env `CS_SUPPORT_ANSWER_API_KEY` の値。ログ・Debug 出力に含めないため、この構造体は
    /// `Debug` を derive しない。
    pub api_key: String,
    /// 製品カード画像の同梱ディレクトリ（解決済み絶対パス。`cards::select_cards` と
    /// `ServeDir` の両方が同じパスを参照する）。
    pub images_dir: PathBuf,
    /// `ProductCard.image_url` へホストを付与するための、advisor 自身の公開ホスト名
    /// （`CS_SUPPORT_PUBLIC_DOMAIN`）。
    pub public_host: String,
    /// CS サポートモード（design doc §13）専用の `Harness`。`[[projects]]` を
    /// `support_schema` 1 件だけに差し替えた `AppConfig` から構築する（上記構造体 doc
    /// コメント参照）。`crate::advisor::cs_support::run_support_turn` の第一引数。
    pub support_harness: Arc<Harness>,
    /// `crate::advisor::cs_support::run_support_turn` に渡す `ToolService`
    /// （`ToolService::new(vegapunk.clone())`）。
    pub support_tools: crate::mcp::ToolService,
    /// CS サポートモードが実行する vegapunk schema（`AdvisorConfig.support_schema`、通常
    /// `"urtect"`）。
    pub support_schema: String,
    /// 上記 schema の manual 取得経路種別（`AdvisorConfig.support_manual_schema`）。
    pub support_manual_schema: crate::config::ManualSchemaKind,
}

/// `POST /{project_id}/api/reply` のレスポンス（design doc §3.3）。
///
/// `reply_text` / `case_id` は CS の `crate::api::ReplyResponse` と同形。`product_cards` は
/// advisor 固有の加算フィールドで、CS 側は常に省略する（design doc §3.3: 省略時のアダプタ挙動は
/// 従来どおりで後方互換）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AdvisorReplyResponse {
    pub reply_text: String,
    pub case_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_cards: Option<Vec<ProductCard>>,
    /// `clarify` / `time_pref` ターン、および `answer` ターンで `closing == question_choice`
    /// のときの選択肢(design doc §3.3 加算フィールド、Issue #34。2026-08-21
    /// conversation-rhythm-implementation §要件3 により `answer` ターンにも拡張)。
    /// CS 側は常に省略する(同上の理由)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_replies: Option<Vec<QuickReplyItem>>,
}

/// `/{project_id}/api/reply` の router を組み立てる。
pub fn advisor_api_router(state: AdvisorApiState) -> Router {
    Router::new()
        .route("/{project_id}/api/reply", post(advisor_reply_handler))
        .with_state(state)
}

/// advisor が支える案件は監査主体を持たない固定の service identity として扱う
/// （`crate::api::service_principal` と同じ発想。advisor は Google 認証を持たないため、
/// `RequestContext.actor` は「advisor answer-api 経由の呼び出しである」ことだけを表す）。
fn advisor_service_principal() -> VerifiedIdentity {
    VerifiedIdentity {
        sub: "service:homesec-answer-api".to_string(),
        email: "homesec-answer-api@cs-support.internal".to_string(),
    }
}

/// design doc §6 手順4「会話履歴の要約」の組み立て。`history` は時系列昇順（古い→新しい）で
/// 渡される契約（`crate::api::ReplyRequest.history` の doc コメントと同じ）。
///
/// ラベルは日本語(「顧客」「アドバイザー」)にする。`understand::build_understand_prompt` /
/// `draftgen::build_advisor_user_message` はこの文字列を不透明なテキストとして
/// `neutralize_delimiters` に通すため、ラベルの形式そのものはプロンプト契約に影響しない。
fn build_history_digest(history: &[HistoryEntry]) -> String {
    history
        .iter()
        .map(|entry| {
            let label = match entry.role {
                HistoryRole::Customer => "顧客",
                HistoryRole::Assistant => "アドバイザー",
            };
            format!("{label}: {}", entry.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// design doc §6 手順4「累積条件」を LLM Call #1 の `accumulated` 引数へ渡す形式へ整形する。
/// `understand::ConditionKey::as_str` の語彙表キー名をそのまま使う
/// （`materials::conditions_to_signal_set` の `"{key}:{value}"` 形式とは異なる見せ方でよい —
/// ここは LLM への参考情報であり、KR 照合用の signal 表現ではないため）。
fn build_accumulated_digest(conditions: &[(ConditionKey, String)]) -> String {
    conditions
        .iter()
        .map(|(key, value)| format!("{}:{value}", key.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// design doc §7.2 手順3「送出したら `shown_product_cards` へ追記する」の実装。
/// 既存 CSV を `csv_list` で分解し、今回送出したカードの `material_key` を追記して
/// 再度 CSV へ結合する。重複が起きないことは呼び出し元（`select_cards` の再表示抑止フィルタ）が
/// 既に保証済みなので、ここでは単純追記でよい。
fn append_shown_product_cards(existing_csv: &str, new_keys: &[&str]) -> String {
    let mut keys = crate::harness::knowledge::csv_list(existing_csv);
    keys.extend(new_keys.iter().map(|k| (*k).to_string()));
    keys.join(",")
}

/// design doc §6 手順7: `decide::AdvisorAction` の各 variant が doc コメントで定める
/// 「呼び出し側の契約」をここで一括して適用する。ハンドラは `match` で応答文を組み立てる
/// **前に** この関数を必ず 1 回呼ぶ(reviewer 指摘 Critical 2: 以前はこの 4 つの副作用が
/// `match` の各アーム内にインラインで書かれており、どれか 1 行消してもテストが 1 件も
/// 落ちなかった)。`match` 自体は応答文・`reply_kind`・materials の決定だけを担う。
///
/// **Issue #50 バッチ2(design doc §13.2)**: `support_mode` の書き戻しもここに集約する。
/// `AdvisorAction::SupportMode` は `support_mode = true`(入場・継続)。`TimePrefContinue` は
/// 触れない(advisor 自身のリード時間帯受付は CS 継続性を壊さない設計)。それ以外の全
/// アクション(`Safety` / `OutOfDomain` / `LeadSolicit` / `Clarify` / `LeadConfirmed` /
/// `Answer`)は退場とみなし `support_mode = false` に落とす(`support_case_id` はどの
/// アクションでもクリアしない。design doc §13.2: 退場後に再入場したとき、同じ CS case を
/// 再利用して会話を継続できるようにするため)。
///
/// **codex レビュー指摘 High(コミット 18f421d)**: 当初 `Safety` は `TimePrefContinue` と
/// 同様に触れない設計だった。これは誤りだった: `advisor_reply_handler` の `cs_continuing`
/// 計算は `advisor_attrs.support_mode` を起点にゲートしており、emergency ターンで
/// `support_mode` が true のまま残ると、次ターンが in_domain でない発話のとき
/// (`decide::decide` 優先順1)、または `support_last_reply_kind == "support_clarify"` が
/// 残ったまま次ターンで `cs_continuing` が短絡的に true になるとき(優先順4)、緊急案内の
/// 直後にもかかわらず advisor が誤って CS サポートモードへ迂回してしまう(「緊急案内後の
/// 次ターンは通常の入場判定からやり直す」という状態不変条件の違反)。そのため `Safety` も
/// 退場アクションに含める。
///
/// **Issue #50 バッチ2 レビュー修正 Critical 1**: `support_mode` を false に落とす同じ
/// アクションで `support_last_reply_kind` も空文字列にクリアする。退場時に stale な値
/// (例: 前回入場時の `"support_clarify"`)が残ったままだと、次にこの case が再入場したとき
/// `advisor_reply_handler` の `cs_continuing` 計算が `peek_conv_state` を呼ばずに誤って
/// 継続扱いにしてしまう(`decide::AdvisorCaseAttrs::support_last_reply_kind` の doc comment
/// 参照)。
fn apply_action_contract(
    action: &AdvisorAction,
    conv: &mut CaseConvState,
    advisor_attrs: &mut AdvisorCaseAttrs,
) {
    match action {
        AdvisorAction::Safety => {
            conv.awaiting_time_pref = false;
            conv.time_pref_false_count = 0;
            advisor_attrs.support_mode = false;
            advisor_attrs.support_last_reply_kind = String::new();
        }
        AdvisorAction::TimePrefContinue => {}
        AdvisorAction::SupportMode => {
            advisor_attrs.support_mode = true;
        }
        AdvisorAction::LeadSolicit => {
            conv.awaiting_time_pref = true;
            conv.time_pref_false_count = 0;
            conv.clarify_turns = 0;
            advisor_attrs.support_mode = false;
            advisor_attrs.support_last_reply_kind = String::new();
        }
        AdvisorAction::Clarify { .. } => {
            conv.clarify_turns += 1;
            advisor_attrs.support_mode = false;
            advisor_attrs.support_last_reply_kind = String::new();
        }
        AdvisorAction::LeadConfirmed { .. } => {
            advisor_attrs.lead_requested = true;
            advisor_attrs.support_mode = false;
            advisor_attrs.support_last_reply_kind = String::new();
        }
        AdvisorAction::OutOfDomain | AdvisorAction::Answer => {
            advisor_attrs.support_mode = false;
            advisor_attrs.support_last_reply_kind = String::new();
        }
    }
}

/// design doc §4.4 手順1: リード提案文を実際に出したターンだけ `lead_offered` を焼く。
///
/// `drafted_text` は `draft_with_materials` の戻り値そのもの(`to_plain_text` 適用前でよい —
/// `draftgen::draft_advisor_reply` は fallback 文字列も含め、既に `to_plain_text` を通した値を
/// 返すため、ここでの再適用は冪等: `draftgen.rs` の `apply_advisor_output_gates`)。
///
/// 3 条件をすべて満たすときだけ true を返す(このターンで「焼く」べきと判断する)。呼び出し側は
/// `true` のときだけ `advisor_attrs.lead_offered = true` を代入し、`false` のときは何も
/// 代入しない(既に `true` だった値を書き戻さないので、二重提案の抑止は自然に保たれる):
/// - `!already_offered`(1 会話に 1 回までの抑止)
/// - `drafted_text != canned::FALLBACK_TEXT`(fallback へ倒れたターンでは焼かない。fallback は
///   顧客に一切提案を見せていない)
/// - `drafted_text.contains(draftgen::LEAD_OFFER_MARKER)`(実際にリード提案文言を含むターンだけ)
///
/// (reviewer 指摘 Critical 1: 以前は `draft_with_materials` を呼んだターンなら、LLM が実際に
/// 提案したかに関わらず無条件で `lead_offered = true` を焼いており、design doc §2.3 の
/// 能動的リード獲得経路が実質常に発火しなくなっていた)。
///
/// `LEAD_OFFER_MARKER` は提案文そのものに近い長さの文字列であり、単なる「担当者」ではない
/// (codex レビュー2巡目 Critical: 「担当者」は一般語で、「担当者に連絡する必要はありません」の
/// ような否定文や、design doc §5.2 の `partner_product` 材料が言及する他社サービスの説明文
/// (「警備会社の担当者が駆けつけます」等)でも出現し、提案していないターンで機会を焼いて
/// しまっていた。`draftgen::LEAD_OFFER_MARKER` の doc comment に誤検知シナリオの詳細がある)。
///
/// **残る制約**: この判定は文字列照合である以上、LLM が指示に反してこの一文を言い換えた場合は
/// 検知漏れになり、同一会話で2回提案されうる(design doc §9 不変条件6 違反)。恒久対処は
/// `draft_advisor_reply` の戻り値を構造化して「提案文を挿入したか」を型で返すことだが、それは
/// draftgen の契約変更を伴うため別スコープとする。
fn should_burn_lead_offered(already_offered: bool, drafted_text: &str) -> bool {
    !already_offered
        && drafted_text != canned::FALLBACK_TEXT
        && drafted_text.contains(draftgen::LEAD_OFFER_MARKER)
}

/// 2次 codex レビュー Warning B 是正: `AdvisorAction::Clarify` 経路で Call#2 から返ってきた
/// `meta` を上位(`select_cards` / `question_streak` 更新)へ渡してよいかを決める純関数。
///
/// `DraftMode::Clarify` は design doc §4.3 手順6「1問だけ聞き返す」契約のターンであり、
/// メタ出力を一切指示しない。通常は `meta == None` のはずだが、LLM の逸脱やプロンプト
/// インジェクションが有効な `proposal` メタ(featured 付き)を返した場合、それをそのまま
/// 上位へ渡すと「締めが質問のターンに商品カードが付く」という design doc §2.4 違反
/// (Issue #34 実害 (a) の再発経路)になる。「通常は None のはず」というコメントは安全境界に
/// ならないため、ここでモード境界を LLM 出力に依存しない決定論的なゲートにする: Clarify
/// 経路で取れたメタは常に破棄する。
fn clarify_meta_for_upstream(
    _llm_meta: Option<draftgen::DraftMeta>,
) -> Option<draftgen::DraftMeta> {
    None
}

/// design doc §8「vegapunk 検索失敗 → 製品カードは添付しない」の一般化。`reply_kind ==
/// "fallback"` のターンは答えを返せていない(材料検索自体は成功していても、下書きは顧客に
/// 一切見えていない)ため、`cards::select_cards` を呼ばず製品カードを添付しない(reviewer 指摘
/// Warning 1: fallback の詫び文の直後にカルーセルが出る、かつ `shown_product_cards` への誤った
/// 追記で design doc §7.2 手順3 の再表示抑止が誤発火するのを防ぐ)。
fn should_select_cards(reply_kind: &str) -> bool {
    reply_kind != "fallback"
}

/// design doc §6 手順10「support_case への書き込み」を組み立てる純関数。
///
/// **契約**: `existing`（手順2で読んだ既存属性、新規 case なら空 map）を出発点にし、
/// 以下だけを上書きする。`turn_count` を含むそれ以外の既存キーには一切触れない
/// （vegapunk の `UpsertNodes` は全置換のため、ここで拾わなかったキーは呼び出し元が渡す
/// map から消え、結果として support_case から消える）。
/// - `case_id` / `question`: 常に今回の値で上書き
/// - `created_at`: 既存に無ければ `now_rfc3339` を設定、あれば既存を保持（上書きしない）
/// - `end_user_id`: 既存に無く、かつ `end_user_id` が `Some` のときだけ設定。それ以外は
///   既存を保持（上書きしない）
/// - conv state 5 属性(`clarify_turns` / `awaiting_time_pref` / `time_pref_false_count` /
///   `preferred_contact_time` / `time_pref_extraction_error_count`): 常に `conv` の値で上書き
///   （`harness::mod::merge_conv_state_attributes` と同じキー・同じ形。あちらは `harness` に
///   private なため advisor 側で複製する — 実装計画 Task 6 の指示どおり、1 回の `record()`
///   呼び出しに全属性をまとめるため）
/// - `decide::advisor_attr_updates` の3属性、`decide::accumulated_condition_updates` の
///   最大5属性: 常に上書き（後者は非空の値だけを書く契約は `accumulated_condition_updates`
///   自身が担保する）
#[allow(clippy::too_many_arguments)]
fn merge_support_case_attrs(
    existing: &HashMap<String, String>,
    case_id: &str,
    question: &str,
    end_user_id: Option<&str>,
    conv: &CaseConvState,
    advisor_attrs: &AdvisorCaseAttrs,
    conditions: &[(ConditionKey, String)],
    now_rfc3339: &str,
) -> Vec<(String, String)> {
    let mut merged = existing.clone();
    merged.insert("case_id".to_string(), case_id.to_string());
    merged.insert("question".to_string(), question.to_string());
    merged
        .entry("created_at".to_string())
        .or_insert_with(|| now_rfc3339.to_string());
    if !merged.contains_key("end_user_id") {
        if let Some(id) = end_user_id {
            merged.insert("end_user_id".to_string(), id.to_string());
        }
    }
    merged.insert("clarify_turns".to_string(), conv.clarify_turns.to_string());
    merged.insert(
        "awaiting_time_pref".to_string(),
        conv.awaiting_time_pref.to_string(),
    );
    merged.insert(
        "time_pref_false_count".to_string(),
        conv.time_pref_false_count.to_string(),
    );
    merged.insert(
        "preferred_contact_time".to_string(),
        conv.preferred_contact_time.clone().unwrap_or_default(),
    );
    merged.insert(
        "time_pref_extraction_error_count".to_string(),
        conv.time_pref_extraction_error_count.to_string(),
    );
    for (key, value) in decide::advisor_attr_updates(advisor_attrs) {
        merged.insert(key, value);
    }
    for (key, value) in decide::accumulated_condition_updates(conditions) {
        merged.insert(key, value);
    }
    merged.into_iter().collect()
}

/// `state.harness.store()` / `KnowledgeStore` 呼び出しの失敗を 500 応答へ変換する。
/// 内部エラー文字列は運用者向けログにのみ残し、クライアントへは `request_id` だけを返す
/// （`crate::api` の 500 応答と同じ規律。CS の
/// `reply_returns_500_without_leaking_internal_error_when_knowledge_unavailable` テストが
/// 固定している契約と揃える）。
fn internal_error_response(err: &anyhow::Error, request_id: &str, step: &str) -> Response {
    tracing::error!(
        error = ?err,
        request_id = %request_id,
        step,
        "homesec advisor answer api: internal error"
    );
    crate::api::error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        format!("internal error processing the request; request_id={request_id}"),
    )
}

/// `AdvisorAction::SupportMode` 経路(`run_cs_support_mode_turn` → `cs_support::run_support_turn`、
/// `harness.begin` / `harness.product_allowlist` / `harness.load_conv_state` /
/// `harness.save_conv_state` / `Harness::evaluate` の vegapunk gRPC 呼び出しを含む)の失敗を
/// HTTP へ変換する。
///
/// codex レビュー指摘 Medium(コミット 18f421d): 当初は `internal_error_response` で一律 500 に
/// 倒しており、vegapunk 到達不能(tonic エラー)でも 500 になっていた。運用者が「vegapunk 障害」と
/// 「advisor 自身のバグ」を切り分けられるよう、分類関数そのものは CS 側 `crate::api::reply_handler`
/// と共有する(同一 crate 内に公開された `crate::api::classify_evaluate_error`(`pub fn`)と
/// `crate::api::error_response`(`pub(crate) fn`)をそのまま呼ぶ。CS ファイルは変更しない)。
///
/// **ただし適用範囲は CS 側と非対称**。CS 側 `reply_handler`(`server/src/api.rs` の
/// `Err(err) =>` 分岐)は `classify_evaluate_error` を `evaluate` の失敗にだけ適用し、
/// `conv_state_load_failed` / `conv_state_save_failed` / `product_allowlist_fetch_failed` は
/// 呼び出し元で常に 500 に倒している。一方 `run_cs_support_mode_turn` →
/// `cs_support::run_support_turn` は `harness.begin` / `harness.product_allowlist` /
/// `harness.load_conv_state` / `harness.save_conv_state` / `evaluate` の**すべての失敗**を `?`
/// で素通し伝播するため、この関数にはそれら全段階のエラーが届く。方針は「vegapunk 到達不能は
/// 段階を問わず 503 に倒す(運用者が『vegapunk 障害』と『advisor 自身のバグ』を切り分けられる
/// ことを優先する)」。CS 側 `reply_handler` が evaluate 以外を常に 500 にしているのは意図的な
/// 実装であり、ここではそれに揃えていない(揃える場合は CS 側の変更を要するため今回のスコープ外)。
fn support_turn_error_response(err: &anyhow::Error, request_id: &str) -> Response {
    let (status, code) = crate::api::classify_evaluate_error(err);
    if status == StatusCode::SERVICE_UNAVAILABLE {
        tracing::warn!(
            request_id = %request_id,
            error = ?err,
            "homesec advisor answer api: cs support turn failed due to upstream (vegapunk) \
             unavailability"
        );
        crate::api::error_response(
            status,
            code,
            format!("vegapunk is unavailable (request_id={request_id}); please retry"),
        )
    } else {
        tracing::error!(
            request_id = %request_id,
            error = ?err,
            "homesec advisor answer api: cs support turn failed"
        );
        crate::api::error_response(
            status,
            code,
            format!("internal error (request_id={request_id}); see server logs"),
        )
    }
}

/// design doc §13.3: CS 応答種別(`cs_support::SupportReplyKind`)を advisor 自身の
/// `ConversationTurn.reply_kind` へ写像する。catch-all を書かない(将来 variant が増えたときに
/// コンパイルエラーで気づけるようにするため。このファイルの他の網羅的 match と同じ流儀)。
fn support_reply_kind_to_advisor(kind: cs_support::SupportReplyKind) -> &'static str {
    use cs_support::SupportReplyKind;
    match kind {
        SupportReplyKind::Answer => "support_answer",
        SupportReplyKind::Clarify => "support_clarify",
        SupportReplyKind::Escalation => "support_escalation",
        SupportReplyKind::TimePref => "support_time_pref",
        SupportReplyKind::OutOfScope => "support_out_of_scope",
    }
}

/// design doc §13.2 手順4: SupportMode のターンは CS の応答をそのまま返し、製品カード・チップ・
/// リード提案を一切出さない。`materials`(空 Vec)・`quick_replies`(None)・`meta`(None)を固定で
/// 返すことで、後続の `select_cards` / `quick_replies::for_answer` 組み立てが自然に「何も
/// 出さない」へ倒れることを保証する(reviewer 指摘 Critical 2 と同じ流儀: この抑止をハンドラ
/// 本体にインラインで書くと、将来 materials/meta を渡すよう書き換えられてもテストで検出
/// できない。ここへ切り出すことで単体テストで固定できる)。
fn support_mode_reply_parts(
    outcome: cs_support::SupportReplyOutcome,
) -> (
    String,
    &'static str,
    Vec<AdvisorMaterial>,
    Option<Vec<QuickReplyItem>>,
    Option<draftgen::DraftMeta>,
) {
    (
        outcome.reply_text,
        support_reply_kind_to_advisor(outcome.reply_kind),
        Vec::new(),
        None,
        None,
    )
}

/// `crate::api::HistoryEntry`(advisor の answer-api 契約が受け取る形)を
/// `crate::harness::reply::ReplyHistoryTurn`(`cs_support::run_support_turn` が要求する形)へ
/// 変換する。ロールの対応関係以外に意味を持たない純関数。
fn convert_history_for_support(history: &[HistoryEntry]) -> Vec<ReplyHistoryTurn> {
    history
        .iter()
        .map(|entry| ReplyHistoryTurn {
            role: match entry.role {
                HistoryRole::Customer => ReplyHistoryRole::Customer,
                HistoryRole::Assistant => ReplyHistoryRole::Assistant,
            },
            text: entry.text.clone(),
        })
        .collect()
}

/// design doc §13.2(SupportMode)の呼び出し構築: `cs_support::run_support_turn` を urtect
/// schema(`state.support_schema`)で呼ぶための薄い glue。ハンドラ本体から切り出しているのは
/// テスト容易性のため(reviewer 指摘 Critical 2 と同じ流儀: インラインで書くと、schema の
/// 取り違え・`case_id` の `Some`/`None` 変換ミスをテストで検出できない)。
///
/// **契約**: `advisor_attrs.support_case_id` が空文字列なら新規 CS case として `None` を渡し、
/// 非空ならその case を継続する `Some(&str)` を渡す。呼び出しが成功したら、返ってきた
/// `outcome.case_id` を `advisor_attrs.support_case_id` へ**常に**書き戻す(同じ値でも冪等)。
/// 失敗時は `advisor_attrs` を一切変更せず、そのまま `Err` を呼び出し元へ伝播する
/// (呼び出し元 `advisor_reply_handler` が `support_turn_error_response` へ変換する)。
async fn run_cs_support_mode_turn(
    state: &AdvisorApiState,
    identity: &VerifiedIdentity,
    message: &str,
    history: &[HistoryEntry],
    end_user_id: Option<&str>,
    advisor_attrs: &mut AdvisorCaseAttrs,
) -> anyhow::Result<cs_support::SupportReplyOutcome> {
    let support_history = convert_history_for_support(history);
    let case_id = if advisor_attrs.support_case_id.is_empty() {
        None
    } else {
        Some(advisor_attrs.support_case_id.as_str())
    };
    let outcome = cs_support::run_support_turn(
        &state.support_harness,
        &state.support_tools,
        &state.config.api,
        identity,
        &state.support_schema,
        state.support_manual_schema,
        message,
        &support_history,
        case_id,
        end_user_id,
    )
    .await?;
    advisor_attrs.support_case_id = outcome.case_id.clone();
    // Issue #50 バッチ2 レビュー修正 Critical 1: 「直前の CS 応答が聞き返しだったか」を advisor
    // 側の case 属性へ書き戻す(CS 側の `clarify_turns` は累積カウンタで継続状態を表せないため。
    // `decide::AdvisorCaseAttrs::support_last_reply_kind` の doc comment 参照)。
    advisor_attrs.support_last_reply_kind =
        support_reply_kind_to_advisor(outcome.reply_kind).to_string();
    Ok(outcome)
}

/// design doc §6 手順6・8 の「材料検索・KR 照合・下書き生成」をまとめた薄いラッパー。
/// ネットワーク呼び出し（vegapunk 検索・known_resolution 読み込み・LLM Call #2）を伴うため
/// 単体テスト対象外（`understand::understand` / `draftgen::draft_advisor_reply` と同じ方針）。
///
/// known_resolution 読み込みが失敗しても request を失敗させない（warn して空 Vec のまま続行、
/// design doc §8 と同じ「材料ゼロでも Call#2 は実行する」寛容スキップ）。
///
/// `product_intent` は design doc §6 手順6「own_product の保証注入」のトリガー
/// （`understanding.product_intent`。累積条件に `concern` がある場合も同様に注入する）。
/// 検索が own_product を引けず接地規則が製品提案を封じる本番実害（背景 (b)）への決定論対処。
#[allow(clippy::too_many_arguments)]
async fn draft_with_materials(
    state: &AdvisorApiState,
    schema: &str,
    mode: DraftMode<'_>,
    conditions: &[(ConditionKey, String)],
    search_query: &str,
    message: &str,
    history_digest: &str,
    is_continuation: bool,
    lead_offered: bool,
    question_streak: i32,
    product_intent: bool,
) -> (String, Option<draftgen::DraftMeta>, Vec<AdvisorMaterial>) {
    let resolutions = match state.harness.store() {
        Ok(store) => match store.load_known_resolutions(schema).await {
            Ok(list) => list,
            Err(err) => {
                tracing::warn!(
                    error = %format!("{err:#}"),
                    schema,
                    "homesec advisor: load_known_resolutions failed; continuing without a KR \
                     match (design doc §8 degrade-not-block policy)"
                );
                Vec::new()
            }
        },
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                schema,
                "homesec advisor: knowledge store unavailable while loading known resolutions; \
                 continuing without a KR match"
            );
            Vec::new()
        }
    };
    let kr = materials::match_advisor_known_resolution(&resolutions, conditions);
    let (searched, own_products, category_pool) =
        materials::gather_materials(&state.vegapunk, schema, search_query, conditions).await;
    let mut composed = materials::compose_materials(kr, searched);
    let concern_category = materials::concern_category(conditions);
    if materials::should_guarantee_own_products(product_intent, conditions) {
        let guaranteed = materials::select_own_product_materials(&own_products, concern_category);
        composed = materials::inject_guaranteed_own_products(composed, guaranteed);
    }
    // design doc §6 手順6「category 合致材料」: own_product 保証注入のトリガー条件
    // (product_intent / concern の有無)には依存させない。`select_category_materials` 自体が
    // `concern_category == None` のとき空を返すことで「累積条件に concern がある限り常に
    // 試みる」を表現する。
    let category_candidates =
        materials::select_category_materials(&category_pool, concern_category);
    composed = materials::inject_category_materials(composed, category_candidates);
    let (text, meta) = draftgen::draft_advisor_reply(
        &state.llm,
        mode,
        &composed,
        conditions,
        message,
        history_digest,
        is_continuation,
        lead_offered,
        question_streak,
        &state.ng,
    )
    .await;
    (text, meta, composed)
}

/// `POST /{project_id}/api/reply`（design doc §6 の 11 手順そのもの）。
async fn advisor_reply_handler(
    State(state): State<AdvisorApiState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<ReplyRequest>, JsonRejection>,
) -> Response {
    // 手順1: 認可・project 解決・入力検証。
    if !authorize(&headers, &state.api_key) {
        return crate::api::error_response(
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
        return crate::api::error_response(
            StatusCode::NOT_FOUND,
            "unknown_project",
            format!("no project configured for project_id={project_id}"),
        );
    };
    let req = match body {
        Ok(Json(req)) => req,
        Err(rejection) => {
            return crate::api::error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                rejection.to_string(),
            );
        }
    };
    if let Err(message) = validate(&req) {
        return crate::api::error_response(StatusCode::BAD_REQUEST, "invalid_request", message);
    }

    let identity = advisor_service_principal();
    let ctx = match state
        .harness
        .begin(&identity, &project.schema, project.manual_schema)
    {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::error!(
                error = ?err,
                project_id = %project_id,
                "homesec advisor answer api: harness.begin failed"
            );
            return crate::api::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to start the request; see server logs",
            );
        }
    };

    // 手順2: case_id を確定し、既存属性を読む。
    let case_id = req
        .case_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let store = match state.harness.store() {
        Ok(store) => store,
        Err(err) => return internal_error_response(&err, &ctx.request_id, "store"),
    };
    let existing_attrs = match store.load_case(&project.schema, &case_id).await {
        Ok(attrs) => attrs.unwrap_or_default(),
        Err(err) => return internal_error_response(&err, &ctx.request_id, "load_case"),
    };

    // 手順3: 会話状態と advisor 固有の case 属性を復元する。
    let mut conv = match state.harness.load_conv_state(&ctx, &case_id).await {
        Ok(conv) => conv,
        Err(err) => return internal_error_response(&err, &ctx.request_id, "load_conv_state"),
    };
    let mut advisor_attrs = decide::parse_advisor_case_attrs(&existing_attrs);
    let accumulated_conditions = decide::parse_accumulated_conditions(&existing_attrs);

    // 手順4: 会話履歴の要約 + LLM Call #1(理解)。
    let history: &[HistoryEntry] = req.history.as_deref().unwrap_or(&[]);
    let history_digest = build_history_digest(history);
    let accumulated_digest = build_accumulated_digest(&accumulated_conditions);
    let understanding_result = understand::understand(
        &state.llm,
        &req.message,
        &history_digest,
        &accumulated_digest,
    )
    .await;

    let (reply_text, reply_kind, final_conditions, materials_used, quick_reply_items, draft_meta): (
        String,
        &'static str,
        Vec<(ConditionKey, String)>,
        Vec<AdvisorMaterial>,
        Option<Vec<QuickReplyItem>>,
        Option<draftgen::DraftMeta>,
    ) = match understanding_result {
        Err(err) => {
            // design doc §4.3 末尾・§8: 理解 LLM が(内部で1回再試行しても)失敗したら
            // fallback 定型を返す。手順5〜9はすべてスキップする。fallback ターンは
            // quick_replies を出さない(仕様: reply_kind="fallback"では出さない)。
            tracing::warn!(
                error = %err,
                request_id = %ctx.request_id,
                case_id = %case_id,
                "homesec advisor: understanding call failed; returning the fallback reply \
                 (design doc §4.3 tail / §8)"
            );
            (
                canned::FALLBACK_TEXT.to_string(),
                "fallback",
                accumulated_conditions.clone(),
                Vec::new(),
                None,
                None,
            )
        }
        Ok(mut understanding) => {
            // 手順5: 累積条件を merge する(decide の契約: 渡す conditions は累積済み全量)。
            understanding.conditions =
                decide::merge_conditions(&accumulated_conditions, &understanding.conditions);
            let is_continuation = !history.is_empty() || req.case_id.is_some();

            // Issue #50 バッチ2(design doc §13.2 rule 2(a)): CS 側(urtect schema)が継続状態
            // (聞き返し中・時間帯受付中)にあるかを、decide 系関数を呼ぶ前に確定させる
            // (`decide::decide` は非同期にできないため、time_pref 抽出と同じ理由で呼び出し元に
            // 前倒しさせる設計。decide.rs の `cs_continuing` 引数 doc comment 参照)。
            // - emergency ターンは確認しない(常に false でよい。110番案内の遅延を避けるため
            //   time_pref 抽出 LLM 呼び出しを emergency 時にスキップしているのと同じ理由)。
            // - support_mode == false、または CS case がまだ無い(support_case_id が空)ときは
            //   vegapunk 往復が無駄なので false を即値で返す。
            //
            // レビュー修正 Critical 1: 「聞き返し中」は CS 側 conv state(`clarify_turns`)からは
            // 判定できない(累積カウンタのため。`cs_support::cs_continuing_from_conv_state` の
            // doc comment 参照)。advisor 側で保持する `support_last_reply_kind` が
            // `"support_clarify"` ならその時点で継続確定なので、`peek_conv_state` の vegapunk
            // 往復は行わない(短絡評価: ローカル判定を先に評価する)。それ以外は従来どおり
            // `peek_conv_state`(いまは `awaiting_time_pref` のみを見る)で時間帯受付中かどうかを
            // 確認する。
            let cs_continuing = if understanding.emergency {
                false
            } else if !advisor_attrs.support_mode || advisor_attrs.support_case_id.is_empty() {
                false
            } else if advisor_attrs.support_last_reply_kind == "support_clarify" {
                true
            } else {
                cs_support::peek_conv_state(
                    &state.support_harness,
                    &identity,
                    &state.support_schema,
                    state.support_manual_schema,
                    &advisor_attrs.support_case_id,
                )
                .await
            };

            // 手順6: 応答種別を決める。emergency は decide::decide /
            // decide::decide_time_pref / decide::decide_time_pref_extraction_failed の
            // いずれもが最優先で Safety を返す契約だが(decide.rs の各関数冒頭)、時間帯受付
            // モード中はそれらを呼ぶ前に `time_pref::extract_time_preference`(LLM 呼び出し)を
            // 経由してしまうと、110番案内という最も遅延に敏感な応答が無駄な LLM 往復1回分だけ
            // 遅れる(reviewer 指摘 Warning 3)。ここで先に emergency を判定して Safety に
            // 倒すのは、後続の 3 関数の呼び出しを省略するだけで分岐結果を変えない(3 関数は
            // どれも冒頭で emergency を見て conv を変更せず Safety を返すため、この前倒しは
            // 分岐結果として完全に等価)。
            let action = if understanding.emergency {
                AdvisorAction::Safety
            } else if conv.awaiting_time_pref {
                match time_pref::extract_time_preference(&state.llm, &req.message).await {
                    Ok(extraction) => decide::decide_time_pref(
                        &understanding,
                        &extraction,
                        &mut conv,
                        &state.config.api.business_hours,
                        advisor_attrs.lead_requested,
                        state.config.api.clarify_max_turns,
                        advisor_attrs.support_mode,
                        cs_continuing,
                    ),
                    Err(_) => decide::decide_time_pref_extraction_failed(
                        &understanding,
                        &mut conv,
                        advisor_attrs.lead_requested,
                        state.config.api.clarify_max_turns,
                        advisor_attrs.support_mode,
                        cs_continuing,
                    ),
                }
            } else {
                decide::decide(
                    &understanding,
                    &conv,
                    advisor_attrs.lead_offered,
                    advisor_attrs.lead_requested,
                    state.config.api.clarify_max_turns,
                    advisor_attrs.support_mode,
                    cs_continuing,
                )
            };

            // 手順7: action を canned/生成 応答へ変換する。呼び出し側の契約(各 variant の
            // doc comment 参照)は match の前に一括で適用する(`apply_action_contract`)。
            // match 自体は応答文・reply_kind・materials の組み立てだけを担う。
            apply_action_contract(&action, &mut conv, &mut advisor_attrs);
            let (text, kind, materials_list, quick_replies, meta): (
                String,
                &'static str,
                Vec<AdvisorMaterial>,
                Option<Vec<QuickReplyItem>>,
                Option<draftgen::DraftMeta>,
            ) = match action {
                AdvisorAction::Safety => (
                    canned::SAFETY_TEXT.to_string(),
                    "safety",
                    Vec::new(),
                    None,
                    None,
                ),
                AdvisorAction::TimePrefContinue => (
                    canned::lead_time_pref_reask(&state.config.api.business_hours),
                    "time_pref",
                    Vec::new(),
                    quick_replies::for_time_pref(),
                    None,
                ),
                AdvisorAction::OutOfDomain => (
                    canned::OUT_OF_DOMAIN_TEXT.to_string(),
                    "out_of_domain",
                    Vec::new(),
                    None,
                    None,
                ),
                AdvisorAction::SupportMode => match run_cs_support_mode_turn(
                    &state,
                    &identity,
                    &req.message,
                    history,
                    req.end_user_id.as_deref(),
                    &mut advisor_attrs,
                )
                .await
                {
                    Ok(outcome) => support_mode_reply_parts(outcome),
                    Err(err) => {
                        return support_turn_error_response(&err, &ctx.request_id);
                    }
                },
                AdvisorAction::LeadSolicit => (
                    canned::lead_solicit(&state.config.api.business_hours),
                    "time_pref",
                    Vec::new(),
                    quick_replies::for_time_pref(),
                    None,
                ),
                AdvisorAction::LeadConfirmed { slot } => (
                    canned::lead_confirmed(&slot),
                    "lead",
                    Vec::new(),
                    None,
                    None,
                ),
                AdvisorAction::Clarify { missing } => {
                    // design doc §6 手順6: 検索クエリは「累積条件 + 相談要旨」。
                    // understanding.summary_ja が §4.1 の「相談要旨」そのもの。
                    // question_streak はここでは更新しない(要件2: 更新対象は
                    // `DraftMode::Answer` がメタ付きで成功したターンのみ)。
                    let (text, meta, materials_list) = draft_with_materials(
                        &state,
                        &project.schema,
                        DraftMode::Clarify { missing: &missing },
                        &understanding.conditions,
                        &understanding.summary_ja,
                        &req.message,
                        &history_digest,
                        is_continuation,
                        advisor_attrs.lead_offered,
                        advisor_attrs.question_streak,
                        understanding.product_intent,
                    )
                    .await;
                    // 2次 codex レビュー Warning B 是正: Clarify 経路のメタは常に破棄する
                    // (clarify_meta_for_upstream 参照。quick_replies 生成は従来どおり
                    // `for_clarify` の固定語彙のみを使い、この meta には依存しない)。
                    let meta = clarify_meta_for_upstream(meta);
                    if should_burn_lead_offered(advisor_attrs.lead_offered, &text) {
                        advisor_attrs.lead_offered = true;
                    }
                    let kind = if text == canned::FALLBACK_TEXT {
                        "fallback"
                    } else {
                        "clarify"
                    };
                    // fallback に差し替わったターンは quick_replies も出さない(仕様)。
                    let quick_replies = if kind == "fallback" {
                        None
                    } else {
                        quick_replies::for_clarify(&missing)
                    };
                    (text, kind, materials_list, quick_replies, meta)
                }
                AdvisorAction::Answer => {
                    let (text, meta, materials_list) = draft_with_materials(
                        &state,
                        &project.schema,
                        DraftMode::Answer,
                        &understanding.conditions,
                        &understanding.summary_ja,
                        &req.message,
                        &history_digest,
                        is_continuation,
                        advisor_attrs.lead_offered,
                        advisor_attrs.question_streak,
                        understanding.product_intent,
                    )
                    .await;
                    if should_burn_lead_offered(advisor_attrs.lead_offered, &text) {
                        advisor_attrs.lead_offered = true;
                    }
                    // 要件2: question_streak の更新は Answer モードの Call#2 がメタ付きで
                    // 成功したターンのみ(fallback へ差し替わったターンは draftgen 側で既に
                    // meta = None になっている。draftgen::draft_advisor_reply の doc comment
                    // 参照)。
                    advisor_attrs.question_streak = decide::next_question_streak(
                        advisor_attrs.question_streak,
                        meta.as_ref().map(|m| m.closing),
                    );
                    let kind = if text == canned::FALLBACK_TEXT {
                        "fallback"
                    } else {
                        "answer"
                    };
                    // 要件5: quick_replies は reply_kind == "answer" のときだけ、Call#2 の
                    // メタ(closing == QuestionChoice かつ choices 非空)から生成する。fallback
                    // へ差し替わったターンは出さない(仕様: fallback は quick_replies なし)。
                    let quick_replies = if kind == "answer" {
                        quick_replies::for_answer(meta.as_ref())
                    } else {
                        None
                    };
                    (text, kind, materials_list, quick_replies, meta)
                }
            };
            (
                text,
                kind,
                understanding.conditions,
                materials_list,
                quick_replies,
                meta,
            )
        }
    };

    // 出口関門を通過した最終応答文(正規化後)。canned 文言は元々 Markdown を含まないため
    // 冪等に通る(`crate::api::ok_reply_response` と同じ「一律で通す」方針)。
    let final_text = crate::harness::prompt_input::to_plain_text(&reply_text);

    // 手順9: 製品カードの添付判定(design doc §7.2、2026-08-21
    // conversation-rhythm-implementation §要件3)。canned 応答(Safety/OutOfDomain/...)は
    // materials_used が常に空なので select_cards は自然に空 Vec を返す(=呼ばなかったのと
    // 同じ結果になる)。fallback ターン(`reply_kind == "fallback"`)だけは select_cards 自体を
    // 呼ばない(`should_select_cards` 参照。reviewer 指摘 Warning 1: 答えを返せていないターンに
    // カードを出さない、かつ shown_product_cards への誤った追記を防ぐ)。`draft_meta` は
    // `DraftMode::Answer` がメタ付きで成功したターンだけ `Some` になり、`closing != Proposal`
    // (`draft_meta` が `None` の場合を含む)なら `select_cards` 自身が空を返す。
    let mut selected_cards = if should_select_cards(reply_kind) {
        cards::select_cards(
            &final_text,
            &materials_used,
            &advisor_attrs.shown_product_cards,
            &state.images_dir,
            draft_meta.as_ref(),
            &state.ng,
        )
    } else {
        Vec::new()
    };
    let product_cards: Option<Vec<ProductCard>> = if selected_cards.is_empty() {
        None
    } else {
        for card in selected_cards.iter_mut() {
            if let Some(rel) = card.image_url.take() {
                card.image_url = Some(format!("https://{}{}", state.public_host, rel));
            }
        }
        let new_keys: Vec<&str> = selected_cards
            .iter()
            .map(|c| c.material_key.as_str())
            .collect();
        advisor_attrs.shown_product_cards =
            append_shown_product_cards(&advisor_attrs.shown_product_cards, &new_keys);
        Some(selected_cards)
    };

    // design doc §6 末尾: 内部判断の info ログ(顧客メッセージ本文・応答本文は出さない)。
    // `selected_cards` は非空の場合 `product_cards` へ move 済みのため、カードの material_key
    // は `product_cards` 側から読む。
    log_turn_decision(
        &ctx.request_id,
        &case_id,
        reply_kind,
        &materials_used,
        product_cards.as_deref(),
        draft_meta.as_ref(),
        advisor_attrs.question_streak,
    );

    // 手順10: support_case への書き込み(1 回だけ)。
    let now = chrono::Utc::now().to_rfc3339();
    let merged_attrs = merge_support_case_attrs(
        &existing_attrs,
        &case_id,
        &req.message,
        req.end_user_id.as_deref(),
        &conv,
        &advisor_attrs,
        &final_conditions,
        &now,
    );
    if let Err(err) = store
        .record(&project.schema, "support_case", &case_id, merged_attrs)
        .await
    {
        return internal_error_response(&err, &ctx.request_id, "record_support_case");
    }

    // 手順11: ConversationTurn の永続化(応答優先・5秒上限)+ 応答。advisor は WORM 監査を
    // 持たないため audit_event_id は空文字列(`crate::api::ok_reply_response` と同じ設計)。
    match tokio::time::timeout(
        CONVERSATION_TURN_WRITE_TIMEOUT,
        store.record_conversation_turn(
            &project.schema,
            &case_id,
            req.end_user_id.as_deref(),
            &req.message,
            &final_text,
            reply_kind,
            "",
        ),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                error = ?err,
                case_id = %case_id,
                reply_kind,
                "homesec advisor answer api: failed to record a ConversationTurn; continuing \
                 without blocking the reply (design doc §8: turn loss is tolerated)"
            );
        }
        Err(_elapsed) => {
            tracing::warn!(
                case_id = %case_id,
                reply_kind,
                timeout_secs = CONVERSATION_TURN_WRITE_TIMEOUT.as_secs(),
                "homesec advisor answer api: gave up waiting for a ConversationTurn write at the \
                 timeout; returning the reply anyway (design doc §8: turn loss is tolerated)"
            );
        }
    }

    (
        StatusCode::OK,
        Json(AdvisorReplyResponse {
            reply_text: final_text,
            case_id,
            product_cards,
            quick_replies: quick_reply_items,
        }),
    )
        .into_response()
}

/// design doc §6 末尾「内部判断の info ログ」本体。`advisor_reply_handler` から切り出した
/// テスト可能な純関数(reviewer 指摘 Critical 1: このログは「顧客メッセージ本文・応答本文を
/// 一切出さない」プライバシー特性を持つが、ハンドラ内にインラインで書かれている限り
/// `capture_logs` で固定できず、将来の編集で本文を引数に足す変更が無警告で入りうる状態
/// だった)。
///
/// ログに載せるのは request_id / case_id / action_kind(応答種別)/ material_keys(注入した
/// 材料の material_key。`{kind}:{slug}` 形式で kind を含む)/ card_keys(選定したカードの
/// material_key)/ closing(Call#2 のメタの締め方。メタが無いターンは `"none"`)/
/// featured_len(メタの featured 件数。メタが無ければ0)/ question_streak(このターン終了後に
/// 書き戻す最終値)の8つのみ。
/// `materials_used` の `body_ja` / `title_ja`、`message` / 応答本文は一切渡さない・出さない。
/// reviewer 一次レビュー Major 4 是正: カード・チップが出るかどうかを決める入力
/// (`closing` / `featured` / `question_streak`)が従来出ておらず、本番で「なぜこのターンに
/// カードが出なかったか」を運用者が特定できなかった。`choices` の中身(顧客向け文言)と
/// `featured` の値そのもの(材料の中身が推測できる)は出さず、件数だけに留める。
fn log_turn_decision(
    request_id: &str,
    case_id: &str,
    reply_kind: &str,
    materials_used: &[AdvisorMaterial],
    product_cards: Option<&[ProductCard]>,
    draft_meta: Option<&draftgen::DraftMeta>,
    question_streak: i32,
) {
    tracing::info!(
        request_id = %request_id,
        case_id = %case_id,
        action_kind = reply_kind,
        material_keys = ?materials_used
            .iter()
            .map(|m| m.material_key.clone())
            .collect::<Vec<String>>(),
        card_keys = ?product_cards
            .map(|cards| cards.iter().map(|c| c.material_key.clone()).collect::<Vec<String>>())
            .unwrap_or_default(),
        closing = draft_meta.map(|m| m.closing.as_str()).unwrap_or("none"),
        featured_len = draft_meta.map(|m| m.featured.len()).unwrap_or(0),
        question_streak,
        "homesec advisor turn decision"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LlmConfig, ManualSchemaKind};

    // ---- build_history_digest ----

    #[test]
    fn build_history_digest_returns_empty_string_when_no_history() {
        assert_eq!(build_history_digest(&[]), "");
    }

    #[test]
    fn build_history_digest_labels_customer_and_assistant_lines_in_order() {
        let history = vec![
            HistoryEntry {
                role: HistoryRole::Customer,
                text: "玄関の防犯が心配です".to_string(),
            },
            HistoryEntry {
                role: HistoryRole::Assistant,
                text: "詳しく教えてください".to_string(),
            },
        ];
        assert_eq!(
            build_history_digest(&history),
            "顧客: 玄関の防犯が心配です\nアドバイザー: 詳しく教えてください"
        );
    }

    // ---- build_accumulated_digest ----

    #[test]
    fn build_accumulated_digest_returns_empty_string_when_no_conditions() {
        assert_eq!(build_accumulated_digest(&[]), "");
    }

    #[test]
    fn build_accumulated_digest_formats_key_colon_value_lines() {
        let conditions = vec![
            (ConditionKey::Housing, "apartment_rented".to_string()),
            (ConditionKey::Concern, "intrusion".to_string()),
        ];
        assert_eq!(
            build_accumulated_digest(&conditions),
            "housing:apartment_rented\nconcern:intrusion"
        );
    }

    // ---- append_shown_product_cards ----

    #[test]
    fn append_shown_product_cards_appends_to_existing_csv() {
        assert_eq!(
            append_shown_product_cards(
                "own_product:adc-v724",
                &["partner_product:alsok", "own_product:adc-v523"]
            ),
            "own_product:adc-v724,partner_product:alsok,own_product:adc-v523"
        );
    }

    #[test]
    fn append_shown_product_cards_from_empty_existing_yields_only_new_keys() {
        assert_eq!(
            append_shown_product_cards("", &["own_product:adc-v724"]),
            "own_product:adc-v724"
        );
    }

    #[test]
    fn append_shown_product_cards_returns_existing_unchanged_when_no_new_keys() {
        assert_eq!(
            append_shown_product_cards("own_product:adc-v724", &[]),
            "own_product:adc-v724"
        );
    }

    // ---- apply_action_contract ----
    // decide::AdvisorAction の各 variant の doc comment が定める「呼び出し側の契約」を、
    // ここで実際に検証する(reviewer 指摘 Critical 2: この契約を担う行を 1 行消しても
    // 以前はテストが全部 green のままだった)。

    #[test]
    fn apply_action_contract_safety_disarms_time_pref_and_resets_false_count() {
        // decide::AdvisorAction::Safety の doc comment: 忘れると緊急対応中も時間帯受付
        // モードが armed のまま残り、次ターンが必ず希望時間帯抽出パスへ入ってしまう。
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        conv.time_pref_false_count = 2;
        let mut attrs = base_advisor_attrs();

        apply_action_contract(&AdvisorAction::Safety, &mut conv, &mut attrs);

        assert!(!conv.awaiting_time_pref);
        assert_eq!(conv.time_pref_false_count, 0);
        assert_eq!(
            attrs,
            base_advisor_attrs(),
            "Safety must leave lead_offered / lead_requested / shown_product_cards / \
             question_streak / support_case_id untouched (support_mode and \
             support_last_reply_kind are already false/empty in base_advisor_attrs, so \
             equality still holds)"
        );
    }

    #[test]
    fn apply_action_contract_lead_solicit_arms_time_pref_and_resets_clarify_turns() {
        // decide::AdvisorAction::LeadSolicit の doc comment: 忘れると時間帯受付モードへ
        // 遷移せず、リードフローが開始しない。
        let mut conv = base_conv();
        conv.awaiting_time_pref = false;
        conv.time_pref_false_count = 1;
        conv.clarify_turns = 2;
        let mut attrs = base_advisor_attrs();

        apply_action_contract(&AdvisorAction::LeadSolicit, &mut conv, &mut attrs);

        assert!(conv.awaiting_time_pref);
        assert_eq!(conv.time_pref_false_count, 0);
        assert_eq!(conv.clarify_turns, 0);
        assert_eq!(
            attrs,
            base_advisor_attrs(),
            "LeadSolicit must leave lead_offered / lead_requested / shown_product_cards / \
             question_streak / support_case_id untouched (support_mode and \
             support_last_reply_kind are already false/empty in base_advisor_attrs, so \
             equality still holds)"
        );
    }

    #[test]
    fn apply_action_contract_clarify_increments_clarify_turns_from_zero() {
        // decide::AdvisorAction::Clarify の doc comment: 忘れると clarify_turns が永久に
        // 0 のままになり、clarify 予算ゲートが機能せず Clarify を無限に返し続ける。
        let mut conv = base_conv();
        conv.clarify_turns = 0;
        let mut attrs = base_advisor_attrs();

        apply_action_contract(
            &AdvisorAction::Clarify {
                missing: vec![ConditionKey::Concern],
            },
            &mut conv,
            &mut attrs,
        );

        assert_eq!(conv.clarify_turns, 1);
    }

    #[test]
    fn apply_action_contract_clarify_increments_clarify_turns_from_nonzero() {
        let mut conv = base_conv();
        conv.clarify_turns = 2;
        let mut attrs = base_advisor_attrs();

        apply_action_contract(
            &AdvisorAction::Clarify {
                missing: vec![ConditionKey::Housing],
            },
            &mut conv,
            &mut attrs,
        );

        assert_eq!(conv.clarify_turns, 3);
    }

    #[test]
    fn apply_action_contract_lead_confirmed_sets_lead_requested() {
        // decide::AdvisorAction::LeadConfirmed の doc comment: 忘れると同一会話で再び
        // LeadSolicit が発火しうる。
        let mut conv = base_conv();
        let mut attrs = base_advisor_attrs();

        apply_action_contract(
            &AdvisorAction::LeadConfirmed {
                slot: "平日13:00〜15:00".to_string(),
            },
            &mut conv,
            &mut attrs,
        );

        assert!(attrs.lead_requested);
        assert_eq!(conv, base_conv(), "LeadConfirmed must not touch conv state");
    }

    #[test]
    fn apply_action_contract_time_pref_continue_touches_nothing() {
        let mut conv = base_conv();
        conv.clarify_turns = 1;
        conv.awaiting_time_pref = true;
        conv.time_pref_false_count = 1;
        let mut attrs = base_advisor_attrs();
        attrs.lead_offered = true;

        let conv_before = conv.clone();
        let attrs_before = attrs.clone();

        apply_action_contract(&AdvisorAction::TimePrefContinue, &mut conv, &mut attrs);

        assert_eq!(
            conv, conv_before,
            "TimePrefContinue must not touch conv state"
        );
        assert_eq!(
            attrs, attrs_before,
            "TimePrefContinue must not touch advisor attrs"
        );
    }

    // ---- apply_action_contract: Issue #50 バッチ2(design doc §13.2)の support_mode 契約 ----

    #[test]
    fn apply_action_contract_safety_clears_support_mode_after_clarify_continuation() {
        // codex レビュー指摘 High(コミット 18f421d): 「サポート聞き返し中 → emergency」
        // シナリオ。当初の実装は Safety で support_mode / support_last_reply_kind に一切
        // 触れず、emergency 後も CS サポート継続状態が残っていた。advisor_reply_handler の
        // cs_continuing 計算は support_mode を起点にゲートしているため、次ターンが in_domain
        // でない発話のとき(decide::decide 優先順1)、あるいは support_last_reply_kind ==
        // "support_clarify" が残ったまま cs_continuing が短絡的に true になるとき(優先順4)、
        // 緊急案内の直後にもかかわらず advisor が誤って CS サポートモードへ迂回してしまう
        // (「緊急案内後の次ターンは通常の入場判定からやり直す」という状態不変条件の違反)。
        let mut conv = base_conv();
        let mut attrs = base_advisor_attrs();
        attrs.support_mode = true;
        attrs.support_last_reply_kind = "support_clarify".to_string();
        attrs.support_case_id = "case-123".to_string();

        apply_action_contract(&AdvisorAction::Safety, &mut conv, &mut attrs);

        assert!(
            !attrs.support_mode,
            "Safety must clear support_mode after a support-clarify continuation"
        );
        assert!(
            attrs.support_last_reply_kind.is_empty(),
            "Safety must clear support_last_reply_kind after a support-clarify continuation"
        );
        assert_eq!(
            attrs.support_case_id, "case-123",
            "support_case_id must be preserved (unlike support_mode) so a later re-entry can \
             reuse the same CS case"
        );
    }

    #[test]
    fn apply_action_contract_safety_clears_support_mode_after_time_pref_continuation() {
        // 「サポート時間帯受付中 → emergency」シナリオ。上のテストと同じ不変条件を
        // support_last_reply_kind == "support_time_pref" で固定する。
        let mut conv = base_conv();
        let mut attrs = base_advisor_attrs();
        attrs.support_mode = true;
        attrs.support_last_reply_kind = "support_time_pref".to_string();
        attrs.support_case_id = "case-456".to_string();

        apply_action_contract(&AdvisorAction::Safety, &mut conv, &mut attrs);

        assert!(
            !attrs.support_mode,
            "Safety must clear support_mode after a support-time_pref continuation"
        );
        assert!(
            attrs.support_last_reply_kind.is_empty(),
            "Safety must clear support_last_reply_kind after a support-time_pref continuation"
        );
        assert_eq!(
            attrs.support_case_id, "case-456",
            "support_case_id must be preserved so a later re-entry can reuse the same CS case"
        );
    }

    // codex レビュー指摘 High の回帰テスト本体: apply_action_contract(Safety, ...) が
    // support_mode を false に落とした結果を実際に decide::decide まで通し、次ターンが
    // decide_in_domain_flow の優先順1・優先順4 のどちらからも SupportMode へ迂回しないことを
    // 証明する。1 件のテストで両方の優先順を同時に踏むと、片方の分岐だけが実装 2 行の削除に
    // 反応しない(=判別できない)組み合わせを見落とすため、優先順ごとに分ける。

    #[test]
    fn apply_action_contract_safety_after_support_mode_prevents_priority1_reroute_into_support_mode(
    ) {
        // 優先順1(decide_in_domain_flow: `!in_domain && support_mode` → SupportMode)の判別。
        // 「emergency 直後の、advisor の in_domain 判定に載らない発話」を模して in_domain =
        // false にする。cs_continuing はこの分岐の条件に含まれないため false を渡す
        // (advisor_reply_handler の契約上も、support_mode が false のときは常に false で
        // 決定される)。
        //
        // 判別性: apply_action_contract の Safety アームから
        // `advisor_attrs.support_mode = false;` を revert すると、attrs.support_mode が true の
        // まま decide::decide に渡り、優先順1 が成立して SupportMode が返る → 末尾の assert_eq!
        // が red になる。
        let mut conv = base_conv();
        let mut attrs = base_advisor_attrs();
        attrs.support_mode = true;
        attrs.support_last_reply_kind = "support_clarify".to_string();

        apply_action_contract(&AdvisorAction::Safety, &mut conv, &mut attrs);
        // 注意: ここで `!attrs.support_mode` を先に precondition assert すると、実装 2 行を
        // revert したときにその assert が真っ先に red になり、下の decide::decide 経由の
        // assert_eq! が一度も評価されず「判別できたつもり」になる(codex レビュー指摘の再現)。
        // そのため precondition を置かず、attrs.support_mode をそのまま decide::decide へ渡す。

        let next_turn_understanding = understand::Understanding {
            in_domain: false,
            emergency: false,
            urtect_support: false,
            lead_interest: false,
            product_intent: false,
            summary_ja: "テスト用の要約".to_string(),
            conditions: Vec::new(),
        };
        let next_conv = base_conv();

        let action = decide::decide(
            &next_turn_understanding,
            &next_conv,
            attrs.lead_offered,
            attrs.lead_requested,
            3,
            attrs.support_mode,
            false,
        );

        assert_eq!(
            action,
            AdvisorAction::OutOfDomain,
            "emergency 応答の直後、in_domain でない次ターンの発話は優先順1で SupportMode へ \
             迂回せず OutOfDomain になること"
        );
    }

    #[test]
    fn apply_action_contract_safety_after_support_mode_prevents_priority4_reroute_into_support_mode(
    ) {
        // 優先順4(decide_in_domain_flow: `support_mode && cs_continuing` → SupportMode)の
        // 判別。次ターンは in_domain な通常相談とし、conditions に concern を積んで
        // missing_conditions を空にすることで、優先順4 を通過すれば手順7(Answer)まで落ちる形に
        // する。cs_continuing = true を明示的に渡し、優先順4 の右辺を成立させる。
        //
        // 判別性: apply_action_contract の Safety アームから
        // `advisor_attrs.support_mode = false;` を revert すると、attrs.support_mode が true の
        // まま decide::decide に渡り、`support_mode && cs_continuing`(true && true)が成立して
        // SupportMode が返る → 末尾の assert_eq! が red になる。
        let mut conv = base_conv();
        let mut attrs = base_advisor_attrs();
        attrs.support_mode = true;
        attrs.support_last_reply_kind = "support_clarify".to_string();

        apply_action_contract(&AdvisorAction::Safety, &mut conv, &mut attrs);
        // 注意: ここで `!attrs.support_mode` を先に precondition assert すると、実装 2 行を
        // revert したときにその assert が真っ先に red になり、下の decide::decide 経由の
        // assert_eq! が一度も評価されず「判別できたつもり」になる(codex レビュー指摘の再現)。
        // そのため precondition を置かず、attrs.support_mode をそのまま decide::decide へ渡す。

        let next_turn_understanding = understand::Understanding {
            in_domain: true,
            emergency: false,
            urtect_support: false,
            lead_interest: false,
            product_intent: false,
            summary_ja: "テスト用の要約".to_string(),
            conditions: vec![(ConditionKey::Concern, "monitoring".to_string())],
        };
        let next_conv = base_conv();

        let action = decide::decide(
            &next_turn_understanding,
            &next_conv,
            attrs.lead_offered,
            attrs.lead_requested,
            3,
            attrs.support_mode,
            true,
        );

        assert_eq!(
            action,
            AdvisorAction::Answer,
            "emergency 応答の直後、CS 側が継続状態(cs_continuing=true)でも支援モードには \
             優先順4で迂回せず通常の Answer になること"
        );
    }

    #[test]
    fn apply_action_contract_time_pref_continue_does_not_touch_support_mode_when_already_true() {
        // decide::AdvisorAction::TimePrefContinue も Safety と同じ設計(advisor 自身のリード
        // 時間帯受付は CS 継続性を壊さない)。
        let mut conv = base_conv();
        let mut attrs = base_advisor_attrs();
        attrs.support_mode = true;

        apply_action_contract(&AdvisorAction::TimePrefContinue, &mut conv, &mut attrs);

        assert!(attrs.support_mode);
    }

    #[test]
    fn apply_action_contract_support_mode_sets_support_mode_true_and_does_not_touch_conv() {
        // design doc §13.2 rule 1・2(入場・継続): SupportMode を返したら support_mode = true。
        let mut conv = base_conv();
        let conv_before = conv.clone();
        let mut attrs = base_advisor_attrs();

        apply_action_contract(&AdvisorAction::SupportMode, &mut conv, &mut attrs);

        assert!(attrs.support_mode);
        assert_eq!(conv, conv_before, "SupportMode must not touch conv state");
    }

    #[test]
    fn apply_action_contract_clears_support_mode_for_out_of_domain_lead_solicit_clarify_lead_confirmed_and_answer(
    ) {
        // design doc §13.2 rule 3(退場): SupportMode 以外の全アクションは support_mode を
        // false へ落とす。既存の各 variant 固有の副作用(clarify_turns 加算・lead_requested
        // セット等)は上の個別テストで既に固定済みなので、ここでは support_mode のクリアだけを
        // 横断的に検証する。
        //
        // Issue #50 バッチ2 レビュー修正 Critical 1: 同じアクションで `support_last_reply_kind`
        // も空文字列にクリアされることを固定する。退場時に stale な値(前回入場時の
        // "support_clarify")が残ると、次に再入場したときの `cs_continuing` 計算
        // (`advisor_reply_handler`)が `peek_conv_state` を呼ばずに誤って継続扱いにしてしまう。
        let actions = [
            AdvisorAction::OutOfDomain,
            AdvisorAction::LeadSolicit,
            AdvisorAction::Clarify {
                missing: vec![ConditionKey::Concern],
            },
            AdvisorAction::LeadConfirmed {
                slot: "平日13:00〜15:00".to_string(),
            },
            AdvisorAction::Answer,
        ];
        for action in actions {
            let mut conv = base_conv();
            let mut attrs = base_advisor_attrs();
            attrs.support_mode = true;
            attrs.support_last_reply_kind = "support_clarify".to_string();

            apply_action_contract(&action, &mut conv, &mut attrs);

            assert!(!attrs.support_mode, "{action:?} must clear support_mode");
            assert_eq!(
                attrs.support_last_reply_kind, "",
                "{action:?} must clear support_last_reply_kind"
            );
        }
    }

    // ---- should_burn_lead_offered ----

    #[test]
    fn should_burn_lead_offered_true_when_draft_contains_the_marker_and_not_yet_offered() {
        let text = format!("ご希望であれば{}。", draftgen::LEAD_OFFER_MARKER);
        assert!(should_burn_lead_offered(false, &text));
    }

    #[test]
    fn should_burn_lead_offered_false_when_draft_does_not_contain_the_marker() {
        assert!(!should_burn_lead_offered(
            false,
            "まずは施錠の徹底をおすすめします。"
        ));
    }

    #[test]
    fn should_burn_lead_offered_false_when_the_draft_is_the_fallback_text() {
        // FALLBACK_TEXT はマーカーを含まない(下の fallback_text_never_contains_the_
        // lead_offer_marker で固定)ため実質は二重ガードだが、fallback を明示的に除外する
        // 契約そのものをここで固定する。
        assert!(!should_burn_lead_offered(false, canned::FALLBACK_TEXT));
    }

    #[test]
    fn should_burn_lead_offered_false_when_already_offered_even_if_marker_present() {
        let text = format!("{}。", draftgen::LEAD_OFFER_MARKER);
        assert!(
            !should_burn_lead_offered(true, &text),
            "already-true means the caller does not assign, so the flag stays true \
             (二重提案の抑止)"
        );
    }

    #[test]
    fn should_burn_lead_offered_false_for_a_negated_mention_of_the_marker_word() {
        // codex レビュー2巡目 Critical の回帰テスト: 「担当者」は一般語であり、否定文でも
        // 出現する。このターンは提案していないので、機会(1会話1回までのリード提案枠)を
        // 焼いてはならない。旧マーカー("担当者")では誤って true を返し、以後リード提案が
        // 永久にできなくなっていた。
        assert!(!should_burn_lead_offered(
            false,
            "担当者に連絡する必要はありません。まずはご自身で確認してみてください。"
        ));
    }

    #[test]
    fn should_burn_lead_offered_false_for_a_partner_service_mention_of_the_marker_word() {
        // codex レビュー2巡目 Critical の回帰テスト: design doc §5.2 の partner_product
        // 材料には警備会社サービスの説明が含まれ、「担当者」という語が他社サービスの文脈で
        // 出うる。このターンも提案していないので、機会を焼いてはならない。
        assert!(!should_burn_lead_offered(
            false,
            "警備会社の担当者が駆けつけるサービスもあります。"
        ));
    }

    // --- support_turn_error_response(codex レビュー指摘 Medium、コミット 18f421d) ---
    // run_cs_support_mode_turn の失敗を一律 500 に倒していたのを、CS 側
    // crate::api::classify_evaluate_error と同じ 503 upstream_unavailable / 500 internal の
    // 分類に揃える(vegapunk 到達不能を internal 扱いすると運用者が障害箇所を切り分けられない)。
    // server/src/api.rs の classify_evaluate_error テスト(2317〜2346行付近)と同じ入力パターンを
    // support_turn_error_response 経由で固定する。

    #[tokio::test]
    async fn support_turn_error_response_maps_tonic_status_to_503() {
        let err = anyhow::Error::from(tonic::Status::unavailable("vegapunk down"));
        let response = support_turn_error_response(&err, "req-1");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).expect("error responses are always JSON");
        assert_eq!(
            value["error"], "upstream_unavailable",
            "503 でも error コードが internal のままだと運用者が vegapunk 障害と advisor 自身の \
             バグを切り分けられない"
        );
    }

    #[tokio::test]
    async fn support_turn_error_response_maps_plain_error_to_500() {
        let err = anyhow::anyhow!("boom");
        let response = support_turn_error_response(&err, "req-1");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).expect("error responses are always JSON");
        assert_eq!(value["error"], "internal");
    }

    // --- clarify_meta_for_upstream(2次 codex レビュー Warning B) ---

    #[test]
    fn clarify_meta_for_upstream_discards_a_leaked_llm_meta() {
        // LLM の逸脱・プロンプトインジェクションで DraftMode::Clarify のターンでも有効な
        // proposal メタ(featured 付き)が返ってきたケース。Clarify は「締めが質問」の契約
        // ターンなので、これをそのまま上位へ渡すと商品カードが付いてしまう
        // (design doc §2.4 違反、Issue #34 実害 (a) の再発経路)。常に破棄することを固定する。
        let leaked = draftgen::DraftMeta {
            featured: vec!["own_product:adc-v724".to_string()],
            closing: draftgen::ClosingKind::Proposal,
            choices: Vec::new(),
        };
        assert_eq!(clarify_meta_for_upstream(Some(leaked)), None);
    }

    #[test]
    fn clarify_meta_for_upstream_is_a_noop_when_there_is_no_meta() {
        // 通常経路(DraftMode::Clarify はメタ出力を指示しないので meta == None)でも
        // そのまま None を返すことを固定する。
        assert_eq!(clarify_meta_for_upstream(None), None);
    }

    #[test]
    fn fallback_text_never_contains_the_lead_offer_marker() {
        assert!(!canned::FALLBACK_TEXT.contains(draftgen::LEAD_OFFER_MARKER));
    }

    // ---- should_select_cards ----

    #[test]
    fn should_select_cards_false_for_fallback_reply_kind() {
        // reviewer 指摘 Warning 1: fallback ターンは答えを返せていないため、材料検索が
        // 成功していてもカードを添付しない。
        assert!(!should_select_cards("fallback"));
    }

    #[test]
    fn should_select_cards_true_for_non_fallback_reply_kinds() {
        for kind in [
            "answer",
            "clarify",
            "safety",
            "out_of_domain",
            "time_pref",
            "lead",
            "support_answer",
            "support_clarify",
            "support_escalation",
            "support_time_pref",
            "support_out_of_scope",
        ] {
            assert!(should_select_cards(kind), "{kind}");
        }
    }

    // ---- fallback 判定の前提(reviewer 指摘 Warning 2) ----

    #[test]
    fn fallback_text_survives_plain_text_normalization_unchanged() {
        // reply_kind = "fallback" の判定は `text == canned::FALLBACK_TEXT` の等値比較に
        // 依存しており、その前提は draft_advisor_reply が to_plain_text を通した文字列を
        // 返すこと(draftgen.rs)。この恒等性が破れると全 fallback ターンが answer/clarify
        // として記録される(無言の回帰)。
        assert_eq!(
            crate::harness::prompt_input::to_plain_text(canned::FALLBACK_TEXT),
            canned::FALLBACK_TEXT
        );
    }

    // ---- merge_support_case_attrs ----

    fn base_conv() -> CaseConvState {
        CaseConvState {
            clarify_turns: 1,
            awaiting_time_pref: false,
            time_pref_false_count: 0,
            preferred_contact_time: None,
            time_pref_extraction_error_count: 0,
        }
    }

    fn base_advisor_attrs() -> AdvisorCaseAttrs {
        AdvisorCaseAttrs {
            lead_offered: false,
            lead_requested: false,
            shown_product_cards: String::new(),
            question_streak: 0,
            support_mode: false,
            support_case_id: String::new(),
            support_last_reply_kind: String::new(),
        }
    }

    #[test]
    fn merge_support_case_attrs_preserves_unrelated_existing_keys() {
        // turn_count はこの関数の関心事(conv state / advisor attrs / conditions)の外側の
        // 既存キー。read-merge-write の「read」として素通りすることを固定する。
        let mut existing = HashMap::new();
        existing.insert("turn_count".to_string(), "3".to_string());
        existing.insert(
            "actor".to_string(),
            "service:homesec-answer-api".to_string(),
        );

        let attrs = merge_support_case_attrs(
            &existing,
            "case-1",
            "質問文",
            None,
            &base_conv(),
            &base_advisor_attrs(),
            &[],
            "2026-08-18T00:00:00Z",
        );
        let map: HashMap<String, String> = attrs.into_iter().collect();
        assert_eq!(map.get("turn_count").map(String::as_str), Some("3"));
        assert_eq!(
            map.get("actor").map(String::as_str),
            Some("service:homesec-answer-api")
        );
    }

    #[test]
    fn merge_support_case_attrs_always_overwrites_case_id_and_question() {
        let mut existing = HashMap::new();
        existing.insert("case_id".to_string(), "stale-id".to_string());
        existing.insert("question".to_string(), "古い質問".to_string());

        let attrs = merge_support_case_attrs(
            &existing,
            "case-1",
            "新しい質問",
            None,
            &base_conv(),
            &base_advisor_attrs(),
            &[],
            "2026-08-18T00:00:00Z",
        );
        let map: HashMap<String, String> = attrs.into_iter().collect();
        assert_eq!(map.get("case_id").map(String::as_str), Some("case-1"));
        assert_eq!(map.get("question").map(String::as_str), Some("新しい質問"));
    }

    #[test]
    fn merge_support_case_attrs_sets_created_at_when_missing() {
        let existing = HashMap::new();
        let attrs = merge_support_case_attrs(
            &existing,
            "case-1",
            "質問文",
            None,
            &base_conv(),
            &base_advisor_attrs(),
            &[],
            "2026-08-18T00:00:00Z",
        );
        let map: HashMap<String, String> = attrs.into_iter().collect();
        assert_eq!(
            map.get("created_at").map(String::as_str),
            Some("2026-08-18T00:00:00Z")
        );
    }

    #[test]
    fn merge_support_case_attrs_preserves_existing_created_at_without_overwriting() {
        let mut existing = HashMap::new();
        existing.insert("created_at".to_string(), "2026-01-01T00:00:00Z".to_string());
        let attrs = merge_support_case_attrs(
            &existing,
            "case-1",
            "質問文",
            None,
            &base_conv(),
            &base_advisor_attrs(),
            &[],
            "2026-08-18T00:00:00Z",
        );
        let map: HashMap<String, String> = attrs.into_iter().collect();
        assert_eq!(
            map.get("created_at").map(String::as_str),
            Some("2026-01-01T00:00:00Z"),
            "an existing created_at must never be overwritten by a later turn"
        );
    }

    #[test]
    fn merge_support_case_attrs_sets_end_user_id_when_absent_and_provided() {
        let existing = HashMap::new();
        let attrs = merge_support_case_attrs(
            &existing,
            "case-1",
            "質問文",
            Some("end-user-abc"),
            &base_conv(),
            &base_advisor_attrs(),
            &[],
            "2026-08-18T00:00:00Z",
        );
        let map: HashMap<String, String> = attrs.into_iter().collect();
        assert_eq!(
            map.get("end_user_id").map(String::as_str),
            Some("end-user-abc")
        );
    }

    #[test]
    fn merge_support_case_attrs_does_not_overwrite_existing_end_user_id() {
        let mut existing = HashMap::new();
        existing.insert("end_user_id".to_string(), "original-user".to_string());
        let attrs = merge_support_case_attrs(
            &existing,
            "case-1",
            "質問文",
            Some("different-user"),
            &base_conv(),
            &base_advisor_attrs(),
            &[],
            "2026-08-18T00:00:00Z",
        );
        let map: HashMap<String, String> = attrs.into_iter().collect();
        assert_eq!(
            map.get("end_user_id").map(String::as_str),
            Some("original-user"),
            "end_user_id is set on first turn only (Issue #31 design doc §2 semantics)"
        );
    }

    #[test]
    fn merge_support_case_attrs_writes_conv_state_advisor_attrs_and_conditions() {
        let existing = HashMap::new();
        let mut conv = base_conv();
        conv.awaiting_time_pref = true;
        conv.time_pref_false_count = 1;
        conv.preferred_contact_time = Some("平日13:00〜15:00".to_string());
        conv.time_pref_extraction_error_count = 2;
        let advisor_attrs = AdvisorCaseAttrs {
            lead_offered: true,
            lead_requested: true,
            shown_product_cards: "own_product:adc-v724".to_string(),
            question_streak: 3,
            support_mode: true,
            support_case_id: "case-urtect-abc".to_string(),
            support_last_reply_kind: "support_clarify".to_string(),
        };
        let conditions = vec![
            (ConditionKey::Concern, "intrusion".to_string()),
            (ConditionKey::Housing, "apartment_rented".to_string()),
        ];

        let attrs = merge_support_case_attrs(
            &existing,
            "case-1",
            "質問文",
            None,
            &conv,
            &advisor_attrs,
            &conditions,
            "2026-08-18T00:00:00Z",
        );
        let map: HashMap<String, String> = attrs.into_iter().collect();

        assert_eq!(map.get("clarify_turns").map(String::as_str), Some("1"));
        assert_eq!(
            map.get("awaiting_time_pref").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            map.get("time_pref_false_count").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            map.get("preferred_contact_time").map(String::as_str),
            Some("平日13:00〜15:00")
        );
        assert_eq!(
            map.get("time_pref_extraction_error_count")
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(map.get("lead_offered").map(String::as_str), Some("true"));
        assert_eq!(map.get("lead_requested").map(String::as_str), Some("true"));
        assert_eq!(
            map.get("shown_product_cards").map(String::as_str),
            Some("own_product:adc-v724")
        );
        assert_eq!(map.get("question_streak").map(String::as_str), Some("3"));
        assert_eq!(map.get("support_mode").map(String::as_str), Some("true"));
        assert_eq!(
            map.get("support_case_id").map(String::as_str),
            Some("case-urtect-abc")
        );
        assert_eq!(
            map.get("support_last_reply_kind").map(String::as_str),
            Some("support_clarify")
        );
        assert_eq!(
            map.get("advisor_cond_concern").map(String::as_str),
            Some("intrusion")
        );
        assert_eq!(
            map.get("advisor_cond_housing").map(String::as_str),
            Some("apartment_rented")
        );
        assert!(
            !map.contains_key("advisor_cond_target"),
            "unset condition keys must not be written (decide::accumulated_condition_updates \
             contract)"
        );
    }

    // ---- log_turn_decision (design doc §6 末尾・§11 テスト一覧「内部判断の info ログ」) ----

    fn material_fixture(kind: &str, material_key: &str) -> AdvisorMaterial {
        AdvisorMaterial {
            material_key: material_key.to_string(),
            kind: kind.to_string(),
            title_ja: "タイトル".to_string(),
            body_ja: "本文".to_string(),
            source_url: None,
            category: None,
            product_key: None,
            price_band: None,
            card_description: None,
            card_match_terms: None,
            product_page_url: None,
        }
    }

    #[test]
    fn log_turn_decision_emits_info_with_action_kind_and_material_and_card_keys() {
        // design doc §11: 「応答種別・注入した material_key 一覧・カード選定結果が info で
        // ログに出ること」を固定する。
        let materials = vec![material_fixture("own_product", "own_product:adc-v724")];
        let cards = vec![sample_card()];
        // material_key は `{kind}:{slug}` 形式で kind を既に含むため、そのまま出る
        // (kind を重ねて `own_product:own_product:...` にならない)ことを固定する。
        let expected_material_key = materials[0].material_key.clone();
        let meta = draftgen::DraftMeta {
            featured: vec!["own_product:adc-v724".to_string()],
            closing: draftgen::ClosingKind::Proposal,
            choices: Vec::new(),
        };

        let (_, logs) = crate::test_support::capture_logs(|| {
            log_turn_decision(
                "req-1",
                "case-1",
                "answer",
                &materials,
                Some(&cards),
                Some(&meta),
                2,
            );
        });

        assert!(logs.contains("INFO"), "logs: {logs}");
        assert!(
            logs.contains("homesec advisor turn decision"),
            "logs: {logs}"
        );
        assert!(logs.contains("request_id"), "logs: {logs}");
        assert!(logs.contains("req-1"), "logs: {logs}");
        assert!(logs.contains("case-1"), "logs: {logs}");
        assert!(
            logs.contains("action_kind=answer") || logs.contains("action_kind=\"answer\""),
            "action_kind must be the reply_kind passed in: {logs}"
        );
        assert!(
            logs.contains(&expected_material_key),
            "material_keys must log the material_key as-is (already kind-prefixed): {logs}"
        );
        assert!(
            !logs.contains("own_product:own_product:"),
            "material_keys must not duplicate the kind prefix: {logs}"
        );
        assert!(
            logs.contains(&sample_card().material_key),
            "card_keys must include the selected card's material_key: {logs}"
        );
        // reviewer 一次レビュー Major 4 是正: closing / featured_len / question_streak が
        // 出ることを固定する(カード・チップが出るかどうかを決める入力の可観測性)。
        assert!(
            logs.contains("closing=proposal") || logs.contains("closing=\"proposal\""),
            "closing must reflect the draft meta's ClosingKind::as_str(): {logs}"
        );
        assert!(
            logs.contains("featured_len=1"),
            "featured_len must be the draft meta's featured count: {logs}"
        );
        assert!(
            logs.contains("question_streak=2"),
            "question_streak must be the final value passed in: {logs}"
        );
    }

    #[test]
    fn log_turn_decision_never_includes_customer_facing_material_text() {
        // design doc §11: 「顧客メッセージ本文がログに含まれないこと」。ここでは同じ懸念を
        // 材料側(顧客に見せる文章そのもの)にも広げ、body_ja / title_ja が出ないことを
        // 一意なマーカーで固定する。
        let mut material = material_fixture("statistic", "statistic:musimari");
        material.title_ja = "ログに出てはいけない材料タイトルマーカー".to_string();
        material.body_ja = "ログに出てはいけない材料本文マーカー".to_string();

        let (_, logs) = crate::test_support::capture_logs(|| {
            log_turn_decision("req-1", "case-1", "answer", &[material], None, None, 0);
        });

        assert!(
            !logs.contains("ログに出てはいけない材料本文マーカー"),
            "logs: {logs}"
        );
        assert!(
            !logs.contains("ログに出てはいけない材料タイトルマーカー"),
            "logs: {logs}"
        );
    }

    #[test]
    fn log_turn_decision_handles_no_product_cards_without_panicking() {
        // design doc §11: 「`product_cards` が `None` のとき `card_keys` が空表記になること
        // (panic せず出力されること)」。fallback ターン等(手順9で select_cards 自体を
        // 呼ばない経路)が該当する。
        let (_, logs) = crate::test_support::capture_logs(|| {
            log_turn_decision("req-1", "case-1", "fallback", &[], None, None, 0);
        });

        assert!(logs.contains("INFO"), "logs: {logs}");
        assert!(
            logs.contains("card_keys=[]"),
            "card_keys must render as an empty list when product_cards is None: {logs}"
        );
        assert!(
            logs.contains("material_keys=[]"),
            "material_keys must render as an empty list when materials_used is empty: {logs}"
        );
        // reviewer 一次レビュー Major 4 是正: メタが無いターン(fallback 等)は closing が
        // "none" になる境界を固定する。
        assert!(
            logs.contains("closing=none") || logs.contains("closing=\"none\""),
            "closing must be \"none\" when draft_meta is None: {logs}"
        );
        assert!(
            logs.contains("featured_len=0"),
            "featured_len must be 0 when draft_meta is None: {logs}"
        );
    }

    // ---- AdvisorReplyResponse serialization (design doc §3.3) ----

    fn sample_card() -> ProductCard {
        ProductCard {
            material_key: "own_product:adc-v724".to_string(),
            title: "URTECT ADC-V724".to_string(),
            description: "屋外対応・夜間撮影".to_string(),
            image_url: Some("https://advisor.example.com/static/products/adc-v724.jpg".to_string()),
            buttons: vec![
                cards::CardButton::Uri {
                    label: "商品ページを見る".to_string(),
                    url: "https://example.com/products/adc-v724".to_string(),
                },
                cards::CardButton::Message {
                    label: "詳しく聞く".to_string(),
                    message: "ADC-V724について詳しく教えて".to_string(),
                },
            ],
        }
    }

    #[test]
    fn advisor_reply_response_serializes_product_cards_when_present() {
        let response = AdvisorReplyResponse {
            reply_text: "ご提案です。".to_string(),
            case_id: "case-1".to_string(),
            product_cards: Some(vec![sample_card()]),
            quick_replies: None,
        };
        let json = serde_json::to_value(&response).expect("must serialize");
        let cards = json
            .get("product_cards")
            .expect("product_cards key must be present when Some")
            .as_array()
            .expect("product_cards must serialize as a JSON array");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["material_key"], "own_product:adc-v724");
        assert_eq!(cards[0]["title"], "URTECT ADC-V724");
        let buttons = cards[0]["buttons"]
            .as_array()
            .expect("buttons must serialize as a JSON array");
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0]["kind"], "uri");
        assert_eq!(buttons[0]["label"], "商品ページを見る");
        assert_eq!(buttons[0]["url"], "https://example.com/products/adc-v724");
        assert_eq!(buttons[1]["kind"], "message");
        assert_eq!(buttons[1]["label"], "詳しく聞く");
        assert_eq!(buttons[1]["message"], "ADC-V724について詳しく教えて");
    }

    #[test]
    fn advisor_reply_response_omits_product_cards_key_when_none() {
        let response = AdvisorReplyResponse {
            reply_text: "ご質問をもう少し教えてください。".to_string(),
            case_id: "case-1".to_string(),
            product_cards: None,
            quick_replies: None,
        };
        let json = serde_json::to_value(&response).expect("must serialize");
        assert!(
            json.get("product_cards").is_none(),
            "product_cards key itself must be absent from the JSON object when None \
             (design doc §3.3: CS 側は常に省略), got: {json}"
        );
        assert_eq!(json["reply_text"], "ご質問をもう少し教えてください。");
        assert_eq!(json["case_id"], "case-1");
    }

    #[test]
    fn advisor_reply_response_serializes_quick_replies_when_present() {
        let response = AdvisorReplyResponse {
            reply_text: "どのようなことがご不安ですか?".to_string(),
            case_id: "case-1".to_string(),
            product_cards: None,
            quick_replies: Some(vec![QuickReplyItem {
                label: "侵入・空き巣が心配".to_string(),
                message: "侵入や空き巣が心配です".to_string(),
            }]),
        };
        let json = serde_json::to_value(&response).expect("must serialize");
        let items = json
            .get("quick_replies")
            .expect("quick_replies key must be present when Some")
            .as_array()
            .expect("quick_replies must serialize as a JSON array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["label"], "侵入・空き巣が心配");
        assert_eq!(items[0]["message"], "侵入や空き巣が心配です");
    }

    #[test]
    fn advisor_reply_response_omits_quick_replies_key_when_none() {
        let response = AdvisorReplyResponse {
            reply_text: "ご提案です。".to_string(),
            case_id: "case-1".to_string(),
            product_cards: None,
            quick_replies: None,
        };
        let json = serde_json::to_value(&response).expect("must serialize");
        assert!(
            json.get("quick_replies").is_none(),
            "quick_replies key itself must be absent from the JSON object when None \
             (CS 側は常に省略), got: {json}"
        );
    }

    // ---- handler routing (認可・入力検証・ルーティング。LLM/vegapunk 呼び出しには到達しない) ----

    fn test_llm_client() -> AnthropicClient {
        let dir =
            std::env::temp_dir().join(format!("advisor-api-test-llm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let key_path = dir.join("llm-api-key");
        std::fs::write(&key_path, "test-key\n").expect("write api key file");
        AnthropicClient::from_config(&LlmConfig {
            enabled: true,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            ..Default::default()
        })
        .expect("llm client must build from the stub config")
        .expect("enabled = true with a readable key file must yield a client")
    }

    /// `crate::api` の `test_harness_with_knowledge` と同じ流儀の最小 `Harness`。
    /// advisor のハンドラは `product_gate` / `manual` / `corpus` / `reply_drafter` を
    /// 一切使わないため、すべて `None` のままでよい。
    fn advisor_test_harness(
        knowledge: Option<crate::harness::knowledge::KnowledgeStore>,
    ) -> Harness {
        let dir =
            std::env::temp_dir().join(format!("advisor-api-test-harness-{}", uuid::Uuid::new_v4()));
        let lexicon = Arc::new(
            crate::harness::signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap(),
        );
        Harness {
            authenticator: crate::harness::authn::Authenticator::new(vec!["homesec".to_string()]),
            normalizer: lexicon.clone(),
            extractor: Arc::new(crate::harness::extraction::HybridExtractor::new(
                lexicon.clone(),
                None,
            )),
            lexicon,
            ng: NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":[]}"#).unwrap(),
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
            product_gate: None,
        }
    }

    fn advisor_test_state(
        api_key: &str,
        knowledge: Option<crate::harness::knowledge::KnowledgeStore>,
    ) -> AdvisorApiState {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://vegapunk.invalid:6840"
[[projects]]
project_id = "homesec"
schema = "homesec"
manual_schema = "manual_v1"
[api]
enabled = true
clarify_max_turns = 3
"#;
        let config: AppConfig = toml::from_str(toml).expect("valid test config");
        let vegapunk = crate::vegapunk::VegapunkClient::connect_lazy_with_limits(
            &config.vegapunk_endpoint,
            "",
            crate::vegapunk::GrpcLimits::default(),
        )
        .expect("lazy connect never touches the network");
        let images_dir =
            std::env::temp_dir().join(format!("advisor-api-test-images-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&images_dir).expect("create temp images dir");
        // Issue #50 バッチ1: support_harness / support_tools は `vegapunk.clone()` を先に
        // 済ませておく(下の struct literal で `vegapunk,` フィールドが値を move するため、
        // それより後ろで clone しようとすると borrow checker に弾かれる)。
        let support_tools = crate::mcp::ToolService::new(vegapunk.clone());
        AdvisorApiState {
            config: Arc::new(config),
            harness: Arc::new(advisor_test_harness(knowledge)),
            llm: test_llm_client(),
            ng: Arc::new(
                NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":[]}"#).unwrap(),
            ),
            vegapunk,
            api_key: api_key.to_string(),
            images_dir,
            public_host: "advisor.example.com".to_string(),
            support_harness: Arc::new(advisor_test_harness(None)),
            support_tools,
            support_schema: "urtect".to_string(),
            support_manual_schema: crate::config::ManualSchemaKind::ManualV1,
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
        let router = advisor_api_router(advisor_test_state("correct-key", None));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/homesec/api/reply",
            None,
            r#"{"message":"こんにちは"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
    }

    #[tokio::test]
    async fn reply_route_rejects_mismatched_api_key_with_401() {
        let router = advisor_api_router(advisor_test_state("correct-key", None));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/homesec/api/reply",
            Some("Bearer wrong-key"),
            r#"{"message":"こんにちは"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
    }

    #[tokio::test]
    async fn reply_route_rejects_unknown_project_with_404_before_validating_body() {
        let router = advisor_api_router(advisor_test_state("correct-key", None));
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
    async fn reply_route_rejects_empty_message_with_400() {
        let router = advisor_api_router(advisor_test_state("correct-key", None));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/homesec/api/reply",
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

    #[tokio::test]
    async fn reply_route_rejects_malformed_json_with_400() {
        let router = advisor_api_router(advisor_test_state("correct-key", None));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/homesec/api/reply",
            Some("Bearer correct-key"),
            "{ not json",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_request");
    }

    /// `knowledge: None` の `Harness` に対して、認可・project・validate をすべて通過した
    /// リクエストは `state.harness.store()`（手順2）で必ず `Err` になり 500 を返す
    /// （`crate::api` の同種テストと同じ「実 vegapunk なしで到達できる 500 経路」）。
    /// LLM Call #1 に到達する前に短絡するため、ネットワーク無しで検証できる。
    #[tokio::test]
    async fn reply_route_returns_500_without_leaking_internal_error_when_knowledge_unavailable() {
        let router = advisor_api_router(advisor_test_state("correct-key", None));
        let (status, body) = oneshot_json(
            router,
            "POST",
            "/homesec/api/reply",
            Some("Bearer correct-key"),
            r#"{"message":"玄関の防犯が心配です"}"#,
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

    /// `advisor_service_principal` / `advisor_test_harness` の schema と
    /// `advisor_test_state` の project schema が一致していること(`Harness::begin` が
    /// `scope::resolve_scope` で弾かないこと)の前提を、`ManualSchemaKind` の parse も含めて
    /// 固定する回帰テスト。
    #[tokio::test]
    async fn advisor_test_state_project_uses_manual_v1_schema() {
        let state = advisor_test_state("correct-key", None);
        assert_eq!(state.config.projects[0].schema, "homesec");
        assert!(matches!(
            state.config.projects[0].manual_schema,
            ManualSchemaKind::ManualV1
        ));
    }

    // ---- Issue #50 バッチ2(design doc §13.2〜13.4、CS サポートモード) ----

    // ---- support_reply_kind_to_advisor ----

    #[test]
    fn support_reply_kind_to_advisor_covers_all_five_variants() {
        use cs_support::SupportReplyKind;
        assert_eq!(
            support_reply_kind_to_advisor(SupportReplyKind::Answer),
            "support_answer"
        );
        assert_eq!(
            support_reply_kind_to_advisor(SupportReplyKind::Clarify),
            "support_clarify"
        );
        assert_eq!(
            support_reply_kind_to_advisor(SupportReplyKind::Escalation),
            "support_escalation"
        );
        assert_eq!(
            support_reply_kind_to_advisor(SupportReplyKind::TimePref),
            "support_time_pref"
        );
        assert_eq!(
            support_reply_kind_to_advisor(SupportReplyKind::OutOfScope),
            "support_out_of_scope"
        );
    }

    // ---- support_mode_reply_parts ----

    #[test]
    fn support_mode_reply_parts_suppresses_materials_quick_replies_and_meta() {
        // design doc §13.2 手順4: サポートモードのターンは製品カード・チップ・リード提案を
        // 一切出さない。
        let outcome = cs_support::SupportReplyOutcome {
            reply_text: "回答本文です。".to_string(),
            case_id: "case-urtect-1".to_string(),
            reply_kind: cs_support::SupportReplyKind::Answer,
            audit_event_id: "audit-1".to_string(),
        };

        let (text, kind, materials, quick_replies, meta) = support_mode_reply_parts(outcome);

        assert_eq!(text, "回答本文です。");
        assert_eq!(kind, "support_answer");
        assert!(
            materials.is_empty(),
            "SupportMode must not attach materials"
        );
        assert!(
            quick_replies.is_none(),
            "SupportMode must not attach quick_replies (no chips/lead proposal)"
        );
        assert!(
            meta.is_none(),
            "SupportMode must not leak a DraftMeta (a Some(meta) would let select_cards attach \
             product cards)"
        );
    }

    // ---- convert_history_for_support ----

    #[test]
    fn convert_history_for_support_maps_roles_and_preserves_order() {
        let history = vec![
            HistoryEntry {
                role: HistoryRole::Customer,
                text: "録画が見られません".to_string(),
            },
            HistoryEntry {
                role: HistoryRole::Assistant,
                text: "型番を教えてください".to_string(),
            },
        ];

        let converted = convert_history_for_support(&history);

        assert_eq!(
            converted,
            vec![
                ReplyHistoryTurn {
                    role: ReplyHistoryRole::Customer,
                    text: "録画が見られません".to_string(),
                },
                ReplyHistoryTurn {
                    role: ReplyHistoryRole::Assistant,
                    text: "型番を教えてください".to_string(),
                },
            ]
        );
    }

    // ---- run_cs_support_mode_turn(呼び出し構築: schema 配線・case_id の Option 変換・
    // outcome.case_id の書き戻し) ----
    //
    // 実 vegapunk 接続は使わない。`knowledge: None` の support_harness に対して質問側ゲート
    // (`ProductAllowlist::first_out_of_scope_token`、evaluate() より前の LLM 不使用ゲート)を
    // 発火させる型番言及の発話を送ると、`cs_support::run_support_turn` は evaluate() を呼ばず
    // `Ok` を返す(`cs_support.rs` 自身の
    // `run_support_turn_returns_a_canned_reply_for_an_out_of_scope_model_without_calling_evaluate`
    // と同じ手法)。`record_out_of_scope_case` は knowledge 不在時、渡された `case_id` を
    // (`Some` ならそのまま、`None` なら新規 UUID で)フォールバック採用して `Ok` を返すため、
    // 戻ってきた `case_id` を見れば呼び出し側が `Some`/`None` のどちらを渡したかを判別できる。

    /// `support_schema` を差し替えた `Harness` を持つ `AdvisorApiState` を組み立てる。
    /// `advisor_test_harness` / `test_harness`(`cs_support.rs`)と同じ最小構成だが、
    /// authenticator と product_gate を呼び出し元が指定する schema 専用にする(schema の
    /// 取り違えがあれば `harness.begin` が resolve_scope で弾いて `Err` になるようにするため)。
    fn support_mode_test_state(
        support_schema: &str,
        allowlist_models: Vec<&str>,
    ) -> AdvisorApiState {
        let mut state = advisor_test_state("correct-key", None);
        let dir = std::env::temp_dir().join(format!(
            "advisor-api-support-harness-{}",
            uuid::Uuid::new_v4()
        ));
        let lexicon = Arc::new(
            crate::harness::signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap(),
        );
        let support_harness = Harness {
            authenticator: crate::harness::authn::Authenticator::new(vec![
                support_schema.to_string()
            ]),
            normalizer: lexicon.clone(),
            extractor: Arc::new(crate::harness::extraction::HybridExtractor::new(
                lexicon.clone(),
                None,
            )),
            lexicon,
            ng: NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":[]}"#).unwrap(),
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
            product_gate: Some(crate::harness::product_gate::ProductGate::seeded_for_test(
                support_schema,
                crate::harness::product_gate::ProductAllowlist::from_models(
                    allowlist_models.into_iter().map(str::to_string).collect(),
                ),
            )),
        };
        state.support_harness = Arc::new(support_harness);
        state.support_schema = support_schema.to_string();
        state
    }

    #[tokio::test]
    async fn run_cs_support_mode_turn_routes_to_the_configured_support_schema() {
        // support_harness の authenticator/product_gate は "urtect" にしか登録していないので、
        // run_support_turn が正しく state.support_schema("urtect")を渡していなければ
        // harness.begin の resolve_scope で Err になる。
        let state = support_mode_test_state("urtect", vec!["ADC-V724"]);
        let identity = advisor_service_principal();
        let mut attrs = base_advisor_attrs();

        let outcome = run_cs_support_mode_turn(
            &state,
            &identity,
            "ADC-V999は使えますか",
            &[],
            None,
            &mut attrs,
        )
        .await
        .expect("the question-side gate must short-circuit before evaluate() is called");

        assert_eq!(outcome.reply_kind, cs_support::SupportReplyKind::OutOfScope);
    }

    #[tokio::test]
    async fn run_cs_support_mode_turn_passes_none_when_support_case_id_is_empty() {
        let state = support_mode_test_state("urtect", vec!["ADC-V724"]);
        let identity = advisor_service_principal();
        let mut attrs = base_advisor_attrs();
        assert_eq!(attrs.support_case_id, "", "precondition: no CS case yet");

        let outcome = run_cs_support_mode_turn(
            &state,
            &identity,
            "ADC-V999は使えますか",
            &[],
            None,
            &mut attrs,
        )
        .await
        .expect("question-side gate short-circuit");

        assert!(
            outcome.case_id.starts_with("case-"),
            "an empty support_case_id must be passed through as None, which \
             record_out_of_scope_case falls back to a freshly generated \"case-<uuid>\" id \
             (a wrongly-passed Some(\"\") would instead yield an empty case_id): {}",
            outcome.case_id
        );
        assert_eq!(
            attrs.support_case_id, outcome.case_id,
            "the outcome's case_id must be written back to advisor_attrs.support_case_id"
        );
    }

    #[tokio::test]
    async fn run_cs_support_mode_turn_passes_some_when_support_case_id_is_present() {
        let state = support_mode_test_state("urtect", vec!["ADC-V724"]);
        let identity = advisor_service_principal();
        let mut attrs = base_advisor_attrs();
        attrs.support_case_id = "case-existing-abc".to_string();

        let outcome = run_cs_support_mode_turn(
            &state,
            &identity,
            "ADC-V999は使えますか",
            &[],
            None,
            &mut attrs,
        )
        .await
        .expect("question-side gate short-circuit");

        assert_eq!(
            outcome.case_id, "case-existing-abc",
            "a non-empty support_case_id must be passed through verbatim as Some(&str); \
             record_out_of_scope_case echoes it back unchanged when knowledge is unavailable"
        );
        assert_eq!(
            attrs.support_case_id, "case-existing-abc",
            "the outcome's case_id must be written back to advisor_attrs.support_case_id \
             (idempotent: same value)"
        );
    }

    #[tokio::test]
    async fn run_cs_support_mode_turn_writes_back_support_last_reply_kind_on_success() {
        // Issue #50 バッチ2 レビュー修正 Critical 1: 成功時に
        // `support_reply_kind_to_advisor(outcome.reply_kind)` を `support_last_reply_kind` へ
        // 書き戻すこと。この harness は質問側ゲートで OutOfScope 短絡するので、期待値は
        // "support_out_of_scope"。
        let state = support_mode_test_state("urtect", vec!["ADC-V724"]);
        let identity = advisor_service_principal();
        let mut attrs = base_advisor_attrs();

        let outcome = run_cs_support_mode_turn(
            &state,
            &identity,
            "ADC-V999は使えますか",
            &[],
            None,
            &mut attrs,
        )
        .await
        .expect("question-side gate short-circuit");

        assert_eq!(outcome.reply_kind, cs_support::SupportReplyKind::OutOfScope);
        assert_eq!(attrs.support_last_reply_kind, "support_out_of_scope");
    }
}
