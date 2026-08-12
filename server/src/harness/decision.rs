use crate::harness::rules::{
    match_known_resolution, match_layer1, match_layer2, EscalationRule, KnownResolution, KrMatch,
    ProhibitedDomain,
};
use crate::harness::signal::SignalSet;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stakes {
    Low,
    Mid,
    High,
}

/// classify_stakes の入力。3 フラグの算出は Harness（呼び出し側）が決定論に行う。
#[derive(Debug, Clone)]
pub struct StakesInput {
    /// binding=mandatory の禁止領域に近接（signal 交差 or パターン部分ヒット）
    pub mandatory_domain_near: bool,
    /// 決定論 NG 辞書（block/abstain 語）への近接ヒット
    pub ng_near_hit: bool,
    /// lexicon class=hazard の signal 数
    pub hazard_signal_count: usize,
}

/// S1-6: stakes 3 段離散。将来 Π 連続変調に差し替わる（呼び出し側は不変）。
pub fn classify_stakes(input: &StakesInput) -> Stakes {
    if input.mandatory_domain_near || input.ng_near_hit {
        Stakes::High
    } else if input.hazard_signal_count > 0 {
        Stakes::Mid
    } else {
        Stakes::Low
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Thresholds {
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

impl From<&crate::config::ThresholdsConfig> for Thresholds {
    fn from(config: &crate::config::ThresholdsConfig) -> Self {
        Self {
            low: config.low,
            mid: config.mid,
            high: config.high,
        }
    }
}

/// S1-6: 3 段の階段関数。第3層の可否比較は「threshold を受け取って比較」のみ。
pub fn answerability_threshold(thresholds: &Thresholds, stakes: Stakes) -> f32 {
    match stakes {
        Stakes::Low => thresholds.low,
        Stakes::Mid => thresholds.mid,
        Stakes::High => thresholds.high,
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EvidenceRequirement {
    DirectManualCoverage { required: f32, best: f32 },
}

#[derive(Debug)]
pub enum Sufficiency {
    Sufficient,
    Insufficient { missing: Vec<EvidenceRequirement> },
}

/// 数でなく直接性で測る。何が足りないか（missing）を返す純関数（spec「evidence_sufficient の定義」）。
pub fn evidence_sufficient(threshold: f32, best_hit_score: Option<f32>) -> Sufficiency {
    let best = best_hit_score.unwrap_or(0.0);
    if best >= threshold {
        Sufficiency::Sufficient
    } else {
        Sufficiency::Insufficient {
            missing: vec![EvidenceRequirement::DirectManualCoverage {
                required: threshold,
                best,
            }],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnswerSource {
    KnownResolution,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EscalateReason {
    PermissionDenied,
    RegulatedOrSafety,
    RequiresHumanApproval,
    InsufficientDirectness,
    UnknownAddedSignal,
}

/// 顧客に開示してよい情報の範囲。文面そのものは client（生成側）が作る（spec「message_policy の扱い」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DisclosureScope {
    /// 「担当部署に確認する」旨のみ開示可
    ConfirmingWithTeam,
    /// 内部事情（権限・根拠不足の詳細）を開示しない
    NoInternalDetails,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum AnswerDecision {
    Allowed {
        source: AnswerSource,
        evidence_section_keys: Vec<String>,
        known_resolution_id: Option<String>,
        stakes: Stakes,
        threshold: f32,
    },
    Escalate {
        reason: EscalateReason,
        layer: u8,
        route_to: String,
        disclosure_scope: DisclosureScope,
        audit_required: bool,
        missing: Vec<EvidenceRequirement>,
    },
}

pub struct DecisionInput<'a> {
    pub question_signals: &'a SignalSet,
    pub question_raw: &'a str,
    pub rules: &'a [EscalationRule],
    pub domains: &'a [ProhibitedDomain],
    pub resolutions: &'a [KnownResolution],
    pub best_manual_score: Option<f32>,
    pub best_manual_sections: &'a [String],
    pub stakes_input: StakesInput,
    pub thresholds: &'a Thresholds,
    /// config 由来の既定エスカレーション route（第3層エスカレーション先）。
    pub default_route: &'a str,
}

/// (B) 3 層判定の decision function。LLM 非介在・同じ入力なら必ず同じ判定（純関数）。
/// 先に止まった層で確定し、後段は評価しない。第1・2層にメモ化を適用しない。
pub fn decide(input: &DecisionInput) -> AnswerDecision {
    // 第1層: 明示エスカレーションルール
    if let Some(rule) = match_layer1(input.rules, input.question_signals) {
        return AnswerDecision::Escalate {
            reason: EscalateReason::RegulatedOrSafety,
            layer: 1,
            route_to: rule.route.clone(),
            disclosure_scope: DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
        };
    }
    // 第2層: 禁止領域
    if let Some(domain) = match_layer2(input.domains, input.question_signals, input.question_raw) {
        return AnswerDecision::Escalate {
            reason: EscalateReason::RegulatedOrSafety,
            layer: 2,
            route_to: domain.route.clone(),
            disclosure_scope: DisclosureScope::ConfirmingWithTeam,
            audit_required: true,
            missing: Vec::new(),
        };
    }
    // 第3層: 回答可能性
    let stakes = classify_stakes(&input.stakes_input);
    let threshold = answerability_threshold(input.thresholds, stakes);
    let kr_match = match_known_resolution(input.resolutions, input.question_signals);
    if let KrMatch::Applicable(kr) = kr_match {
        return AnswerDecision::Allowed {
            source: AnswerSource::KnownResolution,
            evidence_section_keys: Vec::new(),
            known_resolution_id: Some(kr.id.clone()),
            stakes,
            threshold,
        };
    }
    match evidence_sufficient(threshold, input.best_manual_score) {
        Sufficiency::Sufficient => AnswerDecision::Allowed {
            source: AnswerSource::Manual,
            evidence_section_keys: input.best_manual_sections.to_vec(),
            known_resolution_id: None,
            stakes,
            threshold,
        },
        Sufficiency::Insufficient { missing } => {
            let reason = if matches!(kr_match, KrMatch::BlockedByAddedSignal { .. }) {
                EscalateReason::UnknownAddedSignal
            } else {
                EscalateReason::InsufficientDirectness
            };
            AnswerDecision::Escalate {
                reason,
                layer: 3,
                route_to: input.default_route.to_string(),
                disclosure_scope: DisclosureScope::ConfirmingWithTeam,
                audit_required: true,
                missing,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::rules::{
        Binding, EscalationRule, Grade, KnownResolution, ProhibitedDomain, RootCause,
        SourceAuthority,
    };
    use crate::harness::signal::{Signal, SignalSet};

    fn signals(values: &[&str]) -> SignalSet {
        values.iter().map(|v| Signal::new(*v)).collect()
    }

    fn thresholds() -> Thresholds {
        Thresholds {
            low: 0.6,
            mid: 0.8,
            high: 0.95,
        }
    }

    fn kr(id: &str, set: &[&str]) -> KnownResolution {
        KnownResolution {
            id: id.to_string(),
            signal_set: signals(set),
            applicability: "全ロット".to_string(),
            answer: "answer".to_string(),
            source_authority: SourceAuthority::Authoritative,
            root_cause: RootCause::KnowledgeError,
            grade: Grade::ApprovalRequired,
            approval_count: 0,
            rejection_count: 0,
            approver_set: Vec::new(),
            origin: "test".to_string(),
            binding: Binding::Advisory,
            registration_trigger: "single_ruling".to_string(),
            knowledge_class: "commercial".to_string(),
            outcome_ref: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn input<'a>(
        question_signals: &'a SignalSet,
        rules: &'a [EscalationRule],
        domains: &'a [ProhibitedDomain],
        resolutions: &'a [KnownResolution],
        best_manual_score: Option<f32>,
        stakes_input: StakesInput,
        thresholds: &'a Thresholds,
        sections: &'a [String],
    ) -> DecisionInput<'a> {
        DecisionInput {
            question_signals,
            question_raw: "質問",
            rules,
            domains,
            resolutions,
            best_manual_score,
            best_manual_sections: sections,
            stakes_input,
            thresholds,
            default_route: "triage",
        }
    }

    fn calm() -> StakesInput {
        StakesInput {
            mandatory_domain_near: false,
            ng_near_hit: false,
            hazard_signal_count: 0,
        }
    }

    // --- stakes / threshold ---

    #[test]
    fn stakes_ladder() {
        assert_eq!(
            classify_stakes(&StakesInput {
                mandatory_domain_near: true,
                ng_near_hit: false,
                hazard_signal_count: 0
            }),
            Stakes::High
        );
        assert_eq!(
            classify_stakes(&StakesInput {
                mandatory_domain_near: false,
                ng_near_hit: true,
                hazard_signal_count: 0
            }),
            Stakes::High
        );
        assert_eq!(
            classify_stakes(&StakesInput {
                mandatory_domain_near: false,
                ng_near_hit: false,
                hazard_signal_count: 1
            }),
            Stakes::Mid
        );
        assert_eq!(classify_stakes(&calm()), Stakes::Low);
    }

    #[test]
    fn threshold_is_monotonic_staircase() {
        let t = thresholds();
        assert!(
            answerability_threshold(&t, Stakes::Low) < answerability_threshold(&t, Stakes::Mid)
        );
        assert!(
            answerability_threshold(&t, Stakes::Mid) < answerability_threshold(&t, Stakes::High)
        );
    }

    // --- evidence_sufficient（数でなく直接性・missing を返す純関数）---

    #[test]
    fn evidence_sufficient_returns_missing_details() {
        match evidence_sufficient(0.8, Some(0.5)) {
            Sufficiency::Insufficient { missing } => {
                assert_eq!(missing.len(), 1);
                let EvidenceRequirement::DirectManualCoverage { required, best } = &missing[0];
                assert_eq!(*required, 0.8);
                assert_eq!(*best, 0.5);
            }
            Sufficiency::Sufficient => panic!("expected insufficient"),
        }
        assert!(matches!(
            evidence_sufficient(0.8, Some(0.9)),
            Sufficiency::Sufficient
        ));
        assert!(matches!(
            evidence_sufficient(0.8, None),
            Sufficiency::Insufficient { .. }
        ));
    }

    // --- decide: 3 層短絡 ---

    #[test]
    fn layer1_short_circuits_everything() {
        // 第1層マッチ時は KR が完全一致でも回答に進まない（バイパス不可）
        let rules = vec![EscalationRule {
            id: "r1".to_string(),
            condition: signals(&["post_ingestion_symptom"]),
            route: "safety_team".to_string(),
            owner: None,
            binding: Binding::Mandatory,
        }];
        let resolutions = vec![kr("kr1", &["post_ingestion_symptom"])];
        let q = signals(&["post_ingestion_symptom"]);
        let d = decide(&input(
            &q,
            &rules,
            &[],
            &resolutions,
            Some(1.0),
            calm(),
            &thresholds(),
            &[],
        ));
        match d {
            AnswerDecision::Escalate {
                layer,
                route_to,
                reason,
                audit_required,
                ..
            } => {
                assert_eq!(layer, 1);
                assert_eq!(route_to, "safety_team");
                assert_eq!(reason, EscalateReason::RegulatedOrSafety);
                assert!(audit_required);
            }
            other => panic!("expected layer1 escalate, got {other:?}"),
        }
    }

    #[test]
    fn layer2_blocks_before_layer3() {
        let domains = vec![ProhibitedDomain {
            id: "d1".to_string(),
            domain_signals: signals(&["skin_irritation"]),
            text_patterns: Vec::new(),
            route: "derm_liaison".to_string(),
            binding: Binding::Mandatory,
        }];
        let resolutions = vec![kr("kr1", &["skin_irritation"])];
        let q = signals(&["skin_irritation"]);
        let d = decide(&input(
            &q,
            &[],
            &domains,
            &resolutions,
            Some(1.0),
            calm(),
            &thresholds(),
            &[],
        ));
        match d {
            AnswerDecision::Escalate {
                layer, route_to, ..
            } => {
                assert_eq!(layer, 2);
                assert_eq!(route_to, "derm_liaison");
            }
            other => panic!("expected layer2 escalate, got {other:?}"),
        }
    }

    #[test]
    fn layer3_reuses_known_resolution() {
        let resolutions = vec![kr("kr1", &["discoloration"])];
        let q = signals(&["discoloration"]);
        let d = decide(&input(
            &q,
            &[],
            &[],
            &resolutions,
            None,
            calm(),
            &thresholds(),
            &[],
        ));
        match d {
            AnswerDecision::Allowed {
                source,
                known_resolution_id,
                ..
            } => {
                assert_eq!(source, AnswerSource::KnownResolution);
                assert_eq!(known_resolution_id.as_deref(), Some("kr1"));
            }
            other => panic!("expected allowed via KR, got {other:?}"),
        }
    }

    #[test]
    fn layer3_added_signal_escalates_with_unknown_added_signal() {
        let resolutions = vec![kr("kr1", &["discoloration"])];
        let q = signals(&["discoloration", "mold"]);
        let d = decide(&input(
            &q,
            &[],
            &[],
            &resolutions,
            Some(0.1),
            calm(),
            &thresholds(),
            &[],
        ));
        match d {
            AnswerDecision::Escalate {
                layer,
                reason,
                route_to,
                ..
            } => {
                assert_eq!(layer, 3);
                assert_eq!(reason, EscalateReason::UnknownAddedSignal);
                assert_eq!(route_to, "triage");
            }
            other => panic!("expected UnknownAddedSignal escalate, got {other:?}"),
        }
    }

    #[test]
    fn layer3_direct_manual_answers() {
        let q = SignalSet::new();
        let sections = vec!["doc-1#storage".to_string()];
        let d = decide(&input(
            &q,
            &[],
            &[],
            &[],
            Some(0.95),
            calm(),
            &thresholds(),
            &sections,
        ));
        match d {
            AnswerDecision::Allowed {
                source,
                evidence_section_keys,
                ..
            } => {
                assert_eq!(source, AnswerSource::Manual);
                assert_eq!(evidence_section_keys, sections);
            }
            other => panic!("expected allowed via manual, got {other:?}"),
        }
    }

    #[test]
    fn high_stakes_raises_threshold_and_escalates() {
        // S1-8 Done 条件 5: 第2層列挙に無くても stakes=high でしきい値が上がり escalate に倒れる
        let q = SignalSet::new();
        let high = StakesInput {
            mandatory_domain_near: false,
            ng_near_hit: true,
            hazard_signal_count: 0,
        };
        let d = decide(&input(
            &q,
            &[],
            &[],
            &[],
            Some(0.9),
            high,
            &thresholds(),
            &[],
        ));
        match d {
            AnswerDecision::Escalate {
                layer,
                reason,
                missing,
                ..
            } => {
                assert_eq!(layer, 3);
                assert_eq!(reason, EscalateReason::InsufficientDirectness);
                assert!(!missing.is_empty());
            }
            other => panic!("expected high-stakes escalate, got {other:?}"),
        }
        // 同じ根拠でも low stakes なら答えられる（実用性のダイヤル）
        let d2 = decide(&input(
            &q,
            &[],
            &[],
            &[],
            Some(0.9),
            calm(),
            &thresholds(),
            &[],
        ));
        assert!(matches!(d2, AnswerDecision::Allowed { .. }));
    }

    // --- 実データ回帰: human_handoff_request のサイレント never-match 検知（PR #17 review W3）---
    //
    // "human_handoff_request" という signal 名は
    // data/urtect/signal-lexicon.json / data/signal-lexicon.json / data/urtect/rules.json の
    // 3 箇所に手書きで重複している。どこか 1 箇所で typo が起きると
    // `rule.condition.is_subset(question)` が永久に false になり、取次依頼が第1層で
    // エスカレーションされず素通りする（黙って第3層評価に流れる）。ここでは実際に配布される
    // JSON ファイルをロードし、lexicon → rules → decide() の一気通貫でこの不整合を検知する。

    fn bundled_lexicon_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/urtect/signal-lexicon.json")
    }

    fn bundled_rules_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/urtect/rules.json")
    }

    #[test]
    fn bundled_lexicon_extracts_human_handoff_request_for_known_surface_forms() {
        use crate::harness::signal::{LexiconNormalizer, SignalNormalizer};
        let lex = LexiconNormalizer::from_path(&bundled_lexicon_path())
            .expect("bundled urtect signal-lexicon.json loads");
        for utterance in [
            "担当者につないでください",
            "担当者に繋いでください",           // W2: 漢字表記「繋いで」
            "オペレーターに繋いでもらえますか", // W2: 漢字表記「繋いで」
            "人に代わってください",             // W2: 「人に代わって」
            "サポートにつないでほしい",         // W2: 「サポートにつないで」
        ] {
            assert!(
                lex.normalize(utterance)
                    .contains(&Signal::new("human_handoff_request")),
                "expected human_handoff_request signal for utterance: {utterance}"
            );
        }
    }

    // --- 実データ回帰: human_handoff_request の代理問い合わせ誤爆検知（PR #17 review Critical）---
    //
    // surface_forms は resolve::normalize_key 後の単純部分一致で照合される。かつて
    // 「人に代わって」を surface_form に持っていたため、「本人に代わって」のような代理問い合わせの
    // 発話にも部分一致してしまい、human_handoff_request（rules.json の human-handoff、第1層
    // mandatory エスカレーション）が誤って立っていた。human_handoff_request は第1層に落ちると
    // clarification_allowed() が false になり、聞き返しも回答も一切行わずエスカレーションで
    // 確定するため、正当な代理問い合わせが全件人手に回る Critical だった。ここでは実際に配布される
    // JSON をロードし、非マッチ（代理問い合わせ）とマッチ（依頼形）の両方を実データで固定する。
    #[test]
    fn bundled_lexicon_does_not_flag_human_handoff_for_proxy_inquiry_phrases() {
        use crate::harness::signal::{LexiconNormalizer, SignalNormalizer};
        let lex = LexiconNormalizer::from_path(&bundled_lexicon_path())
            .expect("bundled urtect signal-lexicon.json loads");

        for utterance in [
            "本人に代わって問い合わせています。カメラがオフラインです",
            "代理人に代わって連絡しています",
            "設置は業者の人に代わってやってもらいました",
            "母に代わって問い合わせています",
        ] {
            assert!(
                !lex.normalize(utterance)
                    .contains(&Signal::new("human_handoff_request")),
                "expected human_handoff_request to NOT fire for proxy inquiry utterance: {utterance}"
            );
        }

        // W2 の後退防止: 依頼形（人に代わって + ください/ほしい 等）は引き続き立つこと
        for utterance in ["人に代わってください", "人に代わってほしいです"] {
            assert!(
                lex.normalize(utterance)
                    .contains(&Signal::new("human_handoff_request")),
                "expected human_handoff_request signal for utterance: {utterance}"
            );
        }
    }

    /// ingest_rules.rs の RuleInput と同形（rule_id/condition/owner/route/binding）の
    /// テスト専用パース struct。CLI 側の構造体をテストから直接 import できないため複製する。
    #[derive(Debug, serde::Deserialize)]
    struct BundledRuleInput {
        rule_id: String,
        condition: Vec<String>,
        owner: Option<String>,
        route: String,
        binding: String,
    }

    #[derive(Debug, serde::Deserialize)]
    struct BundledRulesFile {
        escalation_rules: Vec<BundledRuleInput>,
    }

    fn load_bundled_escalation_rules() -> Vec<EscalationRule> {
        let body =
            std::fs::read_to_string(bundled_rules_path()).expect("read bundled urtect rules.json");
        let parsed: BundledRulesFile =
            serde_json::from_str(&body).expect("parse bundled urtect rules.json");
        parsed
            .escalation_rules
            .into_iter()
            .map(|r| EscalationRule {
                id: r.rule_id,
                condition: r.condition.into_iter().map(Signal::new).collect(),
                route: r.route,
                owner: r.owner,
                binding: match r.binding.as_str() {
                    "mandatory" => Binding::Mandatory,
                    "advisory" => Binding::Advisory,
                    other => panic!("unknown binding in bundled urtect rules.json: {other}"),
                },
            })
            .collect()
    }

    #[test]
    fn bundled_rules_contain_human_handoff_rule_and_layer1_matches_it() {
        let rules = load_bundled_escalation_rules();
        let expected = signals(&["human_handoff_request"]);
        let rule = rules.iter().find(|r| r.condition == expected).expect(
            "data/urtect/rules.json must define an escalation rule \
                 with condition == [\"human_handoff_request\"]",
        );
        let matched = match_layer1(&rules, &expected);
        assert_eq!(matched.map(|r| r.id.as_str()), Some(rule.id.as_str()));
    }

    #[test]
    fn bundled_human_handoff_rule_escalates_at_layer1_even_with_high_manual_score() {
        // 第1層が第3層より優先されることを実データで確認する（layer 短絡契約の回帰防止）。
        // best_manual_score=0.99 は単独なら Allowed になる水準だが、human_handoff_request は
        // 必ず第1層で捕捉され、回答生成（第3層）へ到達してはならない。
        let rules = load_bundled_escalation_rules();
        let question = signals(&["human_handoff_request"]);
        let d = decide(&input(
            &question,
            &rules,
            &[],
            &[],
            Some(0.99),
            calm(),
            &thresholds(),
            &[],
        ));
        match d {
            AnswerDecision::Escalate {
                layer, route_to, ..
            } => {
                assert_eq!(layer, 1);
                assert_eq!(route_to, "support_desk");
            }
            other => panic!("expected layer1 escalate for human_handoff_request, got {other:?}"),
        }
    }

    #[test]
    fn decide_is_deterministic() {
        let resolutions = vec![kr("kr1", &["discoloration"])];
        let q = signals(&["discoloration"]);
        let a = decide(&input(
            &q,
            &[],
            &[],
            &resolutions,
            Some(0.5),
            calm(),
            &thresholds(),
            &[],
        ));
        let b = decide(&input(
            &q,
            &[],
            &[],
            &resolutions,
            Some(0.5),
            calm(),
            &thresholds(),
            &[],
        ));
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }
}
