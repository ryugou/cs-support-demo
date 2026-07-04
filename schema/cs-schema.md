# CS Support MCP Schema

This file documents the logical schema used by the `sivira-cs-demo` vegapunk schema.
The concrete schema body used for registration is [cs-schema.yml](cs-schema.yml).

## Registration

Register or update the schema with the Rust CLI:

```sh
cargo run --manifest-path server/Cargo.toml --bin ingest_demo -- \
  --endpoint http://vegapunk.local:6840 \
  --schema sivira-cs-demo \
  --token-env VEGAPUNK_BEARER_TOKEN \
  --schema-file schema/cs-schema.yml \
  --manual-file server/data/manual.sample.json
```

The CLI uses vegapunk low-level graph APIs:

- `CreateSchema` / `UpdateSchema` for schema lifecycle
- `UpsertNodes` for `product`, `document`, `section`, `spec`
- `UpsertEdges` for `HAS_DOCUMENT`, `CONTAINS`, `REFERENCES`, `HAS_SPEC`, `DEFINED_IN`

Low-level graph node IDs are prefixed with the schema name, for example:

```text
sivira-cs-demo:gen1:product:SVR-HB100
sivira-cs-demo:gen1:section:svr-hb100-user-guide#charging
```

## Scope

The demo intentionally has no issue/trouble-log nodes. Product manuals, sections,
and specs must be sufficient for the first customer-support demo.
