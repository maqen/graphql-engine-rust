# 01 — Metadata format, storage, metadata API and migrations

Target: a drop-in Rust reimplementation of Hasura GraphQL Engine V2 (Postgres first). This spec covers:

- the metadata **data model** (JSON v3, plus the legacy v1/v2 forms);
- how the CLI lays metadata out **on disk**;
- how the server **stores** metadata in `hdb_catalog`;
- the `/v1/metadata` **operation catalogue**;
- **consistency** rules and the inconsistent-object format;
- the **migrations/seeds** workflow, and which server endpoints the CLI needs.

Ground truth is the Haskell source (upstream tag at catalog version 48, ~v2.4x–v2.51). All paths are relative to the upstream repo root. Anything marked "(inferred)" was not verified line by line in source. CLI facts come from the Go sources under `cli/` (fetched read-only from upstream master).

Conventions in this doc:
- Field naming is always snake_case JSON (`hasuraJSON` = `aesonPrefix snakeCase`, defined in `server/src-lib/Hasura/Prelude.hs` / `Hasura/Base/...`).
- "omitted when default" refers to **export** (ToJSON). Parsers always accept the key being present.
- Rust sketches use `serde`. `#[serde(default)]` marks a key as optional on input. Where export has special omission rules, these are listed separately, because serde `skip_serializing_if` alone is not enough to reproduce the exact output.

---

## Scope and priority

**Essential (P0 — existing projects break without these)**
- Metadata v3 JSON parse/serialize, with exact defaults and the error messages for common mistakes.
- Legacy acceptance for:
  - `table` as a string or as `{schema,name}`;
  - `version: 1|2` metadata in `replace_metadata`;
  - `custom_column_names`;
  - legacy remote relationship form;
  - legacy logical-model field form.
- Sources (`pg`), with all their nested objects:
  - tables;
  - relationships (local, plus remote to source and to remote schema);
  - permissions;
  - computed fields;
  - event triggers;
  - functions and function permissions;
  - table/source customization;
  - `query_tags`.
- Top-level `remote_schemas`, `actions` + `custom_types`, `cron_triggers`, `query_collections`, `allowlist`, `rest_endpoints`, `inherited_roles`, `network.tls_allowlist`.
- Storage:
  - `hdb_catalog.hdb_metadata` with `resource_version` optimistic locking;
  - `hdb_catalog.hdb_version` with `cli_state` / `console_state`;
  - catalog bootstrap + version check (catalog version **48**);
  - `hdb_schema_notifications` for multi-instance sync;
  - the event-trigger source catalog (`hdb_catalog.event_log` etc.) in each source that has triggers.
- `/v1/metadata` ops used by CLI and console:
  - `replace_metadata` (v1 and v2 forms), `export_metadata` (v1 and v2), `reload_metadata`, `clear_metadata`;
  - `get/drop_inconsistent_metadata`, `get/set_catalog_state`;
  - `bulk`, `bulk_keep_going`;
  - the `pg_*` track/permission/relationship/event-trigger/function ops;
  - `add_source` / `drop_source`;
  - remote schema, action, collection and allowlist ops.
- `/v2/query` `run_sql` (migrations) and `bulk`.
- `/v1/version`, `/healthz`.
- Boot-time loading of a CLI metadata directory and migrations directory. This is our replacement for the `cli-migrations-v3` image (see §1.6).

**Rarely used (P1)**
- `native_queries`, `logical_models` (PG), `apollo_federation_config`.
- `bulk_atomic`, `suggest_relationships`.
- `test_webhook_transform`.
- `get_table_info` / `get_source_tables`.
- `dump_internal_state`.
- Config v2 directory layout (single `tables.yaml`), config v1 YAML migrations.
- `metrics_config` (EE feature, but the key is parsed in CE metadata).
- `set_table_is_enum`, `rename_source`, `rename_relationship`.
- `cleanup_config` and the event cleanup ops.
- `MetadataDefaults` (`HASURA_GRAPHQL_METADATA_DEFAULTS`).

**EE / Cloud-only (parse and round-trip, but feature may be a no-op or rejected in CE)**
- `api_limits`.
- `opentelemetry`.
- `graphql_schema_introspection`. Parsed, stored and exported in OSS; the docs mark it Cloud/EE. **In OSS it is a no-op**: the OSS `executeIntrospection` (`Hasura/App.hs` ~l.802, `instance MonadGQLExecutionCheck AppM`) ignores the disabled-roles set and always answers introspection. The set is only consulted by the EE implementation of that class (verified).
- Postgres source options: `read_replicas`, `connection_template`, `connection_set`, `health_check`, `ssl_configuration`, `pool_settings.total_max_connections`.
- `hdb_version.ee_client_id/secret`.
- `backend_configs.dataconnector` (data connectors — out of Postgres scope).
- `stored_procedures` (MSSQL only).
- Backends `mssql`, `bigquery`, `citus`, `cockroach`, data connectors. Parse `kind` and keep the source opaquely; mark it inconsistent if unsupported (design choice).

**Design rule:** metadata must **round-trip losslessly** for every key we parse, including EE/unsupported keys, so that `export_metadata` after `replace_metadata` does not drop user data. For unsupported backends we keep the raw JSON of the whole source.

---

## 1. CLI metadata directory layout and assembly

### 1.1 config.yaml (project root)

From `docs/docs/hasura-cli/config-reference.mdx` and the CLI source:

```yaml
version: 3                         # 1 | 2 | 3 (config version, NOT metadata version)
endpoint: http://localhost:8080
admin_secret: ...                  # or env HASURA_GRAPHQL_ADMIN_SECRET
access_key: ...                    # deprecated alias
insecure_skip_tls_verify: false
certificate_authority: path
api_paths:                         # defaults shown
  query: v1/query
  graphql: v1/graphql
  config: v1alpha1/config
  pg_dump: v1alpha1/pg_dump
  version: v1/version
  metadata: v1/metadata            # (inferred default)
  v2_query: v2/query               # (inferred default)
metadata_directory: metadata
migrations_directory: migrations
seeds_directory: seeds
actions:
  kind: synchronous                # default kind for new actions
  handler_webhook_baseurl: http://localhost:3000
  codegen: { framework: ..., output_dir: ..., uri: ... }
```

The server does not read config.yaml. Our boot loader only needs `metadata_directory`, `migrations_directory`, `seeds_directory` and `actions.handler_webhook_baseurl` / `actions.kind`. The last two are used by the CLI when building actions from `actions.graphql` (§1.4).

### 1.2 Config v3 metadata directory (current)

Source: `docs/docs/migrations-metadata-seeds/metadata-format.mdx`, `cli/internal/metadataobject/*`.

```
metadata/
├── version.yaml                      # "version: 3"
├── databases/
│   ├── databases.yaml                # list of sources; tables/functions via !include
│   └── <source_name>/
│       ├── tables/
│       │   ├── tables.yaml           # list of "!include <schema>_<table>.yaml"
│       │   └── <schema>_<table>.yaml # one TableMetadata object
│       └── functions/
│           ├── functions.yaml        # list of "!include <schema>_<fn>.yaml"
│           └── <schema>_<fn>.yaml    # one FunctionMetadata object
├── actions.graphql                   # SDL: action fields on Query/Mutation + custom types
├── actions.yaml                      # {actions: [...], custom_types: {...}} (non-SDL parts)
├── allow_list.yaml                   # allowlist (list)
├── api_limits.yaml                   # api_limits (object)
├── backend_configs.yaml              # backend_configs (object)
├── cron_triggers.yaml                # cron_triggers (list)
├── graphql_schema_introspection.yaml # graphql_schema_introspection (object)
├── inherited_roles.yaml              # inherited_roles (list)
├── metrics_config.yaml               # metrics_config (object)
├── network.yaml                      # network (object)
├── opentelemetry.yaml                # opentelemetry (object)
├── query_collections.yaml            # query_collections (list)
├── remote_schemas.yaml               # remote_schemas (list)
└── rest_endpoints.yaml               # rest_endpoints (list)
```

The docs list the exact file names; `backend_configs.yaml` comes from the CLI metadataobject list (inferred filename).

`databases.yaml` example as exported by the CLI (`cli/internal/metadataobject/sources/sources.go`):

```yaml
- name: default
  kind: postgres
  configuration:
    connection_info:
      database_url: { from_env: HASURA_GRAPHQL_DATABASE_URL }
      isolation_level: read-committed
      pool_settings: { connection_lifetime: 600, idle_timeout: 180, max_connections: 50, retries: 1 }
      use_prepared_statements: true
  tables: "!include default/tables/tables.yaml"
  functions: "!include default/functions/functions.yaml"
```

`tables.yaml`:

```yaml
- "!include public_author.yaml"
- "!include public_article.yaml"
```

Rules confirmed from the CLI exporter (`sources.go` lines ~185–380):

- The `!include` value is emitted as a **double-quoted YAML string scalar** `"!include <relpath>"`, **not** a YAML tag.
  - The CLI reader treats both forms as includes (verified, `cli/internal/metadatautil/yaml.go` `resolveTags`): a node with YAML tag `!include`, **or any plain `!!str` scalar whose value contains the substring `!include`**. For the string form the path is everything after the first space-separated token, with surrounding `"` trimmed.
  - `resolveTags` walks every node (document/sequence/mapping) of the decoded file, so includes are honoured **anywhere** in a file read through this decoder, not only under `tables`/`functions` keys.
  - If the included file's extension is **not `.yaml`** (e.g. `.sql`, `.graphql`), its raw contents are inlined as a **string scalar** (e.g. `code: !include my_query.sql` for a native query). `.yaml` files are parsed and recursively resolved. Note `.yml` is treated as a non-YAML string (verified: `filepath.Ext(...) != ".yaml"`).
- **Include paths are relative to the directory of the file containing them** (verified: the resolver sets the new base to `filepath.Dir(baseDir/value)` before parsing the included file).
  - In `databases.yaml` the base is `metadata/databases/`, so the path is `default/tables/tables.yaml`.
  - In `tables.yaml` the base is `metadata/databases/default/tables/`.
  - The CLI sets `IncludeTagBaseDirectory: filepath.Join(MetadataDir, "databases")` for the root.
  - There is no cycle detection in the CLI (a cycle recurses until failure).
- Table file name: `<schema>_<name>.yaml` for Postgres (`getTableYamlFileName`). Other backends use their own qualified-name joins, e.g. BigQuery `<dataset>_<name>`.
- Function file name: `<schema>_<name>.yaml`. For a function that is a string list it is `strings.Join(parts,"_") + ".yaml"`.
- If a source has no tables, the `tables` key is the include of an empty `tables.yaml` (`[]`) (inferred). Same for functions.
- Native queries, logical models, stored procedures, customization, query_tags and health_check are **inline** in `databases.yaml` on export. There are no separate files (inferred from exporter only splitting tables/functions). On read, users may still `!include` them (see above; the older `upgrade-v3.mdx` wording "tables and functions keys only" is outdated relative to `resolveTags`).
- Export order of files is irrelevant. The order **inside** each file follows server `export_metadata` order, because the CLI converts JSON→YAML preserving key order (`ExportMetadata` comment in `cli/internal/projectmetadata/handler.go`). This is why exported YAML shows `table: {name: ..., schema: ...}` (see the lexicographic nested-key rule in §2.1).

**Design note (P0):** our loader must support `!include` anywhere (YAML tag, or a string scalar containing `!include`), resolved relative to the including file, inlining non-`.yaml` files as strings. Includes may recurse; detect cycles (our addition). This matches the CLI.

### 1.3 Assembly into replace_metadata JSON

`cli/internal/projectmetadata/handler.go`:

- `BuildJSONMetadata()` builds a map by calling `Build()` on each metadata object. Each object reads its file(s) and returns `{key: value}`.
- The map is encoded via an ordered helper struct `Metadata`, whose key order is: `version, sources, tables, functions, actions, custom_types, remote_schemas, query_collections, allowlist, cron_triggers, network, api_limits, rest_endpoints, inherited_roles, opentelemetry, backend_configs, graphql_schema_introspection, metrics_config`. All keys except `version` are `omitempty`.
- The result is converted YAML→JSON.

**`hasura metadata apply`**:

- Default path (server supports v2 replace): `V2ApplyMetadata(disallowInconsistentMetadata)`.
- It POSTs to `/v1/metadata`:
  ```json
  {"type":"replace_metadata","version":2,
   "args":{"allow_inconsistent_metadata": <!--disallow-inconsistent-metadata, default true>,
           "metadata": <assembled JSON>}}
  ```
- So **by default the CLI applies metadata even if it is inconsistent**. It then prints warnings listing `inconsistent_objects` from the response.
- With `--disallow-inconsistent-metadata`, `allow_inconsistent_metadata=false`.
- `allow_warnings` is not sent, so the server default `true` applies (inferred from struct fields `AllowInconsistentMetadata`, `Metadata` only).
- A v1 fallback `V1ApplyMetadata` sends `{"type":"replace_metadata","args":<metadata>}`.
- `--dry-run` prints the assembled metadata and sends nothing.
- After apply, the CLI calls `get_inconsistent_metadata` to report problems (inferred).

**`hasura metadata export`**:

- POSTs `{"type":"export_metadata","args":{}}` (v1 form) and receives the raw metadata JSON.
- Converts JSON→YAML preserving order, then each metadata object's `Export()` writes its file(s).
- Existing files in `metadata/` are removed first (inferred).

**Other `hasura metadata` subcommands:**

| Command | Request |
|---|---|
| `metadata reload` | `{"type":"reload_metadata","args":{}}` |
| `metadata clear` (alias `reset`) | `{"type":"clear_metadata","args":{}}` |
| `metadata inconsistency list/status` | `{"type":"get_inconsistent_metadata","args":{}}` |
| `metadata inconsistency drop` | `{"type":"drop_inconsistent_metadata","args":{}}` |

`hasura metadata diff` is entirely client side: export plus a local build.

### 1.4 actions.graphql + actions.yaml

`cli/internal/metadataobject/actions/actions.go`:

- `actions.graphql` is plain SDL. It contains:
  - `type Query { … }` / `type Mutation { … }`, possibly written as `extend type …`, whose fields are actions with arguments and an output type;
  - `type`, `input`, `enum` and `scalar` definitions, which become `custom_types`.
- `actions.yaml` holds what SDL cannot express:
  ```yaml
  actions:
    - name: login
      definition:
        kind: synchronous
        handler: http://host/login
        forward_client_headers: true
        headers: [...]
        timeout: 30
        request_transform: ...
        response_transform: ...
      permissions: [{role: user}]
      comment: ...
  custom_types:
    enums: [...]
    input_objects: [...]
    objects: [{name: LoginOutput, relationships: [...]}]
    scalars: [...]
  ```
- On build (`ActionConfig.Build`, used by `metadata apply`), the CLI calls its bundled **cli-ext** (a Node binary) `ConvertSDLToMetadata` (`sdl from`), which parses the SDL into `{actions, types}`.
- Then, **for each action listed in actions.yaml** (matched by name), it copies from actions.yaml onto the SDL-derived action: `permissions`, `comment`, `definition.timeout`, `definition.kind`, `definition.handler`, `definition.forward_client_headers`, `definition.headers`, `definition.request_transform`, `definition.response_transform`. `definition.type` is **not** copied in Build (it always comes from the SDL parent type; only `hasura actions create` copies `type`). Verified, `cli/internal/metadataobject/actions/actions.go` Build.
- For each custom type listed in actions.yaml (per kind: enums, input_objects, objects, scalars), its `description` and `relationships` **replace** the SDL-derived ones — so a type listed in actions.yaml without a `description` loses any SDL description; a type not listed keeps the SDL description. Field/value/argument descriptions always come from the SDL.
- **No defaults are applied at build time.** The `<actions.handler_webhook_baseurl>/<action_name>` handler default and the `actions.kind` default are only applied by `hasura actions create` (verified, same file ~l.175–181). An SDL action with no actions.yaml entry is sent with `"kind": ""` and `"handler": ""` (Go struct `ActionDef` has `yaml:"kind"`/`yaml:"handler"` without omitempty). The server then rejects `kind: ""` for a mutation (the `ActionMutationKind` parser only knows `synchronous`/`asynchronous`); for a query `kind` is ignored. Our loader should be lenient here (treat empty as absent → `synchronous`) — a safe superset.
- Errors:
  - an action in actions.yaml that is absent from actions.graphql → "action X is not present in actions.graphql";
  - the same check for custom types: "custom type X is not present in actions.graphql".
- Mapping from SDL to metadata (verified, `cli-ext/src/services/sdl/from/handler.js` + `cli-ext/src/shared/utils/sdlUtils.js` `getAllActionsFromSdl`/`getAllTypesFromSdl`):
  - every definition **named** `Query` or `Mutation` (so both `type Mutation {…}` and `extend type Mutation {…}`) contributes one action per field; `Mutation` → `definition.type: "mutation"`, `Query` → `"query"`;
  - each action is `{name, definition: {type, arguments, output_type}}` — **the SDL description on the action field is dropped** (no `comment`); `comment` only comes from actions.yaml;
  - arguments → `[{name, type: "<GraphQL type string e.g. [String!]!>", description}]`, where `description` is the trimmed SDL description or JSON `null`;
  - `output_type` = the field's type string including `!`/`[]`;
  - all other definitions become custom types: `scalar` → `{name, description}`, `enum` → `{name, description, values: [{value, description}]}`, `input` → `{name, description, fields: [{name, type, description}]}`, `type` → `{name, description, fields: [{name, type, description}]}` (object-field **arguments are not carried**). Descriptions are trimmed or `null`. `is_deprecated` is never produced. Interface/schema definitions produce nameless error entries that `reformCustomTypes` drops (inferred).
  - `types` is `{scalars, input_objects, objects, enums}` (all keys present, possibly empty lists).
- On export the CLI does the reverse (`ConvertMetadataToSDL`). It writes `actions.graphql` with only the SDL (action `comment` is rendered as a `""" … """` description, which is then ignored on the next build — the authoritative comment is the one in actions.yaml), and `actions.yaml` with the remainder.

**Design note:** to load a directory at boot without the CLI, we must implement this SDL→action conversion in Rust. We have a GraphQL parser anyway. This is P0, since actions are common.

### 1.5 Config v2 layout (legacy, P1)

From `docs/docs/migrations-metadata-seeds/legacy-configs/config-v2/*`:

```
metadata/
├── version.yaml        # version: 2
├── tables.yaml         # list of TableMetadata (all in "default" source)
├── functions.yaml      # list of FunctionMetadata (v2) or qualified names
├── actions.graphql, actions.yaml, allow_list.yaml, cron_triggers.yaml,
├── query_collections.yaml, remote_schemas.yaml
migrations/<version>_<name>/{up.sql,down.sql,up.yaml,down.yaml}   # single DB
```

The assembled JSON is `{"version":2,"tables":[...],"functions":[...],...}`. The server converts it into a `default` source (§4.4).

State in config v2 lives in the `hdb_catalog.schema_migrations` table (§6.3).

Config v1 (pre-1.0) has no metadata dir: `migrations/<version>_<name>.{up,down}.{sql,yaml}` plus `metadata.yaml`. The YAML migration files are lists of `/v1/query` API calls.

### 1.6 Design note: boot-time directory loading (our replacement for cli-migrations-v3)

Upstream behaviour (verified, `packaging/cli-migrations/v3/docker-entrypoint.sh`): the `hasura/graphql-engine:<v>.cli-migrations-v3` image starts a temporary server with `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING=true graphql-engine serve --enabled-apis=metadata --server-port=$HASURA_GRAPHQL_MIGRATIONS_SERVER_PORT`, waits for the port, then (each step only if its directory exists) runs `hasura-cli metadata apply [--disallow-inconsistent-metadata]`, then `hasura-cli migrate apply --all-databases` followed by `hasura-cli metadata reload`, kills the temporary server and `exec`s the real command. It writes a minimal `config.yaml` (`version: 3`, `endpoint`, `metadata_directory: metadata`) — no `actions:` section. Env:
- `HASURA_GRAPHQL_MIGRATIONS_DIR` (default `/hasura-migrations`);
- `HASURA_GRAPHQL_METADATA_DIR` (default `/hasura-metadata`);
- `HASURA_GRAPHQL_MIGRATIONS_SERVER_PORT` (default 9691);
- `HASURA_GRAPHQL_MIGRATIONS_SERVER_TIMEOUT` (default 30, seconds);
- `HASURA_GRAPHQL_DISALLOW_INCONSISTENT_METADATA` (if set to `true`, metadata apply passes `--disallow-inconsistent-metadata`);
- `HASURA_GRAPHQL_MIGRATIONS_DATABASE_ENV_VAR` belongs to the older config-v2 (`cli-migrations-v2`) image (inferred; not in the v3 entrypoint).

Source: `docs/docs/migrations-metadata-seeds/auto-apply-migrations.mdx`, the entrypoint script above.

Recommended Rust behaviour, supporting the same env vars:

1. Bootstrap/upgrade `hdb_catalog` (§3).
2. If `HASURA_GRAPHQL_METADATA_DIR` is set and exists, load and assemble it (`!include`, actions SDL) into the same JSON the CLI would build.
3. Run `replace_metadata` (v2 semantics) with `allow_inconsistent_metadata=true` (or `false` if `HASURA_GRAPHQL_DISALLOW_INCONSISTENT_METADATA=true`). This writes `hdb_metadata` and bumps `resource_version`.
   - The cli-migrations image order is **metadata apply → migrate apply → metadata reload**. Metadata may reference tables that the migrations create, so the first apply is allowed to be inconsistent.
4. Apply pending migrations for each source listed in metadata. Use directory `<migrations_dir>/<source_name>/`. Config v2 uses `<migrations_dir>/` for `default`. Record state in `hdb_version.cli_state` exactly like the CLI (§6.3), so that later `hasura migrate status` agrees.
5. Reload metadata (rebuild the schema cache).
6. If still inconsistent, log the inconsistent objects. Continue serving unless a strict flag (our own, e.g. `HASURA_GRAPHQL_METADATA_STRICT`) says to fail.
7. Optionally (our extension) watch the directory in dev mode.

---

## 2. Metadata JSON (v3) — the data model

Primary sources:
- `server/src-lib/Hasura/RQL/Types/Metadata.hs` (top level)
- `.../Metadata/Common.hs` (SourceMetadata)
- `.../Metadata/Serialization.hs` (export ordering)
- `server/src-lib/Hasura/Table/Metadata.hs`
- `server/src-lib/Hasura/RQL/Types/*`

The OpenAPI schema `metadata.openapi.json` is generated from autodocodec codecs. It mostly agrees with aeson; the known disagreement is noted in §2.13.

### 2.1 Top level

```rust
/// Parsing: Metadata.hs `instance FromJSON Metadata`
struct Metadata {
    version: MetadataVersion,            // required in practice: default 1, but must equal 3 here
    sources: Vec<SourceMetadata>,        // REQUIRED key; map keyed by name, LAST WINS on dup (oMapFromL)
    #[serde(default)] remote_schemas: Vec<RemoteSchemaMetadata>,  // dup names -> error
    #[serde(default)] query_collections: Vec<CreateCollection>,   // dup -> error
    #[serde(default)] allowlist: Vec<AllowlistEntry>,             // dup by collection -> error
    #[serde(default)] actions: Vec<ActionMetadata>,               // dup -> error
    #[serde(default)] custom_types: CustomTypes,                  // default empty
    #[serde(default)] cron_triggers: Vec<CronTriggerMetadata>,    // dup -> error
    #[serde(default)] rest_endpoints: Vec<CreateEndpoint>,        // NO dup check: oMapFromL by name, LAST WINS
    #[serde(default)] api_limits: ApiLimit,                       // default empty
    #[serde(default)] metrics_config: MetricsConfig,              // default {false,false}
    #[serde(default)] inherited_roles: Vec<InheritedRole>,        // dup by role_name -> error
    #[serde(default)] graphql_schema_introspection: SetGraphqlIntrospectionOptions, // default {}
    #[serde(default)] network: Network,                           // default {tls_allowlist: []}
    #[serde(default)] backend_configs: BackendConfigs,            // default {}
    #[serde(default)] opentelemetry: OpenTelemetryConfig,         // default "empty" (status disabled)
}
enum MetadataVersion { V1 = 1, V2 = 2, V3 = 3 }   // integers; other -> "expected 1, 2 or 3, encountered N"
```

Parse rules:

- `FromJSON Metadata` reads `version` (default `MVVersion1`). If it is not 3: fail `"unexpected metadata version from storage: <v>"`.
  - So a stored or replace-v3 payload must say 3. For `replace_metadata`, versions 1/2 go through `MetadataNoSources` instead (§4.4).
- Duplicate names inside lists fail via `parseListAsMap` (`Hasura/RQL/Types/Metadata/Common.hs` ~l.92), with message `"multiple declarations exist for the following <things>: <names>"` (names comma-separated by `T.commaSeparated`). Exact `<things>` strings (verified, `parseNonSourcesMetadata`): `remote schemas`, `query collections`, `allowlist entries`, `actions`, `cron triggers`, `inherited roles`.
- Sources and `rest_endpoints`: `oMapFromL` — **no duplicate check, last one wins** (quirk to preserve; verified `FromJSON Metadata`, `Metadata.hs` ~l.177–178).
- Unknown top-level keys are **ignored** (aeson object parser, no strictness).
- Each source is parsed by dispatching on `kind` (default `postgres`) to the backend's `SourceMetadata b` parser.
  - An unknown kind is accepted as a data-connector name (`BackendSourceKind` parsing). It then fails at schema build if the agent is unknown (inferred).

Export: `metadataToOrdJSON` (`Serialization.hs`). Key order is:

```
version, sources, remote_schemas, query_collections, allowlist, actions, custom_types,
cron_triggers, rest_endpoints, api_limits, metrics_config, inherited_roles,
graphql_schema_introspection, network, backend_configs, opentelemetry
```

- Each key except `version` and `sources` is **omitted when empty/default**: empty list, empty custom types, `emptyApiLimit`, `emptyMetricsConfig`, `emptyNetwork`, empty introspection options, empty backend_configs, empty opentelemetry.
- `sources` is always present, even as `[]`.
- Lists are sorted (all via a stable `sortOn`, verified `Serialization.hs`):
  - sources by name; remote schemas by name; query collections by name; actions by name; cron triggers by name;
  - **allowlist by `collection`** (`allowlistToOrdJSONList`, `listToMaybeArraySort … aeCollection`);
  - rest endpoints by **URL only** (`sortOn _ceUrl`); ties keep insertion order (stable sort);
  - inherited roles by role_name.
  - **Not** sorted (insertion order kept): custom types lists, remote schema `permissions` and `remote_relationships`, select-permission `computed_fields`, action `permissions`/`arguments`, collection queries.

**Nested key order rule (important, verified):** only the objects built by the hand-written `AO.object [...]` serializers in `Metadata.hs`/`Serialization.hs` have the export orders listed in this doc (top level, source, table, rel/perm/event-trigger/computed-field/function wrappers, remote schema + its `definition`, action + its `definition`, cron trigger, custom types, collections, inherited roles, opentelemetry). Every other sub-object is produced as an aeson `Value` and converted with `AO.toOrdered`, which emits keys in **KeyMap order = lexicographic by key** (aeson 2.3 with the default `ordered-keymap` flag, `cabal.project.freeze`). Examples: `{"name":…,"schema":…}` for `QualifiedTable`; `configuration` → `connection_info` → `database_url, isolation_level, pool_settings, use_prepared_statements`; `pool_settings` → `connection_lifetime, idle_timeout, max_connections, retries`; table `configuration` → `column_config, comment, custom_column_names, custom_name, custom_root_fields`; allowlist entry `{collection, scope}`; boolean expressions and `using` blocks also come out key-sorted. The docs example `public_author.yaml` (`docs/.../metadata-format.mdx` l.157) shows exactly this. A Rust implementation can get this for free by serialising those sub-objects through a `BTreeMap`/sorted `serde_json::Map` (do **not** enable `preserve_order` for them).

**Exactness matters:** the console and CLI diff tools compare exported metadata. Exact key order and omission rules keep `hasura metadata diff` quiet. Order alone is not semantic, but we should match it.

`emptyMetadata` is the v3 metadata with `sources: []` and everything else empty.

### 2.2 MetadataDefaults (P1)

`HASURA_GRAPHQL_METADATA_DEFAULTS` / `--metadata-defaults` takes a JSON object in metadata shape. Defined in `Hasura/RQL/Types/Metadata.hs` (`MetadataDefaults`, `overrideMetadataDefaults`). Behaviour:

- It is merged into the metadata used to **build the schema cache**.
- It is **not** stored or exported, and it is not used by modifying ops.
- Merge rules:
  - collections: union, with user entries winning;
  - `api_limits` (and similar) use `<|>` per field, so a user value wins;
  - `metrics_config` booleans are OR'd.
- In practice it is used by Hasura Cloud for `backend_configs.dataconnector` agent defaults.

### 2.3 Source

`Hasura/RQL/Types/Metadata/Common.hs` (`SourceMetadata`), `Serialization.hs` (`sourcesToOrdJSONList`).

```rust
struct SourceMetadata {
    name: SourceName,                              // required; "default" is special-cased in places
    #[serde(default = "postgres")] kind: BackendSourceKind, // "postgres"|"pg"|"citus"|"cockroach"|"mssql"|"bigquery"|<dc name>
    tables: Vec<TableMetadata>,                    // REQUIRED; NO dup check (oMapFromL by `table`, last wins)
    #[serde(default)] functions: Vec<FunctionMetadata>,         // NO dup check, last wins
    #[serde(default)] native_queries: Vec<NativeQueryMetadata>, // NO dup check (by root_field_name), last wins
    #[serde(default)] stored_procedures: Vec<StoredProcedureMetadata>, // MSSQL only; no dup check
    #[serde(default)] logical_models: Vec<LogicalModelMetadata>,       // NO dup check (by name), last wins
    // verified: FromJSONWithContext SourceMetadata, Metadata/Common.hs ~l.156-168 uses oMapFromL, not parseListAsMap.
    // Same for MetadataNoSources.tables/functions (v1/v2).
    configuration: SourceConnConfiguration,        // REQUIRED, backend-specific (PostgresConnConfiguration)
    #[serde(default)] query_tags: Option<QueryTagsConfig>,
    #[serde(default)] customization: SourceCustomization,       // default empty
    #[serde(default)] health_check: Option<HealthCheckConfig>,  // EE
}
```

Kind aliases: `pg` and `postgres` both mean vanilla Postgres. Export writes `postgres` (`BackendSourceKind` ToJSON uses the canonical name, inferred).

Export key order:
- `name, kind, tables, functions, native_queries, stored_procedures, logical_models, configuration, query_tags, customization, health_check`.
- `functions`, `native_queries`, `stored_procedures` and `logical_models` are omitted when empty. `query_tags` and `health_check` are omitted when Nothing. `customization` is omitted when empty.
- `tables` is always present.
- `tables` is sorted by the `table` name. For PG, `QualifiedTable` ordering is (schema, name).
- `functions` is sorted by function name; native queries by root field name; logical models by name.

### 2.4 Postgres source configuration

`server/src-lib/Hasura/Backends/Postgres/Connection/Settings.hs`, `Hasura/RQL/Types/Common.hs` (UrlConf), `Hasura/Backends/Postgres/Connection/*`.

```rust
struct PostgresConnConfiguration {
    connection_info: PostgresSourceConnInfo,          // required
    #[serde(default)] read_replicas: Option<Vec<PostgresSourceConnInfo>>,   // EE; omitted when None
    #[serde(default = "public")] extensions_schema: String,                 // omitted on export when "public"
    #[serde(default)] connection_template: Option<ConnectionTemplate>,      // EE: {version: 1, template: "<kriti>"}
    #[serde(default)] connection_set: Option<Vec<ConnectionSetElement>>,    // EE: [{name, connection_info}]
}

struct PostgresSourceConnInfo {
    database_url: UrlConf,                                       // required
    #[serde(default)] pool_settings: Option<PostgresPoolSettings>,
    #[serde(default)] use_prepared_statements: bool,             // default false
    #[serde(default)] isolation_level: TxIsolation,              // default "read-committed"
    #[serde(default)] ssl_configuration: Option<PGClientCerts>,  // cloud/EE
}

#[serde(untagged)]
enum UrlConf {
    Value(String),                                    // InputWebhook-like template; "{{ENV}}" interpolation
    FromEnv { from_env: String },
    Params { connection_parameters: ConnectionParameters },
    DynamicFromFile { dynamic_from_file: String },    // EE-ish
}
// errors: object with none of from_env/connection_parameters/dynamic_from_file
//                                  -> 'Either "from_env" or "connection_parameters" should be provided'
//         object with >1 of them   -> 'Only one of "from_env" or "connection_parameters" should be provided'
//         neither string nor object -> "one of string or object must be provided for url/webhook"
// export: string | {"from_env":..} | {"connection_parameters":..} | {"dynamic_from_file":..}
struct ConnectionParameters { username: String, password: Option<String>, host: String, port: i32, database: String }
// password omitted on export when None; keys exported lexicographically (database, host, password?, port, username)

enum TxIsolation { #[serde(rename="read-committed")] ReadCommitted, #[serde(rename="repeatable-read")] RepeatableRead, Serializable }

struct PostgresPoolSettings {          // all keys optional on input; export omits None (omitNothingFields)
    max_connections: Option<i32>,      // default 50 (applied at pool creation, not stored)
    total_max_connections: Option<i32>,// EE/cloud; no default
    idle_timeout: Option<i32>,         // default 180 (s)
    retries: Option<i32>,              // default 1
    pool_timeout: Option<NonNegativeDiffTime>, // default none (wait forever)
    connection_lifetime: Option<NonNegativeDiffTime>,
    // PARSE QUIRK (verified, Hasura/Server/Utils.hs parseConnLifeTime): absent -> Some(600); 0 -> None; n -> Some(n).
    // So the default 600 IS materialised into metadata whenever a pool_settings object is present, and
    // `connection_lifetime: 0` disappears on export.
}

struct PGClientCerts {                 // keys: sslcert, sslkey, sslrootcert, sslmode, sslpassword (aesonDrop 3 lowercase)
    sslmode: String,                   // "disable"|"allow"|"prefer"|"require"|"verify-ca"|"verify-full"; else "Invalid SSL Mode <x>"
    sslcert: Option<FromEnv>, sslkey: Option<FromEnv>, sslrootcert: Option<FromEnv>, // each {from_env}
    sslpassword: Option<CertPassword>, // string or {from_env} (inferred)
}
```

Notes:
- The string form of `database_url` supports `{{ENV_VAR}}` templating (`InputWebhook`-style resolution, `Hasura/RQL/Types/Common.hs`).
- A URL with scheme `dynamic-from-file://` is also recognised for the default DB (`App.hs`).
- Pool defaults are applied when building the pool; the metadata keeps what the user wrote, **except** `connection_lifetime` (see quirk above: an absent value becomes `600` on re-export, `0` is dropped).
  - The bootstrapped `default` source writes explicit values from the server flags (§3.4).
- `PostgresConnConfiguration` export: `connection_info` always; `read_replicas`, `connection_template`, `connection_set` only when set; `extensions_schema` only when ≠ `"public"` (keys lexicographic per §2.1).
- `use_prepared_statements` default: false in the parser.
  - The default source created at bootstrap uses the `--use-prepared-statements` flag value (default true).
  - That explains why exported metadata often shows `true` (§3.4).

### 2.5 Source customization, query tags, health check

`Hasura/RQL/Types/SourceCustomization.hs`:

```rust
struct SourceCustomization {                         // all optional; export omits Nones
    root_fields: Option<RootFieldsCustomization>,    // {namespace?, prefix?, suffix?}
    type_names: Option<SourceTypeCustomization>,     // {prefix?, suffix?}
    naming_convention: Option<NamingCase>,           // "hasura-default" | "graphql-default"
}
```

`graphql-default` requires experimental feature `naming_convention`. Using it without the feature yields an inconsistency/error (inferred). See the schema spec for details.

`Hasura/RQL/Types/QueryTags.hs`, `Hasura/QueryTags/Types.hs`:

```rust
struct QueryTagsConfig {
    #[serde(default)] disabled: bool,                        // false
    #[serde(default)] format: QueryTagsFormat,               // "standard" (default) | "sqlcommenter" | "standard_prepended"; parsed case-insensitively
    #[serde(default = "true")] omit_request_id: bool,        // true
}
// bad format -> "Not a valid query tags format value. Use either standard, sqlcommenter, or standard_prepended"
// export: all three keys always emitted (disabled, format, omit_request_id)
```

`Hasura/RQL/Types/HealthCheck.hs` (EE):

```rust
struct HealthCheckConfig {
    #[serde(default)] test: HealthCheckTest,     // {sql: "SELECT 1"} for PG
    interval: u32,                                // seconds, REQUIRED
    #[serde(default = 3)] retries: u32,
    #[serde(default = 10)] retry_interval: u32,
    #[serde(default = 10)] timeout: u32,
}
```

### 2.6 Table

`server/src-lib/Hasura/Table/Metadata.hs`.

```rust
struct TableMetadata {
    table: QualifiedTable,                                   // required
    #[serde(default)] is_enum: bool,                         // export only when true
    #[serde(default)] configuration: TableConfig,            // export only when != emptyTableConfig
    #[serde(default)] object_relationships: Vec<RelDef<ObjRelUsing>>,
    #[serde(default)] array_relationships: Vec<RelDef<ArrRelUsing>>,
    #[serde(default)] computed_fields: Vec<ComputedFieldMetadata>,
    #[serde(default)] remote_relationships: Vec<RemoteRelationship>,
    #[serde(default)] insert_permissions: Vec<PermDef<InsPerm>>,
    #[serde(default)] select_permissions: Vec<PermDef<SelPerm>>,
    #[serde(default)] update_permissions: Vec<PermDef<UpdPerm>>,
    #[serde(default)] delete_permissions: Vec<PermDef<DelPerm>>,
    #[serde(default)] event_triggers: Vec<EventTriggerConf>,
    #[serde(default)] apollo_federation_config: Option<ApolloFederationConfig>, // {enable: "v1"}
    #[serde(default)] logical_model: Option<LogicalModelName>,
}
```

**Strict keys:** TableMetadata's FromJSON fails on any key outside the 14 above. The error is `"unexpected keys when parsing TableMetadata: [k1,k2]"` (Haskell `show` of a list of Text, e.g. `["foo"]`).

- This is the main place where a typo is an error rather than ignored.
- Each list is a map keyed by name (relationships, computed fields, remote relationships, event triggers) or role (permissions). Duplicates fail with `"multiple declarations exist for the following <kind>: <names>"`.
  - Kinds: "object relationships", "array relationships", "computed fields", "remote relationships", "insert permissions", "select permissions", "update permissions", "delete permissions", "event triggers".

Export key order:
- `table, is_enum, configuration, object_relationships, array_relationships, computed_fields, remote_relationships, insert_permissions, select_permissions, update_permissions, delete_permissions, event_triggers, apollo_federation_config, logical_model`.
- Every list is omitted when empty, and is sorted by name (relationships, computed fields, remote relationships, event triggers) or by role (permissions).

#### QualifiedTable / QualifiedFunction (legacy string form)

`server/src-lib/Hasura/Backends/Postgres/SQL/Types.hs` (`QualifiedObject`):

```rust
#[serde(untagged)]
enum QualifiedObjectInput { Bare(String) /* schema = "public" */, Obj { #[serde(default="public")] schema: String, name: String } }
// any other JSON type -> "expecting a string/object for QualifiedObject"
// Export ALWAYS the object form. ToJSON (Types.hs ~l.304) lists schema then name, BUT it builds an aeson
// KeyMap, so the emitted order is lexicographic: {"name": ..., "schema": ...} (see §2.1 nested-key rule;
// matches the docs' exported `table: {name: author, schema: public}`).
```

For non-PG backends `table` differs: MSSQL `{schema,name}`, BigQuery `{dataset,name}`, data connectors use an array of strings. These are out of scope, but keep raw.

#### TableConfig

`server/src-lib/Hasura/Table/Cache.hs` (`TableConfig`, `TableCustomRootFields`, `ColumnConfig`).

```rust
struct TableConfig {
    #[serde(default)] custom_root_fields: TableCustomRootFields,  // default empty
    #[serde(default)] column_config: HashMap<Column, ColumnConfig>, // {custom_name?, comment?}
    #[serde(default)] custom_column_names: HashMap<Column, Name>,   // DEPRECATED, merged: column_config wins
    #[serde(default)] custom_name: Option<Name>,                    // GraphQL name for the table
    #[serde(default)] comment: Option<String>,
}
struct ColumnConfig { custom_name: Option<Name>, comment: Option<String> }
```

Parse:
- `column_config` is merged with `custom_column_names`. Each `custom_column_names` entry becomes `ColumnConfig{custom_name, comment: Automatic}`. The maps are combined with `HashMap.unionWith (<>)` (column_config first), i.e. per-column the `ColumnConfig` semigroup merges the two, with `column_config` values taking precedence (`Cache.hs` ~l.917–922).
- `comment` is a `Comment` (absent/null → Automatic).

Export (`Cache.hs` ~l.924):
- Keys `custom_root_fields, custom_column_names, column_config, custom_name, comment` with null-valued keys removed; emitted in **lexicographic order** (`column_config, comment, custom_column_names, custom_name, custom_root_fields`) per the §2.1 nested-key rule.
- `custom_column_names` is **derived** from `column_config` (columns that have a custom_name).
- `column_config` drops entries that are entirely empty.
- `custom_root_fields`, `custom_column_names` and `column_config` are always present (possibly `{}`) when configuration itself is exported. Configuration is only exported when not equal to `emptyTableConfig`.

```rust
struct TableCustomRootFields {      // all optional, default None
    select, select_by_pk, select_aggregate, select_stream,
    insert, insert_one, update, update_by_pk, update_many, delete, delete_by_pk: Option<CustomRootField>,
}
#[serde(untagged)]
enum CustomRootField { Null, Name(GraphQLName), Obj { name: Option<GraphQLName>, comment: Option<String> /* Comment */ } }
```

- Parse quirk (verified, `Cache.hs` ~l.217): a **string** that is not a valid GraphQL name is silently treated as unset (`G.mkName` → Nothing), not an error. Non string/object/null → `"parsing CustomRootField failed, expected Object, String or Null, …"`.
- Validation: duplicates among the given names fail with `"the following custom root field names are duplicated: <names>"`, names joined English-style with "and" (`englishList "and"`).
- Export (verified): unset (no name, Automatic comment) → omitted; name + Automatic comment → string; otherwise object `{"comment": …, "name": …}` (name may be `null`; Explicit-empty comment → `""`).

`Comment` semantics (`Hasura/RQL/Types/Common.hs`, `Comment`):
- absent/`null` → `Automatic` (generated description);
- `""` → explicit empty (no description);
- `"text"` → explicit.

### 2.7 Relationships

`server/src-lib/Hasura/RQL/Types/Relationships/Local.hs`.

```rust
struct RelDef<U> { name: RelName, using: U, #[serde(default)] comment: Option<String> }
// export: name, using, comment (only if Some)

// Object relationship `using` — exactly one key
enum ObjRelUsing {
    ForeignKeyConstraintOn(ObjRelFk),       // key "foreign_key_constraint_on"
    ManualConfiguration(RelManualConfig),   // key "manual_configuration"
}
// errors: none present -> "atleast one of foreign_key_constraint_on/manual_configuration should be present"
//         both present -> "only one of foreign_key_constraint_on/manual_configuration should be present"

#[serde(untagged)]
enum ObjRelFk {
    Column(String),                                   // same-table FK column: "author_id"
    Columns(Vec<String>),                             // same-table composite FK (NonEmpty)
    Remote { table: QualifiedTable, column: String }, // FK on the OTHER table pointing here
    RemoteMany { table: QualifiedTable, columns: Vec<String> },
}

// Array relationship `using`
enum ArrRelUsing {
    ForeignKeyConstraintOn(ArrRelFk),       // {table, column} | {table, columns}
    ManualConfiguration(RelManualConfig),
}

struct RelManualConfig {
    // exactly one target:
    remote_table: Option<QualifiedTable>,
    remote_native_query: Option<NativeQueryName>,   // native query target (P1)
    column_mapping: IndexMap<Column, Column>,       // ordered
    #[serde(default)] insertion_order: Option<InsertOrder>,  // "before_parent" | "after_parent" | null
}
```

ToJSON of `ArrRelFk` / remote `ObjRelFk`: a single column is written as `column`, several as `columns` (verified, `Relationships/Local.hs` ~l.290–345). Both `column` and `columns` accept a string or an array on input.

Other parse errors (verified): `using` not an object → `"using should be an object"`; both/neither of `column`/`columns` → `"expected exactly one of 'column' or 'columns'"`; array-rel fk not an object → `"Expecting object { table, columns }."`; bad column value → `"Expected string or array"`; bad `insertion_order` → `"insertion_order should be 'before_parent' or 'after_parent'"`.

The legacy form `foreign_key_constraint_on: {table, column}` for array relationships is the classic one; the `columns` variant was added later. Both are accepted.

#### Remote relationships (table → source / remote schema)

`Hasura/RQL/Types/Relationships/Remote.hs`, `ToSource.hs`, `Hasura/RemoteSchema/Metadata/RemoteRelationship.hs`.

```rust
struct RemoteRelationship { name: RelName, definition: RemoteRelationshipDefinition }

// Parse (RRParseMode: lenient for metadata):
//   1. if key "to_source" present -> ToSource
//   2. if key "to_remote_schema" present -> ToRemoteSchema
//   3. else legacy: {remote_schema, hasura_fields, remote_field} -> ToRemoteSchema(legacy=true)
// Export preserves the format it was parsed from (legacy stays legacy).
enum RemoteRelationshipDefinition {
    ToSource { to_source: ToSourceRelationshipDef },
    ToRemoteSchema { to_remote_schema: ToSchemaRelationshipDef },
    LegacyToRemoteSchema { remote_schema: RemoteSchemaName, hasura_fields: Vec<FieldName>, remote_field: RemoteFields },
}
struct ToSourceRelationshipDef {
    relationship_type: RelType,                     // "object" | "array"
    field_mapping: IndexMap<FieldName, FieldName>,  // lhs field -> target column
    source: SourceName,
    table: serde_json::Value,                       // target backend's TableName (PG: QualifiedTable)
}
struct ToSchemaRelationshipDef {
    remote_schema: RemoteSchemaName,
    lhs_fields: Vec<FieldName>,                     // set of LHS fields (export as list)
    remote_field: RemoteFields,
}
// RemoteFields: {"<rootField>": {"arguments": {"arg": "$lhsField" | literal | {...}}, "field": {<nested>}?}}
// exactly one top-level key; nested via "field".
```

Create-op note: `create_remote_relationship` accepts both `{name, definition}` and the legacy top-level `remote_schema/hasura_fields/remote_field` (§4.6).

### 2.8 Computed fields

`Hasura/Backends/Postgres/Types/ComputedField.hs`, `Hasura/RQL/Types/ComputedField.hs`.

```rust
struct ComputedFieldMetadata { name: ComputedFieldName, definition: PgComputedFieldDefinition, #[serde(default)] comment: Option<String> }
struct PgComputedFieldDefinition {
    function: QualifiedFunction,                    // string or {schema,name}
    #[serde(default)] table_argument: Option<FunctionArgName>,   // default: first argument
    #[serde(default)] session_argument: Option<FunctionArgName>,
}
```

`comment` is a `Comment` with the same null/""/text semantics as above (inferred).

### 2.9 Permissions

`Hasura/RQL/Types/Permission.hs`.

```rust
struct PermDef<P> { role: RoleName, permission: P, #[serde(default)] comment: Option<String> }
// export: role, permission, comment(only if Some)

#[serde(untagged)] enum PermColSpec { Star /* "*" */, List(Vec<Column>) }

struct InsPerm {
    check: BoolExp,                                          // REQUIRED
    #[serde(default)] set: Option<HashMap<Column, Value>>,   // column presets; "X-Hasura-*" strings = session var
    #[serde(default)] columns: Option<PermColSpec>,          // None => (behaviour: all columns? see perms spec)
    #[serde(default)] backend_only: bool,                    // false
    #[serde(default)] validate_input: Option<ValidateInput>,
}
struct SelPerm {
    columns: PermColSpec,                                    // REQUIRED
    #[serde(default)] computed_fields: Vec<ComputedFieldName>,
    filter: BoolExp,                                         // REQUIRED
    #[serde(default)] limit: Option<i32>,
    #[serde(default)] allow_aggregations: bool,
    #[serde(default)] query_root_fields: Option<Vec<QueryRootField>>,        // "select"|"select_by_pk"|"select_aggregate"
    #[serde(default)] subscription_root_fields: Option<Vec<SubscriptionRootField>>, // + "select_stream"
}
struct UpdPerm {
    columns: PermColSpec,                                    // REQUIRED
    #[serde(default)] set: Option<HashMap<Column, Value>>,
    filter: BoolExp,                                         // REQUIRED
    #[serde(default)] check: Option<BoolExp>,                // post-update check
    #[serde(default)] backend_only: bool,
    #[serde(default)] validate_input: Option<ValidateInput>,
}
struct DelPerm {
    filter: BoolExp,                                         // REQUIRED
    #[serde(default)] backend_only: bool,
    #[serde(default)] validate_input: Option<ValidateInput>,
}
// ValidateInput: {"type": "http", "definition": {url: InputWebhook, #[default []] headers: Vec<HeaderConf>,
//                 #[default 10] timeout: u32, #[default false] forward_client_headers: bool}}
```

`BoolExp` is opaque JSON at the metadata level (parsed later against the table). See the permissions spec.

Export orders (`Serialization.hs`):

| Perm | Key order and omission |
|---|---|
| insert | `check`; `set` (if non-empty); `columns`; `backend_only` (if true); `validate_input` (if present) |
| select | `columns`; `computed_fields` (if non-empty); `filter`; `limit` (if present); `allow_aggregations` (if true); `query_root_fields` / `subscription_root_fields` (only if present) |
| update | `columns`; `filter`; `check` (**always emitted, `null` if absent**); `set` (if non-empty); `backend_only` (if true); `validate_input` (if present) |
| delete | `filter`; `backend_only` (if true); `validate_input` (if present) |

### 2.10 Event triggers

`Hasura/RQL/Types/EventTrigger.hs`.

```rust
struct EventTriggerConf {
    name: TriggerName,                       // ^[A-Za-z]+[A-Za-z0-9_\-]*$, max 42 chars (validated in replace/create)
    definition: TriggerOpsDef,
    #[serde(default)] webhook: Option<InputWebhook>,          // exactly one of webhook / webhook_from_env
    #[serde(default)] webhook_from_env: Option<String>,
    retry_conf: RetryConf,
    #[serde(default)] headers: Option<Vec<HeaderConf>>,
    #[serde(default)] request_transform: Option<RequestTransform>,
    #[serde(default)] response_transform: Option<MetadataResponseTransform>,
    #[serde(default)] cleanup_config: Option<AutoTriggerLogCleanupConfig>,
    #[serde(default)] trigger_on_replication: bool,           // PG default false; export omitted when == backend default
}
struct TriggerOpsDef {                       // export omits None keys (omitNothingFields)
    #[serde(default)] insert: Option<SubscribeOpSpec>,
    #[serde(default)] update: Option<SubscribeOpSpec>,
    #[serde(default)] delete: Option<SubscribeOpSpec>,
    #[serde(default)] enable_manual: Option<bool>,   // Maybe Bool: absent stays absent, explicit false is kept
}
struct SubscribeOpSpec { columns: SubscribeColumns /* "*" | [cols] */, #[serde(default)] payload: Option<SubscribeColumns> }
// RetryConf in METADATA: num_retries and interval_sec are REQUIRED (genericParseJSON, plain Int);
// timeout_sec optional (omitted on export when None). The 0/10/60 defaults (defaultRetryConf) only apply
// when `retry_conf` is omitted in the create_event_trigger API.
struct RetryConf { num_retries: i32, interval_sec: i32, #[serde(default)] timeout_sec: Option<i32> }
struct AutoTriggerLogCleanupConfig {
    schedule: CronSchedule,
    #[serde(default=10000)] batch_size: i32,
    clear_older_than: i32,                    // hours
    #[serde(default=60)] timeout: i32,
    #[serde(default)] clean_invocation_logs: bool,
    #[serde(default)] paused: bool,
}
#[serde(untagged)] enum HeaderConf { Value{name: String, value: String}, Env{name: String, value_from_env: String} }
```

HeaderConf errors (verified, `Hasura/RQL/Types/Headers.hs` ~l.62):
- `"expecting value or value_from_env keys"`;
- `"expecting only one of value or value_from_env keys"`;
- not an object → `"expecting object for headers"`;
- a `value_from_env` starting with `HASURA_GRAPHQL_` is rejected **at parse time** (so it also breaks loading stored metadata): `env variables starting with "HASURA_GRAPHQL_" are not allowed in value_from_env: <VAR>`.
- `value` is parsed as a template string (`{{ENV}}` interpolation).
- `trigger_on_replication` (PG default `false`) is omitted on export when equal to the default; `cleanup_config.paused` is a bool.
- Webhook "exactly one of webhook / webhook_from_env" is **not** checked by the metadata parser; it is checked by `create_event_trigger` / the schema-cache build (inferred placement).

Export order: `name, definition, retry_conf, webhook, webhook_from_env, headers, request_transform, response_transform, cleanup_config, trigger_on_replication` (each optional one only when present/non-default).

`create_event_trigger` (§4.6) flattens `definition` into `insert/update/delete/enable_manual` at the top level of args.

### 2.11 Functions, native queries, logical models

`Hasura/Function/Metadata.hs`, `Hasura/Function/Cache.hs`.

```rust
struct FunctionMetadata {
    function: QualifiedFunction,
    #[serde(default)] configuration: FunctionConfig,          // export only if non-empty
    #[serde(default)] permissions: Vec<FunctionPermissionInfo>, // [{role}], export only if non-empty
    #[serde(default)] comment: Option<String>,
}
struct FunctionConfig {
    #[serde(default)] session_argument: Option<FunctionArgName>,
    #[serde(default)] exposed_as: Option<FunctionExposedAs>,  // "query" | "mutation" (default by volatility: VOLATILE->mutation)
    #[serde(default)] custom_root_fields: FunctionCustomRootFields, // {function?, function_aggregate?}; dup -> error
    #[serde(default)] custom_name: Option<Name>,
    #[serde(default)] response: Option<Value>,                // data-connector only
}
```

Function permissions are only enforced when `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS=false`. See the permissions spec.

Native queries and logical models (P1) are defined in `Hasura/NativeQuery/Metadata.hs` and `Hasura/LogicalModel/Metadata.hs`:

```rust
struct NativeQueryMetadata {
    root_field_name: NativeQueryName,
    code: InterpolatedQuery,                 // SQL text with {{arg}} placeholders
    returns: LogicalModelLocation,           // untagged: String (logical model name) | {fields: [...], select_permissions?: [...]}
    #[serde(default)] arguments: IndexMap<ArgumentName, NullableScalarType>, // {type, nullable=false, description?}
    #[serde(default)] array_relationships: Vec<RelDef<..manual..>>,
    #[serde(default)] object_relationships: Vec<RelDef<..manual..>>,
    #[serde(default)] description: Option<String>,
}
struct LogicalModelMetadata {
    name: LogicalModelName,
    fields: Vec<LogicalModelField>,          // ordered, dup names -> error
    #[serde(default)] description: Option<String>,
    #[serde(default)] select_permissions: Vec<PermDef<SelPerm>>,
}
struct LogicalModelField { name: Column, r#type: LogicalModelType, #[serde(default)] description: Option<String> }
// LogicalModelType: {scalar, nullable?} | {array: LogicalModelType, nullable?} | {logical_model, nullable?}
// Legacy field form also accepted: {name, type: "<scalar>", nullable: bool, description?}
```

### 2.12 Remote schemas

`Hasura/RemoteSchema/Metadata/Core.hs`, `Customization.hs`, `Permission.hs`.

```rust
struct RemoteSchemaMetadata {
    name: RemoteSchemaName,
    definition: RemoteSchemaDef,
    #[serde(default)] comment: Option<String>,
    #[serde(default)] permissions: Vec<RemoteSchemaPermissionMetadata>,   // [{role, definition: {schema: "<SDL>"}, comment?}]
    #[serde(default)] remote_relationships: Vec<RemoteSchemaTypeRelationships>, // [{type_name, relationships: [{name, definition}]}]
}
struct RemoteSchemaDef {
    #[serde(default)] url: Option<InputWebhook>,       // exactly one of url / url_from_env
    #[serde(default)] url_from_env: Option<String>,
    #[serde(default)] headers: Option<Vec<HeaderConf>>,
    #[serde(default)] forward_client_headers: bool,    // export only when true
    #[serde(default)] timeout_seconds: Option<i32>,    // default 60 at runtime
    #[serde(default)] customization: Option<RemoteSchemaCustomization>,
    #[serde(default)] introspection_headers: Option<Vec<HeaderConf>>,  // (newer versions)
}
struct RemoteSchemaCustomization {
    #[serde(default)] root_fields_namespace: Option<Name>,
    #[serde(default)] type_names: Option<{prefix?, suffix?, mapping: HashMap<Name,Name>}>,
    #[serde(default)] field_names: Option<Vec<{parent_type: Name, prefix?, suffix?, mapping: HashMap<Name,Name>}>>,
}
```

Remote schema relationship definitions use the same `{to_source}` / `{to_remote_schema}` forms as §2.7, with the LHS being remote-schema fields.

Export order: `name, definition{url, url_from_env, timeout_seconds, customization, headers, introspection_headers, forward_client_headers}, comment, permissions, remote_relationships` (omitting absent/empty).

### 2.13 Actions and custom types

`Hasura/RQL/Types/Action.hs`, `Hasura/RQL/Types/CustomTypes.hs`.

```rust
struct ActionMetadata {
    name: ActionName,
    definition: ActionDefinitionInput,
    #[serde(default)] comment: Option<String>,
    #[serde(default)] permissions: Vec<ActionPermissionMetadata>,   // [{role, comment?}]
}
struct ActionDefinitionInput {
    #[serde(default)] arguments: Vec<ArgumentDefinition>,  // [{name, type: "<GraphQL type string>", description?}]
    output_type: String,                                   // GraphQL type string, e.g. "LoginResponse!" or "[T]"
    // "type" + "kind" combine into ActionType:
    #[serde(default = "mutation")] r#type: String,         // "query" | "mutation"; else "expected mutation or query, but found <x>"
    #[serde(default = "synchronous")] kind: String,        // only for mutation: "synchronous" | "asynchronous"
    #[serde(default)] headers: Vec<HeaderConf>,
    #[serde(default)] forward_client_headers: bool,
    #[serde(default = default_ignored_headers)] ignored_client_headers: Vec<String>,
    handler: InputWebhook,                                 // URL template with {{ENV}}
    #[serde(default = 30)] timeout: u32,                   // seconds
    #[serde(default)] request_transform: Option<RequestTransform>,
    #[serde(default)] response_transform: Option<MetadataResponseTransform>,
}
// default_ignored_headers = ["Content-Length","Content-MD5","User-Agent","Host","Origin","Referer",
//   "Accept","Accept-Encoding","Accept-Language","Accept-Datetime","Cache-Control","Connection","DNT","Content-Type"]
```

Action export order:
- `name`, `definition`, `comment`, `permissions`.
- Inside `definition`: `handler, output_type, forward_client_headers (only if true), ignored_client_headers (always), headers (if non-empty), arguments (if non-empty), request_transform, response_transform, type, kind (only for mutation), timeout (only if != 30)` (verified, `Serialization.hs` `actionDefinitionToOrdJSON`).
- Each argument: `name, type, description (if present)`. Each permission: `role, comment (if present)`.
- `kind` is emitted as `"synchronous"` / `"asynchronous"` for mutations. A query emits `type: query` with no kind. On input, `kind` is only read when `type` is `mutation` (absent → synchronous; any other string, including `""`, is a parse error).

```rust
struct CustomTypes {
    #[serde(default)] input_objects: Vec<InputObjectTypeDefinition>, // {name, description?, fields: [{name, description?, type}]}
    #[serde(default)] objects: Vec<ObjectTypeDefinition>,           // {name, description?, fields: [{name, arguments?, description?, type}], relationships?: [...]}
    #[serde(default)] scalars: Vec<ScalarTypeDefinition>,           // {name, description?}
    #[serde(default)] enums: Vec<EnumTypeDefinition>,               // {name, description?, values: [{value, description?, is_deprecated?}]}
}
struct TypeRelationshipDefinition {
    name: RelName, r#type: RelType /* object|array */,
    #[serde(default = "default")] source: SourceName,
    remote_table: QualifiedTable,
    field_mapping: HashMap<FieldName, Column>,
}
```

Export (verified, `customTypesToOrdJSON`): `custom_types` omitted when all four lists are empty; otherwise keys `input_objects, objects, scalars, enums`, each omitted when empty; lists keep insertion order (not sorted). Per type:
- input object: `name, fields, description?`; each field `name, type, description?`;
- object: `name, fields, description?, relationships?` (relationships omitted when empty); each field `name, type, arguments?, description?`;
- scalar: `name, description?`; enum: `name, values, description?` (values use generic ToJSON, keys lexicographic).
- `ObjectTypeDefinition.fields` is NonEmpty (empty list → parse error).

#### Resolved: graphql_schema_introspection shape

- aeson (used by the server for metadata storage and the API) — verified `Hasura/RQL/Types/GraphqlSchemaIntrospection.hs`: `newtype SetGraphqlIntrospectionOptions { _idrDisabledForRoles :: HashSet RoleName }` with `genericParseJSON hasuraJSON` (`aesonPrefix snakeCase`, `unwrapUnaryRecords = False`) ⇒ **`{"disabled_for_roles": [RoleName, ...]}`**. Inside the object the key is **required** (`{}` fails to parse); the whole top-level key defaults to empty when absent.
- The same shape is the arg of `set_graphql_schema_introspection_options`.
- The autodocodec codec in `metadata.openapi.json` describes it as a bare list (`hashSetCodec` via `dimapCodec`). **Follow aeson.**
- Export omits the top-level key when the set is empty. Element order of the HashSet on export is hash order (unspecified); we may sort.
- Behaviour: no-op in OSS (see Scope section).

### 2.14 Query collections, allowlist, REST endpoints

`Hasura/RQL/Types/QueryCollection.hs`, `Hasura/RQL/Types/Allowlist.hs`, `Hasura/RQL/Types/Endpoint.hs`.

```rust
struct CreateCollection { name: CollectionName, definition: CollectionDef, #[serde(default)] comment: Option<String> }
struct CollectionDef { queries: Vec<ListedQuery> }        // dup query names -> error (inferred) 
struct ListedQuery { name: QueryName, query: GQLQueryWithText /* raw GraphQL text string */ }

struct AllowlistEntry { collection: CollectionName, #[serde(default)] scope: AllowlistScope }
// AllowlistScope JSON:
//   {"global": true}                       (default when `scope` key absent)
//   {"global": false, "roles": [r1, ...]}  (roles NonEmpty, no dups)
// `global` is REQUIRED inside a scope object (o .: "global").
// errors: "roles are not allowed when global is true"; "duplicate roles are not allowed"
// export (verified, Allowlist.hs genericToJSON hasuraJSON): ALWAYS {"collection": .., "scope": {"global": true}}
//   or {"collection": .., "scope": {"global": false, "roles": [...]}} -- scope is never omitted.
//   Entries sorted by collection.

struct CreateEndpoint {
    name: EndpointName,
    url: EndpointUrl,                         // "path/:var/segment" (no leading slash)
    methods: Vec<Method>,                     // "GET"|"POST"|"PUT"|"PATCH"|"DELETE"; NonEmpty
    definition: EndpointDef,                  // {"query": {"collection_name": .., "query_name": ..}}
    #[serde(default)] comment: Option<String>,
}
```

REST endpoint validation (inconsistencies):
- duplicate variables;
- empty segments / unnamed variables;
- ambiguous URL+method across endpoints (`AmbiguousRestEndpoints`);
- the referenced collection/query must exist and be in the allowlist (inferred).

### 2.15 Cron triggers

`Hasura/RQL/Types/ScheduledTrigger.hs`.

```rust
struct CronTriggerMetadata {
    name: TriggerName,
    webhook: InputWebhook,                       // URL template
    schedule: CronSchedule,                      // 5-field cron string
    #[serde(default)] payload: Option<Value>,
    #[serde(default)] retry_conf: STRetryConf,   // export only if != default
    #[serde(default)] headers: Vec<HeaderConf>,  // export only if non-empty
    include_in_metadata: bool,                   // REQUIRED
    #[serde(default)] comment: Option<String>,
    #[serde(default)] request_transform: Option<RequestTransform>,
    #[serde(default)] response_transform: Option<MetadataResponseTransform>,
}
struct STRetryConf {
    #[serde(default = 0)] num_retries: i32,
    #[serde(default = 10)] retry_interval_seconds: u32,
    #[serde(default = 60)] timeout_seconds: u32,
    #[serde(default = 21600)] tolerance_seconds: u32,
}
```

Export order: `name, webhook, schedule, include_in_metadata, payload, retry_conf, headers, comment, request_transform, response_transform`.

- Triggers with `include_in_metadata: false` live in metadata storage but are **excluded from `export_metadata`**, and are **preserved across `replace_metadata`** (§4.4).
- `create_cron_trigger` args = CronTriggerMetadata + `replace: bool` (default false).

### 2.16 api_limits, metrics_config, inherited_roles, network, opentelemetry, backend_configs

```rust
// Hasura/RQL/Types/ApiLimit.hs  (EE)
struct ApiLimit {
    #[serde(default)] disabled: bool,
    #[serde(default)] rate_limit: Option<Limit<RateLimitConfig>>,  // RateLimitConfig {max_reqs_per_min: i32, unique_params?: "IP" | [session_var]}
    #[serde(default)] depth_limit: Option<Limit<i32>>,
    #[serde(default)] node_limit: Option<Limit<i32>>,
    #[serde(default)] time_limit: Option<Limit<i32>>,       // seconds
    #[serde(default)] batch_limit: Option<Limit<i32>>,
}
struct Limit<T> { global: T, #[serde(default)] per_role: HashMap<RoleName, T> }

// Hasura/RQL/Types/Metadata (MetricsConfig) (EE)
struct MetricsConfig { analyze_query_variables: bool, analyze_response_body: bool }   // both REQUIRED when key present

// Hasura/RQL/Types/Roles.hs
struct InheritedRole { role_name: RoleName, role_set: Vec<RoleName> }   // export: {role_name, role_set}

// Hasura/RQL/Types/Network.hs
struct Network { #[serde(default)] tls_allowlist: Vec<TlsAllow> }
#[serde(untagged)] enum TlsAllowInput { Host(String), Obj { host: String, #[serde(default)] suffix: Option<String>, #[serde(default)] permissions: Option<Vec<TlsPermission /* "self-signed" */>> } }
// "" as string form -> 'missing "host" field in input'
// export (verified, lib/hasura-extras/src/Network/Types/Extended.hs): ALWAYS the object form with nulls kept:
//   {"host": "...", "permissions": null, "suffix": null}; network = {"tls_allowlist": [...]}

// Hasura/RQL/Types/OpenTelemetry.hs  (EE)
struct OpenTelemetryConfig {
    #[serde(default)] status: OtelStatus,            // "enabled" | "disabled" (default) | "{{ENV}}"-template
    #[serde(default)] data_types: Vec<OtelDataType>, // "traces" | "metrics" | "logs"
    #[serde(default)] exporter_otlp: OtelExporterConfig, // {otlp_traces_endpoint?, otlp_metrics_endpoint?, otlp_logs_endpoint?,
                                                         //  protocol: "http/protobuf" (only), headers: [HeaderConf],
                                                         //  resource_attributes: [{name, value}], traces_propagators: ["b3"(default)|"tracecontext"]}
    #[serde(default)] batch_span_processor: OtelBatchSpanProcessorConfig, // {max_export_batch_size: 512}
}
// export order: status, data_types, exporter_otlp, batch_span_processor

// backend_configs (data connectors only)
struct BackendConfigs { #[serde(default)] dataconnector: HashMap<DataConnectorName, {uri: String, display_name?: String}> }
```

### 2.17 Legacy metadata (v1/v2, no sources)

`MetadataNoSources` in `Hasura/RQL/Types/Metadata.hs`:

```rust
struct MetadataNoSources {
    version: MetadataVersion,                  // 1 or 2; 3 -> "unexpected version for metadata without sources: 3"
    tables: Vec<TableMetadata>,                // REQUIRED (PG tables, dup -> error)
    #[serde(default)] functions: FunctionsV1V2,
    #[serde(default)] remote_schemas, query_collections, allowlist, custom_types, actions, cron_triggers,
}
// v1: functions = [QualifiedFunction]              -> each becomes FunctionMetadata{configuration: empty, permissions: [], comment: None}
// v2: functions = [FunctionMetadata]
```

Older remote-schema/allowlist items had no extra fields. The legacy allowlist entry `{collection}` defaults to global scope.

### 2.18 CatalogState (not part of Metadata)

`Hasura/RQL/Types/Metadata/Common.hs` / `Hasura/RQL/Types/Common.hs`:

```rust
struct CatalogState { id: String /* hasura_uuid */, cli_state: Value, console_state: Value }
struct SetCatalogState { r#type: CatalogStateType /* "cli" | "console" */, state: Value }
```

### 2.19 Rust modelling recommendations

- Keep an **ordered map keyed by name** (e.g. `IndexMap<Name, T>`) for every named list. Sort on export.
- Represent every "omitted on export when default" rule explicitly in a hand-written `Serialize`, or a `skip_serializing_if` per field. Write golden tests from upstream fixtures (§Upstream tests).
- Use `serde_json::Value` passthrough for:
  - boolean expressions, request/response transforms and Kriti templates;
  - the remote field argument tree;
  - unsupported-backend source objects, which must round-trip.
- Unknown keys are ignored everywhere except `TableMetadata` (strict). Unknown keys are **not** preserved on export.
  - Match this: re-exporting drops unknown keys, which is what upstream does.
- Implement two parsing paths: `Metadata` (v3) for storage and replace_metadata v3, and `MetadataNoSources` for v1/v2 replace.
- `version` handling: if absent, upstream assumes 1. A stored metadata row without `version: 3` fails to load. Our storage writes always include `"version": 3`.

---

## 3. Storage

### 3.1 Which database

`server/src-lib/Hasura/App.hs` (`initBasicConnectionInfo`, `resolvePostgresConnInfo`):

| Flags/env | Metadata DB | `default` source |
|---|---|---|
| only `--database-url` / `HASURA_GRAPHQL_DATABASE_URL` | same DB | added at first boot |
| only `--metadata-database-url` / `HASURA_GRAPHQL_METADATA_DATABASE_URL` | metadata DB | none |
| both | metadata DB (separate) | added at first boot, pointing to database-url |
| neither | fatal: `"Either of --metadata-database-url or --database-url option expected"` | |

- The metadata DB holds `hdb_catalog.hdb_metadata`, `hdb_version`, `hdb_schema_notifications`, action logs and scheduled/cron event tables.
- Each **source** DB that has event triggers holds its own `hdb_catalog.event_log`, `event_invocation_logs`, `hdb_source_catalog_version`, `hdb_event_log_cleanups` (§3.5).
- When the metadata DB equals the default source DB, both sets of tables live in the same `hdb_catalog` schema.

### 3.2 Metadata catalog DDL (catalog version 48)

`server/src-rsr/initialise.sql` (verbatim-equivalent):

```sql
-- schema created by code: CREATE SCHEMA hdb_catalog  (Hasura/Server/Migrate.hs)
-- pgcrypto: CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA <extensions_schema default public>
--   (failure is tolerated if gen_random_uuid already exists; Migrate.hs checks)

CREATE OR REPLACE FUNCTION hdb_catalog.gen_hasura_uuid() RETURNS uuid AS
  'select gen_random_uuid()' LANGUAGE SQL;

CREATE TABLE hdb_catalog.hdb_version (
  hasura_uuid UUID PRIMARY KEY DEFAULT hdb_catalog.gen_hasura_uuid(),
  version TEXT NOT NULL,
  upgraded_on TIMESTAMPTZ NOT NULL,
  cli_state JSONB NOT NULL DEFAULT '{}'::jsonb,
  console_state JSONB NOT NULL DEFAULT '{}'::jsonb,
  ee_client_id TEXT,
  ee_client_secret TEXT
);
CREATE UNIQUE INDEX hdb_version_one_row ON hdb_catalog.hdb_version((version IS NOT NULL));

CREATE TABLE hdb_catalog.hdb_metadata (
  id INTEGER PRIMARY KEY,
  metadata JSON NOT NULL,
  resource_version INTEGER NOT NULL DEFAULT 1 UNIQUE
);

CREATE TABLE hdb_catalog.hdb_action_log (
  id UUID PRIMARY KEY DEFAULT hdb_catalog.gen_hasura_uuid(),
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

CREATE TABLE hdb_catalog.hdb_cron_events (
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  trigger_name TEXT NOT NULL,
  scheduled_time TIMESTAMPTZ NOT NULL,
  status TEXT NOT NULL DEFAULT 'scheduled',
  tries INTEGER NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  next_retry_at TIMESTAMPTZ,
  CONSTRAINT valid_status CHECK (status IN ('scheduled','locked','delivered','error','dead'))
);
CREATE INDEX hdb_cron_event_status ON hdb_catalog.hdb_cron_events (status);
CREATE UNIQUE INDEX hdb_cron_events_unique_scheduled ON
  hdb_catalog.hdb_cron_events (trigger_name, scheduled_time) WHERE status = 'scheduled';

CREATE TABLE hdb_catalog.hdb_cron_event_invocation_logs (
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  event_id TEXT,
  status INTEGER,
  request JSON,
  response JSON,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  FOREIGN KEY (event_id) REFERENCES hdb_catalog.hdb_cron_events (id)
    ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE INDEX hdb_cron_event_invocation_event_id ON hdb_catalog.hdb_cron_event_invocation_logs (event_id);

CREATE TABLE hdb_catalog.hdb_scheduled_events (
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  webhook_conf JSON NOT NULL,
  scheduled_time TIMESTAMPTZ NOT NULL,
  retry_conf JSON,
  payload JSON,
  header_conf JSON,
  status TEXT NOT NULL DEFAULT 'scheduled',
  tries INTEGER NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  next_retry_at TIMESTAMPTZ,
  comment TEXT,
  CONSTRAINT valid_status CHECK (status IN ('scheduled','locked','delivered','error','dead'))
);
CREATE INDEX hdb_scheduled_event_status ON hdb_catalog.hdb_scheduled_events (status);

CREATE TABLE hdb_catalog.hdb_scheduled_event_invocation_logs (
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  event_id TEXT,
  status INTEGER,
  request JSON,
  response JSON,
  created_at TIMESTAMPTZ DEFAULT NOW(),
  FOREIGN KEY (event_id) REFERENCES hdb_catalog.hdb_scheduled_events (id)
    ON DELETE CASCADE ON UPDATE CASCADE
);

CREATE TABLE hdb_catalog.hdb_schema_notifications (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  notification JSON NOT NULL,
  resource_version INTEGER NOT NULL DEFAULT 1,
  instance_id UUID NOT NULL,
  updated_at TIMESTAMPTZ DEFAULT NOW()
);
```

`hdb_version.version` stores the **catalog version as text**, e.g. `'48'`. `server/src-rsr/catalog_versions.txt` maps release tags to catalog versions (… `v2.51.0 48`); `catalog_version.txt` = `48`. `hdb_catalog.hdb_metadata.metadata` is `JSON` (not JSONB), so the stored text is kept byte for byte. Note upstream writes it via `PG.ViaJSON metadata` = aeson `toJSON` → `AO.fromOrdered` → plain aeson `Value`, so the **stored** row has **lexicographically sorted keys at every level** (not export order) (verified `Catalog.hs` `setMetadataInCatalog`; inferred from aeson's ordered-keymap default). Readers must not depend on stored key order; we may store either form.

Catalog migration scripts are `server/src-rsr/migrations/<n>_to_<n+1>.sql`; downgrades are in `server/src-rsr/migrations/<n>_to_<n-1>.sql`. The relevant steps:

| Step | Change |
|---|---|
| 42→43 | **Metadata separation**: the old per-object catalog tables (`hdb_table`, `hdb_relationship`, `hdb_permission`, …) are dumped into one JSON metadata row in `hdb_metadata` and dropped. Handled in Haskell code (`Hasura/Server/Migrate.hs` `from42To43` builds metadata from the old tables via `Hasura/Server/Migrate/Internal.hs`). |
| 43→44 | `resource_version` column. |
| 44→45 | `hdb_schema_notifications` (previously a `pg_notify` channel `hasura_schema_update`). |
| 45→46 | `hdb_cron_events_unique_scheduled` index. |
| 46→47 | `hdb_cron_event_invocation_event_id` index. |
| 47→48 | `ee_client_id`, `ee_client_secret`. |

**P0:** support DBs at catalog versions 43–48 by running the same SQL upgrades. Older versions (<43, i.e. v1.x) are P2. Recommend failing with a clear message telling the user to upgrade via upstream v2 first (design decision).

### 3.3 Bootstrap / version check

`Hasura/Server/Migrate.hs` (`migrateCatalog`):

1. If schema `hdb_catalog` is missing:
   - `CREATE SCHEMA hdb_catalog`;
   - ensure pgcrypto (`enablePgcryptoExtension`, `Backends/Postgres/Connection/MonadTx.hs` ~l.224): if `pgcrypto` is already in `pg_extension` do nothing; else if it is available run `CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA <extensions_schema>` (permission error `42501` → 500 `postgres-error`); else 400 `unexpected` `"pgcrypto extension is required, but could not find the extension in the PostgreSQL server. Please make sure this extension is available."`;
   - run `initialise.sql`;
   - upsert the version;
   - store the initial metadata (§3.4) with a plain `INSERT INTO hdb_catalog.hdb_metadata(id, metadata) VALUES (1, $1::json)`.
2. If `hdb_catalog` exists but `hdb_version` does not: run the same initialise step without `CREATE SCHEMA` (verified `migrateCatalog`, `initialize False`).
3. Otherwise read `SELECT version FROM hdb_catalog.hdb_version`.
   - If it equals 48: no-op.
   - If it is older: run each `from N` migration (43 and earlier ones are Haskell-driven; 44–48 are the `.sql` files) then upsert the version.
   - If it is newer (unknown): 400 `not-supported` `"Cannot use database previously used with a newer version of graphql-engine (expected a catalog version <=48, but the current version is N)."` (verified exact text, `Migrate.hs` ~l.183).
   - After migrating, metadata is read with `fetchMetadataAndResourceVersionFromCatalog` (§3.6).
4. Version upsert:
   ```sql
   INSERT INTO hdb_catalog.hdb_version (version, upgraded_on) VALUES ($1, $2)
   ON CONFLICT ((version IS NOT NULL)) DO UPDATE SET version = $1, upgraded_on = $2
   ```
5. Maintenance mode (`--enable-maintenance-mode`): never creates or migrates. Missing pieces are 500s: `"unexpected: hdb_catalog schema not found in maintenance mode"`, `"unexpected: hdb_catalog.hdb_version table not found in maintenance mode"`, `the "hdb_catalog.hdb_metadata" table is expected to exist and contain the metadata of the graphql-engine`. (It does not itself check the version number.)
6. Read-only mode: migrations are still done? Upstream skips source catalog init in read-only mode (inferred).

All of this runs inside one transaction at `SERIALIZABLE`/`REPEATABLE READ` (inferred). Two instances booting concurrently are serialized by the transaction plus the unique index.

### 3.4 Initial metadata

When the catalog is created and `--database-url` is configured, the initial metadata is:

```json
{"version":3,"sources":[{"name":"default","kind":"postgres","tables":[],
  "configuration":{"connection_info":{
     "database_url": {"from_env":"HASURA_GRAPHQL_DATABASE_URL"},   // if URL came from env; else the literal URL string
     "isolation_level":"read-committed",                            // from flags
     "pool_settings":{"max_connections":50,"idle_timeout":180,"retries":1,"connection_lifetime":600},  // from flags
     "use_prepared_statements": true}}}]}                           // from --use-prepared-statements (default true)
```

(Verified `src-exec/Main.hs` ~l.69 + `App.hs` `mkSourceConfig`: `pool_settings` = `{max_connections: --max-connections, idle_timeout: --idle-timeout (cpIdleTime), retries: --retries or 1, pool_timeout: only if --pool-timeout set, connection_lifetime: --conn-lifetime unless 0}`; `database_url` is `{"from_env":"HASURA_GRAPHQL_DATABASE_URL"}` when the URL came only from that env var, else the literal URL/template string from `--database-url`. Keys are exported lexicographically per §2.1.)

- This only happens at catalog creation, or when migrating 42→43.
- Later changes to `HASURA_GRAPHQL_DATABASE_URL` are picked up because `from_env` is stored.
- If metadata exists but has no `default` source, nothing is added.

### 3.5 Source catalog (event triggers)

`server/src-rsr/init_pg_source.sql` and `server/src-rsr/pg_source_migrations/{0_to_1,1_to_2,2_to_3,3_to_4}.sql`. Latest **source catalog version = 4** (`Hasura/Backends/Postgres/DDL/Source.hs`, `latestSourceCatalogVersion`).

- Created lazily, **only for sources with at least one event trigger** (`prepareCatalog` → `initSource`).
- Skipped in read-only mode, maintenance mode, or when `events` is not in `HASURA_GRAPHQL_ENABLED_APIS` (inferred).
- Retried 3 times with a 10 s delay (inferred).
- Contents:
  - `hdb_catalog.hdb_source_catalog_version(version TEXT NOT NULL, upgraded_on TIMESTAMPTZ NOT NULL)`, with unique index `hdb_source_catalog_version_one_row ((version IS NOT NULL))`;
  - `hdb_catalog.event_log(id TEXT DEFAULT gen_random_uuid() PK, schema_name TEXT, table_name TEXT, trigger_name TEXT, payload JSONB, delivered BOOL default false, error BOOL default false, tries INT default 0, created_at timestamp default now(), locked TIMESTAMPTZ, next_retry_at TIMESTAMP, archived BOOL default false)`, plus indexes;
  - `hdb_catalog.event_invocation_logs(id TEXT PK, trigger_name TEXT, event_id TEXT, status INT, request JSON, response JSON, created_at timestamp)`;
  - plpgsql `hdb_catalog.insert_event_log(schema_name, table_name, trigger_name, op, row_data) RETURNS text`;
  - `hdb_catalog.hdb_event_log_cleanups(...)`.
  - The exact DDL belongs to the event-trigger spec; copy `init_pg_source.sql` verbatim.

Per-trigger SQL triggers are named `notify_hasura_<trigger>_<OP>` (function in schema `hdb_catalog`). Details are in the eventing spec.

### 3.6 Reading and writing metadata

`Hasura/RQL/DDL/Schema/Catalog.hs`:

```sql
-- fetchMetadataAndResourceVersionFromCatalog
SELECT metadata, resource_version FROM hdb_catalog.hdb_metadata
-- 0 rows  -> (emptyMetadata, resource_version = 0)   [initialResourceVersion = 0, SchemaCache.hs ~l.180]
-- >1 row  -> 500 "multiple rows in hdb_metadata table"
-- else parse metadata with FromJSON Metadata (must be version 3)

-- setMetadataInCatalog (optimistic lock)
INSERT INTO hdb_catalog.hdb_metadata(id, metadata) VALUES (1, $1::json)
ON CONFLICT (id) DO UPDATE SET metadata = $1::json,
  resource_version = hdb_catalog.hdb_metadata.resource_version + 1
WHERE hdb_catalog.hdb_metadata.resource_version = $2
RETURNING resource_version
-- no row returned -> 409 code "conflict":
--   "metadata resource version referenced (<given>) did not match current version"

-- bumpMetadataVersionInCatalog (used by reload_metadata etc.)
UPDATE hdb_catalog.hdb_metadata SET resource_version = resource_version + 1
```

On the first insert (no row), `resource_version` takes the DEFAULT 1 (the `WHERE` of `DO UPDATE` only applies on conflict, so the version passed — 0 when no row existed — is irrelevant).

Note on locking: the `WHERE` in `ON CONFLICT DO UPDATE` means a stale resource_version yields zero rows, hence the 409.

### 3.7 Multi-instance schema sync

`Hasura/App.hs` (`notifySchemaCacheSyncTx`), `Hasura/Server/SchemaUpdate.hs`, `Catalog.hs`:

- After each metadata-modifying API call commits, the instance writes:
  ```sql
  INSERT INTO hdb_catalog.hdb_schema_notifications(id, notification, resource_version, instance_id)
  VALUES (1, $1::json, $2, $3::uuid)
  ON CONFLICT (id) DO UPDATE SET notification = $1::json, resource_version = $2, instance_id = $3::uuid
  ```
- `notification` is `CacheInvalidations`: `{"metadata": bool, "remote_schemas": [names], "sources": [names], "data_connectors": [names]}`. It says which caches the reload must invalidate (e.g. reload_remote_schemas).
- `instance_id` is a UUID generated per process at start.
- **Listener thread:**
  - every `HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL` ms (default 1000; 0 disables) it runs `SELECT resource_version FROM hdb_catalog.hdb_metadata` (inferred: fetches metadata version);
  - if that is greater than the local cache's version, it fetches the notification: `SELECT resource_version, notification FROM hdb_catalog.hdb_schema_notifications WHERE resource_version > $1 AND instance_id != $2::uuid`;
  - it then wakes the processor.
- **Processor thread:** reloads metadata (`fetchMetadata`), rebuilds the schema cache with the invalidations, and swaps it in. Only do this if the version is still newer.
- Upstream note: an instance never processes its own notifications (`instance_id != self`).

---

## 4. `/v1/metadata` API

`server/src-lib/Hasura/Server/API/Metadata.hs`, `Hasura/Server/API/Metadata/Instances.hs` (RQLMetadataV1 parsing), `Hasura/Server/API/Backend.hs` (per-backend command list), `Hasura/Backends/Postgres/Instances/API.hs`.

### 4.1 Envelope

```json
POST /v1/metadata
{"type": "<command>", "args": {...}, "version": 1|2 (optional, default 1), "resource_version": <int> (optional)}
```

`RQLMetadata` parsing:
- `version` 1 → `RQLMetadataV1`, parsed from `{type, args}`.
- `version` 2 → `RQLMetadataV2`, which only knows `replace_metadata` (`ReplaceMetadataV2` args) and `export_metadata` (`{}`) (generic `TaggedObject "type" "args"`, `Instances.hs`).
  - Other types under version 2 fail with a parse error (aeson tagged-sum error).
- **`args` is required** for every command (`args = o .: "args"` in `Instances.hs`; a missing key fails with aeson's `key "args" not found`), except the three nullary commands `get_cron_triggers`, `remove_api_limits`, `remove_metrics_config`, which ignore it. For the no-argument commands (`export_metadata`, `clear_metadata`, `get_inconsistent_metadata`, `drop_inconsistent_metadata`, `get_catalog_state`, …) **any value** (`{}`, `null`, …) is accepted (`parseJSON _ = pure …`). `reload_metadata` requires an object.

Authorization: **admin only** (`onlyAdmin`).
- A non-admin role (including the admin secret not provided, when one is configured, falling back to an unauthenticated role) gets HTTP 400:
  `{"path":"$.args","error":"restricted access : admin only","code":"access-denied"}`.
- Fixture: `server/tests-py/queries/v1/metadata/metadata_as_user_err.yaml`.
- The `/v1/metadata` endpoint can be disabled via `HASURA_GRAPHQL_ENABLED_APIS` (missing `metadata` → 404 `"resource does not exist"`; see the HTTP spec).

Command name dispatch (`Instances.hs`):
- If `type` is one of the backend-agnostic names → that.
- Otherwise split at the first `_`: prefix = backend (`pg`, `postgres`, `citus`, `cockroach`, `mssql`, `bigquery`, or a data-connector name), rest = command.
- Errors:
  - unknown prefix: `'unknown metadata command "<type>"; "<prefix>" was not recognized as a valid backend name'`;
  - unknown command for backend: `'unknown metadata command "<cmd>" for backend <backend>'`;
  - arg parse failure **for backend-prefixed commands only** (`commandParserWithExplicitParser`, `Server/API/Backend.hs` ~l.70): message lines joined by `\n`: `"Error when parsing command <cmd>."`, `"See our documentation at https://hasura.io/docs/latest/graphql/core/api-reference/metadata-api/index.html#metadata-apis."`, `"Internal error message: <aeson error>"`, where `<cmd>` is the suffix without the backend prefix (e.g. `track_table`). Code `parse-failed`, HTTP 400, path `$.args...`. Backend-agnostic commands report the raw aeson error.
  - unknown agnostic name without `_` match of a backend (e.g. `"foo"`): the prefix rule applies, so it gets the "was not recognized as a valid backend name" error.

### 4.2 Execution model

`runMetadataQuery` (`Hasura/Server/API/Metadata.hs`):

1. Takes a global `MVar` lock, so **metadata requests are serialized per instance**.
2. Fetches `(metadata, currentResourceVersion)` from the catalog.
3. Runs the command against an in-memory `(Metadata, SchemaCache)`. The schema cache is incrementally rebuilt (`buildSchemaCacheWithInvalidations`).
4. If the command **modifies metadata** (`queryModifiesMetadata`; true for all except the read-only ones listed below):
   - Maintenance mode → 500 `"metadata cannot be modified in maintenance mode"`.
   - Read-only mode → 400 code `not-supported` `"metadata cannot be modified in read-only mode"` (verified, `Server/API/Metadata.hs` ~l.215). Maintenance + read-only → the maintenance 500.
   - Writes the new metadata with `setMetadataInCatalog(newMetadata, resource_version_from_request OR currentResourceVersion)`. The optimistic check only bites when the client passes `resource_version`. Otherwise it is still protected against concurrent instances in between steps 2 and 4.
   - Writes the schema notification (§3.7).
   - Swaps the schema cache. Closes websocket connections if needed (subscriptions on changed objects; inferred).
5. Non-modifying commands (verified, exhaustive `queryModifiesMetadata`, `Server/API/Metadata.hs` ~l.220–262):
   - `export_metadata`, `get_inconsistent_metadata`, `get_catalog_state`, `set_catalog_state` (writes `hdb_version` directly), `dump_internal_state`;
   - `<b>_redeliver_event`, `<b>_invoke_event_trigger`, `<b>_get_event_logs`, `<b>_get_event_invocation_logs`, `<b>_get_event_by_id`;
   - `create_scheduled_event`, `delete_scheduled_event`, `get_scheduled_events`, `get_scheduled_event_invocations`, `get_cron_triggers`;
   - `list_source_kinds`, `get_source_kind_capabilities`, `<b>_get_source_tables`, `<b>_get_source_trackables`, `<b>_get_table_info`, unprefixed `get_table_info`;
   - `introspect_remote_schema`, `test_webhook_transform`, `<b>_test_connection_template`, `<b>_suggest_relationships`, `<b>_get_native_query`, `<b>_get_stored_procedure`, `<b>_get_logical_model`;
   - `bulk` / `bulk_keep_going` / `bulk_atomic` are modifying if **any** child is.
   - Everything else is modifying, including `reload_metadata` (bumps `resource_version`).
6. Metadata defaults (`HASURA_GRAPHQL_METADATA_DEFAULTS`) are merged into the in-memory metadata only for commands that neither export nor modify metadata (`Server/API/Metadata.hs` ~l.137).

Response for most mutations: `{"message":"success"}` (HTTP 200).

`resource_version` behaviour (tested in `server/tests-py/test_metadata.py`):
- `export_metadata` v2 returns `{"resource_version": N, "metadata": {...}}`.
- `replace_metadata` with `"resource_version": N` succeeds, and the stored version becomes N+1.
- Passing a stale N → 409 `conflict`.
- `reload_metadata` bumps the version by 1.

### 4.3 Backend-agnostic commands

All args below are JSON objects. Defaults in parentheses.

| Command | Args | Notes |
|---|---|---|
| `replace_metadata` | §4.4 | |
| `export_metadata` | `{}` | v1 → metadata JSON; v2 → `{resource_version, metadata}`. Excludes cron triggers with `include_in_metadata:false`. |
| `clear_metadata` | any value | Verified (`DDL/Metadata.hs` `runClearMetadata`): resets to `emptyMetadata` but keeps the `default` source's name, kind and **configuration** (customization, query_tags and health_check are reset; tables etc. cleared). Cron triggers with `include_in_metadata: false` are kept. Runs the post-drop hook for `default` (drops event-trigger SQL objects). Implemented via replace-metadata, so it returns `{"message":"success"}` plus `warnings` if any. |
| `reload_metadata` | `{reload_remote_schemas: bool\|[names] (true), reload_sources: bool\|[names] (true), recreate_event_triggers: bool\|[names] ([]/false), reload_data_connectors: bool\|[names] (true)}` | Response `{"message":"success","is_consistent":bool,"inconsistent_objects":[...]}`; the last key is only present when inconsistent. Unknown names → 400 `not-exists` e.g. `"Remote schema with name X not found in metadata"` / `"Source with name X not found in metadata"`. |
| `get_inconsistent_metadata` | `{}` | `{"is_consistent":bool,"inconsistent_objects":[...]}` |
| `drop_inconsistent_metadata` | `{}` | Purges inconsistent objects (in dependency-safe reverse order), rebuilds. If droppable inconsistencies remain: 400 `"cannot continue due to new inconsistent metadata"`. Response `{"message":"success"}`. |
| `get_catalog_state` | `{}` | `{"id": "<hasura_uuid>", "cli_state": {...}, "console_state": {...}}` |
| `set_catalog_state` | `{type: "cli"\|"console", state: json}` | `UPDATE hdb_catalog.hdb_version SET cli_state = $1` (or console_state). Response `{"message":"success"}`. |
| `dump_internal_state` | `{}` | Debug dump of the schema cache (P2, any JSON). |
| `bulk` | `[RQLMetadataV1...]` | Sequential, all-or-nothing in one metadata transaction; returns a JSON array of results. Error in item i → whole request errors with that item's error (path `$.args[i]...`). |
| `bulk_keep_going` | `[...]` | Each item runs; failures are returned inline as error objects and that item's changes are rolled back; others committed. |
| `bulk_atomic` | `[...]` | Only a subset is allowed (relationship / native query / logical model / permission ops on DB objects); others → 500 `"Bulk atomic does not support this command"`; any inconsistency → 400 `"Schema inconsistency"` with details. |
| `rename_source` | `{name, new_name}` | |
| `add_remote_schema` | `{name, definition, comment?}` | Introspects the remote; failures → 400 `remote-schema-error`. |
| `update_remote_schema` | `{name, definition, comment?}` | |
| `remove_remote_schema` | `{name}` | Fails if dependents exist (remote relationships/permissions) (inferred). |
| `reload_remote_schema` | `{name}` | |
| `introspect_remote_schema` | `{name}` | Returns the cached introspection JSON. |
| `add_remote_schema_permissions` | `{remote_schema, role, definition: {schema: SDL}, comment?}` | Requires `HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS`. |
| `drop_remote_schema_permissions` | `{remote_schema, role}` | |
| `create_remote_schema_remote_relationship` / `update_...` | `{remote_schema, type_name, name, definition}` | |
| `delete_remote_schema_remote_relationship` | `{remote_schema, type_name, name}` | |
| `create_cron_trigger` | CronTriggerMetadata + `replace` (false) | |
| `delete_cron_trigger` | `{name}` | |
| `create_scheduled_event` | `{webhook, schedule_at, payload?, headers?, retry_conf?, comment?, request_transform?, response_transform?}` | Not metadata (inserts into `hdb_scheduled_events`). |
| `delete_scheduled_event` | `{type: "one_off"\|"cron", event_id}` | |
| `get_scheduled_events`, `get_scheduled_event_invocations`, `get_cron_triggers` | see eventing spec | |
| `cleanup_event_trigger_logs`, `pause_event_trigger_cleanups`, `resume_event_trigger_cleanups` | see eventing spec | |
| `create_action` / `update_action` | `{name, definition, comment?}` | |
| `drop_action` | `{name, clear_data (true)}` | `clear_data` deletes `hdb_action_log` rows. |
| `create_action_permission` | `{action, role, definition?, comment?}` | |
| `drop_action_permission` | `{action, role}` | |
| `set_custom_types` | CustomTypes | Replaces all custom types. |
| `create_query_collection` | `{name, definition, comment?}` | |
| `rename_query_collection` | `{name, new_name}` | |
| `drop_query_collection` | `{collection, cascade}` | `cascade` required (verified, no default) — fails if in allowlist / used by REST endpoint unless cascade. |
| `add_query_to_collection` | `{collection_name, query_name, query}` | |
| `drop_query_from_collection` | `{collection_name, query_name}` | |
| `add_collection_to_allowlist` | `{collection, scope? (global)}` | Existing collection → 200 `{"message":"collection \"x\" already exists in the allowlist, scope ignored; to change scope, use update_scope_of_collection_in_allowlist"}` (metadata unchanged). |
| `drop_collection_from_allowlist` | `{collection}` | |
| `update_scope_of_collection_in_allowlist` | `{collection, scope}` | Missing → 400 `not-found` `collection "x" doesn't exist in the allowlist`. |
| `create_rest_endpoint` | CreateEndpoint | |
| `drop_rest_endpoint` | `{name}` | |
| `set_api_limits` / `remove_api_limits` | ApiLimit / `{}` | EE. |
| `set_metrics_config` / `remove_metrics_config` | MetricsConfig / `{}` | EE. |
| `add_inherited_role` | `{role_name, role_set}` | |
| `drop_inherited_role` | `{role_name}` | |
| `add_host_to_tls_allowlist` | `{host, suffix?, permissions?}` | |
| `drop_host_from_tls_allowlist` | `{host, suffix?}` | |
| `set_graphql_schema_introspection_options` | `{disabled_for_roles: [...]}` | |
| `set_query_tags` | `{source_name, disabled?, format?, omit_request_id?}` | |
| `set_opentelemetry_config` / `set_opentelemetry_status` | OpenTelemetryConfig / `"enabled"\|"disabled"` | EE. |
| `test_webhook_transform` | `{webhook_url: string\|{from_env}, body, request_transform, response_transform?, request_headers?, session_variables?, env?}` | Returns `{method, webhook_url, body, headers}`. |
| `dc_add_agent` / `dc_delete_agent` | `{name, url, display_name?, skip_check?}` / `{name}` | Data connectors; skip. |
| `list_source_kinds`, `get_source_kind_capabilities` | `{}` / `{name}` | |

### 4.4 replace_metadata in detail

`Hasura/RQL/DDL/Metadata.hs` (`runReplaceMetadata`, `runReplaceMetadataV1`, `runReplaceMetadataV2`).

**Arg parsing:**

```rust
// FromJSON ReplaceMetadata:
//   if args is an object with key "metadata": V2 form
struct ReplaceMetadataV2 {
    #[serde(default)] allow_inconsistent_metadata: bool,   // false
    #[serde(default = true)] allow_warnings: bool,         // true
    metadata: ReplaceMetadataPayload,
}
//   else args itself is ReplaceMetadataPayload (V1 form)
enum ReplaceMetadataPayload {          // dispatch on "version" (default 1)
    WithSources(Metadata),              // version 3
    NoSources(MetadataNoSources),       // version 1 or 2
}
```

Note (verified, `Hasura/RQL/DDL/Metadata.hs` `runReplaceMetadata`): the **arg shape decides the response format**, not the envelope version. V1 args (no `metadata` key) go to `runReplaceMetadataV1`, which always uses `allow_inconsistent=false` and `allow_warnings=true` and returns `{"message":"success"}`. V2 args go to `runReplaceMetadataV2` and return `{is_consistent, inconsistent_objects}`. The envelope `version: 2` (`RMV2ReplaceMetadata`) also calls `runReplaceMetadataV2` with V2 args.

**Semantics:**

1. NoSources metadata (v1/v2):
   - Requires an existing `default` source in current metadata. Otherwise 400 `"cannot import metadata without sources since no default source is defined"`.
   - The new metadata is the current default source config + the given tables/functions (into `default`), plus the given top-level objects.
   - Top-level objects not present in the v1/v2 schema are **reset**: `api_limits`, `metrics_config`, `inherited_roles`, `network`, `opentelemetry`, introspection, backend_configs.
   - **Exception:** `rest_endpoints` from the current metadata are kept.
2. Cron triggers:
   - Triggers in the *current* metadata with `include_in_metadata=false` are kept and merged in.
   - If the new metadata defines a trigger whose name clashes with one of those → 400 `already-exists`: `'cron trigger with name X already exists as a cron trigger with "included_in_metadata" as false'`.
   - For cron triggers that were added, removed or changed (compared by definition), the future scheduled `hdb_cron_events` are deleted and new ones generated. Unchanged triggers keep their events.
3. Event trigger name validation, per source (all errors are 400 code `not-supported`):
   - duplicates (the same trigger name in two tables of a source) → 400 `"Event trigger with duplicate names not allowed: <names>"`;
   - invalid characters → `"Starting in v2.50 only alphanumeric and underscore and hyphens allowed for name"` (hard error in replace; on the incremental path it may be a warning, `illegal-event-trigger-name`);
   - length > 42 → `"event trigger name can be at most 42 characters"`.
4. Put the new metadata, **invalidate all sources** (reconnect pools), and rebuild the schema cache from scratch.
5. Inconsistency handling:
   - `allow_inconsistent_metadata=false` (default) and new inconsistencies → 400, code `unexpected`, error `"cannot continue due to inconsistent metadata"`, `internal` = list of inconsistent objects. Nothing is stored.
   - `true` → store anyway.
6. Post-steps:
   - For event triggers that existed before but not now, drop their SQL triggers in the source DB.
   - Create or replace SQL triggers for all current event triggers (only if changed; inferred).
   - Update cleanup schedules.
   - For sources dropped by the replace, run the backend's post-drop hook (drops the source catalog `hdb_catalog` event tables if no longer needed) (inferred detail).
   - Failures here produce a `source-cleanup-failed` **warning**, not an error.
7. Response:
   - V1 response: `{"message":"success"}`, plus `"warnings": [...]` if any.
   - V2 response: `{"is_consistent": bool, "inconsistent_objects": [...], "warnings"?: [...]}`.
   - If `allow_warnings=false` and warnings exist → 400 code `metadata-warnings`, message `"failed due to metadata warnings"`, with the warnings in `internal` (inferred placement).

Warnings format (`Hasura/RQL/DDL/Warnings.hs`):
- `{"message": "...", "type": "<object type>", "name": "<object name>", "code": "<code>"}`.
- Codes: `source-cleanup-failed`, `illegal-event-trigger-name`, `time-limit-exceeded-system-limit`, `track-table-failed`, `untrack-table-failed`.

### 4.5 Incremental commands: common behaviour

- Each incremental command (e.g. `pg_track_table`) validates its args, updates the metadata, and rebuilds the schema cache incrementally.
- Verified in `Hasura/RQL/Types/SchemaCache/Build.hs`, `buildSchemaCacheFor objectId`, used by most single-object ops:
  - if **the object being created/modified** is itself inconsistent → 400 code `invalid-configuration`, error = the comma-separated `reason`s of that object, `internal` = its inconsistency objects;
  - else, if any **other** object became newly inconsistent → 400 code `unexpected`, `"cannot continue due to new inconsistent metadata"`, `internal` = list.
- Other paths in the same file:
  - `withNewInconsistentObjsCheck` (used around e.g. `run_sql` and some bulk paths) → **500** `unexpected` `"cannot continue due to newly found inconsistent metadata"`;
  - `tryBuildSchemaCache` (track_tables etc.) → 400 `unexpected` `"cannot continue due to newly found inconsistent metadata"`;
  - `throwOnInconsistencies` (replace without allow) → 400 `unexpected` `"cannot continue due to inconsistent metadata"`.
- Nothing is persisted on these errors. Pre-existing inconsistencies don't block.
- Dependency checks: dropping an object others depend on fails unless `cascade: true`.
  - Error: 400 `dependency-error`, `"cannot drop due to the following dependent objects: <objs>"` where `<objs>` is the sorted, comma-separated (`", "`) list of `reportSchemaObj` renderings, each with ` in source \"<s>\"` appended for source objects (verified, `reportDependentObjectsExist`, `SchemaCache.hs`).
  - Cascade removes the dependents too.
- `source` defaults to `"default"` in all `pg_*` commands.
- Responses are `{"message":"success"}` unless stated otherwise.

### 4.6 Postgres-prefixed commands (`pg_` or `postgres_`)

From `Hasura/Server/API/Backend.hs` (`sourceCommands`, `tableCommands`, `tablePermissionsCommands`, `functionCommands`, `functionPermissionsCommands`, `relationshipCommands`, `remoteRelationshipCommands`, `eventTriggerCommands`, `computedFieldCommands`, `nativeQueriesCommands`, `logicalModelsCommands`, `connectionTemplateCommands`) and `Hasura/Backends/Postgres/Instances/API.hs`.

**Sources**

| Command | Args |
|---|---|
| `pg_add_source` | `{name, configuration, replace_configuration (false), customization?, health_check?}`. If the name exists and `replace_configuration=false` → 400 `already-exists` `"source with name X already exists"`. |
| `pg_drop_source` | `{name, cascade (false)}`. Verified (`RQL/DDL/Schema/Source.hs` `runDropSource`): without cascade it fails (dependency-error) only if objects **outside** the source depend on it (e.g. remote relationships from other sources); tracked objects inside the source are dropped with it. Missing source → 400 `not-exists` `source with name "x" does not exist`. Inconsistent source + cascade → 400 `unexpected` `source with name "x" is inconsistent`; inconsistent source without cascade is just removed from metadata. Runs the post-drop hook (drop `hdb_catalog` source tables; drop event-trigger SQL). |
| `pg_update_source` | `{name, configuration?, customization?, health_check?}` |
| `pg_set_table_customization` | `{source, table, configuration: TableConfig}` |
| `pg_set_apollo_federation_config` | `{source, table, apollo_federation_config?}` |
| `pg_test_connection_template` | EE |
| `pg_get_source_tables` | `{source}` → list of tables (P1) |
| `pg_get_table_info` | `{source, table}` (P1). An unprefixed `get_table_info` also exists. |
| `pg_get_source_trackables` | `{source}` (P1, non-modifying) |

**Tables**

| Command | Args |
|---|---|
| `pg_track_table` | `{source, table, configuration?, is_enum (false), apollo_federation_config?, logical_model?}`. The v1 form `{table: <name>}` also works (it is the same shape). Already tracked → 400 `already-tracked` `"view/table already tracked: X"`. Missing table → 400 `not-exists` (or `invalid-configuration`). |
| `pg_track_tables` | `{tables: [TrackTable…], allow_warnings (true)}` → `{"tracked": [...], ...}` or warnings `track-table-failed` (P1). |
| `pg_untrack_table` | `{source, table, cascade (false)}` |
| `pg_untrack_tables` | `{tables: [...], allow_warnings}` |
| `pg_set_table_is_enum` | `{source, table, is_enum}` |

**Permissions**

| Command | Args |
|---|---|
| `pg_create_{insert,select,update,delete}_permission` | `{source, table, role, permission, comment?}`. Exists → 400 `already-exists`. Role `admin` → error (`"cannot define permission for admin role"`). |
| `pg_drop_{insert,select,update,delete}_permission` | `{source, table, role}` |
| `pg_set_permission_comment` | `{source, table, role, type: "insert"\|"select"\|"update"\|"delete", comment?}` |

**Relationships**

| Command | Args |
|---|---|
| `pg_create_object_relationship` / `pg_create_array_relationship` | `{source, table, name, using, comment?}` |
| `pg_drop_relationship` | `{source, table, relationship, cascade (false)}` |
| `pg_set_relationship_comment` | `{source, table, relationship, comment?}` |
| `pg_rename_relationship` | `{source, table, name, new_name}` |
| `pg_suggest_relationships` | `{source, tables?, omit_tracked (false)}` (P1) |
| `pg_create_remote_relationship` / `pg_update_remote_relationship` | `{source, table, name, definition}`, or legacy `{source, table, name, remote_schema, hasura_fields, remote_field}` |
| `pg_delete_remote_relationship` | `{source, table, name}` |

**Functions**

| Command | Args |
|---|---|
| `pg_track_function` | v2: `{source, function, configuration?, comment?}`; v1: `{schema, name}` or a bare string — parsed by trying v2 first, then v1 (inferred order) |
| `pg_untrack_function` | `{source, function}` |
| `pg_set_function_customization` | `{source, function, configuration}` |
| `pg_create_function_permission` / `pg_drop_function_permission` | `{source, function, role}` |

**Computed fields**

| Command | Args |
|---|---|
| `pg_add_computed_field` | `{source, table, name, definition, comment?}` |
| `pg_drop_computed_field` | `{source, table, name, cascade (false)}` |

**Event triggers**

| Command | Args |
|---|---|
| `pg_create_event_trigger` | `{source, name, table, insert?, update?, delete?, enable_manual (false), retry_conf?, webhook?, webhook_from_env?, headers?, replace (false), request_transform?, response_transform?, cleanup_config?, trigger_on_replication?}`. At least one op or enable_manual required; exactly one of webhook / webhook_from_env. |
| `pg_delete_event_trigger` | `{source, name}` |
| `pg_redeliver_event` | `{source, event_id}` |
| `pg_invoke_event_trigger` | `{source, name, payload}` → `{event_id}` |
| `pg_get_event_logs`, `pg_get_event_invocation_logs`, `pg_get_event_by_id` | eventing spec |

**Native queries / logical models (P1)**

| Command | Args |
|---|---|
| `pg_get_native_query` / `pg_track_native_query` / `pg_untrack_native_query` | `{source}` / NativeQueryMetadata + `source` / `{source, root_field_name}` |
| `pg_get_logical_model` / `pg_track_logical_model` / `pg_untrack_logical_model` | `{source}` / LogicalModelMetadata + `source` / `{source, name}` |
| `pg_create_logical_model_select_permission` / `pg_drop_logical_model_select_permission` | `{source, name, role, permission}` / `{source, name, role}` |

Unprefixed legacy names (from `/v1/query` era), such as `track_table`, `create_select_permission`, `add_existing_table_or_view`: in `/v1/metadata` these are **not** accepted without a prefix. The parser would treat `track` as a backend name → error. They live on `/v1/query` (deprecated, see the HTTP spec). Exception: `add_existing_table_or_view` (inferred: absent).

Citus and Cockroach use the same command set with prefixes `citus_` and `cockroach_`, minus unsupported ones. Out of scope.

### 4.7 `/v2/query` (needed by CLI migrations)

`Hasura/Server/API/V2Query.hs`, `Hasura/Backends/Postgres/DDL/RunSQL.hs`.

- Envelope (verified, `V2Query.hs` `RQLQuery` parser): `{"type": "insert"|"select"|"update"|"delete"|"count"|"run_sql"|"pg_run_sql"|"mssql_run_sql"|"citus_run_sql"|"cockroach_run_sql"|"bigquery_run_sql"|"bigquery_database_inspection"|"<dc>_run_sql"|"bulk"|"concurrent_bulk", "args": ...}`. `pg_run_sql` **is** accepted (same as `run_sql`). `concurrent_bulk` requires every child to be read-only. Unknown type → 400 `parse-failed` `"Unrecognised RQLQuery type: X"`. The CLI uses `run_sql` for pg (`cli/migrate/database/hasuradb/hasuradb.go` queryTypes list: `run_sql`, `mssql_run_sql`, `citus_run_sql`, `bigquery_run_sql`, `cockroach_run_sql`).
- `run_sql` args:
  ```
  {sql: String,
   source: String ("default"),
   cascade: bool (false),
   check_metadata_consistency: Option<bool>,   // default: !read_only && regex match (see below)
   read_only: bool (false),                    // run in READ ONLY tx
   no_transaction: bool (false)}               // run without BEGIN/COMMIT (e.g. CREATE INDEX CONCURRENTLY)
  ```
- Response: `{"result_type": "CommandOk", "result": null}` or `{"result_type":"TuplesOk","result":[[colnames...],[row...]...]}`. Values are rendered as text, NULL as `null`. Details are in the HTTP spec.
- When `check_metadata_consistency` is on, the server:
  - runs the SQL in a tx with event-trigger-safe handling;
  - diffs the table/function catalog;
  - auto-renames/drops metadata for renamed/dropped tables/columns;
  - **fails** if the metadata becomes inconsistent (400 `dependency-error` etc.);
  - otherwise writes the updated metadata (bumping resource_version).
  - **This is P0.** `hasura migrate apply` with default settings sends `run_sql` without `check_metadata_consistency`, so DDL detection turns it on.
  - The CLI only sets `check_metadata_consistency=false` when the CLI config flag `enableCheckMetadataConsistency` is set (`hasuradb.go` lines ~305–312; despite the name, setting it sends **false**).
- DDL detection (verified, `RunSQL.hs` `isSchemaCacheBuildRequiredRunSQL`): when `check_metadata_consistency` is absent it is `not read_only && sql =~ /\balter\b|\bdrop\b|\breplace\b|\bcreate function\b|\bcomment on\b/i`. A plain `CREATE TABLE` does **not** trigger it.
- `no_transaction: true` splits the SQL into statements and runs each separately (verified). SQL errors → 400 `postgres-error`, `"query execution failed"` with the PG error in `internal`.
- Read-only mode: write queries → 400 `"Cannot run write queries when read-only mode is enabled"`. Maintenance mode → 500.
- `bulk` on /v2/query (verified, `V2Query.hs` `runQueryM`/`runQuery`): items run **in sequence**, and the response is an array. It is **not** one DB transaction: each `run_sql` runs and commits its own transaction. The metadata changes are kept in memory and written to `hdb_metadata` once at the end, only if every item succeeded. A failing item aborts the rest and nothing is written to metadata, but SQL from earlier items stays committed.

---

## 5. Consistency and validation

### 5.1 Inconsistent object format

`server/src-lib/Hasura/RQL/Types/Metadata/Object.hs` (`InconsistentMetadata`, `MetadataObject`, `MetadataObjId`, `moiTypeName`, `moiName`):

```rust
// Each element of "inconsistent_objects": an object with "reason" + variant fields
enum InconsistentMetadata {
    InconsistentObject { reason: String /* "Inconsistent object: " + message */, r#type: String, name: String, definition: Value, message?: Value },
    ConflictingObjects { reason: String /* "Conflicting objects: " + msg */, objects: Vec<{type, name, definition}> },
    DuplicateObjects  { reason: String /* "Multiple definitions for: " + name */, r#type: String, definitions: Vec<Value> },
    DuplicateRestVariables { reason: "Duplicate variables found in endpoint path: " + ..., ... },
    InvalidRestSegments { reason: "Empty segments or unnamed variables are not allowed: " + ..., ... },
    AmbiguousRestEndpoints { reason: "Ambiguous URL paths: " + ..., conflicts: [...] },
    ConflictingInheritedPermission { reason: "Could not inherit permission for the role '<r>' for the entity: '<entity>'", name: <role>, r#type: "inherited role permission inconsistency", entity: {table, source, permission_type} | {remote_schema} },
}
```

- `type` values (`moiTypeName`): `source`, `table`, `function`, `native_query`, `stored_procedure`, `logical_model`, `function_permission`, `object_relation`, `array_relation`, `insert_permission`, `select_permission`, `update_permission`, `delete_permission`, `event_trigger`, `computed_field`, `remote_relationship`, `remote_schema`, `remote_schema_permission`, `remote_schema_remote_relationship`, `cron_trigger`, `custom_types`, `action`, `action_permission`, `inherited_role`, `rest_endpoint`, `query_collections`, `data_connector_agent`, `open_telemetry`.
- `name` formats (`moiName`):
  - `"source <s>"`;
  - `"table <qualified table> in source <s>"` (PG qualified table rendered as `schema.name` if the schema is not public, else just `name`; inferred `toTxt QualifiedTable` → `public.foo` renders as `foo`);
  - `"<type> <name> in table <t> in source <s>"`, e.g. `"object_relation author in table article in source default"`, `"select_permission user in table article in source default"`;
  - `"function <f> in source <s>"`;
  - `"remote_schema_permission <role> permission in remote schema <rs>"`;
  - `"remote_relationship <rel> on type <T> in remote schema <rs>"` (for `remote_schema_remote_relationship`);
  - `"function_permission <role> permission for function <f> in source <s>"`;
  - `"action_permission <role> permission in <action>"`;
  - action `<name>`, cron trigger `<name>`, rest endpoint `<name>`, `"inherited role <name>"`, `"custom_types"`, query collection `"<query> in <collection>"`.
  - (Verified against `Metadata/Object.hs` `moiName`. The bare-name cases are plain `toTxt` of the name.)
- `definition`: the metadata JSON of the object, **augmented** with `source` and `table` for table sub-objects. E.g. a relationship definition is `{"name": "...", "using": {...}, "comment": null, "source": "default", "table": {"schema":"public","name":"article"}}`.

Fixture (`server/tests-py/queries/v1/metadata/replace_metadata_allow_inconsistent_inconsistent.yaml`):

```yaml
inconsistent_objects:
- definition: {schema: public, name: foobar}
  reason: 'Inconsistent object: no such table/view exists in source: "foobar"'
  name: table foobar in source default
  type: table
```

### 5.2 What makes an object inconsistent (schema-cache build)

Collected while building the schema cache (`Hasura/RQL/DDL/Schema/Cache.hs`). The object is dropped from the GraphQL schema, plus its dependents (with their own inconsistency entries; dependents generally get `"Inconsistent object: ... depends on ..."` messages, inferred).

- **Source:**
  - cannot connect or resolve its config (env var missing: `"environment variable \"X\" not set"`);
  - catalog init failure.
  - Everything in the source is then missing from the schema, but **only the source itself** is listed (inferred).
- **Table:**
  - doesn't exist (`no such table/view exists in source: "<t>"`);
  - custom names collide;
  - `is_enum` requirements fail (a single text PK column + optional comment column; values must be valid GraphQL names);
  - `logical_model` not found.
- **Relationship:**
  - referenced table/columns/FK constraint missing (`"no foreign constraint exists on the given column(s)"`, inferred);
  - name clashes with a column (`"relationship name conflicts with ..."`).
- **Computed field:**
  - function missing;
  - wrong return type / argument types;
  - name conflicts with a column.
- **Permission:**
  - unknown columns or relationships in a filter/columns list;
  - wrong types in bool exp;
  - role `admin`;
  - a referenced computed field must be scalar-returning for select columns.
- **Event trigger:**
  - unknown columns;
  - webhook env var unset;
  - SQL trigger creation failure (at replace time).
- **Function:**
  - missing;
  - unsupported return type (must return SETOF a tracked table, or a table; or a scalar if functions-return-scalars is enabled... out of scope);
  - VARIADIC args;
  - `session_argument` not of type json;
  - overloaded functions are not supported (`"function X is overloaded. Overloaded functions are not supported"`).
- **Remote schema:**
  - introspection fails (HTTP error, invalid JSON, invalid introspection);
  - customization conflicts;
  - **type/field name conflicts with other remote schemas or DB schema** (reported as `ConflictingObjects`?, inferred);
  - permission SDL invalid / not a subset.
- **Remote relationship:**
  - remote field/args mismatch;
  - lhs fields missing;
  - target table/source missing.
- **Action:**
  - output type / argument types unknown;
  - handler env var missing;
  - output type a non-object for async.
- **Custom types:**
  - invalid relationships;
  - names clashing with DB types (whole `custom_types` object inconsistent).
- **Inherited role:**
  - a role in `role_set` that is itself not defined is fine; conflicting permissions → `ConflictingInheritedPermission`.
- **REST endpoint:**
  - query not found in collection;
  - collection not in allowlist? (inferred);
  - URL issues listed above.
- **Cron trigger:** invalid webhook env.
- **Duplicate GraphQL root field names** across sources/remote schemas/actions: handled at GraphQL schema build. Conflicts produce a `ConflictingObjects` entry or a hard error (inferred; see the schema spec).

### 5.3 Static (parse-time) validation recap

Parse-time failures are rejected outright, as `parse-failed` errors, never stored:
- duplicate names in lists (per-kind messages);
- strict TableMetadata keys;
- `using` with zero or both keys;
- HeaderConf with zero or both value keys;
- allowlist scope rules;
- custom root field duplicates;
- `MetadataVersion` not in 1..3;
- `type` for actions;
- `QualifiedObject` wrong JSON type;
- UrlConf object keys;
- event trigger names (on replace/create);
- cron schedule syntax (`"invalid cron expression"`, inferred).

### 5.4 Hard errors vs inconsistencies (summary)

| Situation | Behaviour |
|---|---|
| `replace_metadata`, allow_inconsistent=false, any inconsistency | 400 `unexpected` "cannot continue due to inconsistent metadata"; not stored |
| `replace_metadata`, allow_inconsistent=true | stored; response lists inconsistencies (v2) |
| incremental op causes new inconsistency | 400 `invalid-configuration` (target object) or 400 `unexpected` "cannot continue due to new inconsistent metadata" (others); some paths 500 "…newly found…" (§4.5); not stored |
| server boot with inconsistent stored metadata | serves the consistent subset; logs inconsistencies |
| `run_sql` with consistency check producing new inconsistency | 400; SQL tx rolled back |
| `reload_metadata` | always succeeds (stores bump); reports inconsistencies |

---

## 6. Migrations, seeds and the CLI

### 6.1 Directory format (config v3)

```
migrations/
└── <source_name>/                         # one dir per database (source), e.g. "default"
    ├── 1680000000000_init/
    │   ├── up.sql
    │   └── down.sql                       # optional
    └── 1680000001234_add_users_table/
        ├── up.sql
        └── down.sql
seeds/
└── <source_name>/
    └── 1680000005000_users.sql
```

- Version = the numeric prefix before the first `_` (milliseconds since epoch from `hasura migrate create`; any integer parses). Migrations sort numerically by version.
- `up.yaml` / `down.yaml` (lists of API calls) are config v1/v2 legacy. They are still accepted by the CLI (sent to `/v1/query` or `/v2/query` + `/v1/metadata` split; see `hasuradb.go` `isQueryRequest` split, which sends query-type requests to `/v2/query` as `bulk` and others to `/v1/metadata` as `bulk`).
- `-- hasura:no-transaction` on the first line of `up.sql` → `run_sql` with `no_transaction: true` (only when `--per-migration-transaction`... per docs; PG only).
- An empty `up.sql` is skipped (counted as applied).
- Config v2: `migrations/<version>_<name>/up.sql` with no source dir level. The migrations are applied to `default`.

### 6.2 How `hasura migrate apply` talks to the server

`cli/migrate/database/hasuradb/hasuradb.go`, `cli/internal/statestore/*`, `cli/internal/hasura/*`:

1. Server version / feature detection: `GET /v1/version` → `{"version":"v2.x.y"}`. Our server must return a string the CLI parses as semver ≥ v2.0 (e.g. `v2.48.0`); otherwise the CLI may assume older features. Report a real upstream-compatible version string (design decision; inferred necessity).
   - EE adds extra keys via `getExtraDataForVersionAPI`; CE adds nothing (`server/src-lib/Hasura/Server/App.hs` line ~982).
2. Source list (`--all-databases` or `--database-name`): `export_metadata` via `/v1/metadata`. The CLI reads `sources[].name` / `kind` (inferred).
3. Migration state: `POST /v1/metadata {"type":"get_catalog_state","args":{}}` → reads `cli_state`.
   - `CLIState` JSON (`cli/internal/statestore/statestore.go`):
     ```json
     {"migrations": {"<source>": {"<version>": <dirty bool>}},
      "settings": {"migration_mode": "true"},
      "isStateCopyCompleted": true}
     ```
   - A version present in the map means applied; `true` means dirty (failed mid-way).
   - `settings` holds CLI settings such as `migration_mode` (inferred key).
4. For each pending migration, in order:
   - mark dirty (`set_catalog_state` with the version → `true`) (inferred ordering: the CLI's golang-migrate pattern is SetVersion(dirty=true) → run → SetVersion(dirty=false));
   - send `POST /v2/query {"type":"run_sql","args":{"sql": <up.sql>, "source": "<db>", [no_transaction], [check_metadata_consistency:false]}}`;
   - on success, update `cli_state` (version → `false`).
   - Every `set_catalog_state` sends the **whole** `cli_state` object: `{"type":"set_catalog_state","args":{"type":"cli","state":{...full CLIState...}}}`.
5. `migrate apply --skip-execution` only updates `cli_state`. `--down`/`--goto` run `down.sql` and remove versions from the map.
6. `hasura migrate status` = local dir vs `cli_state`.
7. **Legacy state copy:**
   - Config v3 projects upgraded from v2 had state in `hdb_catalog.schema_migrations (version BIGINT PRIMARY KEY, dirty BOOLEAN NOT NULL)`, and settings in `hdb_catalog.migration_settings (setting TEXT PRIMARY KEY, value TEXT NOT NULL)` (inferred DDL from CLI v1/v2 era).
   - On first use the CLI copies these into `cli_state` (via `run_sql` SELECTs) and sets `isStateCopyCompleted: true` (`cli/internal/statestore/migrations/catalogstate.go`, "from v1.4 clients are expected to make use of the catalog API").
   - **Our server must not create these tables.** It simply returns whatever `cli_state` holds.

The CLI itself is unaffected by `hdb_catalog` internals except via `get/set_catalog_state` and `run_sql`, so implementing those correctly is sufficient.

### 6.3 Seeds

- `hasura seed create <name> [--from-table t]` writes `seeds/<db>/<ts>_<name>.sql`. `--from-table` uses `POST /v1alpha1/pg_dump {"opts": ["--column-inserts","--data-only","--table", "..."], "clean_output": true, "source": "<db>"}` (inferred args shape).
- `hasura seed apply [--file f] [--database-name d]` sends each file as `run_sql` via `/v2/query` (inferred: possibly bulk). No state is recorded.

### 6.4 `hasura migrate create --from-server`

Uses `/v1alpha1/pg_dump`:
- request: `{"opts": ["-O","-x","--schema-only","--schema","public"], "clean_output": true, "source": "default"}`;
- response: plain-text SQL.

The server runs the `pg_dump` binary (must be on PATH in the image) against the source's URL. `clean_output` strips comments, `SET` statements and `hdb_catalog` objects (`Hasura/Server/API/PGDump.hs`). P1: it is needed for `--from-server` and console "export schema".

### 6.5 Endpoint requirements per CLI command

| CLI command | Endpoints / ops required |
|---|---|
| any command | `GET /v1/version`; `GET /healthz` (some commands wait for health; inferred) |
| `hasura metadata apply` | `/v1/metadata`: `replace_metadata` (v2 args, envelope version 2 or v1; inferred), `get_inconsistent_metadata` (inferred, for reporting) |
| `hasura metadata export` | `/v1/metadata`: `export_metadata` (v1, args `{}`) |
| `hasura metadata reload / clear / ic list / ic drop / diff` | `reload_metadata`, `clear_metadata`, `get_inconsistent_metadata`, `drop_inconsistent_metadata`, `export_metadata` |
| `hasura migrate apply` | `/v1/metadata`: `export_metadata` (source list), `get_catalog_state`, `set_catalog_state`; `/v2/query`: `run_sql`, `bulk`; `/v1/metadata`: `bulk` (only for legacy `up.yaml` migrations) |
| `hasura migrate status` | `get_catalog_state`, `export_metadata` |
| `hasura migrate create --from-server` | `/v1alpha1/pg_dump` |
| `hasura seed apply / create --from-table` | `/v2/query` `run_sql`; `/v1alpha1/pg_dump` |
| `hasura deploy` | metadata apply + migrate apply + metadata reload |
| `hasura console` | The CLI serves console UI assets on :9695 (from a CDN / bundled) and a **migrate API proxy on :9693**. The console in browser talks to the server directly: `/v1/metadata` (all ops above + `get_catalog_state`/`set_catalog_state` for `console_state`), `/v2/query` (`run_sql` for schema introspection queries — the console runs **large catalog SQL queries** against `information_schema`/`pg_catalog` via `run_sql` with `read_only: true`), `/v1/graphql` (GraphiQL, introspection), `GET /v1alpha1/config` (server config), `GET /v1/version`, `GET /healthz`, `/v1alpha1/pg_dump` (export schema). Writes (e.g. creating a table) go through the CLI's :9693 API, which creates migration files and then forwards to the server. |

`GET /v1alpha1/config` response (admin only; `Hasura/Server/API/Config.hs`, hasuraJSON of `ServerConfig`):

```json
{"version": "...", "is_function_permissions_inferred": true, "is_remote_schema_permissions_enabled": false,
 "is_admin_secret_set": true, "is_auth_hook_set": false, "is_jwt_set": false, "jwt": [],
 "is_allow_list_enabled": false, "live_queries": {...}, "streaming_queries": {...},
 "console_assets_dir": null, "experimental_features": [], "is_prometheus_metrics_enabled": false,
 "default_naming_convention": "hasura-default", "is_apollo_federation_enabled": false}
```

The exact JSON of nested fields is in the HTTP/config spec.

**Implementation consequence:** for `hasura console` to work, `run_sql` must faithfully execute arbitrary read-only SQL and return `TuplesOk` text tables. The console's SQL is large but plain Postgres.

---

## 7. Implementation checklist (data model)

1. `metadata` crate:
   - serde types §2 with exact defaults;
   - a custom `Serialize` reproducing export ordering/omission (§2.1, §2.3, §2.6, §2.9, …);
   - `MetadataNoSources` → v3 upgrade;
   - golden tests against tests-py fixtures.
2. `catalog` crate:
   - DDL §3.2 embedded verbatim (copy `initialise.sql`, migrations 43–48, `init_pg_source.sql`, `pg_source_migrations/*`);
   - version check;
   - optimistic `set_metadata`;
   - notifications + poller.
3. `metadata-api`:
   - dispatch per §4.1 (prefix split, error strings);
   - global lock;
   - modify/no-modify classification;
   - incremental ops implemented as pure `Metadata -> Result<Metadata>` transforms followed by a cache rebuild + "new inconsistencies" check.
4. `project-loader`:
   - YAML + `!include` resolver;
   - actions SDL → metadata;
   - migrations runner sharing the `cli_state` format (§1.6, §6.2).

---

## 8. Open questions / uncertainties

1. (Resolved) The replace_metadata response format follows the arg shape (§4.4). The inconsistency error status codes are in §4.5.
2. Which ops use `withNewInconsistentObjsCheck` (500) and which use `buildSchemaCacheFor` (400) needs a per-op audit when implementing.
3. (Resolved) Dependency error wording (§4.5) and newer-catalog-version error (§3.3).
4. The CLI's exact ordering of `set_catalog_state` calls around each migration (dirty flag handling) was not seen in source; the golang-migrate pattern is assumed.
5. (Resolved) The CLI accepts both the YAML tag and a plain string (§1.2).
6. (Resolved, corrected) The Haskell source builds `{schema, name}`, but aeson 2 KeyMap orders keys, so the exported bytes are `{"name":…,"schema":…}` (see the NESTED KEY ORDER RULE in §2.1).
7. (Resolved) cli-ext mapping in §1.4: the SDL description is dropped (only `actions.yaml` `comment` is used), and there is no build-time `kind`/`handler` default.
8. (Resolved) `scope` is always emitted (§2.14).
9. (Resolved) No; see §4.6.
10. (Resolved) `pg_run_sql` is accepted; `bulk` is not a single transaction (§4.7).
11. The `version` the CLI puts on its `replace_metadata` envelope (1 vs 2) was not checked in the Go source; accept both.

---

## Upstream tests to reuse

Postgres metadata API fixtures (tests-py; YAML request/response pairs that can drive a Rust harness directly):

- `server/tests-py/test_metadata.py`: test classes for replace/export/reload/clear and **resource_version** (export v2 → replace with resource_version → +1; wrong version → 409; reload bumps version). Port these first.
- `server/tests-py/queries/v1/metadata/`:
  - `replace_metadata*.yaml`: v1/v2 replace incl. `replace_metadata_allow_inconsistent_inconsistent.yaml` (inconsistent object format), `replace_metadata_wo_remote_schemas`, and the no-sources variants.
  - `reload_metadata.yaml`, `clear_metadata.yaml`: reload/clear responses.
  - `export_metadata.yaml`: **stale v2-era shape**. Use only for structure; regenerate expected output with real upstream.
  - `metadata_as_user_err.yaml`: admin-only error body.
  - `pg_add_source*.yaml`, `pg_drop_source*.yaml`, `rename_source.yaml`, `pg_source_customization*.yaml`: source ops, customization validation.
  - `set_table_customization.yaml`: TableConfig round-trip.
  - `test_webhook_transform_*.yaml`: test_webhook_transform (P1).
  - Skip `*_mssql*`, `*_bigquery*`, `*_citus*`, `*_cockroach*`.
- `server/tests-py/queries/v1/metadata_order/`: export ordering. Fixtures are stale v2-format; re-derive v3 expected output, but the ordering assertions remain useful.
- `server/tests-py/queries/v1/set_table_custom_fields/`: custom root fields/column names, duplicate errors.
- `server/tests-py/queries/v1/` subdirs for `track_table`, `relationships`, `permissions`, `computed_fields`, `event_triggers`, `inconsistent_metadata`/`inconsistent_metadata_*`: incremental op errors and inconsistency messages (P0 for the op catalogue).
- `server/tests-py/queries/v1/allowlist/`, `query_collections` tests: allowlist scope errors.
- `server/tests-py/queries/v2/run_sql/` (if present) / `server/tests-py/queries/v1/run_sql/`: `run_sql` result format, `check_metadata_consistency` effects (rename/drop column).

Haskell api-tests (hspec specs; read for request/response expectations, port the PG ones):

- `server/lib/api-tests/src/Test/API/Metadata/InconsistentSpec.hs`: inconsistency reporting.
- `.../Metadata/TablesSpec.hs`, `TrackTablesSpec.hs`, `UntrackTablesSpec.hs`: track/untrack + warnings.
- `.../Metadata/WarningsSpec.hs`: warning format / `allow_warnings`.
- `.../Metadata/BulkKeepGoingSpec.hs`, `BulkAtomicSpec.hs`: bulk semantics.
- `.../Metadata/ComputedFieldsSpec.hs`: computed field errors.
- `.../Metadata/LogicalModelsSpec.hs`, `NativeQueries*` specs under `Test/Queries/NativeQueries` (P1).
- `.../Metadata/SuggestRelationshipsSpec.hs` (P1).
- `.../Metadata/TestConnectionTemplateSpec.hs` (EE; skip).
- `.../Metadata/TransparentDefaultsSpec.hs`: uses a data connector; **skip**, but conceptually covers MetadataDefaults.
- `server/lib/api-tests/src/Test/DataConnector/MetadataApiSpec.hs`: **skip** (data connectors).

Haskell unit tests:

- `server/src-test/Hasura/Backends/Postgres/TableMetadataSpec.hs` (or similar): TableMetadata JSON parsing.
- `server/src-test/Hasura/RQL/MetadataSpec.hs` (inferred name): metadata round-trip / OpenAPI codec agreement. Good for golden JSON.
- `server/src-test/Hasura/Server/MigrateSuite.hs`: catalog migrations up/down consistency (concept to replicate: migrate an empty DB from 43 → 48 and compare to fresh `initialise.sql`).

Fixtures and schema:

- `metadata.openapi.json` (repo root): the full codec-derived schema. Use it as a cross-check, remembering the `graphql_schema_introspection` discrepancy.
- `server/src-rsr/initialise.sql`, `server/src-rsr/migrations/*.sql`, `server/src-rsr/init_pg_source.sql`, `server/src-rsr/pg_source_migrations/*.sql`: copy verbatim.

Skip: everything under `server/lib/api-tests/src/Test/DataConnector`, `*BigQuery*`, `*SQLServer*`/MSSQL, Citus, Cockroach variants, and EE-only (`api_limits`, `opentelemetry`, `health_check`, `connection_template`, read replicas) unless EE parity is later in scope.
