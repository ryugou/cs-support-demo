import { DatePipe } from '@angular/common';
import { Component, input, output } from '@angular/core';
import type { ThreadSummary } from '../core/models';
import { ReplyKindBadgeComponent } from './reply-kind-badge';

/**
 * スレッド一覧テーブル（design doc §5「一覧画面とユーザー別一覧画面の両方から使う」を満たす
 * 共有コンポーネント）。テーブル markup をここに一元化し、呼び出し側は行データと
 * `showEndUserColumn` の出し分けだけを指定する。
 */
@Component({
  selector: 'app-threads-table',
  imports: [DatePipe, ReplyKindBadgeComponent],
  templateUrl: './threads-table.html',
})
export class ThreadsTableComponent {
  readonly threads = input.required<ThreadSummary[]>();
  readonly showEndUserColumn = input(true);

  readonly rowClick = output<string>();
  readonly userClick = output<string>();

  protected onRowClick(caseId: string): void {
    this.rowClick.emit(caseId);
  }

  protected onUserClick(event: Event, endUserId: string): void {
    // 行クリック（詳細遷移）とユーザーリンク（ユーザー別一覧へ遷移）は別の遷移先なので、
    // ユーザーリンクのクリックが行クリックへ二重発火しないようにする（spec 指定どおり）。
    event.stopPropagation();
    this.userClick.emit(endUserId);
  }
}
