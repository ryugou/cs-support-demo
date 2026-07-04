use crate::{
    harness::{
        correction::{correction_intake, CorrectionRouting},
        egress::{egress_gate, EgressVerdict, EmitChannel, EmitContext},
        grading::regrade,
        knowledge::{NewKnownResolution, PastCase},
        rules::{Grade, KrMatch, RootCause, SourceAuthority},
        signal::Signal,
        Harness, RequestContext,
    },
    mcp::ToolService,
    model::{ProductCandidate, ProductView, SectionHit, SectionView},
    resolve::normalize_key,
};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct CsSupportRmcpServer {
    schema: String,
    tools: ToolService,
    harness: Arc<Harness>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResolveProductRequest {
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchManualRequest {
    pub query_ja: String,
    pub product_key: Option<String>,
    pub top_k: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetSectionRequest {
    pub section_key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetProductRequest {
    pub product_key: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ResolveProductResponse {
    pub candidates: Vec<ProductCandidate>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchManualResponse {
    pub hits: Vec<SectionHit>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvaluateAnswerabilityRequest {
    /// 顧客質問（日本語）。今ターンの発話。signal 正規化と 3 層判定の対象。
    pub question: String,
    pub product_key: Option<String>,
    /// 会話の継続キー。同一問い合わせの 2 ターン目以降は必ず前回返された case_id を渡す。
    /// サーバは case の累積 signal 集合に今ターン分を加算し、累積集合で再判定する。
    pub case_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EvaluateAnswerabilityResponse {
    pub decision: crate::harness::decision::AnswerDecision,
    /// 今ターンで抽出した signal
    pub signals: Vec<String>,
    /// 判定に使った累積 signal 集合（判定根拠）
    pub accumulated_signals: Vec<String>,
    /// 次ターンで渡す会話キー
    pub case_id: String,
    /// true のとき、不足条件（decision.missing）について利用者へ聞き返してよい。
    /// 文面は client（LLM）が生成する。第1・2層エスカレーションでは常に false。
    pub clarification_allowed: bool,
    pub hits: Vec<SectionHit>,
    pub audit_event_id: String,
    pub request_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchKnownResolutionsRequest {
    /// 顧客質問（日本語）。signal 正規化して照合する。
    pub question: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct KnownResolutionView {
    pub kr_id: String,
    pub signals: Vec<String>,
    pub answer: String,
    pub applicability: String,
    pub grade: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchKnownResolutionsResponse {
    /// applicable / blocked_by_added_signal / none
    pub match_kind: String,
    pub resolution: Option<KnownResolutionView>,
    /// blocked_by_added_signal のとき、既存ルールが想定していない残余 signal（再利用不可の理由）
    pub leftover_signals: Vec<String>,
    pub question_signals: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchPastCasesRequest {
    pub query_ja: String,
    pub top_k: Option<i32>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PastCaseHit {
    #[serde(flatten)]
    pub case: PastCase,
    pub score: f32,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchPastCasesResponse {
    pub cases: Vec<PastCaseHit>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordAnswerAttemptRequest {
    pub case_id: Option<String>,
    /// 顧客に出す予定の draft 全文。AI 草案・担当者修正文の区別なく必ずここを通す。
    pub draft: String,
    pub question: String,
    pub product_key: Option<String>,
    /// 判定済み evaluate_answerability の request_id（lineage 接続用、任意）
    pub evaluation_request_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordAnswerAttemptResponse {
    pub attempt_id: String,
    pub egress: EgressVerdict,
    /// pass のときのみ true（Step 1 の利用者は担当者なので pass は直接応答可）。
    /// block / abstain はエスカレーション応答へ（未検証草案の参考添付は可）。
    pub emit_allowed: bool,
    pub audit_event_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordAnswerOutcomeRequest {
    pub attempt_id: String,
    /// resolved / unresolved / re_inquiry / wrong_answer
    pub outcome: String,
    /// この応答が known_resolution 由来だった場合に渡す（承認/却下カウントと格付けを更新）
    pub known_resolution_id: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordAnswerOutcomeResponse {
    /// grade 更新が行われた場合の新しい格付け
    pub grade: Option<String>,
    pub audit_event_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordOperatorFeedbackRequest {
    pub attempt_id: Option<String>,
    /// "operator"（担当者自身の訂正）か "customer"（顧客からの「違う」の中継）
    pub feedback_source: String,
    pub corrected_answer: String,
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordOperatorFeedbackResponse {
    pub routing: CorrectionRouting,
    pub root_cause: Option<String>,
    pub audit_event_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateEscalationEventRequest {
    pub question: String,
    pub layer: i32,
    pub reason: String,
    pub route_to: String,
    pub case_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CreateEscalationEventResponse {
    pub escalation_id: String,
    pub audit_event_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddKnownResolutionRequest {
    /// 適用条件となる signal 値の集合（specs/signal-vocabulary.md の語彙）
    pub signals: Vec<String>,
    /// 商品/ロット/契約/時期などの適用条件。人間が明示的に書く（LLM に推測させない）
    pub applicability: String,
    /// その条件下での正しい答え（OK 回答も NG 回答も同枠）
    pub answer: String,
    /// どの escalation 起点か
    pub origin_escalation_id: Option<String>,
    /// 根拠となる manual section（BECAUSE 辺で結線）
    pub rationale_section_keys: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AddKnownResolutionResponse {
    pub kr_id: String,
    pub audit_event_id: String,
}

fn grade_label(grade: Grade) -> &'static str {
    match grade {
        Grade::ApprovalRequired => "approval_required",
        Grade::AutoAnswerAudited => "auto_answer_audited",
        Grade::Demoted => "demoted",
    }
}

fn operator_emit_context() -> EmitContext {
    // Step 1 のチャネルは operator 固定（S1-4 / 遵守事項 4）
    EmitContext {
        channel: EmitChannel::Operator,
    }
}

#[tool_router]
impl CsSupportRmcpServer {
    pub fn new(schema: String, tools: ToolService, harness: Arc<Harness>) -> Self {
        Self {
            schema,
            tools,
            harness,
        }
    }

    /// 全 tool の共通入口。認証 → scope 強制 → RequestContext（S1-1 前半）。
    fn begin(&self, extensions: &rmcp::model::Extensions) -> Result<RequestContext, ErrorData> {
        let authorization = extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.headers.get(http::header::AUTHORIZATION))
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        self.harness
            .begin(authorization.as_deref(), &self.schema)
            .map_err(|err| ErrorData::invalid_request(err.to_string(), None))
    }

    #[tool(
        name = "resolve_product",
        description = "商品名・型番・顧客表現から候補 product を返す。aliases テーブルは使わない。"
    )]
    async fn resolve_product(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<ResolveProductRequest>,
    ) -> Result<Json<ResolveProductResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        self.tools
            .resolve_product(&ctx.schema, &req.text)
            .await
            .map(|candidates| Json(ResolveProductResponse { candidates }))
            .map_err(to_error)
    }

    #[tool(
        name = "search_manual",
        description = "日本語 query_ja で日本語マニュアル本文 body_ja を検索し、breadcrumb と英語原文 fallback を返す。認証 actor の scope 内のみ検索される。"
    )]
    async fn search_manual(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<SearchManualRequest>,
    ) -> Result<Json<SearchManualResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        self.tools
            .search_manual(
                &ctx.schema,
                &req.query_ja,
                req.product_key.as_deref(),
                req.top_k.unwrap_or(5),
            )
            .await
            .map(|hits| Json(SearchManualResponse { hits }))
            .map_err(to_error)
    }

    #[tool(
        name = "get_section",
        description = "section と祖先、最大2hopの子、REFERENCES 参照先を返す。"
    )]
    async fn get_section(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<GetSectionRequest>,
    ) -> Result<Json<SectionView>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        self.tools
            .get_section(&ctx.schema, &req.section_key)
            .await
            .map(Json)
            .map_err(to_error)
    }

    #[tool(
        name = "get_product",
        description = "product 概要、HAS_SPEC 仕様、document TOC を返す。"
    )]
    async fn get_product(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<GetProductRequest>,
    ) -> Result<Json<ProductView>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        self.tools
            .get_product(&ctx.schema, &req.product_key)
            .await
            .map(Json)
            .map_err(to_error)
    }

    #[tool(
        name = "evaluate_answerability",
        description = "顧客質問を 3 層判定（明示ルール → 禁止領域 → 回答可能性）にかけ、回答可否・エスカレーション判定・根拠を返す。回答系フローの必須入口。マルチターンの問い合わせでは前回の case_id を渡すこと（累積条件で毎回再判定される）。"
    )]
    async fn evaluate_answerability(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<EvaluateAnswerabilityRequest>,
    ) -> Result<Json<EvaluateAnswerabilityResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let outcome = self
            .harness
            .evaluate(
                &ctx,
                &req.question,
                req.product_key.as_deref(),
                req.case_id.as_deref(),
                &self.tools,
            )
            .await
            .map_err(to_error)?;
        Ok(Json(EvaluateAnswerabilityResponse {
            decision: outcome.decision,
            signals: outcome
                .signals
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
            accumulated_signals: outcome
                .accumulated_signals
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
            case_id: outcome.case_id,
            clarification_allowed: outcome.clarification_allowed,
            hits: outcome.hits,
            audit_event_id: outcome.audit_event_id,
            request_id: ctx.request_id,
        }))
    }

    #[tool(
        name = "search_known_resolutions",
        description = "質問を signal 集合に正規化し、検証済みノウハウ（known_resolution）と照合する。未知の追加条件が残る場合は再利用不可の理由（leftover_signals）を返す。"
    )]
    async fn search_known_resolutions(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<SearchKnownResolutionsRequest>,
    ) -> Result<Json<SearchKnownResolutionsResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let store = self.harness.store().map_err(to_error)?;
        let question_signals = self.harness.normalizer.normalize(&req.question);
        let resolutions = store
            .load_known_resolutions(&ctx.schema)
            .await
            .map_err(to_error)?;
        // 照合は Rust 集合演算（rules::match_known_resolution）。tool は結果を返すだけ。
        let (match_kind, resolution, leftover) =
            match crate::harness::rules::match_known_resolution(&resolutions, &question_signals) {
                KrMatch::Applicable(kr) => (
                    "applicable".to_string(),
                    Some(KnownResolutionView {
                        kr_id: kr.id.clone(),
                        signals: kr
                            .signal_set
                            .iter()
                            .map(|s| s.as_str().to_string())
                            .collect(),
                        answer: kr.answer.clone(),
                        applicability: kr.applicability.clone(),
                        grade: grade_label(kr.grade).to_string(),
                    }),
                    Vec::new(),
                ),
                KrMatch::BlockedByAddedSignal { leftover } => (
                    "blocked_by_added_signal".to_string(),
                    None,
                    leftover.iter().map(|s| s.as_str().to_string()).collect(),
                ),
                KrMatch::None => ("none".to_string(), None, Vec::new()),
            };
        Ok(Json(SearchKnownResolutionsResponse {
            match_kind,
            resolution,
            leftover_signals: leftover,
            question_signals: question_signals
                .iter()
                .map(|s| s.as_str().to_string())
                .collect(),
        }))
    }

    #[tool(
        name = "search_past_cases",
        description = "過去の問い合わせ事例（support_case）を日本語クエリで検索する。actor の scope 内のみ。"
    )]
    async fn search_past_cases(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<SearchPastCasesRequest>,
    ) -> Result<Json<SearchPastCasesResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let store = self.harness.store().map_err(to_error)?;
        let query_norm = normalize_key(&req.query_ja);
        let mut cases: Vec<PastCaseHit> = store
            .load_cases(&ctx.schema, 500)
            .await
            .map_err(to_error)?
            .into_iter()
            .filter_map(|case| {
                let text_norm = normalize_key(&case.question);
                let score = if !query_norm.is_empty() && text_norm.contains(&query_norm) {
                    1.0
                } else {
                    let q_chars: Vec<char> = query_norm.chars().collect();
                    if q_chars.is_empty() {
                        0.0
                    } else {
                        let matched = q_chars.iter().filter(|ch| text_norm.contains(**ch)).count();
                        matched as f32 / q_chars.len() as f32 * 0.6
                    }
                };
                (score > 0.3).then_some(PastCaseHit { case, score })
            })
            .collect();
        cases.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        cases.truncate(req.top_k.unwrap_or(5).max(1) as usize);
        Ok(Json(SearchPastCasesResponse { cases }))
    }

    #[tool(
        name = "record_answer_attempt",
        description = "顧客に出す予定の draft を出口ゲート（NG 辞書 + 暗示効能 abstain）に通し、answer_attempt として記録する。pass = 担当者へ応答可 / block・abstain = エスカレーション応答（草案は参考添付のみ）。誤答になるかもしれないものはエスカレーションに倒す。"
    )]
    async fn record_answer_attempt(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<RecordAnswerAttemptRequest>,
    ) -> Result<Json<RecordAnswerAttemptResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let store = self.harness.store().map_err(to_error)?;
        // 出口ゲート（S1-4）。AI 製・人間製を問わず全 outbound がここを通る。
        let verdict = egress_gate(&req.draft, &operator_emit_context(), &self.harness.ng);
        let emit_allowed = matches!(verdict, EgressVerdict::Pass);
        let verdict_label = match &verdict {
            EgressVerdict::Pass => "pass".to_string(),
            EgressVerdict::Block { .. } => "block".to_string(),
            EgressVerdict::Abstain { .. } => "abstain".to_string(),
        };
        let audit_event_id = self
            .harness
            .worm
            .append(crate::harness::audit::AuditDraft {
                request_id: ctx.request_id.clone(),
                schema: ctx.schema.clone(),
                actor: ctx.actor.sub.clone(),
                used_scope: ctx.scope.clone(),
                retrieved_node_ids: Vec::new(),
                decision: format!("egress:{verdict_label}"),
                route: None,
                governing_norm_ids: Vec::new(),
            })
            .map_err(to_error)?;
        let attempt_id = format!("attempt-{}", uuid::Uuid::new_v4());
        store
            .record(
                &ctx.schema,
                "answer_attempt",
                &attempt_id,
                vec![
                    ("attempt_id".to_string(), attempt_id.clone()),
                    (
                        "case_id".to_string(),
                        req.case_id.clone().unwrap_or_default(),
                    ),
                    ("request_id".to_string(), ctx.request_id.clone()),
                    ("actor".to_string(), ctx.actor.sub.clone()),
                    ("draft".to_string(), req.draft.clone()),
                    (
                        "decision".to_string(),
                        req.evaluation_request_id.clone().unwrap_or_default(),
                    ),
                    ("egress_verdict".to_string(), verdict_label),
                    ("audit_event_id".to_string(), audit_event_id.clone()),
                    ("created_at".to_string(), chrono::Utc::now().to_rfc3339()),
                ],
            )
            .await
            .map_err(to_error)?;
        Ok(Json(RecordAnswerAttemptResponse {
            attempt_id,
            egress: verdict,
            emit_allowed,
            audit_event_id,
        }))
    }

    #[tool(
        name = "record_answer_outcome",
        description = "応答の結果（resolved / unresolved / re_inquiry / wrong_answer）を記録する。known_resolution 由来の応答なら known_resolution_id を渡すこと（承認/却下カウントと格付けが更新される）。"
    )]
    async fn record_answer_outcome(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<RecordAnswerOutcomeRequest>,
    ) -> Result<Json<RecordAnswerOutcomeResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let store = self.harness.store().map_err(to_error)?;
        if !["resolved", "unresolved", "re_inquiry", "wrong_answer"].contains(&req.outcome.as_str())
        {
            return Err(ErrorData::invalid_params(
                format!("unknown outcome: {}", req.outcome),
                None,
            ));
        }
        // attempt へ outcome を追記（upsert merge）
        store
            .record(
                &ctx.schema,
                "answer_attempt",
                &req.attempt_id,
                vec![
                    ("attempt_id".to_string(), req.attempt_id.clone()),
                    ("outcome".to_string(), req.outcome.clone()),
                    (
                        "outcome_note".to_string(),
                        req.note.clone().unwrap_or_default(),
                    ),
                ],
            )
            .await
            .map_err(to_error)?;
        // grade 運用（遵守事項 3）: 承認/却下の写像 → regrade 純関数 → 永続化
        let mut new_grade: Option<String> = None;
        let mut governing_norm_ids = Vec::new();
        if let Some(kr_id) = &req.known_resolution_id {
            let resolutions = store
                .load_known_resolutions(&ctx.schema)
                .await
                .map_err(to_error)?;
            let kr = resolutions
                .iter()
                .find(|kr| &kr.id == kr_id)
                .ok_or_else(|| {
                    ErrorData::invalid_params(format!("known_resolution not found: {kr_id}"), None)
                })?;
            let mut approval_count = kr.approval_count;
            let mut rejection_count = kr.rejection_count;
            let mut approver_set = kr.approver_set.clone();
            match req.outcome.as_str() {
                "resolved" => {
                    approval_count += 1;
                    if !approver_set.contains(&ctx.actor.sub) {
                        approver_set.push(ctx.actor.sub.clone());
                    }
                }
                "wrong_answer" => rejection_count += 1,
                _ => {}
            }
            let regraded = regrade(
                kr.grade,
                approval_count,
                rejection_count,
                approver_set.len(),
                &self.harness.grading,
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
                .await
                .map_err(to_error)?;
            if regraded != kr.grade {
                new_grade = Some(grade_label(regraded).to_string());
            }
            governing_norm_ids.push(kr_id.clone());
        }
        let decision = match &new_grade {
            Some(grade) => format!("outcome:{} regrade:{grade}", req.outcome),
            None => format!("outcome:{}", req.outcome),
        };
        let audit_event_id = self
            .harness
            .worm
            .append(crate::harness::audit::AuditDraft {
                request_id: ctx.request_id.clone(),
                schema: ctx.schema.clone(),
                actor: ctx.actor.sub.clone(),
                used_scope: ctx.scope.clone(),
                retrieved_node_ids: Vec::new(),
                decision,
                route: None,
                governing_norm_ids,
            })
            .map_err(to_error)?;
        Ok(Json(RecordAnswerOutcomeResponse {
            grade: new_grade,
            audit_event_id,
        }))
    }

    #[tool(
        name = "record_operator_feedback",
        description = "訂正・フィードバックを訂正インテークに通す。feedback_source=customer（顧客の「違う」の中継）は永続層に書かれない。担当者訂正は root_cause を再検索で切り分け、retrieval_miss は検索改善キュー、knowledge_error は known_resolution 候補（登録は add_known_resolution で明示的に行う）。"
    )]
    async fn record_operator_feedback(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<RecordOperatorFeedbackRequest>,
    ) -> Result<Json<RecordOperatorFeedbackResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let authority = match req.feedback_source.as_str() {
            "customer" => SourceAuthority::NonAuthoritative,
            "operator" => SourceAuthority::Authoritative,
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unknown feedback_source: {other}"),
                    None,
                ))
            }
        };
        // non_authoritative: 永続層に一切書かない（S1-5 不変条件・最優先）。WORM のみ。
        if authority == SourceAuthority::NonAuthoritative {
            let audit_event_id = self
                .harness
                .worm
                .append(crate::harness::audit::AuditDraft {
                    request_id: ctx.request_id.clone(),
                    schema: ctx.schema.clone(),
                    actor: ctx.actor.sub.clone(),
                    used_scope: ctx.scope.clone(),
                    retrieved_node_ids: Vec::new(),
                    decision: "correction:conversation_only".to_string(),
                    route: None,
                    governing_norm_ids: Vec::new(),
                })
                .map_err(to_error)?;
            return Ok(Json(RecordOperatorFeedbackResponse {
                routing: CorrectionRouting::ConversationOnly,
                root_cause: None,
                audit_event_id,
            }));
        }
        // authoritative: root_cause を再検索で切り分け（S1-5）
        let root_cause = self
            .harness
            .root_cause_probe(&ctx, &req.corrected_answer, &self.tools)
            .await
            .map_err(to_error)?;
        let routing = correction_intake(authority, root_cause);
        if routing == CorrectionRouting::SearchImprovementQueue {
            // known_resolution を増やさず検索改善キューへ（S1-8 条件 4）
            self.harness
                .enqueue_search_improvement(&ctx, &req.corrected_answer)
                .map_err(to_error)?;
        }
        let routing_label = match routing {
            CorrectionRouting::ConversationOnly => "conversation_only",
            CorrectionRouting::SearchImprovementQueue => "search_improvement_queue",
            CorrectionRouting::KnownResolutionCandidate => "known_resolution_candidate",
        };
        let root_cause_label = match root_cause {
            RootCause::RetrievalMiss => "retrieval_miss",
            RootCause::KnowledgeError => "knowledge_error",
        };
        let store = self.harness.store().map_err(to_error)?;
        let feedback_id = format!("fb-{}", uuid::Uuid::new_v4());
        store
            .record(
                &ctx.schema,
                "operator_feedback",
                &feedback_id,
                vec![
                    ("feedback_id".to_string(), feedback_id.clone()),
                    (
                        "attempt_id".to_string(),
                        req.attempt_id.clone().unwrap_or_default(),
                    ),
                    ("request_id".to_string(), ctx.request_id.clone()),
                    ("actor".to_string(), ctx.actor.sub.clone()),
                    ("feedback_source".to_string(), req.feedback_source.clone()),
                    ("corrected_answer".to_string(), req.corrected_answer.clone()),
                    ("routing".to_string(), routing_label.to_string()),
                    ("created_at".to_string(), chrono::Utc::now().to_rfc3339()),
                ],
            )
            .await
            .map_err(to_error)?;
        let audit_event_id = self
            .harness
            .worm
            .append(crate::harness::audit::AuditDraft {
                request_id: ctx.request_id.clone(),
                schema: ctx.schema.clone(),
                actor: ctx.actor.sub.clone(),
                used_scope: ctx.scope.clone(),
                retrieved_node_ids: Vec::new(),
                decision: format!("correction:{routing_label} root_cause:{root_cause_label}"),
                route: None,
                governing_norm_ids: Vec::new(),
            })
            .map_err(to_error)?;
        Ok(Json(RecordOperatorFeedbackResponse {
            routing,
            root_cause: Some(root_cause_label.to_string()),
            audit_event_id,
        }))
    }

    #[tool(
        name = "create_escalation_event",
        description = "エスカレーションを正式に記録する（escalation_event）。evaluate_answerability の Escalate 判定を受けて呼ぶ。"
    )]
    async fn create_escalation_event(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<CreateEscalationEventRequest>,
    ) -> Result<Json<CreateEscalationEventResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let store = self.harness.store().map_err(to_error)?;
        let escalation_id = format!("esc-{}", uuid::Uuid::new_v4());
        store
            .record(
                &ctx.schema,
                "escalation_event",
                &escalation_id,
                vec![
                    ("escalation_id".to_string(), escalation_id.clone()),
                    ("request_id".to_string(), ctx.request_id.clone()),
                    ("actor".to_string(), ctx.actor.sub.clone()),
                    ("layer".to_string(), req.layer.to_string()),
                    ("reason".to_string(), req.reason.clone()),
                    ("route_to".to_string(), req.route_to.clone()),
                    ("question".to_string(), req.question.clone()),
                    ("created_at".to_string(), chrono::Utc::now().to_rfc3339()),
                ],
            )
            .await
            .map_err(to_error)?;
        let audit_event_id = self
            .harness
            .worm
            .append(crate::harness::audit::AuditDraft {
                request_id: ctx.request_id.clone(),
                schema: ctx.schema.clone(),
                actor: ctx.actor.sub.clone(),
                used_scope: ctx.scope.clone(),
                retrieved_node_ids: Vec::new(),
                decision: "escalation_event".to_string(),
                route: Some(req.route_to.clone()),
                governing_norm_ids: vec![escalation_id.clone()],
            })
            .map_err(to_error)?;
        Ok(Json(CreateEscalationEventResponse {
            escalation_id,
            audit_event_id,
        }))
    }

    #[tool(
        name = "add_known_resolution",
        description = "担当者による検証済みノウハウの追加（GMR の進化 = 例外ルールの離散 insert）。supervisor / admin のみ。signal は specs/signal-vocabulary.md の語彙に限る。適用条件（applicability）は人間が明示的に書くこと。"
    )]
    async fn add_known_resolution(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<AddKnownResolutionRequest>,
    ) -> Result<Json<AddKnownResolutionResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        // ガード 1: authoritative の担い手（supervisor / admin）のみ
        if !matches!(
            ctx.actor.role,
            crate::harness::authn::Role::Supervisor | crate::harness::authn::Role::Admin
        ) {
            return Err(ErrorData::invalid_request(
                "permission_denied: add_known_resolution requires supervisor or admin role"
                    .to_string(),
                None,
            ));
        }
        // ガード 2: 語彙外 signal は照合不能なので拒否
        if req.signals.is_empty() {
            return Err(ErrorData::invalid_params("signals must not be empty", None));
        }
        for value in &req.signals {
            if self.harness.lexicon.class_of(&Signal::new(value)).is_none() {
                return Err(ErrorData::invalid_params(
                    format!("unknown signal (not in vocabulary): {value}"),
                    None,
                ));
            }
        }
        // ガード 3: NG 語を含む知識は登録させない（egress と同じ辞書）
        if let EgressVerdict::Block { term } =
            egress_gate(&req.answer, &operator_emit_context(), &self.harness.ng)
        {
            return Err(ErrorData::invalid_params(
                format!("answer contains blocked term: {term}"),
                None,
            ));
        }
        // ガード 4: binding は常に advisory で書く（mandatory は自動で書けない。S1-5 不変条件）
        // build_known_resolution_graph が advisory 固定で組み立てる。
        let store = self.harness.store().map_err(to_error)?;
        let new_kr = NewKnownResolution {
            signal_set: req.signals.iter().map(Signal::new).collect(),
            applicability: req.applicability.clone(),
            answer: req.answer.clone(),
            origin: req
                .origin_escalation_id
                .clone()
                .map(|id| format!("escalation:{id}"))
                .unwrap_or_else(|| "manual".to_string()),
            created_by: ctx.actor.sub.clone(),
            rationale_section_keys: req.rationale_section_keys.clone(),
        };
        let kr_id = store
            .insert_known_resolution(&ctx.schema, &new_kr)
            .await
            .map_err(to_error)?;
        let audit_event_id = self
            .harness
            .worm
            .append(crate::harness::audit::AuditDraft {
                request_id: ctx.request_id.clone(),
                schema: ctx.schema.clone(),
                actor: ctx.actor.sub.clone(),
                used_scope: ctx.scope.clone(),
                retrieved_node_ids: Vec::new(),
                decision: "kr_insert".to_string(),
                route: None,
                governing_norm_ids: vec![kr_id.clone()],
            })
            .map_err(to_error)?;
        Ok(Json(AddKnownResolutionResponse {
            kr_id,
            audit_event_id,
        }))
    }
}

#[tool_handler]
impl ServerHandler for CsSupportRmcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "日本語カスタマーサポート向け CS domain MCP。回答系フローは evaluate_answerability を必ず入口にし、マルチターンでは case_id を引き回す。resolve_product / search_manual / get_section / get_product で根拠を取得する。"
                .to_string(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

fn to_error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}
