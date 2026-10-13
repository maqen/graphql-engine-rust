# 07 — Event triggers, cron triggers and one-off scheduled events

Compatibility spec for the Rust reimplementation of Hasura GraphQL Engine V2 (Postgres first).
Ground truth: the Haskell source under `server/` at the checked-out revision (catalog version
`48`, source catalog version `4`, latest tag in `server/src-rsr/catalog_versions.txt` is
`v2.51.0`). Paths below are relative to the upstream repo root. "(inferred)" means the statement
was not checked line-by-line against the source.

Related specs (written by other workers): metadata API envelope and error format, metadata
directory format and `replace_metadata`, webhook request/response transforms (shared with
actions), session variables and `SET LOCAL hasura.user` in mutations, `run_sql`. This doc
links to them by topic only.

---

## 1. Scope and priority

| Feature | Priority | Notes |
|---|---|---|
| Event trigger metadata (`event_triggers` in table YAML), `pg_create_event_trigger`, `pg_delete_event_trigger` (and legacy `/v1/query` `create_event_trigger`, `delete_event_trigger`) | **Essential** | Most V2 projects with events use these. |
| Installing / dropping / recreating the SQL trigger functions in `hdb_catalog` of each source; `hdb_catalog.event_log`, `event_invocation_logs`, `insert_event_log()` | **Essential** | Must be byte-compatible in behaviour: existing databases already contain Hasura-created functions, and existing rows must keep working. |
| Event delivery loop (fetch with `FOR UPDATE SKIP LOCKED`, HTTP delivery, retries, `Retry-After`, invocation logs, timeouts, graceful shutdown unlock) | **Essential** | |
| `webhook_from_env`, `{{ENV}}` URL templates, headers (`value` / `value_from_env`) | **Essential** | |
| `pg_invoke_event_trigger` (manual events, `enable_manual`) | Common | Used by console "Invoke" button. |
| `pg_redeliver_event` | Common | Console "redeliver". |
| `request_transform` / `response_transform` on event & cron triggers | Common | Shares the transform engine with actions. |
| Cron triggers (`cron_triggers.yaml`, `create_cron_trigger`, `delete_cron_trigger`, `get_cron_triggers`), cron event generator | **Essential** for projects that use them | |
| One-off scheduled events (`create_scheduled_event`, `delete_scheduled_event`) | Common | |
| `get_scheduled_events`, `get_scheduled_event_invocations` | Common (console) | |
| `pg_get_event_logs`, `pg_get_event_invocation_logs`, `pg_get_event_by_id` | Console-only, medium | OSS handlers exist. |
| `trigger_on_replication` | Rare | `ALTER TABLE … ENABLE ALWAYS TRIGGER`. |
| `cleanup_config` (auto cleanup of event logs), `cleanup_event_trigger_logs`, `pause_event_trigger_cleanups`, `resume_event_trigger_cleanups` | **EE-only** for the actual deletion | In OSS the field is accepted/stored and pause/resume flips the metadata flag, but `cleanup_event_trigger_logs` returns 400 `not-supported` "Event log cleanup feature is enterprise edition only" and no cleanup schedules are generated (`server/src-lib/Hasura/App.hs`, `instance MonadEventLogCleanup AppM`). Implement storage/round-trip now; deletion later as optional. |
| Prometheus metrics for events (`hasura_event_*`) | EE-only | OSS exposes EKG-ish counters only; skip for CE parity. |
| MSSQL event triggers, maintenance mode (`PreviousMMVersion` queries) | Skip | Postgres-first; maintenance mode is a Cloud/EE upgrade feature. |
| Citus / Cockroach | Skip | Event triggers are not supported there upstream either (`Hasura.Eventing.Backend` throws `not-supported` for Citus). |

---

## 2. Where things live

| Object | Database | Schema | Created by |
|---|---|---|---|
| `event_log`, `event_invocation_logs`, `hdb_event_log_cleanups`, `hdb_source_catalog_version`, functions `gen_hasura_uuid()`, `insert_event_log(...)` | **each source DB** that has ≥1 event trigger | `hdb_catalog` | `server/src-rsr/init_pg_source.sql` (+ `pg_source_migrations/*.sql`) |
| Per-trigger PL/pgSQL functions `hdb_catalog."notify_hasura_<trigger>_<OP>"()` | source DB | `hdb_catalog` | `server/src-rsr/trigger.sql.shakespeare` via `mkTriggerFunctionQ` |
| Per-trigger SQL triggers `"notify_hasura_<trigger>_<OP>"` | source DB | on the user's table | `mkTrigger` (`server/src-lib/Hasura/Backends/Postgres/DDL/EventTrigger.hs`) |
| `hdb_cron_events`, `hdb_cron_event_invocation_logs`, `hdb_scheduled_events`, `hdb_scheduled_event_invocation_logs` | **metadata DB** | `hdb_catalog` | `server/src-rsr/initialise.sql` (+ `migrations/*.sql`) |
| Cron trigger definitions | metadata JSON (`hdb_catalog.hdb_metadata.metadata` → `cron_triggers`) | — | metadata APIs |

There is **no** `hdb_catalog.hdb_cron_triggers` table any more: it existed in catalog versions
35–42 and was dropped in `server/src-rsr/migrations/42_to_43.sql` when metadata moved to the
JSON blob. Cron trigger definitions are only in metadata.

When the default source and metadata DB are the same database, both sets of tables live in the
same `hdb_catalog` schema.

---

## 3. Event trigger metadata

### 3.1 Metadata shape (`databases/<db>/tables/<schema>_<table>.yaml` → `event_triggers`)

Parsed by `EventTriggerConf` (`server/src-lib/Hasura/RQL/Types/EventTrigger.hs`):

```yaml
event_triggers:
  - name: author_created            # required, TriggerName
    definition:                      # required, TriggerOpsDef
      enable_manual: false           # optional Bool (may be omitted)
      insert:                        # optional SubscribeOpSpec
        columns: '*'
      update:
        columns: [name, email]       # listen columns
        payload: [id, name, email]   # optional delivery columns
      delete:
        columns: '*'
    retry_conf:                      # REQUIRED in metadata (not defaulted when reading metadata)
      num_retries: 0
      interval_sec: 10
      timeout_sec: 60                # optional
    webhook: http://httpbin.org/post # exactly one of webhook / webhook_from_env
    # webhook_from_env: EVENT_WEBHOOK_URL
    headers:                         # optional
      - name: X-Api-Key
        value_from_env: API_KEY
      - name: X-Static
        value: abc
    request_transform: {...}         # optional, see transforms spec
    response_transform: {...}        # optional
    cleanup_config:                  # optional (EE behaviour)
      schedule: '0 0 * * *'
      clear_older_than: 168
      batch_size: 10000
      timeout: 60
      clean_invocation_logs: false
      paused: false
    trigger_on_replication: false    # optional; Postgres default false; omitted on export when default
```

Field rules (verified):

* `name`: non-empty text. **Metadata decoding does not validate the regex**, but
  `create_event_trigger` and (since v2.50) `replace_metadata` reject illegal names:
  * regex `^[A-Za-z]+[A-Za-z0-9_\-]*$` (`isIllegalTriggerName`);
  * length ≤ **42** (`maxTriggerNameLength`; 63 − len("notify_hasura_") − len("_INSERT")).
* `definition.insert|update|delete`: `SubscribeOpSpec { columns: "*" | [col], payload?: "*" | [col] }`.
  `columns` is required inside an op spec. `"*"` is the literal string; any other string fails with
  `only * or [] allowed`.
* `enable_manual`: optional in metadata; `create_event_trigger` always stores `Just <bool>` (defaults
  `false`), so exported metadata from the API always contains `enable_manual`.
* `retry_conf` (`RetryConf`): `num_retries` (Int, required), `interval_sec` (Int, required),
  `timeout_sec` (Int, optional; when absent the delivery uses 60 s). No validation of negatives
  (inferred: aeson Int parse only).
* `webhook`: `InputWebhook` = URL template string; `{{VAR}}` is substituted from the environment
  at schema-cache build time (`resolveWebhook`). `webhook_from_env`: env var name.
  Exactly one must be present (`create_event_trigger` parse error otherwise; at schema-build time
  `throw500 "expected webhook or webhook_from_env"` if both/none, which makes the trigger
  inconsistent).
* `headers`: list of `HeaderConf`: `{name, value}` or `{name, value_from_env}`.
  * `value` is itself a template (`{{VAR}}` substitution, `getHeaderInfosFromConf`).
  * `value_from_env` may not start with `HASURA_GRAPHQL_` (parse error
    `env variables starting with "HASURA_GRAPHQL_" are not allowed in value_from_env: <var>`).
  * both keys → `expecting only one of value or value_from_env keys`; neither →
    `expecting value or value_from_env keys`.
* `trigger_on_replication`: Bool; Postgres default `false` (`defaultTriggerOnReplication =
  Just ((), TORDisableTrigger)`, `server/src-lib/Hasura/Backends/Postgres/Instances/Types.hs`).
  Serialised only when different from default.
* `cleanup_config` (`AutoTriggerLogCleanupConfig`): `schedule` (cron, required),
  `clear_older_than` (hours, required), `batch_size` (default 10000), `timeout` (seconds, default 60),
  `clean_invocation_logs` (default false), `paused` (default false).

Env resolution failures at schema build (missing env var for `webhook_from_env` →
`environment variable 'X' not set` (code `not-found`); missing `{{X}}` in template →
`Value for environment variables not found: X` (code `unexpected`); unresolvable `{{X}}` in a header
`value` template → `template cannot be resolved: <err>` (code `not-found`, `getHeaderInfosFromConf`);
missing env var for a header `value_from_env` → `environment variable 'X' not set` (`not-found`)) mark the event trigger
**inconsistent** (`in table "public.author": in event trigger "name": ...` context prefixes
via `addTableContext` / `addTriggerContext`). The SQL trigger is not (re)created in that case.

Schema dependencies (`getTrigDefDeps`): the trigger depends on its table (`DRParent`), on every
column listed in any op's `columns` (`DRColumn`) and `payload` (`DRPayload`). Dropping such a
column via `run_sql` without `cascade` fails with the usual dependency error; column renames are
propagated into the trigger definition (`updateColumnInEventTrigger`).

### 3.2 `pg_create_event_trigger` / `create_event_trigger`

Endpoints: `POST /v1/metadata` `{"type":"pg_create_event_trigger","args":{...}}`; legacy
`POST /v1/query` `{"type":"create_event_trigger","args":{...}}` (Postgres default source only).
Args (`CreateEventTriggerQuery`, `server/src-lib/Hasura/RQL/DDL/EventTrigger.hs`):

| key | req | default | notes |
|---|---|---|---|
| `source` | no | `"default"` | |
| `name` | yes | | regex + 42 chars validated at parse time |
| `table` | yes | | `"name"` or `{schema,name}` |
| `insert`, `update`, `delete` | no | | `SubscribeOpSpec` |
| `enable_manual` | no | `false` | |
| `retry_conf` | no | `{num_retries:0, interval_sec:10, timeout_sec:60}` | |
| `webhook` / `webhook_from_env` | one of | | |
| `headers` | no | | |
| `replace` | no | `false` | |
| `request_transform`, `response_transform` | no | | |
| `cleanup_config` | no | | |
| `trigger_on_replication` | no | `false` | |

Parse-time errors (HTTP 400, code `parse-failed`, `path: "$.args"` on /v1/metadata; `"$"` on
/v1/query per `tests-py/queries/event_triggers/create-delete/*_err.yaml`):

* `only alphanumeric and underscore and hyphens allowed for name`
* `event trigger name can be at most 42 characters`
* `atleast one amongst insert/update/delete/enable_manual spec must be provided`
* `only one of webhook or webhook_from_env should be given`
* `must provide webhook or webhook_from_env`
* `found empty column specification` (`columns: []`), `found empty payload specification` (`payload: []`).
  Quirk (verified, `checkEmptyCols`): the payload check only fires when `columns` is `"*"`; with
  `columns: [a]` and `payload: []` no error is raised (payload rows then render as `{}` — inferred).

Check order matters for which message is returned: illegal name → length → no ops → webhook → empty cols.

Semantic steps (`createEventTriggerQueryMetadata`):

1. Table must be tracked (`askTableCoreInfo`), else the standard table-not-found error.
2. If `replace: true`: the trigger must exist (400 `not-exists`
   `event trigger "<name>" does not exist`) and must be on the **same table**
   (400 `not-supported` `cannot replace table or schema for trigger`).
3. Every column in each op's `columns` must exist (`assertColumnExists`). Note: `payload`
   columns are **not** checked here (only via dependencies at schema build).
4. `replace: false`: if any function `hdb_catalog.notify_hasura_<name>_{INSERT,UPDATE,DELETE}`
   exists in `pg_proc` of the source (checked by name in `hdb_catalog`) → 400 `already-exists`
   `Event trigger with name "<name>" already exists`. This makes names unique per source DB
   even across tables.
5. `replace: true`: drop SQL triggers for ops that existed before and are now absent
   (`droppedTriggerOps` → `DROP FUNCTION IF EXISTS hdb_catalog."notify_hasura_<n>_<OP>"() CASCADE`).
6. Update metadata (insert or replace in `tables[...].event_triggers`), rebuild schema cache with
   `CatalogUpdate` → the build creates/replaces the SQL triggers (§4.3), and initialises the
   source catalog if this is the first event trigger of the source (§4.1).
7. Response `{"message":"success"}`.

### 3.3 `pg_delete_event_trigger` / `delete_event_trigger`

Args `{name, source?}`. Steps (`runDeleteEventTriggerQuery`): find table of trigger (400
`not-exists` `event trigger "<name>" does not exist`), remove from metadata, rebuild, then in the
source DB in one transaction:

```sql
DROP FUNCTION IF EXISTS hdb_catalog."notify_hasura_<name>_INSERT"() CASCADE;
DROP FUNCTION IF EXISTS hdb_catalog."notify_hasura_<name>_UPDATE"() CASCADE;
DROP FUNCTION IF EXISTS hdb_catalog."notify_hasura_<name>_DELETE"() CASCADE;
UPDATE hdb_catalog.event_log SET archived = 't' WHERE trigger_name = $1;
```

(`CASCADE` on the function drops the SQL trigger on the table.) Then delete scheduled cleanups
(`DELETE FROM hdb_catalog.hdb_event_log_cleanups WHERE status='scheduled' AND trigger_name=$1`).
Response `{"message":"success"}`. Events are **archived, not deleted**.

### 3.4 Other metadata operations that touch SQL triggers

* `untrack_table` (and `pg_untrack_tables`): after rebuild, for every trigger of the table:
  drop the 3 functions + archive events (`server/src-lib/Hasura/Table/API.hs`).
* `replace_metadata` (`server/src-lib/Hasura/RQL/DDL/Metadata.hs`, `dropSourceSQLTriggers`):
  * validates **every** trigger name in the new metadata: duplicates within a source →
    400 `not-supported` `Event trigger with duplicate names not allowed: "a", "b"`; illegal names →
    400 `not-supported` `Starting in v2.50 only alphanumeric and underscore and hyphens allowed for name`;
    too long → 400 `not-supported` `event trigger name can be at most 42 characters`.
  * triggers removed from a source: drop functions + archive events;
  * triggers that moved to another table: drop all 3 ops on the old table (no archival);
  * retained triggers with ops removed: drop only those ops;
  * source removed / inconsistent: warning `Could not drop SQL triggers present in the source ...`.
* Dropping a source (`pg_drop_source`, `postDropSourceHook`, `server/src-lib/Hasura/Backends/Postgres/DDL/Source.hs`):
  if `hdb_catalog.hdb_metadata` exists in that DB (default source / metadata DB): drop each trigger's
  functions and run `server/src-rsr/drop_pg_source.sql` (drops `hdb_source_catalog_version`,
  `insert_event_log`, `event_invocation_logs`, `event_log`, `hdb_event_log_cleanups`); otherwise
  `DROP SCHEMA hdb_catalog CASCADE`.
* `run_sql` (`server/src-lib/Hasura/Backends/Postgres/DDL/RunSQL.hs`, `withMetadataCheck`): after the
  SQL and schema-cache rebuild, re-runs `mkAllTriggersQ` for **every** event trigger of every table in
  the source (CREATE OR REPLACE functions so that new/renamed columns are picked up; create the
  SQL trigger if missing). This only happens on the metadata-check path
  (`isSchemaCacheBuildRequiredRunSQL`, verified): never for `read_only: true`; otherwise when
  `check_metadata_consistency` is `true`, or — when that key is absent — when the SQL matches the
  case-insensitive regex `\balter\b|\bdrop\b|\breplace\b|\bcreate function\b|\bcomment on\b`.
  A plain `CREATE TABLE` / `INSERT` run_sql does **not** recreate triggers.
* `reload_metadata` (`recreate_event_triggers: true | [sources]`, default none): forces
  `createTableEventTrigger` for those sources (§4.3).
* `pg_dump` API strips `CREATE TRIGGER "notify_hasura_..."` lines from its output (regex in
  `server/src-lib/Hasura/Server/API/PGDump.hs`).

---

## 4. Database objects

### 4.1 Source catalog initialisation (`prepareCatalog`)

Runs during schema-cache build for a source **only if the source has ≥1 event trigger** in
metadata (`initCatalogIfNeeded`, `server/src-lib/Hasura/RQL/DDL/Schema/Cache.hs`). Skipped when
read-only mode, `HASURA_GRAPHQL_DISABLE_EVENTING`, or maintenance mode is on. Retried 3× with 10 s
delay on failure (multiple instances racing). Logic (`server/src-lib/Hasura/Backends/Postgres/DDL/Source.hs`):

1. `hdb_catalog` schema missing → `CREATE SCHEMA hdb_catalog`, ensure `pgcrypto`
   (`CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA <extensions_schema>` only if not already
   created; `extensions_schema` from source config, default `public`), run `init_pg_source.sql`,
   set source version → `RETDoNothing`.
2. schema exists but neither `hdb_source_catalog_version` nor `event_log` → run
   `init_pg_source.sql` → `RETDoNothing`.
3. `event_log` exists but no version table (pre-2.0 single-DB install) → run metadata-catalog
   migrations up to 43, create version table, run source migrations from 0.
4. otherwise → `migrateSourceCatalog`: if version < 4 run the missing
   `pg_source_migrations/<n>_to_<n+1>.sql` and return `RETRecreate` (forces trigger recreation);
   if version > 4 → 400 `not-supported` `Expected source catalog version <= 4, but the current version is N`.

Version row:

```sql
INSERT INTO hdb_catalog.hdb_source_catalog_version(version, upgraded_on)
  VALUES ($1, NOW())
ON CONFLICT ((version IS NOT NULL))
DO UPDATE SET version = $1, upgraded_on = NOW();      -- $1 = '4'
SELECT version FROM hdb_catalog.hdb_source_catalog_version;
```

### 4.2 Exact DDL — source DB (`server/src-rsr/init_pg_source.sql`, source catalog v4)

```sql
CREATE OR REPLACE FUNCTION hdb_catalog.gen_hasura_uuid() RETURNS uuid AS
'select gen_random_uuid()' LANGUAGE SQL;

CREATE TABLE hdb_catalog.hdb_source_catalog_version(
  version TEXT NOT NULL,
  upgraded_on TIMESTAMPTZ NOT NULL
);
CREATE UNIQUE INDEX hdb_source_catalog_version_one_row
ON hdb_catalog.hdb_source_catalog_version((version IS NOT NULL));

CREATE TABLE hdb_catalog.event_log
(
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  schema_name TEXT NOT NULL,
  table_name TEXT NOT NULL,
  trigger_name TEXT NOT NULL,
  payload JSONB NOT NULL,
  delivered BOOLEAN NOT NULL DEFAULT FALSE,
  error BOOLEAN NOT NULL DEFAULT FALSE,
  tries INTEGER NOT NULL DEFAULT 0,
  created_at TIMESTAMP DEFAULT NOW(),       -- NOTE: timestamp WITHOUT time zone (server local time)
  /* when locked IS NULL the event is unlocked and can be processed */
  locked TIMESTAMPTZ,
  next_retry_at TIMESTAMP,                  -- NOTE: timestamp WITHOUT time zone
  archived BOOLEAN NOT NULL DEFAULT FALSE
);
/* This powers `archiveEvents` */
CREATE INDEX ON hdb_catalog.event_log (trigger_name);     -- auto-named event_log_trigger_name_idx
/* This index powers `fetchEvents` */
CREATE INDEX event_log_fetch_events
  ON hdb_catalog.event_log (locked NULLS FIRST, next_retry_at NULLS FIRST, created_at)
  WHERE delivered = 'f'
    and error = 'f'
    and archived = 'f'
;

CREATE TABLE hdb_catalog.event_invocation_logs
(
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  trigger_name TEXT,
  event_id TEXT,                 -- NOTE: no FK (dropped in pg_source_migrations/2_to_3.sql)
  status INTEGER,
  request JSON,
  response JSON,
  created_at TIMESTAMP DEFAULT NOW()
);
CREATE INDEX ON hdb_catalog.event_invocation_logs (event_id);   -- auto-named event_invocation_logs_event_id_idx

CREATE OR REPLACE FUNCTION
  hdb_catalog.insert_event_log(schema_name text, table_name text, trigger_name text, op text, row_data json)
  RETURNS text AS $$
  DECLARE
    id text;
    payload json;
    session_variables json;
    server_version_num int;
    trace_context json;
  BEGIN
    id := gen_random_uuid();
    server_version_num := current_setting('server_version_num');
    IF server_version_num >= 90600 THEN
      -- In some cases postgres sets the setting to an empty string, which is not a valid json.
      -- NULLIF will convert the empty string to NULL.
      -- Ref: https://github.com/hasura/graphql-engine/issues/8498
      session_variables := NULLIF(current_setting('hasura.user', 't'), '');
      trace_context := NULLIF(current_setting('hasura.tracecontext', 't'), '');
    ELSE
      BEGIN
        session_variables := current_setting('hasura.user');
      EXCEPTION WHEN OTHERS THEN
                  session_variables := NULL;
      END;
      BEGIN
        trace_context := current_setting('hasura.tracecontext');
      EXCEPTION WHEN OTHERS THEN
        trace_context := NULL;
      END;
    END IF;
    payload := json_build_object(
      'op', op,
      'data', row_data,
      'session_variables', session_variables,
      'trace_context', trace_context
    );
    INSERT INTO hdb_catalog.event_log
                (id, schema_name, table_name, trigger_name, payload)
    VALUES
    (id, schema_name, table_name, trigger_name, payload);
    RETURN id;
  END;
$$ LANGUAGE plpgsql;

CREATE TABLE hdb_catalog.hdb_event_log_cleanups
(
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  trigger_name TEXT NOT NULL,
  scheduled_at TIMESTAMP NOT NULL,
  deleted_event_logs INTEGER,
  deleted_event_invocation_logs INTEGER,
  status TEXT NOT NULL,
  CHECK (status IN ('scheduled', 'paused', 'completed', 'dead')),
  UNIQUE (trigger_name, scheduled_at)
);
```

Rust implementation notes:

* Embed `init_pg_source.sql` and the four `pg_source_migrations/{0..3}_to_{1..4}.sql` files verbatim
  (they are short; see upstream). Migration 0→1 converts the old boolean `locked` to `TIMESTAMPTZ`;
  1→2 replaces two old indexes with `event_log_fetch_events`; 2→3 adds `hdb_event_log_cleanups`,
  `event_invocation_logs.trigger_name` and drops the FK `event_invocation_logs_event_id_fkey`;
  3→4 replaces `insert_event_log` with the trace-context-aware version shown above.
* `insert_event_log` uses the unqualified `gen_random_uuid()` (search_path) – keep identical.
* Session variables: every Hasura mutation transaction runs
  `SET LOCAL "hasura.user" = '<json of session variables>'` (e.g. `'{"x-hasura-role":"admin"}'`)
  and, if a trace context exists, `SET LOCAL "hasura.tracecontext" = '{"trace_id":"<32 hex>","span_id":"<16 hex>","sampling_state":"1"}'`
  (`sampling_state` omitted when deferred) (`server/src-lib/Hasura/Backends/Postgres/Connection/MonadTx.hs`,
  `server/src-lib/Hasura/Tracing/Context.hs`). Writes done outside Hasura produce
  `session_variables: null` and `trace_context: null`. **The Rust mutation executor must set these
  GUCs** or event payloads lose `session_variables`.

### 4.3 Per-trigger function and trigger (`trigger.sql.shakespeare`)

Naming (`pgTriggerName`, `pgIdenTrigger`):

* logical name: `notify_hasura_<triggerName>_<OP>` with `OP ∈ {INSERT, UPDATE, DELETE}` (MANUAL never
  gets a function);
* in DDL the name is **always double-quoted** (`pgFmtIdentifier`: wrap in `"`, double inner `"`,
  truncate at NUL): `hdb_catalog."notify_hasura_my-trigger_INSERT"`;
* SQL trigger has the **same name** as the function and lives on the user table.

One function per op is created with `CREATE OR REPLACE` (template, verbatim):

```sql
CREATE OR REPLACE function hdb_catalog.#{qualifiedTriggerName}() RETURNS trigger
  LANGUAGE plpgsql
  AS $$
  DECLARE
    _old record;
    _new record;
    _data json;
  BEGIN
    IF TG_OP = 'UPDATE' THEN
      _old := #{oldRow};
      _new := #{newRow};
    ELSE
    /* initialize _old and _new with dummy values for INSERT and UPDATE events*/
      _old := row((select 1));
      _new := row((select 1));
    END IF;
    _data := json_build_object(
      'old', #{oldPayloadExpression},
      'new', #{newPayloadExpression}
    );
    BEGIN
    /* NOTE: formerly we used TG_TABLE_NAME in place of tableName here. However in the case of
    partitioned tables this will give the name of the partitioned table and since we use the table name to
    get the event trigger configuration from the schema, this fails because the event trigger is only created
    on the original table.  */
      IF (TG_OP <> 'UPDATE') OR (_old <> _new) THEN
        PERFORM hdb_catalog.insert_event_log(CAST(#{schemaName} AS text), CAST(#{tableName} AS text), CAST(#{name} AS text), TG_OP, _data);
      END IF;
      EXCEPTION WHEN undefined_function THEN
        IF (TG_OP <> 'UPDATE') OR (_old *<> _new) THEN
          PERFORM hdb_catalog.insert_event_log(CAST(#{schemaName} AS text), CAST(#{tableName} AS text), CAST(#{name} AS text), TG_OP, _data);
        END IF;
    END;

    RETURN NULL;
  END;
$$;
```

Placeholders (`mkTriggerFunctionQ`):

| placeholder | value |
|---|---|
| `qualifiedTriggerName` | `"notify_hasura_<name>_<OP>"` (quoted) |
| `schemaName`, `tableName`, `name` | SQL string literals via `pgFmtLit` (`'...'` with `'`→`''`; if the text contains `\`, prefix `E` and double backslashes) |
| `oldRow` / `newRow` | `row(<rowexp OLD/NEW over LISTEN columns>)` |
| `oldPayloadExpression` | INSERT: `NULL`; UPDATE/DELETE: `row_to_json(<rowexp OLD over PAYLOAD columns>)` |
| `newPayloadExpression` | INSERT/UPDATE: `row_to_json(<rowexp NEW over PAYLOAD columns>)`; DELETE: `NULL` |

`<rowexp V over cols>` = `(SELECT "e" FROM (SELECT <col-exprs>) AS "e")` (`mkRowExp`; whitespace in the
upstream rendering differs slightly — not semantically relevant), where each col-expr is
`V."col"` transformed by `toJSONableExp` (`server/src-lib/Hasura/Backends/Postgres/Translate/Column.hs`):

* `isBigNum` types (exactly: `bigint`, `bigserial`, `numeric`, `double precision`, `money` —
  `server/src-lib/Hasura/Backends/Postgres/SQL/Types.hs`) when `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`
  is on → `(V."col")::text`;
* geometry/geography → `ST_AsGeoJSON(V."col", 15, 4)::json AS "col"` (alias added);
* otherwise `V."col"`.

The stringify setting is baked in at function-creation time.

Listen columns = op's `columns` (`*` = all table columns **known to the schema cache at that time**,
i.e. scalar columns only — computed fields excluded). Payload columns = op's `payload` or, when absent,
all columns. The UPDATE function only emits an event when the listen-column projection changed
(`_old <> _new`; record comparison with "NULLs equal" semantics; if the column types have no `<>`
operator, e.g. `json`, the `undefined_function` handler retries with binary `*<>`).

Example (table `public.users(id int, name text, big bigint)`, trigger `user_upd`, update `columns: [name]`,
`payload: [id, name]`, stringify off) — rendered shape (inferred whitespace):

```sql
CREATE OR REPLACE function hdb_catalog."notify_hasura_user_upd_UPDATE"() RETURNS trigger
  LANGUAGE plpgsql AS $$
  ...
    IF TG_OP = 'UPDATE' THEN
      _old := row((SELECT "e" FROM (SELECT OLD."name") AS "e"));
      _new := row((SELECT "e" FROM (SELECT NEW."name") AS "e"));
    ...
    _data := json_build_object(
      'old', row_to_json((SELECT "e" FROM (SELECT OLD."id", OLD."name") AS "e")),
      'new', row_to_json((SELECT "e" FROM (SELECT NEW."id", NEW."name") AS "e"))
    );
    ... PERFORM hdb_catalog.insert_event_log(CAST('public' AS text), CAST('users' AS text), CAST('user_upd' AS text), TG_OP, _data); ...
$$;
```

Creating the SQL trigger (`mkTrigger`) — only if it does not already exist:

```sql
SELECT EXISTS (
  SELECT 1 FROM pg_trigger
  WHERE NOT tgisinternal
  AND tgname = $1 AND tgrelid = (quote_ident($2) || '.' || quote_ident($3))::regclass);
-- $1 = notify_hasura_<name>_<OP> (unquoted), $2 = schema, $3 = table

CREATE TRIGGER "notify_hasura_<name>_<OP>" AFTER <OP> ON "<schema>"."<table>"
  FOR EACH ROW EXECUTE PROCEDURE hdb_catalog."notify_hasura_<name>_<OP>"();

-- only when trigger_on_replication = true, and only right after creating the trigger:
ALTER TABLE "<schema>"."<table>" ENABLE ALWAYS TRIGGER "notify_hasura_<name>_<OP>";
```

Quirk (verified): because the `ALTER ... ENABLE ALWAYS` runs only when the trigger is newly created,
flipping `trigger_on_replication` on an existing trigger has no DB effect until the SQL trigger is
dropped and recreated (e.g. op removed and re-added). Keep for parity or fix deliberately.

Function-existence checks:

```sql
-- createMissingSQLTriggers (any schema!)
SELECT EXISTS ( SELECT 1 FROM pg_proc WHERE proname = $1 );        -- $1 = notify_hasura_<n>_<OP>
-- checkIfFunctionExistsQ (create_event_trigger uniqueness)
SELECT EXISTS ( SELECT 1 FROM pg_catalog.pg_proc
  JOIN pg_namespace ON pg_catalog.pg_proc.pronamespace = pg_namespace.oid
  WHERE proname = $1 AND pg_namespace.nspname = 'hdb_catalog');
```

### 4.4 When SQL triggers are (re)created

`buildTableEventTriggers` in `server/src-lib/Hasura/RQL/DDL/Schema/Cache.hs`. Nothing is done when
maintenance mode or read-only mode is on. Build reasons:

* `CatalogSync` (startup build, and rebuilds caused by another instance's metadata change): **no DDL**,
  except when the source catalog migration returned `RETRecreate` (catalog upgraded) — then the
  "recreate if needed + create missing" path below runs.
* `CatalogUpdate` (any metadata API call on this instance, including `reload_metadata`):
  1. if `recreate_event_triggers` included this source → unconditionally `createTableEventTrigger`
     (CREATE OR REPLACE functions + create missing triggers);
  2. `recreateTriggerIfNeeded` — memoised with `Inc.cache` on
     `(sqlGenCtx, table, columns, triggerName, triggerOnReplication, opsDef, sourceConfig, pk)`;
     it re-runs `createTableEventTrigger` only when one of these inputs changed since the previous
     build in this process;
  3. `createMissingSQLTriggers`: for each op in the definition, if no `pg_proc` named
     `notify_hasura_<n>_<OP>` exists, run `mkTrigger` for that op.

Upstream test `EventTriggersRecreationSpec` asserts that a plain `reload_metadata` issues **no DDL in
`hdb_catalog`**, that `recreate_event_triggers: true` does, and that adding a column to the table
(via `run_sql`) recreates the function. Rust: keep a per-process memo `trigger → hash(inputs)`; on
`CatalogUpdate` run DDL only for changed hashes, plus the cheap existence check.

Known upstream gap: a fresh source DB attached to existing metadata gets its catalog initialised at
startup (`RETDoNothing`) but its SQL triggers are only created on the next `CatalogUpdate`
(e.g. `hasura metadata apply`/`reload`). Rust may improve this by running `createMissingSQLTriggers`
at startup when `prepareCatalog` initialised the catalog (behavioural improvement, low risk).

All of a single trigger's DDL runs in one source write transaction.

---

## 5. Event processing loop

Source: `server/src-lib/Hasura/Eventing/EventTrigger.hs` (`processEventQueue`), Postgres SQL in
`server/src-lib/Hasura/Backends/Postgres/DDL/EventTrigger.hs`, thread start-up in
`server/src-lib/Hasura/App.hs`.

### 5.1 Configuration

| env var / flag | default | meaning |
|---|---|---|
| `HASURA_GRAPHQL_EVENTS_HTTP_POOL_SIZE` / `--events-http-pool-size` | 100 (positive) | max concurrently processing events (one task per event) |
| `HASURA_GRAPHQL_EVENTS_FETCH_BATCH_SIZE` / `--events-fetch-batch-size` | 100 (≥0) | `LIMIT` of the fetch query, per source; **0 disables** event-trigger delivery |
| `HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL` / `--events-fetch-interval` | 1000 ms (≥0) | sleep after a fetch returned 0 events; **0 disables** delivery |
| `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` / `--graceful-shutdown-timeout` | 60 s | wait for in-flight events at shutdown |
| `HASURA_GRAPHQL_DISABLE_EVENTING` / `--disable-eventing` | false | no source catalog init, no pollers (events, cron, scheduled, async actions) |
| `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING` / `--disable-event-processing` | false | catalogs created, but no pollers start (recent upstream addition) |
| `HASURA_GRAPHQL_ENABLE_TRIGGERS_ERROR_LOG_LEVEL` | false | failed deliveries logged at `error` instead of `info` |
| `HASURA_GRAPHQL_REDACT_EVENT_TRIGGER_LOGS` | false | null-out request body, session vars, transform ctx, response body in `event-trigger` logs |
| `HASURA_GRAPHQL_REDACT_SCHEDULED_TRIGGER_LOGS` | false | same for `scheduled-trigger` logs |

(`server/src-lib/Hasura/Server/Init/Arg/Command/Serve.hs`.) Pollers start only when eventing and
event processing are both enabled (`App.hs` `case (appEnvEventingMode, appEnvEventProcessingMode)`).

### 5.2 Fetching

Each loop iteration ("popEventsBatch"): for every source in the schema cache **concurrently**,
if the source has ≥1 event trigger, run in a source **write** transaction (read committed):

```sql
UPDATE hdb_catalog.event_log
SET locked = NOW()
WHERE id IN ( SELECT l.id
              FROM hdb_catalog.event_log l
              WHERE l.delivered = 'f' and l.error = 'f'
                    and (l.locked IS NULL or l.locked < (NOW() - interval '30 minute'))
                    and (l.next_retry_at is NULL or l.next_retry_at <= now())
                    and l.archived = 'f'
                    and l.trigger_name = ANY($2)
              /* NB: this ordering is important for our index `event_log_fetch_events` */
              ORDER BY locked NULLS FIRST, next_retry_at NULLS FIRST, created_at
              LIMIT $1
              FOR UPDATE SKIP LOCKED )
RETURNING id, schema_name, table_name, trigger_name, payload::json, tries, created_at, next_retry_at,
  (select (select created_at at time zone (select current_setting ('TIMEZONE'))) at time zone 'UTC'),
  (select (select next_retry_at at time zone (select current_setting ('TIMEZONE'))) at time zone 'UTC');
-- $1 = batch size (int8), $2 = text[] of trigger names of this source present in the schema cache
```

Notes:

* Locks older than **30 minutes** are considered abandoned (crash recovery). This is the only recovery
  for events locked by a crashed/killed instance.
* Only trigger names present in the current schema cache are fetched; events of unknown triggers stay
  in the table forever unless archived.
* The last two columns are UTC versions for metrics only.
* Errors are logged (`EventInternalErr`, log type `event-trigger`), prefixed
  `something went wrong while fetching events: ` and the source yields 0 events.
* Fetched IDs are added to an in-memory "locked events" set per source (used at shutdown).

### 5.3 Loop structure and concurrency (`go`)

```
events := popEventsBatch()
loop:
  if events is empty: sleep(fetch_interval)
  spawn prefetch = async popEventsBatch()        -- next batch fetched while current one is processed
  for each event in events (in order):
      wait until active_tasks < HTTP_POOL_SIZE    -- STM check
      active_tasks += 1
      spawn task: processEvent(event); finally active_tasks -= 1
  events := await prefetch                        -- some tasks of the old batch may still run
  if len(old batch) == batch_size: full_fetch_count += 1; if ≥3 full fetches in a row,
      warn once: "Events processor may not be keeping up with events generated in postgres, or we're
      working on a backlog of events. Consider increasing HASURA_GRAPHQL_EVENTS_HTTP_POOL_SIZE"
  else: reset; if previously warned: warn "It looks like the events processor is keeping up again."
```

No ordering guarantee (events "approximately in created_at order"). At-least-once delivery.

Rust: one tokio task per source fetch, a `Semaphore(pool_size)` for processing, prefetch with
`tokio::spawn`, and a `CancellationToken` for shutdown. Keep the semantics that the next fetch starts
before the current batch is fully dispatched (it is bounded by batch size, not by pool size).

### 5.4 Processing one event (`processEvent`)

1. Look up `(source, table, trigger)` in the current schema cache. If missing (trigger dropped, cache
   out of date): log `table '...' not found` / `event trigger 'x' on table '...' not found` (500
   `unexpected`), then `UPDATE hdb_catalog.event_log SET next_retry_at = $1, locked = NULL WHERE id = $2`
   with `$1 = now + 60 s`. `tries` is **not** incremented and no invocation row is written.
2. Build the payload (§5.5), headers (§5.6), and the request (with optional request transform).
   HTTP method is always `POST`. HTTP response timeout = `retry_conf.timeout_sec` or 60 s.
3. Whole processing (request + transform + DB bookkeeping) is wrapped in a timeout of
   `min(30 min, timeout_sec)` seconds (`upperBoundEventTriggerTimeout`); 30 min if `timeout_sec` absent.
   On this timeout: log `Event Trigger "<name>" timed out while processing.` (500 `timeout-error`) and run
   the error path with `HOther "Event Trigger \"<name>\" timed out while processing."` and request
   payload `null`.
4. Success = HTTP status 200–299 (`anyBodyParser`). Any other status, connection failure, response
   timeout, invalid URL → error path.
5. If a `response_transform` exists and the HTTP call succeeded, it is applied to the body; a transform
   error becomes `HOther <json of error>` → error path (status 500).
6. A **request transform** error → logged, event marked `error = 't'` directly (no invocation row, no
   retry, tries not incremented):
   `UPDATE hdb_catalog.event_log SET error = 't', next_retry_at = NULL, locked = NULL WHERE id = $1`.
7. Remove event id from the in-memory locked set.

A tracing span named `Event trigger: <name>` is created; if the payload has
`trace_context.trace_id` (+ `span_id`, `sampling_state`), the span continues that trace.

### 5.5 Delivery payload (request body)

`EventPayload` (`EventTrigger.hs`), serialised with `hasuraJSON` (snake_case, prefix stripped) via
`J.encode (J.toJSON ep)` — i.e. through an aeson `Value`. Upstream pins aeson 2.3.0.0 (default
`ordered-keymap` flag ⇒ `KeyMap` is a `Data.Map`), so **every object in the body is emitted with keys
sorted lexicographically**, at every nesting level, including `event` (decoded from the stored JSONB)
and the row objects inside `data` (column order is *not* preserved). Exact body bytes (modulo
whitespace — aeson emits none):

```json
{
  "created_at": "2024-03-01T10:15:30.123456",
  "delivery_info": { "current_retry": 0, "max_retries": 0 },
  "event": {
    "data": { "new": { "id": 1, "name": "b" }, "old": { "id": 1, "name": "a" } },
    "op": "UPDATE",
    "session_variables": { "x-hasura-role": "admin", "x-hasura-user-id": "1" },
    "trace_context": { "sampling_state": "1", "span_id": "…16 hex…", "trace_id": "…32 hex…" }
  },
  "id": "85558393-c75d-4d2f-9c15-e80591b83894",
  "table": { "name": "users", "schema": "public" },
  "trigger": { "name": "user_upd" }
}
```

(Key order is not semantically meaningful in JSON, but matching it makes golden/HMAC-signature
comparisons byte-identical; Rust should serialise via a `BTreeMap`-ordered value or `serde_json`
without `preserve_order`.)

* `event` is the stored `payload` column (`payload::json` from JSONB), decoded and re-encoded.
  `session_variables` / `trace_context` are `null` when not set. `data.old`/`data.new` per op:
  INSERT `{old:null,new:row}`, UPDATE `{old,new}`, DELETE `{old:row,new:null}`,
  MANUAL `{old:null,new:<payload given to invoke>}`. Number literals from JSONB are re-rendered by
  aeson's `Scientific` encoder (inferred: e.g. `1.0` stays `1.0`, large integers stay exact).
* `delivery_info.current_retry` = `tries` column value **before** this attempt (0 on first attempt);
  `max_retries` = `retry_conf.num_retries`.
* `created_at` = `event_log.created_at` (a `TIMESTAMP` in the DB server's time zone, decoded as
  `LocalTime` with the binary `timestamp_int` decoder) rendered by aeson as
  `YYYY-MM-DDTHH:MM:SS[.fff…]` with **no zone suffix** (no `Z`, no offset). Fractional part: omitted
  when zero, otherwise the minimal number of digits with trailing zeros removed digit-by-digit (aeson
  `timeOfDay64` `trunc12/trunc6/trunc3/trunc2/trunc1`), so 1–6 digits for Postgres µs values —
  identical to Postgres' own `to_json(timestamp)` text. Examples: `…:30`, `…:30.5`, `…:30.12`,
  `…:30.123456`. (The upstream docs example `"2020-07-29T10:00:00.000Z"` is wrong.) Corroborated by
  `tests-py/test_scheduled_triggers.py` (asserts engine `created_at` == `to_json(timezone('utc',…))` + `Z`
  for the analogous `UTCTime` field). aeson's encoder is not vendored on disk; the per-digit trimming
  is from aeson source knowledge + that test.
* No `"payload"` wrapper — the docs' `payload` key is from the invocation log (`request.payload`).

### 5.6 Request headers

`prepareHeaders` (`server/src-lib/Hasura/Eventing/HTTP.hs`) + `addDefaultHeaders` (`server/src-lib/Hasura/HTTP.hs`):

* `Content-Type: application/json` and `User-Agent: hasura-graphql-engine/<version>` are always sent
  first and **override** user headers with the same (case-insensitive) name;
* then configured headers in order, with values resolved at schema-build time (env / template);
* tracing propagation headers (verified: `invokeRequest` → `traceHTTPRequest`,
  `server/src-lib/Hasura/Tracing/Utils.hs`): the delivery runs inside a span `POST <url>` (kind client)
  and the current context is injected by the propagator from metadata
  `opentelemetry.exporter_otlp.traces_propagators` (default `["b3"]`; values `"b3"`, `"tracecontext"`,
  combined in list order). The injected headers are **prepended** to the request's header list:
  * `b3`: `X-B3-TraceId` (32 hex), `X-B3-SpanId` (16 hex, the HTTP span), `X-B3-ParentSpanId` (if a
    parent span exists — here the `Event trigger: <name>` span), `X-B3-Sampled` (`1`/`0`, omitted when
    deferred; event/scheduled traces use `sampleAlways` ⇒ `1`);
  * `tracecontext`: `traceparent` and `tracestate` (W3C).
  For event triggers the trace id is the one stored in the event's `trace_context` when present (so the
  webhook call joins the trace of the mutation that produced the event), otherwise a fresh trace.
  Scheduled/cron deliveries always start a fresh trace (`Tracing.newTrace`). Whether OSS without an
  OTLP exporter still has a trace context (and hence emits these headers) is inferred: yes, `newTrace`
  always creates one; Rust should emit the B3 set by default for parity;
* a request transform may change method/URL/headers/body afterwards.

### 5.7 Bookkeeping SQL

Success (`recordSuccess`, one transaction):

```sql
INSERT INTO hdb_catalog.event_invocation_logs (event_id, trigger_name, status, request, response)
VALUES ($1, $2, $3, $4, $5);
UPDATE hdb_catalog.event_log SET tries = tries + 1 WHERE id = $1;
UPDATE hdb_catalog.event_log SET delivered = 't', next_retry_at = NULL, locked = NULL WHERE id = $1;
```

Failure (`recordError`, one transaction): same INSERT + `tries = tries + 1`, then either

```sql
UPDATE hdb_catalog.event_log SET next_retry_at = $1, locked = NULL WHERE id = $2;   -- retry
UPDATE hdb_catalog.event_log SET error = 't', next_retry_at = NULL, locked = NULL WHERE id = $1;  -- give up
```

`next_retry_at` is bound as a Haskell `UTCTime` (sent as `timestamptz`); Postgres converts it to the
session time zone when assigning to the `TIMESTAMP` column, which matches `now()` comparisons. Rust
must bind it as `timestamptz` (not naive UTC `timestamp`), or compute `now() + interval` in SQL.

### 5.8 Retry decision (`retryOrSetError`)

```
tries            = event.tries (value before this attempt)
retry_after      = for non-2xx HTTP responses only: header "Retry-After" (case-insensitive),
                   parsed as integer seconds; accepted only if > 0 (HTTP-date form NOT supported)
exhausted        = tries >= retry_conf.num_retries
if exhausted and retry_after is None: give up (error = 't')
else: next_retry_at = now + (retry_after or retry_conf.interval_sec) seconds
```

Consequences: total attempts = `num_retries + 1`; a webhook returning `Retry-After` keeps the event
alive **indefinitely** (upstream TODO acknowledges this). `Retry-After` on 2xx is ignored. Connection
errors/timeouts never have `Retry-After`.

### 5.9 Invocation log record

Row in `event_invocation_logs`: `status` = HTTP status or NULL; `request`/`response` JSON:

```json
// request (WebhookRequest)
{
  "payload": { /* final request body actually sent (after request transform), parsed as JSON; null if not JSON or on processing timeout */ },
  "headers": [
    { "name": "Content-Type", "value": "application/json" },
    { "name": "User-Agent", "value": "hasura-graphql-engine/v2.x" },
    { "name": "X-Api-Key", "value_from_env": "API_KEY" },     // configured headers echo their HeaderConf
    { "name": "X-Static", "value": "abc" }
  ],
  "version": "2"
}
// response, 2xx
{ "type": "webhook_response",
  "data": { "body": "<response body as string>", "headers": [{"name":"..","value":".."}], "status": 200 },
  "version": "2" }
// response, non-2xx / client error / internal error
{ "type": "client_error", "data": { "message": "<string>" }, "version": "2" }
```

* Event triggers use invocation `version` `"2"`; scheduled/cron use `"1"` (`invocationVersionET/ST`).
* Stored text: both columns are written as `PG.ViaJSON (toJSON …)`, i.e. through an aeson `Value`, so
  object keys are sorted (`{"headers":…,"payload":…,"version":"2"}`, `{"data":…,"type":…,"version":…}`,
  `data` = `{"body","headers","status"}`) — see the key-order note in §5.5. The columns are `JSON`
  (not JSONB), so this text is returned verbatim by the log APIs.
* Logged request headers are the pre-transform headers (`prepareHeaders` output), even if a
  request transform changed them.
* `body`/`message` are `SerializableBlob`: a JSON **string** (UTF-8 text); if bytes are not valid
  UTF-8: `["Base64", "<base64>"]`.
* `client_error.message`:
  * non-2xx HTTP: the raw response body; `status` column = HTTP status;
  * HTTP exception (connect failure, timeout, invalid URL): the JSON text of
    `{"type":"http_exception","message":"<e.g. Response timeout | Connection failure: …>","request":{"host","port","secure","path","queryString","method","responseTimeout"},"status"?:n}`
    or `{"type":"invalid_url","message":"..."}`; `status` column = NULL (or the status in a
    `StatusCodeException`);
  * internal (`HOther`, e.g. processing timeout, response-transform error): JSON-encoded string
    (i.e. with surrounding quotes) of the message; `status` column = **500**.

### 5.10 Server logs

Each delivery logs (type `event-trigger`, level info, or error when failed and
`ENABLE_TRIGGERS_ERROR_LOG_LEVEL`):

```json
{"type":"event-trigger","level":"info","detail":{
  "event_id":"…","event_name":"user_upd",
  "request":{"original_request":{…,"url":"<webhook template or env var NAME, never the resolved URL>","headers":{"X-Api-Key":"<from_env: API_KEY>","Authorization":"<REDACTED>"}},"original_size":123,"transformed_request":null,…,"session_vars":{…},"req_transform_ctx":null},
  "response":{"status":200,"headers":[…],"body":"…","size":2}   // or {"type":"status|client|internal","detail":…}
}}
```

Exact structure is in `HTTPRespExtra`/`sanitiseReqJSON` (`server/src-lib/Hasura/Eventing/HTTP.hs`).
Fetch statistics are aggregated and logged periodically as `event-trigger-process`
(`{"num_events_fetched":{"<source>":n},"num_fetches":n}`). Implement log shapes as best-effort; not
contract-critical.

### 5.11 Graceful shutdown and startup

* On SIGTERM the processing thread's shutdown action (`waitForProcessingAction`, `App.hs`): every 5 s
  check the number of event ids in the locked set; when 0 → done; when
  `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` is exhausted → for each source, unlock remaining ids
  (with retry on `concurrent-update` errors):

```sql
WITH "cte" AS
(UPDATE hdb_catalog.event_log
SET locked = NULL
WHERE id = ANY($1::text[])
-- only unlock those events that have been locked, it's possible
-- that an event has been processed but not yet been removed from
-- the saved locked events, which will lead to a double send
AND locked IS NOT NULL
RETURNING *)
SELECT count(*) FROM "cte";
```

  Logs `"<n> events for source <s> were successfully unlocked"`.
* There is **no** startup unlock for event-trigger events (only the 30-minute stale-lock rule).

### 5.12 Archival

`archived = 't'` is set only by dropping the trigger (delete trigger, untrack table, replace_metadata
removal). Archived rows are never fetched and are excluded from `pending`/`processed` log listings.
There is no OSS API to delete event logs; users delete rows manually (docs: `event-triggers/clean-up`).

---

## 6. Event trigger runtime APIs

All `/v1/metadata`, Postgres prefix `pg_` (also `citus_`/`mssql_` etc. upstream). Admin only.

### 6.1 `pg_invoke_event_trigger` (also `/v1/query` `invoke_event_trigger`)

Args: `{ "name": TriggerName, "source"?: "default", "payload": <any JSON> }`.

1. Trigger must exist → else 400 `not-exists` `event trigger "<name>" does not exist`.
2. `enable_manual` must be `true` → else 400 `not-supported` `manual mode is not enabled for event trigger`.
3. In one source write transaction:
   ```sql
   SET LOCAL "hasura.user" = '<caller session vars JSON>';
   SET LOCAL "hasura.tracecontext" = '<trace ctx JSON>';      -- if any
   SELECT hdb_catalog.insert_event_log($1, $2, $3, $4, $5);
   -- $1 schema, $2 table, $3 trigger name, $4 'MANUAL', $5 '{"old":null,"new":<payload>}'::json
   ```
4. Response: `{"event_id":"<uuid>"}`. Delivery happens asynchronously via the normal loop.

### 6.2 `pg_redeliver_event` (also `/v1/query` `redeliver_event`)

Args `{ "event_id": "...", "source"?: "default" }`. Transaction:

```sql
SELECT l.locked IS NOT NULL AND l.locked >= (NOW() - interval '30 minute')
FROM hdb_catalog.event_log l WHERE l.id = $1;
-- no row  → 400 not-exists  "event not found"
-- true    → 400 busy        "event is already being processed"
UPDATE hdb_catalog.event_log SET delivered = 'f', error = 'f', tries = 0 WHERE id = $1;
```

Response `{"message":"success"}`. Does not reset `next_retry_at` or `archived` (an archived event is
not redelivered). The trigger name is not checked.

### 6.3 `pg_get_event_logs`

Args `{name, source?="default", limit?=100, offset?=0, status?}` where `status` ∈ `"pending"`,
`"processed"`; absent ⇒ all. (`"all"` is **rejected** by the parser: message
`event logs status can only be one of the following: processed or pending`.) Trigger must exist in
the source's schema cache (400 `not-exists` `event trigger "<name>" does not exist`).

```sql
SELECT id, schema_name, table_name, trigger_name, payload, delivered, error, tries, created_at, locked, next_retry_at, archived
  FROM hdb_catalog.event_log
  WHERE trigger_name = $1
  -- pending:   AND delivered=false AND error=false AND archived=false
  -- processed: AND (delivered=true OR error=true) AND archived=false
  ORDER BY created_at DESC LIMIT $2 OFFSET $3;
```

Response: JSON array of
`{"id","schema_name","table_name","trigger_name","payload","delivered","error","tries","created_at","locked","next_retry_at","archived"}`;
timestamps rendered as aeson `UTCTime`: `YYYY-MM-DDTHH:MM:SS[.f{1,6}]Z` (fraction trimmed per digit as
in §5.5). Verified mechanism: `EventLog.elCreatedAt/elNextRetryAt :: UTCTime` are decoded with pg-client's
binary `timestamptz_int` decoder (`server/lib/pg-client/src/Database/PG/Query/Class.hs`), which on a
`TIMESTAMP` (without tz) column reinterprets the naive wall-clock value as UTC — so `created_at` /
`next_retry_at` carry a `Z` but are really DB-server-local time (correct only when the DB `TimeZone` is
UTC). `locked` is a real `TIMESTAMPTZ` → true UTC. Rust: format the naive value + `Z` (do not convert).
Field order in the response follows the record declaration (generic `toEncoding`), but `payload`
(a decoded `Value`) has sorted keys. DB errors prefixed `unexpected error while fetching event logs: `.

### 6.4 `pg_get_event_invocation_logs`

Args `{name, source?, limit?=100, offset?=0}`; same existence check.

```sql
SELECT id, trigger_name, event_id, status, request, response, created_at
  FROM hdb_catalog.event_invocation_logs
  WHERE trigger_name = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3;
```

Response array of `{"id","trigger_name","event_id","http_status","request","response","created_at"}`
(note `status` column is exposed as `http_status`; `created_at` is a naive `TIMESTAMP` rendered with
`Z` exactly as in §6.3). DB errors prefixed `unexpected error while fetching invocation logs: `.

### 6.5 `pg_get_event_by_id`

Args `{source?, event_id, invocation_log_limit?=100, invocation_log_offset?=0}`. Response
`{"event": <EventLog>, "invocations": [<EventInvocationLog>…]}` (invocations ordered
`created_at DESC`, filtered by `event_id`). Missing → 400 `not-exists` `event id <id> does not exist`
(id not quoted). Unlike §6.3/6.4 there is **no** trigger-existence check (works for events of
dropped/archived triggers). DB errors prefixed `unexpected error while fetching event with id <id>: `.

### 6.6 Cleanup APIs (EE)

* `cleanup_event_trigger_logs` args (`TriggerLogCleanupConfig`): `{event_trigger_name, source?="default",
  batch_size?=10000, clear_older_than (hours, required), timeout?=60, clean_invocation_logs?=false}`.
  OSS → 400 `not-supported` `Event log cleanup feature is enterprise edition only`.
* `pause_event_trigger_cleanups` / `resume_event_trigger_cleanups` args:
  `{"event_triggers": {"sources": "*" | ["src", …]}}` or
  `{"event_triggers": [{"source_name"?: "default", "event_triggers": ["t1", …]}, …]}`.
  Sets `cleanup_config.paused` true/false in metadata for matching triggers (those without
  `cleanup_config` are skipped in the `sources` form; in the qualifier form a missing cleanup config is
  400 `not-exists` `cleanup config does not exist for <name>`; unknown source → 400 `not-exists`
  `source with name <s> does not exists`). Works in OSS (pure metadata).
* EE deletion algorithm, for reference (`deleteEventTriggerLogsTx`): `SET statement_timeout = <timeout*1000>`;
  select ids with `(delivered OR error) AND trigger_name=$1 AND created_at < now() - interval '<h> hours'
  AND locked IS NULL LIMIT $2`; set `locked = now()` on them; delete (or retag `trigger_name`) invocation
  logs; delete events; `SET statement_timeout = 0`; repeat until a batch deletes nothing. Schedules are
  rows in `hdb_event_log_cleanups` (50 generated at a time when <5 remain).

---

## 7. Cron triggers

### 7.1 Metadata (`metadata/cron_triggers.yaml`)

`CronTriggerMetadata` (`server/src-lib/Hasura/RQL/Types/ScheduledTrigger.hs`):

```yaml
- name: test-trigger                 # required TriggerName (no regex/length validation for cron)
  webhook: https://example.com/hook  # required; URL template, "{{ENV}}" allowed
  schedule: 0 12 * * 1-5            # required cron expression (UTC)
  include_in_metadata: true          # REQUIRED Bool
  payload: {}                        # optional any JSON
  retry_conf:                        # optional; defaults below
    num_retries: 0                   # >= 0 ("num_retries cannot be a negative value")
    retry_interval_seconds: 10       # >= 0, seconds (number, may be fractional)
    timeout_seconds: 60              # >= 0
    tolerance_seconds: 21600         # >= 0 (6 h)
  headers: []                        # optional HeaderConf list
  comment: optional text
  request_transform: {...}           # optional
  response_transform: {...}          # optional
```

* There is no `webhook_from_env` for cron triggers; use `{{ENV}}` templates.
* Env resolution happens at schema build (`resolveCronTrigger`); failure → cron trigger inconsistent
  (`in cron trigger <name>: …`) and it is absent from the schema cache (no events generated or
  delivered for it).
* `include_in_metadata: false` triggers are stored in the metadata blob but **filtered out** of
  `export_metadata` (v1 and v2) (`processCronTriggersMetadata`), are preserved across
  `replace_metadata`, and cannot be changed through it: a new metadata containing a cron trigger with
  the same name → 400 `already-exists`
  `cron trigger with name "<n>" already exists as a cron trigger with "included_in_metadata" as false`.
* `retry_conf` serialisation: `STRetryConf` → `{"num_retries","retry_interval_seconds","timeout_seconds","tolerance_seconds"}`
  numbers (DiffTime as seconds).

Cron schedule: parsed with the Haskell `cron` package — version pinned by `cabal.project.freeze`
(`any.cron ==0.7.0`; `graphql-engine.cabal` says `cron >= 0.6.2`) — via `System.Cron.parseCronSchedule`
(`FromJSON CronSchedule` in `server/lib/hasura-base/src/Hasura/Base/Instances.hs`; a parse failure is an
aeson `fail` with the parser's message → 400 `parse-failed`). Serialised back with
`serializeCronSchedule` (so `get_cron_triggers` / export may normalise the text, e.g. `@daily` →
`0 0 * * *` — inferred). Next times come from `System.Cron.nextMatch` (`generateScheduleTimes`,
`server/src-lib/Hasura/Eventing/Common.hs`), which returns the first match **strictly after** the given
instant, at minute resolution (seconds = 0). The `cron` package source is not vendored on disk; the
following semantics are from cron-0.7.0 source knowledge (inferred, but high confidence) and should be
locked down with golden tests (`croniter`, used by upstream `test_scheduled_triggers.py`, agrees on all
of them):

* exactly 5 whitespace-separated fields `minute hour day-of-month month day-of-week` (strict parser:
  no 6th seconds/year field, trailing garbage rejected);
* per field: `*`, `n`, `a-b`, `a,b,…` (list of ranges/values), `*/n`, `a-b/n`; ranges validated
  (minute 0–59, hour 0–23, dom 1–31, month 1–12, dow 0–7 with **both 0 and 7 = Sunday**);
* keywords `@yearly`, `@monthly`, `@weekly`, `@daily`, `@hourly` (no `@reboot`, no `@annually`/
  `@midnight`);
* **no** month/day names (`JAN`, `MON`), no `L`/`W`/`#`/`?`;
* DOM/DOW: when **both** day-of-month and day-of-week are restricted (neither is `*`), a day matches if
  **either** matches (Vixie-cron OR); if one of them is `*`, only the other constrains;
* all evaluation in **UTC** (no time-zone support).

Rust: the `cron` crate uses 6/7-field Quartz syntax and different DOW numbering — do not use it
directly; write a small parser (or use `croner` with `dom_and_dow = false`/OR semantics and 5-field
mode) and golden-test against croniter.

### 7.2 `create_cron_trigger` (`/v1/metadata`; also `/v1/query`)

Args `CreateCronTrigger`: all fields above plus `replace?: false`.

* `replace: false` and name exists in schema cache → 400 `already-exists`
  `cron trigger with name: <name> already exists` (no quotes). The check is against the **schema
  cache**, not metadata: a same-named trigger that is in metadata but inconsistent (e.g. unresolvable
  `{{ENV}}`) is silently overwritten (`InsOrdHashMap.insert`).
* `buildSchemaCacheFor` makes the call fail if the new trigger itself is inconsistent (inferred, per
  the standard `buildSchemaCacheFor` contract — see metadata spec).
* Adds to metadata (`buildSchemaCacheFor (MOCronTrigger name)`), then
  `populateInitialCronTriggerEvents`: **100** schedule times strictly after `now` (Haskell clock) →
  `insertCronEvents`.
* `replace: true`: trigger must exist (400 `not-exists` `cron trigger with name: <name> does not exist`);
  metadata replaced; future un-tried events deleted
  (`DELETE FROM hdb_catalog.hdb_cron_events WHERE trigger_name = $1 AND scheduled_time > now() AND tries = 0`);
  100 new events generated from now.
* Response `{"message":"success"}`.

```sql
-- insertCronEventsTx (rendered from the S.SQLInsert AST; values are literals)
INSERT INTO "hdb_catalog"."hdb_cron_events" ("trigger_name", "scheduled_time")
VALUES ('name', '2024-03-01T12:00:00Z'), ...
ON CONFLICT DO NOTHING;
```

(`scheduled_time` literals are ISO-8601 `iso8601Show` UTC strings. The `ON CONFLICT DO NOTHING` relies on
the partial unique index `hdb_cron_events_unique_scheduled`.)

### 7.3 `delete_cron_trigger` (`/v1/metadata`; also `/v1/query`)

Args `{ "name": … }`. Must exist (400 `not-exists` `cron trigger with name: <name> does not exist`).
Removes from metadata, then deletes future untried events (same DELETE as above). Past/delivered
events and invocation logs stay. Response `{"message":"success"}`. (Docs say "scheduled events associated
will also be deleted" — only future, untried ones are.)

### 7.4 `get_cron_triggers`

No args (`{}` accepted/ignored). Response `{"cron_triggers": [<CronTriggerMetadata>…]}` including
`include_in_metadata: false` ones, in metadata order.

### 7.5 `replace_metadata` interaction

(`processCronTriggers`, `server/src-lib/Hasura/RQL/DDL/Metadata.hs`) Compare old *included-in-metadata*
triggers with new triggers by full definition equality:

* dropped-or-changed old triggers → delete their future untried events
  (`DELETE FROM hdb_catalog.hdb_cron_events WHERE scheduled_time > now() AND tries = 0 AND trigger_name = ANY($1::text[])`);
* added-or-changed new triggers → `populateInitialCronTriggerEvents` (100 from now);
* unchanged → nothing.

### 7.6 Cron event generator (`runCronEventsGenerator`)

Every **60 s** (fixed), if the schema cache has ≥1 cron trigger:

```sql
SELECT t.trigger_name, coalesce(q.upcoming_events_count, 0), coalesce(q.max_scheduled_time, now())
FROM (SELECT UNNEST ($1::text[]) as trigger_name) as t
LEFT JOIN
( SELECT
   trigger_name,
    count(1) as upcoming_events_count,
    max(scheduled_time) as max_scheduled_time
   FROM hdb_catalog.hdb_cron_events
   WHERE tries = 0 and status = 'scheduled'
   GROUP BY trigger_name
) AS q
ON t.trigger_name = q.trigger_name
WHERE coalesce(q.upcoming_events_count, 0) < 100;
```

For each "deprived" trigger generate the next **100** schedule times strictly after
`max_scheduled_time` (or `now()` if none) and insert them (`ON CONFLICT DO NOTHING`). Note
`max_scheduled_time` may be in the past (overdue events not yet processed) — generation continues
from there; the delivery side marks over-tolerance ones `dead`. Errors are logged
(`scheduled-trigger` log type) and the loop continues. Stats are logged as
`cron-event-generator-process`.

Multi-instance safety: the partial unique index `(trigger_name, scheduled_time) WHERE status='scheduled'`
prevents duplicates when several instances generate concurrently.

---

## 8. One-off scheduled events

### 8.1 `create_scheduled_event` (`/v1/metadata`; also `/v1/query`)

Args `CreateScheduledEvent`:

| key | req | default |
|---|---|---|
| `webhook` | yes | URL template (resolved at **delivery** time) |
| `schedule_at` | yes | ISO-8601 `UTCTime` via aeson 2.3's `FromJSON UTCTime` (`text-iso8601`): `YYYY-MM-DD(T or space)HH:MM[:SS[.fff…]]` followed by a **required** zone `Z` or `±HH:MM`/`±HHMM`/`±HH` (inferred from aeson source knowledge — aeson is not on disk; a zone-less value is then 400 `parse-failed`). Upstream tests always send `…Z` (`tests-py/test_scheduled_triggers.py`, `ScheduledEventsInvalidEnvVarSpec.hs`). Stored as `TIMESTAMPTZ` (µs precision) |
| `payload` | no | |
| `headers` | no | `[]` |
| `retry_conf` | no | defaults as cron |
| `comment` | no | |
| `request_transform`, `response_transform` | no | **accepted but not persisted** (see quirk) |

```sql
INSERT INTO hdb_catalog.hdb_scheduled_events
(webhook_conf,scheduled_time,payload,retry_conf,header_conf,comment)
VALUES
($1, $2, $3, $4, $5, $6) RETURNING id
-- $1 json string of the webhook template, $4 STRetryConf JSON, $5 header conf JSON array
```

Response: `{"message":"success","event_id":"<uuid>"}`. No metadata change; no schema cache rebuild.
Runs in its own metadata-DB transaction (isolation RepeatableRead for all `MonadMetadataStorage` ops).

Quirk (verified): `hdb_scheduled_events` has no transform columns and the INSERT does not store them,
so `request_transform`/`response_transform` given to `create_scheduled_event` are silently ignored at
delivery time. Replicate (or fix behind a flag; fixing requires a schema change, so don't by default).

### 8.2 `delete_scheduled_event`

Args `{ "type": "one_off" | "cron", "event_id": "…" }`:
`DELETE FROM hdb_catalog.hdb_scheduled_events WHERE id = $1` / `DELETE FROM hdb_catalog.hdb_cron_events WHERE id = $1`.
Invocation logs cascade via FK. No not-found error. Response `{"message":"success"}`.

---

## 9. Scheduled-event storage (metadata DB) — exact DDL (`server/src-rsr/initialise.sql`)

```sql
CREATE TABLE hdb_catalog.hdb_cron_events
(
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
hdb_catalog.hdb_cron_events (trigger_name, scheduled_time)
where status = 'scheduled';

CREATE TABLE hdb_catalog.hdb_cron_event_invocation_logs
(
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

CREATE TABLE hdb_catalog.hdb_scheduled_events
(
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

CREATE TABLE hdb_catalog.hdb_scheduled_event_invocation_logs
(
  id TEXT DEFAULT hdb_catalog.gen_hasura_uuid() PRIMARY KEY,
  event_id TEXT,
  status INTEGER,
  request JSON,
  response JSON,
  created_at TIMESTAMPTZ DEFAULT NOW(),

  FOREIGN KEY (event_id) REFERENCES hdb_catalog.hdb_scheduled_events (id)
     ON DELETE CASCADE ON UPDATE CASCADE
);
```

These are part of the metadata catalog (version 48) — the metadata-catalog migration engine (other
spec) owns them; relevant historical migrations: `34_to_35` (cron tables), `36_to_37`/`37_to_38`
(scheduled events), `41_to_42` (timestamptz), `42_to_43` (drop `hdb_cron_triggers`), `45_to_46`
(unique partial index), `46_to_47` (invocation event_id index).

Status lifecycle (Note `[Scheduled event lifecycle]` in `ScheduledTrigger.hs`):
`scheduled → locked → delivered | error | dead`, `locked → scheduled` (retry).

---

## 10. Scheduled-event delivery loop (`processScheduledTriggers`)

Single thread for both cron and one-off events (`server/src-lib/Hasura/Eventing/ScheduledTrigger.hs`).

### 10.1 Startup

Before the loop: **unlock all** locked scheduled events (retrying on `concurrent-update`):

```sql
UPDATE hdb_catalog.hdb_cron_events SET status = 'scheduled' WHERE status = 'locked';
UPDATE hdb_catalog.hdb_scheduled_events SET status = 'scheduled' WHERE status = 'locked';
```

(See §12 — this is unsafe with several live instances.)

### 10.2 Iteration (every 10 s, fixed)

1. One metadata-DB transaction fetching **all** due events (no LIMIT):

```sql
WITH cte AS
  ( UPDATE hdb_catalog.hdb_cron_events
    SET status = 'locked'
    WHERE id IN ( SELECT t.id
                  FROM hdb_catalog.hdb_cron_events t
                  WHERE ( t.status = 'scheduled'
                          and (
                           (t.next_retry_at is NULL and t.scheduled_time <= now()) or
                           (t.next_retry_at is not NULL and t.next_retry_at <= now())
                          )
                        ) AND trigger_name = ANY($1)
                  FOR UPDATE SKIP LOCKED
                  )
    RETURNING *
  )
SELECT row_to_json(t.*) FROM cte AS t;
-- $1 = cron trigger names in the schema cache

WITH cte AS (
    UPDATE hdb_catalog.hdb_scheduled_events
    SET status = 'locked'
    WHERE id IN ( SELECT t.id
                  FROM hdb_catalog.hdb_scheduled_events t
                  WHERE ( t.status = 'scheduled'
                          and (
                           (t.next_retry_at is NULL and t.scheduled_time <= now()) or
                           (t.next_retry_at is not NULL and t.next_retry_at <= now())
                          )
                        )
                  FOR UPDATE SKIP LOCKED
                  )
    RETURNING *
  )
SELECT row_to_json(t.*) FROM cte AS t;
```

2. Process **all cron events concurrently** (unbounded concurrency, `forConcurrently_`) and wait for all;
   then process **all one-off events concurrently** and wait. Then sleep 10 s.
   (Max lateness of a due event ≈ 10 s + the time spent on the previous batch.)
3. Ids are kept in in-memory locked sets for shutdown.

Rust: keep the 10 s cadence and per-iteration semantics, but consider bounding concurrency
(e.g. reuse `HTTP_POOL_SIZE`) and a `LIMIT` — documented deviation candidates.

### 10.3 Processing one scheduled event (`processScheduledEvent`)

* Cron: look up `CronTriggerInfo` by `trigger_name` in the schema cache (missing → log 500
  `could not find cron trigger "<n>" in the schema cache`; the event stays `locked` until restart/unlock —
  verified quirk). Webhook/headers/payload/retry conf/transforms come from the **current** trigger
  definition. One-off: webhook template and headers resolved from env **now**
  (`resolveWebhookEither`, `getHeaderInfosFromConfEither`); on failure → error path with
  request payload `{"error":"Error creating the request. The value for environment variables not found: <vars>"}`
  and `HOther "The value for environment variables not found: <vars>"` (status 500), i.e. counts as a
  failed attempt and is retried per retry_conf (verified by `ScheduledEventsInvalidEnvVarSpec`).
  The invocation row for this path (and for processing timeouts) has `request.headers = []`.
  Upstream bug (verified): in both this env-error path and the cron "trigger not in cache" path the
  event id is never removed from the in-memory locked set, so a later graceful shutdown waits the full
  `GRACEFUL_SHUTDOWN_TIMEOUT`. Rust should remove it (no wire impact).
* Timeouts: HTTP response timeout = `round(timeout_seconds)` s; outer processing timeout = exact
  `min(30 min, timeout_seconds)`; retry delay = `round(retry_interval_seconds)` (Haskell `round` is
  banker's rounding).
* Tolerance: if `now − scheduled_time > tolerance_seconds` → `status = 'dead'`, no HTTP call, no
  invocation row, tries unchanged. Note it uses `scheduled_time`, not `next_retry_at`, so retries
  scheduled beyond the tolerance window also die.
* HTTP: `POST`, response timeout `timeout_seconds`; overall processing timeout
  `min(30 min, timeout_seconds)`; on overall timeout: log
  `Cron Scheduled event "<id>" of cron trigger "<n>" timed out while processing.` /
  `One-off Scheduled event "<id>" timed out while processing.` and run the error path with request
  payload `{"error":"<message>"}`.
* Headers: same default headers as event triggers (`Content-Type`, `User-Agent`) + configured.
* Success = 2xx → invocation row + `status = 'delivered'`.
* Request transform error → log + `status = 'error'` (no invocation, no retry).
* Tracing span `Scheduled trigger: <name>` (cron) / `Scheduled trigger` (one-off), always sampled.

Bookkeeping (`insertInvocationTx`, `setScheduledEventOpTx`), each in its **own** transaction
(not atomic with each other):

```sql
INSERT INTO hdb_catalog.hdb_cron_event_invocation_logs (event_id, status, request, response) VALUES ($1, $2, $3, $4);
UPDATE hdb_catalog.hdb_cron_events SET tries = tries + 1 WHERE id = $1;
-- then one of:
UPDATE hdb_catalog.hdb_cron_events SET status = $2 WHERE id = $1;                          -- delivered | error | dead
UPDATE hdb_catalog.hdb_cron_events SET next_retry_at = $1, STATUS = 'scheduled' WHERE id = $2;  -- retry
-- identical statements for hdb_scheduled_events / hdb_scheduled_event_invocation_logs
```

Retry decision (`retryOrMarkError`): identical to §5.8 with `tries` = value before the attempt,
`num_retries` from `STRetryConf`, delay = `Retry-After` seconds (non-2xx only, integer > 0) or
`round(retry_interval_seconds)`.

Invocation `request`/`response` JSON as in §5.9 but `"version": "1"`; for scheduled events
`client_error` is used for any status ≥ 300 (`isClientError`).

### 10.4 Delivery payload (request body)

`ScheduledEventWebhookPayload` (`server/src-lib/Hasura/Eventing/ScheduledTrigger/Types.hs`), `hasuraJSON`
with `omitNothingFields = True`. The body is `J.encode (J.toJSON payload)` (`processScheduledEvent`),
i.e. built through an aeson `Value`, so keys come out **sorted** (see §5.5), not in declaration order:

```json
// cron
{
  "comment": "…",                           // only if set
  "id": "b7a6…",
  "name": "daily_report",
  "payload": {"key1": "value1"},            // null if no payload configured (always present)
  "scheduled_time": "2024-03-01T12:00:00Z"
}
// one-off
{
  "comment": "test scheduled event",
  "created_at": "2024-03-01T11:59:50.123456Z",
  "id": "…",
  "payload": {…},
  "scheduled_time": "2024-03-01T12:00:00.5Z"
}
```

* Timestamps are true UTC (`TIMESTAMPTZ` columns, read via `row_to_json` and parsed as `UTCTime`) rendered
  by aeson as `YYYY-MM-DDTHH:MM:SS[.f{1,6}]Z` — fraction omitted when zero, trailing zeros trimmed per
  digit (verified by `test_scheduled_triggers.py`, which compares `created_at` against
  `to_json(timezone('utc', created_at))` + `Z`). Cron `scheduled_time` always has `:00` seconds, no fraction.
* Quirk (verified): the fields `request_transform` and `response_transform` of the payload type are
  serialised too (omitted only when absent), so a cron trigger **with** a request/response transform
  sends (before transformation) e.g. `"request_transform": {…}, "response_transform": {…}` keys in the
  body. If the transform rewrites the body the user never sees them. Replicate for strict parity.

### 10.5 Shutdown

Same `waitForProcessingAction` as §5.11 (count = cron + one-off ids in locked sets); on timeout runs the
**global** unlock (§10.1 SQL, all locked rows, not just this instance's).

---

## 11. Scheduled-event query APIs

### 11.1 `get_scheduled_events`

Args (`GetScheduledEvents`):

```json
{ "type": "one_off" | "cron",
  "trigger_name": "…",              // required when type = cron (must exist in schema cache:
                                     // 400 not-exists "cron trigger with name: <n> does not exist")
  "limit": 10, "offset": 0,          // optional, no default (no LIMIT)
  "status": ["scheduled","locked","delivered","error","dead"],   // optional, [] = all
  "get_rows_count": false,           // optional
  "order_by": [ {"column": "scheduled_time", "type": "asc"|"desc", "nulls": "first"|"last"} ]  // optional
}
```

Default ordering `scheduled_time ASC`. SQL shape (`mkPaginationSelectExp`):

```sql
WITH "count_cte" AS (SELECT * FROM "hdb_catalog"."hdb_cron_events"
                     WHERE ("trigger_name" = 'name') AND ("status" IN ('scheduled','delivered'))
                     ORDER BY "scheduled_time" ASC),
     "limit_cte" AS (SELECT * FROM "count_cte" LIMIT 10 OFFSET 0)
SELECT (SELECT count(*) FROM "count_cte"),            -- only when get_rows_count
       coalesce((SELECT json_agg(row_to_json(limit_cte.*)) FROM "limit_cte"), '[]')
```

Response: `{"events": [...], "count": N}` (`count` only when `get_rows_count: true`). Rows are decoded and
re-encoded:

* one-off: `{"id","webhook_conf","scheduled_time","retry_conf","payload","header_conf","status","tries","created_at","next_retry_at","comment","request_transform":null,"response_transform":null}`
* cron: `{"id","trigger_name","scheduled_time","status","tries","created_at","next_retry_at"}`

Security note: `order_by[].column` is spliced into SQL **unescaped** upstream (`SEUnsafe`). The Rust
implementation must whitelist column names of the target table (behaviour for valid input unchanged).

### 11.2 `get_scheduled_event_invocations`

Args: either `{ "event_id": "…", "type": "one_off"|"cron" }` (tried first) or
`{ "type": "one_off" }` / `{ "type": "cron", "trigger_name": "…" }`; plus `limit`, `offset`,
`get_rows_count` (pagination applies in all forms). SQL selects
`id, event_id, status, request::json, response::json, created_at` from the invocation table, ordered
`created_at DESC`; for `cron` by trigger name it joins `hdb_cron_events` on `id = event_id` filtered by
`trigger_name`. Cron trigger existence is checked as above.
Response: `{"invocations": [{"id","event_id","status","request","response","created_at"}…], "count"?: N}`.

---

## 12. Design considerations for a stateless / scale-to-zero deployment (Cloud Run, Cloudflare)

Facts that follow from the upstream design:

1. **Capturing** events is pure Postgres (triggers + `insert_event_log`); it works with zero engine
   instances. **Delivery** only happens while some engine instance runs its pollers.
2. Cron events are only **generated** (100 ahead, refreshed every minute) while an instance runs. With
   100 events buffered, a `* * * * *` trigger has ~100 minutes of pre-generated events; delivery still
   needs a running poller. Events overdue by more than `tolerance_seconds` (default 6 h) become `dead`
   when finally seen — scale-to-zero longer than the tolerance loses cron/one-off runs (by design).
3. Event-trigger events locked by an instance that is killed (Cloud Run scale-down, OOM, SIGKILL after
   grace) are only re-picked after **30 minutes**. Cloud Run gives ~10 s after SIGTERM, while the
   upstream default graceful timeout is 60 s and polls every 5 s.
4. On startup upstream unlocks **all** `locked` cron/one-off events. With autoscaling (instances
   starting while others are mid-delivery) this causes duplicate deliveries.
5. Cloud Run with "CPU only during requests" throttles background tasks between requests → pollers
   effectively stall.

Recommendations for the Rust engine (keep wire/DB compatibility):

* Separate roles via existing flags: API instances with `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING=true`
  (they still init catalogs and create SQL triggers), plus a worker deployment (min-instances = 1,
  CPU always allocated) that runs the pollers. Document this as the recommended Cloud Run topology.
* Shutdown: on SIGTERM immediately stop fetching, unlock not-yet-started events (prefetched batch)
  right away, then wait for in-flight deliveries up to `min(GRACEFUL_SHUTDOWN_TIMEOUT, platform grace −
  margin)`, then unlock the rest. Poll in-flight count with a notify/condvar, not a 5 s sleep.
* Startup unlock of scheduled events: provide a compat flag; default to a safer variant that only
  unlocks rows that cannot be in flight, e.g. `status='locked' AND coalesce(next_retry_at,
  scheduled_time) < now() - (timeout_seconds + 30 min)` (approximation, since no `locked_at`
  column exists; must not add columns).
* Optional "tick" mode for platforms without background CPU (Cloudflare Workers + Cron Triggers, Cloud
  Scheduler → Cloud Run): an admin-only endpoint (Rust extension, e.g. `POST /v1/eventing/tick`) that
  performs one fetch/deliver cycle for event triggers and scheduled events and one generator run,
  bounded by a time budget. This adds no DB changes.
* Optional latency improvement: `LISTEN` on a channel is **not** possible without changing the trigger
  DDL (upstream functions don't `pg_notify`). Do not change the installed function body by default
  (other tools diff it / existing DBs contain it); polling at `EVENTS_FETCH_INTERVAL` stays.
* Make the cron generator idempotent and cheap (it already is via `ON CONFLICT DO NOTHING`), so any
  instance may run it.
* Use pooled connections sparingly: the fetch query runs every second per source per instance;
  with many idle instances this is DB load (consider jitter and backoff when idle — compatible).
* All "now" comparisons should use DB `now()` where upstream does (fetch queries) and the engine clock
  where upstream does (retry times, tolerance, cron generation from `now`) — clock skew between
  instances and DB matters for tolerance/dead decisions.

Suggested Rust module boundaries (SOLID-ish): `EventStore` trait (source DB ops: fetch/lock, record
success/error, unlock, manual insert, redeliver, log queries), `ScheduledEventStore` trait (metadata DB
ops), `TriggerDdl` (render + apply function/trigger SQL; pure renderer unit-testable against golden
SQL), `WebhookClient` trait (HTTP with timeout, returns status/headers/body; mockable),
`RetryPolicy` (pure function of tries/conf/Retry-After), `CronSchedule` (parse/next — pure), `Clock`
trait for tests, and a `Dispatcher` owning concurrency (semaphore, prefetch, shutdown token).

---

## 13. Edge cases checklist

* Trigger names: case-sensitive; hyphens allowed (`notify_hasura_my-trigger_INSERT`, quoted).
* Tables with uppercase/special names: identifiers quoted; schema/table literal via `pgFmtLit`.
* Partitioned tables: trigger created on the parent; `insert_event_log` receives the *parent* table
  name from the literal (not `TG_TABLE_NAME`).
* UPDATE that changes no listened column → no event. UPDATE on JSON columns works via `*<>` fallback.
* `columns: "*"` captures the column set at function-creation time; new columns are picked up only
  when the function is recreated (run_sql / metadata change / recreate flag).
* Computed fields are never in payloads.
* `event_log.created_at`/`next_retry_at` are naive timestamps in the DB server time zone.
* Retry-After `0`, negative, non-integer, or HTTP-date → ignored.
* Manual invoke requires `enable_manual: true`; payload is wrapped `{old:null,new:payload}`.
* Redeliver of a currently-locked (≤30 min) event → `busy`.
* Deleting an event trigger archives its events; recreating a trigger with the same name later does
  **not** unarchive them.
* Scheduled events whose cron trigger is removed from metadata: future untried ones deleted; past
  locked ones remain locked.
* One-off `webhook` env var resolved at delivery; cron/event-trigger at schema build.

---

## Upstream tests to reuse

Python golden tests (`server/tests-py`):

* `test_events.py` + `queries/event_triggers/**` — the main event-trigger suite. Postgres-relevant classes:
  * `TestEventCreateAndDelete`, `TestEventCreateAndResetNonDefaultSource` (`create-delete/`, `create_and_reset_non_default/`) — create/delete, illegal-name and missing-ops errors, case-sensitive table names, non-default source reset.
  * `TestEventFloodPostgresMSSQL` (`flood/`) — many events, batch/pool behaviour.
  * `TestEventDataFormat` (`data_format/`) — bigint/numeric/geo serialisation in payloads.
  * `TestCreateEventQueryPostgres*`, `TestUpdateEventQuery`, `TestDeleteEventQuery` (`basic/`, `update_query/`, `delete_query/`) — replace semantics, dropped ops.
  * `TestEventRetryConfPostgresMSSQL` (`retry_conf/`) — `current_retry` counting, `timeout_sec`.
  * `TestEventHeadersPostgresMSSQL` (`headers/`) — `value` / `value_from_env` headers.
  * `TestEventSelCols`, `TestEventSelPayload`, `TestEventInsertOnly` (`selected_cols/`, `selected_payload/`, `insert_only/`, `update_only/`) — listen vs payload columns.
  * `TestWebhookEvent`, `TestEventWebhookTemplateURL` (`webhook_env/`, `webhook_template_url/`) — env webhooks and `{{ENV}}` templates.
  * `TestEventSessionVariables` — `session_variables` capture.
  * `TestManualEvents` (`manual_events/`) — `pg_invoke_event_trigger`, enabled vs disabled, survives reload_metadata.
  * `TestEventsAsynchronousExecutionPostgresMSSQL` (`async_execution/`) — concurrency.
  * `TestEventTransform` (`transform/`) — request/response transforms on events.
  * Skip all `*MSSQL`-only classes and `*_mssql.yaml` files.
* `test_scheduled_triggers.py` (`queries/scheduled_triggers/`) — `TestScheduledEvent` (one-off success / dead / error with `tries == num_retries+1`, payload keys `scheduled_time`,`created_at`,`id`, headers), `TestCronTrigger` (create, generated events count, update via `replace`, fired webhook payload, `get_cron_triggers`, export/import with `include_in_metadata`, duplicate create error, delete). Uses `croniter` to compute expected times — good oracle for cron parsing.
* Webhook test servers in `tests-py/context.py` (`EvtsWebhookServer`, scheduled webhook) define `/fail`, `/sleep_2s` etc.; reuse their behaviour in Rust integration tests.

Haskell api-tests (`server/lib/api-tests/src/Test`):

* `EventTriggers/PG/EventTriggersRecreationSpec.hs` — SQL triggers created on create; catalog initialised; plain `reload_metadata` causes no DDL; `recreate_event_triggers: true` does; adding a column recreates (uses a DDL-history event trigger — very useful).
* `EventTriggers/PG/EventTriggersUniqueNameSpec.hs` — `already-exists` and `Event trigger with duplicate names not allowed` errors.
* `EventTriggers/PG/EventTriggersUntrackTableCleanupSpec.hs` — untrack drops SQL triggers.
* `EventTriggers/PG/EventTriggersMoveTableSpec.hs` — replace_metadata moving a trigger between tables.
* `EventTriggers/PG/EventTriggersReplaceMetadataCleanupSpec.hs`, `EventTriggersClearMetadataSpec.hs` — source removal drops `event_log` etc.
* `EventTriggers/PG/EventTriggersRunSQLSpec.hs` — triggers keep working after `run_sql` DDL.
* `EventTriggers/PG/EventTriggersForReplicationSpec.hs` — `trigger_on_replication` → `tgenabled` `A` vs `O`.
* `EventTriggers/PG/EventTriggersExtensionSchemaSpec.hs` — pgcrypto in custom `extensions_schema`.
* `EventTriggers/EventTriggersSpecialCharactersSpec.hs` — special characters in table/column names.
* `ScheduledEvents/ScheduledEventsInvalidEnvVarSpec.hs` — one-off events with missing env vars produce errored invocation logs.
* Skip `EventTriggers/MSSQL/*`.

Other references: `server/src-test` unit tests for `Hasura.Eventing` (if present) and the
`metadata.openapi.json` schemas `EventTriggerConf`, `CronTriggerMetadata`, `STRetryConf`,
`AutoTriggerLogCleanupConfig` for metadata (de)serialisation checks.
