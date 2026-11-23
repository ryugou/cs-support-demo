use std::collections::HashMap;

use super::catalog::{NameCatalog, NameKind, NameTarget};
use super::company::RegisteredCompany;
use crate::harness::product_gate::ProductAllowlist;
use crate::proto::graphrag::NodeResult;

pub(crate) const F_P7: [&str; 7] = [
    "ADC-V523",
    "ADC-V523X",
    "ADC-V724",
    "ADC-V724X",
    "ADC-VC729P",
    "ADC-VC727P",
    "ADC-VC827P",
];

pub(crate) fn product_node(model: &str, name: &str, aliases: &[&str]) -> NodeResult {
    let attributes: HashMap<String, String> = [
        ("product_key", model.to_string()),
        ("model", model.to_string()),
        ("name", name.to_string()),
        ("aliases", aliases.join(",")),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    NodeResult {
        node_id: format!("urtect:gen1:Product:{model}"),
        node_type: "Product".to_string(),
        attributes,
    }
}

pub(crate) fn f_p7() -> ProductAllowlist {
    ProductAllowlist::from_nodes(
        F_P7.iter()
            .map(|model| product_node(model, model, &[]))
            .collect(),
    )
}

pub(crate) fn f_name() -> ProductAllowlist {
    ProductAllowlist::from_nodes(
        F_P7.iter()
            .map(|model| match *model {
                "ADC-V724" => product_node(model, "みまもりドームカメラ", &["見守りドーム"]),
                "ADC-VC729P" => product_node(model, "おでかけパノラマカメラ", &[]),
                other => product_node(other, other, &[]),
            })
            .collect(),
    )
}

pub(crate) fn company(key: &str, name: &str, aliases: &[&str]) -> RegisteredCompany {
    RegisteredCompany {
        key: key.to_string(),
        name: name.to_string(),
        aliases: aliases.iter().map(ToString::to_string).collect(),
    }
}

pub(crate) fn f_org() -> Vec<RegisteredCompany> {
    vec![
        company("midorino", "ミドリノ警備保障株式会社", &[]),
        company("higashi", "ヒガシ電設工業株式会社", &[]),
    ]
}

pub(crate) fn f_name_and_org() -> ProductAllowlist {
    f_name().with_companies(f_org())
}

pub(crate) fn catalog_of(allowlist: &ProductAllowlist) -> NameCatalog {
    NameCatalog::products_and_companies(allowlist)
}

pub(crate) fn product(model: &str) -> NameTarget {
    NameTarget {
        kind: NameKind::Product,
        id: model.to_string(),
        display: model.to_string(),
    }
}

pub(crate) fn company_target(name: &str) -> NameTarget {
    NameTarget {
        kind: NameKind::Company,
        id: name.to_string(),
        display: name.to_string(),
    }
}
