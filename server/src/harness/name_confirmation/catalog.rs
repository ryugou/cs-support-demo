use serde::{Deserialize, Serialize};

use super::matcher::name_chars;
use crate::harness::product_gate::ProductAllowlist;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NameKind {
    Product,
    Company,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NameTarget {
    pub(crate) kind: NameKind,
    pub(crate) id: String,
    pub(crate) display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogEntry {
    pub(super) key: Vec<char>,
    pub(super) target: NameTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NameCatalog {
    entries: Vec<CatalogEntry>,
}

impl NameCatalog {
    pub(crate) fn products(allowlist: &ProductAllowlist) -> Self {
        let mut catalog = Self::default();
        for (model, name) in allowlist.catalog() {
            let official = name.unwrap_or(model);
            catalog.push(NameKind::Product, model, model, model);
            catalog.push(NameKind::Product, model, official, official);
            for alias in allowlist.aliases_of(model) {
                catalog.push(NameKind::Product, model, alias, official);
            }
        }
        catalog
    }

    pub(crate) fn products_and_companies(allowlist: &ProductAllowlist) -> Self {
        let mut catalog = Self::products(allowlist);
        for company in allowlist.companies() {
            catalog.push(
                NameKind::Company,
                &company.name,
                &company.name,
                &company.name,
            );
            for alias in &company.aliases {
                catalog.push(NameKind::Company, &company.name, alias, &company.name);
            }
        }
        catalog
    }

    pub(super) fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    fn push(&mut self, kind: NameKind, id: &str, written: &str, display: &str) {
        let key = name_chars(written);
        let known = self
            .entries
            .iter()
            .any(|entry| entry.key == key && entry.target.kind == kind && entry.target.id == id);
        if key.is_empty() || known {
            return;
        }
        self.entries.push(CatalogEntry {
            key,
            target: NameTarget {
                kind,
                id: id.to_string(),
                display: display.to_string(),
            },
        });
    }
}

#[cfg(test)]
mod tests;
