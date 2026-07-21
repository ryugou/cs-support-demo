use super::verifier::GoogleTokenVerifier;
use super::AuthError;
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
/// - 成功 → 検証済み identity（安定した `sub` + 当時の email）を `request.extensions` に
///   注入して次のハンドラへ渡す。
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
        .and_then(parse_bearer)
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
        Ok(identity) => {
            request.extensions_mut().insert(identity);
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

    /// auth-scheme が `bearer` 以外（例: `Basic`）の場合は、旧実装と同じく
    /// verifier を呼ばず即 401 になることを保証する（退行防止）。
    #[tokio::test]
    async fn basic_scheme_yields_401_without_calling_verifier() {
        let res = app()
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
    //
    // 「200 相当」を実際の HTTP ステータスとして確認するには本物の Google
    // tokeninfo に到達させる必要があり、外部ネットワーク依存になってテストが
    // 不安定になる（到達できれば 401/503、到達できなければ 503 になり得て、
    // どちらも「200 が返る」ことの確認にならない）。パース結果が
    // `Some(token)`（= verifier まで到達する形）になることを、ネットワークを
    // 一切使わないこのユニットテストで直接保証する。

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
    /// 分割できないため `None`。旧実装の `strip_prefix("Bearer ")` でも
    /// 一致せず `None` になっていた経路と同じ扱い。
    #[test]
    fn parse_bearer_rejects_scheme_without_separator() {
        assert_eq!(parse_bearer("Bearer"), None);
    }
}
