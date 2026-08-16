import { Routes } from '@angular/router';

/**
 * design doc §5 の画面3枚 + 一覧へのリダイレクト。各画面は `loadComponent` で遅延読み込みする
 * （SPA 初回表示のバンドルサイズを抑える。管理画面規模ではオーバーエンジニアリングにならない
 * 程度の粒度）。ルートパラメータは `app.config.ts` の `withComponentInputBinding()` により
 * 各コンポーネントの `input()` へ直接バインドされる。
 */
export const routes: Routes = [
  { path: '', pathMatch: 'full', redirectTo: 'threads' },
  {
    path: 'threads',
    loadComponent: () =>
      import('./features/threads/thread-list').then((m) => m.ThreadListComponent),
  },
  {
    path: 'threads/:caseId',
    loadComponent: () =>
      import('./features/thread-detail/thread-detail').then((m) => m.ThreadDetailComponent),
  },
  {
    path: 'users/:endUserId/threads',
    loadComponent: () =>
      import('./features/user-threads/user-threads').then((m) => m.UserThreadsComponent),
  },
];
