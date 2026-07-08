use crate::harness::signal::SignalSet;
use crate::manual::schema_ids::manual_node_id;
use crate::model::{ManualHit, ManualProductCandidate, ManualSectionView};
use crate::proto::graphrag::GetGraphSnapshotResponse;
use crate::resolve::normalize_key;
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

const SNAPSHOT_MAX_NODES: i32 = 5000;

/// title+body への直接性（mcp::section_score と同一規則。DRY）。
pub fn score_section(question: &str, title: &str, body: &str) -> f32 {
    let text = format!("{title}\n{body}");
    crate::mcp::section_score(&normalize_key(question), question, &text)
}

/// snapshot の MENTIONS_SIGNAL 辺から、質問 signal に結線された ManualSection node_id 集合を返す。
pub fn sections_for_signals(
    snapshot: &GetGraphSnapshotResponse,
    signals: &SignalSet,
) -> HashSet<String> {
    // Signal.value → node_id
    let wanted: HashSet<String> = snapshot
        .nodes
        .iter()
        .filter(|n| n.node_type == "Signal")
        .filter_map(|n| {
            n.attributes
                .get("value")
                .map(|v| (v.clone(), n.node_id.clone()))
        })
        .filter(|(v, _)| signals.iter().any(|s| s.as_str() == v))
        .map(|(_, id)| id)
        .collect();
    snapshot
        .edges
        .iter()
        .filter(|e| e.edge_type == "MENTIONS_SIGNAL" && wanted.contains(&e.to_id))
        .map(|e| e.from_id.clone())
        .collect()
}

pub struct ManualStore {
    client: Arc<VegapunkClient>,
}

impl ManualStore {
    pub fn new(client: Arc<VegapunkClient>) -> Self {
        Self { client }
    }

    async fn snapshot(&self, schema: &str) -> Result<GetGraphSnapshotResponse> {
        let snap = self
            .client
            .graph_snapshot(schema, SNAPSHOT_MAX_NODES)
            .await?;
        if snap.nodes.len() >= SNAPSHOT_MAX_NODES as usize {
            anyhow::bail!("manual snapshot reached node limit; refusing on incomplete data");
        }
        Ok(snap)
    }

    /// signal 絞り込み(A) と body 全文(B) の max スコアで ManualSection を返す。
    pub async fn search(
        &self,
        schema: &str,
        question: &str,
        signals: &SignalSet,
        top_k: usize,
    ) -> Result<Vec<ManualHit>> {
        let snap = self.snapshot(schema).await?;
        self.search_with_snapshot(schema, question, signals, top_k, &snap)
    }

    pub fn search_with_snapshot(
        &self,
        _schema: &str,
        question: &str,
        signals: &SignalSet,
        top_k: usize,
        snapshot: &GetGraphSnapshotResponse,
    ) -> Result<Vec<ManualHit>> {
        let signal_narrowed = sections_for_signals(snapshot, signals);
        let mut hits: Vec<ManualHit> = snapshot
            .nodes
            .iter()
            .filter(|n| n.node_type == "ManualSection")
            .map(|n| {
                let a = n.attributes.clone();
                let title = a.get("title").cloned().unwrap_or_default();
                let body = a.get("body").cloned().unwrap_or_default();
                // (B) body 全文スコア。(A) signal で絞られた候補は同じ score だがヒット保証で残す。
                let base = score_section(question, &title, &body);
                // signal 絞り込みに入っていれば最低 0.6 を下限にせず、base をそのまま使う（過剰応答を防ぐ）。
                // A/B の max は「A の候補集合に入るか」で候補を残し、score は section_score を使う。
                let in_signal = signal_narrowed.contains(&n.node_id);
                let score = base; // A・B とも直接性は section_score で測る（max は候補集合の和）
                (
                    in_signal,
                    score,
                    ManualHit {
                        section_key: a.get("section_key").cloned().unwrap_or_default(),
                        title,
                        body,
                        source_url: a.get("source_url").cloned().unwrap_or_default(),
                        breadcrumb: a.get("breadcrumb").cloned().unwrap_or_default(),
                        score,
                    },
                )
            })
            // 候補: signal 絞り込みに入る or body スコアが立つ（0 超）ものを残す
            .filter(|(in_signal, score, _)| *in_signal || *score > 0.0)
            .map(|(_, _, h)| h)
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k.max(1));
        Ok(hits)
    }

    /// ManualSection + 祖先(PARENT_OF)・子・BASED_ON 経由の Rationale を返す。
    pub async fn get_section(&self, schema: &str, section_key: &str) -> Result<ManualSectionView> {
        let snap = self.snapshot(schema).await?;
        let node_id = manual_node_id(schema, "ManualSection", section_key);
        let node_json = |id: &str| -> Option<serde_json::Value> {
            snap.nodes.iter().find(|n| n.node_id == id).map(|n| {
                serde_json::json!({"node_id": n.node_id, "node_type": n.node_type, "attributes": n.attributes})
            })
        };
        let section = node_json(&node_id)
            .ok_or_else(|| anyhow!("manual section not found: {section_key}"))?;
        // 祖先: PARENT_OF の to=node_id を辿って from を親に
        let mut ancestors = Vec::new();
        let mut cur = node_id.clone();
        for _ in 0..8 {
            let Some(parent) = snap
                .edges
                .iter()
                .find(|e| e.edge_type == "PARENT_OF" && e.to_id == cur)
            else {
                break;
            };
            if let Some(j) = node_json(&parent.from_id) {
                ancestors.push(j);
            }
            cur = parent.from_id.clone();
        }
        // 子: PARENT_OF の from=node_id
        let children: Vec<_> = snap
            .edges
            .iter()
            .filter(|e| e.edge_type == "PARENT_OF" && e.from_id == node_id)
            .filter_map(|e| node_json(&e.to_id))
            .collect();
        // BASED_ON でこの section を根拠にする KR → その Rationale（参考情報）
        let based_on_rationale = Vec::new(); // Step 1 では section 視点の逆引きは省略（KR 側で辿れる）
        Ok(ManualSectionView {
            section,
            ancestors,
            children,
            based_on_rationale,
        })
    }

    /// Product ノードを name/model/aliases の正規化一致で解決する。
    pub async fn resolve_product(
        &self,
        schema: &str,
        text: &str,
    ) -> Result<Vec<ManualProductCandidate>> {
        let products = self
            .client
            .query_nodes(schema, "Product", Vec::new(), 1000)
            .await
            .context("query Product")?;
        let q = normalize_key(text);
        let mut cands: Vec<ManualProductCandidate> = products
            .into_iter()
            .filter_map(|p| {
                let a = p.attributes;
                let model = a.get("model").cloned().unwrap_or_default();
                let name = a.get("name").cloned().unwrap_or_default();
                let aliases = a.get("aliases").cloned().unwrap_or_default();
                let score = [model.as_str(), name.as_str()]
                    .into_iter()
                    .chain(aliases.split(',').map(str::trim))
                    .map(|c| crate::resolve::fuzzy_score(&q, &normalize_key(c)))
                    .fold(0.0_f32, f32::max);
                (score > 0.1).then_some(ManualProductCandidate {
                    model,
                    name,
                    score,
                    reason: if score >= 1.0 {
                        "normalized_match".into()
                    } else {
                        "semantic_nearby".into()
                    },
                })
            })
            .collect();
        cands.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        cands.truncate(5);
        Ok(cands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::signal::Signal;

    #[test]
    fn score_exact_substring_is_one() {
        // 質問がタイトル/本文の部分文字列 → 1.0（トラブルシュート系: タイトル=症状名）
        let s = score_section(
            "SDカードが認識されない",
            "SDカードが認識されない",
            "抜き差ししてください",
        );
        assert_eq!(s, 1.0);
    }

    #[test]
    fn score_unrelated_is_low() {
        let s = score_section("送料はいくら", "SDカードが認識されない", "抜き差し");
        assert!(s < 0.6);
    }

    #[test]
    fn sections_for_signals_follows_mentions_signal_reverse() {
        // Signal ノード + MENTIONS_SIGNAL 辺 から section を逆引き
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge as PE, GraphNode as PN};
        let snap = GetGraphSnapshotResponse {
            nodes: vec![PN {
                node_id: "urtect:gen1:Signal:sd_not_recognized".into(),
                node_type: "Signal".into(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: [("value".to_string(), "sd_not_recognized".to_string())]
                    .into_iter()
                    .collect(),
            }],
            edges: vec![PE {
                edge_id: String::new(),
                from_id: "urtect:gen1:ManualSection:sec-sd".into(),
                to_id: "urtect:gen1:Signal:sd_not_recognized".into(),
                edge_type: "MENTIONS_SIGNAL".into(),
            }],
            truncated: false,
            total_node_count: 0,
        };
        let want: SignalSet = [Signal::new("sd_not_recognized")].into_iter().collect();
        let got = sections_for_signals(&snap, &want);
        assert!(got.contains("urtect:gen1:ManualSection:sec-sd"));
    }
}
