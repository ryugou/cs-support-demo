use crate::{
    harness::{
        correction::{correction_intake, CorrectionRouting, FeedbackSource},
        egress::{egress_gate, EgressVerdict, EmitChannel, EmitContext},
        grading::AnswerOutcome,
        knowledge::{NewKnownResolution, PastCase},
        rules::{KrMatch, SourceAuthority},
        Harness, RequestContext,
    },
    mcp::ToolService,
    model::{ProductCandidate, ProductView, SectionHit, SectionView},
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
    /// evaluate_answerability が返した case_id（必須）。質問文・product_key は
    /// case 側に記録済みのためここでは受け取らない（API 最小化）。
    pub case_id: String,
    /// 顧客に出す予定の draft 全文。AI 草案・担当者修正文の区別なく必ずここを通す。
    pub draft: String,
    /// 同一 case に対する evaluate_answerability の request_id（必須）。
    /// サーバ側の判定記録と突合され、最新判定が Allowed の場合のみ emit 候補になる。
    pub evaluation_request_id: String,
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
    /// record_answer_attempt が返した attempt_id（存在検証される）
    pub attempt_id: String,
    /// resolved / unresolved / re_inquiry / wrong_answer
    pub outcome: AnswerOutcome,
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
    /// operator（担当者自身の訂正）か customer（顧客からの「違う」の中継）
    pub feedback_source: FeedbackSource,
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
    /// 担当者の判断理由（任意）。BECAUSE → Rationale で残す
    pub rationale_text: Option<String>,
    /// マニュアル出典 section（BASED_ON → ManualSection で結線）
    pub manual_section_keys: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AddKnownResolutionResponse {
    pub kr_id: String,
    pub audit_event_id: String,
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
        let candidates = self
            .tools
            .resolve_product(&ctx.schema, &req.text)
            .await
            .map_err(to_error)?;
        // 誰が・いつ・どの scope で・何を検索したかを残す（spec 課題2 Harness の責務）
        let retrieved = candidates
            .iter()
            .map(|c| crate::ingest::product_node_id(&ctx.schema, &c.product_key))
            .collect();
        self.harness
            .audit_with_nodes(&ctx, "read:resolve_product", None, Vec::new(), retrieved)
            .await
            .map_err(to_error)?;
        Ok(Json(ResolveProductResponse { candidates }))
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
        let hits = self
            .tools
            .search_manual(
                &ctx.schema,
                &req.query_ja,
                req.product_key.as_deref(),
                req.top_k.unwrap_or(5),
            )
            .await
            .map_err(to_error)?;
        let retrieved = hits
            .iter()
            .map(|h| crate::ingest::section_node_id(&ctx.schema, &h.section_key))
            .collect();
        self.harness
            .audit_with_nodes(&ctx, "read:search_manual", None, Vec::new(), retrieved)
            .await
            .map_err(to_error)?;
        Ok(Json(SearchManualResponse { hits }))
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
        let view = self
            .tools
            .get_section(&ctx.schema, &req.section_key)
            .await
            .map_err(to_error)?;
        self.harness
            .audit_with_nodes(
                &ctx,
                "read:get_section",
                None,
                Vec::new(),
                vec![crate::ingest::section_node_id(
                    &ctx.schema,
                    &req.section_key,
                )],
            )
            .await
            .map_err(to_error)?;
        Ok(Json(view))
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
        let view = self
            .tools
            .get_product(&ctx.schema, &req.product_key)
            .await
            .map_err(to_error)?;
        self.harness
            .audit_with_nodes(
                &ctx,
                "read:get_product",
                None,
                Vec::new(),
                vec![crate::ingest::product_node_id(
                    &ctx.schema,
                    &req.product_key,
                )],
            )
            .await
            .map_err(to_error)?;
        Ok(Json(view))
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
                        grade: kr.grade.as_str().to_string(),
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
        let retrieved = resolution
            .iter()
            .map(|r| {
                crate::harness::knowledge::harness_node_id(&ctx.schema, "KnownResolution", &r.kr_id)
            })
            .collect();
        self.harness
            .audit_with_nodes(
                &ctx,
                format!("read:search_known_resolutions:{match_kind}"),
                None,
                Vec::new(),
                retrieved,
            )
            .await
            .map_err(to_error)?;
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
        let cases = store
            .search_cases(
                &ctx.schema,
                &req.query_ja,
                req.top_k.unwrap_or(5).max(1) as usize,
            )
            .await
            .map_err(to_error)?
            .into_iter()
            .map(|(case, score)| PastCaseHit { case, score })
            .collect::<Vec<_>>();
        let retrieved = cases
            .iter()
            .map(|hit| {
                crate::harness::knowledge::harness_node_id(
                    &ctx.schema,
                    "support_case",
                    &hit.case.case_id,
                )
            })
            .collect();
        self.harness
            .audit_with_nodes(&ctx, "read:search_past_cases", None, Vec::new(), retrieved)
            .await
            .map_err(to_error)?;
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
        // 入口強制: 3 層判定（evaluate_answerability）の Allowed 判定に紐づかない draft は
        // emit 候補にしない（判定バイパスの封鎖）。サーバ側の判定記録と突合する。
        // 判定が KR 由来なら kr_id をサーバ記録から引き継ぐ（client 申告を使わない）。
        let lineage_kr_id = self
            .harness
            .verify_answer_lineage(&ctx, &req.case_id, &req.evaluation_request_id)
            .await
            .map_err(|err| ErrorData::invalid_request(err.to_string(), None))?;
        // 出口ゲート（S1-4）。AI 製・人間製を問わず全 outbound がここを通る。
        let verdict = egress_gate(&req.draft, &operator_emit_context(), &self.harness.ng);
        let emit_allowed = matches!(verdict, EgressVerdict::Pass);
        let audit_event_id = self
            .harness
            .audit(
                &ctx,
                format!("egress:{}", verdict.label()),
                None,
                Vec::new(),
            )
            .await
            .map_err(to_error)?;
        let attempt_id = format!("attempt-{}", uuid::Uuid::new_v4());
        store
            .record(
                &ctx.schema,
                "answer_attempt",
                &attempt_id,
                vec![
                    ("attempt_id".to_string(), attempt_id.clone()),
                    ("case_id".to_string(), req.case_id.clone()),
                    ("request_id".to_string(), ctx.request_id.clone()),
                    ("actor".to_string(), ctx.actor.sub.clone()),
                    ("draft".to_string(), req.draft.clone()),
                    (
                        "evaluation_request_id".to_string(),
                        req.evaluation_request_id.clone(),
                    ),
                    (
                        "known_resolution_id".to_string(),
                        lineage_kr_id.unwrap_or_default(),
                    ),
                    ("egress_verdict".to_string(), verdict.label().to_string()),
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
        description = "応答の結果（resolved / unresolved / re_inquiry / wrong_answer）を attempt_id に対して記録する。応答が known_resolution 由来かはサーバ記録から自動で判定され、該当時は承認/却下カウントと格付けが更新される。outcome は write-once（同一 outcome の再送のみ冪等に受理）。"
    )]
    async fn record_answer_outcome(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<RecordAnswerOutcomeRequest>,
    ) -> Result<Json<RecordAnswerOutcomeResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        // 存在検証・write-once 強制・KR 紐づけ（サーバ記録）・grade 更新は Harness が
        // 単一クリティカルセクションで行う（判定を handler に直書きしない）。
        let (regraded, kr_id) = self
            .harness
            .record_answer_outcome(&ctx, &req.attempt_id, req.outcome, req.note.as_deref())
            .await
            .map_err(|err| ErrorData::invalid_params(err.to_string(), None))?;
        let new_grade: Option<String> = regraded.map(|grade| grade.as_str().to_string());
        let governing_norm_ids: Vec<String> = kr_id.into_iter().collect();
        let decision = match &new_grade {
            Some(grade) => format!("outcome:{} regrade:{grade}", req.outcome.as_str()),
            None => format!("outcome:{}", req.outcome.as_str()),
        };
        let audit_event_id = self
            .harness
            .audit(&ctx, decision, None, governing_norm_ids)
            .await
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
        let authority = req.feedback_source.authority();
        // non_authoritative: 永続層に一切書かない（S1-5 不変条件・最優先）。WORM のみ。
        if authority == SourceAuthority::NonAuthoritative {
            let audit_event_id = self
                .harness
                .audit(&ctx, "correction:conversation_only", None, Vec::new())
                .await
                .map_err(to_error)?;
            return Ok(Json(RecordOperatorFeedbackResponse {
                routing: CorrectionRouting::ConversationOnly,
                root_cause: None,
                audit_event_id,
            }));
        }
        let store = self.harness.store().map_err(to_error)?;
        // attempt_id が渡された場合は実在を検証する（provenance を偽装させない）
        if let Some(attempt_id) = &req.attempt_id {
            store
                .load_attempt(&ctx.schema, attempt_id)
                .await
                .map_err(to_error)?
                .ok_or_else(|| {
                    ErrorData::invalid_params(format!("unknown attempt_id: {attempt_id}"), None)
                })?;
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
                .await
                .map_err(to_error)?;
        }
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
                    (
                        "feedback_source".to_string(),
                        req.feedback_source.as_str().to_string(),
                    ),
                    ("corrected_answer".to_string(), req.corrected_answer.clone()),
                    ("routing".to_string(), routing.as_str().to_string()),
                    ("created_at".to_string(), chrono::Utc::now().to_rfc3339()),
                ],
            )
            .await
            .map_err(to_error)?;
        let audit_event_id = self
            .harness
            .audit(
                &ctx,
                format!(
                    "correction:{} root_cause:{}",
                    routing.as_str(),
                    root_cause.as_str()
                ),
                None,
                Vec::new(),
            )
            .await
            .map_err(to_error)?;
        Ok(Json(RecordOperatorFeedbackResponse {
            routing,
            root_cause: Some(root_cause.as_str().to_string()),
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
        // layer は 3 層判定の定義域（1..=3）以外を永続化させない
        if !(1..=3).contains(&req.layer) {
            return Err(ErrorData::invalid_params(
                format!("layer must be 1..=3, got {}", req.layer),
                None,
            ));
        }
        let store = self.harness.store().map_err(to_error)?;
        // case_id が渡された場合は実在を検証する（存在しない case への紐づけを拒否）。
        // 最新判定が Allowed でもエスカレーション記録は拒否しない: エスカレーションは
        // 常に安全側の行為であり、担当者の予防的エスカレーションを塞がない。
        if let Some(case_id) = &req.case_id {
            store
                .load_case(&ctx.schema, case_id)
                .await
                .map_err(to_error)?
                .ok_or_else(|| {
                    ErrorData::invalid_params(format!("unknown case_id: {case_id}"), None)
                })?;
        }
        let escalation_id = format!("esc-{}", uuid::Uuid::new_v4());
        store
            .record(
                &ctx.schema,
                "escalation_event",
                &escalation_id,
                vec![
                    ("escalation_id".to_string(), escalation_id.clone()),
                    (
                        "case_id".to_string(),
                        req.case_id.clone().unwrap_or_default(),
                    ),
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
            .audit(
                &ctx,
                "escalation_event",
                Some(req.route_to.clone()),
                vec![escalation_id.clone()],
            )
            .await
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
        // admission 判定（役割・語彙・NG 語）は Harness に一元化されている
        let signal_set = self
            .harness
            .admit_known_resolution(&ctx, &req.signals, &req.answer)
            .map_err(|err| ErrorData::invalid_request(err.to_string(), None))?;
        let store = self.harness.store().map_err(to_error)?;
        let new_kr = NewKnownResolution {
            signal_set,
            applicability: req.applicability.clone(),
            answer: req.answer.clone(),
            origin: req
                .origin_escalation_id
                .clone()
                .map(|id| format!("escalation:{id}"))
                .unwrap_or_else(|| "manual".to_string()),
            created_by: ctx.actor.sub.clone(),
            rationale_text: req.rationale_text.clone(),
            manual_section_keys: req.manual_section_keys.clone(),
        };
        let kr_id = store
            .insert_known_resolution(&ctx.schema, &new_kr)
            .await
            .map_err(to_error)?;
        let audit_event_id = self
            .harness
            .audit(&ctx, "kr_insert", None, vec![kr_id.clone()])
            .await
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
