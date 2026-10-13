# graphql-engine-rust

A drop-in replacement for [Hasura GraphQL Engine V2](https://github.com/hasura/graphql-engine),
written in Rust. Point it at an existing V2 project (database, metadata, permissions) and it
serves the same GraphQL API.

Compatibility is the contract: metadata format, HTTP APIs, GraphQL schema, responses, errors
and `HASURA_GRAPHQL_*` configuration match V2. Internals are free to differ.

## Status

Early. Implemented so far:

- Metadata from a Hasura CLI metadata directory (`HASURA_GRAPHQL_METADATA_DIR`), from
  `hdb_catalog.hdb_metadata`, or an empty default source from `HASURA_GRAPHQL_DATABASE_URL`.
- Postgres sources: table introspection, V2 schema generation (names, types, custom root
  fields and column names), introspection queries.
- Queries: `t`, `t_by_pk` with `where`, `order_by` (incl. object relationships),
  `limit`, `offset`, `distinct_on`, nested object/array relationships, JSON path arguments,
  variables, fragments, `@skip`/`@include`, batched requests.
- Aggregates: `t_aggregate` and `<rel>_aggregate` (`count`, `sum`, `avg`, `max`, `min`, …,
  `nodes`), `order_by` on `<rel>_aggregate`, aggregation predicates in `where`.
- Admin secret auth, `x-hasura-role`, unauthorized role; select permissions without row
  filters (row filters arrive with the permissions workstream).
- `/healthz`, `/v1/version`, `/v1/graphql`.

See `/plan/roadmap.md` in the project files for the full plan.

## Running

```sh
cargo run --release -- serve \
  --database-url postgres://postgres:postgres@localhost:5432/postgres \
  --metadata-dir ./hasura/metadata
```

or with Docker (listens on `$PORT`, defaulting to 8080, as Cloud Run expects):

```sh
docker build -t graphql-engine-rust .
docker run -p 8080:8080 \
  -e HASURA_GRAPHQL_DATABASE_URL=postgres://... \
  -e HASURA_GRAPHQL_ADMIN_SECRET=... \
  -v $PWD/hasura/metadata:/metadata -e HASURA_GRAPHQL_METADATA_DIR=/metadata \
  graphql-engine-rust
```

Supported environment variables: `HASURA_GRAPHQL_DATABASE_URL`,
`HASURA_GRAPHQL_METADATA_DATABASE_URL`, `HASURA_GRAPHQL_METADATA_DIR`,
`HASURA_GRAPHQL_ADMIN_SECRET(S)`, `HASURA_GRAPHQL_UNAUTHORIZED_ROLE`,
`HASURA_GRAPHQL_SERVER_HOST/PORT`, `HASURA_GRAPHQL_DEV_MODE`,
`HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS`, `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`,
`HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE`, `HASURA_GRAPHQL_CORS_DOMAIN`,
`HASURA_GRAPHQL_DISABLE_CORS`, `HASURA_GRAPHQL_ENABLED_APIS`, `HASURA_GRAPHQL_PG_CONNECTIONS`,
`HASURA_GRAPHQL_USE_PREPARED_STATEMENTS`, `HASURA_GRAPHQL_PRESERVE_401_ERRORS`,
`HASURA_GRAPHQL_LOG_LEVEL`.

## Layout

| Crate | Role |
|---|---|
| `hge-core` | Shared types: errors, names, sessions, database catalog. No IO. |
| `hge-metadata` | V2 metadata model and the CLI metadata directory loader. No IO beyond files. |
| `hge-ir` | Backend-neutral query IR and the `SourceExecutor` trait. |
| `hge-schema` | Per-role GraphQL schema, validation, introspection, lowering to IR. |
| `hge-postgres` | Postgres catalog introspection, SQL generation, pooling, execution. |
| `hge-auth` | Admin secret / role resolution (JWT and webhooks next). |
| `hge-server` | axum HTTP server, configuration, state. |
| `graphql-engine` | The binary (`serve`, `version`, `export-metadata`). |

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
HASURA_GRAPHQL_DATABASE_URL=postgres://postgres:postgres@localhost:5432/hge_test cargo test --workspace
```

End-to-end cases live in `crates/hge-server/tests/cases/*.yaml` (V2 test format: `query`,
`headers`, `status`, `response`). The test database is wiped and seeded from
`crates/hge-server/tests/fixtures/basic/schema.sql`, so use a throwaway database.
