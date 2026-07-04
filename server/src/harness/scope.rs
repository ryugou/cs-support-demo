use crate::harness::authn::Actor;
use anyhow::{anyhow, Result};
use serde::Serialize;

/// サーバ導出の認可 scope（I1）。client 入力からは決して作らない。
/// Step 1 の実効 scope は allowed_schemas のみ。max_sensitivity / label_allowlist は
/// 構造予約であり判定に使わない（S1-1）。
#[derive(Debug, Clone, Serialize)]
pub struct AccessScope {
    pub allowed_schemas: Vec<String>,
    pub max_sensitivity: Option<String>,
    pub label_allowlist: Option<Vec<String>>,
}

impl AccessScope {
    /// PunkRecord 検索に必ず注入する schema。tenant=schema 隔離（S1-9 確定 (b)）。
    pub fn enforced_schema(&self) -> &str {
        &self.allowed_schemas[0]
    }
}

/// actor + project から deterministic に scope を算出する（(A) 経路封鎖ハーネス）。
pub fn resolve_scope(actor: &Actor, project_schema: &str) -> Result<AccessScope> {
    if !actor.allowed_schemas.iter().any(|s| s == project_schema) {
        return Err(anyhow!(
            "actor {} is not allowed to access schema {project_schema}",
            actor.sub
        ));
    }
    Ok(AccessScope {
        allowed_schemas: vec![project_schema.to_string()],
        max_sensitivity: None,
        label_allowlist: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::authn::{Actor, Role};

    fn actor(schemas: &[&str]) -> Actor {
        Actor {
            sub: "op-001".to_string(),
            role: Role::Operator,
            allowed_schemas: schemas.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn allowed_schema_yields_scope() {
        let scope = resolve_scope(&actor(&["sivira-cs-demo"]), "sivira-cs-demo").unwrap();
        assert_eq!(scope.enforced_schema(), "sivira-cs-demo");
        // Step 1 の構造予約: 空で存在する（S1-1）
        assert!(scope.max_sensitivity.is_none());
        assert!(scope.label_allowlist.is_none());
    }

    #[test]
    fn disallowed_schema_is_rejected() {
        assert!(resolve_scope(&actor(&["other-tenant"]), "sivira-cs-demo").is_err());
    }

    #[test]
    fn scope_is_deterministic() {
        let a = resolve_scope(&actor(&["sivira-cs-demo"]), "sivira-cs-demo").unwrap();
        let b = resolve_scope(&actor(&["sivira-cs-demo"]), "sivira-cs-demo").unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }
}
