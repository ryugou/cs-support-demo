/// answers.alarm.com（MindTouch KB）マニュアルクローラ / ingest CLI。
///
/// 第 2 のマニュアルソース。設計・検証記録は
/// `docs/superpowers/specs/2026-07-22-ingest-alarmcom-design.md` を参照。
///
/// urtect（Google Sites）との差分:
/// - 対象記事は sitemap.xml + vegapunk 製品マスタ（Product ノード）駆動で絞る。全 3,490 記事は
///   取り込まない。製品の型番/別名が「ファミリーハブ URL」に現れる記事群だけをクロールする。
/// - 本文は完全 SSR。`#elm-main-content` 配下から抽出する。パンくずは `.mt-breadcrumbs`。
/// - 検索対象は日本語（`?mt-language=JA` の機械翻訳）。英語原文は body_original に保存し、
///   差分 ingest（content_hash）は英語原文で駆動する。
/// - robots.txt の Crawl-delay=5 を守るため、全 HTTP リクエストを 5 秒以上空けて逐次実行する。
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
        schema_ids::{manual_node_id, section_slug, with_schema_name, KIND_PRODUCT, KIND_SECTION},
        vectors::{embed_all, vector_entry, EMBED_CONCURRENCY},
    },
    model::GraphBuild,
    vegapunk::VegapunkClient,
};
use scraper::{ElementRef, Html, Selector};
use serde_json::json;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
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

/// クロール対象のうち fetch 失敗・本文空で skip した割合の上限。これを超えたら upsert 前に
/// bail する（サイト構造・抽出セレクタ変化の疑い）。
const MAX_SKIP_RATIO: f64 = 0.2;

/// query_nodes の limit。ちょうど limit 件返ったら silent truncation を疑い fail closed。
const PRODUCT_QUERY_LIMIT: i32 = 1000;

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

/// ASCII 英数字だけを残して大文字化する緩和正規化。**URL 選別専用**。
/// ハイフン・アンダースコア・括弧・空白・非 ASCII は全て捨てる。本文からの型番検出は
/// 別（`detect_product_models` の厳密境界チェック）で行い、意味論を混ぜない。
fn relaxed_normalize(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_uppercase()
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

/// URL の末尾パスセグメント（末尾スラッシュ由来の空セグメントは無視）。取れなければ None。
fn last_path_segment(u: &Url) -> Option<String> {
    u.path_segments()?
        .filter(|s| !s.is_empty())
        .next_back()
        .map(str::to_string)
}

/// `?mt-language=JA` を付与した日本語版 URL を組み立てる純関数。既存クエリがあれば `&` 連結。
fn japanese_url(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&mt-language=JA")
    } else {
        format!("{url}?mt-language=JA")
    }
}

/// `hub_path` が `url_path` のセグメント境界プレフィックスか（同一、または `hub_path/` で始まる）。
/// 素の文字列プレフィックスだと `.../foo` が `.../fooBar` に誤一致するため境界で比較する。
fn is_segment_prefix(hub_path: &str, url_path: &str) -> bool {
    let hp = hub_path.trim_end_matches('/');
    let up = url_path.trim_end_matches('/');
    up == hp || up.starts_with(&format!("{hp}/"))
}

/// 製品ファミリーのハブ URL 1 件（sitemap 出現順で確定）。
#[derive(Debug, Clone, PartialEq)]
struct FamilyHub {
    /// canonical（英語版）URL。
    url: String,
    /// alarmcom section slug。
    slug: String,
    /// canonical パス（配下 URL のセグメント境界プレフィックス判定に使う）。
    path: String,
    /// このハブにマッチした product_key の集合（決定論のため BTreeSet）。
    matched_product_keys: BTreeSet<String>,
}

/// クロール対象 URL 1 件。
#[derive(Debug, Clone, PartialEq)]
struct CrawlTarget {
    /// canonical（英語版）URL。
    url: String,
    /// alarmcom section slug。
    slug: String,
    /// 所属ハブの slug。ハブ自身は None（親なし）。
    parent_slug: Option<String>,
    /// sitemap 出現順の 0 始まり連番。
    order: i32,
}

/// クロール計画（ハブ一覧・クロール対象一覧・未マッチ製品）。全て sitemap 出現順で決定論的。
#[derive(Debug)]
struct CrawlPlan {
    hubs: Vec<FamilyHub>,
    targets: Vec<CrawlTarget>,
    /// どのハブにもマッチしなかった製品 product_key（昇順）。空でなければ呼び出し側で bail。
    unmatched_product_keys: Vec<String>,
}

/// sitemap URL とハブ末尾セグメントの緩和正規化一致で「製品ファミリーハブ」を特定する。
/// ハブ一覧は sitemap 出現順（HashMap の非決定イテレーションに依存しない）。
fn detect_family_hubs(
    sitemap_urls: &[String],
    product_lexicon: &HashMap<String, String>,
) -> Vec<FamilyHub> {
    // 表層形（大文字）→ product_key を緩和正規化した対応。空になるものは除外。
    let relaxed_surfaces: Vec<(String, String)> = product_lexicon
        .iter()
        .filter_map(|(surface, key)| {
            let relaxed = relaxed_normalize(surface);
            if relaxed.is_empty() {
                None
            } else {
                Some((relaxed, key.clone()))
            }
        })
        .collect();

    let mut hubs = Vec::new();
    let mut seen_hub_canonical: HashSet<String> = HashSet::new();
    for raw in sitemap_urls {
        let Ok(u) = Url::parse(raw) else {
            continue;
        };
        let Some(segment) = last_path_segment(&u) else {
            continue;
        };
        let relaxed_seg = relaxed_normalize(&segment);
        if relaxed_seg.is_empty() {
            continue;
        }
        let mut matched: BTreeSet<String> = BTreeSet::new();
        for (relaxed_surface, product_key) in &relaxed_surfaces {
            if relaxed_seg.contains(relaxed_surface.as_str()) {
                matched.insert(product_key.clone());
            }
        }
        if matched.is_empty() {
            continue;
        }
        let canonical = canonical_url(&u);
        if !seen_hub_canonical.insert(canonical.clone()) {
            continue; // 同一 canonical のハブ重複（末尾スラッシュ差など）は最初の 1 件のみ
        }
        hubs.push(FamilyHub {
            slug: alarmcom_slug(&canonical),
            path: canonical_path(&u),
            url: canonical,
            matched_product_keys: matched,
        });
    }
    hubs
}

/// ハブ配下のクロール対象を確定する。各 URL はハブ一覧（sitemap 順）で最初に一致したハブへ
/// 割り当てる（1 URL につき所属ハブは 1 つ）。ハブ URL 自身は親なし。canonical で重複排除する。
fn build_crawl_targets(sitemap_urls: &[String], hubs: &[FamilyHub]) -> Vec<CrawlTarget> {
    let hub_canonical: HashSet<&str> = hubs.iter().map(|h| h.url.as_str()).collect();
    let mut targets = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in sitemap_urls {
        let Ok(u) = Url::parse(raw) else {
            continue;
        };
        let canonical = canonical_url(&u);
        let parent_slug = if hub_canonical.contains(canonical.as_str()) {
            None // ハブ自身は親なし（別ハブのプレフィックスに含まれても自ハブとして扱う）
        } else {
            let url_path = canonical_path(&u);
            match hubs.iter().find(|h| is_segment_prefix(&h.path, &url_path)) {
                Some(hub) => Some(hub.slug.clone()),
                None => continue, // どのハブ配下でもない URL は対象外
            }
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let slug = alarmcom_slug(&canonical);
        targets.push(CrawlTarget {
            url: canonical,
            slug,
            parent_slug,
            order: 0,
        });
    }
    for (idx, target) in targets.iter_mut().enumerate() {
        target.order = idx as i32;
    }
    targets
}

/// ハブ検出 + クロール対象確定 + 製品マスタ完全性チェックをまとめた純関数。
fn plan_crawl(sitemap_urls: &[String], product_lexicon: &HashMap<String, String>) -> CrawlPlan {
    let hubs = detect_family_hubs(sitemap_urls, product_lexicon);
    let targets = build_crawl_targets(sitemap_urls, &hubs);
    // 製品マスタ由来の distinct product_key 全体（build_product_lexicon が空 model を弾く基準と
    // 同じく lexicon.values() を採る）。1 つでもハブ未マッチなら呼び出し側で bail する。
    let all_keys: BTreeSet<String> = product_lexicon.values().cloned().collect();
    let matched: BTreeSet<String> = hubs
        .iter()
        .flat_map(|h| h.matched_product_keys.iter().cloned())
        .collect();
    let unmatched_product_keys: Vec<String> = all_keys.difference(&matched).cloned().collect();
    CrawlPlan {
        hubs,
        targets,
        unmatched_product_keys,
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

/// `plan.targets` を「ハブ（`parent_slug` が None）を先に、子（Some）を後に」の 2 群へ安定並べ替え
/// した処理順序（参照列）にする純関数。各群内の相対順序（sitemap 出現順）は保持する。
///
/// これによりループ内で「親ハブがまだ処理されていない」と「親ハブの処理が失敗した」を区別する
/// 必要が無くなる（子を処理する時点で対応するハブは必ず処理済みのため）。`target.order`（表示用の
/// sitemap 出現順連番、ManualSection の `order` 属性のもと）はここでは一切変更しない — 変更するのは
/// 反復順序だけで、`order` フィールドの値そのものは元のまま各 `CrawlTarget` に残る。
fn hub_first_order(targets: &[CrawlTarget]) -> Vec<&CrawlTarget> {
    let mut hubs: Vec<&CrawlTarget> = Vec::new();
    let mut children: Vec<&CrawlTarget> = Vec::new();
    for target in targets {
        if target.parent_slug.is_none() {
            hubs.push(target);
        } else {
            children.push(target);
        }
    }
    hubs.extend(children);
    hubs
}

/// 子記事（`parent_slug = Some(parent)`）の親ハブが「過去の ingest 実行で既に upsert 済み
/// （`existing_hash` に slug が存在）」または「今回の実行で既に ingest 済み（`ingested_this_run`
/// に slug が存在）」のいずれかであるかを判定する純関数。親なし（ハブ自身）は常に ingestable
/// として true を返す。
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
    let crawl_delay_secs = if args.crawl_delay_secs < MIN_CRAWL_DELAY_SECS {
        tracing::warn!(
            requested = args.crawl_delay_secs,
            enforced = MIN_CRAWL_DELAY_SECS,
            "requested crawl delay is below answers.alarm.com robots.txt Crawl-delay=5; \
             raising to 5s to honor the site's stated rate limit"
        );
        MIN_CRAWL_DELAY_SECS
    } else {
        args.crawl_delay_secs
    };
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

    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await?;

    // 製品マスタは vegapunk の Product ノードが正本（Issue #6）。crawl 前に取得し、型番検出語彙
    // （表層形 → product_key）とファミリーハブ選別のもとにする。0 件 / limit 到達で fail closed。
    let product_nodes = client
        .query_nodes(&args.schema, "Product", Vec::new(), PRODUCT_QUERY_LIMIT)
        .await
        .context("query existing Product nodes (product master lookup)")?;
    if product_nodes.is_empty() {
        anyhow::bail!(
            "no Product nodes found in schema {}; the product master is empty — run \
             `ingest_products` first to seed it before running ingest_alarmcom",
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
    if product_lexicon.is_empty() {
        anyhow::bail!(
            "product master in schema {} has no usable model attributes; cannot derive crawl \
             targets — fix products.json (each product needs a non-empty model) and re-run \
             ingest_products",
            args.schema
        );
    }
    // 未マッチ製品の警告に使う product_key -> name（build_product_lexicon と同じ空 model 除外基準）。
    let name_by_key: HashMap<String, String> = product_nodes
        .iter()
        .filter_map(|n| {
            let model = n.attributes.get("model").filter(|m| !m.trim().is_empty())?;
            Some((
                model.clone(),
                n.attributes.get("name").cloned().unwrap_or_default(),
            ))
        })
        .collect();

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

    // sitemap fetch（HTTP、スロットル起点）と graph_snapshot（gRPC、スロットル対象外）は
    // 互いに依存しないので並行に発行する。
    let (sitemap_xml, existing_snapshot) = tokio::try_join!(
        async {
            fetch(&http, &sitemap_url)
                .await
                .with_context(|| format!("fetch sitemap {sitemap_url}"))
        },
        async {
            client
                .graph_snapshot(&args.schema, 5000)
                .await
                .context("snapshot existing graph (diff + stale-edge check)")
        },
    )?;
    // sitemap fetch をスロットルトラッカーの起点として記録する（後続の記事 fetch はここから
    // crawl_delay 以上空ける）。
    let mut last_request: Option<tokio::time::Instant> = Some(tokio::time::Instant::now());

    if existing_snapshot.truncated {
        anyhow::bail!(
            "existing graph snapshot truncated at node limit; \
             cannot verify diff/stale-edge state — aborting ingest"
        );
    }

    let sitemap_urls_raw = parse_sitemap_urls(&sitemap_xml);
    if sitemap_urls_raw.is_empty() {
        anyhow::bail!(
            "sitemap {sitemap_url} yielded no <loc> URLs; the sitemap format may have changed \
             or the fetch returned an error page — aborting ingest"
        );
    }
    // ハブ判定・実フェッチより前に、base_url と同一 origin の URL だけへ絞る（外部ホストへの
    // 意図しないフェッチ = SSRF/資格情報漏洩の経路を crawl 前に閉じる。filter_same_origin 参照）。
    let sitemap_urls = filter_same_origin(&sitemap_urls_raw, &base_url);
    let off_origin_dropped = sitemap_urls_raw.len() - sitemap_urls.len();
    if off_origin_dropped > 0 {
        tracing::warn!(
            dropped = off_origin_dropped,
            base_url = %args.base_url,
            "dropped off-origin sitemap URLs before crawl; only URLs matching base_url's \
             scheme+host are crawled (external hosts are never fetched)"
        );
    }
    if sitemap_urls.is_empty() {
        anyhow::bail!(
            "all {} <loc> URL(s) in sitemap {} are off-origin (not matching {}); refusing to \
             crawl external hosts — aborting ingest",
            sitemap_urls_raw.len(),
            sitemap_url,
            args.base_url
        );
    }

    let plan = plan_crawl(&sitemap_urls, &product_lexicon);
    if !plan.unmatched_product_keys.is_empty() {
        let named: Vec<String> = plan
            .unmatched_product_keys
            .iter()
            .map(|key| match name_by_key.get(key) {
                Some(name) if !name.is_empty() => format!("{key} ({name})"),
                _ => key.clone(),
            })
            .collect();
        tracing::warn!(
            unmatched = ?named,
            "one or more product master entries matched no family hub URL in the sitemap"
        );
        anyhow::bail!(
            "{} product(s) in the product master have no matching family hub URL in {}: {:?}; \
             add the URL-surface spelling to each product's aliases in products.json (then re-run \
             ingest_products) so their alarm.com manuals are not silently omitted",
            plan.unmatched_product_keys.len(),
            sitemap_url,
            named
        );
    }
    if plan.targets.is_empty() {
        anyhow::bail!(
            "no crawl targets derived from sitemap {sitemap_url} despite a non-empty product \
             master; the sitemap or product URL spellings may have changed — aborting ingest"
        );
    }

    // 差分 ingest 用の既存 hash マップ（DOC_KEY 配下の ManualSection のみ）。この doc_key フィルタで
    // urtect 側 section を絶対に触らないことを構造的に保証する。
    let existing_hash: HashMap<String, String> = existing_snapshot
        .nodes
        .iter()
        .filter(|n| n.node_type == "ManualSection")
        .filter(|n| n.attributes.get("doc_key").map(String::as_str) == Some(DOC_KEY))
        .filter_map(|n| {
            let key = n.attributes.get("section_key")?.clone();
            let hash = n.attributes.get("content_hash")?.clone();
            Some((key, hash))
        })
        .collect();

    // 既存の派生 edge / 親 edge を索引する（stale 検出用。alarmcom の sec_id でしか引かれないため
    // urtect 側には影響しない）。ingest_urtect と同じ構造。
    let mut old_derived: HashMap<String, HashSet<String>> = HashMap::new();
    let mut old_parents: HashMap<String, HashSet<String>> = HashMap::new();
    for e in &existing_snapshot.edges {
        if e.edge_type == "DESCRIBES" || e.edge_type == "MENTIONS_SIGNAL" {
            old_derived
                .entry(e.from_id.clone())
                .or_default()
                .insert(e.to_id.clone());
        } else if e.edge_type == "PARENT_OF" {
            old_parents
                .entry(e.to_id.clone())
                .or_default()
                .insert(e.from_id.clone());
        }
    }

    // disappeared 検出: 既存 DOC_KEY section のうち今回のクロール対象一覧に無いものは削除/非公開化
    // とみなし fail closed（backend に delete が無く、stale な本文/edge が検索候補に残り続ける）。
    let target_slugs: HashSet<String> = plan.targets.iter().map(|t| t.slug.clone()).collect();
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

    let fetched_at = chrono::Utc::now().to_rfc3339();

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut ingested = 0usize;
    let mut skipped_unchanged = 0usize;
    // fetch 失敗・本文空で skip した (url, reason)。全記事共通で skip+warn（urtect の既存 section
    // fail-closed 出し分けはしない。安全弁は MAX_SKIP_RATIO のみ）。
    let mut skip_records: Vec<(String, String)> = Vec::new();
    let mut zero_signal_sections: Vec<String> = Vec::new();
    let mut describes_by_model: HashMap<String, usize> = HashMap::new();
    // embed 対象（実際に ingest された section の (slug, body_ja)）。
    let mut section_bodies: Vec<(String, String)> = Vec::new();
    // 今回の実行で ingest 対象として確定した slug（親ハブ実在性チェック用）。
    let mut ingested_this_run: HashSet<String> = HashSet::new();

    for target in hub_first_order(&plan.targets) {
        let slug = target.slug.clone();

        // 親ハブがこれまでも今回も一度も ingest されていない子記事は孤児 PARENT_OF 辺を生む
        // ため、fetch する前に skip する（ハブ自身はこのチェックの対象外）。
        if !parent_is_known(
            target.parent_slug.as_deref(),
            &existing_hash,
            &ingested_this_run,
        ) {
            let parent = target.parent_slug.as_deref().unwrap_or("");
            tracing::warn!(
                url = %target.url,
                parent_slug = %parent,
                "parent hub was never ingested (fetch/extraction failed or not yet upserted); \
                 skipping child article to avoid a PARENT_OF edge pointing at a non-existent node"
            );
            skip_records.push((
                target.url.clone(),
                format!(
                    "parent hub {parent} not ingested (skipped to avoid orphan PARENT_OF edge)"
                ),
            ));
            continue;
        }

        // 英語版（canonical、クエリなし）。失敗したらこの記事を skip し日本語版は取りに行かない。
        let en_html = match throttled_fetch(&http, &target.url, crawl_delay, &mut last_request)
            .await
        {
            Ok(html) => html,
            Err(err) => {
                tracing::warn!(url = %target.url, error = %err, "english fetch failed; skipping article");
                skip_records.push((target.url.clone(), format!("english fetch failed: {err}")));
                continue;
            }
        };
        let body_en = normalize_body(&extract_main_body(&Html::parse_document(&en_html)));
        if body_en.is_empty() {
            tracing::warn!(url = %target.url, "english body empty after extraction; skipping article");
            skip_records.push((target.url.clone(), "english body empty".to_string()));
            continue;
        }

        // 日本語版（?mt-language=JA）。英語版が成功したときだけ取りに行く。
        let ja_url = japanese_url(&target.url);
        let ja_html = match throttled_fetch(&http, &ja_url, crawl_delay, &mut last_request).await {
            Ok(html) => html,
            Err(err) => {
                tracing::warn!(url = %ja_url, error = %err, "japanese fetch failed; skipping article");
                skip_records.push((ja_url.clone(), format!("japanese fetch failed: {err}")));
                continue;
            }
        };
        let ja_document = Html::parse_document(&ja_html);
        let body_ja = normalize_body(&extract_main_body(&ja_document));
        if body_ja.is_empty() {
            tracing::warn!(url = %ja_url, "japanese body empty after extraction; skipping article");
            skip_records.push((ja_url.clone(), "japanese body empty".to_string()));
            continue;
        }
        let crumbs = breadcrumb_crumbs(&ja_document);
        let breadcrumb = breadcrumb_string(&crumbs);
        let title = breadcrumb_title(&crumbs);
        if title.is_empty() {
            tracing::warn!(
                url = %ja_url,
                "breadcrumb title empty (.mt-breadcrumbs absent or last crumb empty); skipping article"
            );
            skip_records.push((ja_url.clone(), "breadcrumb title empty".to_string()));
            continue;
        }

        // content_hash は英語原文（source of truth）のみで駆動する。差分 ingest は英語原文の
        // 変化だけを見る。
        //
        // 【既知の制約（設計 spec が意図的に選んだ簡略化）】ingest_urtect は複合ハッシュ
        // （本文 + 検出型番 + signal + order/parent 等を全てハッシュに含める）で、本文以外の
        // 派生要素が変わっても再検出できる安全弁を持つ。alarmcom はこの安全弁を落としており、
        // 英語本文が不変な限り product_lexicon / 製品マスタの変更で detect_product_models /
        // signal_values の結果（DESCRIBES / MENTIONS_SIGNAL 辺のもと）が変化しても、当該 section は
        // 「変更なし」と判定され再 upsert されない。例: ingest_products で新型番を追加し、既存の
        // 英語記事が偶然その型番に言及していても、本文が変わっていなければ新しい DESCRIBES 辺は
        // 張られない。運用上の回避策: lexicon / 製品マスタを大きく変更したら alarmcom の tenant
        // schema を作り直して全件再 ingest する（backend に delete が無く、部分更新では派生辺を
        // 張り直せないため）。ロジックは spec の選択どおり本文のみのハッシュに保つ。
        let hash = content_hash(&body_en);
        if existing_hash.get(&slug) == Some(&hash) {
            skipped_unchanged += 1;
            continue;
        }

        // 型番検出は英語+日本語本文の連結に対して行う（型番は機械翻訳でも残るが取りこぼし防止）。
        let combined = format!("{body_en} {body_ja}");
        let product_models = detect_product_models(&combined, &product_lexicon);
        let signal_values: Vec<String> = lexicon
            .normalize(&body_ja)
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();

        // 変更された既存 section の stale 派生/親 edge 検出（ingest_urtect と同じ。backend に
        // delete が無く、除去が必要な edge が残ると検索を誤らせるため fail closed）。
        if existing_hash.contains_key(&slug) {
            let sec_id = manual_node_id(&args.schema, KIND_SECTION, &slug);
            if let Some(old_targets) = old_derived.get(&sec_id) {
                let new_targets: HashSet<String> = product_models
                    .iter()
                    .map(|m| manual_node_id(&args.schema, KIND_PRODUCT, m))
                    .chain(
                        signal_values
                            .iter()
                            .map(|s| manual_node_id(&args.schema, "Signal", s)),
                    )
                    .collect();
                let stale: Vec<&String> = old_targets
                    .iter()
                    .filter(|t| !new_targets.contains(*t))
                    .collect();
                if !stale.is_empty() {
                    anyhow::bail!(
                        "section {slug} requires removing derived edges ({stale:?}) but the \
                         backend exposes no delete; recreate the tenant schema and re-ingest \
                         from scratch"
                    );
                }
            }
            if let Some(old_parent_ids) = old_parents.get(&sec_id) {
                let new_parent_id = target
                    .parent_slug
                    .as_deref()
                    .map(|p| manual_node_id(&args.schema, KIND_SECTION, p));
                let stale_parents: Vec<&String> = old_parent_ids
                    .iter()
                    .filter(|p| Some(p.as_str()) != new_parent_id.as_deref())
                    .collect();
                if !stale_parents.is_empty() {
                    anyhow::bail!(
                        "section {slug} changed parent (old {stale_parents:?} vs new \
                         {new_parent_id:?}) which requires removing PARENT_OF edges, but the \
                         backend exposes no delete; recreate the tenant schema and re-ingest \
                         from scratch"
                    );
                }
            }
        }

        for model in &product_models {
            *describes_by_model.entry(model.clone()).or_insert(0) += 1;
        }
        if signal_values.is_empty() {
            zero_signal_sections.push(slug.clone());
        }

        section_bodies.push((slug.clone(), body_ja.clone()));
        let input = ManualSectionInput {
            slug: slug.clone(),
            title,
            body: body_ja,
            source_url: target.url.clone(),
            breadcrumb,
            section_no: None,
            order: target.order,
            parent_slug: target.parent_slug.clone(),
            product_models,
            signal_values,
            // 英語原文とその hash を書く。content_hash（upsert 判定）も同じ英語原文 hash。
            body_original: Some(body_en),
            original_hash: Some(hash.clone()),
        };
        let build = build_section_graph(&args.schema, DOC_KEY, &input, &hash);
        nodes.extend(build.nodes);
        edges.extend(build.edges);
        ingested += 1;
        ingested_this_run.insert(slug);
    }

    // 安全弁: skip 割合が閾値超なら upsert 前に bail（サイト構造/セレクタ変化の疑い）。
    let skipped_fetch_or_empty = skip_records.len();
    let target_count = plan.targets.len();
    let skip_ratio = skipped_fetch_or_empty as f64 / target_count as f64;
    if skip_ratio > MAX_SKIP_RATIO {
        anyhow::bail!(
            "skipped {skipped_fetch_or_empty}/{target_count} articles ({:.1}%) due to fetch \
             failure or empty body, exceeding the {:.0}% safety threshold; the site structure or \
             extraction selector (#elm-main-content) may have changed — aborting before upsert \
             (inspect the skipped URLs)",
            skip_ratio * 100.0,
            MAX_SKIP_RATIO * 100.0
        );
    }

    // document ノード（Alarm.com Answers）を 1 件集約する。
    nodes.push(build_document_node(
        &args.schema,
        DOC_KEY,
        "Alarm.com Answers",
        &args.base_url,
        &fetched_at,
    ));

    // 同一 id のノード重複を除去（Signal ノードが節ごとに重複しやすい）。順序は保持する。
    let mut seen_node_ids = HashSet::new();
    nodes.retain(|n| seen_node_ids.insert(n.id.clone()));

    // embedding → vector upsert を node/edge upsert より必ず先に行う（ingest_urtect と同じ
    // fail-closed 順序）。ここで失敗して bail しても content_hash は旧値のまま残るため、次回の
    // 差分 ingest が当該 section を再検出し embedding ごと再試行する。逆順だと vector 欠落のまま
    // 永久 skip される。embed は fail closed（1 件でも失敗で abort）。
    let vectors_skipped = args.no_vectors;
    let upserted_vectors = if vectors_skipped {
        0
    } else {
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
        let mut entries: Vec<(String, Vec<f32>, Vec<(String, String)>)> =
            Vec::with_capacity(section_slugs.len());
        for ((slug, text), vector) in section_slugs
            .iter()
            .zip(section_texts.iter())
            .zip(section_vectors)
        {
            let id = manual_node_id(&args.schema, KIND_SECTION, slug);
            entries.push(vector_entry(
                id,
                vector,
                text,
                KIND_SECTION,
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

    let fetch_or_empty_skipped_urls: Vec<serde_json::Value> = skip_records
        .iter()
        .map(|(url, reason)| json!({ "url": url, "reason": reason }))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "base_url": args.base_url,
            "sitemap_url": sitemap_url,
            "sitemap_url_count": sitemap_urls.len(),
            "off_origin_urls_dropped": off_origin_dropped,
            "matched_family_hub_count": plan.hubs.len(),
            "crawl_target_count": target_count,
            "ingested_manual_sections": ingested,
            "skipped_unchanged": skipped_unchanged,
            "skipped_fetch_or_empty": skipped_fetch_or_empty,
            "skip_ratio": skip_ratio,
            "expected_nodes": expected_nodes,
            "expected_edges": expected_edges,
            "upserted_nodes": upserted_nodes,
            "upserted_edges": upserted_edges,
            "upserted_vectors": upserted_vectors,
            "vectors_skipped": vectors_skipped,
            "describes_by_model": describes_by_model,
            "sections_with_zero_signal_matches": zero_signal_sections,
            "fetch_or_empty_skipped_urls": fetch_or_empty_skipped_urls,
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
/// （改竄・設定ミス由来の）外部 URL が、末尾セグメントの緩和正規化マッチで偶然ファミリーハブと
/// 誤認されると、実フェッチが外部ホスト（例: `http://169.254.169.254/...` の cloud metadata
/// endpoint）へ飛び、Cloud Run 環境ではサービスアカウント資格情報の漏洩につながりうる。
/// 既存の redirect policy は「リダイレクト」にしか効かず**初期 fetch 先 URL 自体**の origin を
/// 検証しないため、ハブ判定より前のこの段階で同じ「同一 scheme/host のみ」境界を課す。
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

    /// ADC-V724（model）、ADC-VC727P（model）、VC727PRO（ADC-VC727P の別名）の 3 表層形。
    fn sample_lexicon() -> HashMap<String, String> {
        let mut lexicon = HashMap::new();
        lexicon.insert("ADC-V724".to_string(), "ADC-V724".to_string());
        lexicon.insert("ADC-VC727P".to_string(), "ADC-VC727P".to_string());
        lexicon.insert("VC727PRO".to_string(), "ADC-VC727P".to_string());
        lexicon
    }

    const HUB_V724: &str =
        "https://answers.alarm.com/Partner/Video_Devices/1080p_Outdoor_Camera_ADC-V724_724X";
    const CHILD_WIFI: &str = "https://answers.alarm.com/Partner/Video_Devices/1080p_Outdoor_Camera_ADC-V724_724X/Wi-Fi_Setup";
    const HUB_VC727PRO: &str =
        "https://answers.alarm.com/Partner/Video_Devices/Mini_Bullet_Camera_VC727PRO";
    const UNRELATED_THERMOSTAT: &str =
        "https://answers.alarm.com/Partner/Thermostats/Smart_Thermostat_ADC-T2000";

    fn sample_sitemap_urls() -> Vec<String> {
        vec![
            HUB_V724.to_string(),
            CHILD_WIFI.to_string(),
            UNRELATED_THERMOSTAT.to_string(),
            HUB_VC727PRO.to_string(),
        ]
    }

    #[test]
    fn relaxed_normalize_keeps_only_ascii_alnum_uppercased() {
        assert_eq!(
            relaxed_normalize("1080p_Outdoor_ADC-V724_724X"),
            "1080POUTDOORADCV724724X"
        );
        assert_eq!(relaxed_normalize("ADC-V724"), "ADCV724");
        // 非 ASCII・記号・空白は全て捨てる
        assert_eq!(relaxed_normalize("（型番）V724 テスト"), "V724");
    }

    #[test]
    fn detect_family_hubs_matches_model_and_alias_but_not_unrelated() {
        let hubs = detect_family_hubs(&sample_sitemap_urls(), &sample_lexicon());
        // ハブは V724（model 一致）と VC727PRO（alias 一致）の 2 件、sitemap 出現順。
        let hub_urls: Vec<&str> = hubs.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(hub_urls, vec![HUB_V724, HUB_VC727PRO]);
        // model 一致は ADC-V724 のみ
        assert_eq!(
            hubs[0].matched_product_keys,
            BTreeSet::from(["ADC-V724".to_string()])
        );
        // alias（VC727PRO）経由で ADC-VC727P にマッチ
        assert_eq!(
            hubs[1].matched_product_keys,
            BTreeSet::from(["ADC-VC727P".to_string()])
        );
        // 無関係ファミリー（サーモスタット）はハブにならない
        assert!(!hubs.iter().any(|h| h.url == UNRELATED_THERMOSTAT));
    }

    #[test]
    fn detect_family_hubs_ignores_url_matching_no_product() {
        // 製品語彙に一致しない URL はハブ扱いされない。
        let sitemap = vec![UNRELATED_THERMOSTAT.to_string()];
        let hubs = detect_family_hubs(&sitemap, &sample_lexicon());
        assert!(hubs.is_empty());
    }

    #[test]
    fn is_segment_prefix_respects_path_boundaries() {
        assert!(is_segment_prefix("/a/b", "/a/b")); // 同一
        assert!(is_segment_prefix("/a/b", "/a/b/c")); // 配下
        assert!(!is_segment_prefix("/a/b", "/a/bc")); // 素の前方一致は境界外なので不一致
        assert!(!is_segment_prefix("/a/b", "/a")); // 親は配下でない
    }

    #[test]
    fn build_crawl_targets_selects_hub_and_children_excludes_siblings() {
        let hubs = detect_family_hubs(&sample_sitemap_urls(), &sample_lexicon());
        let targets = build_crawl_targets(&sample_sitemap_urls(), &hubs);
        // クロール対象は V724 ハブ・その配下 Wi-Fi・VC727PRO ハブの 3 件。
        // 無関係サーモスタットは除外される。
        let urls: Vec<&str> = targets.iter().map(|t| t.url.as_str()).collect();
        assert_eq!(urls, vec![HUB_V724, CHILD_WIFI, HUB_VC727PRO]);
        // order は sitemap 出現順の 0 始まり連番
        assert_eq!(targets[0].order, 0);
        assert_eq!(targets[1].order, 1);
        assert_eq!(targets[2].order, 2);
        // ハブ自身は親なし
        assert_eq!(targets[0].parent_slug, None);
        assert_eq!(targets[2].parent_slug, None);
        // 配下 URL の親はハブ自身の slug（フラット 2 階層）
        assert_eq!(targets[1].parent_slug, Some(alarmcom_slug(HUB_V724)));
    }

    #[test]
    fn plan_crawl_flags_products_without_a_matching_hub() {
        // ADC-DB772 は sitemap にファミリー URL が無い → unmatched に出る。
        let mut lexicon = sample_lexicon();
        lexicon.insert("ADC-DB772".to_string(), "ADC-DB772".to_string());
        let plan = plan_crawl(&sample_sitemap_urls(), &lexicon);
        assert_eq!(plan.unmatched_product_keys, vec!["ADC-DB772".to_string()]);
    }

    #[test]
    fn plan_crawl_has_no_unmatched_when_all_products_have_hubs() {
        let plan = plan_crawl(&sample_sitemap_urls(), &sample_lexicon());
        assert!(plan.unmatched_product_keys.is_empty());
        assert_eq!(plan.hubs.len(), 2);
        assert_eq!(plan.targets.len(), 3);
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
            HUB_V724.to_string(), // 同一 origin
            // cloud metadata endpoint（host も scheme も違う）は除外される
            "http://169.254.169.254/latest/meta-data/ADC-V724_724X".to_string(),
            // host 違いは除外
            "https://evil.example.com/Partner/ADC-V724_724X".to_string(),
            // scheme 違い（http vs https）も除外
            "http://answers.alarm.com/Partner/ADC-V724_724X".to_string(),
            // パース不能な文字列も除外
            "not a url".to_string(),
            HUB_VC727PRO.to_string(), // 同一 origin
        ];
        let filtered = filter_same_origin(&urls, &base);
        assert_eq!(
            filtered,
            vec![HUB_V724.to_string(), HUB_VC727PRO.to_string()]
        );
    }

    #[test]
    fn filter_same_origin_is_noop_for_all_same_origin_fixture() {
        // 既存の 14 件テストが使う fixture は全て同一 origin。フィルタで欠落しないことを固定する
        // （フィルタ導入による既存挙動への影響が無いことの担保）。
        let base = Url::parse("https://answers.alarm.com").unwrap();
        let filtered = filter_same_origin(&sample_sitemap_urls(), &base);
        assert_eq!(filtered, sample_sitemap_urls());
    }

    #[test]
    fn japanese_url_appends_mt_language_param() {
        assert_eq!(
            japanese_url("https://answers.alarm.com/Partner/A"),
            "https://answers.alarm.com/Partner/A?mt-language=JA"
        );
        // 既存クエリがあれば & で連結する
        assert_eq!(
            japanese_url("https://answers.alarm.com/Partner/A?foo=bar"),
            "https://answers.alarm.com/Partner/A?foo=bar&mt-language=JA"
        );
    }

    /// `#elm-main-content` 本文と `.mt-breadcrumbs` を含む最小 HTML fixture。
    const SAMPLE_ARTICLE_HTML: &str = r#"<html><body>
      <div class="mt-breadcrumbs">
        <span><a href="/Partner">Partner</a></span>
        <span><a href="/Partner/Video">ビデオ機器</a></span>
        <span class="mt-breadcrumbs-current-page">Wi-Fi設定</span>
      </div>
      <article class="elm-content-container" id="elm-main-content">
        <nav>パンくずナビ</nav>
        <h1>Wi-Fi設定</h1>
        <p>カメラをWi-Fiに接続します。</p>
        <script>var noise = 1;</script>
        <style>.x{color:red}</style>
      </article>
    </body></html>"#;

    #[test]
    fn extract_main_body_collects_content_and_excludes_noise() {
        let document = Html::parse_document(SAMPLE_ARTICLE_HTML);
        let body = normalize_body(&extract_main_body(&document));
        assert!(body.contains("Wi-Fi設定"));
        assert!(body.contains("カメラをWi-Fiに接続します。"));
        // nav / script / style は本文に取り込まない
        assert!(!body.contains("パンくずナビ"));
        assert!(!body.contains("noise"));
        assert!(!body.contains("color:red"));
    }

    #[test]
    fn extract_main_body_empty_when_container_absent() {
        let document = Html::parse_document("<html><body><p>本文</p></body></html>");
        assert_eq!(extract_main_body(&document), "");
    }

    #[test]
    fn breadcrumb_crumbs_lists_direct_children_and_title_is_last() {
        let document = Html::parse_document(SAMPLE_ARTICLE_HTML);
        let crumbs = breadcrumb_crumbs(&document);
        assert_eq!(crumbs, vec!["Partner", "ビデオ機器", "Wi-Fi設定"]);
        assert_eq!(
            breadcrumb_string(&crumbs),
            "Partner > ビデオ機器 > Wi-Fi設定"
        );
        assert_eq!(breadcrumb_title(&crumbs), "Wi-Fi設定");
    }

    #[test]
    fn breadcrumb_absent_yields_empty() {
        let document = Html::parse_document("<html><body><p>本文</p></body></html>");
        assert!(breadcrumb_crumbs(&document).is_empty());
        assert_eq!(breadcrumb_title(&breadcrumb_crumbs(&document)), "");
    }

    #[test]
    fn hub_first_order_moves_hubs_before_children_preserving_relative_order() {
        // 子が sitemap 上ハブより先に出現するケース（ハブ側の実処理順序をこの関数が正す）。
        let child_a = CrawlTarget {
            url: "https://x/child-a".to_string(),
            slug: "child-a".to_string(),
            parent_slug: Some("hub".to_string()),
            order: 0,
        };
        let hub = CrawlTarget {
            url: "https://x/hub".to_string(),
            slug: "hub".to_string(),
            parent_slug: None,
            order: 1,
        };
        let child_b = CrawlTarget {
            url: "https://x/child-b".to_string(),
            slug: "child-b".to_string(),
            parent_slug: Some("hub".to_string()),
            order: 2,
        };
        let other_hub = CrawlTarget {
            url: "https://x/other-hub".to_string(),
            slug: "other-hub".to_string(),
            parent_slug: None,
            order: 3,
        };
        let targets = vec![
            child_a.clone(),
            hub.clone(),
            child_b.clone(),
            other_hub.clone(),
        ];
        let ordered = hub_first_order(&targets);
        let slugs: Vec<&str> = ordered.iter().map(|t| t.slug.as_str()).collect();
        // ハブ群（hub, other-hub、元の相対順序を保持）が先、子群（child-a, child-b）が後。
        assert_eq!(slugs, vec!["hub", "other-hub", "child-a", "child-b"]);
        // order フィールド自体は並べ替えの影響を受けない（表示用の sitemap 出現順連番のまま）。
        assert_eq!(hub.order, 1);
        assert_eq!(child_a.order, 0);
        assert_eq!(child_b.order, 2);
        assert_eq!(other_hub.order, 3);
    }

    #[test]
    fn hub_first_order_is_noop_when_all_hubs_already_precede_all_children() {
        // 「全ハブが全子より先」という 2 群構造そのままの入力は並べ替えても変化しない。
        // `sample_sitemap_urls()`（ハブ・子・ハブの順）はこの意味では「既にハブ優先」ではない点に
        // 注意（hub_first_order_moves_hubs_before_children_preserving_relative_order が検証する
        // ケース）。ここでは 2 群構造が既に成立している入力そのものを固定する。
        let hub_a = CrawlTarget {
            url: "https://x/hub-a".to_string(),
            slug: "hub-a".to_string(),
            parent_slug: None,
            order: 0,
        };
        let hub_b = CrawlTarget {
            url: "https://x/hub-b".to_string(),
            slug: "hub-b".to_string(),
            parent_slug: None,
            order: 1,
        };
        let child_a = CrawlTarget {
            url: "https://x/child-a".to_string(),
            slug: "child-a".to_string(),
            parent_slug: Some("hub-a".to_string()),
            order: 2,
        };
        let child_b = CrawlTarget {
            url: "https://x/child-b".to_string(),
            slug: "child-b".to_string(),
            parent_slug: Some("hub-b".to_string()),
            order: 3,
        };
        let targets = vec![hub_a, hub_b, child_a, child_b];
        let ordered = hub_first_order(&targets);
        let slugs: Vec<&str> = ordered.iter().map(|t| t.slug.as_str()).collect();
        assert_eq!(slugs, vec!["hub-a", "hub-b", "child-a", "child-b"]);
    }

    #[test]
    fn parent_is_known_covers_all_branches() {
        let mut existing_hash = HashMap::new();
        existing_hash.insert("hub-existing".to_string(), "hash1".to_string());
        let mut ingested_this_run = HashSet::new();
        ingested_this_run.insert("hub-this-run".to_string());

        // 親なし（ハブ自身）は常に true。
        assert!(parent_is_known(None, &existing_hash, &ingested_this_run));
        // 親が過去の ingest 実行で既に upsert 済み（existing_hash に存在）。
        assert!(parent_is_known(
            Some("hub-existing"),
            &existing_hash,
            &ingested_this_run
        ));
        // 親が今回の実行で既に ingest 済み。
        assert!(parent_is_known(
            Some("hub-this-run"),
            &existing_hash,
            &ingested_this_run
        ));
        // どちらの集合にも無い親は false（孤児 PARENT_OF 辺を防ぐため skip 対象）。
        assert!(!parent_is_known(
            Some("hub-never-ingested"),
            &existing_hash,
            &ingested_this_run
        ));
    }

    #[test]
    fn alarmcom_slug_is_prefixed_and_stable() {
        let slug = alarmcom_slug(HUB_V724);
        assert!(slug.starts_with("alarmcom-sec-"));
        // 同一 URL は同一 slug（冪等キー）
        assert_eq!(slug, alarmcom_slug(HUB_V724));
        // urtect の section slug（sec-...）と衝突しない
        assert_ne!(slug, section_slug(HUB_V724));
    }
}
