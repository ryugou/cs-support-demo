//! design doc `2026-10-07-partial-answer-with-handoff-design.md` §9「実出力の検証」。
//!
//! 単体テストと reviewer の判定だけでは、顧客に返る最終文面を確認したことにならない
//! （2026-10-09 の本番で下書きが全件捨てられていた実例を参照）。ここは本番と同じ部品
//! （`data/urtect/signal-lexicon.json` / `data/urtect/rules.json` /
//! `config.cloudrun.toml` の `[llm]` / `[harness]`、実 Anthropic API）を使い、取次ターンでも
//! 答えられる部分（料金）が実際に顧客向け文面に出ることを確認する。
//!
//! `#[ignore]`: 実 API キー・ネットワーク到達性・`ingest_urtect_lp --dry-run` の出力ファイルを
//! 要求するため CI では走らせない。
//! ```sh
//! cargo test --manifest-path server/Cargo.toml --lib real_llm_partial_answer_smoke -- \
//!   --ignored --nocapture
//! ```
//! API キーは env からのみ読み（`llm::AnthropicClient::from_config`）、ファイル・ログには書かない。

use super::decision::{self, AnswerDecision, DecisionInput, StakesInput, Thresholds};
use super::extraction::{
    self, AnthropicProductReferenceClassifier, AnthropicSignalClassifier, ClassifyLlm,
    ClassifyProductReferences, HybridExtractor, ProductReferenceExtractor,
};
use super::product_gate::ProductAllowlist;
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
const SECTIONS_ENV: &str = "CS_VERIFY_SECTIONS_JSON";
const QUESTION_ENV: &str = "CS_VERIFY_QUESTION";
const RUNS_ENV: &str = "CS_VERIFY_RUNS";
const DEFAULT_QUESTION: &str = "導入にどれくらいの費用がかかるの？";
const DEFAULT_RUNS: usize = 5;

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

/// `ingest_urtect_lp --dry-run` が出力する 1 節分の形（`anchor` / `title_ja` / `body` /
/// `source_url` のみ使う。`body_chars` は無視する）。
#[derive(Debug, serde::Deserialize)]
struct DrySection {
    anchor: String,
    title_ja: String,
    body: String,
    source_url: String,
}

#[derive(Debug, serde::Deserialize)]
struct DryRunOutput {
    sections: Vec<DrySection>,
}

/// design doc §9 の材料固定: sections JSON の `price` / `faq` / `flow` を、この順で
/// スコア 0.9 / 0.7 / 0.6 の `SectionHit` にする。本番の上位 3 件の厳密な再現ではないが、
/// 料金ページの本文は同一である。
fn material_hits(sections_json_path: &str) -> Vec<SectionHit> {
    let body = std::fs::read_to_string(sections_json_path).unwrap_or_else(|err| {
        panic!(
            "{SECTIONS_ENV} must point at a readable file ({sections_json_path:?}); run \
             `ingest_urtect_lp --dry-run` and point {SECTIONS_ENV} at its output: {err}"
        )
    });
    let parsed: DryRunOutput = serde_json::from_str(&body).unwrap_or_else(|err| {
        panic!(
            "{SECTIONS_ENV} ({sections_json_path:?}) is not `ingest_urtect_lp --dry-run` JSON: \
             {err}"
        )
    });
    [("price", 0.9_f32), ("faq", 0.7), ("flow", 0.6)]
        .into_iter()
        .map(|(anchor, score)| {
            let section = parsed
                .sections
                .iter()
                .find(|s| s.anchor == anchor)
                .unwrap_or_else(|| {
                    panic!(
                        "{SECTIONS_ENV} ({sections_json_path:?}) has no section with anchor \
                         {anchor:?}; re-run `ingest_urtect_lp --dry-run` against the current site"
                    )
                });
            SectionHit {
                section_key: format!("lp#{anchor}"),
                title_ja: section.title_ja.clone(),
                body_ja: Some(section.body.clone()),
                body_en: None,
                translation_status: None,
                breadcrumb: vec!["URTECT".to_string(), section.title_ja.clone()],
                score,
                source_url: Some(section.source_url.clone()),
            }
        })
        .collect()
}

/// `CS_VERIFY_RUNS`（既定 [`DEFAULT_RUNS`]）を正の整数として読む。
fn runs_from_env() -> usize {
    let Ok(raw) = std::env::var(RUNS_ENV) else {
        return DEFAULT_RUNS;
    };
    let runs: usize = raw
        .parse()
        .unwrap_or_else(|err| panic!("{RUNS_ENV} must be a positive integer, got {raw:?}: {err}"));
    assert!(runs > 0, "{RUNS_ENV} must be greater than 0, got {runs}");
    runs
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
    mandatory_domains: Vec<rules::ProhibitedDomain>,
    thresholds: Thresholds,
    allowlist: ProductAllowlist,
    hours_label: String,
    hits: Vec<SectionHit>,
    question: String,
}

fn load_config_and_client() -> (AppConfig, AnthropicClient) {
    let config = AppConfig::load(&server_path("config.cloudrun.toml"))
        .expect("load server/config.cloudrun.toml");
    // 下書きが無効なら本番は下書きを作らず、§9 の合否は成立しない（`Harness::build` と同じ前提）。
    assert!(
        config.harness.customer_reply_draft_enabled,
        "config.cloudrun.toml must have [harness] customer_reply_draft_enabled = true; \
         production builds no draft otherwise, so this smoke test cannot verify §9"
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
    }
}

fn build_fixture(
    config: AppConfig,
    harness: Harness,
    lexicon: Arc<signal::LexiconNormalizer>,
    hits: Vec<SectionHit>,
    question: String,
) -> Fixture {
    let prohibited_domains = load_bundled_prohibited_domains();
    let mandatory_domains: Vec<rules::ProhibitedDomain> = prohibited_domains
        .iter()
        .filter(|d| d.binding == rules::Binding::Mandatory)
        .cloned()
        .collect();
    let thresholds: Thresholds = (&config.harness.thresholds).into();
    let hours_label = hours::business_hours_label(&config.api.business_hours);
    Fixture {
        harness,
        lexicon,
        escalation_rules: load_bundled_escalation_rules(),
        prohibited_domains,
        mandatory_domains,
        thresholds,
        allowlist: ProductAllowlist::from_models(seed_product_models()),
        hours_label,
        hits,
        question,
        config,
    }
}

/// signal 抽出から `decide` までを実行し、第1層 `initial-cost-quote` の取次であることを確かめる。
async fn decide_and_assert_layer1(
    fx: &Fixture,
    run: usize,
    runs: usize,
) -> (extraction::ExtractionResult, AnswerDecision) {
    let question = &fx.question;
    let extracted = fx.harness.extractor.extract(question).await;
    let (signals, mode) = (&extracted.signals, extracted.mode);
    // 製品言及抽出は signal 抽出と完全に独立したコンポーネント（Issue #52）。この質問は
    // 型番言及を含まないため判定には使わないが、本番と同じ配線を実際に動かして確認する
    // （design doc §9「実 LLM による...製品言及抽出」）。
    let product_references = fx
        .harness
        .product_reference_extractor
        .extract(question, fx.allowlist.display_list())
        .await;
    println!("signals: {signals:?} (extraction_mode={mode:?})");
    println!("product_references: {product_references:?}");

    // `Harness::evaluate` と同じ式（単ターンなので accumulated = signals）。
    let stakes_input = StakesInput {
        mandatory_domain_near: rules::match_layer2(&fx.mandatory_domains, signals, question)
            .is_some(),
        ng_near_hit: fx.harness.ng.near_hit(question),
        hazard_signal_count: signals
            .iter()
            .filter(|s| fx.lexicon.class_of(s) == Some(signal::SignalClass::Hazard))
            .count(),
    };
    let price_section_key = fx.hits[0].section_key.clone();
    let decision = decision::decide(&DecisionInput {
        question_signals: signals,
        question_raw: question,
        rules: &fx.escalation_rules,
        domains: &fx.prohibited_domains,
        resolutions: &[],
        best_manual_score: Some(fx.hits[0].score),
        best_manual_sections: std::slice::from_ref(&price_section_key),
        stakes_input,
        thresholds: &fx.thresholds,
        default_route: &fx.config.harness.default_escalation_route,
    });
    let matched_layer1_rule_ids: Vec<String> = matching_layer1_rules(&fx.escalation_rules, signals)
        .into_iter()
        .map(|rule| rule.id.clone())
        .collect();
    println!("decision: {decision:?}");
    println!("matched layer-1 rule ids: {matched_layer1_rule_ids:?}");

    // 判定の合否は下書き生成より先に見る（判定が外れたまま下書き失敗で panic すると
    // 失敗理由が誤誘導になる）。併発ルールがあっても `decide` が選ぶルールが
    // initial-cost-quote なら §9 は満たされる。
    assert!(
        matches!(&decision, AnswerDecision::Escalate { layer: 1, .. }),
        "run {run}/{runs}: expected a layer-1 escalate decision, got {decision:?}"
    );
    let selected_rule_id = match_layer1(&fx.escalation_rules, signals).map(|r| r.id.as_str());
    assert_eq!(
        selected_rule_id,
        Some("initial-cost-quote"),
        "run {run}/{runs}: expected initial-cost-quote to be the selected layer-1 rule \
         (matched: {matched_layer1_rule_ids:?})"
    );
    (extracted, decision)
}

/// 下書きを生成し、本番と同じ採否判定（`accept_draft`、truncated 判定、取扱外型番ゲート）を
/// 通して本文を返す。
async fn draft_accepted_text(
    fx: &Fixture,
    decision: &AnswerDecision,
    mode: extraction::ExtractionMode,
    items: &[handoff_items::HandoffItem],
    run: usize,
    runs: usize,
) -> String {
    let labels: Vec<String> = items.iter().map(|item| item.label.clone()).collect();
    let policy = ReplyDraftPolicy::SkipWhenUnused {
        response_allowlist: &fx.allowlist,
        already_escalated: false,
    };
    let plan = super::plan_reply_draft(
        &policy,
        decision,
        items,
        mode,
        false,
        Some(fx.hits[0].score),
        &fx.thresholds,
        fx.config.harness.customer_reply_draft_enabled,
    );

    let draft = if plan.generate {
        fx.harness
            .draft_customer_reply(
                &fx.question,
                decision,
                &fx.hits,
                &[],
                &[],
                false,
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
            "run {run}/{runs}: draft_customer_reply did not generate a draft \
             (plan.generate={}, plan.partial_answer_ok={})",
            plan.generate, plan.partial_answer_ok
        )
    };
    if let Err(reason) =
        handoff_items::accept_draft(&draft.text, plan.partial_answer_ok, items, &fx.lexicon)
    {
        panic!("run {run}/{runs}: draft was not accepted: {reason}");
    }
    assert!(
        !draft.truncated,
        "run {run}/{runs}: draft was truncated at the generation limit; production's \
         partial_answer_draft_for_escalation discards it and falls back to the \
         acknowledgement text instead of using it:\n{}",
        draft.text
    );
    // api.rs の `gate_customer_reply_draft`（private）§3.5(1) と同じ判定。
    // 本番ならこの下書きは捨てられる。
    let out_of_scope = fx.allowlist.out_of_scope_mentions(&draft.text);
    assert!(
        out_of_scope.is_empty(),
        "run {run}/{runs}: draft mentions out-of-scope models \
         {out_of_scope:?} (production would discard it):\n{}",
        draft.text
    );
    println!("draft: accepted");
    draft.text
}

async fn run_once(fx: &Fixture, run: usize, runs: usize) {
    println!("==== real_llm_partial_answer_smoke run {run}/{runs} ====");
    let (extracted, decision) = decide_and_assert_layer1(fx, run, runs).await;
    let items = handoff_items::handoff_items_for_decision(
        &decision,
        &fx.escalation_rules,
        &fx.prohibited_domains,
        &extracted.signals,
        &fx.question,
        &fx.lexicon,
    );
    let labels: Vec<String> = items.iter().map(|item| item.label.clone()).collect();
    println!("handoff items: {labels:?}");

    let draft_text = draft_accepted_text(fx, &decision, extracted.mode, &items, run, runs).await;

    let case_id = format!("case-smoketest{run:020}");
    let deterministic_block =
        escalation_reply::build_deterministic_block(&case_id, &fx.hours_label, false);
    let final_reply =
        escalation_reply::assemble_partial_answer_reply(&draft_text, &labels, &deterministic_block);
    println!("final reply:\n{final_reply}");

    let handoff_sentence = escalation_reply::build_handoff_sentence(&labels);
    assert!(
        final_reply.contains("1,540"),
        "run {run}/{runs}: final reply does not mention the monthly fee (1,540):\n{final_reply}"
    );
    assert!(
        final_reply.contains(&handoff_sentence),
        "run {run}/{runs}: final reply does not contain the handoff sentence \
         ({handoff_sentence:?}):\n{final_reply}"
    );
    assert!(
        final_reply.contains("受付番号: "),
        "run {run}/{runs}: final reply does not contain the case reference line:\n{final_reply}"
    );
}

/// design doc §9 の実出力検証。本番と同じ部品（signal-lexicon.json、rules.json、
/// config.cloudrun.toml の `[llm]`/`[harness]`、実 LLM 抽出・下書き生成）を使い、
/// 「導入にどれくらいの費用がかかるの？」が第1層 `initial-cost-quote` にマッチし、
/// 下書きが採用され、最終応答文に月額料金・取次の一文・受付番号が含まれることを
/// `CS_VERIFY_RUNS` 回連続で確認する。1 run でも外れたら即座に失敗させる。
#[tokio::test]
#[ignore = "requires CS_SUPPORT_LLM_API_KEY, network access to the Anthropic API, and \
            CS_VERIFY_SECTIONS_JSON (ingest_urtect_lp --dry-run output); see design doc \
            2026-10-07-partial-answer-with-handoff-design.md §9"]
async fn real_llm_partial_answer_smoke() {
    let (config, client) = load_config_and_client();
    let sections_json_path = std::env::var(SECTIONS_ENV).unwrap_or_else(|_| {
        panic!(
            "{SECTIONS_ENV} is not set; point it at the output file of \
             `ingest_urtect_lp --dry-run` (the price/faq/flow sections of the URTECT landing \
             page)"
        )
    });
    let question = std::env::var(QUESTION_ENV).unwrap_or_else(|_| DEFAULT_QUESTION.to_string());
    let runs = runs_from_env();
    let hits = material_hits(&sections_json_path);

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
    let fx = build_fixture(config, harness, lexicon, hits, question);
    for run in 1..=runs {
        run_once(&fx, run, runs).await;
    }
}
