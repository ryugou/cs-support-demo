use crate::harness::rules::Grade;

/// 応答の結果（record_answer_outcome の閉じた語彙）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnswerOutcome {
    Resolved,
    Unresolved,
    ReInquiry,
    WrongAnswer,
}

impl AnswerOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AnswerOutcome::Resolved => "resolved",
            AnswerOutcome::Unresolved => "unresolved",
            AnswerOutcome::ReInquiry => "re_inquiry",
            AnswerOutcome::WrongAnswer => "wrong_answer",
        }
    }
}

/// attempt 群（outcome / outcome_actor 属性）から承認・却下カウントと承認者集合を導出する
/// 純関数。増分更新でなく毎回の再計算にすることで、outcome 記録の再送・部分失敗に対して
/// 構成上冪等になる（同じ attempt 群からは必ず同じカウントが出る）。
pub fn derive_outcome_counts(
    attempts: &[std::collections::HashMap<String, String>],
) -> (u32, u32, Vec<String>) {
    let mut approval_count = 0u32;
    let mut rejection_count = 0u32;
    let mut approver_set: Vec<String> = Vec::new();
    for attempt in attempts {
        match attempt.get("outcome").map(String::as_str) {
            Some("resolved") => {
                approval_count += 1;
                if let Some(actor) = attempt.get("outcome_actor").filter(|a| !a.is_empty()) {
                    if !approver_set.contains(actor) {
                        approver_set.push(actor.clone());
                    }
                }
            }
            Some("wrong_answer") => rejection_count += 1,
            _ => {}
        }
    }
    approver_set.sort();
    (approval_count, rejection_count, approver_set)
}

/// 昇格・降格しきい値（S1-11 追記 4）。具体値は未決のため config [harness.grading] で注入。
#[derive(Debug, Clone)]
pub struct GradingThresholds {
    pub promote_approvals: u32,
    pub promote_approvers: u32,
    pub promote_max_rejection_rate: f32,
    pub demote_rejections: u32,
}

impl From<&crate::config::GradingConfig> for GradingThresholds {
    fn from(config: &crate::config::GradingConfig) -> Self {
        Self {
            promote_approvals: config.promote_approvals,
            promote_approvers: config.promote_approvers,
            promote_max_rejection_rate: config.promote_max_rejection_rate,
            demote_rejections: config.demote_rejections,
        }
    }
}

/// grade の昇格・降格判定（決定論・純関数）。
/// 深さ方向（経験済みパターンの自動化）は量で進み、降格で一方通行にしない（spec「昇格・降格」）。
/// Step 1 の利用者は担当者のため応答セマンティクスは変わらないが、Step 2 の
/// 「顧客直に即答してよいか」の判定材料としてここから運用する（遵守事項 3）。
pub fn regrade(
    current: Grade,
    approval_count: u32,
    rejection_count: u32,
    approver_count: usize,
    thresholds: &GradingThresholds,
) -> Grade {
    // 降格条件を先に評価する（安全側優先）
    if matches!(current, Grade::AutoAnswerAudited)
        && rejection_count >= thresholds.demote_rejections
    {
        return Grade::Demoted;
    }
    if matches!(current, Grade::Demoted) {
        // 降格中の再昇格は Step 1 では自動化しない（人手の見直しを経る）
        return Grade::Demoted;
    }
    let total = approval_count + rejection_count;
    let rejection_rate = if total == 0 {
        0.0
    } else {
        rejection_count as f32 / total as f32
    };
    if approval_count >= thresholds.promote_approvals
        && approver_count >= thresholds.promote_approvers as usize
        && rejection_rate <= thresholds.promote_max_rejection_rate
    {
        return Grade::AutoAnswerAudited;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::rules::Grade;

    fn t() -> GradingThresholds {
        GradingThresholds {
            promote_approvals: 3,
            promote_approvers: 2,
            promote_max_rejection_rate: 0.2,
            demote_rejections: 2,
        }
    }

    #[test]
    fn promotes_when_all_conditions_met() {
        // 承認 3・承認者 2 名・却下率 0 → 自動回答(事後監査)へ格上げ
        assert_eq!(
            regrade(Grade::ApprovalRequired, 3, 0, 2, &t()),
            Grade::AutoAnswerAudited
        );
    }

    #[test]
    fn does_not_promote_on_single_approver() {
        // 承認者多様性が閾値未満なら量が積もっても昇格しない
        assert_eq!(
            regrade(Grade::ApprovalRequired, 10, 0, 1, &t()),
            Grade::ApprovalRequired
        );
    }

    #[test]
    fn does_not_promote_on_high_rejection_rate() {
        // 承認 3・却下 1 → 却下率 0.25 > 0.2 で昇格しない
        assert_eq!(
            regrade(Grade::ApprovalRequired, 3, 1, 2, &t()),
            Grade::ApprovalRequired
        );
    }

    #[test]
    fn demotes_promoted_resolution_on_rejections() {
        // 一方通行にしない: 格上げ済みでも却下が閾値に達したら戻す
        assert_eq!(
            regrade(Grade::AutoAnswerAudited, 5, 2, 3, &t()),
            Grade::Demoted
        );
    }

    #[test]
    fn demoted_stays_until_repromoted() {
        // 降格中は昇格条件を満たし直すまで approval_required 相当として扱う
        assert_eq!(regrade(Grade::Demoted, 3, 1, 2, &t()), Grade::Demoted);
    }

    #[test]
    fn derive_outcome_counts_is_idempotent_over_attempts() {
        use std::collections::HashMap;
        let attempt = |outcome: &str, actor: &str| -> HashMap<String, String> {
            [
                ("outcome".to_string(), outcome.to_string()),
                ("outcome_actor".to_string(), actor.to_string()),
            ]
            .into_iter()
            .collect()
        };
        let attempts = vec![
            attempt("resolved", "op-001"),
            attempt("resolved", "sup-001"),
            attempt("resolved", "op-001"), // 同一承認者は多様性に重複計上しない
            attempt("wrong_answer", "op-002"),
            attempt("unresolved", "op-003"), // カウント対象外
            attempt("", ""),                 // outcome 未確定はカウント対象外
        ];
        let (approvals, rejections, approvers) = derive_outcome_counts(&attempts);
        assert_eq!(approvals, 3);
        assert_eq!(rejections, 1);
        assert_eq!(approvers, vec!["op-001".to_string(), "sup-001".to_string()]);
        // 同じ入力からは必ず同じ導出結果（再送・再計算しても増えない）
        assert_eq!(
            derive_outcome_counts(&attempts),
            (approvals, rejections, approvers)
        );
    }

    #[test]
    fn regrade_is_deterministic() {
        assert_eq!(
            regrade(Grade::ApprovalRequired, 3, 0, 2, &t()),
            regrade(Grade::ApprovalRequired, 3, 0, 2, &t())
        );
    }
}
