# 応答生成 API と LINE アダプタ（デモ向け v1）

| 項目 | 内容 |
| --- | --- |
| 目的 | LINE 経由のクライアント体験検証に向け、顧客メッセージから最終応答文を返す HTTP API と LINE webhook アダプタの仕様を定める |
| 読者 | 実装エージェント、レビュー担当 |
| 正本の範囲 | `/api/reply` の契約、API キー認証、応答文の決定規則、会話履歴の扱い、LINE アダプタの挙動、デプロイ構成 |
| 関連文書 | `specs/production-cs-mcp.md`（harness / evaluate の正本）、GitHub Issue #14（本件）、#13（crate 分離、本件のスコープ外） |

## 1. 採用する設計

既存サーバに axum ルート `/{project_id}/api/reply` を追加し、`Harness::evaluate()`（rmcp 非依存の domain 入口）を直接呼ぶ。判定・応答文生成・egress gate・フォールバックはすべてサーバ内で完結し、レスポンスは「そのまま顧客へ送信できる完成文」だけを返す。

LINE アダプタは同リポジトリの別バイナリ `line_adapter` とし、判断ロジックを持たない: webhook 受信 → 署名検証 → 保存済み履歴と case_id を無加工で添えて API を 1 回コール → 返却された `reply_text` をそのまま LINE へ返信 → 履歴と case_id を更新。

MCP インターフェースは変更しない。両者は同じ harness 入口を共有する。

## 2. API 契約

```
POST /{project_id}/api/reply
Authorization: Bearer <API キー>
Content-Type: application/json
```

リクエスト:

```json
{
  "message": "string",
  "history": [ { "role": "customer" | "assistant", "text": "string" } ],
  "case_id": "string"
}
```

| フィールド | 制約 | 違反時 |
| --- | --- | --- |
| `message` | 必須。trim 後 1〜5,000 字 | 400 |
| `history` | 任意。最大 20 要素。各 `text` は 1〜2,000 字。時系列昇順（古い→新しい） | 400 |
| `case_id` | 任意。最大 128 字 | 400 |

レスポンス（200 のみ）:

```json
{ "reply_text": "string", "case_id": "string" }
```

- `reply_text`: 顧客向け最終応答文。呼び出し側は無加工で送信する
- `case_id`: 次リクエストで渡す値。サーバが発番し vegapunk に永続化する（既存の case 機構）

エラー（body は `{ "error": "<code>", "message": "<説明>" }`）:

| status | code | 条件 |
| --- | --- | --- |
| 400 | `invalid_request` | 上記制約違反、JSON 不正 |
| 401 | `unauthorized` | Authorization 欠落・キー不一致 |
| 404 | `unknown_project` | config に無い `project_id` |
| 503 | `upstream_unavailable` | vegapunk 不達 |
| 500 | `internal` | その他の内部エラー |

未知の `case_id`（load 失敗）はエラーにせず新規 case として処理し、warn ログを出す。

## 3. 認証

- env `CS_SUPPORT_ANSWER_API_KEY`（Secret Manager 注入）と `Authorization: Bearer` の値を定数時間比較する。Google OAuth（`require_google_auth`）はこのルートに適用しない
- config `[api] enabled`（既定 `false`）。`enabled = true` かつキー未設定なら起動時に失敗させる（`CS_SUPPORT_LLM_API_KEY` と同じ fail-closed パターン）。`enabled = false` ならルートを登録しない
- 認証通過後は固定のサービス principal（`sub = "service:answer-api"`、`email = "answer-api@cs-support.internal"`、`email_verified = true` 相当の VerifiedIdentity）を `harness.begin()` に渡す。actor 解決は既存の `lookup_by_identity` をそのまま使う（本 API が外部へ公開する操作は evaluate による応答生成のみであり、supervisor 専用操作は公開しない）。監査 actor はこのサービス principal になる

## 4. 応答文の決定

| evaluate の結果 | `reply_text` |
| --- | --- |
| allowed かつ下書きあり | `customer_reply_draft`（LLM 生成・egress gate 通過済み）をそのまま使う |
| allowed かつ下書き null（LLM 失敗・egress 却下） | `[api] fallback_reply_text` |
| escalate | `[api] fallback_reply_text` |
| rule_match | `[api] fallback_reply_text` |

- `[api] fallback_reply_text` の既定値: 「お問い合わせありがとうございます。担当者が確認のうえ、あらためてご連絡いたします。」
- decision・evidence はレスポンスに含めない。判定内訳は既存の audit log（`audit_event_id`）で追跡する
- フォールバックに落ちた場合は warn ログに `request_id` / 判定 / 理由を出す

## 5. 会話履歴の扱い

- v1 は呼び出し側（LINE アダプタ）が原文の履歴を保持し、リクエストの `history` で渡す。サーバは履歴を保存しない
- サーバ側の使用規則: 新しい側から最大 6 ターン・合計 4,000 字まで採用し、超過分は古い側から捨てる。採用した履歴は応答文生成プロンプトの会話履歴ブロックにのみ注入する
- 履歴は**生成にのみ**使う。evaluate の判定（signal 抽出・escalation 判定・signal 累積）には使わない。判定のターン間文脈は既存の case 機構（`case_id` による signal 累積）が担う
- egress gate は履歴注入後の生成物にも従来どおり適用する
- 運用注意: 履歴注入により Anthropic への送信量が最大で約 4,000 字ぶん増える（顧客本文の外部送信に関する既存の運用注意の適用範囲が広がる）

## 6. LINE アダプタ

`server/src/bin/line_adapter.rs`。独立した axum アプリで、ルートは `POST /line/webhook`、`GET /healthz`、`GET /livez`。

env（`LINE_CHANNEL_SECRET` / `LINE_CHANNEL_ACCESS_TOKEN` / `CS_ANSWER_API_URL` / `CS_ANSWER_API_KEY` は必須。欠落時は起動失敗）:

| env | 用途 |
| --- | --- |
| `LINE_CHANNEL_SECRET` | webhook 署名検証 |
| `LINE_CHANNEL_ACCESS_TOKEN` | LINE Reply API 呼び出し |
| `CS_ANSWER_API_URL` | 応答生成 API の完全 URL（`https://<host>/urtect/api/reply`） |
| `CS_ANSWER_API_KEY` | 応答生成 API の Bearer キー |
| `CS_LINE_FALLBACK_TEXT` | 任意。API 失敗時の詫び文。既定「申し訳ありません。ただいま応答できません。時間をおいてもう一度お試しください。」 |
| `CS_LINE_NONTEXT_TEXT` | 任意。非テキスト受信時の案内。既定「恐れ入りますが、テキストでお送りください。」 |

処理順（webhook 1 リクエストにつき）:

1. `X-Line-Signature` を channel secret の HMAC-SHA256（base64）で定数時間比較する。不一致は 400
2. イベントを順に処理する。`message` かつ `text` 以外のメッセージは `CS_LINE_NONTEXT_TEXT` を返信、`message` 以外のイベントは無視
3. テキストイベント: セッションストアから該当 user の履歴・case_id を取り、API を 1 回コール（タイムアウト 50 秒）
4. 200 なら `reply_text` を Reply API で返信し、履歴に `customer` / `assistant` の 2 ターンを追記、`case_id` を保存。非 200・タイムアウトなら `CS_LINE_FALLBACK_TEXT` を返信し、履歴と case_id は変更しない
5. 全イベント処理後に 200 を返す

セッションストア（プロセス内メモリ）:

- `user_id → { case_id, history（上限 20 ターンのリング）, last_at }`
- 最終アクセスから 60 分で破棄（アクセス時 + 定期スイープ）。エントリ上限 10,000、超過時は `last_at` 最古を破棄
- プロセス再起動で消える。その場合は新規 case として継続する（履歴・文脈が切れることをデモの許容事項とする）

LINE Reply API が失敗した場合は error ログを出して当該イベントを終了する（push 再送は行わない）。

## 7. 設定・Secret・デプロイ

config 追加（`[api]` セクション）:

| キー | 既定 | cloudrun |
| --- | --- | --- |
| `enabled` | `false` | `true` |
| `fallback_reply_text` | 第 4 節の既定文 | 同左 |

Secret Manager 追加（すべて `openssl rand -base64 32` 相当以上の強度、または LINE 発行値）:

- `cs-support-answer-api-key` → service の `CS_SUPPORT_ANSWER_API_KEY` と LINE アダプタの `CS_ANSWER_API_KEY` に注入
- `line-channel-secret` / `line-channel-access-token` → LINE アダプタに注入

デプロイ:

- 応答生成 API: 既存 service `cs-support-mcp` に同居（イメージ更新のみ）
- LINE アダプタ: 新規 Cloud Run service `cs-support-line`。同一イメージ、`command = /usr/local/bin/line_adapter`。公開 ingress。VPC connector 不要（API へ公開 URL で到達）。LINE Developers console の webhook URL に `https://<cs-support-line の URL>/line/webhook` を設定する

## 8. テスト

- API: 認証（ヘッダ欠落 / 不一致 / 一致）、入力制約（400 の各条件）、応答文決定表の 4 行（decision → reply_text を純関数として切り出して検証）、履歴の採用規則（6 ターン・4,000 字・古い側から破棄）、エラー分類（503 / 500 の判別関数）。`Harness::evaluate()` を通す全経路と未知 case_id の新規化は実 vegapunk が必要なためユニットテストの対象外とし、デプロイ後 E2E で検証する
- LINE アダプタ: 署名検証（正・不正・欠落）、イベント振り分け（テキスト / 非テキスト / 非 message）、セッション TTL・上限・再起動相当（新規ストア）の挙動、API 非 200 時のフォールバック文
- E2E（デプロイ後）: `curl` で `/api/reply` の 200 / 401 / 400 を確認後、実 LINE でテキスト往復とマルチターン（case_id 維持）を確認

## 9. スコープ外（v1）と次フェーズ

- 会話履歴の AI サマライズ: 次フェーズでサーバ側に実装する（case に紐づく要約をサーバが更新し、アダプタは `message` + `case_id` のみ送る形へ移行する。要約もプロンプト予算・egress の管理下に置くため、アダプタ側では行わない）。Issue 起票済み
- 雑談 triage の高度化（v1 では回答不能 → フォールバック文で吸収する）
- 担当者個人単位の identity・権限（actor 突合表 DB 化に従属）
- crate / workspace 分離(#13)
- アダプタのセッション永続化、push 再送、非テキストメッセージの内容処理
