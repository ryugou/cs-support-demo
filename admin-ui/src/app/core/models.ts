/**
 * `/{project_id}/admin/api/*` のレスポンス/リクエスト型。
 *
 * 正本は `docs/superpowers/specs/2026-08-16-admin-dashboard-design.md` §4 と
 * `server/src/admin.rs`（実装済み）。フィールド名・省略可否はサーバ実装に合わせる
 * （このファイル単体で仕様を変えない）。
 */

/** スレッド一覧・ユーザー別一覧の 1 行。`server/src/admin.rs` の `ThreadSummary`。 */
export interface ThreadSummary {
  case_id: string;
  case_ref: string;
  end_user_id: string | null;
  question_preview: string;
  turn_count: number;
  last_reply_kind: ReplyKind;
  last_created_at: string;
}

/** `GET /threads` / `GET /users/{end_user_id}/threads` の共通レスポンス形式。 */
export interface ThreadsPage {
  threads: ThreadSummary[];
  next_cursor: string | null;
}

/** スレッド詳細の 1 ターン。`server/src/admin.rs` の `TurnView`。 */
export interface TurnView {
  turn_id: string;
  seq: number;
  created_at: string;
  question: string;
  reply_text: string;
  reply_kind: ReplyKind;
  audit_event_id: string;
  end_user_id: string | null;
}

/** `GET /threads/{case_id}` のレスポンス。 */
export interface ThreadDetail {
  case_id: string;
  case_ref: string;
  turns: TurnView[];
  clarify_turns: number;
  preferred_contact_time: string | null;
  accumulated_signals: string[];
}

/** `POST /corrections` のリクエストボディ。`rationale_text` は省略可（サーバが既定文言を補う）。 */
export interface CorrectionRequest {
  case_id: string;
  turn_id: string;
  signals: string[];
  applicability: string;
  answer: string;
  rationale_text?: string;
}

/** `POST /corrections` の成功レスポンス。 */
export interface CorrectionResponse {
  kr_id: string;
  audit_event_id: string;
}

/** `GET /stats/summary` のレスポンス。 */
export interface StatsSummary {
  days: number;
  turn_count: number;
  thread_count: number;
  reply_kind_counts: Record<string, number>;
  unique_end_user_count: number;
}

/** 管理 API 共通のエラーレスポンス形式（`/api/reply` と共有、`crate::api::error_response`）。 */
export interface ErrorBody {
  error: string;
  message: string;
}

/**
 * ターンの最終応答種別（design doc §4、`server/src/admin.rs` の `TurnRow::reply_kind` が
 * そのまま透過する自由文字列）。サーバは将来値を追加しうるため、未知の値も受理できるよう
 * `string` を許容するユニオンにする（バッジ表示側は未知値のフォールバックを持つ）。
 */
export type ReplyKind =
  | 'answer'
  | 'clarify'
  | 'escalation'
  | 'out_of_scope'
  | 'time_pref'
  | 'fallback'
  | 'handoff'
  | 'safety'
  | 'out_of_domain'
  | 'lead'
  | (string & {});
