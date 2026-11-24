use std::collections::BTreeSet;

use super::catalog::NameCatalog;
use super::matcher::{find_mentions, MentionMatch};
use crate::harness::product_gate::{
    ProductAllowlist, ProductReference, ProductReferenceResolution,
};

pub(crate) fn screen_product_references(
    allowlist: &ProductAllowlist,
    text: &str,
    references: Vec<ProductReference>,
) -> Vec<ProductReference> {
    let mentions = find_mentions(text, &NameCatalog::products(allowlist));
    let written: Vec<(&str, &str)> = mentions
        .iter()
        .filter(|mention| mention.matched == MentionMatch::Exact)
        .filter_map(|mention| {
            mention
                .single_target()
                .map(|target| (mention.surface.as_str(), target.id.as_str()))
        })
        .collect();
    let written_models: BTreeSet<&str> = written.iter().map(|(_, model)| *model).collect();
    let typo_models: BTreeSet<&str> = mentions
        .iter()
        .filter(|mention| mention.matched == MentionMatch::Typo)
        .flat_map(|mention| mention.targets.iter().map(|target| target.id.as_str()))
        .filter(|model| !written_models.contains(model))
        .collect();
    let mut screened: Vec<ProductReference> = references
        .into_iter()
        .map(|reference| hold_back_typo_match(reference, allowlist, &typo_models))
        .collect();
    for (surface, model) in written {
        if !screened
            .iter()
            .any(|reference| matched_model(reference, allowlist).as_deref() == Some(model))
        {
            screened.push(ProductReference {
                surface: surface.to_string(),
                resolution: ProductReferenceResolution::Matched,
                matched_model: Some(model.to_string()),
            });
        }
    }
    screened
}

fn hold_back_typo_match(
    reference: ProductReference,
    allowlist: &ProductAllowlist,
    typo_models: &BTreeSet<&str>,
) -> ProductReference {
    match matched_model(&reference, allowlist) {
        Some(model) if typo_models.contains(model.as_str()) => ProductReference {
            resolution: ProductReferenceResolution::Ambiguous,
            ..reference
        },
        _ => reference,
    }
}

fn matched_model(reference: &ProductReference, allowlist: &ProductAllowlist) -> Option<String> {
    if reference.resolution != ProductReferenceResolution::Matched {
        return None;
    }
    reference
        .matched_model
        .as_deref()
        .and_then(|model| allowlist.canonical_model(model))
}

#[cfg(test)]
mod tests;
