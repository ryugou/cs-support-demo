import { HttpClient, HttpParams } from '@angular/common/http';
import { Injectable, inject } from '@angular/core';
import type { Observable } from 'rxjs';
import { environment } from '../../environments/environment';
import type {
  CorrectionRequest,
  CorrectionResponse,
  StatsSummary,
  ThreadDetail,
  ThreadsPage,
} from './models';

/**
 * `/{project_id}/admin/api/*` の薄いラッパー。
 *
 * `environment.apiBase` は `AuthInterceptor` が Authorization ヘッダを付与するかどうかの
 * 判定にも使う（このサービスの URL 組み立てと interceptor の対象判定が食い違うと、
 * 管理 API 呼び出しが無トークンで飛んで 401 になる。両者は同じ `environment.apiBase` を
 * source of truth として共有する）。
 */
@Injectable({ providedIn: 'root' })
export class AdminApiService {
  private readonly http = inject(HttpClient);
  private readonly base = environment.apiBase;

  listThreads(limit?: number, cursor?: string | null): Observable<ThreadsPage> {
    return this.http.get<ThreadsPage>(`${this.base}/threads`, {
      params: this.buildThreadsParams(limit, cursor),
    });
  }

  getThread(caseId: string): Observable<ThreadDetail> {
    return this.http.get<ThreadDetail>(`${this.base}/threads/${encodeURIComponent(caseId)}`);
  }

  listUserThreads(
    endUserId: string,
    limit?: number,
    cursor?: string | null,
  ): Observable<ThreadsPage> {
    return this.http.get<ThreadsPage>(
      `${this.base}/users/${encodeURIComponent(endUserId)}/threads`,
      { params: this.buildThreadsParams(limit, cursor) },
    );
  }

  createCorrection(request: CorrectionRequest): Observable<CorrectionResponse> {
    return this.http.post<CorrectionResponse>(`${this.base}/corrections`, request);
  }

  getStatsSummary(days?: number): Observable<StatsSummary> {
    let params = new HttpParams();
    if (days !== undefined) {
      params = params.set('days', days);
    }
    return this.http.get<StatsSummary>(`${this.base}/stats/summary`, { params });
  }

  private buildThreadsParams(limit?: number, cursor?: string | null): HttpParams {
    let params = new HttpParams();
    if (limit !== undefined) {
      params = params.set('limit', limit);
    }
    if (cursor) {
      params = params.set('cursor', cursor);
    }
    return params;
  }
}
