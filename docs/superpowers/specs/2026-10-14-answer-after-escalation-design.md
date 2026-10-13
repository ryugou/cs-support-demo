# 取次済み case でも今のターンに答える

| 項目    | 内容 |
| ----- | ---- |
| 目的    | 取次（受付番号発行）後の case で、以降の質問が内容に関係なく固定返信になる挙動をやめ、今のターンに答えられる質問には答える（Issue #101） |
| 読者    | 実装者、レビュアー |
| 正本の範囲 | 取次済み case での判定入力、部分回答の条件、応答の合成 |
| 関連文書  | [`2026-10-07-partial-answer-with-handoff-design.md`](2026-10-07-partial-answer-with-handoff-design.md)（部分回答。§3.1 の「未取次」条件を本書で改める）、[`2026-08-11-answer-api-line-adapter-design.md`](2026-08-11-answer-api-line-adapter-design.md)（応答の形）、Issue #54（取次状態の安定化。取次事象の重複防止はそのまま） |

## 1. 不具合と原則

本番（受付番号 9b1119cc、2026-10-02）: 同じ LINE セッションの 1 ターン目が第 1 層で取次になった後、2 ターン目「24時間録画対応されているか」が「内容は担当者へお伝えします」+ 受付番号の固定返信になった。材料はあった。

原因は 2 つの組み合わせ。(a) case 内で signal が累積するため、1 ターン目で当たった取次ルールが以降も当たり続ける。(b) 取次済み case（`CaseConvState::is_already_escalated()`）では `build_already_escalated_reply` の固定返信になり、部分回答も「未取次」を条件にしているため下書きを生成しない。

原則: **取次済みでも、今のターンに答えられるなら答える。防ぐのは取次の重複（受付番号の再発行・取次事象の再作成・希望時間帯の再聞き取り）だけ。**

## 2. 判定（`Harness::evaluate`）

- 呼び出し側が `ReplyDraftPolicy::SkipWhenUnused { already_escalated: true }` を渡したターンでは、`decide` の `question_signals` に **今のターンの signal だけ**を渡す。累積集合（`accumulated`）は従来どおり case の `signals` 属性と監査に記録するが、判定には使わない。理由: 取次後にヒアリング（累積が必要な理由）を続ける必要は無く、累積を使うと取次ルールが永久に当たる。
- `Always`（MCP）と未取次の case は変更しない。
- Jev（ヒアリング契約の判定）は取次済み case では呼ばない（現行どおり。`api.rs` 側が `!is_already_escalated()` でガードしている）。
- 監査: 判定が今ターンのみの signal で行われたことを `EvaluationOutcome` に `decision_basis: TurnOnly | Accumulated` として持ち、監査イベントの `decision` 文字列に `:turn_only` を付ける（例: `allowed:turn_only`、`escalate:layer1:turn_only`）。既存の文字列は変えない。

## 3. 部分回答の条件（`handoff_items::can_draft_partial_answer`）

`2026-10-07-partial-answer-with-handoff-design.md` §3.1 の「その case が既に取次済みではない」を**削除**する。取次済み case でも、第 1・2 層の取次で取り次ぐ項目があり材料の関連度が十分なら、部分回答の下書きを生成する。`ReplyDraftPolicy::SkipWhenUnused` の `already_escalated` は §2 の判定入力の切り替えと §4 の合成にだけ使う。

## 4. 応答の合成（`api.rs`）

取次済み case（`conv.is_already_escalated()`）の応答は、今のターンの判定で分ける。

| 今のターンの結果 | 応答 |
| --- | --- |
| `Allowed` で下書きあり | 下書き / 空行 / 「※先にお受けしたお問い合わせ（受付番号: XXXX）は、担当者が確認を進めています。」 |
| `Escalate` で部分回答の下書きあり | 下書き / 空行 / 取次の一文（§3.3 の `build_handoff_sentence`）/ 空行 / 「受付番号: XXXX で担当者が確認を進めています。」 |
| `Escalate` で下書きなし、`Allowed` で下書きなし（`NO_ANSWER` 等） | 現行の `build_already_escalated_reply`（固定返信） |
| `LexiconFallback` | 現行どおり固定返信（fail closed） |

- 受付番号は case の既存のものを使い、新しい取次事象（`create_escalation_event` 相当）を作らない。
- `awaiting_time_pref` / `preferred_contact_time` / `escalation_confirmed` の状態遷移は変えない。希望時間帯を待っている状態（`awaiting_time_pref`）で希望時間帯らしい発話が来た場合の処理（`time_pref.rs`）は、本書の分岐より**先に**評価する（現行の順序を維持）。
- 営業時間外の案内行（`out_of_hours_line`）は、固定返信の行と同じ条件で末尾に付ける。
- 上記の固定文は `escalation_reply.rs` に定数として置き、`build_answer_in_escalated_case` / `build_partial_answer_in_escalated_case` の 2 つの純関数で組み立てる（`assemble_partial_answer_reply` と同じ流儀）。

## 5. 不変条件

- 取次事象・受付番号は 1 case に 1 回。取次済み case で再発行しない。
- 取次済み case でも、取り次ぐ項目について金額・可否・条件を顧客に提示しない（部分回答の歯止めは §3.5 のまま適用）。
- 監査に発話本文を出さない。
- MCP `evaluate_answerability` の挙動は変わらない。

## 6. テスト

| 対象 | 固定する内容 |
| --- | --- |
| 判定入力の切り替え（純関数） | `already_escalated = true` のとき `decide` に渡る signal が今ターンのみ。`false` と `Always` では累積 |
| 監査文字列 | `allowed:turn_only` / `escalate:layer1:turn_only` が付く。未取次では従来の文字列のまま |
| 部分回答の条件 | 取次済みでも、第 1・2 層 + 項目あり + 関連十分なら可 |
| 合成（純関数） | 表の 4 行の文面。受付番号が case のもの。取次の一文の位置 |
| 取次事象 | 取次済み case のターンで `create_escalation_event` が呼ばれない。`awaiting_time_pref` の状態が変わらない |
| 回帰 | 「24時間録画対応されているか」相当（signal `continuous_recording`、材料あり）を取次済み case で投げると、固定返信ではなく回答が返る |
| 既存 | 時間帯聞き取り、取次済みの固定返信（下書きなし）、`LexiconFallback` の既存テストが通る |

実出力の検証（§9 の実 LLM スモーク）に、「取次済み case での情報質問」のケースを加える: 1 ターン目を取次ルールに当たる発話（例: 「解約したいです」）、2 ターン目を「24時間録画対応されているか」とし、2 ターン目の最終応答文が回答 + 受付番号の確認中の一文になることを assert する。材料は本番 `search_manual` の実結果（`question_hits` / `signal_hits`）。

## 7. ドキュメント更新

- `2026-10-07-partial-answer-with-handoff-design.md` §3.1 / §3.6 の「取次済み」条件を本書への参照に置き換える。
- `2026-08-11-answer-api-line-adapter-design.md` の応答の形に、取次済み case の回答形式を追記する。
- `specs/production-cs-mcp.md` の会話層に本書への参照を追記する。

## 8. 対象外

- KnownResolution の照合順序の変更（別途、ナレッジ優先の設計で扱う）。
- 累積 signal の仕様自体の変更（未取次の case のヒアリングには累積が必要）。

## 9. `ReplyDraftPolicy` の目的分離（実装時の判断、2026-10-14）

`advisor/cs_support.rs`（homesec の CS 連携モード）は `SkipWhenUnused { already_escalated: true }` を固定で渡し、「homesec では部分回答を出さない」（`2026-10-07-partial-answer-with-handoff-design.md` §8）を `already_escalated` の不適格条件で実現していた。本書 §2 が同じフィールドを「今ターンのみで判定する」の切替に使うと、homesec の累積判定が壊れる。

- `ReplyDraftPolicy::SkipWhenUnused` に `partial_answer_enabled: bool` を追加する。部分回答の下書きを生成するのはこれが `true` のときだけ（§3 の条件に加える）。`/api/reply` は `true`、homesec の CS 連携モードは `false` を渡す。`Always`（MCP）は従来どおり部分回答あり。
- `already_escalated` は本来の意味（その会話が取次済みか）だけに使う。homesec の CS 連携モードは会話の取次状態を持たないため `false` を渡す（判定は従来どおり累積集合で行う）。
- これにより homesec の挙動は本変更の前後で変わらない（部分回答なし・累積判定）。テストで固定する（homesec の policy で部分回答の下書きが生成されないこと、`decide` に累積集合が渡ること）。
