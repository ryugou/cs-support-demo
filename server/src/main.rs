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
    // `CS_SUPPORT_PUBLIC_DOMAIN` 未設定時は空文字のまま `resource` が
    // `https://` のみになり不正な URL を返すが、これは Task 3/5 で認証を有効化する際に
    // 必須 env として検証されるべき設定不足であり、ここでは fail-closed にしない
    // （メタデータ endpoint 自体は未認証で公開する仕様のため、起動を止める理由がない）。
    let public_host = env::var("CS_SUPPORT_PUBLIC_DOMAIN").unwrap_or_default();
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
    let google_client_id = env::var("CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID").context(
        "CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID is required for OAuth (Google tokeninfo aud check)",
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
