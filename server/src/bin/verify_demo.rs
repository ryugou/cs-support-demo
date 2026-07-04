use anyhow::{anyhow, Context, Result};
use clap::Parser;
use cs_support_mcp::{mcp::ToolService, vegapunk::VegapunkClient};
use serde_json::json;
use std::env;

#[derive(Debug, Parser)]
struct Args {
    #[arg(
        long,
        env = "VEGAPUNK_ENDPOINT",
        default_value = "http://vegapunk.local:6840"
    )]
    endpoint: String,
    #[arg(long, default_value = "sivira-cs-demo")]
    schema: String,
    #[arg(long, default_value = "VEGAPUNK_BEARER_TOKEN")]
    token_env: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let token = env::var(&args.token_env)
        .with_context(|| format!("missing bearer token env {}", args.token_env))?;
    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    let tools = ToolService::new(client.clone());

    let products = client
        .query_nodes(&args.schema, "product", Vec::new(), 100)
        .await?;
    let sections = client
        .query_nodes(&args.schema, "section", Vec::new(), 1000)
        .await?;
    let specs = client
        .query_nodes(&args.schema, "spec", Vec::new(), 100)
        .await?;
    let snapshot = client.graph_snapshot(&args.schema, 5000).await?;
    let reset_hits = tools
        .search_manual(&args.schema, "リセット穴を何秒押す", Some("SVR-HB100"), 5)
        .await?;
    let mist_hits = tools
        .search_manual(&args.schema, "白い粉を防ぐには", Some("SVR-CM240"), 5)
        .await?;
    let candidates = tools.resolve_product(&args.schema, "HB100").await?;
    let product = tools.get_product(&args.schema, "SVR-CM240").await?;
    let section = tools
        .get_section(&args.schema, "svr-hb100-user-guide#factory-reset")
        .await?;
    let vector_search = client
        .search(&args.schema, "チャイルドロック", 3)
        .await
        .unwrap_or_default();

    if products.len() < 3 {
        return Err(anyhow!(
            "expected at least 3 products, got {}",
            products.len()
        ));
    }
    if sections.len() < 8 {
        return Err(anyhow!(
            "expected at least 8 sections, got {}",
            sections.len()
        ));
    }
    if specs.len() < 5 {
        return Err(anyhow!("expected at least 5 specs, got {}", specs.len()));
    }
    if reset_hits.is_empty() || mist_hits.is_empty() || candidates.is_empty() {
        return Err(anyhow!(
            "manual search or resolve verification returned no results"
        ));
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "products": products.len(),
            "sections": sections.len(),
            "specs": specs.len(),
            "snapshot_nodes": snapshot.nodes.len(),
            "snapshot_edges": snapshot.edges.len(),
            "snapshot_truncated": snapshot.truncated,
            "resolve_hb100": candidates,
            "search_reset": reset_hits,
            "search_mist": mist_hits,
            "product_cm240": product,
            "section_factory_reset": section,
            "vector_search_result_count": vector_search.len()
        }))?
    );
    Ok(())
}
