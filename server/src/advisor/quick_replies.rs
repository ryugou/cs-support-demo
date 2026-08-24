//! quick_replies 生成(design doc `2026-08-17-homesec-advisor-design.md` §3.3・Issue #34
//! カルーセル→Flex 移行に伴う追加、ユーザー指定語彙)。
//!
//! 生成は**コードの決定論のみ**(LLM を使わない)。`clarify` ターンは尋ねた条件キーの語彙
//! 選択肢、`time_pref` ターンは営業時間内の固定スロット、それ以外のターンでは付けない。
//! `api.rs` が応答種別確定後に呼ぶ。

use crate::advisor::draftgen::{ClosingKind, DraftMeta};
use crate::advisor::understand::ConditionKey;
use serde::Serialize;
use std::collections::HashSet;

/// LINE quick reply 1件(design doc §3.3)。フィールド名は JSON 例(`label` / `message`)と
/// 完全一致しているため `#[serde(rename = ...)]` は不要。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuickReplyItem {
    pub label: String,
    pub message: String,
}

/// quick reply の送出上限件数。LINE 仕様上の上限(13件)より狭く運用する。2026-08-21
/// conversation-rhythm-implementation §要件5 により 6 → 4 へ縮小(本番実害 (d): チップと
/// 質問の連発が尋問的、への是正の一環。選択肢の数自体を絞ることでタップの認知負荷を下げる)。
const MAX_QUICK_REPLIES: usize = 4;

/// `label` の文字数上限(文字数、`chars().count()`)。語彙表の全項目は 20 字以内に収まるが、
/// 将来語彙を足したときに無検査で LINE へ送って 400 を招かないよう、常にここで検査する。
const LABEL_MAX_CHARS: usize = 20;

/// `for_answer` が受け取る LLM 由来の `choices` 候補1件あたりの文字数上限(2次 codex レビュー
/// Warning D)。spec の「短い回答候補(各20字目安)」を踏まえつつ、`label` の20字切り詰めとは
/// 別に「message として次ターンへ送り返すには長すぎる」候補そのものを破棄する判断に使うため、
/// 余裕を持たせた値にする(`label` の20字上限より広い。20字ちょうどで切ると目安を僅かに
/// 超えただけの正当な候補まで捨ててしまうため)。
const CHOICE_MAX_CHARS: usize = 100;

fn item(label: &str, message: &str) -> QuickReplyItem {
    QuickReplyItem {
        label: truncate_label(label),
        message: message.to_string(),
    }
}

/// `label` を [`LABEL_MAX_CHARS`] へ切り詰める(文字数、バイト数ではない)。
fn truncate_label(label: &str) -> String {
    if label.chars().count() <= LABEL_MAX_CHARS {
        label.to_string()
    } else {
        label.chars().take(LABEL_MAX_CHARS).collect()
    }
}

/// concern 語彙(仕様書指定の5件、この順序)。
fn concern_vocabulary() -> Vec<QuickReplyItem> {
    vec![
        item("侵入・空き巣が心配", "侵入や空き巣が心配です"),
        item("留守中の見守り", "留守中の様子を見守りたいです"),
        item("宅配便の盗難防止", "宅配便の盗難が心配です"),
        item("ストーカー・不審者対策", "ストーカーや不審者が心配です"),
        item("火災・災害対策", "火災や災害が心配です"),
    ]
}

/// housing 語彙(仕様書指定の4件、この順序)。
fn housing_vocabulary() -> Vec<QuickReplyItem> {
    vec![
        item("一戸建て(持ち家)", "持ち家の一戸建てです"),
        item("一戸建て(賃貸)", "賃貸の一戸建てです"),
        item(
            "マンション・アパート(持ち家)",
            "持ち家のマンション・アパートです",
        ),
        item(
            "マンション・アパート(賃貸)",
            "賃貸のマンション・アパートです",
        ),
    ]
}

/// time_pref 語彙(仕様書指定の3件、この順序)。
fn time_pref_vocabulary() -> Vec<QuickReplyItem> {
    vec![
        item("平日10-12時", "平日10時から12時にお願いします"),
        item("平日13-15時", "平日13時から15時にお願いします"),
        item("平日16-18時", "平日16時から18時にお願いします"),
    ]
}

/// [`MAX_QUICK_REPLIES`] 件へ切り詰める(現行の3語彙表はどれも上限未満だが、将来語彙が
/// 増えても無検査で LINE 仕様を超えないための安全弁)。
fn cap(items: Vec<QuickReplyItem>) -> Vec<QuickReplyItem> {
    items.into_iter().take(MAX_QUICK_REPLIES).collect()
}

/// `clarify` ターンの quick_replies。`missing.first()`(design doc §4.3 手順6の
/// `missing_conditions` は常に0〜1要素を返す契約)だけを見る。`Concern` / `Housing` 以外
/// (未取得なし、または将来語彙が増えた場合)は `None`。
pub fn for_clarify(missing: &[ConditionKey]) -> Option<Vec<QuickReplyItem>> {
    match missing.first() {
        Some(ConditionKey::Concern) => Some(cap(concern_vocabulary())),
        Some(ConditionKey::Housing) => Some(cap(housing_vocabulary())),
        _ => None,
    }
}

/// `time_pref` ターン(`LeadSolicit` / `TimePrefContinue` の両方)の quick_replies。
pub fn for_time_pref() -> Option<Vec<QuickReplyItem>> {
    Some(cap(time_pref_vocabulary()))
}

/// `answer` ターンの quick_replies(2026-08-21 conversation-rhythm-implementation §要件5)。
///
/// LLM Call#2 のメタ(`DraftMeta`)が `closing == QuestionChoice` かつ `choices` が非空のときだけ
/// 選択肢チップを出す。`choices` は LLM 出力(信頼できない入力)であるため、`missing`/`housing`
/// 等の固定語彙表と異なりコード側で検証してから使う。タップ後にこの `message` は顧客発話として
/// 次ターンへ入力されるため、検証は次の順序で行う(2次 codex レビュー Warning D 是正):
/// 1. 各候補をまず `trim()` して正規化済みの値を1つ作る。以降の破棄判定・重複除去・
///    `label`/`message` の生成は**すべてこの正規化済みの値**を使う(旧実装は重複除去だけ
///    `trim()` 前の値で行っていたため、`"はい"` と `" はい "` が別候補として2つのチップに
///    なっていた)。
/// 2. 次のいずれかに該当する候補は要素ごと破棄する(全体を落とさない): 正規化後が空、
///    制御文字(改行・タブを含む)を含む、正規化後の文字数が [`CHOICE_MAX_CHARS`] を超える。
///    破棄が発生したら件数だけを `tracing::warn!` に出す(候補本文は顧客向け文言のため
///    ログに出さない)。
/// 3. 重複除去(順序維持)→ [`MAX_QUICK_REPLIES`] 件へ切り詰め → 各項目を
///    [`truncate_label`] で20字へ切り詰める(`message` は正規化済みの値をそのまま使う)。
///
/// `closing == QuestionOpen` / `closing == Proposal`、`meta` が `None`(Call#2 がメタ付きで
/// 成功しなかった、または `DraftMode::Clarify` のターン)のいずれでも `None` を返す
/// (本番実害 (d) 「チップと質問の連発が尋問的」への是正: 開いた質問や提案のターンにまで
/// チップを付けない)。有効な候補が0件になった場合も `None` を返す(既存挙動を維持)。
pub fn for_answer(meta: Option<&DraftMeta>) -> Option<Vec<QuickReplyItem>> {
    let meta = meta?;
    if meta.closing != ClosingKind::QuestionChoice || meta.choices.is_empty() {
        return None;
    }
    let total = meta.choices.len();
    let normalized: Vec<&str> = meta.choices.iter().map(|s| s.trim()).collect();
    let valid: Vec<&str> = normalized
        .into_iter()
        .filter(|choice| is_valid_answer_choice(choice))
        .collect();
    let discarded = total - valid.len();
    if discarded > 0 {
        tracing::warn!(
            route = "advisor_quick_replies",
            discarded_count = discarded,
            total_count = total,
            "advisor for_answer discarded one or more LLM-provided quick reply choices \
             (blank after trimming, containing control characters, or exceeding the length \
             limit); choice text is not logged since it is customer-facing"
        );
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let deduped: Vec<&str> = valid
        .into_iter()
        .filter(|choice| seen.insert(*choice))
        .collect();
    if deduped.is_empty() {
        return None;
    }
    Some(cap(deduped
        .into_iter()
        .map(|choice| item(choice, choice))
        .collect()))
}

/// [`for_answer`] の破棄判定(`choice` は既に `trim()` 済みの値を渡すこと)。
fn is_valid_answer_choice(choice: &str) -> bool {
    !choice.is_empty()
        && !choice.chars().any(char::is_control)
        && choice.chars().count() <= CHOICE_MAX_CHARS
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 語彙表(必須テスト5: label/message が仕様どおりであること) ---

    #[test]
    fn concern_vocabulary_has_5_items_in_the_specified_order() {
        let items = concern_vocabulary();
        let expected = [
            ("侵入・空き巣が心配", "侵入や空き巣が心配です"),
            ("留守中の見守り", "留守中の様子を見守りたいです"),
            ("宅配便の盗難防止", "宅配便の盗難が心配です"),
            ("ストーカー・不審者対策", "ストーカーや不審者が心配です"),
            ("火災・災害対策", "火災や災害が心配です"),
        ];
        assert_eq!(items.len(), 5);
        for (item, (label, message)) in items.iter().zip(expected.iter()) {
            assert_eq!(item.label, *label);
            assert_eq!(item.message, *message);
        }
    }

    #[test]
    fn housing_vocabulary_has_4_items_in_the_specified_order() {
        let items = housing_vocabulary();
        let expected = [
            ("一戸建て(持ち家)", "持ち家の一戸建てです"),
            ("一戸建て(賃貸)", "賃貸の一戸建てです"),
            (
                "マンション・アパート(持ち家)",
                "持ち家のマンション・アパートです",
            ),
            (
                "マンション・アパート(賃貸)",
                "賃貸のマンション・アパートです",
            ),
        ];
        assert_eq!(items.len(), 4);
        for (item, (label, message)) in items.iter().zip(expected.iter()) {
            assert_eq!(item.label, *label);
            assert_eq!(item.message, *message);
        }
    }

    #[test]
    fn time_pref_vocabulary_has_3_items_in_the_specified_order() {
        let items = time_pref_vocabulary();
        let expected = [
            ("平日10-12時", "平日10時から12時にお願いします"),
            ("平日13-15時", "平日13時から15時にお願いします"),
            ("平日16-18時", "平日16時から18時にお願いします"),
        ];
        assert_eq!(items.len(), 3);
        for (item, (label, message)) in items.iter().zip(expected.iter()) {
            assert_eq!(item.label, *label);
            assert_eq!(item.message, *message);
        }
    }

    // --- for_clarify ---

    #[test]
    fn for_clarify_returns_concern_vocabulary_when_missing_first_is_concern() {
        // concern_vocabulary は5件だが、MAX_QUICK_REPLIES(=4、要件5)により先頭4件へ
        // 切り詰められる(cap() 経由)。仕様変更に伴う正当な追随(意味は変えない: 語彙表
        // 自体が5件であることは concern_vocabulary_has_5_items_in_the_specified_order が
        // 別途固定している)。
        let result = for_clarify(&[ConditionKey::Concern]);
        let expected: Vec<QuickReplyItem> = concern_vocabulary().into_iter().take(4).collect();
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn for_clarify_returns_housing_vocabulary_when_missing_first_is_housing() {
        let result = for_clarify(&[ConditionKey::Housing]);
        assert_eq!(result, Some(housing_vocabulary()));
    }

    #[test]
    fn for_clarify_only_looks_at_the_first_element() {
        // missing_conditions(decide.rs)は常に0〜1要素を返す契約だが、防御的に2要素目以降は
        // 無視することを固定する。件数を4件に切り詰める理由は上のテストと同じ(要件5)。
        let result = for_clarify(&[ConditionKey::Concern, ConditionKey::Housing]);
        let expected: Vec<QuickReplyItem> = concern_vocabulary().into_iter().take(4).collect();
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn for_clarify_returns_none_when_missing_is_empty() {
        assert_eq!(for_clarify(&[]), None);
    }

    #[test]
    fn for_clarify_returns_none_when_missing_first_is_neither_concern_nor_housing() {
        assert_eq!(for_clarify(&[ConditionKey::Budget]), None);
    }

    // --- for_time_pref ---

    #[test]
    fn for_time_pref_returns_the_time_pref_vocabulary() {
        assert_eq!(for_time_pref(), Some(time_pref_vocabulary()));
    }

    // --- truncate_label(必須テスト5: 20字切り詰め) ---

    #[test]
    fn truncate_label_keeps_a_label_at_exactly_20_chars_untouched() {
        let label: String = "あ".repeat(20);
        assert_eq!(truncate_label(&label), label);
    }

    #[test]
    fn truncate_label_truncates_a_label_over_20_chars() {
        let label: String = "あ".repeat(25);
        let truncated = truncate_label(&label);
        assert_eq!(truncated.chars().count(), 20);
        assert_eq!(truncated, "あ".repeat(20));
    }

    // --- cap(必須テスト5: 上限4件、要件5で6件から縮小) ---

    #[test]
    fn cap_limits_to_4_items() {
        let items: Vec<QuickReplyItem> = (0..8)
            .map(|i| item(&format!("項目{i}"), &format!("メッセージ{i}")))
            .collect();
        let capped = cap(items);
        assert_eq!(capped.len(), 4);
        assert_eq!(capped[0].label, "項目0");
        assert_eq!(capped[3].label, "項目3");
    }

    #[test]
    fn cap_keeps_all_items_when_under_the_limit() {
        let items = vec![item("A", "a"), item("B", "b")];
        let capped = cap(items);
        assert_eq!(capped.len(), 2);
    }

    // --- for_answer(2026-08-21 conversation-rhythm-implementation §要件5、必須テスト3:
    // closing別の出し分け) ---

    fn meta(closing: ClosingKind, choices: &[&str]) -> DraftMeta {
        DraftMeta {
            featured: Vec::new(),
            closing,
            choices: choices.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn for_answer_returns_choices_when_closing_is_question_choice() {
        let m = meta(ClosingKind::QuestionChoice, &["侵入が心配", "留守中が心配"]);
        let result = for_answer(Some(&m)).expect("QuestionChoice with non-empty choices");
        assert_eq!(
            result,
            vec![
                QuickReplyItem {
                    label: "侵入が心配".to_string(),
                    message: "侵入が心配".to_string(),
                },
                QuickReplyItem {
                    label: "留守中が心配".to_string(),
                    message: "留守中が心配".to_string(),
                },
            ]
        );
    }

    #[test]
    fn for_answer_returns_none_when_closing_is_question_open() {
        let m = meta(ClosingKind::QuestionOpen, &[]);
        assert_eq!(
            for_answer(Some(&m)),
            None,
            "QuestionOpen must never produce quick_replies (本番実害(d)の是正)"
        );
    }

    #[test]
    fn for_answer_returns_none_when_closing_is_proposal() {
        let m = meta(ClosingKind::Proposal, &[]);
        assert_eq!(
            for_answer(Some(&m)),
            None,
            "Proposal must never produce quick_replies"
        );
    }

    #[test]
    fn for_answer_returns_none_when_meta_is_none() {
        assert_eq!(
            for_answer(None),
            None,
            "no meta (Call#2 did not succeed with a structured meta) means no quick_replies"
        );
    }

    #[test]
    fn for_answer_returns_none_when_closing_is_question_choice_but_choices_is_empty() {
        let m = meta(ClosingKind::QuestionChoice, &[]);
        assert_eq!(
            for_answer(Some(&m)),
            None,
            "QuestionChoice with no choices must not produce an empty quick_replies list"
        );
    }

    #[test]
    fn for_answer_deduplicates_choices_preserving_order() {
        let m = meta(
            ClosingKind::QuestionChoice,
            &["侵入が心配", "留守中が心配", "侵入が心配"],
        );
        let result = for_answer(Some(&m)).expect("non-empty choices");
        assert_eq!(
            result.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            vec!["侵入が心配", "留守中が心配"]
        );
    }

    #[test]
    fn for_answer_caps_choices_to_max_quick_replies() {
        let choices = ["選択肢1", "選択肢2", "選択肢3", "選択肢4", "選択肢5"];
        let m = meta(ClosingKind::QuestionChoice, &choices);
        let result = for_answer(Some(&m)).expect("non-empty choices");
        assert_eq!(result.len(), 4, "must be capped to MAX_QUICK_REPLIES(=4)");
    }

    // --- reviewer 指摘 Warning 3: 空白のみ・空文字の choices を落とす ---

    #[test]
    fn for_answer_discards_blank_and_whitespace_only_choices() {
        let m = meta(
            ClosingKind::QuestionChoice,
            &["侵入が心配", "", "   ", "留守中が心配"],
        );
        let result = for_answer(Some(&m)).expect("at least one non-blank choice remains");
        assert_eq!(
            result.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            vec!["侵入が心配", "留守中が心配"],
            "blank/whitespace-only choices must never surface as an empty label/message item"
        );
    }

    #[test]
    fn for_answer_returns_none_when_all_choices_are_blank_or_whitespace_only() {
        let m = meta(ClosingKind::QuestionChoice, &["", "   ", "\t"]);
        assert_eq!(
            for_answer(Some(&m)),
            None,
            "an empty result after filtering blanks must yield None, not an empty Vec"
        );
    }

    #[test]
    fn for_answer_truncates_each_choice_label_to_20_chars_but_keeps_the_message_untouched() {
        let long_choice = "あ".repeat(25);
        let m = meta(ClosingKind::QuestionChoice, &[&long_choice]);
        let result = for_answer(Some(&m)).expect("non-empty choices");
        assert_eq!(result[0].label.chars().count(), 20);
        assert_eq!(
            result[0].message, long_choice,
            "message must carry the original (untruncated) choice text"
        );
    }

    // --- 2次 codex レビュー Warning D: choices の正規化が不十分だった穴の是正 ---

    #[test]
    fn for_answer_merges_duplicates_that_only_differ_by_surrounding_whitespace() {
        // 旧実装は重複除去を trim() 前の値で行っていたため、"はい" と " はい " が別候補として
        // 2件のチップになっていた。正規化(trim)を重複除去より前に行うことを固定する。
        let m = meta(ClosingKind::QuestionChoice, &["はい", " はい ", "いいえ"]);
        let result = for_answer(Some(&m)).expect("non-empty choices");
        assert_eq!(
            result.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            vec!["はい", "いいえ"],
            "trim-equivalent duplicates must collapse into a single chip"
        );
    }

    #[test]
    fn for_answer_discards_a_choice_containing_a_control_character() {
        // タップ後にこの message は顧客発話として次ターンへ入力される。改行・タブ等の制御
        // 文字を含む候補は要素ごと破棄することを固定する。
        let m = meta(
            ClosingKind::QuestionChoice,
            &["侵入が心配", "見守り\nしたい", "留守中が心配"],
        );
        let result = for_answer(Some(&m)).expect("non-blank choices remain");
        assert_eq!(
            result.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            vec!["侵入が心配", "留守中が心配"],
            "a choice containing a control character (newline) must be discarded outright"
        );
    }

    #[test]
    fn for_answer_discards_a_choice_exceeding_the_length_limit() {
        let too_long = "あ".repeat(CHOICE_MAX_CHARS + 1);
        let m = meta(ClosingKind::QuestionChoice, &["侵入が心配", &too_long]);
        let result = for_answer(Some(&m)).expect("the short choice remains");
        assert_eq!(
            result.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
            vec!["侵入が心配"],
            "a choice longer than CHOICE_MAX_CHARS must be discarded outright"
        );
    }

    #[test]
    fn for_answer_returns_none_when_every_choice_is_discarded() {
        let too_long = "あ".repeat(CHOICE_MAX_CHARS + 1);
        let m = meta(ClosingKind::QuestionChoice, &["", "\n", &too_long]);
        assert_eq!(
            for_answer(Some(&m)),
            None,
            "an empty result after discarding invalid choices must yield None, not an empty Vec"
        );
    }
}
