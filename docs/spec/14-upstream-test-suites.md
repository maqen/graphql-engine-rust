# 14. Upstream test suites and differential testing plan

How to reuse Hasura's own test suites against the Rust engine, and how to run a side-by-side
("differential") comparison against the upstream `hasura/graphql-engine` v2 Docker image.

Upstream locations (relative to the repo root):

- `server/tests-py/` - Python (pytest) integration suite with YAML golden fixtures in `server/tests-py/queries/`. **Primary reusable asset.**
- `server/lib/api-tests/` + `server/lib/test-harness/` - Haskell hspec "golden API tests"; mostly run the engine **in-process** (linked as a library), so they are a fixture source, not directly runnable against another binary.
- `server/benchmarks/` - `bench.sh` + `graphql-bench` (k6) load benchmarks with fixed datasets (chinook etc.).
- Docker Hub `hasura/graphql-engine` - reference binaries.

---

## Scope and priority

- **Essential**: running `server/tests-py` against the Rust binary in `--hge-bin` mode, Postgres
  backend only, CE-only tests. This gives roughly 1,000+ test cases with exact expected
  responses for GraphQL queries/mutations/subscriptions, metadata API, `/v1/query`, actions,
  event triggers, remote schemas, auth.
- **Essential**: a differential harness that sends identical requests to upstream
  `hasura/graphql-engine:v2.51.0-ce` and the Rust engine (same Postgres data, same metadata) and
  compares normalized responses. This catches everything the golden tests do not cover (real
  user metadata, error messages, introspection output).
- **Useful**: harvesting fixtures from `server/lib/api-tests` (YAML embedded in Haskell
  quasi-quotes) into our own golden tests; running the handful of api-tests specs that use the
  external-process harness.
- **Useful later**: `server/benchmarks` chinook set for throughput/latency comparison.
- **Skip**: BigQuery, MSSQL, Citus, Data Connector tests; EE tests (`enterprise_edition`,
  `pro_tests_fixtures`, `--pro-tests`, Redis caching `test_query_cache.py`); upgrade tests.

---

## 1. Which upstream version to target

- Latest v2 release on Docker Hub (checked 2026-10-13 via the Docker Hub API):
  **`v2.51.0`**, pushed 2026-09-21; previous: `v2.50.3` (2026-09-08), `v2.50.2`.
  `latest` currently points to v2.51.0. (The CHANGELOG.md in the source snapshot is stale at
  v2.10.1 and is useless for this; the docs mention CLI v2.50.3.)
- Image tag families:
  - `hasura/graphql-engine:v2.51.0` - the **EE binary** that runs in "CE-compatible" mode without a
    license key (some EE-only code paths and fields may still differ, e.g. `server_type`, extra
    log fields).
  - `hasura/graphql-engine:v2.51.0-ce` - the open-source CE build. **Use this for parity testing.**
  - Suffixes `.ubuntu` / `.ubi` (base image), `.amd64` / `.arm64`, and
    `.cli-migrations-v2` / `.cli-migrations-v3` (entrypoint that applies migrations/metadata at
    startup - see doc 11 section 3.12).
- The upstream source snapshot used (master `895e7e9`) is close to the latest release
  (Python `requirements.txt` pins `certifi==2026.1.4`; catalog version 48). Verified 2026-10-13
  against GitHub raw files at tag `v2.51.0`: `server/src-rsr/catalog_version.txt` = **48**
  (same as the snapshot), and the `serve` flag set of v2.51.0 equals the snapshot's except that
  `--remote-schema-healing-interval` / `HASURA_GRAPHQL_REMOTE_SCHEMA_HEALING_INTERVAL` is
  snapshot-only (unreleased). Tags `v2.51.1`/`v2.52.0` did not exist. Flag introduction points
  (for projects pinned to older releases; see doc 11): `--enable-relay`, `--disable-eventing`,
  `--log-masked-variables` in v2.49.0; `--disable-event-processing`, `--websocket-queue-size` in
  v2.50.0; `--disable-admin-secret` and the three `--redact-*-logs` flags in v2.51.0;
  `--server-timeout`, `--preserve-401-errors`, `--max-total-header-length`,
  `--configured-header-precedence` already in v2.48.0. Pin the differential harness to
  `v2.51.0-ce`.

---

## 2. `server/tests-py` - organisation

### 2.1 Layout

| Path | Content |
|---|---|
| `conftest.py` | pytest options, fixtures (`hge_ctx`, `hge_server`, `per_class_tests_db_state`, `per_method_tests_db_state`, webhook servers, JWT, remote schema servers, Postgres DB creation). |
| `context.py` | `HGECtx` (HTTP/WS client helpers `v1q`, `v1metadataq`, `v2q`, `v1graphqlq`, WS clients for `/v1/graphql` (legacy apollo protocol and `graphql-transport-ws`), `/v1alpha1/graphql`, `/v1/relay`, `/v1beta1/relay`), plus event/action webhook servers. |
| `validate.py` | `check_query_f` / `check_query`: loads a YAML fixture and compares the response. |
| `fixtures/hge.py` | spawns the engine binary (`--hge-bin` mode). |
| `fixtures/postgres.py` | creates a fresh Postgres database per test class for metadata and for each source. |
| `fixtures/jwt.py`, `fixtures/tls.py` | JWT keys, TLS certs for webhook tests. |
| `queries/**.yaml` | 1,666 YAML files (setup/teardown + request/response pairs). |
| `remote_schemas/nodejs/` | Node (Apollo) remote schema servers (`npm ci` needed). |
| `remote_server.py`, `graphql_server.py`, `webhook.py`, `auth_webhook_server.py`, `jwk_server.py`, `webserver.py` | Python helper servers (remote schemas via graphene, action/event webhooks, auth webhook, JWK endpoint). |
| `pgdump/` | expected pg_dump outputs. |
| `run.sh`, `docker-compose.yml`, `docker/postgres-init.sh` | convenience runner (Postgres x2, Citus, SQL Server, Redis). **Caveat (verified):** `docker-compose.yml` `extends` services from `../../docker-compose/databases.yaml`, which is **absent** from the snapshot, so `docker compose up` / `run.sh` fail as-is - provide your own Postgres (PostGIS image, password `hasura`) or recreate that file. `run.sh` also builds HGE with cabal unless `HGE_BIN` is set, and passes `--redis-url`. |
| `pytest.ini` | markers (`backend`, `admin_secret`, `no_admin_secret`, `requires_an_admin_secret`, `hge_env`, `jwk_path`, `jwt`, `tls_webhook_server`, `no_tls_webhook_server`, `tls_insecure_certificate`, `capture_hge_logs`, `value`), `xfail_strict = true`, `norecursedirs = queries webhook test_upgrade`. |
| `requirements.txt` | pinned Python deps (pytest 6.2.5, websocket-client 0.56.0, graphene 2, sqlalchemy 1.x, PyJWT 2.3, ruamel.yaml, jsondiff, croniter, redis). Python 3.12 works per comments. |

### 2.2 Test files and sizes (number of `def test` per file; many are parametrized by
`transport` (`http`/`websocket`) and backend, so case counts are higher)

| File | tests | Area | Reuse |
|---|---|---|---|
| `test_graphql_queries.py` | 273 | queries: basic, aggregates, bool exps, order_by, limits, permissions, computed fields, enums, functions, relay, custom schema, read-only source | must (Postgres classes) |
| `test_v1_queries.py` | 194 | legacy `/v1/query` (run_sql, track_table, DDL, permissions, select/insert/update/delete/count) | must |
| `test_graphql_mutations.py` | 195 | insert/update/delete/upsert, nested inserts, enums, functions, transactions, inherited roles | must |
| `test_actions.py` | 72 | sync/async actions, relationships, transforms | must |
| `test_schema_stitching.py` | 62 | remote schemas | should |
| `test_events.py` | 49 | event triggers | must |
| `test_metadata.py` | 46 | metadata API (export/replace/reload/inconsistency, sources) | must |
| `test_remote_relationships.py` | 44 | remote relationships | should |
| `test_subscriptions.py` | 34 | live queries, streaming, both WS protocols | must |
| `test_endpoints.py` | 24 | RESTified endpoints (`TestCustomEndpointsPro` is EE) | should |
| `test_remote_schema_permissions.py` | 23 | remote schema permissions (`ENABLE_REMOTE_SCHEMA_PERMISSIONS`) | should |
| `test_allowlist_queries.py` | 20 | allowlist (role-based parts EE) | should |
| `test_v2_queries.py` | 17 | `/v2/query` (run_sql etc.) | must |
| `test_openapi.py` | 15 | `/api/swagger/json` | may |
| `test_jwt.py` | 15 | JWT auth | must |
| `test_config_api.py` | 13 | `/v1alpha1/config` | should |
| `test_roles_inheritance.py` | 12 | inherited roles | should |
| `test_logging.py` | 12 | log shapes (needs `capture_hge_logs`) | should |
| `test_graphql_introspection.py` | 12 | introspection (disable-for-role parts EE) | must |
| `test_webhook.py` | 9 | auth webhook | must |
| `test_scheduled_triggers.py` | 9 | cron/scheduled triggers | must |
| `test_naming_conventions.py` | 9 | naming conventions | should |
| `test_jwt_claims_map.py` | 9 | JWT claims_map | should |
| `test_query_cache.py` | 8 | `@cached` caching (EE, Redis) | skip |
| `test_dev_endpoints.py` | 8 | developer APIs | may |
| `test_apis_disabled.py` | 8 | ENABLED_APIS | should |
| `test_jwk.py` | 6 | JWK URL refresh | should |
| `test_cors.py` | 6 | CORS | must |
| `test_validation.py` | 4 | GraphQL validation | must |
| `test_v1alpha1_endpoint.py` | 4 | `/v1alpha1/graphql` | should |
| `test_apollo_federation.py` | 4 | federation (needs Node gateway for 2 tests) | may |
| `test_websocket_init_cookie.py` | 3 | WS cookie | may |
| `test_webhook_request_context.py` | 3 | webhook POST body | should |
| `test_schema_duplication.py` | 3 | duplicate GraphQL names across sources/tables | should |
| `test_remote_schema_prioritize_data.py` / `_none.py` | 3/3 | remote schema data+errors | may |
| `test_webhook_insecure.py` | 2 | insecure TLS webhooks | may |
| `test_pg_dump.py` | 2 | `/v1alpha1/pg_dump` | may (needs `pg_dump` in our image) |
| `test_graphql_read_only_source.py` | 2 | read-only Postgres source | should |
| `test_compression.py` | 2 | gzip | should |
| `test_auth_webhook_cookie.py` | 2 | cookie forwarding to webhook | may |
| `test_version.py` | 1 | `/v1/version` equals `VERSION` env | adapt |
| `test_inconsistent_meta.py` | 1 | inconsistent metadata | must |
| `test_heterogeneous.py` | 1 | multiple backends (MSSQL) | skip |
| `test_disable_admin_secret.py` | 1 | DISABLE_ADMIN_SECRET | may |
| `test_compat.py` | 1 | backwards-compat (old metadata shapes) | should |
| `test_tests.py`, `test_tests/` | 14 | self-tests of the harness | run once to validate harness setup |

`queries/` sizes (YAML files): `graphql_query` 411, `graphql_mutation` 321, `v1` 319,
`remote_schemas` 184, `event_triggers` 122, `actions` 89, `subscriptions` 44,
`graphql_introspection` 29, `endpoints` 26, `explain` 24, `v2` 22, `openapi` 17, `query_cache` 10
(EE), `naming_conventions` 7, `graphql_validation` 6, `unauthorized_role` 5, `schema` 5,
`heterogeneous` 4, `compression` 4, `apollo_federation` 4, `inconsistent_objects` 3, `webhooks` 2,
`horizontal_scale` 2, `scheduled_triggers` 1, `logging` 1.
Subdirectories of note: `graphql_query/{agg_perm, aggregations, allowlist, allowlist_role_based, basic, bigquery(skip), boolexp, caching(EE), computed_fields, computed_fields_row_permission_bypass, custom_schema, enums, functions, limits, offset, order_by, order_by_nulls, permissions, read_only_source, relay, root_col_perm, v1alpha1}`,
`graphql_mutation/{custom_schema, delete, enums, functions, insert, roles_inheritance, transactions, update}`,
`v1/{basic, bulk, computed_fields, count, ddl, delete, insert, metadata, metadata_order, non_empty_text, permissions, relationships, run_sql, select, set_table_configuration, set_table_custom_fields, set_table_is_enum, track_table, update}`,
`subscriptions/{basic, cohort_isolation, customized_source, live_queries, mssql(skip), multiplexing, streaming, udf_session_args}`.
Files with `_mssql`, `_citus`, `_bigquery` suffixes are backend variants - skip.

### 2.3 Fixture format

A request/response fixture (e.g. `queries/graphql_query/limits/select_query_article_string_limit_error.yaml`):

```yaml
description: Nested select on article with limit expecting error
url: /v1/graphql
status: 200
response:
  errors:
    - extensions:
        code: validation-failed
        path: $.selectionSet.article.args.limit
      message: expected a non-negative 32-bit integer for type 'Int', but found a string
query:
  query: |
    query { article(limit: "3", offset: 1) { id title } }
```

Keys: `description`, `url`, `method` (default POST; `GET`, `PUT`, ... used by REST endpoint
tests), `status` (default 200), `headers` (request headers; role/session vars), `query` (request
body: for `/v1/graphql` an object with `query`, `variables`, `operationName`; for metadata the
`{type,args}` object), `body` (raw body for some REST tests), `response` (expected JSON),
`allowed_responses` (list of acceptable responses), `resp_headers` (subset of expected response
headers). A file may contain a list of such documents (executed in order).

Setup/teardown files are `/v1/query` (or `/v2/query` + `/v1/metadata` when the class sets
`setup_metadata_api_version = "v2"`) requests, typically `{"type":"bulk","args":[{"type":"run_sql",...},{"type":"track_table",...},{"type":"create_select_permission",...}]}`.
**Consequence: the legacy `/v1/query` API including `bulk`, `run_sql` and the v1 metadata
commands (`track_table`, `add_existing_table_or_view`, `create_object_relationship`,
`create_insert_permission`, ...) must work before almost any tests-py test can run.** Prioritise it.

Per class fixtures:
- `per_class_tests_db_state` - `setup.yaml` before the class, `teardown.yaml` after (or
  `schema_setup.yaml`/`schema_teardown.yaml` + metadata files in v2 mode); used for read-only tests.
- `per_method_tests_db_state` - same per test (metadata tests).
- `per_class_db_schema_for_mutation_tests` + `per_method_db_data_for_mutation_tests` -
  `schema_setup`/`values_setup.yaml`/`values_teardown.yaml` per test for mutations.
- Backend suffix: `hge_ctx.backend_suffix('setup')` -> `setup.yaml` for postgres, `setup_mssql.yaml` etc.

### 2.4 How responses are compared (`validate.py`)

- `check_query` (verified): if the fixture has no headers, randomly (50%) adds `X-Hasura-Role: admin`. When an
  admin secret is configured, it adds `X-Hasura-Admin-Secret` and **also** runs
  `test_forbidden_when_admin_secret_reqd`: the same request without secret and with a random
  secret must fail with status 200 (GraphQL endpoints `/v1/graphql`, `/v1/relay`, `/v1beta1/relay`
  - errors in body) or 401/404 (other endpoints). With webhook auth it adds
  `X-Hasura-Auth-Mode: webhook` and a bearer token handled by the test webhook, and runs
  `test_forbidden_webhook`.
- `transport: websocket` replays GraphQL fixtures over WS (legacy `graphql-ws` protocol, or
  `graphql-transport-ws` with `gqlws=True`) and compares the `data`/`errors` payload.
- Comparison (`assert_graphql_resp_expected`, `collapse_order_not_selset`, `equal_CommentedMap`):
  - For GraphQL requests, key order matters **within objects that correspond to selection sets**
    (fields must come back in query order); key order elsewhere (scalar JSON values,
    `errors` entries, non-GraphQL responses) is ignored. Note it walks by `field.name`, not alias,
    and only the first definition of the document.
  - Arrays: order and length matter.
  - Response headers: expected headers must be a subset.
  - **Leniency** (verified, `assert_graphql_resp_expected` in `validate.py`): the HTTP status
    code is always asserted exactly (`assert_response_code`), but if both expected and actual
    bodies are error responses (have a truthy `error` or `errors` key) and differ (e.g. different
    message or code), the harness only emits `warnings.warn("Response does not have the expected error message ...")`
    and passes. So tests-py does not enforce exact error messages/codes; our differential harness
    must (section 4). Run pytest with `-W error::UserWarning` (or grep the warnings summary) to
    surface these mismatches.
  - `--accept` rewrites expected outputs (never use against our engine except to diff).
- Numbers: the harness always starts the engine with `--stringify-numeric-types`, so
  `numeric`/`bigint`/float columns are strings in expected outputs. Our engine must implement
  this flag before these fixtures pass.

### 2.5 Running modes

**Mode A - harness spawns our binary (`--hge-bin`)** (recommended; required for `hge_env` and
`capture_hge_logs` tests):

```sh
cd server/tests-py
python3 -m venv .venv && . .venv/bin/activate && pip install -r requirements.txt
(cd remote_schemas/nodejs && npm ci)        # for remote schema tests
# Postgres: the bundled docker-compose.yml extends a missing ../../docker-compose/databases.yaml,
# so start your own, e.g. two `postgis/postgis:15-3.4` containers with POSTGRES_PASSWORD=hasura
export HASURA_GRAPHQL_PG_SOURCE_URL_1=postgresql://postgres:hasura@localhost:<p1>/postgres
export HASURA_GRAPHQL_PG_SOURCE_URL_2=postgresql://postgres:hasura@localhost:<p2>/postgres
export VERSION=<string our /v1/version returns>   # context.py otherwise runs ../../scripts/get-version.sh (absent in snapshot)
pytest --dist=loadscope -n auto \
  --hge-bin=/path/to/hasura-rs \
  --pg-urls "$HASURA_GRAPHQL_PG_SOURCE_URL_1" "$HASURA_GRAPHQL_PG_SOURCE_URL_2" \
  --backend postgres \
  -k "not MSSQL and not Citus and not BigQuery and not Pro and not EE" \
  --deselect test_query_cache.py
```

What the harness does in this mode (`fixtures/hge.py`, `fixtures/postgres.py`, `conftest.py`):
- For each test class, creates a fresh database `test_metadata_<test name>` (name truncated / hashed
  to <64 chars) and a source database `test_source_default_<...>` owned by the superuser from
  `--pg-urls`; creates extensions `citext`, `ltree`, `pgcrypto`, schema `hge_tests`; grants all to a
  **non-superuser** `hasura_server_tests_py` (password `password`) which the engine uses.
  (Some classes need PostGIS: fixture `postgis` runs `CREATE EXTENSION postgis` - use a PostGIS image.)
- Spawns (verified, `fixtures/hge.py`): `<hge-bin> --metadata-database-url <metadata db url> serve --enable-telemetry=false --server-port <free port> --stringify-numeric-types [--admin-secret <uuid>]`
  (note: the **global** option comes before `serve`; there is no `--database-url`, so the engine
  starts with an empty metadata and no `default` source)
  with env restricted to `PATH`, `HASURA_GRAPHQL_PG_SOURCE_URL_1/2`, `HASURA_GRAPHQL_CITUS_SOURCE_URL`,
  `HASURA_GRAPHQL_MSSQL_SOURCE_URL`, `ODBCSYSINI`, `ODBCINSTINI`, plus fixture env (e.g.
  `EVENT_WEBHOOK_HANDLER`, `ACTION_WEBHOOK_HANDLER`, `SCHEDULED_TRIGGERS_WEBHOOK_DOMAIN`,
  `EVENT_WEBHOOK_HEADER=MyEnvValue`, `HASURA_GRAPHQL_AUTH_HOOK`, `HASURA_GRAPHQL_JWT_SECRET`) and
  the class's `@pytest.mark.hge_env(NAME, VALUE)` markers (markers with value `None` are
  dropped). Waits up to 30 s for the **TCP port to open** (`ports.wait_for_port`) - it does not
  poll `/healthz`, so the engine must not open the listening socket before it can serve
  `/v1/metadata` (upstream opens the port only after catalog migration and schema build).
  stdout+stderr are piped and captured only for classes with `capture_hge_logs`.
  On teardown (in a background thread) sends SIGTERM, waits 5 s, then SIGKILL - our graceful
  shutdown must finish quickly when idle.
- Then adds the source with `export_metadata` + `replace_metadata` (verified,
  `fixtures/postgres.py` `add_source`: appends
  `{"name":"default","kind":"postgres","configuration":{"connection_info":{"database_url":"<url>"}},"customization":{},"tables":[]}`
  where `<url>` is a literal `postgresql://hasura_server_tests_py:password@.../test_source_default_<...>` string),
  so `export_metadata`/`replace_metadata` (with `customization: {}`) must work immediately.
  Classes with the `default_source_disabled` marker get no source. Metadata/source DB names are
  `test_metadata_<listchain names>` / `test_source_<name>_<...>`, replaced by an adler32 hash
  when >= 64 chars.
- Admin secret: only when the class has the `admin_secret` marker (random UUID).
- Common `hge_env` values seen: `HASURA_GRAPHQL_EXPERIMENTAL_FEATURES` (`naming_convention`,
  `streaming_subscriptions`, `group_by_aggregations`, `apollo_federation`),
  `HASURA_GRAPHQL_AUTH_HOOK_MODE`, `HASURA_GRAPHQL_UNAUTHORIZED_ROLE=anonymous`,
  `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS=false`, `HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS=true`,
  `HASURA_GRAPHQL_ENABLED_APIS`, `HASURA_GRAPHQL_DISABLE_CORS`, `HASURA_GRAPHQL_WS_READ_COOKIE`,
  `HASURA_GRAPHQL_LOG_LEVEL=debug`, `HASURA_GRAPHQL_ENABLE_ALLOWLIST`,
  `HASURA_GRAPHQL_ENABLED_LOG_TYPES`, `HASURA_GRAPHQL_DEFAULT_NAMING_CONVENTION`,
  `HASURA_GRAPHQL_CORS_DOMAIN`, `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS=false`.
  All must be parsed (doc 11).

**Mode B - external server (`--hge-urls URL --pg-urls PG`)**: start our server yourself with the
env from the README (`EVENT_WEBHOOK_HEADER=MyEnvValue EVENT_WEBHOOK_HANDLER=http://localhost:5592 SCHEDULED_TRIGGERS_WEBHOOK_DOMAIN=http://127.0.0.1:5594 ... serve --stringify-numeric-types`),
then `pytest --hge-urls http://localhost:8080 --pg-urls <pg>`. Classes whose `hge_env` markers do
not match the running server are **skipped**; the harness clears the DB between classes with
`queries/clear_db.yaml` via `/v2/query`. Useful for quick iteration and for running the same suite
against upstream (`--hge-urls` pointed at the upstream container) to establish the baseline.

Helper servers: in Mode A the fixtures start helper servers on free ports and pass their URLs via
fixture env (`EVENT_WEBHOOK_HANDLER`, `ACTION_WEBHOOK_HANDLER`, `HASURA_GRAPHQL_AUTH_HOOK`, remote
schema URLs in setup files), so the engine must reach `localhost` of the test runner (run our
binary on the host, or in a container with `--network host`). In Mode B the README uses fixed
ports (`EVENT_WEBHOOK_HANDLER=http://localhost:5592`, `SCHEDULED_TRIGGERS_WEBHOOK_DOMAIN=http://127.0.0.1:5594`).

### 2.6 Baseline first

Before measuring our engine, run the suite (Mode A) against the upstream CE binary extracted from
the image: `docker create hasura/graphql-engine:v2.51.0-ce` then
`docker cp <id>:/bin/graphql-engine ./hge-upstream` (path inferred; check `docker inspect` `Cmd`/`Entrypoint`; the binary needs the image's shared libs - easier: a wrapper script
`hge-docker.sh` that runs `docker run --rm --network host -e ... hasura/graphql-engine:v2.51.0-ce graphql-engine "$@"`
forwarding the env markers). Record the pass/skip/xfail list as the baseline; our target is
"everything that passes upstream CE passes for us".

### 2.7 Suggested order of enabling directories

1. `test_tests.py` (harness sanity), `test_version.py` (with `VERSION` env), `test_apis_disabled.py`, `test_cors.py`.
2. `test_v2_queries.py` / `queries/v2` and `test_v1_queries.py` subsets `v1/run_sql`, `v1/track_table`, `v1/bulk`, `v1/ddl` (setup machinery).
3. `test_graphql_queries.py` Postgres classes: `queries/graphql_query/{basic, limits, offset, order_by, order_by_nulls, boolexp, aggregations, agg_perm, permissions, root_col_perm, computed_fields, enums, functions, custom_schema}` (http transport first, then websocket).
4. `test_graphql_introspection.py`, `test_validation.py` / `queries/graphql_validation`, `test_schema_duplication.py`.
5. `test_graphql_mutations.py` / `queries/graphql_mutation/*`.
6. `test_metadata.py` / `queries/v1/metadata`, `queries/v1/metadata_order`, `test_inconsistent_meta.py`, `test_compat.py`.
7. Auth: `test_jwt.py`, `test_jwt_claims_map.py`, `test_jwk.py`, `test_webhook.py`, `test_webhook_request_context.py`, `queries/unauthorized_role`.
8. `test_subscriptions.py` / `queries/subscriptions/{basic, live_queries, multiplexing, streaming, cohort_isolation, customized_source, udf_session_args}`.
9. `test_events.py` / `queries/event_triggers`, `test_scheduled_triggers.py`.
10. `test_actions.py` / `queries/actions`.
11. `test_schema_stitching.py`, `test_remote_relationships.py`, `test_remote_schema_permissions.py` / `queries/remote_schemas` (needs Node + graphene servers).
12. `test_roles_inheritance.py`, `test_naming_conventions.py`, `test_endpoints.py`, `test_allowlist_queries.py`, `test_openapi.py`, `test_config_api.py`, `test_logging.py`, `test_compression.py`, `test_graphql_read_only_source.py`, `explain` fixtures (SQL text will differ; compare only structure/`field`).
13. Rest: `test_dev_endpoints.py`, `test_pg_dump.py`, `test_apollo_federation.py`, cookie/insecure webhook tests.

Expected structural mismatches to plan for: tests comparing exact SQL (`queries/explain/*`,
`query-log` `generated_sql`) - our SQL will differ; replace exact comparison with
execution-result comparison or mark as known deviations.

---

## 3. `server/lib/api-tests` (Haskell hspec)

- 172 `*Spec.hs` modules under `server/lib/api-tests/src/Test/`: `API` 16, `Auth` 10,
  `DataConnector` 17 (skip), `Databases` 15 (Postgres/SQLServer/BigQuery; take Postgres),
  `EventTriggers` 16, `Mutations` 14, `Queries` 29, `Regression` 19, `ScheduledEvents` 1,
  `Schema` 27, `Subscriptions` 3, `Webhook` 1, plus `HealthCheckSpec`, `HelloWorldSpec`,
  `NamingConventionsSpec`, `PortedFromPytest/`. 106 of them reference Postgres fixtures.
- Engine execution: `Harness.GraphqlEngine.startServerThread`/`runApp`
  (`server/lib/test-harness/src/Harness/GraphqlEngine.hs`) runs the engine **in-process** with
  `Constants.serveOptions` - not swappable for an external binary without modifying the harness.
- A few specs use the external-process harness
  (`server/lib/test-harness/src/Harness/Services/ExternalProcess/GraphqlEngine.hs`, binary from env
  `GRAPHQL_ENGINE`): args
  `+Heartbeat --enable-monitoring -Heartbeat serve --enable-console --console-assets-dir frontend/dist/apps/server-assets-console-ce --server-port <N> --metadata-database-url <URL>`,
  env `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT=0`, `HASURA_GRAPHQL_ADMIN_SECRET=<...>`. Specs:
  `HelloWorldSpec`, `HealthCheckSpec`, `NamingConventionsSpec`,
  `Regression/OptimizePermissionFiltersSpec`, `Queries/Relationship/PermissionRootReferenceSpec`,
  `Queries/PermissionsKnowAboutColumnNullabilitySpec`, `Queries/Simple/ObjectQueriesSpec`. These
  can be run against our binary by setting `GRAPHQL_ENGINE=/path/to/hasura-rs` (requires a
  Haskell toolchain to build `api-tests`; priority may). Our binary must tolerate the
  `+Heartbeat ... -Heartbeat` span (doc 11 1.3) and `--enable-console` with a missing assets dir.
- Practical reuse: the tests embed setup metadata and expected responses as YAML quasi-quotes
  (`[yaml| ... |]`, `[interpolateYaml| ... |]`) with backend-parametrized table names. Write a
  one-off extractor (regex over `shouldReturnYaml`/`shouldReturnOneOfYaml` blocks + the `Schema.Table`
  definitions) to turn Postgres variants into our own golden tests, or port high-value modules by
  hand: `Queries/` (filters, sorting, pagination, nested objects, native queries, aggregations,
  `Errors/`), `Mutations/`, `Schema/` (custom root fields, enums, computed fields, logical models),
  `Auth/` (JWT/webhook edge cases), `Regression/` (each a past bug: high signal),
  `API/Metadata/` (`InconsistentSpec`, `TablesSpec`, `TrackTablesSpec`, `UntrackTablesSpec`,
  `WarningsSpec`, `BulkAtomicSpec`, `BulkKeepGoingSpec`, `LogicalModelsSpec`,
  `ComputedFieldsSpec`, `SuggestRelationshipsSpec`, `TransparentDefaultsSpec`,
  `TestConnectionTemplateSpec` (EE - skip)), `API/ExplainSpec`, `API/ConcurrentBulkSpec`,
  `EventTriggers/`, `Subscriptions/`.
- Skip: `DataConnector/`, BigQuery/SQLServer/Citus/Cockroach variants, anything requiring an EE license.

---

## 4. Differential testing against upstream (side-by-side)

### 4.1 Topology

```
                 +-------------------------------+
  replay/fuzz -> | diff-runner (Rust or Python)  |
                 +------+----------------+-------+
                        |                |
          http/ws       v                v        http/ws
        +------------------+      +------------------+
        | upstream         |      | hasura-rs        |
        | v2.51.0-ce :8080 |      | :8081            |
        +--------+---------+      +---------+--------+
                 | metadata DB A            | metadata DB B
                 +---------+    +-----------+
                           v    v
                  Postgres (postgis/postgis:15) with two *identical* data DBs
                  (data_a, data_b) restored from the same dump
```

docker compose sketch:

```yaml
services:
  pg:
    image: postgis/postgis:15-3.4
    environment: {POSTGRES_PASSWORD: pw}
  upstream:
    image: hasura/graphql-engine:v2.51.0-ce
    environment:
      HASURA_GRAPHQL_METADATA_DATABASE_URL: postgres://postgres:pw@pg/meta_a
      PG_DATABASE_URL: postgres://postgres:pw@pg/data_a
      HASURA_GRAPHQL_ADMIN_SECRET: secret
      HASURA_GRAPHQL_ENABLE_TELEMETRY: "false"
      HASURA_GRAPHQL_DEV_MODE: "true"          # include internal errors to diff
      HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES: "false"
      HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL: "200"
      # webhook URLs for actions/events point at a shared echo/recorder service
    ports: ["8080:8080"]
  rust:
    image: hasura-rs:dev
    environment: {... same, with meta_b / data_b ...}
    ports: ["8081:8080"]
  recorder:   # receives action/event/remote-schema webhooks from both engines, tags by Host header/port
    image: our-webhook-recorder
```

Two separate data DBs are needed because mutations would otherwise interfere; for read-only
query diffing a single shared data DB is fine (and faster). Use separate metadata DBs always
(both engines write `hdb_catalog`).

Alternative for metadata-heavy projects: point both at the same project directory and apply via
the hasura CLI: `hasura metadata apply --endpoint http://localhost:8080` and `--endpoint
http://localhost:8081` (also exercises CLI compatibility: the CLI calls `/v1/version`,
`/v1/metadata` `replace_metadata` v2, `get_inconsistent_metadata`, and `/v2/query` `run_sql` for
migrations (inferred list)).

### 4.2 Inputs (what to replay)

1. **tests-py fixtures as a corpus**: iterate all `queries/**/*.yaml` (Postgres variants); run each
   class's setup on both engines, then send each fixture request to both and diff the two actual
   responses (ignoring the stored expected response). This upgrades tests-py's lenient error
   comparison to strict parity.
2. **api-tests extracted fixtures** (section 3).
3. **Real projects**: for each sample project (metadata dir + migrations + seed data), apply to
   both; then:
   - diff `export_metadata` outputs (after `replace_metadata` round trip),
   - diff `get_inconsistent_metadata`,
   - diff full introspection (`IntrospectionQuery` with all fields) per role (admin + every role
     in metadata, using `X-Hasura-Role` with admin secret) - compare as SDL printed in a canonical
     order (sort types/fields alphabetically **only** for SDL diff; field order of the actual
     introspection JSON should match too but is a secondary check),
   - replay captured production traffic (http-log with query logging, or a proxy capture).
4. **Generated queries**: for each role, generate random valid queries from the introspected
   schema (random selection sets, `where`/`order_by`/`limit`/`distinct_on`/aggregates, variables)
   - a grammar-based fuzzer; also mutate into invalid queries to diff validation errors.
5. **Mutations**: run in a transaction-isolated way: for each mutation case, restore both data DBs
   from a template (`CREATE DATABASE data_a TEMPLATE seed`) or wrap in a sequence that is
   deterministic (sequences reset). Compare responses and then compare table contents
   (`SELECT * ORDER BY pk`) after the mutation.
6. **Subscriptions**: open the same subscription on both, apply the same sequence of mutations
   through a third path (direct SQL to both DBs), collect the message streams, compare
   sequences after collapsing duplicates (timing differs; compare the set of distinct payloads
   and the final payload).
7. **Event triggers / actions / remote schemas**: route all webhooks to the recorder; compare
   delivered payloads (normalize ids/timestamps), headers sent, retry behaviour, and the
   responses returned to the client.

### 4.3 Normalization rules before diffing

- HTTP: compare status code exactly; compare `Content-Type`; ignore `Date`, `x-request-id`
  value (but require presence), `Server`, `Content-Length` (when gzip), `Vary` ordering.
- JSON body:
  - GraphQL `data`: exact, including key order (use an order-preserving JSON parser).
  - `errors`: compare as list in order; `message`, `extensions.code`, `extensions.path` exact;
    `extensions.internal` compared only for a curated set (SQL text and Postgres error `statement`
    differ - drop `internal.statement`, `internal.arguments`, `internal.prepared`; keep
    `internal.error.status_code` and `exec_status`).
  - Metadata API: exact, but known nondeterministic fields removed: `resource_version`
    (compare monotonicity instead), event ids (UUIDs), `created_at`, timestamps, `hasura_uuid`.
  - Numeric formatting: compare JSON numbers by value **and** textual representation for
    `float8`/`numeric` (upstream prints Postgres text output; e.g. `1e+20` vs `100000000000000000000` matters).
- Introspection: exact JSON, plus SDL diff for readability.
- Logs: diff `http-log` / `websocket-log` structure (keys and types) only.
- Generated SQL: never diff text; optionally diff `EXPLAIN` plan shapes for performance issues.

### 4.4 Reporting

Store each mismatch as `{case_id, request, upstream_response, rust_response, diff}` JSONL; group by
`(endpoint, error code, first differing path)`; maintain an allowlist file of accepted deviations
(each with justification, e.g. "PORT env extension"; note the startup-log password masking is
upstream behaviour too, not a deviation - doc 11 5.4).

### 4.5 Running the upstream Python suite against both

Mode B (`--hge-urls`) can point at either engine, so run:
`pytest --hge-urls http://localhost:8080 ...` (upstream) and `--hge-urls http://localhost:8081 ...`
(rust) with identical env; `--accept` on a scratch copy of `queries/` for each, then `diff -r` the
two rewritten `queries/` trees - this yields a strict, file-level diff of all actual responses with
zero custom tooling (`--accept` rewrites a fixture file only when the response did not match - `validate.py` checks `--accept and not matched`; run on both engines to capture each one's outputs).

---

## 5. `server/benchmarks`

- `bench.sh <benchmark_set> [<hasura_docker_image>] [<sleep_before_bench>]`
  (`server/benchmarks/bench.sh`): starts Postgres in Docker (port 25432; restores
  `benchmark_sets/<set>/dump.sql.gz`), starts the engine image with
  `HASURA_GRAPHQL_DATABASE_URL`, `HASURA_GRAPHQL_ENABLE_CONSOLE=true`,
  `HASURA_GRAPHQL_SERVER_PORT=8181`, `HASURA_GRAPHQL_ADMIN_SECRET=my-secret` (when no EE key),
  `HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL=0`, `HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL=0`,
  command `graphql-engine serve +RTS -T ... -RTS` (our binary must ignore the RTS span), waits for
  `/v1/query` to respond, applies `replace_metadata.json`, clones and builds `graphql-bench`
  (k6-based) and runs `config.query.yaml`, writing `report.json`. `adhoc_operations/` scripts read
  `/dev/rts_stats` for memory stats (upstream-only; skip or provide compatible JSON).
- Sets: `chinook` (classic Chinook DB, roles `employee`/`customer`; headers
  `X-Hasura-Admin-Secret: my-secret`, `X-Hasura-Role: employee`, `X-Hasura-User-Id: 4`,
  `Accept-Encoding: gzip`; queries `simple_query_low_load` (20 rps) / `simple_query_high_load` (500 rps),
  `complex_query_{low,high}_load_{small,large}_result`, `full_introspection`; k6
  `constant-arrival-rate`, 60 s, `maxVUs: 500`, `discardResponseBodies: true`), `chinook_throughput`
  (max throughput), `huge_schema` (3.7 MB metadata - schema build time and memory), `deep_schema`
  (deeply nested relationships), `remote_schema` and `9447_repro` (marked `SKIP_CI`).
- Reuse: run `bench.sh chinook hasura/graphql-engine:v2.51.0-ce` and `bench.sh chinook hasura-rs:dev`
  (our image must provide a `graphql-engine` executable on PATH, or adapt the script's command)
  and compare p50/p95/p99 latency, throughput, memory. `huge_schema` is the key test for metadata
  load time and schema cache memory. The chinook dataset + metadata is also a good fixed corpus
  for differential query testing (section 4).

---

## 6. Harness compatibility requirements on our binary (checklist)

- CLI: `graphql-engine --metadata-database-url URL serve --enable-telemetry=false --server-port N --stringify-numeric-types [--admin-secret K]`; also `serve ... --metadata-database-url URL` order; `+RTS ... -RTS` and `+Heartbeat ... -Heartbeat` ignored.
- Env: all `HASURA_GRAPHQL_*` used in `hge_env` markers parsed; `{{ENV}}` templates and `from_env` for webhook URLs (`EVENT_WEBHOOK_HANDLER` etc.).
- Starts within 30 s on an empty metadata DB as a **non-superuser** with `pgcrypto` pre-created (must not fail if `CREATE EXTENSION` is not permitted when the extension already exists).
- Exits within 5 s of SIGTERM when idle (else harness kills it - not fatal but slow).
- `/v1/version` returns `{"version": $VERSION}`-compatible output (configure via build-time string; set the harness `VERSION` env to match). `test_version.py` passes unconditionally when the reported version is the literal `"12345"` (upstream's local-dev magic value) - do not ship that, but it is handy early on.
- `/v1/query` (incl. `bulk`, `run_sql`, v1 metadata commands), `/v2/query`, `/v1/metadata` (`export_metadata`, `replace_metadata`, `clear_metadata`, `reload_metadata`) work from the first request.
- WebSocket on `/v1/graphql`, `/v1alpha1/graphql`, `/v1/relay`, `/v1beta1/relay` for both subprotocols.
- Logs to stdout as JSON lines (for `capture_hge_logs`).

---

## Upstream tests to reuse

- `server/tests-py/` whole suite in `--hge-bin` mode, Postgres only - primary golden suite (see 2.2 table for per-file priority). Skip `test_query_cache.py`, `test_heterogeneous.py`, classes/fixtures using `enterprise_edition`, `pro_tests_fixtures`, `--pro-tests`, `--redis-url`, and backend variants (`*_mssql.yaml`, `*_citus.yaml`, `bigquery/`, `subscriptions/mssql/`).
- `server/tests-py/queries/clear_db.yaml` - DB reset used in external mode; must work on our engine.
- `server/tests-py/test_tests.py` + `server/tests-py/test_tests/` - harness self-tests; run first to validate the setup.
- `server/lib/api-tests/src/Test/HelloWorldSpec.hs`, `HealthCheckSpec.hs`, `NamingConventionsSpec.hs`, `Regression/OptimizePermissionFiltersSpec.hs`, `Queries/Relationship/PermissionRootReferenceSpec.hs`, `Queries/PermissionsKnowAboutColumnNullabilitySpec.hs`, `Queries/Simple/ObjectQueriesSpec.hs` - runnable against our binary via `GRAPHQL_ENGINE`.
- `server/lib/api-tests/src/Test/{Queries,Mutations,Schema,Auth,Regression,API/Metadata,EventTriggers,Subscriptions}/` - fixture source to port (Postgres variants only); skip `DataConnector/` and non-Postgres `Databases/`.
- `server/benchmarks/benchmark_sets/chinook/` (`dump.sql.gz`, `replace_metadata.json`, `config.query.yaml`) - performance comparison and differential corpus; `huge_schema/`, `deep_schema/` - schema-build stress; skip `remote_schema/`, `9447_repro/` initially.
