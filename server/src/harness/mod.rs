pub mod audit;
pub mod authn;
pub mod clarify;
pub mod correction;
pub mod decision;
pub mod egress;
pub mod escalation_reply;
pub mod extraction;
pub mod grading;
pub mod hours;
pub mod knowledge;
pub mod product_gate;
pub(crate) mod prompt_input;
pub mod reply;
pub mod rules;
pub mod scope;
pub mod signal;
pub mod time_pref;

use crate::config::AppConfig;
use crate::mcp::ToolService;
use crate::model::SectionHit;
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// evaluate 経路の manual 検索 top_k（vector_hits / search_with_snapshot の両方で使う）。
/// tool handler 側の `unwrap_or(5)`（リクエストの既定値）とは別物で、対象外。
const EVALUATE_TOP_K: usize = 5;

/// AuthN → (A) scope → 取得 → 正規化 → 会話層 → (B) 3 層判定 → 記録 を束ねる本体。
/// tool handler はここを経由し、判定ロジックを直書きしない（S1-0 三原則 1）。
pub struct Harness {
    pub authenticator: authn::Authenticator,
    /// admission 検証（admit_known_resolution / validate_rule_vocabulary）専用の
    /// 決定論 lexicon 直参照。signal 抽出そのものは `extractor` を使う（S1-11 改訂）。
    pub normalizer: Arc<dyn signal::SignalNormalizer>,
    pub lexicon: Arc<signal::LexiconNormalizer>,
    /// signal 抽出の入口（lexicon ∪ LLM のハイブリッド、LLM 不達時は lexicon フォールバック）。
    /// `evaluate` / `root_cause_probe` はここ経由で signal を得る（S1-11 改訂）。
    pub extractor: Arc<dyn extraction::AsyncSignalExtractor>,
    pub ng: egress::NgDictionary,
    pub worm: Arc<audit::WormAuditLog>,
    pub knowledge: Option<knowledge::KnowledgeStore>,
    pub thresholds: decision::Thresholds,
    pub grading: grading::GradingThresholds,
    pub queue_path: PathBuf,
    /// grade 更新（read-modify-write）のプロセス内直列化。単一インスタンス運用が前提。
    // TODO: bind to vegapunk atomic increment/CAS — backend 側の原子更新が使えるようになったら置き換える。
    pub grade_lock: tokio::sync::Mutex<()>,
    /// manual_v1 スキーマ向けの manual 取得。project.manual_schema が LegacySection のみの
    /// 構成では未使用（None でも動く）。
    pub manual: Option<crate::manual::retrieval::ManualStore>,
    /// 材料 corpus の共有ローダ。`evaluate`（ManualV1）が manual_corpus / live_corpus を、
    /// ManualStore が manual_corpus を、同一インスタンス経由で使い TTL キャッシュを共有する。
    /// LegacySection 専用構成（テスト含む）では未使用（None でも動く）。
    pub corpus: Option<Arc<crate::corpus::CorpusLoader>>,
    /// 第3層エスカレーションの既定 route（config.harness.default_escalation_route）。
    pub default_route: String,
    /// 意味検索（ベクトル経路）を manual retrieval に合成するか
    /// （config.harness.vector_route_enabled、urtect design §2.3）。
    pub vector_route_enabled: bool,
    /// 顧客向け返信文の**下書き**生成に使う LLM（デモ用）。
    /// `harness.customer_reply_draft_enabled = false`（既定）なら `None` で、
    /// `evaluate` は下書きを作らない。詳細は `harness::reply` の doc を参照。
    pub reply_drafter: Option<crate::llm::AnthropicClient>,
    /// 返信文下書きの `max_tokens`（config.harness.customer_reply_draft_max_tokens）。
    pub reply_draft_max_tokens: u32,
    /// 取扱製品スコープ（Issue #28）。vegapunk の Product ノード一覧を TTL 10 分でキャッシュし、
    /// 質問側ゲート（`api.rs`）・材料選別・プロンプト注入・応答側ゲートの前提として使う。
    /// `manual` / `corpus` と同じ理由（VegapunkClient を要求するため、実接続を張れない同期
    /// テストの `harness_for_test()` では構築できない）で `Option` にしてある。本番は
    /// `Harness::build` が常に `Some` を設定する。
    pub product_gate: Option<product_gate::ProductGate>,
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub actor: authn::Actor,
    pub scope: scope::AccessScope,
    pub schema: String,
    pub request_id: String,
    /// project 設定から解決した manual スキーマ種別。evaluate の manual 取得経路を分岐する。
    pub manual_schema: crate::config::ManualSchemaKind,
}

pub struct EvaluationOutcome {
    pub decision: decision::AnswerDecision,
    /// 今ターンで抽出した signal
    pub signals: signal::SignalSet,
    /// 判定に使った累積 signal 集合（会話層。判定根拠は常にこちら）
    pub accumulated_signals: signal::SignalSet,
    /// 会話の継続キー。新規作成時は採番して返す
    pub case_id: String,
    /// 聞き返し可否（第3層グレーのみ true。第1・2層は問答無用でルーティング）
    pub clarification_allowed: bool,
    pub hits: Vec<SectionHit>,
    pub audit_event_id: String,
    /// S1-1 取得段: 参考として返す類似の過去事例（自 case は除外）。
    /// あくまで client 向けの参考情報であり、3 層判定（decide）の入力には使わない
    /// （判定材料は KR/manual のみという定義を変えない）。
    pub related_cases: Vec<RelatedCase>,
    /// 今ターンの signal 抽出がどの経路を通ったか（S1-11 改訂・WORM 監査にも記録済み）。
    pub extraction_mode: extraction::ExtractionMode,
    /// 顧客向け返信文の**下書き**（デモ用シミュレーション出力）。
    ///
    /// `harness.customer_reply_draft_enabled = false`（既定）、LLM 未設定、生成失敗のいずれでも
    /// `None`。**権威ある回答ではない**（文面の正本は client 側という spec の結論は不変）。
    pub customer_reply_draft: Option<String>,
    /// 上記の下書きが `max_tokens` で**途中で切れている**か。
    ///
    /// 切れ目がたまたま「。」の直後に落ちると下書きは完成文に見えるため、これを client へ
    /// 伝えないと、**末尾の注意書きだけが落ちた案内**がそのまま顧客へ送られうる。
    /// 下書きが無いとき（`customer_reply_draft` が `None`）は常に `false`。
    pub customer_reply_draft_truncated: bool,
}

/// 参考情報として返す過去事例の最小ビュー（S1-1 取得段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedCase {
    pub case_id: String,
    pub question: String,
    pub last_decision: String,
}

/// support_case ノードに永続化する会話状態（会話フロー v1.1 design doc §6）。
/// すべて加算属性・後方互換（欠落は既定値）。
#[derive(Debug, Clone, PartialEq)]
pub struct CaseConvState {
    /// 聞き返し回数。エスカレーション応答送信時に 0 へリセットする（design doc §3）。
    pub clarify_turns: u32,
    /// 次の顧客発話を希望時間帯の返信として解釈するか（design doc §5）。
    pub awaiting_time_pref: bool,
    /// `awaiting_time_pref` 中に `is_time_preference = false` と分類された連続回数
    /// （2 回連続で自動解除、design doc §5）。
    pub time_pref_false_count: u32,
    /// 担当者への申し送り用の希望時間帯（時間外希望は付記を含む）。
    pub preferred_contact_time: Option<String>,
    /// `awaiting_time_pref` 中に希望時間帯の抽出インフラが失敗（LLM 呼び出しエラー・
    /// 応答 parse 失敗）した連続回数。`time_pref_false_count`（真の分類結果が false だった
    /// 回数）とは別枠で数える。3 回に達したら `awaiting_time_pref = false` かつ
    /// `time_pref_false_count = 0` へ自動解除し、自身も 0 へリセットする（design doc §5）。
    /// この判定自体は `Harness` ではなくオーケストレーション層（`api.rs`）の責務で、
    /// ここは読み書きの器のみを持つ。
    pub time_pref_extraction_error_count: u32,
}

/// support_case の属性 map から [`CaseConvState`] を復元する純関数。
///
/// 欠落・parse 失敗は既定値（0 / false / None）に倒す。数値の parse 失敗を warn しないのは、
/// `knowledge.rs` の `approval_count` / `rejection_count` 読み取りと同じ規律に合わせるため
/// （読み取り側で毎回 warn すると、既存 case（この 4 属性を持たない）を読むたびに warn が出る）。
fn conv_state_from_attrs(attrs: &std::collections::HashMap<String, String>) -> CaseConvState {
    let get = |key: &str| attrs.get(key).map(String::as_str).unwrap_or("");
    CaseConvState {
        clarify_turns: get("clarify_turns").parse().unwrap_or(0),
        awaiting_time_pref: get("awaiting_time_pref") == "true",
        time_pref_false_count: get("time_pref_false_count").parse().unwrap_or(0),
        preferred_contact_time: attrs
            .get("preferred_contact_time")
            .filter(|s| !s.is_empty())
            .cloned(),
        time_pref_extraction_error_count: get("time_pref_extraction_error_count")
            .parse()
            .unwrap_or(0),
    }
}

/// [`CaseConvState`] を support_case の属性 map へ書き戻す全属性を組み立てる純関数。
///
/// read-merge-write: 既存属性（`question` / `actor` 等、この 4 キー以外)を土台に、
/// 会話状態の 4 キーだけを重ねる（`merge_outcome_attributes` と同じ形。vegapunk の
/// `UpsertNodes` は全置換のため、部分送信すると既存属性が消える）。
fn merge_conv_state_attributes(
    existing: &std::collections::HashMap<String, String>,
    state: &CaseConvState,
) -> std::collections::HashMap<String, String> {
    let mut merged = existing.clone();
    merged.insert("clarify_turns".to_string(), state.clarify_turns.to_string());
    merged.insert(
        "awaiting_time_pref".to_string(),
        state.awaiting_time_pref.to_string(),
    );
    merged.insert(
        "time_pref_false_count".to_string(),
        state.time_pref_false_count.to_string(),
    );
    merged.insert(
        "preferred_contact_time".to_string(),
        state.preferred_contact_time.clone().unwrap_or_default(),
    );
    merged.insert(
        "time_pref_extraction_error_count".to_string(),
        state.time_pref_extraction_error_count.to_string(),
    );
    merged
}

/// [`Harness::save_conv_state`] が使う判定を独立させた純関数（テスト容易性のため、
/// 実際の `KnowledgeStore::load_case` 呼び出しから切り離してある）。
///
/// `existing` は `load_case` の結果そのもの。`None`（case 未存在）を許してしまうと、会話状態
/// 4 属性だけを持つ `case_id` 属性なしの support_case ノードを書くことになり、以後
/// `load_case` / `load_cases` のどちらからも二度と見えなくなる（Warning 2 の回帰防止）。
fn require_existing_case_attrs(
    existing: Option<std::collections::HashMap<String, String>>,
    case_id: &str,
    schema: &str,
) -> Result<std::collections::HashMap<String, String>> {
    existing.ok_or_else(|| {
        anyhow!(
            "save_conv_state: case {case_id} not found in schema {schema}; the caller must \
             create the case (Harness::evaluate) before saving conversation state"
        )
    })
}

/// outcome 確定時に answer_attempt へ書き戻す全属性を組み立てる純関数。
///
/// read-merge-write: 既存属性（draft / case_id / known_resolution_id / 起票者の
/// actor・actor_email 等）を土台に、outcome 側の属性を重ねる。
///
/// 承認者は安定 ID（`outcome_actor`）と email（`outcome_actor_email`）の両方を書く。
/// ID だけだと、同じノード上に隣接する起票者の `actor_email` が承認者の email と
/// 誤読される。承認者は昇格・降格を駆動するガバナンス上の主体であり、
/// 「誰が承認したか」を人間が読める形で残す必要がある。
/// なお email は「認証時点の当時の値」であり、同一人物判定には使わない（authn.rs）。
fn merge_outcome_attributes(
    attempt: &std::collections::HashMap<String, String>,
    attempt_id: &str,
    outcome: grading::AnswerOutcome,
    actor: &authn::Actor,
    note: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mut merged = attempt.clone();
    merged.insert("attempt_id".to_string(), attempt_id.to_string());
    merged.insert("outcome".to_string(), outcome.as_str().to_string());
    merged.insert("outcome_actor".to_string(), actor.sub.clone());
    merged.insert("outcome_actor_email".to_string(), actor.email.clone());
    merged.insert(
        "outcome_note".to_string(),
        note.unwrap_or_default().to_string(),
    );
    merged
}

/// 聞き返し可否（決定論）: 第3層グレーのみ。第1・2層は問答無用でルーティング
/// （会話フロー v1.1 design doc §2）。
///
/// **この関数が契約そのもの。** `evaluate()` はこの関数を呼ぶだけで、判定式をインラインに
/// 複製しない。テスト（本ファイル `mod tests`）もこの関数を呼ぶこと。式をテスト側に複製すると、
/// ここを書き換えて `matches!` の条件を変えてもテストが検出できなくなる（Critical 1 の回帰）。
fn clarification_allowed(decision: &decision::AnswerDecision) -> bool {
    matches!(
        decision,
        decision::AnswerDecision::Escalate {
            layer: 3,
            reason: decision::EscalateReason::InsufficientDirectness
                | decision::EscalateReason::UnknownAddedSignal,
            ..
        }
    )
}

/// Issue #28 codex レビュー採用1(Critical): `hits` をカバレッジ判定(`decision::decide`)へ渡す
/// 前に、材料テキスト(title_ja + body_ja + body_en のうち空でないもの全て)が「取扱外型番のみを
/// 言及し、取扱型番の言及が1つも無い」節を除外する。汎用材料(型番言及なし)は通す。
///
/// **この関数を `evaluate()` 内、`best_manual_score` / `best_manual_sections` の算出より前に
/// 呼ぶことが本修正の核心。** 以前は同じ除外判定を `reply::build_reply_brief_with_resolution`
/// （判定確定・下書き生成の直前）でしか行っておらず、材料が全除外されても `decide()` には
/// フィルタ前の `best_manual_score` がそのまま渡っていた。取扱外型番のみを言及する記事が
/// たまたま高スコアで検索に掛かると、判定は `Allowed` のまま確定し、その後ろで下書きの材料が
/// 0 件になるという矛盾（「回答してよい」+「材料なし」）が起きていた。ここで先に除外すれば、
/// 全除外時は `section_hits` が空になり、以降の `best_manual_score` / `best_manual_sections` の
/// 計算・`decide()` が自然にカバレッジ不足として扱う（聞き返し/エスカレーションへ倒れる）。
///
/// `title_ja` / `body_ja` / `body_en` のうち空でないものすべてを半角スペース区切りで連結して
/// から `out_of_scope_material_exclusion` に渡す（検査対象に `title_ja` も含めるのは codex
/// レビュー採用4）。`body_ja` と `body_en` のどちらか一方だけを選ぶ（`Option::or`）実装は、
/// `body_ja = Some("")`（翻訳が `missing` / `stale` の section で実際に起きる。`body_ja` 属性が
/// 空文字のまま `body_en` にだけ本文がある）のとき `Some("")` を有効値として選んでしまい
/// `body_en` を一切検査しない fail-open だった（2026-08-14 修正。回帰テスト
/// `filter_out_of_scope_hits_excludes_when_japanese_body_is_empty_and_english_body_has_only_
/// out_of_scope_model` 参照）。ゲートは「検査漏れゼロ」が最優先の fail-closed 判定なので、
/// 利用可能なテキストは全部見る。`out_of_scope_material_exclusion` は「取扱内型番の言及が1つ
/// でもあれば除外しない」仕様なので、検査対象を広げても取扱内記事を過剰に除外することはない
/// （取扱内の言及も同時に拾えるため）。
///
/// **一方、抜粋生成側（`reply::build_reply_brief_with_resolution` の `.or()`）は `body_ja` /
/// `body_en` のどちらか一方だけを選ぶ実装のまま変更していない**（Issue #28 のスコープ外。LLM へ
/// 渡す材料の選び方を変えるのは別途判断が必要）。ここでの不整合に見える差は意図的なもの:
/// ゲートは「除外すべきか」を判定するために広く検査するが、抜粋生成は「顧客に見せる1つの本文」
/// を選ぶ処理であり目的が異なる。
///
/// 除外時は debug ログ（section_key / 検出型番）を出す。監査ログ（WORM）ではなく debug に
/// 留めるのは、除外そのものが「異常」ではなく検索結果の通常のノイズだから
/// （§3.2 は「除外時は debug ログ」と定める）。
fn filter_out_of_scope_hits(
    hits: Vec<SectionHit>,
    allowlist: &product_gate::ProductAllowlist,
) -> Vec<SectionHit> {
    hits.into_iter()
        .filter(|h| {
            // title_ja は常に含める。body_ja / body_en は Some かつ非空のものだけ追加する
            // （`Option::or` で一方だけ選ぶと `body_ja = Some("")` のとき body_en が一切検査
            // されない fail-open になる。上の doc コメント参照）。
            let combined = [
                Some(h.title_ja.as_str()),
                h.body_ja.as_deref(),
                h.body_en.as_deref(),
            ]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            match allowlist.out_of_scope_material_exclusion(&combined) {
                Some(detected) => {
                    tracing::debug!(
                        section_key = %h.section_key,
                        model = %detected,
                        "excluding manual hit from the coverage decision and reply draft: it \
                         mentions only out-of-scope product model(s) (title_ja/body_ja/body_en) \
                         and no in-scope model"
                    );
                    false
                }
                None => true,
            }
        })
        .collect()
}

/// `Harness::evaluate()` に渡された case_id が既存 case として解決できなかった場合の挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownCaseIdPolicy {
    /// MCP 経路(`evaluate_answerability`)の従来挙動。未知 case_id を `Err` にする。
    /// CS 担当の case_id 打ち間違いを黙って新規 case へ合流させず、即エラーで気づけるようにする。
    Reject,
    /// `/api/reply` 契約限定(design doc §2)。未知 case_id をエラーにせず新規 case として
    /// 処理する。クライアント保持の case_id がサーバ再起動・保存漏れで失効するのは通常運用。
    StartNew,
}

impl Harness {
    pub fn build(
        config: &AppConfig,
        client: Arc<VegapunkClient>,
        config_dir: &Path,
    ) -> Result<Self> {
        let resolve_path = |p: &str| {
            let path = Path::new(p);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                config_dir.join(path)
            }
        };
        let lexicon = Arc::new(signal::LexiconNormalizer::from_path(&resolve_path(
            &config.harness.signal_lexicon_path,
        ))?);
        // LLM signal 抽出（S1-11 改訂）。`enabled = true` かつ鍵が解決できない場合は
        // `from_config` が Err を返し、ここで起動が fail closed する。
        let anthropic_client = crate::llm::AnthropicClient::from_config(&config.llm)
            .context("configure llm signal extraction client")?;
        // 返信文下書き（デモ用）は同じクライアントを使い回す。`customer_reply_draft_enabled`
        // が true でも `[llm] enabled = false` なら client が無いので、下書きは黙って出ない
        // （signal 抽出が lexicon 単独へフォールバックするのと同じ degrade。起動は止めない）。
        let reply_drafter = if config.harness.customer_reply_draft_enabled {
            // max_tokens = 0 は API エラーになるだけで、毎回 warn + null という分かりにくい
            // 壊れ方をする。設定ミスは起動時に気づける形で弾く。
            anyhow::ensure!(
                config.harness.customer_reply_draft_max_tokens > 0,
                "harness.customer_reply_draft_max_tokens must be greater than 0 when \
                 customer_reply_draft_enabled = true (got 0; every draft would fail at the API \
                 and silently return null)"
            );
            if anthropic_client.is_none() {
                tracing::warn!(
                    "harness.customer_reply_draft_enabled = true ですが [llm] enabled = false の\
                     ため下書きは生成されません（customer_reply_draft は常に null になります）"
                );
            }
            anthropic_client.clone()
        } else {
            None
        };
        let llm_classifier: Option<Arc<dyn extraction::ClassifyLlm>> =
            anthropic_client.map(|client| {
                Arc::new(extraction::AnthropicSignalClassifier::new(
                    client,
                    lexicon.vocabulary_for_prompt(),
                )) as Arc<dyn extraction::ClassifyLlm>
            });
        let extractor: Arc<dyn extraction::AsyncSignalExtractor> = Arc::new(
            extraction::HybridExtractor::new(lexicon.clone(), llm_classifier),
        );
        // 材料 corpus ローダは 1 インスタンスを ManualStore と evaluate で共有し、
        // manual_corpus の TTL キャッシュを read 経路・評価経路の双方で使い回す。
        let corpus = Arc::new(crate::corpus::CorpusLoader::new(client.clone()));
        Ok(Self {
            authenticator: authn::Authenticator::new(
                config.projects.iter().map(|p| p.schema.clone()).collect(),
            ),
            normalizer: lexicon.clone(),
            lexicon,
            extractor,
            ng: egress::NgDictionary::from_path(&resolve_path(&config.harness.ng_dictionary_path))?,
            worm: Arc::new(audit::WormAuditLog::open(&resolve_path(
                &config.harness.audit_log_path,
            ))?),
            knowledge: Some(knowledge::KnowledgeStore::new(client.clone())),
            thresholds: (&config.harness.thresholds).into(),
            grading: (&config.harness.grading).into(),
            queue_path: resolve_path(&config.harness.search_improvement_queue_path),
            grade_lock: tokio::sync::Mutex::new(()),
            manual: Some(crate::manual::retrieval::ManualStore::new(
                client.clone(),
                corpus.clone(),
                config.harness.manual_scoring_v2_enabled,
            )),
            corpus: Some(corpus),
            default_route: config.harness.default_escalation_route.clone(),
            vector_route_enabled: config.harness.vector_route_enabled,
            reply_drafter,
            reply_draft_max_tokens: config.harness.customer_reply_draft_max_tokens,
            product_gate: Some(product_gate::ProductGate::new(client)),
        })
    }

    fn knowledge(&self) -> Result<&knowledge::KnowledgeStore> {
        self.knowledge
            .as_ref()
            .ok_or_else(|| anyhow!("knowledge store is not configured"))
    }

    fn corpus(&self) -> Result<&crate::corpus::CorpusLoader> {
        self.corpus
            .as_deref()
            .ok_or_else(|| anyhow!("corpus loader is not configured"))
    }

    fn product_gate(&self) -> Result<&product_gate::ProductGate> {
        self.product_gate
            .as_ref()
            .ok_or_else(|| anyhow!("product gate is not configured"))
    }

    /// 取扱製品 allowlist（Issue #28 design doc §2）。schema 単位に TTL 10 分でキャッシュされる。
    /// 質問側ゲート（`api.rs`）・材料選別・プロンプト注入・応答側ゲートの共通入口。
    pub async fn product_allowlist(
        &self,
        schema: &str,
    ) -> Result<Arc<product_gate::ProductAllowlist>> {
        self.product_gate()?.allowlist(schema).await
    }

    /// tool handler から材料ストアへアクセスするための入口（判定は持たない）。
    pub fn store(&self) -> Result<&knowledge::KnowledgeStore> {
        self.knowledge()
    }

    /// support_case の会話状態（会話フロー v1.1）を読む。case 未存在は全既定値として扱う
    /// （エラーにしない。新規会話・古い case（この 4 属性を持たない）の両方が該当する）。
    pub async fn load_conv_state(
        &self,
        ctx: &RequestContext,
        case_id: &str,
    ) -> Result<CaseConvState> {
        let attrs = self
            .knowledge()?
            .load_case(&ctx.schema, case_id)
            .await?
            .unwrap_or_default();
        Ok(conv_state_from_attrs(&attrs))
    }

    /// support_case の会話状態を保存する。read-merge-write で既存属性（`question` 等）を保ち、
    /// 会話状態 4 属性だけを上書きしたうえで全属性を明示再送する（vegapunk 0.2.0 の
    /// `UpsertNodes` は全置換のため、部分送信は既存属性を消す。`backfill_concept_keys` と
    /// 同じ流儀）。
    ///
    /// **契約: 呼び出し側は case が既に存在する状態でだけ呼ぶこと。** `evaluate()` は冒頭で
    /// case 属性を読み、末尾で全属性を再送する（新規 case ならその時点で `record` 済み）。この
    /// 順序を守らずに case 未作成のタイミングで本メソッドを呼ぶと、[`require_existing_case_attrs`]
    /// が `Err` にする（Warning 2 の回帰防止）。
    ///
    /// **契約（lost update）: 本メソッドは `evaluate()` と並行に、または `evaluate()` の実行中に
    /// 呼んではならない。必ず `evaluate()` が完了した後に呼ぶこと。** `evaluate()` は関数冒頭で
    /// 読んだ case 属性のスナップショットを保持したまま vegapunk 検索・LLM 抽出を挟み、最後に
    /// **全属性を再送**する（read-merge-write の「read」が古いまま「write」される）。
    /// `evaluate()` の実行中に本メソッドが割り込むと、本メソッドが書いた会話状態 4 属性を、
    /// 後から確定する `evaluate()` の書き込みが古いスナップショットで上書きし、会話状態が
    /// 消える（逆順・非重複なら問題ない）。呼び出し側（Part B のオーケストレーション）はこの
    /// 順序を守ること。
    /// 黙って `unwrap_or_default()` していた旧実装は、case が無いと会話状態 4 属性だけを持つ
    /// `case_id` 属性なしの support_case ノードを書いていた。このノードは `case_id eq` で
    /// 検索する `knowledge::load_case` にも、`attrs.get("case_id")?` で filter_map する
    /// `load_cases` にも二度と見えなくなり、会話状態が保存されたつもりで毎ターン既定値へ戻る
    /// （聞き返しの 3 ターン上限が機能しなくなる）。加えて schema 上 `case_id` は
    /// required（`schema/cs-support.yml`）であり、無属性ノードはスキーマ違反でもある。
    /// **なので握りつぶさず fail closed する。**
    pub async fn save_conv_state(
        &self,
        ctx: &RequestContext,
        case_id: &str,
        state: &CaseConvState,
    ) -> Result<()> {
        let knowledge = self.knowledge()?;
        let existing = knowledge.load_case(&ctx.schema, case_id).await?;
        let existing = require_existing_case_attrs(existing, case_id, &ctx.schema)?;
        let merged = merge_conv_state_attributes(&existing, state);
        knowledge
            .record(
                &ctx.schema,
                "support_case",
                case_id,
                merged.into_iter().collect(),
            )
            .await
    }

    /// 監査イベントの共通入口。ctx 由来の provenance フィールドをここで一元的に埋める。
    pub async fn audit(
        &self,
        ctx: &RequestContext,
        decision: impl Into<String>,
        route: Option<String>,
        governing_norm_ids: Vec<String>,
    ) -> Result<String> {
        self.audit_with_nodes(ctx, decision, route, governing_norm_ids, Vec::new(), None)
            .await
    }

    /// 監査イベントの入口（retrieved_node_ids・extraction_mode を additive に受け取る版）。
    /// WORM の同期ファイル書き込み（hash chain のため直列）は spawn_blocking で
    /// async ワーカーから隔離する（tool handler をブロックしない）。
    ///
    /// `extraction_mode`: 今ターンの signal 抽出がどの経路を通ったか。抽出を行わない
    /// tool（resolve_product / get_section / get_product / search_past_cases /
    /// legacy search_manual 等）は `None` を渡す（WORM には `"not_applicable"` と記録
    /// される、`extraction::audit_extraction_mode` 参照）。抽出を伴う経路（evaluate、
    /// signal 抽出統一後の search_manual / search_known_resolutions）は `Some(mode)`
    /// を渡す。
    pub async fn audit_with_nodes(
        &self,
        ctx: &RequestContext,
        decision: impl Into<String>,
        route: Option<String>,
        governing_norm_ids: Vec<String>,
        retrieved_node_ids: Vec<String>,
        extraction_mode: Option<extraction::ExtractionMode>,
    ) -> Result<String> {
        let draft = audit::AuditDraft {
            request_id: ctx.request_id.clone(),
            schema: ctx.schema.clone(),
            actor: ctx.actor.sub.clone(),
            actor_email: ctx.actor.email.clone(),
            used_scope: ctx.scope.clone(),
            retrieved_node_ids,
            decision: decision.into(),
            route,
            governing_norm_ids,
            extraction_mode: extraction::audit_extraction_mode(extraction_mode),
        };
        let worm = self.worm.clone();
        tokio::task::spawn_blocking(move || worm.append(draft))
            .await
            .context("join audit write task")?
    }

    /// record_answer_outcome の本体（遵守事項 3）。attempt の存在検証 → outcome の
    /// write-once 強制 → outcome 記録 → KR 紐づけ（サーバ記録）があれば grade 更新、を
    /// grade_lock の同一クリティカルセクションで行う（重複加算・TOCTOU を封鎖）。
    /// 戻り値: (格付けが変わった場合の新 grade, 対象 KR id)。
    pub async fn record_answer_outcome(
        &self,
        ctx: &RequestContext,
        attempt_id: &str,
        outcome: grading::AnswerOutcome,
        note: Option<&str>,
    ) -> Result<(Option<rules::Grade>, Option<String>)> {
        // プロセス内直列化（backend atomic は TODO）。ただし正しさはロックに依存しない:
        // カウントは attempt 群からの再計算（導出）なので、再送・部分失敗のどこから
        // やり直しても同じ結果になる（増分方式の多重加算・欠落の両方が構造的に消える）。
        let _guard = self.grade_lock.lock().await;
        let store = self.knowledge()?;
        let attempt = store
            .load_attempt(&ctx.schema, attempt_id)
            .await?
            .ok_or_else(|| anyhow!("unknown attempt_id: {attempt_id}"))?;
        // KR 紐づけはサーバ記録（attempt.known_resolution_id）のみを使う
        let kr_id = attempt
            .get("known_resolution_id")
            .filter(|kr_id| !kr_id.is_empty())
            .cloned();
        // outcome は write-once。同一 outcome の再送のみ冪等に受理する
        // （部分失敗後の再開経路。導出方式なので再計算しても増えない）。
        if let Some(recorded) = attempt.get("outcome").filter(|o| !o.is_empty()) {
            if recorded != outcome.as_str() {
                return Err(anyhow!(
                    "outcome {recorded} is already recorded for attempt {attempt_id}; \
                     outcomes are write-once"
                ));
            }
        } else {
            // read-merge-write: 既存属性（draft / case_id / known_resolution_id 等）を
            // ベースに outcome を重ねて全属性を再送する（全属性置換セマンティクスでも安全）。
            let merged = merge_outcome_attributes(&attempt, attempt_id, outcome, &ctx.actor, note);
            store
                .record(
                    &ctx.schema,
                    "answer_attempt",
                    attempt_id,
                    merged.into_iter().collect(),
                )
                .await?;
        }
        let new_grade = match &kr_id {
            Some(kr_id) => self.recompute_grade(ctx, kr_id).await?,
            None => None,
        };
        Ok((new_grade, kr_id))
    }

    /// KR の承認/却下カウント・承認者集合を attempt 群から導出し直し、regrade 純関数で
    /// 昇格・降格を判定して永続化する。格付けが変わった場合のみ Some を返す。
    /// 導出＝再計算なので何度呼んでも同じ結果（冪等）。呼び出し元が grade_lock を保持していること。
    async fn recompute_grade(
        &self,
        ctx: &RequestContext,
        kr_id: &str,
    ) -> Result<Option<rules::Grade>> {
        let store = self.knowledge()?;
        let resolutions = store.load_known_resolutions(&ctx.schema).await?;
        let kr = resolutions
            .iter()
            .find(|kr| kr.id == kr_id)
            .ok_or_else(|| anyhow!("known_resolution not found: {kr_id}"))?;
        let attempts = store.load_attempts_for_kr(&ctx.schema, kr_id).await?;
        let counts = grading::derive_outcome_counts(&attempts);
        // 旧形式 actor は名寄せ不能なため昇格判定の母集団から外している（grading.rs）。
        // 除外が無音だと「承認は積んだのに昇格しない」理由を運用者が辿れないので、
        // 除外が起きた回だけ kr_id と件数を残す。
        if counts.legacy_excluded_count > 0 {
            tracing::warn!(
                kr_id = %kr_id,
                schema = %ctx.schema,
                legacy_excluded_approvers = counts.legacy_excluded_count,
                approver_count = counts.approver_count,
                approver_set_len = counts.approver_set.len(),
                promote_approvers = self.grading.promote_approvers,
                "legacy google:{{email}} approvers are excluded from approver diversity; \
                 promotion may be blocked. approver_set is persisted in full; only the \
                 diversity count is reduced."
            );
        }
        let regraded = grading::regrade(
            kr.grade,
            counts.approval_count,
            counts.rejection_count,
            counts.approver_count,
            &self.grading,
        );
        // 永続化には除外前の全承認者を渡す。除外後の集合を書き戻すと、cutover 前に
        // 記録済みの承認者が次の outcome 記録で静かに消える（W2）。
        store
            .update_known_resolution_grade(
                &ctx.schema,
                kr_id,
                counts.approval_count,
                counts.rejection_count,
                &counts.approver_set,
                regraded,
            )
            .await?;
        Ok((regraded != kr.grade).then_some(regraded))
    }

    /// add_known_resolution の admission 判定（S1-5 / GMR の進化の入口）。
    /// 役割・語彙・NG 語のガードをここで一元化し、通過時は signal 集合を返す。
    pub fn admit_known_resolution(
        &self,
        ctx: &RequestContext,
        signals: &[String],
        answer: &str,
        rationale_text: Option<&str>,
        manual_section_keys: &[String],
    ) -> Result<signal::SignalSet> {
        // authoritative の担い手のみ（supervisor / admin）
        if !matches!(ctx.actor.role, authn::Role::Supervisor | authn::Role::Admin) {
            anyhow::bail!(
                "permission_denied: add_known_resolution requires supervisor or admin role"
            );
        }
        // legacy schema (sivira) には Rationale ノード型が無いため、rationale_text を
        // サイレントに落とすのではなく admission 側で拒否する（build 側は無視するだけになる）。
        if ctx.manual_schema == crate::config::ManualSchemaKind::LegacySection
            && rationale_text.is_some()
        {
            anyhow::bail!("rationale_text is not supported on legacy schemas");
        }
        // 監査可能性: KR は最低 1 つの根拠アンカー（BASED_ON/BECAUSE の結線元）を持つこと。
        // manual_section_keys が空で、かつ rationale_text も無い KR は traceable evidence を
        // 一切持たないため拒否する（legacy は rationale_text 不可なので実質 section 必須）。
        if manual_section_keys.is_empty() && rationale_text.is_none() {
            anyhow::bail!(
                "known resolution requires at least one evidence anchor: \
                 provide manual_section_keys and/or rationale_text"
            );
        }
        // 語彙外 signal は照合不能なので拒否
        if signals.is_empty() {
            anyhow::bail!("signals must not be empty");
        }
        let mut set = signal::SignalSet::new();
        for value in signals {
            let sig = signal::Signal::new(value);
            if self.lexicon.class_of(&sig).is_none() {
                anyhow::bail!("unknown signal (not in vocabulary): {value}");
            }
            set.insert(sig);
        }
        // egress を通らない回答文は知識として登録させない（登録しても emit 時に必ず
        // block / abstain される＝危険なだけの知識になるため、入口で一貫して拒否する）。
        // binding は build_known_resolution_graph が advisory 固定で書く（mandatory は自動で書けない）。
        match egress::egress_gate(
            answer,
            &egress::EmitContext {
                channel: egress::EmitChannel::Operator,
            },
            &self.ng,
        ) {
            egress::EgressVerdict::Block { term } => {
                anyhow::bail!("answer contains blocked term: {term}")
            }
            egress::EgressVerdict::Abstain { term } => {
                anyhow::bail!(
                    "answer contains implied-efficacy term: {term}; \
                     rephrase the answer so it passes the egress gate before registering"
                )
            }
            egress::EgressVerdict::Pass => {}
        }
        Ok(set)
    }

    /// 第1・2層ルールが参照する signal が語彙に存在することを検証する（fail closed）。
    fn validate_rule_vocabulary(
        &self,
        rules: &[rules::EscalationRule],
        domains: &[rules::ProhibitedDomain],
    ) -> Result<()> {
        for rule in rules {
            for sig in &rule.condition {
                if self.lexicon.class_of(sig).is_none() {
                    return Err(anyhow!(
                        "escalation_rule {} references a signal not in the vocabulary: {}",
                        rule.id,
                        sig.as_str()
                    ));
                }
            }
        }
        for domain in domains {
            for sig in &domain.domain_signals {
                if self.lexicon.class_of(sig).is_none() {
                    return Err(anyhow!(
                        "prohibited_domain {} references a signal not in the vocabulary: {}",
                        domain.id,
                        sig.as_str()
                    ));
                }
            }
        }
        Ok(())
    }

    /// S1-1 パイプライン前半: [認証] → [(A) 権限]。全 tool がここを通る。
    /// `identity` は Google OAuth ミドルウェアが検証済みの Google identity
    /// （`oauth::VerifiedIdentity`。安定した `sub` + 認証時点の email）。
    pub fn begin(
        &self,
        identity: &crate::oauth::VerifiedIdentity,
        project_schema: &str,
        project_manual_schema: crate::config::ManualSchemaKind,
    ) -> Result<RequestContext> {
        let actor = self.authenticator.lookup_by_identity(identity)?;
        let access = scope::resolve_scope(&actor, project_schema)?;
        Ok(RequestContext {
            schema: access.enforced_schema().to_string(),
            actor,
            scope: access,
            request_id: uuid::Uuid::new_v4().to_string(),
            manual_schema: project_manual_schema,
        })
    }

    /// S1-1 パイプライン後半: [取得] → [正規化] → [会話層 累積] → [(B) 3 層判定] → [記録]。
    /// 会話層（S1-0 / 遵守事項 1）: case_id 単位の累積 signal 集合をサーバ側で維持し、
    /// **毎ターン累積集合で再判定**する。条件が増えたら（変色 → 変色+カビ）再判定が
    /// 自動的にエスカレーションへ倒れる。会話履歴の言質は判定入力にしない。
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate(
        &self,
        ctx: &RequestContext,
        question: &str,
        product_key: Option<&str>,
        case_id: Option<&str>,
        tools: &ToolService,
        history: &[reply::ReplyHistoryTurn],
        is_continuation: bool,
        unknown_case_id_policy: UnknownCaseIdPolicy,
    ) -> Result<EvaluationOutcome> {
        let knowledge = self.knowledge()?;
        // [取得] scope は ctx.schema として全検索に注入済み（tenant=schema）。
        // 独立な読み取りは並列に発行し、graph snapshot は 1 回だけ取得して
        // KR 復元・マニュアル検索・case signal 復元で共有する（重複取得を避ける）。
        let (rules, domains) = tokio::try_join!(
            knowledge.load_escalation_rules(&ctx.schema),
            knowledge.load_prohibited_domains(&ctx.schema),
        )?;
        // ルールの signal が語彙外だと「決してマッチしないルール」＝サイレントな
        // fail open になるため、判定前に語彙と突合して fail closed にする。
        self.validate_rule_vocabulary(&rules, &domains)?;
        // 判定入力（support_case / Signal / HAS_SIGNAL）に使う live corpus と、マニュアル検索に
        // 使う corpus を manual_schema で分けて取得する。graph_snapshot(5000) の全件依存・
        // truncate 停止を ManualV1 で撤去する（数万ノード規模でも読み取りを止めない）。
        // - ManualV1: live_corpus（都度取得・小）を KR/case/related に、manual_corpus
        //   （TTL キャッシュ・ページングで上限なし）を manual 検索に。
        // - LegacySection: 従来どおり graph_snapshot を全消費で共有（sivira-cs-demo 専用・本番外）。
        let (live_snapshot, manual_corpus): (
            crate::proto::graphrag::GetGraphSnapshotResponse,
            Option<Arc<crate::proto::graphrag::GetGraphSnapshotResponse>>,
        ) = match ctx.manual_schema {
            crate::config::ManualSchemaKind::ManualV1 => {
                let corpus = self.corpus()?;
                let (live, manual) = tokio::try_join!(
                    corpus.live_corpus(&ctx.schema),
                    corpus.manual_corpus(&ctx.schema),
                )?;
                (live, Some(manual))
            }
            crate::config::ManualSchemaKind::LegacySection => {
                (knowledge.fetch_snapshot(&ctx.schema).await?, None)
            }
        };
        // manual 検索は accumulated signal 集合（会話層）を使うため、hits の取得は
        // accumulated が確定した後ろに回す（下記 manual 取得ブロック）。
        // [正規化] lexicon ∪ LLM のハイブリッド抽出（S1-11 改訂）。今ターン分。
        // KR 読み込み（gRPC）と signal 抽出（LLM 有効時は HTTP 往復を伴う）は互いに
        // 依存しないため並列発行し、LLM 往復レイテンシを KR 読み込みの裏に隠す。
        let (resolutions, extraction_outcome) = tokio::join!(
            knowledge.load_known_resolutions_with(&ctx.schema, &live_snapshot),
            self.extractor.extract(question),
        );
        let resolutions = resolutions?;
        let signals = extraction_outcome.signals;
        let extraction_mode = extraction_outcome.mode;
        // [会話層] 累積 signal 集合の維持。client 供給の prior signals は受けない（入力不信）。
        // 既存 case_id は存在を確認する。存在すれば復元する。存在しない（未知の id）場合の
        // 扱いは `unknown_case_id_policy` で経路ごとに分ける:
        // - `/api/reply`（`UnknownCaseIdPolicy::StartNew`）: design doc §2「未知の case_id は
        //   エラーにせず新規 case として処理し、warn ログを出す」。クライアント側（LINE
        //   アダプタ等）が保持する case_id をそのまま渡すため、サーバ再起動やクライアント側の
        //   保存漏れで未知 id が届くのは異常系ではなく通常運用として扱う。この fallback は
        //   `/api/reply` の契約としてのみ定義されている（design doc §2）。
        // - MCP `evaluate_answerability`（`UnknownCaseIdPolicy::Reject`）: 従来どおり Err。
        //   CS 担当が case_id を打ち間違えた場合に黙って新規 case へ合流すると、会話層の
        //   累積 signal（エスカレーション判定の根拠）が失われたまま気づけなくなるため、
        //   即エラーで気づける従来の厳格な挙動を維持する。
        // case の全属性を手元に保持し、後段の判定記録は read-merge-write で全属性を再送する
        // （UpsertNodes が全属性置換セマンティクスでも既存属性を失わない）。
        let existing_case = match case_id {
            Some(id) => knowledge.load_case(&ctx.schema, id).await?,
            None => None,
        };
        if let Some(id) = case_id {
            if existing_case.is_none() {
                if matches!(unknown_case_id_policy, UnknownCaseIdPolicy::Reject) {
                    return Err(anyhow!("unknown case_id: {id}"));
                }
                tracing::warn!(
                    request_id = %ctx.request_id,
                    schema = %ctx.schema,
                    requested_case_id = id,
                    "case_id not found; starting a new case under the /api/reply contract (UnknownCaseIdPolicy::StartNew)"
                );
            }
        }
        // existing_case が None かつ case_id が Some だった場合のみ「未知 case_id → 新規 case」の
        // フォールバックが発生している(case_id が最初から None の通常の新規会話とは区別する)。
        let previous_case_id: Option<&str> = if existing_case.is_none() {
            case_id
        } else {
            None
        };
        let (case_id, prior_signals, mut case_attrs) = match existing_case {
            Some(attrs) => {
                // 直前の `match case_id { Some(id) => ... }` で `existing_case` を得ているため、
                // ここに来る時点で `case_id` は必ず `Some`。
                let id = case_id.expect("existing_case is Some only when case_id was Some");
                (
                    id.to_string(),
                    knowledge::case_signals_from_snapshot(&ctx.schema, id, &live_snapshot),
                    attrs,
                )
            }
            None => {
                let new_id = format!("case-{}", uuid::Uuid::new_v4());
                let attrs: std::collections::HashMap<String, String> = [
                    ("case_id".to_string(), new_id.clone()),
                    ("request_id".to_string(), ctx.request_id.clone()),
                    ("actor".to_string(), ctx.actor.sub.clone()),
                    // actor は安定 ID（google-sub:{sub}）で人間には読めないため、
                    // case を追う担当者向けに当時の email も併記する（加算属性）。
                    ("actor_email".to_string(), ctx.actor.email.clone()),
                    ("question".to_string(), question.to_string()),
                    (
                        "product_key".to_string(),
                        product_key.unwrap_or_default().to_string(),
                    ),
                    ("created_at".to_string(), chrono::Utc::now().to_rfc3339()),
                ]
                .into_iter()
                .collect();
                knowledge
                    .record(
                        &ctx.schema,
                        "support_case",
                        &new_id,
                        attrs.clone().into_iter().collect(),
                    )
                    .await?;
                if let Some(prev) = previous_case_id {
                    tracing::warn!(
                        request_id = %ctx.request_id,
                        schema = %ctx.schema,
                        previous_case_id = prev,
                        case_id = %new_id,
                        "unknown case_id folded into new case (UnknownCaseIdPolicy::StartNew)"
                    );
                }
                (new_id, signal::SignalSet::new(), attrs)
            }
        };
        let accumulated: signal::SignalSet = prior_signals.union(&signals).cloned().collect();
        let new_signals: signal::SignalSet = signals.difference(&prior_signals).cloned().collect();
        knowledge
            .append_case_signals(&ctx.schema, &case_id, &new_signals)
            .await?;
        // stakes 入力の決定論算出（累積集合に対して）。
        // mandatory 領域だけに絞って match_layer2 を 1 回呼ぶ（質問文の再正規化を N 回しない）。
        let mandatory_domains: Vec<rules::ProhibitedDomain> = domains
            .iter()
            .filter(|d| d.binding == rules::Binding::Mandatory)
            .cloned()
            .collect();
        let stakes_input = decision::StakesInput {
            mandatory_domain_near: rules::match_layer2(&mandatory_domains, &accumulated, question)
                .is_some(),
            ng_near_hit: self.ng.near_hit(question),
            hazard_signal_count: accumulated
                .iter()
                .filter(|s| self.lexicon.class_of(s) == Some(signal::SignalClass::Hazard))
                .count(),
        };
        // manual 取得を manual_schema で分岐する。ManualV1 は ManualStore（signal 絞り込み(A) +
        // body 全文(B) の max）、LegacySection は従来の tools.search_manual_with_snapshot。
        // 判定へは共通の best_manual_score / best_manual_sections に落とし、
        // EvaluationOutcome.hits へは SectionHit に揃えて返す（From<ManualHit> で変換）。
        let (section_hits, retrieved_manual_ids): (Vec<SectionHit>, Vec<String>) =
            match ctx.manual_schema {
                crate::config::ManualSchemaKind::ManualV1 => {
                    let store = self
                        .manual
                        .as_ref()
                        .ok_or_else(|| anyhow!("manual store not configured"))?;
                    // 意味検索（ベクトル経路）は urtect design §2.3: 合成の可否・最終スコアは
                    // 決定論の search_with_snapshot が握る。ここでは候補材料を用意するだけ。
                    let vector_hits = store
                        .vector_hits(
                            self.vector_route_enabled,
                            &ctx.schema,
                            question,
                            EVALUATE_TOP_K,
                        )
                        .await;
                    let manual_corpus = manual_corpus
                        .as_deref()
                        .ok_or_else(|| anyhow!("manual corpus missing for ManualV1 evaluate"))?;
                    // 回答可能性（coverage / best_manual_score）は product で hard-scope しない。
                    // product スコープは「当該 Product を DESCRIBES する節 or 機種非依存の節」だけを
                    // 残し、他機種のみを DESCRIBES する節を除外する。ところがパスワードリセットのような
                    // 機種横断 how-to は特定機種ページとして DESCRIBES 辺を持つことがあり、resolve 済み
                    // product で絞ると本来 answerable なページが候補から消え、best_manual_score が低く
                    // 出て false-escalate する（実測: スコープ有 0.561 < 閾値、スコープ無 0.917）。
                    // そこで evaluate の内部検索は product_key=None で走らせ、best_manual_score と
                    // best_manual_sections（＝ evidence lineage）を同一 hit 列から coherent に導出する
                    // （score は横断ページ、evidence は別ページ、という不整合を作らない）。
                    // product は「絞り込み」から「（任意の）加点」へ格下げする方針で、現状は加点も
                    // 掛けない（最小差分・ゲート挙動優先）。search_manual ツールが明示 product_key を
                    // 尊重する挙動は search_with_snapshot 側で不変（本変更は evaluate の呼び出しのみ）。
                    let hits = store.search_with_snapshot(
                        &ctx.schema,
                        question,
                        &accumulated,
                        None,
                        EVALUATE_TOP_K,
                        manual_corpus,
                        &vector_hits,
                    )?;
                    let ids = hits
                        .iter()
                        .map(|h| {
                            crate::manual::schema_ids::manual_node_id(
                                &ctx.schema,
                                "ManualSection",
                                &h.section_key,
                            )
                        })
                        .collect();
                    let converted = hits.into_iter().map(SectionHit::from).collect();
                    (converted, ids)
                }
                crate::config::ManualSchemaKind::LegacySection => {
                    // search 側は snapshot を消費するため、共有元のここでだけ clone する
                    let hits = tools
                        .search_manual_with_snapshot(
                            &ctx.schema,
                            question,
                            product_key,
                            5,
                            live_snapshot.clone(),
                        )
                        .await?;
                    let ids = hits
                        .iter()
                        .map(|h| crate::ingest::section_node_id(&ctx.schema, &h.section_key))
                        .collect();
                    (hits, ids)
                }
            };
        // Issue #28 codex レビュー採用1(Critical): カバレッジ判定(decide())より前に、取扱外
        // 型番のみを言及する hit を除外する。`retrieved_manual_ids`（監査 lineage）はこの
        // フィルタの影響を受けない意図的な設計（「何を検索で取得したか」の監査記録は、判定・
        // 下書きに使う材料の選別とは独立に保つ）ため、フィルタ前の `retrieved_manual_ids` は
        // 上のタプル分解のまま変更しない。
        let allowlist = self.product_allowlist(&ctx.schema).await?;
        let section_hits = filter_out_of_scope_hits(section_hits, &allowlist);
        // [(B) 3 層判定] 純関数。判定根拠は常に「累積 signal 集合 + known_resolution」。
        let best = section_hits.first();
        let best_manual_score = best.map(|h| h.score);
        let section_keys: Vec<String> =
            section_hits.iter().map(|h| h.section_key.clone()).collect();
        let decision_result = decision::decide(&decision::DecisionInput {
            question_signals: &accumulated,
            question_raw: question,
            rules: &rules,
            domains: &domains,
            resolutions: &resolutions,
            best_manual_score,
            best_manual_sections: &section_keys,
            stakes_input,
            thresholds: &self.thresholds,
            default_route: &self.default_route,
        });
        // 聞き返し可否（決定論）: 第3層グレーのみ。判定条件そのものは clarification_allowed()
        // （本ファイル冒頭のモジュールレベル関数）が契約として持つ。ここでは呼ぶだけにする。
        let clarification_allowed = clarification_allowed(&decision_result);
        // [記録] 判定結果を case に永続化する（record_answer_attempt の lineage 検証の根拠。
        // client の自己申告でなくサーバ側の記録と突合するため）。KR 由来の回答なら
        // その kr_id もサーバ記録として残す（outcome 記録が client 申告に依存しないため）。
        // last_evidence_keys / last_evidence_kind（S1-2）: record_answer_attempt が emit した
        // 根拠を answer_evidence として書けるよう、判定が使った根拠キーをサーバ記録として残す。
        // Allowed-manual は evidence_section_keys の結合、Allowed-KR は kr_id 単体、
        // Escalate は空（エスカレーション済み case は emit 経路に乗らない）。
        let (case_decision, case_kr_id, last_evidence_keys, last_evidence_kind) =
            match &decision_result {
                decision::AnswerDecision::Allowed {
                    known_resolution_id,
                    evidence_section_keys,
                    source,
                    ..
                } => {
                    let kr_id = known_resolution_id.clone().unwrap_or_default();
                    let (keys, kind) = match source {
                        decision::AnswerSource::KnownResolution => {
                            (kr_id.clone(), "known_resolution")
                        }
                        decision::AnswerSource::Manual => {
                            (evidence_section_keys.join(","), "manual")
                        }
                    };
                    ("allowed", kr_id, keys, kind.to_string())
                }
                decision::AnswerDecision::Escalate { .. } => {
                    ("escalate", String::new(), String::new(), String::new())
                }
            };
        case_attrs.insert("case_id".to_string(), case_id.clone());
        case_attrs.insert("last_request_id".to_string(), ctx.request_id.clone());
        case_attrs.insert("last_decision".to_string(), case_decision.to_string());
        case_attrs.insert("last_kr_id".to_string(), case_kr_id);
        case_attrs.insert("last_evidence_keys".to_string(), last_evidence_keys);
        case_attrs.insert("last_evidence_kind".to_string(), last_evidence_kind);
        knowledge
            .record(
                &ctx.schema,
                "support_case",
                &case_id,
                case_attrs.into_iter().collect(),
            )
            .await?;
        // [記録] WORM（S1-8 条件 8）。KR 由来の許可はどの KR に基づいたかを
        // governing_norm_ids / retrieved_node_ids に残す（監査ログ単体で lineage を追跡可能に）。
        let (decision_label, route) = match &decision_result {
            decision::AnswerDecision::Allowed { source, .. } => {
                (format!("allowed:{source:?}"), None)
            }
            decision::AnswerDecision::Escalate {
                layer, route_to, ..
            } => (format!("escalate:layer{layer}"), Some(route_to.clone())),
        };
        // [取得] S1-1: past_case も取得する（参考情報として返すのみ・decide() には渡さない）。
        // 追加 RPC なしで、evaluate 冒頭で取得済みの snapshot を再利用する。自 case は除外する。
        let related_cases: Vec<RelatedCase> = knowledge::search_cases_from_snapshot(
            &live_snapshot,
            question,
            3,
            Some(case_id.as_str()),
        )
        .into_iter()
        .map(|(case, _score)| RelatedCase {
            case_id: case.case_id,
            question: case.question,
            last_decision: case.last_decision,
        })
        .collect();
        let mut retrieved_node_ids: Vec<String> = retrieved_manual_ids;
        retrieved_node_ids.push(knowledge::harness_node_id(
            &ctx.schema,
            "support_case",
            &case_id,
        ));
        for related in &related_cases {
            retrieved_node_ids.push(knowledge::harness_node_id(
                &ctx.schema,
                "support_case",
                &related.case_id,
            ));
        }
        let mut governing_norm_ids = Vec::new();
        if let decision::AnswerDecision::Allowed {
            known_resolution_id: Some(kr_id),
            ..
        } = &decision_result
        {
            retrieved_node_ids.push(knowledge::harness_node_id(
                &ctx.schema,
                "KnownResolution",
                kr_id,
            ));
            governing_norm_ids.push(kr_id.clone());
        }
        let audit_event_id = self
            .audit_with_nodes(
                ctx,
                decision_label,
                route,
                governing_norm_ids,
                retrieved_node_ids,
                Some(extraction_mode),
            )
            .await?;
        // [デモ] 顧客向け返信文の下書き。**判定が確定した後**に、その判定の制約下でだけ作る。
        // 生成に失敗しても評価そのものは成功させる（下書きはデモ用の付加情報であり、これが
        // 落ちたせいで回答可否判定まで失敗させるのは本末転倒）。失敗理由は必ず warn に残す。
        let reply_draft = self
            .draft_customer_reply(
                question,
                &decision_result,
                &section_hits,
                &resolutions,
                history,
                is_continuation,
                &allowlist,
            )
            .await;
        let customer_reply_draft_truncated = reply_draft.as_ref().is_some_and(|d| d.truncated);
        let customer_reply_draft = reply_draft.map(|d| d.text);

        Ok(EvaluationOutcome {
            decision: decision_result,
            signals,
            accumulated_signals: accumulated,
            case_id,
            clarification_allowed,
            hits: section_hits,
            audit_event_id,
            related_cases,
            extraction_mode,
            customer_reply_draft,
            customer_reply_draft_truncated,
        })
    }

    /// 顧客向け返信文の下書きを 1 案作る（デモ用）。無効化時・LLM 未設定時・生成失敗時は
    /// `None` を返し、**評価そのものは成功させる**。
    ///
    /// 材料の選別（Escalate ではマニュアル本文を一切渡さない）は `reply::build_reply_brief`
    /// が担う。取扱外型番のみを言及する材料の除外（Issue #28 §3.2）は、判定確定前の
    /// `evaluate()` 側で `filter_out_of_scope_hits` により既に完了しているため、ここへ渡って
    /// くる `hits` はフィルタ済み。この関数はその結果を送るだけで、安全判断をこの関数に
    /// 持ち込まない。
    ///
    /// `allowlist` は `evaluate()` が判定確定前に取得済みのものをそのまま受け取る（Issue #28
    /// codex レビュー採用1: allowlist の fetch を判定前の 1 箇所へ集約し、ここでの再取得は
    /// しない。以前はここで `self.product_allowlist(schema)` を再度呼んでいたが、二重取得は
    /// TTL キャッシュ経由とはいえ無駄であり、かつ「判定と下書きが異なる瞬間の allowlist を
    /// 見る」余地を生む）。
    ///
    /// `is_continuation` は `evaluate()` から素通しされる会話段階フラグ（判定はサーバ側が
    /// コードで行う。`reply::build_reply_system_prompt` の doc を参照）。
    #[allow(clippy::too_many_arguments)]
    async fn draft_customer_reply(
        &self,
        question: &str,
        decision: &decision::AnswerDecision,
        hits: &[SectionHit],
        resolutions: &[rules::KnownResolution],
        history: &[reply::ReplyHistoryTurn],
        is_continuation: bool,
        allowlist: &product_gate::ProductAllowlist,
    ) -> Option<crate::llm::ReplyDraft> {
        let drafter = self.reply_drafter.as_ref()?;
        // KR 由来 Allowed は evidence_section_keys が空なので、承認済み回答本文を材料として
        // 引いて渡す（引けなければ材料ゼロのまま = でっち上げない。reply.rs の doc を参照）。
        let kr_answer = match decision {
            decision::AnswerDecision::Allowed {
                source: decision::AnswerSource::KnownResolution,
                known_resolution_id: Some(kr_id),
                ..
            } => resolutions
                .iter()
                .find(|kr| &kr.id == kr_id)
                .map(|kr| kr.answer.as_str()),
            _ => None,
        };
        let brief = reply::build_reply_brief_with_resolution(decision, hits, kr_answer);
        let system = reply::build_reply_system_prompt(&brief, is_continuation, allowlist);
        let user = reply::build_reply_user_message(question, &brief, history);
        let draft = match drafter
            .draft_reply(
                &system,
                &user,
                self.reply_draft_max_tokens,
                "customer_reply",
            )
            .await
        {
            Ok(draft) => draft,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    kind = ?brief.kind,
                    "customer reply draft generation failed; returning the evaluation without a \
                     draft (customer_reply_draft = null). The decision itself is unaffected"
                );
                return None;
            }
        };

        // [S1-4] 出口ゲート。spec「egress 位置の固定」は AI 生成 draft も人間製 outbound も
        // 同一の egress_gate を通すと定めている（人間製も信頼しない）。**サーバ生成の下書きは
        // その筆頭**であり、ここを迂回すると新経路だけ NG 表現・暗示効能の統制が外れる。
        // block / abstain は黙って null にせず、必ず理由付きで warn する（規約: 握りつぶし禁止）。
        // チャネルは Step 1 の固定値 operator（S1-4 / 遵守事項 4。rmcp_server の
        // operator_emit_context と同じ）。Step 1 の判定は channel 非依存。
        let ctx = egress::EmitContext {
            channel: egress::EmitChannel::Operator,
        };
        let verdict = egress::egress_gate(&draft.text, &ctx, &self.ng);
        match verdict {
            egress::EgressVerdict::Pass => Some(draft),
            ref blocked => {
                // 一致した NG 語は**サーバ自身の辞書由来**（顧客データではない）ので、ログへ
                // 出して安全であり原因特定が一気に速くなる。下書き本文そのものは出さない
                // （NG 表現をログへ転記しない）。文字数だけ添えて切り分けの材料にする。
                let term = match blocked {
                    egress::EgressVerdict::Block { term }
                    | egress::EgressVerdict::Abstain { term } => term.as_str(),
                    egress::EgressVerdict::Pass => "",
                };
                tracing::warn!(
                    verdict = blocked.label(),
                    term,
                    draft_chars = draft.text.chars().count(),
                    kind = ?brief.kind,
                    "customer reply draft was blocked by the egress gate; returning \
                     customer_reply_draft = null. The decision itself is unaffected. Inspect the \
                     manual excerpts or the known_resolution behind this decision — the draft \
                     contained a term the NG dictionary rejects"
                );
                None
            }
        }
    }

    /// record_answer_attempt の入口強制（S1-1 の短絡順序を emit 側でも閉じる）:
    /// draft は「同一 case の最新 evaluate_answerability が Allowed」の場合のみ emit 候補になる。
    /// 判定はサーバが case に永続化した記録と突合する（client の自己申告を信用しない）。
    /// 通過時は、その判定が KR 由来なら kr_id を返す（attempt へのサーバ側引き継ぎ用）。
    pub async fn verify_answer_lineage(
        &self,
        ctx: &RequestContext,
        case_id: &str,
        evaluation_request_id: &str,
    ) -> Result<Option<String>> {
        let attrs = self
            .knowledge()?
            .load_case(&ctx.schema, case_id)
            .await?
            .ok_or_else(|| anyhow!("unknown case_id: {case_id}"))?;
        let last_request_id = attrs
            .get("last_request_id")
            .map(String::as_str)
            .unwrap_or("");
        if last_request_id != evaluation_request_id {
            return Err(anyhow!(
                "evaluation_request_id does not match the latest evaluation of case {case_id}; \
                 call evaluate_answerability first and use its request_id"
            ));
        }
        match attrs.get("last_decision").map(String::as_str) {
            Some("allowed") => Ok(attrs
                .get("last_kr_id")
                .filter(|kr_id| !kr_id.is_empty())
                .cloned()),
            Some("escalate") => Err(anyhow!(
                "the latest evaluation of case {case_id} was an escalation; \
                 drafts may only be attached as reference, not emitted"
            )),
            _ => Err(anyhow!(
                "case {case_id} has no recorded evaluation; call evaluate_answerability first"
            )),
        }
    }

    /// 訂正時の root_cause 切り分け（S1-5）: 正しい根拠がグラフ内に存在したかを再検索で判定。
    /// manual 取得は ctx.manual_schema で分岐する（evaluate と同じ分岐方針）。
    /// LegacySection は従来どおり tools.search_manual を使う（挙動を変えない）。
    pub async fn root_cause_probe(
        &self,
        ctx: &RequestContext,
        corrected_answer: &str,
        tools: &ToolService,
    ) -> Result<rules::RootCause> {
        let best_score: Option<f32> = match ctx.manual_schema {
            crate::config::ManualSchemaKind::ManualV1 => {
                let store = self
                    .manual
                    .as_ref()
                    .ok_or_else(|| anyhow!("manual store not configured"))?;
                let extraction_outcome = self.extractor.extract(corrected_answer).await;
                tracing::debug!(
                    mode = extraction_outcome.mode.as_str(),
                    "root_cause_probe signal extraction mode"
                );
                let signals = extraction_outcome.signals;
                // root_cause_probe は訂正文の再検索であり、意味検索の合成対象は
                // evaluate/search_manual のみ（本タスクのスコープ外・&[] で従来挙動を維持）。
                let hits = store
                    .search(&ctx.schema, corrected_answer, &signals, None, 3, &[])
                    .await?;
                hits.first().map(|h| h.score)
            }
            crate::config::ManualSchemaKind::LegacySection => {
                let hits = tools
                    .search_manual(&ctx.schema, corrected_answer, None, 3)
                    .await?;
                hits.first().map(|h| h.score)
            }
        };
        let found = best_score
            .map(|score| score >= self.thresholds.mid)
            .unwrap_or(false);
        Ok(if found {
            rules::RootCause::RetrievalMiss
        } else {
            rules::RootCause::KnowledgeError
        })
    }

    /// 検索改善キューへの追記（retrieval_miss の受け皿。known_resolution を増やさない）。
    /// async ハンドラから呼ばれるため tokio::fs で非同期 I/O にする（ワーカーをブロックしない）。
    pub async fn enqueue_search_improvement(
        &self,
        ctx: &RequestContext,
        corrected_answer: &str,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        if let Some(parent) = self.queue_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.queue_path)
            .await?;
        let entry = serde_json::json!({
            "request_id": ctx.request_id,
            "schema": ctx.schema,
            "actor": ctx.actor.sub,
            // actor は安定 ID（google-sub:{sub}）で人間には読めないため、
            // エスカレーションを処理する担当者向けに当時の email も併記する。
            "actor_email": ctx.actor.email,
            "corrected_answer": corrected_answer,
            "queued_at": chrono::Utc::now().to_rfc3339(),
        });
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Issue #28 §3.1: 取扱外定型応答も case へ記録し、audit_event_id を発行する
    /// （design doc §3.1「監査: 取扱外応答も case に記録する」）。`req.case_id` が解決できれば
    /// それを使い、できなければ新規採番する（`evaluate()` の新規 case 作成ブロックと同型だが、
    /// signal 累積・decision 属性更新などフル evaluate() の処理は行わない。case ノードの存在確保と
    /// 監査記録だけを行う）。
    ///
    /// **監査記録に失敗しても、この安全ゲートの応答そのものは失敗させない。** 「当社の取扱外
    /// 製品と正直に伝える」という安全性は allowlist 側の決定論ロジックだけで既に成立しており、
    /// それを audit backend（vegapunk）の可用性に依存させると、vegapunk 障害時に**安全な断り
    /// 文言すら返せなくなる**（`draft_customer_reply` が生成失敗を non-fatal に扱っているのと
    /// 同じ設計判断）。ただし監査記録が欠落した事実は運用者が追えなければならないため、
    /// 必ず `tracing::error!` で警告する（握りつぶさない）。
    pub async fn record_out_of_scope_case(
        &self,
        ctx: &RequestContext,
        question: &str,
        case_id: Option<&str>,
    ) -> String {
        match self
            .try_record_out_of_scope_case(ctx, question, case_id)
            .await
        {
            Ok(id) => id,
            Err(err) => {
                let fallback_id = case_id
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("case-{}", uuid::Uuid::new_v4()));
                tracing::error!(
                    error = ?err,
                    request_id = %ctx.request_id,
                    schema = %ctx.schema,
                    case_id = %fallback_id,
                    "failed to record an audit trail for an out-of-scope product reply; the \
                     customer still received the correct out-of-scope message (that safety \
                     property does not depend on audit availability), but this conversation has \
                     NO case/audit record — investigate vegapunk/knowledge connectivity"
                );
                fallback_id
            }
        }
    }

    async fn try_record_out_of_scope_case(
        &self,
        ctx: &RequestContext,
        question: &str,
        case_id: Option<&str>,
    ) -> Result<String> {
        let knowledge = self.knowledge()?;
        let existing = match case_id {
            Some(id) => knowledge.load_case(&ctx.schema, id).await?,
            None => None,
        };
        let resolved_case_id = match (case_id, existing) {
            (Some(id), Some(_)) => id.to_string(),
            _ => {
                let new_id = format!("case-{}", uuid::Uuid::new_v4());
                let attrs: std::collections::HashMap<String, String> = [
                    ("case_id".to_string(), new_id.clone()),
                    ("request_id".to_string(), ctx.request_id.clone()),
                    ("actor".to_string(), ctx.actor.sub.clone()),
                    ("actor_email".to_string(), ctx.actor.email.clone()),
                    ("question".to_string(), question.to_string()),
                    ("created_at".to_string(), chrono::Utc::now().to_rfc3339()),
                ]
                .into_iter()
                .collect();
                knowledge
                    .record(
                        &ctx.schema,
                        "support_case",
                        &new_id,
                        attrs.into_iter().collect(),
                    )
                    .await?;
                new_id
            }
        };
        self.audit_with_nodes(
            ctx,
            "out_of_scope_product",
            None,
            Vec::new(),
            vec![knowledge::harness_node_id(
                &ctx.schema,
                "support_case",
                &resolved_case_id,
            )],
            None,
        )
        .await?;
        Ok(resolved_case_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness_for_test() -> Harness {
        let dir = std::env::temp_dir().join(format!("harness-test-{}", uuid::Uuid::new_v4()));
        // build() と同じく単一の lexicon を normalizer / lexicon / extractor で共有する。
        let lexicon = Arc::new(signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap());
        Harness {
            // config actor ホワイトリスト廃止（authn.rs 参照）に伴い、Authenticator は
            // project schema 一覧のみを受け取る。email 突合はしない。
            authenticator: authn::Authenticator::new(vec!["sivira-cs-demo".to_string()]),
            normalizer: lexicon.clone(),
            // LLM 未設定（enabled = false 相当）→ lexicon 単独の extractor。
            extractor: Arc::new(extraction::HybridExtractor::new(lexicon.clone(), None)),
            lexicon,
            ng: egress::NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":[]}"#)
                .unwrap(),
            worm: Arc::new(audit::WormAuditLog::open(&dir.join("audit.jsonl")).unwrap()),
            knowledge: None,
            thresholds: decision::Thresholds {
                low: 0.6,
                mid: 0.8,
                high: 0.95,
            },
            grading: grading::GradingThresholds {
                promote_approvals: 3,
                promote_approvers: 2,
                promote_max_rejection_rate: 0.2,
                demote_rejections: 2,
            },
            queue_path: dir.join("queue.jsonl"),
            grade_lock: tokio::sync::Mutex::new(()),
            manual: None,
            corpus: None,
            default_route: "triage".to_string(),
            vector_route_enabled: false,
            // 返信文下書きはデモ用で既定 off。テストは判定そのものを見るため常に無効。
            reply_drafter: None,
            reply_draft_max_tokens: 700,
            // VegapunkClient の実接続を要求するため、`manual` / `corpus` と同じ理由で
            // 同期テストヘルパでは構築しない（`product_gate.rs` の非同期テストが別途カバーする）。
            product_gate: None,
        }
    }

    fn test_identity() -> crate::oauth::VerifiedIdentity {
        crate::oauth::VerifiedIdentity {
            sub: "101572111487015263315".to_string(),
            email: "op@sivira.co".to_string(),
        }
    }

    #[test]
    fn begin_produces_request_context_with_enforced_schema() {
        let harness = harness_for_test();
        let ctx = harness
            .begin(
                &test_identity(),
                "sivira-cs-demo",
                crate::config::ManualSchemaKind::LegacySection,
            )
            .expect("begin");
        assert_eq!(ctx.schema, "sivira-cs-demo");
        // F4: actor の主識別子は安定した Google sub 由来（authn.rs 参照）。
        // email は当時の値として別フィールドに載る。
        assert_eq!(ctx.actor.sub, "google-sub:101572111487015263315");
        assert_eq!(ctx.actor.email, "op@sivira.co");
        assert!(!ctx.request_id.is_empty());
    }

    /// W1 回帰: 起票者（attempt.actor_email）と承認者（outcome_actor_email）が別人のとき、
    /// それぞれの email が別フィールドへ入ること。両者が隣接して載るため、承認者側に email が
    /// 無いと起票者の email が承認者のものと誤読される。
    #[test]
    fn outcome_records_approver_email_separately_from_author_email() {
        let attempt: std::collections::HashMap<String, String> = [
            ("attempt_id", "att-001"),
            ("actor", "google-sub:author-sub"),
            ("actor_email", "author@sivira.co"),
            ("draft", "元の回答案"),
            ("known_resolution_id", "kr-001"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let approver = authn::Actor {
            sub: "google-sub:approver-sub".to_string(),
            email: "approver@sivira.co".to_string(),
            role: authn::Role::Supervisor,
            allowed_schemas: vec![],
        };
        let merged = merge_outcome_attributes(
            &attempt,
            "att-001",
            grading::AnswerOutcome::Resolved,
            &approver,
            Some("確認済み"),
        );
        // 起票者の記録は書き換わらない
        assert_eq!(merged["actor"], "google-sub:author-sub");
        assert_eq!(merged["actor_email"], "author@sivira.co");
        // 承認者は安定 ID と email の両方が承認者側フィールドに載る
        assert_eq!(merged["outcome_actor"], "google-sub:approver-sub");
        assert_eq!(merged["outcome_actor_email"], "approver@sivira.co");
        assert_eq!(merged["outcome"], "resolved");
        assert_eq!(merged["outcome_note"], "確認済み");
        // 既存属性（draft / KR 紐づけ）は read-merge-write で保持される
        assert_eq!(merged["draft"], "元の回答案");
        assert_eq!(merged["known_resolution_id"], "kr-001");
    }

    /// 境界: 起票者と承認者が同一人物でも、両フィールドに同じ値が入るだけで破綻しない。
    #[test]
    fn outcome_by_author_themselves_fills_both_email_fields() {
        let attempt: std::collections::HashMap<String, String> = [
            ("actor", "google-sub:same-sub"),
            ("actor_email", "same@sivira.co"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let actor = authn::Actor {
            sub: "google-sub:same-sub".to_string(),
            email: "same@sivira.co".to_string(),
            role: authn::Role::Supervisor,
            allowed_schemas: vec![],
        };
        let merged = merge_outcome_attributes(
            &attempt,
            "att-002",
            grading::AnswerOutcome::WrongAnswer,
            &actor,
            None,
        );
        assert_eq!(merged["actor_email"], "same@sivira.co");
        assert_eq!(merged["outcome_actor_email"], "same@sivira.co");
        assert_eq!(merged["outcome_note"], "");
        assert_eq!(merged["attempt_id"], "att-002");
    }

    // admission 層（Harness::admit_known_resolution）が legacy schema の rationale_text を
    // 拒否することの直接テスト（codex レビュー Suggestion 対応）。
    // legacy schema には Rationale ノード型が無いため、build 側で無視するのではなく
    // ここで fail closed にする必要がある。
    #[test]
    fn admit_known_resolution_rejects_rationale_text_on_legacy_schema() {
        let harness = harness_for_test();
        let ctx = RequestContext {
            actor: authn::Actor {
                sub: "sup-001".to_string(),
                email: "sup@sivira.co".to_string(),
                role: authn::Role::Supervisor,
                allowed_schemas: vec!["sivira-cs-demo".to_string()],
            },
            scope: scope::AccessScope {
                allowed_schemas: vec!["sivira-cs-demo".to_string()],
                max_sensitivity: None,
                label_allowlist: None,
            },
            schema: "sivira-cs-demo".to_string(),
            request_id: "req-test".to_string(),
            manual_schema: crate::config::ManualSchemaKind::LegacySection,
        };
        let err = harness
            .admit_known_resolution(&ctx, &["mold".to_string()], "answer", Some("because"), &[])
            .expect_err("legacy schema must reject rationale_text");
        assert!(
            err.to_string().contains("rationale_text"),
            "unexpected error: {err}"
        );
        // 根拠アンカーゼロ（section なし・rationale なし）も拒否（監査可能性）
        let err = harness
            .admit_known_resolution(&ctx, &["mold".to_string()], "answer", None, &[])
            .expect_err("KR without any evidence anchor must be rejected");
        assert!(
            err.to_string().contains("evidence anchor"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn begin_rejects_out_of_scope_project() {
        let harness = harness_for_test();
        assert!(harness
            .begin(
                &test_identity(),
                "other-tenant",
                crate::config::ManualSchemaKind::LegacySection,
            )
            .is_err());
    }

    // ---- 下書き生成と出口ゲートの**配線**（spec S1-4「egress 位置の固定」）----
    //
    // 以下 3 件は `draft_customer_reply` が生成結果を実際に `egress_gate` へ通していることを、
    // stub LLM に下書きを喋らせて検証する。**ゲート単体の判定テストではない**
    // （それは `egress.rs` の tests と `reply.rs` の
    // `egress_gate_blocks_and_abstains_on_ng_terms` が持つ）。
    //
    // ここを間接的な検証（ゲート単体の呼び出し）で済ませると、`draft_customer_reply` から
    // `egress_gate` の呼び出しを外しても**全テストが緑のまま NG 表現の統制だけが外れる**。

    /// 本番（Cloud Run）が読むのと同じ NG 辞書。**推測の NG 語をテストに書かない**ため、
    /// 実データを読み、そこから語を取る（`server/config.cloudrun.toml` の
    /// `ng_dictionary_path = "data/urtect/ng-dictionary.json"`）。
    /// パスは cwd 非依存にする（`cargo test` の起動位置に依存させない）。
    fn production_ng_dictionary() -> egress::NgDictionary {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/urtect/ng-dictionary.json"
        ));
        egress::NgDictionary::from_path(path).expect("production ng dictionary must load and parse")
    }

    fn operator_emit_context() -> egress::EmitContext {
        egress::EmitContext {
            channel: egress::EmitChannel::Operator,
        }
    }

    /// stub LLM に `draft_text` をそのまま返させ、`draft_customer_reply` の結果と、stub が
    /// 実際に受け取った生リクエスト（`RequestLog`）を返す。
    ///
    /// `is_continuation` は `draft_customer_reply` へそのまま渡す。呼び出し側が `evaluate()` →
    /// `draft_customer_reply()` → `build_reply_system_prompt()` の素通し配線を検証できるよう、
    /// stub 到達済みの生リクエスト（system prompt を含む）を返す
    /// （`clarify.rs::draft_clarify_question_via_stub` と同じパターン）。
    ///
    /// `AnthropicClient` のフィールドは `llm.rs` で private なので `from_config` 経由で組む。
    /// API キーは env `CS_SUPPORT_LLM_API_KEY` が優先されるが、未設定の環境でも構築できるよう
    /// 一時ファイルを置く（stub は鍵を検証しない。ここで必要なのは「鍵が解決できて client が
    /// 構築されること」だけ）。env を書き換えないのは、並行テストと競合させないため。
    async fn draft_customer_reply_via_stub(
        draft_text: &str,
        is_continuation: bool,
    ) -> (
        Option<crate::llm::ReplyDraft>,
        crate::llm::test_support::RequestLog,
    ) {
        let body = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": draft_text}],
        })
        .to_string();
        let (endpoint, log) = crate::llm::test_support::spawn_messages_stub(body).await;

        let dir = std::env::temp_dir().join(format!("harness-reply-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let key_path = dir.join("llm-api-key");
        std::fs::write(&key_path, "test-key\n").expect("write api key file");
        let drafter = crate::llm::AnthropicClient::from_config(&crate::config::LlmConfig {
            enabled: true,
            endpoint,
            api_key_file: Some(key_path.to_string_lossy().to_string()),
            ..Default::default()
        })
        .expect("llm client must build from the stub config")
        .expect("enabled = true with a readable key file must yield a client");

        let harness = Harness {
            reply_drafter: Some(drafter),
            ng: production_ng_dictionary(),
            ..harness_for_test()
        };
        let decision = decision::AnswerDecision::Allowed {
            source: decision::AnswerSource::Manual,
            evidence_section_keys: vec!["sec-a".to_string()],
            known_resolution_id: None,
            stakes: decision::Stakes::Low,
            threshold: 0.6,
        };
        let hits = vec![SectionHit {
            section_key: "sec-a".to_string(),
            title_ja: "タイトル".to_string(),
            body_ja: Some("マニュアル本文".to_string()),
            body_en: None,
            translation_status: None,
            breadcrumb: Vec::new(),
            score: 0.9,
            source_url: None,
        }];
        // Issue #28 codex レビュー採用1: allowlist は `evaluate()` が判定前に取得済みのものを
        // 渡す設計になったため、`draft_customer_reply` 自体はもう schema も ProductGate も
        // 必要としない。このテストはその関数を直接呼ぶだけなので、allowlist を直接組み立てて
        // 渡す（実接続は張らない）。
        let allowlist = product_gate::ProductAllowlist::from_models(vec!["ADC-V724".to_string()]);
        let draft = harness
            .draft_customer_reply(
                "カメラが反応しません",
                &decision,
                &hits,
                &[],
                &[],
                is_continuation,
                &allowlist,
            )
            .await;
        (draft, log)
    }

    /// 「NG 表現を含まない下書きなら通る」ことは、**下記 2 件の偽陽性を潰すために必須**。
    /// これが無いと、`reply_drafter` を無効化しただけ（＝そもそも生成されない）でも
    /// 「ゲートが効いた」ように見えて 2 件とも緑になる。
    #[tokio::test]
    async fn a_clean_generated_draft_is_returned_as_is() {
        const CLEAN: &str =
            "お問い合わせありがとうございます。担当部署より改めてご連絡いたします。";
        // 前提の明示: この文面は NG 辞書に触れていない（辞書が育って触れた場合は
        // ここが落ち、テスト本体の失敗と区別できる）。
        assert!(
            matches!(
                egress::egress_gate(CLEAN, &operator_emit_context(), &production_ng_dictionary()),
                egress::EgressVerdict::Pass
            ),
            "precondition: pick a draft text that the current NG dictionary passes"
        );
        let (draft, _log) = draft_customer_reply_via_stub(CLEAN, false).await;
        let draft = draft.expect("a draft with no NG term must survive the gate");
        // 生成結果がそのまま返ること。null でないだけでなく**本文が一致する**ことを見るのは、
        // 下書きが実際に stub から流れてきた証拠にするため。
        assert_eq!(draft.text, CLEAN);
        assert!(!draft.truncated);
    }

    #[tokio::test]
    async fn a_generated_draft_with_a_blocked_ng_term_is_dropped() {
        let ng = production_ng_dictionary();
        let term = ng
            .block_terms
            .first()
            .expect("the production NG dictionary must have at least one block term")
            .clone();
        let drafted =
            format!("お問い合わせありがとうございます。本製品は「{term}」とご案内しております。");
        assert!(
            matches!(
                egress::egress_gate(&drafted, &operator_emit_context(), &ng),
                egress::EgressVerdict::Block { .. }
            ),
            "precondition: the term taken from the dictionary must actually block"
        );
        let (draft, _log) = draft_customer_reply_via_stub(&drafted, false).await;
        assert!(
            draft.is_none(),
            "draft_customer_reply must run the generated draft through egress_gate and drop a \
             blocked one (customer_reply_draft = null)"
        );
    }

    #[tokio::test]
    async fn a_generated_draft_with_an_abstain_ng_term_is_dropped() {
        // block だけを特別扱いする実装（abstain を素通し）を許さない。
        let ng = production_ng_dictionary();
        let term = ng
            .abstain_terms
            .first()
            .expect("the production NG dictionary must have at least one abstain term")
            .clone();
        let drafted =
            format!("お問い合わせありがとうございます。本製品は「{term}」とご案内しております。");
        assert!(
            matches!(
                egress::egress_gate(&drafted, &operator_emit_context(), &ng),
                egress::EgressVerdict::Abstain { .. }
            ),
            "precondition: the term taken from the dictionary must actually abstain"
        );
        let (draft, _log) = draft_customer_reply_via_stub(&drafted, false).await;
        assert!(
            draft.is_none(),
            "abstain is 'do not emit' too; the draft must be dropped, not passed through"
        );
    }

    /// design doc §3 配線テスト: `is_continuation` が `evaluate()` → `draft_customer_reply()` →
    /// `build_reply_system_prompt()` まで素通しされていることを確認する。純関数の単体テスト
    /// （`reply.rs` の `system_prompt_adds_continuation_opener_rule_when_a_continuation` 等）
    /// だけでは、呼び出し側（`draft_customer_reply`）がフラグを握り潰しても（例: 固定値
    /// `false` を渡す退行）全テストが緑のままになる。stub に実際に届いた生リクエストを見て
    /// 配線そのものを確認する。
    #[tokio::test]
    async fn draft_customer_reply_forwards_is_continuation_true_to_the_system_prompt() {
        let (_draft, log) = draft_customer_reply_via_stub("本文です。", true).await;
        let requests = log.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "stub に実際にリクエストが届いていること（配線を主張する前提）"
        );
        assert!(
            requests[0].contains("定型オープナー"),
            "is_continuation = true のとき CONTINUATION_OPENER_RULE 由来の制約が \
             system prompt（stub への生リクエスト）に含まれていること"
        );
    }

    /// 対照テスト（false-positive 防止）: `is_continuation = false` のときは含まれないこと。
    /// これが無いと「常に CONTINUATION_OPENER_RULE を含める」実装でも上のテストだけでは緑になる。
    #[tokio::test]
    async fn draft_customer_reply_omits_continuation_rule_when_not_a_continuation() {
        let (_draft, log) = draft_customer_reply_via_stub("本文です。", false).await;
        let requests = log.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "stub に実際にリクエストが届いていること（配線を主張する前提）"
        );
        assert!(!requests[0].contains("定型オープナー"));
    }

    // ---- clarification_allowed 契約テスト（会話フロー v1.1 design doc §2・§8） ----
    //
    // Critical 1 の修正: 以前はここに本体 `matches!` 式の複製ヘルパーがあり、本体を書き換えても
    // テストが追随して緑になり続ける（退行を検出できない）状態だった。いまは本体の
    // `clarification_allowed()`（本ファイル冒頭のモジュールレベル関数）をそのまま呼ぶ。

    fn escalate_for_contract_test(
        layer: u8,
        reason: decision::EscalateReason,
    ) -> decision::AnswerDecision {
        decision::AnswerDecision::Escalate {
            reason,
            layer,
            route_to: "triage".to_string(),
            disclosure_scope: decision::DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
        }
    }

    #[test]
    fn clarification_is_denied_for_layer1_and_layer2_escalations() {
        // 第1層（明示エスカレーションルール）・第2層（禁止ドメイン）は実際の `decide()` では
        // 常に `RegulatedOrSafety` を返す（spec に明記）。第3層の reason 網羅としての価値が
        // あるため、手組み Escalate に対する本テストは残す（`decide()` 経由の版は下に別途置く）。
        let layer1 = escalate_for_contract_test(1, decision::EscalateReason::RegulatedOrSafety);
        assert!(
            !clarification_allowed(&layer1),
            "layer 1 escalation must never allow clarification"
        );

        let layer2 = escalate_for_contract_test(2, decision::EscalateReason::RegulatedOrSafety);
        assert!(
            !clarification_allowed(&layer2),
            "layer 2 escalation must never allow clarification"
        );
    }

    #[test]
    fn clarification_is_allowed_for_layer3_gray() {
        let insufficient_directness =
            escalate_for_contract_test(3, decision::EscalateReason::InsufficientDirectness);
        assert!(
            clarification_allowed(&insufficient_directness),
            "layer 3 InsufficientDirectness must allow clarification"
        );

        let unknown_added_signal =
            escalate_for_contract_test(3, decision::EscalateReason::UnknownAddedSignal);
        assert!(
            clarification_allowed(&unknown_added_signal),
            "layer 3 UnknownAddedSignal must allow clarification"
        );
    }

    // ---- clarification_allowed 契約テスト: decide() の実出力を通す版（Critical 1） ----
    //
    // 上の 2 テストは手組みの `Escalate` に対する reason 網羅であり、`decide()` 自体の配線
    // （第1・2層が本当に layer 1/2 の Escalate を返すか、第3層グレーが本当に
    // InsufficientDirectness/UnknownAddedSignal を返すか）までは見ていない。ここでは
    // `decision::decide()` を実際に通した出力に対して `clarification_allowed()` を検証する
    // （plan Task 3 Step 1 が要求する退行防止テスト）。入力の組み立ては `decision.rs` の
    // `mod tests` にある layer1/layer2/layer3 系テストの入力例を踏襲する。

    fn contract_test_signals(values: &[&str]) -> signal::SignalSet {
        values.iter().map(|v| signal::Signal::new(*v)).collect()
    }

    fn contract_test_thresholds() -> decision::Thresholds {
        decision::Thresholds {
            low: 0.6,
            mid: 0.8,
            high: 0.95,
        }
    }

    fn contract_test_calm_stakes() -> decision::StakesInput {
        decision::StakesInput {
            mandatory_domain_near: false,
            ng_near_hit: false,
            hazard_signal_count: 0,
        }
    }

    fn contract_test_kr(id: &str, set: &[&str]) -> rules::KnownResolution {
        rules::KnownResolution {
            id: id.to_string(),
            signal_set: contract_test_signals(set),
            applicability: "全ロット".to_string(),
            answer: "answer".to_string(),
            source_authority: rules::SourceAuthority::Authoritative,
            root_cause: rules::RootCause::KnowledgeError,
            grade: rules::Grade::ApprovalRequired,
            approval_count: 0,
            rejection_count: 0,
            approver_set: Vec::new(),
            origin: "test".to_string(),
            binding: rules::Binding::Advisory,
            registration_trigger: "single_ruling".to_string(),
            knowledge_class: "commercial".to_string(),
            outcome_ref: Vec::new(),
        }
    }

    #[test]
    fn decide_layer1_escalation_denies_clarification() {
        // decision.rs::layer1_short_circuits_everything と同じ入力形。
        let rules = vec![rules::EscalationRule {
            id: "r1".to_string(),
            condition: contract_test_signals(&["post_ingestion_symptom"]),
            route: "safety_team".to_string(),
            owner: None,
            binding: rules::Binding::Mandatory,
        }];
        let resolutions = vec![contract_test_kr("kr1", &["post_ingestion_symptom"])];
        let q = contract_test_signals(&["post_ingestion_symptom"]);
        let d = decision::decide(&decision::DecisionInput {
            question_signals: &q,
            question_raw: "質問",
            rules: &rules,
            domains: &[],
            resolutions: &resolutions,
            best_manual_score: Some(1.0),
            best_manual_sections: &[],
            stakes_input: contract_test_calm_stakes(),
            thresholds: &contract_test_thresholds(),
            default_route: "triage",
        });
        assert!(
            matches!(d, decision::AnswerDecision::Escalate { layer: 1, .. }),
            "precondition: decide() must actually take the layer 1 branch, got {d:?}"
        );
        assert!(
            !clarification_allowed(&d),
            "layer 1 escalation from decide() must never allow clarification"
        );
    }

    #[test]
    fn decide_layer2_escalation_denies_clarification() {
        // decision.rs::layer2_blocks_before_layer3 と同じ入力形。
        let domains = vec![rules::ProhibitedDomain {
            id: "d1".to_string(),
            domain_signals: contract_test_signals(&["skin_irritation"]),
            text_patterns: Vec::new(),
            route: "derm_liaison".to_string(),
            binding: rules::Binding::Mandatory,
        }];
        let resolutions = vec![contract_test_kr("kr1", &["skin_irritation"])];
        let q = contract_test_signals(&["skin_irritation"]);
        let d = decision::decide(&decision::DecisionInput {
            question_signals: &q,
            question_raw: "質問",
            rules: &[],
            domains: &domains,
            resolutions: &resolutions,
            best_manual_score: Some(1.0),
            best_manual_sections: &[],
            stakes_input: contract_test_calm_stakes(),
            thresholds: &contract_test_thresholds(),
            default_route: "triage",
        });
        assert!(
            matches!(d, decision::AnswerDecision::Escalate { layer: 2, .. }),
            "precondition: decide() must actually take the layer 2 branch, got {d:?}"
        );
        assert!(
            !clarification_allowed(&d),
            "layer 2 escalation from decide() must never allow clarification"
        );
    }

    #[test]
    fn decide_layer3_insufficient_directness_allows_clarification() {
        // decision.rs::high_stakes_raises_threshold_and_escalates と同系の入力
        // （signal 無し・best_manual_score がしきい値未満）。
        let q = signal::SignalSet::new();
        let d = decision::decide(&decision::DecisionInput {
            question_signals: &q,
            question_raw: "質問",
            rules: &[],
            domains: &[],
            resolutions: &[],
            best_manual_score: Some(0.1),
            best_manual_sections: &[],
            stakes_input: contract_test_calm_stakes(),
            thresholds: &contract_test_thresholds(),
            default_route: "triage",
        });
        assert!(
            matches!(
                d,
                decision::AnswerDecision::Escalate {
                    layer: 3,
                    reason: decision::EscalateReason::InsufficientDirectness,
                    ..
                }
            ),
            "precondition: decide() must actually return layer 3 InsufficientDirectness, got {d:?}"
        );
        assert!(
            clarification_allowed(&d),
            "layer 3 InsufficientDirectness from decide() must allow clarification"
        );
    }

    #[test]
    fn decide_layer3_unknown_added_signal_allows_clarification() {
        // decision.rs::layer3_added_signal_escalates_with_unknown_added_signal と同じ入力形
        // （KR の signal_set の部分集合に一致するが、未知の追加 signal が残る）。
        let resolutions = vec![contract_test_kr("kr1", &["discoloration"])];
        let q = contract_test_signals(&["discoloration", "mold"]);
        let d = decision::decide(&decision::DecisionInput {
            question_signals: &q,
            question_raw: "質問",
            rules: &[],
            domains: &[],
            resolutions: &resolutions,
            best_manual_score: Some(0.1),
            best_manual_sections: &[],
            stakes_input: contract_test_calm_stakes(),
            thresholds: &contract_test_thresholds(),
            default_route: "triage",
        });
        assert!(
            matches!(
                d,
                decision::AnswerDecision::Escalate {
                    layer: 3,
                    reason: decision::EscalateReason::UnknownAddedSignal,
                    ..
                }
            ),
            "precondition: decide() must actually return layer 3 UnknownAddedSignal, got {d:?}"
        );
        assert!(
            clarification_allowed(&d),
            "layer 3 UnknownAddedSignal from decide() must allow clarification"
        );
    }

    // ---- filter_out_of_scope_hits（Issue #28 codex レビュー採用1・採用4） ----

    fn scoped_hit(section_key: &str, title_ja: &str, body: &str) -> SectionHit {
        scoped_hit_with_bodies(section_key, title_ja, Some(body), None)
    }

    /// `body_ja` / `body_en` を個別に指定できる版（修正1の回帰テスト用: 翻訳が `missing` /
    /// `stale` の section を模して `body_ja = Some("")` かつ `body_en` にだけ本文がある hit を
    /// 再現するために使う）。
    fn scoped_hit_with_bodies(
        section_key: &str,
        title_ja: &str,
        body_ja: Option<&str>,
        body_en: Option<&str>,
    ) -> SectionHit {
        SectionHit {
            section_key: section_key.to_string(),
            title_ja: title_ja.to_string(),
            body_ja: body_ja.map(str::to_string),
            body_en: body_en.map(str::to_string),
            translation_status: None,
            breadcrumb: Vec::new(),
            score: 0.9,
            source_url: None,
        }
    }

    fn scoped_allowlist() -> product_gate::ProductAllowlist {
        product_gate::ProductAllowlist::from_models(vec!["ADC-V724".to_string()])
    }

    #[test]
    fn filter_out_of_scope_hits_excludes_a_hit_that_mentions_only_out_of_scope_models() {
        let allow = scoped_allowlist();
        let hits = vec![scoped_hit(
            "sec-a",
            "タイトル",
            "ADC-VDB101の初期設定手順です",
        )];
        assert!(filter_out_of_scope_hits(hits, &allow).is_empty());
    }

    #[test]
    fn filter_out_of_scope_hits_keeps_a_hit_that_also_mentions_an_in_scope_model() {
        let allow = scoped_allowlist();
        let hits = vec![scoped_hit(
            "sec-a",
            "タイトル",
            "ADC-V724とADC-VDB101は共通の手順です",
        )];
        let filtered = filter_out_of_scope_hits(hits, &allow);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].section_key, "sec-a");
    }

    #[test]
    fn filter_out_of_scope_hits_keeps_a_hit_with_no_model_mention() {
        // 型番言及の無い汎用材料（Wi-Fi 再接続等）は通す（design doc §3.2）。
        let allow = scoped_allowlist();
        let hits = vec![scoped_hit("sec-a", "タイトル", "Wi-Fiの再接続手順です")];
        assert_eq!(filter_out_of_scope_hits(hits, &allow).len(), 1);
    }

    #[test]
    fn filter_out_of_scope_hits_excludes_when_the_out_of_scope_model_is_only_in_the_title() {
        // codex レビュー採用4: 検査対象に title_ja も含める。body には型番言及が無く
        // title_ja にのみ取扱外型番がある節も除外されること。
        let allow = scoped_allowlist();
        let hits = vec![scoped_hit(
            "sec-a",
            "ADC-VDB101の初期設定",
            "この手順は共通の内容です",
        )];
        assert!(filter_out_of_scope_hits(hits, &allow).is_empty());
    }

    #[test]
    fn filter_out_of_scope_hits_returns_empty_when_every_candidate_is_out_of_scope_only() {
        let allow = scoped_allowlist();
        let hits = vec![
            scoped_hit("sec-a", "タイトル", "ADC-VDB101の設定"),
            scoped_hit("sec-b", "タイトル", "ADC-VDB201の設定"),
        ];
        assert!(filter_out_of_scope_hits(hits, &allow).is_empty());
    }

    #[test]
    fn filter_out_of_scope_hits_detects_a_mention_far_into_a_long_body() {
        // `filter_out_of_scope_hits` は `evaluate()` の中で、`reply::build_reply_brief_with_
        // resolution`（本文を `MAX_EXCERPT_CHARS` で truncate する箇所）より前の生本文に対して
        // 動く。ここで見る本文がどれだけ長くても、切り捨てとは無関係に全文を検査できることを
        // 固定する（切り捨て位置に依存する実装への回帰を防ぐ）。
        let filler = "あ".repeat(3_000);
        let body = format!("{filler}ADC-VDB101の設定です");
        let allow = scoped_allowlist();
        let hits = vec![scoped_hit("sec-a", "タイトル", &body)];
        assert!(
            filter_out_of_scope_hits(hits, &allow).is_empty(),
            "an out-of-scope mention far into a long body must still trigger exclusion"
        );
    }

    #[test]
    fn filter_out_of_scope_hits_excludes_when_japanese_body_is_empty_and_english_body_has_only_out_of_scope_model(
    ) {
        // 修正1の回帰テスト（Critical、2026-08-14）。翻訳が `missing` / `stale` の section は
        // `body_ja` 属性が空文字のまま `body_en` にだけ本文を持つ（design doc の「言語と翻訳
        // 方針」参照）。`body_ja.as_deref().or(body_en.as_deref())` のように一方だけを選ぶ実装は
        // `body_ja = Some("")` を有効値として選んでしまい、`body_en` を一切検査しない fail-open
        // になっていた。修正前のコードではこのテストは失敗する（ADC-VDB101 が検査対象に入らず
        // 除外されない）。
        let allow = scoped_allowlist();
        let hits = vec![scoped_hit_with_bodies(
            "sec-a",
            "初期設定",
            Some(""),
            Some("Setup instructions for ADC-VDB101"),
        )];
        assert!(filter_out_of_scope_hits(hits, &allow).is_empty());
    }

    #[test]
    fn filter_out_of_scope_hits_keeps_when_japanese_body_is_empty_and_english_body_mentions_an_in_scope_model(
    ) {
        let allow = scoped_allowlist();
        let hits = vec![scoped_hit_with_bodies(
            "sec-a",
            "初期設定",
            Some(""),
            Some("Setup instructions for ADC-V724"),
        )];
        let filtered = filter_out_of_scope_hits(hits, &allow);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].section_key, "sec-a");
    }

    /// **Critical 1 の回帰防止（判定レベルの検証）。**
    ///
    /// 修正前は、材料の取扱外除外が `decision::decide()` の**後**（`reply::
    /// build_reply_brief_with_resolution`）でしか効かず、hits 全件が取扱外型番のみを言及して
    /// いても、フィルタ前の `best_manual_score` / `best_manual_sections` がそのまま
    /// `decide()` へ渡っていた。高スコアの取扱外記事が検索に掛かると、判定は `Allowed` の
    /// まま確定し、その後ろで下書きの材料だけが 0 件になるという矛盾が起きていた。
    ///
    /// ここでは `evaluate()` が実際に行う計算手順（`section_hits.first()` →
    /// `best_manual_score` / `best_manual_sections` → `decision::decide()`）を、
    /// フィルタ適用の有無それぞれについて再現する。フィルタ無し（対照・修正前の挙動）では
    /// `Allowed` になること、フィルタ有り（修正後の実装）では `Allowed` にならないことの
    /// 両方を確認することで、「フィルタを判定より前に置いたこと自体が結果を変える」ことを
    /// 直接証明する。
    #[test]
    fn all_hits_out_of_scope_only_prevents_a_manual_allowed_decision() {
        let allow = scoped_allowlist();
        let hits = vec![scoped_hit(
            "sec-a",
            "ADC-VDB101の初期設定",
            "ADC-VDB101の初期設定手順です",
        )];

        // 対照（フィルタ無し）: 修正前の実装はこの経路を通っていたため、高スコアの hit が
        // 1 件でもあれば Allowed になっていたことをまず確認する。
        let unfiltered_best_score = hits.first().map(|h| h.score);
        let unfiltered_sections: Vec<String> = hits.iter().map(|h| h.section_key.clone()).collect();
        let d_unfiltered = decision::decide(&decision::DecisionInput {
            question_signals: &contract_test_signals(&[]),
            question_raw: "ADC-VDB101の設定を教えてください",
            rules: &[],
            domains: &[],
            resolutions: &[],
            best_manual_score: unfiltered_best_score,
            best_manual_sections: &unfiltered_sections,
            stakes_input: contract_test_calm_stakes(),
            thresholds: &contract_test_thresholds(),
            default_route: "triage",
        });
        assert!(
            matches!(d_unfiltered, decision::AnswerDecision::Allowed { .. }),
            "precondition: without the Issue #28 fix, a single high-scoring out-of-scope-only \
             hit alone would already be Allowed, got {d_unfiltered:?}"
        );

        // 修正後（フィルタ有り）: `evaluate()` と同じ順序で filter_out_of_scope_hits を先に
        // 通すと、全除外により hits が空になり、Allowed にならない。
        let filtered = filter_out_of_scope_hits(hits, &allow);
        assert!(
            filtered.is_empty(),
            "precondition: all hits must be excluded"
        );
        let best_manual_score = filtered.first().map(|h| h.score);
        let best_manual_sections: Vec<String> =
            filtered.iter().map(|h| h.section_key.clone()).collect();
        let d = decision::decide(&decision::DecisionInput {
            question_signals: &contract_test_signals(&[]),
            question_raw: "ADC-VDB101の設定を教えてください",
            rules: &[],
            domains: &[],
            resolutions: &[],
            best_manual_score,
            best_manual_sections: &best_manual_sections,
            stakes_input: contract_test_calm_stakes(),
            thresholds: &contract_test_thresholds(),
            default_route: "triage",
        });
        assert!(
            !matches!(d, decision::AnswerDecision::Allowed { .. }),
            "hits that mention only out-of-scope models must not survive into an Allowed \
             decision once the Issue #28 fix filters them out before decide(): {d:?}"
        );
    }

    // ---- CaseConvState（会話フロー v1.1 design doc §6） ----

    #[test]
    fn conv_state_from_attrs_defaults_when_attributes_are_missing() {
        // 古い case（この 4 属性を持たない）を読んでもエラーにせず既定値に倒す（後方互換）。
        let attrs = std::collections::HashMap::new();
        let state = conv_state_from_attrs(&attrs);
        assert_eq!(
            state,
            CaseConvState {
                clarify_turns: 0,
                awaiting_time_pref: false,
                time_pref_false_count: 0,
                preferred_contact_time: None,
                time_pref_extraction_error_count: 0,
            }
        );
    }

    #[test]
    fn conv_state_from_attrs_parses_present_values() {
        let attrs: std::collections::HashMap<String, String> = [
            ("clarify_turns".to_string(), "2".to_string()),
            ("awaiting_time_pref".to_string(), "true".to_string()),
            ("time_pref_false_count".to_string(), "1".to_string()),
            (
                "preferred_contact_time".to_string(),
                "平日午後（対応時間外の希望）".to_string(),
            ),
            (
                "time_pref_extraction_error_count".to_string(),
                "2".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let state = conv_state_from_attrs(&attrs);
        assert_eq!(state.clarify_turns, 2);
        assert!(state.awaiting_time_pref);
        assert_eq!(state.time_pref_false_count, 1);
        assert_eq!(
            state.preferred_contact_time.as_deref(),
            Some("平日午後（対応時間外の希望）")
        );
        assert_eq!(state.time_pref_extraction_error_count, 2);
    }

    #[test]
    fn conv_state_from_attrs_defaults_time_pref_extraction_error_count_when_missing() {
        // 古い case・time_pref_extraction_error_count 追加前の case のどちらもこのキーを
        // 持たない。欠落は 0 に倒す（他の 3 属性と同じ後方互換の規律）。
        let attrs: std::collections::HashMap<String, String> =
            [("clarify_turns".to_string(), "1".to_string())]
                .into_iter()
                .collect();
        let state = conv_state_from_attrs(&attrs);
        assert_eq!(state.time_pref_extraction_error_count, 0);
    }

    #[test]
    fn conv_state_from_attrs_treats_empty_preferred_contact_time_as_none() {
        let attrs: std::collections::HashMap<String, String> =
            [("preferred_contact_time".to_string(), "".to_string())]
                .into_iter()
                .collect();
        let state = conv_state_from_attrs(&attrs);
        assert_eq!(state.preferred_contact_time, None);
    }

    #[test]
    fn merge_conv_state_attributes_preserves_unrelated_existing_keys() {
        // read-merge-write: 会話状態と無関係な既存属性（question / last_decision 等）は消えない。
        let existing: std::collections::HashMap<String, String> = [
            ("question".to_string(), "元の質問".to_string()),
            ("last_decision".to_string(), "escalate".to_string()),
        ]
        .into_iter()
        .collect();
        let state = CaseConvState {
            clarify_turns: 1,
            awaiting_time_pref: true,
            time_pref_false_count: 0,
            preferred_contact_time: None,
            time_pref_extraction_error_count: 2,
        };
        let merged = merge_conv_state_attributes(&existing, &state);
        assert_eq!(merged.get("question").map(String::as_str), Some("元の質問"));
        assert_eq!(
            merged.get("last_decision").map(String::as_str),
            Some("escalate")
        );
        assert_eq!(merged.get("clarify_turns").map(String::as_str), Some("1"));
        assert_eq!(
            merged.get("awaiting_time_pref").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            merged.get("time_pref_false_count").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            merged.get("preferred_contact_time").map(String::as_str),
            Some("")
        );
        assert_eq!(
            merged
                .get("time_pref_extraction_error_count")
                .map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn merge_conv_state_attributes_overwrites_previous_conv_state_values() {
        let existing: std::collections::HashMap<String, String> = [
            ("clarify_turns".to_string(), "3".to_string()),
            ("awaiting_time_pref".to_string(), "true".to_string()),
            ("time_pref_false_count".to_string(), "1".to_string()),
            ("preferred_contact_time".to_string(), "旧い希望".to_string()),
        ]
        .into_iter()
        .collect();
        // エスカレーション応答送信時のリセット相当（design doc §3）。
        let state = CaseConvState {
            clarify_turns: 0,
            awaiting_time_pref: true,
            time_pref_false_count: 0,
            preferred_contact_time: None,
            time_pref_extraction_error_count: 0,
        };
        let merged = merge_conv_state_attributes(&existing, &state);
        assert_eq!(merged.get("clarify_turns").map(String::as_str), Some("0"));
        assert_eq!(
            merged.get("preferred_contact_time").map(String::as_str),
            Some("")
        );
        assert_eq!(
            merged
                .get("time_pref_extraction_error_count")
                .map(String::as_str),
            Some("0"),
            "自動解除時は time_pref_false_count と同じく 0 へリセットされる"
        );
    }

    #[test]
    fn conv_state_round_trips_through_merge_and_parse() {
        for state in [
            CaseConvState {
                clarify_turns: 0,
                awaiting_time_pref: false,
                time_pref_false_count: 0,
                preferred_contact_time: None,
                time_pref_extraction_error_count: 0,
            },
            CaseConvState {
                clarify_turns: 3,
                awaiting_time_pref: true,
                time_pref_false_count: 2,
                preferred_contact_time: Some("平日夕方（対応時間外の希望）".to_string()),
                time_pref_extraction_error_count: 1,
            },
        ] {
            let merged = merge_conv_state_attributes(&std::collections::HashMap::new(), &state);
            let round_tripped = conv_state_from_attrs(&merged);
            assert_eq!(round_tripped, state);
        }
    }

    // ---- require_existing_case_attrs（Warning 2 の回帰防止） ----

    #[test]
    fn require_existing_case_attrs_passes_through_when_case_exists() {
        let attrs: std::collections::HashMap<String, String> =
            [("question".to_string(), "元の質問".to_string())]
                .into_iter()
                .collect();
        let result = require_existing_case_attrs(Some(attrs.clone()), "case-1", "urtect");
        assert_eq!(result.unwrap(), attrs);
    }

    #[test]
    fn require_existing_case_attrs_errors_instead_of_defaulting_when_case_is_missing() {
        // 修正前は `unwrap_or_default()` で空の HashMap に倒し、`case_id` 属性なしの
        // support_case ノードを書いていた（Warning 2）。そのノードは `load_case` /
        // `load_cases` のどちらからも二度と見えなくなる。いまは黙って倒さず Err にする。
        let result = require_existing_case_attrs(None, "case-missing", "urtect");
        let err = result.expect_err("missing case must be an error, not a silent default");
        let message = err.to_string();
        // 運用者が次に何を見ればよいか分かる情報: どの case か・どの schema か。
        assert!(message.contains("case-missing"), "{message}");
        assert!(message.contains("urtect"), "{message}");
    }
}
