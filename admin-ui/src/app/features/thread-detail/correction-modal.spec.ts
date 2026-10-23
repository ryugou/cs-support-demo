import { DIALOG_DATA, DialogRef } from '@angular/cdk/dialog';
import { provideHttpClient } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { TestBed } from '@angular/core/testing';
import { environment } from '../../../environments/environment';
import { CorrectionModalComponent, type CorrectionModalData } from './correction-modal';

function setup(data: Partial<CorrectionModalData> = {}) {
  const dialogRef = { close: vi.fn() } as unknown as DialogRef<unknown>;
  const fullData: CorrectionModalData = {
    caseId: 'case-1',
    turnId: 'turn-1',
    question: '返品期限を過ぎたが返品できるか',
    initialSignals: ['signal-a'],
    ...data,
  };
  TestBed.configureTestingModule({
    providers: [
      provideHttpClient(),
      provideHttpClientTesting(),
      { provide: DIALOG_DATA, useValue: fullData },
      { provide: DialogRef, useValue: dialogRef },
    ],
  });
  const fixture = TestBed.createComponent(CorrectionModalComponent);
  fixture.detectChanges();
  const httpMock = TestBed.inject(HttpTestingController);
  return { fixture, dialogRef, httpMock };
}

// Vitest の afterEach で httpMock.verify() を呼びたいが、setup() ごとに TestBed が
// 作り直されるため各テスト内で inject して都度検証する（afterEach は使わない）。
describe('CorrectionModalComponent', () => {
  it('applicability を question で初期化し、signals を initialSignals で初期化する', () => {
    const { fixture } = setup({ question: '初期質問文', initialSignals: ['s1', 's2'] });
    const instance = fixture.componentInstance;
    expect(instance.applicability()).toBe('初期質問文');
    expect(instance.signals()).toEqual(['s1', 's2']);
  });

  it('signals が空のとき送信ボタンが disabled になる', () => {
    const { fixture } = setup({ initialSignals: [] });
    expect(fixture.componentInstance.canSubmit()).toBe(false);
  });

  it('applicability が空文字のとき送信ボタンが disabled になる', () => {
    const { fixture } = setup({ initialSignals: ['s1'] });
    fixture.componentInstance.applicability.set('   ');
    fixture.componentInstance.answer.set('回答本文');
    expect(fixture.componentInstance.canSubmit()).toBe(false);
  });

  it('answer が空文字のとき送信ボタンが disabled になる', () => {
    const { fixture } = setup({ initialSignals: ['s1'] });
    fixture.componentInstance.applicability.set('適用条件');
    fixture.componentInstance.answer.set('');
    expect(fixture.componentInstance.canSubmit()).toBe(false);
  });

  it('signals・applicability・answer が全て埋まっていれば送信ボタンが activate される', () => {
    const { fixture } = setup({ initialSignals: ['s1'] });
    fixture.componentInstance.applicability.set('適用条件');
    fixture.componentInstance.answer.set('回答本文');
    expect(fixture.componentInstance.canSubmit()).toBe(true);
  });

  it('addSignal はテキストを追加して入力欄をクリアし、重複は追加しない', () => {
    const { fixture } = setup({ initialSignals: ['s1'] });
    const instance = fixture.componentInstance;
    instance.newSignalText.set('s2');
    instance.addSignal();
    expect(instance.signals()).toEqual(['s1', 's2']);
    expect(instance.newSignalText()).toBe('');

    // 重複する signal は追加しない
    instance.newSignalText.set('s1');
    instance.addSignal();
    expect(instance.signals()).toEqual(['s1', 's2']);
  });

  it('removeSignal は指定した signal を取り除く', () => {
    const { fixture } = setup({ initialSignals: ['s1', 's2'] });
    fixture.componentInstance.removeSignal('s1');
    expect(fixture.componentInstance.signals()).toEqual(['s2']);
  });

  it('submit は rationale_text を含まないボディで POST /corrections を呼び、成功時に結果付きで dialog を閉じる', () => {
    const { fixture, dialogRef, httpMock } = setup({
      caseId: 'case-9',
      turnId: 'turn-9',
      initialSignals: ['s1'],
    });
    fixture.componentInstance.applicability.set('適用条件');
    fixture.componentInstance.answer.set('訂正後の回答');

    fixture.componentInstance.submit();

    const req = httpMock.expectOne(`${environment.apiBase}/corrections`);
    expect(req.request.method).toBe('POST');
    expect(req.request.body).toEqual({
      case_id: 'case-9',
      turn_id: 'turn-9',
      signals: ['s1'],
      applicability: '適用条件',
      answer: '訂正後の回答',
    });
    expect('rationale_text' in req.request.body).toBe(false);

    const response = { kr_id: 'kr-1', audit_event_id: 'audit-1' };
    req.flush(response);

    expect(dialogRef.close).toHaveBeenCalledWith(response);
    httpMock.verify();
  });

  it('submit が失敗したら dialog を閉じずにエラーメッセージを表示する', () => {
    const { fixture, dialogRef, httpMock } = setup({ initialSignals: ['s1'] });
    fixture.componentInstance.applicability.set('適用条件');
    fixture.componentInstance.answer.set('訂正後の回答');

    fixture.componentInstance.submit();

    const req = httpMock.expectOne(`${environment.apiBase}/corrections`);
    req.flush(
      { error: 'invalid_request', message: 'signals must not be empty' },
      { status: 400, statusText: 'Bad Request' },
    );

    expect(dialogRef.close).not.toHaveBeenCalled();
    expect(fixture.componentInstance.errorMessage()).toBe('signals must not be empty');
    httpMock.verify();
  });

  describe('answer 文字数上限（サーバの chars().count() と一致）', () => {
    function ready(answer: string) {
      const { fixture } = setup({ initialSignals: ['s1'] });
      fixture.componentInstance.applicability.set('適用条件');
      fixture.componentInstance.answer.set(answer);
      return fixture;
    }

    it.each([
      {
        name: 'あ 2,500 文字は送信できる',
        answer: 'あ'.repeat(2_500),
        length: 2_500,
        canSubmit: true,
      },
      {
        name: 'あ 2,501 文字は送信できない',
        answer: 'あ'.repeat(2_501),
        length: 2_501,
        canSubmit: false,
      },
      {
        name: 'サロゲートペアは 1 文字と数え、絵文字 2,500 個は送信できる',
        answer: '😀'.repeat(2_500),
        length: 2_500,
        canSubmit: true,
      },
      {
        name: 'サロゲートペア 2,501 個は送信できない',
        answer: '😀'.repeat(2_501),
        length: 2_501,
        canSubmit: false,
      },
    ])('$name', ({ answer, length, canSubmit }) => {
      const fixture = ready(answer);
      const { componentInstance } = fixture;
      expect(componentInstance.answerLength()).toBe(length);
      expect(componentInstance.canSubmit()).toBe(canSubmit);

      fixture.detectChanges();
      const submitButton = Array.from<HTMLButtonElement>(
        fixture.nativeElement.querySelectorAll('button'),
      ).find((b) => b.textContent?.includes('登録する'));
      expect(submitButton).toBeDefined();
      expect(submitButton?.disabled).toBe(!canSubmit);
    });

    it.each([
      { name: '3 文字では警告色にしない', count: 3, label: '3', overLimit: false },
      { name: '2,501 文字では警告色にする', count: 2_501, label: '2501', overLimit: true },
    ])('上限と現在の文字数を画面に表示する: $name', ({ count, label, overLimit }) => {
      const fixture = ready('あ'.repeat(count));
      fixture.detectChanges();
      const text: string = fixture.nativeElement.textContent;
      expect(text).toContain('2,500 文字以内');
      expect(text).toContain(`${label} / 2,500 文字`);
      const helper = Array.from<HTMLElement>(fixture.nativeElement.querySelectorAll('p')).find(
        (p) => p.textContent?.includes('文字以内'),
      );
      expect(helper).toBeDefined();
      expect(helper?.classList.contains('text-rose-700')).toBe(overLimit);
    });
  });
});
