# アドバイザー: 提案の要求を担当者連絡の希望と取り違えない

| 項目    | 内容 |
| ----- | ---- |
| 目的    | 「提案して」のようなボットへの提案要求が `lead_interest` と判定され、提案せずに担当者連絡の案内（LeadSolicit）へ進む問題を解消する（Issue #49） |
| 読者    | 実装者、レビュアー |
| 正本の範囲 | 理解プロンプトの `lead_interest` の定義、`decide` の手順 5 の追加条件 |
| 関連文書  | [`2026-08-17-homesec-advisor-design.md`](2026-08-17-homesec-advisor-design.md)（アドバイザーの正本。§4.1 理解の出力、§4.3 応答型の決定） |

## 1. 採用する設計

2 段で直す。

1. **理解プロンプトの定義を狭める**（`server/src/advisor/understand.rs`）: `lead_interest` は、人間の担当者からの連絡・案内を明示的に望む発話だけを `true` にする。ボット自身への提案・回答の要求は `false`。
2. **決定論の保険**（`server/src/advisor/decide.rs` 手順 5）: case の最初のターンで `lead_interest` と `product_intent` の両方が `true` のときは、LeadSolicit に進まず、提案（Clarify または Answer）を優先する。

1 だけでは LLM の判定の揺れを止められないため、2 を置く。2 だけでは「担当者から聞きたい」と「提案して」を区別できないため、1 も要る。

## 2. 理解プロンプト

`lead_interest` の定義を次の趣旨に書き直す。文言は既存プロンプトの文体に合わせる。

- `true` にするのは、人間の担当者からの連絡・案内・相談を明示的に望む場合だけ。例: 「担当者から話を聞きたい」「連絡をください」「導入を相談したい」「（担当者連絡の提案に対して）お願いします」。
- ボットに対する提案・回答・比較の要求は `false`。例: 「提案して」「おすすめを教えて」「どれが合うか選んで」「比較して」。
- 迷う場合は `false`。

`product_intent` の定義は変えない。

テスト: 既存の containment テスト（プロンプトに定義と例が含まれること）に、上の `true` の例と `false` の例を加える。Issue の実測発話（「屋外カメラを本気で選びたい。うちに合うものを提案して。一戸建てで玄関と駐車場を映したい」）を `false` の例として含める。

## 3. `decide` の手順 5

現行:

```
手順5: !suppress_lead_solicit && u.lead_interest && !lead_requested → LeadSolicit
```

変更後:

```
手順5: !suppress_lead_solicit && u.lead_interest && !lead_requested && !(first_turn && u.product_intent) → LeadSolicit
```

- `first_turn` は「この case で、ボットがまだ一度も応答していない」こと。既存の会話状態から決定論で求める（`turn_count` 等。既存の状態で判定できる値を使い、新しい状態は増やさない）。
- `first_turn && u.product_intent` で LeadSolicit を見送ったときは、手順 6（Clarify）以降へ進む。`lead_interest` の値は捨てず、次のターン以降は従来どおり手順 5 で判定する。
- 見送ったことを `tracing::info!` で 1 行出す（`case_id`、理由）。発話本文は出さない。

### 根拠

- 最初のターンでは、担当者連絡の提案（カード・案内）がまだ出ていない。この時点で `lead_interest` が `true` になるのは、利用者が最初から明示的に担当者を求めた場合か、LLM の誤判定かのどちらかで、`product_intent` も `true` なら後者の可能性が高い。
- 2 ターン目以降は、カードの「導入相談」ボタンや LeadSolicit への返答など、担当者連絡を望む発話が自然に起きるので、条件を付けない。

### 受容する挙動

- 最初のターンで「担当者から直接聞きたい。玄関に付けるカメラの導入について」のように、担当者連絡と製品の話を同時に明示した場合も、提案を先に返す。利用者が次のターンで再度担当者を求めれば LeadSolicit になる。提案を 1 回挟む害は、提案を求めた人に提案を返さない害より小さい。

## 4. 不変条件

- 判定・ルーティングは Rust の決定論で行い、LLM は判定材料（`lead_interest` / `product_intent`）を出すだけである（現行の不変条件のまま）。
- `urtect_support`、`emergency`、`in_domain`、時間帯受付の優先順位（手順 1〜4）は変更しない。
- `lead_requested` が `true` の case、`suppress_lead_solicit` の場合の挙動は変更しない。
- 2 ターン目以降の手順 5 の挙動は変更しない。

## 5. テスト

| 対象 | 固定する内容 |
| --- | --- |
| 理解プロンプト | `lead_interest` の定義と、`true` / `false` の例（Issue の実測発話を含む）がプロンプトに含まれる |
| `decide` 手順 5 | 最初のターンで `lead_interest && product_intent` → LeadSolicit にならず、条件が不足なら Clarify、十分なら Answer。最初のターンで `lead_interest && !product_intent` → 従来どおり LeadSolicit。2 ターン目以降で `lead_interest && product_intent` → 従来どおり LeadSolicit。見送り時に `lead_interest` の値が状態として失われない |
| 既存 | `decide` の既存テスト（優先順位、`lead_requested`、`suppress_lead_solicit`、時間帯）が変更なしで通る |

## 6. 本番での確認（マージ後）

Issue #49 の完了条件は本番 E2E。

1. 新規の会話で Issue の実測発話を送り、製品提案（Clarify または Answer）が返ること。
2. 製品カードの「導入相談」ボタンの発話（「〜の導入を相談したい」）で、従来どおり LeadSolicit になること。

どちらも LINE からの確認が必要で、こちらでは行えない。

## 7. 対象外

- `lead_interest` の判定を LLM から決定論へ全面的に置き換えること。
- 理解プロンプトの他のフィールドの定義変更。
