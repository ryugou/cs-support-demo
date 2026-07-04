use anyhow::{Context, Result};
use serde::Deserialize;
use std::{env, fs, path::Path};

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub bind_addr: String,
    pub tls_cert_path: Option<String>,
    pub tls_key_path: Option<String>,
    pub vegapunk_endpoint: String,
    pub projects: Vec<ProjectConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    pub project_id: String,
    pub schema: String,
    pub bearer_token: Option<String>,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let body =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        let mut config: Self = toml::from_str(&body)?;
        if let Ok(bind_addr) = env::var("BIND_ADDR") {
            config.bind_addr = bind_addr;
        }
        if let Ok(endpoint) = env::var("VEGAPUNK_ENDPOINT") {
            config.vegapunk_endpoint = endpoint;
        }
        Ok(config)
    }
}
