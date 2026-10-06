//! Issue #61: 相談対象の切り替えによる case と履歴のリセット。
//!
//! 正本: `docs/superpowers/specs/2026-10-06-case-reset-on-topic-switch-design.md` §2.1・§2.2。
//! ここに置くのは **決定論の純関数のみ**（LLM を使わない）。製品参照そのものの抽出（LLM）は
//! `extraction::ProductReferenceExtractor` が担い、この判定は抽出済みの
//! [`crate::harness::product_gate::ProductReference`] を受け取るだけ。新 case の作成・
//! `previous_case_id` の書き込み・監査記録などの I/O は `Harness::record_case_switch`
//! （`harness/mod.rs`）と `api.rs::reply_handler` が担う。

use super::product_gate::{ProductAllowlist, ProductReference, ProductReferenceResolution};
use std::collections::BTreeSet;

/// 今ターンの製品参照のうち `resolution == Matched` の `matched_model` だけを、trim 後
/// 空文字を除いて取り出す（design doc §2.1: 「`Foreign` と `Ambiguous` は加えない」）。
///
/// ここで取り出す値はまだ allowlist に対して解決していない生の文字列（LLM の自由記述、
/// コードは検証していない）。切り替え判定・`product_models` への追加には使わず、
/// [`resolve_matched_models`] の内部実装としてのみ使う（design doc §2.1）。
fn matched_model_strings(current_turn_refs: &[ProductReference]) -> Vec<String> {
    current_turn_refs
        .iter()
        .filter(|r| r.resolution == ProductReferenceResolution::Matched)
        .filter_map(|r| r.matched_model.as_deref())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// 今ターンの `Matched` 製品参照を allowlist の一覧表記に解決する（design doc §2.1）。
///
/// `matched_model` は LLM が添える自由記述でコードは検証していない。大小文字・ハイフン種別・
/// 全半角の揺れがあると、同じ製品が別の型番として比較され、切り替えでないターンを誤って
/// 切り替えと判定してしまう。解決は allowlist の正規化（[`ProductAllowlist::canonical_model`]）
/// に委ね、ここで解決済みの値だけを集める。
///
/// 解決は 1 か所（この関数）に集約し、[`is_product_switch`] と [`merge_product_models`] の
/// 2 つの判定系が必ず同じ入力（解決済みの値）を見ることを保証する（design doc §2.1 末尾）。
///
/// 解決できなかった `matched_model`（allowlist のどの型番にも正規化後一致しない）は結果に
/// 含めず、件数と値を `tracing::warn!` に出す（発話本文は出さない）。
///
/// 1 ターンに警告が 2 回出ないよう、ログを出さない純関数 [`partition_matched_models`] と
/// ログを出すこの関数に分けている（`product_gate::find_confirmed_foreign_reference` /
/// `confirmed_foreign_reference` と同じ方針）。この関数は `Harness::evaluate` が呼ぶ。
pub fn resolve_matched_models(
    allowlist: &ProductAllowlist,
    current_turn_refs: &[ProductReference],
) -> Vec<String> {
    let (resolved, unresolved) = partition_matched_models(allowlist, current_turn_refs);
    if !unresolved.is_empty() {
        tracing::warn!(
            unresolved_count = unresolved.len(),
            unresolved_values = ?unresolved,
            "issue #61: could not resolve a Matched product reference against the product \
             allowlist; excluding it from product_models and switch detection"
        );
    }
    resolved
}

/// [`resolve_matched_models`] の解決ロジック本体（ログを出さない純関数）。
/// `(解決済み, 未解決)` を返す。解決は [`ProductAllowlist::canonical_model`] のみを使う。
pub fn partition_matched_models(
    allowlist: &ProductAllowlist,
    current_turn_refs: &[ProductReference],
) -> (Vec<String>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for raw in matched_model_strings(current_turn_refs) {
        match allowlist.canonical_model(&raw) {
            Some(canonical) => resolved.push(canonical),
            None => unresolved.push(raw),
        }
    }
    (resolved, unresolved)
}

/// 製品切り替えの検知規則そのもの（design doc §2.2、決定論・LLM を使わない）。
///
/// 次をすべて満たすときだけ `true`:
/// - `case_product_models`（その case でこれまでに確定した取扱製品の型番）が空でない
/// - `resolved_matched_models`（[`resolve_matched_models`] で allowlist の一覧表記に解決済みの
///   今ターンの `Matched` 型番）が 1 件以上ある
/// - `resolved_matched_models` のいずれも `case_product_models` に含まれない
///
/// 次はすべて `false`（design doc §2.2 の非切り替え4条件）:
/// - `case_product_models` が空（これまで型番が出ていない）
/// - `resolved_matched_models` が空（今ターンに `Matched` が無い、型番に触れない発話、
///   取扱外の型番だけが出た、または allowlist に解決できなかった）
/// - `resolved_matched_models` に、`case_product_models` に含まれる型番が1つでもある（既知の
///   製品との比較は同じ相談とみなす）
pub fn is_product_switch(
    case_product_models: &[String],
    resolved_matched_models: &[String],
) -> bool {
    if case_product_models.is_empty() {
        return false;
    }
    if resolved_matched_models.is_empty() {
        return false;
    }
    resolved_matched_models
        .iter()
        .all(|m| !case_product_models.iter().any(|c| c == m))
}

/// case の `product_models` 属性（CSV）に、今ターンの解決済み `Matched` 型番を加えて書き戻す
/// 値を組み立てる（design doc §2.1）。既存の CSV をパースし、
/// `resolved_matched_models`（[`resolve_matched_models`] で allowlist の一覧表記に解決済み）
/// だけを加え、辞書順でソート・重複排除して再度 CSV にする。
pub fn merge_product_models(existing_csv: &str, resolved_matched_models: &[String]) -> String {
    let mut set: BTreeSet<String> = super::knowledge::csv_list(existing_csv)
        .into_iter()
        .collect();
    set.extend(resolved_matched_models.iter().cloned());
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

    fn resolved(models: &[&str]) -> Vec<String> {
        models.iter().map(|m| m.to_string()).collect()
    }

    // ---- is_product_switch: 切り替えと判定するケース ----

    #[test]
    fn is_product_switch_detects_a_newly_matched_model_absent_from_the_case() {
        let case_models = vec!["ADC-V724".to_string()];
        assert!(is_product_switch(&case_models, &resolved(&["ADC-V523"])));
    }

    // ---- is_product_switch: design doc §2.2 の非切り替え4条件 ----

    #[test]
    fn is_product_switch_is_false_when_no_matched_reference_this_turn() {
        let case_models = vec!["ADC-V724".to_string()];
        assert!(!is_product_switch(&case_models, &[]));
    }

    #[test]
    fn is_product_switch_is_false_when_a_matched_model_is_already_known_to_the_case() {
        let case_models = vec!["ADC-V724".to_string()];
        // 既知の型番(ADC-V724)との比較を兼ねた発話。新しい型番(ADC-V523)にも触れているが、
        // 既知の型番が1つでもあれば同じ相談とみなす(design doc §2.2)。
        let resolved_models = resolved(&["ADC-V724", "ADC-V523"]);
        assert!(!is_product_switch(&case_models, &resolved_models));
    }

    #[test]
    fn is_product_switch_is_false_when_the_case_has_no_recorded_product_yet() {
        let case_models: Vec<String> = vec![];
        assert!(!is_product_switch(&case_models, &resolved(&["ADC-V523"])));
    }

    #[test]
    fn is_product_switch_is_false_when_only_an_out_of_scope_model_is_mentioned() {
        // Foreign は resolve_matched_models を通らないため、解決済みの入力は空になる。
        let case_models = vec!["ADC-V724".to_string()];
        assert!(!is_product_switch(&case_models, &[]));
    }

    #[test]
    fn is_product_switch_is_false_when_resolved_models_is_empty_due_to_ambiguous_only() {
        // Ambiguous も resolve_matched_models を通らないため、解決済みの入力は空になる。
        let case_models = vec!["ADC-V724".to_string()];
        assert!(!is_product_switch(&case_models, &[]));
    }

    // ---- merge_product_models ----

    #[test]
    fn merge_product_models_adds_resolved_models_in_dictionary_order() {
        assert_eq!(
            merge_product_models("", &resolved(&["ADC-V724", "ADC-V523"])),
            "ADC-V523,ADC-V724"
        );
    }

    #[test]
    fn merge_product_models_merges_with_existing_csv_and_dedupes() {
        assert_eq!(
            merge_product_models("ADC-V724,ADC-V900", &resolved(&["ADC-V523", "ADC-V724"])),
            "ADC-V523,ADC-V724,ADC-V900"
        );
    }

    #[test]
    fn merge_product_models_is_unchanged_when_no_resolved_models_this_turn() {
        assert_eq!(merge_product_models("ADC-V724", &[]), "ADC-V724");
    }

    // ---- resolve_matched_models（Issue #61 design doc §2.1・§6「型番の正規化」） ----

    fn case_switch_allowlist() -> ProductAllowlist {
        ProductAllowlist::from_models(vec!["ADC-V724".to_string()])
    }

    #[test]
    fn resolve_matched_models_resolves_a_lowercase_matched_model_to_the_allowlist_display_form() {
        let refs = vec![matched("adc-v724")];
        assert_eq!(
            resolve_matched_models(&case_switch_allowlist(), &refs),
            vec!["ADC-V724".to_string()]
        );
    }

    #[test]
    fn partition_matched_models_returns_unresolved_values_separately_from_resolved() {
        let refs = vec![matched("adc-v724"), matched("ADC-V999")];
        let (resolved, unresolved) = partition_matched_models(&case_switch_allowlist(), &refs);
        assert_eq!(resolved, vec!["ADC-V724".to_string()]);
        assert_eq!(unresolved, vec!["ADC-V999".to_string()]);
    }

    #[test]
    fn partition_matched_models_has_no_unresolved_for_foreign_and_ambiguous() {
        let refs = vec![foreign("他社製品X"), ambiguous("カメラ")];
        let (resolved, unresolved) = partition_matched_models(&case_switch_allowlist(), &refs);
        assert!(resolved.is_empty());
        assert!(unresolved.is_empty());
    }

    #[test]
    fn resolve_matched_models_excludes_foreign_and_ambiguous() {
        let refs = vec![foreign("他社製品X"), ambiguous("カメラ")];
        assert!(resolve_matched_models(&case_switch_allowlist(), &refs).is_empty());
    }

    #[test]
    fn resolve_matched_models_excludes_a_matched_model_not_in_the_allowlist() {
        let refs = vec![matched("ADC-V999")];
        assert!(resolve_matched_models(&case_switch_allowlist(), &refs).is_empty());
    }

    #[test]
    fn resolve_matched_models_trims_whitespace_in_matched_model_before_resolving() {
        // matched_model_strings の既存の trim 動作（変更していない）が resolve_matched_models
        // 経由でも保たれていることを固定する。
        let refs = vec![ProductReference {
            surface: "ADC-V724".to_string(),
            resolution: ProductReferenceResolution::Matched,
            matched_model: Some("  ADC-V724  ".to_string()),
        }];
        assert_eq!(
            resolve_matched_models(&case_switch_allowlist(), &refs),
            vec!["ADC-V724".to_string()]
        );
    }

    #[test]
    fn resolved_lowercase_matched_model_does_not_switch_against_the_canonical_case_model() {
        // 正規化を経ずに生の "adc-v724" のまま比較していた場合、大小文字の差で誤って切り替えと
        // 判定してしまう。resolve_matched_models を経由すると一覧表記 "ADC-V724" に揃うため、
        // 既知の型番との比較として扱われ切り替えにならない。
        let case_models = vec!["ADC-V724".to_string()];
        let refs = vec![matched("adc-v724")];
        let resolved_models = resolve_matched_models(&case_switch_allowlist(), &refs);
        assert!(!is_product_switch(&case_models, &resolved_models));
    }

    #[test]
    fn resolve_matched_models_merges_into_product_models_using_the_allowlist_display_form() {
        let refs = vec![matched("adc-v724")];
        let resolved_models = resolve_matched_models(&case_switch_allowlist(), &refs);
        assert_eq!(merge_product_models("", &resolved_models), "ADC-V724");
    }

    #[test]
    fn an_allowlist_unresolvable_matched_model_is_not_counted_as_a_switch() {
        // allowlist に無い型番だけの Matched は resolve_matched_models の結果が空になり、
        // is_product_switch では Matched として数えない（型番に触れない発話と同じ扱い）。
        let case_models = vec!["ADC-V724".to_string()];
        let refs = vec![matched("ADC-V999")];
        let resolved_models = resolve_matched_models(&case_switch_allowlist(), &refs);
        assert!(!is_product_switch(&case_models, &resolved_models));
    }
}
