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

use crate::harness::audit::WormAuditLog;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use std::sync::Arc;

/// poisoned な監査ログを検知したときの 503 応答本文（固定・短文）。
///
/// ヘルスエンドポイントは認証なしで到達できるため、ファイルパスやエラー詳細を
/// 含めない（design doc §2.3 不変条件、Issue #62）。
const AUDIT_LOG_UNAVAILABLE_BODY: &str = "audit log unavailable";

#[derive(Clone)]
struct HealthState {
    /// 監視対象の監査ログ（0 個以上）。`line_adapter` は監査ログを持たないため空。
    /// `cs-support-mcp` は 1 個、`homesec_advisor` は 2 個（advisor 本体 + CS 連携用）。
    audit_logs: Arc<Vec<Arc<WormAuditLog>>>,
}

/// ヘルスチェック応答本体。
///
/// 監視対象の監査ログ（0 個以上）のいずれかが poisoned なら 503、
/// すべて健全（または監視対象が無い）なら 200 を返す（Issue #62）。
async fn health_handler(State(state): State<HealthState>) -> impl IntoResponse {
    if state.audit_logs.iter().any(|log| log.is_poisoned()) {
        (StatusCode::SERVICE_UNAVAILABLE, AUDIT_LOG_UNAVAILABLE_BODY)
    } else {
        (StatusCode::OK, "ok")
    }
}

/// health エンドポイント群を張ったルータを返す。
///
/// `/healthz` は独自ドメイン（GCE/Caddy）互換のために残す。`/livez` は
/// `*.run.app` エッジに予約されていない到達可能パスで、Cloud Run 上での
/// 外形監視・疎通確認に使う。
///
/// `audit_logs` には、このプロセスが持つ `WormAuditLog` を渡す（Issue #62）。
/// 空の `Vec` を渡すと常に 200 を返す（`line_adapter` の既存挙動と同じ）。
pub fn health_router(audit_logs: Vec<Arc<WormAuditLog>>) -> Router {
    let state = HealthState {
        audit_logs: Arc::new(audit_logs),
    };
    Router::new()
        .route("/healthz", get(health_handler))
        .route("/livez", get(health_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    /// 指定パスへ GET し、(status, body) を返す小さなヘルパ。
    async fn get_path(path: &str, audit_logs: Vec<Arc<WormAuditLog>>) -> (StatusCode, String) {
        let response = health_router(audit_logs)
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
    ///
    /// 監視対象の監査ログ 0 個（`line_adapter` と同じ構成）で検証する。
    #[tokio::test]
    async fn livez_returns_200_ok_as_run_app_reachable_health_path() {
        let (status, body) = get_path("/livez", vec![]).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "/livez must be 200 so run.app health checks have a reachable path (/healthz is eaten by the Google edge)"
        );
        assert_eq!(body, "ok");
    }

    /// `/healthz` は独自ドメイン（GCE/Caddy）互換のため 200 "ok" を維持する。
    /// 監視対象の監査ログ 0 個（`line_adapter` と同じ構成）で検証する。
    #[tokio::test]
    async fn healthz_returns_200_ok_for_custom_domain_compat() {
        let (status, body) = get_path("/healthz", vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    /// 健全な監査ログが 1 個以上あっても 200 のままであること
    /// （`cs-support-mcp` の通常稼働と同じ構成）。
    #[tokio::test]
    async fn health_endpoints_return_200_when_all_audit_logs_are_healthy() {
        let dir = std::env::temp_dir().join(format!("health-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = Arc::new(WormAuditLog::open(&path).expect("open worm log"));

        for endpoint in ["/livez", "/healthz"] {
            let (status, body) = get_path(endpoint, vec![log.clone()]).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{endpoint} must be 200 when healthy"
            );
            assert_eq!(body, "ok");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 複数の監査ログのうち1つでも poisoned なら、`/livez` と `/healthz` の両方が 503 を
    /// 返す（`homesec_advisor` が 2 個の監査ログを持つ構成を想定）。本文にファイルパスを
    /// 含めない（認証なしで到達できるため、design doc §2.3 不変条件）。
    #[tokio::test]
    async fn health_endpoints_return_503_when_any_audit_log_is_poisoned() {
        let dir = std::env::temp_dir().join(format!("health-test-{}", uuid::Uuid::new_v4()));
        let healthy_path = dir.join("healthy.jsonl");
        let poisoned_path = dir.join("poisoned.jsonl");
        let healthy = Arc::new(WormAuditLog::open(&healthy_path).expect("open healthy log"));
        let poisoned = Arc::new(WormAuditLog::open(&poisoned_path).expect("open poisoned log"));
        poisoned.poison_for_test();

        for endpoint in ["/livez", "/healthz"] {
            let (status, body) = get_path(endpoint, vec![healthy.clone(), poisoned.clone()]).await;
            assert_eq!(
                status,
                StatusCode::SERVICE_UNAVAILABLE,
                "{endpoint} must be 503 when any audit log is poisoned"
            );
            assert!(
                !body.contains(&poisoned_path.display().to_string()),
                "503 body must not leak the poisoned audit log path: {body}"
            );
            assert!(
                !body.contains(&healthy_path.display().to_string()),
                "503 body must not leak any audit log path: {body}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
