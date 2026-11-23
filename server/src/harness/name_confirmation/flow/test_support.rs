use crate::harness::decision::{AnswerDecision, DisclosureScope, EscalateReason};
use crate::harness::extraction::ExtractionMode;
use crate::harness::name_confirmation::catalog::{NameCatalog, NameTarget};
use crate::harness::name_confirmation::flow::NameConfirmation;
use crate::harness::name_confirmation::matcher::name_key;
use crate::harness::name_confirmation::state::{AskReason, ConfirmationState, PendingMention};
use crate::harness::name_confirmation::test_fixtures::catalog_of;
use crate::harness::product_gate::ProductAllowlist;
use crate::harness::reply::{ReplyHistoryRole, ReplyHistoryTurn};
use crate::harness::signal::SignalSet;
use crate::harness::{DecisionBasis, EvaluationOutcome};

pub(super) fn names_with(
    allowlist: &ProductAllowlist,
    state: ConfirmationState,
) -> NameConfirmation {
    names_in(catalog_of(allowlist), state)
}

pub(super) fn names_in(catalog: NameCatalog, state: ConfirmationState) -> NameConfirmation {
    NameConfirmation {
        llm_configured: true,
        enabled: true,
        catalog,
        state,
    }
}

pub(super) fn unidentified_outcome() -> EvaluationOutcome {
    EvaluationOutcome {
        decision: AnswerDecision::Escalate {
            reason: EscalateReason::ProductUnidentified,
            layer: 3,
            route_to: "support_desk".to_string(),
            disclosure_scope: DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
            hearing: None,
            customer_ack: None,
            known_resolution_id: None,
        },
        signals: SignalSet::new(),
        accumulated_signals: SignalSet::new(),
        case_id: "case-test".to_string(),
        clarification_allowed: true,
        hits: Vec::new(),
        audit_event_id: "audit-test".to_string(),
        related_cases: Vec::new(),
        extraction_mode: ExtractionMode::Hybrid,
        customer_reply_draft: None,
        customer_reply_draft_truncated: false,
        product_references: Vec::new(),
        new_signals: SignalSet::new(),
        handoff_items: Vec::new(),
        partial_answer: false,
        decision_basis: DecisionBasis::Accumulated,
        applicable_known_resolution: None,
        immediate_handoff: false,
    }
}

pub(super) fn customer(text: &str) -> ReplyHistoryTurn {
    ReplyHistoryTurn {
        role: ReplyHistoryRole::Customer,
        text: text.to_string(),
    }
}

pub(super) fn assistant(text: &str) -> ReplyHistoryTurn {
    ReplyHistoryTurn {
        role: ReplyHistoryRole::Assistant,
        text: text.to_string(),
    }
}

pub(super) fn pending(
    surface: &str,
    suggestions: Vec<NameTarget>,
    reason: AskReason,
) -> PendingMention {
    PendingMention {
        surface: surface.to_string(),
        surface_key: name_key(surface),
        suggestions,
        asks: 1,
        reason,
    }
}

pub(super) fn presented_keys(names: &NameConfirmation, message: &str) -> Vec<String> {
    names
        .plan(&unidentified_outcome(), message, &[])
        .map(|presentation| {
            presentation
                .mentions
                .iter()
                .map(|mention| mention.pending.surface_key.clone())
                .collect()
        })
        .unwrap_or_default()
}
