use axum::http::StatusCode;

use super::*;
use crate::harness::knowledge::harness_node_id;
use crate::reply_test_harness::PROJECT_ID;

const EARLIER_HEARING: &str = "ご相談ありがとうございます。製品と症状を確認させてください。\n・お使いの製品の型番をご確認ください。\n・録画されないのは、すべてのカメラですか。";

#[tokio::test]
async fn t1_the_reported_conversation_confirms_the_model_and_resumes() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();

    let first = conv.say("ADV-VC729Pを使っているが録画されない").await;
    log_turn("t1", &h, &first);
    assert_eq!(first.status, StatusCode::OK, "{:?}", first.error_body);
    assert_presented(
        &first,
        "ADV-VC729P",
        &["ADC-VC729P"],
        &["ADC-VC727P", "ADC-VC827P"],
    );
    assert_eq!(
        pending_of(&h, &first.case_id),
        vec![("ADV-VC729P".to_string(), vec!["ADC-VC729P".to_string()])]
    );
    assert!(first.product_models.is_empty());
    assert!(first.reply_text.contains("ADV-VC729P") && first.reply_text.contains("ADC-VC729P"));

    answer_yes(&h);
    let second = conv.say("はい、全く録画されていません").await;
    log_turn("t1", &h, &second);
    assert_eq!(second.case_id, first.case_id);
    assert!(!second.case_reset);
    assert_resumed(&second);
    assert_eq!(second.product_models, vec!["ADC-VC729P".to_string()]);
    assert!(pending_of(&h, &second.case_id).is_empty());
    let classification = second.llm_requests_of(LlmKind::NameConfirmationReply);
    assert_eq!(classification.len(), 1);
    assert!(
        classification[0].user.contains(&format!(
            "<直前の確認文>\n{}\n</直前の確認文>",
            first.reply_text
        )),
        "the classifier must see the confirmation the customer answers: {}",
        classification[0].user
    );
    let materials = hearing_requests(&second);
    assert!(
        materials
            .iter()
            .any(|request| request.user.contains("録画されない")),
        "the resumed turn must carry the first symptom: {:?}",
        materials
            .iter()
            .map(|request| &request.user)
            .collect::<Vec<_>>()
    );

    let third = conv.say("夜の間だけ録画されていないようです").await;
    log_turn("t1", &h, &third);
    assert_resumed(&third);
    assert!(third
        .llm_requests_of(LlmKind::NameConfirmationReply)
        .is_empty());
    assert_eq!(third.product_models, vec!["ADC-VC729P".to_string()]);
}

#[tokio::test]
async fn t1_the_first_message_with_a_misspelled_symptom_still_gets_the_candidate() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let turn = h
        .conversation()
        .say("ADV-VC729Pをつかっているが、録画されなき")
        .await;
    log_turn("t1-original", &h, &turn);
    assert_presented(&turn, "ADV-VC729P", &["ADC-VC729P"], &["ADC-VC727P"]);
}

fn earlier_history(json: &mut serde_json::Value) {
    json["history"] = serde_json::json!([
        {"role": "customer", "text": "ADV-VC729Pをつかっているが、録画されなき"},
        {"role": "assistant", "text": EARLIER_HEARING},
    ]);
}

async fn post(h: &ReplyHarness, message: &str, case_id: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({"message": message});
    earlier_history(&mut body);
    if let Some(case_id) = case_id {
        body["case_id"] = serde_json::Value::String(case_id.to_string());
    }
    let (status, text) = h.post_reply(body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).expect("a 200 reply body is JSON")
}

fn last_turn_kind(h: &ReplyHarness, case_id: &str) -> Option<String> {
    h.conversation_turns(case_id)
        .last()
        .and_then(|turn| turn.attr("reply_kind").map(ToString::to_string))
}

fn seed_case_that_already_asked(h: &ReplyHarness, case_id: &str) {
    h.store().upsert_node(
        &harness_node_id(PROJECT_ID, "support_case", case_id),
        "support_case",
        [
            ("case_id", case_id),
            ("question", "ADV-VC729Pをつかっているが、録画されなき"),
            ("clarify_turns", "1"),
            ("product_models", ""),
        ]
        .map(|(key, value)| (key.to_string(), value.to_string())),
    );
}

async fn assert_recovers(h: &ReplyHarness, existing_case: bool, case: &str) -> String {
    let mut last_case = String::new();
    for (index, message) in [
        "型番は最初に伝えている",
        "全く録画されていない、型番は最初に伝えている",
    ]
    .into_iter()
    .enumerate()
    {
        let case_id = existing_case.then(|| format!("case-t2-premise-a-{index}"));
        if let Some(case_id) = &case_id {
            seed_case_that_already_asked(h, case_id);
        }
        let before = h.llm().request_count();
        let reply = post(h, message, case_id.as_deref()).await;
        let requests = h.llm().requests()[before..].to_vec();
        let reply_case = reply["case_id"].as_str().unwrap().to_string();
        println!(
            "L2 case={case} message={message:?} reply_kind={:?} pending={:?} product_models={:?} reply_text={:?}",
            last_turn_kind(h, &reply_case),
            pending_of(h, &reply_case),
            h.product_models(&reply_case),
            reply["reply_text"]
        );
        assert_eq!(
            last_turn_kind(h, &reply_case).as_deref(),
            Some("name_confirm")
        );
        assert_eq!(
            pending_of(h, &reply_case),
            vec![("ADV-VC729P".to_string(), vec!["ADC-VC729P".to_string()])]
        );
        assert!(h.product_models(&reply_case).is_empty());
        let confirmation = requests
            .iter()
            .find(|request| request.kind == LlmKind::NameConfirmation)
            .expect("the confirmation is drafted");
        assert!(confirmation
            .user
            .contains("以前のメッセージでお知らせいただいた表記"));
        assert!(confirmation.user.contains(message));
        last_case = reply_case;
    }
    last_case
}

#[tokio::test]
async fn t2_a_conversation_without_a_case_picks_the_notation_from_the_history() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    assert_recovers(&h, false, "t2-premise-b").await;
}

#[tokio::test]
async fn t2_an_existing_case_that_already_asked_recovers_and_confirms() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let case_id = assert_recovers(&h, true, "t2-premise-a").await;
    let case_id = case_id.as_str();

    answer_yes(&h);
    let mut body = serde_json::json!({"message": "はい", "case_id": case_id});
    body["history"] = serde_json::json!([
        {"role": "customer", "text": "ADV-VC729Pをつかっているが、録画されなき"},
        {"role": "assistant", "text": EARLIER_HEARING},
        {"role": "customer", "text": "全く録画されていない、型番は最初に伝えている"},
        {"role": "assistant", "text": "ADV-VC729P とお知らせいただきましたが、ADC-VC729P のことでしょうか？"},
    ]);
    let (status, text) = h.post_reply(body).await;
    println!(
        "L2 case=t2-c status={status} body={text} product_models={:?}",
        h.product_models(case_id)
    );
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(h.product_models(case_id), vec!["ADC-VC729P".to_string()]);
    assert_ne!(last_turn_kind(&h, case_id).as_deref(), Some("name_confirm"));
    assert!(pending_of(&h, case_id).is_empty());
}

#[tokio::test]
async fn t2_when_the_confirmation_draft_fails_the_fixed_confirmation_is_returned() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    h.llm().respond_text(LlmKind::NameConfirmation, "");
    let reply = post(&h, "型番は最初に伝えている", None).await;
    let case_id = reply["case_id"].as_str().unwrap();
    println!(
        "L2 case=t2-llm-failure reply_kind={:?} reply_text={:?}",
        last_turn_kind(&h, case_id),
        reply["reply_text"]
    );
    assert_eq!(last_turn_kind(&h, case_id).as_deref(), Some("name_confirm"));
    assert_eq!(
        reply["reply_text"],
        "ADV-VC729P とお知らせいただきましたが、ADC-VC729P のことでしょうか？"
    );
}
