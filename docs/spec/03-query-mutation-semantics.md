# 03 — Query & mutation execution semantics on Postgres (and the SQL Hasura emits)

Status: research spec for the Rust re-implementation of Hasura GraphQL Engine v2 (OSS/CE behaviour).
Ground truth: Haskell sources under `server/src-lib/Hasura/...` of the upstream repo (paths below are
relative to the repo root). Docs (`docs/docs/...`) are secondary. Anything marked **(inferred)** was
reasoned from code but not confirmed by a test fixture or by running the engine.

Related docs (owned by other workers): HTTP transport & generic error envelope, GraphQL schema
generation (type/field names, argument shapes), permissions/metadata, subscriptions (live & streaming
SQL), actions, remote joins, events. This document focuses on *what happens after a GraphQL operation
has been parsed into IR*: the SQL that is generated, how it is executed, and how results/errors come back.

---

## Scope and priority

| Area | Priority | Notes |
|---|---|---|
| Simple selects (list, `_by_pk`), nested object/array relationships, `where`/`order_by`/`limit`/`offset`/`distinct_on` | **Essential** | Every project uses these. |
| Permission injection (row filter, column restriction, `limit`, session variables) | **Essential** | Must be bit-exact in semantics. |
| Aggregates (`_aggregate`, `count`, `sum`/`avg`/..., `nodes`), aggregate `order_by` | **Essential** | Very common. |
| Inserts (`insert_x`, `insert_x_one`), nested inserts, `on_conflict` upserts, insert/update `check` | **Essential** | |
| Updates (`update_x`, `update_x_by_pk`, `_set`, `_inc`, jsonb ops), `update_x_many` | **Essential** (`_many` common since 2.10) | |
| Deletes (`delete_x`, `delete_x_by_pk`) | **Essential** | |
| Multiple mutations in one request → one transaction | **Essential** | Users depend on rollback semantics. |
| Postgres error → GraphQL error mapping | **Essential** | Error codes/messages are part of the client contract. |
| Tracked functions (query & mutation), computed fields (scalar & SETOF) | **Essential** for many projects | |
| `stringify-numeric-types`, enum upper-casing for `graphql-default` naming, PostGIS GeoJSON output | Common | |
| Aggregation predicates in `where` (`<rel>_aggregate: {count: {...}}`) | Common (2.15+) | |
| Relay (`_connection`, node id) | Rare | Only if `/v1beta1/relay` used. Brief coverage here. |
| Inherited roles column redaction (`CASE WHEN ... ELSE NULL`) | Rare | |
| Native queries / logical models (CTE injection) | Rare (2.28+) | Brief coverage. |
| `@cached` directive response caching | **EE/Cloud only** | OSS accepts and ignores the directive (see §8). |
| Read replicas, connection templates, connection sets | **EE/Cloud only** | OSS ignores `read_replicas` (see §7.4). |
| Input validation webhooks (`validate_input` on insert/update/delete permissions) | Rare; CE feature since 2.29 | Executed during planning, before SQL (see §6.10). |
| Cursor-based streaming subscriptions, live-query multiplexing | — | Covered by subscriptions doc. |

**Key implementation freedom:** the exact SQL *text* is only externally observable via
`/v1/graphql/explain`, the `query-log` log type (`generated_sql`), and `internal` error details
(`statement`) shown to admins. A Rust engine may generate different SQL as long as the **JSON results,
row semantics, ordering, error codes and messages are identical**. We still document the upstream SQL
shape precisely because (a) it is the reference semantics, (b) it is proven to perform well, (c) explain
fixtures compare SQL text (`ignoreWhitespace`) — we can choose to match or to skip those tests.

---

## 1. Execution pipeline overview

Upstream flow for a `/v1/graphql` request (`server/src-lib/Hasura/GraphQL/Transport/HTTP.hs` `runGQ`):

1. Parse & validate the GraphQL document, pick the single operation (`getSingleOperation`).
2. Build an **execution plan**: an *ordered* map `RootFieldAlias → ExecutionStep`
   (`Hasura/GraphQL/Execute/Query.hs` `convertQuerySelSet`, `Hasura/GraphQL/Execute/Mutation.hs`
   `convertMutationSelectionSet`). Step kinds:
   - `ExecStepDB` — one SQL statement (query) or one transaction action (mutation) against one source;
   - `ExecStepRemote` — remote schema; `ExecStepAction` — action; `ExecStepRaw` — introspection /
     `__typename` root fields; `ExecStepMulti` — list of steps (namespaced root fields).
   - IR → SQL translation happens **during planning** for every root field before any is executed. For
     mutations, input-validation webhooks also run here (§6.10).
3. Execute steps **sequentially in root-field order** (`InsOrdHashMap.traverseWithKey`), except:
   - all-Postgres mutations against the same source are coalesced into one transaction (§7.2).
4. Each DB step returns a **single JSON text value**; the HTTP response is assembled by splicing those
   raw JSON texts under their aliases: `{"data":{"<alias1>":<json1>,"<alias2>":<json2>}}`
   (`encodeAnnotatedResponseParts`). The JSON from Postgres is *not* re-parsed unless remote joins
   need post-processing.
5. On the first failing step, execution stops (ExceptT short-circuit); the whole response becomes
   `{"errors":[...]}` — **no partial `data`** for DB errors (`buildResponseFromParts`/`buildResponse` in
   `Transport/HTTP.hs`). Only remote-schema `GQExecError`s produce a non-throwing errors response.

Query root fields: **one SQL statement per root field**, each run separately (not combined into one
statement, not in a shared transaction/snapshot), each acquiring a pooled connection
(`runPGQuery` → `runQueryTx`). There is no cross-root-field snapshot consistency for queries.

---

## 2. Response JSON shape rules

### 2.1 JSON is built inside Postgres

Every select root field returns exactly one row with one column aliased `"root"` containing JSON
(`json`, not `jsonb`, so key order is preserved and duplicate-free by construction).

- **Row object**: `row_to_json((SELECT "_e" FROM (SELECT <expr> AS "<field alias>", ...) AS "_e"))`
  (`SQL/DML.hs` `applyRowToJson`/`mkRowExp`; the inner table alias is `e`, rendered `"_e"` after
  identifier renaming, §3.9).
  - If **any** field alias in that object is longer than 63 characters, `json_build_object('<alias>',
    <expr>, ...)` is used instead (Postgres truncates identifiers at 63 chars;
    `Translate/Select/AnnotatedFieldJSON.hs` `pgAnnRowToJson`). Citus always uses `json_build_object`.
- **List** (`JASMultipleRows`): `coalesce(json_agg("root" [ORDER BY ...]), '[]')` — empty result is
  `[]`, never `null` (`Internal/Extractor.hs` `withJsonAggExtr`).
- **Single object** (`JASSingleObject` — `_by_pk`, `insert_x_one`, `update_x_by_pk`,
  `delete_x_by_pk`, functions returning a single row): `coalesce((json_agg("root")->0), 'null')` →
  JSON `null` when no row (`asSingleRowExtr`).
- **Object relationship**: lateral subquery yields `row_to_json(...)` per parent row; missing related row →
  SQL NULL via `LEFT OUTER JOIN` → JSON `null`.
- **Array relationship**: lateral subquery with `coalesce(json_agg(...), '[]')` → `[]` when no rows.
- **Aggregate field**: `json_build_object('aggregate', json_build_object(...), 'nodes', coalesce(json_agg(...),'[]'))`
  — keys in selection order (§4).
- **Mutation response**: `json_build_object('affected_rows', <count>, 'returning', <list>)` in selection
  order (§6.7).

### 2.2 Field aliasing, ordering, `__typename`

- JSON keys are the **GraphQL response keys** (alias if given, else field name) in **selection-set
  order** after GraphQL field collection (fragments inlined; duplicate response keys merged at first
  occurrence — standard GraphQL `CollectFields`). Column values are extracted as
  `"<base alias>"."<column>" AS "<response key>"`.
- The same column may appear several times under different aliases; each is a separate extractor.
- `__typename` inside a table selection → literal: `'author' AS "__typename"` (`AFExpression` →
  `S.SELit`). The literal is the (possibly customised) GraphQL type name.
- `__typename` inside an aggregate/connection object (where the SELECT is an aggregate context) is wrapped
  to force aggregation: `coalesce('author_aggregate', (bool_or('true'))::text)`
  (`Internal/Helpers.hs` `withForceAggregation`).
- `__typename` inside `aggregate {...}` / `sum {...}` → plain literal inside `json_build_object`.
- `__typename` in mutation response → literal in `json_build_object`.
- Root-level `__typename` (and introspection) is answered without SQL (`ExecStepRaw`).

### 2.3 Value encoding (`toJSONableExp`, `Translate/Column.hs`)

Postgres JSON functions decide the textual JSON representation; Hasura only wraps some columns:

| Column type | Expression emitted | JSON output |
|---|---|---|
| bigint, bigserial, numeric/decimal, double precision, money — **when stringify is ON** | `("col")::text` | JSON string |
| same — stringify OFF (default) | `"col"` | Postgres `to_json` format (numeric → JSON number, money → string `"$123.45"`) |
| geometry, geography | `(ST_AsGeoJSON("col", 15, 4))::json` | GeoJSON object (15 max decimal digits, option 4 = include short CRS) |
| enum-table column, when source naming convention is `graphql-default` | `upper("col")` | uppercased string |
| any column requested "as text" (internal mutation fetch) | `("col")::text` | string |
| everything else | `"col"` | `to_json` of the value |

- `isBigNum` (`SQL/Types.hs`) = `bigint`, `bigserial`, `numeric`, `double precision`, `money`. **Not**
  `real`, `integer`, `smallint`. Stringify is controlled by `--stringify-numeric-types` /
  `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES` (default `false`; `Server/Init/Arg/Command/Serve.hs`).
- Stringification is decided by the **column's** type even for aggregate results: `sum`/`avg`/... of
  an `integer` column are never stringified, `sum` of a `bigint` column is (`aggregateFieldToExp` uses
  the column type `ty` of `SFCol`). Scalar computed fields use the function return type.
- `count` is always a JSON number.
- JSON path argument on json/jsonb columns (`col(path: "a.b[0]")`) →
  `("_root.base"."col"#>ARRAY['a', 'b', '0'])` (`Instances/Schema.hs` `pgScalarSelectionArgumentsParser`;
  `jsonbPathOp` = `#>`). Empty path `"$"` → column unchanged.

Formatting produced by Postgres `json`/`row_to_json` (verified by
`server/tests-py/queries/graphql_query/basic/select_query_test_types_postgres.yaml`, run with stringify
ON and a UTC server):

```yaml
c1_smallint: 32767                 c2_integer: 2147483647
c3_bigint: '9223372036854775807'   c4_decimal: '123.45'      c5_numeric: '1.234'
c6_real: 0.00390625                c7_double_precision: '16.0001220703125'
c10_bigserial: '1'
c14_timestamp: '2004-10-19T10:23:54'
c15_timestamptz: '2015-10-17T14:42:43+00:00'
c16_date: '2014-09-14'             c17_time: '11:09:23'      c18_time_with_zone: '15:22:23+00'
c19_interval: '01:03:02'           c20_boolean: true
c21_point: '(1,2)'  c29_cidr: '192.168.100.128/25'  c31_macaddr: '08:00:2b:01:02:03'
c32_json / c33_jsonb: embedded JSON objects
c34_text_array: ["a","b","c"]      c35_integer_2d_array: [[4,5,6],[7,8,9]]
c36_uuid: a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11
c37_composite_type_complex: {r: 1.23, i: -3.456}     # composite → JSON object
c39_range_integer: '[123,456)'     c42_range_timestamp: '["2010-01-01 14:30:00","2010-01-01 15:30:02")'
c44_xml: '<foo>bar</foo>'          c45_money: $123.45        c46_ltree: 'A.B.C'
```

Implementation notes for Rust:
- The cheapest way to match all of this byte-for-byte is to **keep building JSON in Postgres** with the
  same functions and splice the text into the response. Re-encoding values in Rust would require
  replicating Postgres' formatting of timestamps (session `TimeZone` dependent — Hasura does **not** set
  `TimeZone`, `DateStyle`, `IntervalStyle` or `extra_float_digits` on its connections; the only
  per-connection setup is `SET client_encoding = 'UTF8'; SET client_min_messages TO WARNING;` in
  `lib/pg-client/src/Database/PG/Query/Connection.hs`), floats
  (PG ≥ 12 shortest-round-trip output), numerics (scale preserved: `avg` → `2.0000000000000000`),
  arrays, composites, ranges.
- `row_to_json` output is compact (`{"a":1}`); `json_build_object` output contains spaces
  (`{"a" : 1}`). Both appear in real Hasura responses; clients do not care, but golden tests that compare
  raw bytes would. The test harness compares parsed JSON/YAML.
- NaN/Infinity numerics/floats are emitted by Postgres as JSON strings `"NaN"`, `"Infinity"` (inferred,
  Postgres behaviour).

### 2.4 Null handling

- SQL NULL column → JSON `null` (key always present).
- Missing object relationship → `null`; empty array relationship → `[]`.
- `_by_pk` / single-object root with no row (or filtered out by permissions) → `null`.
- `nodes` of an aggregate with no rows → `[]`; `aggregate.count` → `0`; `sum/avg/max/min` of no rows → `null`
  per field (Postgres aggregate semantics).

---

## 3. SELECT translation

Source: `Backends/Postgres/Translate/Select/*.hs` (`Simple.hs` `mkSQLSelect`, `Aggregate.hs`
`mkAggregateSelect`, `Internal/Process.hs`, `Internal/GenerateSelect.hs`, `Internal/OrderBy.hs`,
`Internal/Extractor.hs`, `Internal/Aliases.hs`), `Translate/Types.hs`, `Translate/BoolExp.hs`.

### 3.1 Skeleton of a simple select

For root field `author(where: W, order_by: O, limit: L, offset: F) { id name }`, role with filter `P` and
permission limit `PL`:

```sql
SELECT coalesce(json_agg("root" <ORDER BY node-order>), '[]') AS "root"
FROM (
  SELECT row_to_json((SELECT "_e" FROM (SELECT "_root.base"."id" AS "id",
                                                "_root.base"."name" AS "name") AS "_e")) AS "root",
         <extra order-by / distinct-on extractors>
  FROM (
      SELECT * FROM "public"."author"
      WHERE (<P AND W>)                       -- 'true' when no filter
      [ORDER BY <base order>] [LIMIT n] [OFFSET m]   -- see 3.5 "base vs node"
  ) AS "_root.base"
  LEFT OUTER JOIN LATERAL (<object rel subquery>) AS "_root.or.<rel>" ON ('true')
  LEFT OUTER JOIN LATERAL (<array rel subquery>)  AS "_root.ar.root.<field>" ON ('true')
  ...
  [ORDER BY <node order>] [LIMIT n] [OFFSET m]
) AS "_root"
```

Verbatim upstream output (`server/tests-py/queries/explain/simple_query.yaml`):

```sql
SELECT  coalesce(json_agg("root" ), '[]' ) AS "root" FROM  (SELECT  row_to_json((SELECT  "_e"  FROM  (SELECT  "_root.base"."id" AS "id", "_root.base"."name" AS "name", "_root.base"."age" AS "age"       ) AS "_e"      ) ) AS "root" FROM  (SELECT  *  FROM "public"."user"  WHERE ('true')     ) AS "_root.base"      ) AS "_root"
```

Same query as role `user` with filter `{id: {_eq: X-Hasura-User-Id}}` (explain → literal mode, §5.3;
`id` nullable column ⇒ null-safe equality, §3.3) (`explain/permissions_query.yaml`):

```sql
... FROM  (SELECT  *  FROM "public"."user"  WHERE ((("public"."user"."id") = (('1')::integer)) OR ((("public"."user"."id") IS NULL) AND ((('1')::integer) IS NULL)))     ) AS "_root.base" ...
```

Notes:
- The base select is `SELECT * FROM "schema"."table" WHERE ...` **without a table alias**; all
  filter column references are qualified with the full table name (`"public"."user"."id"`), i.e. the
  `toSQLBoolExp` root reference is `QualTable` (`processSelectParams`).
- `WHERE` is always present; constant-true is rendered `('true')` (`BELit True` → `'true'`).
- Final `WHERE` = `permission filter AND user where` (`andAnnBoolExps permFilter whereM`), simplified
  (`simplifyBoolExp` drops `AND 'true'`).
- Permission `limit` vs user `limit`: `LIMIT min(user, perm)` (either may be absent) for root lists
  (`compareLimits`). For array relationships and aggregates with a permission limit, see §3.7 / §4.3.
- Single-object roots (`_by_pk`) use the same skeleton with `JASSingleObject` and a where of
  `pk1 = $n AND pk2 = $m` (non-nullable comparison) AND the permission filter.

### 3.2 Relationships (LEFT OUTER JOIN LATERAL)

Object relationship (`explain/limit_orderby_relationship_query.yaml`, `article(limit:2, order_by:{author:{id:desc}})`):

```sql
SELECT  coalesce(json_agg("root" ORDER BY "root.or.author.pg.id" DESC NULLS FIRST), '[]' ) AS "root"
FROM  (SELECT  row_to_json((SELECT  "_e"  FROM  (SELECT  "_root.base"."id" AS "id", "_root.base"."title" AS "title", "_root.base"."content" AS "content"       ) AS "_e"      ) ) AS "root",
               "_root.or.author"."root.or.author.pg.id" AS "root.or.author.pg.id"
       FROM  (SELECT  *  FROM "public"."article"  WHERE ('true')     ) AS "_root.base"
       LEFT OUTER JOIN LATERAL
         (SELECT  "_root.or.author.base"."id" AS "root.or.author.pg.id"
          FROM  (SELECT  *  FROM "public"."author"  WHERE (("_root.base"."author_id") = ("id"))    LIMIT 1 ) AS "_root.or.author.base"
         ) AS "_root.or.author" ON ('true')
       ORDER BY "root.or.author.pg.id" DESC NULLS FIRST LIMIT 2 ) AS "_root"
```

Array relationship (`explain/orderby_array_relationship_query.yaml`, `author { name articles(order_by:{id:desc}) { title } }`):

```sql
SELECT  coalesce(json_agg("root" ), '[]' ) AS "root" FROM  (SELECT  row_to_json((SELECT  "_e"  FROM  (SELECT  "_root.base"."name" AS "name", "_root.ar.root.articles"."articles" AS "articles"       ) AS "_e"      ) ) AS "root"
FROM  (SELECT  *  FROM "public"."author"  WHERE ('true')     ) AS "_root.base"
LEFT OUTER JOIN LATERAL
  (SELECT  coalesce(json_agg("articles" ORDER BY "root.ar.root.articles.pg.id" DESC NULLS FIRST), '[]' ) AS "articles"
   FROM  (SELECT  row_to_json((SELECT  "_e"  FROM  (SELECT  "_root.ar.root.articles.base"."title" AS "title"       ) AS "_e"      ) ) AS "articles",
                  "_root.ar.root.articles.base"."id" AS "root.ar.root.articles.pg.id"
          FROM  (SELECT  *  FROM "public"."article"  WHERE (("_root.base"."id") = ("author_id"))   ORDER BY "id" DESC NULLS FIRST  ) AS "_root.ar.root.articles.base"
          ORDER BY "root.ar.root.articles.pg.id" DESC NULLS FIRST  ) AS "_root.ar.root.articles"
  ) AS "_root.ar.root.articles" ON ('true')      ) AS "_root"
```

Rules (`Internal/GenerateSelect.hs` `defaultGenerateSQLSelect`, `Internal/Process.hs` `processAnnFields`):

- Join condition: `("<parent base alias>"."<lcol>") = ("<rcol>")` for each mapping pair, AND-ed, injected
  into the **child base select's WHERE** together with the child's filter: `WHERE (joinCond AND
  childFilter)` (`mkJoinCond`, `injectJoinCond`). The `ON` clause is always `('true')`.
  The right-hand column is **unqualified** (resolved against the child table).
- Object relationship child filter = target table's **select permission filter** of the current role
  (plus `deduplicatePermissions` optimisation behind experimental flag `optimize_permission_filters`).
  Array relationship child filter = target permission filter AND the user's `where` argument on the
  relationship field.
- Object relationships always get **`LIMIT 1`** on the child base select (`objectSelectSourceToSelectSource`,
  issue #7936: guards misconfigured one-to-many object relationships; result for those is
  non-deterministic).
- Join type: object relationships are generated with `Nullable` in the IR for tables, hence always
  `LEFT OUTER JOIN` (the `INNER JOIN` branch exists but is only reachable for some native-query
  relationships). Array relationships, connections and table computed fields: `LEFT OUTER JOIN LATERAL`.
- Join order: base, then object relations, array relations, array connections, computed-field table sets
  (each group in HashMap order — irrelevant for semantics).
- Nested relationships recurse: the child select has its own base/joins; the parent extracts
  `"_<child alias>"."<field>"`.
- Array relationship arguments (`where`, `order_by`, `limit`, `offset`, `distinct_on`) apply inside the
  lateral subquery with the same base/node rules (§3.5).
- **Similar array fields**: several selections of the same array relationship with *identical arguments*
  (and aggregate order-by on that relationship) share one prefix built from the sorted response keys:
  `"<parentAlias>.<key1>.<key2>"` (`mkSimilarArrayFields`, `mkUniqArrayRelationAlias`) so a single
  lateral subquery serves them (JoinTree maps are merged by source). Semantics unchanged.
- Remote relationships (to other sources / remote schemas) are stripped from the SQL and handled by the
  remote-join engine; the join columns are added as hidden extractors (remote joins doc).

### 3.3 Boolean expressions (`where`, permission filters, check expressions)

`Translate/BoolExp.hs` `translateBoolExp`; schema in `GraphQL/Schema/BoolExp.hs`, Postgres operators in
`Backends/Postgres/Instances/Schema.hs` `comparisonExps`.

Connectives:
| Input | SQL |
|---|---|
| object with several keys `{a: .., b: .., _or: ..}` | `BoolAnd` of all → `(x) AND (y)` (fold with `'true'` seed) |
| `{}` | `'true'` |
| `_and: [e1, e2]` | `(e1) AND ((e2) AND ('true'))` (foldr; `_and: []` → `'true'`) |
| `_or: [e1, e2]` | `(e1) OR ((e2) OR ('false'))` (`_or: []` → **`'false'`**) |
| `_not: e` | `NOT (e)` (`_not: {}` → `NOT ('true')`, i.e. false) |
| column `{c: {_op1: v1, _op2: v2}}` | AND of the operator comparisons on that column |

All binary operands are parenthesised: `BECompare` renders `(lhs) op (rhs)`; `BEBin` renders
`(l) AND (r)`.

Comparison operators (lhs = `"<qual>"."<col>"` where qual is the current table reference):

| GraphQL | SQL |
|---|---|
| `_eq: v` | `(c) = (v)` — GraphQL `where` always uses the plain form (`NonNullableComparison`) |
| `_neq: v` | `(c) <> (v)` |
| `_eq`/`_neq` in **permission** JSON (metadata) on a **nullable** column | null-safe: `((c) = (v)) OR (((c) IS NULL) AND ((v) IS NULL))`; `((c) <> (v)) OR (((c) IS NOT NULL) AND ((v) IS NULL))` (`Backends/Postgres/DDL/BoolExp.hs` chooses `NullableComparison` when the column is nullable; `{col: value}` shorthand is always nullable-style). Non-nullable column → plain `=`/`<>`. |
| `_in: [..]` | `(c) = ANY((<array param>)::<type>[])` (`BECompareAny`; list is one array parameter, `mkListParameter`) |
| `_nin: [..]` | `NOT ((c) = ANY(...))` |
| `_gt/_lt/_gte/_lte` | `>`, `<`, `>=`, `<=` |
| `_like/_nlike/_ilike/_nilike` | `LIKE`, `NOT LIKE`, `ILIKE`, `NOT ILIKE` (string types only) |
| `_similar/_nsimilar` | `SIMILAR TO`, `NOT SIMILAR TO` |
| `_regex/_iregex/_nregex/_niregex` | `~`, `~*`, `!~`, `!~*` |
| `_is_null: true/false` | `(c) IS NULL` / `(c) IS NOT NULL` |
| jsonb `_contains` / `_contained_in` | `@>` / `<@` (also arrays) |
| jsonb `_has_key` | `?` ; `_has_keys_any` `?|` (text[] literal); `_has_keys_all` `?&` |
| jsonb `_jsonb_path_exists` / `_jsonb_path_match` | `@?` / `@@` with rhs `(v)::jsonpath` |
| ltree `_ancestor`/`_ancestor_any` | `@>` ; `_descendant`/`_descendant_any` `<@`; `_matches` `~` (lquery); `_matches_any` `?` (lquery[]); `_matches_fulltext` `@` (ltxtquery) |
| geometry `_st_contains/_st_crosses/_st_equals/_st_intersects/_st_3d_intersects/_st_overlaps/_st_touches/_st_within` | `(ST_Contains(c, v))` etc. as boolean function call |
| `_st_d_within: {distance, from}` (geometry) | `ST_DWithin(c, from, distance)`; geography adds `use_spheroid` (default `true`) as 4th arg; `_st_3d_d_within` → `ST_3DDWithin` |
| raster `_st_intersects_rast`, `_st_intersects_nband_geom`, `_st_intersects_geom_nband` | `ST_Intersects(...)` variants |
| `_cast: {String: {...}}` (jsonb→text, geometry↔geography) | inner ops applied to `("c")::<target type>` |
| permission-only `_ceq/_cne/_cgt/_clt/_cgte/_clte: "col"` or `["$","col"]` | column-to-column comparison; `["$", col]` refers to the *root* table of the permission expression (`RootOrCurrentColumn IsRoot`) |
| permission-only `_exists: {_table, _where}` | `EXISTS (SELECT 1 FROM "s"."t" AS "_exists_table_N" WHERE <where on that alias>)` (rendered `"__exists_table_N"` after renaming) |

The operator `CompareOp` text is in `SQL/DML.hs` (instance `Show CompareOp`). Values on the rhs are
parameters/literals with a type cast (§5).

**Relationship in `where`** (`AVRelationship`):

```sql
EXISTS (SELECT 1 FROM "public"."article" AS "__be_0_article"
        WHERE ((("__be_0_article"."author_id") = ("public"."author"."id"))      -- mapping
               AND ((<target table select-permission filter on alias>)
                    AND (<user's nested where on alias>))))
```

- Alias `_be_<n>_<schema_table>` (`freshIdentifier`; schema omitted for `public`; then `_`-prefixed by
  renaming, §3.9). `n` is a per-boolexp counter.
- The **target table's select permission filter for the current role** is always AND-ed into the EXISTS
  (`rfTargetTablePermissions`) — filtering through a relationship can never see rows the role cannot
  select. If the role has no select permission on the target → `annBoolExpFalse` (the relationship is
  also absent from the schema in that case).
- Inside the target's permission filter, `$` (root) references are reset to the target table.
- Nested relationship filters chain EXISTS subqueries.

Verified example from `server/lib/api-tests/src/Test/Regression/OptimizePermissionFiltersSpec.hs`:

```sql
WHERE ((("_root.base"."foo_id") = ("bar_foo_id"))
  AND (EXISTS (SELECT 1 FROM "test_schema"."foo" AS "__be_0_test_schema_foo"
       WHERE (((("__be_0_test_schema_foo"."foo_id") = ("test_schema"."bar"."bar_foo_id")) AND ('true'))
              AND (('true') AND (((("__be_0_test_schema_foo"."foo_id") <= (('42')::integer)) AND ('true')) AND ('true')))))))
```

**Computed fields in filters** (only computed fields without user input arguments):
- Scalar: lhs is the function call `"schema"."fn"("public"."author".*[, <session arg>])`
  (`mkComputedFieldFunctionExp`; table-row argument rendered as `<qual>.*`).
- SETOF table: `EXISTS (SELECT 1 FROM "schema"."fn"("public"."author".*) AS "_be_N_schema_fn" WHERE
  (<returned table's select permission filter>) AND (<nested where>))`.

**Aggregation predicates** (`where: {articles_aggregate: {count: {predicate: {_gt: 1}, arguments: [..],
distinct: .., filter: {...}}}}`; `translateAVAggregationPredicates`):

```sql
EXISTS (SELECT 1 FROM
          (SELECT count(*) AS "article_count"                         -- <fn>(<args or *>) AS "<table>_<fn>"
           FROM "public"."article" AS "__be_0_article"
           WHERE (("__be_0_article"."author_id") = ("public"."author"."id"))   -- relationship
             AND <target select-permission filter> AND <filter>) AS "_sub"
        WHERE (("_sub"."article_count") > (($2)::integer)))
```

Supported functions (`Instances/Schema.hs` `aggregationFunctions`): `avg, bool_and, bool_or, count(*),
max, min, sum, corr(Y,X), covar_samp(Y,X), stddev_samp, var_samp`. **Quirk**: the `distinct` flag is
parsed (`aggPredDistinct`) but ignored by the Postgres translation (no `DISTINCT` emitted) — keep for
compatibility or fix knowingly.

**Null in operator values**: by default an explicit `null` for an operator is a parse error; with
`HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE=true` an explicit null makes the operator be ignored (i.e.
`{_eq: null}` → `true`) (`mkBoolOperator`, `Options.DangerouslyCollapseBooleans`). Schema doc owns details.

**Inherited roles redaction** (`withRedactionExp`): a column that only some parent roles may see is
emitted as `CASE WHEN <OR of granting roles' filters> THEN <col> ELSE NULL END` both in selection and in
where/order-by usage (note in `Translate/Select.hs`).

### 3.4 `order_by`

Enum values (`orderByOperators`): `asc` = `ASC NULLS LAST`, `asc_nulls_first`, `asc_nulls_last`,
`desc` = `DESC NULLS FIRST`, `desc_nulls_first`, `desc_nulls_last`. The nulls order is **always**
emitted explicitly (e.g. `"id" DESC NULLS FIRST`).

Order-by items and their extractors (`Internal/OrderBy.hs`):

| order_by element | extractor added to node select (alias) | value |
|---|---|---|
| column `c` | `"root.pg.c"` (`<prefix>.pg.<col>`) | `"_root.base"."c"` |
| object relationship `rel: {c: desc}` | `"root.or.rel.pg.c"` | lateral object join `"_root.or.rel"` exposes it; relationship join with target permission filter |
| array aggregate `rel_aggregate: {count: asc}` | `"root.ar.root.rel.order_by.count"` (alias = array source prefix + `.count`) | lateral `SELECT COUNT(*) AS "count" FROM (SELECT * FROM t WHERE join AND targetPermFilter) ...` |
| array aggregate `rel_aggregate: {max: {c: desc}}` (also `min`,`sum`,`avg`,`stddev`,... per schema) | `... .order_by.max.c` | `max("<src>"."<src>.pg.c")` |
| scalar computed field `cf: asc` | `"root.cf.cf"` | `"schema"."fn"("_root.base"[, args])` |
| SETOF computed field aggregate `cf_aggregate: {count: asc}` | `"root.cf.cf.order_by.count"` | lateral over `fn(row)` |

- ORDER BY is applied in the node select by alias, and **also** inside `json_agg(... ORDER BY ...)` so the
  array is ordered (`orderByForJsonAgg`).
- Order-by through an object relationship uses the target table's select permission filter (rows the role
  can't see sort as NULL).
- Without `order_by`, row order is whatever Postgres returns (no implicit primary-key order).

### 3.5 limit / offset / distinct_on: base vs node placement

Note "Optimizing queries using limit/offset" in `Internal/OrderBy.hs` + `applySortingAndSlicing` in
`Translate/Types.hs`:

- **No `order_by`**: `LIMIT/OFFSET/DISTINCT ON` applied on the **base** select.
- **`order_by` contains only columns of this table**: `ORDER BY` (by raw column names) + `LIMIT/OFFSET/
  DISTINCT ON` on the **base** select; node select also has `ORDER BY` (by aliases) without limit; json_agg
  ordered. Example (`explain/limit_orderby_column_query.yaml`):

```sql
SELECT  coalesce(json_agg("root" ORDER BY "root.pg.author_id" DESC NULLS FIRST), '[]' ) AS "root" FROM  (SELECT  row_to_json((SELECT  "_e"  FROM  (SELECT  "_root.base"."id" AS "id", "_root.base"."title" AS "title", "_root.base"."content" AS "content"       ) AS "_e"      ) ) AS "root", "_root.base"."author_id" AS "root.pg.author_id" FROM  (SELECT  *  FROM "public"."article"  WHERE ('true')   ORDER BY "author_id" DESC NULLS FIRST LIMIT 5 ) AS "_root.base"    ORDER BY "root.pg.author_id" DESC NULLS FIRST  ) AS "_root"
```

- **`order_by` has any non-column element** (relationship, aggregate, computed field): everything applied
  at the **node** select after the lateral joins (see §3.2 object-relationship example: `ORDER BY ...
  LIMIT 2` / `LIMIT 3 OFFSET 2` on the outer select).
- `DISTINCT ON`: at base as `DISTINCT ON ("c1","c2")`; at node as `DISTINCT ON ("root.pg.c1", ...)` with
  extra extractors. Validation: `distinct_on` columns must equal the first N `order_by` items, all of them
  columns, else parse error `"distinct_on" columns must match initial "order_by" columns`
  (`GraphQL/Schema/Select.hs` `defaultArgsParser`). No `order_by` → allowed.
- `limit`: `Int`, non-negative (`P.nonNegativeInt`), `null` = no limit. `offset`: `bigint`, `null` = none.
  Rendered literally (not parameterised): `LIMIT 5 OFFSET 2` (`intToSQLExp`).
- `FETCH FIRST n ROWS WITH TIES` exists in the AST but is not produced by GraphQL selects today.

### 3.6 Computed fields & functions in selections

- **Scalar computed field**: extractor `"schema"."fn"(<args>) AS "<key>"`, args built by
  `fromTableRowArgs`: table-row arg → `"_root.base"` (row identifier), session arg → session JSON
  (`$1` in queries), user args from the field's `args:` input, positional first then
  `"name" => value` named. Result passed through `toJSONableExp` with the function return type.
  Example: `"public"."author_full_name"("_root.base") AS "full_name"` (inferred rendering).
- **SETOF-table computed field**: `LEFT OUTER JOIN LATERAL (SELECT coalesce(json_agg(...),'[]') AS "<key>"
  FROM (SELECT ... FROM (SELECT * FROM "schema"."fn"("_root.base", ...) AS "_<fn alias>" WHERE
  <returned table perm filter AND where>) ...) ) AS "_root.cf.<key>" ON ('true')`
  (`computedFieldToFromItem`); supports where/order_by/limit/offset/distinct_on of the returned table.
- **Tracked function root field** (`search_articles(args: {...}, where, order_by, ...)`): base select is
  `SELECT * FROM "public"."search_articles"(($2)::text) AS "_search_articles" WHERE <filter of the
  returned table's select permission AND where>` — alias = `qualifiedObjectToText` (`schema.fn`, bare name
  for `public`), filter columns qualified with that alias (`FromFunction` in `processSelectParams`).
  - Arguments: user args are nullable GraphQL fields named after SQL arg names (`arg_1`, `arg_2`... for
    unnamed). All provided args (in SQL order) up to the first missing one are passed **positionally**;
    after a gap the remaining are passed **by name** (`"x" => v`). Missing arg without default →
    `Non default arguments cannot be omitted`; unnamed arg after a gap → `Only last set of positional
    arguments can be omitted` (`functionArgs` in `Backends/Postgres/Schema/Select.hs`).
  - `session_argument` (metadata) receives the session variables JSON (`$1` in queries; in mutation
    transactions a JSON text literal, §5).
  - Functions returning a single row (not SETOF) → `JASSingleObject`.
  - `<fn>_aggregate` root fields exist when allowed.
- VOLATILE functions tracked as mutations (`exposed_as: mutation`) are executed via `convertFunction`:
  same select SQL, **prepared/parameterised like queries**, but run inside the mutation transaction (§7).

### 3.7 Permission `limit` inside array relationships and aggregates

`PermissionLimitSubQuery` (`Internal/Process.hs`, `Internal/Extractor.hs` `withJsonAggExtr`):

- Root simple list: `LIMIT least(user, perm)` directly (`PLSQNotRequired`).
- Array relationship whose target has a permission limit: base select uses only the user limit; the
  permission limit is applied on the aggregated rows through:

```sql
(SELECT coalesce(json_agg("sub_query"."<key>" ORDER BY "ob_col_1" DESC NULLS FIRST), '[]')
 FROM (SELECT "unnest_table"."<key>" AS "<key>", "unnest_table"."ob_col_1" AS "ob_col_1"
       FROM UNNEST(array_agg("<key>"), array_agg("<order exp 1>")) AS "_unnest_table"("<key>", "ob_col_1")
       ORDER BY "ob_col_1" DESC NULLS FIRST
       LIMIT <perm limit>) AS "_sub_query")
```

  Result: same as `LIMIT least(user, perm)` after `OFFSET` (semantically identical).
- Aggregate root/relationship (`x_aggregate`) whose table has a permission limit **and** which selects
  any `aggregate {}` field or orders by an array aggregate: the permission limit is applied **only to
  `nodes`** (via the same unnest subquery); `aggregate { count ... }` is computed over **all** rows
  matching filter + user limit. Verified: `tests-py/queries/graphql_query/agg_perm/author_agg_articles.yaml`
  (role limit 1 → `count: 2`, one node). Must be replicated.

### 3.8 Relay connections (rare)

`_connection` fields (only on `/v1beta1/relay`) use a `WITH` chain: `__base_select` (+ `row_number() over
(partition by 1)` as `__row_number`), optional `__split_select` (cursor `after`/`before` comparisons on
order-by aliases, lexicographic OR/AND expansion), `__slice_select` (`first` → `LIMIT`; `last` → order by
row number desc, limit, re-order asc), `__final_select`, `__cursors_select`
(`(array_agg("__cursor"))[1]`, last element via `array_length`), `__page_info` (`hasPreviousPage`/
`hasNextPage` via `EXISTS` on row numbers) (`connectionToSelectWith`). Cursor = base64 of
`json_build_object(<order-by key path>: value, ...)` (or PK columns when no order_by), base64 via
`regexp_replace(encode(convert_to(<text>, 'UTF8'), 'base64'), '\n', '', 'g')`. Node id =
base64 of `json_build_array(1, '<schema>', '<table>', <pk values>)`. Low priority.

### 3.9 Identifier renaming (`SQL/RenameIdentifiers.hs`)

Applied to every generated query (`toQuery` → `renameIdentifiersSelectWithTopLevelCTE`):
1. Every **table alias** (subquery alias, CTE name, function alias, EXISTS alias) is prefixed with `_`
   and all references to it are rewritten: `root.base` → `"_root.base"`, `e` → `"_e"`,
   `_be_0_article` → `"__be_0_article"`, `mra__author` → `"_mra__author"`. Column aliases are **not**
   prefixed (`"root.pg.id"`). Purpose: avoid `row_to_json` confusion when a column is named like a table
   alias (`root`).
2. Any identifier longer than 63 chars is replaced by `md5_<32 hex md5>_<original>` (so truncation by
   Postgres still leaves it unique).

Rust: replicate (2) or use short generated aliases (e.g. `_t1`, `_t2`); (1) only matters for explain
parity. Long user identifiers (table/column names ≤63 by definition) are never renamed.

### 3.10 Native queries (rare, CE since 2.28)

Native query roots/relationships inject their SQL as top-level CTEs `WITH "_cte_<rootfield>_<n>" AS
(<interpolated SQL, arguments as params>) SELECT ...` and select `FROM` the CTE identifier
(`fromNativeQuery`, `customSQLToTopLevelCTEs`). Lower priority.

---

## 4. Aggregates

`Translate/Select/Aggregate.hs`, `Internal/Process.hs` `processAnnAggregateSelect`,
`aggregateFieldToExp`, `Internal/Extractor.hs` `aggregateFieldsToExtractorExps`.

`author_aggregate(where: W) { aggregate { count sum { id } } nodes { name } }` →

```sql
SELECT json_build_object(
         'aggregate', json_build_object('count', COUNT(*),
                                        'sum', json_build_object('id', sum("_root"."root.pg.id"))),
         'nodes', coalesce(json_agg("root.f.nodes"), '[]')) AS "root"
FROM (SELECT "_root.base"."id" AS "root.pg.id",
             row_to_json((SELECT "_e" FROM (SELECT "_root.base"."name" AS "name") AS "_e")) AS "root.f.nodes"
      FROM (SELECT * FROM "public"."author" WHERE (<perm AND W>)) AS "_root.base") AS "_root"
```
(constructed from source; aliases per `contextualizeBaseTableColumn` = `<prefix>.pg.<col>`,
`contextualizeField` = `<prefix>.f.<field>`, computed-field inputs `<prefix>.ai.<aggfield>.<field>`.)

- Top-level object: `json_build_object` of the requested fields in selection order (`aggregate`,
  `nodes`, `__typename`; aliases allowed, `nodes` may appear multiple times under different aliases).
- `count`:
  - no args → `COUNT(*)`;
  - `count(columns: [a,b])` → `COUNT(("_root"."root.pg.a", "_root"."root.pg.b"))` (`CTSimple`, row
    constructor counts rows whose row value is non-null);
  - `count(columns: [a], distinct: true)` → `COUNT(DISTINCT ("_root"."root.pg.a"))`;
  - `distinct: true` without `columns` → `COUNT(*)`.
- Operators: numeric columns `sum, avg, stddev, stddev_samp, stddev_pop, variance, var_samp, var_pop`;
  comparable columns `max, min` (`numericAggOperators`/`comparisonAggOperators` in
  `GraphQL/Schema/Common.hs`). Each produces `json_build_object('<col key>', <op>("_root"."root.pg.col"),
  ...)`. Scalar computed fields are allowed inside operators.
- Values pass through `toJSONableExp` with the **column** type (§2.3). `avg` of integer returns numeric
  with scale, e.g. raw JSON `2.0000000000000000`.
- With `order_by`, the `nodes` `json_agg` is ordered; `aggregate` ignores ordering.
- `limit`/`offset` restrict the rows **both** aggregate and nodes see (applied at base or node per §3.5).
- Permission limit: §3.7 (nodes only).
- Aggregates over array relationships (`author { articles_aggregate { ... } }`) are the same structure in a
  `LEFT OUTER JOIN LATERAL`.
- `allow_aggregations: false` simply removes the fields from the schema (schema doc).

---

## 5. Parameterisation, session variables, prepared statements

### 5.1 Two preparation modes (`Backends/Postgres/Execute/Prepare.hs`)

| Used by | Mode | GraphQL values | Session variables | Whole session JSON |
|---|---|---|---|---|
| Queries (`pgDBQueryPlan`), function mutations (`convertFunction`), remote-relationship DB queries | `prepareWithPlan` | `($n)::<type>` placeholders, n ≥ 2 | `(($1->>'x-hasura-user-id'))::<type>` | `$1` |
| Insert/update/delete mutations (`convertInsert/Update/Delete`), `/v1/graphql/explain` | `prepareWithoutPlan` | inline literal `('<text>')::<type>` | inline literal `('<value>')::<type>` | inline `'<json text>'` (untyped literal) |

`prepareWithPlan` details:
- `$1` is **always** the session variables object (`withUserVars`), sent as a `json`-typed parameter
  (`PG.ViaJSON` → OID json). Keys are lower-cased session variable names
  (`{"x-hasura-role":"user","x-hasura-user-id":"1"}`), values strings. `$1` is appended to the
  argument list (`mkCurPlanTx`) even when the SQL never references it — a Rust engine using
  `tokio-postgres` must still bind it (or renumber) so the parameter count matches the statement.
  This is the **only** way queries see session variables: queries do **not** run `SET LOCAL
  "hasura.user"` (`runQueryTx` → `NoTxRead`, no transaction block; verified in
  `Instances/Transport.hs` `runPGQuery`, `Connection/MonadTx.hs`).
- Session variable reference: `SEOpApp "->>" [$1, 'x-hasura-user-id']` then `withTypeAnn`:
  rendered `(($1->>'x-hasura-user-id'))::integer` (array-typed session vars, e.g. `{1,3,4}` →
  `::integer[]`; geometry/geography-typed ones additionally wrapped in `ST_GeomFromGeoJSON(...)`
  before the cast, `SQL/Value.hs` `withTypeAnn`/`withConstructorFn`). Type = column type the variable
  is compared with / assigned to.
  Missing session variable → error **400 `not-found`**, message `missing session variable:
  "x-hasura-user-id"` (lower-cased, double-quoted), path `$` (raised while preparing, before SQL).
- GraphQL variables: a variable used several times maps to **one** placeholder number
  (`External <name>`); inline literals and list values each get a fresh number (`FreshVar`) — note
  `_in`/`_nin` lists are always `FreshVar` even when supplied via a GraphQL variable
  (`mkListParameter` in `Instances/Schema.hs`); internal names (`Internal`) are deduplicated separately. Numbering starts at 2 in order of first appearance
  during IR traversal.
- Placeholder rendering: `toPrepParam` → `($n)::<scalar type>`; geometry/geography wrapped in
  `ST_GeomFromGeoJSON($n)` then cast; raster `ST_RastFromHexWKB`.
- Parameter encoding: binary for ints/floats/numeric/bool/text/date/timestamps/json(b) etc., arrays as text
  array literals (`buildArrayLiteral`, elements JSON-quoted), `NULL` with type OID. A Rust implementation
  may send every parameter as **text** with the explicit SQL cast (the cast already exists) — semantics
  are equivalent and much simpler.
- Effect: the SQL text depends only on query shape + role + session-variable *names*, not values, so
  prepared statements are reusable across users.

`prepareWithoutPlan` details (mutations; `toTxtValue` in `Translate/Column.hs`):
`(<constructor>('<text encoding>'))::<type>`, e.g. `('Author 1')::text`, `('42')::integer`,
`(ST_GeomFromGeoJSON('{"type":"Point",...}'))::geometry`, NULL → `NULL` (untyped inside the cast:
`(NULL)::text`). Text encoding: `txtEncodedVal` (`SQL/Value.hs`) — arrays as `{...}` literals, json
values serialised, timestamps ISO; numbers use Haskell `show` (e.g. a `numeric` may render as
`'1.0e-2'`, which Postgres accepts); `money` with 2 fixed decimals. String literal quoting `pgFmtLit`
(`SQL/Types.hs`), applied in this exact order:
1. **truncate the text at the first NUL character** (`trimNullChars = takeWhile (/= '\0')`) — so a
   string value containing `\u0000` is silently cut in literal-mode mutations (and in `SET LOCAL
   "hasura.user"`);
2. wrap in single quotes, doubling every `'` → `''`;
3. double every backslash `\` → `\\`, and if the text contained any backslash prefix the literal with
   `E` (`E'a\\b'`), otherwise leave it as a standard literal.
A Rust engine that parameterises mutation values avoids this entirely; if it keeps literals it must
produce an equivalent literal (the NUL truncation is an observable quirk — reproduce only if parity is
wanted, otherwise a parameterised NUL is rejected by Postgres, presumably 22021 → `data-exception` (inferred)).

### 5.2 Prepared statements & plan caching

- Execution uses libpq `PQexecParams` (unnamed statement) **or** named prepared statements when the
  connection allows preparing (`pg-client` `execQuery`: `allowPrepare && preparable`).
  - Queries call `rawQE ... True` (preparable). Insert/update/delete mutations call `rawQE ... False`
    (never prepared — Note [Prepared statements in Mutations] in `Execute/Mutation.hs`: their SQL embeds
    literal values). **Function mutations** (`convertFunction` → `mkCurPlanTx`) call `rawQE ... True`,
    i.e. they *are* preparable. `BEGIN`/`COMMIT`/`ABORT` are also sent as preparable; `SET LOCAL`
    statements are not.
  - `allowPrepare` per source: metadata `configuration.connection_info.use_prepared_statements`
    (**default `false`** when the key is absent — `FromJSON PostgresSourceConnInfo` in
    `Connection/Settings.hs`). `HASURA_GRAPHQL_USE_PREPARED_STATEMENTS` / `--use-prepared-statements`
    (default `true`, `Serve.hs` `pgUsePreparedStatementsOption`) only feeds the `default` source config
    built from `HASURA_GRAPHQL_DATABASE_URL` (`App.hs` `initBasicConnectionInfo` → `mkSourceConfig`),
    which is **written into the metadata once**, when the catalog is first initialised
    (`Server/Migrate.hs` `initialize`) or migrated from a pre-v2 catalog. After that the persisted
    metadata value (`"use_prepared_statements": true` for such a source) is what counts; changing the
    env var later has no effect on an existing project. Projects whose metadata was written by the CLI
    usually omit the key ⇒ `false`.
  - Per-connection statement cache: key = (SQL text, param OIDs) → generated name `"0"`, `"1"`, ...
    (`Connection.hs` `prepare`); cache is cleared when the connection is reset. No LRU/eviction.
- There is **no GraphQL→SQL plan cache** in v2 OSS: `--query-plan-cache-size` /
  `HASURA_GRAPHQL_QUERY_PLAN_CACHE_SIZE` is parsed and ignored ("DEPRECATED: value ignored"). Every
  request is parsed, validated and translated anew. (A Rust implementation is free to add a cache keyed
  by (role, normalized query, operation name, variable *shape*) — but values that change SQL shape
  (limit/offset are inlined literals; `_in` list sizes do *not* change shape) must be in the key.)
- `/v1/graphql/explain` runs `EXPLAIN <sql>` (never `ANALYZE`) with values inlined (`pgDBQueryExplain`);
  admin-only (HTTP doc).

### 5.3 What the role sees in SQL

Permission filters are inlined as SQL (not as views/RLS); preset values and check expressions too. The
SQL is generated per role; session variable *values* only enter via `$1` (queries) or literals
(mutations).

---

## 6. Mutations

Sources: `Translate/Insert.hs`, `Translate/Update.hs`, `Translate/Delete.hs`, `Translate/Returning.hs`,
`Translate/Mutation.hs`, `Execute/Mutation.hs`, `Execute/Insert.hs`, `Instances/Execute.hs`,
`GraphQL/Schema/Mutation.hs`, `GraphQL/Schema/Update*.hs`, `Backends/Postgres/Schema/OnConflict.hs`.

### 6.1 Output expression (all mutation kinds) — Note [Mutation output expression]

```sql
WITH "_mra__<table>" AS (<INSERT ... | UPDATE ... | DELETE ...> RETURNING *[, (<check>)::boolean AS "check__constraint"]),
     "_aca__<table>" AS (SELECT "col1", "col2", ... FROM "_mra__<table>")   -- all columns, ordinal order
SELECT json_build_object('affected_rows', (SELECT COUNT(*) FROM "_aca__<table>"),
                         'returning', (SELECT coalesce(json_agg("root"), '[]') AS "root" FROM (
                                          SELECT row_to_json((SELECT "_e" FROM (SELECT "_root.base"."id" AS "id", ...) AS "_e")) AS "root"
                                          FROM (SELECT * FROM "_aca__<table>" WHERE ('true')) AS "_root.base") AS "_root")),
       (SELECT coalesce(bool_and("check__constraint"), 'true') FROM "_mra__<table>")   -- only insert/update
```

- `<table>` = `snakeCaseQualifiedObject` (`schema_table`, bare `table` for `public`).
- `MOutMultirowFields` (`insert_x`, `update_x`, `delete_x`, each element of `update_x_many`):
  `json_build_object` of requested fields in selection order: `affected_rows` → `(SELECT COUNT(*) FROM
  aca)`; `returning` → list select over the CTE; `__typename` → literal.
- `MOutSinglerowObject` (`insert_x_one`, `update_x_by_pk`, `delete_x_by_pk`): single-object select over
  the CTE (`coalesce((json_agg("root")->0), 'null')`) → `null` if no row affected.
- **Returning rows are not filtered by the select permission row filter** (`TablePerm annBoolExpTrue
  Nothing` in `mkMutFldExp`/`mkMutationOutputExp`); only column-level select permissions shape the
  `returning` type, and relationships inside returning do apply their targets' filters. `returning`
  exists only if the role has select permission on the table; `_by_pk`/`_one` mutation fields require
  select permission too (schema doc). **Verified** in source: `mutationSelectionSet`
  (`GraphQL/Schema/Mutation.hs`) only checks that a select permission *exists*
  (`_permissions <- tableSelectPermissions ...`, value unused) and builds `returning` from
  `tableSelectionList` (columns/relationships only); `mkMutFldExp` (`MRet`), the
  `MOutSinglerowObject` branch of `mkMutationOutputExp` and `mutateAndFetchCols` all use
  `TablePerm annBoolExpTrue Nothing` (no row filter, no permission `limit`). Consequence: a role can
  receive in `returning` an inserted/updated/deleted row it could not `SELECT` afterwards (e.g. an
  update that moves a row out of its select filter still returns it). The select permission's
  `limit` is not applied to `returning` either. Must be replicated exactly.
- `affected_rows` counts rows returned by the DML (`RETURNING`) — for upsert `DO NOTHING` conflicts not
  counted; for nested inserts the pre-computed total across all tables (§6.4).

**Nested fields in returning** (`hasNestedFld`: any object/array relationship in `returning` or in the
single-row output) — two statements (`mutateAndSel`), because a data-modifying CTE's effects are not
visible to other parts of the same statement:
1. `mutateAndFetchCols`: `WITH "_mutres__<schema.table>" AS (<DML> RETURNING * [, check]) SELECT
   json_build_object('affected_rows', (SELECT COUNT(*) FROM cte), 'returning_columns', <list of all
   columns cast ::text>) [, <check bool_and>]`.
2. Re-select from the returned values cast back to the table row type and evaluate the requested output:
   `... FROM (SELECT ("_row"::"public"."author").* FROM (VALUES (('1')::integer, ('x')::text), ...) AS "_row")`
   (`mkSelectExpFromColumnValues`; columns in ordinal order; zero rows →
   `SELECT * FROM "public"."author" WHERE 'false'`). Relationship subqueries now see the committed-in-tx
   data (inserted children etc.).

### 6.2 Permission `check` enforcement

There is **no** `hdb_catalog.check_violation` function in v2 Postgres DML; the check is computed as a
boolean column of the `RETURNING` clause and verified in the engine:

- Insert: `RETURNING *, (<insert check>)::boolean AS "check__constraint"`.
- Upsert with an update permission `check` (role has update permission with `check`) and an
  `on_conflict` clause: `CASE WHEN ("public"."author"."xmax") = (0) THEN (<insert check>)::boolean ELSE
  (<update check>)::boolean END AS "check__constraint"` (`insertOrUpdateCheckExpr`; `xmax = 0` ⇔ row was
  inserted rather than updated). Without update check → insert check only.
- Update: `RETURNING *, (<update check>)::boolean AS "check__constraint"` (`'true'` if none).
- Delete: no check.
- Check expressions reference the table by full name (`"public"."author"."col"`) — valid in RETURNING —
  and can contain relationship EXISTS subqueries (which see the pre-statement snapshot for other tables
  but the new row itself via the qualifier).
- Final select appends `(SELECT coalesce(bool_and("check__constraint"), 'true') FROM "_mra__<t>")`; if
  `false` → **error 400 `permission-error`, message `check constraint of an insert/update permission
  has failed`** (`withCheckPermission` in `Backends/Postgres/Execute/Mutation.hs`), the transaction is
  aborted. In the two-statement nested-returning path (§6.1) the check is evaluated on the first
  statement (`mutateAndFetchCols`); the second (`MCSelectValues`) has no check column. In nested
  inserts every per-object/per-relationship insert carries its own check and fails at its own path.
  Paths: insert → `$.selectionSet.<root alias>.args.objects` (`.args.object` for `_one`; nested:
  `...objects[0].<rel>.data[...]`); update/delete → `$`. Fixtures:
  `tests-py/queries/graphql_mutation/insert/permissions/user_insert_account_fail.yaml`,
  `.../update/permissions/user_cannot_publish.yaml`.
- Postgres `CHECK` constraint violation (23514) is a different error: `permission-error`
  `Check constraint violation. <pg message>` (§9).

### 6.3 Insert (`insert_x(objects, on_conflict)`, `insert_x_one(object, on_conflict)`)

Flat insert (no relationship data in any object) → **one** statement for all rows (`withoutRelsInsert`):

```sql
WITH "_mra__author" AS (
  INSERT INTO "public"."author" ( "name", "id" ) VALUES (('A1')::text, DEFAULT), (('A2')::text, ('7')::integer)
  [ON CONFLICT ...]
  RETURNING * , (<check>)::boolean AS "check__constraint"),
"_aca__author" AS (SELECT "id", "name" FROM "_mra__author")
SELECT json_build_object(...), (SELECT coalesce(bool_and("check__constraint"), 'true') FROM "_mra__author")
```

- Column set = union of all objects' columns + insert-permission **presets** (`set`, may reference
  session variables) + parent-determined columns; rows missing a column get `DEFAULT`
  (`HashMap.homogenise columnDefaultValue`). Column order = HashMap order (irrelevant).
- Presets override nothing user-visible: preset columns are removed from the insert input type (schema
  doc), values from presets are inlined literals.
- No columns at all → `INSERT INTO t VALUES (DEFAULT), (DEFAULT) ...` (`SQLInsert` ToSQL).
- `objects: []` → no SQL; returns `{"affected_rows":0,"returning":[]}` (`buildEmptyMutResp`; single-row
  output would be `{}` — unreachable since `object` is required).
- Views: insert allowed if `viIsInsertable`; no `ON CONFLICT`/xmax for views (no unique constraints).

### 6.4 Nested inserts (`Execute/Insert.hs`)

If *any* object has relationship data → per-object execution (`withRelsInsert` → `insertObject` for
each object, in order, paths `objects[i]`):

1. **Before-parent object relationships** (`riInsertOrder == BeforeParent` — the default for FK-based
   object relationships; `insertion_order: before_parent`) are inserted first, recursively
   (`insertObjRel`, path `<rel>.data`). Their returned columns supply the parent's mapped columns.
   If that insert affects zero rows (e.g. `on_conflict` DO NOTHING / where false) → **400
   `not-supported`** `cannot proceed to insert object relation "<rel>" since insert to table
   "<table>" affects zero rows`.
2. Parent row: one `INSERT ... VALUES (<single row>)` (`mkInsertQ`) via `mutateAndFetchCols` (returns
   all columns as text + check). More than one row returned → 500 `more than one row returned`.
3. **After-parent** relationships: array relationships and object relationships with
   `insertion_order: after_parent` (converted to array inserts) are inserted with the parent's
   returned mapping columns as additional columns (`insertArrRel` → `insertMultipleObjects`
   recursively; batched per relationship when the children have no further nesting).
   Parent affected zero rows → 400 `not-supported` `cannot proceed to insert array relations since
   insert to table "<table>" affects zero rows`.
4. `affected_rows` = Σ rows inserted in **all** tables (object rels + parent + array rels).
5. Finally the `returning`/single output is evaluated with `MCSelectValues` over the returned parent rows
   with pre-computed `affected_rows` (separate statement, see §6.1).

Validation errors (400 `validation-failed`, raised before SQL for that object):
- a child column also provided by the parent mapping: `cannot insert "author_id" columns as their values
  are already being determined by parent insert` (path `...objects[0].articles.data[0]`);
- object relationship whose mapping columns are also given explicitly (before-parent only): `cannot
  insert object relationship "author" as "author_id" column values are already determined` (path
  `...objects[0].author`).
  (Fixtures: `tests-py/queries/graphql_mutation/insert/nested/*_fail.yaml`.)

Each table's own insert permission (presets, check, columns) applies at its level.

### 6.5 Upsert (`on_conflict`)

`on_conflict: {constraint: <table>_constraint enum, update_columns: [<cols>] = [], where: <bool_exp>}`
(`Backends/Postgres/Schema/OnConflict.hs`, `Translate/Insert.hs` `toSQLConflict`):

- `update_columns` empty → `ON CONFLICT ON CONSTRAINT "c" DO NOTHING`.
- non-empty → `ON CONFLICT ON CONSTRAINT "c" DO UPDATE SET "col" = EXCLUDED."col", ..., "preset_col" =
  <update preset value> WHERE (<update permission filter> AND <user where>)`
  (`buildUpsertSetExp`: update-permission presets `set` override `EXCLUDED`; filter references the
  existing row as `"public"."author"."col"`).
- `on_conflict` is only available if the table has unique/PK constraints; the role needs an update
  permission for non-empty `update_columns` (the enum lists the role's updatable columns; when none, a
  placeholder enum value keeps the type valid, issue #6804/#8260).
- Check: see §6.2 (`xmax` CASE).
- Wrong/missing constraint at runtime (e.g. 42P10) → `constraint-error` `there is no unique or exclusion
  constraint on target column(s)` (§9).

### 6.6 Update (`update_x`, `update_x_by_pk`, `update_x_many`)

```sql
WITH "_mra__author" AS (
  UPDATE "public"."author"
  SET "name" = ('Cory')::text, "score" = ("score" + (('1')::integer)::integer)
  WHERE (<update perm filter AND user where | pk equality>)
  RETURNING * , (<update check>)::boolean AS "check__constraint"), ...
```
(`Translate/Update.hs`; `SET` items comma-joined without space; `WHERE` simplified.)

Operators (`pgkParseUpdateOperators`, `expandOperator`):

| Operator | SET expression |
|---|---|
| `_set: {c: v}` | `"c" = v` |
| `_inc: {c: v}` (numeric columns) | `"c" = ("c" + (v)::<column type>)` (`numeric` if type unknown) |
| `_append: {c: v}` (jsonb) | `"c" = ("c" \|\| (v)::jsonb)` |
| `_prepend: {c: v}` | `"c" = ((v)::jsonb \|\| "c")` |
| `_delete_key: {c: "k"}` | `"c" = ("c" - (v)::text)` |
| `_delete_elem: {c: 2}` | `"c" = ("c" - (v)::integer)` |
| `_delete_at_path: {c: ["a","b"]}` | `"c" = ("c" #- (ARRAY[('a')::text, ('b')::text])::text[])` |
| update-permission presets | `"c" = <preset>` merged in |

- A column may appear in only one operator (including presets): parse error `Column found in multiple
  operators: ...` (`mergeDisjoint`).
- No operator and no presets → no SQL; response `{"affected_rows":0,"returning":[]}` for `update_x`
  (`buildEmptyMutResp`), `{}` for `update_x_by_pk` (quirk: `MOutSinglerowObject` empty response is an
  empty object, not `null`) **(source-verified; no fixture)**; the multirow empty response follows
  selection order and includes `__typename` literals (`MExp`). The check happens in `convertUpdate`
  (`Instances/Execute.hs`) via `updateVariantIsEmpty`; no transaction work beyond `BEGIN`/`SET LOCAL`
  happens for that field. `update_x_many` with **all** batches empty → `[]`. If only *some* batches are
  empty, every batch (including the empty ones) is translated, producing an `UPDATE ... SET  WHERE`
  with an empty SET list → Postgres syntax error → `unexpected` `database query error` **(inferred
  from `Translate/Update.hs`; not covered by a fixture)** — a Rust engine may choose to return the
  empty response for those batches instead.
- `update_x_by_pk(pk_columns: {...}, ...)`: where = pk equality (non-nullable `=`); output single object;
  no matching row → `null` (fixture `update/basic/author_by_pk_null.yaml`).
- `update_x_many(updates: [{where, _set, _inc, ...}, ...])` (present by default; hidden when
  `HASURA_GRAPHQL_EXPERIMENTAL_FEATURES` contains `hide_update_many_fields` — `GraphQL/Schema.hs`
  `soIncludeUpdateManyFields`, `Server/Types.hs` `EFHideUpdateManyFields`): each batch is a **separate
  statement executed sequentially in list order** in the same transaction (`MultiUpdate` →
  `traverse runCTE`); later batches see earlier batches' effects. Result: JSON array of mutation
  responses, one per batch, in order.
- Views: allowed if `viIsUpdatable`.

### 6.7 Delete (`delete_x(where)`, `delete_x_by_pk(pk...)`)

```sql
WITH "_mra__author" AS (DELETE FROM "public"."author" WHERE (<delete perm filter AND where>) RETURNING *),
     "_aca__author" AS (SELECT "id", "name" FROM "_mra__author")
SELECT json_build_object('affected_rows', (SELECT COUNT(*) FROM "_aca__author"), 'returning', ...)
```
No check column. `delete_x_by_pk` arguments are the PK columns directly (not wrapped in `pk_columns`);
returns `null` if nothing deleted.
Quirk: for views, `delete_x`/`delete_x_by_pk` are gated on `viIsInsertable` (not a deletable flag) —
`guard $ isMutable viIsInsertable viewInfo` in `deleteFromTable`/`deleteFromTableByPk`
(`GraphQL/Schema/Mutation.hs`); schema doc owns this.

### 6.8 `where` is required

`update_x` / `delete_x` require `where` (non-null GraphQL argument; `{}` deletes/updates all permitted
rows). Schema doc owns argument shapes.

### 6.9 Backend-only and other permission flags

`backend_only` insert/update/delete permissions are exposed only when the request is `Backend` scenario
(admin secret + `x-hasura-use-backend-only-permissions: true`) — schema/auth doc.

### 6.10 Input validation webhooks

If the permission has `validate_input` (http), the engine POSTs `{version:1, session_variables, role,
data:{input:[...]}}` before any SQL (during planning, `convertInsert/Update/Delete`). 2xx → OK;
400 with `{"message": m}` → 400 `validation-failed` with message `m`; 400 with an undecodable body →
500 `unexpected` `received invalid response from input validation webhook`; any other status → 500
`unexpected` `internal error when validating input data`; HTTP exception → 500 `unexpected` `http
exception when validating input data` (`Execute/Mutation.hs` `validateMutation`). Detailed in permissions doc.

---

## 7. Transactions, isolation, connections

### 7.1 Queries

- Executed with `runQueryTx` → `PGExecCtxInfo NoTxRead` → `PG.runTx'`: **no `BEGIN`/`COMMIT`**, a
  single statement in autocommit mode (`Connection/MonadTx.hs`, `Execute/Types.hs`).
- **`SET LOCAL "hasura.user"` is NOT executed for queries.** Functions/views that read
  `current_setting('hasura.user')` only see it inside mutations; for queries use the function
  `session_argument` (receives `$1`).
- Each root field = separate statement, possibly on different pooled connections.
- Isolation level of the source does not apply (statement-level snapshot under the server default).

### 7.2 Mutations

- Each Postgres mutation root field runs in `runTxWithCtxAndUserInfo ... (Tx PG.ReadWrite Nothing)`:

```sql
BEGIN ISOLATION LEVEL READ COMMITTED READ WRITE      -- source isolation_level: read-committed (default) | repeatable-read | serializable
[SET LOCAL "hasura.tracecontext" = '{...}']           -- first, and only when a trace context exists (W3C/B3 ids)
SET LOCAL "hasura.user" = '{"x-hasura-role":"user","x-hasura-user-id":"1"}'
<mutation statement(s)>
COMMIT                                                -- on any error: ABORT
```
  (`pg-client` `Pool.hs` `beginTx` text: `"BEGIN " <> show isolation <> " " <> show access`;
  `setHeadersTx`, `setTraceContextInTx`; order from `runTxWithCtxAndUserInfo` =
  `withTraceContext ctx $ withUserInfo userInfo tx`.) The `hasura.user` value is the session-variables
  JSON (lower-cased keys) rendered as a `pgFmtLit` literal (§5.1). `hasura.user` is what event triggers
  record as `session_variables`. The isolation level is the source's `isolation_level`
  (`Tx PG.ReadWrite Nothing` → `defaultIsoLevel` in `mkPGExecCtx`, `Execute/Types.hs`).
- **Backwards-compatible transaction optimisation** (`Transport/HTTP.hs` `coalescePostgresMutations`,
  also WebSocket): if **every** root field of the mutation operation is an `ExecStepDB` on the **same
  vanilla-Postgres source** (same resolved connection template) and **none has remote joins**, all are
  executed sequentially inside **one** transaction (`runPGMutationTransaction`), so a failure rolls back
  all. Fixture: `tests-py/queries/graphql_mutation/transactions/transaction_revert_http.yaml`
  (second mutation fails with `constraint-violation` → first insert is rolled back; response has only
  `errors`).
  Exact guard (`coalescePostgresMutations`): the *first* step must be an `ExecStepDB` that unpacks as
  `'Postgres 'Vanilla`; then *every* step must be `ExecStepDB`, vanilla Postgres, same source name,
  same resolved connection template, `remoteJoins == Nothing`; any other step kind makes the whole
  function return `Nothing`. A single-root-field Postgres mutation also takes this path.
- Otherwise (any action, remote schema, a root-level `__typename` (`ExecStepRaw` — source-verified:
  the `_ -> Nothing` branch of the guard), different sources, remote joins, Citus/Cockroach): each root field runs in its own
  transaction sequentially; earlier committed mutations stay committed when a later one fails, but the
  response is still errors-only.
- All plans (including IR→SQL and validation webhooks) are built before the first mutation executes.
- Nested inserts / `update_many` batches / two-step nested returning all run in the same transaction as
  their root field.

### 7.3 Mixing rules

- One operation per request; a `mutation` operation's root type only contains mutation fields (+
  `__typename`), a `query` only query fields. So queries and mutations never share a transaction.
- Batched HTTP requests (JSON array of operations) are executed independently and sequentially; each has
  its own response element (`select_query_batching_with_mutation.yaml`).
- Read-only sources/replicas: a mutation against a read-only DB fails with `unexpected` `database query
  error` (`graphql_query/read_only_source/update_query_aves.yaml`).

### 7.4 Read replicas (EE/Cloud only)

`read_replicas` in source config is accepted by the metadata codec but **ignored by OSS**
(`App.hs` `mkPgSourceResolver` builds only the primary pool; `mkPGExecCtx` has no replica routing). In
EE, queries (`NoTxRead`) and subscriptions go to replicas; mutations to primary. Rust CE target: ignore
(optionally implement later behind a flag).

---

## 8. `@cached` directive

- Directive `@cached(ttl: Int = 60, refresh: Boolean = false)` on `query` operations is **defined and
  validated in OSS** (`lib/schema-parsers/src/Hasura/GraphQL/Parser/Directives.hs`; description
  "whether this query should be cached (Hasura Cloud only)"), so queries using it don't fail.
- OSS `cacheLookup` always returns `ResponseUncached Nothing` (`App.hs` `instance MonadExecuteQuery
  AppM`) — no caching, no cache headers. EE/Cloud: Redis-backed response cache keyed by query, variables,
  role & session vars; response headers `Cache-Control: max-age=<ttl>`, `X-Hasura-Query-Cache-Key`,
  `X-Hasura-Query-Family-Cache-Key`; `warning: 199 - cache-store-size-limit-exceeded` etc. on store
  failure (`Transport/HTTP.hs`). **EE-only**; Rust: accept & ignore (optionally implement an in-memory
  cache later).
- Using `@cached` on mutation/subscription → parse error `directive "cached" is not allowed on a
  mutation` (`parseDirectives`).

---

## 9. Postgres error → GraphQL error mapping

### 9.1 Classification (`Backends/Postgres/Execute/Types.hs` `dmlTxErrorHandler` / `mkTxErrorHandler`,
`SQL/Error.hs` `pgErrorType`)

All GraphQL query/mutation statements use `dmlTxErrorHandler`. A Postgres error is "expected" (shown to
users) only for the classes below; anything else is an internal error.

| SQLSTATE | Condition | `extensions.code` | `message` |
|---|---|---|---|
| 23001 | restrict_violation | `constraint-violation` | `Can not delete or update due to data being referred. ` + pg message |
| 23502 | not_null_violation | `constraint-violation` | `Not-NULL violation. ` + pg message |
| 23503 | foreign_key_violation | `constraint-violation` | `Foreign key violation. ` + pg message |
| 23505 | unique_violation | `constraint-violation` | `Uniqueness violation. ` + pg message |
| 23514 | check_violation | **`permission-error`** | `Check constraint violation. ` + pg message |
| 23P01 | exclusion_violation | `constraint-violation` | `Exclusion violation. ` + pg message |
| 23xxx other | integrity constraint | `constraint-violation` | pg message |
| 22025 | invalid_escape_sequence | `bad-request` | pg message |
| 22xxx other (22P02 invalid text repr., 22007, 22023, 22003 out of range, 22012 division by zero, ...) | data exception | `data-exception` | pg message |
| 42704 | undefined_object | `constraint-error` | pg message |
| 42P10 | invalid_column_reference | `constraint-error` | `there is no unique or exclusion constraint on target column(s)` |
| everything else (42xxx other, 40001, P0001 `RAISE EXCEPTION`, 25006 read-only tx, 57014 statement timeout, XX000, ...) | unexpected | `unexpected` (HTTP 500 class) | `database query error` |

- The pg message is `edMessage` only (the primary message; no DETAIL/HINT).
  Example: `Uniqueness violation. duplicate key value violates unique constraint "author_name_key"`.
- Note the `PGTransactionRollback` (40001 → `concurrent-update` "serialization failure due to concurrent
  update") branch is only enabled in `defaultTxErrorHandler` (metadata/internal operations), **not** for
  GraphQL DML → serialization failures in mutations surface as `unexpected` `database query error`.
- Errors raised by user `RAISE EXCEPTION` in triggers/functions (P0001) → `unexpected`
  `database query error`; the real message is only visible in `extensions.internal` (admin / dev mode).
- `BEGIN`/`COMMIT`/`ABORT` failures use `FromPGTxErr QErr`: 500 `postgres-error` `postgres tx error`
  with internal. Important consequence: errors raised **at `COMMIT`** — e.g. a `DEFERRABLE INITIALLY
  DEFERRED` foreign-key/unique violation, or a serialization failure detected at commit — surface as
  `postgres-error` `postgres tx error`, **not** `constraint-violation`. `SET LOCAL` statements use
  `defaultTxErrorHandler` (same classification as §9.1 plus 40001 → `concurrent-update`). Connection failures: `postgres-error` `connection error`; `too many clients` →
  `postgres-max-connections-error` `max connections reached on postgres`
  (`lib/hasura-base/src/Hasura/Base/Error.hs`).

### 9.2 Internal details

For unexpected errors `qeInternal = ExtraInternal (toJSON PGTxErr)`:

```json
{"statement": "<sql text>", "arguments": ["(Oid 25,Just (\"...\",Binary))", ...], "prepared": true,
 "error": {"exec_status": "FatalError", "hint": null, "message": "syntax error",
           "status_code": "42601", "description": null}}
```
(fixture `tests-py/queries/v1/select/boolexp/ltree/select_path_where_matches_ltxtquery.yaml`; arguments
are Haskell `show` of the prep args — a Rust engine can emit its own representation.)

In the GraphQL envelope (`encodeGQLErr`): `{"message": <msg>, "extensions": {"path": <path>, "code":
<code>[, "internal": {...}]}}`. `internal` is included when:
`HASURA_GRAPHQL_DEV_MODE=true` → all roles; else `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS` (default `true`)
→ admin role only; else never (`Server/Init/Config.hs` `shouldIncludeInternal`, `App/State.hs`).

### 9.3 `extensions.path`

- Query statements & update/delete: `$` (no path context) — e.g. `select_query_invalid_escape_sequence.yaml`:
  `{code: bad-request, path: $, message: "LIKE pattern must not end with escape character"}`.
- Inserts: `$.selectionSet.<root field response key>.args.objects` (or `.args.object` for `_one`); nested
  per-row errors get `[i]`, `.<rel>`, `.data`, `[j]` segments (`withPathK`/`indexedForM`). Example:
  `$.selectionSet.insert_author.args.objects` for a unique violation
  (`insert/constraints/author_unique_constraint_error.yaml`).
- HTTP status on `/v1/graphql` is 200 for all of these (envelope `{"errors": [...]}`, no `data` key) —
  see HTTP doc.

### 9.4 Other execution-time errors raised by the engine

| Situation | code | message |
|---|---|---|
| missing session variable referenced by permission/preset/session arg | `not-found` | `missing session variable: "<name>"` |
| insert/update permission check fails | `permission-error` | `check constraint of an insert/update permission has failed` (`withCheckPermission`, `Backends/Postgres/Execute/Mutation.hs` — note: the Postgres backend module, not `GraphQL/Execute/Mutation.hs`) |
| error raised at `COMMIT` (deferred constraints etc.) | `postgres-error` (500 class) | `postgres tx error` |
| nested insert parent/child affects zero rows | `not-supported` | see §6.4 |
| nested insert column conflicts | `validation-failed` | see §6.4 |
| `distinct_on` mismatch | `validation-failed` (parse error) | `"distinct_on" columns must match initial "order_by" columns` |
| integer value out of bounds (GraphQL input) | `parse-failed` | `The value 2.147483648e9 lies outside the bounds or is not an integer. Maybe it is a float, or is there integer overflow?` (path to the field) |
| mutation on operation with no mutations | `validation-failed` | `no mutations exist` |
| subscriptions over HTTP | `unexpected-payload` (`throw400 UnexpectedPayload` in `Transport/HTTP.hs`) | `subscriptions are not supported over HTTP, use websockets instead` (HTTP doc) |

---

## 10. Functions & computed fields — execution details

- Tracked function root fields as queries: prepared like queries; functions must be STABLE/IMMUTABLE to be
  query fields (VOLATILE default to mutation unless `exposed_as: query`) — metadata doc.
- Function mutation (`exposed_as: mutation`): `MDBFunction` → `convertFunction`: `prepareWithPlan`
  (params, `$1` session) executed in the mutation transaction (so `hasura.user` is also set).
- Permissions on function root fields = select permission of the returned table (filter + limit +
  columns), plus function permissions (`pg_create_function_permission` / inferred) — metadata doc.
- Computed fields: STABLE/IMMUTABLE functions with first (or named) table-row argument; optional
  `session_argument`. Their arguments appear as `args: {...}` on the field. Scalar computed fields can be
  used in `where` and `order_by` only when they have no user arguments.

---

## 11. Rust implementation guidance (non-normative)

1. Model IR → SQL as an AST (like `SQL/DML.hs`) with a renderer; keep `json_agg/row_to_json` JSON
   assembly in Postgres and stream the single-cell text result directly into the HTTP body.
2. Parameterise everything in queries (`$1` = session JSON as `json`, `$n` = values as text + cast);
   consider also parameterising mutations (upstream inlines literals only for historical reasons;
   results are identical). Inline `LIMIT/OFFSET` as integers.
3. Use `tokio-postgres` with an explicit per-connection statement cache keyed by SQL text when the
   source enables prepared statements; default off for metadata sources (`use_prepared_statements:
   false`) — but enabling it is a safe performance win (the SQL is shape-stable).
4. Mutations: one transaction per request when coalescible (§7.2); emit `BEGIN ISOLATION LEVEL ... READ
   WRITE`, `SET LOCAL "hasura.user"`, statements, `COMMIT`/`ROLLBACK`.
5. Do not run queries in explicit transactions (matches upstream & saves round trips); multiple root
   fields could be executed concurrently on different connections as an optimisation (upstream is
   sequential; results are independent anyway).
6. Error mapping table §9.1 must be exact (code + message prefix).

---

## Open questions / uncertainties

- (Resolved in review) `pgFmtLit` escaping is now documented exactly in §5.1 (incl. NUL truncation).
- `update_x_by_pk` with no operators returns `{}` per source (`buildEmptyMutResp`); reachable (the
  schema does not require an operator) but not covered by a fixture.
- (Resolved in review) a root `__typename` in a mutation operation disables single-transaction
  coalescing — source-verified (§7.2).
- `update_x_many` with a mix of empty and non-empty batches (§6.6) — inferred to fail with an SQL
  syntax error; no fixture.
- `COUNT((a, b))` semantics with multi-column `count(columns:)` relies on Postgres row-null semantics.

---

## Upstream tests to reuse

Python golden tests (`server/tests-py/queries/...`, driven by `server/tests-py/test_graphql_queries.py`,
`test_graphql_mutations.py`); skip files ending in `_mssql`, `_citus`, `_bigquery`, `_cockroach`.

| Path | Covers |
|---|---|
| `tests-py/queries/explain/` (`simple_query`, `docs_query`, `limit_*`, `orderby_array_relationship_query`, `permissions_query`) | Exact SQL shape (only if we choose explain parity; skip `*_mssql`, `*_bigquery`) |
| `tests-py/queries/graphql_query/basic/` | Basic selects, type encoding (`select_query_test_types_postgres.yaml`, needs stringify ON), batching (`select_query_batching*`), escape-sequence error, fragments |
| `tests-py/queries/graphql_query/boolexp/` | All comparison operators, jsonb/ltree/postgis ops, relationships in where, `_cast` |
| `tests-py/queries/graphql_query/order_by/`, `order_by_nulls/` | order_by incl. relationships, aggregates, nulls ordering |
| `tests-py/queries/graphql_query/limits/`, `offset/` | limit/offset incl. permission limits |
| `tests-py/queries/graphql_query/aggregations/`, `agg_perm/` | Aggregates, nodes, aliases, permission limit only on nodes, allow_aggregations |
| `tests-py/queries/graphql_query/permissions/` | Row filters, session variables, missing session variable error |
| `tests-py/queries/graphql_query/computed_fields/`, `computed_fields_row_permission_bypass/` | Scalar/table computed fields, session args, permissions on SETOF computed fields |
| `tests-py/queries/graphql_query/functions/` | Tracked functions, args, session argument, aggregate on functions |
| `tests-py/queries/graphql_query/enums/` | Enum table columns (and graphql-default uppercase) |
| `tests-py/queries/graphql_query/relay/` | Relay connections (low priority) |
| `tests-py/queries/graphql_query/read_only_source/` | `database query error` on read-only DB |
| `tests-py/queries/graphql_mutation/insert/` (`basic`, `constraints`, `nested`, `onconflict`, `permissions`, `geojson`, `views`, `nullprefixcolumn`) | Inserts, nested inserts & errors, upserts, check failures, error paths; skip `ifmatched` (MSSQL) and `*_mssql` |
| `tests-py/queries/graphql_mutation/update/` (`basic`, `jsonb`, `permissions`) | `_set/_inc`, jsonb operators, by_pk null, check failure |
| `tests-py/queries/graphql_mutation/delete/` | Deletes, constraint errors, permissions |
| `tests-py/queries/graphql_mutation/transactions/` | Multi-mutation rollback (HTTP & WS) |
| `tests-py/queries/graphql_mutation/functions/` | Volatile function mutations |
| `tests-py/queries/graphql_mutation/roles_inheritance/` | Inherited roles in mutations |
| `tests-py/queries/v1/select/boolexp/ltree/select_path_where_matches_ltxtquery.yaml` | Internal error JSON shape (RQL `/v1/query`) |
| `server/lib/api-tests/src/Test/Queries/*` (`AggregationSpec`, `DistinctSpec`, `SortSpec`, `Paginate/*`, `FilterSearch/*` incl. `AggregationPredicatesSpec`, `Simple/*`, `Relationship/PermissionRootReferenceSpec`, `VariablesAliasesFragments/*`, `CustomFunctions/PortedSpec`, `NestedObjectSpec`) | Postgres fixtures of the same semantics (Haskell harness; port YAML expectations); `NativeQueries/ProOnlySpec`, `StoredProceduresSpec` are non-Postgres/EE — skip |
| `server/lib/api-tests/src/Test/Queries/PermissionsKnowAboutColumnNullabilitySpec.hs` | Null-safe `_eq` in permissions only for nullable columns (explain SQL) |
| `server/lib/api-tests/src/Test/Mutations/*` (`Insert/*`, `Update/*`, `Delete/*`, `Upsert/OnConflictSpec`, `MultiplePerRequest/UpdateManySpec`) | Mutation semantics incl. update_many ordering |
| `server/lib/api-tests/src/Test/Regression/` `ObjectRelationshipsLimit7936Spec`, `LongIdentifiers3796Spec`, `InsertOnConflict8260Spec`, `ArrayLiteralTextEncodingSpec`, `NullsOrderParsing8780Spec`, `RootExistElementAliasing9457Spec`, `MultiColumnObjectRelationshipsSpec`, `OptimizePermissionFiltersSpec`, `AggregateBoolExpConflictSpec`, `UsingTheSameFunctionForRootFieldAndComputedField8643Spec` | Regressions directly tied to SQL generation; skip `SqlServer*`, `DoNotTruncateSessionVariables8158Spec` (MSSQL) |
| `server/lib/api-tests/src/Test/Regression/MultiplexQuerySpec.hs` | `database query error` for function exceptions (subscriptions, shared with subs doc) |
