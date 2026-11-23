use super::*;
use crate::harness::name_confirmation::catalog::NameCatalog;
use crate::harness::name_confirmation::matcher::{find_mentions, MentionMatch};
use crate::harness::name_confirmation::test_fixtures::{company, f_p7};

const F_ORG_FILE: &str = r#"[
  {"key": "midorino", "name": "ミドリノ警備保障株式会社", "aliases": []},
  {"key": "higashi", "name": "ヒガシ電設工業株式会社", "aliases": ["ヒガシ電設"]}
]"#;

fn stored(nodes: &[GraphNode]) -> Vec<NodeResult> {
    nodes
        .iter()
        .map(|node| NodeResult {
            node_id: node.id.clone(),
            node_type: node.node_type.clone(),
            attributes: node.attributes.iter().cloned().collect(),
        })
        .collect()
}

fn register(file: &str, existing: &[NodeResult]) -> (Vec<NodeResult>, CompanySyncSummary) {
    let companies = parse_company_file(file).unwrap();
    let (nodes, summary) = company_sync_nodes("urtect", &companies, existing).unwrap();
    let mut written = existing.to_vec();
    for node in stored(&nodes) {
        written.retain(|existing| existing.node_id != node.node_id);
        written.push(node);
    }
    (written, summary)
}

fn suggested_companies(stored_nodes: &[NodeResult], written: &str) -> Vec<String> {
    let allowlist = f_p7().with_companies(active_companies(stored_nodes.to_vec()));
    find_mentions(written, &NameCatalog::products_and_companies(&allowlist))
        .into_iter()
        .filter(|mention| mention.matched == MentionMatch::Typo)
        .flat_map(|mention| mention.targets.into_iter().map(|target| target.id))
        .collect()
}

#[test]
fn a_valid_company_file_is_read_with_its_aliases() {
    assert_eq!(
        parse_company_file(F_ORG_FILE).unwrap(),
        vec![
            company("midorino", "ミドリノ警備保障株式会社", &[]),
            company("higashi", "ヒガシ電設工業株式会社", &["ヒガシ電設"]),
        ]
    );
    assert_eq!(parse_company_file("[]").unwrap(), Vec::new());
}

#[test]
fn an_invalid_company_file_is_rejected() {
    let long_key = "a".repeat(MAX_COMPANY_KEY_CHARS + 1);
    let cases = vec![
        r#"{"key": "a", "name": "A", "aliases": []}"#.to_string(),
        r#"[{"key": "a", "name": "A"}]"#.to_string(),
        r#"[{"key": "a", "name": "A", "aliases": [], "note": "x"}]"#.to_string(),
        r#"[{"key": "Upper", "name": "A", "aliases": []}]"#.to_string(),
        r#"[{"key": "-a", "name": "A", "aliases": []}]"#.to_string(),
        r#"[{"key": "", "name": "A", "aliases": []}]"#.to_string(),
        format!(r#"[{{"key": "{long_key}", "name": "A", "aliases": []}}]"#),
        r#"[{"key": "a", "name": "  ", "aliases": []}]"#.to_string(),
        r#"[{"key": "a", "name": "A\u0007B", "aliases": []}]"#.to_string(),
        r#"[{"key": "a", "name": "A", "aliases": ["B,C"]}]"#.to_string(),
        r#"[{"key": "a", "name": "---", "aliases": []}]"#.to_string(),
        r#"[{"key": "a", "name": "A", "aliases": []}, {"key": "a", "name": "B", "aliases": []}]"#
            .to_string(),
        r#"[{"key": "a", "name": "ＡＢ社", "aliases": []}, {"key": "b", "name": "ab社", "aliases": []}]"#
            .to_string(),
        r#"[{"key": "a", "name": "AB社", "aliases": ["ab 社"]}]"#.to_string(),
    ];
    for file in cases {
        assert!(parse_company_file(&file).is_err(), "{file}");
    }
}

#[test]
fn synchronization_writes_listed_companies_and_retires_the_rest() {
    let (first, summary) = register(F_ORG_FILE, &[]);
    assert_eq!(
        summary,
        CompanySyncSummary {
            active: 2,
            retired: 0
        }
    );
    let renamed = r#"[{"key": "midorino", "name": "ミドリノ警備株式会社", "aliases": []}]"#;
    let (second, summary) = register(renamed, &first);
    assert_eq!(
        summary,
        CompanySyncSummary {
            active: 1,
            retired: 1
        }
    );
    assert_eq!(
        active_companies(second),
        vec![company("midorino", "ミドリノ警備株式会社", &[])]
    );
}

#[test]
fn registered_companies_become_suggestions_and_updates_change_them() {
    let (registered, _) = register(F_ORG_FILE, &[]);
    assert_eq!(
        suggested_companies(&registered, "ミドリノ警備保証株式会社から購入した"),
        vec!["ミドリノ警備保障株式会社"]
    );
    let updated = r#"[{"key": "midorino", "name": "ミドリノ防犯株式会社", "aliases": []}]"#;
    let (updated, _) = register(updated, &registered);
    assert_eq!(
        suggested_companies(&updated, "ミドリノ警備保証株式会社から購入した"),
        Vec::<String>::new()
    );
    assert_eq!(
        suggested_companies(&updated, "ミドリノ防犯株式会杜から購入した"),
        vec!["ミドリノ防犯株式会社"]
    );
    assert_eq!(
        suggested_companies(&updated, "ヒガシ電設工業株式会杜"),
        Vec::<String>::new()
    );
}

#[test]
fn invalid_or_retired_company_nodes_are_left_out_of_the_catalog() {
    let node = |attributes: &[(&str, &str)]| NodeResult {
        node_id: "urtect:gen1:Company:x".to_string(),
        node_type: KIND_COMPANY.to_string(),
        attributes: attributes
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    };
    let nodes = vec![
        node(&[
            ("company_key", "a"),
            ("name", "A社"),
            ("aliases", ""),
            ("status", "active"),
        ]),
        node(&[
            ("company_key", "b"),
            ("name", "B社"),
            ("aliases", ""),
            ("status", "retired"),
        ]),
        node(&[("company_key", "c"), ("name", "C社"), ("aliases", "")]),
        node(&[
            ("company_key", "d"),
            ("name", "D社"),
            ("aliases", ""),
            ("status", "paused"),
        ]),
        node(&[
            ("company_key", "E"),
            ("name", "E社"),
            ("aliases", ""),
            ("status", "active"),
        ]),
        node(&[
            ("company_key", "f"),
            ("name", " "),
            ("aliases", ""),
            ("status", "active"),
        ]),
    ];
    assert_eq!(active_companies(nodes), vec![company("a", "A社", &[])]);
}

#[test]
fn a_registered_node_without_a_key_stops_the_synchronization() {
    let broken = NodeResult {
        node_id: "urtect:gen1:Company:x".to_string(),
        node_type: KIND_COMPANY.to_string(),
        attributes: HashMap::from([("name".to_string(), "X社".to_string())]),
    };
    let companies = parse_company_file(F_ORG_FILE).unwrap();
    assert!(company_sync_nodes("urtect", &companies, &[broken]).is_err());
}
