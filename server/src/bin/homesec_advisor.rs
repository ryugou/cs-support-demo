// homesec advisor サービス本体。
//
// design doc `docs/superpowers/specs/2026-08-17-homesec-advisor-design.md` §3・§10、
// plan `docs/superpowers/plans/2026-08-17-homesec-advisor.md` Task 6 の実装。
//
// `server/src/main.rs`(CS/urtect 本体)の起動配線パターンをそのまま踏襲するが、advisor は
// MCP endpoint・OAuth 認可サーバ・署名鍵を一切持たない(design doc §9 不変条件2)。管理画面の
// 認証は GIS(ブラウザで Google token 取得)→ 既存 `require_google_auth` の Bearer 検証のみで
// 完結する。

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use cs_support_mcp::{
    admin::{admin_router, mount_admin_api, AdminState},
    advisor::api::{advisor_api_router, AdvisorApiState},
    config::AppConfig,
    harness::{egress::NgDictionary, Harness},
    llm::AnthropicClient,
    oauth::{middleware::AuthState, verifier::GoogleTokenVerifier},
    staticui,
    vegapunk::VegapunkClient,
};
use std::{
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN")]
    vegapunk_bearer_token: Option<String>,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN_FILE")]
    vegapunk_bearer_token_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let config = Arc::new(AppConfig::load(&args.config)?);

    // project 0 件の fail-closed 起動検査(main.rs 由来、codex レビュー指摘)。
    //
    // `mount_admin_api` はエイリアス mount 条件を `project_count == 1`、警告を `> 1` でしか
    // 判定しておらず、0 件はどちらにも該当しない。0 件のまま起動すると admin API
    // (`/{project_id}/admin/api` およびそのエイリアス)は一切登録されない一方、`/admin` の SPA
    // フォールバックだけは無条件に mount されるため、設定不備が `200 text/html` として
    // 隠蔽され、運用者はログを見ない限り気づけない。
    if config.projects.is_empty() {
        anyhow::bail!(
            "config has no [[projects]] entries; add at least one [[projects]] section \
             (project_id, schema, manual_schema) to {} before starting the server",
            args.config.display()
        );
    }

    // fail-closed 起動検査(main.rs 54-84行と同じ規律)。
    //
    // homesec_advisor は /{project_id}/api/reply 専用のバイナリ(design doc §3.1)。
    // [api] enabled = false のまま起動すると、プロセスは正常に立ち上がり /healthz /livez も
    // 200 を返すが、肝心の /api/reply ルート自体が登録されず、LINE からの全メッセージが
    // 404 になる。しかも起動ログに警告が出ないため、この状態に気づく手段が無い
    // (codex レビュー2巡目 Warning)。この検査を通った後は `config.api.enabled` は常に
    // true であることが保証されるため、以降の起動検査・配線からは `config.api.enabled` の
    // 条件分岐そのものを取り除く。
    if !config.api.enabled {
        anyhow::bail!(
            "[api] enabled = false but homesec_advisor exists only to serve \
             /{{project_id}}/api/reply (design doc §3.1); set [api] enabled = true in the \
             config file, or run a different binary if you only need the admin/static routes"
        );
    }

    // F4（main.rs 由来）: trim してから空判定する。`openssl rand -base64 32 |
    // gcloud secrets create --data-file=-` は値の末尾に改行を残すため、trim しないと
    // 定数時間比較（`api::authorize`）が必ず不一致になり、全メッセージ 401 で気づきにくい
    // 障害になる。
    let answer_api_key = env::var("CS_SUPPORT_ANSWER_API_KEY")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if answer_api_key.is_none() {
        anyhow::bail!(
            "[api] enabled = true but CS_SUPPORT_ANSWER_API_KEY is not set (or empty); \
             set it (Secret Manager injection) before starting the server"
        );
    }
    // `[api] fallback_reply_text` は検査しない: advisor の fallback 文は
    // `advisor::canned::FALLBACK_TEXT` 固定で、この config 値を一切読まないため
    // (CS 本体 `main.rs` はこの値を使うので、あちらでは検査が必要)。既定値は
    // 「お問い合わせありがとうございます…」という企業CS定型句で、design doc §2.1 の
    // ペルソナ規則が禁止する文面そのものでもある(codex レビュー2巡目 Warning)。
    validate_business_hours_config(&config.api.business_hours)?;
    let advisor_cfg = config
        .advisor
        .clone()
        .ok_or_else(|| anyhow::anyhow!("[advisor] section is required for homesec_advisor"))?;

    let bearer_token = read_bearer_token(&args)?;
    let vegapunk = VegapunkClient::connect_lazy_with_limits(
        &config.vegapunk_endpoint,
        &bearer_token,
        config.grpc_limits(),
    )
    .context("configure vegapunk client")?;
    let config_dir = args
        .config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let harness = Arc::new(
        Harness::build(&config, Arc::new(vegapunk.clone()), &config_dir)
            .context("build harness")?,
    );

    // `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET` と署名鍵は要求しない(advisor は OAuth AS を
    // 持たない。管理画面のトークン検証は Google tokeninfo 照会のみで完結する)。
    let public_host = require_nonempty_env(
        "CS_SUPPORT_PUBLIC_DOMAIN",
        env::var("CS_SUPPORT_PUBLIC_DOMAIN").ok(),
        "set it to the public hostname this service is served from (e.g. the Cloud Run service URL host) before starting the server",
    )?;
    validate_public_host(&public_host)?;
    let google_client_id = require_nonempty_env(
        "CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID",
        env::var("CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID").ok(),
        "set it to the Google OAuth Client ID from Google Cloud Console before starting the server",
    )?;
    let verifier = Arc::new(GoogleTokenVerifier::new(google_client_id));

    // advisor 専用の LLM クライアント(`Harness.reply_drafter` は homesec では常に `None`。
    // config.homesec.toml が `customer_reply_draft_enabled` を立てていないため)。
    let llm = AnthropicClient::from_config(&config.llm)
        .context("configure llm client")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[llm] enabled = true is required for homesec_advisor (understand/draft calls \
                 need a concrete AnthropicClient)"
            )
        })?;

    // advisor 専用の NG 辞書(`Harness.ng` は CS 用の語彙のため使わない)。相対パスは
    // config_dir 基準(`staticui::resolve_admin_static_dir` と同じ規約を流用する)。
    let ng_dictionary_path =
        staticui::resolve_admin_static_dir(&config_dir, &advisor_cfg.ng_dictionary_path);
    let ng = Arc::new(
        NgDictionary::from_path(&ng_dictionary_path).with_context(|| {
            format!(
                "load advisor ng dictionary {}",
                ng_dictionary_path.display()
            )
        })?,
    );

    // 製品カード画像ディレクトリ(`cards::select_cards` の存在確認と `/static/products`
    // 配信の両方が同じ絶対パスを参照する)。
    let images_dir = staticui::resolve_admin_static_dir(&config_dir, &advisor_cfg.images_dir);

    let bind_addr: SocketAddr = config.bind_addr.parse()?;

    let mut app = cs_support_mcp::health::health_router();

    // admin-ui（Issue #34）は `apiBase` をビルド時に `/admin/api`(project 非依存)で
    // ハードコードしている。project が 2 件以上だと、この 1 パスがどの project を指すか
    // 一意に決まらないため `mount_admin_api`（下のループ内）はエイリアスを mount しない。
    // homesec_advisor は OAuth AS を持たないため main.rs の C5 warn（RFC 8707 `resource`
    // 必須化）に相当するものは存在しない。ここで揃えるのは、あくまで main.rs の
    // admin/api エイリアス無効化 warn と同じ流儀（config を触った時点で運用者が気づけるよう
    // 起動時に 1 回警告する）だけである。
    if config.projects.len() > 1 {
        tracing::warn!(
            project_count = config.projects.len(),
            "more than one project is configured, so the /admin/api convenience alias (used by \
             the bundled admin dashboard's build-time-fixed apiBase) is disabled; requests to \
             /admin/api/* will NOT be rejected, they will silently fall through to the /admin \
             SPA fallback (200 text/html) and the admin UI will fail to parse it as JSON; there \
             is currently no working admin UI for a multi-project deployment — fixing this \
             requires either a per-project build with a project-scoped apiBase or a runtime \
             apiBase resolution mechanism, neither of which exists today (see the project-routing \
             notes in CLAUDE.md)"
        );
    }

    // `/{project_id}/admin/api`(GIS + require_google_auth で保護)。project は実質 1 件だが、
    // main.rs と同じ「複数 project を想定したループ」の形で書く。
    for project in config.projects.iter() {
        let auth_state = AuthState {
            verifier: verifier.clone(),
            // advisor は `.well-known/oauth-protected-resource` を公開しない(MCP endpoint を
            // 持たないため)。この値は 401 応答の `WWW-Authenticate: Bearer resource_metadata=…`
            // にしか使われず、admin-ui は GIS で直接サインインするためこの discovery
            // フローを踏まない。main.rs の MCP 用 URL 形をそのまま流用し、他の値を
            // 独自に発明しない(CS との一貫性を優先する判断。存在しないパスを指すが、
            // 機能上の実害は無い)。
            resource_metadata_url: format!(
                "https://{public_host}/.well-known/oauth-protected-resource/{}/mcp",
                project.project_id
            ),
        };
        let admin_state = AdminState {
            schema: project.schema.clone(),
            manual_schema: project.manual_schema,
            harness: harness.clone(),
        };
        let admin_guarded = admin_router(admin_state).layer(axum::middleware::from_fn_with_state(
            auth_state,
            cs_support_mcp::oauth::middleware::require_google_auth,
        ));
        app = mount_admin_api(
            app,
            &project.project_id,
            config.projects.len(),
            admin_guarded,
        );
    }

    // `/admin` の静的配信(認証なし。design doc: アプリシェルに秘密は含まれない)。project が
    // ちょうど 1 件のときは、上のループで project 非依存の `/admin/api` エイリアス
    // （`admin::mount_admin_api`、Issue #34）も mount 済みで、それはこの `/admin` の直下
    // （`/admin/api`）に重なる。ここで `/admin` を `nest_service` してもエイリアスが SPA
    // フォールバックに飲み込まれないのは、axum(matchit) が登録順によらずより具体的な
    // 静的セグメント（`/admin/api/...`）をワイルドカード（`/admin/...`）より優先するという
    // 実装依存の優先順位のおかげ。契約とテストは `admin::mount_admin_api` の doc コメントと
    // `admin::tests::mount_admin_api_tests` を参照。
    let admin_static_dir =
        staticui::resolve_admin_static_dir(&config_dir, &config.admin_static_dir);
    app = app.nest_service("/admin", staticui::admin_static_router(&admin_static_dir));

    // `/static/products`(製品カード画像。design doc §3.3: 認証不要)。
    let static_products = axum::Router::new().fallback_service(ServeDir::new(&images_dir));
    app = app.nest_service("/static/products", static_products);

    // `/{project_id}/api/reply`(design doc §3.3)。`config.api.enabled` は起動時検査
    // (冒頭の `if !config.api.enabled { bail! }`)で既に true であることが保証済みなので、
    // ここでは条件分岐しない。
    let api_key = answer_api_key.clone().expect(
        "invariant violated: CS_SUPPORT_ANSWER_API_KEY was not resolved at startup (the \
         fail-closed check above should have aborted first)",
    );
    let state = AdvisorApiState {
        config: config.clone(),
        harness: harness.clone(),
        llm,
        ng,
        vegapunk,
        api_key,
        handoff_contact_text: advisor_cfg.handoff_contact_text,
        images_dir,
        public_host,
    };
    app = app.merge(advisor_api_router(state));

    // TraceLayer は全ルート登録後に適用する(axum の Router::layer はその時点で登録済みの
    // ルートにしか掛からないため。先頭で適用すると health 以外の全経路 —— とりわけ
    // /{project_id}/api/reply(LLM・vegapunk・永続化を含む最重要経路)—— が HTTP トレース
    // 対象から外れる。codex レビュー2巡目 / reviewer 一次レビュー独立指摘)。
    //
    // 注意: `server/src/main.rs`(CS 本体)は同じ「先頭で layer」の形をしているが、CS の
    // 既存挙動を変えないため(design doc §9 不変条件1)ここでは main.rs を変更しない。
    // この修正は homesec_advisor.rs に閉じる。
    let app = app.layer(TraceLayer::new_for_http());

    if let (Some(cert), Some(key)) = (&config.tls_cert_path, &config.tls_key_path) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let tls_config = RustlsConfig::from_pem_file(cert, key)
            .await
            .with_context(|| format!("load TLS cert={cert} key={key}"))?;
        tracing::info!(%bind_addr, cert, key, "starting homesec_advisor over HTTPS");
        axum_server::bind_rustls(bind_addr, tls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(bind_addr).await?;
        tracing::info!(%bind_addr, "starting homesec_advisor over HTTP");
        axum::serve(listener, app).await?;
    }
    Ok(())
}

/// `server/src/main.rs::read_bearer_token` の複製(423行目付近)。
fn read_bearer_token(args: &Args) -> Result<String> {
    if let Some(token) = &args.vegapunk_bearer_token {
        return Ok(token.clone());
    }
    if let Some(path) = &args.vegapunk_bearer_token_file {
        return fs::read_to_string(path)
            .with_context(|| format!("read vegapunk bearer token file {}", path.display()))
            .map(|s| s.trim().to_string());
    }
    Ok(String::new())
}

/// `server/src/main.rs::require_nonempty_env` の複製(460行目付近)。
///
/// `env::var(name)` は「設定されているが空文字」の場合 `Ok(String::new())` を返すため、
/// ここで未設定(`None`)と空文字(`Some("")`)を同じ扱いで fail closed にする。
fn require_nonempty_env(name: &str, value: Option<String>, guidance: &str) -> Result<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{name} is required and must not be empty; {guidance}"))
}

/// `CS_SUPPORT_PUBLIC_DOMAIN` は「スキーム無しのホスト名(必要なら :port)」である契約。
/// `api.rs` がこの値を `format!("https://{host}{path}")` で製品カードの画像 URL に埋めるため、
/// スキーム付き・パス付き・空白混入の値を通すと、顧客の LINE カルーセルに壊れた URL が
/// 配信される(起動時には何も起きず、画像が出ないという形でしか気づけない。codex レビュー
/// 2巡目 Warning)。
fn validate_public_host(value: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!(
            "CS_SUPPORT_PUBLIC_DOMAIN must not be empty; set it to a scheme-less hostname \
             (e.g. \"example.com\" or \"127.0.0.1:3443\")"
        );
    }
    if value.contains("://") || value.starts_with("http:") || value.starts_with("https:") {
        anyhow::bail!(
            "CS_SUPPORT_PUBLIC_DOMAIN=\"{value}\" must be a scheme-less hostname, but contains \
             a URL scheme; api.rs builds product card image URLs as \
             https://{{CS_SUPPORT_PUBLIC_DOMAIN}}{{path}}, so a scheme here produces a broken \
             \"https://https://...\" URL delivered to the customer's LINE carousel; fix it to \
             just the hostname, e.g. \"example.com\""
        );
    }
    if value.contains('/') {
        anyhow::bail!(
            "CS_SUPPORT_PUBLIC_DOMAIN=\"{value}\" must be a scheme-less hostname, but contains \
             a path; fix it to just the hostname (and :port if needed), e.g. \"example.com\" \
             or \"127.0.0.1:3443\""
        );
    }
    if value.contains('?') || value.contains('#') {
        anyhow::bail!(
            "CS_SUPPORT_PUBLIC_DOMAIN=\"{value}\" must be a scheme-less hostname, but contains \
             a query string or fragment; fix it to just the hostname (and :port if needed), \
             e.g. \"example.com\""
        );
    }
    if value
        .chars()
        .any(|c| c.is_ascii_whitespace() || c.is_control())
    {
        anyhow::bail!(
            "CS_SUPPORT_PUBLIC_DOMAIN=\"{value}\" contains whitespace or control characters; \
             fix it to just the hostname (and :port if needed), e.g. \"example.com\""
        );
    }
    Ok(())
}

/// `server/src/main.rs::validate_business_hours_config` の複製(478行目付近)。
///
/// メッセージ文面は複製元と異なる: `homesec_advisor` は `main()` 冒頭の fail-closed
/// 検査で `[api] enabled = false` のとき起動時に bail するため、複製元が案内する
/// 「`[api] enabled = false` で回避する」は advisor では存在しない回避策になる。
/// ここでは config の値を直す具体的な直し方だけを案内する。
fn validate_business_hours_config(cfg: &cs_support_mcp::config::BusinessHoursConfig) -> Result<()> {
    if !matches!(cfg.days.as_str(), "mon-fri" | "everyday") {
        anyhow::bail!(
            "[api.business_hours] days \"{}\" is neither \"mon-fri\" nor \"everyday\"; fix the \
             config value to one of \"mon-fri\" or \"everyday\"",
            cfg.days
        );
    }
    cfg.tz.parse::<chrono_tz::Tz>().map_err(|_| {
        anyhow::anyhow!(
            "[api.business_hours] tz \"{}\" is not a valid IANA timezone name; fix the config \
             value to a valid IANA timezone name, e.g. \"Asia/Tokyo\"",
            cfg.tz
        )
    })?;
    chrono::NaiveTime::parse_from_str(&cfg.start, "%H:%M").map_err(|_| {
        anyhow::anyhow!(
            "[api.business_hours] start \"{}\" is not \"HH:MM\"; fix the config value to the \
             \"HH:MM\" format, e.g. \"10:00\"",
            cfg.start
        )
    })?;
    chrono::NaiveTime::parse_from_str(&cfg.end, "%H:%M").map_err(|_| {
        anyhow::anyhow!(
            "[api.business_hours] end \"{}\" is not \"HH:MM\"; fix the config value to the \
             \"HH:MM\" format, e.g. \"18:00\"",
            cfg.end
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_nonempty_env_rejects_missing_value() {
        let err = require_nonempty_env("EXAMPLE_VAR", None, "set it before starting the server")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("EXAMPLE_VAR"), "message was: {msg}");
        assert!(msg.contains("must not be empty"), "message was: {msg}");
    }

    #[test]
    fn require_nonempty_env_rejects_empty_string_value() {
        let err = require_nonempty_env(
            "EXAMPLE_VAR",
            Some(String::new()),
            "set it before starting the server",
        )
        .unwrap_err();
        assert!(err.to_string().contains("EXAMPLE_VAR"));
    }

    #[test]
    fn require_nonempty_env_trims_and_accepts_padded_value() {
        let got = require_nonempty_env(
            "EXAMPLE_VAR",
            Some("  abc  ".to_string()),
            "set it before starting the server",
        )
        .unwrap();
        assert_eq!(got, "abc");
    }

    // ---- validate_public_host (codex レビュー2巡目 Warning) ----

    #[test]
    fn validate_public_host_rejects_a_value_with_a_scheme() {
        let err = validate_public_host("https://example.com").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("scheme"), "message was: {msg}");
    }

    #[test]
    fn validate_public_host_rejects_a_value_with_a_path() {
        let err = validate_public_host("example.com/path").unwrap_err();
        assert!(err.to_string().contains("path"));
    }

    #[test]
    fn validate_public_host_accepts_a_bare_hostname() {
        assert!(validate_public_host("example.com").is_ok());
    }

    #[test]
    fn validate_public_host_accepts_a_hostname_with_a_port() {
        // ローカル開発の形(CLAUDE.md のローカル起動手順が使う)。
        assert!(validate_public_host("127.0.0.1:3443").is_ok());
    }

    #[test]
    fn validate_public_host_accepts_a_cloud_run_hostname() {
        assert!(
            validate_public_host("cs-support-mcp-235108918288.asia-northeast1.run.app").is_ok()
        );
    }

    #[test]
    fn validate_public_host_rejects_a_value_containing_whitespace() {
        let err = validate_public_host("example.com ").unwrap_err();
        assert!(err.to_string().contains("whitespace"));
    }

    fn valid_business_hours() -> cs_support_mcp::config::BusinessHoursConfig {
        cs_support_mcp::config::BusinessHoursConfig::default()
    }

    #[test]
    fn validate_business_hours_config_accepts_the_default_config() {
        assert!(validate_business_hours_config(&valid_business_hours()).is_ok());
    }

    #[test]
    fn validate_business_hours_config_rejects_invalid_days() {
        let mut cfg = valid_business_hours();
        cfg.days = "weekends-only".to_string();
        let err = validate_business_hours_config(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("days"),
            "error must name the offending field: {err}"
        );
    }

    #[test]
    fn validate_business_hours_config_rejects_invalid_tz() {
        let mut cfg = valid_business_hours();
        cfg.tz = "Not/A/Timezone".to_string();
        let err = validate_business_hours_config(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("tz"),
            "error must name the offending field: {err}"
        );
    }

    #[test]
    fn validate_business_hours_config_rejects_invalid_start() {
        let mut cfg = valid_business_hours();
        cfg.start = "not-a-time".to_string();
        let err = validate_business_hours_config(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("start"),
            "error must name the offending field: {err}"
        );
    }

    #[test]
    fn validate_business_hours_config_rejects_invalid_end() {
        let mut cfg = valid_business_hours();
        cfg.end = "25:99".to_string();
        let err = validate_business_hours_config(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("end"),
            "error must name the offending field: {err}"
        );
    }

    #[test]
    fn validate_business_hours_config_errors_do_not_suggest_disabling_the_api() {
        // homesec_advisor は [api] enabled = false のとき main() 冒頭で bail する。
        // この案内を残すと、運用者は「回避策」に従って次回起動で別のエラーに突き当たる
        // (CLAUDE.md: すべてのエラーパスに、運用者が次のアクションを判断できる情報を含める)。
        let cases = [
            {
                let mut cfg = valid_business_hours();
                cfg.days = "weekends-only".to_string();
                cfg
            },
            {
                let mut cfg = valid_business_hours();
                cfg.tz = "Not/A/Timezone".to_string();
                cfg
            },
            {
                let mut cfg = valid_business_hours();
                cfg.start = "not-a-time".to_string();
                cfg
            },
            {
                let mut cfg = valid_business_hours();
                cfg.end = "25:99".to_string();
                cfg
            },
        ];
        for cfg in cases {
            let err = validate_business_hours_config(&cfg).unwrap_err();
            let msg = err.to_string();
            assert!(
                !msg.contains("enabled = false"),
                "error must not suggest a nonexistent workaround (homesec_advisor bails at \
                 startup when [api] enabled = false): {msg}"
            );
        }
    }

    #[test]
    fn read_bearer_token_prefers_the_direct_arg_over_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "homesec-advisor-bearer-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("token-file");
        std::fs::write(&path, "file-token\n").expect("write token file");
        let args = Args {
            config: PathBuf::from("config.toml"),
            vegapunk_bearer_token: Some("direct-token".to_string()),
            vegapunk_bearer_token_file: Some(path),
        };
        assert_eq!(read_bearer_token(&args).unwrap(), "direct-token");
    }

    #[test]
    fn read_bearer_token_falls_back_to_the_file_and_trims_it() {
        let dir = std::env::temp_dir().join(format!(
            "homesec-advisor-bearer-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("token-file");
        std::fs::write(&path, "  file-token  \n").expect("write token file");
        let args = Args {
            config: PathBuf::from("config.toml"),
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: Some(path),
        };
        assert_eq!(read_bearer_token(&args).unwrap(), "file-token");
    }

    #[test]
    fn read_bearer_token_returns_empty_string_when_neither_is_set() {
        let args = Args {
            config: PathBuf::from("config.toml"),
            vegapunk_bearer_token: None,
            vegapunk_bearer_token_file: None,
        };
        assert_eq!(read_bearer_token(&args).unwrap(), "");
    }

    /// `[advisor]` セクション欠落は起動を fail closed で止める(この判定自体は `main()` の
    /// 冒頭にインラインで書いており、`Option::ok_or_else` の単純な組み合わせなので独立した
    /// 関数へは切り出していない。ここでは `AppConfig` の当該フィールドが実際に `None` に
    /// なる config で、`main()` が使うのと同じ判定式を再現して固定する)。
    #[test]
    fn advisor_section_missing_is_detected_as_none() {
        let toml = r#"
bind_addr = "127.0.0.1:3443"
vegapunk_endpoint = "http://x:6840"
[[projects]]
project_id = "p"
schema = "s"
"#;
        let cfg: cs_support_mcp::config::AppConfig = toml::from_str(toml).unwrap();
        let result: Result<cs_support_mcp::config::AdvisorConfig> = cfg
            .advisor
            .ok_or_else(|| anyhow::anyhow!("[advisor] section is required for homesec_advisor"));
        let err = result.unwrap_err();
        assert!(err.to_string().contains("[advisor] section is required"));
    }
}
