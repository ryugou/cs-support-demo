use anyhow::{anyhow, Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use cs_support_mcp::{
    config::AppConfig, harness::Harness, mcp::ToolService, rmcp_server::CsSupportRmcpServer,
    vegapunk::VegapunkClient,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{transport::io::stdio, ServiceExt};
use std::{
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tower_http::trace::TraceLayer;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN")]
    vegapunk_bearer_token: Option<String>,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN_FILE")]
    vegapunk_bearer_token_file: Option<PathBuf>,
    #[arg(long)]
    stdio: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let config = AppConfig::load(&args.config)?;
    let bearer_token = read_bearer_token(&args)?;
    let vegapunk = VegapunkClient::connect_lazy_with_limits(
        &config.vegapunk_endpoint,
        &bearer_token,
        config.grpc_limits(),
    )
    .context("configure vegapunk client")?;
    let tools = ToolService::new(vegapunk.clone());
    let config_dir = args
        .config
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let harness = Arc::new(
        Harness::build(&config, Arc::new(vegapunk), &config_dir).context("build harness")?,
    );

    if args.stdio {
        let project = config
            .projects
            .first()
            .ok_or_else(|| anyhow!("no project configured"))?;
        let service = CsSupportRmcpServer::new(
            project.schema.clone(),
            tools,
            harness.clone(),
            project.manual_schema,
        )
        .serve(stdio())
        .await?;
        service.waiting().await?;
        return Ok(());
    }

    let bind_addr: SocketAddr = config.bind_addr.parse()?;

    // health ルートは /healthz と /livez を張る。`*.run.app` エッジは /healthz を
    // 予約横取りするため、Cloud Run 上での到達可能なヘルスパスは /livez を使う
    // （詳細は cs_support_mcp::health を参照）。
    //
    // OAuth 保護リソースメタデータ（RFC 9728）はここで無認証公開する。
    // ただし `public_host` 自体は必須 env とし、未設定/空文字なら起動を止める
    // （fail-closed）。ここが空のまま起動を許すと、metadata の `resource` が
    // `https://` のみの不正 URL になり、かつ下の `AuthState.resource_metadata_url` も
    // 同じく壊れた URL になって Claude 側の OAuth 発見フローがサイレントに破綻する
    // （401 は返るが WWW-Authenticate が指す先が無意味になる）。client_id と同様、
    // 設定不備は起動失敗として運用者に即座に知らせる。
    //
    // 2026-07 改訂 / reviewer 指摘 W2 に伴う対称化: 末尾改行付きホスト名
    // （Secret Manager / YAML / コピペ由来）を弾かないまま許すと、
    // `https://example.com\n/.well-known/...` のような壊れた metadata URL
    // が組み立てられ、`HeaderValue::from_str`（middleware.rs）が失敗して
    // 診断ログはあるが 401 の WWW-Authenticate が機能しない状態に落ちる。
    // `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` と同じ `require_nonempty_env` に
    // 揃えることで、trim 済みの値だけが下流に渡ることを保証する
    // （エラーメッセージの文言はこれに伴い変わる。理由は上記の通り、
    // 空白混入で metadata URL が壊れる経路を塞ぐ価値が、旧文言の検索性より
    // 優先すると判断したため）。
    let public_host = require_nonempty_env(
        "CS_SUPPORT_PUBLIC_DOMAIN",
        env::var("CS_SUPPORT_PUBLIC_DOMAIN").ok(),
        "set it to the public hostname this service is served from (e.g. the Cloud Run service URL host) before starting the server",
    )?;
    let project_ids: Vec<String> = config
        .projects
        .iter()
        .map(|p| p.project_id.clone())
        .collect();
    let mut app = cs_support_mcp::health::health_router()
        .merge(cs_support_mcp::oauth::metadata::metadata_router(
            public_host.clone(),
            project_ids,
        ))
        .layer(TraceLayer::new_for_http());

    // `/{project_id}/mcp` は Google OAuth ミドルウェアで包む（RFC 9728 の 401 発見トリガを
    // 成立させるため）。verifier は project 間で共有できる（Google 検証は project 非依存）ので
    // 1 個作って Arc で配る。client_id 未設定は「MCP が丸ごと無認証で公開される」という
    // 重大な設定不備になるため、起動時に fail-closed で落とす。
    // `env::var` は「変数が設定されているが空文字」を `Ok(String::new())` として返すため、
    // `.context(...)` だけでは空文字がそのまま通過してしまう（fail-closed の抜け穴）。
    // `require_nonempty_env` で未設定・空文字の両方を同じ扱いで拒否する。
    let google_client_id = require_nonempty_env(
        "CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID",
        env::var("CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID").ok(),
        "set it to the Google OAuth Client ID from Google Cloud Console before starting the server",
    )?;
    let verifier = Arc::new(cs_support_mcp::oauth::verifier::GoogleTokenVerifier::new(
        google_client_id,
    ));

    for project in config.projects.iter() {
        let schema = project.schema.clone();
        let tools = tools.clone();
        let harness = harness.clone();
        let manual_schema = project.manual_schema;
        let mcp = StreamableHttpService::new(
            move || {
                Ok(CsSupportRmcpServer::new(
                    schema.clone(),
                    tools.clone(),
                    harness.clone(),
                    manual_schema,
                ))
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts()),
        );
        let auth_state = cs_support_mcp::oauth::middleware::AuthState {
            verifier: verifier.clone(),
            resource_metadata_url: format!(
                "https://{public_host}/.well-known/oauth-protected-resource/{}/mcp",
                project.project_id
            ),
        };
        let guarded =
            axum::Router::new()
                .fallback_service(mcp)
                .layer(axum::middleware::from_fn_with_state(
                    auth_state,
                    cs_support_mcp::oauth::middleware::require_google_auth,
                ));
        let path = format!("/{}/mcp", project.project_id);
        app = app.nest_service(&path, guarded);
    }

    if let (Some(cert), Some(key)) = (&config.tls_cert_path, &config.tls_key_path) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let tls_config = RustlsConfig::from_pem_file(cert, key)
            .await
            .with_context(|| format!("load TLS cert={cert} key={key}"))?;
        tracing::info!(%bind_addr, cert, key, "starting cs-support-mcp over HTTPS");
        axum_server::bind_rustls(bind_addr, tls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(bind_addr).await?;
        tracing::info!(%bind_addr, "starting cs-support-mcp over HTTP");
        axum::serve(listener, app).await?;
    }
    Ok(())
}

fn allowed_hosts() -> Vec<String> {
    let mut hosts = vec![
        "localhost".to_string(),
        "localhost:3000".to_string(),
        "localhost:3443".to_string(),
        "127.0.0.1".to_string(),
        "127.0.0.1:3000".to_string(),
        "127.0.0.1:3443".to_string(),
        "::1".to_string(),
        "cs-support-mcp".to_string(),
        "cs-support-mcp:8080".to_string(),
    ];
    for key in ["CS_SUPPORT_PUBLIC_DOMAIN", "MCP_ALLOWED_HOSTS"] {
        if let Ok(value) = env::var(key) {
            hosts.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|host| !host.is_empty())
                    .map(ToString::to_string),
            );
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

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

/// 起動時必須 env の「取得済みの値」を検証する純粋関数。
///
/// `env::var(name)` は「変数が設定されているが空文字」の場合 `Ok(String::new())`
/// を返す。これをそのまま `.context(...)` に通すと空文字がそのまま素通りしてしまう
/// （`CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` が空文字のまま起動できてしまっていた不具合）。
/// ここで未設定（`None`）と空文字（`Some("")`）を同じ扱いで fail closed にする。
///
/// `env::var` の呼び出しそのものはテスト対象に含めない。実プロセスの環境変数を
/// 書き換えるテストは並列テスト実行で不安定になるため、この関数は「呼び出し元が
/// 既に取得した `Option<String>`」だけを受け取り、環境変数へは一切触れない。
///
/// `guidance` には運用者が次に何をすべきか（設定すべき値の出どころ）を書く。
/// `config.rs` の `read_secret_file`（ラベル付きで空文字を拒否する先例）と同種の
/// パターンを env var 向けに切り出したもの。
///
/// 2026-07 改訂 / reviewer 指摘 W2:
/// 旧実装は `.filter(|s| !s.is_empty())` のみで、空白のみの値（`"   "`）や
/// 前後に空白・改行が付いた値（Secret Manager / YAML / コピペ由来。例:
/// `"123.apps.googleusercontent.com\n"`）をそのまま通過させていた。
/// 後者は tokeninfo の aud 完全一致判定（`verifier.rs`）が常に不一致になり、
/// しかも既存の診断ログ分類（`aud_mismatch_with_matching_azp` / `azp_mismatch`）
/// のどちらにも該当しない経路で 401 になるため、運用者が「client_id に
/// 空白が混入していたこと」に気づけない。ここで trim した上で空文字を弾き、
/// 以降は trim 済みの値だけが下流（tokeninfo への aud 送信、metadata URL 組み立て）
/// に渡るようにする（`read_secret_file` が trim 後に空文字を弾くのと同じ方針）。
fn require_nonempty_env(name: &str, value: Option<String>, guidance: &str) -> Result<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{name} is required and must not be empty; {guidance}"))
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
        assert!(
            msg.contains("set it before starting the server"),
            "message was: {msg}"
        );
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
    fn require_nonempty_env_accepts_nonempty_value() {
        let got = require_nonempty_env(
            "EXAMPLE_VAR",
            Some("abc".to_string()),
            "set it before starting the server",
        )
        .unwrap();
        assert_eq!(got, "abc");
    }

    /// W2（reviewer 指摘）: 空白のみの値（例: Secret Manager に誤って " " だけが
    /// 登録された場合）は「未設定」と同様に拒否する。素通りすると tokeninfo の
    /// aud 完全一致が常に失敗し、しかも既存の診断ログ分類（aud_mismatch_with_matching_azp
    /// / azp_mismatch）のどちらにも該当しない経路で落ちるため、C2 が解決した
    /// はずの「原因不明の全滅」が空白混入で再現してしまう。
    #[test]
    fn require_nonempty_env_rejects_whitespace_only_value() {
        let err = require_nonempty_env(
            "EXAMPLE_VAR",
            Some("   ".to_string()),
            "set it before starting the server",
        )
        .unwrap_err();
        assert!(err.to_string().contains("EXAMPLE_VAR"));
    }

    /// W2（reviewer 指摘）: Secret Manager / YAML / コピペ由来の前後空白・
    /// 改行付きの値は、前後を trim した上で採用する（`config.rs` の
    /// `read_secret_file` が trim 後に空文字を弾く先例と揃える）。
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

    /// 回帰テスト: `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` が空文字のまま起動されたとき、
    /// 運用者がログだけで「何が」「なぜ」「次に何をすべきか」を判断できる文言に
    /// なっていることを固定する。
    #[test]
    fn require_nonempty_env_error_message_for_google_client_id_names_the_fix() {
        let err = require_nonempty_env(
            "CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID",
            Some(String::new()),
            "set it to the Google OAuth Client ID from Google Cloud Console before starting the server",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID is required and must not be empty; \
             set it to the Google OAuth Client ID from Google Cloud Console before starting the server"
        );
    }
}
