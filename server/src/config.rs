use anyhow::{Context, Result};
use serde::Deserialize;
use std::{env, fs, path::Path};

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub bind_addr: String,
    pub tls_cert_path: Option<String>,
    pub tls_key_path: Option<String>,
    pub vegapunk_endpoint: String,
    /// vegapunk gRPC の per-call timeout（秒）。大きな snapshot 読みに合わせた既定 120。
    #[serde(default = "default_vegapunk_timeout_secs")]
    pub vegapunk_timeout_secs: u64,
    /// vegapunk gRPC の受信メッセージ上限（MiB）。既定 64（URTECT snapshot 実測 ~6MB）。
    #[serde(default = "default_vegapunk_max_decode_mb")]
    pub vegapunk_max_decode_mb: usize,
    pub projects: Vec<ProjectConfig>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub actors: Vec<ActorConfig>,
    #[serde(default)]
    pub harness: HarnessConfig,
}

fn default_vegapunk_timeout_secs() -> u64 {
    120
}

fn default_vegapunk_max_decode_mb() -> usize {
    64
}

impl AppConfig {
    pub fn grpc_limits(&self) -> crate::vegapunk::GrpcLimits {
        crate::vegapunk::GrpcLimits {
            timeout_secs: self.vegapunk_timeout_secs,
            // 最小 1MiB にクランプ（0 は全 decode 失敗になる設定ミス）。
            // 過大値の乗算は saturating_mul で wrap を防ぐ。
            max_decode_bytes: self
                .vegapunk_max_decode_mb
                .max(1)
                .saturating_mul(1024 * 1024),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AuthConfig {
    /// HS256 共有鍵ファイルパス。env CS_SUPPORT_JWT_SECRET_FILE で上書き可。
    pub jwt_secret_file: Option<String>,
    /// 設定時は JWT の iss をこの値と照合する（未設定時は存在のみ要求）。
    pub jwt_issuer: Option<String>,
    /// JWT 未設定時の dev 専用フォールバック actor（sub）。
    pub default_actor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActorConfig {
    pub sub: String,
    pub role: String,
    pub allowed_schemas: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThresholdsConfig {
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

impl Default for ThresholdsConfig {
    fn default() -> Self {
        Self {
            low: 0.6,
            mid: 0.8,
            high: 0.95,
        }
    }
}

/// grade 昇格・降格のしきい値（S1-11 追記 4）。具体値は S1-9 の未決事項のため
/// config 注入とし、既定値は仮置き。業務確認で確定させる。
#[derive(Debug, Clone, Deserialize)]
pub struct GradingConfig {
    pub promote_approvals: u32,
    pub promote_approvers: u32,
    pub promote_max_rejection_rate: f32,
    pub demote_rejections: u32,
}

impl Default for GradingConfig {
    fn default() -> Self {
        Self {
            promote_approvals: 3,
            promote_approvers: 2,
            promote_max_rejection_rate: 0.2,
            demote_rejections: 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HarnessConfig {
    #[serde(default = "default_audit_log_path")]
    pub audit_log_path: String,
    #[serde(default = "default_queue_path")]
    pub search_improvement_queue_path: String,
    #[serde(default = "default_lexicon_path")]
    pub signal_lexicon_path: String,
    #[serde(default = "default_ng_path")]
    pub ng_dictionary_path: String,
    /// 企業ごとの既定方針（spec「企業ごとの既定方針」の予約）。
    /// escalate_unless_answerable / answer_unless_blocked。
    /// Step 1 では未使用: 実用性のダイヤルは [harness.thresholds] で表現する。
    #[serde(default = "default_policy")]
    pub policy: String,
    #[serde(default)]
    pub thresholds: ThresholdsConfig,
    #[serde(default)]
    pub grading: GradingConfig,
    #[serde(default = "default_escalation_route")]
    pub default_escalation_route: String,
}

fn default_audit_log_path() -> String {
    "data/audit/audit.jsonl".to_string()
}
fn default_queue_path() -> String {
    "data/audit/search-improvement-queue.jsonl".to_string()
}
fn default_lexicon_path() -> String {
    "data/signal-lexicon.json".to_string()
}
fn default_ng_path() -> String {
    "data/ng-dictionary.json".to_string()
}
fn default_policy() -> String {
    "escalate_unless_answerable".to_string()
}
fn default_escalation_route() -> String {
    "triage".to_string()
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            audit_log_path: default_audit_log_path(),
            search_improvement_queue_path: default_queue_path(),
            signal_lexicon_path: default_lexicon_path(),
            ng_dictionary_path: default_ng_path(),
            policy: default_policy(),
            thresholds: ThresholdsConfig::default(),
            grading: GradingConfig::default(),
            default_escalation_route: default_escalation_route(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ManualSchemaKind {
    #[default]
    LegacySection,
    ManualV1,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    pub project_id: String,
    pub schema: String,
    pub bearer_token: Option<String>,
    #[serde(default)]
    pub manual_schema: ManualSchemaKind,
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
        if let Ok(path) = env::var("CS_SUPPORT_JWT_SECRET_FILE") {
            config.auth.jwt_secret_file = Some(path);
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_defaults_to_legacy_section() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(matches!(
            cfg.projects[0].manual_schema,
            ManualSchemaKind::LegacySection
        ));
    }

    #[test]
    fn project_can_select_manual_v1() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "urtect"
manual_schema = "manual_v1"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(matches!(
            cfg.projects[0].manual_schema,
            ManualSchemaKind::ManualV1
        ));
    }

    #[test]
    fn default_escalation_route_defaults_to_triage() {
        assert_eq!(HarnessConfig::default().default_escalation_route, "triage");
    }
}
