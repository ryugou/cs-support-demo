# 会話のリズム改善実装(Issue #34, conversation rhythm)

## 背景

`server/` 配下はホームセキュリティアドバイザ AI(homesec)の Rust 実装。本番の実会話スクリーンショットで次の実害が確認されている。

- (a) 「特に心配な場所は?」と質問で締めたターンに商品カードが付き会話が噛み合わない
- (b) 助言の一要素(防犯フィルム等、提案の主役でない商品)までカード化される
- (c) カードのボタンが「聞く」だけで、タップ後の回答が行き止まり(特に他社商品)
- (d) チップと質問の連発が尋問的

設計の正本は `docs/superpowers/specs/2026-08-17-homesec-advisor-design.md`(特に §2.4 会話の締めの動作、§3.3 API契約のquick_replies/カードボタン、§6 処理順手順8、§7.2 カード添付判定)。本ファイルはこの設計書の該当節を実装するための作業spec。ブランチは `feat/34-conversation-rhythm`(既存、切り直し禁止)。

実装対象の既存ファイル(すでに調査済み、行番号は目安):

- `server/src/advisor/draftgen.rs` — LLM Call#2 のプロンプト構築・呼び出し・出口関門(`build_advisor_system_prompt`, `build_advisor_user_message`, `draft_advisor_reply`, `apply_advisor_output_gates` 等)
- `server/src/advisor/cards.rs` — 製品カード判定(`ProductCard` 構造体、`select_cards`、`matches_final_text`、`BUTTON_TEXT` 定数、`resolve_image_url`)
- `server/src/advisor/quick_replies.rs` — `QuickReplyItem` 構造体、`for_clarify`/`for_time_pref`、`MAX_QUICK_REPLIES`(現状6)、`truncate_label`、`cap` ヘルパー
- `server/src/advisor/api.rs` — `AdvisorReplyResponse`、`advisor_reply_handler`、`should_select_cards`、`should_burn_lead_offered`、`merge_support_case_attrs`、Call#2 を呼ぶラッパー(`draft_with_materials` 等、grep で特定すること)
- `server/src/advisor/decide.rs` — `AdvisorAction`、`AdvisorCaseAttrs`(`lead_offered`/`lead_requested`/`shown_product_cards`)、`parse_advisor_case_attrs`/`advisor_attr_updates`、`CONDITION_ATTR_KEYS`
- `server/src/advisor/materials.rs` — `AdvisorMaterial`(`material_key`/`kind`/`card_description`/`card_match_terms`/`product_key`/`title_ja`/`product_page_url` 等)
- `server/src/bin/line_adapter.rs` — `CardPayload`(現状 `button_text`/`button_message` は単一String)、`build_flex_bubble`、`build_flex_message`、`QuickReplyPayload`、`build_quick_reply`
- `schema/homesec.yml`、`schema/cs-schema.yml`、`schema/cs-support.yml` — `nodes.support_case.attributes` に advisor 加算属性(`lead_offered`/`lead_requested`/`shown_product_cards`/`advisor_cond_*`)が複製されている
- `server/src/harness/knowledge.rs`(1943-2064行付近) — `written_support_case_attribute_keys`(テストヘルパー、書き込む全属性キーを列挙)と、3schema分の一致テスト(`cs_schema_yml_declares_...`/`cs_support_yml_declares_...`/`homesec_yml_declares_...`)

実装前に必ずこれらのファイルを実際に読み、現在の関数シグネチャ・命名規約に合わせて変更すること(このspecの関数名は目安であり、実コードのAPIを正とする)。

## 要件

### 1. Call#2 構造化メタの分離(draftgen.rs)

`DraftMode::Answer` のときだけ、LLM に応答本文の後ろへ区切り行 + JSON メタを出力させる。`DraftMode::Clarify` はメタを要求しない(現状どおり本文のみ)。

- 区切りマーカーは固定文字列 `<<<ADVISOR_META>>>`。system prompt に「本文を書き終えたら、改行してこの行だけを書き、次の行に JSON を1行で出力せよ。JSON より後に文章を書くな」という指示を追加する(Answer モードのみ)。
- JSON の形:
  ```json
  {"featured": ["own_product:adc-v724"], "closing": "proposal", "choices": []}
  ```
  - `featured`: このターンで主役として提案した商品の `material_key` 配列
  - `closing`: `"question_choice"` / `"question_open"` / `"proposal"` のいずれか
  - `choices`: `closing == "question_choice"` のときだけ、顧客が選べる短い回答候補(最大4件・各20字目安)。それ以外は空配列にするようプロンプトで指示する
- パース処理: LLM の生テキストから **最初に出現する** `<<<ADVISOR_META>>>` の位置で分割する。
  - マーカーが見つからない場合: 本文 = 生テキスト全体(trim)、メタ = None(fail-soft)
  - マーカーが見つかった場合: 本文 = マーカーより前の部分(trim)。**この分割はマーカー以降の JSON が壊れていても必ず行う**(本文にメタ断片を絶対に残さないため)。マーカーより後ろを JSON としてパースを試み、失敗(不正JSON・`closing` が3値以外・型不一致)したらメタ = None、成功したらメタ = Some(DraftMeta)
- 出口関門(`apply_advisor_output_gates`、NG辞書・Markdown除去・URL allowlist・型番allowlist)は **分離後の本文にのみ** 適用する(メタJSON文字列はゲートを一切通さない)
- Call#2 の呼び出しラッパー(api.rs から呼ばれている関数)の返り値に、本文とメタ(`Option<DraftMeta>`)の両方を含めるようシグネチャを拡張する
- `DraftMeta` 型定義(featured: Vec<String>, closing: enum{QuestionChoice, QuestionOpen, Proposal}, choices: Vec<String>)は draftgen.rs に置く
- 追加のプロンプト規則(Answer モードのみ、常時注入):
  - 締めは「質問」か「提案」のどちらか1つ
  - 質問で締めるのは、答えで次の提案が変わるときだけ
  - ボタン起点の質問(「詳しく聞く」「選び方を聞く」「導入を相談したい」等への返信ターン)への応答は「状況適合 → 要点 → 次の一歩(他社製品なら入手方法・頼み方、自社製品なら担当者相談への誘い)」で締め、行き止まりの返信にしない

### 2. question_streak(decide.rs / api.rs / 3 schema ファイル)

- `support_case` の新規 int 属性 `question_streak` を追加。3つの schema yml(`homesec.yml`/`cs-schema.yml`/`cs-support.yml`)の `support_case.attributes` に、既存の int 属性(`clarify_turns` 等)と同じ YAML 形式で追記する
- `decide.rs` の `AdvisorCaseAttrs` に `question_streak: i32` を追加し、`parse_advisor_case_attrs`/`advisor_attr_updates` を対応させる(既存の int 属性の読み書き方式に合わせる。無ければ既存コード内で他の int 属性がどう文字列⇔数値変換されているか探して合わせる)
- 加算・リセットは **`DraftMode::Answer` の Call#2 が実際にメタ付きで成功したターンのみ** 行う(handoff/safety/out_of_domain/time_pref/lead/clarify/fallback の各ターンは `question_streak` を変更しない。fallback = 出口関門違反や Call#2 失敗で応答が定型文に差し替わったターンも含む):
  - `meta.closing != Proposal` → `question_streak = 現在値 + 1`
  - `meta.closing == Proposal` → `question_streak = 0`
- `question_streak >= 2` の状態で Answer モードの Call#2 を呼ぶ場合、system prompt に「このターンは質問で締めず、いま分かっている情報での提案 + 継続誘導で締めよ」という追加指示を注入する(値はプロンプトに埋め込む)
- `server/src/harness/knowledge.rs` の `written_support_case_attribute_keys` に `question_streak` を追加し、3つの一致テストが無変更で PASS することを確認する

### 3. カード添付判定の起点を featured に変更(cards.rs)

- `select_cards` 相当の関数に、`meta: Option<&DraftMeta>`(または `featured: &[String]` + `closing: Option<ClosingKind>`)を渡せるようシグネチャを拡張する
- **`closing` が `Proposal` でない場合(meta が None の場合も含む)、カードは一切出さない(空配列を返す)**
- `closing == Proposal` のとき、候補は `meta.featured` に列挙された `material_key` のうち:
  1. 今回注入した own_product/partner_product 材料(`card_description` を持つもの)に実在する
  2. 出口関門通過後の最終応答文で言及されている — 判定方法は既存の `matches_final_text` をそのまま流用(own_product は型番明示のみ、partner_product は `card_match_terms` 照合、両方とも変更しない)
  の両方を満たすものだけ
- 上記を通過した候補に対して、既存の `shown_product_cards` 除外・own_product優先ソート・最大3件のロジックは変更せず適用する
- 既存テスト(型番明示判定・card_match_terms判定・3件上限・shown_csv除外等)は、候補集合の決め方が変わる分だけ `featured` 配列と `closing: Proposal` を渡す形にシグネチャを追随させてよい(判定ロジック自体の意味は変えない)

### 4. カードボタンの型付け(cards.rs / api.rs / line_adapter.rs)

`ProductCard` の `button_text`/`button_message`(単一String)を廃止し、`buttons: Vec<CardButton>` に置き換える。

```rust
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardButton {
    Uri { label: String, url: String },
    Message { label: String, message: String },
}
```

JSON シリアライズ形:
```json
{"kind":"uri","label":"商品ページを見る","url":"https://..."}
{"kind":"message","label":"詳しく聞く","message":"ADC-V724について詳しく教えて"}
```

組み立てルール(server 側、`select_cards` またはカード構築箇所):
- `product_page_url` がある場合、先頭に `Uri{ label: "商品ページを見る", url: product_page_url }`
- `kind == own_product` の材料: `Message{ label: "詳しく聞く", message: format!("{title}について詳しく教えて") }` → `Message{ label: "導入を相談する", message: format!("{title}の導入を相談したい") }` の順で2つ
- `kind == partner_product` の材料: `Message{ label: "選び方を聞く", message: format!("{title}の選び方を教えて") }` の1つだけ

`line_adapter.rs` の `CardPayload` を、`buttons: Vec<ButtonPayload>`(`#[serde(default)]` でフィールド欠落時は空Vec、デシリアライズできなかった不正要素があってもクラッシュしない fail-open)に変更する。`build_flex_bubble` は `buttons` を footer のボタン列(URI action / message action)へ写像する。**`buttons` が空(欠落含む)のときは、そのバブルを丸ごとスキップする**(既存の「button_text/button_message が空ならバブルをスキップ」という fail-soft 挙動を踏襲。パニックしないことが必須)。

### 5. quick_replies(quick_replies.rs / api.rs)

- `MAX_QUICK_REPLIES` を 6 → 4 に変更(`for_clarify`/`for_time_pref` はこの定数を使った既存の `cap()` 経由で自動的に4件に切り詰められる。concern語彙が5件ある場合、既存テストの期待件数を4件に更新してよい — これは仕様変更に伴う正当な追随であり、テストの意味自体は変えない)
- 新規関数(例: `for_answer(meta: Option<&DraftMeta>) -> Option<Vec<QuickReplyItem>>`): `meta.closing == QuestionChoice` かつ `choices` が非空のときだけ、`choices` をコード側で検証(重複除去 → 最大4件 → 各20字に切り詰め、既存の `truncate_label`/`cap` を再利用)して `QuickReplyItem{ label: 切り詰め後, message: 元の候補文字列 }` の配列を返す。それ以外(closingがquestion_open/proposal、metaがNone)は `None`
- `closing == QuestionOpen` や `closing == Proposal` のターンにはチップを一切出さない(既存の `for_clarify`/`for_time_pref` はこれまでどおり該当ターンでのみ呼ばれる。answer 経路の呼び出し元を変更する場合は、reply_kind == "answer" のときだけ上記新規関数を呼ぶよう `api.rs` を更新する)

### 6. AdvisorReplyResponse / API 契約(api.rs)

- `product_cards` の各要素は上記 `buttons` 配列を持つ形にシリアライズされる
- `quick_replies` は上記の通り answer/clarify/time_pref の各経路から生成された結果を格納する
- `should_select_cards` 等の既存ゲート(fallback除外等)はそのまま維持し、カード生成自体の可否は上記3節のロジックに委ねる

## 制約

- `cargo` コマンドは必ず `--manifest-path server/Cargo.toml` を使う(`cd` 禁止)
- テスト先行(Red→Green)で実装すること
- commit はしないこと。作業ツリーに変更を残すだけでよい(レビュー後に別エージェントがコミットする)
- push・PR作成は行わない
- 既存テストは無変更で PASS すること。ただし本 spec の構造変更(quick_repliesの上限6→4、cards.rsのシグネチャ変更等)に伴い、期待値の**意味を変えない範囲**での既存テストの追随(件数の更新等)は許容する
- スコープ外の機能追加・リファクタはしない

## 必須テスト(すべて新規または既存追随で用意すること)

1. **メタ分離**: 正常系(マーカー+正しいJSON)、パース失敗系(マーカーはあるがJSON不正/closing値が不正)、**分離後の本文にメタ断片(マーカー文字列やJSON片)が一切残らないことを検証するテスト**(パース失敗ケースでも本文が汚染されないことを含む)
2. **featured検証**: 注入材料に存在しない material_key は破棄される、本文で言及されていない material_key は破棄される
3. **closing別の出し分け**: closingがquestion_choice/question_open/proposalそれぞれでカード・チップの有無が仕様どおりになること(proposal以外はカード無し、question_choice以外はチップ無し)
4. **buttons組み立て**: own_productで2ボタン(詳しく聞く+導入を相談する)、partner_productで1ボタン(選び方を聞く)、product_page_url有りで先頭にURIボタンが追加されること
5. **question_streakの遷移**: closingがquestion系連続で+1、proposalで0にリセットされること。fallback/clarify等の非Answerターンで変化しないこと。schema一致テスト(`written_support_case_attribute_keys`関連)が無変更でPASSすること
6. **line_adapterのbuttons写像**: buttons配列がFlexのfooterボタンへ正しく写像されること、buttonsが空/欠落のときバブルがパニックせずスキップされること

## 完了条件・検証方法

1. `cargo fmt --manifest-path server/Cargo.toml`
2. `cargo test --manifest-path server/Cargo.toml advisor 2>&1 | tail -60`
3. `cargo test --manifest-path server/Cargo.toml --bin line_adapter 2>&1 | tail -60`(もし bin 単体フィルタが効かない場合は `line_adapter` を含むテスト名でフィルタする代替コマンドを使ってよい)
4. `cargo test --manifest-path server/Cargo.toml 2>&1 | tail -80`(全体、最後に1回)
5. `cargo fmt --manifest-path server/Cargo.toml --check`

すべて成功すること。**コマンド出力の全文をそのまま報告に含めず、末尾の要約(失敗があればエラー行、成功なら最終サマリ行)だけを含めること。**

## 報告してほしい内容

- 変更したファイル一覧(`git status --short` 相当)
- 上記5コマンドの末尾出力(成功/失敗が分かる範囲)
- 新規追加・変更した主要な型/関数のシグネチャ
- 設計上の判断で自己完結できなかった曖昧点があれば明記(勝手に非自明な判断をして実装を進めない。判断が必要な場合はここで報告して差し戻しを受けること)
