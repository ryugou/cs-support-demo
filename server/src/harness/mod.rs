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
            ),
            normalizer: lexicon.clone(),
            lexicon,
            ng: egress::NgDictionary::from_path(&resolve_path(&config.harness.ng_dictionary_path))?,
            worm: audit::WormAuditLog::open(&resolve_path(&config.harness.audit_log_path))?,
            knowledge: Some(knowledge::KnowledgeStore::new(client)),
            thresholds: decision::Thresholds {
                low: config.harness.thresholds.low,
                mid: config.harness.thresholds.mid,
                high: config.harness.thresholds.high,
            },
            grading: grading::GradingThresholds {
                promote_approvals: config.harness.grading.promote_approvals,
                promote_approvers: config.harness.grading.promote_approvers,
                promote_max_rejection_rate: config.harness.grading.promote_max_rejection_rate,
                demote_rejections: config.harness.grading.demote_rejections,
            },
            queue_path: resolve_path(&config.harness.search_improvement_queue_path),
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
        // [取得] scope は ctx.schema として全検索に注入済み（tenant=schema）
        let rules = knowledge.load_escalation_rules(&ctx.schema).await?;
        let domains = knowledge.load_prohibited_domains(&ctx.schema).await?;
        let resolutions = knowledge.load_known_resolutions(&ctx.schema).await?;
        let hits = tools
            .search_manual(&ctx.schema, question, product_key, 5)
            .await?;
        // [正規化] 決定論 lexicon（S1-11）。今ターン分。
        let signals = self.normalizer.normalize(question);
        // [会話層] 累積 signal 集合の維持。client 供給の prior signals は受けない（入力不信）。
        let (case_id, prior_signals) = match case_id {
            Some(id) => (
                id.to_string(),
                knowledge.load_case_signals(&ctx.schema, id).await?,
            ),
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
        // stakes 入力の決定論算出（累積集合に対して）
        let stakes_input = decision::StakesInput {
            mandatory_domain_near: domains.iter().any(|d| {
                d.binding == rules::Binding::Mandatory
                    && rules::match_layer2(std::slice::from_ref(d), &accumulated, question)
                        .is_some()
            }),
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
        let audit_event_id = self.worm.append(audit::AuditDraft {
            request_id: ctx.request_id.clone(),
            schema: ctx.schema.clone(),
            actor: ctx.actor.sub.clone(),
            used_scope: ctx.scope.clone(),
            retrieved_node_ids,
            decision: decision_label,
            route,
            governing_norm_ids: Vec::new(),
        })?;
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
