# signal lexicon の抑止語形（suppress_forms）

| 項目    | 内容 |
| ----- | ---- |
| 目的    | 契約前の一般的な質問が「契約」の部分一致で `contract_billing_question` を立て、第 1 層で取次になる問題を解消する（Issue #75） |
| 読者    | 実装者、レビュアー |
| 正本の範囲 | lexicon エントリの属性 `suppress_forms` の型・検証・照合規則、`contract_billing_question` に設定する抑止語形 |
| 関連文書  | [`../../../specs/signal-vocabulary.md`](../../../specs/signal-vocabulary.md)（signal 語彙の正本）、[`../../../specs/production-cs-mcp.md`](../../../specs/production-cs-mcp.md)（3 層判定の正本） |

## 1. 採用する設計

lexicon のエントリに、省略可能な属性 `suppress_forms`（抑止語形の配列）を追加する。`LexiconNormalizer::normalize` は、エントリごとに「正規化後の発話から、そのエントリの抑止語形の出現箇所をすべて取り除いた文字列」に対して `surface_forms` を照合する。

取り除くのは抑止語形に一致した部分だけである。同じ発話の別の箇所に `surface_forms` が残っていれば、signal は従来どおり立つ。

抑止はエントリ単位で独立に行う。あるエントリの抑止語形は、他のエントリの照合に影響しない。

## 2. 型と検証

### 2.1 lexicon ファイル

```json
{ "signal": "...", "class": "context", "surface_forms": ["..."], "suppress_forms": ["..."] }
```

`suppress_forms` は省略可能で、省略時は空配列として扱う。

### 2.2 `server/src/harness/signal.rs`

- `LexiconEntry` に `#[serde(default)] suppress_forms: Vec<String>` を追加する。
- 読み込み時に、`surface_forms` と同じ正規化（`normalize_key`）を抑止語形にも適用して保持する。
- 読み込み時に次を検証し、違反があれば `from_json` をエラーにする。エラー文には signal 名と違反した語形を含める。
  - 正規化後の抑止語形が空文字
  - 正規化後の抑止語形が、同じエントリの正規化後の `surface_forms` のいずれも部分文字列として含まない（抑止しても照合結果が変わらない設定は誤りとして拒否する）
- `llm_only: true` のエントリが `suppress_forms` を持つ場合もエラーにする（文字列照合をしないエントリには意味が無い）。

### 2.3 照合規則

`normalize` は、エントリごとに次の順で判定する。

1. 発話を `normalize_key` で正規化する（現行どおり、発話につき 1 回）。
2. エントリの抑止語形が空なら、正規化後の発話に対して現行どおり `surface_forms` を照合する。
3. エントリの抑止語形が空でなければ、正規化後の発話から抑止語形の出現箇所をすべて取り除いた文字列を作り、それに対して `surface_forms` を照合する。取り除く順序は、正規化後の抑止語形を文字数の長い順に並べた順とする（「契約する前」が「契約すると」より先に処理されるなど、長い語形の一部だけが先に消えることを防ぐ）。

取り除いた結果、前後の文字が連結して新たに `surface_forms` に一致する場合がある。これを防ぐため、取り除いた箇所は空文字ではなく、正規化後の発話に現れない区切り文字 1 文字に置き換える。区切り文字は `normalize_key` が出力しない文字から選び、定数として定義する。

## 3. データ

`server/data/urtect/signal-lexicon.json` の `contract_billing_question` に次を設定する。

```json
"suppress_forms": ["契約前", "契約する前", "契約すると", "契約したら", "契約した場合", "契約を検討", "契約検討", "契約しようか"]
```

同じエントリの `description` を次に置き換える（語義の明確化。ルーティング方針は書かない）。

```
既に契約している顧客の、契約・解約・プラン変更・請求額・支払い方法に関する個別の相談（契約前の一般的な質問は含まない）
```

`surface_forms`、`customer_label`、他のエントリ、`server/data/urtect/rules.json`、root の `signal-lexicon.json` は変更しない。

## 4. 不変条件

- `suppress_forms` を持たないエントリの照合結果は、変更前と同一である。
- 抑止語形は、同じ発話にある別の個別案件語を隠さない（「契約前ですが解約金はいくらですか」は `解約` により `contract_billing_question` が立つ）。
- LLM 分類（`server/src/harness/extraction.rs` の和集合）には影響しない。lexicon が立てなかった signal を LLM が立てることは従来どおりありうる。

## 5. 移行時の挙動

`LexiconEntry` は未知フィールドを拒否しないため、`suppress_forms` を含む lexicon を旧イメージが読んでも失敗しない（属性は無視され、抑止されないだけ）。lexicon はイメージ内ファイルであり、デプロイで反映される。`ingest-rules` の実行は不要である。

lexicon の照合結果は `ingest_urtect` の `MENTIONS_SIGNAL` 辺のハッシュに含まれる。次回 `ingest_urtect` 実行時、抑止語形を含む section は再 upsert される。

## 6. テスト

| 対象 | 固定する内容 |
| --- | --- |
| `signal.rs`（手組みの lexicon） | 抑止語形だけを含む発話では signal が立たない。抑止語形と、別の箇所の `surface_forms` の両方を含む発話では立つ。抑止語形を持たないエントリは影響を受けない |
| `signal.rs`（手組みの lexicon） | 取り除いた箇所の前後が連結して `surface_forms` に一致する入力でも、signal が立たない |
| `signal.rs`（手組みの lexicon） | 長い抑止語形と、その接頭辞を共有する短い抑止語形が両方ある場合に、長い語形が正しく取り除かれる |
| `signal.rs`（読み込み検証） | 空の抑止語形、`surface_forms` を含まない抑止語形、`llm_only` エントリの抑止語形をそれぞれ拒否し、エラー文に signal 名が含まれる |
| bundled lexicon | 「契約前に料金を知りたいです」「契約すると月額いくらですか」「契約を検討していますが料金を教えてください」で `contract_billing_question` が立たず、`price_question` が立つ |
| bundled lexicon + bundled rules | 上記 3 発話が第 1 層のどのルールにもマッチしない |
| bundled lexicon + bundled rules | 「契約内容を変更したい」「契約を更新したい」「解約したいです」「契約前ですが解約金はいくらですか」が第 1 層 `contract-billing` にマッチする |
| root の `signal-lexicon.json` | 従来どおり読み込める（`suppress_forms` なし） |

## 7. ドキュメント更新

- `specs/signal-vocabulary.md`: 運用ルール節に `suppress_forms` の規則（本書への参照）を追記する。`price_question` 節の「価格・費用の質問と個別案件語が同居する発話は取次になる」の実測例から「契約前に料金を知りたいです」を外し、抑止語形の一覧と、変更後の lexicon を `LexiconNormalizer` に通した実測に基づく既知の限界を記録する。

## 8. 既知の限界

- 既存の契約者が抑止語形を使って個別案件を述べる発話（「契約前に聞いた説明と違います」）は、他の個別案件語を含まなければ `contract_billing_question` が立たず、第 1 層を通過する。本番は LLM 分類との和集合が補うが、LLM 失敗時は補われない。
- 抑止語形に無い言い回し（「契約するなら」「契約を考えています」）は抑止されず、従来どおり取次になる。
- 公開されている支払い手段の質問（「支払方法は何がありますか」「クレジットカードは使えますか」「口座振替はできますか」）は本書の対象外で、従来どおり取次になる。
