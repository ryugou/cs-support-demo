# Production CS MCP

この文書は、これから新規に設計・実装する Production CS MCP の source of truth である。

この文書は会話履歴より優先する。Production CS MCP に関する議論・設計・実装判断を始める前に必ず本書を読み、会話で結論が更新された場合は同じターンで本書も更新する。

> 改訂メモ: 初版は「同じシチュエーションを想定して書かせた草案」であり、正解ではなかった。とくにエスカレーションの本質を権限・情報量・法務に寄せていた点が誤りだった。本版では、エスカレーションを「結果の重大性に基づく判定」として捉え直し、判定全体を 3 層構造に再設計した。あわせて「ハーネス」の定義、「LLM に判断させない」の粒度、`evidence_sufficient` の定義を修正した。

## 目的

Production CS MCP は、コールセンター業務で日本語問い合わせ対応を行う担当者を支援する CS domain MCP である。

単なるマニュアル検索ではなく、次の 3 課題をプロダクション要件として解決する。

1. ノウハウの蓄積（使うほど射程が広がる学習ループ）
2. 権限管理（actor ごとのアクセス制御）
3. 確実なエスカレーション（答えてはいけない質問を確実に人へ回す）

この 3 課題を満たさない状態は、本フェーズでは完成とみなさない。

## 絶対前提

`cs-support-mcp` は AuthN / AuthZ Harness を内包する CS domain MCP とする。

これは候補ではなく前提である。LLM client と MCP の間に任意の外部 Harness を置き、その外部 Harness だけに認可・回答可否・エスカレーションを任せる設計は採用しない。

MCP は CS 業務境界そのものであり、以下を自分の責務として持つ。

- 認証済み actor の確定
- actor の権限・所属・担当範囲の解決
- deterministic な access scope 算出
- vegapunk / PunkRecord への query scope 強制
- 回答可能性の判定
- エスカレーション要否の判定
- 監査記録
- ノウハウ記録

LLM client は判断主体ではない。

ただし「LLM は判断しない」を全工程に課すのは誤りだった。**「判断（最終決定）」と「抽出（入力生成）」を分ける。**

- **抽出（自然文を読む）= LLM 可**: 質問にどんな条件 / signal が含まれるか、取得した根拠が質問に直接対応するか、を読み取る。
- **判断（結論を出す）= MCP 内 Harness（Rust）が独占**: 権限可否、scope 決定、回答可否、エスカレーション要否、ルーティング先。ここに LLM を介在させない。

LLM が出すのは「decide() への入力データ」であって結論ではない。この分離が守られる限り、抽出を LLM が担っても判定の確実性は壊れない。

### 用語定義（2026-07-13 確定）

本プロジェクトでは以下の用語を使う。業界で「agent」の定義が割れているため、ここで固定する。

- **エージェント（= LLM コンポーネント）**: 入出力形式が固定され、出力はコードで検証され、**制御フローはコードが所有する**、内部処理に LLM が関与する部品。例: signal 抽出器（質問 → 語彙閉集合）、文面生成、意味検索。単に「エージェント」と書いた場合はこちらを指す。
- **自律エージェント**: 制御フロー・ツール選択の主導権を **LLM 自身が持つ**もの。本システムでは MCP client（Claude / ChatGPT）のみ。判定パス（3 層判定・egress・記録）には置かない。

この語彙で言い直すと、本システムは「オーケストレーター（Harness / Rust・決定論）が、複数のエージェント（LLM コンポーネント）を固定の順序で呼び、各境界に検証ハーネス（scope 上書き・lineage 突合・egress・WORM）を置く」構成である。判断（結論）は常にオーケストレーター側の純関数が出す。

## ハーネスの定義（2 種類ある）

「ハーネス」という 1 語が 2 つの別物を指す。混ぜると設計が濁るので分ける。

### (A) 経路封鎖ハーネス（権限）

Rust で決定論的に実装する「経路の封鎖」。確実性が高いのは、判定をミスっても **そもそも query にスコープ外が乗らないから物理的に取得できない** という構造だからである。

```rust
let scope = resolve_scope(&actor, &project)?;     // deterministic
let filter = scope.to_query_filter();              // tenant/label/sensitivity を強制
let evidence = punkrecord.search(query, filter)?;  // filter は caller 改変不可
```

tool input で来た `tenant_id` / `labels` / `schema` は **信用せず捨て、Harness 側 scope で上書きする**。これで「権限外スキーマにアクセスさせない」は構造で保証される。

### (B) decision ハーネス（エスカレーション・回答可否）

こちらは経路封鎖では実現できない。「答えてよいか / 人に振るべきか」は単一 filter に畳み込めず、複数条件の評価結果だからである。

(B) の確実性の正体は「経路封鎖」ではなく **LLM を挟まない deterministic な decision function** であること。同じ入力なら必ず同じ判定が出る純関数であり、単体テスト可能であること。これが (B) の確実性である。

(A) と (B) は別物だが、1 本の pipeline で繋がる。pipeline 前半が (A)、後半が (B)。(B) は (A) の出力（scope と evidence）を入力に取る。

## 基本アーキテクチャ

```text
Claude / ChatGPT / other MCP client
  ↓ 認証情報付き MCP call
cs-support-mcp
  ├─ AuthN
  ├─ (A) AuthZ Harness / deterministic scope enforcement
  ├─ allowed evidence retrieval（scope filter 注入済み）
  ├─ (B) 3 層判定（後述）
  │     第1層 明示エスカレーションルール
  │     第2層 禁止領域
  │     第3層 回答可能性判定（GMR + 人間検証ループ）
  ├─ audit logging
  └─ know-how / learning record write
  ↓
vegapunk / PunkRecord
```

## 判定アーキテクチャ：3 層構造

質問は上から順に各層を通り、**先に止まった層で確定**する。後段は評価しない。安全に関わる層（1・2）を必ず先に置き、再利用が効く層（3）を最後に置く。この順序自体が安全保証である。

```text
質問
 ↓
[第1層] 明示エスカレーションルール（具体条件 → 具体ルーティング）
 ↓ 該当しない
[第2層] 禁止領域（健康・身体症状など、面で答えを禁止）
 ↓ 触れていない
[第3層] 回答可能性判定（known_resolution 再利用 → マニュアル直接性 → グレーは保留）
 ↓ 直接答えられる
回答
```

各層の判定材料の抽出（自然文を読む部分）は LLM が担い、判定・ルーティングの決定は必ず Rust ハーネスが持つ。これは全層共通の不変条件。

### 第1層 — 明示エスカレーションルール

- **役割**: 「この条件なら、この担当 / 部署へ」が事前確定しているピンポイントのルーティング。
- **判定の向き**: ブラックリスト（危険・要確認の具体条件を列挙）。
- **振り先**: ルールごとに固有（安全担当、品証、特定部署など）。
- **例**: health_food で異常申告 → 安全担当 / 化粧品で肌反応 + 継続可否 → 皮膚科リエゾン。
- **特徴**: マッチしたら問答無用で確定ルーティング、回答生成に進ませない。数は多くなくてよい。
- **学習**: 効かせない。固定ルール。メモ化対象外。

### 第2層 — 禁止領域

- **役割**: 個別条件ではなく、領域そのものを面で禁止する安全線。
- **判定の向き**: 抽象度の高いブラックリスト（「健康判断」「身体症状」など領域を列挙）。面で塞ぐので列挙地獄に陥らない。
- **振り先**: 領域ごとの既定の受け皿。
- **例**: 「健康に該当する判断は答えない」「身体の異常申告はエスカレーション」。
- **特徴**: 触れたら必ず止める絶対線。クライアントごとに領域は変わるが、引いた線は学習で覆らない。
- **学習**: 効かせない（最重要）。known_resolution が何件積もろうと、禁止領域は毎回フル評価。GMR の「確定したら再計算しない」を **適用しない** 領域。

第1層・第2層が「使うほど賢くなるが、安全は一切緩まない」の土台。「判断するか否か（禁止線）」はメモ化しない。「どう答えるか（禁止線を抜けた後）」だけメモ化する。

### 第3層 — 回答可能性判定（ノウハウ蓄積 / GMR が効く層）

- **役割**: 禁止線を通過した質問について「確証を持って答えられるか」を判定。実用性のダイヤルであり、学習で射程が広がる層。
- **判定の向き**: ホワイトリストの事前列挙ではなく、**「取得根拠（マニュアル記載 / known_resolution）がこの質問に直接答えているか」** で測る。

> ホワイトリスト（答えてよい質問を事前列挙）を既定にすると「ほぼ全部エスカレーション」になり、CS エージェントとして使い物にならない。だから第3層は列挙ではなく「根拠と質問の直接性」で判定する。事前列挙の限界を受けない。

内部は GMR の枠組みで 2 段:

```text
(a) Δ判定: この質問は known_resolution（検証済みノウハウ）の適用条件内に収まるか？
      収まる（Δ=なし）→ 確定経路を再利用＝自動回答（再計算しない）
      収まらない（Δ=あり）→ (b)へ
(b) 差分評価: マニュアル記載が、その差分に直接答えているか？
      答えている → 回答（この経路を新たな確定候補として記録）
      答えていない（グレー）→ エスカレーション（＝学習の入口）
```

### 層ごとの性質の対比

| | 第1層 | 第2層 | 第3層 |
|---|---|---|---|
| 列挙の粒度 | 具体条件 | 抽象領域（面） | 列挙しない（根拠との突合） |
| 判定の向き | ブラックリスト | ブラックリスト | 根拠の直接性 |
| 振り先 | 固有・具体 | 領域の既定受け皿 | 回答 or 保留→人 |
| 学習 | 効かない | 効かない（絶対線） | 効く（GMR + 人間検証） |
| メモ化 | しない | しない | する |

## 課題 1: ノウハウの蓄積（GMR + 人間検証ループ）

ノウハウの蓄積とは、元データから情報を取得するだけではなく、「その情報をどう活用して回答すべきか」「実際にどう回答したか」「結果がどうだったか」を PunkRecord に残し、次回以降の回答品質に活かす仕組みである。

### GMR をどう位置づけるか

GMR（Graph-Memorized Reasoning, スライド由来の構想用語であり一般的な学術用語ではない）の核は「すでに確定した判断経路を再計算しない」。決定済み部分グラフを永続化し、新規タスクでは既存トポロジーとの差分（Δ）だけを再計算する。

ただし GMR の素直な実装が生むのは **効率（同じ・近い問いが安く速くなる）** であって、**能力向上（前は解けなかったものが解ける）ではない**。

- 能力向上（射程の拡大）の種を入れるのは **人間の検証ループ**。担当者が新しい正解を与えた瞬間に射程が広がる。
- GMR / 差分推論は、その種を **類似質問へ展開** して効果範囲を広げる係。

つまり「使うほど賢くなる」は、能力の源泉が人間であっても体感として成立する。GMR は「能力を上げる仕組み」ではなく「人間検証で得た知識を安く配り、類似質問へ展開する仕組み」と位置づける。投資先を間違えない（最初に作り込むのは GMR の精緻化ではなく検証ループの質）。

### CS に GMR を持ち込む際の絶対条件

GMR が前提にする「同じ入力なら同じ答えでよい」は CS では成立しない。同じ質問文でも、商品ロット・契約・時期・個体状態が違えば答えが変わる。陳腐化した経路の無条件再利用は事故になる。

したがって再利用には **適用条件（known_resolution が想定する前提の集合）** を必ず持たせ、新質問がその条件内に収まるときだけ再利用する。

### 射程はどう広がるか（一般ルールと例外ルールの追加）

例: 「変色」に対し「自然変色なので食べて OK」が検証済みルールとして存在する。

- 大前提（崩してはいけない）: 「変色 + カビ」は「変色」より条件が多く、既存ルールの射程外。**専用ルールが無い限り必ずエスカレーションする。** 「変色」の蓄積が何件あっても「+カビ」がある時点で「変色ルール」を適用してはならない。
- 進化: 「変色 + カビ」は最初エスカレーションされ、担当者が「変色 + カビ → 廃棄」という **新しいルールを追加** する。次回からは専用ルールがヒットし、エスカレーションせず答えられる。これが GMR の進化＝決定済み部分グラフに枝が 1 本増えること。

知識ベースの操作は「上書き」「否定」ではなく **「より具体的な条件を持つルールの追加（insert）」**。既存の一般ルールは無傷のまま、隣に例外ルールが生える。照合時は **条件がより具体的に一致するルール（例外ルール）が優先** される。

> GMR の進化の定義: 「専用ルールが無くてエスカレーションされた条件」に対し担当者が新ルールを追加し、グラフに枝が 1 本増えること。進化の単位は「ルール 1 本の追加」、トリガーは「大前提によるエスカレーション」、担い手は「担当者の新ルール付与」。量が自動で広げるのではなく、未経験条件との遭遇 + 人間評価を 1 件ずつ経て離散的に広がる。

### ルール照合の核心（signal 正規化と包含判定）

照合は「質問文 vs ルール文」の直接比較ではない。間に **「条件への正規化」** 工程が必須。

区別すべき 2 軸:

- **軸1: 表現のゆれ（同じ意味か）** — 「変色」「色が変わった」「色味がおかしい」は同じ条件。意味で吸収して一致させる。文字列完全一致は不可。**→ LLM（意味的正規化）**。
- **軸2: 条件の増減（射程の問題）** — 「変色」と「変色 + カビ」は違う条件。カビという追加条件を厳密に区別する。**→ Rust（signal 集合の集合演算）**。

正規化＝自然文を標準化された条件語（signal）の集合に落とすこと。これが「signal 抽出」の正体。

照合の既定:

```text
質問を signal 集合に正規化する（表現ゆれは意味で吸収、疑わしい語は必ず signal として立てる）
 → ルールの想定 signal 集合と照合する
 → 質問側に、ルールが想定していない signal が 1 つでも残ればマッチ不可＝エスカレーション
 → 候補が複数あれば、条件具体度（signal 数）が最大のルール（最も具体的な一致）を選ぶ
```

急所: 正規化（LLM）が signal を取りこぼすと、危険な追加条件を見落として誤マッチする。軸2 の Rust 照合がどれだけ厳密でも、入力 signal 集合が不完全なら防げない。よって **正規化は「signal を取りこぼさない」ことを最優先** とし、疑わしい語は必ず signal として立てる（立てれば追加条件として残り安全側に倒れる）。

### 「正しい判断の保証」は機械単体では不可能、という割り切り

機械単体で「正しさを保証する」ことはできない。できるのは「正しい判断に近づける」「間違ったときに止める」まで。AI の自律判断で安全領域の正しさを保証する、は捨てる。人間を経路に残すことで実用にする。

具体的には、完全な包含判定（質問の全条件を漏れなく構造化）は LLM 依存で取りこぼしが安全側に倒れないため厳密には実現困難。代わりに次の近似で現実的精度に乗せる。

1. 「全条件の網羅抽出」ではなく「再利用を禁止すべき signal が含まれていないか」の **逸脱検出** に振り替える（第2層の禁止領域チェックと同じ仕組みを流用）。
2. 再利用対象を **同一商品 × 同一質問型** で先に粗く絞り、判定空間を小さくする。
3. 適用条件は **人間（承認者）が明示的に書く**。LLM に推測させない。機械は逸脱検出だけ担う。
4. 技術で保証しきれない残留リスクは、**第2層の領域設定で被害の上限を抑える**（危険領域はそもそも第3層の再利用対象に入れない）。多層防御。

### 直近の現実的初手: AI 草案 + 人間承認（＝知識蓄積の手段）

承認は **known_resolution を蓄積・昇格させるための検証手段**であり、誤答を許容する根拠ではない。誤答はフェーズ・利用者を問わず NG で、誤答になるかもしれないものはエスカレーションに倒す——この基準は全フェーズ不変。

そのうえで、グレーとしてエスカレーションされた質問には AI が草案を作って添付する（文面は AI が作り、学習履歴として残る）。担当者の負荷は「ゼロから書く」→「草案をレビューして承認」に変わり、承認・却下・修正が全て学習データになる。担当者利用フェーズ（Step 1）は authoritative な訂正を同一画面で最も得やすい環境であり、蓄積が最速で回る。

### 昇格・降格（動的な格付け）

承認済み事例が溜まると、経験済みパターンの **確信度** が上がる（ただし射程＝幅は人間評価でしか広がらない。量だけでは広がらない）。これを動的な格付けで扱う。

```text
初期: 全件 (3)。AI 草案、人が承認。安全最優先で蓄積開始。
中期: 承認回数・却下率・承認者多様性が閾値を超えた known_resolution を
      「自動回答 + 事後監査」に格上げ。担当者は格上げ済みパターンから解放。
常時: 未経験パターン・条件が増えた質問は、何があっても (3) に戻る（承認必須）。自動化しない。
監査: 自動回答へ格上げ済みでも、却下が一定数出たら承認必須へ降格。一方通行にしない。
```

深さ方向（経験済みパターンの自動化）は量で進む。幅方向（未経験パターンへの対応）は人間評価でしか進まない。この非対称を設計に組み込む。

### 保存対象

- 顧客質問 / 正規化された signal 集合
- 対応 actor
- tenant / project / customer context
- 対象 product / document / section
- 参照した manual evidence
- 参照した過去事例 / known_resolution
- 回答可能性判定（どの層で確定したか）
- エスカレーション判定
- 最終回答文（AI 草案 / 承認後）
- 担当者による承認 / 却下 / 修正差分
- 解決 / 未解決 / 再問い合わせ / 誤回答などの outcome
- 後続 feedback

### 想定 PunkRecord record type

- `support_case`
- `answer_attempt`
- `answer_evidence`
- `operator_feedback`
- `escalation_event`
- `escalation_rule`（第1層）
- `prohibited_domain`（第2層）
- `known_resolution`（第3層のルール。OK 回答も NG 回答も同枠）
- `audit_event`

### known_resolution が保持すべきフィールド

- **条件集合（signal 集合）**: どの signal が揃ったとき適用されるか。
- **条件具体度**: signal 集合の大きさ。照合の優先度を決める（多い方が優先）。
- **回答**: その条件下での正しい答え。OK 回答も NG 回答（廃棄案内など）も同じ枠。
- **適用条件（前提）**: 商品 / ロット / 契約 / 時期など、再利用してよい範囲。人間が記述する。
- **由来**: 誰がいつ追加したか、どの escalation 起点か。
- **承認カウント / 却下カウント / 承認者集合**: 昇格・降格の判定材料。
- **現在の格付け**: 承認必須 / 自動回答（事後監査）/ 降格中。

### 制約

- 過去回答は、適用条件・権限条件・根拠・outcome と一緒に扱う。
- 生の `answer_attempt` を学習済み正解として扱わない。人が承認した `known_resolution` のみ自動回答の根拠になる。
- 参照できる過去事例も actor の access scope で制限する。known_resolution 自体にも scope / sensitivity を持たせ、(A) の filter を通す（actor A が見られない事例由来の知識が actor A への回答に漏れない）。
- known_resolution は第1層・第2層を **バイパスできない**。照合順序がそのまま安全保証。
- ノウハウ record と audit record は相互参照可能にする。
- 回答改善に使った record を後から監査できるようにする。

## 課題 2: 権限管理

コールセンターでは、同じ担当者が複数企業の問い合わせを扱う。単一企業内でもロール・研修状態・契約・担当範囲でアクセス可能情報が異なる。

権限ごとに MCP / endpoint を分ける案は、actor × tenant × role の組合せ爆発・scope 変更ごとの再デプロイ・監査の分散を招くため採らない。**ハーネス（A）が AccessScope を一箇所で算出し query に注入する。** 柔軟性・監査一元化の両面で優れる。

### AccessScope 算出の中身（ポリシー評価器の選択）

`AccessPolicy` の評価方式に選択肢がある。

- ハードコードの Rust ロジックで始める。
- policy が増えたら宣言的ポリシーエンジン **Cedar**（Rust 製、ライブラリとして組み込める）に寄せる。Cedar は Harness 内部の評価器であり、判断主体は依然 Harness。「認可を MCP 外部の任意レイヤだけに任せる」には抵触しない。Rust 単一バイナリ方針とも整合する。
- OPA は Rego + 別プロセス常駐になり Rust 単一バイナリ方針と相性が悪いので採らない。

段階導入（ハードコード → Cedar）が現実的。最初から宣言的にしたいなら最初から Cedar でもよい。

**境界（重要）**: Cedar は `actor → scope` の認可（誰がどの tenant / label にアクセスできるか）に使う。`evidence_sufficient`（動的に取得した evidence 集合への数量・kind・しきい値の評価）は Cedar に入れない。ここは独自の typed policy + Rust 評価器が担う。権限とエスカレーションは pipeline で繋がるが、ポリシーの表現形式は別物にする。

### Harness の責務

- MCP request の認証情報から actor を確定する。
- actor の tenant / organization / role / entitlement / training status を解決する。
- project_id から利用可能な tenant / schema 候補を解決する。
- actor と project から access scope を deterministic に算出する。
- tool input の scope 関連値を信用せず、Harness 側の scope で上書きする。
- vegapunk / PunkRecord query に必ず scope filter を注入する。
- 未許可 evidence を MCP response に含めない。
- 誰が、いつ、どの scope で、何を検索し、何を回答根拠に使ったかを audit record に残す。

### 初期 model 候補

- `Actor` / `Tenant` / `Project` / `Role` / `Entitlement`
- `AccessPolicy` / `AccessScope` / `EvidenceScope`
- `Sensitivity` / `DataLabel`

`AccessScope` は少なくとも次を表現する。

- 参照可能 tenant / project / schema
- 参照可能 product / document / section / case / playbook の範囲
- 最大 sensitivity / 許可 data label
- write 可能 record type

## 課題 3: 確実なエスカレーション

> 初版はエスカレーションを「権限外・情報量不足・法務 / 個人情報」に寄せていたが、これは本質を外していた。本質は **結果の重大性** にある。

マニュアルに記載があり、アクセス権もあり、根拠レコードも揃っていても、答えてはいけない質問がある。

- 健康食品の色が変わったが食べてよいか
- 美容品で肌がピリピリするが継続してよいか
- 商品仕様だが、安全性の観点で専門家判断が要るか

これらは権限でも情報量でもなく、**誤ると健康被害になる＝個別状況への安全性判断を人間（専門家・担当）がすべき領域** だから止める。マニュアルに「常温保存可」とあっても「変色した個体を食べてよいか」は記載の射程外であり、ここを LLM が記載から類推して答えることが事故。

エスカレーションは「答えられるか否かの副産物」ではなく、**それ自体が独立した、絶対に守られるルーティング**。「この条件 → 担当 A」「この条件 → 部署 B」が先に定義され、該当したら問答無用でそこへ振る（第1層）。さらに領域単位で面を塞ぐ（第2層）。LLM がどう思おうと迂回できない。

判定の確実性は、(B) のとおり「LLM を挟まない deterministic な decision function」として担保する。エスカレーションも、第1層・第2層にマッチしたら回答生成への経路が閉じ、ルーティング先だけが返る。

### message_policy の扱い（初版からの修正）

初版は `message_policy` を Harness が固定文言で持つ設計だったが、これは過剰。Harness は **「開示してよい情報の範囲」** を返し、文面そのものは生成側（client）が作る。Harness は制約（例: 権限不足の詳細を顧客に開示しない）を返し、文面生成は分離する。

#### 追記: デモ用の返信文下書き（`customer_reply_draft`）

**上記の結論は変えない。** `disclosure_scope` が開示範囲の権威であり、文面の正本は client 側である。

そのうえで、`evaluate_answerability` は **デモ用のシミュレーション出力**として顧客向け返信文の下書きを 1 案返せる。目的は「利用者が問い合わせ本文を貼るだけで、実際どういう回答になるかを見せる」ことにある。判定結果（`escalate` 等）だけでは、チャットボットとしての最終成果物が伝わらないため。

- **権威ではない。** フィールド名・tool description・実装モジュール名すべてに `draft` を含め、「検証前の下書き」であることを型で示す。送信前の担当者確認を前提とする
- **kill switch**: `[harness] customer_reply_draft_enabled`（既定 **false**）。本番デモ構成でのみ true にする。`[llm] enabled = false` なら下書きは生成されない（起動時 warn のうえ常に `null`）
- **出口ゲート（S1-4）を必ず通す。** 生成した下書きは `egress_gate` を通し、`block` / `abstain` なら `null` にして理由を warn に残す。本 spec の「egress 位置の固定」（AI 生成 draft も人間製 outbound も同一ゲートを通す・人間製も信頼しない）は、**サーバ生成の下書きにこそ最初に適用される**。この経路を迂回させないこと
- **Escalate では内部マニュアル本文を LLM に渡さない。** `harness::reply::build_reply_brief` が Escalate のとき抜粋を必ず空にする。プロンプトで「答えるな」と指示するのではなく、**答える材料をそもそも渡さない**構造で担保する（第1層・第2層で回答生成への経路が閉じるという本 spec の判定思想を、文面生成でも再現する）
  - **保証の射程を取り違えないこと。** 構造的に保証されるのは「**内部マニュアル本文が漏れない**」ことであって、「下書きに解決方法が書かれない」ことではない。モデルの事前知識や、顧客が問い合わせ本文に書いた手順は混じりうる。後者はプロンプト指示と区切り無害化（`neutralize_delimiters`）で減らしているだけである
  - 顧客は問い合わせ本文に区切りタグを書けるため、埋め込み前に山括弧を全角へ寄せて**顧客由来の `<資料>` 偽装**を塞ぐ。これが無いと、escalate でも顧客が書いた「解決方法」を下書きに載せさせられる
- **Allowed では判定が `evidence_section_keys` に採った材料のみを渡す。** KR 由来は `evidence_section_keys` が空になるため `kr.answer`（承認済みの正本回答）を渡す。引けない場合は材料ゼロのままとし、**でっち上げた材料は渡さない**
  - **現行のいずれの `manual_schema` 経路でも `evidence_section_keys` は hits 全件と一致する。** `harness::evaluate` は `match ctx.manual_schema` の**外**で `section_hits` 全件から `section_keys` を作り、それを `decision::decide` の `best_manual_sections` として渡すため。**ManualV1 固有ではない**（`ManualSchemaKind` の `#[default]` は `LegacySection`）。したがってこの絞り込みは現状ほぼ無風で、実質「上位 `top_k` のうち先頭 3 件」が渡る。**材料の質は retrieval の順位品質に直結する**ので、順位汚染は下書きの品質にそのまま出る（実測: 型番だけ一致する無関係記事が正解記事より高スコアになる事象あり）。絞り込みの形自体は、将来 `decide` が根拠を絞るようになったときに追随させるため維持する
- 生成失敗は評価全体を失敗させない（`null` を返して warn）。下書きは付加情報であり、これが落ちて回答可否判定まで失敗するのは本末転倒

将来 client 側（チャットボットのバックエンド）が文面生成を担うようになったら、この下書きは不要になる。**その時点で kill switch を false に倒せば、本 spec の本来の構成に戻る。**

### decision の構造（修正版）

```text
AnswerDecision
  Allowed { evidence: [...] }
  Escalate {
    reason            // permission_denied / regulated-or-safety / requires_human_approval
                      // / insufficient_directness / unknown_added_signal
    layer             // 1 / 2 / 3 のどこで確定したか
    route_to          // 第1層は固有、第2層は領域の受け皿、第3層は triage
    disclosure_scope  // 顧客に開示してよい情報の範囲（文面は client が生成）
    audit_required: true
  }
```

### evidence_sufficient の定義（修正版）

初版には定義が無く、最大の曖昧点だった。修正方針:

- **数では測らない**（「根拠が 3 件ある」は救いにならない。根拠が多いほど LLM は自信を持って誤答する）。測るのは **質問への直接性**（質問の条件をマニュアル記載 / known_resolution が正面からカバーしているか）。
- bool を返さず、**何が足りないか（missing）を返す純関数** にする。

```rust
enum Sufficiency {
    Sufficient,
    Insufficient { missing: Vec<EvidenceRequirement> },
}

// 同じ入力なら必ず同じ出力。LLM 非介在。単体テスト可能。
fn evidence_sufficient(req: &EvidenceRequirement, ev: &[Evidence]) -> Sufficiency
```

`Insufficient.missing` がそのまま `Escalate.reason` と「不足根拠: ◯◯」表示に直結し、エスカレーションが説明可能になる。

### 企業ごとの既定方針（policy で切替）

- `escalate_unless_answerable`（安全厳格型）: 第1層・第2層に当たらず、第3層で直接答えられないものは triage へ。健康食品など被害が大きい業態。
- `answer_unless_blocked`（寛容型）: 第1層・第2層の明示ブロックに当たらなければ答える。社内ヘルプデスク等。

同じエンジンで両方を表現する。第3層の直接性しきい値が実用性のダイヤル（厳格 = 直接記載のみ / 寛容 = 近い記載でも可）。

## Production Tool 方針

Production tool は全て Harness 経由で実行する。tool handler が直接 vegapunk / PunkRecord を検索・更新してはいけない。

### 初期 tool 候補

- `resolve_product`
- `search_manual`
- `get_section`
- `get_product`
- `search_past_cases`
- `search_known_resolutions`
- `evaluate_answerability`（3 層判定の入口）
- `record_answer_attempt`
- `record_answer_outcome`
- `record_operator_feedback`
- `create_escalation_event`
- `add_known_resolution`（担当者の新ルール追加 = GMR の進化）

### 例: `search_manual`

```text
1. request から actor を認証する
2. project_id を解決する
3. actor + project から AccessScope を算出する（A）
4. product_key が scope 内か判定する
5. Harness が schema / labels / sensitivity / tenant filter を決定する
6. vegapunk / PunkRecord を検索する（scope filter 注入済み）
7. 未許可 evidence を除外する
8. 質問を signal 集合に正規化する（LLM 抽出、取りこぼさない側に倒す）
9. 第1層 → 第2層 → 第3層 の順に判定する（B、Rust）
10. 回答可能なら許可 evidence と audit id を、不可なら escalation decision を返す
```

## vegapunk / PunkRecord の取り扱い

PunkRecord 側の Step 1 実装方針は別紙『PunkRecord Step 1 前方互換設計』で確定済み。本書は製品（MCP）層を定め、PunkRecord への要求は同設計の範囲に従う（推測しない）。

**格納と駆動を分ける（初期方針の改訂）**:

- **格納は Step 1 からグラフネイティブ**。known_resolution を製品側 RDB や JSON 属性に畳んで持つのはアンチパターン（別紙 §7-2, 7-3）。`KnownResolution` node type + `Signal` 第一級ノード + `HAS_SIGNAL` 辺 + `BECAUSE` 辺（traceable_pairs）として PunkRecord に置く。
- **駆動（L のモチーフ昇華・Λ の判例固定・GMR の Δ抽出）は後段**。Step 1 の照合は「Signal 一致 → HAS_SIGNAL 逆辺で KnownResolution を引く」または「signal テキストの embed 近傍」の 2 経路のいずれでも動く。能力向上を担うのは人間検証ループであり、L/Λ/Δ は効率化レイヤのため後段でよい——この結論は変わらない。変わったのは「格納形式まで素朴にしない」こと。格納をグラフで持てば、後段の L/Λ/GMR は加算操作になり、データ移行が発生しない。

## 非採用

- MCP を GraphRAG の薄い wrapper として扱う。
- 認可・回答可否・エスカレーションを MCP 外部の任意レイヤだけに任せる。
- 権限ごとに MCP endpoint / MCP server を分けることを主設計にする。
- LLM に「回答可否 / エスカレーション要否 / ルーティング先」という結論を出させる（抽出は可、判断は不可）。
- client から渡された `schema` / `tenant_id` / `allowed_labels` / `sensitivity` をそのまま信用する。
- アクセスできない情報を取得してから回答生成時に隠す。
- 過去回答文を無条件に正解として再利用する。
- 第1層・第2層（禁止線）に GMR / メモ化を適用する。
- 条件が増える方向（既存ルールに未知の追加 signal が乗った質問）で既存ルールを再利用する。
- `evidence_sufficient` を「根拠の数」で判定する。
- `evidence_sufficient` を Cedar 等の認可エンジンで表現する。
- 第3層のホワイトリスト（答えてよい質問の事前列挙）を主設計にする。

## 既存サンプル実装由来として扱うもの

以下は既存実装から流用できる可能性があるが、Production CS MCP の前提ではない。採用する場合はプロダクション要件に合うよう明示的に再設計する。

- `sivira-cs-demo` という project id
- `SVR-HB100` などのサンプル商品データ
- fixture 日本語訳 / `manual.sample.json`
- `ingest_demo` / `verify_demo`
- 現行の product / document / section / spec 中心の schema
- 現行の `resolve_product` / `search_manual` / `get_section` / `get_product` の tool contract
- GCE 上の検証用 domain / deploy 設定

既存実装を参照してよいのは、構造や検証済み接続方法を確認する場合に限る。責務境界・認可・ノウハウ蓄積・エスカレーション設計は本書を正とする。

## 未決事項

- ~~認証方式: bearer token / JWT / session token / OIDC のどれを採用するか。~~ → 解決（2026-07-21）: OAuth 2.1 フェデレーション（`cs-support-mcp` が AS 兼 RS、内部で Google に委譲）。詳細は S1-11 追記および「AuthN 現状」節を参照。
- actor / tenant / role / entitlement の保存場所。
- AccessPolicy を PunkRecord に置くか、別の policy store に置くか。Cedar を入れる場合の policy 配置。
- PunkRecord の record model と vegapunk graph schema の境界。
- sensitivity / data label の分類体系。
- **signal の語彙**: どんな標準条件語（discoloration, foreign_substance, post_ingestion_symptom など）を持つか、粒度をどう決めるか。第1層ルール・第2層禁止領域・第3層 known_resolution・照合がすべてこの語彙の上に乗るため、次の最優先論点。
- known_resolution の昇格・降格しきい値（承認回数 / 却下率 / 承認者多様性の具体値）。
- 昇格 gate の運用（誰が承認するか、承認単位）。
- ~~第3層を素朴な事例で始め、いつグラフ化に移行するか~~ → 解決: 格納は Step 1 からグラフネイティブ、駆動（L/Λ/Δ抽出）のみ後段（別紙『PunkRecord Step 1 前方互換設計』）。
- escalation target の運用モデル。
- audit record の保存期間と検索権限。
- 既存実装から Production schema への migration 方針。

## 次に作るべき仕様

1. **signal の語彙仕様**（最優先。3 層と照合の共通土台）
2. `AccessScope` と `AccessPolicy` の型仕様
3. `known_resolution` の型仕様（signal 集合 / 適用条件 / 格付け / 承認カウント）
4. `AnswerDecision` / `Escalate` の構造仕様（layer / reason / disclosure_scope）
5. 第1層 `escalation_rule` / 第2層 `prohibited_domain` の宣言フォーマットと検証
6. signal 正規化（LLM 抽出）と照合（Rust 集合演算）のインターフェース
7. 昇格・降格を含む learning loop の sequence
8. 既存 tool を Harness 経由に変える実装計画

## 作業ルール

- Production CS MCP の議論・設計・実装に入る前に必ず本書を読む。
- 会話で結論が変わったら、同じターンで本書を更新する。
- 古い会話履歴より本書を優先する。
- 本書と `CLAUDE.md` が矛盾する場合は、Production CS MCP に関しては本書を優先する。
- ただし、言語、Python 禁止、Rust 優先、vegapunk API を推測しない、ローカル vegapunk を起動しない、Cloud Run デプロイ手順（`CLAUDE.md` の「Cloud Run デプロイ手順」節に従う）などの運用制約は `CLAUDE.md` に従う。

---

# Step 1 実装仕様（今回の着地）

本章は、Punk zero（予言の書・ADD）を将来フル取り込みする前提で、**今回実装する Step 1** の仕様を定める。範囲は資料の Step 1（決定論辞書＋保留＋来歴＋WORM の安全な最小 CS。暗示効能は保留へ）に対応する。

## S1-0. Step 1 のスコープ宣言

**今回作る（最小形で実装）**

- 案1 本体: 3 層判定 / known_resolution / AccessScope（Cedar 境界）/ evidence_sufficient / message_policy
- **会話層（最小）**: マルチターンの累積 signal 集合の維持・毎ターン累積集合での再判定・聞き返し。利用者が担当者でも会話は曖昧・小出しで来る前提（フェーズロードマップ章の不変条件を参照）
- 追加 4 点（いずれも最小形）:
  1. **出口ゲート**（egress gate）: 決定論 NG 辞書 + abstain。暗示効能は保留に倒す（C′ 本体は次段）
  2. **stakes 3 段離散**: low / mid / high。第2層列挙漏れ（fail-open）を高 stakes 厳格化で拾う
  3. **source_authority**: 顧客訂正は永続層に書かない（学習汚染防止）
  4. **root_cause 2 値**: retrieval_miss は new ルール化せず検索改善へ

**今回作らない（器＝フィールド／関数境界だけ予約し、中身は次段）**

- C′ 含意判定本体 / Π 連続スコア合成 / CIRG フル（route 9 値）/ L と Λ の分離処理 / 共有-規制層・集団免疫 / 成果オラクルの KPI 帰属

**リプレイス回避の三原則（Step 1 実装の絶対制約）**

1. 判断ロジック（可否・stakes・訂正分類）を tool handler に直書きしない。すべて Harness 経由の関数境界にする。
2. known_resolution は素朴な Q&A ペアで作らず、PunkRecord のグラフ構造（`KnownResolution` + `Signal` ノード + `HAS_SIGNAL` 辺）で持ち、将来の基盤フィールド（CIS 互換）を最初から空で予約する（S1-3）。製品側 RDB に置かない。
3. 4 点は関数境界（`egress_gate` / `answerability_threshold` / `correction_intake`）として切る。中身が将来 Punk zero に差し替わっても呼び出し側を変えない。

本三原則は、PunkRecord 側の 5 不変条件（別紙 §1: I1 サーバ導出 scope / I2 グラフネイティブ / I3 加算のみ / I4 材料限定 / I5 provenance キー付き WORM）と対をなす。**役割分担: 判定・強制・ゲートは MCP（Punk zero 系譜）、格納・検索・scope 経路封鎖は PunkRecord（材料）**。MCP 側に格納の再実装を、PunkRecord 側に判定を、それぞれ持ち込まない（I4）。

将来の対応（この境界が何に育つか）:

| Step 1 の最小形 | 関数境界 | 将来差し替わる基盤機能 |
|---|---|---|
| 決定論 NG 辞書 + abstain | `egress_gate()` | C′ / Ψ / 決定論辞書（最終ゲート・egress 位置固定） |
| stakes 3 段離散 | `answerability_threshold()` | Π 連続変調（β_emit 較正） |
| source_authority 3 値分岐 | `correction_intake()` | CIRG classifier の source_authority 判定 |
| root_cause 2 値 | `correction_intake()` | CIRG classifier の root_cause 判定 |

## S1-1. パイプライン（Step 1 の確定フロー）

全 tool は Harness を通る。tool handler が直接 vegapunk / PunkRecord を触らない。

```text
MCP request（認証情報付き）
 ↓
[認証] actor 確定（authn。principal 種別もここで確定。JWT Claims.sub）
 ↓
[(A) 権限] resolve_scope(actor, project) → AccessScope（サーバ導出・deterministic）
           client が渡した tenant / label / sensitivity は破棄して上書き（I1）
           AccessScope = { allowed_schemas, max_sensitivity, label_allowlist } の構造化オブジェクト
           ※ Step 1 の実効 scope は allowed_schemas のみ（tenant=schema 隔離）。
             max_sensitivity / label_allowlist は構造予約（空・判定未使用）
           ※「認可 scope」（サーバ導出・常時無条件）と「query refinement」（source_type 等・client 供給可）は別概念。混ぜない
 ↓
[取得] 認可 scope を PunkRecord 検索に無条件 AND で push-down
       （LanceDB only_if 述語＝query 時点封鎖。後フィルタではない。tenant=schema は既存強制済み）
       manual / KnownResolution / past_case を検索
 ↓
[正規化] normalize_to_signals(question) → SignalSet
         （LLM 抽出。取りこぼさない側に倒す＝疑わしい語は signal を立てる）
 ↓
[(B) 3 層判定]  ← 先に止まった層で確定、後段は評価しない
   第1層 escalation_rule 照合 → マッチ: 固有ルーティングで確定
   第2層 prohibited_domain 照合 → 触れた: 領域の受け皿へエスカレーション
   第3層 answerability
       stakes = classify_stakes(SignalSet, binding, owner)     // low/mid/high
       threshold = answerability_threshold(stakes)             // 3 段（将来 Π 連続）
       (a) known_resolution 適用条件内（Δ=なし・包含方向のみ）→ 回答候補
       (b) manual 記載が質問に直接対応（直接性 ≥ threshold）→ 回答候補
       どちらでもない（グレー）→ エスカレーション（学習の入口）
 ↓
[回答生成] 回答候補 → draft 生成（AI 草案）
 ↓
[出口ゲート] egress_gate(draft) → pass / block / abstain    ← S1-4
   AI 製・人間製を問わず egress に座る（担当者の修正文もここを通す）
 ↓
[emit or escalate] pass: 応答を返す（Step 1 の利用者は担当者）
               block/abstain/グレー: エスカレーション応答（先と理由を提示。
               未検証の AI 草案を参考として添付可 — 担当者が検証・承認すれば
               known_resolution 登録＝知識蓄積。承認は蓄積の検証手段であり、
               誤答許容の根拠ではない。誤答は利用者を問わず NG）
 ↓
[記録] answer_attempt / audit_event（WORM）を書く
```

第1層・第2層に GMR / メモ化を適用しない（毎回フル評価）。known_resolution は第1層・第2層をバイパスできない。この短絡順序自体が安全保証。

## S1-2. record type（Step 1）と格納先

付録B の 9 record type を採用し、格納先を確定する。**知識系は PunkRecord の node type（tenant schema 内・加算のみ）、監査は別建て WORM。**

| record type | 格納先 | 備考 |
|---|---|---|
| `known_resolution` | PunkRecord node `KnownResolution` | 詳細 S1-3。Signal ノード + HAS_SIGNAL 辺 |
| `escalation_rule`（第1層） | PunkRecord node `EscalationRule` | 属性: condition / owner / sensitivity / route ヒント。**判定はしない（材料のみ・I4）** |
| `prohibited_domain`（第2層） | PunkRecord node `ProhibitedDomain` | 属性: pattern / **binding**（stakes 判定が参照） |
| `support_case` / `answer_attempt` / `answer_evidence` / `operator_feedback` / `escalation_event` | PunkRecord（ingest。request_id を lineage として通す） | operator_feedback は correction_intake の feeder |
| `audit_event` | **別建て WORM ストア**（PunkRecord 無改修） | S1-8 参照。provenance キー付き構造化（I5） |

共有・規制系の知識（将来の共有-規制層行き）は tenant schema に混ぜない。`knowledge_class=regulatory` のものは別 node type / 別 schema に隔離して格納する（後で別認可ドメインに分離しても移行不要）。

## S1-3. known_resolution 型（フィールド予約込み・グラフ表現）

Step 1 で「使う」フィールドと、将来のために「予約する（空可）」フィールドを分けて定義する。予約フィールドを最初から持たせることで、将来の機能追加をデータマイグレーションでなく「空フィールドを使い始めるだけ」にする。

**グラフ表現（格納の正本）**: 論理型の `signal_set` は、PunkRecord 上では **JSON 配列属性に畳まず**、`Signal` 第一級ノード + `KnownResolution -[HAS_SIGNAL]-> Signal` 辺で持つ（I2。畳むとアンチパターン: 将来 L/Λ が辿れずデータ移行になる）。根拠は `KnownResolution -[BECAUSE]-> Rationale` 辺で結線し、traceable_pairs `{claim: KnownResolution, evidence: Rationale, edge: BECAUSE}` に追加する（将来 A / citation-at-generation が GetTraceableChain で辿る）。sensitivity / label は**ノード属性を正本**とし、ベクトル列にミラーして push-down に使う。

以下は MCP 層から見た論理型。グラフ属性とのマッピングは上記に従う。

```text
known_resolution {
  # --- Step 1 で使う ---
  id
  signal_set            : SignalSet            # 論理表現。正本は Signal ノード + HAS_SIGNAL 辺
  signal_specificity    : int                  # = |signal_set|。照合の優先度（多い＝具体的＝優先）
  applicability         : Applicability        # 商品/ロット/契約/時期など。人間が明示的に記述
  answer                : Text                 # OK 回答も NG 回答（廃棄案内等）も同枠
  scope                 : EvidenceScope        # (A) filter を通す。漏洩経路を塞ぐ
  source_authority      : {authoritative, non_authoritative}   # 昇格可否の分岐（S1-5）
  root_cause            : {knowledge_error, retrieval_miss}     # retrieval_miss は原則ここに来ない（検索改善へ）
  grade                 : {approval_required, auto_answer_audited, demoted}  # 格付け（昇格/降格）
  approval_count        : int
  rejection_count       : int
  approver_set          : [principal]          # 承認者多様性
  origin                : Origin               # どの escalation 起点か、誰がいつ

  # --- 予約（Step 1 は空/固定でよい。将来使う） ---
  binding               : {mandatory, advisory} = advisory       # 将来 Φ 拘束度。mandatory は自動校正で書けない
  registration_trigger  : {single_ruling, frequency} = single_ruling  # 将来 L/Λ 分離の器
  knowledge_class       : {regulatory, commercial} = commercial  # 将来 共有-規制層 昇格判定の器
  outcome_ref           : [KpiRef] = []        # 将来 成果オラクル帰属の器（Step 1 は空）
  # CIS 互換予約（error_axis / root_cause / owner / binding / direction / route）は
  # PunkRecord 側の KnownResolution / EscalationRule 属性としても予約済み（別紙 §3.2）。
  # 将来 CIRG の分類結果が reschema なしで書ける。
}
```

照合の駆動（Step 1 は 2 経路のどちらでも可・併用可）:

- グラフ経路: `Signal` ノード一致 → `HAS_SIGNAL` 逆辺で `KnownResolution` を引く（QueryNodes traverse）。L/Λ ネイティブ形。
- ベクトル経路: 質問 signal テキストを embed し `KnownResolution` / `Signal` の近傍取得（新オペレータ不要）。

いずれの経路でも、最終の適用可否は下の Rust 集合演算が決める（検索は候補出しにすぎない）。

照合アルゴリズム（Rust・決定論）:

```text
match_known_resolution(question_signals: SignalSet) -> Option<known_resolution>:
  candidates = known_resolution のうち signal_set ⊆ question_signals を満たすもの
    ただし question_signals に、その候補が想定しない signal が残る場合は候補から除外
      （＝未知の追加条件があれば適用不可。包含方向のみ・条件増加で再利用しない）
  if candidates 空: return None            # → 第3層(b) or エスカレーション
  return candidates を signal_specificity 降順で 1 件（最も具体的な一致＝例外ルール優先）
```

軸の分担（前段の合意）:

- 軸1（表現ゆれ「変色≒色が変わった」の吸収）＝ `normalize_to_signals`（LLM）
- 軸2（条件増減「変色≠変色＋カビ」の区別）＝ `match_known_resolution`（Rust 集合演算）

## S1-4. 出口ゲート（egress_gate）

```text
egress_gate(draft: Text, ctx: EmitContext) -> {pass | block | abstain}
```

Step 1 の中身:

- **決定論 NG 辞書**（明示 NG 語の即ブロック。最終ゲート・不変）: マッチ → `block`
- **暗示効能の扱い**: C′ 本体は未搭載。含意判定はしない。代わりに、暗示標榜のリスクがある領域（化粧品・健康食品の効果効能に触れる draft）は **答えず `abstain`（保留）に倒す**。「暗示を検出して通す」のではなく「疑わしきは出さない」。
- **egress 位置の固定**: AI 生成 draft も、担当者が修正・作文した outbound も、同一の `egress_gate` を通す（人間製も信頼しない）。
- **チャネル差**（Step 1 で分岐だけ用意）: `EmitContext` にチャネル種別 `{operator, customer_chat, customer_voice}` を最初から持たせる（Step 1 は operator 固定）。自動送信チャネル = ハードゲート（block/abstain を構造的に強制）。有人リアルタイム音声 = ゲートを座らせられないため勧告警告 + 事後 WORM/QA（`correction_intake` の feeder になる）。

将来: 中身が C′（含意 entailment 判定）+ Ψ に差し替わる。呼び出し側（パイプラインの emit 直前）は不変。

## S1-5. 訂正インテーク（correction_intake）

```text
correction_intake(event: CorrectionEvent) -> Routing
```

Step 1 で判定する軸（CIRG 6 判定のうち今回入れる 2 軸）:

- **source_authority**（最前段・決定論。principal 種別は authn 由来）:
  - `non_authoritative`（エンド顧客の「違う」）→ **永続層に書かない**。会話内修復のみ。known_resolution 昇格経路に入れない。永続昇格は「蓄積閾値 ∧ 権威確認」の二条件（Step 1 は昇格させない）。
  - `authoritative`（担当者/管理者）→ 昇格候補として次段へ。
- **root_cause**（2 値。訂正時に「正しい根拠がグラフ内に存在したか」を再検索して切り分け）:
  - `retrieval_miss`（正しい節はあったが引けなかった）→ **new ルールを作らず検索改善キューへ**（表記追加 / re-rank）。known_resolution を増やさない。
  - `knowledge_error`（正しい節が無い / 内容が誤り）→ known_resolution の追加候補（例外ルールの離散 insert）。

予約（Step 1 は判定しない・将来 CIRG フルで追加）: error_axis（内容正誤/NG/品質）、binding（mandatory/advisory）、direction、owner、route 9 値、横断昇格ゲート。`correction_intake` を単一の入口関数にしておき、将来ここに判定軸を足す（入口の位置は変えない）。

不変条件（Step 1 でも守る）:

- mandatory に触れる訂正は自動で書けない（binding=mandatory の prohibited_domain / known_resolution は人手のみ）。判定不能時は最保守（書かない / エスカレーション）。
- source_authority=non_authoritative はいかなる永続層へも書けない（最優先・例外なし）。

## S1-6. stakes 3 段（answerability_threshold）

```text
classify_stakes(signals, binding, owner) -> {low | mid | high}
  high  if binding == mandatory
        or owner == statutory
        or 決定論 NG 辞書に近接ヒット
        or prohibited_domain に近い（第2層の周辺）
  mid   if 一部の要注意 signal を含む
  low   otherwise

answerability_threshold(stakes) -> Threshold   # 3 段の階段関数
  high → 直接性しきい値を上げる（満たさねば escalate。列挙漏れの fail-open をここで拾う）
  mid  → 中間
  low  → 低い（直接記載があれば答える）
```

依存: `classify_stakes` は `prohibited_domain.binding`（mandatory 属性）を参照する。したがって Step 1 で第2層 `prohibited_domain` に `binding` フィールドを持たせることが前提（S1-2）。将来この階段関数が Π の連続変調に差し替わる。呼び出し側（第3層の可否比較）は「threshold を受け取って比較」のみで不変。

## S1-7. tool（Step 1）

付録B の 11 tool。すべて Harness 経由。

- `resolve_product` / `search_manual` / `get_section` / `get_product`
- `search_past_cases` / `search_known_resolutions`
- `evaluate_answerability`（3 層入口。stakes・threshold・egress 前判定を統括）
- `record_answer_attempt` / `record_answer_outcome`
- `record_operator_feedback`（correction_intake の feeder 入口）
- `create_escalation_event`
- `add_known_resolution`（authoritative ∧ knowledge_error のみ通す。例外ルールの離散 insert）

## S1-8. Step 1 完了の定義（Done 条件）

1. 3 層判定が短絡順序で動き、第1・2層に GMR を適用しない。
2. egress_gate が AI 製・人間製の全 outbound（自動送信チャネル）を通し、NG 辞書ヒットを block、暗示リスクを abstain にする。
3. source_authority=non_authoritative の訂正が永続層に一切書き込まれない。
4. root_cause=retrieval_miss が known_resolution を増やさず検索改善キューへ回る。
5. stakes=high の質問が、第2層列挙に無くても直接性しきい値の上昇で escalate に倒れる。
6. known_resolution が S1-3 の予約フィールドを持って生成される（binding/registration_trigger/knowledge_class/outcome_ref が空でも存在する）。
7. 判断ロジックが tool handler に直書きされておらず、egress_gate / answerability_threshold / correction_intake が関数境界として独立している。
8. 全判定が audit_event（WORM）に残り、後から監査できる。WORM イベントは provenance キー付き構造（`event_id / timestamp / request_id / schema / generation / actor / used_scope / retrieved_node_ids[] / decision / route / governing_norm_ids[]`）で記録され、opaque blob になっていない（I5）。`request_id` は PunkRecord ingest の lineage と一致する。
9. client が渡した tenant / label / sensitivity が検索に到達せず、サーバ導出の認可 scope が全 PunkRecord 検索に無条件 push-down されている（I1。後フィルタが存在しない）。
10. `KnownResolution` の signal が JSON 配列属性でなく `Signal` ノード + `HAS_SIGNAL` 辺として格納され、`BECAUSE` 辺が traceable_pairs で辿れる（I2）。

## S1-9. Step 1 で先に確定が必要な事項（実装前）

**確定済み（別紙『PunkRecord Step 1 前方互換設計』で解決）**:

- 認証: JWT `Claims { sub, role, exp, iss }` が既存。principal 識別は `Claims.sub`。scope 導出は `sub → { allowed_schemas, max_sensitivity, label_allowlist }` のサーバ側写像（Step 1 は自明写像でよい。client 入力破棄の経路を必ず通す）。
- PunkRecord ingest / query 形式: schema+generation 必須・LanceDB only_if push-down・QueryNodes 属性フィルタ・request_id lineage が既存事実として確認済み。node type 追加は加算＝reschema 不要。
- known_resolution 等の格納先: PunkRecord node type（S1-2 / S1-3）。

**確定（PunkRecord 側スコープは別紙 §4「入れる」の範囲で固定。それ以上の改修はしない）**:

- (a) **sensitivity 軸は Step 1 では使わない**。Step 1 の隔離は tenant=schema のみ。したがって vector 列 `sensitivity` / `label` の追加・ingest payload 拡張・`build_lance_filter` 拡張は Step 1 タスクに含まれない（nullable 加算なので必要になった時点で非破壊に追加できる）。AccessScope の `max_sensitivity` / `label_allowlist` は構造体フィールドとして予約のみ（Step 1 は空・判定に使わない）。
- (b) **tenant は schema で表す**（別紙推奨どおり）。既存の query 時点隔離を流用。掛け持ちは将来 Broker の scatter-gather。
- PunkRecord 側 Step 1 タスク（確定）: ①scope 導出関数 + client 入力破棄経路、②node type 加算（KnownResolution / Signal / EscalationRule / ProhibitedDomain + HAS_SIGNAL / BECAUSE + traceable_pairs、CIS 互換フィールド予約）、③（推奨）EdgeDef attributes 宣言。以上。

**残る確定事項（MCP 側）**:

- **signal 語彙の初版**（化粧品・健康食品カテゴリに限定）: discoloration / foreign_substance / odor_abnormality / post_ingestion_symptom / skin_irritation / continue_use_question / efficacy_claim（暗示効能リスク）/ dosage_for_condition など。3 層・照合・egress の全てがこの語彙に乗る。**最優先。** 語彙は `Signal` ノードの `value` として格納される。
- 決定論 NG 辞書の初期エントリ（明示 NG 語）。提携 or 自前の切り分け。
- 第2層 prohibited_domain の初期領域リストと binding 付与（`ProhibitedDomain` ノードとして格納）。

## S1-10. PunkRecord 側 Step 1 との分担（別紙対応表）

| 本書（MCP 層）の要素 | PunkRecord 側の対応（別紙） | 区分 |
|---|---|---|
| AccessScope のサーバ導出・client 破棄（S1-1） | scope 導出関数新設 + 認可 scope 無条件 push-down（§3.1・I1）。**Step 1 は tenant=schema のみ・sensitivity 列追加なし** | PunkRecord: Step 1 で作る |
| known_resolution / Signal / 第1・2層ルールの格納（S1-2, S1-3） | node type 加算: KnownResolution / Signal / EscalationRule / ProhibitedDomain + HAS_SIGNAL / BECAUSE + traceable_pairs（§3.2） | PunkRecord: Step 1 で作る |
| CIS 互換フィールド予約（S1-3, S1-5） | 同名属性を node 属性として予約（§3.2） | PunkRecord: 予約 |
| audit_event の WORM（S1-8） | 別建て WORM・provenance キー構造（§3.3・I5）。PunkRecord 無改修 | MCP 側: Step 1 で作る |
| 3 層判定 / stakes / egress_gate / correction_intake | **PunkRecord には載せない**（I4: 材料/統制の分離） | MCP 側のみ |
| エッジ属性の来歴（将来 V/Y/Z/α） | EdgeDef attributes 宣言 + エッジカテゴリ（推奨・低コスト） | PunkRecord: 推奨追加 |

実装時の禁止リストは別紙 §7（アンチパターン 8 項）に従う。特に本書に直結するのは: client scope をそのまま使う（1）/ known_resolution を RDB に置く（2）/ signal_set を JSON 配列に畳む（3）/ PunkRecord に判定を載せる（5）/ WORM を opaque blob にする（6）。

## S1-11. 追記（2026-07-03 実装計画時の確定）

S1-9「残る確定事項（MCP 側）」および未決事項のうち、次を確定した。

1. **normalize_to_signals は LLM 抽出（lexicon との和集合・lexicon フォールバック）を実装する（2026-07-13 改訂）。**
   - Step 1 時点（2026-07-03）では決定論 lexicon のみを積んでいたが、本改訂で `HybridExtractor`（`server/src/harness/extraction.rs`）を実装し S1-1 本文（LLM による signal 抽出）に準拠させた。パイプライン呼び出し側（`evaluate_answerability` / `root_cause_probe`）は `AsyncSignalExtractor` trait 経由に変わったが、判定入力の形（`SignalSet`）は不変。
   - 抽出は「lexicon.normalize()（決定論・語彙閉集合の表層一致）→ LLM が設定されていれば追加で分類を依頼 → LLM が返した signal 名を lexicon の語彙（`contains_signal`）で検証したものだけを採用 → 両者の和集合」で行う。語彙外の signal 名は破棄する（3 層判定・admission のどちらにも語彙外 signal を漏らさない）。
   - LLM のプロンプトは語彙の閉集合分類として設計され、判断に迷う・語彙で表現できない安全/契約/法務上の懸念がある発話には catch-all `unclassified_risk`（llm_only signal。lexicon には表層一致では登録せず、語彙・分類にのみ存在）を返すよう指示する（取りこぼさない側に倒す）。
   - LLM が未設定（`config.llm.enabled = false`）のときは lexicon 単独（`ExtractionMode::LexiconOnly`）。LLM 呼び出しが失敗した場合は lexicon 単独の結果にフォールバックする（`ExtractionMode::LexiconFallback`。判定は必ず何らかの signal 集合で走らせ、LLM 不達を理由に判定不能にはしない）。LLM 分類に成功した場合は `ExtractionMode::Hybrid`。
   - 決定論 lexicon は安全床・オフライン動作・API キー不要の dev 経路として残す。既知の限界（辞書外表現の取りこぼし）は、(a) 第2層照合が signal だけでなく raw text パターンにも当たること、(b) LLM 抽出が語彙内の追加 signal を拾うこと、(c) Step 1 の利用者が担当者でありグレーは必ずエスカレーションに倒れること、の 3 点で吸収する。
   - どの経路で抽出したか（`extraction_mode`: `lexicon_only` / `hybrid` / `lexicon_fallback`）は WORM 監査（`AuditDraft.extraction_mode`、加算フィールド）に記録し、`evaluate_answerability` の応答にも同値を返す（監査可能性・運用時のフォールバック頻度の可視化）。
2. **MCP の actor 認証は JWT HS256 + config actor 表とする。**（**2026-07-21 更新: 本方式は撤去済み。下記「AuthN 現状」参照。**）
   - `Authorization: Bearer <JWT>` を HS256 共有鍵（env / file から注入。設定ファイルに平文を置かない）で検証し、`Claims { sub, role, exp, iss }` を得る。
   - `Claims.sub` → config の actor 表（role / allowed_schemas）で AccessScope を自明写像する（サーバ導出・I1）。
   - JWT secret 未設定時は config の `default_actor` に明示ログ付きでフォールバックする（ローカル開発専用。GCE では secret 必須）。
3. **会話層の累積 signal 集合はサーバ側で `support_case` に永続する（2026-07-03 追加）。**
   - `evaluate_answerability` は `case_id` を受け取り（無ければ新規 `support_case` を作成）、当該 case の累積 signal 集合をサーバ側で維持する。判定は常に「累積 signal 集合 + known_resolution」に対して行う。
   - client から prior signals を受け取らない。累積集合を client 供給にすると hazard signal の欠落（改変）で誤マッチし得るため、I1 と同じ入力不信原則を会話層にも適用する。
   - 格納は `support_case -[HAS_SIGNAL]-> Signal` 辺（I2 準拠・グラフネイティブ）。
   - 聞き返し（clarification）は会話層の第一級行為として、判定結果に `clarification_allowed`（第3層 insufficient_directness / unknown_added_signal のときのみ true、第1・2層では false）を決定論で付与し、文面生成は client に委ねる（message_policy 原則）。
4. **grade（昇格・降格）は Step 1 から運用し、しきい値は config 駆動とする（2026-07-03 追加）。**
   - 昇格条件（approval_required → auto_answer_audited）: 承認回数 ≥ N ∧ 承認者多様性 ≥ M ∧ 却下率 ≤ r。降格条件: 却下数 ≥ K で approval_required へ戻す。判定は決定論の純関数 `regrade`。
   - N / M / r / K の具体値は S1-9 のとおり未決のため config（`[harness.grading]`）で注入し、初期値は N=3, M=2, r=0.2, K=2 の仮置きとする。**業務確認で確定させること。**
   - Step 1 の利用者は担当者のため、`auto_answer_audited` でも応答セマンティクスは変わらない（担当者に直接応答）。grade は Step 2 の「顧客直に即答してよいか」の判定材料として蓄積する。

### AuthN 現状（2026-07-21 更新、実測済み）

- **AuthN = OAuth 2.1 フェデレーション**。`cs-support-mcp` 自身が**認可サーバ（AS）兼リソースサーバ（RS）**として動作し、内部で Google（`accounts.google.com`）に委譲する。無トークンアクセスは `401` + `WWW-Authenticate: Bearer resource_metadata="…/.well-known/oauth-protected-resource/{project_id}/mcp"` を返し、`GET /.well-known/oauth-protected-resource/{project_id}/mcp` が `200` でリソースメタデータを返す。`authorization_servers` は **Google ではなくこのサービス自身**（`https://{public_host}`）。`/.well-known/oauth-authorization-server` は **200 が正常**（RFC 8414 AS メタデータ）。**旧記述の「404 が正常」は撤回**。
  - **AS を自前化した理由**: Google は RFC 7591 の DCR（動的クライアント登録）に対応していない。認可サーバを Google 自身にすると、claude.ai は利用者ごとに Google の Client ID / Secret を詳細設定へ手入力させる必要があり、CS 担当者に配れず、かつ Client Secret がクライアント側に置かれて秘密として成立しない。現行は Google の client_id / secret をサーバ側 env（`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` / `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET`）に閉じ込め、claude.ai は MCP endpoint の URL のみで接続できる。
  - **経路**: `claude.ai --DCR/authorize/token--> cs-support-mcp (AS) --authorize/token--> Google`。実装は `server/src/oauth/authserver.rs`（`/oauth/register` `/oauth/authorize` `/oauth/callback` `/oauth/token`）、署名・封緘基盤は `server/src/oauth/signing.rs`。
  - **トークンは自前発行しない（2026-07-22 方針転換）**: 一時期この AS は自前のアクセス/リフレッシュトークンを署名付きで発行していたが、デモという位置づけに対し寿命・失効・鍵管理を自前で抱える設計が過剰と判断され撤回した。現行は **Google が発行したトークンをそのままクライアントへ渡す**。`authorization_code` グラントは Google の `access_token` / `refresh_token` / `expires_in` をそのまま返し、`refresh_token` グラントはクライアントから来た refresh_token を **Google の token endpoint へ中継**して応答をそのまま返す（`client_secret` はサーバ側から添える）。リクエスト経路の Bearer 検証も `GoogleTokenVerifier`（tokeninfo 照会）へ戻した。AS が引き受けるのは **DCR の成立**の 1 点だけである。confused deputy 対策は redirect_uri の許可リストで行う(下記「redirect_uri の許可リスト」参照)。
  - **失効手段はこちらに無い**: 発行済みトークンの台帳も失効リストも持たないため、個別失効・一括失効のいずれもこのサーバでは行えない。失効は Google 側（アカウントのアクセス権限管理）で行う。Google 側の停止・取消は、リクエスト経路の tokeninfo 照会（キャッシュ TTL 分の遅延あり）とリフレッシュ中継の両方で拒否に変わる。
  - **署名鍵はプロセス限り（env / Secret Manager から読まない）**: 署名が必要なのは client_id（DCR 登録）・state・認可コードだけになった。改竄されると `redirect_uri` の書き換えによる認可コード横取りや、任意 identity の認可コード偽造が成立するため署名自体は残す。鍵は **起動時に OS CSPRNG から 32 バイトを生成してメモリに保持する**（`SigningKey::generate`）。env も Secret Manager も使わないため、鍵の運用作業は存在しない。
  - **再起動時の影響範囲**: 進行中のログインフロー（state / 認可コード、最長 600 秒）と DCR 登録は無効になる。DCR は claude.ai が再登録すれば自動的に回復する。**アクセストークンとリフレッシュトークンは Google 発行なので、再起動しても利用者はログアウトしない。** これが自前発行を廃止したことの直接の利点であり、鍵を使い捨てにできる根拠でもある。
  - **ステートレス設計**: Cloud Run は `maxScale=1` かつ `minScale` 未設定でゼロスケールするため、インメモリ状態はアイドルのたびに消える。外部ストア（Firestore / Redis）は追加せず、DCR 登録・state・認可コードを**署名付きの値そのものに埋め込む**。
  - **上流の秘密を運ぶブロブは暗号化する**: 認可コード（**Google の access_token と refresh_token を運ぶ**）と **state**（AS 用の PKCE verifier を運び、Google の authorize URL にクエリとして載る）は、HMAC 署名に加えて **ChaCha20-Poly1305 で封緘**する（`SigningKey::seal` / `open`）。署名だけのブロブは base64 された平文であり、通り道にいる誰でも中身を読める。認可コードはクライアントの `redirect_uri` へ**クエリ文字列として**渡る（ブラウザ履歴・Referer・中間ログに残る）ため、自前トークン発行をやめて Google のトークンを運ぶようになったことで、この経路の危険度はむしろ上がった。暗号鍵は署名鍵からドメイン分離文字列付き HMAC で派生させ、追加の env は設けない。nonce は毎回 OS CSPRNG から生成する。
  - **PKCE**: クライアント側は S256 必須（`plain` は拒否）。AS 自身も Google に対して PKCE を使う。`redirect_uri` は DCR 登録値との**完全一致**のみ許可（前方一致・部分一致は不可）。
  - **redirect_uri の許可リスト（confused deputy 対策、2026-07-22 改訂）**: `/oauth/register` は無認証で、redirect_uri を無制限に受理すると攻撃者が自分のクライアントを任意ホストの `redirect_uri` で登録し、本サービスを信頼する利用者を authorize URL へ誘導するだけで認可コードを自分側へ受け取れる（Google が見せる同意画面は本サービスに対するものであって、動的登録された下流クライアントの素性を示さない）。これを防ぐため、`is_acceptable_redirect_uri`（`server/src/oauth/authserver.rs`）が DCR 登録時点で redirect_uri をホストで絞る。`https` は `claude.ai` への**ホスト完全一致**でのみ許可し（`ends_with` のようなサフィックス一致にすると `evil-claude.ai` のような別ドメインが通るため使わない）、`http` は loopback（`localhost` / `127.0.0.1` / `[::1]`、ポートは任意）のみ許可、それ以外は拒否する。**一時期はこれを自前の同意画面（`/oauth/consent`）で塞いでいたが、許可リスト導入により配送先が閉じたため撤去した**（撤去理由・設計は `docs/superpowers/specs/2026-07-22-restrict-redirect-uri-design.md`）。`callback` は Google の identity 確定後、同意画面を経由せずその場で認可コードを発行してクライアントの `redirect_uri` へリダイレクトする。**注意**: このサーバから動的登録クライアントへのリダイレクト（2 段目）は、Google Cloud Console 側の redirect_uri 登録（`https://<host>/oauth/callback`、1 段目のみを守る）では一切守られない。守っているのは `is_acceptable_redirect_uri` のみ。
  - **ログインごとに Google の同意画面が出る（仕様）**: `prompt=consent` を常に付けるため既存同意済み利用者にも毎回出る。`access_type=offline` だけでは同意済み利用者に refresh_token が返らず、下記の失効伝播が成立しないため。運用者が「毎回出るのは故障」と誤解しないこと。
  - **上流失効の伝播とエラー分類**: `refresh_token` グラントは毎回 Google の token endpoint へ中継されるため、**Google が拒否すればこちらも拒否する**。中継に変わってもエラー分類は維持する —— fail closed の分類は **「失効したと確信できる場合だけ拒否する」**方針で決める。`invalid_grant` を返すと OAuth クライアントはリフレッシュトークンを破棄する＝利用者が再ログインを強いられるため、判断が非対称である。**`invalid_grant` を伴う 400 / 401 だけが `invalid_grant`**。**429 / 408 / 5xx / 到達不能 / 本文不正、および `invalid_client` 等を伴う 400 / 401（＝こちらの設定不備）、その他の 4xx はすべて `503`**（`Retry-After` が示されていればクライアントへ転送する。こちらでスリープして待つことはしない ―― `maxScale=1` のインスタンスを掴んだまま待つと、避けたい飽和を自分で起こすため）。**429 を失効として扱わないのが要点**で、扱うと Google の一時的な流量制限だけで利用者が一斉に再ログインさせられる。同様に `invalid_client` を失効と区別することで、`client_secret` の設定ミス 1 つで全利用者のセッションを壊さない。Google が refresh_token を返さなかった場合はクライアントにも refresh_token を渡さず access_token のみ返す（期限切れ後は再認可に落ちる）。Google が `expires_in` を返さなかった場合は短い値（600 秒）を仮定して申告する（長く仮定すると「切れているのに使い続けて 401」になるため、早めのリフレッシュに倒す）。`expires_in` は認可コードの発行から token 交換までの滞留分を差し引いてクライアントへ返す。
  - **警告: トークンに project 束縛は無い（自前トークン廃止に伴う縮退）**: 旧実装のアクセストークンは `aud` に project_id を持ち、`/{project_id}/mcp` のミドルウェアが完全一致を検証していた。Google 発行のトークンにはこちらの project_id を載せられないため、**この境界は失われている**。すなわち **ある project 向けに取得したトークンは、このサーバの全 project の endpoint で通る**。RFC 8707 の `resource` は `/oauth/authorize` と `/oauth/token` で「設定済み project を指しているか」の入力検証にしか使っていない。**`resource` 未指定時は config の project が 1 件のときのみ許し、2 件以上なら `invalid_target` で拒否する** —— したがって project を 2 件目以降に増やすと `resource` が必須になり、送らない既存クライアントは authorize に失敗する（起動時に警告ログを 1 回出す）。ただし `resource` を送ってもテナント分離にはならないため、**project を増やす前に認可境界そのものを設計し直すこと**。
  - **state も単回使用にする**: `Blob::State` は `jti` を持ち、`callback` の入口（**Google を叩く前**）で消費する。`/oauth/register` も `/oauth/authorize` も無認証なので、state が再利用可能だと、認証情報を持たない相手が有効な state を 1 つ入手するだけで `callback?state=<有効>&code=<任意>` を `STATE_TTL_SECS`（600 秒）にわたり任意レートで送り、**こちらから Google の token endpoint への外向きリクエストを無制限に発生させられる**。結果として (a) Google がこの OAuth クライアントをレート制限すれば、失効伝播の要である上流照会が他人の濫用で止まる（上流キャッシュで塞いだのと同じ状態が、クレデンシャル不要のより低いコストで成立する）、(b) `maxScale=1` かつ外向き timeout 5 秒のため、少数の並行 callback でインスタンスを飽和させ MCP endpoint ごと停止させられる。Google がエラーを返した経路でも state は消費する（フローが終了しているため再利用させる理由が無い）。
  - **単回使用の担保は best-effort**: 使用済み state・認可コードの jti 集合はプロセスメモリにのみ持つ。`maxScale=1` によりプロセス生存中の再利用は確実に弾けるが、コンテナ再起動をまたぐと弾けない。残存窓はコードの寿命 60 秒に限られ、かつコードは PKCE の `code_verifier` に束縛されるため、すり抜けても正規クライアント以外は交換できない。**使用済み記録は「記録時刻 + 固定窓」ではなく、そのブロブ自身の `exp` まで保持する**。固定窓（旧実装は `CODE_TTL_SECS` = 60 秒）だと、それより寿命の長かった旧・自前同意ブロブ（`/oauth/consent` ごと撤去済み。当時の寿命は 300 秒）が t=60..300 の窓で再利用でき、しかも消費は `action` 判定より前に行われるため**一度「拒否」した同意をブラウザバックして承認に覆せた**。ブロブ自身の `exp` を保持することで、TTL の異なる種別が増えてもこの不整合は再発しない。**OAuth 2.1 への非準拠点（意図的）**: OAuth 2.1 は認可コードの再利用を検出した際、そのコード由来の発行済みトークンを失効させることを求めるが、トークンが署名付きの自己完結値で台帳も失効リストも持たないため、**再利用を検出しても既発行トークンを取り消せない**。できるのは 2 回目以降の交換を拒否するところまでで、緩和はコードの寿命 60 秒と PKCE 束縛に限られる。**自前トークンを廃止した現在、この非準拠は Google 側の問題になっている**（発行主体が Google なので、失効も Google 側でしか行えない）。解消には外部ストアが要る。
  - **DCR は登録の合計サイズにも上限（2KB）を課す**。件数（10）と 1 件あたりの長さ（2048）だけでは、登録が client_id（base64 で約 1.35 倍）→ 封緘 state（さらに約 1.35 倍）と二重に増幅されて authorize URL に載る経路を制御できない。実測で合計 4KB の登録は authorize URL を 9399 バイトまで押し上げ、実用的な上限（おおむね 8KB）を超えて**そのクライアントのログインが恒久的に失敗する**。登録時点で弾き、原因が authorize のリダイレクト先で初めて現れる事態を避ける。
  - **キャッシュ抑止**: RFC 6749 §5.1 に従い、トークン応答・エラー応答に `Cache-Control: no-store` / `Pragma: no-cache` を付ける。
  - **クライアント指紋（`client_fp`）は発行ログにのみ残す**。「どのクライアント経由の操作か」を監査で切り分けるための短い識別子（client_id の SHA-256 先頭 16 桁。秘密ではない）。旧実装はこれをアクセストークンにも載せていたが、Google 発行のトークンには載せられないため、**現在はトークンからではなく `/oauth/token` の発行ログから辿る**。
  - **クライアント登録の期限は authorize とリフレッシュの両方で検証する**。上流トークンは不透明で期限も分からないため、こちらが課せる期限はクライアント登録の有効期限（90 日）だけになった。`authorize` にだけ課していると、登録失効後もそのクライアントが Google のリフレッシュを中継させ続けられる。
  - **`authorize` の入力検証**: `code_challenge` は S256 の形（base64url 43 文字・パディング無し）を入口で検証する。`state` は 1 KiB を上限とする（署名対象ブロブとリダイレクト URL のクエリに載るため）。
  - **DCR 登録には 90 日の有効期限がある**（`Blob::Client` の `iat` を検証）。登録簿を持たないステートレス設計では個別失効ができないため、期限で自然に切れるようにしている。claude.ai は再登録で回復する。`redirect_uris` は 10 件・1 件あたり 2048 文字、`client_name` は 256 文字を上限とする。
  - **リクエスト経路の Bearer 検証はネットワーク非依存の署名検証**に変わった（`server/src/oauth/middleware.rs`）。Google tokeninfo 照会（`GoogleTokenVerifier`）は `/oauth/callback` での identity 確定（aud 完全一致・`email_verified == "true"`・安定した `sub` の必須化）にのみ使う。
  - **この変更は AuthN の配管のみで、AuthZ は一切変わっていない**。下記の「無条件 supervisor」はそのまま残っている。
- **actor 突合のホワイトリストは廃止済み**（`server/src/harness/authn.rs`）。config `[[actors]]` による email ホワイトリスト、およびその後継として一時導入された `[default_actor]` フォールバック（commit aa9e3d8）も同じ理由で revert 済み（commit e90ef59）。config と DB の二重の正本を避けるため、config 側にホワイトリスト相当を足す実装は再度行わない。
- **`Authenticator::lookup_by_identity` は突合を一切行わず、任意の検証済み email を無条件に `Role::Supervisor` かつ config 全 project の `allowed_schemas` で `Actor` に解決する**（`server/src/harness/authn.rs`）。supervisor は `add_known_resolution` 等の権限ゲート（`Harness::admit_known_resolution`、`server/src/harness/mod.rs`）を無条件に通過する。
- **Google OAuth 同意画面は 2026-07-21 に External（本番公開）へ切替済み**。テストユーザ登録による制限は外れているため、認証到達可能な母集団は sivira.co 内部ではなく **全世界の任意の Google アカウント**である。下記の「無条件 supervisor」と組み合わせて読むこと ―― 片方だけではリスクの規模を誤る。
- **actor 突合表の DB 移行は未実装**。現状の歯止めは「Google 認証を通過したか」のみであり、実質的なアクセス制御は無い ―― 言い換えると、現状は Google アカウントで認証さえ通れば誰でも supervisor 権限の全操作（`add_known_resolution` を含む）が可能であり、実質的な認可（誰が何をできるか）は「Google 認証を通過したか」以上には絞られていない。`Authenticator::lookup_by_identity`（同ファイル doc comment に「DB 実装時の差し替え seam」と明記）を DB 参照に差し替えるまで、本番運用でのアクセス制御としては不十分と扱うこと。

### OAuth AS の残存リスク（2026-07-21、意識的に受容したもの）

以下はレビューで指摘され、**対応しないことを判断した**項目である。「気づいていない」のではなく「アプリ層では解けない、または複雑性に見合わない」と整理した結果なので、環境（前段の防御・スケール設定）を変える際は必ず読み直すこと。

- **`/oauth/authorize` は無認証・回数無制限であり、素の DoS を防げない。** `register` → `authorize` は Google を叩かずに応答するため、攻撃者は任意レートで state を発行させられる。アプリ層で消せない理由は 3 つ: (1) `maxScale=1` かつインメモリ状態はゼロスケールで消えるため、レート制限の記録が保たない、(2) Cloud Run の前段ではクライアント IP を `X-Forwarded-For` 経由でしか得られず、偽装可能な値に基づく制限は攻撃者に効かない一方で NAT 配下の正規利用者を巻き込む、(3) 上限を設ければ設けたで、それ自体が正規ログインの拒否に転化する。**対応は Cloud Armor など Cloud Run 手前の信頼できるレイヤーで行うこと。** アプリ側で担保しているのは「攻撃者のリクエスト数に対して防御側コストが線形を超えないこと」までであり、その線形性は使用済み jti 集合の上限と刈り取りの償却（`MAX_USED_JTIS` / `PRUNE_THRESHOLD`）が支えている。
- **使用済み jti 集合が満杯のときは fail closed で新規ログインを拒否する。** 単回使用を担保できない状態でブロブを通すより、その要求を拒否する方を選んだ。代償として、集合を埋められると**正規のログインを妨害できる**（可用性の低下）。ただし単回使用の回避やトークン偽造には繋がらない。可用性側の防御は上記のとおり前段の領分である。
- **上流呼び出しの single-flight（同時リクエストの集約）は実装しない。** `maxScale=1` かつ上流リフレッシュキャッシュの TTL が 60 秒で、キャッシュミスが並行して重なる窓が狭く、集約機構の複雑性に見合わないと判断した。
- **`/oauth/callback` の一時障害は再試行できない。** state は Google を叩く前に消費される（増幅対策）ため、上流が一時的に失敗すると同じ callback URL では再開できず、利用者は認可フローをやり直す必要がある。単回使用と再試行可能性を両立させるには消費済み state に対する上流交換結果の短期保存が要り、増幅を防ぐという単回使用の目的に対して複雑性が見合わない。エラー応答（`temporarily_unavailable`）の文言でやり直しが必要である旨を明示している。
- **時刻源の統一は `authserver.rs` 内に限る。** `verifier.rs` のトークン単位キャッシュは `Instant`（実時計）のままで `Clock` を注入できず、期限切れをテストで固定できない。許容理由は、`refresh_upstream` が毎回新しい Google access token を検証するため**このキャッシュは同経路では原理的にヒットしない**こと。副作用として、上流リフレッシュキャッシュの TTL 経過後のリフレッシュは Google への 2 往復（token endpoint + tokeninfo）になる。

#### 運用手順: リフレッシュが 503 を返し続けるとき

`invalid_grant` 以外の 4xx（典型的には `invalid_client`）は、利用者のセッションを壊さないために `503 temporarily_unavailable` へ倒している。この設計上、**クライアントは再認証の合図を受け取れず 503 を再試行し続ける**。自前トークンを廃止した現在、リフレッシュトークンは Google 発行で既定では期限切れしないため、**自然回復は期待できない**。したがって 503 の継続は運用者が介入すべき信号である。

1. Cloud Logging で `google refused the upstream refresh for a reason that is not a revocation` の `tracing::error!` を確認する（`oauth_error` フィールドに `invalid_client` 等が出る）。
2. **`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` / `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET` が Google Cloud Console の現行値と一致しているかを確認する。** client_secret のローテーション・失効がこの状態の最頻の原因である。
3. 一致していない場合は Secret Manager の値を更新して再デプロイする。利用者側の操作は不要で、次のリフレッシュから回復する。

### 実装状況（2026-07-04）

- MCP 側 Step 1（Harness / 3 層判定 / 会話層 / egress / correction / grade / WORM / 12 tool / 加算スキーマ / ingest_rules CLI）を実装し `main` にマージ済み。ユニットテスト全件・fmt・check 通過。codex レビュー 6 ラウンド PASS、Copilot レビュー 8 ラウンド対応済み。
- 加算スキーマ・第1層ルール 2 件・第2層領域 2 件・サンプルマニュアルを `vegapunk.local:6840`（gRPC）の `sivira-cs-demo` に投入済み（純加算）。
- **S1-8 Done 条件の実機 E2E 突合: 完了（2026-07-08）**。稼働中の vegapunk gRPC backend + JWT 認証つきローカル MCP（`127.0.0.1:3443`）に対して全項目確認:
  - 条件1 3 層短絡: 第1層（post_ingestion_symptom→safety_team / skin_irritation+continue_use_question→dermatology_liaison）・第2層（raw text「飲み合わせ」→medical_escalation_desk）・第3層で確定
  - 条件2 egress: pass / block（必ず治ります）/ abstain（症状が改善）
  - 条件3 non_authoritative 非永続: customer 訂正は conversation_only、operator_feedback ノードは operator 分のみ永続
  - 条件4 retrieval_miss: 検索改善キューへ追記され known_resolution は増えない
  - 条件5 stakes=high fail-open: 第2層に無い NG 近接語「治る」で threshold 0.95 に上がり escalate
  - 条件6 予約フィールド: KR ノードに binding/registration_trigger/knowledge_class/outcome_ref/error_axis が存在
  - 条件8 WORM: 38 イベント全行で provenance キー完備・hash chain INTACT・read tool 含む全 tool 記録・KR 由来 allowed に governing_norm_ids
  - 条件9 認証/scope: 未登録 sub / role 不一致 / 署名改竄 / 期限切れ / ヘッダ無しを各理由で拒否、tool スキーマに scope 系フィールド漏れなし
  - 条件10 グラフ格納: KR→HAS_SIGNAL→Signal、KR→BECAUSE→section、Signal 第一級ノード、KR に signal_set 属性なし（I2）
  - ロードマップ遵守事項: マルチターン累積再判定（変色→+カビで unknown_added_signal 自動エスカレーション）、grade 昇格（resolved×3・承認者 2 名→auto_answer_audited）・降格（wrong_answer×2→demoted）、GMR 進化（例外ルール「変色+カビ→廃棄」追加で具体ルール優先）
- 残: signal 語彙 / NG 辞書 / grading しきい値の業務レビューによる確定。UpsertNodes は read-merge-write 済みで merge/置換いずれのセマンティクスでも整合。

### ベクトル経路（意味検索）の解決と実測（2026-07-18）

- **A6 ブロッカー解消**: 「UpsertVectors が成功応答を返すのに Search/GetVectors から不可視」の原因は vegapunk 側の未文書化契約 — `VectorEntry.metadata` は固定列マッピングで、認識キーは `node_id` / `text` / `source_type` / `timestamp_ms` の 4 つのみ。**schema スコープは `node_id` 列への `starts_with("{schema}:gen{N}:")` で効くため `metadata.node_id`（= `id` と同一値）が必須**。ingest 側を契約準拠に修正（`vector_entry` ヘルパで `id == metadata.node_id` を構造強制）。
- 受け入れ実測（urtect 再構築後）: `GetVectors({schema:"urtect"})` に 72 vectors（ManualSection 69 + Product 3）が node_id 付きで列挙 / `Search(mode:"local")` が投入 ManualSection id を score 付きで返却。merge 等の後処理は不要（local は即時。global/コミュニティ系のみ merge 待ち）。
- 実機挙動: 意味的言い換え「カメラの映像がぼやけて鮮明ではない」→ allowed 0.75（top=画質の設定。LLM 抽出 + 検索の意味経路が語彙重なりゼロの言い換えを回答に導く）。C 群（NAS/他社カメラ/浴室）は escalate 維持。
- **キャリブレーション留意（要監視）**: backend の vector score はスケール圧縮が強い（完全一致クエリでも ~0.65、無関連でも ~0.53-0.55）。`max(text, vector)` 合成により C 群スコアが 0.53-0.55 まで上昇し、しきい値 0.6 とのマージンが薄い。現状は vector 単独で判定を覆せない安全な構成だが、コーパス拡大・backend モデル変更時は C 群相当の質問で再測定すること。必要になった場合の対策は vector score への上限係数（config 化）を予定。
- ingest 全体 63 秒（69 ページ crawl + 並行 embed 同時 4 + upsert 込み。並行化の実測確認済み）。

### 本番運用時の課題（デモでは保留）

- **actor 突合表の DB 化（B6、旧: JWT 認証の本番化）**: AuthN は Google OAuth 2.1 に移行済み（上記「AuthN 現状」参照）。残る課題は認可側で、`Authenticator::lookup_by_identity` が突合なしに任意の検証済み email を supervisor へ無条件解決する現状を、DB ベースの actor 表（sub/email → role / allowed_schemas）に差し替えること。払い出し・失効運用も本番で確定する。
- **デモ商材と signal 語彙のドメイン整合**: 現行サンプルマニュアルは `SVR-HB100`（スマートホームハブ＝電子機器）だが、signal 語彙初版は化粧品・健康食品向け。納品対象の商材を確定し、マニュアルと語彙のドメインを揃える（電子機器なら安全語彙を発熱・発火・感電系に作り直す）。

---

# フェーズロードマップ — Step 2（顧客直チャットボット）/ Step 3（音声対応）への前方互換

本章の Step 2 / Step 3 は**本製品のフェーズ番号**であり、Punk zero ADD の Step 番号とは別物。

**製品は最初から一貫して同一のチャットボット**である。フェーズは利用者と入出力形態の違いでしかない。

```text
Step 1: 担当者利用（同一チャットボットを担当者が使う。訂正＝知識蓄積を最も回しやすいフェーズ）
Step 2: 顧客直（利用者が顧客に変わる。入力一括・出力は gate 後表示）
Step 3: 音声対応（ストリーミング入出力）
```

**フェーズ不変条件（全フェーズで変えない）**:

- **誤答は利用者を問わず NG**。誤答になるかもしれない場合はエスカレーション。これは担当者相手でも顧客相手でも同じ。
- 判定の厳しさ・エスカレーション基準・egress_gate の水準はフェーズ・利用者で変えない。担当者フェーズは「誤答許容度が高い」のではなく、**authoritative な訂正（＝知識蓄積）を同一画面で最も得やすい環境**であるにすぎない。
- 担当者と顧客の差は、敵対的・無関係・曖昧入力の**発生頻度**であって、信頼境界の設計差ではない。担当者も曖昧に聞き、条件を小出しにし、誤った前提で質問する。質問入力を無条件に信頼するフェーズは存在しない（source_authority の権威性は「訂正を永続層に昇格させてよいか」の区別であり、質問入力の信頼とは別物）。

**大原則**: 判定層（3 層判定 / signal 照合 / egress_gate / 学習ループ）と会話層は Step 1 で作ったものを Step 3 まで**無改修で持ち越す**。各フェーズで足すのは入出力の実行系のみ。これを成立させるための Step 1 側の遵守事項を本章末に定める。

## Step 1 → Step 2 の差分

判定層・会話層・出口ゲートの構造差は**ない**。変わるのは次のみ。

- **利用者**: 担当者 → 顧客。EmitContext のチャネル種別が operator → customer_chat になる。
- **訂正の権威性**: 利用者の訂正が authoritative（担当者）→ non_authoritative（顧客。永続層に書けない）に変わる。知識蓄積の入口が細くなるため、蓄積は Step 1 のうちに進める。
- **性能要求**: 顧客直では応答速度が製品品質に直結する（担当者利用でも「人間とチャットしている」速度は必要であり、Step 1 で known_resolution 即答パスは詰めておく）。
- **エスカレーションの表現**: 担当者向けは「エスカレーション先と理由の提示（＋未検証の AI 草案を参考として添付可）」、顧客向けは「確認して折り返します」への変換。中身の判定は同一で、disclosure_scope の出し分けのみ。

**Step 2 移行の判断基準**: 機能の完成ではなく、**昇格済み known_resolution のカバー率**。在庫が薄いまま顧客直に出すと大半が折り返しになり製品として成立しない。

## Step 2: 顧客直（入出力方式）

入力は**確定後の一括受領**、出力は**全文 egress_gate 通過後に表示**。逐次処理系（投機的抽出・endpointing・逐次出力）は Step 2 に持ち込まず、Step 3 に隔離する。

## Step 3: 音声対応

持ち越し（無改修）: 判定層・会話層・学習ループ・egress_gate の関数境界。

Step 3 で新設するもの（逐次処理系＋実行系。既存音声エージェント製品で確立済みの手法）:

- **入力段**: streaming ASR / endpointing（発話終端検出）/ 投機的 signal 抽出（発話途中から抽出を回し、発話確定時に判定だけ確定）/ ASR 誤認識対策 — 低信頼度の危険 signal 候補は (a) signal を立てて安全側に倒す、または (b) 聞き返す。「疑わしきは signal を立てる」原則の入力段への延長であり、判定層は変わらない。
- **出力段**: egress_gate と逐次出力の両立 — known_resolution ヒット（gate 通過が事前確定）は文単位ストリーミングで即時発話。未ヒットの新規生成は**文単位で gate を通してから**発話、または abstain。
- **実行系**: ASR / LLM / TTS の co-location（ネットワーク加算遅延の排除）/ turn-taking 専用モデル（無音長でなく会話手がかりで発話終端・割り込み可否を判定）/ barge-in（発話中も聴取し、割り込みで即停止）/ 低遅延 TTS モデル + 韻律連結（チャンク間に前後テキストを渡す）。
- **目標値**: 人間の話者間ギャップは 100〜300ms、500ms 超で不自然さを知覚される。TTFB 300ms 未満を目標に置く。

## チャネル間の等価性（この分割が成立する根拠）

- **egress_gate と出力ストリーミングの非両立はチャット・音声共通**（チャットの逐次表示でも gate 前に出力が始まる）。Step 2 が一括表示を選ぶのは、この問題を Step 3 に隔離するための設計判断。
- 入力を逐次処理にした時点で、チャットと音声の差は**入力の物理特性（ASR 誤認識の有無）のみ**に縮む。判定層・会話層・出力段の課題は共通。
- **体感速度 = known_resolution カバー率**。ヒットは即答（ストリーミング可）、未ヒットは gate バッファか折り返し。レイテンシ設計と学習ループが同じ変数（昇格カバー率）に効く。

## フルリプレイス回避 — Step 1 実装への遵守事項（追加）

S1-0 の三原則に加え、Step 2 / Step 3 を無改修で載せるために Step 1 実装で守ること:

1. **会話層を Step 1 から持つ**。マルチターンの累積 signal 集合を維持し、毎ターン累積集合で判定層を再判定する（条件が増えたら——変色→変色+カビ——再判定で自動的にエスカレーションへ倒れる。照合原則の変更は不要）。聞き返し（clarification）を第一級の会話行為として持つ。判定の根拠は常に「累積 signal 集合 + known_resolution」であり、会話履歴の言質（「さっき OK と言った」）は判定入力にしない。判定層は純関数を保ち、この再判定呼び出しに耐える形にする。
2. **egress_gate の入力単位を「全文」に固定しない**。シグネチャは任意のテキスト断片（全文でも文単位でも）を受けられる形にする。Step 1・2 は全文で呼び、Step 3 は文単位で呼ぶ。中身の差し替え（辞書→C′）とは独立に、呼び出し粒度の自由を最初から確保する。
3. **known_resolution の `grade` を Step 1 から運用する**。`auto_answer_audited` への昇格・降格（S1-3）は Step 2 の「顧客直に即答してよいか」の判定にそのまま直結する。Step 1 で昇格制度を回さないと Step 2 に移行できない。
4. **応答の「折り返し変換」を decision の語彙に含める**。Escalate 時の disclosure_scope（既定）に、顧客直チャネル向けの「確認して折り返します」系の応答方針が乗ることを想定し、チャネル種別（operator / customer_chat / customer_voice）を EmitContext に最初から持たせる（Step 1 は operator 固定でよい）。
