# 02 — GraphQL schema generation (Postgres sources)

Status: draft v1. This spec says how Hasura GraphQL Engine v2 turns metadata plus the Postgres catalog into a GraphQL schema. It covers types, root fields, arguments, names, descriptions, nullability, presence rules and introspection output.

The target is **byte-for-byte compatibility of introspection output**: type names, field names, argument names and order, descriptions, default values and nullability. Clients, codegen tools such as graphql-codegen and Apollo, and committed `schema.graphql` files depend on it.

Conventions in this document:

- **Upstream paths** are relative to `server/` in the graphql-engine repo unless they start with `docs/`.
- **`(inferred)`** marks a claim that I derived from reading code but did not see confirmed by a test fixture or a direct code path.
- **SDL examples** follow introspection order, which is alphabetical; see §13. They show the role `admin` unless they say otherwise. An SDL block is a readable rendering of what introspection returns. Hasura's own SDL printer, used for Apollo `_service.sdl`, is a separate concern covered in §15.
- **`T`** stands for the table's base GraphQL identifier, before casing. **`<<T>>`** is the same identifier after the naming convention and the source type-name prefix and suffix have been applied.

---

## Scope and priority

| Priority | Area | Notes |
|---|---|---|
| **Essential (P0)** | Root types `query_root`, `mutation_root` and `subscription_root`; per-table select, by_pk, aggregate, insert, insert_one, update, update_by_pk, delete and delete_by_pk; `bool_exp`, `order_by`, `select_column`, `comparison_exp`, `insert_input`, `on_conflict`, `mutation_response`, aggregate types; object and array relationships; scalar mapping; hasura-default naming; `custom_name`, `custom_root_fields`, `column_config`; descriptions; introspection ordering and shape; `__typename`; role-dependent presence | This is what nearly every Hasura deployment uses. Get it exactly right first. |
| **Essential (P0)** | `update_many`, `_stream` subscription, aggregation predicates (`<rel>_aggregate` in bool_exp), JSONB operators, enum tables, computed fields, tracked functions (query and mutation) | They are on by default, so they appear in every introspection dump and a diff against upstream fails without them. |
| **Important (P1)** | `graphql-default` naming convention; source customization (root-field namespace, prefix and suffix, type-name prefix and suffix); non-public schema naming; Relay endpoint `/v1beta1/relay`; PostGIS and ltree operators; `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES` | Common in larger installations. |
| **Rarely used (P2)** | Experimental flags (`hide_update_many_fields`, `hide_aggregation_predicates`, `hide_stream_fields`, `group_by_aggregations`, `disable_postgres_arrays`, `apollo_federation`), `HASURA_FF_NAMING_CONVENTION_SEP_2023`, Apollo Federation, native queries and logical models, raster operators, `_cast` | Implement after P0 and P1. Keep the hooks in the design. |
| **EE-only / Cloud** | `@cached` is advertised as a directive in OSS as well (§13.5), so it is part of the OSS schema. The caching behaviour is EE. Field-level redaction (`AnnRedactionExp`) is an EE permission feature and does not change type shapes (inferred). | Implement the `@cached` *declaration* in CE. Its semantics belong elsewhere. |

The OSS/CE behaviour described below is the default. Where EE differs, the text says so.

**Fork note (verified).** The checked-out upstream source (master, commit `895e7e9`, 2026-10-12) no longer imports database `COMMENT`s as descriptions; see `src-rsr/pg_table_metadata.sql` lines 20–24 and 51–66, `src-rsr/pg_function_metadata.sql` lines 14–17 and 118–119, and `Backends/Postgres/DDL/Source.hs` `pgStripImportedDescriptions` (also strips descriptions from stored introspection). The latest released v2 tag reachable on raw.githubusercontent.com, **`v2.51.0`**, still imports them: its `pg_table_metadata.sql` joins `pg_description` and calls `col_description`, its `pg_function_metadata.sql` reads `pg_description`, and its `tests-py/queries/graphql_introspection/descriptions/automatic_comment_in_db.yaml` expects `description: What a great comment in the DB`. The Haskell that consumes the comments (`Table/API.hs` `buildDescription`, `Backends/Postgres/Schema/Select.hs`, `GraphQL/Schema/Build.hs`) is byte-identical between `v2.51.0` and master; only the SQL and the strip step differ. §12.3 covers this, and the doc recommends a compatibility switch.

---

## 1. Pipeline overview

1. **Catalog introspection.** Columns, types, primary keys, unique constraints, foreign keys, functions and enum values come from `src-rsr/pg_table_metadata.sql`, `src-rsr/pg_function_metadata.sql` and `Backends/Postgres/DDL/Table.hs`.
2. **Schema cache build** (`RQL/DDL/Schema/Cache.hs`). It resolves table info (`Table/API.hs` `buildTableInfo`), column GraphQL names, relationships, computed fields, functions and permissions.
3. **GraphQL context build** (`GraphQL/Schema.hs` `buildGQLContext`). For every role, and for the frontend and backend scenarios, it builds parsers for every source and remote schema and assembles `query_root`, `mutation_root` and `subscription_root`.
   - It does this for the default GraphQL schema and, separately, for the Relay schema (`buildRelayRoleContext`).
4. **Type collection** (`lib/schema-parsers/src/Hasura/GraphQL/Parser/Schema/Collect.hs`). It walks every reachable type depth-first, checks for conflicts, and produces the type map that introspection serves (`GraphQL/Schema/Introspect.hs` `buildIntrospectionSchema`).

The schema is computed **per role**. Two roles can see different types, fields, enum values and even different descriptions for the same type name; see §12.4 on first-wins descriptions. The admin role sees everything.

The **backend-only scenario** applies to requests with `x-hasura-use-backend-only-permissions: true` plus the admin secret. It adds mutations whose permission has `backend_only: true`. The frontend scenario hides them. The root mutation type name is `mutation_root` in both.

Schema build errors fail the whole metadata operation. They are reported as inconsistencies or as API errors; see doc 01 (metadata) for the envelope.

---

## 2. Running example

```sql
CREATE TABLE public.author (
  id          serial PRIMARY KEY,
  name        text NOT NULL,
  bio         text,
  rating      numeric,
  info        jsonb,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE public.article (
  id           serial PRIMARY KEY,
  title        text NOT NULL,
  author_id    integer NOT NULL REFERENCES public.author(id),
  is_published boolean NOT NULL DEFAULT false
);
CREATE SCHEMA myschema;
CREATE TABLE myschema.book (
  isbn  text PRIMARY KEY,
  title text NOT NULL,
  pages int4
);
```

Metadata:

- Track all three tables.
- `article.author` is an object relationship using `foreign_key_constraint_on: author_id`.
- `author.articles` is an array relationship using `foreign_key_constraint_on: {table: article, column: author_id}`.
- No custom names unless a section says otherwise.

---

## 3. Root types

| Root | Name | Description | Present when |
|---|---|---|---|
| query | `query_root` | `null` | Always. If it would have no fields, it gets the placeholder field (below). |
| mutation | `mutation_root` | `"mutation root"` | Only when at least one mutation field exists for the role. Otherwise `__schema.mutationType` is `null` and the type is absent. |
| subscription | `subscription_root` | `null` | Only when at least one subscription field exists for the role. Otherwise it is `null`. |

Source: `GraphQL/Schema.hs`. Look for `Name._query_root`, `Name._mutation_root`, `"mutation root"` and the `subscription_root` builder.

**Placeholder.** When the query root has no fields for a role, for example a role with no permissions and no remote schemas or actions, `query_root` gets exactly one field:

```graphql
"""
There are no queries available to the current role. Either there are no sources or remote schemas configured, or the current role doesn't have the required permissions.
"""
no_queries_available: String!
```

Querying it is **not** an error: it returns the placeholder text itself as a `String` (`{"data":{"no_queries_available":"There are no queries available to the current role. ..."}}`). Source: `GraphQL/Schema.hs` `queryWithIntrospectionHelper`, where the field resolves to `RFRaw (TypenameResult placeholderText)`.

**Composition order.** Root fields from all sources and actions are merged into the same root object. Duplicate root field names among them fail in `safeSelectionSet` (`lib/schema-parsers/src/Hasura/GraphQL/Parser/Internal/Parser.hs`) with one of:

- one duplicate: `Encountered conflicting definitions in the selection set for 'query_root' for field 'author' defined in [...]. Fields must not be defined more than once across all sources.` (the ` defined in <origins>` part appears only when origins are known; the name is single-quoted by `ToErrorValue G.Name`; no colon after `field`);
- several: `Encountered conflicting definitions in the selection set for 'query_root' for fields: <list>. Fields must not be defined more than once across all sources.`

Remote schemas are checked separately in `GraphQL/Schema.hs` and become metadata inconsistencies, not hard errors: `Duplicate remote field '<name>'` (clash with an earlier remote schema) and `Field cannot be overwritten by remote field '<name>'` (clash with a source field).

**Introspection fields.** `__schema` and `__type` are queryable on `query_root` but are **not** listed in `query_root.fields` (spec-compliant; see the Note in `GraphQL/Schema/Introspect.hs`). `__typename` is available on every object, union and interface type, including root types (§13.7).

Root types never get the source `type_names` prefix or suffix. A source with a `root_fields.namespace` contributes a single field whose object type is customized; see §5.4.

---

## 4. Per-table root fields

### 4.1 Field list for `public.author` (hasura-default, admin)

**query_root**:

```graphql
"""
fetch data from the table: "author"
"""
author(
  """distinct select on columns"""
  distinct_on: [author_select_column!]
  """limit the number of rows returned"""
  limit: Int
  """skip the first n rows. Use only with order_by"""
  offset: Int
  """sort the rows by one or more columns"""
  order_by: [author_order_by!]
  """filter the rows returned"""
  where: author_bool_exp
): [author!]!

"""
fetch aggregated fields from the table: "author"
"""
author_aggregate(distinct_on: [author_select_column!], limit: Int, offset: Int, order_by: [author_order_by!], where: author_bool_exp): author_aggregate!

"""fetch data from the table: "author" using primary key columns"""
author_by_pk(id: Int!): author
```

**subscription_root** has the same three fields, plus:

```graphql
"""
fetch data from the table in a streaming manner: "author"
"""
author_stream(
  """maximum number of rows returned in a single batch"""
  batch_size: Int!
  """cursor to stream the results returned by the query"""
  cursor: [author_stream_cursor_input]!
  """filter the rows returned"""
  where: author_bool_exp
): [author!]!
```

Note that the `cursor` list element is **nullable**: `[author_stream_cursor_input]!`. Verified: `GraphQL/Schema/SubscriptionStream.hs` `tableStreamCursorArg` uses `P.field cursorName cursorDesc $ P.list $ P.nullable cursorParser`, and the code comment there reads `cursor: [table_stream_cursor_input]!`. The stream root-field description is fixed: `custom_root_fields.select_stream.comment` is **not** applied to it (`Build.hs` `buildTableStreamingSubscriptionFields`).

**mutation_root**:

```graphql
"""delete data from the table: "author" """
delete_author(
  """filter the rows which have to be deleted"""
  where: author_bool_exp!
): author_mutation_response

"""delete single row from the table: "author" """
delete_author_by_pk(id: Int!): author

"""insert data into the table: "author" """
insert_author(
  """the rows to be inserted"""
  objects: [author_insert_input!]!
  """upsert condition"""
  on_conflict: author_on_conflict
): author_mutation_response

"""insert a single row into the table: "author" """
insert_author_one(
  """the row to be inserted"""
  object: author_insert_input!
  """upsert condition"""
  on_conflict: author_on_conflict
): author

"""update data of the table: "author" """
update_author(
  """append existing jsonb value of filtered columns with new jsonb value"""
  _append: author_append_input
  """delete the field or element with specified path (for JSON arrays, negative integers count from the end)"""
  _delete_at_path: author_delete_at_path_input
  """delete the array element with specified index (negative integers count from the end). throws an error if top level container is not an array"""
  _delete_elem: author_delete_elem_input
  """delete key/value pair or string element. key/value pairs are matched based on their key value"""
  _delete_key: author_delete_key_input
  """increments the numeric columns with given value of the filtered values"""
  _inc: author_inc_input
  """prepend existing jsonb value of filtered columns with new jsonb value"""
  _prepend: author_prepend_input
  """sets the columns of the filtered rows to the given values"""
  _set: author_set_input
  """filter the rows which have to be updated"""
  where: author_bool_exp!
): author_mutation_response

"""update single row of the table: "author" """
update_author_by_pk(_append: ..., _delete_at_path: ..., _delete_elem: ..., _delete_key: ..., _inc: ..., _prepend: ..., _set: ..., pk_columns: author_pk_columns_input!): author

"""update multiples rows of table: "author" """
update_author_many(
  """updates to execute, in order"""
  updates: [author_updates!]!
): [author_mutation_response]
```

These descriptions are the root field descriptions from `GraphQL/Schema/Build.hs` (select, insert and delete, for example `buildTableQueryAndSubscriptionFields`) and `GraphQL/Schema/Update/Batch.hs` (`update_*`, including `update multiples rows of table`). The `where`, `_set` and `pk_columns` argument descriptions come from `Update/Batch.hs`, `Update.hs` and `Mutation.hs`. The trailing space before `"""` in the samples above is only to make the SDL legal. The real description text ends at the closing double quote: `delete data from the table: "author"`.

The table name in descriptions uses the `<<>`/`<>>` operators, which wrap `toTxt` of the qualified table in double quotes. `public` tables print bare (`"author"`). Other schemas print `"myschema.book"`; see `Backends/Postgres/SQL/Types.hs` `instance ToTxt QualifiedTable`.

**Argument order.** Introspection returns field arguments **sorted by name**, as shown above. See §13.1.

### 4.2 Presence rules per root field

All of these rules are evaluated per role. Admin has implicit full permissions with `allow_aggregations: true`.

| Field | Present iff |
|---|---|
| `T` (select) | The role has a select permission **and** the table selection set is non-empty. Admin always has a non-empty selection set. Also requires `select` in `query_root_fields` (query root) or `subscription_root_fields` (subscription root), when those lists are set in the permission. |
| `T_by_pk` | As for select, **plus** the table has a primary key **plus** the role can select every PK column. Gated by `select_by_pk` in the root-fields lists. |
| `T_aggregate` | As for select, **plus** `allow_aggregations: true`. Gated by `select_aggregate`. |
| `T_stream` | Subscription root only. As for select, plus the role can select at least one scalar column, plus `select_stream` in `subscription_root_fields`. Hidden entirely when the `hide_stream_fields` experimental flag is set. |
| `insert_T` | The role has an insert permission, the table or view is insertable (`viIsInsertable` for views), and in the frontend scenario the permission is not `backend_only`. |
| `insert_T_one` | As for `insert_T`, **plus** the role has a select permission. It returns `T`, so it needs a selection set. |
| `update_T` | The role has an update permission, the view is updatable (`viIsUpdatable`), and the `backend_only` rule applies as for insert. **Verified:** the field exists even when the role has no updatable columns (e.g. every permitted column has a preset, or `columns: []`). It then has only the `where` argument. `GraphQL/Schema/Update/Batch.hs` `buildAnnotatedUpdateGField` has no column guard (its doc comment claiming otherwise is stale), and `Update.hs` `runUpdateOperator` just drops each operator whose column set is empty. Select permission is not required. |
| `update_T_by_pk` | As for `update_T`, **plus** the table has a PK, **plus** the role has a select permission covering all PK columns (`primaryKeysArguments`), **plus** a non-empty selection set (`tableSelectionSet`), because it returns `T`. |
| `update_T_many` | As for `update_T`. Hidden when the `hide_update_many_fields` experimental flag is set. |
| `delete_T` | The role has a delete permission and the `backend_only` rule passes. **Quirk:** for views, deletion is gated on `viIsInsertable` rather than `viIsDeletable`; see `GraphQL/Schema/Mutation.hs` `deleteFromTable` and `deleteFromTableByPk` (`guard $ isMutable viIsInsertable viewInfo`). This is upstream behaviour, so replicate it. |
| `delete_T_by_pk` | As for `delete_T`, **plus** a PK, **plus** the role can select all PK columns, **plus** a non-empty selection set. |

**Mutation return types.** `T_mutation_response.affected_rows: Int!` is always present. `returning` is **conditional**:

- `insert_T`, `update_T`, `update_T_many` and `delete_T` do **not** require a select permission.
- If the role has no select permission on the table (or an empty selection set), `T_mutation_response` has **only** `affected_rows`. The `*_by_pk` and `insert_T_one` fields are absent, because they need `tableSelectionSet`.
- Source: `GraphQL/Schema/Mutation.hs` `mutationSelectionSet` (verified). `returning` is added only if `tableSelectPermissions` and `tableSelectionList` both succeed.
- graphql-default renames `affected_rows` to `affectedRows` (`affectedRowsFieldName` is an identifier). `returning` is unchanged.

**Native queries and logical models.** A native query `nq` yields a root field `nq(args: nq_arguments!, distinct_on, limit, offset, order_by, where): [<logical_model>!]!`. Its arguments type name is `nq_arguments`; verify that in `NativeQuery/Schema.hs`. That is P2, so see §11.

### 4.3 Root field name builders

From `RQL/Types/SourceCustomization.hs`:

| Builder | Identifier | hasura-default | graphql-default |
|---|---|---|---|
| select | `T` | `author` | `author` |
| select_by_pk | `T <> [by, pk]` | `author_by_pk` | `authorByPk` |
| select_aggregate | `T <> [aggregate]` | `author_aggregate` | `authorAggregate` |
| select_stream | `T <> [stream]` | `author_stream` | `authorStream` |
| insert | `[insert] <> T` | `insert_author` | `insertAuthor` |
| insert_one | `[insert] <> T <> [one]` | `insert_author_one` | `insertAuthorOne` |
| update | `[update] <> T` | `update_author` | `updateAuthor` |
| update_by_pk | `[update] <> T <> [by, pk]` | `update_author_by_pk` | `updateAuthorByPk` |
| update_many | `[update] <> T <> [many]` | `update_author_many` | `updateAuthorMany` |
| delete | `[delete] <> T` | `delete_author` | `deleteAuthor` |
| delete_by_pk | `[delete] <> T <> [by, pk]` | `delete_author_by_pk` | `deleteAuthorByPk` |
| relay connection | `T <> [connection]` | `author_connection` | `authorConnection` |

How the name is resolved, from `Table/Cache.hs` around line 1351 (`setFieldNameCase`):

1. If `custom_root_fields.<kind>` is set, use **that name verbatim** (a CustomName, never re-cased).
2. Otherwise apply the builder to the table's base identifier: `custom_name` if set (CustomName), else the autogenerated identifier (§5.1). Then apply the naming case.
3. Finally apply the source `root_fields` prefix and suffix (§5.4) to **every** root field, including fields named by `custom_root_fields`.

---

## 5. Naming

### 5.1 Base table identifier

`getIdentifierQualifiedObject` lives in `Backends/Postgres/SQL/Types.hs`.

- **`public` schema:** `fromSnake(table)`. `author` gives `["author"]`, and `author_local` gives `["author","local"]`.
- **Any other schema:** `fromSnake(schema) ++ fromSnake(table)`. `myschema.book` gives `["myschema","book"]`, which is `myschema_book` in hasura-default and `MyschemaBook`/`myschemaBook` in graphql-default.
- All parts are **Autogenerated**.
- **`configuration.custom_name`**, if set, is used as a single **CustomName** part. Casing never changes it; see `toPascalG` below.

`fromSnake` (`lib/hasura-extras/src/Data/Text/Casing.hs`) is `T.splitOn "_"`, except that a **leading** empty segment is merged into the next part: `_hello_world` gives `["_hello","world"]`. Empty segments are **not** dropped (verified): `a__b` gives `["a","","b"]`, so `toSnakeG` gives back `a__b` and camel/pascal casing gives `aB`/`AB`. `__x` gives `["_","x"]`.

Casing functions on `GQLNameIdentifier`:

- `toSnakeG`, used for hasura-default: joins the parts with `_`, and leaves the case alone.
- `toPascalG`, used for graphql-default type names: upper-cases the first character of the **prefix only if it is Autogenerated**, and upper-cases the first character of **every suffix** whatever its origin. It concatenates without separators.
- `toCamelG`, used for graphql-default field names: lower-cases the first character of the prefix only if it is Autogenerated, and upper-cases the first character of every suffix.
- `transformNameWith` checks the result. If it is not a valid GraphQL name, the original name is used instead.

**Invalid names.** If the autogenerated name is not a valid GraphQL name, for example a table named `my-table` or `1abc`:

- A table with no `custom_name` is **silently dropped** from the schema (`takeValidTables` in `GraphQL/Schema.hs`).
- Tracking may still succeed. When the name is actually needed, the error is (`ValidationFailed`, `Backends/Postgres/SQL/Types.hs` `qualifiedObjectToName`): `cannot include "myschema.my-table" in the GraphQL schema because "myschema_my-table" is not a valid GraphQL identifier`. Both names are double-quoted (`<<>`); the first is `schema.table` (bare for `public`), the second the snake-joined name.

### 5.2 Naming conventions

`NamingCase` is either `HasuraCase` (`hasura-default`) or `GraphqlCase` (`graphql-default`). The convention is resolved in this order (`RQL/Types/SourceCustomization.hs` `getNamingCase`):

1. Source `customization.naming_convention`.
2. Otherwise `HASURA_GRAPHQL_DEFAULT_NAMING_CONVENTION` (server flag `--default-naming-convention`).
3. Otherwise `hasura-default`.

Non-Postgres sources with graphql-default fail with: `sources other than postgres do not support graphql-default as naming convention yet`.

**History.** Docs say naming conventions were experimental from v2.8 behind `HASURA_GRAPHQL_EXPERIMENTAL_FEATURES=naming_convention`, and GA from v2.34 (`docs/docs/schema/postgres/naming-convention.mdx`). In the current source the experimental flag no longer gates the feature: `tests-py/test_naming_conventions.py::TestNamingConventionWithoutExperimentalFeature` expects graphql-default output with no flag set. **Implement the GA behaviour.**

**What graphql-default changes:**

| Thing | hasura-default | graphql-default | Function |
|---|---|---|---|
| Type names built from identifiers | `toSnakeG` | `toPascalG` | `applyTypeNameCaseIdentifier` |
| Field and argument names built from identifiers | `toSnakeG` | `toCamelG` | `applyFieldNameCaseIdentifier` |
| Fixed type names given as a plain `G.Name`, e.g. `order_by`, `cursor_ordering`, `<scalar>_comparison_exp` | unchanged | `snakeToPascal` | `applyTypeNameCaseCust` |
| Fixed field names given as a plain `G.Name`, e.g. `_is_null`, `_has_keys_any`, `<rel>_aggregate` | unchanged | `snakeToCamel` | `applyFieldNameCaseCust` |
| Enum values: `order_by` values, enum-table values | unchanged | upper-case (`T.toUpper`), only if the result is a valid name | `applyEnumValueCase` |
| Column field names (autogenerated) | `created_at` | `createdAt` | `Table/API.hs` `processColumnInfo` |
| Column with `column_config.custom_name` | verbatim | verbatim | — |
| Relationship field names | verbatim | verbatim | — |
| Remote relationship and computed field names | verbatim | verbatim (inferred) | — |
| `select_column`/`update_column` enum values | column GraphQL name | column GraphQL name, so `createdAt` (not upper-cased) | `Table.hs` |
| Constraint enum values | constraint name verbatim | verbatim. With `HASURA_FF_NAMING_CONVENTION_SEP_2023`: `applyFieldNameCaseCust`, i.e. **camelCase** (`author_pkey` becomes `authorPkey`), not upper-case (verified) | `Backends/Postgres/Schema/OnConflict.hs` `conflictConstraint` |
| Scalar type names (`timestamptz`, `numeric`, `jsonb`) | unchanged | **unchanged**. With the Sep-2023 flag: Pascal (`Timestamptz`) | `Instances/Schema.hs` `columnParser` |
| Function argument fields | arg name | arg name verbatim. With the Sep-2023 flag: camelCase | `Backends/Postgres/DDL/Function.hs` |
| `<fn>_args` type | `search_authors_args` | `searchAuthorsArgs` (camelCase type name, a quirk). With the Sep-2023 flag: `SearchAuthorsArgs` | `Function/Common.hs`, `Backends/Postgres/Schema/Select.hs:354` |
| `<cf>_<T>_args` type (computed field) | `full_name_author_args` | **`full_name_author_args` (unchanged)**: `functionArgs` in `Backends/Postgres/Schema/Select.hs` uses `tCase = HasuraCase` unless the Sep-2023 flag is on. With the flag: `Full_nameAuthorArgs`, because the cf name is a single Autogenerated part and is not split on `_` (derived from `mkFunctionArgsTypeName`) | `SourceCustomization.hs:356`, `Backends/Postgres/Schema/Select.hs` `functionArgs` |

**`HASURA_FF_NAMING_CONVENTION_SEP_2023=true`** (`soNamingConventionSep2023`) extends graphql-default to the "unchanged" cases in the table above: scalar names, function argument fields, constraint values, the `<fn>_args` and `<cf>_<T>_args` types, and `<scalar>_cast_exp`. Default is off. Priority is P2. The upstream api-test `lib/api-tests/src/Test/NamingConventionsSpec.hs` runs **with this flag on**: it expects `TestSchemaFooFunctionArgs` with fields `fooArgA`/`fooArgB`, scalar `Uuid` and composite `AnInterestingTypeScalar`. Its hasura-default case shows function argument names verbatim (`FooArgB`, `foo_arg_a`).

**Fixed argument and field names in graphql-default** (verified in source): `distinct_on` becomes `distinctOn`, `order_by` (arg) becomes `orderBy`, `on_conflict` becomes `onConflict`, `update_columns` becomes `updateColumns`, `pk_columns` becomes `pkColumns`, `affected_rows` becomes `affectedRows`, `batch_size` becomes `batchSize` and `initial_value` becomes `initialValue`. The update operator fields become `_deleteKey`, `_deleteElem` and `_deleteAtPath`. Single-word names stay unchanged: `where`, `limit`, `offset`, `objects`, `object`, `updates`, `cursor`, `ordering`, `args`, `path`, `returning`, `nodes`, `aggregate`, `constraint`, `_set`, `_inc`, `_append`, `_prepend`. The aggregation-predicate function fields (`bool_and`, `covar_samp`, ...) keep their snake names, because `fnGQLName` is used raw. The array-relationship aggregate field is `applyFieldNameCaseCust (rel <> "_aggregate")`, which camel-cases the **whole** name: a relationship `author_articles` gives `authorArticlesAggregate`.

**Precedence fixture** (`tests-py/queries/naming_conventions/field_name_precedence.yaml`). In graphql-default, `custom_name: authorDetails`, `custom_root_fields.select_by_pk: author_detail`, `column_config.id.custom_name: author_id` produce:

- select field `authorDetails`;
- `__typename` `authorDetails`;
- by_pk field `author_detail`, with argument `author_id`;
- for the untouched `article_local` table in the same source: `articleLocal`, `ArticleLocal`, `articleLocalByPk(id:)`, `where: {authorId: ...}`.

**Docs vs code mismatch.** `naming-convention.mdx` claims that `custom_name: table_one` gives typename `tableOne`. The code gives `table_one`, because CustomName prefixes are never re-cased. Follow the code and the fixtures.

### 5.3 Table configuration

`Table/Cache.hs` `TableConfig` and `Table/API.hs`.

```yaml
configuration:
  custom_name: Writer            # base identifier (CustomName)
  custom_root_fields:            # each value: string OR {name: string|null, comment: string|null}
    select: writers
    select_by_pk: {name: writer, comment: "fetch a writer"}
    select_aggregate: ...
    select_stream: ...
    insert: ...
    insert_one: ...
    update: ...
    update_by_pk: ...
    update_many: ...
    delete: ...
    delete_by_pk: ...
  column_config:
    created_at: {custom_name: createdOn, comment: "row creation time"}
  custom_column_names: {bio: biography}   # legacy; merged into column_config
  comment: "Authors of articles"          # table object description
```

- **`custom_root_fields.<kind>.comment`** replaces the root field's default description. `null` or absent keeps the default. An **empty string `""`** removes the description, so introspection shows `null` (`Comment` = `Explicit Nothing`; `buildFieldDescription` in `Build.hs` and `Update/Batch.hs`). Fixture: `tests-py/queries/graphql_introspection/descriptions/root_fields.yaml`.
- **An invalid name string** in `custom_root_fields`, i.e. one that is not a GraphQL name, given in the **plain-string form**, is **silently ignored**: `G.mkName` returns Nothing and the default is used (verified, `Table/Cache.hs` `instance FromJSON CustomRootField`). In the object form `{name: "bad-name"}`, the `name` is parsed with `FromJSON G.Name`, which presumably rejects the metadata (inferred).
- **Duplicate custom root field names** inside one table fail with: `the following custom root field names are duplicated: <names>`.
- **Column custom names** must be unique within the table and must not collide with other columns, relationships or computed fields.
  - Two columns mapped to one GraphQL name (`Table/API.hs`, `AlreadyExists`): `the definitions of columns "a" and "b" are in conflict: they are mapped to the same field name, "x"`.
  - A relationship or computed field named like a column (`RQL/DDL/Schema/Cache/Fields.hs`, an inconsistency): `field definition conflicts with postgres column`.
  - A relationship or computed field named like another column's **custom** name: `field definition conflicts with custom field name for postgres column "<col>"`.
  - Relationship vs computed field vs remote relationship (a `ConflictingObjects` inconsistency): `conflicting definitions for field "<name>"` (verified; the name is double-quoted).
- **`custom_column_names`** (legacy) and `column_config` are merged with `HashMap.unionWith (<>) columnConfig legacy` (`Table/Cache.hs` `parseColumnConfig`; the comment there says "columnConfig takes precedence over legacy").
- **`comment`** (all `Comment` fields: table, column, computed field and custom root field; `RQL/Types/Common.hs` `instance FromJSON Comment`):
  - absent **or `null`**: `Automatic`, i.e. the default description `columns and relationships of "author"`. In released v2.x, Automatic first used the DB `COMMENT` (§12.3);
  - a non-empty string: that string;
  - the **empty string `""`**: `Explicit Nothing`, so **no description** (`null`). Fixture: `tests-py/queries/graphql_introspection/descriptions/explicit_no_comment_in_metadata.yaml`.
  - `Table/API.hs` `buildDescription` implements the table case.

**Changing `custom_name` while tracked** is validated by `checkConflictingNode` against the existing schema. It fails with `node <name> already exists in current graphql schema` (verified, unquoted name, code `remote-schema-conflicts`; `Table/API.hs` `checkConflictingNode`).

### 5.4 Source customization

`RQL/Types/SourceCustomization.hs`. The metadata looks like this:

```yaml
customization:
  root_fields: {namespace: my_source, prefix: fpref_, suffix: _fsuff}
  type_names:  {prefix: tpref_, suffix: _tsuff}
  naming_convention: hasura-default | graphql-default
```

**Root field prefix and suffix** apply to every root field produced by the source, including functions and native queries.

- hasura-default: plain concatenation, `prefix <> name <> suffix`, so `fpref_author_fsuff`.
- graphql-default: `prefix <> upperFirst(name) <> upperFirst(suffix)`. The prefix is used as-is. Fixture: prefix `test`, field `authorLocal`, suffix `query` gives `testAuthorLocalQuery`.

**Type name prefix and suffix** apply through `mkTypename` to the source's generated **object, input object and enum types**.

- hasura-default: `prefix <> name <> suffix`, so `tpref_author_tsuff`.
- graphql-default (verified, `SourceCustomization.hs` `applyPrefixSuffix`/`concatPrefixSuffix`): `toPascalG` over the parts present.
  - Prefix and suffix: `[prefix (Custom), name (Auto), suffix (Custom)]`, i.e. prefix verbatim, then the name with its first character upper-cased, then the suffix with its first character upper-cased.
  - Prefix only: `[prefix (Custom), name (Auto)]`.
  - Suffix only: `[name (Custom), suffix (Custom)]`. The name is **not** re-cased; only the suffix's first character is upper-cased.
  - Neither, but `type_names` present: the name is unchanged.
  - Root-field prefixes and suffixes use the same part lists, but with `transformPrefixAndSuffixAndConcat ident id upperFirstChar`: the first part is used as-is and every later part is upper-first.

**Types that do NOT get the type-name prefix or suffix.** These are built without `mkTypename`, so they are shared across sources:

- the built-in GraphQL scalars `Int`, `Float`, `String`, `Boolean` and `ID`, and the Postgres scalar names (`timestamptz`, `numeric`, `jsonb`, ...);
- `<Scalar>_comparison_exp` and `<Scalar>_array_comparison_exp`;
- the `order_by` enum (`OrderBy` in graphql-default);
- `jsonb_cast_exp`, `geography_cast_exp`, `geometry_cast_exp`;
- `st_d_within_input`, `st_d_within_geography_input`, `st_intersects_nband_geom_input`, `st_intersects_geom_nband_input`;
- the aggregation-predicate object types `T_aggregate_bool_exp` and `T_aggregate_bool_exp_<fn>`, and the `*_arguments` objects. **Quirk:** these use the table identifier without `mkTypename`. The per-function object is cased with `applyFieldNameCaseIdentifier`, so graphql-default yields camelCase type names such as `articleAggregateBoolExpCount`. The `T_select_column_..._arguments_columns` enum *does* get `mkTypename`.
- logical model types (§11).

The `cursor_ordering` enum **does** get `mkTypename`. The Relay `PageInfo` type is literally `Name._PageInfo`, so it gets no prefix or suffix (`Select.hs` line ~744). The Relay `Node` interface is also unprefixed.

**Namespace.** With `root_fields.namespace: my_source`, every root field of the source is nested under a single root field:

- On `query_root`, the field is `my_source: <NS>_query`, **nullable**, with description `null` (verified: `GraphQL/Namespace.hs` `customizeNamespace` uses `P.subselection_ namespace Nothing` over a `P.selectionSet`, which is nullable). The object type name is `mkTypename(namespace <> "_query")`. Fixture: `tpref_my_source_query_tsuff` (`tests-py/queries/v1/metadata/pg_source_namespace_query.yaml`).
- The root-field prefix and suffix apply to the fields **inside** the namespace object. The namespace field name itself is verbatim.
- On `mutation_root`: `mkTypename(namespace <> "_mutation_frontend")` or `mkTypename(namespace <> "_mutation_backend")`, depending on the scenario (`GraphQL/Schema.hs` lines 455–467 and 582–594).
- **Casing of these suffixes** (derived from code). `_query`, `_mutation_frontend`, `_mutation_backend` and `_subscription` are appended to the raw namespace name and are never re-cased. In graphql-default, the type-name customization (§5.4) treats the whole `<ns>_query` string as one Auto part. With a type-name prefix, only the first character of `<ns>_query` is upper-cased: prefix `tpref_` gives `tpref_My_source_query`. Without a prefix it stays `my_source_query`.
- On `subscription_root`: `mkTypename(namespace <> "_subscription")`.
- The namespace object's description is `null` (verified: `P.selectionSet ... Nothing`).
- An empty namespace, where a role sees no fields of that source, produces **no** namespace field.
- `__typename` on the namespace field returns the namespace object type name.

---

## 6. Generated types per table

### 6.1 Complete inventory

`<<X>>` means: build the identifier, apply `applyTypeNameCaseIdentifier`, then `mkTypename` (source prefix and suffix). `(no mkTypename)` marks a shared type that skips the prefix and suffix.

| Type | hasura-default (author) | graphql-default | Kind | Description | Present when |
|---|---|---|---|---|---|
| table object | `author` | `Author` | OBJECT | `columns and relationships of "author"`, or the config comment | role has select perm |
| bool exp | `author_bool_exp` | `AuthorBoolExp` | INPUT_OBJECT | `Boolean expression to filter rows from the table "author". All fields are combined with a logical 'AND'.` | select perm |
| order by | `author_order_by` | `AuthorOrderBy` | INPUT_OBJECT | `Ordering options when selecting data from "author".` | select perm |
| select column | `author_select_column` | `AuthorSelectColumn` | ENUM | `select columns of table "author"` | ≥1 selectable column |
| predicate select-column enum | `article_select_column_article_aggregate_bool_exp_bool_and_arguments_columns` | `ArticleSelectColumnArticleAggregateBoolExpBool_andArgumentsColumns` (see note) | ENUM | `select "<pred>" columns of table "article"` | aggregation predicates with typed args |
| update column | `author_update_column` | `AuthorUpdateColumn` | ENUM | `update columns of table "author"`, or the placeholder text (§6.7) | on_conflict built |
| constraint | `author_constraint` | `AuthorConstraint` | ENUM | `unique or primary key constraints on table "author"` | table has unique or PK constraint, and an insert field (or a relationship insert input) for it exists |
| on conflict | `author_on_conflict` | `AuthorOnConflict` | INPUT_OBJECT | `on_conflict condition type for table "author"` | as for constraint. **No update permission is needed** (verified, `Backends/Postgres/Schema/OnConflict.hs` `onConflictFieldParser`: the only guard is `tciUniqueOrPrimaryKeyConstraints`). Without update permission, `update_columns` uses the `_PLACEHOLDER` enum, which allows ON CONFLICT DO NOTHING (issue #6804). |
| insert input | `author_insert_input` | `AuthorInsertInput` | INPUT_OBJECT | `input type for inserting data into table "author"` | insert perm |
| obj rel insert | `author_obj_rel_insert_input` | `AuthorObjRelInsertInput` | INPUT_OBJECT | `input type for inserting object relation for remote table "author"` | another table's insert input has an object relationship to author |
| arr rel insert | `author_arr_rel_insert_input` | `AuthorArrRelInsertInput` | INPUT_OBJECT | `input type for inserting array relation for remote table "author"` | as above, for array relationships |
| mutation response | `author_mutation_response` | `AuthorMutationResponse` | OBJECT | `response of any mutation on the table "author"` | any mutation field exists |
| aggregate | `author_aggregate` | `AuthorAggregate` | OBJECT | `aggregated selection of "author"` | allow_aggregations |
| aggregate fields | `author_aggregate_fields` | `AuthorAggregateFields` | OBJECT | `aggregate fields of "author"` | allow_aggregations |
| op fields | `author_avg_fields` etc. | `AuthorAvgFields` | OBJECT | `aggregate avg on columns` | op has ≥1 applicable column |
| aggregate order by | `author_aggregate_order_by` | `AuthorAggregateOrderBy` | INPUT_OBJECT | `order by aggregate values of table "author"` | the table is the target of an array relationship or set-returning computed field, and the role has select permission on it. **`allow_aggregations` is not checked** (verified, `GraphQL/Schema/OrderBy.hs`). |
| op order by | `author_avg_order_by` etc. | `AuthorAvgOrderBy` | INPUT_OBJECT | `order by avg() on columns of table "author"` | as above, with applicable columns |
| set input | `author_set_input` | `AuthorSetInput` | INPUT_OBJECT | `input type for updating data in table "author"` | update perm with ≥1 settable column |
| inc input | `author_inc_input` | `AuthorIncInput` | INPUT_OBJECT | `input type for incrementing numeric columns in table "author"` | ≥1 updatable numeric column |
| append, prepend, delete_key, delete_elem, delete_at_path inputs | `author_append_input` ... | `AuthorAppendInput` ... | INPUT_OBJECT | same text as the operator field (§4.1) | ≥1 updatable jsonb column |
| pk columns input | `author_pk_columns_input` | `AuthorPkColumnsInput` | INPUT_OBJECT | `primary key columns input for table: author` (**no quotes**; for other schemas `myschema.book`) | update_by_pk present |
| updates | `author_updates` | `AuthorUpdates` | INPUT_OBJECT | `null` (inferred) | update_many present |
| stream cursor input | `author_stream_cursor_input` | `AuthorStreamCursorInput` | INPUT_OBJECT | `Streaming cursor of the table "author"`. This uses `getTableGQLName`: `custom_name` if set, else the snake-joined name (`myschema_book`). It is never naming-cased or prefixed, and it is not the SQL `schema.table` form. | stream present |
| stream cursor value | `author_stream_cursor_value_input` | `AuthorStreamCursorValueInput` | INPUT_OBJECT | `Initial value of the column from where the streaming should start` | stream present |
| aggregate bool exp (no mkTypename) | `author_aggregate_bool_exp` | `AuthorAggregateBoolExp` | INPUT_OBJECT | `null` | some array relationship targets author, aggregation predicates are enabled, and the role has allow_aggregations on author |
| per-function predicate (no mkTypename) | `author_aggregate_bool_exp_count` | `authorAggregateBoolExpCount` (camel quirk). For `bool_and`: `authorAggregateBoolExpBool_and`, because `fn` is a Custom part that is only upper-firsted. | INPUT_OBJECT | `null` | — |
| corr/covar args (no mkTypename) | `author_aggregate_bool_exp_corr_arguments` | `authorAggregateBoolExpCorrArguments` | INPUT_OBJECT | `null` | ≥1 float8 column |
| enum table type | `color_enum` | `ColorEnum` | ENUM | `null` | table is_enum and referenced |
| computed field args | `full_name_author_args` | (see §9.2) | INPUT_OBJECT | `input parameters for computed field "full_name" defined on table "author"` | computed field has user args |
| group by (experimental) | `author_group_by`, `author_group_by_key`, `author_group_by_key_fields` | Pascal | OBJECT/INPUT | see §6.9 | `group_by_aggregations` flag |
| relay connection | `authorConnection`, `authorEdge` | `AuthorConnection`, `AuthorEdge` | OBJECT | `A Relay connection object on "author"` / null | relay endpoint only |

Note on the predicate select-column enum in graphql-default. The identifier is built as `T <> [select, column] <> (T <> [aggregate,bool,exp] <> Custom(fn) <> [arguments, columns])`. `fn` is a CustomName (`bool_and`), and `toPascalG` only upper-cases the first character of each part, so the expected output is `ArticleSelectColumnArticleAggregateBoolExpBool_andArgumentsColumns` (inferred). Verify against upstream before relying on it.

The **type name for the "op" aggregate types** is `T <> op <> [fields]` (`mkTableAggOperatorTypeName`). The op order-by type is `T <> op <> [order, by]` (`mkTableAggregateOrderByOpTypeName`).

The op identifiers come from `GraphQL/Schema/Common.hs` `numericAggOperators` and `comparisonAggOperators`. They are autogenerated tuples: `sum`, `avg`, `stddev`, `[stddev,samp]`, `[stddev,pop]`, `variance`, `[var,samp]`, `[var,pop]`, `max`, `min`.

So in graphql-default:

- types: `AuthorStddevPopFields`, `AuthorVarSampOrderBy`;
- field names: `stddevPop`, `varSamp`;
- descriptions: `aggregate stddevPop on columns`, `order by stddevPop() on columns of table "author"`.

### 6.2 Table object type

```graphql
"""
columns and relationships of "author"
"""
type author {
  """An array relationship"""
  articles(distinct_on: [article_select_column!], limit: Int, offset: Int, order_by: [article_order_by!], where: article_bool_exp): [article!]!
  """An aggregate relationship"""
  articles_aggregate(distinct_on: [article_select_column!], limit: Int, offset: Int, order_by: [article_order_by!], where: article_bool_exp): article_aggregate!
  bio: String
  created_at: timestamptz!
  id: Int!
  info(
    """JSON select path"""
    path: String
  ): jsonb
  name: String!
  rating: numeric
}

type article {
  """An object relationship"""
  author: author!
  author_id: Int!
  id: Int!
  is_published: Boolean!
  title: String!
}
```

Rules:

- **Columns.** The field is `ciName`. Its type is the column scalar, non-null iff the column is NOT NULL. The description comes from `column_config.<col>.comment`, else `null` (§12.3).
- **JSON and JSONB columns** get an optional `path: String` argument, described `JSON select path`.
- **Array columns** become `[Elem!]` when the column is nullable, `[Elem!]!` when NOT NULL (inferred from `columnParser` with arrays).
- **Object relationships.** Type `<target>`. Non-null **only** when the relationship is a forward foreign-key relationship whose FK columns are all NOT NULL ("automatic forward relationship"); otherwise nullable. Fixture: `tests-py/queries/graphql_introspection/nullable_object_relationship`. Manual relationships (`manual_configuration`) and "reverse" object relationships (`foreign_key_constraint_on: {table, columns}`, insert order AfterParent) are always nullable (verified, `GraphQL/Schema/Select.hs` `relationshipField`: non-null only for `(riIsManual = False, riInsertOrder = BeforeParent)` with all FK columns NOT NULL). Relationships to native queries are always nullable. Description: `An object relationship`.
- **Array relationships:** `rel(<select args>): [<target>!]!`, described `An array relationship`.
  - Plus `rel_aggregate(<select args>): <target>_aggregate!`, described `An aggregate relationship`, only when the role has `allow_aggregations` on the target (`defaultSelectTableAggregate`: `guard $ spiAllowAgg`). The field name is `applyFieldNameCaseCust(rel <> "_aggregate")`, so `articlesAggregate` in graphql-default.
  - On the Relay endpoint, `rel_connection(...)` is added (`An array relationship connection`) when the target has a PK. The name is `rel <> "_connection"` and is never re-cased.
- **Computed fields:** see §9.
- **Remote relationships** add fields too; see doc 06 (remote schemas). They take part in sorting like any other field.
- **Relationships are omitted** when the role has no select permission on the target table.
- **Columns are omitted** when not in the role's select permission columns.

**Field ordering** is alphabetical (§13.1), *not* column position. Columns, relationships and computed fields are interleaved.

### 6.3 Boolean expression

```graphql
"""
Boolean expression to filter rows from the table "author". All fields are combined with a logical 'AND'.
"""
input author_bool_exp {
  _and: [author_bool_exp!]
  _not: author_bool_exp
  _or: [author_bool_exp!]
  articles: article_bool_exp
  articles_aggregate: article_aggregate_bool_exp
  bio: String_comparison_exp
  created_at: timestamptz_comparison_exp
  id: Int_comparison_exp
  info: jsonb_comparison_exp
  name: String_comparison_exp
  rating: numeric_comparison_exp
}
```

Source: `GraphQL/Schema/BoolExp.hs` `tableBoolExp`.

- `_and`, `_not` and `_or` have no description.
- All fields are optional with no defaults.
- **Columns** come from the role's select columns and map to the column type's comparison exp.
- **Relationships** to tables with select permission map to `<target>_bool_exp`. This applies to object and array relationships alike: an array relationship means "exists".
- **`<arrrel>_aggregate`**: see §6.4.
- **Computed fields:**
  - A scalar computed field with no user arguments, or only session or table arguments, maps to `<scalar>_comparison_exp`.
  - A set-returning one maps to `<target>_bool_exp`.
  - Computed fields that need user arguments are omitted.
- **Remote relationships** are never in bool_exp.
- **Graphql-default:** `_and`, `_not` and `_or` keep their names, and column fields use their GraphQL names.

**Null handling.** `{"col": null}` and `{"_eq": null}` raise errors by default. `HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE=true` (`soDangerousBooleanCollapse`) treats null as absent. The schema shape is unchanged. Source: the Note "[Columns in comparison expression are never nullable]".

### 6.4 Aggregation predicates (`<rel>_aggregate` in bool_exp)

Source: `GraphQL/Schema/BoolExp/AggregationPredicates.hs`, with function signatures in `Backends/Postgres/Instances/Schema.hs` `aggregationFunctions`.

Present for each **array relationship** whose target the role can select **with `allow_aggregations`**. Hidden globally by the experimental flag `hide_aggregation_predicates`. The field is omitted if no function is usable.

The functions, each a field of `<target>_aggregate_bool_exp`:

| fn | arguments | predicate type |
|---|---|---|
| `avg` | single float8 column | `float8_comparison_exp!` |
| `bool_and` | single boolean column | `Boolean_comparison_exp!` |
| `bool_or` | single boolean column | `Boolean_comparison_exp!` |
| `count` | `[<target>_select_column!]` (optional, defaults to `*`) | `Int_comparison_exp!` |
| `max`, `min`, `sum`, `stddev_samp`, `var_samp` | single float8 column | `float8_comparison_exp!` |
| `corr`, `covar_samp` | object `{X: enum!, Y: enum!}` | `float8_comparison_exp!` |

Only columns of **exactly** type `float8` (`PGDouble`) or `boolean` qualify. `numeric` and `int4` columns do not. A function is included only when a qualifying column exists.

For the example:

```graphql
input article_aggregate_bool_exp {
  bool_and: article_aggregate_bool_exp_bool_and
  bool_or: article_aggregate_bool_exp_bool_or
  count: article_aggregate_bool_exp_count
}
input article_aggregate_bool_exp_bool_and {
  arguments: article_select_column_article_aggregate_bool_exp_bool_and_arguments_columns!
  distinct: Boolean
  filter: article_bool_exp
  predicate: Boolean_comparison_exp!
}
input article_aggregate_bool_exp_count {
  arguments: [article_select_column!]
  distinct: Boolean
  filter: article_bool_exp
  predicate: Int_comparison_exp!
}
"""
select "article_aggregate_bool_exp_bool_and_arguments_columns" columns of table "article"
"""
enum article_select_column_article_aggregate_bool_exp_bool_and_arguments_columns {
  """column name"""
  is_published
}
```

- The bool-exp field name is `applyFieldNameCaseIdentifier(rel <> [aggregate])`, so `articles_aggregate`.
- None of these types has a description, except the predicate enum.
- The quoted name inside that enum's description is the **field-cased** identifier `predName'`.
- `distinct` uses `fieldOptional`, so it shows as nullable `Boolean` with **no** default in introspection.
- Parse error when more than one function is given: `exactly one predicate should be specified`.

### 6.5 Comparison expressions

Type name: `<ScalarGQLName>_comparison_exp`, described:

`Boolean expression to compare columns of type "<ScalarGQLName>". All fields are combined with logical 'AND'.`

Note the asymmetry with bool_exp: here it is "logical 'AND'", without "a". In graphql-default, the type name goes through `applyTypeNameCaseCust`: `IntComparisonExp`, `StringComparisonExp`, `TimestamptzComparisonExp`, `NumericComparisonExp`, `JsonbComparisonExp`, `Float8ComparisonExp` (inferred for the last).

Source: `Backends/Postgres/Instances/Schema.hs` `comparisonExps` (around lines 579–900), plus `equalityOperators` and `comparisonOperators` in `GraphQL/Schema/BoolExp.hs`. The generic operators (`_eq`, `_neq`, `_in`, `_nin`, `_is_null`, `_gt`, `_lt`, `_gte`, `_lte`) and `_cast` have **no description**. Backend-specific operators have the descriptions listed below, which were checked against source.

Every operator name is a `GQLNameIdentifier` cased with `applyFieldNameCaseIdentifier` (`mkBoolOperator`), e.g. `["_is","null"]` and `["_st","3d","d","within"]`.

The `_in`/`_nin` lists are `[T!]`, built with `P.list` of the non-null typed parser. All operator fields are optional, so they show as nullable.

**Operators for every column type:**

```
_eq: T
_gt: T          (not for raster)
_gte: T
_in: [T!]
_is_null: Boolean
_lt: T
_lte: T
_neq: T
_nin: [T!]
```

- `_gt`, `_gte`, `_lt` and `_lte` are present for every scalar column type except `raster`, including jsonb, json, geometry and boolean. The guard is `isScalarColumnWhere (/= PGRaster)`. `isComparable` only restricts `max`/`min` aggregates.
- **Enum-table references** get only `_eq`, `_in`, `_is_null`, `_neq` and `_nin`. The type is `<enum>_enum_comparison_exp`.

**String types** (`text`, `varchar`, `citext`, `bpchar`) add:

| op | type | description |
|---|---|---|
| `_ilike` | String | `does the column match the given case-insensitive pattern` |
| `_iregex` | String | `does the column match the given POSIX regular expression, case insensitive` |
| `_like` | String | `does the column match the given pattern` |
| `_nilike` | String | `does the column NOT match the given case-insensitive pattern` |
| `_niregex` | String | `does the column NOT match the given POSIX regular expression, case insensitive` |
| `_nlike` | String | `does the column NOT match the given pattern` |
| `_nregex` | String | `does the column NOT match the given POSIX regular expression, case sensitive` |
| `_nsimilar` | String | `does the column NOT match the given SQL regular expression` |
| `_regex` | String | `does the column match the given POSIX regular expression, case sensitive` |
| `_similar` | String | `does the column match the given SQL regular expression` |

These descriptions were checked against `Instances/Schema.hs`. The argument type is the column's own scalar (`typedParser`), so a `citext` column gets `_like: citext` and a `bpchar` column gets `_like: bpchar`. The column `String` applies only to text and varchar.

**jsonb:**

```graphql
input jsonb_comparison_exp {
  _cast: jsonb_cast_exp
  """is the column contained in the given json value"""
  _contained_in: jsonb
  """does the column contain the given json value at the top level"""
  _contains: jsonb
  _eq: jsonb
  _gt: jsonb
  _gte: jsonb
  """does the string exist as a top-level key in the column"""
  _has_key: String
  """do all of these strings exist as top-level keys in the column"""
  _has_keys_all: [String!]
  """do any of these strings exist as top-level keys in the column"""
  _has_keys_any: [String!]
  _in: [jsonb!]
  _is_null: Boolean
  """does the jsonpath return any item for the specified JSON value"""
  _jsonb_path_exists: String
  """does the jsonpath predicate check return true for the specified JSON value"""
  _jsonb_path_match: String
  _lt: jsonb
  _lte: jsonb
  _neq: jsonb
  _nin: [jsonb!]
}
input jsonb_cast_exp { String: String_comparison_exp }
```

- These descriptions were checked against source.
- `_jsonb_path_exists` and `_jsonb_path_match` exist in this tree. Older releases may not have them, which matters for diffs against older servers.
- The `_cast` field is present only when cast targets exist: jsonb → `String`, geography → `geometry`, geometry → `geography`.
- The cast-exp type name is `<scalar>_cast_exp` (no mkTypename). It is cased with HasuraCase unless the Sep-2023 flag is set (`castExp`), so it stays `jsonb_cast_exp` even in graphql-default.
- The cast-exp fields are named after the target scalar: `String`, `geometry`, `geography`.

**geography:**

- `_cast: geography_cast_exp {geometry: geometry_comparison_exp}`.
- `_st_d_within: st_d_within_geography_input`, described `is the column within a given distance from the given geography value`. The input is `{distance: Float!, from: geography!, use_spheroid: Boolean = true}`.
- `_st_intersects: geography`, described `does the column spatially intersect the given geography value`.

**geometry** (all of type `geometry` unless noted). Descriptions:

| op | description |
|---|---|
| `_st_3d_d_within` (`st_d_within_input`) | `is the column within a given 3D distance from the given geometry value` |
| `_st_3d_intersects` | `does the column spatially intersect the given geometry value in 3D` |
| `_st_contains` | `does the column contain the given geometry value` |
| `_st_crosses` | `does the column cross the given geometry value` |
| `_st_d_within` (`st_d_within_input {distance: Float!, from: geometry!}`) | `is the column within a given distance from the given geometry value` |
| `_st_equals` | `is the column equal to given geometry value (directionality is ignored)` |
| `_st_intersects` | `does the column spatially intersect the given geometry value` |
| `_st_overlaps` | `does the column 'spatially overlap' (intersect but not completely contain) the given geometry value` |
| `_st_touches` | `does the column have atleast one point in common with the given geometry value` (sic) |
| `_st_within` | `is the column contained in the given geometry value` |

Plus `_cast: geometry_cast_exp {geography: geography_comparison_exp}`.

**raster** (no descriptions):

- `_st_intersects_geom_nband: st_intersects_geom_nband_input {geommin: geometry!, nband: Int}`.
- `_st_intersects_nband_geom: st_intersects_nband_geom_input {geommin: geometry!, nband: Int!}`.
- `_st_intersects_rast: raster`.
- No `_gt`, `_gte`, `_lt` or `_lte`.

**ltree:**

| op | type | description |
|---|---|---|
| `_ancestor` | `ltree` | `is the left argument an ancestor of right (or equal)?` |
| `_ancestor_any` | `[ltree!]` | ``does array contain an ancestor of `ltree`?`` |
| `_descendant` | `ltree` | `is the left argument a descendant of right (or equal)?` |
| `_descendant_any` | `[ltree!]` | ``does array contain a descendant of `ltree`?`` |
| `_matches` | `lquery` | ``does `ltree` match `lquery`?`` |
| `_matches_any` | `[String!]` | ``does `ltree` match any `lquery` in array?`` |
| `_matches_fulltext` | `ltxtquery` | ``does `ltree` match `ltxtquery`?`` |

**Arrays** (default `UsePostgresArrays`). A column of type `text[]` has comparison type `String_array_comparison_exp`. The name is the element type name plus `_array_comparison_exp`, and the description names the element type. Fields:

- `_contains: [String!]`, described `does the array contain the given value`;
- `_contained_in: [String!]`, described `is the array contained in the given array value`;
- `_eq`, `_gt`, `_gte`, `_lt`, `_lte`, `_neq` of type `[String!]`;
- `_in` and `_nin` of type `[[String!]!]` (inferred list nesting);
- `_is_null: Boolean`.

**graphql-default operator names** come from `toCamelG` over the identifier parts:

- `_is_null` becomes `_isNull`.
- `_has_key`, `_has_keys_any` and `_has_keys_all` become `_hasKey`, `_hasKeysAny`, `_hasKeysAll`.
- `_contained_in` becomes `_containedIn`.
- `_jsonb_path_exists` becomes `_jsonbPathExists`.
- `_st_d_within` becomes `_stDWithin`, and `_st_3d_d_within` becomes `_st3dDWithin`.
- `_st_intersects_nband_geom` becomes `_stIntersectsNbandGeom`.
- `_matches_any` becomes `_matchesAny`, and `_ancestor_any` becomes `_ancestorAny`.
- Single-part names (`_eq`, `_like`, `_contains`, ...) are unchanged.
- The first part keeps its leading underscore, and a lower-casing of `_` is a no-op.

### 6.6 Order by

```graphql
"""Ordering options when selecting data from "author"."""
input author_order_by {
  articles_aggregate: article_aggregate_order_by
  bio: order_by
  created_at: order_by
  id: order_by
  info: order_by
  name: order_by
  rating: order_by
}
"""Ordering options when selecting data from "article"."""
input article_order_by {
  author: author_order_by
  author_id: order_by
  id: order_by
  is_published: order_by
  title: order_by
}
"""column ordering options"""
enum order_by {
  """in ascending order, nulls last"""
  asc
  """in ascending order, nulls first"""
  asc_nulls_first
  """in ascending order, nulls last"""
  asc_nulls_last
  """in descending order, nulls first"""
  desc
  """in descending order, nulls first"""
  desc_nulls_first
  """in descending order, nulls last"""
  desc_nulls_last
}
```

Source: `GraphQL/Schema/OrderBy.hs`, and `Instances/Schema.hs` `orderByOperators`.

- **Fields:**
  - every selectable scalar column, of type `order_by`, including JSON, jsonb and geometry columns (verified: `OrderBy.hs` `mkField` handles every `SCIScalarColumn` without a type filter);
  - object relationships: `<target>_order_by`, when the role has select permission on the target;
  - array relationships: `<rel>_aggregate: <target>_aggregate_order_by` whenever the role has select permission on the target. **`allow_aggregations` is NOT required** (verified: the `ArrRel` branch of `OrderBy.hs` `mkField` checks only `tableSelectPermissions`). Plain array relationships are not orderable;
  - scalar computed fields with no user args: `order_by`;
  - set-returning computed fields with no user args: `<cf>_aggregate: <target>_aggregate_order_by` (verified);
  - remote relationships and relationships to native queries are never orderable;
  - all order-by fields have description `null`. The `<rel>_aggregate` name is `applyFieldNameCaseIdentifier` over `(rel, [aggregate])`. In graphql-default this lower-cases the first character of the relationship name, unlike `applyFieldNameCaseCust`.
- **graphql-default:**
  - the enum type is `OrderBy` (via `applyTypeNameCaseCust`, no mkTypename), with values `ASC`, `ASC_NULLS_FIRST`, `ASC_NULLS_LAST`, `DESC`, `DESC_NULLS_FIRST`, `DESC_NULLS_LAST`;
  - array relationships become `articlesAggregate`.
- **`T_aggregate_order_by`:**
  - `count: order_by`;
  - for each op in avg, max, min, stddev, stddev_pop, stddev_samp, sum, var_pop, var_samp, variance with ≥1 applicable column: `<op>: T_<op>_order_by`;
  - each `T_<op>_order_by` has one `order_by` field per applicable column.

### 6.7 Column enums

```graphql
"""select columns of table "author" """
enum author_select_column {
  """column name"""
  bio
  """column name"""
  created_at
  """column name"""
  id
  """column name"""
  info
  """column name"""
  name
  """column name"""
  rating
}
"""update columns of table "author" """
enum author_update_column { ...same shape, columns the role may update... }
```

Source: `GraphQL/Schema/Table.hs`.

- Values are `ciName`. They are **not** upper-cased in graphql-default, so you get `createdAt`.
- Every value has the description `column name`.
- `update_column`: the columns in the role's update permission, **minus columns that have a preset in the update permission**, minus immutable columns (generated or identity-always (inferred)).
- **When the role has insert but no update permission**, or no updatable columns, the update-column enum still exists because `on_conflict` needs it. It contains a single placeholder value:

  ```graphql
  """placeholder for update columns of table "author" (current role has no relevant permissions)"""
  enum author_update_column {
    """placeholder (do not use)"""
    _PLACEHOLDER
  }
  ```

  **Verified:** `on_conflict` is offered in this case. `Backends/Postgres/Schema/OnConflict.hs` `onConflictFieldParser` gates only on the table having unique or PK constraints, and `updateColumnsPlaceholderParser` (`GraphQL/Schema/Table.hs`) supplies the placeholder enum. This enables "insert or ignore" via `update_columns: []` (issue #6804). Passing `_PLACEHOLDER` fails at parse time with `erroneous column name`. The placeholder description quotes the table with `<<>`: `placeholder for update columns of table "author" (current role has no relevant permissions)`.

### 6.8 Insert types

```graphql
"""input type for inserting data into table "author" """
input author_insert_input {
  articles: article_arr_rel_insert_input
  bio: String
  created_at: timestamptz
  id: Int
  info: jsonb
  name: String
  rating: numeric
}
"""input type for inserting array relation for remote table "article" """
input article_arr_rel_insert_input {
  data: [article_insert_input!]!
  """upsert condition"""
  on_conflict: article_on_conflict
}
"""input type for inserting object relation for remote table "author" """
input author_obj_rel_insert_input {
  data: author_insert_input!
  """upsert condition"""
  on_conflict: author_on_conflict
}
"""on_conflict condition type for table "author" """
input author_on_conflict {
  constraint: author_constraint!
  update_columns: [author_update_column!]! = []
  where: author_bool_exp
}
"""unique or primary key constraints on table "author" """
enum author_constraint {
  """unique or primary key constraint on columns "id" """
  author_pkey
}
"""response of any mutation on the table "author" """
type author_mutation_response {
  """number of rows affected by the mutation"""
  affected_rows: Int!
  """data from the rows affected by the mutation"""
  returning: [author!]!
}
```

Source: `GraphQL/Schema/Mutation.hs` and `Backends/Postgres/Schema/OnConflict.hs`.

- **Insert input fields** are the role's insert-permission columns, **minus preset columns**, minus non-insertable (generated) columns.
- All insert input fields are **nullable**, whatever the column's NOT NULL constraint. Missing required values become a Postgres error at runtime.
- Column descriptions come from `column_config` comments.
- **Relationship fields** are present when the role has an insert permission on the remote table:
  - object relationship: `<target>_obj_rel_insert_input`;
  - array relationship: `<target>_arr_rel_insert_input`.
  - Inserting through manual relationships is allowed (inferred).
- **`on_conflict`** inside relation inputs is present only when the target has an on_conflict type.
- **The `constraint` enum** lists the primary key plus **every unique index** (verified: `pg_table_metadata.sql` selects `pg_index` rows with `indisunique AND NOT indisprimary`, named by the index relation's `relname`). That includes partial and expression unique indexes; an expression index has an empty column list. Each value is the constraint or index name verbatim. A name that is not a valid GraphQL name fails the schema build (`conflictConstraint` uses `textToName`). The value description is:

  `unique or primary key constraint on columns <list>`

  where `<list>` is the column names, each double-quoted and joined with `, `. **The column order is HashSet iteration order**, so for multi-column constraints it is not deterministic in principle. In practice, sort to match upstream on a sample, or fall back to definition order (open question).
- **`update_columns`** uses `fieldWithDefault` with default `[]`, so introspection shows `defaultValue: "[]"`.

### 6.9 Aggregate types

```graphql
"""aggregated selection of "author" """
type author_aggregate {
  aggregate: author_aggregate_fields
  nodes: [author!]!
}
"""aggregate fields of "author" """
type author_aggregate_fields {
  avg: author_avg_fields
  count(columns: [author_select_column!], distinct: Boolean): Int!
  max: author_max_fields
  min: author_min_fields
  stddev: author_stddev_fields
  stddev_pop: author_stddev_pop_fields
  stddev_samp: author_stddev_samp_fields
  sum: author_sum_fields
  var_pop: author_var_pop_fields
  var_samp: author_var_samp_fields
  variance: author_variance_fields
}
"""aggregate avg on columns"""
type author_avg_fields {
  id: Float
  rating: Float
}
"""aggregate max on columns"""
type author_max_fields {
  bio: String
  created_at: timestamptz
  id: Int
  name: String
  rating: numeric
}
"""aggregate sum on columns"""
type author_sum_fields {
  id: Int
  rating: numeric
}
```

Source: `GraphQL/Schema/Select.hs` `tableAggregationFields`, and `Common.hs` `numericAggOperators`.

- **Numeric ops** (`avg`, `stddev`, `stddev_pop`, `stddev_samp`, `var_pop`, `var_samp`, `variance`) cover columns where `isNumType` is true: int2, int4, int8, float4, float8, numeric and money. The field type is **`Float`** (nullable).
- **`sum`** covers numeric columns and **keeps the column's scalar type** (nullable).
- **`max`/`min`** cover comparable columns, i.e. every type except json, jsonb, geometry, geography, boolean, unknown types and enum references. The field type is the column type (nullable).
- **Scalar computed fields** without user args also take part, with the same type rules (inferred; see `tableAggregationFields`, which includes computed fields).
- The op description is `aggregate <op> on columns`, where `<op>` is the **field-cased** op name. In graphql-default, `stddev_pop` gives `aggregate stddevPop on columns` (`Select.hs` `parseAggOperator`).
- An op object is omitted entirely if it has no applicable columns.
- `count` has arguments `columns: [T_select_column!]` and `distinct: Boolean`. Both are optional, with no descriptions and no defaults (inferred). It returns `Int!`.
- `aggregate` is nullable, and `nodes` is `[T!]!`.
- **Experimental `group_by_aggregations`:**
  - adds `group_by(keys: [T_group_by_key!]!): [T_group_by!]!` to `T_aggregate`;
  - descriptions: `Groups the table by the specified keys` and `The keys on which to group by`;
  - types `T_group_by`, `T_group_by_key` (`A column grouping key` (inferred placement)) and `T_group_by_key_fields`.
  - P2: copy the shape from `Select.hs` `defaultTableGroupBy...` when needed.

### 6.10 Update types

```graphql
"""input type for updating data in table "author" """
input author_set_input { bio: String, created_at: timestamptz, id: Int, info: jsonb, name: String, rating: numeric }
"""input type for incrementing numeric columns in table "author" """
input author_inc_input { id: Int, rating: numeric }
"""append existing jsonb value of filtered columns with new jsonb value"""
input author_append_input { info: jsonb }
"""prepend existing jsonb value of filtered columns with new jsonb value"""
input author_prepend_input { info: jsonb }
"""delete key/value pair or string element. key/value pairs are matched based on their key value"""
input author_delete_key_input { info: String }
"""delete the array element with specified index (negative integers count from the end). throws an error if top level container is not an array"""
input author_delete_elem_input { info: Int }
"""delete the field or element with specified path (for JSON arrays, negative integers count from the end)"""
input author_delete_at_path_input { info: [String!] }
"""primary key columns input for table: author"""
input author_pk_columns_input { id: Int! }
input author_updates {
  """append existing jsonb value of filtered columns with new jsonb value"""
  _append: author_append_input
  ...same operator fields as update_author...
  """filter the rows which have to be updated"""
  where: author_bool_exp!
}
```

Source: `GraphQL/Schema/Update.hs` `updateOperator` and `Update/Batch.hs`.

- **Every operator-object field** uses `P.fieldOptional`, so in introspection each field is **nullable**, with the column's description.
- **Value types** follow from the parsers (`Backends/Postgres/Instances/Schema.hs`): `_delete_key` uses a nullable text parser (`String`), and `_delete_elem` uses a non-null `Int!` parser. Both show as `String` and `Int` in introspection because the fields are optional. `_delete_at_path` takes `[String!]`. `_set`, `_inc`, `_append` and `_prepend` use the column's own scalar.
- **Operator presence:**
  - `_set`: ≥1 updatable column;
  - `_inc`: ≥1 updatable numeric column (`isNumType`);
  - the JSONB ops: ≥1 updatable `jsonb` column. `json` columns do not count.
- **Updatable columns** are the columns in the update permission, minus the update preset columns, minus immutable columns.
- **Parse error** when the same column is used in several operators, or in an operator and an update preset: `Column found in multiple operators: <cols>.` (verified, `Update.hs` `mergeDisjoint`; note the trailing period).
- **`T_pk_columns_input`** has one non-null field per PK column, using column GraphQL names. Its description uses `toTxt` of the table without quoting, so it reads `primary key columns input for table: myschema.book` for non-public tables.

### 6.11 Streaming types

```graphql
"""Streaming cursor of the table "author" """
input author_stream_cursor_input {
  """Stream column input with initial value"""
  initial_value: author_stream_cursor_value_input!
  """cursor ordering"""
  ordering: cursor_ordering
}
"""Initial value of the column from where the streaming should start"""
input author_stream_cursor_value_input {
  bio: String
  created_at: timestamptz
  id: Int
  info: jsonb
  name: String
  rating: numeric
}
"""ordering argument of a cursor"""
enum cursor_ordering {
  """ascending ordering of the cursor"""
  ASC
  """descending ordering of the cursor"""
  DESC
}
```

Source: `GraphQL/Schema/SubscriptionStream.hs`.

- The value-input fields are all selectable scalar columns, nullable, with column descriptions.
- **Parse errors:** `one streaming column field is expected`, `multiple column cursors are not supported yet`.
- **`cursor_ordering`** goes through `mkTypename` and `applyTypeNameCaseCust`, so it becomes `CursorOrdering` in graphql-default. Its values stay `ASC` and `DESC`.

---

## 7. Postgres → GraphQL scalar mapping

Source: `Backends/Postgres/SQL/Types.hs` (`PGScalarType`, `pgScalarTranslations`, `mkScalarTypeName`), and `Backends/Postgres/Instances/Schema.hs` `columnParser`.

| Postgres type (catalog name, aliases) | GraphQL type name | Input parser | Output encoding |
|---|---|---|---|
| `int4`, `integer`, `int`, `serial` | `Int` | 32-bit int | number |
| `int2`, `smallint`, `smallserial` | `smallint` | int | number |
| `int8`, `bigint`, `bigserial` | `bigint` | int, or string (inferred) | number; string when stringified |
| `float4`, `real` | `Float` | float or int | number |
| `float8`, `double precision` | `float8` | float, int, or string (inferred) | number; string when stringified |
| `numeric`, `decimal` | `numeric` | float, int, or string | number; string when stringified |
| `money` | `money` | — | string when stringified |
| `bool`, `boolean` | `Boolean` | bool | bool |
| `text` | `String` | string | string |
| `varchar`, `character varying` | `String` | string | string |
| `bpchar`, `char`, `character` | `bpchar` | string | string |
| `citext` | `citext` | string | string |
| `date` | `date` | string | string |
| `time`, `time without time zone` | `time` (unknown-type passthrough) | string | string |
| `timetz` | `timetz` | string | string |
| `timestamp` | `timestamp` | string | string |
| `timestamptz` | `timestamptz` | string | string |
| `interval` | `interval` (unknown passthrough) | string | string |
| `json` | `json` | any JSON | JSON |
| `jsonb` | `jsonb` | any JSON | JSON |
| `uuid` | `uuid` | string | string |
| `geometry` / `geography` | `geometry` / `geography` | GeoJSON object | GeoJSON |
| `raster` | `raster` | string | string |
| `ltree`, `lquery`, `ltxtquery` | same name | string | string |
| any other base type (`inet`, `bytea`, `tsvector`, `int4range`, `oid`, `name`, …) | the catalog `typname` verbatim | string or JSON (inferred) | whatever Postgres casts to JSON |
| Postgres **enum** type (`typtype = 'e'`) | the enum's type name, as a **scalar** (not an ENUM) | string | string |
| **domain** | the base type's mapping (catalog `coalesce(base_type.typname, type.typname)`, `src-rsr/pg_table_metadata.sql`) | — | — |
| **composite** column (`PGCompositeScalar`) | `<typname>_scalar` (verified: `mkScalarTypeName` appends `_scalar` to avoid clashing with a tracked table's object type; api-test `NamingConventionsSpec` shows `AnInterestingTypeScalar` under graphql-default + Sep-2023) | — | — |
| array `T[]` | `[Elem!]`, with element mapping as above | list | list |

**Scalar descriptions.** Custom scalars (e.g. `timestamptz`) have description `null`. The built-ins (`Int`, `String`, ...) have `null` too (inferred). Upstream does not emit the spec text.

**Invalid type names.** A type name that is not a valid GraphQL name, e.g. `"my-type"` or `"char"` with quotes, fails with `ValidationFailed`: `cannot use SQL type "my-type" in the GraphQL schema because its name is not a valid GraphQL identifier` (the type name is double-quoted). Inferred: this aborts the table's schema.

**`max`/`min` and unknown types.** `isComparableType` is false for json, jsonb, geometry, geography, boolean and every `PGUnknown` type. So `time`, `interval`, `inet` and other passthrough types get no `max`/`min` aggregate fields, even though they have `_gt` etc. in comparison expressions.

**Experimental `disable_postgres_arrays`.** Array columns map to a scalar named after the array type's `typname`, with the leading underscore: `text[]` becomes scalar `_text`, with string-literal input such as `"{a,b}"`. This was the default before v2.x arrays support.

**Stringify numerics.** `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES=true` (`--stringify-numeric-types`) changes only the **output** JSON of `bigint`, `bigserial`, `numeric`, `float8` and `money` (`isBigNum`) to strings. Schema types are unchanged. Aggregates over these types are also stringified (inferred). Details belong in doc 03 (SQL semantics).

**The `count` and `affected_rows` fields** are `Int!`.

**`limit` and `first`/`last`** use `nonNegativeInt`, i.e. `Int`. **`offset`** uses the `bigInt` parser: also named `Int` in the schema, but it **also accepts a string** containing a 64-bit integer (`lib/schema-parsers/.../Scalars.hs`).

---

## 8. Enum tables

Source: `Backends/Postgres/DDL/Table.hs` (enum validation and value fetch), `Instances/Schema.hs` `enumParser`, and `Common.hs` `mkEnumTypeName`.

**Requirements** for `is_enum: true`:

- a single-column primary key of type exactly **`text`** (`RawColumnTypeScalar PGText`; `varchar` is rejected, verified);
- at most one other column, which must also be exactly `text` and becomes the comment column;
- at least one row;
- every PK value must be a valid GraphQL name, other than `true`, `false` or `null`.

Violations fail tracking with `the table "<t>" cannot be used as an enum ` followed by the reasons (`Backends/Postgres/DDL/Table.hs` `showErrors`). The reasons are:

- `the table must have a primary key`
- `the table’s primary key must not span multiple columns (<cols>)`
- `the table’s primary key ("<col>") must have type "text", not type "<t>"`
- `the table must have at least one row`
- `the value "x" is not a valid GraphQL enum value name`, or the plural form `the values "a" and "b" are not valid GraphQL enum value names`
- `the table’s comment column ("<col>") must have type "text", not type "<t>"`
- `the table must have exactly one primary key and optionally one comment column, not <n> columns (<cols>)`

Note the typographic apostrophe `’`. Copy the exact joining from `makeReasonMessage`.

Example:

```sql
CREATE TABLE color (value text PRIMARY KEY, comment text);
INSERT INTO color VALUES ('red', 'Red colour'), ('blue', NULL);
ALTER TABLE article ADD COLUMN color text REFERENCES color(value);
```

Schema effects:

- **`color` stays a normal table** with all its usual fields and types.
- **A new ENUM type** `color_enum` is created, with description `null`. Its values are the PK values, each described by the comment column, or `null` when absent. Values are listed alphabetically: `blue`, then `red` with description `Red colour`.
- **Columns referencing the enum table through a FK** use `color_enum` instead of `String`: in the table object, insert, set, stream-value and pk inputs, and so on. Their comparison exp is `color_enum_comparison_exp` with `_eq`, `_in`, `_is_null`, `_neq` and `_nin`.
- **Naming:**
  - graphql-default: type `ColorEnum`, values upper-cased (`BLUE`, `RED`). Fixture: `enum_value_convention.yaml`, whose `DaysOfTheWeekEnum` has values `FRIDAY`…`WEDNESDAY`.
  - With `custom_name: hue` on the enum table: `hue_enum` (hasura-default) or `hueEnum` (graphql-default, CustomName prefix not re-cased). This is derived from `mkEnumTableTypeName` (`fromCustomName customName <> enum`) and then `mkTypename`, so the source type-name prefix and suffix also apply.
  - Enum tables in other schemas: `myschema_color_enum`.
- **Values are reloaded** on metadata reload. Data changes are not picked up automatically.

---

## 9. Relationships, computed fields, functions

### 9.1 Relationships

See §6.2 for fields, §6.3 for bool_exp, §6.6 for order_by and §6.8 for insert.

- Names are used verbatim and must be valid GraphQL names.
- An object relationship on `article` named `author` produces `author: author!`, because `author_id` is NOT NULL and the relationship is FK-based.
- With a nullable FK column, or a manual relationship, it produces `author: author`.

### 9.2 Computed fields

Source: `Backends/Postgres/Schema/Select.hs` (`computedFieldPG`), `GraphQL/Schema/Select.hs`, and `Backends/Postgres/DDL/ComputedField.hs`.

```sql
CREATE FUNCTION author_full_name(a author, sep text DEFAULT ' ') RETURNS text AS ... STABLE;
CREATE FUNCTION author_top_articles(a author, n int) RETURNS SETOF article AS ... STABLE;
```

```graphql
type author {
  """A computed field, executes function "author_full_name" """
  full_name(
    """input parameters for computed field "full_name" defined on table "author" """
    args: full_name_author_args
  ): String
  """A computed field, executes function "author_top_articles" """
  top_articles(
    """input parameters for computed field "top_articles" defined on table "author" """
    args: top_articles_author_args!
    distinct_on: [article_select_column!], limit: Int, offset: Int,
    order_by: [article_order_by!], where: article_bool_exp
  ): [article!]
}
input full_name_author_args { sep: String }
input top_articles_author_args { n: Int }
```

- **Description** (`Backends/Postgres/DDL/ComputedField.hs`): a non-empty metadata `comment` wins. Otherwise the default is `A computed field, executes function "<qualified fn>"`.
  - In released v2 (verified at `v2.51.0`), an Automatic comment first used the function's DB COMMENT and only then the default. That is gone in this tree.
  - `comment: null` or absent is Automatic, i.e. the default. `comment: ""` gives description `null` (verified by fixture `descriptions/explicit_no_comment_in_metadata.yaml`).
- **Scalar result:** nullable scalar. JSON and JSONB results also get a `path: String` arg.
- **Table result:** `[T!]`, a **nullable** list, with the standard select args. The return table needs select permission. **A computed field whose function returns a single (non-SETOF) row of a table is still exposed as `[T!]`.** Postgres computed fields have only `CFRScalar` and `CFRSetofTable`: `rfiReturnsTable` decides, not `retset` (verified, `DDL/ComputedField.hs`).
- **Volatile functions** cannot be computed fields (`CFVEFunctionVolatile`).
- **`args`** is present only when the function has user-visible arguments, i.e. arguments other than the table row, the session argument and other hidden ones.
  - It is **non-null** iff at least one argument has no default.
  - The input object fields are **all nullable**, with descriptions `null`. Arguments without defaults are enforced at parse time (inferred).
- **Type name:** `mkTypename(applyTypeNameCaseIdentifier(fromAutogeneratedName(cf) <> T <> [args]))` (`SourceCustomization.hs:356` `mkFunctionArgsTypeName`, called from `Backends/Postgres/Schema/Select.hs:354`).
  - Hasura-default: `full_name_author_args`.
  - graphql-default **without** the Sep-2023 flag: also `full_name_author_args`. `functionArgs` forces `tCase = HasuraCase` unless `soNamingConventionSep2023` is on (verified, `Backends/Postgres/Schema/Select.hs`). The source type-name prefix and suffix still apply via `mkTypename`. Argument field names are verbatim.
  - graphql-default **with** the Sep-2023 flag: the cf name is a single Autogenerated part, not split on `_`, so the result is `Full_nameAuthorArgs` (derived from code). A single-word cf such as `fullname` gives `FullnameAuthorArgs`. Argument field names become camelCase.
- **Presence:** the select permission must list the computed field. Admin always sees it.

### 9.3 Tracked functions

Source: `Function/Cache.hs`, `Function/Common.hs`, `Backends/Postgres/DDL/Function.hs`, `Backends/Postgres/Schema/Select.hs` (`buildFunctionQueryFieldsPG`, `buildFunctionMutationFieldsPG`), and `GraphQL/Schema/Build.hs`.

```sql
CREATE FUNCTION search_authors(search text) RETURNS SETOF author AS ... STABLE;
```

**query_root** (and **subscription_root**) gets:

```graphql
"""execute function "search_authors" which returns "author" """
search_authors(
  """input parameters for function "search_authors" """
  args: search_authors_args!
  distinct_on: [author_select_column!], limit: Int, offset: Int, order_by: [author_order_by!], where: author_bool_exp
): [author!]!

"""execute function "search_authors" and query aggregates on result of table type "author" """
search_authors_aggregate(args: search_authors_args!, distinct_on: ..., limit: Int, offset: Int, order_by: ..., where: ...): author_aggregate!

input search_authors_args { search: String }
```

**Return types:**

- SETOF table: `[T!]!`.
- Single row (non-SETOF composite): nullable `T` (verified: `selectFunction` uses `tableSelectionSet` for `JASSingleObject`). It still takes the full table arguments (`distinct_on`, `limit`, `offset`, `order_by`, `where`).
- Functions returning `SETOF <tracked table>` or a single composite row of a tracked table are required. Others fail validation (`Backends/Postgres/DDL/Function.hs`). Scalar-returning functions can be tracked only as computed fields, or via logical models in newer versions.

**Names:**

- The GraphQL name `getFunctionGQLName` is:
  1. `configuration.custom_root_fields.function`, verbatim, if set;
  2. else `configuration.custom_name`;
  3. else `setCase(funcGivenName)`.

  Here `funcGivenName` is the snake GraphQL name of the qualified function: `search_authors`, or with a schema prefix for non-public schemas, e.g. `myschema_search`. `setCase = applyFieldNameCaseCust tCase` (`Backends/Postgres/DDL/Function.hs:129`), which means `snakeToCamel` in graphql-default, so `searchAuthors`. A `custom_name` is used verbatim.
- **Aggregate** (`getFunctionAggregateGQLName`):
  1. `custom_root_fields.function_aggregate`, verbatim;
  2. else `custom_name <> "_aggregate"`, **not re-cased**. So `custom_name: findAuthors` gives `findAuthors_aggregate` even in graphql-default;
  3. else `setCase(funcGivenName <> "_aggregate")`, i.e. `search_authors_aggregate` or `searchAuthorsAggregate`.
- **Args type** (`getFunctionArgsGQLName`): `setCase((custom_name or funcGivenName) <> "_args")`. That is a **field** casing, so graphql-default yields the camelCase *type* name `searchAuthorsArgs`.
  - It is then wrapped as `fromCustomName` and given mkTypename (`Backends/Postgres/Schema/Select.hs:354`).
  - With the Sep-2023 flag it is wrapped as `fromAutogeneratedName` instead, so `SearchAuthorsArgs`.
  - Hasura-default: `search_authors_args`.
  - The custom root field names do **not** affect the args type name.
- Root field prefix and suffix apply.

**Descriptions:**

- The function `comment` from metadata (a plain `Maybe Text`, not `Comment`) overrides the default for the **query** field only (`buildFunctionQueryFieldsPG`: `fromMaybe default _fiComment`). Verified:
  - the aggregate field always uses its default;
  - a **mutation** function field always uses `execute VOLATILE function ...` and **ignores** `comment` (`buildFunctionMutationFieldsPG`);
  - the Relay connection field always uses the default.
- Defaults:
  - query: `execute function "fn" which returns "t"`;
  - aggregate: `execute function "fn" and query aggregates on result of table type "t"`;
  - mutation: `execute VOLATILE function "fn" which returns "t"`.
- Here `"fn"` and `"t"` use the `<<>` quoting of the qualified names.
- **DB COMMENTs are never used for tracked-function root fields**, in this tree or in released `v2.51.0`. The Postgres code reads only `_fiComment` (metadata); `_fiDescription` (the DB comment) is used only by the DataConnector backend. Inferred: `comment: ""` yields an empty-string description, not `null`.
- Args arg: `input parameters for function "<name>"`. `<name>` is the un-prefixed function GraphQL name (`_fiGQLName`) on the query, mutation and connection fields, and the **aggregate** name (`_fiGQLAggregateName`, e.g. `search_authors_aggregate`) on the aggregate field (verified, `customSQLFunctionArgs` call sites).

**Args:**

- Unnamed arguments become `arg_1`, `arg_2` and so on, 1-based by position.
- The `session_argument` from config is hidden.
- `args` is non-null iff some argument lacks a default.
- The `args` argument is **omitted entirely** when there are no visible arguments.
- Arg input fields are all nullable.

**`exposed_as`:**

- Default: `VOLATILE` functions go to `mutation_root`; `STABLE` and `IMMUTABLE` functions go to `query_root` **and** `subscription_root`.
- **Reversed from the earlier draft (verified, `Backends/Postgres/DDL/Function.hs` `validateFunction`).** `exposed_as: query` on a VOLATILE function is **allowed**: "we take the user at their word". `exposed_as: mutation` on a STABLE or IMMUTABLE function **fails** validation (`NonVolatileFunctionAsMutation`).
- Mutation functions get **no** aggregate field.

**Presence:**

- The return table needs select permission for the role.
- The function needs to be permitted: admin always; otherwise a function permission for the role, or `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS=true` (the default), which applies to **query functions only**. Mutation functions always need explicit function permissions.
- The aggregate field needs `allow_aggregations` on the return table.

**Validation errors** come from `Backends/Postgres/DDL/Function.hs`. Copy the messages verbatim. Cases:

- variadic functions;
- return type is not a composite table type, or the table is not tracked;
- an argument is a table type;
- invalid argument names;
- duplicate names;
- `exposed_as` misuse.

**Function names that are invalid GraphQL names** are skipped, or fail tracking (inferred).

---

## 10. Collisions and sanitisation

| Situation | Behaviour | Source |
|---|---|---|
| Two root fields with the same name, from different tables, sources, functions or actions | Error: `Encountered conflicting definitions in the selection set for 'query_root' for field 'x' defined in [...]. Fields must not be defined more than once across all sources.` (see §3 for the plural form) | `lib/schema-parsers/.../Internal/Parser.hs` `safeSelectionSet` |
| A remote schema root field clashing with another remote or a source field | Inconsistency: `Duplicate remote field 'x'` or `Field cannot be overwritten by remote field 'x'` | `GraphQL/Schema.hs` |
| Two **different** type definitions with the same name, e.g. table `author_aggregate` vs the aggregate type of `author` | `Found conflicting definitions for GraphQL type 'X'.  The definition at '<a.b>' differs from the definition at e.g. '<c.d>'.` followed by `\nFormer has definition:\n<SDL>\nLatter has definition:\n<SDL>` (two spaces after the first period; note `at e.g.`; the type name is single-quoted; error code `unexpected`) | `lib/schema-parsers/.../Schema/Collect.hs` `instance ToErrorValue ConflictingDefinitions` |
| Two definitions with the same name and the same **field names** but different descriptions | **Not a conflict.** The first collected definition wins (§12.4). | `Schema/Definition.hs` `Eq` instances ignore descriptions |
| Table name not a valid GraphQL name | Table silently omitted unless it has `custom_name` | `takeValidTables` |
| Column name not a valid GraphQL name | Error on track, or column omitted (inferred). Fix with `column_config.custom_name`. | `Table/API.hs` |
| Column custom names duplicate | Error | `Table/API.hs` |
| Relationship or computed field name equals a column name | `field definition conflicts with postgres column` | `RQL/DDL/Schema/Cache/Fields.hs` |
| Relationship or computed field name equals another column's custom GraphQL name | `field definition conflicts with custom field name for postgres column "<col>"` | `Fields.hs` |
| Relationship vs computed field vs remote relationship | `conflicting definitions for field "<name>"` (verified) | `Fields.hs` |
| Duplicate custom root field names in one table | `the following custom root field names are duplicated: ...` | `Table/Cache.hs` |
| `custom_name` collides with an existing type or field | `checkConflictingNode`: `node <x> already exists in current graphql schema` | `Table/API.hs` |

Name sanitisation:

- Hasura does **not** rewrite invalid characters.
- `textToName` and `textToGQLIdentifier` (`GraphQL/Schema/Common.hs`) either succeed or throw `Unexpected` / `ValidationFailed`: `cannot include <x> in the GraphQL schema because <y> is not a valid GraphQL identifier`.

Remote-schema vs source type name clashes are covered in doc 06. The same `Collect.hs` check applies.

---

## 11. Native queries and logical models (brief, P2)

Source: `NativeQuery/Schema.hs`, `LogicalModel/Schema.hs` and `LogicalModelResolver/Schema.hs`.

- **Logical model `lm`** yields an OBJECT type named after the logical model, verbatim. It uses **no mkTypename and no naming-convention casing** (inferred).
  - Fields are typed from the field definitions; nested logical models become nested objects or arrays.
  - It also gets `lm_bool_exp`, `lm_order_by` and `lm_enum_name` (inferred names). These are shared, so a logical model used by several native queries yields one set of types.
- **Native query `nq`** with `returns: lm` gets a root field `nq(args: nq_arguments!, distinct_on, limit, offset, order_by, where): [lm!]!`, with root-field prefix and suffix applied.
  - `args` is omitted when the native query has no arguments.
  - The description is the native query `description`, else `null` (inferred).
- **Native query relationships** add fields like table relationships.

Treat this as a separate milestone. Snapshot the upstream introspection output for the api-tests `NativeQueries` suites and diff against it.

---

## 12. Descriptions

### 12.1 Quoting helpers

`lib/hasura-extras/src/Data/Text/Extended.hs`:

- `x <>> y` is `x <> "\"" <> toTxt y <> "\""`.
- `<<>` is the mirror image.

`toTxt` of a qualified table is `schema.table`, or just `table` for `public`. It does **not** quote identifiers with special characters (inferred; see `qualifiedObjectToText`).

### 12.2 Description catalogue (exact strings)

| Element | String |
|---|---|
| table object type (default) | `columns and relationships of "<t>"` |
| select root | `fetch data from the table: "<t>"` |
| by_pk root | `fetch data from the table: "<t>" using primary key columns` |
| aggregate root | `fetch aggregated fields from the table: "<t>"` |
| stream root | `fetch data from the table in a streaming manner: "<t>"` |
| insert / insert_one | `insert data into the table: "<t>"` / `insert a single row into the table: "<t>"` |
| update / by_pk / many | `update data of the table: "<t>"` / `update single row of the table: "<t>"` / `update multiples rows of table: "<t>"` (sic) |
| delete / by_pk | `delete data from the table: "<t>"` / `delete single row from the table: "<t>"` |
| `where` (select) | `filter the rows returned` |
| `where` (update / delete) | `filter the rows which have to be updated` / `filter the rows which have to be deleted` |
| `order_by` | `sort the rows by one or more columns` |
| `distinct_on` | `distinct select on columns` |
| `limit` | `limit the number of rows returned` |
| `offset` | `skip the first n rows. Use only with order_by` |
| `objects` / `object` | `the rows to be inserted` / `the row to be inserted` |
| `on_conflict` arg | `upsert condition` |
| `updates` | `updates to execute, in order` |
| `pk_columns` | `null` (verified, `Update/Batch.hs` `P.field pkFieldName Nothing`) |
| `updates` input type (`T_updates`) | `null` (verified) |
| `_set` field / type | `sets the columns of the filtered rows to the given values` / `input type for updating data in table "<t>"` |
| `_inc` field / type | `increments the numeric columns with given value of the filtered values` / `input type for incrementing numeric columns in table "<t>"` |
| jsonb ops | see §4.1 (field and type share the text) |
| bool_exp | `Boolean expression to filter rows from the table "<t>". All fields are combined with a logical 'AND'.` |
| comparison exp | `Boolean expression to compare columns of type "<S>". All fields are combined with logical 'AND'.` |
| order_by type | `Ordering options when selecting data from "<t>".` |
| `order_by` enum | `column ordering options` (values in §6.6) |
| select_column | `select columns of table "<t>"`, values `column name` |
| update_column | `update columns of table "<t>"`, values `column name` |
| placeholder | enum `placeholder for update columns of table "<t>" (current role has no relevant permissions)`, value `_PLACEHOLDER`: `placeholder (do not use)` |
| constraint | `unique or primary key constraints on table "<t>"`, values `unique or primary key constraint on columns "a", "b"` |
| on_conflict type | `on_conflict condition type for table "<t>"` |
| insert input | `input type for inserting data into table "<t>"` |
| obj / arr rel insert | `input type for inserting object relation for remote table "<t>"` / `... array relation ...` |
| mutation response | `response of any mutation on the table "<t>"`; fields `number of rows affected by the mutation`, `data from the rows affected by the mutation` |
| pk columns input | `primary key columns input for table: <t>` (unquoted) |
| aggregate | `aggregated selection of "<t>"` |
| aggregate fields | `aggregate fields of "<t>"` |
| op fields | `aggregate <op> on columns` |
| aggregate order by | `order by aggregate values of table "<t>"` |
| op order by | `order by <op>() on columns of table "<t>"` |
| JSON `path` arg | `JSON select path` |
| relationships | `An object relationship`, `An array relationship`, `An aggregate relationship`, `An array relationship connection` |
| stream args | `maximum number of rows returned in a single batch`, `cursor to stream the results returned by the query`; cursor fields `Stream column input with initial value`, `cursor ordering` |
| stream types | `Streaming cursor of the table "<GQLTypeName>"`, `Initial value of the column from where the streaming should start`, `ordering argument of a cursor`; values `ascending ordering of the cursor` and `descending ordering of the cursor` |
| function root | `execute function "<fn>" which returns "<t>"`, aggregate `execute function "<fn>" and query aggregates on result of table type "<t>"`, mutation `execute VOLATILE function "<fn>" which returns "<t>"` |
| function args arg | `input parameters for function "<gqlname>"` |
| computed field | `A computed field, executes function "<fn>"`; args `input parameters for computed field "<cf>" defined on table "<t>"` |
| mutation_root | `mutation root` |
| placeholder root field | see §3 |
| Relay | `An object with globally unique ID` (Node), `A globally unique identifier` (id), `A globally unique id` (node arg), `A Relay connection object on "<t>"` |
| Apollo | `Scalar _Any`, `A union of all types that use the @key directive`, `query _Entity union`, `SDL representation of schema` |
| group by (exp.) | `Groups the table by the specified keys`, `The keys on which to group by`, `Allows the selection of a grouping key from ...`, `A column grouping key`, `Group by fields of ...`, `Allows the selection of fields from the grouping key of ...` (copy exact text from `Select.hs`) |

**Custom root field comments.** When `custom_root_fields.X.comment` is set, it replaces the default root description.

**`mkDescriptionWith`** (`GraphQL/Schema/Common.hs:486`) is `maybe default (\d -> T.unlines [d, "\n", default])`. It combines a DB comment with a default, giving `<comment>\n\n\n<default>\n`.

In this tree it is exported but has **no call sites**. It is a leftover from an older DB-comment import, so ignore it. The released-v2 behaviour in §12.3 does **not** use it: there, a DB comment *replaces* the default and is not combined with it.

### 12.3 Database COMMENTs

**Current tree (master `895e7e9`, 2026-10-12):** DB comments are **not** imported.

- Table, column and function descriptions come from metadata only: `configuration.comment`, `column_config.*.comment`, the function `comment` and the computed field `comment`.
- Columns without a configured comment have a `null` description.
- See `src-rsr/pg_table_metadata.sql` and `src-rsr/pg_function_metadata.sql` (description `NULL`), and `Backends/Postgres/DDL/Source.hs` `pgStripImportedDescriptions`, which also clears descriptions restored from stored introspection.
- Fixture: `tests-py/queries/graphql_introspection/descriptions/automatic_comment_in_db.yaml` expects the defaults despite DB comments.

**Released v2.x (verified at tag `v2.51.0`, the newest v2 tag found):** the SQL reads `pg_description` and `col_description`, and the same Haskell then applies them only when the metadata comment is **Automatic** (absent or `null`):

- `COMMENT ON TABLE` replaced the default `columns and relationships of ...` (`Table/API.hs` `buildDescription`: `_ptmiDescription <|> Just default`).
- `COMMENT ON COLUMN` became the column description; otherwise it was `null`.
- `COMMENT ON FUNCTION` became the **computed field** description, before the `A computed field, executes function ...` default.
- **Tracked-function root fields never used the DB comment.** `buildFunctionQueryFieldsPG` reads only the metadata comment, and its code is identical in `v2.51.0` and master.
- An explicit metadata comment always won, and `""` always gave `null`.
- The `v2.51.0` fixture `automatic_comment_in_db.yaml` expects `description: What a great comment in the DB` on the type, `The name column comment in the DB` on the column, and `What a great comment on the function in the DB` on the computed field.
- The CLI codemod `migrate-postgres-comments` moves DB comments into metadata (unverified here).

**Recommendation:**

- Reviewer note: every production v2 deployment runs a **released** image, and `v2.51.0` imports comments. A drop-in replacement should therefore probably **default to importing** (released behaviour), with a switch such as `HASURA_RS_IMPORT_PG_COMMENTS=false` for the master-tree behaviour. The original draft proposed the opposite default. Open question for the architecture thread.
- Either way the switch affects only three things: the table type description, column descriptions and computed-field descriptions. Tracked-function root fields are unaffected (above).

### 12.4 First-wins descriptions for shared types

Types are deduplicated by name during collection (§13.2). Two definitions with the same name and the same field names are considered equal even when their descriptions differ, and **the first one encountered wins**.

Known case: a function `search_authors` returning `author` builds an `author_aggregate` parser **without** a description, while the table's own `author_aggregate` has `aggregated selection of "author"`.

- Which description introspection shows depends on DFS order.
- `query_root` fields are visited in sorted order, so `author_aggregate` (the table field) comes before `search_authors_aggregate`, and the description is present.
- If the function's root field sorted first, e.g. `a_search_aggregate`, the description would be `null`.
- The Rust implementation must reproduce the collection order (§13.2) to match exactly. Verified: `Collect.hs` keeps the first definition seen and never re-checks descriptions (`SomeDefinitionTypeInfo` `Eq` ignores description; `eqByName` compares field *names* only). `Backends/Postgres/Schema/Select.hs` `selectFunctionAggregate` builds `<T>_aggregate` with description `Nothing`.

---

## 13. Introspection specifics

### 13.1 Ordering

Source: `lib/schema-parsers/src/Hasura/GraphQL/Parser/Schema/Definition.hs`. Smart constructors sort their contents, and the printer sorts arguments.

Everything is sorted by **name, using codepoint (byte) order**: `A`–`Z` < `_` < `a`–`z`. Concretely:

- `__schema.types`: sorted by name. Includes the `__*` meta types and the built-in scalars that are reachable.
- Object and interface `fields`: sorted.
- Field `args`: sorted, at print time (`sortOn dName` in `Introspect.hs`).
- `inputFields`: sorted.
- `enumValues`: sorted, so `ASC` < `DESC`, and `_PLACEHOLDER` sorts before lowercase (verified: the `TIEnum` pattern synonym applies `NE.sortWith dName`).
- `possibleTypes` and `interfaces`: sorted.
- `directives`: in declaration order: `include`, `skip`, `cached` (verified: `Directives.hs` `directivesInfo` = `inclusionDirectives` [include, skip] then `customDirectives` [cached, `_multiple_top_level_fields`], filtered by `dAdvertised`).
- **Directive args are NOT sorted** (verified: `Introspect.hs` `directiveSet` prints `diArguments` without sorting). `@cached` lists `ttl` then `refresh`.

Example: `author_bool_exp` input fields come out as `_and, _not, _or, articles, articles_aggregate, bio, ...`, because `_` (0x5F) < `a` (0x61). In graphql-default, a type such as `Author` sorts before `_Any`, and `_Any` before `article`.

### 13.2 Type collection

`Collect.hs` `collectTypeDefinitions`:

- **Roots are visited in this order:** query_root, mutation_root, subscription_root, the argument types of the directives, then the `__schema` field definition, then the `__type` field definition.
- **Depth-first.** For each object field: its arguments first, in their stored order, then the return type. For input objects: the field types in order. Object, interface and input-object field lists are stored sorted by the smart constructors in `Definition.hs`. Inferred: field *arguments* are stored in construction order, and are sorted only when printed. This can matter for first-wins descriptions (§12.4).
- **Only reachable types are listed.** For example, `author_obj_rel_insert_input` appears only if some insert input references it, and `Float` appears only if some field uses it.
- **Conflicts** are raised for same name with a different kind or different field names (see §10).

**Built-in scalars** appear only when referenced:

- `Boolean` and `String` are always referenced by the introspection types.
- `Int` is referenced by `@cached(ttl: Int!)`.
- `Float` and `ID` only when used. `ID` is used in Relay and by remote schemas.

### 13.3 Deprecation

- Hasura-generated fields are **never** deprecated: `isDeprecated: false` and `deprecationReason: null` everywhere.
- `includeDeprecated` is accepted and ignored.
- Remote schema deprecations are not propagated (inferred; see doc 06).

### 13.4 Meta-type self-description quirks

Querying `__type(name: "__Type")` and similar returns Hasura's own definitions from `GraphQL/Schema/Introspect.hs`, which **differ from the spec**. The reviewer verified this against the source: every object-valued meta field is `P.subselection_`/`P.subselection` over a `P.selectionSet`, which has type `TNamed Nullable` (`Internal/Parser.hs` `selectionSetObject`). Every scalar meta field is `P.selection_ ... P.string` (`String!`, from `typeNamed` with NonNullable) or `P.boolean`.

Exact field lists, in introspection (sorted) order:

- `__Schema`: `description: String!`, `directives: __Directive`, `mutationType: __Type`, `queryType: __Type`, `subscriptionType: __Type`, `types: __Type`.
- `__Type`: `description: String!`, `enumValues(includeDeprecated: Boolean = false): __EnumValue`, `fields(includeDeprecated: Boolean = false): __Field`, `inputFields: __InputValue`, `interfaces: __Type`, `kind: __TypeKind!`, `name: String!`, `ofType: __Type`, `possibleTypes: __Type`.
- `__Field`: `args: __InputValue`, `deprecationReason: String!`, `description: String!`, `isDeprecated: String!`, `name: String!`, `type: __Type`.
- `__InputValue`: `defaultValue: String!`, `description: String!`, `name: String!`, `type: __Type`.
- `__EnumValue`: `deprecationReason: String!`, `description: String!`, `isDeprecated: String!`, `name: String!`.
- `__Directive`: `args: __InputValue`, `description: String!`, `isRepeatable: Boolean!`, `locations: String!`, `name: String!`.
- `__TypeKind` (ENUM): `ENUM`, `INPUT_OBJECT`, `INTERFACE`, `LIST`, `NON_NULL`, `OBJECT`, `SCALAR`, `UNION`.
- On `query_root` (not listed): `__schema: __Schema` and `__type(name: String!): __Type`.

All meta types and meta fields have description `null`. `includeDeprecated` is `fieldWithDefault ... (G.VBoolean False) (P.nullable P.boolean)`.

The quirks in detail:

- `__Type.fields(includeDeprecated: Boolean = false): __Field`. The result is a **single nullable object**, not a list. The same applies to:
  - `interfaces`, `possibleTypes`, `enumValues(includeDeprecated: Boolean = false)`, `inputFields`, `ofType`;
  - `__Schema.types`, `queryType`, `mutationType`, `subscriptionType`, `directives`;
  - `__Field.args`, `__Field.type`, `__InputValue.type`, `__Directive.args`.
  - The reason: `P.subselection_` over a `selectionSet` yields a nullable named type.
- `name`, `description`, `defaultValue`, `isDeprecated`, `deprecationReason` and `locations` are all `String!` (`P.string`), even though most can be null and `isDeprecated` is a boolean.
- `__Directive.isRepeatable` is `Boolean!`.
- `__Type.kind` is `__TypeKind!`. The enum `__TypeKind` has values ENUM, INPUT_OBJECT, INTERFACE, LIST, NON_NULL, OBJECT, SCALAR and UNION, with no descriptions.
- There is **no `__DirectiveLocation` enum type**.
- `__Type` has no `specifiedByURL` or `isOneOf` fields. `__Schema` has a `description` field (`String!`), and its value is always `null`.
- `__schema: __Schema` and `__type(name: String!): __Type` are queryable but not listed on `query_root`.
- **All meta types have description `null`.**

Codegen tools usually ignore these meta-types. Byte-for-byte diffs of a full introspection dump will still include them, so reproduce them exactly. To obtain the exact field list per meta type, transcribe `Introspect.hs` lines ~258–720 (P0 for golden-file tests).

**Values returned by introspection:**

- `defaultValue` is the GraphQL-printed default (`"[]"`, `"60"`, `"false"`, `"true"`), or `null`.
- `locations` are printed as strings: `["FIELD","FRAGMENT_SPREAD","INLINE_FRAGMENT"]`.
- `description` is `null` when absent.

### 13.5 Directives

From `lib/schema-parsers/src/Hasura/GraphQL/Parser/Directives.hs`:

```graphql
"""whether this query should be included"""
directive @include(if: Boolean!) on FIELD | FRAGMENT_SPREAD | INLINE_FRAGMENT
"""whether this query should be skipped"""
directive @skip(if: Boolean!) on FIELD | FRAGMENT_SPREAD | INLINE_FRAGMENT
"""whether this query should be cached (Hasura Cloud only)"""
directive @cached(
  """measured in seconds"""
  ttl: Int! = 60
  """refresh the cache entry"""
  refresh: Boolean! = false
) on QUERY
```

- `isRepeatable: false` for all three.
- The `if` args have no description (verified: `ifArgument = field Name._if Nothing boolean`).
- `@_multiple_top_level_fields` (description `INTERNAL TESTING TOOL DO NOT USE`, location SUBSCRIPTION) is accepted by the parser but **not advertised** (verified: `dAdvertised = False`). Unknown directives fail with `directive '<name>' is not defined in the schema`. Misplaced ones fail with `directive '<name>' is not allowed on a query` (or `a field`, and so on). Duplicates fail with `the following directives are used more than once: ...`.
- `@deprecated` and `@specifiedBy` are **not** advertised.

### 13.6 Introspection disabling

- Metadata `graphql_schema_introspection.disabled_for_roles` (doc 01) makes `__schema` and `__type` queries fail for those roles.
- **This is EE/Pro-only.** In OSS, `executeIntrospection` (`src-lib/Hasura/App.hs` ~line 802) ignores the disabled-roles list, so the metadata is stored but has no effect (verified). The test class `TestDisableGraphQLIntrospection` is marked `enterprise_edition`.
- The EE error, from the fixture `tests-py/queries/graphql_introspection/disable_introspection/disable_introspection.yaml`, is code `not-supported` with message `introspection is disabled for role: "<role>"`. The earlier draft's wording was wrong.
- `__typename` remains allowed.
- Decision for the Rust port: either match OSS (ignore the setting) or EE. Since deployments that set it expect it to work, implementing the EE behaviour is reasonable.

### 13.7 `__typename`

Returns the runtime object type name, after customization:

- for table objects: `author`, `AuthorLocal`, `tpref_author_tsuff`;
- for root types: `query_root`, `mutation_root`, `subscription_root`;
- for namespace fields: the namespace object type;
- for aggregates: `author_aggregate`, `author_aggregate_fields`, and so on;
- for `mutation_response`: `author_mutation_response`.

---

## 14. Relay (`/v1beta1/relay`)

Source: `GraphQL/Schema/Relay.hs`, `GraphQL/Schema/Node.hs`, `GraphQL/Schema/Select.hs` (connection), and `GraphQL/Schema.hs` `buildRelayRoleContext`.

**query_root contains:**

- `node(id: ID!): Node`. The arg is described `A globally unique id`.
- For every table **with a primary key** and select permission: `author_connection(after: String, before: String, distinct_on: [author_select_column!], first: Int, last: Int, order_by: [author_order_by!], where: author_bool_exp): authorConnection!`, with root description `fetch data from the table: "<t>"` (verified, `buildTableRelayQueryFields`; custom root-field comments are not applied). The connection type is non-null, hence `authorConnection!` (verified, `selectTableConnection` uses `P.nonNullableParser`).
- For every tracked function returning such a table: `search_authors_connection(args: ..., ...)`.

**No other root fields.** There are no plain selects, by_pk or aggregate fields. **Remote schemas are excluded.** Actions are included as mutations only.

**mutation_root** contains the same table mutations as the normal endpoint, plus action mutations.

**subscription_root** contains the connection fields (inferred).

**Types:**

```graphql
"""An object with globally unique ID"""
interface Node {
  """A globally unique identifier"""
  id: ID!
}
type author implements Node {
  articles(...): [article!]!
  articles_aggregate(...): article_aggregate!
  articles_connection(after: String, before: String, distinct_on: ..., first: Int, last: Int, order_by: ..., where: ...): articleConnection!
  bio: String
  ...
  id: ID!            # replaces the integer `id` column field; the PK value is reachable only through the node id
  ...
}
"""A Relay connection object on "author" """
type authorConnection { edges: [authorEdge!]!  pageInfo: PageInfo! }
type authorEdge { cursor: String!  node: author! }
type PageInfo { endCursor: String!  hasNextPage: Boolean!  hasPreviousPage: Boolean!  startCursor: String! }
```

- **Connection and Edge names** are the type-cased table name with `Connection` / `Edge` appended *as a plain name*, then mkTypename. In hasura-default that is `authorConnection` and `authorEdge`, with no underscore.
- **`PageInfo` and `Node`** do not get mkTypename.
- **Tables without a PK** have no connection field, and their object type does not implement Node (verified: `Select.hs` `tableSelectionSet` adds the `id` field and `Node` interface only for `(RelaySchema, Just pkeyColumns, Just xRelayInfo)`).
- **The `id` column clash.** A table with a column whose GraphQL name is `id` loses that column field: `id: ID!` replaces it (verified: `Select.hs` `guard $ isHasuraSchema schemaKind || fieldName /= Name._id`). The object's `id` field has description `null`; only the `Node` interface's `id` carries `A globally unique identifier`.
- **Node id encoding:** base64 of JSON `[1, "<schema>", "<table>", <pk values...>]`, version 1. Details belong in doc 03.
- **Supported backends:** only Vanilla Postgres (`pgKind = Vanilla`). Citus, Cockroach and other backends do not support Relay.

---

## 15. Apollo Federation (brief, P2)

Source: `GraphQL/ApolloFederation.hs`.

**Enabled** by the experimental flag `apollo_federation`, or by the metadata setting `apollo_federation_config` with a status of enabled (doc 01).

**Added schema elements:**

- `_service: _Service!`, where `_Service { sdl: String! }`. The **`sdl` field** is described `SDL representation of schema`; the `_Service` type and the `_service` field have description `null` (verified, `GraphQL/ApolloFederation.hs`).
- `_entities(representations: [_Any!]!): _Entity`, described `query _Entity union`. Note the result is a **nullable single `_Entity`**, not `[_Entity]!` as in the Apollo spec: `P.subselection` over `P.selectionSetUnion` adds no list wrapper (verified from code, `mkEntityUnionFieldParser`). `representations` has no description. Present only when ≥1 table has `apollo_federation_config: {enable: v1}`.
- Scalar `_Any`, described `Scalar _Any`.
- Union `_Entity`, described `A union of all types that use the @key directive`, whose possible types are the enabled tables' object types.

**Effects on tables:**

- Enabled table types get `@key(fields: "<pk columns space-separated>")` in the `_service.sdl` output only. Introspection does not show directives on types.
- `_service.sdl` is Hasura's own SDL printer (`generateSDL`). Its output format must also match byte-for-byte for federation gateways that hash it (inferred importance). Defer to a later doc.

---

## 16. Role-dependence summary

For a non-admin role `user`:

| Permission | Effect |
|---|---|
| select `columns: [id, name]`, filter, no `allow_aggregations` | `author` has only `id` and `name`, plus relationships to readable tables. `author_bool_exp`, `author_order_by` and `author_select_column` are restricted the same way. There is no `author_aggregate` root field and no `articles_aggregate` on other tables pointing at author. `author_by_pk` exists because `id` is selectable. |
| insert `columns: [name, bio]`, `set: {id: ...}` | `author_insert_input` has `bio`, `name` and any relationship inserts. Preset columns are excluded. |
| update `columns: [name]`, `set: {bio: ...}` | `author_set_input` has `name`. There is no `_inc` because no numeric column is updatable, and no jsonb ops. `author_update_column` has `name`. |
| no update permission, has insert | `author_update_column` is the `_PLACEHOLDER` enum (§6.7). `on_conflict` is **still offered** if the table has a PK or unique constraint (verified). |
| update permission with no updatable columns | `update_author` still exists, with only `where` (verified, §4.2). There are no `_set`/`_inc` input types, `author_update_column` is the placeholder, and `update_author_by_pk` exists if the PK is selectable, with only `pk_columns`. |
| `query_root_fields: [select]` | Only `author` on query_root. `subscription_root_fields` is independent. An empty list `[]` hides all root fields while keeping relationships through other tables. |
| `backend_only: true` on insert | `insert_author` and `insert_author_one` appear only for admin-secret requests with `x-hasura-use-backend-only-permissions: true`. |

Inherited roles combine permissions (doc 04). The schema is built from the combined permission. Columns that are allowed only in some parent roles are still exposed, but are **nullable**: the redaction semantics make them nullable even for NOT NULL columns. Verified: in `GraphQL/Schema/Select.hs` the column field uses `nullability = ciIsNullable columnInfo || redactionExp /= NoRedaction`. This applies to table object fields; aggregate fields are always nullable anyway.

---

## 17. Implementation notes for the Rust port

1. **Model names as `GqlIdentifier { prefix: Part, suffixes: Vec<Part> }`**, where each `Part` is `{ text, origin: Custom | Auto }`. Implement `to_snake`, `to_pascal` and `to_camel` exactly as `Data/Text/Casing.hs` does, including the leading-underscore rule. Unit-test against `naming_conventions` fixtures.
2. **Keep three type-name pipelines distinct:**
   - identifier, then case, then mkTypename (per source);
   - fixed `G.Name`, then `applyTypeNameCaseCust`, then (sometimes) mkTypename;
   - fixed name with no customization.

   The tables in §5.4 and §6.1 say which one applies to each type. Getting this wrong breaks multi-source schemas.
3. **Build the schema per role, per scenario (frontend/backend), per endpoint (graphql/relay).** Cache aggressively. Upstream memoizes parsers per (source, table, role).
4. **The introspection output generator** must:
   - sort everything as in §13.1;
   - deduplicate types with first-wins descriptions in the upstream DFS order (§13.2);
   - emit the quirky meta types (§13.4).
5. **Golden tests.**
   - For each fixture DB in `tests-py`, dump the full introspection JSON from upstream v2.x and compare exactly.
   - Record which upstream version was used (the DB-comment behaviour differs).
   - Strip nothing. Compare `description`, `defaultValue` and arg order.
6. **The error messages listed** in §10 are part of the API (they appear in metadata inconsistency output). Copy them verbatim from source.

---

## 18. Open questions / uncertainties

1. **DB COMMENT import.** RESOLVED as a fact: master (Oct 2026) removed it; released `v2.51.0` imports table, column and computed-field comments (not tracked-function comments). Still OPEN as a decision: which default the Rust port uses (§12.3 recommends the released behaviour).
2. **Meta-type shapes** (§13.4): RESOLVED from source. The field lists are transcribed in §13.4. A live `__type(name:"__Type")` query against a released server would still be a cheap confirmation.
3. **Namespace types under graphql-default.** RESOLVED from code (§5.4): the suffix is never re-cased; only the first character is upper-cased when a type-name prefix is present.
4. **graphql-default rendering of single-part names that contain `_`**, derived from code but not from a fixture:
   - aggregation-predicate enums: `...Bool_andArgumentsColumns`;
   - computed-field args types: `Full_nameAuthorArgs`;
   - the camelCase function args type `searchAuthorsArgs`.

   Each needs one upstream sample. The op type names are confirmed: `AuthorStddevPopFields`.
5. **Constraint description column order** (HashSet order) for multi-column constraints.
6. RESOLVED: `update_T` appears with zero operator inputs (only `where`), and `on_conflict` appears for a role with no update permission (placeholder enum). See §4.2 and §6.7.
7. **Version drift in operators.** The descriptions are verified against this tree. `_jsonb_path_exists` and `_jsonb_path_match` exist here but are absent in older releases, so pin the upstream comparison version.
8. **Composite-type columns**: naming RESOLVED (`<type>_scalar`). Input parsing of composite and unknown types remains open (doc 03).
9. **Inherited-role nullability**: RESOLVED (nullable when a redaction expression exists; §16).
10. **Stream `cursor` list element nullability**: RESOLVED, `[T_stream_cursor_input]!`.
11. **`no_queries_available` behaviour**: RESOLVED, it returns the placeholder string as data (§3).
12. **Ordering of `__schema.directives`**: RESOLVED for this tree (`include`, `skip`, `cached`; `_multiple_top_level_fields` is hidden; `@deprecated` is not advertised). Not checked against the `v2.51.0` sources.

---

## Upstream tests to reuse

- `server/tests-py/queries/graphql_introspection/` — introspection fixtures. Reuse all of them: `introspection*.yaml`, `descriptions/*` (exact description strings), `nullable_object_relationship/`, `custom_table_name/`, `disable_introspection/`.
- `server/tests-py/queries/graphql_introspection/remote_relationships/` — reuse once remote relationships exist (doc 06).
- `server/tests-py/test_graphql_introspection.py` — driver; port its assertions.
- `server/tests-py/queries/naming_conventions/*.yaml` — all 7 files; the exact graphql-default names, enum casing, prefix/suffix and precedence.
- `server/tests-py/test_naming_conventions.py` — includes `TestNamingConventionWithoutExperimentalFeature`. Reuse it, expecting the GA behaviour.
- `server/tests-py/queries/v1/metadata/pg_source_customization*.yaml`, `pg_source_namespace_query.yaml`, `pg_source_prefix_query.yaml` — source customization.
- `server/tests-py/queries/graphql_query/` — query shapes, args, enums, computed fields and functions. These are schema and execution tests; shared with doc 03.
- `server/tests-py/queries/graphql_mutation/` — insert, update and delete input shapes; on_conflict; presets.
- `server/tests-py/queries/subscriptions/streaming/` (if present) — `_stream` arguments.
- `server/tests-py/queries/graphql_query/relay/` and `test_graphql_queries.py::TestRelay*` — Relay schema.
- `server/lib/api-tests/src/Test/NamingConventionsSpec.hs` — naming conventions, Postgres part.
- `server/lib/api-tests/src/Test/Schema/` — all of it: `EnumsSpec.hs`, `CustomFieldNames/*`, `ComputedFields/*`, `ConflictsSpec.hs`, `ViewsSpec.hs`, the table/column comment specs. **Skip** the non-Postgres backend variants (BigQuery, MSSQL, Citus, Cockroach, DataConnector).
- `server/lib/api-tests/src/Test/Queries/` — Postgres variants only (aggregation predicates, nested objects, NativeQueries/LogicalModels as P2).
- `server/lib/api-tests/src/Test/Mutations/` — Postgres variants only.
- `server/lib/api-tests/src/Test/Subscriptions/StreamingSubscriptionsSpec.hs` — stream schema.
- `server/lib/api-tests/src/Test/PortedFromPytest/` — reuse where the module targets Postgres.
- `server/lib/schema-parsers/test/` — unit tests for parsers, collection and sorting. Port them as Rust unit tests of the introspection layer.
- `server/src-test/Hasura/GraphQL/Schema/*Spec.hs` (e.g. `BoolExp`, `Select`, `RemoteSchema`) — reuse the Postgres-related cases as unit tests.
- `server/lib/hasura-extras/test/` (casing tests, if present) — port for `GqlIdentifier`.
- **Skip:** `tests-py/queries/graphql_query/*mssql*`, `*bigquery*`, `*citus*`, and all `DataConnector`/`hasura-ndc` suites. They are out of scope for Postgres schema generation.
