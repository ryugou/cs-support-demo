import { Injectable, signal } from '@angular/core';
import { environment } from '../../environments/environment';

/** sessionStorage のキー。`localStorage` を使わないのは、タブを閉じたらトークンが確実に
 * 消えるようにするため（design doc §5、spec の指示どおり）。 */
const STORAGE_KEY = 'cs_support_admin_google_access_token';
/** GIS スクリプト（`index.html` の `<script async defer>`）の読み込み完了を待つポーリング間隔。 */
const GIS_POLL_INTERVAL_MS = 100;
/** GIS スクリプトが読み込まれない場合のタイムアウト。ネットワーク不調・広告ブロッカー等で
 * 読み込みが永久に終わらないケースをハングさせず、運用者が原因を特定できるメッセージに変える。 */
const GIS_LOAD_TIMEOUT_MS = 10_000;

interface GoogleTokenResponse {
  access_token?: string;
  error?: string;
  error_description?: string;
}

interface GoogleTokenClient {
  requestAccessToken: () => void;
}

interface GoogleAccountsNamespace {
  accounts: {
    oauth2: {
      initTokenClient: (config: {
        client_id: string;
        scope: string;
        callback: (response: GoogleTokenResponse) => void;
      }) => GoogleTokenClient;
    };
  };
}

declare global {
  interface Window {
    google?: GoogleAccountsNamespace;
  }
}

/**
 * Google Identity Services（OAuth2 token model）でブラウザから直接 access token を取得し、
 * `/admin/api` 呼び出し用に保持する。
 *
 * サーバ側の認証コード（`GoogleTokenVerifier`）には一切触れない。ここが持つのは
 * 「トークンを取得してヘッダに載せる」役割のみで、トークンの正当性判定はサーバに委ねる
 * （design doc §5: 「サーバの認証コードに変更を加えない」）。
 */
@Injectable({ providedIn: 'root' })
export class AuthService {
  // 書き込みはこのサービス内部（signIn/signOut/トークン応答処理）に閉じる。外部（コンポーネント）
  // からは読み取り専用の `.asReadonly()` のみを公開し、`auth.accessToken.set(...)` のような
  // GIS フローを経ないトークン変更を型レベルで禁止する。
  private readonly _accessToken = signal<string | null>(this.restoreToken());
  private readonly _signInError = signal<string | null>(null);

  /** 現在有効な Google access token。null は未ログイン。`AuthInterceptor` と
   * `App`（ログイン画面/シェルの出し分け）の両方がこの signal を参照する。 */
  readonly accessToken = this._accessToken.asReadonly();
  /** 直近の signIn 試行で発生したエラーメッセージ(ログイン画面のバナー表示用)。 */
  readonly signInError = this._signInError.asReadonly();

  private tokenClientPromise: Promise<GoogleTokenClient> | null = null;

  /** ログインボタンから呼ぶ。GIS の読み込み待ち → token client 初期化 → 同意/選択 UI 表示、の
   * 一連の非同期処理をここに閉じ込める。失敗時は例外を投げず `signInError` に格納する
   * （呼び出し元のテンプレートは signal を読むだけでよい）。 */
  async signIn(): Promise<void> {
    this._signInError.set(null);
    try {
      const client = await this.ensureTokenClient();
      client.requestAccessToken();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      console.error('[AuthService] signIn failed', err);
      this._signInError.set(message);
    }
  }

  /** トークンを破棄する。GIS 側のセッション（Google アカウント自体のログイン状態）には
   * 触れない。再度 `signIn()` すれば取り直せる。 */
  signOut(): void {
    this._accessToken.set(null);
    this.removeStoredToken();
  }

  /** 管理 API から 401 を受けたときに `AuthInterceptor` から呼ばれる。現状は `signOut()` と
   * 同じだが、呼び出し意図（「サーバがトークンを拒否した」）を型で区別できるよう別名にしておく。 */
  handleUnauthorized(): void {
    this.signOut();
  }

  private restoreToken(): string | null {
    try {
      return sessionStorage.getItem(STORAGE_KEY);
    } catch (err) {
      // プライベートブラウジング等で sessionStorage が使えない環境でも起動は継続する
      // （その場合はリロードのたびに再ログインが必要になるだけで、機能停止にはしない）。
      console.warn('[AuthService] sessionStorage unavailable, falling back to in-memory token', err);
      return null;
    }
  }

  private removeStoredToken(): void {
    try {
      sessionStorage.removeItem(STORAGE_KEY);
    } catch {
      // 読み取りと対称に無視してよい（保存できていない = 削除も不要）。
    }
  }

  private onTokenResponse(response: GoogleTokenResponse): void {
    if (response.error || !response.access_token) {
      const message = response.error_description ?? response.error ?? 'Google からアクセストークンを取得できませんでした';
      console.error('[AuthService] Google token response error', response);
      this._signInError.set(message);
      return;
    }
    this._accessToken.set(response.access_token);
    try {
      sessionStorage.setItem(STORAGE_KEY, response.access_token);
    } catch (err) {
      // 保存できなくても取得済みトークンでこのタブは動作継続する（リロードで再ログインが
      // 必要になるだけ）。
      console.warn('[AuthService] failed to persist token to sessionStorage', err);
    }
  }

  private ensureTokenClient(): Promise<GoogleTokenClient> {
    if (!this.tokenClientPromise) {
      this.tokenClientPromise = this.waitForGis().then((google) =>
        google.accounts.oauth2.initTokenClient({
          client_id: environment.googleClientId,
          scope: 'openid email profile',
          callback: (response) => this.onTokenResponse(response),
        }),
      );
    }
    return this.tokenClientPromise;
  }

  private waitForGis(): Promise<GoogleAccountsNamespace> {
    return new Promise((resolve, reject) => {
      const start = Date.now();
      const poll = () => {
        if (window.google?.accounts?.oauth2) {
          resolve(window.google);
          return;
        }
        if (Date.now() - start > GIS_LOAD_TIMEOUT_MS) {
          reject(
            new Error(
              'Google Identity Services の読み込みがタイムアウトしました。ネットワーク接続または広告ブロッカーの設定を確認してください。',
            ),
          );
          return;
        }
        setTimeout(poll, GIS_POLL_INTERVAL_MS);
      };
      poll();
    });
  }
}
