use axum::http::StatusCode;

use super::*;
use crate::reply_test_harness::llm_stub::default_responder;
use crate::reply_test_harness::PROJECT_ID;

const TYPO_MESSAGE: &str = "ADV-VC729Pを使っているが録画されない";

#[tokio::test]
async fn a_classification_failure_dismisses_the_confirmation_and_the_talk_goes_on() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say(TYPO_MESSAGE).await;

    h.llm()
        .respond_text(LlmKind::NameConfirmationReply, "分類できませんでした");
    let turn = conv.say("はい、全く録画されていません").await;
    log_turn("f10-classification-failure", &h, &turn);
    assert_eq!(turn.status, StatusCode::OK, "{:?}", turn.error_body);
    assert_eq!(
        turn.llm_requests_of(LlmKind::NameConfirmationReply).len(),
        1
    );
    assert!(pending_of(&h, &turn.case_id).is_empty());
    assert_eq!(
        name_state(&h, &turn.case_id)["dismissed"],
        serde_json::json!(["advvc729p"])
    );
    assert_ne!(turn.reply_kind.as_deref(), Some("name_confirm"));
    assert!(confirmation_requests(&turn).is_empty());
    assert!(turn.product_models.is_empty());
}

#[tokio::test]
async fn an_audit_failure_still_returns_the_confirmation_with_an_empty_audit_event_id() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let audit_log = h.audit_log();
    h.llm()
        .respond_with(LlmKind::NameConfirmation, move |request: &LlmRequest| {
            audit_log.poison_for_test();
            default_responder(request)
        });
    let turn = h.conversation().say(TYPO_MESSAGE).await;
    log_turn("f10-audit-failure", &h, &turn);
    assert_eq!(turn.status, StatusCode::OK, "{:?}", turn.error_body);
    assert_eq!(turn.reply_kind.as_deref(), Some("name_confirm"));
    assert_eq!(
        h.conversation_turns(&turn.case_id)
            .last()
            .and_then(|recorded| recorded.attr("audit_event_id").map(ToString::to_string)),
        Some(String::new())
    );
    assert_eq!(turn.decision, None);
    assert_eq!(
        pending_of(&h, &turn.case_id),
        vec![("ADV-VC729P".to_string(), vec!["ADC-VC729P".to_string()])]
    );
}

#[tokio::test]
async fn a_malformed_confirmation_state_is_an_internal_error() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    let first = conv.say(TYPO_MESSAGE).await;
    let mut case = h
        .case_attributes(&first.case_id)
        .expect("the confirmation turn stores the case");
    case.attributes.insert(
        "name_confirmation".to_string(),
        "{\"pending\": 1}".to_string(),
    );
    h.store()
        .upsert_node(&case.node_id, &case.node_type, case.attributes.clone());

    let turn = conv.say("はい").await;
    println!("L2 case=f10-malformed-state {}", turn.log_line());
    assert_eq!(turn.status, StatusCode::INTERNAL_SERVER_ERROR);
    let body = turn.error_body.unwrap_or_default();
    assert!(body.contains("\"internal\""), "{body}");
    assert!(
        body.contains("failed to process the name confirmation"),
        "{body}"
    );
}

#[tokio::test]
async fn an_unavailable_case_store_is_an_upstream_error() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();
    conv.say(TYPO_MESSAGE).await;
    h.store().make_queries_unavailable_for("support_case");

    let turn = conv.say("はい").await;
    println!("L2 case=f10-unavailable-case {}", turn.log_line());
    assert_eq!(turn.status, StatusCode::SERVICE_UNAVAILABLE);
    let body = turn.error_body.unwrap_or_default();
    assert!(body.contains("\"upstream_unavailable\""), "{body}");
    assert!(
        body.contains("failed to process the name confirmation"),
        "{body}"
    );
}

#[tokio::test]
async fn unavailable_company_names_leave_products_and_the_reply_working() {
    let h = ReplyHarness::start(f_name_fixture()).await;
    register_companies(&h, F_ORG_FILE).await;
    h.store().make_queries_unavailable_for("Company");
    h.llm().respond_classify(&[], true);

    let product_typo = h.conversation().say(TYPO_MESSAGE).await;
    log_turn("f5-company-unavailable", &h, &product_typo);
    assert_presented(&product_typo, "ADV-VC729P", &["ADC-VC729P"], &[]);

    let company_typo = h
        .conversation()
        .say("ミドリノ警備保証株式会社から購入したADC-V724が録画されません")
        .await;
    log_turn("f5-company-unavailable", &h, &company_typo);
    assert_eq!(company_typo.status, StatusCode::OK);
    assert_ne!(company_typo.reply_kind.as_deref(), Some("name_confirm"));
    assert_eq!(company_typo.product_models, vec!["ADC-V724".to_string()]);
}

#[tokio::test]
async fn the_product_allowlist_does_not_read_company_names() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.store().make_queries_unavailable_for("Company");
    let allowlist = h
        .harness()
        .product_allowlist(PROJECT_ID)
        .await
        .expect("the product allowlist does not depend on company names");
    assert!(allowlist.canonical_model("ADC-VC729P").is_some());
    assert!(!h
        .store()
        .queried_node_types()
        .contains(&"Company".to_string()));
    assert!(h
        .harness()
        .registered_companies(PROJECT_ID)
        .await
        .is_empty());
    assert!(h
        .store()
        .queried_node_types()
        .contains(&"Company".to_string()));
}
