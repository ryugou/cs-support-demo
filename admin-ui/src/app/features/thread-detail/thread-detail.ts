import { Dialog } from '@angular/cdk/dialog';
import { DatePipe } from '@angular/common';
import { Component, effect, inject, input, signal } from '@angular/core';
import { AdminApiService } from '../../core/admin-api.service';
import type { CorrectionResponse, ThreadDetail, TurnView } from '../../core/models';
import { extractErrorMessage } from '../../shared/http-error';
import { ReplyKindBadgeComponent } from '../../shared/reply-kind-badge';
import { CorrectionModalComponent, type CorrectionModalData } from './correction-modal';

/**
 * スレッド詳細画面。ルートパラメータ `caseId` は `withComponentInputBinding()` により
 * `input()` へ直接バインドされる。チャット風に各ターンを表示し、システム応答ごとに
 * 「訂正を登録」ボタンから `CorrectionModalComponent`（CDK Dialog）を開く。
 *
 * `registeredCorrections`（turn_id -> kr_id）はこのコンポーネントのメモリ上 state。
 * リロードすれば消える（spec 指定: 「リロードで消えてよい」）。サーバ側に「訂正済み
 * フラグ」は無いため、正としての永続化はここでは行わない。
 */
@Component({
  selector: 'app-thread-detail',
  imports: [DatePipe, ReplyKindBadgeComponent],
  templateUrl: './thread-detail.html',
})
export class ThreadDetailComponent {
  private readonly api = inject(AdminApiService);
  private readonly dialog = inject(Dialog);

  readonly caseId = input.required<string>();

  protected readonly detail = signal<ThreadDetail | null>(null);
  protected readonly error = signal<string | null>(null);
  protected readonly loading = signal(false);
  protected readonly registeredCorrections = signal<ReadonlyMap<string, string>>(new Map());

  constructor() {
    effect(() => {
      const caseId = this.caseId();
      this.loadThread(caseId);
    });
  }

  protected openCorrectionModal(turn: TurnView): void {
    const detail = this.detail();
    if (!detail) {
      return;
    }
    const data: CorrectionModalData = {
      caseId: detail.case_id,
      turnId: turn.turn_id,
      question: turn.question,
      initialSignals: [...detail.accumulated_signals],
    };
    const dialogRef = this.dialog.open<CorrectionResponse, CorrectionModalData>(
      CorrectionModalComponent,
      { data },
    );
    dialogRef.closed.subscribe((result) => {
      if (!result) {
        return;
      }
      this.registeredCorrections.update((current) => {
        const next = new Map(current);
        next.set(turn.turn_id, result.kr_id);
        return next;
      });
    });
  }

  private loadThread(caseId: string): void {
    this.loading.set(true);
    this.error.set(null);
    this.registeredCorrections.set(new Map());
    this.api.getThread(caseId).subscribe({
      next: (detail) => {
        this.detail.set(detail);
        this.loading.set(false);
      },
      error: (err: unknown) => {
        this.error.set(extractErrorMessage(err, 'スレッドの取得に失敗しました'));
        this.loading.set(false);
      },
    });
  }
}
