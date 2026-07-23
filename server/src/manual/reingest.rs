//! 再 ingest（差分 ingest）で「除去が必要になった辺」を検出する fail-closed ヘルパ。
//!
//! backend に delete API が無く、`upsert` は追加しかできない。再 ingest で本文・構造が変わり、
//! 旧 DESCRIBES / MENTIONS_SIGNAL / MENTIONS_CONCEPT / PARENT_OF 辺が新しい派生集合・新しい親に
//! 含まれなくなった場合、その stale 辺は検索側に誤って信頼され誤回答を生む。よって「除去が必要」を
//! 検出したら **fail closed**（テナント schema 作り直しへ誘導）する。
//!
//! `ingest_urtect` と `ingest_alarmcom` はいずれも同じ検出を必要とするため、両者で共有する
//! （旧 `ingest_urtect` は `graph_snapshot(5000)` の全件ロードに依存していたが、これは backend の
//! 5000 ノード上限で truncate → stale 判定破綻するため撤去した）。既存辺は「変更のあった section
//! 1 件ごと」に 1-hop traverse で引くため、呼び出し回数はグラフ規模ではなく変更件数にスケールする。

use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::manual::schema_ids::{manual_node_id, KIND_SECTION};
use crate::vegapunk::VegapunkClient;

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
///   `(Concept, MENTIONS_CONCEPT)` を加える。
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

    // 派生辺（DESCRIBES / MENTIONS_SIGNAL / MENTIONS_CONCEPT …）: section → 対象の outgoing。
    let mut old_derived: HashSet<String> = HashSet::new();
    for (neighbor_type, edge_type) in derived_edge_types {
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
}
