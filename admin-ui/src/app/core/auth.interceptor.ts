import { HttpErrorResponse, HttpInterceptorFn } from '@angular/common/http';
import { inject } from '@angular/core';
import { catchError, throwError } from 'rxjs';
import { environment } from '../../environments/environment';
import { AuthService } from './auth.service';

/**
 * `environment.apiBase` 宛のリクエストにのみ `Authorization: Bearer <token>` を付与する。
 * 対象外（GIS 自体への疎通など、将来 admin-ui から他ホストを叩くようになった場合）へは
 * トークンを漏らさない。
 *
 * 401 応答は `AuthService.handleUnauthorized()` を呼んでからそのまま再スローする。ここで
 * 握りつぶすと呼び出し元の `catchError`（画面のエラーバナー表示）が動かなくなるため。
 */
export const authInterceptor: HttpInterceptorFn = (req, next) => {
  const auth = inject(AuthService);
  const isAdminApiRequest = req.url.startsWith(environment.apiBase);
  const token = auth.accessToken();
  const authorizedReq =
    isAdminApiRequest && token
      ? req.clone({ setHeaders: { Authorization: `Bearer ${token}` } })
      : req;

  return next(authorizedReq).pipe(
    catchError((error: unknown) => {
      if (isAdminApiRequest && error instanceof HttpErrorResponse && error.status === 401) {
        auth.handleUnauthorized();
      }
      return throwError(() => error);
    }),
  );
};
