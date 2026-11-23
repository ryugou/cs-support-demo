use super::*;
use crate::harness::clarify::FALLBACK_CLARIFY_TEXT;
use crate::harness::product_gate::{build_out_of_scope_reply, ProductAllowlist};

async fn first_turn(message: &str, signals: &[&str]) -> (ReplyHarness, Turn) {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(signals, true);
    let turn = h.conversation().say(message).await;
    log_turn("t5", &h, &turn);
    (h, turn)
}

fn decision_layer(turn: &Turn) -> Option<String> {
    turn.decision
        .as_deref()
        .and_then(|decision| decision.split(':').nth(1))
        .map(ToString::to_string)
}

#[tokio::test]
async fn t5_an_unknown_model_with_a_candidate_is_confirmed_before_the_out_of_scope_reply() {
    let (h, first) = first_turn("ADC-VC729の設定を教えてください", &[]).await;
    assert_presented(&first, "ADC-VC729", &["ADC-VC729P"], &[]);
    assert_ne!(first.reply_kind.as_deref(), Some("out_of_scope"));
    assert_eq!(
        first.reply_text,
        "（スタブ）ADC-VC729 は ADC-VC729P のことでしょうか？"
    );
    assert_ne!(first.reply_text, FALLBACK_CLARIFY_TEXT);

    answer_yes(&h);
    let mut conv = h.conversation();
    conv.say("ADC-VC729の設定を教えてください").await;
    let confirmed = conv.say("はい").await;
    log_turn("t5-1", &h, &confirmed);
    assert_resumed(&confirmed);
    assert_eq!(confirmed.product_models, vec!["ADC-VC729P".to_string()]);
}

#[tokio::test]
async fn t5_safety_handoffs_are_not_delayed_by_a_name_confirmation() {
    let cases = [
        (
            "ADV-VC729Pから焦げ臭いにおいがして熱い",
            "ADC-VC729Pから焦げ臭いにおいがして熱い",
        ),
        (
            "ADC-VC729から煙が出ています",
            "ADC-VC729Pから煙が出ています",
        ),
        (
            "ADV-VC729Pの録画を証拠として警察に提出したい",
            "ADC-VC729Pの録画を証拠として警察に提出したい",
        ),
        (
            "ADV-VC729Pの件で担当者につないでください",
            "ADC-VC729Pの件で担当者につないでください",
        ),
    ];
    for (typo, written) in cases {
        let (h, turn) = first_turn(typo, &[]).await;
        let (_, reference) = first_turn(written, &[]).await;
        assert_eq!(turn.reply_kind.as_deref(), Some("escalation"), "{typo}");
        assert_eq!(turn.reply_kind, reference.reply_kind, "{typo}");
        assert_eq!(decision_layer(&turn), decision_layer(&reference), "{typo}");
        assert!(confirmation_requests(&turn).is_empty(), "{typo}");
        assert!(pending_of(&h, &turn.case_id).is_empty(), "{typo}");
    }
}

#[tokio::test]
async fn t5_an_unknown_model_without_a_candidate_keeps_the_current_reply() {
    let (_, turn) = first_turn("ADC-VDB101から煙が出ています", &[]).await;
    assert_eq!(turn.reply_kind.as_deref(), Some("out_of_scope"));
    assert_eq!(
        turn.reply_text,
        build_out_of_scope_reply(
            "ADC-VDB101",
            &ProductAllowlist::from_models(
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
                .to_vec()
            )
        )
    );
}

#[tokio::test]
async fn t5_a_typo_the_llm_calls_foreign_is_still_confirmed() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    h.llm().respond_text(
        LlmKind::ProductReferences,
        r#"{"product_references":[{"surface":"ADV-VC729P","resolution":"foreign","matched_model":null}]}"#,
    );
    let turn = h
        .conversation()
        .say("ADV-VC729Pを使っているが録画されない")
        .await;
    log_turn("t5-second-stage", &h, &turn);
    assert_presented(&turn, "ADV-VC729P", &["ADC-VC729P"], &[]);

    let safety = h
        .conversation()
        .say("ADV-VC729Pから焦げ臭いにおいがして熱い")
        .await;
    log_turn("t5-second-stage-safety", &h, &safety);
    assert_eq!(safety.reply_kind.as_deref(), Some("escalation"));
}

#[tokio::test]
async fn t5_a_typo_the_llm_calls_matched_is_not_identified_without_confirmation() {
    let h = ReplyHarness::start(f_p7_fixture()).await;
    h.llm().respond_classify(&[], true);
    h.llm().respond_text(
        LlmKind::ProductReferences,
        r#"{"product_references":[{"surface":"ADV-VC729P","resolution":"matched","matched_model":"ADC-VC729P"}]}"#,
    );
    let turn = h
        .conversation()
        .say("ADV-VC729Pを使っているが録画されない")
        .await;
    log_turn("t5-llm-matched", &h, &turn);
    assert_presented(&turn, "ADV-VC729P", &["ADC-VC729P"], &[]);
    assert!(turn.product_models.is_empty());
}
