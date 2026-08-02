# DCR の redirect_uri を許可リスト化し、同意画面を削除する

## 背景

`server/src/oauth/authserver.rs` の `is_acceptable_redirect_uri` は、スキームが `https` であればホストを一切見ずに `true` を返す。

```rust
match parsed.scheme() {
    "https" => true,
    "http" => matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]" | "::1")),
    _ => false,
}
```

`POST /oauth/register`（DCR）は無認証なので、第三者が `https://attacker.example/cb` を redirect_uri として登録できる。登録値は client_id に埋め込まれ、`/oauth/callback` の最後でそこへ認可コードがリダイレクトされる。Google 側の redirect_uri 登録は「Google → こちらの `/oauth/callback`」の1段目しか守っておらず、「こちら → 動的登録クライアント」の2段目はこのコードが決めている。

このため、攻撃者が自分の redirect_uri で登録して利用者に authorize URL を踏ませると、認可コードが攻撃者へ配送され、PKCE verifier も攻撃者が保持しているためトークンに交換できる（confused deputy）。

この穴を塞ぐために `/oauth/consent`（自前の同意画面）を入れたが、**redirect_uri を許可リスト化すれば経路自体が消えるため、同意画面は不要になる**。

## 方針

GCP のみで完結させる。外部 IdP（Auth0 等）は導入しない。OAuth AS を兼ねる現行構成は維持する。

## やること

### 1. redirect_uri の許可リスト化

`server/src/oauth/authserver.rs` の `is_acceptable_redirect_uri` を、スキームだけでなく**ホストで判定**する形に変更する。

- 本番で許可するのは claude.ai のコールバックのみ
- ローカル開発用に `http://localhost` / `http://127.0.0.1` は残す（ポートは任意で可）
- 上記以外は拒否する
- 判定を**ホストの完全一致**で行うこと。サフィックス一致（`ends_with("claude.ai")`）にしないこと。`evil-claude.ai` のような値が通るため

claude.ai のコールバック URL の実値は、本番の DCR リクエストで実際に送られてくる値を正とする。リポジトリ内に確定値の記録が無い場合は、**推測で決め打ちせず、判定対象のホストを定数として1箇所にまとめ、値が変わったときに1行で直せる形にすること**。現時点で判明している値は `https://claude.ai/api/mcp/auth_callback`。

### 2. 同意画面の削除

`/oauth/consent` と関連するものを削除する。

- ルート定義、`consent` ハンドラ、`render_consent_page`
- `Blob::Consent` とその署名・封緘まわり
- 同意画面用のヘッダ（`X-Frame-Options` / CSP）と HTML エスケープのヘルパは、他で使っていなければ削除してよい
- `callback` は Google の identity 確定後、**そのまま認可コードを発行してクライアントの redirect_uri へリダイレクトする**形に戻す

### 3. 維持すること

削除・変更してはいけないもの。

- DCR（`/oauth/register`）、`/oauth/authorize`、`/oauth/callback`、`/oauth/token` の構成
- Google のトークンを中継する方式（自前のアクセストークン発行はしない）
- リクエスト経路の `GoogleTokenVerifier`（tokeninfo）による検証
- PKCE の S256 強制と `code_challenge` の束縛
- state と認可コードの単回使用（`consume_jti`）
- state と認可コードの AEAD 封緘（Google のトークンと PKCE verifier を運ぶため）
- 秘密（トークン・code・client_secret・署名鍵）をログ・エラー・`Debug` に出さない実装
- 一時障害（429 / 408 / 5xx / 到達不能）を `invalid_grant` として返さないエラー分類
- 署名鍵を起動時に CSPRNG で生成する方式（env / Secret Manager を使わない）

### 4. ドキュメント追随

`CLAUDE.md` / `README.md` / `specs/production-cs-mcp.md` から、同意画面に関する記述を削除する。redirect_uri を許可リストで縛っていること、およびその理由（2段目のリダイレクトは Google の設定では守られない）を記載する。

## やらないこと

- 外部 IdP の導入
- email ホワイトリストによるアクセス制限（別タスク）
- `Authenticator::lookup_by_identity` の変更（引き続き検証済み ID を無条件に supervisor へ解決する）

## 完了条件

- `https://claude.ai/api/mcp/auth_callback` での DCR が成功する
- `https://attacker.example/cb` のような第三者ホストでの DCR が**拒否される**
- `http://localhost:<任意ポート>` での DCR が成功する
- `https://evil-claude.ai/cb` が**拒否される**（サフィックス一致になっていないこと）
- `/oauth/consent` が存在しない
- `cargo test --manifest-path server/Cargo.toml` が通る
- `cargo check --manifest-path server/Cargo.toml --all-targets` と `cargo fmt --manifest-path server/Cargo.toml -- --check` が通る

## テスト

- 許可ホストでの登録が成功する
- 第三者ホストでの登録が拒否される
- サフィックス一致で通らないこと（`evil-claude.ai`）
- localhost が許可される
- 同意画面を経ずに callback が認可コードを発行し、クライアントの redirect_uri へリダイレクトする
- 認可コードの単回使用と PKCE 束縛が維持されている（既存テストが緑のまま）
