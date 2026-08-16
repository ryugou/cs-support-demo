import { Component, inject } from '@angular/core';
import { RouterLink, RouterLinkActive } from '@angular/router';
import { AuthService } from '../../core/auth.service';

/** 共通レイアウト: 左サイドバー（ナビゲーション + サインアウト）+ メインコンテンツ領域
 * （`<ng-content>`）。ルーティングされた画面は `App` ルートテンプレートが
 * `<app-shell><router-outlet /></app-shell>` の形で投影する（spec 指定の構成）。 */
@Component({
  selector: 'app-shell',
  imports: [RouterLink, RouterLinkActive],
  templateUrl: './app-shell.html',
})
export class AppShellComponent {
  protected readonly auth = inject(AuthService);

  protected onSignOut(): void {
    this.auth.signOut();
  }
}
