use super::*;
use crate::harness::name_confirmation::flow::test_support::{
    assistant, customer, names_with, pending, presented_keys,
};
use crate::harness::name_confirmation::matcher::name_key;
use crate::harness::name_confirmation::state::AskReason;
use crate::harness::name_confirmation::test_fixtures::{f_p7, product};

#[test]
fn a_new_case_after_a_switch_confirms_names_again() {
    let mut state = ConfirmationState::default();
    state.push_pending(pending(
        "ADV-VC729P",
        vec![product("ADC-VC729P")],
        AskReason::First,
    ));
    let mut names = NameConfirmation {
        enabled: false,
        ..names_with(&f_p7(), state)
    };
    assert_eq!(
        presented_keys(&names, "ADC-V523とADV-VC729Pを使っているが録画されない"),
        Vec::<String>::new()
    );
    names.start_new_case();
    assert_eq!(
        presented_keys(&names, "ADC-V523とADV-VC729Pを使っているが録画されない"),
        vec![name_key("ADV-VC729P")]
    );
    assert!(names.state.pending().is_empty());

    let mut without_llm = NameConfirmation {
        llm_configured: false,
        enabled: false,
        ..names_with(&f_p7(), ConfirmationState::default())
    };
    without_llm.start_new_case();
    assert!(!without_llm.enabled);
}

#[test]
fn the_confirmed_name_replaces_the_notation_in_the_earlier_message() {
    let history = [
        customer("ADV-VC729Pを使っているが録画されない"),
        assistant("ADV-VC729P とお知らせいただきましたが、ADC-VC729P のことでしょうか？"),
        customer("関係のない発話"),
    ];
    let confirmed = ReplyResolution {
        confirmed: vec![("ADV-VC729P".to_string(), product("ADC-VC729P"))],
        unresolved_surface: None,
    };
    assert_eq!(
        rewritten_context(&history, "はい、全く録画されていません", &confirmed).as_deref(),
        Some("ADC-VC729Pを使っているが録画されない\nはい、全く録画されていません")
    );
    assert_eq!(rewritten_context(&[], "はい", &confirmed), None);
    assert_eq!(
        rewritten_context(&history, "はい", &ReplyResolution::default()),
        None
    );
}
