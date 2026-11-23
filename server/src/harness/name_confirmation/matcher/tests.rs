use std::collections::BTreeSet;

use super::*;
use crate::harness::name_confirmation::test_fixtures::{
    catalog_of, company, f_name, f_name_and_org, f_p7, product_node,
};
use crate::harness::product_gate::ProductAllowlist;

fn target_ids(mention: &NameMention) -> BTreeSet<&str> {
    mention
        .targets
        .iter()
        .map(|target| target.id.as_str())
        .collect()
}

fn typo_mentions_of(text: &str, catalog: &NameCatalog) -> Vec<NameMention> {
    find_mentions(text, catalog)
        .into_iter()
        .filter(|mention| mention.matched == MentionMatch::Typo)
        .collect()
}

#[test]
fn single_typo_offers_only_the_name_one_edit_away() {
    let catalog = catalog_of(&f_name_and_org());
    let cases = [
        ("ADV-VC729P", "ADV-VC729P", "ADC-VC729P"),
        ("ADC-VC729", "ADC-VC729", "ADC-VC729P"),
        ("ADC-VC7729P", "ADC-VC7729P", "ADC-VC729P"),
        ("ADC-V742", "ADC-V742", "ADC-V724"),
        ("ADC-VC827", "ADC-VC827", "ADC-VC827P"),
        ("みまもりドーカムメラ", "みまもりドーカムメラ", "ADC-V724"),
        (
            "ミドリノ警備保証株式会社",
            "ミドリノ警備保証株式会社",
            "ミドリノ警備保障株式会社",
        ),
    ];
    for (written, surface, expected) in cases {
        let mentions = typo_mentions_of(written, &catalog);
        assert_eq!(mentions.len(), 1, "{written}: {mentions:?}");
        assert_eq!(mentions[0].surface, surface, "{written}");
        assert_eq!(
            target_ids(&mentions[0]),
            BTreeSet::from([expected]),
            "{written}"
        );
    }
}

#[test]
fn single_typo_shows_the_official_notation_of_the_matched_name() {
    let catalog = catalog_of(&f_name_and_org());
    let mentions = typo_mentions_of("みまもりドーカムメラ", &catalog);
    assert_eq!(mentions[0].targets[0].display, "みまもりドームカメラ");
    let mentions = typo_mentions_of("ADV-VC729P", &catalog);
    assert_eq!(mentions[0].targets[0].display, "ADC-VC729P");
}

#[test]
fn tied_typo_offers_every_name_one_edit_away() {
    let catalog = catalog_of(&f_p7());
    let mentions = typo_mentions_of("ADC-VC728Pが録画されない", &catalog);
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].surface, "ADC-VC728P");
    assert_eq!(
        target_ids(&mentions[0]),
        BTreeSet::from(["ADC-VC729P", "ADC-VC727P"])
    );
}

#[test]
fn registered_names_written_with_width_or_case_differences_match_exactly() {
    let catalog = catalog_of(&f_name_and_org());
    let cases = [
        ("ADC-V724", "ADC-V724"),
        ("adc-v724x", "ADC-V724X"),
        ("ＡＤＣ－ＶＣ７２９Ｐ", "ADC-VC729P"),
        ("ADC V724", "ADC-V724"),
        ("みまもりドームカメラ", "ADC-V724"),
        ("みまもりﾄﾞｰﾑｶﾒﾗ", "ADC-V724"),
        ("見守りドーム", "ADC-V724"),
        ("ミドリノ警備保障株式会社", "ミドリノ警備保障株式会社"),
    ];
    for (written, expected) in cases {
        let mentions = find_mentions(written, &catalog);
        assert_eq!(mentions.len(), 1, "{written}: {mentions:?}");
        assert_eq!(mentions[0].matched, MentionMatch::Exact, "{written}");
        assert_eq!(
            target_ids(&mentions[0]),
            BTreeSet::from([expected]),
            "{written}"
        );
    }
}

#[test]
fn exact_name_suppresses_a_one_edit_neighbor() {
    let catalog = catalog_of(&f_p7());
    let mentions = find_mentions("ADC-V724が録画されない", &catalog);
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].matched, MentionMatch::Exact);
    assert_eq!(target_ids(&mentions[0]), BTreeSet::from(["ADC-V724"]));
}

#[test]
fn short_fragments_and_similar_meanings_are_not_suggested() {
    let catalog = catalog_of(&f_name_and_org());
    for written in [
        "ADC-VDB101の設定を教えてください",
        "QX-9000を使っているが録画されない",
        "729のカメラが録画されない",
        "ADC",
        "VC",
        "カメラ",
        "見守り用の丸いカメラが録画されない",
        "外出用の広角カメラ",
        "月額料金はいくらですか",
    ] {
        assert_eq!(find_mentions(written, &catalog), Vec::new(), "{written}");
    }
}

#[test]
fn name_is_taken_out_of_a_sentence_with_the_customer_notation() {
    let catalog = catalog_of(&f_p7());
    let mentions = find_mentions("ADV-VC729Pをつかっているが、録画されなき", &catalog);
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].surface, "ADV-VC729P");
    let mentions = find_mentions("型番はadv-vc729pです", &catalog);
    assert_eq!(mentions[0].surface, "adv-vc729p");
}

#[test]
fn a_longer_word_is_not_cut_into_a_registered_name() {
    let catalog = catalog_of(&f_p7());
    let mentions = find_mentions("ADC-V7245を使っています", &catalog);
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].matched, MentionMatch::Typo);
    assert_eq!(mentions[0].surface, "ADC-V7245");
    assert_eq!(
        target_ids(&mentions[0]),
        BTreeSet::from(["ADC-V724", "ADC-V724X"])
    );
}

#[test]
fn names_separated_by_punctuation_are_found_one_by_one() {
    let catalog = catalog_of(&f_p7());
    let mentions = find_mentions("ADC-V724,ADC-V523", &catalog);
    let ids: Vec<BTreeSet<&str>> = mentions.iter().map(target_ids).collect();
    assert_eq!(
        ids,
        vec![BTreeSet::from(["ADC-V724"]), BTreeSet::from(["ADC-V523"])]
    );
}

#[test]
fn a_dropped_last_letter_keeps_the_particle_out_of_the_notation() {
    let catalog = catalog_of(&f_name());
    let mentions = typo_mentions_of("みまもりドームカメが録画されない", &catalog);
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].surface, "みまもりドームカメ");
    let mentions = typo_mentions_of("ADC-VC729の設定", &catalog);
    assert_eq!(mentions[0].surface, "ADC-VC729");
}

#[test]
fn matching_depends_only_on_the_registered_names() {
    let allowlist = ProductAllowlist::from_nodes(vec![
        product_node("ZX-1000", "ZX-1000", &[]),
        product_node("ZX-1000W", "ZX-1000W", &[]),
        product_node("KT-1", "キッチンタイマー丸型", &[]),
    ])
    .with_companies(vec![company("midorino", "ミドリノ警備保障株式会社", &[])]);
    let catalog = catalog_of(&allowlist);
    let cases = [
        ("ZY-1000", "ZX-1000"),
        ("キッチンタイマー丸形", "KT-1"),
        ("ミドリノ警備保証株式会社", "ミドリノ警備保障株式会社"),
    ];
    for (written, expected) in cases {
        let mentions = typo_mentions_of(written, &catalog);
        assert_eq!(mentions.len(), 1, "{written}: {mentions:?}");
        assert_eq!(
            target_ids(&mentions[0]),
            BTreeSet::from([expected]),
            "{written}"
        );
    }
    assert_eq!(typo_mentions_of("ADV-VC729P", &catalog), Vec::new());
}

#[test]
fn an_empty_catalog_suggests_nothing() {
    let catalog = NameCatalog::default();
    assert_eq!(
        find_mentions("ADV-VC729Pが録画されない", &catalog),
        Vec::new()
    );
}

#[test]
fn a_name_removed_from_the_catalog_is_no_longer_suggested() {
    let allowlist = ProductAllowlist::from_nodes(
        crate::harness::name_confirmation::test_fixtures::F_P7
            .iter()
            .filter(|model| **model != "ADC-VC729P")
            .map(|model| product_node(model, model, &[]))
            .collect(),
    );
    let mentions = find_mentions("ADV-VC729Pが録画されない", &catalog_of(&allowlist));
    assert!(mentions
        .iter()
        .all(|mention| !target_ids(mention).contains("ADC-VC729P")));
}

#[test]
fn name_key_ignores_width_case_and_symbols() {
    assert_eq!(name_key("ＡＤＣ－ＶＣ７２９Ｐ"), "adcvc729p");
    assert_eq!(name_key("ADC VC-729P"), "adcvc729p");
    assert_eq!(name_key("ﾄﾞｰﾑ"), "ドーム");
}

#[test]
fn single_typo_counts_each_kind_of_edit_once() {
    let name: Vec<char> = "abcdef".chars().collect();
    let cases = [
        ("abcdef", false),
        ("abxdef", true),
        ("abdef", true),
        ("abcxdef", true),
        ("abdcef", true),
        ("bacdef", true),
        ("abcdfe", true),
        ("abxdex", false),
        ("bcdefa", false),
        ("abcd", false),
    ];
    for (written, expected) in cases {
        let written: Vec<char> = written.chars().collect();
        assert_eq!(is_single_typo(&written, &name), expected, "{written:?}");
    }
}
