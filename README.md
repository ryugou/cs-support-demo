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

### Actor 認証（JWT HS256）

`Authorization: Bearer <JWT>`（`Claims { sub, role, exp, iss }`）を HS256 で検証し、
config の `[[actors]]` 表（sub → role / allowed_schemas）で AccessScope を導出する。

- 共有鍵: `[auth] jwt_secret_file` または env `CS_SUPPORT_JWT_SECRET_FILE`（平文を config に置かない）
- ローカル開発: 鍵未設定時は `[auth] default_actor` にフォールバック（warn ログ付き。GCE では鍵必須）

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
- Running vegapunk gRPC endpoint
- Bearer token for vegapunk

For the current shared vegapunk host, an SSH tunnel can expose gRPC locally:

```sh
ssh -i ~/.ssh/macmini-connect-key -L 16840:127.0.0.1:6840 -N agent@192.168.0.128
```

## Ingest Demo Data

```sh
cd server
export VEGAPUNK_BEARER_TOKEN=...
cargo run --bin ingest_demo -- \
  --endpoint http://127.0.0.1:16840 \
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
  --endpoint http://127.0.0.1:16840 \
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
