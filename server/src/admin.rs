//! 管理 API（`GET/POST /{project_id}/admin/api/*`）。
//!
//! 仕様の正本は `docs/superpowers/specs/2026-08-16-admin-dashboard-design.md`（以下
//! 「design doc」）§4。会話閲覧（スレッド一覧・詳細・ユーザー別一覧）・訂正登録（ノウハウ化）・
//! 利用状況サマリを提供する。フロント（`admin-ui/`）はスコープ外（design doc §5）。
//!
//! 認証は `/{project_id}/mcp` と同じ `oauth::middleware::require_google_auth`（main.rs 側で
//! layer する）。ここでは Google 認証済みであることを前提にハンドラを書く。現状の暫定 AuthZ
//! （認証通過者は全員 supervisor）を継承する（design doc §4）。
//!
//! エラー形式は `/api/reply` と同一（`crate::api::error_response` / `ErrorBody` を再利用する。
//! ロジックを2箇所に複製しない）。

use crate::api::error_response;
use crate::config::ManualSchemaKind;
use crate::harness::{escalation_reply, prompt_input, Harness, RegisterKnownResolutionError};
use crate::oauth::VerifiedIdentity;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// `/{project_id}/admin/api` の router が共有する状態。project 単位に main.rs が組み立てる
/// （`/{project_id}/mcp` が project ごとに `CsSupportRmcpServer` を構築するのと同じ構図）。
#[derive(Clone)]
pub struct AdminState {
    pub schema: String,
    pub manual_schema: ManualSchemaKind,
    pub harness: Arc<Harness>,
}

/// スレッド一覧・ユーザー別一覧の既定 limit（design doc §4）。
const THREADS_DEFAULT_LIMIT: usize = 20;
/// 1 回の内部フェッチで取得する `ConversationTurn` の件数。distinct case 数がまだ `limit` に
/// 満たない場合、このページ幅で追加取得を繰り返す。
const THREADS_FETCH_PAGE_SIZE: i32 = 200;
/// 内部フェッチの繰り返し上限（design doc §4: 「多くとも数回程度のリトライで十分」）。
/// 過度なループガードとして 10 回で打ち切り、それでも `limit` に届かなければ届いた分だけ返す。
const MAX_INTERNAL_FETCH_ROUNDS: usize = 10;
/// 利用状況サマリの既定期間（日数）。
const STATS_DEFAULT_DAYS: u32 = 7;
/// `?days=` の受理上限（日数）。10 年分（365 * 10 = 3650 日）あれば運用上の利用状況分析に
/// 十分で、それ以上を要求する業務要件は design doc に無い。上限を設ける本質的な理由は
/// `chrono::DateTime - chrono::Duration` が表現可能範囲（西暦 262000 年ごろ）を超えると
/// `expect` で panic することで、`?days=` はクライアント入力なので好きな値を送れる
/// （`stats_summary` がこの定数でチェックしてから減算する）。
const STATS_MAX_DAYS: u32 = 3650;

pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/threads", get(list_threads))
        .route("/threads/{case_id}", get(get_thread))
        .route("/users/{end_user_id}/threads", get(list_user_threads))
        .route("/corrections", post(create_correction))
        .route("/stats/summary", get(stats_summary))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// ConversationTurn の内部表現と純関数（テスト容易性のため network I/O から分離）
// ---------------------------------------------------------------------------

/// `ConversationTurn` ノードの属性 map から取り出した最小ビュー。スレッド一覧の集約・
/// スレッド詳細・利用状況サマリの 3 用途で共有する。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnRow {
    case_id: String,
    turn_id: String,
    seq: u32,
    created_at: String,
    question: String,
    reply_text: String,
    reply_kind: String,
    audit_event_id: String,
    end_user_id: Option<String>,
}

impl TurnRow {
    /// `attrs.get("case_id")` / `attrs.get("turn_id")` / `attrs.get("created_at")` のいずれかが
    /// 欠けている行は不完全な ConversationTurn とみなして `None`（スキップ）。他の属性は
    /// 欠落を既定値として許容する（旧行・書き込み途中の行を握りつぶさずに読み進めるため）。
    fn from_attrs(attrs: &HashMap<String, String>) -> Option<Self> {
        Some(Self {
            case_id: attrs.get("case_id")?.clone(),
            turn_id: attrs.get("turn_id")?.clone(),
            seq: attrs.get("seq").and_then(|s| s.parse().ok()).unwrap_or(0),
            created_at: attrs.get("created_at")?.clone(),
            question: attrs.get("question").cloned().unwrap_or_default(),
            reply_text: attrs.get("reply_text").cloned().unwrap_or_default(),
            reply_kind: attrs.get("reply_kind").cloned().unwrap_or_default(),
            audit_event_id: attrs.get("audit_event_id").cloned().unwrap_or_default(),
            end_user_id: attrs.get("end_user_id").filter(|s| !s.is_empty()).cloned(),
        })
    }
}

/// case 単位に集約した 1 行（スレッド一覧・ユーザー別一覧の共通レスポンス形式、design doc §4）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadSummary {
    pub case_id: String,
    pub case_ref: String,
    pub end_user_id: Option<String>,
    /// 先頭ターン（`seq == 1`）の質問文を先頭 80 字に丸めたもの。
    pub question_preview: String,
    pub turn_count: u32,
    pub last_reply_kind: String,
    pub last_created_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThreadsPage {
    pub threads: Vec<ThreadSummary>,
    pub next_cursor: Option<String>,
}

/// `rows` の中で各 case_id が最初に現れる raw index（`rows` の並び順そのままの位置）。`rows`
/// は raw `ConversationTurn` の取得順（vegapunk 側は `created_at` desc）であることが前提。
///
/// `aggregate_threads` の降順ソートの安定なタイブレークと、`paginate_summaries` のページング
/// cutoff の算出の両方が、この「最初に現れた位置」を共有の基準として使う（Issue #31 reviewer
/// 指摘5: `created_at` が完全一致する境界で、タイブレークが raw の取得順と無関係に決まると、
/// 「返したスレッドの集合」と「実際に消費した raw 行の範囲」がずれ、取りこぼしや重複が起きる）。
fn first_occurrence_index(rows: &[TurnRow]) -> HashMap<String, usize> {
    let mut order = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        order.entry(row.case_id.clone()).or_insert(i);
    }
    order
}

/// `TurnRow` の集合を case_id で集約し、`ThreadSummary` の一覧（最新時刻降順）にする純関数。
///
/// - `turn_count`: 当該 case のターン数。
/// - `question_preview`: `seq == 1` のターンを先頭質問とする（無ければ集合中の最初の要素で
///   代替する。旧データや欠番があっても丸ごと落とさないため）。
/// - `end_user_id`: いずれかのターンから取得できれば良い（design doc §4）。
/// - `last_reply_kind` / `last_created_at`: `created_at` が最大のターンを最新ターンとみなす。
/// - `created_at` が完全一致する複数 case のタイブレークは `rows` 内の raw 出現順（早い方が
///   上位）で決める。`HashMap` の走査順に委ねると、`paginate_summaries` が計算する raw offset
///   の cutoff と食い違い、取りこぼし・重複の原因になる（Issue #31 reviewer 指摘5）。
fn aggregate_threads(rows: Vec<TurnRow>) -> Vec<ThreadSummary> {
    let order = first_occurrence_index(&rows);
    let mut by_case: HashMap<String, Vec<TurnRow>> = HashMap::new();
    for row in rows {
        by_case.entry(row.case_id.clone()).or_default().push(row);
    }
    let mut out: Vec<ThreadSummary> = by_case
        .into_values()
        .map(|turns| {
            // `turns` はグループ化の元になった非空 Vec なので必ず 1 件以上ある。
            let latest = turns
                .iter()
                .max_by(|a, b| a.created_at.cmp(&b.created_at))
                .expect("group is non-empty by construction (grouped by its own case_id)");
            let first_question = turns
                .iter()
                .find(|t| t.seq == 1)
                .or_else(|| turns.first())
                .map(|t| t.question.as_str())
                .unwrap_or("");
            let end_user_id = turns.iter().find_map(|t| t.end_user_id.clone());
            ThreadSummary {
                case_id: turns[0].case_id.clone(),
                case_ref: escalation_reply::case_ref(&turns[0].case_id),
                end_user_id,
                question_preview: prompt_input::truncate_chars(first_question, 80),
                turn_count: turns.len() as u32,
                last_reply_kind: latest.reply_kind.clone(),
                last_created_at: latest.created_at.clone(),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.last_created_at
            .cmp(&a.last_created_at)
            .then_with(|| order[&a.case_id].cmp(&order[&b.case_id]))
    });
    out
}

/// ページング境界条件を決める純関数。「集約後の distinct case 数が `limit` に届いた」または
/// 「取得元がもう尽きた（返却件数が内部フェッチの page_size 未満）」のいずれかで打ち切る。
/// それ以外は、今回の内部フェッチで新たに消費した分だけ前進させた次の raw offset
/// （`next_offset_if_continuing`、呼び出し元が `offset + returned_count` として計算済み）を返す。
fn next_internal_offset(
    returned_count: usize,
    fetch_page_size: usize,
    distinct_case_count: usize,
    limit: usize,
    next_offset_if_continuing: i32,
) -> Option<i32> {
    if distinct_case_count >= limit {
        return None;
    }
    if returned_count < fetch_page_size {
        return None;
    }
    Some(next_offset_if_continuing)
}

/// 集約済み `ThreadSummary`（`last_created_at` 降順ソート済み前提）を `limit` 件に切り詰め、
/// 続きがあれば `next_cursor`（offset ベースの内部カーソルを符号化したもの）を発行する。
///
/// `source_exhausted` は `build_threads_page` が内部フェッチのループを「取得元が尽きたため
/// （最後の内部フェッチの返却件数が `THREADS_FETCH_PAGE_SIZE` 未満）」に抜けたかどうかを表す。
/// cursor の発行条件は次の3分岐（Issue #31 reviewer 指摘: distinct case 数が `limit` に
/// ちょうど一致した場合や、内部フェッチのラウンド上限で打ち切った場合にも `next_cursor = None`
/// を返していたため、まだ残っている後続データが管理画面から永久に見えなくなる欠陥があった）:
///
/// 1. `summaries.len() > limit`（切り詰めた）: カーソルの値は「返した各スレッドが raw
///    `ConversationTurn` として `rows` の中で最初に現れた位置」のうち最大のものの直後
///    （`initial_offset` を基準にした絶対 offset）。返したスレッドの代表行（最新ターン）を
///    必ず cutoff より前に置くことで、次ページの取得がそれを再び含まない。cutoff を「返した
///    スレッドの代表行」までに留め、それらの他の（より古い）ターンの位置までは伸ばさないのは、
///    伸ばすと discard したスレッド（`limit` 超過分）の raw 行が cutoff より前に置き去りに
///    なり、次ページで永久に失われうるため（安全側 = 欠落より重複を許容する）。1 スレッドが
///    複数ターンを持ち、かつその代表行以外のターンがページ境界をまたぐ場合に限り、そのスレッドが
///    次ページで再度現れることがある（design doc §4 の許容範囲。`rows`（= `fetch_thread_page`
///    が `initial_offset` から連番で取得した raw 行）が今後拡張され、`turn_id` までの厳密な
///    タイブレークを持てば、この残存条件も解消できる）。
/// 2. 切り詰めていない（`summaries.len() <= limit`）かつ `!source_exhausted`: distinct case 数が
///    `limit` に届いた、またはラウンド上限に達したために打ち切られたが、取得元にはまだ後続
///    データが残っている可能性がある。この窓（`rows`）の中の distinct case は全件 `summaries`
///    へ反映済みなので、cutoff を「今回消費した raw 行の直後」（`initial_offset + rows.len()`）
///    まで伸ばしても取りこぼしは起きない（この窓の外側に未集計の行は存在しないため）。
/// 3. 切り詰めていない かつ `source_exhausted`: 取得元が尽きており、これ以上のデータが無い
///    ことが確定しているので `None`（真の終端）。
fn paginate_summaries(
    mut summaries: Vec<ThreadSummary>,
    limit: usize,
    rows: &[TurnRow],
    initial_offset: i32,
    source_exhausted: bool,
) -> (Vec<ThreadSummary>, Option<String>) {
    if limit == 0 {
        return (summaries, None);
    }
    if summaries.len() > limit {
        summaries.truncate(limit);
        let order = first_occurrence_index(rows);
        let cutoff = summaries.iter().filter_map(|s| order.get(&s.case_id)).max();
        let next_cursor = cutoff.map(|&i| encode_cursor(initial_offset + i as i32 + 1));
        return (summaries, next_cursor);
    }
    if source_exhausted {
        return (summaries, None);
    }
    (
        summaries,
        Some(encode_cursor(initial_offset + rows.len() as i32)),
    )
}

/// 内部カーソル（= このスレッド一覧クエリで既に消費した生 `ConversationTurn` 件数、offset）を
/// 不透明トークンへ符号化する。design doc §4 は当初 `(created_at, turn_id)` の複合カーソルを
/// 想定していたが、vegapunk の `AttributeFilter` に複合キー比較や OR 条件があるかどうかを
/// 確認できていないため、この codebase が既に信頼している offset ページング
/// （`vegapunk::query_nodes_paged` と同じ設計）に統一した（Issue #31 reviewer 指摘5）。値ベース
/// の `created_at < cursor` フィルタは、`created_at` が完全一致する境界でエントリを取りこぼす・
/// 重複させる欠陥があった。
fn encode_cursor(offset: i32) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(offset.to_string().as_bytes())
}

/// cursor を decode し、内部フィルタに使う raw offset を取り出す。壊れた/空/負の cursor は
/// `None`（「cursor 無し」として扱い、先頭（offset 0）から返す。クライアントの不正な cursor で
/// 500 にしない）。
fn decode_cursor(cursor: &str) -> Option<i32> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    text.parse::<i32>().ok().filter(|&offset| offset >= 0)
}

/// 利用状況サマリ（design doc §4）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StatsSummary {
    pub days: u32,
    pub turn_count: u32,
    pub thread_count: u32,
    pub reply_kind_counts: BTreeMap<String, u32>,
    pub unique_end_user_count: u32,
}

/// `TurnRow` の集合から [`StatsSummary`] を組み立てる純関数。
fn summarize_turns(days: u32, rows: &[TurnRow]) -> StatsSummary {
    let mut reply_kind_counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut case_ids: HashSet<&str> = HashSet::new();
    let mut end_user_ids: HashSet<&str> = HashSet::new();
    for row in rows {
        *reply_kind_counts.entry(row.reply_kind.clone()).or_insert(0) += 1;
        case_ids.insert(row.case_id.as_str());
        if let Some(id) = &row.end_user_id {
            end_user_ids.insert(id.as_str());
        }
    }
    StatsSummary {
        days,
        turn_count: rows.len() as u32,
        thread_count: case_ids.len() as u32,
        reply_kind_counts,
        unique_end_user_count: end_user_ids.len() as u32,
    }
}

// ---------------------------------------------------------------------------
// ハンドラ
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ThreadsQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

/// 1 回の raw ページ取得を表す型。`Send` を要求するのは axum のハンドラが複数ワーカー
/// スレッド上で実行されうるため（`fetch_page` は `.await` をまたいで別スレッドへ移りうる）。
type PageFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<TurnRow>>> + Send + 'a>>;

/// スレッド一覧・ユーザー別一覧の共通ページング処理（テスト容易性のため raw ページ取得を
/// `fetch_page` として注入する）。`limit`（distinct case 数）に届く、または取得元が尽きるまで
/// `initial_offset` から連番で内部フェッチを繰り返し、集約・ページング境界の決定を行う。
/// `fetch_thread_page` は実 vegapunk 呼び出し（`KnowledgeStore::load_conversation_turns_page`）
/// を渡す薄いラッパー。テストはネットワーク無しの決定論的フェイクを渡せる。
///
/// ループを抜けた理由（`source_exhausted`: 最後の内部フェッチの返却件数が
/// `THREADS_FETCH_PAGE_SIZE` 未満だったか）を `paginate_summaries` へ引き渡し、cursor の発行
/// 条件をそれに基づかせる。「distinct case 数が `limit` にちょうど一致した」「ラウンド上限
/// （`MAX_INTERNAL_FETCH_ROUNDS`）を使い切った」のいずれも取得元はまだ尽きていないため、
/// `source_exhausted = false` のまま cursor を発行し、後続データを黙って落とさない（Issue #31
/// reviewer 指摘: 修正前はこの2ケースで `next_cursor = None` を返し、境界以降のスレッドが
/// 管理画面から永久に見えなくなっていた）。ラウンド上限を使い切ってなお取得元が尽きていない
/// 場合は、運用者が調査できるよう `tracing::warn!` を1行出す。
async fn build_threads_page<'a>(
    limit: usize,
    initial_offset: i32,
    mut fetch_page: impl FnMut(i32, i32) -> PageFuture<'a>,
) -> anyhow::Result<ThreadsPage> {
    let mut collected: Vec<TurnRow> = Vec::new();
    let mut offset = initial_offset;
    let mut source_exhausted = false;
    for _ in 0..MAX_INTERNAL_FETCH_ROUNDS {
        let rows = fetch_page(offset, THREADS_FETCH_PAGE_SIZE).await?;
        let returned_count = rows.len();
        source_exhausted = returned_count < THREADS_FETCH_PAGE_SIZE as usize;
        collected.extend(rows);
        let distinct = collected
            .iter()
            .map(|r| r.case_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        let next_offset_if_continuing = offset + returned_count as i32;
        match next_internal_offset(
            returned_count,
            THREADS_FETCH_PAGE_SIZE as usize,
            distinct,
            limit,
            next_offset_if_continuing,
        ) {
            Some(next) => offset = next,
            None => break,
        }
    }
    let distinct = collected
        .iter()
        .map(|r| r.case_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    if !source_exhausted && distinct < limit {
        tracing::warn!(
            limit,
            offset,
            distinct_case_count = distinct,
            raw_row_count = collected.len(),
            "admin api: build_threads_page hit MAX_INTERNAL_FETCH_ROUNDS before reaching limit or \
             exhausting the source; emitting a next_cursor so the caller can continue instead of \
             silently dropping the remaining threads"
        );
    }
    let summaries = aggregate_threads(collected.clone());
    let (threads, next_cursor) = paginate_summaries(
        summaries,
        limit,
        &collected,
        initial_offset,
        source_exhausted,
    );
    Ok(ThreadsPage {
        threads,
        next_cursor,
    })
}

/// スレッド一覧・ユーザー別一覧の共通取得ロジック。`base_filter` が `Some` ならその条件
/// （`end_user_id eq` 等）を全内部クエリへ重ねる。`initial_offset` は外部 cursor を decode した
/// raw offset（cursor 無しなら 0）。
async fn fetch_thread_page(
    state: &AdminState,
    base_filter: Option<(&str, &str, &str)>,
    limit: usize,
    initial_offset: i32,
) -> anyhow::Result<ThreadsPage> {
    let store = state.harness.store()?;
    let schema = state.schema.as_str();
    build_threads_page(limit, initial_offset, move |offset, page_size| {
        Box::pin(async move {
            let attrs_list = store
                .load_conversation_turns_page(schema, base_filter, offset, page_size)
                .await?;
            Ok(attrs_list.iter().filter_map(TurnRow::from_attrs).collect())
        })
    })
    .await
}

fn resolved_limit(query_limit: Option<usize>) -> usize {
    query_limit
        .filter(|&l| l > 0)
        .unwrap_or(THREADS_DEFAULT_LIMIT)
}

/// `GET /admin/api/threads?limit=&cursor=`（design doc §4）。
///
/// `harness.begin` を呼ぶのは、認可判定が入りうる唯一の入口をこの一覧経路にも通しておくため
/// （`get_thread` / `create_correction` と同じ理由）。戻り値の `ctx` は一覧取得自体には使わない
/// （schema は `state.schema` のまま）。呼ぶこと自体が目的で、現状（認証通過者は全員
/// supervisor）では常に成功するが、actor 突合表 DB が入った時にこの経路だけが認可を
/// 素通りしないようにする。
async fn list_threads(
    State(state): State<AdminState>,
    Extension(identity): Extension<VerifiedIdentity>,
    Query(query): Query<ThreadsQuery>,
) -> Response {
    if let Err(err) = state
        .harness
        .begin(&identity, &state.schema, state.manual_schema)
    {
        tracing::warn!(
            error = ?err,
            sub = %identity.sub,
            schema = %state.schema,
            "admin api: list_threads scope resolution failed"
        );
        return error_response(StatusCode::FORBIDDEN, "forbidden", err.to_string());
    }
    let limit = resolved_limit(query.limit);
    let offset = query.cursor.as_deref().and_then(decode_cursor).unwrap_or(0);
    match fetch_thread_page(&state, None, limit, offset).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(err) => {
            tracing::error!(error = ?err, schema = %state.schema, "admin api: list_threads failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to load threads; see server logs",
            )
        }
    }
}

/// `GET /admin/api/users/{end_user_id}/threads`（design doc §4）。
///
/// `harness.begin` を呼ぶのは、認可判定が入りうる唯一の入口をこの一覧経路にも通しておくため
/// （`get_thread` / `create_correction` と同じ理由）。戻り値の `ctx` は一覧取得自体には使わない
/// （schema は `state.schema` のまま）。呼ぶこと自体が目的で、現状（認証通過者は全員
/// supervisor）では常に成功するが、actor 突合表 DB が入った時にこの経路だけが認可を
/// 素通りしないようにする。
async fn list_user_threads(
    State(state): State<AdminState>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(end_user_id): Path<String>,
    Query(query): Query<ThreadsQuery>,
) -> Response {
    if let Err(err) = state
        .harness
        .begin(&identity, &state.schema, state.manual_schema)
    {
        tracing::warn!(
            error = ?err,
            sub = %identity.sub,
            schema = %state.schema,
            "admin api: list_user_threads scope resolution failed"
        );
        return error_response(StatusCode::FORBIDDEN, "forbidden", err.to_string());
    }
    let limit = resolved_limit(query.limit);
    let offset = query.cursor.as_deref().and_then(decode_cursor).unwrap_or(0);
    let base_filter = ("end_user_id", "eq", end_user_id.as_str());
    match fetch_thread_page(&state, Some(base_filter), limit, offset).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(err) => {
            tracing::error!(
                error = ?err,
                schema = %state.schema,
                end_user_id,
                "admin api: list_user_threads failed"
            );
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to load the user's threads; see server logs",
            )
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct TurnView {
    turn_id: String,
    seq: u32,
    created_at: String,
    question: String,
    reply_text: String,
    reply_kind: String,
    audit_event_id: String,
    end_user_id: Option<String>,
}

impl From<TurnRow> for TurnView {
    fn from(row: TurnRow) -> Self {
        Self {
            turn_id: row.turn_id,
            seq: row.seq,
            created_at: row.created_at,
            question: row.question,
            reply_text: row.reply_text,
            reply_kind: row.reply_kind,
            audit_event_id: row.audit_event_id,
            end_user_id: row.end_user_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct ThreadDetail {
    case_id: String,
    case_ref: String,
    turns: Vec<TurnView>,
    clarify_turns: u32,
    preferred_contact_time: Option<String>,
    accumulated_signals: Vec<String>,
}

/// `GET /admin/api/threads/{case_id}`（design doc §4）。ターン列は `seq` 昇順、case メタ
/// （累積 signal・`preferred_contact_time`・`clarify_turns`）を併せて返す。
async fn get_thread(
    State(state): State<AdminState>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(case_id): Path<String>,
) -> Response {
    let ctx = match state
        .harness
        .begin(&identity, &state.schema, state.manual_schema)
    {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::warn!(
                error = ?err,
                sub = %identity.sub,
                schema = %state.schema,
                "admin api: get_thread scope resolution failed"
            );
            return error_response(StatusCode::FORBIDDEN, "forbidden", err.to_string());
        }
    };

    let store = match state.harness.store() {
        Ok(store) => store,
        Err(err) => {
            tracing::error!(error = ?err, "admin api: get_thread knowledge store unavailable");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to load the thread; see server logs",
            );
        }
    };
    let attrs_list = match store
        .load_conversation_turns_for_case(&state.schema, &case_id)
        .await
    {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                error = ?err,
                schema = %state.schema,
                case_id,
                "admin api: get_thread load_conversation_turns_for_case failed"
            );
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to load the thread; see server logs",
            );
        }
    };
    let mut turns: Vec<TurnView> = attrs_list
        .iter()
        .filter_map(TurnRow::from_attrs)
        .map(TurnView::from)
        .collect();
    if turns.is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no conversation turns found for case_id={case_id}"),
        );
    }
    turns.sort_by_key(|t| t.seq);

    let conv = match state.harness.load_conv_state(&ctx, &case_id).await {
        Ok(conv) => conv,
        Err(err) => {
            tracing::error!(
                error = ?err,
                schema = %state.schema,
                case_id,
                "admin api: get_thread load_conv_state failed"
            );
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to load the thread; see server logs",
            );
        }
    };
    let signals = match state.harness.load_case_signals(&ctx, &case_id).await {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(
                error = ?err,
                schema = %state.schema,
                case_id,
                "admin api: get_thread load_case_signals failed"
            );
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to load the thread; see server logs",
            );
        }
    };

    let detail = ThreadDetail {
        case_id: case_id.clone(),
        case_ref: escalation_reply::case_ref(&case_id),
        turns,
        clarify_turns: conv.clarify_turns,
        preferred_contact_time: conv.preferred_contact_time,
        accumulated_signals: signals.iter().map(|s| s.as_str().to_string()).collect(),
    };
    (StatusCode::OK, Json(detail)).into_response()
}

#[derive(Debug, Deserialize)]
struct CorrectionRequest {
    case_id: String,
    turn_id: String,
    signals: Vec<String>,
    applicability: String,
    answer: String,
    #[serde(default)]
    rationale_text: Option<String>,
}

#[derive(Debug, Serialize)]
struct CorrectionResponse {
    kr_id: String,
    audit_event_id: String,
}

/// `rationale_text` 省略時に使う既定の根拠アンカー文言を組み立てる。
///
/// design doc §4 の API 契約は `rationale_text?`（任意）だが、admin correction 経路は
/// `manual_section_keys` を常に空で `register_known_resolution` を呼ぶ。一方
/// `Harness::admit_known_resolution` は「`manual_section_keys` が空 かつ `rationale_text` が
/// `None` の KR を拒否する」という不変条件を持つ（KR は最低 1 つの根拠アンカーを持たねば
/// ならないという、`add_known_resolution`（MCP tool）とも共有する監査可能性の要件。この
/// 不変条件自体は緩めない）。そのため `rationale_text` を省略すると実質的に必ず 400 になり、
/// 「任意」という API 契約に反していた（Issue #31 reviewer 指摘）。
///
/// ここで case_id / turn_id を含む自動生成テキストを根拠アンカーとして補うことで、
/// API 契約（任意のまま）と admission 側の不変条件（緩めない）の両方を満たす。
fn default_correction_rationale_text(case_id: &str, turn_id: &str) -> String {
    format!("管理画面からの訂正登録（case: {case_id}, turn: {turn_id}）")
}

/// `POST /admin/api/corrections`（design doc §4）。既存 `add_known_resolution`（MCP tool）と
/// 同じ harness 入口（`Harness::register_known_resolution`）を、ログイン中の Google identity の
/// actor で呼ぶ。`origin` は `case:{case_id}:turn:{turn_id}`（既存の `escalation:{id}` /
/// `manual` の慣習に倣う）。
async fn create_correction(
    State(state): State<AdminState>,
    Extension(identity): Extension<VerifiedIdentity>,
    body: Result<Json<CorrectionRequest>, JsonRejection>,
) -> Response {
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

    let ctx = match state
        .harness
        .begin(&identity, &state.schema, state.manual_schema)
    {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::warn!(
                error = ?err,
                sub = %identity.sub,
                schema = %state.schema,
                "admin api: create_correction scope resolution failed"
            );
            return error_response(StatusCode::FORBIDDEN, "forbidden", err.to_string());
        }
    };

    let rationale_text = req
        .rationale_text
        .clone()
        .unwrap_or_else(|| default_correction_rationale_text(&req.case_id, &req.turn_id));

    let origin = format!("case:{}:turn:{}", req.case_id, req.turn_id);
    match state
        .harness
        .register_known_resolution(
            &ctx,
            &req.signals,
            &req.answer,
            &req.applicability,
            Some(rationale_text.as_str()),
            &[],
            origin,
        )
        .await
    {
        Ok((kr_id, audit_event_id)) => (
            StatusCode::OK,
            Json(CorrectionResponse {
                kr_id,
                audit_event_id,
            }),
        )
            .into_response(),
        Err(RegisterKnownResolutionError::Admission(err)) => {
            error_response(StatusCode::BAD_REQUEST, "invalid_request", err.to_string())
        }
        Err(RegisterKnownResolutionError::Infra(err)) => {
            tracing::error!(
                error = ?err,
                schema = %state.schema,
                case_id = %req.case_id,
                turn_id = %req.turn_id,
                "admin api: create_correction insert/audit failed"
            );
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to register the correction; see server logs",
            )
        }
    }
}

#[derive(Debug, Deserialize)]
struct StatsQuery {
    #[serde(default)]
    days: Option<u32>,
}

/// `?days=` を検証する（`resolved_limit` と同じ「0 は既定値へフォールバック」規約）。
/// `STATS_MAX_DAYS` を超える値は `Err` を返し、ハンドラはこれを 400 として返す。
/// `chrono::DateTime - chrono::Duration` へ渡す前にここで弾くことで、クライアントが
/// 極端に大きい `days` を送っても panic（サーバクラッシュ）にならない。
fn resolved_days(query_days: Option<u32>) -> std::result::Result<u32, String> {
    let days = query_days.filter(|&d| d > 0).unwrap_or(STATS_DEFAULT_DAYS);
    if days > STATS_MAX_DAYS {
        return Err(format!(
            "days must be between 1 and {STATS_MAX_DAYS}, got {days}"
        ));
    }
    Ok(days)
}

/// `GET /admin/api/stats/summary?days=`（design doc §4）。
///
/// `harness.begin` を呼ぶのは、認可判定が入りうる唯一の入口をこの経路にも通しておくため
/// （`get_thread` / `create_correction` と同じ理由）。戻り値の `ctx` はサマリ集計自体には使わない
/// （schema は `state.schema` のまま）。呼ぶこと自体が目的で、現状（認証通過者は全員
/// supervisor）では常に成功するが、actor 突合表 DB が入った時にこの経路だけが認可を
/// 素通りしないようにする。
async fn stats_summary(
    State(state): State<AdminState>,
    Extension(identity): Extension<VerifiedIdentity>,
    Query(query): Query<StatsQuery>,
) -> Response {
    if let Err(err) = state
        .harness
        .begin(&identity, &state.schema, state.manual_schema)
    {
        tracing::warn!(
            error = ?err,
            sub = %identity.sub,
            schema = %state.schema,
            "admin api: stats_summary scope resolution failed"
        );
        return error_response(StatusCode::FORBIDDEN, "forbidden", err.to_string());
    }
    let days = match resolved_days(query.days) {
        Ok(days) => days,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, "invalid_request", message),
    };
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
    let store = match state.harness.store() {
        Ok(store) => store,
        Err(err) => {
            tracing::error!(error = ?err, "admin api: stats_summary knowledge store unavailable");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to load usage stats; see server logs",
            );
        }
    };
    match store
        .load_conversation_turns_since(&state.schema, &cutoff)
        .await
    {
        Ok(attrs_list) => {
            let rows: Vec<TurnRow> = attrs_list.iter().filter_map(TurnRow::from_attrs).collect();
            let summary = summarize_turns(days, &rows);
            (StatusCode::OK, Json(summary)).into_response()
        }
        Err(err) => {
            tracing::error!(
                error = ?err,
                schema = %state.schema,
                days,
                "admin api: stats_summary load_conversation_turns_since failed"
            );
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "failed to load usage stats; see server logs",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        case_id: &str,
        turn_id: &str,
        seq: u32,
        created_at: &str,
        question: &str,
        reply_kind: &str,
        end_user_id: Option<&str>,
    ) -> TurnRow {
        TurnRow {
            case_id: case_id.to_string(),
            turn_id: turn_id.to_string(),
            seq,
            created_at: created_at.to_string(),
            question: question.to_string(),
            reply_text: "回答".to_string(),
            reply_kind: reply_kind.to_string(),
            audit_event_id: "audit-1".to_string(),
            end_user_id: end_user_id.map(str::to_string),
        }
    }

    // ---- TurnRow::from_attrs ----

    #[test]
    fn turn_row_from_attrs_parses_all_fields() {
        let attrs: HashMap<String, String> = [
            ("case_id", "case-1"),
            ("turn_id", "turn-1"),
            ("seq", "2"),
            ("created_at", "2026-08-16T00:00:00+00:00"),
            ("question", "質問"),
            ("reply_text", "回答"),
            ("reply_kind", "answer"),
            ("audit_event_id", "audit-1"),
            ("end_user_id", "eu-1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let row = TurnRow::from_attrs(&attrs).expect("parses");
        assert_eq!(row.case_id, "case-1");
        assert_eq!(row.seq, 2);
        assert_eq!(row.end_user_id.as_deref(), Some("eu-1"));
    }

    #[test]
    fn turn_row_from_attrs_treats_empty_end_user_id_as_none() {
        let attrs: HashMap<String, String> = [
            ("case_id", "case-1"),
            ("turn_id", "turn-1"),
            ("created_at", "2026-08-16T00:00:00+00:00"),
            ("end_user_id", ""),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let row = TurnRow::from_attrs(&attrs).expect("parses");
        assert_eq!(row.end_user_id, None);
    }

    #[test]
    fn turn_row_from_attrs_rejects_missing_required_keys() {
        let attrs: HashMap<String, String> = [("turn_id", "turn-1")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(TurnRow::from_attrs(&attrs).is_none(), "case_id is missing");
    }

    // ---- aggregate_threads ----

    #[test]
    fn aggregate_threads_groups_by_case_and_counts_turns() {
        let rows = vec![
            row(
                "case-a",
                "t1",
                1,
                "2026-08-16T00:00:00+00:00",
                "質問1",
                "clarify",
                None,
            ),
            row(
                "case-a",
                "t2",
                2,
                "2026-08-16T00:05:00+00:00",
                "質問1続き",
                "answer",
                None,
            ),
            row(
                "case-b",
                "t3",
                1,
                "2026-08-16T00:02:00+00:00",
                "質問2",
                "escalation",
                Some("eu-1"),
            ),
        ];
        let summaries = aggregate_threads(rows);
        assert_eq!(summaries.len(), 2);
        let a = summaries.iter().find(|s| s.case_id == "case-a").unwrap();
        assert_eq!(a.turn_count, 2);
        assert_eq!(
            a.last_reply_kind, "answer",
            "seq=2 (最新) の reply_kind を採る"
        );
        assert_eq!(a.question_preview, "質問1", "先頭質問は seq==1 のもの");
        let b = summaries.iter().find(|s| s.case_id == "case-b").unwrap();
        assert_eq!(b.end_user_id.as_deref(), Some("eu-1"));
    }

    #[test]
    fn aggregate_threads_sorts_by_last_created_at_descending() {
        let rows = vec![
            row(
                "case-old",
                "t1",
                1,
                "2026-08-16T00:00:00+00:00",
                "古い",
                "answer",
                None,
            ),
            row(
                "case-new",
                "t2",
                1,
                "2026-08-16T01:00:00+00:00",
                "新しい",
                "answer",
                None,
            ),
        ];
        let summaries = aggregate_threads(rows);
        assert_eq!(summaries[0].case_id, "case-new");
        assert_eq!(summaries[1].case_id, "case-old");
    }

    #[test]
    fn aggregate_threads_truncates_question_preview_to_80_chars() {
        let long_question = "あ".repeat(200);
        let rows = vec![row(
            "case-a",
            "t1",
            1,
            "2026-08-16T00:00:00+00:00",
            &long_question,
            "answer",
            None,
        )];
        let summaries = aggregate_threads(rows);
        // truncate_chars は 80 字 + 省略記号(…) を付ける
        assert_eq!(summaries[0].question_preview.chars().count(), 81);
        assert!(summaries[0].question_preview.ends_with('…'));
    }

    #[test]
    fn aggregate_threads_falls_back_to_first_turn_when_seq_one_is_missing() {
        // 欠番（旧データ等で seq==1 が無い）でも先頭質問を落とさない。
        let rows = vec![row(
            "case-a",
            "t2",
            2,
            "2026-08-16T00:00:00+00:00",
            "seq2の質問",
            "answer",
            None,
        )];
        let summaries = aggregate_threads(rows);
        assert_eq!(summaries[0].question_preview, "seq2の質問");
    }

    // ---- next_internal_offset（ページング境界条件） ----

    #[test]
    fn next_internal_offset_stops_when_limit_reached() {
        assert_eq!(next_internal_offset(200, 200, 20, 20, 400), None);
    }

    #[test]
    fn next_internal_offset_stops_when_source_is_exhausted() {
        // 返却件数が page_size 未満 = これ以上取得しても増えない。
        assert_eq!(next_internal_offset(50, 200, 5, 20, 250), None);
    }

    #[test]
    fn next_internal_offset_continues_with_the_advanced_offset() {
        assert_eq!(next_internal_offset(200, 200, 5, 20, 400), Some(400));
    }

    #[test]
    fn next_internal_offset_stops_when_no_rows_were_returned() {
        // page_size 未満(0件)なので exhausted 判定で止まる。
        assert_eq!(next_internal_offset(0, 200, 0, 20, 0), None);
    }

    // ---- first_occurrence_index ----

    #[test]
    fn first_occurrence_index_records_the_first_position_per_case() {
        let rows = vec![
            row("case-a", "t1", 1, "t", "q", "answer", None),
            row("case-b", "t2", 1, "t", "q", "answer", None),
            row("case-a", "t3", 2, "t", "q", "answer", None),
        ];
        let order = first_occurrence_index(&rows);
        assert_eq!(order.get("case-a"), Some(&0), "first occurrence, not last");
        assert_eq!(order.get("case-b"), Some(&1));
    }

    // ---- paginate_summaries ----

    #[test]
    fn paginate_summaries_returns_no_cursor_when_under_limit_and_source_exhausted() {
        // 真の終端（分岐3）: 切り詰めなし かつ 取得元が尽きている。
        let summaries = vec![ThreadSummary {
            case_id: "case-a".to_string(),
            case_ref: "a".to_string(),
            end_user_id: None,
            question_preview: "q".to_string(),
            turn_count: 1,
            last_reply_kind: "answer".to_string(),
            last_created_at: "2026-08-16T00:00:00+00:00".to_string(),
        }];
        let rows = vec![row(
            "case-a",
            "t1",
            1,
            "2026-08-16T00:00:00+00:00",
            "q",
            "answer",
            None,
        )];
        let (page, next_cursor) = paginate_summaries(summaries, 20, &rows, 0, true);
        assert_eq!(page.len(), 1);
        assert_eq!(next_cursor, None);
    }

    #[test]
    fn paginate_summaries_emits_a_cursor_when_under_limit_but_source_not_exhausted() {
        // 分岐2: distinct case 数が limit にちょうど届いた、またはラウンド上限で打ち切られたが
        // 取得元はまだ尽きていない場合。修正前はここで next_cursor が None になり、後続データが
        // 永久に見えなくなっていた（Critical指摘）。
        let summaries = vec![ThreadSummary {
            case_id: "case-a".to_string(),
            case_ref: "a".to_string(),
            end_user_id: None,
            question_preview: "q".to_string(),
            turn_count: 1,
            last_reply_kind: "answer".to_string(),
            last_created_at: "2026-08-16T00:00:00+00:00".to_string(),
        }];
        let rows = vec![row(
            "case-a",
            "t1",
            1,
            "2026-08-16T00:00:00+00:00",
            "q",
            "answer",
            None,
        )];
        let (page, next_cursor) = paginate_summaries(summaries, 20, &rows, 100, false);
        assert_eq!(page.len(), 1);
        assert_eq!(
            decode_cursor(&next_cursor.expect("must emit a cursor when source is not exhausted")),
            Some(101),
            "cutoff must resume right after the raw rows consumed so far (initial_offset + rows.len())"
        );
    }

    #[test]
    fn paginate_summaries_truncates_and_emits_a_cursor_when_over_limit() {
        let summaries: Vec<ThreadSummary> = (0..5)
            .map(|i| ThreadSummary {
                case_id: format!("case-{i}"),
                case_ref: format!("c{i}"),
                end_user_id: None,
                question_preview: "q".to_string(),
                turn_count: 1,
                last_reply_kind: "answer".to_string(),
                last_created_at: format!("2026-08-16T00:0{i}:00+00:00"),
            })
            .collect();
        // raw 行の出現順（index）は case-0..case-4 の順（sort_by の外側で用意した summaries と
        // 独立に、`rows` 自体の並びが cutoff の基準になることを確認する）。
        let rows: Vec<TurnRow> = (0..5)
            .map(|i| {
                row(
                    &format!("case-{i}"),
                    &format!("t{i}"),
                    1,
                    &format!("2026-08-16T00:0{i}:00+00:00"),
                    "q",
                    "answer",
                    None,
                )
            })
            .collect();
        // 分岐1（切り詰め）は source_exhausted の値に関わらず discard 分の raw 行を守るために
        // cursor を出す。ここでは「まだ後続データがある」典型ケースとして false を渡す。
        let (page, next_cursor) = paginate_summaries(summaries, 3, &rows, 0, false);
        assert_eq!(page.len(), 3);
        let cursor = next_cursor.expect("must emit a cursor when truncated");
        assert_eq!(
            decode_cursor(&cursor),
            Some(3),
            "cursor must resume right after the last kept thread's raw row (index 2 -> offset 3)"
        );
    }

    #[test]
    fn paginate_summaries_offsets_the_cutoff_by_initial_offset() {
        let summaries: Vec<ThreadSummary> = (0..3)
            .map(|i| ThreadSummary {
                case_id: format!("case-{i}"),
                case_ref: format!("c{i}"),
                end_user_id: None,
                question_preview: "q".to_string(),
                turn_count: 1,
                last_reply_kind: "answer".to_string(),
                last_created_at: format!("2026-08-16T00:0{i}:00+00:00"),
            })
            .collect();
        let rows: Vec<TurnRow> = (0..3)
            .map(|i| {
                row(
                    &format!("case-{i}"),
                    &format!("t{i}"),
                    1,
                    &format!("2026-08-16T00:0{i}:00+00:00"),
                    "q",
                    "answer",
                    None,
                )
            })
            .collect();
        let (page, next_cursor) = paginate_summaries(summaries, 1, &rows, 100, false);
        assert_eq!(page.len(), 1);
        assert_eq!(
            decode_cursor(&next_cursor.unwrap()),
            Some(101),
            "cutoff must be relative to initial_offset, not 0"
        );
    }

    // ---- cursor encode/decode ----

    #[test]
    fn cursor_roundtrips_through_encode_and_decode() {
        let original = 42;
        let encoded = encode_cursor(original);
        assert_eq!(decode_cursor(&encoded), Some(original));
    }

    #[test]
    fn decode_cursor_returns_none_for_garbage_input() {
        assert_eq!(decode_cursor("not valid base64!!"), None);
    }

    #[test]
    fn decode_cursor_returns_none_for_empty_string() {
        assert_eq!(decode_cursor(""), None);
    }

    #[test]
    fn decode_cursor_returns_none_for_negative_offset() {
        // 負の offset は vegapunk::query_nodes_paged と同じ「offset は 0 起点」規約に反する。
        // 壊れた cursor と同様、500 にはせず「cursor 無し」（先頭から）として扱う。
        assert_eq!(decode_cursor(&encode_cursor(-1)), None);
    }

    // ---- summarize_turns ----

    #[test]
    fn summarize_turns_counts_reply_kind_distribution_and_uniques() {
        let rows = vec![
            row(
                "case-a",
                "t1",
                1,
                "2026-08-16T00:00:00+00:00",
                "q",
                "answer",
                Some("eu-1"),
            ),
            row(
                "case-a",
                "t2",
                2,
                "2026-08-16T00:01:00+00:00",
                "q",
                "clarify",
                Some("eu-1"),
            ),
            row(
                "case-b",
                "t3",
                1,
                "2026-08-16T00:02:00+00:00",
                "q",
                "answer",
                Some("eu-2"),
            ),
        ];
        let summary = summarize_turns(7, &rows);
        assert_eq!(summary.turn_count, 3);
        assert_eq!(summary.thread_count, 2, "case-a と case-b の 2 スレッド");
        assert_eq!(summary.unique_end_user_count, 2);
        assert_eq!(summary.reply_kind_counts.get("answer"), Some(&2));
        assert_eq!(summary.reply_kind_counts.get("clarify"), Some(&1));
    }

    #[test]
    fn summarize_turns_ignores_turns_without_end_user_id() {
        let rows = vec![row(
            "case-a",
            "t1",
            1,
            "2026-08-16T00:00:00+00:00",
            "q",
            "answer",
            None,
        )];
        let summary = summarize_turns(1, &rows);
        assert_eq!(summary.unique_end_user_count, 0);
    }

    #[test]
    fn summarize_turns_on_empty_input_returns_zeros() {
        let summary = summarize_turns(7, &[]);
        assert_eq!(summary.turn_count, 0);
        assert_eq!(summary.thread_count, 0);
        assert_eq!(summary.unique_end_user_count, 0);
        assert!(summary.reply_kind_counts.is_empty());
    }

    // ---- resolved_limit ----

    #[test]
    fn resolved_limit_uses_default_when_absent() {
        assert_eq!(resolved_limit(None), THREADS_DEFAULT_LIMIT);
    }

    #[test]
    fn resolved_limit_uses_default_when_zero() {
        // limit=0 は「0件返す」ではなく既定値へフォールバックする（0 で永久に next_cursor
        // が出ないなど扱いに困る境界値を避ける）。
        assert_eq!(resolved_limit(Some(0)), THREADS_DEFAULT_LIMIT);
    }

    #[test]
    fn resolved_limit_passes_through_a_positive_value() {
        assert_eq!(resolved_limit(Some(5)), 5);
    }

    // ---- ルーティング: 未認証 401（既存の require_google_auth テストパターンに倣う） ----
    //
    // `oauth::middleware` のテストと同じ構成（到達不能な tokeninfo URL で verifier を包む。
    // 呼んでしまえば 503 になるため、401 が返ること自体が「verifier を呼ばず短絡した」ことの
    // 証拠になる）。

    async fn dead_tokeninfo() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}/")
    }

    fn test_admin_state() -> AdminState {
        test_admin_state_with_lexicon(r#"{"signals":[]}"#)
    }

    /// `test_admin_state` の signal 語彙を差し替えられる版。`create_correction` の admission
    /// を実際に通過させる（`Infra` ステップへ到達させる）テストは、`admit_known_resolution` の
    /// vocabulary チェックを通すために既知の signal が必要（`harness/mod.rs` の
    /// `harness_for_test_with_lexicon` と同じパターン）。
    fn test_admin_state_with_lexicon(lexicon_json: &str) -> AdminState {
        let dir = std::env::temp_dir().join(format!("admin-test-{}", uuid::Uuid::new_v4()));
        let lexicon =
            Arc::new(crate::harness::signal::LexiconNormalizer::from_json(lexicon_json).unwrap());
        let harness = Harness {
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
            product_gate: None,
        };
        AdminState {
            schema: "urtect".to_string(),
            manual_schema: ManualSchemaKind::ManualV1,
            harness: Arc::new(harness),
        }
    }

    async fn guarded_test_router() -> Router {
        let auth_state = crate::oauth::middleware::AuthState {
            verifier: Arc::new(crate::oauth::verifier::GoogleTokenVerifier::with_settings(
                "test-client-id".to_string(),
                dead_tokeninfo().await,
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(2),
            )),
            resource_metadata_url: "https://h/.well-known/oauth-protected-resource/urtect/mcp"
                .to_string(),
        };
        admin_router(test_admin_state()).layer(axum::middleware::from_fn_with_state(
            auth_state,
            crate::oauth::middleware::require_google_auth,
        ))
    }

    async fn get_without_auth(router: Router, uri: &str) -> StatusCode {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        router
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn threads_route_rejects_unauthenticated_requests_with_401() {
        let status = get_without_auth(guarded_test_router().await, "/threads").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn thread_detail_route_rejects_unauthenticated_requests_with_401() {
        let status = get_without_auth(guarded_test_router().await, "/threads/case-1").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn user_threads_route_rejects_unauthenticated_requests_with_401() {
        let status = get_without_auth(guarded_test_router().await, "/users/eu-1/threads").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stats_summary_route_rejects_unauthenticated_requests_with_401() {
        let status = get_without_auth(guarded_test_router().await, "/stats/summary").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn corrections_route_rejects_unauthenticated_requests_with_401() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let router = guarded_test_router().await;
        let status = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/corrections")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ---- fetch_thread_page / get_thread / create_correction: knowledge: None での 500 経路 ----
    //
    // `test_admin_state()` は `knowledge: None` なので、認証をバイパスして直接ハンドラ相当の
    // ロジックを呼べば必ず 500 になる（実 vegapunk 無しで到達できる経路）。

    #[tokio::test]
    async fn fetch_thread_page_fails_when_knowledge_is_unavailable() {
        let state = test_admin_state();
        let err = fetch_thread_page(&state, None, 20, 0)
            .await
            .expect_err("knowledge: None must fail");
        assert!(err.to_string().contains("knowledge"));
    }

    // ---- build_threads_page: offset ページングの境界条件（Critical指摘5） ----
    //
    // `fetch_thread_page` は実 vegapunk（network I/O）に依存するため、`test_admin_state()`
    // （`knowledge: None`）ではページング完走を検証できない。`build_threads_page` はその
    // raw ページ取得を注入可能にした内部関数で、ここではネットワーク無しの決定論的フェイクを
    // 渡して、実際の本番コード経路（`fetch_thread_page` が呼ぶのと同じ関数）を直接検証する。

    #[tokio::test]
    async fn threads_pagination_with_tied_created_at_covers_every_case_exactly_once() {
        // 境界の created_at が完全一致する複数 case を用意し、limit をちょうどその境界で割る
        // （Critical指摘5: (a) 値ベースの `created_at < cursor` フィルタは完全一致する境界の
        // エントリを取りこぼしうる、(b) 集約後 case の境界と raw 行の境界がずれて境界 case が
        // 次ページで重複しうる）。offset ベースのページングに切り替えた後は、複数ページを
        // 辿っても全 case が重複なく・欠落なく1回ずつ出現しなければならない。
        let tied = "2026-08-16T00:00:00+00:00";
        let all_rows: Vec<TurnRow> = (0..6)
            .map(|i| {
                row(
                    &format!("case-{i}"),
                    &format!("t{i}"),
                    1,
                    tied,
                    "q",
                    "answer",
                    None,
                )
            })
            .collect();
        let all_rows = std::sync::Arc::new(all_rows);
        let limit = 3;

        let mut seen: Vec<String> = Vec::new();
        let mut offset = 0i32;
        for _round in 0..10 {
            let source = all_rows.clone();
            let page = build_threads_page(limit, offset, move |o, page_size| {
                let source = source.clone();
                Box::pin(async move {
                    let start = o.max(0) as usize;
                    let end = (start + page_size as usize).min(source.len());
                    Ok(if start >= source.len() {
                        Vec::new()
                    } else {
                        source[start..end].to_vec()
                    })
                })
            })
            .await
            .expect("fake page source never fails");
            assert!(
                !page.threads.is_empty(),
                "must not return an empty page while there is still unseen data"
            );
            for t in &page.threads {
                assert!(
                    !seen.contains(&t.case_id),
                    "case {} appeared more than once across pages",
                    t.case_id
                );
                seen.push(t.case_id.clone());
            }
            match page.next_cursor {
                Some(c) => {
                    offset = decode_cursor(&c).expect("cursor must decode to a valid offset")
                }
                None => break,
            }
        }
        let mut expected: Vec<String> = (0..6).map(|i| format!("case-{i}")).collect();
        let mut seen_sorted = seen.clone();
        seen_sorted.sort();
        expected.sort();
        assert_eq!(
            seen_sorted, expected,
            "all 6 tied-timestamp cases must appear exactly once across all pages, got: {seen:?}"
        );
    }

    // ---- build_threads_page: page_size ちょうどの境界で打ち切られるケース（Issue #31
    // reviewer 指摘: distinct case 数が limit にちょうど一致した場合や、ラウンド上限で
    // 打ち切った場合に next_cursor が None になり、後続データが管理画面から永久に見えなく
    // なる欠陥があった） ----
    //
    // 既存の tied_created_at テストは全 6 行が 1 回のフェイクページ（page_size=200）に収まる
    // ため、この境界（1 回のフェッチが THREADS_FETCH_PAGE_SIZE ちょうどを返す）を再現できない。
    // ここでは `THREADS_FETCH_PAGE_SIZE` ちょうどの行数を返しうるフェイクを使う。

    /// `source` を `initial_offset`/`page_size` に応じてスライスして返すフェイク `fetch_page`。
    /// 複数テストで共有する（tied_created_at テストの匿名クロージャと同じロジック）。
    fn slice_fetcher(
        source: std::sync::Arc<Vec<TurnRow>>,
    ) -> impl FnMut(i32, i32) -> PageFuture<'static> {
        move |offset, page_size| {
            let source = source.clone();
            Box::pin(async move {
                let start = offset.max(0) as usize;
                let end = (start + page_size as usize).min(source.len());
                Ok(if start >= source.len() {
                    Vec::new()
                } else {
                    source[start..end].to_vec()
                })
            })
        }
    }

    /// 先頭 `THREADS_FETCH_PAGE_SIZE` 件が `distinct` 個の case へ均等に分散し、その直後に
    /// 別 case（"case-tail"）のターンが `tail_len` 件続くデータセット。「1 回の内部フェッチが
    /// page_size ちょうど返し、かつ distinct case 数が limit にちょうど一致する」境界を
    /// 再現するために使う。
    fn page_size_boundary_dataset(distinct: usize, tail_len: usize) -> Vec<TurnRow> {
        let page_size = THREADS_FETCH_PAGE_SIZE as usize;
        let mut rows: Vec<TurnRow> = (0..page_size)
            .map(|i| {
                row(
                    &format!("case-{}", i % distinct),
                    &format!("t{i}"),
                    1,
                    "2026-08-16T00:00:00+00:00",
                    "q",
                    "answer",
                    None,
                )
            })
            .collect();
        rows.extend((0..tail_len).map(|i| {
            row(
                "case-tail",
                &format!("tail-{i}"),
                1,
                "2026-08-16T01:00:00+00:00",
                "q",
                "answer",
                None,
            )
        }));
        rows
    }

    #[tokio::test]
    async fn build_threads_page_emits_a_cursor_when_distinct_equals_limit_but_source_not_exhausted()
    {
        // 1 回の内部フェッチが THREADS_FETCH_PAGE_SIZE ちょうど返し、その全件が limit と同数の
        // distinct case に属し、かつ取得元にはまだ後続 case（"case-tail"）が残っている。
        // 修正前は distinct >= limit で早期 return し、next_cursor が None になって
        // "case-tail" が管理画面から永久に見えなくなっていた。
        let limit = 3;
        let dataset = std::sync::Arc::new(page_size_boundary_dataset(limit, 5));

        let page = build_threads_page(limit, 0, slice_fetcher(dataset.clone()))
            .await
            .expect("fake page source never fails");
        assert_eq!(page.threads.len(), limit);
        let cursor = page
            .next_cursor
            .expect("must emit a cursor: the source is not exhausted yet");
        let next_offset = decode_cursor(&cursor).expect("cursor must decode to a valid offset");
        assert_eq!(
            next_offset, THREADS_FETCH_PAGE_SIZE,
            "cutoff must resume right after the fully-consumed first page"
        );

        let next_page = build_threads_page(limit, next_offset, slice_fetcher(dataset))
            .await
            .expect("fake page source never fails");
        assert_eq!(
            next_page.threads.len(),
            1,
            "the remaining tail case must be reachable via the cursor from the first page"
        );
        assert_eq!(next_page.threads[0].case_id, "case-tail");
        assert_eq!(
            next_page.next_cursor, None,
            "the tail case exhausts the source"
        );
    }

    #[tokio::test]
    async fn build_threads_page_across_pages_covers_every_case_exactly_once_at_the_page_size_boundary(
    ) {
        // 上と同じデータ形状で、next_cursor が None になるまで辿ると全 case が重複なく・
        // 欠落なく1回ずつ出現する（tied_created_at テストと同じ検証形式）。
        let limit = 3;
        let dataset = std::sync::Arc::new(page_size_boundary_dataset(limit, 5));

        let mut seen: Vec<String> = Vec::new();
        let mut offset = 0i32;
        loop {
            let page = build_threads_page(limit, offset, slice_fetcher(dataset.clone()))
                .await
                .expect("fake page source never fails");
            assert!(
                !page.threads.is_empty(),
                "must not return an empty page while there is still unseen data"
            );
            for t in &page.threads {
                assert!(
                    !seen.contains(&t.case_id),
                    "case {} appeared more than once across pages",
                    t.case_id
                );
                seen.push(t.case_id.clone());
            }
            match page.next_cursor {
                Some(c) => {
                    offset = decode_cursor(&c).expect("cursor must decode to a valid offset")
                }
                None => break,
            }
        }
        let mut expected: Vec<String> = (0..limit).map(|i| format!("case-{i}")).collect();
        expected.push("case-tail".to_string());
        let mut seen_sorted = seen.clone();
        seen_sorted.sort();
        expected.sort();
        assert_eq!(
            seen_sorted, expected,
            "all cases must appear exactly once across all pages, got: {seen:?}"
        );
    }

    #[tokio::test]
    async fn build_threads_page_returns_no_cursor_at_the_true_end() {
        // 取得元が尽きた（returned_count < page_size）かつ distinct <= limit のとき
        // next_cursor は None（真の終端）。
        let dataset = std::sync::Arc::new(vec![
            row(
                "case-a",
                "t1",
                1,
                "2026-08-16T00:00:00+00:00",
                "q",
                "answer",
                None,
            ),
            row(
                "case-b",
                "t2",
                1,
                "2026-08-16T00:01:00+00:00",
                "q",
                "answer",
                None,
            ),
        ]);
        let page = build_threads_page(10, 0, slice_fetcher(dataset))
            .await
            .expect("fake page source never fails");
        assert_eq!(page.threads.len(), 2);
        assert_eq!(page.next_cursor, None);
    }

    #[tokio::test]
    async fn build_threads_page_emits_a_cursor_when_the_round_limit_is_hit_before_reaching_limit() {
        // 全行が同一 case ("case-solo") に属し、distinct case 数が永遠に limit(3) に届かない
        // フェイク。毎ラウンド THREADS_FETCH_PAGE_SIZE ちょうどを返す（= 取得元は尽きていない）
        // ため、MAX_INTERNAL_FETCH_ROUNDS を使い切ってもループは自然終了する。修正前はこの
        // ケースでも next_cursor が None になり、取得元が尽きていないのに「これで全部」と
        // 返していた。
        let limit = 3;
        let page_size = THREADS_FETCH_PAGE_SIZE as usize;
        let fetch_page = |offset: i32, requested: i32| -> PageFuture<'static> {
            Box::pin(async move {
                Ok((0..requested as usize)
                    .map(|i| {
                        row(
                            "case-solo",
                            &format!("t-{offset}-{i}"),
                            1,
                            "2026-08-16T00:00:00+00:00",
                            "q",
                            "answer",
                            None,
                        )
                    })
                    .collect())
            })
        };
        let page = build_threads_page(limit, 0, fetch_page)
            .await
            .expect("fake page source never fails");
        assert_eq!(page.threads.len(), 1, "only one distinct case exists");
        let cursor = page.next_cursor.expect(
            "must emit a cursor: the round limit was hit but the source was never exhausted \
             (every round returned exactly page_size rows)",
        );
        let offset = decode_cursor(&cursor).expect("cursor must decode to a valid offset");
        assert_eq!(
            offset,
            (MAX_INTERNAL_FETCH_ROUNDS * page_size) as i32,
            "offset must have advanced by page_size on every one of the MAX_INTERNAL_FETCH_ROUNDS rounds"
        );
    }

    // ---- resolved_days（Critical指摘3: stats/summary の panic 経路） ----

    #[test]
    fn resolved_days_uses_default_when_absent() {
        assert_eq!(resolved_days(None), Ok(STATS_DEFAULT_DAYS));
    }

    #[test]
    fn resolved_days_uses_default_when_zero() {
        assert_eq!(resolved_days(Some(0)), Ok(STATS_DEFAULT_DAYS));
    }

    #[test]
    fn resolved_days_passes_through_a_value_within_the_cap() {
        assert_eq!(resolved_days(Some(STATS_MAX_DAYS)), Ok(STATS_MAX_DAYS));
    }

    #[test]
    fn resolved_days_rejects_a_value_beyond_the_cap() {
        // `chrono::DateTime - chrono::Duration` は表現可能範囲を超えると `expect` で panic
        // する。ここで Err を返すことが、その panic を未然に防ぐ唯一のガードになる。
        assert!(resolved_days(Some(STATS_MAX_DAYS + 1)).is_err());
    }

    #[tokio::test]
    async fn stats_summary_route_rejects_an_excessive_days_value_with_400_not_a_panic() {
        // 修正前は `chrono::DateTime - chrono::Duration` が表現可能範囲（西暦 262000 年ごろ）を
        // 超えて `expect` で panic していた（Critical指摘3）。`u32::MAX` まで行かなくても、
        // 3650 日の上限を大きく超える値（100,000,000 日 ≈ 27 万年）で十分再現する。
        let state = test_admin_state();
        let response = stats_summary(
            State(state),
            Extension(supervisor_identity()),
            Query(StatsQuery {
                days: Some(100_000_000),
            }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "an excessive days value must be rejected as a 400, not panic the server"
        );
    }

    // ---- default_correction_rationale_text / create_correction（Critical指摘4） ----

    #[test]
    fn default_correction_rationale_text_includes_case_and_turn_ids() {
        let text = default_correction_rationale_text("case-42", "turn-7");
        assert!(text.contains("case-42"));
        assert!(text.contains("turn-7"));
    }

    fn supervisor_identity() -> VerifiedIdentity {
        VerifiedIdentity {
            sub: "admin-test-user".to_string(),
            email: "admin-test-user@example.com".to_string(),
        }
    }

    /// `harness::mod` の `register_known_resolution_reaches_infra_step_when_admission_passes`
    /// と同じ立証パターン: `test_admin_state()` は `knowledge: None` なので実際に vegapunk へ
    /// 繋がる 200 をここでは確認できない。しかし admission に拒否されれば 400 (Admission) に
    /// なるはずなので、500 (Infra) まで到達すること自体が「admission を通過した」ことの証拠に
    /// なる。修正前は admin correction 経路が `manual_section_keys` を常に空で呼ぶため、
    /// `rationale_text` 省略時は必ず 400 になっていた（Critical指摘4）。
    async fn assert_correction_reaches_infra_step(rationale_text: Option<String>) {
        let state = test_admin_state_with_lexicon(
            r#"{"signals":[{"signal":"mold","class":"hazard","surface_forms":["カビ"]}]}"#,
        );
        let req = CorrectionRequest {
            case_id: "case-1".to_string(),
            turn_id: "turn-1".to_string(),
            signals: vec!["mold".to_string()],
            applicability: "全ロット".to_string(),
            answer: "回答文".to_string(),
            rationale_text,
        };
        let response = create_correction(
            State(state),
            Extension(supervisor_identity()),
            Ok(Json(req)),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "must reach the infra step (500, knowledge: None), not an admission-level 400"
        );
    }

    #[tokio::test]
    async fn create_correction_without_rationale_text_still_passes_admission() {
        assert_correction_reaches_infra_step(None).await;
    }

    #[tokio::test]
    async fn create_correction_with_explicit_rationale_text_passes_admission() {
        assert_correction_reaches_infra_step(Some("明示的な理由".to_string())).await;
    }

    // ---- list_threads / list_user_threads / stats_summary: begin を通過することの証拠
    // （Issue #31 reviewer 指摘: この 3 ハンドラは harness.begin を一切呼んでおらず、認可の
    // 入口を素通りしていた） ----

    /// `test_admin_state()` は `knowledge: None` なので、認証済み identity を渡した場合に
    /// 403（scope 解決失敗）ではなく 500（内部フェッチ失敗）へ到達することが「begin を
    /// 通過した」ことの証拠になる（`fetch_thread_page_fails_when_knowledge_is_unavailable` /
    /// `assert_correction_reaches_infra_step` と同じ立証の流儀）。
    #[tokio::test]
    async fn list_endpoints_reach_the_infra_step_after_scope_resolution_succeeds() {
        let state = test_admin_state();

        let response = list_threads(
            State(state.clone()),
            Extension(supervisor_identity()),
            Query(ThreadsQuery {
                limit: None,
                cursor: None,
            }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "list_threads must reach the infra step (500), not stop at scope resolution (403)"
        );

        let response = list_user_threads(
            State(state.clone()),
            Extension(supervisor_identity()),
            Path("eu-1".to_string()),
            Query(ThreadsQuery {
                limit: None,
                cursor: None,
            }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "list_user_threads must reach the infra step (500), not stop at scope resolution (403)"
        );

        let response = stats_summary(
            State(state),
            Extension(supervisor_identity()),
            Query(StatsQuery { days: None }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "stats_summary must reach the infra step (500), not stop at scope resolution (403)"
        );
    }
}
