use super::*;
use crate::harness::decision::{AnswerDecision, DisclosureScope, EscalateReason};
use crate::harness::name_confirmation::matcher::name_key;
use crate::harness::name_confirmation::test_fixtures::{
    catalog_of, company_target, f_name_and_org, f_p7, product,
};
use crate::harness::reply::ReplyHistoryRole;
use crate::harness::signal::SignalSet;
use crate::harness::DecisionBasis;

fn names_with(allowlist: &ProductAllowlist, state: ConfirmationState) -> NameConfirmation {
    NameConfirmation {
        enabled: true,
        catalog: catalog_of(allowlist),
        state,
    }
}

fn unidentified_outcome() -> EvaluationOutcome {
    EvaluationOutcome {
        decision: AnswerDecision::Escalate {
            reason: EscalateReason::ProductUnidentified,
            layer: 3,
            route_to: "support_desk".to_string(),
            disclosure_scope: DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
            hearing: None,
            customer_ack: None,
            known_resolution_id: None,
        },
        signals: SignalSet::new(),
        accumulated_signals: SignalSet::new(),
        case_id: "case-test".to_string(),
        clarification_allowed: true,
        hits: Vec::new(),
        audit_event_id: "audit-test".to_string(),
        related_cases: Vec::new(),
        extraction_mode: ExtractionMode::Hybrid,
        customer_reply_draft: None,
        customer_reply_draft_truncated: false,
        product_references: Vec::new(),
        new_signals: SignalSet::new(),
        handoff_items: Vec::new(),
        partial_answer: false,
        decision_basis: DecisionBasis::Accumulated,
        applicable_known_resolution: None,
        immediate_handoff: false,
    }
}

fn customer(text: &str) -> ReplyHistoryTurn {
    ReplyHistoryTurn {
        role: ReplyHistoryRole::Customer,
        text: text.to_string(),
    }
}

fn assistant(text: &str) -> ReplyHistoryTurn {
    ReplyHistoryTurn {
        role: ReplyHistoryRole::Assistant,
        text: text.to_string(),
    }
}

fn pending(surface: &str, suggestions: Vec<NameTarget>, reason: AskReason) -> PendingMention {
    PendingMention {
        surface: surface.to_string(),
        surface_key: name_key(surface),
        suggestions,
        asks: 1,
        reason,
    }
}

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
    let names = names_with(&f_name_and_org(), state);
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
        names.out_of_scope_reply_model(&allowlist, message)
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
fn the_confirmed_name_replaces_the_notation_in_the_earlier_message() {
    let history = [
        customer("ADV-VC729Pを使っているが録画されない"),
        assistant("ADV-VC729P とお知らせいただきましたが、ADC-VC729P のことでしょうか？"),
        customer("関係のない発話"),
    ];
    let confirmed = vec![("ADV-VC729P".to_string(), product("ADC-VC729P"))];
    assert_eq!(
        rewritten_context(&history, "はい、全く録画されていません", &confirmed).as_deref(),
        Some("ADC-VC729Pを使っているが録画されない\nはい、全く録画されていません")
    );
    assert_eq!(rewritten_context(&[], "はい", &confirmed), None);
    assert_eq!(rewritten_context(&history, "はい", &[]), None);
}

#[test]
fn confirmed_names_join_the_known_facts() {
    let mut state = ConfirmationState::default();
    state.confirm("a", product("ADC-VC729P"));
    state.confirm("b", company_target("ミドリノ警備保障株式会社"));
    let names = names_with(&f_p7(), state);
    assert_eq!(
        names.with_confirmed_names("- 顧客発話: 録画されない".to_string()),
        "- 顧客発話: 録画されない\n- 確認済みの製品: ADC-VC729P\n- 確認済みの社名: ミドリノ警備保障株式会社"
    );
    assert_eq!(
        names_with(&f_p7(), ConfirmationState::default()).with_confirmed_names(String::new()),
        ""
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
        names.withhold_denied_names("ADC-VC729P ですか？".to_string(), "fallback"),
        "fallback"
    );
    assert_eq!(
        names.withhold_denied_names("型番を教えてください".to_string(), "fallback"),
        "型番を教えてください"
    );
    let untouched = names_with(&allowlist, ConfirmationState::default());
    assert!(matches!(
        untouched.hearing_allowlist(&allowlist),
        Cow::Borrowed(_)
    ));
}

#[test]
fn an_unknown_model_without_a_live_suggestion_after_the_answer_gets_the_out_of_scope_reply() {
    let allowlist = f_p7();
    let mut dismissed = ConfirmationState::default();
    dismissed.dismiss(&name_key("ADC-VC729"));
    let names = names_with(&allowlist, dismissed);
    let nothing = ReplyResolution::default();
    assert_eq!(
        names
            .out_of_scope_model_after_answer(&nothing, "ADC-VC729の初期化方法は？", &allowlist)
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
            .out_of_scope_model_after_answer(&denied, "いいえ、違います", &allowlist)
            .as_deref(),
        Some("ADC-VC729")
    );
    let confirmed = ReplyResolution {
        confirmed: vec![("ADC-VC729".to_string(), product("ADC-VC729P"))],
        unresolved_surface: None,
    };
    assert_eq!(
        names.out_of_scope_model_after_answer(&confirmed, "はい、ADC-VC729のことです", &allowlist),
        None
    );
    let unregistered_typo = ReplyResolution {
        confirmed: Vec::new(),
        unresolved_surface: Some("ADV-VC729P".to_string()),
    };
    assert_eq!(
        names.out_of_scope_model_after_answer(&unregistered_typo, "いいえ、違います", &allowlist),
        None
    );
}
