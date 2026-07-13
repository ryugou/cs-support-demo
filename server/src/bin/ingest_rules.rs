use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    harness::knowledge::harness_node_id,
    model::{GraphBuild, GraphNode},
    vegapunk::VegapunkClient,
};
use serde::Deserialize;
use serde_json::json;
use std::{env, fs, path::PathBuf};

/// 第1層 escalation_rule / 第2層 prohibited_domain の初期データ投入と
/// Step 1 加算スキーマの登録を行う CLI（spec S1-2 / S1-9）。
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
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    #[arg(long, default_value = "../schema/cs-schema.yml")]
    schema_file: PathBuf,
    #[arg(long, default_value = "data/rules.sample.json")]
    rules_file: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RulesFile {
    escalation_rules: Vec<RuleInput>,
    prohibited_domains: Vec<DomainInput>,
}

#[derive(Debug, Deserialize)]
struct RuleInput {
    rule_id: String,
    condition: Vec<String>,
    owner: Option<String>,
    route: String,
    binding: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DomainInput {
    domain_id: String,
    #[serde(default)]
    domain_signals: Vec<String>,
    #[serde(default)]
    pattern: Vec<String>,
    route: String,
    binding: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let token = read_token(&args)?;

    // 1. 加算スキーマ登録（既存 schema に node/edge type を足す）
    // 汎用テンプレを別テナントに登録する場合に備え name をリクエスト schema 名へ揃える
    // （同名なら no-op。vegapunk は create_schema 時に YAML name と登録名の一致を要求）。
    let schema_yaml = cs_support_mcp::manual::schema_ids::with_schema_name(
        &fs::read_to_string(&args.schema_file)
            .with_context(|| format!("read schema file {}", args.schema_file.display()))?,
        &args.schema,
    )?;
    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await?;
    tracing::info!(schema = %args.schema, "schema updated (additive)");

    // 2. 第1層ルール・第2層領域をノードとして投入（判定は載せない・材料のみ、I4）
    let rules: RulesFile = serde_json::from_str(
        &fs::read_to_string(&args.rules_file)
            .with_context(|| format!("read rules file {}", args.rules_file.display()))?,
    )
    .context("parse rules file")?;
    let mut nodes = Vec::new();
    for rule in &rules.escalation_rules {
        nodes.push(GraphNode {
            id: harness_node_id(&args.schema, "EscalationRule", &rule.rule_id),
            node_type: "EscalationRule".to_string(),
            attributes: vec![
                ("rule_id".to_string(), rule.rule_id.clone()),
                ("condition".to_string(), rule.condition.join(",")),
                ("owner".to_string(), rule.owner.clone().unwrap_or_default()),
                ("route".to_string(), rule.route.clone()),
                (
                    "binding".to_string(),
                    rule.binding
                        .clone()
                        .unwrap_or_else(|| "advisory".to_string()),
                ),
            ],
        });
    }
    for domain in &rules.prohibited_domains {
        nodes.push(GraphNode {
            id: harness_node_id(&args.schema, "ProhibitedDomain", &domain.domain_id),
            node_type: "ProhibitedDomain".to_string(),
            attributes: vec![
                ("domain_id".to_string(), domain.domain_id.clone()),
                (
                    "domain_signals".to_string(),
                    domain.domain_signals.join(","),
                ),
                ("pattern".to_string(), domain.pattern.join(",")),
                ("route".to_string(), domain.route.clone()),
                (
                    "binding".to_string(),
                    domain
                        .binding
                        .clone()
                        .unwrap_or_else(|| "mandatory".to_string()),
                ),
            ],
        });
    }
    let build = GraphBuild {
        nodes,
        edges: Vec::new(),
    };
    let (node_count, edge_count) = client.upsert_graph_low_level(build).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "upserted_nodes": node_count,
            "upserted_edges": edge_count,
        }))?
    );
    Ok(())
}

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
