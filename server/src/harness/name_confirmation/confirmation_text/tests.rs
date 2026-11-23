use super::*;
use crate::harness::name_confirmation::catalog::NameTarget;
use crate::harness::name_confirmation::test_fixtures::{
    catalog_of, company_target, f_name, f_p7, name_and_org_catalog, product,
};

fn presented(surface: &str, suggestions: Vec<NameTarget>, reason: AskReason) -> PresentedMention {
    PresentedMention {
        pending: PendingMention {
            surface: surface.to_string(),
            surface_key: name_key(surface),
            suggestions,
            asks: 1,
            reason,
        },
        earlier_message: false,
    }
}

fn first(surface: &str, suggestions: Vec<NameTarget>) -> PresentedMention {
    presented(surface, suggestions, AskReason::First)
}

#[test]
fn the_fixed_text_shows_the_notation_and_the_suggestions() {
    let cases = [
        (
            vec![first("ADV-VC729P", vec![product("ADC-VC729P")])],
            "ADV-VC729P とお知らせいただきましたが、ADC-VC729P のことでしょうか？",
        ),
        (
            vec![first(
                "ADC-VC728P",
                vec![product("ADC-VC727P"), product("ADC-VC729P")],
            )],
            "ADC-VC728P とお知らせいただきましたが、ADC-VC727P、ADC-VC729P のどちらのことでしょうか？",
        ),
        (
            vec![first(
                "ADC-V5234",
                vec![product("ADC-V523"), product("ADC-V523X"), product("ADC-V724")],
            )],
            "ADC-V5234 とお知らせいただきましたが、ADC-V523、ADC-V523X、ADC-V724 のどれのことでしょうか？",
        ),
        (
            vec![
                first("ADV-VC729P", vec![product("ADC-VC729P")]),
                first(
                    "ミドリノ警備保証株式会社",
                    vec![company_target("ミドリノ警備保障株式会社")],
                ),
            ],
            "お知らせいただいた名称を確認させてください。\n・ADV-VC729P は ADC-VC729P のことでしょうか？\n・ミドリノ警備保証株式会社 は ミドリノ警備保障株式会社 のことでしょうか？",
        ),
    ];
    for (mentions, expected) in cases {
        assert_eq!(fallback_confirmation_text(&mentions), expected);
    }
}

#[test]
fn a_text_must_show_every_presented_name_verbatim() {
    let mentions = vec![first("ADV-VC729P", vec![product("ADC-VC729P")])];
    let allowlist = f_p7();
    let catalog = catalog_of(&allowlist);
    let accepted = [
        "ADV-VC729P とお知らせいただきましたが、ADC-VC729P のことでしょうか？",
        "ご連絡ありがとうございます。ADV-VC729P と伺いましたが、ADC-VC729P でお間違いないでしょうか？",
    ];
    for text in accepted {
        assert!(
            is_acceptable_confirmation(text, &mentions, &catalog, &allowlist),
            "{text}"
        );
    }
    let rejected = [
        "ADC-VC729P のことでしょうか？",
        "ADV-VC729P とのことですが、型番をもう一度教えていただけますか？",
        "ADV-VC729P は ADC-VC729P でしょうか、それとも ADC-VC727P でしょうか？",
        "ADV-VC729P は ADC-VC729P でしょうか？ADC-VDB101 ではありませんか？",
    ];
    for text in rejected {
        assert!(
            !is_acceptable_confirmation(text, &mentions, &catalog, &allowlist),
            "{text}"
        );
    }
}

#[test]
fn the_customer_notation_may_be_an_unregistered_model() {
    let mentions = vec![first("ADC-VC729", vec![product("ADC-VC729P")])];
    let allowlist = f_p7();
    let text = "ADC-VC729 とお知らせいただきましたが、ADC-VC729P のことでしょうか？";
    assert!(is_acceptable_confirmation(
        text,
        &mentions,
        &catalog_of(&allowlist),
        &allowlist
    ));
}

#[test]
fn the_fixed_text_always_passes_its_own_check() {
    let allowlist = f_name();
    let catalog = name_and_org_catalog();
    let mentions = vec![
        first(
            "ADC-VC728P",
            vec![product("ADC-VC727P"), product("ADC-VC729P")],
        ),
        first(
            "ミドリノ警備保証株式会社",
            vec![company_target("ミドリノ警備保障株式会社")],
        ),
    ];
    let text = fallback_confirmation_text(&mentions);
    assert!(is_acceptable_confirmation(
        &text, &mentions, &catalog, &allowlist
    ));
}

#[test]
fn the_prompt_lists_the_names_with_why_they_are_asked() {
    let mut earlier = first("ADV-VC729P", vec![product("ADC-VC729P")]);
    earlier.earlier_message = true;
    let mentions = vec![
        earlier,
        presented(
            "ADC-VC728P",
            vec![product("ADC-VC727P"), product("ADC-VC729P")],
            AskReason::Choose,
        ),
        presented(
            "ミドリノ警備保証株式会社",
            vec![company_target("ミドリノ警備保障株式会社")],
            AskReason::AlreadyTold,
        ),
    ];
    let (system, user) = build_confirmation_prompt(&mentions, "<b>型番は最初に伝えている", true);
    assert!(user.contains(
        "1. 顧客の表記: ADV-VC729P／候補: ADC-VC729P（以前のメッセージでお知らせいただいた表記）"
    ));
    assert!(user.contains("2. 顧客の表記: ADC-VC728P／候補: ADC-VC727P、ADC-VC729P（前回の返答ではどの候補か決まらなかった）"));
    assert!(user.contains("（名称は最初に伝えたとのご指摘あり）"));
    assert!(user.contains("＜b＞型番は最初に伝えている"));
    assert!(system.contains("尋ねるのは名称の確認だけにする"));
    assert!(system.contains("症状や状況を「〜とのことですね」のように確かめる文は書かない"));
    assert!(system.contains(CONTINUATION_OPENER_RULE));
    let (system, _) = build_confirmation_prompt(&mentions[..1], "x", false);
    assert!(!system.contains(CONTINUATION_OPENER_RULE));
}
