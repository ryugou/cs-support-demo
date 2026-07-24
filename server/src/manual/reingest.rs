//! 再 ingest（差分 ingest）で「除去が必要になった辺」を検出する fail-closed ヘルパ。
//!
//! backend に delete API が無く、`upsert` は追加しかできない。再 ingest で本文・構造が変わり、
//! 旧 DESCRIBES / MENTIONS_SIGNAL / PARENT_OF 辺が新しい派生集合・新しい親に含まれなくなった
//! 場合、その stale 辺は検索側に誤って信頼され誤回答を生む。よって「除去が必要」を検出したら
//! **fail closed**（テナント schema 作り直しへ誘導）する。
//!
//! ただし **MENTIONS_CONCEPT だけは fail-closed 対象から除外する**（`DRIFT_TOLERANT_EDGE_TYPES`）。
//! concept 辺は Gemini(LLM) の concept 抽出由来で、同一英語本文でも実行ごとに抽出結果がドリフト
//! しうる。backend に delete が無いこの環境で strict 扱いすると、1 section の concept が 1 語
//! 揺れただけで crawl 全体が「schema 作り直せ」で bail し、incremental 再 ingest が事実上不可能に
//! なる（実測: `section …access-control requires removing derived edges (["…:Concept:requesttoexit"])`）。
//! 旧 concept 辺が残っても実害が限定的なため許容する:
//!   - search/corpus loader は MENTIONS_CONCEPT を読まない（`corpus.rs`）＝検索結果に無影響。
//!   - 残存辺が影響するのは community クラスタリングのみで、そこへ軽微なノイズを足すだけ。
//!   - vegapunk に delete が無く LLM ドリフトが不可避なので、strict にしても是正手段が無い。
//!
//! DESCRIBES / MENTIONS_SIGNAL / PARENT_OF は correctness-critical かつ変化が稀なので従来どおり
//! strict な fail-closed を維持する。
//!
//! `ingest_urtect` と `ingest_alarmcom` はいずれも同じ検出を必要とするため、両者で共有する
//! （旧 `ingest_urtect` は `graph_snapshot(5000)` の全件ロードに依存していたが、これは backend の
//! 5000 ノード上限で truncate → stale 判定破綻するため撤去した）。既存辺は「変更のあった section
//! 1 件ごと」に 1-hop traverse で引くため、呼び出し回数はグラフ規模ではなく変更件数にスケールする。

use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::manual::schema_ids::{manual_node_id, KIND_SECTION};
use crate::vegapunk::VegapunkClient;

/// stale fail-closed 対象から除外する（＝旧辺が残っても bail しない）派生 edge type。
/// ここに載る edge type は「LLM 由来などでドリフトが不可避、かつ残存しても検索の正しさを
/// 壊さない」辺に限る。現状は MENTIONS_CONCEPT のみ（理由はモジュール doc を参照）。
/// DESCRIBES / MENTIONS_SIGNAL / PARENT_OF はここに入れない＝ strict な fail-closed 対象。
const DRIFT_TOLERANT_EDGE_TYPES: &[&str] = &["MENTIONS_CONCEPT"];

/// この edge type を strict な stale fail-closed 対象として扱うか。
/// `DRIFT_TOLERANT_EDGE_TYPES` に載っていなければ strict（消えたら bail）。
fn is_strict_edge_type(edge_type: &str) -> bool {
    !DRIFT_TOLERANT_EDGE_TYPES.contains(&edge_type)
}

/// 呼び出し側が渡した派生 edge type のうち、strict な（stale 消失で bail する）ものだけを返す
/// 純関数。drift-tolerant な edge type（MENTIONS_CONCEPT）はここで落ちるため、その旧辺は
/// traverse すらされず stale 判定に一切入らない。「どの edge type を strict にするか」の唯一の
/// 判断点であり、I/O 無しでテストできる。
fn strict_edge_types<'a>(derived_edge_types: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    derived_edge_types
        .iter()
        .copied()
        .filter(|(_, edge_type)| is_strict_edge_type(edge_type))
        .collect()
}

/// 1-hop traverse のページサイズ上限。`traverse_neighbors_paged` は backend 申告の
/// `total_count` を権威に完全性を fail-closed で保証する（不足ページで打ち切らない）ため、
/// この値は 1 ページあたりの取得件数であって取りこぼし境界ではない。
const NEIGHBOR_LIMIT: i32 = 1000;

/// `old` のうち `new` に含まれない要素（＝ backend に delete が無い状況で「除去が必要」になる
/// 辺の宛先）を返す純関数。空なら「除去不要 ＝ 安全に追加 upsert できる」。I/O を伴わないため、
/// stale 検出の中核判断だけをネットワーク無しでテストできる。
fn stale_targets<'a>(old: &'a HashSet<String>, new: &HashSet<String>) -> Vec<&'a String> {
    old.iter().filter(|t| !new.contains(*t)).collect()
}

/// 変更された既存 section の旧派生辺・旧 PARENT_OF 辺のうち、新しい派生集合・新しい親に
/// 含まれない（＝除去が必要な）ものを検出し、1 件でもあれば fail closed する。
///
/// - `derived_edge_types`: outgoing で辿る `(neighbor_node_type, edge_type)` の並び。
///   urtect は `[(Product, DESCRIBES), (Signal, MENTIONS_SIGNAL)]`、alarmcom はこれに
///   `(Concept, MENTIONS_CONCEPT)` を加える。このうち `DRIFT_TOLERANT_EDGE_TYPES` に載る
///   edge type（MENTIONS_CONCEPT）は strict 判定から除外され、traverse も stale 判定もしない。
/// - `new_derived_targets`: 今回の本文から導出した派生辺の宛先 node_id 集合。
/// - `parent_slug`: 今回の親 section slug（root なら `None`）。旧 PARENT_OF の from（親）が
///   これと一致しなければ「親が変わった/root 化した」＝ PARENT_OF 除去が必要とみなす。
///
/// この section が変更された（`existing_hash` に slug があり、かつ内容ハッシュが変わった）
/// 場合にのみ呼ぶこと。新規 section は旧辺を持たないため呼ぶ必要はない。
pub async fn assert_no_stale_section_edges(
    client: &VegapunkClient,
    schema: &str,
    slug: &str,
    derived_edge_types: &[(&str, &str)],
    new_derived_targets: &HashSet<String>,
    parent_slug: Option<&str>,
) -> Result<()> {
    let sec_id = manual_node_id(schema, KIND_SECTION, slug);

    // 派生辺（DESCRIBES / MENTIONS_SIGNAL …）: section → 対象の outgoing。
    // strict な edge type だけを traverse する。drift-tolerant（MENTIONS_CONCEPT）は
    // `strict_edge_types` で除外され、旧辺が残っても bail 対象にしない（＝ traverse もしない）。
    let mut old_derived: HashSet<String> = HashSet::new();
    for (neighbor_type, edge_type) in strict_edge_types(derived_edge_types) {
        let neighbors = client
            .traverse_neighbor_ids(
                schema,
                neighbor_type,
                edge_type,
                "outgoing",
                &sec_id,
                NEIGHBOR_LIMIT,
            )
            .await
            .with_context(|| {
                format!("load existing {edge_type} edges for changed section {slug}")
            })?;
        old_derived.extend(neighbors);
    }
    let stale = stale_targets(&old_derived, new_derived_targets);
    if !stale.is_empty() {
        anyhow::bail!(
            "section {slug} requires removing derived edges ({stale:?}) but the backend exposes \
             no delete; recreate the tenant schema and re-ingest from scratch"
        );
    }

    // PARENT_OF は 親 → 子。子（この section）から見て incoming の from 側が親。
    let old_parent_ids = client
        .traverse_neighbor_ids(
            schema,
            KIND_SECTION,
            "PARENT_OF",
            "incoming",
            &sec_id,
            NEIGHBOR_LIMIT,
        )
        .await
        .with_context(|| format!("load existing PARENT_OF edges for changed section {slug}"))?;
    let new_parent_id = parent_slug.map(|p| manual_node_id(schema, KIND_SECTION, p));
    let old_parent_set: HashSet<String> = old_parent_ids.into_iter().collect();
    let new_parent_set: HashSet<String> = new_parent_id.iter().cloned().collect();
    let stale_parents = stale_targets(&old_parent_set, &new_parent_set);
    if !stale_parents.is_empty() {
        anyhow::bail!(
            "section {slug} changed parent (old {stale_parents:?} vs new {new_parent_id:?}) which \
             requires removing PARENT_OF edges, but the backend exposes no delete; recreate the \
             tenant schema and re-ingest from scratch"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stale_targets_returns_only_old_not_in_new() {
        let old = set(&["a", "b", "c"]);
        let new = set(&["b", "c", "d"]);
        let mut stale: Vec<String> = stale_targets(&old, &new).into_iter().cloned().collect();
        stale.sort();
        assert_eq!(stale, vec!["a".to_string()]);
    }

    #[test]
    fn stale_targets_empty_when_old_is_subset_of_new() {
        // 旧派生集合が新集合に完全に含まれる ＝ 除去不要 ＝ 追加 upsert で安全。
        let old = set(&["a"]);
        let new = set(&["a", "b"]);
        assert!(stale_targets(&old, &new).is_empty());
    }

    #[test]
    fn stale_targets_empty_when_old_is_empty() {
        // 新規 section 相当（旧辺なし）は常に stale なし。
        let old = set(&[]);
        let new = set(&["a", "b"]);
        assert!(stale_targets(&old, &new).is_empty());
    }

    #[test]
    fn correctness_critical_edge_types_are_strict() {
        // DESCRIBES / MENTIONS_SIGNAL / PARENT_OF は strict（消えたら bail）。
        assert!(is_strict_edge_type("DESCRIBES"));
        assert!(is_strict_edge_type("MENTIONS_SIGNAL"));
        assert!(is_strict_edge_type("PARENT_OF"));
    }

    #[test]
    fn mentions_concept_is_drift_tolerant() {
        // MENTIONS_CONCEPT は LLM ドリフト由来なので strict 対象から外す（消えても bail しない）。
        assert!(!is_strict_edge_type("MENTIONS_CONCEPT"));
    }

    #[test]
    fn strict_edge_types_drops_only_mentions_concept() {
        // alarmcom が渡す 3 種のうち、strict traverse 対象に残るのは DESCRIBES と
        // MENTIONS_SIGNAL のみ。MENTIONS_CONCEPT は落ちる（＝旧 concept 辺が消えても bail しない）。
        let requested = [
            ("Product", "DESCRIBES"),
            ("Signal", "MENTIONS_SIGNAL"),
            ("Concept", "MENTIONS_CONCEPT"),
        ];
        let strict = strict_edge_types(&requested);
        assert_eq!(
            strict,
            vec![("Product", "DESCRIBES"), ("Signal", "MENTIONS_SIGNAL")]
        );
    }

    #[test]
    fn strict_edge_types_preserves_order_and_passes_through_all_strict() {
        // urtect が渡す 2 種はどちらも strict なので、順序そのまま全通過する。
        let requested = [("Product", "DESCRIBES"), ("Signal", "MENTIONS_SIGNAL")];
        assert_eq!(strict_edge_types(&requested), requested.to_vec());
    }

    #[test]
    fn concept_drift_does_not_produce_stale_but_signal_removal_does() {
        // 派生辺の stale 判定を、edge type ごとに strict フィルタ経由で組み立てて確認する
        // （`assert_no_stale_section_edges` の traverse ループと同じ手順を I/O 無しで再現）。
        // 旧辺: Signal:a（新集合にあり残る）, Concept:x（新集合から消える）。
        let old_by_type = [
            ("Signal", "MENTIONS_SIGNAL", set(&["Signal:a"])),
            ("Concept", "MENTIONS_CONCEPT", set(&["Concept:x"])),
        ];
        let derived_edge_types: Vec<(&str, &str)> =
            old_by_type.iter().map(|(nt, et, _)| (*nt, *et)).collect();
        let new_targets = set(&["Signal:a"]); // concept x は今回消えた

        // strict な edge type の旧辺だけを集約して stale を取る。
        let mut old_strict: HashSet<String> = HashSet::new();
        for (_, edge_type) in strict_edge_types(&derived_edge_types) {
            if let Some((_, _, old)) = old_by_type.iter().find(|(_, et, _)| *et == edge_type) {
                old_strict.extend(old.iter().cloned());
            }
        }
        // Concept:x は strict フィルタで除外され、消えても stale にならない。
        assert!(
            stale_targets(&old_strict, &new_targets).is_empty(),
            "concept drift must not be treated as stale"
        );

        // 逆に Signal 辺が消えたケースは従来どおり stale として検出される。
        let new_targets_signal_gone = set(&[]);
        let stale = stale_targets(&old_strict, &new_targets_signal_gone);
        assert_eq!(
            stale.into_iter().cloned().collect::<Vec<_>>(),
            vec!["Signal:a".to_string()],
            "removal of a strict (MENTIONS_SIGNAL) edge must still be detected as stale"
        );
    }
}
