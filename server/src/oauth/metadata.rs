use axum::{extract::Path, routing::get, Json, Router};
use serde::Serialize;

const GOOGLE_ISSUER: &str = "https://accounts.google.com";

#[derive(Debug, Serialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
}

fn metadata(resource: String) -> Json<ProtectedResourceMetadata> {
    Json(ProtectedResourceMetadata {
        resource,
        authorization_servers: vec![GOOGLE_ISSUER.to_string()],
    })
}

/// RFC 9728 の保護リソースメタデータを公開する（無認証）。
/// ルート版と、テナントパス版 `/.well-known/oauth-protected-resource/{project_id}/mcp` の両方を張る
/// （Claude は path 付きを先に問い合わせるため）。
///
/// `_project_ids` は将来 allowlist 検証（未登録 project_id を 404 にする等）に使う想定で受けるが、
/// 現状は任意の project_id をそのまま resource URL に反映する（YAGNI）。
pub fn metadata_router(public_host: String, _project_ids: Vec<String>) -> Router {
    let base = public_host.clone();
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(move || {
                let r = format!("https://{base}");
                async move { metadata(r) }
            }),
        )
        .route(
            "/.well-known/oauth-protected-resource/{project_id}/mcp",
            get(move |Path(project_id): Path<String>| {
                let r = format!("https://{public_host}/{project_id}/mcp");
                async move { metadata(r) }
            }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn path_scoped_metadata_returns_resource_and_google_as() {
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
        assert_eq!(
            json["authorization_servers"][0],
            "https://accounts.google.com"
        );
    }
}
