/// URTECT (Google Sites) manual crawler / ingest CLI。設計・検証記録は
/// `docs/superpowers/plans/2026-07-08-manual-domain-template-urtect.md` を参照。
use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    harness::signal::{LexiconNormalizer, SignalNormalizer},
    manual::{
        crawl::{
            build_product_lexicon, detect_product_models, extract_text_excluding_noise,
            normalize_body,
        },
        ingest_model::{
            build_document_node, build_section_graph, content_hash, ManualSectionInput,
        },
        reingest::assert_no_stale_section_edges,
        schema_ids::{manual_node_id, section_slug, KIND_PRODUCT, KIND_SECTION},
        vectors::{embed_all, vector_entry, EMBED_CONCURRENCY},
    },
    model::GraphBuild,
    vegapunk::VegapunkClient,
};
use scraper::{Html, Selector};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::PathBuf,
};
use url::Url;

/// 差分 ingest の安定 doc key。build_section_graph の doc_key と揃える
/// （Task 3 サンプルテストと同じ規約 "doc-manual"）。
const DOC_KEY: &str = "doc-manual";

// 本文中の型番検出語彙は vegapunk の Product ノードが正本（Issue #6: KNOWN_MODELS 廃止）。
// ingest 開始時に Product ノード一覧を取得し、`model` 属性と `aliases` 属性
// （カンマ区切り）から表層形 → product_key の対応表を組み立てる
// （`build_product_lexicon` 参照）。製品の追加・変更は `ingest_products` CLI で
// products.json を投入するだけで反映され、本バイナリの再ビルドは不要になった。
//
// "ADC-V724" は "ADC-V724X" の前方一致になるため、detect_product_models 側で英数字境界を
// 見て誤爆(V724X ページを V724 とも誤判定)を防ぐ（`contains_as_token`）。
//
// data/urtect/signal-lexicon.json の model_adc_v724 / model_adc_v724x / model_adc_vc727p
// signal は今も型番をハードコードしている（Issue #7、本件のスコープ外）。あちらは
// MENTIONS_SIGNAL（表現ゆれ吸収・normalize_key 部分一致）用、こちらは DESCRIBES（型番の
// 厳密な同一性）用で判定基準が異なるため別管理になっている（lexicon の surface_forms は
// normalize_key で "-" ごと失われ、境界チェック付きの厳密一致には使えない）。Issue #7 で
// signal-lexicon 側も Product ノード起点に統一するまでは、二重管理のままである点に注意。

/// 定型の免責文（変更予告）。footer/header/nav などの意味タグに入っていなくても
/// 本文中に平文で混ざるケースを想定し、文字列一致で除去する。
const BOILERPLATE_SENTENCES: &[&str] =
    &["マニュアルの内容や画面は予告なく変更になる場合があります"];

/// document の `<title>` テキストを取り出す（無ければ空文字列）。
fn extract_title(document: &Html) -> String {
    let title_selector = Selector::parse("title").expect("valid selector");
    document
        .select(&title_selector)
        .next()
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// HTML から `<title>` とメイン本文テキストを抽出する純関数。
/// `<main>`（無ければ document ルート）を対象に、入れ子の nav/header/footer と
/// 定型免責文を除去する。空白正規化は行わない（`normalize_body` の責務）。
/// nav/header/footer/script/style 除外の共通ロジックは `crawl::extract_text_excluding_noise`
/// に集約し、ここでは `<main>` の選択と urtect 固有の定型免責文除去だけを行う。
fn extract_main_text(html: &str) -> (String, String) {
    let document = Html::parse_document(html);
    let title = extract_title(&document);

    let main_selector = Selector::parse("main").expect("valid selector");
    let container = document
        .select(&main_selector)
        .next()
        .unwrap_or_else(|| document.root_element());

    let mut raw = extract_text_excluding_noise(container);

    for boilerplate in BOILERPLATE_SENTENCES {
        raw = raw.replace(boilerplate, "");
    }

    (title, raw)
}

/// nav リンク 1 件（同一ホスト・manual 配下のみ列挙。列挙順を保つ Vec）。
struct NavEntry {
    url: String,
    /// 入れ子 <ul>/<ol> の深さ。breadcrumb/parent_slug の親子判定に使う。
    depth: usize,
}

/// breadcrumb/parent_slug 組み立て用の祖先スタック 1 段。
struct StackEntry {
    depth: usize,
    slug: String,
    breadcrumb: String,
}

/// URL のフラグメント/クエリを外し、末尾スラッシュを揃えた正準形（dedup キー）。
fn canonical_url(u: &Url) -> String {
    let mut c = u.clone();
    c.set_fragment(None);
    c.set_query(None);
    c.as_str().trim_end_matches('/').to_string()
}

/// top URL の最終セグメントを除いた prefix（"manual 配下のみ" フィルタに使う）。
fn manual_prefix(base: &Url) -> String {
    let mut segments: Vec<&str> = base.path().trim_end_matches('/').split('/').collect();
    segments.pop();
    let joined = segments.join("/");
    if joined.is_empty() {
        "/".to_string()
    } else {
        format!("{joined}/")
    }
}

/// anchor から scope（nav 要素）までの間にある <ul>/<ol> の数を depth として数える。
fn ancestor_list_depth(anchor: scraper::ElementRef<'_>, scope: scraper::ElementRef<'_>) -> usize {
    anchor
        .ancestors()
        .take_while(|node| *node != *scope)
        .filter_map(scraper::ElementRef::wrap)
        .filter(|el| matches!(el.value().name(), "ul" | "ol"))
        .count()
}

/// top ページの nav からページ一覧を列挙する（scraper で nav リンク抽出、
/// 同一ホスト・manual 配下のみ、列挙順を保って重複排除）。top 自身は除く。
/// 呼び出し側で一度 parse 済みの `Html` を受け取り、top ページを二重 parse しない。
fn enumerate_nav_entries(document: &Html, base_url: &Url) -> Vec<NavEntry> {
    let nav_container_selector = Selector::parse("nav").expect("valid selector");
    let anchor_selector = Selector::parse("a[href]").expect("valid selector");
    let prefix = manual_prefix(base_url);
    let top_canonical = canonical_url(base_url);

    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for nav_root in document.select(&nav_container_selector) {
        for anchor in nav_root.select(&anchor_selector) {
            let Some(href) = anchor.attr("href") else {
                continue;
            };
            let Ok(resolved) = base_url.join(href) else {
                continue;
            };
            if resolved.host_str() != base_url.host_str() || resolved.scheme() != base_url.scheme()
            {
                continue;
            }
            if !resolved.path().starts_with(&prefix) {
                continue;
            }
            let key = canonical_url(&resolved);
            if key == top_canonical {
                continue; // top 自身は nav 抽出専用ページとして扱い、列挙対象から除く
            }
            if !seen.insert(key.clone()) {
                continue;
            }
            let depth = ancestor_list_depth(anchor, nav_root);
            entries.push(NavEntry { url: key, depth });
        }
    }
    entries
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(
        long,
        env = "VEGAPUNK_ENDPOINT",
        default_value = "http://vegapunk.local:6840"
    )]
    endpoint: String,
    #[arg(long, default_value = "urtect")]
    schema: String,
    #[arg(long, default_value = "VEGAPUNK_BEARER_TOKEN")]
    token_env: String,
    /// bearer token ファイル（主経路）。CLAUDE.md のローカル/GCE 起動手順の既定パスと揃える。
    /// ファイルが無い/読めない場合のみ --token-env にフォールバックする。
    #[arg(
        long,
        env = "VEGAPUNK_BEARER_TOKEN_FILE",
        default_value = "/private/tmp/vegapunk-bearer-token"
    )]
    token_file: Option<PathBuf>,
    #[arg(long, default_value = "../schema/cs-support.yml")]
    schema_file: PathBuf,
    #[arg(long, default_value = "data/urtect/signal-lexicon.json")]
    lexicon_file: PathBuf,
    #[arg(
        long,
        default_value = "https://sites.google.com/view/urtect-manual/top"
    )]
    top_url: String,
    /// embed / upsert_vectors を一切呼ばずスキップする（ベクトル基盤未整備な環境向けの
    /// 明示的な opt-out）。未指定時は embed 失敗を fail closed で扱う。
    #[arg(long)]
    no_vectors: bool,
}

/// token 解決: 既定は --token-file（CLAUDE.md のローカル/GCE 起動手順と同じ経路）。
/// ファイルが無い/読めない場合のみ --token-env にフォールバックする。
fn read_token(args: &Args) -> Result<String> {
    if let Some(path) = &args.token_file {
        match fs::read_to_string(path) {
            Ok(body) => {
                let trimmed = body.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "token file unreadable; falling back to --token-env"
                );
            }
        }
    }
    let token = env::var(&args.token_env).with_context(|| {
        format!(
            "vegapunk bearer token not found (tried --token-file {:?} and env {})",
            args.token_file, args.token_env
        )
    })?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        anyhow::bail!("vegapunk bearer token env {} is empty", args.token_env);
    }
    Ok(trimmed.to_string())
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-success status for {url}"))?;
    resp.text()
        .await
        .with_context(|| format!("read response body {url}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    // vector metadata の timestamp_ms は run 開始時に 1 回だけ取得する。entry ごとに now を
    // 取ると同一 run 内で値がばらつき、決定性が失われる（2026-07-18 backend 契約）。
    let ingest_timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before unix epoch; cannot compute vector metadata timestamp_ms")?
        .as_millis()
        .to_string();
    let token = read_token(&args)?;

    // 汎用テンプレの name をテナント schema 名に差し替える（vegapunk は name 一致を要求）。
    let schema_yaml = cs_support_mcp::manual::schema_ids::with_schema_name(
        &fs::read_to_string(&args.schema_file)
            .with_context(|| format!("read schema file {}", args.schema_file.display()))?,
        &args.schema,
    )?;
    let lexicon = LexiconNormalizer::from_path(&args.lexicon_file)
        .with_context(|| format!("load signal lexicon {}", args.lexicon_file.display()))?;
    let top_url =
        Url::parse(&args.top_url).with_context(|| format!("parse top url {}", args.top_url))?;

    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await?;

    // 製品マスタは vegapunk の Product ノードが正本（Issue #6: KNOWN_MODELS 廃止）。
    // crawl を始める前に取得し、型番検出語彙（表層形 → product_key）を組み立てる。
    // limit は他の query_nodes 呼び出し（resolve_product 等）と揃えて 1000 とし、取りこぼしを
    // 検出できるようにする。
    const PRODUCT_QUERY_LIMIT: i32 = 1000;
    let product_nodes = client
        .query_nodes(&args.schema, "Product", Vec::new(), PRODUCT_QUERY_LIMIT)
        .await
        .context("query existing Product nodes (product master lookup)")?;
    if product_nodes.is_empty() {
        anyhow::bail!(
            "no Product nodes found in schema {}; the product master is empty — run \
             `ingest_products` first to seed it before running ingest_urtect",
            args.schema
        );
    }
    if product_nodes.len() as i32 == PRODUCT_QUERY_LIMIT {
        anyhow::bail!(
            "Product node query returned exactly the limit ({PRODUCT_QUERY_LIMIT}); this may \
             indicate silent truncation and an incomplete product master — aborting ingest \
             (raise the limit or investigate the product count in schema {})",
            args.schema
        );
    }
    let product_lexicon = build_product_lexicon(&product_nodes);

    // redirect は top_url と同一 scheme/host のみ追従する（nav 由来 URL が 3xx で
    // 外部ホストへ誘導された場合の意図しない外部フェッチ/SSRF を防ぐ）。
    let allowed_scheme = top_url.scheme().to_string();
    let allowed_host = top_url.host_str().unwrap_or_default().to_string();
    let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
        let same_origin = attempt.url().scheme() == allowed_scheme
            && attempt.url().host_str() == Some(allowed_host.as_str());
        if !same_origin {
            attempt.error("cross-origin redirect blocked (same-host policy)")
        } else if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    });
    let http = reqwest::Client::builder()
        .user_agent("cs-support-mcp/ingest_urtect")
        .redirect(redirect_policy)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build http client")?;

    // 1. top ページ fetch → nav からページ一覧を列挙する。
    // 2. 既存 ManualSection（差分 ingest の hash マップの元）を query_nodes の offset ページングで
    //    読み切る。旧実装は graph_snapshot(5000) の全件ロードに依存していたが、これは backend の
    //    5000 ノード上限で truncate → 差分/stale 判定破綻を招く。alarm.com を同一 schema に投入
    //    すると urtect 側の再 ingest まで巻き添えで破綻するため、書き込み側の 5000 依存を撤去した。
    //    既存の派生/親 edge（stale 検出用）は全件ロードせず、「変更のあった section 1 件ごと」に
    //    1-hop traverse で引く（`assert_no_stale_section_edges`。呼び出し回数はグラフ規模ではなく
    //    変更件数にスケールする）。ManualSection は DOC_KEY（doc-manual）配下だけを読むため、
    //    同一 schema に同居する alarm.com（alarmcom-）側の section は構造的に一切触らない。
    //    互いに依存しない読み取り（HTTP / gRPC）なので並行に発行する。
    const PAGE_SIZE: i32 = 1000;
    let (top_html, existing_sections) = tokio::try_join!(
        async {
            fetch(&http, top_url.as_str())
                .await
                .with_context(|| format!("fetch top url {top_url}"))
        },
        async {
            client
                .query_nodes_paged(
                    &args.schema,
                    KIND_SECTION,
                    vec![("doc_key", "eq", DOC_KEY)],
                    PAGE_SIZE,
                )
                .await
                .context("load existing doc-manual ManualSection nodes (diff + stale-edge check)")
        },
    )?;

    // top ページは 1 度だけ parse し、タイトル抽出と nav 列挙の両方で使い回す。
    let top_document = Html::parse_document(&top_html);
    let doc_title_raw = extract_title(&top_document);
    let doc_title = if doc_title_raw.is_empty() {
        "URTECT Manual".to_string()
    } else {
        doc_title_raw
    };
    let nav_entries = enumerate_nav_entries(&top_document, &top_url);
    let enumerated_url_count = nav_entries.len();
    // nav の入れ子構造が読めていない(全 entry が depth 0)場合、breadcrumb/parent_slug は
    // 実質フラットになる。ページが複数あるのに全 depth 0 なら要目視確認（レポートに出す）。
    let nav_hierarchy_flat = enumerated_url_count > 1 && nav_entries.iter().all(|e| e.depth == 0);

    // 差分 ingest 用の既存 hash マップ。query_nodes_paged が既に DOC_KEY 配下の ManualSection
    // だけを返すため（node_type=ManualSection + doc_key=DOC_KEY でフィルタ済み）、ここでの
    // 再フィルタは不要。同一 schema に同居する alarm.com 側 section は構造的に混ざらない。
    let existing_hash: HashMap<String, String> = existing_sections
        .iter()
        .filter_map(|n| {
            let key = n.attributes.get("section_key")?.clone();
            let hash = n.attributes.get("content_hash")?.clone();
            Some((key, hash))
        })
        .collect();

    // nav に現れた既存 section の追跡（fetch 失敗・空本文でも nav に居る限り slug は確定する）。
    // crawl 後、「既存にあるが今回 nav に居ない」section は削除/非公開化とみなし fail closed する
    // （backend に delete が無く、stale な本文と派生 edge が検索候補に残り続けるため）。
    let seen_slugs: HashSet<String> = nav_entries.iter().map(|e| section_slug(&e.url)).collect();
    let disappeared: Vec<&String> = existing_hash
        .keys()
        .filter(|k| !seen_slugs.contains(*k))
        .collect();
    if !disappeared.is_empty() {
        anyhow::bail!(
            "{} existing ManualSection(s) are no longer in the nav ({:?}); the backend exposes \
             no delete, so their stale content/edges would keep polluting search — recreate the \
             tenant schema and re-ingest from scratch",
            disappeared.len(),
            disappeared
        );
    }

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut ingested = 0usize;
    let mut skipped = 0usize;
    let mut fetch_failed: Vec<String> = Vec::new();
    let mut empty_content: Vec<String> = Vec::new();
    let mut zero_signal_sections: Vec<String> = Vec::new();
    let mut describes_by_model: HashMap<String, usize> = HashMap::new();
    // nav 入れ子 depth に沿った祖先スタック（breadcrumb/parent_slug の組み立てに使う）。
    let mut stack: Vec<StackEntry> = Vec::new();
    // embed 対象。今回 skip されず実際に ingest された section の (slug, body) のみを集める
    // （body は ManualSectionInput へ move される直前に clone する）。
    let mut section_bodies: Vec<(String, String)> = Vec::new();

    // 3-4. 各 URL: fetch → extract → normalize_body → content_hash。差分があれば ingest。
    for (idx, entry) in nav_entries.iter().enumerate() {
        let slug = section_slug(&entry.url);
        let html = match fetch(&http, &entry.url).await {
            Ok(html) => html,
            Err(err) => {
                // 既存 section の再検証が fetch 失敗で出来ない場合は fail closed
                // （「取得不能=変更なし」と黙って扱うと stale content/edge が残るため）。
                // 新規 section の失敗は stale を生まないので、記録して継続する。
                if existing_hash.contains_key(&slug) {
                    anyhow::bail!(
                        "fetch failed for existing section {slug} ({}): {err}; cannot verify \
                         staleness — aborting ingest (retry, or recreate the tenant schema)",
                        entry.url
                    );
                }
                tracing::warn!(
                    url = %entry.url,
                    error = %err,
                    "fetch failed for new section; recording and continuing crawl"
                );
                fetch_failed.push(entry.url.clone());
                continue;
            }
        };

        let (title, raw_body) = extract_main_text(&html);
        let body = normalize_body(&raw_body);

        // 200 だが本文が空(ログイン/同意/エラー画面や抽出失敗の可能性)。title の有無に
        // よらず body が空なら検索対象になり得ない。既存 section なら fetch 失敗と同じく
        // 再検証不能として fail closed、新規なら記録して継続（空 body を ingest しない）。
        if body.is_empty() {
            if existing_hash.contains_key(&slug) {
                anyhow::bail!(
                    "empty body for existing section {slug} ({}); cannot verify \
                     staleness — aborting ingest (retry, or recreate the tenant schema)",
                    entry.url
                );
            }
            tracing::warn!(
                url = %entry.url,
                "empty body after extraction; skipping (unexpected page content?)"
            );
            empty_content.push(entry.url.clone());
            continue;
        }

        // 親子: 現在の depth 以上を積み戻し、直近の浅い entry を親にする。
        while let Some(top) = stack.last() {
            if top.depth < entry.depth {
                break;
            }
            stack.pop();
        }
        let (parent_slug, breadcrumb) = match stack.last() {
            Some(parent) => (
                Some(parent.slug.clone()),
                format!("{} > {title}", parent.breadcrumb),
            ),
            None => (None, title.clone()),
        };
        stack.push(StackEntry {
            depth: entry.depth,
            slug: slug.clone(),
            breadcrumb: breadcrumb.clone(),
        });

        // 派生属性（型番検出・signal 検出）は skip 判定より前に計算する。ハッシュに
        // これらも含めることで、本文が変わらなくても nav 構造・lexicon・型番検出ロジックの
        // 変更が diff-ingest の skip 判定に反映される（本文だけを見ると変更を見逃す）。
        let product_models = detect_product_models(&body, &product_lexicon);
        let signal_values: Vec<String> = lexicon
            .normalize(&body)
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        // 各フィールドを区切り文字 \x1f で連結し、決定論的な複合ハッシュにする。
        // section_no は常に None（未実装）なので空文字列で固定する。
        // 一時 String はローカルに束縛してから借用する（temporary lifetime に依存しない）。
        let order_str = idx.to_string();
        let models_joined = product_models.join(",");
        let signals_joined = signal_values.join(",");
        let composite = [
            body.as_str(),                        // body（正規化済み）
            title.as_str(),                       // title
            breadcrumb.as_str(),                  // breadcrumb
            "",                                   // section_no（常に None）
            order_str.as_str(),                   // order
            parent_slug.as_deref().unwrap_or(""), // parent_slug
            entry.url.as_str(),                   // source_url
            models_joined.as_str(),               // 検出済み型番（DESCRIBES 辺のもと）
            signals_joined.as_str(),              // マッチ済み signal（MENTIONS_SIGNAL 辺のもと）
        ]
        .join("\u{1f}");
        let hash = content_hash(&composite);

        if existing_hash.get(&slug) == Some(&hash) {
            // 未変更: 子の breadcrumb/parent 継続のためスタックには積んだが、upsert はスキップする。
            skipped += 1;
            continue;
        }

        // 変更された既存 section について、旧 DESCRIBES / MENTIONS_SIGNAL / PARENT_OF の宛先が
        // 新しい派生集合・新しい親に全て含まれるか検証する。含まれない（= 除去が必要な）edge が
        // あれば fail closed（upsert は追加しかできず、stale edge が検索を誤らせるため）。
        // 既存 edge は graph_snapshot の全件ロードではなく、この section 1 件について 1-hop
        // traverse で引く（`assert_no_stale_section_edges`。5000 ノード上限を回避）。
        if existing_hash.contains_key(&slug) {
            let new_targets: HashSet<String> = product_models
                .iter()
                .map(|m| manual_node_id(&args.schema, KIND_PRODUCT, m))
                .chain(
                    signal_values
                        .iter()
                        .map(|s| manual_node_id(&args.schema, "Signal", s)),
                )
                .collect();
            assert_no_stale_section_edges(
                &client,
                &args.schema,
                &slug,
                &[("Product", "DESCRIBES"), ("Signal", "MENTIONS_SIGNAL")],
                &new_targets,
                parent_slug.as_deref(),
            )
            .await?;
        }

        for model in &product_models {
            *describes_by_model.entry(model.clone()).or_insert(0) += 1;
        }
        if signal_values.is_empty() {
            zero_signal_sections.push(slug.clone());
        }

        section_bodies.push((slug.clone(), body.clone()));
        let input = ManualSectionInput {
            slug,
            title,
            body,
            source_url: entry.url.clone(),
            breadcrumb,
            section_no: None,
            order: idx as i32,
            parent_slug,
            product_models,
            signal_values,
            // urtect の原文は日本語（Google Sites）なので英語原文予約は使わない。
            // source_lang は build_section_graph 側で従来どおり "ja" になる。
            body_original: None,
            original_hash: None,
            // urtect（Google Sites）は Concept 抽出の対象外（抽出は alarm.com 側の翻訳パスにのみ
            // ある）。concept_expansion_design.md の「スコープ外」節に明記済み。
            concept_keys: Vec::new(),
        };
        let build = build_section_graph(&args.schema, DOC_KEY, &input, &hash);
        nodes.extend(build.nodes);
        edges.extend(build.edges);
        ingested += 1;
    }

    // 5. build_document_node を集約する。Product ノードは ingest_products の責務
    // （Issue #6: KNOWN_MODELS 廃止）で、ここでは生成しない。
    nodes.push(build_document_node(
        &args.schema,
        DOC_KEY,
        &doc_title,
        top_url.as_str(),
        &chrono::Utc::now().to_rfc3339(),
    ));

    // 同一 id のノード重複を除去してから upsert する（Signal ノードは節ごとに生成されるため
    // 共通 signal が節数ぶん重複しやすい。冪等 upsert なので正しさには影響しないが、
    // gRPC ペイロードと ingest 時間を無駄に膨らませる）。順序は保持する。
    let mut seen_node_ids = HashSet::new();
    nodes.retain(|n| seen_node_ids.insert(n.id.clone()));

    // 5. embedding: 今回 ingest された section を embed し、一括 upsert する。Product の
    // embed/upsert は ingest_products の責務（Issue #6: KNOWN_MODELS 廃止）。
    // embed は fail closed（1 件でも失敗したらベクトル無しの中途半端な状態を作らず abort）。
    // `--no-vectors` は明示的な opt-out のみで、途中失敗の代替経路にはしない。
    //
    // node/edge upsert（content_hash を含む）より必ず先に実行する: ここで失敗して bail した
    // 場合、当該 section の content_hash はまだ古い値のまま vegapunk に残るため、次回の
    // 差分 ingest はその section を「変更あり」として再検出し、embedding も含めて再試行する。
    // 逆順（先に node/edge を確定 → 後で embed）だと、embed/vector upsert だけが失敗しても
    // content_hash は新しい値で確定してしまい、次回実行時に不変とみなされて section が
    // 永久に skip され、vector が無いまま取り残される（この一巻き戻し不能ギャップを避ける）。
    //
    // 既知のトレードオフ（Accepted Risk。task-8-report.md 参照）: vector upsert 成功後に
    // 続く node/edge upsert が失敗すると、新しい本文由来の vector が投入済みなのに
    // ManualSection の body/content_hash は旧状態のまま残る一時的な不整合が起き得る
    // （検索が新 vector 経由で旧本文の section を返す）。ただし次回再実行時は
    // content_hash 不一致により自動的に再試行・自己修復される（＝上記の永久欠落より軽微）。
    // proto には nodes/edges/vectors を単一 RPC でまとめる `UpsertGraph`（atomic、vector
    // 失敗時は node/edge を best-effort rollback）が既に存在する。将来的にはそちらへ
    // 一本化し、この一時不整合そのものを無くすことを検討する。
    let vectors_skipped = args.no_vectors;
    let upserted_vectors = if vectors_skipped {
        0
    } else {
        let mut entries: Vec<cs_support_mcp::vegapunk::VectorUpsertEntry> =
            Vec::with_capacity(section_bodies.len());

        // section_bodies はここ以降使わないため move で消費する。body は section 本文の
        // 全文で ingest 中最大級の String だが、embed 後も vector metadata の `text`
        // （embedding 元テキストそのもの）として必要なため、ここだけ 1 回 clone して残す
        // （section_items 側は embed_all に move する）。slug も zip 用に残す。
        let mut section_slugs: Vec<String> = Vec::with_capacity(section_bodies.len());
        let mut section_texts: Vec<String> = Vec::with_capacity(section_bodies.len());
        let section_items: Vec<(String, String)> = section_bodies
            .into_iter()
            .map(|(slug, body)| {
                let label = format!("section {slug}");
                section_slugs.push(slug);
                section_texts.push(body.clone());
                (label, body)
            })
            .collect();
        let section_vectors = embed_all(&client, section_items, EMBED_CONCURRENCY).await?;
        for ((slug, text), vector) in section_slugs
            .iter()
            .zip(section_texts.iter())
            .zip(section_vectors)
        {
            let id = cs_support_mcp::manual::schema_ids::manual_node_id(
                &args.schema,
                cs_support_mcp::manual::schema_ids::KIND_SECTION,
                slug,
            );
            // metadata は固定列マッピングで認識キーは node_id/text/source_type/timestamp_ms
            // のみ（2026-07-18 backend 契約）。旧キー node_type/section_key/doc_key は
            // backend に保存されない dead weight だったため削除した。id と
            // metadata.node_id の一致は vector_entry ヘルパが構造的に保証する。
            entries.push(vector_entry(
                id,
                vector,
                text,
                cs_support_mcp::manual::schema_ids::KIND_SECTION,
                &ingest_timestamp_ms,
            ));
        }

        let entries_len = entries.len();
        client
            .upsert_vectors(entries)
            .await
            .with_context(|| format!("upsert vectors (entries={entries_len})"))?
    };

    let graph = GraphBuild { nodes, edges };
    let expected_nodes = graph.nodes.len();
    let expected_edges = graph.edges.len();
    let (upserted_nodes, upserted_edges) = client.upsert_graph_low_level(graph).await?;

    // 6. URL 網羅率レポート。
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "top_url": top_url.as_str(),
            "enumerated_url_count": enumerated_url_count,
            "nav_hierarchy_flat": nav_hierarchy_flat,
            "ingested_manual_sections": ingested,
            "skipped_unchanged": skipped,
            "expected_nodes": expected_nodes,
            "expected_edges": expected_edges,
            "upserted_nodes": upserted_nodes,
            "upserted_edges": upserted_edges,
            "upserted_vectors": upserted_vectors,
            "vectors_skipped": vectors_skipped,
            "describes_by_model": describes_by_model,
            "sections_with_zero_signal_matches": zero_signal_sections,
            "fetch_failed_urls": fetch_failed,
            "empty_content_urls": empty_content,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_boilerplate_and_normalizes() {
        let html = r#"<html><head><title>SDカードが認識されない</title>
          <style>.x{color:red}</style></head>
          <body><nav>目次</nav><main><h1>SDカードが認識されない</h1><p>抜き差し。</p>
          <script>var Symbol=typeof window!=="undefined";function noise(){return 42}</script>
          <footer>マニュアルの内容や画面は予告なく変更になる場合があります</footer></main></body></html>"#;
        let (title, raw) = extract_main_text(html);
        let body = normalize_body(&raw);
        assert_eq!(title, "SDカードが認識されない");
        assert!(body.contains("抜き差し"));
        assert!(!body.contains("予告なく変更"));
        assert!(!body.contains("目次"));
        // inline JS/CSS を本文に取り込まない（Google Sites 汚染対策）
        assert!(!body.contains("Symbol"));
        assert!(!body.contains("noise"));
        assert!(!body.contains("color:red"));
    }

    // 型番検出・製品語彙のユニットテストは `manual::crawl` へ移設した
    // （build_product_lexicon / detect_product_models / contains_as_token の定義先）。
}
