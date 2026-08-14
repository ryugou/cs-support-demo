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
    manual_schema: crate::config::ManualSchemaKind,
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
    /// 参考として返す類似の過去事例（自 case は除外）。3 層判定の入力ではなく、
    /// あくまで client 向けの参考情報（S1-1 取得段）。
    pub related_cases: Vec<RelatedCaseJson>,
    /// 今ターンの signal 抽出モード（S1-11 改訂）: `lexicon_only` / `hybrid` /
    /// `lexicon_fallback`。WORM 監査にも同値を記録している。
    pub extraction_mode: String,
    /// 顧客へ送る返信文の**下書き**（デモ用。無効化時・生成失敗時・出口ゲートで弾かれた
    /// ときは null）。
    ///
    /// **これは検証前の下書きであって、承認された回答ではない。送信前に必ず担当者が内容を
    /// 確認すること。**
    ///
    /// `decision.decision` が escalate のとき、サーバは**内部マニュアル本文を生成に
    /// 渡していない**（内部資料の漏洩は構造的に防いでいる）。ただし**文面に解決方法が
    /// 現れないことは保証しない** — モデルの事前知識や、顧客が問い合わせ本文に書いた
    /// 手順が混じりうる。escalate の下書きは「取り次ぐ旨」として扱い、内容を鵜呑みに
    /// しないこと。
    ///
    /// **escalate のとき `hits` から手順を補ってはならない。** サーバが意図的に渡さな
    /// かった本文で下書きを補完することは、回答してはいけないと判定した場面で答える
    /// ことに等しく、判定そのものを無効化する。
    ///
    /// 開示範囲の正本は `decision.disclosure_scope` であり、この文面ではない。
    pub customer_reply_draft: Option<String>,
    /// `customer_reply_draft` が生成上限で**途中で切れている**か。
    ///
    /// **true のときは、そのまま顧客へ送ってはならない。** 理由は 2 つある。
    ///
    /// 1. **出口ゲートの統制が構造的に迂回されうる。** `egress_gate` は NG 語の部分文字列
    ///    一致で判定するため、切断が「絶対に治ります」を「絶対に治りま」で切ると**ゲートは
    ///    Pass する**。主張は読者に伝わるのに、統制だけが外れた状態になる
    /// 2. **末尾の注意書きが落ちうる。** 切れ目がたまたま句点の直後だと文面は完成して
    ///    見えるが、日本語のビジネス文は結び・注意書き（「電源を切ってから作業してください」
    ///    等）が末尾に来るため、**安全上の但し書きだけが欠けた案内**になっている可能性がある
    ///
    /// 補い方は `decision.decision` で変わる。**allowed のときに限り** `hits` の原文と
    /// 突き合わせて補うこと。**escalate のときは補わない** — 上記
    /// `customer_reply_draft` の注記のとおり、判定そのものを無効化するため。
    /// 切れた下書きは破棄し、取り次ぐ旨だけを書く。
    pub customer_reply_draft_truncated: bool,
    /// 今ターンでLLMが抽出した製品参照（Issue #28 §3.1 二段目。追加のLLM呼び出しは発生させない）。
    /// MCP側ではこの判定（foreign→取扱外）は行わない。
    pub product_references: Vec<ProductReferenceJson>,
}

/// `EvaluationOutcome::related_cases` の JSON ミラー（S1-1 取得段の参考情報）。
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RelatedCaseJson {
    pub case_id: String,
    pub question: String,
    /// 旧行には無いことがあるため空文字を許容する。
    pub last_decision: String,
}

impl From<crate::harness::RelatedCase> for RelatedCaseJson {
    fn from(c: crate::harness::RelatedCase) -> Self {
        Self {
            case_id: c.case_id,
            question: c.question,
            last_decision: c.last_decision,
        }
    }
}

/// `product_gate::ProductReference` の JSON ミラー（Issue #28 §3.1 二段目）。
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProductReferenceJson {
    pub surface: String,
    pub resolution: String,
    pub matched_model: Option<String>,
}

impl From<crate::harness::product_gate::ProductReference> for ProductReferenceJson {
    fn from(r: crate::harness::product_gate::ProductReference) -> Self {
        Self {
            // Issue #28 Suggestion 3: LLM 応答は検証なしに deserialize されるため（`llm.rs`
            // `parse_one_product_reference`）、surface が意図した最大40文字を超える出力を
            // 返す可能性がある。反射安全性の上限（`product_gate::MAX_REFLECTABLE_SURFACE_CHARS`
            // = 64 文字）で切り詰めてから MCP client（CS 担当）へ返す。
            surface: crate::harness::prompt_input::truncate_chars(
                &r.surface,
                crate::harness::product_gate::MAX_REFLECTABLE_SURFACE_CHARS,
            ),
            resolution: match r.resolution {
                crate::harness::product_gate::ProductReferenceResolution::Matched => "matched",
                crate::harness::product_gate::ProductReferenceResolution::Ambiguous => "ambiguous",
                crate::harness::product_gate::ProductReferenceResolution::Foreign => "foreign",
            }
            .to_string(),
            matched_model: r.matched_model,
        }
    }
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
    /// マニュアル出典 section（manual_v1: BASED_ON → ManualSection / legacy: BECAUSE → section）。
    /// 旧 field 名 `rationale_section_keys` は deprecated alias として受理する（後方互換）。
    #[serde(default, alias = "rationale_section_keys")]
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
    pub fn new(
        schema: String,
        tools: ToolService,
        harness: Arc<Harness>,
        manual_schema: crate::config::ManualSchemaKind,
    ) -> Self {
        Self {
            schema,
            tools,
            harness,
            manual_schema,
        }
    }

    /// manual_schema=ManualV1 の read tool 分岐共通ヘルパー。未設定は構成ミスとして internal_error。
    fn manual_store(&self) -> Result<&crate::manual::retrieval::ManualStore, ErrorData> {
        self.harness
            .manual
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("manual store not configured", None))
    }

    /// 全 tool の共通入口。認証 → scope 強制 → RequestContext（S1-1 前半）。
    /// identity は axum の Google OAuth ミドルウェア（`oauth::middleware::require_google_auth`）が
    /// 検証済みで `http::request::Parts` の extensions に注入している
    /// （`oauth::VerifiedIdentity`。安定した `sub` + 認証時点の email）。
    /// ここに値が無いのはミドルウェアの配線漏れ・構成ミスであり、fail closed で拒否する。
    fn begin(&self, extensions: &rmcp::model::Extensions) -> Result<RequestContext, ErrorData> {
        let identity = extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<crate::oauth::VerifiedIdentity>())
            .cloned();
        let identity = identity.ok_or_else(|| {
            tracing::warn!(
                reason = "missing_verified_identity",
                "auth rejected at begin"
            );
            ErrorData::invalid_request("unauthenticated".to_string(), None)
        })?;
        self.harness
            .begin(&identity, &self.schema, self.manual_schema)
            .map_err(|err| {
                // allowlist 外 / scope 外の拒否は無音にしない。sub / email は actor 識別に
                // 必要な監査情報でありログ可（token 本体ではない）。token は絶対にログしない。
                tracing::warn!(
                    reason = "unregistered_or_unscoped",
                    sub = %identity.sub,
                    email = %identity.email,
                    error = %err,
                    "authorization denied"
                );
                ErrorData::invalid_request(err.to_string(), None)
            })
    }

    #[tool(
        name = "resolve_product",
        description = "商品名・型番・顧客表現から候補 product を返す。照合は正規化+部分一致(fuzzy)。manual_v1 テナントかつ vector route 有効時は意味検索(embedding)による近傍候補も統合する。reason: normalized_match | fuzzy_match | semantic_nearby。"
    )]
    async fn resolve_product(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<ResolveProductRequest>,
    ) -> Result<Json<ResolveProductResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let (retrieved, body) = match ctx.manual_schema {
            crate::config::ManualSchemaKind::ManualV1 => {
                let store = self.manual_store()?;
                let manual_candidates = store
                    .resolve_product(&ctx.schema, &req.text, self.harness.vector_route_enabled)
                    .await
                    .map_err(to_error)?;
                let retrieved = manual_candidates
                    .iter()
                    .map(|c| {
                        crate::manual::schema_ids::manual_node_id(
                            &ctx.schema,
                            crate::manual::schema_ids::KIND_PRODUCT,
                            &c.model,
                        )
                    })
                    .collect();
                let candidates = manual_candidates
                    .into_iter()
                    .map(|c| ProductCandidate {
                        product_key: c.model.clone(),
                        name_ja: c.name.clone(),
                        name_en: c.name,
                        model: Some(c.model),
                        score: c.score,
                        reason: c.reason,
                    })
                    .collect();
                let body = ResolveProductResponse { candidates };
                (retrieved, body)
            }
            crate::config::ManualSchemaKind::LegacySection => {
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
                let body = ResolveProductResponse { candidates };
                (retrieved, body)
            }
        };
        self.harness
            .audit_with_nodes(
                &ctx,
                "read:resolve_product",
                None,
                Vec::new(),
                retrieved,
                None,
            )
            .await
            .map_err(to_error)?;
        Ok(Json(body))
    }

    #[tool(
        name = "search_manual",
        description = "日本語 query_ja でマニュアル本文を検索し、breadcrumb と出典（manual_v1: source_url / legacy: 英語原文 fallback）を返す。認証 actor の scope 内のみ検索される。"
    )]
    async fn search_manual(
        &self,
        extensions: rmcp::model::Extensions,
        Parameters(req): Parameters<SearchManualRequest>,
    ) -> Result<Json<SearchManualResponse>, ErrorData> {
        let ctx = self.begin(&extensions)?;
        let (retrieved, body, extraction_mode) = match ctx.manual_schema {
            crate::config::ManualSchemaKind::ManualV1 => {
                let store = self.manual_store()?;
                // evaluate と同じ抽出口を使う（lexicon 単独直呼びをやめ、LLM ハイブリッド
                // 抽出とプレビューの signal 集合を一致させる。S1-11 followup）。
                let crate::harness::extraction::ExtractionResult {
                    signals: query_signals,
                    mode: query_extraction_mode,
                    ..
                } = self.harness.extractor.extract(&req.query_ja, None).await;
                let top_k = req.top_k.unwrap_or(5).max(1) as usize;
                // 意味検索（ベクトル経路）は urtect design §2.3: 合成の可否・最終スコアは
                // 決定論の search が握る。ここでは候補材料を用意するだけ。
                let vector_hits = store
                    .vector_hits(
                        self.harness.vector_route_enabled,
                        &ctx.schema,
                        &req.query_ja,
                        top_k,
                    )
                    .await;
                let manual_hits = store
                    .search(
                        &ctx.schema,
                        &req.query_ja,
                        &query_signals,
                        req.product_key.as_deref(),
                        top_k,
                        &vector_hits,
                    )
                    .await
                    .map_err(to_error)?;
                let retrieved = manual_hits
                    .iter()
                    .map(|h| {
                        crate::manual::schema_ids::manual_node_id(
                            &ctx.schema,
                            crate::manual::schema_ids::KIND_SECTION,
                            &h.section_key,
                        )
                    })
                    .collect();
                let hits = manual_hits.into_iter().map(SectionHit::from).collect();
                let body = SearchManualResponse { hits };
                (retrieved, body, Some(query_extraction_mode))
            }
            crate::config::ManualSchemaKind::LegacySection => {
                // legacy 経路は signal 抽出を使わない（従来挙動を変えない）。
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
                let body = SearchManualResponse { hits };
                (retrieved, body, None)
            }
        };
        self.harness
            .audit_with_nodes(
                &ctx,
                "read:search_manual",
                None,
                Vec::new(),
                retrieved,
                extraction_mode,
            )
            .await
            .map_err(to_error)?;
        Ok(Json(body))
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
        let (retrieved_id, body) = match ctx.manual_schema {
            crate::config::ManualSchemaKind::ManualV1 => {
                let store = self.manual_store()?;
                let manual_view = store
                    .get_section(&ctx.schema, &req.section_key)
                    .await
                    .map_err(to_error)?;
                let retrieved_id = crate::manual::schema_ids::manual_node_id(
                    &ctx.schema,
                    crate::manual::schema_ids::KIND_SECTION,
                    &req.section_key,
                );
                let body = SectionView {
                    section: manual_view.section,
                    ancestors: manual_view.ancestors,
                    children: manual_view.children,
                    references: Vec::new(),
                    based_on_rationale: manual_view.based_on_rationale,
                };
                (retrieved_id, body)
            }
            crate::config::ManualSchemaKind::LegacySection => {
                let view = self
                    .tools
                    .get_section(&ctx.schema, &req.section_key)
                    .await
                    .map_err(to_error)?;
                let retrieved_id = crate::ingest::section_node_id(&ctx.schema, &req.section_key);
                (retrieved_id, view)
            }
        };
        self.harness
            .audit_with_nodes(
                &ctx,
                "read:get_section",
                None,
                Vec::new(),
                vec![retrieved_id],
                None,
            )
            .await
            .map_err(to_error)?;
        Ok(Json(body))
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
        let (retrieved_id, view) = match ctx.manual_schema {
            crate::config::ManualSchemaKind::ManualV1 => {
                let store = self.manual_store()?;
                let view = store
                    .get_product(&ctx.schema, &req.product_key)
                    .await
                    .map_err(to_error)?;
                let retrieved_id = crate::manual::schema_ids::manual_node_id(
                    &ctx.schema,
                    crate::manual::schema_ids::KIND_PRODUCT,
                    &req.product_key,
                );
                (retrieved_id, view)
            }
            crate::config::ManualSchemaKind::LegacySection => {
                let view = self
                    .tools
                    .get_product(&ctx.schema, &req.product_key)
                    .await
                    .map_err(to_error)?;
                let retrieved_id = crate::ingest::product_node_id(&ctx.schema, &req.product_key);
                (retrieved_id, view)
            }
        };
        self.harness
            .audit_with_nodes(
                &ctx,
                "read:get_product",
                None,
                Vec::new(),
                vec![retrieved_id],
                None,
            )
            .await
            .map_err(to_error)?;
        Ok(Json(view))
    }

    #[tool(
        name = "evaluate_answerability",
        // **この description に書くのは「client が振る舞いを変えるべきこと」だけにする。**
        // 理由・背景は返却型の field description（`EvaluateAnswerabilityResponse`）へ置く。
        //
        // tool description はモデルへ確実に届く唯一の経路なので、放っておくと
        // 説明が全部ここへ集まって、判断を変える指示が背景説明に埋没する。
        // 逆に field description は届くか不明なので、**振る舞いを変える指示を
        // そちらだけに置いてはならない**（この非対称を逆向きに踏んだのが W-10）。
        //
        // 分節のラベル（【…】）は、呼び出し方の要件（case_id）と返却物の処理を
        // モデルが取り違えないために置いている。
        description = "顧客質問を 3 層判定（明示ルール → 禁止領域 → 回答可能性）にかけ、回答可否・エスカレーション判定・根拠を返す。回答系フローの必須入口。マルチターンの問い合わせでは前回の case_id を渡すこと（累積条件で毎回再判定される）。【返却された customer_reply_draft の扱い】customer_reply_draft は検証前の下書きであり、承認された回答ではない。customer_reply_draft_truncated が true の下書きは生成上限で途中で切れているため、そのまま顧客へ送らないこと。decision.decision が allowed の場合に限り、hits と突き合わせて補完してよい。decision.decision が escalate の場合は、解決方法・手順を一切補わず、取り次ぐ旨に留めること（判定を無効化するため）。"
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
                // MCP 経路は会話履歴を持たない（呼び出し側は case_id による signal 累積で
                // マルチターンを扱う）。応答生成 API（/api/reply）だけが履歴を渡す。
                &[],
                // MCP 経路は会話フロー v1.1 design doc の適用範囲外（§1: 適用は /api/reply
                // 経路のみ）。`case_id` は複数ターンで Some になりうるが、`api.rs::is_continuation`
                // のヒューリスティック（history 非空 or case_id が Some）をここに流用すると、
                // MCP の 2 ターン目以降を誤って「継続」扱いにしてしまう（history は常に空な
                // ので case_id だけで継続判定することになる）。design doc の意図（MCP は常に
                // 初回扱い）に反するため、常にリテラル false を渡す。
                false,
                // 未知 case_id は Err のまま維持する（従来どおり）。/api/reply 限定の
                // fallback（design doc §2）を MCP 経路まで広げると、CS 担当の入力ミスが
                // 黙って新規 case へ合流し、会話層の累積 signal が失われたまま気づけなくなる。
                crate::harness::UnknownCaseIdPolicy::Reject,
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
            related_cases: outcome
                .related_cases
                .into_iter()
                .map(RelatedCaseJson::from)
                .collect(),
            extraction_mode: outcome.extraction_mode.as_str().to_string(),
            customer_reply_draft: outcome.customer_reply_draft,
            customer_reply_draft_truncated: outcome.customer_reply_draft_truncated,
            product_references: outcome
                .product_references
                .into_iter()
                .map(ProductReferenceJson::from)
                .collect(),
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
        // evaluate と同じ抽出口を使う（lexicon 単独直呼びをやめ、evaluate で適用される
        // KR がこのプレビューでは当たらない不整合を解消する。S1-11 followup）。
        let crate::harness::extraction::ExtractionResult {
            signals: question_signals,
            mode: extraction_mode,
            ..
        } = self.harness.extractor.extract(&req.question, None).await;
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
                Some(extraction_mode),
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
            .audit_with_nodes(
                &ctx,
                "read:search_past_cases",
                None,
                Vec::new(),
                retrieved,
                None,
            )
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
                    // actor は安定 ID（google-sub:{sub}）で人間には読めないため、
                    // 回答履歴を追う担当者向けに当時の email も併記する（加算属性）。
                    ("actor_email".to_string(), ctx.actor.email.clone()),
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
        // answer_evidence（S1-2）: 判定が使った根拠キーを evaluate 側が case に記録済みのため、
        // それを読み出して attempt に紐づく evidence として追記する。escalate 済み case は
        // last_evidence_keys が空でここに来ないため、append_answer_evidence 側で無音スキップされる。
        let evidence_case_attrs = store
            .load_case(&ctx.schema, &req.case_id)
            .await
            .map_err(to_error)?
            .unwrap_or_default();
        let last_evidence_keys = evidence_case_attrs
            .get("last_evidence_keys")
            .cloned()
            .unwrap_or_default();
        let last_evidence_kind = evidence_case_attrs
            .get("last_evidence_kind")
            .cloned()
            .unwrap_or_default();
        // kind が空だと evidence lineage が曖昧になる（古い/部分移行データ対策）。
        // keys があるのに kind が空なら append せず警告に留める（不明瞭な evidence を作らない）。
        if !last_evidence_keys.is_empty() && last_evidence_kind.is_empty() {
            tracing::warn!(
                case_id = %req.case_id,
                "case has last_evidence_keys but empty last_evidence_kind; skipping answer_evidence append"
            );
        } else {
            let evidence_items: Vec<(String, String)> =
                crate::harness::knowledge::csv_list(&last_evidence_keys)
                    .into_iter()
                    .map(|key| (key, last_evidence_kind.clone()))
                    .collect();
            store
                .append_answer_evidence(&ctx.schema, &attempt_id, &evidence_items)
                .await
                .map_err(to_error)?;
        }
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
                    // actor は安定 ID（google-sub:{sub}）で人間には読めないため、
                    // 訂正の出所を追う担当者向けに当時の email も併記する（加算属性）。
                    ("actor_email".to_string(), ctx.actor.email.clone()),
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
                    // エスカレーションを受け取る担当者が読む対象そのもの。安定 ID だけでは
                    // 「誰が上げたか」が引けないため、当時の email を併記する（加算属性）。
                    ("actor_email".to_string(), ctx.actor.email.clone()),
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
            .admit_known_resolution(
                &ctx,
                &req.signals,
                &req.answer,
                req.rationale_text.as_deref(),
                &req.manual_section_keys,
            )
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
            created_by_email: ctx.actor.email.clone(),
            rationale_text: req.rationale_text.clone(),
            manual_section_keys: req.manual_section_keys.clone(),
        };
        let kr_id = store
            .insert_known_resolution(&ctx.schema, &new_kr, ctx.manual_schema)
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ProductReferenceJson::from（Issue #28 Suggestion 3） ----

    #[test]
    fn product_reference_json_from_truncates_a_surface_longer_than_the_reflectable_limit() {
        // LLM 応答は検証なしに deserialize されるため、意図した最大40文字を超える surface を
        // 返しうる。MCP client（CS 担当）へ渡す前に反射安全性の上限（64文字）で切り詰める。
        // `truncate_chars` は切り詰め時に省略記号を1文字付与するため（
        // `harness::prompt_input::tests::truncate_chars_appends_ellipsis_on_char_boundary`
        // が同じ挙動を固定している）、切り詰め後の文字数は上限+1になる。
        let long_surface = "A".repeat(100);
        let reference = crate::harness::product_gate::ProductReference {
            surface: long_surface,
            resolution: crate::harness::product_gate::ProductReferenceResolution::Matched,
            matched_model: Some("ADC-V724".to_string()),
        };
        let json = ProductReferenceJson::from(reference);
        let limit = crate::harness::product_gate::MAX_REFLECTABLE_SURFACE_CHARS;
        assert_eq!(
            json.surface.chars().count(),
            limit + 1,
            "surface must be truncated to the reflectable limit ({limit} chars) plus the \
             ellipsis marker, got {} chars: {:?}",
            json.surface.chars().count(),
            json.surface
        );
        assert!(json.surface.ends_with('…'));
    }
}
