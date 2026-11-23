use super::*;
use crate::harness::product_gate::{build_out_of_scope_reply, ProductAllowlist};

const TYPO_MESSAGE: &str = "ADV-VC729Pを使っているが録画されない";

fn f_p7_allowlist() -> ProductAllowlist {
    ProductAllowlist::from_models(
        [
            "ADC-V523",
            "ADC-V523X",
            "ADC-V724",
            "ADC-V724X",
            "ADC-VC729P",
            "ADC-VC727P",
            "ADC-VC827P",
        ]
        .map(ToString::to_string)
        .to_vec(),
    )
}

#[tokio::test]
async fn t4_exact_registered_names_are_used_without_confirmation() {
    let cases = [
        (f_p7_fixture(), "ADC-V724が録画されない", "ADC-V724"),
        (f_p7_fixture(), "adc-v724xが録画されない", "ADC-V724X"),
        (
            f_p7_fixture(),
            "ＡＤＣ－ＶＣ７２９Ｐが録画されない",
            "ADC-VC729P",
        ),
        (
            f_name_fixture(),
            "みまもりドームカメラが録画されない",
            "ADC-V724",
        ),
        (f_name_fixture(), "見守りドームが録画されない", "ADC-V724"),
    ];
    for (fixture, message, model) in cases {
        let h = ReplyHarness::start(fixture).await;
        h.llm().respond_classify(&[], true);
        h.llm()
            .respond_text(LlmKind::ProductReferences, r#"{"product_references":[]}"#);
        let turn = h.conversation().say(message).await;
        log_turn("t4-exact", &h, &turn);
        assert_resumed(&turn);
        assert_eq!(turn.product_models, vec![model.to_string()], "{message}");
    }
}

#[tokio::test]
async fn t4_tied_candidates_are_listed_and_the_choice_decides() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    let first = conv.say("ADC-VC728Pが録画されない").await;
    log_turn("t4-tie", &h, &first);
    assert_presented(
        &first,
        "ADC-VC728P",
        &["ADC-VC729P", "ADC-VC727P"],
        &["ADC-VC827P"],
    );
    let pending = pending_of(&h, &first.case_id);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].1.len(), 2);

    answer_choose(&h, "ADC-VC727P");
    let chosen = conv.say("ADC-VC727Pのほうです").await;
    log_turn("t4-tie", &h, &chosen);
    assert_resumed(&chosen);
    assert_eq!(chosen.product_models, vec!["ADC-VC727P".to_string()]);
}

#[tokio::test]
async fn t4_a_bare_yes_to_tied_candidates_asks_to_choose_again() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say("ADC-VC728Pが録画されない").await;
    answer_yes(&h);
    let yes = conv.say("はい").await;
    log_turn("t4-tie-yes", &h, &yes);
    assert!(yes.product_models.is_empty());
    assert_eq!(yes.reply_kind.as_deref(), Some("name_confirm"));
    assert_eq!(
        name_state(&h, &yes.case_id)["pending"][0]["reason"],
        serde_json::json!("choose")
    );
    let again = conv.say("はい").await;
    log_turn("t4-tie-yes", &h, &again);
    assert!(again.product_models.is_empty());
    assert_ne!(again.reply_kind.as_deref(), Some("name_confirm"));
}

#[tokio::test]
async fn t4_notations_without_a_close_name_are_accepted_as_written() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let unknown_model = h
        .conversation()
        .say("ADC-VDB101の設定を教えてください")
        .await;
    log_turn("t4-none", &h, &unknown_model);
    assert_eq!(unknown_model.reply_kind.as_deref(), Some("out_of_scope"));
    assert_eq!(
        unknown_model.reply_text,
        build_out_of_scope_reply("ADC-VDB101", &f_p7_allowlist())
    );
    assert!(pending_of(&h, &unknown_model.case_id).is_empty());
    for message in [
        "QX-9000を使っているが録画されない",
        "729のカメラが録画されない",
    ] {
        let turn = h.conversation().say(message).await;
        log_turn("t4-none", &h, &turn);
        assert_ne!(
            turn.reply_kind.as_deref(),
            Some("name_confirm"),
            "{message}"
        );
        assert!(confirmation_requests(&turn).is_empty(), "{message}");
        assert!(pending_of(&h, &turn.case_id).is_empty(), "{message}");
    }
}

#[tokio::test]
async fn t4_a_similar_meaning_is_never_offered_as_a_candidate() {
    let h = ReplyHarness::start(f_name_fixture()).await;
    h.llm().respond_classify(&[], true);
    h.llm().respond_text(
        LlmKind::ProductReferences,
        r#"{"product_references":[{"surface":"見守り用の丸いカメラ","resolution":"matched","matched_model":"ADC-V724"}]}"#,
    );
    let turn = h
        .conversation()
        .say("見守り用の丸いカメラが録画されない")
        .await;
    log_turn("t4-meaning", &h, &turn);
    assert_ne!(turn.reply_kind.as_deref(), Some("name_confirm"));
    assert!(confirmation_requests(&turn).is_empty());
}

#[tokio::test]
async fn t4_a_denied_candidate_is_not_offered_again() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say(TYPO_MESSAGE).await;

    answer_no(&h);
    let denied = conv.say("いいえ、違います").await;
    log_turn("t4-deny", &h, &denied);
    assert!(denied.product_models.is_empty());
    assert!(confirmation_requests(&denied).is_empty());
    assert_eq!(
        denied_of(&h, &denied.case_id),
        vec!["ADC-VC729P".to_string()]
    );
    for request in hearing_requests(&denied) {
        let offered = request
            .user
            .lines()
            .filter(|line| !line.starts_with("- 顧客が違うと答えた候補（返信に書かない）: "))
            .any(|line| line.contains("ADC-VC729P"));
        assert!(!offered, "{}", request.user);
        assert!(!request.system.contains("ADC-VC729P"), "{}", request.system);
        assert!(
            request
                .user
                .contains("- 顧客が違うと答えた候補（返信に書かない）: ADC-VC729P"),
            "{}",
            request.user
        );
        assert!(
            request
                .user
                .contains("- 顧客がお知らせくださった表記: ADV-VC729P"),
            "{}",
            request.user
        );
    }
    assert!(!denied.reply_text.contains("ADC-VC729P"));

    let repeated = conv.say("ADV-VC729Pです").await;
    log_turn("t4-deny", &h, &repeated);
    assert!(repeated.product_models.is_empty());
    assert!(confirmation_requests(&repeated).is_empty());
    assert!(!repeated.reply_text.contains("ADC-VC729P"));
}

#[tokio::test]
async fn t4_a_denied_candidate_for_an_unknown_model_ends_in_the_out_of_scope_reply() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    let first = conv.say("ADC-VC729の設定を教えてください").await;
    log_turn("t4-deny-out-of-scope", &h, &first);
    assert_presented(&first, "ADC-VC729", &["ADC-VC729P"], &[]);

    answer_no(&h);
    let denied = conv.say("いいえ、ADC-VC729です").await;
    log_turn("t4-deny-out-of-scope", &h, &denied);
    assert_eq!(denied.reply_kind.as_deref(), Some("out_of_scope"));
    assert_eq!(
        denied.reply_text,
        build_out_of_scope_reply("ADC-VC729", &f_p7_allowlist())
    );
}

#[tokio::test]
async fn t4_a_correction_to_a_registered_name_resumes_with_the_first_symptom() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    let first = conv.say(TYPO_MESSAGE).await;
    answer_no(&h);
    let corrected = conv.say("いえ、ADC-VC727Pです").await;
    log_turn("t4-correct-exact", &h, &corrected);
    assert_eq!(corrected.case_id, first.case_id);
    assert!(!corrected.case_reset);
    assert_resumed(&corrected);
    assert_eq!(corrected.product_models, vec!["ADC-VC727P".to_string()]);
    assert!(hearing_requests(&corrected)
        .iter()
        .any(|request| request.user.contains("録画されない")));
}

#[tokio::test]
async fn t4_a_correction_with_another_typo_is_matched_again() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say(TYPO_MESSAGE).await;
    answer_no(&h);
    let retyped = conv.say("いえ、ADC-VC827です").await;
    log_turn("t4-correct-typo", &h, &retyped);
    assert_presented(&retyped, "ADC-VC827", &["ADC-VC827P"], &["ADC-VC729P"]);
    assert_eq!(
        pending_of(&h, &retyped.case_id),
        vec![("ADC-VC827".to_string(), vec!["ADC-VC827P".to_string()])]
    );
}

#[tokio::test]
async fn t4_switching_to_another_product_drops_the_pending_candidate() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say(TYPO_MESSAGE).await;
    reply_type(&h, "unrelated");
    let switched = conv.say("ADC-V724の設置方法を教えてください").await;
    log_turn("t4-switch-product", &h, &switched);
    assert!(confirmation_requests(&switched).is_empty());
    assert!(switched.product_models.contains(&"ADC-V724".to_string()));
    assert!(!switched.product_models.contains(&"ADC-VC729P".to_string()));
    assert!(pending_of(&h, &switched.case_id).is_empty());
}

#[tokio::test]
async fn t4_switching_to_another_topic_drops_the_pending_candidate_for_good() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say(TYPO_MESSAGE).await;
    reply_type(&h, "unrelated");
    let switched = conv.say("月額料金はいくらですか").await;
    log_turn("t4-switch-topic", &h, &switched);
    assert!(confirmation_requests(&switched).is_empty());
    assert!(switched.product_models.is_empty());

    answer_yes(&h);
    let yes = conv.say("はい").await;
    log_turn("t4-switch-topic", &h, &yes);
    assert!(yes.product_models.is_empty());
    assert!(confirmed_of(&h, &yes.case_id).is_empty());
    assert!(yes
        .llm_requests_of(LlmKind::NameConfirmationReply)
        .is_empty());
    assert!(confirmation_requests(&yes).is_empty());
}

#[tokio::test]
async fn t4_after_confirmation_another_model_starts_a_new_case() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    let first = conv.say(TYPO_MESSAGE).await;
    answer_yes(&h);
    conv.say("はい").await;
    let switched = conv.say("ADC-V724の設置方法を教えてください").await;
    log_turn("t4-switch-after-confirmation", &h, &switched);
    assert!(switched.case_reset);
    assert_ne!(switched.case_id, first.case_id);
}

#[tokio::test]
async fn t4_an_unknown_model_asked_about_again_without_answering_gets_the_out_of_scope_reply() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say("ADC-VC729の設定を教えてください").await;
    reply_type(&h, "unrelated");
    let again = conv.say("ADC-VC729の初期化方法は？").await;
    log_turn("t4-dismissed-out-of-scope", &h, &again);
    assert_eq!(again.reply_kind.as_deref(), Some("out_of_scope"));
    assert_eq!(
        again.reply_text,
        build_out_of_scope_reply("ADC-VC729", &f_p7_allowlist())
    );
}

#[tokio::test]
async fn t4_a_correction_to_an_unknown_model_resolves_the_pending_candidate_first() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say(TYPO_MESSAGE).await;

    answer_no(&h);
    let corrected = conv.say("いえ、ADC-VDB101です").await;
    log_turn("t4-correction-out-of-scope", &h, &corrected);
    assert_eq!(corrected.reply_kind.as_deref(), Some("out_of_scope"));
    assert_eq!(
        corrected.reply_text,
        build_out_of_scope_reply("ADC-VDB101", &f_p7_allowlist())
    );
    assert!(pending_of(&h, &corrected.case_id).is_empty());
    assert_eq!(
        denied_of(&h, &corrected.case_id),
        vec!["ADC-VC729P".to_string()]
    );

    answer_yes(&h);
    let later = conv.say("はい、わかりました").await;
    log_turn("t4-correction-out-of-scope", &h, &later);
    assert!(later
        .llm_requests_of(LlmKind::NameConfirmationReply)
        .is_empty());
    assert!(later.product_models.is_empty());
}

#[tokio::test]
async fn t4_a_retyped_notation_that_is_unsafe_to_show_is_not_asked_about() {
    let cases = [
        (f_p7_fixture(), "いいえ、ADC-VC8\u{200B}27です"),
        (f_name_fixture(), "いいえ、みまもりドー\u{200B}カムメラです"),
    ];
    for (fixture, correction) in cases {
        let h = ReplyHarness::start(fixture).await;
        h.llm().respond_classify(&[], true);
        let mut conv = h.conversation();
        conv.say(TYPO_MESSAGE).await;
        answer_no(&h);
        let turn = conv.say(correction).await;
        log_turn("t4-unsafe-retyped-notation", &h, &turn);
        assert!(
            pending_of(&h, &turn.case_id)
                .iter()
                .all(|(surface, _)| !surface.contains('\u{200B}')),
            "{correction}"
        );
        assert!(!turn.reply_text.contains('\u{200B}'), "{correction}");
        assert!(confirmation_requests(&turn).is_empty(), "{correction}");
        assert_eq!(
            denied_of(&h, &turn.case_id),
            vec!["ADC-VC729P".to_string()],
            "{correction}"
        );
    }
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let first = h
        .conversation()
        .say("ADV-VC7\u{200B}29Pを使っているが録画されない")
        .await;
    log_turn("t4-unsafe-first-notation", &h, &first);
    assert_ne!(first.reply_kind.as_deref(), Some("name_confirm"));
}

#[tokio::test]
async fn t4_a_case_switched_away_from_an_escalated_case_confirms_names_again() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    let escalated = conv.say("ADC-V724から焦げ臭いにおいがします").await;
    log_turn("t4-switch-from-escalated", &h, &escalated);
    assert_eq!(escalated.reply_kind.as_deref(), Some("escalation"));

    let switched = conv
        .say("ADC-V523とADV-VC729Pを使っているが録画されない")
        .await;
    log_turn("t4-switch-from-escalated", &h, &switched);
    assert!(switched.case_reset);
    assert_ne!(switched.case_id, escalated.case_id);
    assert_presented(&switched, "ADV-VC729P", &["ADC-VC729P"], &[]);
}
