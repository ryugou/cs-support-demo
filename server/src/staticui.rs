//! `/admin` 配下の静的配信 + SPA フォールバックを組み立てる純関数群。
//!
//! `main.rs` から切り出したもの（挙動・doc コメントとも無変更の移動）。後続の
//! `homesec_advisor` バイナリなど、別の静的アセット配信でも同じロジックを再利用するため。

use std::path::{Path, PathBuf};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

/// config ファイル起点の相対パスを実パスへ解決する純関数。絶対パスはそのまま使い、相対パスは
/// `config_dir`（`args.config.parent()`、main() 冒頭で計算済み）基準で解決する。
/// `admin_static_dir` のほか、`homesec_advisor` の `ng_dictionary_path` / `images_dir` の
/// 解決にも使う（関数名は初出の用途由来で、実態は汎用のパス解決）。
///
/// `harness/mod.rs::Harness::build` 内の `resolve_path` クロージャと同じロジック
/// （相対パスは config ファイルの置き場所基準、絶対パスはそのまま）。共有関数への切り出しは
/// せず、2 箇所の重複を許容する（設定ファイル起点のパス解決は用途ごとに閉じたロジックで、
/// 抽象化の便益より結合が増える害の方が大きいと判断したため）。
pub fn resolve_admin_static_dir(config_dir: &Path, admin_static_dir: &str) -> PathBuf {
    let path = Path::new(admin_static_dir);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}

/// `/admin` 配下の静的配信 + SPA フォールバックを組み立てる純関数。
///
/// - 既知のファイル（`dir` 配下に実在するパス）は `ServeDir` がそのまま返す。
/// - 未知のパス（クライアントサイドルーティングが担う SPA ルート）は `index.html` の内容を
///   **200** で返す（`ServeDir::not_found_service` は常に 404 に上書きしてしまうため使わない。
///   SPA のブラウザ直リロードやブックマークは 404 ページではなくアプリシェルを受け取るべき、
///   という一般的な SPA 配信の作法に合わせる）。
/// - `dir` 自体が存在しない場合でも、`ServeDir` は構築時に検証せず、リクエスト時に
///   `std::io::ErrorKind::NotFound` を 404 へ変換する（tower-http の既定動作）。したがって
///   このディレクトリが無くてもサーバ起動自体は落ちない。静的ファイル配信はセキュリティ境界
///   ではない（design doc §5: 認証不要でアプリシェルに秘密は含まれない）ため、
///   ここを起動時 fail-closed の対象にはしない。
///
/// main() からもテスト（`tower::ServiceExt::oneshot`）からも呼べるよう純粋関数として切り出す。
///
/// **`Cache-Control: no-cache` を全応答へ付与する（reviewer 指摘 Warning 2）。** Angular は
/// `outputHashing: "all"` でビルドするため、デプロイのたびに JS/CSS チャンク名がハッシュごと
/// 変わるが、エントリポイントの `index.html` だけはハッシュ無しファイル名のまま変わらない。
/// ヘッダが無いとブラウザは `Last-Modified` による heuristic caching（RFC 9111 §4.2.2）を
/// 適用しうり、デプロイ後も古い `index.html` がキャッシュされたままだと、もう存在しない
/// チャンクを要求して白画面になる（利用者はハードリロードするまで復旧できない）。
/// `SetResponseHeaderLayer` で router 全体（`ServeDir` の既知ファイル経路・`ServeFile`
/// フォールバック経路の両方）に一括で掛ける。ハッシュ付きアセットまで `no-cache` になるが、
/// `Last-Modified` による条件付き GET で 304 が返るだけで、この規模の管理画面では実害が無い
/// （index.html だけ層を分けて掛け忘れるリスクの方が実害が大きいと判断した）。
pub fn admin_static_router(dir: &Path) -> axum::Router {
    let index_html = dir.join("index.html");
    let serve_dir = ServeDir::new(dir).fallback(ServeFile::new(index_html));
    axum::Router::new()
        .fallback_service(serve_dir)
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- resolve_admin_static_dir ----

    #[test]
    fn resolve_admin_static_dir_joins_relative_path_with_config_dir() {
        let config_dir = Path::new("/app/server");
        let resolved = resolve_admin_static_dir(config_dir, "admin-ui/browser");
        assert_eq!(resolved, PathBuf::from("/app/server/admin-ui/browser"));
    }

    #[test]
    fn resolve_admin_static_dir_keeps_absolute_path_as_is() {
        let config_dir = Path::new("/app/server");
        let resolved = resolve_admin_static_dir(config_dir, "/data/admin-ui/browser");
        assert_eq!(resolved, PathBuf::from("/data/admin-ui/browser"));
    }

    // ---- admin_static_router（/admin 配下の静的配信 + SPA フォールバック） ----

    mod admin_static_router_tests {
        use super::*;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use std::fs;
        use tower::ServiceExt; // for `oneshot`

        /// テスト用の一時ディレクトリ（テストごとに衝突しないよう uuid でユニーク化する。
        /// `admin.rs::test_admin_state_with_lexicon` と同じ流儀）。呼び出し元がビルド成果物
        /// 相当のダミーファイルを配置する。
        fn temp_static_dir() -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "cs-support-admin-static-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(dir.join("assets")).expect("create temp static dir");
            dir
        }

        async fn get_response(router: axum::Router, path: &str) -> axum::response::Response {
            router
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("build request"),
                )
                .await
                .expect("router must not error on a plain GET")
        }

        async fn get_path(router: axum::Router, path: &str) -> (StatusCode, String) {
            let response = get_response(router, path).await;
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("read body");
            (
                status,
                String::from_utf8(bytes.to_vec()).expect("utf8 body"),
            )
        }

        /// 既知のファイル（ビルド成果物のアセット相当）への GET は 200 でその内容を返す。
        /// 本番では `nest_service("/admin", ...)` 経由で `/admin/assets/app.js` として
        /// 到達するが、`admin_static_router` 自体は prefix を知らない純関数なので、ここでは
        /// prefix 剥がし後のパス（`/assets/app.js`）で直接叩く。
        #[tokio::test]
        async fn known_file_returns_200_with_its_content() {
            let dir = temp_static_dir();
            fs::write(dir.join("index.html"), "<html>app shell</html>").expect("write index.html");
            fs::write(dir.join("assets/app.js"), "console.log('app');")
                .expect("write assets/app.js");

            let (status, body) = get_path(admin_static_router(&dir), "/assets/app.js").await;

            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, "console.log('app');");
        }

        /// 未知のパス（Angular router が担う SPA ルート、例: `/threads/case-123`）への GET は
        /// 404 ではなく 200 で `index.html` の内容を返す（フォールバックが効いていることの固定）。
        /// ブラウザの直リロード・ブックマークが 404 ページに落ちないことを保証する。
        #[tokio::test]
        async fn unknown_spa_route_falls_back_to_index_html_with_200() {
            let dir = temp_static_dir();
            fs::write(dir.join("index.html"), "<html>app shell</html>").expect("write index.html");

            let (status, body) = get_path(admin_static_router(&dir), "/threads/case-123").await;

            assert_eq!(
                status,
                StatusCode::OK,
                "SPA route must fall back to the app shell, not 404"
            );
            assert_eq!(body, "<html>app shell</html>");
        }

        /// `/admin` はデプロイのたびに Angular の `outputHashing: "all"` によりハッシュ付き
        /// チャンク名が変わる。`Cache-Control` が無いとブラウザは `Last-Modified` ベースの
        /// heuristic caching（RFC 9111 §4.2.2）を適用しうり、デプロイ後も古い `index.html` が
        /// キャッシュから使われて存在しないチャンクを要求し、白画面になりうる（reviewer 指摘
        /// Warning 2）。既知ファイル（ハッシュ付きアセット）への応答にも一律で `no-cache` を
        /// 掛けるのは、`ServeDir` の `fallback` 経路と共通の `Router::layer` で両方をまとめて
        /// カバーする方が「index.html だけ層を分けて掛け忘れる」リスクより安全だと判断した
        /// ため。ハッシュ付きアセットが `no-cache` になっても `Last-Modified` による条件付き
        /// GET で 304 が返るだけで、この規模の管理画面では実害が無い。
        #[tokio::test]
        async fn known_file_response_carries_no_cache_header() {
            let dir = temp_static_dir();
            fs::write(dir.join("index.html"), "<html>app shell</html>").expect("write index.html");
            fs::write(dir.join("assets/app.js"), "console.log('app');")
                .expect("write assets/app.js");

            let response = get_response(admin_static_router(&dir), "/assets/app.js").await;

            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some("no-cache"),
                "known file (hashed asset) response must carry Cache-Control: no-cache"
            );
        }

        /// SPA フォールバック（`index.html`）は、デプロイでハッシュ付きチャンク名が変わった
        /// ときに白画面を引き起こす張本人（このエントリポイントだけがハッシュ無しファイル名で
        /// 提供される）。`known_file_response_carries_no_cache_header` と対で、両方の応答経路を
        /// 固定する。
        #[tokio::test]
        async fn spa_fallback_response_carries_no_cache_header() {
            let dir = temp_static_dir();
            fs::write(dir.join("index.html"), "<html>app shell</html>").expect("write index.html");

            let response = get_response(admin_static_router(&dir), "/threads/case-123").await;

            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some("no-cache"),
                "SPA fallback (index.html) response must carry Cache-Control: no-cache"
            );
        }

        /// ビルド成果物ディレクトリ自体が存在しない場合でも、`admin_static_router` の構築や
        /// リクエスト処理は panic せず 404 に倒れる（静的ファイル配信はセキュリティ境界では
        /// ないため、起動時 fail-closed の対象にしない、という設計判断の実測）。
        #[tokio::test]
        async fn missing_directory_returns_404_instead_of_crashing() {
            let dir = std::env::temp_dir().join(format!(
                "cs-support-admin-static-missing-{}",
                uuid::Uuid::new_v4()
            ));
            assert!(!dir.exists(), "precondition: directory must not exist");

            let (status, _body) = get_path(admin_static_router(&dir), "/assets/app.js").await;

            assert_eq!(status, StatusCode::NOT_FOUND);
        }
    }
}
