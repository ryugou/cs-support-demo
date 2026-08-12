# cs-support-mcp OAuth コネクタ化 設計

> **[supersession 注記, 2026-07-21]** 本ドキュメント §3.4 の email→actor ホワイトリスト運用（config `[[actors]]` の email 事前登録による fail-closed 拒否）は commit `1809b8e`（`feat(authn): remove config-based email whitelist, fail-open to supervisor actor`）で撤去済み。現行の認可設計は `specs/production-cs-mcp.md` の「AuthN 現状」節（2026-07-21 更新）を正とする。本ドキュメントは設計検討時点の履歴として保持する。

**日付:** 2026-07-20
**対象:** cs-support-mcp（Cloud Run 稼働・Rust axum + rmcp）
**目的:** claude.ai / Claude Desktop の「カスタムコネクタ追加」UI から、CS 担当者が **Gmail ログイン**で cs-support-mcp を接続・利用できるようにする。認可サーバ(AS)は Google（GCP の OAuth 同意画面＋OAuth クライアント）とし、cs-support-mcp を **OAuth リソースサーバ(RS)** 化する。**認証は Google OAuth 一本**（静的 JWT は廃止）。第三者 IdP・追加 SaaS は使わない。

---

## 1. 背景と現状

- cs-support-mcp は Cloud Run で稼働済み。公開 URL: `https://cs-support-mcp-235108918288.asia-northeast1.run.app/{project}/mcp`（テナント: `urtect`(主), `sivira-cs-demo`(legacy)）。
- 現状の認証は**静的 Bearer JWT 必須**（HS256・`sub`/`role` claims → config actor 表で actor 確定。`server/src/harness/authn.rs`）。
- この静的 JWT 方式は Claude Code の `.mcp.json` や API では使えるが、**claude.ai / Desktop の UI コネクタ追加では使えない**（UI は OAuth メタデータを発見して OAuth 2.1(PKCE) フローでトークン取得するため、静的トークンの手入力口が無い）。
- **本設計で静的 JWT 経路は撤去し、Google OAuth に一本化する**（移行順は §5・実装計画で担保し、無認証の隙を作らない）。

### 確認済みの事実（調査結果）

- MCP 認可は OAuth 2.1 準拠が MUST。**DCR(RFC 7591) は SHOULD で必須ではない**。DCR 非対応 AS は事前登録 client_id で接続可能。**PKCE(S256) はクライアント側 MUST**。
- **Google の OAuth は公開 DCR 非対応**。よって Google を AS にする場合、Claude 側に**事前登録した client_id/secret を渡す**（claude.ai カスタムコネクタの「詳細設定」の OAuth Client ID/Secret 入力欄に設定）。
- RS は **RFC 9728 `/.well-known/oauth-protected-resource` を実装 MUST**、401 で **`WWW-Authenticate: Bearer resource_metadata=...`** を返す MUST。

---

## 2. 全体構成

```
CS担当(ブラウザ/Claude)                claude.ai / Desktop
      │  Gmailでログイン                     │  カスタムコネクタ追加(URL + Client ID/Secret)
      ▼                                       ▼
  Google OAuth (AS)  ◀── authorize/token/consent ──  Claude(OAuthクライアント)
      │  アクセストークン発行                          │  取得したBearerをMCPへ
      ▼                                                ▼
                          cs-support-mcp (Resource Server, Cloud Run)
                          - /.well-known/oauth-protected-resource を公開
                          - 401 に WWW-Authenticate: Bearer resource_metadata=…
                          - Google Bearer を introspect → email 取得 + aud=自client確認
                          - email → actor/role/allowed_schemas を server 導出(AuthZ)
                                     │ gRPC(VPC peering)
                                     ▼
                             vegapunk (graph backend, 10.10.0.2:6840)
```

- **AS = Google**（`https://accounts.google.com`）。GCP プロジェクト `sivira-cs-support` に OAuth 同意画面と OAuth 2.0 Web クライアントを作成。ログイン・同意画面・トークン発行はすべて Google が担う。
- **RS = cs-support-mcp**。トークン検証と、そこから actor/role/allowed_schemas を **server 導出**して AuthZ・scope・回答可否・エスカレーション判定を行う中核は現状のまま MCP 内に保持（spec `production-cs-mcp.md` の「外部 Harness に認可を委譲しない」原則を維持）。OAuth はあくまで **AuthN（誰か）** の委譲。

---

## 3. コンポーネント（cs-support-mcp 側の変更）

すべて `server/` 配下。実装は kaneko、レビューは reviewer。Rust(axum + rmcp)。Python/TypeScript 禁止。

### 3.1 保護リソースメタデータ endpoint（新規ルート）

- `GET /.well-known/oauth-protected-resource` と、テナントパス対応版 `GET /.well-known/oauth-protected-resource/{project_id}/mcp`（Claude は path 付きを先に見に行くため両方用意）。
- 返す JSON（RFC 9728）:
  ```json
  {
    "resource": "https://<public-host>/{project_id}/mcp",
    "authorization_servers": ["https://accounts.google.com"]
  }
  ```
- `main.rs` の Router に `.route(...)` を追加（`/livez` と同じ要領）。静的ファイルではなくハンドラ。
- public host は既存 env `CS_SUPPORT_PUBLIC_DOMAIN` から解決。

### 3.2 認証は HTTP ミドルウェアで実施（401 の発見トリガを成立させる）

**重要:** Claude の OAuth 発見フローには **HTTP 401 + `WWW-Authenticate`** が要る。現状の認証は各 tool ハンドラ内（`Harness::begin`）で JSON-RPC エラー（HTTP 200）を返すため 401 にならない。よって**認証を `/{project_id}/mcp` を包む axum ミドルウェアに移す**:

- ミドルウェアの責務:
  1. `Authorization: Bearer <token>` を取得。無ければ **401** + `WWW-Authenticate: Bearer resource_metadata="https://<public-host>/.well-known/oauth-protected-resource/{project_id}/mcp"`。
  2. あれば §3.3 の Google introspection で検証。検証失敗（無効/期限切れ/aud 不一致/email 未検証）→ **401**（同ヘッダ）。Google 到達不能 → **503**。
  3. 検証成功なら **検証済み email を request の extensions に注入**して次へ。
- rmcp は HTTP `Parts`（extensions 含む）を JSON-RPC メッセージ extensions に転送するため、ミドルウェアが注入した email は下流ハンドラの `extensions.get::<http::request::Parts>()` 経由で読める。
- これにより **各 tool ハンドラ（`rmcp_server.rs` の 12 箇所）は無改変**。`Harness::begin` は「Authorization ヘッダ」ではなく「注入済み検証 email」を読む形に内部だけ変更（sync 維持）。
- `/.well-known/oauth-protected-resource*`（§3.1）はミドルウェアの外側に置き、未認証で公開する。

### 3.3 Google トークン検証（AuthN の実体）= Google OAuth 一本

`authn.rs` の HS256 静的 JWT 検証・actor 表の `sub` 経路・関連 env/config は撤去し、Google 検証に置換する:

- 受け取った Bearer を **Google introspection** で検証:
  - Google tokeninfo endpoint（`https://oauth2.googleapis.com/tokeninfo?access_token=…`）を `reqwest`(既存依存) で叩く。
  - 検証項目: 有効性・有効期限、`aud`/`azp` が**自 OAuth クライアント ID（env `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID`）と一致**、`email` と `email_verified=true` を取得。
  - 検証結果（email）をトークン単位で短時間キャッシュ（数分）し Google への RTT を抑える。Google への到達は public egress。
- 検証の成否は §3.2 のミドルウェアが 401/503 に写像（fail-closed）。

### 3.4 email → actor マッピング（AuthZ・server 導出・ホワイトリスト）

- config の `[[actors]]` を **email キー中心**に再定義:
  - `sub`（監査で使う安定した内部 actor id・任意の識別子）
  - `email`（Google ログインの照合キー・必須）
  - `role`（operator / supervisor / admin）
  - `allowed_schemas`
- Google 経路で得た検証済み `email` を actor 表と突合して actor を確定。
- **email が actor 表に無ければ拒否**（fail-closed）。＝**ホワイトリスト運用**（許可する Gmail を明示登録。sivira.co 内外を問わず 1 件ずつ role 付きで登録）。
  > **[撤去済み, 2026-07-21]** この email ホワイトリストは commit `1809b8e` で撤去済み。現行実装（`server/src/harness/authn.rs:70-85`）は突合を行わず、検証済み email を無条件に supervisor へ解決する。詳細は `specs/production-cs-mcp.md` の「AuthN 現状」節を参照。
- role→tool 認可（例: `add_known_resolution` は supervisor/admin）は現行ロジックのまま。
- （将来オプション・本スコープ外）「`@sivira.co` ドメインは既定 role で自動許可＋外部は個別ホワイトリスト」も config で拡張可能。今回はホワイトリストのみ。
  > **[撤去済み, 2026-07-21]** ホワイトリスト自体が撤去済みのため、この拡張案も前提が成立しない。

### 3.5 マルチテナント

- OAuth はサーバ共通。resource metadata は `/{project_id}/mcp` ごとに `resource` を出し分け。
- テナントのアクセス可否は actor の `allowed_schemas` で従来どおり制御。urtect / sivira-cs-demo とも同一 OAuth で扱える。

---

## 4. GCP 側セットアップ（Fable / gcloud・コンソール）

1. **OAuth 同意画面**を `sivira-cs-support` に構成。
   - **External**（外部 Gmail もホワイトリストで使えるようにするため）。ログイン可否の実制御は §3.4 の allowlist。
     > **[撤去済み, 2026-07-21]** §3.4 の allowlist は撤去済み（commit `1809b8e`）。現状 External 公開は「ログイン可否の実制御」を allowlist に委ねられていない点に注意。Google 同意画面は本日（2026-07-21）本番（External 公開）へ切替済みで、任意の Google アカウントがログイン自体は通過できる。
   - 起動時は「テスト」公開状態＋許可 Gmail を**テストユーザ**に登録（未審査アプリの警告画面は出るが、`openid email profile` scope のみで Google 審査は不要）。
   - （sivira.co 限定にしたくなった場合のみ Internal に切替。コード不変）。
   - scope は `openid email profile` のみ。
2. **OAuth 2.0 クライアント ID（Web application）**を発行。認可済みリダイレクト URI に **Claude のコールバック URL** を登録（正確な URL はコネクタ追加画面/公式ドキュメントで確定）。
3. 発行した **Client ID / Client Secret** を Secret Manager に格納（`cs-support-google-oauth-client`）。RS は aud 照合に client_id を使う。
4. Cloud Run service に env 追加: `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID`（aud 照合用）。旧 JWT secret（`CS_SUPPORT_JWT_SECRET_FILE` / `cs-support-jwt-secret`）は撤去。

---

## 5. 実装・移行順

現状の唯一のクライアントは動作確認用（本番利用者はまだ居ない）ため、静的 JWT は**併存させず一括で置換**する（`begin` が「Authorization ヘッダ」→「注入済み email」に変わるため二重経路を持たない）:

1. §3.1 resource metadata endpoint を追加（公開・無認証）。
2. §3.3 Google トークン検証器（introspection＋キャッシュ）を実装。
3. §3.2 認証ミドルウェアを実装し `/{project_id}/mcp` を包む。§3.4 の email→actor に `begin` を接続。**同じ実装で静的 JWT 検証・`sub` 経路・`CS_SUPPORT_JWT_SECRET_FILE`/`cs-support-jwt-secret`/`jwt_issuer`/`default_actor` を撤去**。
4. Google OAuth 同意画面・クライアント作成（Fable）→ claude.ai の redirect URI 登録・Secret/env 配線・デプロイ。
5. claude.ai で**カスタムコネクタ追加 → Gmail ログイン** → 全 tool の OAuth 経路 E2E（`evaluate_answerability` / `add_known_resolution`(supervisor)）を確認。

---

## 6. エラーハンドリング / 可観測性

- すべての認証失敗パスに、運用者が次アクションを判断できる情報を tracing に残す（トークン全体はログしない。email・失敗理由の分類のみ）。
- introspection の Google 到達失敗(503) / 検証失敗(401) / allowlist 外(403 or 401) を切り分けてログ。
- 移行中（静的 JWT 併存期間）の挙動は既存テスト green を維持。撤去後は OAuth 経路テストに置換。

---

## 7. テスト方針

- `authn.rs` の Google トークン検証（introspection はモック境界で）: 有効/期限切れ/aud 不一致/email 未取得 の各ケース。
- email→actor 突合（allowlist ヒット / ミス→拒否）の単体テスト。
- resource metadata JSON と 401 `WWW-Authenticate` ヘッダの HTTP レベルテスト（`/livez` テストと同形）。
- 実機 E2E: claude.ai コネクタ追加 → Gmail ログイン → `evaluate_answerability` / `add_known_resolution`(supervisor) が通ること。

---

## 8. スコープ（含む / 含まない）

**含む:** resource metadata / 401 ヘッダ、Google introspection、email→actor(ホワイトリスト)、静的 JWT 経路の撤去、GCP OAuth 同意画面・クライアント作成。

**含まない(YAGNI):**
- 自前 OAuth AS / DCR の実装、ORY Hydra 等（必要になった場合のみ再検討）。
- Google トークンのリフレッシュを RS 側で行うこと（Claude 側が担う）。
- `@sivira.co` ドメイン自動許可（今回はホワイトリストのみ）、ロール昇格 UI・管理画面。
- sivira-cs-demo の再 ingest（本件と独立）。

---

## 9. 確定が必要な残項目（実装中/デプロイ前に確定）

- Claude のコールバック URL の実値（コネクタ追加画面で確認 → Google クライアントに登録）。
- 初期ホワイトリスト（許可 email と role の一覧）。少なくとも `ryugo@sivira.co` を supervisor で登録。
