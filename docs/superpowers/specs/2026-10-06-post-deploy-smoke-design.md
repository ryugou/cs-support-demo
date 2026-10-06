# デプロイ後スモークの機械化

| 項目    | 内容 |
| ----- | ---- |
| 目的    | デプロイ後に人手で行っている本番確認を、1 コマンドで実行できる検証 CLI に置き換える（Issue #40） |
| 読者    | 実装者、レビュアー、デプロイ後に確認を行う運用者 |
| 正本の範囲 | 検証 CLI `verify_deploy` の検査項目・入力・出力・終了コード、期待値ファイルの形式 |
| 関連文書  | [`../../../specs/production-cs-mcp.md`](../../../specs/production-cs-mcp.md)（3 層判定の正本）、[`2026-08-16-admin-dashboard-design.md`](2026-08-16-admin-dashboard-design.md)（管理 API の正本） |

## 1. 採用する設計

検証 CLI `server/src/bin/verify_deploy.rs` を追加し、同一イメージの Cloud Run job `verify-deploy` として実行する。CLI は本番の vegapunk に対して**読み取りだけ**を行い、次の 2 種類を検査する。

1. **管理 API の読み出し検査**: 管理 API が使う読み出しを、HTTP を介さず同じ関数で実行し、本番のデータ規模で失敗しないことを確かめる。
2. **第 1 層判定の検査**: 期待値ファイルに書いた発話を、イメージに同梱された lexicon と vegapunk に投入済みのルールで判定し、期待どおりの第 1 層ルールにマッチすること（またはどのルールにもマッチしないこと）を確かめる。

2 は「lexicon はイメージに同梱、ルールは `ingest-rules` で vegapunk に投入」という反映経路の違いから生じる不整合（ルールの投入漏れ、lexicon とルールの食い違い）を検出する。

CLI は vegapunk へ書き込まない。case・会話ターン・監査イベントを作らない。LLM を呼ばない。

## 2. 入力

### 2.1 引数

| 引数 | 必須 | 内容 |
| --- | --- | --- |
| `--target <config_path>:<project_id>` | 必須（複数指定可） | 検査する対象。サービスと同じ形式の設定ファイルのパスと、その設定ファイルに定義された project_id を、**最後の `:`** で区切って指定する。同じ設定ファイルを複数の対象で指定してよい。project_id が空、`:` が無い、設定ファイルに project が無い、同じ project_id が複数の対象に現れる場合はエラーにする |
| `--expectations <path>` | 省略可 | 期待値ファイル。省略時は第 1 層判定の検査を行わない |
| `--expectations-project <project_id>` | `--expectations` 指定時は必須 | 期待値ファイルの対象。`--target` のいずれかの project_id と一致しなければならない |

vegapunk の認証情報は、サービスと同じ環境変数（`VEGAPUNK_BEARER_TOKEN` / `VEGAPUNK_BEARER_TOKEN_FILE`）から 1 つ読み、すべての対象で使う。

### 2.2 期待値ファイル

`server/data/urtect/smoke-expectations.json` に置く。

```json
{
  "layer1": [
    { "utterance": "初期費用はいくらですか？", "expect_rule": "initial-cost-quote" },
    { "utterance": "月額いくらですか？", "expect_rule": null }
  ]
}
```

- `expect_rule`: マッチすべき第 1 層ルールの `rule_id`。どのルールにもマッチしないことを期待する場合は `null`。
- 未知のフィールドは拒否する（`deny_unknown_fields`）。`layer1` が空の場合はエラーにする。

初期の内容は、次の発話とする。期待値は現在の `server/data/urtect/signal-lexicon.json` と `server/data/urtect/rules.json` に対する正しい結果である。

| 発話 | `expect_rule` |
| --- | --- |
| 月額いくらですか？ | `null` |
| 契約前に料金を知りたいです | `null` |
| 初期費用はいくらですか？ | `initial-cost-quote` |
| 解約したいです | `contract-billing` |
| 請求額が違います | `contract-billing` |
| 担当者につないでください | `human-handoff` |

## 3. 検査項目

### 3.1 管理 API の読み出し検査（project ごと）

管理 API（`server/src/admin.rs`）のハンドラが呼んでいる読み出し関数を、同じ引数の組み立てで直接呼ぶ。HTTP サーバは起動しない。

| 検査 | 内容 | 合格条件 |
| --- | --- | --- |
| `threads` | スレッド一覧の 1 ページ目を取得する | エラーにならない |
| `thread_detail` | `threads` の先頭のスレッドについて、詳細（ターン、会話状態、case の signal）を取得する | エラーにならない。スレッドが 0 件の場合は `skipped` とし、不合格にしない |
| `stats` | 統計を既定の期間で取得する | エラーにならない |

新たに全域の読み出し（`GetGraphSnapshot` 等）を追加しない。管理 API が現在使っている読み出しだけを使う。

### 3.2 第 1 層判定の検査（期待値ファイルを指定した project のみ）

1. その project の schema から、サービスと同じ関数でエスカレーションルールと禁止領域を読む（`load_escalation_rules`、`load_prohibited_domains`）。
2. 設定ファイルが指す lexicon を、サービスと同じ方法で読む。
3. 読んだルールと禁止領域の signal を、サービスの評価経路と同じ関数（`Harness::validate_rule_vocabulary`）で lexicon と突合する。語彙に無い signal を参照するルールが 1 つでもあれば、発話を評価せずに `layer1` を `fail` にし、`detail` にエラー文（ルール id と signal 名。発話は含まない）を入れる。理由: サービスはこの突合で全評価を fail closed にするため、期待値の 6 発話がそのルールを踏まなくても、本番は全件失敗している。突合を省くと、この検査が最も検出したい lexicon とルールの不一致を見逃す。
4. 期待値ファイルの各発話を lexicon だけで signal に変換し（LLM を使わない）、`match_layer1` の結果を `expect_rule` と比べる。

1 件ごとに、発話・立った signal・マッチしたルール・期待値・合否を記録する。

`validate_rule_vocabulary` は `Harness` の非公開メソッドなので、crate 内から呼べる可視性（`pub(crate)`）に広げて再利用する。CLI に突合ロジックを複製しない（§5）。

期待値ファイルは 1 つの project に対するものである。対象は `--expectations-project` で 1 つに決める（§2.1）。

## 4. 出力と終了コード

標準出力に JSON を 1 つ出す。

```json
{
  "passed": false,
  "projects": [
    {
      "config": "/app/server/config.cloudrun.toml",
      "project_id": "urtect",
      "schema": "urtect",
      "checks": [
        { "name": "threads", "status": "pass", "detail": { "count": 20 } },
        { "name": "thread_detail", "status": "pass", "detail": { "turns": 6 } },
        { "name": "stats", "status": "pass", "detail": {} },
        { "name": "layer1", "status": "fail", "detail": { "total": 6, "failed": 1 } }
      ],
      "layer1": [
        { "utterance": "初期費用はいくらですか？", "signals": ["initial_cost_question", "price_question"], "matched_rule": null, "expect_rule": "initial-cost-quote", "status": "fail" }
      ]
    }
  ]
}
```

- `projects` の各要素の `config` は、`--target` で渡された設定ファイルのパスの文字列である。
- `status` は `pass` / `fail` / `skipped`。
- `detail` に顧客の発話本文・会話の内容を出さない。件数だけを出す。`layer1` の `utterance` は期待値ファイルに書いた検査用の発話であり、顧客の発話ではない。
- 1 つでも `fail` があれば `passed` は `false`、終了コードは 1。すべて `pass` または `skipped` なら 0。
- 検査の失敗は、原因（どの読み出しが、どのエラーで失敗したか）を標準エラーへ `tracing::error!` で出す。1 つの検査が失敗しても、残りの検査は続行する。
- 引数や設定の誤りは、JSON を出さずにエラーで終了する（終了コード 1）。
- 検査の前に、対象 project ごとに疎通確認（preflight）を行う。読み出しは `ConversationTurn` の 1 件取得（`threads` 検査が使う `load_conversation_turns_page` を `limit` 1 で呼ぶ。`Search` と書き込みは使わない）。1 つでも失敗したら、設定ファイル・project・schema・接続先・エラー・エラー種別に応じた確認先（認証情報の環境変数、接続先、VPC）を標準エラーへ `tracing::error!` で出し、JSON を出さずに終了する（終了コード 1）。基盤障害・認証設定の誤りを、スモークの不合格と区別するためである。
- 疎通確認が全 project で成功した後に起きた個別の失敗は、検査の `fail` として JSON に記録する。
- vegapunk への接続は、サービス（`main.rs` / `homesec_advisor.rs`）と同じ遅延接続（`connect_lazy_with_limits`）で組み立てる。実際の接続は疎通確認の 1 件読み出しで初めて起きる。即時接続（`connect_with_limits`）にすると、VPC や接続先の障害で疎通確認に到達する前にランタイムの組み立てで終了し、上記の project・schema・確認先つきの診断が出ず、残りの対象の確認も行われない。接続先の構文誤りや token のメタデータ化の失敗は、遅延接続でも組み立て時のエラーのままでよい。

## 5. 不変条件

- CLI は vegapunk へ書き込まない。
- CLI は LLM・Jev などの外部サービスを呼ばない。
- 判定の検査は、サービスが使うのと同じ関数（lexicon の読み込み、ルールの読み込み、`match_layer1`）を使う。判定ロジックを CLI に複製しない。

### 5.1 サービスの設定を読み取り専用にして使う

CLI はサービスと同じ設定ファイルを読むが、そのまま `Harness::build` に渡さない。本番の設定は LLM と Jev を有効にしており、そのままでは API キーが無いと起動に失敗し、監査ログを書き込み用に開く。CLI は `Harness::build` に渡す前に、次の上書きを行う（`read_only_harness_config`）。

| 設定 | 上書き後 |
| --- | --- |
| `llm.enabled` | `false` |
| `jev.enabled` | `false` |
| `harness.customer_reply_draft_enabled` | `false` |
| `harness.audit_log_path` | 実行ごとの一時ディレクトリの下 |
| `harness.search_improvement_queue_path` | 実行ごとの一時ディレクトリの下 |

lexicon、NG 辞書、project の定義、vegapunk の接続設定は上書きしない。

上書き、`Harness::build`、vegapunk クライアントの生成は、**設定ファイルごとに 1 回**行い、同じ設定ファイルを指す複数の対象は同じ Harness を使う。一時ディレクトリは、全体を 1 つの実行ごとのディレクトリの下に置き、設定ファイルごとに別のサブディレクトリ（`config-<連番>`）を使う。異なる設定の監査ログ・キューのパスが衝突しないようにするためである。Harness はすべて、一時ディレクトリより先に drop される。

`Harness::build` が書き込み用に開くのは `[harness]` の `audit_log_path` と `search_improvement_queue_path` だけである（`server/src/harness/mod.rs` の `Harness::build`）。`config.homesec.toml` の `[advisor]` にある `support_audit_log_path` / `support_search_improvement_queue_path` は `server/src/bin/homesec_advisor.rs` だけが読み、この CLI の経路では開かれないため、上書きの対象にしない。

この上書きにより、job に必要な秘密は vegapunk の認証情報（`VEGAPUNK_BEARER_TOKEN`）だけになる。LLM と Jev の API キーを job に注入しない。一時ディレクトリは CLI の終了時（正常終了・エラー終了とも）に削除される。

## 6. 実行と運用

- Cloud Run job `verify-deploy` は、`merge-schema` と同じ VPC connector・service account・Secret Manager 注入で作成する。作成はリポジトリ管理者が `gcloud run jobs create` で行う。
- `.github/workflows/deploy.yml` の `RUN_JOBS` への追加は、job の実体を作成した後に別の変更として行う（未作成のまま追加するとデプロイ経路全体が止まるため）。本変更では `deploy.yml` を変更しない。
- 実行はデプロイ後に手動で行う。

```sh
gcloud run jobs execute verify-deploy --project sivira-cs-support --region asia-northeast1 --wait
```

- `Dockerfile` に `verify_deploy` バイナリの同梱を追加する（既存の検証 CLI と同じ方法）。
- イメージの ENTRYPOINT は `cs-support-mcp` なので、job の起動コマンドを `/usr/local/bin/verify_deploy` に上書きする。引数は次のとおり。

```sh
--target /app/server/config.cloudrun.toml:urtect --target /app/server/config.homesec.toml:homesec --expectations /app/server/data/urtect/smoke-expectations.json --expectations-project urtect
```

`Dockerfile` は `server/config.cloudrun.toml` と `server/config.homesec.toml` を `WORKDIR /app/server` 直下へ、`server/data` を `/app/server/data` へ同梱する。1 回の実行で urtect と homesec の両方の読み出しを検査する。

- 注入する秘密は `VEGAPUNK_BEARER_TOKEN` のみ（理由は §5.1）。監査ログ用の GCS ボリュームも不要。
- 結果 JSON は標準出力、ログは標準エラーに出る。終了コードは §4。

## 7. テスト

| 対象 | 固定する内容 |
| --- | --- |
| 期待値ファイルの読み込み | 正しい形式を読める。未知のフィールド・空の `layer1` を拒否する |
| 同梱の期待値ファイル | `server/data/urtect/smoke-expectations.json` を、同梱の lexicon と同梱の `rules.json` から復元したルールで判定すると、全件が期待どおりになる |
| 第 1 層判定の検査 | 期待と一致する場合は `pass`、マッチするルールが違う場合・マッチしないはずがマッチした場合・マッチするはずがしない場合は `fail` |
| 語彙の突合 | 語彙に無い signal を参照するルール（または禁止領域）が 1 つでもあれば、発話を評価せずに `layer1` が `fail` になり、`detail` にルール id と signal 名を含むエラー文が入る。全ルールが語彙内なら発話の評価へ進む。突合の関数はサービスの評価経路と同じもの（`Harness::validate_rule_vocabulary`）を呼ぶ |
| 結果の集約 | `fail` が 1 件でもあれば `passed` が `false`。`skipped` は不合格にしない |
| 出力 | JSON に顧客の発話本文・会話の内容を含むフィールドが無い |
| 疎通確認 | 読み出しが成功したら到達可能と分類する。失敗したら、設定ファイル・project・schema・接続先・エラーをメッセージに含め、認証拒否・接続断・その他でそれぞれ確認先が変わる |
| `--target` の解釈 | `<config_path>:<project_id>` を読める。最後の `:` で分割する（パスに `:` を含む場合）。`:` が無い・project_id が空・設定ファイルのパスが空・同じ project_id の重複を拒否する。同じ設定ファイルを複数の対象で指定できる |
| 対象の解決 | 設定ファイルに無い project はエラーになる（`config.homesec.toml` に `homesec` はあり、`config.cloudrun.toml` には無い）。`--expectations-project` がどの対象にも無い場合、`--expectations` と `--expectations-project` の片方だけの場合はエラーになる |
| 設定ファイルごとのまとめ | 同じ設定ファイルの対象が 1 つにまとまり、出現順を保つ |
| 読み取り専用化（両設定） | `config.cloudrun.toml` と `config.homesec.toml` の両方で、監査ログ・検索改善キューが一時ディレクトリの下になり、LLM・Jev・返信文下書きが無効になる |
| 一時サブディレクトリ | 設定ファイルごとに別のサブディレクトリになり、両設定の書き込み先が衝突しない |
| 一時ディレクトリ | スコープを抜けるとディレクトリとその中身が消える。作られていないディレクトリの削除で失敗しない |

管理 API の読み出し検査は実 vegapunk が必要なため、単体テストの対象にしない。検査の組み立て（結果を `pass` / `fail` / `skipped` に分類する部分）を純関数にしてテストする。

## 8. ドキュメント更新

- `CLAUDE.md` の「Cloud Run デプロイ手順」に、job `verify-deploy` の作成が必要であること、デプロイ後に上記コマンドで実行すること、`RUN_JOBS` へ追加する前に実体を作ることを追記する（Issue #40 の完了条件）。
- `CLAUDE.md` の Cloud Run jobs の一覧に `verify-deploy` を加える。

## 9. 対象外

- 回答可否の判定全体（第 2 層・第 3 層、LLM による signal 抽出、返信文の生成）の検査。case と監査イベントを作り、LLM を呼ぶため、読み取り専用の検査にできない。
- CI のデプロイ後ステップへの組み込み。
- reviewer の観点への追記（`~/.claude/agents/reviewer.md` はリポジトリ外で、依頼文をチャットで提示する）。
