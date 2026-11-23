use super::*;
use crate::harness::clarify::{build_clarify_prompt, build_product_identification_prompt};
use crate::harness::name_confirmation::flow::test_support::{assistant, customer, names_with};
use crate::harness::name_confirmation::state::ConfirmationState;
use crate::harness::name_confirmation::test_fixtures::{company_target, f_name, f_p7, product};

#[test]
fn confirmed_names_join_the_known_facts() {
    let mut state = ConfirmationState::default();
    state.confirm("a", product("ADC-VC729P"));
    state.confirm("b", company_target("ミドリノ警備保障株式会社"));
    let names = names_with(&f_p7(), state);
    assert_eq!(
        names.with_name_facts("- 顧客発話: 録画されない".to_string(), None, &[], &f_p7()),
        "- 顧客発話: 録画されない\n- 確認済みの製品: ADC-VC729P\n- 確認済みの社名: ミドリノ警備保障株式会社"
    );
    assert_eq!(
        names_with(&f_p7(), ConfirmationState::default()).with_name_facts(
            String::new(),
            None,
            &[],
            &f_p7()
        ),
        ""
    );

    let mut by_name = ConfirmationState::default();
    by_name.confirm(
        "c",
        NameTarget {
            kind: NameKind::Product,
            id: "ADC-V724".to_string(),
            display: "みまもりドームカメラ".to_string(),
        },
    );
    assert_eq!(
        names_with(&f_p7(), by_name).with_name_facts(String::new(), None, &[], &f_name()),
        "- 確認済みの製品: みまもりドームカメラ（型番 ADC-V724）"
    );
}

#[test]
fn a_denial_tells_the_hearing_the_notation_and_the_names_to_leave_out() {
    let mut state = ConfirmationState::default();
    state.deny(&[product("ADC-VC729P")]);
    let names = names_with(&f_p7(), state);
    let denied = ReplyResolution {
        confirmed: Vec::new(),
        unresolved_surface: Some("ADV-VC729P".to_string()),
    };
    assert_eq!(
        names.with_name_facts("- 顧客発話: 録画されない".to_string(), Some(&denied), &[], &f_p7()),
        "- 顧客発話: 録画されない\n\
         - 顧客がお知らせくださった名称（ADV-VC729P）は当社の登録名称と一致せず、確認した候補とも違うとのことです\n\
         - 顧客が違うと答えた候補（返信に書かない）: ADC-VC729P"
    );
    assert_eq!(
        names.with_name_facts(String::new(), None, &[], &f_p7()),
        "- 顧客が違うと答えた候補（返信に書かない）: ADC-VC729P"
    );
}

#[test]
fn the_hearing_catalog_leaves_out_denied_products() {
    let allowlist = f_p7();
    let mut state = ConfirmationState::default();
    state.deny(&[product("ADC-VC729P")]);
    let names = names_with(&allowlist, state);
    let hearing = names.hearing_allowlist(&allowlist);
    assert!(hearing.canonical_model("ADC-VC729P").is_none());
    assert!(hearing.canonical_model("ADC-VC727P").is_some());
    assert_eq!(
        names.withhold_denied_names(
            "ADC-VC729P ですか？".to_string(),
            "fallback",
            "req-test",
            "case-test"
        ),
        "fallback"
    );
    assert_eq!(
        names.withhold_denied_names(
            "型番を教えてください".to_string(),
            "fallback",
            "req-test",
            "case-test"
        ),
        "型番を教えてください"
    );
    let untouched = names_with(&allowlist, ConfirmationState::default());
    assert!(matches!(
        untouched.hearing_allowlist(&allowlist),
        Cow::Borrowed(_)
    ));
}

#[test]
fn the_hearing_after_a_denial_reads_the_notation_the_customer_wrote() {
    let history = [
        customer("ADV-VC729Pを使っているが録画されない"),
        assistant("ADV-VC729P とお知らせいただきましたが、ADC-VC729P のことでしょうか？"),
    ];
    let denied = ReplyResolution {
        confirmed: Vec::new(),
        unresolved_surface: Some("ADV-VC729P".to_string()),
    };
    assert_eq!(
        NameConfirmation::hearing_question("いいえ、違います", &history, Some(&denied)),
        "ADV-VC729Pを使っているが録画されない\nいいえ、違います"
    );
    assert_eq!(
        NameConfirmation::hearing_question("いいえ、違います", &history, None),
        "いいえ、違います"
    );
    assert_eq!(
        NameConfirmation::hearing_question("いいえ、違います", &[], Some(&denied)),
        "いいえ、違います"
    );
}

#[test]
fn an_identified_product_with_a_name_tells_the_hearing_its_model() {
    let names = names_with(&f_name(), ConfirmationState::default());
    let identified = vec!["ADC-V724".to_string(), "ADC-V523".to_string()];
    assert_eq!(
        names.with_name_facts(String::new(), None, &identified, &f_name()),
        "- 特定済みの製品: みまもりドームカメラ（型番 ADC-V724）"
    );
    assert_eq!(
        names.with_name_facts(String::new(), None, &identified, &f_p7()),
        ""
    );

    let mut confirmed = ConfirmationState::default();
    confirmed.confirm(
        "c",
        NameTarget {
            kind: NameKind::Product,
            id: "ADC-V724".to_string(),
            display: "みまもりドームカメラ".to_string(),
        },
    );
    assert_eq!(
        names_with(&f_name(), confirmed).with_name_facts(
            String::new(),
            None,
            &identified,
            &f_name()
        ),
        "- 確認済みの製品: みまもりドームカメラ（型番 ADC-V724）"
    );
}

#[test]
fn the_identified_products_are_read_from_the_case_attributes() {
    let attributes = HashMap::from([(
        "product_models".to_string(),
        "ADC-V523,ADC-V724".to_string(),
    )]);
    assert_eq!(
        identified_product_models(&attributes),
        vec!["ADC-V523".to_string(), "ADC-V724".to_string()]
    );
    assert!(identified_product_models(&HashMap::new()).is_empty());
}

#[test]
fn the_hearing_after_a_denial_lists_no_product_and_no_similar_one() {
    let allowlist = f_p7();
    let mut state = ConfirmationState::default();
    state.deny(&[product("ADC-VC729P")]);
    let denied = names_with(&allowlist, state);
    let untouched = names_with(&allowlist, ConfirmationState::default());
    for (names, withheld) in [(&denied, true), (&untouched, false)] {
        let hearing = names.hearing_allowlist(&allowlist);
        let clarify = build_clarify_prompt("録画されない", "型番", "", false, &hearing);
        let identification =
            build_product_identification_prompt("録画されない", "", false, &hearing);
        for (system, user) in [&clarify, &identification] {
            for rule in [
                "この返信では当社の取扱製品を一覧にして挙げない",
                "「近い」「似ている」という言葉を使わない",
                "言い換えずにそのまま書いて受け止める",
                "本体のラベルや購入時の書類で型番を確認してもらう",
            ] {
                assert_eq!(system.contains(rule), withheld, "{system}");
            }
            let prompt = format!("{system}{user}");
            assert_eq!(prompt.contains("ADC-VC827P"), !withheld, "{prompt}");
            assert!(!withheld || !prompt.contains("ADC-VC729P"), "{prompt}");
        }
    }
}
