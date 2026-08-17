import { Component, computed, input } from '@angular/core';
import type { ReplyKind } from '../core/models';
import { replyKindBadgeClass, replyKindLabel } from './reply-kind';

/** `reply_kind` を Stripe Dashboard 風の状態バッジ（薄い背景 + 濃い文字色、角丸小さめ）で表示する。 */
@Component({
  selector: 'app-reply-kind-badge',
  template: `
    <span
      class="inline-flex items-center rounded px-2 py-0.5 text-xs font-medium"
      [class]="badgeClass()"
    >
      {{ label() }}
    </span>
  `,
})
export class ReplyKindBadgeComponent {
  readonly kind = input.required<ReplyKind>();

  protected readonly label = computed(() => replyKindLabel(this.kind()));
  protected readonly badgeClass = computed(() => replyKindBadgeClass(this.kind()));
}
