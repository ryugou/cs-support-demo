use crate::harness::rules::{RootCause, SourceAuthority};
use serde::Serialize;

/// フィードバックの発生源（record_operator_feedback の閉じた語彙）。
/// customer は「顧客の『違う』の中継」であり、authn 済み担当者経由でも non_authoritative 扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSource {
    Operator,
    Customer,
}

impl FeedbackSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackSource::Operator => "operator",
            FeedbackSource::Customer => "customer",
        }
    }

    /// source_authority への写像（S1-5: principal 種別は authn 由来 + 中継元区分）。
    pub fn authority(self) -> SourceAuthority {
        match self {
            FeedbackSource::Operator => SourceAuthority::Authoritative,
            FeedbackSource::Customer => SourceAuthority::NonAuthoritative,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionRouting {
    /// 会話内修復のみ。永続層に一切書かない。
    ConversationOnly,
    /// 検索改善キューへ（表記追加 / re-rank）。known_resolution を増やさない。
    SearchImprovementQueue,
    /// known_resolution の追加候補（例外ルールの離散 insert）。
    KnownResolutionCandidate,
}

impl CorrectionRouting {
    /// 監査・永続属性用の正本ラベル（serde の snake_case 名と一致させる）。
    pub fn as_str(self) -> &'static str {
        match self {
            CorrectionRouting::ConversationOnly => "conversation_only",
            CorrectionRouting::SearchImprovementQueue => "search_improvement_queue",
            CorrectionRouting::KnownResolutionCandidate => "known_resolution_candidate",
        }
    }
}

/// 訂正インテーク（S1-5）。CIRG 6 判定のうち Step 1 は source_authority / root_cause の 2 軸。
/// 将来の判定軸（error_axis / binding / direction / owner / route）はこの関数に足す。入口の位置は変えない。
pub fn correction_intake(authority: SourceAuthority, root_cause: RootCause) -> CorrectionRouting {
    match authority {
        // source_authority=non_authoritative はいかなる永続層へも書けない（最優先・例外なし）
        SourceAuthority::NonAuthoritative => CorrectionRouting::ConversationOnly,
        SourceAuthority::Authoritative => match root_cause {
            RootCause::RetrievalMiss => CorrectionRouting::SearchImprovementQueue,
            RootCause::KnowledgeError => CorrectionRouting::KnownResolutionCandidate,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::rules::{RootCause, SourceAuthority};

    #[test]
    fn non_authoritative_never_persists() {
        // 最優先・例外なし（S1-5 不変条件）: エンド顧客の訂正は永続層に書かない
        assert_eq!(
            correction_intake(SourceAuthority::NonAuthoritative, RootCause::KnowledgeError),
            CorrectionRouting::ConversationOnly
        );
        assert_eq!(
            correction_intake(SourceAuthority::NonAuthoritative, RootCause::RetrievalMiss),
            CorrectionRouting::ConversationOnly
        );
    }

    #[test]
    fn retrieval_miss_goes_to_search_improvement() {
        // S1-8 Done 条件 4: retrieval_miss は known_resolution を増やさない
        assert_eq!(
            correction_intake(SourceAuthority::Authoritative, RootCause::RetrievalMiss),
            CorrectionRouting::SearchImprovementQueue
        );
    }

    #[test]
    fn authoritative_knowledge_error_becomes_kr_candidate() {
        assert_eq!(
            correction_intake(SourceAuthority::Authoritative, RootCause::KnowledgeError),
            CorrectionRouting::KnownResolutionCandidate
        );
    }
}
