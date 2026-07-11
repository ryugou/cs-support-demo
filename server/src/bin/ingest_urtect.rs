/// URTECT (Google Sites) manual crawler / ingest CLI. See
/// `.superpowers/sdd/task-9-brief.md` for the task contract this file implements.
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

    let http = reqwest::Client::builder()
        .user_agent("cs-support-mcp/ingest_urtect")
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
    let mut old_derived: HashMap<String, HashSet<String>> = HashMap::new();
    for e in &existing_snapshot.edges {
        if e.edge_type == "DESCRIBES" || e.edge_type == "MENTIONS_SIGNAL" {
            old_derived
                .entry(e.from_id.clone())
                .or_default()
                .insert(e.to_id.clone());
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
        let composite = [
            body.as_str(),                        // body（正規化済み）
            title.as_str(),                       // title
            breadcrumb.as_str(),                  // breadcrumb
            "",                                   // section_no（常に None）
            idx.to_string().as_str(),             // order
            parent_slug.as_deref().unwrap_or(""), // parent_slug
            entry.url.as_str(),                   // source_url
            product_models.join(",").as_str(),    // 検出済み型番（DESCRIBES 辺のもと）
            signal_values.join(",").as_str(),     // マッチ済み signal（MENTIONS_SIGNAL 辺のもと）
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
        }

        for model in &product_models {
            *describes_by_model.entry(model.clone()).or_insert(0) += 1;
        }
        if signal_values.is_empty() {
            zero_signal_sections.push(slug.clone());
        }

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
    for &model in KNOWN_MODELS {
        nodes.push(build_product_node(
            &args.schema,
            &ManualProductInput {
                model: model.to_string(),
                name: model.to_string(),
                aliases: Vec::new(),
            },
        ));
    }

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
