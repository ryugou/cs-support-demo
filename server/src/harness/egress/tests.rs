use super::*;

fn ng() -> NgDictionary {
    NgDictionary::from_json(
        r#"{ "block_terms": ["必ず治ります", "副作用はありません"],
             "abstain_terms": ["治る", "痩せる"],
             "customer_leak_terms": [] }"#,
    )
    .expect("ng dictionary parses")
}

fn operator() -> EmitContext {
    EmitContext {
        channel: EmitChannel::Operator,
    }
}

#[test]
fn explicit_ng_word_blocks() {
    match egress_gate(
        "この商品で必ず治りますのでご安心ください",
        &operator(),
        &ng(),
    ) {
        EgressVerdict::Block { term } => assert_eq!(term, "必ず治ります"),
        other => panic!("expected block, got {other:?}"),
    }
}

#[test]
fn implied_efficacy_abstains() {
    // 暗示効能は「検出して通す」でなく「疑わしきは出さない」（S1-4）
    match egress_gate("継続すると治ると言われています", &operator(), &ng()) {
        EgressVerdict::Abstain { term } => assert_eq!(term, "治る"),
        other => panic!("expected abstain, got {other:?}"),
    }
}

#[test]
fn block_takes_precedence_over_abstain() {
    assert!(matches!(
        egress_gate("必ず治りますし痩せます", &operator(), &ng()),
        EgressVerdict::Block { .. }
    ));
}

#[test]
fn clean_draft_passes() {
    assert!(matches!(
        egress_gate(
            "保存方法は直射日光を避けて常温で保管してください",
            &operator(),
            &ng()
        ),
        EgressVerdict::Pass
    ));
}

#[test]
fn gate_accepts_sentence_fragments() {
    // 入力単位を全文に固定しない（Step 3 は文単位で呼ぶ。遵守事項 2）
    assert!(matches!(
        egress_gate("治る", &operator(), &ng()),
        EgressVerdict::Abstain { .. }
    ));
    assert!(matches!(
        egress_gate("", &operator(), &ng()),
        EgressVerdict::Pass
    ));
}

#[test]
fn verdict_is_channel_invariant() {
    // 判定水準はチャネルで変えない（フェーズ不変条件）
    for channel in [
        EmitChannel::Operator,
        EmitChannel::CustomerChat,
        EmitChannel::CustomerVoice,
    ] {
        let ctx = EmitContext { channel };
        assert!(matches!(
            egress_gate("必ず治ります", &ctx, &ng()),
            EgressVerdict::Block { .. }
        ));
    }
}

fn ng_with_leak_terms() -> NgDictionary {
    NgDictionary::from_json(
        r#"{ "block_terms": ["必ず治ります"],
             "abstain_terms": ["治る"],
             "customer_leak_terms": ["資料に基づくと", "資料を"] }"#,
    )
    .expect("ng dictionary with leak terms parses")
}

#[test]
fn customer_leak_term_blocks() {
    match egress_gate(
        "資料に基づくと、保証期間は1年です",
        &operator(),
        &ng_with_leak_terms(),
    ) {
        EgressVerdict::Block { term } => assert_eq!(term, "資料に基づくと"),
        other => panic!("expected block, got {other:?}"),
    }
}

#[test]
fn near_hit_ignores_customer_leak_terms() {
    assert!(!ng_with_leak_terms().near_hit("資料を送ってください"));
}

#[test]
fn dictionary_without_customer_leak_terms_is_rejected() {
    assert!(NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":[]}"#).is_err());
}

#[test]
fn customer_leak_term_wins_over_abstain_term() {
    assert!(matches!(
        egress_gate(
            "資料に基づくと治ると言われています",
            &operator(),
            &ng_with_leak_terms()
        ),
        EgressVerdict::Block { .. }
    ));
}

#[test]
fn production_dictionary_blocks_source_citation_forms() {
    let ng = production_ng_dictionary();
    for text in [
        "社内資料を確認したところ、月額料金は税込1,100円です。",
        "資料の内容では、24時間録画に対応しています。",
        "資料に基づくと、保証期間は1年です。",
        "資料の内容に基づき、保証期間は1年です。",
        "マニュアルによると対応しています。",
        "その点は記載がありません。",
    ] {
        assert!(
            matches!(
                egress_gate(text, &operator(), &ng),
                EgressVerdict::Block { .. }
            ),
            "expected block for {text}"
        );
    }
}

#[test]
fn production_dictionary_allows_brochure_requests() {
    let ng = production_ng_dictionary();
    for text in [
        "資料請求はこちらからお願いします。",
        "資料をお送りします。",
        "資料の内容をご確認ください。",
        "資料の内容についてご不明な点はお問い合わせください。",
    ] {
        assert!(
            matches!(egress_gate(text, &operator(), &ng), EgressVerdict::Pass),
            "unexpected verdict for {text}"
        );
    }
}

#[test]
fn near_hit_detects_question_proximity() {
    assert!(ng().near_hit("これを飲むと治るのでしょうか"));
    assert!(!ng().near_hit("送料はいくらですか"));
}
