use crate::config::{ActorConfig, DefaultActorConfig};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// email → actor の突合（認可の正本＝config actor 表。ホワイトリスト）。
/// AuthN(誰か)は Google OAuth ミドルウェアが済ませ、ここは email→role/scope の導出のみ。
pub struct Authenticator {
    actors_by_email: HashMap<String, ActorConfig>,
    /// 未登録 email へのフォールバック。config `[default_actor]` が明示設定された場合のみ
    /// `Some`。DB ベースのホワイトリストに置き換わるまでの暫定運用（fail-open は既定で禁止、
    /// このフィールドが `None` の限り従来どおり fail-closed）。
    default_actor: Option<DefaultActorConfig>,
}

impl Authenticator {
    pub fn new(actors: &[ActorConfig], default_actor: Option<DefaultActorConfig>) -> Self {
        Self {
            actors_by_email: actors
                .iter()
                .map(|a| (a.email.clone(), a.clone()))
                .collect(),
            default_actor,
        }
    }

    /// 検証済み email を actor に写す。
    ///
    /// 1. `[[actors]]` に一致する email は常にそちらを優先する。
    /// 2. 一致せず `[default_actor]` が設定されていれば、そのフォールバック actor を返す
    ///    （`sub` は `google:{email}`）。誰がフォールバックで入ったか運用者が追跡できるよう
    ///    `tracing::warn!` に email と付与 role を記録する。
    /// 3. どちらにも該当しなければ fail-closed で拒否する。
    pub fn lookup_by_email(&self, email: &str) -> Result<Actor> {
        if let Some(config) = self.actors_by_email.get(email) {
            return Ok(Actor {
                sub: config.sub.clone(),
                role: config.role.parse()?,
                allowed_schemas: config.allowed_schemas.clone(),
            });
        }
        if let Some(default) = &self.default_actor {
            let role: Role = default.role.parse()?;
            tracing::warn!(
                email,
                role = ?role,
                allowed_schemas = ?default.allowed_schemas,
                "未登録 email を default_actor 設定でフォールバック actor として受理した"
            );
            return Ok(Actor {
                sub: format!("google:{email}"),
                role,
                allowed_schemas: default.allowed_schemas.clone(),
            });
        }
        Err(anyhow!("actor not registered for email: {email}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ActorConfig;

    fn actors() -> Vec<ActorConfig> {
        vec![ActorConfig {
            sub: "op-001".to_string(),
            email: "op@sivira.co".to_string(),
            role: "operator".to_string(),
            allowed_schemas: vec!["urtect".to_string()],
        }]
    }

    fn default_actor() -> DefaultActorConfig {
        DefaultActorConfig {
            role: "operator".to_string(),
            allowed_schemas: vec!["urtect".to_string(), "sivira-cs-demo".to_string()],
        }
    }

    #[test]
    fn known_email_resolves_actor() {
        let a = Authenticator::new(&actors(), None);
        let actor = a.lookup_by_email("op@sivira.co").unwrap();
        assert_eq!(actor.sub, "op-001");
        assert_eq!(actor.role, Role::Operator);
    }

    /// default_actor 未設定（`None`）なら、従来どおり未登録 email は fail-closed で拒否する。
    #[test]
    fn unknown_email_is_rejected() {
        let a = Authenticator::new(&actors(), None);
        assert!(a.lookup_by_email("stranger@example.com").is_err());
    }

    /// default_actor 設定ありなら、未登録 email はフォールバック actor
    /// （`sub` は `google:{email}`、role/allowed_schemas は設定値）に解決される。
    #[test]
    fn unknown_email_falls_back_to_default_actor_when_configured() {
        let a = Authenticator::new(&actors(), Some(default_actor()));
        let actor = a.lookup_by_email("stranger@example.com").unwrap();
        assert_eq!(actor.sub, "google:stranger@example.com");
        assert_eq!(actor.role, Role::Operator);
        assert_eq!(
            actor.allowed_schemas,
            vec!["urtect".to_string(), "sivira-cs-demo".to_string()]
        );
    }

    /// default_actor が設定されていても、明示登録済み email はフォールバックに食われず
    /// 従来どおり `[[actors]]` 側の actor（元の sub/role）に解決される。
    #[test]
    fn registered_email_is_not_overridden_by_default_actor() {
        let a = Authenticator::new(&actors(), Some(default_actor()));
        let actor = a.lookup_by_email("op@sivira.co").unwrap();
        assert_eq!(actor.sub, "op-001");
        assert_eq!(actor.role, Role::Operator);
        assert_eq!(actor.allowed_schemas, vec!["urtect".to_string()]);
    }
}
