mod hearing;
mod presentation;

pub(crate) use hearing::identified_product_models;

use anyhow::{bail, Result};

use super::catalog::{NameCatalog, NameKind, NameTarget};
use super::confirmation_text::PresentedMention;
use super::interpret::{apply_reply, classify_reply, ReplyResolution};
use super::state::{ConfirmationState, STATE_ATTRIBUTE};
use crate::harness::case_switch::merge_product_models;
use crate::harness::product_gate::ProductAllowlist;
use crate::harness::reply::{customer_history_window, ReplyHistoryRole, ReplyHistoryTurn};
use crate::harness::{conv_state_from_attrs, Harness, RequestContext};

const PRODUCT_MODELS_ATTRIBUTE: &str = "product_models";

pub(crate) struct NameConfirmation {
    llm_configured: bool,
    enabled: bool,
    catalog: NameCatalog,
    state: ConfirmationState,
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
        let llm_configured = harness.reply_drafter.is_some();
        let stored = match case_id {
            Some(id) => harness.store()?.load_case(&ctx.schema, id).await?,
            None => None,
        };
        let companies = if llm_configured {
            harness.registered_companies(&ctx.schema).await
        } else {
            Vec::new().into()
        };
        let catalog = NameCatalog::products_and_companies(allowlist, &companies);
        let Some(attributes) = stored else {
            return Ok(Self {
                llm_configured,
                enabled: llm_configured,
                catalog,
                state: ConfirmationState::default(),
            });
        };
        Ok(Self {
            llm_configured,
            enabled: llm_configured && !conv_state_from_attrs(&attributes).is_already_escalated(),
            catalog,
            state: ConfirmationState::from_case_attributes(&attributes)?,
        })
    }

    pub(crate) fn start_new_case(&mut self) {
        self.enabled = self.llm_configured;
        self.state = ConfirmationState::default();
    }

    pub(crate) fn confirmed_product_model(&self) -> Option<&str> {
        self.state.confirmed_product_model()
    }

    pub(crate) async fn interpret_reply(
        &mut self,
        harness: &Harness,
        ctx: &RequestContext,
        message: &str,
        history: &[ReplyHistoryTurn],
    ) -> Option<ReplyResolution> {
        let drafter = harness.reply_drafter.as_ref()?;
        if !self.enabled || self.state.pending().is_empty() {
            return None;
        }
        let confirmation = history
            .iter()
            .rev()
            .find(|turn| turn.role == ReplyHistoryRole::Assistant)
            .map(|turn| turn.text.as_str());
        match classify_reply(drafter, self.state.pending(), confirmation, message).await {
            Ok(classification) => Some(apply_reply(
                &mut self.state,
                &classification,
                message,
                &self.catalog,
            )),
            Err(err) => {
                tracing::warn!(request_id = %ctx.request_id, error = %format_args!("{err:#}"), "failed to classify the reply to a name confirmation; dismissing the confirmation");
                for pending in self.state.take_pending() {
                    self.state.dismiss(&pending.surface_key);
                }
                Some(ReplyResolution::default())
            }
        }
    }

    pub(crate) async fn save_reply(
        &self,
        harness: &Harness,
        ctx: &RequestContext,
        case_id: &str,
        resolution: &ReplyResolution,
    ) -> Result<()> {
        let confirmed_product = resolution
            .confirmed
            .iter()
            .rev()
            .find(|(_, target)| target.kind == NameKind::Product)
            .map(|(_, target)| target.id.as_str());
        self.save(harness, ctx, case_id, confirmed_product).await
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

pub(crate) fn rewritten_context(
    history: &[ReplyHistoryTurn],
    message: &str,
    resolution: &ReplyResolution,
) -> Option<String> {
    let confirmed: &[(String, NameTarget)] = &resolution.confirmed;
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
mod test_support;
#[cfg(test)]
mod tests;
