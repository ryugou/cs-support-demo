use crate::config::ActorConfig;
use anyhow::{anyhow, bail, Context, Result};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub role: String,
    pub exp: usize,
    pub iss: String,
}

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

pub struct Authenticator {
    decoding_key: Option<DecodingKey>,
    actors: HashMap<String, ActorConfig>,
    default_actor: Option<String>,
    issuer: Option<String>,
}

impl Authenticator {
    pub fn new(
        secret: Option<Vec<u8>>,
        actors: &[ActorConfig],
        default_actor: Option<String>,
    ) -> Self {
        Self {
            decoding_key: secret.map(|s| DecodingKey::from_secret(&s)),
            actors: actors.iter().map(|a| (a.sub.clone(), a.clone())).collect(),
            default_actor,
            issuer: None,
        }
    }

    /// 設定時は JWT の iss をこの値と照合する（未設定時は存在のみ要求）。
    pub fn with_issuer(mut self, issuer: Option<String>) -> Self {
        self.issuer = issuer;
        self
    }

    /// Authorization ヘッダから actor を確定する（S1-1 の [認証]）。
    pub fn authenticate(&self, authorization: Option<&str>) -> Result<Actor> {
        match (&self.decoding_key, authorization) {
            (Some(key), Some(header)) => {
                let token = header
                    .strip_prefix("Bearer ")
                    .ok_or_else(|| anyhow!("authorization header is not a bearer token"))?;
                let mut validation = Validation::new(Algorithm::HS256);
                validation.set_required_spec_claims(&["exp", "sub", "iss"]);
                if let Some(issuer) = &self.issuer {
                    validation.set_issuer(&[issuer]);
                }
                let data = decode::<Claims>(token, key, &validation).context("invalid jwt")?;
                self.lookup(&data.claims.sub)
            }
            (Some(_), None) => Err(anyhow!("missing authorization header")),
            (None, _) => {
                let sub = self.default_actor.as_deref().ok_or_else(|| {
                    anyhow!("jwt secret is not configured and no default_actor is set")
                })?;
                tracing::warn!(
                    sub,
                    "jwt secret not configured; falling back to default_actor (dev only)"
                );
                self.lookup(sub)
            }
        }
    }

    fn lookup(&self, sub: &str) -> Result<Actor> {
        let config = self
            .actors
            .get(sub)
            .ok_or_else(|| anyhow!("actor not registered: {sub}"))?;
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
    use jsonwebtoken::{encode, EncodingKey, Header};

    const SECRET: &[u8] = b"test-secret";

    fn actors() -> Vec<ActorConfig> {
        vec![ActorConfig {
            sub: "op-001".to_string(),
            role: "operator".to_string(),
            allowed_schemas: vec!["sivira-cs-demo".to_string()],
        }]
    }

    fn token(sub: &str, exp_offset_secs: i64) -> String {
        let exp = (chrono::Utc::now().timestamp() + exp_offset_secs) as usize;
        let claims = Claims {
            sub: sub.to_string(),
            role: "operator".to_string(),
            exp,
            iss: "test".to_string(),
        };
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(SECRET),
        )
        .unwrap()
    }

    #[test]
    fn valid_jwt_resolves_actor() {
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None);
        let actor = auth
            .authenticate(Some(&format!("Bearer {}", token("op-001", 3600))))
            .unwrap();
        assert_eq!(actor.sub, "op-001");
        assert_eq!(actor.role, Role::Operator);
        assert_eq!(actor.allowed_schemas, vec!["sivira-cs-demo".to_string()]);
    }

    #[test]
    fn expired_jwt_is_rejected() {
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None);
        assert!(auth
            .authenticate(Some(&format!("Bearer {}", token("op-001", -3600))))
            .is_err());
    }

    #[test]
    fn unknown_sub_is_rejected() {
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None);
        assert!(auth
            .authenticate(Some(&format!("Bearer {}", token("ghost", 3600))))
            .is_err());
    }

    #[test]
    fn missing_header_is_rejected_when_secret_configured() {
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None);
        assert!(auth.authenticate(None).is_err());
    }

    #[test]
    fn tampered_jwt_is_rejected() {
        let auth = Authenticator::new(Some(b"other-secret".to_vec()), &actors(), None);
        assert!(auth
            .authenticate(Some(&format!("Bearer {}", token("op-001", 3600))))
            .is_err());
    }

    #[test]
    fn issuer_is_pinned_when_configured() {
        // token(iss="test") に対して issuer を照合する
        let auth = Authenticator::new(Some(SECRET.to_vec()), &actors(), None)
            .with_issuer(Some("test".to_string()));
        assert!(auth
            .authenticate(Some(&format!("Bearer {}", token("op-001", 3600))))
            .is_ok());
        let wrong = Authenticator::new(Some(SECRET.to_vec()), &actors(), None)
            .with_issuer(Some("expected-issuer".to_string()));
        assert!(wrong
            .authenticate(Some(&format!("Bearer {}", token("op-001", 3600))))
            .is_err());
    }

    #[test]
    fn default_actor_fallback_only_without_secret() {
        let auth = Authenticator::new(None, &actors(), Some("op-001".to_string()));
        let actor = auth.authenticate(None).unwrap();
        assert_eq!(actor.sub, "op-001");
        // secret も default_actor も無ければエラー
        let strict = Authenticator::new(None, &actors(), None);
        assert!(strict.authenticate(None).is_err());
    }
}
