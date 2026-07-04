use crate::{
    harness::{Harness, RequestContext},
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
            signals: outcome.signals.iter().map(|s| s.as_str().to_string()).collect(),
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
