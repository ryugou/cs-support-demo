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

**`history` の入力契約（呼び出し側の責務。サーバは検証できない）**: `history` は**同一 case・同一相談対象**の時系列履歴に限る。相談対象（製品・事象）が変わったら、呼び出し側は `history` を空にし `case_id` も渡さず、新しい相談として送ること。守られない場合、古い製品名 + 今ターンの症状の組み合わせで Jev の `has_enough_info` が不当に上がり、誤った製品文脈で即時エスカレーションしうる（`[jev] enabled = true` のときのみ。§5 の例外と `2026-09-21-jev-shadow-design.md` §7 を参照）。

レスポンス（200 のみ）:

```json
{ "reply_text": "string", "case_id": "string" }
```

- `reply_text`: 顧客向け最終応答文。呼び出し側は無加工で送信する。**必ずプレーンテキスト**（LINE は Markdown を描画しないため）。保証は二段構え: 生成プロンプトで Markdown 記法を禁止し、さらに `/api/reply` の応答確定点で**コードによるプレーンテキスト正規化**を必ず通す（太字/斜体マーカー `**`・`__`・`*`・`_` の対除去、行頭 `#` 見出し記号の除去、インラインコード・コードフェンスの除去、`[text](url)` → `text（url）`、行頭の `-`/`*` 箇条書きを「・」へ。改行は保持）。この正規化は生成物・フォールバック定型文を問わず全応答に適用する。MCP 経路（CS 担当が検分する下書き）には適用しない
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

この fallback は `/api/reply` の契約としてのみ定義する。既存 MCP tool
`evaluate_answerability`（`rmcp_server.rs`）は対象外で、未知 `case_id` は従来どおり Err
にする（`Harness::evaluate` の `allow_unknown_case_id` 引数で経路ごとに分岐する）。CS 担当が
MCP 経由で case_id を打ち間違えたときに黙って新規 case へ合流すると、会話層の累積 signal
（エスカレーション判定の根拠）が失われたまま気づけなくなるため。

## 3. 認証

- env `CS_SUPPORT_ANSWER_API_KEY`（Secret Manager 注入）と `Authorization: Bearer` の値を定数時間比較する。Google OAuth（`require_google_auth`）はこのルートに適用しない
- config `[api] enabled`（既定 `false`）。`enabled = true` かつキー未設定なら起動時に失敗させる（`CS_SUPPORT_LLM_API_KEY` と同じ fail-closed パターン）。`enabled = false` ならルートを登録しない
- 認証通過後は固定のサービス principal（`sub = "service:answer-api"`、`email = "answer-api@cs-support.internal"`、`email_verified = true` 相当の VerifiedIdentity）を `harness.begin()` に渡す。actor 解決は既存の `lookup_by_identity` をそのまま使う（本 API が外部へ公開する操作は evaluate による応答生成のみであり、supervisor 専用操作は公開しない）。監査 actor はこのサービス principal になる

## 4. 応答文の決定

| evaluate の結果 | `reply_text` |
| --- | --- |
| allowed かつ下書きあり | `customer_reply_draft`（LLM 生成・egress gate 通過済み）をそのまま使う |
| allowed かつ下書き null（LLM 失敗・egress 却下） | `[api] fallback_reply_text` |
| allowed かつ下書きが truncated（生成上限で途中切断） | `[api] fallback_reply_text`（切れた文を顧客へ送らない。§4 の他行と同じくフォールバック扱い） |
| escalate | `[api] fallback_reply_text` |
| rule_match | `[api] fallback_reply_text` |

- `[api] fallback_reply_text` の既定値: 「お問い合わせありがとうございます。担当者が確認のうえ、あらためてご連絡いたします。」
- truncated な下書きを捨てる理由: 生成上限で途中切断された下書きは、切れ目がたまたま「。」の直後に落ちると完成文に見え、日本語のビジネス文では末尾に来る安全上の但し書きだけが欠落しうる（`llm.rs` の `ReplyDraft` doc コメント）。MCP 経路は人間の CS 担当が下書きを検分してから送るため `truncated=true` を返すだけで足りるが、`/api/reply` は人間の検分が一切入らない自動送信経路のため同じ扱いにはできない
- decision・evidence はレスポンスに含めない。判定内訳は既存の audit log（`audit_event_id`）で追跡する
- フォールバックに落ちた場合は warn ログに `request_id` / 判定 / 理由 / `draft_truncated` を出す（truncation が原因か運用者が切り分けられるようにする）

## 5. 会話履歴の扱い

- v1 は呼び出し側（LINE アダプタ）が原文の履歴を保持し、リクエストの `history` で渡す。サーバは履歴を保存しない
- サーバ側の使用規則: 新しい側から最大 6 ターン・合計 4,000 字まで採用し、超過分は古い側から捨てる。採用した履歴は応答文生成プロンプトの会話履歴ブロックにのみ注入する
- 履歴は原則**生成にのみ**使う。evaluate 本体の判定（signal 抽出・第1〜3層の escalation 判定・signal 累積）には使わない。判定のターン間文脈は既存の case 機構（`case_id` による signal 累積）が担う。**例外**: Issue #58 で導入した、ヒアリング契約 `product_and_symptom` を宣言した第1層ルール（`warranty-failure`）の聞き返し判定（Jev の `has_enough_info`）に限り、顧客発話の履歴が「聞き返し（Clarify）か即エスカレーション（EscalationReply）か」の分岐材料になる。この経路は `[jev] enabled = true` のときだけ動く（現状どの config も `false`）。state の組み立て規律・閾値・fail-back・監査の正本は `docs/superpowers/specs/2026-09-21-jev-shadow-design.md` §7 とする
- egress gate は履歴注入後の生成物にも従来どおり適用する
- 運用注意: 履歴注入により Anthropic への送信量が最大で約 4,000 字ぶん増える（顧客本文の外部送信に関する既存の運用注意の適用範囲が広がる）
- 運用注意: `[jev] enabled = true` のとき、ヒアリング契約 `product_and_symptom` を宣言した第1層ルール（`warranty-failure`）にマッチしたターンに限り（`contract-billing` にマッチしたターンでは送信しない）、今ターンの顧客発話に加えて過去の顧客発話（最大 6 件・合計 2,000 字まで、時系列昇順。発話は切り詰めず、超過分は古い側から丸ごと落とす）が前置されて TypeSafe（Jev）へ送信される。assistant 発話は送信しない（組み立ては `server/src/api.rs` の `build_jev_state`）

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
3. テキストイベント: まず**同一ユーザーのセッションロックを取得**し、その直後に LINE の chat loading API（`POST https://api.line.me/v2/bot/chat/loading/start`、`chatId` = source.userId、`loadingSeconds` = 60）を呼んで処理中アニメーションを表示する（ロック取得後に呼ぶ理由は下記の直列化の項を参照。ロック取得前に呼ぶと、同一ユーザーの並行イベントの処理順が chat loading API の応答速度で決まってしまい、会話履歴・signal 累積・case_id 確定の順序が入れ替わりうる）。**この呼び出しの失敗は warn ログのみで処理を継続する**（表示は体験改善であり必須機能ではない）。続けてロック済みセッションから該当 user の履歴・case_id を取り、API を 1 回コール（タイムアウト 50 秒）
4. API が 200 を返した時点で、`case_id` と顧客発話（customer ターン）を直ちにセッションへ保存する。サーバ側では 200 の時点で case が確定し signal が追記済みのため、以降の発話を同じ case に必ず合流させる（LINE 返信の成否でこの保存を左右させると、返信失敗時に次の発話が新規 case となり蓄積 signal が判定から脱落する）。その後 `reply_text` を Reply API で返信し、**返信成功時のみ** assistant ターンを履歴へ追記する（顧客が受信していない発話を履歴に残さない）。API が非 200・タイムアウトの場合は `CS_LINE_FALLBACK_TEXT` を返信し、セッションは変更しない
5. 全イベント処理後に 200 を返す

イベントは 1 リクエスト内・同一ユーザーとも逐次処理する（並行処理しない）。そのため 1 イベントあたりのタイムアウトは最大で約 103 秒（chat loading API 呼び出し、per-request timeout 3 秒・失敗/タイムアウトしても継続 + 応答生成 API 呼び出し 50 秒 + LINE Reply API 呼び出し分）まで累積しうる。chat loading API は「失敗しても warn ログのみで継続する」設計（手順3）のため、ここに `state.http` の既定 timeout（50 秒）をそのまま使うと LINE reply token の実効予算（実測往復 10〜15 秒、第 8 節）を食い潰しうる。これを避けるため chat loading API 呼び出しだけ短い per-request timeout（3 秒、`server/src/bin/line_adapter.rs` の `DEFAULT_LINE_LOADING_TIMEOUT`）を明示的に掛けている。この累積は v1 の Accepted Risk として受容し、デプロイ後の E2E（第 8 節）で実測した往復時間をもとに妥当性を再評価する。

なおこの約 103 秒は 1 イベント単体の上限であり、**同一ユーザーの先行イベントのロック解放待ちは含まない**（`SessionStore::lock_session` はロック取得時点からイベント処理全体を直列化するため、後続イベントの実待ち時間は同一ユーザーのキュー長に比例して増える）。これも v1 の Accepted Risk として受容し、第 8 節の E2E で同一ユーザー連続発話時の reply token 失効と webhook 再送の有無を実測して再評価する。

セッションストア（プロセス内メモリ）:

- `user_id → { case_id, history（上限 20 ターンのリング）, last_at }`。保存・送信する履歴 1 ターンの text は各 2,000 字上限へ切り詰める（`/api/reply` の入力契約, 第 2 節の `history[].text` 上限に合わせた切り詰め。超過分をそのまま保存すると次回リクエストが必ず 400 になるため）
- 最終アクセスから 60 分で破棄（アクセス時 + 定期スイープ）。エントリ上限 10,000、超過時は `last_at` 最古を破棄
- プロセス再起動で消える。その場合は新規 case として継続する（履歴・文脈が切れることをデモの許容事項とする）

LINE へ返信する `reply_text` は LINE プラットフォームの 1 メッセージあたりの文字数上限に対応するため 4,900 字に切り詰める。下書き生成上限（`customer_reply_draft_max_tokens` の既定 700 トークン）では、日本語 1 トークンあたりの文字数を踏まえても実質この上限には到達しない（切り詰めは安全マージンとして常設するが、通常経路では発火しない）。

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
- LINE アダプタ: 新規 Cloud Run service `cs-support-line`。同一イメージ、`command = /usr/local/bin/line_adapter`。公開 ingress。VPC connector 不要（API へ公開 URL で到達）。**`--max-instances=1` で運用する**（セッションストアがプロセス内メモリのため、複数インスタンスに分散すると同一ユーザーの会話が非決定的に分裂する。セッション永続化を実装するまでこの制約を維持する）。LINE Developers console の webhook URL に `https://<cs-support-line の URL>/line/webhook` を設定する

## 8. テスト

- API: 認証（ヘッダ欠落 / 不一致 / 一致）、入力制約（400 の各条件）、応答文決定表の 4 行（decision → reply_text を純関数として切り出して検証）、履歴の採用規則（6 ターン・4,000 字・古い側から破棄）、エラー分類（503 / 500 の判別関数）。`Harness::evaluate()` を通す全経路と未知 case_id の新規化は実 vegapunk が必要なためユニットテストの対象外とし、デプロイ後 E2E で検証する
- LINE アダプタ: 署名検証（正・不正・欠落）、イベント振り分け（テキスト / 非テキスト / 非 message）、セッション TTL・上限・再起動相当（新規ストア）の挙動、API 非 200 時のフォールバック文
- E2E（デプロイ後）: `curl` で `/api/reply` の 200 / 401 / 400 を確認後、実 LINE でテキスト往復とマルチターン（case_id 維持）を確認。往復時間と reply token の失効有無を実測し、アダプタの API タイムアウト値（50 秒）の妥当性をこの実測で確定する
- E2E 実測結果（2026-08-12）: curl 200 / 401 / 400 / 404 確認済み。実 LINE のテキスト往復は約 10〜15 秒で応答し、reply token の失効なし。API タイムアウト 50 秒は妥当として確定する

## 9. スコープ外（v1）と次フェーズ

- 会話履歴の AI サマライズ: 次フェーズでサーバ側に実装する（case に紐づく要約をサーバが更新し、アダプタは `message` + `case_id` のみ送る形へ移行する。要約もプロンプト予算・egress の管理下に置くため、アダプタ側では行わない）。Issue 起票済み
- 雑談 triage の高度化（v1 では回答不能 → フォールバック文で吸収する）
- 担当者個人単位の identity・権限（actor 突合表 DB 化に従属）
- crate / workspace 分離(#13)
- アダプタのセッション永続化、push 再送、非テキストメッセージの内容処理
- セッションストア上限（10,000 エントリ）到達時の eviction の構造的改善: デモ規模では到達しない条件のため v1 の Accepted Risk とする（構造改修は別スコープ）
