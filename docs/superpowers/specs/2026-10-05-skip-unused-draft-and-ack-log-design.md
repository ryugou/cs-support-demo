# 取次時の未使用下書きの生成停止と、宣言文不使用のログ

| 項目    | 内容 |
| ----- | ---- |
| 目的    | 応答生成 API の取次で使われない返信文下書きの LLM 生成を止める（Issue #78）。`customer_ack` が複数ルールのマッチで使われなかったことを観測できるようにする（Issue #79） |
| 読者    | 実装者、レビュアー |
| 正本の範囲 | `Harness::evaluate` の下書き生成方針 `ReplyDraftPolicy`、各呼び出し経路が渡す値、宣言文不使用ログと重複ルール警告の出力条件・項目 |
| 関連文書  | [`2026-10-05-initial-cost-handoff-design.md`](2026-10-05-initial-cost-handoff-design.md)（`customer_ack` と適用条件の正本）、[`../../../specs/production-cs-mcp.md`](../../../specs/production-cs-mcp.md)（3 層判定の正本） |

## 1. 採用する設計

1. `Harness::evaluate` に下書き生成方針 `ReplyDraftPolicy` を引数として追加する。応答生成 API とアドバイザの経路は「使われない下書きは生成しない」（取次、または LLM 分類失敗）を、MCP の経路は「常に生成する」を渡す。
2. `Harness::evaluate` は、第 1 層の取次で `customer_ack` が複数マッチにより使われなかったとき、ログを 1 行出す。
3. `load_escalation_rules` は、同じ `rule_id` のルールを複数読み込んだとき、警告を 1 行出す。

判定（`decide`）・応答文・MCP tool の入出力の形は変更しない。

## 2. 下書き生成方針（Issue #78）

### 2.1 型

`server/src/harness/mod.rs` に定義する。

```rust
/// `Harness::evaluate` が返信文下書き（`customer_reply_draft`）を生成する条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyDraftPolicy {
    /// 判定によらず生成する。
    Always,
    /// 呼び出し元が使わないと分かっている下書きは生成しない。次のいずれかのとき生成しない。
    /// - 判定が取次（`AnswerDecision::Escalate`）
    /// - 抽出モードが `ExtractionMode::LexiconFallback`（判定によらない）
    SkipWhenUnused,
}
```

既定値は設けない。`evaluate` の全呼び出し箇所が明示的に渡す。

### 2.2 各経路が渡す値

| 呼び出し箇所 | 渡す値 | 理由 |
| --- | --- | --- |
| `server/src/rmcp_server.rs`（MCP `evaluate_answerability`） | `Always` | 応答の `customer_reply_draft` は出力契約であり、取次時も返す |
| `server/src/api.rs`（`/{project_id}/api/reply`） | `SkipWhenUnused` | 取次時は受け止め文と決定的ブロック、または聞き返しで応答を組み立て、`LexiconFallback` 時は判定によらず取次応答へ倒す（`decide_reply_action`）ため、いずれも下書きを使わない |
| `server/src/advisor/cs_support.rs`（homesec 経由） | `SkipWhenUnused` | 同上 |

### 2.3 `evaluate` の挙動

判定が確定し監査記録（`audit_with_nodes`）を終えた後、下書き生成の直前で次を判定する。

- 方針が `SkipWhenUnused` で、判定が `AnswerDecision::Escalate` である、または抽出モードが `ExtractionMode::LexiconFallback` のとき: `draft_customer_reply` を呼ばない。`EvaluationOutcome.customer_reply_draft` は `None`、`customer_reply_draft_truncated` は `false` にする。
- それ以外: 現行どおり `draft_customer_reply` を呼ぶ。

`LexiconFallback` で下書きを作らないのは、応答生成 API とアドバイザが LLM 分類失敗のターンを判定（`Allowed` を含む）によらず取次応答にするためである。抽出モードは下書き生成より前に確定している。

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
| `harness/mod.rs`（`evaluate`） | `SkipWhenUnused` で取次になるターン、または `LexiconFallback` のターンは drafter のリクエストが 0 件で、`customer_reply_draft` が `None`、`customer_reply_draft_truncated` が `false` |
| `harness/mod.rs`（`evaluate`） | `SkipWhenUnused` で回答（`Allowed`）かつ `LexiconFallback` 以外のターンは下書きが生成される |
| `harness/mod.rs`（`evaluate`） | `Always` で取次になるターンは下書きが生成される |
| `harness/mod.rs`（`evaluate`） | 3.2 の 3 条件を満たすときログが 1 行出て `matched_rule_ids` が昇順で含まれる。マッチ 1 件のとき、および複数マッチでも宣言を持つルールが無いときは出ない。ログに発話本文が含まれない |
| `harness/rules.rs` | `matching_layer1_rules` が 0 件・1 件・複数件を返し、空条件のルールを含めない。`count_layer1_matches` の既存テストが変更なしで通る |
| `harness/knowledge.rs` | 重複した `rule_id` を含む入力で警告が 1 行出て、重複 ID が昇順で含まれる。重複が無いときは出ない |

`evaluate` を通すテストの土台（vegapunk クライアントのスタブ等）が既存テストに無く、新たな仕組みを作らないと書けない場合は、該当する判定部分を純関数に切り出して単体テストする。切り出す関数は次の 2 つとする。

- 下書きを生成するかどうか: `fn should_draft_reply(policy: ReplyDraftPolicy, decision: &AnswerDecision, extraction_mode: ExtractionMode) -> bool`（`SkipWhenUnused` は取次または `LexiconFallback` で `false`、`Always` は常に `true`）
- ログを出すかどうかと出力する ID: `fn suppressed_ack_rule_ids(decision: &AnswerDecision, rules: &[EscalationRule], signals: &SignalSet) -> Option<Vec<String>>`（3.2 の条件を満たすとき昇順の `rule_id` 一覧、満たさないとき `None`）

この場合、`evaluate` はこの 2 関数を呼ぶだけにし、呼び出し箇所ごとの方針（2.2 の表）は各経路の既存テスト、または呼び出し箇所のコードレビューで担保する。どちらの方法を採ったかを実装報告に書く。

## 5. ドキュメント更新

- `specs/production-cs-mcp.md`: 返信文下書きの記述に、経路ごとの生成方針（本書への参照）を追記する。
- `2026-10-05-initial-cost-handoff-design.md`: §4.1 に、宣言文を使う取次では返信文下書きも生成されないこと（本書への参照）を追記する。
- プロジェクトの `CLAUDE.md` は変更しない（「evaluate 1 回につき Anthropic 呼び出しが 1 回増える」の記述の更新は、実装完了後にリポジトリ管理者が判断する。実装報告にその旨を書く）。
