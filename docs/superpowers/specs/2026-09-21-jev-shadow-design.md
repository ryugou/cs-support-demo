# Jev(TypeSafe System One)シャドー運転

| 項目 | 内容 |
| --- | --- |
| 目的 | 顧客発話の「理解」を型付き + 確率で得る外部判断器(Jev)を、本番の判定を変えずに並行実行し、較正を測るための記録を残す |
| 読者 | 実装エージェント、レビュー担当 |
| 正本の範囲 | Jev クライアントの契約、質問定義の置き場、起動点と fail-soft 規律、記録の形、config と secret |
| 関連文書 | `2026-08-11-answer-api-line-adapter-design.md`(/api/reply 契約)、GitHub Issue #56 |

**2026-09-21 追記**: Issue #58 により、第1層 advisory の聞き返し判定にのみ Jev の
`has_enough_info` を使うよう変更した(詳細 §7)。他の経路は引き続き §1〜§6 の shadow-only の
まま。

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

**§7 の経路はこの限りではない**: 第1層 advisory の聞き返し判定に限り、Jev の結果(`has_enough_
info`)を判定に使い(2 と矛盾)、対象ターンの応答時間は Jev の 1 往復分だけ増える(1 と矛盾)。
これは Issue #58 で意図的に導入した例外であり、詳細と許容根拠は §7 を参照。1 と 2 は §7 の
経路(第1層 advisory のエスカレーションターン)では成立しない。3・4 は §7 の経路においても
真のまま(顧客発話の本文はログに書かず、`[jev] enabled = false` の構成では Jev へ一切
アクセスしない)。

## 4. テスト

- 無効時: クライアントが構築されず、呼び出しも発生しない
- 有効時: スタブで成功・HTTP エラー・タイムアウトを与え、いずれも応答本文と `reply_kind` が不変であること
- パース: noul / choice / score の 3 型、未知フィールド、壊れた JSON(失敗として扱い応答に影響しない)
- ログ: 顧客発話の本文が出力に含まれないことを固定

## 5. 運用

- 本番有効化の手順: secret `typesafe-api-key` を作成 → ランタイム SA へ `secretAccessor` を付与 → service へ注入 → `config.cloudrun.toml` の `[jev] enabled = true`
- 顧客の発話が第三者(TypeSafe)へ送信される。CLAUDE.md の Anthropic に関する記載と同じ扱いで明記する

## 6. 次段階(本タスクの範囲外)

第一層 advisory の聞き返し判定は §7 で先行実施済み。本節の残り(緊急・製品・担当者要望への
拡張)は引き続き未着手。

実会話 1〜2 週間の記録で較正を確認したのち、判定の置き換えへ進む。初期の閾値案(すべてコード側):

- 緊急: 0.5 以上で安全案内(見落としのコストが高いため低めに置く)
- 故障: 0.8 以上 かつ 情報十分 0.6 以上 → エスカレーション / 0.8 以上 かつ情報不足 → ヒアリング / 0.3〜0.8 → 通常の回答を試みる
- 製品: 信頼度 0.8 以上 → 確定 / 0.4〜0.8 → 型番の確認 / 未満 → 言及なし。他社製品の断りは 0.8 以上のときだけ
- 担当者要望: 0.8 以上

複数の型番に言及されたケースは Choice では 1 つしか返らないため(実測: V724 0.84 / V523 0.05)、置き換え時は**型番ごとの Noul** に変更する。型番の音写(「びーけーごーにーさん」)は解決できない(実測: 製品なし 0.93)。

## 7. 段階A: 第一層 advisory の聞き返し判定への適用(Issue #58、実装済み)

### 背景

本番実測(2026-09-21)で、「電源が入らなくなった」という問い合わせが第1層の明示エスカレー
ションルール(advisory binding)にマッチしたが、そのルールの `missing`(マニュアル材料の
カバレッジ不足で決まる)がたまたま空になり、聞き返し(Clarify)に入らず即エスカレーション
していた。`missing` は「マニュアル側の根拠十分性」であって「顧客が話した情報の十分性」では
ないため、この経路に限り、後者を直接測る Jev の `has_enough_info`(質問定義
`server/data/urtect/jev-questions.json`、`criteria.true`: 「対象の製品と具体的な症状の両方が
分かる」)に置き換えた。

### トリガー条件

次の 3 条件をすべて満たすターンでだけ、Jev を `await` して使う(毎ターンではない)。

1. `layer == 1` かつ `rule_binding == Some(Binding::Advisory)` の `Escalate` であること
   (mandatory ルール・第2層・第3層は対象外。`missing` の中身には依存しない — これが本是正の
   核)
2. 今ターンの signal 抽出が `ExtractionMode::LexiconFallback` に落ちていないこと(抽出 LLM
   不調時は `decide_reply_action` 自身が fail-closed で `EscalationReply` に倒すため)
3. `conv.is_already_escalated()` でないこと(確定済み case は `decide_reply_action` 自身が
   `!conv.is_already_escalated()` ガードで `EscalationReply` に倒すため、Jev を呼んでも結果を
   使わず無駄な待ちとコストが発生するだけ)

判別は `AnswerDecision::Escalate` に新設した `rule_binding: Option<harness::rules::Binding>`
フィールドで行う(`route_to` は mandatory/advisory のどちらでも `"support_desk"` になりうる
ため、route では拘束度を判別できない)。実装は `server/src/api.rs` の
`is_layer1_advisory_escalate` / `resolve_jev_has_enough_info`。

`layer == 1` の選択自体(`rules::match_layer1`)は、マッチした全ルールのうち
`Binding::Mandatory` を `Binding::Advisory` より必ず優先する(同一 binding 内は配列の
先頭優先)。したがって advisory と mandatory の条件が同時に成立するターン(累積 signal
により実際に起こりうる)でも `rule_binding` は `Mandatory` になり、Jev は呼ばれない。
この優先順位が mandatory の即時エスカレーション契約を担保しており、`rules.json` の
並び順を変えてもこの契約は壊れない(Issue #58 の codex レビューで、advisory が後続
mandatory を覆い隠す経路が Critical として指摘され是正した)。

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

- mandatory ルール(`rule_binding == Some(Binding::Mandatory)`)
- 第2層(禁止ドメイン)
- 第3層(`InsufficientDirectness` / `UnknownAddedSignal` を含む)
- `/{project_id}/api/reply` 以外の経路(MCP `evaluate_answerability` 等)。`decision::decide`
  自体は共有ロジックだが、`rule_binding` を見て Jev を呼ぶかどうかを決めるのは `api.rs::
  reply_handler` だけであり、MCP 経路は従来どおり `missing` ベースのまま
