use crate::config::{AppConfig, ProjectConfig};
use anyhow::{anyhow, Result};
use axum::http::HeaderMap;

#[derive(Clone)]
pub struct ProjectRegistry {
    projects: Vec<ProjectConfig>,
}

impl ProjectRegistry {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            projects: config.projects.clone(),
        }
    }

    pub fn resolve(&self, project_id: &str, headers: &HeaderMap) -> Result<ProjectConfig> {
        let project = self
            .projects
            .iter()
            .find(|project| project.project_id == project_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown project_id: {project_id}"))?;

        if let Some(token) = project
            .bearer_token
            .as_deref()
            .filter(|token| !token.is_empty())
        {
            let expected = format!("Bearer {token}");
            let actual = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if actual != expected {
                return Err(anyhow!("invalid bearer token"));
            }
        }
        Ok(project)
    }
}
