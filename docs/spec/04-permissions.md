# 04 - Permissions (authorization)

Compatibility spec for Hasura GraphQL Engine V2 authorization. It covers the Postgres backend first. Ground truth is the Haskell source under `server/`. Docs live under `docs/docs/auth/authorization/` and `docs/docs/schema/postgres/`. Where the docs and the source disagree, this spec follows the source and says so. Statements marked "(inferred)" were not verified line by line.

How authentication produces a role and session variables is covered in `05-authentication.md`. This document starts from a resolved `UserInfo`:

```
UserInfo {
  role: RoleName,                      -- never empty; "admin" is special
  session_variables: Map<CI "x-hasura-*", Text>,   -- includes x-hasura-role
  backend_only_field_access: Allowed | Disallowed
}
```

Primary source files, relative to the repo root:

- `server/src-lib/Hasura/RQL/Types/Permission.hs`: metadata types and JSON (de)serialisation.
- `server/src-lib/Hasura/RQL/DDL/Permission.hs`, `server/src-lib/Hasura/RQL/DDL/Permission/Internal.hs`: metadata API and validation.
- `server/src-lib/Hasura/Backends/Postgres/DDL/BoolExp.hs`, `server/src-lib/Hasura/Backends/Postgres/DDL.hs`: parsing boolean expressions in permissions.
- `server/src-lib/Hasura/Backends/Postgres/Translate/BoolExp.hs`: bool exp to SQL.
- `server/src-lib/Hasura/Backends/Postgres/Execute/Prepare.hs`: session variable to SQL.
- `server/src-lib/Hasura/Table/Cache.hs`: `SelPermInfo`, `InsPermInfo`, `UpdPermInfo`, `DelPermInfo`, `RolePermInfo`, `getRolePermInfo`.
- `server/src-lib/Hasura/RQL/DDL/Schema/Cache/Permission.hs`, `server/src-lib/Hasura/RQL/Types/Roles/Internal.hs`, `server/src-lib/Hasura/RQL/DDL/InheritedRoles.hs`: inherited roles.
- `server/src-lib/Hasura/GraphQL/Schema.hs`, `server/src-lib/Hasura/GraphQL/Schema/{Build,Table,Select,Mutation,Update,Common}.hs`: per-role schema.
- `server/src-lib/Hasura/Backends/Postgres/Execute/{Mutation,Insert}.hs`: check constraints and `validate_input`.
- `server/src-lib/Hasura/RemoteSchema/{Metadata,MetadataAPI,SchemaCache}/Permission.hs`: remote schema permissions.
- `server/src-lib/Hasura/Function/API.hs`: function permissions.
- `server/src-lib/Hasura/RQL/DDL/Action.hs`: action permissions.

---

## Scope and priority

| Feature | Priority | Edition |
|---|---|---|
| Admin role bypass (role `admin` sees everything) | Essential | OSS |
| select permission: `columns`, `filter`, `limit`, `allow_aggregations`, `computed_fields` | Essential | OSS |
| insert permission: `check`, `columns`, `set` | Essential | OSS |
| update permission: `columns`, `filter`, `check`, `set` | Essential | OSS |
| delete permission: `filter` | Essential | OSS |
| Bool exp operators, relationship traversal, `_and/_or/_not`, `_exists` | Essential | OSS |
| Session variables in filters, presets and `_in` with arrays | Essential | OSS |
| Missing session variable error | Essential | OSS |
| Per-role GraphQL schema generation | Essential | OSS |
| Column comparison operators (`_ceq` etc.) and the `$` root reference | Common | OSS |
| `query_root_fields` / `subscription_root_fields` | Common | OSS |
| `backend_only` with the `x-hasura-use-backend-only-permissions` header | Common | OSS |
| Inherited roles: select union with column redaction | Common | OSS (since v2.0; the docs mark it GA) |
| Inherited roles for insert/update/delete (all parents must agree) | Rare | OSS |
| Function permissions and `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS` | Common | OSS |
| Action permissions | Common | OSS |
| Remote schema permissions (`@preset`) | Rare | OSS (disabled by default) |
| `validate_input` (HTTP webhook before mutations) | Rare | OSS, v2.29+, Postgres only |
| Remote source relationships inside permission predicates | Rare | OSS |
| `comment` on permissions, `pg_set_permission_comment` | Rare | OSS |
| Logical model / native query permissions | Rare | OSS, out of scope here |
| Data-connector backends, MSSQL, BigQuery permission differences | Skip | n/a |
| Row-level permissions on "tables via API limits" (`api_limits` per role) | Not covered here | EE for some limits; see config spec |

Build order suggestion: admin bypass, select permission, missing-variable error, insert/update/delete, schema shaping, backend_only, inherited roles, function/action/remote permissions, validate_input.

---

## 1. The admin role

- The role name `admin` (exactly `admin`, case-sensitive; `adminRoleName` in `Hasura/Authentication/Role.hs`) has full access everywhere.
- `getRolePermInfo` (`Hasura/Table/Cache.hs`) returns `_tiAdminRolePermInfo` for `admin`. This is a synthesised `RolePermInfo` with:
  - all columns and computed fields selectable, with no filter and no limit;
  - aggregations allowed;
  - all insertable columns insertable, all updatable columns updatable;
  - delete allowed;
  - every check and filter set to `BoolAnd []` (true);
  - `backend_only = false`.
- Admin sees every query, mutation and subscription root field. It also sees every function, action and remote schema (whole schema, even when remote schema permissions are enabled).
- Defining a permission for `admin` in metadata fails:
  - `pg_create_*_permission` with role `admin` gives 400 `already-exists`, message `<type> permission already defined on table "<table>" with role "admin"`. The check is `role == adminRoleName` in `runCreatePerm`.
  - Metadata containing an `admin` permission (e.g. via `replace_metadata`) produces an inconsistency: `cannot define permission for admin role`, code `constraint-violation` (`Hasura/RQL/DDL/Schema/Cache/Permission.hs`).
- Any JWT, webhook or header flow that yields role `admin` gets admin access. That includes a JWT whose `x-hasura-allowed-roles` contains `admin`. This is the upstream behaviour and must be kept.
- Admin-only HTTP endpoints check `role == admin` (see 05-authentication.md section 9).

The `<table>` text in error messages comes from `qualifiedObjectToText` (`server/src-lib/Hasura/Backends/Postgres/SQL/Types.hs`):

- For tables in the `public` schema it is just the table name (`article`).
- Otherwise it is `schema.table` (`hge_tests.article`).
- `<<>` / `<>>` wrap the value in double quotes.

---

## 2. Permission metadata format

### 2.1 Where permissions live

Permissions live in the table metadata under `insert_permissions`, `select_permissions`, `update_permissions` and `delete_permissions`. Each is a list of `PermDef`:

```yaml
- table: {schema: public, name: article}
  select_permissions:
    - role: user
      permission:
        columns: [id, title, author_id]
        filter: {author_id: {_eq: X-Hasura-User-Id}}
        limit: 10
        allow_aggregations: true
      comment: "optional"
```

`PermDef` JSON fields (`Hasura/RQL/Types/Permission.hs`):

- `role` (required, non-empty `RoleName`).
- `permission` (required, the type-specific object).
- `comment` (optional `Text`; omitted from output when null).

### 2.2 Insert permission (`InsPerm`)

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `check` | BoolExp | yes | n/a | Row check, evaluated on inserted rows. |
| `set` | object column -> value | no | `{}` | Column presets. |
| `columns` | `"*"` or `[col]` | no | all insertable columns (absent is treated like `"*"`: `fromMaybe PCStar`) | Columns the role may provide. |
| `backend_only` | bool | no | `false` | |
| `validate_input` | ValidateInput | no | absent | See section 10. |

### 2.3 Select permission (`SelPerm`)

| Field | Type | Required | Default |
|---|---|---|---|
| `columns` | `"*"` or `[col]` | yes | n/a |
| `filter` | BoolExp | yes | n/a |
| `limit` | non-negative int | no | none |
| `allow_aggregations` | bool | no | `false` |
| `computed_fields` | `[name]` | no | `[]` |
| `query_root_fields` | `[select, select_by_pk, select_aggregate]` | no | absent = all allowed |
| `subscription_root_fields` | `[select, select_by_pk, select_aggregate, select_stream]` | no | absent = all allowed |

Notes on the select fields:

- `columns: "*"` is serialised back as `"*"`. A list is a list of column names; relationships are not allowed in it.
- Root-field values are serialised exactly as `select`, `select_by_pk`, `select_aggregate` and `select_stream`.
- Unknown values fail JSON parsing.
- An empty list `[]` means no root fields of that kind: the table is reachable only through relationships.

### 2.4 Update permission (`UpdPerm`)

| Field | Type | Required | Default |
|---|---|---|---|
| `columns` | `"*"` or `[col]` | yes | n/a |
| `filter` | BoolExp | yes | n/a |
| `check` | BoolExp | no | null (no post-check) |
| `set` | object | no | `{}` |
| `backend_only` | bool | no | `false` |
| `validate_input` | ValidateInput | no | absent |

When `check` is absent it is still emitted as `"check": null` when serialising (from source).

### 2.5 Delete permission (`DelPerm`)

| Field | Type | Required | Default |
|---|---|---|---|
| `filter` | BoolExp | yes | n/a |
| `backend_only` | bool | no | `false` |
| `validate_input` | ValidateInput | no | absent |

### 2.6 `validate_input`

```json
{
  "type": "http",
  "definition": {
    "url": "http://validator/insert",
    "headers": [{"name": "X-Secret", "value": "v"}, {"name": "X-From-Env", "value_from_env": "ENV"}],
    "timeout": 10,
    "forward_client_headers": false
  }
}
```

- The only accepted `type` is `"http"`. Any other value fails to parse with `expecting only 'http' for 'type' but got <type>`.
- `url` may be a string or `{"from_env": "ENV"}` (inferred: same `InputWebhook` type as actions).
- Defaults: `headers` = `[]`, `timeout` = 10 (seconds), `forward_client_headers` = `false`.

### 2.7 Metadata API commands (`/v1/metadata`)

Payload `args`:

| Command | args |
|---|---|
| `pg_create_insert_permission` | `{source?: "default", table, role, permission: InsPerm, comment?}` |
| `pg_create_select_permission` | `{source?, table, role, permission: SelPerm, comment?}` |
| `pg_create_update_permission` | `{source?, table, role, permission: UpdPerm, comment?}` |
| `pg_create_delete_permission` | `{source?, table, role, permission: DelPerm, comment?}` |
| `pg_drop_{insert,select,update,delete}_permission` | `{source?, table, role}` |
| `pg_set_permission_comment` | `{source?, table, role, type: insert/select/update/delete, comment?}` |

The legacy `/v1/query` names (`create_select_permission` etc.) exist without the `pg_` prefix and imply source `default`.

`table` is a table name string (schema `public`) or `{schema, name}`. All of these commands need the admin role.

Errors:

| Situation | HTTP | code | message |
|---|---|---|---|
| Create when one already exists (or role is `admin`) | 400 | `already-exists` | `select permission already defined on table "article" with role "user"` |
| Drop non-existent | 400 | `permission-denied` | `select permission on "article" for role "user" does not exist` |
| Set comment on non-existent | 400 | `permission-denied` | `'select' permission on "article" for role "user" does not exist` (`assertPermDefined`, `RQL/DDL/Permission/Internal.hs`; checks the built role permission map, so an inherited-role-derived permission counts as existing) |
| `set` references unknown column | 400 | `not-exists` (inferred) | `column "foo" not found in table "article"` |
| Insert `columns` contains a non-insertable column (e.g. generated) | 500 | `unexpected` | `Column "c" is not insertable and so cannot have insert permissions defined` |
| Insert or update `columns` contains a relationship or other non-column | 400 | (inferred `not-exists`/`validation-failed`) | `Only table columns can have insert permissions defined, not relationships or other field types` (the update text says "update") |
| Update `columns` contains a non-updatable column | 500 | `unexpected` | `Column "c" is not updatable and so cannot have update permissions defined` |
| Select `columns` contains a relationship | 400 | | `permissions for relationships are automatically inferred` |
| Select `computed_fields` names a table-returning computed field | 400 | `not-supported` | `select permissions on computed field "x" are auto-derived from the permissions on its returning table "t" and cannot be specified manually` |
| `limit` negative | 400 | `not-supported` | `unexpected negative value` |
| `select_by_pk` in root fields but PK not fully selectable (or no PK) | 400 | `validation-failed` | `The "select_by_pk" field cannot be included in the query_root_fields or subscription_root_fields because the role "user" does not have access to the primary key of the table "article" in the source "default"` |
| `select_aggregate` in root fields but `allow_aggregations` false | 400 | `validation-failed` | `The "select_aggregate" root field can only be enabled in the query_root_fields or  the subscription_root_fields when "allow_aggregations" is set to true` (the double space is in the source) |

Create and drop go through `buildSchemaCacheFor`. Any new inconsistency caused by the permission, such as an unknown column in a filter, is returned as a 400 error. For details see the metadata spec (inconsistent-object handling).

Column, relationship and computed-field references in permissions create schema dependencies. Dropping a referenced column cascades or errors per the dependency rules (regression test `DropColumnWithPermissions8415Spec`).

---

## 3. Boolean expressions in permissions

### 3.1 Grammar

The grammar is in `Hasura/RQL/IR/BoolExp.hs` (parse) and `Hasura/Backends/Postgres/DDL/BoolExp.hs`:

```
BoolExp       := { key: value, ... }                  -- multiple keys are ANDed
key           := "_and" | "$and"   -> [BoolExp]       -- AND of list, [] = true
               | "_or"  | "$or"    -> [BoolExp]       -- OR of list,  [] = false
               | "_not" | "$not"   -> BoolExp
               | "_exists"         -> {"_table": TableName, "_where": BoolExp}
               | <column>          -> ColumnExp
               | <relationship>    -> BoolExp (evaluated against the target table)
               | <computed field>  -> ColumnExp (scalar CF) | BoolExp (table CF)
               | <remote source relationship> -> restricted, see 3.8
ColumnExp     := { op: value, ... }   -- multiple ops ANDed
               | <non-object value>   -- shorthand for {_eq: value}
```

Every operator also accepts a `$` prefix in place of `_`, e.g. `$eq`, `$in`, `$gt`.

An empty object `{}` is true. This is the usual "no restriction" filter.

### 3.2 Postgres operators (`parseBoolExpOperations` in `Backends/Postgres/DDL/BoolExp.hs`)

| Operator(s) | RHS | Applies to | SQL |
|---|---|---|---|
| `_eq`, `$eq` | scalar | any | `=` (see null-safety below) |
| `_ne`, `_neq`, `$ne`, `$neq` | scalar | any | `<>` |
| `_in`, `$in` | array or session var | any | `= ANY(...)` / `IN` |
| `_nin`, `$nin` | array or session var | any | `NOT IN` / `<> ALL` |
| `_gt`, `_lt`, `_gte`, `_lte` (and `$`) | scalar | any | `>`, `<`, `>=`, `<=` |
| `_like`, `_nlike`, `_ilike`, `_nilike`, `_similar`, `_nsimilar`, `_regex`, `_iregex`, `_nregex`, `_niregex` | text | string types | `LIKE`, `NOT LIKE`, `ILIKE`, `NOT ILIKE`, `SIMILAR TO`, `NOT SIMILAR TO`, `~`, `~*`, `!~`, `!~*` |
| `_is_null` | bool | any | `IS NULL` / `IS NOT NULL` |
| `_contains`, `_contained_in` | jsonb or array | jsonb, arrays | `@>`, `<@` |
| `_has_key` | text | jsonb | `?` |
| `_has_keys_any`, `_has_keys_all` | text[] | jsonb | `?|`, `?&` |
| `_jsonb_path_exists`, `_jsonb_path_match` | jsonpath | jsonb | `jsonb_path_exists`, `@@` (inferred) |
| `_st_contains`, `_st_crosses`, `_st_equals`, `_st_intersects`, `_st_overlaps`, `_st_touches`, `_st_within`, `_st_d_within`, `_st_3d_d_within`, `_st_3d_intersects` (exactly this list; the raster operators `_st_intersects_rast` etc. exist only in GraphQL `where`, not in permissions) | geometry/geography | PostGIS | `ST_*` functions |
| `_ceq`, `_cne`/`_cneq`, `_cgt`, `_clt`, `_cgte`, `_clte` | column reference | any | column-to-column comparison |
| `_is_null`, `$is_null`, `_contains`/`$contains`, `_has_key`/`$has_key`, ... | | | every operator in this table has a `$` twin (`parseBoolExpOperations`) |
| `_ancestor`, `_ancestor_any`, `_descendant`, `_descendant_any`, `_matches`, `_matches_any`, `_matches_fulltext` | ltree/lquery/ltxtquery | ltree | `@>`, `<@`, `~`, `?`, `@` |
| `_cast` | `{ "<target type>": ColumnExp }` | geometry to geography and back, jsonb to text (inferred list) | `CAST` |

Errors:

| Situation | Message |
|---|---|
| Unknown operator | code `unexpected-payload`: `Unknown operator: <op>` |
| `json` (not `jsonb`) column used in an expression | code `unexpected-payload`: `JSON column can not be part of boolean expression` |
| Computed field with input arguments | `Computed columns with input arguments can not be part of the where clause` |
| Column operator RHS not a string or `["$", col]` | code `not-supported`: `Relationship references are not supported in column comparison RHS` |
| Column operator types mismatch | `incompatible column types: ...` |
| Unknown field | (inferred) `no such column exists: "x"` / `field "x" not found in table "t"` (see metadata spec for the exact text) |

### 3.3 Value parsing and session variables

The RHS of every scalar operator is parsed by `parseCollectableType` (`Backends/Postgres/DDL.hs`):

- **JSON string recognised as a session variable.** A string is treated as a session variable when, case-insensitively, it starts with `x-hasura-`. (`isSessionVariable`, `Hasura/Authentication/Session.hs`.) The string `"REQ_USER_ID"` (compared case-insensitively, `isReqUserId` in `Hasura/Server/Utils.hs`) is a legacy alias for `x-hasura-user-id`. It is accepted everywhere `parseCollectableType` is used: filters, checks and `set` presets alike (not only presets). Both become `PSESessVar (type, var)`.
- **Any other value.** The value is parsed as a literal of the column's type. A JSON value of the wrong type fails at metadata time with a parse error naming the column type (exact text: see the SQL spec, `parseScalarValueColumnType`).
- **`_in` / `_nin`.** The collectable type is `CollectableTypeArray colType`:
  - A JSON array is converted to `ARRAY[v1, v2, ...]::coltype[]` (literal values).
  - A session-variable string is accepted. At run time the session variable value must be a Postgres array literal, e.g. `X-Hasura-Allowed-Ids: {1,2,3}`. It is cast as `(... )::integer[]`.
- **`_has_keys_any`, `_has_keys_all`.** These take `text[]` the same way. A session var with value `{display,memory}` works (tests: `developer_insert_has_keys_any_*`).
- **Postgres array columns.** The literal must be a JSON array, or a session var holding a Postgres array literal.

### 3.4 Session variable SQL

From `Backends/Postgres/Execute/Prepare.hs` and `Backends/Postgres/SQL/Value.hs`:

- The session variables object is sent to Postgres as prepared argument `$1` of type `json`. Its keys are the lowercased session variable names, e.g. `{"x-hasura-role":"user","x-hasura-user-id":"1"}`.
- A reference in a filter becomes:

  ```sql
  (($1->>'x-hasura-user-id'))::integer
  ```

  - The key is the lowercased variable name.
  - The cast target is the column's Postgres type. For an array collectable it is `type[]`.
  - For PostGIS geometry/geography the value is wrapped as `ST_GeomFromGeoJSON(($1->>'x-hasura-geom'))::geometry` (the value must be GeoJSON text); for `raster` it is `ST_RastFromHexWKB(...)` (`withConstructorFn`, `Backends/Postgres/SQL/Value.hs`). Arrays get no constructor function.
- Some code paths substitute the literal session value in place of the prepared argument: the non-prepared "no plan" path, used for subscription cohorts (multiplexed) and when no plan is passed. In the multiplexed live query the session JSON is instead a column of the cohort variables (`_subs.result_vars#>>'{session,x-hasura-user-id}'`; see the subscriptions spec).
- The cast is done by Postgres. A value that does not cast (e.g. `X-Hasura-User-Id: abc` for an integer) fails with a Postgres error:
  - HTTP 200 on `/v1/graphql`.
  - code `data-exception`.
  - message = the Postgres message, e.g. `invalid input syntax for type integer: "abc"`.
- Session variable values are never truncated (regression `DoNotTruncateSessionVariables8158Spec`).

**Missing session variable.** If a permission (filter, check, or `set` preset) references a session variable that the request does not have, the query fails before SQL is executed:

```json
{"errors":[{"extensions":{"path":"$","code":"not-found"},
            "message":"missing session variable: \"x-hasura-critic-id\""}]}
```

- HTTP 200 on `/v1/graphql`; the real status is 400 (`throw400 NotFound`).
- The variable name is lowercased inside double quotes.
- Fixtures: `server/tests-py/queries/graphql_query/permissions/select_articles_without_required_headers.yaml`, `artist_select_query_Track_fail.yaml`, `queries/graphql_mutation/insert/permissions/leads_upsert_check_with_headers.yaml`.
- The check is lazy on `/v1/graphql`. It fires only when a permission that is actually used by the query needs the variable: each `UVSessionVar` is looked up when the SQL is prepared (`prepareWithPlan` / `prepareWithoutPlan` in `Backends/Postgres/Execute/Prepare.hs`), so the error names the first missing variable reached.
- The "required headers" sets (`spiRequiredHeaders` = session vars of the select `filter`; `ipiRequiredHeaders` = insert `check` + `set`; `upiRequiredHeaders` = update `filter` + `set`; `dpiRequiredHeaders` = delete `filter`) are used only by the legacy `/v1/query` DML (`validateHeaders`, `RQL/DML/Internal.hs`), which checks them up front with a different message: 400 `not-found`, `"x-hasura-user-id" header is expected but not found`.

### 3.5 Null-safe equality in permissions

In permission predicates (not user `where` arguments):

- `_eq` against a nullable column (or a column whose nullability is unknown) is compiled null-safely (`equalsBoolExpBuilder`, `Backends/Postgres/Translate/BoolExp.hs`):

  ```sql
  (("col" = v) OR (("col" IS NULL) AND (v IS NULL)))
  ```

- `_neq` / `_ne` on a nullable column (`notEqualsBoolExpBuilder`):

  ```sql
  (("col" <> v) OR (("col" IS NOT NULL) AND (v IS NULL)))
  ```

- For non-nullable columns plain `=` / `<>` is used.
- The bare-value shorthand (`{"id": "X-Hasura-User-Id"}`) is ALWAYS compiled null-safely, whatever the column nullability (`AEQ NullableComparison` in `parseOperations`).
- Test: `server/lib/api-tests/src/Test/Queries/PermissionsKnowAboutColumnNullabilitySpec.hs` (inferred name; see the tests section). Regression `OptimizePermissionFiltersSpec` checks that redundant null checks are not generated for non-nullable columns.

### 3.6 Relationship traversal

`{author: {id: {_eq: X-Hasura-User-Id}}}` on table `article`, where `author` is an object relationship to `author`, becomes:

```sql
EXISTS (
  SELECT 1
  FROM "public"."author" AS "__be_0_public_author"
  WHERE (("__be_0_public_author"."id") = ("public"."article"."author_id"))
    AND ((("__be_0_public_author"."id") = (($1->>'x-hasura-user-id'))::integer))
)
```

- Array relationships use the same `EXISTS`, so "any related row matches". The alias naming is backend-internal; it only needs to be unique.
- A relationship filter does NOT apply the target table's own select permission. Only the given expression is used, which is a common surprise. (Upstream: `AVRelationship` carries the user's bool exp only. In GraphQL `where` arguments the target's select filter IS ANDed in; see the SQL spec.)
- Manual relationships and relationships via a foreign key behave the same. Relationships over remote sources are handled in 3.8.

### 3.7 `_exists`

```json
{"_exists": {"_table": {"schema": "public", "name": "user_flags"},
             "_where": {"user_id": {"_eq": "X-Hasura-User-Id"}, "active": {"_eq": true}}}}
```

- Compiles to `EXISTS (SELECT 1 FROM "public"."user_flags" AS "_exists_table_N" WHERE <where>)` (the alias is upstream-internal).
- There is no join condition: the target table need not be related.
- `_where` may contain session variables, relationships of the `_table`, and column comparison operators.
- Inside `_where`, `["$", "col"]` in a column operator refers to the root table: the table the permission is defined on.

### 3.8 Column comparison operators and the root reference

- `{"price": {"_cgt": "cost"}}` compares two columns of the current table: the table the expression is evaluated against, which is the relationship target when nested.
- `{"owner_id": {"_ceq": ["$", "id"]}}` inside a relationship or `_exists` refers to column `id` of the root table of the permission.
- Test: `server/lib/api-tests/src/Test/Queries/Relationship/PermissionRootReferenceSpec.hs` (inferred path under `Test/Queries/...`).

### 3.9 Remote source relationships in permissions

- Remote source relationships (Postgres-to-Postgres across sources) may appear in permission filters only in this form (`RQL/IR/BoolExp/RemoteRelationshipPredicate.hs`, `RQL/DDL/Permission/Internal.hs`):
  - The join is single-column (else 400 `unexpected-payload` `remote field mapping should have exactly one column`); computed-field joins and remote SCHEMA relationships are rejected (`remote field mapping should not have computed field`, `remote schema field unsupported`).
  - The nested expression is an object with exactly one key, a column of the target: `{"<remote col>": {<op>: <value>, ...}}`. Otherwise: `Expected the remote relationship expression predicate to be {"<remote column name>":{"_eq":"<some session variable name>"}}`.
  - Supported ops (no `$` aliases): `_eq`, `_neq`, `_gt`, `_lt`, `_gte`, `_lte`, `_in`, `_nin`, `_like`, `_nlike`, `_is_null`. Others: `Expected one of the supported operators (_eq, _neq, _gt, _lt, _gte, _lte, _in, _nin, _like, _nlike and _is_null)`. Values are session variables or literals.
- Execution:
  1. Before running the main query, the engine queries the remote source for the values.
  2. It then emits `lhs_col IN (v1, v2, ...)`, or `lhs_col IN (NULL)` when no values are found (inferred SQL shape).
- Test: `server/lib/api-tests/src/Test/Databases/Postgres/DataValidation/RemoteSourcePermissionPredicateSpec.hs` (path inferred). This is low priority.

### 3.10 Computed fields in permissions

- A scalar computed field (function returning a base type) without extra input arguments may be used like a column: `{"full_name": {"_ilike": "%a%"}}` becomes `"public"."author_full_name"("public"."author")`.
- A function that takes the session argument gets the session JSON.
- Table-returning computed fields are used like an array relationship (`EXISTS` over the function call).

---

## 4. Insert permissions semantics

### 4.1 Column presets (`set`)

```yaml
set:
  author_id: X-Hasura-User-Id
  status: draft
```

- Processing is in `procSetObj` (`RQL/DDL/Permission/Internal.hs`). Each value is parsed with the column type. A string starting with `x-hasura-` (case-insensitive), or the legacy `REQ_USER_ID` (case-insensitive; maps to `x-hasura-user-id`), becomes a session variable reference.
- Unknown column error: `column "foo" not found in table "article"`.
- Preset columns are removed from the role's insertable column set, so they do not appear in `<table>_insert_input`. Preset value wins: the user cannot supply it.
- Session-var presets are resolved at run time. A missing variable gives the missing session variable error (3.4).
- Preset values are injected into the `INSERT` column list for every row.

### 4.2 Effective insertable columns

`ipiCols` = (`columns` or all insertable columns) minus preset columns. Generated columns and identity columns `GENERATED ALWAYS` are not insertable.

Columns not in this set do not exist in `<table>_insert_input`, so supplying one is a GraphQL validation error: `field 'x' not found in type: 'article_insert_input'` (see the schema spec for the text).

### 4.3 Check

The `check` predicate is evaluated on each inserted row after insertion, in the same statement:

```sql
WITH "article__mutation_result_alias" AS (
  INSERT INTO "public"."article" ("title", "author_id")
  VALUES ($2, (($1->>'x-hasura-user-id'))::integer)
  RETURNING *,
    (<check expression on "public"."article">)::boolean AS "check__constraint"
  -- for upserts: CASE WHEN (xmax = 0) THEN (<insert check>) ELSE (<update check>) END
)
SELECT json_build_object('affected_rows', ..., 'returning', ...),
       (SELECT bool_and("check__constraint") FROM "article__mutation_result_alias")
```

- The upstream shape is in `Backends/Postgres/Translate/Insert.hs` and `Execute/Mutation.hs`. Here `$1` is the session JSON.
- If the aggregated check is not true (false, or NULL when it errors), the whole transaction is rolled back with:

  ```json
  {"errors":[{"extensions":{"path":"$.selectionSet.insert_article.args.objects","code":"permission-error"},
              "message":"check constraint of an insert/update permission has failed"}]}
  ```

  - The real status is 400; it is 200 on `/v1/graphql`.
  - Path: `$.selectionSet.<field>.args.objects` for insert (fixtures in `server/tests-py/queries/graphql_mutation/insert/permissions/`). For `insert_x_one` it is (inferred) `$.selectionSet.insert_x_one.args.object`.
- Relationships in the check are evaluated against the database state after the insert. This is what makes nested inserts with `check: {author: {...}}` work.
- A Postgres `CHECK` constraint violation (SQLSTATE 23514) maps to code `permission-error`, with message `Check constraint violation. <pg message>` (`Execute/Types.hs`).

### 4.4 Upserts (`on_conflict`)

- `on_conflict` is exposed only if the table has unique or primary key constraints and the role has an update permission. Upstream: `Backends/Postgres/Schema/OnConflict.hs` (inferred: requires update permission; DO NOTHING may be allowed without it, see the schema spec).
- `update_columns` is an enum `<table>_update_column` of updatable, non-preset columns. If none exist a placeholder enum value `_PLACEHOLDER` is used, and only `update_columns: []` (DO NOTHING) is meaningful.
- The ON CONFLICT DO UPDATE `WHERE` is the update permission filter ANDed with the user's `on_conflict.where`.
- Update presets (`set` of the update permission) are applied in the DO UPDATE SET.
- The check: inserted rows (`xmax = 0`) use the insert check. Updated rows use the update permission `check` if set, else true (inferred).

### 4.5 Nested inserts

- Inserting through a relationship requires an insert permission on the target table for the same role. Otherwise the relationship field is absent from `<table>_insert_input`.
- Each table's preset and check apply to its own rows.

---

## 5. Select permissions semantics

### 5.1 Row filter

```sql
SELECT ... FROM "public"."article"
WHERE (<permission filter>) AND (<user where>)
```

- The permission filter is applied to every select: root fields, relationships, aggregates, `_by_pk` and subscriptions. It is NOT applied to the rows of a mutation's `returning` / single-row output.
- Mutation output (`returning` of `insert_x`/`update_x`/`delete_x`, and the object returned by `insert_x_one`/`update_x_by_pk`/`delete_x_by_pk`):
  - Requires a select permission to exist (otherwise `returning` / the `_one`/`_by_pk` field is absent), and only the select-permitted columns appear (with inherited-role redaction CASEs).
  - The rows themselves are every row the mutation touched: the SQL builds the output select over the mutation CTE with `TablePerm annBoolExpTrue Nothing`, i.e. no row filter and no limit (`mkMutationOutputExp`, `Backends/Postgres/Translate/Returning.hs`; `mutationSelectionSet` in `GraphQL/Schema/Mutation.hs` only checks that a select permission exists). So a role can see a row in `returning` that its select filter would hide.
  - Relationships selected inside `returning` DO apply the target table's select filter (normal relationship selection).
- Relationships traversed in a query apply the target table's select filter in the joined subquery.

### 5.2 Columns

- Only permitted columns appear in the object type, the `bool_exp`, `order_by`, `select_column` enum and aggregate fields.
- With `columns: "*"`, all columns are allowed, including columns added later. `"*"` is resolved at schema-build time.

### 5.3 Limit

- The effective limit is `min(user limit, permission limit)`. With only one present, that one applies.
- Aggregate queries:
  - If the aggregate query selects aggregate fields or uses an aggregate order-by, the permission limit applies to `nodes` only: aggregates see every permitted row (within the user's own `limit`). Checked on v2.51.0-ce: with `limit: 2` over 5 rows, `count` is 5 and `nodes` has 2 rows; with a user `limit: 3`, `count` is 3.
  - Otherwise it applies to the rows, as for a plain select.
  - Upstream: `Translate/Select/Internal/Process.hs` (`PermissionLimitSubQuery`).
- The limit also applies to array relationships of that table.

### 5.4 Aggregations

- `allow_aggregations: false` (the default) hides `<table>_aggregate` root fields.
- It also hides `<rel>_aggregate` relationship fields that point to the table, and aggregation predicates (`where: {<rel>_aggregate: {count: ...}}`) on relationships targeting the table.

### 5.5 Computed fields

- Scalar computed fields must be listed in `computed_fields` to be selectable.
- Table-returning computed fields are visible when the role has a select permission on the returned table. That table's filter applies to the result.

### 5.6 Root field visibility (`query_root_fields`, `subscription_root_fields`)

Absent means all of:

- query: `select`, `select_by_pk`, `select_aggregate`
- subscription: `select`, `select_by_pk`, `select_aggregate`, `select_stream`

When listed, only those root fields are generated. Relationship access is unaffected.

`select_by_pk` additionally requires all PK columns to be selectable. `select_aggregate` additionally requires `allow_aggregations: true`. Both are enforced at metadata time (2.7) and at schema build.

`select_stream` also requires streaming subscriptions to be enabled. They are on by default since v2.x (inferred; see the subscriptions spec).

When combining permissions (inherited roles): any side "all allowed" gives all allowed; two explicit sets give their union.

Tests: `server/lib/api-tests/src/Test/Auth/Authorization/DisableRootFields/*`.

---

## 6. Update permissions semantics

The SQL shape for update is:

```sql
WITH "article__mutation_result_alias" AS (
  UPDATE "public"."article"
  SET "title" = $2, "updated_by" = (($1->>'x-hasura-user-id'))::integer   -- presets
  WHERE (<update permission filter>) AND (<user where>)
  RETURNING *, (<update check>)::boolean AS "check__constraint"
)
...
```

- Rows not matching the filter are silently not updated (`affected_rows` reflects that). This is not an error.
- `check` (post-update check) runs on the new row values. If it fails, the error is the same as for insert: `permission-error`, `check constraint of an insert/update permission has failed`. When `check` is absent the check column is `true`.
- `set` presets are always applied and removed from the updatable set.
- Operators (`_set`, `_inc`, `_append`, `_prepend`, `_delete_key`, `_delete_elem`, `_delete_at_path`) list only updatable non-preset columns of suitable types.
  - A column may not appear in two operators. Error: `Column found in multiple operators; <col>` (inferred exact suffix; upstream `GraphQL/Schema/Update.hs`).
  - `update_many` (`update_<table>_many`) uses the same permission for every entry.
- `update_<table>_by_pk` requires all PK columns to be selectable (select permission) and a select permission at all, since it returns the row.
- `update_<table>` requires an update permission and, for the table, `viIsUpdatable`.

---

## 7. Delete permissions semantics

```sql
DELETE FROM "public"."article" WHERE (<delete filter>) AND (<user where>) RETURNING *
```

- `delete_<table>` exists if the role has a delete permission. Upstream checks `viIsInsertable` for views here, which is a quirk; reproduce it (inferred harmless for tables).
- `delete_<table>_by_pk` additionally requires a select permission with all PK columns selectable.

---

## 8. Backend-only permissions

- `backend_only: true` on an insert, update or delete permission means the mutation is generated only in the role's backend schema.
- Each role has two contexts: `frontend` and `backend`.
  - frontend: all fields except backend-only mutations.
  - backend: all fields including backend-only mutations.
- The backend context is used when the request's `UserInfo.backend_only_field_access == Allowed`. That holds when:
  1. the request was authenticated by the admin secret (header `x-hasura-admin-secret` matched), or auth is not configured at all (no admin secret); AND
  2. the header `x-hasura-use-backend-only-permissions` parses as true.
- Accepted values for the header (case-insensitive):
  - true: `true`, `t`, `yes`, `y`
  - false: `false`, `f`, `no`, `n`
- Any other value is a 400 `bad-request`:

  ```
  "x-hasura-use-backend-only-permissions":  Not a valid boolean text. True values are ["true","t","yes","y"] and  False values are ["false","f","no","n"]. All values are case insensitive
  ```

  There are two spaces after the colon (`": "` followed by an error text that itself starts with a space) and two spaces before `False`; both are copied from the source (`mkUserInfo`, `Hasura/Authentication/User.hs`). The header is only parsed when condition 1 holds.
- JWT and webhook requests never get backend access, even with the header. The role is typically set via `x-hasura-role` along with the admin secret (impersonation).
- A backend-only insert permission still requires the mutation to pass `check`, `set` and so on.
- When `backend_only: true`, the corresponding `insert_x` / `update_x` / `delete_x` fields are absent from the normal role schema. Absent fields give the usual GraphQL validation error (`field 'insert_x' not found in type: 'mutation_root'`).
- Test: `server/lib/api-tests/src/Test/Databases/Postgres/BackendOnlyPermissionsSpec.hs`.

---

## 9. How permissions shape the per-role schema

The schema is built for every role in the role set. The role set is the union of:

- all roles named in table permissions, function permissions, action permissions, remote schema permissions and inherited roles;
- plus `admin`.

Code: `GraphQL/Schema.hs` `buildGQLContext`; role collection in `RQL/DDL/Schema/Cache.hs`.

### 9.1 Per-table rules

| GraphQL element | Requirement for role R |
|---|---|
| Object type `<t>` and `query_root.<t>` | select permission on t; root field allowed by `query_root_fields` |
| `<t>_by_pk` | select perm + all PK columns selectable + root field allowed |
| `<t>_aggregate` | select perm + `allow_aggregations` + root field allowed |
| `subscription_root.<t>_stream` | select perm + `select_stream` allowed |
| Column field | column in select `columns` |
| Scalar computed field | listed in `computed_fields` |
| Table computed field | select permission on returned table |
| Object/array relationship field | select permission on both source and target tables |
| `<rel>_aggregate` field | target table `allow_aggregations` |
| Remote relationship fields | always visible if the source table is visible (no permission on remote relationships; remote side applies its own) |
| `insert_<t>` | insert permission, not backend_only (frontend) |
| `insert_<t>_one` | insert permission + select permission |
| `update_<t>`, `update_<t>_many` | update permission, not backend_only (frontend) |
| `update_<t>_by_pk` | update permission + select permission with PK columns |
| `delete_<t>` | delete permission, not backend_only (frontend) |
| `delete_<t>_by_pk` | delete permission + select permission with PK columns |
| `returning` in mutation response | select permission (columns per select permission; rows are NOT filtered by the select filter, see 5.1) |
| `affected_rows` | always |
| `<t>_bool_exp` fields | selectable columns, selectable computed fields, relationships to visible tables |
| `<t>_order_by`, `<t>_select_column`, `distinct_on` | selectable columns only |

Types are generated only when reachable. A table with no select permission for a role has no `<t>` type in that role's schema, even if insert is allowed (only `insert_<t>` with `affected_rows`).

Fields that are not visible are not just null: they are absent. Querying them gives a GraphQL validation error, with message `field 'x' not found in type: 'y'` and code `validation-failed`.

### 9.2 Functions

See section 12.

### 9.3 Unknown roles

A role that appears in no permission gets the "unauthenticated" context (`makeGQLContext` in `GraphQL/Execute.hs`):

- If remote schema permissions are disabled: a schema with only the remote schemas (and no tables).
- If enabled: an empty schema.

A query against it fails with field-not-found validation errors (on a fresh engine with no remote schemas: `field 'x' not found in type: 'query_root'`). The usual `query_root` with `__typename` and introspection still works.

Note (inferred): the "schema registry" introspection payload is empty for non-admin roles. This is EE-only and irrelevant here.

### 9.4 Relay

Relay (`/v1beta1/relay`) builds an equivalent per-role schema with the same rules, plus a `node` field. For `node`, the role needs select permission with PK columns.

### 9.5 Session variables at execution

Mutations (and, in upstream, any transaction that writes) run inside a transaction. That transaction first executes:

```sql
SET LOCAL "hasura.user" = '{"x-hasura-role":"user","x-hasura-user-id":"1"}'
```

so that triggers can read `current_setting('hasura.user')`.

- Code: `setHeadersTx` in `Backends/Postgres/Connection/MonadTx.hs`.
- The value is the JSON of the session variables with lowercase keys.
- Queries (reads) do not set it (inferred; check the SQL spec).

---

## 10. `validate_input` (input validation webhook)

- OSS, Postgres only, available since v2.29.
- Docs: `docs/docs/schema/postgres/input-validations.mdx`.
- Code: `Backends/Postgres/Execute/Mutation.hs` (`validateUpdateMutation`, `validateDeleteMutation`) and `Backends/Postgres/Execute/Insert.hs` (`validateInsertInput`).

Flow:

1. It runs before the database transaction. The engine sends one HTTP POST per mutation root field. For nested inserts it sends one per table that has `validate_input`; nested rows are grouped per table.
2. Request:
   - `POST <url>`
   - `Content-Type: application/json`
   - Headers: the configured `headers`, the default headers (`Content-Type: application/json`, `User-Agent: hasura-graphql-engine/<version>`), plus the forwarded client headers if `forward_client_headers: true`. Merge (`validateMutation`, `Backends/Postgres/Execute/Mutation.hs`): `ClientHeadersFirst` = `uniquesOn name (client <> default <> configured)`; `ConfiguredHeadersFirst` = `uniquesOn name (configured <> default <> client)` (first occurrence wins).
     - Client headers are filtered by the same rules as action webhooks (`mkClientHeadersForward`, see 05-authentication.md 7.3).
     - Precedence between configured and client headers follows `HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE` (default `ClientHeadersFirst`; the flag makes configured headers win). See the config spec for the exact merge.
   - Timeout: `timeout` seconds.
   - Body:

     ```json
     {
       "version": 1,
       "role": "user",
       "session_variables": {"x-hasura-role": "user", "x-hasura-user-id": "1"},
       "data": {
         "input": [ ... ]
       }
     }
     ```

   - `data.input` per mutation type:

     | Mutation | `input` items |
     |---|---|
     | insert / insert_one | one object per row, built from the parsed IR (not the raw GraphQL): keys are DB column names, values are the parsed column values; literal `set` presets are included, session-variable presets are omitted; nested object/array relationships appear as `"<rel>": {"data": <row>}` / `{"data": [<rows>]}`. Nested rows are validated in a separate call per table (`validateInsertRows`, `Backends/Postgres/Execute/Insert.hs`) |
     | update | `[{"_set": {...}, "_inc": {...}, "where": {...}}]` (the GraphQL field arguments as given, variables substituted) |
     | update_by_pk | `[{"_set": {...}, "pk_columns": {...}}]` (arguments as given) |
     | update_many | the `updates` argument list as given |
     | delete | `[{"where": {...}}]` |
     | delete_by_pk | `[{"pk_columns": {"id": 1}}]` (the field's PK arguments wrapped in `pk_columns`) |

     If the field arguments are unavailable, `data` is `null`.

3. Response handling:

   | Webhook response | Result |
   |---|---|
   | 2xx (any body) | proceed |
   | 400 with body `{"message": "<text>"}` (`message` must be a string) | 400 `validation-failed`, message `<text>` |
   | 400 with another body | 500 `unexpected`, `received invalid response from input validation webhook` (internal: `"invalid response: <aeson error>"`) |
   | other status | 500 `unexpected`, `internal error when validating input data` (internal: `"expecting 200 or 400 status code, but found <n>"`) |
   | HTTP exception (connection refused, timeout) | 500 `unexpected`, `http exception when validating input data` |

   - The 500 errors carry `internal` details (request and response) when internal errors are shown.
   - Error path is `$` (inferred).
   - All of these become HTTP 200 on `/v1/graphql`.
4. Admin requests: the admin role's synthesised permissions have no `validate_input`, so admin skips validation.

Tests: `server/lib/api-tests/src/Test/Schema/DataValidations/Permissions/{InsertSpec,SelectSpec,UpdateSpec}.hs` (and DeleteSpec where present) and the input-validation tests in `server/lib/api-tests/src/Test/Mutations/...` (search for `validate_input`).

---

## 11. Inherited roles

- Docs: `docs/docs/auth/authorization/inherited-roles.mdx`.
- Code: `RQL/DDL/InheritedRoles.hs`, `RQL/Types/Roles/Internal.hs`, `RQL/DDL/Schema/Cache/Permission.hs`.

### 11.1 Metadata and API

```yaml
inherited_roles:
  - role_name: manager
    role_set: [employee, auditor]
```

| Command | args |
|---|---|
| `add_inherited_role` | `{role_name, role_set}` |
| `drop_inherited_role` | `{role_name}` |

Errors:

| Situation | HTTP | code | message |
|---|---|---|---|
| Role in its own `role_set` | 400 | `invalid-params` | `an inherited role name cannot be in the role combination` |
| Parent roles do not exist in any permission | 400 | `not-exists` | `the following parent role(s) are not found: a, b which are required to construct the inherited role: "manager"` |
| Cycle among inherited roles | 400 | `cyclic-dependency` | `found cycle(s) in roles: ["a","b","a"]` (each cycle is the Haskell `show` of a list of role names with the first repeated at the end; cycles joined with `, `; `orderRoles`) |
| Drop non-existent | 400 | `not-exists` | `"manager" inherited role doesn't exist` |
| Add existing | no error | | `runAddInheritedRole` does `InsOrdHashMap.insert`, so re-adding an existing inherited role silently REPLACES its `role_set` |

Further rules:

- Parents may themselves be inherited roles. Roles are processed in topological order.
- A role that exists only through permissions counts as existing.
- An explicit permission defined directly for the inherited role on a table or entity overrides the derived one for that entity.

### 11.2 Select permission combination

For each table, the parents' select permissions are combined (`CombinedSelPermInfo` in `Table/Cache.hs`):

- Parents without a select permission are ignored. If no parent has one, there is none.
- **filter:** OR of the parent filters.
- **columns:** union. For each column, `RedactIfFalse(P)` where P is the OR of the filters of the parents that grant this column. If every parent with a select permission grants the column, no redaction is needed (`NoRedaction`).
- **computed_fields:** same rule as columns.
- **limit:** max of the limits of the parents that HAVE a limit (`Maybe (Max Int)` semigroup: `Nothing <> Just n = Just n`). A parent without a limit does NOT remove the limit; only when no parent has a limit is there none.
- **allow_aggregations:** true if any parent allows.
- **required headers:** union.
- **query_root_fields / subscription_root_fields:** union. Any "all" gives all.

Redacted columns:

- They become nullable in the GraphQL type, even when the column is NOT NULL.
- SQL wherever the column value is read: in the selection, in `where`, in `order_by`, in `distinct_on` (inferred) and inside aggregate arguments:

  ```sql
  CASE WHEN (<P1> OR <P2>) THEN "col" ELSE NULL END
  ```

- Nested inherited roles: when a parent is itself an inherited role whose column already carries `RedactIfFalse(R)`, that parent's contribution is `R AND <its filter>` instead of just its filter; `NoRedaction` is used only when every contributing expression is `NoRedaction` (`combinedSelPermInfoToSelPermInfo`, `Table/Cache.hs`, issue #10711).

Example: `employee` sees `{id, name}` with filter `{id: X-Hasura-User-Id}`. `auditor` sees `{id, name, salary}` with filter `{dept: X-Hasura-Dept}`. Then `manager` sees rows matching either filter, and `salary` is non-null only on rows matching the auditor filter.

Tests: `server/lib/api-tests/src/Test/Auth/Authorization/InheritedRoles/ColumnRedactionSpec.hs` and `.../ColumnRedaction/*`; `server/tests-py/queries/graphql_query/permissions/inherited_roles/*`.

### 11.3 Insert, update, delete, remote schema and logical-model permissions

- Parents without the permission are ignored.
- If all parents that define the permission define equal permissions, the inherited role gets that permission.
  - Equality is `OnlyRelevantEq`: structural equality where `_and`/`_or` lists compare as sets.
- If they differ, the inherited role gets no permission for that entity and an inconsistent object is reported:
  - `ConflictingInheritedPermission` (`RQL/Types/Metadata/Object.hs`). JSON in `get_inconsistent_metadata`: `{"reason": "Could not inherit permission for the role '<role>' for the entity: '<entity>'", "name": "<role>", "type": "inherited role permission inconsistency", "entity": <entity JSON>}`.
  - Equality covers everything relevant: columns, check/filter, presets, `backend_only`, required headers and `validate_input`.
  - Shown in `get_inconsistent_metadata`.
  - It cannot be dropped via `drop_inconsistent_metadata` (it is not a metadata object).
  - Fix it by defining the permission explicitly on the inherited role.
- Docs vs source: the docs say mutation permissions are inherited only if "the same for all parent roles". The source ignores parents that lack the permission. Follow the source.

### 11.4 Functions and actions

- These permissions are boolean (`mkBooleanPermissionMap`): the inherited role has access if any parent has access.
- For functions with inference on (12.1), access follows the return table's select permission anyway.

---

## 12. Function permissions

### 12.1 Inference

- `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS` (flag `--infer-function-permissions`). Default `true`.
- When true, a tracked function exposed as a query (default for STABLE/IMMUTABLE functions) is visible to every role that has a select permission on its return table.
  - Rows are filtered by that select permission, and columns follow it.
- When false, non-admin roles need an explicit function permission.
- Functions exposed as mutations (`exposed_as: mutation`, or VOLATILE by default) always need an explicit permission for non-admin roles, regardless of inference.

### 12.2 API

| Command | args |
|---|---|
| `pg_create_function_permission` | `{source?: "default", function, role}` |
| `pg_drop_function_permission` | `{source?, function, role}` |

`function` is a name string or `{schema, name}`. Metadata: `functions[].permissions: [{role: r}]`.

Errors (`Function/API.hs`):

- Return table has no select permission for the role (the actual check is that the role has ANY permission entry on the return table, `HashMap.member role (_tiRolePermInfoMap returnTable)`; the message still says select): 400 `not-supported` with message `function permission can only be added when the function's return table "author" has select permission configured for role: "user"`.
- Already exists: 400 `already-exists`, `permission of role "user" already exists for function "search_articles" in source: "default"`.
- Drop missing: 400 `not-exists`, `permission of role "user" does not exist for function "search_articles" in source: "default"`.

### 12.3 Effect

- The function root field (`<function>`, `<function>_aggregate` when the return table has `allow_aggregations`) appears in the role's schema.
- The return table's select filter and columns apply to the function's result rows.
- `session_argument`: the function receives the session JSON (lowercase keys) as a `json` argument.

---

## 13. Action permissions

| Command | args |
|---|---|
| `create_action_permission` | `{action, role, definition?: <ignored>, comment?}` |
| `drop_action_permission` | `{action, role}` |

Metadata: `actions[].permissions: [{role: r, comment?}]`.

Errors (`RQL/DDL/Action.hs`):

- Exists: 400 `already-exists`, `permission for role "user" is already defined on "create_user"`.
- Drop missing: 400 `not-exists`, `permission for role: "user" is not defined on "create_user"`.

Effect:

- The action root field and its custom output types are visible only to `admin` and roles with a permission (including inherited roles, by boolean union).
- Relationships from an action output type to tables require select permission on the target table.
- Async actions: `<action>` returns an id. The query `<action>(id)` returns the result only to the same role and session (upstream checks the `session_variables` stored with the action log; details in the actions spec).

---

## 14. Remote schema permissions

- Docs: `docs/docs/auth/authorization/permissions/remote-schema-permissions.mdx`.
- Code: `RemoteSchema/MetadataAPI/Permission.hs`, `RemoteSchema/SchemaCache/Permission.hs`, `GraphQL/Execute/Remote.hs`.

### 14.1 Enabling

- `HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS=true` (flag `--enable-remote-schema-permissions`). Default false.
- Disabled: every role (including unknown roles) sees every remote schema in full, and the engine forwards queries unchanged.
- Enabled: only `admin` and roles with a remote schema permission for that schema (including inherited) see it.

### 14.2 API

| Command | args |
|---|---|
| `add_remote_schema_permissions` | `{remote_schema, role, definition: {schema: "<SDL>"}, comment?}` |
| `drop_remote_schema_permissions` | `{remote_schema, role}` |

Metadata: `remote_schemas[].permissions: [{role, definition: {schema}}]`.

Errors:

| Situation | HTTP | code | message |
|---|---|---|---|
| Feature disabled | 400 | `constraint-violation` | `remote schema permissions can only be added when remote schema permissions are enabled in the graphql-engine` |
| Remote schema unknown | 400 | `not-exists` | `remote schema "x" doesn't exist` |
| Exists | 400 | `already-exists` | `permissions for role: "user" for remote schema:"x" already exists` |
| Drop missing | 400 | `not-exists` | `permissions for role: "user" for remote schema:"x" doesn't exist` |
| SDL invalid against upstream | 400 | `validation-failed` | `validation for the given role-based schema failed because <reason>`; for several reasons: `validation for the given role-based schema failed for the following reasons:\n • <r1>\n • <r2>\n` |

### 14.3 SDL rules

- The SDL must be a subset of the upstream introspected schema:
  - same type names and kinds;
  - a subset of fields;
  - a subset of arguments, but all non-nullable upstream arguments must be present (unless preset);
  - same argument and field types;
  - a subset of enum values and of union members;
  - only upstream interfaces.
- A `schema { query: ... }` definition is optional when the root type is named `Query`. Otherwise the query root must exist: `query root does not exist in the schema definition`.
- Validation reasons (`showRoleBasedSchemaValidationError`), verbatim fragments:
  - `<Kind>: "<name>" does not exist in the upstream remote schema`
  - `expected type of "<field>"(<Kind>) to be <upstream type> but received <given type>`
  - `field: "<f>" expects the following non nullable arguments to be present: "a" and "b"`
  - `field "<f>" does not exist in the <Kind>: "<T>"`
  - `union "<U>" contains members which do not exist in the members of the remote schema union :"A"`
  - `custom interfaces are not supported. Object"<O>" implements the following custom interfaces: "I"`
  - `enum "<E>" contains the following enum values that do not exist in the corresponding upstream remote enum: "X"`
  - `multiple schema definitions found`
  - `duplicate type names found: "A" and "B"`
  - `Preset directives can be defined only on INPUT_FIELD_DEFINITION or ARGUMENT_DEFINITION`
  - `found multiple preset directives at <parentType> "<name>"`
  - `no arguments found in the preset directive`
  - `preset argument "value" not found at "<arg>"`
  - `"<v>" not found in the enum: "<E>"`
  - `expected preset value "<v>" of type "<T>" to be an enum value` (also `... to be a scalar value`, `... to be an input object value`)
  - `"<k>" does not exist in the input object "<T>"`
  - `illegal preset value at "<name>". Session arguments can only be set for singleton values`
  - `expected preset static value to be a Boolean value`
- Lists use `englishList "and"`: `"a"`, `"a" and "b"`, `"a", "b" and "c"`.

### 14.4 `@preset`

```graphql
type Query {
  user(id: Int! @preset(value: "x-hasura-user-id")): User
  posts(filter: PostFilter @preset(value: {status: "published"})): [Post]
  echo(text: String @preset(value: "x-hasura-literal", static: true)): String
}
```

- `@preset(value: <GraphQL value>, static: Boolean = false)` is allowed on argument definitions and input-object field definitions only.
- An argument or input field with a preset is removed from the role's schema. The engine injects the value on every call.
- A string value starting with `x-hasura-` (case-insensitive) is a session variable reference, unless `static: true`. Session variable presets are allowed only on singleton scalar or enum types, not lists or input objects.
- Static presets may be any valid literal of the type, including input objects and lists.

Runtime (`GraphQL/Execute/Remote.hs`, `resolveRemoteVariable`):

- A session-variable preset becomes a GraphQL variable named after the variable with `-` replaced by `_` (e.g. `$x_hasura_user_id`). Its type is the non-null base type, and it is added to the operation's variable definitions.
- Missing variable: 400 `not-found`, `"x-hasura-user-id" session variable expected, but not found`. Note the wording differs from table permissions.
- Coercion of the text value by the target type:
  - `Int`: parsed as an integer; failure gives `coercion-error` `"abc" cannot be coerced into an Int value`.
  - `Float`: parsed as a number; message `... cannot be coerced into a Float value`.
  - `Boolean`: exactly `true` or `false`, else `... cannot be coerced into a Boolean value`.
  - `String`, `ID` and custom scalars: string as-is.
  - Enum: the value must be a valid GraphQL name (else `"<v>" is not a valid GraphQL name`) and one of the enum values, else `"<v>" is not one of the valid enum values`. All coercion errors use code `coercion-error`.

Tests: `server/tests-py/test_remote_schema_permissions.py` and `server/tests-py/queries/remote_schemas/permissions/`.

---

## 15. Role and session resolution summary for permission evaluation

1. The role comes from authentication (05). `x-hasura-role` is always present in the session variables and equals the resolved role.
2. `admin` short-circuits everything (section 1).
3. The schema context is chosen by role: the per-role context, or unauthenticated for unknown roles. Within it, frontend vs backend is chosen by `backend_only_field_access`.
4. Permission bool exps are instantiated with session variables:
   - Prepared as `$1->>'key'` with a cast to the column type.
   - Missing gives 400 not-found `missing session variable: "<name>"`.
   - Bad cast gives `data-exception`.
5. Mutations:
   1. validate_input (if any)
   2. transaction with `SET LOCAL hasura.user`
   3. DML with filter/presets
   4. check aggregate
   5. commit or rollback with `permission-error`.

---

## 16. Edge cases and quirks to reproduce

- `filter: {}` = true. `check: {}` = true. `_or: []` = false. `_and: []` = true. `_not: {}` = false.
- A bare value in a column position is `_eq`: `{"id": "X-Hasura-User-Id"}` is valid.
- Session variable names in metadata are case-insensitive. They are lowercased in errors and SQL.
- The string value `"X-Hasura-User-Id"` in a text column's filter is treated as a session variable, never as a literal string. There is no escaping. This is upstream behaviour.
- `columns: []` for select is allowed: the role can select only `__typename`, aggregates `count`, and relationships (inferred).
- Permissions with an unknown column produce metadata inconsistencies rather than hard failures when loaded from stored metadata. API calls fail with 400.
- `limit: 0` is allowed (returns no rows) (inferred).
- Inherited role column redaction applies even when the column is used only in `where`. Filtering on a redacted column compares against NULL for rows the granting parents do not cover.
- On `/v1/graphql`, every error above is returned with HTTP 200. The real status code matters for `/v1alpha1/graphql` and for `/v1/query` and `/v1/metadata`.

---

## Upstream tests to reuse

Postgres and OSS tests to port or run against the Rust engine:

- `server/tests-py/queries/graphql_query/permissions/`: core select permission fixtures (session vars, `_in`/`_nin` with array session vars, jsonb `_has_keys_*`, geometry with session vars, missing-variable error, `limit`, `_by_pk` hidden).
- `server/tests-py/queries/graphql_query/permissions/inherited_roles/` and `.../roles_inheritance/`: inherited select roles, cycles, override, parents without permissions.
- `server/tests-py/queries/graphql_mutation/insert/permissions/`: insert check, presets, upsert with update check, array session vars, `permission-error` paths.
- `server/tests-py/queries/graphql_mutation/update/permissions/`: update filter and check, presets.
- `server/tests-py/queries/graphql_mutation/delete/permissions/`: delete filter.
- `server/tests-py/queries/graphql_mutation/roles_inheritance/` (if present) and `server/tests-py/test_roles_inheritance.py`: inherited mutation permission conflicts and inconsistency reporting.
- `server/tests-py/queries/remote_schemas/permissions/` and `server/tests-py/test_remote_schema_permissions.py`: SDL validation errors, `@preset`, session-var coercion.
- `server/tests-py/queries/unauthorized_role/`: unknown or unauthorized role schema behaviour.
- `server/tests-py/test_graphql_queries.py` and `server/tests-py/test_graphql_mutations.py`: the classes that use the directories above (grep `permissions`).
- `server/tests-py/test_v1_queries.py` (permission sections): `/v1/query` `create_*_permission` errors.
- `server/lib/api-tests/src/Test/Auth/Authorization/DisableRootFields/` (`DefaultRootFieldsSpec.hs`, `SelectPermission/*`): `query_root_fields` and `subscription_root_fields`.
- `server/lib/api-tests/src/Test/Auth/Authorization/InheritedRoles/ColumnRedactionSpec.hs` and `ColumnRedaction/*`: CASE-redaction semantics. Run only the Postgres fixtures.
- `server/lib/api-tests/src/Test/Databases/Postgres/BackendOnlyPermissionsSpec.hs`: frontend vs backend schema and header parsing.
- `server/lib/api-tests/src/Test/Schema/DataValidations/Permissions/{InsertSpec,SelectSpec,UpdateSpec}.hs`: permission validation at metadata time.
- `server/lib/api-tests/src/Test/Regression/OptimizePermissionFiltersSpec.hs`: null-safety only for nullable columns.
- `server/lib/api-tests/src/Test/Regression/DoNotTruncateSessionVariables8158Spec.hs`: long session variable values.
- `server/lib/api-tests/src/Test/Regression/DropColumnWithPermissions8415Spec.hs`: dependency handling when dropping columns used in permissions.
- `server/lib/api-tests/src/Test/Queries/Relationship/PermissionRootReferenceSpec.hs` (locate with `grep -rl PermissionRootReference server/lib/api-tests`): `$` root references.
- `server/lib/api-tests/src/Test/Queries/...PermissionsKnowAboutColumnNullability...` (locate with grep): nullable column equality.
- api-tests matching `validate_input` / `InputValidation` (grep `validate_input` in `server/lib/api-tests`): the input validation webhook.

Skip:

- `server/tests-py/queries/graphql_query/permissions/inherited_roles_mssql/` and any `*_mssql.yaml`.
- `server/lib/api-tests/src/Test/DataConnector/*`.
- BigQuery, SQL Server and Citus variants of the api-tests above (the `Sqlserver`, `BigQuery` and `Citus` fixtures in each spec).
- `ColumnRedaction` "NotSupported" backends.
- Anything in `server/src-test/Hasura/Backends/DataConnector`.
- EE-only test suites under `pro/` (not in the OSS repo).
