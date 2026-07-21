# cs-support-mcp Google OAuth コネクタ化 実装計画

> **[supersession 注記, 2026-07-21]** 本ドキュメントが前提とする email→actor ホワイトリスト（config `[[actors]]` の email 事前登録による fail-closed 拒否、15行目・641行目・715-729行目のコード例）は commit `1809b8e`（`feat(authn): remove config-based email whitelist, fail-open to supervisor actor`）で撤去済み。現行の認可設計は `specs/production-cs-mcp.md` の「AuthN 現状」節（2026-07-21 更新）を正とする。本ドキュメントは実装計画の履歴として保持する。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax. 実装は kaneko、レビューは reviewer、GCP/gcloud 作業と実機 E2E は Fable。

**Goal:** cs-support-mcp を Google OAuth のリソースサーバ化し、claude.ai / Desktop の UI コネクタ追加から Gmail ログインで利用可能にする。静的 JWT は撤去。

**Architecture:** 認可サーバは Google（GCP OAuth 同意画面＋Web クライアント）。cs-support-mcp は `/.well-known/oauth-protected-resource` を公開し、`/{project_id}/mcp` を **axum 認証ミドルウェア**で包む。ミドルウェアは Bearer を Google tokeninfo で検証（aud=自クライアント / email_verified）し、検証済み email を request extensions に注入。未/無効トークンは **HTTP 401 + WWW-Authenticate**（Claude の OAuth 発見トリガ）。下流の `Harness::begin` は注入 email を actor 表（email キー・ホワイトリスト）に突合して AuthZ。

**Tech Stack:** Rust, axum 0.8, rmcp 1.7, reqwest 0.12（既存依存）, tokio, serde, anyhow, tracing。

## Global Constraints

- Python / TypeScript 禁止。新規 crate 依存は追加しない（`reqwest` は既存 `[dependencies]` にあり全 bin にリンク済み）。
- 認証は Google OAuth 一本。静的 HS256 JWT の検証・`Claims`・`sub` 経路・`default_actor`・`jwt_issuer`・`CS_SUPPORT_JWT_SECRET_FILE`/`cs-support-jwt-secret` を撤去する。
- 認可の正本は config の actor 表（server 導出）。email が actor 表に無ければ fail-closed 拒否（ホワイトリスト）。role→tool 認可（`add_known_resolution` は supervisor/admin 等）は現行維持。
  > **[撤去済み, 2026-07-21]** この email ホワイトリスト（fail-closed 拒否）は commit `1809b8e` で撤去済み。現行実装は突合なしで検証済み email を無条件に supervisor へ解決する（`server/src/harness/authn.rs:70-85`）。詳細は `specs/production-cs-mcp.md` の「AuthN 現状」節を参照。
- エラーは握りつぶさない。全認証失敗パスに分類（missing/invalid/unreachable/not-authorized）をログ（tracing）で残す。**トークン全体はログしない**（email・分類のみ）。
- Google tokeninfo endpoint: `https://oauth2.googleapis.com/tokeninfo?access_token=<token>`。到達は public egress（VPC 経由でない）。
- aud 照合の期待値は env `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID`。
- 検証コマンド（RUSTC_WRAPPER unset 必須。以下 CARGO）: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo <cmd>`。コミット前に `CARGO fmt` → `CARGO fmt --check` クリーン → `CARGO test --lib` 全 pass → `CARGO check --bins` Finished。
- コミットは Conventional Commits。push はユーザ。実装は main 直 push 禁止（ブランチ `feat/gcp-cloudrun-deploy` 上で継続、または新ブランチ）。

## File Structure

- `server/src/oauth/mod.rs`（新規）: `pub struct VerifiedEmail(pub String)`（Clone）、`pub enum AuthError`、サブモジュール再エクスポート。
- `server/src/oauth/metadata.rs`（新規）: `/.well-known/oauth-protected-resource*` の axum ハンドラ・Router 構築。
- `server/src/oauth/verifier.rs`（新規）: `GoogleTokenVerifier`（introspection＋キャッシュ）と純粋判定関数 `decide()`。
- `server/src/oauth/middleware.rs`（新規）: 認証ミドルウェア（401/検証/email 注入）。
- `server/src/lib.rs`（修正）: `pub mod oauth;`。
- `server/src/harness/authn.rs`（修正）: JWT 撤去、`Authenticator` を email→actor に。
- `server/src/harness/mod.rs`（修正）: `Authenticator::new` 呼び出し・`begin` シグネチャ、secret 読み込み撤去。
- `server/src/rmcp_server.rs`（修正）: `begin` が VerifiedEmail を読む。
- `server/src/config.rs`（修正）: `ActorConfig.email` 追加、`AuthConfig` の JWT フィールド・env 撤去。
- `server/src/main.rs`（修正）: metadata router 追加、MCP nest を認証ミドルウェアで包む、verifier 構築。
- `server/config.cloudrun.toml`（修正）: `[auth]` 撤去、`[[actors]]` に `email` 追加。
- `Dockerfile` / GCP 側は Task 5（Fable）。

---

## Task 1: OAuth 保護リソースメタデータ endpoint

**Files:**
- Create: `server/src/oauth/mod.rs`, `server/src/oauth/metadata.rs`
- Modify: `server/src/lib.rs`（`pub mod oauth;` 追加）, `server/src/main.rs`（router に merge）

**Interfaces:**
- Produces: `oauth::metadata::metadata_router(public_host: String, project_ids: Vec<String>) -> axum::Router`
- Produces: `oauth::VerifiedEmail(pub String)`（Clone）— Task 3/4 が使用
- Produces: `oauth::AuthError`（`Missing` / `Invalid(String)` / `Unreachable(String)`）— Task 2/3 が使用

- [ ] **Step 1: `oauth/mod.rs` の型を作る（失敗テスト先行）**

`server/src/oauth/mod.rs`:
```rust
pub mod metadata;

/// 認証ミドルウェアが request extensions に注入する、検証済み Google email。
/// 下流の Harness::begin がこれを actor 表に突合する。
#[derive(Debug, Clone)]
pub struct VerifiedEmail(pub String);

/// Google トークン検証の失敗分類。ミドルウェアが HTTP ステータスへ写像する。
#[derive(Debug)]
pub enum AuthError {
    /// Bearer が無い → 401 + WWW-Authenticate
    Missing,
    /// 検証失敗（無効/期限切れ/aud不一致/email未検証）→ 401 + WWW-Authenticate
    Invalid(String),
    /// Google 到達不能 → 503
    Unreachable(String),
}
```

`server/src/lib.rs` に `pub mod oauth;` を追加（既存 `pub mod health;` の並び）。

- [ ] **Step 2: metadata ハンドラの失敗テストを書く**

`server/src/oauth/metadata.rs` の `#[cfg(test)]`（health.rs のテストと同形・`tower::ServiceExt::oneshot` 使用）:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn path_scoped_metadata_returns_resource_and_google_as() {
        let app = metadata_router("cs-support.example.com".to_string(), vec!["urtect".to_string()]);
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
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["resource"], "https://cs-support.example.com/urtect/mcp");
        assert_eq!(json["authorization_servers"][0], "https://accounts.google.com");
    }
}
```

- [ ] **Step 3: テスト失敗を確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test --lib metadata`
Expected: コンパイルエラー（`metadata_router` 未定義）

- [ ] **Step 4: metadata 実装**

`server/src/oauth/metadata.rs`:
```rust
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
```
> 注: axum 0.8 のパスパラメータ構文は `{project_id}`（`:project_id` ではない）。`_project_ids` は将来の allowlist 用に受けるが現状未使用（YAGNI: 任意 project_id を反射）。

- [ ] **Step 5: main.rs に merge**

`server/src/main.rs` の health router 構築（77行目付近）を次に変更:
```rust
    let public_host = env::var("CS_SUPPORT_PUBLIC_DOMAIN").unwrap_or_default();
    let project_ids: Vec<String> = config.projects.iter().map(|p| p.project_id.clone()).collect();
    let mut app = cs_support_mcp::health::health_router()
        .merge(cs_support_mcp::oauth::metadata::metadata_router(public_host, project_ids))
        .layer(TraceLayer::new_for_http());
```

- [ ] **Step 6: テスト・fmt・check**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test --lib metadata && cargo fmt && cargo check --bins`
Expected: test pass / fmt clean / check Finished

- [ ] **Step 7: Commit**
```bash
git add server/src/oauth server/src/lib.rs server/src/main.rs
git commit -m "feat(oauth): serve RFC 9728 protected-resource metadata (Google as AS)"
```

---

## Task 2: Google トークン検証器（introspection + キャッシュ）

**Files:**
- Create: `server/src/oauth/verifier.rs`
- Modify: `server/src/oauth/mod.rs`（`pub mod verifier;`）

**Interfaces:**
- Consumes: `oauth::AuthError`
- Produces: `oauth::verifier::GoogleTokenVerifier::new(client_id: String) -> Self`
- Produces: `async fn GoogleTokenVerifier::verify(&self, bearer_token: &str) -> Result<String /*email*/, AuthError>`
- Produces（内部・単体テスト対象）: `fn decide(info: &TokenInfo, expected_client_id: &str) -> Result<String, AuthError>`

- [ ] **Step 1: 純粋判定 `decide()` の失敗テストを書く**

`server/src/oauth/verifier.rs` の `#[cfg(test)]`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn info(aud: &str, verified: &str, email: Option<&str>) -> TokenInfo {
        TokenInfo {
            aud: Some(aud.to_string()),
            azp: None,
            email: email.map(ToString::to_string),
            email_verified: Some(verified.to_string()),
        }
    }

    #[test]
    fn accepts_matching_aud_and_verified_email() {
        let got = decide(&info("client-123", "true", Some("a@sivira.co")), "client-123").unwrap();
        assert_eq!(got, "a@sivira.co");
    }

    #[test]
    fn rejects_aud_mismatch() {
        let err = decide(&info("other", "true", Some("a@sivira.co")), "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    #[test]
    fn rejects_unverified_email() {
        let err = decide(&info("client-123", "false", Some("a@sivira.co")), "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    #[test]
    fn rejects_missing_email() {
        let err = decide(&info("client-123", "true", None), "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    #[test]
    fn accepts_when_azp_matches_even_if_aud_differs() {
        let mut i = info("aud-other", "true", Some("a@sivira.co"));
        i.azp = Some("client-123".to_string());
        assert_eq!(decide(&i, "client-123").unwrap(), "a@sivira.co");
    }
}
```

- [ ] **Step 2: テスト失敗を確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test --lib verifier`
Expected: コンパイルエラー（`decide`/`TokenInfo` 未定義）

- [ ] **Step 3: verifier 実装**

`server/src/oauth/verifier.rs`:
```rust
use super::AuthError;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TOKENINFO_URL: &str = "https://oauth2.googleapis.com/tokeninfo";
const MAX_CACHE_TTL: Duration = Duration::from_secs(300);

/// Google tokeninfo のレスポンス（フィールドは文字列で返る）。
#[derive(Debug, Deserialize)]
pub(crate) struct TokenInfo {
    pub aud: Option<String>,
    pub azp: Option<String>,
    pub email: Option<String>,
    pub email_verified: Option<String>,
}

/// tokeninfo 応答から email を導く純粋関数（ネットワーク非依存・単体テスト対象）。
pub(crate) fn decide(info: &TokenInfo, expected_client_id: &str) -> Result<String, AuthError> {
    let aud_ok = info.aud.as_deref() == Some(expected_client_id)
        || info.azp.as_deref() == Some(expected_client_id);
    if !aud_ok {
        return Err(AuthError::Invalid("token audience does not match this client".into()));
    }
    if info.email_verified.as_deref() != Some("true") {
        return Err(AuthError::Invalid("email is not verified".into()));
    }
    let email = info
        .email
        .clone()
        .ok_or_else(|| AuthError::Invalid("token has no email claim".into()))?;
    Ok(email)
}

pub struct GoogleTokenVerifier {
    client_id: String,
    http: reqwest::Client,
    tokeninfo_url: String,
    cache: Mutex<HashMap<String, (String, Instant)>>,
}

impl GoogleTokenVerifier {
    pub fn new(client_id: String) -> Self {
        Self {
            client_id,
            http: reqwest::Client::new(),
            tokeninfo_url: TOKENINFO_URL.to_string(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Bearer を Google tokeninfo で検証し email を返す。トークン単位で最大 5 分キャッシュ。
    pub async fn verify(&self, bearer_token: &str) -> Result<String, AuthError> {
        if let Some(email) = self.cached(bearer_token) {
            return Ok(email);
        }
        let resp = self
            .http
            .get(&self.tokeninfo_url)
            .query(&[("access_token", bearer_token)])
            .send()
            .await
            .map_err(|e| AuthError::Unreachable(format!("tokeninfo request failed: {e}")))?;
        if resp.status() == reqwest::StatusCode::BAD_REQUEST
            || resp.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err(AuthError::Invalid("token rejected by Google tokeninfo".into()));
        }
        if !resp.status().is_success() {
            return Err(AuthError::Unreachable(format!(
                "tokeninfo returned {}",
                resp.status()
            )));
        }
        let info: TokenInfo = resp
            .json()
            .await
            .map_err(|e| AuthError::Unreachable(format!("tokeninfo parse failed: {e}")))?;
        let email = decide(&info, &self.client_id)?;
        self.store(bearer_token, &email);
        Ok(email)
    }

    fn cached(&self, token: &str) -> Option<String> {
        let cache = self.cache.lock().unwrap();
        cache
            .get(token)
            .filter(|(_, exp)| *exp > Instant::now())
            .map(|(email, _)| email.clone())
    }

    fn store(&self, token: &str, email: &str) {
        let mut cache = self.cache.lock().unwrap();
        cache.insert(token.to_string(), (email.to_string(), Instant::now() + MAX_CACHE_TTL));
    }
}
```

`server/src/oauth/mod.rs` に `pub mod verifier;` を追加。

- [ ] **Step 4: テスト pass 確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test --lib verifier`
Expected: 5 tests pass

- [ ] **Step 5: fmt・check・Commit**
```bash
cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo fmt && cargo check --bins
git add server/src/oauth
git commit -m "feat(oauth): Google tokeninfo verifier with aud/email checks and TTL cache"
```

---

## Task 3: 認証ミドルウェア（401/検証/email 注入）

**Files:**
- Create: `server/src/oauth/middleware.rs`
- Modify: `server/src/oauth/mod.rs`（`pub mod middleware;`）, `server/src/main.rs`（各 `/{project}/mcp` を包む・verifier 構築）

**Interfaces:**
- Consumes: `oauth::verifier::GoogleTokenVerifier`, `oauth::VerifiedEmail`, `oauth::AuthError`
- Produces: `oauth::middleware::AuthState { verifier: std::sync::Arc<GoogleTokenVerifier>, resource_metadata_url: String }`（Clone）
- Produces: `async fn oauth::middleware::require_google_auth(State<AuthState>, Request, Next) -> Response`

- [ ] **Step 1: ミドルウェアの失敗テストを書く**

`server/src/oauth/middleware.rs` の `#[cfg(test)]`（トークン無し→401＋ヘッダ を検証。verify は Google 実 endpoint に触れないよう、トークン欠如ケースのみを HTTP レベルで確認する）:
```rust
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
            verifier: Arc::new(crate::oauth::verifier::GoogleTokenVerifier::new("client-x".into())),
            resource_metadata_url: "https://h/.well-known/oauth-protected-resource/urtect/mcp".into(),
        };
        Router::new()
            .route("/urtect/mcp", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_google_auth))
    }

    #[tokio::test]
    async fn missing_bearer_yields_401_with_www_authenticate() {
        let res = app()
            .oneshot(Request::builder().uri("/urtect/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let wa = res.headers().get(header::WWW_AUTHENTICATE).unwrap().to_str().unwrap();
        assert!(wa.contains("resource_metadata="));
        assert!(wa.contains("/.well-known/oauth-protected-resource/urtect/mcp"));
    }
}
```

- [ ] **Step 2: テスト失敗を確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test --lib middleware`
Expected: コンパイルエラー（`AuthState`/`require_google_auth` 未定義）

- [ ] **Step 3: ミドルウェア実装**

`server/src/oauth/middleware.rs`:
```rust
use super::verifier::GoogleTokenVerifier;
use super::{AuthError, VerifiedEmail};
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{extract::Request, middleware::Next};
use std::sync::Arc;

#[derive(Clone)]
pub struct AuthState {
    pub verifier: Arc<GoogleTokenVerifier>,
    /// この保護リソースのメタデータ URL（401 の WWW-Authenticate に載せる）。
    pub resource_metadata_url: String,
}

/// `/{project_id}/mcp` を包む認証ミドルウェア。
/// Bearer 無し/検証失敗 → 401 + WWW-Authenticate（Claude の OAuth 発見トリガ）。
/// Google 到達不能 → 503。成功 → 検証済み email を extensions に注入して次へ。
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
        Some(t) => t,
        None => {
            tracing::info!(reason = "missing_bearer", "auth rejected");
            return unauthorized(&state.resource_metadata_url);
        }
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
        Err(AuthError::Missing) => unauthorized(&state.resource_metadata_url),
    }
}

fn unauthorized(resource_metadata_url: &str) -> Response {
    let mut res = StatusCode::UNAUTHORIZED.into_response();
    let value = format!("Bearer resource_metadata=\"{resource_metadata_url}\"");
    if let Ok(hv) = HeaderValue::from_str(&value) {
        res.headers_mut().insert(header::WWW_AUTHENTICATE, hv);
    }
    res
}
```

`server/src/oauth/mod.rs` に `pub mod middleware;` を追加。

- [ ] **Step 4: main.rs で MCP nest を包む・verifier 構築**

`server/src/main.rs`:
- `use std::sync::Arc;` は既存。verifier を 1 個作り Arc 共有:
```rust
    let google_client_id = env::var("CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID")
        .context("CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID is required for OAuth")?;
    let verifier = Arc::new(cs_support_mcp::oauth::verifier::GoogleTokenVerifier::new(google_client_id));
    let public_host_for_mw = env::var("CS_SUPPORT_PUBLIC_DOMAIN").unwrap_or_default();
```
- projects ループ内、`nest_service` を認証ミドルウェアで包む:
```rust
        let auth_state = cs_support_mcp::oauth::middleware::AuthState {
            verifier: verifier.clone(),
            resource_metadata_url: format!(
                "https://{public_host_for_mw}/.well-known/oauth-protected-resource/{}/mcp",
                project.project_id
            ),
        };
        let guarded = axum::Router::new()
            .fallback_service(mcp)
            .layer(axum::middleware::from_fn_with_state(auth_state, cs_support_mcp::oauth::middleware::require_google_auth));
        let path = format!("/{}/mcp", project.project_id);
        app = app.nest_service(&path, guarded);
```
> 注: `nest_service` に渡すため、mcp サービスを `Router::fallback_service` で包んでからミドルウェア layer を付ける（axum 0.8 で Service にミドルウェアを合成する定石）。kaneko は既存の `nest_service(&path, mcp)` を上記へ置換する。

- [ ] **Step 5: テスト・fmt・check**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test --lib middleware && cargo fmt && cargo check --bins`
Expected: test pass / fmt clean / check Finished

- [ ] **Step 6: Commit**
```bash
git add server/src/oauth server/src/main.rs
git commit -m "feat(oauth): HTTP auth middleware returning 401+WWW-Authenticate and injecting verified email"
```

---

## Task 4: email→actor 突合・begin 接続・静的 JWT 撤去

**Files:**
- Modify: `server/src/config.rs`（`ActorConfig.email` 追加、`AuthConfig` の JWT フィールド・env 撤去）
- Modify: `server/src/harness/authn.rs`（JWT 撤去、email→actor）
- Modify: `server/src/harness/mod.rs`（`Authenticator::new` 呼び出し・`begin` シグネチャ・secret 読み込み撤去・テストビルダーの ActorConfig）
- Modify: `server/src/rmcp_server.rs`（`begin` が VerifiedEmail を読む）
- Modify: `server/config.cloudrun.toml`（`[auth]` 撤去・`[[actors]]` に email）

**Interfaces:**
- Consumes: `oauth::VerifiedEmail`
- Produces: `authn::Authenticator::new(actors: &[ActorConfig]) -> Self`
- Produces: `authn::Authenticator::lookup_by_email(&self, email: &str) -> Result<Actor>`
- Produces: `Harness::begin(&self, email: &str, project_schema: &str, project_manual_schema: ManualSchemaKind) -> Result<RequestContext>`

- [ ] **Step 1: config に email を足す失敗テスト（authn の email 突合テスト）を書く**

`server/src/harness/authn.rs` のテストモジュールを email ベースに置換:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ActorConfig;

    fn actors() -> Vec<ActorConfig> {
        vec![ActorConfig {
            sub: "op-001".to_string(),
            email: "op@sivira.co".to_string(),
            role: "operator".to_string(),
            allowed_schemas: vec!["urtect".to_string()],
        }]
    }

    #[test]
    fn known_email_resolves_actor() {
        let a = Authenticator::new(&actors());
        let actor = a.lookup_by_email("op@sivira.co").unwrap();
        assert_eq!(actor.sub, "op-001");
        assert_eq!(actor.role, Role::Operator);
    }

    #[test]
    fn unknown_email_is_rejected() {
        let a = Authenticator::new(&actors());
        assert!(a.lookup_by_email("stranger@example.com").is_err());
    }
}
```

- [ ] **Step 2: テスト失敗を確認**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test --lib authn`
Expected: コンパイルエラー（`ActorConfig.email` 無し・`lookup_by_email` 無し・`Authenticator::new` シグネチャ不一致）

- [ ] **Step 3: `config.rs` を変更**

`ActorConfig` に `email` を追加:
```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ActorConfig {
    pub sub: String,
    pub email: String,
    pub role: String,
    pub allowed_schemas: Vec<String>,
}
```
`AuthConfig` と、その env 上書きを撤去する:
- `AuthConfig` struct（97-105行目）を削除。
- `AppConfig` の `#[serde(default)] pub auth: AuthConfig,`（38行目）フィールドを削除。
- `AppConfig::load` の `CS_SUPPORT_JWT_SECRET_FILE` env 上書きブロック（242-244行目）を削除。
- `read_secret_file`（14-22行目）は llm.rs が使うため**残す**。

- [ ] **Step 4: `authn.rs` を email→actor に置換**

`server/src/harness/authn.rs` 全体を次へ（`Claims`・`decode`・`jsonwebtoken`・`decoding_key`・`issuer`・`default_actor` を撤去。`Role`・`Actor` は維持）:
```rust
use crate::config::ActorConfig;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Operator,
    Supervisor,
    Admin,
}

impl std::str::FromStr for Role {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "operator" => Ok(Role::Operator),
            "supervisor" => Ok(Role::Supervisor),
            "admin" => Ok(Role::Admin),
            other => bail!("unknown actor role: {other}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Actor {
    pub sub: String,
    pub role: Role,
    pub allowed_schemas: Vec<String>,
}

/// email → actor の突合（認可の正本＝config actor 表。ホワイトリスト）。
/// AuthN(誰か)は Google OAuth ミドルウェアが済ませ、ここは email→role/scope の導出のみ。
// [撤去済み, 2026-07-21] この実装（email ホワイトリスト・fail-closed 拒否）は commit 1809b8e で
// email ホワイトリストごと撤去され、現行実装（server/src/harness/authn.rs）とは異なる。
// このコード例は計画時点の履歴として残す。
pub struct Authenticator {
    actors_by_email: HashMap<String, ActorConfig>,
}

impl Authenticator {
    pub fn new(actors: &[ActorConfig]) -> Self {
        Self {
            actors_by_email: actors.iter().map(|a| (a.email.clone(), a.clone())).collect(),
        }
    }

    /// 検証済み email を actor に写す。未登録 email は fail-closed で拒否。
    pub fn lookup_by_email(&self, email: &str) -> Result<Actor> {
        let config = self
            .actors_by_email
            .get(email)
            .ok_or_else(|| anyhow!("actor not registered for email: {email}"))?;
        Ok(Actor {
            sub: config.sub.clone(),
            role: config.role.parse()?,
            allowed_schemas: config.allowed_schemas.clone(),
        })
    }
}
```

- [ ] **Step 5: `harness/mod.rs` を変更**

- secret 読み込み（107-117行目）を削除。
- Authenticator 構築（136-141行目）を `authn::Authenticator::new(&config.actors),` に置換（`.with_issuer(...)`・`default_actor` 除去）。
- `begin`（417-432行目）のシグネチャと本体を email ベースに:
```rust
    pub fn begin(
        &self,
        email: &str,
        project_schema: &str,
        project_manual_schema: crate::config::ManualSchemaKind,
    ) -> Result<RequestContext> {
        let actor = self.authenticator.lookup_by_email(email)?;
        let access = scope::resolve_scope(&actor, project_schema)?;
        Ok(RequestContext {
            schema: access.enforced_schema().to_string(),
            actor,
            scope: access,
            request_id: uuid::Uuid::new_v4().to_string(),
            manual_schema: project_manual_schema,
        })
    }
```
- テストビルダー（856-869行目）の `Authenticator::new(...)` を `authn::Authenticator::new(&[ActorConfig { sub: "op-001".into(), email: "op@sivira.co".into(), role: "operator".into(), allowed_schemas: vec!["sivira-cs-demo".into()] }])` に置換（`None`/`default_actor` 引数を除去）。この harness_for_test を使う既存テストが `begin(...)` を呼ぶ箇所は、第1引数を `"op@sivira.co"` に変更する。

- [ ] **Step 6: `rmcp_server.rs` の `begin` を VerifiedEmail 読取に変更**

`server/src/rmcp_server.rs`（280-289行目）:
```rust
    fn begin(&self, extensions: &rmcp::model::Extensions) -> Result<RequestContext, ErrorData> {
        let email = extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<crate::oauth::VerifiedEmail>())
            .map(|v| v.0.clone());
        let email = email
            .ok_or_else(|| ErrorData::invalid_request("unauthenticated".to_string(), None))?;
        self.harness
            .begin(&email, &self.schema, self.manual_schema)
            .map_err(|err| ErrorData::invalid_request(err.to_string(), None))
    }
```
> 各 tool ハンドラ（`self.begin(&extensions)?` の 12 箇所）は無改変。

- [ ] **Step 7: `config.cloudrun.toml` を更新**

- `[auth]` セクション（default_actor コメント含む）を削除。
- `[[actors]]` に `email` を追加（少なくとも自分を supervisor で登録）:
  > **[撤去済み, 2026-07-21]** 以下の `[[actors]]` email ホワイトリスト運用は commit `1809b8e` で撤去済み。現行実装とは異なる（履歴として残す）。
```toml
[[actors]]
sub = "op-001"
email = "op@sivira.co"
role = "operator"
allowed_schemas = ["sivira-cs-demo", "urtect"]

[[actors]]
sub = "sup-001"
email = "ryugo@sivira.co"
role = "supervisor"
allowed_schemas = ["sivira-cs-demo", "urtect"]
```
> 初期ホワイトリスト（email と role）は Fable が Task 5 で最終確定。

- [ ] **Step 8: 全テスト・check・fmt**

Run: `cd server && env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo test --lib && cargo check --bins && cargo fmt --check`
Expected: 全 pass / Finished / fmt clean。`jsonwebtoken` 依存が未使用になるため `cargo check` の warning を確認し、`Cargo.toml` から `jsonwebtoken = "9"` を削除（Global Constraints の「依存を増やさない」に対応し、不要依存は減らす）。削除後 `cargo check --bins` 再実行。

- [ ] **Step 9: Commit**
```bash
git add server/src/harness/authn.rs server/src/harness/mod.rs server/src/rmcp_server.rs server/src/config.rs server/config.cloudrun.toml server/Cargo.toml server/Cargo.lock
git commit -m "feat(auth): replace static JWT with Google OAuth email->actor whitelist; wire begin to verified email"
```

---

## Task 5（Fable・gcloud/コンソール）: GCP OAuth セットアップとデプロイ

> kaneko 実装ではない。Fable が gcloud/コンソールで実施し、Dockerfile/デプロイを更新する。

1. `sivira-cs-support` に **OAuth 同意画面**を構成: External・テスト公開、scope `openid email profile`、テストユーザに許可 Gmail を登録。
   > **[現況と相違, 2026-07-21]** Google 同意画面は本日（2026-07-21）「テスト公開」ではなく「本番（External 公開）」に切替済み。任意の Google アカウントがこの経路に到達しうる。テストユーザ限定という前提はもう成立しない。
2. **OAuth 2.0 クライアント ID（Web application）**を発行。認可済みリダイレクト URI に Claude のコールバック URL を登録（実値は claude.ai のコネクタ追加画面で確認）。
3. Client ID/Secret を Secret Manager に格納（`cs-support-google-oauth-client`）。Cloud Run に env `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID`（= Client ID）を設定。
4. 旧 JWT secret を撤去: Cloud Run から `CS_SUPPORT_JWT_SECRET_FILE` env と `cs-support-jwt-secret` の volume/secret mount を外す。`Dockerfile` の該当記述があれば除去。
5. 新イメージを `gcloud builds submit` → `gcloud run services update`（service）→ `gcloud run jobs update ingest-rules/ingest-urtect`（ingest は認証非依存だが同イメージに揃える）。
6. `curl` で `/.well-known/oauth-protected-resource/urtect/mcp` が JSON を返し、無トークンの `/urtect/mcp` が 401＋`WWW-Authenticate` を返すことを確認。

---

## Task 6（Fable）: claude.ai コネクタ E2E・ドキュメント・PR

1. claude.ai で **カスタムコネクタ追加**: URL=`https://<public-host>/urtect/mcp`、詳細設定に Google の Client ID/Secret を入力 → **Gmail ログイン** → 接続確立を確認。
2. 実利用 E2E: `evaluate_answerability`（operator/supervisor）と `add_known_resolution`（supervisor のみ通過・operator は権限エラー）を Claude 上で実行し、ノウハウ保存＋監査が効くことを確認。
3. プロジェクト `CLAUDE.md` の起動/接続 runbook を OAuth コネクタ手順に更新。`.mcp.json`（静的 JWT ヘッダ）と `.env` の `CS_SUPPORT_MCP_TOKEN` は撤去または OAuth 前提へ更新。`specs/production-cs-mcp.md` に AuthN=Google OAuth を反映。
4. PR 作成（push はユーザ）→ `code-review` フロー（reviewer → codex → Copilot）。

---

## 自己レビュー結果

- **spec coverage:** spec §3.1→Task1, §3.3→Task2, §3.2→Task3, §3.4/§5(JWT撤去)→Task4, §4(GCP)→Task5, §5.5/§7 E2E→Task6。全カバー。
- **placeholder scan:** 実装ステップは実コード掲載。Task5/6 は Fable の ops で TDD 対象外のため手順記述（許容）。redirect URL 実値のみ外部依存で Task6 手順内に確認ステップあり。
- **type consistency:** `VerifiedEmail(pub String)` / `AuthError{Missing,Invalid,Unreachable}` / `GoogleTokenVerifier::new(String)`/`verify(&str)->Result<String,AuthError>` / `Authenticator::new(&[ActorConfig])`/`lookup_by_email(&str)->Result<Actor>` / `Harness::begin(&str,&str,ManualSchemaKind)` は全タスクで一貫。`ActorConfig{sub,email,role,allowed_schemas}` は Task4 で定義し authn/mod テストで同形使用。
