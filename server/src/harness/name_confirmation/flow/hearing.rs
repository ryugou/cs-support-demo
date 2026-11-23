use std::borrow::Cow;

use super::NameConfirmation;
use crate::harness::name_confirmation::catalog::NameKind;
use crate::harness::name_confirmation::interpret::ReplyResolution;
use crate::harness::name_confirmation::matcher::{find_mentions, MentionMatch};
use crate::harness::product_gate::ProductAllowlist;

const CONFIRMED_PRODUCT_FACT: &str = "- 確認済みの製品: ";
const CONFIRMED_MODEL_OPEN: &str = "（型番 ";
const CONFIRMED_MODEL_CLOSE: &str = "）";
const CONFIRMED_COMPANY_FACT: &str = "- 確認済みの社名: ";
const UNMATCHED_NOTATION_FACT: &str = "- 顧客がお知らせくださった表記: ";
const UNMATCHED_NOTATION_NOTE: &str =
    "（当社の登録名称と一致しません。確認した候補とは違うとのことです）";
const DENIED_NAMES_FACT: &str = "- 顧客が違うと答えた候補（返信に書かない）: ";
const NAME_SEPARATOR: &str = "、";

impl NameConfirmation {
    pub(crate) fn with_name_facts(
        &self,
        known_facts: String,
        answered: Option<&ReplyResolution>,
    ) -> String {
        let mut lines: Vec<String> = Vec::new();
        if !known_facts.is_empty() {
            lines.push(known_facts);
        }
        for confirmed in self.state.confirmed() {
            let target = &confirmed.target;
            let line = match target.kind {
                NameKind::Product if target.display == target.id => {
                    format!("{CONFIRMED_PRODUCT_FACT}{}", target.id)
                }
                NameKind::Product => format!(
                    "{CONFIRMED_PRODUCT_FACT}{}{CONFIRMED_MODEL_OPEN}{}{CONFIRMED_MODEL_CLOSE}",
                    target.display, target.id
                ),
                NameKind::Company => format!("{CONFIRMED_COMPANY_FACT}{}", target.display),
            };
            if !lines.contains(&line) {
                lines.push(line);
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
