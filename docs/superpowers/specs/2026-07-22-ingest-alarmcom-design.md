# answers.alarm.com（日本語版）ingest 設計

GitHub Issue: #8。前提ブランチ: `refactor/6-product-master-vegapunk`（#6 の製品マスタ vegapunk 正本化を含む）。

## 背景

urtect 製品（ADC-*）の元メーカー Alarm.com のサポートサイト https://answers.alarm.com/ を第 2 のマニュアルソースとして vegapunk に登録する。既存ソースは Google Sites の urtect マニュアル（`ingest_urtect`）のみ。

## サイト調査結果（2026-07-22 実測）

- プラットフォーム: MindTouch 系 KB。**完全 SSR**（本文は素の HTML に含まれる。JS レンダリング不要）
- 本文コンテナ: `<article class="elm-content-container" id="elm-main-content">`。パンくず: `class="mt-breadcrumbs"`（現在ページは `mt-breadcrumbs-current-page`）
- robots.txt: `Crawl-delay: 5`、`Request-rate: 1/5`、`Disallow: /@*`（= MindTouch JSON API `/@api/deki/pages/...` は**使用禁止**。技術的には無認証で動くが規約違反になるため使わない）
- sitemap: `https://answers.alarm.com/sitemap.xml`。フラットな `<url>` 一覧 約 3,490 件（`/Partner/...` 2,957 件、`/Customer/...` 529 件）。`lastmod` は活発に更新されている
- 日本語: 同一 URL に `?mt-language=JA` を付けるとサイト側の機械翻訳が返る（`content-language: ja`）。cookie 非依存・リクエスト単位。**canonical URL は英語版と共通**なので、英日ペアリングは「同一 URL + クエリ有無」で自明
- 対象製品のハブページ（実在確認済み）:
  - `.../Partner/Installation_and_Troubleshooting/Video_Devices/1080p_Outdoor_Wi-Fi_Camera_with_Two-Way_Audio_ADC-V724_724X`（配下に Wi-Fi 設定 / factory reset / LED 意味 / データシート等のサブ記事）
  - `.../Partner/Installation_and_Troubleshooting/Video_Devices/1080p_Mini-Bullet_Camera_(ADC-VC727P)`（同様のサブ記事構成）

## スコープ判断

**全 3,490 記事は取り込まない。** robots の 5 秒間隔で英日 2 取得 ×3,490 は約 10 時間かかり、無関係製品（パネル・鍵・サーモスタット等）でグラフが汚れる。

**vegapunk 製品マスタ駆動で対象を絞る**（#6 と同じ思想）:

1. 起動時に `query_nodes(schema, "Product", ...)` で製品マスタを取得（`ingest_urtect` と同じ fail closed: 0 件 / limit 到達で bail）
2. sitemap.xml を 1 回取得し、URL 一覧を得る
3. 「ファミリーハブ URL」を特定する: sitemap URL のパス末尾セグメント（ファミリー名）を**緩和正規化**（英数字のみ残して大文字化。例: `1080p_Outdoor_..._ADC-V724_724X` → `1080POUTDOOR...ADCV724724X`）し、製品の `model` / `aliases` を同じ正規化にかけた表層形が部分文字列として含まれるものをハブとみなす
4. クロール対象 = ハブ URL 自身 + sitemap 中でハブ URL をパスプレフィックスとする全 URL
5. どのファミリーにもマッチしなかった製品は warn ログ（製品名を列挙し、「products.json の aliases に URL 上の表記を追加せよ」という次アクション付き）。マッチ 0 製品なら bail

緩和正規化は **URL 選別専用**。本文からの DESCRIBES 検出は既存の厳密境界チェック（`detect_product_models` / `contains_as_token`）をそのまま使い、意味論を混ぜない。

補足: `ADC-V724X` はハブ URL 上 `724X` としか現れないため単独ではマッチしないが、同一ハブが `ADC-V724` でマッチするため取り込み漏れは起きない。将来この形の漏れが出た製品は aliases で吸収する（コードは直さない）。

## 新規 CLI `server/src/bin/ingest_alarmcom.rs`

CLI 引数（`ingest_urtect` の `Args` と同じ流儀）:

- `--endpoint` / `--schema`（既定 `urtect`）/ `--token-env` / `--token-file` / `--schema-file`（既定 `../schema/cs-support.yml`）/ `--lexicon-file`（既定 `data/urtect/signal-lexicon.json`）/ `--no-vectors`: すべて `ingest_urtect` と同じ意味
- `--base-url`（既定 `https://answers.alarm.com`）
- `--sitemap-url`（既定 `{base-url}/sitemap.xml`）
- `--crawl-delay-secs`（既定 `5`。robots.txt の Crawl-delay。**5 未満を指定されてもエラーにする**のではなく 5 に切り上げて warn）

### クロール実装

- HTTP client は `ingest_urtect` と同様の構成（UA は `cs-support-mcp/ingest_alarmcom`、同一 host のみ redirect 追従、connect 10s / total 30s timeout）
- **リクエスト間隔: 全リクエスト（sitemap 含む）で `--crawl-delay-secs` 以上空ける。逐次実行。並行フェッチ禁止**
- 各対象 URL について 2 回取得する:
  - 英語（canonical、クエリなし）→ `body_en`
  - 日本語（`?mt-language=JA` を付与。既存クエリがある URL は `&` 連結）→ `title_ja` / `body_ja`
- 本文抽出: `#elm-main-content`（`article.elm-content-container`）配下のテキスト。`script`/`style`/`noscript`/`nav`/`header`/`footer` 除外と空白正規化は `ingest_urtect` の `extract_main_text` / `normalize_body` と同じ方針（共通化できる部分は `server/src/manual/` 配下へ helper として括り出してよい）
- パンくず: `.mt-breadcrumbs` から取得し、` > ` 結合で breadcrumb 文字列にする
- 日本語版の取得に失敗（HTTP エラー / 本文空）した場合はその記事を skip して warn（URL と理由）。英語版の取得失敗も同様に skip + warn。**全記事の 2 割超が skip になったら bail**（サイト構造変化の疑い。閾値はコード内定数でよい）

### グラフ写像

- document ノード: `DOC_KEY = "doc-alarmcom"` を 1 件（`build_document_node`。title は "Alarm.com Answers"、source_url は base-url）。既存 `doc-manual`（urtect Google Sites）とは別 document として共存する
- section ノード: 記事 1 件 = ManualSection 1 件
  - `slug`: URL パスから生成。`section_slug` 既存 helper の規約に合わせ、`alarmcom-` プレフィックスを付けて urtect 側 slug と衝突しないようにする
  - `title` / `body`: **日本語版**（検索対象は日本語本文、という既存方針どおり）
  - `body_original` / `original_hash`: **英語原文とその hash を書く**。`build_section_graph` は現在この 2 属性を「純予約」として書いていないが、本件で予約を実際に使い始める。`ManualSectionInput` に `body_original: Option<String>` / `original_hash: Option<String>` を追加し、`Some` のときだけ属性を出力する（**None の場合は属性自体を書かない**。既存 `ingest_urtect` は None を渡し、既存挙動・既存テストを変えない。加算のみのスキーマ変更）
  - `source_lang`: `body_original` が Some の場合は `"en"`、None の場合は従来どおり `"ja"` を書く（`build_section_graph` の引数化。urtect 側は従来値を維持）
  - `content_hash`: **英語原文の normalize 済み本文の hash**（`content_hash(normalized_body_en)`）。英語が source of truth であり、差分再 ingest は英語原文の変化で駆動する（英語不変なら日本語再取得もスキップしてよい。ただし初回実装では「hash 不一致の記事のみ英日とも再取得・再 upsert」の単純な形でよい）
    - **既知の制約（この簡略化の帰結）**: `ingest_urtect` は複合ハッシュ（本文 + 検出型番 + signal + order/parent 等）を採り、本文以外の派生要素が変わっても再検出できる。alarmcom は本文のみのハッシュにするため、英語本文が不変な限り `product_lexicon` / 製品マスタが変わって `detect_product_models` / `signal_values`（DESCRIBES / MENTIONS_SIGNAL 辺のもと）が変化しても、当該 section は「変更なし」と判定され再 upsert されない（例: `ingest_products` で新型番を追加し、既存の英語記事が偶然その型番に言及していても、本文不変なら新しい DESCRIBES 辺は張られない）。**運用上の回避策**: lexicon / 製品マスタを大きく変更した場合は、alarmcom の tenant schema を作り直して全件再 ingest する（backend に delete が無く、部分更新では派生辺を張り直せない）。この簡略化は本 spec が意図的に採用しており、コードのロジックは本文のみのハッシュに保つ。
  - `parent_slug`: URL パスの親（ハブページ）の slug。ハブ自身は parent なし
  - `order`: sitemap 内の出現順で採番
  - `section_no`: None
- `product_models`: 既存の厳密検出（`detect_product_models`）を **英語本文と日本語本文の連結**に対して適用（型番文字列は機械翻訳でも保存されるが、取りこぼし防止のため両方見る）
- `signal_values`: `ingest_urtect` と同じ lexicon 抽出を日本語本文に適用
- 差分 ingest: `ingest_urtect` と同じ方式（既存 snapshot から `content_hash` マップを引き、不一致・新規のみ upsert。stale 検出も同様）。snapshot 取得は `doc-alarmcom` 配下に限定できないため全体 snapshot でよいが、**hash 比較・stale 削除の対象は `alarmcom-` プレフィックスの section に限定**し、urtect 側の section を絶対に消さないこと
- embed / `upsert_vectors`: `manual::vectors` の共通 helper（#6 で抽出済み）を使う。embed 対象は日本語本文。fail closed（vector 先行）の順序も `ingest_urtect` と同じ

## Dockerfile / Cargo.toml

`ingest_alarmcom` bin を builder のビルド列と runtime `COPY` に追加する（Cloud Run job 新設はスコープ外）。

## CLAUDE.md 追記

Cloud Run 節の ingest 記述に、第 2 ソースとして `ingest_alarmcom` を簡潔に追記する（製品マスタ駆動でクロール対象が決まること、robots.txt の 5 秒間隔を守るため対象ファミリー数に比例して時間がかかること、実行順序は `ingest_products` → `ingest_urtect` / `ingest_alarmcom`）。

## テスト

ネットワーク不要の純関数を分離してテストする:

- ファミリーハブ選別: 緩和正規化のマッチ（`ADC-V724` が `..._ADC-V724_724X` ファミリーにマッチ / 無関係ファミリーにマッチしない / aliases 経由マッチ / どの製品にもマッチしない場合の除外）
- プレフィックスによる配下 URL 選別
- sitemap XML パース（実サイトの形式を模した最小 fixture 文字列）
- 本文抽出: `elm-main-content` / `mt-breadcrumbs` を含む最小 HTML fixture からの title / body / breadcrumb 抽出
- `?mt-language=JA` の URL 付与（クエリ有無両ケース）
- `build_section_graph` の `body_original` / `original_hash` / `source_lang`: Some で属性が出る / None で属性が出ない（既存テストの「純予約は書かない」表明は「None なら書かない」に更新）
- 既存テスト全通過

## 検証（evidence として最終出力に含める）

- `cargo fmt --manifest-path server/Cargo.toml -- --check`
- `cargo check --manifest-path server/Cargo.toml --all-targets`
- `cargo test --manifest-path server/Cargo.toml`
- 実サイトへのクロール・実 vegapunk への投入 E2E はコンテナ内では実行しない（ネットワーク必要）。未実行である旨を最終出力に明記する。ホスト側で別途実施する

## スコープ外

- Partner ツリー全量・他製品カテゴリの取り込み（対象拡大は将来、選別条件の変更で行う）
- MindTouch JSON API の利用（robots.txt Disallow のため。許可を書面で得た場合のみ将来検討）
- 翻訳品質の検証・手動オーバーライド
- Cloud Run job `ingest-alarmcom` の新設
- signal-lexicon の model signal 二重管理解消（#7）

## 制約

- Python / TypeScript を使わない
- robots.txt 遵守（5 秒間隔・API 不使用）をコードで強制する。「速くするために間隔を詰める」変更を入れない
- エラーを握りつぶさない。skip は必ず URL と理由を warn ログに残す
- 既存の構成・命名・型に合わせ最小差分。`ingest_urtect` との共通化は `server/src/manual/` 配下の helper 抽出で行い、コピペ増殖させない
- Conventional Commits。commit は可、push・PR 作成は禁止
- spec にない非自明な判断が必要になったら実装せず差し戻す
