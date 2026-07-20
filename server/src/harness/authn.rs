use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

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
    pub sub: String,
    pub role: Role,
    pub allowed_schemas: Vec<String>,
}

/// email → actor の導出。
///
/// 運用判断（2026-07、config `[[actors]]` ホワイトリストの廃止に伴う）:
/// アクセス制御は将来 DB ベースで実装する方針であり、config と DB の二重の正本を
/// 避けるため、config ベースの email ホワイトリストは廃止した。直前に導入した
/// `[default_actor]`（オプショナルなフォールバック的ホワイトリスト設定、
/// commit aa9e3d8）も同じ理由で revert 済み（commit e90ef59）。
/// 同方向の「config 側にホワイトリスト相当の設定を足す」実装は再度行わないこと。
///
/// 現状は AuthN(誰か)を済ませた Google OAuth ミドルウェアの検証済み email を
/// 突合なしに受け入れ、無条件で Actor を導出する:
/// - `sub` は `google:{email}` とし、監査ログ（WORM audit / queue 等）で
///   個人を追跡できるようにする。
/// - `role` は `Supervisor` 固定とする。ホワイトリスト廃止に伴い全 tool を
///   実行可能とする運用判断であり、`harness::admit_known_resolution` 等が
///   要求する `Supervisor | Admin` ゲートを通す必要があるため、他 role では
///   機能が壊れる。
/// - `allowed_schemas` は config `[[projects]]` の schema 全件とする
///   （project ごとの制限は `Harness::begin` → `scope::resolve_scope` が
///   URL の `project_id` から解決した schema 単体に絞り込む）。
///
/// 【DB 実装時の差し替え seam】ここが email→role/allowed_schemas 導出の一点。
/// DB ベースのアクセス制御を実装する際は `lookup_by_email` の中身を DB 参照に
/// 差し替える。呼び出し口（`Harness::begin`）や `Actor` の型は変えなくてよい。
pub struct Authenticator {
    /// config `[[projects]]` 由来の schema 一覧。無条件許可 actor の
    /// allowed_schemas はここから複製する。
    project_schemas: Vec<String>,
}

impl Authenticator {
    pub fn new(project_schemas: Vec<String>) -> Self {
        Self { project_schemas }
    }

    /// 検証済み email を actor に写す。
    ///
    /// 現状は突合をしないため失敗しないが、将来 DB 参照に差し替えると
    /// 接続失敗等で `Err` を返しうるため、シグネチャは `Result` を維持する。
    pub fn lookup_by_email(&self, email: &str) -> Result<Actor> {
        let sub = format!("google:{email}");
        // 無条件許可は運用上見えるようにする（毎リクエスト発生しうるため info ではなく debug）。
        // 個人を追跡できる sub/email を記録するのは、fail-open な導出であることを
        // 障害調査・監査時にログから追えるようにするため。
        tracing::debug!(
            email = %email,
            sub = %sub,
            role = "supervisor",
            "actor resolved without whitelist check (config-based [[actors]] whitelist removed; DB-based ACL is a future seam, see Authenticator doc comment)"
        );
        Ok(Actor {
            sub,
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

    /// 仕様変更（config actor ホワイトリスト廃止）の中核テスト:
    /// 突合表に登録の有無を問わず、任意の検証済み email が supervisor role の
    /// Actor に解決され、allowed_schemas は config 全 project の schema と一致する。
    #[test]
    fn any_verified_email_resolves_to_supervisor_actor_with_all_project_schemas() {
        let a = Authenticator::new(project_schemas());
        let actor = a.lookup_by_email("nobody-registered@example.com").unwrap();
        assert_eq!(actor.sub, "google:nobody-registered@example.com");
        assert_eq!(actor.role, Role::Supervisor);
        assert_eq!(actor.allowed_schemas, project_schemas());
    }

    /// 旧仕様 `unknown_email_is_rejected` の反転テスト。
    /// ホワイトリスト廃止により「未登録 email は拒否される」という旧仕様は成立しなくなった。
    #[test]
    fn formerly_unknown_email_is_no_longer_rejected() {
        let a = Authenticator::new(project_schemas());
        assert!(a.lookup_by_email("stranger@example.com").is_ok());
    }

    #[test]
    fn different_emails_yield_distinct_sub_for_audit_traceability() {
        let a = Authenticator::new(project_schemas());
        let one = a.lookup_by_email("one@example.com").unwrap();
        let two = a.lookup_by_email("two@example.com").unwrap();
        assert_ne!(one.sub, two.sub);
    }
}
