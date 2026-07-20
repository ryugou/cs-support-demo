use super::verifier::GoogleTokenVerifier;
use super::{AuthError, VerifiedEmail};
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{extract::Request, middleware::Next};
use std::sync::Arc;

/// `/{project_id}/mcp` を包む認証ミドルウェアの状態。
/// `verifier` は project 間で共有する（Google 検証は project 非依存）。
/// `resource_metadata_url` は project ごとに異なるため、project 単位で
/// `AuthState` を構築して layer する（main.rs 側の責務）。
#[derive(Clone)]
pub struct AuthState {
    pub verifier: Arc<GoogleTokenVerifier>,
    /// この保護リソースのメタデータ URL（401 の WWW-Authenticate に載せる）。
    pub resource_metadata_url: String,
}

/// `/{project_id}/mcp` を包む認証ミドルウェア。
///
/// - Bearer 無し / 検証失敗（無効・期限切れ・aud 不一致・email 未検証）→ 401 + `WWW-Authenticate`
///   （Claude 側の OAuth 発見フローのトリガ。RFC 9728 準拠）。
/// - Google tokeninfo 到達不能 → 503（クライアント側のトークン不備ではなく運用側の障害だと
///   運用者が切り分けられるよう、401 とは区別する）。
/// - 成功 → 検証済み email を `request.extensions` に注入して次のハンドラへ渡す。
///   下流の `Harness::begin` はこの extensions を読み、Authorization ヘッダを直接見ない。
///
/// 失敗理由は分類（missing_bearer / invalid_token / google_unreachable）のみを
/// tracing に残す。トークン文字列そのものは絶対にログしない（漏洩防止）。
pub async fn require_google_auth(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_string);

    let token = match bearer {
        None => {
            tracing::info!(reason = "missing_bearer", "auth rejected");
            return unauthorized(&state.resource_metadata_url);
        }
        // `Bearer ` の後ろが空文字（ヘッダはあるがトークンが空）。ここで弾かないと
        // 空トークンのまま Google tokeninfo に問い合わせてしまう（無駄なリクエスト、かつ
        // 呼び出しごとに区別できないログになる）。missing_bearer とは reason を分けて残す。
        Some(t) if t.is_empty() => {
            tracing::info!(reason = "empty_bearer", "auth rejected");
            return unauthorized(&state.resource_metadata_url);
        }
        Some(t) => t,
    };

    match state.verifier.verify(&token).await {
        Ok(email) => {
            request.extensions_mut().insert(VerifiedEmail(email));
            next.run(request).await
        }
        Err(AuthError::Unreachable(msg)) => {
            tracing::error!(reason = "google_unreachable", error = %msg, "auth check failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(AuthError::Invalid(msg)) => {
            tracing::info!(reason = "invalid_token", error = %msg, "auth rejected");
            unauthorized(&state.resource_metadata_url)
        }
        Err(AuthError::Missing) => {
            tracing::info!(reason = "missing_bearer", "auth rejected");
            unauthorized(&state.resource_metadata_url)
        }
    }
}

/// 401 + RFC 9728 の発見用 `WWW-Authenticate: Bearer resource_metadata="..."` を組み立てる。
fn unauthorized(resource_metadata_url: &str) -> Response {
    let mut res = StatusCode::UNAUTHORIZED.into_response();
    let value = format!("Bearer resource_metadata=\"{resource_metadata_url}\"");
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
    use tower::ServiceExt;

    fn app() -> Router {
        let state = AuthState {
            verifier: Arc::new(crate::oauth::verifier::GoogleTokenVerifier::new(
                "client-x".into(),
            )),
            resource_metadata_url: "https://h/.well-known/oauth-protected-resource/urtect/mcp"
                .into(),
        };
        Router::new()
            .route("/urtect/mcp", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                state,
                require_google_auth,
            ))
    }

    #[tokio::test]
    async fn missing_bearer_yields_401_with_www_authenticate() {
        let res = app()
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
    }

    /// `Bearer ` の後ろが空文字のケース。実 Google エンドポイントに到達できない
    /// テスト環境でも、このケースは verifier を呼ばず即 401 になることを保証する
    /// （呼んでしまうと到達不能で 503 になり得るため、このテストが両者を区別する）。
    #[tokio::test]
    async fn empty_bearer_yields_401_without_calling_verifier() {
        let res = app()
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
}
