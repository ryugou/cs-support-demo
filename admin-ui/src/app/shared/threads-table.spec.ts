import { TestBed } from '@angular/core/testing';
import { ThreadsTableComponent } from './threads-table';
import type { ThreadSummary } from '../core/models';

function makeThread(overrides: Partial<ThreadSummary> = {}): ThreadSummary {
  return {
    case_id: 'case-1',
    case_ref: 'REF-001',
    end_user_id: 'eu-1',
    question_preview: 'エアコンの電源が入らない',
    turn_count: 3,
    last_reply_kind: 'answer',
    last_created_at: '2026-08-16T00:00:00+00:00',
    ...overrides,
  };
}

describe('ThreadsTableComponent', () => {
  it('threads の各行の質問プレビュー・ターン数・end_user_id を描画する', () => {
    const fixture = TestBed.createComponent(ThreadsTableComponent);
    fixture.componentRef.setInput('threads', [makeThread()]);
    fixture.detectChanges();

    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(text).toContain('エアコンの電源が入らない');
    expect(text).toContain('3');
    expect(text).toContain('eu-1');
  });

  it('showEndUserColumn=false のときユーザー列を描画しない', () => {
    const fixture = TestBed.createComponent(ThreadsTableComponent);
    fixture.componentRef.setInput('threads', [makeThread({ end_user_id: 'eu-should-not-render' })]);
    fixture.componentRef.setInput('showEndUserColumn', false);
    fixture.detectChanges();

    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(text).not.toContain('eu-should-not-render');
  });

  it('行クリックで rowClick を case_id 付きで emit する', () => {
    const fixture = TestBed.createComponent(ThreadsTableComponent);
    fixture.componentRef.setInput('threads', [makeThread({ case_id: 'case-42' })]);
    fixture.detectChanges();

    let emitted: string | undefined;
    fixture.componentInstance.rowClick.subscribe((caseId: string) => (emitted = caseId));

    const row = (fixture.nativeElement as HTMLElement).querySelector('[data-testid="thread-row"]');
    (row as HTMLElement).click();

    expect(emitted).toBe('case-42');
  });

  it('ユーザーリンククリックで userClick を emit し、rowClick へ伝播させない', () => {
    const fixture = TestBed.createComponent(ThreadsTableComponent);
    fixture.componentRef.setInput('threads', [
      makeThread({ case_id: 'case-42', end_user_id: 'eu-99' }),
    ]);
    fixture.detectChanges();

    let userClickEmitted: string | undefined;
    let rowClickEmitted: string | undefined;
    fixture.componentInstance.userClick.subscribe((id: string) => (userClickEmitted = id));
    fixture.componentInstance.rowClick.subscribe((id: string) => (rowClickEmitted = id));

    const userLink = (fixture.nativeElement as HTMLElement).querySelector(
      '[data-testid="user-link"]',
    );
    (userLink as HTMLElement).click();

    expect(userClickEmitted).toBe('eu-99');
    expect(rowClickEmitted).toBeUndefined();
  });

  it('end_user_id が null の行はユーザーリンクを描画しない', () => {
    const fixture = TestBed.createComponent(ThreadsTableComponent);
    fixture.componentRef.setInput('threads', [makeThread({ end_user_id: null })]);
    fixture.detectChanges();

    const userLink = (fixture.nativeElement as HTMLElement).querySelector(
      '[data-testid="user-link"]',
    );
    expect(userLink).toBeNull();
  });
});
