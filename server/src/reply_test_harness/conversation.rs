use std::collections::VecDeque;

use axum::http::StatusCode;

use super::llm_stub::{LlmKind, LlmRequest};
use super::reply_harness::ReplyHarness;

const MAX_HISTORY_TURNS: usize = 20;
const CUSTOMER_ROLE: &str = "customer";
const ASSISTANT_ROLE: &str = "assistant";

#[derive(Debug, Clone)]
pub(crate) struct Turn {
    pub index: usize,
    pub message: String,
    pub status: StatusCode,
    pub reply_text: String,
    pub case_id: String,
    pub case_reset: bool,
    pub error_body: Option<String>,
    pub reply_kind: Option<String>,
    pub decision: Option<String>,
    pub product_models: Vec<String>,
    pub llm_requests: Vec<LlmRequest>,
}

impl Turn {
    pub(crate) fn llm_requests_of(&self, kind: LlmKind) -> Vec<&LlmRequest> {
        self.llm_requests
            .iter()
            .filter(|r| r.kind == kind)
            .collect()
    }

    pub(crate) fn log_line(&self) -> String {
        format!(
            "turn={} status={} message={:?} reply_kind={:?} decision={:?} case_id={:?} \
             case_reset={} product_models={:?} reply_text={:?}",
            self.index,
            self.status.as_u16(),
            self.message,
            self.reply_kind,
            self.decision,
            self.case_id,
            self.case_reset,
            self.product_models,
            self.reply_text,
        )
    }

    fn failed(
        index: usize,
        message: &str,
        status: StatusCode,
        body: String,
        llm_requests: Vec<LlmRequest>,
    ) -> Self {
        Self {
            index,
            message: message.to_string(),
            status,
            reply_text: String::new(),
            case_id: String::new(),
            case_reset: false,
            error_body: Some(body),
            reply_kind: None,
            decision: None,
            product_models: Vec::new(),
            llm_requests,
        }
    }
}

struct ReplyBody {
    reply_text: String,
    case_id: String,
    case_reset: bool,
}

impl ReplyBody {
    fn parse(text: &str) -> Self {
        let parsed: serde_json::Value =
            serde_json::from_str(text).expect("a 200 reply body is JSON");
        Self {
            reply_text: parsed["reply_text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            case_id: parsed["case_id"].as_str().unwrap_or_default().to_string(),
            case_reset: parsed["case_reset"].as_bool().unwrap_or(false),
        }
    }
}

pub(crate) struct Conversation<'h> {
    harness: &'h ReplyHarness,
    project_id: String,
    case_id: Option<String>,
    history: VecDeque<(&'static str, String)>,
}

impl<'h> Conversation<'h> {
    pub(crate) fn new(harness: &'h ReplyHarness, project_id: &str) -> Self {
        Self {
            harness,
            project_id: project_id.to_string(),
            case_id: None,
            history: VecDeque::new(),
        }
    }

    pub(crate) async fn say(&mut self, message: &str) -> Turn {
        let index = self.next_index();
        let llm_before = self.harness.llm().request_count();
        let (status, text) = self
            .harness
            .post_reply_to(&self.project_id, self.request_body(message))
            .await;
        let llm_requests = self.harness.llm().requests()[llm_before..].to_vec();
        if status != StatusCode::OK {
            return Turn::failed(index, message, status, text, llm_requests);
        }
        let reply = ReplyBody::parse(&text);
        let turn = self.observe(index, message, status, reply, llm_requests);
        self.remember(&turn);
        turn
    }

    pub(crate) fn case_id(&self) -> Option<&str> {
        self.case_id.as_deref()
    }

    pub(crate) fn history(&self) -> Vec<(&'static str, String)> {
        self.history.iter().cloned().collect()
    }

    fn next_index(&self) -> usize {
        self.history
            .iter()
            .filter(|(role, _)| *role == CUSTOMER_ROLE)
            .count()
            + 1
    }

    fn request_body(&self, message: &str) -> serde_json::Value {
        let mut body = serde_json::json!({"message": message});
        if !self.history.is_empty() {
            body["history"] = self
                .history
                .iter()
                .map(|(role, text)| serde_json::json!({"role": role, "text": text}))
                .collect();
        }
        if let Some(case_id) = &self.case_id {
            body["case_id"] = serde_json::Value::String(case_id.clone());
        }
        body
    }

    fn observe(
        &self,
        index: usize,
        message: &str,
        status: StatusCode,
        reply: ReplyBody,
        llm_requests: Vec<LlmRequest>,
    ) -> Turn {
        let turns = self
            .harness
            .conversation_turns_in(&self.project_id, &reply.case_id);
        let recorded = turns.last();
        let reply_kind = recorded
            .and_then(|t| t.attr("reply_kind"))
            .map(ToString::to_string);
        let decision = recorded
            .and_then(|t| t.attr("audit_event_id"))
            .and_then(|id| self.harness.audit_decision(id));
        let product_models = self
            .harness
            .product_models_in(&self.project_id, &reply.case_id);
        Turn {
            index,
            message: message.to_string(),
            status,
            reply_text: reply.reply_text,
            case_id: reply.case_id,
            case_reset: reply.case_reset,
            error_body: None,
            reply_kind,
            decision,
            product_models,
            llm_requests,
        }
    }

    fn remember(&mut self, turn: &Turn) {
        if turn.case_reset {
            self.history.clear();
        }
        self.case_id = Some(turn.case_id.clone());
        self.history
            .push_back((CUSTOMER_ROLE, turn.message.clone()));
        self.history
            .push_back((ASSISTANT_ROLE, turn.reply_text.clone()));
        while self.history.len() > MAX_HISTORY_TURNS {
            self.history.pop_front();
        }
    }
}
