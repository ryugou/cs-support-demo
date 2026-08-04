//! `ManualSection.concept_keys` backfill / 観測 CLI（Issue #8 Phase B2-0/B2-1）。
//!
//! 設計・仕様の正本は `docs/superpowers/specs/2026-08-02-concept-expansion-design.md`。
//! `MENTIONS_CONCEPT` 辺はグラフの正、`concept_keys` 属性はその読み取り最適化の射影であり、
//! 食い違ったら辺が正。
//!
//! 4 モード（相互排他。同時指定は clap が拒否する）:
//! - `--probe-only`: 書き込まない。ManualSection 件数・Concept 件数・fan-in 分布を JSON で出す
//!   （B2-0 の実測に使う）
//! - `--verify`: 書き込まない。辺と `concept_keys` 属性の乖離件数を出す
//! - `--probe-one <section_key>`: 指定 1 件だけに `concept_keys` を書き、`UpsertNodes` の意味論
//!   （部分マージか全置換か）を実測してから即座に全属性で復旧する
//! - （既定）: 全 ManualSection に `concept_keys` を書き込む（既に一致していれば skip、冪等）
use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    manual::schema_ids::{with_schema_name, KIND_CONCEPT, KIND_SECTION},
    model::GraphNode,
    vegapunk::VegapunkClient,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::PathBuf,
};

/// `query_nodes_paged` / `traverse_neighbors_paged` の 1 ページあたり件数。backend 上限 1000。
const PAGE_SIZE: i32 = 1000;

/// 書き込み前に存在を検査する必須属性。1 つでも欠けた状態で「読み出した全属性 + concept_keys」
/// を再送すると、本文や TOC 順が失われるうえ `retrieval.rs` の `order` は `parse().unwrap_or(0)`
/// のため失敗が静かに進む（spec の安全規定）。
const REQUIRED_ATTRS: [&str; 8] = [
    "section_key",
    "title",
    "body",
    "source_url",
    "breadcrumb",
    "order",
    "source_lang",
    "content_hash",
];

/// missing_required_attrs による skip が対象件数（書き込み候補数）に占める許容比率。
/// 超過したら fail closed する（`ingest_alarmcom.rs` の `site_skip_bail` と同じ規律）。
const MAX_SKIP_RATIO: f64 = 0.10;

/// `upsert_nodes` を発行するバッチサイズ。
const UPSERT_BATCH_SIZE: usize = 50;

#[derive(Debug, Parser)]
struct Args {
    #[arg(
        long,
        env = "VEGAPUNK_ENDPOINT",
        default_value = "http://vegapunk.local:6840"
    )]
    endpoint: String,
    #[arg(long, default_value = "urtect")]
    schema: String,
    #[arg(long, default_value = "VEGAPUNK_BEARER_TOKEN")]
    token_env: String,
    /// bearer token ファイル（主経路）。無い/読めない場合のみ --token-env にフォールバックする。
    #[arg(
        long,
        env = "VEGAPUNK_BEARER_TOKEN_FILE",
        default_value = "/private/tmp/vegapunk-bearer-token"
    )]
    token_file: Option<PathBuf>,
    #[arg(long, default_value = "../schema/cs-support.yml")]
    schema_file: PathBuf,
    /// 書き込まず、Concept 分布（fan-in）を JSON で出す（B2-0 の実測）。
    #[arg(long, conflicts_with_all = ["verify", "probe_one"])]
    probe_only: bool,
    /// 書き込まず、MENTIONS_CONCEPT 辺と concept_keys 属性の乖離を出す。
    #[arg(long, conflicts_with_all = ["probe_only", "probe_one"])]
    verify: bool,
    /// 指定 1 件に concept_keys だけの最小ノードを upsert して UpsertNodes の意味論を実測する。
    #[arg(long, conflicts_with_all = ["probe_only", "verify"])]
    probe_one: Option<String>,
    /// この section_key より後（辞書順）から再開する。**既定の書き込みモード専用。**
    ///
    /// 観測モード（`--probe-only` / `--verify`）では禁止する。母集団を縮めたまま
    /// `ratio`（B2-0 の閾値決定の根拠）や `"diverged": 0`（B2-1 の受理条件）を出すと、
    /// 部分集合に対する結果を全体の結果と誤読させる。spec L93「打ち切る場合は打ち切った旨と
    /// 全件数を必ず出す」の再発防止として、観測モードは打ち切れない形にしておく。
    #[arg(long, conflicts_with_all = ["probe_only", "verify"])]
    start_after: Option<String>,
}

/// `--start-after` の辞書順フィルタ判定（純関数）。
///
/// `start_after` が無ければ全件通す。`section_key` 属性が無いノードは、`start_after` 指定時
/// のみ除外対象になる（辞書順の位置を決められないため）。**呼び出し側はこの除外を必ず warn
/// すること** — フラグの有無で同じ異常がログ有りと無言に分かれるのを避ける。
fn after_start(section_key: Option<&str>, start_after: Option<&str>) -> bool {
    match (section_key, start_after) {
        (_, None) => true,
        (Some(key), Some(after)) => key > after,
        (None, Some(_)) => false,
    }
}

/// token 解決: 既定は --token-file、ファイルが無い/読めない場合のみ --token-env。
/// `ingest_alarmcom.rs` の `read_token` と同じ挙動（Args の型が異なるため関数は複製する）。
fn read_token(args: &Args) -> Result<String> {
    if let Some(path) = &args.token_file {
        match fs::read_to_string(path) {
            Ok(body) => {
                let trimmed = body.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
                tracing::warn!(
                    path = %path.display(),
                    "token file is empty; falling back to --token-env"
                );
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "token file unreadable; falling back to --token-env"
                );
            }
        }
    }
    let token = env::var(&args.token_env).with_context(|| {
        format!(
            "vegapunk bearer token not found (tried --token-file {:?} and env {})",
            args.token_file, args.token_env
        )
    })?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        anyhow::bail!("vegapunk bearer token env {} is empty", args.token_env);
    }
    Ok(trimmed.to_string())
}

/// 1 ManualSection と、それが MENTIONS_CONCEPT で outgoing 到達する Concept の集約。
#[derive(Debug, Clone)]
struct SectionConcepts {
    section_key: String,
    node_id: String,
    /// 辺から集めた concept_key の集合（重複排除・ソート済み。辺が正）。
    concept_keys: Vec<String>,
    /// ManualSection.concept_keys 属性の生値（欠落なら None。射影の現在値）。
    attr_concept_keys: Option<String>,
    /// ManualSection の生の全属性（読み出したそのまま）。書き込み時の全属性再送に使う
    /// （`UpsertNodes` が部分マージか全置換か未確定なため、常に全属性を持たせて再送する）。
    attrs: HashMap<String, String>,
}

/// `collect_section_concepts` の戻り値。**打ち切りの有無を呼び出し側が必ず観測できるよう、
/// 行データだけでなくフィルタ前の件数も返す**（spec L93: 打ち切った旨と全件数を必ず出す。
/// B1 の `--jobs-limit` で「打ち切りに気づけない」問題を踏んだため）。
struct Collected {
    rows: Vec<SectionConcepts>,
    /// concept_key → name_ja（traverse で観測できたものだけ）。
    name_ja: HashMap<String, String>,
    /// `--start-after` フィルタ適用**前**の ManualSection 総数。
    total_before_start_after: usize,
    /// `section_key` 属性が無く `--start-after` フィルタで除外された件数。
    excluded_missing_section_key: usize,
}

impl Collected {
    /// `--start-after` によって母集団が縮んでいるか。summary の `truncated` にそのまま出す。
    fn truncated(&self) -> bool {
        self.rows.len() != self.total_before_start_after
    }
}

/// `--probe-only` の出力（B2-0 の実測 JSON）を組み立てる純関数。
///
/// fan-in の分母は spec で固定された `sections_with_concepts`（`concept_keys` が非空の
/// section 数）。全 section を分母にすると、Concept 抽出が無い urtect 由来 section の分だけ
/// 比率が希釈され、閾値判定が体系的にずれる（`ratio_denominator` で分母を明示する理由）。
/// `name_ja` はこの関数の外（呼び出し側が traverse で得た Concept 属性）から埋める前提で、
/// ここでは空文字を既定値として出す。
fn probe_summary(rows: &[SectionConcepts], truncated: bool) -> Value {
    let total_sections = rows.len();
    let sections_with_concepts = rows.iter().filter(|r| !r.concept_keys.is_empty()).count();

    let mut counts: HashMap<String, usize> = HashMap::new();
    for row in rows {
        for key in &row.concept_keys {
            *counts.entry(key.clone()).or_insert(0) += 1;
        }
    }
    let total_concepts = counts.len();

    let mut ordered: Vec<(String, usize)> = counts.into_iter().collect();
    // section_count 降順 → concept_key 昇順（決定論的な出力順）。
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let concepts: Vec<Value> = ordered
        .into_iter()
        .map(|(concept_key, section_count)| {
            let ratio = if sections_with_concepts == 0 {
                0.0
            } else {
                section_count as f64 / sections_with_concepts as f64
            };
            json!({
                "concept_key": concept_key,
                "name_ja": "",
                "section_count": section_count,
                "ratio": ratio,
            })
        })
        .collect();

    json!({
        "total_sections": total_sections,
        "sections_with_concepts": sections_with_concepts,
        "total_concepts": total_concepts,
        "ratio_denominator": "sections_with_concepts",
        "concepts": concepts,
        "truncated": truncated,
    })
}

/// 全 ManualSection と、それぞれの MENTIONS_CONCEPT outgoing 隣接（Concept）を集約する。
///
/// 戻り値は (行データ, concept_key → name_ja の観測済みマップ)。name_ja は traverse で得た
/// Concept ノードの属性から拾えたときだけ埋める（未 ingest な Concept は無いはずだが、
/// 属性欠落があっても fail closed にはしない。probe 出力の補助情報のため）。
///
/// `query_nodes_paged` / `traverse_neighbors_paged` は取りこぼしを検出したら自身が fail closed
/// で bail するため、ここで受け取る `sections` / `neighbors` は常に完全集合である。
async fn collect_section_concepts(
    client: &VegapunkClient,
    schema: &str,
    start_after: Option<&str>,
) -> Result<Collected> {
    let sections = client
        .query_nodes_paged(schema, KIND_SECTION, Vec::new(), PAGE_SIZE)
        .await
        .context("load all ManualSection nodes")?;
    let total_before_start_after = sections.len();
    tracing::info!(
        count = total_before_start_after,
        "loaded ManualSection nodes"
    );

    let mut excluded_missing_section_key = 0usize;
    let mut filtered: Vec<_> = sections
        .into_iter()
        .filter(|n| {
            let section_key = n.attributes.get("section_key").map(|k| k.as_str());
            let keep = after_start(section_key, start_after);
            // section_key 欠落による除外は「読みが壊れている signal」なので必ず残す。
            // --start-after 無しなら後段の missing_required_attrs が warn + skip するが、
            // フィルタ経路では無言に消えるため、ここで明示的に記録する。
            if !keep && section_key.is_none() {
                excluded_missing_section_key += 1;
                tracing::warn!(
                    node_id = %n.node_id,
                    "ManualSection has no section_key attribute; excluded by --start-after \
                     filter (cannot place it in lexicographic order)"
                );
            }
            keep
        })
        .collect();
    filtered.sort_by(|a, b| {
        a.attributes
            .get("section_key")
            .cloned()
            .unwrap_or_default()
            .cmp(&b.attributes.get("section_key").cloned().unwrap_or_default())
    });
    let total = filtered.len();

    let mut rows = Vec::with_capacity(total);
    let mut name_ja: HashMap<String, String> = HashMap::new();
    for (idx, section) in filtered.iter().enumerate() {
        let section_key = section
            .attributes
            .get("section_key")
            .cloned()
            .unwrap_or_default();
        let neighbors = client
            .traverse_neighbors_paged(
                schema,
                KIND_CONCEPT,
                "MENTIONS_CONCEPT",
                "outgoing",
                &section.node_id,
                PAGE_SIZE,
            )
            .await
            .with_context(|| format!("traverse MENTIONS_CONCEPT for section {section_key}"))?;
        let mut keys: Vec<String> = Vec::new();
        for n in &neighbors {
            let Some(key) = n.attributes.get("concept_key") else {
                tracing::warn!(
                    section_key = %section_key,
                    node_id = %n.node_id,
                    "MENTIONS_CONCEPT neighbor missing concept_key attribute; skipping this mention"
                );
                continue;
            };
            if let Some(ja) = n.attributes.get("name_ja") {
                if !ja.is_empty() {
                    name_ja.insert(key.clone(), ja.clone());
                }
            }
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        keys.sort();
        rows.push(SectionConcepts {
            section_key,
            node_id: section.node_id.clone(),
            concept_keys: keys,
            attr_concept_keys: section.attributes.get("concept_keys").cloned(),
            attrs: section.attributes.clone(),
        });
        if (idx + 1) % 100 == 0 {
            tracing::info!(processed = idx + 1, total, "collecting section concepts");
        }
    }
    tracing::info!(total, "collected section concepts");
    Ok(Collected {
        rows,
        name_ja,
        total_before_start_after,
        excluded_missing_section_key,
    })
}

/// `probe_summary` が既定値で埋めた `name_ja: ""` を、traverse で観測できた実値で上書きする。
/// probe_summary 自体を純関数のまま保つため、name_ja の補完はここ（呼び出し側）で行う。
fn apply_concept_names(mut summary: Value, name_ja: &HashMap<String, String>) -> Value {
    if let Some(concepts) = summary.get_mut("concepts").and_then(|v| v.as_array_mut()) {
        for concept in concepts.iter_mut() {
            let key = concept
                .get("concept_key")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(key) = key {
                if let Some(ja) = name_ja.get(&key) {
                    concept["name_ja"] = Value::String(ja.clone());
                }
            }
        }
    }
    summary
}

/// 書き込み前の必須属性チェック。欠けている `REQUIRED_ATTRS` を（宣言順で）列挙する。
/// 空なら安全に全属性再送できる。
fn missing_required_attrs(attrs: &HashMap<String, String>) -> Vec<&'static str> {
    REQUIRED_ATTRS
        .iter()
        .copied()
        .filter(|key| !attrs.contains_key(*key))
        .collect()
}

/// 既存の `concept_keys` 属性（JSON 配列文字列）と、計算済みの concept_key 集合を
/// **集合として**比較し、書き込みが要るかを判定する。
///
/// ingest は抽出順（重複排除のみ）、backfill はソート順で書くため、文字列比較のままだと
/// 順序差だけで永久に「乖離」と判定してしまう（`needs_update` / `divergences` 共通の理由）。
/// 属性が不正 JSON の場合は内容を判定できないため、常に書き直して修復する。
fn needs_update(attr_concept_keys: Option<&str>, computed_sorted: &[String]) -> bool {
    let Some(raw) = attr_concept_keys else {
        return !computed_sorted.is_empty();
    };
    let existing: Vec<String> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let existing_set: HashSet<&str> = existing.iter().map(|s| s.as_str()).collect();
    let computed_set: HashSet<&str> = computed_sorted.iter().map(|s| s.as_str()).collect();
    existing_set != computed_set
}

/// `MENTIONS_CONCEPT` 辺（`row.concept_keys`、正）と `concept_keys` 属性（射影）の乖離を
/// **集合比較**で検出する（`needs_update` と同じ理由: 出現順とソート順の差を乖離と誤判定しない
/// ため）。一致する行は結果に含めない。属性が不正 JSON の場合は判定不能だが「一致」とは
/// 絶対にみなさず、`attr: null` を付けて必ず乖離として報告する（fail closed）。
fn divergences(rows: &[SectionConcepts]) -> Vec<Value> {
    let mut out = Vec::new();
    for row in rows {
        let attr_keys = match &row.attr_concept_keys {
            None => Some(Vec::new()),
            Some(raw) => serde_json::from_str::<Vec<String>>(raw).ok(),
        };
        match attr_keys {
            Some(keys) => {
                let edge_set: HashSet<&str> = row.concept_keys.iter().map(|s| s.as_str()).collect();
                let attr_set: HashSet<&str> = keys.iter().map(|s| s.as_str()).collect();
                if edge_set != attr_set {
                    out.push(json!({
                        "section_key": row.section_key,
                        "edges": row.concept_keys,
                        "attr": keys,
                    }));
                }
            }
            None => {
                tracing::warn!(
                    section_key = %row.section_key,
                    "concept_keys attribute is not valid JSON; reporting as diverged \
                     (cannot assume it matches the edges)"
                );
                out.push(json!({
                    "section_key": row.section_key,
                    "edges": row.concept_keys,
                    "attr": Value::Null,
                    "note": "concept_keys attribute is not valid JSON",
                }));
            }
        }
    }
    out
}

/// 読み出した全属性 + 計算済み `concept_keys`（ソート済み JSON）で `GraphNode` を組み立てる。
/// `UpsertNodes` が部分マージか全置換か未確定なため、常に全属性を持たせて再送する
/// （`--probe-one` の実測で確定しても、復旧可能性のためこの方針自体は変えない）。
fn build_backfill_node(row: &SectionConcepts) -> GraphNode {
    let mut attributes: Vec<(String, String)> = row
        .attrs
        .iter()
        .filter(|(k, _)| k.as_str() != "concept_keys")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    attributes.push((
        "concept_keys".to_string(),
        serde_json::to_string(&row.concept_keys).unwrap_or_else(|_| "[]".to_string()),
    ));
    GraphNode {
        id: row.node_id.clone(),
        node_type: KIND_SECTION.to_string(),
        attributes,
    }
}

/// `UpsertNodes` が部分マージか全置換かを、`before`（probe 前の全属性）と `after`
/// （`concept_keys` だけを持つ最小ノードを upsert して読み戻した後の全属性）から判定する。
///
/// `before` に `REQUIRED_ATTRS` が 1 つも無ければ判定材料が無いので `"inconclusive"`。
/// それ以外は、`before` にあった required 属性が `after` にすべてそのまま残っていれば
/// `"merge"`、1 つでも消えていれば `"replace"`。
fn probe_one_verdict(
    before: &HashMap<String, String>,
    after: &HashMap<String, String>,
) -> &'static str {
    let before_required: Vec<&str> = REQUIRED_ATTRS
        .iter()
        .copied()
        .filter(|k| before.contains_key(*k))
        .collect();
    if before_required.is_empty() {
        return "inconclusive";
    }
    let retained = before_required
        .iter()
        .all(|k| after.get(*k) == before.get(*k));
    if retained {
        "merge"
    } else {
        "replace"
    }
}

/// 最小ノード upsert 後の読み戻しと `probe_one_verdict` による判定。
///
/// `run_probe_one` から**切り出してある理由**: この関数の失敗を `?` で呼び出し元へ素通し
/// させると、破壊的 upsert 済みの状態で復旧を飛ばして早期 return してしまう。呼び出し側は
/// 戻り値を `Result` のまま受け、復旧を実行してから改めて評価すること。
///
/// 読み戻しは `.first()` ではなく `node_id` 一致で選ぶ。backend の `eq` が将来 prefix 的に
/// 振る舞った場合、先頭要素だと**別ノードの属性で判定してしまう**（復旧自体は `row.node_id`
/// を狙うので安全だが、判定だけ静かに嘘になる）。
async fn read_back_verdict(
    client: &VegapunkClient,
    schema: &str,
    section_key: &str,
    row: &SectionConcepts,
) -> Result<&'static str> {
    let read_back = client
        .query_nodes(
            schema,
            KIND_SECTION,
            vec![("section_key", "eq", section_key)],
            5,
        )
        .await
        .context("probe-one: read back section after minimal upsert")?;
    let after = read_back
        .iter()
        .find(|n| n.node_id == row.node_id)
        .with_context(|| {
            format!(
                "probe-one: section {section_key} (node_id {}) not found after minimal upsert \
                 (query_nodes returned {} node(s), none matching); cannot determine UpsertNodes \
                 semantics",
                row.node_id,
                read_back.len()
            )
        })?
        .attributes
        .clone();
    Ok(probe_one_verdict(&row.attrs, &after))
}

/// `--probe-one <section_key>` 本体。
///
/// 1. 対象 section の現属性（`row.attrs`）を退避（既に `collect_section_concepts` が読み出し
///    済みのメモリ上の値。この関数自身が upsert する前の状態）
/// 2. `concept_keys` だけを持つ最小ノードを 1 件 upsert
/// 3. 読み戻して `probe_one_verdict` で判定
/// 4. 判定結果によらず、退避した全属性 + 計算済み `concept_keys` で即座に再送して復旧する
///    （`build_backfill_node` を再利用。全置換だった場合はこれが唯一の復旧手段）
async fn run_probe_one(
    client: &VegapunkClient,
    schema: &str,
    rows: &[SectionConcepts],
    section_key: &str,
) -> Result<Value> {
    let row = rows
        .iter()
        .find(|r| r.section_key == section_key)
        .with_context(|| {
            format!(
                "section_key {section_key} not found among loaded ManualSection nodes; \
                 refusing to probe a section that does not exist"
            )
        })?;

    let minimal_node = GraphNode {
        id: row.node_id.clone(),
        node_type: KIND_SECTION.to_string(),
        attributes: vec![(
            "concept_keys".to_string(),
            serde_json::to_string(&row.concept_keys).unwrap_or_else(|_| "[]".to_string()),
        )],
    };
    client
        .upsert_nodes(vec![minimal_node])
        .await
        .context("probe-one: upsert concept_keys-only minimal node")?;

    // ここから先は「破壊的 upsert 済み」の状態。判定が失敗しても復旧を飛ばしてはならないため、
    // 判定は `?` で伝播させず Result のまま受ける（spec L89:「どちらの結果でも…即座に再送して
    // 復旧する」）。`VegapunkClient::call` にリトライは無く per-request timeout は 120s なので、
    // 読み戻し 1 発の一過性失敗（接続リセット・backend の一時停滞）はここに到達しうる。
    let verdict_result = read_back_verdict(client, schema, section_key, row).await;

    // 判定の成否によらず、退避した全属性 + concept_keys で即座に復旧する。
    let restore_node = build_backfill_node(row);
    let restore_result = client
        .upsert_nodes(vec![restore_node])
        .await
        .context("probe-one: restore full attributes after minimal upsert probe");

    // 復旧が失敗したら、判定結果の有無に関わらず最優先で報告する。この時点で対象 section は
    // 「concept_keys だけの最小ノード」に化けている可能性があり（UpsertNodes が全置換だった
    // 場合）、放置すると body / order が失われたまま検索に残る。運用者が手で戻せるよう、
    // 退避してあった全属性を stderr に JSON で吐いてから bail する。
    let restored_count = match restore_result {
        Ok(count) => count,
        Err(err) => {
            let salvage: HashMap<&str, &str> = row
                .attrs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "probe_one_restore_failed": true,
                    "section_key": section_key,
                    "node_id": row.node_id,
                    "saved_attributes": salvage,
                    "computed_concept_keys": row.concept_keys,
                }))
                .unwrap_or_else(|e| format!("{{\"salvage_serialization_failed\":\"{e}\"}}"))
            );
            return Err(err).with_context(|| {
                format!(
                    "probe-one: FAILED TO RESTORE section {section_key}. The minimal \
                     concept_keys-only upsert already ran, so if UpsertNodes replaces attributes \
                     this section now has no body/title/order and retrieval will silently rank it \
                     at order=0. The full saved attributes were printed to stderr as JSON — \
                     restore them manually (or rerun ingest for this section) before using this \
                     schema for search"
                )
            });
        }
    };

    // 復旧は成功した。判定自体が失敗していた場合は、復旧済みである事実を添えて報告する
    // （データは安全なので、運用者は単に再実行すればよい）。
    let verdict = verdict_result.with_context(|| {
        format!(
            "probe-one: could not determine UpsertNodes semantics for section {section_key}, \
             but the full attributes were successfully restored ({restored_count} node(s) \
             upserted) — the graph is intact; rerun --probe-one to retry the measurement"
        )
    })?;

    tracing::info!(
        section_key,
        verdict,
        restored_count,
        "probe-one restored section to full attributes + concept_keys"
    );

    Ok(json!({
        "section_key": section_key,
        "verdict": verdict,
        "restored": true,
    }))
}

/// missing-attrs skip が閾値を超えたか判定する純関数（`site_skip_bail` と同じ流儀）。
///
/// 分母は **更新候補数**（`needs_update` が true の行）であって全 section 数ではない。
/// 全 section を分母にすると「候補が全体の 5% しかなく、その 5% が全部壊れている」ケースで
/// 比率が 5% となり、ガードをすり抜けて壊れたデータを書きに行く。候補を分母にする方が厳密に
/// 保守的である。
///
/// 返り値 `Some(ratio)` は「閾値超過 = bail すべき」の意。候補 0 件では除算せず常に `None`。
/// 境界はちょうど閾値なら通す（`>` 判定）。
fn exceeds_skip_threshold(skipped: usize, candidates: usize) -> Option<f64> {
    if candidates == 0 {
        return None;
    }
    let ratio = skipped as f64 / candidates as f64;
    (ratio > MAX_SKIP_RATIO).then_some(ratio)
}

/// `write_backfill` の summary に載せる、打ち切り関連の文脈。
///
/// 書き込み件数だけを出すと `--start-after` で縮んだ母集団に対する結果を全体の結果と
/// 誤読させる（spec L93）。呼び出し側が観測した実値をそのまま渡す。
struct WriteContext<'a> {
    truncated: bool,
    total_before_start_after: usize,
    excluded_missing_section_key: usize,
    start_after: Option<&'a str>,
}

/// 既定モード（全件書き込み）本体。
///
/// 対象件数は「未確定で増え続ける」ingest_alarmcom のクロールと違い、`rows` の時点で全件
/// 確定している。したがって missing-attrs skip 率の判定は **書き込みを始める前に** 行う
/// （閾値超過なら 1 件も書かずに bail する方が、書きかけの状態を残すより安全）。
async fn write_backfill(
    client: &VegapunkClient,
    rows: &[SectionConcepts],
    ctx: WriteContext<'_>,
) -> Result<Value> {
    let total_sections = rows.len();
    let candidates: Vec<&SectionConcepts> = rows
        .iter()
        .filter(|r| needs_update(r.attr_concept_keys.as_deref(), &r.concept_keys))
        .collect();
    let total_candidates = candidates.len();
    let skipped_up_to_date = total_sections - total_candidates;

    let mut writable: Vec<&SectionConcepts> = Vec::with_capacity(total_candidates);
    let mut skipped_missing_attrs = 0usize;
    let mut missing_examples: Vec<String> = Vec::new();
    for row in &candidates {
        let missing = missing_required_attrs(&row.attrs);
        if missing.is_empty() {
            writable.push(row);
            continue;
        }
        skipped_missing_attrs += 1;
        if missing_examples.len() < 3 {
            missing_examples.push(row.section_key.clone());
        }
        tracing::warn!(
            section_key = %row.section_key,
            missing = ?missing,
            "required attrs missing; skipping to avoid destructive resend"
        );
    }

    if let Some(skip_ratio) = exceeds_skip_threshold(skipped_missing_attrs, total_candidates) {
        anyhow::bail!(
            "{skipped_missing_attrs}/{total_candidates} update-candidate ManualSection(s) \
             ({:.1}%) are missing required attrs, exceeding the {:.0}% safety threshold; a \
             full-attribute resend on a section missing required attrs would drop body/TOC \
             order (retrieval.rs order parses via unwrap_or(0), so the failure is silent) — \
             aborting before writing anything this run (representative section_key(s): {:?})",
            skip_ratio * 100.0,
            MAX_SKIP_RATIO * 100.0,
            missing_examples
        );
    }

    let mut requested = 0usize;
    let mut upserted = 0i64;
    let mut batches = 0usize;
    // 直前に成功したバッチの末尾 section_key。中断時に `--start-after` へそのまま渡せる形で
    // エラーに載せる（CLAUDE.md「すべてのエラーパスに、運用者が次のアクションを判断できる
    // 情報を含める」。これが無いと再開点が運用者に届かず、全件やり直しになる）。
    let mut last_committed_key: Option<&str> = None;
    for chunk in writable.chunks(UPSERT_BATCH_SIZE) {
        let nodes: Vec<GraphNode> = chunk.iter().map(|row| build_backfill_node(row)).collect();
        let n = nodes.len();
        let count = client.upsert_nodes(nodes).await.with_context(|| {
            let resume = last_committed_key
                .map(|k| format!("--start-after {k}"))
                .unwrap_or_else(|| {
                    "no batch committed yet; rerun without --start-after".to_string()
                });
            format!(
                "upsert concept_keys batch #{batches} ({n} node(s)) failed after {upserted} \
                 node(s) already upserted across {batches} batch(es); resume with: {resume}"
            )
        })?;
        requested += n;
        upserted += i64::from(count);
        batches += 1;
        last_committed_key = chunk.last().map(|row| row.section_key.as_str());
        tracing::info!(
            requested,
            upserted,
            total_candidates,
            batches,
            "backfill write progress"
        );
    }

    // backend の申告値と要求件数が食い違ったら黙って成功と report しない。この summary は
    // spec L39 の B2-1 完了条件（全 ManualSection に concept_keys が入った）の唯一の evidence。
    if upserted != requested as i64 {
        tracing::warn!(
            requested,
            upserted,
            "backend reported a different upserted count than requested; the projection may be \
             incomplete — rerun --verify to confirm edge/attribute agreement before accepting B2-1"
        );
    }

    Ok(json!({
        "total_sections": total_sections,
        "requested": requested,
        "updated": upserted,
        "skipped_missing_attrs": skipped_missing_attrs,
        "skipped_up_to_date": skipped_up_to_date,
        "batches": batches,
        "truncated": ctx.truncated,
        "total_sections_before_start_after": ctx.total_before_start_after,
        "excluded_missing_section_key": ctx.excluded_missing_section_key,
        "start_after": ctx.start_after,
    }))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let token = read_token(&args)?;

    let schema_yaml = with_schema_name(
        &fs::read_to_string(&args.schema_file)
            .with_context(|| format!("read schema file {}", args.schema_file.display()))?,
        &args.schema,
    )?;

    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    // backfill が concept_keys 追加後の最初の実行になるため、ここで schema を確定させる
    // （spec: 属性追加は世代据え置き・既存ノード再投入不要。伝播は create_or_update_schema）。
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await
        .context("register/update schema (adds concept_keys attribute)")?;

    let collected =
        collect_section_concepts(&client, &args.schema, args.start_after.as_deref()).await?;
    let truncated = collected.truncated();
    let Collected {
        rows,
        name_ja,
        total_before_start_after,
        excluded_missing_section_key,
    } = collected;

    if args.probe_only {
        // clap の conflicts_with により --start-after とは併用できないため truncated は false に
        // なるはずだが、値をハードコードせず実測値を出す（将来 conflicts を緩めたときに、
        // 打ち切りが黙って `false` として出る事故を防ぐ）。
        let summary = apply_concept_names(probe_summary(&rows, truncated), &name_ja);
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    if args.verify {
        let diverged = divergences(&rows);
        let summary = json!({
            "total_sections": rows.len(),
            "diverged": diverged.len(),
            // 乖離 0 件が「全件検査した結果の 0」なのか「部分集合に対する 0」なのかを
            // 読み手が区別できるようにする（B2-1 の受理条件が「--verify の乖離 0 件」のため）。
            "truncated": truncated,
            "total_sections_before_start_after": total_before_start_after,
            "excluded_missing_section_key": excluded_missing_section_key,
            "items": diverged,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    if let Some(section_key) = &args.probe_one {
        let summary = run_probe_one(&client, &args.schema, &rows, section_key).await?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    let summary = write_backfill(
        &client,
        &rows,
        WriteContext {
            truncated,
            total_before_start_after,
            excluded_missing_section_key,
            start_after: args.start_after.as_deref(),
        },
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(section_key: &str, concept_keys: &[&str]) -> SectionConcepts {
        let mut keys: Vec<String> = concept_keys.iter().map(|s| s.to_string()).collect();
        keys.sort();
        SectionConcepts {
            section_key: section_key.to_string(),
            node_id: format!("node-{section_key}"),
            concept_keys: keys,
            attr_concept_keys: None,
            attrs: HashMap::new(),
        }
    }

    /// 8 required 属性が全部入った最小 HashMap（`missing_required_attrs` のテストで使う）。
    fn full_attrs() -> HashMap<String, String> {
        REQUIRED_ATTRS
            .iter()
            .map(|k| (k.to_string(), "x".to_string()))
            .collect()
    }

    #[test]
    fn missing_required_attrs_lists_each_absent_key() {
        let mut attrs = full_attrs();
        attrs.remove("body");
        attrs.remove("order");
        assert_eq!(missing_required_attrs(&attrs), vec!["body", "order"]);
        assert!(missing_required_attrs(&full_attrs()).is_empty());
    }

    #[test]
    fn needs_update_compares_as_set_not_string() {
        // ingest は出現順、backfill はソート順で書くため、順序差は「同じ」と扱う
        assert!(!needs_update(
            Some(r#"["b","a"]"#),
            &["a".into(), "b".into()]
        ));
        assert!(needs_update(Some(r#"["a"]"#), &["a".into(), "b".into()]));
        assert!(needs_update(None, &["a".into()]));
        assert!(!needs_update(None, &[])); // 両方空は書かない
        assert!(needs_update(Some("not json"), &[])); // 不正 JSON は書き直して修復する
    }

    #[test]
    fn build_backfill_node_replaces_concept_keys_and_keeps_other_attrs() {
        let mut attrs = full_attrs();
        attrs.insert("concept_keys".to_string(), r#"["stale"]"#.to_string());
        let mut r = row("s1", &["a", "b"]);
        r.attrs = attrs;
        let node = build_backfill_node(&r);
        assert_eq!(node.id, "node-s1");
        let concept_keys_attr = node
            .attributes
            .iter()
            .filter(|(k, _)| k == "concept_keys")
            .count();
        assert_eq!(concept_keys_attr, 1, "must not duplicate the attribute key");
        let value = node
            .attributes
            .iter()
            .find(|(k, _)| k == "concept_keys")
            .map(|(_, v)| v.clone())
            .unwrap();
        let parsed: Vec<String> = serde_json::from_str(&value).unwrap();
        assert_eq!(parsed, vec!["a".to_string(), "b".to_string()]);
        // 他の必須属性はそのまま残る
        assert!(node.attributes.iter().any(|(k, v)| k == "body" && v == "x"));
    }

    fn row_with_attr(
        section_key: &str,
        concept_keys: &[&str],
        attr_concept_keys: Option<&str>,
    ) -> SectionConcepts {
        let mut r = row(section_key, concept_keys);
        r.attr_concept_keys = attr_concept_keys.map(|s| s.to_string());
        r
    }

    #[test]
    fn divergences_reports_set_difference_only() {
        let rows = vec![
            row_with_attr("s1", &["a", "b"], Some(r#"["b","a"]"#)), // 順序差 = 一致
            row_with_attr("s2", &["a"], Some(r#"["a","zzz"]"#)),    // 乖離
            row_with_attr("s3", &[], None),                         // 両方空 = 一致
        ];
        let d = divergences(&rows);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["section_key"], "s2");
    }

    #[test]
    fn divergences_flags_invalid_json_attr_as_diverged() {
        let rows = vec![row_with_attr("s1", &["a"], Some("not json"))];
        let d = divergences(&rows);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["section_key"], "s1");
        assert!(d[0]["attr"].is_null());
    }

    #[test]
    fn after_start_filters_lexicographically_and_drops_keyless_nodes_only_when_resuming() {
        // フィルタ無効なら、section_key の有無に関わらず全件通す。
        assert!(after_start(Some("a"), None));
        assert!(after_start(None, None));
        // 辞書順で「後」だけ通す。ちょうど一致は通さない（再開点自身は処理済みのため）。
        assert!(after_start(Some("b"), Some("a")));
        assert!(!after_start(Some("a"), Some("a")));
        assert!(!after_start(Some("a"), Some("b")));
        // section_key を持たないノードは辞書順の位置を決められないので除外する
        // （呼び出し側が warn することで無言脱落にならないようにしてある）。
        assert!(!after_start(None, Some("a")));
    }

    #[test]
    fn exceeds_skip_threshold_guards_boundary_and_zero_candidates() {
        // 候補 0 件では除算せず、常に None（bail しない）。
        assert_eq!(exceeds_skip_threshold(0, 0), None);
        assert_eq!(exceeds_skip_threshold(5, 0), None);
        // ちょうど 10% は超過ではないので通す（`>` 判定）。
        assert_eq!(exceeds_skip_threshold(10, 100), None);
        // 10% を超えたら Some（bail）。
        assert!(exceeds_skip_threshold(11, 100).is_some());
        // 分母は「更新候補数」。全 section 数を分母にすると緩くなることを固定する:
        // 候補 20 件中 10 件欠落（50%）は必ず bail する。
        assert!(exceeds_skip_threshold(10, 20).is_some());
    }

    #[test]
    fn needs_update_treats_duplicate_entries_as_the_same_set() {
        // 集合比較なので重複は無視される（書き込みを無限に繰り返さない）。
        assert!(!needs_update(Some(r#"["a","a"]"#), &["a".into()]));
        assert!(!needs_update(
            Some(r#"["a","b","a"]"#),
            &["a".into(), "b".into()]
        ));
    }

    #[test]
    fn build_backfill_node_round_trips_every_attribute() {
        // 必須 8 個だけでなく、doc_key / body_original など「読み出した全属性」が
        // 1 つ残らず再送されることを固定する（全置換だった場合に失われないため）。
        let mut attrs = full_attrs();
        attrs.insert("doc_key".to_string(), "doc-alarmcom".to_string());
        attrs.insert("body_original".to_string(), "English body".to_string());
        attrs.insert("original_hash".to_string(), "deadbeef".to_string());
        attrs.insert("section_no".to_string(), String::new());
        let mut r = row("s1", &["a"]);
        r.attrs = attrs.clone();
        let node = build_backfill_node(&r);
        for (key, value) in &attrs {
            if key == "concept_keys" {
                continue;
            }
            assert!(
                node.attributes.iter().any(|(k, v)| k == key && v == value),
                "attribute {key} must survive the resend"
            );
        }
        assert_eq!(node.node_type, "ManualSection");
    }

    #[test]
    fn probe_one_verdict_detects_merge_and_replace() {
        let before = full_attrs();
        let mut after_merge = full_attrs();
        after_merge.insert("concept_keys".into(), "[\"a\"]".into());
        assert_eq!(probe_one_verdict(&before, &after_merge), "merge");
        let mut after_replace = HashMap::new();
        after_replace.insert("concept_keys".into(), "[\"a\"]".into());
        assert_eq!(probe_one_verdict(&before, &after_replace), "replace");
        assert_eq!(
            probe_one_verdict(&HashMap::new(), &HashMap::new()),
            "inconclusive"
        );
    }

    #[test]
    fn probe_summary_reports_fan_in_with_declared_denominator() {
        let rows = vec![
            row("s1", &["a", "b"]),
            row("s2", &["a"]),
            row("s3", &[]), // concept 無し section
        ];
        let v = probe_summary(&rows, false);
        assert_eq!(v["total_sections"], 3);
        assert_eq!(v["sections_with_concepts"], 2);
        assert_eq!(v["total_concepts"], 2);
        assert_eq!(v["ratio_denominator"], "sections_with_concepts");
        assert_eq!(v["truncated"], false);
        // concepts は section_count 降順 → concept_key 昇順。ratio の分母は sections_with_concepts
        assert_eq!(v["concepts"][0]["concept_key"], "a");
        assert_eq!(v["concepts"][0]["section_count"], 2);
        assert!((v["concepts"][0]["ratio"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(v["concepts"][1]["concept_key"], "b");
        assert!((v["concepts"][1]["ratio"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn probe_summary_handles_zero_sections_without_division() {
        let v = probe_summary(&[], false);
        assert_eq!(v["total_sections"], 0);
        assert_eq!(v["sections_with_concepts"], 0);
        assert_eq!(v["concepts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn apply_concept_names_fills_only_known_keys() {
        let summary = probe_summary(&[row("s1", &["a", "b"])], false);
        let mut names = HashMap::new();
        names.insert("a".to_string(), "エー".to_string());
        let patched = apply_concept_names(summary, &names);
        let concepts = patched["concepts"].as_array().unwrap();
        let a = concepts.iter().find(|c| c["concept_key"] == "a").unwrap();
        let b = concepts.iter().find(|c| c["concept_key"] == "b").unwrap();
        assert_eq!(a["name_ja"], "エー");
        assert_eq!(b["name_ja"], "");
    }
}
