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
