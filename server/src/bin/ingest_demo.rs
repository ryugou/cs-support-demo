use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    ingest::{build_graph, load_catalog},
    translate::load_glossary,
    vegapunk::VegapunkClient,
};
use serde_json::json;
use std::{env, fs, path::PathBuf};

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
    #[arg(long, default_value = "../schema/cs-schema.yml")]
    schema_file: PathBuf,
    #[arg(long, default_value = "data/manual.sample.json")]
    manual_file: PathBuf,
    #[arg(long, default_value = "data/glossary.json")]
    glossary_file: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let token = env::var(&args.token_env)
        .with_context(|| format!("missing bearer token env {}", args.token_env))?;

    let schema_yaml = fs::read_to_string(&args.schema_file)
        .with_context(|| format!("read schema file {}", args.schema_file.display()))?;
    let catalog = load_catalog(&args.manual_file)?;
    let glossary = load_glossary(&args.glossary_file)?;
    let graph = build_graph(&args.schema, &catalog, &glossary)?;

    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await?;
    let expected_nodes = graph.nodes.len();
    let expected_edges = graph.edges.len();
    let (nodes, edges) = client.upsert_graph_low_level(graph).await?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "expected_nodes": expected_nodes,
            "expected_edges": expected_edges,
            "upserted_nodes": nodes,
            "upserted_edges": edges
        }))?
    );
    Ok(())
}
