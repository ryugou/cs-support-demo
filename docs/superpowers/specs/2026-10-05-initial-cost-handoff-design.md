# 初期費用の問い合わせの取次と、ルール宣言の受け止め文

| 項目    | 内容 |
| ----- | ---- |
| 目的    | 初期費用の問い合わせを第 1 層で即時取次にし、取次ルールが顧客向けの受け止め文を宣言できるようにする（Issue #76） |
| 読者    | 実装者、レビュアー |
| 正本の範囲 | signal `initial_cost_question` と取次ルール `initial-cost-quote` の定義、ルール属性 `customer_ack` の型・検証・伝搬経路・適用箇所 |
| 関連文書  | [`../../../specs/production-cs-mcp.md`](../../../specs/production-cs-mcp.md)（3 層判定の正本）、[`../../../specs/signal-vocabulary.md`](../../../specs/signal-vocabulary.md)（signal 語彙の正本）、[`2026-09-21-jev-shadow-design.md`](2026-09-21-jev-shadow-design.md)（同じ経路で伝搬するルール属性 `hearing` の先例） |

## 1. 採用する設計

1. signal `initial_cost_question` を新設する。
2. 第 1 層ルール `initial-cost-quote`（`binding: mandatory`）を追加し、`initial_cost_question` が立った問い合わせを即時取次にする。
3. 取次ルールに省略可能な属性 `customer_ack`（顧客向けの受け止め文）を追加する。宣言があるルールで取次になったとき、`/{project_id}/api/reply` は LLM で受け止め文を生成せず、宣言された文をそのまま使う。

`mandatory` にする理由: 初期費用は設置環境で決まり、聞き返しても公開情報からは答えられない。advisory はマニュアル材料が不足すると聞き返しに開放されるため使わない。

## 2. データ定義

### 2.1 `server/data/urtect/signal-lexicon.json`

次の signal を追加する。

```json
{ "signal": "initial_cost_question", "class": "context", "surface_forms": ["初期費用", "初期コスト", "設置費", "工事費", "導入費", "取り付け費", "取付費", "設置料金", "工事料金", "設置代金", "工事代金"], "description": "設置・導入時にかかる初期費用や工事費用の照会", "customer_label": "初期費用・設置工事費に関するご質問" }
```

`price_question` の surface_forms から `初期費用` を削除する（他の語は変更しない）。「初期費用はいくらですか」は `いくらですか` により引き続き `price_question` も立つが、第 1 層は `initial-cost-quote` で確定する。

`設置代` `工事代` は採用しない（「設置代行」に部分一致するため）。

root の `signal-lexicon.json` と `server/data/rules.sample.json` は変更しない。

### 2.2 `server/data/urtect/rules.json`

`escalation_rules` に次を追加する。

```json
{ "rule_id": "initial-cost-quote", "condition": ["initial_cost_question"], "owner": "contract", "route": "support_desk", "binding": "mandatory", "customer_ack": "初期費用はお客様の状況によって異なりますので、担当者におつなぎします。" }
```

既存ルールと `prohibited_domains` は変更しない。

### 2.3 スキーマ

`schema/cs-schema.yml` と `schema/cs-support.yml` の `EscalationRule.attributes` に次を追加する（`hearing` の直後）。加算のみ。

```yaml
customer_ack: { type: string }
```

## 3. 属性 `customer_ack`

### 3.1 型と検証

| 箇所 | 型・規則 |
| --- | --- |
| `rules.json`（`escalation_rules` の各要素） | 省略可能な文字列。`prohibited_domains` には追加しない |
| `ingest_rules` の入力型 `RuleInput` | `customer_ack: Option<String>`（`deny_unknown_fields` は維持） |
| vegapunk の `EscalationRule` ノード属性 | `customer_ack`。宣言なしは空文字（`hearing` と同じ規約） |
| `harness::rules::EscalationRule` | `pub customer_ack: Option<String>` |
| `AnswerDecision::Escalate` | `#[serde(skip)] customer_ack: Option<String>`。MCP の出力契約には載せない |

`ingest_rules` は、宣言された値が次のいずれかに当たる場合、vegapunk へ接続する前にエラーで拒否する。エラー文には rule_id と違反内容を含める。

- 前後の空白を除いた結果が空文字
- 改行（`\n` `\r`）を含む
- 文字数（`chars().count()`）が 120 を超える

`binding` による制限は設けない（advisory のルールも宣言できる）。

### 3.2 読み出し

`harness::knowledge::escalation_rule_from_attributes` は、属性が無い場合と空文字の場合を「宣言なし」（`None`）として読む。値がある場合は前後の空白を除いて `Some` にする。読み出し時に長さ・改行の検証はしない（投入時に検証済みのため。属性が無い旧データでもルールの読み込みを失敗させない）。

### 3.3 判定への伝搬

`harness::decision::decide` は、第 1 層でマッチしたルールの `customer_ack` を `AnswerDecision::Escalate.customer_ack` に複製する。第 2 層・第 3 層の取次では常に `None` にする。`binding` による抑止はしない（`hearing_contract` と異なり、mandatory でも宣言を有効にする）。

## 4. 適用箇所

### 4.1 `/{project_id}/api/reply`（`server/src/api.rs` の `build_escalation_reply_text`）

取次応答（`ReplyAction::EscalationReply`）の受け止め文を次の順で決める。

1. case が取次確定済み（`conv.is_already_escalated()`）の場合は現行どおり `build_already_escalated_reply` を使う。`customer_ack` は使わない。
2. 判定が `customer_ack: Some(text)` を持つ場合、LLM を呼ばず `text` を受け止め文にする。`is_continuation` の値によらず同じ文を使う。
3. それ以外は現行どおり（`draft_ack_text`、drafter が無ければ `fallback_ack`）。

2 の文も、LLM 生成の受け止め文と同じ 2 つのゲートを通す。

- NG 表現ゲート: `escalation_reply.rs` に、宣言文を `apply_draft_gate_or_fallback` に通す関数を追加する（`draft_ack_text` の後半と同じ引数構成、`EmitChannel::Operator`）。却下時は `fallback_ack(is_continuation)` の文へ倒し、warn を出す。
- 取扱製品ゲート: 既存の `gate_generated_text` をそのまま通す。

決定的ブロック（受付番号・希望時間帯・時間外案内）の組み立てと `arm_time_pref_solicitation` は変更しない。

聞き返し（`ReplyAction::Clarify`）では `customer_ack` を使わない。

### 4.2 MCP `evaluate_answerability` の `customer_reply_draft`（`server/src/harness/reply.rs`）

`ReplyBrief` に `handoff_ack: Option<String>` を追加する。`build_reply_brief` は `AnswerDecision::Escalate.customer_ack` を複製する（`ReplyKind::Answer` では常に `None`）。

取次用プロンプト（`ReplyKind::Escalation` の分岐）は、`handoff_ack` が `Some` のとき、次の趣旨の規則を 1 行追加する: 「取り次ぐ理由として、次の一文をそのまま含める: <宣言文>」。宣言文は既存の `neutralize_delimiters` を通してから埋め込む。

`evaluate_answerability` の応答 JSON の形は変更しない。

## 5. 不変条件

- `customer_ack` を宣言していないルールの取次応答は、変更前と同じ経路・同じ文面生成になる。
- `customer_ack` は顧客に見せる文であり、社内の判定理由・ルール ID・スコアを書かない（データ作成時の規約）。
- 顧客に出る受け止め文は、宣言文であっても NG 表現ゲートと取扱製品ゲートを必ず通る。
- MCP tool の入出力の形は変更しない。

## 6. 障害時・移行時の挙動

| 状況 | 挙動 |
| --- | --- |
| 新イメージがデプロイ済みで `ingest-rules` が未実行 | `initial-cost-quote` ルールが vegapunk に無い。初期費用の問い合わせは第 1 層を通過し、変更前と同じ判定になる |
| vegapunk のノードに `customer_ack` 属性が無い（旧データ） | 「宣言なし」として読む。ルールの読み込みは失敗しない |
| 宣言文が NG 表現ゲートまたは取扱製品ゲートで却下された | `fallback_ack` の文へ倒し、warn を出す。取次自体は成立する |

反映は main マージ後の CI で行われる（`server/data/**` の変更により `auto-ingest` が `ingest-rules` を実行する）。

## 7. テスト

既存テストの書き方（bundled lexicon / bundled rules を読むテスト、`ingest_rules.rs` と `knowledge.rs` の `hearing` のテスト）に合わせる。

| 対象 | 固定する内容 |
| --- | --- |
| lexicon 抽出 | 「初期費用はいくらですか？」「設置工事費はいくらかかりますか」「導入費用を教えてください」で `initial_cost_question` が立つ |
| lexicon 抽出 | 「月額いくらですか？」「設置方法を教えてください」「設置代行はありますか」で `initial_cost_question` が立たない |
| 第 1 層 | 上記 3 つの初期費用の発話が bundled rules で `initial-cost-quote` にマッチする。「月額いくらですか？」は第 1 層のどのルールにもマッチしない |
| `decide` | `initial-cost-quote` にマッチしたとき `missing` が空で、`customer_ack` が宣言文と一致する。第 2 層・第 3 層の取次では `customer_ack` が `None` |
| `ingest_rules` | bundled `rules.json` がパースでき、`initial-cost-quote` のノードに `customer_ack` 属性が宣言文で書かれ、他のルールは空文字になる |
| `ingest_rules` | 空白のみ・改行入り・121 文字の `customer_ack` をそれぞれ拒否し、エラー文に rule_id が含まれる。`customer_ack` を持たない rules ファイルは従来どおりパースできる |
| `knowledge.rs` | 属性なし・空文字は `None`、値ありは `Some`（前後の空白を除く） |
| `api.rs` | `customer_ack` を持つ取次では、drafter があっても LLM を呼ばず、応答が宣言文で始まり決定的ブロックが続く。宣言なしの取次は現行の経路を通る。取次確定済みの case では宣言文を使わない |
| `reply.rs` | `handoff_ack` が `Some` のとき取次用プロンプトに宣言文が含まれ、`None` のとき含まれない |
| MCP 出力契約 | `AnswerDecision::Escalate` のシリアライズ結果に `customer_ack` キーが現れない |

## 8. ドキュメント更新

- `specs/signal-vocabulary.md`: `initial_cost_question` の節を追加し、`price_question` 節の surface_forms 一覧から `初期費用` を除く。受容した誤発火は、変更後の lexicon を `LexiconNormalizer` に通した実測に基づいて記録する。
- `specs/production-cs-mcp.md`: 第 1 層ルールの記述に、ルール属性 `customer_ack` の存在と本書への参照を追記する。

## 9. 既知の限界

- 初期費用の語と他の質問が同じ発話にある場合（「月額と初期費用を教えてください」）は、発話全体が取次になり、月額には答えない。
- 「設置にいくらかかりますか」のように、2.1 の語を含まない言い回しは lexicon では拾えない。本番は LLM 分類との和集合（`server/src/harness/extraction.rs`）が補うが、LLM 失敗時は補われない。
