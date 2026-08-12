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
    /// 応答生成 API（`POST /{project_id}/api/reply`）の設定。既定は無効。
    #[serde(default)]
    pub api: ApiConfig,
}

/// 応答生成 API の設定。API キーは env `CS_SUPPORT_ANSWER_API_KEY`（Secret Manager 注入）
/// からのみ読む（config への平文記載はしない）。`enabled = true` かつ鍵未設定は
/// main.rs の起動時 fail-closed チェックで弾く（`LlmConfig` と同じパターン）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub enabled: bool,
    pub fallback_reply_text: String,
    /// 聞き返し（ヒアリングループ）の上限ターン数（会話フロー v1.1 design doc §2）。
    pub clarify_max_turns: u32,
    /// 営業時間案内・希望時間帯の重なり判定に使う設定（design doc §6）。
    pub business_hours: BusinessHoursConfig,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fallback_reply_text: default_fallback_reply_text(),
            clarify_max_turns: 3,
            business_hours: BusinessHoursConfig::default(),
        }
    }
}

fn default_fallback_reply_text() -> String {
    "お問い合わせありがとうございます。担当者が確認のうえ、あらためてご連絡いたします。".to_string()
}

/// 営業時間の設定（会話フロー v1.1 design doc §6）。祝日は考慮しない（v1.1 スコープ外）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BusinessHoursConfig {
    /// `"mon-fri"` または `"everyday"`。
    pub days: String,
    /// "HH:MM"（例: "10:00"）。
    pub start: String,
    /// "HH:MM"（例: "18:00"）。
    pub end: String,
    /// IANA タイムゾーン名（例: "Asia/Tokyo"）。
    pub tz: String,
}

impl Default for BusinessHoursConfig {
    fn default() -> Self {
        Self {
            days: "mon-fri".to_string(),
            start: "10:00".to_string(),
            end: "18:00".to_string(),
            tz: "Asia/Tokyo".to_string(),
        }
    }
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
    /// manual 検索スコアの v2（TF / 長さ正規化 / 型番 run 除外 / 密度 tiebreak）を有効にするか。
    ///
    /// **既定 false。** design `docs/superpowers/specs/2026-08-05-manual-scoring-tf-lengthnorm-design.md`
    /// の 4 つの変更を**まとめて**切り替える kill switch（個別フラグにすると組み合わせが
    /// 16 通りになり、デモ中の切り分けが実行不能になる）。false のとき従来と完全に同一の
    /// **スコアと順位**になる。デモで劣化が見えたらこの 1 行を false に戻して再デプロイする。
    #[serde(default)]
    pub manual_scoring_v2_enabled: bool,
    /// `evaluate_answerability` が顧客向け返信文の**下書き**を返すか（デモ用）。
    ///
    /// **既定 false。** 有効化すると評価 1 回につき Anthropic API 呼び出しが 1 回増え、
    /// 顧客問い合わせ本文と（Allowed 時のみ）マニュアル抜粋が Anthropic へ送信される。
    /// 文面の正本は client 側という spec の結論は変わらない（`harness::reply` の doc を参照）。
    #[serde(default)]
    pub customer_reply_draft_enabled: bool,
    /// 返信文下書きの `max_tokens`。signal 抽出用（`[llm] max_tokens`、既定 300）とは別枠。
    /// 返信文は数百字必要で、抽出用の上限では途中で切れる。
    #[serde(default = "default_reply_draft_max_tokens")]
    pub customer_reply_draft_max_tokens: u32,
}

fn default_reply_draft_max_tokens() -> u32 {
    700
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
            customer_reply_draft_enabled: false,
            customer_reply_draft_max_tokens: default_reply_draft_max_tokens(),
            default_escalation_route: default_escalation_route(),
            vector_route_enabled: false,
            manual_scoring_v2_enabled: false,
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
        assert!(
            !cfg.harness.manual_scoring_v2_enabled,
            "manual scoring v2 must stay opt-in (kill switch defaults to off)"
        );
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
manual_scoring_v2_enabled = true
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(cfg.llm.enabled);
        assert_eq!(cfg.llm.api_key_file.as_deref(), Some("/tmp/key"));
        assert!(cfg.harness.vector_route_enabled);
        assert!(cfg.harness.manual_scoring_v2_enabled);
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
    fn api_config_defaults_to_disabled_when_section_is_absent() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(!cfg.api.enabled);
        assert_eq!(
            cfg.api.fallback_reply_text,
            "お問い合わせありがとうございます。担当者が確認のうえ、あらためてご連絡いたします。"
        );
        assert_eq!(cfg.api.clarify_max_turns, 3);
        assert_eq!(cfg.api.business_hours.days, "mon-fri");
        assert_eq!(cfg.api.business_hours.start, "10:00");
        assert_eq!(cfg.api.business_hours.end, "18:00");
        assert_eq!(cfg.api.business_hours.tz, "Asia/Tokyo");
    }

    #[test]
    fn api_config_parses_section() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
[api]
enabled = true
fallback_reply_text = "テスト用フォールバック"
clarify_max_turns = 5
[api.business_hours]
days = "everyday"
start = "09:00"
end = "21:00"
tz = "UTC"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(cfg.api.enabled);
        assert_eq!(cfg.api.fallback_reply_text, "テスト用フォールバック");
        assert_eq!(cfg.api.clarify_max_turns, 5);
        assert_eq!(cfg.api.business_hours.days, "everyday");
        assert_eq!(cfg.api.business_hours.start, "09:00");
        assert_eq!(cfg.api.business_hours.end, "21:00");
        assert_eq!(cfg.api.business_hours.tz, "UTC");
    }

    #[test]
    fn default_escalation_route_defaults_to_triage() {
        assert_eq!(HarnessConfig::default().default_escalation_route, "triage");
    }

    #[test]
    fn grpc_limits_keeps_resident_server_keep_alive_default() {
        // 常駐サーバ（main.rs）が使う `grpc_limits()` は keepalive を config から
        // 変更できない安全前提（`..GrpcLimits::default()` の 1 行とコメントだけで守られている）。
        // merge_schema CLI は自前の `merge_cli_limits()` で別途 `keep_alive: None` を組む
        // （こちらは通らない）ので、config 経路の既定が変わっていないことをここで固定する。
        // config から keepalive を切れるようにする変更が将来入っても、この assert が
        // 赤くならない限り「常駐サーバの keepalive は不変」という前提は検出できない。
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.grpc_limits().keep_alive,
            Some(crate::vegapunk::KeepAlive::default()),
            "常駐サーバの h2 PING keepalive は既定のまま（config からは切れない）"
        );
    }
}
