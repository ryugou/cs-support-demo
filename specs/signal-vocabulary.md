# Signal 語彙仕様（初版 / 化粧品・健康食品カテゴリ限定）

Production CS MCP の 3 層判定・照合・egress は全てこの語彙の上に乗る（specs/production-cs-mcp.md S1-9）。
語彙は PunkRecord の `Signal` ノードの `value` として格納される。表現ゆれの吸収（軸1）は
`server/data/signal-lexicon.json` の surface_forms で行う（Step 1 は決定論 lexicon、S1-11）。

## class の意味

- `hazard`: 安全に関わる条件語。stakes 判定（S1-6 の mid 昇格）に使う。
- `context`: 質問型・文脈の条件語。stakes には効かないが照合の条件にはなる。

## 語彙（初版 12 語）

| signal | class | 意味 | surface_forms 例 |
|---|---|---|---|
| discoloration | hazard | 変色 | 変色, 色が変わ, 色味がおかし, 茶色くな, 黒ずん |
| mold | hazard | カビ | カビ, かび, 白い綿, ふわふわした付着 |
| foreign_substance | hazard | 異物混入 | 異物, 虫が入, 髪の毛が入, 破片 |
| odor_abnormality | hazard | 異臭 | 異臭, 変な匂い, 変なにおい, 酸っぱい匂い |
| post_ingestion_symptom | hazard | 摂取後の体調異常 | 食べたら, 飲んだら気分, 腹痛, 吐き気, 下痢 |
| skin_irritation | hazard | 肌トラブル | ピリピリ, かぶれ, 赤み, かゆみ, 湿疹, ヒリヒリ |
| allergy_concern | hazard | アレルギー懸念 | アレルギー, アレルゲン |
| efficacy_claim | hazard | 効果効能への言及 | 効果, 効能, 治る, 痩せ, 改善します |
| dosage_for_condition | hazard | 症状・状態に応じた用量相談 | 妊娠中, 授乳中, 持病, 服薬中, 子供に飲ませ |
| continue_use_question | context | 継続使用可否の相談 | 使い続けて, 続けて大丈夫, 食べてもいい, 飲んでもいい |
| expiry_question | context | 期限・保存の相談 | 賞味期限, 消費期限, 保存方法, 開封後 |
| refund_request | context | 返金・返品の要望 | 返金, 返品, 交換して |

## 運用ルール

- 語彙の追加は加算のみ。既存 signal の削除・意味変更をしない（I3 と同じ規律）。
- surface_forms は `resolve::normalize_key` 正規化後の部分一致で照合される。
- 既知の限界: 辞書外の表現は取りこぼす。第2層の raw text パターン照合と全件人承認で吸収する（S1-11）。
- **本初版は 2026-07-03 時点のドラフト。業務担当のレビューで確定させること。**

## `llm_only` / `description` フィールド（加算）

`server/data/signal-lexicon.json` および `server/data/urtect/signal-lexicon.json` の各 signal エントリに、以下 2 フィールドを加算した（既存フィールドの削除・意味変更なし）。

- `description`（string, optional, default 空文字）: signal の語義・限定条件を短文で記す。`LexiconNormalizer::vocabulary_for_prompt()` が `signal (class): description` の 1 行形式で全 signal を列挙する際に使う。今後の LLM 分類器がこの語彙一覧をプロンプトに埋め込み、signal 抽出の根拠として参照する。
- `llm_only`（bool, optional, default `false`）: `true` の signal は決定論的な文字列照合（surface_forms 部分一致）の対象から除外される。`surface_forms` は空配列でよく、既存の「有効な surface form が 1 つも無い signal は拒否する」バリデーション（サイレント never-match 防止）は `llm_only = false` のエントリにのみ適用される。`llm_only = true` の signal は `class_of` / `contains_signal` / `vocabulary_for_prompt` には引き続き登録され、LLM 抽出結果としてのみ `SignalSet` に現れる。

## `unclassified_risk`（catch-all, llm_only, hazard）

両 lexicon に `unclassified_risk`（`class: hazard`, `llm_only: true`, `surface_forms: []`）を追加した。これは S1-1「疑わしい語は signal を立てる」の実装であり、既存のどの signal にも分類できないが安全・契約・法務・製品破損などの懸念を含みうる発話を、LLM 抽出器が「分類に迷う場合はとりあえず立てる」ための catch-all である。

- 文字列照合（surface_forms 部分一致）には一切使われない。決定論 lexicon の `normalize()` からは絶対に出力されない。
- LLM 分類器が `vocabulary_for_prompt()` の語彙一覧を見て、既存 signal に当てはまらないが hazard 相当と判断した発話に対してのみ立てる。
- `class: hazard` のため、stakes 判定（S1-6 の mid 昇格）に効く。疑わしい発話を誤って `context` 扱いで握りつぶさないための設計。
