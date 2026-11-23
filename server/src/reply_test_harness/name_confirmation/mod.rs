mod misproposals;
mod real_llm;
mod registered_names;
mod regressions;
mod reply_paths;
mod reported_conversation;

use super::llm_stub::{LlmKind, LlmRequest};
use super::{Fixture, ReplyHarness, Turn};
use crate::harness::name_confirmation::company::{
    parse_company_file, sync_registered_companies, CompanySyncSummary,
};

const RECORDING_SECTION_BODY: &str = "録画されない場合は、SDカードの空き容量と録画スケジュールの設定を確認してください。録画が全く保存されない場合は、カメラを再起動してから録画設定を開き、録画が有効になっていることを確認してください。";

const F_ORG_FILE: &str = r#"[
  {"key": "midorino", "name": "ミドリノ警備保障株式会社", "aliases": []},
  {"key": "higashi", "name": "ヒガシ電設工業株式会社", "aliases": []}
]"#;

const PRODUCT_UNIDENTIFIED: &str = "escalate:layer3:product_unidentified";

fn with_recording_section(fixture: Fixture) -> Fixture {
    fixture.bundled_rules().manual_section(
        "sec-recording",
        "録画されない場合の確認",
        RECORDING_SECTION_BODY,
        &[
            "ADC-V523",
            "ADC-V523X",
            "ADC-V724",
            "ADC-V724X",
            "ADC-VC729P",
            "ADC-VC727P",
            "ADC-VC827P",
        ],
        &[],
    )
}

fn f_p7_fixture() -> Fixture {
    with_recording_section(Fixture::new().products_f_p7())
}

fn f_name_fixture() -> Fixture {
    with_recording_section(Fixture::new().products_f_name())
}

async fn register_companies(h: &ReplyHarness, file: &str) -> CompanySyncSummary {
    let companies = parse_company_file(file).expect("the company file is valid");
    sync_registered_companies(&h.vegapunk_client(), super::PROJECT_ID, &companies)
        .await
        .expect("the registration procedure writes to the fake vegapunk")
}

fn answer_reply(h: &ReplyHarness, answers: serde_json::Value) {
    let body = serde_json::json!({"reply_type": "answer", "answers": answers}).to_string();
    h.llm().respond_text(LlmKind::NameConfirmationReply, &body);
}

fn answer_yes(h: &ReplyHarness) {
    answer_reply(
        h,
        serde_json::json!([{"no": 1, "verdict": "yes", "chosen": null}]),
    );
}

fn answer_no(h: &ReplyHarness) {
    answer_reply(
        h,
        serde_json::json!([{"no": 1, "verdict": "no", "chosen": null}]),
    );
}

fn answer_choose(h: &ReplyHarness, chosen: &str) {
    answer_reply(
        h,
        serde_json::json!([{"no": 1, "verdict": "choose", "chosen": chosen}]),
    );
}

fn reply_type(h: &ReplyHarness, reply_type: &str) {
    let body = serde_json::json!({"reply_type": reply_type, "answers": []}).to_string();
    h.llm().respond_text(LlmKind::NameConfirmationReply, &body);
}

fn name_state(h: &ReplyHarness, case_id: &str) -> serde_json::Value {
    h.case_attributes(case_id)
        .and_then(|case| case.attr("name_confirmation").map(ToString::to_string))
        .map(|written| serde_json::from_str(&written).expect("the stored state is JSON"))
        .unwrap_or(serde_json::Value::Null)
}

fn pending_of(h: &ReplyHarness, case_id: &str) -> Vec<(String, Vec<String>)> {
    name_state(h, case_id)["pending"]
        .as_array()
        .map(|pending| {
            pending
                .iter()
                .map(|mention| {
                    let suggestions = mention["suggestions"]
                        .as_array()
                        .map(|targets| {
                            targets
                                .iter()
                                .filter_map(|target| target["id"].as_str().map(ToString::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    (
                        mention["surface"].as_str().unwrap_or_default().to_string(),
                        suggestions,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn confirmed_of(h: &ReplyHarness, case_id: &str) -> Vec<String> {
    name_state(h, case_id)["confirmed"]
        .as_array()
        .map(|confirmed| {
            confirmed
                .iter()
                .filter_map(|entry| entry["target"]["id"].as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn denied_of(h: &ReplyHarness, case_id: &str) -> Vec<String> {
    name_state(h, case_id)["denied"]
        .as_array()
        .map(|denied| {
            denied
                .iter()
                .filter_map(|entry| entry["id"].as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn log_turn(case: &str, h: &ReplyHarness, turn: &Turn) {
    println!(
        "L2 case={case} {} pending={:?} confirmed={:?} denied={:?}",
        turn.log_line(),
        pending_of(h, &turn.case_id),
        confirmed_of(h, &turn.case_id),
        denied_of(h, &turn.case_id),
    );
}

fn confirmation_requests(turn: &Turn) -> Vec<&LlmRequest> {
    turn.llm_requests_of(LlmKind::NameConfirmation)
}

fn hearing_requests(turn: &Turn) -> Vec<&LlmRequest> {
    turn.llm_requests
        .iter()
        .filter(|request| {
            matches!(
                request.kind,
                LlmKind::Reply | LlmKind::Clarify | LlmKind::ProductIdentification
            )
        })
        .collect()
}

fn assert_presented(turn: &Turn, written: &str, suggestions: &[&str], left_out: &[&str]) {
    assert_eq!(
        turn.reply_kind.as_deref(),
        Some("name_confirm"),
        "{}",
        turn.log_line()
    );
    let requests = confirmation_requests(turn);
    assert_eq!(requests.len(), 1, "{}", turn.log_line());
    assert!(requests[0].user.contains(written), "{}", requests[0].user);
    for suggestion in suggestions {
        assert!(
            requests[0].user.contains(suggestion),
            "{}",
            requests[0].user
        );
    }
    for name in left_out {
        assert!(!requests[0].user.contains(name), "{}", requests[0].user);
    }
    assert!(
        turn.llm_requests_of(LlmKind::Reply).is_empty(),
        "a confirmation turn must not draft an answer: {}",
        turn.log_line()
    );
}

fn assert_resumed(turn: &Turn) {
    assert_ne!(
        turn.reply_kind.as_deref(),
        Some("name_confirm"),
        "{}",
        turn.log_line()
    );
    assert_ne!(
        turn.decision.as_deref(),
        Some(PRODUCT_UNIDENTIFIED),
        "{}",
        turn.log_line()
    );
    assert!(
        confirmation_requests(turn).is_empty(),
        "{}",
        turn.log_line()
    );
}
