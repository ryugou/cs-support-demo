# ホームセキュリティアドバイザ AI(デモ)

| 項目 | 内容 |
| --- | --- |
| 目的 | ホームセキュリティ全般の相談に応答するアドバイザ AI デモの、回答ポリシー・構成・会話設計・schema とデータ計画・デプロイ形を定める |
| 読者 | 実装エージェント、レビュー担当 |
| 正本の範囲 | 接地 2 層の回答ポリシー、advisor パイプラインの処理順、homesec schema と初期データ、advisor 固有の出口関門、デモのデプロイ構成 |
| 関連文書 | `2026-08-11-answer-api-line-adapter-design.md`(/api/reply 契約と LINE アダプタの正本)、`2026-08-16-admin-dashboard-design.md`(ターン永続化と /admin API の正本)、GitHub Issue #34 |

## 1. 採用する設計

現行の URTECT CS AI(project `urtect`)とは別系統として、ホームセキュリティ相談のアドバイザを立てる。デモの目標は「ウソがない」「ユーザに寄り添って対話し答えを導く」体験の提供であり、網羅性・厳密性は目標にしない。

- **分離**: 別 LINE 公式アカウント、別 Cloud Run service(`homesec-advisor` / `homesec-line`)、別 vegapunk schema(`homesec`)。現行 CS のコード経路・挙動は変更しない
- **共有**: 同一リポジトリ・同一 crate・同一 Docker イメージ。LINE アダプタ、会話状態、ターン永続化、管理画面、出口関門(プレーンテキスト正規化)、vegapunk 接続層、known_resolution ループを流用する
- **差し替え**: 判定ポリシー(三層 fail-closed → 接地 2 層)、プロンプト、条件語彙、データ
- **最終形**(本デモのスコープ外): アドバイザが外側の会話ループを持ち、自社取扱製品の個別サポートと判明した時点で既存 CS フローへルーティングする composition で統合する

## 2. 回答ポリシー(接地 2 層)

応答文の内容を 2 層に分け、層ごとに根拠の要件を変える。

| 層 | 内容 | 根拠の要件 |
| --- | --- | --- |
| 事実主張 | 統計値・傾向(「侵入窃盗の◯割が窓から」)、製品・サービスの仕様/価格帯/名称、効果の断定 | vegapunk `homesec` schema の材料(検索ヒット + known_resolution)に接地する。材料に無い事実は述べない |
| 一般助言・対話 | 状況整理、優先順位づけの考え方、声かけ、聞き返し | LLM の知識・言語能力に任せる |

- **安全下限**(接地より優先):
  - 緊急事態(侵入進行中・身の危険・ストーカー被害の切迫)は、材料・提案より先に 110 番と警察相談専用電話 #9110 を案内する定型応答を返す
  - 資格・工事を要する作業(分電盤・屋内配線等)の手順を案内しない
  - 防犯効果の保証表現(「絶対に防げます」「100%安全」等)を使わない
- **自社優遇**: 相談条件に URTECT 製品(ADC-V523 / V523X / V724 / V724X / VC729P / VC727P / VC827P)が合致する場合は自社製品を先に提案する。合致しない場合は `partner_product` 材料の範囲で他社カテゴリ・製品を紹介し、詳細確認は公式サイトへ誘導する。優遇はプロンプト規則と材料の厚み(own_product エントリのみ提案情報が詳しい)で実現し、事実の捏造・他社の貶めはしない
- Markdown 記法の禁止とプレーンテキスト正規化は現行 CS と同一(共有モジュールを使う)

## 3. 構成

### 3.1 バイナリと service

| 実体 | 内容 |
| --- | --- |
| `server/src/bin/homesec_advisor.rs` | advisor 本体。axum service。`/homesec/api/reply`(Bearer 認証)、`/admin`(SPA 静的配信)、`/homesec/admin/api/*`(GIS 認証)、`/healthz` `/livez` を持つ |
| `homesec-line` service | 既存 `line_adapter` バイナリをコード変更なしで別インスタンス起動。env で新 LINE OA の鍵と advisor の reply URL を指す。`--max-instances=1`(セッションストアがプロセス内メモリのため) |

advisor は MCP endpoint・OAuth 認可サーバ(AS)・署名鍵を持たない。管理画面の認証は GIS(ブラウザで Google token 取得)→ 既存 `require_google_auth` の Bearer 検証で完結し、AS に依存しない。

### 3.2 共有モジュールの利用

- 会話状態: `support_case` ノードの read-merge-write(homesec schema 上)。聞き返し予算 `clarify_turns`(上限 3)を流用
- ターン永続化: `ConversationTurn` を homesec schema へ書き切り(応答優先・5 秒上限・失敗 warn)。`end_user_id` は LINE userId の SHA-256 先頭 32 字(アダプタ既存実装のまま)
- 管理画面: threads / thread 詳細 / users / stats / corrections の 5 endpoint を advisor にもマウントする。corrections は既存 add_known_resolution 入口で homesec schema に登録し、advisor パイプラインの KR 照合(第 7 節)で次回から効く
- 出口関門: `to_plain_text`、継続会話の挨拶抑制(CONTINUATION_OPENER_RULE)、Markdown 禁止(MARKDOWN_BAN_RULE)を流用
- 使わない共有モジュール: 時間帯受付(time_pref)、営業時間(hours)、エスカレーション応答 — advisor には人間エスカレーションが無いため

### 3.3 API 契約

`POST /homesec/api/reply` のリクエスト・レスポンス形式は既存 `/api/reply` と同一(正本: `2026-08-11-answer-api-line-adapter-design.md` §2)。これにより `line_adapter` が無変更で接続できる。`reply_kind` の値だけ advisor 固有とする:

| reply_kind | 内容 |
| --- | --- |
| `answer` | 提案・回答 |
| `clarify` | 条件の聞き返し(1 問) |
| `handoff` | URTECT 製品の個別サポート相談 → 既存 CS 窓口への案内定型文 |
| `safety` | 緊急事態の 110 / #9110 案内定型文 |
| `out_of_domain` | ホームセキュリティ無関係の相談 → 守備範囲の案内定型文 |
| `fallback` | LLM 障害・出口関門違反時の定型文 |

管理画面のバッジ表示に上記 6 値を追加する。

## 4. 会話設計

### 4.1 LLM 理解(Call #1)の出力型

```json
{
  "in_domain": true,
  "emergency": false,
  "urtect_support": false,
  "summary_ja": "賃貸マンションで玄関の防犯を強化したい",
  "conditions": [
    {"key": "housing", "value": "apartment_rented"},
    {"key": "concern", "value": "intrusion"}
  ]
}
```

- `emergency`: 侵入進行中・身の危険・ストーカー被害の切迫のみ true
- `urtect_support`: 既に URTECT 製品を所有しており、その操作・不具合の個別サポートを求めている場合のみ true(導入検討・比較は false)
- `conditions`: 第 4.2 節の語彙へ正規化。語彙外の値は破棄し warn ログに出す(コード判定)

### 4.2 条件語彙(advisor 版 signal)

| key | 値 |
| --- | --- |
| `housing` | `detached_owned` / `detached_rented` / `apartment_owned` / `apartment_rented` |
| `target` | `self_home` / `parent_home` / `vacant_home` / `store` |
| `concern` | `intrusion` / `monitoring` / `package_theft` / `stalking` / `fire_disaster` |
| `budget` | `under_10k` / `10k_50k` / `over_50k` |
| `install` | `construction_ok` / `no_construction` |

条件は `support_case` に累積し、ターンごとに全量で再評価する(現行 CS の signal 累積と同じ流儀)。語彙の追加は本 spec の更新を伴う(PDCA で会話ログから発見して足す)。

### 4.3 応答種別の決定(コード判定)

優先順に評価し、最初に該当したものを返す:

1. `emergency` → `safety` 定型
2. `in_domain == false` → `out_of_domain` 定型
3. `urtect_support` → `handoff` 定型。案内文面は config `handoff_contact_text` から読む(デモ初期値: 「URTECT 製品の操作・不具合については、URTECT 公式 LINE アカウントよりお問い合わせください。」)。handoff 後も会話は継続可能で、次ターンが防犯相談なら通常応答する
4. 提案に必要な条件(`concern` と、`concern == intrusion` のとき `housing`)が未取得、かつ `clarify_turns < 3` → `clarify`(LLM Call #2 で 1 問生成。選択肢は第 4.2 節の語彙と材料の範囲内)
5. それ以外 → `answer`(LLM Call #2 で下書き生成)

LLM Call #1 が失敗(タイムアウト・パース不能)した場合は 1 回だけ再試行し、再失敗で `fallback` 定型を返す(劣化カウンタは持たない)。

## 5. schema とデータ計画

### 5.1 vegapunk schema `homesec`

ノード型(すべて日本語ネイティブ。翻訳パイプラインは使わない):

- `advisor_material`
  - `material_key` string required, stable upsert key。形式 `{kind}:{slug}`(例 `statistic:mujimari-shinnyu`)
  - `kind` string required: `statistic` / `own_product` / `partner_product` / `scenario`
  - `title_ja` string required
  - `body_ja` string required(検索対象)
  - `source_url` string, `statistic` と `partner_product` は required、他は optional
  - `category` string optional: `intrusion` / `monitoring` / `package_theft` / `stalking` / `fire_disaster`(第 4.2 節 `concern` と同一語彙)
  - `product_key` string optional(`own_product` のとき URTECT 型番)
  - `price_band` string optional(`own_product` / `partner_product`)
- `support_case`、`ConversationTurn`、known_resolution 系ノード: 現行 CS と同一定義(正本: `2026-08-16-admin-dashboard-design.md` と `specs/production-cs-mcp.md`)。検索非汚染の閉じ込めテスト(ターン・case が材料検索に現れない)を homesec にも適用する

エッジは初期投入では張らない。シナリオと材料の関連は `category` 属性の一致で代替し、構造 traversal は PDCA 後の課題とする。

### 5.2 初期データ(手作りキュレーション)

入力ファイル: `server/data/homesec/materials.json`(1 ファイル、日本語ネイティブ、上記属性をそのまま持つ配列)。

| kind | 件数 | 内容 |
| --- | --- | --- |
| `statistic` | 20〜30 | 警察庁「住まいる防犯110番」等の公開統計の要約(侵入口の内訳、手口、時間帯、「5 分以上で大半が諦める」等)。全件 `source_url` 必須 |
| `own_product` | 7(型番ごと) | URTECT 製品の提案用エントリ: どの悩みに効くか、賃貸可否、工事要否、屋外対応、見守り適性 |
| `partner_product` | 12 以上 | 他社カテゴリ・製品の紹介(警備会社サービス、スマートロック、センサーライト、窓センサー、防犯フィルム等)。価格は価格帯まで。`source_url` は公式サイト |
| `scenario` | 5 | 賃貸一人暮らし / 戸建て家族 / 高齢の親の見守り / 帰省・空き家 / 宅配・置き配。聞くべき条件と提案の骨格 |

ingest CLI: `server/src/bin/ingest_homesec.rs`。schema `homesec` の作成(`advisor_material`・`support_case`・`ConversationTurn`・known_resolution 系の全ノード型を登録。既作成なら skip)と `advisor_material` の冪等 upsert を行う。Cloud Run job `ingest-homesec` として実行する。

## 6. 処理順(1 ターン)

1. `homesec-line` が署名検証・履歴付与を行い `/homesec/api/reply` を呼ぶ(既存アダプタの挙動そのまま。ローディング表示含む)
2. Bearer 認証・入力検証(既存 /api/reply と同一)
3. 会話状態ロード(`support_case`。無ければ作成)
4. LLM Call #1: 理解(第 4.1 節の型へ構造化)
5. コード判定: `safety` / `out_of_domain` / `handoff` は定型を確定(第 4.3 節)
6. known_resolution 照合と材料検索(homesec schema、top_k = 5。累積条件 + 相談要旨で検索)
7. コード判定: `clarify` か `answer` かを確定(第 4.3 節)
8. LLM Call #2: 下書き生成。プロンプトに材料全文・累積条件・会話履歴・接地 2 層規則・自社優遇規則・安全下限・Markdown 禁止・継続会話の挨拶抑制を注入
9. 出口関門(第 7 節)。違反時は `fallback` 定型へ差し替え(warn)
10. 条件・`clarify_turns` を `support_case` へ書き戻し、`ConversationTurn` を永続化(5 秒上限・応答優先)して返却

LLM 呼び出しはターンあたり最大 2 回(理解 + 生成)。定型応答(safety / out_of_domain / handoff / fallback)のターンは Call #2 を行わない。

## 7. 出口関門(advisor 固有分 + 共有分)

決定論で検査できるものだけを関門にし、できないものはプロンプトと観測で担保する(受容リスクとして明記)。

| 関門 | 検査 | 違反時 |
| --- | --- | --- |
| URL allowlist | 応答内の URL が、今回注入した材料の `source_url` 集合に含まれない場合 | 応答破棄 → fallback |
| 型番 allowlist | 応答内の ADC- 型番(既存の型番検出 regex)が URTECT 7 型番以外の場合 | 応答破棄 → fallback |
| NG 辞書 | 保証表現・資格作業の語(既存 NG 辞書機構に advisor 語彙を追加) | 応答破棄 → fallback |
| プレーンテキスト正規化 | Markdown 記法・不可視文字の除去(既存 `to_plain_text`) | 除去して通す |

**受容リスク(デモとして許容し、PDCA で観測する)**: 統計数値や他社製品名の接地は決定論では検査できない。プロンプトの接地規則で抑止し、ターンログ(管理画面)で逸脱を発見して材料追加・プロンプト修正で潰す。これがこのデモの PDCA 対象そのものである。

## 8. 障害時挙動

| 障害 | 挙動 |
| --- | --- |
| LLM Call #1 失敗(再試行 1 回込み) | `fallback` 定型を返す |
| LLM Call #2 失敗 | `fallback` 定型を返す |
| vegapunk 検索失敗 | warn ログ。材料ゼロで Call #2 を実行(接地規則により事実主張なしの一般助言になる)。応答は止めない |
| 会話状態・ターン書き込み失敗 | warn ログ。応答優先(現行 CS と同じ) |
| ingest 途中失敗 | 冪等 upsert のため再実行で収束。部分投入状態でも検索は動作する |

## 9. 不変条件

1. 現行 CS(urtect)の経路・挙動を変更しない。共有モジュールへ変更を入れる場合、既存テストが無変更で PASS すること
2. advisor は MCP endpoint・OAuth AS・署名鍵を持たない
3. 応答内の URL は注入材料の `source_url` のみ、ADC- 型番は URTECT 7 型番のみ
4. Markdown 記法を含む応答を顧客へ返さない
5. `emergency == true` のターンは、他のどの応答種別よりも `safety` 定型を優先する
6. `ConversationTurn` / `support_case` が材料検索の結果に現れない(検索非汚染)
7. 顧客メッセージ・会話履歴は Anthropic API へ送信される(現行 CS と同じ運用留意)

## 10. デプロイ

- 同一イメージに `homesec_advisor` バイナリを追加(Dockerfile)。config は `server/config.homesec.toml`(project `homesec` 1 件、schema `homesec`、`[llm] enabled = true`、`[api] enabled = true`)
- 新 Cloud Run service(CI の `RUN_SERVICES` へ追加する**前に** `gcloud run services create` で実体を作る。jobs も同様。未作成のまま CI に足すとデプロイ経路全体が NOT_FOUND で止まる):
  - `homesec-advisor`: command `/usr/local/bin/homesec_advisor`。VPC connector 必要(vegapunk 到達)。env: `CS_SUPPORT_PUBLIC_DOMAIN`(advisor 自身の URL)、`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID`(既存と同一の公開 client id)。Secret 注入: `CS_SUPPORT_ANSWER_API_KEY` ← 新 secret `homesec-answer-api-key`、`CS_SUPPORT_LLM_API_KEY` / `VEGAPUNK_BEARER_TOKEN` ← 既存 secret を共用
  - `homesec-line`: command `/usr/local/bin/line_adapter`、`--max-instances=1`、VPC connector 不要。env: `CS_ANSWER_API_URL`(advisor の reply URL)。Secret 注入: `CS_ANSWER_API_KEY` ← `homesec-answer-api-key`、`LINE_CHANNEL_SECRET` / `LINE_CHANNEL_ACCESS_TOKEN` ← 新 secret `homesec-line-channel-secret` / `homesec-line-channel-access-token`
- 新 Cloud Run job: `ingest-homesec`(merge-schema と同じ VPC connector / SA / Secret 注入)
- 新 secret は 3 件のみ: `homesec-answer-api-key`(`openssl rand -base64 32`)、`homesec-line-channel-secret` / `homesec-line-channel-access-token`(LINE Developers console 発行値)
- ユーザー作業: 新 LINE OA の作成と channel secret / access token の発行、webhook URL 設定(`https://<homesec-line の URL>/line/webhook`)、Google Cloud Console の承認済み JavaScript 生成元へ advisor の URL を追加(管理画面ログイン用)

## 11. テスト

- 応答種別決定(第 4.3 節)の全分岐(emergency 優先、out_of_domain、handoff、clarify 予算、answer)
- 出口関門: 材料外 URL・URTECT 外型番・保証表現・資格作業語の各違反で fallback に差し替わること
- 条件語彙: 語彙外の値が破棄され warn が出ること
- 検索非汚染: homesec schema でターン・case が材料検索に現れないこと
- 共有モジュール変更時、既存(urtect)テストが無変更で PASS すること
- ingest 冪等性: 同一入力の再実行で件数・内容が変わらないこと
- E2E(デプロイ後・手動): 新 LINE OA から「賃貸で玄関が不安」→ 聞き返し → 出典付き提案、緊急相談 → 110 案内、URTECT 操作質問 → handoff 案内

## 12. スコープ外

- 現行 CS との統合(composition)・crate 分割(Issue #13)
- データの自動収集(クロール)・翻訳・材料の retention
- MCP endpoint の提供、権限細分化
- 時間帯受付・営業時間・人間エスカレーション
- スケール(homesec-line の複数インスタンス化、セッション永続化)
