import { HttpClient, provideHttpClient, withInterceptors } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { TestBed } from '@angular/core/testing';
import { environment } from '../../environments/environment';
import { authInterceptor } from './auth.interceptor';
import { AuthService } from './auth.service';

/**
 * `AuthService` の sessionStorage キーと同じ値（`auth.service.ts` 参照）。
 * `AuthService` はコンストラクタで sessionStorage を読んで初期状態を作るため、各テストは
 * `TestBed.configureTestingModule` を呼ぶ**前**にこのキーへ書き込む（`setup()` の中で
 * `TestBed.inject` が初めて `AuthService` をインスタンス化するタイミングに合わせる）。
 */
const STORAGE_KEY = 'cs_support_admin_google_access_token';

describe('authInterceptor', () => {
  let httpClient: HttpClient;
  let httpMock: HttpTestingController;

  function setup(): void {
    TestBed.configureTestingModule({
      providers: [provideHttpClient(withInterceptors([authInterceptor])), provideHttpClientTesting()],
    });
    httpClient = TestBed.inject(HttpClient);
    httpMock = TestBed.inject(HttpTestingController);
  }

  afterEach(() => {
    // 各テストが発行したリクエストを全て消化したこと（意図しない追加リクエストが無いこと）を
    // 検証してから、次のテストへ sessionStorage の状態を持ち越さない。
    httpMock.verify();
    sessionStorage.removeItem(STORAGE_KEY);
  });

  it('environment.apiBase 配下へのリクエストには、保持中のトークンが Authorization: Bearer として付く', () => {
    sessionStorage.setItem(STORAGE_KEY, 'stored-token');
    setup();

    httpClient.get(`${environment.apiBase}/threads`).subscribe();

    const req = httpMock.expectOne(`${environment.apiBase}/threads`);
    expect(req.request.headers.get('Authorization')).toBe('Bearer stored-token');
    req.flush({});
  });

  it('environment.apiBase 以外の URL には Authorization ヘッダを付けない（トークン漏出防止の不変条件）', () => {
    sessionStorage.setItem(STORAGE_KEY, 'stored-token');
    setup();

    httpClient.get('https://accounts.google.com/o/oauth2/v2/auth').subscribe();
    httpClient.get('/other/path').subscribe();

    const googleReq = httpMock.expectOne('https://accounts.google.com/o/oauth2/v2/auth');
    expect(googleReq.request.headers.has('Authorization')).toBe(false);
    googleReq.flush({});

    const otherReq = httpMock.expectOne('/other/path');
    expect(otherReq.request.headers.has('Authorization')).toBe(false);
    otherReq.flush({});
  });

  it('トークン未保持のときは apiBase 宛のリクエストにも Authorization を付けない', () => {
    sessionStorage.removeItem(STORAGE_KEY);
    setup();

    httpClient.get(`${environment.apiBase}/threads`).subscribe();

    const req = httpMock.expectOne(`${environment.apiBase}/threads`);
    expect(req.request.headers.has('Authorization')).toBe(false);
    req.flush({});
  });

  it('管理 API (apiBase) が 401 を返すと AuthService のトークンが破棄され、エラーは購読側に届く', () => {
    sessionStorage.setItem(STORAGE_KEY, 'stored-token');
    setup();
    const auth = TestBed.inject(AuthService);
    expect(auth.accessToken()).toBe('stored-token');

    let receivedError: unknown;
    httpClient.get(`${environment.apiBase}/threads`).subscribe({
      next: () => {
        throw new Error('401 response must not resolve as a successful next()');
      },
      error: (err) => {
        receivedError = err;
      },
    });

    const req = httpMock.expectOne(`${environment.apiBase}/threads`);
    req.flush({ error: 'unauthorized' }, { status: 401, statusText: 'Unauthorized' });

    expect(auth.accessToken()).toBeNull();
    expect(receivedError).toBeTruthy();
  });

  it('管理 API 以外が 401 を返してもトークンは破棄されない', () => {
    sessionStorage.setItem(STORAGE_KEY, 'stored-token');
    setup();
    const auth = TestBed.inject(AuthService);

    httpClient.get('/other/path').subscribe({
      next: () => {
        throw new Error('401 response must not resolve as a successful next()');
      },
      error: () => {
        // このテストの関心はトークンが破棄されないことなので、エラー自体の中身は見ない。
      },
    });

    const req = httpMock.expectOne('/other/path');
    req.flush({ error: 'unauthorized' }, { status: 401, statusText: 'Unauthorized' });

    expect(auth.accessToken()).toBe('stored-token');
  });
});
