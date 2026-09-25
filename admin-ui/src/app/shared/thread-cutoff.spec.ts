import type { ThreadSummary } from '../core/models';
import { filterThreadsAfterCutoff, isAfterDisplayCutoff } from './thread-cutoff';

function summary(lastCreatedAt: string): ThreadSummary {
  return {
    case_id: 'case-1',
    case_ref: 'c1',
    end_user_id: null,
    question_preview: 'q',
    turn_count: 1,
    last_reply_kind: 'answer',
    last_created_at: lastCreatedAt,
  };
}

describe('isAfterDisplayCutoff', () => {
  it('カットオフ直前のタイムスタンプを false と判定する', () => {
    expect(isAfterDisplayCutoff('2026-09-25T04:30:48Z')).toBe(false);
  });

  it('カットオフちょうどのタイムスタンプを true と判定する', () => {
    expect(isAfterDisplayCutoff('2026-09-25T04:30:49Z')).toBe(true);
  });

  it('カットオフ以降のタイムスタンプを true と判定する', () => {
    expect(isAfterDisplayCutoff('2026-09-25T04:30:50Z')).toBe(true);
  });

  it('タイムゾーンオフセット表記でも UTC 換算で比較する', () => {
    // JST 13:30:49 == UTC 04:30:49（カットオフちょうど）
    expect(isAfterDisplayCutoff('2026-09-25T13:30:49+09:00')).toBe(true);
  });

  it('空文字は fail-closed で false を返す', () => {
    expect(isAfterDisplayCutoff('')).toBe(false);
  });

  it('パース不能な文字列は fail-closed で false を返す', () => {
    expect(isAfterDisplayCutoff('not-a-timestamp')).toBe(false);
  });

  // サーバの chrono::Utc::now().to_rfc3339() はナノ秒精度 + `+00:00` オフセットの文字列
  // （例: 2026-09-25T04:30:49.123456789+00:00）を生成する。この形式は ECMAScript の
  // Date Time String Format（ミリ秒3桁まで）の範囲外で、Date.parse の実装依存パスを通る。
  // fail-closed 設計のため、ここでパースが崩れると一覧が丸ごと空になる。
  it('サーバの to_rfc3339() が生むナノ秒精度+オフセット形式（カットオフ以降）を true と判定する', () => {
    expect(isAfterDisplayCutoff('2026-09-25T04:30:49.123456789+00:00')).toBe(true);
  });

  it('サーバの to_rfc3339() が生むナノ秒精度+オフセット形式（カットオフ直後）を true と判定する', () => {
    expect(isAfterDisplayCutoff('2026-09-25T04:30:49.000000001+00:00')).toBe(true);
  });

  it('サーバの to_rfc3339() が生むナノ秒精度+オフセット形式（カットオフ直前）を false と判定する', () => {
    expect(isAfterDisplayCutoff('2026-09-25T04:30:48.999999999+00:00')).toBe(false);
  });
});

describe('filterThreadsAfterCutoff', () => {
  it('カットオフより前の会話を一覧から除外する', () => {
    const threads = [summary('2020-01-01T00:00:00Z'), summary('2026-09-25T04:30:49Z')];
    const result = filterThreadsAfterCutoff(threads);
    expect(result).toEqual([summary('2026-09-25T04:30:49Z')]);
  });

  it('タイムスタンプが欠落・パース不能な行を除外する', () => {
    const threads = [summary(''), summary('garbage'), summary('2026-09-25T04:30:50Z')];
    const result = filterThreadsAfterCutoff(threads);
    expect(result).toEqual([summary('2026-09-25T04:30:50Z')]);
  });

  it('空配列を渡すと空配列を返す', () => {
    expect(filterThreadsAfterCutoff([])).toEqual([]);
  });
});
