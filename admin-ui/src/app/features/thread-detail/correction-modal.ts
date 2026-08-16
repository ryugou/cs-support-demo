import { DIALOG_DATA, DialogRef } from '@angular/cdk/dialog';
import { Component, computed, inject, signal } from '@angular/core';
import { FormsModule } from '@angular/forms';
import { AdminApiService } from '../../core/admin-api.service';
import type { CorrectionResponse } from '../../core/models';
import { extractErrorMessage } from '../../shared/http-error';

/** `CorrectionModalComponent` を開く際に渡すデータ（design doc §5「訂正登録モーダル」）。 */
export interface CorrectionModalData {
  caseId: string;
  turnId: string;
  /** そのターンの顧客質問文。`applicability` の初期値として使う（operator が編集する前提）。 */
  question: string;
  /** そのスレッドの累積 signal。チップ入力の初期値。 */
  initialSignals: string[];
}

/**
 * 訂正登録モーダル（CDK Dialog content）。`POST /admin/api/corrections` を叩き、既存の
 * `add_known_resolution` と同じ harness 入口にノウハウを登録する。
 *
 * 成功時は `DialogRef.close(response)` で呼び出し元へ結果を渡す（呼び出し元がそれを見て
 * 「登録済み」状態にする、spec 指定の契約）。失敗時はモーダルを閉じずエラーを表示する
 * （送信し直せるように、入力内容を保持したままにする）。
 */
@Component({
  selector: 'app-correction-modal',
  imports: [FormsModule],
  templateUrl: './correction-modal.html',
})
export class CorrectionModalComponent {
  private readonly api = inject(AdminApiService);
  protected readonly dialogRef = inject(DialogRef<CorrectionResponse>);
  private readonly data = inject<CorrectionModalData>(DIALOG_DATA);

  protected readonly question = this.data.question;

  readonly signals = signal<string[]>([...this.data.initialSignals]);
  readonly applicability = signal(this.data.question);
  readonly answer = signal('');
  readonly newSignalText = signal('');
  readonly submitting = signal(false);
  readonly errorMessage = signal<string | null>(null);

  readonly canSubmit = computed(
    () =>
      this.signals().length > 0 &&
      this.applicability().trim().length > 0 &&
      this.answer().trim().length > 0 &&
      !this.submitting(),
  );

  addSignal(): void {
    const text = this.newSignalText().trim();
    if (!text) {
      return;
    }
    if (!this.signals().includes(text)) {
      this.signals.update((current) => [...current, text]);
    }
    this.newSignalText.set('');
  }

  removeSignal(target: string): void {
    this.signals.update((current) => current.filter((s) => s !== target));
  }

  submit(): void {
    if (!this.canSubmit()) {
      return;
    }
    this.submitting.set(true);
    this.errorMessage.set(null);
    this.api
      .createCorrection({
        case_id: this.data.caseId,
        turn_id: this.data.turnId,
        signals: this.signals(),
        applicability: this.applicability(),
        answer: this.answer(),
      })
      .subscribe({
        next: (response) => {
          this.dialogRef.close(response);
        },
        error: (err: unknown) => {
          this.submitting.set(false);
          this.errorMessage.set(extractErrorMessage(err, '登録に失敗しました'));
        },
      });
  }

  cancel(): void {
    this.dialogRef.close();
  }
}
