use axum::{extract::Path, http::StatusCode, routing::get, Json, Router};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
}

/// RFC 9728 の保護リソースメタデータ。
///
/// `authorization_servers` は **このサービス自身**を指す。
/// 旧構成は Google（`https://accounts.google.com`）を指していたが、Google は
/// RFC 7591 の DCR に対応していないため、claude.ai は接続のたびに利用者へ
/// Google の Client ID / Secret を手入力させる必要があった。運用不能であり、
/// かつ Client Secret をクライアント側に置くことになる。
/// 現行は `oauth::authserver` が AS を引き受け、内部で Google に委譲する。
fn metadata(resource: String, issuer: &str) -> Json<ProtectedResourceMetadata> {
    Json(ProtectedResourceMetadata {
        resource,
        authorization_servers: vec![issuer.to_string()],
    })
}

/// RFC 8414 の認可サーバメタデータ。
///
/// `token_endpoint_auth_methods_supported` が `none` なのは public client + PKCE
/// だから（`client_secret` を発行しない）。`code_challenge_methods_supported` に
/// `plain` を載せないのは、`plain` では challenge が verifier そのもので、
/// 認可リクエストを覗ける相手に verifier を渡すのと同じになるため。
fn authorization_server_metadata(public_host: &str) -> Json<serde_json::Value> {
    let base = format!("https://{public_host}");
    Json(serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["openid", "email", "profile"],
    }))
}

/// RFC 9728 の保護リソースメタデータを公開する（無認証）。
/// ルート版と、テナントパス版 `/.well-known/oauth-protected-resource/{project_id}/mcp` の両方を張る
/// （Claude は path 付きを先に問い合わせるため）。
///
/// `project_ids` は実際に mount されている project の一覧（`main.rs` が `config.projects` から構築）。
/// パススコープ版はこの一覧に無い `project_id` を 404 にする。実体 `/{project_id}/mcp` は既に
/// 未設定 project では 404 を返すため、ここで 200 を返すと discovery だけ成功して接続が失敗する
/// という不整合（原因究明困難）が生じる。ルート無し版はテナント非依存のため対象外。
pub fn metadata_router(public_host: String, project_ids: Vec<String>) -> Router {
    let base = public_host.clone();
    let as_host = public_host.clone();
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(move || {
                let host = as_host.clone();
                async move { authorization_server_metadata(&host) }
            }),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(move || {
                let r = format!("https://{base}");
                let issuer = r.clone();
                async move { metadata(r, &issuer) }
            }),
        )
        .route(
            "/.well-known/oauth-protected-resource/{project_id}/mcp",
            get(move |Path(project_id): Path<String>| {
                let public_host = public_host.clone();
                let project_ids = project_ids.clone();
                async move {
                    if !project_ids.iter().any(|p| p == &project_id) {
                        // axum の Path<String> extractor は percent-decode 済みの値を返すため、
                        // 未認証の攻撃者が project_id に %0A 等を仕込むと decode 後に実際の改行文字
                        // になり得る。Display（%project_id）でログ出力すると改行がそのまま出力され
                        // ログ1行の前提が崩れ、ログ偽造・注入につながる。Debug（?project_id）は
                        // 制御文字をエスケープして出力するため、ログはこの1行に収まる。
                        tracing::warn!(
                            project_id = ?project_id,
                            "oauth protected-resource metadata requested for unconfigured project_id; returning 404"
                        );
                        return Err(StatusCode::NOT_FOUND);
                    }
                    let r = format!("https://{public_host}/{project_id}/mcp");
                    Ok(metadata(r, &format!("https://{public_host}")))
                }
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// AS メタデータ（RFC 8414）。旧構成では認可サーバが Google 自身だったため
    /// このパスは 404 が正常だったが、DCR 非対応の Google を AS にしていると
    /// claude.ai が接続できない。本サービス自身が AS になったため 200 を返す。
    #[tokio::test]
    async fn authorization_server_metadata_advertises_this_service_as_the_issuer() {
        let app = metadata_router(
            "cs-support.example.com".to_string(),
            vec!["urtect".to_string()],
        );
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-authorization-server")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["issuer"], "https://cs-support.example.com");
        assert_eq!(
            json["authorization_endpoint"],
            "https://cs-support.example.com/oauth/authorize"
        );
        assert_eq!(
            json["token_endpoint"],
            "https://cs-support.example.com/oauth/token"
        );
        assert_eq!(
            json["registration_endpoint"],
            "https://cs-support.example.com/oauth/register"
        );
        assert_eq!(json["response_types_supported"][0], "code");
        assert_eq!(json["grant_types_supported"][0], "authorization_code");
        assert_eq!(json["grant_types_supported"][1], "refresh_token");
        // PKCE 必須 / public client。plain は広告しない。
        assert_eq!(json["code_challenge_methods_supported"][0], "S256");
        assert_eq!(
            json["code_challenge_methods_supported"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(json["token_endpoint_auth_methods_supported"][0], "none");
    }

    #[tokio::test]
    async fn path_scoped_metadata_points_at_this_service_as_the_authorization_server() {
        let app = metadata_router(
            "cs-support.example.com".to_string(),
            vec!["urtect".to_string()],
        );
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource/urtect/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["resource"],
            "https://cs-support.example.com/urtect/mcp"
        );
        // Google ではなく自分自身を指す（DCR 非対応の Google を AS にすると
        // claude.ai が接続できないため AS を自前化した）。
        assert_eq!(
            json["authorization_servers"][0],
            "https://cs-support.example.com"
        );
    }

    #[tokio::test]
    async fn path_scoped_metadata_returns_404_for_unconfigured_project() {
        let app = metadata_router(
            "cs-support.example.com".to_string(),
            vec!["urtect".to_string()],
        );
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource/nonexistent-project/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn path_scoped_metadata_returns_200_for_second_configured_project() {
        // project_ids が2件以上のとき、先頭要素以外にも一致できることを固定するテスト。
        // `project_ids.iter().any(|p| p == &project_id)` を
        // `project_ids.first() == Some(&project_id)`（先頭要素のみ比較する誤実装）に
        // 壊すと、このテストだけが red になる。
        let app = metadata_router(
            "cs-support.example.com".to_string(),
            vec!["urtect".to_string(), "other-project".to_string()],
        );
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource/other-project/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["resource"],
            "https://cs-support.example.com/other-project/mcp"
        );
    }

    #[tokio::test]
    async fn root_metadata_returns_200_regardless_of_project_ids() {
        let app = metadata_router(
            "cs-support.example.com".to_string(),
            vec!["urtect".to_string()],
        );
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["resource"], "https://cs-support.example.com");
        assert_eq!(
            json["authorization_servers"][0],
            "https://cs-support.example.com"
        );
    }
}
