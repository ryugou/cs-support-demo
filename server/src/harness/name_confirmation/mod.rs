mod catalog;
/// Registers company names in the project schema and reads the active ones back.
pub mod company;
mod confirmation_text;
mod flow;
mod interpret;
mod matcher;
mod screening;
mod state;
#[cfg(test)]
pub(crate) mod test_fixtures;

pub(crate) use flow::{
    identified_product_models, rewritten_context, NameConfirmation, Presentation,
};
pub(crate) use interpret::ReplyResolution;
pub(crate) use screening::screen_product_references;

/// Selects how `Harness::evaluate` identifies the catalog products that a customer message names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductNameMatching {
    /// Identifies products from the LLM product references only.
    LlmReferencesOnly,
    /// Also identifies registered model numbers, product names, and aliases written in the
    /// message, and identifies no product from a notation that is one typo away from a registered
    /// name.
    RegisteredNames,
}
