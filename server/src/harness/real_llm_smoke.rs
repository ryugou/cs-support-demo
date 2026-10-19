//! design doc `2026-10-07-partial-answer-with-handoff-design.md` §10.3・§10.5「検証」。
//!
//! 単体テストと reviewer の判定だけでは、顧客に返る最終文面を確認したことにならない
//! （2026-10-09 の本番で下書きが全件捨てられていた実例、2026-10-13 の本番で検索依存の
//! 取りこぼしがあった実例を参照）。ここは本番と同じ部品（`data/urtect/signal-lexicon.json` /
//! `data/urtect/rules.json` / `config.cloudrun.toml` の `[llm]` / `[harness]`、実 Anthropic API）
//! と、本番 MCP `search_manual` の実結果を使い、取次ターンでも答えられる部分（料金）が実際に
//! 顧客向け文面に出ることを確認する。材料は手で選ばず、§10.2 の併合をコード側の同じ関数
//! （`super::supplementary_search::fold_signal_search_results`）で行う。
//!
//! 言い回しによって判定が `Escalate`（取次）と `Allowed`（回答のみ）に割れる（§10.5）。各ケースは
//! `expect` で自分がどちらになるはずかを宣言し、本テストは本番 `evaluate()` と同じ条件
//! （`handoff_items::is_partial_answer_candidate`）でだけ補助検索を併合し、`expect` ごとに
//! 異なる最終応答文の形（取次の一文・受付番号あり、または下書きそのまま）を確認する。
//!
//! `#[ignore]`: 実 API キー・ネットワーク到達性・`CS_VERIFY_CASES_JSON`（本番 MCP
//! `search_manual` の実結果ファイル）を要求するため CI では走らせない。
//! ```sh
//! cargo test --manifest-path server/Cargo.toml --lib real_llm_partial_answer_smoke -- \
//!   --ignored --nocapture
//! ```
//! API キーは env からのみ読み（`llm::AnthropicClient::from_config`）、ファイル・ログには書かない。

use super::decision::{self, AnswerDecision, DecisionInput, Thresholds};
use super::extraction::{
    self, AnthropicProductReferenceClassifier, AnthropicSignalClassifier, ClassifyLlm,
    ClassifyProductReferences, HybridExtractor, ProductReferenceExtractor,
};
use super::product_gate::ProductAllowlist;
use super::reply::{ReplyHistoryRole, ReplyHistoryTurn};
use super::rules::{match_layer1, matching_layer1_rules};
use super::{audit, authn, egress, escalation_reply, handoff_items, hours, rules, signal};
use super::{Harness, ReplyDraftPolicy};
use crate::config::AppConfig;
use crate::llm::AnthropicClient;
use crate::model::SectionHit;
use crate::test_support::{load_bundled_escalation_rules, load_bundled_prohibited_domains};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const LLM_API_KEY_ENV: &str = "CS_SUPPORT_LLM_API_KEY";
const CASES_ENV: &str = "CS_VERIFY_CASES_JSON";
const MIN_VERIFY_CASES: usize = 3;
const MIN_HANDOFF_CASES: usize = 2;
const SUPPLEMENTARY_SEARCH_SIGNAL: &str = "initial_cost_question";
/// design doc §10.5: 本番で取次になるべきだった回帰の質問文。handoff ケースに必ず含める。
const PRODUCTION_REGRESSION_QUESTION: &str = "導入にいくらかかる？";
/// 料金ページに載る月額料金の表記。最終応答文に出ることを確認する。
const MONTHLY_FEE_TEXT: &str = "1,540";
/// 取次ターンの確定文面（`escalation_reply::build_deterministic_block`）に付く受付番号行の接頭辞。
const CASE_REFERENCE_PREFIX: &str = "受付番号: ";

/// `server/` 相対パスを `CARGO_MANIFEST_DIR` 基準で解決する。cwd 非依存にするため
/// （`production_ng_dictionary` / `test_support::load_bundled_escalation_rules` と同じ作法）。
fn server_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// config の相対パスを `Harness::build` の `resolve_path` と同じく解決する（絶対パスはそのまま）。
fn resolve_server_path(configured: &str) -> PathBuf {
    let path = Path::new(configured);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        server_path(configured)
    }
}

#[derive(Debug, serde::Deserialize)]
struct SeedProduct {
    model: String,
}

/// 本番の取扱製品は vegapunk の Product ノード。その seed `products.json` の全型番で
/// allowlist を作る（下書きの system prompt に注入されるため、1 型番だとプロンプトが変わる）。
fn seed_product_models() -> Vec<String> {
    let path = server_path("data/urtect/products.json");
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let products: Vec<SeedProduct> = serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("parse {} as a product array: {err}", path.display()));
    products.into_iter().map(|p| p.model).collect()
}

/// MCP `search_manual` の返却 JSON（`SearchManualResponse`、`server/src/rmcp_server.rs`
/// 参照）をそのまま読むための、このファイル専用の読み取り専用デシリアライズ型。
#[derive(Debug, serde::Deserialize)]
struct SearchManualHits {
    hits: Vec<SectionHit>,
}

/// `CS_VERIFY_CASES_JSON` の各 `hits_json` が指すファイルの形
/// （design doc `2026-10-07-partial-answer-with-handoff-design.md` §10.3）。
#[derive(Debug, serde::Deserialize)]
struct CaseHitsFile {
    question_hits: SearchManualHits,
    signal_hits: SearchManualHits,
}

/// design doc §10.5 の `expect`: このケースの判定・最終応答文がどちらの形になるはずか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseExpectation {
    /// 判定が第1層 `Escalate`。部分回答の下書きが採用され、最終応答文に取次の一文・受付番号が
    /// 付く。
    Handoff,
    /// 判定が `Allowed`。`customer_reply_draft` が採用され、最終応答文は下書きそのもの。
    Allowed,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawExpectation {
    Handoff,
    Allowed,
    EscalatedFollowup,
}

impl RawExpectation {
    fn tag(self) -> &'static str {
        match self {
            Self::Handoff => "handoff",
            Self::Allowed => "allowed",
            Self::EscalatedFollowup => "escalated_followup",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVerifyCase {
    question: String,
    hits_json: String,
    expect: RawExpectation,
    #[serde(default)]
    followup_question: Option<String>,
    #[serde(default)]
    followup_hits_json: Option<String>,
}

#[derive(Debug)]
struct FollowupTurn {
    question: String,
    hits_json: String,
}

#[derive(Debug)]
enum Expectation {
    Single(CaseExpectation),
    EscalatedFollowup(FollowupTurn),
}

#[derive(Debug, serde::Deserialize)]
#[serde(try_from = "RawVerifyCase")]
struct VerifyCase {
    question: String,
    hits_json: String,
    expectation: Expectation,
}

fn non_empty_field(field: &str, value: String, question: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err(format!(
            "invalid field \"{field}\": empty (case {question:?})"
        ));
    }
    Ok(value)
}

fn required_followup_field(
    field: &str,
    value: Option<String>,
    question: &str,
) -> Result<String, String> {
    let value = value.ok_or_else(|| {
        format!(
            "invalid field \"{field}\": required for expect \"escalated_followup\" (case {question:?})"
        )
    })?;
    non_empty_field(field, value, question)
}

fn reject_followup_fields(raw: &RawVerifyCase) -> Result<(), String> {
    for (field, value) in [
        ("followup_question", &raw.followup_question),
        ("followup_hits_json", &raw.followup_hits_json),
    ] {
        if value.is_some() {
            return Err(format!(
                "invalid field \"{field}\": not allowed for expect \"{}\" (case {:?})",
                raw.expect.tag(),
                raw.question
            ));
        }
    }
    Ok(())
}

impl TryFrom<RawVerifyCase> for VerifyCase {
    type Error = String;

    fn try_from(raw: RawVerifyCase) -> Result<Self, String> {
        let question = non_empty_field("question", raw.question.clone(), &raw.question)?;
        let hits_json = non_empty_field("hits_json", raw.hits_json.clone(), &raw.question)?;
        let expectation = match raw.expect {
            RawExpectation::EscalatedFollowup => {
                let question = required_followup_field(
                    "followup_question",
                    raw.followup_question,
                    &raw.question,
                )?;
                let hits_json = required_followup_field(
                    "followup_hits_json",
                    raw.followup_hits_json,
                    &raw.question,
                )?;
                Expectation::EscalatedFollowup(FollowupTurn {
                    question,
                    hits_json,
                })
            }
            RawExpectation::Handoff => {
                reject_followup_fields(&raw)?;
                Expectation::Single(CaseExpectation::Handoff)
            }
            RawExpectation::Allowed => {
                reject_followup_fields(&raw)?;
                Expectation::Single(CaseExpectation::Allowed)
            }
        };
        Ok(Self {
            question,
            hits_json,
            expectation,
        })
    }
}

enum LoadedExpectation {
    Single(CaseExpectation),
    EscalatedFollowup {
        question: String,
        hits: Vec<SectionHit>,
    },
}

struct LoadedVerifyCase {
    question: String,
    question_hits: Vec<SectionHit>,
    signal_hits: Vec<SectionHit>,
    expectation: LoadedExpectation,
}

/// design doc §10.3・§10.5 の「言い回しの検証」: 相異なる言い回し数、handoff 側の相異なる数、
/// production 回帰質問を含むことを確認する。
fn assert_case_paraphrases(cases: &[VerifyCase]) {
    // design doc §10.3: 同じ通りやすい言い回しを複製しても合格しないよう、言い回しは相異なる
    // ことを要求する（codex レビュー RH-8）。
    let distinct_questions =
        |keep: &dyn Fn(&Expectation) -> bool| -> std::collections::BTreeSet<&str> {
            cases
                .iter()
                .filter(|case| keep(&case.expectation))
                .map(|case| case.question.trim())
                .collect()
        };
    let distinct = distinct_questions(&|_| true);
    assert!(
        distinct.len() >= MIN_VERIFY_CASES,
        "{CASES_ENV} must list at least {MIN_VERIFY_CASES} distinct paraphrases (design doc \
         §10.3), got {} distinct of {}",
        distinct.len(),
        cases.len()
    );
    // design doc §10.5: 言い回しによって判定が `Allowed` に割れることがあるため、取次
    // （`"handoff"`）の退行を検出できるよう、相異なる言い回しの handoff ケースを要求し、
    // 本番の回帰の質問文をその1つに含めることを強制する。
    let handoff =
        distinct_questions(&|e| matches!(e, Expectation::Single(CaseExpectation::Handoff)));
    assert!(
        handoff.len() >= MIN_HANDOFF_CASES,
        "{CASES_ENV} must list at least {MIN_HANDOFF_CASES} distinct \"handoff\" paraphrases \
         (design doc §10.5), got {} distinct",
        handoff.len()
    );
    assert!(
        handoff.contains(PRODUCTION_REGRESSION_QUESTION),
        "{CASES_ENV} must include the production regression \"{PRODUCTION_REGRESSION_QUESTION}\" \
         as a \"handoff\" case (design doc §10.5), got handoff questions {handoff:?}"
    );
    // #73 の料金問い合わせ（回答のみ）の経路も必ず 1 件は通す（Copilot レビュー）。
    let allowed =
        distinct_questions(&|e| matches!(e, Expectation::Single(CaseExpectation::Allowed)));
    assert!(
        !allowed.is_empty(),
        "{CASES_ENV} must include at least one distinct \"allowed\" paraphrase (the #73 \
         answer-only price inquiry path, design doc §10.5), got none"
    );
    let followup = distinct_questions(&|e| matches!(e, Expectation::EscalatedFollowup(_)));
    assert!(
        !followup.is_empty(),
        "{CASES_ENV} must include at least one distinct \"escalated_followup\" case (the #101 \
         answer-after-escalation path, design doc 2026-10-14-answer-after-escalation-design.md \
         §6), got none"
    );
}

/// `CS_VERIFY_CASES_JSON`（`[{"question": ..., "hits_json": <path>, "expect": ...}, ...]`）を読み、
/// 各ケースの `hits_json` が指す本番 MCP `search_manual` の実結果（`{"question_hits": ...,
/// "signal_hits": ...}`）を読み込む（design doc §10.3・§10.5）。
fn load_verify_cases() -> Vec<LoadedVerifyCase> {
    let cases_path = std::env::var(CASES_ENV).unwrap_or_else(|_| {
        panic!(
            "{CASES_ENV} is not set; point it at a json file of `[{{\"question\": ..., \
             \"hits_json\": <path>, \"expect\": \"handoff\" | \"allowed\"}}, ...]` (at least 3 \
             paraphrases of the pricing question; each hits_json file holds the real \
             `search_manual` output for the question text and for the signal-guided \
             supplementary search; see design doc 2026-10-07-partial-answer-with-handoff-design.md \
             §10.3, §10.5)"
        )
    });
    let body = std::fs::read_to_string(&cases_path).unwrap_or_else(|err| {
        panic!("{CASES_ENV} must point at a readable file ({cases_path:?}): {err}")
    });
    let cases: Vec<VerifyCase> = serde_json::from_str(&body).unwrap_or_else(|err| {
        panic!(
            "{CASES_ENV} ({cases_path:?}) is not a json array of {{question, hits_json, \
             expect}}: {err}"
        )
    });
    assert!(
        cases.len() >= MIN_VERIFY_CASES,
        "{CASES_ENV} ({cases_path:?}) must list at least {MIN_VERIFY_CASES} cases (paraphrases of \
         the pricing question, design doc §10.3), got {}",
        cases.len()
    );
    assert_case_paraphrases(&cases);
    cases.into_iter().map(load_verify_case).collect()
}

fn read_case_hits_file(question: &str, field: &str, path: &str) -> CaseHitsFile {
    let body = std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!("case {question:?}: {field} {path:?} must be a readable file: {err}")
    });
    serde_json::from_str(&body).unwrap_or_else(|err| {
        panic!(
            "case {question:?}: {field} {path:?} is not {{question_hits, signal_hits}} \
             search_manual output: {err}"
        )
    })
}

fn load_verify_case(case: VerifyCase) -> LoadedVerifyCase {
    let hits_file = read_case_hits_file(&case.question, "hits_json", &case.hits_json);
    let expectation = match case.expectation {
        Expectation::Single(expect) => LoadedExpectation::Single(expect),
        Expectation::EscalatedFollowup(turn) => LoadedExpectation::EscalatedFollowup {
            hits: read_case_hits_file(&case.question, "followup_hits_json", &turn.hits_json)
                .question_hits
                .hits,
            question: turn.question,
        },
    };
    LoadedVerifyCase {
        question: case.question,
        question_hits: hits_file.question_hits.hits,
        signal_hits: hits_file.signal_hits.hits,
        expectation,
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.0) {
            eprintln!(
                "failed to remove temp dir {} of the harness worm audit log/queue: {err}",
                self.0.display()
            );
        }
    }
}

struct Fixture {
    config: AppConfig,
    harness: Harness,
    lexicon: Arc<signal::LexiconNormalizer>,
    escalation_rules: Vec<rules::EscalationRule>,
    prohibited_domains: Vec<rules::ProhibitedDomain>,
    thresholds: Thresholds,
    allowlist: ProductAllowlist,
    hours_label: String,
}

fn load_config_and_client() -> (AppConfig, AnthropicClient) {
    let config = AppConfig::load(&server_path("config.cloudrun.toml"))
        .expect("load server/config.cloudrun.toml");
    // 下書きが無効なら本番は下書きを作らず、§10.3 の合否は成立しない（`Harness::build` と同じ前提）。
    assert!(
        config.harness.customer_reply_draft_enabled,
        "config.cloudrun.toml must have [harness] customer_reply_draft_enabled = true; \
         production builds no draft otherwise, so this smoke test cannot verify §10.3"
    );
    assert!(
        config.harness.customer_reply_draft_max_tokens > 0,
        "config.cloudrun.toml [harness] customer_reply_draft_max_tokens must be > 0 \
         (Harness::build rejects 0)"
    );

    // config.cloudrun.toml は [llm] enabled = true なので、鍵が解決できなければここで
    // Err になる。メッセージは env 名を含む（llm.rs::resolve_api_key 経由）。
    let client = AnthropicClient::from_config(&config.llm)
        .unwrap_or_else(|err| {
            panic!(
                "failed to configure the real Anthropic client for \
                 real_llm_partial_answer_smoke: {err:#}; set {LLM_API_KEY_ENV} to a valid \
                 Anthropic API key before running this test with --ignored"
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "config.cloudrun.toml must have [llm] enabled = true for \
                 real_llm_partial_answer_smoke to exercise the real LLM"
            )
        });
    (config, client)
}

fn build_harness(
    config: &AppConfig,
    client: AnthropicClient,
    lexicon: &Arc<signal::LexiconNormalizer>,
    dir: &Path,
) -> Harness {
    // signal 抽出・製品言及抽出の配線は `Harness::build` と同一（別々の LLM 呼び出しに
    // 分離済みの 2 コンポーネントを、同じ AnthropicClient を使い回して組む）。
    let signal_classifier: Arc<dyn ClassifyLlm> = Arc::new(AnthropicSignalClassifier::new(
        client.clone(),
        lexicon.vocabulary_for_prompt(),
    ));
    let product_reference_classifier: Arc<dyn ClassifyProductReferences> =
        Arc::new(AnthropicProductReferenceClassifier::new(client.clone()));

    Harness {
        authenticator: authn::Authenticator::new(
            config.projects.iter().map(|p| p.schema.clone()).collect(),
        ),
        normalizer: lexicon.clone(),
        lexicon: lexicon.clone(),
        extractor: Arc::new(HybridExtractor::new(
            lexicon.clone(),
            Some(signal_classifier),
        )),
        product_reference_extractor: ProductReferenceExtractor::new(Some(
            product_reference_classifier,
        )),
        ng: egress::NgDictionary::from_path(&resolve_server_path(
            &config.harness.ng_dictionary_path,
        ))
        .expect("production ng dictionary must load and parse"),
        worm: Arc::new(
            audit::WormAuditLog::open(&dir.join("audit.jsonl")).expect("open temp worm audit log"),
        ),
        knowledge: None,
        thresholds: (&config.harness.thresholds).into(),
        grading: (&config.harness.grading).into(),
        queue_path: dir.join("queue.jsonl"),
        grade_lock: tokio::sync::Mutex::new(()),
        manual: None,
        corpus: None,
        default_route: config.harness.default_escalation_route.clone(),
        vector_route_enabled: config.harness.vector_route_enabled,
        reply_drafter: Some(client),
        reply_draft_max_tokens: config.harness.customer_reply_draft_max_tokens,
        product_gate: None,
        jev_client: None,
        kr_applicability: None,
        excluded_known_resolution_ids: Vec::new(),
    }
}

fn build_fixture(
    config: AppConfig,
    harness: Harness,
    lexicon: Arc<signal::LexiconNormalizer>,
) -> Fixture {
    let prohibited_domains = load_bundled_prohibited_domains();
    let thresholds: Thresholds = (&config.harness.thresholds).into();
    let hours_label = hours::business_hours_label(&config.api.business_hours);
    Fixture {
        harness,
        lexicon,
        escalation_rules: load_bundled_escalation_rules(),
        prohibited_domains,
        thresholds,
        allowlist: ProductAllowlist::from_models(seed_product_models()),
        hours_label,
        config,
    }
}

/// signal 抽出から `decide` までを実行する。判定内容そのものへの assert はここでは行わず、
/// 呼び出し側（`run_once`）が `expect`（design doc §10.5）に応じて [`assert_handoff_decision`] /
/// [`assert_allowed_decision`] のどちらを課すか選ぶ。`question_hits` は本番 MCP `search_manual`
/// の実結果（質問文検索。補助検索より前に判定へ渡るので merge 前のまま使う。`evaluate()` と同じ
/// 順序）。
async fn decide_for_case(
    fx: &Fixture,
    question: &str,
    question_hits: &[SectionHit],
    case_index: usize,
    total_cases: usize,
) -> (extraction::ExtractionResult, AnswerDecision, Vec<String>) {
    let extracted = fx.harness.extractor.extract(question).await;
    let (signals, mode) = (&extracted.signals, extracted.mode);
    // 製品言及抽出は signal 抽出と完全に独立したコンポーネント（Issue #52）。この質問は
    // 型番言及を含まないため判定には使わないが、本番と同じ配線を実際に動かして確認する
    // （design doc §10.3「実 LLM による...製品言及抽出」）。
    let product_references = fx
        .harness
        .product_reference_extractor
        .extract(question, fx.allowlist.display_list())
        .await;
    println!("signals: {signals:?} (extraction_mode={mode:?})");
    println!("product_references: {product_references:?}");

    let (decision, matched_layer1_rule_ids) = decide_on_signals(
        fx,
        question,
        question_hits,
        signals,
        signals,
        case_index,
        total_cases,
    );
    println!("decision: {decision:?}");
    println!("matched layer-1 rule ids: {matched_layer1_rule_ids:?}");
    (extracted, decision, matched_layer1_rule_ids)
}

fn decide_on_signals(
    fx: &Fixture,
    question: &str,
    question_hits: &[SectionHit],
    question_signals: &signal::SignalSet,
    accumulated: &signal::SignalSet,
    case_index: usize,
    total_cases: usize,
) -> (AnswerDecision, Vec<String>) {
    let stakes_input =
        fx.harness
            .stakes_input_for_turn(&fx.prohibited_domains, accumulated, question);
    let top_hit = question_hits.first().unwrap_or_else(|| {
        panic!(
            "case {case_index}/{total_cases} ({question:?}): question_hits is empty (the \
             search_manual result for the question text has no hit, or every hit was excluded as \
             out-of-scope)"
        )
    });
    let section_keys: Vec<String> = question_hits
        .iter()
        .map(|h| h.section_key.clone())
        .collect();
    let decision = decision::decide(&DecisionInput {
        question_signals,
        question_raw: question,
        rules: &fx.escalation_rules,
        domains: &fx.prohibited_domains,
        resolutions: &[],
        best_manual_score: Some(top_hit.score),
        best_manual_sections: &section_keys,
        stakes_input,
        thresholds: &fx.thresholds,
        default_route: &fx.config.harness.default_escalation_route,
        known_resolution: None,
    });
    let matched_layer1_rule_ids: Vec<String> =
        matching_layer1_rules(&fx.escalation_rules, question_signals)
            .into_iter()
            .map(|rule| rule.id.clone())
            .collect();
    (decision, matched_layer1_rule_ids)
}

/// `decide_for_case` の判定が design doc §10.5 の `"handoff"` ケースの期待（第1層 `Escalate`
/// かつ選択ルールが `initial-cost-quote`）を満たすことを確かめる。併発ルールがあっても `decide`
/// が選ぶルールが `initial-cost-quote` なら §10.5 は満たされる。
fn assert_handoff_decision(
    decision: &AnswerDecision,
    escalation_rules: &[rules::EscalationRule],
    signals: &signal::SignalSet,
    matched_layer1_rule_ids: &[String],
    question: &str,
    case_index: usize,
    total_cases: usize,
) {
    assert!(
        matches!(decision, AnswerDecision::Escalate { layer: 1, .. }),
        "case {case_index}/{total_cases} ({question:?}): expected a layer-1 escalate decision for \
         a \"handoff\" case (design doc §10.5), got {decision:?}"
    );
    let selected_rule_id = match_layer1(escalation_rules, signals).map(|r| r.id.as_str());
    assert_eq!(
        selected_rule_id,
        Some("initial-cost-quote"),
        "case {case_index}/{total_cases} ({question:?}): expected initial-cost-quote to be the \
         selected layer-1 rule for a \"handoff\" case (design doc §10.5; matched: \
         {matched_layer1_rule_ids:?})"
    );
}

/// `decide_for_case` の判定が design doc §10.5 の `"allowed"` ケースの期待（`Allowed`）を
/// 満たすことを確かめる。
fn assert_allowed_decision(
    decision: &AnswerDecision,
    question: &str,
    case_index: usize,
    total_cases: usize,
) {
    assert!(
        matches!(decision, AnswerDecision::Allowed { .. }),
        "case {case_index}/{total_cases} ({question:?}): expected an allowed decision for an \
         \"allowed\" case (design doc §10.5), got {decision:?}"
    );
}

/// `expect`（design doc §10.5）に応じて [`assert_handoff_decision`] / [`assert_allowed_decision`]
/// のどちらを課すか選ぶ。
#[allow(clippy::too_many_arguments)]
fn assert_decision_matches(
    decision: &AnswerDecision,
    expect: CaseExpectation,
    escalation_rules: &[rules::EscalationRule],
    signals: &signal::SignalSet,
    matched_layer1_rule_ids: &[String],
    question: &str,
    case_index: usize,
    total_cases: usize,
) {
    match expect {
        CaseExpectation::Handoff => assert_handoff_decision(
            decision,
            escalation_rules,
            signals,
            matched_layer1_rule_ids,
            question,
            case_index,
            total_cases,
        ),
        CaseExpectation::Allowed => {
            assert_allowed_decision(decision, question, case_index, total_cases)
        }
    }
}

/// 下書きを生成し、本番と同じ採否判定（`accept_draft`、truncated 判定、取扱外型番ゲート）を
/// 通して本文を返す。`merged_hits` / `merged_best_score` は `run_once` が design doc §10.2 の
/// 条件で確定した下書き材料（取次候補のときは質問文検索と補助検索を
/// `super::supplementary_search::fold_signal_search_results` で併合した後の値、それ以外は質問文
/// 検索の結果そのまま）。この関数自体は `decision` / `items` の種類（`Escalate` / `Allowed`）に
/// 依らない汎用の採否判定（`plan_reply_draft` が `is_allowed` を見て方針を決める）。
#[allow(clippy::too_many_arguments)]
async fn draft_accepted_text(
    fx: &Fixture,
    decision: &AnswerDecision,
    mode: extraction::ExtractionMode,
    items: &[handoff_items::HandoffItem],
    question: &str,
    merged_hits: &[SectionHit],
    merged_best_score: Option<f32>,
    history: &[ReplyHistoryTurn],
    case_index: usize,
    total_cases: usize,
) -> String {
    let labels: Vec<String> = items.iter().map(|item| item.label.clone()).collect();
    let policy = ReplyDraftPolicy::SkipWhenUnused {
        response_allowlist: &fx.allowlist,
        already_escalated: !history.is_empty(),
        partial_answer_enabled: true,
    };
    let plan = super::plan_reply_draft(
        &policy,
        decision,
        items,
        mode,
        false,
        merged_best_score,
        &fx.thresholds,
        fx.config.harness.customer_reply_draft_enabled,
    );

    let draft = if plan.generate {
        fx.harness
            .draft_customer_reply(
                question,
                "smoke-request-id",
                decision,
                merged_hits,
                &[],
                history,
                !history.is_empty(),
                &fx.allowlist,
                &labels,
                plan.partial_answer_ok,
            )
            .await
    } else {
        None
    };

    let Some(draft) = draft else {
        panic!(
            "case {case_index}/{total_cases} ({question:?}): draft_customer_reply did not \
             generate a draft (plan.generate={}, plan.partial_answer_ok={})",
            plan.generate, plan.partial_answer_ok
        )
    };
    if let Err(reason) =
        handoff_items::accept_draft(&draft.text, plan.partial_answer_ok, items, &fx.lexicon)
    {
        panic!("case {case_index}/{total_cases} ({question:?}): draft was not accepted: {reason}");
    }
    assert!(
        !draft.truncated,
        "case {case_index}/{total_cases} ({question:?}): draft was truncated at the generation \
         limit; production's partial_answer_draft_for_escalation discards it and falls back to \
         the acknowledgement text instead of using it:\n{}",
        draft.text
    );
    // api.rs の `gate_customer_reply_draft`（private）§3.5(1) と同じ判定。
    // 本番ならこの下書きは捨てられる。
    let out_of_scope = fx.allowlist.out_of_scope_mentions(&draft.text);
    assert!(
        out_of_scope.is_empty(),
        "case {case_index}/{total_cases} ({question:?}): draft mentions out-of-scope models \
         {out_of_scope:?} (production would discard it):\n{}",
        draft.text
    );
    println!("draft: accepted");
    draft.text
}

/// Handoff ケースの最終応答文を組み立て、design doc §10.5 の条件（取次の一文・受付番号）を
/// 確認する。戻り値は確認済みの最終応答文（呼び出し側が println する）。
fn assemble_and_assert_handoff_reply(
    fx: &Fixture,
    draft_text: &str,
    labels: &[String],
    question: &str,
    case_index: usize,
    total_cases: usize,
) -> String {
    let case_id = format!("case-smoketest{case_index:020}");
    let deterministic_block =
        escalation_reply::build_deterministic_block(&case_id, &fx.hours_label, false);
    let final_reply =
        escalation_reply::assemble_partial_answer_reply(draft_text, labels, &deterministic_block);

    let handoff_sentence = escalation_reply::build_handoff_sentence(labels);
    assert!(
        final_reply.contains(&handoff_sentence),
        "case {case_index}/{total_cases} ({question:?}): final reply does not contain the \
         handoff sentence ({handoff_sentence:?}):\n{final_reply}"
    );
    assert!(
        final_reply.contains(CASE_REFERENCE_PREFIX),
        "case {case_index}/{total_cases} ({question:?}): final reply does not contain the case \
         reference line:\n{final_reply}"
    );
    final_reply
}

/// Allowed ケースの最終応答文（下書きそのもの。`api.rs` の `decide_reply_action`（
/// `AnswerDecision::Allowed` で採用済み非truncated下書きがあるときに `ReplyAction::Answer` が
/// 採用する経路と同じ））について、取次項目が無いこと・受付番号を含まないことを design doc
/// §10.5 に沿って確認する。
fn assert_allowed_final_reply(
    final_reply: &str,
    labels: &[String],
    question: &str,
    case_index: usize,
    total_cases: usize,
) {
    // `handoff_items_for_decision` の既存の doc コメント（layer=3 と Allowed は空）どおり、
    // Allowed では取次項目が無いことを明示的に確認する。
    assert!(
        labels.is_empty(),
        "case {case_index}/{total_cases} ({question:?}): expected no handoff items for an \
         \"allowed\" case (design doc §10.5), got {labels:?}"
    );
    assert!(
        !final_reply.contains(CASE_REFERENCE_PREFIX),
        "case {case_index}/{total_cases} ({question:?}): final reply for an \"allowed\" case \
         must not contain the case reference line (design doc §10.5):\n{final_reply}"
    );
}

/// design doc §10.2・本番 `evaluate()`（`server/src/harness/mod.rs` の
/// `handoff_items::is_partial_answer_candidate` 分岐）と同じ条件: 取次候補
/// （第1・2層 Escalate かつ取り次ぐ項目が1つ以上、hearing宣言なし）のときだけ、マッチした signal
/// の lexicon 語彙で引いた補助検索を質問文検索の結果と併合する。それ以外（Allowed・第3層）は
/// 質問文検索の結果（取扱外型番は既に除外済み）をそのまま下書きの材料にする（検索回数を増やさ
/// ない）。
#[allow(clippy::too_many_arguments)]
fn select_material_hits(
    fx: &Fixture,
    decision: &AnswerDecision,
    items: &[handoff_items::HandoffItem],
    question_hits: &[SectionHit],
    signal_hits: &[SectionHit],
    question: &str,
    case_index: usize,
    total_cases: usize,
) -> (Vec<SectionHit>, Option<f32>) {
    if handoff_items::is_partial_answer_candidate(decision, items) {
        let item_signals: std::collections::BTreeSet<&signal::Signal> =
            items.iter().flat_map(|item| item.signals.iter()).collect();
        let covered = signal::Signal::new(SUPPLEMENTARY_SEARCH_SIGNAL);
        assert!(
            item_signals.len() == 1 && item_signals.contains(&covered),
            "case {case_index}/{total_cases} ({question:?}): the case's signal_hits only cover \
             {SUPPLEMENTARY_SEARCH_SIGNAL:?}, but production searches every signal of every \
             handoff item, so the materials would differ from production; handoff item signals: \
             {item_signals:?}"
        );
        let question_best = question_hits.first().map(|h| h.score);
        let (merged_hits, merged_best_score, _) =
            super::supplementary_search::fold_signal_search_results(
                question_hits.to_vec(),
                question_best,
                Vec::new(),
                vec![(
                    signal::Signal::new(SUPPLEMENTARY_SEARCH_SIGNAL),
                    Ok((signal_hits.to_vec(), Vec::new())),
                )],
                &fx.allowlist,
                &format!("smoke-case-{case_index}"),
            );
        (merged_hits, merged_best_score)
    } else {
        (
            question_hits.to_vec(),
            question_hits.first().map(|h| h.score),
        )
    }
}

/// `question_hits` は判定（`decide_for_case`）に使う（本番 `evaluate()` の評価順序で判定確定が
/// 補助検索より先に来るため、merge 前のまま渡す）。`signal_hits` は補助検索語で引いた結果
/// （未加工。`fold_signal_search_results` が内部で取扱外型番の除外を行う）。`expect` で design
/// doc §10.5 の2種類のケースを分岐する。
#[allow(clippy::too_many_arguments)]
async fn run_once(
    fx: &Fixture,
    question: &str,
    question_hits: &[SectionHit],
    signal_hits: &[SectionHit],
    expect: CaseExpectation,
    case_index: usize,
    total_cases: usize,
) {
    println!(
        "==== real_llm_partial_answer_smoke case {case_index}/{total_cases} ({expect:?}): \
         {question:?} ===="
    );
    let (extracted, decision, matched_layer1_rule_ids) =
        decide_for_case(fx, question, question_hits, case_index, total_cases).await;
    assert!(
        extracted.mode != extraction::ExtractionMode::LexiconFallback,
        "case {case_index}/{total_cases} ({question:?}): decide_reply_action forces an \
         EscalationReply on LexiconFallback and skips the draft, so this test's premise fails"
    );
    assert_decision_matches(
        &decision,
        expect,
        &fx.escalation_rules,
        &extracted.signals,
        &matched_layer1_rule_ids,
        question,
        case_index,
        total_cases,
    );
    // design doc `2026-10-07-partial-answer-with-handoff-design.md` §2・§3.1: 取り次ぐ項目
    // （customer_label の列）を、マッチした第1層ルールの `condition` / 第2層禁止領域の
    // `domain_signals` から導出する。layer=1 はマッチした**全**ルールの和集合、layer=3 と
    // Allowed は空。
    let items = handoff_items::handoff_items_for_decision(
        &decision,
        &fx.escalation_rules,
        &fx.prohibited_domains,
        &extracted.signals,
        question,
        &fx.lexicon,
    );
    let labels: Vec<String> = items.iter().map(|item| item.label.clone()).collect();
    println!("handoff items: {labels:?}");
    let (material_hits, material_best_score) = select_material_hits(
        fx,
        &decision,
        &items,
        question_hits,
        signal_hits,
        question,
        case_index,
        total_cases,
    );
    let draft_text = draft_accepted_text(
        fx,
        &decision,
        extracted.mode,
        &items,
        question,
        &material_hits,
        material_best_score,
        &[],
        case_index,
        total_cases,
    )
    .await;
    let outcome = build_single_turn_outcome(&decision, &extracted, &material_hits, draft_text);
    let final_reply = assert_final_reply(
        fx,
        expect,
        &outcome,
        &labels,
        question,
        case_index,
        total_cases,
    );
    println!("final reply:\n{final_reply}");
}

struct EscalatedTurn1 {
    extracted: extraction::ExtractionResult,
    assistant_reply: String,
}

async fn assert_turn1_escalates_to_set_up_the_case(
    fx: &Fixture,
    turn1_question: &str,
    turn1_question_hits: &[SectionHit],
    case_id: &str,
    case_index: usize,
    total_cases: usize,
) -> EscalatedTurn1 {
    let (extracted, decision1, _) = decide_for_case(
        fx,
        turn1_question,
        turn1_question_hits,
        case_index,
        total_cases,
    )
    .await;
    assert!(
        matches!(decision1, AnswerDecision::Escalate { layer: 1 | 2, .. }),
        "case {case_index}/{total_cases}: turn 1 ({turn1_question:?}) must escalate at layer 1 \
         or 2 to set up the already-escalated case, got {decision1:?}"
    );
    let assistant_reply = escalation_reply::assemble_escalation_reply(
        escalation_reply::fallback_ack(false).0,
        &escalation_reply::build_deterministic_block(case_id, &fx.hours_label, false),
    );
    EscalatedTurn1 {
        extracted,
        assistant_reply,
    }
}

fn select_turn_only_signals<'a>(
    fx: &Fixture,
    turn2_signals: &'a signal::SignalSet,
    accumulated: &'a signal::SignalSet,
    case_index: usize,
    total_cases: usize,
) -> &'a signal::SignalSet {
    let policy = ReplyDraftPolicy::SkipWhenUnused {
        response_allowlist: &fx.allowlist,
        already_escalated: true,
        partial_answer_enabled: true,
    };
    let (decide_signals, basis) =
        super::decide_signals_for_policy(&policy, turn2_signals, accumulated);
    assert_eq!(
        basis,
        super::DecisionBasis::TurnOnly,
        "case {case_index}/{total_cases}: an already-escalated case must decide on the current \
         turn's signals only"
    );
    assert_eq!(
        decide_signals, turn2_signals,
        "case {case_index}/{total_cases}: the selected signal set must be turn 2's own signals"
    );
    decide_signals
}

fn assert_accumulated_signals_would_escalate(
    fx: &Fixture,
    turn2_question: &str,
    turn2_question_hits: &[SectionHit],
    accumulated: &signal::SignalSet,
    case_index: usize,
    total_cases: usize,
) {
    let (accumulated_decision, _) = decide_on_signals(
        fx,
        turn2_question,
        turn2_question_hits,
        accumulated,
        accumulated,
        case_index,
        total_cases,
    );
    assert!(
        matches!(accumulated_decision, AnswerDecision::Escalate { .. }),
        "case {case_index}/{total_cases}: deciding turn 2 ({turn2_question:?}) on the accumulated \
         signals must escalate (the pre-fix behavior), otherwise this case does not depend on the \
         turn-only selection; got {accumulated_decision:?}"
    );
}

fn decide_allowed_turn2(
    fx: &Fixture,
    turn2_question: &str,
    turn2_question_hits: &[SectionHit],
    decide_signals: &signal::SignalSet,
    accumulated: &signal::SignalSet,
    case_index: usize,
    total_cases: usize,
) -> AnswerDecision {
    let (decision2, _) = decide_on_signals(
        fx,
        turn2_question,
        turn2_question_hits,
        decide_signals,
        accumulated,
        case_index,
        total_cases,
    );
    println!("decision: {decision2:?}");
    assert!(
        matches!(decision2, AnswerDecision::Allowed { .. }),
        "case {case_index}/{total_cases}: turn 2 ({turn2_question:?}) must be answerable on its \
         own, got {decision2:?}"
    );
    assert_accumulated_signals_would_escalate(
        fx,
        turn2_question,
        turn2_question_hits,
        accumulated,
        case_index,
        total_cases,
    );
    decision2
}

async fn draft_allowed_turn2_answer(
    fx: &Fixture,
    turn1: &EscalatedTurn1,
    turn1_question: &str,
    turn2_question: &str,
    turn2_question_hits: &[SectionHit],
    case_index: usize,
    total_cases: usize,
) -> (
    extraction::ExtractionResult,
    signal::SignalSet,
    AnswerDecision,
    String,
) {
    let extracted2 = fx.harness.extractor.extract(turn2_question).await;
    let signals2 = &extracted2.signals;
    println!(
        "signals: {signals2:?} (extraction_mode={:?})",
        extracted2.mode
    );
    let accumulated: signal::SignalSet = turn1.extracted.signals.union(signals2).cloned().collect();
    let decide_signals =
        select_turn_only_signals(fx, signals2, &accumulated, case_index, total_cases);
    let decision2 = decide_allowed_turn2(
        fx,
        turn2_question,
        turn2_question_hits,
        decide_signals,
        &accumulated,
        case_index,
        total_cases,
    );
    let items = handoff_items::handoff_items_for_decision(
        &decision2,
        &fx.escalation_rules,
        &fx.prohibited_domains,
        signals2,
        turn2_question,
        &fx.lexicon,
    );
    assert!(
        items.is_empty(),
        "case {case_index}/{total_cases}: an Allowed decision must have no handoff items, got \
         {items:?}"
    );
    let history = [
        ReplyHistoryTurn {
            role: ReplyHistoryRole::Customer,
            text: turn1_question.to_owned(),
        },
        ReplyHistoryTurn {
            role: ReplyHistoryRole::Assistant,
            text: turn1.assistant_reply.clone(),
        },
    ];
    let best_score = turn2_question_hits.first().map(|h| h.score);
    let draft_text = draft_accepted_text(
        fx,
        &decision2,
        extracted2.mode,
        &items,
        turn2_question,
        turn2_question_hits,
        best_score,
        &history,
        case_index,
        total_cases,
    )
    .await;
    (extracted2, accumulated, decision2, draft_text)
}

async fn run_escalated_followup_case(
    fx: &Fixture,
    turn1_question: &str,
    turn1_question_hits: &[SectionHit],
    turn2_question: &str,
    turn2_question_hits: &[SectionHit],
    case_index: usize,
    total_cases: usize,
) {
    println!(
        "==== real_llm_partial_answer_smoke case {case_index}/{total_cases} \
         (EscalatedFollowup): turn1={turn1_question:?} turn2={turn2_question:?} ===="
    );
    let case_id = format!("case-smoketest-escalated{case_index:08}");
    let turn1 = assert_turn1_escalates_to_set_up_the_case(
        fx,
        turn1_question,
        turn1_question_hits,
        &case_id,
        case_index,
        total_cases,
    )
    .await;
    let (extracted2, accumulated, decision2, draft_text) = draft_allowed_turn2_answer(
        fx,
        &turn1,
        turn1_question,
        turn2_question,
        turn2_question_hits,
        case_index,
        total_cases,
    )
    .await;
    let new_signals: signal::SignalSet = extracted2
        .signals
        .difference(&turn1.extracted.signals)
        .cloned()
        .collect();
    let outcome = build_outcome(
        &decision2,
        &extracted2,
        turn2_question_hits,
        draft_text.clone(),
        TurnSignals {
            accumulated,
            new_signals,
            basis: super::DecisionBasis::TurnOnly,
        },
    );
    let conv = super::CaseConvState {
        clarify_turns: 0,
        awaiting_time_pref: false,
        time_pref_false_count: 0,
        preferred_contact_time: None,
        time_pref_extraction_error_count: 0,
        escalation_confirmed: true,
    };
    let action = crate::api::decide_reply_action(&outcome, &conv, &fx.config.api);
    let text = match action {
        crate::api::ReplyAction::Answer(text) => text,
        other => panic!(
            "case {case_index}/{total_cases}: turn 2 ({turn2_question:?}) must map to \
             ReplyAction::Answer for an Allowed decision with a non-truncated draft, got {other:?}"
        ),
    };
    let final_reply =
        crate::api::compose_answer_reply_text(&conv, text, &case_id, &fx.hours_label, false);
    let expected_reply = escalation_reply::build_answer_in_escalated_case(
        &draft_text,
        &case_id,
        &fx.hours_label,
        false,
    );
    assert_eq!(
        final_reply, expected_reply,
        "case {case_index}/{total_cases}: final reply is not the drafted answer followed by the \
         already-escalated note"
    );
    println!("final reply:\n{final_reply}");
}

struct TurnSignals {
    accumulated: signal::SignalSet,
    new_signals: signal::SignalSet,
    basis: super::DecisionBasis,
}

fn build_single_turn_outcome(
    decision: &AnswerDecision,
    extracted: &extraction::ExtractionResult,
    material_hits: &[SectionHit],
    draft_text: String,
) -> super::EvaluationOutcome {
    build_outcome(
        decision,
        extracted,
        material_hits,
        draft_text,
        TurnSignals {
            accumulated: extracted.signals.clone(),
            new_signals: extracted.signals.clone(),
            basis: super::DecisionBasis::Accumulated,
        },
    )
}

fn build_outcome(
    decision: &AnswerDecision,
    extracted: &extraction::ExtractionResult,
    material_hits: &[SectionHit],
    draft_text: String,
    turn: TurnSignals,
) -> super::EvaluationOutcome {
    super::EvaluationOutcome {
        decision: decision.clone(),
        signals: extracted.signals.clone(),
        accumulated_signals: turn.accumulated,
        case_id: "case-smoketest".to_owned(),
        clarification_allowed: super::clarification_allowed(decision),
        hits: material_hits.to_vec(),
        audit_event_id: String::new(),
        related_cases: Vec::new(),
        extraction_mode: extracted.mode,
        customer_reply_draft: Some(draft_text),
        customer_reply_draft_truncated: false,
        product_references: Vec::new(),
        new_signals: turn.new_signals,
        handoff_items: Vec::new(),
        partial_answer: false,
        decision_basis: turn.basis,
        applicable_known_resolution: None,
    }
}

/// 本番の `decide_reply_action` で最終応答の種別を算出し、`expect` に応じた最終応答文を組み立てて
/// 形を確認する。どちらのケースにも共通の月額料金の言及（design doc §10.5）をここで1回だけ確認
/// する。戻り値は確認済みの最終応答文。
fn assert_final_reply(
    fx: &Fixture,
    expect: CaseExpectation,
    outcome: &super::EvaluationOutcome,
    labels: &[String],
    question: &str,
    case_index: usize,
    total_cases: usize,
) -> String {
    let conv = super::CaseConvState {
        clarify_turns: 0,
        awaiting_time_pref: false,
        time_pref_false_count: 0,
        preferred_contact_time: None,
        time_pref_extraction_error_count: 0,
        escalation_confirmed: false,
    };
    let action = crate::api::decide_reply_action(outcome, &conv, &fx.config.api);
    let final_reply = match (expect, action) {
        (CaseExpectation::Handoff, crate::api::ReplyAction::EscalationReply) => {
            let draft_text = outcome
                .customer_reply_draft
                .as_deref()
                .expect("build_outcome always sets customer_reply_draft");
            assemble_and_assert_handoff_reply(
                fx,
                draft_text,
                labels,
                question,
                case_index,
                total_cases,
            )
        }
        (CaseExpectation::Allowed, crate::api::ReplyAction::Answer(text)) => {
            assert_allowed_final_reply(&text, labels, question, case_index, total_cases);
            text
        }
        (expect, action) => panic!(
            "case {case_index}/{total_cases} ({question:?}): decide_reply_action returned \
             {action:?} for a {expect:?} case (expected EscalationReply for \"handoff\", \
             Answer for \"allowed\")"
        ),
    };
    assert!(
        final_reply.contains(MONTHLY_FEE_TEXT),
        "case {case_index}/{total_cases} ({question:?}): final reply does not mention the \
         monthly fee ({MONTHLY_FEE_TEXT}):\n{final_reply}"
    );
    final_reply
}

/// design doc §10.3・§10.5 の実出力検証。本番と同じ部品（signal-lexicon.json、rules.json、
/// config.cloudrun.toml の `[llm]`/`[harness]`、実 LLM 抽出・下書き生成）と、本番 MCP
/// `search_manual` の実結果（`CS_VERIFY_CASES_JSON`）を使い、費用を尋ねる複数の言い回しが
/// `expect` どおりの判定（取次 or 回答のみ）になり、最終応答文がそれぞれの形（取次の一文・受付
/// 番号あり、または下書きそのまま）で月額料金を含むことを確認する。1 ケースでも外れたら即座に
/// 失敗させる。
#[tokio::test]
#[ignore = "requires CS_SUPPORT_LLM_API_KEY, network access to the Anthropic API, and \
            CS_VERIFY_CASES_JSON (real search_manual output for the question text and for the \
            signal-guided supplementary search); see design doc \
            2026-10-07-partial-answer-with-handoff-design.md §10.3, §10.5"]
async fn real_llm_partial_answer_smoke() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stdout)
        .try_init()
        .expect(
            "the warn subscriber could not be installed because another global subscriber exists; run this smoke alone as in the module doc command",
        );
    let (config, client) = load_config_and_client();
    let cases = load_verify_cases();

    let lexicon = Arc::new(
        signal::LexiconNormalizer::from_path(&resolve_server_path(
            &config.harness.signal_lexicon_path,
        ))
        .expect("production signal lexicon must load and parse"),
    );
    let dir =
        TempDir(std::env::temp_dir().join(format!("real-llm-smoke-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&dir.0).expect("create temp dir for the harness worm audit log/queue");

    let harness = build_harness(&config, client, &lexicon, &dir.0);
    let fx = build_fixture(config, harness, lexicon);

    let total_cases = cases.len();
    for (index, case) in cases.into_iter().enumerate() {
        let case_index = index + 1;
        // 本番 `evaluate()` と同じ順序: 質問文検索の hits を取扱外型番の除外
        // （`filter_out_of_scope_hits`）に通してから判定・下書きへ渡す。補助検索の併合は取次
        // 候補のときだけ `run_once` 内で行う（design doc §10.2・§10.5）。
        let question_hits = super::filter_out_of_scope_hits(case.question_hits, &fx.allowlist);
        match case.expectation {
            LoadedExpectation::EscalatedFollowup {
                question: followup_question,
                hits: followup_hits,
            } => {
                let followup_hits = super::filter_out_of_scope_hits(followup_hits, &fx.allowlist);
                run_escalated_followup_case(
                    &fx,
                    &case.question,
                    &question_hits,
                    &followup_question,
                    &followup_hits,
                    case_index,
                    total_cases,
                )
                .await;
            }
            LoadedExpectation::Single(expect) => {
                run_once(
                    &fx,
                    &case.question,
                    &question_hits,
                    &case.signal_hits,
                    expect,
                    case_index,
                    total_cases,
                )
                .await;
            }
        }
    }
}

/// `CS_VERIFY_CASES_JSON` の各ケースは `expect` の綴り・大小文字・型を人手の JSON 編集で
/// 崩しうる。`#[derive(serde::Deserialize)]` だけで不正値が確実に `Err` になることを、実 API
/// キーもネットワークも要らない単体テストで確認する。
#[test]
fn verify_case_rejects_invalid_expect() {
    let cases: [(&str, &str); 4] = [
        (
            "expect field missing",
            r#"{"question":"q","hits_json":"h.json"}"#,
        ),
        (
            "expect capitalized (\"Handoff\")",
            r#"{"question":"q","hits_json":"h.json","expect":"Handoff"}"#,
        ),
        (
            "expect misspelled (\"handof\")",
            r#"{"question":"q","hits_json":"h.json","expect":"handof"}"#,
        ),
        (
            "expect is a number",
            r#"{"question":"q","hits_json":"h.json","expect":1}"#,
        ),
    ];
    for (label, json) in cases {
        assert!(
            serde_json::from_str::<VerifyCase>(json).is_err(),
            "expected {label} ({json:?}) to fail VerifyCase deserialization, but it succeeded"
        );
    }
}

fn paraphrase_case(question: &str, expect: CaseExpectation) -> VerifyCase {
    VerifyCase {
        question: question.to_string(),
        hits_json: "h.json".to_string(),
        expectation: Expectation::Single(expect),
    }
}

fn followup_paraphrase_case(question: &str) -> VerifyCase {
    VerifyCase {
        question: question.to_string(),
        hits_json: "h.json".to_string(),
        expectation: Expectation::EscalatedFollowup(FollowupTurn {
            question: "設定方法を教えてください".to_string(),
            hits_json: "f.json".to_string(),
        }),
    }
}

#[test]
#[should_panic(expected = "at least one distinct \"escalated_followup\" case")]
fn case_paraphrases_reject_a_list_without_an_escalated_followup_case() {
    assert_case_paraphrases(&[
        paraphrase_case(PRODUCTION_REGRESSION_QUESTION, CaseExpectation::Handoff),
        paraphrase_case("導入費用を教えてください", CaseExpectation::Handoff),
        paraphrase_case("設定方法を教えてください", CaseExpectation::Allowed),
    ]);
}

#[test]
fn case_paraphrases_accept_a_list_with_an_escalated_followup_case() {
    assert_case_paraphrases(&[
        paraphrase_case(PRODUCTION_REGRESSION_QUESTION, CaseExpectation::Handoff),
        paraphrase_case("導入費用を教えてください", CaseExpectation::Handoff),
        paraphrase_case("設定方法を教えてください", CaseExpectation::Allowed),
        followup_paraphrase_case("取付工事の費用はいくらですか"),
    ]);
}

#[test]
fn verify_case_followup_fields_are_validated_against_expect() {
    let case_json = |expect: &str, question: bool, hits: bool| {
        let mut fields = vec![
            r#""question":"q""#.to_string(),
            r#""hits_json":"h.json""#.to_string(),
            format!(r#""expect":"{expect}""#),
        ];
        if question {
            fields.push(r#""followup_question":"q2""#.to_string());
        }
        if hits {
            fields.push(r#""followup_hits_json":"f.json""#.to_string());
        }
        format!("{{{}}}", fields.join(","))
    };
    let cases: [(&str, String, Option<&str>); 12] = [
        (
            "escalated_followup with both fields",
            case_json("escalated_followup", true, true),
            None,
        ),
        (
            "handoff without followup fields",
            case_json("handoff", false, false),
            None,
        ),
        (
            "escalated_followup without followup_question",
            case_json("escalated_followup", false, true),
            Some("invalid field \"followup_question\": required"),
        ),
        (
            "escalated_followup without followup_hits_json",
            case_json("escalated_followup", true, false),
            Some("invalid field \"followup_hits_json\": required"),
        ),
        (
            "handoff with followup_question",
            case_json("handoff", true, false),
            Some("invalid field \"followup_question\": not allowed for expect \"handoff\""),
        ),
        (
            "handoff with followup_hits_json",
            case_json("handoff", false, true),
            Some("invalid field \"followup_hits_json\": not allowed for expect \"handoff\""),
        ),
        (
            "allowed with followup_question",
            case_json("allowed", true, false),
            Some("invalid field \"followup_question\": not allowed for expect \"allowed\""),
        ),
        (
            "allowed with followup_hits_json",
            case_json("allowed", false, true),
            Some("invalid field \"followup_hits_json\": not allowed for expect \"allowed\""),
        ),
        (
            "unknown field",
            r#"{"question":"q","hits_json":"h.json","expect":"handoff","followup_questoin":"q2"}"#.to_string(),
            Some("unknown field `followup_questoin`"),
        ),
        (
            "empty question",
            r#"{"question":"","hits_json":"h.json","expect":"handoff"}"#.to_string(),
            Some("invalid field \"question\": empty"),
        ),
        (
            "whitespace-only followup_question",
            r#"{"question":"q","hits_json":"h.json","expect":"escalated_followup","followup_question":"  \n","followup_hits_json":"f.json"}"#.to_string(),
            Some("invalid field \"followup_question\": empty"),
        ),
        (
            "empty followup_hits_json",
            r#"{"question":"q","hits_json":"h.json","expect":"escalated_followup","followup_question":"q2","followup_hits_json":""}"#.to_string(),
            Some("invalid field \"followup_hits_json\": empty"),
        ),
    ];
    for (label, json, expected_error) in cases {
        match (serde_json::from_str::<VerifyCase>(&json), expected_error) {
            (Ok(_), None) => {}
            (Err(err), Some(fragment)) => assert!(
                err.to_string().contains(fragment)
                    && (!fragment.starts_with("invalid field")
                        || err.to_string().contains("(case \"")),
                "{label}: expected an error containing {fragment:?}, got {err:?}"
            ),
            (result, expected) => {
                panic!("{label}: expected error {expected:?}, got {result:?}")
            }
        }
    }
}
