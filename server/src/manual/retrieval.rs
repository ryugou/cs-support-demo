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

/// title+body への直接性。manual パス専用: 内容語 bigram カバレッジ（Task 11）。
/// legacy の `crate::mcp::section_score` 本体・sivira パスはこの変更の対象外。
pub fn score_section(question: &str, title: &str, body: &str) -> f32 {
    manual_directness_score(question, title, body)
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
pub(crate) fn content_runs(question: &str) -> Vec<String> {
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
/// - 直接一致しないが run の bigram 数が 2 以上あり、その 50% 以上が含まれる → matched
///   （「設定方法」のような漢字ランの過剰結合を救済する。bigrams {設定,定方,方法} のうち
///   設定・方法 が本文にあれば 2/3 ≥ 0.5 で matched）
/// - それ以外 → unmatched
///
/// 既知の限界（コードコメント）: run の bigram 断片一致は「ペリメーター」→「メーカー」のような
/// 無関係語への誤マッチを許してしまう場合がある。business 語彙チューニング/ベクトル検索フェーズで扱う。
fn run_matches(run: &str, text_nfkc: &str) -> bool {
    if text_nfkc.contains(run) {
        return true;
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

/// テスト可能に切り出した本体ロジック: 質問と節本文の集合を受け取り、各節のスコアを返す。
/// `search_with_snapshot` はこれを ManualSection 群に対して使う（node 構造 → body 抽出は呼び出し側）。
/// title は fast path 用に空文字を渡してよい（コーパス単位の呼び出しでは title 別枠は無い）。
pub(crate) fn score_against_corpus(question: &str, section_bodies: &[String]) -> Vec<f32> {
    let Some(runs) = unique_content_runs(question) else {
        // run が 1 つも取れない質問（全ひらがな等）は legacy フォールバックを各節に適用する。
        let query_norm = normalize_key(question);
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
            // fast path: 完全部分文字列 → 1.0、正規化部分文字列 → 0.95
            if body.contains(question) {
                return 1.0;
            }
            let query_norm = normalize_key(question);
            let body_norm = normalize_key(body);
            if !query_norm.is_empty() && body_norm.contains(&query_norm) {
                return 0.95;
            }
            idf_weighted_score(&runs, &dfs, corpus_nfkc.len(), body_nfkc)
        })
        .collect()
}

/// manual パス専用の直接性スコア（Task 11: 内容語 run カバレッジ、等重み）。
/// 1. 完全部分文字列 → 1.0
/// 2. normalize_key 正規化後の部分文字列 → 0.95
/// 3. 質問から内容語ラン（重複除去）を抽出し、run 単位で body (NFKC+lowercase, 非英数字は残す) に
///    マッチするか判定（`run_matches`）、マッチした run の割合をスコアとする。
///    単一節にはコーパスが無く IDF が定義できないため、ここは等重み。
/// 4. run が 1 つも取れない質問（全ひらがな等）は legacy の section_score にフォールバック。
///
/// 実装メモ（brief からの意図的な差分）: fast path (1・2) は
/// `title + "\n" + body` を見る。しかし run カバレッジ (3) は body のみを見る。
/// title はしばしば症状名・見出しに過ぎず、質問の一般的な話題（例:「カメラ」）を
/// 含むだけで具体的な回答本文が無いケースがある（実測: 浴室設置の質問に対し
/// title「カメラの設置」が「カメラ」「設置」を含むだけで score が閾値を超えてしまう）。
/// これは本タスクが修正対象とする false-positive とまったく同型の欠陥のため、
/// カバレッジ判定は「回答本文に具体的な内容語があるか」を問う body 限定とした。
///
/// トレードオフ: 症状語がタイトルにしか無い薄い body では過小評価になり得る。
/// ただし ingest（extract_main_text）はページ見出しを body 本文に含めて格納するため、
/// 実データでは見出し語は body 側にも現れる。
pub fn manual_directness_score(question: &str, title: &str, body: &str) -> f32 {
    let text = format!("{title}\n{body}");
    if text.contains(question) {
        return 1.0;
    }
    let query_norm = normalize_key(question);
    let text_norm = normalize_key(&text);
    if !query_norm.is_empty() && text_norm.contains(&query_norm) {
        return 0.95;
    }
    let Some(runs) = unique_content_runs(question) else {
        return crate::mcp::section_score(&query_norm, question, &text);
    };
    let body_nfkc = nfkc_lowercase(body);
    let matched = runs.iter().filter(|r| run_matches(r, &body_nfkc)).count();
    matched as f32 / runs.len() as f32
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
        let manual_sections: Vec<_> = snapshot
            .nodes
            .iter()
            .filter(|n| n.node_type == "ManualSection")
            .collect();
        // IDF 版 corpus スコア（run 単位マッチ + コーパス IDF 重み、body のみ対象）。
        // DF は質問 1 回・snapshot 全体に対して 1 度だけ計算される(score_against_corpus 内部)。
        let bodies: Vec<String> = manual_sections
            .iter()
            .map(|n| n.attributes.get("body").cloned().unwrap_or_default())
            .collect();
        let corpus_scores = score_against_corpus(question, &bodies);
        let mut hits: Vec<ManualHit> = manual_sections
            .into_iter()
            .zip(corpus_scores.into_iter())
            .map(|(n, corpus_score)| {
                let a = n.attributes.clone();
                let title = a.get("title").cloned().unwrap_or_default();
                let body = a.get("body").cloned().unwrap_or_default();
                // title+body 対象の fast path は維持する(現行どおり): 完全部分文字列 → 1.0、
                // 正規化部分文字列 → 0.95。それ以外は body のみに対する IDF corpus スコアを使う。
                let text = format!("{title}\n{body}");
                let score = if text.contains(question) {
                    1.0
                } else {
                    let query_norm = normalize_key(question);
                    let text_norm = normalize_key(&text);
                    if !query_norm.is_empty() && text_norm.contains(&query_norm) {
                        0.95
                    } else {
                        corpus_score
                    }
                };
                // signal 絞り込みに入っていれば最低 0.6 を下限にせず、score をそのまま使う（過剰応答を防ぐ）。
                let in_signal = signal_narrowed.contains(&n.node_id);
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
    fn content_runs_extracts_katakana_kanji_ascii() {
        let runs = content_runs("SDカードの推奨メーカーはどこか");
        assert_eq!(runs, vec!["sd", "カード", "推奨", "メーカー"]);
    }

    #[test]
    fn directness_low_when_content_words_absent() {
        // SD カード節に「推奨」「メーカー」の記載が無い → 0.6 未満
        let title = "SDカードが認識されない";
        let body = "SDカードを一度抜き差ししてください。カードの向きを確認し、カチッと音がするまで挿入します。";
        let s = manual_directness_score("SDカードの推奨メーカーはどこか", title, body);
        assert!(s < 0.6, "expected < 0.6, got {s}");
    }

    #[test]
    fn directness_high_when_content_words_present() {
        let title = "録画ルール（クラウド）";
        let body = "録画ルールの設定方法を説明します。設定画面から録画ルールを選択してください。";
        let s = manual_directness_score("録画ルールの設定方法", title, body);
        assert!(s > 0.6, "expected > 0.6, got {s}");
    }

    #[test]
    fn directness_low_for_unlisted_environment() {
        let title = "カメラの設置";
        let body =
            "設置までのステップを説明します。壁面への取り付けは付属のブラケットを使用します。";
        let s = manual_directness_score("カメラを浴室に設置できるか", title, body);
        // 実測 0.333 (matched=設置のみ/3 runs)。brief は等重み版で 2/3≈0.67 になり
        // 0.6 を超えると予測し、その場合は本テストを IDF 版テストへ移行してよいと
        // 事前承認していたが、実装・実測では run 単位マッチにより「カメラ」も
        // unmatched (body に断片も現れない) となるため 1/3 に留まり、このテストは
        // 等重み版のままでも通る。数値の相違を記録した上でテストは維持する。
        assert!(s < 0.6, "expected < 0.6, got {s}");
    }

    #[test]
    fn directness_exact_substring_still_one() {
        let s = manual_directness_score(
            "リセット穴を12秒押し続けます",
            "工場出荷時リセット",
            "リセット穴を12秒押し続けます。",
        );
        assert_eq!(s, 1.0);
    }

    #[test]
    fn directness_falls_back_for_hiragana_only_question() {
        // 内容ランが取れない質問はフォールバック（パニックしない・0.0..=1.0 を返す）
        let s = manual_directness_score("これはどうすればいいの", "タイトル", "本文です。");
        assert!((0.0..=1.0).contains(&s));
    }

    #[test]
    fn directness_high_for_paraphrase_when_body_contains_heading() {
        // ingest は見出しテキストを body に含めるため、実データの body は
        // 症状語（タイトル相当）を先頭に持つ。言い換え質問でも body 側で拾える。
        let title = "SDカードが認識されない";
        let body = "SDカードが認識されない SDカードを一度抜き差ししてください。";
        let s = manual_directness_score("SDカードを認識しません。", title, body);
        assert!(s > 0.6, "expected > 0.6, got {s}");
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
