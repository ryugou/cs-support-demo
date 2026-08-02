# Phase B: Merge 配線と concept-expansion（Issue #8 Phase B）

対象: `cs-support-mcp`（このリポジトリ）
前提 spec: [`2026-07-22-ingest-alarmcom-design.md`](./2026-07-22-ingest-alarmcom-design.md) の Phase B、`specs/production-cs-mcp.md`
vegapunk 側契約の正本: `/Users/ryugo/Developer/src/AI-Project/vegapunk/docs/specs/integration-guide.md`（§5 検索 / §6 コミュニティと Merge）

## この spec のスコープ

Phase B を **B1 / B2 に分割する**。本 spec は両方の正本とする。

- **B1（実施済み）**: `Merge` RPC を配線して本番 schema に対し実行し、**global 検索が何を返すかを実測する**
- **B2（実装対象）**: `MENTIONS_CONCEPT` による Concept 跨ぎの結合（concept-expansion）を実装する

分割した理由: R4（別記事 join）の実装形態が「`search` を `mode=hybrid` に切り替えるだけ」で済むのか「自前 concept-expansion が必要」なのかは、**global の返却物を見るまで決まらない**ため。統合仕様書 §5.1 は global を「コミュニティ要約を検索し**代表メンバーを返す**」と記述しているが、`server/proto/graphrag.proto` の `SearchResultItem`（160 行目）に**メンバー一覧フィールドは無い**。現行 retrieval は node_id に ManualSection の kind marker を含むヒットだけを残す（`server/src/manual/retrieval.rs` の `search_ids_with_scores`）ため、代表メンバーが ManualSection の node_id として返るかどうかが分岐点になる。この判定は Phase C で hybrid を再検討するときにも使う。

**B1 の実測により B2 は自前 concept-expansion に確定した**（「B2: concept-expansion」節）。

## 調査で確定した事実（推測ではない）

- `MergeRequest { string schema }` → `MergeResponse {}`（空）。`server/proto/graphrag.proto:357-360`。進捗・ジョブ ID を返す口は無い
- `Merge` は **admin ロール・同期実行・schema 全体の再計算**。Node2Vec も Merge 内で同期実行（integration-guide §6.2 / §6.4）。**同一 schema で Merge は同時 1 本のみ**（衝突は `FAILED_PRECONDITION`、非リトライ）
- `SearchResponse.execution`（`SearchExecution`）に `requested_mode` / `effective_mode` / `degraded` / `degradations` / `readiness` が入る（proto 231-245 行）。**現行 `VegapunkClient::search` は `results` だけ取って `execution` を捨てている**（`server/src/vegapunk.rs:557` 付近、`mode: "local"` / `structural_weight: 0.0`）
- `hybrid` は Merge 未実行なら global 部分を吸収して local へ degrade する（integration-guide §5.1）。**落ちない**
- `structural_weight > 0` にすると score は「テキスト類似と構造類似の正規化ブレンド＝順序専用の相対値」になる（proto 164-166 行のコメント、integration-guide §5.2）
- `GetStatsResponse` に `community_count`（proto 690 行）。Merge 成否の一次証跡に使える
- `GetGraphSnapshotResponse` の `GraphNode` に `optional int32 community`（proto 830 行）。Merge 後は snapshot 経由でも community id が見える
- 本番 vegapunk（`10.10.0.2:6840`）は VPC 内部限定。`GetStats` の確認すら Cloud Run job 経由になる
- 対象 schema は **`urtect`**（`server/config.cloudrun.toml` の `[[projects]] schema = "urtect"`。`manual_schema = "manual_v1"` は schema 名ではなく retrieval 経路の種別フラグ）

## 確定した設計判断（2026-07-27、ユーザ承認済み）

| 論点 | 決定 | 理由 |
|---|---|---|
| Merge の実行経路 | **専用 CLI + 専用 Cloud Run job に分離**（`2026-07-22` spec の「`ingest_alarmcom` 末尾で呼ぶ」から変更） | Merge は schema 全体対象の同期バッチで、1 ソースの ingest とは粒度が違う。`ingest_alarmcom` は実測 約6時間かかっており、その末尾に同期 Merge を積むと job タイムアウトの危険度と再実行コストが跳ね上がる（Merge だけやり直したいのに 6h のクロールから走り直しになる）。integration-guide §6.4 も「バッチ投入の後にまとめて Merge」を挙げている |
| hybrid 化の踏み込み | **`mode=hybrid` のみ。`structural_weight` は 0.0 据え置き** | `structural_weight` を上げると score の意味が順序専用の相対値に変わり、現行 answerability 閾値判定の前提が崩れる。未解決の should-miss false-positive 率 0.15 と同時に動かすと原因の切り分けが不可能になる |
| R4 の進め方 | **実測ファースト**（B1 / B2 分割） | 上記「この spec のスコープ」参照 |

## B1 の成果物

### 1. `VegapunkClient` の追加（`server/src/vegapunk.rs`）

- `merge(&self, schema: &str) -> Result<()>`
  - `MergeRequest { schema }` を呼ぶ
  - **エラーを握りつぶさない**。gRPC の `Code` を運用者が次のアクションを判断できる文言に写像する:
    - `FAILED_PRECONDITION` → 「同一 schema で Merge が既に実行中、または前提未達（embedding/LLM 設定等）。実行中なら完了を待つ。リトライしても解決しない」
    - `PERMISSION_DENIED` → 「Merge は admin ロール必須。使用中の bearer token の権限を確認する」
    - `DEADLINE_EXCEEDED` → 「`--timeout-secs` を引き上げる。Merge 自体はサーバ側で継続している可能性があるため、再実行前に `GetStats.community_count` を確認する」
    - その他 → 元の `Status` を保ったまま context を付けて返す
- `stats(&self, schema: &str) -> Result<GetStatsResponse>`
  - `GetStatsRequest { schema, node_type: None, filters: [] }`
- `search` の拡張
  - 戻り値に `SearchExecution` を含める（例: `pub struct SearchOutcome { pub results: Vec<SearchResultItem>, pub execution: Option<SearchExecution> }`）
  - `mode` を引数化する（既定は現行どおり `local`。**B1 では retrieval 経路の mode を変えない**）
  - `execution.degraded == true` のとき `tracing::warn!` で `requested_mode` / `effective_mode` / 各 `degradation`（component・reason・message）を出す。現状は静かに degrade して気づけない
  - 既存呼び出し元（`manual/retrieval.rs`、`bin/verify_demo.rs`、`harness/mod.rs` 経由）は `results` しか使わないので、**互換ヘルパを残して最小差分**にする

### 2. 新 bin `server/src/bin/merge_schema.rs`

Merge の実行と、**その前後の観測**を担う。

- 引数（既存 ingest / verify CLI の流儀に合わせる）:
  - `--endpoint`（env `VEGAPUNK_ENDPOINT`、既定 `http://vegapunk.local:6840`）
  - `--schema`（既定 `urtect`）
  - `--token-file`（主経路）/ `--token-env`（フォールバック）
  - `--timeout-secs`（既定 `21600` = 6h）
  - `--probe-only`（Merge を実行せず観測だけ行う。実行前の状態確認と、接続断後に Merge がサーバ側で継続していないかの確認に使う）
- 実行順:
  1. `stats(before)` をログ（`node_count` / `edge_count` / `vector_count` / **`community_count`**）
  2. **probe(before)**
  3. `Merge`（開始・終了時刻と所要時間を計測してログ）
  4. `stats(after)`
  5. **probe(after)**
  6. before / after を並べたサマリを 1 箇所に出す（community_count の増分、probe の分類件数の差）
- **probe の定義**（hybrid 切替の可否を決める実測）:
  - 固定の日本語クエリを数件（マニュアル横断で答えが散っていそうな問い合わせ文。ソースにコメントで選定理由を残す）
  - 各クエリを `mode=local` / `mode=hybrid` / `mode=global` の 3 つで叩く（`local` は Merge の影響を受けない**基準線**。hybrid の変化が Merge 由来か probe の揺らぎかを切り分けるために要る）
  - **前後の差分は `(query, mode)` ペア単位で取り、mode 別に出す**。Merge 前の `global` は正常に失敗するため、mode を横断合算すると「Merge の効果」ではなく「probe が何本通ったか」を映した値になる。比較可能ペアが無い mode は `null`、各 mode の delta には母数 `compared_pairs` を併記する。probe の合算値は母数が読み取れるキー名（`totals_of_succeeded_probes`）で出す
  - 返った `SearchResultItem` を **`type` / node_id の kind marker / score** で分類して JSON 出力する。少なくとも「ManualSection の node_id を持つ件数」「それ以外（community summary 等）の件数と、その `type` と id の形」が読み取れること
  - `SearchExecution`（requested / effective / degraded / degradations / readiness）も併せて出す
  - `mode=global` は Merge 前に `FAILED_PRECONDITION` を返すのが正常。**probe はこれで落ちず、記録して次のクエリへ進む**（Merge 前後の差分を取るのが目的のため）
- チャネル timeout: 既定 `GrpcLimits.timeout_secs = 120` では Merge に足りない。**この bin だけ `--timeout-secs` で `GrpcLimits` を張り替える**
- 失敗時は非 0 終了（fail closed）

`verify_alarmcom` は eval 専用のまま**触らない**。Merge 前後の retrieval 品質比較は B2 で既存のまま回す。

### 3. Cloud Run job `merge-schema` の新設

- service / 既存 job と**同一イメージ・同一 tag**
- `--task-timeout` を Merge の想定所要時間より十分長く取る
- VPC connector / service account / Secret Manager injection は既存 `ingest-alarmcom` job と同設定
- `CLAUDE.md` の Cloud Run 節に job 追加と再デプロイ時の tag 揃えを追記する

## B1 実行で判明した事実（2026-07-27 実測。以後の運用判断はこれを前提にする）

### h2 keepalive が正常な Merge を 40 秒で殺す

初回の実 Merge（`merge-schema-k2pc4`）は **40 秒**で `status: Unavailable, message: "http2 error" ... keep-alive timed out` で落ちた。40 秒 = `http2_keep_alive_interval(30s)` + `keep_alive_timeout(10s)`。**Merge の失敗ではなくクライアント側の切断**で、Merge は同期実行のためサーバが h2 PING に応答できないことが原因。

対処: `GrpcLimits` に keepalive の調整口を足し、`merge_schema` CLI だけ h2 keepalive を無効化した。常駐サーバの keepalive（アイドル後の死んだ接続を掴む事象への対策）は変えない。トレードオフとして、接続が本当に死んだ場合の検知は TCP keepalive（Linux 既定で約 12 分）と per-request timeout（6h）だけになる。

### クライアントが切れてもサーバ側 Merge は継続する

切断の 18 分後に `--probe-only` で観測したところ、`community_count` は **0 → 198**、`node_count` 5971 → 6280、`edge_count` 18549 → 24638 に増えていた。**Merge の再実行は不要**で、走行中の再実行は `FAILED_PRECONDITION`（同時 1 本）になる。接続断を見たら必ず `--probe-only` で `community_count` を確認してから判断する。

### Leiden クラスタリングと CommunitySummary は別タイミングで揃う

`community_count = 198` になった時点でも `global` はまだ `FAILED_PRECONDITION: No community summaries found` を返し、`readiness.global` / `community_summary` は `NOT_READY` のままだった。**`community_count > 0` は「global が使える」ことを意味しない。** global の可否は `readiness` で判断する。

### Merge が完了しない真因: node2vec ジョブが vegapunk 側のジョブタイムアウトを超える

`ListJobs` 診断（`merge-schema-rhnxw`）で取得した `JobInfo.error`:

| job_type | error | status |
|---|---|---|
| **node2vec** ×2 | **`job timed out (exceeded job_timeout_secs)`** | failed（`retry_count = 3`） |
| community_summary | `unavailable: Gemini API request failed: error sending request` | completed（retry 後に成功） |
| search_evaluation | `storage error: CozoDB script error: database is locked (code 5)` / Gemini エラー | failed / completed |

node2vec ジョブは `created_at` 1785141057698 → `completed_at` 1785142826363 ＝ **約 29.5 分**走ってからタイムアウト判定されている。vegapunk の `worker.job_timeout_secs` の既定は **300 秒**（integration-guide §11）。3 回リトライして同じ理由で失敗し、**Merge 全体が abort** → CommunitySummary 未生成 → `global` 使用不可、という連鎖。

**cs-support 側には打つ手が無い。** Merge の構成要素（Leiden / CommunitySummary / Node2Vec）は選択できず、ジョブのタイムアウトはサーバ設定である。解消には vegapunk 側で次が要る:

- `worker.job_timeout_secs` を実測（約 30 分）より十分大きく上げる。**env 上書きのホワイトリストに含まれる**（integration-guide §11）ので `config.yml` の編集なしで変えられるが、config はホットリロードされないため**プロセス再起動が必要**
- `worker.job_ttl_secs`（既定 3600 = 1 時間、「ジョブ終端の主条件」）も併せて確認する。node2vec が 1 回 30 分 × リトライで TTL を超えると、timeout を上げても TTL 側で終端する。**こちらは env ホワイトリストに無く `config.yml` 直接編集が必要**

副次的に判明した 2 点（Merge の blocker ではないが記録する）:

- vegapunk の LLM バックエンド（Gemini）が断続的に `error sending request` を返している。community_summary / search_evaluation がリトライで吸収しているが、Merge の所要時間を押し上げる
- `search_evaluation` が `CozoDB: database is locked` で失敗している。Search の副作用として非同期 enqueue される検索品質採点ジョブ（integration-guide §5.4）で、書き込み競合が起きている

### Merge 実行中は本番検索のレイテンシが悪化する

probe 1 件あたりの所要が Merge 前 約 5 秒 → Merge 走行中 30〜60 秒に伸びた。`search_manual` は Merge 中も落ちないが遅くなる。**Merge を定期実行する運用にするなら、業務時間外に回すか、遅延を許容できるかを先に決める。**

## B1 実測 JSON の判読手順（この順で読む。順序を守らないと誤読する）

0. **`merge.status` が `failed` なら、まず `recent_jobs.jobs[].error` を読む**。Merge の abort 理由（例: `node2vec ジョブが terminal failure state`）だけでは何が起きたか分からず、具体的な失敗内容は `ListJobs` の `JobInfo.error` にしか入らない。`recent_jobs` は 3 形を取る: 診断を取らなかった（Merge 成功パス）＝ `null` / 取得失敗＝ `{"jobs": [], "error": "...", "total_count": null, "since_ms": N}` / 成功＝ `{"jobs": [...], "error": null, "total_count": N, "since_ms": N}`
   - `--jobs-limit`（既定 50、上限 500）で打ち切られたかは `total_count` と `jobs` の長さを比べる。**時間窓 `--jobs-since-hours` を狭めても打ち切りは減らない**（`ListJobs` は `created_at DESC` でソートしてから `limit` を適用するため、押し出すのは新しいジョブだけ）
   - **インシデントから 24 時間以上経っている場合は `--jobs-since-hours 0`** を付ける。既定のままだと目的のジョブが窓外に落ち、`jobs` が空なのを「失敗ジョブは無い」と誤読する
   - `ListJobs` は **cross-schema**（proto に schema 絞り込みが無い）。他 schema のジョブの `error` 文字列がこの JSON と Cloud Run ログに混ざる。意図的に受容している副作用なので、**summary JSON をそのまま外部へ共有しない**
1. `summary.verdict` と `verdict_code` を見る。`fatal` なら以降の数値は信用しない
2. `stats_before.community_count` → `stats_after.community_count` と `summary.community_count_delta` を見る。Merge が実際に何を作ったかの一次証跡
3. **`probe_after.entries[].execution` の `degraded` / `effective_mode` / `readiness.global` / `readiness.community_summary` を先に確認する**。`hybrid` は Merge 未実行でも local へ degrade して「成功」扱いになるため、after 側 hybrid が degrade したままだと `probe_counts_delta.hybrid` はほぼ 0 になる。これを「community item に top_k を食われない ＝ hybrid 切替は安全」と読むのが**最も危険な誤読**（真相は「hybrid が一度も本来の形で動いていない」）
4. そのうえで `summary.probe_counts_delta` を mode 別に読む。`local` は基準線（Merge 非感受）、`hybrid` が本命、`global` は初回実行では `null`（Merge 前に比較可能ペアが無いため）
5. hybrid 切替の可否は `probe_after.entries[].samples[].id` を見て決める。ManualSection の node_id が返っているなら hybrid 切替だけで別記事 join が成立する（Phase C で扱う）。**B1 では `readiness.global` が READY にならず、この判定自体が未実施**

### merge_schema の未実装事項（Merge の再実行前に入れる。B1 の実行結果には影響しない）

- 各 mode の delta に `degraded_pairs`（片側でも degraded だったペア数）を併記し、上記 3 の確認を summary だけで完結できるようにする
- `probe_grid_and_mode_list_stay_aligned` テストが恒真式（`(a*b) % b == 0`）で、probe ループの順序変更を検出できない。グリッド生成を純関数に切り出して index → mode の写像を assert する
- 比較可能ペアが全 mode で 0 のとき `probe_counts_delta` 全体が `null` になり mode キーが消える。「測っていない」と「キー名を間違えた」を区別させる方針と逆なので、`{"local":null,...}` を返す形に揃える

## B2: concept-expansion

### 採用する設計

Concept → section の引き当てを **ManualSection への非正規化**で行い、検索経路の RPC 増加をゼロにする。`MENTIONS_CONCEPT` 辺はグラフの正として残し、`ManualSection.concept_keys` はその読み取り最適化の射影とする。**両者が食い違った場合は辺を正とする。**

クエリ時に一致 Concept を incoming traverse する案は採らない。汎用的な Concept ほど fan-in が大きく、**利用者が投げがちなクエリで最も遅くなる**。hot Signal の incoming traverse が 1 ページ目で call timeout 120s を超え、corpus load を丸ごと落とした事象（`server/src/corpus.rs:129-145` で MENTIONS_SIGNAL を載せていない理由）と同じ失敗構造になる。加えて fan-in を知るには traverse するしかないため、識別力の無い Concept を候補から落とす判定が原理的にできない。

### スキーマ変更（additive）

`schema/cs-support.yml` の `ManualSection` に 1 属性を足す。

```yaml
      concept_keys: { type: string }   # JSON 配列文字列。既存 aliases_ja と同じ扱い
```

属性追加は世代据え置き・job 不要・既存ノード再投入不要（integration-guide §8.2）。全 ingest CLI が起動時に呼ぶ `ensure_schema` で伝播する。

### 書き込み経路

1. **`ingest_alarmcom.rs`**: セクションごとに集めた `concept_keys`（`:832` で重複排除済み）を JSON 配列文字列にして section ノード属性へ載せる。`MENTIONS_CONCEPT` 辺は従来どおり張る
2. **`server/src/bin/backfill_concept_keys.rs`（新規）**: 既存データ用。ManualSection 全件を `query_nodes` で取得 → 各件について `MENTIONS_CONCEPT` を **outgoing** `traverse_neighbors_paged` → `concept_keys` を組み立てて `upsert_nodes`
   - **読み出した全属性に `concept_keys` を足して再送する**。`UpsertNodes` が部分マージか全置換かは未確定（下記「未決事項」）で、全属性再送はどちらの意味論でも安全側に倒れる
   - 既に同じ `concept_keys` を持つ section は skip する（冪等・RPC 削減）。`--dry-run` で upsert せず件数と差分だけ出す
   - concept ごとの fan-in 分布を stdout に JSON で出す（下記 fan-in 閾値の決定材料）
   - `ingest_alarmcom` の 6h クロールとは独立に単体で再実行できる。既存 bin と同じく `clap::Parser` と複製した `read_token` を使う

outgoing 方向の fan-out は 1 section あたり数件で、incoming と違い timeout しない。

### 読み取り経路（`server/src/manual/retrieval.rs`）

1. `corpus.rs` の `load_manual_corpus` に `load_nodes(schema, KIND_CONCEPT)` を 1 回追加する（低カーディナリティ。既存 TTL 60s に乗る）。in-memory 表現への復元は既存の `concept::restore_registry_from_nodes` をそのまま使う
2. 正規化したクエリに Concept の `name_ja` / `aliases_ja` / `name_en` が部分一致するかで、一致 Concept 集合を作る
3. **fan-in フィルタ**: corpus 上の `concept_keys` から Concept ごとの出現 section 数を数え、**全 ManualSection の 5% を超える Concept を候補から除外**する
4. 一致 Concept を `concept_keys` に含む section へ `CONCEPT_SCORE = 0.5` を与え、`score = text_score.max(vector_score).max(concept_score)` とする
5. `score_source` に `"concept"` を追加する（concept が最終スコアを供給し、text も vector もそれに届かなかった場合）

### 不変条件

**`CONCEPT_SCORE`(0.5) < answerability の low 閾値**。concept 経路だけで拾った section は、単独では絶対に「回答可能」へ転ばない。既定値（`server/config.cloudrun.toml` の low = 0.6）に対して単体テストで固定する。

閾値は config で変更できるため、起動時に `low 閾値 <= CONCEPT_SCORE` を検出したら `tracing::warn!` で「concept 経路単独で回答可能へ転ぶ設定である」と 1 回警告する（起動は止めない。閾値を下げるのは運用判断であり、起動不能にするのは過剰）。

### 受理条件

`verify_alarmcom` を before / after で実行し、**recall@5 ≥ 0.86 / recall@1 ≥ 0.48 / should-miss false-positive ≤ 0.15**（いずれも現状値）を満たすこと。false-positive 率が悪化していないことは、上記の不変条件が構造的に保証する部分と実測の両方で確認する。

併せて `server/src/manual/concept.rs:3` のドックコメント（`translate::translate_and_extract` を「現時点は stub」と書いている）を実態に合わせて修正する。実際には Gemini 実装が `ingest_alarmcom.rs:788` から呼ばれている。

### 未決事項

| 未決 | 決定者 | 決定時期 | 影響範囲 |
|---|---|---|---|
| `UpsertNodes` の属性が部分マージか全置換か | 実機検証（backfill CLI の `--dry-run`） | backfill CLI 初回実行時 | **実装変更は不要**。全属性再送で両方に対応済みのため、判明しても upsert ペイロードが縮むだけ |
| fan-in 閾値 5% の妥当性 | ryugo | backfill CLI が fan-in 分布を出力した時点 | 検索候補の広さのみ。閾値変更に再 ingest は不要（メモリ内フィルタのため） |

本番 `urtect` の Concept ノード件数は未計測である。5% は暫定値で、根拠は分布を見るまで無い。

### テスト

ネットワーク非依存の純関数に単体テストを置く。

- `concept_keys` の JSON シリアライズ / デシリアライズ（不正 JSON は空配列へフォールバックし warn する。`restore_registry_from_nodes` の `aliases_ja` と同じ規律）
- クエリ → 一致 Concept 集合（正規化、部分一致、`aliases_ja` 経由の一致）
- fan-in フィルタ（閾値の境界を含む）
- スコア融合と `score_source` の写像（concept が最終スコアを供給するケース、text / vector が上回るケース）
- `CONCEPT_SCORE < low 閾値` の不変条件

実 backfill と実検索は本番 vegapunk への到達が必要なため CI・コンテナでは実行しない。backfill CLI の実行ログを evidence とし、未実行の検証はその旨を明記する。

## スコープ外

- `mode=hybrid` への切替（Phase C）。B1 で `readiness.global` が READY にならず判定材料が取れていない。vegapunk 側の設定変更後に Merge が成功したら再判定する。concept-expansion とは排他ではなく重ねられる
- `structural_weight` の有効化と閾値の再較正（Phase C）
- should-miss 閾値の較正（false-positive 率 0.15 の課題。本件とは別軸）
- Merge の定期実行自動化（当面は job の手動実行）
- 10 万ノード超のスケール最適化

## リスクと前提

- Merge の所要時間は**まだ確定していない**。B1 では node2vec が約 29.5 分でタイムアウトして abort したため、完走時の総所要は未計測である。vegapunk 側の設定変更後の再実行で初めて測れる。運用ルール（いつ回すか）はその実測後に決める
- 切替順序は「Merge 完了 → hybrid 化」で安全側。hybrid は Merge 未実行でも local に degrade するだけで落ちない
- auto_merge（`worker.auto_merge_enabled`）は `Ingest` 経路のみに効き、低レベル Upsert 経路の本プロダクトには効いていない。**B1 で `stats(before).community_count = 0` を実測して裏取り済み**
- Merge は `SetMaintenanceMode` ON でも通り得る（integration-guide §12）。メンテナンス中でも実行可能だが、逆に「メンテだから止まっているはず」と期待しないこと

## B1 のテスト

ネットワーク非依存の純関数に単体テストを置く:

- node_id → kind 分類（probe の集計ロジック）
- probe 出力の整形（JSON 構造）
- gRPC `Code` → 運用者向けメッセージの写像
- CLI 引数のパース（既存 CLI と同じ流儀の範囲で）

実 Merge / 実 probe は本番 vegapunk への到達が必要なため CI・コンテナでは実行しない。**Cloud Run job `merge-schema` の実行ログを evidence とし、未実行の検証はその旨を明記する**。

## 検証コマンド（evidence として最終出力に含める）

- `cargo fmt --manifest-path server/Cargo.toml -- --check`
- `cargo check --manifest-path server/Cargo.toml --all-targets`
- `cargo test --manifest-path server/Cargo.toml`

## 制約

- Python / TypeScript を使わない
- 認証情報をハードコードしない。bearer token はファイル / env 経由
- エラーを握りつぶさない。skip・degrade は必ず理由付きでログする
- 既存の構成・命名・型に合わせ最小差分。新 bin は既存 `ingest_*` / `verify_*` CLI の引数・token 解決の流儀を踏襲する
- B1 ではスキーマを変更せず、グラフも書き換えない（Merge はサーバ側の派生データを作るだけ）。B2 のスキーマ変更は `ManualSection.concept_keys` の追加 1 件のみとし、既存属性の型・必須性は変えない
- Conventional Commits。commit は可、push・PR 作成は指示があるまで禁止
