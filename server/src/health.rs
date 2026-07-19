//! ヘルスチェック用ルータ。
//!
//! Cloud Run の `*.run.app` エッジ（Google Front End）は、リクエストをコンテナへ
//! 転送する前にリテラルパス `/healthz` を予約パスとして横取りし、独自の 404
//! （`Error 404 (Not Found)!!1` の HTML、`x-cloud-trace-context` なし）を返す。
//! このためコンテナ内に `/healthz` ルートを張っても、run.app URL 経由では
//! リクエストがアプリに一切届かない（tower_http のリクエストログも出ない）。
//!
//! 検証結果（`cs-support-mcp-....run.app` 実測）:
//!   - `/healthz`  -> Google エッジ 404（trace-context なし、1568B の Google HTML）
//!   - `/healthz/` -> コンテナ到達（axum 404、trace-context あり）
//!   - `/livez`    -> コンテナ到達（axum 404、trace-context あり）
//!   - `/foobar`   -> コンテナ到達（axum 404、trace-context あり）
//!
//! よって run.app からのヘルス確認は `/healthz` 以外のパスで行う必要がある。
//! GCE / Caddy のような独自ドメイン経由では `/healthz` は従来どおり到達するため、
//! 互換維持のため `/healthz` は残し、run.app でも到達可能な `/livez` を併設する。

use axum::{routing::get, Router};

/// ヘルスチェック応答本体。liveness のみを示す固定応答。
async fn ok() -> &'static str {
    "ok"
}

/// health エンドポイント群を張ったルータを返す。
///
/// `/healthz` は独自ドメイン（GCE/Caddy）互換のために残す。`/livez` は
/// `*.run.app` エッジに予約されていない到達可能パスで、Cloud Run 上での
/// 外形監視・疎通確認に使う。
pub fn health_router() -> Router {
    Router::new()
        .route("/healthz", get(ok))
        .route("/livez", get(ok))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    /// 指定パスへ GET し、(status, body) を返す小さなヘルパ。
    async fn get_path(path: &str) -> (StatusCode, String) {
        let response = health_router()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router must not error on a plain GET");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("read body");
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("utf8 body"),
        )
    }

    /// `/livez` は 200 "ok" を返さねばならない。
    ///
    /// この経路は「run.app エッジが `/healthz` を予約横取りする」問題の恒久回避策で、
    /// Cloud Run 上でコンテナに到達可能な唯一のヘルスパスである。ここが 404 に
    /// 退行すると、run.app からのヘルス確認手段が完全に失われる。
    #[tokio::test]
    async fn livez_returns_200_ok_as_run_app_reachable_health_path() {
        let (status, body) = get_path("/livez").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "/livez must be 200 so run.app health checks have a reachable path (/healthz is eaten by the Google edge)"
        );
        assert_eq!(body, "ok");
    }

    /// `/healthz` は独自ドメイン（GCE/Caddy）互換のため 200 "ok" を維持する。
    #[tokio::test]
    async fn healthz_returns_200_ok_for_custom_domain_compat() {
        let (status, body) = get_path("/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }
}
