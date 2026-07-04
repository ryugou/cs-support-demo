use crate::{
    mcp::ToolService,
    model::{ProductCandidate, ProductView, SectionHit, SectionView},
};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct CsSupportRmcpServer {
    schema: String,
    tools: ToolService,
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

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UpsertProductRequest {
    pub product_key: String,
    pub name_en: String,
    pub name_ja: String,
    pub model: Option<String>,
    pub status: Option<String>,
    pub description_en: Option<String>,
    pub description_ja: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UpsertSectionRequest {
    pub section_key: String,
    pub doc_id: String,
    pub product_key: String,
    pub order: i32,
    pub level: i32,
    pub title_en: String,
    pub title_ja: String,
    pub body_en: Option<String>,
    pub body_ja: Option<String>,
    pub translation_status: Option<String>,
    pub anchor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UpsertSpecRequest {
    pub product_key: String,
    pub key_slug: String,
    pub key_en: String,
    pub key_ja: String,
    pub value_en: String,
    pub value_ja: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ResolveProductResponse {
    pub candidates: Vec<ProductCandidate>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchManualResponse {
    pub hits: Vec<SectionHit>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UpsertResponse {
    pub upserted_nodes: i32,
}

#[tool_router]
impl CsSupportRmcpServer {
    pub fn new(schema: String, tools: ToolService) -> Self {
        Self { schema, tools }
    }

    #[tool(
        name = "resolve_product",
        description = "商品名・型番・顧客表現から候補 product を返す。aliases テーブルは使わない。"
    )]
    async fn resolve_product(
        &self,
        Parameters(req): Parameters<ResolveProductRequest>,
    ) -> Result<Json<ResolveProductResponse>, ErrorData> {
        self.tools
            .resolve_product(&self.schema, &req.text)
            .await
            .map(|candidates| Json(ResolveProductResponse { candidates }))
            .map_err(to_error)
    }

    #[tool(
        name = "search_manual",
        description = "日本語 query_ja で日本語マニュアル本文 body_ja を検索し、breadcrumb と英語原文 fallback を返す。"
    )]
    async fn search_manual(
        &self,
        Parameters(req): Parameters<SearchManualRequest>,
    ) -> Result<Json<SearchManualResponse>, ErrorData> {
        self.tools
            .search_manual(
                &self.schema,
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
        Parameters(req): Parameters<GetSectionRequest>,
    ) -> Result<Json<SectionView>, ErrorData> {
        self.tools
            .get_section(&self.schema, &req.section_key)
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
        Parameters(req): Parameters<GetProductRequest>,
    ) -> Result<Json<ProductView>, ErrorData> {
        self.tools
            .get_product(&self.schema, &req.product_key)
            .await
            .map(Json)
            .map_err(to_error)
    }
}

#[tool_handler]
impl ServerHandler for CsSupportRmcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "日本語カスタマーサポート向けの製品マニュアル検索 MCP。まず resolve_product で商品を絞り、search_manual / get_section / get_product で根拠を取得する。"
                .to_string(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

fn to_error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}
