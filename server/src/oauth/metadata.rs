use axum::{extract::Path, http::StatusCode, routing::get, Json, Router};
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
/// `project_ids` は実際に mount されている project の一覧（`main.rs` が `config.projects` から構築）。
/// パススコープ版はこの一覧に無い `project_id` を 404 にする。実体 `/{project_id}/mcp` は既に
/// 未設定 project では 404 を返すため、ここで 200 を返すと discovery だけ成功して接続が失敗する
/// という不整合（原因究明困難）が生じる。ルート無し版はテナント非依存のため対象外。
pub fn metadata_router(public_host: String, project_ids: Vec<String>) -> Router {
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
                    Ok(metadata(r))
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
            "https://accounts.google.com"
        );
    }
}
