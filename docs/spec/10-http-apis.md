# 10 — HTTP API surface (routes, envelopes, headers, CORS, compression)

Spec for the HTTP layer of a drop-in Rust replacement of Hasura GraphQL Engine V2 (CE/OSS).
Ground truth: `server/src-lib/Hasura/Server/App.hs` (routing, `mkSpockAction`, `httpApp`),
`server/src-lib/Hasura/App.hs` (CE instances of the pluggable type classes), `server/src-lib/Hasura/Server/{Cors,Middleware,Compression,Types,Rest}.hs`,
`server/src-lib/Hasura/Server/API/{Query,V2Query,Metadata,Config,PGDump}.hs`,
`server/src-lib/Hasura/GraphQL/Transport/HTTP.hs` and `.../HTTP/Protocol.hs`, `server/src-lib/Hasura/GraphQL/Explain.hs`.
Error *formats* and the full `code` catalogue live in `12-errors.md`; this doc references them.

Conventions: "QErr JSON" = the non-GraphQL error object `{"error","path","code"[, "internal"]}`;
"GQL error JSON" = `{"errors":[{"message", "extensions":{"path","code"[, "internal"]}}]}` (see 12-errors.md §2).

---

## Scope and priority

| Area | Priority | Notes |
|---|---|---|
| `POST /v1/graphql` (single + batched), error→200 mapping, `x-request-id`, gzip | **Essential** | Every client uses it. |
| `GET /healthz`, `/hasura/healthz`, `GET /v1/version` | **Essential** | Load balancers / Cloud Run probes, CLI (`hasura version`). |
| `POST /v1/metadata`, `POST /v2/query` (run_sql + DML) | **Essential** | CLI `hasura metadata apply`, `hasura migrate apply` use these. |
| `POST /v1/query` (legacy combined API) | **High** | Older CLIs/projects and many tests-py fixtures; console still issues some calls. |
| CORS middleware (`HASURA_GRAPHQL_CORS_DOMAIN`, `HASURA_GRAPHQL_DISABLE_CORS`) | **Essential** | Browser clients. |
| `HASURA_GRAPHQL_ENABLED_APIS` gating, 404 for unknown routes | **Essential** | Security posture of prod deployments. |
| `/api/rest/*` RESTified endpoints | **High** | Used by many projects (metadata `rest_endpoints.yaml`). |
| `POST /v1/relay`, `/v1beta1/relay` | Medium | Only for projects using Relay. |
| `POST /v1alpha1/graphql` (legacy status codes) | Medium | Legacy clients. |
| `GET /v1alpha1/config` | Medium | Console + CLI read it. |
| `POST /v1/graphql/explain` (+ `/v1alpha1/graphql/explain`) | Medium | Console "Analyze" button. |
| `GET /console`, `/console/assets/*`, `GET /` redirect | Medium | Only with `HASURA_GRAPHQL_ENABLE_CONSOLE=true` (default **false**). Serve the same HTML template loading assets from the Hasura CDN. |
| `POST /v1alpha1/pg_dump` | Low-Medium | Used by `hasura migrate create --from-server`; requires a `pg_dump` binary (not available on Cloudflare Workers; ship it in the container for Cloud Run). |
| `GET /api/swagger/json` | Low | OpenAPI for REST endpoints. |
| `/dev/*` developer endpoints | Low | Only with `developer` in enabled APIs; content is implementation-specific. |
| `GET /v1/graphql` | Low | CE: always returns `PersistedQueryNotSupported` error. APQ/persisted queries are **EE-only**. |
| `/v1/metrics` (Prometheus), query-cache headers (`X-Hasura-Query-Cache-Key`, `Cache-Control`, `Warning`), APQ, `/v1/entitlement`, API limits, OpenTelemetry export config UI | **EE/Cloud-only** | Not routed in CE source. A Rust impl may add `/v1/metrics` as an extension, but CE parity does not require it. |

---

## 1. Server/transport basics

* Single HTTP server (Warp) on `HASURA_GRAPHQL_SERVER_PORT` (default `8080`), host `HASURA_GRAPHQL_SERVER_HOST` (default `*`). WebSocket upgrades on the same port are intercepted *before* HTTP routing (`WSC.websocketsOr` in `mkWaiApp`); only paths `/v1/graphql`, `/v1alpha1/graphql`, `/v1/relay`, `/v1beta1/relay` are accepted for WS (else 404 `only '/v1/graphql', '/v1alpha1/graphql', '/v1/relay' and '/v1beta1/relay' are supported on websockets`), see `server/src-lib/Hasura/GraphQL/Transport/WebSocket.hs` (`checkPath`). WS protocol details: see the subscriptions spec.
* Warp settings (`server/src-lib/Hasura/App.hs` `runHGEServer`): graceful shutdown timeout 30 s; **no `Server` header** (`setServerName ""`); max total request header length `HASURA_GRAPHQL_MAX_TOTAL_HEADER_LENGTH` (default 1 MiB = 1048576 bytes; Warp answers `431` when exceeded (inferred)); connection inactivity timeout `HASURA_GRAPHQL_SERVER_TIMEOUT` seconds (default 30).
* **No request body size limit** is enforced by Hasura (`Wai.strictRequestBody` reads it whole). A Rust impl should allow large bodies (metadata `replace_metadata` payloads can be many MB); a configurable generous limit (e.g. 100 MB) is acceptable (deviation, document it).
* Every response gets a `Date` header (RFC 7231 format) from `dateHeaderMiddleware` (`Hasura/Server/Middleware.hs`).
* Routing library is Spock. Paths are matched literally (no trailing-slash normalisation is guaranteed; treat `/v1/graphql/` as unknown (inferred)).
* Route registration order matters only for `/api/rest/*` (wildcard, all methods) and the final catch-alls.

### 1.1 Middleware order (outermost first)
1. WebSocket upgrade interception.
2. CORS middleware (§9) — may short-circuit `OPTIONS` preflights with `204` **for any path**.
3. Date header middleware.
4. Spock routes.

---

## 2. Common request pipeline (`mkSpockAction`)

All "API" routes (GraphQL, metadata, query, explain, config, pg_dump, dev, REST, swagger) go through `mkSpockAction` (`App.hs`). Exceptions (raw handlers): `/healthz`, `/hasura/healthz`, `/v1/version`, `/console*`, `/`, `/dev/rts_stats`, the 404 catch-all, and the "API disabled"/"Relay disabled" short-circuits.

Steps:
1. Read the full body.
2. **Request id**: if request header `x-request-id` (case-insensitive) is present, use its value verbatim; else generate a random UUID v4 string (`generateFingerprint`) and add it to the request headers passed downstream (so it is forwarded to webhooks etc.). `Hasura/Server/Types.hs` `getRequestId`.
3. Tracing context extraction (OTel propagators from metadata) — no response effect.
4. Authentication + body parsing, order depends on handler kind:
   * `AHGet` (GET handlers): authenticate (`resolveUserInfo` with no request body).
   * `AHPost` (metadata/query/v2/explain/pg_dump): **authenticate first**, then parse body JSON. Auth failure → error response with internal errors hidden. Body parse failure → error (internal errors per role policy).
   * `AHGraphQLRequest` (`/v1alpha1/graphql`, relay): **parse body first** (so the auth webhook can receive the GraphQL request in POST mode). If parsing fails, auth is still run (without body) only to obtain a role for logging; the parse error is returned with internal hidden.
   * `AHPersistedGraphQLRequest` (`/v1/graphql` POST): same as above; for APQ-shaped bodies, the webhook receives `{query, variables, operationName}` reconstructed (EE path; CE errors anyway).
5. Body parsing (`parseBody`): **every byte `< 0x20` in the raw body is replaced by a space (0x20) before JSON decoding** (compat for clients embedding raw newlines/tabs inside JSON strings). Then strict JSON decode (`eitherDecode'`): failure → `400 invalid-json`, message = decoder error text. Then `FromJSON` decode of the request type: failure → `400 parse-failed` with the aeson JSON path and message (see 12-errors.md §5). Important: the control-char mapping means a GraphQL query containing literal newlines inside the JSON string is accepted and newlines become spaces.
6. `includeInternal` is computed from the *resolved role* and `ResponseInternalErrorsConfig` (12-errors.md §4).
7. Run handler inside the per-handler resource limiter (`askHTTPHandlerLimit`; CE: no-op). Haskell `error` calls inside a handler are caught → `500 unexpected "Internal Server Error"` with `internal: {"error": "<text>"}`.
8. Apply route's *error modifier* (`id` for most routes, `maybeMod200` for `/v1/graphql` GET/POST and relay; see §3.4).
9. Respond:
   * **Error**: status = `qeStatus` (after modifier); body = encoder(includeInternal, err) where encoder is `encodeQErr` (QErr JSON) for non-GraphQL routes and `encodeGQErr` (GQL error JSON) for GraphQL routes. Headers exactly: `x-request-id`, `Content-Length`, `Content-Type: application/json; charset=utf-8` (+ `Date`, CORS). **Error responses are never compressed** and never carry auth/webhook `Set-Cookie` headers.
   * **Success**: body bytes; for JSON responses `Content-Type: application/json; charset=utf-8` is added; compression negotiated (§10); headers in order: `x-request-id`, `Content-Length`, `Content-Encoding` (if gzipped), `Content-Type`, handler-provided headers (e.g. `Set-Cookie` forwarded from actions/remote schemas), auth headers (e.g. `Set-Cookie` returned by the auth webhook). Status 200.

Response header name for the request id is lowercase `x-request-id`.

Logging (http-log / query-log) never changes the response.

---

## 3. GraphQL endpoints

### 3.1 `POST /v1/graphql`
* Enabled when `graphql` ∈ `HASURA_GRAPHQL_ENABLED_APIS`; otherwise `404` QErr JSON `{"error":"resource does not exist","path":"$","code":"not-found"}` (note: QErr format even though it is the GraphQL route; produced by `onlyWhenApiEnabled` before `mkSpockAction`, no `x-request-id` header).
* Auth: any role (per permissions). Role resolution via admin secret / JWT / webhook / unauthorized role (see auth spec).
* Request body (`GQLReq`, `Hasura/GraphQL/Transport/HTTP/Protocol.hs`):

```json
{ "query": "query q($id: Int!) { author(where:{id:{_eq:$id}}) { name } }",
  "variables": { "id": 1 },
  "operationName": "q" }
```

  * `query`: **required** string (GraphQL document). Missing → `parse-failed` at path `$` (aeson *generic* decoder, `genericParseJSON (aesonPrefix camelCase)`, so the message carries a context prefix, roughly `parsing Hasura.GraphQL.Transport.HTTP.Protocol.GQLReq(GQLReq) failed, key "query" not found` (inferred; aeson-version dependent — match on code/path only)). A body that is a JSON string/number → `parse-failed` `parsing … failed, expected Object, but encountered String` (inferred). Internal errors are hidden for these envelope failures. On `/v1/graphql` all of these become HTTP 200 (§3.4).
  * `variables`: optional object (or `null`) mapping variable name → JSON value. Keys must be valid GraphQL names.
  * `operationName`: optional string (or `null`); must be a valid GraphQL name.
  * Extra keys are ignored (e.g. `extensions` when it has no `persistedQuery`).
  * If the body has `extensions.persistedQuery` (Apollo APQ shape) → CE returns error `400 not-supported "PersistedQueryNotSupported"` (mapped to 200, see §3.4).
* **Batching**: body may be a JSON array of `GQLReq` objects → response is a JSON array of the same length, element *i* = the full response for request *i* (`{"data":...}` or `{"errors":[...]}` or `{"data":null,"errors":[...]}`), executed **sequentially in order** (`for reqs`) — mutations in earlier items are committed before later ones run. Each element that fails with a QErr becomes `{"errors":[<gql error>]}` (internal included per role policy). **No response headers from individual operations are forwarded for batched requests** (e.g. no `Set-Cookie` from actions). Empty array `[]` → `[]` (inferred). Overall status 200. (`runGQBatched` in `Hasura/GraphQL/Transport/HTTP.hs`.)
* `Content-Type` of the request is not checked.
* Response (single): see §3.3.

### 3.2 `GET /v1/graphql`
* CE: always error `400 not-supported` message `PersistedQueryNotSupported` → after `maybeMod200` → **HTTP 200** `{"errors":[{"message":"PersistedQueryNotSupported","extensions":{"path":"$","code":"not-supported"}}]}`. (`Hasura/App.hs` instance `MonadGQLApiHandler AppM`.) Gated by `graphql` API as for POST.
* Query-string GraphQL (`?query=...`) is **not** supported in CE. A Rust impl may keep this behaviour for parity.

### 3.3 Response body shapes (POST /v1/graphql, relay, v1alpha1)
1. Success: `{"data": { <rootFieldAlias>: <value>, ... }}` — keys in selection order (insertion-ordered), namespaced remote/source customisations nest objects.
2. Any error raised before/during execution as a QErr (parse/validation/permission/DB errors, action errors, auth errors): **no `data` key**, body = `{"errors":[ <one gql error> ]}` — exactly one error object (Hasura does not accumulate multiple errors; first failure aborts the whole operation). Partial results are never returned for DB root fields.
3. Remote-schema GraphQL errors (`GQExecError`, `extractFieldFromResponse`): body = `{"data": null, "errors": [ <remote errors verbatim> ]}` (`encodeGQResp (Left e)`). Exact data/errors merging depends on `HASURA_GRAPHQL_REMOTE_SCHEMA_PRIORITIZE_DATA` — see remote-schemas spec.
4. Subscription operation over HTTP → error `400 unexpected-payload` `subscriptions are not supported over HTTP, use websockets instead`.

Mutations: if all root fields target the same Postgres source, they run in **one transaction** (`coalescePostgresMutations`); a failure in any root field rolls back all and returns form (2). Otherwise each step runs separately, errors abort subsequent steps (form 2).

Successful mutation responses can carry `Set-Cookie` headers from sync action handlers and remote schemas (only `Set-Cookie` is forwarded: `mkSetCookieHeaders`, `Transport/HTTP.hs` `runRemoteGQ`). Query responses carry `Set-Cookie` from remote schemas too.

### 3.4 Status-code rule for `/v1/graphql` and relay: `maybeMod200`
After the handler, the QErr status is rewritten:
* default (`HASURA_GRAPHQL_PRESERVE_401_ERRORS` unset/false): **every error → HTTP 200** (including auth failures, invalid JSON, 500 internal errors).
* `HASURA_GRAPHQL_PRESERVE_401_ERRORS=true`: errors whose status is 401 keep 401 (e.g. `invalid "x-hasura-admin-secret"`, auth webhook returning 401); all others → 200. JWT verification errors are 400 internally, so they still become 200.
The "API disabled" 404 and the Relay-disabled 400 are produced outside `mkSpockAction` and are **not** rewritten.

### 3.5 `POST /v1alpha1/graphql` (legacy)
* Same request/response bodies and GQL error JSON as `/v1/graphql`, gated by `graphql` API.
* Difference: **no `maybeMod200`** — HTTP status is the QErr status: 400 for validation/parse/constraint errors, 401 for auth failures, 500 for unexpected, 404 etc. Successful responses (including `{"data":null,"errors":[...]}` from remote schemas) are 200. Batching supported; batched responses are always 200 (per-item errors are embedded).
* Does not support APQ shape (plain `GQLReq`/array only; `ReqsText` parser — an `extensions` key is simply ignored).
* WebSocket on `/v1alpha1/graphql` uses the legacy (non-GraphQL-compliant) error payload — see subscriptions spec. Test: `server/tests-py/test_v1alpha1_endpoint.py`.

### 3.6 `POST /v1/relay` and `POST /v1beta1/relay`
* Identical semantics; served against the Relay schema (`E.QueryRelay`). Gated by `graphql` API, then by `HASURA_GRAPHQL_ENABLE_RELAY` (default **true** in this source tree; older docs say relay is always on).
* When relay is disabled: HTTP **400** QErr JSON (not GraphQL format, not mapped to 200), no `x-request-id`:
  `{"error":"The Relay API is disabled. To enable it, set the HASURA_GRAPHQL_ENABLE_RELAY environment variable to 'true'.","path":"$","code":"not-supported"}`.
* Error modifier `maybeMod200` applies (same as `/v1/graphql`). No GET route.

### 3.7 `POST /v1/graphql/explain` and `POST /v1alpha1/graphql/explain`
* Gated by **`metadata`** API (not `graphql`). Admin only: non-admin → `400 access-denied` `You have to be an admin to access this endpoint` (QErr JSON, path `$`). Errors use QErr JSON with real status codes.
* Request (`GQLExplain`, `Hasura/GraphQL/Explain.hs`, hasuraJSON snake_case, nothing fields omitted):

```json
{ "query": { "query": "query getUsers { user { name } }", "operationName": "getUsers", "variables": {} },
  "user": { "x-hasura-role": "user", "x-hasura-user-id": "1" },
  "is_relay": false }
```
  * `query`: required object (`GQLReq` whose `query` string is parsed as a GraphQL document during JSON decoding — a syntax error is a `parse-failed` at `$.query.query` (inferred)).
  * `user`: optional session-variable map; role taken from `x-hasura-role` in it, default `admin`. The plan is generated as that role (permissions applied), executed with admin-secret-sent semantics.
  * `is_relay`: optional bool, `true` → use Relay schema.
* Response for a **query**: JSON array, one element per root field, in order:
  `[{"field":"user","sql":"<generated SQL>","plan":["<line1>","<line2>",...]}]`
  `plan` is the output of `EXPLAIN <sql>` (text format, one row per array element; **not** `EXPLAIN ANALYZE`). Introspection/`__typename`/multi root fields produce `{"field":"<alias>","sql":null,"plan":null}`. Session variables are substituted as literals (`prepareWithoutPlan`), query variables inlined.
* Response for a **subscription**: single object
  `{"sql":"<multiplexed SQL>","plan":[...],"variables":{"session":{...},"query":{...},"synthetic":[...],"cursor":{...}}}` (key order of `variables`: session, query, synthetic, cursor — `CohortVariables` ToJSON). The SQL is the multiplexed form `... FROM UNNEST(($1)::uuid[], ($2)::json[]) AS "_subs"(...)`.
* Errors (all `400`):
  * mutation: `invalid-params` `only queries can be explained`
  * remote schema root field: `invalid-params` `only hasura queries can be explained`
  * action root field: `invalid-params` `query actions cannot be explained`
  * remote relationships in selection: `invalid-params` `queries with remote relationships cannot be explained`
  * async action subscription without relationships: `not-supported` `async action query fields without relationships to table cannot be explained`
  * no subscription root: `not-found` `no subscriptions found`
* Docs: `docs/docs/api-reference/explain.mdx`. Fixtures: `server/tests-py/queries/explain/*.yaml`.

---

## 4. Metadata / schema APIs

All three are gated by **`metadata`** API, use QErr JSON errors with real status codes, `id` error modifier, AHPost pipeline (auth before body parse).

### 4.1 `POST /v1/metadata`
* Admin only: CE `authorizeV1MetadataApi` → non-admin role: `400 access-denied` `restricted access : admin only` with path `$.args` (`Hasura/App.hs`, `accessDeniedErrMsg`).
* Envelope (`RQLMetadata`, `Hasura/Server/API/Metadata.hs` + `Metadata/Instances.hs`):

```json
{ "type": "pg_track_table", "args": { ... }, "version": 1, "resource_version": 7 }
```
  * `type`: required string. Backend-agnostic commands (`add_remote_schema`, `bulk`, `bulk_keep_going`, `bulk_atomic`, `replace_metadata`, `export_metadata`, `reload_metadata`, ...) or backend-prefixed `<kind>_<cmd>` (`pg_`, `citus_`, `cockroach_`, `mssql_`, `bigquery_`, data-connector kinds). Unknown prefix → parse-failed `unknown metadata command "<type>"; "<prefix>" was not recognized as a valid backend name`; known prefix but unknown cmd → `unknown metadata command "<cmd>" for backend <kind>` (path `$`).
  * `args`: command arguments (most commands require it; `get_cron_triggers`, etc. ignore it). Argument parse errors are wrapped: message `Error when parsing command <cmd>.\nSee our documentation at https://hasura.io/docs/latest/graphql/core/api-reference/metadata-api/index.html#metadata-apis.\nInternal error message: <aeson msg>`, code `parse-failed`, path `$.args...` (`Hasura/Server/API/Backend.hs`) — this wrapping only applies to backend-prefixed commands.
  * `version`: optional `1` (default) or `2` (v2 commands: `replace_metadata` v2, `export_metadata` v2). Other values → `parse-failed` `expected 1 or 2, encountered 3` at `$.version`.
  * `resource_version`: optional integer; if given and the command modifies metadata, the write is conditional: mismatch → `409 conflict` `metadata resource version referenced (<n>) did not match current version` (`Hasura/RQL/DDL/Schema/Catalog.hs`).
* Responses: command-specific JSON; the common success body is `{"message":"success"}`. `bulk` → JSON array of sub-results (stops at first error, error returned as whole response); `bulk_keep_going` → array with error objects inline (status 200); `bulk_atomic` → all-or-nothing. Full catalogue: metadata spec.
* Read-only/maintenance mode: metadata-modifying commands fail: read-only → `400 not-supported` `metadata cannot be modified in read-only mode`; maintenance → `500 unexpected` `metadata cannot be modified in maintenance mode`.
* Side effects: after a successful modifying command, the new metadata is stored with incremented resource_version and a schema-sync notification is emitted; if `HASURA_GRAPHQL_CLOSE_WEBSOCKETS_ON_METADATA_CHANGE` (default true) websockets are closed with code 1012.
* The handler holds a global schema-cache write lock for the whole request (serialises metadata writes).

### 4.2 `POST /v2/query` (Schema API)
* **Admin only for every query type** in CE (`authorizeV2QueryApi` rejects any non-admin, even `select`): `400 access-denied` `restricted access : admin only`, path `$.args`. Fixture: `server/tests-py/queries/v2/basic/select_article_role_error.yaml`.
* Envelope: `{"type": "<t>", "args": {...}}` (`Hasura/Server/API/V2Query.hs`). Types:
  * `run_sql` / `pg_run_sql`, `citus_run_sql`, `cockroach_run_sql`, `mssql_run_sql`, `bigquery_run_sql`, `<dataconnector>_run_sql`, `bigquery_database_inspection`
  * DML: `insert`, `select`, `update`, `delete`, `count` (Postgres default-source DML, same args as v1/query §4.4)
  * `bulk` (array of v2 queries, sequential, results array; first error aborts the whole request and only the error is returned). **Not transactional**: every DML sub-query opens its own transaction (`runInsert`/`runUpdate`/`runDelete` → `runTxWithCtx … (Tx PG.ReadWrite Nothing)`, `runSelect`/`runCount` → `Tx PG.ReadOnly`; `run_sql` → its own `runTxWithCtx`/`withMetadataCheck`), so DB effects of earlier elements stay committed when a later element fails. Metadata changes (from DDL `run_sql`) are persisted only after the whole bulk succeeds (`runQuery` stores `updatedMetadata` once at the end). Verified in `Hasura/Server/API/V2Query.hs` + `Hasura/RQL/DML/*.hs`.
  * `concurrent_bulk` (array; if any element `queryModifiesSchema` (i.e. a DDL-ish `run_sql`) → `500 unexpected` `Only read-only queries are allowed in a concurrent_bulk`; elements run concurrently (`mapConcurrently`); results array in input order; first failure is the response). Used by the console. Note DML `insert/update/delete` are *not* rejected by this check (only schema-modifying elements are).
  * Unknown type → `400 parse-failed` `Unrecognised RQLQuery type: <t>`, path `$`. Missing `type` → `key "type" not found` (path `$`).
* **Error paths differ from `/v1/query`** (`FromJSON RQLQuery` in `V2Query.hs` is hand-written with `o .: "args"`): argument *decoding* errors carry the `args` key, e.g. `$.args.sql`, `$.args[1].args.table` (bulk); *execution* errors are **not** prefixed with `$.args` (`v2QueryHandler` has no `withPathK "args"`): run_sql failure → path `$`; inside `bulk` → `$[i]` (`indexedMapM`). Compare v1 (§4.3), where execution errors are under `$.args`. Example: v2 `bigquery_run_sql` failure fixture has `path: $` (`server/lib/api-tests/src/Test/API/Schema/RunSQLSpec.hs`).
* `run_sql` args (`Hasura/Backends/Postgres/DDL/RunSQL.hs`):

| field | type | default | meaning |
|---|---|---|---|
| `sql` | string | required | SQL text (may contain multiple statements) |
| `source` | string | `"default"` | source name |
| `cascade` | bool | `false` | when dropping objects referenced by metadata, drop the dependent metadata instead of failing |
| `check_metadata_consistency` | bool | absent → heuristic | whether to diff schema & reload metadata afterwards. Absent: true iff not read-only and the SQL matches case-insensitive regex `\balter\b|\bdrop\b|\breplace\b|\bcreate function\b|\bcomment on\b` |
| `read_only` | bool | `false` | run in a `READ ONLY` transaction; never triggers metadata check |
| `no_transaction` | bool | `false` | run statements outside a transaction (needed for `CREATE INDEX CONCURRENTLY`); statements are split and run one by one, result of the last returned |

  Response (`RunSQLRes`, `Hasura/RQL/DDL/Schema.hs`, `PG.FromRes RunSQLRes`): keys always `result_type` then `result` (generic `toEncoding`, `result` is a plain `Value`, so it is **always present**, `null` for CommandOk — the docs' "present only when TuplesOk" is wrong):
  * libpq `PGRES_COMMAND_OK` (DDL, INSERT/UPDATE/DELETE without RETURNING, SET, …) → `{"result_type":"CommandOk","result":null}`
  * libpq `PGRES_TUPLES_OK` (SELECT, `… RETURNING`, SHOW, EXPLAIN) → `{"result_type":"TuplesOk","result":[["col1","col2"],["v11","v12"],...]}`. Row 0 = column names (`PQfname`; missing name → `"unknown"`). Every cell is the **Postgres text-format output as a JSON string** (simple-query protocol): ints `"1"`, booleans `"t"`/`"f"`, json/jsonb as their text, arrays `"{1,2}"`, timestamps per server `DateStyle`; SQL NULL → the literal string `"NULL"` (indistinguishable from the text `'NULL'`). A SELECT with zero rows → `[["col1","col2"]]` (header only). Cells that are not valid UTF-8 → error.
  * Multiple statements in `sql` (without `no_transaction`) are sent as one simple-query (`PG.multiQE`), run in the request's transaction, and the result is that of the **last** statement (libpq `PQexec` semantics).
  * Example fixture (`server/tests-py/queries/v1/run_sql/sql_select_query_read_only.yaml`): `SELECT alterable, droppable FROM malicious` → `{"result_type":"TuplesOk","result":[["alterable","droppable"],["t","f"],["f","t"]]}`.
  * Errors: `400 postgres-error` `query execution failed` with `internal` = PGTxErr JSON `{statement, prepared: false, error:{exec_status, hint, message, status_code, description}, arguments: []}` (12-errors.md §6), internal shown per role policy (admin only by default). Path: `$.args` on `/v1/query`, `$` on `/v2/query`. Connection failures → 500 per 12-errors.md §6.3.
  * Transaction mode: `read_only: true` → `BEGIN … READ ONLY` (Postgres itself then rejects writes: `25006 cannot execute DROP TABLE in a read-only transaction`); `no_transaction: true` → no `BEGIN`, statements split with a comment/quote/dollar-quote–aware splitter on `;` (`splitSQLStatements`) and executed one by one; output of the last is returned; earlier ones stay applied if a later one fails.
  * Metadata check (`check_metadata_consistency` true/heuristic): SQL runs inside `withMetadataCheck` — event-trigger SQL triggers are dropped before and recreated after, schema diff applied to metadata (renames/drops); if dependent metadata exists and `cascade=false` the whole tx is rolled back with a `dependency-error` (see metadata spec).
* Read-only mode (`HASURA_GRAPHQL_ENABLE_READ_ONLY_MODE`, `queryModifiesUserDB` in `V2Query.hs`): `insert`, `update`, `delete`, `run_sql`/`citus_`/`cockroach_` with `read_only` absent/false, every `mssql_run_sql`/`bigquery_run_sql`/`<dc>_run_sql`, and a `bulk` containing any of them → `400 not-supported` `Cannot run write queries when read-only mode is enabled` (path `$`). `select`, `count`, `run_sql` with `read_only:true`, and `concurrent_bulk` are allowed.
* Maintenance mode: a query that `queryModifiesSchema` → `500 unexpected` `metadata cannot be modified in maintenance mode` (raised *after* the SQL already ran).
* Success body for DML: same as v1/query DML (§4.4).

### 4.3 `POST /v1/query` (legacy combined Schema/Metadata API, deprecated but live)
* Envelope `{"type","args","version"?}` parsed with aeson `TaggedObject "type" "args"` — **note: parse errors inside `args` report paths relative to `args` (e.g. `$`, `$.name`, `$.on_conflict`) because aeson tagged-object decoding does not add `args` to the path**, while errors raised during *execution* are prefixed with `$.args` (`runQueryM` wraps everything in `withPathK "args"`). Fixtures: `server/tests-py/queries/v1/basic/query_args_as_string_err.yaml` (`path: $`), `v1/update/basic/person_error_no_where_clause.yaml` (`key "where" not found`, path `$`).
* `version`: 1 (default) or 2; v2 types: `track_table` (v2 args with `configuration`), `set_table_custom_fields` (deprecated), `track_function` (v2), `replace_metadata` (v2).
* v1 types (snake_case of constructors, `Hasura/Server/API/Query.hs`): `add_existing_table_or_view`, `track_table`, `untrack_table`, `set_table_is_enum`, `set_table_customization`, `track_function`, `untrack_function`, `create_object_relationship`, `create_array_relationship`, `drop_relationship`, `set_relationship_comment`, `rename_relationship`, `add_computed_field`, `drop_computed_field`, `create_remote_relationship`, `update_remote_relationship`, `delete_remote_relationship`, `create_insert_permission`, `create_select_permission`, `create_update_permission`, `create_delete_permission`, `drop_insert_permission`, `drop_select_permission`, `drop_update_permission`, `drop_delete_permission`, `set_permission_comment`, `get_inconsistent_metadata`, `drop_inconsistent_metadata`, `insert`, `select`, `update`, `delete`, `count`, `bulk`, `add_remote_schema`, `update_remote_schema`, `remove_remote_schema`, `reload_remote_schema`, `introspect_remote_schema`, `create_event_trigger`, `delete_event_trigger`, `redeliver_event`, `invoke_event_trigger`, `create_cron_trigger`, `delete_cron_trigger`, `create_scheduled_event`, `create_query_collection`, `rename_query_collection`, `drop_query_collection`, `add_query_to_collection`, `drop_query_from_collection`, `add_collection_to_allowlist`, `drop_collection_from_allowlist`, `run_sql`, `replace_metadata`, `export_metadata`, `clear_metadata`, `reload_metadata`, `create_action`, `drop_action`, `update_action`, `create_action_permission`, `drop_action_permission`, `create_rest_endpoint`, `drop_rest_endpoint`, `dump_internal_state`, `set_custom_types`. All operate on the `default` Postgres source (args identical to the `pg_*` metadata commands without `source`).
* **Authorization**: `requiresAdmin` — every type except `insert`, `select`, `update`, `delete`, `count` requires admin; `bulk` requires admin iff any element does. Non-admin admin-only query → `400 access-denied` `restricted access : admin only` path `$.args`. DML by non-admin roles is evaluated with that role's table permissions (legacy RQL permission semantics; `permission-denied`/`permission-error` errors).
* `bulk`: array of v1 queries, results array (one element per sub-query, in order); first error aborts and becomes the whole response; sub-query execution error paths `$.args[i].args...` (`runQueryM` = `withPathK "args"` + `indexedMapM`); metadata changes only persisted if the whole bulk succeeds; **DB side effects of earlier sub-queries are not rolled back** (verified: each DML/run_sql sub-query runs its own `runTxWithCtx`, see §4.2).
* Unknown `type` (v1 TaggedObject generic decoding) → `400 parse-failed`, path `$`, aeson-generated message listing the allowed tags (wording aeson-version dependent; do not match on it).
* Read-only mode (`queryModifiesUserDB` in `Query.hs`) differs from v2: **every** `run_sql` is treated as a write (even `read_only: true`), as are `insert`/`update`/`delete`, `create_event_trigger`, `delete_event_trigger`, `replace_metadata` (v1 and v2) → `400 not-supported` `Cannot run write queries when read-only mode is enabled`. Maintenance mode: schema-cache-modifying queries → `500 unexpected` `metadata cannot be modified in maintenance mode`.
* Holds the schema-cache write lock only if `queryModifiesSchemaCache`.

### 4.4 JSON DML (v1/query and v2/query)
Arg schemas (`Hasura/RQL/DML/Types.hs`); all accept optional `"source"` (default `"default"`), `table` is `"name"` or `{"schema","name"}`:

All five are Postgres-only (vanilla; `askSourceConfig @('Postgres 'Vanilla)`), evaluated with the caller's role (admin = no filters; other roles only on `/v1/query`, using the table's insert/select/update/delete permissions incl. filters, presets, column lists, `check`, select `limit`). Values for columns are JSON values parsed by the column's Postgres type (same scalar parsing as GraphQL inputs). Number output honours `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`.

**`select`** (`DMLQuery SelectQ`):

| key | type | req | notes |
|---|---|---|---|
| `table` | `"name"` \| `{"schema","name"}` | yes | schema defaults to `public` |
| `source` | string | no | default `"default"` |
| `columns` | array of column-spec | **yes** | `"col"`; `"*"` (all columns); `"*.*"`, `"*.*.*"` (all columns + all relationships recursively to that depth); relationship object `{"name": <rel>, "alias"?: <str>, "columns": [...] (required), "where"?, "order_by"?, "limit"?, "offset"?}` (`where/order_by/limit/offset` not allowed on object relationships: `when selecting an 'obj_relationship' 'where', 'order_by', 'limit' and 'offset'  can't be used` (sic, two spaces)). Non-string/non-object → `A column should either be a string or an object (relationship)` |
| `where` | BoolExp | no | legacy RQL bool-exp: `{"col": {"$eq"\|"_eq": v, ...}}`, `$and/$or/$not`, `_and/_or/_not`, `$exists`, relationship nesting |
| `order_by` | string \| object \| array | no | string `"+col"` / `"-col"` / `"col"` (no prefix = default asc), `"rel.col"` traverses object relationships; object `{"column": "col"|"rel.col" (req), "type"?: "asc"\|"desc", "nulls"?: "first"\|"last"}`; array of either. Bad string → `string format for 'order_by' entry : {+/-}column Eg : +posted`; bad array element → `expecting an object or string for order by`; other JSON → `Expecting : array/string/object` |
| `limit`, `offset` | int | no | negative → `400 not-supported` `unexpected negative value` (path `$.args.limit` on v1); string → `parse-failed` `parsing Int failed, expected Number, but encountered String` |

Response: JSON **array** of row objects (keys in `columns` order; relationship keys use `alias` else rel name; object rel → object/`null`, array rel → array). Example: `{"type":"select","args":{"table":"author","columns":["id","name"],"where":{"id":1}}}` → `[{"id":1,"name":"Author 1"}]`. Runs in a READ ONLY transaction.

**`insert`** (`InsertQuery`): `{table (req), source?, objects (req): object | [object] (empty → 400 unexpected-payload "objects should not be empty"), on_conflict?: {constraint?: name, constraint_on?: col | [col], action (req): "ignore" | "update"}, returning?: [col]}`.
  * `on_conflict` rules (`buildConflictClause`): `action:"ignore"` with neither → `ON CONFLICT DO NOTHING`; `action:"update"` needs exactly one of `constraint`/`constraint_on` else `Expecting 'constraint' or 'constraint_on' when the 'action' is 'update'`; both given → `'constraint' and 'constraint_on' cannot be set at a time`; unknown constraint → `400 unexpected` `constraint "x" for table "t" does not exist` (path `…constraint`); bad action → parse-failed `Expecting 'ignore' or 'update'`. `update` updates the inserted columns (respecting the role's update permission filter/preset).
  * Non-insertable column for role → `400 not-supported` `column "c" is not insertable for role "r"`.

**`update`** (`UpdateQuery`): `{table (req), source?, where (req BoolExp; missing → parse-failed key "where" not found, path $), "$set"?: {col: val} (alias key `"values"`), "$inc"?: {col: num}, "$mul"?: {col: num}, "$default"?: [col], returning?: [col]}`. All of set/inc/mul/default (plus permission presets) empty → `400 unexpected-payload` `atleast one of $set, $inc, $mul has to be present`. Updating a preset column → `400 not-supported` `column "c" is not updatable for role "r"; its value is predefined in permission`.

**`delete`** (`DeleteQuery`): `{table (req), source?, where (req), returning?: [col]}`.

Mutation response (insert/update/delete; `mkDefaultMutFlds`): without `returning` → `{"affected_rows": n}`; with `returning` → `{"returning": [ {<only the listed cols>}, ... ], "affected_rows": n}` — `returning` key **first** (field list order in `mkDefaultMutFlds`; matches fixture `server/tests-py/queries/v1/update/permissions/user_update_resident_preset.yaml`). Insert/update permission `check` failure → `400 permission-error` `check constraint of an insert/update permission has failed`.

**`count`** (`CountQuery`): `{table (req), source?, distinct?: [col], where?}` → `{"count": n}` (exact bytes, hand-built: `{"count":` + integer + `}`). Relationship in `distinct` → `Relationships can't be used in "distinct".`. Non-admin needs select permission (`; "count" is only allowed if the role has "select" permissions on the table` appended to the permission-denied message).

Permission errors for non-admin (`permission-denied`, 400): `<select|insert|update|delete> on "table" for role "r" is not allowed. ` (+ optional suffix), `role "r" does not have permission to select column "c"`; admin with unknown column: `no such column exists: "c"`; unknown table: `400 not-exists` `table "t" does not exist`.
DB errors: `dmlTxErrorHandler` (12-errors.md §6.1). Fixtures: `server/tests-py/queries/v1/{select,insert,update,delete,count,bulk}/`, docs `docs/docs/api-reference/schema-metadata-api/` (legacy).

---

## 5. RESTified endpoints: `/api/rest/<path>` (any method)

* Route: `hookRouteAll ("api" <//> "rest" <//> wildcard)` — **all HTTP methods**, not gated by `HASURA_GRAPHQL_ENABLED_APIS` (inferred: no `onlyWhenApiEnabled` wrapper), errors in **QErr JSON** with real status codes (`id` modifier; `encodeQErr`), GET-style pipeline (auth with no body to the webhook).
* Method mapping: GET/POST/PUT/PATCH/DELETE → endpoint methods; others → `400 bad-request` `Method <M> not supported.`; non-standard → `Nonstandard method not allowed for REST endpoints`.
* Matching (`Hasura/Server/Rest.hs`, `EndpointTrie`): path split on `/`; endpoint URL templates may contain `:var` segments.
  * no match → `404 not-found` `Endpoint not found`
  * path matches but method not allowed → `405 method-not-allowed` `Allowed methods: GET, POST` (comma+space separated, as stored)
  * ambiguous at runtime → `500 unexpected` `Multiple endpoints match request`
  * stored query not exactly one operation → `500` `A stored query should contain exactly one definition`
* Variables, collected from: query-string params (and form params) as strings (`Left`), top-level keys of a JSON object body (`Right`, any JSON type), URL `:var` segments (strings). Combined list `queryParams ++ bodyParams ++ urlParams` (`App.hs`: `allParams = fmap Left <$> queryParams <|> fmap Right <$> bodyParams` — list `<|>` is `++`; `Rest.hs` `runCustomEndpoint`: `reqArgs ++ zip (parseVariableNames queryx) (map Left matches)`), converted with `HashMap.fromList` (`alignVars`), where later mappings take precedence → **precedence: URL path segment > JSON body key > query-string/form param** (verified from source). Keys that are not valid GraphQL names are silently dropped (`mapMaybe … G.mkName`). Body params are taken only if the body decodes as a JSON *object*; any other body (array, invalid JSON, empty) contributes nothing, without error.
  * Variable not declared in the query (from *any* source, including stray query-string params like `?foo=1`) → `400 bad-request` `Unexpected variable <name>` (name unquoted).
  * String-sourced value decoding by declared type: list types → `400` `List variables are not currently supported in URL or Query parameters. (Variable <name>, with value "<v>")`; `Boolean` with empty value (`?flag`) → `true`; nullable type with empty value → omitted (null); otherwise try JSON-decode the string: a JSON bool is used if type ∈ {`Boolean`, `bool`}, a JSON number if type ∈ {`Int`, `Float`, `Number`, `Double`, `float8`, `numeric`}; anything else is passed as a JSON **string** (so `uuid`, `date`, `bigint` etc. arrive as strings).
  * Declared but missing variables are left unset (defaults / nullable apply; non-nullable → `400 validation-failed` `expecting a value for non-nullable variable: "first_name"`).
* Execution: same as `/v1/graphql` single request with role from auth; allowlist applies as usual.
* **Success response is the GraphQL `data` object without the envelope**: e.g. `{"test_table":[{"first_name":"Foo"}]}` (`encodeHTTPResp`). Remote-schema GraphQL errors → status 200, body = the JSON *array* of errors (no envelope) (inferred from `encodeHTTPResp (Left e)`).
* GraphQL validation errors are returned as QErr JSON with **HTTP 400** (not 200), e.g. `{"path":"$.selectionSet.test_table","error":"field 'test_table' not found in type: 'query_root'","code":"validation-failed"}`.
* Subscriptions as endpoints: rejected at metadata time (see metadata spec); at runtime would hit the HTTP subscription error.
* Fixtures: `server/tests-py/queries/endpoints/*.yaml`, `server/tests-py/test_endpoints.py`.

### 5.1 `GET /api/swagger/json`
* Admin only (`onlyAdmin` → `400 access-denied` `You have to be an admin to access this endpoint`); **not gated** by enabled APIs. Returns an OpenAPI 3 document describing the REST endpoints (`Hasura/Server/OpenAPI.hs`). Fixtures: `server/tests-py/queries/openapi/`, `test_openapi.py`. Low priority.

---

## 6. Health and version

### 6.1 `GET /healthz` and `GET /hasura/healthz` (identical)
* Public, not gated, raw handler (no `x-request-id` header, no auth). `GET` only (a `HEAD /healthz` hits the Spock fallback, see §12; Cloud Run / k8s probes must use GET). Query param `strict` parsed as a boolean via Spock `param` (http-api-data `FromHttpApiData Bool`: `true`/`false`, case-insensitive (inferred)); absent or unparsable → false.
* Logic: run metadata-storage health check (`SELECT 1`-style query on the metadata DB, `checkDbConnection`):
  * failure → `500`, body `ERROR`.
  * success and schema cache has inconsistent objects:
    * `strict=true` → `500`, body `ERROR: inconsistent objects in schema`
    * otherwise → `200`, body `WARN: inconsistent objects in schema`
  * else → `200`, body `OK`.
* Content-Type: `text/plain; charset=utf-8` (Spock `text`). Body has no trailing newline.
* Note there is also a per-source health check system (`source_health`, EE) — not part of this route.

### 6.2 `GET /v1/version`
* Public, not gated, raw handler (any role, no auth evaluated). `Content-Type: application/json; charset=utf-8`. Body (exact bytes, no whitespace): `{"server_type":"ce","version":"v2.48.0"}` — **`server_type` first**: the handler builds an aeson `Value` (`encode $ object (["version" .= v] <> extraData)`, `Hasura/Server/App.hs`), and aeson 2.3 (`cabal.project.freeze`, default `ordered-keymap`) serialises `Value` objects with keys sorted, so the source-code order is lost. The docs sample (`docs/docs/api-reference/version.mdx`) shows the same order. `version` is the build version string (`v` + semver for releases, arbitrary text for dev builds, `...-ce` suffix variants); `server_type` is always `"ce"` in the OSS build (EE builds report `"ee"`/`"cloud"`). The Rust engine should report a V2-compatible version string (the CLI compares major versions; a value like `v2.x.y` avoids CLI warnings) and may add extra keys.
* No `x-request-id` header.

---

## 7. Config, pg_dump, developer endpoints

### 7.1 `GET /v1alpha1/config`
* Gated by `config` API; admin only (`onlyAdmin` → `400 access-denied` `You have to be an admin to access this endpoint`). Fixtures/tests: `server/tests-py/test_config_api.py`.
* Response (`ServerConfig`, `Hasura/Server/API/Config.hs`, keys in this order, nulls included):

```json
{
  "version": "v2.48.0",
  "is_function_permissions_inferred": true,
  "is_remote_schema_permissions_enabled": false,
  "is_admin_secret_set": true,
  "is_auth_hook_set": false,
  "is_jwt_set": true,
  "jwt": [ { "claims_namespace": "https://hasura.io/jwt/claims", "claims_format": "json", "claims_map": null } ],
  "is_allow_list_enabled": false,
  "live_queries": { "batch_size": 100, "refetch_delay": 1 },
  "streaming_queries": { "batch_size": 100, "refetch_delay": 1 },
  "console_assets_dir": null,
  "experimental_features": [],
  "is_prometheus_metrics_enabled": false,
  "default_naming_convention": "hasura-default",
  "is_apollo_federation_enabled": false
}
```
  * `is_admin_secret_set`: true for every auth mode except no-auth (i.e. true also for JWT/webhook modes, which require an admin secret).
  * `jwt`: list (one per configured JWT secret; `[]` if none). `claims_namespace`: the namespace string, or the JSON path string (e.g. `$.hasura.claims`) when `claims_namespace_path` is used; `claims_format`: `"json"` | `"stringified_json"`; `claims_map`: null, or object `{ "x-hasura-default-role": <spec>, "x-hasura-allowed-roles": <spec>, <other claim>: <spec> }`. With `claims_map`, namespace/format show defaults.
  * `refetch_delay` is in **seconds** (number, may be fractional: env value ms/1000).
  * `experimental_features`: array of keys from `HASURA_GRAPHQL_EXPERIMENTAL_FEATURES` (`inherited_roles`, `optimize_permission_filters`, `naming_convention`, `streaming_subscriptions`, `apollo_federation`, `hide_update_many_fields`, `bigquery_string_numeric_input`, `hide_aggregation_predicates`, `hide_stream_fields`, `group_by_aggregations`, `disable_postgres_arrays`, `no_null_unbound_variable_default`, `remove_empty_subscription_responses`); order not guaranteed (HashSet).
  * `is_prometheus_metrics_enabled`: `metrics` ∈ enabled APIs.
  * `default_naming_convention`: `"hasura-default"` | `"graphql-default"`.

### 7.2 `POST /v1alpha1/pg_dump`
* Gated by `pgdump` API; admin only (`onlyAdmin`).
* Request: `{"opts": [string] (required), "clean_output": bool (default false), "source": string (default "default")}`.
* Source must be a vanilla Postgres source in the schema cache else `400 not-found` `source "<name>" not found`.
* Executes `pg_dump <connstring> --encoding=utf8 --restrict-key=hasura <opts...>` (`Hasura/Server/API/PGDump.hs`). Exit ≠ 0 → `500 unexpected` `error while executing pg_dump` with `internal` = stderr text (string). Exec failure → `500` `internal exception while executing pg_dump`.
* `clean_output: true` removes lines that are: blank/whitespace-only; start with `--`; exactly one of: `SET statement_timeout = 0;`, `SET lock_timeout = 0;`, `SET idle_in_transaction_session_timeout = 0;`, `SET client_encoding = 'UTF8';`, `SET standard_conforming_strings = on;`, `SELECT pg_catalog.set_config('search_path', '', false);`, `SET xmloption = content;`, `SET client_min_messages = warning;`, `SET row_security = off;`, `SET default_tablespace = '';`, `SET default_with_oids = false;`, `SET default_table_access_method = heap;`, `CREATE SCHEMA public;`, `COMMENT ON SCHEMA public IS 'standard public schema';`; or match regex `^CREATE TRIGGER "?notify_hasura_.+"? AFTER [[:alnum:]]+ ON .+ FOR EACH ROW EXECUTE (FUNCTION|PROCEDURE) "?hdb_catalog"?\."?notify_hasura_.+"?\(\);$`. Lines rejoined with `\n` (each line terminated by `\n`).
* Success: `200`, `Content-Type: application/sql; charset=utf-8`, raw SQL body (gzip negotiation applies).
* Fixture: `server/tests-py/pgdump/`, `test_pg_dump.py`. Cloud-agnostic note: the Rust image must bundle a `pg_dump` matching the server major version; on platforms without process spawning (Cloudflare Workers) respond `400 not-supported` (deviation).

### 7.3 Developer endpoints (gated by `developer` API, admin only via `onlyAdmin`, GET)
| Route | Response |
|---|---|
| `/dev/ekg` | EKG metrics sample JSON (implementation-specific; Rust: any JSON metrics object) |
| `/dev/plan_cache` | `null` (deprecated) |
| `/dev/subscriptions` | subscription pollers dump (see subscriptions spec) |
| `/dev/subscriptions/extended` | extended dump |
| `/dev/dataconnector/schema` | OpenAPI schema of the Data Connector agent API |
| `/dev/rts_stats` | only when GHC RTS stats enabled; no auth, not gated. Skip in Rust. |
Non-admin → `400 access-denied`. Test: `server/tests-py/test_dev_endpoints.py`.

---

## 8. Console

* Enabled when `HASURA_GRAPHQL_ENABLE_CONSOLE=true` (default false) **and** `metadata` API enabled; otherwise these routes return the 404 QErr (`resource does not exist`).
* `GET /` → `302` redirect, `Location: console` (relative).
* `GET /console` and `GET /console/<anything>` → `200 text/html; charset=utf-8`, rendered from mustache template `server/src-rsr/console.html` with variables: `isAdminSecretSet` (`"true"`/`"false"` — true unless no-auth), `isAdminSecretDisabled` (`HASURA_GRAPHQL_DISABLE_ADMIN_SECRET`), `consolePath` (`/console` or `/console/<path>`), `enableTelemetry`, `cdnAssets` (`true` iff `HASURA_GRAPHQL_CONSOLE_ASSETS_DIR` unset), `consoleSentryDsn` (always `""`), `assetsVersion` (see below), `serverVersion`, `consoleType` (`"oss"`). The page sets `window.__env` and loads `https://graphql-engine-cdn.hasura.io/console/assets/<assetsVersion>/assetLoader.js.gz` (CDN) or `<prefix>/console/assets/versioned/assetLoader.js.gz` (local).
  * `assetsVersion` (`Hasura/Server/Version.hs` `versionToAssetsVersion`): dev version text `t` → `versioned/<t>`; release `vX.Y.Z` → `channel/stable/vX.Y`; prerelease `vX.Y.Z-beta.1` → `channel/beta/vX.Y`; `...-ce` → `channel/versioned/<text>`.
  * The Rust impl should serve the same template, pointing at the official CDN with the assetsVersion of the V2 release it emulates (e.g. `channel/stable/v2.48`), so the stock console works.
* `GET /console/assets/<path>` (only when `HASURA_GRAPHQL_CONSOLE_ASSETS_DIR` set): serve `<dir>/<path>`; reject absolute paths or paths containing `..`; if filename ends with `.gz` add `Content-Encoding: gzip` and use the MIME type of the name without `.gz`; missing → `404` QErr `{"error":"Couldn't find console asset <path>","path":"$","code":"not-found"}`.
* Console also needs: `/v1alpha1/config`, `/v1/metadata`, `/v2/query` (`concurrent_bulk`, `run_sql`), `/v1/graphql` (introspection), `/v1/version`, `/v1/graphql/explain`, `/api/swagger/json`, and websockets.

---

## 9. CORS

Implementation: `Hasura/Server/Middleware.hs` `corsMiddleware`, config `Hasura/Server/Cors.hs`, init `Hasura/Server/Init.hs` `mkCorsConfig`.

* Config:
  * `HASURA_GRAPHQL_DISABLE_CORS=true` (or `--disable-cors`) → CORS disabled: **no CORS headers are ever sent**, preflights fall through to routing. In this mode `HASURA_GRAPHQL_WS_READ_COOKIE=true` is allowed (else startup error `HASURA_GRAPHQL_WS_READ_COOKIE can only be used when CORS is disabled`).
  * `HASURA_GRAPHQL_CORS_DOMAIN`: default `*` (allow all). Otherwise comma-separated list, each entry trimmed, each must be `scheme://host[:port]` or `scheme://*.host[:port]` (scheme chars: alnum, `+`, `.`, `-`). Invalid → startup error `invalid domain: '<d>'. All domains should have scheme + (optional wildcard) host + (optional port)`.
* Behaviour (only when request has an `Origin` header; no Origin → no CORS processing):
  * Allow-all: origin allowed (echoed back, never `*`).
  * List: allowed iff origin string exactly equals an FQDN entry (`scheme://host[:port]` textual compare) or matches a wildcard: origin parsed as `scheme://<first-label>.<rest>[:port]` and `(scheme, rest, port)` equals a wildcard entry → `*.foo.bar.com` matches `https://app.foo.bar.com` but **not** `https://foo.bar.com` nor `https://a.app.foo.bar.com`. Ports must match exactly (absent ≠ present).
  * Not allowed → request passes through without CORS headers (no rejection).
* For allowed origins:
  * Method `OPTIONS` → **preflight short-circuit, any path**: status `204`, empty body, headers: `Access-Control-Allow-Origin: <origin>`, `Access-Control-Allow-Credentials: true`, `Access-Control-Allow-Methods: GET,POST,PUT,PATCH,DELETE,OPTIONS`, `Access-Control-Expose-Headers: X-Hasura-Query-Cache-Key,X-Hasura-Query-Family-Cache-Key,Warning`, `Access-Control-Max-Age: 1728000`, `Access-Control-Allow-Headers: <value of request Access-Control-Request-Headers, or empty>`, `Content-Length: 0`, `Content-Type: text/plain charset=UTF-8` (sic, no semicolon).
  * Other methods → route normally, then prepend the four headers `Access-Control-Allow-Origin`, `Access-Control-Allow-Credentials`, `Access-Control-Allow-Methods`, `Access-Control-Expose-Headers` to the response (also on error responses).
* Test: `server/tests-py/test_cors.py`.

---

## 10. Response compression

`Hasura/Server/Compression.hs`. Applies only to successful responses produced via `mkSpockAction` (GraphQL, metadata, queries, REST, config, pg_dump, ...). Not to errors, healthz, version, console.

* Parse all `Accept-Encoding` header values, split on `,`, trim. q-values are ignored except the exact tokens below.
  * `identityRejected` = token `identity;q=0` present, or (`*;q=0` present and no token starts with `identity`).
  * `gzipAccepted` = some token starts with `gzip` and token `gzip;q=0` is not present.
  * No header, or header exactly `*` → treat as identity only (historical behaviour).
* Decision:
  * gzip accepted and identity not rejected → gzip **only if body ≥ 700 bytes**.
  * gzip accepted and identity rejected → always gzip.
  * otherwise → no compression.
* gzip level: 6 if body > 20000 bytes, else 1 (libdeflate). Add `Content-Encoding: gzip`; `Content-Length` is the compressed length. No `Vary` header is added.
* Fixtures: `server/tests-py/queries/compression/`, `test_compression.py`.

---

## 11. API enablement: `HASURA_GRAPHQL_ENABLED_APIS`

* Comma-separated, case-insensitive, trimmed values from `metadata`, `graphql`, `pgdump`, `developer`, `config`, `metrics`. Default `metadata,graphql,pgdump,config`. Invalid value → startup error `Only expecting list of comma separated API types metadata,graphql,pgdump,developer,config,metrics`.
* Gating (checked per request, before auth, via `onlyWhenApiEnabled`; disabled → `404` QErr JSON `{"error":"resource does not exist","path":"$","code":"not-found"}`, Content-Type JSON, no request id):

| API | Routes |
|---|---|
| `graphql` | `POST /v1/graphql`, `GET /v1/graphql`, `POST /v1alpha1/graphql`, `POST /v1/relay`, `POST /v1beta1/relay` (and WS — see subscriptions spec) |
| `metadata` | `POST /v1/metadata`, `POST /v1/query`, `POST /v2/query`, `POST /v1/graphql/explain`, `POST /v1alpha1/graphql/explain`, console routes (`/`, `/console*`) |
| `pgdump` | `POST /v1alpha1/pg_dump` |
| `config` | `GET /v1alpha1/config` |
| `developer` | `/dev/ekg`, `/dev/plan_cache`, `/dev/subscriptions`, `/dev/subscriptions/extended`, `/dev/dataconnector/schema` |
| `metrics` | EE `/v1/metrics` (CE: only flips `is_prometheus_metrics_enabled` in config) |
| (none) | `/healthz`, `/hasura/healthz`, `/v1/version`, `/api/rest/*`, `/api/swagger/json`, `/console/assets/*` |

* Test: `server/tests-py/test_apis_disabled.py`.

---

## 12. Fallbacks

* Any other `GET` or `POST` path → `404` QErr JSON `{"error":"resource does not exist","path":"$","code":"not-found"}`, `Content-Type: application/json; charset=utf-8` (no `x-request-id` header — `raiseGenericApiError` only logs it).
* Note the catch-alls exist only for `GET` and `POST` (`forM_ [Spock.GET, Spock.POST] … Spock.hookAny`), so a *wrong method on a known path* that is GET or POST still gets 404: e.g. `GET /v1/metadata` → 404, `POST /v1/version` → 404, `POST /healthz` → 404.
* Any other method (PUT/PATCH/DELETE/HEAD/OPTIONS-without-CORS, …) on a non-REST path, e.g. `PUT /v1/graphql`, `HEAD /healthz`: no route matches, Spock calls `sc_errorHandler`, which Hasura overrides with `spockInternalErrorHandler` (`App.hs` `mkSpockConfig`) — it **ignores the status Spock passes and always answers `500`** with body `{"error":"Internal Server Error","path":"$","code":"unexpected"}` (`encodeQErr HideInternalErrors`). No `x-request-id`; no explicit `Content-Type` (Spock `lazyBytes`; inferred none is added). Resolution: unmatched methods → **500** (handler source-verified; that Spock routes "no matching route" through `sc_errorHandler` is inferred from Spock-core 0.14 behaviour, no upstream test). Recommended Rust behaviour: replicate 500 + that body (adding `Content-Type: application/json; charset=utf-8` is harmless). With CORS enabled and an `Origin` header, `OPTIONS` never gets here (204 preflight, §9).
* Spock-internal exceptions → same 500 body.

---

## 13. Request/response headers summary

Request headers with special meaning at the HTTP layer:
* `x-request-id` — reused as request id.
* `Accept-Encoding` — compression.
* `Origin`, `Access-Control-Request-Headers` — CORS.
* `x-hasura-admin-secret` (the legacy `x-hasura-access-key` *header* is not recognised by this source; only the `--access-key` CLI flag alias survives), `Authorization`, `Cookie`, `x-hasura-role`, `x-hasura-*` session variables, `x-hasura-use-backend-only-permissions` — authentication (auth spec).
* All client headers are available to forward to actions/remote schemas/webhooks (except the ignored list `Content-Length, Content-MD5, User-Agent, Host, Origin, Referer, Accept, Accept-Encoding, Accept-Language, Accept-Datetime, Cache-Control, Connection, DNT, Content-Type` — `Hasura/Authentication/Headers.hs` `commonClientHeadersIgnored`), see actions/remote spec.

Response headers produced by CE:
* `x-request-id` (all `mkSpockAction` responses, success and error).
* `Content-Type`: `application/json; charset=utf-8` (JSON APIs), `application/sql; charset=utf-8` (pg_dump), `text/plain; charset=utf-8` (healthz), `text/html; charset=utf-8` (console).
* `Content-Length` (always set explicitly; no chunked encoding).
* `Content-Encoding: gzip` when compressed.
* `Set-Cookie` forwarded from auth webhook responses, sync action handler responses, and remote schema responses (single requests only).
* `Date`.
* CORS headers.
* **No `X-Hasura-*` response headers in CE.** (EE query caching adds `X-Hasura-Query-Cache-Key`, `X-Hasura-Query-Family-Cache-Key`, `Cache-Control: max-age=N`, `Warning: 199 - cache-store-...`.)

---

## 14. Implementation notes for the Rust engine

* Use one router (e.g. axum) with: a WS-upgrade check on the 4 GraphQL paths; a CORS layer reproducing §9 exactly (do not use tower-http's CorsLayer defaults — echo origin, `204` preflight on any path, exact header strings); a request-id layer; a compression decision function reproducing §10 (do not blindly use tower-http compression, its negotiation differs — thresholds 700/20000 matter for byte-exact tests but not for clients).
* Model handlers as `enum ApiKind { GraphQl{legacy_status: bool}, Relay, Rql(QErrFormat), Raw }` with a single `respond_error(err, format, include_internal, status_rewrite)`.
* Keep the control-char → space body preprocessing before JSON parsing (clients rely on it).
* The `/v1/graphql` 200-for-errors rule (and `PRESERVE_401_ERRORS`) must be applied *after* auth errors too.
* pg_dump should be optional at build time (feature flag) for edge targets.

---

## Upstream tests to reuse

* `server/tests-py/test_apis_disabled.py` — 404 for disabled API groups (`HASURA_GRAPHQL_ENABLED_APIS`).
* `server/tests-py/test_cors.py` — wildcard / port / sub-sub-domain CORS matching and header values.
* `server/tests-py/test_compression.py` + `queries/compression/` — gzip on `/v1/graphql` and `/v1/query`.
* `server/tests-py/test_config_api.py` — `/v1alpha1/config` shape per auth mode (JWT tests need key fixtures).
* `server/tests-py/test_version.py` — `/v1/version` shape (skip the git-version comparison).
* `server/tests-py/test_dev_endpoints.py` — developer API admin gating.
* `server/tests-py/test_pg_dump.py` + `server/tests-py/pgdump/pg_dump_public.yaml` — pg_dump output with `clean_output` (byte-exact; depends on pg_dump version).
* `server/tests-py/test_v1alpha1_endpoint.py` — legacy endpoint status codes (400) vs `/v1/graphql`.
* `server/tests-py/test_endpoints.py` + `queries/endpoints/*.yaml` — RESTified endpoints: methods (405), not found (404), arg sources, validation errors as 400 QErr.
* `server/tests-py/test_openapi.py` + `queries/openapi/` — `/api/swagger/json`.
* `server/tests-py/queries/explain/*.yaml` (Postgres files only; skip `*_mssql.yaml`, `*_bigquery.yaml`) — explain query/subscription output (plan text varies by PG version; compare `field`/`sql` and plan non-emptiness).
* `server/tests-py/queries/v2/basic/*` — `/v2/query` admin-only + DML; skip `queries/v2/mssql`.
* `server/tests-py/queries/v1/**` (`basic`, `bulk`, `select`, `insert`, `update`, `delete`, `count`, `run_sql`, `metadata`) — legacy `/v1/query`; note `bulk/mixed_access_mode.yaml` is disabled upstream (stale).
* `server/tests-py/queries/unauthorized_role/*` — 200 GraphQL error for auth failures on `/v1/graphql`.
* `server/lib/api-tests/src/Test/API/GraphQL/ContentLengthSpec.hs` — `Content-Length` present on success and error.
* `server/lib/api-tests/src/Test/HealthCheckSpec.hs` — `/console` returns 200/204 (console enabled in harness).
* `server/lib/api-tests/src/Test/API/ExplainSpec.hs` — explain API.
* `server/lib/api-tests/src/Test/API/ConcurrentBulkSpec.hs` — `/v2/query` `concurrent_bulk`.
* `server/lib/api-tests/src/Test/API/Schema/RunSQLSpec.hs` — run_sql behaviours (Postgres parts).
* `server/lib/api-tests/src/Test/API/Metadata/BulkAtomicSpec.hs`, `BulkKeepGoingSpec.hs` — metadata bulk envelopes.
* Skip/EE: `test_query_cache.py` / `queries/query_cache` (EE caching headers), anything under `api-tests/.../DataConnector`, BigQuery/MSSQL/Citus variants.
