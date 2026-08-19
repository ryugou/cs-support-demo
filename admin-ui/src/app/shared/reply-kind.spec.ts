import { replyKindBadgeClass, replyKindLabel } from './reply-kind';

describe('replyKindLabel / replyKindBadgeClass', () => {
  it('CS の既存6値のラベル・バッジクラスを返す', () => {
    expect(replyKindLabel('answer')).toBe('回答');
    expect(replyKindLabel('escalation')).toBe('エスカレーション');
    expect(replyKindBadgeClass('answer')).toBe('bg-emerald-50 text-emerald-700');
  });

  it('advisor 固有4値(handoff/safety/out_of_domain/lead)のラベルを返す', () => {
    expect(replyKindLabel('handoff')).toBe('個別サポート案内');
    expect(replyKindLabel('safety')).toBe('緊急案内');
    expect(replyKindLabel('out_of_domain')).toBe('対象外');
    expect(replyKindLabel('lead')).toBe('リード確定');
  });

  it('advisor 固有4値のバッジクラスを返す', () => {
    expect(replyKindBadgeClass('handoff')).toBe('bg-indigo-50 text-indigo-700');
    expect(replyKindBadgeClass('safety')).toBe('bg-red-50 text-red-700');
    expect(replyKindBadgeClass('out_of_domain')).toBe('bg-slate-100 text-slate-600');
    expect(replyKindBadgeClass('lead')).toBe('bg-orange-50 text-orange-700');
  });

  it('CS の out_of_scope と advisor の out_of_domain は同じ表示(対象外・同一バッジ色)になる', () => {
    // design doc §3.3 は CS の out_of_scope と advisor の out_of_domain を別の reply_kind 値
    // として定義しているが、管理画面での見え方は同じ「対象外」でよいという spec の判断
    // (オーケストレーターの spec に明記)を固定する。
    expect(replyKindLabel('out_of_domain')).toBe(replyKindLabel('out_of_scope'));
    expect(replyKindBadgeClass('out_of_domain')).toBe(replyKindBadgeClass('out_of_scope'));
  });

  it('未知の reply_kind は生の文字列をラベルとして返し、既定バッジクラスにフォールバックする', () => {
    expect(replyKindLabel('some_future_kind')).toBe('some_future_kind');
    expect(replyKindBadgeClass('some_future_kind')).toBe('bg-slate-100 text-slate-600');
  });
});
