/// URTECT (Google Sites) manual crawler / ingest CLI。設計・検証記録は
/// `docs/superpowers/plans/2026-07-08-manual-domain-template-urtect.md` を参照。
use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    harness::signal::{LexiconNormalizer, SignalNormalizer},
    manual::{
        ingest_model::{
            build_document_node, build_product_node, build_section_graph, content_hash,
            ManualProductInput, ManualSectionInput,
        },
        schema_ids::section_slug,
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

/// embed 呼び出しの同時実行数上限。vegapunk backend への負荷配慮のため固定値とする
/// （ingest 規模は ~数十〜百件程度で、動的なチューニングが要る負荷特性ではない）。
const EMBED_CONCURRENCY: usize = 4;

/// 本文中に出現しうる既知型番。"ADC-V724" は "ADC-V724X" の前方一致になるため、
/// detect_product_models 側で英数字境界を見て誤爆(V724X ページを V724 とも誤判定)を防ぐ。
///
/// data/urtect/signal-lexicon.json の model_adc_v724 / model_adc_v724x / model_adc_vc727p
/// signal と同じ 3 型番を指す。あちらは MENTIONS_SIGNAL（表現ゆれ吸収・normalize_key 部分一致）
/// 用、こちらは DESCRIBES（型番の厳密な同一性）用で、判定基準が異なるため意図的に別管理にして
/// いる（lexicon の surface_forms は normalize_key で "-" ごと失われ、境界チェック付きの厳密
/// 一致には使えない）。型番を増減する際は両ファイルを合わせて更新すること。
const KNOWN_MODELS: &[&str] = &["ADC-V724", "ADC-V724X", "ADC-VC727P"];

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
fn extract_main_text(html: &str) -> (String, String) {
    let document = Html::parse_document(html);
    let title = extract_title(&document);

    let main_selector = Selector::parse("main").expect("valid selector");
    let container = document
        .select(&main_selector)
        .next()
        .unwrap_or_else(|| document.root_element());

    // script/style/noscript/nav/header/footer 配下のテキストは本文でないため除外して収集する。
    // `.text()` をそのまま使うと inline JS/CSS を拾い、Google Sites では本文が JS で汚染される。
    let mut raw = String::new();
    for node in container.descendants() {
        let scraper::Node::Text(text) = node.value() else {
            continue;
        };
        let under_noise = node.ancestors().any(|anc| {
            matches!(anc.value(), scraper::Node::Element(el)
                if matches!(el.name(), "script" | "style" | "noscript" | "nav" | "header" | "footer"))
        });
        if !under_noise {
            let chunk: &str = text;
            raw.push_str(chunk);
            raw.push(' ');
        }
    }

    for boilerplate in BOILERPLATE_SENTENCES {
        raw = raw.replace(boilerplate, "");
    }

    (title, raw)
}

/// 空白（改行・タブ・全角スペース含む）を単一の半角スペースへ正規化する純関数。
fn normalize_body(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// needle が haystack 中に「英数字境界で」出現するか（前後が英数字でない位置のみ一致とみなす）。
/// "ADC-V724" が "ADC-V724X" の内部に前方一致してしまう誤爆を防ぐために使う。
fn contains_as_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(pos, _)| {
        let end = pos + needle.len();
        let before_ok = haystack[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        before_ok && after_ok
    })
}

/// body に出現する既知型番を検出する（DESCRIBES 辺のもとになる）。
fn detect_product_models(body: &str) -> Vec<String> {
    // KNOWN_MODELS は元から大文字なので、比較対象の body 側だけ大文字化すれば足りる。
    let upper = body.to_uppercase();
    KNOWN_MODELS
        .iter()
        .filter(|model| contains_as_token(&upper, model))
        .map(|model| model.to_string())
        .collect()
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
    // 2. 既存グラフ snapshot（差分 ingest の hash マップと派生 edge の stale 検出の両方に使う。
    //    query_nodes は limit=1000 で取りこぼすと stale 検出が fail open になるため使わない）。
    // 互いに依存しない読み取りなので並行に発行する。
    let (top_html, existing_snapshot) = tokio::try_join!(
        async {
            fetch(&http, top_url.as_str())
                .await
                .with_context(|| format!("fetch top url {top_url}"))
        },
        async {
            client
                .graph_snapshot(&args.schema, 5000)
                .await
                .context("snapshot existing graph (diff + stale-edge check)")
        },
    )?;
    // snapshot が不完全だと差分判定・stale 検出の両方が信頼できないため fail closed。
    if existing_snapshot.truncated {
        anyhow::bail!(
            "existing graph snapshot truncated at node limit; \
             cannot verify diff/stale-edge state — aborting ingest"
        );
    }
    // 既存の派生 edge（DESCRIBES / MENTIONS_SIGNAL）を from_id ごとに索引する。
    // backend に delete API が無いため、再 ingest で「除去が必要になる」edge 変化
    // （旧 edge が新しい派生集合に含まれない）を検出したら fail closed にする
    // （検索側は snapshot 上の全 DESCRIBES を信頼するため、stale edge は誤回答に直結する）。
    // PARENT_OF（親→子）も同様: 親が変わった/root になった section に旧 PARENT_OF が残ると
    // 1 section に複数 parent が付き、TOC・ancestor traversal・breadcrumb が不整合になる。
    let mut old_derived: HashMap<String, HashSet<String>> = HashMap::new();
    let mut old_parents: HashMap<String, HashSet<String>> = HashMap::new();
    for e in &existing_snapshot.edges {
        if e.edge_type == "DESCRIBES" || e.edge_type == "MENTIONS_SIGNAL" {
            old_derived
                .entry(e.from_id.clone())
                .or_default()
                .insert(e.to_id.clone());
        } else if e.edge_type == "PARENT_OF" {
            // key = 子 section の node_id、value = 親 section の node_id 集合
            old_parents
                .entry(e.to_id.clone())
                .or_default()
                .insert(e.from_id.clone());
        }
    }

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

    // 差分 ingest 用の既存 hash マップ。snapshot（完全性は上で fail closed 済み）から構築する。
    // 同一 schema に別 document の ManualSection が同居しても誤って disappeared 判定しないよう、
    // 今回 ingest 対象の document（DOC_KEY）に限定する。
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
        let product_models = detect_product_models(&body);
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

        // 変更された既存 section について、旧 DESCRIBES / MENTIONS_SIGNAL の宛先が
        // 新しい派生集合に全て含まれるか検証する。含まれない（= 除去が必要な）edge が
        // あれば fail closed（upsert は追加しかできず、stale edge が検索を誤らせるため）。
        if existing_hash.contains_key(&slug) {
            let sec_id = cs_support_mcp::manual::schema_ids::manual_node_id(
                &args.schema,
                cs_support_mcp::manual::schema_ids::KIND_SECTION,
                &slug,
            );
            if let Some(old_targets) = old_derived.get(&sec_id) {
                let new_targets: HashSet<String> = product_models
                    .iter()
                    .map(|m| {
                        cs_support_mcp::manual::schema_ids::manual_node_id(
                            &args.schema,
                            cs_support_mcp::manual::schema_ids::KIND_PRODUCT,
                            m,
                        )
                    })
                    .chain(signal_values.iter().map(|s| {
                        cs_support_mcp::manual::schema_ids::manual_node_id(
                            &args.schema,
                            "Signal",
                            s,
                        )
                    }))
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
            // PARENT_OF: 旧親が新しい親（root なら「親なし」）と一致しない場合も除去が必要。
            if let Some(old_parent_ids) = old_parents.get(&sec_id) {
                let new_parent_id = parent_slug.as_deref().map(|p| {
                    cs_support_mcp::manual::schema_ids::manual_node_id(
                        &args.schema,
                        cs_support_mcp::manual::schema_ids::KIND_SECTION,
                        p,
                    )
                });
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
        };
        let build = build_section_graph(&args.schema, DOC_KEY, &input, &hash);
        nodes.extend(build.nodes);
        edges.extend(build.edges);
        ingested += 1;
    }

    // 5. build_document_node 1 回 + build_product_node（3 型番）を集約する。
    nodes.push(build_document_node(
        &args.schema,
        DOC_KEY,
        &doc_title,
        top_url.as_str(),
        &chrono::Utc::now().to_rfc3339(),
    ));
    let mut product_inputs: Vec<ManualProductInput> = Vec::new();
    for &model in KNOWN_MODELS {
        let input = ManualProductInput {
            model: model.to_string(),
            name: model.to_string(),
            aliases: Vec::new(),
        };
        nodes.push(build_product_node(&args.schema, &input));
        product_inputs.push(input);
    }

    // 同一 id のノード重複を除去してから upsert する（Signal ノードは節ごとに生成されるため
    // 共通 signal が節数ぶん重複しやすい。冪等 upsert なので正しさには影響しないが、
    // gRPC ペイロードと ingest 時間を無駄に膨らませる）。順序は保持する。
    let mut seen_node_ids = HashSet::new();
    let mut nodes = nodes;
    nodes.retain(|n| seen_node_ids.insert(n.id.clone()));

    // 5. embedding: 今回 ingest された section と全 product を embed し、一括 upsert する。
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
        let mut entries: Vec<(String, Vec<f32>, Vec<(String, String)>)> =
            Vec::with_capacity(section_bodies.len() + product_inputs.len());

        // section_bodies はここ以降使わないため move で消費する（body は section 本文の
        // 全文で ingest 中最大級の String。clone を避ける）。slug は zip 用に残す。
        let mut section_slugs: Vec<String> = Vec::with_capacity(section_bodies.len());
        let section_items: Vec<(String, String)> = section_bodies
            .into_iter()
            .map(|(slug, body)| {
                let label = format!("section {slug}");
                section_slugs.push(slug);
                (label, body)
            })
            .collect();
        let section_vectors = embed_all(&client, section_items, EMBED_CONCURRENCY).await?;
        for (slug, vector) in section_slugs.iter().zip(section_vectors) {
            let id = cs_support_mcp::manual::schema_ids::manual_node_id(
                &args.schema,
                cs_support_mcp::manual::schema_ids::KIND_SECTION,
                slug,
            );
            entries.push((
                id,
                vector,
                vec![
                    ("node_type".to_string(), "ManualSection".to_string()),
                    ("section_key".to_string(), slug.clone()),
                    ("doc_key".to_string(), DOC_KEY.to_string()),
                ],
            ));
        }

        let product_items: Vec<(String, String)> = product_inputs
            .iter()
            .map(|input| {
                let text = format!("{} {}", input.name, input.aliases.join(" "));
                (format!("product {}", input.model), text)
            })
            .collect();
        let product_vectors = embed_all(&client, product_items, EMBED_CONCURRENCY).await?;
        for (input, vector) in product_inputs.iter().zip(product_vectors) {
            let id = cs_support_mcp::manual::schema_ids::manual_node_id(
                &args.schema,
                cs_support_mcp::manual::schema_ids::KIND_PRODUCT,
                &input.model,
            );
            entries.push((
                id,
                vector,
                vec![
                    ("node_type".to_string(), "Product".to_string()),
                    ("product_key".to_string(), input.model.clone()),
                ],
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

/// `items`（`(label, text)`）を最大 `concurrency` 件まで同時実行で embed する。
///
/// - 全タスクを先に spawn するが、各タスクは Semaphore permit を取ってから embed RPC を
///   発行するため、実行中の RPC は常に最大 `concurrency` 件に制限される。
/// - fail-closed: 1 件でも失敗したら失敗 label を context に含めて即座に bail する。
///   early return による `JoinSet` の drop が未完了タスクを abort するため、部分的な
///   ベクトル状態を後続処理に渡さない（呼び出し側は成功時の `Vec` を丸ごと使うか、
///   エラーで ingest 全体を abort するかの二択になる）。
/// - 返り値は `items` と同じ順序を保つ（呼び出し側が id/attrs を zip で組み立てられるように）。
///   completion 順は不定なので、`idx` 付きで結果を集めてから index 順に並べ直す。
async fn embed_all(
    client: &VegapunkClient,
    items: Vec<(String, String)>,
    concurrency: usize,
) -> Result<Vec<Vec<f32>>> {
    // concurrency = 0 は permit が永久に取れず全タスクがハングする（プログラミングエラー）。
    // 静かなデッドロックより即時失敗を選ぶ。
    anyhow::ensure!(concurrency > 0, "embed_all: concurrency must be > 0");
    let total = items.len();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut set: tokio::task::JoinSet<(usize, String, Result<Vec<f32>>)> =
        tokio::task::JoinSet::new();
    // JoinError（タスク panic / cancel）経路でも失敗 label を報告できるように、
    // task id -> label を控えておく（成功/embed エラー経路は tuple の label を使う）。
    let mut labels_by_task: HashMap<tokio::task::Id, String> = HashMap::with_capacity(total);

    for (idx, (label, text)) in items.into_iter().enumerate() {
        let client = client.clone();
        let semaphore = semaphore.clone();
        let label_for_join_error = label.clone();
        let handle = set.spawn(async move {
            // owned permit: 同時実行数を concurrency 件に絞る。Semaphore を close() する
            // 経路が無いため acquire_owned が Err になることは実運用上ないが、パニックせず
            // 呼び出し元まで context 付きでエラーを伝搬させる（観測性優先、unwrap しない）。
            let result = match semaphore.acquire_owned().await {
                Ok(_permit) => client.embed(&text).await,
                Err(err) => Err(anyhow::anyhow!("embed concurrency semaphore closed: {err}")),
            };
            (idx, label, result)
        });
        labels_by_task.insert(handle.id(), label_for_join_error);
    }

    let mut results: Vec<(usize, Vec<f32>)> = Vec::with_capacity(total);
    while let Some(joined) = set.join_next().await {
        // JoinError（タスク panic / cancel）自体も fail-closed の対象。
        let (idx, label, result) = joined.map_err(|err| {
            let label = labels_by_task
                .get(&err.id())
                .map(String::as_str)
                .unwrap_or("<unknown item>");
            anyhow::anyhow!(err)
                .context(format!("embed task for {label} panicked or was cancelled"))
        })?;
        let vector = result.with_context(|| format!("embed {label}"))?;
        results.push((idx, vector));
    }

    // 全 join が成功した場合のみここへ到達する（= results は必ず total 件）。
    // idx は enumerate 由来で一意なので、sort 後は入力順と一致する。
    results.sort_by_key(|(idx, _)| *idx);
    Ok(results.into_iter().map(|(_, vector)| vector).collect())
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
        let (title, body) = extract_main_text(html);
        assert_eq!(title, "SDカードが認識されない");
        assert!(body.contains("抜き差し"));
        assert!(!body.contains("予告なく変更"));
        assert!(!body.contains("目次"));
        // inline JS/CSS を本文に取り込まない（Google Sites 汚染対策）
        assert!(!body.contains("Symbol"));
        assert!(!body.contains("noise"));
        assert!(!body.contains("color:red"));
    }

    #[test]
    fn detects_model_without_false_positive_on_prefix() {
        // "ADC-V724X" は "ADC-V724" の前方一致だが、V724 単体としては誤検出しない。
        let models = detect_product_models("この設定は ADC-V724X 専用です。");
        assert_eq!(models, vec!["ADC-V724X".to_string()]);
    }

    #[test]
    fn detects_multiple_models_when_both_mentioned() {
        let models = detect_product_models("ADC-V724 と ADC-V724X の両方に対応します。");
        assert_eq!(
            models,
            vec!["ADC-V724".to_string(), "ADC-V724X".to_string()]
        );
    }
}
