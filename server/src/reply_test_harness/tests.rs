//! 検査基盤自体のテスト。
//!
//! 基盤が「返信APIを、本番と同じ経路で、複数ターン、外部接続なしに駆動できる」ことを確認する。
//! 機能（タイポ候補など）の合否は、この基盤を使う各機能のテストが判定する。
use super::llm_stub::LlmKind;
use super::*;

/// 通常の質問（型番が取扱一覧と一致し、材料がある）を流すための共通フィクスチャ。
/// F-P7 の製品、本番のエスカレーションルール、ADC-V724 の SD カード節。
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

/// (a) 製品が特定された通常の質問が、`evaluate()` を通って 200 で返る。
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
    // 観測点: 会話ターンの reply_kind、監査イベントの decision、案件の product_models。
    assert_eq!(turn.reply_kind.as_deref(), Some("answer"));
    assert_eq!(turn.decision.as_deref(), Some("allowed:Manual"));
    assert_eq!(turn.product_models, vec!["ADC-V724".to_string()]);
    // evaluate() が実際に走った証拠: 3 種類の LLM 呼び出しがスタブに届いている。
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
    // 返信文の下書き要求に、材料（マニュアル節の本文）が渡っている。
    let reply_request = turn.llm_requests_of(LlmKind::Reply)[0];
    assert!(
        reply_request.user.contains("抜き差し"),
        "the draft request must carry the manual material: {}",
        reply_request.user
    );
    // 応答の本文はスタブが返した下書き。
    assert_eq!(turn.reply_text, "SDカードを抜き差ししてください。");
    // 案件の状態は代替に保存されている。
    let case = h
        .case_attributes(&turn.case_id)
        .expect("support_case must be stored");
    assert_eq!(case.attr("last_decision"), Some("allowed"));
    assert_eq!(h.conversation_turns(&turn.case_id).len(), 1);
}

/// (b) 2 ターン目に `case_id` と履歴を渡すと、同じ案件として状態が引き継がれる。
#[tokio::test]
async fn second_turn_with_case_id_and_history_continues_the_same_case() {
    let h = ReplyHarness::start(fixture()).await;
    h.llm().respond_classify(&["sd_not_recognized"], true);
    h.llm()
        .respond_text(LlmKind::Reply, "SDカードを抜き差ししてください。");

    let mut conv = h.conversation();
    let first = conv.say("ADC-V724でSDカードが認識されません").await;
    // 型番を含まない 2 ターン目。1 ターン目で確定した型番が案件に残っているので、製品未特定にならない。
    let second = conv
        .say("SDカードが認識されません。フォーマットすればよいですか")
        .await;
    println!("{}\n{}", first.log_line(), second.log_line());

    assert_eq!(second.status, StatusCode::OK, "{:?}", second.error_body);
    // 同じ案件: case_id が同じ。切り替え（case_reset）は起きない。
    assert_eq!(second.case_id, first.case_id);
    assert!(!second.case_reset);
    // 状態の引き継ぎ: 確定した型番が残り、会話ターンが 2 件積まれる（seq 昇順）。
    assert_eq!(second.product_models, vec!["ADC-V724".to_string()]);
    let turns = h.conversation_turns(&first.case_id);
    let seqs: Vec<_> = turns.iter().filter_map(|t| t.attr("seq")).collect();
    assert_eq!(seqs, ["1", "2"]);
    assert_eq!(
        turns[1].attr("question"),
        Some("SDカードが認識されません。フォーマットすればよいですか")
    );
    assert_eq!(second.reply_kind.as_deref(), Some("answer"));
    // 履歴を渡した証拠: 2 ターン目の signal 抽出の依頼に、1 ターン目の顧客発話が「過去の
    // 顧客発話」として入っている。
    let classify = second.llm_requests_of(LlmKind::Classify)[0];
    assert!(
        classify.user.contains("ADC-V724でSDカードが認識されません"),
        "the history must reach the classifier: {}",
        classify.user
    );
    // ドライバの履歴は LINE アダプタと同じ形（顧客発話 → 応答、昇順）。
    assert_eq!(
        conv.history(),
        vec![
            ("customer", first.message.clone()),
            ("assistant", first.reply_text.clone()),
            ("customer", second.message.clone()),
            ("assistant", second.reply_text.clone()),
        ]
    );
    // 比較対象: case_id を渡さない別の会話は、別の案件になる。
    let other = h
        .conversation()
        .say("SDカードが認識されません。フォーマットすればよいですか")
        .await;
    assert_ne!(other.case_id, first.case_id);
}

/// (c) 取扱外の型番に、定型応答が返る。evaluate() には到達しない（LLM は呼ばれない）。
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
    // evaluate() にも LLM にも到達しない。
    assert!(
        turn.llm_requests.is_empty(),
        "no LLM call is expected: {:?}",
        turn.llm_requests.iter().map(|r| r.kind).collect::<Vec<_>>()
    );
    assert!(turn.decision.is_some(), "the out-of-scope turn is audited");
    assert!(turn.product_models.is_empty());
}

/// 製品切り替え（#61）で `case_reset: true` が返ると、ドライバは LINE アダプタと同じく履歴を
/// 捨て、そのターンの顧客発話と応答から作り直す。次のターンは新しい案件に続く。
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
    // 旧案件は変更されない。
    assert_eq!(
        h.product_models(&first.case_id),
        vec!["ADC-V724".to_string()]
    );
    // 履歴は切り替えターンの顧客発話と応答だけ。case_id は新しい案件。
    assert_eq!(
        conv.history(),
        vec![
            ("customer", switched.message.clone()),
            ("assistant", switched.reply_text.clone()),
        ]
    );
    assert_eq!(conv.case_id(), Some(switched.case_id.as_str()));
}

/// 基盤が外部へ出ないことの確認: 認証が無い要求は 401 になり、LLM にも vegapunk 代替にも
/// 届かない。ルートは `api_router` の HTTP ルートである。
#[tokio::test]
async fn the_entry_point_is_the_api_router_http_route() {
    let h = ReplyHarness::start(fixture()).await;
    let request = Request::builder()
        .method("POST")
        .uri(format!("/{PROJECT_ID}/api/reply"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"message":"こんにちは"}"#))
        .unwrap();
    let response = api_router(h.state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(h.llm().request_count(), 0);
}

/// 基準ログ（検査設計 §9「86250e3 の基準ログ」）。報告された会話を現行のコードで流し、各ターンの
/// 観測点を出す。機能の実装後は挙動が変わるため、現行の不具合を固定する恒久テストにはせず
/// `#[ignore]` にして、ログを取るときだけ実行する。
///
/// ```sh
/// cargo test --lib reply_test_harness::tests::baseline_reported_conversation -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "baseline log for #153 at 86250e3; prints the observations of the reported conversation"]
async fn baseline_reported_conversation() {
    let h = ReplyHarness::start(Fixture::new().products_f_p7().bundled_rules()).await;
    // スタブ LLM: 現行の実 LLM と同じく、型番が取扱一覧に無い（タイポ）ので製品参照は空、
    // 症状の質問なので製品依存（product_dependent = true）。
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

/// フィクスチャ: 製品名・別名つきの製品（F-NAME）と、任意の種類のノード（社名など）を投入でき、
/// 本番の読み出し処理（取扱一覧の組み立て）と、テストからの読み出しの両方で使える。
#[tokio::test]
async fn fixture_accepts_products_with_names_aliases_and_arbitrary_nodes() {
    let h = ReplyHarness::start(Fixture::new().products_f_name().bundled_rules().node(
        "Organization",
        "midorino",
        &[("name", "ミドリノ警備保障株式会社")],
    ))
    .await;
    h.llm().respond_classify(&[], true);

    // テストから読める: 本番の投入処理と同じ形で保存されている。
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

    // 本番の読み出しに届く: 取扱製品の一覧が、製品参照抽出の依頼（system prompt）に入る。
    let turn = h.conversation().say("録画されません").await;
    assert_eq!(turn.status, StatusCode::OK, "{:?}", turn.error_body);
    assert_eq!(turn.reply_kind.as_deref(), Some("clarify"));
    // 製品名は取扱一覧の表記（型番（製品名））で、聞き返しの依頼に入る。
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
    // 応答の長さ上限は本番設定（config.cloudrun.toml）のまま。
    let reply_requests = h.llm().requests_of(LlmKind::Reply);
    assert!(reply_requests.iter().all(|r| r.max_tokens == 1500));
}
