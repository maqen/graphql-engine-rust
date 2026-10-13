# 12 — Error responses: formats, status codes, codes, messages

Spec for error behaviour of a drop-in Rust replacement of Hasura GraphQL Engine V2 (CE/OSS).
Ground truth: `server/lib/hasura-base/src/Hasura/Base/Error.hs` (`Code`, `QErr`, encoders),
`server/src-lib/Hasura/GraphQL/Transport/HTTP/Protocol.hs` (`encodeGQErr`, `toParsed`, `getSingleOperation`),
`server/src-lib/Hasura/Server/App.hs` (`mkSpockAction`, `maybeMod200`, `onlyAdmin`, 404s),
`server/src-lib/Hasura/Server/Init/Config.hs` + `Hasura/App/State.hs` (internal-error policy),
`server/lib/schema-parsers/src/Hasura/GraphQL/Parser/**` (GraphQL validation messages),
`server/src-lib/Hasura/GraphQL/Execute/{Resolve,Inline}.hs`, `server/src-lib/Hasura/Backends/Postgres/Execute/Types.hs` (Postgres error mapping),
`server/src-lib/Hasura/Server/Auth*.hs` (auth messages). Route-level behaviour is in `10-http-apis.md`.

> **Testing caveat.** The Python suite (`server/tests-py/validate.py` `assert_graphql_resp_expected`) only *warns* when an expected error body differs ("Response does not have the expected error message"); status codes are still asserted. So error **messages** in `tests-py/queries/**` fixtures may be stale. The Haskell api-tests (`shouldReturnYaml`) compare exactly. When a fixture and the source disagree, this doc follows the source and flags it.

---

## Scope and priority

* **Essential**: the two envelopes (§1, §2), `internal` policy (§4), HTTP status rules per endpoint (§3), the `code` strings (§5), and the high-frequency messages clients match on: invalid JSON, `not a valid graphql query`, `field 'x' not found in type: 'y'`, `missing session variable: "x"`, variable errors, constraint violations, permission check failure, auth errors, `restricted access : admin only`, `resource does not exist`.
* **High**: Postgres error mapping (§6) incl. `internal` shape for admins; action-handler error passthrough (§7.6).
* **Medium**: metadata-API error messages (most are per-command; see metadata spec), aeson-derived `parse-failed` wording (cannot be byte-exact in Rust; keep shape and path).
* **EE-only / skip**: API limits errors (`rate limit`, depth/node limits), connection-template errors (`template-resolution-failed`), data-connector/BigQuery/MSSQL codes (keep the strings in the enum for completeness).

---

## 1. Non-GraphQL error envelope ("QErr JSON")

Used by `/v1/metadata`, `/v1/query`, `/v2/query`, `/v1/graphql/explain`, `/v1alpha1/config`, `/v1alpha1/pg_dump`, `/api/rest/*`, `/api/swagger/json`, `/dev/*`, 404/disabled routes, console asset errors, and the legacy websocket protocol on `/v1alpha1/graphql`.

```json
{"error": "<message>", "path": "$.args.table", "code": "<code>", "internal": <any JSON>}
```

* Key order is fixed: `error`, `path`, `code`, then `internal` (last, only if present and allowed) (`instance ToJSON QErr`, `toEncoding`). Caveat: wherever a QErr is first converted to an aeson `Value` (`toJSON`) and embedded in a larger value — e.g. the inline error objects of `bulk_keep_going`, or the websocket legacy payload if built via `toJSON` — aeson 2.3 sorts object keys, giving `code`, `error`, `internal`, `path` order (inferred per call site; JSON-equal either way).
* Route-level responses produced outside `mkSpockAction` (404 catch-all via `raiseGenericApiError` = `encode qErr`, API-disabled 404, relay-disabled 400, Spock 500) use the same `toEncoding` order and never include `internal`.
* `path`: JSON path string (§1.1), `"$"` when no context.
* `internal`: present only when the error carries `ExtraInternal` data **and** internal errors are allowed for the request (§4). `ExtraExtensions` data (from action webhooks) is **never** emitted in this envelope.
* Example: `{"error":"restricted access : admin only","path":"$.args","code":"access-denied"}`.

### 1.1 JSON path encoding (`Data/Parser/JSONPath.hs` `encodeJSONPath`)
* Starts with `$`. Array index → `[i]`. Object key → `.key` if the key is non-empty, first char ∈ `[A-Za-z_]`, rest ∈ `[A-Za-z0-9_-]`; otherwise `["<json-escaped key>"]` (e.g. `$["x.y"]`, `$["1abc"]`).
* Examples: `$.selectionSet.insert_author.args.objects`, `$.args[1].args.sql`, `$.selectionSet.insert_test_types.args.objects[0].c2_integer`.

---

## 2. GraphQL error envelope ("GQL error JSON")

Used by `/v1/graphql`, `/v1alpha1/graphql`, `/v1/relay`, `/v1beta1/relay` (HTTP), and graphql-ws/`graphql-transport-ws` error payloads on `/v1/graphql` (see subscriptions spec).

```json
{"errors": [ {"message": "<message>", "extensions": {"path": "$.selectionSet.author", "code": "validation-failed", "internal": <any>}} ]}
```
(`encodeGQErr` / `encodeGQLErr` in `Hasura/Base/Error.hs` and `.../HTTP/Protocol.hs`)
* Exactly **one** error object per failed request (Hasura stops at the first error; no `locations`, no top-level `path` array).
* Key order: `message`, `extensions`; inside extensions: `path`, `code`, then `internal` (only if allowed, §4).
* No `data` key when the error comes from a QErr. (`{"data": null, "errors": [...]}` only appears when a remote schema returned GraphQL errors — those error objects are passed through verbatim — see remote-schemas spec.)
* **`ExtraExtensions` override**: if the error carries extensions from an action webhook (§7.6), `extensions` is exactly that JSON value, and `path`/`code`/`internal` are omitted, e.g. webhook 4xx body `{"message":"bad","extensions":{"code":"FOO","x":1}}` → `{"errors":[{"message":"bad","extensions":{"code":"FOO","x":1}}]}`.
* Batched requests: each failed element is a full `{"errors":[...]}` object inside the response array.

---

## 3. HTTP status code rules

Every QErr carries a status (400, 401, 404, 405, 409, 429, 500; constructors `err400`…`err500`). How it surfaces:

| Endpoint | Status on error |
|---|---|
| `POST/GET /v1/graphql`, `POST /v1/relay`, `/v1beta1/relay` | **200** for every QErr (validation, parse, auth, DB, 500 internal…). With `HASURA_GRAPHQL_PRESERVE_401_ERRORS=true`: errors whose status is 401 stay **401** (admin-secret invalid/missing, auth webhook 401). JWT errors are internally 400 → still 200. Remote-schema GraphQL errors → 200 with `data: null`. |
| `POST /v1alpha1/graphql` | The QErr status (400/401/404/500…). Test `server/tests-py/test_v1alpha1_endpoint.py`: validation error → 400, constraint violation → 400. |
| `/v1/metadata`, `/v1/query`, `/v2/query`, explain, config, pg_dump, dev, REST, swagger | The QErr status. |
| Disabled API (`HASURA_GRAPHQL_ENABLED_APIS`), unknown GET/POST route | 404 QErr `resource does not exist` (even for `/v1/graphql` — GraphQL envelope is NOT used here). |
| Relay disabled | 400 QErr (`not-supported`), not rewritten to 200. |
| Unmatched method on non-REST path (e.g. `PUT /v1/graphql`, `HEAD /healthz`) — i.e. any method other than GET/POST, which have 404 catch-alls | 500 `{"error":"Internal Server Error","path":"$","code":"unexpected"}` (`spockInternalErrorHandler` always sets 500, ignoring Spock's status; that Spock routes no-match through it is inferred). `GET /v1/metadata` or `POST /v1/version` → 404 `resource does not exist`. |
| `/v2/query` vs `/v1/query` | Same statuses; paths differ: v1 execution errors under `$.args…`, v2 execution errors at `$` / `$[i]`; v2 arg-decoding errors under `$.args…`, v1 arg-decoding errors relative to args. |

Status conventions used by the source:
* **400**: almost everything (`throw400`) — validation, parse, permission, constraint violations, metadata errors, **access-denied for non-admin on admin APIs**, JWT errors.
* **401**: authentication failures via `throw401` (code always `access-denied`): `invalid "x-hasura-admin-secret"`, `"x-hasura-admin-secret" required, but not found`, `Authentication hook unauthorized this request`, `unauthenticated` (unauthenticated mode), `EE License Key Required.` (DC agents).
* **404**: `not-found` route/endpoint/console-asset errors (`throw404`).
* **405**: REST endpoint method mismatch (`method-not-allowed`).
* **409**: metadata `resource_version` mismatch (`conflict`).
* **429**: rate limits (EE only).
* **500**: `throw500` (`unexpected`), Postgres connection errors (`postgres-error`), unexpected DB errors (`database query error`), maintenance mode, auth webhook transport failures, some action errors, pg_dump failures.

Note: `/v1/graphql` returns 200 even for 500-class failures; clients detect errors via the `errors` key only.

---

## 4. When is `internal` included?

`ResponseInternalErrorsConfig` (`Hasura/App/State.hs` `buildResponseInternalErrorsConfig`, `Hasura/Server/Init/Config.hs` `shouldIncludeInternal`):

| Setting | Policy |
|---|---|
| `HASURA_GRAPHQL_DEV_MODE=true` | include `internal` for **all** roles |
| else `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS=true` (**default true**) | include only when the resolved role is `admin` |
| else | never |

Details:
* The role is the role resolved by authentication for this request. Errors raised *during authentication* (and JSON-body parse failures on GraphQL routes) are always encoded with internal **hidden** (`mkSpockAction` passes `HideInternalErrors` there).
* The 404/disabled/relay-disabled/Spock 500 responses always hide internal.
* For batched GraphQL requests each element uses the same role-based policy.
* Some producers also gate *adding* internal data on the same policy (e.g. action errors use `includeInternalErrors` to decide between `internal error` with/without detail, see §7.6).
* `internal` content is free-form JSON. Most common shapes: Postgres tx error object (§6.2); list of inconsistent-metadata objects; `{"message":"query is not in any of the allowlists"}`; action request/response info (§7.6); for unhandled exceptions `{"error":"<text>"}`.

---

## 5. Code catalogue (`Hasura/Base/Error.hs` `Code`)

All values (JSON string ← constructor), typical HTTP status, and representative uses. A Rust `enum ErrorCode` must serialise to exactly these strings; plus two open variants: `ActionWebhookCode(String)` and `CustomCode(String)` which serialise to the carried string.

| code | Constructor | Typical status | Where / examples |
|---|---|---|---|
| `access-denied` | AccessDenied | 400 / 401 | non-admin on admin API: `restricted access : admin only` (path `$.args`) or `You have to be an admin to access this endpoint` (path `$`); auth: `invalid "x-hasura-admin-secret"` (401), `"x-hasura-admin-secret" required, but not found` (401), `Authentication hook unauthorized this request` (401), `unauthenticated` (401); JWT: `Your requested role is not in allowed roles` (400) |
| *(action code)* | ActionWebhookCode t | webhook's 4xx status | action handler returned 4xx with `code` field |
| `already-exists` | AlreadyExists | 400 | metadata: object/relationship/trigger already exists |
| `already-tracked` | AlreadyTracked | 400 | `view/table already tracked: "author"`, functions, logical models (`Logical model 'x' is already tracked.`) |
| `already-untracked` | AlreadyUntracked | 400 | `view/table already untracked: "x"` |
| `bad-request` | BadRequest | 400 | REST variable errors (`Unexpected variable x`), unsupported REST method, invalid `x-hasura-use-backend-only-permissions` value (`"x-hasura-use-backend-only-permissions": <reason>`), Postgres invalid escape sequence |
| `bigquery-error` | BigQueryError | 400/500 | BigQuery (skip) |
| `busy` | Busy | 400 | `event is already being processed` (redeliver) |
| `concurrent-update` | ConcurrentUpdate | 400 | Postgres serialization failure: `serialization failure due to concurrent update` (other transaction-rollback errors: PG message) |
| `connection-not-established` | ConnectionNotEstablished | 500 | data connector agent unreachable |
| `coercion-error` | CoercionError | 400 | remote schema session-variable preset coercion: `"<val>" cannot be coerced into an Int value` / `a Boolean value` / `a Float value` |
| `conflict` | Conflict | 409 | `metadata resource version referenced (N) did not match current version` |
| `constraint-error` | ConstraintError | 400 | `there is no unique or exclusion constraint on target column(s)` (PG 42P10); other PG syntax/access-rule errors that are "expected" (undefined_object); metadata: `no foreign constraint exists on the given column(s)` |
| `constraint-violation` | ConstraintViolation | 400 | PG integrity violations (§6.1) |
| *(custom)* | CustomCode t | any | free-form codes |
| `cyclic-dependency` | CyclicDependency | 400 | `found cycle(s) in roles: ...` (inherited roles) |
| `data-exception` | DataException | 400 | PG class 22 errors (invalid input syntax, numeric out of range, …): message = PG message |
| `data-connector-error` | DataConnectorError | 400/500 | data connectors (skip) |
| `dependency-error` | DependencyError | 400 | dropping objects with dependents (`cannot drop due to the following dependent objects: ...`), query collection in use |
| `invalid-configuration` | InvalidConfiguration | 400 | metadata object became inconsistent when added (message = inconsistency reasons, internal = inconsistent objects), REST endpoint config errors; healthz strict log only |
| `invalid-headers` | InvalidHeaders | 400 | `Missing 'Authorization' or 'Cookie' header in JWT authentication mode`, `Malformed Authorization header`, `Could not verify JWT: Multiple JWTs found` |
| `invalid-json` | InvalidJSON | 400 | request body is not valid JSON (message = decoder error) |
| `invalid-params` | InvalidParams | 400 | `"x-hasura-role" not found in session variables` (webhook response without role), explain API errors, misc metadata arg errors |
| `invalid-jwt` | JWTInvalid | 400 | `Could not verify JWT: JWTExpired`, `Could not verify JWT: JWSError JWSInvalidSignature`, `Could not verify JWT: JWTNotInAudience`, `Could not verify JWT: JWTNotInIssuer`, `Could not verify JWT: JWTNotYetValid` (text after the colon is the Haskell `show` of the jose error — reproduce these strings for the common cases) |
| `jwt-invalid-claims` | JWTInvalidClaims | 400 | `claims key: '<ns>' not found`, `claims not found at claims_namespace_path: '<path>'`, `expecting a json object when claims_format is json`, `expecting a string when claims_format is stringified_json`, `invalid "x-hasura-allowed-roles"; should be a list of roles: <aeson err>`, `JWT claim from claims_map, "<k>" not found` |
| `jwt-missing-role-claims` | JWTRoleClaimMissing | 400 | `JWT claim does not contain "x-hasura-allowed-roles"` / `"x-hasura-default-role"` (source wraps the claim name in double quotes via `<>>`; `server/tests-py/test_jwt.py` expects it unquoted — fixture likely stale, see caveat) |
| `mssql-error` | MSSQLError | 400 | skip |
| `method-not-allowed` | MethodNotAllowed | 405 | REST: `Allowed methods: GET` |
| `not-exists` | NotExists | 400 | metadata: `table "x" does not exist`, `function ... not found in the cache`, `remote schema with name "x" does not exist`, etc. |
| `not-found` | NotFound | 404 (routes) / 400 (others) | `resource does not exist` (404), `Endpoint not found` (404), `Couldn't find console asset <p>` (404); **`missing session variable: "x-hasura-user-id"` (400)**; `source "x" not found` (pg_dump, 400); `no subscriptions found` (explain) |
| `not-supported` | NotSupported | 400 | `PersistedQueryNotSupported`, `Mutations are not allowed when read-only mode is enabled`, `Cannot run write queries when read-only mode is enabled`, `metadata cannot be modified in read-only mode`, relay disabled, subscription restrictions (`subscription to remote server is not supported`, `Remote relationships are not allowed in subscriptions`, `query actions cannot be run as a subscription`, `all fields of a subscription must be from the same source`, `exactly one root field is allowed for streaming subscriptions`, `streaming and livequery subscriptions cannot be executed in the same subscription`, `Introspection at root field level is not supported over subscriptions`), `Event log cleanup feature is enterprise edition only`, EE-only metadata features |
| `parse-failed` | ParseFailed | 400 | JSON body/args decoding errors (§8), GraphQL scalar out of range, custom scalar parse errors, `multiple definitions for fragment "f"` |
| `permission-denied` | PermissionDenied | 400 | legacy RQL DML permission errors (`Hasura/RQL/DML/Internal.hs`): `select on "article" for role "contractor" is not allowed. ` (trailing space; DML may append e.g. `; "count" is only allowed if the role has "select" permissions on the table`), `role "user" does not have permission to select column "id"`, admin: `no such column exists: "id"`; metadata `drop_*_permission` on a missing permission (`Hasura/RQL/DDL/Permission.hs` `runDropPerm`): `select permission on "article" for role "user" does not exist` (perm type unquoted) |
| `permission-error` | PermissionError | 400 | `check constraint of an insert/update permission has failed` (GraphQL mutations, path `$.selectionSet.<field>.args.objects` for inserts), PG `Check constraint violation. <pg msg>` (note: PG CHECK violations map to permission-error, not constraint-violation), dynamic_from_file restrictions |
| `postgres-error` | PostgresError | 400 / 500 | run_sql failures: `query execution failed` (400, internal = PG tx error); connection failures: `connection error` (500), `root certificate error`, `certificate error`, `private-key error` (500) |
| `postgres-max-connections-error` | PostgresMaxConnectionsError | 500 | `max connections reached on postgres` (PG "too many clients") |
| `remote-schema-conflicts` | RemoteSchemaConflicts | 400 | tracking a table whose GraphQL names conflict with remote schema fields |
| `remote-schema-error` | RemoteSchemaError | 400 | invalid remote response: `expecting key <field>`, remote response not an object, remote schema HTTP errors (see remote spec) |
| `start-failed` | StartFailed | — | websocket: `start received before the connection is initialised` |
| `unexpected` | Unexpected | 500 / 400 | action response validation (400, §7.6), `Internal Server Error`, `database query error` (500, internal = PG tx error), `cannot continue due to newly found inconsistent metadata` (400 or 500), `cannot continue due to inconsistent metadata` (400), `cannot continue due to new inconsistent metadata` (400), `metadata cannot be modified in maintenance mode` (500), `webhook authentication request failed` (500), `Invalid response from authorization hook` (500), action `internal error` / `not a valid json response from webhook` (500), `Request Transformation Failed` / `Response Transformation Failed` (500), `Only read-only queries are allowed in a concurrent_bulk` (500) |
| `unexpected-payload` | UnexpectedPayload | 400 | `subscriptions are not supported over HTTP, use websockets instead`; many metadata arg checks |
| `validation-failed` | ValidationFailed | 400 | all GraphQL validation errors (§7), `query is not allowed` (allowlist) |
| `template-resolution-failed` | TemplateResolutionFailed | 400 | EE connection templates |
| `timeout` | TimeoutErrorCode | 500 | event/scheduled trigger delivery timeouts (stored in invocation logs, not HTTP) |

---

## 6. Postgres error mapping

### 6.1 Expected (user-facing) DB errors — `mkTxErrorHandler` (`Hasura/Backends/Postgres/Execute/Types.hs`)
Used for GraphQL queries/mutations (`dmlTxErrorHandler`) and RQL DML. The PG error's SQLSTATE class decides; "expected" errors become **400 without `internal`**:

| PG condition (SQLSTATE) | code | message |
|---|---|---|
| 23001 restrict_violation | constraint-violation | `Can not delete or update due to data being referred. ` + PG message |
| 23502 not_null_violation | constraint-violation | `Not-NULL violation. ` + PG message |
| 23503 foreign_key_violation | constraint-violation | `Foreign key violation. ` + PG message |
| 23505 unique_violation | constraint-violation | `Uniqueness violation. ` + PG message |
| 23514 check_violation | **permission-error** | `Check constraint violation. ` + PG message |
| 23P01 exclusion_violation | constraint-violation | `Exclusion violation. ` + PG message |
| other class 23 | constraint-violation | PG message |
| 22025 invalid_escape_sequence | bad-request | PG message |
| other class 22 (data exception) | data-exception | PG message |
| 42P10 invalid_column_reference | constraint-error | `there is no unique or exclusion constraint on target column(s)` |
| 42704 undefined_object | constraint-error | PG message |
| class 40 (transaction rollback) — only in `defaultTxErrorHandler` | concurrent-update | 40001 → `serialization failure due to concurrent update`; others → PG message |

"PG message" = the server's primary message (`edMessage`), e.g. `duplicate key value violates unique constraint "author_name_key"`. Example (fixture `server/tests-py/queries/graphql_mutation/insert/constraints/author_unique_constraint_error.yaml`):
```json
{"errors":[{"message":"Uniqueness violation. duplicate key value violates unique constraint \"author_name_key\"","extensions":{"path":"$.selectionSet.insert_author.args.objects","code":"constraint-violation"}}]}
```
Path for insert mutation DB errors: `$.selectionSet.<root field name>.args.objects` (insert) or `.args.object` (insert_one) (`Hasura/Backends/Postgres/Execute/Insert.hs` `withPaths`). Other DB errors in queries/mutations: path `$`.

### 6.2 Unexpected DB errors
Any other error → **500** `unexpected` `database query error` with `internal` = PG tx error object:
```json
{"statement": "<SQL text>", "prepared": true, "error": {"exec_status": "FatalError", "hint": null, "message": "...", "status_code": "42P01", "description": null}, "arguments": ["<debug repr of params>", ...]}
```
(`Database/PG/Query/Transaction.hs` `ToJSON PGTxErr`; `PGStmtErrDetail` snake_case: `exec_status`, `status_code`, `message`, `description`, `hint`.) For non-statement failures `error` is a plain string. `arguments` are Haskell `show` renderings of typed params — Rust may emit any representation (clients do not parse it). On `/v1/graphql` the status becomes 200 and `internal` is visible only per §4.

`run_sql` uses its own handler: **400** `postgres-error` `query execution failed`, internal = the same object (`prepared: false`, `arguments: []`). Fixture: `server/tests-py/queries/v1/run_sql/sql_select_query_read_only.yaml`.

### 6.3 Connection errors (`FromPGConnErr QErr`)
* contains `too many clients` → 500 `postgres-max-connections-error` `max connections reached on postgres`, internal = connection error text (string).
* contains `root certificate file` → 500 `postgres-error` `root certificate error`; `certificate file` → `certificate error`; `private key file` → `private-key error` (no internal).
* otherwise → 500 `postgres-error` `connection error`, internal = error text.

---

## 7. GraphQL request errors (exact wording)

All below are `validation-failed` unless noted. `'x'` = single-quoted GraphQL names/types (`toErrorValue` on `G.Name`/`G.GType` uses single quotes, `Hasura/Base/ToErrorValue.hs`); `"x"` = double quotes (`<>>`/`dquote`). Status per §3 (200 on `/v1/graphql`).

### 7.1 Document-level
| Situation | path | message |
|---|---|---|
| `query` string fails to lex/parse | `$.query` | `not a valid graphql query` (fixed text; no position info) |
| `operationName` given, no such named operation | `$` | `no such operation found in the document: "<name>"` |
| `operationName` given but document has an anonymous operation (shorthand `{...}`) | `$` | `operationName cannot be used when an anonymous operation exists in the document` |
| No `operationName` and ≠ 1 operation | `$` | `exactly one operation has to be present in the document when operationName is not specified` |
| Fragment spread to unknown fragment | `$.selectionSet...` | `reference to undefined fragment "<name>"` |
| Fragment cycle | — | `the fragment definition(s) <A>, <B>, and <C> form a cycle` (English list with "and") |
| Duplicate fragment names | — | `parse-failed` `multiple definitions for fragment "<name>"` |
| Mutation but schema has no mutations for role | `$` | `no mutations exist` |
| Subscription root missing | `$` | `no subscriptions exist` |
| Subscription with >1 root field (WS) | `$` | `subscriptions must select one top level field` (`Hasura/GraphQL/Execute.hs`) |
| Duplicate variable definitions | `$` | `parse-failed` `multiple definitions for variable "<name>"` (`Execute/Resolve.hs`) |
| Subscription over HTTP | `$` | `unexpected-payload` `subscriptions are not supported over HTTP, use websockets instead` |
| Allowlist enabled, non-admin, query not allowed | `$` | `query is not allowed`; internal `{"message":"query is not in any of the allowlists"}` |
| Read-only mode mutation | `$` | `not-supported` `Mutations are not allowed when read-only mode is enabled` |

Unused fragments and unused declared variables are **not** errors (validation disabled in source).

### 7.2 Variables (`Hasura/GraphQL/Execute/Resolve.hs`, `.../Parser/Internal/TypeChecking.hs`)
| Situation | path | message |
|---|---|---|
| `variables` contains a key not used by the operation | `$` | `unexpected variables in variableValues: <a>, <b>` (names comma-joined, no quotes) |
| Non-nullable variable with no value and no default | `$` | `expecting a value for non-nullable variable: "<name>"` |
| JSON `null` given for non-nullable variable (default behaviour, unless `HASURA_GRAPHQL_BACKWARDS_COMPAT_NULL_IN_NONNULLABLE_VARIABLES=true`) | `$` | `null value found for non-nullable type: "Int!"` (type rendered GraphQL-style) |
| `$var` referenced but not declared | `$` | `unbound variable "<name>"` |
| Variable used at incompatible location | field path | `variable 'id' is declared as 'Int', but used where 'Int!' is expected` |
| Variable without value at a non-nullable location | field path | `no variable value specified for non-nullable variable no value specified for variable 'x'` (sic: concatenation of the two message parts) |
| Variable object key not a valid GraphQL name | arg path | `variable value contains object with key "<key>", which is not a legal GraphQL name` |

### 7.3 Selection sets and arguments (`.../Parser/Internal/Parser.hs`, `Collect.hs`, `Input.hs`)
Path: `$.selectionSet.<fieldName>.selectionSet.<fieldName>...` using field **names** (not aliases); arguments add `.args.<argName>` then nested input keys / `[i]`.

| Situation | message |
|---|---|
| Unknown field | `field 'notPresentCol' not found in type: 'author'` (root: `type: 'query_root'`, `'mutation_root'`, `'subscription_root'`) |
| Object field without selection set | `missing selection set for 'author'` — the quoted name is the field's **object type name** (e.g. `'author'`, `'author_aggregate'`, `'author_mutation_response'`), not the field name; path `$.selectionSet.<field>.selectionSet` (`Parser/Internal/Parser.hs` `selectionSetObject`) |
| Selection set on scalar | `unexpected subselection set for non-object field` |
| Unknown argument | `'author' has no argument named 'foo'` (field name, then argument name) |
| Same response key, different fields | `selection of both 'a' and 'b' specify the same response name, 'x'` |
| Same field twice with different args | `inconsistent arguments between multiple selections of field 'author'` |
| Unknown input object field | `field 'foo' not found in type: 'author_bool_exp'` |
| Missing required input field / argument | `missing required field 'objects'` |
| Bad enum value | `expected one of the values ['asc', 'asc_nulls_first', ...] for type 'order_by', but found 'foo'` (`Parser/Internal/Input.hs` `enum`: values in **schema definition order**, list rendered `[` + `, `-separated single-quoted names + `]`, then `, but found '<value>'`), or, when the input is not an enum literal / a JSON string that is a valid GraphQL name: `expected an enum value for type 'order_by', but found <value-desc>` (e.g. `a string`, `an integer`). Table-enum columns parsed outside GraphQL (`Hasura/RQL/Types/Column.hs`): `400 unexpected-payload` `expected one of the values "a", "b" for type "t_enum", given "x"` (double-quoted, sorted) |
| Wrong value kind for object type | `expected an object for type 'author_bool_exp', but found null` (fixture `server/tests-py/queries/graphql_validation/null_value_err.yaml`) |
| List expected but null | `expected a list, but found null` |
| Scalar type mismatch | `expected <desc> for type '<Type>', but found <value-desc>` — `<desc>` ∈ `a boolean`, `a 32-bit integer`, `a float`, `a string`, `a String or a 32-bit integer` (ID), `a UUID`, `a non-negative 32-bit integer` (limit/offset), `a 32-bit integer, or a 64-bit integer represented as a string`, `Decimal represented as a string`, `an enum value`, `an object`; `<value-desc>` ∈ `null`, `a boolean`, `a string`, `a number`, `a list`, `an object` (JSON values) or `an integer`, `a float`, `an enum value` (GraphQL literals), or `no value specified for variable 'x'` |
| Int overflow | `parse-failed` `The value 2.147483648e9 lies outside the bounds or is not an integer. Maybe it is a float, or is there integer overflow?` (number rendered via Haskell `Scientific` show — e.g. `2.147483648e9`; Rust must format numbers the same way to match exactly) |
| Float overflow | `parse-failed` `The value <n> lies outside the bounds. Is it overflowing the float bounds?` |
| Null for non-null column scalar | `unexpected null value for type 'Int'` (fixture `graphql_validation/null_variable_value.yaml`) |
| Column value fails Postgres-type parse (e.g. bad uuid / date / geojson) | `parse-failed` with the type parser message, e.g. `A LinearRing needs at least 4 Positions`, `unexpected geometry type: Random`, aeson messages like `parsing Int32 failed, expected Number, but encountered String` (inferred) |
| Unknown directive | `directive 'foo' is not defined in the schema` |
| Directive in wrong location | `directive 'include' is not allowed on a query` (location phrase from `humanReadable`) |
| Repeated directives | `the following directives are used more than once: ['skip']` |
| `@cached` etc. on CE | accepted and ignored (inferred) |

### 7.4 Session / permission errors at execution
| Situation | code | path | message |
|---|---|---|---|
| Permission references a session variable absent from the request | not-found | `$` | `missing session variable: "x-hasura-user-id"` (lower-cased name, double-quoted) |
| Insert/update permission `check` fails | permission-error | `$.selectionSet.<field>.args.objects` (insert) / `$` | `check constraint of an insert/update permission has failed` |
| Role has no access to the table/field | validation-failed | field path | the field simply doesn't exist for the role → `field 'x' not found in type: 'query_root'` |
| Role has no root fields at all | — | — | query root exposes `no_queries_available: String` returning `There are no queries available to the current role. Either there are no sources or remote schemas configured, or the current role doesn't have the required permissions.` (api-test `Test/Queries/Errors/NoQueriesAvailableSpec.hs`) |

### 7.5 Authentication errors (raised before execution; `/v1/graphql` → status 200 unless PRESERVE_401)
| Situation | code | status | message |
|---|---|---|---|
| Admin secret configured, none sent, no unauthorized role, no JWT/webhook | access-denied | 401 | `"x-hasura-admin-secret" required, but not found` |
| Wrong admin secret | access-denied | 401 | `invalid "x-hasura-admin-secret"` |
| Webhook returns 401 | access-denied | 401 | `Authentication hook unauthorized this request` |
| Webhook other non-200 | unexpected | 500 | `Invalid response from authorization hook` |
| Webhook 200 but body not JSON object | unexpected | 500 | `Invalid response from authorization hook: <aeson err>` |
| Webhook unreachable | unexpected | 500 | `webhook authentication request failed` |
| Webhook 200 without `X-Hasura-Role` | invalid-params | 400 | `"x-hasura-role" not found in session variables` |
| JWT mode, no token, no unauthorized role | invalid-headers | 400 | `Missing 'Authorization' or 'Cookie' header in JWT authentication mode` |
| `Authorization` not `Bearer <token>` | invalid-headers | 400 | `Malformed Authorization header` |
| Token invalid | invalid-jwt | 400 | `Could not verify JWT: <JoseError>` (§5) |
| Requested role not allowed | access-denied | 400 | `Your requested role is not in allowed roles` |
| Bad backend-only header value | bad-request | 400 | `"x-hasura-use-backend-only-permissions": <reason>` |
Full auth semantics: auth spec.

### 7.6 Action handler errors (`Hasura/GraphQL/Execute/Action.hs`)
* Handler 2xx with body that is not JSON → 500 `unexpected` `not a valid json response from webhook`; internal (if allowed) `{"error":"invalid JSON: <err>","request":{...},"response":{...}}`.
* Handler 2xx with JSON not matching the output type → 400 `unexpected` (from `validateResponse`, e.g. `field "x" expected in webhook response, but not found`, `expecting not null value for field "x"`, `got null for the action webhook response`, `got scalar String for the action webhook response, expecting ...`; extra fields are ignored — see actions spec), internal (if allowed) `{"error":"unexpected response","request":{...},"response":{...}}`.
* Handler 4xx with body `{"message": str, "code"?: str, "extensions"?: json}` → error with the **handler's HTTP status** (e.g. 400/401/403/404/422; on `/v1/graphql` rewritten to 200 except 401 with PRESERVE_401), message = `message`, code = `code` if given else `unexpected`, and if `extensions` given it **replaces** the GraphQL `extensions` object (§2). 4xx body lacking `message` → parse error (400 `parse-failed`) with internal per policy.
* Handler 5xx / 3xx / 1xx → 500 `unexpected` `internal error`; internal (if allowed) `{"error":"expecting 2xx or 4xx status code, but found 500","request":{...},"response":{...}}`.
* Request/response transform failures → 500 `Request Transformation Failed` / `Response Transformation Failed` with internal = transform error.
* When internal errors are hidden, the 500 cases are plain `internal error` / `not a valid json response from webhook` without detail.

### 7.7 Remote schema errors
See remote-schemas spec. Summary: transport failures → `remote-schema-error` (400/500), response errors → `{"data":null,"errors":[...verbatim...]}`, missing field in response → `remote-schema-error` `expecting key <alias>`.

---

## 8. JSON-body decoding errors (`parse-failed`) — non-GraphQL APIs and GraphQL request envelope

* Invalid JSON (any route using `parseBody`): `400 invalid-json`, path `$`, message = JSON decoder error. Hasura's text is aeson's (e.g. `Error in $: Failed reading: not a valid json value at '...'`, wording varies with aeson version). **Clients must not rely on it**; Rust should produce a similar `Error in $: <serde message>` text.
* Decoding into request types: `400 parse-failed`, path = JSON path where decoding failed, message = aeson message. Common aeson-2 messages to mimic:
  * missing key: `key "where" not found` (older aeson / `formatMsg` path: `the key 'action' was not present`)
  * wrong JSON type: `parsing Object failed, expected Object, but encountered String`; `parsing Int failed, expected Number, but encountered String`; `parsing Text failed, expected String, but encountered Number`; `parsing Bool failed, expected Boolean, but encountered String` (inferred pattern `parsing <HaskellType> failed, expected <JSONKind>, but encountered <JSONKind>`; JSON kinds: Object, Array, String, Number, Boolean, Null)
  * `NonEmptyText` fields: `empty string not allowed`
  * version field: `expected 1 or 2, encountered 3` (path `$.version`)
  * custom `fail` texts from each command parser (e.g. `Expecting 'ignore' or 'update'`, `A column should either be a string or an object (relationship)`, `atleast one amongst insert/update/delete/enable_manual spec must be provided`, `only alphanumeric and underscore and hyphens allowed for name`).
* Path quirks (`10-http-apis.md` §4.2/§4.3): `/v1/query` arg-decoding paths are relative to `args` (`$`, `$.name`; aeson `TaggedObject` generic decoding), whereas **`/v2/query` arg-decoding paths include `args`** (`$.args.sql`, `$.args[0].args.table` — hand-written `o .: "args"` in `V2Query.hs`) but its *execution* errors are NOT prefixed (`$`, or `$[i]` inside `bulk`); `/v1/metadata` backend-prefixed commands use `$.args...` and wrap the message (`Error when parsing command <cmd>.\nSee our documentation at https://hasura.io/docs/latest/graphql/core/api-reference/metadata-api/index.html#metadata-apis.\nInternal error message: <msg>`); execution-time errors in `/v1/query` are prefixed `$.args`.
* For GraphQL routes, envelope decoding failures (e.g. body is a JSON string, or `query` missing) are reported in GQL error JSON with path `$` (status 200 on `/v1/graphql`).
* The `formatMsg` rewrite in `liftIResult`: a message containing `the key X was not present` becomes `the key 'X' was not present`.

---

## 9. Other fixed messages worth reproducing

| Message | code | status | Source |
|---|---|---|---|
| `resource does not exist` | not-found | 404 | `App.hs` disabled API / unknown route |
| `restricted access : admin only` (path `$.args`) | access-denied | 400 | `Hasura/App.hs` `accessDeniedErrMsg` (v1/query admin-only types, all v1/metadata, all v2/query for non-admin) |
| `You have to be an admin to access this endpoint` (path `$`) | access-denied | 400 | `onlyAdmin` (explain, config, pg_dump, dev, swagger) |
| `The Relay API is disabled. To enable it, set the HASURA_GRAPHQL_ENABLE_RELAY environment variable to 'true'.` | not-supported | 400 | relay |
| `PersistedQueryNotSupported` | not-supported | 400→200 | CE APQ / GET /v1/graphql |
| `Internal Server Error` | unexpected | 500 | unhandled exceptions; internal `{"error": "<text>"}` when from handler `error` calls |
| `metadata resource version referenced (N) did not match current version` | conflict | 409 | `RQL/DDL/Schema/Catalog.hs` |
| `metadata cannot be modified in read-only mode` | not-supported | 400 | metadata API |
| `metadata cannot be modified in maintenance mode` | unexpected | 500 | metadata/query APIs |
| `Cannot run write queries when read-only mode is enabled` | not-supported | 400 | v1/v2 query |
| `cannot continue due to newly found inconsistent metadata` | unexpected | 400 (bulk tracking) / 500 (`withNewInconsistentObjsCheck`) | `RQL/Types/SchemaCache/Build.hs`; internal = list of inconsistent objects `{definition, reason, name, type}` |
| `cannot continue due to inconsistent metadata` | unexpected | 400 | same file |
| `Endpoint not found` | not-found | 404 | REST |
| `Allowed methods: GET, POST` | method-not-allowed | 405 | REST |
| `Multiple endpoints match request` | unexpected | 500 | REST |
| `query execution failed` | postgres-error | 400 | run_sql |
| `database query error` | unexpected | 500 | unexpected PG error |
| `error while executing pg_dump` | unexpected | 500 | pg_dump (internal = stderr) |
| `Event log cleanup feature is enterprise edition only` | not-supported | 400 | CE |
| `Unrecognised RQLQuery type: <t>` (path `$`) | parse-failed | 400 | `/v2/query` unknown `type` |
| `Only read-only queries are allowed in a concurrent_bulk` | unexpected | 500 | `/v2/query` |
| `objects should not be empty` | unexpected-payload | 400 | RQL `insert` (`RQL/DML/Insert.hs`) |
| `Expecting 'constraint' or 'constraint_on' when the 'action' is 'update'` / `'constraint' and 'constraint_on' cannot be set at a time` | unexpected-payload | 400 | RQL `insert` `on_conflict` |
| `constraint "<c>" for table "<t>" does not exist` | unexpected | 400 | RQL `insert` `on_conflict` |
| `column "<c>" is not insertable for role "<r>"` | not-supported | 400 | RQL `insert` |
| `atleast one of $set, $inc, $mul has to be present` | unexpected-payload | 400 | RQL `update` |
| `column "<c>" is not updatable for role "<r>"; its value is predefined in permission` | not-supported | 400 | RQL `update` |
| `unexpected negative value` (path `…limit`/`…offset`) | not-supported | 400 | RQL `select` |
| `string format for 'order_by' entry : {+/-}column Eg : +posted` | parse-failed | 400 | RQL `select` |
| `A column should either be a string or an object (relationship)` | parse-failed | 400 | RQL `select` |
| `table "<t>" does not exist` | not-exists | 400 | RQL DML |
| `Unexpected variable <name>` / `List variables are not currently supported in URL or Query parameters. (Variable <n>, with value "<v>")` | bad-request | 400 | REST endpoints |
| `A stored query should contain exactly one definition` | unexpected | 500 | REST |

Healthz bodies (`OK`, `WARN: inconsistent objects in schema`, `ERROR: inconsistent objects in schema`, `ERROR`) are plain text, not JSON (see 10-http-apis.md §6.1).

---

## 10. Websocket error payloads (pointer)

Same QErr values are used, but the framing differs (subscriptions spec):
* `graphql-ws` (Apollo legacy) / `graphql-transport-ws` on `/v1/graphql`, `/v1/relay`: `error` message payload uses the GraphQL-compliant shape `[{"message", "extensions":{"path","code"}}]` (exact framing per protocol).
* `/v1alpha1/graphql` websocket: legacy payload = QErr JSON `{"path","error","code"}`; execution errors as `{"data":null,"errors":[{"path","error","code"}]}`; `start received before the connection is initialised` code `start-failed`.

---

## 11. Rust implementation guidance

* `struct QErr { path: Vec<PathElem>, status: StatusCode, message: String, code: Code, extra: Option<Extra> }`, `enum Extra { Internal(serde_json::Value), Extensions(serde_json::Value) }` mirroring Haskell; helpers `err400(code,msg)` … and `with_path_key("args")`, `with_path_index(i)` that **prepend** path elements while an error propagates (Haskell `withPathK`/`withPathI`).
* Two encoders: `encode_qerr(err, include_internal)` (key order error,path,code,internal) and `encode_gql_err(err, include_internal)`; a third, `encode_gql_errors_envelope` wrapping in `{"errors":[...]}`. Use an order-preserving JSON writer (serde_json with `preserve_order`, or manual writing) to keep key order — byte-exact diffing in golden tests depends on it.
* Single `fn respond_error(route_kind, err, include_internal, preserve_401)` applying §3.
* Keep message templates in one module (`errors/messages.rs`) with functions like `field_not_found(field, ty)`, `missing_session_variable(var)` to guarantee quoting conventions (`'x'` for GraphQL names/types, `"x"` for `<>>`/`dquote` sites).
* Number formatting in messages (`2.147483648e9`) follows Haskell `Scientific` `show` = `formatScientific Generic Nothing`: with base-10 digits `d1d2…dn` and exponent `e` such that value = `0.d1…dn × 10^e`, it uses **fixed** notation when `0 ≤ e ≤ 7` (i.e. `0.1 ≤ |x| < 10^7`), else **exponent** notation `d1.d2…dn e(e-1)`; a single digit gets `.0` (`5.0e9`, `1.0e-2`), and fixed integers also get `.0` (`3.0`). Examples: `2147483648` → `2.147483648e9`; `1.5` → `1.5` (Int parser: `The value 1.5 lies outside the bounds or is not an integer. …`); `1e400` → `1.0e400`; `12345678` → `1.2345678e7`; `-3000000000` → `-3.0e9`. Implement this small formatter for byte-exact messages.

---

## Upstream tests to reuse

* `server/tests-py/test_v1alpha1_endpoint.py` — status codes on legacy endpoint vs `/v1/graphql`, legacy WS error format.
* `server/tests-py/queries/graphql_validation/*.yaml` + `test_validation.py` — null/variable/type-mismatch messages.
* `server/tests-py/queries/graphql_query/basic/*err*.yaml`, `graphql_query/permissions/*fail*.yaml`, `select_articles_without_required_headers.yaml` — field-not-found, missing session variable (`not-found`, path `$`).
* `server/tests-py/queries/graphql_mutation/insert/constraints/*.yaml`, `graphql_mutation/transactions/*.yaml` — PG constraint violation mapping and paths (Postgres files only; skip `*_mssql.yaml`).
* `server/tests-py/queries/graphql_mutation/insert/permissions/*fail*.yaml` — `permission-error` check failures.
* `server/tests-py/queries/graphql_mutation/insert/basic/insert_integer_overflow.yaml` — `parse-failed` integer overflow wording.
* `server/tests-py/queries/graphql_mutation/insert/geojson/*err*.yaml` — custom scalar parse errors.
* `server/tests-py/queries/v1/basic/*err*.yaml`, `v1/update/basic/person_error_no_where_clause.yaml`, `v1/insert/onconflict/*no_action*`, `v1/non_empty_text/*.yaml`, `v1/select/limits/*error*.yaml` — `parse-failed` messages and paths on `/v1/query` (messages are aeson-version dependent).
* `server/tests-py/queries/v1/run_sql/sql_select_query_read_only.yaml`, `sql_no_tx.yaml`, `sql_query_as_user_error*.yaml` — `postgres-error` with internal, `access-denied` on `/v1/query` and `/v2/query`.
* `server/tests-py/queries/v2/basic/select_article_role_error.yaml` — `/v2/query` admin-only.
* `server/tests-py/queries/endpoints/*.yaml` — REST 404/405/400 QErr bodies.
* `server/tests-py/queries/unauthorized_role/*.yaml`, `test_jwt.py`, `test_webhook.py`, `test_auth_webhook_cookie.py` — auth error codes/messages (JWT tests need key setup; note message-level assertions are warnings only).
* `server/tests-py/queries/actions/**` (sync error cases) — action error passthrough incl. `extensions` override.
* `server/lib/api-tests/src/Test/Queries/Errors/{InvalidQuerySpec,MissingTableSpec,NoQueriesAvailableSpec,EmptyRootsAreNotAvailableSpec}.hs` — exact GraphQL error bodies (exact comparison).
* `server/lib/api-tests/src/Test/Queries/VariablesAliasesFragments/**` — variable/fragment errors.
* `server/lib/api-tests/src/Test/API/Metadata/{TestConnectionTemplateSpec,BulkKeepGoingSpec,InconsistentSpec,WarningsSpec}.hs` — metadata error envelopes (`unknown metadata command ...`, inline errors in bulk_keep_going). Skip EE-only/non-Postgres expectations.
* `server/lib/api-tests/src/Test/Auth/Authorization/**` — permission-related errors.
* Skip: `*_mssql.yaml`, `*_bigquery.yaml`, `*_citus.yaml`, DataConnector modules, `test_query_cache.py`, API-limit tests (EE).
