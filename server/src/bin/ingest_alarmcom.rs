/// answers.alarm.com（MindTouch KB）マニュアルクローラ / ingest CLI。
///
/// 第 2 のマニュアルソース。設計・検証記録は
/// `docs/superpowers/specs/2026-07-22-ingest-alarmcom-design.md`（v2）を参照。v2 で
/// family-hub 選別・製品マスタ fail closed・英日 2 fetch は全廃し、以下の設計へ改めた:
///
/// - 対象記事は sitemap.xml 駆動で `/Customer` + `/Partner` 配下の**全 URL**（製品フィルタ無し）。
/// - **英語版のみ 1 fetch/記事**。`?mt-language=JA` の機械翻訳は使わず、
///   `translate::translate_and_extract`（Gemini Flash 3.6 `generateContent`）で自前翻訳する。
/// - 本文は完全 SSR。`#elm-main-content` 配下から抽出する。パンくずは `.mt-breadcrumbs`。
/// - **alarm.com は製品非依存**（DESCRIBES を張らない）。parent_slug は URL パス階層
///   （1 階層上のパスが今回のクロール対象に実在するか）から導出する。
/// - 翻訳と同じ 1 LLM パスで Concept を抽出し、`manual::concept` で正規化・fuzzy マージして
///   MENTIONS_CONCEPT 辺を張る（概念クエリ・記事横断 join の拠り所）。
/// - **記事単位でインクリメンタル upsert**（embed → vector upsert → node/edge upsert の順、
///   fail closed）。クラッシュ耐性のため、全記事を溜め込んでからの一括 upsert はしない。
/// - robots.txt の Crawl-delay=5 を守るため、全 HTTP リクエスト（sitemap 含む）を 5 秒以上
///   空けて逐次実行する。
use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    harness::signal::{LexiconNormalizer, SignalNormalizer},
    manual::{
        concept::{
            build_concept_node, build_mentions_concept_edge, merge_concept,
            restore_registry_from_nodes, ConceptRecord,
        },
        crawl::{extract_text_excluding_noise, normalize_body},
        ingest_model::{
            build_document_node, build_section_graph, content_hash, ManualSectionInput,
        },
        reingest::assert_no_stale_section_edges,
        schema_ids::{manual_node_id, section_slug, with_schema_name, KIND_CONCEPT, KIND_SECTION},
        vectors::vector_entry,
    },
    model::{GraphEdge, GraphNode},
    translate::{
        load_glossary, translate_and_extract, GeminiClient, GeminiConfig, Glossary,
        TranslationContext,
    },
    vegapunk::VegapunkClient,
};
use scraper::{ElementRef, Html, Selector};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::PathBuf,
    time::Duration,
};
use url::Url;

/// 差分 ingest の安定 doc key。urtect（`doc-manual`）とは別 document として共存する。
/// alarmcom の section slug は `alarmcom-` プレフィックスを持ち、この doc_key で snapshot を
/// 絞ることで urtect 側の section を一切触らないことを構造的に保証する。
const DOC_KEY: &str = "doc-alarmcom";

/// robots.txt の Crawl-delay。これ未満を指定されても切り上げる（規約遵守をコードで強制）。
const MIN_CRAWL_DELAY_SECS: u64 = 5;

/// site-structure 由来 skip（fetch 失敗・本文空・breadcrumb 欠落・翻訳失敗）の割合上限。これを
/// 超えたら bail する（サイト構造・抽出セレクタ変化の疑い）。**parent-not-ingested の cascade
/// skip はこの比率に含めない**（Warning 2: 空ハブ配下 subtree の巻き添え skip で誤発火するため。
/// 判定は `site_skip_bail` を参照）。
const MAX_SKIP_RATIO: f64 = 0.2;

/// MAX_SKIP_RATIO の判定を開始する最小処理件数。記事単位インクリメンタル upsert に伴い、
/// この閾値判定は「全件処理後に一括判定」ではなく「処理するたびに判定」する
/// （既存 upsert 済みデータを守りつつ、なるべく早く異常を検知して残りのクロールを止めるため）。
/// 件数が少ないうちは 1 件の失敗で比率が跳ね上がるため、最低サンプル数を設ける。
const MIN_SKIP_SAMPLE: usize = 20;

/// クロール対象を絞る URL パスプレフィックス（sitemap 全体のうち、この配下だけが対象）。
const TARGETED_PATH_PREFIXES: [&str; 2] = ["/Customer", "/Partner"];

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
    /// 翻訳 glossary（型番・製品名・専門語の訳ブレ防止）。`translate_and_extract` に渡す。
    #[arg(long, default_value = "data/glossary.json")]
    glossary_file: PathBuf,
    /// Gemini モデル ID（翻訳 + Concept 抽出、design spec 2026-07-22 で確定済み）。
    #[arg(long, default_value = "gemini-3.6-flash")]
    gemini_model: String,
    /// Gemini API のベース URL（`/models/{model}:generateContent` を末尾に補完する）。
    #[arg(
        long,
        default_value = "https://generativelanguage.googleapis.com/v1beta"
    )]
    gemini_api_base: String,
    /// Gemini API 呼び出しの timeout（秒）。
    #[arg(long, default_value_t = 30)]
    gemini_timeout_secs: u64,
    /// embed / upsert_vectors を一切呼ばずスキップする（ベクトル基盤未整備な環境向けの
    /// 明示的な opt-out）。未指定時は embed 失敗を fail closed で扱う。
    #[arg(long)]
    no_vectors: bool,
    #[arg(long, default_value = "https://answers.alarm.com")]
    base_url: String,
    /// sitemap URL。未指定時は `{base_url}/sitemap.xml` を実行時に補完する。
    #[arg(long)]
    sitemap_url: Option<String>,
    /// リクエスト間隔（秒）。robots.txt の Crawl-delay=5 を下回る値は 5 に切り上げる。
    #[arg(long, default_value_t = MIN_CRAWL_DELAY_SECS)]
    crawl_delay_secs: u64,
}

/// token 解決: 既定は --token-file、ファイルが無い/読めない場合のみ --token-env。
/// `ingest_urtect.rs` / `ingest_products.rs` の `read_token` と同じ挙動（Args の型が
/// 異なるため関数は複製する）。
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

/// robots.txt の Crawl-delay を守るための逐次スロットル付き fetch。直近リクエスト時刻からの
/// 経過が `delay` 未満なら不足分だけ sleep してから送出する。sitemap fetch を含む全リクエストで
/// 同じ `last_request` トラッカーを共有し、連続リクエスト開始間隔を必ず `delay` 以上に保つ。
async fn throttled_fetch(
    client: &reqwest::Client,
    url: &str,
    delay: Duration,
    last_request: &mut Option<tokio::time::Instant>,
) -> Result<String> {
    if let Some(prev) = *last_request {
        let elapsed = prev.elapsed();
        if elapsed < delay {
            tokio::time::sleep(delay - elapsed).await;
        }
    }
    *last_request = Some(tokio::time::Instant::now());
    fetch(client, url).await
}

/// URL のフラグメント/クエリを外し、末尾スラッシュを揃えた正準形（dedup キー）。
/// `ingest_urtect.rs` の同名関数と同じ考え方（URL 構造非依存なのでローカルに複製する）。
fn canonical_url(u: &Url) -> String {
    let mut c = u.clone();
    c.set_fragment(None);
    c.set_query(None);
    c.as_str().trim_end_matches('/').to_string()
}

/// canonical URL のパス（末尾スラッシュを揃えた形）。canonical_url はクエリ/フラグメントしか
/// 落とさないため、`u.path()` の trim で canonical パスと一致する。
fn canonical_path(u: &Url) -> String {
    u.path().trim_end_matches('/').to_string()
}

/// alarmcom の section slug（`alarmcom-` プレフィックスで urtect 側 slug と衝突させない）。
fn alarmcom_slug(url: &str) -> String {
    format!("alarmcom-{}", section_slug(url))
}

/// パスが `/Customer` または `/Partner` 配下か（境界一致。`/CustomerXYZ` のような
/// 前方一致誤爆を避けるため、完全一致か `/prefix/` で始まる場合のみ true とする）。
fn is_targeted_path(path: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    TARGETED_PATH_PREFIXES
        .iter()
        .any(|prefix| trimmed == *prefix || trimmed.starts_with(&format!("{prefix}/")))
}

/// sitemap 由来 URL を `/Customer` + `/Partner` 配下だけに絞る純関数。
fn filter_targeted_urls(urls: &[String]) -> Vec<String> {
    urls.iter()
        .filter(|raw| {
            Url::parse(raw)
                .map(|u| is_targeted_path(u.path()))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// クロール対象 URL 1 件（sitemap 出現順で確定。製品マスタ・ファミリーハブには一切依存しない）。
#[derive(Debug, Clone, PartialEq)]
struct CrawlTarget {
    /// canonical（英語版）URL。
    url: String,
    /// alarmcom section slug。
    slug: String,
    /// canonical パス（parent_slug 導出・処理順序決定に使う）。
    path: String,
    /// sitemap 出現順（フィルタ・重複排除後）の 0 始まり連番。
    order: i32,
}

/// フィルタ済み URL 一覧から、canonical 化・重複排除済みのクロール対象一覧を組み立てる。
/// 順序は入力の出現順を保つ（`order` フィールドの元になる）。
fn build_crawl_targets(urls: &[String]) -> Vec<CrawlTarget> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut targets = Vec::new();
    for raw in urls {
        let Ok(u) = Url::parse(raw) else {
            continue;
        };
        let canonical = canonical_url(&u);
        if !seen.insert(canonical.clone()) {
            continue;
        }
        targets.push(CrawlTarget {
            slug: alarmcom_slug(&canonical),
            path: canonical_path(&u),
            url: canonical,
            order: 0,
        });
    }
    for (idx, target) in targets.iter_mut().enumerate() {
        target.order = idx as i32;
    }
    targets
}

/// canonical パス → slug の対応表（parent_slug 導出用）。
fn build_path_slug_map(targets: &[CrawlTarget]) -> HashMap<String, String> {
    targets
        .iter()
        .map(|t| (t.path.clone(), t.slug.clone()))
        .collect()
}

/// URL パス階層の 1 つ上のパスを返す（`/a/b/c` → `/a/b`）。トップレベル（`/a`）や
/// 空パスには親が無いので None。この親パスが実際のクロール対象に存在するかどうかは
/// 呼び出し側が `build_path_slug_map` の結果と突き合わせて判定する
/// （sitemap 上に親ページが無いこともあるため、存在しない親は「親なし」= root section にする）。
fn parent_path_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    if parent.is_empty() {
        None
    } else {
        Some(parent.to_string())
    }
}

/// URL パスのセグメント数（`/a/b/c` → 3）。`depth_first_order` の並べ替えキーに使う。
fn segment_count(path: &str) -> usize {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .count()
}

/// `targets` をパス階層の浅い順（セグメント数昇順）に安定並べ替えした参照列を返す。
/// 親は常に子よりセグメント数が少ないため、この順序で処理すれば「子を処理する時点で
/// 親のクロール対象は必ず処理済み」が保証される（孤児 PARENT_OF 辺の判定に使う）。
/// 同じセグメント数の中では元の並び（sitemap 出現順）を保つ（安定ソート）。
fn depth_first_order(targets: &[CrawlTarget]) -> Vec<&CrawlTarget> {
    let mut ordered: Vec<&CrawlTarget> = targets.iter().collect();
    ordered.sort_by_key(|t| segment_count(&t.path));
    ordered
}

/// 子記事（`parent_slug = Some(parent)`）の親が「過去の ingest 実行で既に upsert 済み
/// （`existing_hash` に slug が存在）」または「今回の実行で既に ingest 済み
/// （`ingested_this_run` に slug が存在）」のいずれかであるかを判定する純関数。親なしは常に true。
///
/// どちらの集合にも無い場合、そのハブは今回も過去にも一度も upsert されていない ＝ 実在しない
/// 親ノードを指す孤児 PARENT_OF 辺を生成しうる状態なので false を返す（呼び出し側で子記事を
/// fetch 前に skip する）。
fn parent_is_known(
    parent_slug: Option<&str>,
    existing_hash: &HashMap<String, String>,
    ingested_this_run: &HashSet<String>,
) -> bool {
    match parent_slug {
        None => true,
        Some(parent) => existing_hash.contains_key(parent) || ingested_this_run.contains(parent),
    }
}

/// `#elm-main-content` 配下の本文テキスト（未正規化）。コンテナが無ければ空文字列。
fn extract_main_body(document: &Html) -> String {
    let selector = Selector::parse("#elm-main-content").expect("valid selector");
    match document.select(&selector).next() {
        Some(container) => extract_text_excluding_noise(container),
        None => String::new(),
    }
}

/// `.mt-breadcrumbs` の直接の子要素を DOM 順に列挙し、空でないテキストの列を返す。
/// 子要素のタグ名（li/a/span 等）は決め打ちしない（実サイトの正確な HTML を確認できないため、
/// 直接の子要素なら何でも拾う）。
fn breadcrumb_crumbs(document: &Html) -> Vec<String> {
    let selector = Selector::parse(".mt-breadcrumbs").expect("valid selector");
    let Some(container) = document.select(&selector).next() else {
        return Vec::new();
    };
    container
        .children()
        .filter_map(ElementRef::wrap)
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// crumb 列を ` > ` 結合した breadcrumb 文字列。
fn breadcrumb_string(crumbs: &[String]) -> String {
    crumbs.join(" > ")
}

/// breadcrumb の最後の crumb（= 現在ページ名）を title に採る。無ければ空文字列。
fn breadcrumb_title(crumbs: &[String]) -> String {
    crumbs.last().cloned().unwrap_or_default()
}

/// 1 記事分の upsert 対象一式。`commit_article` の引数を束ねる（借用フィールドは記事ごとの
/// String 再確保を避けるため。nodes/edges は所有権を移して upsert に渡す）。
struct ArticleCommit<'a> {
    schema: &'a str,
    slug: &'a str,
    body_ja: &'a str,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    vectors_skipped: bool,
    ingest_timestamp_ms: &'a str,
}

/// 1 記事分の embed/vector upsert → node upsert → edge upsert をこの順で実行する
/// （ingest_urtect と同じ fail-closed 順序: embed が先に失敗すれば content_hash は
/// 旧値のまま残り、次回の差分 ingest がこの section を再検出して再試行する。逆順だと
/// vector 欠落のまま content_hash だけ確定し、以後永久に skip される）。
async fn commit_article(
    client: &VegapunkClient,
    commit: ArticleCommit<'_>,
) -> Result<(i32, i32, i32)> {
    let ArticleCommit {
        schema,
        slug,
        body_ja,
        nodes,
        edges,
        vectors_skipped,
        ingest_timestamp_ms,
    } = commit;
    let upserted_vectors = if vectors_skipped {
        0
    } else {
        let vector = client
            .embed(body_ja)
            .await
            .with_context(|| format!("embed section {slug}"))?;
        let id = manual_node_id(schema, KIND_SECTION, slug);
        let entry = vector_entry(id, vector, body_ja, KIND_SECTION, ingest_timestamp_ms);
        client
            .upsert_vectors(vec![entry])
            .await
            .with_context(|| format!("upsert vector for section {slug}"))?
    };
    let upserted_nodes = client
        .upsert_nodes(nodes)
        .await
        .with_context(|| format!("upsert nodes for section {slug}"))?;
    let upserted_edges = client
        .upsert_edges(edges)
        .await
        .with_context(|| format!("upsert edges for section {slug}"))?;
    Ok((upserted_vectors, upserted_nodes, upserted_edges))
}

/// site-structure 由来の skip（fetch 失敗 / 本文空 / breadcrumb 欠落 / 翻訳失敗 など「サイトが
/// 変わった」ことを示す skip）だけで早期 bail 比率を判定する純関数。
///
/// **parent-not-ingested による cascade skip は分子にも分母にも入れない**（Warning 2）。
/// 空ハブが 1 件でもあると健全なサイトでも配下の subtree が丸ごと「親未 ingest」で skip され、
/// これを比率に混ぜると 4.8h の full run を途中 bail させ、しかも「selector が変わった」と
/// 誤誘導するため。呼び出し側は site-structure skip 件数だけを `site_skips` として渡す。
///
/// 返り値 `Some(ratio)` は「site 異常比率が閾値超過 = bail すべき」の意。サンプルが少ないうちの
/// 誤検知を避けるため `min_sample` 未満では常に `None`。
fn site_skip_bail(
    site_skips: usize,
    ingested: usize,
    skipped_unchanged: usize,
    min_sample: usize,
    max_ratio: f64,
) -> Option<f64> {
    let processed = ingested + skipped_unchanged + site_skips;
    if processed < min_sample {
        return None;
    }
    let ratio = site_skips as f64 / processed as f64;
    (ratio > max_ratio).then_some(ratio)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    // vector metadata の timestamp_ms は run 開始時に 1 回だけ取得する（ingest_urtect と同じ理由:
    // entry ごとに now を取ると同一 run 内で値がばらつき決定性が失われる）。
    let ingest_timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before unix epoch; cannot compute vector metadata timestamp_ms")?
        .as_millis()
        .to_string();
    let token = read_token(&args)?;

    // robots.txt Crawl-delay=5 未満の指定は 5 に切り上げる（規約遵守をコードで強制）。
    // 下限クランプは `.max()` で表現し、切り上げが起きたときだけ warn する。
    let crawl_delay_secs = args.crawl_delay_secs.max(MIN_CRAWL_DELAY_SECS);
    if crawl_delay_secs > args.crawl_delay_secs {
        tracing::warn!(
            requested = args.crawl_delay_secs,
            enforced = crawl_delay_secs,
            "requested crawl delay is below answers.alarm.com robots.txt Crawl-delay=5; \
             raising to 5s to honor the site's stated rate limit"
        );
    }
    let crawl_delay = Duration::from_secs(crawl_delay_secs);

    let base_url =
        Url::parse(&args.base_url).with_context(|| format!("parse base url {}", args.base_url))?;
    let sitemap_url = args
        .sitemap_url
        .clone()
        .unwrap_or_else(|| format!("{}/sitemap.xml", args.base_url.trim_end_matches('/')));

    let schema_yaml = with_schema_name(
        &fs::read_to_string(&args.schema_file)
            .with_context(|| format!("read schema file {}", args.schema_file.display()))?,
        &args.schema,
    )?;
    let lexicon = LexiconNormalizer::from_path(&args.lexicon_file)
        .with_context(|| format!("load signal lexicon {}", args.lexicon_file.display()))?;
    let glossary: Glossary = load_glossary(&args.glossary_file)
        .with_context(|| format!("load glossary {}", args.glossary_file.display()))?;
    // 翻訳はこの CLI の中心機能であり、鍵が無ければ何もできない。3,490 記事のクロールを
    // 何時間も走らせた後に毎記事で翻訳失敗するのを避けるため、クロール開始前に fail closed
    // で構築する（`GeminiClient::from_config` は鍵を解決できなければ Err を返す）。
    let gemini_config = GeminiConfig {
        model: args.gemini_model.clone(),
        api_base: args.gemini_api_base.clone(),
        timeout_secs: args.gemini_timeout_secs,
    };
    let gemini_client = GeminiClient::from_config(&gemini_config)
        .context("construct gemini client (check CS_SUPPORT_GEMINI_API_KEY)")?;

    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await?;

    // redirect は base_url と同一 scheme/host のみ追従（意図しない外部フェッチ/SSRF 防止）。
    let allowed_scheme = base_url.scheme().to_string();
    let allowed_host = base_url.host_str().unwrap_or_default().to_string();
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
        .user_agent("cs-support-mcp/ingest_alarmcom")
        .redirect(redirect_policy)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .context("build http client")?;

    // operator 指定の --sitemap-url も base_url と同一 origin に制限する（crawl 対象 URL は
    // filter_same_origin で守られているが、sitemap fetch 自体の SSRF 面も同じ境界で塞ぐ）。
    let sitemap_parsed =
        Url::parse(&sitemap_url).with_context(|| format!("parse sitemap url {sitemap_url}"))?;
    if sitemap_parsed.scheme() != base_url.scheme()
        || sitemap_parsed.host_str() != base_url.host_str()
    {
        anyhow::bail!(
            "--sitemap-url {sitemap_url} is not same-origin with --base-url {} (scheme+host must \
             match); refusing to fetch a cross-origin sitemap",
            args.base_url
        );
    }

    // 既存状態のロード（差分 hash マップ・parent title・Concept registry・stale edge 検出）は
    // graph_snapshot ではなく query_nodes の offset ページングで行う。graph_snapshot は max_nodes
    // に backend ハード上限 5000 があり、約 3,490 ManualSection + Concept 数千 + urtect +
    // Product/Signal の合算はこれを超えるため、2 回目以降の差分 re-ingest で truncate → stale
    // 判定破綻 → bail していた（Warning 1）。ページングなら任意件数へスケールする。
    //
    // ManualSection は DOC_KEY（alarmcom）配下だけを読む。この doc_key フィルタで urtect 側
    // section を絶対に触らないことを構造的に保証する。Concept は doc スコープを持たない共有
    // ノードなので全件読む。sitemap fetch（HTTP）と 2 本の query_nodes（gRPC）は互いに依存
    // しないので並行に発行する。
    const PAGE_SIZE: i32 = 1000;
    let (sitemap_xml, existing_sections, existing_concepts) = tokio::try_join!(
        async {
            fetch(&http, &sitemap_url)
                .await
                .with_context(|| format!("fetch sitemap {sitemap_url}"))
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
                .context("load existing alarmcom ManualSection nodes (diff + parent context)")
        },
        async {
            client
                .query_nodes_paged(&args.schema, KIND_CONCEPT, Vec::new(), PAGE_SIZE)
                .await
                .context("load existing Concept nodes (registry restore)")
        },
    )?;
    // sitemap fetch をスロットルトラッカーの起点として記録する（後続の記事 fetch はここから
    // crawl_delay 以上空ける）。
    let mut last_request: Option<tokio::time::Instant> = Some(tokio::time::Instant::now());

    let sitemap_urls_raw = parse_sitemap_urls(&sitemap_xml);
    if sitemap_urls_raw.is_empty() {
        anyhow::bail!(
            "sitemap {sitemap_url} yielded no <loc> URLs; the sitemap format may have changed \
             or the fetch returned an error page — aborting ingest"
        );
    }
    // ハブ判定・実フェッチより前に、base_url と同一 origin の URL だけへ絞る（外部ホストへの
    // 意図しないフェッチ = SSRF/資格情報漏洩の経路を crawl 前に閉じる）。
    let same_origin_urls = filter_same_origin(&sitemap_urls_raw, &base_url);
    let off_origin_dropped = sitemap_urls_raw.len() - same_origin_urls.len();
    if off_origin_dropped > 0 {
        tracing::warn!(
            dropped = off_origin_dropped,
            base_url = %args.base_url,
            "dropped off-origin sitemap URLs before crawl; only URLs matching base_url's \
             scheme+host are crawled (external hosts are never fetched)"
        );
    }

    // R1: 製品フィルタ無しで /Customer + /Partner 配下の全 URL を対象にする。
    let targeted_urls = filter_targeted_urls(&same_origin_urls);
    let off_prefix_dropped = same_origin_urls.len() - targeted_urls.len();
    if targeted_urls.is_empty() {
        anyhow::bail!(
            "none of the {} same-origin sitemap URL(s) fall under {:?}; the sitemap layout may \
             have changed — aborting ingest",
            same_origin_urls.len(),
            TARGETED_PATH_PREFIXES
        );
    }

    let targets = build_crawl_targets(&targeted_urls);
    if targets.is_empty() {
        anyhow::bail!(
            "no crawl targets derived from sitemap {sitemap_url} after filtering; \
             aborting ingest"
        );
    }
    let path_slug_map = build_path_slug_map(&targets);
    let target_count = targets.len();

    // 差分 ingest 用の既存 hash マップ + parent 文脈用の title マップ。query_nodes_paged が
    // 既に DOC_KEY 配下の ManualSection だけを返すため、ここでの再フィルタは不要
    // （urtect 側 section は構造的に混ざらない）。
    let mut existing_hash: HashMap<String, String> = HashMap::new();
    let mut titles_by_slug: HashMap<String, String> = HashMap::new();
    for n in &existing_sections {
        let Some(key) = n.attributes.get("section_key").cloned() else {
            continue;
        };
        if let Some(hash) = n.attributes.get("content_hash") {
            existing_hash.insert(key.clone(), hash.clone());
        }
        if let Some(title) = n.attributes.get("title") {
            titles_by_slug.insert(key, title.clone());
        }
    }
    // Concept registry を過去の ingest 実行から復元する（差分 ingest をまたいだ fuzzy マージ）。
    let mut concept_registry: Vec<ConceptRecord> = restore_registry_from_nodes(&existing_concepts);

    // 既存の派生/親 edge は graph_snapshot の全件ロードではなく、変更のあった既存 section 1 件
    // ごとに query_nodes の 1-hop traverse で引く（stale 検出用。`traverse_neighbor_ids` を参照）。
    // これにより 5000 ノード上限に縛られず、呼び出し回数はグラフ規模ではなく再 ingest での
    // 変更件数にスケールする。実際の traverse はループ内で lazy に発行する。

    // disappeared 検出: 既存 DOC_KEY section のうち今回のクロール対象一覧に無いものは削除/非公開化
    // とみなし fail closed（backend に delete が無く、stale な本文/edge が検索候補に残り続ける）。
    let target_slugs: HashSet<String> = targets.iter().map(|t| t.slug.clone()).collect();
    let disappeared: Vec<&String> = existing_hash
        .keys()
        .filter(|k| !target_slugs.contains(*k))
        .collect();
    if !disappeared.is_empty() {
        anyhow::bail!(
            "{} existing alarmcom ManualSection(s) are no longer crawl targets ({:?}); the backend \
             exposes no delete, so their stale content/edges would keep polluting search — recreate \
             the tenant schema and re-ingest from scratch",
            disappeared.len(),
            disappeared
        );
    }

    // document ノード（Alarm.com Answers）を最初に確定させる。各記事の HAS_SECTION 辺は
    // このノードを参照するため、記事単位インクリメンタル upsert を始める前に単独で upsert する。
    let fetched_at = chrono::Utc::now().to_rfc3339();
    let doc_node = build_document_node(
        &args.schema,
        DOC_KEY,
        "Alarm.com Answers",
        &args.base_url,
        &fetched_at,
    );
    let upserted_doc_nodes = client.upsert_nodes(vec![doc_node]).await?;

    let mut ingested = 0usize;
    let mut skipped_unchanged = 0usize;
    // site-structure 由来の skip（fetch 失敗 / 本文空 / breadcrumb 欠落 / 翻訳失敗）。早期 bail
    // 比率の分子はこれだけ（Warning 2）。
    let mut skip_records: Vec<(String, String)> = Vec::new();
    // parent-not-ingested による cascade skip。空ハブ配下の subtree が丸ごと入りうるため
    // site-structure skip とは別勘定にし、bail 比率には一切含めない（記録は残す）。
    let mut parent_skip_records: Vec<(String, String)> = Vec::new();
    let mut zero_signal_sections: Vec<String> = Vec::new();
    let mut ingested_this_run: HashSet<String> = HashSet::new();
    let mut total_nodes_upserted = upserted_doc_nodes;
    let mut total_edges_upserted = 0i32;
    let mut total_vectors_upserted = 0i32;
    let mut concepts_new_count = 0usize;
    let mut concept_mentions_total = 0usize;
    let vectors_skipped = args.no_vectors;

    for target in depth_first_order(&targets) {
        let slug = target.slug.clone();

        let parent_slug = parent_path_of(&target.path).and_then(|p| path_slug_map.get(&p).cloned());

        // 親がこれまでも今回も一度も ingest されていない子記事は孤児 PARENT_OF 辺を生む
        // ため、fetch する前に skip する。
        if !parent_is_known(parent_slug.as_deref(), &existing_hash, &ingested_this_run) {
            let parent = parent_slug.as_deref().unwrap_or("");
            tracing::warn!(
                url = %target.url,
                parent_slug = %parent,
                "parent article was never ingested (fetch/extraction/translation failed or not yet \
                 upserted); skipping child article to avoid a PARENT_OF edge pointing at a \
                 non-existent node"
            );
            // site-structure skip とは別勘定（Warning 2: bail 比率に混ぜない）。
            parent_skip_records.push((
                target.url.clone(),
                format!("parent {parent} not ingested (skipped to avoid orphan PARENT_OF edge)"),
            ));
            continue;
        }

        let en_html = match throttled_fetch(&http, &target.url, crawl_delay, &mut last_request)
            .await
        {
            Ok(html) => html,
            Err(err) => {
                tracing::warn!(url = %target.url, error = %err, "english fetch failed; skipping article");
                skip_records.push((target.url.clone(), format!("fetch failed: {err}")));
                continue;
            }
        };
        let en_document = Html::parse_document(&en_html);
        let body_en = normalize_body(&extract_main_body(&en_document));
        if body_en.is_empty() {
            tracing::warn!(url = %target.url, "body empty after extraction; skipping article");
            skip_records.push((
                target.url.clone(),
                "body empty after extraction".to_string(),
            ));
            continue;
        }
        let crumbs = breadcrumb_crumbs(&en_document);
        let breadcrumb = breadcrumb_string(&crumbs);
        let title_en = breadcrumb_title(&crumbs);
        if title_en.is_empty() {
            tracing::warn!(
                url = %target.url,
                "breadcrumb title empty (.mt-breadcrumbs absent or last crumb empty); skipping article"
            );
            skip_records.push((target.url.clone(), "breadcrumb title empty".to_string()));
            continue;
        }

        // content_hash / en_hash は英語原文（source of truth）のみで駆動する。差分 ingest は
        // 英語原文の変化だけを見る。英語が不変なら Gemini（translate_and_extract）は呼ばない。
        let hash = content_hash(&body_en);
        if existing_hash.get(&slug) == Some(&hash) {
            skipped_unchanged += 1;
            continue;
        }

        let context = TranslationContext {
            breadcrumb: breadcrumb.clone(),
            parent_title: parent_slug
                .as_deref()
                .and_then(|p| titles_by_slug.get(p).cloned()),
        };
        let translation = match translate_and_extract(
            &gemini_client,
            &title_en,
            &body_en,
            &context,
            &glossary,
        )
        .await
        {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(url = %target.url, error = %err, "translation failed; skipping article");
                skip_records.push((target.url.clone(), format!("translation failed: {err}")));
                continue;
            }
        };
        if translation.body_ja.trim().is_empty() {
            tracing::warn!(url = %target.url, "translated body_ja empty; skipping article");
            skip_records.push((target.url.clone(), "translated body_ja empty".to_string()));
            continue;
        }
        if translation.title_ja.trim().is_empty() {
            tracing::warn!(url = %target.url, "translated title_ja empty; skipping article");
            skip_records.push((target.url.clone(), "translated title_ja empty".to_string()));
            continue;
        }

        // MENTIONS_SIGNAL は既存 lexicon 抽出を日本語本文に適用する。alarm.com は product
        // 非依存なので product_models は常に空（DESCRIBES を張らない）。
        let signal_values: Vec<String> = lexicon
            .normalize(&translation.body_ja)
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();

        // Concept 抽出結果を registry へマージし、この記事が言及する concept_key の集合を作る
        // （同一記事内の重複抽出は 1 辺に畳む）。
        let mut concept_keys: Vec<String> = Vec::new();
        for extract in &translation.concepts {
            let before_len = concept_registry.len();
            if let Some(key) = merge_concept(&mut concept_registry, extract) {
                if concept_registry.len() > before_len {
                    concepts_new_count += 1;
                }
                if !concept_keys.contains(&key) {
                    concept_keys.push(key);
                }
            } else {
                tracing::warn!(
                    url = %target.url,
                    name_en = %extract.name_en,
                    "concept name_en normalizes to empty; skipping this concept mention"
                );
            }
        }

        // 変更された既存 section の stale 派生/親 edge 検出（backend に delete が無く、除去が
        // 必要な edge が残ると検索を誤らせるため fail closed）。既存 edge は graph_snapshot の
        // 全件ロードではなく、この section 1 件について query_nodes の 1-hop traverse で引く
        // （Warning 1: 5000 ノード上限回避）。urtect と共有する `assert_no_stale_section_edges`
        // に委譲する（呼び出しは「既存かつ内容が変わった section」だけ通る。上の未変更 skip 済み）。
        // DESCRIBES を含めるのは v1 時代の残存 DESCRIBES を「除去が必要な stale edge」として
        // 検出するため（v2 alarm.com は product 非依存で新規 DESCRIBES を張らない）。
        if existing_hash.contains_key(&slug) {
            let new_targets: HashSet<String> = signal_values
                .iter()
                .map(|s| manual_node_id(&args.schema, "Signal", s))
                .chain(
                    concept_keys
                        .iter()
                        .map(|k| manual_node_id(&args.schema, "Concept", k)),
                )
                .collect();
            assert_no_stale_section_edges(
                &client,
                &args.schema,
                &slug,
                &[
                    ("Product", "DESCRIBES"),
                    ("Signal", "MENTIONS_SIGNAL"),
                    (KIND_CONCEPT, "MENTIONS_CONCEPT"),
                ],
                &new_targets,
                parent_slug.as_deref(),
            )
            .await?;
        }

        if signal_values.is_empty() {
            zero_signal_sections.push(slug.clone());
        }

        let input = ManualSectionInput {
            slug: slug.clone(),
            title: translation.title_ja.clone(),
            body: translation.body_ja.clone(),
            source_url: target.url.clone(),
            breadcrumb,
            section_no: None,
            order: target.order,
            parent_slug: parent_slug.clone(),
            product_models: Vec::new(),
            signal_values,
            body_original: Some(body_en),
            original_hash: Some(hash.clone()),
        };
        let build = build_section_graph(&args.schema, DOC_KEY, &input, &hash);
        let mut article_nodes = build.nodes;
        let mut article_edges = build.edges;
        concept_mentions_total += concept_keys.len();
        for key in &concept_keys {
            if let Some(record) = concept_registry.iter().find(|r| &r.concept_key == key) {
                article_nodes.push(build_concept_node(&args.schema, record));
            }
            article_edges.push(build_mentions_concept_edge(&args.schema, &slug, key));
        }
        let mut seen_node_ids = HashSet::new();
        article_nodes.retain(|n| seen_node_ids.insert(n.id.clone()));

        let (upserted_vectors, upserted_nodes, upserted_edges) = commit_article(
            &client,
            ArticleCommit {
                schema: &args.schema,
                slug: &slug,
                body_ja: &input.body,
                nodes: article_nodes,
                edges: article_edges,
                vectors_skipped,
                ingest_timestamp_ms: &ingest_timestamp_ms,
            },
        )
        .await?;
        total_vectors_upserted += upserted_vectors;
        total_nodes_upserted += upserted_nodes;
        total_edges_upserted += upserted_edges;
        ingested += 1;
        ingested_this_run.insert(slug.clone());
        titles_by_slug.insert(slug, input.title.clone());

        // 安全弁: 記事単位インクリメンタル upsert に伴い、全件処理後ではなく処理するたびに
        // skip 比率を判定する（既に upsert 済みの記事は守りつつ、なるべく早く異常を検知して
        // 残りのクロールを止める）。parent-not-ingested の cascade skip は分子・分母とも除外
        // （Warning 2: 空ハブ配下 subtree の巻き添えで誤発火するため。判定は site_skip_bail）。
        if let Some(running_ratio) = site_skip_bail(
            skip_records.len(),
            ingested,
            skipped_unchanged,
            MIN_SKIP_SAMPLE,
            MAX_SKIP_RATIO,
        ) {
            let processed_so_far = ingested + skipped_unchanged + skip_records.len();
            anyhow::bail!(
                "site-structure skips {}/{processed_so_far} articles fetched so far ({:.1}%), \
                 exceeding the {:.0}% safety threshold; the site structure or extraction selector \
                 (#elm-main-content / .mt-breadcrumbs) may have changed — aborting before \
                 crawling further (this ratio excludes {} parent-not-ingested cascade skips; \
                 articles already upserted this run remain committed; inspect the skipped URLs)",
                skip_records.len(),
                running_ratio * 100.0,
                MAX_SKIP_RATIO * 100.0,
                parent_skip_records.len()
            );
        }
    }

    let skipped_fetch_or_empty = skip_records.len();
    let parent_skipped = parent_skip_records.len();
    // skip_ratio は「fetch を試みた記事に対する site-structure skip の割合」。分母から
    // parent-not-ingested の cascade skip（fetch 未試行）を除く（Warning 2 と一貫させる）。
    let fetch_attempted = target_count.saturating_sub(parent_skipped);
    let skip_ratio = if fetch_attempted == 0 {
        0.0
    } else {
        skipped_fetch_or_empty as f64 / fetch_attempted as f64
    };

    let fetch_or_empty_skipped_urls: Vec<serde_json::Value> = skip_records
        .iter()
        .map(|(url, reason)| json!({ "url": url, "reason": reason }))
        .collect();
    let parent_skipped_urls: Vec<serde_json::Value> = parent_skip_records
        .iter()
        .map(|(url, reason)| json!({ "url": url, "reason": reason }))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "base_url": args.base_url,
            "sitemap_url": sitemap_url,
            "sitemap_url_count": sitemap_urls_raw.len(),
            "off_origin_urls_dropped": off_origin_dropped,
            "off_targeted_prefix_urls_dropped": off_prefix_dropped,
            "crawl_target_count": target_count,
            "ingested_manual_sections": ingested,
            "skipped_unchanged": skipped_unchanged,
            "skipped_fetch_or_empty": skipped_fetch_or_empty,
            "skipped_parent_not_ingested": parent_skipped,
            "fetch_attempted": fetch_attempted,
            "skip_ratio": skip_ratio,
            "upserted_nodes": total_nodes_upserted,
            "upserted_edges": total_edges_upserted,
            "upserted_vectors": total_vectors_upserted,
            "vectors_skipped": vectors_skipped,
            "concepts_created": concepts_new_count,
            "concept_mentions": concept_mentions_total,
            "sections_with_zero_signal_matches": zero_signal_sections,
            "fetch_or_empty_skipped_urls": fetch_or_empty_skipped_urls,
            "parent_skipped_urls": parent_skipped_urls,
        }))?
    );
    Ok(())
}

/// sitemap XML から `<loc>` の URL を出現順で全て抽出する純関数。
/// 新規 XML クレートを足さず、既存依存の scraper（html5ever）で `<loc>` を CSS セレクタで拾う。
fn parse_sitemap_urls(xml: &str) -> Vec<String> {
    let document = Html::parse_document(xml);
    let loc_selector = Selector::parse("loc").expect("valid selector");
    document
        .select(&loc_selector)
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// sitemap 由来 URL を base_url と同一 origin（scheme + host）のものだけに絞る純関数。
///
/// sitemap.xml 自体は answers.alarm.com から取得する信頼済みソースだが、そこに紛れ込んだ
/// （改竄・設定ミス由来の）外部 URL を実フェッチしてしまうと、Cloud Run 環境では
/// サービスアカウント資格情報の漏洩につながりうる（例: cloud metadata endpoint）。既存の
/// redirect policy は「リダイレクト」にしか効かず**初期 fetch 先 URL 自体**の origin を
/// 検証しないため、クロール対象確定より前のこの段階で同じ「同一 scheme/host のみ」境界を課す。
/// パース不能な URL も除外する。
fn filter_same_origin(urls: &[String], base_url: &Url) -> Vec<String> {
    urls.iter()
        .filter(|raw| match Url::parse(raw) {
            Ok(u) => u.scheme() == base_url.scheme() && u.host_str() == base_url.host_str(),
            Err(_) => false,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_skip_bail_ignores_parent_skips_and_respects_min_sample() {
        // 分子・分母は site-structure skip / (ingested + unchanged + site skips) のみ。
        // parent-not-ingested の cascade skip は引数に現れない ＝ 構造的に比率へ寄与しない
        // （Warning 2 の核心）。健全なサイトの例: 空ハブ 1 件配下で 100 件が親未 ingest で
        // skip されても、実 fetch した 30 件中 site skip が 2 件なら bail しない。
        assert_eq!(
            site_skip_bail(2, 25, 3, 20, 0.2),
            None,
            "6.7% site-skip must not bail even alongside many parent-cascade skips"
        );
        // site-structure skip が閾値超過なら Some（bail）。
        assert!(
            site_skip_bail(10, 20, 0, 20, 0.2).is_some(),
            "33% site-skip must bail"
        );
        // サンプル不足（processed < min_sample）では常に None。
        assert_eq!(
            site_skip_bail(5, 2, 0, 20, 0.2),
            None,
            "below min_sample must never bail regardless of ratio"
        );
        // ちょうど閾値は超過ではないので None（> 判定）。
        assert_eq!(site_skip_bail(4, 16, 0, 20, 0.2), None);
    }

    #[test]
    fn parse_sitemap_extracts_loc_urls_in_order() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://answers.alarm.com/Partner/A</loc><lastmod>2026-07-01</lastmod></url>
  <url><loc>https://answers.alarm.com/Partner/B</loc><lastmod>2026-07-02</lastmod></url>
  <url><loc>https://answers.alarm.com/Customer/C</loc></url>
</urlset>"#;
        let urls = parse_sitemap_urls(xml);
        assert_eq!(
            urls,
            vec![
                "https://answers.alarm.com/Partner/A".to_string(),
                "https://answers.alarm.com/Partner/B".to_string(),
                "https://answers.alarm.com/Customer/C".to_string(),
            ]
        );
    }

    #[test]
    fn filter_same_origin_keeps_only_base_scheme_and_host() {
        let base = Url::parse("https://answers.alarm.com").unwrap();
        let urls = vec![
            "https://answers.alarm.com/Partner/A".to_string(),
            // cloud metadata endpoint（host も scheme も違う）は除外される
            "http://169.254.169.254/latest/meta-data/foo".to_string(),
            "https://evil.example.com/Partner/A".to_string(),
            "http://answers.alarm.com/Partner/A".to_string(),
            "not a url".to_string(),
            "https://answers.alarm.com/Customer/B".to_string(),
        ];
        let filtered = filter_same_origin(&urls, &base);
        assert_eq!(
            filtered,
            vec![
                "https://answers.alarm.com/Partner/A".to_string(),
                "https://answers.alarm.com/Customer/B".to_string(),
            ]
        );
    }

    #[test]
    fn filter_targeted_urls_keeps_only_customer_and_partner_paths() {
        let urls = vec![
            "https://answers.alarm.com/Partner/A".to_string(),
            "https://answers.alarm.com/Customer/B".to_string(),
            "https://answers.alarm.com/About/Contact".to_string(),
            // 境界一致（前方一致誤爆を避ける）
            "https://answers.alarm.com/CustomerService/X".to_string(),
        ];
        let filtered = filter_targeted_urls(&urls);
        assert_eq!(
            filtered,
            vec![
                "https://answers.alarm.com/Partner/A".to_string(),
                "https://answers.alarm.com/Customer/B".to_string(),
            ]
        );
    }

    #[test]
    fn is_targeted_path_matches_exact_and_nested_but_not_prefix_collision() {
        assert!(is_targeted_path("/Customer"));
        assert!(is_targeted_path("/Customer/"));
        assert!(is_targeted_path("/Partner/Video/A"));
        assert!(!is_targeted_path("/CustomerService"));
        assert!(!is_targeted_path("/About"));
    }

    #[test]
    fn build_crawl_targets_canonicalizes_dedupes_and_orders() {
        let urls = vec![
            "https://answers.alarm.com/Partner/A/".to_string(),
            "https://answers.alarm.com/Partner/A".to_string(), // 末尾スラッシュ違いの重複
            "https://answers.alarm.com/Partner/A/B".to_string(),
        ];
        let targets = build_crawl_targets(&urls);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].url, "https://answers.alarm.com/Partner/A");
        assert_eq!(targets[0].order, 0);
        assert_eq!(targets[1].url, "https://answers.alarm.com/Partner/A/B");
        assert_eq!(targets[1].order, 1);
    }

    #[test]
    fn parent_path_of_derives_one_level_up() {
        assert_eq!(
            parent_path_of("/Partner/A/B"),
            Some("/Partner/A".to_string())
        );
        assert_eq!(parent_path_of("/Partner/A"), Some("/Partner".to_string()));
        assert_eq!(parent_path_of("/Partner"), None);
        assert_eq!(parent_path_of(""), None);
    }

    #[test]
    fn build_path_slug_map_and_parent_lookup_yields_none_when_parent_not_crawled() {
        let urls = vec![
            "https://answers.alarm.com/Partner/A".to_string(),
            "https://answers.alarm.com/Partner/A/B".to_string(),
            // C の親 "/Partner/Missing" は sitemap に無い → 親なし扱い
            "https://answers.alarm.com/Partner/Missing/C".to_string(),
        ];
        let targets = build_crawl_targets(&urls);
        let map = build_path_slug_map(&targets);
        let b = &targets[1];
        let parent_of_b = parent_path_of(&b.path).and_then(|p| map.get(&p).cloned());
        assert_eq!(parent_of_b, Some(targets[0].slug.clone()));

        let c = &targets[2];
        let parent_of_c = parent_path_of(&c.path).and_then(|p| map.get(&p).cloned());
        assert_eq!(parent_of_c, None);
    }

    #[test]
    fn segment_count_counts_non_empty_segments() {
        assert_eq!(segment_count("/Partner"), 1);
        assert_eq!(segment_count("/Partner/A"), 2);
        assert_eq!(segment_count("/Partner/A/B/"), 3);
        assert_eq!(segment_count(""), 0);
    }

    #[test]
    fn depth_first_order_processes_shallower_paths_before_deeper_ones() {
        let urls = vec![
            "https://x/a/b/c".to_string(), // depth 3, sitemap 出現順 0
            "https://x/a".to_string(),     // depth 1, sitemap 出現順 1
            "https://x/a/b".to_string(),   // depth 2, sitemap 出現順 2
        ];
        let targets = build_crawl_targets(&urls);
        let ordered = depth_first_order(&targets);
        let paths: Vec<&str> = ordered.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(paths, vec!["/a", "/a/b", "/a/b/c"]);
    }

    #[test]
    fn depth_first_order_preserves_relative_order_within_same_depth() {
        let urls = vec![
            "https://x/b".to_string(),
            "https://x/a".to_string(),
            "https://x/c".to_string(),
        ];
        let targets = build_crawl_targets(&urls);
        let ordered = depth_first_order(&targets);
        // 同じ depth(1) 同士は sitemap 出現順（b, a, c）を保つ。
        let paths: Vec<&str> = ordered.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(paths, vec!["/b", "/a", "/c"]);
    }

    #[test]
    fn parent_is_known_covers_all_branches() {
        let mut existing_hash = HashMap::new();
        existing_hash.insert("hub-existing".to_string(), "hash1".to_string());
        let mut ingested_this_run = HashSet::new();
        ingested_this_run.insert("hub-this-run".to_string());

        assert!(parent_is_known(None, &existing_hash, &ingested_this_run));
        assert!(parent_is_known(
            Some("hub-existing"),
            &existing_hash,
            &ingested_this_run
        ));
        assert!(parent_is_known(
            Some("hub-this-run"),
            &existing_hash,
            &ingested_this_run
        ));
        assert!(!parent_is_known(
            Some("hub-never-ingested"),
            &existing_hash,
            &ingested_this_run
        ));
    }

    #[test]
    fn alarmcom_slug_is_prefixed_and_stable() {
        let url = "https://answers.alarm.com/Partner/Video_Devices/1080p_Outdoor_Camera_ADC-V724";
        let slug = alarmcom_slug(url);
        assert!(slug.starts_with("alarmcom-sec-"));
        assert_eq!(slug, alarmcom_slug(url));
        assert_ne!(slug, section_slug(url));
    }

    /// `#elm-main-content` 本文と `.mt-breadcrumbs` を含む最小 HTML fixture（英語版）。
    const SAMPLE_ARTICLE_HTML: &str = r#"<html><body>
      <div class="mt-breadcrumbs">
        <span><a href="/Partner">Partner</a></span>
        <span><a href="/Partner/Video">Video Devices</a></span>
        <span class="mt-breadcrumbs-current-page">Wi-Fi Setup</span>
      </div>
      <article class="elm-content-container" id="elm-main-content">
        <nav>breadcrumb nav noise</nav>
        <h1>Wi-Fi Setup</h1>
        <p>Connect the camera to Wi-Fi.</p>
        <script>var noise = 1;</script>
        <style>.x{color:red}</style>
      </article>
    </body></html>"#;

    #[test]
    fn extract_main_body_collects_content_and_excludes_noise() {
        let document = Html::parse_document(SAMPLE_ARTICLE_HTML);
        let body = normalize_body(&extract_main_body(&document));
        assert!(body.contains("Wi-Fi Setup"));
        assert!(body.contains("Connect the camera to Wi-Fi."));
        assert!(!body.contains("breadcrumb nav noise"));
        assert!(!body.contains("noise"));
        assert!(!body.contains("color:red"));
    }

    #[test]
    fn extract_main_body_empty_when_container_absent() {
        let document = Html::parse_document("<html><body><p>body</p></body></html>");
        assert_eq!(extract_main_body(&document), "");
    }

    #[test]
    fn breadcrumb_crumbs_lists_direct_children_and_title_is_last() {
        let document = Html::parse_document(SAMPLE_ARTICLE_HTML);
        let crumbs = breadcrumb_crumbs(&document);
        assert_eq!(crumbs, vec!["Partner", "Video Devices", "Wi-Fi Setup"]);
        assert_eq!(
            breadcrumb_string(&crumbs),
            "Partner > Video Devices > Wi-Fi Setup"
        );
        assert_eq!(breadcrumb_title(&crumbs), "Wi-Fi Setup");
    }

    #[test]
    fn breadcrumb_absent_yields_empty() {
        let document = Html::parse_document("<html><body><p>body</p></body></html>");
        assert!(breadcrumb_crumbs(&document).is_empty());
        assert_eq!(breadcrumb_title(&breadcrumb_crumbs(&document)), "");
    }
}
