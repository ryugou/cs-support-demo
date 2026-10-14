//! 取り次ぐ項目（handoff items）の導出と、部分回答下書きの可否判定・安全側の歯止め。
//!
//! **`decision::decide` 自体は変更しない。** ここは
//! `decide` が確定した `AnswerDecision`・マッチしたルール/禁止領域から「顧客に見せてよい取次
//! 項目」を導出し、下書き生成の可否と、生成された下書きが歯止めを通るかを判定する純関数の集まり。

use crate::harness::decision::{AnswerDecision, Thresholds};
use crate::harness::reply::NO_ANSWER_TOKEN;
use crate::harness::rules::{
    match_layer2, matching_layer1_rules, EscalationRule, ProhibitedDomain,
};
use crate::harness::signal::{LexiconNormalizer, Signal, SignalSet};
use crate::resolve::normalize_key;
use std::collections::HashMap;
use std::sync::OnceLock;

/// 取り次ぐ項目 1 件（design doc §2）。顧客提示用の `label`（lexicon の `customer_label`）と、
/// そのラベルを導いた signal の列を持つ。同じラベルを宣言する signal が複数あれば、全て同じ
/// `HandoffItem` の `signals` に束ねる（`derive_handoff_items` の重複除去はラベル単位）。
///
/// `signals` は `passes_handoff_safeguards` が表層形照合に使う。ラベル文字列ではなく signal の
/// `surface_forms` で照合するため、どの signal がこのラベルを導いたかをここで保持しておく
/// 必要がある（2026-10-09 改訂）。
#[derive(Debug, Clone, PartialEq)]
pub struct HandoffItem {
    pub label: String,
    pub signals: Vec<Signal>,
}

/// マッチした第1層ルールの `condition`、または第2層禁止領域の `domain_signals` から、
/// 取り次ぐ項目（design doc §2）を導出する。
///
/// - 順序は lexicon の宣言順（`SignalSet` は `BTreeSet` でアルファベット順になるため、
///   そのまま列挙すると lexicon が意図した提示順が失われる）。
/// - 同じ `customer_label` を宣言する signal は 1 件の `HandoffItem` にまとめ、その `signals`
///   に両方を積む（重複除去はラベル単位）。
/// - `customer_label_of` が `None` を返す signal（lexicon 未登録、または `customer_label` が
///   未設定）は結果に含めず、`tracing::warn!` で signal 名（内部識別子）だけを出す。質問本文・
///   顧客発話はここでは一切扱っていないので出す対象にもならない。
pub fn derive_handoff_items(
    normalizer: &LexiconNormalizer,
    signals: &SignalSet,
) -> Vec<HandoffItem> {
    let order = normalizer.declared_signal_order();
    let position_of = |name: &str| order.iter().position(|s| s == name).unwrap_or(usize::MAX);

    let mut ordered: Vec<_> = signals.iter().collect();
    ordered.sort_by_key(|signal| position_of(signal.as_str()));

    let mut items: Vec<HandoffItem> = Vec::new();
    let mut index_of_label: HashMap<String, usize> = HashMap::new();
    for signal in ordered {
        match normalizer.customer_label_of(signal) {
            Some(label) => {
                if let Some(&idx) = index_of_label.get(label) {
                    items[idx].signals.push(signal.clone());
                } else {
                    index_of_label.insert(label.to_string(), items.len());
                    items.push(HandoffItem {
                        label: label.to_string(),
                        signals: vec![signal.clone()],
                    });
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

/// 部分回答の下書きを見送った理由（design doc §3.1「見送りの記録」）。`tracing::info!` に
/// そのまま載せる値で、発話本文・signal の値は含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialAnswerSkipReason {
    /// hearing（聞き返し契約）を宣言したルールにマッチしたターン。
    HearingDeclared,
    /// 取り次ぐ項目が1つも無い。
    NoHandoffItems,
    /// 材料の関連度が `thresholds.low` 未満（または取得不可）。
    InsufficientRelevance,
    PartialAnswerDisabled,
    /// 下書き生成そのものが無効（`customer_reply_draft_enabled = false`、または LLM 未設定）。
    DraftGenerationDisabled,
}

/// [`partial_answer_eligibility`] の内部結果。公開 API（[`can_draft_partial_answer`] /
/// [`partial_answer_skip_reason`]）はこれを bool / `Option<PartialAnswerSkipReason>` へ
/// それぞれ写す。
enum PartialAnswerEligibility {
    Eligible,
    /// `Escalate` 以外（`Allowed`）、または第3層の取次。design doc §3.1 がそもそも部分回答の
    /// 対象外と定めている区分であり、「見送り」としての記録対象にもしない。
    NotApplicable,
    Ineligible(PartialAnswerSkipReason),
}

/// design doc §3.1 の条件1・2（判定が第1層・第2層の `Escalate`、hearing 宣言なし、取り次ぐ項目が
/// 1つ以上）だけを見る。[`partial_answer_eligibility`] の前半であり、§10.2 の signal 誘導補助検索の
/// 起動条件（[`is_partial_answer_candidate`]）と同じ判定を1か所に持つ。条件を満たすとき
/// `Eligible` を返す（残りの条件は呼び出し側が続けて見る）。
///
/// - `hearing` を宣言したルールにマッチしたターンは一律不可とする。spec §3.1 の条件は
///   「hearing による `Clarify` ではない」だが、`Clarify` になるかどうかは `evaluate()` の後に
///   `api.rs` の Jev（`has_enough_info`）判定で決まり、この関数の呼び出し時点では分からない。
///   そのため hearing 契約を宣言したルールにマッチしたターンは一律 `false` とする（安全側の
///   近似であり、**spec が明示した決定ではない**）。
/// - 判定が第1層または第2層の取次である。第3層（根拠不足・確信度不足）は `handoff_items` の
///   中身に依存せず、`layer` で明示的に除外する（`handoff_items_for_decision` が将来第3層で
///   非空を返しても、spec 変更なしに部分回答が有効化されないようにするため）。
fn partial_answer_structural_eligibility(
    decision: &AnswerDecision,
    handoff_items: &[HandoffItem],
) -> PartialAnswerEligibility {
    let AnswerDecision::Escalate { hearing, layer, .. } = decision else {
        return PartialAnswerEligibility::NotApplicable;
    };
    if !matches!(*layer, 1 | 2) {
        return PartialAnswerEligibility::NotApplicable;
    }
    if hearing.is_some() {
        return PartialAnswerEligibility::Ineligible(PartialAnswerSkipReason::HearingDeclared);
    }
    if handoff_items.is_empty() {
        return PartialAnswerEligibility::Ineligible(PartialAnswerSkipReason::NoHandoffItems);
    }
    PartialAnswerEligibility::Eligible
}

/// design doc §10.2: signal 誘導補助検索を行うターンかどうか（§3.1 の条件1・2を満たすか）。
/// 検索回数を増やさないため、部分回答の対象にならないターン（`Allowed`・第3層・hearing 宣言あり・
/// 取り次ぐ項目なし）では補助検索を行わない。
pub fn is_partial_answer_candidate(
    decision: &AnswerDecision,
    handoff_items: &[HandoffItem],
) -> bool {
    matches!(
        partial_answer_structural_eligibility(decision, handoff_items),
        PartialAnswerEligibility::Eligible
    )
}

fn partial_answer_eligibility(
    decision: &AnswerDecision,
    handoff_items: &[HandoffItem],
    best_manual_score: Option<f32>,
    thresholds: &Thresholds,
    partial_answer_enabled: bool,
    draft_generation_enabled: bool,
) -> PartialAnswerEligibility {
    match partial_answer_structural_eligibility(decision, handoff_items) {
        PartialAnswerEligibility::Eligible => {}
        not_eligible => return not_eligible,
    }
    if !partial_answer_enabled {
        return PartialAnswerEligibility::Ineligible(
            PartialAnswerSkipReason::PartialAnswerDisabled,
        );
    }
    if !draft_generation_enabled {
        return PartialAnswerEligibility::Ineligible(
            PartialAnswerSkipReason::DraftGenerationDisabled,
        );
    }
    if best_manual_score.is_some_and(|score| score >= thresholds.low) {
        PartialAnswerEligibility::Eligible
    } else {
        PartialAnswerEligibility::Ineligible(PartialAnswerSkipReason::InsufficientRelevance)
    }
}

pub fn can_draft_partial_answer(
    decision: &AnswerDecision,
    handoff_items: &[HandoffItem],
    best_manual_score: Option<f32>,
    thresholds: &Thresholds,
    partial_answer_enabled: bool,
    draft_generation_enabled: bool,
) -> bool {
    matches!(
        partial_answer_eligibility(
            decision,
            handoff_items,
            best_manual_score,
            thresholds,
            partial_answer_enabled,
            draft_generation_enabled,
        ),
        PartialAnswerEligibility::Eligible
    )
}

/// 部分回答の下書きを見送った理由（design doc §3.1「見送りの記録」）。第1・2層の `Escalate` で
/// [`can_draft_partial_answer`] が `false` を返したときに `Some` を返す。第3層・`Allowed`
/// （記録対象外）、および可である場合は `None`。呼び出し側（`evaluate()`）はこれが `Some` の
/// ときだけ `tracing::info!` を 1 行出す。
pub fn partial_answer_skip_reason(
    decision: &AnswerDecision,
    handoff_items: &[HandoffItem],
    best_manual_score: Option<f32>,
    thresholds: &Thresholds,
    partial_answer_enabled: bool,
    draft_generation_enabled: bool,
) -> Option<PartialAnswerSkipReason> {
    match partial_answer_eligibility(
        decision,
        handoff_items,
        best_manual_score,
        thresholds,
        partial_answer_enabled,
        draft_generation_enabled,
    ) {
        PartialAnswerEligibility::Ineligible(reason) => Some(reason),
        PartialAnswerEligibility::Eligible | PartialAnswerEligibility::NotApplicable => None,
    }
}

/// 数字（半角・全角、桁区切り「,」「，」、小数点、「万」「億」を含む）または漢数字
/// （〇一二三四五六七八九十百千万億）の並びの直後に、任意の空白を挟んで `円` / `%` / `％` が
/// 続く断定表現、または数字の並びの直前に `￥` / `¥` が付く前置き記法を検出する（design doc
/// §3.5 の3、2026-10-12 改訂。§5「既知の限界」: 単位を伴わない断定「無料です」は対象外。egress
/// gate の NG 辞書側で補う）。「3万円」「１，５４０円」「5,000 円」「1万5千円（末尾の「5千円」で
/// 検出）」「三万円」「百万円」「￥30,000」「¥30,000」を拾う。
fn monetary_assertion_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?:[0-9０-９][0-9０-９,，.]*[\s　]*[万千百億]?[\s　]*|[〇一二三四五六七八九十百千万億]+[\s　]*)(?:円|%|％)|[￥¥][\s　]*[0-9０-９][0-9０-９,，.]*",
        )
        .expect("monetary assertion regex must compile")
    })
}

/// 下書き 1 件を採用してよいかの判定（`evaluate()` が下書き生成後に 1 回呼ぶ）。
///
/// - `NO_ANSWER_TOKEN` そのもの（trim 後）は `partial_answer_ok` に関係なく**常に**不採用。
///   Allowed の下書きに混ざると、そのまま顧客へ送られる（`api.rs` の `ReplyAction::Answer`）
///   ため。
/// - §3.5 の歯止め（金額断定。2026-10-09 改訂でラベル網羅検査は廃止した）は `partial_answer_ok`
///   のときだけ課す。通常の Allowed 下書きには余計な制約を課さない。
///
/// `normalizer` は歯止め（取り次ぐ項目の表層形照合）が signal の `surface_forms` を引くために
/// 使う（[`passes_handoff_safeguards`] にそのまま渡す）。
///
/// `Err` の理由文字列に顧客発話・下書き本文は含まれない。
pub fn accept_draft(
    draft: &str,
    partial_answer_ok: bool,
    handoff_items: &[HandoffItem],
    normalizer: &LexiconNormalizer,
) -> Result<(), String> {
    if draft.trim() == NO_ANSWER_TOKEN {
        return Err(format!(
            "the draft is exactly the {NO_ANSWER_TOKEN:?} token (no answerable part found)"
        ));
    }
    if partial_answer_ok {
        passes_handoff_safeguards(draft, handoff_items, normalizer)?;
    }
    Ok(())
}

/// 判定結果に対応する取り次ぐ項目を導出する（design doc §2）。
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
) -> Vec<HandoffItem> {
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

/// 安全側の歯止め（design doc §3.5 の3・4。2026-10-09 改訂で旧2番「ラベル網羅」は廃止済み）。
/// 1番（egress gate / 取扱外型番ゲート）は呼び出し側（`Harness::draft_customer_reply` 内の
/// `egress_gate`、`api.rs` / `advisor/cs_support.rs` の `gate_customer_reply_draft`）で既に
/// 適用済みのため、ここでは対象外。
///
/// 1 つでも失敗すれば `Err`。返す理由の文字列には**顧客発話・下書き本文は含めない**。
///
/// 既知の限界（design doc §3.5(3) の定義どおりで、ここでは塞がない）: 金額検査は「取り次ぐ項目の
/// 表層形を含む文（「。」「！」「？」区切り。改行は区切りにしない）」だけを見る。表層形を含まない
/// 別の文での金額断定、可否・条件の断定（数字+単位を伴わないもの）は検出しない。これらは
/// プロンプト指示と egress gate に委ねている（design doc §3.2・§5）。
pub fn passes_handoff_safeguards(
    draft: &str,
    handoff_items: &[HandoffItem],
    normalizer: &LexiconNormalizer,
) -> Result<(), String> {
    // 4. 下書きが NO_ANSWER_TOKEN そのものでない。最初に判定する。
    if draft.trim() == NO_ANSWER_TOKEN {
        return Err(format!(
            "the draft is exactly the {NO_ANSWER_TOKEN:?} token (no answerable part found)"
        ));
    }
    for item in handoff_items.iter().filter(|item| {
        item.signals
            .iter()
            .all(|signal| normalizer.normalized_surface_forms_of(signal).is_none())
    }) {
        let names: Vec<&str> = item.signals.iter().map(Signal::as_str).collect();
        tracing::warn!(
            signals = ?names,
            "handoff item has no surface forms to check (llm_only or unregistered signal); the monetary-assertion safeguard cannot cover it"
        );
    }
    // 3. 取り次ぐ項目の signal の surface_forms（lexicon 正規化済み）を含む文
    //    （「。」「！」「？」区切り。改行は区切りにしない。2026-10-12 改訂: 「・設置工事費\n
    //    30,000円」のような箇条書きで表層形と金額が別の文に分かれて素通りするのを防ぐため、
    //    改行を区切りから外した）に、金額・数量の断定が含まれない。ラベル文字列ではなく
    //    表層形で照合するのは、LLM の言い換えに追従するため（2026-10-09 改訂）。本番で、LLM が
    //    「設置工事費は担当者がご案内します」と自然に言い換えた下書きが、ラベル
    //    「初期費用・設置工事費に関するご質問」の完全一致を要求する旧検査に落ちて全件
    //    不採用になった事故が実際の契機。
    for sentence in draft.split(['。', '！', '!', '？', '?']) {
        let normalized_sentence = normalize_key(sentence);
        let mentions_handoff_item = handoff_items.iter().any(|item| {
            item.signals.iter().any(|signal| {
                normalizer
                    .normalized_surface_forms_of(signal)
                    .is_some_and(|forms| {
                        forms
                            .iter()
                            .any(|form| normalized_sentence.contains(form.as_str()))
                    })
            })
        });
        // 金額断定の検出は正規化前の生テキストに対して行う。全角/半角数字の区別が必要であり、
        // `normalize_key` は英数字以外を除去するため、正規化後の文字列には規則表現が機能しない。
        if mentions_handoff_item && monetary_assertion_regex().is_match(sentence) {
            return Err(
                "invalid draft: a sentence mentioning a handoff item's surface form contains a \
                 monetary or quantity assertion (the sentence text itself is not included in \
                 this message)"
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

    fn signals(values: &[&str]) -> SignalSet {
        values.iter().map(|v| Signal::new(*v)).collect()
    }

    fn labels_of(items: &[HandoffItem]) -> Vec<String> {
        items.iter().map(|item| item.label.clone()).collect()
    }

    // ---- derive_handoff_items ----

    fn lexicon_with_labels() -> LexiconNormalizer {
        LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "zebra_question", "class": "context", "surface_forms": ["ゼブラ"],
                  "customer_label": "ゼブラに関するご質問" },
                { "signal": "initial_cost_question", "class": "context",
                  "surface_forms": ["初期費用", "設置費", "工事費", "工事代金"],
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
            labels_of(&items),
            vec![
                "ゼブラに関するご質問".to_string(),
                "初期費用・設置工事費に関するご質問".to_string(),
            ]
        );
    }

    #[test]
    fn derive_handoff_items_deduplicates_identical_labels_and_merges_their_signals() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "a", "class": "context", "surface_forms": ["a"], "customer_label": "同じラベル" },
                { "signal": "b", "class": "context", "surface_forms": ["b"], "customer_label": "同じラベル" }
            ] }"#,
        )
        .unwrap();
        let items = derive_handoff_items(&lex, &signals(&["a", "b"]));
        assert_eq!(items.len(), 1, "同じラベルは1件にまとまること");
        assert_eq!(items[0].label, "同じラベル");
        assert_eq!(
            items[0].signals,
            vec![Signal::new("a"), Signal::new("b")],
            "両方の signal が同じ HandoffItem に束ねられること"
        );
    }

    #[test]
    fn derive_handoff_items_omits_signals_without_a_customer_label_and_warns() {
        let lex = lexicon_with_labels();
        let (items, logs) = crate::test_support::capture_logs(|| {
            derive_handoff_items(&lex, &signals(&["zebra_question", "no_label_signal"]))
        });
        assert_eq!(labels_of(&items), vec!["ゼブラに関するご質問".to_string()]);
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

    // ---- can_draft_partial_answer / partial_answer_skip_reason ----

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

    fn handoff_items_fixture() -> Vec<HandoffItem> {
        vec![HandoffItem {
            label: "初期費用・設置工事費に関するご質問".to_string(),
            signals: vec![Signal::new("initial_cost_question")],
        }]
    }

    #[test]
    fn can_draft_partial_answer_is_true_when_every_condition_is_met() {
        assert!(can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            Some(0.6),
            &thresholds(),
            true,
            true
        ));
        assert!(can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            Some(0.9),
            &thresholds(),
            true,
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
            true,
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
            true,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_false_when_there_are_no_handoff_items() {
        assert!(!can_draft_partial_answer(
            &escalate(None),
            &[],
            Some(0.9),
            &thresholds(),
            true,
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
            true,
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
            true,
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
                    true,
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
            true,
            true
        ));
        assert!(!can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            None,
            &thresholds(),
            true,
            true
        ));
    }

    #[test]
    fn can_draft_partial_answer_is_true_for_an_already_escalated_case_when_partial_answer_is_enabled(
    ) {
        for layer in [1, 2] {
            assert!(
                can_draft_partial_answer(
                    &escalate_at(layer),
                    &handoff_items_fixture(),
                    Some(0.9),
                    &thresholds(),
                    true,
                    true
                ),
                "layer {layer} must remain eligible even for an already-escalated case"
            );
        }
    }

    #[test]
    fn can_draft_partial_answer_is_false_when_partial_answer_is_disabled() {
        assert!(!can_draft_partial_answer(
            &escalate(None),
            &handoff_items_fixture(),
            Some(0.9),
            &thresholds(),
            false,
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
            true,
            false
        ));
    }

    #[test]
    fn is_partial_answer_candidate_is_false_for_an_allowed_decision() {
        assert!(!is_partial_answer_candidate(
            &allowed(),
            &handoff_items_fixture()
        ));
    }

    #[test]
    fn is_partial_answer_candidate_is_false_for_a_layer3_escalate() {
        assert!(!is_partial_answer_candidate(
            &escalate_at(3),
            &handoff_items_fixture()
        ));
    }

    #[test]
    fn is_partial_answer_candidate_is_false_when_a_hearing_contract_is_declared() {
        assert!(!is_partial_answer_candidate(
            &escalate(Some(HearingContract::ProductAndSymptom)),
            &handoff_items_fixture()
        ));
    }

    #[test]
    fn is_partial_answer_candidate_is_false_when_there_are_no_handoff_items() {
        assert!(!is_partial_answer_candidate(&escalate(None), &[]));
    }

    #[test]
    fn is_partial_answer_candidate_is_true_for_layer1_and_layer2_escalates_with_handoff_items() {
        for layer in [1, 2] {
            assert!(
                is_partial_answer_candidate(&escalate_at(layer), &handoff_items_fixture()),
                "layer {layer} must be a candidate"
            );
        }
    }

    #[test]
    fn partial_answer_skip_reason_is_none_for_an_allowed_decision() {
        assert_eq!(
            partial_answer_skip_reason(&allowed(), &[], Some(0.9), &thresholds(), true, true),
            None
        );
    }

    #[test]
    fn partial_answer_skip_reason_is_none_for_a_layer3_escalate() {
        // 第3層は「見送り」ではなく対象外。記録対象にもしない（design doc §3.1）。
        assert_eq!(
            partial_answer_skip_reason(&escalate_at(3), &[], Some(0.9), &thresholds(), true, true),
            None
        );
    }

    #[test]
    fn partial_answer_skip_reason_is_none_when_eligible() {
        assert_eq!(
            partial_answer_skip_reason(
                &escalate(None),
                &handoff_items_fixture(),
                Some(0.9),
                &thresholds(),
                true,
                true
            ),
            None
        );
    }

    #[test]
    fn partial_answer_skip_reason_reports_hearing_declared() {
        assert_eq!(
            partial_answer_skip_reason(
                &escalate(Some(HearingContract::ProductAndSymptom)),
                &handoff_items_fixture(),
                Some(0.9),
                &thresholds(),
                true,
                true
            ),
            Some(PartialAnswerSkipReason::HearingDeclared)
        );
    }

    #[test]
    fn partial_answer_skip_reason_reports_no_handoff_items() {
        assert_eq!(
            partial_answer_skip_reason(&escalate(None), &[], Some(0.9), &thresholds(), true, true),
            Some(PartialAnswerSkipReason::NoHandoffItems)
        );
    }

    #[test]
    fn partial_answer_skip_reason_reports_insufficient_relevance() {
        assert_eq!(
            partial_answer_skip_reason(
                &escalate(None),
                &handoff_items_fixture(),
                Some(0.1),
                &thresholds(),
                true,
                true
            ),
            Some(PartialAnswerSkipReason::InsufficientRelevance)
        );
        assert_eq!(
            partial_answer_skip_reason(
                &escalate(None),
                &handoff_items_fixture(),
                None,
                &thresholds(),
                true,
                true
            ),
            Some(PartialAnswerSkipReason::InsufficientRelevance)
        );
    }

    #[test]
    fn partial_answer_skip_reason_reports_partial_answer_disabled() {
        assert_eq!(
            partial_answer_skip_reason(
                &escalate(None),
                &handoff_items_fixture(),
                Some(0.9),
                &thresholds(),
                false,
                true
            ),
            Some(PartialAnswerSkipReason::PartialAnswerDisabled)
        );
    }

    #[test]
    fn partial_answer_skip_reason_reports_draft_generation_disabled() {
        assert_eq!(
            partial_answer_skip_reason(
                &escalate(None),
                &handoff_items_fixture(),
                Some(0.9),
                &thresholds(),
                true,
                false
            ),
            Some(PartialAnswerSkipReason::DraftGenerationDisabled)
        );
    }

    // ---- passes_handoff_safeguards ----

    const LABEL: &str = "初期費用・設置工事費に関するご質問";

    /// 取り次ぐ項目の歯止めテスト専用 lexicon。design doc §3.5(3) の例と同じ surface_forms
    /// （「初期費用」「設置費」「工事費」「工事代金」）を持つ。
    fn lexicon_with_surface_forms() -> LexiconNormalizer {
        LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "initial_cost_question", "class": "context",
                  "surface_forms": ["初期費用", "設置費", "工事費", "工事代金"],
                  "customer_label": "初期費用・設置工事費に関するご質問" }
            ] }"#,
        )
        .expect("lexicon parses")
    }

    fn items_fixture() -> Vec<HandoffItem> {
        vec![HandoffItem {
            label: LABEL.to_string(),
            signals: vec![Signal::new("initial_cost_question")],
        }]
    }

    #[test]
    fn passes_handoff_safeguards_rejects_a_monetary_assertion_in_a_sentence_with_the_surface_form()
    {
        let lex = lexicon_with_surface_forms();
        let drafted = "工事費は1,540円です。";
        assert!(passes_handoff_safeguards(drafted, &items_fixture(), &lex).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_warns_but_accepts_an_item_whose_signals_have_no_surface_forms() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "unclassified_risk", "class": "hazard", "surface_forms": [],
                  "llm_only": true, "customer_label": "その他のご懸念" }
            ] }"#,
        )
        .expect("lexicon parses");
        let items = vec![HandoffItem {
            label: "その他のご懸念".to_string(),
            signals: vec![Signal::new("unclassified_risk")],
        }];
        let (result, logs) = crate::test_support::capture_logs(|| {
            passes_handoff_safeguards("月額利用料金は1台1,540円です。", &items, &lex)
        });
        assert!(result.is_ok());
        let warnings = crate::test_support::filter_warn_and_error_lines(&logs);
        assert!(
            warnings.contains("unclassified_risk"),
            "expected a warning naming the unchecked signal, got: {warnings}"
        );
    }

    #[test]
    fn passes_handoff_safeguards_does_not_warn_for_an_item_with_surface_forms() {
        let lex = lexicon_with_surface_forms();
        let (result, logs) = crate::test_support::capture_logs(|| {
            passes_handoff_safeguards("月額利用料金は1台1,540円です。", &items_fixture(), &lex)
        });
        assert!(result.is_ok());
        let warnings = crate::test_support::filter_warn_and_error_lines(&logs);
        assert!(
            !warnings.contains("no surface forms"),
            "unexpected warning for a checkable item: {warnings}"
        );
    }

    #[test]
    fn passes_handoff_safeguards_rejects_the_no_answer_token_verbatim() {
        let lex = lexicon_with_surface_forms();
        assert!(passes_handoff_safeguards(NO_ANSWER_TOKEN, &[], &lex).is_err());
        // 前後の空白は trim されるので同様に拒否される。
        assert!(passes_handoff_safeguards(&format!("  {NO_ANSWER_TOKEN}  "), &[], &lex).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_accepts_a_clean_draft() {
        let lex = lexicon_with_surface_forms();
        let drafted = "月額利用料金は1台1,540円（税込）です。初期費用・設置工事費に関する\
                        ご質問は担当者がご案内します。";
        assert!(passes_handoff_safeguards(drafted, &items_fixture(), &lex).is_ok());
    }

    #[test]
    fn passes_handoff_safeguards_is_vacuously_satisfied_when_there_are_no_handoff_items() {
        // handoff_items が空のとき、3(金額断定なし) は自動的に満たされる。
        let lex = lexicon_with_surface_forms();
        assert!(passes_handoff_safeguards("月額利用料金は1台1,540円です。", &[], &lex).is_ok());
    }

    #[test]
    fn passes_handoff_safeguards_rejects_monetary_assertions_with_unit_prefixes_widths_and_spaces()
    {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        for amount in [
            "3万円",
            "１，５４０円",
            "5,000 円",
            "10％",
            "10%",
            "1万5千円",
        ] {
            let drafted = format!("工事費は{amount}です。");
            assert!(
                passes_handoff_safeguards(&drafted, &items, &lex).is_err(),
                "{amount:?} next to the surface form must be rejected"
            );
        }
    }

    #[test]
    fn passes_handoff_safeguards_accepts_an_amount_in_a_sentence_without_the_surface_form() {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        let drafted = "月額は1,540円です。工事費は担当者がご案内します。".to_string();
        assert!(passes_handoff_safeguards(&drafted, &items, &lex).is_ok());
    }

    #[test]
    fn passes_handoff_safeguards_rejects_an_amount_stated_using_a_shorter_surface_form_in_a_later_sentence(
    ) {
        // 旧仕様（ラベル文字列の完全一致）はラベル全文（「初期費用・設置工事費に関するご質問」）
        // と異なる略称（「初期費用」単体）での金額断定を見逃していた。新仕様（表層形照合）は
        // この略称自体が signal の surface_form なので検出する（旧仕様からの改善）。
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        let drafted = "工事費は担当者がご案内します。初期費用は1,540円です。";
        assert!(passes_handoff_safeguards(drafted, &items, &lex).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_known_limit_does_not_detect_a_non_monetary_assertion() {
        // design doc §5 既知の限界: 数字 + 単位を伴わない断定（「無料です」等）は検出しない。
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        let drafted = "工事費は無料です。";
        // これは仕様上の既知の限界であり、望ましい挙動として固定しているのではない。
        assert!(passes_handoff_safeguards(drafted, &items, &lex).is_ok());
    }

    #[test]
    fn passes_handoff_safeguards_no_longer_treats_a_newline_as_a_sentence_boundary() {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        let drafted = "工事費は担当者がご案内します\n月額は1,540円です。";
        assert!(passes_handoff_safeguards(drafted, &items, &lex).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_rejects_a_monetary_assertion_following_a_newline_in_the_same_sentence(
    ) {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        let drafted = format!("{LABEL}は担当者がご案内します\n工事費は3万円です。");
        assert!(passes_handoff_safeguards(&drafted, &items, &lex).is_err());
    }

    #[test]
    fn passes_handoff_safeguards_rejects_kanji_numeral_amounts_and_bulleted_or_wrapped_amounts() {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        for drafted in [
            "工事費は三万円です。",
            "初期費用は百万円です。",
            "・設置工事費\n30,000円",
            "工事費は\n3万円です。",
            "工事費は1.5万円です。",
            "工事費は３０，０００円です。",
        ] {
            assert!(
                passes_handoff_safeguards(drafted, &items, &lex).is_err(),
                "{drafted:?} must be rejected"
            );
        }
    }

    #[test]
    fn passes_handoff_safeguards_rejects_yen_prefixed_amounts_across_widths_separators_and_spaces()
    {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        for drafted in [
            "工事費は￥30,000です。",
            "工事費は¥30,000です。",
            "工事費は￥３０，０００です。",
            "工事費は¥1.5万です。",
            "工事費は￥ 30,000です。",
            "工事費は¥　30,000です。",
        ] {
            assert!(
                passes_handoff_safeguards(drafted, &items, &lex).is_err(),
                "{drafted:?} must be rejected"
            );
        }
    }

    #[test]
    fn passes_handoff_safeguards_known_limit_does_not_detect_a_postfixed_yen_sign() {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        for drafted in ["工事費は5,000￥です。", "工事費は5,000¥です。"] {
            assert!(
                passes_handoff_safeguards(drafted, &items, &lex).is_ok(),
                "{drafted:?} is a known limit and must be accepted"
            );
        }
    }

    #[test]
    fn passes_handoff_safeguards_accepts_a_yen_prefixed_amount_in_a_sentence_without_the_surface_form(
    ) {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        let drafted = "月額は￥1,540です。設置工事費は別途です。";
        assert!(passes_handoff_safeguards(drafted, &items, &lex).is_ok());
    }

    #[test]
    fn passes_handoff_safeguards_accepts_amount_sentences_separated_by_a_terminator_other_than_a_period(
    ) {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        for drafted in [
            "設置工事費用は別途かかります。月額は1,540円です。",
            "月額1,540円です！設置工事費は別途です。",
            "月額1,540円ですか？設置工事費は別途です。",
            "月額1,540円です?設置工事費は別途です。",
            "月額1,540円です!設置工事費は別途です。",
        ] {
            assert!(
                passes_handoff_safeguards(drafted, &items, &lex).is_ok(),
                "{drafted:?} must be accepted"
            );
        }
    }

    #[test]
    fn passes_handoff_safeguards_reports_the_no_answer_token_as_the_reason() {
        let lex = lexicon_with_surface_forms();
        let err = passes_handoff_safeguards(NO_ANSWER_TOKEN, &items_fixture(), &lex).unwrap_err();
        assert!(
            err.contains(NO_ANSWER_TOKEN),
            "the reason must name the NO_ANSWER token: {err}"
        );
    }

    /// 2026-10-09 本番回帰: LLM が「設置工事費は担当者がご案内します」と自然に言い換えた下書きが、
    /// ラベル「初期費用・設置工事費に関するご質問」の完全一致を要求する旧検査
    /// （§3.5(2)、2026-10-09 廃止）に落ち、全件 `reason=a handoff item label is missing from
    /// the draft` で不採用になった。旧検査は廃止済みであり、ラベル文字列を一度も使わない下書き
    /// でも、表層形を含む文に金額断定が無ければ採用されることを固定する。
    #[test]
    fn passes_handoff_safeguards_accepts_a_draft_that_never_uses_the_customer_label_verbatim() {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        let drafted = "月額利用料金は1台1,540円（税込）です。設置工事費は担当者がご案内します。";
        assert!(
            !drafted.contains(LABEL),
            "precondition: the draft never contains the literal customer_label"
        );
        assert!(passes_handoff_safeguards(drafted, &items, &lex).is_ok());
    }

    // ---- accept_draft ----

    #[test]
    fn accept_draft_rejects_no_answer_token_for_a_non_partial_draft() {
        let lex = lexicon_with_surface_forms();
        assert!(accept_draft(NO_ANSWER_TOKEN, false, &[], &lex).is_err());
        assert!(accept_draft(&format!(" {NO_ANSWER_TOKEN}\n"), false, &[], &lex).is_err());
    }

    #[test]
    fn accept_draft_imposes_no_handoff_safeguards_on_a_non_partial_draft() {
        // 表層形を含む文に金額があっても、通常の Allowed 下書きは採用される。
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        assert!(accept_draft("工事費は1,540円です。", false, &items, &lex).is_ok());
    }

    #[test]
    fn accept_draft_rejects_a_partial_draft_with_a_monetary_assertion_on_the_surface_form() {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        assert!(accept_draft("工事費は1,540円です。", true, &items, &lex).is_err());
    }

    #[test]
    fn accept_draft_accepts_a_clean_partial_draft() {
        let lex = lexicon_with_surface_forms();
        let items = items_fixture();
        let drafted = "月額は1,540円です。工事費は担当者がご案内します。";
        assert!(accept_draft(drafted, true, &items, &lex).is_ok());
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
            labels_of(&items),
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
        assert_eq!(labels_of(&items), vec!["法的なご相談".to_string()]);
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
