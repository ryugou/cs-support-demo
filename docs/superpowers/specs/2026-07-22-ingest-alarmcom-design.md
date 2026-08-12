# answers.alarm.com 全マニュアル ingest 設計（v2: 全件取り込み + 自前翻訳 + Concept/community）

GitHub Issue: #8。前提ブランチ: `feat/8-ingest-alarmcom`（`refactor/6-product-master-vegapunk` の上に積む）。

> **v2 改訂の経緯（2026-07-22）**: v1（製品マスタ駆動でクロール対象を絞る設計）は破棄した。要件は「answers.alarm.com のマニュアルを**全て** MCP 経由で参照可能にする」であり、製品による絞り込み・products.json 運用は要件に反する。加えて (a) サイト機械翻訳（`?mt-language=JA`）は品質不良のため不採用とし英語原文から自前で全ページ翻訳する、(b) 概念クエリ（型番を含まない自然文）と section 横断の答えを成立させるため Concept ノード + community を導入する、という 2 点を追加した。v1 の family-hub 選別・製品マスタ fail closed・英日 2 fetch は全て削除する。

## 要件（source of truth）

| ID | 要件 |
|----|------|
| R1 | answers.alarm.com の **全記事**（`/Customer` + `/Partner`、約 3,490 件、製品フィルタ一切なし）を MCP 経由で検索可能にする |
| R2 | サイト機械翻訳は使わない。**英語原文から自前で全ページ再翻訳**（Gemini Flash 3.6）。`en_hash` 差分再翻訳は維持 |
| R3 | 概念クエリ（型番なしの自然文。例「開店時にドアのロックが解除されるようにしたい」）が意味で引ける |
| R4 | 複数 section にまたがる答え（例「First Person In ルールを作る」+「営業時間が無効だと表示されない」）を join する |
| R5 | 表記揺れ（例「ファーストパーソンイン」↔「First Person In」）。当面は保留だが基盤（aliases）は用意する |

## サイト調査結果（2026-07-22 実測、v1 から継続）

- プラットフォーム: MindTouch 系 KB。**完全 SSR**（本文は素の HTML。JS レンダリング不要）
- 本文コンテナ: `<article class="elm-content-container" id="elm-main-content">`。パンくず: `class="mt-breadcrumbs"`（現在ページは `mt-breadcrumbs-current-page`）
- robots.txt: `Crawl-delay: 5`、`Request-rate: 1/5`、`Disallow: /@*`（= MindTouch JSON API `/@api/deki/pages/...` は**使用禁止**。技術的には無認証で動くが規約違反のため使わない）
- sitemap: `https://answers.alarm.com/sitemap.xml`。フラットな `<url>` 一覧 約 3,490 件（`/Partner/...` 2,957 件、`/Customer/...` 529 件）
- 日本語版（`?mt-language=JA`）は**使わない**（品質不良。R2）。英語版のみ取得し自前翻訳する

## アーキテクチャ全体像

```
sitemap.xml（全 /Customer + /Partner）
  → クロール（EN のみ, 1 fetch/記事, Crawl-delay=5 厳守, 逐次）
  → Gemini Flash 3.6【翻訳 + Concept抽出 を 1 LLM パスに統合】
  → Upsert（ManualSection + Concept + 辺）  ← 決定論的, en_hash 差分, 記事単位インクリメンタル
  → Merge RPC 明示呼び（community 生成; Phase B）
  → search_manual(mode=hybrid)（Phase B で local→hybrid）
```

Upsert を使う理由（確認済み）: 高レベル `Ingest` RPC は生テキストを LLM でグラフ再構築するため、`section_key` / `en_hash` / `translation_status` を決定論的に保持できない。我々は手組みグラフを `UpsertNodes`/`UpsertEdges`/`UpsertVectors` でそのまま永続化する。community は `Ingest` 専用ではなく、Upsert 後に **`Merge` RPC を明示呼び**すれば得られる。

## グラフモデル（加算のみ、スキーマ破壊なし）

### 既存（維持）
- ノード: Document / ManualSection / Product / Signal
- 辺: HAS_SECTION / PARENT_OF / DESCRIBES / MENTIONS_SIGNAL

### 追加
- **Concept ノード**: `concept_key`（正規化キー, stable upsert key）/ `name_en` / `name_ja` / `aliases_ja`（カタカナ等の表記揺れ, JSON 配列文字列）/ `kind`（`feature` | `rule` | `setting` | `term`）
- **MENTIONS_CONCEPT 辺**: `section → Concept`。**community が clustering する拠り所**。同一 Concept を指す別記事の section は `section→Concept→section` で連結され、同一 community に入る

### alarm.com は product 非依存
- alarm.com の section には **DESCRIBES（→Product）を張らない**。products.json / 製品マスタとは完全に無関係（R1）。v1 の「製品マスタ 0 件で fail closed」は削除する
- Product ノードは urtect（Google Sites, `ingest_urtect`）用に残すが、`ingest_alarmcom` は触らない
- `MENTIONS_SIGNAL`（signal-lexicon 抽出）は既存挙動のまま日本語本文に適用してよい（無害。中心ではない）

### Concept が 3 要件を同時に満たす
- R3: クエリと本文を「概念」で橋渡しし、意味検索の当たりを上げる
- R4: community クラスタの拠り所（別記事 section の join）
- R5: `aliases_ja` が表記揺れ辞書そのもの

## 翻訳 + Concept 抽出（1 LLM パスに統合）★設計の要

同じ本文を翻訳用と抽出用で 2 回 LLM に渡さない。**Gemini Flash 3.6 の 1 コールで両方を出す。**

- 実装境界: 既存 `server/src/translate.rs`（外部翻訳境界）に Gemini client を差す
- **確定した Gemini アクセス（2026-07-22）**:
  - API 面: **Google AI Studio の Gemini API**（`generativelanguage.googleapis.com`。API キー方式）
  - モデル ID: **`gemini-3.6-flash`**
  - API キー env 名: **`CS_SUPPORT_GEMINI_API_KEY`**（1Password → `.env`、本番は Secret Manager injection。`.env.example` 追記済み）
  - エンドポイント: `https://generativelanguage.googleapis.com/v1beta/models/gemini-3.6-flash:generateContent`。認証は **`x-goog-api-key` ヘッダ**（`?key=` クエリではない。鍵を URL・ログに出さないため）
  - 構造化出力: `generationConfig.responseMimeType = "application/json"` + `responseSchema`（`{ body_ja, concepts[] }`）で JSON を強制する
  - **検証済み（2026-07-22）**: 上記キーで `GET /v1beta/models` が 200 を返し、`models/gemini-3.6-flash` の実在を確認済み
  - モデル ID / エンドポイントは config の翻訳セクション（`[llm]` に倣った新セクション）に置く。**キーだけを env にし、キーをコードにハードコードしない**（既存 `server/src/llm.rs` の `CS_SUPPORT_LLM_API_KEY` 解決と同じ流儀）
- 入力: section の EN 本文 + glossary（`server/data/glossary.json`）+ page 文脈（breadcrumb・親 section title を**非翻訳の context** としてプロンプトに添付。翻訳の一貫性と Concept 抽出精度のため）
- 出力（構造化 JSON）:
  ```json
  {
    "body_ja": "…",
    "concepts": [
      {"name_en": "First Person In rule", "name_ja": "ファーストパーソンインルール",
       "aliases_ja": ["ファーストパーソンイン", "最初に人が入る"], "kind": "rule"}
    ]
  }
  ```
- glossary 注入で型番・製品名・専門語の訳ブレを防ぐ
- Concept 抽出は「durable な製品概念・機能・ルール・設定」に限定するプロンプト規律（一般語をノイズにしない）
- Concept 正規化: `name_en` を正規化して `concept_key` を生成し、別ページ間で fuzzy マージして同一 Concept ノードに集約する。concept_key の正規化・マージ規則は純関数として実装しテストする
- **en_hash 差分**: `en_hash` が変わった section だけ再翻訳 + 再抽出。英語不変の section は Gemini を呼ばない

## クロール（`server/src/bin/ingest_alarmcom.rs`）

- **sitemap.xml 駆動で /Customer + /Partner 全 URL**。製品マスタ絞り込みは無し
- **EN のみ 1 fetch/記事**（`?mt-language=JA` は撤去）。fetch 数が v1 の半分。約 3,490 記事 × 5s ≒ **約 4.8h**
- HTTP client 構成は `ingest_urtect` に合わせる（UA `cs-support-mcp/ingest_alarmcom`、同一 host のみ redirect、connect 10s / total 30s）
- **リクエスト間隔: 全リクエスト（sitemap 含む）で `--crawl-delay-secs`（既定 5、5 未満は 5 に切り上げ warn）以上空ける。逐次実行。並行フェッチ禁止。robots 遵守をコードで強制し、緩める変更を入れない**
- 本文抽出: `#elm-main-content` 配下テキスト。`script`/`style`/`noscript`/`nav`/`header`/`footer` 除外・空白正規化は `ingest_urtect` の `extract_main_text` / `normalize_body` 方針（共通化は `server/src/manual/` 配下 helper で）
- パンくず: `.mt-breadcrumbs` から ` > ` 結合
- 取得失敗（HTTP エラー / 本文空）は当該記事 skip + warn（URL と理由）。全記事の 2 割超 skip なら bail（サイト構造変化の疑い。閾値はコード内定数）
- **記事単位でインクリメンタル upsert**（クラッシュ耐性。v1 の「全メモリ蓄積→最後に一括」は廃止）。embed/upsert_vectors → node/edge upsert の順（fail closed、`ingest_urtect` と同じ理由）

### section 写像
- document: `DOC_KEY = "doc-alarmcom"` 1 件（title "Alarm.com Answers"、source_url base-url）。urtect の `doc-manual` と共存
- section: 記事 1 件 = ManualSection 1 件
  - `slug`: URL パスから生成。`alarmcom-` プレフィックスで urtect slug と衝突回避
  - `title` / `body`: **日本語（Gemini 翻訳）**（検索対象は body_ja）
  - `body_original` / `original_hash`: **英語原文とその hash**（`ManualSectionInput` に `Option` フィールド追加。`Some` のときだけ属性出力。urtect は None を渡し既存挙動不変。加算のみ）
  - `source_lang`: `body_original` が Some なら `"en"`
  - `en_hash` / `content_hash`: **英語原文の normalize 済み本文の hash**。差分再 ingest は英語変化で駆動
  - `translation_status`: `current`（Gemini 翻訳直後）。`en_hash != translated_from_hash` の判定は既存方針を踏襲
  - `parent_slug`: URL パスの親の slug。sitemap の階層から導出（ハブ自身は parent なし）
  - `order`: sitemap 内出現順
- 差分 ingest: 全体 snapshot から hash マップを引き、不一致・新規のみ upsert。**hash 比較・stale 削除は `alarmcom-` プレフィックス section に限定**し、urtect 側 section を絶対に消さない

## 検索（Phase B）

- `search_manual` の vegapunk 呼び出しを **`mode="local"` → `"hybrid"`**（`server/src/vegapunk.rs`）。hybrid = local(vector + IDF) ∪ global(community summary)。**summary 不在時は local に自動フォールバック**するので安全に先行切替できる
- vector 経路（`client.search`）で R3 が効く（良翻訳前提）
- `max(text, vector)` の未較正合成（コード自認の Task 11）は Phase C で RRF/加重へ
- scale 注意: `search_with_snapshot` は毎クエリ snapshot 全 section 走査。数万 section 規模で 10 万ノードに近づくため将来最適化として記録（現状は out of scope）

## community 配線（Phase B）

> **2026-07-27 更新**: Phase B の確定 spec は [`2026-07-27-phase-b-community-merge-design.md`](./2026-07-27-phase-b-community-merge-design.md) に移した。下記「`ingest_alarmcom` の末尾で Merge を呼ぶ」は**専用 CLI + 専用 Cloud Run job `merge-schema` に分離する**方針へ変更済み（`ingest_alarmcom` の実測所要が約 6 時間で、その末尾に同期 Merge を積むと Merge だけの再実行ができないため）。Phase B は B1（Merge 配線 + 実行 + global 返却物の実測）と B2（実測に基づく結合実装）に分割した。

- Upsert 完了後、**`Merge` RPC を明示呼び**（`VegapunkClient` に `merge(schema)` を追加）。呼び出し経路は上記のとおり専用 CLI へ変更
- **vegapunk 側前提（対応済み・2026-07-22）**: `community.target_node_types` に `ManualSection` + `Concept` を追加済み（既定 8 型 + 2 型 = 10 型）。手順は [`docs/runbooks/vegapunk-community-target-node-types.md`](../../runbooks/vegapunk-community-target-node-types.md)。この設定はグローバル（schema 別上書き不可）だが、該当型を持たない schema には無害な加算
- 代替経路（community を使わない場合）: query 時に `MENTIONS_CONCEPT` を辿る concept-expansion（get_section 展開の延長）。vegapunk 変更不要だが LLM 合成サマリは得られない

## 段階デリバリ

| Phase | 内容 | 満たす要件 | 外部依存 |
|-------|------|-----------|---------|
| **A** | 全取り込み + 全 Gemini 翻訳 + Concept 抽出/ノード化 + hybrid(実質 vector) 検索。同一記事内 join は get_section 展開 | R1, R2, R3 | Gemini API アクセス。**ここで「全マニュアル MCP 参照可能」達成** |
| **B** | `Merge` RPC 明示呼び + search を hybrid 化 + community global | R4（別記事 join） | vegapunk target_node_types（対応済み） |
| **C** | `aliases_ja` でクエリ正規化 + fusion 較正 | R5, 品質 | なし |

Concept 抽出は Phase A の翻訳パスに**最初から同梱**する（後で本文を LLM に流し直さない）。community 配線（Merge 呼び・hybrid 切替）だけを Phase B に切り出す。

## Cloud Run job

- GCP 本番 vegapunk（`10.10.0.2:6840`）は VPC 内部のみ到達可。**`ingest_alarmcom` は cs-support VPC 内の Cloud Run job `ingest-alarmcom`（新設）から実行する**（ラップトップから直接投入は不可）
- service / 既存 job と同一イメージ。`<tag>` を揃える。CLAUDE.md の Cloud Run 節に追記
- 実行順序: `ingest_products` → `ingest_urtect` / `ingest_alarmcom`（ただし alarm.com は製品マスタ非依存なので products.json 投入の前後を問わない）

## テスト（ネットワーク不要の純関数を分離）

- sitemap XML パース（最小 fixture）
- 本文抽出: `elm-main-content` / `mt-breadcrumbs` を含む最小 HTML fixture からの title / body / breadcrumb 抽出
- 親子（parent_slug）導出: URL パス階層からの導出
- Concept 正規化 / concept_key 生成 / fuzzy マージ（純関数）
- `build_section_graph` の `body_original` / `original_hash` / `source_lang` / MENTIONS_CONCEPT: Some で属性・辺が出る / None で出ない（既存テスト更新）
- translate.rs: Gemini レスポンス JSON のパース（`body_ja` / `concepts` 抽出、壊れた JSON のエラーパス）。実 API 呼び出しはテストしない（境界で分離）
- 既存テスト全通過

## 検証（evidence として最終出力に含める）

- `cargo fmt --manifest-path server/Cargo.toml -- --check`
- `cargo check --manifest-path server/Cargo.toml --all-targets`
- `cargo test --manifest-path server/Cargo.toml`
- 実サイトクロール・実 vegapunk 投入・実 Gemini 呼び出しの E2E はコンテナ/CI では実行しない（ネットワーク・API キー必要）。未実行を最終出力に明記し、Cloud Run job で別途実施する

## スコープ外

- 訳の手動オーバーライド保護（`manual_override` は予約値のまま）
- fusion スコア較正（Phase C）
- 10 万ノード超のスケール最適化
- signal-lexicon の model signal 二重管理解消（#7）

## 制約

- Python / TypeScript を使わない
- robots.txt 遵守（5 秒間隔・API 不使用）をコードで強制。「速くするために間隔を詰める」変更を入れない
- 認証情報をハードコードしない。Gemini API キーは env（1Password → `.env`）経由。モデル ID / エンドポイントは config 境界
- エラーを握りつぶさない。skip は必ず URL と理由を warn
- 既存の構成・命名・型に合わせ最小差分。`ingest_urtect` との共通化は `server/src/manual/` 配下 helper 抽出で行いコピペ増殖させない
- スキーマ変更は加算のみ（Concept / MENTIONS_CONCEPT / body_original 属性）
- Conventional Commits。commit は可、push・PR 作成は指示があるまで禁止
- spec にない非自明な判断が必要になったら実装せず差し戻す
