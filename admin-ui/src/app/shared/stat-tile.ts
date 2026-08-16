import { Component, input } from '@angular/core';

/** スレッド一覧の利用状況サマリで使う、値 1 件を見せる小さなタイル（Stripe Dashboard 風の
 * stat カード）。 */
@Component({
  selector: 'app-stat-tile',
  template: `
    <div class="rounded-lg border border-slate-200 bg-white p-4">
      <div class="text-xs font-medium uppercase tracking-wide text-slate-500">{{ label() }}</div>
      <div class="mt-1 text-2xl font-semibold text-slate-900">{{ value() }}</div>
    </div>
  `,
})
export class StatTileComponent {
  readonly label = input.required<string>();
  readonly value = input.required<string | number>();
}
