# CS Support MCP Demo

Rust implementation of a customer-support MCP demo backed by vegapunk GraphRAG.

The demo stores product manuals as graph structure:

- `product`
- `document`
- `section`
- `spec`
- `HAS_DOCUMENT`
- `CONTAINS`
- `REFERENCES`
- `HAS_SPEC`
- `DEFINED_IN`

No issue/trouble-log data is included in the initial demo.

## Production CS MCP Step 1 (Harness)

`specs/production-cs-mcp.md` の Step 1 実装。全 tool が MCP 内 Harness
（AuthN → scope 強制 → 取得 → signal 正規化 → 3 層判定 → egress → WORM 監査）を経由する。

追加された node/edge type（加算のみ）: `KnownResolution` / `Signal` /
`EscalationRule` / `ProhibitedDomain` / `support_case` / `answer_attempt` /
`answer_evidence` / `operator_feedback` / `escalation_event`、
`HAS_SIGNAL`（KnownResolution・support_case → Signal）/ `BECAUSE`（KnownResolution → section）。

Tool 一覧（S1-7 の 12 本）:

- read: `resolve_product` / `search_manual` / `get_section` / `get_product`
- 判定入口: `evaluate_answerability`（マルチターンは返却された `case_id` を引き回す。累積 signal 集合で毎ターン再判定）
- 検索: `search_known_resolutions` / `search_past_cases`
- 記録: `record_answer_attempt`（出口ゲート適用。pass = 担当者へ応答可）/ `record_answer_outcome`（grade 昇格・降格）/ `record_operator_feedback`（訂正インテーク）/ `create_escalation_event`
- 知識追加: `add_known_resolution`（supervisor / admin のみ）

### Actor 認証（Google OAuth 2.1）

`cs-support-mcp` は OAuth 2.1 の**認可サーバ（AS）兼リソースサーバ（RS）**であり、
内部で Google（`accounts.google.com`）に委譲する（OAuth フェデレーション）。

```text
claude.ai --DCR/authorize/token--> cs-support-mcp (AS) --authorize/token--> Google
```

AS を自前化した理由: Google は RFC 7591 の DCR（動的クライアント登録）に対応していない。
認可サーバを Google 自身にすると、claude.ai は接続のたびに利用者へ Google の
Client ID / Secret を手入力させる必要があり、CS 担当者に配れず、かつ Client Secret を
クライアント側に置くことになる。現行は Google の client_id / secret をサーバ側 env に
閉じ込め、claude.ai は MCP endpoint の URL を入れるだけで接続できる。

公開する OAuth endpoint（いずれも無認証）:

| endpoint | 役割 |
| --- | --- |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 AS メタデータ（**200**。旧構成の 404 から反転） |
| `GET /.well-known/oauth-protected-resource/{project_id}/mcp` | RFC 9728 RS メタデータ。`authorization_servers` は**自分自身** |
| `POST /oauth/register` | RFC 7591 DCR。public client + PKCE（`client_secret` は発行しない） |
| `GET /oauth/authorize` | 検証後に Google の同意画面へリダイレクト |
| `GET /oauth/callback` | Google からの戻り。token 交換と identity 確定 → **その場で認可コードを発行し、クライアントの redirect_uri へリダイレクト** |
| `POST /oauth/token` | `authorization_code` / `refresh_token` グラント |

トークン無しで `/{project_id}/mcp` にアクセスすると `401` と
`WWW-Authenticate: Bearer resource_metadata="https://{public_host}/.well-known/oauth-protected-resource/{project_id}/mcp"`
ヘッダを返す（署名不正・期限切れ・種別違い・**別 project 向け**のトークンでも同様に 401）。

#### ログインごとに Google の同意画面が出る（仕様。故障ではない）

利用者が 1 回ログインすると、**Google の同意画面**が毎回表示される。
`/oauth/authorize` が Google へ `prompt=consent` を常に付けてリダイレクトするため、
既に同意済みの利用者にも毎回出る。これは Google の refresh_token を確実に受け取るために
必要である（`access_type=offline` だけでは、同意済み利用者に refresh_token が返らない）。
refresh_token が無いと、下記の「上流失効の伝播」が成立しない。

#### confused deputy 対策は redirect_uri の許可リスト

`POST /oauth/register`（DCR）は無認証で、redirect_uri を無制限に受け付けると攻撃者が
任意ホストの `redirect_uri` を持つクライアントを登録でき、`/oauth/callback` が識別済みの
identity に対する認可コードをその攻撃者へ配送してしまう。Google が見せる同意画面は
「cs-support-mcp」に対するものであって、動的登録された下流クライアントの素性を
一切示さない。

これを防ぐため、`is_acceptable_redirect_uri`（`server/src/oauth/authserver.rs`）が
DCR 登録時点で `redirect_uri` をホストで絞る:

- `https` は `claude.ai` への**ホスト完全一致**でのみ許可する（`ends_with` のような
  サフィックス一致ではない。`evil-claude.ai` のような別ドメインを通さないため）。
- `http` は loopback（`localhost` / `127.0.0.1` / `[::1]`）のみ許可し、ポートは任意。
- それ以外はすべて拒否する。

一時期はこれを cs-support-mcp 自身の同意画面（`/oauth/consent`）で塞いでいたが、
許可リスト導入により配送先が閉じたため撤去した。**このサーバから動的登録クライアントへの
リダイレクトは、Google Cloud Console 側の redirect_uri 登録（`https://<host>/oauth/callback`）
では一切守られない**点に注意すること。守っているのはあくまで
`is_acceptable_redirect_uri` である。詳細:
`docs/superpowers/specs/2026-07-22-restrict-redirect-uri-design.md`。

#### トークンは自前発行しない

一時期この AS は自前のアクセス/リフレッシュトークンを署名付きで発行していたが、デモに対して
寿命・失効・鍵管理を自前で抱える設計が過剰と判断され撤回した。現在は次のとおり。

- `POST /oauth/token` の `authorization_code` グラントは、Google から受け取った
  **access_token / refresh_token / expires_in をそのままクライアントへ返す**。
- `refresh_token` グラントは、クライアントから来た refresh_token を **Google の token
  endpoint へ中継**し、Google の応答をそのまま返す。`client_secret` はサーバ側から添える
  （これを添えられることが、クライアントに secret を持たせずに済ませる唯一の理由である）。
- リクエスト経路の Bearer 検証は Google tokeninfo への照会（`GoogleTokenVerifier`）。

AS が引き受けるのは **DCR の成立**の 1 点だけになった。confused deputy 対策は、上記の
redirect_uri の許可リスト（`is_acceptable_redirect_uri`）で行う。

**失効**: このサーバは失効台帳を持たないため、個別のトークン失効手段が無い。失効は Google 側
（アカウントのアクセス権限管理）で行う。Google 側でアカウント停止・グラント取消が行われると、
リクエスト経路の tokeninfo 照会（キャッシュ TTL 分の遅延あり）とリフレッシュ中継の両方が
拒否に変わる。

> **失われた保証**: 旧実装のアクセストークンは `aud` に project_id を持ち、
> `/{project_id}/mcp` ごとに束縛されていた。Google 発行のトークンにはこの束縛を載せられない
> ため、**ある project 向けに取得したトークンは全 project の endpoint で通る**。
> project を増やす前に認可境界そのものを設計し直すこと。

#### 状態の持ち方と署名鍵

状態は外部ストアに持たず、**署名付きの値そのものに埋め込む**
（Cloud Run がゼロスケールするため。`server/src/oauth/signing.rs` 参照）。

上流の秘密を運ぶブロブ（**state・認可コード**）は署名に加えて
**ChaCha20-Poly1305 で暗号化**する（`SigningKey::seal` / `open`）。認可コードは
Google の access_token / refresh_token を運び、しかもクライアントの redirect_uri へ
**クエリ文字列として**渡る（ブラウザ履歴・Referer・中間ログに残る）。署名だけだと中身は
base64 された平文なので、上流クレデンシャルがそれらすべてに残ることになる。state を含めるのは、
Google の authorize URL に載る `state` の中に AS 用の PKCE verifier が入っており、署名だけだと
ブラウザ履歴・Referer・Google 側ログから読めてしまうため。暗号鍵は署名鍵からドメイン分離付きで
派生させており、追加の env は無い。

**署名鍵に env は無い。** 起動時に OS の CSPRNG から 32 バイトを生成してメモリに保持する
（`SigningKey::generate`）。Secret Manager にも置かない。守る対象が client_id / state /
認可コードに限られ、鍵を運用物として抱える理由が無くなったため。

**再起動時の影響**:

| 対象 | 再起動後 |
| --- | --- |
| 進行中のログインフロー（state / 認可コード、最長 600 秒） | 無効。ログインをやり直す |
| DCR 登録（client_id） | 無効。claude.ai が再登録して**自動的に回復する** |
| アクセストークン / リフレッシュトークン | **影響なし。利用者はログアウトしない** |

最後の行が、自前トークン発行を廃止したことの直接の利点であり、鍵を使い捨てにできる根拠でもある。

**上流失効の伝播**: `refresh_token` グラントは毎回 Google の token endpoint へ中継されるため、
Google がグラントを拒否すればこちらも拒否する。加えてリクエスト経路の tokeninfo 照会が
`email_verified` や `aud` を毎回確認する。

fail closed の分類は **「失効したと確信できる場合だけ拒否する」**方針で決めている。
`invalid_grant` を返すと OAuth クライアントはリフレッシュトークンを破棄する＝利用者は
再ログインを強いられるため、非対称なコストを踏まえて片側に倒す。

| 上流の状態 | 応答 |
| --- | --- |
| `invalid_grant` を伴う 400 / 401（取消・失効） | `invalid_grant` |
| **429 Too Many Requests**、408 | `503`（`Retry-After` があれば転送） |
| 5xx、到達不能、応答本文が不正、検証系が使えない | `503` |
| `invalid_client` 等を伴う 400 / 401（こちらの設定不備） | `503` + `error` ログ |
| その他の 4xx（403 等） | `503` |

429 を失効として扱わないのが要点である。扱ってしまうと **Google の一時的な流量制限だけで
利用者が一斉に再ログインさせられる**。同様に、`client_secret` の設定ミスで全利用者の
セッションを壊さないよう、`invalid_client` も失効とは区別している。
同じ分類は `/oauth/callback` の認可コード交換にも適用される（同一の関数を共有）。

> **運用注意**: `invalid_client` 等を 503 に倒す設計の裏返しとして、**クライアントは
> 再認証の合図を受け取れず 503 を再試行し続ける**。503 が継続する場合は Cloud Logging で
> `google refused the upstream refresh for a reason that is not a revocation` を確認し、
> `CS_SUPPORT_GOOGLE_OAUTH_CLIENT_ID` / `..._SECRET` が Google Cloud Console の現行値と
> 一致しているか確かめること。手順の詳細は `specs/production-cs-mcp.md` の
> 「OAuth AS の残存リスク」節を参照。

`/oauth/callback` の一時障害は**再試行できない**（state を Google 呼び出し前に消費するため）。
利用者は認可フローをやり直す必要があり、エラー応答の文言でその旨を明示している。

Google が refresh_token を返さなかった場合は、クライアントにも **refresh_token を渡さない**
（access_token のみ返す）。クライアントには標準的な「refresh_token 無しの応答」に見え、
期限切れ後は再認可に落ちる（黙ってリフレッシュ不能になるのではない）。

Google が `expires_in` を返さなかった場合は、寿命不明のまま渡さず短い値（600 秒）を仮定して
申告する。長く仮定すると「切れているのに使い続けて 401 を踏む」になるため、早めのリフレッシュに
倒す。`expires_in` は認可コードの発行から token 交換までの滞留分を差し引いた値をクライアントへ返す。

**state・認可コードはいずれも単回使用**（`AuthServerState::consume_jti`）。
使用済み記録は各ブロブ自身の有効期限まで保持される。state を単回使用にしているのは、
`/oauth/register` と `/oauth/authorize` が無認証であるため、有効な state を 1 つ入手した
相手が `callback` を連打して **Google への外向きリクエストを無制限に増幅できる**のを
防ぐため（`maxScale=1` なのでインスタンス飽和にも直結する）。

単回使用の担保はプロセス内メモリでの **best-effort**。コンテナ再起動をまたぐと弾けないが、
コードの寿命は 60 秒（state 600 秒）で、かつコードは PKCE の
`code_verifier` に束縛されている。

> **OAuth 2.1 への非準拠点（意図的）**: OAuth 2.1 は認可コードの再利用を検出した際、
> そのコードから発行済みのトークンを失効させることを求めるが、本実装はトークン台帳を
> 持たないため**再利用を検出しても既発行トークンを取り消せない**。できるのは 2 回目以降の
> 交換を拒否するところまで。解消には外部ストアが要る。

#### トークンの project 束縛（RFC 8707）

アクセストークンは発行時に `aud`（project_id）へ束縛され、`/{project_id}/mcp` の
ミドルウェアが**完全一致**を検証する。署名鍵は全 project で共有されるため、この照合が
ある project 向けのトークンが他 project で通ることを防いでいる唯一の境界である。

`/oauth/authorize` は RFC 8707 の `resource` パラメータ（`https://{public_host}/{project_id}/mcp`）
を受け取る。**`resource` が指定されない場合、config の project が 1 件のときに限り
それに束縛し、2 件以上あるときは `invalid_target` で拒否する。**
すなわち **project を 2 件目以降に増やすと、`resource` を送らないクライアントは
authorize に失敗するようになる**（起動時に警告ログを 1 回出す）。

> **警告:** 現状は検証済み Google アカウントで認証さえ通れば、突合表を経由せず
> 無条件で `Role::Supervisor` として扱われる（`server/src/harness/authn.rs`
> `Authenticator::lookup_by_identity`）。`add_known_resolution` を含む全操作が
> Google アカウントを持つ任意のユーザーから実行可能であり、actor 突合表の
> DB 実装が完了するまでアクセス制御としては不十分。
>
> さらに Google OAuth 同意画面は 2026-07-21 に **External（本番公開）** へ切替済みで、
> テストユーザによる制限は無い。したがって上記「任意のユーザー」の母集団は
> sivira.co 内部ではなく**全世界の任意の Google アカウント**である。
> 詳細は `specs/production-cs-mcp.md` の「AuthN 現状」節を参照。

JWT HS256 の共有鍵検証や config `[[actors]]` / `[auth] default_actor` による
role 導出は**撤去済みの旧方式**（commit 1809b8e / e90ef59 で撤去）であり、
現行実装には存在しない。

### Step 1 ルール・語彙の投入

```sh
cd server
VEGAPUNK_BEARER_TOKEN_FILE=/private/tmp/vegapunk-bearer-token \
  cargo run --bin ingest_rules -- \
  --endpoint http://vegapunk.local:6840 \
  --schema sivira-cs-demo \
  --schema-file ../schema/cs-schema.yml \
  --rules-file data/rules.sample.json
```

- signal 語彙: `specs/signal-vocabulary.md` / `server/data/signal-lexicon.json`（初版ドラフト・業務レビュー要）
- NG 辞書: `server/data/ng-dictionary.json`
- 監査 WORM: `server/data/audit/audit.jsonl`（append-only + hash chain。コミット対象外）

## Prerequisites

- Rust toolchain
- Reachable vegapunk gRPC endpoint（例: `http://vegapunk.local:6840`。ローカルで
  `vegapunk` を起動しない。SSH tunnel も不要。詳細はリポジトリルート
  `CLAUDE.md` の「ローカル MCP 起動手順」節を参照）
- Bearer token for vegapunk

## Ingest Demo Data

```sh
cd server
export VEGAPUNK_BEARER_TOKEN=...
cargo run --bin ingest_demo -- \
  --endpoint http://vegapunk.local:6840 \
  --schema sivira-cs-demo \
  --schema-file ../schema/cs-schema.yml \
  --manual-file data/manual.sample.json \
  --glossary-file data/glossary.json
```

Expected result:

```json
{
  "expected_edges": 23,
  "expected_nodes": 20,
  "schema": "sivira-cs-demo",
  "upserted_edges": 23,
  "upserted_nodes": 20
}
```

## Verify

```sh
cd server
export VEGAPUNK_BEARER_TOKEN=...
cargo run --bin verify_demo -- \
  --endpoint http://vegapunk.local:6840 \
  --schema sivira-cs-demo
```

The verifier checks:

- product count
- section count
- spec count
- graph snapshot nodes and edges
- Japanese manual search over `body_ja`
- product resolution without aliases
- section traversal through `CONTAINS` and `REFERENCES`

## Run MCP Server

```sh
cd server
export VEGAPUNK_BEARER_TOKEN=...
cargo run --bin cs-support-mcp -- --config config.toml
```

Endpoint:

```text
POST http://127.0.0.1:3000/sivira-cs-demo/mcp
```

Example:

```sh
curl -sS http://127.0.0.1:3000/sivira-cs-demo/mcp \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_manual","arguments":{"query_ja":"リセット穴を何秒押す","product_key":"SVR-HB100","top_k":2}}}'
```

## Local HTTPS

The MCP server can listen with TLS locally. Generate a trusted localhost
certificate with `mkcert`:

```sh
cd server
mkdir -p certs
mkcert -install
mkcert -cert-file certs/cert.pem -key-file certs/key.pem localhost 127.0.0.1
```

Run the HTTPS server:

```sh
cd server
export VEGAPUNK_BEARER_TOKEN=...
cargo run --bin cs-support-mcp -- --config config.local-https.toml
```

Local HTTPS endpoint:

```text
POST https://127.0.0.1:3443/sivira-cs-demo/mcp
```

Local verification:

```sh
curl -sS https://127.0.0.1:3443/sivira-cs-demo/mcp \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

## Development

```sh
cargo fmt --manifest-path server/Cargo.toml
cargo test --manifest-path server/Cargo.toml
cargo check --manifest-path server/Cargo.toml
```
