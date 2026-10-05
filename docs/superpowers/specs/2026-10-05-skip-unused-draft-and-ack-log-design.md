# 取次時の未使用下書きの生成停止と、宣言文不使用のログ

| 項目    | 内容 |
| ----- | ---- |
| 目的    | 応答生成 API の取次で使われない返信文下書きの LLM 生成を止める（Issue #78）。`customer_ack` が複数ルールのマッチで使われなかったことを観測できるようにする（Issue #79） |
| 読者    | 実装者、レビュアー |
| 正本の範囲 | `Harness::evaluate` の下書き生成方針 `ReplyDraftPolicy`、各呼び出し経路が渡す値、宣言文不使用ログと重複ルール警告の出力条件・項目 |
| 関連文書  | [`2026-10-05-initial-cost-handoff-design.md`](2026-10-05-initial-cost-handoff-design.md)（`customer_ack` と適用条件の正本）、[`../../../specs/production-cs-mcp.md`](../../../specs/production-cs-mcp.md)（3 層判定の正本） |

## 1. 採用する設計

1. `Harness::evaluate` に下書き生成方針 `ReplyDraftPolicy` を引数として追加する。応答生成 API とアドバイザの経路は「使われない下書きは生成しない」（取次、LLM 分類失敗、二段目ゲートの打ち切り）を、MCP の経路は「常に生成する」を渡す。
2. `Harness::evaluate` は、第 1 層の取次で `customer_ack` が複数マッチにより使われなかったとき、ログを 1 行出す。
3. `load_escalation_rules` は、同じ `rule_id` のルールを複数読み込んだとき、警告を 1 行出す。

判定（`decide`）・応答文・MCP tool の入出力の形は変更しない。

## 2. 下書き生成方針（Issue #78）

### 2.1 型

`server/src/harness/mod.rs` に定義する。

```rust
/// `Harness::evaluate` が返信文下書き（`customer_reply_draft`）を生成する条件。
#[derive(Debug, Clone, Copy)]
pub enum ReplyDraftPolicy<'a> {
    /// 判定によらず生成する。
    Always,
    /// 応答側が下書きを使わないと確定しているターンでは生成しない。次のいずれかのとき生成しない。
    /// - 判定が取次（`AnswerDecision::Escalate`）
    /// - 抽出モードが `ExtractionMode::LexiconFallback`（判定によらない）
    /// - 二段目ゲート（確定した取扱外製品への言及）が打ち切る（判定によらない）
    ///
    /// `response_allowlist` は、応答側が二段目ゲート（取扱外製品の打ち切り）に渡すのと同じ allowlist。
    SkipWhenUnused {
        response_allowlist: &'a product_gate::ProductAllowlist,
    },
}
```

既定値は設けない。`evaluate` の全呼び出し箇所が明示的に渡す。

### 2.2 各経路が渡す値

| 呼び出し箇所 | 渡す値 | 理由 |
| --- | --- | --- |
| `server/src/rmcp_server.rs`（MCP `evaluate_answerability`） | `Always` | 応答の `customer_reply_draft` は出力契約であり、取次時も返す |
| `server/src/api.rs`（`/{project_id}/api/reply`） | `SkipWhenUnused { response_allowlist }`。`response_allowlist` は質問側ゲートで取得し二段目ゲート（`second_stage_short_circuit`）にも渡す同じ変数の `&allowlist` | 取次時は受け止め文と決定的ブロック、または聞き返しで応答を組み立て、`LexiconFallback` 時は判定によらず取次応答へ倒し（`decide_reply_action`）、二段目ゲートが打ち切るときは判定によらず取扱外の定型応答を返す（`second_stage_out_of_scope_reply`）ため、いずれも下書きを使わない |
| `server/src/advisor/cs_support.rs`（homesec 経由） | `SkipWhenUnused { response_allowlist }`。`response_allowlist` は同上（二段目ゲートに渡す同じ変数の `&allowlist`） | 同上 |

### 2.3 `evaluate` の挙動

判定が確定し監査記録（`audit_with_nodes`）を終えた後、下書き生成の直前で次を判定する。

- 方針が `SkipWhenUnused` で、判定が `AnswerDecision::Escalate` である、抽出モードが `ExtractionMode::LexiconFallback` である、または二段目ゲートが打ち切るときのいずれか: `draft_customer_reply` を呼ばない。`EvaluationOutcome.customer_reply_draft` は `None`、`customer_reply_draft_truncated` は `false` にする。
- それ以外: 現行どおり `draft_customer_reply` を呼ぶ。

`LexiconFallback` で下書きを作らないのは、応答生成 API とアドバイザが LLM 分類失敗のターンを判定（`Allowed` を含む）によらず取次応答にするためである。抽出モードは下書き生成より前に確定している。

二段目ゲートが打ち切るかどうかは、`product_gate::find_confirmed_foreign_reference(&product_references, question, response_allowlist).confirmed` が `Some` かどうかで決める（純関数 `second_stage_short_circuits`）。この関数は、応答側（`api.rs` / `advisor/cs_support.rs` の `second_stage_out_of_scope_reply`）が呼ぶ `product_gate::confirmed_foreign_reference` と同じ判定本体で、入力も同一である。違いはログだけである。判定本体 `find_confirmed_foreign_reference` はログを出さず、veto（取扱内型番と矛盾する `foreign` 分類の打ち消し）が起きたことを戻り値で返す。`confirmed_foreign_reference` はその戻り値から veto の警告（`tracing::warn!`）を出すラッパーである。事前判定は判定本体だけを呼ぶため、同じ入力が事前判定と応答側で 2 回判定されても、veto の警告は応答側の判定で 1 回だけ出る。`product_references` は `EvaluationOutcome.product_references` と同じ値、質問本文は応答側が二段目ゲートに渡すメッセージと同じ値、allowlist は方針に載せた `response_allowlist`（応答側が二段目ゲートに渡すのと同じ変数）である。`evaluate` 内で取得した allowlist はこの事前判定に使わない。したがって事前判定は応答側の判定と必ず一致する。方針が `Always` のときは `false` を返す。

`should_draft_reply` の条件は、応答側（`decide_reply_action` と二段目ゲート）が下書きを読まない条件と一致させる。

`reply_drafter` が無い構成（`customer_reply_draft_enabled = false`）の挙動は変わらない。

### 2.4 不変条件

- `/api/reply` とアドバイザ経路で、判定が回答（`Allowed`）になるターンの応答は変更前と同一である。
- MCP `evaluate_answerability` の応答は、取次時も含めて変更前と同一である。
- 下書きを生成しなかったことは、判定・監査記録・case の累積状態に影響しない。

## 3. 宣言文不使用のログ（Issue #79）

### 3.1 補助関数

`server/src/harness/rules.rs` に、第 1 層でマッチしたルールの参照を返す純関数を追加する。マッチ条件は既存の `rule_matches` を使う。`count_layer1_matches` はこの関数の結果の件数を返すように書き換え、条件式を複製しない。

```rust
pub fn matching_layer1_rules<'a>(rules: &'a [EscalationRule], question: &SignalSet) -> Vec<&'a EscalationRule>
```

### 3.2 出力条件と項目

`Harness::evaluate` は、判定確定後に次の条件をすべて満たすとき `tracing::info!` を 1 行出す。

- 判定が第 1 層の取次（`AnswerDecision::Escalate` かつ `layer == 1`）である。
- 累積 signal 集合に対する `matching_layer1_rules` の結果が 2 件以上である。
- その中に `customer_ack` を宣言したルールが 1 件以上ある。

出力する項目:

| 項目 | 内容 |
| --- | --- |
| `request_id` | リクエスト ID |
| `case_id` | case の ID |
| `matched_rule_ids` | マッチした全ルールの `rule_id`（昇順に並べる） |
| メッセージ | 宣言された受け止め文が、case の累積 signal（前のターンの signal を含む）に対する複数ルールのマッチにより使われなかった旨 |

顧客の発話本文、signal の値、宣言文の本文は出力しない。

このログは下書き生成方針によらず出す（MCP 経路でも出す）。

### 3.3 重複ルールの警告

`server/src/harness/knowledge.rs` の `load_escalation_rules` は、読み込んだルールの中に同じ `rule_id` が複数あるとき `tracing::warn!` を 1 行出す。項目は `schema` と、重複していた `rule_id` の一覧（昇順、重複なし）とする。読み込み自体は失敗させず、ルールの除去もしない（判定の挙動を変えない）。

メッセージには、運用者が取る行動（vegapunk 上の `EscalationRule` ノードの重複を確認し、`ingest-rules` の投入元 `rules.json` と突き合わせる）を含める。

## 4. テスト

既存テストの書き方に合わせる。LLM 呼び出しの有無は、既存テストが使うスタブ drafter のリクエストログで検証する。

| 対象 | 固定する内容 |
| --- | --- |
| `harness/mod.rs`（`evaluate`） | `SkipWhenUnused` で取次になるターン、`LexiconFallback` のターン、または二段目ゲートが打ち切るターンは drafter のリクエストが 0 件で、`customer_reply_draft` が `None`、`customer_reply_draft_truncated` が `false` |
| `harness/mod.rs`（`evaluate`） | `SkipWhenUnused` で回答（`Allowed`）、`LexiconFallback` 以外、かつ二段目ゲートが打ち切らないターンは下書きが生成される |
| `harness/mod.rs`（`evaluate`） | `Always` で取次になるターン、または二段目ゲートが打ち切るターンは下書きが生成される |
| `harness/mod.rs`（`second_stage_short_circuits`） | `Always` は常に `false`。`SkipWhenUnused` は、取扱外が確定する `response_allowlist` で `true`、同じ製品参照・質問でもその型番を取扱製品として含む `response_allowlist` で `false`（方針に載せた allowlist が判定に使われることの固定） |
| `harness/mod.rs`（`second_stage_short_circuits`） | veto が起きる入力（`matched_model` が取扱内型番）で呼んでも警告が出ない |
| `harness/product_gate.rs`（`find_confirmed_foreign_reference` / `confirmed_foreign_reference`） | 判定本体とラッパーが、確定あり・参照なし・ambiguous のみ・matched のみ・各 veto 条件で同じ結果を返す。判定本体は警告を出さず veto 情報（種別と文字数）を返す。ラッパーは veto 時に警告を 1 行だけ出し、veto が無いときは出さない。複数の参照が混在する場合（matched_model veto → surface veto → 確定 → 確定より後ろの veto 対象）は、判定本体が veto された参照を飛ばして最初の確定参照を返し、`vetoes` が走査順にちょうど 2 件（確定より後ろは記録しない）で、ラッパーが同じ順序で警告をちょうど 2 行出す |
| `harness/mod.rs`（`evaluate`） | 3.2 の 3 条件を満たすときログが 1 行出て `matched_rule_ids` が昇順で含まれる。マッチ 1 件のとき、および複数マッチでも宣言を持つルールが無いときは出ない。ログに発話本文が含まれない |
| `harness/rules.rs` | `matching_layer1_rules` が 0 件・1 件・複数件を返し、空条件のルールを含めない。`count_layer1_matches` の既存テストが変更なしで通る |
| `harness/knowledge.rs` | 重複した `rule_id` を含む入力で警告が 1 行出て、重複 ID が昇順で含まれる。重複が無いときは出ない |

`evaluate` を通すテストの土台（vegapunk クライアントのスタブ等）が既存テストに無く、新たな仕組みを作らないと書けない場合は、該当する判定部分を純関数に切り出して単体テストする。切り出す関数は次の 3 つとする。

- 二段目ゲートの打ち切り有無: `fn second_stage_short_circuits(policy: &ReplyDraftPolicy, product_references: &[ProductReference], question: &str) -> bool`（`Always` は `false`、`SkipWhenUnused` は `response_allowlist` で `find_confirmed_foreign_reference` を評価する）
- 下書きを生成するかどうか: `fn should_draft_reply(policy: &ReplyDraftPolicy, decision: &AnswerDecision, extraction_mode: ExtractionMode, second_stage_short_circuits: bool) -> bool`（`SkipWhenUnused` は取次、`LexiconFallback`、または `second_stage_short_circuits` で `false`、`Always` は常に `true`）
- ログを出すかどうかと出力する ID: `fn suppressed_ack_rule_ids(decision: &AnswerDecision, rules: &[EscalationRule], signals: &SignalSet) -> Option<Vec<String>>`（3.2 の条件を満たすとき昇順の `rule_id` 一覧、満たさないとき `None`）

この場合、`evaluate` はこの 3 関数を呼ぶだけにし、呼び出し箇所ごとの方針（2.2 の表）は各経路の既存テスト、または呼び出し箇所のコードレビューで担保する。どちらの方法を採ったかを実装報告に書く。

## 5. ドキュメント更新

- `specs/production-cs-mcp.md`: 返信文下書きの記述に、経路ごとの生成方針（本書への参照）を追記する。
- `2026-10-05-initial-cost-handoff-design.md`: §4.1 に、宣言文を使う取次では返信文下書きも生成されないこと（本書への参照）を追記する。
- プロジェクトの `CLAUDE.md` は変更しない（「evaluate 1 回につき Anthropic 呼び出しが 1 回増える」の記述の更新は、実装完了後にリポジトリ管理者が判断する。実装報告にその旨を書く）。
