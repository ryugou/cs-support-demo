import { HttpErrorResponse } from '@angular/common/http';
import type { ErrorBody } from '../core/models';

/**
 * 管理 API 呼び出しの失敗を、画面のインラインエラーバナーに出せる日本語メッセージへ変換する。
 * サーバのエラー形式（`ErrorBody`）が返っていればその `message` を使い、無ければ HTTP
 * ステータス、それも取れない場合（ネットワーク断など）は `err.error instanceof ProgressEvent`
 * のケースとして扱う。
 *
 * 想定外の型（`HttpErrorResponse` ですらない値）は握りつぶさず `console.error` に残す
 * （本番でここに来るのは実装ミスの可能性が高く、ブラウザの devtools だけが手がかりになる）。
 */
export function extractErrorMessage(err: unknown, fallbackPrefix: string): string {
  if (err instanceof HttpErrorResponse) {
    const body = err.error as ErrorBody | undefined;
    if (body?.message) {
      return body.message;
    }
    if (err.status === 0) {
      return `${fallbackPrefix}。ネットワーク接続を確認してください。`;
    }
    return `${fallbackPrefix}（HTTP ${err.status}）`;
  }
  console.error('[extractErrorMessage] unexpected non-HTTP error', err);
  return `${fallbackPrefix}。ネットワーク接続を確認してください。`;
}
