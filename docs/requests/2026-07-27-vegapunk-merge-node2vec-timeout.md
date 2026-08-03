# vegapunk への依頼: Merge が node2vec ジョブのタイムアウトで完遂できない

- 起票日: 2026-07-27
- 依頼元: cs-support-mcp（AI-x-EC / cs-support-demo リポジトリ）
- 対象 schema: `urtect`（generation `gen1`）
- 対象インスタンス: GCP VPC 内 `10.10.0.2:6840`（punkrecord-egghead）

---

## 0. 要約（先に結論）

`Merge` RPC を実行すると、**node2vec ジョブが `job timed out (exceeded job_timeout_secs)` で失敗し、Merge 全体が abort します**。2 回実行して 2 回とも同じ結果でした。

その結果 CommunitySummary が commit されず、`Search(mode="global")` は `FAILED_PRECONDITION: No community summaries found. Run Merge RPC to enable global search.` を返し続けます。`readiness.global` / `readiness.community_summary` は `NOT_READY`（reason: `merge has not completed for this schema`）のままです。

**クライアント側でできることは無く**（Merge の構成要素は選べず、ジョブのタイムアウトはサーバ設定のため）、vegapunk 側での対応をお願いしたい、というのがこの依頼です。

お願いしたいことは大きく 3 つです。

1. **`worker.job_timeout_secs` の現行値の確認と引き上げ**（+ プロセス再起動）。あわせて `worker.job_ttl_secs` の妥当性確認
2. **node2vec の所要時間そのものの妥当性確認**（6,367 ノード / 30,662 エッジで 1 ジョブ 30〜53 分は想定内か）
3. **Merge の部分失敗設計についての相談**（node2vec だけ失敗したときに Leiden + CommunitySummary の成果まで無効化される挙動は意図どおりか）

---

## 1. 背景: cs-support-mcp が Merge に何を期待しているか

cs-support-mcp は、CS 担当者向けの日本語マニュアル検索 MCP サーバです。英語マニュアル（Google Sites / answers.alarm.com）を機械翻訳して文書グラフとして vegapunk に投入し、日本語クエリで検索しています。

- 投入は **低レベル Graph API**（`UpsertNodes` / `UpsertEdges` / `UpsertVectors`）を使っています。`section_key` / `en_hash` / `translation_status` を決定論的に保持する必要があるため、高レベル `Ingest` の LLM 抽出は使っていません
- ノード型: `ManualDocument` / `ManualSection` / `Product` / `Signal` / `Concept`
- エッジ型: `HAS_SECTION` / `PARENT_OF` / `DESCRIBES` / `MENTIONS_SIGNAL` / `MENTIONS_CONCEPT`

いま解こうとしている課題は「**別記事にまたがる根拠の結合**」です。同じ概念（例: モーション検知、Wi-Fi 再接続）を扱う節が複数の記事に散っており、1 記事に閉じた検索では回答根拠が揃いません。

統合仕様書 §6 の「コミュニティ検出 + CommunitySummary で話題の塊を検索できるようにする（= global search）」がまさにこの用途に見えたため、**自前でクラスタリングや俯瞰要約を実装せず**、`community.target_node_types` に `ManualSection` + `Concept` を追加していただいたうえで（2026-07-22 対応済み、`docs/runbooks/vegapunk-community-target-node-types.md`）、`Merge` RPC を明示的に呼ぶ設計にしました。

その最初の実行で今回の問題に当たっています。

---

## 2. 発生している現象（時系列、すべて実測）

Cloud Run job から `Merge(schema="urtect")` を 2 回実行しました。時刻は JST です。

### 1 回目: 2026-07-27 15:43

| 時刻 | 出来事 |
|---|---|
| 15:43:22 | `Merge` 開始。直前の `GetStats`: `node_count=5971` / `edge_count=18549` / `vector_count=3507` / **`community_count=0`** |
| 15:44:21 | node2vec ジョブ `node2vec:urtect:gen1:mrev1:att5e1151cc-...` が作成される |
| 15:44:02 | **クライアント側の h2 keepalive がここで接続を切ってしまった**（40 秒。これは当方の設定ミスで、後述のとおり修正済み。vegapunk 側の問題ではありません） |
| 16:02 頃 | 切断後もサーバ側処理は継続しており、`GetStats` が `community_count=198` / `node_count=6280` / `edge_count=24638` に増加。**Leiden コミュニティ検出は完了している** |
| 16:37:22 | node2vec ジョブが `job timed out (exceeded job_timeout_secs)` で `failed`（`retry_count=3`）。作成から **53.0 分** |
| 16:36 以降 | `node_count` / `edge_count` が凍結し、検索レイテンシも元に戻る（サーバ側処理の停止） |

この時点でも `Search(mode="global")` は `No community summaries found` のままでした。

### 2 回目: 2026-07-27 17:29（クライアント側 keepalive 修正後）

| 時刻 | 出来事 |
|---|---|
| 17:29:34 | `Merge` 開始。直前の `GetStats`: `node_count=6367` / `edge_count=24725` / `community_count=198` |
| 17:30:57 | node2vec ジョブ `node2vec:urtect:gen1:mrev1:att5415b75b-...` が作成される |
| 18:00:26 | node2vec が `job timed out (exceeded job_timeout_secs)` で `failed`（`retry_count=3`）。作成から **29.5 分** |
| 18:00:26 | `Merge` RPC が `FAILED_PRECONDITION` を返す。`elapsed_secs=1852`（30.9 分）<br>メッセージ: `merge aborted: 1 job(s) reached a terminal failure state: node2vec:urtect:gen1:mrev1:att5415b75b-ebfa-4aa0-9791-1edb27fcfa6b (failed)` |

今回はクライアントが切断せず最後まで待ち切ったので、**サーバ側の abort 理由をそのまま受け取れました**。

---

## 3. 取得済みの証跡（`ListJobs` の `JobInfo`）

`Merge` のエラーメッセージには「どのジョブが失敗したか」しか入らないため、統合仕様書 §4.4 の復旧手順に従い `ListJobs` を叩いて `JobInfo.error` を取得しました（直近 24 時間・上限 500 件、全 32 件が該当）。

内訳は `community_summary` 15 件 / `search_evaluation` 15 件 / `node2vec` 2 件です。

### 3-1. Merge を止めている node2vec（本題）

```json
{
  "job_id": "node2vec:urtect:gen1:mrev1:att5415b75b-ebfa-4aa0-9791-1edb27fcfa6b",
  "job_type": "node2vec",
  "status": "failed",
  "error": "job timed out (exceeded job_timeout_secs)",
  "retry_count": 3,
  "created_at": 1785141057698,   // 2026-07-27 17:30:57 JST
  "completed_at": 1785142826363  // 2026-07-27 18:00:26 JST（29.5 分）
}
{
  "job_id": "node2vec:urtect:gen1:mrev1:att5e1151cc-333a-409c-88f3-d64da4b16006",
  "job_type": "node2vec",
  "status": "failed",
  "error": "job timed out (exceeded job_timeout_secs)",
  "retry_count": 3,
  "created_at": 1785134661096,   // 2026-07-27 15:44:21 JST
  "completed_at": 1785137842361  // 2026-07-27 16:37:22 JST（53.0 分）
}
```

### 3-2. CommunitySummary は動いてはいる（ただし Gemini が断続的に失敗）

15 件すべて `status: completed` ですが、`error` フィールドに以下が残っています。

```json
{
  "job_id": "community_summary:urtect:gen1:cs-mrev1-L0-105:att5e1151cc-...",
  "job_type": "community_summary",
  "status": "completed",
  "error": "unavailable: Gemini API request failed: error sending request",
  "retry_count": 1
}
```

リトライで吸収されているようですが、**198 コミュニティに対して 24 時間の窓に 15 件しか要約ジョブが存在しない**点は気になっています（Merge が abort したためにそこで打ち切られた、という理解で合っていますでしょうか）。

### 3-3. search_evaluation が DB ロックで失敗している（副次）

```json
{
  "job_id": "eval-1deecc76-fd9e-4cf4-aae0-14ca50baaa2f",
  "job_type": "search_evaluation",
  "status": "failed",
  "error": "storage error: CozoDB script error: database is locked (code 5)",
  "retry_count": 3
}
```

統合仕様書 §5.4 の「Search の副作用として非同期 enqueue される検索品質採点ジョブ」だと理解しています。Merge の blocker ではありませんが、書き込み競合が起きているようなので共有します。

---

## 4. 分析（分かっていること / 分かっていないこと）

**分かっていること**

- node2vec ジョブが terminal failure になると **Merge 全体が abort** し、Leiden の結果（198 コミュニティ）が残っていても CommunitySummary は commit されず、global search は使えないままになる
- 失敗理由は `job timed out (exceeded job_timeout_secs)` で、これはサーバ設定に起因する
- 対象グラフは **6,367 ノード / 30,662 エッジ**（`GetStats` 実測）。統合仕様書 §6.4 が長時間化の目安として挙げている「10 万ノード超」には遠く及ばない規模

**分かっていないこと（教えていただきたい点）**

- **`worker.job_timeout_secs` の現行値**。統合仕様書 §11 の既定は 300 秒ですが、`created_at` → `completed_at` が 29.5 分 / 53.0 分で `retry_count=3`（＝ 4 試行）であることから、1 試行あたり 7〜13 分程度かかっている計算になります。現行値は既定より大きく設定されている可能性があり、実値を確認しないと「いくつに上げるべきか」が決められません
- **`worker.job_ttl_secs`（既定 3600 秒 = 1 時間）との関係**。統合仕様書 §11 は「ジョブ終端の主条件。超過ジョブは次の失敗で `failed`」としています。1 試行 30 分級のジョブを 4 回試行すると TTL を必ず超えるため、`job_timeout_secs` だけ上げても TTL 側で終端する懸念があります
- **node2vec が 6 千ノード規模で 30〜53 分かかるのは想定内か**。仮に想定外なら、タイムアウトを上げるのは対症療法になります

---

## 5. 依頼事項

優先度順に記載します。A だけでも実施いただければ、こちらで効果を検証できます。

### A. 【最優先】`worker.job_timeout_secs` の確認と引き上げ（+ 再起動）

1. 現行値を確認いただきたい（`config.yml` と環境変数の両方）
2. node2vec の 1 試行が完走できる値へ引き上げていただきたい。**実測 30〜53 分**を踏まえると、余裕を見て **3600 秒（1 時間）以上**を提案します
3. あわせて **`worker.job_ttl_secs`（既定 3600 秒）** を、`job_timeout_secs × (retry 回数 + 1)` を上回る値へ。上記の提案値なら **14400 秒（4 時間）以上**が目安です

補足として、統合仕様書 §11 によると `worker.job_timeout_secs` は環境変数の上書きホワイトリストに含まれますが、**`worker.job_ttl_secs` は含まれない**ため `config.yml` の直接編集が必要という理解です。いずれも config はホットリロードされないため、**プロセスの再起動**が要るという理解で合っていますでしょうか。

> 再起動は他プロダクトと共用のインスタンスに影響するため、タイミングはそちらのご判断にお任せします。こちらは再起動後に検証を回すだけです。

### B. 【要調査】node2vec の所要時間の妥当性

6,367 ノード / 30,662 エッジという小規模グラフで 1 ジョブ 30〜53 分は妥当でしょうか。もし想定より遅いようであれば、タイムアウト引き上げは対症療法になるため、原因の切り分け（ウォーク長 / ウォーク数 / 次元数などのパラメータ、CPU 資源、他ジョブとの競合）をご検討いただけると助かります。

参考として、同時間帯に **cs-support 側の検索レイテンシが 5 秒 → 30〜60 秒に悪化**していました（Merge 走行中のみ。終了後は元に戻りました）。同一プロセス内で計算資源を食い合っている可能性があります。

### C. 【相談】Merge の部分失敗時の挙動

現状、node2vec が失敗すると **Leiden と CommunitySummary の成果まで含めて Merge 全体が abort** し、global search は使えないままになります。

こちらの用途では、**構造スコア（Node2Vec）は無くても CommunitySummary さえ使えれば価値が出ます**（別記事の結合が目的で、構造的近さの重み付けは必須ではありません）。

そこで相談です。

- node2vec の失敗を Merge 全体の abort とする現在の設計は意図どおりでしょうか
- 部分成功（Leiden + CommunitySummary は commit し、構造スコアだけ `NOT_READY` のままにする）を許容する余地はありますか
- あるいは、Merge から node2vec を除外して呼ぶ手段（リクエストのオプション、または config）はありますか

もし「全部揃わないと commit しない」が意図的な設計であれば、それはそれで理解できますので、**その旨を教えていただければこちらは A の対応だけで進めます**。

### D. 【提案】エラーの可観測性

今回、`Merge` の `FAILED_PRECONDITION` メッセージからは「node2vec ジョブが失敗した」ことしか分からず、**具体的な理由（タイムアウト）を知るために `ListJobs` を叩く診断機能をクライアントに実装する必要がありました**（本番 vegapunk が VPC 内部限定のため、外から grpcurl で確認する経路も無いという事情があります）。

`Merge` の失敗メッセージに、失敗ジョブの `error` 文字列（今回なら `job timed out (exceeded job_timeout_secs)`）を含めていただけると、運用者が一次情報だけで次のアクションを判断できるようになります。

> **【取り下げ済み・2026-07-27】** 当初この節で「`ListJobs` の `status` の有効値が proto コメント（`dead_letter`）と統合仕様書（`failed`）で食い違っている」と質問しましたが、**これはこちらの誤解でした**。ご指摘のとおり proto（`proto/graphrag.proto:658`）・統合仕様書（`:263`）・実装（`src/worker/queue.rs:17-30`）の 3 つはいずれも `"pending" | "running" | "completed" | "failed"` で一貫しています。`dead_letter` が残っていたのは**当方がリポジトリに vendor している proto のコピー**（`server/proto/graphrag.proto`）だけで、そちらに不整合はありません。混乱させてしまい失礼しました。当方のコピーは修正済みです。
>
> あわせて、**vendor 済み proto が古かったこと自体**を当方の課題として持ち帰ります（この proto から gRPC クライアントを生成しているため、他のメッセージにも drift がある可能性があります）。最新の proto との突き合わせを別途行います。

### E. 【共有のみ】副次的に観測された 2 件

対応要否の判断はお任せします。Merge の blocker ではありません。

1. **Gemini バックエンドの断続的な失敗**: `unavailable: Gemini API request failed: error sending request` が `community_summary` と `search_evaluation` の両方で出ています。リトライで吸収されていますが、Merge の所要時間を押し上げている可能性があります
2. **CozoDB のロック競合**: `search_evaluation` が `storage error: CozoDB script error: database is locked (code 5)` で失敗しています（`retry_count=3` で terminal failure に至ったものが 1 件）

---

## 6. 変更後の検証（cs-support 側で実施します）

設定変更と再起動が終わったら、こちらで以下を回して結果を共有します。追加のご負担はありません。

1. `merge-schema` Cloud Run job を Merge モードで実行（クライアント側 timeout は 6 時間、h2 keepalive は無効化済みなので、長時間の同期実行でも切断しません）
2. 完了後に確認する項目
   - `Merge` RPC が正常終了するか、`merge_elapsed_secs` の実測値
   - `GetStats.community_count` の推移
   - **`SearchExecution.readiness.global` / `readiness.community_summary` が `READY` になるか**（`community_count > 0` は global が使えることを意味しないと今回学びました）
   - `Search(mode="global")` / `mode="hybrid"` が何を返すか（特に代表メンバーとして `ManualSection` の node_id が返るかどうか。これが当方の次フェーズの設計分岐そのものです）
3. 失敗した場合は `ListJobs` の診断結果（`JobInfo.error`）を添えて再度共有します

---

## 7. 環境情報

| 項目 | 値 |
|---|---|
| schema | `urtect` / generation `gen1` / merge revision `mrev1` |
| グラフ規模 | `node_count=6367` / `edge_count=30662` / `vector_count=3507`（2026-07-27 19:04 時点） |
| コミュニティ | `community_count=198`（Leiden は完了、CommunitySummary は未 commit） |
| 呼び出し元 | Cloud Run job `merge-schema`（GCP project `sivira-cs-support` / region `asia-northeast1`）、VPC connector 経由 |
| 対象エンドポイント | `http://10.10.0.2:6840` |
| 認証 | ルート Bearer トークン（Secret Manager 注入） |
| 失敗ジョブ ID | `node2vec:urtect:gen1:mrev1:att5415b75b-ebfa-4aa0-9791-1edb27fcfa6b`<br>`node2vec:urtect:gen1:mrev1:att5e1151cc-333a-409c-88f3-d64da4b16006` |

---

## 8. 補足: 依頼側で既に対処したこと

vegapunk 側の問題と混同しないよう、こちらの不具合として直したものを明記しておきます。

**クライアントの h2 keepalive が正常な Merge を 40 秒で切っていました。** `http2_keep_alive_interval(30s)` + `keep_alive_timeout(10s)` の設定で、Merge が同期実行のためサーバが PING に応答できず、`Unavailable: http2 error ... keep-alive timed out` になっていたものです。1 回目の失敗はこれが原因で、**Merge 自体の失敗ではありませんでした**。

Merge を呼ぶ CLI に限り h2 keepalive を無効化し（常駐サーバ側の設定は変更していません）、2 回目は 31 分間切断せずに待ち切ってサーバ側の本当の理由を受け取れています。

---

## 9. 再テスト結果（2026-08-03、設定変更後の検証）

vegapunk 側の設定変更後として Merge を再実行しましたが、**同一のエラーで再現しました。設定がプロセスに反映されていないと考えられます。**

| 項目 | 値 |
|---|---|
| 実行 | Cloud Run job `merge-schema-g64rn`（2026-08-03 03:19 UTC 開始、31.5 分で abort） |
| Merge の失敗理由 | `FailedPrecondition: merge aborted: 1 job(s) reached a terminal failure state: node2vec:urtect:gen1:mrev1:att31024da8-d761-4d70-9fac-d9dee5e79d04 (failed)` |
| node2vec の `JobInfo.error` | **`job timed out (exceeded job_timeout_secs)`**（前回と同一） |
| node2vec の所要 | `created_at` → `completed_at` 約 29.8 分、`retry_count = 3`（前回実測 約 29.5 分と同水準） |
| `readiness.global` | `NOT_READY`（`No community summaries found`）のまま |
| community_summary | 今回は retry 0 で多数完走（前回断続的だった Gemini 側は安定） |

`job_timeout_secs` が 3600s に上がっていれば 1 試行に 60 分許容されるため、29.8 分でのタイムアウトは起こり得ません。確認をお願いしたい点（可能性の高い順）:

1. **プロセス再起動を実施したか**（config はホットリロードされないため、変更だけでは反映されません）
2. **`worker.job_ttl_secs`（既定 3600s）も引き上げたか**（env ホワイトリスト外のため `config.yml` 直接編集が必要。`job_timeout_secs` だけ上げても TTL 側で終端されると同種のエラーになります）
3. **env で上書きした場合、起動プロセスにその env が届いているか**（systemd unit / シェル環境の差異）

なお、**クライアントが切断してもサーバ側の Merge は継続していました**（切断後に `community_count` が 0 → 198 に増加）。これは想定どおりの挙動でしょうか。もしそうであれば、接続断のあとに再実行すると `FAILED_PRECONDITION`（同時実行不可）になるはずなので、こちらの運用手順に「再実行前に `GetStats` で確認する」を入れてあります。
