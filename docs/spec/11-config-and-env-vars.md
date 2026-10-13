# 11. Server configuration, environment variables, logging and startup

This document specifies how the Hasura V2 server (`graphql-engine`) is configured (CLI flags and
environment variables), what each option does, the JSON shapes of the structured logs it emits,
and what happens at startup (catalog initialisation/migration, metadata load, inconsistency
handling, background threads, graceful shutdown).

Primary upstream sources (all paths relative to the repo root):

- `server/src-lib/Hasura/Server/Init/Arg.hs` - global options (`--database-url`, `--metadata-database-url`, `--retries`, PG connection pieces) and subcommand parser.
- `server/src-lib/Hasura/Server/Init/Arg/Command/Serve.hs` - every `serve` option: flag name, env var, default, help text.
- `server/src-lib/Hasura/Server/Init/Config.hs` - option/config types (`ServeOptionsRaw`, `ServeOptions`, `Option`, `withOptionDefault`).
- `server/src-lib/Hasura/Server/Init/Env.hs` - env var parsing (`FromEnv` instances).
- `server/src-lib/Hasura/Server/Init.hs` - `mkServeOptions` (CLI > env > default resolution), deprecation warnings.
- `server/src-lib/Hasura/Server/Init/Logging.hs` - the startup log describing the configuration.
- `server/src-lib/Hasura/Logging.hs` - log envelope, log levels, log types.
- `server/src-lib/Hasura/Server/Logging.hs` - `http-log`, `webhook-log` etc.
- `server/src-lib/Hasura/GraphQL/Logging/QueryLog.hs` - `query-log`.
- `server/src-lib/Hasura/GraphQL/Transport/WebSocket/Server.hs`, `.../WebSocket.hs` - `websocket-log`.
- `server/src-lib/Hasura/App.hs` - startup sequence, catalog migration, threads, shutdown.
- `server/src-lib/Hasura/Server/Migrate.hs`, `server/src-rsr/catalog_version.txt`, `server/src-rsr/initialise.sql` - catalog.
- `server/src-lib/Hasura/Server/Types.hs` - `ExperimentalFeature`, `MaintenanceMode`, etc.
- Docs: `docs/docs/deployment/graphql-engine-flags/reference.mdx`, `docs/docs/deployment/logging.mdx`.

Authentication options (admin secret, JWT, webhook, unauthorized role) are only listed here; their
semantics are in `05-*` (permissions and auth spec). CORS semantics are in the HTTP API spec.

---

## Scope and priority

Priority tags used throughout: **must** (needed for a typical existing project or for the upstream
test harness to run), **should** (used by a meaningful fraction of projects; implement in the
first release), **may** (rare; accept and either implement later or ignore with a warning),
**ignore** (accept/parse without error, no behaviour; usually deprecated or EE/Cloud-only).

Essential (must):

- `HASURA_GRAPHQL_DATABASE_URL` / `--database-url`, `HASURA_GRAPHQL_METADATA_DATABASE_URL` / `--metadata-database-url`.
- Server host/port (default `8080`). We additionally honour `PORT` (Cloud Run) as an extension (see 2.2).
- Auth: `HASURA_GRAPHQL_ADMIN_SECRET`, `HASURA_GRAPHQL_JWT_SECRET`, `HASURA_GRAPHQL_AUTH_HOOK(+_MODE)`, `HASURA_GRAPHQL_UNAUTHORIZED_ROLE`.
- `HASURA_GRAPHQL_ENABLED_APIS`, `HASURA_GRAPHQL_ENABLE_CONSOLE` (console may be served from upstream assets, or disabled - see 3.6), `HASURA_GRAPHQL_CORS_DOMAIN`, `HASURA_GRAPHQL_DISABLE_CORS`.
- Logging: `HASURA_GRAPHQL_ENABLED_LOG_TYPES`, `HASURA_GRAPHQL_LOG_LEVEL`, JSON log line format, `http-log` shape.
- `HASURA_GRAPHQL_DEV_MODE`, `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS`, `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES`, `HASURA_GRAPHQL_ENABLE_ALLOWLIST`, `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS`, `HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS`, `HASURA_GRAPHQL_EXPERIMENTAL_FEATURES` (parse all names), `HASURA_GRAPHQL_DEFAULT_NAMING_CONVENTION`.
- Eventing/async knobs (`EVENTS_*`, `ASYNC_ACTIONS_*`), live query knobs, websocket keepalive/init timeout, graceful shutdown.
- Startup: catalog creation/migration to catalog version 48 compatible schema (48 is also the catalog version of the latest release v2.51.0 - verified `server/src-rsr/catalog_version.txt` at tag `v2.51.0`), metadata load, inconsistency logging, `/healthz`.
- CLI compatibility: `graphql-engine [global opts] serve [serve opts]`, global opts allowed after `serve`, `+RTS ... -RTS` and `+Heartbeat ... -Heartbeat` stripped (see 1.3).

Rare (may): stripes, prepared statements toggle, `V1_BOOLEAN_NULL_COLLAPSE`, `BACKWARDS_COMPAT_NULL_IN_NONNULLABLE_VARIABLES`, `REMOTE_SCHEMA_SKIP_NULLS`, `REMOTE_SCHEMA_PRIORITIZE_DATA`, `CONFIGURED_HEADER_PRECEDENCE`, `PRESERVE_401_ERRORS`, `MAX_TOTAL_HEADER_LENGTH`, `WEBSOCKET_QUEUE_SIZE`, redaction flags, `LOG_MASKED_VARIABLES`, maintenance mode, `export`/`clean`/`downgrade` commands, `METADATA_DEFAULTS`.

EE/Cloud-only (ignore, but accept the env var without failing): `HASURA_GRAPHQL_EE_LICENSE_KEY`, `HASURA_GRAPHQL_ADMIN_SECRETS`, `HASURA_GRAPHQL_JWT_SECRETS`, Redis vars (`HASURA_GRAPHQL_REDIS_URL`, `HASURA_GRAPHQL_RATE_LIMIT_REDIS_URL`, ...), `HASURA_GRAPHQL_MAX_CACHE_SIZE`, `HASURA_GRAPHQL_CACHE_MAX_ENTRY_TTL`, `HASURA_GRAPHQL_METRICS_SECRET`, `HASURA_GRAPHQL_ENABLE_PERSISTED_QUERIES` (CE parses it but rejects persisted queries), `HASURA_GRAPHQL_ENABLE_QUERY_TRACING`, `HASURA_GRAPHQL_CONNECTIONS_PER_READ_REPLICA`, `HASURA_GRAPHQL_ENABLE_LOG_COMPRESSION`, `HASURA_GRAPHQL_SSO_PROVIDERS`, `HASURA_GRAPHQL_PG_SSL_CERTIFICATE_PATH`. Note: the CE binary does not read most of these at all (they are read only by the EE binary); for CE parity we simply ignore them. Since unknown env vars are never an error, nothing needs to be done.

Telemetry: upstream sends anonymous telemetry to `telemetry.hasura.io` by default. The Rust engine must **not** phone home; it accepts `HASURA_GRAPHQL_ENABLE_TELEMETRY` and ignores it (it only affects the console config). Same for the update check (`releases.hasura.io`).

---

## 1. Command line structure

### 1.1 Grammar

```
graphql-engine [GLOBAL OPTIONS] COMMAND [COMMAND OPTIONS]

COMMAND := serve | export | clean | downgrade | version
```

Source: `server/src-lib/Hasura/Server/Init/Arg.hs` (`parseHgeOpts`, `parseHGECommand`).

Global options (parsed before the command; optparse-applicative's backtracking also lets them
appear **after** the subcommand, e.g. `graphql-engine serve --metadata-database-url ...`; both
upstream test harnesses rely on this ordering being accepted in some form - tests-py uses
`graphql-engine --metadata-database-url URL serve ...`, api-tests passes `serve ... --metadata-database-url URL`):

| Flag | Env var | Type | Default | Notes |
|---|---|---|---|---|
| `--database-url URL` | `HASURA_GRAPHQL_DATABASE_URL` | Postgres URL (template, see 2.1) | none | Becomes the `default` source if no metadata exists yet. |
| `--metadata-database-url URL` | `HASURA_GRAPHQL_METADATA_DATABASE_URL` | Postgres URL | none | Where `hdb_catalog` lives. |
| `--retries N` | `HASURA_GRAPHQL_NO_OF_RETRIES` | Int | `1` | Connection retries for the default source (deprecated, moved to metadata). |
| `--host HOST` | (none) | string | - | PG host (piecewise connection). |
| `-p/--port PORT` | (none) | Int | - | PG port. Note: `-p` is the **Postgres** port, NOT the server port. |
| `-u/--user USER` | (none) | string | - | PG user. |
| `--password PW` | (none) | string | - | PG password. |
| `-d/--dbname NAME` | (none) | string | - | PG database. |
| `-o/--pg-connection-options OPTS` | (none) | string | - | Extra libpq options. |

Piecewise options (`--host`, ... ) are only used when `--database-url` is absent; they build a
connection string (priority: may).

Precedence for database URL (`server/src-lib/Hasura/Server/Init.hs`, `mkDefaultPostgresConnInfo`/`processPostgresConnInfo`):
1. `--database-url` flag;
2. `HASURA_GRAPHQL_DATABASE_URL` env - in this case the default source is stored in metadata as
   `{"from_env": "HASURA_GRAPHQL_DATABASE_URL"}` (not the literal URL) when the catalog is first created;
3. piecewise flags;
4. none.

When `--database-url` is given literally (flag), the new `default` source stores the URL in
metadata as a literal string (inferred from `UrlFromEnv`/`UrlValue` handling in `Init.hs`; the
api-tests and tests-py harnesses use `--metadata-database-url` only, so this is rarely observable).

### 1.2 Commands

- `serve` - run the server (all options in section 3).
- `export` - connect to the metadata DB, print the stored metadata JSON on stdout, exit 0
  (`server/src-lib/Hasura/App.hs`, `runHGEServer`/`handleCommand`; priority: may).
- `clean` - drop `hdb_catalog` (and related schema objects) from the metadata DB; prints
  `successfully cleaned graphql-engine related data` (priority: may).
- `downgrade --to-<version> | --to-catalog-version N [--dryRun]` - run catalog downgrade
  scripts (priority: ignore - our catalog is fixed at the upstream latest; implementing downgrade is
  out of scope; print an error).
- `version` - prints `Hasura GraphQL Engine: <version>` and exits 0 (priority: should).

### 1.3 RTS / Heartbeat arguments (must)

GHC programs accept `+RTS ... -RTS` (and `--RTS`) runtime-system arguments anywhere on the command
line; upstream benchmarks run `graphql-engine serve +RTS -T -RTS` and the api-tests external-process
harness runs `graphql-engine +Heartbeat --enable-monitoring -Heartbeat serve ...`
(`server/lib/test-harness/src/Harness/Services/ExternalProcess/GraphqlEngine.hs`). The Rust binary
must strip any `+RTS ... -RTS` span (also `+RTS` to end of argv when no `-RTS`) and any
`+Heartbeat ... -Heartbeat` span before parsing. The `/dev/rts_stats` endpoint exists upstream
only when RTS stats are enabled (`-T`); we may either omit it or return a compatible JSON subset
(the benchmark adhoc operations read `max_live_bytes`/`allocated_bytes` etc. - priority: may).

### 1.4 Resolution rules (must)

`server/src-lib/Hasura/Server/Init/Config.hs` (`withOptionDefault`, `withOptionSwitch`) and
`server/src-lib/Hasura/Server/Init.hs`:

- Value options: CLI flag value, else env var value, else default.
- Switch options (flags without argument such as `--enable-console`, `--dev-mode`,
  `--disable-cors`): if the switch is present -> `true`; else env var (if set); else default.
  A switch cannot turn something off; the env var can (`HASURA_GRAPHQL_ENABLE_CONSOLE=false`).
- NOT every boolean flag is a switch. These boolean flags take a **value** on the CLI
  (`--flag true|false`, parsed with the env Bool parser): `--enable-telemetry`,
  `--use-prepared-statements`, `--auth-hook-send-request-body`, `--admin-internal-errors`,
  `--infer-function-permissions`, `--v1-boolean-null-collapse`,
  `--null-in-nonnullable-variables`, `--configured-header-precedence` (verified in
  `Arg/Command/Serve.hs`: `Opt.option`, not `Opt.switch`). `--enable-telemetry=false` is used by
  the tests-py harness.
- An env var that is set but fails to parse aborts startup with
  `Fatal Error:- Environment variable <NAME>: <parse error>` (verified, `considerEnv` in
  `server/src-lib/Hasura/Server/Init/Env.hs`). All fatal startup errors (`ExitException`) are
  printed with `putStrLn` to **stdout** (not stderr) and the process exits with code 1
  (`server/src-exec/Main.hs`, `main`: `BC.putStrLn msg >> Sys.exitFailure`).
- Upstream `main` snapshots the environment and then **clears the process environment**
  (`clearEnvironment` in `Main.hs`); all env access goes through the snapshot.
- Empty env var values: treated as set (and usually fail parsing for non-string types) (inferred).
  For string-typed options an empty value is used as is. The Rust engine should treat
  `VAR=""` the same way (parse error for numbers/bools).
- The engine keeps a snapshot of the full process environment (`Env.Environment`) used for
  `{{ENV}}` templates, `from_env` values, `value_from_env` headers, and action/event webhook
  URLs from env. All env vars (not just `HASURA_*`) are available, except that header
  `value_from_env` may not reference vars starting with `HASURA_GRAPHQL_` (see 4).

### 1.5 Env value parsers (must)

From `server/src-lib/Hasura/Server/Init/Env.hs`:

| Type | Accepted values | Error text (after `Environment variable X: `) |
|---|---|---|
| Bool | case-insensitive `true`,`t`,`yes`,`y` -> true; `false`,`f`,`no`,`n` -> false (no whitespace trimming) | ` Not a valid boolean text. True values are ["true","t","yes","y"] and  False values are ["false","f","no","n"]. All values are case insensitive` (verbatim incl. leading space and double space) |
| Int / Integer | Haskell `readMaybe` (decimal; leading `-` allowed) | `Expecting Int value` / `Expecting Integer value` |
| Refined NonNegative numbers (stripes, connections, timeout, events fetch interval/batch size, graceful shutdown, server timeout, keepalive) | as underlying type, must be >= 0 | `Only expecting a non negative numeric` |
| Refined Positive Int (events HTTP pool size, websocket queue size) | integer > 0 | `Only expecting a positive integer` |
| Port | integer 0..65535 | `Only expecting a value between 0 and 65535` |
| Seconds (`GRACEFUL_SHUTDOWN_TIMEOUT`, `WEBSOCKET_KEEPALIVE`) and RefetchInterval (live/streaming refetch, ms) | **integer** only (`fromInteger . readEither`); `1.5` is a parse error | Haskell `readEither` error (`Prelude.read: no parse`) |
| Milliseconds (`EVENTS_FETCH_INTERVAL`, OptionalInterval: `ASYNC_ACTIONS_FETCH_INTERVAL`, `SCHEMA_SYNC_POLL_INTERVAL`, `REMOTE_SCHEMA_HEALING_INTERVAL`) | Haskell Double syntax via `Read (TimeUnit ..)` (fractional allowed, e.g. `500.5`); must be >= 0 | `Prelude.read: no parse` / `Only expecting a non negative numeric` |
| NominalDiffTime seconds (`PG_CONN_LIFETIME`, `PG_POOL_TIMEOUT`) | Double (fractional allowed) | `could not parse as a Double` |
| `WEBSOCKET_CONNECTION_INIT_TIMEOUT` | Int seconds, >= 0 | `Expecting Int value` / `WebSocket Connection Timeout must not be negative` |
| `LIVE_QUERIES_/STREAMING_QUERIES_MULTIPLEXED_BATCH_SIZE` | integer >= 0 | `batch size should be a non negative integer` |
| List of T | comma-separated; each item `strip`ped of whitespace | per item |
| `ENABLED_APIS` | items case-insensitive (upper-cased + stripped) from `metadata`,`graphql`,`pgdump`,`developer`,`config`,`metrics` | `Only expecting list of comma separated API types metadata,graphql,pgdump,developer,config,metrics` (verified) |
| `LOG_LEVEL` | `debug`,`info`,`warn`,`error` (case-insensitive, stripped) | `Valid log levels: debug, info, warn or error` |
| `TX_ISOLATION` | `read-committed`, `repeatable-read`, `serializable` (case-sensitive) | `Only expecting read-committed / repeatable-read / serializable` (verified, `readIsoLevel` in `Server/Utils.hs`) |
| `AUTH_HOOK_MODE` | `GET`, `POST` (**case-sensitive**: `get` is rejected) | `Only expecting GET / POST` |
| Role name (`UNAUTHORIZED_ROLE`) | any non-empty string | `empty string not allowed` |
| OptionalInterval (milliseconds) | non-negative number; `0` means "disabled" (`Skip`) | `Only expecting a non negative numeric` |
| `LOG_MASKED_VARIABLES` (HashSet Text) | comma list; items stripped; empty items dropped; never fails | - |
| `ENABLED_LOG_TYPES` | see 5.3 | see 5.3 (aeson-prefixed message) |
| `EXPERIMENTAL_FEATURES` | see 3.9; items lower-cased and stripped | `Only expecting list of comma separated experimental features, options are:` immediately followed (no space) by the comma+space separated key list |
| `DEFAULT_NAMING_CONVENTION` | `hasura-default`, `graphql-default` (case-sensitive) | `naming_convention can either be "hasura-default" or "graphql-default"` |
| JSON options (`JWT_SECRET`, `METADATA_DEFAULTS`) | JSON text | aeson error |
| String options (`ADMIN_SECRET`, `SERVER_HOST`, `CONSOLE_ASSETS_DIR`, ...) | used as-is | never fails |

---

## 2. Database connection

### 2.1 URL forms (must)

- Standard libpq URI: `postgres://user:pass@host:5432/db?sslmode=require` or `postgresql://...`.
  Any libpq keyword/value query parameters must be passed to the driver (sslmode, sslrootcert,
  sslcert, sslkey, sslpassword, application_name, options, connect_timeout, ...).
- libpq keyword/value strings (`host=... dbname=...`) are accepted upstream (it is handed to libpq)
  (inferred; priority: may).
- Template syntax: `HASURA_GRAPHQL_DATABASE_URL` may contain `{{ENV_VAR}}` placeholders expanded
  from the environment (`server/src-lib/Hasura/Server/Init/Arg.hs` uses `Template` parsing for the
  database URL; also used by source `connection_info.database_url` in metadata) (priority: may).
- `HASURA_GRAPHQL_METADATA_DATABASE_URL` additionally accepts `dynamic-from-file:///abs/path`:
  the URL is read from the file on each new connection (used for rotating credentials). For the
  metadata DB URL the path is **not** checked against `HASURA_GRAPHQL_DYNAMIC_SECRETS_ALLOWED_PATH_PREFIX`
  (verified comment in `App.hs`, `initBasicConnectionInfo`: "Since this can only be set by the
  administrator, there's no need to validate"); that prefix applies only to `dynamic_from_file`
  in source metadata (see 3.11) (priority: may).

### 2.2 Which database is which (must)

`server/src-lib/Hasura/App.hs` (`initialiseAppEnv`/`resolvePostgresConnInfo`):

- If neither `--metadata-database-url` nor `--database-url` (nor piecewise flags) is given:
  fatal `Fatal Error: Either of --metadata-database-url or --database-url option expected`, exit 1.
- Metadata DB = metadata URL if given, else the database URL.
- On first start against an empty metadata DB **with** a database URL, the initial metadata
  contains one Postgres source named `default` (see 7.2) using the database URL (stored as
  `{"from_env":"HASURA_GRAPHQL_DATABASE_URL"}` when it came from the env). Without database URL,
  initial metadata has no sources.
- If the metadata already exists, the database URL is NOT used to (re)create the `default` source;
  it is only consulted if the `default` source in metadata references it via `from_env`.

### 2.3 Pool settings for the metadata DB and default source (should)

These env vars/flags configure (a) the metadata DB pool and (b) the initial `default` source's
`pool_settings` when the catalog is first created. For existing sources, pool settings live in
metadata (`connection_info.pool_settings`), and these env vars are deprecated; upstream logs a
warning when they are set (see 2.4).

| Flag | Env var | Type | Default | Effect |
|---|---|---|---|---|
| `-s/--stripes` | `HASURA_GRAPHQL_PG_STRIPES` | Int | `1` | Deprecated (ignored for sources). |
| `-c/--connections` | `HASURA_GRAPHQL_PG_CONNECTIONS` | Int | `50` | Max connections in pool. |
| `--timeout` | `HASURA_GRAPHQL_PG_TIMEOUT` | Int seconds | `180` | Idle connection timeout. |
| `--conn-lifetime` | `HASURA_GRAPHQL_PG_CONN_LIFETIME` | Int seconds | `600` | Max connection lifetime; `0` = unlimited. |
| `--pool-timeout` | `HASURA_GRAPHQL_PG_POOL_TIMEOUT` | Int seconds | unlimited | Max wait to acquire a connection. Docs name it `HASURA_GRAPHQL_POOL_TIMEOUT` - the source uses `HASURA_GRAPHQL_PG_POOL_TIMEOUT`; accept **both** (source wins on conflict). |
| `--use-prepared-statements` | `HASURA_GRAPHQL_USE_PREPARED_STATEMENTS` | Bool | `true` | Use prepared statements for generated SQL. |
| `-i/--tx-iso` | `HASURA_GRAPHQL_TX_ISOLATION` | enum | `read-committed` | Isolation level for queries (mutations always run in their own RW tx). |
| `--retries` | `HASURA_GRAPHQL_NO_OF_RETRIES` | Int | `1` | Connection retries. |

Metadata DB pool: upstream builds the metadata DB pool directly from these same resolved
`ConnParams` (`PG.initPGPool bciMetadataConnInfo ... soConnParams` in `App.hs`,
`initialiseAppEnv`), so `PG_CONNECTIONS`, `PG_TIMEOUT`, `PG_CONN_LIFETIME`, `PG_POOL_TIMEOUT`,
`USE_PREPARED_STATEMENTS` also size/configure the metadata pool (defaults 50 / 180 s / 600 s /
unlimited / true); metadata DB retries = `--retries`/`NO_OF_RETRIES` or 1 (verified).
`PG_CONN_LIFETIME=0` means no lifetime limit. `PG_CONN_LIFETIME` and `PG_POOL_TIMEOUT` accept
fractional seconds (Double); the others are integers. `export`/`clean`/`downgrade` use a
1-connection pool.

### 2.4 Deprecation warnings (may)

`server/src-lib/Hasura/Server/Init.hs` / `Init/Utils.hs`:

- `envVarsMovedToMetadata` = `HASURA_GRAPHQL_NO_OF_RETRIES`, `HASURA_GRAPHQL_PG_CONNECTIONS`,
  `HASURA_GRAPHQL_PG_TIMEOUT`, `HASURA_GRAPHQL_PG_CONN_LIFETIME`, `HASURA_GRAPHQL_PG_POOL_TIMEOUT`,
  `HASURA_GRAPHQL_USE_PREPARED_STATEMENTS`, `HASURA_GRAPHQL_TX_ISOLATION`,
  `HASURA_GRAPHQL_CONNECTIONS_PER_READ_REPLICA`. If any is set and a `default` source exists,
  logs (type `unstructured`, level `warn`): `The following environment variables are deprecated and moved to metadata: <comma list>`.
- `deprecatedEnvVars` = `HASURA_GRAPHQL_PG_STRIPES`, `HASURA_GRAPHQL_QUERY_PLAN_CACHE_SIZE`,
  `HASURA_GRAPHQL_STRIPES_PER_READ_REPLICA`. If set: warn `The following environment variables are deprecated: <list>`.

---

## 3. `serve` options - complete reference

Source of every row: `server/src-lib/Hasura/Server/Init/Arg/Command/Serve.hs` (option record
names `serverPortOption`, `serverHostOption`, ...). Types: `Bool` = env bool syntax; switches
marked "(switch)". "Pri" = compatibility priority.

### 3.1 Network / HTTP

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--server-port` | `HASURA_GRAPHQL_SERVER_PORT` | Port | `8080` | TCP port to listen on. | must |
| `--server-host` | `HASURA_GRAPHQL_SERVER_HOST` | host preference | `*` | Bind address. Warp `HostPreference` strings: `*` (any IPv4 or IPv6), `*4` (any IPv4, prefer), `!4` (IPv4 only), `*6`, `!6`, or a literal host/IP. | must |
| `--server-timeout` | `HASURA_GRAPHQL_SERVER_TIMEOUT` | Int seconds (>= 0) | `30` | Warp `setTimeout` (slowloris/idle timeout). Not a request execution timeout. Present in v2.48.0 and later releases incl. v2.51.0 (verified against release tags). | may |
| `--max-total-header-length` | `HASURA_GRAPHQL_MAX_TOTAL_HEADER_LENGTH` | Int bytes | `1048576` (1 MiB) | Max total size of request headers; larger -> 431 (Warp behaviour) (inferred status). | may |
| `--cors-domain` | `HASURA_GRAPHQL_CORS_DOMAIN` | comma list of domains | allow all (`*`) | See CORS spec. Wildcards like `https://*.foo.com`, `http://localhost:*`. Docs claim default `null`; source default is "allow all origins" (`CCAllowAll`). Follow source. | must |
| `--disable-cors` (switch) | `HASURA_GRAPHQL_DISABLE_CORS` | Bool | `false` | Disable CORS handling (no CORS headers at all; browser same-origin policy applies). | must |
| `--ws-read-cookie` (switch) | `HASURA_GRAPHQL_WS_READ_COOKIE` | Bool | `false` | Read `Cookie` header on websocket handshake and forward to auth webhook. Only valid when CORS is disabled; otherwise startup fails: `HASURA_GRAPHQL_WS_READ_COOKIE can only be used when CORS is disabled` (`server/src-lib/Hasura/Server/Init.hs`). | may |
| `--enabled-apis` | `HASURA_GRAPHQL_ENABLED_APIS` | list | `metadata,graphql,pgdump,config` | Which route groups are served (see 3.2). | must |

Cloud Run / PaaS extension (our addition, mark as non-upstream): if neither `--server-port` nor
`HASURA_GRAPHQL_SERVER_PORT` is set and env `PORT` is a valid port, listen on `PORT`.
Precedence: `--server-port` > `HASURA_GRAPHQL_SERVER_PORT` > `PORT` > `8080`. Upstream ignores
`PORT`; Cloud Run users of upstream set `HASURA_GRAPHQL_SERVER_PORT=$PORT` or rely on Cloud Run's
default `PORT=8080`. Report the effective port in the `server_configuration` startup log as `port`.

### 3.2 `ENABLED_APIS` route gating (must)

`server/src-lib/Hasura/Server/App.hs` (`httpApp`, `onlyWhenApiEnabled`). Disabled routes return
**404** with body `{"path":"$","error":"resource does not exist","code":"not-found"}`
(`err404 NotFound "resource does not exist"`, internal errors hidden).

| API name | Routes |
|---|---|
| `graphql` | `POST /v1/graphql`, `GET /v1/graphql` (plain HTTP GET handler for persisted queries; CE always answers 400 `not-supported` `PersistedQueryNotSupported`), `POST /v1alpha1/graphql`, `POST /v1/relay`, `POST /v1beta1/relay`. (WebSockets are NOT gated, see below.) Console (`/`, `/console/*`) is gated separately by `metadata` + console enabled. |
| `metadata` | `POST /v1/query`, `POST /v1/metadata`, `POST /v2/query`, `POST /v1/graphql/explain`, `POST /v1alpha1/graphql/explain`; also needed for console. |
| `pgdump` | `POST /v1alpha1/pg_dump` |
| `config` | `GET /v1alpha1/config` |
| `developer` | `GET /dev/ekg`, `GET /dev/plan_cache`, `GET /dev/subscriptions`, `GET /dev/subscriptions/extended`, `GET /dev/dataconnector/schema` (not implied by `DEV_MODE`; must be listed explicitly) |
| `metrics` | `GET /v1/metrics` - EE only; CE has no Prometheus endpoint (CE `makeDummyPrometheusMetrics`) |

Always served regardless of `ENABLED_APIS`: `GET /healthz`, `GET /hasura/healthz`,
`GET /v1/version`, `/api/rest/*` (RESTified endpoints - gated by nothing; they check the allowlist
and permissions), `GET /api/swagger/json` (admin only), `/console/assets/*` when
`CONSOLE_ASSETS_DIR` is set, `GET /dev/rts_stats` only when the RTS `-T` flag was given.
Unknown route: 404 `{"path":"$","error":"resource does not exist","code":"not-found"}` - the
catch-all (`Spock.hookAny`) is registered only for GET and POST (other methods on unknown paths
fall through to Spock's default 404 (inferred body)); it also emits an `http-log` error entry.

WebSocket upgrades are handled by `websocketsOr` *in front of* the Spock router
(`Server/App.hs`), so they are **not** gated by `ENABLED_APIS` nor by `ENABLE_RELAY` (verified:
no API/relay check in `GraphQL/Transport/WebSocket*.hs`). Accepted ws paths: `/v1/graphql`,
`/v1alpha1/graphql`, `/v1/relay`, `/v1beta1/relay`; other paths are rejected with 404
`only '/v1/graphql', '/v1alpha1/graphql', '/v1/relay' and '/v1beta1/relay' are supported on websockets`.

Error status mapping per route (`mkSpockAction ... maybeMod200` vs `id` in `Server/App.hs`):
`/v1/graphql` (POST and GET), `/v1/relay`, `/v1beta1/relay` map every error status to HTTP 200
(except 401 when `PRESERVE_401_ERRORS`); `/v1alpha1/graphql` keeps the real error status.
Developer routes, `/v1alpha1/config` and `/api/swagger/json` are admin-only: non-admin gets 400
`{"path":"$","error":"You have to be an admin to access this endpoint","code":"access-denied"}`
(`onlyAdmin`).

Relay: when `HASURA_GRAPHQL_ENABLE_RELAY=false` (see 3.4), `POST /v1/relay` returns 400
`{"path":"$","error":"The Relay API is disabled. To enable it, set the HASURA_GRAPHQL_ENABLE_RELAY environment variable to 'true'.","code":"not-supported"}`.

`/healthz` semantics (`server/src-lib/Hasura/Server/App.hs`): first checks metadata storage
(`SELECT 1`-like). On error -> HTTP 500, plain text body `ERROR`. Otherwise if the schema cache
has inconsistent objects: with query param `?strict=true` -> 500 `ERROR: inconsistent objects in schema`;
without -> 200 `WARN: inconsistent objects in schema`; else 200 `OK`. Content-Type `text/plain; charset=utf-8`.

`/v1/version` -> `{"version":"v2.x.y","server_type":"ce"}` (`server_type` from
`getExtraDataForVersionAPI` in `server/src-lib/Hasura/App.hs`: "we always default to CE"). For tool
compatibility (hasura CLI checks server version for feature flags) the Rust engine should report
a v2-style semver (e.g. the upstream version it is compatible with, suffixed: `v2.48.0-rs.1`) -
(inferred; the hasura CLI parses `version` with a semver library and enables features when
`>= v2.0.0`; a non-semver string makes the CLI treat it as a dev build).

### 3.3 Authentication (details in doc 05)

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--admin-secret` | `HASURA_GRAPHQL_ADMIN_SECRET` | string | none | Admin secret; when unset, all requests are admin (and startup warns). | must |
| `--access-key` | `HASURA_GRAPHQL_ACCESS_KEY` | string | none | Deprecated alias of admin secret; used only when admin secret is not set (`withOptions rsoAdminSecret [adminSecretOption, accessKeyOption]` in `Server/Init.hs`). | may |
| `--auth-hook` | `HASURA_GRAPHQL_AUTH_HOOK` | URL | none | Webhook auth. | must |
| `--auth-hook-mode` | `HASURA_GRAPHQL_AUTH_HOOK_MODE` (also legacy `HASURA_GRAPHQL_AUTH_HOOK_TYPE`) | `GET`/`POST` | `GET` | Webhook method. | must |
| `--auth-hook-send-request-body` | `HASURA_GRAPHQL_AUTH_HOOK_SEND_REQUEST_BODY` | Bool | `true` | Include `request` in POST webhook body; forced `false` for GET. | should |
| `--jwt-secret` | `HASURA_GRAPHQL_JWT_SECRET` | JSON | none | JWT config. | must |
| `--unauthorized-role` | `HASURA_GRAPHQL_UNAUTHORIZED_ROLE` | role name | none | Role for unauthenticated requests (JWT/admin-secret modes). | must |
| `--disable-admin-secret` (switch) | `HASURA_GRAPHQL_DISABLE_ADMIN_SECRET` | Bool | `false` | Disable admin-secret auth, requires JWT or webhook; else fatal `Fatal Error: requires either HASURA_GRAPHQL_AUTH_HOOK or HASURA_GRAPHQL_JWT_SECRET when the admin secret is disabled` (verified, `initialiseAppEnv` in `App.hs`). Added in v2.51.0 (absent in v2.50.0; verified against release tags). | may |
| `--preserve-401-errors` (switch) | `HASURA_GRAPHQL_PRESERVE_401_ERRORS` | Bool | `false` | `/v1/graphql` and relay endpoints normally map every error QErr to HTTP 200; with this, 401 errors keep HTTP 401 (`maybeMod200` in `App.hs`). Present in v2.48.0+ incl. v2.51.0. | may |
| (none) | `HASURA_GRAPHQL_JWT_SECRETS`, `HASURA_GRAPHQL_ADMIN_SECRETS` | JSON | - | EE only (multiple secrets). | ignore |

Startup checks (`setupAuthMode` in `server/src-lib/Hasura/Server/Auth.hs`; verified messages; printed and exit 1):

| admin secret | auth hook | JWT | disable-admin-secret | Result |
|---|---|---|---|---|
| any | no | no | yes | `AMUnauthentication`: every request rejected (App.hs additionally fails startup: `Fatal Error: requires either HASURA_GRAPHQL_AUTH_HOOK or HASURA_GRAPHQL_JWT_SECRET when the admin secret is disabled`) |
| no | no | no | no | no auth (everyone admin); if unauthorized role set: `Fatal Error: --unauthorized-role (HASURA_GRAPHQL_UNAUTHORIZED_ROLE) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set` |
| yes | no | no | no | admin secret (+ optional unauthorized role) |
| any | no | yes | yes | JWT only |
| yes | no | yes | no | admin secret or JWT |
| no | no | yes | no | `Fatal Error : --jwt-secret (HASURA_GRAPHQL_JWT_SECRET) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set` |
| any | yes | no | yes | webhook only |
| yes | yes | no | no | admin secret or webhook; with unauthorized role: `Fatal Error: --unauthorized-role (HASURA_GRAPHQL_UNAUTHORIZED_ROLE) is not allowed when --auth-hook (HASURA_GRAPHQL_AUTH_HOOK) is set` |
| no | yes | no | no | `Fatal Error : --auth-hook (HASURA_GRAPHQL_AUTH_HOOK) requires --admin-secret (HASURA_GRAPHQL_ADMIN_SECRET) to be set` |
| any | yes | yes | any | `Fatal Error: Both webhook and JWT mode cannot be enabled at the same time` |

(Note the inconsistent spacing `Fatal Error :` vs `Fatal Error:` is upstream's.)
- No admin secret (and admin secret not disabled): warning log (type `startup`, kind `no_admin_secret`, level `warn`): `WARNING: No admin secret provided`.
- Admin secret disabled and (console enabled or `metadata` API enabled): `startup` log, kind
  `admin_secret_disabled`, level `info`, info = `The admin secret is disabled. Console and CLI will not functional because they require the admin-secret authentication. You can disable the console and metadata APIs by setting HASURA_GRAPHQL_ENABLE_CONSOLE=false and HASURA_GRAPHQL_ENABLED_APIS=graphql.` (verbatim, `App.hs`).

### 3.4 GraphQL behaviour

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--stringify-numeric-types` (switch) | `HASURA_GRAPHQL_STRINGIFY_NUMERIC_TYPES` | Bool | `false` | Return big numeric types as JSON strings via a `::text` cast. Exactly `bigint`, `bigserial`, `numeric`, `double precision` (float8) and `money` (`isBigNum` in `Backends/Postgres/SQL/Types.hs`, used by `toJSONableExp` in `Backends/Postgres/Translate/Column.hs`); `real` (float4) and `integer` are **not** stringified. The tests-py harness always passes this flag. | must |
| `--v1-boolean-null-collapse <bool>` (takes a value, not a switch) | `HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE` | Bool | `false` | When true, `where: {col: {_eq: null}}` collapses to `true` (pre-v2 behaviour). Default v2 behaviour: explicit `null` in a boolean expression input is an error (`unexpected null value for type ...`) (see doc on query semantics). | should |
| `--null-in-nonnullable-variables <bool>` (takes a value, not a switch) | `HASURA_GRAPHQL_BACKWARDS_COMPAT_NULL_IN_NONNULLABLE_VARIABLES` | Bool | `false` | Allow `null` for variables declared non-nullable (emulates behaviour that existed before v2.34.0, per the help text). | may |
| `--enable-allowlist` (switch) | `HASURA_GRAPHQL_ENABLE_ALLOWLIST` | Bool | `false` | Only allowlisted operations allowed for non-admin roles (doc 13). | should |
| `--infer-function-permissions` | `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS` | Bool | `true` | When true, a function is exposed to a role if the role has select permission on the function's return table; when false, explicit `function_permissions` required (see permissions doc). | must |
| `--enable-remote-schema-permissions` (switch) | `HASURA_GRAPHQL_ENABLE_REMOTE_SCHEMA_PERMISSIONS` | Bool | `false` | When false, all roles see the full remote schema and `add_remote_schema_permissions` fails; when true, remote schemas are only exposed to roles with permissions. | should |
| `--default-naming-convention` | `HASURA_GRAPHQL_DEFAULT_NAMING_CONVENTION` | `hasura-default`/`graphql-default` | `hasura-default` | Default naming convention for sources without `customization.naming_convention`. In older releases needed `EXPERIMENTAL_FEATURES=naming_convention` to take effect (see 3.9). | should |
| `--enable-relay` (switch; default already true, so only the env var can disable) | `HASURA_GRAPHQL_ENABLE_RELAY` | Bool | `true` | Serve HTTP `POST /v1/relay` and `/v1beta1/relay` (ws relay is not affected). Added in v2.49.0 (absent in v2.48.0; present in v2.51.0 - verified against tags); older releases always serve relay. | may |
| `--remote-schema-skip-nulls` (switch) | `HASURA_GRAPHQL_REMOTE_SCHEMA_SKIP_NULLS` | Bool | `false` | Omit null-valued arguments when forwarding to remote schemas. | may |
| `--remote-schema-prioritize-data` (switch) | `HASURA_GRAPHQL_REMOTE_SCHEMA_PRIORITIZE_DATA` | Bool | `false` | If a remote schema returns both `data` and `errors`, return the data with errors instead of failing the whole field. | may |
| `--configured-header-precedence <bool>` (takes a value, not a switch) | `HASURA_GRAPHQL_CONFIGURED_HEADER_PRECEDENCE` | Bool | `false` | For actions/remote schemas: configured headers override forwarded client headers (default: client headers win when `forward_client_headers` true). | may |
| `--disable-native-query-validation` (switch) | `HASURA_GRAPHQL_DISABLE_NATIVE_QUERY_VALIDATION` | Bool | `false` | Skip validating native query SQL against the DB at metadata build. | may |
| `--metadata-defaults` | `HASURA_GRAPHQL_METADATA_DEFAULTS` | JSON (partial metadata) | `{}` | Defaults merged into metadata at load time (e.g. `{"backend_configs":{"dataconnector":{...}}}`); used mostly for data connectors. | ignore |
| `--enable-apollo-federation` (switch) | `HASURA_GRAPHQL_ENABLE_APOLLO_FEDERATION` | Bool | `false` | Same as experimental feature `apollo_federation` (doc 13). | may |
| `--query-plan-cache-size` | `HASURA_GRAPHQL_QUERY_PLAN_CACHE_SIZE` | Int | - | Deprecated; ignored (deprecation warning). | ignore |
| `--enable-persisted-queries` (switch) | `HASURA_GRAPHQL_ENABLE_PERSISTED_QUERIES` | Bool | `false` | EE (Automatic Persisted Queries). In CE (`MonadGQLApiHandler AppM`, verified) an APQ-shaped POST body and **any** `GET /v1/graphql` fail with `throw400 NotSupported "PersistedQueryNotSupported"` (mapped to HTTP 200 on `/v1/graphql`, see 3.2), regardless of this flag. | ignore |
| `--persisted-queries-ttl` | `HASURA_GRAPHQL_PERSISTED_QUERIES_TTL` | Int seconds | `5` | EE. | ignore |
| `--trace-sql-query` (switch) | `HASURA_GRAPHQL_ENABLE_QUERY_TRACING` | Bool | `false` | EE/Cloud: include SQL in traces. | ignore |
| `--console-sentry-dsn` | `HASURA_CONSOLE_SENTRY_DSN` | string | none | Passed into console HTML. | ignore |

### 3.5 Error reporting

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--dev-mode` (switch) | `HASURA_GRAPHQL_DEV_MODE` | Bool | `false` | Include `internal` debug info in error responses for **all** roles (and for GraphQL errors under `extensions.internal`). | must |
| `--admin-internal-errors <bool>` (takes a value) | `HASURA_GRAPHQL_ADMIN_INTERNAL_ERRORS` | Bool | `true` | Include `internal` in errors for admin role only. | must |

Resolution (`server/src-lib/Hasura/Server/Init.hs`, `mkServeOptions`): if dev mode -> `ResponseInternalErrorsAllRequests`; else if admin internal errors -> `ResponseInternalErrorsAdminOnly`; else `ResponseInternalErrorsNoRequests`.
Example of an admin-visible internal error on `/v1/graphql` (Postgres error):

```json
{"errors":[{"message":"database query error","extensions":{"path":"$","code":"unexpected","internal":{"statement":"...","prepared":true,"error":{"exec_status":"FatalError","hint":null,"message":"...","status_code":"42P01","description":null},"arguments":["(Oid 114,Just (\"{\\\"x-hasura-role\\\":\\\"admin\\\"}\",Binary))"]}}}]}
```

The precise error shapes belong to the HTTP API/errors doc.

### 3.6 Console

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--enable-console` (switch) | `HASURA_GRAPHQL_ENABLE_CONSOLE` | Bool | `false` | Serve console at `/console` (and redirect `/` -> `/console`). Requires `metadata` API enabled too. | should |
| `--console-assets-dir` | `HASURA_GRAPHQL_CONSOLE_ASSETS_DIR` | path | none (upstream Docker image sets `/srv/console-assets`) | Serve `/console/assets/*` from this dir instead of the CDN. | may |

Console implementation for the Rust engine: the console HTML is a template
(`server/src-rsr/console.html`) referencing assets at a CDN (`https://graphql-engine-cdn.hasura.io/...`)
or `/console/assets` if `CONSOLE_ASSETS_DIR` set, with `window.__env` containing
`{"version","dataApiUrl"..,"isAdminSecretSet","consoleMode":"server","enableTelemetry",...}`.
Recommended: serve the same template + bundle upstream CE console assets (Apache-2.0) from a
directory; console calls `/v1/metadata`, `/v2/query`, `/v1/graphql`, `/v1alpha1/config`,
`/v1/version`. Priority: should (many users rely on the console in dev); the CLI console
(`hasura console`) is unaffected because it serves its own console and only calls the APIs.

### 3.7 Logging options

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--enabled-log-types` | `HASURA_GRAPHQL_ENABLED_LOG_TYPES` | list | `startup,http-log,webhook-log,websocket-log,jwk-refresh-log` | Which public log types are emitted (5.3). | must |
| `--log-level` | `HASURA_GRAPHQL_LOG_LEVEL` | enum | `info` | Minimum level emitted. | must |
| `--enable-metadata-query-logging` (switch) | `HASURA_GRAPHQL_ENABLE_METADATA_QUERY_LOGGING` | Bool | `false` | Include metadata/`/v1/query` request bodies in `http-log` (when `query-log` enabled). | should |
| `--http-log-query-only-on-error` (switch) | `HASURA_GRAPHQL_HTTP_LOG_QUERY_ONLY_ON_ERROR` | Bool | `false` | Include full query in http-log only for errors; otherwise only `{"operationName":...}`. | may |
| `--enable-triggers-error-log-level` (switch) | `HASURA_GRAPHQL_ENABLE_TRIGGERS_ERROR_LOG_LEVEL` | Bool | `false` | Event/scheduled trigger delivery failures logged at `error` level instead of `info`. | may |
| `--redact-event-trigger-logs` (switch) | `HASURA_GRAPHQL_REDACT_EVENT_TRIGGER_LOGS` | Bool | `false` | Omit request/response bodies (payload) from event trigger webhook logs. The three `REDACT_*` flags were added in v2.51.0 (absent in v2.50.0; verified against tags). | may |
| `--redact-scheduled-trigger-logs` (switch) | `HASURA_GRAPHQL_REDACT_SCHEDULED_TRIGGER_LOGS` | Bool | `false` | Same for scheduled triggers. | may |
| `--redact-action-handler-logs` (switch) | `HASURA_GRAPHQL_REDACT_ACTION_HANDLER_LOGS` | Bool | `false` | Same for action handler logs. | may |
| `--log-masked-variables` | `HASURA_GRAPHQL_LOG_MASKED_VARIABLES` | comma list of keys | none | Applied to **`query-log` only** (`maskQueryLog` in `GraphQL/Logging/QueryLog.hs`, called from `MonadQueryLog AppM`; http-log `query` is NOT masked). Top-level variables whose name is in the set, and any JSON object key at any depth inside variable values and inside `generated_sql.*.prepared_arguments`, have their value replaced by the string `"[MASKED]"`. Exact, case-sensitive match. Added in v2.49.0 (present in v2.51.0, absent in v2.48.0 - verified). | may |
| `--enable-log-compression` | `HASURA_GRAPHQL_ENABLE_LOG_COMPRESSION` | Bool | - | EE. | ignore |

### 3.8 Subscriptions and websockets

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--live-queries-multiplexed-refetch-interval` | `HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_REFETCH_INTERVAL` | Int ms | `1000` | Poll interval for live queries. | must |
| `--live-queries-multiplexed-batch-size` | `HASURA_GRAPHQL_LIVE_QUERIES_MULTIPLEXED_BATCH_SIZE` | Int | `100` | Max cohorts per multiplexed SQL. | must |
| `--streaming-queries-multiplexed-refetch-interval` | `HASURA_GRAPHQL_STREAMING_QUERIES_MULTIPLEXED_REFETCH_INTERVAL` | Int ms | `1000` | Poll interval for streaming subscriptions. | must |
| `--streaming-queries-multiplexed-batch-size` | `HASURA_GRAPHQL_STREAMING_QUERIES_MULTIPLEXED_BATCH_SIZE` | Int | `100` | Batch size for streaming. | must |
| `--websocket-compression` (switch) | `HASURA_GRAPHQL_CONNECTION_COMPRESSION` | Bool | `false` | Enable permessage-deflate. | may |
| `--websocket-keepalive` | `HASURA_GRAPHQL_WEBSOCKET_KEEPALIVE` | Int seconds | `5` | Keep-alive interval: legacy `graphql-ws` (subscriptions-transport-ws) protocol sends `{"type":"ka"}`; for `graphql-transport-ws` see doc 09 (subscriptions) for ping/pong behaviour. | must |
| `--websocket-connection-init-timeout` | `HASURA_GRAPHQL_WEBSOCKET_CONNECTION_INIT_TIMEOUT` | Int seconds | `3` | `graphql-transport-ws`: close with 4408 `Connection initialisation timeout` if no `connection_init` within this time. | must |
| `--websocket-queue-size` | `HASURA_GRAPHQL_WEBSOCKET_QUEUE_SIZE` | Int >=1 | `300` | Per-connection send queue bound. When full, the offending operation is stopped with an error message then `complete`; for streaming subscriptions the error includes `extensions.resume_cursor`. Message: `This operation's result could not be delivered because the connection's send queue was at capacity, so it has been stopped. ...` Added in v2.50.0 (absent in v2.49.0; present in v2.51.0 - verified against tags). | may |
| `--disable-close-websockets-on-metadata-change` (switch) | `HASURA_GRAPHQL_CLOSE_WEBSOCKETS_ON_METADATA_CHANGE` | Bool | `true` | When metadata is changed through this instance's metadata API (`Server/API/Metadata.hs`), close all websockets with code `1012` (Service Restart) so clients reconnect against the new schema. The switch turns it off; env `false` too. | should |

### 3.9 Experimental features (should: parse all; implement per feature)

Flag `--experimental-features`, env `HASURA_GRAPHQL_EXPERIMENTAL_FEATURES`, comma list, default
empty. Names (`server/src-lib/Hasura/Server/Types.hs`, `ExperimentalFeature`; parser in `Init/Env.hs`):

| Name | Effect | Pri |
|---|---|---|
| `inherited_roles` | Historical; inherited roles are always on. Accepted, ignored. | ignore |
| `optimize_permission_filters` | Optimise permission filter SQL (`EXISTS` hoisting). Behavioural change is perf only. | may |
| `naming_convention` | In releases before ~v2.29 this gated `graphql-default` naming conventions; in current source naming conventions work without it. To be compatible with older projects, **accept** it; treat naming conventions as always enabled. Note: many tests-py naming tests set it via `hge_env`. | should |
| `streaming_subscriptions` | Historical gate for streaming subscriptions; always on in current source. Accept, ignore. | ignore |
| `apollo_federation` | Enable Apollo Federation support (`_service`, `_entities`, `@key`) (doc 13). | may |
| `hide_update_many_fields` | Do not generate `update_<table>_many` mutation fields. | should |
| `bigquery_string_numeric_input` | BigQuery only. | ignore |
| `hide_aggregation_predicates` | Do not generate aggregation predicates (`<rel>_aggregate` in `bool_exp`). | should |
| `hide_stream_fields` | Do not generate `<table>_stream` subscription fields. | should |
| `group_by_aggregations` | Enable `group_by` in aggregate queries (v2.4x addition). | may |
| `disable_postgres_arrays` | Represent Postgres arrays as before (as `_text` scalars / string literals) instead of GraphQL lists. | should |
| `no_null_unbound_variable_default` | Unbound nullable variable: omit argument instead of defaulting to `null`. | may |
| `remove_empty_subscription_responses` | Streaming subscriptions do not send empty result arrays. | may |

Unknown name: fatal `Fatal Error:- Environment variable HASURA_GRAPHQL_EXPERIMENTAL_FEATURES: Only expecting list of comma separated experimental features, options are:inherited_roles, optimize_permission_filters, naming_convention, streaming_subscriptions, apollo_federation, hide_update_many_fields, bigquery_string_numeric_input, hide_aggregation_predicates, hide_stream_fields, group_by_aggregations, disable_postgres_arrays, no_null_unbound_variable_default, remove_empty_subscription_responses` (verified: no space after `are:`; order = constructor order of `ExperimentalFeature`; names are matched after lower-casing and stripping). `naming_convention`, `inherited_roles`, `streaming_subscriptions` are referenced nowhere else in the current source (verified by grep), i.e. pure no-ops. Recommendation for the Rust engine: accept unknown names with a `warn` log instead of failing, because projects may carry names introduced in versions newer than our snapshot (deviation; mark as our choice).

Feature flags (separate mechanism): `HASURA_FF_NAMING_CONVENTION_SEP_2023=true` enables a
revised `graphql-default` naming behaviour (`server/src-lib/Hasura/Server/FeatureFlag.hs` (inferred path)). Priority: may.

### 3.10 Eventing, async actions, scheduled triggers

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--events-http-pool-size` | `HASURA_GRAPHQL_EVENTS_HTTP_POOL_SIZE` | Int | `100` | Max concurrent event webhook deliveries. | must |
| `--events-fetch-interval` | `HASURA_GRAPHQL_EVENTS_FETCH_INTERVAL` | Int ms | `1000` | Sleep between polls of `hdb_catalog.event_log`, applied only when a poll returned no events (`server/src-lib/Hasura/Eventing/EventTrigger.hs`: `when (null events) $ sleep fetchInterval`). `0` is **not** "disabled": it means poll again immediately (benchmarks set `0` on projects without event triggers). Rust: same semantics, but yield/sleep at least ~1 ms to avoid a hot loop (deviation acceptable). | must |
| `--events-fetch-batch-size` | `HASURA_GRAPHQL_EVENTS_FETCH_BATCH_SIZE` | Int | `100` | Events fetched per poll; `0` disables fetching. | must |
| `--async-actions-fetch-interval` | `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_INTERVAL` | Int ms | `1000` | Poll interval for async action log; `0` disables async action processing. | must |
| `--async-actions-fetch-batch-size` | `HASURA_GRAPHQL_ASYNC_ACTIONS_FETCH_BATCH_SIZE` | Int | `10` | Async actions fetched per poll. | should |
| `--disable-eventing` (switch) | `HASURA_GRAPHQL_DISABLE_EVENTING` | Bool | `false` | Disables event triggers, cron triggers, scheduled events and async actions entirely (no pollers). Added in v2.49.0 (present in v2.51.0, absent in v2.48.0 - verified). | may |
| `--disable-event-processing` (switch) | `HASURA_GRAPHQL_DISABLE_EVENT_PROCESSING` | Bool | `false` | Catalogs/triggers are still created and events still captured, but no pollers run (event trigger, async action, cron generator, scheduled event pollers) (e.g. for dedicated API replicas). Added in v2.50.0 (present in v2.51.0 - verified). | may |
| `--schema-sync-poll-interval` | `HASURA_GRAPHQL_SCHEMA_SYNC_POLL_INTERVAL` | Int ms | `1000` | Multi-instance metadata sync polling of `hdb_catalog.hdb_schema_notifications`; `0` disables. | must |
| `--remote-schema-healing-interval` | `HASURA_GRAPHQL_REMOTE_SCHEMA_HEALING_INTERVAL` | Int ms | `5000` | Retry inconsistent remote schemas with per-schema exponential backoff starting at the interval, capped at 120 s (`maxBackoff` in `Server/RemoteSchemaHealing.hs`); `0` disables. **Not in v2.51.0** (master only; verified absent from the v2.51.0 tag). | may |
| (none) | `HASURA_GRAPHQL_SCHEDULED_TRIGGERS_*` | - | - | No such flags exist in the source for intervals; scheduled triggers poll every 10s (hardcoded, see triggers doc) (inferred). | - |

Startup logs about eventing mode (type `startup`, kind `server`, verified verbatim in
`App.hs`): `Starting in eventing enabled mode` / `Starting with event processing disabled: source catalogs will be set up but no events will be delivered`
/ `Starting in eventing disabled mode`.

### 3.11 Lifecycle, maintenance, misc

| Flag | Env var | Type | Default | Effect | Pri |
|---|---|---|---|---|---|
| `--graceful-shutdown-timeout` | `HASURA_GRAPHQL_GRACEFUL_SHUTDOWN_TIMEOUT` | Int seconds | `60` | On SIGTERM/SIGINT: stop accepting, wait for in-flight event/action/scheduled deliveries up to this; events not finished are unlocked (`locked = NULL`) so another instance can pick them up. | must |
| `--enable-maintenance-mode` (switch) | `HASURA_GRAPHQL_ENABLE_MAINTENANCE_MODE` | Bool | `false` | Catalog migrations skipped (`Catalog migrations are skipped because the graphql-engine is in maintenance mode`); startup fails (500 QErr logged as `catalog_migrate`) if `hdb_catalog`, `hdb_catalog.hdb_version` or `hdb_catalog.hdb_metadata` is missing, but the catalog **version is not compared** (verified `migrateCatalog`); metadata writes rejected and event triggers not processed (inferred). | may |
| `--enable-telemetry` | `HASURA_GRAPHQL_ENABLE_TELEMETRY` | Bool | `true` | Telemetry. Rust engine: parse, never send. Reported in console config. | ignore |
| `--metadata-database-extensions-schema` | `HASURA_GRAPHQL_METADATA_DATABASE_EXTENSIONS_SCHEMA` | schema name | `public` | Schema in which `pgcrypto` is created in the metadata DB (`CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA <x>`), needed for `gen_random_uuid()` on PG<13. | should |
| (none) | `HASURA_GRAPHQL_EE_LICENSE_KEY` | - | - | EE. | ignore |
| (none) | `HASURA_GRAPHQL_DYNAMIC_SECRETS_ALLOWED_PATH_PREFIX` | path | none | Allowed prefix for `dynamic_from_file` connection URLs / dynamic secrets. When unset, `dynamic_from_file` sources fail. Errors are `permission-error` 400. | may |

### 3.12 CLI-side environment variables (not read by the server)

These are read by the `hasura` CLI or the `hasura/graphql-engine:*.cli-migrations-v3` image
entrypoint, not by `graphql-engine` itself; listed so the Rust project can ship an equivalent
"cli-migrations" image (priority: should, because many deployments use that image):
`HASURA_GRAPHQL_METADATA_DIR` (default `/hasura-metadata`), `HASURA_GRAPHQL_MIGRATIONS_DIR`
(default `/hasura-migrations`), `HASURA_GRAPHQL_MIGRATIONS_SERVER_PORT` (default `9691`, temporary
server used while applying), `HASURA_GRAPHQL_MIGRATIONS_SERVER_TIMEOUT` (default `30` s),
`HASURA_GRAPHQL_MIGRATIONS_DATABASE_ENV_VAR` / `HASURA_GRAPHQL_MIGRATIONS_DATABASE_URL`
(legacy v2 config), `HASURA_GRAPHQL_ENDPOINT` (CLI). The entrypoint starts the server on the
temporary port with `HASURA_GRAPHQL_ENABLE_CONSOLE=false`, waits for `/healthz`, runs
`hasura-cli migrate apply --all-databases`, `hasura-cli metadata apply`, `hasura-cli metadata reload`,
kills it and execs the real server (all from memory of `scripts/cli-migrations/v3/docker-entrypoint.sh`;
(inferred) - not in this snapshot; verify against the image).

---

## 4. Environment variable usage inside metadata (must)

These are not server flags but are resolved from the same process environment:

- Source `connection_info.database_url`: `"postgres://..."` | `{"from_env": "VAR"}` |
  `{"connection_parameters": {...}}` | `{"dynamic_from_file": "/path"}`. Missing env var at load:
  inconsistent source with message `environment variable 'VAR' not set` (inferred exact wording; see metadata doc).
- Webhook URLs: `webhook_from_env` (event triggers, scheduled), action `handler` templates
  `{{ACTION_BASE_URL}}/path`, remote schema `url_from_env`, `{{VAR}}` in URLs.
- Header confs `{"name":"X","value_from_env":"VAR"}`. Restriction:
  `value_from_env` may not start with `HASURA_GRAPHQL_`; error
  `env variables starting with "HASURA_GRAPHQL_" are not allowed in value_from_env: <VAR>` (code `invalid-params` (inferred), raised at metadata validation).
- OpenTelemetry `status: "{{VAR}}"`.

---

## 5. Logging

### 5.1 Envelope (must)

Every log is a single JSON object on its own line on **stdout** (`server/src-lib/Hasura/Logging.hs`,
`EngineLog`, `mkLogger`):

```json
{"timestamp":"2019-05-30T23:40:24.654+0530","level":"info","type":"http-log","detail":{...}}
```

- `timestamp`: server local time (container: UTC) formatted with Haskell format string
  `%FT%H:%M:%S%3Q%z` (`server/src-lib/Hasura/Logging.hs`, `FormattedTime`) ->
  `YYYY-MM-DDTHH:MM:SS.mmm+HHMM`, e.g. `2019-05-30T23:40:24.654+0530`. `%3Q` in Haskell `time`
  pads the fraction to exactly 3 digits (`.000` possible) (inferred: `%Q` alone would strip
  trailing zeros; the width modifier fixes it). Offset has no colon. Rust: `%Y-%m-%dT%H:%M:%S%.3f%z`.
- `level`: `debug` | `info` | `warn` | `error`.
- `type`: log type string (5.3).
- `detail`: type-specific JSON (string for `unstructured`).
- Optional `trace_id` (32 lowercase hex) and `span_id` (16 lowercase hex) when the log is emitted
  inside a traced request (`omitNothingFields`, so absent otherwise). Verified: they are
  top-level envelope fields after `detail` (generic `EngineLog` record: `timestamp`, `level`,
  `type`, `detail`, `trace_id`, `span_id`), and the comment in `mkLogger` states they are logged
  "even in the OSS server". CE propagates incoming `traceparent` (W3C) / B3 headers into these
  ids (inferred).

Filtering (`mkLogger`): a log is emitted iff `logLevel(log) >= configuredLevel` AND
(`type` is in the enabled log types OR `type` is an internal log type). Ordering: debug < info < warn < error.

### 5.2 Levels at which common logs are emitted

- `http-log`: `info` on success, `error` when the request returned an error (QErr) (even 4xx).
- `startup`: mostly `info`; `warn` for `no_admin_secret`; `error` for fatal problems.
- `websocket-log`: `info`; errors (`conn_err`, `proto_err`, `query_err`) at `error` (inferred).
- `webhook-log`: `info`/`error` (auth webhook failures).
- `query-log`: `info`.
- `unstructured` metadata inconsistency: `warn`.

### 5.3 Log types (must)

`server/src-lib/Hasura/Logging.hs`:

Public (configurable through `ENABLED_LOG_TYPES`):
`startup`, `http-log`, `webhook-log`, `websocket-log`, `query-log`, `execution-log`,
`livequery-poller-log`, `action-handler-log`, `data-connector-log`, `jwk-refresh-log`,
`validate-insert-input-log` (docs call it `validate-input-log`; source name wins - accept both).
EE adds `health-check-log`, `event-trigger-log`, `scheduled-trigger-log`, `cron-trigger-log`? (EE
only; accept names without failing - (inferred) the CE parser rejects unknown names; we should
accept them with a warning).

Internal (always enabled regardless of `ENABLED_LOG_TYPES`, subject to level):
`unstructured`, `unhandled-internal-error`, `event-trigger`, `event-trigger-process`,
`scheduled-trigger`, `scheduled-trigger-process`, `cron-event-generator-process`, `ws-server`,
`pg-client`, `metadata`, `telemetry-log`, `schema-sync`, `source-catalog-migration`,
`stored-introspection`, `stored-introspection-storage`, `model-info`, `remote-schema-healing`.

Parsing (`readLogTypes`): split on `,`, each item is JSON-decoded as a string (then lower-cased
and stripped) and looked up among the public names; an empty item fails. Invalid public name
(including any internal or EE-only name) -> fatal
`Fatal Error:- Environment variable HASURA_GRAPHQL_ENABLED_LOG_TYPES: Error in $: Valid list of comma-separated log types: ["startup","http-log","webhook-log","websocket-log","query-log","execution-log","livequery-poller-log","action-handler-log","data-connector-log","jwk-refresh-log","validate-insert-input-log"]`
(the list is `J.encode userAllowedLogTypes`, a JSON array with quotes; the `Error in $: ` prefix
comes from aeson's `eitherDecodeStrict'` (inferred exact prefix)).

### 5.4 `startup` logs (must for `server_configuration`; others should)

Each startup log: `{"type":"startup","level":"info","detail":{"kind":"<kind>","info":<json>}}`.

`kind: "server_configuration"` (`server/src-lib/Hasura/Server/Init/Logging.hs`, `serveOptsToLog`).
`info` object keys (exact):

```json
{
  "port": 8080,
  "server_host": "HostAny",
  "transaction_isolation": "ReadCommitted",
  "admin_secret_set": true,
  "disable_admin_secret": false,
  "auth_hook": null,
  "auth_hook_mode": null,
  "jwt_secret": null,
  "unauth_role": null,
  "cors_config": {"disabled": false, "ws_read_cookie": null, "allowed_origins": "*"},
  "enable_console": true,
  "console_assets_dir": null,
  "console_sentry_dsn": null,
  "enable_telemetry": true,
  "use_prepared_statements": true,
  "stringify_numeric_types": false,
  "v1-boolean-null-collapse": false,
  "enabled_apis": ["metadata", "graphql", "pgdump", "config"],
  "live_query_options": {"batch_size": 100, "refetch_delay": 1},
  "enable_allowlist": false,
  "enabled_log_types": ["startup", "http-log", "webhook-log", "websocket-log", "jwk-refresh-log"],
  "log_level": "info",
  "remote_schema_permissions": false,
  "websocket_compression_options": "NoCompression",
  "websocket_keep_alive": "KeepAliveDelay {unKeepAliveDelay = Seconds {seconds = 5s}}",
  "infer_function_permissions": true,
  "enable_maintenance_mode": false,
  "experimental_features": [],
  "events_fetch_batch_size": 100,
  "graceful_shutdown_timeout": 60,
  "websocket_connection_init_timeout": "WSConnectionInitTimeout {unWSConnectionInitTimeout = Seconds {seconds = 3s}}",
  "websocket_queue_size": 300,
  "enable_metadata_query_logging": false,
  "http_log_query_only_on_error": false
}
```

Values marked with Haskell `show` output (`HostAny`, `ReadCommitted`, `NoCompression`,
`KeepAliveDelay {...}`) are literally `show`-encoded upstream; their exact form is not a
compatibility concern (nobody parses them) - emit similar strings (inferred values; the keys are
verified). Verified value encodings: `auth_hook` is the hook URL **string** (or null),
`auth_hook_mode` is `"GET"`/`"POST"` (or null); `jwt_secret` is an **array** of JWT config JSON
objects (`[]` when unset, not null - `soJwtSecret` is a list); `enable_console`,
`enable_maintenance_mode`, `enable_metadata_query_logging`, `http_log_query_only_on_error`,
`remote_schema_permissions`, `infer_function_permissions` are booleans; `enabled_apis` are
lower-case names; `cors_config` = `{"disabled":bool,"ws_read_cookie":bool|null,"allowed_origins":"*"|[...]|null}`;
`live_query_options.refetch_delay` is seconds (DiffTime). Note the hyphenated key
`v1-boolean-null-collapse` (verbatim).

Other startup kinds:

| kind | level | info |
|---|---|---|
| `postgres_connection` | info | Metadata DB connection. `{"retries":1,"database_url":"postgres://user:...@host:5432/db"}` - the URL is rendered with `show` of `Network.URI`, which replaces the password with `...` (network-uri `defaultUserInfoMap`), and the `sslpassword` query param value is also censored to `...` (`censorURI` in `Init/Logging.hs`). Unparseable URL -> `{"error":"parsing database url failed"}`. `dynamic-from-file` is logged as `dynamic-from-file://<path>`. Rust: same masking. |
| `no_admin_secret` | warn | `"WARNING: No admin secret provided"` |
| `admin_secret_disabled` | info | verbatim message in 3.3. |
| `catalog_migrate` | info | `"Already at the latest catalog version (48); nothing to do."` / `"Successfully initialized the catalog (at version 48)."` / `"Successfully migrated from catalog version X to version 48."` / `"Catalog migrations are skipped because the graphql-engine is in maintenance mode"`. On failure: level `error`, info = QErr JSON, then exit 1. |
| `schema-sync` | info | `"Schema sync disabled"` / `"Schema sync enabled. Polling at " <> show interval` (verified, `App.hs`; the interval is Haskell-`show`n, exact text not compatibility-relevant) |
| `server` | info | `{"message":"Starting API server","time_taken":1.23}` (StartupTimeInfo; `time_taken` seconds since process start), also `"Starting in eventing enabled mode"`, shutdown messages `"Gracefully shutting down server"` etc. |
| `telemetry` | info | telemetry notice text. Rust: omit or say disabled. |
| `event_triggers` / `scheduled_triggers` | info | e.g. `"Starting workers"` (event_triggers), and on shutdown `"Unlocking events for source: <name>"` / `"Unlocking all locked scheduled events on `hdb_scheduled_events` and `hdb_cron_events` tables"` (verified in `App.hs`). |
| `sources-ping` | info | source ping thread start. |

Example (verbatim shape from docs `docs/docs/deployment/logging.mdx`):

```json
{"timestamp":"2023-01-23T10:00:00.000+0000","level":"info","type":"startup","detail":{"kind":"server","info":{"message":"Starting API server","time_taken":2.1}}}
```

### 5.5 `http-log` (must; precise)

`server/src-lib/Hasura/Server/Logging.hs` (`HttpLogContext`, `HttpInfoLog`, `OperationLog`,
`mkHttpAccessLogContext`, `mkHttpErrorLogContext`). One log per HTTP request (including
`/healthz`, metadata API and REST endpoints; websocket traffic uses `websocket-log`).

```json
{
  "timestamp": "2023-01-23T10:00:00.000+0000",
  "level": "info",
  "type": "http-log",
  "detail": {
    "http_info": {
      "status": 200,
      "method": "POST",
      "ip": "172.18.0.1",
      "url": "/v1/graphql",
      "http_version": "HTTP/1.1",
      "content_encoding": "gzip"
    },
    "operation": {
      "request_id": "d2ede87d-5cb7-44b6-8736-1d898117722a",
      "user_vars": {"x-hasura-role": "user", "x-hasura-user-id": "1"},
      "response_size": 105,
      "uncompressed_response_size": 290,
      "request_read_time": 0.000123,
      "query_execution_time": 0.004534,
      "query": {"type": null},
      "request_mode": "single"
    },
    "request_id": "d2ede87d-5cb7-44b6-8736-1d898117722a"
  }
}
```

Fields (all `omitNothing`: keys with `Nothing` values are omitted, not `null`, except where noted):

`http_info`:
- `status`: HTTP status code number. Upstream quirk (verified): `mkHttpAccessLogContext`
  hardcodes `HTTP.status200` for every success log, whatever the handler returned. For errors
  it is `qeStatus` of the QErr **after** the route's error modifier (`logErrorAndResp` applies
  `qErrModifier` before `logHttpError`, verified in `Server/App.hs`), so `/v1/graphql` errors log
  `status: 200` (or 401 with `PRESERVE_401_ERRORS`), matching what the client received, while the
  log `level` is still `error`.
- `method`: request method string.
- `ip`: client IP (verified, `getSourceFromFallback` in
  `server/lib/hasura-extras/src/Network/Wai/Extended.hs`): the value of the **first request header
  (in request order) named `x-real-ip` or `x-forwarded-for`**, taken verbatim (the whole header
  value, e.g. `"1.2.3.4, 10.0.0.1"` - no splitting); if neither header is present, the socket peer
  address (IPv4 dotted / IPv6 text, no port).
- `url`: `Wai.rawPathInfo` = path **without** query string (e.g. `/v1/graphql`, `/api/rest/foo/1`) (verified).
- `http_version`: `show` of the Wai `HttpVersion` -> `"HTTP/1.1"` (`"HTTP/2.0"` for h2 (inferred)).
- `content_encoding`: `"gzip"` when the response was compressed, else **`null`** (verified: `http_info`
  is built with an explicit `J.object`, so this key is always present). All six `http_info` keys
  are always present.

`operation`:
- `request_id`: from request header `x-request-id` if present (verbatim), else a freshly generated
  UUIDv4 (`generateFingerprint`, `getRequestId` in `Server/Types.hs`); the same id is returned as
  response header `x-request-id`.
- `user_vars`: session variables object (lowercased keys) when the request was authenticated; omitted when auth failed before resolution.
- `response_size`: bytes of the (possibly compressed) response body.
- `uncompressed_response_size`: bytes before compression (always present).
- `request_read_time`: seconds (float) spent reading the body; omitted when no timing is
  available (e.g. `/healthz`, `/v1/version`, unknown-route 404s, disabled-API responses).
- `query_execution_time`: seconds (float) for executing the request (GraphQL: total for all root
  fields); omitted under the same conditions.
- On error logs `response_size` = `uncompressed_response_size` = byte length of `J.encode qErr`
  (not of the actual response body) (verified, `mkHttpErrorLogContext`).
- `query`: see rules below.
- `raw_query`: the raw request body as a string; only in error logs, only when the body was not
  parsed (`parsedReq == Nothing`) and `isQueryIncludedInLogs` (rule below) is true (verified).
- `error`: the QErr JSON (`{"path":"$","error":"...","code":"...","internal"?:...}`) when the request failed; level becomes `error`.
- `request_mode`: `"single"` | `"batched"` | `"non-graphql"` | `"error"`.
  - `single`: one GraphQL request; `batched`: JSON array body to `/v1/graphql`; `non-graphql`:
    metadata/query/healthz/etc.; `error`: request failed before it could be classified (inferred).

`batched_operations` (top-level in `detail`, only for batched GraphQL success logs; omitted
otherwise): array, one entry per operation: success `{"query"?:..., "response_size": N, "query_execution_time": s}`
or failure `{"query"?:..., "error": QErr}` (verified keys: `BatchOperationSuccessLog` /
`BatchOperationErrorLog`). Upstream quirk (verified): each entry's `query` is computed from the
**whole** parsed request body (the full batch array), not from the individual operation.

`request_id` is repeated at `detail.request_id` (top-level of detail) - verified.

`query` field rules (verified: `isQueryIncludedInLogs` / `addQuery` in `server/src-lib/Hasura/Server/Logging.hs`, CE).
"Included" means the parsed request body JSON is logged as-is:
1. If `HTTP_LOG_QUERY_ONLY_ON_ERROR`: included iff the request errored (any path, metadata
   included, regardless of `query-log`); when not included: `{"operationName": <body.operationName string or null>}`.
2. Else if `query-log` is enabled: for exactly the paths `/v1/metadata` and `/v1/query` (NOT
   `/v2/query`) included iff `ENABLE_METADATA_QUERY_LOGGING`; for every other path (GraphQL,
   `/v2/query`, REST, ...) included.
3. When not included (and rule 1 does not apply): `{"type": <body.type if it is a string, else null>}` -
   e.g. `{"type": null}` for GraphQL, `{"type":"replace_metadata"}` for `/v1/metadata`.
   So the `query` key is always present unless there was no parsed body *and* the query is
   included (`Nothing` -> omitted).
http-log `query` is **not** masked by `LOG_MASKED_VARIABLES` (masking applies to query-log only).

Upstream EE additionally adds `parameterized_query_hash` and `operation_name` inside `operation` for
GraphQL requests; CE does **not** include them in http-log (docs examples show them because docs
are written against Cloud/EE). Rust: may add `operation_name`/`parameterized_query_hash` as
extra fields - harmless, but tests-py `test_logging.py` checks presence of specific keys only.

Errors for unknown routes are logged via `raiseGenericApiError` with status 404 and the QErr.

### 5.6 `webhook-log` (should)

Emitted for auth webhook calls (and some other outgoing webhooks):

```json
{"type":"webhook-log","level":"info","detail":{"status_code":200,"url":"http://auth:3000/","method":"GET","http_error":null,"response":"{...}","message":null}}
```

Keys (`server/src-lib/Hasura/Server/Logging.hs`, `WebHookLog`): `status_code` (number or null),
`url`, `method`, `http_error` (null or error object `{"type":"http_exception","message":...}` (inferred)),
`response` (response body text or null), `message` (null or string). Level `error` when the webhook failed.

### 5.7 `query-log` (should)

`server/src-lib/Hasura/GraphQL/Logging/QueryLog.hs`:

```json
{"type":"query-log","level":"info","detail":{
  "kind":"database",
  "request_id":"...",
  "query":{"query":"query { author { id } }","variables":{},"operationName":null},
  "generated_sql":{"author":{"query":"SELECT ... $1 ...","prepared_arguments":["{\"x-hasura-role\":\"admin\"}"]}},
  "connection_template": null
}}
```

- `query`: the GraphQL request as received (variables masked per `LOG_MASKED_VARIABLES`).
- `generated_sql`: object keyed by root field alias -> `{"query": <SQL>, "prepared_arguments": [<text values>]}`; `null` for non-database kinds.
- `request_id`.
- `kind`: `database` | `action` | `remote-schema` | `cached` | `introspection`.
- `connection_template`: present only when a connection template resolved (EE) (omitted otherwise (inferred)).
- One log per root field executed (inferred; mutations and queries log per field; subscriptions log once at start).
- Metadata API requests do not produce query-log (they appear in http-log under rule 2).

### 5.8 `websocket-log` (should)

`server/src-lib/Hasura/GraphQL/Transport/WebSocket/Server.hs` (`WSLog`), `.../WebSocket.hs` (`WSLogInfo`):

```json
{"type":"websocket-log","level":"info","detail":{
  "user_vars":{"x-hasura-role":"admin"},
  "connection_info":{"websocket_id":"f6a4...","token_expiry":null,"msg":null},
  "event":{"type":"operation","detail":{
     "operation_id":"1","request_id":"...","operation_name":"MySub",
     "operation_type":{"type":"started"},
     "query":{"query":"subscription ...","variables":{}},
     "parameterized_query_hash":"8d32..."}}}}
```

- `user_vars`: session or null (before auth).
- `connection_info`: `websocket_id` (UUID), `token_expiry` (ISO time of JWT expiry or null; docs call it `jwt_expiry` - source `token_expiry`), `msg` (optional message).
- `event.type`: `accepted` | `rejected` (detail = QErr) | `conn_err` (detail = error text) | `operation` | `closed`.
- Operation `operation_type.type`: `started` | `proto_err` (detail text) | `query_err` (detail QErr) | `completed` | `stopped`.
- Key shapes above verified against the `deriveToJSON` splices in `GraphQL/Transport/WebSocket.hs` (`WSLogInfo`, `WsConnInfo`, `OperationDetails`, tagged sums `{"type":..,"detail":..}`; nullary events such as `accepted`/`closed`/`started` are just `{"type":"..."}`). `operation_name`, `request_id`, `query`, `parameterized_query_hash` are `null` when absent (hasuraJSON does not omit Nothing) (inferred). Whether `query` is filled depends on the call site (inferred).

Also `ws-server` internal logs (`{"websocket_id":..., "event":{"type":"..."}}`) at debug level.

### 5.9 Other logs (may)

- `livequery-poller-log`: per poll cycle stats (`execution_batches`, `snapshot_time`, `total_time`, `cohorts`...). Shape in subscriptions doc.
- `action-handler-log`: `{"request_size":N,"response_size":N,"action_name":"...","request_body"?..}` (redactable).
- `jwk-refresh-log`: `{"message":"...","url":"...","http_error":...,"response":...}` on JWK fetch.
- `event-trigger` / `scheduled-trigger` delivery logs: see triggers doc.
- `unstructured`: `detail` is a plain string.
- Metadata inconsistency at startup and after metadata changes: `{"type":"metadata","level":"warn","detail":{"message":"Inconsistent Metadata!","info":{"objects":[<inconsistent object JSON as in get_inconsistent_metadata>]}}}`.

---

## 6. Request-level headers influenced by config

- `x-request-id`: echoed/generated (see 5.5). Always set on responses.
- Response `Content-Type: application/json; charset=utf-8` for JSON APIs.
- Warp server name: upstream sets `setServerName ""` so **no `Server` header** (`server/src-lib/Hasura/App.hs`). Rust: omit `Server` header.
- gzip: responses compressed when client sends `Accept-Encoding: gzip` and response is larger than a threshold (upstream compresses JSON responses when gzip accepted; threshold (inferred) none/ small). See HTTP doc.

---

## 7. Startup sequence (must)

`server/src-lib/Hasura/App.hs` (`initialiseAppEnv`, `initialiseAppContext`, `runHGEServer`,
`mkHGEServer`). In order:

1. Parse args & env (sections 1-3). On parse errors: print usage/error, exit 1.
2. Validate combinations (auth checks 3.3, WS_READ_COOKIE vs CORS 3.1, DB URL presence 2.2).
3. Log `startup` `server_configuration` and `postgres_connection` (if a DB URL).
4. Emit deprecation warnings (2.4).
5. Create metadata DB pool; connect with retries (`--retries`). On connection failure: log
   `startup`/`postgres_connection` error and exit 1 (inferred: message `Fatal Error: ... connection error`).
6. **Catalog migration** in a serializable read-write transaction on the metadata DB
   (`server/src-lib/Hasura/Server/Migrate.hs`, `migrateCatalog`):
   - If `hdb_catalog.hdb_version` does not exist: create schema `hdb_catalog` if missing;
     ensure pgcrypto (`enablePgcryptoExtension` in `Backends/Postgres/Connection/MonadTx.hs`,
     verified): if not already created, run `CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA <METADATA_DATABASE_EXTENSIONS_SCHEMA>`
     when available, else fail 400 `unexpected` `pgcrypto extension is required, but could not find the extension in the PostgreSQL server. Please make sure this extension is available.`
     (SQLSTATE 42501 on create -> 500 `postgres-error` with a permissions message). Failure is
     NOT tolerated even on PG13+;
     run `server/src-rsr/initialise.sql` (creates `hdb_catalog.hdb_version`, `hdb_metadata`,
     `hdb_schema_notifications`, `hdb_action_log`, `hdb_cron_events`, `hdb_cron_event_invocation_logs`,
     `hdb_scheduled_events`, `hdb_scheduled_event_invocation_logs`, `hdb_source_catalog_version`? etc. - full DDL
     in the metadata-format doc); upsert `INSERT INTO hdb_catalog.hdb_version (version, upgraded_on) VALUES ('48', now) ON CONFLICT ((version IS NOT NULL)) DO UPDATE ...`
     (`setCatalogVersion`; `hasura_uuid` defaults to `hdb_catalog.gen_hasura_uuid()`, `cli_state`/`console_state` default `'{}'`);
     insert initial metadata `{"version":3,"sources":[<default source if DB URL>]}` into `hdb_catalog.hdb_metadata (id=1, metadata, resource_version=1)`.
     Log `Successfully initialized the catalog (at version 48).`
   - If present with version `48`: `Already at the latest catalog version (48); nothing to do.`
   - If older (`< 48`, down to version 0.x from v1.x): run upgrade steps
     (`server/src-rsr/migrations/<from>_to_<to>.sql`) - in particular pre-v2 (version <= 42)
     catalogs are converted to v2 metadata. Log `Successfully migrated from catalog version X to version 48.` Priority: should for 40..47, may for <= 42 (v1 migrations).
   - If newer than 48: fatal `Cannot use database previously used with a newer version of graphql-engine (expected a catalog version <=48, but the current version is M).`
   - In maintenance mode: skip entirely (no version comparison); fail 500 only if the schema,
     `hdb_version` or `hdb_metadata` table is missing (verified). The newer-version error above is
     code `not-supported` (400-class QErr, logged as `catalog_migrate` error, exit 1).
   Default source initial metadata (from `server/src-lib/Hasura/Server/Migrate.hs`, `emptyMetadataWithDefaultSource`):
   ```json
   {"name":"default","kind":"postgres","tables":[],"configuration":{"connection_info":{"database_url":{"from_env":"HASURA_GRAPHQL_DATABASE_URL"},"isolation_level":"read-committed","pool_settings":{"connection_lifetime":600,"idle_timeout":180,"max_connections":50,"retries":1},"use_prepared_statements":true}}}
   ```
   Pool settings come from `src-exec/Main.hs` (`runApp`, verified): `max_connections` = `PG_CONNECTIONS` (50),
   `idle_timeout` = `PG_TIMEOUT` (180), `retries` = `--retries`/`NO_OF_RETRIES` or 1,
   `connection_lifetime` = `PG_CONN_LIFETIME` (600; omitted when 0), `pool_timeout` only when set
   (`omitNothingFields`). So with defaults `connection_lifetime: 600` **is** present. The
   `database_url` is `{"from_env":"HASURA_GRAPHQL_DATABASE_URL"}` when it came from the env, else
   the literal URL string. Key order / which empty collections are emitted: see doc 01 (inferred).
7. Load metadata from `hdb_catalog.hdb_metadata` (with `resource_version`), apply
   `METADATA_DEFAULTS`, build the schema cache:
   - For each source: connect, run **source catalog migration** if needed (creates
     `hdb_catalog.event_log`, `event_invocation_logs`, `hdb_source_catalog_version`, etc. in the
     *source* DB, only when the source has event triggers or always? - upstream creates source
     catalog lazily when event triggers exist (inferred; see triggers doc)); log type
     `source-catalog-migration`.
   - Resolve tables/functions/relationships/permissions etc.; any failing object becomes an
     **inconsistent object** - non-fatal. Remote schemas unreachable -> inconsistent (retry via healing).
   - Log `metadata` `warn` `Inconsistent Metadata!` with the list.
   - A fatal schema cache build error (not an inconsistency, e.g. internal error) -> logged as
     `startup`/`catalog_migrate` error and exit 1.
8. Start background threads:
   - schema sync listener/poller (multi-instance; `hdb_schema_notifications` poll every
     `SCHEMA_SYNC_POLL_INTERVAL`); after a metadata change by this instance, it bumps
     `resource_version` and others reload.
   - remote schema healing (if interval > 0).
   - event trigger processor, async action processor, scheduled/cron processors (unless disabled /
     maintenance / read-only).
   - update check (Rust: skip), telemetry (Rust: skip), source ping, websocket keepalive reaper,
     JWK refresher (doc 05).
9. Start HTTP server on host:port (Warp settings, verified in `runHGEServer`: `setServerName ""`,
   `setMaxTotalHeaderLength`, `setTimeout SERVER_TIMEOUT`, `setGracefulShutdownTimeout (Just 30)` -
   i.e. in-flight HTTP requests get a fixed 30 s on shutdown, independent of
   `GRACEFUL_SHUTDOWN_TIMEOUT`, which governs event/action/scheduled workers). Log `startup` kind `server` `{"message":"Starting API server","time_taken":N}`.
10. On SIGTERM/SIGINT (`CatchOnce`): log `Gracefully shutting down server`; stop accepting
    connections; close websockets; wait for in-flight HTTP requests; wait up to
    `GRACEFUL_SHUTDOWN_TIMEOUT` for event/action/scheduled workers, then unlock unprocessed
    locked events (`UPDATE hdb_catalog.event_log SET locked = NULL WHERE id = ANY(...)`) and
    similarly for scheduled events; exit 0.

Behaviour when the metadata DB is unreachable after startup: requests that need metadata storage
(e.g. metadata API) fail; `/healthz` returns 500 `ERROR`.

---

## 8. `/v1alpha1/config` (should)

Gated by `config` API (404 when disabled), admin only (verified `onlyAdmin`: non-admin gets 400
`{"path":"$","error":"You have to be an admin to access this endpoint","code":"access-denied"}`).
`server/src-lib/Hasura/Server/API/Config.hs` (`ServerConfig`, `JWTInfo`, both `genericToJSON hasuraJSON`
= prefix-stripped snake_case, `Nothing` encoded as `null`). Verified key list and order:

```json
{
  "version": "v2.x.y",
  "is_function_permissions_inferred": true,
  "is_remote_schema_permissions_enabled": false,
  "is_admin_secret_set": true,
  "is_auth_hook_set": false,
  "is_jwt_set": true,
  "jwt": [{"claims_namespace": "https://hasura.io/jwt/claims", "claims_format": "json", "claims_map": null}],
  "is_allow_list_enabled": false,
  "live_queries": {"batch_size": 100, "refetch_delay": 1},
  "streaming_queries": {"batch_size": 100, "refetch_delay": 1},
  "console_assets_dir": null,
  "experimental_features": [],
  "is_prometheus_metrics_enabled": false,
  "default_naming_convention": "hasura-default",
  "is_apollo_federation_enabled": false
}
```

- `refetch_delay` is in **seconds** (a `DiffTime` encoded as a number; `1` for the 1000 ms default; 500 ms -> `0.5`).
- `jwt`: one entry per configured JWT secret. `claims_namespace` encodes as either a string
  (namespace) or `{"path": "$.x"}`-style for `claims_namespace_path` (inferred - see `JWTNamespace` ToJSON in doc 05);
  `claims_format` `"json"` or `"stringified_json"`; `claims_map` null or the configured map.
- `experimental_features`: array of feature names (order unspecified, HashSet).
- `is_prometheus_metrics_enabled`: `true` iff `metrics` in `ENABLED_APIS` (verified).
- `is_admin_secret_set` is `true` for **every** auth mode except "no auth" (`isAdminSecretSet`:
  `AMNoAuth -> False; _ -> True`), e.g. also for disabled-admin-secret JWT-only mode (verified).
  `is_auth_hook_set` / `is_jwt_set` reflect the auth mode. For a `claims_map` JWT config, `jwt`
  reports the default namespace `https://hasura.io/jwt/claims`, default format `json`, and the map.

## 9. Implementation notes for the Rust engine

- Use a single config struct; build from argv (after stripping RTS/Heartbeat spans) + env + defaults, honouring the switch semantics in 1.4.
- Keep a `HashMap<String,String>` snapshot of the environment for templating (`{{VAR}}`, `from_env`, `value_from_env`).
- Unknown env vars: ignore. Unknown CLI flags: error (as upstream) - except we may accept the EE-only flags silently.
- Log writer: one `serde_json` line per log to stdout; serialize with fixed key order `timestamp, level, type, detail` (consumers must not depend on key order; tests-py `test_logging.py` parses JSON).
- Durations in logs are float seconds.
- Mask DB passwords in `postgres_connection` as `user:...@` (same as upstream, see 5.4).

---

## Upstream tests to reuse

All paths under `server/tests-py/` unless noted. Run with `--hge-bin=<rust binary>` so the harness spawns our binary with the per-class `hge_env` (see doc 14).

- `test_logging.py` (12 tests) - `startup`/`http-log`/`query-log`/`websocket-log` presence and keys (`http_info`, `operation.query`, `request_id`, `query_execution_time`, `generated_sql`, `kind`), metadata query logging on/off, `HTTP_LOG_QUERY_ONLY_ON_ERROR`. Needs stdout capture (`capture_hge_logs` marker), i.e. harness-spawned binary.
- `test_config_api.py` (13 tests) - `/v1alpha1/config` keys under admin secret / JWT / webhook / allowlist / function-permission / remote-schema-permission env combinations.
- `test_apis_disabled.py` - `ENABLED_APIS` gating (404 `resource does not exist`).
- `test_cors.py` - `CORS_DOMAIN` wildcard matching and `DISABLE_CORS`.
- `test_websocket_init_cookie.py`, `test_auth_webhook_cookie.py` - `WS_READ_COOKIE` with CORS disabled.
- `test_disable_admin_secret.py` - `DISABLE_ADMIN_SECRET` with JWT/webhook.
- `test_dev_endpoints.py` - `developer` API routes and `DEV_MODE`.
- `test_version.py` - `/v1/version` shape (asserts the version string equals the harness `VERSION` env; adapt).
- `test_compression.py` - gzip response compression and `content_encoding` behaviour.
- `test_v1alpha1_endpoint.py` - legacy `/v1alpha1/graphql` route.
- `test_pg_dump.py` - `pgdump` API (requires `pg_dump` binary in our image).
- `test_inconsistent_meta.py` - inconsistent metadata at load/reload, healthz-adjacent behaviour.
- `test_graphql_queries.py` classes with `hge_env` such as `HASURA_GRAPHQL_INFER_FUNCTION_PERMISSIONS=false`, `HASURA_GRAPHQL_V1_BOOLEAN_NULL_COLLAPSE=true`, `HASURA_GRAPHQL_UNAUTHORIZED_ROLE=anonymous` - option effects (harness always passes `--stringify-numeric-types`).
- `test_naming_conventions.py` + `queries/naming_conventions/` - `DEFAULT_NAMING_CONVENTION`, experimental `naming_convention`.
- `test_subscriptions.py` - `WEBSOCKET_CONNECTION_INIT_TIMEOUT`, keepalive, close-on-metadata-change.
- `test_events.py` - `EVENTS_FETCH_INTERVAL`, `EVENTS_HTTP_POOL_SIZE`, graceful shutdown/unlocking of events.
- `test_remote_schema_prioritize_data.py` / `test_remote_schema_prioritize_none.py` - `REMOTE_SCHEMA_PRIORITIZE_DATA`.
- `server/lib/api-tests/src/Test/HealthCheckSpec.hs` - `/healthz` OK/WARN/ERROR and `strict=true`.
- `server/lib/api-tests/src/Test/HelloWorldSpec.hs` - smoke test of the external-process harness (argv with `+Heartbeat` span and `serve ... --metadata-database-url`).
- Skip: `test_query_cache.py` (EE/Redis), anything requiring `HASURA_GRAPHQL_EE_LICENSE_KEY` / `--pro-tests` / `--redis-url`, and `--backend` runs other than postgres (MSSQL, BigQuery, Citus).
