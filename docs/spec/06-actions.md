# 06 — Actions, Custom Types, Webhook Transforms and Kriti

Status: research spec for the Rust drop-in replacement of Hasura GraphQL Engine V2.
Ground truth: Haskell source in `server/` (paths below are relative to the
graphql-engine repo root). Kriti behaviour is taken from the pinned
`kriti-lang` dependency (`cabal.project`: `https://github.com/hasura/kriti-lang.git`,
tag `c8fd863d359876af3a0fce47db8a5393dc556667`, version 0.3.3), which is not
vendored in the repo; its sources were fetched separately for this spec.
(Reviewer check: the clone used was
HEAD = `c8fd863` "Support Aeson 2.1 (#78)", `kriti-lang.cabal` version 0.3.3,
clean working tree — i.e. exactly the pinned commit. The upstream repo also has
a small Hasura-side wrapper, `server/lib/hasura-extras/src/Kriti/Extended.hs`,
which only walks the AST and does not change semantics.)
Anything marked **(inferred)** was deduced rather than verified against code or
a golden test.

Primary source files:

| Concern | File |
|---|---|
| Metadata types | `server/src-lib/Hasura/RQL/Types/Action.hs`, `server/src-lib/Hasura/RQL/Types/CustomTypes.hs` |
| Metadata API / resolution | `server/src-lib/Hasura/RQL/DDL/Action.hs`, `server/src-lib/Hasura/RQL/DDL/CustomTypes.hs` |
| Schema cache build | `server/src-lib/Hasura/RQL/DDL/Schema/Cache.hs` (`buildActions`) |
| GraphQL schema | `server/src-lib/Hasura/GraphQL/Schema/Action.hs`, `server/src-lib/Hasura/GraphQL/Schema/Postgres.hs` |
| Execution | `server/src-lib/Hasura/GraphQL/Execute/Action.hs`, `.../Execute/Action/Types.hs`, `.../Execute/Action/Subscription.hs` |
| Remote joins from actions | `server/src-lib/Hasura/GraphQL/Execute/RemoteJoin/Collect.hs` |
| Transforms | `server/src-lib/Hasura/RQL/Types/Webhook/Transform*.hs`, `server/src-lib/Hasura/RQL/DDL/Webhook/Transform*.hs` |
| Kriti glue | `server/src-lib/Data/Aeson/Kriti/Functions.hs` |
| Test transform API | `server/src-lib/Hasura/RQL/DDL/Metadata.hs` (`runTestWebhookTransform`), `.../Metadata/Types.hs` |
| Async log DDL | `server/src-rsr/initialise.sql` |
| Export format | `server/src-lib/Hasura/RQL/Types/Metadata/Serialization.hs` |

---

## Scope and priority

**Essential (typical projects use these):**

- `actions` + `custom_types` metadata (JSON form inside `replace_metadata` /
  `export_metadata`, and the metadata API commands `create_action`,
  `update_action`, `drop_action`, `create_action_permission`,
  `drop_action_permission`, `set_custom_types`).
- Synchronous query and mutation actions: GraphQL schema generation, argument
  parsing into the `input` payload, the webhook POST, header handling,
  timeouts, response validation, error propagation.
- `{{ENV_VAR}}` templating of `handler`, `headers[].value`, and `value_from_env`.
- Action permissions (boolean per role) including inherited roles.
- Custom-type relationships from action output objects to Postgres tables
  (executed through the generic remote-join machinery).
- Async actions: `hdb_catalog.hdb_action_log`, the background processor,
  the `<action>(id: uuid!)` query, and the subscription on it.
- Request/response transforms (REST connectors) with the Kriti template
  language. Many real projects use these, so Kriti has to be implemented in
  full: lexer, parser, evaluator, standard functions, and error JSON.
- `test_webhook_transform` metadata API (console uses it).

**Rarely used / low priority:**

- `ignored_client_headers` customisation.
- `HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE`.
- Action handler logs (`action-handler-log`) and redaction.
- Reading a CLI metadata *directory* (`actions.yaml` + `actions.graphql`)
  directly. The server never does this: the Go CLI converts the directory
  into JSON metadata. Supporting it is an optional convenience (§3.8).
- Exact Kriti error source positions.

**Enterprise / Cloud only (out of scope for OSS/CE parity):**

- The "async action logs clean up" UI and steps are documented as EE/Cloud.
  The table itself, and deleting from it, are OSS behaviour.
- OpenTelemetry trace propagation through async actions. W3C/B3 headers are
  stored in `request_headers`; see §9.2. OSS keeps the tracing hooks, but the
  exporter is EE. Implement as optional.
- Prometheus counters `pmActionBytesSent` / `pmActionBytesReceived` exist in
  the code paths. The Prometheus endpoint is EE.

Postgres-only: async actions with relationships are Postgres-only in V2
(non-Postgres scalars give `encountered non-Postgres scalar in async query actions`).

---

## 1. Concepts and architecture overview

An *action* is a GraphQL root field backed by an HTTP webhook (the *handler*).

- **Query action** (`definition.type: query`): a field on `query_root`. Runs
  synchronously.
- **Synchronous mutation action** (`type: mutation`, `kind: synchronous`, the
  default): a field on `mutation_root`. Runs synchronously.
- **Asynchronous mutation action** (`kind: asynchronous`):
  - the mutation field returns a `uuid!` immediately after inserting a row into
    `hdb_catalog.hdb_action_log` in the **metadata database**;
  - a background poller calls the webhook later and stores the result;
  - a query field and a subscription field `<action>(id: uuid!)` read the result.

Inputs and outputs are typed by **custom types** (`custom_types` in metadata):
input objects, objects (which can have relationships to Postgres tables),
scalars and enums. They can also use built-in scalars (`Int`, `Float`,
`String`, `Boolean`, `ID`) and **any Postgres scalar type** known to a
Postgres source (for example `uuid`, `timestamptz`, `jsonb`, `geography`).

Execution pipeline of a sync action:

1. Parse args into a JSON `input` object.
2. Build the payload `{action:{name}, input, session_variables, request_query}`.
3. Build the HTTP request: POST to the resolved handler, with default,
   configured and forwarded headers, and the timeout.
4. Optionally apply the request transform (Kriti).
5. Send the request.
6. Optionally apply the response transform (Kriti).
7. Branch on the status class: 2xx means validate and build the output; 4xx
   means propagate the error; anything else is an internal error.
8. Run relationship (remote) joins to tables.

---

## 2. Metadata JSON representation

### 2.1 `ActionMetadata` (element of top-level `actions` array)

```json
{
  "name": "create_user",               // required, GraphQL Name ([_A-Za-z][_0-9A-Za-z]*)
  "definition": { ... },               // required, ActionDefinition
  "comment": "optional text",          // optional; becomes the GraphQL field description
  "permissions": [                     // optional, default []
    { "role": "user", "comment": "optional" }
  ]
}
```

Source: `server/src-lib/Hasura/RQL/Types/Action.hs` (`ActionMetadata`, `ActionPermissionMetadata`).

The `FromJSON` instance is lenient: unknown keys are ignored. The autodocodec
codec is used for the OpenAPI schema. Accept unknown keys.

### 2.2 `ActionDefinition`

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `type` | `"query"` \| `"mutation"` | no | `"mutation"` | Any other value fails with `expected mutation or query, but found <t>` |
| `kind` | `"synchronous"` \| `"asynchronous"` | no | `"synchronous"` | **Only read when `type` = `mutation`**. Ignored for `query`, even if set to `asynchronous` or `""` (the CLI writes `kind: ""` for query actions; this must be accepted). For a mutation, `null`/absent means synchronous; any other string (including `""`) is a parse error |
| `arguments` | `[ArgumentDefinition]` | no | `[]` | |
| `output_type` | GraphQL type string | **yes** | | Parsed as a GraphQL type (`Foo`, `Foo!`, `[Foo!]!`, `[[Foo]]`). Error: `not a valid GraphQL type: <t>` |
| `handler` | URL template string | **yes** | | `{{ENV}}` interpolation, see §2.3. Parse error prefix `Parsing URL template failed: ` |
| `headers` | `[HeaderConf]` | no | `[]` | §2.4 |
| `forward_client_headers` | bool | no | `false` | |
| `ignored_client_headers` | `[string]` | no | the list below | Header names (case-insensitive) not forwarded when `forward_client_headers` is true |
| `timeout` | non-negative integer (seconds) | no | `30` (`defaultActionTimeoutSecs`, `server/src-lib/Hasura/RQL/Types/Common.hs`) | Errors: `timeout value cannot be negative`, `<n> is out of bounds` (also for non-integral numbers such as `1.5`, via `toBoundedInteger`). `0` is accepted and becomes `responseTimeoutMicro 0` (inferred: every call times out immediately) |
| `request_transform` | RequestTransform | no | none | §10 |
| `response_transform` | ResponseTransform | no | none | §10 |

Default `ignored_client_headers` (`server/src-lib/Hasura/Authentication/Headers.hs`
`commonClientHeadersIgnored`), compared case-insensitively:

```
Content-Length, Content-MD5, User-Agent, Host, Origin, Referer, Accept,
Accept-Encoding, Accept-Language, Accept-Datetime, Cache-Control, Connection,
DNT, Content-Type
```

`ArgumentDefinition`:

```json
{ "name": "email", "type": "String!", "description": "optional" }
```

`name` must be a GraphQL Name. `type` is a GraphQL type string.

Internal `ActionType` representation: `ActionQuery | ActionMutation (Synchronous|Asynchronous)`.
Its `ToJSON` (used in logs as `action_type`) renders `"query"`,
`"mutation-synchronous"` or `"mutation-asynchronous"`.

### 2.3 Handler URL templating (`InputWebhook`)

The handler is a `Data.URL.Template` (`server/lib/hasura-extras/src/Data/URL/Template.hs`).

- Syntax: the literal text, with `{{VAR}}` segments. The variable name is
  everything between `{{` and the next `}}`. It is **not trimmed**:
  `{{ FOO }}` looks up env var `" FOO "`.
- Parsing (`parseTemplate`: `many (variable <|> textUntil "{{")` then
  `takeText`) never fails for well-formed input. A `{{` without a closing
  `}}` is **not** handled cleanly: the variable alternative fails and the
  text alternative succeeds consuming nothing at the `{{`, so attoparsec's
  `many` would loop (inferred: V2 most likely hangs or never accepts such a
  handler). The Rust engine should reject it with
  `Parsing URL template failed: …` rather than reproduce this.
- Resolution happens at **schema-cache build time**, from the server's process
  environment (`resolveWebhook`). If any variables are missing the action
  becomes **inconsistent** with the error
  `Value for environment variables not found: VAR1, VAR2` (code `unexpected`,
  status 400), wrapped as described in §5.
- The *unresolved* template text (for example `{{ACTION_BASE}}/create-user`)
  is kept as the "env name" of the webhook (`EnvRecord`). It appears as
  `request.url` in internal error details, so secrets in env vars do not leak.
  The resolved URL is used for the actual request and for the Kriti
  `$base_url`.

### 2.4 Headers (`HeaderConf`)

Source: `server/src-lib/Hasura/RQL/Types/Headers.hs`, `server/src-lib/Hasura/RQL/DDL/Headers.hs`.

```json
{ "name": "X-Secret", "value": "static-or-{{ENV}}-templated" }
{ "name": "X-Secret", "value_from_env": "MY_SECRET_ENV" }
```

- Exactly one of `value` / `value_from_env` must be present.
  - Neither: error `expecting value or value_from_env keys`.
  - Both: error `expecting only one of value or value_from_env keys`.
  - Non-object: error `expecting object for headers`.
- `value_from_env` starting with `HASURA_GRAPHQL_` is rejected at parse time:
  `env variables starting with "HASURA_GRAPHQL_" are not allowed in value_from_env: <VAR>`.
  This is a `parse-failed` error at path `$.args.definition.headers[i]`, on `/v1/query` too (v2.51.0 recording; upstream's golden still says `$.definition...`). Golden:
  `tests-py/queries/actions/metadata/create_with_headers.yaml`.
- `value` is itself an env template (`{{VAR}}` interpolation).
- Header values are resolved **per request** (`makeHeadersFromConf`), not at
  build time. Errors at request time:
  - missing template var: `template cannot be resolved: <vars>` (code `not-found`, 400);
  - missing `value_from_env` var: `environment variable '<VAR>' not set` (code `not-found`, 400).

### 2.5 Permissions

- An action is visible to `admin` always, and to role `r` iff `r` has an
  entry in `permissions`, or `r` is an inherited role and **any** parent role
  has access (boolean union; `mkBooleanPermissionMap` in
  `server/src-lib/Hasura/RQL/DDL/Schema/Cache.hs`, Note "Actions and Custom
  function permissions").
- An explicit permission on an inherited role is allowed. It just makes the
  role have access.
- There is no row or column filtering for actions. The handler gets the
  session variables and enforces its own authorisation.
- If a role lacks permission, the field is absent from that role's schema.
  The query fails GraphQL validation (for example `no mutations exist` or
  `field 'x' not found in type: 'query_root'`).
- For async actions, the permission covers the mutation field, the query field
  and the subscription field.

### 2.6 Export serialization (`export_metadata`)

From `actionMetadataToOrdJSONList` in
`server/src-lib/Hasura/RQL/Types/Metadata/Serialization.hs`:

- Actions are **sorted by name**.
- Action object keys in order: `name`, `definition`, then `comment` (if
  present), then `permissions` (if non-empty).
- Definition keys in order:
  1. `handler` (template text as written);
  2. `output_type`;
  3. `forward_client_headers` (**only if true**);
  4. `ignored_client_headers` (**always emitted**, even when it is the default list);
  5. `headers` (if non-empty);
  6. `arguments` (if non-empty; each `{name, type[, description]}`);
  7. `request_transform` (if present);
  8. `response_transform` (if present);
  9. `type`, then `kind` (only for mutations);
  10. `timeout` (**only if ≠ 30**).
- Permission: `{role[, comment]}`.
- `custom_types` is omitted entirely when all four lists are empty. Otherwise
  it has keys `input_objects`, `objects`, `scalars`, `enums`, each present
  only if non-empty. Each type is `name`, `fields`/`values` and optional
  `description`. Object fields are `{name, type[, arguments][, description]}`.
  Objects carry `relationships` if non-empty.
- In the metadata-inconsistency `definition` blob, the action uses
  `ToJSON CreateAction`/`ActionDefinition`. That form *does not* include
  `ignored_client_headers`, and always includes `arguments`, `headers`,
  `forward_client_headers`, `timeout`, `kind` (for mutations) and `type`.
  See `tests-py/queries/actions/nested-relation/create_async_action_with_nested_output_and_relation.yaml`.

### 2.7 Transform JSON round trip

See §10.1. Note the `ToJSON` of `RequestTransform` always emits `version` and
`template_engine`.

### 2.8 CLI metadata directory: `actions.yaml` + `actions.graphql` (optional feature)

The server never reads these files. `hasura metadata apply` (Go CLI,
`cli/internal/metadataobject/actions/actions.go`) uses `cli-ext`
(`cli-ext/src/shared/utils/sdlUtils.js`) to convert them into the JSON
`actions` / `custom_types` metadata. If the Rust project offers a "load
metadata directory" mode, reproduce this merge.

`actions.graphql` (GraphQL SDL), parsed with graphql-js:

- Definitions named `Query` or `Mutation`, either `type Query {...}` or
  `extend type Query {...}` (the exporter emits `extend type`), hold the
  actions. Each field is one action:
  - field name → action `name`;
  - field type → `output_type` (printed with `!` and `[]` wrappers);
  - field args → `arguments` (`name`, `type`, `description` from the arg
    docstring, trimmed);
  - `type` = `query` for `Query`, `mutation` for `Mutation`.
- All other definitions become custom types:
  - `scalar` → `scalars` (`name`, `description`);
  - `enum` → `enums` (`values: [{value, description}]`);
  - `input` → `input_objects` (`fields: [{name, type, description}]`);
  - `type` → `objects` (`fields: [{name, type, description}]`).
  - `schema` definitions and `interface` definitions are errors.

`actions.yaml`:

```yaml
actions:
  - name: greet
    definition:
      kind: synchronous          # '' for query actions
      handler: '{{ACTION_BASE_URL}}/greet'
      forward_client_headers: true
      headers:
        - name: Content-Type
          value: application/json
      timeout: 60                 # omitted when 0/default
      request_transform: {...}
      response_transform: {...}
    permissions:
      - role: user
    comment: optional
custom_types:
  enums: []
  input_objects:
    - name: SampleInput
  objects:
    - name: SampleOutput
      relationships: [...]
  scalars: []
```

Merge rules (`Build()`):

- For each action in YAML, find the same-named action from the SDL. If none
  exists, fail with `action <name> is not present in actions.graphql`. Then
  copy from YAML: `permissions`, `comment`, `timeout`, `kind`, `handler`,
  `forward_client_headers`, `headers`, `request_transform`,
  `response_transform`.
- `type`, `arguments` and `output_type` come from the SDL.
- **`ignored_client_headers` is not carried by the CLI** (the field is absent
  from its struct). (inferred: projects managed through the CLI always end up
  with the default list.)
- For each custom type in YAML, find the same-named SDL type. If none exists,
  fail with `custom type <name> is not present in actions.graphql`. Then copy
  `description` and `relationships` **from the YAML** (the YAML description
  overrides the SDL docstring of the type). Field descriptions come from the SDL.
- An action present in SDL but absent from YAML is still emitted, without a
  handler. (inferred: the server then rejects it because `handler` is required.)

---

## 3. Custom types

### 3.1 JSON shape (`set_custom_types` args / `custom_types` metadata)

```json
{
  "input_objects": [
    { "name": "UserInput", "description": "opt",
      "fields": [ { "name": "email", "type": "String!", "description": "opt" } ] }
  ],
  "objects": [
    { "name": "UserId", "description": "opt",
      "fields": [ { "name": "id", "type": "Int!", "description": "opt", "arguments": null } ],
      "relationships": [
        { "name": "user", "type": "object",            // "object" | "array"
          "source": "default",                         // optional, default "default"
          "remote_table": "user",                      // QualifiedTable: "name" or {"schema","name"}
          "field_mapping": { "id": "id" } }             // object field name -> table column
      ] }
  ],
  "scalars": [ { "name": "myCustomScalar", "description": "opt" } ],
  "enums": [
    { "name": "Color", "description": "opt",
      "values": [ { "value": "RED", "description": "opt", "is_deprecated": false } ] }
  ]
}
```

- All four lists are optional, defaulting to `[]`.
- `fields` and `values` are non-empty lists. (inferred from `NonEmpty`; an
  empty list is a parse error.)
- Object field `arguments` may be present in JSON. If it is non-null the type
  is rejected (§3.2).
- `is_deprecated` on enum values is parsed but **ignored** in the generated
  schema. No `@deprecated` is emitted.
- `remote_table` accepts a string (schema `public`) or `{schema, name}`.

### 3.2 Validation (`server/src-lib/Hasura/RQL/DDL/CustomTypes.hs`)

Validation runs at schema-cache build time.

- If custom types are invalid, the object `custom_types` is
  **inconsistent** (name `custom_types custom_types`). Every action, whether or
  not it uses the broken types, is then also marked inconsistent with reason
  `custom types are inconsistent` (no `in action "…"; ` prefix; verified against
  v2.51.0). The `definition` shown for the custom types is their `ToJSON`: all
  four lists, every optional field as `null`, relationship `source` filled in and
  `remote_table` as `{"schema", "name"}`.
- Error message: `validation for the given custom types failed because <reason>`
  when there is one error. With several errors it is
  `validation for the given custom types failed for the following reasons:\n  • <r1>\n  • <r2>\n`.
- Code `constraint-violation` (400). When surfaced through a metadata API call
  the outer error is `Inconsistent object: <message>` with code
  `invalid-configuration`.

Reasons. Names are rendered in double quotes; `dquoteList` renders
`"a", "b"`.

| Check | Message |
|---|---|
| Type name unique across scalars, enums, input objects and objects | `duplicate type names: "A", "B"` |
| Input-object field base type must be a scalar (custom or default), enum, input object or a backend (PG) scalar | `the type "T" for field "f" in  input object type "I" does not exist` (note the double space) |
| Duplicate input-object field names | `the input object "I" has duplicate fields: "f"` |
| Object field base type must be a custom/default scalar, enum, **object**, or PG scalar (input objects are not allowed) | `the type "T" for field "f" in  object type "O" does not exist` |
| Duplicate names across object fields + relationship names | `the object "O" has duplicate fields: "f"` |
| Object field `arguments` present | `the object "O" can't have arguments` |
| All relationships of one object must use one source | `the object "O" has relationships refers to tables in multiple sources` |
| Relationship source/table exists | `the remote table "user" for relationship "r" of object type "O" does not exist` (table rendered with `qualifiedObjectToText`: `schema.name`, but just `name` in `public`) |
| `field_mapping` key is an object field | `the field "f" for relationship "r" in object type "O" does not exist` |
| Mapped field is not a list type | `the type of the field "f" for relationship "r" in object type "O" is a list type` |
| Mapped column exists in the table | `the column "c" of remote table "user" for relationship "r" of object type "O" does not exist` (same table rendering) |
| Enum values unique | `the enum type "E" has duplicate values: "V"` |

Golden tests: `tests-py/queries/actions/custom-types/*.yaml`. Note the double
space `in  object type` in the expected message.

Built-in scalars `Int`, `Float`, `String`, `Boolean`, `ID` are always
implicitly available. Redefining them as custom scalars is allowed:
`scalarTypes = custom <> defaults`, so a user definition with the same name
wins.

PG scalar reuse (Note "Postgres scalars in custom types"):

- A base type name that is not a custom type is looked up in the union of all
  sources' scalar maps. For Postgres these are the base types from
  `pg_catalog.pg_type`, keyed by type name (for example `uuid`,
  `timestamptz`, `geography`, `jsonb`, `citext`).
- With several sources the first match wins.
- Such scalars are emitted into the GraphQL schema as scalars even if no table
  uses them.
- Input values of PG scalars are **validated** by the backend scalar parser
  (for example an invalid uuid literal is rejected before calling the
  webhook), but forwarded as the original JSON value.

### 3.3 Object relationships (to tables)

- `type: object` gives a nullable single object field of the table's GraphQL
  type. `type: array` gives a list field with the table's usual
  `where`/`order_by`/`limit`/`offset`/`distinct_on` args.
  (inferred: these come from the generic remote-source-relationship field
  builder, the same as db-to-db remote relationships.)
- The relationship field is present only if the current role can select from
  the target table (normal select permissions apply, including row filters).
  Golden: `tests-py/queries/actions/sync/query_action_relationship_with_permission.yaml`.
- Execution is described in §8.7 (sync) and §9.6 (async).

---

## 4. Resolving an action (schema-cache build)

Source: `resolveAction` in `server/src-lib/Hasura/RQL/DDL/Action.hs`.

Errors make the action **inconsistent**. The message is prefixed
`in action "<name>"; ` (`buildActions` in Cache.hs). Via the metadata API you
get `Inconsistent object: in action "<name>"; <msg>`, code
`invalid-configuration`, with `internal` listing the inconsistent objects.

1. **Arguments**: the base type of each argument must be one of:
   1. a backend (PG) scalar, checked **first**;
   2. a custom/default scalar, enum, or input object.

   Otherwise: `the type: "T" is not defined in custom types or it is not a scalar/enum/input_object`
   (code `invalid-params`). Object types are not allowed as arguments.
2. **Output type**: the base type must be one of:
   1. a backend (PG) scalar, which gives a scalar output;
   2. a custom object, which gives an object output;
   3. a custom/default scalar, which gives a scalar output.

   Enums and input objects are **not** allowed as output. Otherwise:
   `the type: "T" is not an object or scalar type defined in custom types`
   (code `not-exists`).
3. **Relationship shape checks**:
   - Sync (query or sync mutation): every relationship of the **top-level**
     output object must map only fields that are scalars or enums of that
     object. Otherwise: `Relationships cannot be defined with nested object fields: "rel1", "rel2"`
     (code `constraint-error`).
   - Async: if the output object has **both** relationships and at least one
     field whose type is an object type, fail with
     `Async action relations cannot be used with object fields: "f1", "f2"`
     (code `constraint-error`).
   - Relationships on *nested* object types are allowed for sync actions.
     Golden: `nested-relation/create_sync_action_with_nested_output_and_nested_relation.yaml`.
4. **Handler resolution** (§2.3).

---

## 5. Metadata API commands

These are accepted on `/v1/metadata`, and on legacy `/v1/query` too (tests
use `/v1/query`). Success response: `{"message":"success"}`.

| Command | Args | Behaviour / errors |
|---|---|---|
| `create_action` | `{name, definition, comment?}` | If the name exists in the schema cache: `action with name "x" already exists` (`already-exists`). Adds an action with empty `permissions`. Rebuilds the cache; an inconsistency fails the call. |
| `update_action` | `{name, definition, comment?}` | Missing action: `action with name "x" does not exist` (`not-exists`). **Replaces** definition and comment (an omitted comment clears it). Keeps permissions. |
| `drop_action` | `{name, clear_data?: bool}` | Missing action: `action with name "x" does not exist`. Removes it from metadata. If `clear_data` is absent or `true` (**default true**, though a misleading source comment says otherwise), runs `DELETE FROM hdb_catalog.hdb_action_log WHERE action_name = $1` after the rebuild. |
| `create_action_permission` | `{action, role, definition?: any, comment?}` | `definition` is accepted and ignored. Existing permission: `permission for role "r" is already defined on "x"` (`already-exists`). The new permission is **prepended** to the list. A missing action does not error explicitly. (inferred: `ix` is a no-op, so the call succeeds silently.) |
| `drop_action_permission` | `{action, role}` | Missing permission: `permission for role: "r" is not defined on "x"` (`not-exists`). |
| `set_custom_types` | CustomTypes object | Replaces all custom types; `{}` clears them. Then rebuilds. |

Interaction with `replace_metadata`: actions and custom types are simply part
of the metadata document. Ordering inside `actions` does not matter, since the
export is sorted.

---

## 6. GraphQL schema exposure

Source: `server/src-lib/Hasura/GraphQL/Schema/Action.hs`, `.../Schema/Postgres.hs`
(`buildActionQueryFields`, `buildActionMutationFields`, `buildActionSubscriptionFields`).

### 6.1 Root placement

| Action | `query_root` | `mutation_root` | `subscription_root` | Relay (`/v1beta1/relay`) |
|---|---|---|---|---|
| `type: query` | `name(args): <output_type>` | – | – | – |
| `type: mutation`, `kind: synchronous` | – | `name(args): <output_type>` | – | mutation root (same field) |
| `type: mutation`, `kind: asynchronous` | `name(id: uuid!): name` (async result object) | `name(args): uuid!` | `name(id: uuid!): name` | mutation root only |

The root field description is the action `comment`.

Action fields go through the same root-field conflict checks as tables and
remote schemas (see the schema spec).

### 6.2 Arguments → `input` JSON

Source: `actionInputArguments`. Field arguments are the action `arguments`, in
order, with their descriptions.

- A nullable argument is optional. If omitted (literal or variable not
  provided), it is **absent** from `input`. If passed as explicit `null`, it is
  present with value `null`.
- A non-null argument is required (standard GraphQL validation).
- Value encoding in `input`:
  - `Int` → JSON number; `Float` → JSON number; `String` → string;
    `Boolean` → bool.
  - `ID` → JSON string (integer literals are converted to their string form).
    (inferred from `P.identifier`.)
  - Custom scalar (not built-in) → **any JSON value** is accepted (GraphQL
    literal converted to JSON, or the variable value as-is).
  - Enum → JSON string of the enum value name.
  - PG scalar (reused) → the original JSON value, after validation by the
    backend parser.
  - Input object → JSON object containing only the fields provided. Nested
    input objects are recursive, and recursive input types are supported.
  - Lists → JSON arrays, with an element parser per wrapper level.
- `input` is `{}` when the action has no arguments.

### 6.3 Output types (sync actions and the async `output` field)

- **Object output**: the GraphQL object type named after the custom object,
  with the custom object's description. Fields:
  - scalar and enum fields with their declared types;
  - nested object fields (recursive, memoized; self-recursive types are
    allowed — golden `query_action_recursive_output.yaml`);
  - relationship fields (§3.3).
- The output field's nullability and list wrappers follow `output_type`
  exactly. For example `[SampleOutput!]` gives `LIST` → `NON_NULL` → `OBJECT`.
  Golden: `tests-py/queries/actions/introspection/output_types_query.yaml`.
- **Scalar output** (custom scalar, built-in scalar, or PG scalar): the field
  is a leaf with the declared wrapping. Custom (non-built-in) scalars are
  JSON-valued.
- `__typename` is allowed anywhere. It resolves to the object type name, and
  for the async root object to the action name.

### 6.4 Async result type

For an async action `create_user` whose output is an object:

```graphql
"""fields of action: "create_user\""""            # description: fields of action: "create_user"
type create_user {
  "the unique id of an action"
  id: uuid!
  "the time at which this action was created"
  created_at: timestamptz!
  "errors related to the invocation"
  errors: json
  "the output fields of this action"
  output: UserId        # nullability/list-ness = output_type of the action
}
# query_root / subscription_root:
create_user("the unique id of an action" id: uuid!): create_user
# mutation_root:
create_user(email: String!, name: String!): uuid!
```

- The scalar names `uuid`, `timestamptz` and `json` are used regardless of
  naming convention (Postgres-flavoured names are hard-coded).
- The async query root field is nullable. (inferred: `P.subselection` without
  a non-null wrapper.)
- **Scalar output async actions**: the query field `name(id: uuid!)` returns
  the scalar type directly, with no `id/created_at/errors/output` wrapper
  object. At execution its result is always `{}`; see Quirk Q7.

---

## 7. The webhook request (sync actions and async processor)

Source: `callWebhook` in `server/src-lib/Hasura/GraphQL/Execute/Action.hs`.

### 7.1 Payload

```json
{
  "action": { "name": "create_user" },
  "input": { "email": "clarke@hasura.io", "name": "Clarke" },
  "request_query": "mutation {\n  create_user(email: \"clarke@hasura.io\", name: \"Clarke\"){\n    id\n  }\n}\n",
  "session_variables": { "x-hasura-role": "admin" }
}
```

- `session_variables` holds all session variables of the request, with
  lower-cased keys and string values (including `x-hasura-role`).
- `request_query` is the **verbatim** `query` string of the GraphQL request
  (the whole document, not just this field). It is `null` for async actions,
  since the processor passes `Nothing`.
- Key order on the wire: Hasura serialises through aeson `Value`, so object
  keys come out **sorted lexicographically** (`action`, `input`,
  `request_query`, `session_variables`). Keys inside `input` are sorted too.
  (aeson 2.3 with ordered-keymap = `Data.Map`.) Byte-level order is not
  semantically required, but matching it is cheap.

### 7.2 Method, URL, timeout

- `POST` to the resolved handler URL.
- `responseTimeout` = `timeout` seconds. (http-client response timeout: time
  until the response *headers* arrive; inferred.)
- Connect behaviour follows the shared HTTP manager (TLS allowlist etc.;
  see the HTTP/networking spec).
- An invalid handler URL makes `mkRequestThrow` throw. (inferred: this surfaces
  as a 500 `unexpected` error.)
- Redirects: http-client defaults to following up to 10 redirects. (inferred)

### 7.3 Headers

```
defaultHeaders  = [("Content-Type","application/json"),
                   ("User-Agent","hasura-graphql-engine/<version>")]
confHeaders     = action.headers resolved from env (per request)
clientHeaders   = if forward_client_headers
                    then mkClientHeadersForward(ignored_client_headers, requestHeaders)
                    else []
mkClientHeadersForward ignored hs =
     [("X-Forwarded-Host",v)       | ("Host",v)       <- hs]
  ++ [("X-Forwarded-User-Agent",v) | ("User-Agent",v) <- hs]
  ++ [("X-Forwarded-Origin",v)     | ("Origin",v)     <- hs]
  ++ [ h | h <- hs, name(h) ∉ ignored (case-insensitive),
            not (lower(name(h)) startsWith "x-hasura-") ]

hdrs = case HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE of
  false (default, ClientHeadersFirst) -> uniqOnName (clientHeaders ++ defaultHeaders ++ confHeaders)
  true  (ConfiguredHeadersFirst)      -> uniqOnName (confHeaders ++ defaultHeaders ++ clientHeaders)
```

- `uniqOnName` keeps the **first** occurrence per (case-insensitive) header
  name. This also collapses repeated client headers, such as multiple
  `Cookie` lines, to the first one.
- By default a configured header **cannot** override a forwarded client
  header of the same name, and neither a client nor a configured
  `Content-Type`/`User-Agent` can override the defaults: `Content-Type` and
  `User-Agent` are in the default ignore list, so the defaults come before
  configured headers. A configured `Content-Type` is therefore dropped unless
  precedence is flipped. (Verified from code.)
- All `x-hasura-*` request headers, including `x-hasura-admin-secret`, are
  never forwarded. The docs claim "resolved user (x-hasura-*) variables" take
  part in precedence; the code does not send them as headers.
- `User-Agent` value: `hasura-graphql-engine/<version>`. Golden
  `test_mirror_headers` asserts the prefix `hasura-graphql-engine/`.
- Async actions: the "client headers" are the request headers stored in
  `hdb_action_log.request_headers` at mutation time (all headers, with trace
  headers prepended and later stripped; §9.2).

### 7.4 Request transform

If `request_transform` is set, it is applied after the request above is
built. The transform sees the original body, URL and query params (§10). On
failure:

- the error bundle is logged at error level;
- the call fails with `throw500WithDetail "Request Transformation Failed" <bundle JSON>`
  (code `unexpected`, status 500; `internal` = the error array).

---

## 8. Response handling (sync)

### 8.1 Pipeline

```
resp <- http(req)   -- exception -> internal error "http exception when calling webhook"
body' <- if response_transform then applyResponseTransform(resp) else resp.body
         -- failure -> 500 "Response Transformation Failed" (internal = bundle)
log ActionHandlerLog
case decodeJSON(body') of
  Left e  -> internal error "not a valid json response from webhook"
  Right v ->
    | 200 <= status < 300 -> validateResponse(v); build output; collect Set-Cookie
    | 400 <= status < 500 -> parse {message, code?, extensions?}; raise as GraphQL error
    | otherwise           -> internal error "internal error"  (detail: "expecting 2xx or 4xx status code, but found <n>")
```

Notes:

- The response transform runs on **every** status, before status branching.
  4xx error bodies are transformed too.
- An empty body (for example `204`) is invalid JSON, so it gives
  `not a valid json response from webhook`.
- `1xx` and `3xx` final statuses fall into "otherwise".

### 8.2 "Internal" errors and the `internal` extension

Hasura distinguishes `IncludeInternalErrors` and `HideInternalErrors`
(`shouldIncludeInternal role responseErrorsConfig`):

- Internal details are **included for admin** when
  `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS` is true (default true).
- They are **included for everyone** in dev mode.
- They are hidden otherwise.
- The async processor always uses `HideInternalErrors`.

`throwInternalError msg`:

- Hidden: `QErr{status 500, code "unexpected", error msg}`.
- Included: the same, plus `internal` = `ActionInternalError`:

```json
{
  "error": <json: detail>,
  "request": {
    "url": "<handler template text, e.g. {{ACTION_BASE}}/create-user>",
    "action": {"name": "create_user"},
    "input": {...},
    "request_query": "<query or null>",
    "transformed_request": <HTTP request JSON or null>
  },
  "response": {                           // absent for http exceptions
    "status": 200,
    "body": <json, or string with raw body if invalid JSON>,
    "headers": [ {"name": "Content-Type", "value": "application/json"}, ... ]
  }
}
```

**Discrepancy:** the `tests-py` fixtures (for example
`tests-py/queries/actions/sync/expecting_array_response.yaml`) expect
`request` = `{body: <payload>, url, headers: [], transformed_request}`.
Current source (`ActionRequestInfo`, generic ToJSON with `aesonDrop 6
snakeCase`) produces `{url, action, input, request_query, transformed_request}`.
**Follow the source.** Treat `internal` as non-contractual when porting those
golden files (strip `extensions.internal` before comparing).

Transformed-request JSON (`ToJSON Client.Request`,
`server/lib/hasura-extras/src/Network/HTTP/Client/Transformable.hs`):

```json
{"url": "...", "method": "POST", "headers": [["Name","value"],...],
 "body": "<utf8 body or null>", "query_string": "?a=b", "response_timeout": "<micros>"}
```

`error` detail values per failure:

| Failure | message | `internal.error` |
|---|---|---|
| HTTP exception (timeout, connection refused, …) | `http exception when calling webhook` | `{"type":"http_exception","message":"<e.g. Response timeout / Connection failure: …>","request":{host,port,secure,path,queryString,method,responseTimeout}[,"status":n]}`, or `{"type":"invalid_url","message":…}` |
| Body not JSON | `not a valid json response from webhook` | `"invalid JSON: <aeson error>"`, with `response.body` = raw text as a JSON string |
| Status not 2xx/4xx | `internal error` | `"expecting 2xx or 4xx status code, but found 500"` |
| 2xx but validation failed | message from §8.3, **status 400**, code `unexpected` | `internal` = ActionInternalError with `error: "unexpected response"` (only when internal is included) |

### 8.3 Output validation (2xx) — exact messages

Source: `validateResponse`, `validateResponseObject`. All are
`throw400 Unexpected` (code `unexpected`).

Let `T` = the action's `output_type` and `F` = the top-level object's field
map (empty for scalar outputs).

```
validate(v, T):
  if isCustomScalar(T, F): accept             # see below; T must be a named type
  match (v, T):
    (null, _)            -> if T non-null: "got null for the action webhook response"
    (number, Named n)    -> n ∈ {Int, Float} else
                            "got scalar String for the action webhook response, expecting <n>"   # sic: says String
    (bool, Named n)      -> n == Boolean else
                            "got scalar Boolean for the action webhook response, expecting <n>"
    (string, Named n)    -> n ∈ {String, ID} else
                            "got scalar String for the action webhook response, expecting <n>"
    (array, Named n)     -> "got array for the action webhook response, expecting <n>"
    (array, List t)      -> validate each element against t
    (object, Named n)    -> if n is a built-in scalar (Int/Float/String/Boolean/ID):
                              "got object for the action webhook response, expecting <n>"
                            else validateObject(obj, F)
    (_, List _)          -> "expecting array for the action webhook response"

validateObject(obj, F):        # TOP-LEVEL ONLY; nested objects are not validated
  for each (field, type) in F where type is non-null:
     key missing   -> 'field "<f>" expected in webhook response, but not found'
     value == null -> 'expecting not null value for field "<f>"'
  extra keys are ignored

isCustomScalar(Named n, F) = n ∈ pgScalarTranslations-names
                             || (F empty && n ∉ {Int,Float,String,Boolean,ID})
isCustomScalar(List _, _)   = False
```

`pgScalarTranslations` names (`server/src-lib/Hasura/Backends/Postgres/SQL/Types.hs`)
include `serial`, `bigserial`, `smallint`, `int2`, `int`, `integer`, `int4`,
`bigint`, `int8`, `real`, `float4`, `double precision`, `float8`, `numeric`,
`decimal`, `money`, `boolean`, `bool`, `bpchar`, `char`, `character`,
`varchar`, `character varying`, `text`, `citext`, `date`, `timestamp`,
`timestamptz`, `timetz`, `json`, `jsonb`, `geometry`, `geography`, `raster`,
`uuid`, `ltree`, `lquery`, and more.

Notes:

- The validation for list element types re-checks `isCustomScalar` per level.
  `[myCustomScalar!]!` with `[{"foo":"bar"}]` passes.
- `[[myCustomScalar!]]!` with a flat array of objects fails with
  `expecting array for the action webhook response`. Golden:
  `test_expecting_custom_scalar_array_response_got_different_type`.
- `[String!]!` with `["foo","bar",null]` fails with
  `got null for the action webhook response`.
- `[Int]!` with `["foo",…]` fails with
  `got scalar String for the action webhook response, expecting Int`.
- **Scalar values are not coerced or type-checked inside objects.** For
  example an `Int!` field receiving `"abc"` is passed through unchanged
  (only presence and non-null are checked).

### 8.4 Building the output (`makeActionResponseNoRelations`)

```
if containsCustomScalar(T) (named base type after unwrapping lists is custom per isCustomScalar):
    return webhook JSON verbatim                 # e.g. json!, myScalar, [myScalar!]!
else match v:
  array  -> map each element: object -> project(selection, obj); other -> verbatim
  object -> project(selection, obj)
  null   -> null
  other  -> verbatim  (e.g. String output)

project(fields, obj) = ordered object, in selection order, keyed by alias:
  __typename        -> type name string
  scalar/enum field -> obj[originalFieldName] or null if missing
  nested object fld -> obj[ALIAS] (sic) then:
                         object -> project(sub, o)
                         array  -> elements mapped the same way (objects projected,
                                   arrays recursed, nulls kept, scalars dropped)
                         null   -> null
                         other scalar, or key missing -> KEY OMITTED from the response
                         (async actions with relationships read the output through
                         jsonb_to_record instead: a missing key is null)
  relationship fld  -> placeholder string "remote relationship placeholder", replaced by the remote join (§8.7)
```

Golden examples:

- An omitted nullable field gives `null`
  (`omitted_field_response_for_nullable_field.yaml`).
- A `null` top-level output for a nullable type gives `null`
  (`null_response.yaml`).
- A nested null is passed through (`expecting_object_response_with_nested_null.yaml`).

### 8.5 4xx error propagation

The body (after the response transform) is decoded as:

```json
{ "message": "<required text>", "code": "<optional text>", "extensions": <optional any JSON> }
```

If decoding fails (for example no `message`, or `message` not a string), the
error comes from `decodeValue` → `liftIResult`
(`server/lib/hasura-base/src/Hasura/Base/Error.hs`): status 400, code
**`parse-failed`** (not `unexpected`), path = the aeson path (`$` for a missing
key), message after `formatMsg`, e.g. `the key 'message' was not present`.
`internal` (ActionInternalError with `error: "unexpected response"`) is
attached when internal errors are included (`modifyQErr addInternalToErr`).

`"extensions": null` decodes as `Nothing`, i.e. it behaves like an absent
`extensions` (normal `path`/`code`). A non-object `extensions` value (e.g. a
string) is emitted verbatim as the `extensions` value.

Resulting GraphQL error, with the HTTP response status still 200 for GraphQL
requests (`encodeGQLErr`, `server/lib/hasura-base/src/Hasura/Base/Error.hs`):

| Webhook body | GraphQL `errors[0]` |
|---|---|
| `{"message":"m"}` | `{"message":"m","extensions":{"path":"$","code":"unexpected"}}` |
| `{"message":"m","code":"c"}` | `{"message":"m","extensions":{"path":"$","code":"c"}}` |
| `{"message":"m","extensions":{"foo":"bar"}}` | `{"message":"m","extensions":{"foo":"bar"}}` |
| `{"message":"m","code":"c","extensions":{"foo":"bar","code":"x"}}` | `{"message":"m","extensions":{"foo":"bar","code":"x"}}` |

When `extensions` is present (even `{}`), it **replaces** the entire
`extensions` object verbatim: no `path`, no `code`, and the top-level `code`
is ignored. Goldens: `tests-py/queries/actions/sync/extensions_code_*.yaml`.

The QErr's HTTP status is the webhook's 4xx status. It matters only for
non-GraphQL contexts such as the stored async errors. For GraphQL over HTTP
the response is 200 with `errors`. In fixtures, a failed mutation shows
`data: null` + `errors`, while a failed query shows only `errors` (see the
GraphQL transport spec for the exact rule).

### 8.6 Response headers (Set-Cookie)

On 2xx, all `Set-Cookie` headers of the webhook response are collected. For
**mutation** actions over HTTP they are added to the GraphQL HTTP response
(golden `TestActionsSyncResponseHeaders.test_set_cookie_header`). For query
actions over HTTP they are discarded (`Transport/HTTP.hs` query path ignores
them). Over WebSocket nothing is forwarded.

### 8.7 Relationships of sync action outputs (joins to tables)

Source: `transformActionFields` in
`server/src-lib/Hasura/GraphQL/Execute/RemoteJoin/Collect.hs`;
`pgDBRemoteRelationshipPlan` in
`server/src-lib/Hasura/Backends/Postgres/Instances/Execute.hs`.

Joins run as **remote joins after the webhook call**:

1. In the action selection set, each relationship field is replaced by a
   placeholder and a `RemoteJoin` is collected. This also happens inside
   nested objects, at their path.
2. For each join, the LHS join fields (`field_mapping` keys) must appear in
   the action result:
   - if the user also selected that field, its alias is reused;
   - otherwise a **phantom field** is added: alias = `<field>` +
     `"_join_column"` + underscores, padded so that it is longer than every
     alias in the selection set (`getJoinColumnAlias`).
3. After the webhook response is projected, the join engine:
   - collects the distinct LHS key tuples;
   - runs **one** SQL query per relationship against the source, selecting
     from `jsonb_to_recordset($1::jsonb) AS (__argument_id__ bigint, <col> <coltype>, ...)`,
     joined to the target table with the role's select permission
     (`convertRemoteSourceRelationship`);
   - splices the results back in;
   - removes the phantom fields.

   Details of this SQL belong to the remote-relationships spec. The argument-id
   column name comes from the join engine; inferred to be
   `__argument_id__`.
4. Object relationships give an object or `null`. Array relationships give a
   list, honouring the user's args.
5. Action relationships to **remote schemas** are not supported.

---

## 9. Async actions

### 9.1 Mutation

`resolveActionMutationAsync`:

- Inserts into the metadata DB:

  ```sql
  INSERT INTO "hdb_catalog"."hdb_action_log"
      ("action_name", "session_variables", "request_headers", "input_payload", "status")
  VALUES ($1, $2, $3, $4, $5)          -- $5 = 'created' (bound as a parameter)
  RETURNING "id"
  ```

  (`insertActionTx` in `server/src-lib/Hasura/GraphQL/Execute/Action.hs`.)

  - `session_variables`: JSON object of the session.
  - `request_headers`: JSON object `{headerName: value}` built from **all**
    client request headers (`HashMap.fromList`). Duplicate names: the last one
    wins (`HashMap.fromList` semantics; note the key is the case-*sensitive*
    original name, so `Foo` and `foo` both survive). The original header-name
    case is kept. When read back by the processor the map is turned into a
    list in HashMap order, so forwarded-header order is unspecified.
  - `input_payload`: the `input` JSON.
- If a trace context is active, the W3C `traceparent`/`tracestate` and B3
  headers are injected (prepended) into the stored headers.
- The response value is the action id as a **JSON string** (uuid text).
- The webhook is **not** called in the request path.

### 9.2 Storage DDL (`server/src-rsr/initialise.sql`)

```sql
CREATE OR REPLACE FUNCTION hdb_catalog.gen_hasura_uuid() RETURNS uuid AS
'select gen_random_uuid()' LANGUAGE SQL;

CREATE TABLE hdb_catalog.hdb_action_log
(
  id UUID PRIMARY KEY DEFAULT hdb_catalog.gen_hasura_uuid(),
  -- we deliberately do not reference the action name
  -- because sometimes we may want to retain history
  -- after dropping the action
  action_name TEXT,
  input_payload JSONB NOT NULL,
  request_headers JSONB NOT NULL,
  session_variables JSONB NOT NULL,
  response_payload JSONB NULL,
  errors JSONB NULL,
  created_at timestamptz NOT NULL default now(),
  response_received_at timestamptz NULL,
  status text NOT NULL,
  CHECK (status IN ('created', 'processing', 'completed', 'error'))
);
```

- There is no secondary index.
- `response_received_at` is never written by current code.
- The Rust engine must create this table in the same catalog migration as the
  rest of `hdb_catalog` (see the metadata/catalog spec), and must tolerate
  existing rows from V2.

Status machine:

```
created --(poller claims)--> processing --(webhook ok)----> completed  (response_payload set)
                                        \--(any QErr)-----> error      (errors set)
processing --(graceful shutdown, still locked)--> created
```

### 9.3 Processor loop (`asyncActionsProcessor`)

One background task runs forever:

```
loop:
  interval <- HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL (ms, default 1000; 0 => disabled ("Skip"))
  if disabled: sleep 1s; continue          # re-reads config each iteration
  asyncActions <- actions in schema cache with type == mutation-asynchronous
  if asyncActions non-empty:
     items <- fetchUndelivered(batchSize = HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_BATCH_SIZE, default 10)
              (on DB error: treat as empty)
     add item ids to lockedActionEvents set
     run callHandler for all items CONCURRENTLY; wait for all
  sleep interval
```

Fetch SQL:

```sql
update hdb_catalog.hdb_action_log set status = 'processing'
where id in (
  select id from hdb_catalog.hdb_action_log
  where status = 'created'
  for update skip locked limit $1
)
returning id, action_name, request_headers::json, session_variables::json, input_payload::json
```

`callHandler(item)`:

1. Restore the trace context from the stored headers, then strip the trace
   headers (`traceparent`, `tracestate`, B3 `X-B3-*` / `b3`; per
   `Tracing.isTraceHeader`).
2. Look up the action by name in the **current** schema cache. If it is
   missing (action dropped), do nothing: the row stays `processing` forever
   (verified: no status update).
3. Call `callWebhook`, the same function as sync, with:
   - the definition's handler, headers, `forward_client_headers`,
     `ignored_client_headers`, timeout and transforms;
   - client headers = the stored headers;
   - payload `request_query: null`;
   - action type `mutation-asynchronous`;
   - `HideInternalErrors`.
4. On success:

   ```sql
   update hdb_catalog.hdb_action_log set response_payload = $1, status = 'completed' where id = $2
   ```

   `response_payload` = the validated webhook JSON after the response
   transform, **not** projected.
5. On error:

   ```sql
   update hdb_catalog.hdb_action_log set errors = $1, status = 'error' where id = $2
   ```

   `errors` = `ToJSON QErr`, i.e. `{"error": msg, "path": "$", "code": code[, "internal": …]}`.
   - `ExtraExtensions` (the webhook's `extensions`) are **dropped** by
     `ToJSON QErr`.
   - `internal` is present only for errors raised with `throw500WithDetail`
     regardless of the flag, i.e. request/response transform failures.
   - Example from the `create_user` 400 handler:
     `{"code":"invalid-email","path":"$","error":"Given email address is not valid"}`.
   - Example for an invalid JSON response:
     `{"error":"not a valid json response from webhook","path":"$","code":"unexpected"}`.
   - Example for a timeout: `{"error":"http exception when calling webhook","path":"$","code":"unexpected"}`.
6. Remove the id from the locked set.

Header precedence is re-read every loop iteration.

Graceful shutdown (`server/src-lib/Hasura/App.hs`):

- Wait up to `--graceful-shutdown-timeout` for the locked set to drain.
- Then reset still-locked ids:

  ```sql
  UPDATE hdb_catalog.hdb_action_log SET status = 'created'
  WHERE status = 'processing' AND id = ANY($1::uuid[])
  ```

- Rows stuck in `processing` after a crash are **not** recovered at startup
  (verified: no startup reset).

### 9.4 Async query (`<action>(id: uuid!)`)

Fetch:

```sql
SELECT created_at, response_payload::json, errors::json, session_variables::json
FROM hdb_catalog.hdb_action_log WHERE id = $1
```

The query expects exactly one row. An unknown id gives `database query error`
(500, `unexpected`, internal `Rows returned != 1`). (inferred, since
`PG.getRow` fails on an empty result.)

Two execution modes, chosen by whether the output **object type has
relationships defined** (not whether they are selected):

**(a) No relationships (`AAQENoRelationships`)**: the result is built in memory.

- `__typename` → the action name.
- `id` → the uuid string.
- `created_at` → `ToJSON UTCTime`, ISO-8601 with `Z`. Format is
  `2024-01-02T03:04:05.123456Z` (aeson UTCTime encoding; inferred), not the
  Postgres text format.
- `errors` → `null`, or the stored object re-encoded as
  `{"error","path","code"}` plus `internal` **only if** internal errors are
  included for this role and the stored JSON has `internal`. The `code`
  defaults to `unexpected` and the message defaults to
  `internal: error in parsing the action log`.
- `output` → `null` if `response_payload` is null. Otherwise
  `makeActionResponseNoRelations(fields, outputType, F = {}, check=False)`,
  i.e. the projection of §8.4 without the custom-scalar passthrough.
- **No ownership check is performed in this mode.** Any role with permission
  on the action can read any action id's result (Quirk Q6).

**(b) With relationships (`AAQEOnSourceDB`)**: the SQL runs on the relationship's source:

```sql
SELECT <json_build_object of selected fields>
FROM pg_catalog.jsonb_to_recordset($1::jsonb)
     AS _ ("id" uuid, "created_at" timestamptz, "response_payload" jsonb,
           "errors" jsonb, "session_variables" jsonb)
WHERE "id" = '<action-id>'
  [AND "session_variables" = $2::jsonb]     -- only for non-admin roles
```

- `$1` = `[ActionLogResponse]` JSON:
  `[{"id","created_at","response_payload","errors","session_variables"}]`.
- `$2` = the current session variables JSON. This is **exact JSON equality**
  of the whole session object, so the same user with a different set of
  `x-hasura-*` headers cannot read it. A mismatch gives no row, so the field is
  `null`. Golden: `test_create_user_roles`, where user 2 gets
  `create_user: null`.
- `output` is a computed-field sub-select over
  `jsonb_to_record(response_payload)` (or `jsonb_to_recordset` if
  `output_type` is a list) `AS (<col> <type>, …)`. The column list covers all
  fields of the output object, typed as follows:
  - a field used in a relationship `field_mapping` → the mapped table
    column's type;
  - custom scalar `Int` → `integer`, `Float` → `float`,
    `String`/`ID` → `text`, `Boolean` → `boolean`;
  - other custom scalars → `json`;
  - enum → `text`;
  - PG scalar → itself.

  Selected scalar fields are emitted as JSON.
- `errors` is the stored JSON, with `- 'internal'` applied unless internal
  errors are included.
- `stringifyNumbers` (`HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`) applies.
- Relationship fields inside `output` are then resolved by the **same
  remote-join mechanism** as §8.7, after this SQL runs (`transformAsyncFields`
  collects them).

### 9.5 Async subscriptions

Source: `server/src-lib/Hasura/GraphQL/Execute/Action/Subscription.hs`, `Transport/WebSocket.hs`.

- `subscription { create_user(id: "...") { id output { ... } errors } }`.
- Selecting **relationship fields** inside an async action subscription fails
  with `Remote relationships are not allowed in subscriptions` (400
  `not-supported`). (inferred from `getRemoteJoinsActionQuery` + check in
  `Execute.hs`.)
- Query actions in subscriptions: `query actions cannot be run as a subscription`.
- Async query fields can be combined with other fields in one subscription
  only in limited ways. The upstream note says "We can't support multiple
  async query fields of different kinds in a single subscription". Out of
  scope for exact parity.
- No-relationship mode:
  - a single background thread loops every **1 s**;
  - for each live operation it fetches all its action-log rows and **sends a
    data message every iteration**, even if nothing changed;
  - when all its actions are complete (`response_payload` or `errors`
    non-null) it sends `complete` and drops the operation;
  - if the fetch fails it sends an error and drops the operation.
- With-relationship mode:
  - the SQL of §9.4(b) becomes a normal multiplexed live query, with the
    action log JSON as a parameter;
  - the background thread refetches the log every second;
  - if the log changed, it restarts the live query with the new log;
  - once all actions are complete it stops tracking the operation. The live
    query itself continues (normal live-query semantics, sending only on
    change).

### 9.6 Other async details

- `forward_client_headers` for async uses the stored headers (§9.1). The
  default ignore list still applies.
- Dropping an action with `clear_data: true` (the default) deletes its log
  rows.

---

## 10. Request / response transforms

Shared with event triggers and scheduled triggers (same types; different
`$body`). Source: `server/src-lib/Hasura/RQL/Types/Webhook/Transform*.hs`,
`server/src-lib/Hasura/RQL/DDL/Webhook/Transform*.hs`.

### 10.1 Metadata shape

`request_transform`:

```json
{
  "version": 2,                       // 1 | 2; default 1; other => "expected 1 or 2, encountered <n>"
  "template_engine": "Kriti",         // only "Kriti"; default "Kriti"
  "method": "PUT",                    // optional; any string (case-insensitive, not validated)
  "url": "{{$base_url}}/users/{{$body.input.id}}",   // optional; string template
  "query_params": { "k": "{{v}}", "drop": null }      // optional; object form
              | "a=1&b={{$body.input.x}}",            //   or string-template form
  "request_headers": {                // optional
    "add_headers":    { "X-Foo": "{{$session_variables['x-hasura-user-id']}}" },  // default {}
    "remove_headers": [ "content-type" ]                                          // default []
  },
  "body": <v1: Kriti template string>
        | <v2: {"action":"transform","template":"<Kriti template>"}
               | {"action":"remove"}
               | {"action":"x_www_form_urlencoded","form_template":{"k":"<string template>"}}>
}
```

- In **v1**, `body` is a template string. It is equivalent to v2
  `{"action":"transform","template": body}`.
- A v2 `body.action` outside the three values fails with
  `invalid transform action`.
- `query_params` that is neither an object nor a string fails with
  `Invalid query parameter`.
- `ToJSON` emits keys: `version`, `template_engine`, then `method`, `url`,
  `query_params`, `request_headers` (each if present), then `body`. For v1
  only a `transform` body can be represented. `request_headers` always emits
  both `add_headers` and `remove_headers`.

`response_transform`:

```json
{ "version": 2, "template_engine": "Kriti",
  "body": <v1: template string | v2: {"action": "transform"|"remove"|"x_www_form_urlencoded", ...}> }
```

The `version` default is 1. `ToJSON` emits `template_engine`, `version`, and
`body` if present.

### 10.2 Metadata-time validation

`validateTransforms` runs for `create_action`, `update_action`, event
triggers and `test_webhook_transform`. It parses every template, without
evaluating it.

- Request transform:
  - the `transform` body template is parsed as is;
  - `url`, `add_headers` values, `query_params` keys and values (object form),
    the `query_params` string, and `form_template` values are parsed as
    *unescaped templates*, i.e. wrapped in `"…"` first (§10.4).
- Failures: `err400 ValidationFailed "request transform validation failed"`
  (code `validation-failed`) with `internal` = the array of Kriti error JSON
  objects (§11.6).
- Response transform failures: `"response transform validation failed"`.
- Response validation runs first, then request validation.

Note: validation happens on the API command only. `validateTransforms` is
wired in `server/src-lib/Hasura/Server/API/Metadata.hs` for exactly
`<backend>_create_event_trigger` (`RMCreateEventTrigger`),
`create_cron_trigger`, `create_action`, `update_action` and
`test_webhook_transform` (verified). `replace_metadata` and metadata load at
startup do **not** parse-check templates; a broken template only fails at
call time (§7.4).

### 10.3 Request context

Built from the *original* request (`mkReqTransformCtx`), as Kriti variables:

| Variable | Value |
|---|---|
| `$body` | The original JSON body (the action payload of §7.1), or `null` if not JSON |
| `$session_variables` | Session variables object (lower-case keys), or `null` if unavailable |
| `$query_params` | Original query params as `[[key, value-or-null], …]` (array of 2-element arrays), parsed from the handler URL |
| `$base_url` | Resolved handler URL **text exactly as configured** (after env interpolation; trailing slash preserved) |

Extra functions: `getSessionVariable(name)` (§11.5) plus the basic function
map.

### 10.4 Field semantics

Each field is computed independently from the **original** request/context,
not from the other transformed fields. The results are then written back by
the `requestL` setter (`server/src-lib/Hasura/RQL/DDL/Webhook/Transform.hs`)
in this order: method, body, url, query_params, request_headers.

**Query-string quirk (verified from code):** setting the URL also sets the
query string from the new URL, but `query_params` is written *after* `url`
and is **always** written — when there is no `query_params` transform it
carries the *original* request's query params. So a URL transform such as
`"{{$base_url}}/x?a=1"` loses `?a=1` unless a `query_params` transform is
also present; the final query string is either the `query_params` transform
result or the original handler URL's query string. (Also, `setUrl` silently
keeps the old URL if the string fails `parseURI` or has no authority, and it
prepends any `user:pass@` userinfo to the host.)

**Unescaped template**: a template `t` for url, headers, query params and form
fields is evaluated as the Kriti program `"` + t + `"`, i.e. a string
template. The result is always a JSON string. `encodeScalar` would also accept
numbers and booleans, but they cannot occur here. Consequences:

- Literal `"` and `\` in such templates must be escaped Kriti-style.
- An interpolated non-string value is JSON-encoded compactly. A `null` becomes
  the text `null`.

| Field | Semantics |
|---|---|
| `method` | Replace the method with the given text. |
| `url` | Evaluate the unescaped template. The result must parse with `Network.URI.parseURI` (absolute URI). Otherwise error `Invalid URL: <text>`. If the result is not a string: `URL Transforms must produce a JSON String: <val>` (unreachable in practice). Host, port (default 80/443), TLS flag and path are taken from the new URL. Its query string is then overwritten by the `query_params` field (see the quirk above). |
| `body` `transform` | Evaluate the template. The result JSON is the new body, encoded with sorted keys. `Content-Type` is unchanged. |
| `body` `remove` | Empty body (zero bytes). Headers are unchanged; use `remove_headers` to drop `content-type`. |
| `body` `x_www_form_urlencoded` | For each `(k, template)` evaluate it to a string `v`. Drop pairs where `v == "null"`. Emit `escape(k)=escape(v)` joined by `&`. Escaping uses `URI.escapeURIString isUnescapedInURIComponent` (unreserved = `A-Za-z0-9-_.!~*'()`; inferred from the `network-uri` definition). Pair order follows HashMap iteration; **use sorted key order**, which matches the golden `baz=world&foo=bar`. Content-Type is **not** set automatically. |
| `query_params` object | Evaluate each key and value template (a value may be JSON `null`, meaning a key without value). Drop pairs whose key or value evaluates to the text `null`. The resulting list **replaces** all query params. Order is HashMap order; use sorted keys (inferred). |
| `query_params` string | Evaluate it, parse it as a query string (`parseQuery`: split on `&`/`;`, `+` → space, percent-decode), and replace the query params. When rendered, values are percent-encoded: `,` → `%2C`. Golden: `tags=apple%2Cbanana%2C` in `lib/api-tests/src/Test/Webhook/WebhookTransformSpec.hs`. |
| `request_headers` | Remove all headers whose (case-insensitive) name is in `remove_headers`, then **append** `add_headers` (evaluated as unescaped templates). Despite the name "add or replace", an existing header with the same name is **not** removed unless also listed in `remove_headers`. Since the default request already has `Content-Type: application/json`, adding `content-type` without removing it sends **two** `Content-Type` headers. (Verified: `applyHeadersTransformFn` does `filtered <> new`.) The Rust engine should reproduce this (or, safer, deduplicate) — flag as a decision. |

Errors from any field are collected (applicative `Validation`) into one
`TransformErrorBundle` (a JSON array). Within the headers, query-params and
form maps all failures are collected. Across fields, the `B.bsequence'`
traversal collects too. (inferred)

Rendering of the final URL (as seen in `test_webhook_transform` output and on
the wire):

- `scheme://host[:port if non-default]<path>[?<query>]`.
- With no URL transform and an original URL without a path, the path is `/`
  (golden `webhook_url: http://localhost:1234/`).
- After a URL transform to `http://www.google.com` plus query params, the
  rendering is `http://www.google.com?foo=bar` (empty path preserved).

### 10.5 Response context and semantics

Applied to the raw response body bytes:

| Variable | Value |
|---|---|
| `$body` | Response body decoded as JSON, or `null` if not JSON |
| `$request` | `null` if the action has **no request transform**. Otherwise an object `{"method": C, "url": C, "body": C, "query_params": C, "request_headers": C}` where every `C` is the same request-context object `{"body": …, "session_variables": …, "base_url": …, "query_params": …}` (quirk of `ToJSON RequestContext`). Note `C` is built by `mkReqTransformCtx` applied to the **transformed** request (`reqTransformCtx <*> Just actualReq` in `callWebhook`): `body` is the transformed body decoded as JSON (`null` if not JSON, e.g. form or removed body), `query_params` the transformed params, while `base_url` is still the original resolved handler URL. |
| `$response` | `{"status": <int status code>}` |
| `$session_variables` | Session variables or `null` |

Body actions:

- `transform`: the result JSON is re-encoded as the new body.
- `remove`: gives the JSON `null` body. For sync actions with a nullable
  output this yields `null` data; with a non-null output it gives `got null …`.
- `x_www_form_urlencoded`: the body becomes a JSON **string** containing the
  form encoding.

Without a body transform (only `version` etc.), the body is re-encoded from
the decoded `$body`: invalid JSON becomes `null`.

Failure: `500 "Response Transformation Failed"` with `internal` = the bundle.

Golden tests (sync):

- `mirror_action_transformed_success.yaml` (request);
- `mirror_action_transformed_output_success.yaml`,
  `results_list_transformed_output_success.yaml`,
  `scalar_response_action_transformed_output.yaml`,
  `object_response_action_transformed_output.yaml` (response);
- async: `test_create_user_transformed_success`.

---

## 11. Kriti template language (v0.3.3) — implementable specification

Kriti is a superset of JSON. A template is a single *expression*. JSON
literals can embed `{{ expr }}` in values, in string literals (string
interpolation), and as whole values. `range` and `if` are control-flow
expressions that are themselves values.

### 11.1 Lexer

The lexer has four modes: `0` (default), `expr`, `string`, `literal`. Modes
are kept in a stack.

| Mode(s) | Pattern | Token |
|---|---|---|
| 0, expr | whitespace | skip |
| 0, expr | `#` to end of line | skip (comment) |
| 0, expr | `true` / `false` | BoolLit |
| 0, expr | `$` | Identifier `$` |
| 0, expr | `\$?[a-zA-Z][$a-zA-Z0-9_\-]*` | Identifier (so `$body`, `x`, `foo-bar`, `a$b` are all identifiers) |
| 0, expr | `"` | StringBegin, push `string` |
| 0, expr | `'` | SingleQuote, push `literal` |
| 0, expr | `-?[0-9]+` | IntLit |
| 0, expr | `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?` | NumLit (longest match wins; `1.5` is a NumLit) |
| 0, expr | `:` `.` `,` `?` `??` `==` `!=` `>` `<` `>=` `<=` `&&` `||` `_` `:=` `{` `}` `[` `]` `(` `)` | Symbols |
| 0, expr | `{{` | DoubleCurlyOpen, push `expr` |
| expr | `}}` | DoubleCurlyClose, pop |
| string | `\{` `\"` `\\` `\/` `\b` `\f` `\n` `\r` `\t` | StringLit `{`, `"`, `\`, **`\/` (two characters: backslash and slash — sic)**, BS, FF, LF, CR, TAB |
| string | `\uXXXX` | StringLit of that code point |
| string | `[^\\"{]+` | StringLit (whitespace preserved) |
| string | `{` (single) | StringLit `{` |
| string | `{{` | DoubleCurlyOpen, push `expr` (longest match beats the single `{`) |
| string | `"` | StringEnd, pop |
| literal | `([\\][\\]|[\\]` + "`" + `|[^'{])+` | StringLit (raw) |
| literal | `'` | SingleQuote, pop |

Lexer notes:

- In mode 0, `}` and `}}` are two `CurlyClose` tokens. `}}` only closes in
  `expr` mode.
- `{{` in mode 0 pushes `expr`.
- `range`, `if`, `elif`, `else`, `end`, `null`, `in` and `not` are ordinary
  identifiers that the grammar matches by text, so they cannot be used as
  variable names. `_` is a symbol, not an identifier.
- Any other character (or a lone `\` followed by an invalid escape inside a
  string) gives a lex error, `InvalidLexeme`.
- Adjacent StringLit fragments are concatenated by the grammar.
- Positions: line is **0-based** (the first line is 0), column is
  **1-based**, columns count Unicode characters, and a newline resets the
  column to 1.

### 11.2 Grammar (Happy, LALR)

```
expr  : atom '>'  atom | atom '<'  atom | atom '>=' atom | atom '<=' atom
      | atom '!=' atom | atom '==' atom | atom '&&' atom | atom '||' atom
      | atom 'in' atom | atom '??' atom
      | ap
      | '{{' expr '}}'                       -- transparent wrapper

ap    : ident '(' expr ')'                   -- function call, exactly ONE argument
      | 'not' expr                           -- sugar for function "not"
      | atom

atom  : var | requiredField | optionalFields | range | iff | json | '(' expr ')'

var           : ident
requiredField : atom '.' ident
              | atom '[' "'" string "'" ']'   -- quoted key
              | atom '[' atom ']'             -- computed key / index
optionalFields: atom '?' field*               -- everything after '?' is optional-chained
field         : '.' ident | '[' "'" string "'" ']' | '[' atom ']'

range : '{{' 'range' (ident | '_') ',' ident ':=' expr '}}' expr '{{' 'end' '}}'
iff   : '{{' 'if' expr '}}' expr ('{{' 'elif' expr '}}' expr)* '{{' 'else' '}}' expr '{{' 'end' '}}'
        -- NOTE: the else branch is MANDATORY

json  : string_lit | number | int | 'true' | 'false' | 'null' | array | object
string_lit : '"' (string | '{{' expr '}}')* '"'
array : '[' ']' | '[' expr (',' expr)* ']'
object: '{' '}' | '{' field (',' field)* '}'
field : '"' string* '"' ':' expr             -- KEYS ARE PLAIN STRINGS ONLY (no {{ }} inside keys)
```

Precedence declarations: `%right in`, `%right LOW`, `%right ']'`,
`%nonassoc > < <= >= == != && ||`, `%left .`, `%left ??`, `%left ident not`.

Practical consequences:

- **Binary operators are non-associative and only combine atoms.**
  `a && b && c` and `a == 1 && b` are **parse errors**. Write
  `(a && b) && c` and `(a == 1) && b`.
- `not` binds to a whole `expr`: `not (x == 1)`; `not x` also works.
- Function calls take one expression argument. Multiple values are passed as
  an array, for example `concat([a, b])`.
- `{{ … }}` may wrap any expression at any depth. A template consisting
  solely of `{{ $body }}` is valid. So are `[ {{ a }}, {{ b }} ]` and nested
  `{{ removeNulls( {{ range … }} ) }}` (golden kriti example 24).
- `range` and `if` constructs must be delimited with `{{ }}` exactly as shown.
- Object keys: duplicate keys keep **one** value. Parsing builds the field
  list in reverse and `fromList` keeps the last one inserted, so the **first
  occurrence in source** wins. (inferred from `object_fields : object_fields
  ',' object_field { $3 : $1 }` + `KeyMap.fromList`.)
- Trailing commas are not allowed.

### 11.3 Evaluation semantics

The environment is a map from variable name to a JSON value. For request
transforms it is `$body`, `$session_variables`, `$query_params`, `$base_url`.
For response transforms it is `$body`, `$request`, `$response`,
`$session_variables`.

| Construct | Result / errors |
|---|---|
| String / number / bool / null literal | The JSON value. Ints are exact. Numbers are arbitrary-precision decimals (Scientific). |
| `"a{{e}}b"` (string template) | Each part is evaluated and concatenated. A String part is inserted raw; **any other JSON value is inserted as its compact JSON encoding** (`null` → `null`, objects as `{"a":1}` with keys sorted, arrays `[1,2]`, numbers aeson-style). |
| Array / object literal | Elements and values are evaluated. |
| `x` (var) | Lookup; if unbound, **Name Error** `Variable 'x' not in scope`. |
| `e.k` | `e` must evaluate to an Object (otherwise **Type Error**, expected `Object`). Missing key → **Attribute Error**. |
| `e[k]` | If `e` is an Object: `k` must evaluate to a String (otherwise Type Error, expected `String`); missing key → Attribute Error. If `e` is an Array: `k` must evaluate to a Number representable as `Int` (otherwise Type Error, expected `Integer`); out of range or negative → **Index Error** (message `Can only range over an array`, sic). Any other `e` → Type Error, expected `Object`. |
| `e['lit']` | Same as `e.lit`. |
| `e?f1 f2 …` (optional chain) | Evaluate `e`; **any error while evaluating `e` becomes null**. Evaluate bracket keys (errors propagate). Then walk: String key on Object → lookup; Number on Array → index (must fit `Int`, `!?`); anything else, or a missing key, → stop with `null`. If `e` is null → null. If `e` is a String/Number/Bool → **Type Error** expected `Object`. Example: `$body?.a.b[0]` with missing `a` → `null`. `$x?` with unbound `$x` → `null`. |
| `a == b`, `a != b` | Structural JSON equality (numbers compared numerically: `1 == 1.0` is true). Both sides are always evaluated. |
| `a < b` etc. | Haskell derived `Ord` on aeson `Value`. Same-type numbers compare numerically, strings by code points, `false < true`. Across types, constructor order: Object < Array < String < Number < Bool < Null. (inferred: aeson `Value` Ord instance.) |
| `a && b`, `a \|\| b` | **Both sides evaluated** (no short circuit). Both must be Bool, otherwise Type Error expected `Boolean`. |
| `a in b` | If `b` is an Object, `a` must be a String (otherwise Type Error expected `String`) → key membership. If `b` is an Array → element membership by JSON equality. Otherwise Type Error expected `Array`. |
| `a ?? b` | If `a` evaluates to `null`, the result is `b`, else `a`. Errors in `a` propagate; use `a?` / `?.` to suppress them. |
| `{{ if c }} t {{ elif c2 }} t2 {{ else }} e {{ end }}` | Conditions must be Bool (Type Error expected `Boolean`). Lazy: only the chosen branch is evaluated. |
| `{{ range i, x := e }} body {{ end }}` | `e` must be an Array, otherwise Type Error expected `Array`. The result is an **Array** of `body` evaluated per element, with `x` bound to the element and `i` (unless `_`) to the 0-based index. Inner bindings shadow outer ones. Wrapping a range in `[ … ]` produces a nested array (golden kriti examples 2–7). |
| `f(e)` | Evaluate `e` **first** (its errors win), then look up and call `f`. Unknown `f` → **Name Error** `Variable 'f' not in scope` (span = the whole call). Function map = kriti `basicFuncMap` left-unioned with Hasura's extras, so a basic function name cannot be overridden. A function failure → **Function Error** with the function's message (span = the argument). |

The final value is the template's JSON result. For `transform` bodies it is
sent JSON-encoded. Object keys are sorted, since the encoder is aeson with a
Map-based KeyMap.

### 11.4 Standard functions (`Kriti.CustomFunctions.basicFuncMap`)

Every function takes one JSON argument.

| Name | Behaviour | Errors |
|---|---|---|
| `empty` | Object/Array: true if no elements. String: true if `strip(s) == ""` (whitespace-only counts as empty). Number: `n == 0`. Null: true. | Bool: `Cannot define emptiness for a boolean` |
| `size` | Object: key count. Array: length. String: character count. Number: the number itself. Bool: 1/0. Null: 0. | – |
| `inverse` | Array: reversed. String: reversed. Number: `1/n`. Bool: `not`. Object/Null: unchanged. | `1/0`: (inferred: Haskell Scientific division by zero is a runtime exception — avoid; emit a Function Error) |
| `head` | First element of an Array, or the first character of a String (as a string). | `Empty array`, `Empty string`, `Expected an array or string` |
| `tail` | Array without its first element, or String without its first character. | Other types: `Expected an array or string`. Empty array/string: crashes in Haskell; return a Function Error (inferred) |
| `toCaseFold` | Unicode case fold of a String. | Non-string: aeson message `parsing String failed, expected String, but encountered <Type>` (inferred aeson wording). Same message shape for `escapeUri`, `toPairs` (`Object`), `removeNulls`/`concat` (`Array`), `not` (`Bool`), using the name passed to `withText`/`withObject`/`withArray`/`withBool` |
| `toLower`, `toUpper`, `toTitle` | Case conversion of a String (Data.Text semantics; `toTitle` capitalises the first letter of each word and lowercases the rest). | as above |
| `escapeUri` | Percent-encode every character that is not RFC3986 *unreserved* (`A-Za-z0-9-._~`), UTF-8 based. Example: `"?foo=bar/baz"` → `"%3Ffoo%3Dbar%2Fbaz"`. | non-string |
| `fromPairs` | `[[k,v],…]` → object; k must be a string. Duplicate keys: last wins (`J.object`). | Element of length ≠ 2: `Expected an array of shape [ [k1,v1], [k2,v2] ... ] - With String keys.`; non-string key, non-array element or non-array argument: the aeson parse message instead (`withArray "Nested Arrays"` / `"Array of Pair"`, Key parser) |
| `toPairs` | Object → `[[k,v],…]`, in key order (sorted). | non-object |
| `removeNulls` | Array without `null` elements. | non-array |
| `concat` | Argument must be an Array. All arrays → flattened one level. All strings → concatenated. All objects → merged, keys from **later** objects win. Tried in that order, so `concat([])` returns `[]`. | non-array, or mixed element types (aeson parse message of the **last** alternative tried, i.e. the object one; inferred from aeson `<|>`) |
| `not` | Bool negation. | non-bool |

Hasura additions:

- `getSessionVariable(name)`
  (`server/src-lib/Data/Aeson/Kriti/Functions.hs`):
  - `null` arg → `null`;
  - String → case-insensitive lookup in the session variables;
  - missing (or no session) → error `Session variable "<name>" not found`;
  - non-string → error `Session variable name should be a string`.
- `getEnvironmentVariable` is **not** available in action or event
  transforms; it is only used for Data Connector config.

### 11.5 Errors — JSON serialization

Every Kriti error serializes as:

```json
{
  "error_code": "Parse Error",        // Parse Error | Lex Error | Attribute Error | Name Error |
                                      // Type Error | Index Error | Function Error | Invalid Path
  "message": "Unexpected token '}'.",
  "source_position": { "start_line": 0, "start_column": 14, "end_line": 0, "end_column": 15 }
}
```

The transform error bundle is a JSON **array** of these objects. Non-Kriti
transform errors use `{"error_code":"TransformationError","message":…[,"value":…]}`.

Messages:

- Parse:
  - `Unexpected token '<tok>'.`, where tok is rendered per `serializeToken`:
    string literals in double quotes, symbols as written, `""` for EOF;
  - `ParseError: Empty token stream.`
- Lex: `Invalid Lexeme: '<src>'`, where `<src>` is the **entire template
  source** (the `parseSource` of the parser state, not the remaining input),
  with span start = the failing position and end column = start column +
  byte length of the whole source (`Kriti/Parser/Monad.hs`). For an invalid
  `\uXXXX` escape, `<src>` is just the matched escape text.
- Attribute: `'Object' has no attritubte '<k>'.`
  - **Note:** the pinned kriti source has the typo `attritubte`.
  - The tests-py fixture `test_webhook_transform_bad_eval.yaml` expects
    `'Object' has no attribute 'world'.`
  - The fixture was likely updated against a newer kriti or never re-run;
    the source wins. Open question.
- Name: `Variable '<x>' not in scope`.
- Type: `Couldn't match expected type '<Expected>' with actual type '<Actual>'`
  (types: Object, Array, String, Number, Boolean, Null).
- Index: `Can only range over an array`.
- Function: the function's message.

Spans:

- Line is 0-based; columns are 1-based, with an exclusive end.
- Eval errors use the span of the offending expression. For example
  `{{ $body.world }}` → start col 4, end col 15 (the span of `$body.world`).
- Function errors use the argument span: `getSessionVariable("x…")` → the
  span of the string literal including its quotes.
- **Parse errors (UnexpectedToken)**: the whole template is lexed *before*
  parsing, so the reported span is the span of the **last token in the
  input**, not of the offending token. For `"$body.hello }}"` in mode 0 the
  span is 14–15 (the final `}`), while the message names the offending token.
  Reproducing exact spans is low priority.
- For unescaped templates the program text is `"` + t + `"`, so columns are
  shifted by 1.

### 11.6 Worked examples (from kriti goldens; context `$` = an array of people)

```
"eyecolor_{{$[0].eyeColor}}"                       => "eyecolor_blue"
{{ range i, x := $ }} {"name": "{{x.name}}", "id": {{ i }}} {{ end }}
                                                   => [{"id":0,"name":"Shepard Mccall"}, …]
[ {{ $[0]?.notIndex }}, {{ $?[9999].index }}, {{ $[0]?.notIndex.baz }} ] => [null, null, null]
{{ $[0]?.foo.bar ?? true }}                        => true
"foo={{$bar? ?? ""}}"                              => "foo="
"{{ $[1].friends[1] }}"                            => "{\"id\":1,\"name\":\"Charmaine Tate\"}"
[ "{{ $?[1000] }}", "is: {{ $?[1000] }}" ]         => ["null", "is: null"]
{{ if ("foo" in { "foo": true }) }} 1 {{ else }} 0 {{ end }} => 1
{{ concat([{"a":1,"b":2},{"b":3,"c":4}]) }}        => {"a":1,"b":3,"c":4}
{{ if $[0].isActive }} "a" {{ elif $[1].isActive }} "b" {{ else }} "c" {{ end }} => "b"
```

Hasura-flavoured examples:

```
url:   "{{$base_url}}/users/{{escapeUri($body.input.id)}}"
query_params: "{{ concat([\"tags=\", concat({{ range _, x := $body.input }} \"{{x}},\" {{ end }})]) }}"
       => URL ...?tags=apple%2Cbanana%2C
header: "Bearer {{$session_variables['x-hasura-user-id']}}"
body (v2 transform): |
  { "input": { "arg": { "id": {{ $body.input.arg.name }}, "name": {{ $body.input.arg.id }} } } }
```

The full kriti golden corpus (34 template/expected pairs, plus
`test/data/eval/success/source.json`) lives in the kriti-lang repo at
`test/data/eval/success/{examples,golden}`. Vendor it as unit tests for the
Rust Kriti crate.

---

## 12. `test_webhook_transform` metadata API

Source: `runTestWebhookTransform` in `server/src-lib/Hasura/RQL/DDL/Metadata.hs`.

Request:

```json
{
  "type": "test_webhook_transform",
  "args": {
    "webhook_url": "http://localhost:1234" | {"from_env": "MY_WEBHOOK"},
    "body": <any JSON, the sample original body>,
    "env": {"VAR": "value"},                 // optional mock env (default {})
    "request_headers": [["name","value"],…], // optional, default []
    "session_variables": {"x-hasura-…": "…"},// optional
    "request_transform": RequestTransform,    // required
    "response_transform": ResponseTransform   // optional; only validated, never applied
  }
}
```

Processing:

1. Validate the response transform, then the request transform (§10.2).
2. Resolve the URL:
   - `from_env`: look the variable up in the **mock `env`** (not the process
     env). Missing → `err400 not-found "Missing Env Var"`.
   - Plain string: `{{VAR}}` segments are interpolated from the mock env. The
     parser is `many1 (env | literal | "{")`; `{{` + text up to `}` + `}}`
     is a var. A parse failure gives `400 parse-failed "Invalid Url Template"`.
     A missing var gives `400 not-found`, `Missing Env Var: <VAR>. For security reasons when testing request options real environment variable values are not available. Please enter a mock value for <VAR> in the Sample Env Variables list. See https://hasura.io/docs/latest/graphql/core/actions/rest-connectors/#action-transforms-sample-context`.
3. Header values are interpolated the same way.
4. Build the request:
   - **method is GET** (http-client default), not POST;
   - body = the JSON-encoded `body`;
   - headers = `request_headers` only (no default headers).
5. Context: `$base_url` = the interpolated URL; `$body` = `body`;
   `$session_variables` = `session_variables` or `null`;
   `$query_params` from the URL.
6. Apply the request transform.

Response 200:

```json
{
  "webhook_url": "http://www.google.com?foo=bar",
  "method": "POST",
  "headers": [["content-type","application/json"],["foo","bar"]],
  "body": <decoded body>
}
```

- `headers` is an array of `[name, value]` pairs with **lower-cased
  (case-folded) names**, in request order.
- `body`: the JSON-decoded body. If it is not JSON, the raw text wrapped in
  `"…"` and decoded as a JSON string (form bodies); if that also fails (e.g.
  the text contains `"` or control characters) or the body is not an
  in-memory LBS, `null`. A removed body gives `""` (golden
  `test_webhook_transform_success_remove_body.yaml`).

Errors:

- An invalid URL gives `400 validation-failed "request transform validation failed"`,
  with internal `[{"error_code":"Request Initialization Error","message":"<show exception>"}]`.
- An evaluation failure gives the same top-level error, with `internal` = the
  Kriti error array.
- `path` is `$.args`.

Goldens: `tests-py/queries/v1/metadata/test_webhook_transform_*.yaml` (8
files) and `lib/api-tests/src/Test/Webhook/WebhookTransformSpec.hs`.

---

## 13. Logging and observability

- **Query log**: `QueryLogKindAction` → the `kind` is `"action"` for action
  root fields (sync, async query, async mutation).
- **`action-handler-log`** (`ELTActionHandler`, level info). It is not enabled
  by default; enable it via `HASURA_GRAPHQL_ENABLED_LOG_TYPES`. Emitted after
  every webhook response, including on error statuses, but not on HTTP
  exceptions. Payload:

  ```json
  {
    "request": {"url":…,"method":…,"headers":[[k,v]…],"body":"<text|null>","query_string":…,"response_timeout":…},
    "request_size": 123,
    "response_size": 456,
    "action_name": "create_user",
    "action_type": "mutation-synchronous",
    "request_trans": {…},              // only if a request transform was applied
    "transformed_request_size": 99     // only if transformed
  }
  ```

  With `HASURA_GRAPHQL_REDACT_ACTION_HANDLER_LOGS=true`
  (`--redact-action-handler-logs`, default false), `request.body` and
  `request_trans.body` are set to `null`.
- Transform failures are logged as unstructured error-level logs containing
  the bundle JSON.
- Metrics (EE Prometheus): bytes sent/received counters.

---

## 14. Configuration summary

| Env var / flag | Default | Effect |
|---|---|---|
| `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL` / `--async-actions-fetch-interval` | `1000` (ms, Haskell `Double` syntax, e.g. `500.5`) | Poll interval. `0` disables processing (the loop sleeps 1 s and re-checks). |
| `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_BATCH_SIZE` / `--async-actions-fetch-batch-size` | `10` | Rows claimed per poll, processed concurrently |
| `HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE` / `--configured-header-precedence` | `false` | `true` means configured headers win over client headers (§7.3) |
| `HASURA_GRAPHQL_REDACT_ACTION_HANDLER_LOGS` / `--redact-action-handler-logs` | `false` | Redact bodies in `action-handler-log` |
| `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS` | `true` | Admin sees `extensions.internal` |
| `HASURA_GRAPHQL_DEV_MODE` | `false` | Everyone sees `internal` |
| `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` | 60 s (see config spec) | Wait for in-flight async actions |
| `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES` | `false` | Affects async SQL path output |

---

## 15. Implementation notes for the Rust engine (non-normative)

- Use a dedicated `kriti` crate:
  - hand-written lexer with a mode stack, plus a recursive-descent or
    LALR parser that follows §11.2 exactly, including non-associative
    operators and the mandatory `else`;
  - an evaluator over `serde_json::Value` with `arbitrary_precision` (or a
    decimal type), so that numbers round-trip like Scientific;
  - use a sorted-key map (`BTreeMap`) to match aeson's key ordering.
  - Port the 34 kriti goldens and the Hasura fixtures as tests.
  - Parse templates once at metadata load and cache the AST; Hasura
    re-parses on every call, which is cheap to beat.
- Keep `callWebhook` a pure-ish function used by both the sync path and the
  async worker.
- Async worker: implement it as a tokio task. It should also be possible to
  run it out of process (Cloud Run scaled to zero cannot poll), for example
  via a `/v1/...`-triggered "process async actions" mode. Keep
  `FOR UPDATE SKIP LOCKED` so several replicas can poll safely.
- Treat the `internal` error payload shape as best effort. Match `message`,
  `extensions.code` and `extensions.path` exactly.

---

## 16. Known quirks to replicate (or consciously fix)

- **Q1**: Nested object fields in a sync action output are looked up in the
  webhook JSON by **alias**, not by field name. With `foo: address { city }`
  the code looks up key `foo`, so the field is usually omitted from the
  response (`makeActionResponseNoRelations`). Scalar fields correctly use the
  original name. Recommendation: fix (use the original name). Compatibility
  risk is minimal.
- **Q2**: A nested object field whose webhook value is a scalar has its key
  **omitted** from the response object, rather than nulled or rejected.
- **Q3**: The number/string type mismatch message says `got scalar String`
  even for numbers.
- **Q4**: `add_headers` appends without removing an existing header of the
  same name (duplicate `Content-Type`).
- **Q5**: A client-forwarded header or a default header beats a configured
  header of the same name by default.
- **Q6**: An async action query without relationships performs **no
  session/owner check**. With relationships it requires an exact
  session-variables JSON match for non-admin roles.
- **Q7**: An async action with a scalar `output_type`: the query field
  returns the scalar type and always resolves to `{}`. With an object output
  but custom-scalar *fields* it works normally.
- **Q8**: The async processor leaves rows in `processing` forever if the
  action was dropped or the server crashed.
- **Q9**: `response_received_at` is never set.
- **Q10**: The Kriti Index Error message is `Can only range over an array`,
  and the attribute error has the typo `attritubte` (pinned version).
- **Q11**: Kriti `&&` and `||` are not short-circuit, and binary operators
  cannot be chained without parentheses.
- **Q12**: In `test_webhook_transform` the default method is `GET`, and
  `response_transform` is validated but not applied.
- **Q13**: `drop_action` clears the async log by default (`clear_data`
  defaults to true).
- **Q14**: `create_action_permission` on a missing action silently succeeds
  (inferred).

### 16.1 Deliberate deviations of the Rust engine (async actions)

- **D1, relationship mode in memory**: with relationships on the output object
  (§9.4 (b)), V2 builds the result with SQL on the relationship's source
  (`jsonb_to_record` with typed columns). The Rust engine builds it in memory: the same
  exact session-variables check for non-admin roles, then the projection of §8.4 and the
  remote joins of §8.7. Values are therefore returned as the handler sent them, not
  re-typed by Postgres. Concretely: a `Float` sent as `1.0` is returned as `1.0` (V2's
  `float` column renders `1`); Postgres-scalar fields are not re-rendered
  (`timestamptz`, `numeric` text forms); `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES` is not
  applied inside `output`; an out-of-range `Int` is returned instead of failing the cast;
  and `created_at` is always rendered as `…+00:00`, whatever the source session's
  `TimeZone`.
- **D2, no `x-hasura-*` request headers stored**: `request_headers` omits `x-hasura-*`
  headers (including `x-hasura-admin-secret`). They are never forwarded (§7.3), so
  handlers see no difference; the admin secret is just not kept in plain text.
- **D3, lower-cased header names**: header names are stored lower-cased (HTTP/2 and
  hyper normalise them), so the case-sensitive duplicate names of §9.1 cannot occur.
- **D4, no trace headers**: the engine has no trace context, so none are injected into
  the stored headers; stored ones (from V2) are still stripped before forwarding.
- **D5, undecodable rows**: a claimed row that cannot be decoded (for example a JSON
  number outside `f64`) is marked `error` with
  `{"error": "invalid JSON in action log <column>: …", "path": "$", "code": "unexpected"}`
  instead of failing the whole batch.

---

## Upstream tests to reuse

All paths are relative to `server/`. Python tests read YAML fixtures. The
actions tests need the webhook test server
(`tests-py/context.py: ActionsWebhookHandler`); port its routes into a Rust
or Python mock handler. The `{{ACTION_WEBHOOK_HANDLER}}` env var points to it.

| Location | Covers |
|---|---|
| `tests-py/test_actions.py` | Driver for all action tests below; also inline assertions (async polling, roles, `User-Agent`, Set-Cookie, timeouts, introspection kinds) |
| `tests-py/queries/actions/sync/schema_setup.yaml` | Canonical set of custom types and actions (sync, query, transforms, recursion) — reuse as fixture metadata |
| `tests-py/queries/actions/sync/create_user_*.yaml`, `create_users_*.yaml`, `get_user_by_email*.yaml`, `get_users_by_email_success.yaml` | Success paths, 4xx error propagation, relationships to tables (object and array), nested outputs with joins |
| `tests-py/queries/actions/sync/expecting_*.yaml`, `get_null_field_expecting_non_nullable_field_array.yaml`, `get_string_scalar_array_*`, `get_scalar_action_output_type_success.yaml` | Exact output-validation messages (§8.3). Strip or adapt `extensions.internal.request` (shape mismatch, §8.2) |
| `tests-py/queries/actions/sync/mirror_action_*.yaml` | Input encoding (omitted vs null), non-null field checks, extra fields ignored, request and response transforms |
| `tests-py/queries/actions/sync/extensions_code_*.yaml` | 4xx `code`/`extensions` mapping table (§8.5) |
| `tests-py/queries/actions/sync/null_response.yaml`, `omitted_field_response_for_nullable_field.yaml`, `expecting_object_response_with_nested_null.yaml` | Null handling |
| `tests-py/queries/actions/sync/*transformed_output*.yaml`, `results_list_transformed_output_success.yaml` | Response transforms incl. `range` |
| `tests-py/queries/actions/sync/query_action_recursive_output.yaml`, `query_action_relationship_with_permission.yaml` | Recursive output types; relationship permission filtering |
| `tests-py/queries/actions/sync/remote_joins/` | Action output joined via remote relationships (needs the Node remote server `remote_schemas/nodejs/actions_remote_join_schema.js`) |
| `tests-py/queries/actions/async/*` | Async setup (incl. transformed and nested variants); behaviour asserted in `TestActionsAsync` |
| `tests-py/queries/actions/timeout/*` + `TestActionTimeout` | Timeout → stored error |
| `tests-py/queries/actions/custom-types/*.yaml` | Custom type validation messages, PG scalar reuse (needs PostGIS for `geography`), relationship add/drop |
| `tests-py/queries/actions/nested-relation/*.yaml` | Relationship shape checks (§4.3) and the inconsistency error format |
| `tests-py/queries/actions/metadata/*.yaml` | `value_from_env` `HASURA_GRAPHQL_` rejection; permission recreate |
| `tests-py/queries/actions/introspection/*` | Introspection output types (list/non-null wrappers) |
| `tests-py/queries/actions/roles_inheritance/*` | Inherited-role action permissions |
| `tests-py/queries/actions/relationships/basic/` | Query actions with relationships over HTTP and WebSocket |
| `tests-py/queries/v1/metadata/test_webhook_transform_*.yaml` | `test_webhook_transform` success and error JSON; Kriti error positions (note `attribute`/`attritubte`, §11.5) |
| `tests-py/queries/event_triggers/transform/basic_transform.yaml` | Same transform engine with an event payload (skip `_mssql`) |
| `lib/api-tests/src/Test/Webhook/WebhookTransformSpec.hs` | Query-param string templates with `range`/`concat`, URL rendering |
| `src-test/Hasura/RQL/DDL/Webhook/TransformSpec.hs`, `src-test/Hasura/RQL/WebhookTransformsSpec.hs` | Transform JSON parsing and round-trip (v1/v2), header add/remove parsing |
| `src-test/Network/HTTP/Client/TransformableSpec.hs` | URL lens semantics (trailing slash, query string) |
| kriti-lang `test/data/eval/success/{examples,golden}` + `source.json` (pinned commit) | 34 Kriti evaluation goldens; also `test/data/parser/` for parse failures |

Skip: MSSQL/BigQuery/Data Connector variants (`basic_transform_mssql.yaml`,
`DataConnector/MockAgent/TransformedConfigurationSpec.hs` — the latter covers
`getEnvironmentVariable`, which is DC-only). Skip any EE-only Prometheus
assertions.
