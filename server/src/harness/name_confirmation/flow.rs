use std::borrow::Cow;

use anyhow::{anyhow, bail, Result};

use super::catalog::{NameCatalog, NameKind, NameTarget};
use super::confirmation_text::{confirmation_text, PresentedMention};
use super::interpret::{apply_reply, classify_reply, ReplyResolution};
use super::matcher::{find_mentions, MentionMatch, NameMention};
use super::state::{
    capped_suggestions, AskReason, ConfirmationState, PendingMention, MAX_PENDING_MENTIONS,
    STATE_ATTRIBUTE,
};
use crate::harness::case_switch::merge_product_models;
use crate::harness::extraction::ExtractionMode;
use crate::harness::product_gate::{
    find_confirmed_foreign_reference, is_safe_to_reflect, ProductAllowlist,
};
use crate::harness::reply::{customer_history_window, ReplyHistoryTurn};
use crate::harness::{conv_state_from_attrs, EvaluationOutcome, Harness, RequestContext};

const NAME_CONFIRMATION_AUDIT_DECISION: &str = "name_confirmation";
const PRODUCT_MODELS_ATTRIBUTE: &str = "product_models";
const CONFIRMED_PRODUCT_FACT: &str = "- 確認済みの製品: ";
const CONFIRMED_COMPANY_FACT: &str = "- 確認済みの社名: ";

pub(crate) struct NameConfirmation {
    enabled: bool,
    catalog: NameCatalog,
    state: ConfirmationState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingAnswer {
    Continue { context_text: Option<String> },
    OutOfScope { model: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Presentation {
    mentions: Vec<PresentedMention>,
}

impl NameConfirmation {
    pub(crate) async fn load(
        harness: &Harness,
        ctx: &RequestContext,
        case_id: Option<&str>,
        allowlist: &ProductAllowlist,
    ) -> Result<Self> {
        let catalog = NameCatalog::products_and_companies(allowlist);
        let llm_configured = harness.reply_drafter.is_some();
        let stored = match case_id {
            Some(id) => harness.store()?.load_case(&ctx.schema, id).await?,
            None => None,
        };
        let Some(attributes) = stored else {
            return Ok(Self {
                enabled: llm_configured,
                catalog,
                state: ConfirmationState::default(),
            });
        };
        Ok(Self {
            enabled: llm_configured && !conv_state_from_attrs(&attributes).is_already_escalated(),
            catalog,
            state: ConfirmationState::from_case_attributes(&attributes)?,
        })
    }

    pub(crate) fn out_of_scope_reply_model(
        &self,
        allowlist: &ProductAllowlist,
        message: &str,
    ) -> Option<String> {
        allowlist
            .out_of_scope_mentions(message)
            .into_iter()
            .find(|model| !self.can_suggest_for(model))
    }

    fn can_suggest_for(&self, written: &str) -> bool {
        self.enabled
            && find_mentions(written, &self.catalog)
                .iter()
                .any(|mention| !self.live_suggestions(mention).is_empty())
    }

    pub(crate) fn start_new_case(&mut self) {
        self.state = ConfirmationState::default();
    }

    pub(crate) fn confirmed_product_model(&self) -> Option<&str> {
        self.state.confirmed_product_model()
    }

    pub(crate) async fn answer_pending(
        &mut self,
        harness: &Harness,
        ctx: &RequestContext,
        case_id: &str,
        message: &str,
        history: &[ReplyHistoryTurn],
        allowlist: &ProductAllowlist,
    ) -> Result<PendingAnswer> {
        if !self.enabled || self.state.pending().is_empty() {
            return Ok(PendingAnswer::Continue { context_text: None });
        }
        let drafter = harness.reply_drafter.as_ref().ok_or_else(|| {
            anyhow!("failed to classify a name confirmation reply: no LLM is configured")
        })?;
        let resolution = match classify_reply(drafter, self.state.pending(), message).await {
            Ok(classification) => {
                apply_reply(&mut self.state, &classification, message, &self.catalog)
            }
            Err(err) => {
                tracing::warn!(request_id = %ctx.request_id, error = %format_args!("{err:#}"), "failed to classify the reply to a name confirmation; dismissing the confirmation");
                for pending in self.state.take_pending() {
                    self.state.dismiss(&pending.surface_key);
                }
                ReplyResolution::default()
            }
        };
        let confirmed_product = resolution
            .confirmed
            .iter()
            .rev()
            .find(|(_, target)| target.kind == NameKind::Product)
            .map(|(_, target)| target.id.as_str());
        self.save(harness, ctx, case_id, confirmed_product).await?;
        let out_of_scope = self.out_of_scope_model_after_answer(&resolution, message, allowlist);
        Ok(match out_of_scope {
            Some(model) => PendingAnswer::OutOfScope { model },
            None => PendingAnswer::Continue {
                context_text: rewritten_context(history, message, &resolution.confirmed),
            },
        })
    }

    fn out_of_scope_model_after_answer(
        &self,
        resolution: &ReplyResolution,
        message: &str,
        allowlist: &ProductAllowlist,
    ) -> Option<String> {
        if !resolution.confirmed.is_empty() || !self.state.pending().is_empty() {
            return None;
        }
        self.out_of_scope_reply_model(allowlist, message)
            .or_else(|| {
                resolution
                    .unresolved_surface
                    .as_deref()
                    .and_then(|surface| allowlist.first_out_of_scope_token(surface))
            })
    }

    pub(crate) fn plan(
        &self,
        outcome: &EvaluationOutcome,
        message: &str,
        history: &[ReplyHistoryTurn],
    ) -> Option<Presentation> {
        if !self.enabled
            || outcome.immediate_handoff
            || outcome.extraction_mode == ExtractionMode::LexiconFallback
        {
            return None;
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
            let listed = mentions
                .iter()
                .any(|presented| presented.pending.surface_key == mention.key);
            let live: Vec<NameTarget> = self
                .live_suggestions(mention)
                .into_iter()
                .filter(|target| !written_now.contains(&target))
                .collect();
            if listed || live.is_empty() {
                continue;
            }
            mentions.push(PresentedMention {
                pending: PendingMention {
                    surface: mention.surface.clone(),
                    surface_key: mention.key.clone(),
                    suggestions: capped_suggestions(live),
                    asks: 0,
                    reason: AskReason::First,
                },
                earlier_message,
            });
        }
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
        let audit_event_id = match harness
            .audit(
                ctx,
                NAME_CONFIRMATION_AUDIT_DECISION,
                None,
                vec![case_id.to_string()],
            )
            .await
        {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(request_id = %ctx.request_id, case_id, error = ?err, "failed to audit a name confirmation; continuing with an empty audit_event_id");
                String::new()
            }
        };
        Ok((text, audit_event_id))
    }

    pub(crate) fn with_confirmed_names(&self, known_facts: String) -> String {
        let mut lines: Vec<String> = Vec::new();
        for confirmed in self.state.confirmed() {
            let line = match confirmed.target.kind {
                NameKind::Product => {
                    format!("{CONFIRMED_PRODUCT_FACT}{}", confirmed.target.display)
                }
                NameKind::Company => {
                    format!("{CONFIRMED_COMPANY_FACT}{}", confirmed.target.display)
                }
            };
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
        if known_facts.is_empty() {
            return lines.join("\n");
        }
        lines.insert(0, known_facts);
        lines.join("\n")
    }

    pub(crate) fn hearing_allowlist<'a>(
        &self,
        allowlist: &'a ProductAllowlist,
    ) -> Cow<'a, ProductAllowlist> {
        let denied = self.state.denied_ids(NameKind::Product);
        if denied.is_empty() {
            Cow::Borrowed(allowlist)
        } else {
            Cow::Owned(allowlist.excluding_models(&denied))
        }
    }

    pub(crate) fn withhold_denied_names(&self, text: String, fallback: &str) -> String {
        if self.mentions_denied_name(&text) {
            tracing::warn!(
                "a hearing names a candidate the customer denied; using the fallback hearing text"
            );
            fallback.to_string()
        } else {
            text
        }
    }

    pub(crate) fn defers_second_stage(
        &self,
        outcome: &EvaluationOutcome,
        message: &str,
        allowlist: &ProductAllowlist,
    ) -> bool {
        find_confirmed_foreign_reference(&outcome.product_references, message, allowlist)
            .confirmed
            .is_some_and(|reference| self.can_suggest_for(reference.surface.trim()))
    }

    fn mentions_denied_name(&self, text: &str) -> bool {
        find_mentions(text, &self.catalog)
            .iter()
            .filter(|mention| mention.matched == MentionMatch::Exact)
            .any(|mention| {
                mention
                    .targets
                    .iter()
                    .any(|target| self.state.is_denied(target))
            })
    }

    fn live_suggestions(&self, mention: &NameMention) -> Vec<NameTarget> {
        let needs_confirmation = match mention.matched {
            MentionMatch::Typo => true,
            MentionMatch::Exact => mention.targets.len() > 1,
        };
        if !needs_confirmation
            || self.state.is_settled(&mention.key)
            || !is_safe_to_reflect(&mention.surface)
        {
            return Vec::new();
        }
        mention
            .targets
            .iter()
            .filter(|target| !self.state.is_denied(target))
            .cloned()
            .collect()
    }

    async fn save(
        &self,
        harness: &Harness,
        ctx: &RequestContext,
        case_id: &str,
        confirmed_product: Option<&str>,
    ) -> Result<()> {
        let store = harness.store()?;
        let Some(mut attributes) = store.load_case(&ctx.schema, case_id).await? else {
            bail!("failed to save the name confirmation state: case {case_id} is missing");
        };
        attributes.insert(STATE_ATTRIBUTE.to_string(), self.state.to_attribute()?);
        if let Some(model) = confirmed_product {
            let merged = merge_product_models(
                attributes
                    .get(PRODUCT_MODELS_ATTRIBUTE)
                    .map_or("", String::as_str),
                &[model.to_string()],
            );
            attributes.insert(PRODUCT_MODELS_ATTRIBUTE.to_string(), merged);
        }
        store
            .record(
                &ctx.schema,
                "support_case",
                case_id,
                attributes.into_iter().collect(),
            )
            .await
    }
}

fn rewritten_context(
    history: &[ReplyHistoryTurn],
    message: &str,
    confirmed: &[(String, NameTarget)],
) -> Option<String> {
    let rewritten: Vec<String> = customer_history_window(history)
        .turns
        .into_iter()
        .filter(|turn| {
            confirmed
                .iter()
                .any(|(surface, _)| turn.contains(surface.as_str()))
        })
        .map(|turn| {
            confirmed.iter().fold(turn, |turn, (surface, target)| {
                turn.replace(surface.as_str(), &target.display)
            })
        })
        .collect();
    (!rewritten.is_empty()).then(|| format!("{}\n{message}", rewritten.join("\n")))
}

#[cfg(test)]
mod tests;
