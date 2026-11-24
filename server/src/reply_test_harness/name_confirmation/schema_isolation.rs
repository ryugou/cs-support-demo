use super::*;
use crate::reply_test_harness::PROJECT_ID;

use crate::reply_test_harness::llm_stub::LlmKind;

const OTHER_PROJECT: &str = "tenant-b";
const SERVER_WIDE_SIGNAL_LEXICON_REQUEST: LlmKind = LlmKind::Classify;
const OTHER_MODEL: &str = "BXR-3105";
const OTHER_PRODUCT_NAME: &str = "あおぞら屋外センサー";
const OTHER_COMPANY: &str = "ミナミ設備株式会社";
const OTHER_ORG_FILE: &str = r#"[{"key": "minami", "name": "ミナミ設備株式会社", "aliases": []}]"#;
const OTHER_PROJECT_NAMES: [&str; 3] = [OTHER_MODEL, OTHER_PRODUCT_NAME, OTHER_COMPANY];
const OWN_PROJECT_NAMES: [&str; 4] = [
    "ADC-V724",
    "みまもりドームカメラ",
    "ミドリノ警備保障株式会社",
    "ヒガシ電設工業株式会社",
];

fn other_project_fixture() -> Fixture {
    Fixture::for_schema(OTHER_PROJECT)
        .product(OTHER_MODEL, OTHER_PRODUCT_NAME, &[])
        .product("BXR-7720", "BXR-7720", &[])
        .bundled_rules()
        .manual_section(
            "sec-recording",
            "録画されない場合の確認",
            RECORDING_SECTION_BODY,
            &[OTHER_MODEL, "BXR-7720"],
            &[],
        )
}

async fn start_two_projects() -> ReplyHarness {
    let h =
        ReplyHarness::start_with_other_projects(f_name_fixture(), vec![other_project_fixture()])
            .await;
    h.llm().respond_classify(&[], true);
    h
}

fn assert_never_shows(turn: &Turn, names: &[&str]) {
    for name in names {
        assert!(
            !turn.reply_text.contains(name),
            "the reply shows {name:?} from another project: {}",
            turn.log_line()
        );
        for request in turn
            .llm_requests
            .iter()
            .filter(|request| request.kind != SERVER_WIDE_SIGNAL_LEXICON_REQUEST)
        {
            assert!(
                !request.user.contains(name) && !request.system.contains(name),
                "an LLM request carries {name:?} from another project: {:?} {}",
                request.kind,
                request.user
            );
        }
    }
}

fn assert_not_proposed(turn: &Turn) {
    assert_ne!(
        turn.reply_kind.as_deref(),
        Some("name_confirm"),
        "{}",
        turn.log_line()
    );
    assert!(
        confirmation_requests(turn).is_empty(),
        "{}",
        turn.log_line()
    );
}

#[tokio::test]
async fn t7_names_registered_only_for_another_project_are_not_proposed_on_the_same_server() {
    let h = start_two_projects().await;
    register_companies_in(&h, PROJECT_ID, F_ORG_FILE).await;
    register_companies_in(&h, OTHER_PROJECT, OTHER_ORG_FILE).await;

    let mut other = h.conversation_in(OTHER_PROJECT);
    let other_first = other
        .say("ミナミ設備株式会杜から購入したBXR-3105が録画されません")
        .await;
    println!("L2 case=t7-other-first {}", other_first.log_line());
    assert_presented(
        &other_first,
        "ミナミ設備株式会杜",
        &[OTHER_COMPANY],
        &OWN_PROJECT_NAMES,
    );
    assert_never_shows(&other_first, &OWN_PROJECT_NAMES);

    let typos_of_other_names = [
        "ミナミ設備株式会杜から購入したADC-V724が録画されません",
        "あおぞら屋外センザーが録画されません",
        "BXR-3106が録画されません",
    ];
    for message in typos_of_other_names {
        let turn = h.conversation().say(message).await;
        log_turn("t7-own-typo-of-other-name", &h, &turn);
        assert_not_proposed(&turn);
        assert_never_shows(&turn, &OTHER_PROJECT_NAMES);
        assert!(name_state(&h, &turn.case_id)["pending"]
            .as_array()
            .is_none_or(Vec::is_empty));
    }

    let mut own = h.conversation();
    let own_first = own
        .say("ミドリノ警備保証株式会社から購入したADC-V724が録画されません")
        .await;
    log_turn("t7-own-first", &h, &own_first);
    assert_presented(
        &own_first,
        "ミドリノ警備保証株式会社",
        &["ミドリノ警備保障株式会社"],
        &OTHER_PROJECT_NAMES,
    );

    answer_yes(&h);
    let other_second = other.say("はい、そうです").await;
    println!("L2 case=t7-other-second {}", other_second.log_line());
    assert_resumed(&other_second);
    assert_eq!(other_second.product_models, vec![OTHER_MODEL.to_string()]);
    assert_eq!(
        confirmed_in(&name_state_in(&h, OTHER_PROJECT, &other_second.case_id)),
        vec![OTHER_COMPANY.to_string()]
    );
    assert_never_shows(&other_second, &OWN_PROJECT_NAMES);

    let own_second = own.say("はい、合っています").await;
    log_turn("t7-own-second", &h, &own_second);
    assert_resumed(&own_second);
    assert_eq!(own_second.product_models, vec!["ADC-V724".to_string()]);
    assert_eq!(
        confirmed_of(&h, &own_second.case_id),
        vec!["ミドリノ警備保障株式会社".to_string()]
    );
    assert_never_shows(&own_second, &OTHER_PROJECT_NAMES);

    let own_third = own.say("ミナミ設備株式会杜にも相談しました").await;
    log_turn("t7-own-third", &h, &own_third);
    assert_not_proposed(&own_third);
    assert_never_shows(&own_third, &OTHER_PROJECT_NAMES);
    assert_eq!(
        confirmed_of(&h, &own_third.case_id),
        vec!["ミドリノ警備保障株式会社".to_string()]
    );

    assert!(h
        .case_attributes_in(OTHER_PROJECT, &own_first.case_id)
        .is_none());
    assert!(h.case_attributes(&other_first.case_id).is_none());
}

#[tokio::test]
async fn t7_a_company_registered_in_both_projects_is_proposed_from_the_projects_own_registration() {
    let h = start_two_projects().await;
    register_companies_in(&h, OTHER_PROJECT, OTHER_ORG_FILE).await;
    register_companies_in(&h, PROJECT_ID, OTHER_ORG_FILE).await;
    register_companies_in(
        &h,
        OTHER_PROJECT,
        r#"[{"key": "kita", "name": "キタ電機株式会社", "aliases": []}]"#,
    )
    .await;
    let typo = "ミナミ設備株式会杜から購入したADC-V724が録画されません";

    let mut own = h.conversation();
    let first = own.say(typo).await;
    log_turn("t7-registered-in-both", &h, &first);
    assert_presented(
        &first,
        "ミナミ設備株式会杜",
        &[OTHER_COMPANY],
        &["キタ電機株式会社", OTHER_MODEL, OTHER_PRODUCT_NAME],
    );

    answer_yes(&h);
    let second = own.say("はい").await;
    log_turn("t7-registered-in-both", &h, &second);
    assert_resumed(&second);
    assert_eq!(
        confirmed_of(&h, &second.case_id),
        vec![OTHER_COMPANY.to_string()]
    );
    assert_eq!(second.product_models, vec!["ADC-V724".to_string()]);

    let other_turn = h
        .conversation_in(OTHER_PROJECT)
        .say("ミナミ設備株式会杜から購入したBXR-3105が録画されません")
        .await;
    println!("L2 case=t7-retired-in-other {}", other_turn.log_line());
    assert_not_proposed(&other_turn);
    assert_never_shows(&other_turn, &OWN_PROJECT_NAMES);
}
