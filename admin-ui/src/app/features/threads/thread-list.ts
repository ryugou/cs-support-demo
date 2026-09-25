import { KeyValuePipe } from '@angular/common';
import { Component, OnInit, inject, signal } from '@angular/core';
import { Router } from '@angular/router';
import { AdminApiService } from '../../core/admin-api.service';
import type { StatsSummary, ThreadSummary } from '../../core/models';
import { extractErrorMessage } from '../../shared/http-error';
import { replyKindLabel } from '../../shared/reply-kind';
import { StatTileComponent } from '../../shared/stat-tile';
import { filterThreadsAfterCutoff } from '../../shared/thread-cutoff';
import { ThreadsTableComponent } from '../../shared/threads-table';

/** `GET /threads` の既定 limit（design doc §5「limit=20 で GET /threads 呼び出し」）。
 * サーバ側の既定値（`THREADS_DEFAULT_LIMIT`）と同じ値をクライアント側でも明示する。 */
const THREADS_PAGE_LIMIT = 20;

/** 利用状況サマリの期間切り替え（design doc §5「本日/7日のターン数」）。 */
type StatsRange = 1 | 7;

/**
 * スレッド一覧画面。上部に利用状況サマリ（本日/7日間トグル）、下にスレッドテーブル
 * （前方のみのカーソルページング）を表示する。
 */
@Component({
  selector: 'app-thread-list',
  imports: [KeyValuePipe, StatTileComponent, ThreadsTableComponent],
  templateUrl: './thread-list.html',
})
export class ThreadListComponent implements OnInit {
  private readonly api = inject(AdminApiService);
  private readonly router = inject(Router);

  protected readonly statsDays = signal<StatsRange>(7);
  protected readonly stats = signal<StatsSummary | null>(null);
  protected readonly statsError = signal<string | null>(null);
  protected readonly statsLoading = signal(false);

  protected readonly threads = signal<ThreadSummary[]>([]);
  protected readonly nextCursor = signal<string | null>(null);
  protected readonly threadsError = signal<string | null>(null);
  protected readonly threadsLoading = signal(false);
  /** 「最初から」ボタンの活性判定用。先頭ページを表示中は無効化する。 */
  protected readonly atFirstPage = signal(true);

  protected readonly replyKindLabel = replyKindLabel;

  ngOnInit(): void {
    this.loadStats();
    this.loadThreads(null);
  }

  protected setStatsRange(days: StatsRange): void {
    if (this.statsDays() === days) {
      return;
    }
    this.statsDays.set(days);
    this.loadStats();
  }

  protected goNext(): void {
    const cursor = this.nextCursor();
    if (cursor) {
      this.loadThreads(cursor);
    }
  }

  protected goFirst(): void {
    this.loadThreads(null);
  }

  protected onRowClick(caseId: string): void {
    void this.router.navigate(['/threads', caseId]);
  }

  protected onUserClick(endUserId: string): void {
    void this.router.navigate(['/users', endUserId, 'threads']);
  }

  private loadStats(): void {
    this.statsLoading.set(true);
    this.statsError.set(null);
    this.api.getStatsSummary(this.statsDays()).subscribe({
      next: (summary) => {
        this.stats.set(summary);
        this.statsLoading.set(false);
      },
      error: (err: unknown) => {
        this.statsError.set(extractErrorMessage(err, '利用状況の取得に失敗しました'));
        this.statsLoading.set(false);
      },
    });
  }

  private loadThreads(cursor: string | null): void {
    this.threadsLoading.set(true);
    this.threadsError.set(null);
    this.api.listThreads(THREADS_PAGE_LIMIT, cursor).subscribe({
      next: (page) => {
        this.threads.set(filterThreadsAfterCutoff(page.threads));
        this.nextCursor.set(page.next_cursor);
        this.atFirstPage.set(cursor === null);
        this.threadsLoading.set(false);
      },
      error: (err: unknown) => {
        this.threadsError.set(extractErrorMessage(err, 'スレッド一覧の取得に失敗しました'));
        this.threadsLoading.set(false);
      },
    });
  }
}
