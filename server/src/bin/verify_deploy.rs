//! デプロイ後スモークの機械化（Issue #40）。
//!
//! 正本: `docs/superpowers/specs/2026-10-06-post-deploy-smoke-design.md`。
//!
//! 本番 vegapunk に対して**読み取り専用**の検証を行う CLI。次の 2 種類を検査する:
//!
//! 1. 管理 API（`server/src/admin.rs`）のハンドラが使う読み出し関数を、HTTP を介さず
//!    同じ関数で直接呼ぶ（`threads` 一覧・`thread_detail`・`stats`）。
//! 2. 期待値ファイル（`--expectations`）に書いた発話を、同梱 lexicon と vegapunk 投入済みの
//!    ルールで第1層判定し、期待どおりの結果になるか確認する。
//!
//! vegapunk へは一切書き込まない。case・会話ターン・監査イベントを作らない。LLM を呼ばない
//! （spec §1, §5）。判定ロジック（signal 正規化・`match_layer1`）はサービスと同じ関数を使い、
//! この CLI では複製しない（spec §5）。
//!
//! 管理 API の読み出し検査・第1層判定の本番ルール読み込みは実 vegapunk が必要なため単体テスト
//! 対象にしない（spec §7）。結果を pass/fail/skipped に分類する部分・期待値ファイルの
//! パース・出力に顧客発話本文が含まれないことは純関数として固定する。

use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    admin::{self, AdminState},
    config::{AppConfig, ManualSchemaKind, ProjectConfig},
    harness::{
        authn,
        rules::{match_layer1, EscalationRule},
        scope,
        signal::SignalNormalizer,
        Harness, RequestContext,
    },
    vegapunk::VegapunkClient,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

#[derive(Debug, Parser)]
#[command(about = "Read-only post-deploy smoke checks against production vegapunk (Issue #40)")]
struct Args {
    /// 検査する対象 `<config_path>:<project_id>`（複数指定可、spec §2.1）。設定ファイルは
    /// サービスと同じ形式。最後の `:` で分割する。同じ設定ファイルを複数の対象で指定してよい。
    #[arg(long = "target", required = true)]
    target: Vec<String>,
    /// 期待値ファイル（省略時は第1層判定の検査を行わない）。
    #[arg(long)]
    expectations: Option<PathBuf>,
    /// `--expectations` の対象 project_id。`--expectations` 指定時は必須で、`--target` の
    /// いずれかの project_id と一致しなければならない（spec §3.2）。
    #[arg(long)]
    expectations_project: Option<String>,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN")]
    vegapunk_bearer_token: Option<String>,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN_FILE")]
    vegapunk_bearer_token_file: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// 出力の型（spec §4）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct CheckResult {
    name: String,
    status: CheckStatus,
    detail: Value,
}

impl CheckResult {
    fn pass(name: &str, detail: Value) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Pass,
            detail,
        }
    }

    fn fail(name: &str, detail: Value) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Fail,
            detail,
        }
    }

    fn skipped(name: &str, detail: Value) -> Self {
        Self {
            name: name.to_string(),
            status: CheckStatus::Skipped,
            detail,
        }
    }
}

/// 第1層判定 1 件分の記録（spec §3.2, §4）。`utterance` は期待値ファイルに書いた検査用の
/// 発話であり、顧客の発話ではない（spec §4 の注記）。
#[derive(Debug, Clone, Serialize, PartialEq)]
struct Layer1Result {
    utterance: String,
    signals: Vec<String>,
    matched_rule: Option<String>,
    expect_rule: Option<String>,
    status: CheckStatus,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectReport {
    /// `--target` で渡された設定ファイルのパス（引数の文字列そのまま、spec §4）。
    config: String,
    project_id: String,
    schema: String,
    checks: Vec<CheckResult>,
    layer1: Vec<Layer1Result>,
}

#[derive(Debug, Clone, Serialize)]
struct Report {
    passed: bool,
    projects: Vec<ProjectReport>,
}

// ---------------------------------------------------------------------------
// 期待値ファイル（spec §2.2）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Layer1Case {
    utterance: String,
    expect_rule: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ExpectationsFile {
    layer1: Vec<Layer1Case>,
}

/// 期待値ファイルのパース。未知のフィールドは拒否する（`deny_unknown_fields`）。`layer1` が
/// 空の場合はエラーにする（spec §2.2）。
fn parse_expectations_file(body: &str) -> Result<ExpectationsFile> {
    let file: ExpectationsFile =
        serde_json::from_str(body).context("parse expectations file json")?;
    if file.layer1.is_empty() {
        anyhow::bail!("expectations file layer1 must not be empty");
    }
    Ok(file)
}

// ---------------------------------------------------------------------------
// 引数の検証・解決（純関数）
// ---------------------------------------------------------------------------

/// `--target` 1 件分: 設定ファイルのパスと、その設定ファイルに定義された project_id。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// 引数で渡された文字列そのまま（出力 JSON の `config` に使う）。
    config_path: String,
    project_id: String,
}

/// `<config_path>:<project_id>` を解釈する純関数。パスに `:` を含み得るため**最後の `:`** で
/// 分割する（project_id に `:` は含まれない）。`:` が無い・どちらかが空はエラー（spec §2.1）。
fn parse_target(raw: &str) -> Result<Target> {
    let Some((config_path, project_id)) = raw.rsplit_once(':') else {
        anyhow::bail!("--target {raw:?} must be <config_path>:<project_id> (no ':' found)");
    };
    if config_path.is_empty() {
        anyhow::bail!("--target {raw:?} has an empty config path");
    }
    if project_id.is_empty() {
        anyhow::bail!("--target {raw:?} has an empty project_id");
    }
    Ok(Target {
        config_path: config_path.to_string(),
        project_id: project_id.to_string(),
    })
}

/// `--target` 全件を解釈し、同じ project_id が複数の対象に現れる場合はエラーにする
/// （期待値の対象や出力の `project_id` が一意に決まらなくなるため、spec §2.1）。
fn parse_targets(raw: &[String]) -> Result<Vec<Target>> {
    let mut targets: Vec<Target> = Vec::with_capacity(raw.len());
    for item in raw {
        let target = parse_target(item)?;
        if let Some(first) = targets.iter().find(|t| t.project_id == target.project_id) {
            anyhow::bail!(
                "project_id {:?} appears in more than one --target (config {:?} and config {:?})",
                target.project_id,
                first.config_path,
                target.config_path
            );
        }
        targets.push(target);
    }
    Ok(targets)
}

/// 同じ設定ファイルを指す対象のまとまり。設定の読み込み・読み取り専用化・`Harness::build`・
/// vegapunk クライアントの生成は、このまとまりごとに 1 回行う（spec §5.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigGroup {
    config_path: String,
    project_ids: Vec<String>,
}

/// 対象を設定ファイルごとにまとめる。出現順を保つ。
fn group_targets_by_config(targets: &[Target]) -> Vec<ConfigGroup> {
    let mut groups: Vec<ConfigGroup> = Vec::new();
    for target in targets {
        match groups
            .iter_mut()
            .find(|g| g.config_path == target.config_path)
        {
            Some(group) => group.project_ids.push(target.project_id.clone()),
            None => groups.push(ConfigGroup {
                config_path: target.config_path.clone(),
                project_ids: vec![target.project_id.clone()],
            }),
        }
    }
    groups
}

/// 設定ファイルごとの一時サブディレクトリ。異なる設定の監査ログ・キューのパスが衝突しない
/// ようにする（spec §5.1）。`index` は `group_targets_by_config` の順序。
fn config_scratch_dir(scratch_root: &Path, index: usize) -> PathBuf {
    scratch_root.join(format!("config-{index}"))
}

/// 引数全体を検証し、解釈済みの対象を返す（spec §2.1, §3.2 末尾）。
fn validate_args(args: &Args) -> Result<Vec<Target>> {
    let targets = parse_targets(&args.target)?;
    if args.expectations.is_some() != args.expectations_project.is_some() {
        anyhow::bail!(
            "--expectations and --expectations-project must be given together (expectations={:?}, \
             expectations_project={:?})",
            args.expectations,
            args.expectations_project
        );
    }
    if let Some(expectations_project) = &args.expectations_project {
        if !targets
            .iter()
            .any(|t| &t.project_id == expectations_project)
        {
            anyhow::bail!(
                "--expectations-project {expectations_project:?} must be one of the --target \
                 project_ids {:?}",
                targets.iter().map(|t| &t.project_id).collect::<Vec<_>>()
            );
        }
    }
    Ok(targets)
}

/// `--target` で指定された project_id を、その設定ファイルの `[[projects]]` から解決する。
/// 設定ファイルに無い project はエラーにする（spec §2.1）。
fn resolve_projects<'a>(
    projects: &'a [ProjectConfig],
    project_ids: &[String],
) -> Result<Vec<&'a ProjectConfig>> {
    project_ids
        .iter()
        .map(|id| {
            projects
                .iter()
                .find(|p| &p.project_id == id)
                .ok_or_else(|| anyhow::anyhow!("project {id:?} is not configured"))
        })
        .collect()
}

/// vegapunk bearer token の解決。`VEGAPUNK_BEARER_TOKEN` / `VEGAPUNK_BEARER_TOKEN_FILE`
/// から読む（spec §2.1）。`verify_alarmcom.rs` の `read_token` と同じ「fail closed」方針
/// （`main.rs::read_bearer_token` と異なり、どちらも無ければ空文字へ倒さずエラーにする。
/// この CLI は読み取り専用の検証が目的であり、無効な認証情報のまま全チェックが
/// 失敗するより、起動時点で原因を伝える方が運用者にとって分かりやすい）。
fn read_bearer_token(args: &Args) -> Result<String> {
    if let Some(token) = &args.vegapunk_bearer_token {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if let Some(path) = &args.vegapunk_bearer_token_file {
        let body = fs::read_to_string(path)
            .with_context(|| format!("read vegapunk bearer token file {}", path.display()))?;
        let trimmed = body.trim();
        if trimmed.is_empty() {
            anyhow::bail!("vegapunk bearer token file {} is empty", path.display());
        }
        return Ok(trimmed.to_string());
    }
    anyhow::bail!(
        "vegapunk bearer token not found; set VEGAPUNK_BEARER_TOKEN or \
         VEGAPUNK_BEARER_TOKEN_FILE"
    );
}

/// `Harness::build` に渡す、読み取り専用化した設定を作る（spec §1, §5）。
///
/// 本番 config をそのまま渡すと `Harness::build` が次を要求・実行してしまう:
/// - `[llm] enabled = true` → `CS_SUPPORT_LLM_API_KEY` が無いと起動時 fail closed
/// - `[jev] enabled = true` → `TYPESAFE_API_KEY` が無いと起動時 fail closed
/// - `audit_log_path` → 本番監査ログを create_dir_all + hash chain 検証 + append で open
///
/// この CLI は LLM・Jev を呼ばず何も書かない不変条件を持つため、読み取り専用 job に不要な
/// 秘密（最小権限に反する）や本番監査ログへの接触を持ち込まない。そこで LLM / Jev /
/// 返信文下書きを無効化し、書き込み先パスは実行ごとに一意な一時ディレクトリへ逃がす。
/// lexicon・NG 辞書・projects・vegapunk 設定は変えない（spec §3.2: サービスと同じ方法で
/// lexicon を読むため）。
fn read_only_harness_config(config: &AppConfig, scratch_dir: &Path) -> AppConfig {
    let mut overridden = config.clone();
    overridden.llm.enabled = false;
    overridden.jev.enabled = false;
    overridden.harness.customer_reply_draft_enabled = false;
    overridden.harness.audit_log_path = scratch_dir
        .join("audit")
        .join("audit.jsonl")
        .to_string_lossy()
        .into_owned();
    overridden.harness.search_improvement_queue_path = scratch_dir
        .join("search-improvement-queue.jsonl")
        .to_string_lossy()
        .into_owned();
    overridden
}

/// この CLI は実際の Google identity を持たないため、`Harness::begin` を経由せず
/// `RequestContext` を直接組み立てる（`server/src/harness/mod.rs` の `#[cfg(test)]`
/// `supervisor_ctx` と同じパターン）。`load_conv_state` / `load_case_signals` は
/// `ctx.schema` だけを参照し、`ctx.actor` / `ctx.scope` は参照しないため
/// （`harness/mod.rs` で確認済み）、actor/scope はプレースホルダでよい。
fn placeholder_request_context(schema: &str, manual_schema: ManualSchemaKind) -> RequestContext {
    RequestContext {
        actor: authn::Actor {
            sub: "verify-deploy-cli".to_string(),
            email: "verify-deploy-cli@sivira.co".to_string(),
            role: authn::Role::Supervisor,
            allowed_schemas: vec![schema.to_string()],
        },
        scope: scope::AccessScope {
            allowed_schemas: vec![schema.to_string()],
            max_sensitivity: None,
            label_allowlist: None,
        },
        schema: schema.to_string(),
        request_id: "verify-deploy".to_string(),
        manual_schema,
    }
}

// ---------------------------------------------------------------------------
// 管理 API 読み出し検査（spec §3.1）
// ---------------------------------------------------------------------------

/// `threads` 検査の結果から、`thread_detail` 検査を実行すべきかを決める純関数。
///
/// `threads_page` が `None`（`threads` 検査自体が失敗した、または取得していない）の場合も
/// `Skip` にする: 対象の `case_id` を特定できないため probe しようがなく、`threads` 側の
/// fail で既に不合格要因は記録済みなので、ここを fail にして二重に不合格要因を作らない。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ThreadDetailPlan {
    Probe(String),
    Skip,
}

fn plan_thread_detail_check(threads_page: Option<&admin::ThreadsPage>) -> ThreadDetailPlan {
    match threads_page.and_then(|page| page.threads.first()) {
        Some(summary) => ThreadDetailPlan::Probe(summary.case_id.clone()),
        None => ThreadDetailPlan::Skip,
    }
}

/// `threads` 検査結果から `CheckResult` を組み立てる純関数。`detail` には件数しか含めない
/// （spec §4: 顧客の発話本文・会話の内容を出さない）。
fn threads_check_from_page(page: &admin::ThreadsPage) -> CheckResult {
    CheckResult::pass("threads", json!({ "count": page.threads.len() }))
}

/// `thread_detail` 検査の成功結果から `CheckResult` を組み立てる純関数。`detail` にはターン数
/// しか含めない（`ThreadDetail` は件数アクセサ `turn_count` だけを公開しており、この CLI は
/// 発話本文を保持するターン一覧そのものへアクセスしない）。
fn thread_detail_check_from_turn_count(turn_count: usize) -> CheckResult {
    CheckResult::pass("thread_detail", json!({ "turns": turn_count }))
}

async fn run_thread_detail_check(
    admin_state: &AdminState,
    project: &ProjectConfig,
    plan: ThreadDetailPlan,
) -> CheckResult {
    let case_id = match plan {
        ThreadDetailPlan::Skip => return CheckResult::skipped("thread_detail", json!({})),
        ThreadDetailPlan::Probe(case_id) => case_id,
    };
    let ctx = placeholder_request_context(&project.schema, project.manual_schema);
    match admin::fetch_thread_detail(admin_state, &ctx, &case_id).await {
        Ok(Some(detail)) => thread_detail_check_from_turn_count(detail.turn_count()),
        Ok(None) => {
            // `threads` が直前に返した case_id に詳細が無い: 一覧と詳細の間で消えた
            // （削除・TTL等）可能性があり、運用者が調査できるよう case_id だけを残す
            // （発話本文は含まれない）。
            tracing::error!(
                project_id = %project.project_id,
                schema = %project.schema,
                case_id = %case_id,
                "verify-deploy: thread_detail check found no turns for a case_id just listed by threads"
            );
            CheckResult::fail("thread_detail", json!({}))
        }
        Err(err) => {
            tracing::error!(
                project_id = %project.project_id,
                schema = %project.schema,
                case_id = %case_id,
                error = %format!("{err:#}"),
                "verify-deploy: thread_detail check failed"
            );
            CheckResult::fail("thread_detail", json!({}))
        }
    }
}

async fn run_stats_check(admin_state: &AdminState, project_id: &str, schema: &str) -> CheckResult {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(admin::STATS_DEFAULT_DAYS as i64))
        .to_rfc3339();
    let store = match admin_state.harness.store() {
        Ok(store) => store,
        Err(err) => {
            tracing::error!(
                project_id = %project_id,
                schema = %schema,
                error = %format!("{err:#}"),
                "verify-deploy: stats check knowledge store unavailable"
            );
            return CheckResult::fail("stats", json!({}));
        }
    };
    match store.load_conversation_turns_since(schema, &cutoff).await {
        Ok(attrs_list) => {
            // `stats_summary` ハンドラと同じ変換（`TurnRow::from_attrs` → `summarize_turns`）を
            // 通す（spec §3.1: 同じ関数を使う）。集計結果そのものは検査に使わない
            // （合否は「エラーにならないか」だけ、spec §3.1 の表）。
            let rows: Vec<admin::TurnRow> = attrs_list
                .iter()
                .filter_map(admin::TurnRow::from_attrs)
                .collect();
            let _summary = admin::summarize_turns(admin::STATS_DEFAULT_DAYS, &rows);
            CheckResult::pass("stats", json!({}))
        }
        Err(err) => {
            tracing::error!(
                project_id = %project_id,
                schema = %schema,
                error = %format!("{err:#}"),
                "verify-deploy: stats check failed"
            );
            CheckResult::fail("stats", json!({}))
        }
    }
}

// ---------------------------------------------------------------------------
// 第1層判定の検査（spec §3.2）
// ---------------------------------------------------------------------------

/// 期待値ファイル 1 件分を判定し、記録を組み立てる純関数。`normalizer` は lexicon だけで
/// signal を立てる（LLM を使わない、spec §3.2 の 3.）。
fn evaluate_layer1_case(
    rules: &[EscalationRule],
    normalizer: &dyn SignalNormalizer,
    case: &Layer1Case,
) -> Layer1Result {
    let signals = normalizer.normalize(&case.utterance);
    let matched_rule = match_layer1(rules, &signals).map(|rule| rule.id.clone());
    let status = if matched_rule == case.expect_rule {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail
    };
    Layer1Result {
        utterance: case.utterance.clone(),
        signals: signals.iter().map(|s| s.as_str().to_string()).collect(),
        matched_rule,
        expect_rule: case.expect_rule.clone(),
        status,
    }
}

/// `layer1` 検査全体の `CheckResult` を組み立てる純関数（spec §4: `detail` は `total` /
/// `failed` の件数のみ）。
fn summarize_layer1_check(results: &[Layer1Result]) -> CheckResult {
    let failed = results
        .iter()
        .filter(|r| r.status == CheckStatus::Fail)
        .count();
    let status = if failed > 0 {
        CheckStatus::Fail
    } else {
        CheckStatus::Pass
    };
    CheckResult {
        name: "layer1".to_string(),
        status,
        detail: json!({ "total": results.len(), "failed": failed }),
    }
}

async fn run_layer1_check(
    harness: &Harness,
    project_id: &str,
    schema: &str,
    cases: &[Layer1Case],
) -> (CheckResult, Vec<Layer1Result>) {
    let rules = match harness.store() {
        Ok(store) => store.load_escalation_rules(schema).await,
        Err(err) => Err(err),
    };
    match rules {
        Ok(rules) => {
            let results: Vec<Layer1Result> = cases
                .iter()
                .map(|case| evaluate_layer1_case(&rules, harness.lexicon.as_ref(), case))
                .collect();
            (summarize_layer1_check(&results), results)
        }
        Err(err) => {
            tracing::error!(
                project_id = %project_id,
                schema = %schema,
                error = %format!("{err:#}"),
                "verify-deploy: layer1 check failed to load escalation rules"
            );
            (CheckResult::fail("layer1", json!({})), Vec::new())
        }
    }
}

// ---------------------------------------------------------------------------
// project 単位のオーケストレーション
// ---------------------------------------------------------------------------

async fn run_project_checks(
    harness: &Arc<Harness>,
    config_path: &str,
    project: &ProjectConfig,
    layer1_cases: Option<&[Layer1Case]>,
) -> ProjectReport {
    let admin_state = AdminState {
        schema: project.schema.clone(),
        manual_schema: project.manual_schema,
        harness: harness.clone(),
    };

    let mut checks = Vec::new();

    let threads_result =
        admin::fetch_thread_page(&admin_state, None, admin::THREADS_DEFAULT_LIMIT, 0).await;
    let detail_plan = match &threads_result {
        Ok(page) => {
            checks.push(threads_check_from_page(page));
            plan_thread_detail_check(Some(page))
        }
        Err(err) => {
            tracing::error!(
                project_id = %project.project_id,
                schema = %project.schema,
                error = %format!("{err:#}"),
                "verify-deploy: threads check failed"
            );
            checks.push(CheckResult::fail("threads", json!({})));
            plan_thread_detail_check(None)
        }
    };
    checks.push(run_thread_detail_check(&admin_state, project, detail_plan).await);
    checks.push(run_stats_check(&admin_state, &project.project_id, &project.schema).await);

    let layer1 = match layer1_cases {
        None => Vec::new(),
        Some(cases) => {
            let (check, results) =
                run_layer1_check(harness, &project.project_id, &project.schema, cases).await;
            checks.push(check);
            results
        }
    };

    ProjectReport {
        config: config_path.to_string(),
        project_id: project.project_id.clone(),
        schema: project.schema.clone(),
        checks,
        layer1,
    }
}

/// 全 project の全 check を見て、1 件でも `fail` があれば `false`（spec §4, §7）。
/// `skipped` は不合格にしない。
fn compute_passed(projects: &[ProjectReport]) -> bool {
    !projects
        .iter()
        .any(|p| p.checks.iter().any(|c| c.status == CheckStatus::Fail))
}

/// 実行ごとの一時ディレクトリ。スコープを抜けると（正常終了・`?` による途中エラーのどちらでも）
/// 削除する。`tempfile` crate は `server/Cargo.toml` の依存に無く、この 1 用途のために依存を
/// 足さない。`std::process::exit` を生存中に呼ぶと `Drop` が走らないため、終了コードは
/// `main` の戻り値で返す。
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            // Harness を build する前に終了した場合など、ディレクトリが作られていないのは正常。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "verify-deploy: failed to remove the scratch dir; remove it manually"
            ),
        }
    }
}

/// vegapunk への疎通確認（preflight）の分類結果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PreflightOutcome {
    Reachable,
    Unreachable { message: String },
}

/// preflight の読み出し結果を分類し、失敗時は運用者が次の行動を決められるメッセージを組み立てる
/// 純関数（spec §4）。`read_error` は読み出しのエラーを `{:#}` で整形した文字列。
fn classify_preflight(
    config_path: &str,
    project_id: &str,
    schema: &str,
    endpoint: &str,
    read_error: Option<&str>,
) -> PreflightOutcome {
    let Some(error) = read_error else {
        return PreflightOutcome::Reachable;
    };
    let lowered = error.to_lowercase();
    let hint = if lowered.contains("unauthenticated") || lowered.contains("permission") {
        "vegapunk rejected the credentials: check VEGAPUNK_BEARER_TOKEN / \
         VEGAPUNK_BEARER_TOKEN_FILE and the secret version injected into the job"
    } else if lowered.contains("unavailable")
        || lowered.contains("transport")
        || lowered.contains("connect")
        || lowered.contains("deadline")
    {
        "vegapunk is unreachable: check the endpoint, the job's VPC connector and the vegapunk \
         host's firewall"
    } else {
        "unexpected vegapunk error: check the vegapunk server logs and that the schema exists"
    };
    PreflightOutcome::Unreachable {
        message: format!(
            "verify-deploy: preflight read (ConversationTurn, 1 row) failed for project \
             {project_id:?} of config {config_path:?} (schema {schema:?}, endpoint {endpoint}): \
             {error}. {hint}. No result JSON is emitted because this is an infrastructure/config \
             failure, not a smoke failure"
        ),
    }
}

/// preflight に使う読み出し: 管理 API の `threads` 検査が既に使う `ConversationTurn` の
/// 1 件取得（最も軽い読み取り専用 RPC。`Search` と書き込みは使わない）。
async fn preflight_project(
    harness: &Harness,
    config_path: &str,
    project: &ProjectConfig,
    endpoint: &str,
) -> PreflightOutcome {
    let result = match harness.store() {
        Ok(store) => store
            .load_conversation_turns_page(&project.schema, None, 0, 1)
            .await
            .map(|_| ()),
        Err(err) => Err(err),
    };
    let error_text = result.err().map(|err| format!("{err:#}"));
    classify_preflight(
        config_path,
        &project.project_id,
        &project.schema,
        endpoint,
        error_text.as_deref(),
    )
}

/// 設定ファイル 1 つ分の実行環境。Harness・vegapunk の接続先・対象 project を束ねる。
struct ConfigRuntime {
    config_path: String,
    vegapunk_endpoint: String,
    harness: Arc<Harness>,
    projects: Vec<ProjectConfig>,
}

/// 設定ファイル 1 つ分について、読み込み・読み取り専用化・vegapunk 接続・`Harness::build` を
/// 行う（spec §5.1）。`scratch_dir` はこの設定ファイル専用のサブディレクトリ。
async fn build_config_runtime(
    group: &ConfigGroup,
    scratch_dir: &Path,
    token: &str,
) -> Result<ConfigRuntime> {
    let config_path = Path::new(&group.config_path);
    let config = AppConfig::load(config_path)
        .with_context(|| format!("load config {}", group.config_path))?;
    let selected = resolve_projects(&config.projects, &group.project_ids)
        .with_context(|| format!("resolve --target projects in config {}", group.config_path))?;
    let projects: Vec<ProjectConfig> = selected.into_iter().cloned().collect();

    // 即時接続（`connect_with_limits`）。接続後の認証拒否・断は preflight が拾う（spec §4）。
    let vegapunk =
        VegapunkClient::connect_with_limits(&config.vegapunk_endpoint, token, config.grpc_limits())
            .await
            .with_context(|| {
                format!(
                    "connect vegapunk {} (config {})",
                    config.vegapunk_endpoint, group.config_path
                )
            })?;
    let config_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let harness_config = read_only_harness_config(&config, scratch_dir);
    tracing::info!(
        config = %group.config_path,
        scratch_dir = %scratch_dir.display(),
        "verify-deploy: harness built with llm/jev/reply-draft disabled and audit/queue paths redirected to a scratch dir (read-only job)"
    );
    let harness = Arc::new(
        Harness::build(&harness_config, Arc::new(vegapunk), &config_dir)
            .with_context(|| format!("build harness for config {}", group.config_path))?,
    );
    Ok(ConfigRuntime {
        config_path: group.config_path.clone(),
        vegapunk_endpoint: config.vegapunk_endpoint.clone(),
        harness,
        projects,
    })
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    // 標準出力は結果 JSON 専用（spec §4）。ログを混ぜると fail 時に `jq` 等で JSON を
    // 読めなくなるため、tracing は標準エラーへ出す。
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    // 引数誤りの終了コードは 1（spec §4）。clap 既定の 2 を避けるため `try_parse` を使う。
    // `--help` / `--version` は成功終了（`use_stderr() == false`）のまま clap に任せる。
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(err) if !err.use_stderr() => err.exit(),
        Err(err) => {
            let _ = err.print();
            std::process::exit(1);
        }
    };
    run(args).await
}

async fn run(args: Args) -> Result<ExitCode> {
    let targets = validate_args(&args)?;
    let groups = group_targets_by_config(&targets);
    let token = read_bearer_token(&args)?;

    // `runtimes`（Harness を保持し、監査ログファイルを開く）より先に宣言する: ローカル変数は
    // 宣言の逆順に drop されるため、Harness が先に drop され、その後で一時ディレクトリが削除される。
    let scratch = ScratchDir::new(
        std::env::temp_dir().join(format!("verify-deploy-{}", uuid::Uuid::new_v4())),
    );
    let mut runtimes: Vec<ConfigRuntime> = Vec::with_capacity(groups.len());
    for (index, group) in groups.iter().enumerate() {
        let scratch_dir = config_scratch_dir(&scratch.path, index);
        runtimes.push(build_config_runtime(group, &scratch_dir, &token).await?);
    }

    let expectations = match &args.expectations {
        Some(path) => {
            let body = fs::read_to_string(path)
                .with_context(|| format!("read expectations file {}", path.display()))?;
            Some(
                parse_expectations_file(&body)
                    .with_context(|| format!("expectations file {}", path.display()))?,
            )
        }
        None => None,
    };

    // 検査の前に project ごとの疎通確認を行う。1 つでも失敗したら JSON を出さずに終了する
    // （spec §4: 基盤障害・認証設定の誤りを、通常のスモーク不合格と区別するため）。
    let mut unreachable = false;
    for runtime in &runtimes {
        for project in &runtime.projects {
            if let PreflightOutcome::Unreachable { message } = preflight_project(
                &runtime.harness,
                &runtime.config_path,
                project,
                &runtime.vegapunk_endpoint,
            )
            .await
            {
                tracing::error!("{message}");
                unreachable = true;
            }
        }
    }
    if unreachable {
        return Ok(ExitCode::from(1));
    }

    let mut projects = Vec::new();
    for runtime in &runtimes {
        for project in &runtime.projects {
            let layer1_cases =
                if args.expectations_project.as_deref() == Some(project.project_id.as_str()) {
                    expectations.as_ref().map(|e| e.layer1.as_slice())
                } else {
                    None
                };
            projects.push(
                run_project_checks(
                    &runtime.harness,
                    &runtime.config_path,
                    project,
                    layer1_cases,
                )
                .await,
            );
        }
    }

    let passed = compute_passed(&projects);
    let report = Report { passed, projects };
    let json = serde_json::to_string_pretty(&report).context("serialize report")?;
    println!("{json}");
    std::io::stdout()
        .flush()
        .context("flush the result JSON to stdout")?;
    Ok(if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs_support_mcp::harness::{
        knowledge::escalation_rule_from_attributes,
        rules::Binding,
        signal::{LexiconNormalizer, Signal},
    };
    use std::collections::HashMap;

    // ---- read_bearer_token ----

    #[test]
    fn read_bearer_token_trims_and_prefers_the_inline_value() {
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: None,
            expectations_project: None,
            vegapunk_bearer_token: Some("  secret-token  ".to_string()),
            vegapunk_bearer_token_file: None,
        };
        assert_eq!(read_bearer_token(&args).unwrap(), "secret-token");
    }

    #[test]
    fn read_bearer_token_falls_back_to_the_file_when_inline_is_absent() {
        let dir = std::env::temp_dir().join(format!("verify-deploy-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "file-token\n").unwrap();
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: None,
            expectations_project: None,
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: Some(path),
        };
        assert_eq!(read_bearer_token(&args).unwrap(), "file-token");
    }

    #[test]
    fn read_bearer_token_rejects_an_empty_file() {
        let dir = std::env::temp_dir().join(format!("verify-deploy-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "   \n").unwrap();
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: None,
            expectations_project: None,
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: Some(path),
        };
        let err = read_bearer_token(&args).unwrap_err();
        assert!(err.to_string().contains("is empty"));
    }

    #[test]
    fn read_bearer_token_fails_closed_when_neither_is_set() {
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: None,
            expectations_project: None,
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: None,
        };
        let err = read_bearer_token(&args).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ---- validate_args ----

    #[test]
    fn validate_args_allows_neither_expectations_flag() {
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: None,
            expectations_project: None,
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: None,
        };
        assert!(validate_args(&args).is_ok());
    }

    #[test]
    fn validate_args_allows_both_expectations_flags_together() {
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: Some(PathBuf::from("expectations.json")),
            expectations_project: Some("urtect".to_string()),
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: None,
        };
        assert!(validate_args(&args).is_ok());
    }

    #[test]
    fn validate_args_rejects_expectations_without_expectations_project() {
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: Some(PathBuf::from("expectations.json")),
            expectations_project: None,
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: None,
        };
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("must be given together"));
    }

    #[test]
    fn validate_args_rejects_expectations_project_without_expectations() {
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: None,
            expectations_project: Some("urtect".to_string()),
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: None,
        };
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("must be given together"));
    }

    #[test]
    fn validate_args_rejects_expectations_project_not_in_project_list() {
        let args = Args {
            target: vec!["config.toml:urtect".to_string()],
            expectations: Some(PathBuf::from("expectations.json")),
            expectations_project: Some("homesec".to_string()),
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: None,
        };
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("must be one of the --target"));
    }

    // ---- parse_target / parse_targets / group_targets_by_config ----

    #[test]
    fn parse_target_splits_config_path_and_project_id() {
        let target = parse_target("/app/server/config.cloudrun.toml:urtect").unwrap();
        assert_eq!(target.config_path, "/app/server/config.cloudrun.toml");
        assert_eq!(target.project_id, "urtect");
    }

    #[test]
    fn parse_target_splits_on_the_last_colon_when_the_path_contains_one() {
        let target = parse_target("/odd:dir/config.toml:homesec").unwrap();
        assert_eq!(target.config_path, "/odd:dir/config.toml");
        assert_eq!(target.project_id, "homesec");
    }

    #[test]
    fn parse_target_rejects_a_value_without_colon() {
        let err = parse_target("config.toml").unwrap_err();
        assert!(err.to_string().contains("no ':' found"));
    }

    #[test]
    fn parse_target_rejects_an_empty_project_id() {
        let err = parse_target("config.toml:").unwrap_err();
        assert!(err.to_string().contains("empty project_id"));
    }

    #[test]
    fn parse_target_rejects_an_empty_config_path() {
        let err = parse_target(":urtect").unwrap_err();
        assert!(err.to_string().contains("empty config path"));
    }

    #[test]
    fn parse_targets_rejects_the_same_project_id_in_two_targets() {
        let raw = vec!["a.toml:urtect".to_string(), "b.toml:urtect".to_string()];
        let err = parse_targets(&raw).unwrap_err();
        assert!(err.to_string().contains("more than one --target"));
    }

    #[test]
    fn parse_targets_allows_the_same_config_for_different_projects() {
        let raw = vec!["a.toml:urtect".to_string(), "a.toml:homesec".to_string()];
        assert_eq!(parse_targets(&raw).unwrap().len(), 2);
    }

    #[test]
    fn group_targets_by_config_merges_same_config_and_keeps_first_seen_order() {
        let raw = vec![
            "b.toml:homesec".to_string(),
            "a.toml:urtect".to_string(),
            "b.toml:other".to_string(),
        ];
        let groups = group_targets_by_config(&parse_targets(&raw).unwrap());
        assert_eq!(
            groups,
            vec![
                ConfigGroup {
                    config_path: "b.toml".to_string(),
                    project_ids: vec!["homesec".to_string(), "other".to_string()],
                },
                ConfigGroup {
                    config_path: "a.toml".to_string(),
                    project_ids: vec!["urtect".to_string()],
                },
            ]
        );
    }

    #[test]
    fn config_scratch_dir_gives_each_config_its_own_subdirectory_under_the_root() {
        let root = Path::new("/scratch/verify-deploy-x");
        let first = config_scratch_dir(root, 0);
        let second = config_scratch_dir(root, 1);
        assert_ne!(first, second);
        assert!(first.starts_with(root) && second.starts_with(root));
    }

    // ---- resolve_projects ----

    fn project(project_id: &str, schema: &str) -> ProjectConfig {
        ProjectConfig {
            project_id: project_id.to_string(),
            schema: schema.to_string(),
            bearer_token: None,
            manual_schema: ManualSchemaKind::ManualV1,
        }
    }

    #[test]
    fn resolve_projects_finds_every_requested_project() {
        let projects = vec![project("urtect", "urtect"), project("homesec", "homesec")];
        let resolved =
            resolve_projects(&projects, &["homesec".to_string(), "urtect".to_string()]).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].project_id, "homesec");
        assert_eq!(resolved[1].project_id, "urtect");
    }

    #[test]
    fn resolve_projects_rejects_an_unconfigured_project_id() {
        let projects = vec![project("urtect", "urtect")];
        let err = resolve_projects(&projects, &["sivira-cs-demo".to_string()]).unwrap_err();
        assert!(err.to_string().contains("sivira-cs-demo"));
    }

    // ---- parse_expectations_file ----

    #[test]
    fn parse_expectations_file_reads_valid_json() {
        let body = r#"{"layer1":[{"utterance":"初期費用はいくらですか？","expect_rule":"initial-cost-quote"},{"utterance":"月額いくらですか？","expect_rule":null}]}"#;
        let file = parse_expectations_file(body).unwrap();
        assert_eq!(file.layer1.len(), 2);
        assert_eq!(file.layer1[0].utterance, "初期費用はいくらですか？");
        assert_eq!(
            file.layer1[0].expect_rule.as_deref(),
            Some("initial-cost-quote")
        );
        assert_eq!(file.layer1[1].expect_rule, None);
    }

    #[test]
    fn parse_expectations_file_rejects_unknown_fields() {
        let body = r#"{"layer1":[{"utterance":"x","expect_rule":null,"unexpected":true}]}"#;
        let err = parse_expectations_file(body).unwrap_err();
        assert!(err.to_string().contains("unexpected") || format!("{err:#}").contains("unknown"));
    }

    #[test]
    fn parse_expectations_file_rejects_an_empty_layer1() {
        let body = r#"{"layer1":[]}"#;
        let err = parse_expectations_file(body).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    // ---- plan_thread_detail_check ----

    fn thread_summary(case_id: &str) -> admin::ThreadSummary {
        admin::ThreadSummary {
            case_id: case_id.to_string(),
            case_ref: format!("ref-{case_id}"),
            end_user_id: None,
            question_preview: "q".to_string(),
            turn_count: 1,
            last_reply_kind: "answer".to_string(),
            last_created_at: "2026-01-01T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn plan_thread_detail_check_probes_the_first_thread_when_present() {
        let page = admin::ThreadsPage {
            threads: vec![thread_summary("case-1"), thread_summary("case-2")],
            next_cursor: None,
        };
        assert_eq!(
            plan_thread_detail_check(Some(&page)),
            ThreadDetailPlan::Probe("case-1".to_string())
        );
    }

    #[test]
    fn plan_thread_detail_check_skips_when_threads_page_is_empty() {
        let page = admin::ThreadsPage {
            threads: vec![],
            next_cursor: None,
        };
        assert_eq!(
            plan_thread_detail_check(Some(&page)),
            ThreadDetailPlan::Skip
        );
    }

    #[test]
    fn plan_thread_detail_check_skips_when_threads_check_failed() {
        assert_eq!(plan_thread_detail_check(None), ThreadDetailPlan::Skip);
    }

    // ---- threads_check_from_page / thread_detail_check_from_turn_count: 出力に顧客発話を
    // 含まないことの固定（spec §7「出力」） ----

    #[test]
    fn threads_check_detail_contains_only_the_count_not_customer_content() {
        let mut summary = thread_summary("case-1");
        summary.question_preview = "顧客の質問本文".to_string();
        summary.end_user_id = Some("customer-secret-id".to_string());
        let page = admin::ThreadsPage {
            threads: vec![summary],
            next_cursor: None,
        };
        let check = threads_check_from_page(&page);
        assert_eq!(check.detail, json!({ "count": 1 }));
        let serialized = serde_json::to_string(&check).unwrap();
        assert!(!serialized.contains("顧客の質問本文"));
        assert!(!serialized.contains("customer-secret-id"));
    }

    #[test]
    fn thread_detail_check_detail_contains_only_the_turn_count() {
        let check = thread_detail_check_from_turn_count(6);
        assert_eq!(check.detail, json!({ "turns": 6 }));
        assert_eq!(check.status, CheckStatus::Pass);
    }

    // ---- evaluate_layer1_case / summarize_layer1_check ----

    fn lexicon_with_one_signal() -> LexiconNormalizer {
        LexiconNormalizer::from_json(
            r#"{"signals":[{"signal":"foo_signal","class":"context","surface_forms":["ふー"]}]}"#,
        )
        .unwrap()
    }

    fn rule(id: &str, condition: &[&str], binding: Binding) -> EscalationRule {
        EscalationRule {
            id: id.to_string(),
            condition: condition.iter().map(|s| Signal::new(*s)).collect(),
            route: "support_desk".to_string(),
            owner: None,
            binding,
            hearing: None,
            customer_ack: None,
        }
    }

    #[test]
    fn evaluate_layer1_case_passes_when_matched_rule_equals_expectation() {
        let lexicon = lexicon_with_one_signal();
        let rules = vec![rule("r1", &["foo_signal"], Binding::Advisory)];
        let case = Layer1Case {
            utterance: "ふーですか".to_string(),
            expect_rule: Some("r1".to_string()),
        };
        let result = evaluate_layer1_case(&rules, &lexicon, &case);
        assert_eq!(result.status, CheckStatus::Pass);
        assert_eq!(result.matched_rule.as_deref(), Some("r1"));
        assert_eq!(result.signals, vec!["foo_signal".to_string()]);
    }

    #[test]
    fn evaluate_layer1_case_passes_when_no_rule_matches_and_none_is_expected() {
        let lexicon = lexicon_with_one_signal();
        let rules = vec![rule("r1", &["foo_signal"], Binding::Advisory)];
        let case = Layer1Case {
            utterance: "関係ない発話".to_string(),
            expect_rule: None,
        };
        let result = evaluate_layer1_case(&rules, &lexicon, &case);
        assert_eq!(result.status, CheckStatus::Pass);
        assert_eq!(result.matched_rule, None);
    }

    #[test]
    fn evaluate_layer1_case_fails_when_a_different_rule_matches() {
        let lexicon = lexicon_with_one_signal();
        let rules = vec![rule("r1", &["foo_signal"], Binding::Advisory)];
        let case = Layer1Case {
            utterance: "ふーですか".to_string(),
            expect_rule: Some("r2".to_string()),
        };
        let result = evaluate_layer1_case(&rules, &lexicon, &case);
        assert_eq!(result.status, CheckStatus::Fail);
        assert_eq!(result.matched_rule.as_deref(), Some("r1"));
    }

    #[test]
    fn evaluate_layer1_case_fails_when_a_match_was_not_expected() {
        let lexicon = lexicon_with_one_signal();
        let rules = vec![rule("r1", &["foo_signal"], Binding::Advisory)];
        let case = Layer1Case {
            utterance: "ふーですか".to_string(),
            expect_rule: None,
        };
        let result = evaluate_layer1_case(&rules, &lexicon, &case);
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn evaluate_layer1_case_fails_when_an_expected_match_did_not_happen() {
        let lexicon = lexicon_with_one_signal();
        let rules = vec![rule("r1", &["foo_signal"], Binding::Advisory)];
        let case = Layer1Case {
            utterance: "関係ない発話".to_string(),
            expect_rule: Some("r1".to_string()),
        };
        let result = evaluate_layer1_case(&rules, &lexicon, &case);
        assert_eq!(result.status, CheckStatus::Fail);
        assert_eq!(result.matched_rule, None);
    }

    #[test]
    fn summarize_layer1_check_passes_when_all_cases_pass() {
        let results = vec![
            Layer1Result {
                utterance: "a".to_string(),
                signals: vec![],
                matched_rule: None,
                expect_rule: None,
                status: CheckStatus::Pass,
            },
            Layer1Result {
                utterance: "b".to_string(),
                signals: vec![],
                matched_rule: None,
                expect_rule: None,
                status: CheckStatus::Pass,
            },
        ];
        let check = summarize_layer1_check(&results);
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.detail, json!({ "total": 2, "failed": 0 }));
    }

    #[test]
    fn summarize_layer1_check_fails_when_any_case_fails() {
        let results = vec![
            Layer1Result {
                utterance: "a".to_string(),
                signals: vec![],
                matched_rule: None,
                expect_rule: None,
                status: CheckStatus::Pass,
            },
            Layer1Result {
                utterance: "b".to_string(),
                signals: vec![],
                matched_rule: Some("r1".to_string()),
                expect_rule: None,
                status: CheckStatus::Fail,
            },
        ];
        let check = summarize_layer1_check(&results);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.detail, json!({ "total": 2, "failed": 1 }));
    }

    // ---- compute_passed（結果の集約） ----

    fn project_report(checks: Vec<CheckResult>) -> ProjectReport {
        ProjectReport {
            config: "config.toml".to_string(),
            project_id: "urtect".to_string(),
            schema: "urtect".to_string(),
            checks,
            layer1: Vec::new(),
        }
    }

    #[test]
    fn compute_passed_is_true_when_every_check_passes_or_is_skipped() {
        let projects = vec![project_report(vec![
            CheckResult::pass("threads", json!({})),
            CheckResult::skipped("thread_detail", json!({})),
        ])];
        assert!(compute_passed(&projects));
    }

    #[test]
    fn compute_passed_is_false_when_any_check_in_any_project_fails() {
        let projects = vec![
            project_report(vec![CheckResult::pass("threads", json!({}))]),
            project_report(vec![CheckResult::fail("stats", json!({}))]),
        ];
        assert!(!compute_passed(&projects));
    }

    // ---- classify_preflight ----

    #[test]
    fn classify_preflight_is_reachable_when_the_read_succeeded() {
        assert_eq!(
            classify_preflight("config.toml", "urtect", "urtect", "http://vp:6840", None),
            PreflightOutcome::Reachable
        );
    }

    fn unreachable_message(error: &str) -> String {
        match classify_preflight(
            "config.toml",
            "urtect",
            "urtect-schema",
            "http://vp:6840",
            Some(error),
        ) {
            PreflightOutcome::Unreachable { message } => message,
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[test]
    fn classify_preflight_names_config_project_schema_endpoint_and_error() {
        let message = unreachable_message("load conversation turns page: boom");
        assert!(message.contains("\"config.toml\""));
        assert!(message.contains("\"urtect\""));
        assert!(message.contains("\"urtect-schema\""));
        assert!(message.contains("http://vp:6840"));
        assert!(message.contains("boom"));
    }

    #[test]
    fn classify_preflight_points_to_credentials_on_unauthenticated() {
        let message = unreachable_message("status: Unauthenticated, message: invalid token");
        assert!(message.contains("VEGAPUNK_BEARER_TOKEN"));
    }

    #[test]
    fn classify_preflight_points_to_network_on_unavailable() {
        let message = unreachable_message("status: Unavailable, message: transport error");
        assert!(message.contains("VPC connector"));
    }

    #[test]
    fn classify_preflight_falls_back_to_a_generic_hint_for_other_errors() {
        let message = unreachable_message("status: NotFound, message: schema missing");
        assert!(message.contains("schema exists"));
    }

    // ---- ScratchDir ----

    #[test]
    fn scratch_dir_is_removed_when_it_goes_out_of_scope() {
        let path =
            std::env::temp_dir().join(format!("verify-deploy-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(path.join("audit")).unwrap();
        std::fs::write(path.join("audit").join("audit.jsonl"), "x").unwrap();
        {
            let _guard = ScratchDir::new(path.clone());
            assert!(path.exists());
        }
        assert!(!path.exists());
    }

    #[test]
    fn scratch_dir_drop_tolerates_a_directory_that_was_never_created() {
        let path =
            std::env::temp_dir().join(format!("verify-deploy-test-{}", uuid::Uuid::new_v4()));
        drop(ScratchDir::new(path.clone()));
        assert!(!path.exists());
    }

    // ---- read_only_harness_config ----

    fn cloudrun_config() -> AppConfig {
        AppConfig::load(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/config.cloudrun.toml"
        )))
        .expect("config.cloudrun.toml must load")
    }

    #[test]
    fn read_only_harness_config_disables_llm_jev_and_reply_draft() {
        let config = cloudrun_config();
        assert!(config.llm.enabled && config.jev.enabled, "precondition");
        let scratch = Path::new("/scratch/verify-deploy-x");
        let overridden = read_only_harness_config(&config, scratch);
        assert!(!overridden.llm.enabled);
        assert!(!overridden.jev.enabled);
        assert!(!overridden.harness.customer_reply_draft_enabled);
    }

    #[test]
    fn read_only_harness_config_moves_write_paths_under_the_scratch_dir() {
        let config = cloudrun_config();
        let scratch = Path::new("/scratch/verify-deploy-x");
        let overridden = read_only_harness_config(&config, scratch);
        assert!(Path::new(&overridden.harness.audit_log_path).starts_with(scratch));
        assert!(Path::new(&overridden.harness.search_improvement_queue_path).starts_with(scratch));
    }

    fn homesec_config() -> AppConfig {
        AppConfig::load(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/config.homesec.toml"
        )))
        .expect("config.homesec.toml must load")
    }

    /// 本番の両設定で、`Harness::build` が開く書き込み先（監査ログ・検索改善キュー）が
    /// すべて一時ディレクトリの下になり、外部サービス（LLM・Jev・返信文下書き）が無効になる。
    /// `[advisor] support_*` の書き込み先は `bin/homesec_advisor.rs` だけが読み、この CLI の
    /// 経路（`Harness::build(&config.harness ...)`）では開かれないため上書き対象にしない。
    #[test]
    fn read_only_harness_config_confines_writes_and_disables_externals_for_both_configs() {
        let scratch = Path::new("/scratch/verify-deploy-x");
        for (name, config) in [
            ("config.cloudrun.toml", cloudrun_config()),
            ("config.homesec.toml", homesec_config()),
        ] {
            let overridden = read_only_harness_config(&config, scratch);
            assert!(
                Path::new(&overridden.harness.audit_log_path).starts_with(scratch),
                "{name}: audit_log_path"
            );
            assert!(
                Path::new(&overridden.harness.search_improvement_queue_path).starts_with(scratch),
                "{name}: search_improvement_queue_path"
            );
            assert!(!overridden.llm.enabled, "{name}: llm");
            assert!(!overridden.jev.enabled, "{name}: jev");
            assert!(
                !overridden.harness.customer_reply_draft_enabled,
                "{name}: reply draft"
            );
        }
    }

    #[test]
    fn per_config_scratch_dirs_keep_the_two_configs_write_paths_apart() {
        let root = Path::new("/scratch/verify-deploy-x");
        let cloudrun = read_only_harness_config(&cloudrun_config(), &config_scratch_dir(root, 0));
        let homesec = read_only_harness_config(&homesec_config(), &config_scratch_dir(root, 1));
        assert_ne!(
            cloudrun.harness.audit_log_path,
            homesec.harness.audit_log_path
        );
        assert_ne!(
            cloudrun.harness.search_improvement_queue_path,
            homesec.harness.search_improvement_queue_path
        );
    }

    #[test]
    fn resolve_projects_finds_homesec_in_its_own_config_but_not_in_cloudrun() {
        let homesec = homesec_config();
        assert!(resolve_projects(&homesec.projects, &["homesec".to_string()]).is_ok());
        let cloudrun = cloudrun_config();
        let err = resolve_projects(&cloudrun.projects, &["homesec".to_string()]).unwrap_err();
        assert!(err.to_string().contains("homesec"));
    }

    #[test]
    fn read_only_harness_config_keeps_lexicon_and_projects_untouched() {
        let config = cloudrun_config();
        let overridden = read_only_harness_config(&config, Path::new("/scratch/x"));
        assert_eq!(
            overridden.harness.signal_lexicon_path,
            config.harness.signal_lexicon_path
        );
        assert_eq!(
            overridden.harness.ng_dictionary_path,
            config.harness.ng_dictionary_path
        );
        assert_eq!(overridden.vegapunk_endpoint, config.vegapunk_endpoint);
        assert_eq!(overridden.projects.len(), config.projects.len());
        assert_eq!(overridden.projects[0].schema, config.projects[0].schema);
    }

    // ---- 同梱の期待値ファイル（spec §7）: smoke-expectations.json を、同梱 lexicon と
    // 同梱 rules.json から復元したルールで判定すると、全件が期待どおりになる ----

    /// `server/data/urtect/rules.json` の最小限の deserialize 構造体。
    /// `ingest_rules.rs` の `RulesFile` / `RuleInput` は private なので再利用できず、
    /// ここに複製する（`verify_alarmcom.rs` の `read_token` 複製と同じパターン）。
    /// `deny_unknown_fields` は付けない: 本番投入時の検証（綴りミス拒否等）は
    /// `ingest_rules` が既に担っており、ここは素直にパースできればよい。
    #[derive(Debug, Deserialize)]
    struct BundledRulesFile {
        escalation_rules: Vec<BundledRuleInput>,
    }

    #[derive(Debug, Deserialize)]
    struct BundledRuleInput {
        rule_id: String,
        condition: Vec<String>,
        #[serde(default)]
        owner: Option<String>,
        route: String,
        #[serde(default)]
        binding: Option<String>,
        #[serde(default)]
        hearing: Option<String>,
        #[serde(default)]
        customer_ack: Option<String>,
    }

    /// vegapunk の `EscalationRule` ノード属性と同じキーの `HashMap` を組み立てる。
    /// `escalation_rule_from_attributes`（実行時のローダ、`knowledge::parse_binding` 等の
    /// fail-back を含む）にそのまま渡せる形にする。
    fn bundled_rule_attrs(rule: &BundledRuleInput) -> HashMap<String, String> {
        let mut attrs = HashMap::new();
        attrs.insert("rule_id".to_string(), rule.rule_id.clone());
        attrs.insert("condition".to_string(), rule.condition.join(","));
        attrs.insert("route".to_string(), rule.route.clone());
        if let Some(owner) = &rule.owner {
            attrs.insert("owner".to_string(), owner.clone());
        }
        if let Some(binding) = &rule.binding {
            attrs.insert("binding".to_string(), binding.clone());
        }
        if let Some(hearing) = &rule.hearing {
            attrs.insert("hearing".to_string(), hearing.clone());
        }
        if let Some(customer_ack) = &rule.customer_ack {
            attrs.insert("customer_ack".to_string(), customer_ack.clone());
        }
        attrs
    }

    /// `rules.json` 本文から、実行時のローダと同じ関数（`escalation_rule_from_attributes`）で
    /// `Vec<EscalationRule>` を復元する。判定ロジック（`match_layer1`）は複製しない
    /// （spec §5 の不変条件）。
    fn load_bundled_escalation_rules(rules_json: &str) -> Result<Vec<EscalationRule>> {
        let file: BundledRulesFile =
            serde_json::from_str(rules_json).context("parse bundled rules.json")?;
        file.escalation_rules
            .iter()
            .map(|r| escalation_rule_from_attributes(&bundled_rule_attrs(r)))
            .collect()
    }

    const BUNDLED_SMOKE_EXPECTATIONS: &str =
        include_str!("../../data/urtect/smoke-expectations.json");
    const BUNDLED_RULES_JSON: &str = include_str!("../../data/urtect/rules.json");
    const BUNDLED_SIGNAL_LEXICON: &str = include_str!("../../data/urtect/signal-lexicon.json");

    #[test]
    fn bundled_smoke_expectations_match_the_bundled_lexicon_and_rules() {
        let expectations = parse_expectations_file(BUNDLED_SMOKE_EXPECTATIONS)
            .expect("smoke-expectations.json must parse");
        let lexicon = LexiconNormalizer::from_json(BUNDLED_SIGNAL_LEXICON)
            .expect("signal-lexicon.json must parse");
        let rules = load_bundled_escalation_rules(BUNDLED_RULES_JSON)
            .expect("rules.json escalation_rules must load");

        for case in &expectations.layer1 {
            let result = evaluate_layer1_case(&rules, &lexicon, case);
            assert_eq!(
                result.status,
                CheckStatus::Pass,
                "utterance {:?}: expected {:?}, matched {:?} (signals {:?})",
                case.utterance,
                case.expect_rule,
                result.matched_rule,
                result.signals,
            );
        }
    }
}
