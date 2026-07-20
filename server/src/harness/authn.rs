use crate::config::ActorConfig;
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
}

impl Authenticator {
    pub fn new(actors: &[ActorConfig]) -> Self {
        Self {
            actors_by_email: actors
                .iter()
                .map(|a| (a.email.clone(), a.clone()))
                .collect(),
        }
    }

    /// 検証済み email を actor に写す。未登録 email は fail-closed で拒否。
    pub fn lookup_by_email(&self, email: &str) -> Result<Actor> {
        let config = self
            .actors_by_email
            .get(email)
            .ok_or_else(|| anyhow!("actor not registered for email: {email}"))?;
        Ok(Actor {
            sub: config.sub.clone(),
            role: config.role.parse()?,
            allowed_schemas: config.allowed_schemas.clone(),
        })
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

    #[test]
    fn known_email_resolves_actor() {
        let a = Authenticator::new(&actors());
        let actor = a.lookup_by_email("op@sivira.co").unwrap();
        assert_eq!(actor.sub, "op-001");
        assert_eq!(actor.role, Role::Operator);
    }

    #[test]
    fn unknown_email_is_rejected() {
        let a = Authenticator::new(&actors());
        assert!(a.lookup_by_email("stranger@example.com").is_err());
    }
}
