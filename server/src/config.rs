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
    /// 管理 SPA（`/admin` 配下）の静的ビルド成果物ディレクトリ（`index.html` を含む）。
    /// design doc（`2026-08-16-admin-dashboard-design.md` §5）: 同一 axum サーバの
    /// `/admin` から静的配信する。config ファイルからの相対パス（`config_dir` 基準）または
    /// 絶対パスのどちらも許容する（main.rs 側で解決する。`HarnessConfig` 配下の各 `*_path` と
    /// 同じ「相対は config_dir 基準」方針に揃える）。
    #[serde(default = "default_admin_static_dir")]
    pub admin_static_dir: String,
    /// homesec advisor（Issue #34）固有設定。CS（urtect）の config には `[advisor]`
    /// セクションが無いため常に `None`（`ProjectConfig.bearer_token` と同じく、
    /// serde は `Option<T>` フィールドを欠落時 `None` として自動的に扱うため
    /// `#[serde(default)]` は不要）。design doc `2026-08-17-homesec-advisor-design.md` §10。
    pub advisor: Option<AdvisorConfig>,
}

/// homesec advisor 固有設定（design doc §4.3 手順4、§5.2、§7.2）。
#[derive(Debug, Clone, Deserialize)]
pub struct AdvisorConfig {
    /// URTECT 製品の個別サポート相談を検知したときの案内定型文（design doc §4.3 手順4）。
    pub handoff_contact_text: String,
    /// 製品カード画像の同梱ディレクトリ（`/static/products/` で配信、design doc §5.2）。
    pub images_dir: String,
    /// advisor 固有 NG 辞書のパス（design doc §7.1）。
    pub ng_dictionary_path: String,
    /// CS サポートモード（design doc §13）が既存 CS パイプラインを実行する先の vegapunk schema
    /// （例: `"urtect"`）。`crate::advisor::cs_support::run_support_turn` へそのまま渡す。
    pub support_schema: String,
    /// 上記 schema の manual 取得経路種別。`#[serde(default)]` は `ManualSchemaKind::default()`
    /// （`LegacySection`）に倒れるが、CS（urtect）は `manual_v1` を使うため config では明示設定する
    /// 運用にする。
    #[serde(default)]
    pub support_manual_schema: ManualSchemaKind,
    /// CS サポートモード専用の WORM 監査ログパス（Issue #50 レビュー指摘1）。
    ///
    /// `bin/homesec_advisor.rs` は `[harness]` を共有する 2 つの `Harness`（advisor 本体用、
    /// CS サポートモード用）を同一プロセスで構築する。`harness::audit::WormAuditLog` の
    /// hash chain はプロセスメモリ上の `Mutex<(File, String)>` の `prev_hash` にしか依存せず、
    /// 同一ファイルへ 2 つの独立した `Mutex` から交互に追記すると chain が破損し、次回起動時に
    /// `WormAuditLog::open` の整合性検証（fail closed）でプロセス起動自体が失敗する。
    /// このフィールドを必須（デフォルト無し）にしているのは、`[harness].audit_log_path` と
    /// 同じ値を書いてしまう事故を config の記述時点で防ぐため（暗黙のデフォルト値に頼ると、
    /// 気づかないまま両方が同じパスを指し続ける）。
    pub support_audit_log_path: String,
    /// CS サポートモード専用の検索改善キューパス。上記と同じ理由で分離する
    /// （こちらは hash chain を持たないため衝突しても即座には壊れないが、監査系統を
    /// 混ぜないという運用方針は audit_log_path と揃える）。
    pub support_search_improvement_queue_path: String,
    /// CS サポートモード専用の signal lexicon パス（Issue #50 バッチ2 レビュー指摘）。
    ///
    /// `bin/homesec_advisor.rs` は `[harness]` を共有する 2 つの `Harness`（advisor 本体用、
    /// CS サポートモード用）を同一プロセスで構築する。`Harness::admit_known_resolution`
    /// （admin 画面の KR 登録経路）は `self.lexicon.class_of()` で `[harness].signal_lexicon_path`
    /// を参照するため、`[harness]` をそのまま support 用（urtect 向け）辞書で上書きすると、
    /// advisor 本体の KR 登録まで urtect 向け語彙で検査されてしまう。このフィールドを必須
    /// （デフォルト無し）にしているのは、`[harness].signal_lexicon_path` と同じ値を書いて
    /// しまう事故を config の記述時点で防ぐため（暗黙のデフォルト値に頼ると、気づかないまま
    /// 両方が同じパスを指し続ける）。`support_audit_log_path` と同じ理由・同じパターン。
    pub support_signal_lexicon_path: String,
    /// CS サポートモード専用の NG 辞書パス。上記と同じ理由で分離する
    /// （`Harness::admit_known_resolution` の `egress_gate(..., &self.ng)` が使う）。
    pub support_ng_dictionary_path: String,
}

fn default_admin_static_dir() -> String {
    "admin-ui/browser".to_string()
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
            // Issue #28 C2(a) レビュー指摘に由来する値（600）。Issue #52 で signals 抽出
            // （`classify_signals`）と製品参照抽出（`extract_product_references`）を別々の
            // LLM 呼び出しに分離したため、signals 呼び出し自体はもう catalog（取扱一覧）を
            // 積まない。ただし両呼び出しはこの同じ [llm] max_tokens を共有しており、
            // catalog を注入して product_references の出力を要求するのは製品参照抽出側に
            // 移っただけである。300 のままだと、この製品参照抽出呼び出しが
            // stop_reason=max_tokens で切り詰められやすくなる（切り詰められると parse に
            // 失敗し、`ProductReferenceExtractor` は空配列へ degrade する。signals 抽出側への
            // 波及は無い）。本番 config.cloudrun.toml と同じ 600 を既定値にする（値そのものは
            // 変更しない。`extract_product_references` も同じ上限を使うため 600 の余裕は
            // 引き続き必要）。
            max_tokens: 600,
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
    /// 返信文下書きの `max_tokens`。signal 抽出用（`[llm] max_tokens`、既定 600）とは別枠。
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
    fn admin_static_dir_defaults_when_absent() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.admin_static_dir, "admin-ui/browser");
    }

    #[test]
    fn admin_static_dir_can_be_overridden() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
admin_static_dir = "../admin-ui/dist/admin-ui/browser"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.admin_static_dir, "../admin-ui/dist/admin-ui/browser");
    }

    #[test]
    fn advisor_config_defaults_to_none_when_section_is_absent() {
        // CS 用 config には [advisor] セクションが無い。既存 CS の config に advisor が
        // 影響しないことの固定テスト（design doc `2026-08-17-homesec-advisor-design.md` §10、
        // plan `2026-08-17-homesec-advisor.md` Task 1）。
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert!(cfg.advisor.is_none());
    }

    #[test]
    fn advisor_config_parses_section() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
[advisor]
handoff_contact_text = "URTECT製品の操作や不具合は、URTECT公式LINEアカウントで詳しくサポートしています。"
images_dir = "data/homesec/images"
ng_dictionary_path = "data/homesec/ng.json"
support_schema = "urtect"
support_manual_schema = "manual_v1"
support_audit_log_path = "/data/audit/audit-support.jsonl"
support_search_improvement_queue_path = "/data/audit/search-improvement-queue-support.jsonl"
support_signal_lexicon_path = "data/urtect/signal-lexicon.json"
support_ng_dictionary_path = "data/urtect/ng-dictionary.json"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        let advisor = cfg.advisor.expect("[advisor] section must parse to Some");
        assert_eq!(
            advisor.handoff_contact_text,
            "URTECT製品の操作や不具合は、URTECT公式LINEアカウントで詳しくサポートしています。"
        );
        assert_eq!(advisor.images_dir, "data/homesec/images");
        assert_eq!(advisor.ng_dictionary_path, "data/homesec/ng.json");
        assert_eq!(advisor.support_schema, "urtect");
        assert!(matches!(
            advisor.support_manual_schema,
            ManualSchemaKind::ManualV1
        ));
        assert_eq!(
            advisor.support_audit_log_path,
            "/data/audit/audit-support.jsonl"
        );
        assert_eq!(
            advisor.support_search_improvement_queue_path,
            "/data/audit/search-improvement-queue-support.jsonl"
        );
        assert_eq!(
            advisor.support_signal_lexicon_path,
            "data/urtect/signal-lexicon.json"
        );
        assert_eq!(
            advisor.support_ng_dictionary_path,
            "data/urtect/ng-dictionary.json"
        );
    }

    #[test]
    fn advisor_config_support_manual_schema_defaults_when_absent() {
        // Issue #50 バッチ1: `support_manual_schema` は加算フィールドで、欠落時は
        // `ManualSchemaKind::default()`（`LegacySection`）に倒れる（後方互換）。
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
[advisor]
handoff_contact_text = "案内文"
images_dir = "data/homesec/images"
ng_dictionary_path = "data/homesec/ng.json"
support_schema = "urtect"
support_audit_log_path = "/data/audit/audit-support.jsonl"
support_search_improvement_queue_path = "/data/audit/search-improvement-queue-support.jsonl"
support_signal_lexicon_path = "data/urtect/signal-lexicon.json"
support_ng_dictionary_path = "data/urtect/ng-dictionary.json"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        let advisor = cfg.advisor.expect("[advisor] section must parse to Some");
        assert!(matches!(
            advisor.support_manual_schema,
            ManualSchemaKind::LegacySection
        ));
    }

    #[test]
    fn config_homesec_toml_loads_and_declares_advisor_section() {
        // 実ファイルを読む統合テスト（plan `2026-08-17-homesec-advisor.md` Task 1 Step 5、
        // Issue #50 バッチ1で support_schema / support_manual_schema を追加検証）。
        // CI・本番デプロイが実際に読む config.homesec.toml がパース可能で、[advisor] の
        // キーが期待どおりに読めることを固定する。
        let path =
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config.homesec.toml"));
        let cfg = AppConfig::load(path).expect("config.homesec.toml must parse");
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].project_id, "homesec");
        assert_eq!(cfg.projects[0].schema, "homesec");
        assert!(matches!(
            cfg.projects[0].manual_schema,
            ManualSchemaKind::ManualV1
        ));
        let advisor = cfg
            .advisor
            .expect("config.homesec.toml must declare [advisor]");
        assert_eq!(
            advisor.handoff_contact_text,
            "URTECT製品の操作や不具合は、URTECT公式LINEアカウントで詳しくサポートしています。"
        );
        assert_eq!(advisor.images_dir, "data/homesec/images");
        assert_eq!(advisor.ng_dictionary_path, "data/homesec/ng.json");
        assert_eq!(advisor.support_schema, "urtect");
        assert!(matches!(
            advisor.support_manual_schema,
            ManualSchemaKind::ManualV1
        ));
        // Issue #50 レビュー指摘1: support_harness 用の監査パスは [harness] のそれと
        // 必ず異なる（同一プロセス内 2 Harness の hash chain 破損を防ぐ、
        // `bin/homesec_advisor.rs::build_support_config` が実際に使う値）。
        assert_ne!(advisor.support_audit_log_path, cfg.harness.audit_log_path);
        assert_ne!(
            advisor.support_search_improvement_queue_path,
            cfg.harness.search_improvement_queue_path
        );
        assert_eq!(
            advisor.support_audit_log_path,
            "/data/audit/audit-support.jsonl"
        );
        assert_eq!(
            advisor.support_search_improvement_queue_path,
            "/data/audit/search-improvement-queue-support.jsonl"
        );
        // Issue #50 バッチ2 レビュー指摘: support_harness 用の signal_lexicon_path /
        // ng_dictionary_path も [harness]（advisor 本体用）のそれと必ず異なる（`[harness]` を
        // urtect 用辞書で上書きしてしまうと、advisor 本体の Harness::admit_known_resolution
        // （admin 画面の KR 登録経路）まで urtect 向け語彙・NG 辞書で検査されてしまう）。
        assert_ne!(
            advisor.support_signal_lexicon_path,
            cfg.harness.signal_lexicon_path
        );
        assert_ne!(
            advisor.support_ng_dictionary_path,
            cfg.harness.ng_dictionary_path
        );
        assert_eq!(
            advisor.support_signal_lexicon_path,
            "data/urtect/signal-lexicon.json"
        );
        assert_eq!(
            advisor.support_ng_dictionary_path,
            "data/urtect/ng-dictionary.json"
        );
        assert!(cfg.api.enabled);
        assert!(cfg.llm.enabled);
        // [[projects]] は homesec 1 件のまま（admin alias が project_count == 1 を前提にする
        // ため、Issue #34 の制約を崩さない）。
        assert_eq!(cfg.projects[0].project_id, "homesec");
    }

    /// Issue #50 バッチ2 レビュー指摘: `support_signal_lexicon_path` / `support_ng_dictionary_path`
    /// は `support_audit_log_path` と同じ理由（config 記述時点で `[harness]` の値を書き写す
    /// 事故を防ぐ）で必須（デフォルト無し）にした。欠落時に TOML パース自体が失敗することを
    /// 固定する（`advisor_config_parses_section` の正常系に対する異常系）。
    #[test]
    fn advisor_config_missing_support_lexicon_paths_fails_to_parse() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
[advisor]
handoff_contact_text = "案内文"
images_dir = "data/homesec/images"
ng_dictionary_path = "data/homesec/ng.json"
support_schema = "urtect"
support_manual_schema = "manual_v1"
support_audit_log_path = "/data/audit/audit-support.jsonl"
support_search_improvement_queue_path = "/data/audit/search-improvement-queue-support.jsonl"
"#;
        let result: std::result::Result<AppConfig, _> = toml::from_str(toml);
        let err = result.expect_err(
            "support_signal_lexicon_path / support_ng_dictionary_path 欠落時は \
             パース失敗するはず",
        );
        let message = err.to_string();
        assert!(
            message.contains("support_signal_lexicon_path") || message.contains("missing field"),
            "unexpected parse error: {message}"
        );
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
