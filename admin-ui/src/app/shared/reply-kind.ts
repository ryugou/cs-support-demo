import type { ReplyKind } from '../core/models';

interface ReplyKindStyle {
  label: string;
  /** Stripe Dashboard 風の状態バッジ（薄い背景色 + 濃い文字色）。プライマリカラーは
   * 意味を持つ箇所（アクション・アクティブ状態）に予約するため、バッジの状態色には使わない。 */
  badgeClass: string;
}

/** design doc §4 / CLAUDE.md の6値の日本語ラベル対応表。ラベルの和訳自体はこのタスクの
 * 裁量事項（オーケストレーターの spec に例示あり）。 */
const REPLY_KIND_STYLES: Record<string, ReplyKindStyle> = {
  answer: { label: '回答', badgeClass: 'bg-emerald-50 text-emerald-700' },
  clarify: { label: '聞き返し', badgeClass: 'bg-sky-50 text-sky-700' },
  escalation: { label: 'エスカレーション', badgeClass: 'bg-amber-50 text-amber-800' },
  out_of_scope: { label: '対象外', badgeClass: 'bg-slate-100 text-slate-600' },
  time_pref: { label: '時間帯希望', badgeClass: 'bg-violet-50 text-violet-700' },
  fallback: { label: 'フォールバック', badgeClass: 'bg-rose-50 text-rose-700' },
};

const UNKNOWN_BADGE_CLASS = 'bg-slate-100 text-slate-600';

/**
 * `reply_kind` の日本語表示ラベルを返す。サーバは将来値を追加しうる自由文字列なので、
 * 未知の値は生の文字列をそのまま表示する（空表示や例外にしない）。
 */
export function replyKindLabel(kind: ReplyKind): string {
  return REPLY_KIND_STYLES[kind]?.label ?? kind;
}

export function replyKindBadgeClass(kind: ReplyKind): string {
  return REPLY_KIND_STYLES[kind]?.badgeClass ?? UNKNOWN_BADGE_CLASS;
}
