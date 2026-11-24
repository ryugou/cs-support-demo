use super::*;

#[tokio::test]
async fn t7_an_exact_model_question_takes_the_current_path() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let turn = h.conversation().say("ADC-V724の設定を教えてください").await;
    log_turn("t7-exact", &h, &turn);
    assert_resumed(&turn);
    assert!(turn
        .llm_requests_of(LlmKind::NameConfirmationReply)
        .is_empty());
    assert_eq!(name_state(&h, &turn.case_id), serde_json::Value::Null);
}

#[tokio::test]
async fn t7_a_safety_escalation_takes_the_current_path() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let turn = h
        .conversation()
        .say("ADC-V724から焦げ臭いにおいがします")
        .await;
    log_turn("t7-safety", &h, &turn);
    assert_eq!(turn.reply_kind.as_deref(), Some("escalation"));
    assert_eq!(turn.decision.as_deref(), Some("escalate:layer1"));
}

#[tokio::test]
async fn t7_the_contact_time_reply_is_not_interrupted() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    let escalated = conv.say("ADC-V724から焦げ臭いにおいがします").await;
    assert!(h.conv_state(&escalated.case_id).await.awaiting_time_pref);
    h.llm().respond_text(
        LlmKind::TimePref,
        r#"{"is_time_preference": true, "windows": [{"days": "weekday", "start": "12:00", "end": "18:00"}], "raw": "平日の午後"}"#,
    );
    let time = conv.say("平日の午後でお願いします").await;
    log_turn("t7-time-pref", &h, &time);
    assert_eq!(time.reply_kind.as_deref(), Some("time_pref"));
    assert!(time
        .llm_requests_of(LlmKind::NameConfirmationReply)
        .is_empty());
}

#[tokio::test]
async fn t7_an_escalated_case_asks_no_name_confirmation() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say("ADC-V724から焦げ臭いにおいがします").await;
    let typo = conv.say("ADV-VC729Pも使っています").await;
    log_turn("t7-escalated", &h, &typo);
    assert_ne!(typo.reply_kind.as_deref(), Some("name_confirm"));
    assert!(confirmation_requests(&typo).is_empty());
}

#[tokio::test]
async fn t7_the_contact_time_reply_is_not_interrupted_by_a_pending_confirmation() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    let first = conv.say("ADV-VC729Pを使っているが録画されない").await;
    log_turn("t7-time-pref-pending", &h, &first);

    answer_reply(
        &h,
        serde_json::json!([{"no": 1, "verdict": "unknown", "chosen": null}]),
    );
    let escalated = conv
        .say("わかりません。それより焦げ臭いにおいがします")
        .await;
    log_turn("t7-time-pref-pending", &h, &escalated);
    assert_eq!(escalated.reply_kind.as_deref(), Some("escalation"));
    assert_eq!(
        pending_of(&h, &escalated.case_id),
        vec![("ADV-VC729P".to_string(), vec!["ADC-VC729P".to_string()])]
    );
    assert!(h.conv_state(&escalated.case_id).await.awaiting_time_pref);

    h.llm().respond_text(
        LlmKind::TimePref,
        r#"{"is_time_preference": true, "windows": [{"days": "weekday", "start": "12:00", "end": "18:00"}], "raw": "平日の午後"}"#,
    );
    let time = conv.say("平日の午後でお願いします").await;
    log_turn("t7-time-pref-pending", &h, &time);
    assert_eq!(time.reply_kind.as_deref(), Some("time_pref"));
    assert!(time
        .llm_requests_of(LlmKind::NameConfirmationReply)
        .is_empty());
    assert!(confirmation_requests(&time).is_empty());
}
