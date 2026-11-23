use super::*;
use crate::harness::clarify::FALLBACK_CLARIFY_TEXT;
use crate::reply_test_harness::LlmBackend;

const LLM_API_KEY_ENV: &str = "CS_SUPPORT_LLM_API_KEY";
const F_P7_MODELS: [&str; 7] = [
    "ADC-V523",
    "ADC-V523X",
    "ADC-V724",
    "ADC-V724X",
    "ADC-VC729P",
    "ADC-VC727P",
    "ADC-VC827P",
];
const QUESTION_ENDINGS: [&str; 6] = ["？", "?", "ください", "でしょうか", "ますか", "いただけ"];

struct Checks {
    scenario: String,
    failures: Vec<String>,
}

impl Checks {
    fn new(scenario: &str) -> Self {
        Self {
            scenario: scenario.to_string(),
            failures: Vec::new(),
        }
    }

    fn check(&mut self, label: &str, passed: bool) {
        println!(
            "L3 scenario={} check {label}={}",
            self.scenario,
            if passed { "pass" } else { "FAIL" }
        );
        if !passed {
            self.failures.push(format!("{}: {label}", self.scenario));
        }
    }
}

fn print_turn(scenario: &str, h: &ReplyHarness, turn: &Turn) {
    println!(
        "L3 scenario={scenario} turn={} message={:?} reply_kind={:?} decision={:?} case_reset={} product_models={:?} pending={:?} confirmed={:?}\nL3 scenario={scenario} turn={} reply_text<<<\n{}\n>>>",
        turn.index,
        turn.message,
        turn.reply_kind,
        turn.decision,
        turn.case_reset,
        turn.product_models,
        pending_of(h, &turn.case_id),
        confirmed_of(h, &turn.case_id),
        turn.index,
        turn.reply_text,
    );
}

fn sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '。' | '？' | '?' | '\n') {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        sentences.push(current);
    }
    sentences
}

fn asks_about(text: &str, words: &[&str]) -> bool {
    sentences(text).iter().any(|sentence| {
        words.iter().any(|word| sentence.contains(word))
            && QUESTION_ENDINGS
                .iter()
                .any(|ending| sentence.contains(ending))
    })
}

fn check_confirmation(
    checks: &mut Checks,
    turn: &Turn,
    written: &str,
    suggestions: &[&str],
    others: &[&str],
) {
    checks.check(
        "reply_kind_name_confirm",
        turn.reply_kind.as_deref() == Some("name_confirm"),
    );
    checks.check(
        "M1",
        turn.reply_text.contains(written)
            && suggestions
                .iter()
                .all(|name| turn.reply_text.contains(name)),
    );
    checks.check(
        "M2",
        others.iter().all(|name| !turn.reply_text.contains(name)),
    );
    checks.check("M3", turn.reply_text != FALLBACK_CLARIFY_TEXT);
    checks.check(
        "M4",
        turn.reply_text != fixed_confirmation(written, suggestions),
    );
}

fn fixed_confirmation(written: &str, suggestions: &[&str]) -> String {
    let question = match suggestions.len() {
        1 => "のことでしょうか？",
        2 => "のどちらのことでしょうか？",
        _ => "のどれのことでしょうか？",
    };
    format!(
        "{written} とお知らせいただきましたが、{} {question}",
        suggestions.join("、")
    )
}

fn check_resumed(checks: &mut Checks, turn: &Turn, name_words: &[&str]) {
    checks.check("M5", !asks_about(&turn.reply_text, name_words));
    checks.check(
        "M6",
        turn.reply_kind.as_deref() != Some("name_confirm")
            && turn.decision.as_deref() != Some(PRODUCT_UNIDENTIFIED),
    );
}

fn others_than(suggestions: &[&str]) -> Vec<&'static str> {
    F_P7_MODELS
        .iter()
        .copied()
        .filter(|model| !suggestions.contains(model))
        .collect()
}

async fn start(fixture: Fixture) -> ReplyHarness {
    assert!(
        std::env::var(LLM_API_KEY_ENV).is_ok_and(|key| !key.trim().is_empty()),
        "set {LLM_API_KEY_ENV} for the real LLM check"
    );
    ReplyHarness::start_with(fixture, LlmBackend::Anthropic).await
}

async fn t1(checks: &mut Checks, first_message: &str) {
    let h = start(f_p7_fixture()).await;
    let mut conv = h.conversation();
    let first = conv.say(first_message).await;
    print_turn(&checks.scenario, &h, &first);
    check_confirmation(
        checks,
        &first,
        "ADV-VC729P",
        &["ADC-VC729P"],
        &others_than(&["ADC-VC729P"]),
    );
    for message in [
        "はい、全く録画されていません",
        "夜の間だけ録画されていないようです",
    ] {
        let turn = conv.say(message).await;
        print_turn(&checks.scenario, &h, &turn);
        check_resumed(checks, &turn, &["型番"]);
        checks.check(
            "identified",
            turn.product_models == vec!["ADC-VC729P".to_string()],
        );
    }
}

async fn t2(checks: &mut Checks) {
    let h = start(f_p7_fixture()).await;
    let history = serde_json::json!([
        {"role": "customer", "text": "ADV-VC729Pをつかっているが、録画されなき"},
        {"role": "assistant", "text": "ご相談ありがとうございます。製品と症状を確認させてください。\n・お使いの製品の型番をご確認ください。\n・録画されないのは、すべてのカメラですか。"},
    ]);
    for message in [
        "型番は最初に伝えている",
        "全く録画されていない、型番は最初に伝えている",
    ] {
        let (status, body) = h
            .post_reply(serde_json::json!({"message": message, "history": history}))
            .await;
        let reply: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let case_id = reply["case_id"].as_str().unwrap_or_default().to_string();
        let reply_text = reply["reply_text"].as_str().unwrap_or_default().to_string();
        let reply_kind = h
            .conversation_turns(&case_id)
            .last()
            .and_then(|turn| turn.attr("reply_kind").map(ToString::to_string));
        println!(
            "L3 scenario={} message={message:?} status={status} reply_kind={reply_kind:?} pending={:?} product_models={:?}\nL3 scenario={} reply_text<<<\n{reply_text}\n>>>",
            checks.scenario,
            pending_of(&h, &case_id),
            h.product_models(&case_id),
            checks.scenario
        );
        let turn = Turn {
            index: 1,
            message: message.to_string(),
            status,
            reply_text,
            case_id: case_id.clone(),
            case_reset: false,
            error_body: None,
            reply_kind,
            decision: None,
            product_models: h.product_models(&case_id),
            llm_requests: Vec::new(),
        };
        check_confirmation(
            checks,
            &turn,
            "ADV-VC729P",
            &["ADC-VC729P"],
            &others_than(&["ADC-VC729P"]),
        );
        checks.check("not_identified", turn.product_models.is_empty());
        if message.starts_with("全く") {
            let mut history_c = history.clone();
            history_c.as_array_mut().into_iter().for_each(|turns| {
                turns.push(serde_json::json!({"role": "customer", "text": message}));
                turns.push(serde_json::json!({"role": "assistant", "text": turn.reply_text}));
            });
            let (status, body) = h
                .post_reply(serde_json::json!({"message": "はい", "history": history_c, "case_id": case_id}))
                .await;
            let reply: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let reply_text = reply["reply_text"].as_str().unwrap_or_default().to_string();
            let last = h.conversation_turns(&case_id);
            let kind = last
                .last()
                .and_then(|turn| turn.attr("reply_kind").map(ToString::to_string));
            let decision = last
                .last()
                .and_then(|turn| turn.attr("audit_event_id"))
                .and_then(|id| h.audit_decision(id));
            println!(
                "L3 scenario={}-c status={status} reply_kind={kind:?} decision={decision:?} product_models={:?}\nL3 scenario={}-c reply_text<<<\n{reply_text}\n>>>",
                checks.scenario,
                h.product_models(&case_id),
                checks.scenario
            );
            let resumed = Turn {
                reply_text,
                reply_kind: kind,
                decision,
                product_models: h.product_models(&case_id),
                ..turn
            };
            check_resumed(checks, &resumed, &["型番"]);
            checks.check(
                "identified",
                resumed.product_models == vec!["ADC-VC729P".to_string()],
            );
        }
    }
}

async fn t3_product_name(checks: &mut Checks) {
    let h = start(f_name_fixture()).await;
    let mut conv = h.conversation();
    let first = conv.say("みまもりドーカムメラが録画されません").await;
    print_turn(&checks.scenario, &h, &first);
    check_confirmation(
        checks,
        &first,
        "みまもりドーカムメラ",
        &["みまもりドームカメラ"],
        &["おでかけパノラマカメラ"],
    );
    for message in ["はい、そうです", "昨日から録画されていません"] {
        let turn = conv.say(message).await;
        print_turn(&checks.scenario, &h, &turn);
        check_resumed(checks, &turn, &["型番", "製品名"]);
        checks.check(
            "identified",
            turn.product_models == vec!["ADC-V724".to_string()],
        );
    }
}

async fn t3_company(checks: &mut Checks) {
    let h = start(f_name_fixture()).await;
    register_companies(&h, F_ORG_FILE).await;
    let mut conv = h.conversation();
    let first = conv
        .say("ミドリノ警備保証株式会社から購入したADC-V724が録画されません")
        .await;
    print_turn(&checks.scenario, &h, &first);
    check_confirmation(
        checks,
        &first,
        "ミドリノ警備保証株式会社",
        &["ミドリノ警備保障株式会社"],
        &["ヒガシ電設工業株式会社"],
    );
    for message in ["はい、合っています", "夜だけ録画されません"] {
        let turn = conv.say(message).await;
        print_turn(&checks.scenario, &h, &turn);
        check_resumed(checks, &turn, &["社名", "会社名", "購入先", "ご購入元"]);
        checks.check(
            "company_confirmed",
            confirmed_of(&h, &turn.case_id) == vec!["ミドリノ警備保障株式会社".to_string()],
        );
    }
}

async fn t4_tie_and_denial(checks: &mut Checks) {
    let h = start(f_p7_fixture()).await;
    let tie = h.conversation().say("ADC-VC728Pが録画されない").await;
    print_turn(&checks.scenario, &h, &tie);
    check_confirmation(
        checks,
        &tie,
        "ADC-VC728P",
        &["ADC-VC729P", "ADC-VC727P"],
        &["ADC-VC827P"],
    );

    let none = h
        .conversation()
        .say("QX-9000を使っているが録画されない")
        .await;
    print_turn(&checks.scenario, &h, &none);
    checks.check(
        "no_candidate_for_unknown_model",
        none.reply_kind.as_deref() != Some("name_confirm"),
    );

    let mut conv = h.conversation();
    let first = conv.say("ADV-VC729Pを使っているが録画されない").await;
    print_turn(&checks.scenario, &h, &first);
    for message in ["いいえ、違います", "ADV-VC729Pです"] {
        let turn = conv.say(message).await;
        print_turn(&checks.scenario, &h, &turn);
        checks.check("H5_machine", !turn.reply_text.contains("ADC-VC729P"));
        checks.check("not_identified", turn.product_models.is_empty());
    }
}

async fn t5_out_of_scope_path(checks: &mut Checks) {
    let h = start(f_p7_fixture()).await;
    let mut conv = h.conversation();
    let first = conv.say("ADC-VC729の設定を教えてください").await;
    print_turn(&checks.scenario, &h, &first);
    check_confirmation(
        checks,
        &first,
        "ADC-VC729",
        &["ADC-VC729P"],
        &others_than(&["ADC-VC729P"]),
    );
    let turn = conv.say("はい").await;
    print_turn(&checks.scenario, &h, &turn);
    check_resumed(checks, &turn, &["型番"]);
    checks.check(
        "identified",
        turn.product_models == vec!["ADC-VC729P".to_string()],
    );
}

#[tokio::test]
#[ignore = "needs CS_SUPPORT_LLM_API_KEY and calls the Anthropic API"]
async fn l3_real_llm_name_confirmation() {
    let mut failures = Vec::new();
    for (scenario, first_message) in [
        ("t1", "ADV-VC729Pを使っているが録画されない"),
        ("t1-original", "ADV-VC729Pをつかっているが、録画されなき"),
    ] {
        let mut checks = Checks::new(scenario);
        t1(&mut checks, first_message).await;
        failures.extend(checks.failures);
    }
    let mut checks = Checks::new("t2");
    t2(&mut checks).await;
    failures.extend(checks.failures);
    let mut checks = Checks::new("t3-product-name");
    t3_product_name(&mut checks).await;
    failures.extend(checks.failures);
    let mut checks = Checks::new("t3-company");
    t3_company(&mut checks).await;
    failures.extend(checks.failures);
    let mut checks = Checks::new("t4");
    t4_tie_and_denial(&mut checks).await;
    failures.extend(checks.failures);
    let mut checks = Checks::new("t5-out-of-scope-path");
    t5_out_of_scope_path(&mut checks).await;
    failures.extend(checks.failures);
    println!("L3 summary failures={failures:?}");
    assert!(failures.is_empty(), "{failures:?}");
}
