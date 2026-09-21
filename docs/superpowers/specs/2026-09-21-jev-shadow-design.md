# Jev(TypeSafe System One)シャドー運転

| 項目 | 内容 |
| --- | --- |
| 目的 | 顧客発話の「理解」を型付き + 確率で得る外部判断器(Jev)を、本番の判定を変えずに並行実行し、較正を測るための記録を残す |
| 読者 | 実装エージェント、レビュー担当 |
| 正本の範囲 | Jev クライアントの契約、質問定義の置き場、起動点と fail-soft 規律、記録の形、config と secret |
| 関連文書 | `2026-08-11-answer-api-line-adapter-design.md`(/api/reply 契約)、GitHub Issue #56 |

## 1. 採用する設計

Jev は「解釈」だけを担い、判定はコードに残す(既存の原則どおり)。本タスクでは判定に一切使わず、**並行実行して記録するだけ**とする。

- 呼び出し: `POST https://api.typesafe.ai/v1/systemone`、`Authorization: Bearer <TYPESAFE_API_KEY>`
- リクエスト: `{ "state": <顧客発話>, "model": "jev-latest", "questions": <質問定義> }`
- レスポンス: `{ "model", "answers": { <id>: { "type": "noul"|"choice"|"score", ... } }, "usage": { "input_tokens", "output_tokens" } }`
  - noul: `noul`(0〜1)
  - choice: `choice` / `confidence` / `probabilities`
  - score: `score` / `confidence` / `probabilities` / `legend`
- 既知の実測値(2026-09-21、日本語 12 件): 全件 200、0.49〜0.67 秒。5 問で入力 945・出力 220 トークン、14 問で入力 1,221・出力 385 トークン、レイテンシは不変

## 2. 実装

- `server/src/jev.rs`(新規)
  - `JevClient::from_config(&JevConfig) -> Result<Option<Self>>`: `enabled = false` なら `None`、`enabled = true` で鍵が解決できなければ起動時 `Err`(`AnthropicClient::from_config` と同じ流儀)
  - `async fn evaluate(&self, state: &str) -> Result<JevOutcome>`: 質問定義は構築時に読み込んで保持する
  - タイムアウトは config(既定 3 秒)。エラーは型で返し、呼び出し側が握りつぶす
- 質問定義: `server/data/urtect/jev-questions.json`(イメージ同梱)。パスは config の `[jev] questions_path`
- 起動点: `/{project_id}/api/reply` の入力検証を通った直後。**`tokio::spawn` で fire-and-forget**(応答を待たない。ターン永続化と同じ規律)
- 記録: `tracing::info!` の構造化ログ 1 行。`request_id` / `case_id`(無ければ空)/ 各 answer の値・信頼度・上位確率 / 所要ミリ秒 / 入出力トークン。**顧客発話の本文はログに出さない**(request_id で監査ログと突合する)
- config:
  ```toml
  [jev]
  enabled = false
  endpoint = "https://api.typesafe.ai/v1/systemone"
  model = "jev-latest"
  questions_path = "data/urtect/jev-questions.json"
  timeout_secs = 3
  ```
  鍵は env `TYPESAFE_API_KEY`(本番は Secret Manager `typesafe-api-key`)

## 3. 不変条件

1. Jev の成否・遅延が、応答内容と応答時間に影響しない(無効時・障害時・タイムアウト時とも)
2. Jev の結果を判定・分岐・生成プロンプトに一切渡さない(本タスクの範囲)
3. 顧客発話の本文をログに書かない
4. `[jev] enabled = false` の構成では Jev へ一切アクセスしない

## 4. テスト

- 無効時: クライアントが構築されず、呼び出しも発生しない
- 有効時: スタブで成功・HTTP エラー・タイムアウトを与え、いずれも応答本文と `reply_kind` が不変であること
- パース: noul / choice / score の 3 型、未知フィールド、壊れた JSON(失敗として扱い応答に影響しない)
- ログ: 顧客発話の本文が出力に含まれないことを固定

## 5. 運用

- 本番有効化の手順: secret `typesafe-api-key` を作成 → ランタイム SA へ `secretAccessor` を付与 → service へ注入 → `config.cloudrun.toml` の `[jev] enabled = true`
- 顧客の発話が第三者(TypeSafe)へ送信される。CLAUDE.md の Anthropic に関する記載と同じ扱いで明記する

## 6. 次段階(本タスクの範囲外)

実会話 1〜2 週間の記録で較正を確認したのち、判定の置き換えへ進む。初期の閾値案(すべてコード側):

- 緊急: 0.5 以上で安全案内(見落としのコストが高いため低めに置く)
- 故障: 0.8 以上 かつ 情報十分 0.6 以上 → エスカレーション / 0.8 以上 かつ情報不足 → ヒアリング / 0.3〜0.8 → 通常の回答を試みる
- 製品: 信頼度 0.8 以上 → 確定 / 0.4〜0.8 → 型番の確認 / 未満 → 言及なし。他社製品の断りは 0.8 以上のときだけ
- 担当者要望: 0.8 以上

複数の型番に言及されたケースは Choice では 1 つしか返らないため(実測: V724 0.84 / V523 0.05)、置き換え時は**型番ごとの Noul** に変更する。型番の音写(「びーけーごーにーさん」)は解決できない(実測: 製品なし 0.93)。
