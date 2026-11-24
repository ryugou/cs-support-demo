use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::llm_stub::LlmKind;
use super::*;
use crate::api::api_router;

fn fixture() -> Fixture {
    Fixture::new()
        .products_f_p7()
        .bundled_rules()
        .manual_section(
            "sec-sd-not-recognized",
            "SDカードが認識されない",
            "SDカードを一度抜き差ししてください。それでも認識されない場合は、SDカードを\
             カメラ本体でフォーマットしてください。",
            &["ADC-V724"],
            &["sd_not_recognized"],
        )
}

#[tokio::test]
async fn identified_product_question_goes_through_evaluate_and_returns_200() {
    let h = ReplyHarness::start(fixture()).await;
    h.llm().respond_classify(&["sd_not_recognized"], true);
    h.llm()
        .respond_text(LlmKind::Reply, "SDカードを抜き差ししてください。");

    let mut conv = h.conversation();
    let turn = conv.say("ADC-V724でSDカードが認識されません").await;
    println!("{}", turn.log_line());

    assert_eq!(turn.status, StatusCode::OK, "{:?}", turn.error_body);
    assert!(!turn.case_id.is_empty());
    assert!(!turn.case_reset);
    assert_eq!(turn.reply_kind.as_deref(), Some("answer"));
    assert_eq!(turn.decision.as_deref(), Some("allowed:Manual"));
    assert_eq!(turn.product_models, vec!["ADC-V724".to_string()]);
    for kind in [
        LlmKind::Classify,
        LlmKind::ProductReferences,
        LlmKind::Reply,
    ] {
        assert!(
            !turn.llm_requests_of(kind).is_empty(),
            "{kind:?} request must reach the LLM stub; got {:?}",
            turn.llm_requests.iter().map(|r| r.kind).collect::<Vec<_>>()
        );
    }
    let reply_request = turn.llm_requests_of(LlmKind::Reply)[0];
    assert!(
        reply_request.user.contains("抜き差し"),
        "the draft request must carry the manual material: {}",
        reply_request.user
    );
    assert_eq!(turn.reply_text, "SDカードを抜き差ししてください。");
    let case = h
        .case_attributes(&turn.case_id)
        .expect("support_case must be stored");
    assert_eq!(case.attr("last_decision"), Some("allowed"));
    assert_eq!(h.conversation_turns(&turn.case_id).len(), 1);
}

#[tokio::test]
async fn second_turn_with_case_id_and_history_continues_the_same_case() {
    let h = ReplyHarness::start(fixture()).await;
    h.llm().respond_classify(&["sd_not_recognized"], true);
    h.llm()
        .respond_text(LlmKind::Reply, "SDカードを抜き差ししてください。");

    let mut conv = h.conversation();
    let first = conv.say("ADC-V724でSDカードが認識されません").await;
    let second = conv
        .say("SDカードが認識されません。フォーマットすればよいですか")
        .await;
    println!("{}\n{}", first.log_line(), second.log_line());

    assert_eq!(second.status, StatusCode::OK, "{:?}", second.error_body);
    assert_eq!(second.case_id, first.case_id);
    assert!(!second.case_reset);
    assert_eq!(second.product_models, vec!["ADC-V724".to_string()]);
    let turns = h.conversation_turns(&first.case_id);
    let seqs: Vec<_> = turns.iter().filter_map(|t| t.attr("seq")).collect();
    assert_eq!(seqs, ["1", "2"]);
    assert_eq!(
        turns[1].attr("question"),
        Some("SDカードが認識されません。フォーマットすればよいですか")
    );
    assert_eq!(second.reply_kind.as_deref(), Some("answer"));
    let classify = second.llm_requests_of(LlmKind::Classify)[0];
    assert!(
        classify.user.contains("ADC-V724でSDカードが認識されません"),
        "the history must reach the classifier: {}",
        classify.user
    );
    assert_eq!(
        conv.history(),
        vec![
            ("customer", first.message.clone()),
            ("assistant", first.reply_text.clone()),
            ("customer", second.message.clone()),
            ("assistant", second.reply_text.clone()),
        ]
    );
    let other = h
        .conversation()
        .say("SDカードが認識されません。フォーマットすればよいですか")
        .await;
    assert_ne!(other.case_id, first.case_id);
}

#[tokio::test]
async fn out_of_scope_model_gets_the_fixed_reply_without_reaching_evaluate() {
    let h = ReplyHarness::start(fixture()).await;

    let mut conv = h.conversation();
    let turn = conv.say("ADC-V999が動きません").await;
    println!("{}", turn.log_line());

    assert_eq!(turn.status, StatusCode::OK, "{:?}", turn.error_body);
    assert_eq!(turn.reply_kind.as_deref(), Some("out_of_scope"));
    assert!(
        turn.reply_text.contains("ADC-V999"),
        "the fixed reply names the unsupported model: {}",
        turn.reply_text
    );
    assert!(
        turn.reply_text.contains("ADC-V724"),
        "the fixed reply lists the supported models: {}",
        turn.reply_text
    );
    assert!(
        turn.llm_requests.is_empty(),
        "no LLM call is expected: {:?}",
        turn.llm_requests.iter().map(|r| r.kind).collect::<Vec<_>>()
    );
    assert!(turn.decision.is_some(), "the out-of-scope turn is audited");
    assert!(turn.product_models.is_empty());
}

#[tokio::test]
async fn driver_follows_case_reset_like_the_line_adapter() {
    let h = ReplyHarness::start(fixture()).await;
    h.llm().respond_classify(&["sd_not_recognized"], true);

    let mut conv = h.conversation();
    let first = conv.say("ADC-V724でSDカードが認識されません").await;
    let switched = conv.say("ADC-V523のSDカードが認識されません").await;
    println!("{}\n{}", first.log_line(), switched.log_line());

    assert_eq!(switched.status, StatusCode::OK, "{:?}", switched.error_body);
    assert!(
        switched.case_reset,
        "a different supported model resets the case"
    );
    assert_ne!(switched.case_id, first.case_id);
    assert_eq!(switched.product_models, vec!["ADC-V523".to_string()]);
    assert_eq!(
        h.product_models(&first.case_id),
        vec!["ADC-V724".to_string()]
    );
    assert_eq!(
        conv.history(),
        vec![
            ("customer", switched.message.clone()),
            ("assistant", switched.reply_text.clone()),
        ]
    );
    assert_eq!(conv.case_id(), Some(switched.case_id.as_str()));
}

#[tokio::test]
async fn the_entry_point_is_the_api_router_http_route() {
    let h = ReplyHarness::start(fixture()).await;
    let request = Request::builder()
        .method("POST")
        .uri(format!("/{PROJECT_ID}/api/reply"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"message":"こんにちは"}"#))
        .unwrap();
    let response = api_router(h.api_state()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(h.llm().request_count(), 0);
}

#[tokio::test]
#[ignore = "baseline log for #153 at 86250e3; prints the observations of the reported conversation"]
async fn baseline_reported_conversation() {
    let h = ReplyHarness::start(Fixture::new().products_f_p7().bundled_rules()).await;
    h.llm().respond_classify(&[], true);

    let mut conv = h.conversation();
    for message in [
        "ADV-VC729Pをつかっているが、録画されなき",
        "全く録画されていない、型番は最初に伝えている",
    ] {
        let turn = conv.say(message).await;
        println!("BASELINE {}", turn.log_line());
        println!(
            "BASELINE turn={} llm_requests={:?} history_sent={}",
            turn.index,
            turn.llm_requests.iter().map(|r| r.kind).collect::<Vec<_>>(),
            conv.history().len(),
        );
        for request in turn.llm_requests_of(LlmKind::ProductIdentification) {
            println!(
                "BASELINE turn={} product_identification_request.user={:?}",
                turn.index, request.user
            );
        }
        println!(
            "BASELINE turn={} conv_state={:?}",
            turn.index,
            h.conv_state(&turn.case_id).await
        );
    }
}

#[tokio::test]
async fn fixture_accepts_products_with_names_aliases_and_arbitrary_nodes() {
    let h = ReplyHarness::start(Fixture::new().products_f_name().bundled_rules().node(
        "Organization",
        "midorino",
        &[("name", "ミドリノ警備保障株式会社")],
    ))
    .await;
    h.llm().respond_classify(&[], true);

    let products = h.store().nodes_of_type(PROJECT_ID, "Product");
    assert_eq!(products.len(), 7);
    let dome = products
        .iter()
        .find(|p| p.attr("model") == Some("ADC-V724"))
        .expect("ADC-V724 is seeded");
    assert_eq!(dome.attr("name"), Some("みまもりドームカメラ"));
    assert_eq!(dome.attr("aliases"), Some("見守りドーム"));
    let orgs = h.store().nodes_of_type(PROJECT_ID, "Organization");
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].attr("name"), Some("ミドリノ警備保障株式会社"));

    let turn = h.conversation().say("録画されません").await;
    assert_eq!(turn.status, StatusCode::OK, "{:?}", turn.error_body);
    assert_eq!(turn.reply_kind.as_deref(), Some("clarify"));
    let ident = turn.llm_requests_of(LlmKind::ProductIdentification);
    assert_eq!(ident.len(), 1);
    assert!(
        ident[0].user.contains("ADC-V724（みまもりドームカメラ）"),
        "{}",
        ident[0].user
    );
    let requests = h.llm().requests_of(LlmKind::ProductReferences);
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].system.contains("ADC-V523X") || requests[0].system.contains("ADC-VC729P"),
        "the catalog from the fixture must reach the extraction prompt: {}",
        requests[0].system
    );
    let reply_requests = h.llm().requests_of(LlmKind::Reply);
    assert!(reply_requests.iter().all(|r| r.max_tokens == 1500));
}
