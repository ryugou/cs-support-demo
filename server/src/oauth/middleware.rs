use super::verifier::GoogleTokenVerifier;
use super::AuthError;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{extract::Request, middleware::Next};
use std::sync::Arc;

/// `/{project_id}/mcp` を包む認証ミドルウェアの状態。
///
/// 2026-07 改訂 / 自前トークン発行の廃止:
/// 一時期このサービスは自前の署名付きアクセストークンを発行し、ここでその署名を
/// 検証していた。トークンの寿命・失効・鍵管理を自前で抱える設計を撤回したため、
/// クライアントが提示する Bearer は **Google が発行したアクセストークン**に戻った。
/// したがって検証も Google tokeninfo への照会（`GoogleTokenVerifier`）に戻る。
///
/// この形の代償と利点:
/// - 代償: リクエスト経路が Google への RTT と障害に晒される。`GoogleTokenVerifier`
///   の TTL キャッシュがこれを緩和する。
/// - 代償: トークンに project_id を載せられないため、**ある project 向けに取得した
///   トークンがこのサーバの全 project の endpoint で通る**（`authserver` の
///   モジュールコメント参照）。旧実装の `aud` 照合はここで失われている。
/// - 利点: 失効が Google 側の操作でそのまま効く。こちらは失効台帳を持たない。
/// - 利点: 署名鍵に依存しないので、**プロセス再起動で利用者がログアウトしない**。
///
/// `verifier` は project 間で共有する（Google の client_id 単位の検証であり
/// project 非依存）。`resource_metadata_url` は project ごとに異なるため、
/// project 単位で `AuthState` を構築して layer する（main.rs 側の責務）。
#[derive(Clone)]
pub struct AuthState {
    pub verifier: Arc<GoogleTokenVerifier>,
    /// この保護リソースのメタデータ URL（401 の WWW-Authenticate に載せる）。
    pub resource_metadata_url: String,
}

/// `/{project_id}/mcp` を包む認証ミドルウェア。
///
/// - Bearer 無し / 検証失敗（無効・期限切れ・aud 不一致・email 未検証）
///   → 401 + `WWW-Authenticate`（Claude 側の OAuth 発見フローのトリガ。RFC 9728 準拠）。
/// - Google 到達不能 → 503。**401 に倒さない**。到達できないことは「トークンが
///   無効である」ことの証拠ではなく、401 を返すとクライアントは再ログインを
///   試み、Google 障害が全利用者の強制ログアウトに化ける。
/// - 成功 → 検証済み identity（安定した `sub` + 認証時点の email）を `request.extensions` に
///   注入して次のハンドラへ渡す。
///   下流の `Harness::begin` はこの extensions を読み、Authorization ヘッダを直接見ない。
///
/// 失敗理由は分類（missing_bearer / empty_bearer / invalid_token）のみを
/// tracing に残す。トークン文字列そのものは絶対にログしない（漏洩防止）。
/// `GoogleTokenVerifier` が返すメッセージもトークン本体を含まない
/// （`verifier.rs` の `describe_transport_error` / `sanitize_url` 参照）。
pub async fn require_google_auth(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bearer)
        .map(str::to_string);

    let token = match bearer {
        None => {
            tracing::info!(reason = "missing_bearer", "auth rejected");
            return unauthorized(&state.resource_metadata_url, false);
        }
        // `Bearer ` の後ろが空文字（ヘッダはあるがトークンが空）。署名検証でも弾けるが、
        // 「ヘッダ自体が無い」と「ヘッダはあるがトークンが空」はクライアント側の
        // 不具合として原因が違うため、reason を分けて残す。
        Some(t) if t.is_empty() => {
            tracing::info!(reason = "empty_bearer", "auth rejected");
            return unauthorized(&state.resource_metadata_url, true);
        }
        Some(t) => t,
    };

    match state.verifier.verify(&token).await {
        Ok(identity) => {
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Err(AuthError::Invalid(msg)) => {
            tracing::info!(reason = "invalid_token", error = %msg, "auth rejected");
            unauthorized(&state.resource_metadata_url, true)
        }
        Err(AuthError::Missing) => {
            tracing::info!(reason = "missing_bearer", "auth rejected");
            unauthorized(&state.resource_metadata_url, false)
        }
        Err(AuthError::Unreachable(msg)) => {
            // Google に届かなかった。**401 に倒さない**（上のドキュメント参照）。
            tracing::error!(reason = "verifier_unreachable", error = %msg, "auth check failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// Authorization ヘッダ値から Bearer トークンを取り出す純粋関数（ネットワーク非依存）。
///
/// RFC 9110 により auth-scheme は case-insensitive。旧実装は
/// `strip_prefix("Bearer ")`（大文字 `B` 固定・単一スペース固定）の文字列一致
/// だったため、`bearer x` / `BEARER x` や、スキームとトークンの間がタブ・
/// 複数スペースの正当なヘッダを誤って拒否していた。
///
/// ヘッダ値を最初の空白文字（space または tab）で2分割し、前半を `"bearer"`
/// と大小文字を無視して比較する。一致すれば後半の前後の空白列（space/tab）を
/// trim して返す。RFC 9110 は field value 末尾の OWS（optional whitespace）を
/// 許容するため、先頭だけでなく末尾も trim しないと `"Bearer abc "` のような
/// 正当なヘッダのトークンに余分な空白が残ってしまう。
/// トークン部分が空文字になるケース（例: `"Bearer "`）も
/// そのまま `Some("")` として返す —「スキームは合っているがトークンが空」を
/// 呼び出し側（`require_google_auth`）が区別できるようにするため。空トークンは
/// Google tokeninfo へ問い合わせず即 401 にする既存の短絡があり、この関数が
/// `None` に潰すとその短絡が働かなくなる。
///
/// スキームが `bearer` 以外、または区切りとなる空白が無い場合（`"Bearer"` 単体
/// など、スキームの後にトークンが続かない）は `None`。
fn parse_bearer(header_value: &str) -> Option<&str> {
    let idx = header_value.find([' ', '\t'])?;
    let (scheme, rest) = header_value.split_at(idx);
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(rest.trim_matches([' ', '\t']))
}

/// `WWW-Authenticate` の値を組み立てる純関数。
///
/// `credentials_presented` が true（トークンを提示したが無効・期限切れだった）のときだけ、
/// RFC 6750 §3 の `error` / `error_description` を載せる。**認証情報を一切提示していない
/// リクエストには載せない** — 同 §3 が明示しており、載せると「持っているトークンが無効」と
/// いう誤情報をクライアントへ渡すことになる。
///
/// `error_description` はヘッダ値なので、二重引用符・改行を含めない固定文にする
/// （動的な文字列を入れるとヘッダを壊しうる。トークン由来の情報は絶対に入れない）。
fn challenge_value(resource_metadata_url: &str, credentials_presented: bool) -> String {
    let base = format!("Bearer resource_metadata=\"{resource_metadata_url}\"");
    if !credentials_presented {
        return base;
    }
    format!(
        "{base}, error=\"invalid_token\", \
         error_description=\"The access token is expired or invalid. \
         Reconnect this connector to sign in again.\""
    )
}

/// 401 + RFC 9728 の発見用 `WWW-Authenticate` を組み立てる。
///
/// `credentials_presented` は [`challenge_value`] へそのまま渡す（意味はそちらの doc を参照）。
fn unauthorized(resource_metadata_url: &str, credentials_presented: bool) -> Response {
    let mut res = StatusCode::UNAUTHORIZED.into_response();
    let value = challenge_value(resource_metadata_url, credentials_presented);
    match HeaderValue::from_str(&value) {
        Ok(hv) => {
            res.headers_mut().insert(header::WWW_AUTHENTICATE, hv);
        }
        Err(e) => {
            // resource_metadata_url が HeaderValue として不正な文字を含む場合。
            // ヘッダを付けられないと Claude 側の OAuth 発見が動かないため、
            // 運用者が設定不備に気づけるようログに残す（401 自体は返す）。
            tracing::error!(
                error = %e,
                url = resource_metadata_url,
                "failed to build WWW-Authenticate header from resource_metadata_url"
            );
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    use crate::oauth::VerifiedIdentity;

    const GOOGLE_CLIENT_ID: &str = "google-client-id.apps.googleusercontent.com";

    /// 固定 JSON を返す使い捨て tokeninfo stub。実 Google を叩かずに、
    /// 「検証を通ったリクエストが 200 になり identity が注入されること」まで
    /// 確認できるようにする（この経路が無いと 401 系のテストしか書けない）。
    async fn spawn_tokeninfo(body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}/")
    }

    /// tokeninfo が到達不能な AS（誰も listen していないポートを指す）。
    async fn dead_tokeninfo() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}/")
    }

    fn app_with(tokeninfo_url: String) -> Router {
        let state = AuthState {
            verifier: Arc::new(GoogleTokenVerifier::with_settings(
                GOOGLE_CLIENT_ID.to_string(),
                tokeninfo_url,
                Duration::from_secs(2),
                Duration::from_secs(2),
            )),
            resource_metadata_url: "https://h/.well-known/oauth-protected-resource/urtect/mcp"
                .into(),
        };
        Router::new()
            // 認証を通ったハンドラが、注入された identity を実際に読めることまで
            // 確認する（extensions への注入が抜けても 200 になってしまうため）。
            .route(
                "/urtect/mcp",
                get(|req: axum::extract::Request| async move {
                    match req.extensions().get::<VerifiedIdentity>() {
                        Some(id) => format!("ok:{}", id.sub),
                        None => "ok:no-identity".to_string(),
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                state,
                require_google_auth,
            ))
    }

    /// tokeninfo を一切叩かずに 401 になる経路のテスト用。到達不能な URL を
    /// 指しておくことで、「verifier を呼んでしまったら 503 になる」= テストが
    /// 失敗する形にしてある（401 と 503 の区別がそのまま短絡の生死を表す）。
    async fn app_without_upstream() -> Router {
        app_with(dead_tokeninfo().await)
    }

    async fn get_with_bearer(app: Router, token: &str) -> axum::http::Response<Body> {
        app.oneshot(
            Request::builder()
                .uri("/urtect/mcp")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    const VALID_TOKENINFO: &str = concat!(
        r#"{"aud":"google-client-id.apps.googleusercontent.com","#,
        r#""azp":"google-client-id.apps.googleusercontent.com","sub":"1122334455","#,
        r#""email":"cs@example.com","email_verified":"true","expires_in":"3599"}"#
    );

    /// Google が発行したトークンで通り、identity が下流に届くこと。
    /// 自前トークン発行を廃止した後の**正常系そのもの**である。
    #[tokio::test]
    async fn a_google_token_is_accepted_and_injects_identity() {
        let app = app_with(spawn_tokeninfo(VALID_TOKENINFO).await);
        let res = get_with_bearer(app, "google-access-token").await;
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "ok:1122334455");
    }

    /// `aud` が別の OAuth クライアント宛のトークンは 401。これが無いと、
    /// 任意の Google アプリ向けに発行されたトークンでこの MCP が開通する
    /// （OAuth の confused deputy の典型）。
    #[tokio::test]
    async fn a_token_for_another_google_client_yields_401() {
        const OTHER_AUD: &str = concat!(
            r#"{"aud":"someone-else.apps.googleusercontent.com","#,
            r#""azp":"someone-else.apps.googleusercontent.com","sub":"1122334455","#,
            r#""email":"cs@example.com","email_verified":"true","expires_in":"3599"}"#
        );
        let app = app_with(spawn_tokeninfo(OTHER_AUD).await);
        let res = get_with_bearer(app, "token-for-another-app").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// 未検証 email は 401。email は下流の actor 表示に使われるため、
    /// 検証されていない値を identity として通さない。
    #[tokio::test]
    async fn an_unverified_email_yields_401() {
        const UNVERIFIED: &str = concat!(
            r#"{"aud":"google-client-id.apps.googleusercontent.com","#,
            r#""azp":"google-client-id.apps.googleusercontent.com","sub":"1122334455","#,
            r#""email":"cs@example.com","email_verified":"false","expires_in":"3599"}"#
        );
        let app = app_with(spawn_tokeninfo(UNVERIFIED).await);
        let res = get_with_bearer(app, "unverified-email-token").await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// **Google に届かないときは 503 で、401 ではない。**
    /// 401 に倒すとクライアントは再ログインを試み、Google 側の一時障害が
    /// 全利用者の強制ログアウトに化ける。
    #[tokio::test]
    async fn an_unreachable_google_yields_503_not_401() {
        let app = app_without_upstream().await;
        let res = get_with_bearer(app, "some-token").await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn missing_bearer_yields_401_with_www_authenticate() {
        let res = app_without_upstream()
            .await
            .oneshot(
                Request::builder()
                    .uri("/urtect/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let wa = res
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(wa.contains("resource_metadata="));
        assert!(wa.contains("/.well-known/oauth-protected-resource/urtect/mcp"));
        // RFC 6750 §3: 認証情報を一切提示していないリクエストには error を載せない
        // （「持っているトークンが無効」という誤った情報をクライアントへ渡さないため）。
        assert!(
            !wa.contains("error="),
            "a request with no credentials must not be told its token is invalid: {wa}"
        );
    }

    #[test]
    fn invalid_token_challenge_carries_rfc6750_error_and_description() {
        // トークンを提示したが無効／期限切れだった場合は、RFC 6750 §3 に従って
        // error="invalid_token" と人間可読な説明を載せる。クライアント（claude.ai）が
        // 「再接続が必要」と表示する材料になる。表示するかは client 側の裁量。
        let wa = challenge_value(
            "https://h/.well-known/oauth-protected-resource/urtect/mcp",
            true,
        );
        assert!(wa.starts_with("Bearer "));
        assert!(wa.contains("resource_metadata=\"https://h/"));
        assert!(wa.contains("error=\"invalid_token\""));
        assert!(wa.contains("error_description=\""));
        // 説明文はヘッダ値なので、二重引用符や制御文字を含めない（ヘッダを壊す）。
        let description = wa.split("error_description=\"").nth(1).unwrap();
        let description = description.trim_end_matches('"');
        assert!(!description.contains('"'));
        assert!(!description.contains('\n'));
    }

    #[test]
    fn challenge_without_credentials_omits_error_params() {
        let wa = challenge_value("https://h/meta", false);
        assert_eq!(wa, "Bearer resource_metadata=\"https://h/meta\"");
    }

    /// `Bearer ` の後ろが空文字のケース。verifier を呼ばず即 401 になることを
    /// 保証する。呼んでしまうとこの構成では到達不能で 503 になるため、
    /// ステータスの違いがそのまま短絡の生死を表す。
    #[tokio::test]
    async fn empty_bearer_yields_401_without_calling_the_verifier() {
        let res = app_without_upstream()
            .await
            .oneshot(
                Request::builder()
                    .uri("/urtect/mcp")
                    .header(header::AUTHORIZATION, "Bearer ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// auth-scheme が `bearer` 以外（例: `Basic`）の場合も、verifier を呼ばず
    /// 即 401 になること（退行防止）。
    #[tokio::test]
    async fn basic_scheme_yields_401_without_calling_the_verifier() {
        let res = app_without_upstream()
            .await
            .oneshot(
                Request::builder()
                    .uri("/urtect/mcp")
                    .header(header::AUTHORIZATION, "Basic x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    // --- parse_bearer: ネットワーク非依存の純粋関数ユニットテスト ---
    //
    // RFC 9110 では auth-scheme は case-insensitive。しかし旧実装は
    // `strip_prefix("Bearer ")` という固定文字列一致だったため、`bearer x` /
    // `BEARER x` や、スキームとトークンの間がタブ・複数スペースの正当な
    // ヘッダを誤って 401 で拒否していた。

    #[test]
    fn parse_bearer_accepts_canonical_scheme() {
        assert_eq!(parse_bearer("Bearer x"), Some("x"));
    }

    #[test]
    fn parse_bearer_accepts_lowercase_scheme() {
        assert_eq!(parse_bearer("bearer x"), Some("x"));
    }

    #[test]
    fn parse_bearer_accepts_uppercase_scheme() {
        assert_eq!(parse_bearer("BEARER x"), Some("x"));
    }

    #[test]
    fn parse_bearer_accepts_tab_separated_token() {
        assert_eq!(parse_bearer("Bearer\tx"), Some("x"));
    }

    #[test]
    fn parse_bearer_accepts_multiple_spaces_before_token() {
        assert_eq!(parse_bearer("Bearer   x"), Some("x"));
    }

    /// スキームは合っているがトークンが空文字のケース。`require_google_auth` 側の
    /// 「空トークンは verifier を呼ばず即 401」短絡が機能するために、ここで
    /// `None` に潰さず `Some("")` を返すことが必須。
    #[test]
    fn parse_bearer_returns_empty_string_for_blank_token() {
        assert_eq!(parse_bearer("Bearer "), Some(""));
    }

    /// W3（reviewer 指摘）: 空トークン短絡の生死を分ける最重要ケース。
    /// 複数スペース/タブが続くだけの「空白のみのトークン」も `Some("")` に
    /// 潰れることを保証する。将来 `split_once` 等へ書き換えた際に `Some(" ")`
    /// を返す実装が混入すると、`is_empty()` が false になり短絡をすり抜けて
    /// 空白トークンのまま Google tokeninfo に投げてしまうため、これを防ぐ。
    #[test]
    fn parse_bearer_returns_empty_string_for_multiple_spaces_only_token() {
        assert_eq!(parse_bearer("Bearer   "), Some(""));
    }

    #[test]
    fn parse_bearer_returns_empty_string_for_tabs_only_token() {
        assert_eq!(parse_bearer("Bearer\t\t"), Some(""));
    }

    /// W1（reviewer 指摘）: RFC 9110 は field value 末尾の OWS を許容するため、
    /// `"Bearer abc "`（トークン末尾に空白）は `Some("abc")` になるべきで、
    /// 末尾空白付きトークンをそのまま Google tokeninfo に送ってはならない。
    #[test]
    fn parse_bearer_trims_trailing_whitespace_from_token() {
        assert_eq!(parse_bearer("Bearer abc "), Some("abc"));
    }

    #[test]
    fn parse_bearer_rejects_non_bearer_scheme() {
        assert_eq!(parse_bearer("Basic x"), None);
    }

    /// 区切りとなる空白が無い（`"Bearer"` 単体、トークンが続かない）場合は
    /// 分割できないため `None`。
    #[test]
    fn parse_bearer_rejects_scheme_without_separator() {
        assert_eq!(parse_bearer("Bearer"), None);
    }
}
