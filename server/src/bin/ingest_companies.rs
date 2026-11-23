use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::json;

use cs_support_mcp::harness::name_confirmation::company::{
    parse_company_file, sync_registered_companies,
};
use cs_support_mcp::manual::schema_ids::with_schema_name;
use cs_support_mcp::vegapunk::{read_cli_bearer_token, VegapunkClient};

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
    #[arg(
        long,
        env = "VEGAPUNK_BEARER_TOKEN_FILE",
        default_value = "/private/tmp/vegapunk-bearer-token"
    )]
    token_file: Option<PathBuf>,
    #[arg(long, default_value = "../schema/cs-support.yml")]
    schema_file: PathBuf,
    #[arg(long, default_value = "data/urtect/companies.json")]
    companies_file: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let companies = parse_company_file(
        &fs::read_to_string(&args.companies_file)
            .with_context(|| format!("failed to read {}", args.companies_file.display()))?,
    )
    .with_context(|| format!("invalid {}", args.companies_file.display()))?;
    let token = read_cli_bearer_token(args.token_file.as_deref(), &args.token_env)?;
    let schema_yaml = with_schema_name(
        &fs::read_to_string(&args.schema_file)
            .with_context(|| format!("failed to read {}", args.schema_file.display()))?,
        &args.schema,
    )?;
    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await?;
    let summary = sync_registered_companies(&client, &args.schema, &companies).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "companies_file": args.companies_file.display().to_string(),
            "active_companies": summary.active,
            "retired_companies": summary.retired,
        }))?
    );
    Ok(())
}
