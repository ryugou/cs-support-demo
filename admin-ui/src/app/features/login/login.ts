import { Component, inject } from '@angular/core';
import { AuthService } from '../../core/auth.service';

/** 未ログイン時に `App` ルートテンプレートが表示するログイン画面。ガード＋リダイレクトの
 * 往復は作らず、ルートコンポーネントの `@if` で出し分けるだけのシンプルな構成にする
 * （spec 指定どおり）。 */
@Component({
  selector: 'app-login',
  templateUrl: './login.html',
})
export class LoginComponent {
  protected readonly auth = inject(AuthService);

  protected onSignIn(): void {
    void this.auth.signIn();
  }
}
