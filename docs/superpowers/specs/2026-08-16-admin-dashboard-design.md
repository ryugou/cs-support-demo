# 管理画面: 会話閲覧と訂正学習

| 項目 | 内容 |
| --- | --- |
| 目的 | CS 会話の閲覧・ユーザー別把握・誤回答の訂正登録（ノウハウ化）・利用状況の把握を行う管理画面と、その基盤（会話ターン永続化・管理 API）を定める |
| 読者 | 実装エージェント、レビュー担当 |
| 正本の範囲 | ConversationTurn のデータモデル、/api/reply の end_user_id 拡張、/admin API 契約、認証、SPA 構成、検索非汚染の閉じ込め条件 |
| 関連文書 | `2026-08-11-answer-api-line-adapter-design.md`（/api/reply 契約）、`specs/production-cs-mcp.md`（ノウハウ蓄積 = add_known_resolution の正本）、GitHub Issue #31 |

## 1. 採用する設計

3 部構成: (a) **会話ターンの永続化** — `/api/reply` の応答確定点で 1 ターン = 1 ノードを vegapunk へ**書き切り**（immutable、部分更新なし）、(b) **管理 API** — 既存 Google OAuth（`require_google_auth`）で保護した `/admin/api/*`、(c) **SPA** — Angular v22 + Tailwind + Angular CDK を同一サーバの `/admin` から静的配信。新規インフラなし。

訂正登録は既存の `add_known_resolution`（検証済みノウハウの離散登録）と同じ harness 入口を使う。管理画面は入力 UI であり、学習の仕組みを再実装しない。

## 2. データモデル（vegapunk、加算のみ）

新ノード型 `ConversationTurn`:

| 属性 | 型 | 内容 |
| --- | --- | --- |
| `turn_id` | string（upsert key） | `turn-{uuid}` |
| `case_id` | string | 紐づく case |
| `end_user_id` | string（任意） | 匿名化済みエンドユーザー識別子（第 4 節） |
| `seq` | int | case 内の通し番号（1 起点） |
| `created_at` | string（RFC3339 UTC） | サーバ時刻 |
| `question` | string | 顧客メッセージ（そのまま） |
| `reply_text` | string | 実際に返した最終応答文（プレーンテキスト正規化後） |
| `reply_kind` | string | `answer` / `clarify` / `escalation` / `out_of_scope` / `time_pref` / `fallback` |
| `audit_event_id` | string | 監査との突合キー |

辺: `case -HAS_TURN-> ConversationTurn`。case ノードに属性 `end_user_id`（任意、初回提供時に設定）を加算。

**書き込み規則**: `/api/reply` の応答確定点（プレーンテキスト正規化の直後）で書く。**書き込み失敗は応答を止めない**（warn ログ + 応答優先。ターン欠落は許容し、監査ログとの突合で検出可能）。ターンは書き切りで、UpsertNodes 全置換問題の対象外。

**検索非汚染の閉じ込め（受け入れ条件）**:
- `ConversationTurn` に埋め込みベクトルを作らない
- manual 検索・KR 検索の対象ノード型に含まれないことをテストで固定（「ターンが `search_manual` の結果に決して現れない」テスト）
- コミュニティ検出対象は vegapunk サーバ側 `community.target_node_types`（明示列挙）であり、新型は列挙に無いため対象外（先方への依頼不要）

**保持期間**: 無制限に増えるため、retention（例: 180 日で削除）は将来項目として記録する（本件スコープ外）。

## 3. /api/reply 契約の拡張（加算）

リクエストに任意フィールド `end_user_id?: string`（1〜64 字、`[a-f0-9]` 想定だが形式は強制しない）を追加。LINE アダプタは **LINE userId の SHA-256 の 16 進先頭 32 字**を送る（生の platform ID をサーバへ渡さない）。未提供でも従来どおり動作する（後方互換）。

## 4. 管理 API（`/admin/api/*`、Google 認証必須）

すべて `require_google_auth` を適用（claude.ai 用 OAuth 基盤を再利用）。現状の暫定 AuthZ（認証通過者は全員 supervisor）を継承する — 権限細分化は actor 突合表 DB の実装（既存の別課題）とセットで行う。

| endpoint | 内容 |
| --- | --- |
| `GET /admin/api/threads?limit=&cursor=` | case 単位の一覧。最新ターン時刻の降順。各項目: case_id / case_ref / end_user_id / 先頭質問の先頭 80 字 / ターン数 / 最新 reply_kind / 最新時刻 |
| `GET /admin/api/threads/{case_id}` | ターン列（seq 昇順）+ case メタ（累積 signal、preferred_contact_time、clarify_turns） |
| `GET /admin/api/users/{end_user_id}/threads` | 当該ユーザーのスレッド一覧（形式は threads と同じ） |
| `POST /admin/api/corrections` | `{ case_id, turn_id, signals: string[], applicability: string, answer: string, rationale_text?: string }` → 既存 add_known_resolution と同じ harness 入口を、ログイン中の Google identity の actor で呼ぶ。応答 `{ kr_id, audit_event_id }` |
| `GET /admin/api/stats/summary?days=` | 期間内のターン数・スレッド数・reply_kind 分布・ユニーク end_user_id 数 |

- ページングは cursor（`created_at` + `turn_id`）方式。エラー形式は `/api/reply` と同一
- 一覧系は vegapunk への型スコープクエリで実装（新 RPC 不要の範囲で設計し、既存 QueryNodes / traverse を使う）

## 5. SPA（Angular v22 + Tailwind + Angular CDK）

- 配信: 同一 axum サーバの `/admin` 配下から静的配信（ビルド成果物を Docker イメージへ同梱）。`/admin` への未認証アクセスは Google OAuth へ誘導
- デザイン: Stripe Dashboard 風・モダン。メインカラー `#0093A4`。実装開始前に `~/.claude/specs/frontend-style.md` を必ず Read する
- 画面 3 枚:
  1. **スレッド一覧**: 上部に stats カード（本日/7 日のターン数・スレッド数・reply_kind 分布・ユニークユーザー）、下にスレッドテーブル（クリックで詳細へ）
  2. **スレッド詳細**: チャット風のターン表示（顧客/システムの吹き出し、reply_kind バッジ、時刻）。各システム応答に「訂正を登録」ボタン → モーダル: 質問文・case の累積 signal（編集可・語彙は specs/signal-vocabulary.md）・applicability（既定: 質問の要約を operator が編集）・訂正後回答を入力 → corrections API へ。登録成功でバッジ表示
  3. **ユーザー別一覧**: end_user_id のスレッド一覧（一覧からユーザークリックで遷移）
- リポジトリ CLAUDE.md の「TypeScript を使用しない / package.json を追加しない」ルールは、本件で**「MCP サーバ実装（`server/` 配下）に限る」へ改訂する**（ユーザー承認済み 2026-08-14）。Angular アプリは `admin-ui/` 配下に置き、npm / package.json は `admin-ui/` 内に閉じる。サーバの依存・ビルドに混ぜない。Docker イメージへはビルド成果物（静的ファイル）のみを同梱する。CLAUDE.md の当該ルール改訂は本件 PR に同梱する

## 6. テスト

- ターン書き込み: 属性組み立て・reply_kind 分類の全分岐・書き込み失敗時に応答が返ること
- 検索非汚染: ターンノードが manual 検索の対象型に含まれないことの固定テスト
- 管理 API: 認証必須（未認証 401）・ページング・corrections が KR 入口を正しい actor で呼ぶこと
- SPA: ビルド成功 + 主要 component の最小テスト。E2E はデプロイ後に手動確認（一覧表示 → 訂正登録 → 同質問への回答が変わること）

## 7. スコープ外

- retention（保持期間・削除）
- 権限細分化（supervisor / viewer 等）— actor 突合表 DB とセット
- リアルタイム更新（ポーリング/リロードで足りる）
- 導入以前の会話の遡及表示（ターン永続化の開始以降のみ表示可能）
- MCP 経路の会話の表示（/api/reply 経路のみが対象。MCP は CS 担当の対話でありスレッド概念が異なる）
