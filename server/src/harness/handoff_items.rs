//! 取り次ぐ項目（handoff items）の導出と、部分回答下書きの可否判定・安全側の歯止め。
//!
//! 正本: `docs/superpowers/specs/2026-10-07-partial-answer-with-handoff-design.md` §2, §3.1,
//! §3.5。**`decision::decide` 自体は変更しない。** ここは `decide` が確定した
//! `AnswerDecision`・マッチしたルール/禁止領域から「顧客に見せてよい取次項目のラベル」を導出し、
//! 下書き生成の可否と、生成された下書きが歯止めを通るかを判定する純関数の集まり。

use crate::harness::decision::{AnswerDecision, Thresholds};
use crate::harness::reply::NO_ANSWER_TOKEN;
use crate::harness::rules::{
    match_layer2, matching_layer1_rules, EscalationRule, ProhibitedDomain,
};
use crate::harness::signal::{LexiconNormalizer, SignalSet};
use std::collections::HashSet;
use std::sync::OnceLock;

/// マッチした第1層ルールの `condition`、または第2層禁止領域の `domain_signals` から、
/// 顧客提示用ラベル（`customer_label`）の列を導出する（design doc §2「取り次ぐ項目」）。
///
/// - 順序は lexicon の宣言順（`SignalSet` は `BTreeSet` でアルファベット順になるため、
///   そのまま列挙すると lexicon が意図した提示順が失われる）。
/// - 重複するラベルは除く（同じ文言を2回案内しない）。
/// - `customer_label_of` が `None` を返す signal（lexicon 未登録、または `customer_label` が
///   未設定）は結果に含めず、`tracing::warn!` で signal 名（内部識別子）だけを出す。質問本文・
///   顧客発話はここでは一切扱っていないので出す対象にもならない。
pub fn derive_handoff_items(normalizer: &LexiconNormalizer, signals: &SignalSet) -> Vec<String> {
    let order = normalizer.declared_signal_order();
    let position_of = |name: &str| order.iter().position(|s| s == name).unwrap_or(usize::MAX);

    let mut ordered: Vec<_> = signals.iter().collect();
    ordered.sort_by_key(|signal| position_of(signal.as_str()));

    let mut items = Vec::new();
    let mut seen_labels: HashSet<String> = HashSet::new();
    for signal in ordered {
        match normalizer.customer_label_of(signal) {
            Some(label) => {
                if seen_labels.insert(label.to_string()) {
                    items.push(label.to_string());
                }
            }
            None => {
                tracing::warn!(
                    signal = signal.as_str(),
                    "signal has no customer_label in the lexicon; omitting it from the handoff \
                     items shown to the customer (the lexicon entry is missing, or declares no \
                     usable customer_label)"
                );
            }
        }
    }
    items
}

/// 部分回答の可否（決定論、design doc §3.1）。
///
/// `Escalate` のターンで、次をすべて満たすときだけ `true`:
/// - `hearing.is_none()`。spec §3.1 の条件は「hearing による `Clarify` ではない」だが、
///   `Clarify` になるかどうかは `evaluate()` の後に `api.rs` の Jev（`has_enough_info`）判定で
///   決まり、この関数の呼び出し時点では分からない。そのため hearing 契約を宣言したルールに
///   マッチしたターンは一律 `false` とする（安全側の近似であり、**spec が明示した決定ではない**）。
///   帰結として、Jev が「情報十分」と判定して `Clarify` ではなく取次（EscalationReply）になる
///   ターン（例: rules.json の `warranty-failure`）でも部分回答は生成されない。Jev 判定の後で
///   部分回答を計画するよう制御フローを変えるかどうかは spec の明確化待ち（オーケストレーター判断）。
/// - 判定が第1層または第2層の取次である。第3層（根拠不足・確信度不足）は `handoff_items` の
///   中身に依存せず、`layer` で明示的に除外する（`handoff_items_for_decision` が将来第3層で
///   非空を返しても、spec 変更なしに部分回答が有効化されないようにするため）。
/// - 取り次ぐ項目（`handoff_items`）が1つ以上ある（上の layer 条件とは独立に両方必要）。
/// - その case が既に取次済みではない（`already_escalated == false`）。
/// - 下書き生成が有効（`draft_generation_enabled == true`。`customer_reply_draft_enabled =
///   true` かつ `[llm] enabled = true` と同値）。
/// - `best_manual_score` が関連十分（`>= thresholds.low`。`None`、または `low` 未満は不十分）。
///
/// `decision` が `Allowed` のときは常に `false`（部分回答という概念自体が `Escalate` 専用）。
pub fn can_draft_partial_answer(
    decision: &AnswerDecision,
    handoff_items: &[String],
    best_manual_score: Option<f32>,
    thresholds: &Thresholds,
    already_escalated: bool,
    draft_generation_enabled: bool,
) -> bool {
    let AnswerDecision::Escalate { hearing, layer, .. } = decision else {
        return false;
    };
    if hearing.is_some() {
        return false;
    }
    if !matches!(*layer, 1 | 2) {
        return false;
    }
    if handoff_items.is_empty() {
        return false;
    }
    if already_escalated || !draft_generation_enabled {
        return false;
    }
    best_manual_score.is_some_and(|score| score >= thresholds.low)
}

/// 半角・全角数字の後に、任意の空白と任意の漢数字単位（万・千・百・億）を挟んで `円` / `%` / `％`
/// が続く断定表現を検出する（design doc §3.5 の3、§5「既知の限界」: 単位を伴わない断定
/// 「無料です」は対象外。egress gate の NG 辞書側で補う）。「3万円」「１，５４０円」
/// 「5,000 円」「1万5千円（末尾の「5千円」で検出）」を拾う。
fn monetary_assertion_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"[0-9０-９][\s　]*[万千百億]?[\s　]*(円|%|％)")
            .expect("monetary assertion regex must compile")
    })
}

/// 下書き 1 件を採用してよいかの判定（`evaluate()` が下書き生成後に 1 回呼ぶ）。
///
/// - `NO_ANSWER_TOKEN` そのもの（trim 後）は `partial_answer_ok` に関係なく**常に**不採用。
///   Allowed の下書きに混ざると、そのまま顧客へ送られる（`api.rs` の `ReplyAction::Answer`）
///   ため。
/// - §3.5 の歯止め（ラベル網羅・金額断定）は `partial_answer_ok` のときだけ課す。通常の
///   Allowed 下書きには余計な制約を課さない。
///
/// `Err` の理由文字列に顧客発話・下書き本文は含まれない。
pub fn accept_draft(
    draft: &str,
    partial_answer_ok: bool,
    handoff_items: &[String],
) -> Result<(), String> {
    if draft.trim() == NO_ANSWER_TOKEN {
        return Err(format!(
            "the draft is exactly the {NO_ANSWER_TOKEN:?} token (no answerable part found)"
        ));
    }
    if partial_answer_ok {
        passes_handoff_safeguards(draft, handoff_items)?;
    }
    Ok(())
}

/// 判定結果に対応する取り次ぐ項目（顧客提示用ラベル）を導出する（design doc §2）。
///
/// `decide()` が内部で使ったのと同一の純関数・入力を再度呼ぶだけなので、判定結果とは必ず
/// 一致する。layer=1 は**マッチした全ての第1層ルール**（`matching_layer1_rules`。
/// `match_layer1` は 1 件しか返さず、他ルールの話題が取り次ぐ項目から漏れて材料で答えられて
/// しまうため）の `condition` の和集合、layer=2 は `match_layer2` の `domain_signals`。
/// layer=3 と `Allowed` は空。
pub fn handoff_items_for_decision(
    decision: &AnswerDecision,
    rules: &[EscalationRule],
    domains: &[ProhibitedDomain],
    accumulated: &SignalSet,
    question: &str,
    normalizer: &LexiconNormalizer,
) -> Vec<String> {
    match decision {
        AnswerDecision::Escalate { layer: 1, .. } => {
            let union: SignalSet = matching_layer1_rules(rules, accumulated)
                .into_iter()
                .flat_map(|rule| rule.condition.iter().cloned())
                .collect();
            derive_handoff_items(normalizer, &union)
        }
        AnswerDecision::Escalate { layer: 2, .. } => match_layer2(domains, accumulated, question)
            .map(|domain| derive_handoff_items(normalizer, &domain.domain_signals))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// 安全側の歯止め（design doc §3.5 の2〜4）。1番（egress gate / 取扱外型番ゲート）は呼び出し側
/// （`Harness::draft_customer_reply` 内の `egress_gate`、`api.rs` / `advisor/cs_support.rs` の
/// `gate_customer_reply_draft`）で既に適用済みのため、ここでは対象外。
///
/// 1 つでも失敗すれば `Err`。返す理由の文字列には**顧客発話・下書き本文は含めない**
/// （`handoff_items` のラベル自体は運用者が承認済みの顧客提示用文言なので含めてよい）。
///
/// 既知の限界（design doc §3.5(3) の定義どおりで、ここでは塞がない）: 金額検査は「ラベルを含む
/// 文（「。」区切り）」だけを見る。ラベルを含まない別の文での金額断定、短縮名（例「初期費用は
/// …」）での断定、可否・条件の断定（数字+単位を伴わないもの）は検出しない。これらはプロンプト
/// 指示と egress gate に委ねている（design doc §3.2・§5）。
pub fn passes_handoff_safeguards(draft: &str, handoff_items: &[String]) -> Result<(), String> {
    // 4. 下書きが NO_ANSWER_TOKEN そのものでない。運用ログの理由が「ラベル欠落」に化けない
    //    よう、最初に判定する。
    if draft.trim() == NO_ANSWER_TOKEN {
        return Err(format!(
            "the draft is exactly the {NO_ANSWER_TOKEN:?} token (no answerable part found)"
        ));
    }
    // 2. handoff_items の各ラベルが下書きに1回以上現れる（黙って答えていない・黙って落として
    //    いないことの確認）。
    for label in handoff_items {
        if !draft.contains(label.as_str()) {
            return Err(format!(
                "a handoff item label is missing from the draft: {label:?}"
            ));
        }
    }
    // 3. handoff_items のラベルを含む文（「。」区切り）に金額・数量の断定が含まれない。
    for sentence in draft.split('。') {
        let mentions_handoff_item = handoff_items
            .iter()
            .any(|label| sentence.contains(label.as_str()));
        if mentions_handoff_item && monetary_assertion_regex().is_match(sentence) {
            return Err(
                "a sentence mentioning a handoff item label contains a monetary/quantity \
                 assertion (the sentence text itself is not included in this message)"
                    .to_string(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::decision::{AnswerSource, DisclosureScope, EscalateReason, Stakes};
    use crate::harness::rules::{Binding, HearingContract};
    use crate::harness::signal::Signal;

    fn signals(values: &[&str]) -> SignalSet {
        values.iter().map(|v| Signal::new(*v)).collect()
    }

    // ---- derive_handoff_items ----

    fn lexicon_with_labels() -> LexiconNormalizer {
        LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "zebra_question", "class": "context", "surface_forms": ["ゼブラ"],
                  "customer_label": "ゼブラに関するご質問" },
                { "signal": "initial_cost_question", "class": "context", "surface_forms": ["初期費用"],
                  "customer_label": "初期費用・設置工事費に関するご質問" },
                { "signal": "no_label_signal", "class": "context", "surface_forms": ["ラベル無し"] }
            ] }"#,
        )
        .expect("lexicon parses")
    }

    #[test]
    fn derive_handoff_items_orders_by_lexicon_declaration_not_alphabetically() {
        let lex = lexicon_with_labels();
        // SignalSet（BTreeSet）はアルファベット順: initial_cost_question < zebra_question。
        // lexicon の宣言順（JSON 配列順）は zebra_question が先。導出結果は宣言順になること。
        let items =
            derive_handoff_items(&lex, &signals(&["initial_cost_question", "zebra_question"]));
        assert_eq!(
            items,
            vec![
                "ゼブラに関するご質問".to_string(),
                "初期費用・設置工事費に関するご質問".to_string(),
            ]
        );
    }

    #[test]
    fn derive_handoff_items_deduplicates_identical_labels() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "a", "class": "context", "surface_forms": ["a"], "customer_label": "同じラベル" },
                { "signal": "b", "class": "context", "surface_forms": ["b"], "customer_label": "同じラベル" }
            ] }"#,
        )
        .unwrap();
        let items = derive_handoff_items(&lex, &signals(&["a", "b"]));
        assert_eq!(items, vec!["同じラベル".to_string()]);
    }

    #[test]
    fn derive_handoff_items_omits_signals_without_a_customer_label_and_warns() {
        let lex = lexicon_with_labels();
        let (items, logs) = crate::test_support::capture_logs(|| {
            derive_handoff_items(&lex, &signals(&["zebra_question", "no_label_signal"]))
        });
        assert_eq!(items, vec!["ゼブラに関するご質問".to_string()]);
        let warnings = crate::test_support::filter_warn_and_error_lines(&logs);
        assert!(
            warnings.contains("no_label_signal"),
            "expected a warning naming the signal without a customer_label, got: {warnings}"
        );
    }

    #[test]
    fn derive_handoff_items_returns_empty_for_an_empty_signal_set() {
        let lex = lexicon_with_labels();
        assert!(derive_handoff_items(&lex, &SignalSet::new()).is_empty());
    }

    // ---- can_draft_partial_answer ----

    fn escalate(hearing: Option<HearingContract>) -> AnswerDecision {
        AnswerDecision::Escalate {
            reason: EscalateReason::RegulatedOrSafety,
            layer: 1,
            route_to: "support_desk".to_string(),
            disclosure_scope: DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
            hearing,
            customer_ack: None,
        }
    }

    fn allowed() -> AnswerDecision {
        AnswerDecision::Allowed {
            source: AnswerSource::Manual,
            evidence_section_keys: Vec::new(),
            known_resolution_id: None,
            stakes: Stakes::Low,
            threshold: 0.6,
        }
    }

    fn thresholds() -> Thresholds {
        Thresholds {
            low: 0.6,
            mid: 0.8,
            high: 0.95,
        }
    }

    fn handoff_items_fixture() -> Vec<String> {
        vec!["初期費用・設置工事費に関するご質問".to_string()]
    }

    #[test]
    fn can_draft_partial_answer_is_true_when_every_condition_is_met() {
        assert!(can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            Some(0.6),
            &thresholds(),
            false,
            true
        ));
        assert!(can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            Some(0.9),
            &thresholds(),
            false,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_false_for_an_allowed_decision() {
        // Allowed は `handoff_items` の値に関わらず早期 false になるため、空配列で構わない。
        assert!(!can_draft_partial_answer(
            &allowed(),
            &[],
            Some(0.9),
            &thresholds(),
            false,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_false_when_a_hearing_contract_is_declared() {
        // spec §3.1 の「Clarify ではない」条件の安全側の近似: Clarify かどうかは Jev 判定（evaluate()
        // の後）まで分からないため、hearing 契約を宣言したルールのターンは一律スキップする。
        // spec が改訂されたら見直す対象（spec が明示した決定ではない）。
        assert!(!can_draft_partial_answer(
            &escalate(Some(HearingContract::ProductAndSymptom)),
            &handoff_items_fixture(),
            Some(0.9),
            &thresholds(),
            false,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_false_when_there_are_no_handoff_items() {
        // design doc §3.1: 取り次ぐ項目が1つも無いとき（第3層の取次を含む）は部分回答の対象に
        // しない。他の条件（関連十分・未取次・下書き有効・hearing なし）をすべて満たしていても
        // false になることを固定する。
        assert!(!can_draft_partial_answer(
            &escalate(None),
            &[],
            Some(0.9),
            &thresholds(),
            false,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_false_for_a_layer3_escalate_even_with_sufficient_relevance() {
        // 第3層の取次は `handoff_items_for_decision` が常に空配列を返すため、実際に起きるのは
        // この組み合わせ（layer=3 かつ handoff_items が空）。根拠不足・確信度不足は「答えてはいけ
        // ない部分」ではなく「答える根拠が足りない」ので、材料の関連度が下限を超えていても部分
        // 回答を試みてはならない（design doc §3.1）。
        assert!(!can_draft_partial_answer(
            &escalate_at(3),
            &[],
            Some(0.9),
            &thresholds(),
            false,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_false_for_a_layer3_escalate_even_with_handoff_items() {
        // handoff_items が非空でも第3層は対象外（design doc §3.1）。`handoff_items_for_decision` の
        // 将来の変更で暗黙に有効化されないよう、layer での明示的な除外を固定する。
        assert!(!can_draft_partial_answer(
            &escalate_at(3),
            &handoff_items_fixture(),
            Some(0.9),
            &thresholds(),
            false,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_true_for_layer1_and_layer2_escalates_with_handoff_items() {
        // design doc §3.1: 第1層・第2層のマッチによる取次は部分回答の対象。
        for layer in [1, 2] {
            assert!(
                can_draft_partial_answer(
                    &escalate_at(layer),
                    &handoff_items_fixture(),
                    Some(0.9),
                    &thresholds(),
                    false,
                    true
                ),
                "layer {layer} must be eligible"
            );
        }
    }

    #[test]
    fn can_draft_partial_answer_is_false_when_relevance_is_insufficient() {
        assert!(!can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            Some(0.59),
            &thresholds(),
            false,
            true
        ));
        assert!(!can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            None,
            &thresholds(),
            false,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_false_when_the_case_is_already_escalated() {
        assert!(!can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            Some(0.9),
            &thresholds(),
            true,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_false_when_draft_generation_is_disabled() {
        assert!(!can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            Some(0.9),
            &thresholds(),
            false,
            false
        ));
    }

    // ---- passes_handoff_safeguards ----

    #[test]
    fn passes_handoff_safeguards_rejects_a_draft_missing_a_handoff_label() {
        let items = vec!["初期費用・設置工事費に関するご質問".to_string()];
        let drafted = "月額利用料金は1台1,540円（税込）です。";
        assert!(passes_handoff_safeguards(drafted, &items).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_rejects_a_monetary_assertion_in_a_sentence_with_the_label() {
        let items = vec!["初期費用・設置工事費に関するご質問".to_string()];
        let drafted = "初期費用・設置工事費に関するご質問は1,540円です。";
        assert!(passes_handoff_safeguards(drafted, &items).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_rejects_the_no_answer_token_verbatim() {
        assert!(passes_handoff_safeguards(NO_ANSWER_TOKEN, &[]).is_err());
        // 前後の空白は trim されるので同様に拒否される。
        assert!(passes_handoff_safeguards(&format!("  {NO_ANSWER_TOKEN}  "), &[]).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_accepts_a_clean_draft() {
        let items = vec!["初期費用・設置工事費に関するご質問".to_string()];
        let drafted = "月額利用料金は1台1,540円（税込）です。初期費用・設置工事費に関する\
                        ご質問は担当者がご案内します。";
        assert!(passes_handoff_safeguards(drafted, &items).is_ok());
    }

    #[test]
    fn passes_handoff_safeguards_is_vacuously_satisfied_when_there_are_no_handoff_items() {
        // handoff_items が空のとき、2(ラベル網羅)・3(金額断定なし)は自動的に満たされる。
        assert!(passes_handoff_safeguards("月額利用料金は1台1,540円です。", &[]).is_ok());
    }

    const LABEL: &str = "初期費用・設置工事費に関するご質問";

    #[test]
    fn passes_handoff_safeguards_rejects_monetary_assertions_with_unit_prefixes_widths_and_spaces()
    {
        let items = vec![LABEL.to_string()];
        for amount in [
            "3万円",
            "１，５４０円",
            "5,000 円",
            "10％",
            "10%",
            "1万5千円",
        ] {
            let drafted = format!("{LABEL}は{amount}です。");
            assert!(
                passes_handoff_safeguards(&drafted, &items).is_err(),
                "{amount:?} next to the label must be rejected"
            );
        }
    }

    #[test]
    fn passes_handoff_safeguards_accepts_an_amount_in_a_sentence_without_the_label() {
        let items = vec![LABEL.to_string()];
        let drafted = format!("月額は1,540円です。{LABEL}は担当者がご案内します。");
        assert!(passes_handoff_safeguards(&drafted, &items).is_ok());
    }

    #[test]
    fn passes_handoff_safeguards_known_limit_does_not_detect_an_amount_in_a_separate_sentence() {
        let items = vec![LABEL.to_string()];
        let drafted = format!("{LABEL}は担当者がご案内します。初期費用は1,540円です。");
        // これは仕様上の既知の限界であり、望ましい挙動として固定しているのではない。
        // spec を改訂して検出対象を広げるときは、このテストを反転（is_err）させる。
        assert!(passes_handoff_safeguards(&drafted, &items).is_ok());
    }

    #[test]
    fn passes_handoff_safeguards_treats_a_newline_separated_clause_as_the_same_sentence() {
        let items = vec![LABEL.to_string()];
        // 区切りは「。」のみ（`split('。')`）なので、改行で分けても同一文として検出される。
        let drafted = format!("{LABEL}は担当者がご案内します\n工事費は3万円です。");
        assert!(passes_handoff_safeguards(&drafted, &items).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_reports_the_no_answer_token_before_a_missing_label() {
        let items = vec![LABEL.to_string()];
        let err = passes_handoff_safeguards(NO_ANSWER_TOKEN, &items).unwrap_err();
        assert!(
            err.contains(NO_ANSWER_TOKEN),
            "the reason must name the NO_ANSWER token, not a missing label: {err}"
        );
    }

    // ---- accept_draft ----

    #[test]
    fn accept_draft_rejects_no_answer_token_for_a_non_partial_draft() {
        assert!(accept_draft(NO_ANSWER_TOKEN, false, &[]).is_err());
        assert!(accept_draft(&format!(" {NO_ANSWER_TOKEN}\n"), false, &[]).is_err());
    }

    #[test]
    fn accept_draft_imposes_no_handoff_safeguards_on_a_non_partial_draft() {
        // ラベル欠落・金額を含んでいても、通常の Allowed 下書きは採用される。
        let items = vec![LABEL.to_string()];
        assert!(accept_draft("月額は1,540円です。", false, &items).is_ok());
    }

    #[test]
    fn accept_draft_rejects_a_partial_draft_missing_a_label() {
        let items = vec![LABEL.to_string()];
        assert!(accept_draft("月額は1,540円です。", true, &items).is_err());
    }

    #[test]
    fn accept_draft_accepts_a_clean_partial_draft() {
        let items = vec![LABEL.to_string()];
        let drafted = format!("月額は1,540円です。{LABEL}は担当者がご案内します。");
        assert!(accept_draft(&drafted, true, &items).is_ok());
    }

    // ---- handoff_items_for_decision ----

    fn rule(id: &str, condition: &[&str], binding: Binding) -> EscalationRule {
        EscalationRule {
            id: id.to_string(),
            condition: signals(condition),
            route: "support_desk".to_string(),
            owner: None,
            binding,
            hearing: None,
            customer_ack: None,
        }
    }

    fn lexicon_for_decision() -> LexiconNormalizer {
        LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "initial_cost_question", "class": "context", "surface_forms": ["初期費用"],
                  "customer_label": "初期費用に関するご質問" },
                { "signal": "cancel_question", "class": "context", "surface_forms": ["解約"],
                  "customer_label": "解約に関するご質問" },
                { "signal": "legal_question", "class": "context", "surface_forms": ["訴訟"],
                  "customer_label": "法的なご相談" }
            ] }"#,
        )
        .expect("lexicon parses")
    }

    fn escalate_at(layer: u8) -> AnswerDecision {
        AnswerDecision::Escalate {
            reason: EscalateReason::RegulatedOrSafety,
            layer,
            route_to: "support_desk".to_string(),
            disclosure_scope: DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
            hearing: None,
            customer_ack: None,
        }
    }

    #[test]
    fn handoff_items_for_decision_unions_every_matching_layer1_rule_in_lexicon_order() {
        let lex = lexicon_for_decision();
        let rules = vec![
            rule("contract-billing", &["cancel_question"], Binding::Advisory),
            rule(
                "initial-cost-quote",
                &["initial_cost_question"],
                Binding::Mandatory,
            ),
            rule(
                "dup",
                &["initial_cost_question", "cancel_question"],
                Binding::Advisory,
            ),
        ];
        let accumulated = signals(&["initial_cost_question", "cancel_question"]);
        let items = handoff_items_for_decision(
            &escalate_at(1),
            &rules,
            &[],
            &accumulated,
            "初期費用と解約について",
            &lex,
        );
        assert_eq!(
            items,
            vec![
                "初期費用に関するご質問".to_string(),
                "解約に関するご質問".to_string()
            ]
        );
    }

    #[test]
    fn handoff_items_for_decision_uses_domain_signals_for_layer2() {
        let lex = lexicon_for_decision();
        let domains = vec![ProhibitedDomain {
            id: "legal".to_string(),
            domain_signals: signals(&["legal_question"]),
            text_patterns: Vec::new(),
            route: "legal".to_string(),
            binding: Binding::Mandatory,
        }];
        let items = handoff_items_for_decision(
            &escalate_at(2),
            &[],
            &domains,
            &signals(&["legal_question"]),
            "訴訟を考えています",
            &lex,
        );
        assert_eq!(items, vec!["法的なご相談".to_string()]);
    }

    #[test]
    fn handoff_items_for_decision_is_empty_for_layer3_and_allowed() {
        let lex = lexicon_for_decision();
        let rules = vec![rule("r", &["cancel_question"], Binding::Advisory)];
        let accumulated = signals(&["cancel_question"]);
        for decision in [escalate_at(3), allowed()] {
            assert!(
                handoff_items_for_decision(&decision, &rules, &[], &accumulated, "q", &lex)
                    .is_empty()
            );
        }
    }
}
