use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::catalog::{NameKind, NameTarget};
use crate::harness::product_gate::is_safe_to_reflect;

pub(crate) const STATE_ATTRIBUTE: &str = "name_confirmation";
pub(crate) const MAX_ASKS: u8 = 2;
pub(crate) const MAX_PENDING_MENTIONS: usize = 3;
const MAX_SUGGESTIONS: usize = 5;
const MAX_RECORDS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AskReason {
    First,
    Choose,
    AlreadyTold,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingMention {
    pub(crate) surface: String,
    pub(crate) surface_key: String,
    pub(crate) suggestions: Vec<NameTarget>,
    pub(crate) asks: u8,
    pub(crate) reason: AskReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfirmedName {
    pub(crate) surface_key: String,
    pub(crate) target: NameTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeniedName {
    pub(crate) kind: NameKind,
    pub(crate) id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfirmationState {
    pending: Vec<PendingMention>,
    confirmed: Vec<ConfirmedName>,
    denied: Vec<DeniedName>,
    dismissed: Vec<String>,
}

impl ConfirmationState {
    pub(crate) fn from_case_attributes(attributes: &HashMap<String, String>) -> Result<Self> {
        let Some(written) = attributes.get(STATE_ATTRIBUTE) else {
            return Ok(Self::default());
        };
        let state: Self =
            serde_json::from_str(written).context("invalid name confirmation state")?;
        state.validated()
    }

    pub(crate) fn to_attribute(&self) -> Result<String> {
        serde_json::to_string(self).context("failed to encode name confirmation state")
    }

    pub(crate) fn pending(&self) -> &[PendingMention] {
        &self.pending
    }

    pub(crate) fn confirmed(&self) -> &[ConfirmedName] {
        &self.confirmed
    }

    pub(crate) fn take_pending(&mut self) -> Vec<PendingMention> {
        std::mem::take(&mut self.pending)
    }

    pub(crate) fn push_pending(&mut self, mention: PendingMention) {
        if self.pending.len() < MAX_PENDING_MENTIONS
            && !self
                .pending
                .iter()
                .any(|pending| pending.surface_key == mention.surface_key)
        {
            self.pending.push(mention);
        }
    }

    pub(crate) fn replace_pending(&mut self, pending: Vec<PendingMention>) {
        self.pending = pending;
    }

    pub(crate) fn confirm(&mut self, surface_key: &str, target: NameTarget) {
        self.confirmed
            .retain(|confirmed| confirmed.surface_key != surface_key);
        self.confirmed.push(ConfirmedName {
            surface_key: surface_key.to_string(),
            target,
        });
        trim_oldest(&mut self.confirmed);
    }

    pub(crate) fn deny(&mut self, targets: &[NameTarget]) {
        for target in targets {
            if !self.is_denied(target) {
                self.denied.push(DeniedName {
                    kind: target.kind,
                    id: target.id.clone(),
                });
            }
        }
        trim_oldest(&mut self.denied);
    }

    pub(crate) fn dismiss(&mut self, surface_key: &str) {
        if !self
            .dismissed
            .iter()
            .any(|dismissed| dismissed == surface_key)
        {
            self.dismissed.push(surface_key.to_string());
        }
        trim_oldest(&mut self.dismissed);
    }

    pub(crate) fn is_denied(&self, target: &NameTarget) -> bool {
        self.denied
            .iter()
            .any(|denied| denied.kind == target.kind && denied.id == target.id)
    }

    pub(crate) fn is_confirmed(&self, surface_key: &str) -> bool {
        self.confirmed
            .iter()
            .any(|confirmed| confirmed.surface_key == surface_key)
    }

    pub(crate) fn is_settled(&self, surface_key: &str) -> bool {
        self.is_confirmed(surface_key)
            || self
                .dismissed
                .iter()
                .any(|dismissed| dismissed == surface_key)
    }

    pub(crate) fn denied_ids(&self, kind: NameKind) -> Vec<String> {
        self.denied
            .iter()
            .filter(|denied| denied.kind == kind)
            .map(|denied| denied.id.clone())
            .collect()
    }

    pub(crate) fn confirmed_product_model(&self) -> Option<&str> {
        self.confirmed
            .iter()
            .rev()
            .find(|confirmed| confirmed.target.kind == NameKind::Product)
            .map(|confirmed| confirmed.target.id.as_str())
    }

    fn validated(self) -> Result<Self> {
        if self.pending.len() > MAX_PENDING_MENTIONS
            || self.confirmed.len() > MAX_RECORDS
            || self.denied.len() > MAX_RECORDS
            || self.dismissed.len() > MAX_RECORDS
        {
            bail!("invalid name confirmation state: too many entries");
        }
        let pending_valid = self.pending.iter().all(|pending| {
            !pending.surface.is_empty()
                && is_safe_to_reflect(&pending.surface)
                && !pending.surface_key.is_empty()
                && (1..=MAX_SUGGESTIONS).contains(&pending.suggestions.len())
                && pending.suggestions.iter().all(is_complete_target)
                && pending.asks <= MAX_ASKS
        });
        let records_valid = self.confirmed.iter().all(|confirmed| {
            !confirmed.surface_key.is_empty() && is_complete_target(&confirmed.target)
        }) && self.denied.iter().all(|denied| !denied.id.is_empty())
            && self.dismissed.iter().all(|dismissed| !dismissed.is_empty());
        if !pending_valid || !records_valid {
            bail!("invalid name confirmation state: an entry is incomplete");
        }
        Ok(self)
    }
}

pub(crate) fn capped_suggestions(mut suggestions: Vec<NameTarget>) -> Vec<NameTarget> {
    suggestions.truncate(MAX_SUGGESTIONS);
    suggestions
}

fn is_complete_target(target: &NameTarget) -> bool {
    !target.id.is_empty() && !target.display.is_empty()
}

fn trim_oldest<T>(records: &mut Vec<T>) {
    let excess = records.len().saturating_sub(MAX_RECORDS);
    records.drain(..excess);
}

#[cfg(test)]
mod tests;
