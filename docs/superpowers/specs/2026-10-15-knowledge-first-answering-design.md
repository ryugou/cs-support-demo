# 承認済みノウハウを今のターンに優先適用する

| 項目    | 内容 |
| ----- | ---- |
| 目的    | 担当者が承認した KnownResolution（KR）を、会話の累積や取次状態に関係なく今のターンの質問へ適用し、その回答本文を一次材料として顧客への返信に反映する（Issue #103） |
| 読者    | 実装者、レビュアー |
| 正本の範囲 | 3 層判定の層順、KR の照合規則、KR の回答本文を一次材料として下書きを生成する規則と後段のゲート、取次を伴うターンでの合成、lexicon への signal 追加、本番でのノウハウ登録手順 |
| 関連文書  | [`2026-10-07-partial-answer-with-handoff-design.md`](2026-10-07-partial-answer-with-handoff-design.md)（部分回答の合成。本書はその材料に KR を加える）、[`2026-10-14-answer-after-escalation-design.md`](2026-10-14-answer-after-escalation-design.md)（取次済み case の判定入力と合成。本書はそのまま使う）、`specs/production-cs-mcp.md`（3 層判定の正本。層順と第 3 層 (a) の照合規則を本書で改める）、`specs/signal-vocabulary.md`（lexicon の語彙）、[`2026-08-12-conversation-flow-v11-design.md`](2026-08-12-conversation-flow-v11-design.md) と [`2026-09-21-jev-shadow-design.md`](2026-09-21-jev-shadow-design.md)（`clarification_allowed` と Jev を呼ばない経路の列挙。本書の規則に合わせて書き換える） |

## 1. 原則と採用する設計

本番 LINE の実問い合わせ（Issue #103）で、担当者が登録したノウハウがあるのに応答に使われない。原因は 3 つ。(a) KR の照合が「余り signal なし」を要求するため、会話の累積で 1 つでも別の signal が立つと外れる。(b) KR は第 1 層 advisory ルールより後に照合されるため、契約・保証のルールが当たるターンでは評価されない。(c) ノウハウを表す signal が lexicon に無く登録できない。

原則:

- **ノウハウは今のターンの質問に対して適用する。** 会話の累積と取次状態は KR の適用可否に影響しない（累積に hazard 級 signal がある会話を除く。§3.2）。
- **KR の回答本文を一次材料にする。** LLM は KR の回答本文を唯一の材料として、顧客の質問文への返信を生成する。手順の番号・順序・数値・注意書きは KR のまま保ち、材料に無い手順・数値を補わない。KR の回答本文を定型文として返すのではなく、質問に答える文として組み立てる。
- **安全側の順序は変えない。** 第 2 層（禁止領域）と第 1 層 mandatory は KR より先に確定し、KR はそれを越えない。第 1 層 advisory（契約・保証）と同時に当たるターンは「KR の回答 + 取次の一文」にし、取次事象は従来どおり作る。hazard 級の signal がある会話では KR を使わない。

KR を第 1 層 mandatory と第 2 層の下に置く理由: これらの保証はデータに依存してはならない。現状、Google アカウントで認証した誰でも `add_known_resolution` を実行できる（`specs/production-cs-mcp.md` の AuthN 現状）ため、KR が禁止線を越えられる設計にすると、登録 1 件で禁止領域の固定応答が書き換わる。

## 2. 用語

- **今のターンの signal**: `Harness::evaluate` が今回の質問文から抽出した `SignalSet`（`EvaluationOutcome.signals`）。
- **累積 signal**: case の `signals` 属性に蓄積した集合（`EvaluationOutcome.accumulated_signals`）。
- **判定用 signal**: `decide_signals_for_policy` が返す集合。未取次の case と MCP では累積 signal、取次済み case では今のターンの signal（`2026-10-14-answer-after-escalation-design.md` §2）。第 1・2 層の照合、取り次ぐ項目の抽出、`customer_ack` の件数判定に使う。
- **KR 同乗の取次**: 判定が `Escalate`（第 1 層 advisory）で `known_resolution_id` を持つ状態。下書きは KR の回答本文から作り、取次の一文を添える。

## 3. 判定（`decision::decide`）

### 3.1 入力

`DecisionInput` に `turn_signals: &SignalSet`（今のターンの signal）を加える。`question_signals`（判定用 signal）はそのまま第 1・2 層に使う。`turn_signals` は KR の照合にだけ使う。呼び出し側（`Harness::evaluate`）は `ReplyDraftPolicy` に関係なく `signals`（今のターンの抽出結果）を渡す。

### 3.2 層順

```text
stakes / threshold を算出（hazard_signal_count は累積 signal）
第 1 層 mandatory（question_signals）→ 当たれば Escalate layer 1
第 2 層（question_signals, question_raw）→ 当たれば Escalate layer 2
KR 照合（turn_signals。§3.3）。stakes_input.hazard_signal_count > 0 のときは照合しない
  当たった、かつ第 1 層 advisory も当たる → Escalate layer 1 + known_resolution_id（KR 同乗の取次）
  当たった、かつ第 1 層 advisory が当たらない → Allowed { source: KnownResolution }
第 1 層 advisory（question_signals）→ Escalate layer 1（missing / hearing / customer_ack の算出規則は変えない）
マニュアル直接性（evidence_sufficient）→ Allowed { source: Manual } または Escalate layer 3 InsufficientDirectness
```

本書は層順を **第 1 層 mandatory → 第 2 層 → KR → 第 1 層 advisory → マニュアル** と定める。`decide` の現行実装は第 1 層（mandatory → advisory）→ 第 2 層の順で、第 1 層 advisory と第 2 層が同時に当たる入力は layer 1（advisory。`missing` と `hearing` を持ちうる）になる。本書の層順ではこの入力は **layer 2（禁止領域の受け皿。`missing` 空・`hearing: None`・聞き返し不可）** になる。採る理由: 禁止領域に触れた質問に advisory の聞き返しを返すこと自体が「答えられる可能性」を示唆する漏洩経路であり（`2026-08-12-conversation-flow-v11-design.md` の第 2 層の規則と同じ理由）、禁止領域は第 1 層 advisory より先に確定すべきである。mandatory の取次ルールは従来どおり第 2 層より先に確定する。この入力の `handoff_items` は、第 2 層の `Escalate` と同じく `handoff_items_for_decision` が第 2 層の `domain_signals` から導く項目だけで、advisory ルールの項目は含めない（禁止領域の取次に advisory の項目を並べると、advisory の件は答えられる可能性があると示唆するため）。

`match_layer1` は mandatory を優先して 1 件返す。本書の層順では、第 1 層の照合を「mandatory の照合」と「advisory の照合」に分けて呼ぶ。`rules::match_layer1_mandatory` / `rules::match_layer1_advisory`（いずれも `rule_matches` を使い、同一 binding 内は配列順）を新設し、`match_layer1` はこの 2 つの合成として残す。`customer_ack` の件数判定（`count_layer1_matches`）は mandatory と advisory を合わせて数える（変えない）。

KR 同乗の取次の `Escalate` は次の値を持つ。

| フィールド | 値 |
| --- | --- |
| `reason` | `RegulatedOrSafety` |
| `layer` | `1` |
| `route_to` | 当たった advisory ルールの `route` |
| `disclosure_scope` | `ConfirmingWithTeam` |
| `audit_required` | `true` |
| `missing` | 空（KR で答えるので聞き返さない。`clarification_allowed` は `false`） |
| `hearing` | `None`（Jev を呼ばない） |
| `customer_ack` | 現行の規則（第 1 層で当たるルールがちょうど 1 件ならそのルールの宣言） |
| `known_resolution_id` | `Some(kr.id)` |

hazard で照合しない理由: hazard 級 signal（`physical_construction_risk`、`security_incident`、`security_guarantee`、`legal_privacy_question`、`physical_damage_smell_heat`、`unclassified_risk`）のうち `unclassified_risk` はどのルール・領域の条件にも無く、第 1・2 層を通過する。こうした会話では承認済みの手順（KR）を自動適用しない。マニュアル直接性の判定は現行どおり stakes の閾値（hazard で Mid に上がる）で行い、`Allowed { source: Manual }` になりうる。累積で見るのは `2026-10-14-answer-after-escalation-design.md` §2 の stakes の扱いと同じ（過去ターンの hazard は安全側に効かせる）。取次済み case で前ターンにだけ hazard がある場合も、`hazard_signal_count` は累積から数えるので KR は適用されない。

### 3.3 KR の照合規則（`rules::match_known_resolution`）

- 適用条件: `kr.signal_set` が空でなく、`kr.signal_set ⊆ turn_signals`。余り（`turn_signals − kr.signal_set`）があっても適用する。
- 複数当たるときは `signal_specificity()`（`signal_set.len()`）が最大の KR を採る。同値は `verified_at` が新しい順、それも同値なら `id` の昇順で決める（vegapunk からの取得順に依存させない）。`KnownResolution` に `verified_at: Option<chrono::DateTime<chrono::Utc>>` を加え、`knowledge.rs::known_resolutions_from_nodes` が KR ノードの `verified_at` 属性（`add_known_resolution` が登録時に RFC 3339 で書く。`update_known_resolution_grade` は変更しない）を解析して入れる。属性が無い KR は `None` とし、最も古いものとして扱う（`add_known_resolution` を通らずに投入されたレコードを読めなくするより、最古として照合に残す方が「新しい登録が勝つ」規則と整合する）。属性が有って RFC 3339 として解析できない KR は、`invalid verified_at: <解析エラー> (kr_id=<id>)` のエラーで `known_resolutions_from_nodes` 全体を `Err` にする（`kr_id` 欠落と同じ経路。`Harness::evaluate` と MCP `search_known_resolutions` はこのエラーをそのまま返す）。格納値の破損で選ばれる KR が黙って変わるのを防ぐためで、`verified_at` は誤登録の上書き（§11）を決める値である。新しい順にする理由: 同じ `signals` で正しいレコードを登録し直せば、誤登録したレコードより新しいものが選ばれ、削除手段を持たない backend でも誤登録を上書きできる（§11）。
- 除外 id リスト: `[harness] excluded_known_resolution_ids = []`（`HarnessConfig`、`Vec<String>`、既定は空。`config.cloudrun.toml` にも空のリストと用途のコメントを置く）に載った id の KR は、照合の対象にしない。CS の `Harness::evaluate`（`load_known_resolutions_with` の結果）と MCP `search_known_resolutions`（`load_known_resolutions` の結果）は、読み込んだ `Vec<KnownResolution>` を `match_known_resolution` に渡す前に純関数 `rules::exclude_known_resolutions(resolutions, &excluded_ids)` で絞る。`recompute_grade` と homesec の `advisor/api.rs` は対象外（前者は id 指定の読み出し、後者は別 config で既定の空のまま）。用途は `signals` を誤って登録した KR を止めること（§11）で、config は image に含まれるため変更は PR → CI デプロイで反映する（`deploy-only-via-ci`）。
- 機種固有の KR（特定の型番でしか成立しない手順）は、`signals` に lexicon の型番 signal（`model_adc_v724x` 等）を含める。`applicability` は人間が読む前提の記述で、照合には使わない。型番 signal を含む KR は、その型番 signal が今のターンに無い質問（型番の無い質問。取次済み case で「赤く点滅しています」とだけ書かれたターンや、別の型番だけを書いた「ADC-V523 が赤く点滅しています」）に当たらない。lexicon の語形は部分一致なので「ADC-V724X」からは `model_adc_v724` も同時に立つが、余りとして許す（他の型番 signal があることを理由に除外しない）。
- 戻り値は `Option<&KnownResolution>`。`KrMatch` 列挙型と `KrMatch::BlockedByAddedSignal`、`EscalateReason::UnknownAddedSignal` は削除する（余りを許す規則のもとでは到達しない）。第 3 層の `Escalate` の `reason` は `InsufficientDirectness` だけになる。
- `search_known_resolutions`（MCP）は、入力 `question` から抽出した signal（応答の `question_signals` フィールドに入る集合）で照合する。`match_kind` は `applicable` / `none` の 2 値。`leftover_signals` は `applicable` のときの余り（抽出した signal − `kr.signal_set`）を返し、`none` のときは空。
- homesec の `advisor/materials.rs::match_advisor_known_resolution` は `Option` をそのまま返す（挙動は変わらない）。

KR の signal が複数ターンに分かれた会話には当たらない。1 ターン目「赤く点滅しています」の今のターンの signal は `led_red_blinking` だけなので、`led_red_blinking` + `model_adc_v724x` の KR は不適用で、現行どおりマニュアル直接性の判定（回答、または第 3 層の聞き返し）になる。2 ターン目「ADC-V724X です」の今のターンの signal は型番 signal だけなので、この KR はここでも不適用で、再びマニュアル直接性の判定になる（累積 signal は第 1・2 層と manual 検索には使うが、KR には使わない）。今のターンだけで照合する理由: 累積で照合すると、未取次の case で前のターンに答えた KR（赤い点滅、specificity 2）が次の「クラウドの保存期間はどのくらいですか」（`cloud_storage_retention`、specificity 1）にも当たり、specificity 最大の規則で前の KR が選ばれて別の質問に前の回答を返す。また前のターンの型番 signal が、後のターンの別の機種の質問に機種固有の KR を当てる。Issue #103 の 3 件はいずれも 1 通に KR の signal が揃っており、今のターンだけの照合で解決する。手順が機種に依存しない KR は型番 signal を含めずに登録すれば、症状だけのターンで当たる。

余りを許して安全か: 第 1 層 mandatory と第 2 層は判定用 signal（未取次と MCP では累積、取次済み case では今のターン。§2）で KR より先に照合しており、判定用 signal の中で取次・禁止の条件に触れる signal があればそこで確定している。取次済み case では判定用 signal が今のターンだけになるが、過去ターンの mandatory・禁止領域の signal で取次済みになった case は既に受付番号を持ち、今のターンの KR 回答に受付番号の確認中の一文が付く（`2026-10-14-answer-after-escalation-design.md` §4。取次事象は再発行しない）。過去ターンの hazard 級 signal は `hazard_signal_count` が累積から数えるので §3.2 のとおり KR を止める。KR まで来た今のターンの余り signal は、第 1 層 advisory の条件か、どのルールにも無い文脈語である。前者は KR 同乗の取次（取次の一文を添える）で扱い、後者は回答を妨げない。

### 3.4 `AnswerDecision` の形

`AnswerDecision::Escalate` に `known_resolution_id: Option<String>` を加える（serde で出力する。`None` のときはフィールドを出さない）。`Allowed` の `known_resolution_id` は現行どおり。

## 4. 下書き（KR を一次材料にする）

### 4.1 部分回答の可否（`handoff_items::can_draft_partial_answer`）

`2026-10-07-partial-answer-with-handoff-design.md` §3.1 の「`best_manual_score` が関連十分である」を、「判定が `known_resolution_id` を持つ、または `best_manual_score >= thresholds.low`」に改める。構造条件（第 1・2 層の `Escalate`、`hearing` なし、取り次ぐ項目 1 つ以上）と有効化条件（`partial_answer_enabled`、下書き生成有効）は変えない。`PartialAnswerSkipReason::InsufficientRelevance` は KR が無く関連も不十分なときだけ。

### 4.2 下書きの出所（`Harness::draft_customer_reply`）

| 判定 | 下書き |
| --- | --- |
| `Allowed { source: KnownResolution }` | KR の回答本文を唯一の材料（出典「承認済みの回答（known_resolution）」）として LLM が生成する。`build_reply_brief_with_resolution` の `Allowed { KnownResolution }` 分岐が `ReplyBrief { kind: Answer, disclosure: None, handoff_ack: None, excerpts: [KR 本文 1 件] }` を組む（KR 本文は切り詰めない。下記） |
| `Escalate` で `known_resolution_id` あり、`partial_answer_ok` | 材料は上と同じ KR 本文 1 件（切り詰めない。マニュアル抜粋は使わない）。`build_reply_brief_with_resolution` の `Escalate` 分岐が、`known_resolution_answer` が `Some` のときに `excerpts_from_hits` の代わりにそれを材料にして `ReplyBrief { kind: Escalation, disclosure: Some(disclosure_scope), handoff_ack: customer_ack, partial_answer_ok: true, excerpts: [KR 本文 1 件] }` を組む。`kind: Escalation` と `partial_answer_ok` により、system prompt には現行の部分回答の規則（`handoff_items` の金額・可否・条件を書かない §3.5 の歯止め、取次表現・受付番号の禁止、挨拶・結びなし）が入り、`ReplyKind::Answer` 専用の結びの規則（「解決したかを確認し会話の継続を促す一文で終える」）は入らない |
| `Escalate` で `known_resolution_id` なし、`partial_answer_ok` | マニュアル抜粋を材料に LLM が生成する（現行どおり） |
| `Allowed { source: Manual }` | マニュアル抜粋を材料に LLM が生成する（現行どおり） |
| 上記以外の `Escalate` | 下書きなし（現行どおり） |

- KR の回答本文は `truncate_material` の 2,500 字の切り詰めを適用せず全文を渡す。KR は担当者が承認した完結した回答で、切り詰めると手順が欠ける。
- `Harness::draft_customer_reply` は、`Allowed` に加えて `Escalate` の `known_resolution_id` からも KR の回答本文を引き、`build_reply_brief_with_resolution` に渡す。引く先は `decide` に渡したのと同じ `resolutions` スライスなので、id が見つからない状態には到達しない。到達した場合は下書きを生成しない（`None`）。
- system prompt（`reply.rs` の回答規則）に、材料が KR のときの規則を加える: 「承認済みの回答の手順は、番号・順序・数値・注意書き（※ や ⚠ で始まる行を含む）を変えずに書く。省略・並べ替え・言い換えで手順の意味を変えない」。現行の「資料に無い事実・手順・数値を補わない」はそのまま。
- 下書き生成の有効化条件（`customer_reply_draft_enabled`、LLM 有効）は現行のまま適用する。
- 取次ターンで `is_partial_answer_candidate` が真のときに走る signal 絞り込みの補助検索（`Harness::evaluate`）は、`known_resolution_id` の有無で変えない。補助検索の結果は返す manual hit と `best_manual_score` にだけ入り、KR 同乗の取次の下書き材料には使わない（上表）。

KR の回答本文を定型文として返さず LLM の材料にする理由: 顧客の質問文は KR の想定と語り口・範囲が異なる（型番の有無、症状の書き方、取次の一文との接続）ため、返信は質問に答える文として組み立てる。手順・数値・注意書きが保たれているかは、system prompt の規則と、実 LLM smoke の出力をユーザーが確認すること（§10・§11）で担保する。保持を決定論で検査する本番のゲートは作らない（§13）。

KR 同乗の取次でマニュアル抜粋を使わない理由: KR は担当者が承認した回答で、マニュアルの一般記述と手順が食い違うことがある（赤い点滅に初期化を案内する記事と、初期化を禁じる KR）。両方を顧客に見せると、KR の注意書きが打ち消される。

### 4.3 後段のゲート

KR を材料にした下書きも、マニュアル抜粋を材料にした下書きと同じ順で後段を通す: egress gate（`draft_customer_reply` 内）→ `accept_draft`（`Harness::evaluate`。NO_ANSWER、部分回答の歯止め）→ 取扱製品ゲート（呼び出し側の `gate_customer_reply_draft`）。いずれかで不採用になれば `customer_reply_draft` は `null`（KR 由来であっても例外にしない）。各段の warn は現行のまま出る。

### 4.4 記録

- WORM 監査（`Harness::evaluate` の `audit_with_nodes`）: `Escalate { known_resolution_id: Some }` でも、`Allowed { known_resolution_id: Some }` と同じく `retrieved_node_ids` に KR ノード id（`knowledge::harness_node_id(schema, "KnownResolution", kr_id)`）を、`governing_norm_ids` に KR id を記録する。KR が材料になった判定は全て provenance から辿れる（`specs/production-cs-mcp.md` の条件 8）。`decision` 文字列の形（`escalate:layer{layer}`）は変えない。
- case 属性 `last_kr_id` / `last_decision` の書き込み規則は現行どおり（`Allowed` のときだけ KR id を記録する）。

## 5. 応答の合成（`/api/reply`）

合成規則は変えない。KR 同乗の取次は、部分回答の下書きがあるターンとして既存の分岐に乗る。

| 状態 | 今のターン | 応答 |
| --- | --- | --- |
| 未取次 | `Allowed { KnownResolution }` | 下書き（KR を材料にした返信） |
| 未取次 | KR 同乗の取次 | 下書き / 空行 / 取次の一文（`build_handoff_sentence`）/ 空行 / 受付番号ブロック。取次事象を作る |
| 取次済み | `Allowed { KnownResolution }` | `build_answer_in_escalated_case`（下書き + 受付番号の確認中の一文。営業時間外はその後に営業時間の案内行 `out_of_hours_line`。`2026-10-14-answer-after-escalation-design.md` §4） |
| 取次済み | KR 同乗の取次 | `build_partial_answer_in_escalated_case`（下書き + 取次の一文 + 受付番号の確認中の一文。営業時間外は同じく案内行が続く）。取次事象・受付番号を再発行しない |

## 6. MCP `evaluate_answerability` / `search_known_resolutions`

- `decision` が `escalate` のとき `known_resolution_id` を出力しうる（§3.4）。`customer_reply_draft` と `partial_answer: true`、`handoff_items` の意味は `2026-10-07-partial-answer-with-handoff-design.md` §3.4 のまま。
- `reason` の値から `unknown_added_signal` が無くなる。`clarification_allowed`（`harness/mod.rs::clarification_allowed`）の第 3 層の分岐は `InsufficientDirectness` だけを見る（`UnknownAddedSignal` の腕を削除する。第 3 層グレーで `true`、第 2 層で `false`、第 1 層は binding と `missing` で決める規則は変えない）。
- `search_known_resolutions` の `match_kind` から `blocked_by_added_signal` が無くなる（§3.3）。
- 第 1 層 advisory と第 2 層が同時に当たる質問の `decision` は `escalate` の `layer: 2` になる（§3.2）。

## 7. lexicon への signal 追加（`server/data/urtect/signal-lexicon.json`）

| `signal` | `class` | `surface_forms` | `description` | `customer_label` |
| --- | --- | --- | --- | --- |
| `cloud_storage_retention` | `context` | 録画の保存期間 / クラウドの保存期間 / 映像の保存期間 / 動画の保存期間 / 録画の保存日数 / クラウドの保存日数 / 録画は何日 / 録画が何日 / 映像は何日 / 録画を何日間保存 / 映像を何日間保存 / 録画をさかのぼ / 映像をさかのぼ / 何日前の録画 | クラウド録画の保存期間・さかのぼれる日数の相談 | クラウド録画の保存期間に関するご相談 |
| `led_red_blinking` | `context` | 赤く点滅 / 赤い点滅 / 赤点滅 / 赤で点滅 / 赤が点滅 / 赤色に点滅 / 赤ランプが点滅 / 赤いランプが点滅 | カメラの LED が赤く点滅している状態の相談 | 赤いランプの点滅に関するご相談 |

- 語形は、録画・保存・LED の文脈でしか現れない形に限る。余りを許す照合（§3.3）では、無関係な質問で signal が立つとその KR が適用されるため、文脈を選ばない語形は入れない。「保存期間」「保存日数」は録画・クラウド・映像・動画を前置した形だけにする（保証期間や個人情報の保存期間の質問で立てない）。「何日分」は日割り請求の質問で現れるため入れない。「何日間保存」「さかのぼれ」も録画・映像を前置した形だけにする（個人情報の保存や請求の遡及の質問で立てない）。
- どちらも `escalation_rules` / `prohibited_domains` の条件には加えない（第 1・2 層の挙動を変えない）。
- lexicon はサービス起動時にイメージ内ファイルから読む。反映はデプロイ（main マージ → CI）で行い、`ingest-rules` では反映されない（`CLAUDE.md`）。`ingest_urtect` の再実行は不要（新 signal の `MENTIONS_SIGNAL` 辺が無くても、manual 検索は質問文の全文検索の結果を併用する）。

## 8. 不変条件

- 第 2 層と第 1 層 mandatory の `Escalate` は KR の有無・内容に依存しない（同じ入力で同じ判定）。
- 第 1 層 advisory と第 2 層が同時に当たる入力は layer 2（§3.2）。
- hazard 級 signal が累積にある会話では、`known_resolution_id` を持つ判定を返さない。
- KR 同乗の取次でも、取り次ぐ項目について金額・可否・条件を顧客に提示しない（`2026-10-07-partial-answer-with-handoff-design.md` §3.5 の歯止めをそのまま適用）。
- `known_resolution_id` を持つ判定の下書きは、KR の回答本文を唯一の材料として生成し、手順の番号・順序・数値・注意書きを KR のまま保つ（§4.2）。KR 由来の下書きも egress gate・`accept_draft`・取扱製品ゲートを免除されない（§4.3）。
- 取次事象・受付番号は 1 case に 1 回（`2026-10-14-answer-after-escalation-design.md` §5）。
- 同じ `signals` の KR が複数あるときは `verified_at` が最新のものが選ばれる（§3.3）。
- `excluded_known_resolution_ids` に載った id の KR は、`evaluate` でも `search_known_resolutions` でも当たらない（§3.3）。
- 監査に発話本文と KR の回答本文を出さない。

## 9. テスト

| 対象 | 固定する内容 |
| --- | --- |
| `decide` の層順（純関数） | 第 2 層と第 1 層 mandatory は KR があっても現行の `Escalate`。第 1 層 advisory + 第 2 層の同時一致 → `Escalate layer 2`（`missing` 空、`hearing: None`）。第 1 層 mandatory + 第 2 層の同時一致 → `Escalate layer 1`。KR + advisory → `Escalate layer 1` で `known_resolution_id: Some`、`missing` 空、`hearing: None`。KR のみ → `Allowed { KnownResolution }`。`hazard_signal_count > 0` → KR を使わず現行の判定。KR は `turn_signals` で照合し `question_signals` では照合しない（累積にだけ KR の signal がある入力で当たらない） |
| 取次済み case での KR（`Harness::evaluate`） | 前ターンに hazard 級 signal、今ターンに KR の signal だけ → `hazard_signal_count` が累積から数えられ、KR 不適用。前ターンに advisory の signal、今ターンに KR の signal だけ → `Allowed { KnownResolution }`（判定用 signal が今のターンだけのため） |
| `match_known_resolution` | 余りありで適用。複数当たりは specificity 最大、同値は `verified_at` の新しい順、それも同値なら `id` 昇順（入力順を入れ替えても結果が同じ。`verified_at` が `None` の KR は最も古い）。空の `signal_set` は不適用。型番 signal を含む KR は、その型番 signal の無い質問（型番なし、別の型番だけ）に当たらず、`model_adc_v724` が同時に立つ `led_red_blinking` + `model_adc_v724x` + `model_adc_v724` の質問に当たる |
| 分割入力（`Harness::evaluate`、未取次の case） | 1 ターン目に `led_red_blinking` だけ、2 ターン目に `model_adc_v724x` だけの会話で、どちらのターンも `known_resolution_id` を持たず、マニュアル直接性の判定になる（累積に KR の signal が揃っていても KR を使わない。§3.3） |
| `handoff_items_for_decision` | 第 1 層 advisory + 第 2 層の同時一致（`Escalate layer 2`）で、項目が第 2 層の `domain_signals` から導かれ、advisory ルールの項目を含まないこと |
| 補助検索（`Harness::evaluate`） | KR 同乗の取次でも `is_partial_answer_candidate` が真なら現行どおり走り、結果が `hits` に入ること。下書きの材料には入らないこと（下の `draft_customer_reply` 行） |
| `can_draft_partial_answer` | `known_resolution_id` ありなら `best_manual_score` が `None` でも可。構造条件・有効化条件は現行のテストのまま |
| `draft_customer_reply`（KR の下書き） | `Allowed { KnownResolution }` と KR 同乗の取次で、LLM に渡る材料が KR 本文 1 件だけ（出典「承認済みの回答（known_resolution）」。マニュアル抜粋を含まない）で、2,500 字を超える KR 本文が切り詰められないこと。system prompt に §4.2 の KR の規則が含まれること。KR 同乗の取次では brief が `kind: Escalation`・`disclosure: Some`・`handoff_ack` が `customer_ack`・`partial_answer_ok: true` で、system prompt に部分回答の規則（`handoff_items` の金額・可否・条件を書かない規則を含む）が含まれ、`ReplyKind::Answer` 専用の結びの規則「こちらで解決しそうでしょうか」が含まれないこと。KR なしの取次は現行どおりマニュアル抜粋を材料に LLM を呼ぶ。`resolutions` に id が無ければ `None` |
| 後段のゲート | KR 由来の下書きが egress gate で不採用なら `None`、`accept_draft` で不採用なら `customer_reply_draft` が `null` になること（§4.3） |
| `known_resolutions_from_nodes` | `verified_at` 属性が RFC 3339 なら `Some`、無ければ `None` で、他の属性の読み取りが変わらないこと。属性が有って解析できなければ `kr_id` を含む `invalid verified_at` のエラーで全体が `Err` になること |
| `exclude_known_resolutions`（純関数）と適用箇所 | リストの id だけが除かれ、空のリストでは入力がそのまま返ること。`Harness::evaluate` で除外 id の KR の signal が揃った入力が KR に当たらずマニュアル直接性の判定になること。`search_known_resolutions` で同じ入力が `none` になること |
| 合成 | KR 同乗の取次（未取次・取次済み）の最終応答文が §5 の形になること。取次事象が未取次では 1 回作られ、取次済みでは作られないこと |
| `clarification_allowed` | 第 3 層 `InsufficientDirectness` で `true`。第 1 層の KR 同乗の取次（`missing` 空）で `false` |
| WORM 監査の provenance（`Harness::evaluate`） | KR 同乗の取次で、監査イベントの `retrieved_node_ids` に KR ノード id、`governing_norm_ids` に KR id が入ること。KR なしの取次ではどちらにも KR が入らないこと |
| `search_known_resolutions` | `match_kind` が `applicable` で `leftover_signals` に余りが入ること。`none` で空 |
| 既存 | `UnknownAddedSignal` / `BlockedByAddedSignal` を参照していたテストを新しい規則へ書き換える。他の既存テストが通る |
| lexicon | 2 signal が読み込め、`register_known_resolution` の語彙チェックを通る。「クラウドの保存期間はどのくらいですか」「ADC-V724X が赤く点滅しています」で該当 signal が立つ。「どのくらい残っていますか」「保証期間はどのくらいですか」「個人情報の保存期間を教えてください」「解約月は何日分の請求になりますか」「個人情報は何日間保存しますか」「請求はどこまでさかのぼれますか」では `cloud_storage_retention` が立たない |

## 10. 実出力の検証（マージ前に必須）

`real_llm_partial_answer_smoke`（`server/src/harness/real_llm_smoke.rs`）に KR のケースを加える。材料は現行どおり本番 `search_manual` の実結果（`question_hits` / `signal_hits`）で、KR は `server/data/urtect/known-resolutions.sample.json` から与える。

### 10.1 ケースファイル（`CS_VERIFY_CASES_JSON`）の項目

| 項目 | 必須 | 内容 |
| --- | --- | --- |
| `question` | 常に | 1 ターン目の質問文 |
| `hits_json` | 常に | 1 ターン目の `search_manual` 実結果ファイル（現行の形） |
| `expect` | 常に | `handoff` / `allowed` / `escalated_followup`（現行）に、`knowledge_first` / `knowledge_first_handoff` / `knowledge_first_followup` を加える |
| `kr_signals` | `knowledge_first*` の 3 値で必須、他では禁止 | 適用されるべき KR の `signals`。フィクスチャの `signals` 集合とこの集合が一致するレコードを 1 件に特定する（0 件・2 件以上はケース読み込みのエラー） |
| `followup_question` / `followup_hits_json` | `escalated_followup` と `knowledge_first_followup` で必須、他では禁止 | 2 ターン目の質問文と `search_manual` 実結果ファイル |
| `kr_must_contain` | `knowledge_first*` の 3 値で必須（1 件以上）、他では禁止 | 下書きが含むべき文字列の配列（部分一致）。KR の `answer` にある、書き方が 1 通りに決まる短い語（IP アドレス、秒数、名詞。「192.168.1.1」「15秒」「初期化」など）を `answer` から写して書く。文や句読点を含む長い文字列は LLM の言い回しで外れるため書かない。`answer` に無い文字列はケース読み込みのエラー |

- `kr_signals` を持つケースでは、フィクスチャの **全レコード** を `decide` の `resolutions` として全ターンに渡す（specificity の選択も実動させる）。id は配列順に `sample-1` 〜 `sample-4` を振る。`kr_signals` を持たないケースは現行どおり `resolutions` を空にする。
- KR を材料にした下書きの生成と後段のゲートは本番の `Harness::draft_customer_reply` を通す（smoke 側で複製しない）。

### 10.2 `expect` ごとの assert

| `expect` | ターン | assert |
| --- | --- | --- |
| `knowledge_first` | 1 | 判定が `Allowed { source: KnownResolution, known_resolution_id: Some(<kr_signals のレコード id>) }`。下書きが採用され、最終応答文は下書きそのもの。下書きが `kr_must_contain` の文字列を全て含む。取次の一文（`build_handoff_sentence`）と「受付番号」を含まない |
| `knowledge_first_handoff` | 1 | 判定が `Escalate { layer: 1, known_resolution_id: Some(<同上>) }` で `missing` 空・`hearing: None`。部分回答の下書きが採用され、最終応答文が 下書き / 取次の一文 / 受付番号ブロック の順（`assemble_partial_answer_reply`）。下書きが `kr_must_contain` の文字列を全て含む |
| `knowledge_first_followup` | 1 | 判定が `Escalate { layer: 1, known_resolution_id: None }`（mandatory `human-handoff`。現行 `escalated_followup` の 1 ターン目（mandatory `initial-cost-quote`）と同じ組み立て。受付番号を発行する） |
| 同 | 2 | `already_escalated: true`、累積 = 1 ターン目 ∪ 2 ターン目で評価し、`decision_basis: TurnOnly`、判定が `Allowed { source: KnownResolution, known_resolution_id: Some(<同上>) }`。対照として、累積を判定用 signal にすると mandatory が KR より先に確定し `Escalate layer 1` で `known_resolution_id: None` になること（§3.2 の層順）を assert する。最終応答文が `build_answer_in_escalated_case`（下書き + 受付番号の確認中の一文）で、受付番号が 1 ターン目のもの。smoke は現行の `escalated_followup` と同じく `out_of_hours_now: false` で合成するため、営業時間の案内行は含まない。下書きが `kr_must_contain` の文字列を全て含む |

下書きの文面は LLM が生成するため assert で固定しない。assert で固定するのは `kr_must_contain`（§10.1。KR の `answer` にある短い語）の包含だけで、手順の番号・順序・注意書きが KR のまま保たれているかは、smoke がケースごとに KR の `answer` と生成された下書きを並べて出力し、ユーザーが確認して承認する。取次の一文・受付番号ブロック・確認中の一文との合成を含む最終応答文の全文も同じ出力で確認する（`verify-real-output-before-merge`）。

### 10.3 ケース

| ケース | `expect` | 1 ターン目 | `kr_signals` | `kr_must_contain` | 2 ターン目 |
| --- | --- | --- | --- | --- | --- |
| クラウドの保存期間 | `knowledge_first` | 「クラウドの保存期間はどのくらいですか」 | `cloud_storage_retention` | 「クリップ数」「保護」 | なし |
| 赤い点滅 | `knowledge_first` | 「ADC-V724X が赤く点滅しています」 | `led_red_blinking`, `model_adc_v724x` | 「起動中」「192.168.1.1」「15秒」 | なし |
| ルーター変更後 | `knowledge_first` | 「ルーターを変えたら、カメラが Wi-Fi につながらない」 | `network_environment_changed`, `wifi_connection_failure` | 「30秒」「初期化」 | なし |
| KR + advisory | `knowledge_first_handoff` | 「保証期間中ですが、ADC-V724X が赤く点滅して動きません」 | `led_red_blinking`, `model_adc_v724x` | 赤い点滅と同じ | なし |
| 取次済み case で KR | `knowledge_first_followup` | 「担当者につないでください」 | `led_red_blinking`, `model_adc_v724x` | 赤い点滅と同じ | 「ADC-V724X が赤く点滅しています」 |

質問文は現行 lexicon の語形で KR の signal が全て立つものにしてある（`normalize_key` は英数字以外を落とすので「Wi-Fi につながらない」は `wifiにつながらない` に一致する。「繋がらなくなりました」は `wifi_connection_failure` のどの語形にも一致しないため使わない）。

### 10.4 フィクスチャ

KR の signal 集合・適用条件・回答本文の正本は `server/data/urtect/known-resolutions.sample.json`（本書と同じブランチで新設。配列で 4 レコード: 登録済みの `continuous_recording`、`network_environment_changed` + `wifi_connection_failure`、本書で登録する `cloud_storage_retention`、`led_red_blinking` + `model_adc_v724x`）。各レコードは `signals: string[]`、`applicability: string`、`answer: string`、`rationale_text: string` の 4 フィールドだけを持ち（MCP `add_known_resolution` の同名フィールド。`signals` / `applicability` / `answer` が必須フィールドで、`rationale_text` は admission（`Harness::admit_known_resolution`）が要求する根拠アンカーである。admission は `manual_section_keys` が空で `rationale_text` も無い登録を「known resolution requires at least one evidence anchor」で拒否する。本書の KR は出典となるマニュアル節を持たないため、根拠アンカーは `rationale_text` だけにし、`manual_section_keys` と `origin_escalation_id` は渡さない。`rationale_text` は本番 `urtect` の `manual_schema = "manual_v1"` で受理され、`Rationale` ノードと `BECAUSE` 辺として残る）、`kr_id` は持たない（id はサーバが登録時に採番する）。smoke が読むのは `signals` / `applicability` / `answer` だけで、`rationale_text` は読み捨てる（照合用の `rules::KnownResolution` に対応するフィールドが無い）。smoke はこのファイルを `CARGO_MANIFEST_DIR` からの相対パスで読み、本文を本書や smoke 側に複製しない。登録済み 2 件の本文は本番の `search_known_resolutions` が返した `answer` と同一である。登録済み 2 件の `rationale_text` は、§11 の誤登録で登録し直すときに渡す値であり、元の登録で渡した根拠アンカーは読み戻さず（`search_known_resolutions` は返さない）、本書では変更しない。

## 11. 展開手順

手順 1〜6 を順に行い、全て通った時点で Issue #103 を完了とする。途中で止まった場合の扱いは次の 3 種類だけで、手順ごとの該当を各項に書く。

- 疎通・認証・基盤（応答が得られない。401 / 403、5xx、タイムアウト、到達失敗、MCP の接続エラー）: 本書の対象外の障害として扱い、本書の手順の判定には使わない。401 / 403 は資格情報（MCP は claude.ai コネクタの再接続、`/api/reply` は `CS_SUPPORT_ANSWER_API_KEY` の値）を直してから、5xx・タイムアウト・到達失敗は Cloud Run の稼働 revision が健全（`/livez` が 200）になってから、同じ手順の失敗した呼び出しだけをやり直す。手順を遡らない。例外は手順 2 で、下記のとおり再登録の前に登録の有無を確かめる。
- コードの不具合（判定・下書き・合成・lexicon が本書と違う）: 本番を手で戻さない。修正 PR を main にマージし CI がデプロイした後、失敗した手順からやり直す（roll forward。`deploy-only-via-ci`）。修正が入るまでの本番は、登録済みの KR で §3 の判定を続ける。不具合が KR の回答を誤って出すものなら、修正 PR に、その KR の id を `excluded_known_resolution_ids` に加える変更を含める（修正と同じデプロイで止まる）。
- KR の誤登録（登録したレコードの `signals` / `applicability` / `answer` がフィクスチャと違う）: backend に削除が無く、MCP にも無効化ツールが無い（§13）ため、誤ったフィールドで手を分ける。
  - `applicability` / `answer` だけが違う（`signals` はフィクスチャどおり）: 同じ `signals` でフィクスチャどおりのレコードを `add_known_resolution` で登録し直す。§3.3 の規則で `verified_at` が新しいレコードが選ばれるため、誤ったレコードは残るが以後選ばれない。デプロイは要らない。登録し直した後は手順 3 からやり直す。
  - `signals` が違う（集合の過不足・綴りの誤り）: 再登録では止められない（誤った集合が立つ質問に共通する正しい本文は存在しない）。誤ったレコードの `kr_id`（手順 2 の `add_known_resolution` の応答 `kr_id`。控え損ねたら、誤った `signals` が立つ質問文で `search_known_resolutions` を呼んだ応答の `resolution.kr_id`）を、`config.cloudrun.toml` の `excluded_known_resolution_ids` にその id を加える PR をマージし、CI がデプロイしたら、正しい `signals` のレコードを登録して手順 3 からやり直す。デプロイまでの間、誤ったレコードはその集合が立つ質問に当たり続ける。

1. main へマージし CI がデプロイする（lexicon が反映される）。CI が失敗したら、原因を直す PR をマージして CI を通し直す。通るまで手順 2 へ進まない（`register_known_resolution` は lexicon に無い signal を拒否するため、登録は失敗して何も書かれない）。
2. 本番 MCP `add_known_resolution` で `known-resolutions.sample.json` の `cloud_storage_retention` と `led_red_blinking` の 2 レコードを登録する。渡す `signals` / `applicability` / `answer` / `rationale_text` の 4 フィールドは、次のコマンドの出力（レコード全体）をそのまま使い、手で打ち直さない。`manual_section_keys` と `origin_escalation_id` は渡さない。`rationale_text` を落とすと admission が「known resolution requires at least one evidence anchor」で拒否し、何も書かれない（§10.4）。登録が `signals` の語彙エラーで拒否されたら、手順 1 のデプロイが反映されていない（Cloud Run の稼働 revision の image tag がマージコミットの SHA か確認する）か lexicon の不具合なので、コードの不具合として扱う。登録の応答が得られなかった（タイムアウト・5xx・接続エラー）場合は、登録し直す前に手順 3 の `search_known_resolutions` を同じ質問文で呼び、`applicable` で該当レコードが返るなら登録は済んでいるので手順 3 へ進み、`none` なら登録し直す。確認できないまま登録し直して同じ内容が 2 件になっても、§3.3 の規則で `verified_at` が新しい方が選ばれ、内容が同じなので応答は変わらない。

   ```sh
   jq '.[] | select(.signals == ["cloud_storage_retention"])' server/data/urtect/known-resolutions.sample.json
   ```

   ```sh
   jq '.[] | select(.signals == ["led_red_blinking","model_adc_v724x"])' server/data/urtect/known-resolutions.sample.json
   ```

3. 本番 MCP `search_known_resolutions` を「クラウドの保存期間はどのくらいですか」「ADC-V724X が赤く点滅しています」で呼び、`match_kind: applicable` で返る `resolution` の `signals` / `applicability` / `answer` がフィクスチャの同名フィールドと一致し、`question_signals` が §10.3 の `kr_signals` を含むことを確認する。続けて「ADC-V523 が赤く点滅しています」で呼び、`match_kind: none` が返ることを確認する。`resolution` の 3 フィールドのどれかが違えば KR の誤登録。`question_signals` に `kr_signals` が揃わない、または「ADC-V523」で `applicable` が返るのは、lexicon か照合の不具合（コードの不具合）。`applicable` のはずが `none` で `question_signals` は揃っているなら、登録したレコードの `signals` の誤登録。
4. 本番 MCP `evaluate_answerability`（`ReplyDraftPolicy::Always`。判定用 signal は累積で、取次済みの経路を通らず、取次の一文・受付番号ブロックも合成しない）で §10.3 のケース 1〜4 を、ケースごとに新しい case で実行し、次を確認する。
   - ケース 1〜3: `decision: allowed`、`known_resolution_id` あり、`customer_reply_draft` が §10.3 の `kr_must_contain` の文字列を全て含み、手順の番号・順序・注意書きがフィクスチャの `answer` と一致する（目視）。
   - ケース 4: `decision: escalate`、`layer: 1`、`known_resolution_id` あり、`partial_answer: true`、`handoff_items` に故障・保証交換の項目、`customer_reply_draft` がケース 1〜3 と同じ条件を満たす。
   - 判定・`known_resolution_id`・`handoff_items` が違えばコードの不具合。判定は合っていて `customer_reply_draft` が `null` なら、Cloud Logging の warn（egress gate の理由、または `accept_draft` の不採用理由）を読む。KR 本文の表現が egress gate に当たっているなら、本文を直したフィクスチャを PR で入れた上で、同じ `signals` で登録し直す（KR の誤登録と同じ手）。それ以外はコードの不具合。
5. ケース 5 は MCP では再現できないため `POST /urtect/api/reply`（Bearer は `CS_SUPPORT_ANSWER_API_KEY`。`cs-support-line` が使うのと同じ経路）で確認する。1 回目は `message: "担当者につないでください"`（mandatory `human-handoff`。advisory の `contract-billing` に当たる「解約したいです」は `clarify_max_turns` まで聞き返しになりうるため使わない）を送り、応答の `case_id` と本文の受付番号を控える（本番に取次事象が 1 件作られる。手順 5 をやり直すたびに 1 件増える）。2 回目は同じ `case_id` と、1 回目の顧客発話・応答を `history` に入れて `message: "ADC-V724X が赤く点滅しています"` を送り、`reply_text` が次のどちらかになることを確認する（`build_answer_in_escalated_case`。§5）。
   - 営業時間内に実行したとき: KR を材料にした下書き / 空行 / 「※先にお受けしたお問い合わせ（受付番号: <1 回目の番号>）は、担当者が確認を進めています。」
   - 営業時間外に実行したとき: 上記に続けて「現在は対応時間外のため、担当者からのご連絡は翌営業時間（<営業時間>）以降となります。」の 1 行。
   - 1 回目が取次にならない、2 回目が固定返信や取次の一文付きになる、受付番号が 1 回目と違う、営業時間内なのに案内行が付く（またはその逆）、のいずれもコードの不具合。営業時間の判定は `config.cloudrun.toml` の `[api.business_hours]` を基準にする。
6. `gcloud run jobs execute verify-deploy --project sivira-cs-support --region asia-northeast1 --wait` を実行し `"passed": true` を確認する。`false` なら `fail` の検査名と `detail` を読み、`2026-10-06-post-deploy-smoke-design.md` の各検査の規則に照らして原因を特定する。本書の変更に起因するもの（`layer1` 検査の lexicon 突合と判定）はコードの不具合。疎通確認（preflight）の失敗と `threads` / `thread_detail` / `stats` 検査の読み出しエラーは「疎通・認証・基盤」として扱い、復旧後に手順 6 だけやり直す。

## 12. ドキュメント更新

正本（`specs/`）、概説（`docs/system-overview.md`）、現行仕様として参照されている設計文書（`2026-10-07-partial-answer-with-handoff-design.md`、`2026-10-14-answer-after-escalation-design.md`、`2026-08-12-conversation-flow-v11-design.md`、`2026-09-21-jev-shadow-design.md`）は本書の規則に書き換え、旧規則を残さない。`docs/superpowers/plans/` 配下の実装計画は実行済みの記録として変更しない。行番号は本書作成時点のもの。本表の書き換えは本書の実装と同じ PR で行い、コードと正本を同時に main に入れる（旧規則と新規則が main で共存する期間を作らない）。

| 文書 | 箇所 | 変更 |
| --- | --- | --- |
| `specs/production-cs-mcp.md` | 「### 制約」の「known_resolution は第1層・第2層をバイパスできない」（309 行目）と GMR の節の同じ文（643 行目） | 「第 1 層 mandatory・第 2 層をバイパスできない。第 1 層 advisory と同時に当たるときは KR の回答に取次の一文を添える」に改め、本書 §3.2 を参照する |
| 同 | 構成図の「(B) 3 層判定」の 3 行（89〜92 行目） | 「第1層 mandatory / 第2層 禁止領域 / known_resolution / 第1層 advisory / 第3層 マニュアル直接性」の順に改める |
| 同 | 「判定アーキテクチャ：3 層構造」の本文と図（99〜112 行目）「先に止まった層で確定」「安全に関わる層（1・2）を必ず先に置き」「[第3層] 回答可能性判定（known_resolution 再利用 → マニュアル直接性 → グレーは保留）」 | 本文を「第 1 層 mandatory と第 2 層を先に置き、KR はその後、第 1 層 advisory の前」に改め、図を本書 §3.2 の層順に描き直す。「先に止まった層で確定」は KR 同乗の取次（KR と第 1 層 advisory の両方が確定に関与する）を例外として明記する |
| 同 | 第 1 層の「特徴」（123 行目）「マッチしたら問答無用で確定ルーティング、回答生成に進ませない」 | mandatory に限る旨を加え、advisory は KR 同乗の取次（本書 §3.2）と部分回答（`2026-10-07-partial-answer-with-handoff-design.md`）で回答を添えうると改める |
| 同 | エスカレーションの位置づけの段落（367 行目）「該当したら問答無用でそこへ振る（第1層）」と、判定の確実性の段落（369 行目）「第1層・第2層にマッチしたら回答生成への経路が閉じ、ルーティング先だけが返る」 | 「問答無用で振る」と「回答生成への経路が閉じる」を第 1 層 mandatory と第 2 層に限る。第 1 層 advisory は KR 同乗の取次（本書 §3.2）と部分回答（`2026-10-07-partial-answer-with-handoff-design.md`）で回答を添えうると改める |
| 同 | 実装順序の「9. 第1層 → 第2層 → 第3層 の順に判定する」（481 行目） | 本書 §3.2 の層順に改める |
| 同 | S1-1 パイプラインの「(B) 3 層判定」（619〜627 行目） | 層順を本書 §3.2（第 1 層 mandatory → 第 2 層 → KR → 第 1 層 advisory → マニュアル）に改め、「(a) known_resolution 適用条件内（Δ=なし・包含方向のみ）」を §3.3 の規則に改める |
| 同 | S1-3 の照合アルゴリズム（`match_known_resolution` の擬似コード、704〜710 行目）と軸 2（「変色≠変色＋カビ」、716 行目） | 余りを許す規則に書き換え、軸 2 は「KR の条件集合を含む質問に適用する」に改める |
| 同 | Issue #54 改訂の段落（406 行目）、decision 構造の `reason` の列挙（417 行目）、S1-11 の Issue #54 改訂の項（852 行目）、ロードマップ遵守事項の「変色→+カビで unknown_added_signal 自動エスカレーション」（921 行目） | `unknown_added_signal` を外し、累積再判定の項は本書 §3.3 を参照する |
| 同 | KnownResolution の構成要素「適用条件（前提）」（299 行目）と構造の `applicability` のコメント（673 行目） | 「人間が記述する」に「照合には使わない。機種・契約の限定は signal 集合（lexicon の型番 signal 等）で表す」を加える |
| `docs/system-overview.md` | D1（36 行目）「判定は常に会話全体で行う」 | 「第 1・2 層は判定用 signal（未取次は累積、取次済み case は今のターン。`2026-10-14-answer-after-escalation-design.md` §2）、KR は今のターンの signal で照合する」に改める |
| 同 | D3（38 行目）の三層の順序と「検証済みノウハウ(known_resolution)の適用条件照合も判定に組み込まれる」 | 本書 §3.2 の層順（第 1 層 mandatory → 第 2 層 → KR → 第 1 層 advisory → 第 3 層）に改め、KR は今のターンの signal が KR の signal 集合を含むときに適用する（余りを許す）と改める |
| `specs/signal-vocabulary.md` | `いくら` の語形限定の項（`match_known_resolution` は leftover が空のときしか適用しない）、`price_question` が KR 適用に与える影響の段落 | 余りを許す規則に合わせて書き換える。新 signal 2 件を語彙表に加える |
| 同 | 既知の限界（受容済みリスク）の段落（61 行目） | 「第三層グレー（`UnknownAddedSignal` → `clarification_allowed = true`）」を「第三層グレー（`InsufficientDirectness` → `clarification_allowed = true`）」に改める |
| `2026-10-07-partial-answer-with-handoff-design.md` | §3.1 | 関連度の条件を本書 §4.1 への参照に置き換える |
| 同 | §3.2 の下書きの入力契約 | `known_resolution_id` を持つ判定では KR の回答本文を唯一の材料として LLM に渡し、マニュアル抜粋を渡さない（本書 §4.2）と、その下書きも §3.5 の歯止めと egress gate を通る（本書 §4.3）ことを加える |
| `2026-10-14-answer-after-escalation-design.md` | §8 | 「KnownResolution の照合順序の変更」を本書への参照に置き換える |
| 同 | §2 の `question_signals` の定義（21 行目）と §6 の「判定入力の切り替え」の行（58 行目） | 「第 1・2 層ルールと KnownResolution の照合」から KnownResolution を外す。KR の照合は policy によらず今のターンの signal（`turn_signals`）で行う（本書 §3.2・§3.3） |
| `specs/production-cs-mcp.md` | 「CS に GMR を持ち込む際の絶対条件」の段落（201 行目）「新質問がその条件内に収まるときだけ再利用する」 | 「KR の signal 集合を今のターンの signal が含むときに再利用する（余りを許す）」に改め、本書 §3.3 を参照する |
| 同 | 第 3 層の説明の「(a) Δ判定: この質問は known_resolution の適用条件内に収まるか？」（148〜149 行目。「収まる（Δ=なし）→ 確定経路を再利用」を含む） | 「(a) KR 照合: 今のターンの signal が KR の signal 集合を含むか（余りを許す。本書 §3.3）」に改める |
| 同 | 会話層（最小）の項（568 行目）「毎ターン累積集合での再判定」 | 第 1・2 層の説明として残し、「KR は今のターンの signal で照合する（本書 §3.3）」を加える |
| 同 | 「射程はどう広がるか」の大前提の項（207 行目）「『+カビ』がある時点で『変色ルール』を適用してはならない」 | 追加条件は第 1 層 mandatory・第 2 層・hazard 判定が KR より先に受け止める規則（本書 §3.2）に書き換え、どのルールにも無い余り signal は KR の適用を妨げないことを明記する |
| 同 | 「ルール照合の核心」の軸 2（221 行目）「条件の増減（射程の問題）」 | S1-3 の軸 2 と同じく「KR の条件集合を含む質問に適用する」に改める |
| 同 | 「Step 1 実装への遵守事項」の 1（999 行目）「条件が増えたら…再判定で自動的にエスカレーション…照合原則の変更は不要」 | 第 1・2 層の累積再判定の説明として残し、KR の照合は今のターンの signal で余りを許す規則（本書 §3.3）に改める。「照合原則の変更は不要」を削る |
| 同 | 会話層の累積 signal 集合の項（849 行目）「MCP `evaluate_answerability` の判定は常に『累積 signal 集合 + known_resolution』に対して行う」 | 「第 1・2 層と stakes・manual 検索は累積 signal 集合で、KR は今のターンの signal で照合する（本書 §3.2・§3.3）」に改める |
| 同 | 音声応答の出力段の項（985 行目）「known_resolution ヒット（gate 通過が事前確定）」 | 「known_resolution ヒット（KR 本文も egress gate・`accept_draft`・取扱製品ゲートを通す。本書 §4.3）」に改め、「事前確定」を削る |
| `2026-08-12-conversation-flow-v11-design.md` | `clarification_allowed` の定義と契約テストの列挙（29 行目・119 行目） | `UnknownAddedSignal` を列挙から外す |
| `2026-09-21-jev-shadow-design.md` | Jev を呼ばない経路の列挙（296〜305 行目） | `UnknownAddedSignal` を列挙から外す。「第 1 層 advisory に KR が同乗するターン（`hearing: None`。本書 §3.2）」と「第 1 層 advisory と第 2 層の同時一致（layer 2、`hearing: None`。本書 §3.2）」を列挙に加える（`warranty-failure` に当たっても、この 2 つでは Jev を呼ばず顧客発話も送らない） |
| `server/src/rmcp_server.rs` | `evaluate_answerability` の `clarification_allowed` の doc comment、`SearchKnownResolutionsResponse` の `match_kind` / `leftover_signals` の doc comment、tool description の `reason` | 新しい値の集合に書き換える |
| `server/src/harness/mod.rs` | `EvaluationOutcome.clarification_allowed` の doc comment、`clarification_allowed` 関数の doc comment | 新しい規則に書き換える |
| `server/src/advisor/materials.rs` | `match_advisor_known_resolution` の doc comment | `Option` を返す規則に書き換える |

## 13. 対象外

- URTECT ガイドを alarm.com より優先する検索順位、第 1・2 層以外での signal 誘導検索。
- 破壊的操作（初期化・フォーマット・削除）の案内に対する決定論の歯止め。
- KR 由来の下書きが手順・数値・注意書きを保っているかを決定論で検査する本番のゲート。保持は system prompt の規則（§4.2）と smoke の確認（§10）で担保する。
- `prohibited_domains` ごとの固定応答文。
- KR ごとの「advisory の取次を省略する」フラグ（actor 突合表が DB に入るまで作らない）。
- KR 同乗の取次ターンの `last_kr_id` 記録と outcome 連携。
- KR の `applicability`（自由文）による照合。照合は `signals` だけで行い、機種の限定は lexicon の型番 signal を `signals` に含めて表す（§3.3）。
- KR を削除・無効化する MCP ツール（`status` による無効化と、照合での `status` / `grade` の参照を含む）。誤登録は §11 のとおり、`answer` / `applicability` は同じ `signals` での再登録で、`signals` は `excluded_known_resolution_ids` で止める（§3.3）。
- `api.rs` / `harness/mod.rs` の分割など、Issue #102 のレビューで提案された構造変更。
