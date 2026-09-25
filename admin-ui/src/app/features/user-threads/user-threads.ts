import { Component, effect, inject, input, signal } from '@angular/core';
import { Router } from '@angular/router';
import { AdminApiService } from '../../core/admin-api.service';
import type { ThreadSummary } from '../../core/models';
import { extractErrorMessage } from '../../shared/http-error';
import { filterThreadsAfterCutoff } from '../../shared/thread-cutoff';
import { ThreadsTableComponent } from '../../shared/threads-table';

/** `GET /users/{end_user_id}/threads` の既定 limit（`ThreadListComponent` と同じ値）。 */
const THREADS_PAGE_LIMIT = 20;

/**
 * ユーザー別スレッド一覧画面。ルートパラメータ `endUserId` は `withComponentInputBinding()`
 * により直接 `input()` へバインドされる。
 *
 * ルート再利用（同じルート定義のまま `endUserId` だけが変わるナビゲーション）でも
 * コンポーネントインスタンスは再生成されないため、`ngOnInit` ではなく `effect()` で
 * `endUserId()` の変化そのものを検知して再取得する。
 */
@Component({
  selector: 'app-user-threads',
  imports: [ThreadsTableComponent],
  templateUrl: './user-threads.html',
})
export class UserThreadsComponent {
  private readonly api = inject(AdminApiService);
  private readonly router = inject(Router);

  readonly endUserId = input.required<string>();

  protected readonly threads = signal<ThreadSummary[]>([]);
  protected readonly nextCursor = signal<string | null>(null);
  protected readonly error = signal<string | null>(null);
  protected readonly loading = signal(false);
  protected readonly atFirstPage = signal(true);

  constructor() {
    effect(() => {
      const endUserId = this.endUserId();
      this.loadThreads(endUserId, null);
    });
  }

  protected goNext(): void {
    const cursor = this.nextCursor();
    if (cursor) {
      this.loadThreads(this.endUserId(), cursor);
    }
  }

  protected goFirst(): void {
    this.loadThreads(this.endUserId(), null);
  }

  protected onRowClick(caseId: string): void {
    void this.router.navigate(['/threads', caseId]);
  }

  private loadThreads(endUserId: string, cursor: string | null): void {
    this.loading.set(true);
    this.error.set(null);
    this.api.listUserThreads(endUserId, THREADS_PAGE_LIMIT, cursor).subscribe({
      next: (page) => {
        this.threads.set(filterThreadsAfterCutoff(page.threads));
        this.nextCursor.set(page.next_cursor);
        this.atFirstPage.set(cursor === null);
        this.loading.set(false);
      },
      error: (err: unknown) => {
        this.error.set(extractErrorMessage(err, 'スレッド一覧の取得に失敗しました'));
        this.loading.set(false);
      },
    });
  }
}
