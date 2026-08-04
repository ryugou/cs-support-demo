# Phase B2: concept-expansion（Issue #8）

| 項目 | 内容 |
|---|---|
| 目的 | Concept を介した別記事 join（R4）を、Merge の成否に依存せず成立させる |
| 読者 | 本機能の実装者と、backfill / eval を実行する運用者 |
| 正本の範囲 | 2-hop 拡張の判定規則・スコア規則・不変条件、`ManualSection.concept_keys` の書き込み規則、backfill CLI の仕様、B2 の受理条件と eval 改修 |
| 関連文書 | [`2026-07-27-phase-b-community-merge-design.md`](./2026-07-27-phase-b-community-merge-design.md)（Phase B 全体と B1 の正本）、[`2026-07-22-ingest-alarmcom-design.md`](./2026-07-22-ingest-alarmcom-design.md)（R1〜R4 の要件）、`specs/production-cs-mcp.md` |
| vegapunk 側契約の正本 | `/Users/ryugo/Developer/src/AI-Project/vegapunk/docs/specs/integration-guide.md` |

## 採用する設計

**text / vector で上位ヒットした section が持つ Concept を seed に、同じ Concept を持つ別 section を `top_k` の外側へ最大 `CONCEPT_EXPANSION_LIMIT` 件追加して返す（2-hop 拡張）。**

`top_k` の「残枠に詰める」方式は採らない。text スコアは内容語ランが 1 つでも一致すれば正の値を返すため、`score > 0` の候補が `top_k` に満たないのは内容語が corpus に無いクエリ（should-miss 相当）だけであり、残枠は事実上生まれない。残枠方式は「効いてほしい局面で発火せず、発火するのは should-miss のときだけ」という逆の分布になる。

引き当ては `ManualSection.concept_keys` への非正規化で行い、検索経路の RPC 増加をゼロにする。`MENTIONS_CONCEPT` 辺はグラフの正とし、`concept_keys` はその読み取り最適化の射影とする。両者が食い違った場合は辺を正とする。

### クエリ文字列と Concept 名を直接照合する経路は持たない

既存 text 経路（`retrieval.rs` の `content_runs` / `run_matches`）は、NFKC 正規化のうえ「カタカナ+ / 漢字+ / ASCII 英数字+」を内容語ランに分解し、ひらがな・記号・空白を区切りとして IDF 重み付きで照合する。Concept は section 本文から抽出されたものなので、その表記は本文にも出現する。したがって:

- クエリに Concept 名が助詞を挟まず現れる場合 — text 経路も当たる
- 助詞・活用が挟まる場合 — text 経路は内容語ランに分解して当たるが、連続部分一致は当たらない

**Concept 名の直接照合が当たって text 経路が当たらない状況は構造的にほぼ存在しない。** 直接照合を足しても既存経路の劣化版が本番検索に乗るだけなので採用しない。

### クエリ時の incoming traverse も持たない

`corpus.rs` が `MENTIONS_SIGNAL` を載せていないのは、hot Signal の incoming traverse が offset 0 の 1 ページ目で call timeout 120s を超え、corpus load を丸ごと落とした実測による。Concept でも汎用的なものほど fan-in が大きく、利用者が投げがちなクエリで最も遅くなる。

`concept_keys` を section 側へ非正規化すれば、seed の Concept 集合との突き合わせは corpus の全 section 走査 1 パスに畳める。**RPC は 1 本も増えない。**

## 段階分割

| 段階 | 内容 | 完了の定義 |
|---|---|---|
| **B2-0（計測）** | backfill CLI を probe モードだけ先に作り、Cloud Run job と共に本番 `urtect` へ投入して実測する | Concept 件数 / ManualSection 件数 / 1 section あたり平均 concept 数 / fan-in 分布が JSON で得られている |
| **B2-1（データ）** | schema へ `concept_keys` を追加し、`ingest_alarmcom` の書き込みと backfill 本体を実装・実行する | 本番 `urtect` の全 ManualSection に `concept_keys` が入り、`--verify` の乖離が 0 件 |
| **B2-2（検索 + eval）** | 2-hop 拡張と eval 改修を実装し、受理条件を測る | 下記「受理条件」を満たす |

B2-0 を分けるのは、**fan-in 閾値と seed 上限を実測なしに確定させないため**である。現時点で本番 `urtect` の Concept ノード件数は未計測で、閾値の根拠が無い。

B2-0 と B2-1 は同一の bin（`backfill_concept_keys`）を使う。B2-0 ではその `--probe-only` だけを実装・投入し、B2-1 で書き込み経路を足す。

## スキーマ変更（additive）

`schema/cs-support.yml` の `ManualSection` に 1 属性を足す。既存属性の型・必須性は変えない。

```yaml
      concept_keys: { type: string }   # JSON 配列文字列。既存 aliases_ja と同じ扱い
```

属性追加は世代据え置き・job 不要・既存ノード再投入不要（integration-guide §8.2）。伝播は各 CLI が起動時に `--schema-file`（既定 `../schema/cs-support.yml`）を読んで呼ぶ `VegapunkClient::create_or_update_schema` が行う。**`backfill_concept_keys` も起動時にこれを呼ぶ**（backfill が schema 追加後の最初の実行になるため）。

## 書き込み経路

### ingest_alarmcom

`ManualSectionInput` に `concept_keys: Vec<String>` を追加し、`build_section_graph` で **値があるときだけ属性を書く**（`body_original` と同じ流儀）。`ingest_urtect` は常に空を渡す。属性 Vec を後から書き換える形は取らない（重複キーを生む余地があるため）。

`MENTIONS_CONCEPT` 辺は従来どおり張る。

**再実行では既存 section に `concept_keys` は入らない。** 未変更記事は `existing_hash == hash` で skip されるため、ingest 側の変更だけでは既存データは埋まらない。したがって backfill は任意ではなく必須で、実行順序は **schema 更新 → backfill → 読み取り経路の有効化**で固定する。

### backfill CLI（`server/src/bin/backfill_concept_keys.rs`、新規）

既存 bin と同じ流儀（`clap::Parser`、各 bin に複製した `read_token`、`--endpoint` / `--schema` / `--token-file` / `--token-env`）に従う。

処理順:

1. `create_or_update_schema` を呼ぶ
2. **`query_nodes_paged`** で ManualSection を全件取得する。`query_nodes` は `offset: 0` 固定の 1 発呼び出しで backend 上限 1000 件のため使わない。取得件数は必ず summary に出す
3. 各 section について `MENTIONS_CONCEPT` を **outgoing** `traverse_neighbors_paged` する。outgoing の fan-out は 1 section あたり数件で、incoming と違い timeout しない
4. `concept_keys` を組み立て、**読み出した全属性に足して**再送する

モード:

| フラグ | 動作 |
|---|---|
| `--probe-only` | 書き込まない。ManualSection 件数・Concept 件数・1 section あたり concept 数・fan-in 分布を JSON で出す（B2-0 で使う） |
| `--verify` | 書き込まない。辺と `concept_keys` の乖離件数と該当 `section_key` を出す。定期実行で drift を検出する運用に使う |
| `--probe-one <section_key>` | 指定 1 件だけに `concept_keys` を書き、読み戻して他属性の残存を確認する。`UpsertNodes` の意味論を判定する唯一の手段（下記「安全規定」） |
| （既定） | 全件書き込む。既に同じ `concept_keys` を持つ section は skip する（冪等・RPC 削減） |

安全規定:

- **再送前に required 属性（`section_key` / `doc_key` / `title` / `body` / `source_url` / `breadcrumb` / `order` / `source_lang` / `content_hash`）が存在し、かつ空文字でないことを検査する。** 欠けた section は理由付き warn で skip し、欠落率が閾値を超えたら fail closed する（`site_skip_bail` と同じ規律）。`UpsertNodes` が全置換だった場合、読み出しで 1 属性でも欠けると本文や TOC 順が失われるうえ、`retrieval.rs` の `order` は `parse().unwrap_or(0)` なので**失敗が静かに進む**
  - `doc_key` を含めるのは、検索経路ではなく**差分 ingest と eval の母集団**がこれで絞られるため（`ingest_alarmcom` / `ingest_urtect` の既存 section 読み込みと `verify_alarmcom` が `doc_key eq` でフィルタする）。失うと当該 section が差分 ingest から見えなくなり、次回 new 扱いで再翻訳され（LLM 課金）、eval の分母からも黙って消える。`retrieval.rs` / `corpus.rs` は `doc_key` を読まないので検索自体は壊れず、**壊れたことに気づく経路が無い**
  - **存在検査だけでなく非空検査もする**のは、backend が「schema 宣言済みだが未設定」の属性を読み出しで空文字として返す場合、キーの存在検査だけではガードが常に true になり無意味化するため。上記 9 属性はいずれも `build_section_graph` が正当に空文字で書くことがない（空本文・空 title の記事は ingest 側で skip される）ので、非空を要求しても正常な section を弾かない。`section_no` は空になりうるため required に含めない
- 最初に **`--probe-one <section_key>`** を通す。`concept_keys` だけを持つ最小ノードを 1 件 upsert して読み戻し、他属性が残っているかを見る。全置換なら消えるので、**その 1 件を全属性再送で即復旧する**。復旧可能な 1 件で意味論を確かめてから全件へ進む
- `--start-after <section_key>` で再開できる。upsert は 50 件ごとにバッチし、100 件ごとに進捗をログする
- Cloud Run job の `--task-timeout` は CLI の per-request timeout より長く取る（B1 で「同着させると summary 出力前に kill される」を踏んだため）

`--probe-only` の出力形式は固定する。打ち切る場合は打ち切った旨と全件数を必ず出す（B1 の `--jobs-limit` で「打ち切りに気づけない」問題を踏んだため）。

```json
{"total_sections": 0, "sections_with_concepts": 0, "total_concepts": 0,
 "ratio_denominator": "sections_with_concepts",
 "concepts": [{"concept_key": "", "name_ja": "", "section_count": 0, "ratio": 0.0}],
 "truncated": false}
```

### 実行手段

`Dockerfile` は bin を 2 箇所（`RUN cargo build ... --bin ...` と `COPY --from=builder`）で明示列挙している。**両方に `backfill_concept_keys` を追加する。**

Cloud Run job `backfill-concept-keys` を新設し、VPC connector / service account / Secret Manager injection を `merge-schema` job と同設定にする。`CLAUDE.md` の Cloud Run 節に job 追加と tag 揃えの対象として追記する。

## 読み取り経路

### corpus（`server/src/corpus.rs`）

`manual_corpus` の戻り型を、snapshot と派生表を持つ型へ変える。

```rust
pub struct ManualCorpus {
    pub snapshot: Arc<GetGraphSnapshotResponse>,
    pub concepts: Vec<ConceptRecord>,
    pub concept_fan_in: HashMap<String, usize>, // concept_key -> その concept を持つ section 数
    pub sections_with_concepts: usize,          // fan-in の分母
}
```

**`concept_key → section_key` の逆引き表は持たない。** `HashMap<String, Vec<String>>` にすると「ここから拡張候補を引く」という誤実装を積極的に誘い、それをやると product 絞り込みを迂回して他機種専用ページが漏れる。拡張候補は必ず走査中の section 側が持つ `concept_keys` から判定する。retrieval が fan-in 表に求めるのは**件数だけ**なので、型を `usize` に縮めて誤用の余地を潰す。section 数分の文字列を保持するメモリも不要になる。

`--verify` の乖離検出は CLI 側で辺を引くため、この表を使わない。

派生表の生存期間は corpus と完全に一致するため、既存の 60 秒 TTL キャッシュに載せる。`search_with_snapshot` は `&GetGraphSnapshotResponse` を取る純関数で、ここに派生表を持たせないと**毎クエリ再計算**になる。全 caller（`ManualStore::search`、`harness/mod.rs`）に波及するが、fan-in の分母もここで一意に定まる。

`ConceptRecord` への復元は、`concept.rs` に

```rust
pub fn concept_record_from_attributes(node_type: &str, attrs: &HashMap<String, String>) -> Option<ConceptRecord>
```

を切り出し、既存の `restore_registry_from_nodes` と corpus の読み取りの両方をその上に載せる。`restore_registry_from_nodes` は `&[NodeResult]` を取るが corpus が持つのは `proto::graphrag::GraphNode` で、型が合わず「そのまま使う」ことはできない。挙動は変えないので既存テストはそのまま通る。

### 2-hop 拡張（`server/src/manual/retrieval.rs`）

`concept_expansion_enabled` が true のときだけ動く。処理順:

1. **走査**: 既存ループで各 section の `ManualHit` を作る際、`concept_keys` を併せて保持する（`(in_signal, score, ManualHit, Vec<String>)` のタプル列。現行も filter 前に全 section 分の `ManualHit` を構築しているため、この保持で増えるのは Vec 本体だけでメモリ増は実質無い）
2. **分割**: 現行の `filter(|(in_signal, score, _)| *in_signal || *score > 0.0)` を partition に変え、候補と非候補に分ける
3. **ランキング**: 候補側を現行どおり score 降順 → `section_key` 昇順で sort し、`split_off(top_k)` で直接ヒット（先頭 `top_k` 件）とあふれ分に分ける
4. **プールの定義**: **プール ＝ 直接ヒットに入らなかった全 section**（手順 2 の非候補 + 手順 3 のあふれ分）。「filter で落ちた section」だけをプールにすると、`score = 0.0` は拡張候補になれるのに `score = 0.001` で truncate からあふれた section はなれない、という逆転が起きる。プールは `manual_sections`（`product_key` 絞り込み適用済み）由来なので絞り込みが自動的に効く。別集合として snapshot から引き直すと属性抽出が二重化し、`excluded_by_product` の再適用漏れというバグ経路を作る
5. **seed の確定**: 直接ヒットのうち `score >= CONCEPT_SEED_MIN_SCORE` を満たす上位 `CONCEPT_SEED_LIMIT` 件を seed とする。下限が無いと、ランキング上位が全部 0.02 のようなクエリでもその section の concept で拡張が走り、無関係な記事を関連記事として提示する経路が開く
6. **Concept 集合**: seed の `concept_keys` の和集合を取る
7. **fan-in フィルタ**: `concept_fan_in` の値が `sections_with_concepts × CONCEPT_FAN_IN_MAX_RATIO` を**超える**Concept を除外する（ちょうどは残す）
8. **追加**: プールから「残った Concept を 1 つ以上持つ section」を、一致 Concept 数の降順 → `section_key` 昇順で最大 `CONCEPT_EXPANSION_LIMIT` 件選び、**直接ヒットの後ろに追加**する。seed 自身は拡張対象に含めない

fan-in の分母は **`sections_with_concepts`（`concept_keys` が非空の section 数）** とする。以下の 2 軸を混同しないこと。

- **product 絞り込み前 / 後**: 分母は**絞り込み前**。fan-in は Concept 固有の識別力の性質であり、クエリごとに閾値が動くのは説明できない。**拡張候補の集合（絞り込み後）とは母集団が異なる**
- **全 section / concept を持ち得る section**: 分母は**後者**。Concept 抽出は `ingest_alarmcom` にしか無く、`ingest_urtect`（Google Sites）由来 section の `concept_keys` は永久に空である。全 section を分母にすると alarm.com 側の hot concept の比率が**空 section の分だけ希釈されてフィルタをすり抜ける**

**`--probe-only` の `ratio` は retrieval と同一の分母で計算する。** 運用者はこの `ratio` を見て `CONCEPT_FAN_IN_MAX_RATIO` を決めるため、分母がずれると閾値が体系的にずれる。probe 出力に分母のキー名（`ratio_denominator`）を含めて、どちらで割ったかを明示する。

拡張候補の `score` は `CONCEPT_SCORE = 0.5`、`score_source` は `"concept"` とする。**concept は既存ランキングの score 融合には一切入らない**（`score = text_score.max(vector_score)` は現行のまま変えない）。`"concept"` は拡張候補にのみ付き、既存の 4 値写像は変更しない。

**返却契約の変更（明記必須）**: 返却件数は最大 `top_k + CONCEPT_EXPANSION_LIMIT` 件になり、配列は score 降順ではなく**「直接ヒット・ブロック（score 降順）→ 拡張候補ブロック」の 2 ブロック構成**になる（直接ヒット末尾が 0.3 で拡張候補が 0.5 という並びが正常）。`score_source == "concept"` で判別できる。この契約は `search_manual` の tool description と `SearchManualResponse` の説明の**両方**に書く。黙って件数と並びを変えるのが最悪の形。

`CONCEPT_EXPANSION_LIMIT` は暫定 **2**（コード内定数）。直接ヒットの枠を消費しないため recall@top_k への影響はゼロだが、LLM に渡る文脈が増えるため小さく始める。

`concept_keys` の属性欠落・空文字は無言で「concept 無し」として扱う（backfill 前は全 section がこの状態になるため）。不正 JSON のみ warn する（`aliases_ja` の既存規律と同じ）。

### 提示への反映

`score_source` は現状 `SectionHit::from(ManualHit)` で捨てられており、`search_manual` にも `evaluate_answerability` にも届かない。`SectionHit` に additive フィールドとして追加する。

```rust
#[serde(default)]
pub score_source: Option<String>,   // legacy 経路は None
```

`evidence_section_keys`（`harness/decision.rs`）からは **concept-only の section を除外する**。concept 経由で拾った section は回答の根拠ではなく関連記事であり、Allowed 判定の「参照した manual evidence」に混ぜると監査上の後退になる（`specs/production-cs-mcp.md` の「回答改善に使った record を後から監査できるようにする」に反する）。`search_manual` の返却には含める。

実装は `harness/mod.rs` で `section_hits` から `section_keys` を作る 1 箇所に `filter(|h| h.score_source.as_deref() != Some("concept"))` を足すだけで足りる。**`SectionHit.score_source` の追加がこの除外を可能にする唯一の手段**であり、両者は同じ変更として扱う。

**監査ログの `retrieved` からは除外しない。** `retrieved_manual_ids` は `ManualHit` 側から別途作られており、`section_keys` とは派生元も派生点も別である。「何を retrieve したか」の記録なので concept-only も含めるのが正しい。

拡張が発火した回数と、発火しなかった理由（seed が下限スコア未満 / seed に concept 無し / fan-in で全除外 / プールに一致 section 無し / kill switch off）を `tracing` に 1 クエリ 1 行で出す。これが無いと「効果ゼロ」と「一度も走っていない」を区別できない。`ingest_urtect` 由来 section が seed になった場合、拡張は黙って no-op するため、この区別は効果測定に必須である。

なお既存挙動として、`evaluate_answerability` は意図的に `product_key = None` で検索し、MCP の `search_manual` は `product_key` を尊重する。**同じクエリでも evaluate と tool で拡張の広がりが変わる**が、これは既存の非対称の踏襲であり変更しない（「evaluate と tool で hits が違う」はバグではない）。

## 不変条件

1. **拡張は直接ヒットを 1 件も変えない。** 追加は `top_k` の外側のみで、先頭 `top_k` 件の集合・順序・score は拡張の有無で完全に一致する
2. **`best_manual_score` が変化しない。** `harness/mod.rs` の `best_manual_score` は `hits.first().score` であり、上記 1 から従う。したがって**回答可否判定と 3 層判定は 2-hop 拡張の影響を受けない**
3. **`CONCEPT_SCORE` < 全 stakes 閾値の最小値、かつ < `mid`。** 既定値は low = 0.6 / mid = 0.8 / high = 0.95 で、`root_cause_probe` は `mid` と比較して `RetrievalMiss` / `KnowledgeError` を分ける。0.5 はいずれも下回る

1 と 2 は単体テストで固定する。3 は 2 段構えでテストする。

- `assert!(CONCEPT_SCORE < ThresholdsConfig::default().low)`
- `include_str!("../../config.cloudrun.toml")` を既存の config パーサに通して同じ assert をする。**本番 config の閾値を下げた瞬間に CI が落ちる形はこれだけ**で、ネットワーク非依存に書ける

加えて `main.rs` の起動時チェック群で、`low <= CONCEPT_SCORE` を検出したら `tracing::warn!` を 1 回出す（起動は止めない。閾値を下げるのは運用判断であり、起動不能にするのは過剰）。`CONCEPT_SCORE` は `manual::retrieval` に `pub const` で公開する。

## 受理条件

### eval の改修（B2-2 に含む）

現行 `verify_alarmcom` は 1 件の section の body から 1 問を生成し、その section が top-k に入るかを見る。**定義上すべての問いが単一 section で完結する**ため、別記事 join の効果を測れない。should-miss も Concept 名を含まないドメイン外クエリのみで、concept 経路は構造的に発火しない。

改修:

1. **join 指標**: 同一 `concept_key` を共有する section ペアを決定論サンプリングし、**両方の内容が必要な質問**を生成して「ペア両方が**返却全体**（直接ヒット + 拡張ブロック）に入る率」を測る。既存の `deterministic_sample` / `evenly_spaced_indices` / `find_rank` / `summarize_recall` を再利用する。section 一覧は既に `query_nodes_paged(ManualSection, doc_key = DOC_KEY)` で取っており、`concept_keys` はその属性から読めるので**追加 RPC は不要**
2. **paired 比較**: 生成質問を JSON fixture に保存し、`--questions-file` で再利用する。現状は実行のたびに Gemini で生成され seed も温度指定も無いため、before と after が別の質問集合になる
   - 形式を固定する: `{"version":1,"singles":[{"section_key","question"}],"pairs":[{"a","b","question"}],"should_miss":["..."]}`
   - **`--questions-file` 指定時はサンプリングと生成を一切行わない**（集合はファイルが正）
   - **参照先の `section_key` が corpus に無ければ fail closed** とする。黙って skip すると before と after で分母が変わり、paired 比較を導入した目的そのものが失われる（「黙って減った分母」は B1 の probe 合算で既に踏んだ誤読パターン）
3. **should-miss の追加**: 「Concept 名を含むがドメイン外」のクエリ群を足す。これが無いと concept 経路の false-positive は測れない。**このクエリ選定は B2-0 の probe 出力に依存する**（本番にどんな Concept 名があるかを知る必要があるため）。選定理由はソースコメントに残す（B1 の probe クエリと同じ流儀）

### kill switch（`concept_expansion_enabled`）

`[harness] concept_expansion_enabled`（既定 true）を config に足し、`verify_alarmcom` に `--no-concept-expansion` の上書きを付ける。既存の `vector_route_enabled` と同じ流儀に従う。

これが無いと **before / after を同一バイナリで測る手段が無く、受理条件が実行不能**になる。ペアサンプリングには `concept_keys` が必要なため、before は「B2-1 完了後・拡張コードあり」の状態で測るしかない。「before は git stash して測る」では evidence の再現性が無い。

副次的に、本番検索に載る機能の切り戻し手段を兼ねる。

### 受理の判定

| 指標 | 測る範囲 | 条件 |
|---|---|---|
| join 指標（新設） | **返却全体**（直接ヒット + 拡張ブロック） | before 比で改善していること。**改善が観測できなければ受理しない** |
| recall@5 / recall@1 | **先頭 `top_k` のみ**（拡張ブロックを含めない） | paired 比較で、同一質問の before hit → after miss が発生していないこと。拡張は先頭 `top_k` を変えないため、**理論上は完全一致**する（一致しなければ実装の不変条件違反） |
| should-miss false-positive | top1 score | 悪化していないこと（`best_manual_score` 不変により構造的に保証。実測はその検算） |

`sample_size = 100` での `recall@5 = 0.86` の 95% 信頼区間は概ね ±0.07 で、点推定に対する閾値 gate は無変更でも落ちたり通ったりする。したがって非劣化は**点推定の比較ではなく paired 差分で判定する**。

before / after は `--top-k` を同値で測る。join 指標が改善しない場合の切り分けとして、**`CONCEPT_EXPANSION_LIMIT` を一時的に広げた測定**（拡張枠が足りないのか、そもそも一致する Concept が無いのか）と、上記 tracing の発火統計（走っていないのか、走って外しているのか）を使う。

## 未決事項

| 未決 | 決定者 | 決定時期 | 影響範囲 |
|---|---|---|---|
| `CONCEPT_FAN_IN_MAX_RATIO` の値 | ryugo | B2-0 の fan-in 分布が出た時点 | 拡張候補の広さのみ。変更に再 ingest は不要（メモリ内フィルタのため） |
| `CONCEPT_SEED_LIMIT` の値 | ryugo | B2-0 で 1 section あたり平均 concept 数が出た時点 | 拡張候補の広さのみ。同上 |
| `CONCEPT_SEED_MIN_SCORE` の値（暫定 0.3） | ryugo | B2-2 で 0.2 / 0.3 / 0.4 を測った時点 | 拡張の発火条件。低すぎると無関係な記事を関連記事として提示する経路が開く |
| `UpsertNodes` の属性が部分マージか全置換か | 実機検証（`--probe-one`） | B2-1 の backfill 実行前 | 実装は常に全属性再送とするため**判明しても実装は変えない**。判定は破壊時の復旧手順を確かめる意味で行う |

`--dry-run` で `UpsertNodes` の意味論は判定できない（書き込まないため原理的に観測できない）。判定は `--probe-one` で行う。

## テスト

純関数に切り出して単体テストを置く。現行の `search_with_snapshot` は引数が多く `&self` を取り snapshot を直読みするため、この中に書くと純関数として切り出せない。

- `fn expand_by_concept(ranked: &[(ManualHit, Vec<String>)], pool: Vec<(ManualHit, Vec<String>)>, fan_in: &HashMap<String, usize>, sections_with_concepts: usize, seed_limit: usize, seed_min_score: f32, max_ratio: f32, expansion_limit: usize) -> Vec<ManualHit>` — seed 確定（下限スコア込み）・fan-in フィルタ・選抜・`score` / `score_source` の設定をここに閉じる。**拡張候補だけを返し、呼び出し側が `hits.extend(...)` する**（不変条件 1 が call site で目視でき、テストは「何が追記されたか」だけを見ればよい）。`ranked` は直接ヒットとその `concept_keys`、`pool` は直接ヒットに入らなかった section（値渡しで受けて `into_iter` で move する。借用で受けると `ManualHit` を返すのに clone が要る）
- `fn filter_by_fan_in(concepts: &HashSet<String>, fan_in: &HashMap<String, usize>, sections_with_concepts: usize, max_ratio: f32) -> HashSet<String>` — 分母と境界（`>` で除外、ちょうどは残す）をここで固定
- `fn parse_concept_keys(raw: Option<&str>) -> Vec<String>` — 属性欠落・空文字は無言で空、不正 JSON のみ warn
- `concept_record_from_attributes` — 不正 JSON のフォールバックは `aliases_ja` と同じテストで済む

既存の `score_source` 写像（`retrieval.rs` の 4 値 match）は**変更しない**。concept はランキングの融合に入らないため、この match に分岐を足す必要が無い。

固定する不変条件:

- 先頭 `top_k` 件（集合・順序・score）が拡張の有無で完全一致する（上記 1）
- `hits.first()` が拡張の有無で変わらない（上記 2）
- 拡張候補が最大 `CONCEPT_EXPANSION_LIMIT` 件で、全件 `score_source == "concept"` かつ `score == CONCEPT_SCORE`
- seed 自身が拡張候補に含まれない
- `CONCEPT_SCORE` と閾値の関係（上記 3、既定値と本番 config の 2 段）
- `normalize_key("データ・ルール") == "データルール"`（長音・中黒の扱いを固定する）

実 backfill と実検索は本番 vegapunk への到達が必要なため CI・コンテナでは実行しない。Cloud Run job の実行ログを evidence とし、未実行の検証はその旨を明記する。

## スコープ外

- `mode=hybrid` への切替と `structural_weight` の有効化（Phase C）
- should-miss 閾値そのものの較正（false-positive 率 0.15 の課題。本件とは別軸）
- Concept 抽出そのものの品質改善（`translate.rs` の抽出プロンプト）
- `ingest_urtect`（Google Sites）由来 section への Concept 付与

## 制約

- Python / TypeScript を使わない
- 認証情報をハードコードしない。bearer token はファイル / env 経由
- エラーを握りつぶさない。skip・degrade は必ず理由付きでログする
- 既存の構成・命名・型に合わせ最小差分にする
- Conventional Commits。作業ブランチは `feat/8-ingest-alarmcom`（PR #10）とし、main へ直接 push しない
