use super::*;
use crate::harness::name_confirmation::flow::test_support::{
    assistant, customer, names_in, names_with, pending, presented_keys, unidentified_outcome,
};
use crate::harness::name_confirmation::state::{AskReason, ConfirmationState};
use crate::harness::name_confirmation::test_fixtures::{f_p7, name_and_org_catalog, product};

fn surfaces(presentation: &Presentation) -> Vec<(&str, bool)> {
    presentation
        .mentions
        .iter()
        .map(|mention| (mention.pending.surface.as_str(), mention.earlier_message))
        .collect()
}

#[test]
fn a_typo_in_the_message_is_presented_with_its_suggestion() {
    let names = names_with(&f_p7(), ConfirmationState::default());
    let presentation = names
        .plan(
            &unidentified_outcome(),
            "ADV-VC729Pを使っているが録画されない",
            &[],
        )
        .unwrap();
    assert_eq!(surfaces(&presentation), vec![("ADV-VC729P", false)]);
    assert_eq!(
        presentation.mentions[0].pending.suggestions,
        vec![product("ADC-VC729P")]
    );
    assert_eq!(presentation.mentions[0].pending.asks, 0);
}

#[test]
fn a_typo_written_only_in_an_earlier_message_is_presented() {
    let names = names_with(&f_p7(), ConfirmationState::default());
    let history = [
        customer("ADV-VC729Pをつかっているが、録画されなき"),
        assistant("型番を教えてください。ADV-VC729P は確認できませんでした。"),
    ];
    let presentation = names
        .plan(&unidentified_outcome(), "型番は最初に伝えている", &history)
        .unwrap();
    assert_eq!(surfaces(&presentation), vec![("ADV-VC729P", true)]);
}

#[test]
fn nothing_is_presented_for_an_immediate_handoff_a_fallback_or_a_disabled_turn() {
    let message = "ADV-VC729Pから焦げ臭いにおいがして熱い";
    let mut handoff = unidentified_outcome();
    handoff.immediate_handoff = true;
    let mut fallback = unidentified_outcome();
    fallback.extraction_mode = ExtractionMode::LexiconFallback;
    let names = names_with(&f_p7(), ConfirmationState::default());
    assert_eq!(names.plan(&handoff, message, &[]), None);
    assert_eq!(names.plan(&fallback, message, &[]), None);
    let disabled = NameConfirmation {
        enabled: false,
        ..names_with(&f_p7(), ConfirmationState::default())
    };
    assert_eq!(disabled.plan(&unidentified_outcome(), message, &[]), None);
}

#[test]
fn settled_notations_and_denied_suggestions_are_not_presented_again() {
    let outcome = unidentified_outcome();
    let mut confirmed = ConfirmationState::default();
    confirmed.confirm(&name_key("ADV-VC729P"), product("ADC-VC729P"));
    let mut dismissed = ConfirmationState::default();
    dismissed.dismiss(&name_key("ADV-VC729P"));
    let mut denied = ConfirmationState::default();
    denied.deny(&[product("ADC-VC729P")]);
    for state in [confirmed, dismissed, denied] {
        let names = names_with(&f_p7(), state);
        assert_eq!(names.plan(&outcome, "ADV-VC729Pです", &[]), None);
    }
}

#[test]
fn a_suggestion_the_message_already_names_is_not_asked() {
    let names = names_with(&f_p7(), ConfirmationState::default());
    let plan = names.plan(
        &unidentified_outcome(),
        "ADV-VC729P、いえ ADC-VC729P です",
        &[],
    );
    assert_eq!(plan, None);
}

#[test]
fn pending_names_come_first_and_the_list_is_capped() {
    let mut state = ConfirmationState::default();
    state.push_pending(pending(
        "ADC-VC728P",
        vec![product("ADC-VC727P"), product("ADC-VC729P")],
        AskReason::Choose,
    ));
    let names = names_in(name_and_org_catalog(), state);
    let presentation = names
        .plan(
            &unidentified_outcome(),
            "ミドリノ警備保証株式会社で買ったADC-V742とADV-VC729P",
            &[],
        )
        .unwrap();
    assert_eq!(
        surfaces(&presentation),
        vec![
            ("ADC-VC728P", false),
            ("ミドリノ警備保証株式会社", false),
            ("ADC-V742", false)
        ]
    );
}

#[test]
fn the_out_of_scope_reply_names_the_first_unknown_model_without_a_suggestion() {
    let allowlist = f_p7();
    let names = names_with(&allowlist, ConfirmationState::default());
    let reply_model = |names: &NameConfirmation, message: &str| {
        names.out_of_scope_reply_model(&allowlist, message, &[], None)
    };
    assert_eq!(reply_model(&names, "ADC-VC729の設定を教えてください"), None);
    assert_eq!(reply_model(&names, "ADC-VC728Pが録画されない"), None);
    assert_eq!(reply_model(&names, "ADC-V724の設定"), None);
    assert_eq!(
        reply_model(&names, "ADC-VDB101の設定を教えてください").as_deref(),
        Some("ADC-VDB101")
    );
    assert_eq!(
        reply_model(&names, "ADC-VC729とADC-VDB101").as_deref(),
        Some("ADC-VDB101")
    );
    assert_eq!(
        reply_model(&names, "ADC-VC729PとADC-VC729の違いを教えてください").as_deref(),
        Some("ADC-VC729")
    );

    let mut denied = ConfirmationState::default();
    denied.deny(&[product("ADC-VC729P")]);
    let names = names_with(&allowlist, denied);
    assert_eq!(
        reply_model(&names, "いいえ、ADC-VC729です").as_deref(),
        Some("ADC-VC729")
    );

    let disabled = NameConfirmation {
        enabled: false,
        ..names_with(&allowlist, ConfirmationState::default())
    };
    assert_eq!(
        reply_model(&disabled, "ADC-VC729の設定を教えてください").as_deref(),
        Some("ADC-VC729")
    );
}

#[test]
fn an_unknown_model_is_deferred_only_when_the_same_turn_presents_it() {
    let allowlist = f_p7();
    let names = names_with(&allowlist, ConfirmationState::default());
    for message in [
        "ADC-VC729の設定を教えてください",
        "ADC-VC728Pが録画されない",
        "ADC-VC729PとADC-VC729の違いを教えてください",
        "ADC-VC729とADC-VDB101",
        "ADC-VC729、ADC-VC827、ADC-V742、ADC-V5233 の違い",
    ] {
        let presented = presented_keys(&names, message);
        let deferred = names
            .out_of_scope_reply_model(&allowlist, message, &[], None)
            .is_none();
        let every_unknown_model_presented = allowlist
            .out_of_scope_mentions(message)
            .iter()
            .all(|model| presented.contains(&name_key(model)));
        assert_eq!(deferred, every_unknown_model_presented, "{message}");
    }
}

#[test]
fn an_unknown_model_the_customer_confirmed_is_not_out_of_scope() {
    let allowlist = f_p7();
    let mut state = ConfirmationState::default();
    state.confirm(&name_key("ADC-VC729"), product("ADC-VC729P"));
    let names = names_with(&allowlist, state);
    let confirmed = ReplyResolution {
        confirmed: vec![("ADC-VC729".to_string(), product("ADC-VC729P"))],
        unresolved_surface: None,
    };
    assert_eq!(
        names.out_of_scope_reply_model(
            &allowlist,
            "はい、ADC-VC729のことです",
            &[],
            Some(&confirmed)
        ),
        None
    );
    assert_eq!(
        names.out_of_scope_reply_model(&allowlist, "ADC-VC729の夜間モードは？", &[], None),
        None
    );
}

#[test]
fn an_unresolved_unknown_model_after_the_answer_gets_the_out_of_scope_reply() {
    let allowlist = f_p7();
    let mut dismissed = ConfirmationState::default();
    dismissed.dismiss(&name_key("ADC-VC729"));
    let names = names_with(&allowlist, dismissed);
    assert_eq!(
        names
            .out_of_scope_reply_model(
                &allowlist,
                "ADC-VC729の初期化方法は？",
                &[],
                Some(&ReplyResolution::default())
            )
            .as_deref(),
        Some("ADC-VC729")
    );

    let names = names_with(&allowlist, ConfirmationState::default());
    let denied = ReplyResolution {
        confirmed: Vec::new(),
        unresolved_surface: Some("ADC-VC729".to_string()),
    };
    assert_eq!(
        names
            .out_of_scope_reply_model(&allowlist, "いいえ、違います", &[], Some(&denied))
            .as_deref(),
        Some("ADC-VC729")
    );
    let unregistered_typo = ReplyResolution {
        confirmed: Vec::new(),
        unresolved_surface: Some("ADV-VC729P".to_string()),
    };
    assert_eq!(
        names.out_of_scope_reply_model(
            &allowlist,
            "いいえ、違います",
            &[],
            Some(&unregistered_typo)
        ),
        None
    );
}
