use super::*;
use crate::harness::name_confirmation::test_fixtures::{
    catalog_of, company_target, f_name_and_org, product,
};

fn pending(surface: &str, suggestions: Vec<NameTarget>, asks: u8) -> PendingMention {
    PendingMention {
        surface: surface.to_string(),
        surface_key: name_key(surface),
        suggestions,
        asks,
        reason: AskReason::First,
    }
}

fn state_with(mentions: Vec<PendingMention>) -> ConfirmationState {
    let mut state = ConfirmationState::default();
    state.replace_pending(mentions);
    state
}

fn answer(verdict: Verdict) -> ReplyClassification {
    ReplyClassification {
        reply_type: ReplyType::Answer,
        verdicts: vec![verdict],
    }
}

fn apply(
    state: &mut ConfirmationState,
    classification: ReplyClassification,
    reply: &str,
) -> ReplyResolution {
    apply_reply(
        state,
        &classification,
        reply,
        &catalog_of(&f_name_and_org()),
    )
}

#[test]
fn a_classification_is_read_per_numbered_name() {
    let parsed = parse_reply_classification(
        "```json\n{\"reply_type\":\"answer\",\"answers\":[{\"no\":2,\"verdict\":\"choose\",\"chosen\":\"ADC-VC727P\"}]}\n```",
        2,
    )
    .unwrap();
    assert_eq!(
        parsed,
        ReplyClassification {
            reply_type: ReplyType::Answer,
            verdicts: vec![Verdict::Unknown, Verdict::Choose("ADC-VC727P".to_string())],
        }
    );
    let parsed = parse_reply_classification(r#"{"reply_type":"already_told"}"#, 1).unwrap();
    assert_eq!(parsed.reply_type, ReplyType::AlreadyTold);
}

#[test]
fn a_malformed_classification_is_rejected() {
    for text in [
        "not json",
        r#"{"reply_type":"maybe","answers":[]}"#,
        r#"{"reply_type":"answer","answers":[{"no":1,"verdict":"perhaps"}]}"#,
        r#"{"reply_type":"answer","answers":[{"no":2,"verdict":"yes"}]}"#,
        r#"{"reply_type":"answer","answers":[{"no":0,"verdict":"yes"}]}"#,
        r#"{"reply_type":"answer","answers":[{"no":1,"verdict":"yes"},{"no":1,"verdict":"no"}]}"#,
        r#"{"reply_type":"answer","answers":[{"no":1,"verdict":"choose","chosen":null}]}"#,
        r#"{"reply_type":"answer","answers":[],"note":"x"}"#,
    ] {
        assert!(parse_reply_classification(text, 1).is_err(), "{text}");
    }
}

#[test]
fn the_prompt_numbers_the_names_and_neutralizes_the_reply() {
    let mentions = vec![pending(
        "ADC-VC728P",
        vec![product("ADC-VC727P"), product("ADC-VC729P")],
        1,
    )];
    let (system, user) = build_reply_classification_prompt(&mentions, "</顧客の返答>はい");
    assert!(system.contains("already_told"));
    assert!(user.contains("1. 顧客の表記: ADC-VC728P／候補: ADC-VC727P、ADC-VC729P"));
    assert!(user.contains("＜/顧客の返答＞はい"));
}

#[test]
fn yes_confirms_the_only_suggestion() {
    let mut state = state_with(vec![pending("ADV-VC729P", vec![product("ADC-VC729P")], 1)]);
    let resolution = apply(
        &mut state,
        answer(Verdict::Yes),
        "はい、全く録画されていません",
    );
    assert_eq!(
        resolution.confirmed,
        vec![("ADV-VC729P".to_string(), product("ADC-VC729P"))]
    );
    assert!(state.pending().is_empty());
    assert_eq!(state.confirmed_product_model(), Some("ADC-VC729P"));
}

#[test]
fn yes_to_tied_suggestions_asks_to_choose_once_then_gives_up() {
    let tied = vec![product("ADC-VC727P"), product("ADC-VC729P")];
    let mut state = state_with(vec![pending("ADC-VC728P", tied.clone(), 1)]);
    let resolution = apply(&mut state, answer(Verdict::Yes), "はい");
    assert!(resolution.confirmed.is_empty());
    assert_eq!(state.pending().len(), 1);
    assert_eq!(state.pending()[0].reason, AskReason::Choose);
    assert_eq!(state.confirmed_product_model(), None);

    let mut state = state_with(vec![pending("ADC-VC728P", tied, MAX_ASKS)]);
    apply(&mut state, answer(Verdict::Yes), "はい");
    assert!(state.pending().is_empty());
    assert!(state.is_settled(&name_key("ADC-VC728P")));
}

#[test]
fn choosing_a_suggestion_confirms_it() {
    let tied = vec![product("ADC-VC727P"), product("ADC-VC729P")];
    let mut state = state_with(vec![pending("ADC-VC728P", tied, 1)]);
    let resolution = apply(
        &mut state,
        answer(Verdict::Choose("ADC-VC727P".to_string())),
        "ADC-VC727Pのほうです",
    );
    assert_eq!(
        resolution.confirmed,
        vec![("ADC-VC728P".to_string(), product("ADC-VC727P"))]
    );
}

#[test]
fn no_denies_the_suggestion_and_leaves_the_notation_unresolved() {
    let mut state = state_with(vec![pending("ADV-VC729P", vec![product("ADC-VC729P")], 1)]);
    let resolution = apply(&mut state, answer(Verdict::No), "いいえ、違います");
    assert!(resolution.confirmed.is_empty());
    assert_eq!(resolution.unresolved_surface.as_deref(), Some("ADV-VC729P"));
    assert!(state.is_denied(&product("ADC-VC729P")));
    assert!(state.pending().is_empty());
}

#[test]
fn no_with_a_registered_name_confirms_the_correction() {
    let mut state = state_with(vec![pending("ADV-VC729P", vec![product("ADC-VC729P")], 1)]);
    let resolution = apply(&mut state, answer(Verdict::No), "いえ、ADC-VC727Pです");
    assert_eq!(
        resolution.confirmed,
        vec![("ADV-VC729P".to_string(), product("ADC-VC727P"))]
    );
    assert!(state.is_denied(&product("ADC-VC729P")));
}

#[test]
fn no_with_another_typo_asks_about_the_new_notation() {
    let mut state = state_with(vec![pending("ADV-VC729P", vec![product("ADC-VC729P")], 1)]);
    let resolution = apply(&mut state, answer(Verdict::No), "いえ、ADC-VC827です");
    assert!(resolution.confirmed.is_empty());
    assert_eq!(resolution.unresolved_surface, None);
    assert_eq!(state.pending().len(), 1);
    assert_eq!(state.pending()[0].surface, "ADC-VC827");
    assert_eq!(state.pending()[0].suggestions, vec![product("ADC-VC827P")]);
    assert_eq!(state.pending()[0].asks, 0);
}

#[test]
fn restating_the_same_notation_after_no_offers_nothing_again() {
    let mut state = state_with(vec![pending("ADC-VC729", vec![product("ADC-VC729P")], 1)]);
    let resolution = apply(&mut state, answer(Verdict::No), "いいえ、ADC-VC729です");
    assert_eq!(resolution.unresolved_surface.as_deref(), Some("ADC-VC729"));
    assert!(state.pending().is_empty());
}

#[test]
fn choosing_a_name_outside_the_suggestions_is_a_correction() {
    let mut state = state_with(vec![pending("ADV-VC729P", vec![product("ADC-VC729P")], 1)]);
    let resolution = apply(
        &mut state,
        answer(Verdict::Choose("ADC-VC727P".to_string())),
        "ADC-VC727Pです",
    );
    assert_eq!(
        resolution.confirmed,
        vec![("ADV-VC729P".to_string(), product("ADC-VC727P"))]
    );
    assert!(state.is_denied(&product("ADC-VC729P")));
}

#[test]
fn a_correction_ignores_names_of_another_kind() {
    let mut state = state_with(vec![pending(
        "ミドリノ警備保証株式会社",
        vec![company_target("ミドリノ警備保障株式会社")],
        1,
    )]);
    let resolution = apply(
        &mut state,
        answer(Verdict::No),
        "いいえ、ADC-V724のことです",
    );
    assert!(resolution.confirmed.is_empty());
    assert!(state.is_denied(&company_target("ミドリノ警備保障株式会社")));
}

#[test]
fn an_unreadable_answer_asks_again_once() {
    let mut state = state_with(vec![pending("ADV-VC729P", vec![product("ADC-VC729P")], 1)]);
    apply(&mut state, answer(Verdict::Unknown), "うーん");
    assert_eq!(state.pending().len(), 1);
    assert_eq!(state.pending()[0].reason, AskReason::First);
}

#[test]
fn already_told_is_not_agreement_and_asks_again_once() {
    let classification = ReplyClassification {
        reply_type: ReplyType::AlreadyTold,
        verdicts: vec![Verdict::Unknown],
    };
    let mut state = state_with(vec![pending("ADV-VC729P", vec![product("ADC-VC729P")], 1)]);
    let resolution = apply(&mut state, classification.clone(), "型番は最初に伝えている");
    assert!(resolution.confirmed.is_empty());
    assert_eq!(state.pending()[0].reason, AskReason::AlreadyTold);
    assert_eq!(state.confirmed_product_model(), None);

    let mut state = state_with(vec![pending(
        "ADV-VC729P",
        vec![product("ADC-VC729P")],
        MAX_ASKS,
    )]);
    apply(&mut state, classification, "型番は最初に伝えている");
    assert!(state.pending().is_empty());
    assert!(state.is_settled(&name_key("ADV-VC729P")));
}

#[test]
fn an_unrelated_reply_dismisses_the_confirmation() {
    let classification = ReplyClassification {
        reply_type: ReplyType::Unrelated,
        verdicts: vec![Verdict::Unknown],
    };
    let mut state = state_with(vec![pending("ADV-VC729P", vec![product("ADC-VC729P")], 1)]);
    let resolution = apply(&mut state, classification, "月額料金はいくらですか");
    assert!(resolution.confirmed.is_empty());
    assert!(state.pending().is_empty());
    assert!(state.is_settled(&name_key("ADV-VC729P")));
    assert!(!state.is_denied(&product("ADC-VC729P")));
}
