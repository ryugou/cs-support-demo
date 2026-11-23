mod conversation;
pub(crate) mod fake_vegapunk;
mod fixture;
pub(crate) mod llm_stub;
mod name_confirmation;
mod reply_harness;
mod temp_dir;
mod tests;

pub(crate) use conversation::Turn;
pub(crate) use fixture::Fixture;
pub(crate) use reply_harness::{LlmBackend, ReplyHarness};

pub(crate) const PROJECT_ID: &str = "urtect";
