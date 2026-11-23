use super::*;
use crate::harness::name_confirmation::test_fixtures::{company_target, product};

fn pending(surface: &str, suggestions: Vec<NameTarget>) -> PendingMention {
    PendingMention {
        surface: surface.to_string(),
        surface_key: surface.to_lowercase(),
        suggestions,
        asks: 1,
        reason: AskReason::First,
    }
}

fn attributes_with(state: &str) -> HashMap<String, String> {
    HashMap::from([(STATE_ATTRIBUTE.to_string(), state.to_string())])
}

#[test]
fn a_case_without_the_attribute_starts_empty() {
    let state = ConfirmationState::from_case_attributes(&HashMap::new()).unwrap();
    assert_eq!(state, ConfirmationState::default());
}

#[test]
fn the_state_round_trips_through_the_case_attribute() {
    let mut state = ConfirmationState::default();
    state.push_pending(pending("ADV-VC729P", vec![product("ADC-VC729P")]));
    state.confirm("advvc729p", product("ADC-VC729P"));
    state.deny(&[company_target("ヒガシ電設工業株式会社")]);
    state.dismiss("adcvc728p");
    let attributes = attributes_with(&state.to_attribute().unwrap());
    assert_eq!(
        ConfirmationState::from_case_attributes(&attributes).unwrap(),
        state
    );
}

#[test]
fn a_malformed_or_incomplete_state_is_rejected() {
    let incomplete_pending = r#"{"pending":[{"surface":"","surface_key":"x","suggestions":[{"kind":"product","id":"A","display":"A"}],"asks":1,"reason":"first"}],"confirmed":[],"denied":[],"dismissed":[]}"#;
    let no_suggestion = r#"{"pending":[{"surface":"x","surface_key":"x","suggestions":[],"asks":1,"reason":"first"}],"confirmed":[],"denied":[],"dismissed":[]}"#;
    let too_many_asks = r#"{"pending":[{"surface":"x","surface_key":"x","suggestions":[{"kind":"product","id":"A","display":"A"}],"asks":9,"reason":"first"}],"confirmed":[],"denied":[],"dismissed":[]}"#;
    let unknown_kind = r#"{"pending":[],"confirmed":[{"surface_key":"x","target":{"kind":"vendor","id":"A","display":"A"}}],"denied":[],"dismissed":[]}"#;
    let unknown_field = r#"{"pending":[],"confirmed":[],"denied":[],"dismissed":[],"extra":1}"#;
    let missing_field = r#"{"pending":[],"confirmed":[],"denied":[]}"#;
    for written in [
        "",
        "not json",
        incomplete_pending,
        no_suggestion,
        too_many_asks,
        unknown_kind,
        unknown_field,
        missing_field,
    ] {
        assert!(
            ConfirmationState::from_case_attributes(&attributes_with(written)).is_err(),
            "{written}"
        );
    }
}

#[test]
fn denial_and_settlement_are_recorded_per_name() {
    let mut state = ConfirmationState::default();
    state.deny(&[product("ADC-VC729P")]);
    state.confirm("advvc729p", product("ADC-VC727P"));
    state.dismiss("adcvc728p");
    assert!(state.is_denied(&product("ADC-VC729P")));
    assert!(!state.is_denied(&product("ADC-VC727P")));
    assert!(state.is_settled("advvc729p"));
    assert!(state.is_settled("adcvc728p"));
    assert!(!state.is_settled("adcvc827"));
    assert_eq!(state.confirmed_product_model(), Some("ADC-VC727P"));
    assert_eq!(state.denied_ids(NameKind::Product), vec!["ADC-VC729P"]);
}

#[test]
fn the_latest_confirmed_product_wins_and_companies_are_not_products() {
    let mut state = ConfirmationState::default();
    state.confirm("a", product("ADC-V724"));
    state.confirm("b", company_target("ミドリノ警備保障株式会社"));
    assert_eq!(state.confirmed_product_model(), Some("ADC-V724"));
    state.confirm("c", product("ADC-VC729P"));
    assert_eq!(state.confirmed_product_model(), Some("ADC-VC729P"));
}

#[test]
fn records_keep_only_the_newest_entries() {
    let mut state = ConfirmationState::default();
    for index in 0..MAX_RECORDS + 3 {
        state.dismiss(&format!("key{index}"));
    }
    assert!(!state.is_settled("key0"));
    assert!(state.is_settled(&format!("key{}", MAX_RECORDS + 2)));
    let attributes = attributes_with(&state.to_attribute().unwrap());
    assert!(ConfirmationState::from_case_attributes(&attributes).is_ok());
}

#[test]
fn pending_mentions_are_capped_and_unique() {
    let mut state = ConfirmationState::default();
    for surface in ["a", "a", "b", "c", "d"] {
        state.push_pending(pending(surface, vec![product("ADC-V724")]));
    }
    let surfaces: Vec<&str> = state
        .pending()
        .iter()
        .map(|mention| mention.surface.as_str())
        .collect();
    assert_eq!(surfaces, vec!["a", "b", "c"]);
}
