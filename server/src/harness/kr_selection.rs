use super::{
    decide_signals_for_policy, decision, extraction, kr_applicability, rules, signal,
    DecisionBasis, Harness, ReplyDraftPolicy, RequestContext,
};

const KR_CANDIDATE_LIMIT: usize = 3;

pub(super) struct KrTurn<'a> {
    pub(super) resolutions: Vec<rules::KnownResolution>,
    pub(super) decide_signals: &'a signal::SignalSet,
    pub(super) decision_basis: DecisionBasis,
    pub(super) stakes_input: decision::StakesInput,
    pub(super) applicable_known_resolution: Option<rules::KnownResolution>,
}

impl Harness {
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
