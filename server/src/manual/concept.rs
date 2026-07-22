//! Concept ノード（Issue #8 v2: answers.alarm.com の概念クエリ・記事横断 join の拠り所）。
//!
//! 実際の翻訳 + Concept 抽出は `translate::translate_and_extract`（Gemini 想定、現時点は
//! stub）が担う。ここでは抽出済み `translate::ConceptExtract` を正規化し、別ページ間で
//! fuzzy マージして同一 Concept ノードに集約する純関数と、グラフノード/辺への変換だけを持つ
//! （ネットワーク非依存。ingest_alarmcom から呼ばれる）。

use crate::manual::schema_ids::{manual_node_id, KIND_CONCEPT, KIND_SECTION};
use crate::model::{GraphEdge, GraphNode};
use crate::proto::graphrag::NodeResult;
use crate::resolve::{levenshtein, normalize_key};
use crate::translate::ConceptExtract;

/// 既知 Concept 1 件（vegapunk 上の Concept ノードに対応する in-memory 表現）。
#[derive(Debug, Clone, PartialEq)]
pub struct ConceptRecord {
    pub concept_key: String,
    pub name_en: String,
    pub name_ja: String,
    pub aliases_ja: Vec<String>,
    pub kind: String,
}

/// name_en を正規化して concept_key を作る。`resolve::normalize_key`（NFKC + lowercase +
/// 英数字以外除去）を使い、product/signal の正規化キーと規則を揃える。
pub fn concept_key(name_en: &str) -> String {
    normalize_key(name_en)
}

/// 別ページ間で近い concept_key を同一 Concept とみなすための編集距離の許容値。
/// 「trailing rule/feature の有無」「単数複数」程度の抽出ゆれの吸収を狙った小さい値。
/// 大きくしすぎると無関係な概念を誤って同一視するため、コード内定数として固定する
/// （運用実績が無いための暫定値。閾値の見直しは実データで行う）。
const FUZZY_MAX_DISTANCE: usize = 2;

/// registry の中から candidate（正規化済み concept_key）に一致 / 近似する既存レコードの
/// index を返す。完全一致を優先し、無ければ編集距離 `FUZZY_MAX_DISTANCE` 以内で最初に
/// 見つかったレコードを返す（registry の並びは出現順を保つため決定論的）。
fn find_match(registry: &[ConceptRecord], candidate_key: &str) -> Option<usize> {
    if candidate_key.is_empty() {
        return None;
    }
    if let Some(idx) = registry.iter().position(|r| r.concept_key == candidate_key) {
        return Some(idx);
    }
    registry
        .iter()
        .position(|r| levenshtein(&r.concept_key, candidate_key) <= FUZZY_MAX_DISTANCE)
}

/// 抽出済み Concept を registry へマージする。一致する既存レコードが無ければ新規追加し、
/// あれば aliases_ja を重複無く追記する（name_ja/kind は最初に登録された値を正として
/// 上書きしない。記事ごとに訳語が微妙に揺れても第一登録を安定させるため）。
///
/// `name_en` が正規化後に空になる（記号のみ等）場合は Concept を作らず None を返す。
/// 呼び出し側はこのとき MENTIONS_CONCEPT 辺を張らずに当該抽出を skip する。
pub fn merge_concept(
    registry: &mut Vec<ConceptRecord>,
    extract: &ConceptExtract,
) -> Option<String> {
    let candidate_key = concept_key(&extract.name_en);
    if candidate_key.is_empty() {
        return None;
    }
    match find_match(registry, &candidate_key) {
        Some(idx) => {
            let record = &mut registry[idx];
            for alias in &extract.aliases_ja {
                let trimmed = alias.trim();
                if !trimmed.is_empty() && !record.aliases_ja.iter().any(|a| a == trimmed) {
                    record.aliases_ja.push(trimmed.to_string());
                }
            }
            Some(record.concept_key.clone())
        }
        None => {
            let aliases_ja = extract
                .aliases_ja
                .iter()
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect();
            registry.push(ConceptRecord {
                concept_key: candidate_key.clone(),
                name_en: extract.name_en.clone(),
                name_ja: extract.name_ja.clone(),
                aliases_ja,
                kind: extract.kind.clone(),
            });
            Some(candidate_key)
        }
    }
}

/// ConceptRecord を GraphNode（Concept）に変換する。aliases_ja は JSON 配列文字列で書く
/// （schema 上の属性型が string 固定のため）。
pub fn build_concept_node(schema: &str, record: &ConceptRecord) -> GraphNode {
    let aliases_json =
        serde_json::to_string(&record.aliases_ja).unwrap_or_else(|_| "[]".to_string());
    GraphNode {
        id: manual_node_id(schema, KIND_CONCEPT, &record.concept_key),
        node_type: KIND_CONCEPT.to_string(),
        attributes: vec![
            ("concept_key".to_string(), record.concept_key.clone()),
            ("name_en".to_string(), record.name_en.clone()),
            ("name_ja".to_string(), record.name_ja.clone()),
            ("aliases_ja".to_string(), aliases_json),
            ("kind".to_string(), record.kind.clone()),
        ],
    }
}

/// section → Concept の MENTIONS_CONCEPT 辺を 1 件組み立てる。
pub fn build_mentions_concept_edge(
    schema: &str,
    section_slug: &str,
    concept_key: &str,
) -> GraphEdge {
    GraphEdge {
        from_id: manual_node_id(schema, KIND_SECTION, section_slug),
        to_id: manual_node_id(schema, KIND_CONCEPT, concept_key),
        edge_type: "MENTIONS_CONCEPT".to_string(),
        attributes: Vec::new(),
    }
}

/// vegapunk から取得した既存 Concept ノード一覧（`query_nodes` の `NodeResult`）を
/// registry（in-memory 表現）へ復元する。差分 ingest 実行をまたいで fuzzy マージを継続させる
/// ために使う。
///
/// `aliases_ja` の JSON 配列パースに失敗した要素は空配列にフォールバックして warn する
/// （fail-open: Concept の別名復元漏れは検索精度の劣化に留まり、ingest 全体を止めるほどの
/// 重大度ではないため。本体の concept_key/name_en/name_ja/kind は復元する）。
pub fn restore_registry_from_nodes(nodes: &[NodeResult]) -> Vec<ConceptRecord> {
    nodes
        .iter()
        .filter(|n| n.node_type == KIND_CONCEPT)
        .filter_map(|n| {
            let concept_key = n.attributes.get("concept_key")?.clone();
            let name_en = n.attributes.get("name_en").cloned().unwrap_or_default();
            let name_ja = n.attributes.get("name_ja").cloned().unwrap_or_default();
            let kind = n.attributes.get("kind").cloned().unwrap_or_default();
            let aliases_ja = match n.attributes.get("aliases_ja") {
                Some(raw) => serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|err| {
                    tracing::warn!(
                        concept_key = %concept_key,
                        error = %err,
                        "Concept.aliases_ja is not valid JSON array; restoring with no aliases"
                    );
                    Vec::new()
                }),
                None => Vec::new(),
            };
            Some(ConceptRecord {
                concept_key,
                name_en,
                name_ja,
                aliases_ja,
                kind,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(name_en: &str, aliases: &[&str]) -> ConceptExtract {
        ConceptExtract {
            name_en: name_en.to_string(),
            name_ja: "テスト概念".to_string(),
            aliases_ja: aliases.iter().map(|s| s.to_string()).collect(),
            kind: "rule".to_string(),
        }
    }

    #[test]
    fn concept_key_normalizes_case_and_symbols() {
        assert_eq!(
            concept_key("First Person In"),
            concept_key("first-person-in")
        );
        assert_eq!(concept_key("First Person In"), "firstpersonin");
    }

    #[test]
    fn merge_concept_creates_new_record_when_no_match() {
        let mut registry = Vec::new();
        let key = merge_concept(
            &mut registry,
            &extract("First Person In rule", &["最初に人が入る"]),
        );
        assert_eq!(key, Some(concept_key("First Person In rule")));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry[0].aliases_ja, vec!["最初に人が入る".to_string()]);
    }

    #[test]
    fn merge_concept_merges_aliases_on_exact_key_match() {
        let mut registry = Vec::new();
        merge_concept(
            &mut registry,
            &extract("First Person In rule", &["最初に人が入る"]),
        );
        let key = merge_concept(
            &mut registry,
            &extract("First Person In rule", &["ファーストパーソンイン"]),
        );
        assert_eq!(key, Some(concept_key("First Person In rule")));
        assert_eq!(registry.len(), 1); // 新規 Concept は作らない
        assert_eq!(
            registry[0].aliases_ja,
            vec![
                "最初に人が入る".to_string(),
                "ファーストパーソンイン".to_string()
            ]
        );
    }

    #[test]
    fn merge_concept_treats_distance_beyond_threshold_as_a_different_concept() {
        // "First Person In rule" と "First Person In" は正規化後の編集距離が 4
        // （閾値 2 を超える）なので、意図的に別 Concept として扱う（過剰マージの回避）。
        let mut registry = Vec::new();
        merge_concept(&mut registry, &extract("First Person In rule", &[]));
        let before = concept_key("First Person In rule");
        assert_eq!(levenshtein(&before, &concept_key("First Person In")), 4);
        let key = merge_concept(
            &mut registry,
            &extract("First Person In", &["ファーストパーソンイン"]),
        );
        assert_ne!(key, Some(before));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn merge_concept_merges_when_within_fuzzy_threshold() {
        let mut registry = Vec::new();
        merge_concept(&mut registry, &extract("Business Hours Rule", &[]));
        // 末尾の複数形程度の差（distance <= 2）は同一 Concept として吸収される。
        let key = merge_concept(
            &mut registry,
            &extract("Business Hours Rules", &["営業時間ルール"]),
        );
        assert_eq!(registry.len(), 1);
        assert_eq!(key, Some(concept_key("Business Hours Rule")));
    }

    #[test]
    fn merge_concept_does_not_merge_unrelated_concepts() {
        let mut registry = Vec::new();
        merge_concept(&mut registry, &extract("First Person In rule", &[]));
        merge_concept(&mut registry, &extract("Geofence Arming", &[]));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn merge_concept_skips_when_name_en_normalizes_to_empty() {
        let mut registry = Vec::new();
        let key = merge_concept(&mut registry, &extract("---", &["何か"]));
        assert_eq!(key, None);
        assert!(registry.is_empty());
    }

    #[test]
    fn merge_concept_does_not_duplicate_identical_alias() {
        let mut registry = Vec::new();
        merge_concept(
            &mut registry,
            &extract("First Person In rule", &["最初に人が入る"]),
        );
        merge_concept(
            &mut registry,
            &extract("First Person In rule", &["最初に人が入る"]),
        );
        assert_eq!(registry[0].aliases_ja, vec!["最初に人が入る".to_string()]);
    }

    #[test]
    fn build_concept_node_serializes_aliases_as_json_array() {
        let record = ConceptRecord {
            concept_key: "firstpersonin".to_string(),
            name_en: "First Person In rule".to_string(),
            name_ja: "ファーストパーソンインルール".to_string(),
            aliases_ja: vec![
                "ファーストパーソンイン".to_string(),
                "最初に人が入る".to_string(),
            ],
            kind: "rule".to_string(),
        };
        let node = build_concept_node("urtect", &record);
        assert_eq!(node.node_type, "Concept");
        assert_eq!(node.id, "urtect:gen1:Concept:firstpersonin");
        let aliases = node
            .attributes
            .iter()
            .find(|(k, _)| k == "aliases_ja")
            .map(|(_, v)| v.clone())
            .unwrap();
        let parsed: Vec<String> = serde_json::from_str(&aliases).unwrap();
        assert_eq!(
            parsed,
            vec![
                "ファーストパーソンイン".to_string(),
                "最初に人が入る".to_string()
            ]
        );
    }

    #[test]
    fn build_mentions_concept_edge_points_from_section_to_concept() {
        let edge = build_mentions_concept_edge("urtect", "alarmcom-sec-x", "firstpersonin");
        assert_eq!(edge.from_id, "urtect:gen1:ManualSection:alarmcom-sec-x");
        assert_eq!(edge.to_id, "urtect:gen1:Concept:firstpersonin");
        assert_eq!(edge.edge_type, "MENTIONS_CONCEPT");
    }

    fn node_result(node_type: &str, attrs: &[(&str, &str)]) -> NodeResult {
        let mut attributes = std::collections::HashMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v.to_string());
        }
        NodeResult {
            node_id: format!("urtect:gen1:{node_type}:x"),
            node_type: node_type.to_string(),
            attributes,
        }
    }

    #[test]
    fn restore_registry_from_nodes_parses_concept_nodes_only() {
        let nodes = vec![
            node_result(
                "Concept",
                &[
                    ("concept_key", "firstpersonin"),
                    ("name_en", "First Person In rule"),
                    ("name_ja", "ファーストパーソンインルール"),
                    ("aliases_ja", r#"["ファーストパーソンイン"]"#),
                    ("kind", "rule"),
                ],
            ),
            node_result("ManualSection", &[("section_key", "alarmcom-sec-x")]),
        ];
        let registry = restore_registry_from_nodes(&nodes);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry[0].concept_key, "firstpersonin");
        assert_eq!(
            registry[0].aliases_ja,
            vec!["ファーストパーソンイン".to_string()]
        );
    }

    #[test]
    fn restore_registry_from_nodes_falls_back_to_empty_aliases_on_malformed_json() {
        let nodes = vec![node_result(
            "Concept",
            &[
                ("concept_key", "broken"),
                ("name_en", "Broken Concept"),
                ("aliases_ja", "not json"),
            ],
        )];
        let registry = restore_registry_from_nodes(&nodes);
        assert_eq!(registry.len(), 1);
        assert!(registry[0].aliases_ja.is_empty());
    }

    #[test]
    fn restore_registry_from_nodes_skips_concept_missing_concept_key() {
        let nodes = vec![node_result("Concept", &[("name_en", "No Key")])];
        assert!(restore_registry_from_nodes(&nodes).is_empty());
    }
}
