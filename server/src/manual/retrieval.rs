use crate::harness::signal::SignalSet;
use crate::manual::schema_ids::{manual_node_id, KIND_DOC, KIND_PRODUCT, KIND_SECTION};

/// node_id 内で kind を挟む marker（`{schema}:gen1:{kind}:{key}` の `:{kind}:` 部分）。
/// Search 結果 id をノード種別で絞る際のリテラル散在を避ける。
pub fn kind_marker(kind: &str) -> String {
    format!(":{kind}:")
}
use crate::model::{ManualHit, ManualProductCandidate, ManualSectionView, ProductView};
use crate::proto::graphrag::GetGraphSnapshotResponse;
use crate::resolve::normalize_key;
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;

/// get_section の祖先チェーン traversal 上限（hop 数）。CLAUDE.md の論理スキーマ契約
/// 「get_section の traversal は最大 2 hop に制限する」に合わせる。
const MAX_ANCESTOR_HOPS: usize = 2;

/// BM25 の TF 飽和パラメータ。design（`2026-08-05-manual-scoring-tf-lengthnorm-design.md`）の
/// 「`k1` / `b` は調整しない」に従い標準値を使う。実測できるようになるまでチューニングしない。
const BM25_K1: f32 = 1.2;
/// BM25 の長さ正規化パラメータ（同 design。標準値）。
const BM25_B: f32 = 0.75;

/// `NodeResult` を read tool が返す JSON ビュー（node_id / node_type / attributes）に整形する。
/// 旧 snapshot 経路の `node_json` と同一形状を保つ（クライアント契約を変えない）。
fn node_result_json(n: &crate::proto::graphrag::NodeResult) -> serde_json::Value {
    serde_json::json!({
        "node_id": n.node_id,
        "node_type": n.node_type,
        "attributes": n.attributes,
    })
}

/// `NodeResult` を部分グラフ組み立て用の proto `GraphNode` へ変換する。
/// display_text / degree / community は `product_view_from_snapshot` が読まないため既定値。
fn node_result_to_graph_node(
    n: crate::proto::graphrag::NodeResult,
) -> crate::proto::graphrag::GraphNode {
    crate::proto::graphrag::GraphNode {
        node_id: n.node_id,
        node_type: n.node_type,
        display_text: String::new(),
        degree: 0,
        community: None,
        attributes: n.attributes,
    }
}

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
///
/// 戻り値は**出現回数**（0 は不一致）。直接一致は非重複の出現回数を数える。
/// bigram 救済で一致した漢字 run は「元の run が何回出たか」を定義できないため
/// `1`（非ゼロ最小）に固定する ── 救済は元々「取りこぼしを防ぐ」ための弱い一致なので、
/// TF でも最弱に置くのが一貫している（design「bigram 救済で一致した漢字 run の tf」）。
fn run_term_frequency(run: &str, text_nfkc: &str) -> usize {
    // 空 run は `str::matches` が文字数+1 個の空マッチを返し tf が本文長に化けるため弾く。
    // content_runs は空 run を作らないので、これは呼び出し側の破れに対する防御。
    if run.is_empty() {
        return 0;
    }
    let direct = text_nfkc.matches(run).count();
    if direct > 0 {
        return direct;
    }
    // bigram 救済は漢字 run 限定
    if !run.chars().all(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)) {
        return 0;
    }
    let single = [run.to_string()];
    let bigrams = run_bigrams(&single);
    if bigrams.len() < 2 {
        return 0;
    }
    let hit = bigrams
        .iter()
        .filter(|b| text_nfkc.contains(b.as_str()))
        .count();
    if (hit as f32 / bigrams.len() as f32) >= 0.5 {
        1
    } else {
        0
    }
}

/// run が正規化済みテキストにマッチするか（`run_term_frequency` の bool 射影）。
/// マッチ判定の定義を 1 箇所に保つため、独自の判定は持たない。
fn run_matches(run: &str, text_nfkc: &str) -> bool {
    run_term_frequency(run, text_nfkc) > 0
}

/// v2 の長さ正規化に必要な 1 節分の長さ情報。単位は NFKC 正規化後の文字数。
#[derive(Debug, Clone, Copy)]
struct DocLength {
    chars: usize,
    /// 採点対象コーパスの平均文字数。0 は「平均が取れない」を意味する（下記の縮退を参照）。
    avg_chars: f32,
}

/// v2 の run 重み。BM25 の TF 飽和項に長さ正規化を掛け、min クランプで 0..=1 に収める。
///
/// ```text
/// raw = tf × (k1 + 1) / ( tf + k1 × (1 - b + b × len/avg_len) )
/// w   = min(1.0, raw)
/// ```
///
/// 分子の `(k1 + 1)` と min クランプが要点。`tf = 1` かつ `len = avg_len` でちょうど 1.0 に
/// なり、v1（bool 一致 = 1.0）の満点条件が上限として保存される。素朴な `tf/(tf+1)` だと
/// 「平均長の記事で 1 回言及」が 0.5 になり、短く簡潔な記事が閾値 0.6 を割って escalate へ
/// 倒れる（design「分子の `(k1 + 1)` と min クランプが要点」）。
fn saturating_run_weight(tf: usize, length: DocLength) -> f32 {
    if tf == 0 {
        return 0.0;
    }
    // avg_len = 0（空コーパス、または全節が空本文）ではゼロ除算になる。NaN を返すと
    // score が全順序を失い、閾値比較も無言で false になるため、長さ正規化を掛けない
    // （= 平均長扱い）方向へ縮退させる。v1 と同じ満点条件に戻るだけで、順位は壊れない。
    let length_ratio = if length.avg_chars > 0.0 {
        length.chars as f32 / length.avg_chars
    } else {
        1.0
    };
    let tf = tf as f32;
    let denominator = tf + BM25_K1 * (1.0 - BM25_B + BM25_B * length_ratio);
    (tf * (BM25_K1 + 1.0) / denominator).min(1.0)
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
/// score = Σ_{runs} idf(run) × w(run, doc) / Σ_{runs} idf(run)
///
/// `length_norm` が `None`（v1）のとき w は bool 一致（0 または 1）で、従来と完全に同一。
/// `Some`（v2）のときは TF + 長さ正規化の飽和重み（`saturating_run_weight`）を使う。
/// どちらでも w ≤ 1 なので score は 0..=1 に収まり、回答可能性の閾値の枠組みを壊さない。
fn idf_weighted_score(
    runs: &[String],
    dfs: &[usize],
    corpus_len: usize,
    body_nfkc: &str,
    length_norm: Option<DocLength>,
) -> f32 {
    debug_assert_eq!(runs.len(), dfs.len());
    let n = corpus_len as f32;
    let mut matched_weight = 0.0_f32;
    let mut total_weight = 0.0_f32;
    for (run, df) in runs.iter().zip(dfs.iter()) {
        let idf = (1.0 + n / (1.0 + *df as f32)).ln();
        total_weight += idf;
        let weight = match length_norm {
            Some(length) => saturating_run_weight(run_term_frequency(run, body_nfkc), length),
            None if run_matches(run, body_nfkc) => 1.0,
            None => 0.0,
        };
        matched_weight += idf * weight;
    }
    if total_weight <= 0.0 {
        return 0.0;
    }
    matched_weight / total_weight
}

/// 内容語 run が 1 つも残らない質問（全ひらがな、あるいは型番だけ）のスコア。
/// 現行の legacy 経路（`crate::mcp::section_score`）を各節に適用する。
fn legacy_fallback_scores(question: &str, query_norm: &str, section_bodies: &[String]) -> Vec<f32> {
    section_bodies
        .iter()
        .map(|b| crate::mcp::section_score(query_norm, question, b))
        .collect()
}

/// manual スコアの構成。`v2_enabled` は design（2026-08-05）の TF / 長さ正規化 / 型番 run 除外を
/// **まとめて**切り替える kill switch（`[harness] manual_scoring_v2_enabled`、既定 false）。
/// 個別フラグにすると組み合わせが 8 通りになり、デモ中の切り分けが実行不能になるため分けない。
/// `false` のときは v1 と完全に同一のスコアを返す。
#[derive(Debug, Clone, Default)]
pub(crate) struct ManualScoring {
    pub v2_enabled: bool,
    /// 質問 run から落とす型番 run（Product ノードの `model` / `aliases` を `content_runs` で
    /// 分解したもの）。`v2_enabled = false` のときは参照しない。
    pub model_runs: HashSet<String>,
}

/// snapshot の Product ノードから型番 run 集合を作る。
///
/// `model` と `aliases` を、質問側と**同じ `content_runs`** で分解する（`ADC-V724` →
/// `{adc, v724}`）。分解方法を揃えることが要点で、揃えないと除外が効かない。
/// `aliases` は ingest 側が `,` 連結で 1 属性に詰めている（`manual::ingest_model::
/// build_product_node`）が、`,` は `content_runs` の run 区切りなので前処理は要らない。
pub(crate) fn model_runs_from_snapshot(snapshot: &GetGraphSnapshotResponse) -> HashSet<String> {
    snapshot
        .nodes
        .iter()
        .filter(|n| n.node_type == KIND_PRODUCT)
        .flat_map(|n| {
            ["model", "aliases"]
                .into_iter()
                .filter_map(|key| n.attributes.get(key))
        })
        .flat_map(|raw| content_runs(raw))
        .collect()
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
/// **スコアだけを見るテスト用。** production 経路は
/// [`score_against_corpus_ranked`] を使う（同点時の並び替えに密度が要るため）。
/// スコア自体は同じものを返すので、閾値・カバレッジの検証はこちらで足りる。
#[cfg(test)]
pub(crate) fn score_against_corpus(
    question: &str,
    section_bodies: &[String],
    scoring: &ManualScoring,
) -> Vec<f32> {
    score_against_corpus_ranked(question, section_bodies, scoring)
        .into_iter()
        .map(|s| s.score)
        .collect()
}

/// 1 節の採点結果。
///
/// **`score` と `density` は役割が違う。混ぜてはならない。**
///
/// - `score`: 回答可能性の閾値（low 0.6 / mid 0.8 / high 0.95）と**絶対値で比較**される。
///   ここに補助信号を足すと閾値の意味が変わり、実測なしに較正し直せなくなる
/// - `density`: **同点時の並び順にだけ**使う。順位にしか効かないので、**「答えないはずの
///   ものを答える」方向の事故を構造的に起こせない**（`score` が閾値を跨がないため）
///
/// density が要る理由: 満点条件が `tf ≥ (1 - b + b × len/avg_len)` なので、**平均より
/// 長い記事でも数回言及すれば満点に届く**。実測では正解記事（`パスワード` 50 回 / 2,152 字）と
/// 無関係な記事（同 2 回 / 5,175 字）が**ともに 1.0** になった。カバレッジは「クエリ語を
/// 覆っているか」しか見ないので、この 2 つを区別できない。区別できるのは**密度**である
/// （2.65% 対 0.48%、実測で 5 倍以上の開き）。
///
/// 二段構えの意図: **カバレッジで回答可否を決め、密度で並べる。**
#[derive(Debug, Clone, Copy)]
pub(crate) struct SectionScore {
    pub score: f32,
    /// クエリ run の出現回数合計 ÷ 本文文字数。v1（kill switch off）では常に 0.0 で、
    /// 並び順に一切影響しない。
    pub density: f32,
}

/// [`score_against_corpus`] に、同点時の並び替え用の密度を足した版。
fn score_against_corpus_ranked(
    question: &str,
    section_bodies: &[String],
    scoring: &ManualScoring,
) -> Vec<SectionScore> {
    // 密度を持たない（= 並び順に効かない）採点結果へ畳む補助。
    let flat = |scores: Vec<f32>| -> Vec<SectionScore> {
        scores
            .into_iter()
            .map(|score| SectionScore {
                score,
                density: 0.0,
            })
            .collect()
    };
    let query_norm = normalize_key(question);
    let Some(mut runs) = unique_content_runs(question) else {
        // run が 1 つも取れない質問（全ひらがな等）は legacy フォールバックを各節に適用する。
        return flat(legacy_fallback_scores(
            question,
            &query_norm,
            section_bodies,
        ));
    };
    if scoring.v2_enabled {
        // 型番 run を落とす。alarm.com の記事は設計上あえて製品非依存で書かれており、
        // 正解記事に型番が 1 度も出てこない。型番 run を残すと分母だけが膨らみ、
        // 型番を書いて質問するほど汎用的な正解記事の順位が下がる（design 原因 1）。
        runs.retain(|run| !scoring.model_runs.contains(run));
        if runs.is_empty() {
            // 質問が型番だけ（例:「ADC-V724」）。run が取れない質問と同じ扱いへ倒す。
            return flat(legacy_fallback_scores(
                question,
                &query_norm,
                section_bodies,
            ));
        }
    }
    let corpus_nfkc: Vec<String> = section_bodies.iter().map(|b| nfkc_lowercase(b)).collect();
    // DF は質問 1 回につき 1 度だけ計算する（節ごとに再計算しない）。
    let dfs = run_document_frequency(&corpus_nfkc, &runs);
    // 長さ正規化の材料。v1 では空のままにして `doc_chars.get(i)` を None に落とし、
    // 従来の bool 一致に戻す（分岐を採点ループ側に二重化しない）。
    let doc_chars: Vec<usize> = if scoring.v2_enabled {
        corpus_nfkc.iter().map(|b| b.chars().count()).collect()
    } else {
        Vec::new()
    };
    let avg_chars = if doc_chars.is_empty() {
        0.0
    } else {
        doc_chars.iter().sum::<usize>() as f32 / doc_chars.len() as f32
    };
    section_bodies
        .iter()
        .zip(corpus_nfkc.iter())
        .enumerate()
        .map(|(i, (body, body_nfkc))| {
            // doc_chars が空（= v1）なら length_norm は None になり、従来の bool 一致になる。
            let length_norm = doc_chars.get(i).map(|chars| DocLength {
                chars: *chars,
                avg_chars,
            });
            let score = substring_fast_path(question, &query_norm, body).unwrap_or_else(|| {
                idf_weighted_score(&runs, &dfs, corpus_nfkc.len(), body_nfkc, length_norm)
            });
            // 密度は v2 のときだけ立てる。v1 では 0.0 のままなので tiebreak が no-op になり、
            // 並び順も従来と完全に一致する（kill switch の「完全に同一」を順位側でも守る）。
            let density = match doc_chars.get(i) {
                Some(0) | None => 0.0,
                Some(chars) => {
                    let occurrences: usize = runs
                        .iter()
                        .map(|run| run_term_frequency(run, body_nfkc))
                        .sum();
                    occurrences as f32 / *chars as f32
                }
            };
            SectionScore { score, density }
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

/// DESCRIBES 辺（ManualSection -> Product）を `product_node_id` に対して 2 集合に分割する。
/// 戻り値: `(any, target)`。`any` は何らかの DESCRIBES 辺を持つ節全体（＝機種依存ページ）、
/// `target` はそのうち `product_node_id` を DESCRIBES する節。
/// `product_view_from_snapshot` と `search_with_snapshot` の product スコープ絞り込みで
/// 同じ集合計算を共有する（挙動は変えない。重複していた走査を 1 箇所に畳むだけ）。
fn describes_sets<'a>(
    edges: &'a [crate::proto::graphrag::GraphEdge],
    product_node_id: &str,
) -> (HashSet<&'a str>, HashSet<&'a str>) {
    let mut any: HashSet<&str> = HashSet::new();
    let mut target: HashSet<&str> = HashSet::new();
    for e in edges.iter().filter(|e| e.edge_type == "DESCRIBES") {
        any.insert(e.from_id.as_str());
        if e.to_id == product_node_id {
            target.insert(e.from_id.as_str());
        }
    }
    (any, target)
}

/// Product + DESCRIBES 逆引きの節キー + それら節が属する ManualDocument 配下の TOC を返す純関数。
/// manual_v1 には product -> document の直接辺が無いため、product を DESCRIBES する節の
/// HAS_SECTION 逆引きで document を特定し、その document 配下の節から product-applicable な
/// ものだけを TOC 対象にする。product-applicable の判定基準は search_with_snapshot の
/// スコープ絞り込みと同じ: (a) 当該 Product への DESCRIBES 辺を持つ、または
/// (b) DESCRIBES 辺を一切持たない（機種非依存ページ）。他機種のみを DESCRIBES する節は
/// document 配下であっても TOC から除外する（実データ: 1 document を複数 product が共有し、
/// 各節が特定機種のみを説明する URTECT 構成での TOC 汚染を防ぐ）。
/// specs は manual_v1 では未実装のため常に空 Vec（S1-7 のスコープ外）。
/// toc は order 属性昇順（同着は section_key 昇順）のフラットリストで、
/// 各要素が親 section_key（PARENT_OF 逆引き）を持つ ── 階層 JSON は構築しない。
/// 親が TOC から除外された節（他機種専用）を指す場合でも、参照はそのまま返す（捏造・null 化しない）。
pub fn product_view_from_snapshot(
    product_key: &str,
    snapshot: &GetGraphSnapshotResponse,
) -> Result<ProductView> {
    // node_id → GraphNode の索引を 1 度だけ構築する（ループ内での線形スキャンを避ける）。
    let node_index: std::collections::HashMap<&str, &crate::proto::graphrag::GraphNode> = snapshot
        .nodes
        .iter()
        .map(|n| (n.node_id.as_str(), n))
        .collect();

    let product_node = snapshot
        .nodes
        .iter()
        .find(|n| {
            n.node_type == "Product"
                && n.attributes.get("product_key").map(String::as_str) == Some(product_key)
        })
        .ok_or_else(|| anyhow!("product not found: {product_key}"))?;

    // DESCRIBES: ManualSection -> Product の逆引き。
    // describes_any: 何らかの DESCRIBES 辺を持つ節（＝機種依存ページ）全体。
    // describing_section_ids: このうち当該 product を DESCRIBES する節。
    let (describes_any, describing_section_ids) =
        describes_sets(&snapshot.edges, &product_node.node_id);
    let mut describing_section_keys: Vec<String> = describing_section_ids
        .iter()
        .filter_map(|id| node_index.get(id))
        .map(|n| n.attributes.get("section_key").cloned().unwrap_or_default())
        .collect();
    describing_section_keys.sort();

    // DESCRIBES 節が属する document（HAS_SECTION 逆引き）→ その document 配下の全節を TOC 候補にする。
    let doc_ids: HashSet<&str> = snapshot
        .edges
        .iter()
        .filter(|e| {
            e.edge_type == "HAS_SECTION" && describing_section_ids.contains(e.to_id.as_str())
        })
        .map(|e| e.from_id.as_str())
        .collect();
    let toc_candidate_ids: HashSet<&str> = snapshot
        .edges
        .iter()
        .filter(|e| e.edge_type == "HAS_SECTION" && doc_ids.contains(e.from_id.as_str()))
        .map(|e| e.to_id.as_str())
        .collect();
    // product-applicable のみ TOC に残す: 当該 product を DESCRIBES する、または
    // DESCRIBES 辺を一切持たない節。他機種のみを DESCRIBES する節は除外する。
    let toc_section_ids: HashSet<&str> = toc_candidate_ids
        .into_iter()
        .filter(|id| !describes_any.contains(id) || describing_section_ids.contains(id))
        .collect();
    let parent_of: std::collections::HashMap<&str, &str> = snapshot
        .edges
        .iter()
        .filter(|e| e.edge_type == "PARENT_OF")
        .map(|e| (e.to_id.as_str(), e.from_id.as_str()))
        .collect();

    let mut toc: Vec<(i64, String, serde_json::Value)> = toc_section_ids
        .iter()
        .filter_map(|id| node_index.get(id).map(|n| (*id, *n)))
        .map(|(id, n)| {
            let section_key = n.attributes.get("section_key").cloned().unwrap_or_default();
            let order: i64 = n
                .attributes
                .get("order")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let parent_key = parent_of
                .get(id)
                .and_then(|pid| node_index.get(pid))
                .map(|pn| {
                    pn.attributes
                        .get("section_key")
                        .cloned()
                        .unwrap_or_default()
                });
            (
                order,
                section_key.clone(),
                serde_json::json!({
                    "section_key": section_key,
                    "title": n.attributes.get("title").cloned().unwrap_or_default(),
                    "order": order,
                    "parent": parent_key,
                }),
            )
        })
        .collect();
    toc.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let toc: Vec<serde_json::Value> = toc.into_iter().map(|(_, _, v)| v).collect();

    let product = serde_json::json!({
        "node_id": product_node.node_id,
        "attributes": product_node.attributes,
        "describing_section_keys": describing_section_keys,
    });

    Ok(ProductView {
        product,
        specs: Vec::new(),
        toc,
    })
}

/// `(node_id, score)` 群を node_id → 最大スコアの map に畳む（同一 id が複数回来た場合は
/// max を残す）。ManualSection の vector_hits（`search_with_snapshot`）と Product の
/// vector_hits（`merge_product_candidates`）の両方で使う共通の fold ステップ。
fn fold_max_scores(hits: &[(String, f32)]) -> std::collections::HashMap<&str, f32> {
    let mut map: std::collections::HashMap<&str, f32> = std::collections::HashMap::new();
    for (id, score) in hits {
        map.entry(id.as_str())
            .and_modify(|s| {
                if *score > *s {
                    *s = *score;
                }
            })
            .or_insert(*score);
    }
    map
}

pub struct ManualStore {
    client: Arc<VegapunkClient>,
    /// 材料 corpus の共有ローダ。`search` は `graph_snapshot(5000)` の全件依存をやめ、
    /// ここ経由の TTL キャッシュ付き manual_corpus（ページングで上限なし）を使う。
    corpus: Arc<crate::corpus::CorpusLoader>,
    /// manual スコア v2（TF / 長さ正規化 / 型番 run 除外）の kill switch。
    /// `[harness] manual_scoring_v2_enabled`（既定 false）。テナント設定であって
    /// リクエストごとの引数ではないため、呼び出し引数ではなくここに持つ。
    scoring_v2_enabled: bool,
}

impl ManualStore {
    pub fn new(
        client: Arc<VegapunkClient>,
        corpus: Arc<crate::corpus::CorpusLoader>,
        scoring_v2_enabled: bool,
    ) -> Self {
        Self {
            client,
            corpus,
            scoring_v2_enabled,
        }
    }

    /// signal 絞り込み(A) と body 全文(B) の max スコアで ManualSection を返す。
    /// `vector_hits` は意味検索経路（urtect design §2.3）の (node_id, score) 群。
    /// vector_route_enabled が false の呼び出し元は空 slice を渡す。
    pub async fn search(
        &self,
        schema: &str,
        question: &str,
        signals: &SignalSet,
        product_key: Option<&str>,
        top_k: usize,
        vector_hits: &[(String, f32)],
    ) -> Result<Vec<ManualHit>> {
        let snap = self.corpus.manual_corpus(schema).await?;
        self.search_with_snapshot(
            schema,
            question,
            signals,
            product_key,
            top_k,
            &snap,
            vector_hits,
        )
    }

    /// `client.search` を呼び、node_id に `marker` を含む `SearchResultItem` だけを
    /// (node_id, score) に絞って返す共通ヘルパー。backend 呼び出し失敗は呼び出し元の
    /// 検索経路を止めないよう warn ログ + 空 Vec にフォールバックする。
    /// 返却スコアは backend の 0-1 程度のスケールをそのまま使う（本関数ではリスケールしない。
    /// 較正は Task 11 の実測ベースで検討する）。
    async fn search_ids_with_scores(
        &self,
        schema: &str,
        text: &str,
        top_k: usize,
        marker: &str,
    ) -> Vec<(String, f32)> {
        match self.client.search(schema, text, top_k as i32).await {
            Ok(items) => items
                .into_iter()
                .filter_map(|item| {
                    let id = item.id?;
                    id.contains(marker).then(|| (id, item.score.unwrap_or(0.0)))
                })
                .collect(),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    marker,
                    "vector search failed; continuing with text-only retrieval"
                );
                Vec::new()
            }
        }
    }

    /// 意味検索経路（urtect design §2.3）。`enabled=false` なら即空（no-op）。
    /// `SearchResultItem` を ManualSection の node_id を持つものだけに絞り、(node_id, score) を返す。
    /// enabled ゲートをここに内包し、呼び出し側の if/else 重複を無くす。
    pub async fn vector_hits(
        &self,
        enabled: bool,
        schema: &str,
        question: &str,
        top_k: usize,
    ) -> Vec<(String, f32)> {
        if !enabled {
            return Vec::new();
        }
        let marker = kind_marker(KIND_SECTION);
        self.search_ids_with_scores(schema, question, top_k, &marker)
            .await
    }

    /// `product_key` が Some のとき、候補 ManualSection は
    /// (a) 当該 Product への DESCRIBES 辺を持つ、または (b) DESCRIBES 辺を一切持たない
    /// （機種非依存ページはどの機種にも適用される）のいずれかに限定する。
    /// 他機種のみを DESCRIBES する節は除外する。None のときは絞り込まない（従来通り）。
    ///
    /// `vector_hits` は意味検索経路（urtect design §2.3）の (ManualSection node_id, score) 群。
    /// snapshot に無い node_id は無視する（実体が確認できない候補を捏造しない）。
    /// product_key フィルタは vector 候補にも同様に適用する（テキスト候補と扱いを揃える）。
    /// 最終スコアは `max(text_score, vector_score)`。両方が寄与した場合は "both"、
    /// テキストのみは "text"、vector のみは "vector"、どちらも 0 で signal 絞り込みだけで
    /// 候補に残った場合は "signal" を `ManualHit.score_source` に残す（テキスト一致した
    /// かのような偽りの "text" にしない）。
    // schema/question/signals/product_key/top_k/snapshot/vector_hits を受ける検索本体で
    // 8 引数になる。入力構造体への集約はゲート経路の全 caller・テストに波及する再設計で、
    // 挙動不変・最小差分の範囲を超えるため、意図した引数数として許可する。
    #[allow(clippy::too_many_arguments)]
    pub fn search_with_snapshot(
        &self,
        schema: &str,
        question: &str,
        signals: &SignalSet,
        product_key: Option<&str>,
        top_k: usize,
        snapshot: &GetGraphSnapshotResponse,
        vector_hits: &[(String, f32)],
    ) -> Result<Vec<ManualHit>> {
        let signal_narrowed = sections_for_signals(snapshot, signals);
        // product_key が Some のときだけ DESCRIBES 辺を走査する（None の hot path で
        // 無駄な snapshot.edges スキャンをしない）。除外対象は「何らかの DESCRIBES を持つが
        // 当該 Product への DESCRIBES は持たない」節（＝他機種専用ページ）の 1 集合に畳む。
        let excluded_by_product: Option<HashSet<String>> = product_key.map(|pk| {
            let target = manual_node_id(schema, KIND_PRODUCT, pk);
            let (describes_any, describes_target) = describes_sets(&snapshot.edges, &target);
            describes_any
                .difference(&describes_target)
                .map(|s| s.to_string())
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
        // 型番 run は本文テキストマッチに使わない（構造 = DESCRIBES 辺 / product_key で扱う）。
        // 除外集合は snapshot の Product ノードから毎回組み立てる（製品マスタの正本は
        // vegapunk 側にあり、サーバ側に定数化された型番一覧を持たない方針のため）。
        let scoring = ManualScoring {
            v2_enabled: self.scoring_v2_enabled,
            model_runs: if self.scoring_v2_enabled {
                model_runs_from_snapshot(snapshot)
            } else {
                HashSet::new()
            },
        };
        let corpus_scores = score_against_corpus_ranked(question, &bodies, &scoring);
        let query_norm = normalize_key(question);
        let attr = |n: &crate::proto::graphrag::GraphNode, key: &str| -> String {
            n.attributes.get(key).cloned().unwrap_or_default()
        };
        // vector_hits を node_id → score に畳む（同一 id が複数回来た場合は max を残す）。
        // manual_sections（product_key フィルタ適用後の snapshot 由来の集合）に対して
        // 引くだけなので、snapshot に無い id や product_key フィルタで除外された節の
        // vector スコアは自然に無視される（別集合として union する必要がない）。
        let vector_map = fold_max_scores(vector_hits);
        let mut hits: Vec<(f32, ManualHit)> = manual_sections
            .into_iter()
            .zip(corpus_scores)
            .zip(bodies)
            .map(|((n, corpus_score), body)| {
                let title = attr(n, "title");
                // title 込みの fast path は維持する: 完全部分文字列 → 1.0、
                // 正規化部分文字列 → 0.95。それ以外は body のみに対する IDF corpus スコアを使う。
                let text = format!("{title}\n{body}");
                let text_score =
                    substring_fast_path(question, &query_norm, &text).unwrap_or(corpus_score.score);
                let vector_score = vector_map.get(n.node_id.as_str()).copied().unwrap_or(0.0);
                // 最終スコアは max(text, vector)。backend の vector score は 0-1 程度のスケールを
                // 前提とし、ここではリスケールしない（較正は実測ベースで Task 11 に回す）。
                let score = text_score.max(vector_score);
                // (false, false) は signal 絞り込み（in_signal、下の filter で判定）だけで
                // 候補に残ったケース。テキスト一致は無いので "text" ではなく "signal" と正直に
                // ラベル付けする。
                let score_source = match (text_score > 0.0, vector_score > 0.0) {
                    (true, true) => "both",
                    (true, false) => "text",
                    (false, true) => "vector",
                    (false, false) => "signal",
                }
                .to_string();
                // signal 絞り込み(in_signal)は候補として残すかどうか（下の filter）にだけ効く。
                // score には一切影響しない（floor や boost を掛けない — 過剰応答を防ぐため、
                // 直接性は常に fast path / IDF カバレッジ / vector 類似度の実測値をそのまま使う）。
                let in_signal = signal_narrowed.contains(&n.node_id);
                (
                    in_signal,
                    score,
                    corpus_score.density,
                    ManualHit {
                        section_key: attr(n, "section_key"),
                        title,
                        body,
                        source_url: attr(n, "source_url"),
                        breadcrumb: attr(n, "breadcrumb"),
                        score,
                        score_source,
                    },
                )
            })
            // 候補: signal 絞り込みに入る or (text/vector いずれかの) スコアが立つ（0 超）ものを残す
            .filter(|(in_signal, score, _, _)| *in_signal || *score > 0.0)
            .map(|(_, _, density, h)| (density, h))
            .collect();
        // score 降順 → クエリ語密度 降順 → section_key 昇順 の決定論 tiebreak。
        //
        // **密度は同点のときにしか効かない。** score は閾値と絶対値で比較されるので触らず、
        // 「どちらが先か」だけを決める。したがって回答可否の判定は一切変わらない
        // （＝この tiebreak で「答えないはずのものを答える」方向へ倒れることは起こらない）。
        //
        // 密度を挟む理由: 満点条件が `tf ≥ (1 - b + b × len/avg_len)` なので、平均より長い
        // 記事でも数回言及すれば満点に届く。実測（alarm.com 5 記事）では正解記事と無関係な
        // 固定 IP 記事がともに 1.0 で並び、カバレッジだけでは順位が決まらなかった。
        //
        // section_key 昇順は最終 tiebreak として必ず残す。密度は浮動小数で同着しうるし、
        // v1 では全節 0.0 で並ぶため、これが無いと順序が snapshot のノード順（backend の
        // 返却順）に依存してしまう。top_k 打ち切り・best_manual_sections・WORM 記録が
        // 実行ごとに揺れないよう、入力順に依存しない全順序で並べる。
        hits.sort_by(|(a_density, a), (b_density, b)| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b_density
                        .partial_cmp(a_density)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.section_key.cmp(&b.section_key))
        });
        let mut hits: Vec<ManualHit> = hits.into_iter().map(|(_, h)| h).collect();
        hits.truncate(top_k.max(1));
        Ok(hits)
    }

    /// ManualSection + 祖先(PARENT_OF)・子を返す。
    /// BASED_ON 経由の Rationale は未実装（KR 側で辿れるため、section 視点の逆引きは Step 1 では省略）。
    pub async fn get_section(&self, schema: &str, section_key: &str) -> Result<ManualSectionView> {
        // 全 snapshot をやめ、対象 section 1 件を起点に PARENT_OF を traverse する。
        // 単一起点のため touch するノードは section 近傍だけで、グラフ規模に依存しない。
        let node_id = manual_node_id(schema, KIND_SECTION, section_key);
        let section_node = self
            .client
            .query_nodes(
                schema,
                KIND_SECTION,
                vec![("section_key", "eq", section_key)],
                1,
            )
            .await
            .context("load manual section")?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("manual section not found: {section_key}"))?;
        let section = node_result_json(&section_node);

        // 祖先: PARENT_OF は親→子なので、子（この section）から incoming で辿った from が親。
        // 論理スキーマ契約（CLAUDE.md: get_section の traversal は最大 2 hop）に合わせ、
        // 祖先チェーンは 2 hop に制限する。
        let mut ancestors = Vec::new();
        let mut cur = node_id.clone();
        for _ in 0..MAX_ANCESTOR_HOPS {
            let parents = self
                .client
                .traverse_neighbors_paged(schema, KIND_SECTION, "PARENT_OF", "incoming", &cur, 1000)
                .await
                .with_context(|| format!("load PARENT_OF parents of {cur}"))?;
            // 親は高々 1 本のはず。複数は ingest 側 fail closed 想定の破れ（データ不整合）なので、
            // 非決定な traversal を返さずエラーで検出させる。
            if parents.len() > 1 {
                anyhow::bail!(
                    "section {cur} has {} PARENT_OF edges (expected at most 1); \
                     graph is inconsistent — recreate the tenant schema and re-ingest",
                    parents.len()
                );
            }
            let Some(parent) = parents.into_iter().next() else {
                break;
            };
            cur = parent.node_id.clone();
            ancestors.push(node_result_json(&parent));
        }

        // 子: PARENT_OF は親→子なので、この section から outgoing で辿った to が子。
        let children: Vec<serde_json::Value> = self
            .client
            .traverse_neighbors_paged(
                schema,
                KIND_SECTION,
                "PARENT_OF",
                "outgoing",
                &node_id,
                1000,
            )
            .await
            .with_context(|| format!("load PARENT_OF children of {node_id}"))?
            .iter()
            .map(node_result_json)
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

    /// Product 概要 + DESCRIBES 節キー + document TOC を返す（S1-7）。
    /// 全 snapshot をやめ、対象 product 1 件を起点にした traverse で必要部分グラフだけを
    /// 組み立て、既存の純関数 `product_view_from_snapshot` に渡す（スコアリング/TOC 構築ロジックは不変）。
    pub async fn get_product(&self, schema: &str, product_key: &str) -> Result<ProductView> {
        let subgraph = self.load_product_subgraph(schema, product_key).await?;
        product_view_from_snapshot(product_key, &subgraph)
    }

    /// `product_view_from_snapshot` が必要とする最小部分グラフを単一起点の traverse で組み立てる。
    ///
    /// 必要なもの:
    /// - Product ノード（対象）
    /// - その product を DESCRIBES する節（describing sections）
    /// - それら節が属する document（HAS_SECTION 逆引き）配下の全節（TOC 候補）
    /// - 各 TOC 候補の DESCRIBES 辺全部（`describes_any` の判定に必要。他機種専用節の除外根拠）
    /// - 各 TOC 候補の PARENT_OF 親（TOC の parent 表示）
    ///
    /// urtect の小規模 document でしか呼ばれず、touch 範囲は対象 product の document 部分木に限定される。
    async fn load_product_subgraph(
        &self,
        schema: &str,
        product_key: &str,
    ) -> Result<GetGraphSnapshotResponse> {
        let product_node = self
            .client
            .query_nodes(
                schema,
                KIND_PRODUCT,
                vec![("product_key", "eq", product_key)],
                1,
            )
            .await
            .context("load product")?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("product not found: {product_key}"))?;
        let product_id = product_node.node_id.clone();

        let mut nodes: HashMap<String, crate::proto::graphrag::GraphNode> = HashMap::new();
        let mut edges: Vec<crate::proto::graphrag::GraphEdge> = Vec::new();
        let mut seen_edges: HashSet<(String, String, String)> = HashSet::new();
        let mut push_edge =
            |edges: &mut Vec<_>, from_id: String, to_id: String, edge_type: &str| {
                if seen_edges.insert((from_id.clone(), to_id.clone(), edge_type.to_string())) {
                    edges.push(crate::proto::graphrag::GraphEdge {
                        edge_id: String::new(),
                        from_id,
                        to_id,
                        edge_type: edge_type.to_string(),
                    });
                }
            };
        nodes.insert(product_id.clone(), node_result_to_graph_node(product_node));

        // (1) この product を DESCRIBES する節（Product から incoming DESCRIBES）。
        let describing = self
            .client
            .traverse_neighbors_paged(
                schema,
                KIND_SECTION,
                "DESCRIBES",
                "incoming",
                &product_id,
                1000,
            )
            .await
            .context("load sections describing product")?;
        for sec in &describing {
            push_edge(
                &mut edges,
                sec.node_id.clone(),
                product_id.clone(),
                "DESCRIBES",
            );
            // 全 snapshot 経路は describing 節を必ずノードとして持っていた。document 列挙
            // （step 3）に取りこぼされても describing_section_keys が解決できるよう、ここでも入れる。
            nodes
                .entry(sec.node_id.clone())
                .or_insert_with(|| node_result_to_graph_node(sec.clone()));
        }

        // (2) describing 節が属する document（節から incoming HAS_SECTION の from が document）。
        let mut doc_ids: HashSet<String> = HashSet::new();
        for sec in &describing {
            let docs = self
                .client
                .traverse_neighbor_ids(
                    schema,
                    KIND_DOC,
                    "HAS_SECTION",
                    "incoming",
                    &sec.node_id,
                    1000,
                )
                .await
                .with_context(|| format!("load document of section {}", sec.node_id))?;
            doc_ids.extend(docs);
        }

        // (3) 各 document 配下の全節（document から outgoing HAS_SECTION の to が節）＝ TOC 候補。
        let mut candidates: Vec<crate::proto::graphrag::NodeResult> = Vec::new();
        for doc_id in &doc_ids {
            let secs = self
                .client
                .traverse_neighbors_paged(
                    schema,
                    KIND_SECTION,
                    "HAS_SECTION",
                    "outgoing",
                    doc_id,
                    1000,
                )
                .await
                .with_context(|| format!("load sections of document {doc_id}"))?;
            for sec in &secs {
                push_edge(
                    &mut edges,
                    doc_id.clone(),
                    sec.node_id.clone(),
                    "HAS_SECTION",
                );
            }
            candidates.extend(secs);
        }
        for sec in candidates {
            nodes
                .entry(sec.node_id.clone())
                .or_insert_with(|| node_result_to_graph_node(sec));
        }

        // (4) 各 TOC 候補の DESCRIBES 辺全部（他機種のみを DESCRIBES する節を除外する判定材料）と
        //     PARENT_OF 親（TOC の親表示）。候補集合を確定してから走査する。
        let candidate_ids: Vec<String> = nodes
            .values()
            .filter(|n| n.node_type == KIND_SECTION)
            .map(|n| n.node_id.clone())
            .collect();
        for sec_id in &candidate_ids {
            let described = self
                .client
                .traverse_neighbor_ids(schema, KIND_PRODUCT, "DESCRIBES", "outgoing", sec_id, 1000)
                .await
                .with_context(|| format!("load DESCRIBES targets of section {sec_id}"))?;
            for product_target in described {
                push_edge(&mut edges, sec_id.clone(), product_target, "DESCRIBES");
            }
            let parents = self
                .client
                .traverse_neighbors_paged(
                    schema,
                    KIND_SECTION,
                    "PARENT_OF",
                    "incoming",
                    sec_id,
                    1000,
                )
                .await
                .with_context(|| format!("load PARENT_OF parent of section {sec_id}"))?;
            for parent in parents {
                push_edge(
                    &mut edges,
                    parent.node_id.clone(),
                    sec_id.clone(),
                    "PARENT_OF",
                );
                nodes
                    .entry(parent.node_id.clone())
                    .or_insert_with(|| node_result_to_graph_node(parent));
            }
        }

        Ok(GetGraphSnapshotResponse {
            nodes: nodes.into_values().collect(),
            edges,
            truncated: false,
            total_node_count: 0,
        })
    }

    /// Product ノードを name/model/aliases の正規化一致で解決する。
    /// `use_semantic` が true の場合（manual_v1 かつ vector_route_enabled）、
    /// Product ノードに対する意味検索（urtect design §2.3）の結果も統合する（A5b）。
    pub async fn resolve_product(
        &self,
        schema: &str,
        text: &str,
        use_semantic: bool,
    ) -> Result<Vec<ManualProductCandidate>> {
        // query_nodes と意味検索は互いに依存しない独立した呼び出しなので、直列 await で
        // 待ち時間を積み上げず tokio::join! で並行に投げる（use_semantic=false 時は
        // vector_hits_fut は即座に空 Vec を返す no-op）。
        let product_marker = kind_marker(KIND_PRODUCT);
        let vector_hits_fut = async {
            if use_semantic {
                self.search_ids_with_scores(schema, text, 10, &product_marker)
                    .await
            } else {
                Vec::new()
            }
        };
        let (products, vector_hits) = tokio::join!(
            self.client.query_nodes(schema, "Product", Vec::new(), 1000),
            vector_hits_fut
        );
        let products = products.context("query Product")?;
        let q = normalize_key(text);
        let rows: Vec<ProductRow> = products
            .into_iter()
            .map(|p| {
                let a = p.attributes;
                let model = a.get("model").cloned().unwrap_or_default();
                let name = a.get("name").cloned().unwrap_or_default();
                let aliases = a.get("aliases").cloned().unwrap_or_default();
                let fuzzy_score = [model.as_str(), name.as_str()]
                    .into_iter()
                    .chain(aliases.split(',').map(str::trim))
                    .map(|c| crate::resolve::fuzzy_score(&q, &normalize_key(c)))
                    .fold(0.0_f32, f32::max);
                ProductRow {
                    node_id: p.node_id,
                    model,
                    name,
                    fuzzy_score,
                }
            })
            .collect();
        Ok(merge_product_candidates(rows, &vector_hits))
    }
}

/// resolve_product の候補生成に必要な Product 1 件分の行。
/// fuzzy スコアは閾値適用前の生値を持つ（vector-only 候補判定に使うため）。
#[derive(Debug)]
pub(crate) struct ProductRow {
    pub node_id: String,
    pub model: String,
    pub name: String,
    pub fuzzy_score: f32,
}

/// resolve_product の意味マッチ統合（A5b）本体。全 Product 行の fuzzy スコアと
/// vector hit (node_id, score) 群から最終候補リストを作る純関数。
///
/// - 各行の最終 score = max(fuzzy_score, vector_score)
/// - score <= 0.1 の行は候補から除外する（従来の fuzzy 専用閾値を踏襲）
/// - vector_score が fuzzy_score を上回った行（fuzzy 閾値未達の "vector-only" 候補を含む）は
///   reason = "semantic_nearby"（ここで初めて名実一致する）
/// - それ以外（fuzzy が同点以上で寄与）は既存の reason（normalized_match / fuzzy_match）を維持する
/// - 決定論のため score 降順 + 同点は model 昇順の tiebreak でソートする（Task 9 の
///   section_key tiebreak と同じ思想）
pub(crate) fn merge_product_candidates(
    rows: Vec<ProductRow>,
    vector_hits: &[(String, f32)],
) -> Vec<ManualProductCandidate> {
    let vector_map = fold_max_scores(vector_hits);
    let mut cands: Vec<ManualProductCandidate> = rows
        .into_iter()
        .filter_map(|row| {
            let vector_score = vector_map.get(row.node_id.as_str()).copied().unwrap_or(0.0);
            let score = row.fuzzy_score.max(vector_score);
            if score <= 0.1 {
                return None;
            }
            let reason = if vector_score > row.fuzzy_score {
                "semantic_nearby"
            } else if row.fuzzy_score >= 1.0 {
                "normalized_match"
            } else {
                "fuzzy_match"
            };
            Some(ManualProductCandidate {
                model: row.model,
                name: row.name,
                score,
                reason: reason.to_string(),
            })
        })
        .collect();
    cands.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.model.cmp(&b.model))
    });
    cands.truncate(5);
    cands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::signal::Signal;

    // ---- 検索非汚染（Issue #31 design doc §2 受け入れ条件）----
    //
    // `search_ids_with_scores`（意味検索、`client.search` の結果を marker で絞る経路）は
    // node_id が `kind_marker(KIND_SECTION)` / `kind_marker(KIND_PRODUCT)` を含むものだけを
    // 通す。ConversationTurn の node_id（`harness_node_id(schema, "ConversationTurn", key)`）が
    // これらのマーカーに構造的に一致しないことを、実際に使われている定数・関数で固定する
    // （ネットワーク I/O 無し）。

    #[test]
    fn conversation_turn_node_id_does_not_match_the_manual_section_marker() {
        let schema = "urtect";
        let turn_id =
            crate::harness::knowledge::harness_node_id(schema, "ConversationTurn", "turn-abc");
        let marker = kind_marker(KIND_SECTION);
        assert!(
            !turn_id.contains(&marker),
            "ConversationTurn node_id must not match the ManualSection marker: {turn_id}"
        );
    }

    #[test]
    fn conversation_turn_node_id_does_not_match_the_product_marker() {
        let schema = "urtect";
        let turn_id =
            crate::harness::knowledge::harness_node_id(schema, "ConversationTurn", "turn-abc");
        let marker = kind_marker(KIND_PRODUCT);
        assert!(
            !turn_id.contains(&marker),
            "ConversationTurn node_id must not match the Product marker: {turn_id}"
        );
    }

    #[test]
    fn manual_section_query_type_literal_is_not_conversation_turn() {
        // `get_section` / `load_product_subgraph` 等、型明示クエリのエントリポイントが実際に
        // 使う定数（`load_section`/`load_product_subgraph` で渡している `KIND_SECTION`）を
        // 直接固定する。誤って `ConversationTurn` に差し替わればここが落ちる。
        assert_ne!(KIND_SECTION, "ConversationTurn");
        assert_eq!(KIND_SECTION, "ManualSection");
    }

    /// 単一節コーパスでスコアを取るテストヘルパ（production 経路と同じ score_against_corpus を使う）。
    /// v1（kill switch off）のスコアを見る。v2 は実データ 5 記事のテスト群で固定する。
    fn score_one(question: &str, body: &str) -> f32 {
        score_against_corpus(question, &[body.to_string()], &ManualScoring::default())[0]
    }

    // ---- 実データ回帰: 本番実測の 5 記事 ----
    //
    // 出典: 本番 `evaluate_answerability` レスポンス（質問は `PASSWORD_QUESTION`）の
    // `hits[].body_ja` を **verbatim** で保存したもの。本文の長さそのものが長さ正規化の
    // 判定材料なので、要約・短縮・整形をしてはならない。
    const BODY_STATIC_IP: &str = include_str!("testdata/alarmcom_static_ip.txt");
    const BODY_BANDWIDTH: &str = include_str!("testdata/alarmcom_bandwidth.txt");
    const BODY_RESET_PASSWORD: &str =
        include_str!("testdata/alarmcom_change_or_reset_password.txt");
    const BODY_UNABLE_TO_LOG_IN: &str = include_str!("testdata/alarmcom_unable_to_log_in.txt");
    const BODY_PARTNER_HUB: &str = include_str!("testdata/alarmcom_partner_hub.txt");

    /// 本番で誤った順位が観測された実際の質問。
    const PASSWORD_QUESTION: &str = "ADC-V724 を使っていますが、パスワードを忘れてしまいました";

    /// 正解記事のラベル（この質問に答えているのはこの 1 件）。
    const ANSWER_ARTICLE: &str = "③パスワードの変更またはリセット";

    /// 回答可能性の low 閾値（`[harness.thresholds] low`）。正解記事はこれを超える必要がある。
    const ANSWERABILITY_LOW_THRESHOLD: f32 = 0.6;

    /// 5 記事コーパス（本番レスポンスの hits 順 = 現行スコアの降順）。
    /// ラベルはアサーション失敗時に「どの記事か」を読めるようにするためだけに使う。
    fn alarmcom_articles() -> Vec<(&'static str, String)> {
        vec![
            ("①固定IP", BODY_STATIC_IP.to_string()),
            ("②帯域幅", BODY_BANDWIDTH.to_string()),
            (ANSWER_ARTICLE, BODY_RESET_PASSWORD.to_string()),
            ("④ログインできない", BODY_UNABLE_TO_LOG_IN.to_string()),
            ("⑤パートナー様向け(ハブ)", BODY_PARTNER_HUB.to_string()),
        ]
    }

    // 5 記事の section_key。**正解記事が昇順で最後に来るよう意図的に振ってある。**
    //
    // 最終 tiebreak は section_key 昇順なので、密度 tiebreak が効いていなければ
    // 正解記事は同点集団の**最下位**に沈む。この向きに振っておかないと、
    // 「密度で 1 位になった」のか「同点のまま section_key の巡り合わせで 1 位になった」
    // のかをテストが区別できない（順位テストが実質何も検証していない状態になる）。
    const KEY_STATIC_IP: &str = "a-static-ip";
    const KEY_BANDWIDTH: &str = "b-bandwidth";
    const KEY_UNABLE_TO_LOG_IN: &str = "c-unable-to-log-in";
    const KEY_PARTNER_HUB: &str = "d-partner-hub";
    const ANSWER_SECTION_KEY: &str = "z-change-or-reset-password";
    /// 正解より上に来てはいけない記事の section_key（`MUST_RANK_BELOW` と同じ 3 件）。
    const MUST_RANK_BELOW_KEYS: [&str; 3] = [KEY_STATIC_IP, KEY_BANDWIDTH, KEY_PARTNER_HUB];

    /// 実データ 5 記事を `search_with_snapshot`（= production の検索経路）へ通し、
    /// **返却順**の section_key を返す。
    ///
    /// スコア関数を直接叩くのではなく production の sort を通すのは、順位を決めているのが
    /// `search_with_snapshot` の tiebreak だからである。テスト側で並べ替えを再実装すると、
    /// 本番の sort を書き換えてもテストが緑のままになる。
    async fn rank_alarmcom_through_search(v2_enabled: bool) -> Vec<String> {
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphNode as PN};
        use std::collections::HashMap;
        let schema = "urtect";
        let section = |section_key: &str, title: &str, body: &str| -> PN {
            let attrs: HashMap<String, String> = [
                ("section_key".to_string(), section_key.to_string()),
                ("title".to_string(), title.to_string()),
                ("body".to_string(), body.to_string()),
                ("source_url".to_string(), String::new()),
                ("breadcrumb".to_string(), String::new()),
            ]
            .into_iter()
            .collect();
            PN {
                node_id: manual_node_id(schema, "ManualSection", section_key),
                node_type: "ManualSection".to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: attrs,
            }
        };
        // 型番除外は Product ノード由来なので、製品マスタも同じ snapshot に入れる
        // （本番と同じ `model_runs_from_snapshot` の経路を通す）。
        let mut nodes = urtect_product_snapshot().nodes;
        nodes.extend([
            section(KEY_STATIC_IP, "固定IPの設定", BODY_STATIC_IP),
            section(KEY_BANDWIDTH, "帯域幅要件", BODY_BANDWIDTH),
            section(
                KEY_UNABLE_TO_LOG_IN,
                "ログインできない",
                BODY_UNABLE_TO_LOG_IN,
            ),
            section(KEY_PARTNER_HUB, "パートナー様向け", BODY_PARTNER_HUB),
            section(
                ANSWER_SECTION_KEY,
                "パスワードの変更またはリセット",
                BODY_RESET_PASSWORD,
            ),
        ]);
        let snapshot = GetGraphSnapshotResponse {
            nodes,
            edges: Vec::new(),
            truncated: false,
            total_node_count: 0,
        };
        let client = Arc::new(
            crate::vegapunk::VegapunkClient::connect_lazy("http://127.0.0.1:1", "test")
                .expect("connect_lazy"),
        );
        let corpus = Arc::new(crate::corpus::CorpusLoader::new(client.clone()));
        ManualStore::new(client, corpus, v2_enabled)
            .search_with_snapshot(
                schema,
                PASSWORD_QUESTION,
                &SignalSet::new(),
                None,
                5,
                &snapshot,
                &[],
            )
            .expect("search_with_snapshot")
            .into_iter()
            .map(|h| h.section_key)
            .collect()
    }

    /// 実データ 5 記事を採点し、(ラベル, score) を記事順で返す。
    fn score_alarmcom_articles(scoring: &ManualScoring) -> Vec<(&'static str, f32)> {
        let articles = alarmcom_articles();
        let bodies: Vec<String> = articles.iter().map(|(_, body)| body.clone()).collect();
        let scores = score_against_corpus(PASSWORD_QUESTION, &bodies, scoring);
        articles
            .iter()
            .map(|(label, _)| *label)
            .zip(scores)
            .collect()
    }

    fn score_of(scored: &[(&'static str, f32)], label: &str) -> f32 {
        scored
            .iter()
            .find(|(l, _)| *l == label)
            .unwrap_or_else(|| panic!("article {label} missing from {scored:?}"))
            .1
    }

    /// `server/data/urtect/products.json` と同じ 3 機種を持つ snapshot。
    /// 型番除外集合は本番と同じ `model_runs_from_snapshot` 経由で作る
    /// （テスト専用の別経路を作ると、分解方法のズレという本件の要点を検証できない）。
    fn urtect_product_snapshot() -> GetGraphSnapshotResponse {
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphNode as PN};
        let schema = "urtect";
        let product = |model: &str, aliases: &str| -> PN {
            PN {
                node_id: manual_node_id(schema, KIND_PRODUCT, model),
                node_type: KIND_PRODUCT.to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: [
                    ("product_key".to_string(), model.to_string()),
                    ("name".to_string(), model.to_string()),
                    ("model".to_string(), model.to_string()),
                    ("aliases".to_string(), aliases.to_string()),
                ]
                .into_iter()
                .collect(),
            }
        };
        GetGraphSnapshotResponse {
            nodes: vec![
                product("ADC-V724", ""),
                product("ADC-V724X", ""),
                product("ADC-VC727P", ""),
            ],
            edges: Vec::new(),
            truncated: false,
            total_node_count: 0,
        }
    }

    /// 本番と同じ構成の v2 スコアリング（kill switch on + snapshot 由来の型番除外集合）。
    fn scoring_v2() -> ManualScoring {
        ManualScoring {
            v2_enabled: true,
            model_runs: model_runs_from_snapshot(&urtect_product_snapshot()),
        }
    }

    #[test]
    fn model_runs_from_snapshot_splits_models_the_same_way_questions_are_split() {
        // 質問側の content_runs と同じ分解でなければ除外が効かない（"ADC-V724" という
        // 1 語のままでは、質問由来の run "adc" / "v724" のどちらとも照合できない）。
        let runs = model_runs_from_snapshot(&urtect_product_snapshot());
        for expected in ["adc", "v724", "v724x", "vc727p"] {
            assert!(runs.contains(expected), "expected {expected} in {runs:?}");
        }
        // 質問「ADC-V724 を…」から出る型番 run が確かに除外対象に入っていること。
        let question_runs = content_runs(PASSWORD_QUESTION);
        assert!(question_runs.contains(&"adc".to_string()));
        assert!(question_runs.contains(&"v724".to_string()));
    }

    #[test]
    fn model_runs_from_snapshot_splits_comma_joined_aliases() {
        // ingest 側は aliases を "," 連結で 1 属性に詰める
        // （manual::ingest_model::build_product_node）。"," は content_runs の run 区切り。
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphNode as PN};
        let node = PN {
            node_id: manual_node_id("urtect", KIND_PRODUCT, "ADC-V724"),
            node_type: KIND_PRODUCT.to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: [
                ("model".to_string(), "ADC-V724".to_string()),
                ("aliases".to_string(), "V724 Pro,VC727P".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        let snap = GetGraphSnapshotResponse {
            nodes: vec![node],
            edges: Vec::new(),
            truncated: false,
            total_node_count: 0,
        };
        let runs = model_runs_from_snapshot(&snap);
        for expected in ["adc", "v724", "pro", "vc727p"] {
            assert!(runs.contains(expected), "expected {expected} in {runs:?}");
        }
    }

    #[test]
    fn run_term_frequency_counts_occurrences_and_caps_bigram_rescue_at_one() {
        // 直接一致は非重複の出現回数
        assert_eq!(
            run_term_frequency("パスワード", "パスワードとパスワード"),
            2
        );
        assert_eq!(run_term_frequency("パスワード", "本文にはありません"), 0);
        // bigram 救済（漢字 run 限定）は出現回数を定義できないので非ゼロ最小の 1 に固定する
        assert_eq!(
            run_term_frequency("設定方法", "設定を開き、方法を選択します。設定と方法。"),
            1
        );
        // カタカナ run は断片救済しない（既存の回帰: バックアップ誤マッチ）
        assert_eq!(
            run_term_frequency("バックアップ", "アプリをチェックしてアップデートします。"),
            0
        );
        // 空 run は本文長に化けさせない（防御）
        assert_eq!(run_term_frequency("", "何かの本文"), 0);
    }

    #[test]
    fn saturating_run_weight_clamps_to_one_and_keeps_the_v1_full_credit_condition() {
        // tf=1 かつ len==avg_len でちょうど 1.0 ── v1（bool 一致 = 1.0）の満点条件が
        // 上限として保存される。これが回答可能性の 3 閾値を取り直さずに済む根拠。
        let at_average = saturating_run_weight(
            1,
            DocLength {
                chars: 1000,
                avg_chars: 1000.0,
            },
        );
        assert!(
            (at_average - 1.0).abs() < 1e-6,
            "tf=1 at average length must be exactly 1.0, got {at_average}"
        );
        // raw > 1 になる入力（多数回言及 × 平均より短い本文）でも 1.0 を超えない
        let raw_above_one = saturating_run_weight(
            50,
            DocLength {
                chars: 100,
                avg_chars: 1000.0,
            },
        );
        assert_eq!(
            raw_above_one, 1.0,
            "min clamp must cap the weight at 1.0 (score は 0..=1 を保つ契約)"
        );
        // 平均より長い記事の薄い言及だけが 1.0 未満に落ちる（本改修の実体）
        let long_and_thin = saturating_run_weight(
            1,
            DocLength {
                chars: 5000,
                avg_chars: 1000.0,
            },
        );
        assert!(
            long_and_thin > 0.0 && long_and_thin < 1.0,
            "a single mention in a long document must be discounted, got {long_and_thin}"
        );
        // 不一致は 0
        assert_eq!(
            saturating_run_weight(
                0,
                DocLength {
                    chars: 100,
                    avg_chars: 1000.0
                }
            ),
            0.0
        );
        // avg_len=0（空コーパス / 全節が空本文）でゼロ除算 → NaN にならない
        let degenerate = saturating_run_weight(
            1,
            DocLength {
                chars: 0,
                avg_chars: 0.0,
            },
        );
        assert!(
            degenerate.is_finite() && (0.0..=1.0).contains(&degenerate),
            "avg_len=0 must degrade, not produce NaN; got {degenerate}"
        );
    }

    #[test]
    fn scoring_v1_reproduces_the_production_misranking() {
        // kill switch off の回帰担保。本番実測どおり ②帯域幅 > ①固定IP > ③正解 になり、
        // かつ ③④⑤ が完全同点（本番で観測された 0.6028153896331787 の 3 件同着）になる。
        // この同点が判定の非決定性の出どころなので、v1 の性質としてここに固定しておく。
        let scored = score_alarmcom_articles(&ManualScoring::default());
        let answer = score_of(&scored, ANSWER_ARTICLE);
        assert!(
            score_of(&scored, "②帯域幅") > score_of(&scored, "①固定IP"),
            "v1 ranking changed: {scored:?}"
        );
        assert!(
            score_of(&scored, "①固定IP") > answer,
            "v1 must still rank the irrelevant static-IP article above the answer: {scored:?}"
        );
        assert_eq!(
            answer,
            score_of(&scored, "④ログインできない"),
            "v1 ties the answer with the login article: {scored:?}"
        );
        assert_eq!(
            answer,
            score_of(&scored, "⑤パートナー様向け(ハブ)"),
            "v1 ties the answer with the hub page: {scored:?}"
        );
    }

    // connect_lazy が Tokio ランタイム下での呼び出しを要求するため #[tokio::test]。
    #[tokio::test]
    async fn scoring_v2_ranks_the_password_reset_article_first() {
        let ranked = rank_alarmcom_through_search(true).await;
        assert_eq!(
            ranked.first().map(String::as_str),
            Some(ANSWER_SECTION_KEY),
            "the answering article must rank first: {ranked:?}"
        );
    }

    /// **カバレッジスコアだけでは順位が決まらないことを固定する。**
    ///
    /// 型番除外 + TF + 長さ正規化を入れても、①③④⑤ は**すべて 1.0 で同点**になる
    /// （満点条件が `tf ≥ 長さ係数` なので、5,175 字の記事でも `パスワード` 2 回で届く）。
    /// 落ちるのは `忘` を 1 度も含まない ②帯域幅 だけである。
    ///
    /// このテストが無いと、「同点は残っているが section_key の巡り合わせで正解が上に来た」
    /// 状態を「直った」と誤認する。順位を決めているのが密度であることを、スコアの同点と
    /// セットで固定する。
    #[tokio::test]
    async fn scoring_v2_still_ties_on_coverage_so_density_is_what_orders_them() {
        let scored = score_alarmcom_articles(&scoring_v2());
        let answer = score_of(&scored, ANSWER_ARTICLE);
        assert!(
            (answer - score_of(&scored, "①固定IP")).abs() < 1e-6,
            "coverage is expected to tie here; if this changed, the density tiebreak may no \
             longer be what fixes the ranking: {scored:?}"
        );
        // 順位は production の検索経路で決まる。
        let ranked = rank_alarmcom_through_search(true).await;
        let position = |key: &str| {
            ranked
                .iter()
                .position(|k| k == key)
                .unwrap_or_else(|| panic!("{key} missing from {ranked:?}"))
        };
        for key in MUST_RANK_BELOW_KEYS {
            assert!(
                position(ANSWER_SECTION_KEY) < position(key),
                "answer must outrank {key}: {ranked:?}"
            );
        }
        // 返信文の材料は上位 3 件（`harness::reply` の MAX_EXCERPTS）。無関係な 2 記事が
        // そこへ入らないことが、下書き汚染を止める実質的な条件である。
        let material: Vec<&str> = ranked.iter().take(3).map(String::as_str).collect();
        for key in [KEY_STATIC_IP, KEY_BANDWIDTH] {
            assert!(
                !material.contains(&key),
                "{key} is irrelevant and must not become reply material: {material:?}"
            );
        }
    }

    /// kill switch off では、密度 tiebreak も含めて従来どおりの並びになる。
    ///
    /// section_key は正解記事が最後に来るよう意図的に振ってあるので、v1 では
    /// **同点 → section_key 昇順**で正解が最下位に沈む。これは本番で観測された
    /// 「③④⑤ が同点で並ぶ」状態そのものであり、v2 の効果を測る基準線になる。
    #[tokio::test]
    async fn scoring_v1_leaves_the_answer_buried_by_the_section_key_tiebreak() {
        let ranked = rank_alarmcom_through_search(false).await;
        assert_eq!(
            ranked.last().map(String::as_str),
            Some(ANSWER_SECTION_KEY),
            "v1 must reproduce the buried answer (this is the baseline v2 has to beat): {ranked:?}"
        );
    }

    #[test]
    fn scoring_v2_keeps_the_answer_above_the_answerability_threshold() {
        let scored = score_alarmcom_articles(&scoring_v2());
        let answer = score_of(&scored, ANSWER_ARTICLE);
        assert!(
            answer > ANSWERABILITY_LOW_THRESHOLD,
            "answer must stay answerable (> {ANSWERABILITY_LOW_THRESHOLD}), got {answer}: {scored:?}"
        );
    }

    #[test]
    fn scoring_v2_keeps_every_score_within_zero_and_one() {
        // 閾値（0.6 / 0.8 / 0.95）と絶対値で比較される契約なので、上限 1.0 を割らせない。
        let scored = score_alarmcom_articles(&scoring_v2());
        for (label, score) in &scored {
            assert!(
                score.is_finite() && (0.0..=1.0).contains(score),
                "{label} score out of range: {score} ({scored:?})"
            );
        }
    }

    #[test]
    fn scoring_v2_leaves_average_or_shorter_documents_unchanged() {
        // design の主張「平均長以下の記事は、1 回の言及でも現行どおり満点を得る」を固定する。
        // 単一節コーパスは len == avg_len なので、tf >= 1 の run は min クランプで 1.0 に
        // 張り付き、v1（bool 一致）と完全に同値になる ── これが「閾値を取り直さずに済む」
        // 根拠であり、既存の score_one 系テストが v2 でも壊れない理由でもある。
        let cases = [
            ("録画ルールの設定方法", "録画ルールの設定方法を説明します。設定画面から録画ルールを選択してください。"),
            ("SDカードの推奨メーカーはどこか", "SDカードを一度抜き差ししてください。カードの向きを確認し、カチッと音がするまで挿入します。"),
            ("カメラを浴室に設置できるか", "設置までのステップを説明します。壁面への取り付けは付属のブラケットを使用します。"),
        ];
        for (question, body) in cases {
            let bodies = [body.to_string()];
            let v1 = score_against_corpus(question, &bodies, &ManualScoring::default());
            let v2 = score_against_corpus(question, &bodies, &scoring_v2());
            assert_eq!(
                v1, v2,
                "v2 must not move scores for average-or-shorter documents (question={question})"
            );
        }
    }

    #[test]
    fn model_number_only_question_falls_back_to_legacy_scoring() {
        // 「ADC-V724」だけの質問は型番除外で run が空になる。既存の「run が取れない質問」と
        // 同じ legacy フォールバック（crate::mcp::section_score）へ落ちることを、legacy を
        // 直接呼んだ結果との一致で固定する。
        let question = "ADC-V724";
        let bodies: Vec<String> = alarmcom_articles()
            .into_iter()
            .map(|(_, body)| body)
            .collect();
        let got = score_against_corpus(question, &bodies, &scoring_v2());
        let query_norm = normalize_key(question);
        let want: Vec<f32> = bodies
            .iter()
            .map(|b| crate::mcp::section_score(&query_norm, question, b))
            .collect();
        assert_eq!(got, want, "model-only question must use the legacy path");
        // 非空虚性: legacy 経路が 1 件でも非ゼロを返すコーパスで検証している
        // （全 0 なら「型番除外で run が消えたから 0」と区別できない）。
        assert!(
            want.iter().any(|s| *s > 0.0),
            "fixture must exercise a non-trivial legacy score: {want:?}"
        );
    }

    #[test]
    fn fold_max_scores_keeps_the_larger_of_duplicate_ids() {
        // 同一 node_id が複数回来た場合、低スコアが後に来ても max が採用される
        // （順序に依存しない）ことを直接検証する（codex レビュー Suggestion 対応）。
        let hits = vec![
            ("p1".to_string(), 0.3_f32),
            ("p1".to_string(), 0.9_f32),
            ("p1".to_string(), 0.1_f32),
        ];
        let map = fold_max_scores(&hits);
        assert_eq!(map.get("p1"), Some(&0.9));
    }

    fn product_row(node_id: &str, model: &str, name: &str, fuzzy_score: f32) -> ProductRow {
        ProductRow {
            node_id: node_id.to_string(),
            model: model.to_string(),
            name: name.to_string(),
            fuzzy_score,
        }
    }

    #[test]
    fn merge_vector_boosts_existing_candidate_and_flips_reason() {
        // fuzzy だけでは 0.4 (fuzzy_match) だが、vector が 0.9 で上回る → score=max=0.9、
        // reason は "semantic_nearby" に切り替わる（vector が最終スコアの主因になったため）。
        let rows = vec![product_row("p1", "ADC-V724", "屋外カメラ", 0.4)];
        let vector = [("p1".to_string(), 0.9_f32)];
        let out = merge_product_candidates(rows, &vector);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model, "ADC-V724");
        assert_eq!(out[0].score, 0.9);
        assert_eq!(out[0].reason, "semantic_nearby");
    }

    #[test]
    fn merge_vector_only_candidate_added_as_semantic_nearby() {
        // fuzzy score が閾値(0.1)以下で本来は候補から落ちる行でも、vector hit があれば
        // 候補に加わり、reason="semantic_nearby"。name は query_nodes 由来の行から取る。
        let rows = vec![
            product_row("p1", "ADC-V724", "屋外カメラ", 0.4),
            product_row("p2", "SVR-HB100", "ホームハブ", 0.0),
        ];
        let vector = [("p2".to_string(), 0.55_f32)];
        let out = merge_product_candidates(rows, &vector);
        let p2 = out
            .iter()
            .find(|c| c.model == "SVR-HB100")
            .expect("vector-only candidate must be present");
        assert_eq!(p2.name, "ホームハブ");
        assert_eq!(p2.score, 0.55);
        assert_eq!(p2.reason, "semantic_nearby");
    }

    #[test]
    fn merge_pure_fuzzy_keeps_existing_reasons_when_vector_absent_or_lower() {
        // vector hit が無い行は従来通り。fuzzy>=1.0 は normalized_match、それ未満は fuzzy_match。
        let rows = vec![
            product_row("p1", "ADC-V724", "屋外カメラ", 1.0),
            product_row("p2", "SVR-HB100", "ホームハブ", 0.4),
        ];
        let out = merge_product_candidates(rows, &[]);
        let p1 = out.iter().find(|c| c.model == "ADC-V724").unwrap();
        let p2 = out.iter().find(|c| c.model == "SVR-HB100").unwrap();
        assert_eq!(p1.reason, "normalized_match");
        assert_eq!(p2.reason, "fuzzy_match");

        // vector が来ても fuzzy 以下なら reason は変わらない（同点は fuzzy 側を優先）。
        let rows2 = vec![product_row("p1", "ADC-V724", "屋外カメラ", 1.0)];
        let out2 = merge_product_candidates(rows2, &[("p1".to_string(), 1.0)]);
        assert_eq!(out2[0].reason, "normalized_match");
        assert_eq!(out2[0].score, 1.0);
    }

    #[test]
    fn merge_below_threshold_and_no_vector_hit_is_excluded() {
        let rows = vec![product_row("p1", "ADC-V724", "屋外カメラ", 0.05)];
        let out = merge_product_candidates(rows, &[]);
        assert!(
            out.is_empty(),
            "score<=0.1 with no vector hit must be excluded"
        );
    }

    #[test]
    fn merge_orders_by_score_desc_then_model_asc_deterministically() {
        // 同点スコアは model 昇順で決定論的に並ぶ（実行順や snapshot 順に依存しない）。
        let rows = vec![
            product_row("p1", "ZZZ-100", "後半モデル", 0.5),
            product_row("p2", "AAA-100", "前半モデル", 0.5),
            product_row("p3", "MMM-100", "最高スコア", 0.9),
        ];
        let out = merge_product_candidates(rows, &[]);
        let models: Vec<&str> = out.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(models, vec!["MMM-100", "AAA-100", "ZZZ-100"]);
    }

    #[test]
    fn merge_truncates_to_five_candidates() {
        let rows: Vec<ProductRow> = (0..8)
            .map(|i| product_row(&format!("p{i}"), &format!("M-{i}"), "name", 0.9))
            .collect();
        let out = merge_product_candidates(rows, &[]);
        assert_eq!(out.len(), 5);
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
            &ManualScoring::default(),
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
        let hits = score_against_corpus(
            "カメラを壁面に設置できますか",
            &sections,
            &ManualScoring::default(),
        );
        // 設置節は全 run（カメラ・壁面・設置）を含むので高スコア
        assert!(
            hits.iter().any(|s| *s > 0.6),
            "expected some > 0.6, got {hits:?}"
        );
    }

    /// 実ネットワークに繋がない dummy client（connect_lazy は遅延接続で即座に返る）。
    fn dummy_store() -> ManualStore {
        let client = Arc::new(
            crate::vegapunk::VegapunkClient::connect_lazy("http://127.0.0.1:1", "test")
                .expect("connect_lazy"),
        );
        let corpus = Arc::new(crate::corpus::CorpusLoader::new(client.clone()));
        // search_with_snapshot 系の既存テストは v1（kill switch off）の挙動を見る。
        ManualStore::new(client, corpus, false)
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
                &[],
            )
            .expect("search_with_snapshot");
        let keys: BTreeSet<&str> = hits.iter().map(|h| h.section_key.as_str()).collect();
        assert!(keys.contains("sec-a"), "expected sec-a in {keys:?}");
        assert!(keys.contains("sec-c"), "expected sec-c in {keys:?}");
        assert!(!keys.contains("sec-b"), "sec-b must be excluded: {keys:?}");
    }

    /// vector_hits マージ用のテスト snapshot ビルダ。1 節の body は質問と無関係
    /// （text score = 0）にしておき、vector_hits の寄与だけを見る。
    fn single_section_snapshot(
        schema: &str,
        section_key: &str,
        body: &str,
        describes_product: Option<&str>,
    ) -> GetGraphSnapshotResponse {
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge as PE, GraphNode as PN};
        use std::collections::HashMap;
        let attrs: HashMap<String, String> = [
            ("section_key".to_string(), section_key.to_string()),
            ("title".to_string(), "見出し".to_string()),
            ("body".to_string(), body.to_string()),
            ("source_url".to_string(), String::new()),
            ("breadcrumb".to_string(), String::new()),
        ]
        .into_iter()
        .collect();
        let section = PN {
            node_id: manual_node_id(schema, "ManualSection", section_key),
            node_type: "ManualSection".to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: attrs,
        };
        let mut nodes = vec![section.clone()];
        let mut edges = Vec::new();
        if let Some(pk) = describes_product {
            nodes.push(PN {
                node_id: manual_node_id(schema, KIND_PRODUCT, pk),
                node_type: "Product".to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: HashMap::new(),
            });
            edges.push(PE {
                edge_id: String::new(),
                from_id: section.node_id.clone(),
                to_id: manual_node_id(schema, KIND_PRODUCT, pk),
                edge_type: "DESCRIBES".to_string(),
            });
        }
        GetGraphSnapshotResponse {
            nodes,
            edges,
            truncated: false,
            total_node_count: 0,
        }
    }

    #[tokio::test]
    async fn vector_only_hit_enters_candidates_with_vector_score_source() {
        // body は質問と無関係 → text score = 0。vector_hits にだけ載る節は
        // score = vector score, score_source = "vector" で候補に入る。
        let schema = "urtect";
        let section_key = "sec-vec-only";
        let snap = single_section_snapshot(schema, section_key, "全く関係のない本文です。", None);
        let node_id = manual_node_id(schema, "ManualSection", section_key);
        let store = dummy_store();
        let hits = store
            .search_with_snapshot(
                schema,
                "SDカードが認識されない場合の対処",
                &SignalSet::new(),
                None,
                10,
                &snap,
                &[(node_id, 0.42)],
            )
            .expect("search_with_snapshot");
        let hit = hits
            .iter()
            .find(|h| h.section_key == section_key)
            .expect("vector-only section must be a candidate");
        assert_eq!(hit.score, 0.42);
        assert_eq!(hit.score_source, "vector");
    }

    #[tokio::test]
    async fn both_routes_merge_to_max_score_with_both_source() {
        // body が質問の完全部分文字列 → text score = 1.0（fast path）。
        // vector_hits には 1.0 より低いスコアを与え、max = text score = 1.0 になることを確認する。
        // さらに別ケースで vector のほうが高いときも max がその値になることを確認する。
        let schema = "urtect";
        let question = "SDカードが認識されない場合の対処";
        let section_key = "sec-both";
        let snap = single_section_snapshot(schema, section_key, question, None);
        let node_id = manual_node_id(schema, "ManualSection", section_key);
        let store = dummy_store();
        let hits = store
            .search_with_snapshot(
                schema,
                question,
                &SignalSet::new(),
                None,
                10,
                &snap,
                &[(node_id.clone(), 0.3)],
            )
            .expect("search_with_snapshot");
        let hit = hits
            .iter()
            .find(|h| h.section_key == section_key)
            .expect("section must be a candidate");
        assert_eq!(hit.score, 1.0, "max(text=1.0, vector=0.3) must be 1.0");
        assert_eq!(hit.score_source, "both");
    }

    #[tokio::test]
    async fn signal_only_hit_has_signal_score_source() {
        // body は質問と無関係（text score = 0）、vector_hits も渡さない（vector score = 0）。
        // signal 絞り込み（MENTIONS_SIGNAL）だけで候補に残った節の score_source は、
        // テキスト一致したかのような偽りの "text" ではなく "signal" であるべき
        // （signal 絞り込みのみで候補に残った節の正直なラベル）。
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge as PE, GraphNode as PN};
        let schema = "urtect";
        let section_key = "sec-signal-only";
        let node_id = manual_node_id(schema, "ManualSection", section_key);
        let signal_node_id = "urtect:gen1:Signal:sd_not_recognized".to_string();
        let section = PN {
            node_id: node_id.clone(),
            node_type: "ManualSection".to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: [
                ("section_key".to_string(), section_key.to_string()),
                ("title".to_string(), "見出し".to_string()),
                ("body".to_string(), "全く関係のない本文です。".to_string()),
                ("source_url".to_string(), String::new()),
                ("breadcrumb".to_string(), String::new()),
            ]
            .into_iter()
            .collect(),
        };
        let signal_node = PN {
            node_id: signal_node_id.clone(),
            node_type: "Signal".to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: [("value".to_string(), "sd_not_recognized".to_string())]
                .into_iter()
                .collect(),
        };
        let snap = GetGraphSnapshotResponse {
            nodes: vec![section, signal_node],
            edges: vec![PE {
                edge_id: String::new(),
                from_id: node_id,
                to_id: signal_node_id,
                edge_type: "MENTIONS_SIGNAL".to_string(),
            }],
            truncated: false,
            total_node_count: 0,
        };
        let signals: SignalSet = [Signal::new("sd_not_recognized")].into_iter().collect();
        let store = dummy_store();
        let hits = store
            .search_with_snapshot(
                schema,
                "SDカードが認識されない場合の対処",
                &signals,
                None,
                10,
                &snap,
                &[],
            )
            .expect("search_with_snapshot");
        let hit = hits
            .iter()
            .find(|h| h.section_key == section_key)
            .expect("signal-narrowed section with no text/vector match must still be a candidate");
        assert_eq!(hit.score, 0.0);
        assert_eq!(hit.score_source, "signal");
    }

    #[tokio::test]
    async fn degrades_gracefully_when_snapshot_has_no_mentions_signal_edges() {
        // corpus loader は hot Signal の traverse timeout を避けるため MENTIONS_SIGNAL 辺を
        // 載せなくなった。その結果 snapshot は Signal ノードを持つが section->Signal 辺を持たない。
        // このとき、質問が signal を運んでいても:
        //   (a) text/vector スコアが立つ節は従来どおり候補として返る（検索は生き続ける）、
        //   (b) signal 絞り込みだけで残っていた節（text/vector=0）は候補から落ちる（縮退）。
        // を search_with_snapshot 層で直接固定する。
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphNode as PN};
        let schema = "urtect";
        let question = "SDカードが認識されない場合の対処";
        let signal_node = PN {
            node_id: "urtect:gen1:Signal:sd_not_recognized".to_string(),
            node_type: "Signal".to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: [("value".to_string(), "sd_not_recognized".to_string())]
                .into_iter()
                .collect(),
        };
        let mk_section = |key: &str, body: &str| -> PN {
            PN {
                node_id: manual_node_id(schema, "ManualSection", key),
                node_type: "ManualSection".to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: [
                    ("section_key".to_string(), key.to_string()),
                    ("title".to_string(), "見出し".to_string()),
                    ("body".to_string(), body.to_string()),
                    ("source_url".to_string(), String::new()),
                    ("breadcrumb".to_string(), String::new()),
                ]
                .into_iter()
                .collect(),
            }
        };
        // text_match: 本文が質問を含む（text score が立つ）。signal_only: 本文が無関係で、
        // 従来なら MENTIONS_SIGNAL 辺だけで候補に残っていた節。辺が無いので今回は落ちる。
        let text_match = mk_section("sec-text-match", question);
        let signal_only = mk_section("sec-signal-only", "全く関係のない本文です。");
        let snap = GetGraphSnapshotResponse {
            nodes: vec![signal_node, text_match, signal_only],
            edges: Vec::new(), // ← MENTIONS_SIGNAL 辺を一切持たない（corpus loader の新挙動）
            truncated: false,
            total_node_count: 0,
        };
        let signals: SignalSet = [Signal::new("sd_not_recognized")].into_iter().collect();
        let store = dummy_store();
        let hits = store
            .search_with_snapshot(schema, question, &signals, None, 10, &snap, &[])
            .expect("search_with_snapshot");
        assert!(
            hits.iter().any(|h| h.section_key == "sec-text-match"),
            "text-matching section must still be returned without signal edges: {hits:?}"
        );
        assert!(
            hits.iter().all(|h| h.section_key != "sec-signal-only"),
            "signal-only section must drop when MENTIONS_SIGNAL edges are absent: {hits:?}"
        );
    }

    #[tokio::test]
    async fn vector_id_absent_from_snapshot_is_ignored() {
        let schema = "urtect";
        let section_key = "sec-real";
        let snap = single_section_snapshot(schema, section_key, "全く関係のない本文です。", None);
        let ghost_id = manual_node_id(schema, "ManualSection", "sec-does-not-exist");
        let store = dummy_store();
        let hits = store
            .search_with_snapshot(
                schema,
                "SDカードが認識されない場合の対処",
                &SignalSet::new(),
                None,
                10,
                &snap,
                &[(ghost_id, 0.9)],
            )
            .expect("search_with_snapshot");
        assert!(
            hits.iter().all(|h| h.section_key != "sec-does-not-exist"),
            "vector hit absent from snapshot must not fabricate a candidate: {hits:?}"
        );
        assert!(
            hits.iter().all(|h| h.section_key != section_key),
            "sec-real has no text/signal/vector match and must not appear either: {hits:?}"
        );
    }

    #[tokio::test]
    async fn vector_candidate_excluded_when_product_key_filter_excludes_it() {
        // section は product B のみを DESCRIBES する（他機種専用）。product_key=A で絞り込むと、
        // vector_hits に高スコアで載っていても候補から除外されなければならない。
        let schema = "urtect";
        let product_a = "ADC-V724";
        let product_b = "ADC-VC727P";
        let section_key = "sec-other-product";
        let snap = single_section_snapshot(
            schema,
            section_key,
            "全く関係のない本文です。",
            Some(product_b),
        );
        let node_id = manual_node_id(schema, "ManualSection", section_key);
        let store = dummy_store();
        let hits = store
            .search_with_snapshot(
                schema,
                "SDカードが認識されない場合の対処",
                &SignalSet::new(),
                Some(product_a),
                10,
                &snap,
                &[(node_id, 0.95)],
            )
            .expect("search_with_snapshot");
        assert!(
            hits.iter().all(|h| h.section_key != section_key),
            "product filter must exclude the vector candidate too: {hits:?}"
        );
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
            .search_with_snapshot(schema, question, &SignalSet::new(), None, 10, &snap, &[])
            .expect("search_with_snapshot");
        let keys: BTreeSet<&str> = hits.iter().map(|h| h.section_key.as_str()).collect();
        assert!(keys.contains("sec-a"));
        assert!(keys.contains("sec-b"));
        assert!(keys.contains("sec-c"));
    }

    /// 機種横断 how-to（例: パスワードリセット）が product スコープで沈み、answerable なのに
    /// best_manual_score から消える false-escalate の回帰テスト。`evaluate`（ManualV1）が
    /// coverage 判定を product_key=None で走らせるようにした変更（harness/mod.rs）が前提とする
    /// 性質を search_with_snapshot 層で直接検証する。
    /// 構成: 横断ページ sec-howto は product B のみを DESCRIBES（他機種専用扱い）だが本文は質問の
    /// 完全部分文字列で fast path 1.0。product A 専用ページ sec-a は本文が質問と弱くしか一致しない。
    #[tokio::test]
    async fn cross_product_howto_reaches_best_score_only_when_unscoped() {
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge as PE, GraphNode as PN};
        use std::collections::HashMap;
        let schema = "urtect";
        let product_a = "ADC-V724";
        let product_b = "ADC-VC727P";
        let question = "パスワードをリセットする方法";
        let section_node = |key: &str, body: &str| -> PN {
            let attrs: HashMap<String, String> = [
                ("section_key".to_string(), key.to_string()),
                ("title".to_string(), key.to_string()),
                ("body".to_string(), body.to_string()),
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
                // 横断 how-to: 本文＝質問（fast path 1.0）。product B のみを DESCRIBES する。
                section_node("sec-howto", question),
                // product A 専用ページ: 「方法」だけ一致する弱いページ（best にならない）。
                section_node("sec-a", "カメラの設置方法について説明します。"),
                product_node(product_a),
                product_node(product_b),
            ],
            edges: vec![
                describes_edge("sec-howto", product_b),
                describes_edge("sec-a", product_a),
            ],
            truncated: false,
            total_node_count: 0,
        };
        let store = dummy_store();

        // product A で hard-scope すると横断ページ(sec-howto)は「他機種のみ DESCRIBES」で除外され、
        // best は弱い sec-a に落ちる（＝ answerable なのに coverage が下がる false-escalate の芽）。
        let scoped = store
            .search_with_snapshot(
                schema,
                question,
                &SignalSet::new(),
                Some(product_a),
                10,
                &snap,
                &[],
            )
            .expect("search_with_snapshot scoped");
        assert!(
            scoped.iter().all(|h| h.section_key != "sec-howto"),
            "product scope must drop the cross-product how-to: {scoped:?}"
        );
        let scoped_best = scoped.first().map(|h| h.score).unwrap_or(0.0);
        assert!(
            scoped_best < 1.0,
            "scoped best must be the weak product page, not the 1.0 how-to: {scoped_best}"
        );

        // product_key=None（evaluate の coverage 検索が使う経路）なら横断ページが候補に戻り、
        // best_manual_score が 1.0 になる（＝沈まず反映される）。
        let unscoped = store
            .search_with_snapshot(schema, question, &SignalSet::new(), None, 10, &snap, &[])
            .expect("search_with_snapshot unscoped");
        let best = unscoped.first().expect("at least one hit");
        assert_eq!(best.section_key, "sec-howto");
        assert_eq!(
            best.score, 1.0,
            "cross-product how-to must drive best_manual_score when unscoped"
        );
    }

    #[test]
    fn product_view_from_snapshot_returns_product_describing_sections_and_ordered_toc() {
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge as PE, GraphNode as PN};
        use std::collections::HashMap;
        let schema = "urtect";
        let product_key = "SVR-HB100";
        let product_node = PN {
            node_id: manual_node_id(schema, "Product", product_key),
            node_type: "Product".to_string(),
            display_text: String::new(),
            degree: 0,
            community: None,
            attributes: [
                ("product_key".to_string(), product_key.to_string()),
                ("name".to_string(), "サンプル製品".to_string()),
                ("model".to_string(), product_key.to_string()),
                ("aliases".to_string(), String::new()),
            ]
            .into_iter()
            .collect(),
        };
        let section_node = |key: &str, order: i32| -> PN {
            let attrs: HashMap<String, String> = [
                ("section_key".to_string(), key.to_string()),
                ("doc_key".to_string(), "doc-1".to_string()),
                ("title".to_string(), format!("title-{key}")),
                ("order".to_string(), order.to_string()),
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
        let sec_a = section_node("sec-a", 1);
        let sec_b = section_node("sec-b", 2);
        let snap = GetGraphSnapshotResponse {
            nodes: vec![product_node.clone(), sec_a.clone(), sec_b.clone()],
            edges: vec![
                PE {
                    edge_id: String::new(),
                    from_id: sec_a.node_id.clone(),
                    to_id: product_node.node_id.clone(),
                    edge_type: "DESCRIBES".to_string(),
                },
                PE {
                    edge_id: String::new(),
                    from_id: manual_node_id(schema, "ManualDocument", "doc-1"),
                    to_id: sec_a.node_id.clone(),
                    edge_type: "HAS_SECTION".to_string(),
                },
                PE {
                    edge_id: String::new(),
                    from_id: manual_node_id(schema, "ManualDocument", "doc-1"),
                    to_id: sec_b.node_id.clone(),
                    edge_type: "HAS_SECTION".to_string(),
                },
                PE {
                    edge_id: String::new(),
                    from_id: sec_a.node_id.clone(),
                    to_id: sec_b.node_id.clone(),
                    edge_type: "PARENT_OF".to_string(),
                },
            ],
            truncated: false,
            total_node_count: 0,
        };
        let view = product_view_from_snapshot(product_key, &snap).expect("product view");
        assert_eq!(
            view.product["describing_section_keys"],
            serde_json::json!(["sec-a"])
        );
        assert_eq!(view.specs.len(), 0);
        assert_eq!(view.toc.len(), 2);
        assert_eq!(view.toc[0]["section_key"], serde_json::json!("sec-a"));
        assert_eq!(view.toc[0]["parent"], serde_json::Value::Null);
        assert_eq!(view.toc[1]["section_key"], serde_json::json!("sec-b"));
        assert_eq!(view.toc[1]["parent"], serde_json::json!("sec-a"));
    }

    #[test]
    fn product_view_from_snapshot_excludes_sections_describing_only_other_product() {
        // 実データ想定: 1 document を product A/B が共有し、各節が特定機種のみを説明する。
        // s1: A のみ DESCRIBES / s2: B のみ DESCRIBES（他機種専用 → 除外）/ s3: DESCRIBES 辺なし（機種非依存 → 維持）。
        use crate::proto::graphrag::{GetGraphSnapshotResponse, GraphEdge as PE, GraphNode as PN};
        use std::collections::HashMap;
        let schema = "urtect";
        let product_a = "SVR-HB100";
        let product_b = "SVR-HB200";
        let product_node = |key: &str| -> PN {
            PN {
                node_id: manual_node_id(schema, "Product", key),
                node_type: "Product".to_string(),
                display_text: String::new(),
                degree: 0,
                community: None,
                attributes: [
                    ("product_key".to_string(), key.to_string()),
                    ("name".to_string(), format!("製品-{key}")),
                    ("model".to_string(), key.to_string()),
                    ("aliases".to_string(), String::new()),
                ]
                .into_iter()
                .collect(),
            }
        };
        let section_node = |key: &str, order: i32| -> PN {
            let attrs: HashMap<String, String> = [
                ("section_key".to_string(), key.to_string()),
                ("doc_key".to_string(), "doc-1".to_string()),
                ("title".to_string(), format!("title-{key}")),
                ("order".to_string(), order.to_string()),
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
        let pa = product_node(product_a);
        let pb = product_node(product_b);
        let s1 = section_node("s1", 1);
        let s2 = section_node("s2", 2);
        let s3 = section_node("s3", 3);
        let describes_edge = |section: &PN, product: &PN| -> PE {
            PE {
                edge_id: String::new(),
                from_id: section.node_id.clone(),
                to_id: product.node_id.clone(),
                edge_type: "DESCRIBES".to_string(),
            }
        };
        let has_section_edge = |section: &PN| -> PE {
            PE {
                edge_id: String::new(),
                from_id: manual_node_id(schema, "ManualDocument", "doc-1"),
                to_id: section.node_id.clone(),
                edge_type: "HAS_SECTION".to_string(),
            }
        };
        let snap = GetGraphSnapshotResponse {
            nodes: vec![pa.clone(), pb.clone(), s1.clone(), s2.clone(), s3.clone()],
            edges: vec![
                describes_edge(&s1, &pa),
                describes_edge(&s2, &pb),
                has_section_edge(&s1),
                has_section_edge(&s2),
                has_section_edge(&s3),
                // s3 の親は s2（除外節）。除外されても親参照は捏造・null 化せずそのまま返す。
                PE {
                    edge_id: String::new(),
                    from_id: s2.node_id.clone(),
                    to_id: s3.node_id.clone(),
                    edge_type: "PARENT_OF".to_string(),
                },
            ],
            truncated: false,
            total_node_count: 0,
        };
        let view = product_view_from_snapshot(product_a, &snap).expect("product view");
        let toc_keys: Vec<&str> = view
            .toc
            .iter()
            .map(|v| v["section_key"].as_str().unwrap())
            .collect();
        assert_eq!(
            toc_keys,
            vec!["s1", "s3"],
            "s2 (describes only product B) must be excluded from A's toc: {toc_keys:?}"
        );
        let s3_view = view
            .toc
            .iter()
            .find(|v| v["section_key"] == "s3")
            .expect("s3 present in toc");
        assert_eq!(
            s3_view["parent"],
            serde_json::json!("s2"),
            "parent reference to an excluded section must be kept as-is, not fabricated"
        );
    }

    #[test]
    fn product_view_from_snapshot_errors_when_product_missing() {
        use crate::proto::graphrag::GetGraphSnapshotResponse;
        let snap = GetGraphSnapshotResponse {
            nodes: vec![],
            edges: vec![],
            truncated: false,
            total_node_count: 0,
        };
        let err = product_view_from_snapshot("NOPE", &snap).unwrap_err();
        assert!(err.to_string().contains("product not found: NOPE"));
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
