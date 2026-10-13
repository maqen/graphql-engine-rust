# 13. Miscellaneous features

Covers: allow lists and query collections, RESTified endpoints (and the OpenAPI document),
API limits, GraphQL schema introspection options, query tags, network / TLS allowlist,
`metrics_config`, OpenTelemetry config, naming conventions (reference), Apollo Federation,
the `@cached` directive, source health checks, Prometheus metrics, automatic persisted queries,
event log cleanup. Each section says whether the feature is **CE** (implemented in the open-source
server), **CE-stored/EE-enforced** (metadata accepted and stored by CE, behaviour only in EE/Cloud),
or **EE-only**.

Main upstream sources (paths relative to the repo root):

- `server/src-lib/Hasura/RQL/Types/QueryCollection.hs`, `server/src-lib/Hasura/RQL/DDL/QueryCollection.hs`, `server/src-lib/Hasura/RQL/Types/Allowlist.hs`, `server/src-lib/Hasura/GraphQL/Execute.hs` (`checkQueryInAllowlist`).
- `server/src-lib/Hasura/RQL/Types/Endpoint.hs`, `server/src-lib/Hasura/RQL/Types/Endpoint/Trie.hs`, `server/src-lib/Hasura/RQL/DDL/Endpoint.hs`, `server/src-lib/Hasura/Server/Rest.hs`, `server/src-lib/Hasura/Server/App.hs`, `server/src-lib/Hasura/Server/OpenAPI.hs`.
- `server/src-lib/Hasura/RQL/Types/ApiLimit.hs`, `server/src-lib/Hasura/RQL/DDL/ApiLimit.hs`.
- `server/src-lib/Hasura/RQL/Types/GraphqlSchemaIntrospection.hs`, `server/src-lib/Hasura/RQL/DDL/GraphqlSchemaIntrospection.hs`.
- `server/src-lib/Hasura/QueryTags.hs`, `server/src-lib/Hasura/QueryTags/Types.hs`, `server/src-lib/Hasura/RQL/DDL/QueryTags.hs`.
- `server/lib/hasura-extras/src/Network/Types/Extended.hs`, `server/src-lib/Hasura/RQL/DDL/Network.hs`, `server/src-lib/Hasura/Server/DynamicTlsPermissions.hs` (inferred path name).
- `server/src-lib/Hasura/RQL/Types/OpenTelemetry.hs`, `server/src-lib/Hasura/RQL/DDL/OpenTelemetry.hs`.
- `server/src-lib/Hasura/RQL/Types/HealthCheck.hs`, `server/src-lib/Hasura/GraphQL/ApolloFederation.hs`, `server/lib/schema-parsers/src/Hasura/GraphQL/Parser/Directives.hs`.
- `server/src-lib/Hasura/App.hs` (CE instances of the "pro" hooks: `cacheLookup`, `HasResourceLimits`, `MonadQueryTags`, `executeIntrospection`, persisted queries, metrics).
- Metadata API dispatch: `server/src-lib/Hasura/Server/API/Metadata/Instances.hs`.

Metadata-format details (where these objects live in `metadata/*.yaml`) are in the metadata doc;
this doc gives the object shapes again where needed for the API commands.

Notation for messages: Hasura's `<<>` operator renders the value in double quotes, so
`"query collection with name " <<> n <<> " already exists"` renders as
`query collection with name "c1" already exists`. All error responses below are QErr JSON
`{"path":"$...","error":"<message>","code":"<code>"}` with the given HTTP status (see the HTTP API doc).

---

## Scope and priority

| Feature | Edition | Priority |
|---|---|---|
| Query collections (metadata CRUD) | CE | must (used by REST endpoints and allowlist; part of metadata round-trip) |
| Allowlist, global scope, enforcement via `ENABLE_ALLOWLIST` | CE | should |
| Allowlist role-based scope (`scope.global=false`) | stored in CE, **enforced only in EE** (CE uses global-only mode) | store: must; enforce: ignore (match CE) |
| RESTified endpoints (`/api/rest/*`) | CE | should |
| OpenAPI (`/api/swagger/json`) | CE | may |
| API limits (`set_api_limits`) | CE-stored, EE-enforced | store: must; enforce: ignore |
| `graphql_schema_introspection` (disable introspection per role) | CE-stored, EE-enforced | store: must; enforce: ignore (could be offered as opt-in extra) |
| Query tags | CE-stored (source config), EE-emitted | store: must; emit: may |
| Network / TLS allowlist | CE | may |
| `metrics_config` | CE-stored, Cloud-used | store: must |
| OpenTelemetry config | CE-stored/validated, EE-exported | store+validate: must; export: may (nice extra) |
| W3C/B3 trace header propagation | CE | may |
| Naming conventions | CE | should (details in the schema-generation doc) |
| Apollo Federation | CE (experimental) | may |
| `@cached` directive | accepted by CE, no caching | must accept (parses, no-op) |
| Source health checks | CE-stored, EE-run | store: must |
| Prometheus `/v1/metrics` | EE-only | ignore (optional extra) |
| Automatic persisted queries | EE-only (CE rejects) | match CE rejection |
| Event trigger log cleanup (`cleanup_config`, `cleanup_event_trigger_logs`) | EE-only (CE errors) | match CE error |

General rule for the Rust engine: every metadata object of every CE-stored/EE feature must be
accepted, validated the same way, persisted, and exported unchanged by `export_metadata`
(round-trip fidelity); behaviour follows CE (usually "no effect").

---

## 1. Query collections

### 1.1 Shapes

Metadata top-level key `query_collections` (file `metadata/query_collections.yaml`):

```yaml
- name: allowed-queries          # CollectionName: non-empty text
  definition:
    queries:
      - name: getAuthors         # QueryName: non-empty text
        query: |
          query getAuthors { author { id name } }
  comment: optional text          # optional
```

`server/src-lib/Hasura/RQL/Types/QueryCollection.hs`:
- `CreateCollection {name (required), definition (required) {queries (required list)}, comment (optional)}`.
- `ListedQuery {name, query}`. `query` must parse as a GraphQL executable document
  (`GQLQueryWithText` keeps the original text for re-export; export emits the original text
  verbatim). Parse error -> 400 `parse-failed` (inferred code) at path `$.definition.queries[i].query`.
- Export order: collections sorted by `name` (`queryCollectionsToOrdJSONList`, sorted by `_ccName` in
  `server/src-lib/Hasura/RQL/Types/Metadata/Serialization.hs`); each object keys in order
  `name`, `definition`, `comment` (comment omitted if absent (inferred)).

### 1.2 Metadata API commands

All via `POST /v1/metadata` (also accepted on legacy `/v1/query` (inferred)); success response `{"message":"success"}` unless noted.

| type | args | Errors |
|---|---|---|
| `create_query_collection` | `CreateCollection` | name exists: 400 `already-exists` `query collection with name "<n>" already exists` (path `$.args.name`); duplicate query names inside: 400 `not-supported` `found duplicate query names <list>` (path `$.args.definition.queries`) |
| `rename_query_collection` | `{"name": n, "new_name": m}` | `n` missing: 400 `not-exists` `query collection with name "<n>" does not exist`; `m` exists: 400 `already-exists` `query collection with name "<m>" already exists` (path `$.args.new_name`). Renames references in allowlist and REST endpoints (inferred). |
| `drop_query_collection` | `{"collection": n, "cascade": bool}` - `cascade` is **required** (generic parser, Bool field) | missing: `not-exists` `query collection with name "<n>" does not exist`; in allowlist and `cascade=false`: 400 `dependency-error` `query collection with name "<n>" is present in the allowlist; cannot proceed to drop. please use cascade to confirm you wish to drop it from the allowlist as well`. With cascade, also removed from allowlist. REST endpoints referencing it become inconsistent (inferred: drop succeeds, endpoint appears in inconsistent objects; `withNewInconsistentObjsCheck` would actually reject the drop if new inconsistencies appear - expect 400 `unexpected` "cannot continue due to new inconsistent metadata" (see metadata doc)). |
| `add_query_to_collection` | `{"collection_name": c, "query_name": q, "query": "<graphql text>"}` | collection missing: `not-exists`; query name exists: 400 `already-exists` `query with name "<q>" already exists in collection "<c>"` |
| `drop_query_from_collection` | `{"collection_name": c, "query_name": q}` | collection missing: `not-exists`; query missing: 400 `not-found` `query with name "<q>" not found in collection "<c>"` |

### 1.3 Validation (schema cache)

Each collection query is validated against the **admin** GraphQL schema when building the schema
cache (`server/src-lib/Hasura/RQL/Types/SchemaCache.hs` / `Hasura/Server/...` - "query collection
validation"). Invalid queries produce an inconsistent object (type `query_collection`) only if
the query is used by a REST endpoint or the collection is in the allowlist (inferred: unused
collections are not validated). Message format:

```
In query collection "<c>" the query "<q>" is invalid with the following error(s): [<errors>]. This query is being used in the following REST endpoint(s): [<endpoint names>]. This query is in allowlist.
```

(the "REST endpoint(s)" and "in allowlist" sentences are appended only when applicable).
Because of `withNewInconsistentObjsCheck`, a metadata change that makes a collection query
invalid (e.g. untracking a table it uses) fails unless `allow_inconsistent_metadata` is set (inferred, standard behaviour).

---

## 2. Allowlist

### 2.1 Shape

Metadata key `allowlist` (file `metadata/allow_list.yaml`):

```yaml
- collection: allowed-queries
  scope:                 # optional on input, default {global: true}
    global: true
- collection: role-queries
  scope:
    global: false
    roles: [user, editor]
```

`server/src-lib/Hasura/RQL/Types/Allowlist.hs`:
- `AllowlistEntry {collection, scope}`; `scope` optional on input (default global).
- `AllowlistScope`: `{"global": true}` or `{"global": false, "roles": [non-empty list]}`.
  - `global: true` with `roles` present -> parse error `roles are not allowed when global is true`.
  - `global: false` without `roles` -> missing key error (`key "roles" not found`).
  - duplicate roles -> `duplicate roles are not allowed`.
- Export: `{"collection": c, "scope": {"global": true}}` - `scope` is always emitted (generic
  ToJSON), entries sorted by collection name (`allowlistToOrdJSONList`).

### 2.2 Commands

| type | args | Behaviour / errors |
|---|---|---|
| `add_collection_to_allowlist` | `{"collection": c, "scope"?: scope}` | collection must exist (`not-exists` `query collection with name "<c>" does not exist`, path `$.args.collection`). If already in allowlist: **200** with `{"message": "collection \"<c>\" already exists in the allowlist, scope ignored; to change scope, use update_scope_of_collection_in_allowlist"}` (not an error). Else adds; response `{"message":"success"}`. |
| `drop_collection_from_allowlist` | `{"collection": c}` | collection must exist (`not-exists`); not in allowlist: 400 `not-found` `collection "<c>" doesn't exist in the allowlist`. |
| `update_scope_of_collection_in_allowlist` | `{"collection": c, "scope": scope}` (`scope` required) | collection must exist; not in allowlist: 400 `not-found` `collection "<c>" doesn't exist in the allowlist`. |

### 2.3 Enforcement (CE)

`server/src-lib/Hasura/GraphQL/Execute.hs` `checkQueryInAllowlist`, called from CE `AppM`
(`server/src-lib/Hasura/App.hs`: `checkQueryInAllowlist enableAL AllowlistModeGlobalOnly ...`):

- Only when `HASURA_GRAPHQL_ENABLE_ALLOWLIST=true` **and** the request role is not `admin`.
- CE mode is `AllowlistModeGlobalOnly`: only collections with `scope.global = true` count;
  role-scoped entries are ignored entirely (a role-scoped query is rejected in CE). EE uses
  `AllowlistModeFull` (global + the role's collections) (verified constructor names in
  `RQL/Types/Allowlist.hs`; CE call site `checkGQLExecution` in `App.hs`). Match CE.
- Applies to every GraphQL entry point: `/v1/graphql`, `/v1alpha1/graphql`, `/v1/relay`,
  websocket `start`/`subscribe` messages, and REST endpoints (they go through `runGQ`).
- Not applied to the metadata/query APIs.
- Matching uses `NormalizedQuery` (verified, `normalizeQuery` / `allowlistAllowsQuery` in
  `server/src-lib/Hasura/RQL/Types/Allowlist.hs`): the parsed executable document of the request
  (`G.ExecutableDocument`, all definitions, including fragments) is compared by **AST equality**
  (`HashSet` membership) with each stored query after removing every field named `__typename`
  from selection sets at any depth (inside fields, inline fragments, operation and fragment
  definitions; fragment *spreads* are kept as-is). Both sides are normalized the same way.
  Consequences, following the `graphql-parser` AST (`lib/graphql-parser/.../Syntax.hs`):
  - do NOT matter: whitespace, commas, comments, string quoting style (block vs normal string
    both become `VString`), **order of arguments** of a field or directive and **order of keys
    in object literals** (both are `HashMap`s), `__typename` selections;
  - DO matter: definition order, operation names and type, shorthand `{ a }` vs `query { a }`
    (untyped vs typed operation are different constructors), variable definitions (order, types,
    defaults), aliases, argument values, directives (order and arguments), selection order,
    fragment names and contents, `1` vs `1.0` (Int vs Float literal).
  A request whose document contains several operations must match a stored document with the
  same definitions (the whole document is compared, not just the selected operation). Rust:
  implement as structural equality over a normalized AST with map-like argument/object-field
  comparison (or hash a canonical printed form that sorts arguments and object fields).
- Failure: QErr 400 `validation-failed`, message `query is not allowed`, internal
  `{"message":"query is not in any of the allowlists"}`. On `/v1/graphql` this is encoded as
  GraphQL error with HTTP 200:
  ```json
  {"errors":[{"extensions":{"path":"$","code":"validation-failed"},"message":"query is not allowed"}]}
  ```
  (`internal` `{"message":"query is not in any of the allowlists"}` added under
  `extensions.internal` only per the internal-errors config - admin never hits this path, so only
  in dev mode.) Check point: CE `checkGQLExecution` runs it right after parsing the request,
  before validation/execution (verified, `App.hs`). Websocket: sent as an `error` message for the
  operation (protocol-specific framing, see subscriptions doc). REST endpoint: HTTP 400 with the
  QErr body.
- Introspection queries are also subject to the allowlist (the console's introspection query must
  be allowlisted for non-admin roles) (inferred from the general rule; upstream docs state
  "introspection queries must be added to the allowlist").

---

## 3. RESTified endpoints

### 3.1 Metadata

Metadata key `rest_endpoints` (file `metadata/rest_endpoints.yaml`)
(`server/src-lib/Hasura/RQL/Types/Endpoint.hs`):

```yaml
- name: get_author_by_id            # EndpointName (non-empty text)
  url: author/:id                   # EndpointUrl, no leading /api/rest/
  methods: [GET]                    # non-empty list of GET|POST|PUT|PATCH|DELETE
  definition:
    query:
      collection_name: allowed-queries
      query_name: getAuthorById
  comment: null                     # optional; exported as null when absent
```

Export: sorted by `url` (`endpointsToOrdJSONList`, sorted by `_ceUrl`). Key order `name, url,
methods, definition, comment` (inferred from record order).

Commands:

| type | args | errors |
|---|---|---|
| `create_rest_endpoint` | the endpoint object above | name exists: 400 `already-exists` `Endpoint with name: <n> already exists` (unquoted) |
| `drop_rest_endpoint` | `{"name": n}` | missing: 400 `not-exists` `endpoint with name: <n> does not exist` |

(There is no update command; tools drop+create.)

### 3.2 Consistency checks (schema cache build)

`server/src-lib/Hasura/RQL/DDL/Schema/Cache.hs` (endpoint section) and
`server/src-lib/Hasura/RQL/Types/Endpoint/Trie.hs`. Each failing endpoint is an inconsistent
object of type `endpoint`:

Reference/definition checks (verified, `buildRESTEndpoints`/`resolveEndpoint` in
`RQL/DDL/Schema/Cache.hs`); every message is prefixed `in endpoint <name>: ` (unquoted):
- Collection missing: 400 `not-exists` `collection with name <c> does not exist` (unquoted,
  different from section 1's wording); query missing: `query with name <q> does not exist in collection <c>`.
- Zero definitions: 400 `bad-request` `query with name <q> has no definitions.`; more than one
  definition (incl. operation + fragment): `query with name <q> has multiple definitions.`
- A typed `subscription` operation: 405 `method-not-allowed` `query with name <q> is a subscription`.
- A single shorthand `{ ... }` operation passes this check but fails at request time (3.3).
- Path checks:
  - Duplicate variable names in the URL: `Duplicate variables found in endpoint path: <url>`.
  - Empty segments (e.g. `a//b`, trailing `/`) or bare `:`: `Empty segments or unnamed variables are not allowed: <url>`.
  - Ambiguity across endpoints: two endpoints whose paths can match the same concrete URL for a
    shared method (e.g. `a/:id` vs `a/b` with GET on both) -> both inconsistent: `Ambiguous URL paths: <url1>, <url2>` (internal lists the conflicts).
- The query is validated as in 1.3.

### 3.3 Routing and request handling (CE)

`server/src-lib/Hasura/Server/App.hs` (`hookRouteAll ("api" <//> "rest" <//> wildcard)`) and
`server/src-lib/Hasura/Server/Rest.hs` (`runCustomEndpoint`, `resolveVar`):

- Route `/api/rest/<path>` for any method; not gated by `ENABLED_APIS`. Normal auth applies
  (admin secret / JWT / webhook / unauthorized role); the request role must have permission for
  the fields in the stored query.
- Method mapping: `GET/POST/PUT/DELETE/PATCH` supported; other standard methods (e.g. HEAD,
  OPTIONS when not handled by CORS middleware) -> 400 `bad-request` `Method <M> not supported.`;
  non-standard method -> 400 `Nonstandard method not allowed for REST endpoints`.
- Path matching: `<path>` split on `/` (so a trailing slash yields an empty final segment and
  will not match); trie match where `:name` segments match any single segment.
  - No path match: 404 `not-found` `Endpoint not found`.
  - Path matches but method not allowed: 405 `method-not-allowed` `Allowed methods: GET, POST`
    (comma-separated (`commaSeparated`)).
  - Multiple matches: 500 `unexpected` `Multiple endpoints match request`.
- Variable collection, in increasing precedence (later wins; `HashMap.fromList` over the
  concatenation `queryParams ++ bodyParams ++ pathParams`):
  1. URL query parameters (string values, `Left text`).
  2. Request body, if it decodes as a JSON **object** (any method, including GET): each top-level
     key -> JSON value (`Right json`). Non-object or invalid JSON body is silently ignored.
  3. Path parameters (`:id` segments, `Left text`).
  Keys that are not valid GraphQL names are silently dropped.
- Resolution against the operation's variable definitions (`resolveVar`):
  - Variable defined but not supplied -> omitted from `variables` (so GraphQL defaults / null
    defaulting apply; a non-null variable without default then fails with the normal GraphQL
    error `expecting a value for non-nullable variable: "<name>"`, code `validation-failed`).
  - Supplied but not defined -> 400 `bad-request` `Unexpected variable <name>`.
  - Body JSON value -> passed through unchanged.
  - Text value (query/path param) for a list-typed variable -> 400 `bad-request`
    `List variables are not currently supported in URL or Query parameters. (Variable <name>, with value "<v>")`.
  - Text value for named type:
    - type `Boolean` and empty string (e.g. `?flag` or `?flag=`) -> `true`;
    - nullable variable and empty string -> omitted (null default);
    - else try JSON-decoding the text: a JSON bool is used when the type is `Boolean` or `Bool`;
      a JSON number is used when the type is `Int`, `Float`, `Number`, `Double`, `float8` or
      `numeric`; otherwise the raw text is used as a JSON string (so `uuid`, `String`, `bigint`,
      `timestamptz` etc. get strings; an `Int` param `abc` becomes `"abc"` and later fails GraphQL
      validation).
- Runtime definition check (verified, `runCustomEndpoint` in `Server/Rest.hs`): the stored
  document must be exactly one **typed** operation definition (`query X {..}` / `mutation ..`);
  anything else at request time (including the shorthand `{ ... }` form) -> 500 `unexpected`
  `A stored query should contain exactly one definition`.
- Execution: builds `{"query": <stored text>, "variables": {...}}` (no operationName) and runs it
  through the normal GraphQL pipeline (`runGQ`), including allowlist check, permissions, query
  tags, logging (http-log `request_mode` single (`RequestModeNonBatchable`)).
- Response (success): HTTP 200, `Content-Type: application/json`, body is the GraphQL `data`
  object **without** the `{"data": ...}` envelope, e.g. `GET /api/rest/author/1` ->
  `{"author_by_pk":{"id":1,"name":"Alice"}}`.
- Response (error): request-level errors (parse/validation/permission/DB errors raised as QErr)
  are returned as a QErr body with its real HTTP status (the REST route uses `encodeQErr` with no
  200-mapping), e.g. missing permission:
  HTTP 400 `{"path":"$","error":"field 'author' not found in type: 'query_root'","code":"validation-failed"}`.
  Execution-level GraphQL errors (`GQExecError`, e.g. errors returned by a remote schema) are
  encoded by `encodeHTTPResp` as a **bare JSON array of error objects** `[{"message":...,...}]`
  (no `data`/`errors` envelope), with HTTP 200 (verified `encodeHTTPResp` in
  `GraphQL/Transport/HTTP/Protocol.hs`; status inferred).
- `@cached` in the stored query: in EE adds `Cache-Control: max-age=<ttl>`; CE: no header.

### 3.4 OpenAPI document

`GET /api/swagger/json` (`server/src-lib/Hasura/Server/OpenAPI.hs`; route in `App.hs`), admin
only (verified `onlyAdmin`: non-admin -> 400 `access-denied` `You have to be an admin to access this endpoint`).
Not gated by `ENABLED_APIS`. Returns an OpenAPI 3.0 JSON document:

- `openapi: "3.0.0"` (inferred), `info.title: "Rest Endpoints"` (verified), `info.version` (empty or ""
  (inferred)), `info.description: "This OpenAPI specification is automatically generated by Hasura."` (verified)
  followed by per-endpoint warnings: `\n\nEndpoint "<name>":\n- ⚠️ <message>` for endpoints with
  problems (e.g. unsupported variable types).
- One path per endpoint URL: `/api/rest/` + segments with `:var` rewritten as `{var}`. One
  operation per method; `summary` = endpoint name (verified), `description` = comment (if any) +
  `"***\nThe GraphQl query for this endpoint is:\n``` graphql\n<query>\n```"`.
- Parameters: header `x-hasura-admin-secret` (description `Your x-hasura-admin-secret will be used for authentication of the API request.`), path parameters for `{var}` (required), and query
  parameters for other scalar variables with description `_"<v>" is required (enter it either in parameters or request body)_` when non-null, or `_"<v>" is required as part of the path_` for path vars.
- Request body (for non-GET methods) JSON schema with all variables; description
  `Query parameters can also be provided in the request body as a JSON object`.
- Scalar mapping (case-insensitive type name): `int` -> integer, `float`/`double` -> number,
  `uuid` -> string with pattern `[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89aAbB][a-f0-9]{3}-[a-f0-9]{12}`,
  `bool`/`boolean` -> boolean, `string`/`id` -> string; other scalars -> reference/declared types.
- Response schema derived from the selection set (`Responses for <METHOD> <url>`).
The golden expected outputs in `server/tests-py/queries/openapi/*.yaml` are the definitive
reference for formatting (ordering, `$ref` naming); implement against them.

---

## 4. API limits (CE-stored, EE-enforced)

`server/src-lib/Hasura/RQL/Types/ApiLimit.hs`. Metadata key `api_limits` (file `metadata/api_limits.yaml`):

```yaml
disabled: false                 # default false
rate_limit:
  global:
    max_reqs_per_min: 100
    unique_params: IP           # "IP" | list of session variables (e.g. [x-hasura-user-id]); optional
  per_role:
    user: {max_reqs_per_min: 10, unique_params: [x-hasura-user-id]}
depth_limit:
  global: 10
  per_role: {user: 5}
node_limit:
  global: 100
  per_role: {}
time_limit:                      # seconds
  global: 20
  per_role: {}
batch_limit:
  global: 5
  per_role: {}
```

- Each limit is `{global: <value>, per_role: {role: value}}`; `per_role` default `{}` (inferred).
- `unique_params`: `"IP"` (case-insensitive on input, e.g. `"ip"`; exported as `"IP"`) or a list
  of session variable names (must have the `x-hasura-` prefix, case-insensitive; the exact string
  `x-hasura-role` is rejected). Any invalid value -> parse error
  `Not a valid value. Should be either: 'IP' or a list of Hasura session variables` (verified,
  `RQL/Types/ApiLimit.hs`). Names are stored as given (not lower-cased).
- All five limit keys and `rate_limit` are optional; `disabled` defaults to `false`;
  `per_role` defaults to `{}`; `global` is required inside each limit; `max_reqs_per_min` and
  depth/node limits must be integers >= 0 (verified codecs).
- Commands: `set_api_limits` (args = whole object above, replaces), `remove_api_limits` (args `{}`).
  Both return `{"message":"success"}`.
- CE behaviour: the `HasResourceLimits` instance for `AppM` returns no-op limits
  (`server/src-lib/Hasura/App.hs`), so nothing is enforced. Rust: store + export; no enforcement.
  (Optional extra later: implement depth/node/batch limits with EE-compatible error messages -
  out of scope for CE parity.)

---

## 5. GraphQL schema introspection options (CE-stored, EE-enforced)

Metadata key `graphql_schema_introspection` (file `metadata/graphql_schema_introspection.yaml`):

```yaml
disabled_for_roles: [user, anonymous]
```

- Command `set_graphql_schema_introspection_options` args `{"disabled_for_roles": [...]}`; response `{"message":"success"}`.
- Export omits the key when the list is empty (inferred, consistent with other empty fields).
- CE: `executeIntrospection` in `AppM` ignores this setting, so introspection works for all
  roles. EE behaviour (for reference): introspection by a listed role fails with code
  `not-supported` message `introspection is disabled for role: "<r>"` (`__typename` still allowed).
  Tests asserting EE behaviour are marked `enterprise_edition`.
- Rust: store/export; CE behaviour. Consider an opt-in flag to enforce (security feature commonly
  requested); if implemented use the EE message.

---

## 6. Query tags (CE-stored, EE-emitted)

Source-level config inside `sources[i].query_tags` (`server/src-lib/Hasura/QueryTags/Types.hs`):

```yaml
query_tags:
  disabled: false              # default false
  format: standard             # standard | sqlcommenter | standard_prepended ; default standard
  omit_request_id: true        # default true
```

- Command `set_query_tags` args `{"source_name": "default", "disabled"?, "format"?, "omit_request_id"?}`.
  Non-supporting backends: `<backend> sources do not support query-tags yet`.
- CE: `MonadQueryTags AppM` `createQueryTags` returns empty, so no comments are added to SQL.
  Rust: store/export; no-op (matches CE). Generated SQL in `/v1/graphql/explain` and
  query-log therefore has no tags in CE.
- EE behaviour (reference for an optional implementation): attributes for queries/mutations
  `request_id` (only if `omit_request_id: false`), `field_name`, `parameterized_query_hash`,
  `operation_name`; for subscriptions `field_name`, `parameterized_query_hash`.
  - `standard`: SQL + ` /* request_id=..., field_name=..., operation_name=... */` (appended,
    `k=v` pairs separated by `, `).
  - `standard_prepended`: same comment before the SQL.
  - `sqlcommenter`: `/* k='v',k2='v2' */` with URL-encoded values in single quotes, keys sorted
    (per Google sqlcommenter spec) (inferred formatting).

---

## 7. Network / TLS allowlist (CE)

Metadata key `network` (file `metadata/network.yaml`)
(`server/lib/hasura-extras/src/Network/Types/Extended.hs`):

```yaml
tls_allowlist:
  - host: internal.example.com
    suffix: "8443"            # optional; despite the name it is the port (string)
    permissions: [self-signed]   # optional; only value "self-signed"
  - other-host.local          # string form = {host: other-host.local}
```

- Object form keys `host` (required), `suffix` (optional), `permissions` (optional; only
  `"self-signed"`; other value -> `TlsPermission expecting one of SelfSigned` (Haskell show)).
  String form: empty string -> `missing "host" field in input`.
- Export: `{"host":..., "suffix":..., "permissions":...}` - all three keys always (null when
  absent) (from the explicit `toJSON`); network key omitted when the list is empty (inferred).
- Commands (`server/src-lib/Hasura/RQL/DDL/Network.hs`):
  - `add_host_to_tls_allowlist` args = a TlsAllow object. Errors: empty host 400 `bad-request`
    `key "host" cannot be empty`; existing host (no suffix): 400 `already-exists`
    `the host "<h>" already exists in the allowlist`; with suffix:
    `the host "<h>" with suffix "<s>" already exists in the allowlist`.
  - `drop_host_from_tls_allowlist` args `{"host": h, "suffix"?: s}`. Errors: empty -> 400
    `bad-request` `hostname cannot be empty`; missing -> 400 `not-exists`
    `the host "<h>" isn't present in the allowlist` / `... with suffix "<s>" isn't present in the allowlist`.
- Effect: for outgoing HTTPS requests (actions, event triggers, scheduled triggers, remote
  schemas, auth webhook (inferred: all requests through the shared HTTP manager)), if the target
  host (and port when `suffix` given) matches an entry, certificate validation failures of kind
  self-signed / unknown CA, hostname mismatch and "leaf not V3" are ignored. Other failures
  (expired) still fail (inferred from `DynamicTlsPermissions.hs` failure list).
- Rust: implement with a custom rustls `ServerCertVerifier` wrapping the default and relaxing
  for allowlisted host:port (priority may).

---

## 8. `metrics_config` (CE-stored, Cloud-used)

Metadata key `metrics_config`:

```yaml
analyze_query_variables: true
analyze_response_body: false
```

Both fields required (inferred: codec `requiredField`). Commands `set_metrics_config` (args =
object) and `remove_metrics_config` (args `{}`). No CE behaviour. Store/export only.

---

## 9. OpenTelemetry (CE-stored/validated, EE-exported)

Metadata key `opentelemetry` (`server/src-lib/Hasura/RQL/Types/OpenTelemetry.hs`):

```yaml
status: enabled                  # enabled | disabled | "{{ENV_VAR}}" ; default disabled
data_types: [traces, metrics, logs]   # default []
exporter_otlp:
  otlp_traces_endpoint: http://otel-collector:4318/v1/traces
  otlp_metrics_endpoint: http://otel-collector:4318/v1/metrics
  otlp_logs_endpoint: http://otel-collector:4318/v1/logs
  protocol: http/protobuf         # only http/protobuf accepted
  headers:                        # list of header confs (value / value_from_env)
    - name: x-api-key
      value_from_env: OTEL_KEY
  resource_attributes:
    - name: service.name
      value: hasura
  traces_propagators: [tracecontext]   # b3 | tracecontext ; default [b3]
batch_span_processor:
  max_export_batch_size: 512      # default 512
```

- Commands: `set_opentelemetry_config` (args = whole object; replaces) and
  `set_opentelemetry_status` (args `"enabled"`/`"disabled"` (inferred: bare string)).
- Validation errors (returned on set, and as inconsistency when loaded): unsupported protocol
  `http/json` or `grpc` rejected; when `status: enabled` and `traces` in `data_types`, a traces
  endpoint is required (inferred); invalid URI rejected; `max_export_batch_size` must be positive.
- CE: config is validated and stored; no exporter runs (OTLP export is EE). CE does propagate
  trace context: incoming `traceparent`/`tracestate` (W3C) and `X-B3-TraceId`, `X-B3-SpanId`,
  `X-B3-ParentSpanId`, `X-B3-Sampled` are parsed and forwarded on outgoing webhook/remote schema/
  action requests (inferred: B3 always, W3C per propagators) and the ids appear in log envelopes
  (doc 11).
- Rust: store/validate; exporting via the `opentelemetry-otlp` crate is a cheap, valuable extra
  (mark as non-CE behaviour, disabled unless `status: enabled`).

---

## 10. Naming conventions (reference)

Full rules are in the schema-generation doc. Config points:

- Server default: `HASURA_GRAPHQL_DEFAULT_NAMING_CONVENTION` (`hasura-default` | `graphql-default`).
- Per source: `sources[i].customization.naming_convention`.
- Older releases required `HASURA_GRAPHQL_EXPERIMENTAL_FEATURES=naming_convention`; current
  source does not.
- Non-Postgres sources with `graphql-default`: error
  `sources other than postgres do not support graphql-default as naming convention yet`.
- `HASURA_FF_NAMING_CONVENTION_SEP_2023` feature flag modifies `graphql-default` behaviour (may).

---

## 11. Apollo Federation (CE, experimental)

`server/src-lib/Hasura/GraphQL/ApolloFederation.hs`, `server/src-lib/Hasura/GraphQL/Schema/Select.hs`,
`server/src-lib/Hasura/Table/API.hs`.

- Enabled when `HASURA_GRAPHQL_ENABLE_APOLLO_FEDERATION=true` (or `--enable-apollo-federation`),
  else when the experimental feature `apollo_federation` is set (`getApolloFederationStatus`:
  explicit option wins over experimental flag).
- Per table: `tables[i].apollo_federation_config: {enable: v1}` (only `"v1"`; otherwise
  `enable takes the version of apollo federation. Supported value is v1 only.`). Settable via
  `pg_track_table`/`track_table` v2 args `apollo_federation_config` or command
  `<backend>_set_apollo_federation_config` (`pg_set_apollo_federation_config`) with args
  `{"source"?: "default", "table": ..., "apollo_federation_config"?: {...}}` (omit/null to disable).
- Schema effects when enabled:
  - Tables with config and a primary key get directive `@key(fields: "<pk cols space-separated>")`
    on their object type (only for the `hasura` schema, not relay).
  - Query root gains `_service: _Service!` with `_Service { sdl: String }` - always when federation
    is enabled.
  - If at least one table has federation config: `_entities(representations: [_Any!]!): [_Entity]!`
    where `_Entity` is a union of those tables' object types and `scalar _Any`.
- `_service.sdl`: the printed SDL of the schema for the requesting role, with schema
  definition (`schema { query: query_root mutation: mutation_root subscription: subscription_root }`)
  and all types, and fields whose names start with `__` removed from object types (so `_service`
  and `_entities` themselves remain? - they don't start with `__`; they remain) (`generateSDL`,
  `RemoveSchemaTypes`). Order of type definitions follows the internal hash map (non-deterministic
  in upstream; tests compare loosely).
- `_entities` resolution: each representation must be an object with string `__typename`
  (errors: `__typename key not found`, `__typename can only be a string value`,
  `<t> is not a valid graphql name`, `representations is expecting a list of objects only`); the
  remaining keys must provide all pk columns (`cannot find <col> in _Any type`); unknown typename
  -> `<t> is not found in selection set or apollo federation is not enabled for the type`. Each
  representation becomes a select-by-pk with the role's permissions; results in input order
  (inferred), null for missing rows (inferred).
- Priority: may.

---

## 12. `@cached` directive (accepted by CE, no effect)

`server/lib/schema-parsers/src/Hasura/GraphQL/Parser/Directives.hs`:

- Custom directive `@cached(ttl: Int = 60, refresh: Boolean = false)` on `QUERY` operations,
  advertised in introspection with description `whether this query should be cached (Hasura Cloud only)`;
  `ttl` description `measured in seconds`, `refresh` description `refresh the cache entry`
  (argument nullability: inferred nullable with default). Must appear in `__schema.directives`
  to match introspection snapshots.
- CE: `cacheLookup` always returns uncached (`server/src-lib/Hasura/App.hs`); responses are
  normal and carry no `Cache-Control`. Using `@cached` on a mutation/subscription -> directive
  location error (standard GraphQL validation: `directive "cached" is not allowed on a mutation`
  (inferred wording; see GraphQL validation doc)).
- Hidden directive `@_multiple_top_level_fields` on subscriptions (internal testing, not
  advertised) allows multiple root fields in a subscription; accept it (priority may).
- Cache-related metadata APIs (`/pro/...`, `clear_query_results_cache` (inferred)) are EE-only; skip.

---

## 13. Source health checks (CE-stored, EE-run)

`server/src-lib/Hasura/RQL/Types/HealthCheck.hs`. Inside `sources[i].health_check`:

```yaml
health_check:
  test:
    sql: SELECT 1        # default "SELECT 1"
  interval: 300          # seconds, required
  retries: 3             # default 3
  retry_interval: 10     # seconds, default 10
  timeout: 10            # seconds, default 10
```

CE: stored and exported only; no checks run (EE emits `health-check-log`). Rust: store/export.

---

## 14. Prometheus metrics (EE-only)

CE: `/v1/metrics` is not served; `metrics` in `ENABLED_APIS` is accepted but the route returns
404 `resource does not exist` (verified: CE `httpApp` in `Server/App.hs` registers no
`v1/metrics` route, so the GET catch-all answers).
`is_prometheus_metrics_enabled` in `/v1alpha1/config`. EE metric names (for an optional extra):
`hasura_http_connections`, `hasura_websocket_connections`, `hasura_active_subscriptions`,
`hasura_graphql_requests_total{operation_type,response_status}`,
`hasura_graphql_execution_time_seconds`, `hasura_event_*`, `hasura_websocket_messages_dropped_total`,
`hasura_websocket_messages_queued_total`, secured by `HASURA_GRAPHQL_METRICS_SECRET` as bearer
(inferred from docs `docs/docs/observability/enterprise-edition/prometheus/metrics.mdx`).
Rust: optional extra; not required for parity.

---

## 15. Automatic persisted queries (EE-only; CE rejects)

`server/src-lib/Hasura/App.hs`:
- `GET /v1/graphql?...` with persisted-query params and `POST` with
  `extensions.persistedQuery` (`EqrAPQReq`) -> 400 `not-supported` `PersistedQueryNotSupported`.
  On `/v1/graphql` encoded as GraphQL error with HTTP 200 (`{"errors":[{"extensions":{"path":"$","code":"not-supported"},"message":"PersistedQueryNotSupported"}]}`) (verified: both GET and POST `/v1/graphql` use `maybeMod200`). Note: in CE **every** `GET /v1/graphql` HTTP request is rejected this way (`runPersistedQueriesGetHandler _ _ _ = throw400 ...`), not only APQ-shaped ones.
- `HASURA_GRAPHQL_ENABLE_PERSISTED_QUERIES` has no effect in CE.

---

## 16. Event trigger log cleanup (EE-only; CE errors)

Event trigger `cleanup_config` and command `cleanup_event_trigger_logs`: CE returns 400
`not-supported` `Event log cleanup feature is enterprise edition only` (verified,
`MonadEventLogCleanup AppM` `runLogCleaner` in `App.hs`). `pause_event_trigger_cleanups` /
`resume_event_trigger_cleanups` are NOT blocked in CE: they go through
`toggleEventTriggerCleanupAction` (`RQL/DDL/EventTrigger.hs`), which only flips the `paused`
flag of `cleanup_config` in metadata (errors: unknown source -> 400 `not-exists`
`source with name <s> does not exists`); CE's cleanup scheduler hooks are no-ops.
Metadata containing `cleanup_config` on an event trigger: accepted and stored by CE (inferred) -
must round-trip. See triggers doc.

---

## 17. Inherited roles (reference)

`add_inherited_role` / `drop_inherited_role` and metadata key `inherited_roles` are CE features
(always on now; the `inherited_roles` experimental flag is obsolete). Semantics belong to the
permissions doc.

---

## Upstream tests to reuse

- `server/tests-py/test_allowlist_queries.py` + `server/tests-py/queries/graphql_query/allowlist/` - allowlist enforcement with `HASURA_GRAPHQL_ENABLE_ALLOWLIST=true` (global collections, `__typename` normalization, rejection message), and metadata API for collections/allowlist. Classes testing role-based scopes are EE (`enterprise_edition`/`pro` markers) - skip.
- `server/tests-py/test_metadata.py` (allowlist/collection/endpoint cases) + `server/tests-py/queries/v1/metadata/` - export/replace round-trips that include `query_collections`, `allowlist`, `rest_endpoints`, `api_limits`, `network`, `metrics_config`, `inherited_roles`.
- `server/tests-py/test_endpoints.py` + `server/tests-py/queries/endpoints/` (26 fixtures) - REST endpoint creation, inconsistencies (ambiguous/duplicate/empty segments), variable handling (query/body/path, booleans, numbers, lists), method errors (404/405), auth. Some classes use `@cached` with EE caching headers - skip those (look for `query_cache`/`pro` markers).
- `server/tests-py/test_openapi.py` + `server/tests-py/queries/openapi/` (17 fixtures) - golden OpenAPI JSON for `/api/swagger/json`.
- `server/tests-py/test_apollo_federation.py` + `server/tests-py/queries/apollo_federation/` (`root_fields.yaml`, `entities.yaml`) - `_service`/`_entities` with `EXPERIMENTAL_FEATURES=apollo_federation`; the federated-gateway tests need a Node Apollo gateway (skip unless available).
- `server/tests-py/test_naming_conventions.py` + `server/tests-py/queries/naming_conventions/` - naming convention behaviour.
- `server/tests-py/test_graphql_introspection.py` + `server/tests-py/queries/graphql_introspection/` - introspection (includes `@cached` in directives); the "disable introspection for role" classes are EE - skip.
- `server/tests-py/test_query_cache.py` + `queries/query_cache/` - EE caching with Redis: skip.
- `server/tests-py/test_webhook_insecure.py` - TLS behaviour for insecure webhooks (relates to TLS allowlist).
- `server/tests-py/queries/graphql_query/allowlist/` (global) and `server/tests-py/queries/graphql_query/allowlist_role_based/` (role-based; EE enforcement - in CE these queries must be **rejected**, so only reuse the metadata-API parts).
- `server/lib/api-tests/src/Test/NamingConventionsSpec.hs` - naming conventions against an external engine process (Postgres).
- `server/lib/api-tests/src/Test/API/Metadata/InconsistentSpec.hs` - inconsistent-object reporting format (useful for endpoint/collection inconsistencies).
- Note: the Haskell api-tests contain no specs for allowlist, REST endpoints, API limits, TLS allowlist, OpenTelemetry, Apollo Federation or `@cached` (verified by grep); tests-py is the only golden source for these.
