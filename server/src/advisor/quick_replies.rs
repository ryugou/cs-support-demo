//! quick_replies 生成(design doc `2026-08-17-homesec-advisor-design.md` §3.3・Issue #34
//! カルーセル→Flex 移行に伴う追加、ユーザー指定語彙)。
//!
//! 生成は**コードの決定論のみ**(LLM を使わない)。`clarify` ターンは尋ねた条件キーの語彙
//! 選択肢、`time_pref` ターンは営業時間内の固定スロット、それ以外のターンでは付けない。
//! `api.rs` が応答種別確定後に呼ぶ。

use crate::advisor::understand::ConditionKey;
use serde::Serialize;

/// LINE quick reply 1件(design doc §3.3)。フィールド名は JSON 例(`label` / `message`)と
/// 完全一致しているため `#[serde(rename = ...)]` は不要。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QuickReplyItem {
    pub label: String,
    pub message: String,
}

/// quick reply の送出上限件数。LINE 仕様上の上限(13件)より狭く運用する。
const MAX_QUICK_REPLIES: usize = 6;

/// `label` の文字数上限(文字数、`chars().count()`)。語彙表の全項目は 20 字以内に収まるが、
/// 将来語彙を足したときに無検査で LINE へ送って 400 を招かないよう、常にここで検査する。
const LABEL_MAX_CHARS: usize = 20;

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
        let result = for_clarify(&[ConditionKey::Concern]);
        assert_eq!(result, Some(concern_vocabulary()));
    }

    #[test]
    fn for_clarify_returns_housing_vocabulary_when_missing_first_is_housing() {
        let result = for_clarify(&[ConditionKey::Housing]);
        assert_eq!(result, Some(housing_vocabulary()));
    }

    #[test]
    fn for_clarify_only_looks_at_the_first_element() {
        // missing_conditions(decide.rs)は常に0〜1要素を返す契約だが、防御的に2要素目以降は
        // 無視することを固定する。
        let result = for_clarify(&[ConditionKey::Concern, ConditionKey::Housing]);
        assert_eq!(result, Some(concern_vocabulary()));
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

    // --- cap(必須テスト5: 上限6件) ---

    #[test]
    fn cap_limits_to_6_items() {
        let items: Vec<QuickReplyItem> = (0..8)
            .map(|i| item(&format!("項目{i}"), &format!("メッセージ{i}")))
            .collect();
        let capped = cap(items);
        assert_eq!(capped.len(), 6);
        assert_eq!(capped[0].label, "項目0");
        assert_eq!(capped[5].label, "項目5");
    }

    #[test]
    fn cap_keeps_all_items_when_under_the_limit() {
        let items = vec![item("A", "a"), item("B", "b")];
        let capped = cap(items);
        assert_eq!(capped.len(), 2);
    }
}
