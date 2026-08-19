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
    // `Arc` にするのは `/api/reply`（api::ApiState.config）と `/{project_id}/mcp` ループの
    // 両方が同じ `AppConfig` を共有するため。以降の `config.xxx` 参照は `Arc<AppConfig>` の
    // `Deref` でそのまま動く（型を変えても呼び出し側の書き換えは不要）。
    let config = Arc::new(AppConfig::load(&args.config)?);
    // project 0 件の fail-closed 起動検査。`mount_admin_api` はエイリアス mount 条件を
    // `project_count == 1`、警告を `> 1` でしか判定しておらず、0 件はどちらにも該当しない
    // （codex レビュー指摘）。0 件のまま起動すると `/{project_id}/mcp` も admin API も
    // 一切登録されない一方、`/admin` の SPA フォールバックだけは無条件に mount されるため、
    // 設定不備が `200 text/html` として隠蔽され、運用者はログを見ない限り気づけない。
    if config.projects.is_empty() {
        anyhow::bail!(
            "config has no [[projects]] entries; add at least one [[projects]] section \
             (project_id, schema, manual_schema) to {} before starting the server",
            args.config.display()
        );
    }
    // 応答生成 API（/api/reply）の fail-closed 起動検査。有効化されているのに
    // env 未設定・空文字だと、認証チェックが実質無効な（誰も鍵を持てない＝誰も
    // 通らない、または将来の実装ミスで誰でも通る）ルートを公開してしまう。
    // `CS_SUPPORT_LLM_API_KEY`（LlmConfig）と同じ「設定不備は起動失敗」の方針。
    //
    // F4（reviewer 指摘）: trim してから空判定する。`openssl rand -base64 32 |
    // gcloud secrets create --data-file=-`（CLAUDE.md が推奨する secret 作成手順その
    // もの）は値の末尾に改行を残す。ここで trim しないと、サーバ側の鍵は
    // `"KEY\n"`、LINE アダプタ（`require_env` は既に trim 済み）が送るのは `"KEY"` になり、
    // `api::authorize` の定数時間比較が必ず不一致になる。結果は全メッセージ 401 →
    // フォールバック文の恒常化で、サービス自体は正常起動しているため気づきにくい。
    let answer_api_key = env::var("CS_SUPPORT_ANSWER_API_KEY")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if config.api.enabled && answer_api_key.is_none() {
        anyhow::bail!(
            "[api] enabled = true but CS_SUPPORT_ANSWER_API_KEY is not set (or empty); \
             set it (Secret Manager injection) before starting the server, or set \
             [api] enabled = false to disable the /api/reply route"
        );
    }
    // `fallback_reply_text` が空文字のまま起動すると、escalate / rule_match / 下書き失敗の
    // すべてで `reply_text: ""` が返る。呼び出し元（LINE アダプタ等)は空メッセージ送信に
    // 失敗し、顧客に何も返せないまま気づけない。起動時に弾く。
    // v1.1（Task 6, `api.rs` の会話フローオーケストレーション導入）でこの config は
    // `/api/reply` の応答経路からは外れた（各分岐が `escalation_reply.rs` / `clarify.rs` の
    // 定型文フォールバックを個別に持つ）。互換のためフィールドとこの起動時チェックは残置する。
    if config.api.enabled && config.api.fallback_reply_text.trim().is_empty() {
        anyhow::bail!(
            "[api] enabled = true but [api] fallback_reply_text is empty (or whitespace only); \
             set a non-empty fallback reply text, or set [api] enabled = false to disable \
             the /api/reply route"
        );
    }
    // 会話フロー v1.1: `business_hours` の tz/start/end は営業時間判定（hours.rs）と
    // エスカレーション応答の営業時間表記の両方で使われる。設定ミスがあると hours.rs は
    // fail-closed で「常に営業時間外」に倒すため、機能はするが誤案内が出続ける事故になる。
    // 起動時点で検知して落とす。
    if config.api.enabled {
        validate_business_hours_config(&config.api.business_hours)?;
    }
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

    // `/{project_id}/mcp` は認証ミドルウェアで包む（RFC 9728 の 401 発見トリガを
    // 成立させるため）。
    //
    // 2026-07 改訂 / AS 自前化と、その後の自前トークン発行の廃止:
    // このサービス自身が OAuth 2.1 認可サーバとして DCR と同意画面を引き受けるが、
    // **トークンは自前発行せず Google のものを中継する**
    // （理由は `oauth::authserver` のモジュールコメント）。したがって
    // - リクエスト経路の Bearer 検証は Google tokeninfo への照会
    // - Google の client_id / client_secret はサーバ側 env に閉じ込め、
    //   `/oauth/callback` と `/oauth/token` のトークン交換・中継にだけ使う
    //
    // 署名鍵は `CS_SUPPORT_OAUTH_SIGNING_KEY`（Secret Manager 注入）から読む。
    //
    // **かつては起動時に CSPRNG で生成していたが、それでは利用者が再ログインを強いられる。**
    // 根拠: `token_from_refresh` は毎回 `client_id`（この鍵で封緘した DCR 登録ブロブ）を
    // 署名検証しており、鍵が変わると `unverifiable_client_id` → `invalid_grant` を返す。
    // OAuth クライアントは `invalid_grant` を受けると仕様どおり refresh_token を破棄するため、
    // **デプロイのたび、かつゼロスケールからのコールドスタートのたびに接続が切れていた**
    // （`minScale` 未設定なのでアイドルで必ず起きる）。旧コメントの「再起動で利用者は
    // ログアウトしない」は Google のトークンだけを見た記述で、この経路を見落としていた。
    //
    // 未設定なら従来どおり生成して警告する（ローカル開発は Google の callback が
    // 通らずログインフロー自体を完走できないため、生成鍵で足りる）。
    //
    // 以下 2 つの env は未設定・空文字なら起動を止める（fail closed）。
    // `env::var` は「設定されているが空文字」を `Ok(String::new())` で返すため、
    // `.context(...)` だけでは空文字が素通りする。`require_nonempty_env` で
    // 未設定・空文字・空白のみを同じ扱いで拒否する。
    // - client_id 未設定: Google への authorize / aud 照合が成立しない
    // - client_secret 未設定: Google の token 交換が必ず失敗し、誰もログインできない
    let google_client_id = require_nonempty_env(
        "CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID",
        env::var("CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID").ok(),
        "set it to the Google OAuth Client ID from Google Cloud Console before starting the server",
    )?;
    let google_client_secret = require_nonempty_env(
        "CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET",
        env::var("CS_SUPPORT_GOOGLE_OAUTH_CLIENT_SECRET").ok(),
        "set it to the Google OAuth Client Secret from Google Cloud Console (inject it via Secret Manager; it never leaves this server) before starting the server",
    )?;
    let signing_key = Arc::new(resolve_signing_key(
        env::var("CS_SUPPORT_OAUTH_SIGNING_KEY").ok(),
    )?);
    let verifier = Arc::new(cs_support_mcp::oauth::verifier::GoogleTokenVerifier::new(
        google_client_id.clone(),
    ));

    // 認可サーバの endpoint 群（/oauth/authorize, /oauth/callback, /oauth/token,
    // /oauth/register）は **無認証** で公開する。ここに認証をかけると、
    // 認証を得るための経路そのものが閉じてしまう。
    // C5: project が 2 件以上あると、`resource` パラメータが **必須** になる
    // （`resolve_resource` は束縛先を推測せず `invalid_target` で拒否する）。
    // これは fail closed として正しい向きだが、無警告だと「project を足した瞬間に
    // resource を送らない既存クライアントが全滅する」という障害として現れる。
    // config を触った時点で運用者が気づけるよう、起動時に 1 回警告する。
    if config.projects.len() > 1 {
        tracing::warn!(
            project_count = config.projects.len(),
            "more than one project is configured, so the RFC 8707 `resource` parameter is now \
             REQUIRED on /oauth/authorize; clients that omit it will be rejected with \
             invalid_target (see the project-routing notes in CLAUDE.md)"
        );
    }

    // admin-ui（Issue #34）は `apiBase` をビルド時に `/admin/api`（project 非依存）で
    // ハードコードしている。project が 2 件以上だと、この 1 パスがどの project を指すか
    // 一意に決まらないため `mount_admin_api`（下の project ループ内）はエイリアスを
    // mount しない。この状態でも `/admin/api/...` へのリクエストは 401/404 にはならず、
    // `/admin` の SPA フォールバックへ落ちて `200 text/html` を返す（実測済み）。同梱の
    // admin-ui はビルド時固定の `apiBase` しか持たないため、運用者がこの警告を見た時点で
    // 選べる復旧手段は無い（project ごとに異なる `apiBase` を焼いた別ビルドを用意するか、
    // ランタイムで `apiBase` を解決する仕組みを実装する必要があるが、いずれも現状未実装）。
    // 上の C5 warn と同じ流儀で、config を触った時点で運用者が気づけるよう起動時に 1 回
    // 警告する。
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

    let auth_server = Arc::new(cs_support_mcp::oauth::authserver::AuthServerState::new(
        cs_support_mcp::oauth::authserver::AuthServerConfig::new(
            public_host.clone(),
            google_client_id,
            google_client_secret,
            config
                .projects
                .iter()
                .map(|p| p.project_id.clone())
                .collect(),
        ),
        signing_key,
        verifier.clone(),
    ));
    app = app.merge(cs_support_mcp::oauth::authserver::auth_server_router(
        auth_server,
    ));

    for project in config.projects.iter() {
        let schema = project.schema.clone();
        let tools = tools.clone();
        let harness = harness.clone();
        let manual_schema = project.manual_schema;

        let auth_state = cs_support_mcp::oauth::middleware::AuthState {
            verifier: verifier.clone(),
            resource_metadata_url: format!(
                "https://{public_host}/.well-known/oauth-protected-resource/{}/mcp",
                project.project_id
            ),
        };
        // `/{project_id}/admin/api`（2026-08-16 admin dashboard design doc §4）。MCP と同じ
        // Google 認証（`require_google_auth`）で保護する。`resource_metadata_url` は MCP 用の
        // ものを再利用してよい（design doc §4: OAuth 基盤の再利用であり、admin 専用の discovery
        // metadata は無い）。`auth_state` はここで clone してから MCP 側の
        // `from_fn_with_state` へ渡す（あちらは値を消費する）。`harness` も同様に、この後の
        // `mcp` クロージャ（`move ||`）が消費する**前**にここで clone しておく。
        let admin_state = cs_support_mcp::admin::AdminState {
            schema: schema.clone(),
            manual_schema,
            harness: harness.clone(),
        };
        let admin_guarded = cs_support_mcp::admin::admin_router(admin_state).layer(
            axum::middleware::from_fn_with_state(
                auth_state.clone(),
                cs_support_mcp::oauth::middleware::require_google_auth,
            ),
        );
        app = cs_support_mcp::admin::mount_admin_api(
            app,
            &project.project_id,
            config.projects.len(),
            admin_guarded,
        );

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

    // `/admin` の静的配信（admin dashboard design doc §5）。ビルド成果物（`index.html` を含む）を
    // Docker イメージへ同梱し `ServeDir` で配信する。`/admin` 配下の未知パスは SPA フォールバックとして
    // `index.html` を返す。**認証ミドルウェアは掛けない**（design doc: アプリシェルに秘密は含まれない）。
    // データは認証必須の `/{project_id}/admin/api` から取得する（project ループ内で
    // `require_google_auth` を掛けて登録済み）。project がちょうど 1 件のときは、加えて
    // project 非依存の `/admin/api` エイリアス（`admin::mount_admin_api`、Issue #34）も
    // project ループ内で mount 済みで、**そちらはこの `/admin` の直下（`/admin/api`）に重なる**
    // （project 固有パスとは違い、衝突しない別経路ではない）。ここで `/admin` を
    // `nest_service` してもエイリアスが SPA フォールバックに飲み込まれないのは、axum(matchit)
    // が登録順によらずより具体的な静的セグメント（`/admin/api/...`）をワイルドカード
    // （`/admin/...`）より優先するという実装依存の優先順位のおかげであり、この関数を編集する
    // ときはその前提を崩さないこと。契約は `admin::mount_admin_api` の doc コメントと
    // `admin::tests::mount_admin_api_tests`（登録順の両方向・実ルート解決・SPA 非侵食を実測）
    // を参照。
    let admin_static_dir =
        cs_support_mcp::staticui::resolve_admin_static_dir(&config_dir, &config.admin_static_dir);
    app = app.nest_service(
        "/admin",
        cs_support_mcp::staticui::admin_static_router(&admin_static_dir),
    );

    // `/{project_id}/api/reply`（design doc §3）。`require_google_auth` はここには適用しない
    // — 認証は `api::authorize`（固定 API キーの定数時間比較）が単独で担う。
    // `config.api.enabled = false`（既定）のときはルート自体を登録しない。
    if config.api.enabled {
        // 起動時 fail-closed チェック（本関数冒頭）が `enabled = true` のとき
        // `answer_api_key` を必ず `Some` にしているので、ここでの `None` は
        // 到達しない不変条件の破れであり、`expect` で早期に気づけるようにする。
        let api_key = answer_api_key.clone().expect(
            "invariant violated: [api] enabled = true but CS_SUPPORT_ANSWER_API_KEY was not \
             resolved at startup (the fail-closed check above should have aborted first)",
        );
        let api_state = cs_support_mcp::api::ApiState {
            config: config.clone(),
            harness: harness.clone(),
            tools: tools.clone(),
            api_key,
        };
        app = app.merge(cs_support_mcp::api::api_router(api_state));
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

/// `[api] business_hours`（会話フロー v1.1 design doc §6）を起動時に検証する純関数。
///
/// `hours.rs` は tz/start/end の parse 失敗を実行時に fail-closed で「営業時間外」に
/// 倒すため、設定ミスがあっても機能自体は止まらない。その代わり、運用者が気づかないまま
/// 「常に営業時間外」の誤案内（`escalation_reply.rs` の決定的ブロック）が出続ける事故になる。
/// `days` は tz/start/end と壊れ方が異なる点に注意: `hours::days_overlap` は未知値を
/// fail-closed（重ならない）にする一方、`hours::business_hours_label` は未知の `days` を
/// 黙って「平日」表示に倒す。つまり `days` の設定ミスを検証対象から外すと、「常に対応時間外」
/// の案内がもっともらしいラベル付きで出続けるという、tz/start/end とは別経路の事故が起きる。
/// `[api] enabled = true` のときだけ起動時点で検知して落とす（`CS_SUPPORT_LLM_API_KEY` /
/// `fallback_reply_text` と同じ「有効時のみ fail closed」の方針）。
fn validate_business_hours_config(cfg: &cs_support_mcp::config::BusinessHoursConfig) -> Result<()> {
    if !matches!(cfg.days.as_str(), "mon-fri" | "everyday") {
        anyhow::bail!(
            "[api.business_hours] days \"{}\" is neither \"mon-fri\" nor \"everyday\"; fix the \
             config value, or set [api] enabled = false to disable the /api/reply route",
            cfg.days
        );
    }
    cfg.tz.parse::<chrono_tz::Tz>().map_err(|_| {
        anyhow::anyhow!(
            "[api.business_hours] tz \"{}\" is not a valid IANA timezone name; fix the config \
             value, or set [api] enabled = false to disable the /api/reply route",
            cfg.tz
        )
    })?;
    chrono::NaiveTime::parse_from_str(&cfg.start, "%H:%M").map_err(|_| {
        anyhow::anyhow!(
            "[api.business_hours] start \"{}\" is not \"HH:MM\"; fix the config value, or set \
             [api] enabled = false to disable the /api/reply route",
            cfg.start
        )
    })?;
    chrono::NaiveTime::parse_from_str(&cfg.end, "%H:%M").map_err(|_| {
        anyhow::anyhow!(
            "[api.business_hours] end \"{}\" is not \"HH:MM\"; fix the config value, or set \
             [api] enabled = false to disable the /api/reply route",
            cfg.end
        )
    })?;
    Ok(())
}

/// OAuth 署名鍵を解決する純関数（env 値を引数で受けるのでテストできる）。
///
/// - 値がある → `SigningKey::from_secret`。**短すぎる材料は起動時に弾く**（fail closed）。
///   設定したつもりで脆い鍵を使い続ける事故を防ぐ
/// - 値が無い → 生成鍵にフォールバックし、**警告する**。この状態では再起動・コールド
///   スタートのたびに DCR 登録が無効になり、refresh が `invalid_grant` で落ちて利用者が
///   再ログインを強いられる。本番でこれに気づかないのが最悪なので黙って落とさない
///
/// **空文字・空白のみは「未設定」ではなく設定ミスとして扱い、生成鍵へ落とさずエラーにする。**
/// 上の `require_nonempty_env` は `None` と `Some("")` を同じ扱いにするが、**ここでその
/// イディオムに揃えてはいけない** — Secret Manager のマウント漏れ（空文字が入る）が
/// silent に生成鍵フォールバックへ落ち、warn だけ出して「動くが毎回ログアウトする」状態が
/// 再発する。この非対称は意図的であり、テストで固定してある。
fn resolve_signing_key(
    configured: Option<String>,
) -> Result<cs_support_mcp::oauth::signing::SigningKey> {
    use cs_support_mcp::oauth::signing::SigningKey;
    match configured {
        Some(secret) => SigningKey::from_secret(&secret).map_err(|e| {
            anyhow::anyhow!(
                "CS_SUPPORT_OAUTH_SIGNING_KEY is set but unusable: {e}. \
                 It gates the OAuth refresh path (client_id verification), so a bad value \
                 would log every user out on each restart"
            )
        }),
        None => {
            tracing::warn!(
                "CS_SUPPORT_OAUTH_SIGNING_KEY is not set; falling back to a per-process key. \
                 Client registrations (DCR) will not survive a restart or a cold start, so \
                 token refresh will fail with invalid_grant and users will be asked to \
                 reconnect. Set it (Secret Manager) for any deployment that stays connected"
            );
            Ok(SigningKey::generate())
        }
    }
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

    /// 未設定は生成鍵へフォールバックする（ローカル開発を壊さないため）。
    /// **これが許されるのは「未設定」だけ**である（下の 2 件と対で読むこと）。
    #[test]
    fn resolve_signing_key_falls_back_to_a_generated_key_when_unset() {
        assert!(resolve_signing_key(None).is_ok());
    }

    /// **`require_nonempty_env` のイディオムへ揃えてはいけない**ことを固定する。
    ///
    /// あちらは `None` と `Some("")` を同じ扱い（どちらもエラー）にしているが、こちらは
    /// `None` = 生成鍵 / `Some("")` = **エラー**という非対称を意図的に持つ。将来「一貫性の
    /// ために揃えよう」と `.filter(|s| !s.is_empty())` を挟むと、Secret Manager のマウント
    /// 漏れ（空文字が入る）が silent に生成鍵フォールバックへ落ち、warn だけ出して
    /// 「動くが再起動のたびに全利用者がログアウトする」という本番障害が完全に再発する。
    #[test]
    fn resolve_signing_key_rejects_empty_value_instead_of_falling_back() {
        let err = resolve_signing_key(Some(String::new())).unwrap_err();
        assert!(
            err.to_string().contains("CS_SUPPORT_OAUTH_SIGNING_KEY"),
            "the error must name the env var so the operator can act: {err}"
        );
    }

    #[test]
    fn resolve_signing_key_rejects_whitespace_only_value_instead_of_falling_back() {
        assert!(resolve_signing_key(Some("   ".to_string())).is_err());
    }

    /// 十分な長さの材料は受理する（`openssl rand -base64 32` は 44 バイトを出力する）。
    #[test]
    fn resolve_signing_key_accepts_material_from_openssl_rand_base64_32() {
        let realistic = "K7dQ2mVx8pL4nR6tY9wZ1aB3cD5eF0gH2iJ4kL6mN8o=";
        assert_eq!(realistic.len(), 44);
        assert!(resolve_signing_key(Some(realistic.to_string())).is_ok());
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

    // ---- validate_business_hours_config（会話フロー v1.1 design doc §6） ----

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
}
