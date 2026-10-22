use anyhow::Result;

use super::{
    decide_signals_for_policy, decision, extraction, kr_applicability, rules, signal,
    DecisionBasis, Harness, ReplyDraftPolicy, RequestContext,
};
use crate::mcp::ToolService;
use crate::model::SectionHit;

pub(super) const KR_CANDIDATE_LIMIT: usize = 3;

pub(super) struct KrTurn<'a> {
    pub(super) resolutions: Vec<rules::KnownResolution>,
    pub(super) decide_signals: &'a signal::SignalSet,
    pub(super) decision_basis: DecisionBasis,
    pub(super) stakes_input: decision::StakesInput,
    pub(super) applicable_known_resolution: Option<rules::KnownResolution>,
}

impl Harness {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn search_manual_and_prepare_kr_turn<'a>(
        &self,
        ctx: &RequestContext,
        context_text: &str,
        accumulated: &'a signal::SignalSet,
        product_key: Option<&str>,
        manual_corpus: Option<&crate::proto::graphrag::GetGraphSnapshotResponse>,
        live_snapshot: &crate::proto::graphrag::GetGraphSnapshotResponse,
        tools: &ToolService,
        resolutions: Vec<rules::KnownResolution>,
        policy: &ReplyDraftPolicy<'_>,
        signals: &'a signal::SignalSet,
        domains: &[rules::ProhibitedDomain],
        question: &str,
        extraction_mode: extraction::ExtractionMode,
    ) -> Result<((Vec<SectionHit>, Vec<String>), KrTurn<'a>)> {
        let manual_search = self.manual_search_hits(
            ctx,
            context_text,
            accumulated,
            product_key,
            manual_corpus,
            live_snapshot,
            tools,
        );
        let kr_turn = self.prepare_kr_turn(
            ctx,
            resolutions,
            policy,
            signals,
            accumulated,
            domains,
            question,
            context_text,
            extraction_mode,
        );
        let (manual_search, kr_turn) = tokio::join!(manual_search, kr_turn);
        Ok((manual_search?, kr_turn))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn prepare_kr_turn<'a>(
        &self,
        ctx: &RequestContext,
        resolutions: Vec<rules::KnownResolution>,
        policy: &ReplyDraftPolicy<'_>,
        signals: &'a signal::SignalSet,
        accumulated: &'a signal::SignalSet,
        domains: &[rules::ProhibitedDomain],
        question: &str,
        context_text: &str,
        extraction_mode: extraction::ExtractionMode,
    ) -> KrTurn<'a> {
        self.warn_unmatched_excluded_ids(&ctx.request_id, &resolutions);
        let resolutions =
            rules::exclude_known_resolutions(resolutions, &self.excluded_known_resolution_ids);
        let stakes_input = self.stakes_input_for_turn(domains, accumulated, question);
        let (decide_signals, decision_basis) =
            decide_signals_for_policy(policy, signals, accumulated);
        let applicable_known_resolution = self
            .select_known_resolution(
                ctx,
                &resolutions,
                decide_signals,
                stakes_input.hazard_signal_count,
                extraction_mode,
                context_text,
            )
            .await;
        KrTurn {
            resolutions,
            decide_signals,
            decision_basis,
            stakes_input,
            applicable_known_resolution,
        }
    }

    /// Warns once per process per configured `excluded_known_resolution_ids` entry that matches
    /// no loaded known resolution. Shared by evaluate and MCP `search_known_resolutions` so the
    /// text and the dedupe are single-sourced. Carries no customer text.
    pub(crate) fn warn_unmatched_excluded_ids(
        &self,
        request_id: &str,
        resolutions: &[rules::KnownResolution],
    ) {
        for kr_id in self.newly_unmatched_excluded_ids(resolutions) {
            tracing::warn!(
                kr_id = %kr_id,
                request_id = %request_id,
                "configured excluded_known_resolution_ids entry matches no known resolution; check for a typo, or the known resolution may have been skipped as invalid at load (see the load warn), or the load was truncated at the {}-node query limit (see the load warn)",
                super::knowledge::KNOWN_RESOLUTION_QUERY_LIMIT
            );
        }
    }

    fn newly_unmatched_excluded_ids(&self, resolutions: &[rules::KnownResolution]) -> Vec<String> {
        let mut warned = self
            .warned_unmatched_excluded_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rules::unmatched_excluded_known_resolution_ids(
            resolutions,
            &self.excluded_known_resolution_ids,
        )
        .into_iter()
        .filter(|id| warned.insert(id.clone()))
        .collect()
    }

    async fn select_known_resolution(
        &self,
        ctx: &RequestContext,
        resolutions: &[rules::KnownResolution],
        decide_signals: &signal::SignalSet,
        hazard_signal_count: usize,
        extraction_mode: extraction::ExtractionMode,
        context_text: &str,
    ) -> Option<rules::KnownResolution> {
        if hazard_signal_count > 0 {
            return None;
        }
        let candidates =
            rules::known_resolution_candidates(resolutions, decide_signals, KR_CANDIDATE_LIMIT);
        if candidates.is_empty() {
            return None;
        }
        let judge = self.kr_applicability.as_deref()?;
        if extraction_mode == extraction::ExtractionMode::LexiconFallback {
            tracing::warn!(
                request_id = %ctx.request_id,
                candidates = candidates.len(),
                "known-resolution applicability check skipped because signal extraction fell back to the lexicon; no known resolution is applied this turn. Check the LLM connection"
            );
            return None;
        }
        match kr_applicability::select_applicable(judge, &self.lexicon, context_text, &candidates)
            .await
        {
            Ok(selected) => selected.cloned(),
            Err(err) => {
                tracing::warn!(
                    request_id = %ctx.request_id,
                    error = %format!("{err:#}"),
                    "known-resolution applicability check failed; no known resolution is applied this turn. Check the LLM connection and the kr_applicability output format"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
