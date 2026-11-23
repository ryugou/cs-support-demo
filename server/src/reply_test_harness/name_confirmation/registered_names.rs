use super::*;
use crate::reply_test_harness::PROJECT_ID;

#[tokio::test]
async fn t3_a_swapped_japanese_product_name_is_confirmed_and_the_talk_resumes() {
    let h = ReplyHarness::start(f_name_fixture()).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();

    let first = conv.say("みまもりドーカムメラが録画されません").await;
    log_turn("t3-product-name", &h, &first);
    assert_presented(
        &first,
        "みまもりドーカムメラ",
        &["みまもりドームカメラ"],
        &["おでかけパノラマカメラ"],
    );
    assert!(first.product_models.is_empty());

    answer_yes(&h);
    let second = conv.say("はい、そうです").await;
    log_turn("t3-product-name", &h, &second);
    assert_resumed(&second);
    assert_eq!(second.product_models, vec!["ADC-V724".to_string()]);
    assert!(
        hearing_requests(&second).iter().any(|request| request
            .user
            .contains("確認済みの製品: みまもりドームカメラ（型番 ADC-V724）")),
        "the hearing must know the confirmed product and its model: {}",
        second.log_line()
    );

    let third = conv.say("昨日から録画されていません").await;
    log_turn("t3-product-name", &h, &third);
    assert_resumed(&third);
}

#[tokio::test]
async fn t3_a_japanese_company_name_with_one_wrong_character_is_confirmed_once() {
    let h = ReplyHarness::start(f_name_fixture()).await;
    assert_eq!(
        register_companies(&h, F_ORG_FILE).await,
        CompanySyncSummary {
            active: 2,
            retired: 0
        }
    );
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();

    let first = conv
        .say("ミドリノ警備保証株式会社から購入したADC-V724が録画されません")
        .await;
    log_turn("t3-company", &h, &first);
    assert_presented(
        &first,
        "ミドリノ警備保証株式会社",
        &["ミドリノ警備保障株式会社"],
        &["ヒガシ電設工業株式会社"],
    );
    assert_eq!(first.product_models, vec!["ADC-V724".to_string()]);

    answer_yes(&h);
    let second = conv.say("はい、合っています").await;
    log_turn("t3-company", &h, &second);
    assert_resumed(&second);
    assert_eq!(
        confirmed_of(&h, &second.case_id),
        vec!["ミドリノ警備保障株式会社".to_string()]
    );
    assert_eq!(second.product_models, vec!["ADC-V724".to_string()]);

    let third = conv.say("夜だけ録画されません").await;
    log_turn("t3-company", &h, &third);
    assert_resumed(&third);
    assert_eq!(
        confirmed_of(&h, &third.case_id),
        vec!["ミドリノ警備保障株式会社".to_string()]
    );
}

#[tokio::test]
async fn t3_a_confirmed_company_name_does_not_identify_a_product() {
    let h = ReplyHarness::start(f_name_fixture()).await;
    register_companies(&h, F_ORG_FILE).await;
    h.llm().respond_classify(&[], true);
    let mut conv = h.conversation();

    let first = conv
        .say("ミドリノ警備保証株式会社から購入したカメラが録画されません")
        .await;
    log_turn("t3-company-only", &h, &first);
    assert_presented(
        &first,
        "ミドリノ警備保証株式会社",
        &["ミドリノ警備保障株式会社"],
        &[],
    );

    answer_yes(&h);
    let second = conv.say("はい").await;
    log_turn("t3-company-only", &h, &second);
    assert!(second.product_models.is_empty());
    assert_ne!(second.reply_kind.as_deref(), Some("answer"));
    let hearing = hearing_requests(&second);
    assert!(
        hearing.iter().any(|request| request
            .user
            .contains("確認済みの社名: ミドリノ警備保障株式会社")),
        "{:?}",
        hearing
            .iter()
            .map(|request| &request.user)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn t6_companies_registered_by_the_procedure_change_when_the_file_changes() {
    let h = ReplyHarness::start(f_name_fixture()).await;
    register_companies(&h, F_ORG_FILE).await;
    let updated = r#"[{"key": "midorino", "name": "ミドリノ防犯株式会社", "aliases": []}]"#;
    assert_eq!(
        register_companies(&h, updated).await,
        CompanySyncSummary {
            active: 1,
            retired: 1
        }
    );
    let stored: Vec<(String, String)> = h
        .store()
        .nodes_of_type(PROJECT_ID, "Company")
        .iter()
        .map(|node| {
            (
                node.attr("name").unwrap_or_default().to_string(),
                node.attr("status").unwrap_or_default().to_string(),
            )
        })
        .collect();
    println!("L2 case=t6-procedure stored={stored:?}");
    assert_eq!(
        stored,
        vec![
            ("ヒガシ電設工業株式会社".to_string(), "retired".to_string()),
            ("ミドリノ防犯株式会社".to_string(), "active".to_string()),
        ]
    );
    h.llm().respond_classify(&[], true);

    let old_name = h
        .conversation()
        .say("ミドリノ警備保証株式会社から購入したADC-V724が録画されません")
        .await;
    log_turn("t6-procedure", &h, &old_name);
    assert_ne!(old_name.reply_kind.as_deref(), Some("name_confirm"));

    let retired = h
        .conversation()
        .say("ヒガシ電設工業株式会杜から購入したADC-V724が録画されません")
        .await;
    log_turn("t6-procedure", &h, &retired);
    assert_ne!(retired.reply_kind.as_deref(), Some("name_confirm"));

    let renamed = h
        .conversation()
        .say("ミドリノ防犯株式会杜から購入したADC-V724が録画されません")
        .await;
    log_turn("t6-procedure", &h, &renamed);
    assert_presented(
        &renamed,
        "ミドリノ防犯株式会杜",
        &["ミドリノ防犯株式会社"],
        &[],
    );
}
