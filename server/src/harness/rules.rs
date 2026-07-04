use crate::harness::signal::SignalSet;
use crate::resolve::normalize_key;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    Mandatory,
    Advisory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    ApprovalRequired,
    AutoAnswerAudited,
    Demoted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAuthority {
    Authoritative,
    NonAuthoritative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootCause {
    KnowledgeError,
    RetrievalMiss,
}

/// 第1層: 明示エスカレーションルール（具体条件 → 固有ルーティング）。学習・メモ化しない。
#[derive(Debug, Clone)]
pub struct EscalationRule {
    pub id: String,
    pub condition: SignalSet,
    pub route: String,
    pub owner: Option<String>,
    pub binding: Binding,
}

/// 第2層: 禁止領域（面のブラックリスト）。学習で緩まない絶対線。
#[derive(Debug, Clone)]
pub struct ProhibitedDomain {
    pub id: String,
    pub domain_signals: SignalSet,
    pub text_patterns: Vec<String>,
    pub route: String,
    pub binding: Binding,
}

/// 第3層のルール（S1-3）。正本は PunkRecord の KnownResolution + Signal ノード。
#[derive(Debug, Clone)]
pub struct KnownResolution {
    pub id: String,
    pub signal_set: SignalSet,
    pub applicability: String,
    pub answer: String,
    pub source_authority: SourceAuthority,
    pub root_cause: RootCause,
    pub grade: Grade,
    pub approval_count: u32,
    pub rejection_count: u32,
    pub approver_set: Vec<String>,
    pub origin: String,
    // --- 予約フィールド（Step 1 は既定値のまま。S1-3）---
    pub binding: Binding,
    pub registration_trigger: String,
    pub knowledge_class: String,
    pub outcome_ref: Vec<String>,
}

impl KnownResolution {
    pub fn signal_specificity(&self) -> usize {
        self.signal_set.len()
    }
}

/// 第1層照合: rule.condition ⊆ question のとき確定ルーティング。空条件はマッチしない。
pub fn match_layer1<'a>(
    rules: &'a [EscalationRule],
    question: &SignalSet,
) -> Option<&'a EscalationRule> {
    rules
        .iter()
        .find(|rule| !rule.condition.is_empty() && rule.condition.is_subset(question))
}

/// 第2層照合: signal 一致 or raw text パターン一致で必ず止める（面で塞ぐ）。
pub fn match_layer2<'a>(
    domains: &'a [ProhibitedDomain],
    question: &SignalSet,
    raw_text: &str,
) -> Option<&'a ProhibitedDomain> {
    let raw_norm = normalize_key(raw_text);
    domains.iter().find(|domain| {
        domain.domain_signals.iter().any(|s| question.contains(s))
            || domain.text_patterns.iter().any(|pattern| {
                let p = normalize_key(pattern);
                !p.is_empty() && raw_norm.contains(&p)
            })
    })
}

#[derive(Debug)]
pub enum KrMatch<'a> {
    Applicable(&'a KnownResolution),
    /// 既存ルールの subset は一致したが、未知の追加 signal が残った（＝学習の入口）。
    BlockedByAddedSignal {
        leftover: SignalSet,
    },
    None,
}

/// 第3層(a) 照合（S1-3・Rust 決定論）。包含方向のみ・条件増加で再利用しない。
pub fn match_known_resolution<'a>(
    resolutions: &'a [KnownResolution],
    question: &SignalSet,
) -> KrMatch<'a> {
    let mut applicable: Vec<&KnownResolution> = Vec::new();
    let mut best_blocked: Option<SignalSet> = None;
    for kr in resolutions {
        if kr.signal_set.is_empty() || !kr.signal_set.is_subset(question) {
            continue;
        }
        let leftover: SignalSet = question.difference(&kr.signal_set).cloned().collect();
        if leftover.is_empty() {
            applicable.push(kr);
        } else {
            // より小さい leftover（より具体的な部分一致）を記録する
            let smaller = best_blocked
                .as_ref()
                .map_or(true, |current| leftover.len() < current.len());
            if smaller {
                best_blocked = Some(leftover);
            }
        }
    }
    applicable.sort_by(|a, b| b.signal_specificity().cmp(&a.signal_specificity()));
    if let Some(kr) = applicable.first() {
        return KrMatch::Applicable(kr);
    }
    if let Some(leftover) = best_blocked {
        return KrMatch::BlockedByAddedSignal { leftover };
    }
    KrMatch::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::signal::Signal;

    fn signals(values: &[&str]) -> SignalSet {
        values.iter().map(|v| Signal::new(*v)).collect()
    }

    fn kr(id: &str, set: &[&str], answer: &str) -> KnownResolution {
        KnownResolution {
            id: id.to_string(),
            signal_set: signals(set),
            applicability: "全ロット".to_string(),
            answer: answer.to_string(),
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

    // --- 第1層 ---

    #[test]
    fn layer1_matches_when_all_condition_signals_present() {
        let rules = vec![EscalationRule {
            id: "r1".to_string(),
            condition: signals(&["post_ingestion_symptom"]),
            route: "safety_team".to_string(),
            owner: None,
            binding: Binding::Mandatory,
        }];
        assert!(match_layer1(
            &rules,
            &signals(&["post_ingestion_symptom", "discoloration"])
        )
        .is_some());
        assert!(match_layer1(&rules, &signals(&["discoloration"])).is_none());
    }

    #[test]
    fn layer1_empty_condition_never_matches() {
        let rules = vec![EscalationRule {
            id: "r0".to_string(),
            condition: SignalSet::new(),
            route: "x".to_string(),
            owner: None,
            binding: Binding::Advisory,
        }];
        assert!(match_layer1(&rules, &signals(&["discoloration"])).is_none());
    }

    // --- 第2層 ---

    #[test]
    fn layer2_matches_by_domain_signal() {
        let domains = vec![ProhibitedDomain {
            id: "d1".to_string(),
            domain_signals: signals(&["skin_irritation"]),
            text_patterns: Vec::new(),
            route: "derm_liaison".to_string(),
            binding: Binding::Mandatory,
        }];
        assert!(
            match_layer2(&domains, &signals(&["skin_irritation"]), "肌がピリピリする").is_some()
        );
        assert!(match_layer2(&domains, &signals(&["expiry_question"]), "賞味期限は").is_none());
    }

    #[test]
    fn layer2_matches_by_raw_text_pattern_even_without_signal() {
        // lexicon 取りこぼし時のセーフティネット（S1-11）
        let domains = vec![ProhibitedDomain {
            id: "d2".to_string(),
            domain_signals: SignalSet::new(),
            text_patterns: vec!["飲み合わせ".to_string()],
            route: "pharmacist".to_string(),
            binding: Binding::Mandatory,
        }];
        assert!(match_layer2(&domains, &SignalSet::new(), "薬との飲み合わせは大丈夫？").is_some());
    }

    // --- 第3層(a) match_known_resolution ---

    #[test]
    fn kr_exact_match_applies() {
        let resolutions = vec![kr(
            "kr1",
            &["discoloration"],
            "自然変色なので問題ありません",
        )];
        match match_known_resolution(&resolutions, &signals(&["discoloration"])) {
            KrMatch::Applicable(found) => assert_eq!(found.id, "kr1"),
            other => panic!("expected Applicable, got {other:?}"),
        }
    }

    #[test]
    fn kr_added_signal_blocks_reuse() {
        // 大前提: 「変色 + カビ」は「変色」ルールの射程外。必ずエスカレーション（S1-3）。
        let resolutions = vec![kr(
            "kr1",
            &["discoloration"],
            "自然変色なので問題ありません",
        )];
        match match_known_resolution(&resolutions, &signals(&["discoloration", "mold"])) {
            KrMatch::BlockedByAddedSignal { leftover } => {
                assert!(leftover.contains(&Signal::new("mold")));
            }
            other => panic!("expected BlockedByAddedSignal, got {other:?}"),
        }
    }

    #[test]
    fn kr_more_specific_exception_rule_wins() {
        // 進化: 「変色 + カビ → 廃棄」専用ルールが追加されたら、そちらが優先で適用される。
        let resolutions = vec![
            kr("kr1", &["discoloration"], "自然変色なので問題ありません"),
            kr(
                "kr2",
                &["discoloration", "mold"],
                "カビの可能性があるため廃棄してください",
            ),
        ];
        match match_known_resolution(&resolutions, &signals(&["discoloration", "mold"])) {
            KrMatch::Applicable(found) => assert_eq!(found.id, "kr2"),
            other => panic!("expected Applicable(kr2), got {other:?}"),
        }
        // 「変色」だけなら一般ルールは無傷のまま使える
        match match_known_resolution(&resolutions, &signals(&["discoloration"])) {
            KrMatch::Applicable(found) => assert_eq!(found.id, "kr1"),
            other => panic!("expected Applicable(kr1), got {other:?}"),
        }
    }

    #[test]
    fn kr_no_candidate_returns_none() {
        let resolutions = vec![kr("kr1", &["discoloration"], "a")];
        assert!(matches!(
            match_known_resolution(&resolutions, &signals(&["expiry_question"])),
            KrMatch::None
        ));
    }
}
