import { TestBed } from '@angular/core/testing';
import { provideHttpClient } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { environment } from '../../environments/environment';
import { AdminApiService } from './admin-api.service';
import type { CorrectionRequest, CorrectionResponse, ThreadsPage } from './models';

describe('AdminApiService', () => {
  let service: AdminApiService;
  let httpMock: HttpTestingController;

  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [provideHttpClient(), provideHttpClientTesting()],
    });
    service = TestBed.inject(AdminApiService);
    httpMock = TestBed.inject(HttpTestingController);
  });

  afterEach(() => {
    // 未処理のリクエストが残っていないことを保証する（テストが「発行したこと」だけでなく
    // 「意図しない追加リクエストを発行していないこと」も検証する）。
    httpMock.verify();
  });

  it('listThreads は apiBase/threads へ limit と cursor を query として GET する', () => {
    const page: ThreadsPage = { threads: [], next_cursor: null };

    service.listThreads(20, 'cursor-abc').subscribe((result) => {
      expect(result).toEqual(page);
    });

    const req = httpMock.expectOne(
      (r) => r.url === `${environment.apiBase}/threads` && r.method === 'GET',
    );
    expect(req.request.params.get('limit')).toBe('20');
    expect(req.request.params.get('cursor')).toBe('cursor-abc');
    req.flush(page);
  });

  it('listThreads は limit/cursor 省略時にそれらの query を付けない', () => {
    const page: ThreadsPage = { threads: [], next_cursor: null };

    service.listThreads().subscribe();

    const req = httpMock.expectOne(`${environment.apiBase}/threads`);
    expect(req.request.params.has('limit')).toBe(false);
    expect(req.request.params.has('cursor')).toBe(false);
    req.flush(page);
  });

  it('getThread は apiBase/threads/{case_id} へ GET する', () => {
    service.getThread('case-1').subscribe();

    const req = httpMock.expectOne(`${environment.apiBase}/threads/case-1`);
    expect(req.request.method).toBe('GET');
    req.flush({
      case_id: 'case-1',
      case_ref: 'ref-1',
      turns: [],
      clarify_turns: 0,
      preferred_contact_time: null,
      accumulated_signals: [],
    });
  });

  it('listUserThreads は apiBase/users/{end_user_id}/threads へ GET する', () => {
    service.listUserThreads('eu-1', 10, 'cursor-x').subscribe();

    const req = httpMock.expectOne(
      (r) => r.url === `${environment.apiBase}/users/eu-1/threads` && r.method === 'GET',
    );
    expect(req.request.params.get('limit')).toBe('10');
    expect(req.request.params.get('cursor')).toBe('cursor-x');
    req.flush({ threads: [], next_cursor: null });
  });

  it('createCorrection は apiBase/corrections へ body を POST し、rationale_text を含めない', () => {
    const body: CorrectionRequest = {
      case_id: 'case-1',
      turn_id: 'turn-1',
      signals: ['sig-a'],
      applicability: '対象条件',
      answer: '訂正後の回答',
    };
    const response: CorrectionResponse = { kr_id: 'kr-1', audit_event_id: 'audit-1' };

    service.createCorrection(body).subscribe((result) => {
      expect(result).toEqual(response);
    });

    const req = httpMock.expectOne(`${environment.apiBase}/corrections`);
    expect(req.request.method).toBe('POST');
    expect(req.request.body).toEqual(body);
    expect('rationale_text' in req.request.body).toBe(false);
    req.flush(response);
  });

  it('getStatsSummary は apiBase/stats/summary へ days を query として GET する', () => {
    service.getStatsSummary(1).subscribe();

    const req = httpMock.expectOne(
      (r) => r.url === `${environment.apiBase}/stats/summary` && r.method === 'GET',
    );
    expect(req.request.params.get('days')).toBe('1');
    req.flush({
      days: 1,
      turn_count: 0,
      thread_count: 0,
      reply_kind_counts: {},
      unique_end_user_count: 0,
    });
  });
});
