import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting } from '@angular/common/http/testing';
import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { App } from './app';
import { routes } from './app.routes';

/** `AuthService` の sessionStorage キーと同じ値（`core/auth.service.ts` 参照）。 */
const STORAGE_KEY = 'cs_support_admin_google_access_token';

describe('App', () => {
  afterEach(() => {
    sessionStorage.removeItem(STORAGE_KEY);
  });

  it('未ログイン時はログイン画面(app-login)を表示し、シェルは表示しない', async () => {
    sessionStorage.removeItem(STORAGE_KEY);
    await TestBed.configureTestingModule({
      imports: [App],
      providers: [provideRouter(routes), provideHttpClient(), provideHttpClientTesting()],
    }).compileComponents();

    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('app-login')).toBeTruthy();
    expect(compiled.querySelector('app-shell')).toBeNull();
  });

  it('ログイン済み(sessionStorage にトークンあり)のときはシェル(app-shell)を表示する', async () => {
    sessionStorage.setItem(STORAGE_KEY, 'test-token');
    await TestBed.configureTestingModule({
      imports: [App],
      providers: [provideRouter(routes), provideHttpClient(), provideHttpClientTesting()],
    }).compileComponents();

    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('app-shell')).toBeTruthy();
    expect(compiled.querySelector('app-login')).toBeNull();
  });
});
