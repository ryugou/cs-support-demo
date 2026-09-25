import type { ThreadSummary } from '../core/models';

/**
 * スレッド一覧の表示カットオフ（UTC）。デモ用の一時的な表示カットオフ。既存の会話を
 * 一覧から隠すため。
 *
 * 撤去条件: デモ終了後、本モジュール（`thread-cutoff.ts` / `thread-cutoff.spec.ts`）と、
 * これを呼び出している2箇所（`features/threads/thread-list.ts`,
 * `features/user-threads/user-threads.ts`）を削除すれば、カットオフ適用前の挙動に戻る。
 */
const THREAD_DISPLAY_CUTOFF_MS = Date.parse('2026-09-25T04:30:49Z');

/**
 * `ThreadSummary.last_created_at`（最新ターンの発生時刻。design doc §4 の
 * `aggregate_threads` が `created_at` 最大のターンから採る値）がカットオフ以降かどうかを
 * 判定する。空文字・パース不能なタイムスタンプは、カットオフより前の既存データである
 * 可能性が高いため非表示側（false）に倒す（fail-closed）。
 */
export function isAfterDisplayCutoff(lastCreatedAt: string): boolean {
  const parsed = Date.parse(lastCreatedAt);
  return Number.isFinite(parsed) && parsed >= THREAD_DISPLAY_CUTOFF_MS;
}

/** スレッド一覧・ユーザー別一覧の両方から呼ぶ共通フィルタ。ページング仕様は変更しない
 * （除外により1ページの件数が減ることは許容する）。 */
export function filterThreadsAfterCutoff(threads: readonly ThreadSummary[]): ThreadSummary[] {
  return threads.filter((thread) => isAfterDisplayCutoff(thread.last_created_at));
}
