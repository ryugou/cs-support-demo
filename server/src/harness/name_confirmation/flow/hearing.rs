use std::borrow::Cow;
use std::collections::HashMap;

use super::{NameConfirmation, PRODUCT_MODELS_ATTRIBUTE};
use crate::harness::knowledge::csv_list;
use crate::harness::name_confirmation::catalog::{NameKind, NameTarget};
use crate::harness::name_confirmation::interpret::ReplyResolution;
use crate::harness::name_confirmation::matcher::{find_mentions, MentionMatch};
use crate::harness::product_gate::ProductAllowlist;
use crate::harness::reply::{customer_history_window, ReplyHistoryTurn};

const CONFIRMED_PRODUCT_FACT: &str = "- 確認済みの製品: ";
const CONFIRMED_MODEL_OPEN: &str = "（型番 ";
const CONFIRMED_MODEL_CLOSE: &str = "）";
const CONFIRMED_COMPANY_FACT: &str = "- 確認済みの社名: ";
const UNMATCHED_NOTATION_FACT: &str = "- 顧客がお知らせくださった名称（";
const UNMATCHED_NOTATION_NOTE: &str =
    "）は当社の登録名称と一致せず、確認した候補とも違うとのことです";
const DENIED_NAMES_FACT: &str = "- 顧客が違うと答えた候補（返信に書かない）: ";
const NAME_SEPARATOR: &str = "、";
const IDENTIFIED_PRODUCT_FACT: &str = "- 特定済みの製品: ";

impl NameConfirmation {
    pub(crate) fn with_name_facts(
        &self,
        known_facts: String,
        answered: Option<&ReplyResolution>,
        identified_models: &[String],
        allowlist: &ProductAllowlist,
    ) -> String {
        let mut lines: Vec<String> = Vec::new();
        if !known_facts.is_empty() {
            lines.push(known_facts);
        }
        for confirmed in self.state.confirmed() {
            let line = confirmed_fact(&confirmed.target);
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
        for (model, name) in allowlist.catalog() {
            let identified = identified_models.iter().any(|written| written == model);
            if let (true, Some(name), false) = (identified, name, self.confirmed_product(model)) {
                lines.push(format!(
                    "{IDENTIFIED_PRODUCT_FACT}{name}{CONFIRMED_MODEL_OPEN}{model}{CONFIRMED_MODEL_CLOSE}"
                ));
            }
        }
        if let Some(surface) =
            answered.and_then(|resolution| resolution.unresolved_surface.as_deref())
        {
            lines.push(format!(
                "{UNMATCHED_NOTATION_FACT}{surface}{UNMATCHED_NOTATION_NOTE}"
            ));
        }
        let denied: Vec<String> = [NameKind::Product, NameKind::Company]
            .into_iter()
            .flat_map(|kind| self.state.denied_ids(kind))
            .collect();
        if !denied.is_empty() {
            lines.push(format!(
                "{DENIED_NAMES_FACT}{}",
                denied.join(NAME_SEPARATOR)
            ));
        }
        lines.join("\n")
    }

    fn confirmed_product(&self, model: &str) -> bool {
        self.state.confirmed().iter().any(|confirmed| {
            confirmed.target.kind == NameKind::Product && confirmed.target.id == model
        })
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

    pub(crate) fn withhold_denied_names(
        &self,
        text: String,
        fallback: &str,
        request_id: &str,
        case_id: &str,
    ) -> String {
        if self.mentions_denied_name(&text) {
            tracing::warn!(
                request_id,
                case_id,
                "a hearing names a candidate the customer denied; using the fallback hearing text"
            );
            fallback.to_string()
        } else {
            text
        }
    }

    pub(crate) fn hearing_question<'a>(
        message: &'a str,
        history: &[ReplyHistoryTurn],
        answered: Option<&ReplyResolution>,
    ) -> Cow<'a, str> {
        let Some(surface) =
            answered.and_then(|resolution| resolution.unresolved_surface.as_deref())
        else {
            return Cow::Borrowed(message);
        };
        customer_history_window(history)
            .turns
            .into_iter()
            .rev()
            .find(|turn| turn.contains(surface))
            .map_or(Cow::Borrowed(message), |turn| {
                Cow::Owned(format!("{turn}\n{message}"))
            })
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
}

fn confirmed_fact(target: &NameTarget) -> String {
    match target.kind {
        NameKind::Product if target.display == target.id => {
            format!("{CONFIRMED_PRODUCT_FACT}{}", target.id)
        }
        NameKind::Product => format!(
            "{CONFIRMED_PRODUCT_FACT}{}{CONFIRMED_MODEL_OPEN}{}{CONFIRMED_MODEL_CLOSE}",
            target.display, target.id
        ),
        NameKind::Company => format!("{CONFIRMED_COMPANY_FACT}{}", target.display),
    }
}

pub(crate) fn identified_product_models(case_attributes: &HashMap<String, String>) -> Vec<String> {
    match case_attributes.get(PRODUCT_MODELS_ATTRIBUTE) {
        Some(written) => csv_list(written),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
