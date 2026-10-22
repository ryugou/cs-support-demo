use crate::harness::extraction;
use crate::harness::tests::{
    contract_test_kr, contract_test_signals, kr_ctx, kr_selection_harness, run_kr_turn,
    KrStubJudge, KrTurnCase, CUSTOMER_TEXT, HYBRID,
};
use crate::test_support::{capture_logs_async, filter_warn_and_error_lines};

#[tokio::test]
async fn kr_guards_select_no_kr_and_skip_the_judge() {
    struct Guard {
        name: &'static str,
        judge: bool,
        turn: &'static [&'static str],
        signals: &'static [&'static str],
        escalated: bool,
        mode: extraction::ExtractionMode,
        judge_calls: usize,
    }
    let guards = [
        Guard {
            name: "hazard signal in the accumulated set",
            judge: true,
            turn: &["hz", "a"],
            signals: &["hz", "a"],
            escalated: false,
            mode: HYBRID,
            judge_calls: 0,
        },
        Guard {
            name: "lexicon fallback",
            judge: true,
            turn: &["a"],
            signals: &["a"],
            escalated: false,
            mode: extraction::ExtractionMode::LexiconFallback,
            judge_calls: 0,
        },
        Guard {
            name: "no judge configured",
            judge: false,
            turn: &["a"],
            signals: &["a"],
            escalated: false,
            mode: HYBRID,
            judge_calls: 0,
        },
        Guard {
            name: "hazard signal from a previous turn while escalated",
            judge: true,
            turn: &["a"],
            signals: &["hz", "a"],
            escalated: true,
            mode: HYBRID,
            judge_calls: 0,
        },
        Guard {
            name: "no candidate after escalation",
            judge: true,
            turn: &["b"],
            signals: &["a", "b"],
            escalated: true,
            mode: HYBRID,
            judge_calls: 0,
        },
    ];
    for guard in guards {
        let judge = KrStubJudge::selecting(&["kr1"]);
        let harness = kr_selection_harness(guard.judge.then(|| judge.clone()), &[]);
        let mut case = KrTurnCase::new(guard.turn, guard.signals);
        case.escalated = guard.escalated;
        case.mode = guard.mode;
        let (selected, _) = run_kr_turn(&harness, &case, &[contract_test_kr("kr1", &["a"])]).await;
        assert_eq!(selected, None, "{}", guard.name);
        assert_eq!(judge.calls(), guard.judge_calls, "{}", guard.name);
    }
}

#[tokio::test]
async fn kr_candidates_use_the_decide_signal_set_before_escalation() {
    let judge = KrStubJudge::selecting(&["kr_ab"]);
    let harness = kr_selection_harness(Some(judge.clone()), &[]);
    let krs = [contract_test_kr("kr_ab", &["a", "b"])];
    let case = KrTurnCase::new(&["b"], &["a", "b"]);
    let (selected, _) = run_kr_turn(&harness, &case, &krs).await;
    assert_eq!(selected.as_deref(), Some("kr_ab"));
}

#[tokio::test]
async fn kr_candidates_use_the_decide_signal_set_after_escalation() {
    let judge = KrStubJudge::selecting(&["kr_ab", "kr_b"]);
    let harness = kr_selection_harness(Some(judge.clone()), &[]);
    let krs = [
        contract_test_kr("kr_ab", &["a", "b"]),
        contract_test_kr("kr_b", &["b"]),
    ];
    let mut case = KrTurnCase::new(&["b"], &["a", "b"]);
    case.escalated = true;
    let (selected, _) = run_kr_turn(&harness, &case, &krs).await;
    assert_eq!(selected.as_deref(), Some("kr_b"));
    assert!(judge.offered_to("kr_b"));
    assert!(
        !judge.offered_to("kr_ab"),
        "a KR needing a signal from a previous turn must not be offered once escalated"
    );
}

#[tokio::test]
async fn kr_judge_receives_the_context_text_not_the_question() {
    let judge = KrStubJudge::selecting(&["kr1"]);
    let harness = kr_selection_harness(Some(judge.clone()), &[]);
    let mut case = KrTurnCase::new(&["a"], &["a"]);
    case.context_text = "文脈テキストXYZ";
    case.question = "質問だけの文ABC";
    run_kr_turn(&harness, &case, &[contract_test_kr("kr1", &["a"])]).await;
    let seen = judge.offered.lock().expect("offered lock");
    assert!(seen[0].contains("文脈テキストXYZ"));
    assert!(!seen[0].contains("質問だけの文ABC"));
}

#[tokio::test]
async fn llm_failure_selects_no_kr_and_warns_without_the_customer_text() {
    let judge = KrStubJudge::new(Err("boom"));
    let harness = kr_selection_harness(Some(judge.clone()), &[]);
    let case = KrTurnCase::new(&["a"], &["a"]);
    let krs = [contract_test_kr("kr1", &["a"])];
    let ((selected, _), logs) =
        crate::test_support::capture_logs_async(run_kr_turn(&harness, &case, &krs)).await;
    assert_eq!(selected, None);
    assert_eq!(judge.calls(), 1);
    let warns = crate::test_support::filter_warn_and_error_lines(&logs);
    assert!(warns.contains("req-register-test"), "{warns}");
    assert!(warns.contains("boom"), "{warns}");
    assert!(!warns.contains(CUSTOMER_TEXT), "{warns}");
}

#[tokio::test]
async fn lexicon_fallback_warns_without_the_customer_text() {
    let judge = KrStubJudge::selecting(&["kr1"]);
    let harness = kr_selection_harness(Some(judge), &[]);
    let mut case = KrTurnCase::new(&["a"], &["a"]);
    case.mode = extraction::ExtractionMode::LexiconFallback;
    let krs = [contract_test_kr("kr1", &["a"])];
    let (_, logs) =
        crate::test_support::capture_logs_async(run_kr_turn(&harness, &case, &krs)).await;
    let warns = crate::test_support::filter_warn_and_error_lines(&logs);
    assert!(warns.contains("req-register-test"), "{warns}");
    assert!(warns.contains("fell back to the lexicon"), "{warns}");
    assert!(!warns.contains(CUSTOMER_TEXT), "{warns}");
}

#[tokio::test]
async fn malformed_judge_output_selects_no_kr_and_warns_without_the_customer_text() {
    let echoed = format!(r#"{{"applicable_ids":"{CUSTOMER_TEXT}"}}"#);
    let judge = KrStubJudge::new(Ok(&echoed));
    let harness = kr_selection_harness(Some(judge.clone()), &[]);
    let case = KrTurnCase::new(&["a"], &["a"]);
    let krs = [contract_test_kr("kr1", &["a"])];
    let ((selected, _), logs) =
        crate::test_support::capture_logs_async(run_kr_turn(&harness, &case, &krs)).await;
    assert_eq!(selected, None);
    assert_eq!(judge.calls(), 1);
    let warns = crate::test_support::filter_warn_and_error_lines(&logs);
    assert!(warns.contains("req-register-test"), "{warns}");
    assert!(warns.contains("invalid kr applicability output"), "{warns}");
    assert!(!warns.contains(CUSTOMER_TEXT), "{warns}");
}

#[test]
fn unmatched_excluded_ids_are_reported_once_per_harness() {
    let harness = kr_selection_harness(None, &["kr1", "typo", "other"]);
    let resolutions = [contract_test_kr("kr1", &["a"])];
    assert_eq!(
        harness.newly_unmatched_excluded_ids(&resolutions),
        vec!["typo".to_string(), "other".to_string()]
    );
    assert!(harness
        .newly_unmatched_excluded_ids(&resolutions)
        .is_empty());

    let fresh = kr_selection_harness(None, &["typo"]);
    assert_eq!(
        fresh.newly_unmatched_excluded_ids(&resolutions),
        vec!["typo".to_string()]
    );
}

#[test]
fn a_matched_excluded_id_is_not_recorded_so_it_is_reported_once_it_stops_matching() {
    let harness = kr_selection_harness(None, &["typo"]);
    assert!(harness
        .newly_unmatched_excluded_ids(&[contract_test_kr("typo", &["a"])])
        .is_empty());
    assert_eq!(
        harness.newly_unmatched_excluded_ids(&[contract_test_kr("kr1", &["a"])]),
        vec!["typo".to_string()]
    );
}

#[tokio::test]
async fn an_unmatched_excluded_id_warns_with_its_kr_id_on_the_first_turn_only() {
    let harness = kr_selection_harness(None, &["typo-kr"]);
    let case = KrTurnCase::new(&["a"], &["a"]);
    let resolutions = [contract_test_kr("kr1", &["a"])];
    let (_, first) =
        capture_logs_async(async { run_kr_turn(&harness, &case, &resolutions).await }).await;
    let (_, second) =
        capture_logs_async(async { run_kr_turn(&harness, &case, &resolutions).await }).await;
    let first = filter_warn_and_error_lines(&first);
    assert!(first.contains("kr_id=typo-kr"), "{first}");
    assert!(first.contains("matches no known resolution"), "{first}");
    assert_eq!(filter_warn_and_error_lines(&second), "");
}

#[tokio::test]
async fn a_failed_manual_search_still_runs_the_kr_judge_once_and_returns_the_error() {
    let judge = KrStubJudge::selecting(&["kr1"]);
    let harness = kr_selection_harness(Some(judge.clone()), &[]);
    let tools = crate::test_support::test_tool_service();
    let snapshot = crate::proto::graphrag::GetGraphSnapshotResponse::default();
    let signals = contract_test_signals(&["a"]);
    let accumulated = contract_test_signals(&["a"]);
    let policy = crate::harness::ReplyDraftPolicy::Always;
    let result = harness
        .search_manual_and_prepare_kr_turn(
            &kr_ctx(),
            CUSTOMER_TEXT,
            &accumulated,
            None,
            None,
            &snapshot,
            &tools,
            vec![contract_test_kr("kr1", &["a"])],
            &policy,
            &signals,
            &[],
            CUSTOMER_TEXT,
            HYBRID,
        )
        .await;
    let error = result.err().expect("manual store is not configured");
    assert!(
        error.to_string().contains("manual store not configured"),
        "{error}"
    );
    assert_eq!(judge.calls(), 1);
}

#[test]
fn warn_unmatched_excluded_ids_warns_once_per_id_and_names_the_load_window() {
    let harness = kr_selection_harness(None, &["typo-kr"]);
    let resolutions = [contract_test_kr("kr1", &["a"])];
    let ((), first) = crate::test_support::capture_logs(|| {
        harness.warn_unmatched_excluded_ids("req-mcp", &resolutions)
    });
    let ((), second) = crate::test_support::capture_logs(|| {
        harness.warn_unmatched_excluded_ids("req-mcp", &resolutions)
    });
    let first = filter_warn_and_error_lines(&first);
    assert!(first.contains("kr_id=typo-kr"), "{first}");
    assert!(first.contains("req-mcp"), "{first}");
    assert!(first.contains("1000-node"), "{first}");
    assert!(!first.contains(CUSTOMER_TEXT), "{first}");
    assert_eq!(filter_warn_and_error_lines(&second), "");
}
