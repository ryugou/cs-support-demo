# Signal 語彙仕様（初版 / 化粧品・健康食品カテゴリ限定）

Production CS MCP の 3 層判定・照合・egress は全てこの語彙の上に乗る（specs/production-cs-mcp.md S1-9）。
語彙は PunkRecord の `Signal` ノードの `value` として格納される。表現ゆれの吸収（軸1）は
`server/data/signal-lexicon.json` の surface_forms で行う（Step 1 は決定論 lexicon、S1-11）。

## class の意味

- `hazard`: 安全に関わる条件語。stakes 判定（S1-6 の mid 昇格）に使う。
- `context`: 質問型・文脈の条件語。stakes には効かないが照合の条件にはなる。

## 語彙（初版 12 語 + `human_handoff_request` 追加分、計 13 語）

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
| human_handoff_request | context | 担当者への取次・有人対応を明示的に希望する発話 | 担当者につないで, 担当者に代わって, オペレーターにつないで, 有人対応, 人と話したい |

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

## `human_handoff_request`（取次依頼、第一層 escalation）

両 lexicon に `human_handoff_request`（`class: context`）を追加し、`server/data/urtect/rules.json` の `escalation_rules` に `rule_id: "human-handoff"`（`condition: ["human_handoff_request"]`, `owner: "support_desk"`, `route: "support_desk"`, `binding: "mandatory"`）を追加した。聞き返し（clarification）中に顧客が「担当者につないでほしい」等の明示依頼をした場合、この signal が第一層マッチを成立させて `clarification_allowed = false` となり、ターン残数に関係なく第4節（エスカレーション）へ落ちる。安全 hazard ではなく業務都合の相談系 signal のため `class: context` とした。設計判断の詳細は `docs/superpowers/specs/2026-08-12-conversation-flow-v11-design.md` §3 を参照。

**この保証が成立する根拠は `binding: "mandatory"` である（Issue #54 改訂、reviewer 一次レビュー Critical 1）**。`decision::decide()` は第1層マッチ時に `rule.binding` を見て分岐するようになっており、`binding = advisory` のルールは情報不足（マニュアル一致度が閾値未満）なら `clarification_allowed = true`（聞き返し許可）になりうる。`human-handoff` が `clarification_allowed = false` を保つのは、この signal 自体の性質ではなく `binding: "mandatory"` という設定値によるものであり、将来この値を `advisory` に変更すると「聞き返しループからの脱出手段」という設計意図（本節冒頭）が壊れる。変更する場合は `docs/superpowers/specs/2026-08-12-conversation-flow-v11-design.md` §2・§3 の再検討が必須。

surface_forms の照合は正規化後の単純部分一致（`server/src/harness/signal.rs`）のため、`人に代わって` のような広い形を含めると「本人に代わって問い合わせています」のような代理問い合わせにも部分一致し、第一層 mandatory エスカレーションへ誤って落ちる（回答も聞き返しも行われず全件人手に回る）。このため `人に代わって` 単独は採用せず、`人に代わってください` / `人に代わってほしい` 等の依頼形に限定してある。

**既知の限界（受容済みリスク）**: 対応する escalation_rule は `server/data/urtect/rules.json`（本番 urtect スキーマ）にのみ投入し、`server/data/rules.sample.json`（sivira-cs-demo 用）には追加していない。本番の project 定義は urtect の1件のみのため実害は無いが、local/demo 構成（`config.toml` 等が読む root `signal-lexicon.json`）で「担当者につないで」を試すと、第一層に落ちず第三層グレー（`UnknownAddedSignal` → `clarification_allowed = true`）へ回り、聞き返しループを誘発する逆挙動になる。demo 環境での動作確認結果を本番の参考にしないこと。

## `price_question`（公開価格表への問い合わせ、Issue #73）

本番実測（2026-10-03）: 「月額いくらですか？」が `contract_billing_question` の surface_forms に含まれていた「料金」「月額」にヒットし、第1層 `contract-billing`（advisory）でエスカレーションされていた。マニュアル側には該当記事が score 1.0 でヒットしていたにもかかわらず、第1層が先に確定するため材料が使われなかった。単なる価格の問い合わせ（買う前の人が公開情報を聞いているだけ）と、契約・解約・請求といった個別案件は別の性質であり、取次の要否も異なる。

この区別のため、`server/data/urtect/signal-lexicon.json` の `contract_billing_question` から価格照会に当たる surface_forms（「料金」「月額」）を外し、新設の `price_question`（`class: context`）へ移した。

**「加算のみ」規律（運用ルール節）に対する意図的な例外**: 直前の削除は運用ルール節の「語彙の追加は加算のみ。既存 signal の削除・意味変更をしない」に正面から抵触する。例外とした理由: 「料金」「月額」は本番で実害（公開価格表への単純な問い合わせが第1層 `contract-billing` で取次になり、score 1.0 の材料が使われない）を出しており、加算だけでは解消できない。`match_layer1`（`server/src/harness/rules.rs`）はルールの `condition` が signal 集合の subset かどうかで判定するため、`price_question` を加算しても `contract_billing_question` が立ち続ける限り `contract-billing` ルールへのマッチは消えない。今後この種の surface_forms 削除を行う場合は、削除によって取次から漏れる発話を実データで洗い出し、個別案件側へ代替の surface_forms を加算してから行うこと（本件では下記「第1層の取次床を広げた加算」の節で `contract_billing_question` へ代替語を加算済み）。

### `contract_billing_question` と `price_question` の役割分担

価格語（`price_question` の surface_forms）と個別案件語（`contract_billing_question` の surface_forms）は独立の語彙集合であり、どちらか一方だけを立てる発話もあれば、両方を立てる発話もある。

- 価格語のみ（例:「月額いくらですか？」）: `price_question` だけが立ち、第1層のどのルールにもマッチしない（`price_question` は `rules.json` の condition に含まれない）ため、第1層を通過し、材料が十分な場合に回答される（材料が不足する場合は第3層で取次になりうる）。
- 個別案件語のみ（例:「解約したいです」）: `contract_billing_question` だけが立ち、第1層 `contract-billing`（advisory）へ落ちる。
- 価格語 + 個別案件語（例:「月額料金が引き落とされていないのですが」）: 両方の signal が立つ。`match_layer1` は `rule.condition.is_subset(question)` という subset 判定のため、`contract_billing_question` が累積 signal 集合に含まれていれば `price_question` が同時に立っていても `contract-billing` にマッチする。価格語の有無は個別案件の取次を妨げない。

### 第1層の取次床を広げた加算（reviewer 差し戻し Critical 1、2026-10-03）

main の `contract_billing_question` の surface_forms は `解約` `契約` `料金` `プラン変更` `月額` `支払い` の6語だった。「料金」「月額」を外した直接の副作用として、価格語を含むが残る4語（`解約` `契約` `プラン変更` `支払い`）のいずれも含まない個別案件（引き落とし・二重請求・払いすぎ・未払い・滞納・明細・内訳・料金プランの見直し等）が、第1層のどのルールにもマッチしなくなる。第2層 `prohibited_domains` は `legal-privacy` / `security-guarantee` / `electrical-work` の3件のみで請求系を一切カバーしないため、これらは第3層のマニュアルスコア判定に落ち、「価格表で回答済み」にされうる fail-closed の破れになる。

これを塞ぐため、`contract_billing_question` の surface_forms は次の語集合とする（現行 lexicon の全件。main の `解約` `契約` `プラン変更` は維持。main の `支払い` は上位集合 `支払` へ拡張）:

```
"解約", "契約", "プラン変更", "プランを変更", "プランを見直", "プランの見直", "プランを変え", "プランの変更", "支払", "決済日", "請求額", "請求書", "請求され", "請求が", "請求金額", "請求内容", "二重請求", "月の請求", "月分の請求", "請求を止", "請求が来", "引き落と", "口座", "返金", "払いすぎ", "過払い", "未払い", "滞納", "二重に取られ", "二重に引", "二重払い", "料金が上が", "料金を返", "代金を返", "明細", "内訳", "クレジットカード", "カード情報", "値上", "やめたい"
```

補足:
- `請求` 単独は採用しない。「資料請求したいです」「資料を請求したい」「カタログを請求できますか」（公開情報・資料の入手）に部分一致して第1層取次になるため。請求の個別案件は `請求額` `請求書` `請求され` `請求が` `請求金額` `請求内容` `二重請求` `月の請求` `月分の請求` `請求を止` `請求が来` の語形で拾う。
- `料金プラン` 単独は採用しない。「料金プランを教えてください」「どんな料金プランがありますか」（プラン一覧の照会）が第1層取次になるため。自分のプランの見直し・変更は `プランを見直` `プランの見直` `プランを変え` `プランの変更`（および main 由来の `プラン変更` `プランを変更`）で拾う。
- `引き落と` は「引き落とし」「引き落とされ」の両方を1語で拾うための意図的な語幹形。`二重に引` は「二重に引き落とされた」「二重に引かれ」を拾う。
- `値上` は「値上がり」「値上げ」を拾うための語幹形（main 時点の `値上が` から置換）。
- `支払` は「支払い」「支払日」「支払方法」「支払う」を1語で拾うための語幹形（main 時点の `支払い` から置換）。「月額の支払日を変更したい」は main では「月額」で `contract_billing_question` が立っていたが、「支払日」は「支払い」に部分一致しないため置換が必要になった。`決済日` は同じ理由で「料金の決済日を変更したい」を拾うために加算した。
- `返してほしい` は採用しない。語形が広く、返金・返却に限らない発話に部分一致するため、返金の依頼は `料金を返` `代金を返` の語形に限定して拾う。
- この加算は**意図した取次範囲の拡大**である。実測（2026-10-03、`LexiconNormalizer` に 83 発話を通し main の lexicon と比較）で、main では `contract_billing_question` が立たず、現行 lexicon で新たに第1層 `contract-billing` へ取次になる個別案件: 「請求額が違います」「二重に請求されている」「今月の請求はいくらですか」「今月の請求が高い」「請求内容を確認したい」「請求書を再発行して」「二重に引き落とされた」「引き落とし日を教えてください」「返金してほしい」「カード情報を更新したい」「プランを変更したいのですが」「値上げされたのですか」。いずれも個別案件であり、取次が正しい挙動として扱う。
- `やめたい` を加算した。「料金が高いのでやめたいです」「サービスをやめたいのですが月額はどうなりますか」は変更前（`料金`/`月額` が `contract_billing_question` にあった頃）は取次になっていたため、これを落とすのは回帰である。理由: 解約検討中の顧客に価格表を返して取りこぼす害の方が、機能停止の質問が不要に取次になる害より大きい。エスカレーションは過剰側に倒す。
- 受容した誤発火（`やめたい` が機能停止の質問にも部分一致し、いずれも第1層 `contract-billing` へ落ちる。reviewer 実測）: 「常時録画をやめたいです」「通知をやめたいのですが」「メール通知をやめたい」「自動更新をやめたい」「SDカードへの録画をやめたい」「夜間だけ録画をやめたいです」「アプリの利用をやめたい」「二段階認証をやめたいのですが」「カメラの首振りをやめたい」「共有をやめたいです」。さらに、第1層 advisory でマニュアル材料が十分（score ≥ 閾値）なときは `missing` が空になり `clarification_allowed = false`（`server/src/harness/mod.rs` の `clarification_allowed`）となるため、**これらは回答も聞き返しもされず即取次になる**。
- 受容した誤発火（個別案件語の語形が、個別案件でない発話にも部分一致する。上記と同じ実測による。いずれも main では `contract_billing_question` が立たず、現行 lexicon で立って第1層 `contract-billing` へ落ちる）:
  - `クレジットカード` `口座` `請求書`: 「クレジットカードは使えますか」「クレジットカードで払えますか」「口座振替はできますか」「請求書払いはできますか」（支払い方法の一般的な質問）
  - `内訳`: 「見積もりの内訳を教えてください」「セット内容の内訳を教えて」「同梱物の内訳」
  - `カード情報`: 「SDカード情報が表示されない」「ICカード情報を登録したい」
  - `明細`: 「録画の明細を見たい」「ログイン履歴の明細」
  - `値上`: 「値上げ予定はありますか」
  - `支払`: 「支払方法は何がありますか」「支払期限はいつですか」「電子マネーで支払えますか」「支払う方法を教えて」（支払い方法・期限の一般的な質問。実測 2026-10-03。「支払い方法は何がありますか」は `支払い` の頃から立っていた）
  - これらはエスカレーションを過剰側に倒す方針（`やめたい` と同じ）に従い、取次を受容する。`SDカードの情報が消えた` のように語形が一致しないものは立たない。
- 既知の限界（main では `contract_billing_question` が立って取次だったが、現行 lexicon では立たなくなる個別案件的な発話。実測）: 「月額を安くできませんか」「月額プランを安いものに変えたい」「料金が高すぎる」は価格語（`price_question`）のみが立ち、第1層に落ちない。個別案件語（`解約` `契約` 等）を含まない価格交渉・不満は、第3層のマニュアルスコア判定に委ねられる。
- 既知の限界（lexicon の網羅範囲）: lexicon が拾えるのは列挙した個別案件語を含む発話だけである。価格語と未列挙の個別案件語だけから成る発話は `price_question` のみが立ち、第1層を通過する。本番では LLM 分類との和集合（`server/src/harness/extraction.rs`）が補うが、LLM 分類が失敗したときは補われない。
- main でも立たず現行でも立たない個別案件的な発話（実測、現状維持）: 「退会したい」「サービスを止めたい」「もう使わないので止めたい」「領収書が欲しい」「先月分が払えていない」「カードの有効期限が切れた」「名義変更したい」。
- 価格・費用の質問と個別案件語が同居する発話は、従来どおり `contract_billing_question` が立って取次になる（実測: 「契約前に料金を知りたいです」「解約金はいくらですか」「支払い方法は何がありますか」「料金の内訳を教えてください」）。

### `いくら` の語形限定（reviewer 差し戻し Critical 2、2026-10-03）

`price_question` の surface_forms のうち `いくら` 単独は、価格以外の疑問文にも部分一致していた（実測: 「電池はいくらもちますか」「カメラの画角はいくらですか」「いくらでも相談に乗ってください」「保証期間はいくらですか」）。余計な signal 1 つが立つと次の副作用がある。

1. `match_known_resolution`（`server/src/harness/rules.rs`）は leftover が空のときしか `KnownResolution` を適用しない。余計な signal 1 つで `KrMatch::BlockedByAddedSignal` → 第3層 `UnknownAddedSignal` エスカレーションになりうる。
2. `build_known_facts`（`server/src/api.rs`）が accumulated signal の `customer_label` を聞き返しプロンプトへ「把握済みの条件語」として載せる。価格と無関係な質問に「料金・価格に関するご質問」が混入する。
3. signal は会話単位で累積するため、1ターンの誤発火がケース全体に残る。

このため `いくら` 単独を廃し、価格照会の形に限定した:

```
"いくらですか", "いくらでしょう", "いくらかかり", "いくらになり", "おいくら"
```

この変更により「電池はいくらもちますか」「いくらでも相談に乗ってください」は `price_question` を立てなくなった。本件の対象（「月額いくらですか」「初期費用はいくらですか」「何台だといくらになりますか」「法人だといくらになりますか」）は維持される。

残存する既知の限界: 「カメラの画角はいくらですか」「保証期間はいくらですか」は語形が「いくらですか」に一致するため引き続き `price_question` を立てる。前者は他に一致する signal が無く第3層のマニュアルスコアに委ねられ、後者は `warranty_hardware_failure`（「保証」）と同時に立つため第1層 `warranty-failure` が先に確定し `price_question` の混入は実害が無い。いずれも受容する。

`price_question` は `server/data/urtect/rules.json` の `escalation_rules` にはいずれの condition にも含めていない。第1層のどのルールにもマッチしないため、価格語のみの発話は第1層を通過し、マニュアル検索の材料（`search_manual` / `evaluate_answerability` の manual score）が十分な場合に回答される（材料が不足する場合は第3層で取次になりうる）。`prohibited_domains`（第2層）には影響しない。第3層 `KnownResolution` への影響は次節のとおり。

### `price_question` が `KnownResolution` 適用に与える影響

`価格` `値段` `おいくら` `いくらですか` 系の語は main では signal を立てなかった。現行 lexicon では `price_question` が立つ（実測: 「初期費用はいくらですか」（`いくらですか` による。`初期費用` 自体は Issue #76 で `initial_cost_question` へ移設済み。次節参照）「価格を知りたい」「おいくらですか」「値段はいくら」「法人だといくらになりますか」）。signal は会話単位で累積するため、これらの発話が会話に含まれると、後続ターンで `match_known_resolution`（`server/src/harness/rules.rs`）が `KrMatch::BlockedByAddedSignal` を返しうる（累積 signal に `price_question` が余り、KnownResolution の条件に含まれないため）。

`BlockedByAddedSignal` は `clarification_allowed` では `InsufficientDirectness` と同扱いのため、聞き返しの挙動は変わらない。失われるのは当該ターンでの `KnownResolution` の適用のみである。

### lexicon 変更後の再 ingest

`server/data/urtect/signal-lexicon.json` を変更すると、次回の `ingest_urtect` 実行で一部の section が再 upsert される。`ingest_urtect`（`server/src/bin/ingest_urtect.rs`）は MENTIONS_SIGNAL のもとになるマッチ済み signal を section の composite hash に含める（`signals_joined`）ため、lexicon の変更で signal 集合が変わった section は、本文が同一でもハッシュが変わり再 upsert される。signal 集合が変わらない section は skip される。

`ingest_alarmcom` も同じ lexicon で MENTIONS_SIGNAL を作るが、差分判定のハッシュは英語原文（`content_hash(&body_en)`）のみで決まる。lexicon を変更しても未変更の記事は再処理されず、MENTIONS_SIGNAL は記事の本文が変わったときにだけ新しい lexicon で再生成される。alarm.com の既存 section の MENTIONS_SIGNAL を新 lexicon に揃える手段は、この CLI には無い。

`description` フィールド（LLM 分類プロンプトに埋め込まれる。`server/src/harness/signal.rs` の `LexiconNormalizer::vocabulary_for_prompt`）には語義のみを記述し、ルーティングの可否・方針は書かない（分類器が語義ではなく方針で判断する方向へ引っ張られるため）。`price_question` の `description` は「公開されている価格表・費用の照会（特定の契約や請求の個別案件ではなく、一般的な価格の問い合わせ）」とし、「第1層のエスカレーション対象外」のような運用方針の記述はしない。`customer_label`（顧客向け表示専用、「料金・価格に関するご質問」）は語義の言い換えであり変更していない。

## `initial_cost_question`（初期費用・設置工事費の問い合わせを第1層で即時取次、Issue #76）

初期費用は設置環境（配線の有無・設置場所の構造・既存設備の状況等）によって決まり、マニュアル上の一般論では答えられない。聞き返しても公開情報で埋まる見込みが無いため、第1層 advisory（情報不足なら聞き返しに開放する）ではなく mandatory（問答無用で即時取次）で扱う。設計判断の詳細は `docs/superpowers/specs/2026-10-05-initial-cost-handoff-design.md` §1 を参照。

`server/data/urtect/signal-lexicon.json` に `initial_cost_question`（`class: context`）を新設し、surface_forms は次の11語とした: `初期費用` `初期コスト` `設置費` `工事費` `導入費` `取り付け費` `取付費` `設置料金` `工事料金` `設置代金` `工事代金`。`price_question` の surface_forms からは `初期費用` を削除し（前節の修正）、こちらへ移した。

`設置代` `工事代` は採用しない。「設置代行」（カメラの設置作業そのものの代行サービス、費用の話ではない）に部分一致するため（`設置代行はありますか` が誤って取次になる）。

`server/data/urtect/rules.json` の `escalation_rules` に第1層ルール `initial-cost-quote`（`condition: ["initial_cost_question"]`, `owner: "contract"`, `route: "support_desk"`, `binding: "mandatory"`）を追加した。「初期費用はいくらですか」は `いくらですか` により引き続き `price_question` も立つが（前節）、第1層は `match_layer1` の mandatory 優先（`server/src/harness/rules.rs`）により `initial-cost-quote` で確定する（`price_question` は第1層のどの `condition` にも含まれないため、単独では取次を発生させない）。

このルールは顧客向けの受け止め文（`customer_ack` 属性）を宣言している: 「初期費用はお客様の状況によって異なりますので、担当者におつなぎします。」累積 signal 集合（会話単位）にマッチする第 1 層ルールがちょうど 1 件のときだけ、`/{project_id}/api/reply` は LLM で受け止め文を生成せず、この宣言文をそのまま使う（NG 表現ゲート・取扱製品ゲートは通す）。2 件以上マッチしたときは宣言文を使わず LLM が作る。属性の型・検証・伝搬経路は design doc §3 が正本。

### 受容した誤発火・既知の限界（実測、2026-10-05、`LexiconNormalizer` に候補発話を通した結果）

実測は変更後の `server/data/urtect/signal-lexicon.json` を `LexiconNormalizer::from_path` で読み、候補発話を `normalize()` に通して確認した（一時ファイルは実測後に削除済み。本番コードには含まれない）。

- 意図した該当発話はすべて `initial_cost_question` を立てた: 「初期費用はいくらですか」「初期コストはどれくらいですか」「設置費はいくらかかりますか」「工事費はいくらですか」「導入費はどれくらいですか」「取り付け費を教えてください」「取付費はいくらですか」「設置料金を教えてください」「工事料金はいくらですか」「設置代金はどれくらいですか」「工事代金の見積もりをお願いします」。
- 既存の回帰対象（「月額いくらですか」「設置方法を教えてください」「設置代行はありますか」）はいずれも `initial_cost_question` を立てなかった（`設置代行はありますか` は `camera_installation` のみ。`設置代` を採用しなかった判断どおり）。
- **受容した誤発火**: 請求・支払いの個別案件（契約・請求の相談であって初期費用の見積もり相談ではない）の発話に `initial_cost_question` の語（`初期費用` `工事費` `設置費` `導入費` `取り付け費` `取付費` `工事代金` `設置代金`）が含まれると、`contract_billing_question` と同時に立つ。実測（`customer_ack` の適用を第 1 層のマッチ件数 1 件に限る実装で `decide` まで通した結果）: 「工事費の請求書を再発行してください」「初期費用を返金してほしいです」「設置費が二重に請求されています」「導入費の支払いが完了していません」「取り付け費の請求額が間違っています」「工事代金の請求書を再発行してください」「設置代金の支払いが完了していません」「初期費用の請求書の宛名を変更したい」はいずれも両 signal を立て（`設置費` `取り付け費` `設置代金` の発話は `camera_installation` も立つ）、第 1 層で `contract-billing`（advisory）と `initial-cost-quote`（mandatory）の 2 件にマッチする。`match_layer1`（`server/src/harness/rules.rs`）は mandatory を advisory より優先するため mandatory の `initial-cost-quote` で取次が確定し、`contract-billing` の聞き返しには回らない。マッチが 2 件のため `customer_ack` は `None` になり、宣言文は使われず LLM が相談内容に沿った受け止め文を作る。両ルールの `owner`（`contract`）と `route`（`support_desk`）は同一なので取次先は変わらない。過剰エスカレーション側に倒す既存方針（`contract_billing_question` 節の「やめたい」と同種の受容）に従い、取次が成立することは受容する。なお「取付費の領収書を送ってください」は `contract_billing_question` が立たず `initial_cost_question` のみで、第 1 層のマッチは `initial-cost-quote` の 1 件になり、宣言文が使われる。
- **既知の限界**: 11語のいずれも連続する部分文字列として含まない言い回しは lexicon で拾えない。実測: 「初期設定費用はかかりますか」（「初期」と「費用」の間に「設定」が入るため `初期費用` に部分一致しない）、「導入にかかる費用を教えてください」（`導入費` に部分一致しない）、「設置にいくらかかりますか」（design doc §9 に記載の既知の限界どおり、`price_question` のみが立ち `initial_cost_question` は立たない）はいずれも signal が立たない、または `initial_cost_question` を欠く。本番は LLM 分類との和集合（`server/src/harness/extraction.rs`）が補うが、LLM 分類が失敗したときは補われない。
