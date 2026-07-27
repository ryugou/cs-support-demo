use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{env, fs, path::Path};

/// secret ファイルを読み、trim して返す共通ヘルパー。空（trim 後）は拒否する
/// （設定ミスによる実質的な認証無効化・鍵未設定を防ぐ fail closed）。
///
/// `label` は呼び出し元がエラーメッセージに残したい用途名（例: `"jwt secret file"` /
/// `"llm api_key_file"`）。呼び出し側で後から `.with_context()` を重ねる形にすると、
/// anyhow の `Display`（`to_string()`）は最も外側のフレームしか見せないため、内側の
/// "is empty" 等の詳細が呼び出し元から見えなくなる（`{:?}` の chain 表示でしか追えなくなる）。
/// 呼び出し元テストは `err.to_string()` で "is empty" 等の文言を直接 assert しているため、
/// ここで label を埋め込んで従来メッセージと同一のフラットな 1 メッセージを生成する。
pub(crate) fn read_secret_file(label: &str, path: &Path) -> Result<String> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("read {label} {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{label} {} is empty", path.display());
    }
    Ok(trimmed.to_string())
}

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
    pub harness: HarnessConfig,
    /// signal 抽出エージェント（LLM コンポーネント）の設定。既定は無効（lexicon 単独）。
    #[serde(default)]
    pub llm: LlmConfig,
}

/// Anthropic Messages API による signal 抽出エージェントの設定。
/// API キーは env `CS_SUPPORT_LLM_API_KEY` または `api_key_file` から注入する
/// （config への平文記載は禁止）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub enabled: bool,
    pub model: String,
    pub endpoint: String,
    pub api_key_file: Option<String>,
    pub timeout_secs: u64,
    pub max_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "claude-haiku-4-5-20251001".to_string(),
            endpoint: "https://api.anthropic.com/v1/messages".to_string(),
            api_key_file: None,
            timeout_secs: 20,
            max_tokens: 300,
        }
    }
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
            // keepalive は既定のまま。常駐サーバは長寿命チャネルを使い回すため、
            // アイドル後の死んだ接続を h2 PING で検知する必要がある（config で切らせない）。
            ..crate::vegapunk::GrpcLimits::default()
        }
    }
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
    /// 意味検索（ベクトル経路）を manual retrieval に合成するか。
    /// embeddings を ingest 済みのテナントでのみ有効化する（urtect design §2.3）。
    #[serde(default)]
    pub vector_route_enabled: bool,
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
            vector_route_enabled: false,
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
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の一時ディレクトリ（テストごとに衝突しないよう uuid でユニーク化する）。
    fn temp_dir() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cs-support-config-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn read_secret_file_trims_and_returns_content() {
        let dir = temp_dir();
        let path = dir.join("secret.txt");
        std::fs::write(&path, "  s3cr3t\n").expect("write secret file");
        let got = read_secret_file("test secret", &path).expect("non-empty file must read ok");
        assert_eq!(got, "s3cr3t");
    }

    #[test]
    fn read_secret_file_rejects_blank_content() {
        let dir = temp_dir();
        let path = dir.join("blank.txt");
        std::fs::write(&path, "   \n\t \n").expect("write blank secret file");
        let err = read_secret_file("test secret", &path)
            .expect_err("whitespace-only content must be rejected as empty");
        assert!(
            err.to_string().contains("is empty"),
            "error must say the secret is empty, got: {err}"
        );
    }

    #[test]
    fn read_secret_file_reports_label_and_path_on_missing_file() {
        let dir = temp_dir();
        let path = dir.join("does-not-exist.txt");
        let err = read_secret_file("test secret", &path)
            .expect_err("missing file must be a read error, not silently Ok");
        let msg = err.to_string();
        assert!(
            msg.contains("test secret"),
            "error must identify which secret failed to read, got: {msg}"
        );
        assert!(
            msg.contains(&path.display().to_string()),
            "error must include the file path for operator diagnosis, got: {msg}"
        );
    }

    #[test]
    fn llm_config_defaults_disabled() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(!cfg.llm.enabled);
        assert_eq!(cfg.llm.model, "claude-haiku-4-5-20251001");
        assert!(!cfg.harness.vector_route_enabled);
    }

    #[test]
    fn llm_config_parses_section() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
[llm]
enabled = true
api_key_file = "/tmp/key"
[harness]
vector_route_enabled = true
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(cfg.llm.enabled);
        assert_eq!(cfg.llm.api_key_file.as_deref(), Some("/tmp/key"));
        assert!(cfg.harness.vector_route_enabled);
    }

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
