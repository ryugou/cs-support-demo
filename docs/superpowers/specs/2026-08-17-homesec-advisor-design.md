# ホームセキュリティアドバイザ AI(デモ)

| 項目 | 内容 |
| --- | --- |
| 目的 | ホームセキュリティ全般の相談に応答するアドバイザ AI デモの、回答ポリシー・構成・会話設計・schema とデータ計画・デプロイ形を定める |
| 読者 | 実装エージェント、レビュー担当 |
| 正本の範囲 | 接地 2 層の回答ポリシー、advisor パイプラインの処理順、homesec schema と初期データ、advisor 固有の出口関門、営業リード獲得フロー、デモのデプロイ構成 |
| 関連文書 | `2026-08-11-answer-api-line-adapter-design.md`(/api/reply 契約と LINE アダプタの正本)、`2026-08-16-admin-dashboard-design.md`(ターン永続化と /admin API の正本)、`2026-08-17-homesec-advisor-dialogue-examples.md`(期待対話例)、GitHub Issue #34 |

## 1. 採用する設計

現行の URTECT CS AI(project `urtect`)とは別系統として、ホームセキュリティ相談のアドバイザを立てる。デモの目標は「ウソがない」「ユーザに寄り添って対話し答えを導く」体験の提供であり、網羅性・厳密性は目標にしない。あわせて、相談の流れから URTECT 製品への興味を育て、担当者連絡のリードを獲得する営業ツールとしても動作する。

- **分離**: 別 LINE 公式アカウント、別 Cloud Run service(`homesec-advisor` / `homesec-line`)、別 vegapunk schema(`homesec`)。現行 CS のコード経路・挙動は変更しない
- **共有**: 同一リポジトリ・同一 crate・同一 Docker イメージ。LINE アダプタ、会話状態、ターン永続化、管理画面、出口関門(プレーンテキスト正規化)、時間帯受付・営業時間、vegapunk 接続層、known_resolution ループを流用する
- **差し替え**: 判定ポリシー(三層 fail-closed → 接地 2 層)、プロンプト、条件語彙、データ
- **最終形**(本デモのスコープ外): アドバイザが外側の会話ループを持ち、自社取扱製品の個別サポートと判明した時点で既存 CS フローへルーティングする composition で統合する

## 2. 回答ポリシー

### 2.1 ペルソナ

「いつもそばにいる、自分専用のホームセキュリティアドバイザー」として会話する。企業窓口の応対ではない。

- 禁止する定型句(プロンプト規則 + NG 辞書): 「ご相談ありがとうございます」「お問い合わせいただき」「ご利用いただき」等の企業 CS 定型句
- 自社製品は「当社の」ではなく「URTECT の」と呼ぶ(ブランドは明示しつつ、口調は専属アドバイザー)
- 初回ターンは軽い挨拶(「こんにちは!」程度)、継続会話では挨拶しない(既存 CONTINUATION_OPENER_RULE を流用)
- 締めは相談の継続を誘う一言。毎ターンの定型クロージング(「他にご不明な点が〜」)はしない
- **提案ファースト**: `answer` のターンは、その時点で分かっている条件でできる具体的な提案を必ず応答の前半に置く。追加の質問は 1 ターンに最大 1 問とし、提案の後に添える。条件が既に足りている話題に質問を重ねない。製品のおすすめを直接聞かれたターンでは、材料にある製品を必ず名指しで提案する(不明な条件は「賃貸なら〜」のように仮定を明示して提案する)。ヒアリングだけで終わる応答を返さない(本番で「質問攻めで話が進まない」という実害が出たための規則)

### 2.2 接地 2 層

応答文の内容を 2 層に分け、層ごとに根拠の要件を変える。

| 層 | 内容 | 根拠の要件 |
| --- | --- | --- |
| 事実主張 | 統計値・傾向(「侵入窃盗の◯割が窓から」)、製品・サービスの仕様/価格帯/名称、効果の断定 | vegapunk `homesec` schema の材料(検索ヒット + known_resolution)に接地する。材料に無い事実は述べない |
| 一般助言・対話 | 状況整理、優先順位づけの考え方、声かけ、聞き返し | LLM の知識・言語能力に任せる |

- **安全下限**(接地より優先):
  - 緊急事態(侵入進行中・身の危険・ストーカー被害の切迫)は、材料・提案より先に 110 番と警察相談専用電話 #9110 を案内する定型応答を返す
  - 資格・工事を要する作業(分電盤・屋内配線等)の手順を案内しない
  - 防犯効果の保証表現(「絶対に防げます」「100%安全」等)を使わない
- Markdown 記法の禁止とプレーンテキスト正規化は現行 CS と同一(共有モジュールを使う)
- **制約との向き合い方**(本番で「ネット環境が無い → 対策できない」と受け取れる応答が出た実害への規則): 顧客の制約(ネット環境が無い・スマホを使わない・賃貸・予算等)を「対策不可能」の宣告理由にしない。**防犯・見守りの大半はネット環境なしで成立する** — 施錠の徹底・補助錠・防犯フィルム・センサーライト等の物理対策(統計材料の「無締り最多」「5 分ルール」がそのまま根拠になる)、通信内蔵型の見守りサービス(材料にある。家庭のネット不要)。制約の中で成立する対策から組み立てる。ネット前提なのはカメラ等の通信機器だけであり、顧客が遠隔での映像確認を望む場合に限り「工事不要のホームルーターでネット環境を作る選択肢もある」と従属的に添える
- **利用者と閲覧者の区別**: 見守り用途では、機器を設置される本人(親等)と、映像・通知を確認する相談者は別人である。本人がスマートフォンを使わないことを、相談者側で確認する構成まで不可能と誤推論しない(これは事実関係の正しさのための規則であり、カメラを推す理由にしない)

### 2.3 営業への接続(URTECT 優遇とリード獲得)

**アドバイザーが第一、営業は従**(本番で「営業的すぎる」実害が出たための順序規則。売り込み特化は将来の営業ツールで別途作る)。優遇の程度は「**複数の手段が同等に成立する場面では、嘘にならない範囲でカメラ(自社製品)を優先的に挙げてよい**」まで — 成立しない・明らかに劣る場面で優先しない(ユーザー指定 2026-08-21):

- **解決策の提示順序**: (1) お金のかからない習慣・設定(施錠・インターホン対応等)→ (2) 汎用の対策カテゴリ(補助錠・センサーライト・見守りサービス等、他社材料含む)→ (3) 製品。製品の中でだけ URTECT を先に挙げる。**1 応答での自社製品言及は多くても 2 件**まで
- **概念的な質問**(「カメラは意味ある?」「防犯って何から?」等)には考え方と根拠で答え、製品を挟まない(必要なら最後に一言添える程度)
- **顧客が除外・限定した種類を尊重する**: 「カメラ以外で」と言われたらカメラを提案しない。除外された領域は partner 材料(センサーライト・補助錠・見守りサービス等)で答える
- 相談条件に URTECT 製品(ADC-V523 / V523X / V724 / V724X / VC729P / VC727P / VC827P)が合致する場合、**製品を挙げる場面では** URTECT を先に提案する。合致しない場合は `partner_product` 材料の範囲で他社カテゴリ・製品を紹介し、詳細確認は公式サイトへ誘導する。優遇はプロンプト規則と材料の厚みで実現し、事実の捏造・他社の貶めはしない。注入された own_product 材料は「使える選択肢」であり毎回言及する義務ではない
- 応答文で提案した製品は製品カード(Flex)で見せる(第 7.2 節の決定論ルール)
- 担当者連絡の提案は、**明確な導入意欲**(価格・購入方法・設置依頼・機種の絞り込みへの言及)が読み取れたターンだけ、応答の末尾で 1 会話につき 1 回行う(概念的な質問や初回の一般相談への機械的な付加を禁止。押し売りしない。断られたら再提案しない)
- 顧客が担当者連絡を望んだら、時間帯受付フロー(第 4.4 節)で希望時間帯を確定し、リードとして記録する

## 3. 構成

### 3.1 バイナリと service

| 実体 | 内容 |
| --- | --- |
| `server/src/bin/homesec_advisor.rs` | advisor 本体。axum service。`/homesec/api/reply`(Bearer 認証)、`/admin`(SPA 静的配信)、`/homesec/admin/api/*`(GIS 認証)、`/static/products/*`(製品カード画像、認証不要)、`/healthz` `/livez` を持つ |
| `homesec-line` service | 既存 `line_adapter` バイナリの別インスタンス起動(第 3.3 節の `product_cards` 描画のみ追加)。env で新 LINE OA の鍵と advisor の reply URL を指す。`--max-instances=1`(セッションストアがプロセス内メモリのため) |

advisor は MCP endpoint・OAuth 認可サーバ(AS)・署名鍵を持たない。管理画面の認証は GIS(ブラウザで Google token 取得)→ 既存 `require_google_auth` の Bearer 検証で完結し、AS に依存しない。

### 3.2 共有モジュールの利用

- 会話状態: `support_case` ノードの read-merge-write(homesec schema 上)。聞き返し予算 `clarify_turns`(上限 3)を流用
- 時間帯受付(time_pref)・営業時間(hours): **担当者連絡のリード獲得に使う**。解釈・状態機械(受付モード、2 回連続不成立で解除、営業時間外は即時にその場で案内)は既存実装のまま、文言だけ営業連絡用に差し替える。営業時間の既定は平日 10:00〜18:00 JST(config)
- ターン永続化: `ConversationTurn` を homesec schema へ書き切り(応答優先・5 秒上限・失敗 warn)。`end_user_id` は LINE userId の SHA-256 先頭 32 字(アダプタ既存実装のまま)
- 管理画面: threads / thread 詳細 / users / stats / corrections の 5 endpoint を advisor にもマウントする。corrections は既存 add_known_resolution 入口で homesec schema に登録し、advisor パイプラインの KR 照合(第 6 節)で次回から効く。リード(第 4.4 節)はスレッド詳細の case メタ(`lead_requested` / `preferred_contact_time`)で確認する。**認可は CS(urtect)と同じ `require_google_auth` を流用するため、CLAUDE.md の「Project Routing and Auth」節が記す actor 突合の欠如(Google 認証さえ通れば任意アカウントが supervisor 相当になる)がそのまま適用される。** homesec ではこれにより顧客の相談内容(会話ログ・リード)が同じ露出面に載る。デモの受容リスクとして扱い、actor 突合の実装が入るまでアクセス制御としては不十分と認識すること
- 出口関門: `to_plain_text`、継続会話の挨拶抑制(CONTINUATION_OPENER_RULE)、Markdown 禁止(MARKDOWN_BAN_RULE)を流用
- 使わない共有モジュール: エスカレーション応答(CS の受付番号・折返し文言) — advisor に人間サポートエスカレーションは無い。人間が関与するのは営業リード(第 4.4 節)のみ

### 3.3 API 契約

`POST /homesec/api/reply` のリクエスト・レスポンス形式は既存 `/api/reply` と同一(正本: `2026-08-11-answer-api-line-adapter-design.md` §2)とし、レスポンスに任意フィールド `product_cards` を**加算**する(CS 側は常に省略。省略時のアダプタ挙動は従来どおりで後方互換):

```json
{
  "product_cards": [
    {
      "material_key": "own_product:adc-v724",
      "title": "URTECT ADC-V724",
      "description": "屋外対応・夜間撮影。スマホから映像確認",
      "image_url": "https://<advisor host>/static/products/adc-v724.jpg",
      "product_page_url": "https://<商品ページ URL。材料の product_page_url>",
      "button_text": "この商品について聞く",
      "button_message": "ADC-V724について詳しく教えて"
    }
  ]
}
```

- `line_adapter` は `product_cards` が非空のとき、テキスト応答の後に **Flex Message** を 1 通送る(カルーセルテンプレートは廃止)。1 件なら単一バブル、2 件以上なら Flex カルーセル(バブル横並び)。バブル構成: hero 画像(`image_url` があるとき)→ 商品名 → 説明 → ボタン 2 つ: **「商品ページを見る」**(URI action、`product_page_url` があるときだけ)と **「この商品について聞く」**(message action、タップで `button_message` がユーザー発話として送信)。アダプタは server が返した構造化データを Flex JSON へ写像するだけで、独自判断を持たない
- カードは最大 3 件・**1 件でも表示する**(商品を提案したターンの標準 UI。第 7.2 節)。own_product / partner_product 材料から組み立てる。`image_url` / `product_page_url` は任意で、無い場合はその要素を省いたバブルになる。他社製品の実写画像は権利上使わず、使うのは同梱の自社製品画像と自前の汎用カテゴリ画像のみ

レスポンスにはもう 1 つ任意フィールド `quick_replies` を加算する(CS 側は常に省略。省略時のアダプタ挙動は従来どおり):

```json
{
  "quick_replies": [
    {"label": "一戸建て", "message": "一戸建てです"},
    {"label": "マンション・アパート", "message": "マンション・アパートです"}
  ]
}
```

- `line_adapter` は `quick_replies` が非空のとき、送信する最後のメッセージに LINE の quick reply items(message action のみ)として付与する。上限 6 件(LINE 仕様の 13 件より狭く運用)。label は 20 字以内に切り詰め
- 生成は**コードの決定論のみ**: `clarify` ターンは尋ねた条件キーの語彙選択肢(顧客向けラベル)、`time_pref` ターンは営業時間内の固定スロット(「平日 10-12 時」「13-15 時」「16-18 時」)。`answer` 等その他のターンでは付けない

`reply_kind` の値は advisor 固有に次の 8 値とする。管理画面のバッジ表示に追加する:

| reply_kind | 内容 |
| --- | --- |
| `answer` | 提案・回答 |
| `clarify` | 条件の聞き返し(1 問) |
| `handoff` | URTECT 製品の個別サポート相談 → 既存 CS 窓口への案内定型文 |
| `safety` | 緊急事態の 110 / #9110 案内定型文 |
| `out_of_domain` | ホームセキュリティ無関係の相談 → 守備範囲の案内定型文 |
| `time_pref` | 担当者連絡の希望時間帯を受付・確認中 |
| `lead` | 担当者連絡が時間帯込みで確定(リード成立) |
| `fallback` | LLM 障害・出口関門違反時の定型文 |

## 4. 会話設計

### 4.1 LLM 理解(Call #1)の出力型

```json
{
  "in_domain": true,
  "emergency": false,
  "urtect_support": false,
  "lead_interest": false,
  "product_intent": true,
  "summary_ja": "賃貸マンションで玄関の防犯を強化したい",
  "conditions": [
    {"key": "housing", "value": "apartment_rented"},
    {"key": "concern", "value": "intrusion"}
  ]
}
```

- `emergency`: 侵入進行中・身の危険・ストーカー被害の切迫のみ true
- `urtect_support`: 既に URTECT 製品を所有しており、その操作・不具合の個別サポートを求めている場合のみ true(導入検討・比較は false)
- `lead_interest`: 担当者からの連絡・案内を望む意思が読み取れる場合のみ true(「お願いします」「話を聞きたい」等。単なる製品への興味は false)
- `product_intent`: 発話が具体的な機器・製品(カメラ・センサー等)の導入について尋ねている、またはそれらの物品に言及している場合のみ true。悩み・状況の相談のみで物品に触れていない場合は false(第 6 節手順 6 の own_product 保証注入で使う)
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
2. 時間帯受付モード中 → 既存 time_pref 状態機械で解釈・確定(第 4.4 節)。確定で `lead`、受付継続で `time_pref`
3. `in_domain == false` → `out_of_domain` 定型
4. `urtect_support` → `handoff` 定型。案内文面は config `handoff_contact_text` から読む(デモ初期値: 「URTECT 製品の操作や不具合は、URTECT 公式 LINE アカウントで詳しくサポートしています。」)。handoff 後も会話は継続可能で、次ターンが防犯相談なら通常応答する
5. `lead_interest` かつ `lead_requested == false` → 時間帯受付モードを ON にし、希望時間帯を尋ねる定型を返す(`time_pref`)。営業時間(平日 10:00〜18:00 JST)を添える
6. 提案に必要な条件(`concern` と、`concern == intrusion` のとき `housing`)が未取得、かつ `clarify_turns < 3` → `clarify`(LLM Call #2 で 1 問生成。選択肢は第 4.2 節の語彙と材料の範囲内)
7. それ以外 → `answer`(LLM Call #2 で下書き生成)

LLM Call #1 が失敗(タイムアウト・パース不能)した場合は 1 回だけ再試行し、再失敗で `fallback` 定型を返す(劣化カウンタは持たない)。

### 4.4 リード獲得フロー

1. LLM Call #2 のプロンプト規則: `lead_offered == false` の間、導入意欲が見えたら応答末尾に固定文言「担当者から詳しくご案内できます」を含む 1 文で担当者連絡を提案してよい(言い換えを禁止し、この文言をそのまま使うようプロンプトで指示する)。コードは生成された最終応答文(出口関門通過後)にこの固定文言が含まれるかを文字列照合し、含まれていれば `lead_offered = true` を書き戻す。フォールバック定型文(`fallback`)が返ったターンは判定対象にしない。**受容リスク**: LLM がこの文言を使わず言い換えた場合、コードは提案を検知できず `lead_offered` は false のまま残る(次ターン以降も提案規則が注入され続け、まれに複数ターンにわたって提案文言が出うる)。デモでは、常に最初の対象ターンで `lead_offered` を焼き切り提案が実質発生しなくなる設計より、この文字列照合方式を優先する
2. 顧客が応じたら(`lead_interest == true`)、時間帯受付モード ON。営業時間外の希望には即時その場で伝えて代替を聞く(既存 hours 実装)
3. 時間帯が確定したら `lead_requested = true` と `preferred_contact_time` を case へ書き戻し、`lead` 定型(「◯◯に担当者からご連絡しますね」+ 継続誘導)を返す
4. 担当者はリードを管理画面のスレッド詳細(case メタ)で確認する。デモでは通知連携(メール・Slack 等)は行わない

case 属性の加算: `lead_offered`(bool)、`lead_requested`(bool)、`shown_product_cards`(string、カード表示済み material_key の CSV)。`preferred_contact_time` は既存属性を流用する。

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
  - `card_description` string optional(`own_product` / `partner_product`。製品カードの 1 行説明。これを持つ材料だけがカード化対象)
  - `card_match_terms` string optional(`partner_product` のカード合致判定に使う語の CSV。省略時は `title_ja` で照合する。own_product の合致は常に `product_key` の型番明示のみ)
  - `product_page_url` string optional(`own_product` / `partner_product`。カードの「商品ページを見る」ボタンの遷移先。無い場合はボタンを出さない)
- `support_case`、`ConversationTurn`、known_resolution 系ノード: 現行 CS と同一定義(正本: `2026-08-16-admin-dashboard-design.md` と `specs/production-cs-mcp.md`)に、第 4.4 節の case 属性 3 つを加算。検索非汚染の閉じ込めテスト(ターン・case が材料検索に現れない)を homesec にも適用する

エッジは初期投入では張らない。シナリオと材料の関連は `category` 属性の一致で代替し、構造 traversal は PDCA 後の課題とする。

### 5.2 初期データ(手作りキュレーション)

入力ファイル: `server/data/homesec/materials.json`(1 ファイル、日本語ネイティブ、上記属性をそのまま持つ配列)。製品カード画像: `server/data/homesec/images/{product_key 小文字}.jpg`(7 点、ユーザー提供。イメージに同梱し `/static/products/` で配信)。

| kind | 件数 | 内容 |
| --- | --- | --- |
| `statistic` | 20〜30 | 警察庁「住まいる防犯110番」等の公開統計の要約(侵入口の内訳、手口、時間帯、「5 分以上で大半が諦める」等)。全件 `source_url` 必須 |
| `own_product` | 7(型番ごと) | URTECT 製品の提案用エントリ: どの悩みに効くか、賃貸可否、工事要否、屋外対応、見守り適性、カード用 1 行説明 |
| `partner_product` | 12 以上 | 他社カテゴリ・製品の紹介(警備会社サービス、スマートロック、センサーライト、窓センサー、防犯フィルム等)。価格は価格帯まで。`source_url` は公式サイト |
| `scenario` | 5 | 賃貸一人暮らし / 戸建て家族 / 高齢の親の見守り / 帰省・空き家 / 宅配・置き配。聞くべき条件と提案の骨格 |

ingest CLI: `server/src/bin/ingest_homesec.rs`。schema `homesec` の作成(`advisor_material`・`support_case`・`ConversationTurn`・known_resolution 系の全ノード型を登録。既作成なら skip)と `advisor_material` の冪等 upsert を行う。Cloud Run job `ingest-homesec` として実行する。

## 6. 処理順(1 ターン)

1. `homesec-line` が署名検証・履歴付与を行い `/homesec/api/reply` を呼ぶ(既存アダプタの挙動そのまま。ローディング表示含む)
2. Bearer 認証・入力検証(既存 /api/reply と同一)
3. 会話状態ロード(`support_case`。無ければ作成)
4. LLM Call #1: 理解(第 4.1 節の型へ構造化)
5. コード判定: `safety` / time_pref 継続 / `out_of_domain` / `handoff` / リード受付開始は定型を確定(第 4.3 節)
6. known_resolution 照合と材料検索(homesec schema、top_k = 5。累積条件 + 相談要旨で検索)。**保証注入**(検索が関連材料を引けず、接地規則が提案を封じたり投入済み材料が使われない本番実害への決定論対処):
   - **own_product**: `understanding.product_intent == true`(第 4.1 節)、または累積条件に `concern` があるターンは、検索順位に関わらず別枠注入(`category` が `concern` の値に合致するものを優先し、合致が無ければ全 7 件。material_key の重複は除去)
   - **category 合致材料**: 累積条件の `concern` と同じ `category` を持つ材料を、検索結果と別枠で kind ごとに注入する — `statistic` 最大 2・`partner_product` 最大 2・`scenario` 最大 1(material_key で重複除去。検索ヒットと保証注入を合わせた総数は最大 12 件)
7. コード判定: `clarify` か `answer` かを確定(第 4.3 節)
8. LLM Call #2: 下書き生成。プロンプトに材料全文・累積条件・会話履歴・ペルソナ規則・接地 2 層規則・URTECT 優遇規則・リード提案規則(第 4.4 節)・安全下限・Markdown 禁止・継続会話の挨拶抑制を注入
9. 出口関門(第 7 節)。違反時は `fallback` 定型へ差し替え(warn)
10. 製品カードの添付判定(第 7.2 節、決定論)
11. 条件・`clarify_turns`・リード関連属性を `support_case` へ書き戻し、`ConversationTurn` を永続化(5 秒上限・応答優先)して返却

LLM 呼び出しはターンあたり最大 2 回(理解 + 生成)。定型応答(safety / out_of_domain / handoff / time_pref / lead / fallback)のターンは Call #2 を行わない。

**内部判断の info ログ**: 応答種別(decide 結果)・注入した material_key 一覧・カード選定結果を毎ターン info でログに出す(本番会話の内部判断を運用者が追えるようにする。プロンプト・材料の PDCA はこのログとターン永続化を計器にする)。

## 7. 出口関門(advisor 固有分 + 共有分)

### 7.1 応答文の関門

決定論で検査できるものだけを関門にし、できないものはプロンプトと観測で担保する(受容リスクとして明記)。

| 関門 | 検査 | 違反時 |
| --- | --- | --- |
| URL allowlist | 応答内の URL が、今回注入した材料の `source_url` 集合に含まれない場合 | 応答破棄 → fallback |
| 型番 allowlist | 応答内の ADC- 型番(既存の型番検出 regex)が URTECT 7 型番以外の場合 | 応答破棄 → fallback |
| NG 辞書 | 保証表現・資格作業の語・企業 CS 定型句(第 2.1 節) | 応答破棄 → fallback |
| プレーンテキスト正規化 | Markdown 記法・不可視文字の除去(既存 `to_plain_text`) | 除去して通す |

**受容リスク(デモとして許容し、PDCA で観測する)**: 統計数値や他社製品名の接地は決定論では検査できない。プロンプトの接地規則で抑止し、ターンログ(管理画面)で逸脱を発見して材料追加・プロンプト修正で潰す。これがこのデモの PDCA 対象そのものである。

### 7.2 製品カードの添付判定(決定論)

カードは「**このターンで提案した商品**」の提示 UI であり、言及の装飾ではない(本番で、提案していない製品までカード化される実害が出たための規則)。

1. 今回のターンで注入した own_product / partner_product 材料(`card_description` を持つもの)について、出口関門を通過した最終応答文(正規化後)との合致を判定する:
   - **own_product**: 応答文にその**型番**(`product_key`。既存の型番検出・正規化と同じ規約)が明示されている場合のみ合致。`card_match_terms` の汎用語(「防犯カメラ」等)では合致させない
   - **partner_product**: `card_match_terms`(省略時は `title_ja`)の文字列照合(カテゴリ語がそのまま製品の同一性であるため現行どおり)
2. 合致した材料のうち、case の `shown_product_cards`(material_key 単位)に未記録のものをカード化する。**1 件でも返す**(提案した商品は常にカードで見せる)。3 件を超えるときは own_product を優先し、残りは検索ヒット順
3. 送出したら `shown_product_cards` へ追記する(同じカードを同一会話で繰り返し出さない)
4. カードの `image_url` は advisor 自ホストの `/static/products/` の同梱画像(自社製品・汎用カテゴリ)のみ。今回注入していない材料のカードは組み立てない

## 8. 障害時挙動

| 障害 | 挙動 |
| --- | --- |
| LLM Call #1 失敗(再試行 1 回込み) | `fallback` 定型を返す |
| LLM Call #2 失敗 | `fallback` 定型を返す |
| vegapunk 検索失敗(材料検索・known_resolution 読み取り) | warn ログ。材料ゼロで Call #2 を実行(接地規則により事実主張なしの一般助言になる)。応答は止めない。製品カードは添付しない(own_product 材料を引けないため) |
| support_case の書き込み失敗 | 500 を返す(fail closed)。会話状態の正本を失うため応答を継続しない |
| ConversationTurn の書き込み失敗・5 秒タイムアウト | warn ログ。応答優先(現行 CS の `CONVERSATION_TURN_WRITE_TIMEOUT` と同じ)。`shown_product_cards` の書き戻し失敗は同一カードの再表示として現れる(許容) |
| 同一 `case_id` への並行リクエスト | vegapunk に CAS が無く、support_case の read-merge-write はプロセス内外を問わず排他制御しない。`clarify_turns` の取りこぼし・カードの二重表示・担当者連絡提案の二重発生・`turn_count` の重複採番が起こりうる。デモでは受容する(LINE の 1 ユーザーが同時に複数発話を送る頻度は低いと想定。恒久対処は case 単位のロックまたは vegapunk 側の CAS が要る) |
| ingest 途中失敗 | 冪等 upsert のため再実行で収束。部分投入状態でも検索は動作する |

## 9. 不変条件

1. 現行 CS(urtect)の経路・挙動を変更しない。共有モジュールへ変更を入れる場合、既存テストが無変更で PASS すること。`line_adapter` の `product_cards` 描画は加算であり、フィールド省略時(CS 経路)の挙動は従来と同一であること
2. advisor は MCP endpoint・OAuth AS・署名鍵を持たない
3. 応答内の URL は注入材料の `source_url` のみ、ADC- 型番は URTECT 7 型番のみ。カードは今回注入した製品材料からのみ組み立て、カード画像は advisor 自ホストの同梱画像のみ
4. Markdown 記法を含む応答を顧客へ返さない
5. `emergency == true` のターンは、他のどの応答種別よりも `safety` 定型を優先する
6. 担当者連絡の提案は 1 会話につき 1 回までを目標とする(`lead_offered`、第 4.4 節 1)。検知は生成文への固定文言の文字列照合であり、LLM が言い換えた場合は保証できない(第 4.4 節記載の受容リスク)
7. `ConversationTurn` / `support_case` が材料検索の結果に現れない(検索非汚染)
8. 顧客メッセージ・会話履歴は Anthropic API へ送信される(現行 CS と同じ運用留意)

## 10. デプロイ

- 同一イメージに `homesec_advisor` バイナリを追加(Dockerfile)。config は `server/config.homesec.toml`(project `homesec` 1 件、schema `homesec`、`[llm] enabled = true`、`[api] enabled = true`、営業時間 = 平日 10:00〜18:00 JST)
- 新 Cloud Run service(CI の `RUN_SERVICES` へ追加する**前に** `gcloud run services create` で実体を作る。jobs も同様。未作成のまま CI に足すとデプロイ経路全体が NOT_FOUND で止まる):
  - `homesec-advisor`: command `/usr/local/bin/homesec_advisor`。VPC connector 必要(vegapunk 到達)。env: `CS_SUPPORT_PUBLIC_DOMAIN`(advisor 自身の URL)、`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID`(既存と同一の公開 client id)。Secret 注入: `CS_SUPPORT_ANSWER_API_KEY` ← 新 secret `homesec-answer-api-key`、`CS_SUPPORT_LLM_API_KEY` / `VEGAPUNK_BEARER_TOKEN` ← 既存 secret を共用
  - `homesec-line`: command `/usr/local/bin/line_adapter`、`--max-instances=1`、VPC connector 不要。env: `CS_ANSWER_API_URL`(advisor の reply URL)。Secret 注入: `CS_ANSWER_API_KEY` ← `homesec-answer-api-key`、`LINE_CHANNEL_SECRET` / `LINE_CHANNEL_ACCESS_TOKEN` ← 新 secret `homesec-line-channel-secret` / `homesec-line-channel-access-token`
- 新 Cloud Run job: `ingest-homesec`(merge-schema と同じ VPC connector / SA / Secret 注入)
- 新 secret は 3 件のみ: `homesec-answer-api-key`(`openssl rand -base64 32`)、`homesec-line-channel-secret` / `homesec-line-channel-access-token`(LINE Developers console 発行値)
- ユーザー作業: 新 LINE OA の作成と channel secret / access token の発行、webhook URL 設定(`https://<homesec-line の URL>/line/webhook`)、Google Cloud Console の承認済み JavaScript 生成元へ advisor の URL を追加(管理画面ログイン用)、**製品画像 7 点の提供**(カルーセル用。JPEG、1 枚 1MB 以下目安。他社カテゴリ用の汎用画像は任意 — 無ければ画像なしカードで表示する)

## 11. テスト

- 応答種別決定(第 4.3 節)の全分岐(emergency 優先、time_pref モード継続、out_of_domain、handoff、リード受付開始、clarify 予算、answer)
- リードフロー: 提案 1 回制限(`lead_offered`)、営業時間外希望の即時案内、確定時の case 書き戻しと `lead` 定型
- 提案ファースト(第 2.1 節): `answer` モードの system prompt に、提案を前半に置く規則・追加質問 1 問までの規則・条件充足時に質問を重ねない規則・直接のおすすめ依頼で製品を名指しする規則・ヒアリングのみで終える応答を禁じる規則が含まれること
- own_product の保証注入(第 6 節手順 6): `product_intent == true` のとき/累積条件に `concern` があるときそれぞれで注入されること、両方とも無いときは注入されないこと、`category` 合致がある場合はそれを優先すること、合致が無ければ全件注入されること、検索結果と重複する material_key が除去されること
- 内部判断の info ログ(第 6 節末尾): 応答種別・注入した material_key 一覧・カード選定結果が info でログに出ること、顧客メッセージ本文がログに含まれないこと
- 製品カード: 応答文と `card_match_terms` の合致判定(own / partner の両方)、own 優先の 3 件上限、`shown_product_cards`(material_key 単位)による再表示抑止、材料を引けないときは添付しない、画像なし材料がカード化できること
- `line_adapter`: `product_cards` 非空でカルーセル送信、省略時は従来挙動(既存テスト無変更 PASS)
- 出口関門: 材料外 URL・URTECT 外型番・保証表現・資格作業語・企業 CS 定型句の各違反で fallback に差し替わること
- 条件語彙: 語彙外の値が破棄され warn が出ること
- 検索非汚染: homesec schema でターン・case が材料検索に現れないこと
- ingest 冪等性: 同一入力の再実行で件数・内容が変わらないこと
- E2E(デプロイ後・手動): 新 LINE OA から「賃貸で玄関が不安」→ 聞き返し → 出典付き提案 + 製品カルーセル → 担当者連絡の提案 → 希望時間帯 → リード成立が管理画面で見えること。緊急相談 → 110 案内。URTECT 操作質問 → handoff 案内

## 12. スコープ外

- 現行 CS との統合(composition)・crate 分割(Issue #13)
- データの自動収集(クロール)・翻訳・材料の retention
- MCP endpoint の提供、権限細分化
- 人間サポートエスカレーション(リード獲得の担当者連絡は本 spec の範囲内)
- リードの通知連携(メール・Slack 等。デモは管理画面での確認のみ)
- スケール(homesec-line の複数インスタンス化、セッション永続化)
