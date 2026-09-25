# Jev(TypeSafe System One)シャドー運転

| 項目 | 内容 |
| --- | --- |
| 目的 | 顧客発話の「理解」を型付き + 確率で得る外部判断器(Jev)を、本番の判定を変えずに並行実行し、較正を測るための記録を残す |
| 読者 | 実装エージェント、レビュー担当 |
| 正本の範囲 | Jev クライアントの契約、質問定義の置き場、起動点と fail-soft 規律、記録の形、config と secret |
| 関連文書 | `2026-08-11-answer-api-line-adapter-design.md`(/api/reply 契約)、GitHub Issue #56 |

**2026-09-21 追記**: Issue #58 により、第1層のうちヒアリング契約 `product_and_symptom` を
宣言したルール(実データでは `warranty-failure` のみ)にマッチしたターンの聞き返し判定にのみ、
Jev の `has_enough_info` を使うよう変更した(詳細 §7)。他の経路は引き続き §1〜§6 の
shadow-only のまま。`contract-billing`(契約・請求)は advisory だが宣言を持たないため対象外
(型番も症状も関係ない問い合わせを、型番と症状を尋ねるヒアリングへ流さないため。§7)。

## 1. 採用する設計

Jev は「解釈」だけを担い、判定はコードに残す(既存の原則どおり)。本タスクでは判定に一切使わず、**並行実行して記録するだけ**とする(ただし Issue #58 により §7 の経路だけが例外。ヒアリング契約 `product_and_symptom` を宣言した第1層ルール(`warranty-failure`)の聞き返し判定にのみ `has_enough_info` を使う。詳細は §7)。

- 呼び出し: `POST https://api.typesafe.ai/v1/systemone`、`Authorization: Bearer <TYPESAFE_API_KEY>`
- リクエスト: `{ "state": <顧客発話>, "model": "jev-latest", "questions": <質問定義> }`
  - §7 の経路(ヒアリング契約 `product_and_symptom` を宣言した第1層ルールの聞き返し判定)では、`state` は今ターンの発話 1 件ではなく「過去の顧客発話 + 今ターンの発話」に組み立て直される(§7「Jev へ渡す state」)
- レスポンス: `{ "model", "answers": { <id>: { "type": "noul"|"choice"|"score", ... } }, "usage": { "input_tokens", "output_tokens" } }`
  - noul: `noul`(0〜1)
  - choice: `choice` / `confidence` / `probabilities`
  - score: `score` / `confidence` / `probabilities` / `legend`
- 既知の実測値(2026-09-21、日本語 12 件): 全件 200、0.49〜0.67 秒。5 問で入力 945・出力 220 トークン、14 問で入力 1,221・出力 385 トークン、レイテンシは不変

## 2. 実装

- `server/src/jev.rs`(新規)
  - `JevClient::from_config(&JevConfig) -> Result<Option<Self>>`: `enabled = false` なら `None`、`enabled = true` で鍵が解決できなければ起動時 `Err`(`AnthropicClient::from_config` と同じ流儀)
  - `async fn evaluate(&self, state: &str, request_id: &str) -> Result<JevOutcome>`: 質問定義は構築時に読み込んで保持する
    - `request_id` は**ログの突合専用**で、Jev へは送信しない(リクエスト本文は §1 の `state` / `model` / `questions` のみ)。応答の要素をパースできず捨てるときの warn に載せ、呼び出し側(`api.rs::query_jev_has_enough_info`)の warn と同じ値で結ぶ
  - タイムアウトは config(既定 3 秒)。エラーは型で返し、呼び出し側が握りつぶす
- 質問定義: `server/data/urtect/jev-questions.json`(イメージ同梱)。パスは config の `[jev] questions_path`
- 起動点: `/{project_id}/api/reply` の入力検証を通った直後。**`tokio::spawn` で fire-and-forget**(応答を待たない。ターン永続化と同じ規律)
- 記録: `tracing::info!` の構造化ログ 1 行。`request_id` / `case_id`(無ければ空)/ 各 answer の**種別**(`JevAnswer::kind()`)と数値(`noul` / `score` / `confidence`)/ 所要ミリ秒 / 入出力トークン。**顧客発話の本文はログに出さない**(request_id で監査ログと突合する)。**`choice` の選択値・`probabilities` のキー・`legend` は出さない**(モデル生成文字列で、顧客由来のデータを含みうる。`api.rs::query_jev_has_enough_info` と同じ規律。`{:?}` での丸ごと出力も不可)
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
5. Jev の answer のうちモデル生成文字列(`choice` の選択値・`probabilities` のキー・`legend`)をログに書かない(顧客由来のデータを含みうる。書いてよいのは種別 `JevAnswer::kind()` と数値のみ)

**§7 の経路はこの限りではない**: ヒアリング契約 `product_and_symptom` を宣言した第1層ルール
(`warranty-failure`)の聞き返し判定に限り、Jev の結果(`has_enough_info`)を判定に使い(2 と矛盾)、
対象ターンの応答時間は Jev の 1 往復分だけ増える(1 と矛盾)。
これは Issue #58 で意図的に導入した例外であり、詳細と許容根拠は §7 を参照。1 と 2 は §7 の
経路(`warranty-failure` にマッチしたターン)では成立しない。3・4・5 は §7 の経路においても
真のまま(顧客発話の本文と Jev の answer のモデル生成文字列はログに書かず、
`[jev] enabled = false` の構成では Jev へ一切アクセスしない)。

## 4. テスト

- 無効時: クライアントが構築されず、呼び出しも発生しない
- 有効時: スタブで成功・HTTP エラー・タイムアウトを与え、いずれも応答本文と `reply_kind` が不変であること
- パース: noul / choice / score の 3 型、未知フィールド、壊れた JSON(失敗として扱い応答に影響しない)
- ログ: 顧客発話の本文と、Jev の answer のモデル生成文字列(`choice` の選択値・`probabilities` のキー・`legend`)が出力に含まれないことを固定(後者は `api.rs` の `..._logs_only_the_kind_when_answer_is_a_choice` / `..._score` がセンチネル値で固定)

## 5. 運用

- 本番有効化の手順: secret `typesafe-api-key` を作成 → ランタイム SA へ `secretAccessor` を付与 → service へ注入 → `config.cloudrun.toml` の `[jev] enabled = true`。
  **前 3 段階（secret 作成・`secretAccessor` 付与・service への注入）は完了済み**(revision `cs-support-mcp-00044-scz` で注入を確認済み)。**4 段階目の `config.cloudrun.toml` の `[jev] enabled = true` は本変更に含まれ、main マージ後の CI デプロイで反映される**。
- 顧客の発話が第三者(TypeSafe)へ送信される。CLAUDE.md の Anthropic に関する記載と同じ扱いで明記する(反映済み)

## 6. 次段階(本タスクの範囲外)

第1層 `warranty-failure`(ヒアリング契約 `product_and_symptom` を宣言したルール)の聞き返し
判定は §7 で先行実施済み。本節の残り(緊急・製品・担当者要望への拡張)は引き続き未着手。

実会話 1〜2 週間の記録で較正を確認したのち、判定の置き換えへ進む。初期の閾値案(すべてコード側):

- 緊急: 0.5 以上で安全案内(見落としのコストが高いため低めに置く)
- 故障: 0.8 以上 かつ 情報十分 0.6 以上 → エスカレーション / 0.8 以上 かつ情報不足 → ヒアリング / 0.3〜0.8 → 通常の回答を試みる
- 製品: 信頼度 0.8 以上 → 確定 / 0.4〜0.8 → 型番の確認 / 未満 → 言及なし。他社製品の断りは 0.8 以上のときだけ
- 担当者要望: 0.8 以上

複数の型番に言及されたケースは Choice では 1 つしか返らないため(実測: V724 0.84 / V523 0.05)、置き換え時は**型番ごとの Noul** に変更する。型番の音写(「びーけーごーにーさん」)は解決できない(実測: 製品なし 0.93)。

## 7. 段階A: ヒアリング契約を宣言した第1層ルールの聞き返し判定への適用(Issue #58、実装済み)

### 背景

本番実測(2026-09-21)で、「電源が入らなくなった」という問い合わせが第1層の明示エスカレー
ションルール(`warranty-failure`、advisory binding)にマッチしたが、そのルールの `missing`
(マニュアル材料のカバレッジ不足で決まる)がたまたま空になり、聞き返し(Clarify)に入らず
即エスカレーションしていた。`missing` は「マニュアル側の根拠十分性」であって「顧客が話した
情報の十分性」ではないため、この経路に限り、後者を直接測る Jev の `has_enough_info`(質問定義
`server/data/urtect/jev-questions.json`、`criteria.true`: 「対象の製品と具体的な症状の両方が
分かる」)に置き換えた。

### ヒアリング契約: なぜ binding(advisory か)ではなくルール自身の宣言で判別するか

`server/data/urtect/rules.json` の advisory ルールは 2 件あり、**情報の契約が違う**。

| rule_id | 問い合わせ | 話が進むのに要る情報 |
| --- | --- | --- |
| `warranty-failure` | 製品の故障 | 型番と症状が揃って初めて話が進む |
| `contract-billing` | 契約・請求 | 型番も症状も関係ない |

`has_enough_info` の判定基準は「対象の製品と具体的な症状の両方が分かる」で、聞き返し文言
(`JEV_HEARING_MISSING_TEXT`)も固定で型番と症状を尋ねる。したがって「advisory だから聞き返す」
という binding での判別だと、**十分に具体的な契約・請求の問い合わせが、型番と症状を尋ねる無関係
なヒアリングに流れる**。これは Issue #58 が直そうとした「質問ばかりで話が進まない」不具合を
別の入口で再現する(PR #60 の Copilot レビューで指摘)。加えて、契約・請求の顧客発話を不要に
第三者(TypeSafe)へ送ることにもなる。

そこで、**ルール自身が「どの情報契約でヒアリングするか」を宣言する**形にした。rule_id では
分岐しない(ルールを足しても、宣言を付けるだけで対象にできる)。

- `rules.json` の `escalation_rules[]` に任意フィールド `hearing` を持てる。現在定義済みの値は
  `product_and_symptom`(対象の製品の型番と具体的な症状の両方が分かって初めて話が進む問い合わせ。
  質問 `has_enough_info` がこれに対応する)のみ。**`warranty-failure` にだけ付ける**。
  `contract-billing` と mandatory 4 件(`security-incident` / `physical-damage` /
  `construction-risk` / `human-handoff`)には付けない
- 実行時の型は `harness::rules::HearingContract`(`EscalationRule.hearing: Option<HearingContract>`)。
  **`Binding::Mandatory` のルールは宣言があっても常に「宣言なし」として扱う**
  (`EscalationRule::hearing_contract()`)。mandatory は情報の有無を問わず即エスカレーションする
  拘束度そのもので、設定ミスで宣言が付いても聞き返しへ開放しない
- 永続化: vegapunk の `EscalationRule` ノードの `hearing` 属性(`schema/cs-schema.yml` と
  `schema/cs-support.yml` の両方に宣言)。書き込みは `ingest_rules` CLI、読み込みは
  `harness::knowledge::escalation_rule_from_attributes`。宣言なしは**空文字を明示的に書く**
  (属性を省略すると、`UpsertNodes` が部分マージの場合に、宣言を消した再投入が旧い宣言を
  残しうるため)。読み込み側は欠落・空文字を同じく「宣言なし」とする
- 検証(vegapunk へ接続する前): `ingest_rules` は次をエラーにして、vegapunk へ接続する前
  (＝何も書く前)に落ちる(CI の `ingest-rules` job が赤くなる)。
  - **rules ファイルの未知フィールド名**(綴りミス。`RulesFile` / `RuleInput` / `DomainInput`
    すべて `deny_unknown_fields`。例: `hearing` → `hearng`)。serde の既定だと綴りミスは黙って
    捨てられ、「宣言なし」として通って本番で Jev が永久に呼ばれなくなるため。エラーは間違えた
    フィールド名と有効なフィールド名の一覧を名指しする
  - **`binding` の省略、および未知の `binding` 値**(`"manadatory"` / `"Mandatory"` / 空文字等。
    **`binding` は必須で、既定値は無い。値は完全一致のみ受理**。`escalation_rules` と
    `prohibited_domains` の両方。キーなし・`null` は `missing binding`、未知値は `unknown binding`
    で、どちらもエラーは rule_id / domain_id と有効な値の一覧 `mandatory, advisory` を名指しする)。
    実行時のローダは未知の binding・属性なしを advisory に倒す(下記)ため、ここで落とさないと
    mandatory の綴りミスや `binding` の書き忘れが黙って advisory になり、`match_layer1` の
    mandatory 優先・`decide` の `missing` 抑止・`hearing` の mandatory ガード(この CLI の検証と
    実行時の `hearing_contract`)が**同時に**破れる。`human-handoff`(聞き返しループからの
    脱出手段)のような mandatory が Jev のヒアリングに吸収されうる。入力形式を狭めるのは意図した
    fail-closed(現行の `rules.json` / `rules.sample.json` は全件明示している)。旧仕様は省略を許し、
    第1層は advisory・第2層は mandatory を既定にしていたが、必須化で省略経路ごと消えた
  - **未知の `hearing` 値**、および **`binding = mandatory` への `hearing` 宣言**

  実行時のローダは、未知の `hearing` 値を **Err にせず「宣言なし」に倒して warn**、未知の
  `binding` 値を **Err にせず advisory に倒して warn** する(`load_escalation_rules` /
  `load_prohibited_domains` は 1 件でも Err を返すと第1層・第2層の全ルールが読めなくなるため。
  デプロイ順序のずれで、新しい契約名を旧リビジョンが読む場合に第1層全体が止まらないようにする)。
  「宣言なし」は従来の `missing` ベース判定へ戻るだけで安全側。advisory への倒しは mandatory の
  綴りミスなら保証が失われる方向なので、warn(種別・ID・値)を必ず残す。CLI が fail-closed、
  ローダが fail-back という非対称は意図した設計
- `hearing` は `api.rs` の内部判定専用で、**MCP の出力契約(`evaluate_answerability` の
  応答 payload と生成スキーマ)には載せない**。`AnswerDecision` は
  `EvaluateAnswerabilityResponse.decision` としてそのままシリアライズされるため、
  `#[serde(skip)]` で外している(`specs/production-cs-mcp.md` の「MCP tool の入出力の形は
  変更しない」不変条件。`decision.rs` と `rmcp_server.rs` のテストが payload・スキーマ・
  `tools/list` の `outputSchema` の 3 か所で固定している)。先行して同じ扱いだった
  `rule_binding`(拘束度の内部分類)は、`api.rs` での唯一の用途がこの判別だったため、
  `hearing` に置き換えて廃止した

### トリガー条件

次の 3 条件をすべて満たすターンでだけ、Jev を `await` して使う(毎ターンではない)。

1. `layer == 1` かつ、マッチしたルールが `HearingContract::ProductAndSymptom` を宣言している
   `Escalate` であること(宣言の無いルール(`contract-billing` を含む)・mandatory ルール・
   第2層・第3層は対象外。`missing` の中身には依存しない — これが本是正の核)
2. 今ターンの signal 抽出が `ExtractionMode::LexiconFallback` に落ちていないこと(抽出 LLM
   不調時は `decide_reply_action` 自身が fail-closed で `EscalationReply` に倒すため)
3. `conv.is_already_escalated()` でないこと(確定済み case は `decide_reply_action` 自身が
   `!conv.is_already_escalated()` ガードで `EscalationReply` に倒すため、Jev を呼んでも結果を
   使わず無駄な待ちとコストが発生するだけ)

判別は `AnswerDecision::Escalate` の `hearing: Option<HearingContract>` フィールドで行う
(`decision::decide` が第1層マッチ時に、マッチしたルールの `hearing_contract()` を積む)。
実装は `server/src/api.rs` の `is_product_and_symptom_hearing_turn` /
`resolve_jev_has_enough_info`。

`layer == 1` の選択自体(`rules::match_layer1`)は、マッチした全ルールのうち
`Binding::Mandatory` を `Binding::Advisory` より必ず優先する(同一 binding 内は配列の
先頭優先)。したがって advisory と mandatory の条件が同時に成立するターン(累積 signal
により実際に起こりうる)でも選ばれるのは mandatory で、その decision の `hearing` は
`None` になり、Jev は呼ばれない。
この優先順位が mandatory の即時エスカレーション契約を担保しており、`rules.json` の
並び順を変えてもこの契約は壊れない(Issue #58 の codex レビューで、advisory が後続
mandatory を覆い隠す経路が Critical として指摘され是正した)。

### Jev へ渡す state

Jev の `state` には、今ターンの発話だけでなく**過去の顧客発話**も含める。`has_enough_info` の
判定基準は「対象の製品と具体的な症状の両方が分かる」であり、今ターンの発話だけを渡すと、
多ターンのヒアリングで顧客が 1 ターン目に症状、2 ターン目に型番を答えたとき、どのターン単体
でも「両方分かる」にならず `has_enough_info` が上がらない。その結果 `clarify_max_turns`(既定
3)に達するまで聞き返しを繰り返してからエスカレーションすることになる。パイプラインの他の部分
(累積 signal、`build_known_facts` の把握済み事項)は履歴を見ているため、Jev だけが履歴を見ない
不整合の是正でもある。

- 組み立て: 過去の顧客発話を**時系列昇順**で前置し、最後に今ターンの発話を足して `\n` で連結する
  (実装: `server/src/api.rs::build_jev_state`。対象ターンと判定した後に
  `resolve_jev_has_enough_info` の中で組み立てる。Jev 無効・対象外のターンでは組み立てない)。
  履歴に顧客発話が無い(初回ターン)ときは今ターンの発話のみを渡す。今ターン・過去とも改行は 1 行へ
  潰す(「1 顧客発話 = 必ず 1 行」)が、**どちらも途中で切り詰めない**(今ターンの入力上限は
  `validate` の `MAX_MESSAGE_CHARS` が既に持つ)
- 過去の顧客発話の選択(`select_customer_history_for_jev`): customer 発話のみ → 改行潰しのみの
  正規化 → 正規化後に空になるものを除外 → 新しい側から最大 6 件(`MAX_HISTORY_TURNS`。把握済み
  事項の窓と件数を揃える) → **合計 2,000 字(`MAX_JEV_HISTORY_CHARS`)を超える間、古い側の発話を
  1 件ずつ丸ごと落とす**。1 件だけで超える発話も切り詰めず丸ごと落とす(過去発話 0 件になりうる。
  今ターンの発話は残る)。件数窓・予算超過のいずれかで 1 件でも落としたら `tracing::warn!` を
  1 回出す(`request_id` / `window_dropped_turns`(件数窓で落ちた件数) / `dropped_turns`(予算超過
  で落ちた件数) / `kept_turns` / `reason = "jev_history_budget"`。**発話本文は出さない**)。
  6 件の件数窓による除外も、予算超過と同じく warn に出す(固定長の窓だからといって対象外にしない。
  下記「既知の限界」参照)
- **表示用の要約予算を判定入力へ流用しない。** 把握済み事項用の
  `select_customer_history_for_known_facts` は 1 発話を 100 字 + `…` へ切り詰める(表示用)。これを
  Jev の判定入力に流用すると発話が途中で切れて意味が反転する。型番が 101 字目以降なら Jev から
  見えず `has_enough_info` が不当に**下がる**。より重いのは、訂正・否定(「ADC-V523 ではなく実際の
  型番は不明です」等)が 101 字目以降で切り落とされ、誤った型番だけが残って `has_enough_info` が
  不当に**上がる**場合で、症状のみの今ターンが「誤った型番 + 症状」に見えて、聞き返すべきところが
  即エスカレーションになる(しかも切り詰めは warn に出ず痕跡が残らない)。発話単位で丸ごと落とす
  方式はこの経路より厳密に安全(発話の一部だけ生き残って文意が変わることは無い)だが、**「安全側
  (has_enough_info が下がる側)にしか振れない」という保証は無い**。古いターンが後続ターンの言及を
  限定・否定しているケースでは、その古いターンが丸ごと落ちることで逆に `has_enough_info` が不当に
  **上がる**方向へ振れうる。例: 古い発話「後で例に出す ADC-V523 は他人の製品です。私の型番は不明
  です」が予算超過で丸ごと落ち、「ADC-V523 の症状」に触れる新しい発話だけが残ると、型番が確定して
  見えてしまう
- **既知の限界**: 上記の理由により、除外(件数窓・予算超過のいずれも)が起きたターンでは
  `has_enough_info` が会話全体から見て本来より高くも低くも出うる。これを検出・補正する仕組みは
  無い。そのため除外は必ず `tracing::warn!` に記録し、事後に該当リクエストの会話を突き合わせて
  再構成できるようにしている(是正済みの反例テストは `server/src/api.rs` の
  `build_jev_state_known_limitation_dropping_a_qualifying_older_turn_can_leave_a_misleading_model_reference`)
- **assistant 発話は含めない。** `has_enough_info` は顧客が伝えた情報の十分性を測る指標であり、
  聞き返し文など自社発話が判定を押し上げてはならない
- 送信範囲: `[jev] enabled = true` のとき TypeSafe へ送られる顧客発話は、今ターンの発話に加えて
  過去の顧客発話(最大 6 件・合計 2,000 字まで。発話は切り詰めず、超過分は古い側から丸ごと
  落とす)に広がる。送信が発生するのは対象ターン(上記トリガー条件)に限る点は変わらない

### 閾値と判定

config `[jev] enough_info_threshold`(既定 0.5)。`has_enough_info < enough_info_threshold`
かつ聞き返し予算内(`conv.clarify_turns < clarify_max_turns`)なら `Clarify`、それ以外は
`EscalationReply`(実装: `server/src/api.rs::decide_jev_hearing_action`、純関数)。

### フォールバック規律(fail-back、fail-closed にしない)

次のいずれでも、既存の `missing` ベースの `decide_reply_action` にそのまま委譲する(**応答内容**
は従来と同一になる。処理を止めない):

- `[jev] enabled = false`(`Harness.jev_client == None`)
- Jev の HTTP エラー(4xx/5xx)
- Jev のタイムアウト(`[jev] timeout_secs`)
- Jev の応答に `has_enough_info` の `noul` 回答が無い(欠落・型不一致)

失敗時の warn(`jev hearing evaluate failed`)は `error` に原因チェーン(`{err:#}`)を出すため、endpoint の URL が含まれうる(config 由来の公開値。endpoint に userinfo(`user:pass@`)・クエリ文字列(`?key=value`。空の `?` を含む)・フラグメント(`#token=...`。空の `#` を含む)のいずれかが含まれる構成は、資格情報がログに載るため、起動時検証 `JevClient::build` が fail closed で拒否する。フラグメントは reqwest がエラー文言へ実際に露出するかを実測していないが、`url::Url::as_str()` が保持する以上ログ経路が 1 つ増えるだけで漏れるため、露出の有無に関係なく拒否する。エラーメッセージに endpoint の値を出してよいのは、userinfo・クエリ・フラグメントのいずれも無いと確認できた後だけ(現状は scheme 拒否のメッセージのみ)で、parse 失敗・userinfo 拒否・クエリ拒否・フラグメント拒否のメッセージには endpoint の値を出さない。**パスは検証対象外**(正当な endpoint のパスと資格情報を機械的に区別できないため)なので、パスに資格情報を置くと reqwest のエラー文言にも scheme 拒否のメッセージにもそのまま載る。認証は env `TYPESAFE_API_KEY` で行い、URL には資格情報を置かない。本番の endpoint `https://api.typesafe.ai/v1/systemone` はクエリもフラグメントも持たないため影響しない)。API キー・顧客発話(`state`)・モデル生成文字列は含まれない(§3 の 3・5。API キーは `Authorization` ヘッダで送るため)。タイムアウトのうち `send()` 中(接続・リクエスト送出・応答ヘッダ受信まで)のタイムアウトは `jev evaluate api timed out after <timeout_secs>s` という固有の文言で判別でき(`jev.rs` が所有する安定した文字列で、reqwest / hyper の文言には依存しない)、接続拒否・DNS 失敗・TLS 失敗などそれ以外の送出エラーは `call jev evaluate api` の下に原因チェーンが続く。応答ヘッダ受信後の本文読み込み中(`response.chunk()`)のタイムアウトはこの固有文言が付かず、`read jev evaluate api response body` の下に原因チェーンが続く。

**応答時間は従来と同一にはならない。** `resolve_jev_has_enough_info` は `evaluate` を `await`
する同期呼び出しであり、トリガー条件(上記 3 条件)を満たした対象ターンに限り、Jev の 1 往復分
(実測 0.49〜0.67 秒、§1)だけ応答が遅延する。HTTP エラー時は 1 往復分、タイムアウト時は最大
`[jev] timeout_secs`(既定 3 秒)の遅延が乗る。`[jev] enabled = false` の構成、および対象外の
ターン(トリガー条件を満たさない)では Jev を一切呼ばないため、遅延は一切増えない。この遅延は
LINE アダプタの累積タイムアウト予算(`2026-08-11-answer-api-line-adapter-design.md` §6 の
「1 イベントあたり最大約 103 秒」という Accepted Risk)の内側に収まる。

実装は `server/src/api.rs::resolve_jev_has_enough_info` が `None` を返す経路として一元化し、
呼び出し側(`reply_handler`)は `Option<f64>` の `Some`/`None` で分岐するだけで、両経路を別々に
実装しない。

### 聞き返し文言

Jev 起点の `Clarify` では、マニュアルカバレッジ由来の `missing_to_text` ではなく固定文言
`JEV_HEARING_MISSING_TEXT`(「対象の製品の型番と、具体的な症状(いつから・どんな状態か)が
確認できていません。」)を使う。Jev は「顧客が話した情報の十分性」を見ており、`missing`
(マニュアル材料との一致度)とは無関係な指標のため。

### 監査

Jev を実際に使って判定したターンのみ、`evaluate()` が既に書いた通常の監査行
(`allowed:*`/`escalate:*`)に加えて、`Harness::record_jev_hearing_decision` が
`jev_hearing:clarify` / `jev_hearing:escalate` のラベルと `jev_has_enough_info`(WORM の
`AuditEvent` に加算したフィールド)を記録する。この監査書き込みの失敗は応答自体を失敗させない
(`record_out_of_scope_case` と同じ「応答継続を優先する」設計判断)が、`tracing::warn!` は残す。
Jev を呼ばなかった/使わなかったターンではこの新規監査行を書かない。

### 対象外(従来どおり `decide_reply_action` のまま)

- ヒアリング契約を宣言していない第1層ルール。**advisory でも対象外**(`contract-billing`:
  型番も症状も関係ない問い合わせのため。§7「ヒアリング契約」)。判定は従来どおり `missing`
  ベース(`decide_reply_action`)で、Jev は呼ばず顧客発話も TypeSafe へ送らない
- mandatory ルール(`security-incident` / `physical-damage` / `construction-risk` /
  `human-handoff`。宣言が付いていても `hearing_contract()` が無効化する)
- 第2層(禁止ドメイン)
- 第3層(`InsufficientDirectness` / `UnknownAddedSignal` を含む)
- `/{project_id}/api/reply` 以外の経路(MCP `evaluate_answerability` 等)。`decision::decide`
  自体は共有ロジックだが、`hearing` を見て Jev を呼ぶかどうかを決めるのは `api.rs::
  reply_handler` だけであり、MCP 経路は従来どおり `missing` ベースのまま

### 有効化条件の確認記録(2026-09-25、`config.cloudrun.toml` で `[jev] enabled = true` にした)

以下のうち、`hearing` 属性・トークン/レイテンシ再実測・E2E 別製品切替・履歴除外時の挙動の
4 項目は、元々「有効化前の確認項目」として列挙していたもの。閾値 `enough_info_threshold = 0.5`
の分離確認は、2026-09-25 に本番 Cloud Run を有効化するにあたり追加で実測した項目であり、
元々の「有効化前の確認項目」には無かった。以下は 2026-09-25 に確認・実測した結果を記録する。
**未消化の項目は消さず、未消化のまま残す。**

- **[消化済み] 本番 vegapunk の `EscalationRule` ノードに `hearing` 属性が入っていること。**
  ヒアリング契約は vegapunk 上のルールノードから実行時に読むため、`rules.json` を変えただけ
  では反映されない。`ingest-rules` job の成功を 2026-09-22 に確認済み(`upserted_nodes: 9`)。
  未反映・失敗の間は `warranty-failure` も「宣言なし」として読まれ、Jev を呼ばず従来の
  `missing` ベース判定へ fail-back する(安全側だが、この機能は効かない)。`ingest_rules` は
  未知の `hearing` 値・`binding` の省略・未知の `binding` 値・mandatory への `hearing` 宣言・
  未知のフィールド名を投入前に拒否するため、綴りミス・書き忘れは job の失敗として見える。
- **[消化済み] Jev の入力トークン・レイテンシの再実測。** §1 の実測値(5 問で入力 945 トークン
  等)は今ターン 1 発話だけを `state` に送った時点の測定だった。2026-09-21、実 TypeSafe に対し、
  本番と同じ 14 問・`build_jev_state` と同じ形の `state` で 2 段階測り直した。
  - state 324 字(小規模): 所要 0.52〜0.71 秒、入力トークン 1,217〜1,528(+311 トークン)。
  - state 7,070 字(worst case。内訳: 過去発話 `MAX_JEV_HISTORY_CHARS` = 2,000 字級 + 今ターン
    `MAX_MESSAGE_CHARS` = 5,000 字級。`server/src/api.rs` の該当定数を参照): 3 回とも HTTP 200、
    所要 1.25 秒 / 0.64 秒 / 0.69 秒(1 回目が最も遅い)、入力トークン 7,894(3 回とも同一)、
    出力トークン 389(3 回とも同一)。`has_enough_info` は 0.87(型番と症状が揃った state のため
    エスカレーション側になる想定どおりの値)。
  - state の長さが小規模(324 字)→ worst case(7,070 字)へ約 22 倍になっても、入力トークンは
    約 6.4 倍(1,217〜1,528 → 7,894)、レイテンシは最悪でも 1.25 秒に収まった。
  - **結論: `[jev] timeout_secs = 3` は worst case(最悪観測値 1.25 秒)でも余裕がある。**
- **[消化済み] 閾値 `enough_info_threshold = 0.5` の分離確認。** 2026-09-21、実 TypeSafe に対し、
  聞き返し側とエスカレーション側それぞれを想定した発話パターンで `has_enough_info` を実測し、
  0.5 を挟んで分離することを確認した: 症状のみ 0.13 / 型番のみ 0.09 / 訂正あり全文 0.13(聞き
  返し側)、履歴に症状+今ターン型番 0.80 / 同じ発話を 100 字で切った版 0.78 / 長い履歴 0.85
  (エスカレーション側)。顧客発話の本文はこの記録に残さない(§3 の 3 と同じ規律)。
- **[未消化] E2E: 同一利用者が別製品の相談へ切り替えるケース。** `/api/reply` の `history` は
  同一 case・同一相談対象に限る契約(`2026-08-11-answer-api-line-adapter-design.md` §2)だが、
  現状の LINE アダプタは 60 分 TTL での失効のみで、相談対象の切り替えを検知してリセットする
  機構を持たない(根本原因は Issue #61)。切り替え後も古い製品名が `history` に残ると、古い
  製品名 + 今ターンの症状で `has_enough_info` が不当に上がり、誤った製品文脈で即時
  エスカレーションしうる。Issue #61 の対応後、実会話で挙動を確認する。
- **[未消化] 履歴の除外が起きたターンの挙動確認。** 件数窓(`MAX_HISTORY_TURNS`)・予算超過
  (`MAX_JEV_HISTORY_CHARS`)のいずれかで過去発話が丸ごと落ちた実会話を使い、`has_enough_info` が
  不当に上振れ(誤って `EscalationReply` 側に倒れる)していないかを、warn ログの
  `window_dropped_turns` / `dropped_turns` と実際の会話内容を突き合わせて確認する必要がある
  (「Jev へ渡す state」の既知の限界を参照)。本番投入後の実会話蓄積を待って確認する。
