/// homesec advisor 用材料（advisor_material）投入 CLI（Issue #34 Task 2）。
///
/// 背景: 別 LINE OA 向けホームセキュリティアドバイザは、vegapunk `homesec` schema の
/// `advisor_material` ノード（統計・自社製品・他社製品・シナリオ）を根拠材料として使う
/// （design doc `docs/superpowers/specs/2026-08-17-homesec-advisor-design.md` §5）。
/// `ingest_rules.rs` の流儀（Args を clap で定義 → JSON を読んでバリデーション →
/// `GraphBuild` を組み立てて `upsert_graph_low_level`）をそのまま踏襲する。
///
/// スコープ: このバイナリは材料投入のみを行う。advisor パイプライン本体
/// （`server/src/advisor/` モジュール、`homesec_advisor` バイナリ）は Task 3 以降の範囲。
use anyhow::{bail, Context, Result};
use clap::Parser;
use cs_support_mcp::{
    advisor::materials::is_well_formed_https_url,
    config::AppConfig,
    harness::knowledge::harness_node_id,
    manual::schema_ids::with_schema_name,
    model::{GraphBuild, GraphNode},
    vegapunk::VegapunkClient,
};
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, env, fs, path::Path, path::PathBuf};

#[derive(Debug, Parser)]
struct Args {
    /// homesec advisor の AppConfig（`--validate-only` 指定時は読まない）。
    #[arg(long, default_value = "config.homesec.toml")]
    config: PathBuf,
    /// advisor_material の入力ファイル（JSON 配列、`MaterialEntry` の形）。
    #[arg(long, default_value = "data/homesec/materials.json")]
    materials_file: PathBuf,
    /// vegapunk へは一切接続せず、パース + バリデーションのみ行って exit する。
    #[arg(long)]
    validate_only: bool,
    /// `--validate-only` 未指定時のみ使用する schema テンプレート YAML。
    #[arg(long, default_value = "../schema/homesec.yml")]
    schema_file: PathBuf,
    #[arg(long, default_value = "VEGAPUNK_BEARER_TOKEN")]
    token_env: String,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN_FILE")]
    token_file: Option<PathBuf>,
}

/// `materials.json` 1 件分の入力形式。design doc §5.1 の `advisor_material` 属性そのもの。
#[derive(Debug, Clone, Deserialize)]
struct MaterialEntry {
    material_key: String,
    kind: String,
    title_ja: String,
    body_ja: String,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    product_key: Option<String>,
    #[serde(default)]
    price_band: Option<String>,
    #[serde(default)]
    card_description: Option<String>,
    #[serde(default)]
    card_match_terms: Option<String>,
    /// カードの「商品ページを見る」ボタンの遷移先(design doc §5.1、Issue #34)。
    /// `own_product` / `partner_product` のみ意味を持つ。
    #[serde(default)]
    product_page_url: Option<String>,
}

const VALID_KINDS: [&str; 4] = ["statistic", "own_product", "partner_product", "scenario"];

/// `source_url` を必須とする kind（design doc §5.1）。
fn requires_source_url(kind: &str) -> bool {
    matches!(kind, "statistic" | "partner_product")
}

/// 非空判定は trim 後で行う（空白のみの値は意図しない空データとして拒否する）。
fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

/// `material_key` の slug 部分が `^[a-z0-9]+(-[a-z0-9]+)*$` に一致するか。
/// 先頭/末尾ハイフン・連続ハイフン・大文字・非英数字を許さない。
fn is_valid_slug(slug: &str) -> bool {
    if slug.is_empty() {
        return false;
    }
    slug.split('-').all(|seg| {
        !seg.is_empty()
            && seg
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

/// 1 エントリのバリデーション（spec ルール 1〜5）。違反理由は `material_key` を含めて
/// 運用者が該当エントリを即座に特定できるようにする。
fn validate_entry(entry: &MaterialEntry) -> Result<()> {
    // ルール1: kind は既定の4値のいずれか
    if !VALID_KINDS.contains(&entry.kind.as_str()) {
        bail!(
            "material {:?}: invalid kind {:?} (expected one of {VALID_KINDS:?})",
            entry.material_key,
            entry.kind
        );
    }
    // ルール3（先に検査): material_key / title_ja / body_ja は trim 後非空
    if is_blank(&entry.material_key) {
        bail!(
            "material entry has an empty material_key (kind={:?})",
            entry.kind
        );
    }
    if is_blank(&entry.title_ja) {
        bail!(
            "material {:?}: title_ja must not be empty",
            entry.material_key
        );
    }
    if is_blank(&entry.body_ja) {
        bail!(
            "material {:?}: body_ja must not be empty",
            entry.material_key
        );
    }
    // ルール2: material_key は `{kind}:{slug}` 形式
    let (prefix, slug) = entry.material_key.split_once(':').ok_or_else(|| {
        anyhow::anyhow!(
            "material {:?}: must be in `{{kind}}:{{slug}}` format (no ':' found)",
            entry.material_key
        )
    })?;
    if prefix != entry.kind {
        bail!(
            "material {:?}: material_key prefix {:?} does not match kind {:?}",
            entry.material_key,
            prefix,
            entry.kind
        );
    }
    if !is_valid_slug(slug) {
        bail!(
            "material {:?}: slug {:?} must be lowercase alphanumeric segments separated by single hyphens",
            entry.material_key,
            slug
        );
    }
    // ルール4: statistic / partner_product は source_url 必須
    if requires_source_url(&entry.kind) {
        let has_source_url = entry
            .source_url
            .as_deref()
            .map(|s| !is_blank(s))
            .unwrap_or(false);
        if !has_source_url {
            bail!(
                "material {:?}: source_url is required for kind {:?}",
                entry.material_key,
                entry.kind
            );
        }
    }
    // ルール5: card_description を持つなら title_ja が非空であること。
    // card_match_terms 省略時の照合語は title_ja / product_key の OR 判定（design doc §7.2）
    // であり、title_ja は上のルール3で常に非空が保証されるため、product_key の非空を
    // ここで要求してはならない（partner_product は product_key を持たないのが通常）。
    if entry.card_description.is_some() && is_blank(&entry.title_ja) {
        bail!(
            "material {:?}: card_description present but title_ja is empty",
            entry.material_key
        );
    }
    // ルール6: product_page_url を持つ場合は well-formed な https URL であること
    // （Issue #47 Critical指摘2: AdvisorMaterial::from_attributes 側の検証と揃える。ingest
    // 時点で弾くことで、不正な値が vegapunk へ投入されて初めて検索側の warn ログで気づく、
    // という発見の遅延を防ぐ）
    if let Some(url) = entry.product_page_url.as_deref() {
        if !is_blank(url) && !is_well_formed_https_url(url) {
            bail!(
                "material {:?}: product_page_url {:?} is not a well-formed absolute https:// URL",
                entry.material_key,
                url
            );
        }
    }
    Ok(())
}

/// 全件バリデーション。1件でも違反があれば中断し、その理由を返す
/// （何も upsert しない。`ingest_products.rs::parse_products` と同じ fail-closed 方針）。
///
/// 個別エントリの検査に加えて `material_key` の一意性も検査する（reviewer 一次レビュー
/// 指摘2・Critical）。`build_materials_graph` は `material_key` をそのままノード id にするため、
/// 重複があると `upsert_nodes`（`vegapunk.rs::upsert_graph_low_level`）が同一 id の 2 ノードを
/// 1 リクエストで受け取り、重複検出も除去もせず last-write-wins で先着エントリを黙って
/// 上書きする。CLI の JSON サマリは entries 件数（`total`）を出すため、この上書きは
/// 運用者からは見えない（コピー&ペーストで 1 件だけ material_key を直し忘れた、という
/// ありふれたミスが本番投入で静かにデータを消す）。判定は trim・大小文字正規化を行わない
/// 完全一致（`is_valid_slug` が既に小文字英数字とハイフンのみを強制しているため正規化の
/// 余地が無い）。
fn validate_all(entries: &[MaterialEntry]) -> Result<()> {
    for entry in entries {
        validate_entry(entry)?;
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in entries {
        *counts.entry(entry.material_key.as_str()).or_insert(0) += 1;
    }
    let duplicates: Vec<&str> = counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(key, _)| key)
        .collect();
    if !duplicates.is_empty() {
        bail!(
            "duplicate material_key found (advisor_material upsert is last-write-wins by node id; \
             earlier entries with the same material_key would be silently dropped): {duplicates:?}"
        );
    }
    Ok(())
}

/// JSON 文字列を `MaterialEntry` の配列としてパースする（トップレベル配列、ラップ無し）。
fn parse_materials(content: &str, path: &Path) -> Result<Vec<MaterialEntry>> {
    serde_json::from_str(content).with_context(|| {
        format!(
            "parse materials file {} as JSON (expected a top-level array of advisor_material entries)",
            path.display()
        )
    })
}

/// `--materials-file` を読んでパース + 全件バリデーションまで行う。
fn load_and_validate_materials(path: &Path) -> Result<Vec<MaterialEntry>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read materials file {}", path.display()))?;
    let entries = parse_materials(&content, path)?;
    validate_all(&entries)?;
    Ok(entries)
}

/// kind 別件数（JSON 出力の安定順のため `BTreeMap` を使う）。
fn kind_summary(entries: &[MaterialEntry]) -> BTreeMap<String, usize> {
    let mut summary = BTreeMap::new();
    for entry in entries {
        *summary.entry(entry.kind.clone()).or_insert(0) += 1;
    }
    summary
}

/// `advisor_material` ノードの `GraphBuild` を組み立てる純粋関数（edge は生成しない）。
/// 属性名は `schema/homesec.yml` の `advisor_material` 定義と完全一致させる。
fn build_materials_graph(schema: &str, entries: &[MaterialEntry]) -> GraphBuild {
    let nodes = entries
        .iter()
        .map(|entry| GraphNode {
            id: harness_node_id(schema, "advisor_material", &entry.material_key),
            node_type: "advisor_material".to_string(),
            attributes: vec![
                ("material_key".to_string(), entry.material_key.clone()),
                ("kind".to_string(), entry.kind.clone()),
                ("title_ja".to_string(), entry.title_ja.clone()),
                ("body_ja".to_string(), entry.body_ja.clone()),
                (
                    "source_url".to_string(),
                    entry.source_url.clone().unwrap_or_default(),
                ),
                (
                    "category".to_string(),
                    entry.category.clone().unwrap_or_default(),
                ),
                (
                    "product_key".to_string(),
                    entry.product_key.clone().unwrap_or_default(),
                ),
                (
                    "price_band".to_string(),
                    entry.price_band.clone().unwrap_or_default(),
                ),
                (
                    "card_description".to_string(),
                    entry.card_description.clone().unwrap_or_default(),
                ),
                (
                    "card_match_terms".to_string(),
                    entry.card_match_terms.clone().unwrap_or_default(),
                ),
                (
                    "product_page_url".to_string(),
                    entry.product_page_url.clone().unwrap_or_default(),
                ),
            ],
        })
        .collect();
    GraphBuild {
        nodes,
        edges: Vec::new(),
    }
}

/// `ingest_rules.rs::read_token` と同一の解決順（env 優先 → token file）。
/// Args の型が異なる（`ingest_rules.rs` は `endpoint` / `schema` も持つ）ため関数は複製する。
fn read_token(args: &Args) -> Result<String> {
    if let Ok(token) = env::var(&args.token_env) {
        if !token.trim().is_empty() {
            return Ok(token.trim().to_string());
        }
    }
    if let Some(path) = &args.token_file {
        return fs::read_to_string(path)
            .with_context(|| format!("read token file {}", path.display()))
            .map(|s| s.trim().to_string());
    }
    anyhow::bail!(
        "vegapunk bearer token is required (env {} or VEGAPUNK_BEARER_TOKEN_FILE)",
        args.token_env
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let entries = load_and_validate_materials(&args.materials_file)?;
    let by_kind = kind_summary(&entries);

    if args.validate_only {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "materials_file": args.materials_file.display().to_string(),
                "status": "ok",
                "total": entries.len(),
                "by_kind": by_kind,
            }))?
        );
        return Ok(());
    }

    let app_config = AppConfig::load(&args.config)
        .with_context(|| format!("load config {}", args.config.display()))?;
    let project = app_config
        .projects
        .iter()
        .find(|p| p.project_id == "homesec")
        .with_context(|| {
            format!(
                "config {} has no project with project_id \"homesec\"",
                args.config.display()
            )
        })?;
    let schema = project.schema.clone();

    let token = read_token(&args)?;

    // 加算スキーマ登録（既存 schema に node/edge type を足す。既に homesec が登録済みなら no-op）。
    let schema_yaml = with_schema_name(
        &fs::read_to_string(&args.schema_file)
            .with_context(|| format!("read schema file {}", args.schema_file.display()))?,
        &schema,
    )?;
    let client = VegapunkClient::connect(&app_config.vegapunk_endpoint, &token).await?;
    client.create_or_update_schema(&schema, schema_yaml).await?;
    tracing::info!(schema = %schema, "schema updated (additive)");

    let build = build_materials_graph(&schema, &entries);
    let (node_count, edge_count) = client.upsert_graph_low_level(build).await?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": schema,
            "materials_file": args.materials_file.display().to_string(),
            "total": entries.len(),
            "by_kind": by_kind,
            "upserted_nodes": node_count,
            "upserted_edges": edge_count,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全ルールを満たす最小エントリ。個々のテストで違反させたいフィールドだけ上書きする。
    fn valid_entry(kind: &str, slug: &str) -> MaterialEntry {
        MaterialEntry {
            material_key: format!("{kind}:{slug}"),
            kind: kind.to_string(),
            title_ja: "タイトル".to_string(),
            body_ja: "本文".to_string(),
            source_url: if requires_source_url(kind) {
                Some("https://example.com/source".to_string())
            } else {
                None
            },
            category: None,
            product_key: None,
            price_band: None,
            card_description: None,
            card_match_terms: None,
            product_page_url: None,
        }
    }

    // --- ルール1: kind は既定の4値のいずれか ---

    #[test]
    fn all_four_kinds_pass_validation() {
        for kind in VALID_KINDS {
            let entry = valid_entry(kind, "sample-slug");
            assert!(
                validate_entry(&entry).is_ok(),
                "kind {kind:?} must be accepted"
            );
        }
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.kind = "unknown_kind".to_string();
        entry.material_key = "unknown_kind:sample-slug".to_string();
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("invalid kind"),
            "expected invalid kind error, got: {err}"
        );
    }

    // --- ルール2: material_key は {kind}:{slug} 形式 ---

    #[test]
    fn material_key_matching_kind_and_slug_format_passes() {
        let entry = valid_entry("statistic", "musimari-46percent");
        assert!(validate_entry(&entry).is_ok());
    }

    #[test]
    fn material_key_prefix_mismatch_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.material_key = "own_product:sample-slug".to_string();
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("does not match kind"),
            "expected prefix mismatch error, got: {err}"
        );
    }

    #[test]
    fn material_key_without_colon_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.material_key = "statistic-sample-slug".to_string();
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("must be in"),
            "expected missing ':' error, got: {err}"
        );
    }

    #[test]
    fn slug_with_uppercase_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.material_key = "statistic:Sample-Slug".to_string();
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("slug"),
            "expected slug format error, got: {err}"
        );
    }

    #[test]
    fn slug_with_underscore_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.material_key = "statistic:sample_slug".to_string();
        assert!(validate_entry(&entry).is_err());
    }

    #[test]
    fn slug_with_leading_hyphen_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.material_key = "statistic:-sample-slug".to_string();
        assert!(validate_entry(&entry).is_err());
    }

    #[test]
    fn slug_with_double_hyphen_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.material_key = "statistic:sample--slug".to_string();
        assert!(validate_entry(&entry).is_err());
    }

    #[test]
    fn slug_that_is_empty_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.material_key = "statistic:".to_string();
        assert!(validate_entry(&entry).is_err());
    }

    // --- ルール3: material_key / title_ja / body_ja は trim 後非空 ---

    #[test]
    fn empty_title_ja_is_rejected() {
        let mut entry = valid_entry("scenario", "sample-slug");
        entry.title_ja = "   ".to_string();
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("title_ja"),
            "expected title_ja error, got: {err}"
        );
    }

    #[test]
    fn empty_body_ja_is_rejected() {
        let mut entry = valid_entry("scenario", "sample-slug");
        entry.body_ja = "\n\t".to_string();
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("body_ja"),
            "expected body_ja error, got: {err}"
        );
    }

    #[test]
    fn empty_material_key_is_rejected() {
        let mut entry = valid_entry("scenario", "sample-slug");
        entry.material_key = "".to_string();
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("material_key"),
            "expected material_key error, got: {err}"
        );
    }

    // --- ルール4: statistic / partner_product は source_url 必須 ---

    #[test]
    fn statistic_without_source_url_is_rejected() {
        let mut entry = valid_entry("statistic", "sample-slug");
        entry.source_url = None;
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("source_url"),
            "expected source_url error, got: {err}"
        );
    }

    #[test]
    fn partner_product_with_blank_source_url_is_rejected() {
        let mut entry = valid_entry("partner_product", "sample-slug");
        entry.source_url = Some("   ".to_string());
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("source_url"),
            "expected source_url error, got: {err}"
        );
    }

    #[test]
    fn own_product_without_source_url_passes() {
        let entry = valid_entry("own_product", "sample-slug");
        assert!(entry.source_url.is_none());
        assert!(validate_entry(&entry).is_ok());
    }

    #[test]
    fn scenario_without_source_url_passes() {
        let entry = valid_entry("scenario", "sample-slug");
        assert!(entry.source_url.is_none());
        assert!(validate_entry(&entry).is_ok());
    }

    // --- ルール5: card_description ---

    #[test]
    fn partner_product_card_without_match_terms_or_product_key_passes() {
        // design doc §7.2: card_match_terms 省略時の照合語は title_ja / product_key の
        // OR 判定。title_ja は常に必須非空なので、product_key 欠落を理由に validation を
        // 落としてはならない（partner_product は product_key を持たないのが通常）。
        let mut entry = valid_entry("partner_product", "sample-slug");
        entry.card_description = Some("1行説明".to_string());
        assert!(entry.card_match_terms.is_none());
        assert!(entry.product_key.is_none());
        assert!(
            validate_entry(&entry).is_ok(),
            "partner_product with card_description but no card_match_terms/product_key must pass"
        );
    }

    #[test]
    fn own_product_card_with_product_key_passes() {
        let mut entry = valid_entry("own_product", "adc-v724");
        entry.product_key = Some("ADC-V724".to_string());
        entry.card_description = Some("屋外対応".to_string());
        assert!(validate_entry(&entry).is_ok());
    }

    // --- ルール6: product_page_url は well-formed な https URL であること（Issue #47） ---

    #[test]
    fn product_page_url_that_is_well_formed_https_passes() {
        let mut entry = valid_entry("own_product", "adc-v724");
        entry.product_page_url = Some("https://example.com/products/adc-v724".to_string());
        assert!(validate_entry(&entry).is_ok());
    }

    #[test]
    fn plain_http_product_page_url_is_rejected() {
        let mut entry = valid_entry("own_product", "adc-v724");
        entry.product_page_url = Some("http://example.com/products/adc-v724".to_string());
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("product_page_url"),
            "expected product_page_url error, got: {err}"
        );
    }

    #[test]
    fn hostless_https_product_page_url_is_rejected() {
        let mut entry = valid_entry("own_product", "adc-v724");
        entry.product_page_url = Some("https://".to_string());
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("product_page_url"),
            "expected product_page_url error, got: {err}"
        );
    }

    #[test]
    fn product_page_url_with_userinfo_impersonating_a_host_is_rejected() {
        let mut entry = valid_entry("own_product", "adc-v724");
        entry.product_page_url = Some("https://example.com@evil.example/path".to_string());
        let err = validate_entry(&entry).unwrap_err();
        assert!(
            err.to_string().contains("product_page_url"),
            "expected product_page_url error, got: {err}"
        );
    }

    #[test]
    fn blank_product_page_url_is_treated_as_absent_and_passes() {
        let mut entry = valid_entry("own_product", "adc-v724");
        entry.product_page_url = Some("   ".to_string());
        assert!(validate_entry(&entry).is_ok());
    }

    // --- 複数エントリの一括バリデーション ---

    #[test]
    fn validate_all_stops_at_first_invalid_entry() {
        let good = valid_entry("scenario", "good-one");
        let mut bad = valid_entry("scenario", "bad-one");
        bad.title_ja = "".to_string();
        let entries = vec![good, bad];
        let err = validate_all(&entries).unwrap_err();
        assert!(err.to_string().contains("title_ja"));
    }

    #[test]
    fn validate_all_passes_when_every_entry_is_valid() {
        let entries = vec![
            valid_entry("statistic", "a"),
            valid_entry("own_product", "b"),
            valid_entry("partner_product", "c"),
            valid_entry("scenario", "d"),
        ];
        assert!(validate_all(&entries).is_ok());
    }

    // --- reviewer 一次レビュー指摘2: material_key の重複検出（fail-closed） ---

    #[test]
    fn validate_all_rejects_duplicate_material_key() {
        // 2 件とも個別には valid だが material_key が同一（コピー&ペーストで直し忘れた想定）。
        // 検出しないと build_materials_graph が同一ノード id を 2 件生成し、
        // upsert_nodes で先着エントリが黙って上書きされる（指摘2の失敗シナリオ）。
        let mut first = valid_entry("scenario", "apartment-solo-living");
        first.body_ja = "1件目の本文".to_string();
        let mut second = valid_entry("scenario", "apartment-solo-living");
        second.body_ja = "2件目の本文".to_string();
        let entries = vec![first, second];

        let err = validate_all(&entries).unwrap_err();
        assert!(
            err.to_string().contains("scenario:apartment-solo-living"),
            "expected duplicate material_key error to name the key, got: {err}"
        );
    }

    #[test]
    fn validate_all_rejects_when_any_material_key_among_many_duplicates() {
        // 45 件規模での「1件だけ直し忘れ」を模した回帰: 重複しない大量のエントリの中に
        // 1 組だけ重複が混ざっていても検出できること。
        let mut entries: Vec<MaterialEntry> = (0..10)
            .map(|i| valid_entry("statistic", &format!("unique-{i}")))
            .collect();
        entries.push(valid_entry("statistic", "unique-3"));
        let err = validate_all(&entries).unwrap_err();
        assert!(
            err.to_string().contains("statistic:unique-3"),
            "expected duplicate material_key error to name the key, got: {err}"
        );
    }

    // --- parse_materials ---

    #[test]
    fn parse_materials_reads_top_level_array() {
        let json = r#"[
            {"material_key": "scenario:a", "kind": "scenario", "title_ja": "t", "body_ja": "b"}
        ]"#;
        let entries = parse_materials(json, Path::new("materials.json")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].material_key, "scenario:a");
        assert!(entries[0].source_url.is_none());
    }

    #[test]
    fn parse_materials_rejects_non_json() {
        let err = parse_materials("not json", Path::new("materials.json")).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("materials.json"),
            "expected error to include the file path, got: {message}"
        );
    }

    #[test]
    fn parse_materials_rejects_object_wrapper() {
        // spec: トップレベル JSON 配列。`{"materials": [...]}` のようなラップはしない。
        let json = r#"{"materials": []}"#;
        assert!(parse_materials(json, Path::new("materials.json")).is_err());
    }

    // --- kind_summary ---

    #[test]
    fn kind_summary_counts_per_kind() {
        let entries = vec![
            valid_entry("statistic", "a"),
            valid_entry("statistic", "b"),
            valid_entry("own_product", "c"),
        ];
        let summary = kind_summary(&entries);
        assert_eq!(summary.get("statistic"), Some(&2));
        assert_eq!(summary.get("own_product"), Some(&1));
        assert_eq!(summary.get("partner_product"), None);
    }

    // --- build_materials_graph ---

    #[test]
    fn build_materials_graph_maps_entry_to_advisor_material_node() {
        let mut entry = valid_entry("own_product", "adc-v724");
        entry.product_key = Some("ADC-V724".to_string());
        entry.category = Some("monitoring".to_string());
        entry.card_description = Some("屋外対応".to_string());
        entry.card_match_terms = Some("ADC-V724,屋外".to_string());
        entry.product_page_url = Some("https://example.com/products/adc-v724".to_string());
        let build = build_materials_graph("homesec", std::slice::from_ref(&entry));

        assert_eq!(
            build.edges.len(),
            0,
            "advisor_material ingest creates no edges"
        );
        assert_eq!(build.nodes.len(), 1);
        let node = &build.nodes[0];
        assert_eq!(
            node.id,
            harness_node_id("homesec", "advisor_material", "own_product:adc-v724")
        );
        assert_eq!(node.node_type, "advisor_material");
        let attrs: std::collections::HashMap<_, _> = node.attributes.iter().cloned().collect();
        assert_eq!(attrs.get("material_key").unwrap(), "own_product:adc-v724");
        assert_eq!(attrs.get("kind").unwrap(), "own_product");
        assert_eq!(attrs.get("title_ja").unwrap(), "タイトル");
        assert_eq!(attrs.get("body_ja").unwrap(), "本文");
        assert_eq!(attrs.get("product_key").unwrap(), "ADC-V724");
        assert_eq!(attrs.get("category").unwrap(), "monitoring");
        assert_eq!(attrs.get("card_description").unwrap(), "屋外対応");
        assert_eq!(attrs.get("card_match_terms").unwrap(), "ADC-V724,屋外");
        assert_eq!(
            attrs.get("product_page_url").unwrap(),
            "https://example.com/products/adc-v724"
        );
        // 未設定の optional 属性は空文字列で埋める（vegapunk 側の属性欠落を避けるため）。
        assert_eq!(attrs.get("source_url").unwrap(), "");
        assert_eq!(attrs.get("price_band").unwrap(), "");
    }

    #[test]
    fn build_materials_graph_fills_missing_optionals_with_empty_string() {
        let entry = valid_entry("scenario", "apartment-solo-living");
        let build = build_materials_graph("homesec", std::slice::from_ref(&entry));
        let attrs: std::collections::HashMap<_, _> =
            build.nodes[0].attributes.iter().cloned().collect();
        for key in [
            "source_url",
            "category",
            "product_key",
            "price_band",
            "card_description",
            "card_match_terms",
            "product_page_url",
        ] {
            assert_eq!(
                attrs.get(key).unwrap(),
                "",
                "optional attribute {key} must default to empty string"
            );
        }
    }

    #[test]
    fn build_materials_graph_is_idempotent_across_calls() {
        let entries = vec![
            valid_entry("statistic", "a"),
            valid_entry("own_product", "b"),
            valid_entry("partner_product", "c"),
            valid_entry("scenario", "d"),
        ];
        let first = build_materials_graph("homesec", &entries);
        let second = build_materials_graph("homesec", &entries);
        assert_eq!(first, second);
    }

    // --- load_and_validate_materials (seed fixture の統合テスト) ---

    #[test]
    fn seed_materials_file_parses_and_validates() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/homesec/materials.json"
        ));
        let entries = load_and_validate_materials(path)
            .expect("server/data/homesec/materials.json must parse and validate");
        assert!(!entries.is_empty());
        let summary = kind_summary(&entries);
        for kind in VALID_KINDS {
            assert!(
                summary.contains_key(kind),
                "seed materials.json must contain at least one entry of kind {kind:?}"
            );
        }
    }
}
