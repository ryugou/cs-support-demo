/// 製品マスタ（vegapunk Product ノード）投入 CLI。
///
/// 背景（Issue #6）: 従来 `ingest_urtect.rs` の `KNOWN_MODELS` 定数が型番検出語彙と
/// Product ノード生成マスタの両方を兼ねており、製品を 1 件追加するたびにコード修正 →
/// ビルド → イメージ再ビルド → 再デプロイが必要だった。本 CLI を製品マスタ投入の唯一の
/// 経路とし、vegapunk の Product ノードを正本にする。全体リセット後の seed 投入と、
/// 製品追加の両方に使う（`CLAUDE.md` の運用手順を参照）。
///
/// 設計・検証記録: `docs/superpowers/specs/2026-07-22-product-master-vegapunk-design.md`。
use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    manual::{
        ingest_model::{build_product_node, ManualProductInput},
        schema_ids::{manual_node_id, KIND_PRODUCT},
        vectors::{embed_all, vector_entry, EMBED_CONCURRENCY},
    },
    vegapunk::VegapunkClient,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
};

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
    #[arg(long, default_value = "data/urtect/products.json")]
    products_file: PathBuf,
    /// embed / upsert_vectors を一切呼ばずスキップする（ベクトル基盤未整備な環境向けの
    /// 明示的な opt-out）。未指定時は embed 失敗を fail closed で扱う。
    #[arg(long)]
    no_vectors: bool,
}

/// products.json 1 件分の入力形式。`aliases` 省略時は空配列として扱う。
#[derive(Debug, Deserialize, Clone)]
struct ProductEntry {
    model: String,
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
}

/// token 解決: 既定は --token-file（CLAUDE.md のローカル/GCE 起動手順と同じ経路）。
/// ファイルが無い/読めない場合のみ --token-env にフォールバックする。
/// `ingest_urtect.rs` の `read_token` と同じ挙動（Args の型が異なるため関数は複製する）。
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

/// products.json の内容（生 JSON 文字列）をパース・検証する。fail closed: `model` が
/// 空、または重複（大文字小文字無視）するエントリが 1 件でもあれば全体をエラーにする
/// （何も upsert しない）。`path` はエラーメッセージにのみ使う（呼び出し側のファイル
/// パスをそのまま表示し、運用者が対象ファイルを即座に特定できるようにする）。
fn parse_products(content: &str, path: &Path) -> Result<Vec<ProductEntry>> {
    let entries: Vec<ProductEntry> = serde_json::from_str(content).with_context(|| {
        format!(
            "parse products file {} as JSON (expected an array of {{model, name, aliases}})",
            path.display()
        )
    })?;

    let mut seen_models: HashSet<String> = HashSet::with_capacity(entries.len());
    for entry in &entries {
        if entry.model.trim().is_empty() {
            anyhow::bail!(
                "products file {} contains an entry with an empty model (name={:?}); \
                 fix the entry and re-run ingest_products — nothing was upserted",
                path.display(),
                entry.name
            );
        }
        let key = entry.model.to_uppercase();
        if !seen_models.insert(key) {
            anyhow::bail!(
                "products file {} contains a duplicate model (case-insensitive): {:?}; \
                 each model must be unique — nothing was upserted",
                path.display(),
                entry.model
            );
        }
    }
    Ok(entries)
}

/// `--products-file` を読んでパース・検証する。
fn load_products(path: &Path) -> Result<Vec<ProductEntry>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read products file {}", path.display()))?;
    parse_products(&content, path)
}

/// product embed テキストの組み立て。`name` + `aliases`（空白区切り）で、
/// 旧 `ingest_urtect` の product embed が使っていた組み立てと同一にする
/// （embed 対象を変えると同じ入力から違うベクトルが生成され、既存 vector との
/// 一貫性が失われるため）。
fn product_embed_text(input: &ManualProductInput) -> String {
    format!("{} {}", input.name, input.aliases.join(" "))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    // vector metadata の timestamp_ms は run 開始時に 1 回だけ取得する（ingest_urtect と
    // 同じ理由: entry ごとに now を取ると同一 run 内で値がばらつき決定性が失われる）。
    let ingest_timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before unix epoch; cannot compute vector metadata timestamp_ms")?
        .as_millis()
        .to_string();
    let token = read_token(&args)?;

    let entries = load_products(&args.products_file)?;

    // 汎用テンプレの name をテナント schema 名に差し替える（vegapunk は name 一致を要求）。
    // 全体リセット直後は本 CLI が最初に走るため、schema 登録をここでも担保する。
    let schema_yaml = cs_support_mcp::manual::schema_ids::with_schema_name(
        &fs::read_to_string(&args.schema_file)
            .with_context(|| format!("read schema file {}", args.schema_file.display()))?,
        &args.schema,
    )?;

    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await?;

    let product_inputs: Vec<ManualProductInput> = entries
        .into_iter()
        .map(|entry| ManualProductInput {
            model: entry.model,
            name: entry.name,
            aliases: entry.aliases,
        })
        .collect();
    let nodes: Vec<_> = product_inputs
        .iter()
        .map(|input| build_product_node(&args.schema, input))
        .collect();

    // vector 投入 → node 投入の順（ingest_urtect と同じ理由）: embed/upsert_vectors が
    // 途中失敗した場合、まだ node が存在しない/古いままなので、再実行すれば同じ内容を
    // そのまま再投入できる（中途半端な「node はあるが vector が無い」状態を作らない）。
    let vectors_skipped = args.no_vectors;
    let upserted_vectors = if vectors_skipped {
        0
    } else {
        let mut texts: Vec<String> = Vec::with_capacity(product_inputs.len());
        let items: Vec<(String, String)> = product_inputs
            .iter()
            .map(|input| {
                let text = product_embed_text(input);
                texts.push(text.clone());
                (format!("product {}", input.model), text)
            })
            .collect();
        // 1 件でも embed 失敗したら fail closed（embed_all の契約。中途半端なベクトル
        // 状態を後続の upsert_nodes に渡さない）。
        let vectors = embed_all(&client, items, EMBED_CONCURRENCY).await?;
        let mut entries: Vec<cs_support_mcp::vegapunk::VectorUpsertEntry> =
            Vec::with_capacity(product_inputs.len());
        for ((input, text), vector) in product_inputs.iter().zip(texts.iter()).zip(vectors) {
            let id = manual_node_id(&args.schema, KIND_PRODUCT, &input.model);
            entries.push(vector_entry(
                id,
                vector,
                text,
                KIND_PRODUCT,
                &ingest_timestamp_ms,
            ));
        }
        let entries_len = entries.len();
        client
            .upsert_vectors(entries)
            .await
            .with_context(|| format!("upsert vectors (entries={entries_len})"))?
    };

    let upserted_nodes = client.upsert_nodes(nodes).await?;

    tracing::info!(
        schema = %args.schema,
        products_file = %args.products_file.display(),
        ingested_products = product_inputs.len(),
        upserted_nodes,
        upserted_vectors,
        vectors_skipped,
        "product master ingest complete"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "products_file": args.products_file.display().to_string(),
            "ingested_products": product_inputs.len(),
            "upserted_nodes": upserted_nodes,
            "upserted_vectors": upserted_vectors,
            "vectors_skipped": vectors_skipped,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_products_json() {
        let json = r#"[
            {"model": "ADC-V724", "name": "ADC-V724", "aliases": []},
            {"model": "ADC-V724X", "name": "ADC-V724X", "aliases": ["V724 Pro"]}
        ]"#;
        let entries = parse_products(json, Path::new("products.json")).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].model, "ADC-V724");
        assert_eq!(entries[1].aliases, vec!["V724 Pro".to_string()]);
    }

    #[test]
    fn missing_aliases_field_defaults_to_empty() {
        let json = r#"[{"model": "ADC-V724", "name": "ADC-V724"}]"#;
        let entries = parse_products(json, Path::new("products.json")).unwrap();
        assert_eq!(entries[0].aliases, Vec::<String>::new());
    }

    #[test]
    fn rejects_empty_model() {
        let json = r#"[{"model": "", "name": "no model here", "aliases": []}]"#;
        let err = parse_products(json, Path::new("products.json")).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("empty model"),
            "expected error to mention empty model, got: {message}"
        );
        assert!(
            message.contains("products.json"),
            "expected error to include the file path, got: {message}"
        );
    }

    #[test]
    fn rejects_duplicate_model_case_insensitive() {
        let json = r#"[
            {"model": "ADC-V724", "name": "A", "aliases": []},
            {"model": "adc-v724", "name": "B", "aliases": []}
        ]"#;
        let err = parse_products(json, Path::new("products.json")).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("duplicate model"),
            "expected error to mention duplicate model, got: {message}"
        );
    }

    #[test]
    fn rejects_invalid_json() {
        let err = parse_products("not json at all", Path::new("products.json")).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("products.json"),
            "expected error to include the file path, got: {message}"
        );
    }

    #[test]
    fn product_embed_text_joins_name_and_aliases() {
        let input = ManualProductInput {
            model: "ADC-V724".to_string(),
            name: "ADC-V724".to_string(),
            aliases: vec!["V724 Pro".to_string(), "旧型番".to_string()],
        };
        assert_eq!(product_embed_text(&input), "ADC-V724 V724 Pro 旧型番");
    }
}
