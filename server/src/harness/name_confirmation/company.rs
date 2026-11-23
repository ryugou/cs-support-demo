use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use super::matcher::name_key;
use crate::harness::knowledge::csv_list;
use crate::harness::product_gate::validated_catalog_name;
use crate::manual::schema_ids::{manual_node_id, KIND_COMPANY};
use crate::model::GraphNode;
use crate::proto::graphrag::NodeResult;
use crate::vegapunk::VegapunkClient;

const MAX_COMPANY_KEY_CHARS: usize = 64;
const COMPANY_QUERY_LIMIT: i32 = 1000;
const ACTIVE_STATUS: &str = "active";
const RETIRED_STATUS: &str = "retired";
const ALIAS_SEPARATOR: char = ',';

/// A company name registered in the project schema, with its aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredCompany {
    /// Stable key that identifies the company across renames.
    pub key: String,
    /// Official company name shown to customers.
    pub name: String,
    /// Other notations of the same company that count as naming it exactly.
    pub aliases: Vec<String>,
}

/// Counts of the company nodes that one synchronization wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompanySyncSummary {
    /// Companies written as active.
    pub active: usize,
    /// Registered companies that the file no longer lists, written as retired.
    pub retired: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompanyFileEntry {
    key: String,
    name: String,
    aliases: Vec<String>,
}

/// Parses and validates a company names file, a JSON array of `{"key", "name", "aliases"}` objects.
///
/// # Errors
///
/// Returns an error when the content is not such an array, when a key is malformed or repeated,
/// when a name or alias is blank, holds a control character or a comma, or is too long, or when
/// two names or aliases normalize to the same notation.
pub fn parse_company_file(content: &str) -> Result<Vec<RegisteredCompany>> {
    let entries: Vec<CompanyFileEntry> =
        serde_json::from_str(content).context("failed to parse companies file")?;
    let companies = entries
        .into_iter()
        .map(|entry| validated_company(entry.key, &entry.name, &entry.aliases))
        .collect::<Result<Vec<_>>>()?;
    ensure_distinct_notations(&companies)?;
    Ok(companies)
}

/// Writes `companies` as the active company names of `schema` and writes every registered company
/// that `companies` no longer lists as retired.
///
/// # Errors
///
/// Returns an error when vegapunk rejects the query or the write, when a registered company node
/// has no key, or when the schema holds as many company nodes as one query can return.
pub async fn sync_registered_companies(
    client: &VegapunkClient,
    schema: &str,
    companies: &[RegisteredCompany],
) -> Result<CompanySyncSummary> {
    let existing = query_company_nodes(client, schema).await?;
    let (nodes, summary) = company_sync_nodes(schema, companies, &existing)?;
    client
        .upsert_nodes(nodes)
        .await
        .context("failed to write company nodes")?;
    Ok(summary)
}

pub(crate) async fn query_company_nodes(
    client: &VegapunkClient,
    schema: &str,
) -> Result<Vec<NodeResult>> {
    let nodes = client
        .query_nodes(schema, KIND_COMPANY, Vec::new(), COMPANY_QUERY_LIMIT)
        .await
        .context("failed to read company nodes")?;
    if nodes.len() >= COMPANY_QUERY_LIMIT as usize {
        bail!("invalid company nodes: schema {schema} holds at least {COMPANY_QUERY_LIMIT} nodes, more than one query reads");
    }
    Ok(nodes)
}

pub(crate) fn active_companies(nodes: Vec<NodeResult>) -> Vec<RegisteredCompany> {
    nodes
        .into_iter()
        .filter_map(|node| match company_from_attributes(&node.attributes) {
            Ok((company, CompanyStatus::Active)) => Some(company),
            Ok((_, CompanyStatus::Retired)) => None,
            Err(err) => {
                tracing::warn!(
                    node_id = %node.node_id,
                    error = %err,
                    "excluding an invalid company node from the name catalog"
                );
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompanyStatus {
    Active,
    Retired,
}

fn company_from_attributes(
    attributes: &HashMap<String, String>,
) -> Result<(RegisteredCompany, CompanyStatus)> {
    let attribute = |name: &str| {
        attributes
            .get(name)
            .ok_or_else(|| anyhow!("invalid company node: {name} is missing"))
    };
    let status = match attribute("status")?.as_str() {
        ACTIVE_STATUS => CompanyStatus::Active,
        RETIRED_STATUS => CompanyStatus::Retired,
        other => bail!("unsupported company status: {other}"),
    };
    let company = validated_company(
        attribute("company_key")?.clone(),
        attribute("name")?,
        &csv_list(attribute("aliases")?),
    )?;
    Ok((company, status))
}

fn validated_company(key: String, name: &str, aliases: &[String]) -> Result<RegisteredCompany> {
    let well_formed_key = !key.is_empty()
        && key.chars().count() <= MAX_COMPANY_KEY_CHARS
        && key
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && !key.starts_with('-')
        && !key.ends_with('-');
    if !well_formed_key {
        bail!("invalid company key: expected 1 to {MAX_COMPANY_KEY_CHARS} lowercase letters, digits, or inner hyphens");
    }
    let name = validated_catalog_name(name)
        .map_err(|reason| anyhow!("invalid company \"{key}\": name {reason}"))?
        .to_string();
    let aliases = aliases
        .iter()
        .map(|alias| validated_alias(&key, alias))
        .collect::<Result<Vec<_>>>()?;
    Ok(RegisteredCompany { key, name, aliases })
}

fn validated_alias(key: &str, alias: &str) -> Result<String> {
    let valid = validated_catalog_name(alias)
        .map_err(|reason| anyhow!("invalid company \"{key}\": alias {reason}"))?;
    if valid.contains(ALIAS_SEPARATOR) {
        bail!("invalid company \"{key}\": alias contains a comma");
    }
    Ok(valid.to_string())
}

fn ensure_distinct_notations(companies: &[RegisteredCompany]) -> Result<()> {
    let mut keys = BTreeSet::new();
    let mut notations: HashMap<String, &str> = HashMap::new();
    for company in companies {
        if !keys.insert(company.key.as_str()) {
            bail!("invalid company \"{}\": key is repeated", company.key);
        }
        for written in std::iter::once(&company.name).chain(&company.aliases) {
            let normalized = name_key(written);
            if normalized.is_empty() {
                bail!(
                    "invalid company \"{}\": a name or alias has no letters or digits",
                    company.key
                );
            }
            if let Some(other) = notations.insert(normalized, &company.key) {
                bail!(
                    "invalid company \"{}\": a name or alias normalizes to the same notation as company \"{other}\"",
                    company.key
                );
            }
        }
    }
    Ok(())
}

fn company_sync_nodes(
    schema: &str,
    companies: &[RegisteredCompany],
    existing: &[NodeResult],
) -> Result<(Vec<GraphNode>, CompanySyncSummary)> {
    let listed: BTreeSet<&str> = companies
        .iter()
        .map(|company| company.key.as_str())
        .collect();
    let mut nodes: Vec<GraphNode> = companies
        .iter()
        .map(|company| company_node(schema, company, ACTIVE_STATUS))
        .collect();
    let mut retired = 0;
    for node in existing {
        let key = node.attributes.get("company_key").ok_or_else(|| {
            anyhow!(
                "invalid company node {}: company_key is missing",
                node.node_id
            )
        })?;
        if listed.contains(key.as_str()) {
            continue;
        }
        let mut attributes: Vec<(String, String)> = node
            .attributes
            .iter()
            .filter(|(name, _)| name.as_str() != "status")
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        attributes.push(("status".to_string(), RETIRED_STATUS.to_string()));
        nodes.push(GraphNode {
            id: manual_node_id(schema, KIND_COMPANY, key),
            node_type: KIND_COMPANY.to_string(),
            attributes,
        });
        retired += 1;
    }
    let summary = CompanySyncSummary {
        active: companies.len(),
        retired,
    };
    Ok((nodes, summary))
}

fn company_node(schema: &str, company: &RegisteredCompany, status: &str) -> GraphNode {
    GraphNode {
        id: manual_node_id(schema, KIND_COMPANY, &company.key),
        node_type: KIND_COMPANY.to_string(),
        attributes: vec![
            ("company_key".to_string(), company.key.clone()),
            ("name".to_string(), company.name.clone()),
            (
                "aliases".to_string(),
                company.aliases.join(&ALIAS_SEPARATOR.to_string()),
            ),
            ("status".to_string(), status.to_string()),
        ],
    }
}

#[cfg(test)]
mod tests;
