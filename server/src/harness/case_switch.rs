//! Issue #61: 相談対象の切り替えによる case と履歴のリセット。
//!
//! 正本: `docs/superpowers/specs/2026-10-06-case-reset-on-topic-switch-design.md` §2.1・§2.2。
//! ここに置くのは **決定論の純関数のみ**（LLM を使わない）。製品参照そのものの抽出（LLM）は
//! `extraction::ProductReferenceExtractor` が担い、この判定は抽出済みの
//! [`crate::harness::product_gate::ProductReference`] を受け取るだけ。新 case の作成・
//! `previous_case_id` の書き込み・監査記録などの I/O は `Harness::record_case_switch`
//! （`harness/mod.rs`）と `api.rs::reply_handler` が担う。

use super::product_gate::{ProductReference, ProductReferenceResolution};
use std::collections::BTreeSet;

/// 今ターンの製品参照のうち `resolution == Matched` の `matched_model` だけを、trim 後
/// 空文字を除いて取り出す（design doc §2.1: 「`Foreign` と `Ambiguous` は加えない」）。
pub(crate) fn matched_model_strings(current_turn_refs: &[ProductReference]) -> Vec<String> {
    current_turn_refs
        .iter()
        .filter(|r| r.resolution == ProductReferenceResolution::Matched)
        .filter_map(|r| r.matched_model.as_deref())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// 製品切り替えの検知規則そのもの（design doc §2.2、決定論・LLM を使わない）。
///
/// 次をすべて満たすときだけ `true`:
/// - `case_product_models`（その case でこれまでに確定した取扱製品の型番）が空でない
/// - 今ターンの `current_turn_refs` に `Matched` が 1 件以上ある
/// - 今ターンの `Matched` の型番のいずれも `case_product_models` に含まれない
///
/// 次はすべて `false`（design doc §2.2 の非切り替え4条件）:
/// - `case_product_models` が空（これまで型番が出ていない）
/// - 今ターンに `Matched` が無い（型番に触れない発話、または取扱外の型番だけが出た。
///   `Foreign` / `Ambiguous` はこの関数が内部で除外するので、呼び出し側で事前に filter する
///   必要は無い）
/// - 今ターンの `Matched` に、`case_product_models` に含まれる型番が1つでもある（既知の製品
///   との比較は同じ相談とみなす）
pub fn is_product_switch(
    case_product_models: &[String],
    current_turn_refs: &[ProductReference],
) -> bool {
    if case_product_models.is_empty() {
        return false;
    }
    let matched = matched_model_strings(current_turn_refs);
    if matched.is_empty() {
        return false;
    }
    matched
        .iter()
        .all(|m| !case_product_models.iter().any(|c| c == m))
}

/// case の `product_models` 属性（CSV）に、今ターンの `Matched` 型番を加えて書き戻す値を
/// 組み立てる（design doc §2.1）。既存の CSV をパースし、今ターンの `Matched` だけを加え、
/// 辞書順でソート・重複排除して再度 CSV にする。`Foreign` / `Ambiguous` は加えない。
pub fn merge_product_models(existing_csv: &str, current_turn_refs: &[ProductReference]) -> String {
    let mut set: BTreeSet<String> = super::knowledge::csv_list(existing_csv)
        .into_iter()
        .collect();
    set.extend(matched_model_strings(current_turn_refs));
    set.into_iter().collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched(model: &str) -> ProductReference {
        ProductReference {
            surface: model.to_string(),
            resolution: ProductReferenceResolution::Matched,
            matched_model: Some(model.to_string()),
        }
    }

    fn foreign(surface: &str) -> ProductReference {
        ProductReference {
            surface: surface.to_string(),
            resolution: ProductReferenceResolution::Foreign,
            matched_model: None,
        }
    }

    fn ambiguous(surface: &str) -> ProductReference {
        ProductReference {
            surface: surface.to_string(),
            resolution: ProductReferenceResolution::Ambiguous,
            matched_model: None,
        }
    }

    // ---- is_product_switch: 切り替えと判定するケース ----

    #[test]
    fn is_product_switch_detects_a_newly_matched_model_absent_from_the_case() {
        let case_models = vec!["ADC-V724".to_string()];
        let refs = vec![matched("ADC-V523")];
        assert!(is_product_switch(&case_models, &refs));
    }

    // ---- is_product_switch: design doc §2.2 の非切り替え4条件 ----

    #[test]
    fn is_product_switch_is_false_when_no_matched_reference_this_turn() {
        let case_models = vec!["ADC-V724".to_string()];
        let refs: Vec<ProductReference> = vec![];
        assert!(!is_product_switch(&case_models, &refs));
    }

    #[test]
    fn is_product_switch_is_false_when_a_matched_model_is_already_known_to_the_case() {
        let case_models = vec!["ADC-V724".to_string()];
        // 既知の型番(ADC-V724)との比較を兼ねた発話。新しい型番(ADC-V523)にも触れているが、
        // 既知の型番が1つでもあれば同じ相談とみなす(design doc §2.2)。
        let refs = vec![matched("ADC-V724"), matched("ADC-V523")];
        assert!(!is_product_switch(&case_models, &refs));
    }

    #[test]
    fn is_product_switch_is_false_when_the_case_has_no_recorded_product_yet() {
        let case_models: Vec<String> = vec![];
        let refs = vec![matched("ADC-V523")];
        assert!(!is_product_switch(&case_models, &refs));
    }

    #[test]
    fn is_product_switch_is_false_when_only_an_out_of_scope_model_is_mentioned() {
        let case_models = vec!["ADC-V724".to_string()];
        let refs = vec![foreign("他社製品X")];
        assert!(!is_product_switch(&case_models, &refs));
    }

    #[test]
    fn is_product_switch_ignores_ambiguous_references() {
        let case_models = vec!["ADC-V724".to_string()];
        let refs = vec![ambiguous("カメラ")];
        assert!(!is_product_switch(&case_models, &refs));
    }

    // ---- merge_product_models ----

    #[test]
    fn merge_product_models_adds_only_matched_models_in_dictionary_order() {
        let refs = vec![
            matched("ADC-V724"),
            foreign("他社製品X"),
            ambiguous("カメラ"),
            matched("ADC-V523"),
        ];
        assert_eq!(merge_product_models("", &refs), "ADC-V523,ADC-V724");
    }

    #[test]
    fn merge_product_models_merges_with_existing_csv_and_dedupes() {
        let refs = vec![matched("ADC-V523"), matched("ADC-V724")];
        assert_eq!(
            merge_product_models("ADC-V724,ADC-V900", &refs),
            "ADC-V523,ADC-V724,ADC-V900"
        );
    }

    #[test]
    fn merge_product_models_is_unchanged_when_no_matched_reference_this_turn() {
        let refs: Vec<ProductReference> = vec![foreign("他社製品X")];
        assert_eq!(merge_product_models("ADC-V724", &refs), "ADC-V724");
    }

    #[test]
    fn merge_product_models_trims_whitespace_in_matched_model() {
        let refs = vec![ProductReference {
            surface: "ADC-V724".to_string(),
            resolution: ProductReferenceResolution::Matched,
            matched_model: Some("  ADC-V724  ".to_string()),
        }];
        assert_eq!(merge_product_models("", &refs), "ADC-V724");
    }
}
