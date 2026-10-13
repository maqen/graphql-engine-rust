# 08 — Remote Schemas, Remote Relationships and Data Federation

Status: research spec for the Rust drop-in reimplementation of Hasura GraphQL Engine V2.
Ground truth: Haskell source under `server/src-lib/Hasura/...` (paths below are relative to the
upstream repo root). Docs (`docs/docs/...`) are secondary; disagreements are called out.
Anything not verified directly in source is marked **(inferred)**.

Primary upstream modules for this area:

| Concern | Upstream file(s) |
|---|---|
| Remote schema metadata types | `server/src-lib/Hasura/RemoteSchema/Metadata/{Core,Customization,Permission,RemoteRelationship,Base}.hs` |
| Remote schema metadata API | `server/src-lib/Hasura/RemoteSchema/MetadataAPI/{Core,Permission}.hs` |
| Schema-cache build for remote schemas | `server/src-lib/Hasura/RemoteSchema/SchemaCache/{Build,Types,Permission,RemoteRelationship}.hs` |
| Introspection fetch, customization, HTTP execution | `server/src-lib/Hasura/GraphQL/RemoteServer.hs` |
| GraphQL parsers for remote schemas | `server/src-lib/Hasura/GraphQL/Schema/Remote.hs` |
| Remote relationship parsers | `server/src-lib/Hasura/GraphQL/Schema/RemoteRelationship.hs` |
| Root-field merge / conflict detection | `server/src-lib/Hasura/GraphQL/Schema.hs` (`buildAndValidateRemoteSchemas`) |
| Outgoing request IR + printer | `server/src-lib/Hasura/RQL/IR/RemoteSchema.hs`, `server/lib/graphql-parser/src/Language/GraphQL/Draft/Printer.hs` |
| Executing a remote root field | `server/src-lib/Hasura/GraphQL/Execute/Remote.hs`, `server/src-lib/Hasura/GraphQL/Transport/HTTP.hs` (`runRemoteGQ`, `extractFieldFromResponse`) |
| Remote relationship metadata | `server/src-lib/Hasura/RQL/Types/Relationships/{Remote,ToSource}.hs`, `server/src-lib/Hasura/RQL/DDL/RemoteRelationship.hs` |
| Remote join execution | `server/src-lib/Hasura/GraphQL/Execute/RemoteJoin/{Collect,Join,RemoteSchema,Source,Types}.hs` |
| DB-side join SQL | `server/src-lib/Hasura/Backends/Postgres/Instances/Execute.hs` (`pgDBRemoteRelationshipPlan`), `server/src-lib/Hasura/GraphQL/Execute/Backend.hs` (`convertRemoteSourceRelationship`) |
| Result customization (`__typename` rewriting) | `server/src-lib/Hasura/RQL/Types/ResultCustomization.hs` |
| Remote schema healing thread | `server/src-lib/Hasura/Server/RemoteSchemaHealing.hs` |

---

## Scope and priority

**Essential (P0 — typical V2 projects use these):**

- `remote_schemas` metadata (`remote_schemas.yaml`): `url` / `url_from_env` (incl. `{{ENV}}` URL
  templates), `headers` (`value` / `value_from_env`), `forward_client_headers`, `timeout_seconds`.
- Introspection of the remote at metadata load; failure -> inconsistent object `remote_schema`.
- Exposing remote root fields (query + mutation) in `query_root` / `mutation_root`, conflict
  detection vs. DB root fields and other remotes.
- Forwarding a query to the remote: one HTTP request per remote root field, session variables as
  `x-hasura-*` headers, configured headers, optional client headers, error passthrough.
- Metadata API: `add_remote_schema`, `update_remote_schema`, `remove_remote_schema`,
  `reload_remote_schema`, `introspect_remote_schema` (the Console calls this).
- Remote relationships **table -> remote schema** (`to_remote_schema`, and the legacy
  `remote_schema`/`hasura_fields`/`remote_field` format which is still very common in exported
  metadata), and **table -> table across sources** (`to_source`), incl. `pg_create_remote_relationship`
  / `pg_update_remote_relationship` / `pg_delete_remote_relationship` and their `/v1/query` legacy
  (unprefixed) forms.
- Remote join execution (batching, nulls, nested joins, phantom columns).

**Commonly used (P1):**

- Customization: `root_fields_namespace`, `type_names` (prefix/suffix/mapping),
  `field_names` (per parent type prefix/suffix/mapping) incl. `__typename` value rewriting.
- Remote schema permissions (`HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS=true`),
  `add_remote_schema_permissions` / `drop_remote_schema_permissions`, `@preset` (static and
  session-variable presets), inherited roles.
- `introspection_headers` (newer field; present in this source tree).
- Remote schema -> table and remote schema -> remote schema relationships
  (`create_remote_schema_remote_relationship` etc.).

**Rarely used / quirks (P2):**

- `HASURA_GRAPHQL_REMOTE_SCHEMA_SKIP_NULLS`, `HASURA_GRAPHQL_REMOTE_SCHEMA_PRIORITIZE_DATA`.
- Remote schema healing thread (`HASURA_GRAPHQL_REMOTE_SCHEMA_HEALING_INTERVAL`) — present in this
  source tree; may be newer than some deployed V2 versions.
- Exact re-printing of permission SDL in `export_metadata`.

**Enterprise / Cloud only (skip for CE):**

- Stored-introspection fallback (if the remote is down at startup, EE/Cloud uses the last stored
  introspection and records a "stale introspection" inconsistency). In OSS
  `fetchSourceIntrospection` always returns `Nothing` (`server/src-lib/Hasura/App.hs:861`), so the
  remote is simply inconsistent.
- Remote relationships to non-Postgres targets (MSSQL, BigQuery, Data Connector) — out of scope
  for the Postgres-first port, but the design must keep the "source join" abstraction generic.
- Remote schema caching (`@cached` on remote fields is a Cloud/EE response-cache concern).

**Not supported by upstream at all (do not implement):** remote schema subscriptions (exposed in
schema but rejected at execution), remote schema fields over Relay (`/v1beta1/relay`), remote
relationships in Relay, remote relationships *to* remote-schema fields that return lists at an
intermediate `field` hop, enum literal values in remote relationship argument templates.

---

## 1. Data model (metadata)

### 1.1 `remote_schemas.yaml` / `metadata.remote_schemas[]`

Each entry is a `RemoteSchemaMetadata` (`server/src-lib/Hasura/RemoteSchema/Metadata/Core.hs`):

```yaml
- name: my-remote            # RemoteSchemaName: any non-empty text (no charset validation!)
  definition:                # RemoteSchemaDef
    url: https://example.com/graphql        # or url_from_env (exactly one of the two)
    # url_from_env: REMOTE_URL
    headers:                 # optional; list of HeaderConf
      - name: X-Api-Key
        value_from_env: API_KEY
      - name: X-Static
        value: abc
    introspection_headers:   # optional; see 1.2
      - name: X-Introspect
        value: yes
    forward_client_headers: true   # optional, default false
    timeout_seconds: 60            # optional, default 60
    customization:                 # optional; see §4
      root_fields_namespace: foo
      type_names:
        prefix: Foo_
        suffix: _x
        mapping: { Droid: Android }
      field_names:
        - parent_type: Character
          prefix: foo_
          suffix: _f
          mapping: { id: ident }
  comment: optional text
  permissions:               # optional, default []; see §7
    - role: user
      definition:
        schema: |
          type Query { hello: String }
      comment: optional
  remote_relationships:      # optional, default []; see §8.3
    - type_name: Writer      # a type in the ORIGINAL (un-customized) remote schema
      relationships:
        - name: articles
          definition:
            to_remote_schema: { ... }   # or to_source: { ... }
```

Field-level rules (`RemoteSchemaDef` FromJSON):

| key | type | required | default | notes |
|---|---|---|---|---|
| `url` | `InputWebhook` (URL template string) | one of url/url_from_env | — | Supports `{{ENV_VAR}}` templating (same template engine as action handlers / webhooks). Resolved at schema-cache build via `resolveWebhook`: missing env var -> `400 unexpected` `Value for environment variables not found: <err>` (`RQL/Types/Common.hs:359`); result must parse as an absolute URI else `400 invalid-params` `not a valid URI generated from the template: <template>` |
| `url_from_env` | text | one of url/url_from_env | — | Env var must be set (`400 invalid-params` `environment variable '<X>' not set`) and parse as URI (`not a valid URI in environment variable: <X>`) |
| `headers` | `[HeaderConf]` | no | `[]` | `HeaderConf` = `{name, value}` or `{name, value_from_env}` (shared with actions/events; see headers spec). `value` is itself a `{{ENV}}` template. Resolved per request by `makeHeadersFromConf` (`RQL/DDL/Headers.hs:20-35`): missing `value_from_env` var -> `400 not-found` `environment variable 'X' not set`; unresolvable template -> `400 not-found` `template cannot be resolved: <e>` |
| `introspection_headers` | `[HeaderConf]` or `null` | no | absent | `null` is treated as absent |
| `forward_client_headers` | bool | no | `false` | |
| `timeout_seconds` | int | no | `60` | HTTP *response* timeout (`responseTimeoutMicro (timeout*1e6)`) |
| `customization` | object | no | absent | see §4 |

Validation errors (`validateRemoteSchemaDef`, `server/src-lib/Hasura/RemoteSchema/SchemaCache/Types.hs`):

- both missing: `400 invalid-params` — `both `+"`url`"+` and `+"`url_from_env`"+` can't be empty` (literal text: ``both `url` and `url_from_env` can't be empty``)
- both present: ``both `url` and `url_from_env` can't be present``
- field-name customization mapping keys starting with `__`: `400 invalid-params` `attempt to customize reserved field name "<name>"`.

`EnvRecord` naming used in error messages: for `url` the "name" is the **template text** (e.g.
`{{REMOTE_SCHEMAS_WEBHOOK_DOMAIN}}/hello-graphql`); for `url_from_env` it is the **env var name**.
URLs are never echoed resolved (secret hygiene).

### 1.2 `introspection_headers`

- Omitted (or `null`): introspection uses `headers` (backward compatible).
- Present (even `[]`): introspection uses **only** this list; `[]` sends no configured headers.
- Runtime operations always use `headers` (+ session vars + forwarded client headers).
- Introspection **never** sends session variables or client headers
  (`execRemoteGQForIntrospection`).

### 1.3 Export format (`export_metadata`)

`remoteSchemasToOrdJSONList` (`server/src-lib/Hasura/RQL/Types/Metadata/Serialization.hs:383`):

- list sorted by `name`.
- entry keys in order: `name`, `definition`, then `comment` (only if non-null), `permissions`
  (only if non-empty), `remote_relationships` (only if non-empty).
- `definition` keys in order: `url`, `url_from_env`, `timeout_seconds`, `customization`,
  `headers` (omitted when absent **or empty**), `introspection_headers` (emitted whenever present,
  even `[]`), `forward_client_headers` (emitted **only when true**).
- permission entries: `role`, `definition: {schema: <re-printed SDL>}`, `comment` if present.
  The SDL is **re-printed** from the parsed AST by graphql-parser's `schemaDocument` printer
  (types sorted, fields sorted, see §7.6) — not the text originally submitted.
- remote relationship entries: `{type_name, relationships: [{name, definition}]}`; definition
  keeps the format it was read in (§8.1).

---

## 2. Metadata API

All are `POST /v1/metadata` commands (backend-agnostic, no prefix). `add_remote_schema`,
`update_remote_schema`, `remove_remote_schema`, `reload_remote_schema`, `introspect_remote_schema`
are also accepted by legacy `POST /v1/query` (`server/src-lib/Hasura/Server/API/Query.hs`).
All successful mutations return `{"message":"success"}`.

### 2.1 `add_remote_schema`

```json
{"type":"add_remote_schema","args":{"name":"r","definition":{...RemoteSchemaDef},"comment":"opt"}}
```

Algorithm (`runAddRemoteSchema`):

1. If a remote schema with that name exists (consistent **or** inconsistent —
   `getAllRemoteSchemas` includes inconsistent ones) -> `400 already-exists`
   `remote schema with name "r" already exists`.
2. **Synchronously** validate the definition and fetch + validate introspection (§3). Any failure
   is thrown directly (not recorded as inconsistency): typically `400 remote-schema-error`.
3. Insert `RemoteSchemaMetadata name defn comment [] []` and rebuild the schema cache for
   `MORemoteSchema name` (`buildSchemaCacheFor`):
   - if the new remote schema itself becomes inconsistent (e.g. root field conflicts, type
     conflicts) -> `400 invalid-configuration`, `error` = comma-separated reasons, e.g.
     `Inconsistent object: Field cannot be overwritten by remote field 'hello'`, `internal` = the
     inconsistent objects list.
   - if **another** object becomes inconsistent -> `400 unexpected`
     `cannot continue due to new inconsistent metadata`.
   NOTE: which of two conflicting remotes gets blamed depends on HashMap iteration order in
   upstream (tests only assert the generic code/message; see `test_add_schema_conflicts`). In
   Rust iterate remotes in a deterministic order (metadata order) — the newly added one last — so
   the new remote is the one reported.

### 2.2 `update_remote_schema`

Same args as add. Algorithm (`runUpdateRemoteSchema`):

1. Must exist -> else `400 not-exists` `remote schema with name "r" doesn't exist`.
2. Validate definition (`validateRemoteSchemaDef`).
3. Re-introspect **only if the URL changed**: skipped when (`url` old == new, both present) or
   (`url_from_env` old == new, both present). Errors thrown synchronously.
4. Replace the metadata entry with `RemoteSchemaMetadata name defn comment <old permissions> []`.
   **Quirk:** existing remote-schema-to-X relationships (`remote_relationships` of the remote
   schema) are **dropped** by update (they are reset to `mempty`). Replicate for compatibility.
   Permissions are preserved and re-validated against the new schema.
5. Rebuild with `withNewInconsistentObjsCheck` + `buildSchemaCacheFor (MORemoteSchema name)`.
   `buildSchemaCacheFor` (`RQL/Types/SchemaCache/Build.hs:430`) throws first, so the observable
   errors are the same as for add: the remote schema itself inconsistent (even if it already was)
   -> `400 invalid-configuration` with the reasons; any other new inconsistency (e.g. a
   `remote_schema_permission` no longer valid against the new schema) -> `400 unexpected`
   `cannot continue due to new inconsistent metadata` (internal = the inconsistent objects). The
   outer `withNewInconsistentObjsCheck` (`500 unexpected` `cannot continue due to newly found
   inconsistent metadata`) is effectively unreachable here. tests-py
   `test_update_remote_schema_details_with_permissions_set_with_error` expects **400**.

Note: because add/update always run step 2 with a fresh fetch, a customization change with the
same URL is still validated via the schema rebuild (e.g. test expects
`Inconsistent object: Type name mappings are not distinct; the following types appear more than once: "Foo"`).

### 2.3 `remove_remote_schema`

```json
{"type":"remove_remote_schema","args":{"name":"r","cascade":false}}
```

- `cascade` optional (default false). (Docs only document `name`.)
- Looks up **consistent** remote schemas only (`scRemoteSchemas`): an inconsistent remote schema
  gives `400 not-exists` `no such remote schema` (user must use `drop_inconsistent_metadata`).
- Dependents: all schema objects depending on `SORemoteSchema r` except its own remote schema
  permissions. If non-empty and not cascade: `400 dependency-error`
  `cannot drop due to the following dependent objects: remote relationship custom.profiles.messageBasic, ...`
  (format of each item per `reportSchemaObj`: `remote relationship <table>.<rel>`,
  `remote schema <name>`, `remote schema permission <name>.<role>` (inferred formatting)).
- With cascade: dependents purged (`purgeSourceAndSchemaDependencies`), then the remote schema
  entry removed (its permissions and its own remote relationships go with it).
- The rebuild is a full `buildSchemaCache` wrapped in `withNewInconsistentObjsCheck`: if removal
  makes any other object newly inconsistent -> `500 unexpected`
  `cannot continue due to newly found inconsistent metadata` (internal = the new inconsistent
  objects) and nothing is changed (inferred: metadata API errors roll back the change).

### 2.4 `reload_remote_schema`

```json
{"type":"reload_remote_schema","args":{"name":"r"}}
```

- Name must be in consistent ∪ inconsistent remote schemas -> else `400 not-exists`
  `remote schema with name "r" does not exist`.
- Bumps the invalidation key for that remote (`ciRemoteSchemas`) and rebuilds; the remote is
  re-introspected. Only this remote is re-fetched; others use cached introspection.
- Wrapped in `withNewInconsistentObjsCheck`: if a *previously consistent* remote now fails ->
  `500 unexpected` `cannot continue due to newly found inconsistent metadata`. If it was already
  inconsistent and still fails -> success (no *new* inconsistency).
- In upstream the invalidation key is part of the metadata resource version so other replicas
  also reload (inferred from `CatalogUpdate` path).

`reload_metadata` with `reload_remote_schemas: true|[names]` also re-introspects (covered by the
metadata spec; the semantics equal reload_remote_schema for each listed name).

### 2.5 `introspect_remote_schema`

```json
{"type":"introspect_remote_schema","args":{"name":"r"}}
```

- Consistent remote schemas only -> else `400 not-exists` `remote schema: "r" not found`.
- Returns the **raw bytes** of the remote server's response to the introspection query, verbatim
  (`_rscRawIntrospectionResult`), i.e. `{"data":{"__schema":{...}}}` exactly as the remote sent
  it (including any extra keys, original — un-customized — names). Not admin-only beyond the
  normal metadata API auth. Read-only (allowed in read-only mode).

### 2.6 `add_remote_schema_permissions` / `drop_remote_schema_permissions` (see §7)

```json
{"type":"add_remote_schema_permissions","args":{"remote_schema":"r","role":"user",
  "definition":{"schema":"type Query { hello: String }"},"comment":"opt"}}
{"type":"drop_remote_schema_permissions","args":{"remote_schema":"r","role":"user"}}
```

### 2.7 Remote relationship commands (see §8.2)

`<backend>_create_remote_relationship`, `<backend>_update_remote_relationship`,
`<backend>_delete_remote_relationship` (`pg_`, `citus_`, `cockroach_`, `mssql_`, `bigquery_`, ...
prefix required on `/v1/metadata`; unprefixed `create_remote_relationship` etc. are accepted on
`/v1/query` for Postgres `default` source), plus `create_remote_schema_remote_relationship`,
`update_remote_schema_remote_relationship`, `delete_remote_schema_remote_relationship`.

---

## 3. Introspection at metadata load

### 3.1 When

- On every full schema-cache build (startup, `reload_metadata`, `replace_metadata`, any metadata
  change that touches the remote schema). The result is cached keyed on the remote schema's
  metadata (incl. definition) plus its invalidation key: unchanged remotes are **not**
  re-fetched on unrelated metadata changes (`Inc.cache` in `buildRemoteSchemas`).
- On `add_remote_schema`, `update_remote_schema` (if URL changed), `reload_remote_schema`.
- By the healing thread (§3.6).

### 3.2 Request

`POST <url>` with body (from `server/src-rsr/introspection.json`, sent as JSON):

```json
{"query":"query IntrospectionQuery {\n  __schema {\n    queryType {\n      name\n    }\n    mutationType {\n      name\n    }\n    subscriptionType {\n      name\n    }\n    types {\n      ...FullType\n    }\n    directives {\n      name\n      description\n      locations\n      args {\n        ...InputValue\n      }\n    }\n  }\n}\n\nfragment FullType on __Type {\n  kind\n  name\n  description\n  fields(includeDeprecated: true) {\n    name\n    description\n    args {\n      ...InputValue\n    }\n    type {\n      ...TypeRef\n    }\n    isDeprecated\n    deprecationReason\n  }\n  inputFields {\n    ...InputValue\n  }\n  interfaces {\n    ...TypeRef\n  }\n  enumValues(includeDeprecated: true) {\n    name\n    description\n    isDeprecated\n    deprecationReason\n  }\n  possibleTypes {\n    ...TypeRef\n  }\n}\n\nfragment InputValue on __InputValue {\n  name\n  description\n  type {\n    ...TypeRef\n  }\n  defaultValue\n}\n\nfragment TypeRef on __Type {\n  kind\n  name\n  ofType {\n    kind\n    name\n    ofType {\n      kind\n      name\n      ofType {\n        kind\n        name\n        ofType {\n          kind\n          name\n          ofType {\n            kind\n            name\n            ofType {\n              kind\n              name\n              ofType {\n                kind\n                name\n              }\n            }\n          }\n        }\n      }\n    }\n  }\n}\n","variables":null,"operationName":"IntrospectionQuery"}
```

Important: upstream parses this file and re-renders it with its printer before sending
(`introspectionQuery` is a parsed `GQLReqOutgoing`; `renderGQLReqOutgoing` prints it, inlining the
fragments). So the *actual wire text* is the single-line printer output (§6.6), with fragments
inlined, e.g. `query IntrospectionQuery { __schema { queryType { name  } ...`. Remote servers do
not care; test remote servers in tests-py don't assert on it. Recommendation: send the
pre-rendered single operation (fragments inlined) — any valid equivalent query is acceptable.
**Wire JSON envelope (verified):** the file's `"variables": null` is decoded into `Nothing`, and
`GQLReq`'s `ToJSON` uses `omitNothingFields = True` with generic field order
(`server/src-lib/Hasura/GraphQL/Transport/HTTP/Protocol.hs:68-80`), so the body actually sent is
`{"operationName":"IntrospectionQuery","query":"query IntrospectionQuery { __schema { ... } }"}` —
**no `variables` key** and keys in the order `operationName`, `query`, `variables`.
Note the `TypeRef` depth is 8 levels (`ofType` nested 7 times); deeper wrapping is truncated.

Headers: `Content-Type: application/json`, `User-Agent: hasura-graphql-engine/<version>`
(`addDefaultHeaders`, these two override any same-named configured header), plus
`introspection_headers` or `headers` resolved from env. No session vars, no client headers.
Timeout: `timeout_seconds`. HTTP status of the response is **not checked** — only the body is
parsed.

### 3.3 Parsing the introspection result (`FromIntrospection`, `RemoteServer.hs`)

Body must be JSON with `data.__schema`:

- `queryType.name` required; `mutationType`, `subscriptionType` optional (null ok).
- `types[]`: each `{kind, name, description?}`:
  - `SCALAR` -> scalar.
  - `OBJECT` -> `fields` (optional, absent = `[]`), `interfaces` (optional; each must be an
    interface ref — parsed as type definitions with `kind:"INTERFACE"`; any other kind ->
    `Error: object type X can only implement interfaces`).
  - `INTERFACE` -> `fields` (optional), `possibleTypes` (optional; must be `OBJECT`s else
    `Error: interface type X can only be implemented by objects`). Interfaces implementing
    interfaces are ignored.
  - `UNION` -> `possibleTypes` **required**; must be objects (`Error: union type X can only be
    implemented by objects`).
  - `ENUM` -> `enumValues` required; only `name`,`description` kept (deprecation dropped).
  - `INPUT_OBJECT` -> `inputFields` optional.
  - other kind -> `unknown kind: <kind>`.
- Field: `name`, `description?`, `args` (required), `type`. `isDeprecated`/`deprecationReason`
  are ignored (Hasura's schema never re-exposes deprecation for remote fields — inferred from
  "TODO add directives, deprecation" in `Schema/Remote.hs`).
- Type refs: `NON_NULL` requires `ofType`; `LIST` requires `ofType`; named kinds require `name`.
- `defaultValue` is a string containing a GraphQL **literal** and is parsed with the GraphQL
  value parser (`G.runParser G.value`); parse failure fails the whole introspection.
- `directives` are fetched but ignored entirely.
- Types are keyed by name (duplicates: last wins, inferred from `HashMap.fromListOn`).

Parse errors become `400 remote-schema-error` with the Aeson error text, e.g.
`Error in $.types[1].possibleTypes[0].name: parsing Name failed, expected String, but encountered Null`.
(Exact Aeson wording can't be reproduced byte-for-byte; tests asserting these strings will need
golden overrides. Keep the `Error in $.<json path>: ...` shape.)

`addDefaultRoots`: if `mutationType`/`subscriptionType` is null but an OBJECT type literally named
`Mutation` / `Subscription` exists, it is treated as the mutation / subscription root.

### 3.4 Validation performed at load (`stitchRemoteSchema`)

1. Build the customizer (§4) and validate it:
   - interface/implementation field-name consistency;
   - customized type names distinct; customized field names distinct per object/interface.
2. Build the full remote parser once as admin (`buildRemoteParser` with no relationships), which
   checks (all `400 remote-schema-error`):
   - root type exists and is an object: `"<Root>" has to be an object type` (code `unexpected`).
   - every referenced type exists: `Could not find type with name "X"`.
   - input positions reference input types: `expected input type, but got output type`; output
     positions: `expected output type, but got input type`.
   - interfaces non-empty: `List of fields cannot be empty for interface 'Character'`.
   - unions non-empty: `List of member types cannot be empty for union type 'U'`.
   - union/interface members are objects: `Could not find type 'X', which is defined as a member type of Union 'U'`,
     `Union type 'U' can only include object types. It cannot include 'I'` (same for Interface).
   - objects implementing interfaces: `Could not find interface 'I' implemented by Object type 'O'`;
     interface field present: `Interface field 'I'."f" expected, but 'O' does not provide it`;
     type covariance: `The type of Object field 'O'."f" (T1) is not the same type/sub type of Interface field 'I'."f" (T2)`
     (non-null may upgrade nullable; object may implement interface type; any object accepted for union type);
     interface args present with identical type:
     `Interface field argument 'I'."f"("a":) required, but Object field 'O'."f" does not provide it`,
     `Interface field argument 'I'."f"("a":) expects type T, but 'O'."f"("a":) has type T2`;
     extra object args must be nullable:
     `Object field argument 'O'."f"("a":) is of required type T!, but is not provided by Interface field 'I'."f"`.
   (Quoting: `squote` -> `'x'`, `dquote` -> `"x"`.)

### 3.5 Failure handling

- During a full build: failure of a remote schema records an **inconsistent object**
  `{type: "remote_schema", name: "remote_schema <name>", definition: <RemoteSchemaMetadata JSON>, reason: "Inconsistent object: <error message>"}`
  and the remote (and everything depending on it: its permissions, remote relationships targeting
  it, relationships defined on it) is excluded. The server still starts. Visible via
  `get_inconsistent_metadata`, `reload_metadata` response `is_consistent: false`.
- Dependents of an inconsistent remote become inconsistent too, e.g. a table remote relationship:
  `in table "custom.profiles": in remote relationship "messageBasic": remote schema with name "r" not found`.
- HTTP failures: `400 remote-schema-error`,
  message `HTTP exception occurred while sending the request to "<EnvRecord name>"`
  (name quoted via `tshow`), `internal` =
  `{"type":"http_exception","message":<text e.g. "Response timeout">,"request":{...},"status"?:<int>}`
  or `{"type":"invalid_url","message":...}`.

### 3.6 Remote schema healing (newer feature; P2)

Background thread (`server/src-lib/Hasura/Server/RemoteSchemaHealing.hs`), enabled by default:

- `HASURA_GRAPHQL_REMOTE_SCHEMA_HEALING_INTERVAL` (ms, default `5000`; `0`/skip disables — see
  `Config.OptionalInterval`).
- Every interval: for each remote schema whose **own** inconsistency is an `InconsistentObject` on
  `MORemoteSchema` (not conflicts), if due: probe (fetch + `stitchRemoteSchema`) without holding the
  schema-cache lock; if any probe succeeds, rebuild the schema cache locally invalidating only
  those remotes (does **not** bump the metadata resource version — each replica heals itself).
- Per-remote exponential backoff starting at the interval, doubling, capped at 120 s; reset when
  the remote's metadata definition changes; never gives up; logs a warning once at cap.

---

## 4. Customization

`RemoteSchemaCustomization` (`Metadata/Customization.hs`), JSON keys:

```json
{
  "root_fields_namespace": "Name",          // optional GraphQL name
  "type_names": {                           // optional
    "prefix": "Name", "suffix": "Name",     // optional
    "mapping": {"Orig": "New"}              // optional in FromJSON (default {}); docs/codec say required
  },
  "field_names": [                          // optional
    {"parent_type": "Orig", "prefix": "Name", "suffix": "Name", "mapping": {"orig": "new"}}
  ]
}
```

All names must be valid GraphQL names (prefix/suffix are parsed as `G.Name`, so e.g. `foo_` is ok,
`1foo` is not). All keys refer to **original** remote names.

### 4.1 Building the customizer (`getCustomizer`, `RemoteServer.hs`)

Let `roots = {queryRoot, mutationRoot?, subscriptionRoot?}` (after `addDefaultRoots`).

**Type rename map** (orig -> new):

```
protected = builtInScalars {Boolean, Float, ID, Int, String}
            ∪ (roots if root_fields_namespace is NOT set else ∅)
typesToRename = all type names except names starting with "__" and except `protected`
rootTypeNameMap = if namespace N set: {root -> N <> root  for each root}   // e.g. Query -> fooQuery
                  else {}
typeRenameMap = case type_names of
   absent -> rootTypeNameMap
   present -> mapping  ⊎  rootTypeNameMap  ⊎  prefixSuffix(prefix, suffix, typesToRename)
```

`⊎` is **left-biased** union: explicit `mapping` beats the namespace-derived root rename, which
beats prefix/suffix. `prefixSuffix(p,s,names) = {n -> p<>n<>s}` (only p, only s, or none).
Consequences:

- Without a namespace, root types (`Query`, `Mutation`, ...) are never prefixed/suffixed (they are
  hidden behind Hasura's `query_root` anyway) unless explicitly in `mapping`.
- With namespace `foo` and prefix `Bar`: `Query -> fooQuery` (namespace wins over prefix), other
  types `T -> BarT`.
- Built-in scalars are never prefixed/suffixed but **can** be renamed via `mapping` (test maps
  `Int: MyInt`; the customized scalar then appears in the schema as `MyInt` and variables must be
  declared `MyInt!` — `variable 'ep' is declared as 'Int!', but used where 'MyInt!' is expected`).

**Field rename map** (per parent type, orig -> new): for each `field_names` entry whose
`parent_type` is an existing object or interface: `mapping ⊎ prefixSuffix(prefix, suffix, all field names of that type)`.
Entries for unknown/other-kind types are ignored. Field customization applies to object and
interface fields only (not input-object fields, not arguments, not enum values). Entries with the
same `parent_type` -> last wins (HashMap.fromList) **(inferred)**.

`__typename` and other `__` fields are never renamed (reserved-name mapping keys are rejected,
prefix/suffix does not apply to them since they are not in the type's field list).

### 4.2 Validation at load (all `400 remote-schema-error`)

- Interface consistency: for every interface I, every possible type T, every field f of I:
  `custom(I,f) == custom(T,f)` else
  `Remote schema customization inconsistency: field name mapping for field "f" of interface "I" is inconsistent with mapping for type "T". Interface field name maps to "x". Type field name maps to "y".`
- Distinct types: `Type name mappings are not distinct; the following types appear more than once: "Foo"` (dquoted, comma-separated list).
- Distinct fields: `Field name mappings for object type "T" are not distinct; the following fields appear more than once: "a"` (and the `interface type` variant).

### 4.3 Where customizations apply

- **Hasura schema**: all remote types appear under customized names; fields under customized
  names; descriptions preserved. Remote relationship generated input types
  (`*_remote_rel_*`, §8.4) are not in the rename map (inferred: they are added after the map is
  computed, so they keep their generated names).
- **Root fields**: with `root_fields_namespace: ns`, all remote query root fields are nested under
  a single field `ns` in `query_root` (type = customized query root type name, e.g. `nsQuery`), and
  likewise mutation fields under `ns` in `mutation_root` (type e.g. `nsMutation`). If the remote has
  no fields for that operation type, no namespace field is added. The namespace field is
  **nullable** (verified: `customizeNamespace`, `GraphQL/Namespace.hs:96`, uses
  `P.subselection_` over `P.selectionSet`, whose type is `TNamed Nullable`,
  `lib/schema-parsers/src/Hasura/GraphQL/Parser/Internal/Parser.hs:264`).
- **Outgoing query**: always uses *original* names. A customized field selected without an alias
  is sent with the customized name as alias (`my_hello: hello(arg: "me")`) so the response keys
  come back customized. Aliased fields keep the user alias. Variables types are rewritten back to
  original type names (`mkRemoteGType`). Inline-fragment type conditions use original type names.
- **Response**: `__typename` values are rewritten original -> customized by the result customizer
  (§6.5), through all nesting, arrays, aliases.
- `__typename` selected directly on the namespace field: sent to the remote as a separate root
  step `query { __typename }` (yes, upstream calls the remote) and the value mapped through the
  type map (so `Query` becomes `nsQuery`) (`customizeRemoteNamespace`).
- **Permissions SDL and relationship definitions use original names** (they are validated against
  `_rscIntroOriginal`).
- `__type(name: "Droid")` for an original name that was renamed returns `null`.

Inline fragment quirk: a fragment `... on Droid` (original name, not in the Hasura schema) inside a
selection on a customized interface is **silently ignored** (no error; its fields are omitted)
(`customize_all_the_things.yaml`, case "query with prefix original (non-customized) type name in
inline fragment"). This is a generic Hasura parser behaviour (inline fragments whose type condition
is not a possible type of the parent are dropped) — owned by the GraphQL-parser spec.

---

## 5. Schema merging (per role)

`buildGQLContext` / `buildAndValidateRemoteSchemas` (`server/src-lib/Hasura/GraphQL/Schema.hs`):

### 5.1 Which remote schema a role sees (`getIntrospectionResult`, `Schema/Common.hs:567`)

| condition | introspection used |
|---|---|
| role = `admin` | original |
| remote schema permissions disabled (default) | original, for **every** role incl. unknown roles and the unauthenticated role |
| permissions enabled | the role's resolved permission schema (direct or inherited); none -> remote absent for the role |

Unauthenticated context (no role / `HASURA_GRAPHQL_UNAUTHORIZED_ROLE` unknown): when permissions are
disabled remotes are included but **remote relationships to sources are excluded** from it; when
enabled, remotes are excluded (unknown role -> `field 'messages' not found in type: 'query_root'`).

### 5.2 Building and conflict detection

For each remote (upstream: HashMap order; Rust: metadata order), build its parsers for the role
(query, mutation, subscription field lists; with remote relationships of that remote attached to
its object types). Then, before accepting it:

1. Query root field names: duplicates between this remote and previously accepted remotes ->
   inconsistency `Duplicate remote field 'x'`; duplicates with source query root fields ->
   `Field cannot be overwritten by remote field 'x'`.
2. Same for mutation root fields (only if the remote has mutation fields).
3. Subscription fields are not checked.
4. Type conflicts: collect all type definitions reachable from source query+mutation fields,
   action query+mutation fields, and this remote's query+mutation fields; any two different
   definitions with the same name -> `Conflicting types: Found conflicting definitions for GraphQL type 'User'.  The definition at 'user' differs from the definition at e.g. 'User'.\nFormer has definition:\n<sdl>\nLatter has definition:\n<sdl>`.
   Identical definitions are fine (e.g. two remotes sharing the same custom scalar; test
   `test_add_schema_same_type_containing_same_scalar`). Equality is on the full definition
   (description, fields, args, ...).
5. If any inconsistency, the remote is dropped **for this role** and the inconsistency is
   recorded against `MORemoteSchema name` (message prefix `Inconsistent object: `). Previously
   accepted remotes are kept. The remote's name root fields with namespace count as a single
   field `ns`.
6. Accepted remote fields are appended to the role's `query_root` / `mutation_root` /
   `subscription_root` after source fields (actions are also merged; conflicts between action root
   fields and remote fields surface later as a generic duplicate-field schema error (inferred)).

Tracking a table whose root field collides with an existing remote field fails
(`create_conflicting_table.yaml`: `400 remote-schema-conflicts`, `pg_track_tables` bulk:
warning `node user already exists in current graphql schema` code `track-table-failed`) — owned by
the table-tracking spec but note the code `remote-schema-conflicts`.

### 5.3 What the remote contributes to the Hasura schema

- Only types reachable from the remote's root fields (plus types pulled in by remote
  relationships) appear; unreachable remote types do not (consequence of parser-driven schema).
- Field nullability and list wrapping preserved exactly.
- Descriptions preserved (types, fields, args, enum values). Deprecation and all directives
  dropped (inferred).
- Custom scalars exposed as-is (customized name), values passed through untouched.
- Argument default values preserved in the schema (`fieldWithDefault'`); when the client omits
  such an argument Hasura's parser yields the default, which is then sent explicitly to the remote
  (inferred from parser semantics).
- Remote `subscription` root fields ARE placed in `subscription_root`, but executing them fails:
  `{"errors":[{"extensions":{"code":"not-supported","path":"$"},"message":"subscription to remote server is not supported"}]}`
  (`server/src-lib/Hasura/GraphQL/Execute.hs:239`), HTTP 200, also over WebSocket.
- Relay (`/v1beta1/relay`): remote schemas and remote relationships to remote schemas are not
  exposed (`with_relay.yaml`: `field 'messageBasic' not found in type: 'custom_profiles'`).
- Remote relationships to **sources** are exposed in Relay (only RS targets are excluded).

---

## 6. Executing remote schema root fields

### 6.1 Execution plan

Each remote root field (after namespace unflattening) is an independent `ExecStepRemote`
(`buildExecStepRemote`, `Execute/Remote.hs`). A query mixing DB fields, action fields and remote
fields executes each root field step **sequentially** in selection order
(`InsOrdHashMap.traverseWithKey` in `ExceptT`) and assembles `{"data": {<alias>: <value>, ...}}`
preserving order. Two fields from the same remote = **two HTTP requests**.

Mutations: remote mutation fields are separate steps; the PG single-transaction optimisation only
applies when *all* mutation root fields are PG DB fields on one source, so mixing a remote mutation
with DB mutations runs them sequentially, non-atomically.

### 6.2 Building the outgoing GraphQL request

Input: the parsed IR field (`GraphQLField`), with role presets applied (§7.4) and variables
resolved.

```
GQLReq {
  operationName: <client's operationName, if any>,
  query: TypedOperationDefinition {
     type: query|mutation,
     name: <client's operationName, if any>,
     variableDefinitions: one per distinct variable actually referenced in this field's tree,
     directives: [],
     selectionSet: [ <the single root field> ]
  },
  variables: { name: value } or Nothing when no variables
}
```

`operationName` is the client request's JSON `operationName` field (`GH._grOperationName gqlUnparsed`
in `Execute/Query.hs:156` / `Execute/Mutation.hs:164`), **not** the name written in the client's
document: a client sending `query Foo { ... }` without a JSON `operationName` produces an anonymous
outgoing operation with no `operationName`. Test: `basic_query_with_op_name.yaml`
(`test_remote_schema_operation_name_in_response`).

Rules:

- **Aliases**: alias emitted only when it differs from the field name (`convertGraphQLField`). A
  customized field with no client alias gets alias = customized name (§4.3).
- **Field names / type conditions**: original remote names.
- **Fragments**: Hasura fully normalizes the selection (named fragments and inline fragments are
  flattened; `@include`/`@skip` evaluated locally). No directives are forwarded (`_fDirectives =
  mempty`). Object selection sets are plain field lists.
- **Abstract types (interface/union)**: the normalized form is "for each possible member type, its
  full field list". It is re-encoded (`reduceAbstractTypeSelectionSet`):
  1. take the longest common prefix of all member selection lists (by alias+field equality);
  2. from that prefix keep the leading fields that are defined on the abstract type itself
     (`__typename` always is; for interfaces the interface's fields; for unions only `__typename`);
     these become the base selection;
  3. strip the base from each member list; drop members whose remaining list is empty; emit
     `... on Member { rest }` inline fragments (original type names) after the base fields.
  Example: `node(id:"x"){ __typename id ... on Film{title} ... on Species{name} }` is sent as
  `node(id: "x") { __typename  id  ... on Film{ title  } ... on Species{ name  } }` (exact printer spacing, §6.6; alias
  `__typename: __typename` dropped because alias == name). Member order follows HashMap order
  upstream (non-deterministic); any order is fine.
- **Variables**: a variable is kept as a variable (not inlined) when the parser did not need to
  alter it (§6.3). Its definition is `$name: <type as declared by client, with base type name
  mapped back to the original remote name>` plus the client's default value if any (`VIOptional`).
  Value = the JSON value supplied by the client (or the default; absent optional -> `null`).
- **Literals**: forwarded as written by the client. Note the remote-input parsers do *not* coerce
  scalars: e.g. `run(foo: {bar: "0"})` where `bar: Int!` is accepted and forwarded verbatim
  (remote decides). Variables are type-checked against the (customized) schema types.
- **Null forwarding** (`HASURA_GRAPHQL_REMOTE_SCHEMA_SKIP_NULLS`, default false =
  `RemoteForwardAccurately`): explicit `null` for a nullable argument/input field is forwarded as
  `null`; absent stays absent. With `true` (`RemoteOnlyForwardNonNull`), explicit nulls on
  nullable positions are dropped from the outgoing request.

### 6.3 Variable expansion with presets / customization (`Schema/Remote.hs` notes)

Parsers track an `Altered` flag. If the value at some argument subtree is unaltered, the original
input (variable reference or literal) is forwarded unchanged. A subtree is *altered* when:

- a `@preset` was injected somewhere inside it, or
- a type name in it was customized (input object, enum, scalar whose customized name != original),
  because the variable's declared type would be wrong for the remote.

When a variable's value must be expanded (altered subtree inside a variable), the variable is
"peeled": its JSON value is walked, presets injected, and every **leaf** JSON value that remains
becomes a new variable `$hasura_json_var_<n>` (n = 1, 2, ... in order of first use; identical
`(type, value)` pairs share a variable) of the leaf's remote type:

```
# role schema: input Range { low: Int! @preset(value: 0)  high: Int! }
# client:
query($r: Range!) { getValues(range: $r) }      variables: {"r": {"high": 42}}
# sent:
query ($hasura_json_var_1: Int!) { getValues(range: {low: 0,high: $hasura_json_var_1})  }
variables: {"hasura_json_var_1": 42}
```

(Exact printer output per §6.6: object fields are joined by `,` with **no** space, and a leaf field
is followed by `' '` before the closing ` }`, hence the double space. Object-field order is
HashMap order upstream.)

Short-circuit: if an input-object JSON value is unaltered, a single JSON variable of the whole
object's type is created instead of per-leaf variables.

Session presets produce variables named after the session variable with `-` -> `_`
(`x-hasura-user-id` -> `$x_hasura_user_id`), declared non-null of the preset's base type (§7.4).

### 6.4 HTTP request

`execRemoteGQ` (`RemoteServer.hs`):

- `POST <resolved url>`, body = `J.encode` of `GQLReq GQLQueryText` (verified,
  `Transport/HTTP/Protocol.hs:78-80`: generic encoding, `omitNothingFields = True`):
  keys in order `operationName`, `query`, `variables`; **`operationName` is omitted (never
  `null`) when absent, and `variables` is omitted (never `null`) when the outgoing operation has no
  variables**. E.g. `{"query":"query  { hello  }"}` or
  `{"operationName":"HelloMe","query":"query HelloMe { hello  }"}` or
  `{"query":"query ($hasura_json_var_1: Int!) { ... }","variables":{"hasura_json_var_1":42}}`.
  Remote-join calls (§9.4) never carry `operationName`.
- Headers, merged by name with priority **configured headers > session-variable headers > client
  headers**:
  1. configured `headers` (env resolved);
  2. **all** session variables of the request's `UserInfo` as headers (`x-hasura-role` always
     present — set to the resolved role; `x-hasura-admin-secret` removed; names lowercase, e.g.
     `x-hasura-user-id: 1`). Sent regardless of `forward_client_headers`;
  3. if `forward_client_headers`: client request headers minus
     `Content-Length, Content-MD5, User-Agent, Host, Origin, Referer, Accept, Accept-Encoding,
     Accept-Language, Accept-Datetime, Cache-Control, Connection, DNT, Content-Type` and minus any
     `x-hasura-*` header (those come from the session), plus `X-Forwarded-Host` (from `Host`),
     `X-Forwarded-User-Agent` (from `User-Agent`), `X-Forwarded-Origin` (from `Origin`) when present.
     `Authorization`, `Cookie` etc. are forwarded.
  Then `Content-Type: application/json` and `User-Agent: hasura-graphql-engine/<version>` override.
  Header-name comparisons are case-insensitive. Test: `test_remote_schema_forward_headers` (remote
  asserts no duplicate headers).
- Timeout `timeout_seconds` (response timeout). Errors -> `400 remote-schema-error`
  `HTTP exception occurred while sending the request to "<name>"`, internal
  `{type:"http_exception", message:"Response timeout", ...}` (`basic_timeout_query.yaml`).
- Trace context propagated (B3 for introspection; configured OTel propagator at runtime).
- Response status code ignored; body parsed.

`Set-Cookie` headers from the remote response are copied to the client HTTP response (only
`Set-Cookie`; `check_resp_headers.yaml`).

### 6.5 Response handling (`extractFieldFromResponse`, `Transport/HTTP.hs`)

1. Decode JSON (ordered). Non-JSON -> `400 remote-schema-error` with the parser message.
2. `errors`: absent or `null` -> none; non-empty array -> errors; `[]` ->
   `Empty "errors" field in response from remote`; other -> `Invalid "errors" field in response from remote`.
3. `data` vs errors:
   - data absent/null, no errors -> `Missing "data" field with no errors in response from remote` /
     `Received null "data" field with no errors in response from remote` (400 remote-schema-error);
   - data absent/null + errors -> **GQ exec error** with the remote errors;
   - data + no errors -> data;
   - data + errors -> with `HASURA_GRAPHQL_REMOTE_SCHEMA_PRIORITIZE_DATA=true`
     (`--remote-schema-prioritize-data`) use data and **discard** the errors; default: treat as
     errors (data discarded).
4. Apply the result customizer to `data` (rename `__typename` values), then look up the root
   field's response key (alias or customized name). Missing -> `400 remote-schema-error`
   `expecting key <alias>`.
5. Process remote joins on the value (§9).

**Error propagation**: a GQ exec error from any root field step aborts the **whole** operation:
HTTP 200 with body `{"data": null, "errors": [<remote error objects verbatim>]}`. Remote errors are
copied byte-for-byte in content (message, locations, path, extensions — **path is not rewritten**,
namespace not prefixed, aliases as sent). Errors are not merged with errors from other root
fields; other fields' data is discarded. Hasura-side errors (`QErr`) use the normal
`{"errors":[{"message", "extensions":{"code","path":"$"...}}]}` envelope without `data`.

Example (`validate_data_errors_prioritization/no_prioritization`): remote returns
`{"data":{...},"errors":[{"message":"field 'test' not found in type: 'query_root'","extensions":{"code":"validation-failed","path":"$.selectionSet.test_lol"}}]}`
-> Hasura responds `{"data":null,"errors":[<same error object>]}`.

WebSocket (graphql-ws / graphql-transport-ws) queries and mutations to remotes behave the same;
the error/data envelope is sent as a `data`/`next` payload.

### 6.6 GraphQL printer format (wire text)

Upstream renders outgoing documents with graphql-parser's printer
(`server/lib/graphql-parser/src/Language/GraphQL/Draft/Printer.hs`). Golden tests (e.g.
`query_with_errors_obj.yaml`) assert error `locations` columns returned by the test remote, so the
**exact spacing matters**:

```
operation      := opType ' ' [name] [ '(' varDefs ')' ] ' ' selectionSet
selectionSet   := ''                                   (empty)
                | '{ ' sel (' ' sel)* ' }'
field          := [alias ': '] name [ '(' args ')' ] ' ' selectionSet
                  -- NOTE: always a space after the field, even if selectionSet empty
inlineFragment := '... ' ['on ' TypeName] selectionSet          -- no space between name and '{'
args / object  := name ': ' value (',' name ': ' value)*        -- comma, NO space
list           := '[' value (',' value)* ']'
varDefs        := '$' name ': ' type [' = ' default] (',' ...)*
string         := JSON-encoded string (aeson), or block string if multi-line rules apply
float          := scientific rendering
```

`inlineFragment` prints `"... " <> "on " <> Type <> selectionSet` -> `... on Film{ title  }`
(there is **no** space between the type name and `{`; verified: `Printer.hs:210` `inlineFragment` concatenates `"... "`, `"on " <> name`, directives, `selectionSetP` directly).

Worked example (verified against `query_with_errors_obj.yaml` locations): the remote-join query
for three LHS rows is

```
query  { f0: message(id: 1) { msg  errorMsg  } f1: message(id: 2) { msg  errorMsg  } f2: message(id: 3) { msg  errorMsg  } }
```

(`query` + `' '` + (no name) + `' '` + `{ `; leaf fields render as `name ` and are joined by
`' '`, giving double spaces.) `errorMsg` is at columns 36, 74, 112 — exactly what the test
expects. Argument and variable-definition order follow HashMap order upstream; tests use single
arguments, so any deterministic order is acceptable.

---

## 7. Remote schema permissions

Enabled only when `HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS=true`
(`--enable-remote-schema-permissions`, default false).

### 7.1 API behaviour

`add_remote_schema_permissions` (`MetadataAPI/Permission.hs`):

1. Permissions disabled -> `400 constraint-violation`
   `remote schema permissions can only be added when remote schema permissions are enabled in the graphql-engine`.
2. Remote must be **consistent** -> `400 not-exists` `remote schema "r" doesn't exist`.
3. Duplicate role -> `400 already-exists` `permissions for role: "user" for remote schema:"r" already exists`
   (note missing space after `schema:`).
4. Role `admin` -> `400 constraint-violation` `cannot define permission for admin role`.
5. Validate the SDL against the original remote introspection (§7.2); failure -> `400 validation-failed`.
6. Prepend to the remote's `permissions` list; rebuild.

`drop_remote_schema_permissions`: remote must exist (consistent) else
`remote schema "r" doesn't exist`; role must have a permission else
`permissions for role: "user" for remote schema:"r" doesn't exist` (`not-exists`).

When loaded from metadata, an invalid permission becomes an inconsistent object
`remote_schema_permission` with reason prefix `in remote schema permission for role "user": `.
Permissions present in metadata while the flag is disabled are still validated/built but unused
(inferred: `buildRemoteSchemaPermissions` runs regardless; `getIntrospectionResult` ignores them).

### 7.2 SDL validation (`resolveRoleBasedRemoteSchema`, `SchemaCache/Permission.hs`)

The submitted document is parsed as a GraphQL schema document (type system definitions). Built-in
scalars `Boolean, Float, ID, Int, String` are implicitly added. Rules (errors are accumulated and
reported together; `refute` stops at fatal ones):

- At most one `schema { ... }` definition (`multiple schema definitions found`). Root names come
  from it; **if absent, the query root defaults to `Query`** (regardless of the upstream's root
  name), and mutation/subscription roots are absent unless named in a `schema` block or an object
  named `Mutation`/`Subscription` exists (default-root fallback at parser build).
- No duplicate type names (`duplicate type names found: "A" and "B"`).
- Every type must exist in the upstream with the same kind:
  `"<Kind>": "X" does not exist in the upstream remote schema` (kinds: `Enum`, `Input object`,
  `Object`, `Interface`, `Union`, `Scalar`, `Directive`).
- Objects: provided fields ⊆ upstream fields (`field "f" does not exist in the "Object": "T"`),
  same field types exactly (`expected type of "f"("Object field") to be Int but received Int!`),
  no duplicate fields; implemented interfaces must be upstream's interfaces
  (`object "T" is trying to implement the following interfaces that do not exist in the corresponding upstream remote object: ...`);
  interfaces declared in the doc but not implemented upstream ->
  `custom interfaces are not supported. Object"T" implements the following custom interfaces: "I"`.
- Field arguments: provided ⊆ upstream (`input argument "a" does not exist in the input object:"f"`),
  every **non-nullable** upstream argument must be present
  (`field: "user" expects the following non nullable arguments to be present: "user_id"`),
  same type, same default value (`expected default value of input value: "a"of input object "X" to be <v> but received <v>`).
- Interfaces: field subset & types as objects. `possibleTypes` of the role schema = objects in the
  doc that implement it.
- Unions: members ⊆ upstream members (`union "U" contains members which do not exist in the members of the remote schema union :...`).
- Enums: values ⊆ upstream values (`enum "E" contains the following enum values that do not exist in the corresponding upstream remote enum: ...`), no duplicates.
- Input objects: fields like arguments (subset, all non-null upstream fields present, same types,
  same defaults).
- Directives used in the doc (other than `@preset`) must exist upstream with argument subset.
- Descriptions: provided description wins, else upstream description.
- Error message assembly: one error -> `validation for the given role-based schema failed because <msg>`;
  several -> `validation for the given role-based schema failed for the following reasons:\n • <msg1>\n • <msg2>\n`.

### 7.3 `@preset`

```graphql
directive @preset(value: PresetValue, static: Boolean) on INPUT_FIELD_DEFINITION | ARGUMENT_DEFINITION
```

(not declared in the doc; recognised by name `preset`).

- Only on argument / input-field definitions
  (`Preset directives can be defined only on INPUT_FIELD_DEFINITION or ARGUMENT_DEFINITION`),
  at most one per position (`found multiple preset directives at ...`), must have args
  (`no arguments found in the preset directive`), must have `value`
  (`preset argument "value" not found at "a"`), `static` must be Boolean
  (`expected preset static value to be a Boolean value`).
- Value validated against the position type:
  - scalar: string/int/float/bool/null literal; enum/list/object rejected
    (`expected preset value <v> of type "T" to be a scalar value`). A string matching
    `x-hasura-*` (case-insensitive prefix, even `x-hasura-` alone) is a **session-variable preset**
    unless `static: true`.
  - enum: enum literal that exists (`"V" not found in the enum: "E"`) or a session-variable string
    (`static` ignored for enums; non-session strings rejected `expected preset value ... to be an enum value`).
  - input object: object literal, keys must exist (`"k" does not exist in the input object "T"`),
    recursively validated; other -> `expected preset value <v> of type "T" to be an input object value`.
  - list types: list literal (each element validated), or a single value coerced to a list type;
    a session-variable string on a list type -> `illegal preset value at "a". Session arguments can only be set for singleton values`.
  - output type named -> `expected "T" to be an input type, but it's an output type`.
- Effect on the role schema: preset arguments/input fields are **removed** from the role's GraphQL
  schema (client cannot pass them). If *all* fields of an input object are preset, the input
  object itself does not appear and the containing field/argument is also removed; its preset
  values are always injected (even if the client "would have omitted" it).
- At execution the preset value is always injected into the outgoing request (static literal, or
  session variable value). Presets on arguments of remote-relationship join arguments are ignored
  (value comes from LHS; §8.4).

### 7.4 Session-variable preset resolution (`resolveRemoteVariable`, `Execute/Remote.hs`)

For `SessionPresetVariable sessVar typeName info`:

- session variable missing -> `400 not-found` `"x-hasura-user-id" session variable expected, but not found` (request fails, HTTP 200 GraphQL error with `path: $`).
- coercion by base type name:
  - `Int`: Haskell `read` as Integer; failure -> `400 coercion-error` `"randomid123" cannot be coerced into an Int value`.
  - `Boolean`: exactly `"true"`/`"false"` else `... cannot be coerced into a Boolean value`.
  - `Float`: `read` as Double else `... cannot be coerced into a Float value`.
  - `String`, `ID`, any custom scalar: string as-is.
  - enum: must be a valid GraphQL name (`"v" is not a valid GraphQL name`) and one of the enum's
    values (`"v" is not one of the valid enum values`), sent as enum literal.
- Emitted as variable `$<session var with '-' replaced by '_'>` (e.g. `$x_hasura_user_id`) of type
  `<typeName>!` with the coerced value.

### 7.5 Inherited roles

For an inherited role without its own permission, the parents' resolved permission schemas are
combined: all defined parents must be identical (`OnlyRelevantEq`: same set of type definitions
incl. presets, same root names; descriptions are part of the definitions — inferred); if identical
-> inherited; parents undefined -> no access; different -> inconsistency
`ConflictingInheritedPermission` (serialized as `{"reason": ..., "name": <role>, "type": "inherited role permission inconsistency", "entity": ...}`,
`RQL/Types/Metadata/Object.hs:391-395`; message per the inherited-roles spec). A role-specific permission overrides inheritance.

### 7.6 SDL re-printing in metadata

`RemoteSchemaPermissionDefinition` serializes `schema` by printing the parsed document
(`G.schemaDocument`): type definitions sorted (by the derived `Ord` of the AST — effectively by
kind/description/name **(inferred)**), built-in scalars omitted, blocks separated by `\n\n`, object
fields sorted and rendered `type T { f1: A\n  f2: B\n}`. Interface rendering includes a stray space
(`interface I  { ...`). Implementations needing byte-exact `export_metadata` round-trips should
port the printer; otherwise store and export the original text and accept a diff (P2 decision).

---

## 8. Remote relationships — metadata and validation

### 8.1 Definition formats

A remote relationship is `{name: RelName, definition: RemoteRelationshipDefinition}`. On a table it
lives in `tables[].remote_relationships[]`; on a remote schema type in
`remote_schemas[].remote_relationships[].relationships[]`.

`definition` accepts (`parseRemoteRelationshipDefinition`, `RQL/Types/Relationships/Remote.hs`):

**New (unified) format — to remote schema**

```yaml
definition:
  to_remote_schema:
    remote_schema: my-remote          # NOTE key is "remote_schema" (docs comment says "schema"; source/tests use remote_schema)
    lhs_fields: [id, name]            # set of LHS field names (columns / computed fields / RS fields)
    remote_field:                     # exactly one key per level
      messages:
        arguments:                    # required (may be {})
          where: { id: { eq: "$id" } }
        field:                        # optional nested hop
          ...
```

**New format — to source (database)**

```yaml
definition:
  to_source:
    source: store_db
    table: orders                     # backend-specific table name JSON, parsed against the TARGET source's backend
    relationship_type: array          # object | array
    field_mapping:                    # LHS field name -> target column name
      id: user_id
```

**Legacy format (table -> remote schema only)** — "v1":

```yaml
definition:
  remote_schema: my-remote
  hasura_fields: [id]
  remote_field:
    message:
      arguments:
        id: "$id"
```

Parsing modes:

| context | mode | accepted |
|---|---|---|
| metadata (tables & remote schema relationship entries) | Lenient | legacy or `to_remote_schema` or `to_source` |
| `*_create/update_remote_relationship` with top-level `remote_schema` | Legacy | args are `{source?, table, name, remote_schema, hasura_fields, remote_field}` (inline, no `definition`) |
| `*_create/update_remote_relationship` with `definition` | Strict | `to_remote_schema` or `to_source` only |
| `create/update_remote_schema_remote_relationship` | Strict | |

Errors: both/neither of `remote_schema`/`definition` in create args ->
`create_remote_relationship expects exactly one of: remote_schema, definition`; inside definition:
`remote relationship definition (lenient format) expects exactly one of: remote_schema, to_source, to_remote_schema`
(`(strict format) ... to_source, to_remote_schema`, `(legacy format) ... remote_schema`).

**Round-trip**: the format is remembered (`RRFormat`) and re-emitted identically — legacy stays
legacy (`{remote_schema, hasura_fields, remote_field}` inline under `definition`), unified stays
unified. Must be preserved for `export_metadata` fidelity.

`remote_field` JSON: an object with **exactly one** key (field name) mapping to
`{arguments: {...}, field?: <RemoteFields>}`; errors
`Remote fields are represented by an object that maps each field name to its arguments.` +
`Expecting one single mapping, received none.` / `... received too many.`.
`arguments`: object of argument name -> value where JSON is converted to a GraphQL value:
string `"$x"` -> variable `x` (must be a valid GraphQL name; `"$"` alone -> `Empty variable name`),
other strings -> string literal, integral numbers -> Int, others -> Float, bool, null, arrays,
objects (keys must be GraphQL names). There is no way to write an enum literal (strings stay strings).

### 8.2 Commands

`pg_create_remote_relationship` (and other backends):

```json
{"type":"pg_create_remote_relationship","args":{
  "source":"default",          // optional, default "default"
  "table":"users",             // backend table name (string or {schema,name})
  "name":"messages",
  "definition":{"to_remote_schema":{...}} }}
```

- create: table must exist in the source (`askTableInfo`); inserts/overwrites by name in
  `remote_relationships`; `buildSchemaCacheFor (table remote relationship)` -> validation failures
  become `400 invalid-configuration` with the inconsistency reason (e.g.
  `Inconsistent object: in table "custom.profiles": in remote relationship "message": field with name "id_wrong"is not provided by the lhs entity"custom.profiles"for defining a join condition, the allowed fields are "id" or "name"`
  — note the missing spaces, from `<<>`/`<>>` concatenation).
- update (verified, `RQL/DDL/RemoteRelationship.hs:146`, `Table/Cache.hs:1199-1326`): table must
  exist (`askTableFieldInfoMap` -> `400 not-exists` `table "users" does not exist in source: default`);
  then `askRemoteRel` looks the name up in the table's **field info map**:
  - no field with that name (including a remote relationship that is currently inconsistent and
    therefore not in the cache) -> `400 not-exists` `"messages" does not exist`
    (`askFieldInfo`: `f <<> " does not exist"`);
  - a field with that name that is not a remote relationship (a column, local relationship,
    computed field) -> `400 unexpected-payload` `expecting a remote relationship`;
  then same as create.
- delete: `{"source"?, "table", "name"}` (`source` defaults to `default`); same existence checks
  and messages as update (`execDeleteRemoteRelationship`), then rebuild with `buildSchemaCacheFor`.
- create performs only the table check (`askTableInfo` -> `table "t" does not exist in source: s`)
  and silently **overwrites** an existing relationship of the same name.
- `/v1/query` legacy names: `create_remote_relationship`, `update_remote_relationship`,
  `delete_remote_relationship` (Postgres, `source` defaults to `default`). Fixtures in tests-py use
  this with the legacy inline format.

`create_remote_schema_remote_relationship`:

```json
{"type":"create_remote_schema_remote_relationship","args":{
  "remote_schema":"remote","type_name":"Writer","name":"articles",
  "definition":{"to_remote_schema":{"remote_schema":"remote","lhs_fields":["wIds"],
     "remote_field":{"articles":{"arguments":{"ids":"$wIds"}}}}}}}
```

- Inserts into `remote_schemas[r].remote_relationships[type_name].relationships[name]` (creating
  the type entry if absent). If remote schema `r` does not exist in metadata the lens update is a
  no-op and the command returns success (quirk, inferred from `ix`).
- `update_remote_schema_remote_relationship`: must exist in the schema cache
  (`scRemoteSchemas[r].rscRemoteRelationships[type_name][name]`) ->
  `400 not-exists` `no relationship defined on remote schema "r" with name "n"` (verified,
  `RQL/DDL/RemoteRelationship.hs:262-282`; note the message does not mention `type_name`), then
  identical to create.
- `delete_remote_schema_remote_relationship` `{remote_schema, type_name, name}`: removes via
  `ix`/`ix`/`delete` lenses — **no existence check at all**: unknown remote schema, type or
  relationship name -> no-op, rebuild, `{"message":"success"}` (verified,
  `RQL/DDL/RemoteRelationship.hs:305-325`).

### 8.3 LHS join fields

| LHS entity | allowed join fields |
|---|---|
| table (any backend) | all scalar columns + **scalar computed fields whose function has no input arguments besides the table row and (optional) session argument** (`Cache/Fields.hs:69`) |
| remote schema object type | all fields of that **object** type (original names) **without arguments** (any output type incl. lists/objects) (`getRemoteSchemaEntityJoinColumns`); type must exist (`no type named "T" defined in remote schema "r"`) and be an object (`remote relationships on a remote schema can only be defined on object types`) |
| action output type | see actions spec (only `to_source`) |

LHS identifier (used in messages and generated type names, `LHSIdentifier`): tables -> table name
text (`toTxt` of `QualifiedTable`: `profiles` for schema `public`, else `custom.profiles`);
remote schema -> remote schema name.

### 8.4 Validating `to_remote_schema` (`validateToSchemaRelationship`)

Performed against the **original** introspection (admin) at schema-cache build; repeated per role
against the role's permission schema when permissions are enabled (failure -> field absent for that
role, silently).

1. Every `lhs_fields` entry must be an LHS join field (`JoinFieldNonExistent` message above).
   Each LHS field name must be a valid GraphQL name (it becomes a variable name)
   (`"x" is not a valid GraphQL identifier`).
2. Walk `remote_field` from the remote **query root** (mutations not supported):
   - field must exist on the current object: `remote field with name "f" and type "T" not found`;
   - provided arguments must exist: `argument with name "a" not found in remote schema`;
     (missing required arguments are NOT checked at definition time);
   - each provided value validated (`validateType`):
     - `$var`: must be one of `lhs_fields` (`variable "v" is not found`); its type is not checked.
     - Int/Float/Boolean/String literals: `isTypeCoercible(<literal scalar>!, expected)`:
       if expected base is `ID`, actual must be one of `ID, Int, String, bigint, smallint, uuid`
       (`Only ID, Int, uuid or String scalar types can be joined to the ID type, but received "Boolean"`);
       otherwise base names must be equal and list depth equal or actual depth 0
       (`expected type "Int" but got "String"`).
     - `null` -> `null is not allowed here`; enum -> `enum value is not supported`.
     - list literal: at most one element (`multiple elements in list value is not supported`),
       expected type must be a list (`type "X"is not a list type` — **no space** after the closing
       quote: `"type " <> base <<> err` with `err` lacking a leading space), element validated against the
       unwrapped type.
     - object literal: expected type must be an input object (`type "k"not an input object type`
       — upstream bug: the quoted name is the object **key** being validated, not the type, and no
       space; / `type with name "X" not found`; an empty object `{}` is never checked), keys must exist (`argument with name "k" not found in remote schema`),
       recurse.
   - the next hop's type: if the field returns an object type, continue there; if it returns a
     scalar/enum/interface/union the walk may only end here (else
     `type "X"only output type is expected` — no space, `X` = base of the field's type). Lists are
     unwrapped by base type for lookup.
3. **Argument stripping** for the generated Hasura field (`stripInMap`): for the *leaf* field's
   arguments only:
   - argument with an atomic template value (variable, scalar literal, null, enum, empty list,
     single-element list of atomic) -> **removed** from the Hasura field;
   - argument with an object template -> its input object type is cloned as
     `<OrigInputType>_remote_rel_<lhsGraphQLName><relName>` (lhs identifier with non-alphanumerics
     replaced by `_`, e.g. `MessageWhereInpObj_remote_rel_custom_profilesmessagesNestedArgs`) with
     the templated keys stripped recursively (nested objects cloned likewise); the user can still
     pass the remaining keys;
   - single-element list of object -> list of the cloned type;
   - untouched arguments are exposed as-is (with role presets still applied).
   Intermediate hops' arguments are fully determined by the template (no user args).
4. Result (`RemoteSchemaFieldInfo`): the user-visible param map, the new input types, the remote
   field path, the remote schema info, and the LHS fields actually required.

Dependencies recorded: table (or remote schema) + each LHS column/computed field
(`DRRemoteRelationship`) + target remote schema (`DRRemoteSchema`). Dropping a column used by a
remote relationship requires cascade (run_sql with `cascade: true` drops the relationship);
renaming a column/table updates the relationship metadata automatically
(`rename_col_with_remote_rel_dependency.yaml`).

### 8.5 Validating `to_source`

`buildRemoteFieldInfo` (`RQL/DDL/RemoteRelationship.hs`):

- target source exists (`source not found: "s"`, code `not-found`), supports being a remote
  relationship target (`source s does not support being used as the target of a remote relationship`);
- `table` parsed with the target backend's table-name parser; table exists
  (`table "t" does not exist in source: s`);
- each `field_mapping` key is an LHS join field and each value is a field of the target table;
  both lookups use `askFieldInfo` (verified, `Table/Cache.hs:1204`): `400 not-exists`
  `"x" does not exist` (no prefix naming the side); enum-reference columns ->
  `relationships to enum fields are not supported yet` (`not-supported`); non-scalar ->
  500 `Target field "x"is not a scalar column`.
- Types of LHS and RHS columns are **not** checked for compatibility at definition time
  (mismatch fails at query time in SQL) (inferred).
- Dependencies: target table (`DRTable`) and each target column (`DRRemoteRelationship`).

### 8.6 Generated GraphQL fields

**Table -> remote schema** (`remoteRelationshipToSchemaField`):

- Field name = relationship name (not customized).
- Return type = the leaf remote field's declared type, **exactly** (nullability + lists), with the
  remote's (customized) type names; e.g. `messagesNestedArgs: [Message]` -> kind `LIST`.
- Description = description of the **target type** (e.g. `"A music album with track information"`).
- Arguments = stripped param map (§8.4).
- Visible for a role iff: the LHS table is selectable for the role (remote relationship fields are
  always considered selectable — **no LHS column permission check**, the join columns are fetched
  as phantom fields even if the role can't select them), the role has access to the remote
  (§5.1), and the relationship validates against the role's remote schema.
- Not added to `<table>_bool_exp` or `<table>_order_by` (remote relationships are not
  filterable/sortable), nor to aggregate `nodes`? — they are available inside `nodes` of
  `_aggregate` and in mutation `returning` (Collect handles `TAFNodes` and `MOutMultirowFields`)
  (`mutation_output_with_remote_join_fields.yaml`).
- Subscriptions (verified, `server/src-lib/Hasura/GraphQL/Execute.hs:259-271`): the field is
  present in the schema of table types reachable from `subscription_root`, but selecting it in a
  live-query **or** streaming subscription (DB root field or async-action root field) fails at plan
  time with `400 not-supported` `Remote relationships are not allowed in subscriptions`
  (`RJ.getRemoteJoinsQueryDB` / `getRemoteJoinsActionQuery` returned joins). This applies to
  `to_source` (DB-to-DB) relationships too. Over WebSocket this is an `error` message for the
  operation.

**Table -> table (to_source)** (`remoteRelationshipToSourceField`):

- `object`: field `rel: <target_table_type>` (nullable object); requires the role to have select
  permission on the target table (else no field).
- `array`: field `rel(where, order_by, limit, offset, distinct_on): [T!]!` and
  `rel_aggregate(...): T_aggregate!` (aggregate field only if the role's target select
  permission allows aggregations; name respects the target source's naming convention).
- Target permissions (filter incl. session variables, columns, limit, allow_aggregations) apply.
- `stringify numeric types` setting of the server applies to the target result.

**Remote schema -> X**: the relationship field is added to the remote object type `type_name`
in the remote's schema (under customized type name), for every role that can see that type.

---

## 9. Remote join execution

### 9.1 Overview

```
root step (DB / remote schema / action) ──► LHS JSON response with placeholders + phantom fields
   │ collectJoinArguments: walk the JSON along the JoinTree, collect distinct JoinArguments per join
   ▼
for each distinct RemoteJoin (sequentially):
   build ONE batched call  ──► remote schema: one GraphQL query with aliases f<id>
                           ──► source: one SQL query over jsonb_to_recordset(<all args>)
   build join index {argumentId -> JSON}
   recurse into child joins of that join with the join results as LHS
replace placeholders with join results, drop phantom fields → final JSON
```

### 9.2 Plan-time collection (`RemoteJoin/Collect.hs`)

For every selection set containing remote relationship fields:

- **Join tree**: a trie keyed by response keys (aliases) from the root field down to each remote
  relationship field; each key is `QualifiedFieldName {typeName?, fieldName}`; the type name is
  only set inside union/interface member selections (see below). Identical `RemoteJoin`s at
  different paths are deduplicated by structural equality and share one join call
  (`assignJoinIds`).
- **Placeholder**: the relationship field itself is replaced, at the same position, by
  - DB LHS: a SQL expression field selecting the literal text `'remote relationship placeholder'`
    under the field's alias (`AFExpression`);
  - action LHS: likewise (`ACFExpression`);
  - remote-schema LHS: `alias: __typename` (`mkPlaceholderField`).
  This keeps response key order without bookkeeping; the value is later overwritten.
- **Join columns / phantom fields**: for all remote fields in a selection set, union their LHS join
  fields. For each join field:
  - if the same column (DB: same column; computed field: same name; RS: same field name **with no
    arguments**) is already selected in this selection set -> use its alias (`JCSelected`);
  - else add a **phantom** field with a unique alias
    `<fieldName>_join_column` + `"_" × max(0, longestAliasLen − (len(fieldName)+12) + 1)`
    (guaranteed longer than every alias in the set, hence unique) (`JCPhantom`), selecting the
    column / scalar computed field (with session argument) / RS field.
  Phantom fields are appended at the end of the selection set and removed from the final response.
- **Abstract types on the remote-schema LHS**: in a union/interface member selection that contains
  (directly or deeper) a remote join, the field `__hasura_internal_typename: __typename` is added,
  and join tree keys under it carry the member type name. At traversal time the value of
  `__hasura_internal_typename` picks the branch; the key is always removed from the output.
- Remote joins inside the *target* selection (nested remote relationships, e.g. RS -> DB -> RS)
  are collected recursively into the child join tree of that join.

### 9.3 Collecting join arguments (`collectJoinArguments`, `Join.hs`)

Traverse the LHS JSON with the join tree:

- `null` value where a subtree is expected -> stays `null` (no join; e.g. object relationship was
  null or row filtered).
- arrays -> traverse each element; objects -> per key:
  - key matches a `Leaf` join: build `JoinArgument = {lhsFieldName -> value at its alias}` (a map
    of FieldName -> JSON). Missing alias -> `500` `a join column is missing from the response: <alias>`.
    **If any value is JSON `null` -> the relationship value is `null` and no argument is
    collected** (`remote_rel_with_null_joining_fields.yaml`: `employeeMessages: null`; DB-target
    array relationships also return `null`, not `[]`, in that case — even though the field type
    is `[T!]!`).
    Otherwise look up the argument in this join's argument map; new arguments get the next
    `argumentId` from a counter **shared across all joins** in this traversal (ids 0,1,2,...);
    duplicates reuse the existing id (deduplication by full argument value).
    The key's value becomes a replacement token `(joinId, argumentId)`.
  - key matches a `Tree` -> recurse into the value.
  - otherwise copy the value, except phantom keys and `__hasura_internal_typename` which are
    dropped.
- scalar where a subtree is expected -> `500` `found a scalar value when traversing with a non-empty join tree`.
- Order of keys preserved.

If a join has zero arguments (all LHS rows null or no rows), **no call** is made.

### 9.4 Call to a remote schema (`RemoteJoin/RemoteSchema.hs`)

For one join with arguments `{id_k -> JoinArgument_k}` (k in increasing id order):

1. For each argument: convert each LHS value JSON -> GraphQL **literal** (`jsonToGraphQL`:
   string -> String, integral number -> Int, other number -> Float, bool, null, array -> list,
   object -> object). Join values are inlined as literals, **not** variables.
2. Build a field from the `remote_field` path (`fieldCallsToField`):
   - top alias `f<argumentId>` (e.g. `f0`, `f7` — ids are global, so a join's aliases need not
     start at 0);
   - each hop: `name(args)` where args = the template with `$var` substituted by the literal of
     LHS field `var` (missing -> `400 unexpected` `Found errors: Value for variable "v" not provided`);
   - at the **leaf** hop, the user-supplied arguments (from the Hasura field, incl. presets and
     client variables) are merged with the templated arguments via `combineValues`: objects merge
     key-wise recursively, lists **concatenate** (user list first), any other collision ->
     `500` `combineValues: cannot combine values (...) and (...) for field <name>; lists can only be merged with lists, objects can only be merged with objects`;
   - leaf selection set = the user's selection on the relationship field (converted as in §6.2,
     incl. placeholders/phantoms for nested joins).
3. One request for all arguments:
   ```
   query <varDefs?> { f0: message(id: 1) { ... } f1: message(id: 2) { ... } ... }
   ```
   `operationName` absent; variables = client variables / preset session variables /
   `hasura_json_var_n` used by user arguments (deduplicated across the fields).
4. Send with the same `execRemoteGQ` (headers: configured + session + forwarded client headers;
   timeout). Response handling is **different from root fields**:
   - invalid JSON -> `500` `Remote server response is not valid JSON: ...`;
   - `errors` present and non-null -> `400 unexpected` `Errors from remote server`, with
     `extensions.internal = {"errors": [<remote errors>]}` (internal visible to admin/dev mode).
     The whole GraphQL request fails (no partial data). Prioritize-data setting does NOT apply.
   - `data` missing -> `500` `"data" field not found in remote response`.
   - apply the result customizer (alias-aware: `f<n>` mapped) then for each argument read the
     value at path `f<n>.<hop2>...<leaf>`; missing key ->
     `500 failed to lookup key '<k>' in response`; a non-object (e.g. list or null) at an
     intermediate hop -> `500 unexpected non-object json value found while path not empty: ...`.
   Example golden (`query_with_errors_obj.yaml`):
   ```json
   {"errors":[{"extensions":{"internal":{"errors":[
      {"path":["f0","errorMsg"],"message":"intentional-error","locations":[{"line":1,"column":36}]},
      {"path":["f1","errorMsg"],"message":"intentional-error","locations":[{"line":1,"column":74}]},
      {"path":["f2","errorMsg"],"message":"intentional-error","locations":[{"line":1,"column":112}]}]},
     "path":"$","code":"unexpected"},"message":"Errors from remote server"}]}
   ```
5. The value at the leaf path for argument k replaces every token `(joinId, k)`.

### 9.5 Call to a database source (`RemoteJoin/Source.hs`, `Postgres/Instances/Execute.hs`)

For one join with arguments:

1. Rows: `[{"__argument_id__": k, "<lhsField>": <value>, ...} for each argument k]` (keys = LHS
   field names, i.e. the keys of `field_mapping`).
2. Plan a normal select (`convertRemoteSourceRelationship`) over a function source:
   ```
   FROM "pg_catalog"."jsonb_to_recordset"(($1)::jsonb)
        AS "pg_catalog.jsonb_to_recordset"("__argument_id__" bigint, "<lhsField1>" <rhs column type 1>, ...)
   ```
   (record definition: `__argument_id__ bigint` + each LHS field name typed with the **target
   column's** scalar type), and select two fields: `__argument_id__` and `f` = an object/array/
   array-aggregate relationship (named after the remote relationship) from the record to the target
   table with mapping `"<lhsField>" = <target column>`, applying the role's select permission on
   the target table (filter, column set, limit) plus the user's args (where/order_by/limit/offset/
   distinct_on), nullable. The query runs as a `QDBMultipleRows` select, producing JSON
   `[{"__argument_id__": 0, "f": <object|null|[...]|{aggregate}>}, ...]`. The rows JSON is passed
   as **one prepared parameter** of type jsonb.
   Approximate SQL (exact text is generated by the common select translator — see the SQL spec):
   ```sql
   SELECT coalesce(json_agg("root"), '[]') AS "root" FROM (
     SELECT row_to_json((SELECT "_e" FROM (SELECT "_root.base"."__argument_id__" AS "__argument_id__",
                                                "_root.ar.root.f"."f" AS "f") AS "_e")) AS "root"
     FROM (SELECT * FROM "pg_catalog"."jsonb_to_recordset"(($1)::jsonb)
             AS "pg_catalog.jsonb_to_recordset"("__argument_id__" bigint, "id" integer)
           WHERE ('true')) AS "_root.base"
     LEFT OUTER JOIN LATERAL (
        SELECT coalesce(json_agg("f" ORDER BY ...), '[]') AS "f" FROM (
           SELECT row_to_json(...) AS "f" FROM (SELECT * FROM "public"."album"
              WHERE (("_root.base"."id") = ("artist_id")) AND (<permission filter>)
              LIMIT <perm/user limit>) AS ...) ...
     ) AS "_root.ar.root.f" ON ('true')
   ) AS "_root"
   ```
   Verified pieces (`Backends/Postgres/Instances/Execute.hs:689-748`,
   `Translate/Select/Internal/Process.hs:169-176`, `SQL/DML.hs:749-763`): the function is
   `QualifiedObject "pg_catalog" "jsonb_to_recordset"` rendered `"pg_catalog"."jsonb_to_recordset"`;
   its relation alias is `functionNameToTableAlias` = `qualifiedObjectToText` =
   **`"pg_catalog.jsonb_to_recordset"`** (one quoted identifier containing a dot), and the column
   definition list is rendered `("__argument_id__" bigint, "<col>" <pgtype>, ...)` — argument id
   first, then the join columns in HashMap order; the single jsonb argument is a fresh prepared
   parameter (`UVParameter FreshVar` of `PGJSONB`). The outer `SELECT coalesce(json_agg(...))`,
   `"_root.base"` wrapper (`SELECT * FROM <fn> WHERE ('true')`), and `LEFT OUTER JOIN LATERAL ...
   ON ('true')` come from the generic translator (`defaultGenerateSQLSelect`). The select has
   `TablePerm annBoolExpTrue Nothing` and no args at the record level; permission/user args live
   inside the relationship. `stringifyNum` is the server setting (`_rsjStringifyNum`).
   It is **not** an `_in` filter: each LHS argument row is laterally joined, so per-row
   `limit`/`order_by`/aggregates are exact (test `user-limit-greater-than-permission-limit`).
   Query tags are not attached to these queries.
3. Execute on the target source's pool (read replica if configured, inferred). Parse the result:
   must be an array of objects each with `f` and `__argument_id__` (number or numeric string);
   violations -> 500 errors `cannot find relationship data (aliased as 'f') within the source response`,
   `cannot find '__argument_id__' within the source response`, `expected 'argument_id' to get parsed as backend integer type`,
   `expected an object one level deep in the remote schema's response, but found an array/scalar value instead`,
   `expected a list of objects in the remote schema's response, but found an object/scalar value instead`.
4. Join index `{argumentId -> f}`. Array relationship with no matching rows -> `[]`; object
   relationship with no match -> `null`.

The same mechanism serves table -> table, remote schema -> table, and action -> table.

### 9.6 Stitching and recursion

- After all joins of a level return, child join trees (joins inside the RHS selection) are
  processed with the list of join-result values as the new LHS (`foldJoinTreeWith` recursion),
  batching across all values of that level.
- Finally replace tokens with results (`joinResults`); a missing result for a token is a 500
  (`couldn't find results for the join with id: ...`).
- The result customizer of the target remote schema (typename mapping) is applied to RS join
  results; the alias mapping makes it apply under `f<n>`.
- Ordering: the LHS order is preserved; RHS ordering is whatever the target returns (for DB
  targets the user's `order_by` applies within each LHS row).
- Joins at the same level run **sequentially** upstream; the Rust port may run independent joins
  of a level concurrently (no observable difference except timing; ensure deterministic error
  selection — upstream reports the first failing join in join-id order).

### 9.7 Where remote joins are processed

- Query root DB fields (`ExecStepDB` + remoteJoins), mutation outputs (insert/update/delete
  `returning`, `insert_one`, function mutations), action outputs (sync, and async action query
  `output`), remote schema root fields (RS -> X relationships).
- When a query has remote joins, PG mutations are not coalesced into one transaction (each step
  with remote joins disqualifies `coalescePostgresMutations`).
- Response caching (`@cached`) caches the final joined result (EE; inferred).

---

## 10. Rust implementation guidance (non-normative)

Suggested module boundaries (SOLID-ish, each testable in isolation):

- `remote_schema::metadata` — serde types with exact field names/defaults/format-preserving enums
  (`RemoteRelationshipDefinition::{ToSource, ToSchema{format: Legacy|Unified}}`).
- `remote_schema::introspect` — HTTP fetch (reqwest/hyper client shared, per-remote timeout),
  introspection JSON -> internal `RemoteSchemaDoc` (IndexMap of types), validation.
- `remote_schema::customize` — pure functions building `TypeMap`/`FieldMap` + validation; result
  customizer as a pre-compiled tree (avoid per-request closures; precompute a "typename rewrite
  plan" per selection).
- `remote_schema::permissions` — SDL parser (graphql-parser crate or own), subset validation,
  presets.
- `graphql::schema::remote` — contributes types/fields to the per-role schema; conflict checker.
- `execute::remote` — selection normalization -> outgoing document builder + printer (byte-exact
  printer per §6.6), variable/preset resolution, header merge, response decoding.
- `execute::remote_join` — join tree, argument collection (use `serde_json::Value` with
  `preserve_order` / or a custom streaming visitor), batched calls via a `JoinTarget` trait with
  `RemoteSchemaTarget` and `SourceTarget<B: Backend>` impls, stitching.
- Keep a `Clock`/`HttpClient` trait for tests; golden tests from tests-py run against the real
  test remote servers in `server/tests-py/remote_schemas/nodejs` and `remote_server.py`.

Performance opportunities that don't change semantics: run independent root-field remote calls and
same-level joins concurrently; reuse HTTP connections (HTTP/1.1 keep-alive, HTTP/2 when offered);
avoid re-serialising the LHS JSON (upstream encodes/decodes twice); stream-parse remote responses.
Keep the observable error precedence identical (first failing root field in selection order).

---

## 11. Edge cases and quirks checklist

1. `update_remote_schema` silently drops the remote schema's own `remote_relationships`.
2. `remove_remote_schema` on an inconsistent remote -> `no such remote schema`.
3. `reload_remote_schema` turning a consistent remote inconsistent -> HTTP **500** error.
4. Remote errors at root level -> `{"data":null,"errors":[...verbatim...]}`, HTTP 200, whole
   operation aborted; remote errors inside a join -> `Errors from remote server` (`unexpected`)
   with the remote errors only in `extensions.internal`.
5. `data` + `errors` from remote -> errors win unless `HASURA_GRAPHQL_REMOTE_SCHEMA_PRIORITIZE_DATA=true`.
6. `__typename` on a namespace field calls the remote.
7. Null in any join column -> relationship `null` (even for `[T!]!` array relationships).
8. Remote relationship fields bypass LHS column select permissions for join columns.
9. Literal join values are inlined into the outgoing query (string escaping via JSON encoding).
10. Single-element list templates `["$id"]` are allowed; multi-element lists rejected.
11. Session presets with value exactly `x-hasura-` are treated as a session variable `x-hasura-`
    (almost always missing -> runtime `not-found` error).
12. Remote subscriptions are in the schema but fail at execution with `not-supported`. Remote
    relationships (to remote schemas **and** `to_source`) selected inside any subscription fail
    with `400 not-supported` `Remote relationships are not allowed in subscriptions`.
12a. Outgoing remote requests omit `operationName` / `variables` keys when absent (never `null`).
12b. `delete_remote_schema_remote_relationship` never checks existence (always success).
13. Introspection response HTTP status is ignored; only the body matters (a 500 with a valid
    GraphQL body succeeds).
14. Customization keys and permission SDL use original names; relationship `type_name` too.
15. Remote schema names are arbitrary non-empty text (spaces allowed: tests use `"simple 1"`).
16. Built-in scalars can be renamed via `type_names.mapping` (affects variable type checking).
17. Upstream iteration order of remotes for conflict detection is HashMap order; choose metadata
    order and document it.

## 12. Open questions / unverified points

- Exact Aeson error texts for malformed introspection responses (tests assert some of them).
- Exact whitespace of the jsonb_to_recordset join SQL beyond the verified pieces in §9.5 (owned by
  the SQL spec's generic select translator; no upstream golden fixture asserts this SQL).

Resolved by review (see the referenced sections): namespace field is nullable (§4.3);
`operationName`/`variables` are omitted, never `null` (§3.2, §6.4); remote joins in subscriptions are
rejected with `Remote relationships are not allowed in subscriptions` (§8.6); jsonb_to_recordset
alias/shape (§9.5); update/delete error messages (§8.2).

---

## Upstream tests to reuse

**tests-py** (`server/tests-py/`, Python harness + YAML fixtures; remote servers in
`server/tests-py/remote_schemas/nodejs/*` and `server/tests-py/remote_server.py`):

| path | covers |
|---|---|
| `test_schema_stitching.py` + `queries/remote_schemas/*.yaml` | add/update/remove/reload remote schema, introspection, basic query/mutation, timeouts, Set-Cookie passthrough, header forwarding, conflicts with tables/other remotes, interface/union validation errors (`add_remote_schema_err_*`, `*_iface_*`, `*_union_*`), JSON scalar, op-name forwarding, subscription not supported, websocket remote queries, introspection_headers |
| `queries/remote_schemas/validation/*` | customization: type prefix, field prefix, interface field consistency, namespace, multiple namespaced remotes, "customize all the things" (`__typename` mapping, aliases, fragments, variables with renamed scalars) |
| `queries/remote_schemas/basic_query_customized.yaml` | field-name customization round trip |
| `test_remote_relationships.py` + `queries/remote_schemas/remote_relationships/*` | table -> RS relationships: create valid/invalid (`setup_invalid_*`), legacy format, nested args, arrays, singleton->array, multiple fields, nested fields, null joining fields, arguments merge, variables, fragments, interface/union/enum, errors in joins (`query_with_errors_obj/arr.yaml` — printer-exact), aliased join keys, scalar relationship, deep nesting paths, computed-field LHS (incl. session), mutation output joins, rename/drop dependencies, relay exclusion, permissions, field type introspection |
| `test_remote_schema_permissions.py` + `queries/remote_schemas/permissions/**` | permission SDL validation (object/interface/union/enum/scalar/input), presets (static/session, validation errors, execution), partial args/fields, inheritance (conflicts, multiple parents, override), remote relationships with permissions, customization + permissions, update_remote_schema with permissions, unknown role. Requires `HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS=true` |
| `test_remote_schema_prioritize_data.py`, `test_remote_schema_prioritize_none.py` + `queries/remote_schemas/validate_data_errors_prioritization/**` | data-vs-errors priority flag |
| `queries/graphql_introspection/remote_relationships/*` | generated `*_remote_rel_*` input type naming |
| `test_metadata.py` (remote schema parts), `test_roles_inheritance.py` (remote schema permission inheritance) | export/replace metadata with remote schemas; inherited roles |
| `test_actions.py` (remote relationship parts) | action -> source relationships (shared join engine) |

**api-tests** (`server/lib/api-tests/src/Test/`, Haskell hspec; use the Postgres fixtures, skip
others):

| module | covers |
|---|---|
| `Schema/RemoteRelationships/XToDBArrayRelationshipSpec.hs` | PG/RS/action LHS -> PG array `to_source`: schema types (`[T!]!`, `_aggregate`), empty array, null join value, permissions (rows, columns, aggregations, limits) — skip MSSQL/BigQuery/DC/Citus/Cockroach fixtures |
| `Schema/RemoteRelationships/XToDBObjectRelationshipSpec.hs` | same for object relationships |
| `Schema/RemoteRelationships/XToRemoteSchemaRelationshipSpec.hs` | DB -> RS (`to_remote_schema` new format), null/non-null joins, schema (field description from target type, not in bool_exp/order_by), role permissions (needs perms flag) |
| `Schema/RemoteRelationships/FromRemoteSchemaSpec.hs` | RS -> RS and RS -> DB relationships, metadata API create/update/delete, union member joins (`__hasura_internal_typename`), deeply nested joins |
| `Schema/RemoteRelationships/MetadataAPI/DropSource/RSToDBRelationshipSpec.hs` | dropping a source used by an RS -> DB relationship |
| `Schema/RemoteSchemaCustomizationSpec.hs` | enums as arguments/variables with type-name customization |
| `Regression/RemoteSchemaConflictingTypesSpec.hs` | type conflict message between remote and tracked table |
| `Regression/NullRemoteRelationship8345Spec.hs` | null remote relationship regression |
| `Regression/RemoteRelationshipStringifyNum8387Spec.hs` | stringify numeric types with DB-to-DB joins |
| `Databases/Postgres/DataValidation/RemoteSourcePermissionPredicateSpec.hs` | permission predicates across remote-source relationships (owned with authz spec) |
| `API/Metadata/BulkAtomicSpec.hs` (remote parts) | bulk metadata with remote relationships |
| Skip: `Regression/SqlServerRemoteRelationshipRhs8712Spec.hs` (MSSQL), `DataConnector/MockAgent/RemoteRelationshipsSpec.hs`, `DataConnector/MockAgent/RemoteSourcePermissionPredicateSpec.hs` (Data Connector) |
