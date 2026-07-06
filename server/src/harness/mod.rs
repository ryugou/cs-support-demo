pub mod audit;
pub mod authn;
pub mod correction;
pub mod decision;
pub mod egress;
pub mod grading;
pub mod knowledge;
pub mod rules;
pub mod scope;
pub mod signal;

use crate::config::AppConfig;
use crate::mcp::ToolService;
use crate::model::SectionHit;
use crate::vegapunk::VegapunkClient;
use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// AuthN → (A) scope → 取得 → 正規化 → 会話層 → (B) 3 層判定 → 記録 を束ねる本体。
/// tool handler はここを経由し、判定ロジックを直書きしない（S1-0 三原則 1）。
pub struct Harness {
    pub authenticator: authn::Authenticator,
    pub normalizer: Arc<dyn signal::SignalNormalizer>,
    pub lexicon: Arc<signal::LexiconNormalizer>,
    pub ng: egress::NgDictionary,
    pub worm: audit::WormAuditLog,
    pub knowledge: Option<knowledge::KnowledgeStore>,
    pub thresholds: decision::Thresholds,
    pub grading: grading::GradingThresholds,
    pub queue_path: PathBuf,
    /// grade 更新（read-modify-write）のプロセス内直列化。単一インスタンス運用が前提。
    // TODO: bind to vegapunk atomic increment/CAS — backend 側の原子更新が使えるようになったら置き換える。
    pub grade_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub actor: authn::Actor,
    pub scope: scope::AccessScope,
    pub schema: String,
    pub request_id: String,
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
        let secret = match &config.auth.jwt_secret_file {
            Some(path) => {
                let raw = std::fs::read_to_string(resolve_path(path))
                    .with_context(|| format!("read jwt secret file {path}"))?;
                Some(raw.trim().as_bytes().to_vec())
            }
            None => None,
        };
        let lexicon = Arc::new(signal::LexiconNormalizer::from_path(&resolve_path(
            &config.harness.signal_lexicon_path,
        ))?);
        Ok(Self {
            authenticator: authn::Authenticator::new(
                secret,
                &config.actors,
                config.auth.default_actor.clone(),
            )
            .with_issuer(config.auth.jwt_issuer.clone()),
            normalizer: lexicon.clone(),
            lexicon,
            ng: egress::NgDictionary::from_path(&resolve_path(&config.harness.ng_dictionary_path))?,
            worm: audit::WormAuditLog::open(&resolve_path(&config.harness.audit_log_path))?,
            knowledge: Some(knowledge::KnowledgeStore::new(client)),
            thresholds: (&config.harness.thresholds).into(),
            grading: (&config.harness.grading).into(),
            queue_path: resolve_path(&config.harness.search_improvement_queue_path),
            grade_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn knowledge(&self) -> Result<&knowledge::KnowledgeStore> {
        self.knowledge
            .as_ref()
            .ok_or_else(|| anyhow!("knowledge store is not configured"))
    }

    /// tool handler から材料ストアへアクセスするための入口（判定は持たない）。
    pub fn store(&self) -> Result<&knowledge::KnowledgeStore> {
        self.knowledge()
    }

    /// 監査イベントの共通入口。ctx 由来の provenance フィールドをここで一元的に埋める。
    pub fn audit(
        &self,
        ctx: &RequestContext,
        decision: impl Into<String>,
        route: Option<String>,
        governing_norm_ids: Vec<String>,
    ) -> Result<String> {
        self.audit_with_nodes(ctx, decision, route, governing_norm_ids, Vec::new())
    }

    pub fn audit_with_nodes(
        &self,
        ctx: &RequestContext,
        decision: impl Into<String>,
        route: Option<String>,
        governing_norm_ids: Vec<String>,
        retrieved_node_ids: Vec<String>,
    ) -> Result<String> {
        self.worm.append(audit::AuditDraft {
            request_id: ctx.request_id.clone(),
            schema: ctx.schema.clone(),
            actor: ctx.actor.sub.clone(),
            used_scope: ctx.scope.clone(),
            retrieved_node_ids,
            decision: decision.into(),
            route,
            governing_norm_ids,
        })
    }

    /// grade 運用（遵守事項 3）: outcome を承認/却下に写像し、regrade 純関数で
    /// 昇格・降格を判定して永続化する。格付けが変わった場合のみ Some を返す。
    pub async fn apply_answer_outcome(
        &self,
        ctx: &RequestContext,
        kr_id: &str,
        outcome: grading::AnswerOutcome,
    ) -> Result<Option<rules::Grade>> {
        // read-modify-write の lost update を防ぐ（プロセス内直列化。backend atomic は TODO）
        let _guard = self.grade_lock.lock().await;
        let store = self.knowledge()?;
        let resolutions = store.load_known_resolutions(&ctx.schema).await?;
        let kr = resolutions
            .iter()
            .find(|kr| kr.id == kr_id)
            .ok_or_else(|| anyhow!("known_resolution not found: {kr_id}"))?;
        let mut approval_count = kr.approval_count;
        let mut rejection_count = kr.rejection_count;
        let mut approver_set = kr.approver_set.clone();
        match outcome {
            grading::AnswerOutcome::Resolved => {
                approval_count += 1;
                if !approver_set.contains(&ctx.actor.sub) {
                    approver_set.push(ctx.actor.sub.clone());
                }
            }
            grading::AnswerOutcome::WrongAnswer => rejection_count += 1,
            grading::AnswerOutcome::Unresolved | grading::AnswerOutcome::ReInquiry => {}
        }
        let regraded = grading::regrade(
            kr.grade,
            approval_count,
            rejection_count,
            approver_set.len(),
            &self.grading,
        );
        store
            .update_known_resolution_grade(
                &ctx.schema,
                kr_id,
                approval_count,
                rejection_count,
                &approver_set,
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
    ) -> Result<signal::SignalSet> {
        // authoritative の担い手のみ（supervisor / admin）
        if !matches!(ctx.actor.role, authn::Role::Supervisor | authn::Role::Admin) {
            anyhow::bail!(
                "permission_denied: add_known_resolution requires supervisor or admin role"
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
        // NG 語を含む知識は登録させない（egress と同じ辞書）。
        // binding は build_known_resolution_graph が advisory 固定で書く（mandatory は自動で書けない）。
        if let egress::EgressVerdict::Block { term } = egress::egress_gate(
            answer,
            &egress::EmitContext {
                channel: egress::EmitChannel::Operator,
            },
            &self.ng,
        ) {
            anyhow::bail!("answer contains blocked term: {term}");
        }
        Ok(set)
    }

    /// S1-1 パイプライン前半: [認証] → [(A) 権限]。全 tool がここを通る。
    pub fn begin(
        &self,
        authorization: Option<&str>,
        project_schema: &str,
    ) -> Result<RequestContext> {
        let actor = self.authenticator.authenticate(authorization)?;
        let access = scope::resolve_scope(&actor, project_schema)?;
        Ok(RequestContext {
            schema: access.enforced_schema().to_string(),
            actor,
            scope: access,
            request_id: uuid::Uuid::new_v4().to_string(),
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
        // 4 つの読み取りは互いに独立なので並列に発行する（レイテンシ = max(RTT)）。
        let (rules, domains, resolutions, hits) = tokio::try_join!(
            knowledge.load_escalation_rules(&ctx.schema),
            knowledge.load_prohibited_domains(&ctx.schema),
            knowledge.load_known_resolutions(&ctx.schema),
            tools.search_manual(&ctx.schema, question, product_key, 5),
        )?;
        // [正規化] 決定論 lexicon（S1-11）。今ターン分。
        let signals = self.normalizer.normalize(question);
        // [会話層] 累積 signal 集合の維持。client 供給の prior signals は受けない（入力不信）。
        // 既存 case_id は存在を検証する（未知の id への orphan edge 追加を防ぐ）。
        let (case_id, prior_signals) = match case_id {
            Some(id) => {
                if knowledge.load_case(&ctx.schema, id).await?.is_none() {
                    return Err(anyhow!("unknown case_id: {id}"));
                }
                (
                    id.to_string(),
                    knowledge.load_case_signals(&ctx.schema, id).await?,
                )
            }
            None => {
                let new_id = format!("case-{}", uuid::Uuid::new_v4());
                knowledge
                    .record(
                        &ctx.schema,
                        "support_case",
                        &new_id,
                        vec![
                            ("case_id".to_string(), new_id.clone()),
                            ("request_id".to_string(), ctx.request_id.clone()),
                            ("actor".to_string(), ctx.actor.sub.clone()),
                            ("question".to_string(), question.to_string()),
                            (
                                "product_key".to_string(),
                                product_key.unwrap_or_default().to_string(),
                            ),
                            ("created_at".to_string(), chrono::Utc::now().to_rfc3339()),
                        ],
                    )
                    .await?;
                (new_id, signal::SignalSet::new())
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
        // [(B) 3 層判定] 純関数。判定根拠は常に「累積 signal 集合 + known_resolution」。
        let best = hits.first();
        let section_keys: Vec<String> = hits.iter().map(|h| h.section_key.clone()).collect();
        let decision_result = decision::decide(&decision::DecisionInput {
            question_signals: &accumulated,
            question_raw: question,
            rules: &rules,
            domains: &domains,
            resolutions: &resolutions,
            best_manual_score: best.map(|h| h.score),
            best_manual_sections: &section_keys,
            stakes_input,
            thresholds: &self.thresholds,
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
        let (case_decision, case_kr_id) = match &decision_result {
            decision::AnswerDecision::Allowed {
                known_resolution_id,
                ..
            } => ("allowed", known_resolution_id.clone().unwrap_or_default()),
            decision::AnswerDecision::Escalate { .. } => ("escalate", String::new()),
        };
        knowledge
            .record(
                &ctx.schema,
                "support_case",
                &case_id,
                vec![
                    ("case_id".to_string(), case_id.clone()),
                    ("last_request_id".to_string(), ctx.request_id.clone()),
                    ("last_decision".to_string(), case_decision.to_string()),
                    ("last_kr_id".to_string(), case_kr_id),
                ],
            )
            .await?;
        // [記録] WORM（S1-8 条件 8）
        let (decision_label, route) = match &decision_result {
            decision::AnswerDecision::Allowed { source, .. } => {
                (format!("allowed:{source:?}"), None)
            }
            decision::AnswerDecision::Escalate {
                layer, route_to, ..
            } => (format!("escalate:layer{layer}"), Some(route_to.clone())),
        };
        let mut retrieved_node_ids: Vec<String> = hits
            .iter()
            .map(|h| crate::ingest::section_node_id(&ctx.schema, &h.section_key))
            .collect();
        retrieved_node_ids.push(knowledge::harness_node_id(
            &ctx.schema,
            "support_case",
            &case_id,
        ));
        let audit_event_id =
            self.audit_with_nodes(ctx, decision_label, route, Vec::new(), retrieved_node_ids)?;
        Ok(EvaluationOutcome {
            decision: decision_result,
            signals,
            accumulated_signals: accumulated,
            case_id,
            clarification_allowed,
            hits,
            audit_event_id,
        })
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
    pub async fn root_cause_probe(
        &self,
        ctx: &RequestContext,
        corrected_answer: &str,
        tools: &ToolService,
    ) -> Result<rules::RootCause> {
        let hits = tools
            .search_manual(&ctx.schema, corrected_answer, None, 3)
            .await?;
        let found = hits
            .first()
            .map(|h| h.score >= self.thresholds.mid)
            .unwrap_or(false);
        Ok(if found {
            rules::RootCause::RetrievalMiss
        } else {
            rules::RootCause::KnowledgeError
        })
    }

    /// 検索改善キューへの追記（retrieval_miss の受け皿。known_resolution を増やさない）。
    pub fn enqueue_search_improvement(
        &self,
        ctx: &RequestContext,
        corrected_answer: &str,
    ) -> Result<()> {
        if let Some(parent) = self.queue_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.queue_path)?;
        let entry = serde_json::json!({
            "request_id": ctx.request_id,
            "schema": ctx.schema,
            "actor": ctx.actor.sub,
            "corrected_answer": corrected_answer,
            "queued_at": chrono::Utc::now().to_rfc3339(),
        });
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ActorConfig;

    fn harness_for_test() -> Harness {
        let dir = std::env::temp_dir().join(format!("harness-test-{}", uuid::Uuid::new_v4()));
        Harness {
            authenticator: authn::Authenticator::new(
                None,
                &[ActorConfig {
                    sub: "op-001".to_string(),
                    role: "operator".to_string(),
                    allowed_schemas: vec!["sivira-cs-demo".to_string()],
                }],
                Some("op-001".to_string()),
            ),
            normalizer: Arc::new(
                signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap(),
            ),
            lexicon: Arc::new(signal::LexiconNormalizer::from_json(r#"{"signals":[]}"#).unwrap()),
            ng: egress::NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":[]}"#)
                .unwrap(),
            worm: audit::WormAuditLog::open(&dir.join("audit.jsonl")).unwrap(),
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
        }
    }

    #[test]
    fn begin_produces_request_context_with_enforced_schema() {
        let harness = harness_for_test();
        let ctx = harness.begin(None, "sivira-cs-demo").expect("begin");
        assert_eq!(ctx.schema, "sivira-cs-demo");
        assert_eq!(ctx.actor.sub, "op-001");
        assert!(!ctx.request_id.is_empty());
    }

    #[test]
    fn begin_rejects_out_of_scope_project() {
        let harness = harness_for_test();
        assert!(harness.begin(None, "other-tenant").is_err());
    }
}
