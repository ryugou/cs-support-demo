pub mod audit;
pub mod authn;
pub mod correction;
pub mod decision;
pub mod egress;
pub mod extraction;
pub mod grading;
pub mod knowledge;
pub mod reply;
pub mod rules;
pub mod scope;
pub mod signal;

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
}

/// 参考情報として返す過去事例の最小ビュー（S1-1 取得段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedCase {
    pub case_id: String,
    pub question: String,
    pub last_decision: String,
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
            )),
            corpus: Some(corpus),
            default_route: config.harness.default_escalation_route.clone(),
            vector_route_enabled: config.harness.vector_route_enabled,
            reply_drafter,
            reply_draft_max_tokens: config.harness.customer_reply_draft_max_tokens,
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

    /// tool handler から材料ストアへアクセスするための入口（判定は持たない）。
    pub fn store(&self) -> Result<&knowledge::KnowledgeStore> {
        self.knowledge()
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
    pub async fn evaluate(
        &self,
        ctx: &RequestContext,
        question: &str,
        product_key: Option<&str>,
        case_id: Option<&str>,
        tools: &ToolService,
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
        // 既存 case_id は存在を検証する（未知の id への orphan edge 追加を防ぐ）。
        // case の全属性を手元に保持し、後段の判定記録は read-merge-write で全属性を再送する
        // （UpsertNodes が全属性置換セマンティクスでも既存属性を失わない）。
        let (case_id, prior_signals, mut case_attrs) = match case_id {
            Some(id) => {
                let attrs = knowledge
                    .load_case(&ctx.schema, id)
                    .await?
                    .ok_or_else(|| anyhow!("unknown case_id: {id}"))?;
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
        // 聞き返し可否（決定論）: 第3層グレーのみ。第1・2層は問答無用でルーティング。
        let clarification_allowed = matches!(
            &decision_result,
            decision::AnswerDecision::Escalate {
                layer: 3,
                reason: decision::EscalateReason::InsufficientDirectness
                    | decision::EscalateReason::UnknownAddedSignal,
                ..
            }
        );
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
        let customer_reply_draft = self
            .draft_customer_reply(question, &decision_result, &section_hits, &resolutions)
            .await;

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
        })
    }

    /// 顧客向け返信文の下書きを 1 案作る（デモ用）。無効化時・LLM 未設定時・生成失敗時は
    /// `None` を返し、**評価そのものは成功させる**。
    ///
    /// 材料の選別（Escalate ではマニュアル本文を一切渡さない）は `reply::build_reply_brief`
    /// が担う。ここはその結果を送るだけで、安全判断をこの関数に持ち込まない。
    async fn draft_customer_reply(
        &self,
        question: &str,
        decision: &decision::AnswerDecision,
        hits: &[SectionHit],
        resolutions: &[rules::KnownResolution],
    ) -> Option<String> {
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
        let system = reply::build_reply_system_prompt(&brief);
        let user = reply::build_reply_user_message(question, &brief);
        let text = match drafter
            .draft_reply(&system, &user, self.reply_draft_max_tokens)
            .await
        {
            Ok(text) => text,
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
        let verdict = egress::egress_gate(&text, &ctx, &self.ng);
        match verdict {
            egress::EgressVerdict::Pass => Some(text),
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
                    draft_chars = text.chars().count(),
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
}
