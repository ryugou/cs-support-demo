use super::*;
use crate::harness::name_confirmation::test_fixtures::{
    company, f_name_and_org, f_p7, product_node,
};

fn entries_of(catalog: &NameCatalog) -> Vec<(String, NameKind, String, String)> {
    catalog
        .entries()
        .iter()
        .map(|entry| {
            (
                entry.key.iter().collect(),
                entry.target.kind,
                entry.target.id.clone(),
                entry.target.display.clone(),
            )
        })
        .collect()
}

#[test]
fn a_name_equal_to_the_model_is_registered_once() {
    let catalog = NameCatalog::products(&f_p7());
    assert_eq!(catalog.entries().len(), 7);
    assert!(entries_of(&catalog).contains(&(
        "adcvc729p".to_string(),
        NameKind::Product,
        "ADC-VC729P".to_string(),
        "ADC-VC729P".to_string()
    )));
}

#[test]
fn product_names_and_aliases_point_at_the_model_and_show_the_official_name() {
    let catalog = NameCatalog::products(&f_name_and_org());
    let entries = entries_of(&catalog);
    assert!(entries.contains(&(
        "みまもりドームカメラ".to_string(),
        NameKind::Product,
        "ADC-V724".to_string(),
        "みまもりドームカメラ".to_string()
    )));
    assert!(entries.contains(&(
        "見守りドーム".to_string(),
        NameKind::Product,
        "ADC-V724".to_string(),
        "みまもりドームカメラ".to_string()
    )));
    assert!(entries
        .iter()
        .all(|(_, kind, _, _)| *kind == NameKind::Product));
}

#[test]
fn companies_join_the_catalog_with_their_aliases() {
    let allowlist = crate::harness::product_gate::ProductAllowlist::from_nodes(vec![product_node(
        "ZX-1000",
        "ZX-1000",
        &[],
    )])
    .with_companies(vec![company(
        "midorino",
        "ミドリノ警備保障株式会社",
        &["ミドリノ警備"],
    )]);
    let entries = entries_of(&NameCatalog::products_and_companies(&allowlist));
    assert!(entries.contains(&(
        "ミドリノ警備".to_string(),
        NameKind::Company,
        "ミドリノ警備保障株式会社".to_string(),
        "ミドリノ警備保障株式会社".to_string()
    )));
    assert_eq!(entries.len(), 3);
}

#[test]
fn product_only_catalog_leaves_companies_out() {
    let catalog = NameCatalog::products(&f_name_and_org());
    assert!(catalog
        .entries()
        .iter()
        .all(|entry| entry.target.kind == NameKind::Product));
}
