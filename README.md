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
