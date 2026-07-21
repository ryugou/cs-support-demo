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

/// F4 以前の actor ID prefix（`google:{email}`）。現行は `google-sub:{sub}`。
/// `"google-sub:..."` は 7 文字目が `-` なのでこの prefix には一致せず、両形式は曖昧さなく判別できる。
const LEGACY_GOOGLE_ACTOR_PREFIX: &str = "google:";

/// 承認者の「多様性」判定に使える actor ID か。
///
/// 旧形式 `google:{email}` は email 由来、現行 `google-sub:{sub}` は Google の sub 由来で、
/// **同一人物でも文字列が一致しない**。そのため文字列の重複排除だけでは、1 人が cutover を
/// 跨いで承認しただけで承認者 2 名に見え、`promote_approvers` を単独で満たせてしまう。
///
/// 名寄せ（email → sub の対応付け）には突合表が要るが、それ自体が未実装のため、ここでは
/// 旧形式を多様性の母集団から外して fail-safe 側に倒す。副作用として cutover 前の承認は
/// 多様性に寄与しなくなるが、影響は「昇格が起きにくくなる」方向のみで、降格は妨げない。
fn counts_toward_approver_diversity(actor: &str) -> bool {
    !actor.starts_with(LEGACY_GOOGLE_ACTOR_PREFIX)
}

/// attempt 群から導出した grade 判定の材料。
///
/// 「判定は厳しく、記録は完全に」を型で分離する。`approver_set` は KnownResolution ノードへ
/// 書き戻される**記録**なので旧形式も含む全承認者を保持し、`approver_count` は
/// `regrade` に渡す**判定**用なので名寄せ可能な承認者だけを数える。
/// この 2 つを 1 つの Vec で兼ねると、旧形式を除外した集合がそのまま永続化され、
/// cutover 前に記録済みの承認者名がグラフ上から静かに消える。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeCounts {
    pub approval_count: u32,
    pub rejection_count: u32,
    /// 永続化用の全承認者（旧形式を含む・重複排除済み・ソート済み）。
    pub approver_set: Vec<String>,
    /// 昇格判定に使う承認者数（旧形式を除外した母集団の大きさ）。
    pub approver_count: usize,
    /// 多様性の母集団から外した旧形式承認者の数。0 でなければ昇格が起きにくくなっているため、
    /// 呼び出し側は運用者向けに warn を出すこと。
    pub legacy_excluded_count: usize,
}

/// attempt 群（outcome / outcome_actor 属性）から承認・却下カウントと承認者集合を導出する
/// 純関数。増分更新でなく毎回の再計算にすることで、outcome 記録の再送・部分失敗に対して
/// 構成上冪等になる（同じ attempt 群からは必ず同じカウントが出る）。
///
/// 承認・却下の「量」は旧形式 actor の分も数える。量の判定（`promote_approvals` /
/// `demote_rejections` / `promote_max_rejection_rate`）は同一性に依存せず、除外すると
/// 却下率が下がって降格が遅れる（fail-open）ため。旧形式を落とすのは
/// 承認者の多様性判定（`promote_approvers` の母集団＝`approver_count`）だけに限定し、
/// 永続化される `approver_set` からは落とさない。
pub fn derive_outcome_counts(
    attempts: &[std::collections::HashMap<String, String>],
) -> OutcomeCounts {
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
    let approver_count = approver_set
        .iter()
        .filter(|a| counts_toward_approver_diversity(a))
        .count();
    OutcomeCounts {
        legacy_excluded_count: approver_set.len() - approver_count,
        approval_count,
        rejection_count,
        approver_set,
        approver_count,
    }
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
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(counts.approval_count, 3);
        assert_eq!(counts.rejection_count, 1);
        assert_eq!(
            counts.approver_set,
            vec!["op-001".to_string(), "sup-001".to_string()]
        );
        assert_eq!(counts.approver_count, 2);
        // 同じ入力からは必ず同じ導出結果（再送・再計算しても増えない）
        assert_eq!(derive_outcome_counts(&attempts), counts);
    }

    /// テスト用 attempt。`outcome` / `outcome_actor` 以外の属性は本関数の判定に影響しない。
    fn attempt(outcome: &str, actor: &str) -> std::collections::HashMap<String, String> {
        [
            ("outcome".to_string(), outcome.to_string()),
            ("outcome_actor".to_string(), actor.to_string()),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn same_person_across_actor_id_cutover_is_not_counted_as_two_approvers() {
        // 回帰: actor ID 形式が google:{email} → google-sub:{sub} に変わったため、
        // 同一人物の cutover 前後の承認が別人として 2 名に見えていた（promote_approvers=2 を
        // 1 人で満たせる昇格経路）。旧形式は名寄せ不能なので多様性から除外し、1 名に落とす。
        let attempts = vec![
            attempt("resolved", "google:alice@example.com"),
            attempt("resolved", "google-sub:12345"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(counts.approver_count, 1);
        // 承認「量」は事実として残す（除外するのは同一性判定＝多様性のみ）
        assert_eq!(counts.approval_count, 2);
        assert_eq!(counts.rejection_count, 0);
    }

    #[test]
    fn legacy_actor_ids_alone_yield_no_approver_diversity() {
        // 境界: 旧形式のみ。互いに別人であっても名寄せの根拠が無いため多様性 0 に倒す
        // （fail-safe: 昇格が起きないだけで、降格判定には影響しない）。
        let attempts = vec![
            attempt("resolved", "google:alice@example.com"),
            attempt("resolved", "google:bob@example.com"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(counts.approval_count, 2);
        assert_eq!(counts.approver_count, 0);
        // ただし記録としては両名とも残す（永続化で欠損させない）
        assert_eq!(counts.approver_set.len(), 2);
    }

    #[test]
    fn distinct_current_actor_ids_still_count_as_two_approvers() {
        // 退行防止: 新形式のみの正常系は従来どおり 2 名として数える
        let attempts = vec![
            attempt("resolved", "google-sub:12345"),
            attempt("resolved", "google-sub:67890"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(
            counts.approver_set,
            vec![
                "google-sub:12345".to_string(),
                "google-sub:67890".to_string()
            ]
        );
        assert_eq!(counts.approver_count, 2);
    }

    #[test]
    fn repeated_approvals_by_same_current_actor_count_once() {
        // 境界: 同一 actor の重複承認は多様性 1 のまま
        let attempts = vec![
            attempt("resolved", "google-sub:12345"),
            attempt("resolved", "google-sub:12345"),
            attempt("resolved", "google-sub:12345"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(counts.approval_count, 3);
        assert_eq!(counts.approver_set, vec!["google-sub:12345".to_string()]);
        assert_eq!(counts.approver_count, 1);
    }

    #[test]
    fn empty_attempts_yield_zero_counts() {
        // 境界: attempt 0 件
        let counts = derive_outcome_counts(&[]);
        assert_eq!((counts.approval_count, counts.rejection_count), (0, 0));
        assert!(counts.approver_set.is_empty());
        assert_eq!(counts.approver_count, 0);
        assert_eq!(counts.legacy_excluded_count, 0);
    }

    #[test]
    fn legacy_rejections_still_count_toward_demotion() {
        // 却下は除外しない: 除外すると却下率が下がり降格が遅れる（fail-open）ため、
        // 旧形式の却下も従来どおり数える。
        let attempts = vec![
            attempt("wrong_answer", "google:alice@example.com"),
            attempt("wrong_answer", "google-sub:12345"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!((counts.approval_count, counts.rejection_count), (0, 2));
        assert_eq!(
            regrade(Grade::AutoAnswerAudited, 0, counts.rejection_count, 0, &t()),
            Grade::Demoted
        );
    }

    #[test]
    fn non_google_actor_ids_are_unaffected_by_legacy_exclusion() {
        // 除外は Google の旧 prefix に限定する。他 issuer / 既存の運用 ID を巻き込まない。
        let attempts = vec![
            attempt("resolved", "op-001"),
            attempt("resolved", "sup-001"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(
            counts.approver_set,
            vec!["op-001".to_string(), "sup-001".to_string()]
        );
        assert_eq!(counts.approver_count, 2);
        assert_eq!(counts.legacy_excluded_count, 0);
    }

    #[test]
    fn cutover_straddling_single_person_cannot_promote() {
        // 統合: 承認 3 件すべてが同一人物（cutover 跨ぎ）なら AutoAnswerAudited に昇格しない
        let attempts = vec![
            attempt("resolved", "google:alice@example.com"),
            attempt("resolved", "google:alice@example.com"),
            attempt("resolved", "google-sub:12345"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(
            regrade(
                Grade::ApprovalRequired,
                counts.approval_count,
                counts.rejection_count,
                counts.approver_count,
                &t()
            ),
            Grade::ApprovalRequired
        );
        // 昇格しなくても、記録側には 2 名（旧形式含む）が残っている
        assert_eq!(counts.approver_set.len(), 2);
    }

    #[test]
    fn legacy_approvers_are_preserved_for_persistence_but_excluded_from_diversity() {
        // W2 回帰: approver_set は KnownResolution ノードへ書き戻される（記録）。
        // 旧形式を集合から落とすと、cutover 前に記録済みの承認者名が次の outcome 記録で
        // 静かに消える。記録は完全に、判定だけ厳しく。
        let attempts = vec![
            attempt("resolved", "google:alice@example.com"),
            attempt("resolved", "google-sub:12345"),
        ];
        let counts = derive_outcome_counts(&attempts);
        // 永続化用: 除外前の全承認者が残る
        assert_eq!(
            counts.approver_set,
            vec![
                "google-sub:12345".to_string(),
                "google:alice@example.com".to_string()
            ]
        );
        // 判定用: 旧形式を除いた 1 名
        assert_eq!(counts.approver_count, 1);
        // 運用者が「なぜ昇格しないか」を辿れるよう除外件数を露出する（warn と同じ値）
        assert_eq!(counts.legacy_excluded_count, 1);
    }

    #[test]
    fn no_legacy_approvers_reports_zero_exclusions() {
        // 境界: 新形式のみなら除外は起きず、warn も出ない（件数 0）
        let attempts = vec![
            attempt("resolved", "google-sub:12345"),
            attempt("resolved", "google-sub:67890"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(counts.approver_count, 2);
        assert_eq!(counts.legacy_excluded_count, 0);
        assert_eq!(counts.approver_set.len(), 2);
    }

    #[test]
    fn duplicate_legacy_approver_is_counted_once_in_exclusions() {
        // 境界: 同一の旧形式 actor が複数回承認しても、集合・除外件数ともに 1
        let attempts = vec![
            attempt("resolved", "google:alice@example.com"),
            attempt("resolved", "google:alice@example.com"),
        ];
        let counts = derive_outcome_counts(&attempts);
        assert_eq!(counts.approver_set, vec!["google:alice@example.com"]);
        assert_eq!(counts.approver_count, 0);
        assert_eq!(counts.legacy_excluded_count, 1);
    }

    #[test]
    fn regrade_is_deterministic() {
        assert_eq!(
            regrade(Grade::ApprovalRequired, 3, 0, 2, &t()),
            regrade(Grade::ApprovalRequired, 3, 0, 2, &t())
        );
    }
}
