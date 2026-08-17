import { Component, inject } from '@angular/core';
import { RouterOutlet } from '@angular/router';
import { AuthService } from './core/auth.service';
import { LoginComponent } from './features/login/login';
import { AppShellComponent } from './features/shell/app-shell';

/**
 * ルートコンポーネント。ルーティングガード + リダイレクトの往復は作らず、
 * `auth.accessToken()` の有無だけでログイン画面/シェルを出し分ける（spec 指定のシンプルな構成）。
 */
@Component({
  selector: 'app-root',
  imports: [RouterOutlet, AppShellComponent, LoginComponent],
  templateUrl: './app.html',
})
export class App {
  protected readonly auth = inject(AuthService);
}
