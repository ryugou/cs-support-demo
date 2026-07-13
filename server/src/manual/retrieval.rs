use crate::harness::signal::SignalSet;
use crate::manual::schema_ids::manual_node_id;
use crate::model::{ManualHit, ManualProductCandidate, ManualSectionView};
use crate::proto::graphrag::GetGraphSnapshotResponse;
use crate::resolve::normalize_key;
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;

const SNAPSHOT_MAX_NODES: i32 = 5000;

/// 完全部分文字列 → 1.0 / normalize_key 正規化後の部分文字列 → 0.95 の fast path。
/// どちらでもなければ None（呼び出し側が run カバレッジ等で判定する）。
/// query_norm は呼び出し側で 1 回だけ計算して渡す（節ごとに再計算しない）。
fn substring_fast_path(question: &str, query_norm: &str, text: &str) -> Option<f32> {
    if text.contains(question) {
        return Some(1.0);
    }
    if !query_norm.is_empty() && normalize_key(text).contains(query_norm) {
        return Some(0.95);
    }
    None
}

/// NFKC + lowercase のみ（非英数字を落とさない）。normalize_key と違い記号・空白を残す。
fn nfkc_lowercase(input: &str) -> String {
    input.nfkc().flat_map(char::to_lowercase).collect()
}

fn is_katakana(c: char) -> bool {
    ('\u{30A0}'..='\u{30FF}').contains(&c)
}

fn is_kanji(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

/// 質問文字を 3 クラス（ASCII 英数字 / カタカナ(ー含む) / 漢字）に分類する。
/// 同一クラス外・ひらがな・記号・空白はラン区切り。
fn char_class(c: char) -> Option<u8> {
    if c.is_ascii_alphanumeric() {
        Some(0)
    } else if is_katakana(c) {
        Some(1)
    } else if is_kanji(c) {
        Some(2)
    } else {
        None
    }
}

/// 質問から内容語ラン（カタカナ+ / 漢字+ / ASCII 英数字+）を抽出する。
/// ASCII ランは小文字化する。ひらがな・記号・空白はランの区切り。
/// 本文側（nfkc_lowercase）と揃えるため、質問もまず NFKC 正規化する
/// （半角カタカナ ｶﾒﾗ→カメラ、全角英数 ＳＤ→SD の幅ゆれを吸収）。
pub(crate) fn content_runs(question: &str) -> Vec<String> {
    let question: String = question.nfkc().collect();
    let mut runs = Vec::new();
    let mut current = String::new();
    let mut current_class: Option<u8> = None;
    for c in question.chars() {
        match char_class(c) {
            Some(class) if current_class == Some(class) => current.push(c),
            Some(class) => {
                if !current.is_empty() {
                    runs.push(std::mem::take(&mut current));
                }
                current.push(c);
                current_class = Some(class);
            }
            None => {
                if !current.is_empty() {
                    runs.push(std::mem::take(&mut current));
                }
                current_class = None;
            }
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs.into_iter()
        .map(|r| {
            if r.starts_with(|c: char| c.is_ascii_alphanumeric()) {
                r.to_lowercase()
            } else {
                r
            }
        })
        .collect()
}

/// 各ラン内の文字 bigram を列挙する（ラン間をまたぐ bigram は作らない。1 文字ランは unigram）。
pub(crate) fn run_bigrams(runs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for run in runs {
        let chars: Vec<char> = run.chars().collect();
        if chars.len() <= 1 {
            out.push(run.clone());
        } else {
            for w in chars.windows(2) {
                out.push(w.iter().collect());
            }
        }
    }
    out
}

/// run が正規化済みテキスト（NFKC+lowercase）にマッチするか判定する（body 限定）。
/// - 部分文字列として直接含まれる → matched
/// - 直接一致しない**漢字 run** は、bigram 数が 2 以上あり、その 50% 以上が含まれれば matched
///   （「設定方法」のような漢字ランの過剰結合の救済。bigrams {設定,定方,方法} のうち
///   設定・方法 が本文にあれば 2/3 ≥ 0.5 で matched）
/// - カタカナ run / ASCII run に bigram 救済は適用しない → unmatched
///   （「バックアップ」が ック/アッ/ップ 等の断片で誤 matched になるのを防ぐ。
///   過剰結合は助詞省略による漢字複合語に固有で、カタカナ・英語は語自体が単位のため）
///
/// 既知の限界（コードコメント）: 漢字 run の bigram 断片一致は無関係語への誤マッチを
/// 許す場合がある。business 語彙チューニング/ベクトル検索フェーズで扱う。
fn run_matches(run: &str, text_nfkc: &str) -> bool {
    if text_nfkc.contains(run) {
        return true;
    }
    // bigram 救済は漢字 run 限定
    if !run.chars().all(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)) {
        return false;
    }
    let single = [run.to_string()];
    let bigrams = run_bigrams(&single);
    if bigrams.len() < 2 {
        return false;
    }
    let hit = bigrams
        .iter()
        .filter(|b| text_nfkc.contains(b.as_str()))
        .count();
    (hit as f32 / bigrams.len() as f32) >= 0.5
}

/// 質問の内容語ラン（重複除去済み）を返す。run が 1 つも取れない質問は None。
fn unique_content_runs(question: &str) -> Option<Vec<String>> {
    let runs = content_runs(question);
    if runs.is_empty() {
        return None;
    }
    let unique: Vec<String> = runs
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Some(unique)
}

/// snapshot コーパス（正規化済み ManualSection body 群）に対し、run ごとの
/// document frequency（その run が run_matches する節数）を数える。
/// 質問 1 回につき 1 度だけ呼ぶ想定（節ごとの再計算を避けるため）。
pub(crate) fn run_document_frequency(corpus_bodies_nfkc: &[String], runs: &[String]) -> Vec<usize> {
    runs.iter()
        .map(|run| {
            corpus_bodies_nfkc
                .iter()
                .filter(|body| run_matches(run, body))
                .count()
        })
        .collect()
}

/// run 単位 IDF 重み付きカバレッジスコア。
/// idf(run) = ln(1 + N / (1 + df(run)))
/// score = Σ_{matched runs} idf(run) / Σ_{all runs} idf(run)
fn idf_weighted_score(runs: &[String], dfs: &[usize], corpus_len: usize, body_nfkc: &str) -> f32 {
    debug_assert_eq!(runs.len(), dfs.len());
    let n = corpus_len as f32;
    let mut matched_weight = 0.0_f32;
    let mut total_weight = 0.0_f32;
    for (run, df) in runs.iter().zip(dfs.iter()) {
        let idf = (1.0 + n / (1.0 + *df as f32)).ln();
        total_weight += idf;
        if run_matches(run, body_nfkc) {
            matched_weight += idf;
        }
    }
    if total_weight <= 0.0 {
        return 0.0;
    }
    matched_weight / total_weight
}

/// manual 直接性スコアの本体: 質問と節本文の集合を受け取り、各節のスコアを返す。
/// `search_with_snapshot` はこれを ManualSection 群に対して使う（node 構造 → body 抽出は呼び出し側）。
///
/// 判定は body のみを見る（title を見ない）。title はしばしば症状名・見出しに過ぎず、
/// 質問の一般的な話題（例:「カメラ」）を含むだけで具体的な回答本文が無いケースがあり、
/// それを answerable と誤判定する false-positive の原因になるため。
/// トレードオフ: 症状語がタイトルにしか無い薄い body では過小評価になり得るが、
/// ingest（extract_main_text）はページ見出しを body 本文に含めて格納するため、
/// 実データでは見出し語は body 側にも現れる。
///
/// スコア: fast path（完全部分文字列 1.0 / 正規化部分文字列 0.95）→
/// run 単位 IDF 重み付きカバレッジ。run が取れない質問（全ひらがな等）は
/// legacy の section_score にフォールバックする。
pub(crate) fn score_against_corpus(question: &str, section_bodies: &[String]) -> Vec<f32> {
    let query_norm = normalize_key(question);
    let Some(runs) = unique_content_runs(question) else {
        // run が 1 つも取れない質問（全ひらがな等）は legacy フォールバックを各節に適用する。
        return section_bodies
            .iter()
            .map(|b| crate::mcp::section_score(&query_norm, question, b))
            .collect();
    };
    let corpus_nfkc: Vec<String> = section_bodies.iter().map(|b| nfkc_lowercase(b)).collect();
    // DF は質問 1 回につき 1 度だけ計算する（節ごとに再計算しない）。
    let dfs = run_document_frequency(&corpus_nfkc, &runs);
    section_bodies
        .iter()
        .zip(corpus_nfkc.iter())
        .map(|(body, body_nfkc)| {
            substring_fast_path(question, &query_norm, body)
                .unwrap_or_else(|| idf_weighted_score(&runs, &dfs, corpus_nfkc.len(), body_nfkc))
        })
        .collect()
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
        if snap.truncated {
            anyhow::bail!("manual snapshot truncated at node limit; refusing on incomplete data");
        }
        Ok(snap)
    }

    /// signal 絞り込み(A) と body 全文(B) の max スコアで ManualSection を返す。
    pub async fn search(
        &self,
        schema: &str,
        question: &str,
        signals: &SignalSet,
        product_key: Option<&str>,
        top_k: usize,
    ) -> Result<Vec<ManualHit>> {
        let snap = self.snapshot(schema).await?;
        self.search_with_snapshot(schema, question, signals, product_key, top_k, &snap)
    }

    /// `product_key` が Some のとき、候補 ManualSection は
    /// (a) 当該 Product への DESCRIBES 辺を持つ、または (b) DESCRIBES 辺を一切持たない
    /// （機種非依存ページはどの機種にも適用される）のいずれかに限定する。
    /// 他機種のみを DESCRIBES する節は除外する。None のときは絞り込まない（従来通り）。
    pub fn search_with_snapshot(
        &self,
        schema: &str,
        question: &str,
        signals: &SignalSet,
        product_key: Option<&str>,
        top_k: usize,
        snapshot: &GetGraphSnapshotResponse,
    ) -> Result<Vec<ManualHit>> {
        let signal_narrowed = sections_for_signals(snapshot, signals);
        // product_key が Some のときだけ DESCRIBES 辺を走査する（None の hot path で
        // 無駄な snapshot.edges スキャンをしない）。除外対象は「何らかの DESCRIBES を持つが
        // 当該 Product への DESCRIBES は持たない」節（＝他機種専用ページ）の 1 集合に畳む。
        let excluded_by_product: Option<HashSet<String>> = product_key.map(|pk| {
            let target = manual_node_id(schema, "Product", pk);
            let mut describes_any: HashSet<String> = HashSet::new();
            let mut describes_target: HashSet<String> = HashSet::new();
            for e in snapshot.edges.iter().filter(|e| e.edge_type == "DESCRIBES") {
                describes_any.insert(e.from_id.clone());
                if e.to_id == target {
                    describes_target.insert(e.from_id.clone());
                }
            }
            describes_any
                .difference(&describes_target)
                .cloned()
                .collect()
        });
        let manual_sections: Vec<_> = snapshot
            .nodes
            .iter()
            .filter(|n| n.node_type == "ManualSection")
            .filter(|n| {
                excluded_by_product
                    .as_ref()
                    .is_none_or(|excluded| !excluded.contains(&n.node_id))
            })
            .collect();
        // IDF 版 corpus スコア（run 単位マッチ + コーパス IDF 重み、body のみ対象）。
        // DF は質問 1 回・snapshot 全体に対して 1 度だけ計算される(score_against_corpus 内部)。
        let bodies: Vec<String> = manual_sections
            .iter()
            .map(|n| n.attributes.get("body").cloned().unwrap_or_default())
            .collect();
        let corpus_scores = score_against_corpus(question, &bodies);
        let query_norm = normalize_key(question);
        let attr = |n: &crate::proto::graphrag::GraphNode, key: &str| -> String {
            n.attributes.get(key).cloned().unwrap_or_default()
        };
        let mut hits: Vec<ManualHit> = manual_sections
            .into_iter()
            .zip(corpus_scores)
            .zip(bodies)
            .map(|((n, corpus_score), body)| {
                let title = attr(n, "title");
                // title 込みの fast path は維持する: 完全部分文字列 → 1.0、
                // 正規化部分文字列 → 0.95。それ以外は body のみに対する IDF corpus スコアを使う。
                let text = format!("{title}\n{body}");
                let score =
                    substring_fast_path(question, &query_norm, &text).unwrap_or(corpus_score);
                // signal 絞り込み(in_signal)は候補として残すかどうか（下の filter）にだけ効く。
                // score には一切影響しない（floor や boost を掛けない — 過剰応答を防ぐため、
                // 直接性は常に fast path / IDF カバレッジの実測値をそのまま使う）。
                let in_signal = signal_narrowed.contains(&n.node_id);
                (
                    in_signal,
                    score,
                    ManualHit {
                        section_key: attr(n, "section_key"),
                        title,
                        body,
                        source_url: attr(n, "source_url"),
                        breadcrumb: attr(n, "breadcrumb"),
                        score,
                    },
                )
            })
            // 候補: signal 絞り込みに入る or body スコアが立つ（0 超）ものを残す
            .filter(|(in_signal, score, _)| *in_signal || *score > 0.0)
            .map(|(_, _, h)| h)
            .collect();
        // score 降順 + 同点は section_key 昇順の決定論 tiebreak。
        // （sort_by 自体は stable sort だが、同点時の順序が snapshot のノード順=backend の
        // 返却順に依存してしまう。top_k 打ち切り・best_manual_sections・WORM 記録が
        // 実行ごとに揺れないよう、入力順に依存しない全順序で並べる。）
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.section_key.cmp(&b.section_key))
        });
        hits.truncate(top_k.max(1));
        Ok(hits)
    }

    /// ManualSection + 祖先(PARENT_OF)・子を返す。
    /// BASED_ON 経由の Rationale は未実装（KR 側で辿れるため、section 視点の逆引きは Step 1 では省略）。
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
            // 親は高々 1 本のはず。複数付いている場合はデータ不整合（ingest 側で fail closed
            // している想定が破れている）なので、非決定な traversal を返さずエラーで検出させる。
            let parents: Vec<&crate::proto::graphrag::GraphEdge> = snap
                .edges
                .iter()
                .filter(|e| e.edge_type == "PARENT_OF" && e.to_id == cur)
                .collect();
            if parents.len() > 1 {
                anyhow::bail!(
                    "section {cur} has {} PARENT_OF edges (expected at most 1); \
                     graph is inconsistent — recreate the tenant schema and re-ingest",
                    parents.len()
                );
            }
            let Some(parent) = parents.first() else {
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
                        "fuzzy_match".into()
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

    /// 単一節コーパスでスコアを取るテストヘルパ（production 経路と同じ score_against_corpus を使う）。
    fn score_one(question: &str, body: &str) -> f32 {
        score_against_corpus(question, &[body.to_string()])[0]
    }

    #[test]
    fn score_exact_substring_is_one() {
        // 質問が本文の部分文字列 → 1.0（トラブルシュート系: body 先頭に見出し=症状名）
        let s = score_one(
            "SDカードが認識されない",
            "SDカードが認識されない 抜き差ししてください",
        );
        assert_eq!(s, 1.0);
    }

    #[test]
    fn score_unrelated_is_low() {
        let s = score_one("送料はいくら", "SDカードが認識されない 抜き差し");
        assert!(s < 0.6);
    }

    #[test]
    fn content_runs_extracts_katakana_kanji_ascii() {
        let runs = content_runs("SDカードの推奨メーカーはどこか");
        assert_eq!(runs, vec!["sd", "カード", "推奨", "メーカー"]);
    }

    #[test]
    fn content_runs_normalizes_width_variants() {
        // 半角カタカナ・全角英数でも NFKC で本文側と揃う
        assert_eq!(content_runs("ＳＤｶｰﾄﾞの推奨"), vec!["sd", "カード", "推奨"]);
    }

    #[test]
    fn directness_low_when_content_words_absent() {
        // SD カード節に「推奨」「メーカー」の記載が無い → 0.6 未満
        let body = "SDカードを一度抜き差ししてください。カードの向きを確認し、カチッと音がするまで挿入します。";
        let s = score_one("SDカードの推奨メーカーはどこか", body);
        assert!(s < 0.6, "expected < 0.6, got {s}");
    }

    #[test]
    fn directness_high_when_content_words_present() {
        let body = "録画ルールの設定方法を説明します。設定画面から録画ルールを選択してください。";
        let s = score_one("録画ルールの設定方法", body);
        assert!(s > 0.6, "expected > 0.6, got {s}");
    }

    #[test]
    fn directness_low_for_unlisted_environment() {
        // 「カメラ」「浴室」が本文に無い（設置のみ一致）→ 0.6 未満
        let body =
            "設置までのステップを説明します。壁面への取り付けは付属のブラケットを使用します。";
        let s = score_one("カメラを浴室に設置できるか", body);
        assert!(s < 0.6, "expected < 0.6, got {s}");
    }

    #[test]
    fn directness_falls_back_for_hiragana_only_question() {
        // 内容ランが取れない質問はフォールバック（パニックしない・0.0..=1.0 を返す）
        let s = score_one("これはどうすればいいの", "本文です。");
        assert!((0.0..=1.0).contains(&s));
    }

    #[test]
    fn directness_high_for_paraphrase_when_body_contains_heading() {
        // ingest は見出しテキストを body に含めるため、実データの body は
        // 症状語（タイトル相当）を先頭に持つ。言い換え質問でも body 側で拾える。
        let body = "SDカードが認識されない SDカードを一度抜き差ししてください。";
        let s = score_one("SDカードを認識しません。", body);
        assert!(s > 0.6, "expected > 0.6, got {s}");
    }

    #[test]
    fn katakana_run_not_rescued_by_fragments() {
        // 「バックアップ」は ック/アッ/ップ 等の断片が本文にあっても matched にしない
        // （実測: NAS バックアップ質問が断片救済で 0.7 → 誤 allowed になった回帰）
        let text = "アプリをチェックしてアップデートを実行します。クリックして設定します。";
        assert!(!run_matches("バックアップ", text));
        // 漢字 run の救済は維持（設定方法 → 設定+方法）
        assert!(run_matches("設定方法", "設定を開き、方法を選択します。"));
        // カタカナ run も完全一致なら matched
        assert!(run_matches("バックアップ", "バックアップを作成します。"));
    }

    #[test]
    fn idf_dilution_missing_rare_run_dominates() {
        // 3 節の小コーパス。iphone/カメラ/設置 はありふれ、浴室 はどこにも無い。
        let sections = vec![
            "アプリの設定 iphoneの場合 アプリをインストールしてカメラを追加します。".to_string(),
            "カメラの設置 壁面への設置は付属ブラケットでカメラを固定します。".to_string(),
            "録画ルールの設定 録画ルールを設定します。".to_string(),
        ];
        let hits = score_against_corpus(
            "iPhoneで使っていますが、カメラを浴室に設置できますか",
            &sections,
        );
        // どの節も 0.6 未満（浴室の欠落が支配する）
        assert!(
            hits.iter().all(|s| *s < 0.6),
            "expected all < 0.6, got {hits:?}"
        );
    }

    #[test]
    fn idf_all_runs_present_scores_high() {
        let sections = vec![
            "アプリの設定 iphoneの場合 アプリをインストールしてカメラを追加します。".to_string(),
            "カメラの設置 壁面への設置は付属ブラケットでカメラを固定します。".to_string(),
            "録画ルールの設定 録画ルールを設定します。".to_string(),
        ];
        let hits = score_against_corpus("カメラを壁面に設置できますか", &sections);
        // 設置節は全 run（カメラ・壁面・設置）を含むので高スコア
        assert!(
            hits.iter().any(|s| *s > 0.6),
            "expected some > 0.6, got {hits:?}"
        );
    }

    /// 実ネットワークに繋がない dummy client（connect_lazy は遅延接続で即座に返る）。
    fn dummy_store() -> ManualStore {
        let client = crate::vegapunk::VegapunkClient::connect_lazy("http://127.0.0.1:1", "test")
            .expect("connect_lazy");
        ManualStore::new(Arc::new(client))
    }

    // connect_lazy は tonic の内部リアクタが Tokio ランタイム下での呼び出しを要求するため、
    // このテストは #[tokio::test] にする（body 自体は await しない）。
    #[tokio::test]
    async fn search_with_snapshot_scopes_to_product_via_describes_or_no_describes() {
        // A: ADC-V724 を DESCRIBES / B: ADC-VC727P のみを DESCRIBES / C: DESCRIBES 辺なし（機種非依存）
        // すべて同一本文（質問の完全部分文字列）で fast path 1.0 を取り、絞り込みだけを検証する。
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge as PE, GraphNode as PN};
        use std::collections::HashMap;
        let schema = "urtect";
        let question = "SDカードが認識されない場合の対処";
        let section_node = |key: &str| -> PN {
            let attrs: HashMap<String, String> = [
                ("section_key".to_string(), key.to_string()),
                ("title".to_string(), key.to_string()),
                ("body".to_string(), question.to_string()),
                ("source_url".to_string(), String::new()),
                ("breadcrumb".to_string(), String::new()),
            ]
            .into_iter()
            .collect();
            PN {
                node_id: manual_node_id(schema, "ManualSection", key),
                node_type: "ManualSection".to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: attrs,
            }
        };
        let product_node = |key: &str| -> PN {
            PN {
                node_id: manual_node_id(schema, "Product", key),
                node_type: "Product".to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: HashMap::new(),
            }
        };
        let describes_edge = |section_key: &str, product_key: &str| -> PE {
            PE {
                edge_id: String::new(),
                from_id: manual_node_id(schema, "ManualSection", section_key),
                to_id: manual_node_id(schema, "Product", product_key),
                edge_type: "DESCRIBES".to_string(),
            }
        };
        let snap = GetGraphSnapshotResponse {
            nodes: vec![
                section_node("sec-a"),
                section_node("sec-b"),
                section_node("sec-c"),
                product_node("ADC-V724"),
                product_node("ADC-VC727P"),
            ],
            edges: vec![
                describes_edge("sec-a", "ADC-V724"),
                describes_edge("sec-b", "ADC-VC727P"),
            ],
            truncated: false,
            total_node_count: 0,
        };
        let store = dummy_store();
        let hits = store
            .search_with_snapshot(
                schema,
                question,
                &SignalSet::new(),
                Some("ADC-V724"),
                10,
                &snap,
            )
            .expect("search_with_snapshot");
        let keys: BTreeSet<&str> = hits.iter().map(|h| h.section_key.as_str()).collect();
        assert!(keys.contains("sec-a"), "expected sec-a in {keys:?}");
        assert!(keys.contains("sec-c"), "expected sec-c in {keys:?}");
        assert!(!keys.contains("sec-b"), "sec-b must be excluded: {keys:?}");
    }

    // 同じ describes-edge 構成でも product_key が None のときは絞り込まない（従来通り）ことの
    // 非回帰テスト（codex レビュー Suggestion 対応）。
    #[tokio::test]
    async fn search_with_snapshot_does_not_scope_when_product_key_is_none() {
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge as PE, GraphNode as PN};
        use std::collections::HashMap;
        let schema = "urtect";
        let question = "SDカードが認識されない場合の対処";
        let section_node = |key: &str| -> PN {
            let attrs: HashMap<String, String> = [
                ("section_key".to_string(), key.to_string()),
                ("title".to_string(), key.to_string()),
                ("body".to_string(), question.to_string()),
                ("source_url".to_string(), String::new()),
                ("breadcrumb".to_string(), String::new()),
            ]
            .into_iter()
            .collect();
            PN {
                node_id: manual_node_id(schema, "ManualSection", key),
                node_type: "ManualSection".to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: attrs,
            }
        };
        let product_node = |key: &str| -> PN {
            PN {
                node_id: manual_node_id(schema, "Product", key),
                node_type: "Product".to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: HashMap::new(),
            }
        };
        let describes_edge = |section_key: &str, product_key: &str| -> PE {
            PE {
                edge_id: String::new(),
                from_id: manual_node_id(schema, "ManualSection", section_key),
                to_id: manual_node_id(schema, "Product", product_key),
                edge_type: "DESCRIBES".to_string(),
            }
        };
        let snap = GetGraphSnapshotResponse {
            nodes: vec![
                section_node("sec-a"),
                section_node("sec-b"),
                section_node("sec-c"),
                product_node("ADC-V724"),
                product_node("ADC-VC727P"),
            ],
            edges: vec![
                describes_edge("sec-a", "ADC-V724"),
                describes_edge("sec-b", "ADC-VC727P"),
            ],
            truncated: false,
            total_node_count: 0,
        };
        let store = dummy_store();
        let hits = store
            .search_with_snapshot(schema, question, &SignalSet::new(), None, 10, &snap)
            .expect("search_with_snapshot");
        let keys: BTreeSet<&str> = hits.iter().map(|h| h.section_key.as_str()).collect();
        assert!(keys.contains("sec-a"));
        assert!(keys.contains("sec-b"));
        assert!(keys.contains("sec-c"));
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
