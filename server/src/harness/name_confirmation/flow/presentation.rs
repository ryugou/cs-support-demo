use anyhow::{anyhow, Result};

use super::{NameConfirmation, Presentation};
use crate::harness::extraction::ExtractionMode;
use crate::harness::name_confirmation::catalog::NameTarget;
use crate::harness::name_confirmation::confirmation_text::{confirmation_text, PresentedMention};
use crate::harness::name_confirmation::interpret::ReplyResolution;
use crate::harness::name_confirmation::matcher::{
    find_mentions, name_key, MentionMatch, NameMention,
};
use crate::harness::name_confirmation::state::{PendingMention, MAX_PENDING_MENTIONS};
use crate::harness::product_gate::{find_confirmed_foreign_reference, ProductAllowlist};
use crate::harness::reply::{customer_history_window, ReplyHistoryTurn};
use crate::harness::{EvaluationOutcome, Harness, RequestContext};

const NAME_CONFIRMATION_AUDIT_DECISION: &str = "name_confirmation";

impl NameConfirmation {
    pub(crate) fn out_of_scope_reply_model(
        &self,
        allowlist: &ProductAllowlist,
        message: &str,
        history: &[ReplyHistoryTurn],
        answered: Option<&ReplyResolution>,
    ) -> Option<String> {
        let presentable = self.presentable_mentions(message, history);
        if let Some(model) = allowlist
            .out_of_scope_mentions(message)
            .into_iter()
            .find(|model| !self.covers(model, &presentable))
        {
            return Some(model);
        }
        let resolution = answered?;
        if !resolution.confirmed.is_empty() || !self.state.pending().is_empty() {
            return None;
        }
        resolution
            .unresolved_surface
            .as_deref()
            .and_then(|surface| allowlist.first_out_of_scope_token(surface))
    }

    pub(crate) fn defers_second_stage(
        &self,
        outcome: &EvaluationOutcome,
        message: &str,
        history: &[ReplyHistoryTurn],
        allowlist: &ProductAllowlist,
    ) -> bool {
        let presentable = self.presentable_mentions(message, history);
        find_confirmed_foreign_reference(&outcome.product_references, message, allowlist)
            .confirmed
            .is_some_and(|reference| self.covers(reference.surface.trim(), &presentable))
    }

    pub(crate) fn plan(
        &self,
        outcome: &EvaluationOutcome,
        message: &str,
        history: &[ReplyHistoryTurn],
    ) -> Option<Presentation> {
        if outcome.immediate_handoff || outcome.extraction_mode == ExtractionMode::LexiconFallback {
            return None;
        }
        let mentions = self.presentable_mentions(message, history);
        (!mentions.is_empty()).then_some(Presentation { mentions })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn present(
        &mut self,
        harness: &Harness,
        ctx: &RequestContext,
        case_id: &str,
        presentation: Presentation,
        message: &str,
        is_continuation: bool,
        allowlist: &ProductAllowlist,
    ) -> Result<(String, String)> {
        let mut mentions = presentation.mentions;
        for mention in &mut mentions {
            mention.pending.asks += 1;
        }
        let drafter = harness.reply_drafter.as_ref().ok_or_else(|| {
            anyhow!("failed to present a name confirmation: no LLM is configured")
        })?;
        let text = confirmation_text(
            drafter,
            &harness.ng,
            harness.reply_draft_max_tokens,
            &mentions,
            message,
            is_continuation,
            &self.catalog,
            allowlist,
        )
        .await;
        self.state.replace_pending(
            mentions
                .into_iter()
                .map(|mention| mention.pending)
                .collect(),
        );
        self.save(harness, ctx, case_id, None).await?;
        let audited = harness
            .audit(
                ctx,
                NAME_CONFIRMATION_AUDIT_DECISION,
                None,
                vec![case_id.to_string()],
            )
            .await;
        Ok((text, audit_event_id_or_empty(audited, ctx, case_id)))
    }

    fn presentable_mentions(
        &self,
        message: &str,
        history: &[ReplyHistoryTurn],
    ) -> Vec<PresentedMention> {
        if !self.enabled {
            return Vec::new();
        }
        let current = find_mentions(message, &self.catalog);
        let written_now: Vec<&NameTarget> = current
            .iter()
            .filter(|mention| mention.matched == MentionMatch::Exact)
            .filter_map(NameMention::single_target)
            .collect();
        let mut mentions: Vec<PresentedMention> = self
            .state
            .pending()
            .iter()
            .map(|pending| PresentedMention {
                pending: pending.clone(),
                earlier_message: false,
            })
            .collect();
        let earlier: Vec<NameMention> = customer_history_window(history)
            .turns
            .iter()
            .flat_map(|turn| find_mentions(turn, &self.catalog))
            .collect();
        let found = current
            .iter()
            .map(|mention| (mention, false))
            .chain(earlier.iter().map(|mention| (mention, true)));
        for (mention, earlier_message) in found {
            if mentions.len() >= MAX_PENDING_MENTIONS {
                break;
            }
            if mentions
                .iter()
                .any(|presented| presented.pending.surface_key == mention.key)
            {
                continue;
            }
            let live = self
                .live_suggestions(mention)
                .into_iter()
                .filter(|target| !written_now.contains(&target))
                .collect();
            if let Some(pending) = PendingMention::first_ask(mention, live) {
                mentions.push(PresentedMention {
                    pending,
                    earlier_message,
                });
            }
        }
        mentions
    }

    fn covers(&self, notation: &str, presentable: &[PresentedMention]) -> bool {
        let key = name_key(notation);
        !key.is_empty()
            && (self.state.is_confirmed(&key)
                || presentable
                    .iter()
                    .any(|mention| mention.pending.surface_key == key))
    }

    fn live_suggestions(&self, mention: &NameMention) -> Vec<NameTarget> {
        let needs_confirmation = match mention.matched {
            MentionMatch::Typo => true,
            MentionMatch::Exact => mention.targets.len() > 1,
        };
        if !needs_confirmation || self.state.is_settled(&mention.key) {
            return Vec::new();
        }
        mention
            .targets
            .iter()
            .filter(|target| !self.state.is_denied(target))
            .cloned()
            .collect()
    }
}

fn audit_event_id_or_empty(audited: Result<String>, ctx: &RequestContext, case_id: &str) -> String {
    match audited {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(request_id = %ctx.request_id, case_id, error = ?err, "failed to audit a name confirmation; continuing with an empty audit_event_id");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests;
