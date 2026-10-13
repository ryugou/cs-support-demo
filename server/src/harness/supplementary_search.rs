//! signal 誘導補助検索（design doc `2026-10-07-partial-answer-with-handoff-design.md` §10）の
//! 純関数。質問文検索の結果と、取り次ぐ項目の signal ごとの補助検索の結果を併合して、下書きの
//! 材料（上位 `MAX_EXCERPTS` 件）と、併合後の最良スコア・監査用の取得 ID 列を作る。
//!
//! 検索そのもの（`&self` を要する `Harness::manual_search_hits`）は `mod.rs` に残す。ここは
//! 検索結果を受け取って畳み込むだけで、判定（`decision::decide`）と取り次ぐ項目の確定には
//! 一切関与しない。

use crate::harness::product_gate::ProductAllowlist;
use crate::harness::reply::MAX_EXCERPTS;
use crate::harness::signal::Signal;
use crate::model::SectionHit;
use std::collections::{HashMap, HashSet};

/// 質問文検索の結果から無条件に採る枠数。残りの枠（`MAX_EXCERPTS` - この値）を補助検索の
/// 上位に割り当てる（design doc §10.2）。
const QUESTION_SLOTS: usize = 2;

/// 複数 signal の検索結果を跨いだ `section_key` の重複を除去する。最初に現れた位置を保ち、
/// 後から現れた同一キーの score が厳密に高ければ、その位置の内容だけを差し替える（同点なら
/// 先に現れた方を残す）。HashMap の反復順に依存しないので、本番が全 score を 1.0 で返しても
/// 検索の順位が保たれる（§10.2）。
fn dedupe_keeping_first_seen_order(hits: Vec<SectionHit>) -> Vec<SectionHit> {
    let mut deduped: Vec<SectionHit> = Vec::new();
    let mut index_of_key: HashMap<String, usize> = HashMap::new();
    for hit in hits {
        match index_of_key.get(&hit.section_key) {
            Some(&index) => {
                if hit.score > deduped[index].score {
                    deduped[index] = hit;
                }
            }
            None => {
                index_of_key.insert(hit.section_key.clone(), deduped.len());
                deduped.push(hit);
            }
        }
    }
    deduped
}

/// design doc §10.2: 質問文検索結果の上位2件と補助検索結果の上位1件を合わせて基本の3枠
/// （`MAX_EXCERPTS`）とする。section_key の重複はスキップして次点を繰り上げる。補助検索の
/// 上位1件は、質問文検索の枠に既に含まれていない限り、スコアに関係なく必ず1枠を得る（質問文
/// 検索で埋まった枠に押し出されない）。枠に満たない場合（補助検索が尽きた場合を含む）は、
/// 両リストの残りをスコア降順で詰めて埋める。補助検索結果の内部順序は、スコア降順（同点は
/// 到着順）で決める。
pub(super) fn merge_question_and_signal_hits(
    question_hits: Vec<SectionHit>,
    signal_hits: Vec<SectionHit>,
) -> Vec<SectionHit> {
    let mut signal_hits = dedupe_keeping_first_seen_order(signal_hits);
    signal_hits.sort_by(|a, b| b.score.total_cmp(&a.score));

    let mut selected: Vec<SectionHit> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();

    for hit in question_hits.iter().take(QUESTION_SLOTS) {
        if used.insert(hit.section_key.clone()) {
            selected.push(hit.clone());
        }
    }

    if let Some(hit) = signal_hits.iter().find(|h| !used.contains(&h.section_key)) {
        used.insert(hit.section_key.clone());
        selected.push(hit.clone());
    }

    if selected.len() < MAX_EXCERPTS {
        let mut rest: Vec<&SectionHit> = question_hits
            .iter()
            .skip(QUESTION_SLOTS)
            .chain(signal_hits.iter())
            .filter(|h| !used.contains(&h.section_key))
            .collect();
        rest.sort_by(|a, b| b.score.total_cmp(&a.score));
        for hit in rest {
            if selected.len() >= MAX_EXCERPTS {
                break;
            }
            if used.insert(hit.section_key.clone()) {
                selected.push(hit.clone());
            }
        }
    }

    selected
}

/// design doc §10.2: `best_manual_score` の併合。補助検索結果の最大 score と、質問文検索の
/// 最良 score の大きい方を採る（どちらも無ければ `None`）。
pub(super) fn merged_best_manual_score(
    question_best_score: Option<f32>,
    signal_hits: &[SectionHit],
) -> Option<f32> {
    let signal_best = signal_hits.iter().map(|h| h.score).reduce(f32::max);
    match (question_best_score, signal_best) {
        (Some(question), Some(signal)) => Some(question.max(signal)),
        (question, None) => question,
        (None, signal) => signal,
    }
}

/// signal ごとの補助検索 1 回分の結果（`Harness::manual_search_hits` の戻り値と同じ形）。
pub(super) type SignalSearchResult = anyhow::Result<(Vec<SectionHit>, Vec<String>)>;

/// 質問文検索の結果（取扱外型番の除外済みの hits・その最良 score・監査用の取得 ID 列）に、
/// signal ごとの補助検索の結果を畳み込み、`(併合後の hits, 併合後の最良 score, 併合後の
/// 取得 ID 列)` を返す。
///
/// - 補助検索の失敗は `tracing::warn!` を出して、その signal を飛ばして続行する（design doc
///   §10.2。応答を止めない）。質問本文・hits の本文は出さない。
/// - 補助検索の hits は、質問文検索側と同じ `filter_out_of_scope_hits` を通してから併合する。
/// - 取得 ID 列（監査 lineage）は、除外フィルタの影響を受けない（質問文検索側と同じ設計）。
pub(super) fn fold_signal_search_results(
    question_hits: Vec<SectionHit>,
    question_best_score: Option<f32>,
    question_retrieved_ids: Vec<String>,
    signal_results: Vec<(Signal, SignalSearchResult)>,
    allowlist: &ProductAllowlist,
    request_id: &str,
) -> (Vec<SectionHit>, Option<f32>, Vec<String>) {
    let mut raw_signal_hits: Vec<SectionHit> = Vec::new();
    let mut retrieved_ids = question_retrieved_ids;
    for (signal, result) in signal_results {
        match result {
            Ok((hits, ids)) => {
                raw_signal_hits.extend(hits);
                retrieved_ids.extend(ids);
            }
            Err(err) => {
                tracing::warn!(
                    request_id = %request_id,
                    signal = signal.as_str(),
                    error = %err,
                    "signal-guided supplementary search failed for this signal; skipping it \
                     and continuing with the question-text results plus any other signal's \
                     supplementary hits"
                );
            }
        }
    }
    let signal_hits = super::filter_out_of_scope_hits(raw_signal_hits, allowlist);
    let best_score = merged_best_manual_score(question_best_score, &signal_hits);
    let hits = merge_question_and_signal_hits(question_hits, signal_hits);
    (hits, best_score, retrieved_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{capture_logs, filter_warn_and_error_lines};

    fn scored_hit(section_key: &str, score: f32) -> SectionHit {
        SectionHit {
            section_key: section_key.to_string(),
            title_ja: String::new(),
            body_ja: None,
            body_en: None,
            translation_status: None,
            breadcrumb: Vec::new(),
            score,
            source_url: None,
        }
    }

    fn hit_with_body(section_key: &str, score: f32, body_ja: &str) -> SectionHit {
        SectionHit {
            body_ja: Some(body_ja.to_string()),
            ..scored_hit(section_key, score)
        }
    }

    fn section_keys_of(hits: &[SectionHit]) -> Vec<&str> {
        hits.iter().map(|h| h.section_key.as_str()).collect()
    }

    fn scoped_allowlist() -> ProductAllowlist {
        ProductAllowlist::from_models(vec!["ADC-V724".to_string()])
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    // ---- merge_question_and_signal_hits ----

    #[test]
    fn merge_question_and_signal_hits_does_not_let_a_low_score_question_hit_push_out_the_signal_top_hit(
    ) {
        let question_hits = vec![
            scored_hit("a", 0.9),
            scored_hit("b", 0.8),
            scored_hit("c", 0.7),
            scored_hit("d", 0.6),
        ];
        let signal_hits = vec![scored_hit("e", 0.5)];
        let merged = merge_question_and_signal_hits(question_hits, signal_hits);
        assert_eq!(section_keys_of(&merged), vec!["a", "b", "e"]);
    }

    #[test]
    fn merge_question_and_signal_hits_promotes_the_next_question_hit_when_the_signal_top_hit_duplicates_a_top_two_question_hit(
    ) {
        let question_hits = vec![scored_hit("a", 0.9), scored_hit("b", 0.8)];
        let signal_hits = vec![scored_hit("a", 0.95), scored_hit("f", 0.4)];
        let merged = merge_question_and_signal_hits(question_hits, signal_hits);
        assert_eq!(section_keys_of(&merged), vec!["a", "b", "f"]);
    }

    #[test]
    fn merge_question_and_signal_hits_falls_back_to_the_question_hits_top_three_when_signal_hits_is_empty(
    ) {
        let question_hits = vec![
            scored_hit("a", 0.9),
            scored_hit("b", 0.8),
            scored_hit("c", 0.7),
            scored_hit("d", 0.6),
        ];
        let merged = merge_question_and_signal_hits(question_hits, Vec::new());
        assert_eq!(section_keys_of(&merged), vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_question_and_signal_hits_backfills_the_third_slot_from_question_hits_when_every_signal_hit_duplicates_the_top_two(
    ) {
        let question_hits = vec![
            scored_hit("a", 0.9),
            scored_hit("b", 0.8),
            scored_hit("c", 0.7),
        ];
        let signal_hits = vec![scored_hit("a", 0.95), scored_hit("b", 0.85)];
        let merged = merge_question_and_signal_hits(question_hits, signal_hits);
        assert_eq!(section_keys_of(&merged), vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_question_and_signal_hits_caps_the_result_at_the_material_slots() {
        let question_hits = vec![
            scored_hit("a", 0.9),
            scored_hit("b", 0.8),
            scored_hit("c", 0.7),
            scored_hit("d", 0.6),
        ];
        let signal_hits = vec![scored_hit("e", 0.95), scored_hit("f", 0.5)];
        let merged = merge_question_and_signal_hits(question_hits, signal_hits);
        assert_eq!(merged.len(), MAX_EXCERPTS);
    }

    #[test]
    fn merge_question_and_signal_hits_keeps_search_rank_order_when_every_signal_score_ties() {
        // 本番が全 score を 1.0 で返しても、補助検索の上位1件は検索の先頭（x）になる。
        // HashMap の反復順で選ばれると、実行ごとに x / y / z のどれかに揺れる。
        for _ in 0..32 {
            let signal_hits = vec![
                scored_hit("x", 1.0),
                scored_hit("y", 1.0),
                scored_hit("z", 1.0),
            ];
            let merged = merge_question_and_signal_hits(
                vec![scored_hit("a", 0.9), scored_hit("b", 0.8)],
                signal_hits,
            );
            assert_eq!(section_keys_of(&merged), vec!["a", "b", "x"]);
        }
    }

    #[test]
    fn merge_question_and_signal_hits_keeps_first_seen_order_for_duplicates_across_signals() {
        // signal 1 の結果 [p, q]、signal 2 の結果 [q, r]（q は重複）。全 score が同点なら
        // 先に現れた順 p, q, r を保つ。
        for _ in 0..32 {
            let signal_hits = vec![
                scored_hit("p", 1.0),
                scored_hit("q", 1.0),
                scored_hit("q", 1.0),
                scored_hit("r", 1.0),
            ];
            let merged = merge_question_and_signal_hits(Vec::new(), signal_hits);
            assert_eq!(section_keys_of(&merged), vec!["p", "q", "r"]);
        }
    }

    #[test]
    fn merge_question_and_signal_hits_keeps_the_higher_score_of_a_duplicate_across_signals() {
        let signal_hits = vec![
            scored_hit("p", 0.5),
            scored_hit("q", 0.6),
            scored_hit("p", 0.9),
        ];
        let merged = merge_question_and_signal_hits(Vec::new(), signal_hits);
        assert_eq!(section_keys_of(&merged), vec!["p", "q"]);
        assert_eq!(merged[0].score, 0.9);
    }

    // ---- merged_best_manual_score ----

    #[test]
    fn merged_best_manual_score_is_none_without_question_best_and_signal_hits() {
        assert_eq!(merged_best_manual_score(None, &[]), None);
    }

    #[test]
    fn merged_best_manual_score_is_the_question_best_when_there_are_no_signal_hits() {
        assert_eq!(merged_best_manual_score(Some(0.7), &[]), Some(0.7));
    }

    #[test]
    fn merged_best_manual_score_is_the_signal_max_when_there_is_no_question_best() {
        let signal_hits = vec![scored_hit("a", 0.4), scored_hit("b", 0.8)];
        assert_eq!(merged_best_manual_score(None, &signal_hits), Some(0.8));
    }

    #[test]
    fn merged_best_manual_score_is_the_larger_of_the_question_best_and_the_signal_max() {
        let signal_hits = vec![scored_hit("a", 0.4), scored_hit("b", 0.8)];
        assert_eq!(merged_best_manual_score(Some(0.9), &signal_hits), Some(0.9));
        assert_eq!(merged_best_manual_score(Some(0.5), &signal_hits), Some(0.8));
    }

    // ---- fold_signal_search_results ----

    #[test]
    fn fold_signal_search_results_warns_and_continues_with_the_other_signals_when_one_search_fails()
    {
        let question_hits = vec![hit_with_body("a", 0.9, "QUESTION-BODY-MARKER")];
        let signal_results: Vec<(Signal, SignalSearchResult)> = vec![
            (
                Signal::new("failing_signal"),
                Err(anyhow::anyhow!("vegapunk unavailable")),
            ),
            (
                Signal::new("working_signal"),
                Ok((vec![scored_hit("w", 0.6)], ids(&["w"]))),
            ),
        ];
        let ((hits, best, retrieved), logs) = capture_logs(|| {
            fold_signal_search_results(
                question_hits,
                Some(0.9),
                ids(&["a"]),
                signal_results,
                &scoped_allowlist(),
                "req-fold-1",
            )
        });
        assert_eq!(section_keys_of(&hits), vec!["a", "w"]);
        assert_eq!(best, Some(0.9));
        assert_eq!(retrieved, ids(&["a", "w"]));
        let warnings = filter_warn_and_error_lines(&logs);
        assert!(warnings.contains("req-fold-1"), "{warnings}");
        assert!(warnings.contains("failing_signal"), "{warnings}");
        assert!(warnings.contains("vegapunk unavailable"), "{warnings}");
        assert!(!warnings.contains("QUESTION-BODY-MARKER"), "{warnings}");
        assert!(!warnings.contains("working_signal"), "{warnings}");
    }

    #[test]
    fn fold_signal_search_results_equals_the_question_only_results_when_every_search_fails() {
        let signal_results: Vec<(Signal, SignalSearchResult)> =
            vec![(Signal::new("s"), Err(anyhow::anyhow!("boom")))];
        let (hits, best, retrieved) = fold_signal_search_results(
            vec![scored_hit("a", 0.9), scored_hit("b", 0.8)],
            Some(0.9),
            ids(&["a", "b"]),
            signal_results,
            &scoped_allowlist(),
            "req-fold-2",
        );
        assert_eq!(section_keys_of(&hits), vec!["a", "b"]);
        assert_eq!(best, Some(0.9));
        assert_eq!(retrieved, ids(&["a", "b"]));
    }

    #[test]
    fn fold_signal_search_results_keeps_the_retrieved_ids_of_signal_hits_the_allowlist_filters_out()
    {
        let out_of_scope = hit_with_body("o", 0.95, "ADC-VDB101の初期設定手順です");
        let signal_results: Vec<(Signal, SignalSearchResult)> =
            vec![(Signal::new("s"), Ok((vec![out_of_scope], ids(&["o"]))))];
        let (hits, best, retrieved) = fold_signal_search_results(
            vec![scored_hit("a", 0.7)],
            Some(0.7),
            ids(&["a"]),
            signal_results,
            &scoped_allowlist(),
            "req-fold-3",
        );
        assert_eq!(section_keys_of(&hits), vec!["a"]);
        assert_eq!(
            best,
            Some(0.7),
            "a filtered-out hit must not raise the best"
        );
        assert_eq!(retrieved, ids(&["a", "o"]));
    }

    #[test]
    fn fold_signal_search_results_uses_the_merged_max_as_the_best_score() {
        let signal_results: Vec<(Signal, SignalSearchResult)> = vec![
            (
                Signal::new("s1"),
                Ok((vec![scored_hit("x", 0.8)], Vec::new())),
            ),
            (
                Signal::new("s2"),
                Ok((vec![scored_hit("y", 0.95)], Vec::new())),
            ),
        ];
        let (_, best, _) = fold_signal_search_results(
            vec![scored_hit("a", 0.7)],
            Some(0.7),
            Vec::new(),
            signal_results,
            &scoped_allowlist(),
            "req-fold-4",
        );
        assert_eq!(best, Some(0.95));
    }
}
