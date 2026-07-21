use crate::oauth::VerifiedIdentity;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// actor ID の prefix。Google の安定した principal ID（tokeninfo `sub`）由来であることを示す。
///
/// 旧形式は `google:{email}` だった（F4 以前）。prefix を `google:` から `google-sub:` へ
/// 変えているのは、既存の WORM 監査エントリと新エントリを **識別子の形式で判別可能**に
/// するため。`google:` のまま sub を入れると、旧新の区別が「`@` を含むか」という
/// 推測に頼ることになり、監査の読み手が確信を持てない。
const ACTOR_ID_PREFIX: &str = "google-sub:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Operator,
    Supervisor,
    Admin,
}

impl std::str::FromStr for Role {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "operator" => Ok(Role::Operator),
            "supervisor" => Ok(Role::Supervisor),
            "admin" => Ok(Role::Admin),
            other => bail!("unknown actor role: {other}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Actor {
    /// 監査主体の主識別子。`google-sub:{Google の sub}`。
    /// email 変更・別名・再割当てに影響されない安定 ID（F4）。
    pub sub: String,
    /// 認証時点の email。表示・調査・旧形式エントリとの名寄せ用の「当時の値」であり、
    /// 同一人物の判定に使ってはならない（変わりうるため）。
    pub email: String,
    pub role: Role,
    pub allowed_schemas: Vec<String>,
}

/// 検証済み Google identity → actor の導出。
///
/// 運用判断（2026-07、config `[[actors]]` ホワイトリストの廃止に伴う）:
/// アクセス制御は将来 DB ベースで実装する方針であり、config と DB の二重の正本を
/// 避けるため、config ベースの email ホワイトリストは廃止した。直前に導入した
/// `[default_actor]`（オプショナルなフォールバック的ホワイトリスト設定、
/// commit aa9e3d8）も同じ理由で revert 済み（commit e90ef59）。
/// 同方向の「config 側にホワイトリスト相当の設定を足す」実装は再度行わないこと。
///
/// 現状は AuthN(誰か)を済ませた Google OAuth ミドルウェアの検証済み identity を
/// 突合なしに受け入れ、無条件で Actor を導出する:
/// - `sub` は `google-sub:{Google の sub}` とし、監査ログ（WORM audit / queue 等）で
///   個人を安定して追跡できるようにする（F4。旧 `google:{email}` は email 変更・
///   別名・再割当てで同一性が壊れるため廃止）。email は `Actor.email` に併走させる。
/// - `role` は `Supervisor` 固定とする。ホワイトリスト廃止に伴い全 tool を
///   実行可能とする運用判断であり、`harness::admit_known_resolution` 等が
///   要求する `Supervisor | Admin` ゲートを通す必要があるため、他 role では
///   機能が壊れる。
/// - `allowed_schemas` は config `[[projects]]` の schema 全件とする
///   （project ごとの制限は `Harness::begin` → `scope::resolve_scope` が
///   URL の `project_id` から解決した schema 単体に絞り込む）。
///
/// 【DB 実装時の差し替え seam】ここが identity→role/allowed_schemas 導出の一点。
/// DB ベースのアクセス制御を実装する際は `lookup_by_identity` の中身を DB 参照に
/// 差し替える。呼び出し口（`Harness::begin`）や `Actor` の型は変えなくてよい。
/// 突合キーには email ではなく `VerifiedIdentity::sub` を使うこと（email はユーザ側で
/// 変わりうるため、権限表の主キーにすると権限が意図せず移動する）。
pub struct Authenticator {
    /// config `[[projects]]` 由来の schema 一覧。無条件許可 actor の
    /// allowed_schemas はここから複製する。
    project_schemas: Vec<String>,
}

impl Authenticator {
    pub fn new(project_schemas: Vec<String>) -> Self {
        Self { project_schemas }
    }

    /// 検証済み Google identity を actor に写す。
    ///
    /// 現状は突合をしないため失敗しないが、将来 DB 参照に差し替えると
    /// 接続失敗等で `Err` を返しうるため、シグネチャは `Result` を維持する。
    pub fn lookup_by_identity(&self, identity: &VerifiedIdentity) -> Result<Actor> {
        let sub = format!("{ACTOR_ID_PREFIX}{}", identity.sub);
        // 無条件許可は運用上見えるようにする（毎リクエスト発生しうるため info ではなく debug）。
        // 個人を追跡できる sub/email を記録するのは、fail-open な導出であることを
        // 障害調査・監査時にログから追えるようにするため。
        tracing::debug!(
            email = %identity.email,
            sub = %sub,
            role = "supervisor",
            "actor resolved without whitelist check (config-based [[actors]] whitelist removed; DB-based ACL is a future seam, see Authenticator doc comment)"
        );
        Ok(Actor {
            sub,
            email: identity.email.clone(),
            role: Role::Supervisor,
            allowed_schemas: self.project_schemas.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_schemas() -> Vec<String> {
        vec!["urtect".to_string(), "sivira-cs-demo".to_string()]
    }

    fn identity(sub: &str, email: &str) -> VerifiedIdentity {
        VerifiedIdentity {
            sub: sub.to_string(),
            email: email.to_string(),
        }
    }

    /// 仕様変更（config actor ホワイトリスト廃止）の中核テスト:
    /// 突合表に登録の有無を問わず、任意の検証済み identity が supervisor role の
    /// Actor に解決され、allowed_schemas は config 全 project の schema と一致する。
    #[test]
    fn any_verified_identity_resolves_to_supervisor_actor_with_all_project_schemas() {
        let a = Authenticator::new(project_schemas());
        let actor = a
            .lookup_by_identity(&identity("100000000000000000001", "nobody@example.com"))
            .unwrap();
        assert_eq!(actor.role, Role::Supervisor);
        assert_eq!(actor.allowed_schemas, project_schemas());
    }

    /// F4: actor の主識別子は Google の安定した principal ID（`sub`）由来であり、
    /// email は含めない（email は変更・再割当てされうるため監査主体にしない）。
    #[test]
    fn actor_sub_is_derived_from_stable_google_subject_not_email() {
        let a = Authenticator::new(project_schemas());
        let actor = a
            .lookup_by_identity(&identity("101572111487015263315", "alice@sivira.co"))
            .unwrap();
        assert_eq!(actor.sub, "google-sub:101572111487015263315");
        assert!(
            !actor.sub.contains("alice@sivira.co"),
            "actor id must not embed the mutable email"
        );
        // email は「当時の値」として別フィールドに残す（調査・表示用）
        assert_eq!(actor.email, "alice@sivira.co");
    }

    /// F4: 同一人物の email が変わっても actor の主識別子は変わらない
    /// （旧実装の `google:{email}` では別人として記録されていた）。
    #[test]
    fn actor_sub_is_stable_across_email_change() {
        let a = Authenticator::new(project_schemas());
        let before = a
            .lookup_by_identity(&identity("101572111487015263315", "old@sivira.co"))
            .unwrap();
        let after = a
            .lookup_by_identity(&identity("101572111487015263315", "new@sivira.co"))
            .unwrap();
        assert_eq!(before.sub, after.sub);
        assert_ne!(before.email, after.email);
    }

    /// F4: 旧形式 `google:{email}` と新形式 `google-sub:{sub}` は prefix で判別できる。
    /// 既存の WORM 監査エントリが読めなくならないことの前提（`audit.rs` のテスト参照）。
    #[test]
    fn actor_sub_prefix_distinguishes_new_format_from_legacy_email_format() {
        let a = Authenticator::new(project_schemas());
        let actor = a
            .lookup_by_identity(&identity("101572111487015263315", "alice@sivira.co"))
            .unwrap();
        let legacy = "google:alice@sivira.co";
        assert!(actor.sub.starts_with("google-sub:"));
        assert!(!legacy.starts_with("google-sub:"));
        assert_ne!(actor.sub, legacy);
    }

    /// 旧仕様 `unknown_email_is_rejected` の反転テスト。
    /// ホワイトリスト廃止により「未登録 email は拒否される」という旧仕様は成立しなくなった。
    #[test]
    fn formerly_unknown_email_is_no_longer_rejected() {
        let a = Authenticator::new(project_schemas());
        assert!(a
            .lookup_by_identity(&identity("100000000000000000002", "stranger@example.com"))
            .is_ok());
    }

    #[test]
    fn different_subjects_yield_distinct_sub_for_audit_traceability() {
        let a = Authenticator::new(project_schemas());
        let one = a
            .lookup_by_identity(&identity("100000000000000000001", "one@example.com"))
            .unwrap();
        let two = a
            .lookup_by_identity(&identity("100000000000000000002", "two@example.com"))
            .unwrap();
        assert_ne!(one.sub, two.sub);
    }

    /// 同一 email が別 principal に再割当てされた場合、別 actor として記録される
    /// （email 主識別だと同一人物に見えてしまっていたケース）。
    #[test]
    fn same_email_reassigned_to_new_subject_yields_distinct_actor() {
        let a = Authenticator::new(project_schemas());
        let old = a
            .lookup_by_identity(&identity("100000000000000000001", "shared@example.com"))
            .unwrap();
        let new = a
            .lookup_by_identity(&identity("100000000000000000009", "shared@example.com"))
            .unwrap();
        assert_ne!(old.sub, new.sub);
        assert_eq!(old.email, new.email);
    }
}
